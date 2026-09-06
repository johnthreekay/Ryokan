//! `list_scoped` wire shape: `core.get_torrents_status` with a
//! `{label: ...}` filter, response keyed by infohash, defensive
//! injection when Deluge omits the inner `hash` field.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{install_rpc, new_fixture};
use crate::services::download_client::{DownloadClient, DownloadItemState};

fn torrent_dict(include_inner_hash: bool) -> serde_json::Value {
    let inner_hash = if include_inner_hash {
        json!("aabbcc0011223344")
    } else {
        json!("")
    };
    json!({
        "aabbcc0011223344": {
            "hash": inner_hash,
            "name": "Test Release",
            "total_size": 1_000_000_000_i64,
            "progress": 50.0,
            "download_payload_rate": 1_000_000,
            "state": "Downloading",
            "eta": 42,
            "save_path": "/downloads",
            "is_finished": false,
            "label": "ryokan-test"
        }
    })
}

#[tokio::test]
async fn list_scoped_sends_label_filter_to_get_torrents_status() {
    // The `{"label": "ryokan-test"}` filter is load-bearing — it's
    // the server-side partition that scopes the listing to
    // Ryokan-owned torrents. A missing filter would return every
    // torrent on the daemon, which Ryokan would then try to import.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.get_torrents_status",
            "params": [{"label": "ryokan-test"}, []],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": torrent_dict(true),
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items.len(), 1);
}

#[tokio::test]
async fn list_scoped_parses_torrent_fields() {
    let (server, client) = new_fixture().await;
    install_rpc(&server, "core.get_torrents_status", torrent_dict(true)).await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items[0].hash, "aabbcc0011223344");
    assert_eq!(items[0].name, "Test Release");
    assert_eq!(items[0].size, 1_000_000_000);
    // Deluge's progress field is 0..100, not 0..1 — normalized to
    // 0..1 by the conversion. Pin the normalization.
    assert!(
        items[0].progress > 0.49 && items[0].progress < 0.51,
        "progress should be normalized 0..1, got {}",
        items[0].progress
    );
    assert_eq!(items[0].dlspeed, 1_000_000);
}

#[tokio::test]
async fn list_scoped_injects_key_when_inner_hash_is_empty() {
    // Defensive branch: Deluge's status dict may omit `hash` in
    // future fork builds or when a reduced key set is requested.
    // The outer dict key carries the infohash; the impl injects it
    // into the inner struct so post-processing's grab→torrent
    // match via by_hash doesn't silently break.
    let (server, client) = new_fixture().await;
    install_rpc(&server, "core.get_torrents_status", torrent_dict(false)).await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(
        items[0].hash, "aabbcc0011223344",
        "empty inner hash should be replaced with outer dict key"
    );
}

#[tokio::test]
async fn list_scoped_maps_deluge_states_to_normalized_enum() {
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "core.get_torrents_status",
        json!({
            "hash1": {
                "hash": "hash1",
                "name": "downloading one",
                "total_size": 1,
                "progress": 10.0,
                "download_payload_rate": 0,
                "state": "Downloading",
                "eta": 0,
                "save_path": "/downloads",
                "is_finished": false,
                "label": "ryokan-test"
            },
            "hash2": {
                "hash": "hash2",
                "name": "seeding one",
                "total_size": 2,
                "progress": 100.0,
                "download_payload_rate": 0,
                "state": "Seeding",
                "eta": 0,
                "save_path": "/downloads",
                "is_finished": true,
                "label": "ryokan-test"
            },
            "hash3": {
                "hash": "hash3",
                "name": "errored one",
                "total_size": 3,
                "progress": 0.0,
                "download_payload_rate": 0,
                "state": "Error",
                "eta": 0,
                "save_path": "/downloads",
                "is_finished": false,
                "label": "ryokan-test"
            }
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    let by_hash: std::collections::HashMap<String, DownloadItemState> = items
        .iter()
        .map(|i| (i.hash.clone(), i.state_kind))
        .collect();
    assert!(
        !by_hash[&"hash1".to_string()].is_complete(),
        "Downloading should not be complete"
    );
    assert!(
        by_hash[&"hash2".to_string()].is_complete(),
        "Seeding should be complete"
    );
    assert!(
        by_hash[&"hash3".to_string()].is_errored(),
        "Error state should be errored"
    );
}

#[tokio::test]
async fn list_scoped_empty_dict_returns_empty_vec() {
    let (server, client) = new_fixture().await;
    install_rpc(&server, "core.get_torrents_status", json!({})).await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}

#[tokio::test]
async fn list_scoped_reports_seeding_done_from_the_ratio_stop_keys() {
    // Issue #228: `core.get_torrents_status` with an empty key list
    // returns every key, including `stop_at_ratio`, `stop_ratio`, and
    // `ratio`, which decide `seeding_done`.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "core.get_torrents_status",
        json!({
            "done000000000000": {
                "hash": "done000000000000", "name": "Done", "total_size": 10, "progress": 100.0,
                "download_payload_rate": 0, "state": "Paused", "eta": 0, "save_path": "/dl",
                "is_finished": true, "label": "ryokan-test",
                "stop_at_ratio": true, "stop_ratio": 2.0, "ratio": 2.1
            },
            "seed000000000000": {
                "hash": "seed000000000000", "name": "Seeding", "total_size": 10, "progress": 100.0,
                "download_payload_rate": 0, "state": "Seeding", "eta": 0, "save_path": "/dl",
                "is_finished": true, "label": "ryokan-test",
                "stop_at_ratio": true, "stop_ratio": 2.0, "ratio": 1.0
            }
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    let done = |h: &str| items.iter().find(|i| i.hash == h).unwrap().seeding_done;
    assert!(done("done000000000000"));
    assert!(!done("seed000000000000"));
}
