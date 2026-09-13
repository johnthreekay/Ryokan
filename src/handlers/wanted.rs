//! The Wanted page: Sonarr's Wanted → Missing / Cutoff Unmet across
//! the whole library, with "search selected" / "search all".
//!
//! **Missing** is every monitored episode of a monitored series that
//! has aired (the same aired bound the library cards use), is not on
//! disk or recorded as completed, and is not downloading. **Cutoff
//! unmet** is every file on disk below the quality cutoff for a series
//! that allows upgrades, the exact list the daily upgrade sweep would
//! target (`auto_search::build_upgrade_targets`). Both are computed on
//! request from the disk scan and the tag table; nothing is cached.
//!
//! The search runs the series' own auto-search (missing episodes plus
//! upgrade targets, batch probing included) one series at a time in a
//! detached task under `WANTED_SEARCH_LOCK`, reporting through the
//! sticky progress toast. Sonarr groups the same way: a season search
//! when several episodes of one season are missing, an episode search
//! otherwise; here the per-series auto-search makes that choice.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use askama::Template;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use axum_htmx::{HxBoosted, HxRequest};
use serde::Deserialize;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, local_metadata, monitoring, series};
use crate::services::source::{self, Resolution};
use crate::services::{auto_search, logger, media, naming, progress};

/// Most series ids one search request may carry; a library is the
/// natural ceiling and the lookups run one per id.
const MAX_SEARCH_IDS: usize = 5_000;

/// One search at a time: a second "search all" while one runs is
/// refused with 409 rather than queued behind it.
pub static WANTED_SEARCH_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// One episode slot on the page.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WantedSlot {
    pub episode: i32,
    /// `E05`.
    pub label: String,
    /// The file's quality for a cutoff-unmet slot, empty for missing.
    pub quality: String,
}

/// One series on the page with the slots it wants.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WantedRow {
    pub series_id: i64,
    pub anilist_id: i64,
    pub title: String,
    pub cover_url: String,
    pub slots: Vec<WantedSlot>,
}

impl WantedRow {
    pub fn count(&self) -> usize {
        self.slots.len()
    }
}

/// Missing rows from the library-wide inputs. Pure, so the rule is
/// testable without a database: a series is included when its monitor
/// mode is not `none`; an episode when it is monitored, has aired
/// (`aired` count, else the series' episode total, none for a series
/// not yet released), is not on disk (season 1 or unseasoned files
/// only, the library card rule), is not a completed tag row, and is
/// not a `grabbed` tag row (downloading). Titles follow
/// `title_language` like every other page.
///
/// `aired` is a count of aired episode rows, read as the highest aired
/// number: the two agree for the contiguous 1..N numbering AniList
/// hands out, which is what the library cards assume too.
pub fn build_missing_rows(
    library: &[series::Series],
    disk: &HashMap<i64, Vec<media::EpisodeFile>>,
    tag_rows: &[(i64, i32, String)],
    aired: &HashMap<i64, i64>,
    monitored: &HashMap<i64, HashSet<i32>>,
    title_language: &str,
) -> Vec<WantedRow> {
    let mut completed: HashMap<i64, HashSet<i32>> = HashMap::new();
    let mut grabbed: HashMap<i64, HashSet<i32>> = HashMap::new();
    for (sid, ep, state) in tag_rows {
        if state == "grabbed" {
            grabbed.entry(*sid).or_default().insert(*ep);
        } else {
            completed.entry(*sid).or_default().insert(*ep);
        }
    }
    let mut rows = Vec::new();
    for s in library {
        if s.monitor_mode == "none" {
            continue;
        }
        // No monitor rows at all means monitoring never computed for
        // the series (no episode count yet); nothing to want.
        let Some(monitored_eps) = monitored.get(&s.id) else {
            continue;
        };
        let total = i64::from(s.episodes.unwrap_or(0));
        let bound = match aired.get(&s.id) {
            Some(&n) if n > 0 => n,
            _ if s.status == "NOT_YET_RELEASED" => 0,
            _ => total,
        };
        if bound <= 0 {
            continue;
        }
        let mut have: HashSet<i32> = HashSet::new();
        if let Some(files) = disk.get(&s.id) {
            for f in files {
                if f.episode_number <= 0 {
                    continue;
                }
                if total > 0 && matches!(f.season_number, Some(n) if n != 1) {
                    continue;
                }
                have.extend(f.episodes());
            }
        }
        if let Some(eps) = completed.get(&s.id) {
            have.extend(eps);
        }
        let downloading = grabbed.get(&s.id);
        let mut wanted: Vec<i32> = monitored_eps
            .iter()
            .copied()
            .filter(|ep| *ep >= 1 && i64::from(*ep) <= bound)
            .filter(|ep| !have.contains(ep))
            .filter(|ep| !downloading.is_some_and(|d| d.contains(ep)))
            .collect();
        if wanted.is_empty() {
            continue;
        }
        wanted.sort_unstable();
        rows.push(WantedRow {
            series_id: s.id,
            anilist_id: s.anilist_id,
            title: naming::SeriesNames::from_series(s).preferred_title(title_language),
            cover_url: s.cover_url.clone(),
            slots: wanted
                .into_iter()
                .map(|ep| WantedSlot {
                    episode: ep,
                    label: format!("E{ep:02}"),
                    quality: String::new(),
                })
                .collect(),
        });
    }
    rows.sort_by_key(|r| r.title.to_lowercase());
    rows
}

/// Cutoff-unmet rows: the upgrade sweep's targets, per series that
/// allows upgrades, with the file's current quality on each slot.
async fn build_cutoff_rows(
    state: &AppState,
    cfg: &config::Config,
    library: &[series::Series],
    disk: &HashMap<i64, Vec<media::EpisodeFile>>,
) -> Vec<WantedRow> {
    let (cutoff_source, cutoff_is_remux, cutoff_is_bdmv) =
        source::parse_cutoff_source(&cfg.cutoff_source);
    let cutoff_resolution = Resolution::from_str(&cfg.cutoff_resolution);
    let mut rows = Vec::new();
    for s in library {
        if !s.allow_upgrades {
            continue;
        }
        let Some(files) = disk.get(&s.id).filter(|f| !f.is_empty()) else {
            continue;
        };
        let tags = episode_tags::get_for_series(&state.db, s.id)
            .await
            .unwrap_or_default();
        let on_disk: Vec<i32> = files.iter().flat_map(|f| f.episodes()).collect();
        let targets = auto_search::build_upgrade_targets(
            files,
            &on_disk,
            cutoff_source,
            cutoff_resolution,
            cutoff_is_remux,
            cutoff_is_bdmv,
            &tags,
        );
        let slots: Vec<WantedSlot> = targets
            .into_iter()
            .filter_map(|(t, existing)| match t {
                auto_search::SearchTarget::Episode(ep) => Some(WantedSlot {
                    episode: ep,
                    label: format!("E{ep:02}"),
                    quality: existing.label(),
                }),
                auto_search::SearchTarget::Single => None,
            })
            .collect();
        if slots.is_empty() {
            continue;
        }
        rows.push(WantedRow {
            series_id: s.id,
            anilist_id: s.anilist_id,
            title: naming::SeriesNames::from_series(s).preferred_title(&cfg.title_language),
            cover_url: s.cover_url.clone(),
            slots,
        });
    }
    rows.sort_by_key(|r| r.title.to_lowercase());
    rows
}

#[derive(Deserialize, Default)]
pub struct WantedQuery {
    pub tab: Option<String>,
}

#[derive(Template)]
#[template(path = "wanted.html")]
struct WantedPageTemplate {
    page: String,
    title_language: String,
    tab: String,
    rows: Vec<WantedRow>,
    library_is_empty: bool,
}

#[derive(Template)]
#[template(path = "partials/wanted/list.html")]
struct WantedListPartial {
    tab: String,
    rows: Vec<WantedRow>,
    library_is_empty: bool,
}

/// `GET /wanted?tab=missing|cutoff`. An HTMX (non-boost) request gets
/// the list partial for the tab swap; anything else the full page.
pub async fn page(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    HxBoosted(is_boosted): HxBoosted,
    Query(q): Query<WantedQuery>,
) -> Html<String> {
    let tab = match q.tab.as_deref() {
        Some("cutoff") => "cutoff".to_string(),
        _ => "missing".to_string(),
    };
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let library = series::get_all(&state.db).await.unwrap_or_default();
    let library_is_empty = library.is_empty();
    let folders: Vec<(i64, String)> = library
        .iter()
        .filter(|s| !s.folder_name.is_empty())
        .map(|s| (s.id, s.folder_name.clone()))
        .collect();
    let disk = media::scan_series_folders_batch(&cfg.media_root, folders).await;
    let rows = if tab == "cutoff" {
        build_cutoff_rows(&state, &cfg, &library, &disk).await
    } else {
        let (aired, tag_rows, monitored) = tokio::join!(
            local_metadata::aired_episode_counts(&state.db),
            episode_tags::active_states_all_series(&state.db),
            monitoring::get_monitored_all_series(&state.db),
        );
        build_missing_rows(
            &library,
            &disk,
            &tag_rows.unwrap_or_default(),
            &aired.unwrap_or_default(),
            &monitored.unwrap_or_default(),
            &cfg.title_language,
        )
    };
    if is_htmx && !is_boosted {
        let partial = WantedListPartial {
            tab,
            rows,
            library_is_empty,
        };
        return Html(partial.render().unwrap_or_default());
    }
    let tmpl = WantedPageTemplate {
        page: "wanted".to_string(),
        title_language: cfg.title_language.clone(),
        tab,
        rows,
        library_is_empty,
    };
    Html(tmpl.render().unwrap_or_default())
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct WantedSearchRequest {
    /// Library series ids (`series.id`), searched in the order given.
    pub series_ids: Vec<i64>,
    /// `missing` (default) or `cutoff`. The cutoff tab's search builds
    /// its upgrade targets from every file on disk, not only the
    /// monitored episodes, so it can reach what the tab lists.
    #[serde(default)]
    pub tab: Option<String>,
}

/// `POST /api/wanted/search?progress_id=<id>`: run each series' own
/// auto-search (missing monitored episodes plus upgrade targets), one
/// series at a time, in a detached task. Returns as soon as the task
/// is queued; progress arrives on the sticky toast.
#[utoipa::path(
    post,
    path = "/api/wanted/search",
    tag = "Library",
    summary = "Search for the wanted episodes of the given series",
    description = "Runs each series' auto-search one after another in the background. Pass ?progress_id= to follow it on /api/progress/{id}. 409 when a wanted search is already running.",
    request_body = WantedSearchRequest,
    responses(
        (status = 202, description = "Search queued", body = serde_json::Value),
        (status = 409, description = "A wanted search is already running", body = serde_json::Value),
    ),
)]
pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<crate::handlers::library::search::AutoSearchQuery>,
    Json(req): Json<WantedSearchRequest>,
) -> Response {
    if req.series_ids.len() > MAX_SEARCH_IDS {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "message": format!("At most {MAX_SEARCH_IDS} series per search.")
            })),
        )
            .into_response();
    }
    // The lock first, so a refused request registers no progress job;
    // then the job, before any lookup, so the toast's stream finds it
    // (the page opens the stream as it sends the request).
    let Ok(guard) = WANTED_SEARCH_LOCK.try_lock() else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "message": "A wanted search is already running. Wait for it to finish."
            })),
        )
            .into_response();
    };
    let handle = match progress::sanitize_progress_id(q.progress_id.as_deref()) {
        Some(id) => Some(state.progress.register(id).await),
        None => None,
    };
    let include_disk_upgrades = req.tab.as_deref() == Some("cutoff");
    let mut seen: HashSet<i64> = HashSet::new();
    let mut targets: Vec<(i64, String)> = Vec::new();
    for id in req.series_ids {
        if !seen.insert(id) {
            continue;
        }
        if let Ok(Some(row)) = series::get_by_id(&state.db, id).await {
            targets.push((row.anilist_id, row.title));
        }
    }
    if targets.is_empty() {
        if let Some(h) = &handle {
            h.emit("done", "error", "No series to search", None, true)
                .await;
        }
        drop(guard);
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "message": "No series to search."})),
        )
            .into_response();
    }
    let queued = targets.len();
    let state_for_task = state.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let state = state_for_task;
        let total = targets.len();
        let handle_for_loop = handle.clone();
        let state_for_loop = state.clone();
        // The loop runs in its own task so a panic inside one series'
        // search still ends with a terminal event: a toast with no
        // end would spin forever and its job would never be swept.
        let loop_task = tokio::spawn(async move {
            let state = state_for_loop;
            let handle = handle_for_loop;
            let mut grabbed = 0usize;
            let mut errors = 0usize;
            for (i, (anilist_id, title)) in targets.into_iter().enumerate() {
                if let Some(h) = &handle {
                    h.emit(
                        "search",
                        "info",
                        format!("Searching {title}"),
                        Some(format!("{} of {total}", i + 1)),
                        false,
                    )
                    .await;
                }
                // Not wrapped in `progress::scope` on purpose: the
                // series search's own `progress::emit` calls (its
                // terminal "done" included) are no-ops here, so only
                // this task writes to the toast.
                let result = crate::handlers::library::search::auto_search_series(
                    State(state.clone()),
                    Path(anilist_id),
                    Query(crate::handlers::library::search::AutoSearchQuery {
                        progress_id: None,
                        include_disk_upgrades,
                    }),
                )
                .await;
                match result {
                    Ok(report) => grabbed += report.0.grabbed.len(),
                    Err((_, e)) => {
                        errors += 1;
                        logger::warn(
                            &state.db,
                            LogCategory::AutoSearch,
                            &format!("Wanted search: '{title}' failed"),
                            &e,
                        )
                        .await;
                    }
                }
            }
            (grabbed, errors)
        });
        let (summary, kind) = match loop_task.await {
            Ok((grabbed, errors)) => {
                let summary = format!(
                    "Searched {total} series, grabbed {grabbed} release{}{}",
                    if grabbed == 1 { "" } else { "s" },
                    if errors > 0 {
                        format!(", {errors} failed")
                    } else {
                        String::new()
                    }
                );
                let kind = if errors == total {
                    "error"
                } else if errors == 0 {
                    "success"
                } else {
                    "warn"
                };
                (summary, kind)
            }
            Err(join_error) => (
                format!("Wanted search stopped early: {join_error}"),
                "error",
            ),
        };
        if kind == "error" {
            logger::error(&state.db, LogCategory::AutoSearch, &summary, "").await;
        } else {
            logger::info(&state.db, LogCategory::AutoSearch, &summary, "").await;
        }
        if let Some(h) = &handle {
            h.emit("done", kind, summary, None, true).await;
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"ok": true, "queued": queued})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(id: i64, monitor_mode: &str, episodes: Option<i32>, status: &str) -> series::Series {
        series::Series {
            is_adult: false,
            id,
            anilist_id: id * 10,
            mal_id: None,
            title: format!("Show {id}"),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: status.to_string(),
            episodes,
            season_year: None,
            end_year: None,
            folder_name: format!("Show {id}"),
            monitor_mode: monitor_mode.to_string(),
            allow_upgrades: true,
            allow_pt_upgrades: false,
            custom_query_tokens: String::new(),
            restrict_to_uploader: String::new(),
            alternate_titles: String::new(),
            cumulative_prior_episodes: 0,
            monitor_mode_manual_override: false,
            user_score: None,
            added_at: String::new(),
        }
    }

    fn file(ep: i32, last: i32, season: Option<i32>) -> media::EpisodeFile {
        media::EpisodeFile {
            filename: format!("Show - S01E{ep:02}.mkv"),
            episode_number: ep,
            episode_last: last,
            season_number: season,
            quality: String::new(),
            size_bytes: 1,
            size_display: String::new(),
            modified_secs: None,
            is_special: false,
        }
    }

    #[test]
    fn missing_rows_follow_the_library_card_rule() {
        let library = vec![
            show(1, "all", Some(12), "RELEASING"),
            show(2, "none", Some(12), "FINISHED"),
            show(3, "all", Some(12), "NOT_YET_RELEASED"),
            show(4, "future", None, "RELEASING"),
        ];
        let mut disk = HashMap::new();
        // Series 1: E01-E02 on disk (E02 half of a range), E03 as a
        // season-2 file that does not count.
        disk.insert(1, vec![file(1, 2, Some(1)), file(3, 3, Some(2))]);
        // Series 4: no files.
        let tag_rows = vec![
            (1, 4, "completed".to_string()),
            (1, 5, "grabbed".to_string()),
            (4, 1, "grabbed".to_string()),
        ];
        let mut aired = HashMap::new();
        aired.insert(1, 7); // 7 of 12 have aired
        aired.insert(4, 3);
        let mut monitored = HashMap::new();
        monitored.insert(1, (1..=12).collect::<HashSet<i32>>());
        monitored.insert(2, (1..=12).collect::<HashSet<i32>>());
        monitored.insert(3, (1..=12).collect::<HashSet<i32>>());
        monitored.insert(4, [2, 3, 9].into_iter().collect::<HashSet<i32>>());
        let rows = build_missing_rows(&library, &disk, &tag_rows, &aired, &monitored, "romaji");
        let by_id: HashMap<i64, Vec<i32>> = rows
            .iter()
            .map(|r| (r.series_id, r.slots.iter().map(|s| s.episode).collect()))
            .collect();
        // 1-2 on disk, 4 completed, 5 downloading, 8+ not aired: 3, 6, 7.
        assert_eq!(by_id.get(&1), Some(&vec![3, 6, 7]));
        assert!(!by_id.contains_key(&2), "monitor mode none");
        assert!(!by_id.contains_key(&3), "not yet released");
        // Aired bound 3 with no episode total: 2 and 3 wanted, 9 not
        // aired, 1 not monitored.
        assert_eq!(by_id.get(&4), Some(&vec![2, 3]));
        assert_eq!(rows[0].slots[0].label, "E03");
        assert_eq!(rows[0].count(), 3);
    }

    #[tokio::test]
    async fn page_renders_both_tabs_and_the_partial() {
        use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        // Empty library: the empty state names the library.
        let html = page(
            State(state.clone()),
            HxRequest(false),
            HxBoosted(false),
            Query(WantedQuery::default()),
        )
        .await
        .0;
        assert!(html.contains("id=\"wanted-page\""), "full page rendered");
        assert!(html.contains("any series in your library yet"), "{html}");
        // A monitored series with no files: every aired episode is
        // missing on the Missing tab, nothing on the cutoff tab.
        let id = seed_series(&db, 1, "Show").await;
        sqlx::query("UPDATE series SET monitor_mode = 'all', episodes = 3 WHERE id = ?")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        for ep in 1..=3 {
            sqlx::query(
                "INSERT INTO episode_monitor_state (series_id, episode_number, monitored) VALUES (?, ?, 1)",
            )
            .bind(id)
            .bind(ep)
            .execute(&db)
            .await
            .unwrap();
        }
        let partial = page(
            State(state.clone()),
            HxRequest(true),
            HxBoosted(false),
            Query(WantedQuery {
                tab: Some("missing".to_string()),
            }),
        )
        .await
        .0;
        assert!(!partial.contains("id=\"wanted-page\""), "partial only");
        assert!(
            partial.contains("E01") && partial.contains("E03"),
            "{partial}"
        );
        assert!(partial.contains("3 missing"), "{partial}");
        let cutoff = page(
            State(state),
            HxRequest(true),
            HxBoosted(false),
            Query(WantedQuery {
                tab: Some("cutoff".to_string()),
            }),
        )
        .await
        .0;
        assert!(cutoff.contains("Nothing is below the cutoff"), "{cutoff}");
    }

    #[tokio::test]
    async fn search_with_no_known_series_is_a_bad_request() {
        use crate::test_support::{build_test_app_state, in_memory_pool};
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = search(
            State(state),
            Query(crate::handlers::library::search::AutoSearchQuery::default()),
            Json(WantedSearchRequest {
                series_ids: vec![999],
                tab: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_is_refused_while_one_is_running() {
        use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
        let db = in_memory_pool().await;
        let id = seed_series(&db, 1, "Show").await;
        let state = build_test_app_state(db, None);
        let held = WANTED_SEARCH_LOCK.lock().await;
        let resp = search(
            State(state),
            Query(crate::handlers::library::search::AutoSearchQuery::default()),
            Json(WantedSearchRequest {
                series_ids: vec![id],
                tab: Some("cutoff".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        drop(held);
    }

    #[test]
    fn missing_rows_sort_by_title() {
        let mut b = show(1, "all", Some(2), "FINISHED");
        b.title = "beta".to_string();
        let mut a = show(2, "all", Some(2), "FINISHED");
        a.title = "Alpha".to_string();
        let monitored: HashMap<i64, HashSet<i32>> =
            [(1, [1].into()), (2, [1].into())].into_iter().collect();
        let rows = build_missing_rows(
            &[b, a],
            &HashMap::new(),
            &[],
            &HashMap::new(),
            &monitored,
            "romaji",
        );
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["Alpha", "beta"]);
    }
}
