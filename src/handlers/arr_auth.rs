//! Shared API-key middleware for the Sonarr/Radarr compatibility shims.
//!
//! Both shims accept the key either as an `X-Api-Key` header or a
//! percent-encoded `?apikey=` query param, compare in constant time, and
//! return 503 (with Retry-After) for transient config-load failures so
//! Seerr doesn't mark the indexer broken and back off for a long window.
//!
//! The only per-shim difference is which config fields to read and which
//! label to put in the disabled-shim error body; both are passed via the
//! `extract` closure and `label` argument.
use axum::{
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::AppState;
use crate::models::config::{self, Config};

/// Validate the incoming request against the configured API key for one of
/// the arr-compatibility shims. `extract` returns `(enabled, api_key)` from
/// the loaded `Config`, and `label` names the shim for the disabled-shim
/// response body.
pub async fn check_api_key<F>(
    state: AppState,
    req: Request<axum::body::Body>,
    next: Next,
    extract: F,
    label: &str,
) -> Response
where
    F: FnOnce(&Config) -> (bool, String),
{
    // 503 (with Retry-After) for transient config-load failures and for
    // "config row missing" (fresh install, user hasn't saved settings yet).
    // Returning 500 here would have Seerr mark the indexer broken and back
    // off for a long window — 503 advertises "try again soon" instead. The
    // 401 path below stays for "key mismatch" so a real auth failure is
    // still visible.
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        Ok(None) | Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "5")],
                "Ryokan config not yet available",
            )
                .into_response();
        }
    };

    let (enabled, expected) = extract(&cfg);
    // A placeholder from a restored sanitized backup is "no key".
    if !enabled || expected.is_empty() || crate::services::sanitize::is_placeholder(&expected) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{} API compatibility layer is disabled", label),
        )
            .into_response();
    }

    // Check X-Api-Key header first, then fall back to ?apikey= query param.
    // Query-string values are percent-decoded — Seerr URL-encodes apikey
    // values that contain `+`, `=`, `&`, or `%` (all legal in API keys and
    // not restricted by the settings UI), so a raw string compare would
    // silently reject every Seerr request whose key contained any of those
    // characters.
    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            let query_str = req.uri().query().unwrap_or("");
            query_str.split('&').find_map(|pair| {
                let (key, val) = pair.split_once('=')?;
                if key == "apikey" {
                    Some(urlencoding::decode(val).ok()?.into_owned())
                } else {
                    None
                }
            })
        });

    // Constant-time compare so the equality check itself never becomes a
    // timing oracle. The threat is largely theoretical over the network,
    // but it costs nothing to remove.
    if crate::handlers::auth::api_key_throttled(&req, label) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "60")],
            "Too many wrong API keys; wait a minute",
        )
            .into_response();
    }
    let valid = match &api_key {
        Some(key) => bool::from(subtle::ConstantTimeEq::ct_eq(
            key.as_bytes(),
            expected.as_bytes(),
        )),
        None => false,
    };
    if valid {
        next.run(req).await
    } else {
        crate::handlers::auth::api_key_failed(&req, label);
        (StatusCode::UNAUTHORIZED, "Invalid or missing API key").into_response()
    }
}
