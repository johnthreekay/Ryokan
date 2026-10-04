//! `add_torrent_returning_id` + `add_torrent_paused` against a
//! mocked SAB. Pins the wire-format expectations: `mode=addurl`,
//! `name=<URL>`, `cat=<category>`, `priority=-1` for the paused
//! variant; `nzo_id` extraction from the response; the empty-
//! `nzo_ids` → AlreadyPresent fallback path via queue scan.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_API_KEY, new_fixture};
use crate::services::download_client::{AddOutcome, DownloadClient};

#[tokio::test]
async fn add_returns_canonical_nzo_id_from_addurl_response() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .and(query_param("apikey", TEST_API_KEY))
        .and(query_param("cat", "ryokan-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": ["SABnzbd_nzo_test1"],
        })))
        .mount(&server)
        .await;

    let (outcome, id) = client
        .add_torrent_returning_id("https://nzb.example.com/sample.nzb", "")
        .await
        .expect("add must succeed");
    assert!(matches!(outcome, AddOutcome::Added));
    assert_eq!(
        id, "SABnzbd_nzo_test1",
        "returned id must come from SAB's nzo_ids array, not the caller's input"
    );
}

#[tokio::test]
async fn add_paused_sends_priority_minus_1() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .and(query_param("priority", "-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": ["SABnzbd_nzo_paused"],
        })))
        .mount(&server)
        .await;

    let outcome = client
        .add_torrent_paused("https://nzb.example.com/x.nzb", "")
        .await
        .expect("add_paused must succeed");
    // `priority=-1` is what SAB's "Paused" priority maps to, and the
    // wiremock matcher above asserts the query string carries it.
    // If a refactor stops sending it, the mock 404s and the call
    // surfaces as Err — this test would fail at the .expect line.
    assert!(matches!(outcome, AddOutcome::Added));
}

#[tokio::test]
async fn empty_nzo_ids_falls_through_to_queue_scan_for_already_present() {
    // SAB's pre-queue dedup returns `status:true, nzo_ids:[]` when
    // it recognizes a duplicate URL. The impl scans queue+history
    // for a matching URL and reports AlreadyPresent if found.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": [],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "slots": [
                    {
                        "nzo_id": "SABnzbd_nzo_existing",
                        "filename": "existing.nzb",
                        "cat": "ryokan-test",
                        "status": "Downloading",
                        "url": "https://nzb.example.com/dup.nzb",
                    }
                ]
            }
        })))
        .mount(&server)
        .await;

    let (outcome, id) = client
        .add_torrent_returning_id("https://nzb.example.com/dup.nzb", "")
        .await
        .expect("add must succeed");
    assert!(matches!(outcome, AddOutcome::AlreadyPresent));
    assert_eq!(id, "SABnzbd_nzo_existing");
}

#[tokio::test]
async fn empty_nzo_ids_with_no_matching_slot_surfaces_error() {
    // When the queue scan can't find the URL, the empty-nzo_ids
    // signal is treated as a real failure rather than papered over.
    // Without this guarantee a real malformed-URL error from SAB
    // would slip past as a silent success.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": [],
        })))
        .mount(&server)
        .await;
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

    let result = client
        .add_torrent_returning_id("https://nzb.example.com/missing.nzb", "")
        .await;
    assert!(
        result.is_err(),
        "empty nzo_ids + no matching slot must surface as an error, got: {result:?}"
    );
}

#[tokio::test]
async fn add_returning_id_propagates_status_false_with_error_field() {
    // SAB's failure shape is `{"status": false, "error": "..."}`.
    // The `error` field's content matters — the user reads it in
    // System → Logs to triage. Pin so a refactor that drops the
    // error string and just returns "addurl failed" loses signal.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
            "error": "API Key Required",
        })))
        .mount(&server)
        .await;

    let err = client
        .add_torrent_returning_id("https://nzb.example.com/x.nzb", "")
        .await
        .expect_err("status:false must surface as Err");
    assert!(
        err.contains("API Key Required"),
        "SAB's error string must propagate verbatim; got: {err}"
    );
}

#[tokio::test]
async fn add_returning_id_propagates_status_false_without_error_field() {
    // Older SAB builds + custom proxies sometimes return
    // `{"status":false}` with no `error` key. The handler should
    // surface a useful default rather than crashing on the missing
    // field.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false,
        })))
        .mount(&server)
        .await;

    let err = client
        .add_torrent_returning_id("https://nzb.example.com/x.nzb", "")
        .await
        .expect_err("status:false must surface as Err");
    assert!(
        err.contains("no error provided") || err.contains("rejected"),
        "missing error field must surface a friendly default; got: {err}"
    );
}

#[tokio::test]
async fn add_torrent_propagates_http_error_when_server_500s() {
    // The lower-level wire failure is distinct from SAB's
    // status:false in a 200 — bubble the HTTP error rather than
    // letting reqwest's parse-on-non-2xx silently fail.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = client
        .add_torrent_returning_id("https://nzb.example.com/x.nzb", "")
        .await
        .expect_err("503 must surface as Err");
    assert!(
        err.contains("HTTP 503"),
        "5xx surface must include the status code; got: {err}"
    );
}

#[tokio::test]
async fn add_torrent_passes_configured_category_in_addurl_query() {
    // The configured category is what makes `list_scoped` filter
    // out foreign downloads later — if `cat=` doesn't match the
    // configured category at add time, the new download will be
    // invisible to Ryokan (orphaned in SAB's queue forever).
    let (server, client) = super::fixture::new_with_category("ryokan-prod").await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .and(query_param("cat", "ryokan-prod"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": ["SABnzbd_nzo_categorized"],
        })))
        .mount(&server)
        .await;

    client
        .add_torrent("https://x/c.nzb", "")
        .await
        .expect("add must succeed and pass cat=ryokan-prod");
}

#[tokio::test]
async fn add_torrent_propagates_parse_error_when_addurl_body_is_garbage() {
    // SAB sometimes returns plain-text errors when its API key
    // middleware short-circuits. The JSON parse should fail with a
    // clear "addurl parse failed" tag so the user can distinguish
    // "auth misconfigured" from "URL malformed."
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let err = client
        .add_torrent_returning_id("https://x.nzb", "")
        .await
        .expect_err("non-JSON must surface as Err");
    assert!(
        err.contains("parse failed"),
        "addurl parse failures must be tagged; got: {err}"
    );
}

#[tokio::test]
async fn add_returning_id_takes_first_nzo_id_when_multiple_returned() {
    // SAB can return multiple nzo_ids when the addurl resolves to a
    // collection (very rare in practice — most nzb URLs are 1:1 —
    // but the JSON contract is array-typed). Pin "take the first"
    // semantics so a future refactor can't accidentally pick the
    // wrong one.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "addurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": ["SABnzbd_nzo_first", "SABnzbd_nzo_second"],
        })))
        .mount(&server)
        .await;
    let (_outcome, id) = client
        .add_torrent_returning_id("https://x/multi.nzb", "")
        .await
        .expect("multi-id response must still succeed");
    assert_eq!(id, "SABnzbd_nzo_first");
}

#[tokio::test]
async fn refuses_a_non_url_release_before_any_request() {
    let (server, client) = super::fixture::new_fixture().await;
    crate::services::download_client::test_helpers::assert_refuses_non_url_releases(
        &client, &server,
    )
    .await;
    // SAB's inherent paused variant the picker calls directly.
    let Err(err) = client
        .add_torrent_paused_returning_id("file:///etc/passwd", "")
        .await
    else {
        panic!("SAB's paused add accepted a file URL");
    };
    assert!(err.starts_with("add rejected url="), "{err}");
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "a refused URL must not reach SABnzbd"
    );
}
