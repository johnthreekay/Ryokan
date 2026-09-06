//! Download-client cleanup after import (issue #228).
//!
//! Until 1.9.3 a finished download stayed in its client forever: a
//! move-mode torrent sat in qBittorrent with its episode gone from the
//! download folder, every SABnzbd job kept its history entry and
//! unpacked folder, and a hardlink-mode torrent that had long since met
//! its ratio was never removed. Two entry points, both gated by the
//! client row's `remove_completed` switch (Sonarr's per-client "Remove
//! Completed", default on):
//!
//! - [`remove_after_import`] runs from `run_once` on a fully imported
//!   grab (`ImportOutcome::Imported`; partial imports and failures stay
//!   in the client). A usenet job has nothing to seed and leaves SAB's
//!   history at once. So does a torrent in move mode: its file has left
//!   the download folder and cannot seed anyway. Hardlink and copy mode
//!   torrents keep seeding.
//! - [`sweep_finished_seeds`] runs after every post-processing tick,
//!   throttled to [`FINISHED_SEED_SWEEP_INTERVAL`] and serialized on
//!   `SEED_SWEEP_LOCK`, over grabs post-processing really imported
//!   (`state = 'imported'` **and** `imported_at` set; a row
//!   `mark_completed_no_import` advanced with post-processing off never
//!   qualifies, its download is the user's only copy) whose item may
//!   still sit in a client (`client_removed_at IS NULL`). An item the
//!   client reports done seeding (`DownloadItem::seeding_done`: its own
//!   ratio, seed-time, or inactivity rule met) is removed; an item
//!   already gone (the client's own "remove at ratio" action, or the
//!   user) is stamped without a call; an errored item is never treated
//!   as done seeding; anything else waits. A row imported in move mode
//!   is also removed once its stamped source files are gone while their
//!   folders still exist (errored or not; the files are gone either
//!   way), which is how the torrents #228 was reported against leave
//!   the client after an upgrade. Usenet rows present and complete are
//!   removed on sight. Partial imports are never in the work list.
//!
//! Removal always deletes the client's copy of the files: the library
//! holds its own (a hardlink shares the inode and survives; copy mode
//! has a duplicate) or the file was moved out. SAB's `del_files=1`
//! no-ops when the history `storage` points at the parent complete
//! folder, so the stamped `imported_source_paths` are unlinked as well,
//! the same belt and braces the episode delete path uses. Stamped paths
//! under the media root, or with no media root to check against, are
//! never touched.
//!
//! The "source files gone" rule is deliberately narrow. It applies only
//! to rows imported in move mode (`grabbed_torrents.import_mode`, with
//! the current mode standing in for rows stamped before 1.9.3), and only
//! when each missing file's parent folder still exists: a download mount
//! that drops out takes the folders with it and reads as "waiting", not
//! "gone", so it can never empty a seeding client. The sweep never uses
//! an errored state as a reason on its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, grabbed_torrents};
use crate::services::download_client::{DownloadClient, DownloadItem};
use crate::services::logger;

/// How often the finished-seed sweep asks each client for its list.
pub const FINISHED_SEED_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Serializes sweeps (`try_lock`: a tick that finds one running skips).
pub static SEED_SWEEP_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

static LAST_SWEEP: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

/// Why an item is leaving its client; drives the log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalReason {
    UsenetImported,
    MoveModeImported,
    SeedingDone,
    SourceGone,
}

impl RemovalReason {
    pub fn describe(self) -> &'static str {
        match self {
            RemovalReason::UsenetImported => "usenet job imported, nothing to seed",
            RemovalReason::MoveModeImported => {
                "imported in move mode, the file left the download folder"
            }
            RemovalReason::SeedingDone => "the client reports its seeding rules met",
            RemovalReason::SourceGone => {
                "imported in move mode and the source files are no longer in the download folder"
            }
        }
    }
}

/// One sweep's tally, for tests and the debug line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Items removed from a client.
    pub removed: usize,
    /// Rows stamped because the item was already gone.
    pub gone: usize,
    /// Items still seeding, errored, or on a client that could not be
    /// listed.
    pub waiting: usize,
    /// Client delete calls that failed; the row is retried next pass.
    pub failed: usize,
    /// Rows whose client could not be resolved from the pool.
    pub unresolved: usize,
    /// Rows on a client whose "Remove completed downloads" switch is off.
    pub disabled: usize,
}

/// Remove a just-imported grab's item from its client when nothing is
/// left to seed: a usenet job, or a torrent imported in move mode.
/// Hardlink and copy mode torrents are left for the sweep.
pub(super) async fn remove_after_import(
    state: &AppState,
    cfg: &config::Config,
    grab: &grabbed_torrents::GrabbedTorrent,
    client_id: i64,
    client: &Arc<dyn DownloadClient>,
) {
    if !client_allows_removal(&state.db, client_id).await {
        return;
    }
    let reason = if client.protocol() == "usenet" {
        RemovalReason::UsenetImported
    } else if cfg.post_processing_mode == "move" {
        RemovalReason::MoveModeImported
    } else {
        return;
    };
    remove_from_client(state, cfg, grab, client, reason).await;
}

/// Throttled, serialized wrapper for the post-processing loop.
pub async fn sweep_finished_seeds(state: &AppState) {
    let Ok(_running) = SEED_SWEEP_LOCK.try_lock() else {
        return;
    };
    {
        let mut last = LAST_SWEEP.lock().unwrap();
        if let Some(t) = *last
            && t.elapsed() < FINISHED_SEED_SWEEP_INTERVAL
        {
            return;
        }
        *last = Some(Instant::now());
    }
    if let Err(e) = sweep_finished_seeds_now(state).await {
        logger::warn(
            &state.db,
            LogCategory::PostProcess,
            "Finished-seed sweep failed",
            &e,
        )
        .await;
    }
}

/// One pass over every imported grab whose item may still sit in a
/// client. Each client is listed once per pass; a client that cannot
/// be listed leaves its rows for the next pass. Public for tests; the
/// loop calls [`sweep_finished_seeds`].
pub async fn sweep_finished_seeds_now(state: &AppState) -> Result<SweepReport, String> {
    let mut report = SweepReport::default();
    let Some(cfg) = config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(report);
    };
    // Same gate as `run_once`: with post-processing off nothing was ever
    // imported, and `imported_at IS NOT NULL` below is the per-row form
    // of the same rule.
    if !cfg.post_processing_enabled || cfg.media_root.trim().is_empty() {
        return Ok(report);
    }
    let rows = grabbed_torrents::list_imported_in_client(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        return Ok(report);
    }
    // Listing cache keyed by the client's Arc address. The Arc is kept
    // in the value so the allocation cannot be recycled under the key
    // if the pool is rebuilt mid-sweep; `None` records a failed list.
    type Listing = Option<HashMap<String, DownloadItem>>;
    let mut listings: HashMap<usize, (Arc<dyn DownloadClient>, Listing)> = HashMap::new();
    let mut gone: Vec<i64> = Vec::new();
    let mut switches: HashMap<i64, bool> = HashMap::new();
    for grab in &rows {
        let Some((client_id, client)) = state
            .resolve_grab_client_with_id(grab.download_client_id, &grab.hash)
            .await
        else {
            report.unresolved += 1;
            continue;
        };
        if let std::collections::hash_map::Entry::Vacant(slot) = switches.entry(client_id) {
            slot.insert(client_allows_removal(&state.db, client_id).await);
        }
        if !switches[&client_id] {
            report.disabled += 1;
            continue;
        }
        let key = Arc::as_ptr(&client) as *const () as usize;
        if let std::collections::hash_map::Entry::Vacant(slot) = listings.entry(key) {
            let listed = match client.list_scoped().await {
                Ok(items) => Some(
                    items
                        .into_iter()
                        .map(|i| (i.hash.to_lowercase(), i))
                        .collect::<HashMap<_, _>>(),
                ),
                Err(e) => {
                    // `run_once` already logs a failed list every
                    // minute; a second DB row per sweep would only
                    // double the noise for a client that is down.
                    tracing::debug!(error = %e, "finished-seed sweep could not list a client");
                    None
                }
            };
            slot.insert((client.clone(), listed));
        }
        let Some(by_hash) = listings.get(&key).and_then(|(_, l)| l.as_ref()) else {
            report.waiting += 1;
            continue;
        };
        let Some(item) = by_hash.get(&grab.hash.to_lowercase()) else {
            gone.push(grab.id);
            report.gone += 1;
            continue;
        };
        let reason = if client.protocol() == "usenet" {
            item.state_kind
                .is_complete()
                .then_some(RemovalReason::UsenetImported)
        } else if source_gone_after_move(state, &cfg, grab).await {
            Some(RemovalReason::SourceGone)
        } else if item.state_kind.is_errored() {
            // An errored item is never "finished seeding"; the client
            // could not even read it.
            None
        } else if item.seeding_done {
            Some(RemovalReason::SeedingDone)
        } else {
            None
        };
        match reason {
            None => report.waiting += 1,
            Some(reason) => {
                if remove_from_client(state, &cfg, grab, &client, reason).await {
                    report.removed += 1;
                } else {
                    report.failed += 1;
                }
            }
        }
    }
    if !gone.is_empty()
        && let Err(e) = grabbed_torrents::stamp_client_removed_many(&state.db, &gone).await
    {
        tracing::debug!(error = %e, "finished-seed sweep could not stamp vanished items");
    }
    tracing::debug!(
        removed = report.removed,
        gone = report.gone,
        waiting = report.waiting,
        failed = report.failed,
        unresolved = report.unresolved,
        disabled = report.disabled,
        "finished-seed sweep"
    );
    Ok(report)
}

/// The client row's "Remove completed downloads" switch. A client the
/// pool has but the table does not (only tests) reads as off: removal
/// is the destructive side and needs the row to say yes.
async fn client_allows_removal(db: &sqlx::SqlitePool, client_id: i64) -> bool {
    crate::models::download_clients::get_by_id(db, client_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|row| row.remove_completed)
}

/// The move-mode rule: the row was imported in move mode (its own
/// stamp, or the current mode for rows stamped before 1.9.3), it has
/// stamped source paths, and every one of them is missing while its
/// parent folder is still there. A missing parent means the download
/// mount is gone, not the file, and the row waits.
async fn source_gone_after_move(
    state: &AppState,
    cfg: &config::Config,
    grab: &grabbed_torrents::GrabbedTorrent,
) -> bool {
    let mode = grabbed_torrents::import_mode(&state.db, grab.id)
        .await
        .unwrap_or_else(|| cfg.post_processing_mode.clone());
    if mode != "move" {
        return false;
    }
    let stamped = grabbed_torrents::get_imported_source_paths(&state.db, grab.id).await;
    if stamped.is_empty() {
        return false;
    }
    let owned: Vec<PathBuf> = stamped.iter().map(PathBuf::from).collect();
    tokio::task::spawn_blocking(move || {
        owned
            .iter()
            .all(|p| !p.exists() && p.parent().is_some_and(|d| d.is_dir()))
    })
    .await
    .unwrap_or(false)
}

/// Delete the item (and the client's files) and stamp the row. A failed
/// delete leaves the row unstamped so the sweep tries again.
async fn remove_from_client(
    state: &AppState,
    cfg: &config::Config,
    grab: &grabbed_torrents::GrabbedTorrent,
    client: &Arc<dyn DownloadClient>,
    reason: RemovalReason,
) -> bool {
    if let Err(e) = client.delete(&grab.hash, true).await {
        logger::warn(
            &state.db,
            LogCategory::PostProcess,
            &format!(
                "Could not remove '{}' from the download client",
                grab.torrent_name
            ),
            &format!("{}; {e}", reason.describe()),
        )
        .await;
        return false;
    }
    let mut detail = format!(
        "{}; hash={}; the client's copy of the files was deleted",
        reason.describe(),
        grab.hash
    );
    if client.protocol() == "usenet" {
        // SAB's del_files=1 no-ops when the history storage path is the
        // parent complete folder; the stamped source paths are exact.
        let stamped = grabbed_torrents::get_imported_source_paths(&state.db, grab.id).await;
        let outside_library = paths_outside(&stamped, &cfg.media_root);
        if !outside_library.is_empty() {
            let removed = remove_stamped_source_paths(&outside_library).await;
            if !removed.is_empty() {
                detail.push_str(&format!(
                    "; unlinked {} source file(s) SAB left behind",
                    removed.len()
                ));
            }
        }
    }
    if let Err(e) = grabbed_torrents::stamp_client_removed(&state.db, grab.id).await {
        tracing::debug!(grab_id = grab.id, error = %e, "could not stamp client_removed_at");
    }
    logger::info(
        &state.db,
        LogCategory::PostProcess,
        &format!("Removed '{}' from the download client", grab.torrent_name),
        &detail,
    )
    .await;
    true
}

/// Drop any stamped path that sits under the media root: a source that
/// is also the library file must never be unlinked here. With no media
/// root to check against nothing can be proven outside it, so nothing
/// is returned.
fn paths_outside(sources: &[String], media_root: &str) -> Vec<String> {
    let root = media_root.trim();
    if root.is_empty() {
        return Vec::new();
    }
    let root = Path::new(root);
    sources
        .iter()
        .filter(|s| !Path::new(s).starts_with(root))
        .cloned()
        .collect()
}

/// Remove each path in `sources` (best-effort) and prune the
/// **immediate parent directory only** if it became empty as a
/// result. Used by the episode delete and series-remove paths and by
/// the post-import removal to clean up the source-side files Ryokan
/// imported FROM, stamped at import time so the exact paths are known
/// regardless of how the download client reports its layout.
///
/// The single-level prune cap is critical: removing `complete/job/file.mkv`
/// leaves `complete/job/` empty, which we want gone (the SAB job
/// folder); but we must NEVER ascend further to `complete/` itself,
/// or we'd nuke the user's configured complete root and all sibling
/// jobs in it. Earlier versions walked up unbounded; for users whose
/// `complete/` happened to contain only this one job, that removed the
/// entire complete dir.
pub async fn remove_stamped_source_paths(sources: &[String]) -> Vec<PathBuf> {
    let owned: Vec<PathBuf> = sources.iter().map(PathBuf::from).collect();
    tokio::task::spawn_blocking(move || {
        let mut removed = Vec::new();
        for p in &owned {
            if std::fs::remove_file(p).is_ok() {
                removed.push(p.clone());
            }
        }
        // Single-level parent prune: try once per removed file. If
        // the directory is non-empty (other files we didn't touch)
        // or rmdir fails (permission, etc.), we leave it alone. We
        // deliberately do NOT walk further up; see fn doc.
        for p in &removed {
            if let Some(dir) = p.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
        removed
    })
    .await
    .unwrap_or_default()
}
