//! `add_torrent` and `add_torrent_paused` coverage — magnet vs URL
//! dispatch, duplicate-add substring matching, label fan-out.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{install_rpc, install_rpc_error, new_fixture};
use crate::services::download_client::AddOutcome;
use crate::services::download_client::DownloadClient;

const MAGNET: &str = "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd";
const HTTP_URL: &str = "http://example.com/release.torrent";
const HASH: &str = "aabbccddeeff00112233445566778899aabbccdd";

#[tokio::test]
async fn magnet_url_dispatches_to_add_torrent_magnet() {
    // Dispatch: magnet URIs go to `core.add_torrent_magnet`, NOT
    // `core.add_torrent_url` (the wrong method rejects the input
    // with a cryptic parse error). Use body_partial_json to pin
    // the specific method call rather than the URL path (both
    // methods hit /json).
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(
            json!({"method": "core.add_torrent_magnet"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": HASH,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Expect(0) on the wrong method — if the impl misdispatches,
    // this fails on server drop.
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({"method": "core.add_torrent_url"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(0)
        .mount(&server)
        .await;
    let outcome = client.add_torrent(MAGNET, HASH).await.expect("add magnet");
    assert_eq!(outcome, AddOutcome::Added);
}

#[tokio::test]
async fn http_url_dispatches_to_add_torrent_url() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({"method": "core.add_torrent_url"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            // add_torrent_url returns null on success (not the hash).
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(
            json!({"method": "core.add_torrent_magnet"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": HASH,
            "error": null,
            "id": 1,
        })))
        .expect(0)
        .mount(&server)
        .await;
    let outcome = client
        .add_torrent(HTTP_URL, HASH)
        .await
        .expect("add http url");
    assert_eq!(outcome, AddOutcome::Added);
}

#[tokio::test]
async fn duplicate_add_torrent_already_in_session_returns_already_present() {
    // Deluge's version-unstable error-code situation (ticket
    // deluge-dev/#3507): the error code varies across builds but
    // the message prefix is stable. Ryokan matches on the prefix.
    let (server, client) = new_fixture().await;
    install_rpc_error(
        &server,
        "core.add_torrent_magnet",
        "Failure: Torrent already in session (aabbccddeeff00112233445566778899aabbccdd).",
    )
    .await;
    let outcome = client.add_torrent(MAGNET, HASH).await.expect("add");
    assert_eq!(outcome, AddOutcome::AlreadyPresent);
}

#[tokio::test]
async fn duplicate_add_torrent_already_being_added_returns_already_present() {
    // Alternate error string: "Torrent already being added" fires
    // when a second add lands before the first has finished
    // parsing. Same semantic outcome — AlreadyPresent.
    let (server, client) = new_fixture().await;
    install_rpc_error(
        &server,
        "core.add_torrent_magnet",
        "Failure: Torrent already being added to the session.",
    )
    .await;
    let outcome = client.add_torrent(MAGNET, HASH).await.expect("add");
    assert_eq!(outcome, AddOutcome::AlreadyPresent);
}

#[tokio::test]
async fn non_duplicate_error_propagates() {
    // Other errors (tracker unreachable, quota exceeded, …) must
    // surface to the caller, not get squashed into AlreadyPresent.
    let (server, client) = new_fixture().await;
    install_rpc_error(&server, "core.add_torrent_magnet", "Tracker unreachable").await;
    let err = client.add_torrent(MAGNET, HASH).await.unwrap_err();
    assert!(
        err.contains("Tracker"),
        "non-duplicate error should propagate: {err}"
    );
}

#[tokio::test]
async fn add_torrent_fires_label_set_torrent_with_caller_hash() {
    // After a successful add, the impl fans out `label.set_torrent`
    // to tag the torrent with our scoping label. Pinning this is
    // load-bearing: an unlabeled torrent never shows up in
    // `list_scoped`'s label-filtered listing, so a regression
    // would make post-processing think the torrent was lost.
    let (server, client) = new_fixture().await;
    install_rpc(&server, "core.add_torrent_magnet", json!(HASH)).await;
    // Override the default label.set_torrent mock with expect(1).
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "label.set_torrent",
            "params": [HASH, "ryokan-test"],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client.add_torrent(MAGNET, HASH).await.expect("add");
}

#[tokio::test]
async fn add_torrent_paused_adds_running_then_skips_all_files_and_pauses() {
    // Regression for the paused-add metadata quirk: Deluge's native
    // `add_paused=true` stops the peer handshake, which in turn
    // prevents libtorrent's metadata-exchange extension from running,
    // so `get_files` returns empty forever and the picker modal's
    // readiness poll hangs. The impl works around this by adding
    // running, waiting for metadata, setting every file to SKIP
    // priority (0), then pausing — this test pins the sequence.
    let (server, client) = new_fixture().await;

    // Add with add_paused = FALSE so metadata exchange happens.
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.add_torrent_magnet",
            "params": [MAGNET, {"add_paused": false}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": HASH,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Metadata poll: return a populated `files` array so the loop
    // exits on the first iteration.
    install_rpc(
        &server,
        "core.get_torrent_status",
        json!({
            "files": [
                {"path": "release/file1.mkv", "size": 1_000_000},
                {"path": "release/file2.mkv", "size": 2_000_000},
            ],
        }),
    )
    .await;

    // Every file marked skip (priority 0) for a 2-file torrent.
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.set_torrent_options",
            "params": [[HASH], {"file_priorities": [0, 0]}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Pause after priorities — the order matters because pausing
    // before metadata would regress into the "paused = no metadata"
    // hole the workaround exists to avoid.
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.pause_torrent",
            "params": [[HASH]],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;

    client
        .add_torrent_paused(MAGNET, HASH)
        .await
        .expect("add paused");
}

#[tokio::test]
async fn refuses_a_non_url_release_before_any_request() {
    let (server, client) = super::fixture::new_fixture().await;
    crate::services::download_client::test_helpers::assert_refuses_non_url_releases(
        &client, &server,
    )
    .await;
}
