//! Library CRUD handlers: add/remove series, set folder/monitoring/upgrades,
//! per-series search overrides, episode manual-override, list media folders,
//! and the MAL-fallback reconciliation endpoint.
//!
//! Split out of `handlers::library::mod` — these handlers all mutate the
//! `series`/`episode_tags` tables (or inspect the media root) and share the
//! same request flow shape: form-extract → validate → DB write → log → JSON
//! response.

use askama::Template;
use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
};
use axum_htmx::HxRequest;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, grabbed_torrents, monitoring, series};
use crate::services::{logger, media, metadata_sync, monitoring as monitoring_service};

/// Per-episode monitor button — used as both an in-loop include in
/// `templates/series.html` (the parent loop provides `id` and `ep`)
/// and as the standalone HTMX swap response from `set_episode_monitoring`.
/// The two contexts share field names (`id` for the series row id and
/// `ep.number` / `ep.monitored`) so the same partial template compiles
/// in both call sites without divergence.
#[derive(Template)]
#[template(path = "partials/series/episode_monitor_button.html")]
struct EpisodeMonitorButtonPartial {
    id: i64,
    ep: EpisodeMonitorButtonContext,
}

struct EpisodeMonitorButtonContext {
    number: i32,
    monitored: bool,
}

/// Inline save-status pill returned by `set_search_overrides` and any
/// future series-page "POST a value, show a status pill" handler that
/// fits the same shape. Issue #166 — replaces the JS-driven status
/// label that lived at `static/js/series_config.js::saveSeriesSearchOverrides`.
/// `ok=true` renders the success variant (CSS auto-fades after 2s);
/// `ok=false` renders the message inline so the failure stays visible.
#[derive(Template)]
#[template(path = "partials/series/save_status_pill.html")]
struct SaveStatusPillPartial {
    ok: bool,
    message: String,
}

use super::reconcile::reconcile_all_fallback_entries;
use super::search::{AutoSearchQuery, auto_search_series};
use super::{
    AddSeriesForm, BulkManualOverrideForm, ReclassifyEpisodeForm, RemoveSeriesForm,
    SetAllowPtUpgradesForm, SetAllowUpgradesForm, SetEpisodeMonitoringForm, SetFolderForm,
    SetManualOverrideForm, SetMonitoringForm, SetSearchOverridesForm,
};

#[utoipa::path(
    post,
    path = "/api/library/add",
    tag = "Library",
    summary = "Add series to library",
    description = "Add an anime series to the tracked library. If it already exists, updates the existing entry.",
    request_body = AddSeriesForm,
    responses(
        (status = 200, description = "Series added/updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn add_series(
    State(state): State<AppState>,
    Json(form): Json<AddSeriesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (id, created) = series::upsert(
        &state.db,
        series::SeriesCore {
            anilist_id: form.anilist_id,
            mal_id: form.mal_id,
            title: &form.title,
            title_romaji: &form.title_romaji,
            title_english: &form.title_english,
            title_native: &form.title_native,
            cover_url: &form.cover_url,
            format: &form.format,
            status: &form.status,
            episodes: form.episodes,
            season_year: form.season_year,
            // AddSeriesForm comes from the search result card which doesn't
            // carry an end date — leave null and let the metadata sync pass
            // populate it via refresh_core_metadata when the full detail
            // fetch lands.
            end_year: None,
        },
    )
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "{} library entry: {}",
            if created { "Added" } else { "Updated" },
            form.title
        ),
        &format!(
            "id={}, anilist_id={}, mal_id={:?}, format={}, episodes={:?}",
            id, form.anilist_id, form.mal_id, form.format, form.episodes
        ),
    )
    .await;

    if let Ok(Some(tracked)) = series::get_by_id(&state.db, id).await {
        let db_clone = state.db.clone();
        let tracked_clone = tracked.clone();
        tokio::spawn(async move {
            let force_fallback = crate::models::config::get_config(&db_clone)
                .await
                .ok()
                .flatten()
                .map(|c| c.force_mal_fallback)
                .unwrap_or(false);
            match metadata_sync::refresh_series_metadata(&db_clone, &tracked_clone, force_fallback)
                .await
            {
                Ok(detail) => {
                    logger::info(
                        &db_clone,
                        LogCategory::AniList,
                        &format!("Hydrated local metadata for {}", tracked_clone.title),
                        &format!(
                            "provider_id={}, mal_id={:?}, episodes={:?}",
                            detail.id, detail.id_mal, detail.episodes
                        ),
                    )
                    .await;
                    // Re-derive monitoring now that the episode map (with
                    // aired dates) is populated. `load_episode_info` is
                    // cache-only by design (no blocking Jikan walk inside
                    // the request handlers), so the synchronous
                    // `recompute_series_monitoring` in `add_series` above
                    // ran against an empty map and the aired-date-aware
                    // modes (Missing / Future) used the degraded heuristic.
                    // This catch-up pass — off the request path — picks up
                    // the real aired dates. Quiet on failure; the next
                    // monitor-mode change or series-page render recomputes.
                    let _ = monitoring_service::recompute_series_monitoring(
                        &db_clone,
                        tracked_clone.id,
                    )
                    .await;
                }
                Err(err) => {
                    logger::warn(
                        &db_clone,
                        LogCategory::AniList,
                        &format!(
                            "Failed to hydrate local metadata for {}",
                            tracked_clone.title
                        ),
                        &err,
                    )
                    .await;
                }
            }
        });

        // Stamp upcoming episode air dates inline so the
        // calendar's local-DB read returns this series on the very
        // next request rather than waiting for the next 12h
        // `airing_refresh` tick. One AL roundtrip (single
        // mediaId_in element) — negligible against the 30/min
        // budget. Quiet on failure; the supervised task will pick
        // it up later.
        let airing_db = state.db.clone();
        let airing_title = tracked.title.clone();
        let airing_id = id;
        tokio::spawn(async move {
            if let Err(err) =
                crate::services::airing_refresh::refresh_for_series(&airing_db, airing_id).await
            {
                logger::warn(
                    &airing_db,
                    LogCategory::AniList,
                    &format!("Failed to stamp airings for {}", airing_title),
                    &err,
                )
                .await;
            }
        });

        // Issue #53: kick off a one-shot classification scan for just
        // this series. If the user's media root already has files for
        // this show (pre-existing rip, manual drop, migration from
        // another PVR), they otherwise sit as UNKNOWN until the next
        // 6-hour `library_classify` sweep. The per-series scan reuses
        // the same enumeration + classify pipeline as the periodic
        // task, just scoped to the new id, and `stamp_classification_attempted`
        // ensures a row that classifies as UNKNOWN doesn't get
        // re-attempted on every future sweep.
        let scan_state = state.clone();
        let scan_title = tracked.title.clone();
        let scan_id = id;
        tokio::spawn(async move {
            let report = crate::services::post_processing::scan_series_for_unclassified(
                &scan_state,
                scan_id,
            )
            .await;
            // Only log when the scan actually did work — a typical add
            // (no pre-existing files) returns zeros and would just spam
            // the log with "Scanned 0 files for X" on every import.
            if report.files_scanned > 0 {
                logger::info(
                    &scan_state.db,
                    LogCategory::Library,
                    &format!("Initial classify scan for {}", scan_title),
                    &format!(
                        "files_scanned={}, classified={}, needs_review={}",
                        report.files_scanned, report.files_classified, report.files_needing_review,
                    ),
                )
                .await;
            }
        });
    }

    let monitor = monitoring_service::recompute_series_monitoring(&state.db, id)
        .await
        .ok();

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "created": created,
        "monitor_mode": monitor.as_ref().map(|m| m.mode.as_str()).unwrap_or("future"),
        "monitored_count": monitor.as_ref().map(|m| m.monitored_count).unwrap_or(0),
        "total_count": monitor.as_ref().map(|m| m.total_count).unwrap_or(0),
        "hydrating": true
    })))
}

#[utoipa::path(
    post,
    path = "/api/library/reconcile-fallbacks",
    tag = "Library",
    summary = "Reconcile fallback entries",
    description = "Attempt to upgrade MAL/Jikan-sourced library entries to AniList IDs.",
    responses(
        (status = 200, description = "Reconciliation report", body = serde_json::Value),
    ),
)]
pub async fn reconcile_fallbacks(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let report = reconcile_all_fallback_entries(&state.db).await;
    logger::info(
        &state.db,
        LogCategory::AniList,
        "Fallback reconciliation complete",
        &format!(
            "checked={}, upgraded={}, failed={}",
            report.checked, report.upgraded, report.failed
        ),
    )
    .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "checked": report.checked,
        "upgraded": report.upgraded,
        "failed": report.failed,
        "message": format!("Checked {}, upgraded {}, failed {}", report.checked, report.upgraded, report.failed),
    })))
}

#[utoipa::path(
    post,
    path = "/api/library/remove",
    tag = "Library",
    summary = "Remove series from library",
    description = "Remove a tracked series from the library by its internal database ID.",
    request_body = RemoveSeriesForm,
    responses(
        (status = 200, description = "Series removed", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn remove_series(
    State(state): State<AppState>,
    Json(form): Json<RemoveSeriesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, Json<serde_json::Value>)> {
    let series_id = form.id;
    let delete_files = form.delete_files.unwrap_or(true);

    // Centralised error exit for this handler. Before the rss_seen fix,
    // any failure here (FK violations, stale grabbed_torrents, qBit
    // misconfiguration) returned a plain-text 500 body that the JS in
    // series.html silently swallowed, leaving the user staring at a
    // generic "Error" on the button with nothing in the logs tab. This
    // helper:
    //   1. Writes a LogCategory::Library error row so the failure shows
    //      up in the app's own logs view, not just the devtools network
    //      tab.
    //   2. Returns a JSON body `{ok:false, stage, message}` so the
    //      frontend can display the real reason without having to
    //      special-case content types.
    async fn fail_with(
        db: &sqlx::SqlitePool,
        series_id: i64,
        stage: &'static str,
        err: String,
    ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        logger::error(
            db,
            LogCategory::Library,
            &format!("Remove from library failed at {} (id={})", stage, series_id),
            &err,
        )
        .await;
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "stage": stage,
                "message": err,
            })),
        )
    }

    // Look up the series row up front so we have folder_name to delete on
    // disk and a useful title for the log line. A missing row isn't fatal
    // — the DB delete below is idempotent — but we have no folder/torrent
    // cleanup work to do in that case.
    let tracked = match series::get_by_id(&state.db, series_id).await {
        Ok(t) => t,
        Err(e) => return Err(fail_with(&state.db, series_id, "lookup", e.to_string()).await),
    };

    // Filesystem + torrent cleanup is shared with `bulk::delete_one_series`
    // via the `cleanup_series_files` helper. Both paths went out of sync
    // before — bulk silently bypassed the canonicalize+starts_with
    // traversal guard and the issue-#28 PT seed-rule check (PR #164
    // review). Single-helper keeps them in lockstep.
    let cleanup_report: Option<super::cleanup::SeriesCleanupReport> = if delete_files
        && let Some(ref tracked) = tracked
    {
        let (media_root, recycle_bin_path): (Option<String>, String) =
            match config::get_config(&state.db).await.ok().flatten() {
                Some(c) => (Some(c.media_root), c.recycle_bin_path),
                None => (None, String::new()),
            };
        match super::cleanup::cleanup_series_files(
            &state,
            series_id,
            &tracked.folder_name,
            &tracked.title,
            media_root.as_deref(),
            &recycle_bin_path,
        )
        .await
        {
            Ok(r) => Some(r),
            Err(e) => return Err(fail_with(&state.db, series_id, "list_grabbed_torrents", e).await),
        }
    } else {
        None
    };

    // Pull report fields back out for the response/log shape; using
    // defaults when delete_files=false or `tracked` was None.
    let torrents_removed = cleanup_report
        .as_ref()
        .map(|r| r.torrents_removed)
        .unwrap_or(0);
    let torrent_failures = cleanup_report
        .as_ref()
        .map(|r| r.torrent_failures.clone())
        .unwrap_or_default();
    let folder_status = cleanup_report
        .as_ref()
        .map(|r| r.folder_status)
        .unwrap_or("skipped");
    let folder_detail = cleanup_report
        .as_ref()
        .map(|r| r.folder_detail.clone())
        .unwrap_or_default();
    let jellyfin_status = cleanup_report
        .as_ref()
        .map(|r| r.jellyfin_status)
        .unwrap_or("skipped");

    // Remove the DB tracking rows. This is the irreversible step, so
    // do it last — if filesystem cleanup blew up the operator can
    // still inspect the half-cleaned state via the Library page.
    if let Err(e) = series::remove(&state.db, series_id).await {
        return Err(fail_with(&state.db, series_id, "delete_series", e.to_string()).await);
    }

    // Scrub user-controlled strings for the log line.
    let series_label = tracked
        .as_ref()
        .map(|t| crate::handlers::auth::sanitize_for_log(&t.title))
        .unwrap_or_else(|| format!("id={}", series_id));
    let safe_folder_detail = crate::handlers::auth::sanitize_for_log(&folder_detail);
    let safe_torrent_failures: Vec<String> = torrent_failures
        .iter()
        .map(|e| crate::handlers::auth::sanitize_for_log(e))
        .collect();
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Removed from library: {}", series_label),
        &format!(
            "id={}, delete_files={}, torrents_removed={}, folder={}{}{}, jellyfin={}",
            series_id,
            delete_files,
            torrents_removed,
            folder_status,
            if safe_folder_detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", safe_folder_detail)
            },
            if safe_torrent_failures.is_empty() {
                String::new()
            } else {
                format!(", torrent_errors=[{}]", safe_torrent_failures.join("; "))
            },
            jellyfin_status,
        ),
    )
    .await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "torrents_removed": torrents_removed,
        "torrent_errors": torrent_failures,
        "folder": folder_status,
        "jellyfin": jellyfin_status,
    })))
}

#[utoipa::path(
    post,
    path = "/api/library/folder",
    tag = "Library",
    summary = "Set series folder name",
    description = "Set the media library folder name for a tracked series.",
    request_body = SetFolderForm,
    responses(
        (status = 200, description = "Folder updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_folder(
    State(state): State<AppState>,
    Json(form): Json<SetFolderForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    // Validate the folder name before touching the DB or the filesystem.
    let sanitized = crate::services::media::sanitize_folder_name(&form.folder_name);
    if sanitized.is_empty() || sanitized != form.folder_name {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "Invalid folder name".to_string(),
        ));
    }

    series::update_folder(&state.db, form.series_id, &sanitized)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = monitoring_service::recompute_series_monitoring(&state.db, form.series_id).await;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Sentinel value the dropdown sends for "Sync from AL/MAL". Clears
/// the manual-override flag without touching `monitor_mode` — the
/// next sync tick (manual via "Sync now" or the supervised cadence)
/// computes the AL-derived mode and applies it. Doesn't immediately
/// flip the mode here because we don't want a stale value briefly
/// visible while we wait for the network fetch; the dropdown UI
/// tells the user "Will follow your AL/MAL list" until that happens.
pub(crate) const MONITOR_MODE_SYNC_SENTINEL: &str = "sync";

/// Apply a monitor-mode change to one series + recompute the per-episode
/// monitoring rows that depend on it. Extracted from `set_monitoring`
/// (the per-series handler) so `handlers::library::bulk` can reuse the
/// same write path without duplicating the sentinel-vs-explicit branch.
///
/// Returns `Result<(), String>` to fit the bulk-handler aggregation
/// shape (per-series failures collected into `BulkOutcome.failed`
/// rather than aborting the batch). The string is user-displayable;
/// callers may surface it directly in toasts / failure modals.
pub(crate) async fn apply_monitor_mode(
    db: &sqlx::SqlitePool,
    series_id: i64,
    mode_str: &str,
) -> Result<(), String> {
    // Preflight existence check. Without this, both the sentinel-
    // clear and explicit-pin SQL UPDATE paths return Ok(()) for a
    // non-existent id (sqlite UPDATE-affects-0-rows is not an
    // error). A stale-tab id from a slow-tab race would silently
    // land in `BulkOutcome.succeeded` and the user would never see
    // anything went wrong. Reported by PR #164 review.
    let exists = series::get_by_id(db, series_id)
        .await
        .map_err(|e| format!("Lookup failed: {e}"))?
        .is_some();
    if !exists {
        return Err(format!("Series {series_id} no longer exists"));
    }

    if mode_str == MONITOR_MODE_SYNC_SENTINEL {
        series::update_monitor_mode_manual_override(db, series_id, false)
            .await
            .map_err(|e| format!("Failed to clear monitor override: {e}"))?;
    } else {
        let mode = monitoring::MonitorMode::from_str(mode_str);
        series::update_monitor_mode_with_override(db, series_id, mode.as_str(), true)
            .await
            .map_err(|e| format!("Failed to set monitor mode: {e}"))?;
    }
    // Best-effort recompute. A failure here means the per-episode
    // monitor flags are stale until the next sync tick, but the
    // primary write succeeded; report success to the caller and
    // let the supervised loop fix the per-episode rows on its
    // next pass. Same posture as the existing `set_monitoring`
    // handler, which logs the recompute error and returns 200.
    let _ = monitoring_service::recompute_series_monitoring(db, series_id).await;
    Ok(())
}

#[utoipa::path(
    post,
    path = "/api/library/monitoring",
    tag = "Library",
    summary = "Set series monitoring mode",
    description = "Update the monitoring mode (all, future, none, etc.) for a tracked series.",
    request_body = SetMonitoringForm,
    responses(
        (status = 200, description = "Monitoring updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_monitoring(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<SetMonitoringForm>,
) -> Result<Response, (StatusCode, String)> {
    let series_id = form.series_id;

    // "sync" sentinel: clear the manual-override flag, leave the
    // monitor_mode + monitoring rows in place. Next sync tick will
    // fix them. Returns the existing summary unchanged so the UI
    // reads the current state until the next sync.
    if form.monitor_mode == MONITOR_MODE_SYNC_SENTINEL {
        series::update_monitor_mode_manual_override(&state.db, series_id, false)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let summary = monitoring_service::recompute_series_monitoring(&state.db, series_id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        logger::info(
            &state.db,
            LogCategory::Library,
            &format!("Cleared monitor_mode override for series {}", series_id),
            "next sync tick will apply the AL/MAL-derived monitor_mode",
        )
        .await;
        return Ok(monitoring_response(
            is_htmx,
            serde_json::json!({
                "ok": true,
                "monitor_mode": summary.mode.as_str(),
                "monitor_mode_label": summary.mode.label(),
                "monitor_mode_manual_override": false,
                "monitored_count": summary.monitored_count,
                "total_count": summary.total_count,
            }),
        ));
    }

    let mode = monitoring::MonitorMode::from_str(&form.monitor_mode);
    // Atomic write of monitor_mode + the manual-override flag in a
    // single UPDATE so a partial failure can't leave the row in the
    // "new mode without pin" surprise state — which would silently
    // let the next sync tick overwrite the user's choice. The
    // monitoring-rows recompute that follows is idempotent on
    // episode_monitor_state; running it twice is harmless.
    series::update_monitor_mode_with_override(&state.db, series_id, mode.as_str(), true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let summary = monitoring_service::recompute_series_monitoring(&state.db, series_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Updated monitoring for series {}", series_id),
        &format!(
            "mode={}, monitored={}/{}",
            summary.mode.as_str(),
            summary.monitored_count,
            summary.total_count
        ),
    )
    .await;

    // Auto-grab monitored episodes when either:
    //   1. The caller explicitly asked for it via `form.auto_grab`
    //      (e.g. the add-series flow does this once to seed the
    //      library), gated on `config.auto_grab_on_add`; or
    //   2. `config.search_on_monitoring_change` is on (v1.3.0 opt-in
    //      flag). This fires on every monitoring change so users who
    //      flip `none → all` get an immediate delta-search without
    //      needing to click an extra button.
    //
    // Either trigger bails when the mode is `None` (nothing to search
    // for) or when no download client is configured. `auto_search_
    // series` internally walks only monitored-and-missing episodes,
    // so narrowing transitions (`all → missing`) are a natural no-op.
    if mode != monitoring::MonitorMode::None
        && summary.monitored_count > 0
        && state.default_download_client().await.is_some()
    {
        let cfg = config::get_config(&state.db).await.ok().flatten();
        let auto_grab_on_add = cfg.as_ref().map(|c| c.auto_grab_on_add).unwrap_or(true);
        let search_on_change = cfg
            .as_ref()
            .map(|c| c.search_on_monitoring_change)
            .unwrap_or(false);

        let should_search =
            (form.auto_grab.unwrap_or(false) && auto_grab_on_add) || search_on_change;

        if should_search {
            // Only the add-series flow has metadata hydration in
            // flight — that path passes `form.auto_grab = true`. A
            // pure monitoring-toggle (flipped by the user on an
            // already-tracked series) has nothing to wait for, so
            // skip the 3s delay and kick off the search immediately.
            // Reduces the "user flips monitoring then closes the tab"
            // race window and makes the interactive case feel instant.
            let needs_hydration_delay = form.auto_grab.unwrap_or(false);
            let state_clone = state.clone();
            tokio::spawn(async move {
                if needs_hydration_delay {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                let _ = auto_search_series(
                    axum::extract::State(state_clone),
                    axum::extract::Path(series_id),
                    axum::extract::Query(AutoSearchQuery::default()),
                )
                .await;
            });
        }
    }

    Ok(monitoring_response(
        is_htmx,
        serde_json::json!({
            "ok": true,
            "monitor_mode": summary.mode.as_str(),
            "monitor_mode_label": summary.mode.label(),
            "monitor_mode_manual_override": true,
            "monitored_count": summary.monitored_count,
            "total_count": summary.total_count,
        }),
    ))
}

/// HTMX path returns empty 200 + `HX-Refresh: true` so htmx triggers
/// a real `window.location.reload()` — equivalent to the prior JS
/// `location.reload()` in setMonitoring + confirmMonitoring without
/// the imperative fetch wrapper. Non-HTMX callers (`toggleMonitorAll`
/// in `series_episode_actions.js` which updates many DOM elements
/// imperatively) keep getting the JSON summary they consume.
fn monitoring_response(is_htmx: bool, body: serde_json::Value) -> Response {
    if is_htmx {
        (
            [(
                axum::http::header::HeaderName::from_static("hx-refresh"),
                "true",
            )],
            StatusCode::OK,
        )
            .into_response()
    } else {
        Json(body).into_response()
    }
}

#[utoipa::path(
    post,
    path = "/api/library/episode-monitoring",
    tag = "Library",
    summary = "Set episode monitoring",
    description = "Toggle monitoring on or off for a specific episode of a tracked series.",
    request_body = SetEpisodeMonitoringForm,
    responses(
        (status = 200, description = "Episode monitoring updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_episode_monitoring(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<SetEpisodeMonitoringForm>,
) -> Result<Response, (StatusCode, String)> {
    monitoring::set_episode_monitored(
        &state.db,
        form.series_id,
        form.episode_number,
        form.monitored,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // HTMX migration (issue #129) — the per-episode monitor button on
    // the series detail page uses `hx-target="this" hx-swap="outerHTML"`,
    // so the response body must be the swapped button HTML. The partial
    // shares the same template file as the in-loop include so both call
    // sites stay in sync. JSON-on-non-HTMX path preserves the existing
    // API contract for any future programmatic caller.
    if is_htmx {
        let html = EpisodeMonitorButtonPartial {
            id: form.series_id,
            ep: EpisodeMonitorButtonContext {
                number: form.episode_number,
                monitored: form.monitored,
            },
        }
        .render()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        Ok(Html(html).into_response())
    } else {
        Ok(Json(serde_json::json!({
            "ok": true,
            "episode_number": form.episode_number,
            "monitored": form.monitored,
        }))
        .into_response())
    }
}

/// Toggle the per-series upgrade opt-in. Phase 4 feature — when the user
/// turns this off for a series, the upgrade scanner skips it entirely.
#[utoipa::path(
    post,
    path = "/api/library/allow-upgrades",
    tag = "Library",
    summary = "Toggle series upgrade opt-in",
    description = "Enable or disable automated upgrades for a single tracked series.",
    request_body = SetAllowUpgradesForm,
    responses(
        (status = 200, description = "Allow-upgrades toggled", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_allow_upgrades(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<SetAllowUpgradesForm>,
) -> Result<Response, (StatusCode, String)> {
    series::update_allow_upgrades(&state.db, form.series_id, form.allow)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "Upgrade opt-in for series {} set to {}",
            form.series_id, form.allow
        ),
        "",
    )
    .await;
    // HTMX checkboxes (#166): the visual state is already what the
    // user clicked; just acknowledge with empty 200 + `hx-swap="none"`
    // on the input. The non-HTMX branch returns JSON; note the request
    // body is form-encoded either way (this is `Form<T>`, so an
    // `application/json` POST would 415, not reach the JSON branch).
    if is_htmx {
        Ok(StatusCode::OK.into_response())
    } else {
        Ok(Json(serde_json::json!({
            "ok": true,
            "series_id": form.series_id,
            "allow_upgrades": form.allow,
        }))
        .into_response())
    }
}

/// Issue #28 — toggle the per-series PT upgrade opt-in.
/// Default off; the upgrade sweep won't grab a private-tracker
/// release as the chosen upgrade for this series unless the
/// flag is on. Initial grabs and manual searches aren't gated.
#[utoipa::path(
    post,
    path = "/api/library/allow-pt-upgrades",
    tag = "Library",
    summary = "Toggle series PT upgrade opt-in",
    description = "Enable or disable private-tracker-sourced upgrades for a single tracked series. Default off — when off, the upgrade sweep skips upgrade candidates whose source indexer is marked private (`indexers.is_private_tracker = 1`). Initial grabs and manual searches aren't affected.",
    request_body = SetAllowPtUpgradesForm,
    responses(
        (status = 200, description = "PT upgrade opt-in toggled", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_allow_pt_upgrades(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<SetAllowPtUpgradesForm>,
) -> Result<Response, (StatusCode, String)> {
    series::update_allow_pt_upgrades(&state.db, form.series_id, form.allow)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "PT upgrade opt-in for series {} set to {}",
            form.series_id, form.allow
        ),
        "",
    )
    .await;
    if is_htmx {
        Ok(StatusCode::OK.into_response())
    } else {
        Ok(Json(serde_json::json!({
            "ok": true,
            "series_id": form.series_id,
            "allow_pt_upgrades": form.allow,
        }))
        .into_response())
    }
}

/// #23 — Update the per-series search overrides (custom Nyaa tokens +
/// uploader restriction). Empty strings clear the overrides, which
/// makes the series fall back to the global `config` defaults.
#[utoipa::path(
    post,
    path = "/api/library/search-overrides",
    tag = "Library",
    summary = "Update series search overrides",
    description = "Set or clear the per-series Nyaa uploader restriction and custom query tokens. Empty strings clear the override.",
    request_body = SetSearchOverridesForm,
    responses(
        (status = 200, description = "Search overrides updated", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_search_overrides(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<SetSearchOverridesForm>,
) -> Result<Response, (StatusCode, String)> {
    let result = series::update_search_overrides(
        &state.db,
        form.series_id,
        &form.custom_query_tokens,
        &form.restrict_to_uploader,
        &form.alternate_titles,
    )
    .await;

    // HTMX inline-result swap — per templates/CLAUDE.md, always-200
    // so the partial replaces the status-pill slot regardless of
    // success/failure. Error pill renders the message instead of
    // dropping the swap (which would leave a stuck "Saving…" string).
    if is_htmx {
        if let Err(e) = result {
            let html = SaveStatusPillPartial {
                ok: false,
                message: format!("Failed to save overrides: {e}"),
            }
            .render()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            return Ok(Html(html).into_response());
        }
        logger::info(
            &state.db,
            LogCategory::Library,
            &format!("Search overrides updated for series {}", form.series_id),
            &format!(
                "tokens={:?} restrict_to={:?}",
                form.custom_query_tokens.trim(),
                form.restrict_to_uploader.trim(),
            ),
        )
        .await;
        let html = SaveStatusPillPartial {
            ok: true,
            message: String::new(),
        }
        .render()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return Ok(Html(html).into_response());
    }

    result.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Search overrides updated for series {}", form.series_id),
        &format!(
            "tokens={:?} restrict_to={:?}",
            form.custom_query_tokens.trim(),
            form.restrict_to_uploader.trim(),
        ),
    )
    .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "series_id": form.series_id,
        "custom_query_tokens": form.custom_query_tokens.trim(),
        "alternate_titles": series::normalize_alternate_titles(&form.alternate_titles),
        "restrict_to_uploader": form.restrict_to_uploader.trim(),
    }))
    .into_response())
}

/// Apply (or clear) a user's manual source/resolution override for a single
/// episode. Phase 4 feature — pins the classification so future re-classifies
/// (post-download or library scan) won't overwrite it. Passing an empty
/// `source` clears the override and re-enables automatic re-classification.
#[utoipa::path(
    post,
    path = "/api/library/manual-override",
    tag = "Library",
    summary = "Set manual source override on an episode",
    description = "Force a specific source/resolution classification on an episode, or clear an existing override.",
    request_body = SetManualOverrideForm,
    responses(
        (status = 200, description = "Override applied", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn set_manual_override(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<SetManualOverrideForm>,
) -> Result<Response, (StatusCode, String)> {
    use crate::services::source::{Resolution, Source, WebKind};

    // Validate + canonicalize the form fields *before* writing.
    let (source_str, resolution_str, web_kind_str) = if form.source.is_empty() {
        (String::new(), String::new(), String::new())
    } else {
        let parsed_source = Source::from_str(&form.source);
        if parsed_source == Source::Unknown {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid source: {:?}", form.source),
            ));
        }
        let parsed_resolution = Resolution::from_str(&form.resolution);
        if parsed_resolution == Resolution::Unknown {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid resolution: {:?}", form.resolution),
            ));
        }
        let parsed_web_kind = WebKind::from_str(&form.web_kind);
        (
            parsed_source.as_str().to_string(),
            parsed_resolution.as_str().to_string(),
            parsed_web_kind.as_str().to_string(),
        )
    };

    episode_tags::set_manual_override(
        &state.db,
        form.series_id,
        form.episode_number,
        &source_str,
        &resolution_str,
        form.is_remux,
        form.is_bdmv,
        &web_kind_str,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let action = if source_str.is_empty() {
        "cleared".to_string()
    } else {
        format!("{} {}", source_str, resolution_str)
    };
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "Manual override {} for series {} ep {}",
            action, form.series_id, form.episode_number
        ),
        "",
    )
    .await;

    // HTMX path returns empty 200 + `HX-Refresh: true` so htmx triggers
    // a full `window.location.reload()`. Matches the prior JS behavior
    // (location.reload() on success in `series_config.js`) without the
    // imperative fetch wrapper. The override modal is decorative once
    // the override lands — the row's quality tag re-renders from the
    // refreshed page state.
    if is_htmx {
        return Ok((
            [(
                axum::http::header::HeaderName::from_static("hx-refresh"),
                "true",
            )],
            StatusCode::OK,
        )
            .into_response());
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "series_id": form.series_id,
        "episode_number": form.episode_number,
        "source": source_str,
        "resolution": resolution_str,
        "is_remux": form.is_remux,
    }))
    .into_response())
}

/// Batch apply manual overrides. The bulk-actions UI on `/library/review`
/// posts an array of the same shape `set_manual_override` accepts, lets
/// each row succeed or fail independently, and returns a per-item result
/// summary so the caller can toast "N of M applied" accurately.
///
/// Validation errors on any one item return `ok: false` for that item
/// without aborting the batch — partial success is the desired semantic
/// here. For a rollback-on-any-failure semantic, the caller should
/// check `failed.is_empty()` before treating it as fully applied.
#[utoipa::path(
    post,
    path = "/api/library/bulk-manual-override",
    tag = "Library",
    summary = "Batch-apply manual source overrides",
    description = "Apply (or clear) manual overrides for multiple episodes in one call. Each item is validated independently — per-item failures are reported in `failed[]` without aborting the batch.",
    request_body = BulkManualOverrideForm,
    responses(
        (status = 200, description = "Batch processed (see per-item results)", body = serde_json::Value),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn bulk_manual_override(
    State(state): State<AppState>,
    Json(form): Json<BulkManualOverrideForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use crate::services::source::{Resolution, Source, WebKind};

    let mut applied = 0_usize;
    let mut failed: Vec<serde_json::Value> = Vec::new();

    for item in &form.items {
        let (source_str, resolution_str, web_kind_str) = if item.source.is_empty() {
            (String::new(), String::new(), String::new())
        } else {
            let parsed_source = Source::from_str(&item.source);
            if parsed_source == Source::Unknown {
                failed.push(serde_json::json!({
                    "series_id": item.series_id,
                    "episode_number": item.episode_number,
                    "error": format!("invalid source: {:?}", item.source),
                }));
                continue;
            }
            let parsed_resolution = Resolution::from_str(&item.resolution);
            if parsed_resolution == Resolution::Unknown {
                failed.push(serde_json::json!({
                    "series_id": item.series_id,
                    "episode_number": item.episode_number,
                    "error": format!("invalid resolution: {:?}", item.resolution),
                }));
                continue;
            }
            let parsed_web_kind = WebKind::from_str(&item.web_kind);
            (
                parsed_source.as_str().to_string(),
                parsed_resolution.as_str().to_string(),
                parsed_web_kind.as_str().to_string(),
            )
        };

        let write_result = episode_tags::set_manual_override(
            &state.db,
            item.series_id,
            item.episode_number,
            &source_str,
            &resolution_str,
            item.is_remux,
            item.is_bdmv,
            &web_kind_str,
        )
        .await;

        match write_result {
            Ok(_) => applied += 1,
            Err(e) => failed.push(serde_json::json!({
                "series_id": item.series_id,
                "episode_number": item.episode_number,
                "error": e.to_string(),
            })),
        }
    }

    if failed.is_empty() {
        logger::info(
            &state.db,
            LogCategory::Library,
            &format!(
                "Bulk manual override: {} of {} applied",
                applied,
                form.items.len()
            ),
            "",
        )
        .await;
    } else {
        // Surface the per-item errors through the logs table so a
        // partial failure is observable on System → Logs, not only
        // through the HTTP response body.
        let detail = serde_json::to_string(&failed).unwrap_or_default();
        logger::warn(
            &state.db,
            LogCategory::Library,
            &format!(
                "Bulk manual override: {} of {} applied, {} failed",
                applied,
                form.items.len(),
                failed.len()
            ),
            &detail,
        )
        .await;
    }

    Ok(Json(serde_json::json!({
        "ok": failed.is_empty(),
        "applied": applied,
        "requested": form.items.len(),
        "failed": failed,
    })))
}

/// Re-run the full-pipeline classifier against a single episode on demand.
///
/// Useful when the user has just edited `group_source_map` or changed a
/// custom format and wants to see the new verdict without waiting up to
/// 6 hours for the next `library_classify` sweep. Runs the same
/// `classify_post_download` + persist path as the sweep, but scoped to
/// one (series, episode) pair. Respects `manual_override` — returns
/// 409 if the row is pinned so the caller can decide whether to clear
/// the override first.
/// Classify one on-disk episode and persist the tag, the shared core of
/// the per-episode Reclassify button and the recycle-bin restore path
/// (a restored file needs its quality tag back; the delete had cleared
/// it). Errors carry the HTTP status the reclassify endpoint returns.
pub(crate) async fn reclassify_on_disk_episode(
    state: &AppState,
    series_id: i64,
    episode_number: i32,
) -> Result<crate::services::source::ClassificationResult, (axum::http::StatusCode, String)> {
    use crate::services::source::{self, SeriesContext};
    use std::path::Path;

    let series_row = series::get_by_id(&state.db, series_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            axum::http::StatusCode::NOT_FOUND,
            format!("series {} not found", series_id),
        ))?;

    // Honor user-pinned rows — a re-classify would silently be a no-op
    // against `manual_override = 1` thanks to the COALESCE guard in
    // `update_classification`, and that's more confusing than a hard
    // 409. Caller clears the override first if they want to reclassify.
    let existing_tags = episode_tags::get_for_series(&state.db, series_id)
        .await
        .unwrap_or_default();
    if existing_tags
        .get(&episode_number)
        .map(|t| t.manual_override)
        .unwrap_or(false)
    {
        return Err((
            axum::http::StatusCode::CONFLICT,
            "episode is pinned via manual override — clear the override first to re-classify"
                .to_string(),
        ));
    }

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "config not initialized".to_string(),
        ))?;

    if series_row.folder_name.is_empty() || cfg.media_root.is_empty() {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "series has no on-disk folder yet".to_string(),
        ));
    }

    let disk_files = media::scan_series_folder(&cfg.media_root, &series_row.folder_name).await;
    let file = disk_files
        .iter()
        .find(|f| {
            // Same season filter as `build_episodes`'s main pass —
            // season 1 or unseasoned only.
            let season_ok = match f.season_number {
                Some(s) => s == 1,
                None => true,
            };
            season_ok && f.episode_number == episode_number
        })
        .ok_or((
            axum::http::StatusCode::NOT_FOUND,
            format!(
                "no file on disk for episode {} — import or download it first",
                episode_number
            ),
        ))?;

    let series_root = Path::new(&cfg.media_root).join(&series_row.folder_name);
    let file_path = series_root.join(&file.filename);
    if !file_path.exists() {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            format!("file disappeared before classify: {}", file_path.display()),
        ));
    }

    // Same L1-title precedence as `scan_library_for_unclassified`:
    // prefer the original torrent name so release tags stripped from
    // the post-import filename still feed the filename layer, fall
    // back to the on-disk name for externally-imported files.
    let imported_grabs = grabbed_torrents::imported_grabs_for_series(&state.db, series_id)
        .await
        .unwrap_or_default();
    let classify_title = imported_grabs
        .iter()
        .find(|(_, eps)| eps.contains(&episode_number))
        .map(|(name, _)| name.clone())
        .or_else(|| imported_grabs.first().map(|(n, _)| n.clone()))
        .unwrap_or_else(|| file.filename.clone());

    let is_batch = grabbed_torrents::get_is_batch_by_name(&state.db, series_id, &classify_title)
        .await
        .unwrap_or(false);

    let result = source::classify_post_download(
        &state.db,
        &file_path,
        Some(&series_root),
        &classify_title,
        Some(SeriesContext {
            status: &series_row.status,
            season_year: series_row.season_year,
            end_year: series_row.end_year,
        }),
        is_batch,
    )
    .await;

    // Persist via the same branching as the post-download / scan paths:
    // UPDATE when a row exists, UPSERT via record_grab otherwise.
    let row_exists = existing_tags.contains_key(&episode_number);
    if row_exists {
        episode_tags::update_classification(&state.db, series_id, episode_number, &result)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // Issue #118 — fire `ClassifierNeedsReview` for the manual
        // reclassify path (per-episode Reclassify button on the
        // series page). Same event shape as the sweep + post-
        // download paths.
        if result.needs_review {
            let verdict = result.label();
            crate::services::notifications::emit_classifier_needs_review(
                state,
                series_id,
                episode_number,
                result.confidence as i32,
                &verdict,
            )
            .await;
        }
    } else {
        let file_size = tokio::fs::metadata(&file_path)
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        episode_tags::record_grab(
            &state.db,
            series_id,
            episode_number,
            &result,
            &classify_title,
            "",
            file_size,
            is_batch,
        )
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        episode_tags::stamp_classification_attempted(&state.db, series_id, episode_number)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // `record_grab` hardcodes state='grabbed' for both the tag and
        // history rows. The file is already on disk (checked above), so
        // flip both rows to 'completed' the same way the scan path does
        // in `services/post_processing.rs::scan_for_unclassified`.
        // Without this the UI renders a freshly-reclassified
        // externally-imported episode as download-in-progress until
        // the next 6h sweep corrects the state.
        episode_tags::mark_completed(&state.db, series_id, &[episode_number])
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let imported_basename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&classify_title)
            .to_string();
        episode_tags::mark_grab_history_completed(
            &state.db,
            series_id,
            episode_number,
            &imported_basename,
            file_size,
        )
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let label = result.label();
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "Manual re-classify for series {} ep {}: {}",
            series_id, episode_number, label
        ),
        &format!(
            "confidence={:.2}, needs_review={}",
            result.confidence, result.needs_review
        ),
    )
    .await;

    Ok(result)
}

#[utoipa::path(
    post,
    path = "/api/library/reclassify-episode",
    tag = "Library",
    summary = "Re-classify a single episode",
    description = "Run the full-pipeline classifier against the on-disk file for one episode, bypassing the six-hour sweep cadence and the #53 attempted-at skip rule. Will not overwrite a manually-pinned row — clear the override first if you want to force a re-classify.",
    request_body = ReclassifyEpisodeForm,
    responses(
        (status = 200, description = "Classification applied", body = serde_json::Value),
        (status = 404, description = "Series or on-disk file not found"),
        (status = 409, description = "Episode is pinned via manual_override"),
        (status = 500, description = "Database or classifier error"),
    ),
)]
pub async fn reclassify_episode(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<ReclassifyEpisodeForm>,
) -> Result<Response, (StatusCode, String)> {
    let result = reclassify_on_disk_episode(&state, form.series_id, form.episode_number).await?;
    let label = result.label();

    if is_htmx {
        // Render the verdict into the save-status pill so the user sees
        // the new tag + confidence before the page reloads. The 600ms
        // delay before reload (set in the template's hx-on::after-request)
        // gives them time to read it. Pill copy mirrors the prior JS
        // text at series_config.js::reclassifyEpisode.
        let pill_message = format!(
            "→ {} (conf {:.2}{})",
            label,
            result.confidence,
            if result.needs_review {
                ", needs review"
            } else {
                ""
            }
        );
        let html = SaveStatusPillPartial {
            ok: true,
            message: pill_message,
        }
        .render()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return Ok(Html(html).into_response());
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "series_id": form.series_id,
        "episode_number": form.episode_number,
        "quality_tag": label,
        "source": result.source.as_str(),
        "resolution": result.resolution.as_str(),
        "is_remux": result.is_remux,
        "is_bdmv": result.is_bdmv,
        "web_kind": result.web_kind.as_str(),
        "confidence": result.confidence,
        "needs_review": result.needs_review,
    }))
    .into_response())
}

#[utoipa::path(
    get,
    path = "/api/library/folders",
    tag = "Library",
    summary = "List media folders",
    description = "List existing folder names under the configured media root directory.",
    responses(
        (status = 200, description = "Folder list", body = Vec<String>),
    ),
)]
pub async fn list_folders(
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, (axum::http::StatusCode, String)> {
    let cfg = config::get_config(&state.db).await.ok().flatten();
    let media_root = cfg.map(|c| c.media_root).unwrap_or_default();
    let folders = media::list_media_folders(&media_root);
    Ok(Json(folders))
}

#[cfg(test)]
mod tests;
