use askama::Template;
use axum::{
    Form,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::AppState;
use crate::handlers::library::reconcile::populate_series_cover_urls;
use crate::models::{
    config, episode_tags,
    log::{self, LogCategory, LogLevel},
    rss, scheduled_tasks,
};
use crate::services::{
    airing_refresh, logger, metadata_sync, post_processing, rss as rss_service, upgrade,
};

/// Wrap a handler body in a detached `tokio::spawn` so the work runs
/// to completion even when the client navigates away mid-flight.
///
/// Without this, dropping the request future cancels the body's
/// `.await` chain in place and the `scheduled_task_runs` row stays at
/// `last_status = 'running'` until the next process restart — a click-
/// then-walk-away on Run-now / Sync now / etc. used to corrupt the
/// scheduled-tasks audit trail. Tokio's `tokio::spawn` decouples the
/// task lifetime from the JoinHandle lifetime: dropping the handle
/// (when the request future is dropped) leaves the spawned task
/// running on the runtime, so `mark_finished` always fires.
///
/// One-layer (vs. the three-layer pattern in
/// `api_rebuild_cached_metadata`): a body panic surfaces as
/// `Err(JoinError::Panicked)` and `mark_finished` is missed, leaving
/// the row stuck at running until the next process restart's
/// `scheduled_tasks::recover_stuck_running` boot pass cleans it up.
/// Acceptable trade-off for the periodic-refresh / cleanup /
/// library-classify / etc. family because none of those wrap
/// user-input parsing or speculative I/O with a realistic panic
/// site. Use the three-layer shape in `api_rebuild_cached_metadata`
/// if a new handler does.
async fn detached_task<F, T>(future: F) -> Result<T, (StatusCode, String)>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::spawn(future).await {
        Ok(t) => Ok(t),
        Err(e) => {
            // Log loudly on the panic path so the operator has a
            // breadcrumb in the process logs beyond the eventual
            // `recover_stuck_running` line at next boot. Without
            // this, a panic in a spawn body is silent until the
            // next restart's recovery pass — the row stays at
            // 'running' for the rest of the current process
            // lifetime and System → Scheduled Tasks shows no
            // failure.
            if e.is_panic() {
                tracing::error!(
                    target: "ryokan::system",
                    "detached scheduled task panicked: {} \
                     (row will stay 'running' until the next process restart, \
                     where boot-time recover_stuck_running flips it to 'error')",
                    e
                );
            }
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("scheduled task failed to join: {}", e),
            ))
        }
    }
}

#[derive(Template)]
#[template(path = "system.html")]
struct SystemTemplate {
    page: String,
    tab: String,
    force_mal_fallback: bool,
    force_kitsu_fallback: bool,
    debug_message: Option<String>,
    debug_error: Option<String>,
    logs: Vec<log::LogEntry>,
    log_count: i64,
    filter_level: String,
    filter_category: String,
    filter_search: String,
    /// Current page's cursor (the `before_id` query param value, or
    /// `None` for the first/newest page). Used in the template to
    /// render the "Newest" reset link conditionally.
    log_before_id: Option<i64>,
    /// Cursor for the "Older →" link, set to the `id` of the oldest
    /// entry on the current page. `None` when the page is the last
    /// (or when there are no entries at all).
    log_older_id: Option<i64>,
    /// Mirrors `log_before_id` for the RSS tab — the active cursor
    /// the user navigated to (drives the "← Newest" link).
    rss_before_id: Option<i64>,
    /// Mirrors `log_older_id` for the RSS tab — the next-page cursor
    /// when there's more history beyond the current page.
    rss_older_id: Option<i64>,
    categories: Vec<(&'static str, &'static str)>,
    rss_enabled: bool,
    rss_interval_minutes: i32,
    rss_last_run: Option<rss::RssRun>,
    rss_recent: Vec<rss::RssDecision>,
    scheduled_tasks: Vec<scheduled_tasks::ScheduledTaskStatus>,
    /// Cross-library episodes currently flagged `needs_review`. Only
    /// populated when `tab == "review"`; empty on every other tab so
    /// the serial fan-out stays cheap.
    review_entries: Vec<episode_tags::NeedsReviewEntry>,
    /// Misgrab guardrails: detected misgrabs awaiting Restore or Dismiss;
    /// populated only when `tab == "misgrabs"`.
    misgrab_entries: Vec<crate::models::grabbed_torrents::MisgrabEntry>,
    title_language: String,
    /// Issue gh-121 — notification provider rows for the System →
    /// Notifications tab. Empty until the user adds the first one.
    /// Only populated when `tab == "notifications"`. Each row is
    /// pre-projected so the template doesn't need to re-deserialize
    /// `config_json`.
    notification_providers: Vec<notifications::ProviderView>,
    /// Per-event matrix view for the inline edit form. When the page
    /// renders without `?edit_id=`, this is the seed-default-on view
    /// for a fresh create form. With `?edit_id=N`, this is the loaded
    /// matrix for that provider.
    notification_event_toggles: Vec<notifications::EventToggleView>,
    /// Recycle bin (#123) refused a delete because the bin isn't writable
    /// (`recycle::RECYCLE_UNWRITABLE`); renders the top-of-page banner.
    /// Flag only, no probe: this page shouldn't wake a spun-down disk.
    recycle_unwritable: bool,
    /// System → Backup (#126). Only populated when `tab == "backup"`.
    backup: Option<backup::BackupTabView>,
}

#[derive(Deserialize)]
pub struct SystemQuery {
    tab: Option<String>,
    level: Option<String>,
    category: Option<String>,
    search: Option<String>,
    message: Option<String>,
    error: Option<String>,
    /// Cursor for "Older →" pagination on the logs tab. When set,
    /// the query fetches entries with `id < before_id`. Omitted on
    /// the first page so the user always lands on the newest
    /// entries.
    before_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct DebugSettingsForm {
    force_mal_fallback: Option<String>,
    force_kitsu_fallback: Option<String>,
}

fn normalize_system_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("scoring") => "scoring".to_string(),
        Some("help") => "scoring".to_string(), // legacy alias
        Some("debug") => "debug".to_string(),
        Some("rss") => "rss".to_string(),
        Some("tasks") => "tasks".to_string(),
        Some("review") => "review".to_string(),
        Some("misgrabs") => "misgrabs".to_string(),
        Some("credits") => "credits".to_string(),
        Some("notifications") => "notifications".to_string(),
        Some("backup") => "backup".to_string(),
        _ => "logs".to_string(),
    }
}

#[cfg(test)]
pub(crate) fn normalize_system_tab_for_test(tab: Option<String>) -> String {
    normalize_system_tab(tab)
}

/// Apply the `+1-fetch` cursor pagination contract: the model fetched
/// `page_size + 1` rows, this helper truncates the extra one and
/// returns its now-last entry's id as the cursor for the next page.
/// `None` means "no older page" — either the dataset was smaller
/// than `page_size + 1`, or the model returned exactly `page_size`
/// (no extra row, no next page).
///
/// The strict `> page_size` (not `>=`) is the load-bearing
/// invariant: a `>=` here would return a non-empty cursor on the
/// last page, the user would click "Older" and see an empty page.
/// Pinned by `truncate_to_page_returns_none_at_exact_page_size`
/// and `truncate_to_page_returns_some_when_extra_row_present`.
fn truncate_to_page<T, F: Fn(&T) -> i64>(
    mut entries: Vec<T>,
    page_size: usize,
    id_of: F,
) -> (Vec<T>, Option<i64>) {
    let older_id = if entries.len() > page_size {
        entries.truncate(page_size);
        entries.last().map(&id_of)
    } else {
        None
    };
    (entries, older_id)
}

pub async fn system_page(
    State(state): State<AppState>,
    Query(params): Query<SystemQuery>,
) -> Html<String> {
    let tab = normalize_system_tab(params.tab.clone());

    let filter_level = params.level.unwrap_or_else(|| "info".to_string());
    let filter_category = params.category.unwrap_or_default();
    let filter_search = params.search.unwrap_or_default();

    // Fan out every independent lookup in parallel. The previous code ran
    // these six queries sequentially — the wall time was the sum of all
    // RTTs. With `tokio::join!` each future races on its own pool
    // connection and the handler waits on the slowest one only.
    let logs_before_id = params.before_id;
    let logs_fut = async {
        if tab == "logs" {
            log::query(
                &state.db,
                &log::LogQuery {
                    level: Some(filter_level.clone()),
                    category: if filter_category.is_empty() {
                        None
                    } else {
                        Some(filter_category.clone())
                    },
                    search: if filter_search.is_empty() {
                        None
                    } else {
                        Some(filter_search.clone())
                    },
                    // Fetch one extra row so the template can tell
                    // whether there's an "Older" page to link to
                    // (without a separate COUNT query). Drop the
                    // extra below before passing to the template.
                    limit: 201,
                    before_id: logs_before_id,
                },
            )
            .await
            .unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let rss_before_id = params.before_id;
    let rss_recent_fut = async {
        if tab == "rss" {
            // Same +1 trick the logs query uses: fetch one extra row
            // so we can tell whether "Older →" should render without
            // a separate COUNT query. Truncated below.
            rss::recent_decisions_paginated(&state.db, 201, rss_before_id)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let scheduled_tasks_fut = async {
        if tab == "tasks" {
            scheduled_tasks::list(&state.db).await.unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let misgrab_entries_fut = async {
        if tab == "misgrabs" {
            let title_language = crate::models::config::get_config(&state.db)
                .await
                .ok()
                .flatten()
                .map(|c| c.title_language)
                .unwrap_or_else(|| "romaji".to_string());
            crate::models::grabbed_torrents::list_misgrabs(&state.db, &title_language)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let review_entries_fut = async {
        if tab == "review" {
            let mut entries = episode_tags::get_needs_review(&state.db)
                .await
                .unwrap_or_default();
            populate_series_cover_urls(
                &state.db,
                &mut entries,
                |e| e.series_id,
                |entry, url| entry.cover_url = url,
            )
            .await;
            entries
        } else {
            Vec::new()
        }
    };
    // Issue gh-121 — Notifications tab. Only loads when the user is
    // viewing this tab so the join below stays cheap on every other
    // tab. Edit-target resolution moved to the modal-fetch endpoint
    // (`GET /system/notifications/{id}/edit-form`) when the card+modal
    // frontend landed, so this future just loads the provider list +
    // the default-on matrix seed for the create-form path.
    let notification_payload_fut = async {
        if tab == "notifications" {
            let providers = notifications::load_provider_views(&state.db).await;
            let toggles = notifications::matrix_view(&state.db, None).await;
            (providers, toggles)
        } else {
            (Vec::new(), Vec::new())
        }
    };

    let (
        logs,
        cfg_res,
        rss_last_run_res,
        rss_recent,
        scheduled_tasks,
        log_count_res,
        review_entries,
        misgrab_entries,
        (notification_providers, notification_event_toggles),
    ) = tokio::join!(
        logs_fut,
        config::get_config(&state.db),
        rss::latest_run(&state.db),
        rss_recent_fut,
        scheduled_tasks_fut,
        log::count(&state.db),
        review_entries_fut,
        misgrab_entries_fut,
        notification_payload_fut,
    );
    let cfg = cfg_res.ok().flatten();
    let rss_last_run = rss_last_run_res.unwrap_or(None);
    let log_count = log_count_res.unwrap_or(0);

    let force_mal_fallback = cfg
        .as_ref()
        .map(|cfg| cfg.force_mal_fallback)
        .unwrap_or(false);
    let force_kitsu_fallback = cfg
        .as_ref()
        .map(|cfg| cfg.force_kitsu_fallback)
        .unwrap_or(false);
    let rss_enabled = cfg.as_ref().map(|cfg| cfg.rss_enabled).unwrap_or(false);
    let rss_interval_minutes = cfg
        .as_ref()
        .map(|cfg| cfg.rss_interval_minutes)
        .unwrap_or(5);

    let categories = vec![
        ("search", LogCategory::Search.label()),
        ("grab", LogCategory::Grab.label()),
        ("auto_search", LogCategory::AutoSearch.label()),
        ("nyaa", LogCategory::Nyaa.label()),
        ("rss", LogCategory::Rss.label()),
        ("anilist", LogCategory::AniList.label()),
        ("jikan", LogCategory::Jikan.label()),
        ("kitsu", LogCategory::Kitsu.label()),
        ("download_client", LogCategory::DownloadClient.label()),
        ("jellyfin", LogCategory::Jellyfin.label()),
        ("media", LogCategory::Media.label()),
        ("library", LogCategory::Library.label()),
        ("auth", LogCategory::Auth.label()),
        ("system", LogCategory::System.label()),
        ("post_process", LogCategory::PostProcess.label()),
        ("scoring", LogCategory::Scoring.label()),
        ("quality", LogCategory::Quality.label()),
        ("external_sync", LogCategory::ExternalSync.label()),
        ("notifications", LogCategory::Notifications.label()),
    ];

    let title_language = cfg
        .as_ref()
        .map(|c| c.title_language.clone())
        .unwrap_or_else(|| "english".to_string());
    // Pagination cursor handling: the query asked for `limit + 1` so
    // we can detect whether an "Older" page exists without a separate
    // COUNT. If we got the extra row, drop it and stash the oldest
    // visible row's id as the `before_id` for the next page; if we
    // got fewer than the limit, this is the last page.
    let (logs, log_older_id) = truncate_to_page(logs, 200, |e| e.id);
    let (rss_recent, rss_older_id) = truncate_to_page(rss_recent, 200, |e| e.id);
    let recycle_unwritable = crate::services::recycle::is_unwritable();
    let backup_view = if tab == "backup" {
        Some(backup::backup_tab_view(&state).await)
    } else {
        None
    };
    let template = SystemTemplate {
        page: "system".to_string(),
        tab,
        force_mal_fallback,
        force_kitsu_fallback,
        recycle_unwritable,
        backup: backup_view,
        debug_message: params.message,
        debug_error: params.error,
        logs,
        log_count,
        filter_level,
        filter_category,
        filter_search,
        log_before_id: logs_before_id,
        log_older_id,
        rss_before_id,
        rss_older_id,
        categories,
        rss_enabled,
        rss_interval_minutes,
        rss_last_run,
        rss_recent,
        scheduled_tasks,
        review_entries,
        misgrab_entries,
        title_language,
        notification_providers,
        notification_event_toggles,
    };
    Html(template.render().unwrap_or_default())
}

pub async fn debug_settings_submit(
    State(state): State<AppState>,
    Form(form): Form<DebugSettingsForm>,
) -> Html<String> {
    let mut cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    cfg.force_mal_fallback = form.force_mal_fallback.is_some();
    cfg.force_kitsu_fallback = form.force_kitsu_fallback.is_some();

    let result = config::save_config(&state.db, &cfg).await;
    let (message, error) = match result {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                "Updated fallback debug settings",
                &format!(
                    "mal_jikan={}, kitsu={}",
                    if cfg.force_mal_fallback {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if cfg.force_kitsu_fallback {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ),
            )
            .await;
            (
                Some(format!(
                    "Fallback debug settings saved. MAL/Jikan: {}. Kitsu: {}.",
                    if cfg.force_mal_fallback {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if cfg.force_kitsu_fallback {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )),
                None,
            )
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Failed to update fallback debug settings",
                &e.to_string(),
            )
            .await;
            (None, Some(format!("Failed to save debug settings: {}", e)))
        }
    };

    let recycle_unwritable = crate::services::recycle::is_unwritable();
    let template = SystemTemplate {
        page: "system".to_string(),
        tab: "debug".to_string(),
        force_mal_fallback: cfg.force_mal_fallback,
        force_kitsu_fallback: cfg.force_kitsu_fallback,
        recycle_unwritable,
        backup: None,
        debug_message: message,
        debug_error: error,
        logs: Vec::new(),
        log_count: log::count(&state.db).await.unwrap_or(0),
        filter_level: "info".to_string(),
        filter_category: String::new(),
        filter_search: String::new(),
        log_before_id: None,
        log_older_id: None,
        rss_before_id: None,
        rss_older_id: None,
        categories: vec![
            ("search", LogCategory::Search.label()),
            ("grab", LogCategory::Grab.label()),
            ("auto_search", LogCategory::AutoSearch.label()),
            ("nyaa", LogCategory::Nyaa.label()),
            ("anilist", LogCategory::AniList.label()),
            ("jikan", LogCategory::Jikan.label()),
            ("kitsu", LogCategory::Kitsu.label()),
            ("download_client", LogCategory::DownloadClient.label()),
            ("jellyfin", LogCategory::Jellyfin.label()),
            ("media", LogCategory::Media.label()),
            ("library", LogCategory::Library.label()),
            ("auth", LogCategory::Auth.label()),
            ("system", LogCategory::System.label()),
            ("post_process", LogCategory::PostProcess.label()),
            ("scoring", LogCategory::Scoring.label()),
            ("quality", LogCategory::Quality.label()),
            ("external_sync", LogCategory::ExternalSync.label()),
            ("notifications", LogCategory::Notifications.label()),
        ],
        rss_enabled: cfg.rss_enabled,
        rss_interval_minutes: cfg.rss_interval_minutes,
        rss_last_run: rss::latest_run(&state.db).await.unwrap_or(None),
        rss_recent: Vec::new(),
        scheduled_tasks: scheduled_tasks::list(&state.db).await.unwrap_or_default(),
        review_entries: Vec::new(),
        misgrab_entries: Vec::new(),
        title_language: cfg.title_language.clone(),
        notification_providers: Vec::new(),
        notification_event_toggles: Vec::new(),
    };
    Html(template.render().unwrap_or_default())
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct LogPollQuery {
    after: Option<i64>,
    level: Option<String>,
    category: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/logs/poll",
    tag = "System",
    summary = "Poll log entries",
    description = "Retrieve recent log entries, optionally filtered by level and category. Supports long-polling via the `after` parameter.",
    params(LogPollQuery),
    responses(
        (status = 200, description = "Log entries", body = Vec<log::LogEntry>),
    ),
)]
pub async fn api_logs_poll(
    State(state): State<AppState>,
    Query(params): Query<LogPollQuery>,
) -> Json<Vec<log::LogEntry>> {
    let after_id = params.after.unwrap_or(0);
    // Level + category are pushed into SQL via entries_after so the
    // 3s poll only materializes matching rows. The old path pulled
    // 100 rows per tick and filtered in memory — fine functionally
    // but wasteful when a narrow filter (e.g. level=error) matched
    // nothing in a quiet window.
    let entries = log::entries_after(
        &state.db,
        after_id,
        100,
        params.level.as_deref(),
        params.category.as_deref(),
    )
    .await
    .unwrap_or_default();

    Json(entries)
}

#[utoipa::path(
    post,
    path = "/api/system/rebuild-anilist-cache",
    tag = "System",
    summary = "Rebuild metadata cache",
    description = "Re-fetch and rebuild the cached AniList/MAL metadata for all tracked series.",
    responses(
        (status = 200, description = "Rebuild report", body = serde_json::Value),
    ),
)]
pub async fn api_rebuild_cached_metadata(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    // Detach the sweep from the request handler's lifetime. Previously
    // this was a direct `.await` on the sweep future — so when the
    // client navigated away mid-rebuild, Axum dropped the handler
    // future and cancellation propagated into the loop, stopping the
    // rebuild partway through with no trace other than a browser-side
    // `NetworkError when attempting to fetch resource.` bubbling back
    // into the logs via the client's error toast.
    //
    // Three layers of `tokio::spawn`:
    //   - **Outer** task owns `mark_finished` + translates the middle
    //     task's outcome into the HTTP response. Its body is
    //     maximally simple — one spawn, one await returning Result,
    //     one match, one DB update — so the `scheduled_task_runs`
    //     row is guaranteed to exit `last_status = 'running'` even
    //     if the middle layer panics in a code path we didn't
    //     anticipate (bad future arm in a match, a panic inside
    //     `mark_started`, etc.).
    //   - **Middle** task owns `mark_started` and the inner rebuild
    //     orchestration. Any panic here surfaces as `Err(JoinError)`
    //     on the outer's `.await` and gets translated to a terminal
    //     `"error"` status by the outer.
    //   - **Inner** task runs the actual sweep; its JoinError (on
    //     panic) is caught by the middle task and folded into its
    //     own result so the outer sees a single combined outcome.
    //
    // Distinct task key (`metadata_rebuild`, not the shared
    // `metadata_refresh`) so the manual full-rebuild doesn't overwrite
    // the scheduled 12h `refresh_all_series_metadata` status row
    // when the two overlap — they're semantically different
    // operations and the audit trail for each should stand alone.
    let db = state.db.clone();
    let outer = tokio::spawn(async move {
        let middle_db = db.clone();
        let middle = tokio::spawn(async move {
            let _ = scheduled_tasks::mark_started(
                &middle_db,
                "metadata_rebuild",
                "Manual metadata cache rebuild started",
            )
            .await;

            let rebuild_db = middle_db.clone();
            let inner = tokio::spawn(async move {
                metadata_sync::rebuild_cached_metadata_for_all(&rebuild_db).await
            });
            inner.await // Result<(usize, usize, usize), JoinError>
        });

        let (status, detail, payload): (&str, String, Option<(usize, usize, usize)>) =
            match middle.await {
                Ok(Ok((rebuilt, skipped, failed))) => {
                    let st = if failed > 0 { "warn" } else { "ok" };
                    (
                        st,
                        format!("rebuilt={rebuilt}, skipped={skipped}, failed={failed}"),
                        Some((rebuilt, skipped, failed)),
                    )
                }
                Ok(Err(join_err)) => {
                    // Inner panicked. The middle task caught it and
                    // bubbled it up cleanly.
                    let kind = if join_err.is_panic() {
                        "panicked"
                    } else {
                        "join error"
                    };
                    ("error", format!("rebuild sweep {kind}: {join_err}"), None)
                }
                Err(join_err) => {
                    // Middle itself panicked — e.g. `mark_started`
                    // internals, or something between the nested
                    // spawns. Still mark the run finished so the
                    // status row exits `running`.
                    let kind = if join_err.is_panic() {
                        "panicked"
                    } else {
                        "join error"
                    };
                    (
                        "error",
                        format!("rebuild orchestration task {kind}: {join_err}"),
                        None,
                    )
                }
            };
        let _ = scheduled_tasks::mark_finished(&db, "metadata_rebuild", status, &detail).await;
        payload
    });

    let payload = outer.await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("rebuild orchestration task failed to join: {}", e),
        )
    })?;

    let Some((rebuilt, skipped, failed)) = payload else {
        // Inner panicked — we already wrote an "error" row into
        // scheduled_task_runs so operators can see what happened.
        // Surface a 500 to the client (on the happy path where they
        // stayed on the page) so they don't think it silently
        // succeeded.
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Rebuild task panicked; see scheduled tasks for details.".to_string(),
        ));
    };

    let message = format!(
        "Metadata cache rebuild complete. Rebuilt: {}. Skipped: {}. Failed: {}.",
        rebuilt, skipped, failed
    );

    Ok(Json(serde_json::json!({
        "ok": failed == 0,
        "rebuilt": rebuilt,
        "skipped": skipped,
        "failed": failed,
        "message": message,
    })))
}

#[utoipa::path(
    post,
    path = "/api/system/reload-anibridge",
    tag = "System",
    summary = "Reload Anibridge mappings",
    description = "Re-download the AniList-to-MAL ID mapping table from Anibridge.",
    responses(
        (status = 200, description = "Mappings reloaded", body = serde_json::Value),
        (status = 502, description = "Reload failed"),
    ),
)]
pub async fn api_anibridge_reload(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    detached_task(async move {
        logger::info(
            &state.db,
            LogCategory::System,
            "Anibridge mappings reload requested",
            "",
        )
        .await;
        let _ = scheduled_tasks::mark_started(
            &state.db,
            "anibridge_refresh",
            "Manual anibridge mappings refresh",
        )
        .await;

        if crate::services::anibridge::reload().await {
            let _ = scheduled_tasks::mark_finished(
                &state.db,
                "anibridge_refresh",
                "ok",
                "Mappings refreshed",
            )
            .await;
            Ok(Json(serde_json::json!({
                "ok": true,
                "message": "Anibridge mappings reloaded successfully",
            })))
        } else {
            let _ = scheduled_tasks::mark_finished(
                &state.db,
                "anibridge_refresh",
                "error",
                "Failed to download mappings",
            )
            .await;
            Err((
                axum::http::StatusCode::BAD_GATEWAY,
                "Failed to reload anibridge mappings".to_string(),
            ))
        }
    })
    .await?
}

#[utoipa::path(
    post,
    path = "/api/logs/clear",
    tag = "System",
    summary = "Clear all logs",
    description = "Delete all log entries from the database.",
    responses(
        (status = 200, description = "Logs cleared", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn api_logs_clear(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    logger::info(&state.db, LogCategory::System, "Logs cleared by user", "").await;
    sqlx::query("DELETE FROM logs")
        .execute(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Query for the log-export endpoint. `range` selects a quick preset
/// or `all` for everything. Date-range support could land later via
/// explicit `since` / `until` ISO timestamps; the preset form covers
/// the common cases (recent debugging, weekly snapshot for support).
#[derive(Deserialize, utoipa::ToSchema)]
pub struct LogExportQuery {
    /// `today` / `7d` / `30d` / `all`. Anything else coerces to `all`
    /// so a typo can't return an empty file.
    #[serde(default)]
    pub range: String,
}

#[utoipa::path(
    get,
    path = "/api/logs/export",
    tag = "System",
    summary = "Download logs as a tab-separated text file",
    description = "Returns the full log table (or a date-bounded subset) as a downloadable plain-text file. \
                   `range` selects a quick preset: `today` (since midnight UTC), `7d` (last 7 days), `30d` \
                   (last 30 days), or `all` (no date filter). Format: tab-separated columns \
                   `timestamp\\tlevel\\tcategory\\tmessage\\tdetail` with a header row, one entry per line. \
                   Suitable for grep / awk / pasting into a bug report.",
    params(
        ("range" = Option<String>, Query, description = "today / 7d / 30d / all"),
    ),
    responses(
        (status = 200, description = "Plain-text log dump (Content-Disposition: attachment)"),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn api_logs_export(
    State(state): State<AppState>,
    Query(q): Query<LogExportQuery>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    use axum::http::{HeaderMap, HeaderValue, header};
    use axum::response::IntoResponse;

    // Map the range preset to a SQL date filter. SQLite's
    // `datetime('now', '-N days')` keeps the cutoff comparison cheap
    // and lets the index on `timestamp` do its job.
    let (since_clause, since_label): (&str, &str) = match q.range.as_str() {
        "today" => ("timestamp >= datetime('now', 'start of day')", "today"),
        "7d" => ("timestamp >= datetime('now', '-7 days')", "7d"),
        "30d" => ("timestamp >= datetime('now', '-30 days')", "30d"),
        // Anything else (including empty / malformed) falls through
        // to the unbounded "all" — better to return more data than
        // none on a typo.
        _ => ("1=1", "all"),
    };

    let sql = format!(
        "SELECT timestamp, level, category, message, detail \
         FROM logs WHERE {since_clause} ORDER BY id ASC"
    );
    let rows: Vec<(String, String, String, String, String)> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_all(&state.db)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Tab-separated with a header row. Embedded tabs / newlines / CRs
    // in the message or detail body are escaped to spaces so each entry
    // stays on a single line — matters for grep / awk consumption. CR
    // is in the escape set defensively: tracing output realistically
    // never contains one, but a panic message or external-service
    // error round-tripped through `logger::*` could, and a bare \r
    // makes some line-oriented tools treat it as a record separator.
    let mut body = String::with_capacity(rows.len() * 128);
    body.push_str("timestamp\tlevel\tcategory\tmessage\tdetail\n");
    for (ts, level, category, message, detail) in &rows {
        let m = message.replace(['\t', '\n', '\r'], " ");
        let d = detail.replace(['\t', '\n', '\r'], " ");
        body.push_str(&format!("{ts}\t{level}\t{category}\t{m}\t{d}\n"));
    }

    // Dated filename so a user downloading multiple snapshots doesn't
    // overwrite earlier ones. Use the chrono UTC date — local-time
    // formatting would be misleading since the timestamps inside the
    // file are SQLite-default UTC.
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let filename = format!("ryokan-logs-{date}-{since_label}.tsv");

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/tab-separated-values; charset=utf-8"),
    );
    let disposition = format!("attachment; filename=\"{filename}\"");
    if let Ok(val) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, val);
    }
    Ok((headers, body).into_response())
}

/// Payload for the client-side log ingestion endpoint. Every in-app toast
/// (fired via `window.ryokanToast` in `base.html`) hits this endpoint so
/// the notification persists in the Logs tab after the transient toast
/// fades. Toasts are user-facing so mapping is straightforward:
///   kind `info`/`success` → LogLevel::Info
///   kind `warn`           → LogLevel::Warn
///   kind `error`          → LogLevel::Error
/// The `category` string is looked up against `LogCategory::from_str`
/// and falls back to `System` when the caller doesn't specify or passes
/// a value outside the known set.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct ClientLogForm {
    pub kind: String,
    pub category: Option<String>,
    pub title: String,
    pub body: Option<String>,
}

// Field-length and rate-limit caps for `/api/logs/client`. The endpoint
// is behind cookie auth + same-origin CSRF, so the threat model is a
// buggy/runaway client (or a curious user with devtools open) flooding
// the logs table — not a malicious unauthenticated attacker. The single
// global window is sufficient for a self-hosted single-user PVR; it
// would need to be per-session for a multi-tenant deployment.
const CLIENT_LOG_TITLE_MAX: usize = 512;
const CLIENT_LOG_BODY_MAX: usize = 4096;
const CLIENT_LOG_RATE_WINDOW: Duration = Duration::from_secs(60);
const CLIENT_LOG_RATE_MAX: usize = 30;

static CLIENT_LOG_HITS: LazyLock<Mutex<VecDeque<Instant>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(CLIENT_LOG_RATE_MAX)));

fn check_client_log_rate() -> bool {
    let mut hits = CLIENT_LOG_HITS.lock().unwrap();
    admit_log_event(
        &mut hits,
        Instant::now(),
        CLIENT_LOG_RATE_WINDOW,
        CLIENT_LOG_RATE_MAX,
    )
}

/// Pure sliding-window rate-limit check, factored out of
/// `check_client_log_rate` so the policy is testable without poking
/// the process-wide `CLIENT_LOG_HITS` static. Drops timestamps older
/// than `window` from the front of `hits`, then admits the event if
/// the remaining count is under `max`. On admission, records `now`.
fn admit_log_event(
    hits: &mut VecDeque<Instant>,
    now: Instant,
    window: Duration,
    max: usize,
) -> bool {
    while let Some(front) = hits.front() {
        if now.duration_since(*front) > window {
            hits.pop_front();
        } else {
            break;
        }
    }
    if hits.len() >= max {
        return false;
    }
    hits.push_back(now);
    true
}

#[utoipa::path(
    post,
    path = "/api/logs/client",
    tag = "System",
    summary = "Log a client-side toast notification",
    description = "Persists a transient in-app toast to the logs table so users can see recent notifications in the System → Logs tab after the toast has faded. Fired automatically by window.ryokanToast.",
    request_body = ClientLogForm,
    responses(
        (status = 200, description = "Toast logged", body = serde_json::Value),
        (status = 400, description = "Title or body exceeds size cap"),
        (status = 429, description = "Rate limit exceeded"),
    ),
)]
pub async fn api_logs_client(
    State(state): State<AppState>,
    Json(form): Json<ClientLogForm>,
) -> Response {
    if form.title.len() > CLIENT_LOG_TITLE_MAX
        || form.body.as_deref().map(str::len).unwrap_or(0) > CLIENT_LOG_BODY_MAX
    {
        return (StatusCode::BAD_REQUEST, "title or body too large").into_response();
    }
    if !check_client_log_rate() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "client log rate limit exceeded",
        )
            .into_response();
    }
    let level = match form.kind.as_str() {
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    };
    let category = form
        .category
        .as_deref()
        .and_then(LogCategory::from_str)
        .unwrap_or(LogCategory::System);
    let detail = form.body.as_deref().unwrap_or("");
    logger::log(&state.db, level, category, &form.title, detail).await;
    Json(serde_json::json!({"ok": true})).into_response()
}

#[utoipa::path(
    post,
    path = "/api/rss/sync",
    tag = "System",
    summary = "Trigger RSS sync",
    description = "Manually trigger an RSS feed sync to check for new episodes.",
    responses(
        (status = 200, description = "Sync completed", body = serde_json::Value),
        (status = 500, description = "Sync failed"),
    ),
)]
pub async fn api_rss_sync(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    detached_task(async move {
        let _ =
            scheduled_tasks::mark_started(&state.db, "rss_sync", "Manual RSS sync started").await;
        match rss_service::sync_once(&state, "manual").await {
            Ok(summary) => {
                let _ =
                    scheduled_tasks::mark_finished(&state.db, "rss_sync", "ok", &summary.detail)
                        .await;
                Ok(Json(serde_json::json!({
                    "ok": true,
                    "message": summary.detail,
                    "summary": summary,
                })))
            }
            Err(err) => {
                let _ = scheduled_tasks::mark_finished(&state.db, "rss_sync", "error", &err).await;
                Err((
                    axum::http::StatusCode::BAD_GATEWAY,
                    serde_json::json!({
                        "ok": false,
                        "message": err,
                    })
                    .to_string(),
                ))
            }
        }
    })
    .await?
}

#[utoipa::path(
    post,
    path = "/api/rss/clear-history",
    tag = "System",
    summary = "Clear RSS grab history",
    description = "Clear the RSS grab history so previously grabbed episodes are re-evaluated on the next sync.",
    responses(
        (status = 200, description = "History cleared", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn api_rss_clear_history(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let deleted = rss::clear_grab_history(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    logger::info(
        &state.db,
        LogCategory::System,
        "RSS grab history cleared",
        &format!("Removed {} grabbed entries", deleted),
    )
    .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "message": format!("Cleared {} grab history entries. Previously grabbed episodes will be re-evaluated on next sync.", deleted),
    })))
}

#[utoipa::path(
    post,
    path = "/api/tasks/metadata-refresh",
    tag = "System",
    summary = "Trigger metadata refresh",
    description = "Manually trigger a metadata refresh for all tracked series.",
    responses(
        (status = 200, description = "Refresh report", body = serde_json::Value),
    ),
)]
pub async fn api_force_metadata_refresh(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    detached_task(async move {
        let _ = scheduled_tasks::mark_started(
            &state.db,
            "metadata_refresh",
            "Manual metadata refresh started",
        )
        .await;
        let (refreshed, failed) = metadata_sync::refresh_all_series_metadata(&state.db).await;
        let status = if failed > 0 { "warn" } else { "ok" };
        let detail = format!("refreshed={}, failed={}", refreshed, failed);
        let _ =
            scheduled_tasks::mark_finished(&state.db, "metadata_refresh", status, &detail).await;
        Ok::<_, (StatusCode, String)>(Json(serde_json::json!({
            "ok": failed == 0,
            "message": format!("Metadata refresh complete. Refreshed: {}. Failed: {}.", refreshed, failed),
        })))
    })
    .await?
}

#[utoipa::path(
    post,
    path = "/api/tasks/airing-refresh",
    tag = "System",
    summary = "Trigger airing-schedule refresh",
    description = "Manually re-stamp the local `episode_airings` cache used by the calendar. Skips with `already running` if the supervised task is currently in flight.",
    responses(
        (status = 200, description = "Refresh report", body = serde_json::Value),
    ),
)]
pub async fn api_force_airing_refresh(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    detached_task(async move {
        // try_lock so a manual trigger during the 12h scheduled
        // tick returns "already running" rather than queuing —
        // mirrors the rss_sync / external_sync / upgrade_search
        // shapes.
        let Ok(_guard) = airing_refresh::AIRING_REFRESH_LOCK.try_lock() else {
            return Ok::<_, (StatusCode, String)>(Json(serde_json::json!({
                "ok": false,
                "message": "Airing refresh is already running",
            })));
        };
        let _ = scheduled_tasks::mark_started(
            &state.db,
            "airing_refresh",
            "Manual airing refresh started",
        )
        .await;
        let (status, message, detail) = match airing_refresh::refresh_all(&state.db).await {
            Ok(summary) => {
                let s = if summary.al_failures > 0 {
                    "warn"
                } else {
                    "ok"
                };
                let msg = format!(
                    "Airing refresh complete. Series: {}. Upserted: {}. Pruned: {}.",
                    summary.series_scanned, summary.airings_upserted, summary.airings_pruned,
                );
                (s, msg, summary.detail())
            }
            Err(err) => ("error", format!("Airing refresh failed: {err}"), err),
        };
        let _ = scheduled_tasks::mark_finished(&state.db, "airing_refresh", status, &detail).await;
        Ok::<_, (StatusCode, String)>(Json(serde_json::json!({
            "ok": status != "error",
            "message": message,
        })))
    })
    .await?
}

#[utoipa::path(
    post,
    path = "/api/tasks/cleanup",
    tag = "System",
    summary = "Trigger cleanup",
    description = "Manually trigger cleanup of old log entries and RSS decisions (older than 30 days).",
    responses(
        (status = 200, description = "Cleanup report", body = serde_json::Value),
    ),
)]
pub async fn api_force_cleanup(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    detached_task(async move {
        let _ = scheduled_tasks::mark_started(&state.db, "cleanup", "Manual cleanup started").await;
        let mut errors = Vec::new();
        if let Err(e) = crate::models::log::cleanup(&state.db, 30).await {
            errors.push(format!("logs: {}", e));
        }
        if let Err(e) = rss::cleanup_old_decisions(&state.db, 30).await {
            errors.push(format!("rss: {}", e));
        }
        let status = if errors.is_empty() { "ok" } else { "warn" };
        let detail = if errors.is_empty() {
            "Cleanup completed".to_string()
        } else {
            errors.join("; ")
        };
        let _ = scheduled_tasks::mark_finished(&state.db, "cleanup", status, &detail).await;
        Ok::<_, (StatusCode, String)>(Json(serde_json::json!({
            "ok": errors.is_empty(),
            "message": detail,
        })))
    })
    .await?
}

#[utoipa::path(
    post,
    path = "/api/tasks/external-sync",
    tag = "System",
    summary = "Trigger external watch-list sync",
    description = "Manually trigger a watch-list sync against the linked AL/MAL account. Wraps `external_sync::tick_once_or_busy` so a click while a supervised tick is in flight returns 409 instead of queueing. Returns the success path's summary (`series_added` / `series_updated` / `series_removed`) when the sync completes synchronously. Used by the System → Scheduled Tasks page's Run-now button — the OAuth-flow sync-now path at `/settings/oauth/sync-now` is a different shape (background-spawned with progress polling) and is not interchangeable.",
    responses(
        (status = 200, description = "Sync completed", body = serde_json::Value),
        (status = 400, description = "No external account is linked"),
        (status = 409, description = "Sync is already running"),
    ),
)]
pub async fn api_force_external_sync(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use crate::services::external_sync;

    // Bail early when no account is linked — the user otherwise gets
    // a confusing "no-op success" toast since `tick_once_or_busy`
    // returns Ok(empty summary) on the no-account branch. Pre-spawn:
    // `has_linked_account` is one fast indexed query, no point burning
    // a `tokio::spawn` round-trip just to bounce on the no-account
    // path.
    let has_linked = external_sync::has_linked_account(&state.db).await;
    if !has_linked {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            serde_json::json!({
                "ok": false,
                "message": "No external account is linked. Connect AL or MAL in Settings → Connections first.",
            })
            .to_string(),
        ));
    }

    // Detach the actual sync from the request future. `tick_once_or_busy`
    // can take 30s+ on a large list with the per-entry merge step, so a
    // user who clicks Run-now and tabs away should still see the row
    // exit `running` once the sync completes.
    //
    // mark_started is deferred until we know we actually have a tick to run
    // (i.e. tick_once_or_busy didn't immediately bounce on the in-flight
    // lock). Otherwise a Run-now click during a supervised tick would
    // momentarily clobber `last_started_at` on the row before the busy
    // error path's mark_finished overwrote it again, which would surface
    // as a stale-then-correct flash on the next page load.
    detached_task(async move {
        match external_sync::tick_once_or_busy(&state).await {
            Ok(summary) => {
                let _ = scheduled_tasks::mark_started(
                    &state.db,
                    "external_sync",
                    "Manual watch-list sync started",
                )
                .await;
                let _ = scheduled_tasks::mark_finished(
                    &state.db,
                    "external_sync",
                    "ok",
                    "Sync complete",
                )
                .await;
                Ok(Json(serde_json::json!({
                    "ok": true,
                    "message": format!("Sync complete: {summary}"),
                })))
            }
            Err(err) => Err((
                axum::http::StatusCode::CONFLICT,
                serde_json::json!({
                    "ok": false,
                    "message": err,
                })
                .to_string(),
            )),
        }
    })
    .await?
}

#[utoipa::path(
    post,
    path = "/api/tasks/post-processing",
    tag = "System",
    summary = "Trigger post-processing",
    description = "Manually trigger post-processing to move/rename completed downloads into the media library.",
    responses(
        (status = 200, description = "Post-processing completed", body = serde_json::Value),
    ),
)]
pub async fn api_force_post_processing(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    detached_task(async move {
        let _ = scheduled_tasks::mark_started(
            &state.db,
            "post_processing",
            "Manual post-processing run",
        )
        .await;
        post_processing::run_once(&state).await;
        let _ = scheduled_tasks::mark_finished(
            &state.db,
            "post_processing",
            "ok",
            "Manual run completed",
        )
        .await;
        Ok::<_, (StatusCode, String)>(Json(serde_json::json!({
            "ok": true,
            "message": "Post-processing run completed",
        })))
    })
    .await?
}

#[utoipa::path(
    post,
    path = "/api/tasks/library-classify",
    tag = "System",
    summary = "Classify externally-imported files",
    description = "Walk every tracked series' media folder and run the source/resolution classifier on files that don't yet have a structured classification row. Useful after importing pre-existing media from another PVR or a manual drop.",
    responses(
        (status = 200, description = "Library classify report", body = serde_json::Value),
    ),
)]
pub async fn api_force_library_classify(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    detached_task(async move {
        // try_lock the process-wide LIBRARY_CLASSIFY_LOCK so a
        // Run-now click during the supervised 6h tick (which also
        // holds the lock) returns a friendly busy message instead
        // of interleaving and flipping the scheduled-tasks row's
        // status between the two writers. Same shape as
        // `tick_once_or_busy` for external_sync.
        let _guard = match post_processing::LIBRARY_CLASSIFY_LOCK.try_lock() {
            Ok(g) => g,
            Err(_) => {
                return Err((
                    StatusCode::CONFLICT,
                    serde_json::json!({
                        "ok": false,
                        "message": "Library classify is already running.",
                    })
                    .to_string(),
                ));
            }
        };
        let _ = scheduled_tasks::mark_started(
            &state.db,
            "library_classify",
            "Manual library classify run",
        )
        .await;
        let report = post_processing::scan_library_for_unclassified(&state).await;
        let message = format!(
            "Library classify scan complete. Series scanned: {}. Files scanned: {}. Classified: {}. Needs review: {}.",
            report.series_scanned,
            report.files_scanned,
            report.files_classified,
            report.files_needing_review,
        );
        let _ =
            scheduled_tasks::mark_finished(&state.db, "library_classify", "ok", &message).await;
        Ok(Json(serde_json::json!({
            "ok": true,
            "message": message,
            "series_scanned": report.series_scanned,
            "files_scanned": report.files_scanned,
            "files_classified": report.files_classified,
            "files_needing_review": report.files_needing_review,
        })))
    })
    .await?
}

#[utoipa::path(
    post,
    path = "/api/tasks/upgrade-search",
    tag = "System",
    summary = "Trigger quality upgrade search",
    description = "Manually trigger a search for quality upgrades across all monitored episodes.",
    responses(
        (status = 200, description = "Upgrade search report", body = serde_json::Value),
        (status = 500, description = "Search failed"),
    ),
)]
pub async fn api_force_upgrade_search(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    detached_task(async move {
        let _ = scheduled_tasks::mark_started(
            &state.db,
            "upgrade_search",
            "Manual upgrade search started",
        )
        .await;
        match upgrade::run_once(&state).await {
            Ok(summary) => {
                let _ = scheduled_tasks::mark_finished(
                    &state.db,
                    "upgrade_search",
                    "ok",
                    &summary.detail,
                )
                .await;
                Ok(Json(serde_json::json!({
                    "ok": true,
                    "message": summary.detail,
                    "series_checked": summary.series_checked,
                    "episodes_checked": summary.episodes_checked,
                    "upgrades_grabbed": summary.upgrades_grabbed,
                })))
            }
            Err(err) => {
                let _ = scheduled_tasks::mark_finished(&state.db, "upgrade_search", "error", &err)
                    .await;
                Err((axum::http::StatusCode::BAD_GATEWAY, err))
            }
        }
    })
    .await?
}

/// Wrapper for the `/api/system/tasks` response so OpenAPI / Swagger
/// can describe the actual `{ "tasks": [...] }` shape rather than an
/// opaque `serde_json::Value`. Pre-this-shape the path's `body =`
/// declaration was `serde_json::Value`, which Swagger UI rendered as
/// "any JSON" — clients reading the spec couldn't see the entry
/// fields. Reviewer caught this; mirror Sonarr's habit of typed
/// response wrappers.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct SystemTasksResponse {
    pub tasks: Vec<crate::services::task_registry::TaskSnapshot>,
}

#[utoipa::path(
    get,
    path = "/api/system/tasks",
    tag = "System",
    summary = "Snapshot every supervised background task's lifecycle state",
    description = "Returns one entry per task registered with the supervisor — name, current status (running / backoff), unix-seconds start of the current iteration, last exit (timestamp + cause: panic / join_error / normal), iteration exit count, and the configured backoff duration in milliseconds. Read-only snapshot; no side effects. The System page polls this for the task-status table; ops can also curl it for a quick health check (`curl /api/system/tasks | jq '.tasks[] | select(.status == \"backoff\")'` surfaces every task that's currently in a crash-loop respawn delay).",
    responses(
        (status = 200, description = "Snapshot of every registered task", body = SystemTasksResponse),
    ),
)]
pub async fn api_system_tasks(State(state): State<AppState>) -> Json<SystemTasksResponse> {
    let tasks = state.tasks.snapshot().await;
    Json(SystemTasksResponse { tasks })
}

pub mod backup;
pub mod manual_import;
pub mod notifications;

#[cfg(test)]
mod endpoint_tests;
#[cfg(test)]
mod tasks_endpoint_tests;
#[cfg(test)]
mod tests;
