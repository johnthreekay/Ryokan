//! Shared per-series file/torrent cleanup. Both the per-row Remove
//! handler ([`crud::remove_series`]) and the bulk delete handler
//! ([`bulk::delete_one_series`]) call into [`cleanup_series_files`] so
//! the security-critical canonicalize-and-`starts_with` guard, the PT
//! seed-rule honor (issue #28), the SAB stamped-source-path
//! cleanup, and the Jellyfin refresh nudge all stay in lockstep.
//!
//! Before this lived in one place, the bulk path silently bypassed the
//! traversal guard and the seed-rule check — issue surfaced in PR #164
//! review. Whenever one path changes, the other has to change too;
//! keeping them in one helper is the only way that doesn't drift.
//!
//! [`crud::remove_series`]: super::crud::remove_series
//! [`bulk::delete_one_series`]: super::bulk

use crate::AppState;
use crate::models::grabbed_torrents;
use crate::services::recycle::{self, RecycleKind, RecycleOutcome};

/// Outcome of a single series's filesystem + torrent cleanup pass. Each
/// field tracks one substage so callers can compose their own UX:
/// `crud::remove_series` serializes the whole report into its JSON
/// response, while `bulk::delete_one_series` checks
/// [`SeriesCleanupReport::is_clean`] to decide whether to count the
/// series as `succeeded` or fold the partial-failure signal into a
/// `BulkFailure.reason`.
#[derive(Debug)]
pub struct SeriesCleanupReport {
    /// Count of grabs whose torrent client accepted the delete (or
    /// whose seed rules say to keep seeding — counted as "handled" so
    /// the user sees the grab was processed even though the file
    /// stays on the seedbox).
    pub torrents_removed: u64,
    /// Per-grab error strings: `"Download client not configured"`,
    /// `"<hash>: <client error>"`, or `"clear grabbed_torrents: <db
    /// error>"`. Empty when every grab was handled cleanly.
    pub torrent_failures: Vec<String>,
    /// One of `"skipped"` (delete_files=false or empty media_root /
    /// folder_name), `"recycled"` (moved into the recycle bin, #123),
    /// `"removed"`, `"missing"` (canonicalize-NotFound;
    /// folder already gone), `"refused"` (canonicalized series dir
    /// resolves outside media_root — traversal guard tripped), or
    /// `"error"` (canonicalize / remove_dir_all error).
    pub folder_status: &'static str,
    /// Detail for `folder_status`: the canonical removed path, the
    /// outside-root resolution string, the error message, etc.
    pub folder_detail: String,
    /// `"skipped"` (no Jellyfin client configured), `"refreshed"`, or
    /// `"error"`. Refresh runs whenever the cleanup actually does
    /// work (delete_files=true); `"skipped"` is the no-Jellyfin
    /// posture, distinct from "we didn't ask."
    pub jellyfin_status: &'static str,
}

impl SeriesCleanupReport {
    /// `true` when the cleanup completed without any partial-failure
    /// signal — every torrent client accepted the delete and the
    /// folder either removed cleanly, was already missing, or was
    /// skipped (delete_files=false). Used by the bulk path to decide
    /// whether to count the series as `succeeded` or fold the partial
    /// signal into `BulkFailure.reason`. The per-row path doesn't use
    /// this — its JSON response always reports the full breakdown.
    pub fn is_clean(&self) -> bool {
        self.torrent_failures.is_empty()
            && matches!(
                self.folder_status,
                "recycled" | "removed" | "missing" | "skipped"
            )
    }

    /// One-line summary for the bulk failure-modal copy. Only called
    /// when `is_clean()` is false; assembles the partial-failure
    /// reason out of the substage signals so the user sees what
    /// didn't get cleaned. The reason rides back to the frontend as
    /// JSON via [`BulkOutcome`](super::bulk::BulkOutcome) — no
    /// HX-Trigger involvement — so any UTF-8 the filesystem can
    /// produce in `folder_detail` is fine to interpolate as-is.
    ///
    /// [`BulkOutcome`]: super::bulk::BulkOutcome
    pub fn partial_failure_reason(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match self.folder_status {
            "refused" => parts.push("folder refused (resolves outside media root)".to_string()),
            "error" => parts.push(format!("folder error: {}", self.folder_detail)),
            _ => {}
        }
        if !self.torrent_failures.is_empty() {
            parts.push(format!("torrent: {}", self.torrent_failures.join("; ")));
        }
        if parts.is_empty() {
            "partial cleanup failure".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// Per-series cleanup pass shared by [`crud::remove_series`] and
/// [`bulk::delete_one_series`]. Runs whenever the user asked for
/// `delete_files=true`; callers that don't want filesystem cleanup
/// shouldn't call this at all.
///
/// Order of operations matters and is the same as the original per-row
/// implementation:
///
/// 1. List grabs for the series; for each non-empty hash that doesn't
///    respect a still-active PT seed rule, resolve its download client
///    and call `client.delete(hash, with_files=true)`. After the client
///    delete, replay the import-time-stamped source paths so SAB jobs
///    whose `storage` field is the parent complete-dir get cleaned up
///    (the client's own delete is unreliable for those).
/// 2. Drop the `grabbed_torrents` rows for the series so the table
///    doesn't accumulate stale references to hashes the client just
///    forgot.
/// 3. Canonicalize `media_root`, canonicalize `media_root/<folder_name>`,
///    assert the resolved series dir starts with the resolved media
///    root, and only then hand it to `services::recycle::recycle`
///    (move into the recycle bin; `remove_dir_all` only when no bin is
///    configured, since an unwritable bin refuses the delete). The
///    `starts_with` guard is
///    the security-critical step — without it a corrupted `folder_name`
///    (`"../escape"`) lets the user wipe arbitrary directories.
///    Pinned by the `refuses_traversal_when_resolved_path_escapes_media_root`
///    test in `crud/tests.rs` for per-row and the
///    `bulk_delete_refuses_traversal_when_resolved_path_escapes_media_root`
///    test in `bulk.rs` for bulk.
/// 4. Nudge Jellyfin to rescan so the user's library doesn't show
///    ghost entries until the next refresh tick. Best-effort; logged
///    on failure but doesn't gate cleanup success.
///
/// The DB cascade (`series::remove`) is **not** part of this helper —
/// callers run it after this returns, ordered last because it's the
/// irreversible step.
///
/// [`crud::remove_series`]: super::crud::remove_series
/// [`bulk::delete_one_series`]: super::bulk
pub async fn cleanup_series_files(
    state: &AppState,
    series_id: i64,
    folder_name: &str,
    series_title: &str,
    media_root: Option<&str>,
    recycle_bin_path: &str,
) -> Result<SeriesCleanupReport, String> {
    let mut report = SeriesCleanupReport {
        torrents_removed: 0,
        torrent_failures: Vec::new(),
        folder_status: "skipped",
        folder_detail: String::new(),
        jellyfin_status: "skipped",
    };

    // 1. Per-grab download client cleanup. List-grabs failure is the
    //    one preflight DB error that bubbles to the caller; everything
    //    after this point folds errors into the report. Per-row's
    //    handler turns the bubble into a 500 + log row; bulk turns it
    //    into a BulkFailure entry. Letting one failed-DB-list silently
    //    skip cleanup would leave the user wondering why "Delete with
    //    files" left their on-disk folder intact.
    let hashes = grabbed_torrents::get_all_for_series(&state.db, series_id)
        .await
        .map_err(|e| format!("list_grabbed_torrents: {e}"))?;

    for &(grab_id, ref hash, dc_id) in &hashes {
        if hash.is_empty() {
            continue;
        }
        // Issue #28 — preserve PT seed rules across removal. A
        // user wiping a series typically wants ratio policies
        // honored; the `grabbed_torrents` row gets dropped below
        // either way (delete_all_for_series), so the upgrade sweep
        // can't re-grab the same hash.
        if grabbed_torrents::respects_seed_rules(&state.db, hash).await {
            report.torrents_removed += 1;
            continue;
        }
        // Every hash the series ever had used to be deleted with its
        // data, including one another series now downloads (a cancelled
        // grab here, re-grabbed there) or a batch that also feeds a
        // sibling. That download is the other series' to remove.
        if grabbed_torrents::hash_in_use_elsewhere(&state.db, hash, series_id, grab_id).await {
            tracing::info!(
                grab_id,
                "series delete: leaving torrent {hash} in the client, another series uses it"
            );
            continue;
        }
        let Some(client) = state.resolve_grab_client(dc_id, hash).await else {
            report
                .torrent_failures
                .push("Download client not configured".to_string());
            continue;
        };
        match client.delete(hash, true).await {
            Ok(()) => report.torrents_removed += 1,
            Err(err) => report.torrent_failures.push(format!("{}: {}", hash, err)),
        }

        // SAB stamped-source-path cleanup. Same rationale as
        // `delete_episode_file`: the client's delete is unreliable
        // for SAB jobs whose history `storage` field is the parent
        // complete dir. Stamped paths are precise and mode-agnostic.
        let stamped = grabbed_torrents::get_imported_source_paths(&state.db, grab_id).await;
        if !stamped.is_empty() {
            super::episodes::remove_stamped_source_paths(&stamped).await;
        }
    }

    // 2. Drop grabbed_torrents rows so they don't dangle.
    if let Err(err) = grabbed_torrents::delete_all_for_series(&state.db, series_id).await {
        report
            .torrent_failures
            .push(format!("clear grabbed_torrents: {}", err));
    }

    // 3. Canonicalize+starts_with-gated folder removal. media_root is
    //    None when the caller decided to skip filesystem cleanup
    //    (config not loadable, or empty media_root). folder_name
    //    being empty also short-circuits — we have no folder to
    //    delete.
    if let Some(root) = media_root
        && !root.trim().is_empty()
        && !folder_name.trim().is_empty()
        && !crate::services::media::usable_folder_name(folder_name)
    {
        // `.`, `..` or a path: never joined onto the media root.
        report.folder_status = "refused";
        report.folder_detail = format!("unusable folder name {folder_name:?}");
    } else if let Some(root) = media_root
        && !root.trim().is_empty()
        && !folder_name.trim().is_empty()
    {
        let series_dir = std::path::Path::new(root).join(folder_name);
        match tokio::fs::canonicalize(root).await {
            Ok(media_root_canon) => match tokio::fs::canonicalize(&series_dir).await {
                // Strictly inside the media root: a folder that resolves
                // to the root itself (a `.` folder name, a symlink to the
                // root) would take every series with it.
                Ok(series_canon)
                    if series_canon.starts_with(&media_root_canon)
                        && series_canon != media_root_canon =>
                {
                    // Recycle bin (#123): the folder moves into the bin
                    // when one is configured; with no bin `recycle` does
                    // the permanent `remove_dir_all` this branch used to
                    // call directly, and an unwritable bin refuses (Err).
                    match recycle::recycle(
                        &state.db,
                        recycle_bin_path,
                        RecycleKind::SeriesFolder,
                        Some(series_id),
                        series_title,
                        &series_canon,
                    )
                    .await
                    {
                        Ok(RecycleOutcome::Recycled { entry_id }) => {
                            report.folder_status = "recycled";
                            report.folder_detail =
                                format!("{} (recycle entry {})", series_canon.display(), entry_id);
                        }
                        Ok(RecycleOutcome::DirectDeleted) => {
                            report.folder_status = "removed";
                            report.folder_detail = series_canon.display().to_string();
                        }
                        Ok(RecycleOutcome::Missing) => {
                            report.folder_status = "missing";
                            report.folder_detail = series_canon.display().to_string();
                        }
                        Err(err) => {
                            report.folder_status = "error";
                            report.folder_detail = format!("{}: {}", series_canon.display(), err);
                        }
                    }
                }
                Ok(other) => {
                    report.folder_status = "refused";
                    report.folder_detail = format!(
                        "resolves outside media root: {} -> {}",
                        series_dir.display(),
                        other.display()
                    );
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    report.folder_status = "missing";
                    report.folder_detail = series_dir.display().to_string();
                }
                Err(err) => {
                    report.folder_status = "error";
                    report.folder_detail = format!("{}: {}", series_dir.display(), err);
                }
            },
            Err(err) => {
                report.folder_status = "error";
                report.folder_detail = format!("media_root canonicalize: {}", err);
            }
        }
    }

    // 4. Nudge Jellyfin. Best-effort.
    let jellyfin_opt = state.jellyfin.read().await.clone();
    if let Some(jelly) = jellyfin_opt {
        report.jellyfin_status = match jelly.refresh_library().await {
            Ok(()) => "refreshed",
            Err(_) => "error",
        };
    }

    Ok(report)
}
