//! Auth-failure shapes: HTTP 401 (Prowlarr's pre-torznab-layer
//! auth) AND HTTP 200 + `<error code="100"/>` body (the spec'd
//! shape). Both must surface as Err with the user-readable
//! reason.

use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::indexers::{Indexer, SearchQuery};

#[tokio::test]
async fn http_401_surfaces_status_in_error_message() {
    // Prowlarr returns 401 on bad apikey BEFORE the torznab
    // layer sees the request. The status must be in the error
    // string so the operator can tell "indexer URL unreachable
    // / wrong key" apart from "search returned an error".
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&server)
        .await;

    let result = client.search(&SearchQuery::default()).await;
    let err = result.expect_err("must be Err");
    assert!(err.contains("401"), "error must surface HTTP status: {err}");
}

#[tokio::test]
async fn http_500_surfaces_status_in_error_message() {
    // 500 from upstream (indexer is misbehaving / Cloudflare
    // page / Jackett bug). Treat as Err — the auto-search
    // pipeline's per-indexer outcome captures this without
    // failing the whole search.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let result = client.search(&SearchQuery::default()).await;
    let err = result.expect_err("must be Err");
    assert!(err.contains("500"), "error must surface HTTP status: {err}");
}

#[tokio::test]
async fn http_200_with_error_body_surfaces_error_code() {
    // The spec'd auth-failure shape: HTTP 200 with
    // `<error code="100" description="Invalid API Key"/>`.
    // The client must NOT treat this as a successful empty
    // result — it must surface as Err with the code visible.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0"?>
<error code="100" description="Invalid API Key"/>"#,
        ))
        .mount(&server)
        .await;

    let result = client.search(&SearchQuery::default()).await;
    let err = result.expect_err("must be Err");
    assert!(
        err.contains("100") && err.contains("credentials"),
        "error must surface code + category: {err}"
    );
}

#[tokio::test]
async fn a_failed_request_never_reports_the_api_key() {
    // reqwest's error text includes the full request URL, and the
    // torznab URL carries `apikey=`. The error reached "indexer down"
    // notifications (Discord), System -> Logs, and the poll-error column.
    let (_server, client) = super::fixture::new_fixture_with_row(|row| {
        row.url = "http://127.0.0.1:1/api".to_string();
    })
    .await;
    let err = crate::services::indexers::Indexer::caps(&client)
        .await
        .expect_err("nothing listens on port 1");
    assert!(err.starts_with("indexer request failed"), "{err}");
    assert!(!err.contains(super::fixture::TEST_API_KEY), "{err}");
    assert!(!err.contains("apikey"), "{err}");
}
