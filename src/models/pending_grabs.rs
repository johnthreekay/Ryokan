//! Interactive file-picker scratch state (issue #83).
//!
//! One row per open modal. Lifecycle:
//!
//! 1. User hits Grab → [`create`] inserts a row with the torrent's
//!    `info_hash`, the active client's `client_kind`, and a serialized
//!    `release_metadata_json` snapshot of the `SearchResult` the user
//!    was looking at. `heartbeat_at` is initialized to `created_at`.
//!    `file_list_json` is empty until the `wait_for_metadata` fetch
//!    completes — that's what distinguishes `status: fetching_metadata`
//!    from `status: ready` on `GET /api/grab/preview/{id}`.
//! 2. Modal polls `GET /api/grab/preview/{id}` → reads the row via
//!    [`get`]; once `file_list_json` is non-empty, the modal renders
//!    the file tree.
//! 3. Modal pings `POST /api/grab/heartbeat/{id}` every ~30s → updates
//!    `heartbeat_at` via [`bump_heartbeat`]. While the user is
//!    deliberating, TTL never elapses.
//! 4. User clicks Confirm → backend applies `set_file_wanted` per the
//!    user's selection, resumes the torrent, writes the
//!    `grabbed_torrents` row, and calls [`delete`] to drop this row.
//! 5. Modal closes without confirm (X in corner, tab close, crash) →
//!    heartbeat stops → within `HEARTBEAT_TTL + sweep_interval` the
//!    sweep task catches the stale row, auto-commits with all files
//!    wanted, and calls [`delete`]. Per decision #3 we NEVER delete
//!    the underlying torrent on walkaway — the user said "Grab," so
//!    they wanted the release.
//!
//! The only path that deletes the torrent itself is the internal
//! error-recovery cancel (see `handlers::grab::cancel`) and the
//! sweep's "metadata never arrived, can't auto-commit" fallback.

use sqlx::{FromRow, SqlitePool};

/// Heartbeat TTL — time since the last heartbeat at which the TTL sweep
/// considers a pending grab abandoned and auto-commits it. Matches
/// decision #3: 1-minute TTL with a 1-minute sweep tick, so worst-case
/// auto-commit latency is ~2 minutes after tab close.
pub const HEARTBEAT_TTL_SECS: i64 = 60;

#[derive(Debug, Clone, FromRow)]
// The struct carries more columns than the handler currently reads;
// `client_kind` / `indexer_id` / `series_id` / `created_at` /
// `heartbeat_at` are populated for the sweep's decision-making and
// for future diagnostics. Struct-level dead-code allow so those
// fields stay available for their eventual consumers without
// per-field noise.
#[allow(dead_code)]
pub struct PendingGrab {
    pub preview_id: String,
    pub info_hash: String,
    pub client_kind: String,
    pub indexer_id: Option<i64>,
    pub series_id: Option<i64>,
    pub created_at: i64,
    pub heartbeat_at: i64,
    /// JSON-encoded `Vec<DownloadFile>` once metadata has arrived.
    /// Empty string while `wait_for_metadata` is still polling.
    pub file_list_json: String,
    /// JSON-encoded `SearchResult` snapshot captured when the user
    /// hit Grab — the modal uses it to render the header row
    /// (title, size, seeders) before the file list arrives.
    pub release_metadata_json: String,
    /// Populated by the metadata-fetch task on permanent failure
    /// (add_torrent_paused err, wait_for_files timeout, serialize
    /// error). Empty string = no error so far; any other value is
    /// a human-readable failure description. GET preview flips
    /// `status: "error"` when this is non-empty so the modal can
    /// surface the retry/defaults dialog immediately rather than
    /// waiting for the TTL sweep. The ALTER TABLE adds this with
    /// `DEFAULT ''`, so rows written before the column existed
    /// load as "no error" without any Rust-side default needed.
    pub error_message: String,
    /// The picker's last tick state as a JSON list of file indices,
    /// posted while the modal is open (`POST /api/grab/selection`), so
    /// the walkaway auto-commit takes the files the user had selected.
    /// Empty when nothing was posted: every file, the pre-selection
    /// behavior.
    pub wanted_indices_json: String,
    /// `true` when `add_torrent_paused` returned `Added` for this
    /// preview, `false` when it returned `AlreadyPresent`. `grab_cancel`
    /// gates its destructive `delete(hash, with_files=true)` call on
    /// this so a cancel on a pre-existing (user-added-earlier) torrent
    /// doesn't nuke data the user may have partial-downloaded. The
    /// ALTER TABLE adds this with `DEFAULT 1`, so rows written before
    /// the column existed take the conservative "yes, we added it"
    /// path on read without any Rust-side default needed.
    pub we_added_torrent: bool,
    /// Multi-client refactor — the `download_clients.id` the preview's
    /// `add_torrent_paused` landed on. Confirm reads this back to
    /// resume the torrent against the same client. NULL on legacy
    /// rows; the confirm handler then falls back to default.
    pub download_client_id: Option<i64>,
}

/// Insert a new row. `preview_id` is a caller-supplied opaque string
/// (hex token generated at the handler layer, same shape as session
/// cookies). The `indexer_id` field is kept nullable for forward-
/// compat with issue #28 — current callers (Nyaa-direct only) always
/// pass `None`.
///
/// `we_added_torrent` records whether the preview's backing torrent
/// was added fresh by this preview's `add_torrent_paused` call
/// (`true`) or was already in the download client at add time
/// (`false`, mapped from `AddOutcome::AlreadyPresent`). Used by the
/// cancel path to decide whether the destructive `delete(..., true)`
/// is appropriate or whether it would nuke a pre-existing grab.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    db: &SqlitePool,
    preview_id: &str,
    info_hash: &str,
    client_kind: &str,
    indexer_id: Option<i64>,
    series_id: Option<i64>,
    release_metadata_json: &str,
    we_added_torrent: bool,
    download_client_id: Option<i64>,
) -> Result<(), String> {
    let now = now_unix();
    sqlx::query(
        "INSERT INTO pending_grabs \
         (preview_id, info_hash, client_kind, indexer_id, series_id, \
          created_at, heartbeat_at, file_list_json, release_metadata_json, \
          error_message, we_added_torrent, download_client_id, wanted_indices_json) \
         VALUES (?, ?, ?, ?, ?, ?, ?, '', ?, '', ?, ?, '')",
    )
    .bind(preview_id)
    .bind(info_hash)
    .bind(client_kind)
    .bind(indexer_id)
    .bind(series_id)
    .bind(now)
    .bind(now)
    .bind(release_metadata_json)
    .bind(if we_added_torrent { 1_i64 } else { 0_i64 })
    .bind(download_client_id)
    .execute(db)
    .await
    .map_err(|e| format!("failed to create pending grab: {}", e))?;
    Ok(())
}

const SELECT_COLUMNS: &str = "preview_id, info_hash, client_kind, indexer_id, series_id, \
     created_at, heartbeat_at, file_list_json, release_metadata_json, \
     error_message, we_added_torrent, download_client_id, wanted_indices_json";

pub async fn get(db: &SqlitePool, preview_id: &str) -> Result<Option<PendingGrab>, String> {
    sqlx::query_as::<_, PendingGrab>(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLUMNS} FROM pending_grabs WHERE preview_id = ?"
    )))
    .bind(preview_id)
    .fetch_optional(db)
    .await
    .map_err(|e| format!("failed to read pending grab: {}", e))
}

/// Fetch a pending grab by its backing torrent's `info_hash` — used
/// by the pre-modal concurrency check ("is there already an open
/// modal for this release in another tab?"). Returns the most
/// recently created matching row that is **NOT** in an error state,
/// or `None` when no live modal holds the hash.
///
/// The `error_message = ''` filter exists so Tab 2 opening the
/// same release after Tab 1's metadata fetch errored doesn't get
/// stuck on Tab 1's failure — without this, `get_by_hash` would
/// return the error row and the handler would short-circuit Tab 2
/// onto `status: "error"` immediately, forcing the user to wait
/// ~2 min for the TTL sweep to drop the row before a retry can
/// start. With the filter, Tab 2 falls through to a fresh paused-
/// add attempt on the same hash — qBit's `AddOutcome::AlreadyPresent`
/// handling takes over from there, so the retry is cheap.
pub async fn get_by_hash(db: &SqlitePool, info_hash: &str) -> Result<Option<PendingGrab>, String> {
    if info_hash.is_empty() {
        return Ok(None);
    }
    sqlx::query_as::<_, PendingGrab>(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLUMNS} FROM pending_grabs WHERE info_hash = ? \
         AND error_message = '' \
         ORDER BY created_at DESC LIMIT 1"
    )))
    .bind(info_hash)
    .fetch_optional(db)
    .await
    .map_err(|e| format!("failed to read pending grab by hash: {}", e))
}

/// Update only the `file_list_json` column — called once by the
/// handler-side metadata-fetch task as soon as `wait_for_metadata`
/// returns. A separate setter (rather than overloading
/// `bump_heartbeat`) so the metadata task's write doesn't race the
/// modal's heartbeat write.
pub async fn set_file_list(
    db: &SqlitePool,
    preview_id: &str,
    file_list_json: &str,
) -> Result<(), String> {
    sqlx::query("UPDATE pending_grabs SET file_list_json = ? WHERE preview_id = ?")
        .bind(file_list_json)
        .bind(preview_id)
        .execute(db)
        .await
        .map_err(|e| format!("failed to update file list: {}", e))?;
    Ok(())
}

/// Record a permanent metadata-fetch failure on this preview. The
/// GET endpoint promotes a non-empty `error_message` to
/// `status: "error"` so the modal flips to the retry/defaults
/// dialog immediately rather than waiting for the TTL sweep.
pub async fn set_error(
    db: &SqlitePool,
    preview_id: &str,
    error_message: &str,
) -> Result<(), String> {
    sqlx::query("UPDATE pending_grabs SET error_message = ? WHERE preview_id = ?")
        .bind(error_message)
        .bind(preview_id)
        .execute(db)
        .await
        .map_err(|e| format!("failed to update error_message: {}", e))?;
    Ok(())
}

/// Store the picker's current selection (a JSON list of file indices)
/// for the walkaway auto-commit. Returns `false` when no row matched.
pub async fn set_wanted_indices(
    db: &SqlitePool,
    preview_id: &str,
    wanted_indices_json: &str,
) -> Result<bool, String> {
    let result =
        sqlx::query("UPDATE pending_grabs SET wanted_indices_json = ? WHERE preview_id = ?")
            .bind(wanted_indices_json)
            .bind(preview_id)
            .execute(db)
            .await
            .map_err(|e| format!("failed to store the selection: {}", e))?;
    Ok(result.rows_affected() > 0)
}

/// The stored selection as indices, `None` when nothing was posted or
/// the text does not parse. An empty list counts as nothing posted:
/// committing no file at all is never what a walkaway means.
pub fn wanted_indices(row: &PendingGrab) -> Option<Vec<usize>> {
    if row.wanted_indices_json.is_empty() {
        return None;
    }
    serde_json::from_str::<Vec<usize>>(&row.wanted_indices_json)
        .ok()
        .filter(|v| !v.is_empty())
}

/// Update `heartbeat_at` to the current unix time. Returns `false`
/// when no row matched (caller should surface 404 to the modal so it
/// can show "This grab was already committed" gracefully).
pub async fn bump_heartbeat(db: &SqlitePool, preview_id: &str) -> Result<bool, String> {
    let now = now_unix();
    let result = sqlx::query("UPDATE pending_grabs SET heartbeat_at = ? WHERE preview_id = ?")
        .bind(now)
        .bind(preview_id)
        .execute(db)
        .await
        .map_err(|e| format!("failed to bump heartbeat: {}", e))?;
    Ok(result.rows_affected() > 0)
}

/// Drop the row. Used by the confirm / cancel / sweep paths after
/// the outcome (grab row written, torrent deleted, etc.) has already
/// landed.
pub async fn delete(db: &SqlitePool, preview_id: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM pending_grabs WHERE preview_id = ?")
        .bind(preview_id)
        .execute(db)
        .await
        .map_err(|e| format!("failed to delete pending grab: {}", e))?;
    Ok(())
}

/// Return every pending grab whose `heartbeat_at` is older than
/// `HEARTBEAT_TTL_SECS` seconds ago. The sweep task iterates this
/// list and auto-commits each row.
pub async fn list_expired(db: &SqlitePool) -> Result<Vec<PendingGrab>, String> {
    let cutoff = now_unix() - HEARTBEAT_TTL_SECS;
    sqlx::query_as::<_, PendingGrab>(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLUMNS} FROM pending_grabs \
         WHERE heartbeat_at < ? ORDER BY heartbeat_at ASC"
    )))
    .bind(cutoff)
    .fetch_all(db)
    .await
    .map_err(|e| format!("failed to list expired pending grabs: {}", e))
}

/// Count rows — test helper.
#[cfg(test)]
pub async fn count(db: &SqlitePool) -> Result<i64, String> {
    use sqlx::Row;
    let row = sqlx::query("SELECT COUNT(*) AS n FROM pending_grabs")
        .fetch_one(db)
        .await
        .map_err(|e| format!("count failed: {}", e))?;
    Ok(row.try_get::<i64, _>("n").unwrap_or(0))
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    #[tokio::test]
    async fn create_then_get_roundtrips() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-1",
            "abc",
            "qbittorrent",
            None,
            Some(42),
            "{\"title\":\"test\"}",
            true,
            None,
        )
        .await
        .expect("create");
        let row = get(&db, "pid-1").await.expect("get").expect("row present");
        assert_eq!(row.preview_id, "pid-1");
        assert_eq!(row.info_hash, "abc");
        assert_eq!(row.client_kind, "qbittorrent");
        assert_eq!(row.series_id, Some(42));
        assert_eq!(row.file_list_json, "");
        assert_eq!(row.release_metadata_json, "{\"title\":\"test\"}");
        assert_eq!(row.error_message, "");
        assert!(row.we_added_torrent);
        assert_eq!(row.created_at, row.heartbeat_at);
    }

    /// Pre-PR-F-followup regression: confirm/cancel used
    /// `default_download_client` even when preview had landed on a
    /// pinned client. The schema column `download_client_id` plus
    /// `create()`'s 9th arg + the handler's read-back together cover
    /// the fix; this test pins the round-trip so a future SELECT-list
    /// refactor can't drop the column and silently regress.
    #[tokio::test]
    async fn download_client_id_round_trips() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-pinned",
            "ab",
            "deluge",
            Some(7),
            Some(42),
            "{}",
            true,
            Some(2),
        )
        .await
        .expect("create");
        let row = get(&db, "pid-pinned")
            .await
            .expect("get")
            .expect("row present");
        assert_eq!(row.download_client_id, Some(2));
        assert_eq!(row.indexer_id, Some(7));

        // Legacy (NULL) shape still works.
        create(
            &db,
            "pid-legacy",
            "cd",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .expect("create");
        let row = get(&db, "pid-legacy")
            .await
            .expect("get")
            .expect("row present");
        assert_eq!(row.download_client_id, None);
    }

    #[tokio::test]
    async fn we_added_false_roundtrips() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-1",
            "abc",
            "qbittorrent",
            None,
            None,
            "{}",
            false,
            None,
        )
        .await
        .unwrap();
        let row = get(&db, "pid-1").await.unwrap().unwrap();
        assert!(!row.we_added_torrent);
    }

    #[tokio::test]
    async fn set_error_flips_column() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-1",
            "abc",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        assert!(
            get(&db, "pid-1")
                .await
                .unwrap()
                .unwrap()
                .error_message
                .is_empty()
        );
        set_error(&db, "pid-1", "metadata fetch timed out")
            .await
            .unwrap();
        let row = get(&db, "pid-1").await.unwrap().unwrap();
        assert_eq!(row.error_message, "metadata fetch timed out");
        // Should not touch file_list_json or other columns.
        assert_eq!(row.file_list_json, "");
    }

    #[tokio::test]
    async fn bump_heartbeat_updates_heartbeat_at() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-1",
            "abc",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        let before = get(&db, "pid-1").await.unwrap().unwrap().heartbeat_at;
        // Force a clock tick by sleeping 1s — the unix-seconds resolution
        // needs at least one second to move. Cheap enough for a single test.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let updated = bump_heartbeat(&db, "pid-1").await.unwrap();
        assert!(updated, "should have updated the row");
        let after = get(&db, "pid-1").await.unwrap().unwrap().heartbeat_at;
        assert!(
            after > before,
            "heartbeat_at should advance: before={} after={}",
            before,
            after
        );
    }

    #[tokio::test]
    async fn bump_heartbeat_returns_false_for_missing_row() {
        let db = in_memory_pool().await;
        let result = bump_heartbeat(&db, "nope").await.unwrap();
        assert!(!result, "missing preview_id should return false");
    }

    #[tokio::test]
    async fn set_file_list_preserves_other_columns() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-1",
            "abc",
            "qbittorrent",
            None,
            Some(7),
            "{\"title\":\"t\"}",
            true,
            None,
        )
        .await
        .unwrap();
        set_file_list(&db, "pid-1", "[{\"name\":\"a.mkv\"}]")
            .await
            .unwrap();
        let row = get(&db, "pid-1").await.unwrap().unwrap();
        assert_eq!(row.file_list_json, "[{\"name\":\"a.mkv\"}]");
        assert_eq!(row.release_metadata_json, "{\"title\":\"t\"}");
        assert_eq!(row.info_hash, "abc");
        assert_eq!(row.series_id, Some(7));
        // The picker's selection rides on the row for the walkaway
        // auto-commit; nothing posted, or an empty list, is "every
        // file".
        assert_eq!(row.wanted_indices_json, "");
        assert!(wanted_indices(&row).is_none());
        assert!(set_wanted_indices(&db, "pid-1", "[0,2]").await.unwrap());
        let row = get(&db, "pid-1").await.unwrap().unwrap();
        assert_eq!(wanted_indices(&row), Some(vec![0, 2]));
        assert!(set_wanted_indices(&db, "pid-1", "[]").await.unwrap());
        let row = get(&db, "pid-1").await.unwrap().unwrap();
        assert!(wanted_indices(&row).is_none());
        assert!(!set_wanted_indices(&db, "missing", "[0]").await.unwrap());
    }

    #[tokio::test]
    async fn delete_removes_the_row() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-1",
            "abc",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        delete(&db, "pid-1").await.unwrap();
        assert!(get(&db, "pid-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_by_hash_returns_most_recent() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-old",
            "deadbeef",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        create(
            &db,
            "pid-new",
            "deadbeef",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        let row = get_by_hash(&db, "deadbeef")
            .await
            .unwrap()
            .expect("row present");
        assert_eq!(row.preview_id, "pid-new", "most recent row should win");
    }

    #[tokio::test]
    async fn get_by_hash_ignores_empty_hash() {
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-1",
            "",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        assert!(
            get_by_hash(&db, "").await.unwrap().is_none(),
            "empty hash lookup should never return a row"
        );
    }

    #[tokio::test]
    async fn get_by_hash_skips_rows_with_error_message_set() {
        // PR 89 review fix: an error-flagged row must not be
        // returned by the pre-modal dedup path, otherwise Tab 2
        // opening the same release sees Tab 1's failure
        // immediately and has no retry path except waiting for the
        // TTL sweep. Invisible-to-dedup means Tab 2 falls through
        // to a fresh paused-add attempt.
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-errored",
            "deadbeef",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        set_error(&db, "pid-errored", "metadata fetch timed out")
            .await
            .unwrap();
        assert!(
            get_by_hash(&db, "deadbeef").await.unwrap().is_none(),
            "rows with error_message set must be invisible to dedup lookup"
        );
    }

    #[tokio::test]
    async fn get_by_hash_skips_error_row_but_returns_healthy_newer_row() {
        // Two rows on the same hash: Tab 1 errored, Tab 2 is
        // healthy. The healthy Tab 2 row — the "newer" one — wins,
        // which is correct: error-flagged dedup suppression must
        // not swallow a legitimate parallel session.
        let db = in_memory_pool().await;
        create(
            &db,
            "pid-err",
            "deadbeef",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        set_error(&db, "pid-err", "failure").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        create(
            &db,
            "pid-ok",
            "deadbeef",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        let row = get_by_hash(&db, "deadbeef")
            .await
            .unwrap()
            .expect("healthy row must be returned");
        assert_eq!(row.preview_id, "pid-ok");
    }

    #[tokio::test]
    async fn list_expired_returns_stale_rows_only() {
        let db = in_memory_pool().await;
        // Fresh row — heartbeat just now.
        create(
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
        // Backdate a second row's heartbeat past the TTL.
        create(
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
        let stale_heartbeat = now_unix() - HEARTBEAT_TTL_SECS - 5;
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ? WHERE preview_id = 'stale'")
            .bind(stale_heartbeat)
            .execute(&db)
            .await
            .unwrap();
        let expired = list_expired(&db).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].preview_id, "stale");
    }

    #[tokio::test]
    async fn count_reflects_creates_and_deletes() {
        let db = in_memory_pool().await;
        assert_eq!(count(&db).await.unwrap(), 0);
        create(
            &db,
            "pid-1",
            "a",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        create(
            &db,
            "pid-2",
            "b",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        assert_eq!(count(&db).await.unwrap(), 2);
        delete(&db, "pid-1").await.unwrap();
        assert_eq!(count(&db).await.unwrap(), 1);
    }
}
