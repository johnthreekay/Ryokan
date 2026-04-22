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
use crate::services::{logger, metadata_sync, post_processing, rss as rss_service, upgrade};

#[derive(Template)]
#[template(path = "system.html")]
struct SystemTemplate {
    page: String,
    tab: String,
    force_mal_fallback: bool,
    force_kitsu_fallback: bool,
    auto_grab_on_add: bool,
    allow_non_english: bool,
    debug_message: Option<String>,
    debug_error: Option<String>,
    logs: Vec<log::LogEntry>,
    log_count: i64,
    filter_level: String,
    filter_category: String,
    filter_search: String,
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
}

#[derive(Deserialize)]
pub struct SystemQuery {
    tab: Option<String>,
    level: Option<String>,
    category: Option<String>,
    search: Option<String>,
    message: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct DebugSettingsForm {
    force_mal_fallback: Option<String>,
    force_kitsu_fallback: Option<String>,
    auto_grab_on_add: Option<String>,
    allow_non_english: Option<String>,
}

fn normalize_system_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("scoring") => "scoring".to_string(),
        Some("help") => "scoring".to_string(), // legacy alias
        Some("debug") => "debug".to_string(),
        Some("rss") => "rss".to_string(),
        Some("tasks") => "tasks".to_string(),
        Some("review") => "review".to_string(),
        _ => "logs".to_string(),
    }
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
                    limit: 200,
                    before_id: None,
                },
            )
            .await
            .unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let rss_recent_fut = async {
        if tab == "rss" {
            rss::recent_decisions(&state.db, 500)
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

    let (
        logs,
        cfg_res,
        rss_last_run_res,
        rss_recent,
        scheduled_tasks,
        log_count_res,
        review_entries,
    ) = tokio::join!(
        logs_fut,
        config::get_config(&state.db),
        rss::latest_run(&state.db),
        rss_recent_fut,
        scheduled_tasks_fut,
        log::count(&state.db),
        review_entries_fut,
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
    let auto_grab_on_add = cfg.as_ref().map(|cfg| cfg.auto_grab_on_add).unwrap_or(true);
    let allow_non_english = cfg
        .as_ref()
        .map(|cfg| cfg.allow_non_english)
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
        ("anilist", LogCategory::AniList.label()),
        ("jikan", LogCategory::Jikan.label()),
        ("qbit", LogCategory::QBit.label()),
        ("jellyfin", LogCategory::Jellyfin.label()),
        ("media", LogCategory::Media.label()),
        ("library", LogCategory::Library.label()),
        ("auth", LogCategory::Auth.label()),
        ("system", LogCategory::System.label()),
        ("post_process", LogCategory::PostProcess.label()),
        ("scoring", LogCategory::Scoring.label()),
    ];

    let template = SystemTemplate {
        page: "system".to_string(),
        tab,
        force_mal_fallback,
        force_kitsu_fallback,
        auto_grab_on_add,
        allow_non_english,
        debug_message: params.message,
        debug_error: params.error,
        logs,
        log_count,
        filter_level,
        filter_category,
        filter_search,
        categories,
        rss_enabled,
        rss_interval_minutes,
        rss_last_run,
        rss_recent,
        scheduled_tasks,
        review_entries,
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
    cfg.allow_non_english = form.allow_non_english.is_some();
    cfg.auto_grab_on_add = form.auto_grab_on_add.is_some();

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

    let template = SystemTemplate {
        page: "system".to_string(),
        tab: "debug".to_string(),
        force_mal_fallback: cfg.force_mal_fallback,
        force_kitsu_fallback: cfg.force_kitsu_fallback,
        auto_grab_on_add: cfg.auto_grab_on_add,
        allow_non_english: cfg.allow_non_english,
        debug_message: message,
        debug_error: error,
        logs: Vec::new(),
        log_count: log::count(&state.db).await.unwrap_or(0),
        filter_level: "info".to_string(),
        filter_category: String::new(),
        filter_search: String::new(),
        categories: vec![
            ("search", LogCategory::Search.label()),
            ("grab", LogCategory::Grab.label()),
            ("auto_search", LogCategory::AutoSearch.label()),
            ("nyaa", LogCategory::Nyaa.label()),
            ("anilist", LogCategory::AniList.label()),
            ("jikan", LogCategory::Jikan.label()),
            ("qbit", LogCategory::QBit.label()),
            ("jellyfin", LogCategory::Jellyfin.label()),
            ("media", LogCategory::Media.label()),
            ("library", LogCategory::Library.label()),
            ("auth", LogCategory::Auth.label()),
            ("system", LogCategory::System.label()),
            ("post_process", LogCategory::PostProcess.label()),
            ("scoring", LogCategory::Scoring.label()),
        ],
        rss_enabled: cfg.rss_enabled,
        rss_interval_minutes: cfg.rss_interval_minutes,
        rss_last_run: rss::latest_run(&state.db).await.unwrap_or(None),
        rss_recent: Vec::new(),
        scheduled_tasks: scheduled_tasks::list(&state.db).await.unwrap_or_default(),
        review_entries: Vec::new(),
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
    let now = Instant::now();
    let mut hits = CLIENT_LOG_HITS.lock().unwrap();
    while let Some(front) = hits.front() {
        if now.duration_since(*front) > CLIENT_LOG_RATE_WINDOW {
            hits.pop_front();
        } else {
            break;
        }
    }
    if hits.len() >= CLIENT_LOG_RATE_MAX {
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
    let _ = scheduled_tasks::mark_started(&state.db, "rss_sync", "Manual RSS sync started").await;
    match rss_service::sync_once(&state, "manual").await {
        Ok(summary) => {
            let _ =
                scheduled_tasks::mark_finished(&state.db, "rss_sync", "ok", &summary.detail).await;
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
    let _ = scheduled_tasks::mark_started(
        &state.db,
        "metadata_refresh",
        "Manual metadata refresh started",
    )
    .await;
    let (refreshed, failed) = metadata_sync::refresh_all_series_metadata(&state.db).await;
    let status = if failed > 0 { "warn" } else { "ok" };
    let detail = format!("refreshed={}, failed={}", refreshed, failed);
    let _ = scheduled_tasks::mark_finished(&state.db, "metadata_refresh", status, &detail).await;
    Ok(Json(serde_json::json!({
        "ok": failed == 0,
        "message": format!("Metadata refresh complete. Refreshed: {}. Failed: {}.", refreshed, failed),
    })))
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
    Ok(Json(serde_json::json!({
        "ok": errors.is_empty(),
        "message": detail,
    })))
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
    let _ =
        scheduled_tasks::mark_started(&state.db, "post_processing", "Manual post-processing run")
            .await;
    post_processing::run_once(&state).await;
    let _ =
        scheduled_tasks::mark_finished(&state.db, "post_processing", "ok", "Manual run completed")
            .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "message": "Post-processing run completed",
    })))
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
pub async fn api_force_library_classify(State(state): State<AppState>) -> Json<serde_json::Value> {
    let report = post_processing::scan_library_for_unclassified(&state).await;
    let message = format!(
        "Library classify scan complete. Series scanned: {}. Files scanned: {}. Classified: {}. Needs review: {}.",
        report.series_scanned,
        report.files_scanned,
        report.files_classified,
        report.files_needing_review,
    );
    Json(serde_json::json!({
        "ok": true,
        "message": message,
        "series_scanned": report.series_scanned,
        "files_scanned": report.files_scanned,
        "files_classified": report.files_classified,
        "files_needing_review": report.files_needing_review,
    }))
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
    let _ =
        scheduled_tasks::mark_started(&state.db, "upgrade_search", "Manual upgrade search started")
            .await;
    match upgrade::run_once(&state).await {
        Ok(summary) => {
            let _ =
                scheduled_tasks::mark_finished(&state.db, "upgrade_search", "ok", &summary.detail)
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
            let _ =
                scheduled_tasks::mark_finished(&state.db, "upgrade_search", "error", &err).await;
            Err((axum::http::StatusCode::BAD_GATEWAY, err))
        }
    }
}
