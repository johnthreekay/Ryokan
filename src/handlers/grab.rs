//! Interactive file-picker endpoints (issue #83).
//!
//! The five endpoints here implement the modal lifecycle documented
//! in `models::pending_grabs` and issue #83's interactive file-picker plan.
//!
//! | Method | Path                               | Purpose                              |
//! |--------|------------------------------------|--------------------------------------|
//! | POST   | `/api/grab/preview`                | Add torrent paused, return preview_id |
//! | GET    | `/api/grab/preview/{preview_id}`   | Poll for file-list readiness         |
//! | POST   | `/api/grab/heartbeat/{preview_id}` | Modal keepalive (~30s cadence)       |
//! | POST   | `/api/grab/confirm`                | Apply user's selections + resume     |
//! | POST   | `/api/grab/cancel`                 | Internal/error-path delete           |
//!
//! The preview POST is non-blocking: it writes the `pending_grabs` row
//! with an empty `file_list_json` and spawns a background task that
//! calls `add_torrent_paused` then `get_files`, writing the result
//! back to the row via `set_file_list`. The modal sees `status:
//! fetching_metadata` on GET until the spawned task completes, then
//! `status: ready` with the file list. That asymmetric shape (fast
//! POST + polled GET) avoids holding a request handler thread for
//! the full metadata-fetch budget while keeping the API surface
//! straightforward — no long-poll, no SSE, no WebSocket.
//!
//! Routes are mounted in `main.rs`'s `protected_routes` block and
//! sit behind the cookie-auth + CSRF layer like every other
//! browser-facing endpoint. Curl-test flow: authenticate to get a
//! session cookie, then `curl -b cookies.txt -X POST .../api/grab/preview ...`.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::pending_grabs;
use crate::services::download_client::{self, AddOutcome};
use crate::services::grab_commit;

/// POST body for `/api/grab/preview`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GrabPreviewForm {
    /// Magnet URI or `http(s)://…/*.torrent` URL identifying the
    /// release. Required — used verbatim for the underlying
    /// `DownloadClient::add_torrent_paused` call.
    pub url: String,
    /// v1 info-hash (lowercase hex). Required for qBit-style paused-
    /// add workarounds and for the same-hash dedup check.
    pub info_hash: String,
    /// Target series id. Optional — present when the user triggered
    /// Grab from a specific series-page context. Kept nullable
    /// because a future bare-magnet grab flow may precede series
    /// selection.
    #[serde(default)]
    pub series_id: Option<i64>,
    /// Opaque JSON blob the modal renders in the header before the
    /// file list arrives — typically a serialized `SearchResult`
    /// shape (title, size, seeders, group). Stored verbatim in
    /// `pending_grabs.release_metadata_json` and echoed back on the
    /// GET preview endpoint.
    #[serde(default)]
    pub release_metadata: serde_json::Value,
    /// Multi-client routing — id of the indexer that surfaced this
    /// release. `None` for Nyaa-direct (routes via Nyaa pin), `Some`
    /// for torznab/newznab fan-out (routes via per-indexer pin). The
    /// resolved `download_clients.id` is stamped on the pending row
    /// so confirm/cancel hit the same client.
    #[serde(default)]
    pub indexer_id: Option<i64>,
}

/// POST `/api/grab/preview` response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GrabPreviewCreated {
    pub preview_id: String,
    /// Always `"fetching_metadata"` on creation — the modal polls
    /// the GET endpoint until the file list arrives.
    pub status: String,
}

/// GET `/api/grab/preview/{id}` response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GrabPreviewStatus {
    pub preview_id: String,
    /// One of `"fetching_metadata"` (file list not yet populated),
    /// `"ready"` (file_list is present and the user can pick), or
    /// `"error"` (metadata fetch failed — modal should show the
    /// retry/defaults dialog).
    pub status: String,
    /// Echoed-back release metadata so the modal can render the
    /// header without re-querying the search endpoint.
    pub release_metadata: serde_json::Value,
    /// File list, only populated when `status == "ready"`. Each entry
    /// carries the torrent-internal file path and size in bytes so
    /// the modal can render per-file sizes and a running total.
    #[serde(default)]
    pub file_list: Vec<PreviewFile>,
    /// Human-readable error message, only populated when
    /// `status == "error"`. Modal uses this verbatim in the
    /// retry/defaults dialog.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// True when at least one `grabbed_torrents` row for the preview's
    /// info_hash is in `state='failed'`. Modal renders an inline
    /// "previously blocklisted" warning + an Unblock-and-continue
    /// button that sends `unblock: true` on confirm (plan decision #12).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocklisted: bool,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PreviewFile {
    pub name: String,
    pub size: i64,
}

/// POST body for `/api/grab/confirm`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GrabConfirmForm {
    pub preview_id: String,
    /// Indices into the preview's `file_list` the user kept checked.
    /// Files NOT in this list will be marked `wanted=false` on the
    /// underlying torrent; files IN this list are marked
    /// `wanted=true` (matters on qBit where `add_torrent_paused`
    /// leaves every file at priority 0 and confirmation is what
    /// flips the selection back on).
    pub wanted_indices: Vec<usize>,
    /// `true` when the user explicitly clicked "Unblock and continue"
    /// on the inline blocklist warning in the modal. Flips every
    /// prior `state='failed'` row for this hash to `state='replaced'`
    /// with a back-pointer to the new grab id. Omitted / `false`
    /// leaves the blocklist entries alone — the new grab still goes
    /// through (the partial UNIQUE index on `(hash) WHERE state IN
    /// ('pending', 'imported')` doesn't exclude 'failed' rows), but
    /// the Downloads-page blocked list keeps showing the old entry.
    #[serde(default)]
    pub unblock: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GrabConfirmResult {
    pub ok: bool,
    /// Error messages for any `set_file_wanted` calls that didn't
    /// land. The grab still commits on partial failure (per plan
    /// decision #10 — best-effort + surface failures); failed files
    /// are left at default priority and the modal can warn the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_priority_errors: Vec<String>,
    /// Error message from the final `resume` call, if any. Separate
    /// from the per-file priority errors so the modal can distinguish
    /// "some priorities didn't apply" (recoverable via client UI)
    /// from "torrent may still be paused" (which matters because
    /// the user expected the grab to start downloading).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_error: Option<String>,
}

/// POST body for `/api/grab/cancel`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GrabCancelForm {
    pub preview_id: String,
}

/// Pluck the user-visible release title out of the `release_metadata_json`
/// blob the modal posted on preview. Handles both the expected
/// `{"title": "..."}` shape and the defensive "metadata was garbage"
/// case by returning `None` so the caller can fall back to the
/// info-hash as a stand-in.
pub(crate) fn extract_release_title(release_metadata_json: &str) -> Option<String> {
    if release_metadata_json.trim().is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(release_metadata_json).ok()?;
    let title = value.get("title")?.as_str()?.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.into())
    }
}

/// Pull the `is_batch` flag out of the modal's `release_metadata_json`
/// blob. This is the authoritative source — it reflects the search-hit
/// listing's batch classification, not the file count — so the post-
/// download classifier's Layer 4 temporal inference (finished ≥ 1 year
/// ago + batch ⇒ BluRay) gets the same signal as the non-interactive
/// auto-search path. Using `files.len() > 1` as a proxy mis-flags a
/// single-episode release delivered as `.mkv` + `.ass` + `.srt` (common
/// with SubsPlease-style releases) as a batch, which would wrongly
/// BluRay-infer on a finished-series WEB release during reclassification.
/// Returns `None` when the modal posted a blob without the field; the
/// caller's file-count fallback is fine for older modal payloads.
pub(crate) fn extract_release_is_batch(release_metadata_json: &str) -> Option<bool> {
    if release_metadata_json.trim().is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(release_metadata_json).ok()?;
    value.get("is_batch").and_then(|v| v.as_bool())
}

fn generate_preview_id() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

async fn require_download_client(
    state: &AppState,
) -> Result<
    std::sync::Arc<dyn crate::services::download_client::DownloadClient>,
    (StatusCode, String),
> {
    state.default_download_client().await.ok_or((
        StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))
}

#[utoipa::path(
    post,
    path = "/api/grab/preview",
    tag = "Grab",
    summary = "Open a pending grab preview (interactive file picker)",
    description = "Adds the torrent in a paused state and returns a \
        preview_id the modal uses to poll for the file list. May block \
        up to ~10s on qBittorrent while it waits for metadata before \
        returning (qBit 5.x can't publish files while stopped, so the \
        AddOutcome — needed to decide whether to store we_added_torrent=true \
        — must be resolved synchronously). Subsequent metadata waiting for \
        the remaining budget runs in a background task; the modal polls \
        GET /api/grab/preview/{id} for readiness.",
    request_body = GrabPreviewForm,
    responses(
        (status = 200, description = "Preview created; poll the GET endpoint for the file list", body = GrabPreviewCreated),
        (status = 400, description = "Missing url/info_hash or download client not configured"),
        (status = 500, description = "Torrent add failed"),
    ),
)]
pub async fn grab_preview(
    State(state): State<AppState>,
    Json(form): Json<GrabPreviewForm>,
) -> Result<Json<GrabPreviewCreated>, (StatusCode, String)> {
    if form.url.trim().is_empty() || form.info_hash.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "url and info_hash are required".to_string(),
        ));
    }

    let info_hash = form.info_hash.trim().to_ascii_lowercase();

    // v1 info-hash is a 40-char lowercase-hex string at the trait
    // boundary; the `pending_grabs` row stores it verbatim and
    // every downstream `DownloadClient` call expects that shape.
    // Reject anything else up front so a misformatted hash (v2's
    // 64-byte SHA-256, a URL-encoded variant, stray whitespace the
    // trim didn't catch, or a caller's typo) fails cleanly with
    // 400 rather than inserting an unusable row that the dedup
    // path later trips over.
    if info_hash.len() != 40 || !info_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "info_hash must be a 40-char lowercase-hex v1 BitTorrent infohash".to_string(),
        ));
    }

    // Pre-flight same-session dedup. Two browser tabs both hitting
    // Grab on the same release would otherwise race through
    // `add_torrent_paused` twice — Tab 1 creates a paused torrent
    // with we_added_torrent=true, Tab 2 sees AlreadyPresent and
    // stores we_added_torrent=false, and if Tab 1 then cancels its
    // (we_added_torrent=true) delete nukes the torrent out from under
    // Tab 2's still-open modal. Returning Tab 1's existing preview_id
    // to Tab 2 collapses both tabs onto the same session, so whichever
    // tab confirms first wins and the other just sees a 404 on its
    // next poll. Plan decision #6 also wants the eventual "show
    // current priorities" flow for releases already in the client,
    // but that's a follow-up — this only covers the in-flight modal case.
    if let Some(existing) = pending_grabs::get_by_hash(&state.db, &info_hash)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
    {
        return Ok(Json(GrabPreviewCreated {
            preview_id: existing.preview_id,
            status: if !existing.error_message.is_empty() {
                "error".to_string()
            } else if existing.file_list_json.is_empty() {
                "fetching_metadata".to_string()
            } else {
                "ready".to_string()
            },
        }));
    }

    // Multi-client routing — preview locks the dispatch client at
    // add-time. Confirm + cancel read `download_client_id` back from
    // the pending row so resume / set_file_wanted / delete hit the
    // same client (a torrent paused on the seedbox can't be resumed
    // via the local qBit). Pin chain: indexer_id (torznab/newznab
    // fan-out hits) > Nyaa pin (Nyaa-direct) > default.
    let resolved = if form.indexer_id.is_some() {
        state.client_for_indexer_with_id(form.indexer_id).await
    } else {
        let cfg = crate::models::config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        state
            .client_for_nyaa_with_id(cfg.nyaa_download_client_id)
            .await
    };
    let (client, dispatch_client_id) = resolved.ok_or((
        StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;
    let client_kind = client.sonarr_impl_name().to_string();
    let metadata_json = form.release_metadata.to_string();

    // Run the paused-add synchronously so we know whether the
    // torrent was added fresh or was pre-existing. `we_added_torrent`
    // gates the destructive delete in `grab_cancel` — we can't make
    // that decision after the fact because AddOutcome isn't stored
    // on the row. Blocking the HTTP handler here is acceptable
    // because non-qBit impls return immediately (they don't wait
    // for metadata in `add_torrent_paused`), and qBit's in-impl
    // wait is bounded to 10s.
    let outcome = match client.add_torrent_paused(&form.url, &info_hash).await {
        Ok(v) => v,
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    let we_added = matches!(outcome, AddOutcome::Added);

    let preview_id = generate_preview_id();
    pending_grabs::create(
        &state.db,
        &preview_id,
        &info_hash,
        &client_kind,
        form.indexer_id,
        form.series_id,
        &metadata_json,
        we_added,
        Some(dispatch_client_id),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Spawn the metadata-wait + file-list-persist. qBit already
    // blocked up to 10s inside add_torrent_paused above and may have
    // the file list ready immediately; Deluge/Transmission/rTorrent
    // return before metadata arrives (add_paused=true is non-blocking
    // by design), so wait_for_files does the cross-client bounded
    // poll. Handler returns preview_id immediately either way.
    let db = state.db.clone();
    let hash = info_hash.clone();
    let preview_id_for_task = preview_id.clone();
    tokio::spawn(async move {
        let files = match download_client::wait_for_files(
            client.as_ref(),
            &hash,
            std::time::Duration::from_secs(METADATA_WAIT_SECS),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("metadata fetch failed: {}", e);
                tracing::warn!(
                    target: "ryokan::handlers::grab",
                    preview_id = %preview_id_for_task,
                    error = %e,
                    "wait_for_files failed; modal will flip to status=error"
                );
                if let Err(db_err) = pending_grabs::set_error(&db, &preview_id_for_task, &msg).await
                {
                    tracing::error!(
                        target: "ryokan::handlers::grab",
                        preview_id = %preview_id_for_task,
                        error = %db_err,
                        "set_error failed"
                    );
                }
                return;
            }
        };
        let preview_files: Vec<PreviewFile> = files
            .into_iter()
            .map(|f| PreviewFile {
                name: f.name,
                size: f.size,
            })
            .collect();
        let json = match serde_json::to_string(&preview_files) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("serialize file list failed: {}", e);
                tracing::error!(
                    target: "ryokan::handlers::grab",
                    preview_id = %preview_id_for_task,
                    error = %e,
                    "serialize file list failed"
                );
                let _ = pending_grabs::set_error(&db, &preview_id_for_task, &msg).await;
                return;
            }
        };
        if let Err(e) = pending_grabs::set_file_list(&db, &preview_id_for_task, &json).await {
            tracing::error!(
                target: "ryokan::handlers::grab",
                preview_id = %preview_id_for_task,
                error = %e,
                "set_file_list failed"
            );
            let _ = pending_grabs::set_error(&db, &preview_id_for_task, &e).await;
        }
    });

    Ok(Json(GrabPreviewCreated {
        preview_id,
        status: "fetching_metadata".to_string(),
    }))
}

/// Cross-client metadata-fetch budget for the spawned preview task.
/// Set to 2× qBit's in-impl 10s budget so:
///
/// * On qBit, the in-impl wait succeeds first (typical case), the
///   outer poll sees a populated file list immediately, and no time
///   is spent retrying what already succeeded.
/// * On Deluge / Transmission / rTorrent, the outer poll owns the
///   wait. 20s covers cold-DHT magnet bootstraps for the overwhelming
///   majority of magnet links. Bare magnets that take longer surface
///   as `status: error` on the next GET poll, flipping the modal to
///   the retry/defaults dialog (plan decision #1).
///
/// Changing either this value or qBit's in-impl budget: they should
/// be tuned as a pair — `OUTER ≥ qBit_inner`. If the inner budget is
/// shortened, qBit's time-at-default-priorities window shrinks with
/// it (issue #5 from the review), but the outer must stay larger or
/// the cross-client poll gives up before qBit would.
const METADATA_WAIT_SECS: u64 = 20;

#[utoipa::path(
    get,
    path = "/api/grab/preview/{preview_id}",
    tag = "Grab",
    summary = "Poll a pending grab's file-list readiness",
    description = "Returns `status: fetching_metadata` until the background \
        metadata fetch completes; then `status: ready` with the file list. \
        Returns 404 after the preview has been confirmed, cancelled, or \
        auto-committed by the sweep — modal should show \"already committed\".",
    params(
        ("preview_id" = String, Path, description = "Opaque id from POST /api/grab/preview"),
    ),
    responses(
        (status = 200, description = "Current status", body = GrabPreviewStatus),
        (status = 404, description = "Preview not found (committed, cancelled, or swept)"),
    ),
)]
pub async fn grab_preview_status(
    State(state): State<AppState>,
    Path(preview_id): Path<String>,
) -> Result<Json<GrabPreviewStatus>, (StatusCode, String)> {
    let row = pending_grabs::get(&state.db, &preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or((StatusCode::NOT_FOUND, "preview not found".to_string()))?;

    let release_metadata: serde_json::Value = if row.release_metadata_json.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&row.release_metadata_json).unwrap_or(serde_json::Value::Null)
    };

    // Blocklist status is read once per poll. Cheap single-row
    // SELECT on an indexed hash; keeps the modal's "previously
    // blocklisted" banner accurate even if the user unblocks the
    // release from Downloads mid-poll.
    let blocklisted = crate::models::grabbed_torrents::is_blocklisted(&state.db, &row.info_hash)
        .await
        .unwrap_or(false);

    // Error takes precedence over fetching/ready. If the spawned
    // metadata-fetch task marked an error, surface it immediately so
    // the modal can offer retry/defaults without waiting for the TTL
    // sweep to drop the row.
    if !row.error_message.is_empty() {
        return Ok(Json(GrabPreviewStatus {
            preview_id,
            status: "error".to_string(),
            release_metadata,
            file_list: Vec::new(),
            error: row.error_message,
            blocklisted,
        }));
    }

    if row.file_list_json.is_empty() {
        return Ok(Json(GrabPreviewStatus {
            preview_id,
            status: "fetching_metadata".to_string(),
            release_metadata,
            file_list: Vec::new(),
            error: String::new(),
            blocklisted,
        }));
    }

    let file_list: Vec<PreviewFile> = serde_json::from_str(&row.file_list_json).unwrap_or_default();
    Ok(Json(GrabPreviewStatus {
        preview_id,
        status: "ready".to_string(),
        release_metadata,
        file_list,
        error: String::new(),
        blocklisted,
    }))
}

#[utoipa::path(
    post,
    path = "/api/grab/heartbeat/{preview_id}",
    tag = "Grab",
    summary = "Keepalive for an open file-picker modal",
    description = "Bumps the pending grab's heartbeat timestamp so the \
        TTL sweep doesn't treat it as abandoned. Modal should call this \
        every ~30s while open. Returns 404 when the preview has already \
        been swept — modal should stop polling and show \"already committed\".",
    params(
        ("preview_id" = String, Path, description = "Opaque id from POST /api/grab/preview"),
    ),
    responses(
        (status = 200, description = "Heartbeat recorded", body = serde_json::Value),
        (status = 404, description = "Preview not found"),
    ),
)]
pub async fn grab_heartbeat(
    State(state): State<AppState>,
    Path(preview_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let bumped = pending_grabs::bump_heartbeat(&state.db, &preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !bumped {
        return Err((StatusCode::NOT_FOUND, "preview not found".to_string()));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/grab/confirm",
    tag = "Grab",
    summary = "Confirm file selections and commit the grab",
    description = "Applies wanted/unwanted priorities per the user's \
        selection, resumes the torrent, and deletes the pending_grabs \
        row. Best-effort on per-file priority writes: partial failures \
        (qBit down mid-apply, a priority write rejected) leave the \
        failed files at default priority rather than rolling back \
        the whole grab. Returns 404 if the preview was already \
        committed or swept. \
        \
        NOTE: unlike grab_cancel, confirm does NOT gate on \
        we_added_torrent — the user consciously submitted their \
        selection, so overwriting a pre-existing torrent's priorities \
        is the intended behavior (plan decision #6's \"show current \
        priorities + allow re-apply\" same-hash flow). Prior \
        partial-downloaded files remain on disk; qBit/rTorrent don't \
        delete previously-downloaded data when a file flips to skip, \
        so the data-risk on overwrite is low.",
    request_body = GrabConfirmForm,
    responses(
        (status = 200, description = "Grab committed", body = GrabConfirmResult),
        (status = 400, description = "preview_id missing or file list not yet populated"),
        (status = 404, description = "Preview not found"),
        (status = 500, description = "Download client error"),
    ),
)]
pub async fn grab_confirm(
    State(state): State<AppState>,
    Json(form): Json<GrabConfirmForm>,
) -> Result<Json<GrabConfirmResult>, (StatusCode, String)> {
    if form.preview_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "preview_id required".into()));
    }

    let row = pending_grabs::get(&state.db, &form.preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or((StatusCode::NOT_FOUND, "preview not found".to_string()))?;

    if row.file_list_json.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "file list not yet populated; poll GET first".to_string(),
        ));
    }

    let files: Vec<PreviewFile> = serde_json::from_str(&row.file_list_json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("stored file list corrupt: {}", e),
        )
    })?;
    let total = files.len();

    // Multi-client routing — resume + set_file_wanted must hit the
    // same client `add_torrent_paused` landed on at preview time.
    // Falls back to default for legacy rows (NULL stamp).
    let client = match row.download_client_id {
        Some(id) => match state.client_by_id(id).await {
            Some(c) => c,
            None => {
                // Client got deleted or disabled mid-modal. Pre-fix
                // this would silently fall through to the wrong
                // default; instead surface 503 so the modal shows the
                // error and the user can re-grab. The pending row
                // stays for the TTL sweep — manual cleanup via cancel
                // is the user's escape hatch.
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "download client #{id} is no longer available; the torrent is still in your client but Ryokan can't manage it from this preview"
                    ),
                ));
            }
        },
        None => require_download_client(&state).await?,
    };

    // Compute the wanted / unwanted partitions. Any index outside
    // [0, total) in `wanted_indices` is silently ignored — the modal
    // is expected to send valid indices but we defend against a
    // racy modal state where the file list changed mid-flight.
    let wanted: Vec<usize> = form
        .wanted_indices
        .into_iter()
        .filter(|&i| i < total)
        .collect();
    let wanted_set: std::collections::HashSet<usize> = wanted.iter().copied().collect();
    let unwanted: Vec<usize> = (0..total).filter(|i| !wanted_set.contains(i)).collect();

    let mut file_priority_errors: Vec<String> = Vec::new();
    if !unwanted.is_empty()
        && let Err(e) = client
            .set_file_wanted(&row.info_hash, &unwanted, false)
            .await
    {
        file_priority_errors.push(format!("mark unwanted: {}", e));
    }
    if !wanted.is_empty()
        && let Err(e) = client.set_file_wanted(&row.info_hash, &wanted, true).await
    {
        file_priority_errors.push(format!("mark wanted: {}", e));
    }
    // Resume starts downloading on Deluge / Transmission / rTorrent
    // (they were added paused). On qBit the torrent is already
    // running — resume is idempotent.
    let resume_error = client.resume(&row.info_hash).await.err();

    // Library attribution. Writes the `grabbed_torrents` row
    // and kicks off sibling auto-expand on the user-selected subset —
    // files the user unchecked are excluded from the sibling-detection
    // file list so a deselected sibling (user unchecked all its
    // episodes) doesn't get a ghost library row. Decision #7.
    //
    // We run this BEFORE deleting the pending row so a DB panic inside
    // commit leaves the pending row for the sweep to retry; if we
    // deleted first, a failure mid-commit would strand the grab with
    // no library attribution and no retry path.
    let release_title =
        extract_release_title(&row.release_metadata_json).unwrap_or_else(|| row.info_hash.clone());
    // Prefer the search-hit's batch flag; fall back to file count for
    // payloads from older modals that didn't forward it. The fallback
    // is imperfect (`.mkv + subs` produces false positives) but only
    // fires when a modal predating the fix reaches an updated server.
    let is_batch = extract_release_is_batch(&row.release_metadata_json).unwrap_or(total > 1);
    let selected_filenames: Vec<String> = wanted
        .iter()
        .filter_map(|&i| files.get(i).map(|f| f.name.clone()))
        .collect();
    let new_grab_id = grab_commit::commit_grab_and_expand(
        &state,
        &row,
        selected_filenames,
        &release_title,
        is_batch,
    )
    .await;

    // Inline unblock (plan decision #12). When the user clicked
    // "Unblock and continue" on the blocklisted-release warning,
    // flip the old `state='failed'` rows for this hash to
    // `state='replaced'` with a back-pointer to the fresh grab so
    // the Downloads-page blocked list drops the stale entry. No-op
    // when there's no new grab id (dedup hit, missing series
    // context) — we don't want to clear the blocklist without a
    // fresh row to point at.
    if form.unblock
        && let Some(new_id) = new_grab_id
    {
        let _ = crate::models::grabbed_torrents::unblock_by_hash(&state.db, &row.info_hash, new_id)
            .await;
        // Misgrab guardrails: a release the user chose to unblock is
        // theirs to keep; verification must not flag it again.
        let _ = crate::models::grabbed_torrents::whitelist_by_hash(&state.db, &row.info_hash).await;
    }

    // Drop the pending row — the user has committed, so the sweep
    // should never revisit this preview_id.
    pending_grabs::delete(&state.db, &form.preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(GrabConfirmResult {
        ok: file_priority_errors.is_empty() && resume_error.is_none(),
        file_priority_errors,
        resume_error,
    }))
}

#[utoipa::path(
    post,
    path = "/api/grab/cancel",
    tag = "Grab",
    summary = "Cancel a pending grab (internal/error path)",
    description = "Deletes the torrent from the download client AND drops \
        the pending_grabs row. Per plan decision #4 this endpoint is NOT \
        called by the modal's normal close flow (which falls through to \
        auto-commit via the sweep); it's reserved for error recovery and \
        the blocklisted-release keep-blocked path.",
    request_body = GrabCancelForm,
    responses(
        (status = 200, description = "Cancelled", body = serde_json::Value),
        (status = 404, description = "Preview not found"),
        (status = 500, description = "Download client error"),
    ),
)]
pub async fn grab_cancel(
    State(state): State<AppState>,
    Json(form): Json<GrabCancelForm>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let row = pending_grabs::get(&state.db, &form.preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or((StatusCode::NOT_FOUND, "preview not found".to_string()))?;

    // Same multi-client lookup as confirm — cancel must hit the same
    // client the preview added the torrent to. Legacy NULL stamp →
    // fall back to default. A vanished client (deleted/disabled mid-
    // modal) skips the destructive delete but still drops the
    // pending row so the modal state doesn't linger; the torrent
    // stays orphaned in the unreachable client and the user must
    // remove it from that client's UI manually (Ryokan has no handle
    // to it once the row is gone from `download_clients`, and
    // `rebuild_clients_cache` only refreshes the in-memory pool — it
    // doesn't reach into a removed client to clean up).
    let client_opt: Option<std::sync::Arc<dyn crate::services::download_client::DownloadClient>> =
        match row.download_client_id {
            Some(id) => state.client_by_id(id).await,
            None => state.default_download_client().await,
        };

    // Only delete the torrent if THIS preview added it fresh. If the
    // torrent was already in the client at add time (AlreadyPresent),
    // the user may have partial-downloaded it from a prior grab or
    // added it manually outside Ryokan — cancelling the preview
    // doesn't give us permission to delete data we didn't create.
    // The pending_grabs row is still dropped either way so the modal-
    // state doesn't linger.
    if row.we_added_torrent
        && let Some(client) = client_opt
        && let Err(e) = client.delete(&row.info_hash, true).await
    {
        tracing::warn!(
            target: "ryokan::handlers::grab",
            preview_id = %form.preview_id,
            hash = %row.info_hash,
            error = %e,
            "download client delete failed during cancel; proceeding with pending-row cleanup"
        );
    }

    pending_grabs::delete(&state.db, &form.preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::download_client::{
        AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
    };
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use async_trait::async_trait;

    /// Minimal DownloadClient stub that accepts every call. Used by
    /// caller-level tests that need `grab_confirm` to clear
    /// `require_download_client` + `set_file_wanted` + `resume`
    /// without actually hitting a running qBittorrent.
    struct NoopClient;

    #[async_trait]
    impl DownloadClient for NoopClient {
        async fn test(&self) -> Result<String, String> {
            Ok("noop".into())
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
        async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
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
    }

    // Unit tests against the handler functions directly (not via a
    // live Axum router) so we can assert on concrete response types
    // without a full HTTP round-trip. Download client interactions
    // are minimized — these tests mostly exercise the database and
    // serialization paths. End-to-end client behavior gets its
    // coverage from the `live_smoke*` tests on each
    // `DownloadClient` impl.

    // A valid 40-char lowercase-hex v1 infohash — use this whenever
    // the test wants to clear the hex-validation gate in
    // `grab_preview` and reach the downstream path under test.
    const VALID_HASH: &str = "aabbccddeeff00112233445566778899aabbccdd";

    #[tokio::test]
    async fn preview_status_404_when_missing() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_preview_status(State(state), Path("nope".to_string())).await;
        assert!(matches!(res, Err((StatusCode::NOT_FOUND, _))));
    }

    #[tokio::test]
    async fn preview_status_fetching_then_ready() {
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "pid-1",
            "abc",
            "qbittorrent",
            None,
            None,
            "{\"title\":\"t\"}",
            true,
            None,
        )
        .await
        .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let status = grab_preview_status(State(state.clone()), Path("pid-1".to_string()))
            .await
            .unwrap();
        assert_eq!(status.status, "fetching_metadata");
        assert!(status.file_list.is_empty());

        // Populate file list → status flips to ready.
        let files = vec![PreviewFile {
            name: "episode_1.mkv".into(),
            size: 8192,
        }];
        pending_grabs::set_file_list(&db, "pid-1", &serde_json::to_string(&files).unwrap())
            .await
            .unwrap();

        let status = grab_preview_status(State(state), Path("pid-1".to_string()))
            .await
            .unwrap();
        assert_eq!(status.status, "ready");
        assert_eq!(status.file_list.len(), 1);
        assert_eq!(status.file_list[0].name, "episode_1.mkv");
    }

    #[tokio::test]
    async fn heartbeat_200_when_present_404_when_gone() {
        let db = in_memory_pool().await;
        pending_grabs::create(
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

        let state = build_test_app_state(db.clone(), None);
        let ok = grab_heartbeat(State(state.clone()), Path("pid-1".to_string())).await;
        assert!(ok.is_ok());

        pending_grabs::delete(&db, "pid-1").await.unwrap();
        let missing = grab_heartbeat(State(state), Path("pid-1".to_string())).await;
        assert!(matches!(missing, Err((StatusCode::NOT_FOUND, _))));
    }

    #[tokio::test]
    async fn confirm_rejects_empty_preview_id() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_confirm(
            State(state),
            Json(GrabConfirmForm {
                preview_id: "".to_string(),
                wanted_indices: vec![0],
                unblock: false,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[tokio::test]
    async fn confirm_404_when_missing() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_confirm(
            State(state),
            Json(GrabConfirmForm {
                preview_id: "nope".to_string(),
                wanted_indices: vec![],
                unblock: false,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::NOT_FOUND, _))));
    }

    #[tokio::test]
    async fn confirm_400_when_file_list_empty() {
        let db = in_memory_pool().await;
        pending_grabs::create(
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
        let state = build_test_app_state(db, None);
        let res = grab_confirm(
            State(state),
            Json(GrabConfirmForm {
                preview_id: "pid-1".to_string(),
                wanted_indices: vec![],
                unblock: false,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[tokio::test]
    async fn preview_rejects_empty_url_or_hash() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_preview(
            State(state.clone()),
            Json(GrabPreviewForm {
                url: "".into(),
                info_hash: "abc".into(),
                series_id: None,
                release_metadata: serde_json::Value::Null,
                indexer_id: None,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));

        let res = grab_preview(
            State(state),
            Json(GrabPreviewForm {
                url: "magnet:?xt=urn:btih:abc".into(),
                info_hash: "".into(),
                series_id: None,
                release_metadata: serde_json::Value::Null,
                indexer_id: None,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[tokio::test]
    async fn preview_400_when_client_not_configured() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_preview(
            State(state),
            Json(GrabPreviewForm {
                url: format!("magnet:?xt=urn:btih:{VALID_HASH}"),
                info_hash: VALID_HASH.into(),
                series_id: Some(42),
                release_metadata: serde_json::json!({"title": "test"}),
                indexer_id: None,
            }),
        )
        .await;
        // Clears the hex-validation gate (hash is 40-char lowercase
        // hex) and reaches `require_download_client`, which returns
        // 400 because the fixture's `AppState` has None as the
        // download client.
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[tokio::test]
    async fn preview_rejects_non_hex_info_hash_with_400() {
        // Regression guard on the PR 89 review fix: a v2 hash
        // (64 hex chars), a bare title, or any non-40-char input
        // must not reach the DB. Cheap check at the top of
        // `grab_preview`.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        // 64-char "v2-like" input.
        let too_long = "a".repeat(64);
        let res = grab_preview(
            State(state.clone()),
            Json(GrabPreviewForm {
                url: "magnet:?xt=urn:btih:abc".into(),
                info_hash: too_long,
                series_id: None,
                release_metadata: serde_json::Value::Null,
                indexer_id: None,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));

        // Garbage non-hex — 40 chars but with a "z".
        let bad_chars = "z".repeat(40);
        let res = grab_preview(
            State(state),
            Json(GrabPreviewForm {
                url: "magnet:?xt=urn:btih:abc".into(),
                info_hash: bad_chars,
                series_id: None,
                release_metadata: serde_json::Value::Null,
                indexer_id: None,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }

    // Pre-flight same-session dedup: when a pending_grabs row already
    // exists for this info_hash, the handler returns the existing
    // preview_id before touching the download client. Exercised by
    // seeding a row and checking the response short-circuits to 200.
    // The no-client test above proves the short-circuit is *before*
    // `require_download_client`, which is the whole point — we don't
    // want Tab 2 to add the torrent a second time.
    #[tokio::test]
    async fn preview_dedupes_same_hash_in_flight_modal() {
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "pid-existing",
            VALID_HASH,
            "qbittorrent",
            None,
            None,
            "{\"title\":\"tab-1 snapshot\"}",
            true,
            None,
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        let res = grab_preview(
            State(state),
            Json(GrabPreviewForm {
                url: format!("magnet:?xt=urn:btih:{VALID_HASH}"),
                info_hash: VALID_HASH.into(),
                series_id: None,
                release_metadata: serde_json::json!({"title": "tab-2 request"}),
                indexer_id: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(res.preview_id, "pid-existing");
        // Row exists but file_list_json is empty, so modal sees
        // "fetching_metadata" on its first status poll.
        assert_eq!(res.status, "fetching_metadata");
    }

    #[tokio::test]
    async fn preview_does_not_dedupe_onto_error_row_from_prior_tab() {
        // Regression guard on the PR 89 review fix: when a prior
        // Tab-1's metadata fetch failed and wrote `error_message`,
        // Tab 2 opening the same release must NOT be short-circuited
        // onto Tab 1's failure — otherwise the user sees an
        // immediate "error" for ~2 min until the TTL sweep drops
        // the row. Instead, `get_by_hash`'s `error_message = ''`
        // filter makes the error row invisible to dedup; Tab 2
        // falls through to the full add_torrent_paused path.
        //
        // Here we observe that fall-through via the
        // `require_download_client` short-circuit — with no client
        // configured, the handler reaches that check and returns
        // BadRequest rather than reusing the errored row's
        // preview_id.
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "pid-failed",
            VALID_HASH,
            "qbittorrent",
            None,
            None,
            "{}",
            true,
            None,
        )
        .await
        .unwrap();
        pending_grabs::set_error(&db, "pid-failed", "metadata fetch timed out")
            .await
            .unwrap();
        let state = build_test_app_state(db, None);
        let res = grab_preview(
            State(state),
            Json(GrabPreviewForm {
                url: format!("magnet:?xt=urn:btih:{VALID_HASH}"),
                info_hash: VALID_HASH.into(),
                series_id: None,
                release_metadata: serde_json::Value::Null,
                indexer_id: None,
            }),
        )
        .await;
        // Reached `require_download_client` → BadRequest. If the
        // dedup had incorrectly returned the error row we'd have
        // gotten `Ok(status: "error")` instead.
        assert!(
            matches!(res, Err((StatusCode::BAD_REQUEST, _))),
            "error-row dedup suppression failed; got {res:?}"
        );
    }

    #[tokio::test]
    async fn extract_release_title_handles_missing_and_present_shapes() {
        // Defensive: modal posts release_metadata with title; a
        // browser extension or curl probe could send garbage. We
        // never want a bad payload to take down the grab-commit
        // helper, so extract_release_title returns Option and
        // callers fall back to info_hash.
        assert_eq!(extract_release_title(""), None);
        assert_eq!(extract_release_title("not json at all"), None);
        assert_eq!(extract_release_title("{\"size\":\"1 GB\"}"), None);
        assert_eq!(
            extract_release_title("{\"title\":\"[Group] Show - 01.mkv\"}"),
            Some("[Group] Show - 01.mkv".into())
        );
        // Whitespace-only title falls through to None so the caller
        // uses the info_hash stand-in rather than writing a blank
        // `torrent_name`.
        assert_eq!(extract_release_title("{\"title\":\"   \"}"), None);
    }

    #[tokio::test]
    async fn extract_release_is_batch_prefers_metadata_over_file_count() {
        // Regression guard on the PR 90 review fix (§1): callers must
        // not proxy `is_batch` off `files.len() > 1`, because a single-
        // episode release shipped as `.mkv + .ass + .srt` has three
        // files but is NOT a batch. Layer 4 reclassification would
        // then wrongly infer BluRay on a finished-series WEB release.
        // The authoritative source is the search-hit listing's batch
        // flag, which the modal forwards through `release_metadata.is_batch`.

        // `None` when no metadata → caller falls back to file count.
        assert_eq!(extract_release_is_batch(""), None);
        assert_eq!(extract_release_is_batch("{\"title\":\"x\"}"), None);

        // Explicit `false` → single-episode release; caller MUST NOT
        // fall back to file-count heuristic.
        assert_eq!(
            extract_release_is_batch("{\"title\":\"x\",\"is_batch\":false}"),
            Some(false)
        );

        // Explicit `true` → batch.
        assert_eq!(
            extract_release_is_batch("{\"title\":\"Pack\",\"is_batch\":true}"),
            Some(true)
        );

        // Garbage JSON / wrong types fall through to None so the caller
        // falls back to the file-count proxy. Better to get a possibly-
        // wrong default than to panic on a bad payload.
        assert_eq!(extract_release_is_batch("not json"), None);
        assert_eq!(
            extract_release_is_batch("{\"is_batch\":\"yes\"}"),
            None,
            "string shouldn't coerce to bool"
        );
    }

    #[tokio::test]
    async fn grab_confirm_with_unblock_whitelists_the_hash() {
        // Misgrab guardrails: a release the user consciously unblocks is
        // never flagged by verification again.
        let db = in_memory_pool().await;
        let series_id = crate::test_support::seed_series(&db, 304, "Unblock Series").await;
        let hash = VALID_HASH;
        let old = crate::models::grabbed_torrents::record_grab(
            &db,
            hash,
            "[Group] Unblock - 01 [1080p]",
            series_id,
            &[1],
            false,
        )
        .await
        .unwrap()
        .unwrap();
        crate::models::grabbed_torrents::mark_failed_by_hash_with_reason(&db, hash, "misgrab")
            .await
            .unwrap();
        let release_metadata = serde_json::json!({
            "title": "[Group] Unblock - 01 [1080p]",
            "size": "1.2 GB",
            "seeders": 100,
            "group": "Group",
            "is_batch": false,
        });
        let file_list = serde_json::json!([
            {"name": "[Group] Unblock - 01 [1080p].mkv", "size": 1_000_000_000_i64},
        ]);
        pending_grabs::create(
            &db,
            "pid-unblock",
            hash,
            "qbittorrent",
            None,
            Some(series_id),
            &release_metadata.to_string(),
            true,
            None,
        )
        .await
        .unwrap();
        pending_grabs::set_file_list(&db, "pid-unblock", &file_list.to_string())
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), Some(std::sync::Arc::new(NoopClient)));
        let res = grab_confirm(
            State(state),
            Json(GrabConfirmForm {
                preview_id: "pid-unblock".into(),
                wanted_indices: vec![0],
                unblock: true,
            }),
        )
        .await;
        assert!(res.is_ok(), "confirm with unblock should succeed: {res:?}");
        assert!(
            crate::models::grabbed_torrents::is_whitelisted_hash(&db, hash).await,
            "unblocking whitelists the hash"
        );
        let old_state: String =
            sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
                .bind(old)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(old_state, "replaced");
    }

    #[tokio::test]
    async fn grab_confirm_honors_release_metadata_is_batch_over_file_count() {
        // Caller-level regression guard for the PR 90 bug fix. A future
        // refactor could silently re-introduce `files.len() > 1` at the
        // confirm call site without the `extract_release_is_batch` unit
        // test failing (the helper would keep returning `Some(false)`,
        // the caller would just ignore it). This test exercises the
        // full path: 3 files posted (`.mkv + .ass + .srt`), explicit
        // `is_batch: false` in release_metadata → asserts the written
        // grabbed_torrents row has `is_batch = 0`, not 1.
        let db = in_memory_pool().await;
        let series_id = crate::test_support::seed_series(&db, 303, "Single Episode Series").await;
        let hash = VALID_HASH;
        let release_metadata = serde_json::json!({
            "title": "[Group] Single Ep - 05 [1080p]",
            "size": "1.2 GB",
            "seeders": 100,
            "group": "Group",
            "is_batch": false,
        });
        let file_list = serde_json::json!([
            {"name": "[Group] Single Ep - 05 [1080p].mkv", "size": 1_000_000_000_i64},
            {"name": "[Group] Single Ep - 05 [1080p].en.ass", "size": 20_000},
            {"name": "[Group] Single Ep - 05 [1080p].en.srt", "size": 18_000},
        ]);
        pending_grabs::create(
            &db,
            "pid-subs",
            hash,
            "qbittorrent",
            None,
            Some(series_id),
            &release_metadata.to_string(),
            true,
            None,
        )
        .await
        .unwrap();
        pending_grabs::set_file_list(&db, "pid-subs", &file_list.to_string())
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), Some(std::sync::Arc::new(NoopClient)));
        let res = grab_confirm(
            State(state),
            Json(GrabConfirmForm {
                preview_id: "pid-subs".into(),
                wanted_indices: vec![0, 1, 2],
                unblock: false,
            }),
        )
        .await;
        assert!(
            res.is_ok(),
            "confirm should succeed with NoopClient: {res:?}"
        );

        let is_batch: i64 =
            sqlx::query_scalar("SELECT is_batch FROM grabbed_torrents WHERE hash = ?")
                .bind(hash)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            is_batch, 0,
            "release_metadata.is_batch=false MUST beat the files.len()>1 proxy"
        );
    }

    #[tokio::test]
    async fn preview_status_surfaces_blocklist_flag() {
        // A prior grab of the same hash with state='failed' should
        // flip `blocklisted: true` on the preview response so the
        // modal can render the inline unblock banner. Plan decision
        // #12.
        let db = in_memory_pool().await;
        let series_id = crate::test_support::seed_series(&db, 101, "Test Series").await;
        // Seed a failed grab row on the hash we're about to preview.
        let grab_id = crate::models::grabbed_torrents::record_grab(
            &db,
            VALID_HASH,
            "earlier grab",
            series_id,
            &[1],
            false,
        )
        .await
        .unwrap()
        .unwrap();
        crate::models::grabbed_torrents::mark_failed(&db, grab_id)
            .await
            .unwrap();
        // Open a preview on the same hash.
        pending_grabs::create(
            &db,
            "pid-blocked",
            VALID_HASH,
            "qbittorrent",
            None,
            Some(series_id),
            "{\"title\":\"Test Release\"}",
            true,
            None,
        )
        .await
        .unwrap();

        let state = build_test_app_state(db, None);
        let resp = grab_preview_status(State(state), Path("pid-blocked".to_string()))
            .await
            .expect("preview status should succeed");
        assert!(
            resp.blocklisted,
            "blocklisted flag should fire when a failed row exists for the hash"
        );
    }

    #[tokio::test]
    async fn unblock_by_hash_flips_failed_to_replaced() {
        // Direct check on the model helper — confirm path wiring
        // covered separately. Verifies the UPDATE targets only
        // state='failed' rows and writes the back-pointer.
        let db = in_memory_pool().await;
        let series_id = crate::test_support::seed_series(&db, 202, "Other Series").await;
        let stale = crate::models::grabbed_torrents::record_grab(
            &db,
            VALID_HASH,
            "old",
            series_id,
            &[],
            false,
        )
        .await
        .unwrap()
        .unwrap();
        crate::models::grabbed_torrents::mark_failed(&db, stale)
            .await
            .unwrap();

        let affected = crate::models::grabbed_torrents::unblock_by_hash(&db, VALID_HASH, 99999)
            .await
            .unwrap();
        assert_eq!(affected, 1);

        let (state, replaced_by): (String, Option<i64>) =
            sqlx::query_as("SELECT state, replaced_by_grab_id FROM grabbed_torrents WHERE id = ?")
                .bind(stale)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(state, "replaced");
        assert_eq!(replaced_by, Some(99999));
    }
}
