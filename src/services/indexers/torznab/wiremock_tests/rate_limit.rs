//! 429 + Retry-After handling. Prowlarr enforces per-indexer
//! rate limits and surfaces them as 429 with a Retry-After
//! header. Ryokan's torznab client captures both, stamps a
//! per-id cooldown via `services::indexers::cooldown`, and
//! short-circuits subsequent calls for that indexer until the
//! window lifts.
//!
//! The cooldown table is process-global static state, so every
//! fixture gets its own row id (`fixture::next_fixture_id`): a
//! cooldown stamped here never reaches another test's indexer,
//! whether the tests share a process (`cargo test`) or not.

use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::indexers::{Indexer, SearchQuery};

#[tokio::test]
async fn http_429_with_retry_after_surfaces_seconds_in_error() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "120")
                .set_body_string("Too Many Requests"),
        )
        .mount(&server)
        .await;

    let result = client.search(&SearchQuery::default()).await;
    let err = result.expect_err("must be Err");
    // Surface "rate-limited" + retry_after for log + cooldown.
    assert!(
        err.contains("rate-limited"),
        "must mention rate-limit: {err}"
    );
    assert!(
        err.contains("120"),
        "must surface retry_after seconds: {err}"
    );
}

#[tokio::test]
async fn http_429_without_retry_after_surfaces_unknown_retry() {
    // Some indexers return 429 without Retry-After. The client
    // must still surface it as a rate-limit error rather than a
    // generic HTTP failure, so the caller's cooldown logic can
    // pick a default.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Slow down"))
        .mount(&server)
        .await;

    let result = client.search(&SearchQuery::default()).await;
    let err = result.expect_err("must be Err");
    assert!(err.contains("rate-limited"), "must surface 429: {err}");
    // retry_after surfaces as `None` in the error message.
    assert!(err.contains("None"), "no retry-after → None: {err}");
}

#[tokio::test]
async fn http_429_stamps_cooldown_and_subsequent_call_short_circuits_without_upstream_hit() {
    let (server, client) = new_fixture().await;
    // `expect(1)` — the wiremock will fail the test if it gets
    // hit a second time. After the first 429 the cooldown is
    // stamped, so the second `search()` must short-circuit
    // BEFORE issuing the request.
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "60")
                .set_body_string("Too Many Requests"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let first = client.search(&SearchQuery::default()).await;
    assert!(
        first
            .as_ref()
            .err()
            .map(|e| e.contains("rate-limited"))
            .unwrap_or(false),
        "first call must surface 429: {first:?}"
    );

    // Second call: same client, same query. The cooldown table
    // should short-circuit before the HTTP layer. Error message
    // shape mirrors Jikan's "Jikan rate-limited (cooldown Ns
    // remaining)" so log scanners that already group on
    // "rate-limited (cooldown" continue to work.
    let second = client.search(&SearchQuery::default()).await;
    let err = second.expect_err("second call must surface cooldown error");
    assert!(
        err.contains("cooldown") && err.contains("rate-limited"),
        "expected cooldown short-circuit error, got: {err}"
    );
    // Wiremock's `expect(1)` is verified on Drop — if the second
    // call leaked through to the wire, the MockServer's drop
    // handler would panic. Reaching this line means the gate
    // worked.
}
