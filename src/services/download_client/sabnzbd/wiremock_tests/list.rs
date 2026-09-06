//! `list_scoped` against a mocked SAB. Verifies queue + history
//! merge, category filtering, and SAB → normalized state mapping.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{new_fixture, new_with_category};
use crate::services::download_client::{DownloadClient, DownloadItemState};

#[tokio::test]
async fn list_scoped_merges_queue_and_history_filtered_by_category() {
    let (server, client) = new_fixture().await;
    // Queue carries a mix: one in-category, one foreign-category.
    // Only the in-category slot should land in the result.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_q1",
                        "filename": "show-01.nzb",
                        "cat": "ryokan-test",
                        "status": "Downloading",
                        "percentage": "47",
                        "kbpersec": "2048",
                        "timeleft": "0:01:30",
                        "url": "https://x/q1.nzb",
                    },
                    {
                        "nzo_id": "SABnzbd_nzo_other",
                        "filename": "other-01.nzb",
                        "cat": "different",
                        "status": "Downloading",
                        "percentage": "12",
                        "kbpersec": "0",
                        "timeleft": "0:00:00",
                        "url": "https://x/other.nzb",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_h1",
                        "name": "show-02",
                        "category": "ryokan-test",
                        "status": "Completed",
                        "storage": "/mnt/sab/complete/show-02",
                        "bytes": 524288,
                        "url": "https://x/h1.nzb",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(
        items.len(),
        2,
        "expect one queue slot + one history slot in-category"
    );
    let q = items.iter().find(|i| i.hash == "SABnzbd_nzo_q1").unwrap();
    assert_eq!(q.name, "show-01.nzb");
    assert_eq!(q.state_kind, DownloadItemState::Downloading);
    assert!(
        (q.progress - 0.47).abs() < 0.001,
        "queue progress maps `percentage`/100, got {}",
        q.progress
    );
    assert_eq!(
        q.dlspeed,
        2048 * 1024,
        "kbpersec → bytes/sec conversion: 2048 * 1024 = {}",
        2048 * 1024
    );
    assert_eq!(q.eta, 90, "timeleft 0:01:30 = 90s");
    assert_eq!(
        q.content_path, "",
        "queue slots have no content_path until SAB unpacks (history)"
    );

    let h = items.iter().find(|i| i.hash == "SABnzbd_nzo_h1").unwrap();
    assert_eq!(h.state_kind, DownloadItemState::PausedComplete);
    assert!(
        h.state_kind.is_complete(),
        "Completed must satisfy is_complete() so post-processing imports"
    );
    assert_eq!(h.size, 524288);
    assert_eq!(
        h.content_path, "/mnt/sab/complete/show-02",
        "history `storage` populates content_path"
    );

    assert!(
        items.iter().all(|i| i.hash != "SABnzbd_nzo_other"),
        "foreign-category slot must be filtered out by list_scoped"
    );
}

#[tokio::test]
async fn list_scoped_returns_empty_when_no_matching_category() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": { "slots": [] }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}

#[tokio::test]
async fn list_scoped_unknown_history_status_surfaces_as_errored() {
    // History rows in unknown post-proc states ("Repair Failed",
    // "Move Failed") would import broken data if treated as
    // complete. Errored makes post-processing skip them.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_broken",
                        "name": "broken-show",
                        "category": "ryokan-test",
                        "status": "Repair Failed",
                        "storage": "",
                        "bytes": 0,
                        "url": "",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    let broken = items
        .iter()
        .find(|i| i.hash == "SABnzbd_nzo_broken")
        .unwrap();
    assert_eq!(broken.state_kind, DownloadItemState::Errored);
}

#[tokio::test]
async fn list_scoped_paused_queue_item_maps_to_paused_state() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_paused",
                        "filename": "paused-show.nzb",
                        "cat": "ryokan-test",
                        "status": "Paused",
                        "percentage": "0",
                        "kbpersec": "0",
                        "timeleft": "0:00:00",
                        "url": "https://x/p.nzb",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": { "slots": [] }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    let paused = items
        .iter()
        .find(|i| i.hash == "SABnzbd_nzo_paused")
        .expect("paused queue item should land in result");
    assert_eq!(paused.state_kind, DownloadItemState::Paused);
}

#[tokio::test]
async fn list_scoped_completed_history_maps_to_paused_complete() {
    // History items in `Completed` state are post-imported by SAB
    // already; the trait's PausedComplete signal lets Ryokan's
    // post-processing pick them up exactly once.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_done",
                        "name": "completed-show",
                        "category": "ryokan-test",
                        "status": "Completed",
                        "storage": "/mnt/sab/complete/done",
                        "bytes": 1024,
                        "url": "",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    let done = items.iter().find(|i| i.hash == "SABnzbd_nzo_done").unwrap();
    assert_eq!(done.state_kind, DownloadItemState::PausedComplete);
}

#[tokio::test]
async fn list_scoped_failed_history_maps_to_errored() {
    // Mirrors the unknown-status-Errored test but covers the
    // explicit `Failed` state (the one SAB itself uses for
    // post-proc unrar/par2 failures the user can see in the UI).
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_failed",
                        "name": "failed-show",
                        "category": "ryokan-test",
                        "status": "Failed",
                        "storage": "",
                        "bytes": 0,
                        "url": "",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    let failed = items
        .iter()
        .find(|i| i.hash == "SABnzbd_nzo_failed")
        .unwrap();
    assert_eq!(failed.state_kind, DownloadItemState::Errored);
}

/// Regression: SAB sometimes returns slots with `cat=""` even when
/// Ryokan called addurl with a `cat=anime` parameter — happens when
/// the named category isn't configured in SAB's Settings → Categories
/// (the addurl call accepts the parameter but the resulting slot
/// doesn't carry it). Pre-fix the strict equality filter dropped
/// these slots, the reconcile loop saw nothing matching the grab's
/// hash, and at the 30s grace window flipped the grab to "removed in
/// download client" — even though the SAB job was happily downloading.
/// User-reported during the SAB-on-NZBGeek setup pass.
#[tokio::test]
async fn list_scoped_includes_slot_with_empty_cat_when_category_configured() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_uncategorized",
                        "filename": "show.nzb",
                        "cat": "",
                        "status": "Downloading",
                        "percentage": "10",
                        "kbpersec": "1024",
                        "timeleft": "0:05:00",
                        "url": "",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"history": {"slots": []}})),
        )
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    assert!(
        items.iter().any(|i| i.hash == "SABnzbd_nzo_uncategorized"),
        "slot with empty cat must surface even when client has `ryokan-test` configured; \
         dropping it would let the reconcile loop mark the grab `removed` while SAB is \
         still downloading it"
    );
}

/// SAB category names are user-typed strings; match case-insensitively
/// so `Anime` vs `anime` doesn't silently drop slots. Ryokan's input
/// validation doesn't normalize the category, and SAB itself appears
/// to preserve case in API responses, so a mixed-case mismatch
/// between Ryokan's stored row and SAB's slot category would
/// otherwise reproduce the same "everything looks removed" symptom.
#[tokio::test]
async fn list_scoped_matches_category_case_insensitively() {
    let (server, client) = new_fixture().await;
    // Fixture's configured category is "ryokan-test" (lowercase).
    // SAB returns the slot with "RYOKAN-TEST" (uppercase).
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_caps",
                        "filename": "show.nzb",
                        "cat": "RYOKAN-TEST",
                        "status": "Downloading",
                        "percentage": "10",
                        "kbpersec": "0",
                        "timeleft": "0:00:00",
                        "url": "",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"history": {"slots": []}})),
        )
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    assert!(
        items.iter().any(|i| i.hash == "SABnzbd_nzo_caps"),
        "case-insensitive category match expected; got {:?}",
        items.iter().map(|i| &i.hash).collect::<Vec<_>>()
    );
}

/// When the SAB row in Ryokan has no category configured (empty
/// string), `list_scoped` should pass everything through without
/// filtering. Without this, a user who left the Category field blank
/// on the SAB row would never see ANY of their SAB jobs in Ryokan
/// because every slot has SOME category value (default "*" or
/// whatever) that doesn't equal the empty string.
#[tokio::test]
async fn list_scoped_passes_through_all_when_category_empty() {
    // Empty configured category — `new_with_category("")` mirrors a
    // SAB row in Ryokan with the Category field left blank.
    let (server, client) = new_with_category("").await;

    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_a",
                        "filename": "a.nzb",
                        "cat": "anime",
                        "status": "Downloading",
                        "percentage": "0",
                        "kbpersec": "0",
                        "timeleft": "0:00:00",
                        "url": "",
                    },
                    {
                        "nzo_id": "SABnzbd_nzo_b",
                        "filename": "b.nzb",
                        "cat": "movies",
                        "status": "Downloading",
                        "percentage": "0",
                        "kbpersec": "0",
                        "timeleft": "0:00:00",
                        "url": "",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"history": {"slots": []}})),
        )
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(
        items.len(),
        2,
        "empty configured category must pass through every SAB slot regardless of cat value; \
         got {:?}",
        items.iter().map(|i| &i.hash).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn list_scoped_returns_error_when_queue_endpoint_500s() {
    // Server-side failure on the queue endpoint should bubble up,
    // not silently fall back to "empty list" (which would let
    // post-processing think every active grab vanished).
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let result = client.list_scoped().await;
    assert!(
        result.is_err(),
        "queue 500 must surface as Err — silent empty-list would mask outages"
    );
}

#[tokio::test]
async fn list_scoped_never_reports_seeding_done_for_usenet() {
    // Issue #228: usenet has nothing to seed. A completed history job
    // leaves SAB at import time (post-processing's removal path), so
    // the finished-seed signal stays false and the client's protocol
    // is what routes the removal.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_done",
                        "name": "completed-show",
                        "category": "ryokan-test",
                        "status": "Completed",
                        "storage": "/mnt/sab/complete/done",
                        "bytes": 1024,
                        "url": "",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let items = client.list_scoped().await.expect("list_scoped");
    let done = items.iter().find(|i| i.hash == "SABnzbd_nzo_done").unwrap();
    assert_eq!(done.state_kind, DownloadItemState::PausedComplete);
    assert!(!done.seeding_done);
    assert_eq!(client.protocol(), "usenet");
}
