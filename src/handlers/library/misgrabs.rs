//! System > Misgrabs actions.
//!
//! Restore says "that was actually right": whitelist the hash so
//! verification never flags it again and, if the sweep removed the
//! download, re-add it from the recorded URL (or a bare magnet built
//! from the hash). Dismiss says "yes, that was wrong": a grab still
//! held in the client is removed and blocklisted now, and the row
//! leaves the tab but stays on Downloads > Blocklist.
//!
//! Both answer HTMX with an empty 200 and an `HX-Trigger` payload
//! (ASCII-only, never the torrent name, the same rule as episode
//! delete) so the row swap and the toast come from one response; a
//! failure adds `HX-Reswap: none` so the row stays. Plain POSTs go
//! through `htmx_aware_redirect`.

use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode},
    response::Response,
};
use axum_htmx::HxRequest;

use crate::AppState;
use crate::handlers::responses::htmx_aware_redirect;
use crate::models::log::LogCategory;
use crate::models::{episode_tags, grabbed_torrents};
use crate::services::source::ClassificationResult;
use crate::services::{logger, misgrab, notifications};

/// Build the HTMX response for a Restore or Dismiss outcome.
pub(crate) fn misgrab_trigger(ok: bool, id: i64, action: &str, message: &str) -> Response {
    let safe_message: String = message
        .chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect();
    let payload = serde_json::json!({
        "ryokan-misgrab-action": {
            "ok": ok,
            "id": id,
            "action": action,
            "message": safe_message,
        }
    });
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::OK;
    let header_value: HeaderValue = payload
        .to_string()
        .parse()
        .expect("ASCII-sanitized JSON must parse as a HeaderValue");
    resp.headers_mut().insert("HX-Trigger", header_value);
    if !ok {
        resp.headers_mut()
            .insert("HX-Reswap", HeaderValue::from_static("none"));
    }
    resp
}

fn respond(is_htmx: bool, ok: bool, id: i64, action: &str, message: &str) -> Response {
    if is_htmx {
        misgrab_trigger(ok, id, action, message)
    } else {
        htmx_aware_redirect(
            false,
            &format!("/system?tab=misgrabs&msg={}", urlencoding::encode(message)),
        )
    }
}

fn is_hex40(hash: &str) -> bool {
    hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// The URL Restore re-adds with: the recorded one, else a bare magnet
/// for a BitTorrent infohash.
pub(crate) fn restore_url(row: &grabbed_torrents::GrabbedTorrent) -> Option<String> {
    if !row.source_url.trim().is_empty() {
        Some(row.source_url.trim().to_string())
    } else if is_hex40(&row.hash) {
        Some(format!("magnet:?xt=urn:btih:{}", row.hash))
    } else {
        None
    }
}

#[utoipa::path(
    post,
    path = "/api/library/misgrabs/{id}/restore",
    tag = "Library",
    summary = "Restore a detected misgrab",
    description = "Whitelist the release so verification never flags it again, and re-add it to the download client if the sweep removed it.",
    params(("id" = i64, Path, description = "Grab row id")),
    responses(
        (status = 200, description = "Outcome in the HX-Trigger header for HTMX requests"),
        (status = 303, description = "Redirect back to the Misgrabs tab for plain requests"),
    ),
)]
pub async fn restore_misgrab(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(id): Path<i64>,
) -> Response {
    let db = &state.db;
    let Ok(Some(row)) = grabbed_torrents::get_by_id(db, id).await else {
        return respond(is_htmx, false, id, "restore", "Grab not found");
    };
    if row.verification.as_deref() != Some("misgrab") {
        return respond(
            is_htmx,
            false,
            id,
            "restore",
            "This grab is not a detected misgrab",
        );
    }
    let removed = matches!(
        row.misgrab_action.as_deref(),
        Some("removed") | Some("removed_no_delete")
    ) || row.state == "failed";
    if !removed {
        // Flagged and still in the client: the whitelist alone puts it
        // back on the import path.
        if let Err(e) = grabbed_torrents::whitelist_by_hash(db, &row.hash).await {
            return respond(
                is_htmx,
                false,
                id,
                "restore",
                &format!("Could not whitelist: {e}"),
            );
        }
        logger::info(
            db,
            LogCategory::Grab,
            &format!("Misgrab restored (whitelisted): '{}'", row.torrent_name),
            &format!("series_id={}, hash={}", row.series_id, row.hash),
        )
        .await;
        return respond(
            is_htmx,
            true,
            id,
            "restore",
            "Restored. The download will import normally.",
        );
    }

    // Removed: nothing below may whitelist the row until the client
    // has the release back. A whitelisted row leaves the Misgrabs tab
    // (that is the tab's filter), so whitelisting first would strand a
    // failed re-add with no way to retry it; every early return here
    // leaves the row exactly as it was.
    let Some(url) = restore_url(&row) else {
        return respond(
            is_htmx,
            false,
            id,
            "restore",
            "Cannot restore: no source URL was recorded for this grab",
        );
    };
    let Some(client) = state
        .resolve_grab_client(row.download_client_id, &row.hash)
        .await
    else {
        return respond(
            is_htmx,
            false,
            id,
            "restore",
            "Cannot restore: no download client is configured",
        );
    };
    if let Err(e) = client.add_torrent_returning_id(&url, &row.hash).await {
        logger::warn(
            db,
            LogCategory::DownloadClient,
            &format!(
                "Restore of '{}' failed at the download client",
                row.torrent_name
            ),
            &e,
        )
        .await;
        return respond(
            is_htmx,
            false,
            id,
            "restore",
            "Cannot restore: the download client refused the release",
        );
    }

    // A fresh pending row; the old failed row points at it as replaced.
    let new_id = match grabbed_torrents::record_grab(
        db,
        &row.hash,
        &row.torrent_name,
        row.series_id,
        &row.episode_numbers,
        row.is_batch,
    )
    .await
    {
        Ok(Some(new_id)) => new_id,
        Ok(None) => {
            let _ = grabbed_torrents::whitelist_by_hash(db, &row.hash).await;
            return respond(
                is_htmx,
                true,
                id,
                "restore",
                "Restored. The grab is already active.",
            );
        }
        Err(e) => {
            return respond(
                is_htmx,
                false,
                id,
                "restore",
                &format!("Could not record the grab: {e}"),
            );
        }
    };
    // The new row first, with the reason, then every row for the hash
    // (the old one included) so a re-added torrent is never judged
    // again.
    let detail = serde_json::to_string(&grabbed_torrents::VerificationDetail {
        reason: "restored by the user".to_string(),
        ..Default::default()
    })
    .unwrap_or_default();
    let _ = grabbed_torrents::stamp_verification(db, new_id, "whitelisted", &detail).await;
    if let Err(e) = grabbed_torrents::whitelist_by_hash(db, &row.hash).await {
        logger::warn(
            db,
            LogCategory::Grab,
            &format!(
                "Re-added '{}' but could not whitelist its hash",
                row.torrent_name
            ),
            &e.to_string(),
        )
        .await;
    }
    let _ = grabbed_torrents::unblock_by_hash(db, &row.hash, new_id).await;
    let _ = grabbed_torrents::set_download_client(db, new_id, row.download_client_id).await;
    let _ = grabbed_torrents::set_indexer_attribution(
        db,
        new_id,
        row.indexer_id,
        row.respect_seed_rules,
    )
    .await;
    let _ = grabbed_torrents::set_source_url(db, new_id, &url).await;
    for ep in &row.episode_numbers {
        let _ = episode_tags::record_grab(
            db,
            row.series_id,
            *ep,
            &ClassificationResult::unknown(),
            &row.torrent_name,
            "",
            0,
            row.is_batch,
        )
        .await;
    }
    notifications::emit_grabbed(
        &state,
        row.series_id,
        row.episode_numbers.first().copied().unwrap_or(0),
        &row.torrent_name,
        None,
        None,
        Some(client.sonarr_impl_name().to_string()),
    )
    .await;
    logger::info(
        db,
        LogCategory::Grab,
        &format!("Misgrab restored and re-added: '{}'", row.torrent_name),
        &format!(
            "series_id={}, hash={}, new_grab_id={}",
            row.series_id, row.hash, new_id
        ),
    )
    .await;
    respond(
        is_htmx,
        true,
        id,
        "restore",
        "Restored and re-added to the download client",
    )
}

#[utoipa::path(
    post,
    path = "/api/library/misgrabs/{id}/dismiss",
    tag = "Library",
    summary = "Dismiss a detected misgrab",
    description = "Confirm the misgrab. A grab still held in the client is removed and blocklisted; the row leaves the Misgrabs tab and stays on the blocklist.",
    params(("id" = i64, Path, description = "Grab row id")),
    responses(
        (status = 200, description = "Outcome in the HX-Trigger header for HTMX requests"),
        (status = 303, description = "Redirect back to the Misgrabs tab for plain requests"),
    ),
)]
pub async fn dismiss_misgrab(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(id): Path<i64>,
) -> Response {
    let db = &state.db;
    let Ok(Some(row)) = grabbed_torrents::get_by_id(db, id).await else {
        return respond(is_htmx, false, id, "dismiss", "Grab not found");
    };
    if row.verification.as_deref() != Some("misgrab") {
        return respond(
            is_htmx,
            false,
            id,
            "dismiss",
            "This grab is not a detected misgrab",
        );
    }
    if row.state == "pending" {
        // Flagged and held: the user has decided, so remove it now
        // regardless of the auto-remove setting.
        match misgrab::remove_and_blocklist(&state, &row).await {
            Ok(action) => {
                let _ = grabbed_torrents::set_misgrab_action(db, id, action.as_str()).await;
            }
            Err(e) => {
                return respond(
                    is_htmx,
                    false,
                    id,
                    "dismiss",
                    &format!("Could not remove: {e}"),
                );
            }
        }
    }
    if let Err(e) = grabbed_torrents::mark_misgrab_reviewed(db, id).await {
        return respond(
            is_htmx,
            false,
            id,
            "dismiss",
            &format!("Could not dismiss: {e}"),
        );
    }
    logger::info(
        db,
        LogCategory::Grab,
        &format!("Misgrab dismissed: '{}'", row.torrent_name),
        &format!("series_id={}, hash={}", row.series_id, row.hash),
    )
    .await;
    respond(
        is_htmx,
        true,
        id,
        "dismiss",
        "Dismissed. The release stays on the blocklist.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::series;
    use crate::services::download_client::{
        AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
    };
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use async_trait::async_trait;
    use axum::http::StatusCode;
    use sqlx::SqlitePool;
    use std::sync::{Arc, Mutex};

    const HASH: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

    #[derive(Default)]
    struct RecordingClient {
        add_calls: Mutex<Vec<(String, String)>>,
        delete_calls: Mutex<Vec<(String, bool)>>,
        fail_add: bool,
    }

    #[async_trait]
    impl DownloadClient for RecordingClient {
        async fn test(&self) -> Result<String, String> {
            Ok("fake".into())
        }
        async fn add_torrent(&self, url: &str, hash: &str) -> Result<AddOutcome, String> {
            if self.fail_add {
                return Err("simulated refusal".into());
            }
            self.add_calls
                .lock()
                .unwrap()
                .push((url.to_string(), hash.to_string()));
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
        async fn delete(&self, hash: &str, delete_files: bool) -> Result<(), String> {
            self.delete_calls
                .lock()
                .unwrap()
                .push((hash.to_string(), delete_files));
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

    async fn seed(db: &SqlitePool) -> i64 {
        let (id, _) = series::upsert(
            db,
            series::SeriesCore {
                anilist_id: 21521,
                mal_id: None,
                title: "Kowaremono",
                title_romaji: "Kowaremono",
                title_english: "",
                title_native: "",
                cover_url: "",
                format: "OVA",
                status: "FINISHED",
                episodes: Some(1),
                season_year: Some(2016),
                end_year: None,
            },
        )
        .await
        .unwrap();
        id
    }

    /// A misgrab the sweep already removed and blocklisted.
    async fn removed_misgrab(db: &SqlitePool, sid: i64) -> i64 {
        let id = grabbed_torrents::record_grab(db, HASH, "[Xonline] Grisaia", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        grabbed_torrents::stamp_verification(db, id, "misgrab", "{}")
            .await
            .unwrap();
        grabbed_torrents::mark_failed_by_hash_with_reason(db, HASH, "misgrab")
            .await
            .unwrap();
        grabbed_torrents::set_misgrab_action(db, id, "removed")
            .await
            .unwrap();
        id
    }

    /// A misgrab flagged with auto-remove off: still pending in the client.
    async fn flagged_misgrab(db: &SqlitePool, sid: i64) -> i64 {
        let id = grabbed_torrents::record_grab(db, HASH, "[Xonline] Grisaia", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        grabbed_torrents::stamp_verification(db, id, "misgrab", "{}")
            .await
            .unwrap();
        grabbed_torrents::set_misgrab_action(db, id, "flagged")
            .await
            .unwrap();
        id
    }

    fn trigger(resp: &Response) -> String {
        resp.headers()
            .get("HX-Trigger")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn restore_whitelists_readds_with_magnet_fallback_and_writes_new_pending_row() {
        let db = in_memory_pool().await;
        let sid = seed(&db).await;
        let old = removed_misgrab(&db, sid).await;
        let client = Arc::new(RecordingClient::default());
        let state = build_test_app_state(db.clone(), Some(client.clone()));

        let resp = restore_misgrab(State(state), HxRequest(true), Path(old)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(trigger(&resp).contains("\"ok\":true"), "{}", trigger(&resp));
        assert!(resp.headers().get("HX-Reswap").is_none());

        let adds = client.add_calls.lock().unwrap().clone();
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].0, format!("magnet:?xt=urn:btih:{HASH}"));
        assert!(grabbed_torrents::is_whitelisted_hash(&db, HASH).await);

        let old_row = grabbed_torrents::get_by_id(&db, old)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old_row.state, "replaced");
        let pending = grabbed_torrents::get_all_pending(&db).await.unwrap();
        assert_eq!(pending.len(), 1, "a fresh pending row exists");
        assert_eq!(pending[0].hash, HASH);
        assert_eq!(pending[0].verification.as_deref(), Some("whitelisted"));
        assert_eq!(pending[0].source_url, format!("magnet:?xt=urn:btih:{HASH}"));
        assert!(
            grabbed_torrents::list_misgrabs(&db, "romaji")
                .await
                .unwrap()
                .is_empty()
        );
        let history: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM episode_grab_history WHERE series_id = ? AND state = 'grabbed'",
        )
        .bind(sid)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(history, 1);
    }

    #[tokio::test]
    async fn restore_prefers_recorded_source_url() {
        let db = in_memory_pool().await;
        let sid = seed(&db).await;
        let old = removed_misgrab(&db, sid).await;
        grabbed_torrents::set_source_url(&db, old, "https://indexer.example/dl/1.torrent")
            .await
            .unwrap();
        let client = Arc::new(RecordingClient::default());
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        let resp = restore_misgrab(State(state), HxRequest(true), Path(old)).await;
        assert!(trigger(&resp).contains("\"ok\":true"));
        let adds = client.add_calls.lock().unwrap().clone();
        assert_eq!(adds[0].0, "https://indexer.example/dl/1.torrent");
    }

    #[tokio::test]
    async fn restore_that_cannot_readd_leaves_the_row_on_the_tab_for_a_retry() {
        let db = in_memory_pool().await;
        let sid = seed(&db).await;
        let old = removed_misgrab(&db, sid).await;

        // No client at all, then a client that refuses the release:
        // both must leave the row exactly as it was.
        let state = build_test_app_state(db.clone(), None);
        let resp = restore_misgrab(State(state), HxRequest(true), Path(old)).await;
        assert!(
            trigger(&resp).contains("\"ok\":false"),
            "{}",
            trigger(&resp)
        );
        let refusing = Arc::new(RecordingClient {
            fail_add: true,
            ..Default::default()
        });
        let state = build_test_app_state(db.clone(), Some(refusing.clone()));
        let resp = restore_misgrab(State(state), HxRequest(true), Path(old)).await;
        assert!(
            trigger(&resp).contains("\"ok\":false"),
            "{}",
            trigger(&resp)
        );
        assert_eq!(resp.headers().get("HX-Reswap").unwrap(), "none");
        assert!(
            !grabbed_torrents::is_whitelisted_hash(&db, HASH).await,
            "a failed re-add must not whitelist"
        );
        let row = grabbed_torrents::get_by_id(&db, old)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.verification.as_deref(), Some("misgrab"));
        assert_eq!(row.state, "failed");
        assert_eq!(
            grabbed_torrents::list_misgrabs(&db, "romaji")
                .await
                .unwrap()
                .len(),
            1,
            "still on the tab"
        );
        assert!(
            grabbed_torrents::get_all_pending(&db)
                .await
                .unwrap()
                .is_empty(),
            "no orphan pending row"
        );

        // The client is back: the same button finishes the job.
        let working = Arc::new(RecordingClient::default());
        let state = build_test_app_state(db.clone(), Some(working.clone()));
        let resp = restore_misgrab(State(state), HxRequest(true), Path(old)).await;
        assert!(trigger(&resp).contains("\"ok\":true"), "{}", trigger(&resp));
        assert_eq!(working.add_calls.lock().unwrap().len(), 1);
        assert!(grabbed_torrents::is_whitelisted_hash(&db, HASH).await);
        assert!(
            grabbed_torrents::list_misgrabs(&db, "romaji")
                .await
                .unwrap()
                .is_empty()
        );
        let pending = grabbed_torrents::get_all_pending(&db).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].verification.as_deref(), Some("whitelisted"));
        let reason: String =
            sqlx::query_scalar("SELECT verification_detail FROM grabbed_torrents WHERE id = ?")
                .bind(pending[0].id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(reason.contains("restored by the user"), "{reason}");
    }

    #[tokio::test]
    async fn restore_on_flagged_row_only_whitelists() {
        let db = in_memory_pool().await;
        let sid = seed(&db).await;
        let id = flagged_misgrab(&db, sid).await;
        assert!(
            grabbed_torrents::get_all_pending(&db)
                .await
                .unwrap()
                .is_empty(),
            "held"
        );
        let client = Arc::new(RecordingClient::default());
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        let resp = restore_misgrab(State(state), HxRequest(true), Path(id)).await;
        assert!(trigger(&resp).contains("\"ok\":true"));
        assert!(
            client.add_calls.lock().unwrap().is_empty(),
            "nothing to re-add"
        );
        let row = grabbed_torrents::get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.state, "pending");
        assert_eq!(row.verification.as_deref(), Some("whitelisted"));
        assert_eq!(
            grabbed_torrents::get_all_pending(&db).await.unwrap().len(),
            1,
            "back on the import path"
        );
    }

    #[tokio::test]
    async fn dismiss_sets_reviewed_at_and_keeps_failed_state() {
        let db = in_memory_pool().await;
        let sid = seed(&db).await;
        let id = removed_misgrab(&db, sid).await;
        let client = Arc::new(RecordingClient::default());
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        let resp = dismiss_misgrab(State(state), HxRequest(true), Path(id)).await;
        assert!(trigger(&resp).contains("\"ok\":true"), "{}", trigger(&resp));
        assert!(client.delete_calls.lock().unwrap().is_empty());
        let row = grabbed_torrents::get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert!(
            grabbed_torrents::list_misgrabs(&db, "romaji")
                .await
                .unwrap()
                .is_empty()
        );
        let blocked = grabbed_torrents::get_blocked(&db, "romaji").await.unwrap();
        assert_eq!(blocked.len(), 1, "still on Downloads > Blocklist");
    }

    #[tokio::test]
    async fn dismiss_on_flagged_row_removes_and_blocklists() {
        let db = in_memory_pool().await;
        let sid = seed(&db).await;
        let id = flagged_misgrab(&db, sid).await;
        let client = Arc::new(RecordingClient::default());
        let state = build_test_app_state(db.clone(), Some(client.clone()));
        let resp = dismiss_misgrab(State(state), HxRequest(true), Path(id)).await;
        assert!(trigger(&resp).contains("\"ok\":true"), "{}", trigger(&resp));
        assert_eq!(
            client.delete_calls.lock().unwrap().clone(),
            vec![(HASH.to_string(), true)]
        );
        let row = grabbed_torrents::get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.misgrab_action.as_deref(), Some("removed"));
        assert!(grabbed_torrents::is_blocklisted_release(&db, sid, HASH, "").await);
    }

    #[tokio::test]
    async fn rejects_rows_that_are_not_misgrabs() {
        let db = in_memory_pool().await;
        let sid = seed(&db).await;
        let id = grabbed_torrents::record_grab(&db, HASH, "[G] Fine", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        let state = build_test_app_state(db.clone(), Some(Arc::new(RecordingClient::default())));
        let resp = dismiss_misgrab(State(state.clone()), HxRequest(true), Path(id)).await;
        assert!(trigger(&resp).contains("\"ok\":false"));
        assert_eq!(resp.headers().get("HX-Reswap").unwrap(), "none");
        let resp = restore_misgrab(State(state), HxRequest(true), Path(999)).await;
        assert!(trigger(&resp).contains("Grab not found"));
    }

    #[test]
    fn misgrab_trigger_header_is_ascii_only() {
        let resp = misgrab_trigger(false, 7, "restore", "コワレモノ — nope");
        let header = trigger(&resp);
        assert!(header.is_ascii());
        assert!(header.contains("\"id\":7"));
        assert!(!header.contains("コワレモノ"));
        assert_eq!(resp.headers().get("HX-Reswap").unwrap(), "none");
    }

    #[test]
    fn restore_url_falls_back_to_a_bare_magnet_only_for_infohashes() {
        let mut row = grabbed_torrents::GrabbedTorrent {
            id: 1,
            hash: HASH.to_string(),
            torrent_name: String::new(),
            series_id: 1,
            episode_numbers: vec![],
            state: "failed".to_string(),
            grabbed_at: String::new(),
            is_batch: false,
            download_client_id: None,
            verification: None,
            misgrab_action: None,
            source_url: String::new(),
            indexer_id: None,
            respect_seed_rules: false,
        };
        assert_eq!(
            restore_url(&row).as_deref(),
            Some("magnet:?xt=urn:btih:abcdefabcdefabcdefabcdefabcdefabcdefabcd")
        );
        row.source_url = " https://x/y.torrent ".to_string();
        assert_eq!(restore_url(&row).as_deref(), Some("https://x/y.torrent"));
        row.source_url.clear();
        row.hash = "SABnzbd_nzo_abc".to_string();
        assert_eq!(restore_url(&row), None);
    }

    #[tokio::test]
    async fn plain_post_redirects_htmx_aware() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, Some(Arc::new(RecordingClient::default())));
        let resp = restore_misgrab(State(state), HxRequest(false), Path(12345)).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp.headers().get("Location").unwrap().to_str().unwrap();
        assert!(loc.starts_with("/system?tab=misgrabs&msg="), "{loc}");
    }
}
