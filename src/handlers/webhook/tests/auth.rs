//! Auth surface for `POST /api/webhook/autobrr`. Pins:
//!
//! * 503 when `config.autobrr_api_key` is empty (webhook disabled).
//! * 401 when the key is missing or wrong.
//! * 200 (ok or skipped) when the key is correct via either
//!   `X-Api-Key` header or `?apikey=` query param.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::test_support::{
    autobrr_webhook_router, build_test_app_state, in_memory_pool, seed_autobrr_enabled,
};

const KEY: &str = "test-autobrr-key-abcdef";

fn body_minimal(indexer: &str) -> String {
    format!(
        r#"{{"torrent_name": "Show.S01E01", "info_hash": "aabbccddeeff00112233445566778899aabbccdd", "magnet_uri": "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd", "indexer": "{}"}}"#,
        indexer
    )
}

#[tokio::test]
async fn webhook_disabled_when_api_key_empty() {
    // Default-config row with empty autobrr_api_key. The handler
    // must reject with 503 + a descriptive body so autobrr's
    // "test webhook" button surfaces the gap clearly.
    let db = in_memory_pool().await;
    // Save a default config row (empty autobrr_api_key) so the
    // handler hits the disabled-key branch rather than the
    // config-missing branch.
    crate::models::config::save_config(&db, &crate::models::config::Config::default())
        .await
        .expect("seed default config");
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/webhook/autobrr")
        .header("content-type", "application/json")
        .header("x-api-key", "any")
        .body(Body::from(body_minimal("Nyaa")))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    let s = std::str::from_utf8(&body).unwrap();
    assert!(
        s.contains("disabled"),
        "body must explain disabled state: {s}"
    );
}

#[tokio::test]
async fn missing_api_key_returns_401() {
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/webhook/autobrr")
        .header("content-type", "application/json")
        .body(Body::from(body_minimal("Nyaa")))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_api_key_returns_401() {
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/webhook/autobrr")
        .header("content-type", "application/json")
        .header("x-api-key", "not-the-real-key")
        .body(Body::from(body_minimal("Nyaa")))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn correct_api_key_via_header_passes_auth() {
    // No tracked series in the DB — handler will skip after auth
    // passes, but the response is 200 (status="skipped") rather
    // than 401, which is what we're asserting.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/webhook/autobrr")
        .header("content-type", "application/json")
        .header("x-api-key", KEY)
        .body(Body::from(body_minimal("Nyaa")))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn correct_api_key_via_query_param_passes_auth() {
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/webhook/autobrr?apikey={KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(body_minimal("Nyaa")))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
