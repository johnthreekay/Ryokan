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
use crate::models::{config, login_device, session, user};
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
#[cfg(test)]
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
    classify_login_count(count)
}

/// Count an attempt against every bucket in `keys` as it starts, and
/// classify each, in one critical section. The handler used to check
/// first and record the failure only after the ~50 ms bcrypt verify, so
/// a burst of parallel requests all passed the check before any failure
/// landed and each got a real verdict: hundreds of guesses a minute
/// instead of five. A success calls [`login_clear`], so the reservation
/// only sticks for failures. Past the hard cap a bucket stops growing.
///
/// Only an attempt that every bucket allows creates a bucket. A
/// throttled attempt gets no verdict whatever its username, so it counts
/// only against buckets that already exist: otherwise one client sending
/// a fresh username per request would add a bucket per request until the
/// hourly sweep.
pub(crate) fn login_attempt(keys: &[String]) -> Vec<LoginCheck> {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let now = Instant::now();
    let cutoff = now - LOGIN_WINDOW;
    let tiers: Vec<LoginCheck> = keys
        .iter()
        .map(|key| {
            let count = guard.get_mut(key.as_str()).map_or(0, |times| {
                times.retain(|t| *t > cutoff);
                times.len()
            });
            classify_login_count(count)
        })
        .collect();
    let allowed = tiers.iter().all(|tier| *tier == LoginCheck::Allow);
    for key in keys {
        let times = if allowed {
            Some(guard.entry(key.clone()).or_default())
        } else {
            guard.get_mut(key.as_str())
        };
        if let Some(times) = times
            && times.len() < LOGIN_HARD_CAP
        {
            times.push(now);
        }
    }
    tiers
}

/// The tier for a bucket that already holds `count` attempts.
fn classify_login_count(count: usize) -> LoginCheck {
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
#[cfg(test)]
pub(crate) fn login_record_failure(key: &str) {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let entry = guard.entry(key.to_string()).or_default();
    let cutoff = Instant::now() - LOGIN_WINDOW;
    entry.retain(|t| *t > cutoff);
    entry.push(Instant::now());
}

/// The per-IP bucket for wrong API keys, beside the login buckets.
fn api_key_bucket(req: &Request<Body>) -> String {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let ip = client_ip_from_request(req.headers(), peer);
    format!("key:{}", ip.chars().take(64).collect::<String>())
}

/// Whether this client has sent too many wrong API keys lately (same
/// window and soft cap as logins). The Sonarr / Radarr keys can be any
/// string the user typed, and nothing limited how fast one could be
/// guessed. Only failures count, so a working client is never slowed.
pub(crate) fn api_key_throttled(req: &Request<Body>) -> bool {
    let key = api_key_bucket(req);
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let cutoff = Instant::now() - LOGIN_WINDOW;
    match guard.get_mut(&key) {
        Some(times) => {
            times.retain(|t| *t > cutoff);
            times.len() >= LOGIN_MAX_FAILURES
        }
        None => false,
    }
}

/// Count a wrong API key against this client.
pub(crate) fn api_key_failed(req: &Request<Body>) {
    let key = api_key_bucket(req);
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let cutoff = Instant::now() - LOGIN_WINDOW;
    let times = guard.entry(key).or_default();
    times.retain(|t| *t > cutoff);
    if times.len() < LOGIN_HARD_CAP {
        times.push(Instant::now());
    }
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
/// The value of cookie `name` in the request's `Cookie` header.
fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    header
        .split(';')
        .find_map(|pair| pair.trim().strip_prefix(name)?.strip_prefix('='))
}

/// The per-username throttle bucket. Hashed, so each username costs the
/// map a fixed 66 bytes: the key used to be the whole submitted username
/// (anything up to the 2 MB form limit), kept for up to an hour, which
/// let unauthenticated requests grow the map by hundreds of MB.
fn user_bucket_key(username: &str) -> String {
    use sha2::Digest;
    let normalized = username.trim().to_ascii_lowercase();
    format!(
        "u:{}",
        hex::encode(sha2::Sha256::digest(normalized.as_bytes()))
    )
}

/// Mint a device for `user_id` and return its `Set-Cookie` value, or
/// `None` (logged) when the insert fails; the login itself still works.
async fn new_device_cookie(
    db: &sqlx::SqlitePool,
    user_id: i64,
    headers: &HeaderMap,
) -> Option<String> {
    match login_device::create(db, user_id).await {
        Ok(token) => {
            let secure = if cookie_secure_for(headers) {
                "; Secure"
            } else {
                ""
            };
            Some(format!(
                "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
                login_device::COOKIE,
                token,
                login_device::MAX_AGE_DAYS * 86_400,
                secure
            ))
        }
        Err(e) => {
            tracing::warn!("could not record the login device: {e}");
            None
        }
    }
}

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
#[cfg(test)]
pub(crate) fn url_host(value: &str) -> Option<String> {
    url_authority(value).map(|(host, _)| host)
}

/// A host and its port, as compared by the CSRF check.
type Authority = (String, Option<u16>);

/// Host and port of an `Origin` / `Referer` value. The port is the URL's
/// own, else the scheme's default (80 / 443), so `https://x` and
/// `https://x:443` compare equal.
pub(crate) fn url_authority(value: &str) -> Option<Authority> {
    let (scheme, after_scheme) = value.split_once("://")?;
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..end];
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = split_authority(authority)?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    Some((host, port.or(default_port)))
}

/// `host[:port]` as a lowercased host (an IPv6 literal keeps its
/// brackets) and the port, if any. Splitting at the first `:` used to
/// turn every IPv6 literal into `[`.
pub(crate) fn split_authority(authority: &str) -> Option<Authority> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?.parse().ok()?),
        };
        return Some((format!("[{}]", inner.to_ascii_lowercase()), port));
    }
    match authority.split_once(':') {
        Some((host, port)) if !host.is_empty() => {
            Some((host.to_ascii_lowercase(), Some(port.parse().ok()?)))
        }
        Some(_) => None,
        None => Some((authority.to_ascii_lowercase(), None)),
    }
}

pub(crate) fn allowed_host_matches_with_trust(req: &Request<Body>, trust: bool) -> Vec<Authority> {
    hosts_from_headers(req.headers(), trust)
}

fn hosts_from_headers(headers: &HeaderMap, trust: bool) -> Vec<Authority> {
    let mut hosts = Vec::new();
    if let Some(h) = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(split_authority)
    {
        hosts.push(h);
    }
    if trust
        && let Some(raw) = headers
            .get("x-forwarded-host")
            .and_then(|v| v.to_str().ok())
    {
        // Proxies disagree on whether this carries their own listen port,
        // so a forwarded host is compared without one, as before.
        hosts.extend(
            raw.split(',')
                .filter_map(split_authority)
                .map(|(host, _)| (host, None)),
        );
    }
    hosts
}

/// Whether the browser's `origin` is one of the hosts this request was
/// addressed to. When the Host header names a port, the origin's port
/// has to match it: comparing hosts alone let any other app on the same
/// machine (`http://192.168.1.10:8080`) submit forms to Ryokan on :8978,
/// and `SameSite` ignores ports, so the session cookie went along. A
/// Host header without a port (a reverse proxy that forwards `$host`)
/// says nothing about the port, so only the host is compared there.
fn origin_matches(origin: &Authority, hosts: &[Authority]) -> bool {
    hosts.iter().any(|(host, port)| {
        host == &origin.0
            && match (port, origin.1) {
                (Some(port), Some(origin_port)) => *port == origin_port,
                _ => true,
            }
    })
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
        return match url_authority(origin) {
            Some(h) if origin_matches(&h, &hosts) => Ok(()),
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
        return match url_authority(referer) {
            Some(h) if origin_matches(&h, &hosts) => Ok(()),
            Some(_) => Err("referer host mismatch"),
            None => Err("malformed Referer header"),
        };
    }

    Err("missing Origin and Referer headers")
}

/// Whether a page may show the text in its flash query (`?msg=` /
/// `?err=` on Settings, `?message=` / `?error=` on System). Ryokan's own
/// redirects after a save are same-origin navigations; a link from
/// anywhere else is not, and showing its text would put the sender's
/// words in Ryokan's banner ("your session expired, sign in at ...").
/// A browser without `Sec-Fetch-Site` falls back to the Referer, which
/// `Referrer-Policy: same-origin` sends on Ryokan's own navigations.
pub(crate) fn flash_allowed(headers: &HeaderMap) -> bool {
    flash_allowed_with_trust(headers, *TRUST_PROXY_HEADERS)
}

pub(crate) fn flash_allowed_with_trust(headers: &HeaderMap, trust: bool) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site
            .to_str()
            .is_ok_and(|s| s.eq_ignore_ascii_case("same-origin"));
    }
    let hosts = hosts_from_headers(headers, trust);
    headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(url_authority)
        .is_some_and(|referer| !hosts.is_empty() && origin_matches(&referer, &hosts))
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

/// Refuse a cross-site navigation to a GET that does something: builds
/// a backup, starts an OAuth attempt. `SameSite=Lax` sends the session
/// cookie on a top-level GET, so a link on another site could set these
/// off, and the CSRF check covers unsafe methods only. Only
/// `Sec-Fetch-Site: same-origin`, `none` (typed, bookmarked) or no header
/// at all (curl, a script) passes. `same-site` is refused too: it ignores
/// the port, so every other service on the same host (and every sibling
/// subdomain) counts as same-site.
pub(crate) fn refuse_cross_site_get(headers: &HeaderMap) -> Option<Response> {
    match headers.get("sec-fetch-site").map(|v| v.to_str()) {
        None => None,
        Some(Ok(site))
            if site.eq_ignore_ascii_case("same-origin") || site.eq_ignore_ascii_case("none") =>
        {
            None
        }
        Some(_) => Some(
            (
                StatusCode::FORBIDDEN,
                "Open this from Ryokan, not from a link on another site.",
            )
                .into_response(),
        ),
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

/// Shortest admin password `/setup` accepts.
pub(crate) const MIN_PASSWORD_CHARS: usize = 8;
/// Longest: bcrypt ignores everything past byte 72.
pub(crate) const MAX_PASSWORD_BYTES: usize = 72;

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

    // bcrypt reads only the first 72 bytes, so anything after them was
    // silently not part of the password; and nothing stopped a
    // one-character admin password.
    if form.password.chars().count() < MIN_PASSWORD_CHARS {
        let template = SetupTemplate {
            error: Some(format!(
                "Use a password of at least {MIN_PASSWORD_CHARS} characters."
            )),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }
    if form.password.len() > MAX_PASSWORD_BYTES {
        let template = SetupTemplate {
            error: Some(format!(
                "Use a password of at most {MAX_PASSWORD_BYTES} bytes; longer ones are cut there."
            )),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }

    // The `has_users` gate above runs before the ~50ms bcrypt hash, so
    // two submissions racing through it both pass it. `create_first_user`
    // re-checks inside the insert statement and only one of them gets
    // the row; the other lands on the login page like a late submit.
    match user::create_first_user(&state.db, form.username.trim(), &form.password).await {
        Ok(None) => {
            logger::warn(
                &state.db,
                LogCategory::Auth,
                &format!(
                    "Setup refused for '{}': an account was created first",
                    sanitize_for_log(form.username.trim())
                ),
                "",
            )
            .await;
            Redirect::to("/login").into_response()
        }
        Ok(Some(user_id)) => {
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

            let mut response = Response::builder()
                .status(StatusCode::SEE_OTHER)
                // The new account's first stop is the library step
                // (`handlers::settings::setup_library`), which can be
                // skipped.
                .header(header::LOCATION, "/setup/library")
                .header(header::SET_COOKIE, set_session_cookie(&token, &headers));
            if let Some(device) = new_device_cookie(&state.db, user_id, &headers).await {
                response = response.header(header::SET_COOKIE, device);
            }
            response
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
    let ip_key = format!("ip:{}", ip.chars().take(64).collect::<String>());
    let user_key = user_bucket_key(&form.username);
    let safe_username = sanitize_for_log(&form.username);

    // A browser that has logged in before is throttled on its own
    // bucket rather than the username's and the IP's, so someone else's
    // failed attempts can't lock it out (`models::login_device`).
    let known_device = match cookie_value(&headers, login_device::COOKIE) {
        Some(token) if login_device::is_known(&state.db, token).await => {
            Some(format!("d:{}", login_device::hash(token)))
        }
        _ => None,
    };
    let buckets: Vec<String> = match &known_device {
        Some(device_key) => vec![device_key.clone()],
        None => vec![user_key, ip_key],
    };

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
    //
    // Every attempt is counted as it starts (`login_attempt`); a success
    // clears its buckets below.
    let tiers = login_attempt(&buckets);
    let hard_throttled = tiers.contains(&LoginCheck::HardThrottled);
    let rate_limited = tiers.iter().any(|tier| *tier != LoginCheck::Allow);

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

    // One log row per tripped bucket per minute: a throttled client can
    // keep sending, and each line is a database row. Keyed on the
    // buckets (fixed size) rather than the client, so a username under
    // attack from many addresses is one line, not one per address.
    let tripped: Vec<&str> = buckets
        .iter()
        .zip(&tiers)
        .filter(|(_, tier)| **tier != LoginCheck::Allow)
        .map(|(key, _)| key.as_str())
        .collect();
    if rate_limited
        && !logger::first_in_window(
            &format!("login-throttled:{}", tripped.join(",")),
            LOGIN_WINDOW,
        )
    {
        let template = LoginTemplate {
            error: Some("Too many failed attempts. Please wait a minute and try again.".into()),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }
    if rate_limited {
        logger::warn(
            &state.db,
            LogCategory::Auth,
            &format!(
                "Login rate-limited ({}): {} from {}",
                if hard_throttled { "hard" } else { "soft" },
                safe_username,
                sanitize_for_log(&ip)
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
            for key in &buckets {
                login_clear(key);
            }
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

            let mut response = Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/")
                .header(header::SET_COOKIE, set_session_cookie(&token, &headers));
            if known_device.is_none()
                && let Some(device) = new_device_cookie(&state.db, u.id, &headers).await
            {
                response = response.header(header::SET_COOKIE, device);
            }
            response
                .body(Body::empty())
                .expect("login-redirect response uses only static headers, should always build")
                .into_response()
        }
        _ => {
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
