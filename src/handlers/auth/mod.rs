use askama::Template;
use axum::{
    Form,
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, Request, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, session, user};
use crate::services::logger;

// ---------- Login rate limiting ----------
//
// In-process throttle: reject once a given key has accumulated 5 failed
// logins in a sliding 60-second window. We track two keys per attempt —
// one per username and one per client IP — so neither a per-account nor a
// distributed-across-usernames-from-one-box brute force can slip through.
// Keeping this in memory is fine for the self-hosted PVR deployment: a
// process restart resets the state, but an attacker sustaining 5/min across
// restarts is indistinguishable from an unlimited attacker in practice.

pub(crate) const LOGIN_WINDOW: Duration = Duration::from_secs(60);
pub(crate) const LOGIN_MAX_FAILURES: usize = 5;
/// Hard cap — past this many failures in the window, we stop running
/// `verify_user` entirely and return an immediate throttled response.
/// The soft cap (LOGIN_MAX_FAILURES) still equalizes wall time with a
/// bcrypt call to avoid leaking whether the throttle has tripped; the
/// hard cap is a DoS guard for the pathological case where a single key
/// keeps hammering the endpoint — past the hard cap we'd rather leak a
/// faint timing side channel than burn 50 ms of CPU per attempt forever.
pub(crate) const LOGIN_HARD_CAP: usize = 20;

static LOGIN_FAILURES: LazyLock<Mutex<HashMap<String, Vec<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Outcome of a rate-limit check. Distinguishes the two throttle tiers
/// so the login handler can choose between "equalize timing by running
/// bcrypt anyway" (soft) and "abort before any CPU work" (hard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginCheck {
    /// Under the soft cap — run the full verify path.
    Allow,
    /// Over the soft cap but under the hard cap. The caller still runs
    /// `verify_user` to equalize wall time, then ignores the result.
    SoftThrottled,
    /// Over the hard cap. Skip bcrypt entirely and return throttled.
    HardThrottled,
}

/// Classifies `key` against the rate-limit window. Always sweeps expired
/// entries for `key` as a side effect, and drops the map entry entirely
/// when its Vec empties out so rotated usernames / spoofed X-F-F values
/// can't grow LOGIN_FAILURES unboundedly (one idle key per probe forever).
pub(crate) fn login_check(key: &str) -> LoginCheck {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let cutoff = Instant::now() - LOGIN_WINDOW;
    let (count, empty) = {
        let entry = guard.entry(key.to_string()).or_default();
        entry.retain(|t| *t > cutoff);
        (entry.len(), entry.is_empty())
    };
    if empty {
        guard.remove(key);
    }
    if count >= LOGIN_HARD_CAP {
        LoginCheck::HardThrottled
    } else if count >= LOGIN_MAX_FAILURES {
        LoginCheck::SoftThrottled
    } else {
        LoginCheck::Allow
    }
}

/// Walks every entry in LOGIN_FAILURES, prunes expired timestamps, and
/// drops buckets that empty out. Call from the periodic cleanup task so
/// idle keys (IPs/usernames that failed once an hour ago and never came
/// back) don't linger forever — the per-request sweep in `login_check`
/// only reaches buckets that are actively being touched.
pub fn sweep_login_failures() {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let cutoff = Instant::now() - LOGIN_WINDOW;
    guard.retain(|_, v| {
        v.retain(|t| *t > cutoff);
        !v.is_empty()
    });
}

/// Record a failed login attempt against `key`.
pub(crate) fn login_record_failure(key: &str) {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let entry = guard.entry(key.to_string()).or_default();
    let cutoff = Instant::now() - LOGIN_WINDOW;
    entry.retain(|t| *t > cutoff);
    entry.push(Instant::now());
}

/// Reset the counter for `key` after a successful login so a
/// legitimate user who mistyped a few times isn't locked out by
/// their own prior failures.
pub(crate) fn login_clear(key: &str) {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    guard.remove(key);
}

/// Whether to honor `X-Forwarded-For` / `X-Real-IP` / `X-Forwarded-Host`
/// from the request, or ignore them entirely and use the TCP peer address
/// as the ground truth. Read once at startup from `RYOKAN_TRUSTED_PROXY`
/// (values `1`, `true`, `yes`, `on` enable it, case-insensitive). Default
/// off because Ryokan's default bind is `0.0.0.0:8978` — a direct-exposure
/// deploy (no reverse proxy) is a common self-hosted shape, and in that
/// shape *any* HTTP client can set these headers freely. Trusting them by
/// default would let an attacker spoof a fresh IP per attempt and defeat
/// the per-IP login throttle. Flip this on only when Ryokan is behind a
/// proxy that overwrites the headers on ingress.
pub(crate) static TRUST_PROXY_HEADERS: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("RYOKAN_TRUSTED_PROXY")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
});

/// Client IP extraction. When `RYOKAN_TRUSTED_PROXY` is set, prefers the
/// leftmost `X-Forwarded-For` entry (the address the reverse proxy saw
/// from the outside world), falling back to `X-Real-IP`, then to the TCP
/// peer. When the flag is unset, ignores both headers and uses the TCP
/// peer directly so a direct-exposure deploy can't be bypassed by a
/// spoofed header.
///
/// Thin wrapper around [`client_ip_from_request_with_trust`] that reads
/// the `TRUST_PROXY_HEADERS` LazyLock. Split so tests can drive both
/// trust values without racing the process-wide env-var snapshot.
fn client_ip_from_request(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    client_ip_from_request_with_trust(headers, peer, *TRUST_PROXY_HEADERS)
}

pub(crate) fn client_ip_from_request_with_trust(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trust: bool,
) -> String {
    if trust {
        if let Some(h) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
            && let Some(first) = h.split(',').next()
        {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if let Some(h) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            let trimmed = h.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    match peer {
        Some(addr) => addr.ip().to_string(),
        None => "unknown".to_string(),
    }
}

/// Strip control characters and cap length before embedding a piece of
/// attacker-supplied text (e.g. `form.username`) in a log line. Keeps
/// newlines / terminal escapes / multi-kilobyte probes from showing up
/// in the auth_log table and the tracing stream.
///
/// Default cap is 64 chars — appropriate for usernames + identifier-
/// shaped fields. Longer attacker-controlled strings (release titles,
/// indexer names, autobrr filter labels) should call
/// [`sanitize_for_log_capped`] with a larger cap so their tail isn't
/// truncated; the *security* concern here is the control-char filter,
/// not the length, and a release name without its CRC / extension is
/// noticeably harder to grep for in System → Logs.
pub(crate) fn sanitize_for_log(s: &str) -> String {
    sanitize_for_log_capped(s, 64)
}

/// Length-parameterized variant of [`sanitize_for_log`]. Same control-
/// char filter and trim, configurable take-N. Use 256 for release
/// titles / indexer names / filter labels; the larger budget still
/// truncates a multi-KB probe but preserves a normal anime release
/// title intact.
pub(crate) fn sanitize_for_log_capped(s: &str, max_len: usize) -> String {
    let trimmed = s.trim();
    trimmed
        .chars()
        .filter(|c| !c.is_control())
        .take(max_len)
        .collect()
}

/// Whether to force `Secure` onto the session cookie regardless of how the
/// request arrived. Read once at startup from `RYOKAN_COOKIE_SECURE`
/// (values `1`, `true`, `yes`, `on` enable it, case-insensitive). Default
/// off so `cargo run` on localhost keeps working over HTTP. Most HTTPS
/// deployments never need it: behind a trusted proxy the flag is inferred
/// per request from `X-Forwarded-Proto` (see [`cookie_secure_for`]), so
/// this is the escape hatch for a proxy that doesn't send that header.
static COOKIE_SECURE: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("RYOKAN_COOKIE_SECURE")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
});

// ---------- Templates ----------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "forgot_password.html")]
struct ForgotPasswordTemplate;

// ---------- Form data ----------

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
pub struct SetupForm {
    username: String,
    password: String,
    confirm: String,
}

// ---------- Helpers ----------

fn get_session_token(req: &Request<Body>) -> Option<String> {
    let cookie_header = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix("session=") {
            return Some(value.to_string());
        }
    }
    None
}

/// Whether the session cookie should carry `Secure` for this request.
/// Mirrors Sonarr's cookie auth, which marks the cookie `Secure` only when
/// the request itself came over HTTPS: Ryokan never terminates TLS, so
/// "came over HTTPS" means a trusted reverse proxy said so via
/// `X-Forwarded-Proto: https`. Without `RYOKAN_TRUSTED_PROXY` the header
/// is ignored (any client could send it, and a `Secure` cookie handed out
/// over plain HTTP is never sent back, which locks the user out).
/// `RYOKAN_COOKIE_SECURE` forces it on for proxies that omit the header.
fn cookie_secure_for(headers: &HeaderMap) -> bool {
    cookie_secure_for_with(headers, *COOKIE_SECURE, *TRUST_PROXY_HEADERS)
}

pub(crate) fn cookie_secure_for_with(headers: &HeaderMap, forced: bool, trust_proxy: bool) -> bool {
    if forced {
        return true;
    }
    if !trust_proxy {
        return false;
    }
    // Chained proxies append: the leftmost entry is the scheme the
    // client used, same convention as `X-Forwarded-For`.
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|first| first.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

fn set_session_cookie(token: &str, headers: &HeaderMap) -> String {
    set_session_cookie_with_secure(token, cookie_secure_for(headers))
}

pub(crate) fn set_session_cookie_with_secure(token: &str, secure: bool) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800{}",
        token, secure_attr
    )
}

fn clear_session_cookie(headers: &HeaderMap) -> String {
    clear_session_cookie_with_secure(cookie_secure_for(headers))
}

pub(crate) fn clear_session_cookie_with_secure(secure: bool) -> String {
    // Match the Secure attribute on the set path — some browsers refuse to
    // clear a Secure cookie from a non-Secure response, but the reverse is
    // harmless, so mirror whatever the set path emitted.
    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        secure_attr
    )
}

// ---------- CSRF helpers ----------

/// Extract the host portion (without scheme or port) from an Origin or
/// Referer header value. Returns None if the value is not a well-formed
/// absolute URL we can reason about.
pub(crate) fn url_host(value: &str) -> Option<String> {
    // Strip scheme.
    let after_scheme = value.split_once("://").map(|(_, rest)| rest)?;
    // Host ends at the first `/`, `?`, `#`, or end of string.
    let host_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let host_with_port = &after_scheme[..host_end];
    if host_with_port.is_empty() {
        return None;
    }
    // Strip port so we compare against the Host header cleanly — Host may or
    // may not include a port depending on the client, and we want to match
    // either way. An attacker can't spoof Host from a cross-origin browser
    // anyway, so we're comparing hosts for equality as a sanity check.
    let host_only = host_with_port
        .split_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_with_port);
    Some(host_only.to_ascii_lowercase())
}

pub(crate) fn host_of(req: &Request<Body>) -> Option<String> {
    let raw = req.headers().get(header::HOST)?.to_str().ok()?;
    let host_only = raw.split_once(':').map(|(h, _)| h).unwrap_or(raw);
    Some(host_only.to_ascii_lowercase())
}

/// Build the set of hosts that are acceptable matches for an Origin or
/// Referer check. Always includes the `Host` header. When `trust` is
/// set (driven by `RYOKAN_TRUSTED_PROXY` in production, or an
/// explicit flag in tests), also includes every entry in
/// `X-Forwarded-Host` so a reverse proxy that rewrites the upstream Host
/// header doesn't break every form POST — the browser sees the
/// externally-visible host and sends it in Origin, while the backend sees
/// the rewritten upstream name in Host, so without this check the two
/// never match and every POST is rejected as "origin host mismatch".
pub(crate) fn allowed_host_matches_with_trust(req: &Request<Body>, trust: bool) -> Vec<String> {
    let mut hosts = Vec::new();
    if let Some(h) = host_of(req) {
        hosts.push(h);
    }
    if trust
        && let Some(raw) = req
            .headers()
            .get("x-forwarded-host")
            .and_then(|v| v.to_str().ok())
    {
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let host_only = part.split_once(':').map(|(h, _)| h).unwrap_or(part);
            hosts.push(host_only.to_ascii_lowercase());
        }
    }
    hosts
}

/// Verify that a state-changing request came from the same origin this
/// server is serving. Uses the Origin header if present (modern browsers
/// set this on all POST/PUT/PATCH/DELETE requests, including cross-site
/// form submissions), falling back to Referer. This is the OWASP
/// "Verifying Origin With Standard Headers" CSRF mitigation and is
/// sufficient because an attacker page cannot forge either header from
/// cross-origin JavaScript.
///
/// Returns `Ok(())` if the method is safe (GET/HEAD/OPTIONS) or the
/// request is same-origin. Returns `Err` with a short reason otherwise.
fn verify_same_origin(req: &Request<Body>) -> Result<(), &'static str> {
    verify_same_origin_with_trust(req, *TRUST_PROXY_HEADERS)
}

pub(crate) fn verify_same_origin_with_trust(
    req: &Request<Body>,
    trust: bool,
) -> Result<(), &'static str> {
    match *req.method() {
        Method::GET | Method::HEAD | Method::OPTIONS => return Ok(()),
        _ => {}
    }

    let hosts = allowed_host_matches_with_trust(req, trust);
    if hosts.is_empty() {
        return Err("missing Host header");
    }

    // Prefer Origin (always set by browsers on unsafe methods).
    if let Some(origin) = req.headers().get("origin").and_then(|v| v.to_str().ok()) {
        // "null" is what browsers send for e.g. sandboxed iframes — never
        // same-origin by definition.
        if origin == "null" {
            return Err("null origin");
        }
        return match url_host(origin) {
            Some(h) if hosts.contains(&h) => Ok(()),
            Some(_) => Err("origin host mismatch"),
            None => Err("malformed Origin header"),
        };
    }

    // Fall back to Referer when Origin is absent (older clients, some
    // proxies). Reject if neither header is present — on POST from a real
    // browser at least one of them will be set.
    if let Some(referer) = req
        .headers()
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
    {
        return match url_host(referer) {
            Some(h) if hosts.contains(&h) => Ok(()),
            Some(_) => Err("referer host mismatch"),
            None => Err("malformed Referer header"),
        };
    }

    Err("missing Origin and Referer headers")
}

fn csrf_forbidden(reason: &str) -> Response {
    tracing::warn!("CSRF rejection: {}", reason);
    (
        StatusCode::FORBIDDEN,
        "Forbidden: cross-origin request rejected",
    )
        .into_response()
}

// ---------- Auth middleware ----------

pub async fn require_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // If no users exist, redirect to setup. Once a user has been created
    // the atomic flag on `AppState` pins to `true` for the rest of the
    // process lifetime, so the common case is a lock-free load instead of
    // a `SELECT COUNT(*) FROM users` round trip on every protected
    // request. The slow path only runs pre-setup or right after a clean
    // install, and promotes the flag as soon as the DB agrees.
    //
    // On a DB error we fall through to the session check instead of
    // redirecting to /setup — that mirrors the pre-cache behavior
    // (`if let Ok(false) = has_users { redirect }`) and avoids evicting
    // a real logged-in user to the setup form on a transient SQLite
    // hiccup during the very first request after boot (before `main.rs`
    // primes this flag). The session check below still rejects an
    // unauthenticated user anyway, so nothing bypasses auth.
    if !state.users_exist.load(std::sync::atomic::Ordering::Relaxed) {
        match user::has_users(&state.db).await {
            Ok(true) => {
                state
                    .users_exist
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            // hx-boost rollout Phase C — auth middleware redirects must
            // emit `HX-Redirect: ...` for boosted callers (boost arrives
            // here on a session-expired page click), or htmx will
            // inline-swap the `/setup`/`/login` page HTML into the
            // prior page's body. `htmx_aware_redirect_from_req` reads
            // the `HX-Request` header off the raw request; unauth
            // browser nav still gets the standard 303.
            Ok(false) => {
                return crate::handlers::responses::htmx_aware_redirect_from_req(&req, "/setup");
            }
            Err(_) => {}
        }
    }

    // Check session cookie.
    let token = match get_session_token(&req) {
        Some(t) => t,
        None => return crate::handlers::responses::htmx_aware_redirect_from_req(&req, "/login"),
    };

    match session::validate_session(&state.db, &token).await {
        Ok(Some(_user_id)) => {
            // Session is valid. Enforce same-origin on state-changing
            // requests to block CSRF — a malicious page at evil.com cannot
            // forge either Origin or Referer from cross-origin JS, so even
            // though the browser will attach our session cookie on top-level
            // form POSTs (SameSite=Lax permits this for GET-style
            // navigations, but the rejection here catches the rest), a
            // cross-origin POST is rejected.
            if let Err(reason) = verify_same_origin(&req) {
                return csrf_forbidden(reason);
            }
            next.run(req).await
        }
        _ => crate::handlers::responses::htmx_aware_redirect_from_req(&req, "/login"),
    }
}

/// CSRF middleware for the public `/login` and `/setup` POST paths. These
/// routes have no session to attach a token to, so we fall back to the
/// same Origin/Referer same-origin check used on authenticated routes.
/// An attacker's page cannot set either header to our host from
/// cross-origin JavaScript, so a drive-by POST to `/setup` from a
/// malicious site is rejected before `setup_submit` ever sees the form.
pub async fn csrf_public(req: Request<Body>, next: Next) -> Response {
    if let Err(reason) = verify_same_origin(&req) {
        return csrf_forbidden(reason);
    }
    next.run(req).await
}

// ---------- Setup ----------

pub async fn setup_page(State(state): State<AppState>) -> impl IntoResponse {
    // If users already exist, redirect to login.
    if let Ok(true) = user::has_users(&state.db).await {
        return Redirect::to("/login").into_response();
    }

    let template = SetupTemplate { error: None };
    Html(template.render().unwrap_or_default()).into_response()
}

pub async fn setup_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> impl IntoResponse {
    // Fail closed on a transient has_users error: an Ok(true) -> redirect
    // pattern (the prior code) treated Err(_) the same as Ok(false) and
    // let the form proceed, so a SQLite hiccup during a second admin's
    // setup attempt could create a second account. The UNIQUE(username)
    // constraint catches identical usernames, but a different username
    // through that window would have slipped past.
    match user::has_users(&state.db).await {
        Ok(false) => {} // proceed
        Ok(true) => return Redirect::to("/login").into_response(),
        Err(e) => {
            tracing::error!("setup_submit: has_users failed: {e}");
            let template = SetupTemplate {
                error: Some("Database error. Try again in a moment.".into()),
            };
            return Html(template.render().unwrap_or_default()).into_response();
        }
    }

    if form.username.trim().is_empty() || form.password.is_empty() {
        let template = SetupTemplate {
            error: Some("Username and password are required.".into()),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }

    if form.password != form.confirm {
        let template = SetupTemplate {
            error: Some("Passwords do not match.".into()),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }

    match user::create_user(&state.db, form.username.trim(), &form.password).await {
        Ok(user_id) => {
            logger::info(
                &state.db,
                LogCategory::Auth,
                &format!("Account created: {}", form.username.trim()),
                "",
            )
            .await;
            // Seed a default `config` row so the per-tab subform
            // handlers (settings_general_submit /
            // settings_quality_submit / settings_integrations_submit)
            // don't bail with their "No config row found — run /setup
            // first." guard the very first time the user opens
            // Settings. Pre-this-seed, /setup created the user but
            // never wrote a config row; the user opened Settings →
            // Connections, edited Jellyfin, hit Save, and got a
            // mysterious self-contradicting error since they HAD
            // just run /setup. `INSERT OR IGNORE` so a re-run of
            // setup somehow (shouldn't happen — has_users gate above
            // catches it) doesn't clobber an already-saved config.
            // Failure is non-fatal: the legacy bulk save handler at
            // POST /settings still works without a row, and a noisy
            // log is better than blocking account creation on a
            // config write.
            if let Err(e) = config::save_config(&state.db, &config::Config::default()).await {
                tracing::warn!(
                    "setup_submit: failed to seed default config row: {e} \
                     (subform saves will fail until a row exists; \
                     re-save from Settings → General to recover)"
                );
            }
            let token = session::create_session(&state.db, user_id)
                .await
                .unwrap_or_default();

            Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/")
                .header(header::SET_COOKIE, set_session_cookie(&token, &headers))
                .body(Body::empty())
                .expect("setup-redirect response uses only static headers, should always build")
                .into_response()
        }
        Err(e) => {
            let template = SetupTemplate {
                error: Some(format!("Failed to create account: {}", e)),
            };
            Html(template.render().unwrap_or_default()).into_response()
        }
    }
}

// ---------- Login ----------

pub async fn login_page(State(state): State<AppState>) -> impl IntoResponse {
    if let Ok(false) = user::has_users(&state.db).await {
        return Redirect::to("/setup").into_response();
    }

    let template = LoginTemplate { error: None };
    Html(template.render().unwrap_or_default()).into_response()
}

/// #39 — Account-recovery instructions rendered as a standalone
/// auth-page template (no nav, no logout link). Reached from the
/// "Forgot password?" link on `/login`, so it must be on the
/// unauthenticated route group — a locked-out user can't pass
/// `require_auth`, and that's the one page they need.
///
/// Rendering a dedicated template (rather than sharing `/help`)
/// keeps the recovery shell clean: unauthenticated visitors don't
/// see the authed top-nav or a Logout link they can't use, and the
/// rest of `/help`'s content (scoring tables, search tips, grab
/// instructions) stays behind the auth wall where it belongs.
pub async fn forgot_password_page() -> Html<String> {
    let template = ForgotPasswordTemplate;
    Html(template.render().unwrap_or_default())
}

pub async fn login_submit(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    // Resolve the bucket keys up front so we always rate-limit, even when
    // the incoming form has an empty username.
    let ip = client_ip_from_request(&headers, Some(peer_addr));
    let ip_key = format!("ip:{}", ip);
    let user_key = format!("u:{}", form.username.trim().to_ascii_lowercase());
    let safe_username = sanitize_for_log(&form.username);

    // Pre-check: figure out which throttle tier we're in.
    //
    // - Allow: run verify_user normally.
    // - SoftThrottled: still run verify_user below so the response pays
    //   ~50ms of bcrypt. Returning early here would leak to a probing
    //   attacker whether they're throttled (fast return) vs. just wrong
    //   (slow return), which is enough to confirm that per-user throttling
    //   has tripped — i.e., that the username is worth pounding from
    //   another IP. Equalizing the wall time closes that timing oracle.
    // - HardThrottled: past the hard cap, skip bcrypt entirely. A single
    //   key that's been failing for a minute straight is almost certainly
    //   an attacker — we'd rather leak a faint timing side channel than
    //   keep burning 50 ms of CPU per attempt forever. We still sleep a
    //   randomized ~30–80 ms before responding so the fast-return is not
    //   a crisp signal.
    let user_tier = login_check(&user_key);
    let ip_tier = login_check(&ip_key);
    let hard_throttled =
        user_tier == LoginCheck::HardThrottled || ip_tier == LoginCheck::HardThrottled;
    let rate_limited = hard_throttled
        || user_tier == LoginCheck::SoftThrottled
        || ip_tier == LoginCheck::SoftThrottled;

    // Run verify_user only when we're under the hard cap. Under soft
    // throttling we still pay bcrypt to preserve the equalized-timing
    // property; past the hard cap we drop it to protect the server.
    let verify_result = if hard_throttled {
        // Jittered sleep roughly the width of a bcrypt verify so the fast
        // return doesn't crisply flag the hard-cap transition. Uses a
        // cheap deterministic-but-per-request source (nanos of the
        // current instant) to avoid a full PRNG dep just for this.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let jitter_ms = 30 + (nanos as u64 % 50);
        tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        Ok(None)
    } else {
        user::verify_user(&state.db, &form.username, &form.password).await
    };

    if rate_limited {
        logger::warn(
            &state.db,
            LogCategory::Auth,
            &format!(
                "Login rate-limited ({}): {} from {}",
                if hard_throttled { "hard" } else { "soft" },
                safe_username,
                ip
            ),
            "",
        )
        .await;
        let template = LoginTemplate {
            error: Some("Too many failed attempts. Please wait a minute and try again.".into()),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }

    match verify_result {
        Ok(Some(u)) => {
            // Successful login — clear the counters so an honest user who
            // mistyped a few times isn't punished for their own typos.
            login_clear(&user_key);
            login_clear(&ip_key);
            logger::info(
                &state.db,
                LogCategory::Auth,
                &format!("Login: {}", safe_username),
                "",
            )
            .await;
            let token = session::create_session(&state.db, u.id)
                .await
                .unwrap_or_default();

            Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/")
                .header(header::SET_COOKIE, set_session_cookie(&token, &headers))
                .body(Body::empty())
                .expect("login-redirect response uses only static headers, should always build")
                .into_response()
        }
        _ => {
            login_record_failure(&user_key);
            login_record_failure(&ip_key);
            logger::warn(
                &state.db,
                LogCategory::Auth,
                &format!("Failed login attempt: {}", safe_username),
                "",
            )
            .await;
            let template = LoginTemplate {
                error: Some("Invalid username or password.".into()),
            };
            Html(template.render().unwrap_or_default()).into_response()
        }
    }
}

// ---------- Logout ----------

pub async fn logout(State(state): State<AppState>, req: Request<Body>) -> impl IntoResponse {
    if let Some(token) = get_session_token(&req) {
        let _ = session::delete_session(&state.db, &token).await;
    }

    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/login")
        .header(header::SET_COOKIE, clear_session_cookie(req.headers()))
        .body(Body::empty())
        .expect("logout-redirect response uses only static headers, should always build")
        .into_response()
}

// ---------- Test helpers ----------

/// Seed a specific failure timestamp against `key`. Test-only —
/// lets throttle tests pre-load old timestamps to exercise the
/// window-expiration sweep without sleeping for real wall time.
#[cfg(test)]
pub(crate) fn seed_login_failure_for_test(key: &str, at: Instant) {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    guard.entry(key.to_string()).or_default().push(at);
}

/// Read the recorded failure count for `key` — test-only inspection
/// helper. Returns 0 when the key has no bucket.
#[cfg(test)]
pub(crate) fn login_failure_count_for_test(key: &str) -> usize {
    let guard = LOGIN_FAILURES.lock().unwrap();
    guard.get(key).map(|v| v.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests;
