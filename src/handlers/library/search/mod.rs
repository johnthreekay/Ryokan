//! Search/grab/auto-expand handlers for the library section.
//!
//! Split from a single 2596-line `search.rs` into a directory in
//! v1.5: see `auto_search`, `interactive`, `grab` siblings + the
//! `tests/` topic-split. This `mod.rs` retains the two cheap API
//! endpoints (`anilist_search`, `api_series_detail`) and the public
//! re-export surface that `main.rs`'s router declarations call.
//!
//! - `auto_search` — auto-search pipeline (`auto_search_series`,
//!   `auto_search_episode`, the inner `run_auto_search_targets_with_upgrades`
//!   loop) **and** the auto-expand sibling-pack detector that
//!   `auto_search_targets_with_upgrades` calls back into. The two
//!   directions of the call edge are too tightly coupled to live
//!   apart, so they share a file.
//! - `interactive` — user-driven search variants (`search_batch_releases`,
//!   `interactive_search_episode`, `interactive_search_batches`).
//! - `grab` — `grab_batch_result` + `grab_interactive_result`.
//! - `tests` — the auto-expand cumulative-offset / sibling-routing
//!   suite.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
};
use axum_htmx::HxRequest;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::services::{anilist, logger};

use super::AnilistSearchQuery;
use super::reconcile::{force_mal_fallback_enabled, resolve_series_context_by};

mod auto_search;
mod grab;
mod interactive;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod shim_search_queue_tests {
    use super::*;

    #[test]
    fn shim_searches_are_deduped_capped_and_released() {
        // A queue of its own: the shims' command tests queue searches
        // on the global one while this runs.
        static QUEUE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<i64>>> =
            std::sync::LazyLock::new(Default::default);
        let reserve = |id| reserve_shim_search_in(&QUEUE, id);
        let first = reserve(1).expect("first request queues");
        assert!(
            matches!(reserve(1), Err(NotQueued::AlreadyQueued)),
            "already queued"
        );
        drop(first);
        let again = reserve(1).expect("the slot is given back on drop");
        let held: Vec<_> = (100i64..).map_while(|i| reserve(i).ok()).collect();
        assert_eq!(held.len() + 1, SHIM_SEARCH_QUEUE_CAP);
        assert!(matches!(reserve(2), Err(NotQueued::QueueFull)));
        drop(again);
        assert!(reserve(2).is_ok());
    }

    #[tokio::test]
    async fn a_search_for_a_series_not_in_the_library_is_not_queued() {
        // `auto_search_series` reads an unknown internal id as an
        // AniList id, so a stale id from Seerr searched, and grabbed
        // for, whatever show has that number.
        let db = crate::test_support::in_memory_pool().await;
        let state = crate::test_support::build_test_app_state(db.clone(), None);
        assert!(!queue_shim_search(state, 9_200_001, std::time::Duration::ZERO).await);
        assert!(!SHIM_SEARCHES_QUEUED.lock().unwrap().contains(&9_200_001));
        let logged: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM logs WHERE level = 'warn' AND message LIKE '%9200001%'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(logged, 1);
    }
}

pub(crate) use auto_search::series_still_in_library;
pub use auto_search::{
    __path_auto_search_episode, __path_auto_search_series, AutoSearchQuery, auto_search_episode,
    auto_search_series, run_auto_search_targets,
};
pub use grab::{
    __path_grab_batch_result, __path_grab_interactive_result, grab_batch_result,
    grab_interactive_result,
};
pub use interactive::{
    __path_interactive_search_batches, __path_interactive_search_episode,
    __path_interactive_search_episodes, __path_search_batch_releases, interactive_search_batches,
    interactive_search_episode, interactive_search_episodes, search_batch_releases,
};

/// Series with a shim-started search queued or running.
static SHIM_SEARCHES_QUEUED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<i64>>> =
    std::sync::LazyLock::new(Default::default);
/// How many shim-started searches run at once.
static SHIM_SEARCH_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// Most shim searches queued or running; past it a request is dropped.
const SHIM_SEARCH_QUEUE_CAP: usize = 64;

/// A series' place in a shim search queue, given back on drop.
struct ShimSearchSlot {
    queue: &'static std::sync::Mutex<std::collections::HashSet<i64>>,
    series_id: i64,
}

impl Drop for ShimSearchSlot {
    fn drop(&mut self) {
        self.queue.lock().unwrap().remove(&self.series_id);
    }
}

/// Why [`reserve_shim_search_in`] gave no slot.
#[derive(Debug)]
enum NotQueued {
    AlreadyQueued,
    QueueFull,
}

fn reserve_shim_search_in(
    queue: &'static std::sync::Mutex<std::collections::HashSet<i64>>,
    series_id: i64,
) -> Result<ShimSearchSlot, NotQueued> {
    let mut queued = queue.lock().unwrap();
    if queued.contains(&series_id) {
        return Err(NotQueued::AlreadyQueued);
    }
    if queued.len() >= SHIM_SEARCH_QUEUE_CAP {
        return Err(NotQueued::QueueFull);
    }
    queued.insert(series_id);
    Ok(ShimSearchSlot { queue, series_id })
}

/// Start a series' auto-search in the background for the Sonarr /
/// Radarr shims (Seerr's SeriesSearch and MoviesSearch commands, an add
/// with "search on add"). Each call used to spawn its own full search,
/// so a loop of commands queued thousands on the Nyaa semaphore and the
/// AniList budget and starved RSS and the scheduled searches. Now a
/// series already queued or searching is not queued again, at most
/// [`SHIM_SEARCH_QUEUE_CAP`] wait, and two run at a time. Returns
/// whether the search was queued.
///
/// The id must be a series in the library. One Seerr still holds for a
/// series removed since would reach `auto_search_series` as an AniList
/// id, and search (and grab, through the search-before-add path) for
/// whatever show has that number.
pub(crate) async fn queue_shim_search(
    state: AppState,
    series_id: i64,
    delay: std::time::Duration,
) -> bool {
    // A loop of commands could write a warn row per request; one per
    // series (or one for a full queue) a minute is enough to see it.
    let window = std::time::Duration::from_secs(60);
    let missing = match crate::models::series::get_by_id(&state.db, series_id).await {
        Ok(Some(_)) => None,
        Ok(None) => Some(String::new()),
        Err(e) => Some(e.to_string()),
    };
    if let Some(detail) = missing {
        if logger::first_in_window(&format!("shim-search-missing:{series_id}"), window) {
            logger::warn(
                &state.db,
                LogCategory::AutoSearch,
                &format!("Seerr search request skipped: series {series_id} is not in the library"),
                &detail,
            )
            .await;
        }
        return false;
    }
    let slot = match reserve_shim_search_in(&SHIM_SEARCHES_QUEUED, series_id) {
        Ok(slot) => slot,
        Err(NotQueued::AlreadyQueued) => {
            tracing::debug!("shim search for series {series_id} not queued: already queued");
            return false;
        }
        Err(NotQueued::QueueFull) => {
            if logger::first_in_window("shim-search-queue-full", window) {
                logger::warn(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!(
                        "Seerr search request for series {series_id} dropped: {SHIM_SEARCH_QUEUE_CAP} searches are already waiting"
                    ),
                    "",
                )
                .await;
            }
            return false;
        }
    };
    tokio::spawn(async move {
        let _slot = slot;
        tokio::time::sleep(delay).await;
        let Ok(_permit) = SHIM_SEARCH_PERMITS.acquire().await else {
            return;
        };
        let _ = auto_search_series(
            State(state),
            Path(series_id),
            Query(AutoSearchQuery::default()),
        )
        .await;
    });
    true
}

/// Pre-computed display fields for one search result row. Built by
/// `build_search_results_partial` from a raw `AnimeEntry` so the Askama
/// template stays simple — title-language picking, status-class
/// flattening, external-link selection, and the JSON-in-attribute
/// `data-entry` payload that `addSeries(...)` reads all happen in Rust.
struct SearchResultRow {
    entry: anilist::AnimeEntry,
    title: String,
    subtitle: String,
    format_display: String,
    episodes_display: String,
    status_class: String,
    status_label: String,
    external_href: Option<String>,
    source_label: &'static str,
    /// Pre-serialized JSON of `entry`. Embedded into the `data-entry`
    /// attribute on the Add button; `static/js/index.js::addSeries`
    /// reads it back via `JSON.parse(btn.dataset.entry)` to seed the
    /// monitor-mode modal without re-querying.
    data_entry_json: String,
}

#[derive(Template)]
#[template(path = "partials/library/anilist_search_results.html")]
struct AnilistSearchResultsPartial {
    entries: Vec<SearchResultRow>,
}

/// Pick the user-facing title for a result given a language hint. Mirrors
/// `getTitleByLang` in `static/js/index.js`: native and romaji fall back
/// through the same chain JS used; anything unknown coerces to english.
fn pick_title<'a>(entry: &'a anilist::AnimeEntry, lang: &str) -> &'a str {
    let pick_first_non_empty = |a: &'a str, b: &'a str, c: &'a str| -> &'a str {
        if !a.is_empty() {
            a
        } else if !b.is_empty() {
            b
        } else {
            c
        }
    };
    match lang {
        "native" => pick_first_non_empty(
            &entry.title_native,
            &entry.title_romaji,
            &entry.title_english,
        ),
        "romaji" => pick_first_non_empty(
            &entry.title_romaji,
            &entry.title_english,
            &entry.title_native,
        ),
        _ => pick_first_non_empty(
            &entry.title_english,
            &entry.title_romaji,
            &entry.title_native,
        ),
    }
}

fn build_search_results_partial(
    entries: Vec<anilist::AnimeEntry>,
    lang: &str,
) -> AnilistSearchResultsPartial {
    let rows = entries
        .into_iter()
        .map(|entry| {
            let title = pick_title(&entry, lang).to_string();
            let subtitle = if lang == "english" {
                if !entry.title_romaji.is_empty() {
                    entry.title_romaji.clone()
                } else {
                    entry.title_native.clone()
                }
            } else if !entry.title_english.is_empty() {
                entry.title_english.clone()
            } else if !entry.title_romaji.is_empty() {
                entry.title_romaji.clone()
            } else {
                entry.title_native.clone()
            };
            let format_display = if entry.format.is_empty() {
                "TBA".to_string()
            } else {
                entry.format.replace('_', " ")
            };
            let episodes_display = match entry.episodes {
                Some(n) => format!("{n} eps"),
                None => "?".to_string(),
            };
            let status_class = entry.status.to_lowercase();
            let status_label = if !entry.status_display.is_empty() {
                entry.status_display.replace('_', " ")
            } else {
                entry.status.replace('_', " ")
            };
            let is_mal = entry.source == "mal";
            let external_href = if is_mal {
                entry
                    .id_mal
                    .map(|id| format!("https://myanimelist.net/anime/{id}"))
            } else {
                Some(format!("https://anilist.co/anime/{}", entry.id))
            };
            let source_label = if is_mal { "MAL" } else { "AniList" };
            // Pre-serialize so the template can inline it as the
            // `data-entry` attribute. Askama's auto-escape turns `"`
            // into `&quot;`, which the browser parses back to a literal
            // `"` inside the attribute — no manual escAttr needed.
            let data_entry_json = serde_json::to_string(&entry).unwrap_or_else(|_| "{}".into());
            SearchResultRow {
                entry,
                title,
                subtitle,
                format_display,
                episodes_display,
                status_class,
                status_label,
                external_href,
                source_label,
                data_entry_json,
            }
        })
        .collect();
    AnilistSearchResultsPartial { entries: rows }
}

#[utoipa::path(
    get,
    path = "/api/anilist/search",
    tag = "Library",
    summary = "Search AniList for anime",
    description = "Search for anime by title. Uses AniList as primary source with MAL/Jikan and Kitsu as fallbacks.",
    params(AnilistSearchQuery),
    responses(
        (status = 200, description = "Search results (JSON for plain callers, rendered HTML partial when called via HX-Request)", body = Vec<anilist::AnimeEntry>),
        (status = 500, description = "Search failed"),
    ),
)]
pub async fn anilist_search(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Query(params): Query<AnilistSearchQuery>,
) -> Result<Response, (StatusCode, String)> {
    // Per-search override (?source=al|mal) takes precedence over the
    // ambient config flag. Only `al`, `mal`, or omitted are valid —
    // surface a 400 on anything else so a client with a typo in its
    // query param doesn't silently drop into the config default and
    // look like the toggle is broken.
    let force_fallback = match params.source.as_deref() {
        Some("mal") => true,
        Some("al") => false,
        None => force_mal_fallback_enabled(&state.db).await,
        Some("") => force_mal_fallback_enabled(&state.db).await,
        Some(other) => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                format!(
                    "invalid source override: {:?} (expected \"al\", \"mal\", or omit)",
                    other
                ),
            ));
        }
    };
    let results = anilist::search_anime_with_options(&params.q, force_fallback)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let source = if results.iter().any(|r| r.source == "mal") {
        "MAL/Jikan fallback"
    } else {
        "AniList"
    };
    logger::info(
        &state.db,
        LogCategory::AniList,
        &format!("Title search: '{}'", params.q),
        &format!(
            "results={}, source={}, forced_fallback={}, requested={}",
            results.len(),
            source,
            force_fallback,
            params.source.as_deref().unwrap_or("(config default)"),
        ),
    )
    .await;

    if is_htmx {
        let lang = params.lang.as_deref().unwrap_or("english");
        let partial = build_search_results_partial(results, lang);
        let html = partial
            .render()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        Ok(Html(html).into_response())
    } else {
        Ok(Json(results).into_response())
    }
}

#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}",
    tag = "Library",
    summary = "Get series detail",
    description = "Returns full metadata for a series by its AniList ID or internal database ID.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Series detail", body = anilist::AnimeDetail),
        (status = 500, description = "Failed to fetch detail"),
    ),
)]
pub async fn api_series_detail(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Query(kind): Query<super::SeriesIdKind>,
) -> Result<Json<anilist::AnimeDetail>, (axum::http::StatusCode, String)> {
    let (_, _, detail) = resolve_series_context_by(&state.db, request_id, kind.by_anilist())
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(detail))
}
