//! Episode-file management endpoints.
//!
//! Split out of `handlers::library::mod` for readability — these handlers
//! share the per-episode action surface (delete, cancel, grab history,
//! mark-failed, progress poll, JSON snapshot) and depend only on a small
//! set of resolver + builder helpers in the parent module.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderValue,
    response::{IntoResponse, Response},
};
use axum_htmx::HxRequest;
use serde::Serialize;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, grabbed_torrents};
use crate::services::recycle::{self, RecycleKind, RecycleOutcome};
use crate::services::{auto_search, logger, media};

use super::pages::build_episodes;
use super::reconcile::{resolve_series_context, resolve_tracked_series};
use super::search::run_auto_search_targets;
use super::{Episode, MarkEpisodeFailedForm};

/// Walk `root` recursively (depth cap matches `walk_video_files` in
/// post_processing — 4 levels) and remove any regular file whose
/// inode equals `inode`. Returns the list of removed paths so the
/// caller can log them. Best-effort: I/O errors during walk or
/// remove are swallowed because this is a cleanup pass after the
/// authoritative media-side delete has already succeeded.
///
/// Used by `delete_episode_file` to scrub SAB-side hardlink sources
/// when the client's own `del_files=1` doesn't reach them — the
/// shared-inode property of hardlinks gives us a reliable way to
/// find the source regardless of how SAB reports its history
/// `storage` field. After file removal, walks back up cleaning out
/// any directories that became empty as a result, stopping at
/// `root` so we never empty out the configured complete dir.
#[cfg(unix)]
async fn remove_hardlinks_with_inode(
    root: &std::path::Path,
    inode: u64,
) -> Vec<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;

    fn walk(dir: &std::path::Path, depth: u32, inode: u64, out: &mut Vec<std::path::PathBuf>) {
        const MAX_DEPTH: u32 = 4;
        if depth > MAX_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                walk(&path, depth + 1, inode, out);
            } else if meta.is_file() && meta.ino() == inode {
                out.push(path);
            }
        }
    }

    let root = root.to_path_buf();
    let inode_arg = inode;
    tokio::task::spawn_blocking(move || {
        let mut found = Vec::new();
        walk(&root, 0, inode_arg, &mut found);
        let mut removed = Vec::new();
        for p in found {
            if std::fs::remove_file(&p).is_ok() {
                removed.push(p);
            }
        }
        // Walk back up cleaning out emptied parents. Stop at `root`
        // (don't try to rmdir the root itself — that's the user's
        // configured complete dir).
        let root_canon = std::fs::canonicalize(&root).unwrap_or(root.clone());
        for p in &removed {
            let mut parent = p.parent();
            while let Some(dir) = parent {
                if let Ok(canon) = std::fs::canonicalize(dir)
                    && canon == root_canon
                {
                    break;
                }
                if std::fs::remove_dir(dir).is_err() {
                    // Non-empty or permission denied — stop ascending.
                    break;
                }
                parent = dir.parent();
            }
        }
        removed
    })
    .await
    .unwrap_or_default()
}

#[cfg(not(unix))]
async fn remove_hardlinks_with_inode(
    _root: &std::path::Path,
    _inode: u64,
) -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// Check whether `path` is a regular file with the given inode.
/// Used by the orphan-source-cleanup fallback in
/// `delete_episode_file`: when a grab's stamped paths contain a file
/// matching the just-deleted media file's inode, that's the SAB
/// source we want to remove (regardless of which grab "officially"
/// claims the episode).
#[cfg(unix)]
async fn path_has_inode(path: &str, inode: u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    let p = std::path::PathBuf::from(path);
    tokio::task::spawn_blocking(move || {
        std::fs::metadata(&p)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.ino() == inode)
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

#[cfg(not(unix))]
async fn path_has_inode(_path: &str, _inode: u64) -> bool {
    false
}

/// Shared with the post-import removal (issue #228); lives in
/// `services::post_processing::client_cleanup`.
pub(super) use crate::services::post_processing::remove_stamped_source_paths;

#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/delete-file/{episode_number}",
    tag = "Library",
    summary = "Delete episode file",
    description = "Delete the on-disk media file for a specific episode.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "File deleted", body = serde_json::Value),
        (status = 400, description = "Series not in library or no file found"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn delete_episode_file(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Response {
    // The route serves both the legacy JSON shape (POST `/api/...` from
    // a non-htmx caller — kept for any external programmatic consumer)
    // and the migration's declarative shape (htmx `hx-post` from the
    // episode-detail modal). Empty body + an `HX-Trigger` header
    // carrying the result is the canonical "modal-footer button row
    // doesn't grow to fit the message" pattern from the indexer / DC
    // test handlers — the JS-side row update + toast then key off the
    // trigger event.
    let json_err = |status: axum::http::StatusCode, msg: &str| -> Response {
        if is_htmx {
            episode_delete_trigger(status, episode_number, false, msg, None)
        } else {
            let body = Json(serde_json::json!({"ok": false, "message": msg}));
            (status, body).into_response()
        }
    };

    let (tracked_row, _, _detail) = match resolve_series_context(&state.db, request_id).await {
        Ok(v) => v,
        Err(e) => return json_err(axum::http::StatusCode::BAD_GATEWAY, &e),
    };

    let tracked = match tracked_row {
        Some(t) => t,
        None => return json_err(axum::http::StatusCode::BAD_REQUEST, "Series not in library"),
    };

    let cfg = match config::get_config(&state.db).await.ok().flatten() {
        Some(c) => c,
        None => return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "No config"),
    };

    let files = media::scan_series_folder(&cfg.media_root, &tracked.folder_name).await;
    let target = files.iter().find(|f| f.episode_number == episode_number);

    match target {
        None => json_err(
            axum::http::StatusCode::NOT_FOUND,
            "Episode file not found on disk",
        ),
        Some(file) => {
            let series_dir = std::path::Path::new(&cfg.media_root).join(&tracked.folder_name);
            let full_path = series_dir.join(&file.filename);

            // Canonicalize and verify the resolved path is still inside
            // the configured media root.
            let media_root_canon = match tokio::fs::canonicalize(&cfg.media_root).await {
                Ok(p) => p,
                Err(e) => {
                    return json_err(
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("Failed to resolve media root: {}", e),
                    );
                }
            };
            let full_path_canon = match tokio::fs::canonicalize(&full_path).await {
                Ok(p) => p,
                Err(e) => {
                    return json_err(
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("Failed to resolve file: {}", e),
                    );
                }
            };
            if !full_path_canon.starts_with(&media_root_canon) {
                logger::warn(
                    &state.db,
                    LogCategory::Library,
                    "Refused to delete file outside media root",
                    &format!(
                        "series_id={}, requested={}, resolved={}, media_root={}",
                        tracked.id,
                        full_path.display(),
                        full_path_canon.display(),
                        media_root_canon.display()
                    ),
                )
                .await;
                return json_err(
                    axum::http::StatusCode::BAD_REQUEST,
                    "File resolves outside media root",
                );
            }

            // Capture the media file's inode BEFORE removal. In
            // hardlink import mode the SAB-side source shares this
            // inode; we use it to find and remove the source after
            // the media-side hardlink is gone. Necessary because
            // SAB's `del_files=1` is unreliable when its history
            // entry's `storage` field points at the parent complete
            // dir (SAB refuses to recursively rm the whole complete
            // root) — leaving the original .mkv intact in
            // `complete/<job-folder>/`.
            #[cfg(unix)]
            let media_inode = {
                use std::os::unix::fs::MetadataExt;
                tokio::fs::metadata(&full_path_canon)
                    .await
                    .ok()
                    .map(|m| m.ino())
            };
            #[cfg(not(unix))]
            let media_inode: Option<u64> = None;

            // Recycle bin (#123): the video plus its companions (`.nfo`,
            // subtitles, thumbnail) move into the bin together; with no
            // bin configured `recycle` unlinks them permanently, which is
            // what this handler did before (minus the companion sweep,
            // which used to leak subtitles).
            let recycle_entry_id: Option<String> = match recycle::recycle(
                &state.db,
                &cfg.recycle_bin_path,
                RecycleKind::Episode,
                Some(tracked.id),
                &tracked.title,
                &full_path_canon,
            )
            .await
            {
                Ok(RecycleOutcome::Recycled { entry_id }) => Some(entry_id),
                Ok(RecycleOutcome::DirectDeleted) => None,
                Ok(RecycleOutcome::Missing) => {
                    return json_err(
                        axum::http::StatusCode::NOT_FOUND,
                        "Episode file not found on disk",
                    );
                }
                Err(e) => {
                    return json_err(
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("Failed to delete file: {}", e),
                    );
                }
            };

            let _ = episode_tags::clear_episode_tag(&state.db, tracked.id, episode_number).await;
            // `clear_episode_tag` only touches `episode_quality_tags`;
            // it leaves the `episode_grab_history` row untouched. After
            // a delete the latest history entry for this episode should
            // flip from `completed` (or `grabbed` if post-processing
            // hadn't landed yet) to `removed` so the Grab History modal
            // reflects the deletion. Without this call the modal kept
            // showing the stale `completed` state indefinitely while
            // the file was already gone from disk.
            let _ = episode_tags::mark_grab_history_removed(&state.db, tracked.id, episode_number)
                .await;

            let imported_grabs =
                grabbed_torrents::find_imported_for_episode(&state.db, tracked.id, episode_number)
                    .await
                    .unwrap_or_default();

            // Inode fallback when no grab claims this episode. Pre-fix
            // wide-walk SAB grabs swept in stranger episodes from
            // sibling SAB jobs and stamped them onto whichever grab's
            // import was running; the grab's `episode_numbers` only
            // covers its OWN release, so `find_imported_for_episode`
            // returns empty for the over-imported episode even though
            // a real stamped path for it exists. Walk every imported
            // grab in this series, decode stamped JSON, find a path
            // whose inode matches the just-deleted media file, remove
            // it. Defense-in-depth — most grabs after the
            // `canonical_job_path` fix won't need this.
            if imported_grabs.is_empty()
                && let Some(ino) = media_inode
            {
                let bundles =
                    grabbed_torrents::imported_source_paths_for_series(&state.db, tracked.id)
                        .await
                        .unwrap_or_default();
                // Flatten every grab's stamps into one path list; for
                // a series with N imported grabs and ~M stamps each,
                // this is N×M paths to stat. Run the inode probes in
                // parallel via `buffer_unordered(8)` so a series with
                // a deep history doesn't serialize ~hundreds of
                // `spawn_blocking` round-trips through the blocking
                // pool while the user waits on the delete handler.
                let mut all_paths: Vec<String> = Vec::new();
                for (_grab_id, json) in &bundles {
                    let paths: Vec<String> = serde_json::from_str(json).unwrap_or_default();
                    all_paths.extend(paths);
                }
                use futures_util::StreamExt;
                let targets: Vec<String> = futures_util::stream::iter(all_paths)
                    .map(|p| async move {
                        if path_has_inode(&p, ino).await {
                            Some(p)
                        } else {
                            None
                        }
                    })
                    .buffer_unordered(8)
                    .filter_map(|opt| async move { opt })
                    .collect()
                    .await;
                if !targets.is_empty() {
                    let removed = remove_stamped_source_paths(&targets).await;
                    if !removed.is_empty() {
                        logger::info(
                            &state.db,
                            LogCategory::PostProcess,
                            &format!(
                                "Removed {} orphan source file(s) for episode {} via inode fallback",
                                removed.len(),
                                episode_number
                            ),
                            &removed
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", "),
                        )
                        .await;
                    }
                }
            }

            let mut qbit_removed: Vec<String> = Vec::new();
            if !imported_grabs.is_empty() {
                // Per-grab client routing via `resolve_grab_client`:
                // prefers the stamped `download_client_id`, falls back
                // through a SAB-nzo_id-shape heuristic for legacy
                // grabs (NULL stamp from before grab-time stamping
                // was wired), then to the torrent default. Without
                // the heuristic an old SAB grab routes to qBit's
                // delete, qBit 200s on the unknown hash, and the SAB
                // job survives.
                for grab in &imported_grabs {
                    if grab.is_batch {
                        continue;
                    }
                    if grab.hash.is_empty() {
                        continue;
                    }
                    // Issue #28 — skip the client-side
                    // delete for grabs from a PT indexer with
                    // seed rules in effect; the client owns
                    // when seeding ends. The grab row still
                    // gets `mark_removed` so the upgrade sweep
                    // doesn't re-grab.
                    if grabbed_torrents::respects_seed_rules(&state.db, &grab.hash).await {
                        logger::info(
                            &state.db,
                            LogCategory::DownloadClient,
                            &format!(
                                "Skipping client delete for {} (respect_seed_rules); client will stop on its own ratio policy",
                                grab.torrent_name
                            ),
                            &grab.hash,
                        )
                        .await;
                        let _ = grabbed_torrents::mark_removed(&state.db, grab.id).await;
                        continue;
                    }
                    let Some(client) = state
                        .resolve_grab_client(grab.download_client_id, &grab.hash)
                        .await
                    else {
                        continue;
                    };
                    let delete_result = client.delete(&grab.hash, true).await;

                    // Belt-and-suspenders source-side cleanup. The
                    // client's own `delete(hash, true)` is unreliable
                    // for SAB (its `del_files=1` no-ops when its
                    // history `storage` field is the parent complete
                    // dir). Two complementary local-fs cleanup paths
                    // run alongside the client delete:
                    //
                    //   1. **Stamped source paths** —
                    //      `import_torrent` records the exact paths
                    //      it imported FROM. Direct, mode-agnostic
                    //      (works for hardlink, copy, and move —
                    //      though move's source is already gone).
                    //      Primary cleanup channel for fresh grabs.
                    //   2. **Inode-based fallback** — for legacy
                    //      grabs imported before stamping was wired
                    //      (NULL `imported_source_paths`). Hardlink
                    //      mode shares inodes between media-side and
                    //      SAB-side files; we use the media inode
                    //      (captured pre-deletion) to find the
                    //      surviving SAB-side hardlink under the
                    //      grab's `client_content_path`. Doesn't fire
                    //      for legacy copy-mode grabs — those have
                    //      no recovery path short of the user
                    //      cleaning SAB by hand.
                    let stamped_sources =
                        grabbed_torrents::get_imported_source_paths(&state.db, grab.id).await;
                    if !stamped_sources.is_empty() {
                        let removed = remove_stamped_source_paths(&stamped_sources).await;
                        if !removed.is_empty() {
                            logger::info(
                                &state.db,
                                LogCategory::PostProcess,
                                &format!(
                                    "Removed {} source file(s) for episode {} via import-time stamps",
                                    removed.len(),
                                    episode_number
                                ),
                                &removed
                                    .iter()
                                    .map(|p| p.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            )
                            .await;
                        }
                    } else if let Some(ino) = media_inode {
                        // Live-query the client for this hash's
                        // canonical content_path. The stamped
                        // `client_content_path` on legacy grabs
                        // pre-dates our title-matching narrowing
                        // (`canonical_job_path` in
                        // `services::download_client::sabnzbd`)
                        // and may still be the parent complete dir.
                        // `list_scoped` runs the narrowing fresh, so
                        // its `content_path` is the per-job folder.
                        let live_path = match client.list_scoped().await {
                            Ok(items) => items
                                .into_iter()
                                .find(|i| i.hash == grab.hash)
                                .map(|i| i.content_path),
                            Err(_) => None,
                        };
                        let stamped_path =
                            grabbed_torrents::get_client_content_path(&state.db, grab.id).await;
                        let content_path = live_path
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or(stamped_path);
                        if !content_path.trim().is_empty() {
                            let local = crate::services::download_client::translate_client_path(
                                &content_path,
                                &content_path,
                                crate::services::download_client::per_client_download_path(&cfg),
                            );
                            let candidate = if local.is_empty() {
                                content_path
                            } else {
                                local
                            };
                            let removed =
                                remove_hardlinks_with_inode(std::path::Path::new(&candidate), ino)
                                    .await;
                            if !removed.is_empty() {
                                logger::info(
                                    &state.db,
                                    LogCategory::PostProcess,
                                    &format!(
                                        "Removed {} source hardlink(s) for episode {} via inode match in '{}'",
                                        removed.len(),
                                        episode_number,
                                        candidate
                                    ),
                                    &removed
                                        .iter()
                                        .map(|p| p.display().to_string())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                )
                                .await;
                            } else {
                                logger::warn(
                                    &state.db,
                                    LogCategory::PostProcess,
                                    &format!(
                                        "No source files matched inode for episode {} under '{}'",
                                        episode_number, candidate
                                    ),
                                    &format!("inode={ino} hash={}", grab.hash),
                                )
                                .await;
                            }
                        }
                    }

                    // Mark the grab removed regardless of the
                    // client's delete result. The user explicitly
                    // asked for deletion, the media file IS gone,
                    // and the source-side cleanup above already
                    // ran. Gating `mark_removed` on `client.delete`
                    // returning `Ok` left the row stuck in
                    // 'imported' state any time SAB returned a
                    // non-success on housekeeping (e.g. the nzo_id
                    // had already been purged from history by SAB's
                    // own retention policy, or a stale-state grab
                    // from a prior aborted session). The UI then
                    // showed the episode as still imported even
                    // though it was actually deleted on disk.
                    match delete_result {
                        Ok(()) => {
                            qbit_removed.push(grab.torrent_name.clone());
                        }
                        Err(e) => {
                            logger::warn(
                                &state.db,
                                LogCategory::DownloadClient,
                                &format!(
                                    "Download client delete returned an error for episode {} torrent '{}' — proceeding with grab cleanup anyway",
                                    episode_number, grab.torrent_name
                                ),
                                &e,
                            )
                            .await;
                        }
                    }
                    let _ = grabbed_torrents::mark_removed(&state.db, grab.id).await;
                }
            }

            logger::info(
                &state.db,
                LogCategory::Library,
                &format!("Deleted episode {} file: {}", episode_number, file.filename),
                &format!(
                    "series_id={}, path={}, qbit_removed={}",
                    tracked.id,
                    full_path_canon.display(),
                    qbit_removed.len()
                ),
            )
            .await;

            if is_htmx {
                // The toast uses this as its body under a title that already
                // names the episode, so say what's actionable rather than
                // repeating the title.
                let mut msg = if recycle_entry_id.is_some() {
                    "Restorable from the Recycle Bin until it is purged.".to_string()
                } else {
                    format!("Episode {} file removed.", episode_number)
                };
                if !qbit_removed.is_empty() {
                    msg.push_str(&format!(
                        " {} torrent(s) removed from client.",
                        qbit_removed.len()
                    ));
                }
                episode_delete_trigger(
                    axum::http::StatusCode::OK,
                    episode_number,
                    true,
                    &msg,
                    recycle_entry_id.as_deref(),
                )
            } else {
                (
                    axum::http::StatusCode::OK,
                    Json(serde_json::json!({
                        "ok": true,
                        "deleted": file.filename,
                        "qbit_removed": qbit_removed,
                        "recycle_entry_id": recycle_entry_id,
                    })),
                )
                    .into_response()
            }
        }
    }
}

/// Build the HTMX response for `delete_episode_file`. Mirrors the
/// `indexer_test_trigger` / `dc_test_result_response` shape: an empty
/// body (so the modal-footer button row doesn't reflow to fit the
/// message) plus an `HX-Trigger` header carrying a JSON payload. The
/// frontend listener in `static/js/series.js` (`ryokan-episode-deleted`)
/// reads `event.detail` and runs the row update plus the toast.
///
/// **ASCII-only payload, deliberately**. HTTP headers carry no charset
/// metadata, so any non-ASCII byte round-trips as Latin-1 and either
/// produces mojibake in the toast or — worse — makes htmx fail to
/// parse the header value as JSON, in which case the `CustomEvent`
/// never fires and the JS listener never runs (file deletes silently
/// without the row stamp or toast). Same constraint as the indexer /
/// DC test handlers; recorded in the `feedback_hx_trigger_ascii_only`
/// memory. We **deliberately do NOT echo torrent names back** — group
/// tags / show titles regularly contain em-dashes, kanji, or other
/// non-ASCII; the count is enough for the toast wording. Errors from
/// upstream services flow through `message`, so the helper sanitizes
/// it to ASCII as a defensive step.
/// `recycle_entry_id` is the 8-hex recycle bin entry when the file was
/// recycled (#123); the page's toast turns it into a one-click Undo. It
/// is always ASCII, so it can't break the header-safety argument below.
fn episode_delete_trigger(
    status: axum::http::StatusCode,
    episode_number: i32,
    ok: bool,
    message: &str,
    recycle_entry_id: Option<&str>,
) -> Response {
    let safe_message: String = message
        .chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect();
    let payload = serde_json::json!({
        "ryokan-episode-deleted": {
            "ok": ok,
            "episode_number": episode_number,
            "message": safe_message,
            "recycle_entry_id": recycle_entry_id,
        }
    });
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = status;
    // The parse can't actually fail: `safe_message` is ASCII-only by
    // construction (the `c.is_ascii()` filter above), `episode_number`
    // serializes to an integer literal, `ok` to a bool literal — so
    // the resulting JSON is guaranteed to be valid HTTP-header bytes.
    // `.expect()` is honest about that contract; the prior fallback
    // (degrade to bare event-name) silently produced an `event.detail
    // = "ryokan-episode-deleted"` *string* on the JS side, and the
    // listener would land in its error branch with "Unknown error"
    // copy. Better to crash loudly than to ship a parse-failure path
    // that's reachable by no real input.
    let header_value: HeaderValue = payload
        .to_string()
        .parse()
        .expect("ASCII-sanitized JSON must parse as a HeaderValue");
    resp.headers_mut().insert("HX-Trigger", header_value);
    resp
}

/// Cancel an in-flight grab for an episode: remove the torrent from
/// qBittorrent (with its partial/complete data), mark the grab row as
/// 'removed', and clear the episode's quality tag so it returns to the
/// missing state.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/cancel-pending/{episode_number}",
    tag = "Library",
    summary = "Cancel pending episode grab",
    description = "Remove the in-flight torrent from qBittorrent, mark the grab as removed, and clear the episode's quality tag. Does not trigger a re-search.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "Pending grab cancelled", body = serde_json::Value),
        (status = 400, description = "Series not in library"),
        (status = 404, description = "No pending grab found for this episode"),
    ),
)]
pub async fn cancel_pending_episode(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let json_err = |status: axum::http::StatusCode, msg: &str| {
        (
            status,
            Json(serde_json::json!({"ok": false, "message": msg})),
        )
    };

    let tracked = match resolve_tracked_series(&state.db, request_id).await {
        Ok(Some(t)) => t,
        Ok(None) => return json_err(axum::http::StatusCode::BAD_REQUEST, "Series not in library"),
        Err(e) => {
            return json_err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            );
        }
    };

    let mut pending =
        match grabbed_torrents::find_pending_for_episode(&state.db, tracked.id, episode_number)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                return json_err(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    &e.to_string(),
                );
            }
        };

    // Drift case: `grabbed_torrents.state = 'imported'` but the
    // `episode_quality_tags` row for this episode is still 'grabbed'.
    // We fold these in via `find_imported_for_episode` only when the
    // episode's tag actually says 'grabbed'.
    let tag_state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
    )
    .bind(tracked.id)
    .bind(episode_number)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let tag_is_grabbed = matches!(tag_state.as_deref(), Some("grabbed"));
    if tag_is_grabbed
        && let Ok(stuck) =
            grabbed_torrents::find_imported_for_episode(&state.db, tracked.id, episode_number).await
    {
        let tag_state_recheck: Option<String> = sqlx::query_scalar(
            "SELECT state FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
        )
        .bind(tracked.id)
        .bind(episode_number)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        if matches!(tag_state_recheck.as_deref(), Some("grabbed")) {
            let seen: std::collections::HashSet<i64> = pending.iter().map(|g| g.id).collect();
            for g in stuck {
                if !seen.contains(&g.id) {
                    pending.push(g);
                }
            }
        } else {
            tracing::debug!(
                target: "ryokan::library",
                series_id = tracked.id,
                episode = episode_number,
                tag_state_now = ?tag_state_recheck,
                "cancel_pending_episode: tag flipped away from 'grabbed' mid-handler — skipping drift-repair branch"
            );
        }
    }

    if pending.is_empty() {
        if tag_is_grabbed {
            let _ = episode_tags::clear_tags_for_removal(&state.db, tracked.id, &[episode_number])
                .await;
            return (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "cancelled": 0,
                    "torrent_failures": Vec::<String>::new(),
                    "note": "Tag cleared; no associated torrent was found.",
                })),
            );
        }
        return json_err(
            axum::http::StatusCode::NOT_FOUND,
            "No pending grab found for this episode",
        );
    }

    tracing::debug!(
        target: "ryokan::library",
        series_id = tracked.id,
        episode = episode_number,
        grab_count = pending.len(),
        grab_ids = ?pending.iter().map(|g| g.id).collect::<Vec<_>>(),
        grab_names = ?pending.iter().map(|g| g.torrent_name.clone()).collect::<Vec<_>>(),
        batch_grabs = ?pending.iter().filter(|g| g.episode_numbers.len() > 1).map(|g| g.id).collect::<Vec<_>>(),
        tag_was_stuck_grabbed = tag_is_grabbed,
        "cancel_pending_episode: matching grabs"
    );

    let mut removed_count = 0;
    let mut torrent_failures: Vec<String> = Vec::new();
    for grab in &pending {
        if !grab.hash.is_empty() {
            // Per-grab client routing — see `resolve_grab_client` for
            // the SAB-nzo_id heuristic that rescues legacy NULL stamps.
            let client_for_grab = state
                .resolve_grab_client(grab.download_client_id, &grab.hash)
                .await;
            if let Some(client) = client_for_grab
                && let Err(e) = client.delete(&grab.hash, true).await
            {
                torrent_failures.push(format!("{}: {}", grab.torrent_name, e));
                logger::warn(
                    &state.db,
                    LogCategory::DownloadClient,
                    &format!(
                        "Failed to remove pending torrent for S?E{:02} cancel: '{}'",
                        episode_number, grab.torrent_name
                    ),
                    &e,
                )
                .await;
            }
        }

        if let Err(e) = grabbed_torrents::mark_removed(&state.db, grab.id).await {
            logger::warn(
                &state.db,
                LogCategory::Library,
                &format!(
                    "Failed to mark grab {} as removed during cancel for S?E{:02}",
                    grab.id, episode_number
                ),
                &e.to_string(),
            )
            .await;
        } else {
            removed_count += 1;
        }
    }

    let _ = episode_tags::clear_tags_for_removal(&state.db, tracked.id, &[episode_number]).await;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Cancelled pending grab for episode {}", episode_number),
        &format!(
            "series_id={}, cancelled={}, qbit_failures={}",
            tracked.id,
            removed_count,
            torrent_failures.len()
        ),
    )
    .await;

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "cancelled": removed_count,
            "torrent_failures": torrent_failures,
        })),
    )
}

/// Get grab history for a specific episode.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/grab-history/{episode_number}",
    tag = "Library",
    summary = "Get episode grab history",
    description = "Returns the grab history for a specific episode, including quality tags and release info.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "Grab history entries", body = Vec<episode_tags::GrabHistoryEntry>),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn get_episode_grab_history(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Result<Json<Vec<episode_tags::GrabHistoryEntry>>, (axum::http::StatusCode, String)> {
    let series_id = resolve_tracked_series(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            axum::http::StatusCode::BAD_REQUEST,
            "Series not in library".to_string(),
        ))?
        .id;

    let history = episode_tags::get_grab_history(&state.db, series_id, episode_number)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(history))
}

/// Mark a grab as failed and re-trigger auto-search for the episode.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/mark-failed/{episode_number}",
    tag = "Library",
    summary = "Mark episode grab as failed",
    description = "Mark a grabbed episode as failed and optionally blocklist it, then re-search for a replacement.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    request_body = MarkEpisodeFailedForm,
    responses(
        (status = 200, description = "Re-search report", body = auto_search::AutoSearchReport),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn mark_episode_failed(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Json(form): Json<MarkEpisodeFailedForm>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row
        .ok_or((
            axum::http::StatusCode::BAD_REQUEST,
            "Series not in library".to_string(),
        ))?
        .id;

    let (_sid, _ep, release_title) = episode_tags::mark_grab_failed(&state.db, form.history_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if form.blocklist && !release_title.is_empty() {
        let _ = grabbed_torrents::mark_failed_by_name(&state.db, series_id, &release_title).await;
    }

    if let Ok(old_grabs) =
        grabbed_torrents::find_imported_for_episode(&state.db, series_id, episode_number).await
        && !old_grabs.is_empty()
    {
        for old in &old_grabs {
            if old.hash.is_empty() {
                continue;
            }
            // Issue #28 — preserve PT seed rules across
            // episode-replace. The old torrent has already
            // imported successfully and is seeding to its
            // per-tracker ratio/time policy; deleting it
            // mid-seed could ding the user's tracker ratio.
            if grabbed_torrents::respects_seed_rules(&state.db, &old.hash).await {
                crate::services::logger::info(
                    &state.db,
                    crate::models::log::LogCategory::DownloadClient,
                    &format!(
                        "Skipping client delete for replaced torrent {} (respect_seed_rules)",
                        old.torrent_name
                    ),
                    &old.hash,
                )
                .await;
                continue;
            }
            // Per-grab client routing — see `resolve_grab_client`.
            // Cross-protocol upgrade (SAB→qBit etc) lands here.
            let Some(client) = state
                .resolve_grab_client(old.download_client_id, &old.hash)
                .await
            else {
                continue;
            };
            if let Err(e) = client.delete(&old.hash, true).await {
                crate::services::logger::warn(
                    &state.db,
                    crate::models::log::LogCategory::DownloadClient,
                    &format!(
                        "Failed to remove old torrent for S?E{:02} replacement: '{}'",
                        episode_number, old.torrent_name
                    ),
                    &e,
                )
                .await;
            }
        }
    }

    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        run_auto_search_targets(
            &state_clone,
            request_id,
            vec![target],
            false,
            Some(series_id),
        )
        .await
    });
    let report = handle.await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Search task failed: {}", e),
        )
    })??;

    Ok(Json(report))
}

/// Returns download progress for episodes of a series that are currently downloading.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/download-progress",
    tag = "Library",
    summary = "Episode download progress",
    description = "Returns download progress for all actively downloading episodes of a series.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Download progress per episode", body = Vec<EpisodeProgress>),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn episode_download_progress(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<Vec<EpisodeProgress>>, (axum::http::StatusCode, String)> {
    let tracked = resolve_tracked_series(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            axum::http::StatusCode::BAD_REQUEST,
            "Series not in library".to_string(),
        ))?;

    let pending = crate::models::grabbed_torrents::get_all_pending(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if pending.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let grab_ids: Vec<i64> = pending.iter().map(|g| g.id).collect();
    let routes_by_grab =
        crate::models::grabbed_torrents::get_series_routes_for_grabs(&state.db, &grab_ids)
            .await
            .unwrap_or_default();

    // Fetch list_scoped from EVERY configured client and merge. The
    // pre-fix code called `state.default_download_client()` which
    // returns the torrent default specifically — fine for BT-only
    // setups but wrong as soon as SAB enters the picture, since SAB
    // grabs land on the usenet default and their `nzo_id` hashes
    // never appear in qBit's `list_scoped`. Symptom: SAB grabs got
    // marked "Torrent removed in download client" at the 30s stale
    // mark even when the SAB job was happily completing.
    //
    // Fetching from all clients is cheap (one HTTP call per client
    // per poll, typical homelab has 1-2 clients) and the merge
    // doesn't collide because BT v1 infohashes (40-char hex) and
    // SAB nzo_ids (SABnzbd_nzo_*) share no namespace.
    let pool = state.download_clients.read().await.clone();
    if pool.clients.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let mut all_torrents: Vec<crate::services::download_client::DownloadItem> = Vec::new();
    for (client_id, client) in pool.clients.iter() {
        match client.list_scoped().await {
            Ok(t) => all_torrents.extend(t),
            Err(err) => {
                // One client unreachable shouldn't blank the progress
                // surface for grabs on other clients. Log + continue.
                tracing::debug!(
                    "episode-progress poll: list_scoped failed for client #{}: {}",
                    client_id,
                    err
                );
            }
        }
    }
    let by_hash: HashMap<String, &crate::services::download_client::DownloadItem> = all_torrents
        .iter()
        .map(|t| (t.hash.to_lowercase(), t))
        .collect();

    let mut results = Vec::new();
    for grab in &pending {
        let routes = routes_by_grab.get(&grab.id);
        let ep_nums: Vec<i32> = match routes {
            Some(routes) if !routes.is_empty() => routes
                .iter()
                .filter(|r| r.series_id == tracked.id)
                .flat_map(|r| r.episode_numbers.iter().copied())
                .collect(),
            _ if grab.series_id == tracked.id => grab.episode_numbers.clone(),
            _ => continue,
        };
        if ep_nums.is_empty() {
            continue;
        }

        let torrent = if !grab.hash.is_empty() {
            by_hash.get(&grab.hash.to_lowercase()).copied()
        } else {
            None
        };

        let Some(t) = torrent else {
            if crate::services::post_processing::grab_is_stale(&grab.grabbed_at, 30) {
                logger::info(
                    &state.db,
                    LogCategory::DownloadClient,
                    &format!(
                        "Torrent removed in download client — reconciling '{}'",
                        grab.torrent_name
                    ),
                    &format!(
                        "series_id={} grab_id={} hash={}",
                        grab.series_id, grab.id, grab.hash
                    ),
                )
                .await;
                let _ = crate::models::grabbed_torrents::mark_removed(&state.db, grab.id).await;
                let _ = crate::models::episode_tags::clear_tags_for_removal(
                    &state.db,
                    grab.series_id,
                    &grab.episode_numbers,
                )
                .await;
            }
            continue;
        };

        for ep in ep_nums {
            results.push(EpisodeProgress {
                episode: ep,
                progress: t.progress,
                speed: t.dlspeed,
                state: t.state.clone(),
                state_kind: t.state_kind,
            });
        }
    }

    Ok(Json(results))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct EpisodeProgress {
    pub episode: i32,
    pub progress: f64,
    pub speed: i64,
    /// Client-native state string (qBit: `stalledUP`, Deluge: `Seeding`,
    /// Transmission: numeric code, rtorrent: computed). Kept for debug
    /// tooling; UI code should drive off `state_kind` for cross-client
    /// consistency.
    pub state: String,
    /// Normalized state slug from [`DownloadItemState`]. See its
    /// rendered form in the Downloads page state badges.
    pub state_kind: crate::services::download_client::DownloadItemState,
}

/// Returns the current episode state for a series as JSON.
///
/// Used by the series page's download-progress poller: when a torrent
/// disappears from the progress response (meaning the download completed and
/// the post-processing tick has moved the file into the library), the client
/// fetches this endpoint and patches the affected row in-place so the user
/// sees the new on-disk file without a full page refresh.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/episodes",
    tag = "Library",
    summary = "Episode state snapshot",
    description = "Returns the current list of episodes for a series, reflecting on-disk state.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Episode state", body = Vec<Episode>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn series_episodes_json(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<Vec<Episode>>, (axum::http::StatusCode, String)> {
    let (db_series, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let db_id = db_series.as_ref().map(|s| s.id);
    let folder_name = db_series
        .as_ref()
        .map(|s| s.folder_name.clone())
        .unwrap_or_default();

    let cfg = config::get_config(&state.db).await.ok().flatten();
    let media_root = cfg
        .as_ref()
        .map(|c| c.media_root.clone())
        .unwrap_or_default();

    let (episodes, _, _, _, _) =
        build_episodes(&state.db, &detail, db_id, &folder_name, &media_root).await;

    Ok(Json(episodes))
}

#[cfg(test)]
mod tests;
