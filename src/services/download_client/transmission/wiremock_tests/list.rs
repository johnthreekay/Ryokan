//! `list_scoped` — Transmission's `torrent-get` has no server-side
//! label filter, so the impl filters client-side by the `labels`
//! array. This file pins that filter behavior and the response
//! parsing.

use serde_json::json;
use wiremock::MockServer;

use super::fixture::{install_rpc, install_session_handshake, new_fixture};
use crate::services::download_client::transmission::TransmissionClient;
use crate::services::download_client::{DownloadClient, DownloadItemState};

fn torrent(
    hash: &str,
    name: &str,
    labels: &[&str],
    status: i32,
    percent_done: f64,
) -> serde_json::Value {
    json!({
        "id": 1,
        "hashString": hash,
        "name": name,
        "totalSize": 1_000_000_000_i64,
        "percentDone": percent_done,
        "rateDownload": 0,
        "status": status,
        "eta": 0,
        "downloadDir": "/downloads",
        "labels": labels,
        "isStalled": false,
        "errorString": "",
        "files": [],
        "fileStats": [],
    })
}

#[tokio::test]
async fn list_scoped_filters_by_label_client_side() {
    // Two torrents: one carrying "ryokan-test", one without.
    // Only the labeled one should appear in `list_scoped`.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                torrent("ryokan-hash", "Ours", &["ryokan-test"], 6, 1.0),
                torrent("other-hash", "Theirs", &["something-else"], 4, 0.5),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].hash, "ryokan-hash");
    assert_eq!(items[0].name, "Ours");
}

#[tokio::test]
async fn list_scoped_includes_torrents_with_our_label_among_multiple_labels() {
    // Transmission allows multiple labels on a torrent. Our label
    // being ANY of the values should include the torrent.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                torrent("multi-hash", "Multi", &["user-tag", "ryokan-test", "anime"], 6, 1.0),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].hash, "multi-hash");
}

#[tokio::test]
async fn list_scoped_excludes_torrents_without_any_labels() {
    // Unlabeled torrents (added manually outside Ryokan) must not
    // appear — otherwise post-processing would try to import them.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                torrent("unlabeled", "Orphan", &[], 6, 1.0),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(
        items.is_empty(),
        "unlabeled torrents must be filtered out, got {} items",
        items.len()
    );
}

#[tokio::test]
async fn list_scoped_maps_transmission_status_codes_to_normalized_enum() {
    // Transmission status codes: 0=stopped, 1=check-wait,
    // 2=checking, 3=download-wait, 4=downloading, 5=seed-wait,
    // 6=seeding. Pin the mapping via a representative sample.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                torrent("h4", "downloading", &["ryokan-test"], 4, 0.5),
                torrent("h6", "seeding", &["ryokan-test"], 6, 1.0),
                torrent("h2", "checking", &["ryokan-test"], 2, 0.0),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    let by_hash: std::collections::HashMap<String, DownloadItemState> = items
        .iter()
        .map(|i| (i.hash.clone(), i.state_kind))
        .collect();
    // Status 4 = downloading → not complete.
    assert!(!by_hash[&"h4".to_string()].is_complete());
    // Status 6 = seeding → complete.
    assert!(by_hash[&"h6".to_string()].is_complete());
    // Status 2 = checking → checking-family, not complete.
    assert!(!by_hash[&"h2".to_string()].is_complete());
}

#[tokio::test]
async fn list_scoped_empty_torrents_array_returns_empty_vec() {
    let (server, client) = new_fixture().await;
    install_rpc(&server, "torrent-get", json!({"torrents": []})).await;
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}

#[tokio::test]
async fn list_scoped_missing_torrents_key_returns_empty_vec() {
    // Transmission is loose about the `arguments` shape — if the
    // key is absent (edge case) the impl should default to empty
    // rather than error.
    let server = MockServer::start().await;
    install_session_handshake(&server).await;
    install_rpc(&server, "torrent-get", json!({})).await;
    let client = TransmissionClient::new(&server.uri(), "", "", "ryokan-test");
    let items = client.list_scoped().await.expect("list_scoped");
    assert!(items.is_empty());
}

#[tokio::test]
async fn list_scoped_requests_is_finished_and_maps_it_to_seeding_done() {
    // Issue #228: `isFinished` is Transmission's own "seed limit
    // reached, stopped" flag. It has to be in the requested field list
    // or it never arrives, and a stopped complete torrent without it
    // is a user pause, not a finished seed.
    let (server, client) = new_fixture().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/transmission/rpc"))
        .and(wiremock::matchers::header(
            "x-transmission-session-id",
            super::fixture::TEST_SESSION_ID,
        ))
        .and(wiremock::matchers::body_string_contains("\"isFinished\""))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {
                "torrents": [
                    {
                        "id": 1, "hashString": "finished-hash", "name": "Finished",
                        "totalSize": 10, "percentDone": 1.0, "rateDownload": 0, "status": 0,
                        "eta": 0, "downloadDir": "/downloads", "labels": ["ryokan-test"],
                        "isStalled": false, "errorString": "", "isFinished": true,
                        "files": [], "fileStats": []
                    },
                    torrent("paused-hash", "Paused by hand", &["ryokan-test"], 0, 1.0),
                ]
            },
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    let items = client.list_scoped().await.expect("list_scoped");
    let done = |h: &str| items.iter().find(|i| i.hash == h).unwrap().seeding_done;
    assert!(done("finished-hash"));
    assert!(
        !done("paused-hash"),
        "stopped and complete but not finished is a user pause"
    );
}

#[tokio::test]
async fn list_scoped_detects_a_ratio_stop_without_is_finished() {
    // Transmission 3.x sets isFinished only for idle stops; a torrent
    // stopped at its ratio reports isFinished false. The effective
    // ratio limit (per torrent, or the daemon's global one) fills the
    // gap.
    let (server, client) = new_fixture().await;
    install_rpc(
        &server,
        "session-get",
        json!({"seedRatioLimited": true, "seedRatioLimit": 1.5}),
    )
    .await;
    let stopped = |hash: &str, mode: i32, limit: f64, ratio: f64| {
        json!({
            "id": 1, "hashString": hash, "name": hash, "totalSize": 10, "percentDone": 1.0,
            "rateDownload": 0, "status": 0, "eta": 0, "downloadDir": "/downloads",
            "labels": ["ryokan-test"], "isStalled": false, "errorString": "",
            "isFinished": false, "uploadRatio": ratio, "seedRatioMode": mode,
            "seedRatioLimit": limit, "files": [], "fileStats": []
        })
    };
    install_rpc(
        &server,
        "torrent-get",
        json!({
            "torrents": [
                stopped("own-limit", 1, 2.0, 2.2),
                stopped("global-limit", 0, 0.0, 1.6),
                stopped("below-global", 0, 0.0, 0.4),
                stopped("unlimited", 2, 0.0, 9.0),
            ]
        }),
    )
    .await;
    let items = client.list_scoped().await.expect("list_scoped");
    let done = |h: &str| items.iter().find(|i| i.hash == h).unwrap().seeding_done;
    assert!(done("own-limit"));
    assert!(done("global-limit"));
    assert!(!done("below-global"));
    assert!(!done("unlimited"));
}
