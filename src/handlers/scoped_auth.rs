//! Scoped API key middleware (issue #114).
//!
//! Default-deny by design: only routes that explicitly attach a
//! per-scope middleware (e.g. [`require_calendar_scope`]) accept
//! scoped-key auth. Untagged routes stay cookie-only — same auth
//! shape as today, no behavior change. This deliberately ducks the
//! cookie-OR-key composition problem until we have a dual-mode
//! endpoint to design against.
//!
//! ## Wire shape
//!
//! Accepts the key as either:
//! - An `X-Api-Key` HTTP header.
//! - A percent-encoded `?apikey=` query string parameter.
//!
//! Same surface as the existing Sonarr/Radarr/autobrr keys (see
//! `handlers/arr_auth.rs`). Calendar subscribers (Google Calendar /
//! Apple Calendar / Thunderbird) only support the query-string form,
//! so embedding the key in the URL is the load-bearing path here;
//! the header form is for richer clients.
//!
//! ## Audit logging
//!
//! Every successful match writes a `LogCategory::Auth` row with the
//! key name (NOT the key) and the matched scope. Failed matches log
//! at `warn` level so the System → Logs panel filtered to Auth
//! surfaces probing attempts.
//!
//! Per-scope rate-limited audit logging is a non-goal for now (see
//! issue #114 plan doc). The existing `cleanup` task's log rotation
//! handles volume; revisit if iCal subscriber load makes the table
//! noisy.

use axum::{
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::AppState;
use crate::models::api_key;
use crate::models::log::LogCategory;
use crate::services::logger;

/// Per-scope middleware factory shape. Each scope gets its own
/// middleware function (e.g. [`require_calendar_scope`]) that
/// delegates here with its scope string baked in.
async fn check_scope(
    state: AppState,
    req: Request<axum::body::Body>,
    next: Next,
    required_scope: &'static str,
) -> Response {
    let plaintext = match extract_api_key(&req) {
        Some(k) => k,
        None => {
            // No key supplied at all — 401, no log row (this is the
            // "browser hits the URL without a subscription token"
            // case and would be noisy if logged).
            return (StatusCode::UNAUTHORIZED, "Missing API key").into_response();
        }
    };

    let key = match api_key::lookup_by_plaintext(&state.db, &plaintext).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            // Wrong / disabled / nonexistent key. Log at warn so
            // probing surfaces in System → Logs, once a minute per
            // client: each line is a database row, and a probe loop
            // otherwise wrote thousands a second.
            let peer = req
                .extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0.ip().to_string())
                .unwrap_or_default();
            if logger::first_in_window(
                &format!("scoped-reject:{peer}"),
                std::time::Duration::from_secs(60),
            ) {
                logger::warn(
                    &state.db,
                    LogCategory::Auth,
                    "Scoped API request rejected",
                    &format!("scope={} reason=unknown_key", required_scope),
                )
                .await;
            }
            return (StatusCode::UNAUTHORIZED, "Invalid API key").into_response();
        }
        Err(e) => {
            // DB error during the lookup. Treat as 503 so the client
            // backs off briefly rather than treating it as a hard
            // auth failure (Google Calendar will rage-poll on 401
            // less than on 5xx).
            logger::error(
                &state.db,
                LogCategory::Auth,
                "Scoped API key lookup failed",
                &e.to_string(),
            )
            .await;
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "5")],
                "Auth backend unavailable",
            )
                .into_response();
        }
    };

    if !key.grants(required_scope) {
        // Key exists but doesn't carry the right scope. Log so the
        // user can see why their key isn't working when they
        // troubleshoot.
        logger::warn(
            &state.db,
            LogCategory::Auth,
            "Scoped API request rejected",
            &format!(
                "scope={} reason=missing_scope key_name={}",
                required_scope,
                crate::handlers::auth::sanitize_for_log(&key.name),
            ),
        )
        .await;
        return (
            StatusCode::FORBIDDEN,
            format!("Key does not grant scope: {required_scope}"),
        )
            .into_response();
    }

    // Best-effort last-used stamp. A DB hiccup here doesn't kill the
    // request — the audit trail can survive a missed touch but the
    // user shouldn't see a 5xx because their iCal feed momentarily
    // can't update its `last_used_at`.
    let _ = api_key::touch_last_used(&state.db, key.id).await;

    logger::info(
        &state.db,
        LogCategory::Auth,
        "Scoped API request accepted",
        &format!(
            "scope={} key_name={}",
            required_scope,
            crate::handlers::auth::sanitize_for_log(&key.name)
        ),
    )
    .await;

    next.run(req).await
}

/// Pull the API key out of the `X-Api-Key` header first, then fall
/// back to a `?apikey=` query parameter. Same precedence as the
/// arr-shim and autobrr-webhook auth paths so users only have to
/// learn one shape. Query-string values are percent-decoded — the
/// generated keys are 64 hex chars (no special characters), but a
/// future format change shouldn't silently break user-pasted URLs.
///
/// Both branches reject empty values: the `WHERE key = ?` lookup
/// would otherwise match any row whose `key` column is empty
/// string (impossible for fresh installs because the column has
/// UNIQUE + every row gets a 64-char hex value, but possible for
/// upgraders from the previous hash+encrypted schema where the
/// `key` column was added via ALTER with `DEFAULT ''` and SQLite
/// can't add UNIQUE through ALTER). PR #165 review.
fn extract_api_key(req: &Request<axum::body::Body>) -> Option<String> {
    if let Some(header_val) = req.headers().get("x-api-key")
        && let Ok(s) = header_val.to_str()
        && !s.is_empty()
    {
        return Some(s.to_string());
    }
    let query_str = req.uri().query()?;
    for pair in query_str.split('&') {
        let Some((key, val)) = pair.split_once('=') else {
            continue;
        };
        if key == "apikey" {
            let decoded = urlencoding::decode(val).ok()?.into_owned();
            return if decoded.is_empty() {
                None
            } else {
                Some(decoded)
            };
        }
    }
    None
}

/// Middleware: require a `calendar`-scoped key. Wire onto a route via
/// `.layer(middleware::from_fn_with_state(state.clone(), require_calendar_scope))`.
/// Currently the only wired scope (gates `/api/calendar.ics` from
/// issue #115); the other scopes (`search`, `library:read`,
/// `library:write`) get their own per-scope middleware when their
/// first consumer lands.
pub async fn require_calendar_scope(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    check_scope(state, req, next, "calendar").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use axum::{Router, middleware};
    use tower::ServiceExt;

    /// Trivial inner handler so the middleware-rejection paths can
    /// be observed without coupling to any real route's behavior.
    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn router_with_calendar_guard(state: AppState) -> Router {
        Router::new()
            .route("/protected", get(ok_handler))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_calendar_scope,
            ))
    }

    #[tokio::test]
    async fn missing_key_returns_401() {
        let pool = crate::test_support::in_memory_pool().await;
        let state = crate::test_support::build_test_app_state(pool, None);
        let app = router_with_calendar_guard(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// PR #165 review — pin the empty-`?apikey=` rejection. Without
    /// the `is_empty()` guard in `extract_api_key`'s query branch,
    /// a request with `?apikey=` (no value) would hit
    /// `lookup_by_plaintext` with an empty string and silently match
    /// any row whose `key` column is empty (a real shape on dev-
    /// branch upgraders from the previous hash+encrypted schema,
    /// since SQLite can't add UNIQUE through ALTER ADD COLUMN).
    #[tokio::test]
    async fn empty_apikey_query_returns_401() {
        let pool = crate::test_support::in_memory_pool().await;
        // Insert a row with the upgrader-shape empty `key` column to
        // model the worst case: bypass `create()` (which generates
        // a 64-char hex plaintext) so the row's `key` is literally
        // the empty string the empty-`?apikey=` value would match.
        sqlx::query("INSERT INTO api_keys (name, key, scopes, enabled) VALUES (?, ?, ?, ?)")
            .bind("legacy-upgrader")
            .bind("")
            .bind("[\"calendar\"]")
            .bind(1_i64)
            .execute(&pool)
            .await
            .unwrap();
        let state = crate::test_support::build_test_app_state(pool, None);
        let app = router_with_calendar_guard(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/protected?apikey=")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_key_returns_401() {
        let pool = crate::test_support::in_memory_pool().await;
        let state = crate::test_support::build_test_app_state(pool, None);
        let app = router_with_calendar_guard(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/protected?apikey=not-a-real-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn key_missing_scope_returns_403() {
        // A `search`-scoped key shouldn't satisfy `calendar`. Pin
        // the default-deny policy: the *exact* required scope (or
        // `admin`) is the only thing that grants access.
        let pool = crate::test_support::in_memory_pool().await;
        let (_, plaintext) = api_key::create(&pool, "search-only", &["search".to_string()])
            .await
            .unwrap();
        let state = crate::test_support::build_test_app_state(pool, None);
        let app = router_with_calendar_guard(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/protected?apikey={plaintext}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn calendar_scoped_key_grants_access() {
        let pool = crate::test_support::in_memory_pool().await;
        let (_, plaintext) = api_key::create(&pool, "cal", &["calendar".to_string()])
            .await
            .unwrap();
        let state = crate::test_support::build_test_app_state(pool, None);
        let app = router_with_calendar_guard(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/protected?apikey={plaintext}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_scope_grants_calendar_access() {
        // admin is the universal scope — pin it at the middleware
        // layer too, not just on `ApiKey::grants`, so a future
        // refactor that moves the special-case can't silently break
        // the all-access semantics.
        let pool = crate::test_support::in_memory_pool().await;
        let (_, plaintext) = api_key::create(&pool, "root", &["admin".to_string()])
            .await
            .unwrap();
        let state = crate::test_support::build_test_app_state(pool, None);
        let app = router_with_calendar_guard(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/protected?apikey={plaintext}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn x_api_key_header_path_works() {
        // Header-precedence + body-shape pin: header form satisfies
        // a calendar-scoped request without the query string.
        let pool = crate::test_support::in_memory_pool().await;
        let (_, plaintext) = api_key::create(&pool, "cal", &["calendar".to_string()])
            .await
            .unwrap();
        let state = crate::test_support::build_test_app_state(pool, None);
        let app = router_with_calendar_guard(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/protected")
                    .header("X-Api-Key", &plaintext)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn disabled_key_returns_401() {
        // lookup_by_plaintext filters disabled rows; pin that
        // disabled keys behave identically to deleted keys from
        // the middleware's POV (401, not 403).
        let pool = crate::test_support::in_memory_pool().await;
        let (id, plaintext) = api_key::create(&pool, "cal", &["calendar".to_string()])
            .await
            .unwrap();
        api_key::set_enabled(&pool, id, false).await.unwrap();
        let state = crate::test_support::build_test_app_state(pool, None);
        let app = router_with_calendar_guard(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/protected?apikey={plaintext}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
