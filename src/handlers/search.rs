use askama::Template;
use axum::{
    Form,
    extract::{Query, State},
    response::{Html, Json},
};
use serde::Deserialize;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::services::{logger, nyaa, scoring};

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    page: String,
    results: Vec<nyaa::SearchResult>,
    query: String,
    searched: bool,
    has_next: bool,
    /// Issue #83 — `batches_only` (default) or `never`. Threaded
    /// through to search.js via window.searchState so the Grab button
    /// can bypass the modal when the user's set it to `never`.
    grab_preview_mode: String,
    title_language: String,
}

async fn load_grab_preview_mode(state: &AppState) -> String {
    crate::models::config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.grab_preview_mode)
        .unwrap_or_else(|| "batches_only".to_string())
}

#[derive(Deserialize)]
pub struct SearchForm {
    query: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    uploader: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct PageQuery {
    query: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    uploader: String,
    #[serde(default = "default_page")]
    p: i32,
}

fn default_page() -> i32 {
    1
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct GrabForm {
    url: String,
    /// Optional release title — used for library linkage (v1.3.0 plan
    /// item 6d). When supplied, the grab handler tries to match it
    /// against an existing library series; on match, the grab lands
    /// in `grabbed_torrents` linked to that series and (for batches)
    /// auto_expand runs for sibling-series detection. Empty / absent
    /// = behave like the original grab endpoint (fire-and-forget).
    #[serde(default)]
    title: Option<String>,
    /// Optional info_hash from the frontend. Used both to key the
    /// download-client add and (when matched) as the grabbed_torrents
    /// primary key. Frontend sends it when known.
    #[serde(default)]
    info_hash: Option<String>,
    /// Whether the release was flagged as a batch by the search UI.
    /// Gates auto_expand at grab time.
    #[serde(default)]
    is_batch: Option<bool>,
    /// Multi-client routing — id of the indexer that surfaced this
    /// release. `None` for Nyaa-direct hits (which route via the
    /// Nyaa pin), `Some(id)` for torznab/newznab fan-out hits (which
    /// route via the indexer's per-row pin). Frontend reads this
    /// from `SearchResult.indexer_id` and round-trips it on grab.
    #[serde(default)]
    indexer_id: Option<i64>,
}

/// Helper to build SearchOptions from config.
async fn build_opts(
    state: &AppState,
    query: String,
    category: String,
    filter: String,
    uploader: String,
) -> nyaa::SearchOptions {
    let config = crate::models::config::get_config(&state.db)
        .await
        .ok()
        .flatten();

    let preferred_groups = config
        .as_ref()
        .map(|c| {
            c.preferred_groups
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let preferred_res = config
        .as_ref()
        .map(|c| c.preferred_resolution.clone())
        .unwrap_or_else(|| "1080".to_string());

    let prefer_subs = config.as_ref().map(|c| c.prefer_subs).unwrap_or(true);

    nyaa::SearchOptions {
        query,
        category: if category.is_empty() {
            "1_0".to_string()
        } else {
            category
        },
        filter: if filter.is_empty() {
            "0".to_string()
        } else {
            filter
        },
        // `nyaa::SearchOptions.user` is the wire-API field name —
        // it's the Nyaa URL parameter `?u=<name>`. The form field
        // ships as `uploader` to dodge browser autofill heuristics
        // that pool every `name="user"` text input across sites
        // (banking, login, etc.).
        user: uploader,
        preferred_groups,
        preferred_resolution: preferred_res,
        prefer_subs,
    }
}

pub async fn search_page(State(state): State<AppState>) -> Html<String> {
    let (grab_preview_mode, title_language) = tokio::join!(
        load_grab_preview_mode(&state),
        crate::models::config::get_title_language(&state.db),
    );
    let template = SearchTemplate {
        page: "search".to_string(),
        results: Vec::new(),
        query: String::new(),
        searched: false,
        has_next: false,
        grab_preview_mode,
        title_language,
    };
    Html(template.render().unwrap_or_default())
}

pub async fn search_submit(
    State(state): State<AppState>,
    Form(form): Form<SearchForm>,
) -> Html<String> {
    let opts = build_opts(
        &state,
        form.query.clone(),
        form.category,
        form.filter,
        form.uploader,
    )
    .await;

    let mut response = match nyaa::search(&opts, 1).await {
        Ok(resp) => {
            logger::debug(
                &state.db,
                LogCategory::Search,
                &format!("Search: '{}' — {} results", form.query, resp.results.len()),
                "",
            )
            .await;
            resp
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::Nyaa,
                &format!("Search failed: '{}'", form.query),
                &e,
            )
            .await;
            nyaa::SearchResponse {
                results: Vec::new(),
                page: 1,
                has_next: false,
            }
        }
    };

    // v1.3.0 — augment the base-score breakdown with Custom Format
    // contributions so the search-page expander shows both the base
    // rules and the CF deltas. SeaDex specs never fire here (no
    // series context = empty hash set), which is deliberate: the
    // manual search page is a generic Nyaa search surface, not a
    // per-series auto-grab path.
    let cfs = state.custom_formats.read().await.clone();
    scoring::apply_cf_breakdown(
        &mut response.results,
        &cfs,
        &std::collections::HashSet::new(),
    );

    let (grab_preview_mode, title_language) = tokio::join!(
        load_grab_preview_mode(&state),
        crate::models::config::get_title_language(&state.db),
    );
    let template = SearchTemplate {
        page: "search".to_string(),
        results: response.results,
        query: form.query,
        searched: true,
        has_next: response.has_next,
        grab_preview_mode,
        title_language,
    };
    Html(template.render().unwrap_or_default())
}

/// JSON API endpoint for loading additional pages.
#[utoipa::path(
    get,
    path = "/api/search/page",
    tag = "Search",
    summary = "Search Nyaa torrents",
    description = "Search Nyaa.si for anime torrents with pagination and filtering options.",
    params(PageQuery),
    responses(
        (status = 200, description = "Paginated search results", body = nyaa::SearchResponse),
        (status = 500, description = "Search failed"),
    ),
)]
pub async fn search_page_api(
    State(state): State<AppState>,
    Query(params): Query<PageQuery>,
) -> Result<Json<nyaa::SearchResponse>, (axum::http::StatusCode, String)> {
    let opts = build_opts(
        &state,
        params.query,
        params.category,
        params.filter,
        params.uploader,
    )
    .await;

    let mut response = nyaa::search(&opts, params.p)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Mirror search_submit — keep the expander/scores consistent across
    // page 1 (server-rendered) and page 2+ (JSON-appended via loadMore).
    let cfs = state.custom_formats.read().await.clone();
    scoring::apply_cf_breakdown(
        &mut response.results,
        &cfs,
        &std::collections::HashSet::new(),
    );

    Ok(Json(response))
}

#[utoipa::path(
    post,
    path = "/api/grab",
    tag = "Search",
    summary = "Grab a torrent",
    description = "Send a torrent URL to qBittorrent for download.",
    request_body = GrabForm,
    responses(
        (status = 200, description = "Torrent added", body = serde_json::Value),
        (status = 400, description = "qBittorrent not configured"),
        (status = 500, description = "Failed to add torrent"),
    ),
)]
pub async fn grab_release(
    State(state): State<AppState>,
    Json(form): Json<GrabForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    // Pin chain: indexer_id (when the result came from a torznab/newznab
    // fan-out) > Nyaa pin (Nyaa-direct results) > default. The manual-
    // search page only invokes `nyaa::search` today, so any form arriving
    // here without `indexer_id` is implicitly Nyaa-direct and routes
    // through the Nyaa pin. Pre-PR-F-followup this always hit the default.
    let resolved = if form.indexer_id.is_some() {
        state.client_for_indexer_with_id(form.indexer_id).await
    } else {
        let cfg = crate::models::config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        state
            .client_for_nyaa_with_id(cfg.nyaa_download_client_id)
            .await
    };
    let (client, dispatch_client_id) = resolved.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;

    let form_hash = form.info_hash.clone().unwrap_or_default();
    let info_hash = if !form_hash.is_empty() {
        form_hash
    } else {
        crate::services::nyaa::extract_hash(&form.url)
    };
    let (_outcome, canonical_id) = client
        .add_torrent_returning_id(&form.url, &info_hash)
        .await
        .map_err(|e| {
            let db = state.db.clone();
            let err_msg = e.clone();
            tokio::spawn(async move {
                logger::error(
                    &db,
                    LogCategory::DownloadClient,
                    "Manual grab failed",
                    &err_msg,
                )
                .await;
            });
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;

    logger::info(
        &state.db,
        LogCategory::Grab,
        "Manual grab sent to download client",
        &form.url,
    )
    .await;

    // Library linkage. Pre-1.7 this was a fire-and-forget spawn that
    // only matched against the existing library via the RSS-style
    // fuzzy title matcher and silently no-op'd on miss. Now resolved
    // synchronously through `library_link::resolve_or_add_series_for_grab`
    // so the response can carry the linked / auto-added series title
    // for the search-page toast. The chain is fuzzy-match → anitomy
    // parse → AL search → AL-ID lookup → auto-add (gated by
    // `config.manual_search_auto_add`, default ON). See module docs.
    //
    // The canonical id is the client-returned hash (BT: equals
    // info_hash; SAB: nzo_id) so post-processing's `list_scoped`
    // matching works for both protocols. Pre-add `info_hash` is only
    // the pre-computed BT shape; SAB grabs would silently never
    // import if we recorded that instead of the returned id.
    let title = form.title.clone().unwrap_or_default();
    let is_batch = form.is_batch.unwrap_or(false);
    let hash = canonical_id.clone();
    let mut link_outcome: Option<crate::services::library_link::LibraryLinkOutcome> = None;
    if !title.is_empty() && !hash.is_empty() {
        let outcome =
            crate::services::library_link::resolve_or_add_series_for_grab(&state, &title, is_batch)
                .await;

        // Apply linkage side effects on the three "linked" branches.
        // Ambiguous / disabled / no-match get logged but produce no
        // grabbed_torrents row — that's the right behavior for the
        // user's "auto-add toggle off" / "AL match too weak" cases.
        match &outcome {
            crate::services::library_link::LibraryLinkOutcome::LinkedExisting {
                series,
                episode_numbers,
            }
            | crate::services::library_link::LibraryLinkOutcome::LinkedByAnilist {
                series,
                episode_numbers,
            }
            | crate::services::library_link::LibraryLinkOutcome::AutoAdded {
                series,
                episode_numbers,
            } => {
                let was_added = matches!(
                    &outcome,
                    crate::services::library_link::LibraryLinkOutcome::AutoAdded { .. }
                );
                let grab_id = crate::models::grabbed_torrents::record_grab(
                    &state.db,
                    &hash,
                    &title,
                    series.id,
                    episode_numbers,
                    is_batch,
                )
                .await
                .ok()
                .flatten();
                // Misgrab guardrails: keep the URL so Restore can re-add a removed grab.
                if let Some(gid) = grab_id {
                    let _ =
                        crate::models::grabbed_torrents::set_source_url(&state.db, gid, &form.url)
                            .await;
                }
                if let Some(gid) = grab_id {
                    let _ = crate::models::grabbed_torrents::set_download_client(
                        &state.db,
                        gid,
                        Some(dispatch_client_id),
                    )
                    .await;
                    // Issue #118 — fire `Grabbed` for the manual
                    // search-page path. Nyaa-direct (no indexer
                    // attribution); the search page doesn't run a
                    // scoring pass so `score = None`. `client_kind`
                    // pulled from the resolved client handle.
                    crate::services::notifications::emit_grabbed(
                        &state,
                        series.id,
                        episode_numbers.first().copied().unwrap_or(0),
                        &title,
                        None,
                        None,
                        Some(client.sonarr_impl_name().to_string()),
                    )
                    .await;
                }

                // Populate episode_quality_tags so the series page
                // shows each grabbed episode in 'grabbed' state right
                // away (don't wait for post-processing).
                let classification = crate::services::source::classify_release(
                    &state.db,
                    &title,
                    None,
                    Some(crate::services::source::NyaaContext {
                        info_hash: &hash,
                        view_url: "",
                        is_batch,
                    }),
                    Some(crate::services::source::SeriesContext {
                        status: &series.status,
                        season_year: series.season_year,
                        end_year: series.end_year,
                    }),
                )
                .await;
                for ep in episode_numbers {
                    let _ = crate::models::episode_tags::record_grab(
                        &state.db,
                        series.id,
                        *ep,
                        &classification,
                        &title,
                        "",
                        0,
                        is_batch,
                    )
                    .await;
                }

                let action = if was_added {
                    "auto-added series and linked"
                } else {
                    "linked to series"
                };
                logger::info(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Manual grab {}: {} ({} ep{})",
                        action,
                        series.title,
                        episode_numbers.len(),
                        if episode_numbers.len() == 1 { "" } else { "s" }
                    ),
                    &title,
                )
                .await;

                // Batch grabs get sibling-series detection via
                // auto_expand at metadata-available time — same path
                // RSS + auto-search use. Skipped when the series's
                // provider_id is negative (Jikan-fallback sentinel,
                // no AL graph to walk). The 180s metadata wait is
                // why this stays in a `tokio::spawn`.
                if is_batch
                    && series.anilist_id > 0
                    && let Some(grab_id) = grab_id
                {
                    let db_expand = state.db.clone();
                    let client_expand = client.clone();
                    let hash_expand = hash.clone();
                    let title_expand = title.clone();
                    let series_id_expand = series.id;
                    let provider_id_expand = series.anilist_id;
                    let ep_list_expand = episode_numbers.clone();
                    let classification_expand = classification.clone();
                    tokio::spawn(async move {
                        let detail = match crate::models::metadata_cache::get_by_provider_id(
                            &db_expand,
                            provider_id_expand,
                        )
                        .await
                        {
                            Ok(Some(row)) => row.detail,
                            _ => return,
                        };
                        let files = match crate::services::download_client::wait_for_files(
                            &*client_expand,
                            &hash_expand,
                            std::time::Duration::from_secs(180),
                        )
                        .await
                        {
                            Ok(files) => files,
                            Err(_) => return,
                        };
                        let filenames: Vec<String> = files.into_iter().map(|f| f.name).collect();
                        let ctx = crate::services::auto_expand::AutoExpandGrabContext {
                            classification: classification_expand,
                            release_group: String::new(),
                            size_bytes: 0,
                        };
                        crate::services::auto_expand::expand_from_files(
                            &db_expand,
                            &filenames,
                            &detail,
                            series_id_expand,
                            &ep_list_expand,
                            grab_id,
                            &title_expand,
                            &ctx,
                        )
                        .await;
                    });
                }
            }
            crate::services::library_link::LibraryLinkOutcome::AmbiguousMatch {
                parsed_title,
                al_title,
            } => {
                logger::info(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Manual grab not linked: AL match for \"{}\" was ambiguous (\"{}\")",
                        parsed_title, al_title
                    ),
                    &title,
                )
                .await;
            }
            crate::services::library_link::LibraryLinkOutcome::AutoAddDisabled {
                al_title, ..
            } => {
                logger::info(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Manual grab not auto-added (toggle off): AL match \"{}\"",
                        al_title
                    ),
                    &title,
                )
                .await;
            }
            crate::services::library_link::LibraryLinkOutcome::DetailFetchFailed {
                al_title,
                ..
            } => {
                logger::info(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Manual grab not linked: AL detail fetch failed for matched series \"{}\"",
                        al_title
                    ),
                    &title,
                )
                .await;
            }
            crate::services::library_link::LibraryLinkOutcome::NoMatch { parsed_title } => {
                logger::info(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Manual grab not linked: no library or AL match (parsed=\"{}\")",
                        parsed_title.as_deref().unwrap_or("")
                    ),
                    &title,
                )
                .await;
            }
        }

        link_outcome = Some(outcome);
    }

    // Build the response so the frontend toast can surface the
    // linkage outcome with the resolved series title. New `tag()`
    // strings on `LibraryLinkOutcome` MUST be matched in
    // `static/js/search.js` or the toast falls back to "Sent".
    let link_status = link_outcome
        .as_ref()
        .map(|o| o.tag())
        .unwrap_or("not_attempted");
    // Derive the toast's series title from the user's CURRENT
    // `config.title_language` preference, picking from
    // `series.title_english` / `_romaji` / `_native` rather than the
    // `series.title` column (which was frozen in whatever language
    // was active at series-add time and doesn't update on later
    // preference changes). For the AL-only branches the
    // `al_title` was already derived with the current pref inside
    // the resolver, so no second pick is needed there.
    let title_pref = crate::services::library_link::title_language(&state.db).await;
    let series_title: Option<String> = match &link_outcome {
        Some(crate::services::library_link::LibraryLinkOutcome::LinkedExisting {
            series, ..
        })
        | Some(crate::services::library_link::LibraryLinkOutcome::LinkedByAnilist {
            series, ..
        })
        | Some(crate::services::library_link::LibraryLinkOutcome::AutoAdded { series, .. }) => {
            let picked = crate::services::library_link::pick_title(
                &title_pref,
                &series.title_english,
                &series.title_romaji,
                &series.title_native,
            );
            // pick_title can return "" only when all three slots are
            // empty — degenerate; fall back to the persisted
            // series.title rather than emitting an empty toast.
            Some(if picked.is_empty() {
                series.title.clone()
            } else {
                picked.to_string()
            })
        }
        Some(crate::services::library_link::LibraryLinkOutcome::AmbiguousMatch {
            al_title,
            ..
        })
        | Some(crate::services::library_link::LibraryLinkOutcome::AutoAddDisabled {
            al_title,
            ..
        })
        | Some(crate::services::library_link::LibraryLinkOutcome::DetailFetchFailed {
            al_title,
            ..
        }) => Some(al_title.clone()),
        Some(crate::services::library_link::LibraryLinkOutcome::NoMatch { .. }) | None => None,
    };
    let detail = match &link_outcome {
        Some(crate::services::library_link::LibraryLinkOutcome::AmbiguousMatch {
            parsed_title,
            al_title,
        }) => Some(format!(
            "Parsed \"{}\" matched AL \"{}\" but tokens did not overlap",
            parsed_title, al_title
        )),
        Some(crate::services::library_link::LibraryLinkOutcome::AutoAddDisabled {
            al_title,
            ..
        }) => Some(format!("AL match \"{}\"; auto-add toggle is off", al_title)),
        Some(crate::services::library_link::LibraryLinkOutcome::DetailFetchFailed {
            al_title,
            ..
        }) => Some(format!(
            "Matched AL \"{}\" but detail fetch failed; will retry on next sync",
            al_title
        )),
        Some(crate::services::library_link::LibraryLinkOutcome::NoMatch { parsed_title }) => {
            Some(format!(
                "No library or AL match (parsed=\"{}\")",
                parsed_title.as_deref().unwrap_or("")
            ))
        }
        _ => None,
    };

    Ok(Json(serde_json::json!({
        "ok": true,
        "link_status": link_status,
        "series_title": series_title,
        "detail": detail,
        // Canonical id from the download client (BT: info_hash; SAB:
        // nzo_id). The frontend stores it on the Grab button so the
        // post-grab "Cancel" action knows what to delete via
        // /api/downloads/delete. Without this, manual-search SAB
        // grabs (when newznab indexers are wired up to the search
        // page) couldn't cancel — `row.dataset.infoHash` is the
        // pre-add BT hash, useless for SAB queue lookups.
        "hash": canonical_id,
    })))
}

#[utoipa::path(
    get,
    path = "/api/torrents",
    tag = "Downloads",
    summary = "List active torrents",
    description = "Returns all torrents currently in the download client's queue.",
    responses(
        (status = 200, description = "Torrent list", body = Vec<crate::services::download_client::DownloadItem>),
        (status = 400, description = "Download client not configured"),
    ),
)]
pub async fn get_torrents(
    State(state): State<AppState>,
) -> Result<
    Json<Vec<crate::services::download_client::DownloadItem>>,
    (axum::http::StatusCode, String),
> {
    // Fan out across every enabled client so SAB / Usenet jobs
    // appear in the queue alongside torrent jobs. Mirrors the
    // server-rendered queue tab in `handlers::downloads`.
    let pool = state.download_clients.read().await.clone();
    if pool.clients.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "Download client not configured".to_string(),
        ));
    }
    let mut torrents: Vec<crate::services::download_client::DownloadItem> = Vec::new();
    for c in pool.clients.values() {
        match c.list_scoped().await {
            Ok(mut items) => torrents.append(&mut items),
            Err(e) => {
                tracing::warn!("get_torrents: client list_scoped failed: {e}");
            }
        }
    }
    Ok(Json(torrents))
}
