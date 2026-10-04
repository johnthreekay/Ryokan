//! `add_torrent` coverage. Three load-bearing rtorrent behaviors:
//!
//!   * URL-scheme validation: rtorrent's `load.start_verbose`
//!     silently accepts junk and returns 0 — caller can't tell a
//!     typo'd URL from a successful add. The impl short-circuits
//!     non-magnet/http(s) URLs with an explicit error.
//!   * Silent-0-return duplicate detection: rtorrent doesn't
//!     distinguish fresh adds from duplicates; both return 0. The
//!     impl pre-checks `hash_exists` via `d.multicall2` and reports
//!     `AlreadyPresent` when the hash is already in the session.
//!   * Label stamping via the third positional arg to
//!     `load.start_verbose` so the torrent appears in
//!     `list_scoped`'s filter from the very first tick.

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{array_response, install_xmlrpc, int_response, new_fixture};
use crate::services::download_client::AddOutcome;
use crate::services::download_client::DownloadClient;

const MAGNET: &str = "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd";
const HASH_LC: &str = "aabbccddeeff00112233445566778899aabbccdd";
const HASH_UC: &str = "AABBCCDDEEFF00112233445566778899AABBCCDD";

#[tokio::test]
async fn happy_path_returns_added_when_hash_absent_pre_check() {
    // `hash_exists` returns false (empty d.multicall2), so the
    // impl proceeds to `load.start_verbose` which returns 0.
    let (server, client) = new_fixture().await;
    install_xmlrpc(&server, "d.multicall2", array_response(&[])).await;
    install_xmlrpc(&server, "load.start_verbose", int_response(0)).await;
    let outcome = client.add_torrent(MAGNET, HASH_LC).await.expect("add");
    assert_eq!(outcome, AddOutcome::Added);
}

#[tokio::test]
async fn silent_0_return_with_existing_hash_reports_already_present() {
    // The key rtorrent quirk: `load.start_verbose` returns 0
    // whether the add was fresh or a duplicate. Without a
    // pre-check the impl would report `Added` for duplicates. The
    // `d.multicall2` pre-check fixes this by hash-existence lookup
    // BEFORE the load call fires.
    let (server, client) = new_fixture().await;
    // d.multicall2 returns [[<HASH_UC>]] — hash is present.
    let row = format!("<array><data><value><string>{HASH_UC}</string></value></data></array>");
    install_xmlrpc(&server, "d.multicall2", array_response(&[row])).await;
    // d.custom1.set for label re-application on already-present torrent.
    install_xmlrpc(&server, "d.custom1.set", int_response(0)).await;
    // load.start_verbose MUST NOT fire — if it does, expect(0)
    // triggers on server drop.
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>load.start_verbose</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(0)
        .mount(&server)
        .await;
    let outcome = client.add_torrent(MAGNET, HASH_LC).await.expect("add");
    assert_eq!(outcome, AddOutcome::AlreadyPresent);
}

#[tokio::test]
async fn malformed_url_rejected_before_rpc_fires() {
    // Input "not-a-url" has no scheme. The impl short-circuits and
    // never reaches d.multicall2 or load.start_verbose. Wiremock's
    // expect(0) on any POST would fail on server drop if a call
    // sneaks through.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(0)
        .mount(&server)
        .await;
    let err = client.add_torrent("not-a-url", HASH_LC).await.unwrap_err();
    assert!(
        err.to_lowercase().contains("scheme") || err.to_lowercase().contains("reject"),
        "malformed URL should be rejected with a clear reason: {err}"
    );
}

#[tokio::test]
async fn schemes_are_matched_case_insensitively() {
    // HTTP:// / MAGNET:/ should also pass — scheme matching is
    // case-insensitive per RFC 3986. Pinning this saves a regression
    // where a future "only lowercase scheme" refactor breaks
    // third-party torznab integrations that emit uppercase URLs.
    let (server, client) = new_fixture().await;
    install_xmlrpc(&server, "d.multicall2", array_response(&[])).await;
    install_xmlrpc(&server, "load.start_verbose", int_response(0)).await;
    let outcome = client
        .add_torrent("HTTP://example.com/file.torrent", HASH_LC)
        .await
        .expect("add uppercase http");
    assert_eq!(outcome, AddOutcome::Added);
}

#[tokio::test]
async fn add_torrent_paused_sets_all_files_skip_flushes_and_pauses() {
    // Regression for the picker-leakage bug: rtorrent's `d.pause` is
    // a soft flag — it lowers upload rates but doesn't close peer
    // connections, so a torrent paused immediately after load could
    // still be fetching chunks by the time the modal opens. The impl
    // works around this by waiting for metadata, setting every file's
    // priority to 0 (skip), issuing the mandatory `d.update_priorities`
    // flush, and only THEN pausing.
    //
    // This test pins the three critical method calls: any refactor
    // that removes the skip-all, drops the update_priorities flush
    // (the classic rtorrent "my priorities aren't taking effect"
    // bug), or pauses without the skip will trip expect(1) here.
    let (server, client) = new_fixture().await;

    // Pre-add duplicate check (up-to-once) returns empty, so
    // `add_torrent` proceeds to `load.start_verbose`. Subsequent
    // `d.multicall2` calls (the settle-loop `hash_exists`) see the
    // hash present.
    let present_row =
        format!("<array><data><value><string>{HASH_UC}</string></value></data></array>");
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>d.multicall2</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(array_response(&[])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>d.multicall2</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(array_response(&[present_row])))
        .mount(&server)
        .await;

    install_xmlrpc(&server, "load.start_verbose", int_response(0)).await;

    // Metadata-pending state: first f.multicall returns the
    // `<HASH>.meta` placeholder rtorrent exposes during metadata
    // fetch. The wait loop MUST NOT exit on this — if it does, the
    // subsequent f.priority.set fires on the phantom index only,
    // and real files arrive after at default priority (wanted) and
    // flow. Guard the regression by forcing the loop through the
    // placeholder state before real files arrive on the next call.
    let meta_row = format!(
        "<array><data>\
            <value><string>{HASH_UC}.meta</string></value>\
            <value><i8>1</i8></value>\
            <value><i8>1</i8></value>\
            <value><i8>0</i8></value>\
            <value><i8>1</i8></value>\
        </data></array>"
    );
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>f.multicall</methodName>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(array_response(&[meta_row])))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Metadata-arrived state: two real files. The wait loop should
    // exit here and set priority on both.
    let file_row = "<array><data>\
            <value><string>release/file1.mkv</string></value>\
            <value><i8>1000000</i8></value>\
            <value><i8>1</i8></value>\
            <value><i8>0</i8></value>\
            <value><i8>1000</i8></value>\
        </data></array>"
        .to_string();
    let file_row2 = "<array><data>\
            <value><string>release/file2.mkv</string></value>\
            <value><i8>2000000</i8></value>\
            <value><i8>1</i8></value>\
            <value><i8>0</i8></value>\
            <value><i8>2000</i8></value>\
        </data></array>"
        .to_string();
    install_xmlrpc(
        &server,
        "f.multicall",
        array_response(&[file_row, file_row2]),
    )
    .await;

    // The three regression-critical calls — each must fire.
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>f.priority.set</methodName>",
        ))
        .and(body_string_contains("<i8>0</i8>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>d.update_priorities</methodName>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains("<methodName>d.pause</methodName>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;

    client
        .add_torrent_paused(MAGNET, HASH_LC)
        .await
        .expect("add paused");
}

#[tokio::test]
async fn label_command_appears_in_load_start_verbose_body() {
    // The third positional arg to load.start_verbose is the post-
    // load command executed before the torrent starts. Stamping
    // custom1 here (vs a post-load d.custom1.set) means the
    // torrent never appears unlabeled in `list_scoped`'s filter
    // even for a single tick.
    let (server, client) = new_fixture().await;
    install_xmlrpc(&server, "d.multicall2", array_response(&[])).await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>load.start_verbose</methodName>",
        ))
        .and(body_string_contains("d.custom1.set"))
        .and(body_string_contains("ryokan-test"))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    client.add_torrent(MAGNET, HASH_LC).await.expect("add");
}

#[tokio::test]
async fn refuses_a_non_url_release_before_any_request() {
    let (server, client) = super::fixture::new_fixture().await;
    crate::services::download_client::test_helpers::assert_refuses_non_url_releases(
        &client, &server,
    )
    .await;
}
