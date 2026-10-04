//! CSRF coverage — `verify_same_origin_with_trust` across the
//! Origin / Referer / missing-both paths, plus the
//! `X-Forwarded-Host` allowed-hosts expansion gated on trust.
//!
//! Pure function tests — full middleware wiring (what happens when
//! `csrf_public` rejects) is covered by the integration test suite
//! under `tests/`. Here we pin the decision function itself so the
//! middleware calling it can't silently change policy without a test
//! failure.

use axum::body::Body;
use axum::http::{Method, Request, header};

use crate::handlers::auth::{url_host, verify_same_origin_with_trust};

// ─── url_host parsing ──────────────────────────────────────────────

#[test]
fn url_host_extracts_bare_hostname() {
    assert_eq!(
        url_host("https://ryokan.local"),
        Some("ryokan.local".into())
    );
}

#[test]
fn url_host_strips_port() {
    assert_eq!(
        url_host("http://ryokan.local:8978"),
        Some("ryokan.local".into())
    );
}

#[test]
fn url_host_strips_path() {
    assert_eq!(
        url_host("https://ryokan.local/login?next=/"),
        Some("ryokan.local".into())
    );
}

#[test]
fn url_host_lowercases_for_case_insensitive_compare() {
    assert_eq!(
        url_host("https://Ryokan.LOCAL"),
        Some("ryokan.local".into())
    );
}

#[test]
fn url_host_returns_none_when_scheme_separator_missing() {
    // `url_host` is designed around `scheme://host` — without the
    // `://` separator it can't locate the host and returns None.
    // The downstream CSRF check rejects None as "malformed Origin
    // header", so any input that takes this branch is a rejection.
    assert_eq!(url_host("not-a-url"), None);
    assert_eq!(url_host("ryokan.local/login"), None);
    assert_eq!(url_host(""), None);
}

#[test]
fn url_host_returns_none_when_host_segment_is_empty() {
    // `https:///path` — scheme separator is present but host is
    // empty. Returning None here matches the "no anchor to compare"
    // semantics the CSRF caller expects.
    assert_eq!(url_host("https:///path"), None);
}

// ─── Safe methods bypass origin check entirely ─────────────────────

#[test]
fn safe_methods_always_pass_regardless_of_headers() {
    for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
        let req = Request::builder()
            .method(method.clone())
            .uri("/anything")
            .body(Body::empty())
            .unwrap();
        assert!(
            verify_same_origin_with_trust(&req, false).is_ok(),
            "{method} should skip origin check"
        );
    }
}

// ─── Origin header evaluation (unsafe methods) ─────────────────────

fn post_request(host: &str, origin: Option<&str>, referer: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(Method::POST).uri("/login");
    builder = builder.header(header::HOST, host);
    if let Some(o) = origin {
        builder = builder.header("origin", o);
    }
    if let Some(r) = referer {
        builder = builder.header(header::REFERER, r);
    }
    builder.body(Body::empty()).unwrap()
}

#[test]
fn matching_origin_passes() {
    let req = post_request("ryokan.local", Some("https://ryokan.local"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_ok());
}

#[test]
fn matching_origin_with_port_passes() {
    // Browser's Origin carries a port; Host header may too. Either
    // way the host comparison strips ports on both sides.
    let req = post_request("ryokan.local:8978", Some("http://ryokan.local:8978"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_ok());
}

#[test]
fn mismatched_origin_rejects() {
    let req = post_request("ryokan.local", Some("https://evil.com"), None);
    let err = verify_same_origin_with_trust(&req, false).unwrap_err();
    assert!(err.contains("mismatch"), "unexpected rejection: {err}");
}

#[test]
fn null_origin_rejects() {
    // Sandboxed iframes send Origin: null. Never same-origin.
    let req = post_request("ryokan.local", Some("null"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_err());
}

#[test]
fn malformed_origin_rejects() {
    let req = post_request("ryokan.local", Some("definitely-not-a-url"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_err());
}

// ─── Referer fallback when Origin absent ───────────────────────────

#[test]
fn referer_matches_when_origin_absent() {
    let req = post_request("ryokan.local", None, Some("https://ryokan.local/login"));
    assert!(verify_same_origin_with_trust(&req, false).is_ok());
}

#[test]
fn mismatched_referer_rejects() {
    let req = post_request("ryokan.local", None, Some("https://evil.com/form"));
    assert!(verify_same_origin_with_trust(&req, false).is_err());
}

#[test]
fn both_headers_missing_rejects() {
    let req = post_request("ryokan.local", None, None);
    let err = verify_same_origin_with_trust(&req, false).unwrap_err();
    assert!(
        err.contains("missing"),
        "rejection should mention missing headers, got: {err}"
    );
}

// ─── Host header absence ──────────────────────────────────────────

#[test]
fn missing_host_header_rejects_even_with_matching_origin() {
    // Without Host we have no anchor to compare Origin against.
    // An absent Host is unusual but the defense-in-depth policy
    // is to reject rather than assume.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/login")
        .header("origin", "https://ryokan.local")
        .body(Body::empty())
        .unwrap();
    assert!(verify_same_origin_with_trust(&req, false).is_err());
}

// ─── X-Forwarded-Host expansion under trust ───────────────────────

#[test]
fn origin_matches_xfh_only_when_trust_is_on() {
    // Reverse proxy rewrites Host to its internal upstream name
    // (common for header-scrubbing proxies). Browser sends the
    // externally-visible host in Origin. The backend sees the
    // internal Host. Without trust, this is a mismatch.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/login")
        .header(header::HOST, "ryokan.internal")
        .header("origin", "https://ryokan.example.com")
        .header("x-forwarded-host", "ryokan.example.com")
        .body(Body::empty())
        .unwrap();
    // With trust=false → X-Forwarded-Host is ignored → rejection.
    assert!(verify_same_origin_with_trust(&req, false).is_err());
    // With trust=true → X-Forwarded-Host expands the allowed set → accept.
    assert!(verify_same_origin_with_trust(&req, true).is_ok());
}

#[test]
fn xfh_with_port_still_matches_the_host_only_origin() {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/login")
        .header(header::HOST, "ryokan.internal")
        .header("origin", "https://ryokan.example.com")
        .header("x-forwarded-host", "ryokan.example.com:8443")
        .body(Body::empty())
        .unwrap();
    assert!(verify_same_origin_with_trust(&req, true).is_ok());
}

#[test]
fn xfh_multiple_hosts_all_recognized_under_trust() {
    // Some proxies concatenate hostnames as "a, b, c" — all should
    // join the allowed set.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/login")
        .header(header::HOST, "internal")
        .header("origin", "https://third.example.com")
        .header("x-forwarded-host", "first, second, third.example.com")
        .body(Body::empty())
        .unwrap();
    assert!(verify_same_origin_with_trust(&req, true).is_ok());
}

// ─── Ports and IPv6 ────────────────────────────────────────────────

#[test]
fn another_port_on_the_same_host_is_rejected() {
    // A sibling app at :8080 posting to Ryokan at :8978. SameSite
    // ignores ports, so the cookie goes along; only the Origin check
    // can tell them apart.
    let req = post_request("192.168.1.10:8978", Some("http://192.168.1.10:8080"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_err());
    let req = post_request(
        "192.168.1.10:8978",
        None,
        Some("http://192.168.1.10:8080/page"),
    );
    assert!(verify_same_origin_with_trust(&req, false).is_err());
}

#[test]
fn a_default_port_matches_an_origin_that_omits_it() {
    let req = post_request("ryokan.local:443", Some("https://ryokan.local"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_ok());
    let req = post_request("ryokan.local:80", Some("http://ryokan.local"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_ok());
}

#[test]
fn a_host_header_without_a_port_compares_hosts_only() {
    // A reverse proxy forwarding `$host` drops the public port.
    let req = post_request("ryokan.example", Some("https://ryokan.example:8443"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_ok());
}

#[test]
fn ipv6_hosts_are_told_apart() {
    assert_eq!(url_host("http://[::1]:8978/x").as_deref(), Some("[::1]"));
    let req = post_request("[::1]:8978", Some("http://[::1]:8978"), None);
    assert!(verify_same_origin_with_trust(&req, false).is_ok());
    let req = post_request("[::1]:8978", Some("http://[::2]:8978"), None);
    assert!(
        verify_same_origin_with_trust(&req, false).is_err(),
        "every IPv6 host used to compare as \"[\""
    );
}

#[test]
fn a_cross_site_navigation_to_a_side_effect_get_is_refused() {
    use crate::handlers::auth::refuse_cross_site_get;
    let with = |site: Option<&str>| {
        let mut headers = axum::http::HeaderMap::new();
        if let Some(site) = site {
            headers.insert("sec-fetch-site", site.parse().unwrap());
        }
        refuse_cross_site_get(&headers).map(|r| r.status())
    };
    assert_eq!(
        with(Some("cross-site")),
        Some(axum::http::StatusCode::FORBIDDEN)
    );
    assert_eq!(with(Some("same-origin")), None);
    assert_eq!(with(Some("none")), None, "typed into the address bar");
    assert_eq!(with(None), None, "curl and scripts send no header");
}
