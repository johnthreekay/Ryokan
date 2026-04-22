//! Library CRUD handlers: add/remove series, set folder/monitoring/upgrades,
//! per-series search overrides, episode manual-override, list media folders,
//! and the MAL-fallback reconciliation endpoint.
//!
//! Split out of `handlers::library::mod` — these handlers all mutate the
//! `series`/`episode_tags` tables (or inspect the media root) and share the
//! same request flow shape: form-extract → validate → DB write → log → JSON
//! response.

use axum::{extract::State, response::Json};

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, grabbed_torrents, monitoring, series};
use crate::services::{logger, media, metadata_sync, monitoring as monitoring_service};

use super::reconcile::reconcile_all_fallback_entries;
use super::search::{AutoSearchQuery, auto_search_series};
use super::{
    AddSeriesForm, BulkManualOverrideForm, ReclassifyEpisodeForm, RemoveSeriesForm,
    SetAllowUpgradesForm, SetEpisodeMonitoringForm, SetFolderForm, SetManualOverrideForm,
    SetMonitoringForm, SetSearchOverridesForm,
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

    let mut torrents_removed: u64 = 0;
    let mut torrent_failures: Vec<String> = Vec::new();
    let mut folder_status: &'static str = "skipped";
    let mut folder_detail: String = String::new();

    if delete_files && let Some(ref tracked) = tracked {
        // 1. Tell qBittorrent to drop every torrent (with files) we ever
        //    grabbed for this series.
        let hashes = match grabbed_torrents::get_all_for_series(&state.db, series_id).await {
            Ok(h) => h,
            Err(e) => {
                return Err(fail_with(
                    &state.db,
                    series_id,
                    "list_grabbed_torrents",
                    e.to_string(),
                )
                .await);
            }
        };

        if !hashes.is_empty() {
            let client_opt = state.download_client.read().await.clone();
            if let Some(client) = client_opt {
                for (_id, hash) in &hashes {
                    if hash.is_empty() {
                        continue;
                    }
                    match client.delete(hash, true).await {
                        Ok(()) => torrents_removed += 1,
                        Err(err) => torrent_failures.push(format!("{}: {}", hash, err)),
                    }
                }
            } else {
                torrent_failures.push("Download client not configured".to_string());
            }
        }

        // 2. Drop the grabbed_torrents rows for this series so the table
        //    doesn't accumulate stale references to hashes qBit just
        //    forgot about.
        if let Err(err) = grabbed_torrents::delete_all_for_series(&state.db, series_id).await {
            torrent_failures.push(format!("clear grabbed_torrents: {}", err));
        }

        // 3. Delete the series media folder. Canonicalize + assert under
        //    the configured media root before recursing.
        let cfg_opt = config::get_config(&state.db).await.ok().flatten();
        if let Some(cfg) = cfg_opt
            && !tracked.folder_name.trim().is_empty()
            && !cfg.media_root.trim().is_empty()
        {
            let series_dir = std::path::Path::new(&cfg.media_root).join(&tracked.folder_name);
            match tokio::fs::canonicalize(&cfg.media_root).await {
                Ok(media_root_canon) => match tokio::fs::canonicalize(&series_dir).await {
                    Ok(series_canon) if series_canon.starts_with(&media_root_canon) => {
                        match tokio::fs::remove_dir_all(&series_canon).await {
                            Ok(()) => {
                                folder_status = "removed";
                                folder_detail = series_canon.display().to_string();
                            }
                            Err(err) => {
                                folder_status = "error";
                                folder_detail = format!("{}: {}", series_canon.display(), err);
                            }
                        }
                    }
                    Ok(other) => {
                        folder_status = "refused";
                        folder_detail = format!(
                            "resolves outside media root: {} -> {}",
                            series_dir.display(),
                            other.display()
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        folder_status = "missing";
                        folder_detail = series_dir.display().to_string();
                    }
                    Err(err) => {
                        folder_status = "error";
                        folder_detail = format!("{}: {}", series_dir.display(), err);
                    }
                },
                Err(err) => {
                    folder_status = "error";
                    folder_detail = format!("media_root canonicalize: {}", err);
                }
            }
        }
    }

    // 4. Remove the DB tracking rows. This is the irreversible step, so
    //    do it last — if filesystem cleanup blew up the operator can
    //    still inspect the half-cleaned state via the Library page.
    if let Err(e) = series::remove(&state.db, series_id).await {
        return Err(fail_with(&state.db, series_id, "delete_series", e.to_string()).await);
    }

    // 5. Nudge Jellyfin to rescan.
    let mut jellyfin_status: &'static str = "skipped";
    if delete_files {
        let jellyfin_opt = state.jellyfin.read().await.clone();
        if let Some(jelly) = jellyfin_opt {
            jellyfin_status = match jelly.refresh_library().await {
                Ok(()) => "refreshed",
                Err(_) => "error",
            };
        }
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
    Json(form): Json<SetMonitoringForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mode = monitoring::MonitorMode::from_str(&form.monitor_mode);
    let series_id = form.series_id;
    let summary = monitoring_service::apply_monitor_mode(&state.db, series_id, mode)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

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

    // Auto-grab monitored episodes if requested (e.g. after initial add).
    if form.auto_grab.unwrap_or(false)
        && mode != monitoring::MonitorMode::None
        && summary.monitored_count > 0
        && state.download_client.read().await.is_some()
    {
        let auto_grab_on_add = config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .map(|c| c.auto_grab_on_add)
            .unwrap_or(true);

        if auto_grab_on_add {
            let state_clone = state.clone();
            tokio::spawn(async move {
                // Small delay to let metadata hydration finish.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let _ = auto_search_series(
                    axum::extract::State(state_clone),
                    axum::extract::Path(series_id),
                    axum::extract::Query(AutoSearchQuery::default()),
                )
                .await;
            });
        }
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "monitor_mode": summary.mode.as_str(),
        "monitor_mode_label": summary.mode.label(),
        "monitored_count": summary.monitored_count,
        "total_count": summary.total_count,
    })))
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
    Json(form): Json<SetEpisodeMonitoringForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    monitoring::set_episode_monitored(
        &state.db,
        form.series_id,
        form.episode_number,
        form.monitored,
    )
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "episode_number": form.episode_number,
        "monitored": form.monitored,
    })))
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
    Json(form): Json<SetAllowUpgradesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    series::update_allow_upgrades(&state.db, form.series_id, form.allow)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
    Ok(Json(serde_json::json!({
        "ok": true,
        "series_id": form.series_id,
        "allow_upgrades": form.allow,
    })))
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
    Json(form): Json<SetSearchOverridesForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    series::update_search_overrides(
        &state.db,
        form.series_id,
        &form.custom_query_tokens,
        &form.restrict_to_uploader,
    )
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
        "restrict_to_uploader": form.restrict_to_uploader.trim(),
    })))
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
    Json(form): Json<SetManualOverrideForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use crate::services::source::{Resolution, Source, WebKind};

    // Validate + canonicalize the form fields *before* writing.
    let (source_str, resolution_str, web_kind_str) = if form.source.is_empty() {
        (String::new(), String::new(), String::new())
    } else {
        let parsed_source = Source::from_str(&form.source);
        if parsed_source == Source::Unknown {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                format!("invalid source: {:?}", form.source),
            ));
        }
        let parsed_resolution = Resolution::from_str(&form.resolution);
        if parsed_resolution == Resolution::Unknown {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
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
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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

    Ok(Json(serde_json::json!({
        "ok": true,
        "series_id": form.series_id,
        "episode_number": form.episode_number,
        "source": source_str,
        "resolution": resolution_str,
        "is_remux": form.is_remux,
    })))
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
    Json(form): Json<ReclassifyEpisodeForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use crate::services::source::{self, SeriesContext};
    use std::path::Path;

    let series_row = series::get_by_id(&state.db, form.series_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            axum::http::StatusCode::NOT_FOUND,
            format!("series {} not found", form.series_id),
        ))?;

    // Honor user-pinned rows — a re-classify would silently be a no-op
    // against `manual_override = 1` thanks to the COALESCE guard in
    // `update_classification`, and that's more confusing than a hard
    // 409. Caller clears the override first if they want to reclassify.
    let existing_tags = episode_tags::get_for_series(&state.db, form.series_id)
        .await
        .unwrap_or_default();
    if existing_tags
        .get(&form.episode_number)
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

    let disk_files = media::scan_series_folder(&cfg.media_root, &series_row.folder_name);
    let file = disk_files
        .iter()
        .find(|f| {
            // Same season filter as `build_episodes`'s main pass —
            // season 1 or unseasoned only.
            let season_ok = match f.season_number {
                Some(s) => s == 1,
                None => true,
            };
            season_ok && f.episode_number == form.episode_number
        })
        .ok_or((
            axum::http::StatusCode::NOT_FOUND,
            format!(
                "no file on disk for episode {} — import or download it first",
                form.episode_number
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
    let imported_grabs = grabbed_torrents::imported_grabs_for_series(&state.db, form.series_id)
        .await
        .unwrap_or_default();
    let classify_title = imported_grabs
        .iter()
        .find(|(_, eps)| eps.contains(&form.episode_number))
        .map(|(name, _)| name.clone())
        .or_else(|| imported_grabs.first().map(|(n, _)| n.clone()))
        .unwrap_or_else(|| file.filename.clone());

    let is_batch =
        grabbed_torrents::get_is_batch_by_name(&state.db, form.series_id, &classify_title)
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
    let row_exists = existing_tags.contains_key(&form.episode_number);
    if row_exists {
        episode_tags::update_classification(
            &state.db,
            form.series_id,
            form.episode_number,
            &result,
        )
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    } else {
        let file_size = tokio::fs::metadata(&file_path)
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        episode_tags::record_grab(
            &state.db,
            form.series_id,
            form.episode_number,
            &result,
            &classify_title,
            "",
            file_size,
            is_batch,
        )
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        episode_tags::stamp_classification_attempted(
            &state.db,
            form.series_id,
            form.episode_number,
        )
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // `record_grab` hardcodes state='grabbed' for both the tag and
        // history rows. The file is already on disk (checked above), so
        // flip both rows to 'completed' the same way the scan path does
        // in `services/post_processing.rs::scan_for_unclassified`.
        // Without this the UI renders a freshly-reclassified
        // externally-imported episode as download-in-progress until
        // the next 6h sweep corrects the state.
        episode_tags::mark_completed(&state.db, form.series_id, &[form.episode_number])
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let imported_basename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&classify_title)
            .to_string();
        episode_tags::mark_grab_history_completed(
            &state.db,
            form.series_id,
            form.episode_number,
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
            form.series_id, form.episode_number, label
        ),
        &format!(
            "confidence={:.2}, needs_review={}",
            result.confidence, result.needs_review
        ),
    )
    .await;

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
    })))
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
