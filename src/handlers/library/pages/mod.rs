//! HTML page handlers for the library section.
//!
//! Split out of `handlers::library::mod`: `index` (home library list),
//! `needs_review_page` (cross-library needs-review feed), and
//! `series_detail` (the per-series page) plus their shared builders.
//! `build_episodes` is in here because it's called by both `series_detail`
//! and the `series_episodes_json` polling endpoint in the `episodes`
//! submodule.

use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse},
};
use sqlx::SqlitePool;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, local_metadata, monitoring, series};
use crate::services::{
    anilist, artwork, jikan, kitsu, logger, media, monitoring as monitoring_service,
};

use super::reconcile::{
    force_kitsu_fallback_enabled, populate_series_cover_urls, resolve_series_context,
};
use super::{Episode, ErrorTemplate, IndexTemplate, RelationCard, RelationGroup, SeriesTemplate};

#[derive(Default, serde::Deserialize)]
pub struct LibraryIndexQuery {
    /// #62 — `?list=<name>` filter. When present + non-empty,
    /// the index handler keeps only series whose
    /// `series_custom_lists` rows match. Echoed back to the
    /// template so the dropdown's selected-option state persists
    /// across navigations.
    #[serde(default)]
    pub list: Option<String>,
    /// `?search=<text>` library search. Case-insensitive substring
    /// match against `title_english` / `title_romaji` /
    /// `title_native`; composes with `list` (set both → series must
    /// satisfy both predicates).
    #[serde(default)]
    pub search: Option<String>,
    /// #62 — `?sort=<key>` ordering. Currently supports
    /// `recent` (default; SQL `ORDER BY added_at DESC`) and `score`
    /// (user-score descending — only meaningful when an account is
    /// linked, so the dropdown is hidden otherwise; unrated series
    /// sink to the bottom). Anything unrecognized falls through to
    /// `recent`.
    #[serde(default)]
    pub sort: Option<String>,
}

pub async fn index(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<LibraryIndexQuery>,
) -> Html<String> {
    // Fetch the library list and config concurrently — they're independent
    // and each was previously serialized on the other. `get_all` is the
    // larger query of the two so this shaves the smaller query's RTT off
    // the critical path.
    let (library_res, cfg_res) =
        tokio::join!(series::get_all(&state.db), config::get_config(&state.db),);
    let mut library = library_res.unwrap_or_default();
    let cfg = cfg_res.ok().flatten();

    populate_series_cover_urls(
        &state.db,
        &mut library,
        |item| item.id,
        |item, url| item.cover_url = url,
    )
    .await;

    // #62 — pull the linked account's score_format so library
    // cards can render "You: X" badges per row. Empty string when
    // no account is linked, in which case Series::user_score_display
    // returns None and no badge renders.
    let score_format = crate::models::external_accounts::get_current(&state.db)
        .await
        .ok()
        .flatten()
        .map(|a| a.score_format)
        .unwrap_or_default();

    // Whole-library counts for the identity row, captured BEFORE any
    // filter mutates `library` — "31 series · 2 airing" describes the
    // collection; the chips carry the per-scope counts.
    let total_count = library.len();
    let airing_count = library
        .iter()
        .filter(|s| matches!(s.status.as_str(), "RELEASING" | "CURRENTLY_AIRING"))
        .count();

    // #62 — populate the scope-chip row + apply the active
    // filter. Names+counts are alphabetized; empty result means no
    // memberships synced yet (template hides the list chips).
    let list_counts = crate::models::series_custom_lists::list_counts(&state.db)
        .await
        .unwrap_or_default();
    let custom_list_filter = q.list.unwrap_or_default();
    if !custom_list_filter.is_empty() {
        // In-memory filter against the just-loaded library. Cheaper
        // than a JOIN-based query when the library is already cached
        // — the per-series ids set is small enough that the
        // HashSet lookup on each row is sub-microsecond. A stale or
        // unknown `?list=foo` (e.g. user bookmarked the URL, then
        // synced away the last membership) yields an empty
        // matching_ids set and therefore an empty library — chosen
        // over silently dropping the filter so the dropdown's
        // still-selected value lines up with what the user sees,
        // making the staleness obvious instead of mysterious.
        let matching_ids: std::collections::HashSet<i64> =
            crate::models::series_custom_lists::series_ids_in_list(&state.db, &custom_list_filter)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
        library.retain(|s| matching_ids.contains(&s.id));
    }

    // Library search. Case-insensitive substring match against the
    // three title fields. Composes with the list filter (set both →
    // series must satisfy both predicates).
    let search_query = q.search.unwrap_or_default();
    if !search_query.trim().is_empty() {
        let needle = search_query.trim().to_lowercase();
        library.retain(|s| {
            s.title_english.to_lowercase().contains(&needle)
                || s.title_romaji.to_lowercase().contains(&needle)
                || s.title_native.to_lowercase().contains(&needle)
        });
    }

    // #62 — sort-by-user-score. SQL already returned series
    // ordered by added_at DESC ("recent"); this is an opt-in
    // re-sort applied AFTER filters so the displayed order matches
    // the displayed set. NULL / 0.0 / negative user_score values
    // (unrated, manually-added pre-PR-C, etc.) sort to the bottom
    // so they don't crowd out the rated ones the user is presumably
    // looking at.
    // Sort selector. SQL already returned series ordered by
    // added_at DESC ("recent"); anything else is an opt-in re-sort
    // applied AFTER filters so the displayed order matches the
    // displayed set. Score-based sorts gate on `!score_format.is_empty()`
    // (an external account is linked); title sorts and oldest-first
    // are universal. Unknown keys fall through to "recent".
    let sort_key = q.sort.as_deref().unwrap_or("recent");
    // Tiebreaker for non-title primary sorts: title_english,
    // case-insensitive, with romaji/native fallback so an entry
    // missing English doesn't sort under everything.
    let title_key = |s: &series::Series| -> String {
        let raw = if !s.title_english.is_empty() {
            &s.title_english
        } else if !s.title_romaji.is_empty() {
            &s.title_romaji
        } else {
            &s.title_native
        };
        raw.to_lowercase()
    };
    let sort_value = match sort_key {
        "score" if !score_format.is_empty() => {
            // partial_cmp with NaN-safe ordering: any non-positive
            // or missing score becomes -1.0 so it sinks. Tiebreaker
            // on title keeps the order deterministic across renders
            // for series at the same score.
            library.sort_by(|a, b| {
                let av = a.user_score.filter(|s| *s > 0.0).unwrap_or(-1.0);
                let bv = b.user_score.filter(|s| *s > 0.0).unwrap_or(-1.0);
                bv.partial_cmp(&av)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| title_key(a).cmp(&title_key(b)))
            });
            "score".to_string()
        }
        "score_asc" if !score_format.is_empty() => {
            // Inverse: low → high. Unrated entries still sink — a
            // missing score isn't a 0, conceptually it's "no
            // opinion," and surfacing those above the user's
            // explicit ratings on either end of the range is
            // confusing.
            library.sort_by(|a, b| {
                let av = a.user_score.filter(|s| *s > 0.0).unwrap_or(f64::INFINITY);
                let bv = b.user_score.filter(|s| *s > 0.0).unwrap_or(f64::INFINITY);
                av.partial_cmp(&bv)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| title_key(a).cmp(&title_key(b)))
            });
            "score_asc".to_string()
        }
        "title_asc" => {
            library.sort_by_key(title_key);
            "title_asc".to_string()
        }
        "title_desc" => {
            library.sort_by_key(|s| std::cmp::Reverse(title_key(s)));
            "title_desc".to_string()
        }
        "oldest" => {
            // Sort directly on `added_at` (ISO-8601 from SQLite's
            // CURRENT_TIMESTAMP, lexicographically chronological)
            // rather than reverse-of-SQL-default — the latter would
            // break silently if a future caller's needs reshaped
            // `series::get_all`'s ORDER BY clause.
            library.sort_by(|a, b| {
                a.added_at
                    .cmp(&b.added_at)
                    .then_with(|| title_key(a).cmp(&title_key(b)))
            });
            "oldest".to_string()
        }
        _ => "recent".to_string(),
    };

    // Decompose the canonical sort value into the key+direction pair
    // the two-part sort control renders. Recomposition happens in
    // static/js/index.js (librarySortNavigate); the canonical values
    // in the URL and handler are unchanged.
    let (sort_key, sort_desc) = match sort_value.as_str() {
        "oldest" => ("recent", false),
        "title_asc" => ("title", false),
        "title_desc" => ("title", true),
        "score" => ("score", true),
        "score_asc" => ("score", false),
        _ => ("recent", true),
    };

    // Per-card completeness summaries ("do I have what's aired?").
    // Three sources, each one round-trip and proportional to the
    // library: a batched folder scan (single spawn_blocking hop), the
    // aired-count GROUP BY, and the active-tag-state slice. Computed
    // AFTER filter + sort so only rendered cards pay for it.
    let media_root = cfg
        .as_ref()
        .map(|c| c.media_root.clone())
        .unwrap_or_default();
    let folder_list: Vec<(i64, String)> = library
        .iter()
        .map(|s| (s.id, s.folder_name.clone()))
        .collect();
    let (disk_map, aired_map, tag_rows) = tokio::join!(
        media::scan_series_folders_batch(&media_root, folder_list),
        local_metadata::aired_episode_counts(&state.db),
        episode_tags::active_states_all_series(&state.db),
    );
    let aired_map = aired_map.unwrap_or_default();
    let mut completed_by_series: HashMap<i64, std::collections::HashSet<i32>> = HashMap::new();
    let mut downloading_series: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for (sid, ep, tag_state) in tag_rows.unwrap_or_default() {
        if tag_state == "grabbed" {
            downloading_series.insert(sid);
        } else {
            completed_by_series.entry(sid).or_default().insert(ep);
        }
    }
    let cards: Vec<(series::Series, super::CardProgress)> = library
        .into_iter()
        .map(|s| {
            let total = i64::from(s.episodes.unwrap_or(0));
            // Downloaded = distinct episode numbers on disk or with a
            // completed grab tag — same union the series page's
            // `downloaded` flag uses. The season filter mirrors
            // build_episodes: with a known episode count, only
            // season-1 / unseasoned files belong to the main list.
            let mut have: std::collections::HashSet<i32> = std::collections::HashSet::new();
            if let Some(files) = disk_map.get(&s.id) {
                for f in files {
                    if f.episode_number <= 0 {
                        continue;
                    }
                    if total > 0 && matches!(f.season_number, Some(n) if n != 1) {
                        continue;
                    }
                    have.insert(f.episode_number);
                }
            }
            if let Some(eps) = completed_by_series.get(&s.id) {
                have.extend(eps);
            }
            let downloaded = have.len() as i64;
            let aired = match aired_map.get(&s.id) {
                Some(&n) if n > 0 => n,
                // No cached air dates. NOT_YET_RELEASED genuinely has
                // nothing aired; everything else counts against the
                // total (exact for FINISHED, honest degradation for
                // releasing series with sparse Jikan-fallback data).
                _ if s.status == "NOT_YET_RELEASED" => 0,
                _ => total,
            };
            let downloading = downloading_series.contains(&s.id);
            let pct = if aired > 0 {
                (downloaded * 100 / aired).clamp(0, 100)
            } else {
                0
            };
            // While downloading, guarantee a visible sliver even at
            // zero on-disk episodes — an invisible "in flight" state
            // defeats the point of the bar.
            let pct = if downloading { pct.max(6) } else { pct };
            let (card_state, label) = if downloading {
                if aired > 0 {
                    (
                        "downloading",
                        format!(
                            "Downloading; {} of {} aired episodes on disk",
                            downloaded, aired
                        ),
                    )
                } else {
                    ("downloading", "Downloading".to_string())
                }
            } else if aired == 0 {
                ("idle", "Nothing aired yet".to_string())
            } else if downloaded >= aired {
                (
                    "complete",
                    format!("All {} aired episodes downloaded", aired),
                )
            } else {
                (
                    "missing",
                    format!("{} of {} aired episodes downloaded", downloaded, aired),
                )
            };
            let progress = super::CardProgress {
                state: card_state.to_string(),
                downloaded,
                aired,
                pct,
                monitored: s.monitor_mode != "none",
                label,
            };
            (s, progress)
        })
        .collect();

    let recycle_enabled = cfg
        .as_ref()
        .map(|c| !c.recycle_bin_path.trim().is_empty())
        .unwrap_or(false);
    let recycle_count = if recycle_enabled {
        crate::services::recycle::cached_entry_count(
            cfg.as_ref()
                .map(|c| c.recycle_bin_path.as_str())
                .unwrap_or(""),
        )
        .await
    } else {
        0
    };
    let template = IndexTemplate {
        page: "library".to_string(),
        cards,
        title_language: cfg
            .map(|c| c.title_language)
            .unwrap_or_else(|| "english".to_string()),
        recycle_enabled,
        recycle_count,
        score_format,
        list_counts,
        custom_list_filter,
        search_query,
        sort_value,
        sort_key: sort_key.to_string(),
        sort_desc,
        total_count,
        airing_count,
    };
    Html(template.render().unwrap_or_default())
}

/// `/library/review` used to render its own page. It's now a System
/// tab (`/system?tab=review`) — redirect there so anything
/// bookmarked, linked, or cached still resolves.
///
/// Phase C / D of the hx-boost rollout: under `hx-boost` an `<a>`
/// click is fetched with `fetch`, which transparently follows 3xx
/// redirects — htmx never sees the redirect, only the final
/// destination's HTML. The pushState'd URL stays at the ORIGINAL
/// click target (`/library/review`) while the rendered content is
/// the destination (`/system?tab=review`), producing an awkward
/// URL/content mismatch in the address bar.
///
/// `htmx_aware_redirect_from_req` solves this: HTMX callers get
/// `200 OK` with `HX-Redirect: /system?tab=review`, which htmx
/// translates into a real `window.location` navigation that updates
/// both URL and content together. Plain (non-HTMX) callers — direct
/// browser nav, bookmarks, third-party links — fall through to the
/// `Redirect::permanent` path so search-engine cache invalidation
/// and deep-linking still work the way 308 promises.
pub async fn needs_review_page(
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    let is_htmx = req
        .headers()
        .get("HX-Request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if is_htmx {
        crate::handlers::responses::htmx_aware_redirect(true, "/system?tab=review")
    } else {
        // Non-HTMX path keeps the 308 (vs the helper's 303) so
        // search engines and HTTP caches treat the redirect as
        // permanent — a 303 from `htmx_aware_redirect` would invite
        // re-fetching `/library/review` indefinitely.
        axum::response::Redirect::permanent("/system?tab=review").into_response()
    }
}

pub async fn series_detail(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Html<String> {
    // Fetch config alongside the metadata resolve so both the error
    // and success paths can reuse it. resolve_series_context typically
    // dominates (network round trip to AniList on cold cache), so the
    // cfg fetch overlaps with it for free.
    let (resolve_res, cfg_res) = tokio::join!(
        resolve_series_context(&state.db, request_id),
        config::get_config(&state.db),
    );
    let cfg = cfg_res.ok().flatten();
    let recycle_enabled = cfg
        .as_ref()
        .map(|c| !c.recycle_bin_path.trim().is_empty())
        .unwrap_or(false);
    let title_language_fallback = || {
        cfg.as_ref()
            .map(|c| c.title_language.clone())
            .unwrap_or_else(|| "english".to_string())
    };
    let (db_series, provider_id, mut detail) = match resolve_res {
        Ok(v) => v,
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::AniList,
                &format!("Failed to fetch detail for {}", request_id),
                &e,
            )
            .await;
            let (title, message, tech_detail) = if e.contains("403") {
                (
                    "Metadata Provider Unavailable".to_string(),
                    "The metadata API is temporarily unavailable. This usually resolves itself within a few hours. Try again later.".to_string(),
                    e,
                )
            } else if e.contains("not found") || e.contains("Not Found") {
                (
                    "Series Not Found".to_string(),
                    format!(
                        "Could not find a series with ID {}. It may have been removed from the metadata provider.",
                        request_id
                    ),
                    e,
                )
            } else {
                (
                    "Something Went Wrong".to_string(),
                    "An error occurred while loading this series. Please try again.".to_string(),
                    e,
                )
            };
            let template = ErrorTemplate {
                page: "library".to_string(),
                title,
                message,
                detail: tech_detail,
                title_language: title_language_fallback(),
            };
            return Html(template.render().unwrap_or_default());
        }
    };
    let is_tracked = db_series.is_some();
    let db_id = db_series.as_ref().map(|s| s.id);
    let folder_name = db_series
        .as_ref()
        .map(|s| s.folder_name.clone())
        .unwrap_or_default();

    // Ensure monitoring rows first — this writes to DB, and `build_episodes`
    // below reads the monitored set, so these cannot run concurrently
    // without a read-your-writes race. Everything *after* this point is
    // read-only and fans out in parallel.
    let mut monitor_mode = "future".to_string();
    let mut monitor_mode_label = monitoring::MonitorMode::Future.label().to_string();
    let monitor_mode_manual_override = db_series
        .as_ref()
        .map(|s| s.monitor_mode_manual_override)
        .unwrap_or(false);
    if let Some(ref tracked) = db_series {
        if let Ok(summary) =
            monitoring_service::ensure_series_monitoring_rows(&state.db, tracked).await
        {
            monitor_mode = summary.mode.as_str().to_string();
            monitor_mode_label = summary.mode.label().to_string();
        } else {
            monitor_mode = tracked.monitor_mode.clone();
            monitor_mode_label = tracked.monitor_mode_enum().label().to_string();
        }
    }

    // #62 — derive the "Sync from AL/MAL" dropdown option's
    // visibility + label. Only show when both (a) an account is
    // currently linked, and (b) this series row has a non-NULL
    // synced_from_external_account_id pointing at the same account.
    // Rule (b) keeps manually-added series (synced_from = NULL) from
    // showing an option that wouldn't do anything useful; if the
    // user later puts the manual series on their AL list, the next
    // sync stamps synced_from and the option appears.
    let synced_from = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT synced_from_external_account_id FROM series WHERE id = ?",
    )
    .bind(db_id.unwrap_or(0))
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .flatten();
    let linked_account = crate::models::external_accounts::get_current(&state.db)
        .await
        .ok()
        .flatten();
    let can_sync_from_external_account = matches!(
        (linked_account.as_ref(), synced_from),
        (Some(acct), Some(sf)) if acct.id == sf
    );
    let sync_provider_label = match linked_account.as_ref().map(|a| a.provider.as_str()) {
        Some(crate::models::external_accounts::PROVIDER_ANILIST) => "AniList".to_string(),
        Some(crate::models::external_accounts::PROVIDER_MAL) => "MyAnimeList".to_string(),
        _ => String::new(),
    };
    // When the series is sync-tracked and the user hasn't pinned a
    // manual mode, the dropdown shows "Sync from AL/MAL" as selected.
    // Otherwise the option matching the current monitor_mode is
    // selected. Computed here so the template doesn't need a
    // multi-clause condition per option.
    let monitor_mode_select_value =
        if can_sync_from_external_account && !monitor_mode_manual_override {
            "sync".to_string()
        } else {
            monitor_mode.clone()
        };

    // #62 — render the "You: X" badge string per the linked
    // account's score_format. Hidden when no account is linked, the
    // series has no user_score, or the score is the unrated
    // sentinel. Computed here so the template just renders the
    // already-formatted string.
    let user_score_display = match (db_series.as_ref(), linked_account.as_ref()) {
        (Some(row), Some(acct)) => {
            crate::services::user_score::format_user_score(row.user_score, &acct.score_format)
        }
        _ => None,
    };

    // #62 — read AL custom-list memberships for the badge row.
    // Empty when this series isn't on any user-defined list; the
    // template hides the row in that case. Sorted alphabetically by
    // the model layer so the badge order is stable across renders.
    let custom_list_memberships = match db_id {
        Some(id) => crate::models::series_custom_lists::list_for_series(&state.db, id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.list_name)
            .collect(),
        None => Vec::new(),
    };

    // Fan out the four remaining independent read paths. Each one was
    // previously awaited serially — on a cold cache that meant 4+
    // sequential DB round trips + the build_episodes fs-walk + the
    // relation-group artwork lookups all stacked end to end. Running
    // them concurrently collapses the total wait to ~max(...) instead
    // of sum(...). cfg is fetched at the top of the handler alongside
    // resolve_series_context so it's already in scope here.
    let relation_groups_fut = build_relation_groups(&state.db, db_id, &detail);

    let detail_for_episodes = detail.clone();
    let db_for_episodes = state.db.clone();
    let folder_for_episodes = folder_name.clone();
    let episodes_fut = async move {
        // Pull media_root from config inside the task so build_episodes
        // can still run in parallel with the outer config fetch. The
        // extra `get_config` hit here is harmless — the WAL page cache
        // will serve it from memory after the first concurrent fetch.
        let media_root = config::get_config(&db_for_episodes)
            .await
            .ok()
            .flatten()
            .map(|c| c.media_root)
            .unwrap_or_default();
        let out = build_episodes(
            &db_for_episodes,
            &detail_for_episodes,
            db_id,
            &folder_for_episodes,
            &media_root,
        )
        .await;
        (out, media_root)
    };

    let cover_key = db_series.as_ref().map(|s| format!("series-{}-cover", s.id));
    let banner_key = db_series
        .as_ref()
        .map(|s| format!("series-{}-banner", s.id));
    let cover_url_src = detail.cover_url.clone();
    let banner_url_src = detail.banner_url.clone();
    let detail_id = detail.id;
    let detail_mal_id = detail.id_mal;
    let db_for_art = state.db.clone();
    let cover_fut = async move {
        if let Some(key) = cover_key {
            artwork::cached_or_source_url(&db_for_art, &key, &cover_url_src).await
        } else if detail_id != 0 {
            artwork::first_cached_url(
                &db_for_art,
                &[
                    artwork::provider_cover_key(detail_id, detail_mal_id),
                    format!("provider-{}-cover", detail_id),
                ],
                &cover_url_src,
            )
            .await
        } else {
            cover_url_src
        }
    };
    let db_for_banner = state.db.clone();
    let banner_fut = async move {
        if let Some(key) = banner_key {
            artwork::cached_or_source_url(&db_for_banner, &key, &banner_url_src).await
        } else if detail_id != 0 {
            artwork::first_cached_url(
                &db_for_banner,
                &[
                    artwork::provider_banner_key(detail_id, detail_mal_id),
                    format!("provider-{}-banner", detail_id),
                ],
                &banner_url_src,
            )
            .await
        } else {
            banner_url_src
        }
    };

    // #15b — last metadata refresh + the SQL-derived `is_fresh` flag,
    // folded into the existing concurrent fan-out so it doesn't add a
    // sequential round-trip on top. Cheap (indexed provider_id lookup,
    // WAL-cached) but the pattern of the surrounding handler is "every
    // independent read goes in the join!" so stick with that.
    //
    // Issue #106 — `is_fresh` (computed by SQLite at fetch time using
    // the same TTL constant as the periodic refresh task) is the
    // canonical staleness signal. Re-deriving it client-side from
    // `cached_at` would duplicate the SQL `CASE WHEN cached_at >=
    // datetime('now', '-12 hours')` calculation; reuse the value that
    // already came back from the query.
    let db_for_refresh = state.db.clone();
    let refresh_fut = async move {
        crate::models::metadata_cache::get_by_provider_id(&db_for_refresh, provider_id)
            .await
            .ok()
            .flatten()
            .map(|row| (row.cached_at, !row.is_fresh))
            .unwrap_or_default()
    };

    let (relation_groups, episodes_out, cover_url, banner_url, refresh_meta) = tokio::join!(
        relation_groups_fut,
        episodes_fut,
        cover_fut,
        banner_fut,
        refresh_fut,
    );
    let (metadata_refreshed_at, metadata_is_stale) = refresh_meta;
    let ((episodes, on_disk_count, downloaded_count, size_display, monitored_count), media_root) =
        episodes_out;
    detail.cover_url = cover_url;
    detail.banner_url = banner_url;

    let title_language = title_language_fallback();

    let ep_total = detail.effective_episode_count();
    // #15a — render AL and MAL links independently. AL link is hidden
    // for the Jikan-fallback sentinel case (detail.id < 0); MAL link is
    // hidden only when no MAL id is known.
    let anilist_url = if detail.id > 0 {
        format!("https://anilist.co/anime/{}", detail.id)
    } else {
        String::new()
    };
    let mal_url = detail
        .id_mal
        .filter(|id| *id > 0)
        .map(|id| format!("https://myanimelist.net/anime/{}", id))
        .unwrap_or_default();

    let all_monitored = ep_total > 0 && monitored_count >= ep_total;
    let allow_upgrades = db_series.as_ref().map(|s| s.allow_upgrades).unwrap_or(true);
    // default off (untracked series have no upgrade sweep
    // anyway, so the default is moot for the .unwrap_or() branch).
    let allow_pt_upgrades = db_series
        .as_ref()
        .map(|s| s.allow_pt_upgrades)
        .unwrap_or(false);
    let custom_query_tokens = db_series
        .as_ref()
        .map(|s| s.custom_query_tokens.clone())
        .unwrap_or_default();
    let restrict_to_uploader = db_series
        .as_ref()
        .map(|s| s.restrict_to_uploader.clone())
        .unwrap_or_default();
    let default_custom_query_tokens = cfg
        .as_ref()
        .map(|c| c.default_custom_query_tokens.clone())
        .unwrap_or_default();
    let default_restrict_to_uploader = cfg
        .as_ref()
        .map(|c| c.default_restrict_to_uploader.clone())
        .unwrap_or_default();
    let post_processing_enabled = cfg
        .as_ref()
        .map(|c| c.post_processing_enabled)
        .unwrap_or(false);
    let grab_preview_mode = cfg
        .as_ref()
        .map(|c| c.grab_preview_mode.clone())
        .unwrap_or_else(|| "batches_only".to_string());
    // Issue #219 — the read lock releases before the template renders.
    let adult_without_indexers =
        super::adult_needs_indexer(detail.is_adult, state.indexers.read().await.len());
    let template = SeriesTemplate {
        page: "library".to_string(),
        route_id: db_id.unwrap_or(provider_id),
        detail,
        is_tracked,
        db_id,
        folder_name,
        media_root,
        episodes,
        ep_total,
        on_disk_count,
        downloaded_count,
        size_display,
        title_language,
        relation_groups,
        anilist_url,
        mal_url,
        metadata_refreshed_at,
        metadata_is_stale,
        adult_without_indexers,
        recycle_enabled,
        monitor_mode,
        monitor_mode_label,
        monitor_mode_manual_override,
        can_sync_from_external_account,
        sync_provider_label,
        monitor_mode_select_value,
        user_score_display,
        custom_list_memberships,
        monitored_count,
        all_monitored,
        allow_upgrades,
        allow_pt_upgrades,
        custom_query_tokens,
        restrict_to_uploader,
        default_custom_query_tokens,
        default_restrict_to_uploader,
        post_processing_enabled,
        grab_preview_mode,
    };
    Html(template.render().unwrap_or_default())
}

/// Maximum number of missing trailing Jikan episodes we'll tolerate before
/// falling back to Kitsu. MAL typically lags AniList's airing schedule by 1-2
/// episodes for long-running series (One Piece being the canonical case).
/// Without this tolerance, every One Piece page load re-runs the Kitsu title
/// search (`best_candidate` hits the Kitsu HTTP API before checking the
/// episode cache) to backfill 1-2 trailing episodes. And for long-running
/// shows Kitsu over-counts anyway — it lists episodes past the actual aired
/// count — so falling back here wouldn't even give us accurate titles.
const JIKAN_MAL_LAG_TOLERANCE: i32 = 10;

fn episode_needs_kitsu_backfill<F>(ep_count: i32, mut has_jikan_title: F) -> bool
where
    F: FnMut(i32) -> bool,
{
    if ep_count <= 1 {
        return false;
    }

    let missing = (1..=ep_count)
        .filter(|ep_num| !has_jikan_title(*ep_num))
        .count() as i32;
    missing > JIKAN_MAL_LAG_TOLERANCE
}

/// Build the episode list for a single series (no chain walking).
pub(super) async fn build_episodes(
    db: &SqlitePool,
    detail: &anilist::AnimeDetail,
    db_id: Option<i64>,
    folder_name: &str,
    media_root: &str,
) -> (Vec<Episode>, i32, i32, String, i32) {
    let ep_count = detail.effective_episode_count();
    // Fan out the four independent pre-fetches in parallel:
    //   1. disk file walk (blocking pool)
    //   2. cached episode metadata map (DB, with a fallback path)
    //   3. force_kitsu_fallback config flag (DB)
    //   4. monitored-episode set (DB, only when the series is tracked)
    //   5. per-episode quality tags (DB, only when the series is tracked)
    let disk_files_fut = media::scan_series_folder(media_root, folder_name);

    let detail_id = detail.id;
    let cached_eps_fut = async move {
        if let Some(sid) = db_id {
            let rows = local_metadata::get_episode_map_for_series(db, sid)
                .await
                .unwrap_or_default();
            if rows.is_empty() && detail_id != 0 {
                local_metadata::get_episode_map_for_provider(db, detail_id)
                    .await
                    .unwrap_or_default()
            } else {
                rows
            }
        } else if detail_id != 0 {
            local_metadata::get_episode_map_for_provider(db, detail_id)
                .await
                .unwrap_or_default()
        } else {
            HashMap::new()
        }
    };

    let monitored_fut = async move {
        match db_id {
            Some(id) => monitoring::get_monitored_episode_numbers(db, id)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect::<std::collections::HashSet<i32>>(),
            None => std::collections::HashSet::new(),
        }
    };

    let quality_tags_fut = async move {
        match db_id {
            Some(id) => episode_tags::get_for_series(db, id)
                .await
                .unwrap_or_default(),
            None => std::collections::HashMap::new(),
        }
    };

    let (disk_files, cached_eps, force_kitsu_fallback, monitored_lookup, quality_tags) = tokio::join!(
        disk_files_fut,
        cached_eps_fut,
        force_kitsu_fallback_enabled(db),
        monitored_fut,
        quality_tags_fut,
    );
    let cached_matches_force =
        !force_kitsu_fallback || cached_eps.values().any(|ep| ep.source == "kitsu");
    let use_cached_eps = !cached_eps.is_empty() && cached_matches_force;

    let episodic_format = !matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA");
    // Issue #56: airing series whose episode total isn't known yet
    // (typical for MAL-fed currently-airing entries — Jikan reports
    // `episodes: null`) need the Jikan episodes endpoint as the *source*
    // of episode rows, not just titles. Without the `is_airing` arm an
    // ONA-format airing show like JoJo SBR ends up with `episodic_format
    // = false` AND `ep_count == 0`, so Jikan is skipped, the main
    // 1..=ep_count render loop emits nothing, and the page reads as a
    // zero-episode series even though `/anime/{id}/episodes` would have
    // returned the aired list.
    let is_airing_status = matches!(detail.status.as_str(), "RELEASING" | "CURRENTLY_AIRING");
    let should_fetch_jikan = !use_cached_eps
        && detail.id_mal.is_some()
        && (episodic_format || ep_count > 1 || is_airing_status);
    let jikan_eps = if should_fetch_jikan {
        jikan::fetch_episode_titles_for_detail(db, detail).await
    } else {
        HashMap::new()
    };

    // Promote the larger of (fresh Jikan fetch, locally-cached episode
    // map) into ep_count for airing series whose total wasn't known.
    // The downstream render loop (`for ep_num in 1..=ep_count`), the
    // template's `ep_total > 0` section gate, and the monitoring
    // counters all key off ep_count, so without this the fetched
    // episodes stay invisible.
    //
    // Both arms are needed: `jikan_eps` is only populated when
    // `should_fetch_jikan` fires, which requires `!use_cached_eps`. On
    // the cached path Jikan was skipped and `jikan_eps` stays empty, so
    // the promotion from `jikan_eps.len()` alone would be a no-op —
    // leaving an airing series rendered empty on every revisit after
    // the initial sync populated the local episode map.
    let ep_count = ep_count.max(jikan_eps.len() as i32).max(if use_cached_eps {
        cached_eps.len() as i32
    } else {
        0
    });

    let should_try_kitsu = !use_cached_eps
        && ep_count > 1
        && (force_kitsu_fallback
            || episode_needs_kitsu_backfill(ep_count.max(0), |ep_num| {
                jikan_eps
                    .get(&ep_num)
                    .map(|info| !info.title.trim().is_empty())
                    .unwrap_or(false)
            }));
    let kitsu_eps: HashMap<i32, kitsu::EpisodeInfo> = if should_try_kitsu {
        kitsu::fetch_episode_titles_fallback(
            db,
            &[
                detail.title_english.clone(),
                detail.title_romaji.clone(),
                detail.title_native.clone(),
            ],
            detail.season_year,
            detail.episodes,
        )
        .await
    } else {
        HashMap::new()
    };

    let is_tracked = db_id.is_some();

    let mut episodes = Vec::new();
    let mut on_disk_count = 0i32;
    let mut downloaded_count = 0i32;
    let mut total_size: u64 = 0;
    let mut monitored_count = 0i32;

    // For the unaired-vs-missing split below. Date-only, UTC: provider
    // air dates are calendar dates, not timestamps.
    let today = chrono::Utc::now().date_naive();

    for ep_num in 1..=ep_count.max(0) {
        let disk_match = disk_files.iter().find(|f| {
            if let Some(s) = f.season_number {
                s == 1 && f.episode_number == ep_num
            } else {
                f.episode_number == ep_num
            }
        });

        let (on_disk, quality, size_display, size_bytes, filename) = match disk_match {
            Some(f) => (
                true,
                f.quality.clone(),
                f.size_display.clone(),
                f.size_bytes as i64,
                f.filename.clone(),
            ),
            None => (false, String::new(), String::new(), 0i64, String::new()),
        };

        if on_disk {
            on_disk_count += 1;
            if let Some(f) = disk_match {
                total_size += f.size_bytes;
            }
        }

        let use_series_fallback = ep_count <= 1;
        let fallback_title = if use_series_fallback {
            preferred_title(
                &detail.title_english,
                &detail.title_romaji,
                &detail.title_native,
            )
        } else {
            String::new()
        };
        let fallback_romaji = if use_series_fallback {
            non_empty_or(&detail.title_romaji, &fallback_title)
        } else {
            String::new()
        };
        let fallback_english = if use_series_fallback {
            non_empty_or(&detail.title_english, &fallback_title)
        } else {
            String::new()
        };
        let fallback_native = if use_series_fallback {
            non_empty_or(&detail.title_native, &fallback_title)
        } else {
            String::new()
        };

        let (ep_title, ep_title_romaji, ep_title_english, ep_title_native, ep_aired) =
            if use_cached_eps {
                if let Some(info) = cached_eps.get(&ep_num) {
                    (
                        non_empty_or(&info.title, &fallback_title),
                        non_empty_or(&info.title_romaji, &fallback_romaji),
                        non_empty_or(&info.title_english, &fallback_english),
                        non_empty_or(&info.title_native, &fallback_native),
                        info.aired.clone(),
                    )
                } else {
                    (
                        fallback_title.clone(),
                        fallback_romaji.clone(),
                        fallback_english.clone(),
                        fallback_native.clone(),
                        String::new(),
                    )
                }
            } else if force_kitsu_fallback {
                if let Some(kitsu_info) = kitsu_eps.get(&ep_num) {
                    let t = if !kitsu_info.title.trim().is_empty() {
                        kitsu_info.title.clone()
                    } else {
                        fallback_title.clone()
                    };
                    (t.clone(), t.clone(), t.clone(), t, kitsu_info.aired.clone())
                } else {
                    match jikan_eps.get(&ep_num) {
                        Some(info) if !info.title.trim().is_empty() => (
                            info.title.clone(),
                            info.title.clone(),
                            info.title.clone(),
                            info.title.clone(),
                            info.aired.clone(),
                        ),
                        Some(info) => (
                            fallback_title.clone(),
                            fallback_romaji.clone(),
                            fallback_english.clone(),
                            fallback_native.clone(),
                            info.aired.clone(),
                        ),
                        None => (
                            fallback_title,
                            fallback_romaji,
                            fallback_english,
                            fallback_native,
                            String::new(),
                        ),
                    }
                }
            } else {
                match jikan_eps.get(&ep_num) {
                    Some(info) if !info.title.trim().is_empty() => (
                        info.title.clone(),
                        info.title.clone(),
                        info.title.clone(),
                        info.title.clone(),
                        info.aired.clone(),
                    ),
                    Some(info) => (
                        fallback_title.clone(),
                        fallback_romaji.clone(),
                        fallback_english.clone(),
                        fallback_native.clone(),
                        info.aired.clone(),
                    ),
                    None => {
                        // Try Kitsu fallback for episode title/air date.
                        if let Some(kitsu_info) = kitsu_eps.get(&ep_num) {
                            let t = if !kitsu_info.title.trim().is_empty() {
                                kitsu_info.title.clone()
                            } else {
                                fallback_title.clone()
                            };
                            (t.clone(), t.clone(), t.clone(), t, kitsu_info.aired.clone())
                        } else {
                            (
                                fallback_title,
                                fallback_romaji,
                                fallback_english,
                                fallback_native,
                                String::new(),
                            )
                        }
                    }
                }
            };

        let monitored = monitored_lookup.contains(&ep_num);
        if monitored {
            monitored_count += 1;
        }

        // Quality display: disk file quality takes precedence; fall back to grab tag.
        let (display_quality, quality_state) = if !quality.is_empty() {
            (quality.clone(), "disk".to_string())
        } else if let Some(tag) = quality_tags.get(&ep_num) {
            (tag.quality_tag.clone(), tag.state.clone())
        } else {
            (String::new(), String::new())
        };

        let tag = quality_tags.get(&ep_num);
        let class_source = tag.map(|t| t.source.clone()).unwrap_or_default();
        let class_resolution = tag.map(|t| t.resolution.clone()).unwrap_or_default();
        let class_is_remux = tag.map(|t| t.is_remux).unwrap_or(false);
        let class_is_bdmv = tag.map(|t| t.is_bdmv).unwrap_or(false);
        let class_web_kind = tag.map(|t| t.web_kind.clone()).unwrap_or_default();
        let needs_review = tag.map(|t| t.needs_review).unwrap_or(false);
        let manual_override = tag.map(|t| t.manual_override).unwrap_or(false);

        let downloaded = on_disk || quality_state == "completed";
        if downloaded {
            downloaded_count += 1;
        }
        // Unaired-vs-missing split (display only). Date-only compare:
        // an episode whose air date is today counts as aired. Unknown
        // air date defers to the series status — a still-airing or
        // upcoming series usually lacks dates for episodes providers
        // haven't seen yet, while a finished series with missing dates
        // has certainly aired everything. `quality_state.is_empty()`
        // keeps failed grabs red: a failed grab on an unaired episode
        // means someone grabbed a release for it, so "actionable" is
        // the right read.
        let unaired = !downloaded
            && quality_state.is_empty()
            && match chrono::NaiveDate::parse_from_str(ep_aired.get(..10).unwrap_or(""), "%Y-%m-%d")
            {
                Ok(d) => d > today,
                Err(_) => !detail.is_finished(),
            };
        episodes.push(Episode {
            number: ep_num,
            title: ep_title,
            title_romaji: ep_title_romaji,
            title_english: ep_title_english,
            title_native: ep_title_native,
            aired: ep_aired,
            on_disk,
            downloaded,
            quality: display_quality,
            quality_state,
            size_display,
            size_bytes,
            filename,
            can_auto_search: is_tracked,
            monitored,
            unaired,
            class_source,
            class_resolution,
            class_is_remux,
            class_is_bdmv,
            class_web_kind,
            manual_override,
            needs_review,
        });
    }

    // Surface episodes the main 1..=ep_count loop didn't render. Two
    // cases:
    //   1. ep_count == 0 — movies or airing shows with no episodes yet;
    //      the main loop emits no rows, so every disk file lands here.
    //   2. ep_count > 0 but a release partitioned the series into more
    //      files than AniList's reported episode count. Canonical case:
    //      the [smol] Owarimonogatari BD splits the 48-min aired ep 1
    //      back into two ~24-min files, so S1 has 13 files on disk vs
    //      AL's 12 eps. Auto-expand backfills a grab-tag row for the
    //      overflow ep at grab time AND routes the file to the parent
    //      folder at post-process time. Both pre-import ("downloading"
    //      row from the grab tag) and post-import ("imported" row from
    //      the disk file) need to render — without this pass, the main
    //      loop only iterated 1..=ep_count and the overflow was
    //      orphaned in either state. See issue #45.
    let mut rendered_eps: std::collections::HashSet<i32> =
        episodes.iter().map(|e| e.number).collect();

    // Pass 1: on-disk files past ep_count. Takes precedence — a file
    // on disk carries size/filename/quality that a bare grab tag
    // doesn't, and we want the "imported" state to win over any stale
    // "grabbed" tag if the user somehow hits this for both sources.
    for f in &disk_files {
        // Match the main loop's season filter on the ep_count > 0 path:
        // only render season 1 / unseasoned files. Specials/ or S02
        // files under a tracked series folder aren't part of the main
        // episode list. The ep_count == 0 path historically rendered
        // every file regardless of season — preserve that behavior to
        // avoid regressions for movies and airing-with-no-episodes shows.
        if ep_count > 0
            && let Some(s) = f.season_number
            && s != 1
        {
            continue;
        }
        if f.episode_number <= 0 {
            continue;
        }
        if rendered_eps.contains(&f.episode_number) {
            continue;
        }

        on_disk_count += 1;
        downloaded_count += 1;
        total_size += f.size_bytes;
        let monitored = monitored_lookup.contains(&f.episode_number);
        if monitored {
            monitored_count += 1;
        }
        let (display_quality, quality_state) = if !f.quality.is_empty() {
            (f.quality.clone(), "disk".to_string())
        } else if let Some(tag) = quality_tags.get(&f.episode_number) {
            (tag.quality_tag.clone(), tag.state.clone())
        } else {
            (String::new(), String::new())
        };
        let tag = quality_tags.get(&f.episode_number);
        let class_source = tag.map(|t| t.source.clone()).unwrap_or_default();
        let class_resolution = tag.map(|t| t.resolution.clone()).unwrap_or_default();
        let class_is_remux = tag.map(|t| t.is_remux).unwrap_or(false);
        let class_is_bdmv = tag.map(|t| t.is_bdmv).unwrap_or(false);
        let class_web_kind = tag.map(|t| t.web_kind.clone()).unwrap_or_default();
        let needs_review = tag.map(|t| t.needs_review).unwrap_or(false);
        let manual_override = tag.map(|t| t.manual_override).unwrap_or(false);
        rendered_eps.insert(f.episode_number);
        episodes.push(Episode {
            number: f.episode_number,
            title: String::new(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            aired: String::new(),
            on_disk: true,
            // This branch only runs when the file already exists under
            // media_root (on_disk=true), so `downloaded` is
            // unconditionally true regardless of tag state.
            downloaded: true,
            quality: display_quality,
            quality_state,
            size_display: f.size_display.clone(),
            size_bytes: f.size_bytes as i64,
            filename: f.filename.clone(),
            can_auto_search: is_tracked,
            monitored,
            // On disk by definition in this pass, so never unaired.
            unaired: false,
            class_source,
            class_resolution,
            class_is_remux,
            class_is_bdmv,
            class_web_kind,
            manual_override,
            needs_review,
        });
    }

    // Pass 2: grab-tag rows past ep_count with no matching disk file
    // yet. This is what makes the overflow row render as "downloading"
    // immediately after the batch is queued — auto-expand writes the
    // grab tag, the torrent is still downloading so nothing is on disk,
    // and without this pass the row would be invisible until post-
    // processing imports it.
    if ep_count > 0 {
        for (&ep_num, tag) in quality_tags.iter() {
            if ep_num <= ep_count {
                continue;
            }
            if rendered_eps.contains(&ep_num) {
                continue;
            }

            let monitored = monitored_lookup.contains(&ep_num);
            if monitored {
                monitored_count += 1;
            }
            // `downloaded` tracks completed-state episodes; an overflow
            // tag in 'grabbed' state is mid-download so it counts only
            // when the tag has already been flipped to 'completed' by
            // post-processing. Mirrors the main loop's treatment.
            let downloaded = tag.state == "completed";
            if downloaded {
                downloaded_count += 1;
            }
            rendered_eps.insert(ep_num);
            episodes.push(Episode {
                number: ep_num,
                title: String::new(),
                title_romaji: String::new(),
                title_english: String::new(),
                title_native: String::new(),
                aired: String::new(),
                on_disk: false,
                downloaded,
                quality: tag.quality_tag.clone(),
                quality_state: tag.state.clone(),
                size_display: String::new(),
                size_bytes: 0,
                filename: String::new(),
                can_auto_search: is_tracked,
                monitored,
                // Has a grab tag by definition in this pass (grabbed or
                // completed), so the unaired display state never applies.
                unaired: false,
                class_source: tag.source.clone(),
                class_resolution: tag.resolution.clone(),
                class_is_remux: tag.is_remux,
                class_is_bdmv: tag.is_bdmv,
                class_web_kind: tag.web_kind.clone(),
                manual_override: tag.manual_override,
                needs_review: tag.needs_review,
            });
        }
    }

    episodes.sort_by_key(|e| std::cmp::Reverse(e.number));

    let size_display = format_size(total_size);
    (
        episodes,
        on_disk_count,
        downloaded_count,
        size_display,
        monitored_count,
    )
}

fn relation_identity_key(provider_id: i64, mal_id: Option<i64>) -> String {
    if let Some(mal_id) = mal_id {
        format!("mal:{mal_id}")
    } else {
        format!("provider:{provider_id}")
    }
}

/// Resolve the best link ID for a relation card.  If the related entry is
/// tracked in the library (by AniList ID or MAL ID), return the DB series ID
/// so the link always navigates to `/series/<db_id>`.  Otherwise fall back to
/// the provider ID (which may be negative for MAL-sourced entries, but the
/// detail resolver in `resolve_series_context` knows how to handle that).
async fn resolve_relation_card_id(db: &SqlitePool, provider_id: i64, mal_id: Option<i64>) -> i64 {
    // Try AniList ID first (positive IDs).
    if provider_id > 0
        && let Ok(Some(row)) = series::get_by_anilist_id(db, provider_id).await
    {
        return row.id;
    }
    // Try MAL ID.
    if let Some(mid) = mal_id
        && let Ok(Some(row)) = series::get_by_mal_id(db, mid).await
    {
        return row.id;
    }
    // For MAL-sourced entries, the anilist_id column stores -mal_id.
    if provider_id < 0
        && let Ok(Some(row)) = series::get_by_anilist_id(db, provider_id).await
    {
        return row.id;
    }
    provider_id
}

fn relation_richness(rel: &anilist::RelatedEntry) -> i32 {
    let mut score = 0;
    if !rel.cover_url.trim().is_empty() {
        score += 4;
    }
    if !rel.format.trim().is_empty() && rel.format != "TBA" {
        score += 2;
    }
    if !rel.status.trim().is_empty() && rel.status != "TBA" {
        score += 2;
    }
    if rel.episodes.unwrap_or(0) > 0 {
        score += 1;
    }
    if !preferred_title(&rel.title_english, &rel.title_romaji, &rel.title_native)
        .trim()
        .is_empty()
    {
        score += 1;
    }
    score
}

fn merge_relation_metadata(
    primary: &anilist::RelatedEntry,
    fallback: &anilist::RelatedEntry,
) -> anilist::RelatedEntry {
    let mut merged = primary.clone();

    if merged.title_romaji.trim().is_empty() {
        merged.title_romaji = fallback.title_romaji.clone();
    }
    if merged.title_english.trim().is_empty() {
        merged.title_english = fallback.title_english.clone();
    }
    if merged.title_native.trim().is_empty() {
        merged.title_native = fallback.title_native.clone();
    }
    if merged.cover_url.trim().is_empty() {
        merged.cover_url = fallback.cover_url.clone();
    }
    if merged.format.trim().is_empty() || merged.format == "TBA" {
        merged.format = fallback.format.clone();
    }
    if merged.status.trim().is_empty() || merged.status == "TBA" {
        merged.status = fallback.status.clone();
        merged.status_display = fallback.status_display.clone();
    }
    if merged.episodes.is_none() || merged.episodes == Some(0) {
        merged.episodes = fallback.episodes;
    }
    if merged.season_year.is_none() {
        merged.season_year = fallback.season_year;
    }
    if merged.id_mal.is_none() {
        merged.id_mal = fallback.id_mal;
    }
    if merged.media_type.trim().is_empty() {
        merged.media_type = fallback.media_type.clone();
    }

    merged
}

/// Group the detail's relations by type for display as cards.
async fn build_relation_groups(
    db: &SqlitePool,
    db_id: Option<i64>,
    detail: &anilist::AnimeDetail,
) -> Vec<RelationGroup> {
    let cached_relations = if let Some(series_id) = db_id {
        let rows = local_metadata::get_relations_for_series(db, series_id)
            .await
            .unwrap_or_default();
        if rows.is_empty() && detail.id != 0 {
            local_metadata::get_relations_for_provider(db, detail.id)
                .await
                .unwrap_or_default()
        } else {
            rows
        }
    } else if detail.id != 0 {
        local_metadata::get_relations_for_provider(db, detail.id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Treat the current AniList detail payload as the canonical relation graph whenever it is
    // available. Cached relation rows can be stale from older MAL/Jikan hydration passes, which is
    // how the same title ends up rendered twice under two different relation tags.
    let has_authoritative_relations = !detail.relations.is_empty();
    let mut relations = if has_authoritative_relations {
        detail.relations.clone()
    } else {
        cached_relations.clone()
    };

    if has_authoritative_relations {
        let by_identity: HashMap<String, usize> = relations
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
            .map(|(idx, r)| (relation_identity_key(r.id, r.id_mal), idx))
            .collect();

        for cached in cached_relations {
            if !matches!(cached.media_type.as_str(), "ANIME" | "MUSIC") {
                continue;
            }
            let key = relation_identity_key(cached.id, cached.id_mal);
            let Some(idx) = by_identity.get(&key).copied() else {
                continue;
            };
            let merged = merge_relation_metadata(&relations[idx], &cached);
            relations[idx] = merged;
        }
    }

    if !has_authoritative_relations && (detail.id != 0 || detail.id_mal.is_some()) {
        let existing_relation_keys: std::collections::HashSet<String> = relations
            .iter()
            .filter(|r| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
            .map(|r| relation_identity_key(r.id, r.id_mal))
            .collect();
        let incoming =
            local_metadata::get_incoming_relations_for_provider(db, detail.id, detail.id_mal)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r| {
                    !existing_relation_keys.contains(&relation_identity_key(r.id, r.id_mal))
                })
                .collect::<Vec<_>>();
        relations.extend(incoming);
    }

    // Build identity key for the current series so we can filter self-references.
    let self_key = relation_identity_key(detail.id, detail.id_mal);

    let mut deduped: Vec<anilist::RelatedEntry> = Vec::new();
    let mut deduped_index: HashMap<(String, String), usize> = HashMap::new();
    for related in relations {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        // Skip self-references: relations that point back to the current series.
        let related_key = relation_identity_key(related.id, related.id_mal);
        if related_key == self_key {
            continue;
        }
        let normalized_type =
            local_metadata::normalize_relation_type(&related.relation_type).to_string();
        let key = (related_key, normalized_type);
        if let Some(idx) = deduped_index.get(&key).copied() {
            if relation_richness(&deduped[idx]) < relation_richness(&related) {
                deduped[idx] = related;
            }
        } else {
            deduped_index.insert(key, deduped.len());
            deduped.push(related);
        }
    }
    let relations = deduped;

    let type_order = [
        "PREQUEL",
        "SEQUEL",
        "SIDE_STORY",
        "ALTERNATIVE",
        "SUMMARY",
        "FULL_STORY",
        "SPIN_OFF",
        "OTHER",
        "CHARACTER",
        "PARENT",
        "ADAPTATION",
    ];

    // Resolve the per-relation card_id + cover_url concurrently.
    let mut join_set: tokio::task::JoinSet<(usize, i64, String)> = tokio::task::JoinSet::new();
    for (idx, related) in relations.iter().enumerate() {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        let db = db.clone();
        let rel_id = related.id;
        let rel_mal = related.id_mal;
        let rel_cover = related.cover_url.clone();
        join_set.spawn(async move {
            let card_id = resolve_relation_card_id(&db, rel_id, rel_mal).await;
            let cover_url = if let Some(series_id) = db_id {
                artwork::first_cached_url(
                    &db,
                    &[
                        artwork::series_relation_cover_key(series_id, rel_id, rel_mal),
                        format!("series-{}-relation-{}-cover", series_id, rel_id),
                        artwork::provider_cover_key(rel_id, rel_mal),
                        format!("provider-{}-cover", rel_id),
                    ],
                    &rel_cover,
                )
                .await
            } else if rel_id != 0 || rel_mal.is_some() {
                artwork::first_cached_url(
                    &db,
                    &[
                        artwork::provider_cover_key(rel_id, rel_mal),
                        format!("provider-{}-cover", rel_id),
                    ],
                    &rel_cover,
                )
                .await
            } else {
                rel_cover
            };
            (idx, card_id, cover_url)
        });
    }

    let mut resolved: HashMap<usize, (i64, String)> = HashMap::new();
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok((idx, card_id, cover_url)) => {
                resolved.insert(idx, (card_id, cover_url));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "build_relation_groups: relation resolver task failed; skipping one relation card"
                );
            }
        }
    }

    let mut groups: HashMap<String, Vec<RelationCard>> = HashMap::new();

    for (idx, related) in relations.iter().enumerate() {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        let Some((card_id, cover_url)) = resolved.remove(&idx) else {
            continue;
        };

        let normalized_relation_type =
            local_metadata::normalize_relation_type(&related.relation_type).to_string();
        let cards = groups.entry(normalized_relation_type).or_default();

        cards.push(RelationCard {
            id: card_id,
            title: preferred_title(
                &related.title_english,
                &related.title_romaji,
                &related.title_native,
            ),
            title_romaji: related.title_romaji.clone(),
            title_english: related.title_english.clone(),
            title_native: related.title_native.clone(),
            cover_url,
            format: related.format.clone(),
            status: related.status.clone(),
            episodes: related.episodes,
        });
    }

    let mut result: Vec<RelationGroup> = groups
        .into_iter()
        .map(|(rel_type, mut entries)| {
            entries.sort_by(|a, b| {
                let a_title = a.title.to_ascii_lowercase();
                let b_title = b.title.to_ascii_lowercase();
                a_title
                    .cmp(&b_title)
                    .then_with(|| {
                        a.title_romaji
                            .to_ascii_lowercase()
                            .cmp(&b.title_romaji.to_ascii_lowercase())
                    })
                    .then_with(|| a.id.cmp(&b.id))
            });
            let label = format_relation_label(&rel_type);
            RelationGroup {
                relation_type: rel_type,
                label,
                entries,
            }
        })
        .collect();

    result.sort_by_key(|g| {
        type_order
            .iter()
            .position(|t| *t == g.relation_type)
            .unwrap_or(99)
    });
    result
}

fn format_relation_label(rel_type: &str) -> String {
    match rel_type {
        "PREQUEL" => "Prequel".to_string(),
        "SEQUEL" => "Sequel".to_string(),
        "SIDE_STORY" => "Side Story".to_string(),
        "ALTERNATIVE" => "Alternative".to_string(),
        "SUMMARY" => "Summary".to_string(),
        "FULL_STORY" => "Full Story".to_string(),
        "SPIN_OFF" => "Spin Off".to_string(),
        "OTHER" => "Other".to_string(),
        "CHARACTER" => "Character".to_string(),
        "PARENT" => "Parent".to_string(),
        "ADAPTATION" => "Adaptation".to_string(),
        other => other.replace('_', " "),
    }
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    if !value.trim().is_empty() {
        value.to_string()
    } else {
        fallback.to_string()
    }
}

fn preferred_title(english: &str, romaji: &str, native: &str) -> String {
    if !english.is_empty() {
        english.to_string()
    } else if !romaji.is_empty() {
        romaji.to_string()
    } else {
        native.to_string()
    }
}

fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{:.1} GiB", gb)
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests;
