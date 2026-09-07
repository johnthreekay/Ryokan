//! Recycle bin page + JSON endpoints (#123).
//!
//! `GET /library/recycle` lists every entry grouped by date bucket with
//! Restore / Delete-now actions and an Empty button; the three `POST
//! /api/library/recycle/...` endpoints back those buttons. All filesystem
//! work lives in `services::recycle`; this module only shapes it for the
//! template and maps outcomes to HTTP.

use askama::Template;
use axum::{
    Json,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Response},
};
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;

use crate::AppState;
use crate::models::{config, log::LogCategory};
use crate::services::recycle::{self, RecycleEntry, RecycleKind, RestoreOutcome};
use crate::services::{logger, media};

#[derive(Template)]
#[template(path = "recycle.html")]
struct RecycleTemplate {
    page: String,
    title_language: String,
    /// `recycle_bin_path` is non-empty.
    enabled: bool,
    /// The bin is configured but not writable right now, so deletes are
    /// being refused (live probe, see `services::recycle::check_unwritable`).
    unwritable: bool,
    bin_path: String,
    groups: Vec<DateGroup>,
    total_entries: usize,
    total_size: String,
    /// `(series_id, title)` pairs present in the bin, for the filter.
    series_options: Vec<(i64, String)>,
    filter_series: String,
    filter_from: String,
    filter_to: String,
    /// Non-empty when listing the bin failed (permission, IO).
    load_error: String,
}

struct DateGroup {
    date: String,
    entries: Vec<EntryView>,
}

struct EntryView {
    entry_id: String,
    series_title: String,
    /// `S01E07` for an episode, or "Entire series folder".
    label: String,
    kind_is_folder: bool,
    original_path: String,
    /// Unix seconds; rendered relative by base.js's `data-ts` hook.
    recycled_at: i64,
    size: String,
    size_bytes: u64,
    purge_in: String,
}

#[derive(Deserialize, Default)]
pub struct RecycleQuery {
    #[serde(default)]
    series: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

fn entry_label(entry: &RecycleEntry) -> String {
    match entry.manifest.kind {
        RecycleKind::SeriesFolder => "Entire series folder".to_string(),
        RecycleKind::Episode => {
            let name = std::path::Path::new(&entry.manifest.original_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match media::parse_episode_number(&name.to_lowercase()) {
                Some((season, ep)) => format!("S{:02}E{:02}", season.unwrap_or(1), ep),
                None => name,
            }
        }
    }
}

fn purge_in(date: &str, age_days: i64, today: NaiveDate) -> String {
    if age_days <= 0 {
        return "never (manual only)".to_string();
    }
    let Ok(bucket) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
        return String::new();
    };
    // `purge_old` drops a bucket once `today - bucket > age_days`, i.e.
    // on the first cleanup tick of `bucket + age_days + 1`.
    let purge_day = bucket + Duration::days(age_days + 1);
    let left = (purge_day - today).num_days();
    if left <= 0 {
        "next cleanup".to_string()
    } else if left == 1 {
        "in 1 day".to_string()
    } else {
        format!("in {left} days")
    }
}

pub async fn page(State(state): State<AppState>, Query(q): Query<RecycleQuery>) -> Html<String> {
    let cfg = config::get_config(&state.db).await.ok().flatten();
    let title_language = cfg
        .as_ref()
        .map(|c| c.title_language.clone())
        .unwrap_or_else(|| "english".to_string());
    let bin_path = cfg
        .as_ref()
        .map(|c| c.recycle_bin_path.clone())
        .unwrap_or_default();
    let age_days = cfg.as_ref().map(|c| c.recycle_bin_age_days).unwrap_or(14);
    let enabled = !bin_path.trim().is_empty();

    let (all_entries, load_error) = if enabled {
        match recycle::list_entries(&bin_path).await {
            Ok(v) => (v, String::new()),
            Err(e) => (Vec::new(), e),
        }
    } else {
        (Vec::new(), String::new())
    };

    let mut series_options: Vec<(i64, String)> = all_entries
        .iter()
        .filter_map(|e| {
            e.manifest
                .series_id
                .map(|id| (id, e.manifest.series_title.clone()))
        })
        .collect();
    series_options.sort_by_key(|(_, title)| title.to_lowercase());
    series_options.dedup_by_key(|(id, _)| *id);

    let filter_series_id: Option<i64> = q.series.trim().parse().ok();
    let from = q.from.trim().to_string();
    let to = q.to.trim().to_string();
    let today = Utc::now().date_naive();

    let mut groups: Vec<DateGroup> = Vec::new();
    let mut total_entries = 0usize;
    let mut total_bytes = 0u64;
    for entry in &all_entries {
        if let Some(sid) = filter_series_id
            && entry.manifest.series_id != Some(sid)
        {
            continue;
        }
        if !from.is_empty() && entry.date.as_str() < from.as_str() {
            continue;
        }
        if !to.is_empty() && entry.date.as_str() > to.as_str() {
            continue;
        }
        total_entries += 1;
        total_bytes += entry.manifest.size_bytes;
        let view = EntryView {
            entry_id: entry.entry_id.clone(),
            series_title: if entry.manifest.series_title.is_empty() {
                "(unknown series)".to_string()
            } else {
                entry.manifest.series_title.clone()
            },
            label: entry_label(entry),
            kind_is_folder: entry.manifest.kind == RecycleKind::SeriesFolder,
            original_path: entry.manifest.original_path.clone(),
            recycled_at: entry.manifest.recycled_at,
            size: recycle::human_bytes(entry.manifest.size_bytes),
            size_bytes: entry.manifest.size_bytes,
            purge_in: purge_in(&entry.date, age_days, today),
        };
        match groups.last_mut() {
            Some(g) if g.date == entry.date => g.entries.push(view),
            _ => groups.push(DateGroup {
                date: entry.date.clone(),
                entries: vec![view],
            }),
        }
    }
    // Newest bucket first; entries inside a bucket are already newest
    // first from `list_entries`.
    groups.sort_by(|a, b| b.date.cmp(&a.date));

    let template = RecycleTemplate {
        page: "library".to_string(),
        title_language,
        enabled,
        unwritable: recycle::check_unwritable(&bin_path).await,
        bin_path,
        groups,
        total_entries,
        total_size: recycle::human_bytes(total_bytes),
        series_options,
        filter_series: q.series.trim().to_string(),
        filter_from: from,
        filter_to: to,
        load_error,
    };
    Html(template.render().unwrap_or_default())
}

fn json_err(status: axum::http::StatusCode, msg: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "ok": false, "message": msg })),
    )
        .into_response()
}

fn status_for(err: &str) -> axum::http::StatusCode {
    if err.contains("not found") {
        axum::http::StatusCode::NOT_FOUND
    } else if err.contains("invalid recycle entry id") {
        axum::http::StatusCode::BAD_REQUEST
    } else {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// `(recycle_bin_path, media_root)` or the 400 to return when no bin is
/// configured.
async fn bin_path(state: &AppState) -> Result<(String, String), Box<Response>> {
    let (bin, media_root) = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| (c.recycle_bin_path, c.media_root))
        .unwrap_or_default();
    if bin.trim().is_empty() {
        return Err(Box::new(json_err(
            axum::http::StatusCode::BAD_REQUEST,
            "Recycle bin is not configured",
        )));
    }
    Ok((bin, media_root))
}

/// After an episode file is back on disk, undo the delete's DB side:
/// the quality tag was cleared and the grab history flipped to
/// `removed`, so the row would keep reading "removed" with the file
/// sitting right there. Re-running the reclassify core re-tags the file
/// and records a fresh `completed` history row; a pinned (manual
/// override) row just gets its state back. Best-effort: the restore
/// itself already succeeded, so failures here only log.
async fn retag_restored_episode(state: &AppState, series_id: i64, final_path: &std::path::Path) {
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some((_, episode_number)) = media::parse_episode_number(&name.to_lowercase()) else {
        return;
    };
    match crate::handlers::library::crud::reclassify_on_disk_episode(
        state,
        series_id,
        episode_number,
    )
    .await
    {
        Ok(_) => {}
        Err((axum::http::StatusCode::CONFLICT, _)) => {
            // Pinned via manual override: the tag row survived the delete
            // with a blank state. Restore the state without touching the
            // pin.
            let _ = crate::models::episode_tags::mark_completed(
                &state.db,
                series_id,
                &[episode_number],
            )
            .await;
        }
        Err((_, e)) => {
            logger::warn(
                &state.db,
                LogCategory::Library,
                &format!(
                    "Restored episode {} but could not re-tag it; run Reclassify on the episode",
                    episode_number
                ),
                &e,
            )
            .await;
        }
    }
}

/// Best-effort Jellyfin rescan so a restored folder or file shows up
/// without waiting for the next library refresh.
async fn nudge_jellyfin(state: &AppState) {
    let jelly = state.jellyfin.read().await.clone();
    if let Some(j) = jelly {
        let _ = j.refresh_library().await;
    }
}

/// `POST /api/library/recycle/{entry_id}/restore`
#[utoipa::path(
    post,
    path = "/api/library/recycle/{entry_id}/restore",
    tag = "Library",
    summary = "Restore a recycle bin entry",
    description = "Moves the entry's files back to where they were deleted from and re-tags the episode. Fails if the original location is now occupied.",
    params(("entry_id" = String, Path, description = "Recycle bin entry id (8 hex chars)")),
    responses(
        (status = 200, description = "Restored", body = serde_json::Value),
        (status = 404, description = "No such entry"),
        (status = 409, description = "Destination already exists"),
    ),
)]
pub async fn restore(State(state): State<AppState>, Path(entry_id): Path<String>) -> Response {
    let (bin, media_root) = match bin_path(&state).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    // Read the manifest before the move so the re-tag step knows which
    // series the file belongs to once it's back.
    let series_id = match recycle::find_entry(&bin, &entry_id).await {
        Ok(Some(entry)) if entry.manifest.kind == RecycleKind::Episode => entry.manifest.series_id,
        Ok(_) => None,
        Err(e) => return json_err(status_for(&e), &e),
    };
    match recycle::restore(&bin, &entry_id, &media_root).await {
        Ok(RestoreOutcome::Restored { final_path }) => {
            if let Some(series_id) = series_id {
                retag_restored_episode(&state, series_id, &final_path).await;
            }
            logger::info(
                &state.db,
                LogCategory::Library,
                "Restored from recycle bin",
                &format!("entry={} to={}", entry_id, final_path.display()),
            )
            .await;
            nudge_jellyfin(&state).await;
            let name = final_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| final_path.display().to_string());
            Json(serde_json::json!({
                "ok": true,
                "message": format!("Restored {}", name),
            }))
            .into_response()
        }
        Ok(RestoreOutcome::ConflictAtTarget) => json_err(
            axum::http::StatusCode::CONFLICT,
            "Something already exists at the original location. Move or delete it first.",
        ),
        Ok(RestoreOutcome::OriginalLocationMissing) => json_err(
            axum::http::StatusCode::CONFLICT,
            "The original folder no longer exists. Re-add the series first, then restore.",
        ),
        Ok(RestoreOutcome::OutsideMediaRoot) => json_err(
            axum::http::StatusCode::CONFLICT,
            "The original location is outside the current media root. Move it back by hand.",
        ),
        Err(e) => json_err(status_for(&e), &e),
    }
}

/// `POST /api/library/recycle/{entry_id}/purge`: permanently delete one
/// entry ("Delete now").
#[utoipa::path(
    post,
    path = "/api/library/recycle/{entry_id}/purge",
    tag = "Library",
    summary = "Permanently delete a recycle bin entry",
    params(("entry_id" = String, Path, description = "Recycle bin entry id (8 hex chars)")),
    responses(
        (status = 200, description = "Deleted; reports bytes freed", body = serde_json::Value),
        (status = 404, description = "No such entry"),
    ),
)]
pub async fn purge_entry(State(state): State<AppState>, Path(entry_id): Path<String>) -> Response {
    let (bin, _) = match bin_path(&state).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    match recycle::delete_entry(&bin, &entry_id).await {
        Ok(bytes) => {
            logger::info(
                &state.db,
                LogCategory::Library,
                "Deleted recycle bin entry permanently",
                &format!("entry={} bytes={}", entry_id, bytes),
            )
            .await;
            Json(serde_json::json!({
                "ok": true,
                "bytes": bytes,
                "message": format!("Deleted permanently ({})", recycle::human_bytes(bytes)),
            }))
            .into_response()
        }
        Err(e) => json_err(status_for(&e), &e),
    }
}

/// `POST /api/library/recycle/empty`: permanently delete every entry.
#[utoipa::path(
    post,
    path = "/api/library/recycle/empty",
    tag = "Library",
    summary = "Empty the recycle bin",
    responses(
        (status = 200, description = "Emptied; reports entries and bytes freed", body = serde_json::Value),
        (status = 400, description = "No recycle bin configured"),
    ),
)]
pub async fn empty(State(state): State<AppState>) -> Response {
    let (bin, _) = match bin_path(&state).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    match recycle::empty(&bin).await {
        Ok(report) => {
            logger::info(
                &state.db,
                LogCategory::Library,
                "Emptied recycle bin",
                &format!(
                    "entries={} bytes={} date_dirs={}",
                    report.entries, report.bytes, report.date_dirs
                ),
            )
            .await;
            Json(serde_json::json!({
                "ok": true,
                "entries": report.entries,
                "bytes": report.bytes,
                "message": format!(
                    "Recycle bin emptied ({} entr{}, {})",
                    report.entries,
                    if report.entries == 1 { "y" } else { "ies" },
                    recycle::human_bytes(report.bytes)
                ),
            }))
            .into_response()
        }
        Err(e) => json_err(status_for(&e), &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use axum::body::to_bytes;

    async fn body_string(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// End-to-end through the handlers: configure a bin, recycle a real
    /// file, render the page, restore via the endpoint, then purge an
    /// unknown id. Guards the Askama render path (`unwrap_or_default`
    /// would otherwise turn a runtime template failure into a silent
    /// blank page).
    #[tokio::test]
    async fn page_lists_entries_and_endpoints_round_trip() {
        let db = in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("recycle");
        let season = tmp.path().join("media").join("Show").join("Season 01");
        std::fs::create_dir_all(&season).unwrap();
        let video = season.join("Show - S01E07.mkv");
        std::fs::write(&video, b"bytes").unwrap();
        config::save_config(
            &db,
            &config::Config {
                media_root: tmp.path().join("media").display().to_string(),
                recycle_bin_path: bin.display().to_string(),
                recycle_bin_age_days: 30,
                ..config::Config::default()
            },
        )
        .await
        .unwrap();
        let outcome = recycle::recycle(
            &db,
            bin.to_str().unwrap(),
            RecycleKind::Episode,
            Some(9),
            "Show",
            &video,
        )
        .await
        .unwrap();
        let recycle::RecycleOutcome::Recycled { entry_id } = outcome else {
            panic!("expected Recycled");
        };

        let state = build_test_app_state(db.clone(), None);
        let html = page(State(state.clone()), Query(RecycleQuery::default()))
            .await
            .0;
        assert!(html.contains("<h2>Recycle Bin</h2>"), "page must render");
        assert!(
            html.contains(&format!("recycle-{entry_id}")),
            "entry row missing"
        );
        assert!(html.contains("S01E07"));
        assert!(html.contains("recycle-empty-btn"));
        assert!(!html.contains("Recycle bin is off"));

        // Series filter that matches nothing hides the row but keeps the page.
        let html = page(
            State(state.clone()),
            Query(RecycleQuery {
                series: "12345".into(),
                ..Default::default()
            }),
        )
        .await
        .0;
        assert!(html.contains("Nothing matches these filters"));
        assert!(!html.contains(&format!("recycle-{entry_id}")));

        // Restore through the endpoint puts the file back.
        let resp = restore(State(state.clone()), Path(entry_id.clone())).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert!(video.is_file());
        let resp = restore(State(state.clone()), Path(entry_id.clone())).await;
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        let resp = purge_entry(State(state.clone()), Path("zz".into())).await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        let resp = empty(State(state.clone())).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("\"ok\":true"));
    }

    #[tokio::test]
    async fn page_without_bin_shows_disabled_banner_and_endpoints_refuse() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let html = page(State(state.clone()), Query(RecycleQuery::default()))
            .await
            .0;
        assert!(html.contains("Recycle bin is off"));
        let resp = empty(State(state)).await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn purge_in_copy_matches_sweep_semantics() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 28).unwrap();
        assert_eq!(purge_in("2026-08-28", 30, today), "in 31 days");
        assert_eq!(purge_in("2026-07-29", 30, today), "in 1 day");
        // 31 days old: the next hourly sweep drops it.
        assert_eq!(purge_in("2026-07-28", 30, today), "next cleanup");
        assert_eq!(purge_in("2026-01-01", 30, today), "next cleanup");
        assert_eq!(purge_in("2026-08-28", 0, today), "never (manual only)");
        assert_eq!(purge_in("garbage", 30, today), "");
    }

    #[test]
    fn entry_label_parses_episode_from_filename() {
        let mk = |kind: RecycleKind, path: &str| RecycleEntry {
            entry_id: "aaaaaaaa".into(),
            date: "2026-08-28".into(),
            dir: std::path::PathBuf::from("/bin/2026-08-28/aaaaaaaa"),
            manifest: recycle::RecycleManifest {
                kind,
                series_id: Some(1),
                series_title: "Show".into(),
                original_path: path.into(),
                recycled_at: 0,
                size_bytes: 0,
                files: vec![],
            },
        };
        assert_eq!(
            entry_label(&mk(
                RecycleKind::Episode,
                "/m/Show/Season 01/Show - S01E07.mkv"
            )),
            "S01E07"
        );
        assert_eq!(
            entry_label(&mk(
                RecycleKind::Episode,
                "/m/Show/Season 01/unparseable.mkv"
            )),
            "unparseable.mkv"
        );
        assert_eq!(
            entry_label(&mk(RecycleKind::SeriesFolder, "/m/Show")),
            "Entire series folder"
        );
    }
}
