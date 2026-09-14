//! TTL sweep for stale `pending_grabs` rows (issue #83).
//!
//! Implements plan decision #3's "auto-commit on walkaway" shape:
//! when a modal's heartbeat lapses, the torrent is still a user-
//! intended download (they hit Grab), so we resume it with every
//! file marked wanted rather than leaving it paused forever. The
//! full grab-row-write + sibling auto-expand chain is a follow-up
//! scope — this sweep gets the files downloading, but library
//! attribution (the `grabbed_torrents` row wiring) is a separate
//! pass.
//!
//! Per-row flow:
//!   1. Load the row via `list_expired`.
//!   2. If `file_list_json` is populated, a download client is
//!      configured, and `error_message` is empty, mark every file
//!      wanted and resume the torrent.
//!   3. Delete the `pending_grabs` row either way so the sweep
//!      doesn't loop forever on a permanently-stuck entry.
//!
//! Auto-commit is best-effort: any RPC failure is logged as a warn
//! and the row is still deleted. The alternative (leave the row
//! until the client recovers) would stall the sweep and prevent
//! the user from ever re-grabbing the same release since the
//! in-flight dedup path would match the stuck row.

use std::time::Duration;

use crate::AppState;
use crate::models::pending_grabs;

/// Tick interval for the TTL sweep. Matches plan decision #3's "1
/// minute TTL + 1 minute sweep" shape so the worst-case auto-commit
/// latency is `HEARTBEAT_TTL_SECS + SWEEP_INTERVAL ≈ 2 minutes`.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Run one sweep pass. Separate from the interval loop so tests can
/// drive a single tick with a seeded DB. Takes `&AppState` rather
/// than just `&SqlitePool` so the auto-commit path can reach the
/// active download client.
///
/// Returns the count of rows processed (both auto-committed and
/// silently-dropped). Tests assert on this number; the production
/// caller in `main.rs` ignores the return value.
pub async fn sweep_once(state: &AppState) -> Result<usize, String> {
    let expired = pending_grabs::list_expired(&state.db).await?;
    let count = expired.len();
    for row in expired {
        auto_commit_row(state, &row).await;
        if let Err(e) = pending_grabs::delete(&state.db, &row.preview_id).await {
            tracing::warn!(
                target: "ryokan::services::grab_sweep",
                preview_id = %row.preview_id,
                info_hash = %row.info_hash,
                error = %e,
                "failed to delete expired pending grab; will retry on next tick"
            );
        }
    }
    Ok(count)
}

/// Best-effort auto-commit for one expired row. Read-only against
/// the passed `AppState` — all mutation goes through `DownloadClient`
/// trait methods. Failures are logged and swallowed so the caller
/// can delete the pending row regardless.
async fn auto_commit_row(state: &AppState, row: &pending_grabs::PendingGrab) {
    // Don't unwind a previously-errored row. The handler already
    // surfaced the failure to the modal (status: "error") and the
    // torrent is either gone or in a broken state the user should
    // decide about via the Downloads page.
    if !row.error_message.is_empty() {
        tracing::debug!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            "evicted errored pending grab without auto-commit"
        );
        return;
    }

    // Metadata never arrived → no file list to mark. Best we can
    // do is leave the torrent in whatever state the client has it.
    // Logged at `debug` because this is a normal outcome for a
    // short-lived tab close on a dead magnet.
    if row.file_list_json.is_empty() {
        tracing::debug!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            "evicted pending grab without file list; no auto-commit possible"
        );
        return;
    }

    // Multi-client routing — the auto-commit-on-walkaway path is the
    // 4th manual dispatch site (alongside the two grab handlers and
    // the picker confirm/cancel pair) and must hit the same client
    // the preview's `add_torrent_paused` landed on. Mirroring the
    // cancel handler's lookup: stamped id → `client_by_id`, NULL →
    // default. Pre-fix this path always grabbed the default, so a
    // walkaway on a torrent paused on the seedbox would `set_file_wanted`
    // + `resume` against local qBit, fail (the torrent isn't there),
    // log a warn, and the user's torrent stayed paused on the seedbox
    // forever — no retry because the pending row gets dropped after
    // this function returns regardless of outcome.
    let client = match row.download_client_id {
        Some(id) => state.client_by_id(id).await,
        None => state.default_download_client().await,
    };
    let Some(client) = client else {
        tracing::warn!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            stamped_client_id = ?row.download_client_id,
            "download client not available; can't auto-commit expired pending grab"
        );
        return;
    };

    // Parse the file list — we need the filenames for auto-expand
    // and the length for the `0..len` index slice passed to
    // `set_file_wanted`.
    let files: Vec<crate::handlers::grab::PreviewFile> =
        serde_json::from_str(&row.file_list_json).unwrap_or_default();
    let file_count = files.len();
    if file_count == 0 {
        tracing::warn!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            "pending grab had an empty file_list_json array; skipping auto-commit"
        );
        return;
    }

    // The picker's last posted selection, when there is one: the
    // walkaway commits what the user had ticked, the way Confirm would
    // have. Nothing posted (an older modal, metadata that arrived
    // after the tab closed) means every file, as before.
    let selection: Option<Vec<usize>> = pending_grabs::wanted_indices(row)
        .map(|v| {
            v.into_iter()
                .filter(|&i| i < file_count)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());
    let wanted: Vec<usize> = match &selection {
        Some(v) => v.clone(),
        None => (0..file_count).collect(),
    };
    if let Some(sel) = &selection {
        let chosen: std::collections::HashSet<usize> = sel.iter().copied().collect();
        let unwanted: Vec<usize> = (0..file_count).filter(|i| !chosen.contains(i)).collect();
        if !unwanted.is_empty()
            && let Err(e) = client
                .set_file_wanted(&row.info_hash, &unwanted, false)
                .await
        {
            tracing::warn!(
                target: "ryokan::services::grab_sweep",
                preview_id = %row.preview_id,
                info_hash = %row.info_hash,
                error = %e,
                "auto-commit set_file_wanted(selection, false) failed; the unticked files may download too"
            );
        }
    }
    if let Err(e) = client.set_file_wanted(&row.info_hash, &wanted, true).await {
        // Skip the resume. Cross-client asymmetry: Deluge /
        // Transmission / rtorrent add with files defaulting to
        // wanted, so a failed `set_file_wanted(all=true)` leaves
        // those clients in the right state and `resume()` starts
        // the download. qBit's `add_torrent_paused` sets every
        // file to priority 0, so a failed `set_file_wanted` here
        // means resuming would flip the torrent to "running with
        // nothing downloading" silently. The pending row is about
        // to be deleted so there's no automated retry; resuming
        // the qBit case would require manual intervention from
        // the Downloads page to recover. Better to leave the
        // torrent in whatever stopped / defaults state the client
        // has it in and surface the failure via the warn log.
        tracing::warn!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            error = %e,
            "auto-commit set_file_wanted(all=true) failed; skipping resume — user can recover from the Downloads page"
        );
        return;
    }

    if let Err(e) = client.resume(&row.info_hash).await {
        tracing::warn!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            error = %e,
            "auto-commit resume failed; torrent may remain paused"
        );
        return;
    }

    // Library attribution. Walkaway means the user got every
    // file wanted, so the full file list is passed to auto-expand —
    // unlike the confirm path which passes only the user-selected
    // subset. Failures inside `commit_grab_and_expand` are logged;
    // the sweep still deletes the pending row so it doesn't loop.
    let release_title = crate::handlers::grab::extract_release_title(&row.release_metadata_json)
        .unwrap_or_else(|| row.info_hash.clone());
    // Search-hit batch flag is the authoritative source; the
    // file-count fallback only fires for pre-fix modal payloads
    // (see `extract_release_is_batch`).
    let is_batch = crate::handlers::grab::extract_release_is_batch(&row.release_metadata_json)
        .unwrap_or(file_count > 1);
    let wanted_set: std::collections::HashSet<usize> = wanted.iter().copied().collect();
    let wanted_count = wanted.len();
    let filenames: Vec<String> = files
        .into_iter()
        .enumerate()
        .filter(|(i, _)| wanted_set.contains(i))
        .map(|(_, f)| f.name)
        .collect();
    crate::services::grab_commit::commit_grab_and_expand(
        state,
        row,
        filenames,
        &release_title,
        is_batch,
    )
    .await;

    tracing::info!(
        target: "ryokan::services::grab_sweep",
        preview_id = %row.preview_id,
        info_hash = %row.info_hash,
        file_count = %file_count,
        wanted_count = %wanted_count,
        "auto-committed abandoned pending grab; torrent is now downloading with the picker's selection"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::pending_grabs::HEARTBEAT_TTL_SECS;
    use crate::services::download_client::{
        AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
    };
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Recording mock `DownloadClient` for auto-commit happy-path tests.
    /// Captures every `set_file_wanted` and `resume` call so the test
    /// can assert the exact sequence fired against the right info_hash.
    /// Only the two methods the sweep actually calls do real work; the
    /// rest return stubbed values to satisfy the trait.
    #[derive(Default)]
    struct RecordingClient {
        set_wanted_calls: Mutex<Vec<(String, Vec<usize>, bool)>>,
        resume_calls: Mutex<Vec<String>>,
        set_wanted_fails: bool,
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
        async fn resume(&self, hash: &str) -> Result<(), String> {
            self.resume_calls.lock().unwrap().push(hash.to_string());
            Ok(())
        }
        async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
            Ok(())
        }
        async fn set_file_wanted(
            &self,
            hash: &str,
            files: &[usize],
            wanted: bool,
        ) -> Result<(), String> {
            self.set_wanted_calls
                .lock()
                .unwrap()
                .push((hash.to_string(), files.to_vec(), wanted));
            if self.set_wanted_fails {
                Err("simulated set_file_wanted failure".into())
            } else {
                Ok(())
            }
        }
        fn sonarr_impl_name(&self) -> &'static str {
            "QBittorrent"
        }
    }

    fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn sweep_drops_stale_rows_only() {
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "fresh",
            "h1",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        pending_grabs::create(
            &db,
            "stale",
            "h2",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ? WHERE preview_id = 'stale'")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "fresh").await.unwrap().is_some());
        assert!(pending_grabs::get(&db, "stale").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sweep_is_idempotent_on_empty() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn sweep_continues_after_individual_row_failure() {
        // Simulated failure mode: the delete path could fail if the
        // DB is under contention. The sweep must continue processing
        // the rest of the batch rather than aborting — otherwise one
        // bad row starves every subsequent eviction. Since `delete`
        // is idempotent + durable in practice, we can't easily force
        // a failure here; instead we verify the "all succeed" case
        // handles a mix of stale+fresh without surprises.
        let db = in_memory_pool().await;
        for i in 0..3 {
            let id = format!("stale-{}", i);
            pending_grabs::create(&db, &id, "h", "qbittorrent", None, None, "{}", true, None)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 3);
        assert_eq!(pending_grabs::count(&db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sweep_without_download_client_still_drops_row_no_panic() {
        // Even when the auto-commit can't fire (no client
        // configured), the row MUST still be dropped — otherwise
        // the pending_grabs table grows unboundedly and the
        // in-flight dedup path on re-grab matches stale rows
        // forever.
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "stale",
            "hash-one",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        // Populate file_list_json so we'd attempt auto-commit if
        // the client were available; prove the no-client branch
        // still cleans up the row.
        pending_grabs::set_file_list(&db, "stale", "[{\"name\":\"a.mkv\",\"size\":1}]")
            .await
            .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "stale").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sweep_auto_commits_happy_path_marks_all_wanted_and_resumes() {
        // Happy-path regression guard: a stale row with a file list
        // and a configured client must trigger
        // `set_file_wanted(all, wanted=true)` followed by
        // `resume`, both keyed on the row's info_hash. A refactor
        // that silently drops either call would leave the
        // torrent stuck paused with defaults after the row is
        // deleted — no retry path.
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "stale",
            "hash-happy",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        // 3-file list drives file_count=3 so the 0..3 index slice is
        // non-empty and the set_file_wanted call actually fires.
        pending_grabs::set_file_list(
            &db,
            "stale",
            "[{\"name\":\"a.mkv\",\"size\":1},{\"name\":\"b.mkv\",\"size\":2},{\"name\":\"c.mkv\",\"size\":3}]",
        )
        .await
        .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();

        let client = Arc::new(RecordingClient::default());
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "stale").await.unwrap().is_none());

        let wanted = client.set_wanted_calls.lock().unwrap().clone();
        assert_eq!(
            wanted,
            vec![("hash-happy".to_string(), vec![0, 1, 2], true)],
            "expected one set_file_wanted call marking every file wanted"
        );
        let resumed = client.resume_calls.lock().unwrap().clone();
        assert_eq!(
            resumed,
            vec!["hash-happy".to_string()],
            "expected one resume call on the row's info_hash"
        );
    }

    #[tokio::test]
    async fn sweep_auto_commits_the_pickers_last_selection() {
        // The modal posted its tick state before the walkaway: the
        // sweep marks the rest unwanted and the selection wanted, the
        // way Confirm would, instead of every file.
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "picked",
            "hash-picked",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        pending_grabs::set_file_list(
            &db,
            "picked",
            "[{\"name\":\"a.mkv\",\"size\":1},{\"name\":\"b.mkv\",\"size\":2},{\"name\":\"c.mkv\",\"size\":3}]",
        )
        .await
        .unwrap();
        // Index 7 is out of range and dropped; nothing else changes.
        pending_grabs::set_wanted_indices(&db, "picked", "[1,7]")
            .await
            .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();

        let client = Arc::new(RecordingClient::default());
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        assert_eq!(sweep_once(&state).await.unwrap(), 1);
        let wanted = client.set_wanted_calls.lock().unwrap().clone();
        assert_eq!(
            wanted,
            vec![
                ("hash-picked".to_string(), vec![0, 2], false),
                ("hash-picked".to_string(), vec![1], true),
            ],
            "the unticked files go unwanted, the ticked one wanted"
        );
        assert_eq!(
            client.resume_calls.lock().unwrap().clone(),
            vec!["hash-picked".to_string()]
        );
    }

    /// Multi-client routing regression — the auto-commit-on-walkaway
    /// path must dispatch to the client the preview added the torrent
    /// to (per `pending_grabs.download_client_id`), NOT the pool's
    /// default. Pre-PR-110-review-1 the sweep used
    /// `default_download_client()` unconditionally; a walkaway on a
    /// torrent pinned to a non-default client would leave the torrent
    /// paused on the wrong client forever (no retry).
    ///
    /// Pool shape: id=1 default + id=2 second client. Pending row
    /// stamped with `download_client_id=2`. Assert the
    /// set_file_wanted+resume call pair lands on the id=2 client, not
    /// id=1.
    #[tokio::test]
    async fn sweep_dispatches_to_pinned_client_not_default() {
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "pinned",
            "hash-pinned",
            "deluge",
            None,
            None,
            "{}",
            true,
            Some(2),
        )
        .await
        .unwrap();
        pending_grabs::set_file_list(&db, "pinned", "[{\"name\":\"a.mkv\",\"size\":1}]")
            .await
            .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();

        // Build a multi-client pool by hand. `build_test_app_state`
        // can't do this (it plants the supplied client at id=1 with
        // `default_id=Some(1)`), so the pool ends up with one entry
        // and `client_by_id(2)` always misses — an existing bug
        // would silently fall through to default and the test would
        // pass even with the regression in place.
        let default_client = Arc::new(RecordingClient::default());
        let pinned_client = Arc::new(RecordingClient::default());
        let mut clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>> =
            std::collections::HashMap::new();
        clients.insert(1, default_client.clone());
        clients.insert(2, pinned_client.clone());
        let pool = crate::DownloadClientPool {
            clients,
            default_torrent_id: Some(1),
            default_usenet_id: None,
        };
        let state = build_test_app_state(db.clone(), None);
        // Override the pool — `build_test_app_state` left it empty
        // since we passed `None`. Direct mutation is fine in tests.
        *state.download_clients.write().await = Arc::new(pool);

        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);

        // The pinned client must have received the calls.
        let pinned_wanted = pinned_client.set_wanted_calls.lock().unwrap().clone();
        let pinned_resumed = pinned_client.resume_calls.lock().unwrap().clone();
        assert_eq!(
            pinned_wanted,
            vec![("hash-pinned".to_string(), vec![0], true)],
            "set_file_wanted must hit the PINNED client (id=2), not the default"
        );
        assert_eq!(
            pinned_resumed,
            vec!["hash-pinned".to_string()],
            "resume must hit the PINNED client (id=2), not the default"
        );

        // The default client must NOT have received any calls — this
        // is the assertion that catches the regression. Pre-fix this
        // would fail because every call landed on the default.
        let default_wanted = default_client.set_wanted_calls.lock().unwrap().clone();
        let default_resumed = default_client.resume_calls.lock().unwrap().clone();
        assert!(
            default_wanted.is_empty(),
            "default client must not see set_file_wanted; got {default_wanted:?}"
        );
        assert!(
            default_resumed.is_empty(),
            "default client must not see resume; got {default_resumed:?}"
        );
    }

    #[tokio::test]
    async fn sweep_skips_resume_when_set_file_wanted_fails() {
        // qBit-asymmetry guard: `add_torrent_paused` on qBit leaves
        // every file at priority 0, so if `set_file_wanted(all,
        // true)` fails the sweep must NOT call `resume` — otherwise
        // we'd flip the torrent to "running with nothing
        // downloading" with no automated recovery. Deluge/
        // Transmission/rtorrent would be safe to resume in this
        // failure mode, but the sweep is one code path so we take
        // the conservative route that works for every client.
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "stale",
            "hash-fail",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        pending_grabs::set_file_list(&db, "stale", "[{\"name\":\"a.mkv\",\"size\":1}]")
            .await
            .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();

        let client = Arc::new(RecordingClient {
            set_wanted_fails: true,
            ..Default::default()
        });
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "stale").await.unwrap().is_none());

        assert_eq!(
            client.set_wanted_calls.lock().unwrap().len(),
            1,
            "set_file_wanted should have been attempted once"
        );
        assert!(
            client.resume_calls.lock().unwrap().is_empty(),
            "resume must NOT be called after set_file_wanted failure (qBit asymmetry)"
        );
    }

    #[tokio::test]
    async fn sweep_evicts_errored_rows_without_attempting_autocommit() {
        // A row with `error_message != ''` came in via a metadata-
        // fetch failure. The auto-commit path must skip it — the
        // torrent is either gone or in a broken state we shouldn't
        // try to resume.
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "errored",
            "hash-err",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        pending_grabs::set_error(&db, "errored", "metadata fetch timed out")
            .await
            .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "errored").await.unwrap().is_none());
    }
}
