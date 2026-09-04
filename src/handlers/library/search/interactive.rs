//! Interactive search handlers — the user-driven counterparts to the
//! auto-search pipeline. Each endpoint takes a query, scores Nyaa
//! results, and returns them as JSON (no automatic grab decision —
//! the user picks which release to grab).
//!
//! `search_batch_releases` is here too, even though it returns batch-
//! flavored results: it shares the per-series search shape with the
//! single-episode interactive search and is gated on the same caller
//! permissions, so it sits closer to the per-episode interactive
//! handlers than to the auto-search pipeline.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
};
use axum_htmx::HxRequest;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags};
use crate::services::{auto_search, logger, progress};

use super::super::reconcile::resolve_series_context;
use super::auto_search::{AutoSearchQuery, batch_episode_numbers, display_title_for_progress};

/// Pre-computed display fields for one row in the interactive-search
/// table. Mirrors the AL-search migration's `SearchResultRow` shape:
/// title-language picking and JSON-in-attribute payload built in Rust
/// so the Askama template stays simple and Askama auto-escape replaces
/// the prior hand-rolled `escHtml` / `escAttr` calls in JS.
struct InteractiveSearchRow {
    result: crate::services::nyaa::SearchResult,
    /// `"score-high"`, `"score-mid"`, or `"score-low"` — same thresholds
    /// (>=80 / >=40 / else) the JS used at
    /// `series_interactive_search.js::renderInteractiveResults`. The
    /// `r.score >= 60` cutoff in `templates/search.html` is a different
    /// surface (manual Nyaa search) and isn't shared here on purpose;
    /// interactive search shows scored hits per series, where the band
    /// boundaries shift higher.
    score_class: &'static str,
    /// `indexer_name` falls back to "Nyaa" when empty so the column
    /// always renders a source attribution.
    indexer_display: String,
    /// Pre-serialized JSON of `result`. Embedded into the Grab button's
    /// `data-result` attribute; the JS click handler reads it via
    /// `JSON.parse(btn.dataset.result)` instead of the previous
    /// module-scope `_isearchResults[idx]` array, so the rendered DOM
    /// is the source of truth for grab metadata.
    data_result_json: String,
}

#[derive(Template)]
#[template(path = "partials/series/interactive_search_table.html")]
pub(super) struct InteractiveSearchTablePartial {
    rows: Vec<InteractiveSearchRow>,
    /// `Some(N)` for per-episode flow → Grab button calls
    /// `grabInteractiveResult(N, this)`. `None` for the batch flow →
    /// `grabInteractiveBatchResult(this)`. The two flows share this
    /// partial because the table itself is identical; only the click
    /// target differs.
    grab_episode_number: Option<i32>,
    /// Rendered when `rows.is_empty()`. Per-episode shows "No results
    /// found."; batch shows "No batch releases found." — matches the
    /// pre-migration JS copy verbatim.
    empty_message: &'static str,
    /// Direction line under the lead — what to try next, in the same
    /// voice as the calendar's empty state. An empty result with no
    /// next step is a dead end.
    empty_hint: &'static str,
}

fn build_interactive_search_partial(
    results: Vec<crate::services::nyaa::SearchResult>,
    grab_episode_number: Option<i32>,
) -> InteractiveSearchTablePartial {
    let rows = results
        .into_iter()
        .map(|result| {
            let score_class = if result.score >= 80 {
                "score-high"
            } else if result.score >= 40 {
                "score-mid"
            } else {
                "score-low"
            };
            let indexer_display = if result.indexer_name.is_empty() {
                "Nyaa".to_string()
            } else {
                result.indexer_name.clone()
            };
            let data_result_json = serde_json::to_string(&result).unwrap_or_else(|_| "{}".into());
            InteractiveSearchRow {
                result,
                score_class,
                indexer_display,
                data_result_json,
            }
        })
        .collect();
    let (empty_message, empty_hint) = if grab_episode_number.is_some() {
        (
            "No results found.",
            "Recent episodes can take a while to appear on indexers. A batch search may find a season pack that includes this episode; if the series uses unusual release naming, set a search override on this page.",
        )
    } else {
        (
            "No batch releases found.",
            "Batches usually appear after a season finishes airing. Per-episode searches may still find individual releases.",
        )
    };
    InteractiveSearchTablePartial {
        rows,
        grab_episode_number,
        empty_message,
        empty_hint,
    }
}

fn render_interactive_partial(
    results: Vec<crate::services::nyaa::SearchResult>,
    grab_episode_number: Option<i32>,
) -> Result<Response, (StatusCode, String)> {
    let partial = build_interactive_search_partial(results, grab_episode_number);
    let html = partial
        .render()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Html(html).into_response())
}

/// Test-only seam for the partial builder. The underlying fn + struct
/// are private to this module so callers in `tests/mod.rs` can't see
/// them; this re-exports them under `cfg(test)` without leaking into
/// production builds.
#[cfg(test)]
pub(super) mod test_helpers {
    pub fn build_partial_for_test(
        results: Vec<crate::services::nyaa::SearchResult>,
        grab_episode_number: Option<i32>,
    ) -> super::InteractiveSearchTablePartial {
        super::build_interactive_search_partial(results, grab_episode_number)
    }
}

/// Search batch releases only for a series (no single-episode grabs).
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/search-batch",
    tag = "Library",
    summary = "Search for batch releases",
    description = "Search for batch/complete-series torrent releases and grab the best match.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Batch search report", body = auto_search::AutoSearchReport),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn search_batch_releases(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Query(q): Query<AutoSearchQuery>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let progress_handle = match progress::sanitize_progress_id(q.progress_id.as_deref()) {
        Some(id) => Some(state.progress.register(id).await),
        None => None,
    };
    if let Some(h) = &progress_handle {
        h.emit("start", "info", "Searching for batch release…", None, false)
            .await;
    }

    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id_for_grab = tracked_row.as_ref().map(|s| s.id);

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    if let Some(h) = &progress_handle {
        h.emit(
            "search",
            "info",
            format!("Searching: {}", display_title_for_progress(&detail)),
            None,
            false,
        )
        .await;
    }

    // Pick the best *batch* — filtering to is_batch pre-selection instead
    // of post-selection. The old code called find_best_for_target and
    // post-filtered, which returned None whenever the top-scored result
    // was a single-episode weekly release (i.e. almost every popular show
    // with active weekly seeders).
    let best = auto_search::find_best_batch_for_target(
        &state.db,
        &detail,
        &cfg,
        &auto_search::SearchTarget::Single,
        &cfs,
        &state.indexers,
    )
    .await;

    // Up-front "any client configured?" check. The per-release client
    // resolution lives below in the `Some(result)` arm so a newznab
    // (NZB) result from a SAB-pinned indexer routes to SAB instead of
    // the torrent default — see `auto_search.rs` for the same fix.
    if state.default_download_client().await.is_none() {
        if let Some(h) = &progress_handle {
            let h = h.clone();
            tokio::spawn(async move {
                h.emit(
                    "error",
                    "error",
                    "Download client not configured",
                    None,
                    true,
                )
                .await;
            });
        }
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "Download client not configured".to_string(),
        ));
    }

    match best {
        None => {
            if let Some(h) = &progress_handle {
                h.emit("done", "warn", "No batch release found", None, true)
                    .await;
            }
            Err((
                axum::http::StatusCode::NOT_FOUND,
                "No batch release found".to_string(),
            ))
        }
        Some(result) => {
            // Per-release client routing: resolve through the indexer
            // pin so an NZB result from a newznab (SAB-pinned)
            // indexer dispatches to SAB, not to the torrent default.
            // Mirrors `run_auto_search_targets_with_upgrades` and the
            // manual-grab handler in `grab.rs`.
            let (qbit, dispatch_client_id) =
                match state.client_for_indexer_with_id(result.indexer_id).await {
                    Some(pair) => pair,
                    None => {
                        if let Some(h) = &progress_handle {
                            let h = h.clone();
                            tokio::spawn(async move {
                                h.emit(
                                    "error",
                                    "error",
                                    "No download client configured for this indexer",
                                    None,
                                    true,
                                )
                                .await;
                            });
                        }
                        return Err((
                            axum::http::StatusCode::BAD_REQUEST,
                            "No download client configured for this indexer".to_string(),
                        ));
                    }
                };
            let url = if !result.magnet.is_empty() {
                result.magnet.clone()
            } else {
                result.torrent.clone()
            };
            if url.is_empty() {
                if let Some(h) = &progress_handle {
                    h.emit(
                        "error",
                        "error",
                        "No magnet/torrent URL for batch release",
                        None,
                        true,
                    )
                    .await;
                }
                return Err((
                    axum::http::StatusCode::BAD_GATEWAY,
                    "No magnet/torrent URL for batch release".to_string(),
                ));
            }
            if let Some(h) = &progress_handle {
                h.emit(
                    "grab",
                    "info",
                    format!("Grabbing {}", result.title),
                    None,
                    false,
                )
                .await;
            }
            qbit.add_torrent(&url, &result.info_hash)
                .await
                .map_err(|e| {
                    if let Some(h) = &progress_handle {
                        let h = h.clone();
                        let err = e.clone();
                        tokio::spawn(async move {
                            h.emit(
                                "error",
                                "error",
                                "qBittorrent rejected the torrent",
                                Some(err),
                                true,
                            )
                            .await;
                        });
                    }
                    (axum::http::StatusCode::BAD_GATEWAY, e)
                })?;
            let classification = crate::services::source::classify_release(
                &state.db,
                &result.title,
                Some(&result.resolution),
                Some(crate::services::source::NyaaContext {
                    info_hash: &result.info_hash,
                    view_url: &result.link,
                    is_batch: result.is_batch,
                }),
                Some(crate::services::source::SeriesContext {
                    status: &detail.status,
                    season_year: detail.season_year,
                    end_year: detail.end_year,
                }),
            )
            .await;
            let tier_label = classification.label();
            logger::info(
                &state.db,
                LogCategory::Grab,
                &format!("Grabbed batch: {}", result.title),
                &format!(
                    "group={}, score={}, tier={}{}",
                    result.group,
                    result.score,
                    tier_label,
                    crate::services::auto_search::MatchProvenance::log_suffix(
                        result.match_provenance.as_ref()
                    )
                ),
            )
            .await;
            if let Some(sid) = series_id_for_grab {
                // Parse episode list from the batch title so every covered
                // episode gets a per-episode `episode_quality_tags` row at
                // grab time, not just at post-processing time. Without
                // this the UI shows UNKNOWN for every episode of a
                // freshly-grabbed batch — and if the user has
                // post-processing disabled the rows never get created at
                // all. Mirrors the auto-search-path logic at
                // `run_auto_search_targets_with_upgrades` (look for
                // `parse_release_numbers` above).
                //
                // Fallback when the title carries no explicit range
                // (e.g. "Jellyfish Can't Swim in the Night" with no
                // "01-12" suffix): use the series' known episode count.
                // Capped at 1000 so a garbage AniList record can't
                // spawn a million rows.
                let ep_nums = batch_episode_numbers(&result.title, &detail);
                let grab_id = crate::models::grabbed_torrents::record_grab(
                    &state.db,
                    &result.info_hash,
                    &result.title,
                    sid,
                    &ep_nums,
                    result.is_batch,
                )
                .await
                .ok()
                .flatten();
                // Misgrab guardrails: keep the URL so Restore can re-add a removed grab.
                if let Some(gid) = grab_id {
                    let _ =
                        crate::models::grabbed_torrents::set_source_url(&state.db, gid, &url).await;
                }
                // Stamp the resolved download_client_id on the grab
                // row so per-grab delete routing
                // (`state.resolve_grab_client`) sends the eventual
                // delete to the same client that received the add.
                if let Some(gid) = grab_id {
                    let _ = crate::models::grabbed_torrents::set_download_client(
                        &state.db,
                        gid,
                        Some(dispatch_client_id),
                    )
                    .await;
                    // Issue #118 — fire `Grabbed` on the interactive
                    // batch auto-grab path. Same context shape as
                    // auto_search (indexer + score + client_kind).
                    let indexer = crate::services::notifications::resolve_indexer_name(
                        &state,
                        result.indexer_id,
                    )
                    .await;
                    crate::services::notifications::emit_grabbed(
                        &state,
                        sid,
                        ep_nums.first().copied().unwrap_or(0),
                        &result.title,
                        indexer,
                        Some(result.score),
                        Some(qbit.sonarr_impl_name().to_string()),
                    )
                    .await;
                }
                for ep_num in &ep_nums {
                    let _ = episode_tags::record_grab_with_match(
                        &state.db,
                        sid,
                        *ep_num,
                        &classification,
                        &result.title,
                        &result.group,
                        result.size_bytes,
                        result.is_batch,
                        result.match_provenance.as_ref(),
                    )
                    .await;
                }
            }
            if let Some(h) = &progress_handle {
                h.emit(
                    "done",
                    "success",
                    "Batch grabbed",
                    Some(format!("{} ({})", result.title, tier_label)),
                    true,
                )
                .await;
            }
            Ok(Json(auto_search::AutoSearchReport {
                notes: Vec::new(),
                grabbed: vec![auto_search::AutoSearchHit {
                    target_label: "Batch".to_string(),
                    release_title: result.title,
                    release_group: result.group,
                    quality_tier: tier_label,
                    url,
                    score: result.score,
                }],
                skipped: vec![],
                quality_profile: cfg.quality_profile,
                cancelled: false,
            }))
        }
    }
}

/// Interactive search: return all scored candidates for an episode without grabbing.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/interactive-search/{episode_number}",
    tag = "Library",
    summary = "Interactive episode search",
    description = "Search Nyaa for all available releases of a specific episode, returning scored results for manual selection.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number to search for"),
    ),
    responses(
        (status = 200, description = "Search results", body = Vec<crate::services::nyaa::SearchResult>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn interactive_search_episode(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Result<Response, (StatusCode, String)> {
    // 5-minute TTL cache so rapid reloads of the picker modal during
    // UI iteration don't hammer Nyaa. Scope-limited to interactive
    // search only; auto-search / RSS / manual grabs still go direct.
    let cache_key = (request_id, Some(episode_number));
    if let Some(cached) =
        crate::services::interactive_search_cache::get(&state.interactive_search_cache, cache_key)
    {
        let cached_vec: Vec<_> = (*cached).clone();
        return if is_htmx {
            render_interactive_partial(cached_vec, Some(episode_number))
        } else {
            Ok(Json(cached_vec).into_response())
        };
    }

    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    // Same single-entry collapse as auto_search_episode — the interactive
    // picker otherwise returns zero results for movies.
    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);
    let mut results = auto_search::find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        false,
        &cfs,
        &state.indexers,
    )
    .await;

    // Layer 3 (group-map) enrichment. Auto-search already runs the full
    // source pipeline so its classification is complete, but the interactive
    // picker shows results straight from nyaa::parse_results where only
    // Layer 1 (anitomy filename tokens) has fired. Filling source via the
    // group table here is what lets SubsPlease releases label as WEB-DL
    // and VCB-Studio as BluRay when the filename alone is silent.
    crate::services::nyaa::enrich_results_with_group_map(&state.db, &mut results).await;

    crate::services::interactive_search_cache::insert(
        &state.interactive_search_cache,
        cache_key,
        results.clone(),
    );
    if is_htmx {
        render_interactive_partial(results, Some(episode_number))
    } else {
        Ok(Json(results).into_response())
    }
}

/// Interactive batch search: return all scored batch candidates so the user
/// can pick one. Uses the same query sweep as the auto batch search
/// (`find_best_batch_for_target`) so the interactive and auto paths surface
/// the same candidate pool — the only difference is that this returns every
/// hit instead of picking the top.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/interactive-search-batch",
    tag = "Library",
    summary = "Interactive batch search",
    description = "Search Nyaa for batch/complete releases of a series, returning scored results for manual selection.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Batch search results", body = Vec<crate::services::nyaa::SearchResult>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn interactive_search_batches(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(request_id): Path<i64>,
) -> Result<Response, (StatusCode, String)> {
    // 5-minute TTL cache — see interactive_search_episode for rationale.
    // `None` episode slot distinguishes batch from per-episode.
    let cache_key = (request_id, None);
    if let Some(cached) =
        crate::services::interactive_search_cache::get(&state.interactive_search_cache, cache_key)
    {
        let cached_vec: Vec<_> = (*cached).clone();
        return if is_htmx {
            render_interactive_partial(cached_vec, None)
        } else {
            Ok(Json(cached_vec).into_response())
        };
    }

    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    let mut results = auto_search::collect_scored_batches_for_target(
        &state.db,
        &detail,
        &cfg,
        &auto_search::SearchTarget::Single,
        &cfs,
        &state.indexers,
        false,
    )
    .await;

    crate::services::nyaa::enrich_results_with_group_map(&state.db, &mut results).await;

    crate::services::interactive_search_cache::insert(
        &state.interactive_search_cache,
        cache_key,
        results.clone(),
    );
    if is_htmx {
        render_interactive_partial(results, None)
    } else {
        Ok(Json(results).into_response())
    }
}
