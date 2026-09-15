//! Bulk library operations (issue #125).
//!
//! Per-series actions Ryokan supports today are "click a thing per row,"
//! which scales poorly past two series. This module ships the JSON-API
//! surface for bulk changes, starting with bulk monitor; subsequent PRs
//! layer bulk re-search, bulk delete, bulk finished-mode, and bulk
//! upgrades on top of the same `BulkOutcome` shape.
//!
//! ## Per-series failure isolation
//!
//! Every endpoint returns a structured outcome:
//!
//! ```json
//! { "succeeded": [1, 2, 5, 7], "failed": [{ "series_id": 9, "reason": "..." }] }
//! ```
//!
//! A per-series failure (DB error on series 9, missing folder, download
//! client unreachable) is captured and the loop continues. All-or-nothing
//! semantics would be wrong here — a single broken-folder series shouldn't
//! block 49 valid operations from succeeding. The frontend renders a
//! partial-failure toast + click-for-detail modal so the user sees which
//! IDs failed and why.
//!
//! ## Empty selection is a 200 no-op
//!
//! Defense-in-depth: a request with an empty `series_ids` array returns
//! 200 with empty `succeeded` / `failed` arrays. The toolbar UI is
//! supposed to be hidden when nothing's selected, but a race-during-
//! deselect or a JS bug could still send `{ "series_ids": [] }`. Don't
//! 400 — a benign no-op is the right shape, and validates only that the
//! request body deserialized.
//!
//! ## Audit logging
//!
//! After the per-series loop completes, each endpoint writes one
//! batch-summary `LogCategory::Library` entry: succeeded / failed counts
//! plus the failed IDs inline. Per-series log lines would flood System
//! → Logs at 200-item bulk re-search; the single summary is the
//! auditable "did the bulk action I did yesterday touch series X?"
//! answer without drowning the filter.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, series};
use crate::services::logger;

/// Per-series outcome envelope returned from synchronous bulk endpoints.
/// Async endpoints (bulk re-search) return a `ProgressRegistry` job ID
/// instead and surface this same shape on the progress-completion event.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BulkOutcome {
    pub succeeded: Vec<i64>,
    pub failed: Vec<BulkFailure>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BulkFailure {
    pub series_id: i64,
    pub reason: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkMonitorRequest {
    pub series_ids: Vec<i64>,
    /// Monitor mode value (`all` / `future` / `missing` / `existing` /
    /// `none`) or the [`MONITOR_MODE_SYNC_SENTINEL`] string `"sync"`.
    /// Picking the sentinel clears the per-series manual-override flag
    /// so the next AL/MAL sync tick computes the mode; any other value
    /// pins the mode and sets the manual-override flag.
    ///
    /// [`MONITOR_MODE_SYNC_SENTINEL`]: super::crud::MONITOR_MODE_SYNC_SENTINEL
    pub mode: String,
}

/// `POST /api/library/bulk/monitor` — apply the same monitor-mode change
/// to a list of series. Reuses `crud::apply_monitor_mode` per-series so
/// the sentinel-clear and pin-mode branches stay in lockstep with the
/// per-series handler.
#[utoipa::path(
    post,
    path = "/api/library/bulk/monitor",
    tag = "Library",
    summary = "Bulk-set monitor mode",
    request_body = BulkMonitorRequest,
    responses(
        (status = 200, description = "Outcome envelope; check `failed` for per-series errors", body = BulkOutcome),
    ),
)]
pub async fn bulk_monitor(
    State(state): State<AppState>,
    Json(req): Json<BulkMonitorRequest>,
) -> Json<BulkOutcome> {
    if req.series_ids.is_empty() {
        return Json(BulkOutcome {
            succeeded: vec![],
            failed: vec![],
        });
    }

    let mut succeeded = Vec::with_capacity(req.series_ids.len());
    let mut failed = Vec::new();
    for series_id in &req.series_ids {
        match super::crud::apply_monitor_mode(&state.db, *series_id, &req.mode).await {
            Ok(()) => succeeded.push(*series_id),
            Err(reason) => failed.push(BulkFailure {
                series_id: *series_id,
                reason,
            }),
        }
    }

    let failed_ids: Vec<i64> = failed.iter().map(|f| f.series_id).collect();
    // Log the canonical mode so the trail matches per-row's
    // `set_monitoring` log line — that handler logs
    // `MonitorMode::from_str(...).as_str()` (lowercase known value).
    // Without this both `mode=ALL` and `mode=all` could land in the
    // log depending on which path the user took, which makes the
    // System → Logs filter brittle. Sentinel passes through verbatim
    // because `MonitorMode::from_str` would coerce it to `future`.
    let canonical_mode: &str = if req.mode == super::crud::MONITOR_MODE_SYNC_SENTINEL {
        super::crud::MONITOR_MODE_SYNC_SENTINEL
    } else {
        crate::models::monitoring::MonitorMode::from_str(&req.mode).as_str()
    };
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "Bulk monitor change: {} succeeded, {} failed",
            succeeded.len(),
            failed.len()
        ),
        &format!(
            "action=monitor_set mode={} failed_ids={:?}",
            canonical_mode, failed_ids
        ),
    )
    .await;

    Json(BulkOutcome { succeeded, failed })
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkDeleteRequest {
    pub series_ids: Vec<i64>,
    /// When true, also walks `grabbed_torrents` for each series and
    /// asks the resolved download client to drop the torrent + its
    /// files, then recursively removes the on-disk folder under
    /// `media_root/<folder_name>`. When false, only the database
    /// rows for the series are removed (cascade + `rss_seen`
    /// NULL-out via [`series::remove`]); files stay on disk and
    /// active torrents stay in their download clients.
    pub delete_files: bool,
    /// Keep each series off the AniList / MAL watch-list sync after
    /// the removal (Sonarr's "add list exclusion"). Default false.
    #[serde(default)]
    pub add_exclusion: bool,
}

/// `POST /api/library/bulk/delete` — remove a list of series from the
/// library. Per-series cleanup runs in a loop; failures don't abort
/// the batch.
///
/// With a recycle bin configured (#123), `delete_files: true` moves
/// each series folder into its own recycle entry via
/// `cleanup_series_files`; with no bin it is a permanent unlink and
/// the confirmation modal says so.
#[utoipa::path(
    post,
    path = "/api/library/bulk/delete",
    tag = "Library",
    summary = "Bulk-remove series from library",
    request_body = BulkDeleteRequest,
    responses(
        (status = 200, description = "Outcome envelope; check `failed` for per-series errors", body = BulkOutcome),
    ),
)]
pub async fn bulk_delete(
    State(state): State<AppState>,
    Json(req): Json<BulkDeleteRequest>,
) -> Json<BulkOutcome> {
    if req.series_ids.is_empty() {
        return Json(BulkOutcome {
            succeeded: vec![],
            failed: vec![],
        });
    }

    // Resolve media_root once at the top — bulk delete pays a single
    // config fetch instead of N when delete_files is true. Returns
    // None when config isn't loaded (shouldn't happen post-setup; we
    // guard rather than crash).
    let (media_root, recycle_bin_path): (Option<String>, String) = if req.delete_files {
        match config::get_config(&state.db).await.ok().flatten() {
            Some(c) => (Some(c.media_root), c.recycle_bin_path),
            None => (None, String::new()),
        }
    } else {
        (None, String::new())
    };

    let mut succeeded = Vec::with_capacity(req.series_ids.len());
    let mut failed = Vec::new();
    for series_id in &req.series_ids {
        // Read before the row goes, written only after it is gone.
        let exclusion = if req.add_exclusion {
            super::crud::sync_exclusion_snapshot(&state.db, *series_id).await
        } else {
            None
        };
        match delete_one_series(
            &state,
            *series_id,
            req.delete_files,
            media_root.as_deref(),
            &recycle_bin_path,
        )
        .await
        {
            Ok(()) => {
                if let Some(snapshot) = exclusion {
                    super::crud::record_sync_exclusion(&state.db, snapshot).await;
                }
                succeeded.push(*series_id)
            }
            Err(reason) => failed.push(BulkFailure {
                series_id: *series_id,
                reason,
            }),
        }
    }

    let failed_ids: Vec<i64> = failed.iter().map(|f| f.series_id).collect();
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "Bulk delete: {} succeeded, {} failed",
            succeeded.len(),
            failed.len()
        ),
        &format!(
            "action=delete delete_files={} failed_ids={:?}",
            req.delete_files, failed_ids
        ),
    )
    .await;

    Json(BulkOutcome { succeeded, failed })
}

/// Per-series delete worker for [`bulk_delete`]. Returns `Result<(),
/// String>` so the loop can collect failures into `BulkOutcome.failed`
/// without aborting the batch.
///
/// Order of operations: torrent-client + folder cleanup via the
/// shared [`super::cleanup::cleanup_series_files`] helper (which the
/// per-row [`super::crud::remove_series`] handler also uses), then
/// `series::remove` for the DB cascade. If the DB cascade ran first,
/// the helper's `grabbed_torrents` lookup would return nothing (FK
/// cascade dropped the rows) and active torrents would orphan.
///
/// Partial cleanup failures (folder refused / error, per-grab client
/// errors) are surfaced as a `BulkFailure.reason` rather than counted
/// as success — user needs to know when their `delete_files=true`
/// request didn't actually remove the files.
async fn delete_one_series(
    state: &AppState,
    series_id: i64,
    delete_files: bool,
    media_root: Option<&str>,
    recycle_bin_path: &str,
) -> Result<(), String> {
    if delete_files {
        // Look up the series row for `folder_name` (and to detect
        // stale-tab IDs where the row no longer exists). Per-row's
        // handler does the same lookup; bulk has to do it too
        // because the helper takes `folder_name: &str`.
        let tracked = series::get_by_id(&state.db, series_id)
            .await
            .map_err(|e| format!("lookup: {e}"))?;
        let folder_name = tracked
            .as_ref()
            .map(|t| t.folder_name.as_str())
            .unwrap_or("");
        let series_title = tracked.as_ref().map(|t| t.title.as_str()).unwrap_or("");

        let report = super::cleanup::cleanup_series_files(
            state,
            series_id,
            folder_name,
            series_title,
            media_root,
            recycle_bin_path,
        )
        .await?;
        if !report.is_clean() {
            // Don't `?` — DB cascade still runs even on partial
            // cleanup so the library entry disappears (matches
            // per-row behavior; user asked for delete and we
            // honor that). Surface the partial-failure detail so
            // the user sees what didn't get cleaned. If `series::remove`
            // *also* fails, concat both — the upstream
            // traversal-refused / seed-rule signal is more interesting
            // than the bare sqlx error and should land in the failure
            // modal regardless.
            let reason = report.partial_failure_reason();
            if let Err(e) = series::remove(&state.db, series_id).await {
                return Err(format!("{reason}; db remove after partial cleanup: {e}"));
            }
            return Err(reason);
        }
    }

    // DB cascade. `series::remove` is the canonical path; it does
    // the rss_seen NULL-out + every other FK-cascading delete.
    series::remove(&state.db, series_id)
        .await
        .map_err(|e| format!("db remove: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;
    use sqlx::SqlitePool;

    /// Insert a minimal series row sufficient for the monitor-mode
    /// write paths to succeed. Returns the inserted id.
    async fn insert_series(pool: &SqlitePool, anilist_id: i64, title: &str) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO series (anilist_id, title, monitor_mode, monitor_mode_manual_override)
             VALUES (?, ?, 'future', 0)
             RETURNING id",
        )
        .bind(anilist_id)
        .bind(title)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0
    }

    /// Read back the (mode, override_flag) tuple for assertions.
    async fn read_monitor_state(pool: &SqlitePool, series_id: i64) -> (String, bool) {
        let row: (String, bool) = sqlx::query_as(
            "SELECT monitor_mode, monitor_mode_manual_override FROM series WHERE id = ?",
        )
        .bind(series_id)
        .fetch_one(pool)
        .await
        .unwrap();
        row
    }

    #[tokio::test]
    async fn bulk_monitor_applies_mode_to_every_id() {
        let pool = in_memory_pool().await;
        let a = insert_series(&pool, 1, "A").await;
        let b = insert_series(&pool, 2, "B").await;
        let c = insert_series(&pool, 3, "C").await;

        let req = BulkMonitorRequest {
            series_ids: vec![a, b, c],
            mode: "all".to_string(),
        };
        let state = crate::test_support::build_test_app_state(pool.clone(), None);
        let Json(outcome) = bulk_monitor(axum::extract::State(state), Json(req)).await;

        assert_eq!(outcome.succeeded.len(), 3);
        assert!(outcome.failed.is_empty());
        for id in [a, b, c] {
            let (mode, override_flag) = read_monitor_state(&pool, id).await;
            assert_eq!(mode, "all");
            assert!(
                override_flag,
                "explicit-mode bulk should set the manual-override flag"
            );
        }
    }

    #[tokio::test]
    async fn bulk_monitor_sync_sentinel_clears_override_without_touching_mode() {
        // Sentinel posture: clear `monitor_mode_manual_override`,
        // leave `monitor_mode` alone (next sync tick fixes it).
        // Tested separately because this branch silently drives
        // different behavior than an explicit mode change.
        let pool = in_memory_pool().await;
        let id = insert_series(&pool, 1, "A").await;
        // Pre-stamp it with an override so the sentinel has something
        // to clear.
        sqlx::query(
            "UPDATE series SET monitor_mode = 'all', monitor_mode_manual_override = 1 WHERE id = ?",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let req = BulkMonitorRequest {
            series_ids: vec![id],
            mode: super::super::crud::MONITOR_MODE_SYNC_SENTINEL.to_string(),
        };
        let state = crate::test_support::build_test_app_state(pool.clone(), None);
        let Json(outcome) = bulk_monitor(axum::extract::State(state), Json(req)).await;

        assert_eq!(outcome.succeeded, vec![id]);
        assert!(outcome.failed.is_empty());
        let (mode, override_flag) = read_monitor_state(&pool, id).await;
        assert_eq!(mode, "all", "sentinel should NOT change the mode");
        assert!(!override_flag, "sentinel should clear the override flag");
    }

    #[tokio::test]
    async fn bulk_monitor_isolates_per_series_failures() {
        // Mix a real id with a non-existent one. After PR #164's
        // preflight existence check, the missing id lands in
        // `failed` (not silently swallowed), so the real id still
        // writes successfully and the loop-continues-on-error
        // contract is pinned in BOTH directions: real id in
        // succeeded, missing id in failed.
        let pool = in_memory_pool().await;
        let a = insert_series(&pool, 1, "A").await;
        let req = BulkMonitorRequest {
            series_ids: vec![a, 99999],
            mode: "missing".to_string(),
        };
        let state = crate::test_support::build_test_app_state(pool.clone(), None);
        let Json(outcome) = bulk_monitor(axum::extract::State(state), Json(req)).await;
        assert!(outcome.succeeded.contains(&a));
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].series_id, 99999);
        assert!(
            outcome.failed[0].reason.contains("no longer exists"),
            "expected stale-id failure reason, got: {}",
            outcome.failed[0].reason
        );
        // Real id wrote successfully.
        let (mode, _) = read_monitor_state(&pool, a).await;
        assert_eq!(mode, "missing");
    }

    #[tokio::test]
    async fn bulk_monitor_empty_selection_is_200_noop() {
        let pool = in_memory_pool().await;
        let req = BulkMonitorRequest {
            series_ids: vec![],
            mode: "all".to_string(),
        };
        let state = crate::test_support::build_test_app_state(pool, None);
        let Json(outcome) = bulk_monitor(axum::extract::State(state), Json(req)).await;
        assert!(outcome.succeeded.is_empty());
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn bulk_delete_removes_db_rows_when_delete_files_false() {
        // Covers the "remove from library only" path: rows go from
        // `series` (and cascade across the FK graph), files stay
        // on disk. delete_files=false also skips the
        // grabbed_torrents walk, so this test doesn't need a
        // download client mock.
        let pool = in_memory_pool().await;
        let a = insert_series(&pool, 1, "A").await;
        let b = insert_series(&pool, 2, "B").await;

        let req = BulkDeleteRequest {
            series_ids: vec![a, b],
            delete_files: false,
            add_exclusion: false,
        };
        let state = crate::test_support::build_test_app_state(pool.clone(), None);
        let Json(outcome) = bulk_delete(axum::extract::State(state), Json(req)).await;

        assert_eq!(outcome.succeeded.len(), 2);
        assert!(outcome.failed.is_empty());
        for id in [a, b] {
            let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM series WHERE id = ?")
                .bind(id)
                .fetch_optional(&pool)
                .await
                .unwrap();
            assert!(exists.is_none(), "series {id} should be DB-removed");
        }
    }

    #[tokio::test]
    async fn bulk_delete_empty_selection_is_200_noop() {
        let pool = in_memory_pool().await;
        let req = BulkDeleteRequest {
            series_ids: vec![],
            delete_files: false,
            add_exclusion: false,
        };
        let state = crate::test_support::build_test_app_state(pool, None);
        let Json(outcome) = bulk_delete(axum::extract::State(state), Json(req)).await;
        assert!(outcome.succeeded.is_empty());
        assert!(outcome.failed.is_empty());
    }

    /// Persist a `Config` row with the given media_root for the
    /// path-traversal test. The bulk handler reads `cfg.media_root`
    /// once up front when `delete_files=true` and feeds it into
    /// `cleanup_series_files` for the canonicalize+starts_with guard.
    async fn save_media_root(db: &SqlitePool, media_root: &str) {
        let cfg = crate::models::config::Config {
            media_root: media_root.to_string(),
            ..Default::default()
        };
        crate::models::config::save_config(db, &cfg)
            .await
            .expect("save config");
    }

    /// CVE-shape: pin the `series_canon.starts_with(&media_root_canon)`
    /// guard for the bulk-delete path. PR #164 review flagged that the
    /// previous `bulk::delete_one_series` skipped canonicalization
    /// entirely — a corrupted `folder_name = "../escape"` would have
    /// `remove_dir_all`'d an arbitrary directory. The shared
    /// `cleanup_series_files` helper restores the guard; this test
    /// pins it for bulk so a future regression on either the helper
    /// or the bulk caller surfaces immediately.
    #[tokio::test]
    async fn bulk_delete_refuses_traversal_when_resolved_path_escapes_media_root() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let media_root = tmp.path().join("media");
        let escape = tmp.path().join("escape");
        std::fs::create_dir(&media_root).unwrap();
        std::fs::create_dir(&escape).unwrap();
        let sentinel = escape.join("sentinel.txt");
        std::fs::write(&sentinel, b"do not delete").unwrap();

        let pool = in_memory_pool().await;
        let series_id = insert_series(&pool, 1001, "Show").await;
        // Override folder_name to the traversal payload — bypassing
        // `set_folder`'s `sanitize_folder_name` validator that would
        // normally reject this. Models a row predating the validator
        // or one written by a manual SQL edit.
        sqlx::query("UPDATE series SET folder_name = ? WHERE id = ?")
            .bind("../escape")
            .bind(series_id)
            .execute(&pool)
            .await
            .unwrap();
        save_media_root(&pool, media_root.to_str().unwrap()).await;
        let state = crate::test_support::build_test_app_state(pool.clone(), None);

        let req = BulkDeleteRequest {
            series_ids: vec![series_id],
            delete_files: true,
            add_exclusion: false,
        };
        let Json(outcome) = bulk_delete(axum::extract::State(state), Json(req)).await;

        // Series is still removed from the library DB (user asked to
        // delete; we honor that), but the partial-cleanup signal
        // surfaces as a BulkFailure so the user knows files weren't
        // touched.
        assert_eq!(outcome.succeeded.len(), 0);
        assert_eq!(outcome.failed.len(), 1);
        let failure = &outcome.failed[0];
        assert_eq!(failure.series_id, series_id);
        assert!(
            failure.reason.contains("refused") || failure.reason.contains("outside media root"),
            "expected traversal-refused reason, got: {}",
            failure.reason
        );

        // Critical: the escape target survived. If the traversal guard
        // ever flips, this assertion catches it.
        assert!(sentinel.exists(), "escape target sentinel must survive");
        assert!(escape.exists(), "escape dir must survive");
        assert!(media_root.exists(), "media_root must survive");

        // DB row is gone (delete still proceeds despite the partial
        // cleanup; matches per-row's behavior of going through with
        // the irreversible step).
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    /// Issue #28 — pin the `respects_seed_rules` skip-client-delete
    /// branch for the bulk path. Previously `bulk::delete_one_series`
    /// unconditionally called `client.delete(hash, true)`, which would
    /// silently violate PT ratio policy for every grab on a 50-series
    /// bulk delete. The shared helper now honors the flag; this test
    /// pins it.
    #[tokio::test]
    async fn bulk_delete_honors_seed_rules() {
        use crate::services::download_client::{
            AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
        };
        use async_trait::async_trait;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct RecordingClient {
            deletes: Mutex<Vec<String>>,
        }

        #[async_trait]
        impl DownloadClient for RecordingClient {
            async fn test(&self) -> Result<String, String> {
                Ok("mock".into())
            }
            async fn add_torrent(&self, _url: &str, _hash: &str) -> Result<AddOutcome, String> {
                Ok(AddOutcome::Added)
            }
            async fn add_torrent_with_file_filter(
                &self,
                _url: &str,
                _hash: &str,
                _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
            ) -> Result<SelectiveOutcome, String> {
                Ok(SelectiveOutcome::FullDownload)
            }
            async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
                Ok(vec![])
            }
            async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
                Ok(vec![])
            }
            async fn pause(&self, _hash: &str) -> Result<(), String> {
                Ok(())
            }
            async fn resume(&self, _hash: &str) -> Result<(), String> {
                Ok(())
            }
            async fn delete(&self, hash: &str, _delete_files: bool) -> Result<(), String> {
                self.deletes.lock().unwrap().push(hash.to_string());
                Ok(())
            }
            async fn set_file_wanted(
                &self,
                _hash: &str,
                _files: &[usize],
                _wanted: bool,
            ) -> Result<(), String> {
                Ok(())
            }
            fn sonarr_impl_name(&self) -> &'static str {
                "QBittorrent"
            }
            fn protocol(&self) -> &'static str {
                "torrent"
            }
        }

        let pool = in_memory_pool().await;
        let series_id = insert_series(&pool, 2001, "Show").await;
        // Two grabs: one with the seed-rule flag set, one without. The
        // flagged hash must NOT reach client.delete; the unflagged
        // hash must.
        let kept_hash = "kept-hash-aaa";
        let removed_hash = "removed-hash-bbb";
        crate::test_support::seed_grabbed_torrent(
            &pool,
            series_id,
            kept_hash,
            "kept.torrent",
            &[1],
        )
        .await;
        crate::test_support::seed_grabbed_torrent(
            &pool,
            series_id,
            removed_hash,
            "removed.torrent",
            &[2],
        )
        .await;
        sqlx::query("UPDATE grabbed_torrents SET respect_seed_rules = 1 WHERE hash = ?")
            .bind(kept_hash)
            .execute(&pool)
            .await
            .unwrap();

        let recorder = Arc::new(RecordingClient::default());
        let client: Arc<dyn DownloadClient> = recorder.clone();
        let state = crate::test_support::build_test_app_state(pool.clone(), Some(client));

        let req = BulkDeleteRequest {
            series_ids: vec![series_id],
            delete_files: true,
            add_exclusion: false,
        };
        let Json(_outcome) = bulk_delete(axum::extract::State(state), Json(req)).await;

        let observed = recorder.deletes.lock().unwrap();
        assert_eq!(
            observed.len(),
            1,
            "exactly one client.delete expected; got {:?}",
            observed
        );
        assert_eq!(
            observed[0], removed_hash,
            "non-seed-rule hash should be the only one deleted"
        );
        assert!(
            !observed.contains(&kept_hash.to_string()),
            "seed-rule-flagged hash must NOT reach client.delete"
        );
    }
}
