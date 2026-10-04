//! Sonarr shim API-key middleware coverage. Exercises both the
//! 503/401 split and the header-vs-query-param key delivery paths.
//! The checks here also implicitly exercise the shared
//! `handlers::arr_auth::check_api_key` helper — the Radarr variant
//! has its own parallel suite so both sides of the "Sonarr enabled
//! vs Radarr enabled" gate are independently fenced.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use crate::test_support::{
    build_test_app_state, in_memory_pool, seed_sonarr_enabled, sonarr_router,
};

const KEY: &str = "test-sonarr-key-01234567";

async fn get(
    app: axum::Router,
    uri: &str,
    header_key: Option<&str>,
    query_key: Option<&str>,
) -> axum::http::Response<Body> {
    let full_uri = match query_key {
        Some(k) => format!("{uri}?apikey={k}"),
        None => uri.to_string(),
    };
    let mut builder = Request::builder().method("GET").uri(&full_uri);
    if let Some(k) = header_key {
        builder = builder.header("x-api-key", k);
    }
    let req = builder.body(Body::empty()).unwrap();
    app.oneshot(req).await.unwrap()
}

// ─── 503 paths (transient / disabled) ──────────────────────────────

#[tokio::test]
async fn returns_503_with_retry_after_when_config_row_missing() {
    // Fresh DB — no config row persisted yet. `get_config` returns
    // `Ok(None)` and the middleware should advertise "try again
    // soon" to Seerr rather than marking the indexer broken.
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", Some(KEY), None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        retry_after, "5",
        "503 on missing config should carry Retry-After: 5"
    );
}

#[tokio::test]
async fn returns_503_when_shim_is_disabled_in_config() {
    let db = in_memory_pool().await;
    // Shim disabled — `sonarr_enabled = false` from the default
    // config. Save an empty key to hit the `!enabled || key.empty()`
    // branch.
    crate::models::config::save_config(&db, &crate::models::config::Config::default())
        .await
        .unwrap();
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", Some("any-key"), None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_sanitizer_placeholder_key_counts_as_no_key() {
    // A restored sanitized backup used to leave `[REDACTED]` as the
    // configured key, and anyone could send it.
    let db = in_memory_pool().await;
    crate::test_support::seed_sonarr_enabled(&db, "[REDACTED]").await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", Some("[REDACTED]"), None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ─── 401 paths (enabled, key wrong) ────────────────────────────────

#[tokio::test]
async fn wrong_keys_from_one_client_are_throttled() {
    // A user-typed key can be short, and nothing limited guessing it.
    let db = in_memory_pool().await;
    crate::test_support::seed_sonarr_enabled(&db, "the-real-key-0123456789").await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    for _ in 0..crate::handlers::auth::LOGIN_MAX_FAILURES {
        let response = get(app.clone(), "/api/v3/system/status", Some("guess"), None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = get(app, "/api/v3/system/status", Some("guess"), None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn returns_401_when_api_key_missing_but_shim_enabled() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", None, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn returns_401_when_api_key_mismatched() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", Some("wrong-key"), None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn returns_401_when_query_param_key_mismatched() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", None, Some("nope")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 200 paths (enabled, key correct) ──────────────────────────────

#[tokio::test]
async fn accepts_api_key_via_x_api_key_header() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", Some(KEY), None).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn accepts_api_key_via_query_param() {
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", None, Some(KEY)).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn header_key_takes_precedence_over_query_param() {
    // When both arrive, the header is read first. If an attacker
    // appended `?apikey=wrong` to a proxied URL and the header
    // carried the real key, the request should still succeed.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", Some(KEY), Some("wrong-key")).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn percent_encoded_query_key_is_decoded_before_compare() {
    // Seerr URL-encodes `=`, `+`, `&`, `%` in the apikey value.
    // The middleware must percent-decode before constant-time
    // compare or every key that contains one of those bytes
    // silently rejects.
    let db = in_memory_pool().await;
    let raw_key = "key-with-plus+and=and&and%25";
    let encoded_key = urlencoding::encode(raw_key).into_owned();
    seed_sonarr_enabled(&db, raw_key).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let response = get(app, "/api/v3/system/status", None, Some(&encoded_key)).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn case_variant_quality_profile_route_accepts_same_key() {
    // Seerr ships both `qualityprofile` and `qualityProfile`
    // depending on version. Both route to the same handler and
    // both must accept the shared api key.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router(state);
    let lower = get(app.clone(), "/api/v3/qualityprofile", Some(KEY), None).await;
    let upper = get(app, "/api/v3/qualityProfile", Some(KEY), None).await;
    assert_eq!(lower.status(), StatusCode::OK);
    assert_eq!(upper.status(), StatusCode::OK);
}
