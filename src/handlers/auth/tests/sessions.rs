//! Session cookie shape + HTTP round-trip coverage. Two groups:
//!
//!   * `set_session_cookie_with_secure` / `clear_session_cookie_with_secure`
//!     — pure-function tests pinning the cookie attributes (`HttpOnly`,
//!     `SameSite=Lax`, `Path=/`, `Max-Age=604800` on set, `Max-Age=0`
//!     on clear, `Secure` only when the flag is on), plus
//!     `cookie_secure_for_with`: forced flag, or `X-Forwarded-Proto:
//!     https` only while proxy headers are trusted.
//!   * HTTP round-trip through `handler_router` — anonymous hits to
//!     `/api/health` redirect, a valid `session=<token>` cookie
//!     passes, an unknown token is rejected.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use crate::handlers::auth::{
    clear_session_cookie_with_secure, cookie_secure_for_with, set_session_cookie_with_secure,
};
use crate::test_support::{
    build_test_app_state, handler_router, in_memory_pool, logged_in_session,
};

// ─── Cookie shape — set path ───────────────────────────────────────

#[test]
fn set_session_cookie_contains_core_attributes() {
    let cookie = set_session_cookie_with_secure("abc123", false);
    assert!(cookie.starts_with("session=abc123"), "got: {cookie}");
    assert!(cookie.contains("Path=/"), "missing Path: {cookie}");
    assert!(cookie.contains("HttpOnly"), "missing HttpOnly: {cookie}");
    assert!(
        cookie.contains("SameSite=Lax"),
        "missing SameSite=Lax: {cookie}"
    );
    assert!(
        cookie.contains("Max-Age=604800"),
        "missing 7-day TTL: {cookie}"
    );
}

#[test]
fn set_session_cookie_omits_secure_when_flag_off() {
    let cookie = set_session_cookie_with_secure("abc123", false);
    assert!(
        !cookie.contains("Secure"),
        "Secure should be absent when flag is off: {cookie}"
    );
}

#[test]
fn set_session_cookie_appends_secure_when_flag_on() {
    let cookie = set_session_cookie_with_secure("abc123", true);
    assert!(
        cookie.contains("Secure"),
        "Secure should be present when flag is on: {cookie}"
    );
}

// ─── Secure decision: forced flag vs. trusted X-Forwarded-Proto ─────

fn xfp(value: &str) -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    h.insert("x-forwarded-proto", value.parse().unwrap());
    h
}

#[test]
fn secure_forced_on_wins_regardless_of_headers_or_trust() {
    assert!(cookie_secure_for_with(
        &axum::http::HeaderMap::new(),
        true,
        false
    ));
    assert!(cookie_secure_for_with(&xfp("http"), true, true));
}

#[test]
fn secure_off_by_default_over_plain_http() {
    assert!(!cookie_secure_for_with(
        &axum::http::HeaderMap::new(),
        false,
        false
    ));
    assert!(!cookie_secure_for_with(
        &axum::http::HeaderMap::new(),
        false,
        true
    ));
}

#[test]
fn secure_ignores_forwarded_proto_when_proxy_untrusted() {
    // Direct exposure: any client can send the header, and a Secure
    // cookie set over plain HTTP would never come back.
    assert!(!cookie_secure_for_with(&xfp("https"), false, false));
}

#[test]
fn secure_follows_forwarded_proto_when_proxy_trusted() {
    assert!(cookie_secure_for_with(&xfp("https"), false, true));
    assert!(cookie_secure_for_with(&xfp("HTTPS"), false, true));
    assert!(!cookie_secure_for_with(&xfp("http"), false, true));
}

#[test]
fn secure_reads_the_leftmost_forwarded_proto_hop() {
    // Client → TLS edge → plain internal hop: the client's scheme wins.
    assert!(cookie_secure_for_with(&xfp("https, http"), false, true));
    assert!(!cookie_secure_for_with(&xfp("http, https"), false, true));
    assert!(cookie_secure_for_with(&xfp("  https  "), false, true));
}

// ─── Cookie shape — clear path ─────────────────────────────────────

#[test]
fn clear_session_cookie_uses_max_age_zero() {
    let cookie = clear_session_cookie_with_secure(false);
    assert!(cookie.contains("Max-Age=0"), "got: {cookie}");
    assert!(
        !cookie.contains("Max-Age=604800"),
        "should not carry live TTL: {cookie}"
    );
    assert!(cookie.contains("session="), "should send empty session=");
}

#[test]
fn clear_session_cookie_mirrors_secure_flag() {
    let off = clear_session_cookie_with_secure(false);
    let on = clear_session_cookie_with_secure(true);
    assert!(!off.contains("Secure"));
    assert!(on.contains("Secure"));
}

#[test]
fn clear_session_cookie_keeps_path_and_httponly() {
    // Browsers match cookies on (name, domain, path) — clearing
    // must share the Path of the set cookie or the browser keeps
    // the original. HttpOnly is preserved to match set-path
    // attribute fingerprint.
    let cookie = clear_session_cookie_with_secure(false);
    assert!(cookie.contains("Path=/"));
    assert!(cookie.contains("HttpOnly"));
}

// ─── HTTP round-trip ───────────────────────────────────────────────

#[tokio::test]
async fn anonymous_request_to_protected_endpoint_redirects_to_login() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        matches!(response.status(), StatusCode::SEE_OTHER | StatusCode::FOUND),
        "expected redirect, got {}",
        response.status()
    );
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/login");
}

#[tokio::test]
async fn valid_session_cookie_reaches_protected_endpoint() {
    let db = in_memory_pool().await;
    let (state, cookie) = logged_in_session(&db).await;
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_session_token_is_rejected() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .header(header::COOKIE, "session=nope-this-token-never-existed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Unknown cookie treated same as no cookie: 303 → /login.
    assert!(matches!(
        response.status(),
        StatusCode::SEE_OTHER | StatusCode::FOUND
    ));
}

#[tokio::test]
async fn malformed_cookie_header_is_rejected() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                // Cookie header without a session= key — parse should
                // yield None and the handler should redirect to login.
                .header(header::COOKIE, "other=value; irrelevant=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::SEE_OTHER | StatusCode::FOUND
    ));
}
