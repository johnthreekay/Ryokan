//! AniList rate-limit pacing and cooldown machinery.
//!
//! Split out of `anilist/mod.rs` because this file owns all of the process-wide
//! mutable state around AniList throttling (burst spacing, window reset,
//! cooldown-until, the rate-limit header snapshot) plus the pure helpers that
//! decide *when* to sleep and *for how long*. The module's public surface
//! (`anilist_cooldown_active`, `is_rate_limit_error`) is re-exported from the
//! parent so existing callers still reach it at `services::anilist::foo`.
//!
//! See `throttle_before_anilist_request` for the overall pacing strategy and
//! `classify_anilist_failure` for the failure-kind taxonomy (throttle vs.
//! unavailable vs. not-found) that downstream code keys MAL-fallback decisions
//! off of.

use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long to treat AniList as unavailable after a 429/5xx. If AniList sends a
/// `Retry-After` header we use that value instead (capped at 5 minutes).
pub(super) const ANILIST_COOLDOWN_DEFAULT: Duration = Duration::from_secs(60);
const ANILIST_COOLDOWN_MAX: Duration = Duration::from_secs(300);

/// Safety margin added to AL's `Retry-After` value (or to the default
/// cooldown when no Retry-After is present). Without this, waiting
/// exactly the duration AL hands back consistently lands the next
/// request at the window boundary and trips a fresh 429 — observed
/// live during a sweep where the second 429 followed the first by
/// roughly the original Retry-After value. 2s clears the boundary in
/// practice without meaningfully extending sweep time.
const COOLDOWN_SAFETY_MARGIN: Duration = Duration::from_secs(2);

/// Below this many `X-RateLimit-Remaining`, switch from "minimum
/// inter-request spacing" to "wait until window reset." Picked low so
/// we mostly run at full headroom-driven speed and only slow down when
/// we're about to bump the cap.
const REMAINING_HEADROOM_THRESHOLD: u32 = 3;

/// Per-minute pacing rate Ryokan throttles AL requests against. Used
/// both as the fallback when no rate-limit header has been seen yet
/// AND as a hard ceiling regardless of what `X-RateLimit-Limit`
/// reports — see `decide_wait` for the one-line clamp.
///
/// The ceiling exists because AL's `Limit` header is not trustworthy
/// during degraded operation: live observation 2026-05-03 had AL
/// continuing to return `Limit: 90` (the normal-mode value) while
/// actually enforcing the documented 30 req/min degraded cap. With
/// the original adapt-up-from-fallback logic that meant Ryokan paced
/// at 84 req/min and ate 429s every 30-60s through a 12-minute
/// sweep. Capping spacing at the documented degraded limit costs
/// ~50s on a 27-series sweep in normal mode (27/min vs ~84/min)
/// but eliminates the loop. The header's `Remaining` and `Reset`
/// fields are still trusted — those are about state within the
/// current window and don't depend on the window's reported size.
const ANILIST_LIMIT_FALLBACK: u32 = 30;

/// Per-response snapshot of AniList's rate-limit headers, used by
/// `throttle_before_anilist_request` to pace the next call without
/// guessing.
#[derive(Clone, Copy)]
struct RateLimitState {
    /// `X-RateLimit-Limit` — total requests in the current window.
    limit: u32,
    /// `X-RateLimit-Remaining` from the latest response.
    remaining: u32,
    /// `X-RateLimit-Reset` translated from a Unix timestamp into our
    /// monotonic clock at recording time. None when the header was
    /// absent, which AL only sends on 429s in normal operation.
    reset_at: Option<Instant>,
}

static RATE_LIMIT_STATE: LazyLock<StdMutex<Option<RateLimitState>>> =
    LazyLock::new(|| StdMutex::new(None));

/// Last time we sent a request to AniList. Used to enforce a minimum
/// inter-request spacing derived from the current per-minute limit, so
/// a relation walk that fires N back-to-back AL calls can't burst over
/// AL's burst limiter (which is documented but not header-exposed).
static LAST_AL_REQUEST: LazyLock<StdMutex<Option<Instant>>> = LazyLock::new(|| StdMutex::new(None));

/// Monotonically-set "AniList is in cooldown until Instant". Consulted at the
/// top of every search so that once we've learned AniList is rate-limiting us,
/// we stop wasting a round-trip per search (which was dragging Jikan into the
/// rate-limit bucket too).
static ANILIST_COOLDOWN_UNTIL: LazyLock<StdMutex<Option<Instant>>> =
    LazyLock::new(|| StdMutex::new(None));

/// Sliding 60-second window of AL request timestamps issued from
/// THIS process. Surfaced on 429s so a user staring at "AL says
/// remaining=0 but I'm not making any calls!" can see exactly how
/// many requests Ryokan itself fired in the last minute. If the
/// count is well under AL's 30/min cap and AL still 429s, the
/// remainder of the cap is being burned by something else on the
/// same IP — another tab on anilist.co (each page load makes
/// many GraphQL calls), a second Ryokan instance, an extension or
/// helper tool sharing the IP. Without this counter every 429 was
/// indistinguishable from "Ryokan over-fired" vs "external traffic
/// stole the budget" and the user had to guess.
///
/// The window is bounded by `max(N, oldest > now - 60s)`; entries
/// are popped from the front as they age out, capped at a defensive
/// `RECENT_AL_REQUESTS_MAX_LEN` so a runaway bug can't grow the
/// `VecDeque` without bound.
static RECENT_AL_REQUESTS: LazyLock<StdMutex<std::collections::VecDeque<Instant>>> =
    LazyLock::new(|| StdMutex::new(std::collections::VecDeque::new()));

/// Hard cap on the recent-requests deque length. The 60s window
/// should keep this comfortably under 90 (the highest AL limit
/// Ryokan plausibly paces against if it ever uplifts the clamp);
/// the larger ceiling defends against an unexpected burst, like a
/// runaway loop, growing the deque past reasonable size.
const RECENT_AL_REQUESTS_MAX_LEN: usize = 256;

/// Compute the minimum spacing between AL requests for a given
/// per-minute limit. 60s / limit, with a 10% safety pad on top so that
/// clock drift / measurement noise can't accidentally push us above
/// the cap. Returns a defensive 2s when limit is 0 (shouldn't happen).
fn min_inter_request(limit: u32) -> Duration {
    if limit == 0 {
        return Duration::from_secs(2);
    }
    Duration::from_millis((66_000 / limit as u64).max(100))
}

/// Capture rate-limit headers from the latest AL response. Called for
/// every AL response (success or error) so the next throttle decision
/// is based on AL's fresh count rather than our stale belief. Headers
/// AL doesn't send (e.g. `X-RateLimit-Reset` outside of throttling)
/// just leave the corresponding field at None / unchanged.
pub(super) fn record_rate_limit_headers(headers: &reqwest::header::HeaderMap) {
    let parse = |name: &str| -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    };

    let limit = parse("x-ratelimit-limit").map(|v| v as u32);
    let remaining = parse("x-ratelimit-remaining").map(|v| v as u32);
    let reset_unix = parse("x-ratelimit-reset");

    // If AL didn't send anything useful, leave existing state alone.
    if limit.is_none() && remaining.is_none() && reset_unix.is_none() {
        return;
    }

    let reset_at = reset_unix.and_then(|reset| {
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if reset > now_unix {
            // AniList's windows are a minute long. Clamped, and
            // `checked_add`: a header of u64::MAX overflowed `Instant` and
            // panicked every AniList response.
            let secs = (reset - now_unix).min(3600);
            Instant::now().checked_add(Duration::from_secs(secs))
        } else {
            None
        }
    });

    if let Ok(mut guard) = RATE_LIMIT_STATE.lock() {
        let prev = *guard;
        *guard = Some(RateLimitState {
            limit: limit
                .or(prev.map(|s| s.limit))
                .unwrap_or(ANILIST_LIMIT_FALLBACK),
            remaining: remaining.or(prev.map(|s| s.remaining)).unwrap_or(0),
            // Keep the prior reset_at if AL didn't send one this time —
            // it's the most recent ground truth we have for when the
            // window flips.
            reset_at: reset_at.or(prev.and_then(|s| s.reset_at)),
        });
    }
}

/// Pure throttle decision. Returns how long to sleep before the next
/// AniList request, given the latest rate-limit snapshot, the
/// timestamp of our last request (if any), and the current instant.
/// Extracted from `throttle_before_anilist_request` so the branching
/// is unit-testable without `tokio::time::sleep`.
///
/// Two strategies, in priority order:
///   - **Window-flip** (`remaining <= REMAINING_HEADROOM_THRESHOLD`
///     and `reset_at` is in the future): wait until the next window
///     opens. Returns the larger of (window-wait, burst-guard) so we
///     don't accidentally undercut burst spacing.
///   - **Burst guard** (always): don't exceed `60s / limit` between
///     consecutive requests. Returns 0 if the last request was long
///     enough ago.
fn decide_wait(
    state: Option<RateLimitState>,
    last_request: Option<Instant>,
    now: Instant,
) -> Duration {
    // Clamp the header-reported limit to `ANILIST_LIMIT_FALLBACK`
    // (the documented degraded cap). AL has been observed returning
    // `X-RateLimit-Limit: 90` while actually enforcing 30 req/min,
    // and the prior `unwrap_or(30)` adapt-up-from-fallback shape
    // would happily pace against 90 in that case. See the
    // `ANILIST_LIMIT_FALLBACK` doc-comment for the full incident.
    let limit = state
        .map(|s| s.limit)
        .unwrap_or(ANILIST_LIMIT_FALLBACK)
        .min(ANILIST_LIMIT_FALLBACK);
    let min_spacing = min_inter_request(limit);

    let burst_wait = match last_request {
        Some(last) => {
            let elapsed = now.saturating_duration_since(last);
            min_spacing.saturating_sub(elapsed)
        }
        None => Duration::ZERO,
    };

    if let Some(s) = state
        && s.remaining <= REMAINING_HEADROOM_THRESHOLD
        && let Some(reset_at) = s.reset_at
        && reset_at > now
    {
        // Same `+ COOLDOWN_SAFETY_MARGIN` as the cooldown path. AL's
        // `X-RateLimit-Reset` is integer-seconds and `Instant::now()`
        // came from `SystemTime::now().as_secs()` at recording time —
        // both truncate, so a real reset that's 0.4s away can be
        // recorded as 0s. A bare `+1s` margin is then collapsed by
        // truncation noise, lands at the boundary, and trips a fresh
        // 429 (live-reproduced during the 2026-05-02 sweep where
        // `remaining=0 wait=1` was followed by a 429 ~1.5s later). The
        // shared safety margin keeps the two paths in sync.
        let window_wait = reset_at.saturating_duration_since(now) + COOLDOWN_SAFETY_MARGIN;
        return window_wait.max(burst_wait);
    }

    burst_wait
}

/// Pace the next AniList request to stay inside the documented window
/// and burst limits, using the latest rate-limit headers as the source
/// of truth. Without this, a relation walk burst-fires N calls in
/// seconds — even when N is well under the per-minute limit, AL's
/// burst limiter trips and we eat a full minute of cooldown for what
/// would've been a 1-second pause if we'd just paced ourselves.
pub(super) async fn throttle_before_anilist_request() {
    // One caller at a time through decide / sleep / stamp. Without this,
    // concurrent callers (a series add hydrating metadata while the
    // airing refresh runs, a Seerr fan-out of auto-searches) all read the
    // same `LAST_AL_REQUEST`, computed the same wait and fired together,
    // which is the burst this function exists to prevent: a 429 and a
    // global cooldown. Same shape as `source_description::rate_limit`.
    static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _turn = TURN.lock().await;

    let snap = RATE_LIMIT_STATE.lock().ok().and_then(|g| *g);
    let last = LAST_AL_REQUEST.lock().ok().and_then(|g| *g);
    let wait = decide_wait(snap, last, Instant::now());

    if !wait.is_zero() {
        // Only log the headroom-low case — burst-guard waits are
        // sub-second and dominate normal operation, no point spamming.
        if let Some(s) = snap
            && s.remaining <= REMAINING_HEADROOM_THRESHOLD
            && wait > Duration::from_secs(1)
        {
            tracing::info!(
                target: "ryokan::anilist",
                remaining = s.remaining,
                wait_secs = wait.as_secs(),
                "AniList rate-limit headroom low; pausing until window resets"
            );
        }
        tokio::time::sleep(wait).await;
    }

    let now = Instant::now();
    if let Ok(mut guard) = LAST_AL_REQUEST.lock() {
        *guard = Some(now);
    }
    record_recent_al_request(now);
}

/// Append `at` to the recent-requests deque and prune entries older
/// than 60s. Called from `throttle_before_anilist_request` after the
/// throttle decision so every Ryokan-issued AL request is counted —
/// the window is what surfaces in the 429 diagnostic.
fn record_recent_al_request(at: Instant) {
    if let Ok(mut guard) = RECENT_AL_REQUESTS.lock() {
        let cutoff = at.checked_sub(Duration::from_secs(60));
        if let Some(cutoff) = cutoff {
            while matches!(guard.front(), Some(t) if *t < cutoff) {
                guard.pop_front();
            }
        }
        // Defensive cap — see `RECENT_AL_REQUESTS_MAX_LEN`. Drop
        // the oldest if we've somehow grown past the ceiling.
        while guard.len() >= RECENT_AL_REQUESTS_MAX_LEN {
            guard.pop_front();
        }
        guard.push_back(at);
    }
}

/// Number of AL requests Ryokan issued from this process in the
/// last 60 seconds. Used to disambiguate "Ryokan over-fired"
/// (count at or above the 30/min cap) from "external traffic stole
/// the budget" (count is small but AL says `remaining=0`) on a
/// 429. Pruning happens lazily — callers see entries up to 60s
/// old by definition because every `throttle_before_anilist_request`
/// call prunes before pushing.
pub fn recent_al_request_count_60s() -> usize {
    let now = Instant::now();
    let cutoff = match now.checked_sub(Duration::from_secs(60)) {
        Some(c) => c,
        None => return 0,
    };
    if let Ok(guard) = RECENT_AL_REQUESTS.lock() {
        guard.iter().filter(|t| **t >= cutoff).count()
    } else {
        0
    }
}

/// Compute cooldown duration on a 429 from AL's headers, preferring
/// `X-RateLimit-Reset` (absolute timestamp, the most precise signal AL
/// gives us) over `Retry-After` (relative seconds), with the configured
/// default as the last fallback. The result is capped and padded the
/// same way as the default-only path.
pub(super) fn cooldown_from_headers(
    headers: &reqwest::header::HeaderMap,
    default_dur: Duration,
) -> Duration {
    let parse_u64 = |name: &str| -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    };

    if let Some(reset_unix) = parse_u64("x-ratelimit-reset") {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if reset_unix > now_unix {
            let raw = Duration::from_secs(reset_unix - now_unix).min(ANILIST_COOLDOWN_MAX);
            return raw + COOLDOWN_SAFETY_MARGIN;
        }
    }

    let retry_after = parse_u64("retry-after");
    compute_cooldown_duration(retry_after, default_dur)
}

pub fn anilist_cooldown_active() -> bool {
    if let Ok(guard) = ANILIST_COOLDOWN_UNTIL.lock()
        && let Some(until) = *guard
    {
        return Instant::now() < until;
    }
    false
}

/// Single source of truth for "this AniList error means we're being
/// throttled / blocked." Callers use this to decide whether to
/// defer-and-retry (preserve AL fidelity) vs. treat AL as down and fall
/// back to MAL.
///
/// All errors classified as throttle by `classify_anilist_failure` carry
/// the `AniList rate-limited` prefix; the in-process cooldown short-circuit
/// uses `cooldown active`. Non-throttle failures (5xx, 403 with
/// non-Cloudflare body, parse errors) deliberately do *not* match — those
/// are "AL is genuinely down" and fallback callers should substitute MAL.
pub fn is_rate_limit_error(err: &str) -> bool {
    err.contains("AniList rate-limited") || err.contains("cooldown active")
}

/// Classification of an AniList failure response. Drives both the
/// error-string wording and whether `set_anilist_cooldown` is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AniListFailureKind {
    /// Throttle: 429, Cloudflare challenge, or GraphQL-level "too many
    /// requests". Caller should defer-and-retry; do not substitute MAL.
    RateLimited,
    /// AL itself is unhealthy: 5xx, body parse failure, or a 403 whose
    /// body doesn't look like Cloudflare (suggests an AL-side problem
    /// rather than upstream throttling). Caller may fall back to MAL.
    Unavailable,
    /// AL responded successfully but the requested entity doesn't exist.
    NotFound,
}

/// Inspect status + body to figure out *why* AniList rejected the call.
/// Status alone is ambiguous (especially 403 — Cloudflare vs. AL itself
/// vs. auth issue), so we also look for body markers. Returns the kind
/// plus a tagged error string the caller can return to its consumers;
/// downstream code matches on the tag prefix (`AniList rate-limited` /
/// `AniList unavailable` / `AniList not found`) rather than HTTP codes,
/// so adding new wordings doesn't break the policy.
pub(super) fn classify_anilist_failure(
    status: reqwest::StatusCode,
    body_text: &str,
) -> (AniListFailureKind, String) {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body_text).ok();
    let gql_msg = parsed.as_ref().and_then(extract_graphql_error);

    // Cloudflare HTML challenge — distinctive body markers, can come back
    // with any 4xx/5xx status. Treat as throttle.
    let lower = body_text.to_ascii_lowercase();
    let is_cloudflare = lower.contains("cf-ray")
        || lower.contains("just a moment")
        || lower.contains("attention required")
        || (lower.contains("cloudflare") && lower.contains("<html"));
    if is_cloudflare {
        return (
            AniListFailureKind::RateLimited,
            format!(
                "AniList rate-limited: Cloudflare challenge (HTTP {})",
                status
            ),
        );
    }

    // GraphQL-level throttle hint, regardless of status.
    if let Some(msg) = &gql_msg {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("too many requests")
            || lower.contains("rate limit")
            || lower.contains("throttled")
        {
            return (
                AniListFailureKind::RateLimited,
                format!("AniList rate-limited: {} (HTTP {})", msg, status),
            );
        }
    }

    let detail = gql_msg.unwrap_or_else(|| excerpt(body_text));
    match status.as_u16() {
        429 => (
            AniListFailureKind::RateLimited,
            format!("AniList rate-limited (HTTP 429): {}", detail),
        ),
        404 => (
            AniListFailureKind::NotFound,
            format!("AniList not found (HTTP 404): {}", detail),
        ),
        // 403 lands here when the body wasn't Cloudflare-shaped — that's
        // AL-side (auth / blocked at the app layer), not upstream
        // throttling, so it's "unavailable" and callers may MAL-fallback.
        // 5xx is the same family.
        _ => (
            AniListFailureKind::Unavailable,
            format!("AniList unavailable (HTTP {}): {}", status, detail),
        ),
    }
}

/// Truncate a body string for inclusion in an error message. Char-aware
/// so it can't slice in the middle of a multi-byte UTF-8 sequence.
pub(super) fn excerpt(text: &str) -> String {
    const MAX_CHARS: usize = 200;
    let trimmed = text.trim();
    let mut iter = trimmed.chars();
    let prefix: String = iter.by_ref().take(MAX_CHARS).collect();
    if iter.next().is_some() {
        format!("{}…", prefix)
    } else {
        prefix
    }
}

/// Pure cooldown-duration computation. Extracted from `set_anilist_cooldown`
/// for unit-testability — the wall-clock side effect lives in the caller.
fn compute_cooldown_duration(retry_after_secs: Option<u64>, default_dur: Duration) -> Duration {
    let base = retry_after_secs
        .map(Duration::from_secs)
        .unwrap_or(default_dur)
        .min(ANILIST_COOLDOWN_MAX);
    base + COOLDOWN_SAFETY_MARGIN
}

pub(super) fn set_anilist_cooldown(retry_after_secs: Option<u64>, default_dur: Duration) {
    let dur = compute_cooldown_duration(retry_after_secs, default_dur);
    set_cooldown_until_now_plus(dur);
}

/// Apply the AL rate-limit policy to a response from any AL endpoint
/// reachable outside the `services::anilist` module. Records the
/// `X-RateLimit-*` headroom counters on every response (so the next
/// in-module AL call sees a fresh-from-AL view of the window), and
/// flips the process-wide cooldown on 429 / 5xx so subsequent AL
/// calls (metadata sync, library page render, scoring path) back
/// off.
///
/// Exists so the link-flow viewer fetch in `handlers::oauth` can
/// fully participate in the same throttle state as the in-module
/// callers without exposing the internal `(super)` primitives.
/// Mirrors the per-response `record_rate_limit_headers` call plus
/// the 429/5xx branch in `fetch_media_list_collection`.
pub fn note_external_anilist_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) {
    record_rate_limit_headers(headers);
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        let dur = cooldown_from_headers(headers, ANILIST_COOLDOWN_DEFAULT);
        set_cooldown_until_now_plus(dur);
    }
}

/// Set the cooldown-until marker to `now + dur`. Used by `mod.rs` call sites
/// that have already computed a duration via `cooldown_from_headers` — they
/// don't need the `compute_cooldown_duration` step.
///
/// Also clears `RATE_LIMIT_STATE` so the post-cooldown first request falls
/// to the conservative `min_inter_request(ANILIST_LIMIT_FALLBACK)` spacing
/// instead of acting on a stale `remaining=0, reset_at=…` snapshot from
/// the response that triggered this cooldown. Without the reset, the
/// first call after cooldown either takes the window-flip path against a
/// `reset_at` that's already in the past (no-op, falls through to burst
/// guard) or — if the recorded `reset_at` was very close to the cooldown
/// expiry — fires right at the boundary and trips a fresh 429. Clearing
/// state forces a probe-style first request, and the fresh response's
/// headers re-populate the snapshot for subsequent calls.
pub(super) fn set_cooldown_until_now_plus(dur: Duration) {
    if let Ok(mut g) = RATE_LIMIT_STATE.lock() {
        *g = None;
    }
    if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
        *guard = Some(Instant::now() + dur);
    }
}

/// Test-only: clear the global cooldown + rate-limit-headroom state.
/// Both are process-wide LazyLocks, so a stray cooldown left over
/// from one test (e.g. a wiremock that returned 429) would block the
/// next test's request. Tests that exercise the AL HTTP path should
/// call this at entry and exit.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_state_for_tests() {
    if let Ok(mut g) = ANILIST_COOLDOWN_UNTIL.lock() {
        *g = None;
    }
    if let Ok(mut g) = RATE_LIMIT_STATE.lock() {
        *g = None;
    }
}

/// Extract the first GraphQL-level error message from an AniList response
/// body. Shared with `mod.rs` callers that need to surface the GraphQL
/// message alongside the HTTP status.
pub(super) fn extract_graphql_error(body: &serde_json::Value) -> Option<String> {
    body["errors"]
        .as_array()
        .and_then(|errs| errs.first())
        .and_then(|err| err["message"].as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absurd_reset_header_does_not_panic() {
        // Writes the process-global snapshot, so it holds the lock and
        // clears the snapshot again before asserting: a reset an hour
        // out left behind made the next throttle sleep that hour. The
        // remaining count sits above the headroom threshold so even the
        // instant it is recorded no other test's throttle waits on it.
        let _g = COOLDOWN_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_state_for_tests();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-limit", "90".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "60".parse().unwrap());
        headers.insert("x-ratelimit-reset", u64::MAX.to_string().parse().unwrap());
        record_rate_limit_headers(&headers);
        let reset = RATE_LIMIT_STATE.lock().unwrap().and_then(|s| s.reset_at);
        reset_state_for_tests();
        let reset = reset.expect("a clamped reset is still recorded");
        assert!(reset <= Instant::now() + Duration::from_secs(3601));
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let (kind, msg) = classify_anilist_failure(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"errors":[{"message":"Too Many Requests"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::RateLimited);
        assert!(is_rate_limit_error(&msg), "tag missing: {}", msg);
    }

    #[test]
    fn classify_5xx_is_unavailable_not_throttle() {
        let (kind, msg) = classify_anilist_failure(
            reqwest::StatusCode::BAD_GATEWAY,
            "<html><body>502 Bad Gateway</body></html>",
        );
        assert_eq!(kind, AniListFailureKind::Unavailable);
        assert!(
            !is_rate_limit_error(&msg),
            "5xx must not match throttle: {}",
            msg
        );
    }

    #[test]
    fn classify_403_with_cloudflare_body_is_rate_limited() {
        let body = r#"<html><head><title>Just a moment...</title></head>
                      <body data-translate="checking_browser">
                      cf-ray: abc123 cloudflare</body></html>"#;
        let (kind, msg) = classify_anilist_failure(reqwest::StatusCode::FORBIDDEN, body);
        assert_eq!(kind, AniListFailureKind::RateLimited);
        assert!(is_rate_limit_error(&msg), "tag missing: {}", msg);
    }

    #[test]
    fn classify_403_without_cloudflare_body_is_unavailable() {
        let (kind, msg) = classify_anilist_failure(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"errors":[{"message":"Forbidden"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::Unavailable);
        assert!(
            !is_rate_limit_error(&msg),
            "non-CF 403 must not match throttle: {}",
            msg
        );
    }

    /// Regression for the 2026-04-28 AniList outage where AL returned a
    /// 403 with a JSON body announcing the API was "temporarily disabled
    /// due to severe stability issues." The body has no Cloudflare
    /// markers and the GraphQL message contains no rate-limit keywords,
    /// so the classifier must route this to `Unavailable` (caller may
    /// MAL-fallback) rather than `RateLimited` (caller defers, no
    /// fallback). Misclassifying this as RateLimited would silently
    /// prevent the metadata sweep from substituting MAL data and
    /// leave every series stranded on its previously-cached row for
    /// the duration of the outage.
    #[test]
    fn classify_403_disabled_outage_message_is_unavailable() {
        let body = r#"{"errors":[{"message":"The AniList API has been temporarily disabled due to severe stability issues. Please check the announcements channel in the official AniList Discord for more information.","status":403,"locations":[{"line":1,"column":1}]}],"data":null}"#;
        let (kind, msg) = classify_anilist_failure(reqwest::StatusCode::FORBIDDEN, body);
        assert_eq!(kind, AniListFailureKind::Unavailable);
        assert!(
            !is_rate_limit_error(&msg),
            "AL outage must not be classified as throttle: {}",
            msg
        );
    }

    #[test]
    fn classify_404_is_not_found() {
        let (kind, _msg) = classify_anilist_failure(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"errors":[{"message":"Not Found"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::NotFound);
    }

    #[test]
    fn classify_graphql_throttle_message_overrides_status() {
        let (kind, _msg) = classify_anilist_failure(
            reqwest::StatusCode::OK,
            r#"{"errors":[{"message":"Too Many Requests"}]}"#,
        );
        assert_eq!(kind, AniListFailureKind::RateLimited);
    }

    #[test]
    fn is_rate_limit_error_matches_cooldown_string() {
        assert!(is_rate_limit_error(
            "AniList rate-limit cooldown active; skipping AniList request"
        ));
    }

    #[test]
    fn cooldown_pads_retry_after_with_safety_margin() {
        let dur = compute_cooldown_duration(Some(60), ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, Duration::from_secs(60) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_falls_back_to_default_when_no_retry_after() {
        let dur = compute_cooldown_duration(None, Duration::from_secs(45));
        assert_eq!(dur, Duration::from_secs(45) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_caps_retry_after_at_max_then_pads() {
        let dur = compute_cooldown_duration(Some(3600), ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, ANILIST_COOLDOWN_MAX + COOLDOWN_SAFETY_MARGIN);
    }

    /// `set_cooldown_until_now_plus` must clear `RATE_LIMIT_STATE` so
    /// the post-cooldown first request falls to defensive spacing
    /// instead of acting on the stale `remaining=0, reset_at=…`
    /// snapshot from the 429 that triggered this cooldown. Pinned
    /// because the visible failure mode is silent — `decide_wait`
    /// just takes the wrong branch and a sweep eats more 429s.
    ///
    /// Touches process-global state, so this test serializes itself
    /// behind `COOLDOWN_TEST_LOCK` to avoid cross-test pollution. Every
    /// test here that touches these globals takes the same lock; the
    /// rest are pure (compute_* / decide_wait).
    #[test]
    fn set_cooldown_clears_rate_limit_state() {
        let _g = COOLDOWN_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Seed state to a known non-None value.
        if let Ok(mut g) = RATE_LIMIT_STATE.lock() {
            *g = Some(RateLimitState {
                limit: 30,
                remaining: 0,
                reset_at: Some(Instant::now() + Duration::from_secs(10)),
            });
        }
        set_cooldown_until_now_plus(Duration::from_millis(1));
        let snap = RATE_LIMIT_STATE.lock().ok().and_then(|g| *g);
        assert!(
            snap.is_none(),
            "RATE_LIMIT_STATE must be cleared after cooldown set; got {:?}",
            snap.map(|s| (s.limit, s.remaining))
        );
        // Belt-and-suspenders: clean up the cooldown-until marker too,
        // since later tests may run while this one's brief cooldown is
        // still notionally "active." The Duration::from_millis(1) above
        // expires before any test could observe it, but explicit reset
        // makes that not depend on test ordering.
        if let Ok(mut g) = ANILIST_COOLDOWN_UNTIL.lock() {
            *g = None;
        }
    }

    static COOLDOWN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn state(limit: u32, remaining: u32, reset_at: Option<Instant>) -> RateLimitState {
        RateLimitState {
            limit,
            remaining,
            reset_at,
        }
    }

    #[test]
    fn concurrent_callers_are_spaced_not_fired_together() {
        // A request went out 1 s ago, so both callers have to wait. They
        // used to read that same stamp, sleep the same ~1 s and fire
        // together. With no rate-limit headers the spacing is 60 / 30 = 2 s.
        //
        // The throttle reads the process-global snapshot, so this runs
        // under the lock from a cleared one (a leftover `remaining = 0`
        // with a reset an hour out made it sleep the hour). A plain test
        // driving its own runtime, so the lock is never held across an
        // `.await`.
        let _g = COOLDOWN_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_state_for_tests();
        *LAST_AL_REQUEST.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let start = Instant::now();
        let (a, b) = runtime.block_on(async {
            tokio::join!(
                async {
                    throttle_before_anilist_request().await;
                    start.elapsed()
                },
                async {
                    throttle_before_anilist_request().await;
                    start.elapsed()
                },
            )
        });
        *LAST_AL_REQUEST.lock().unwrap() = None;
        let gap = a.max(b) - a.min(b);
        assert!(
            gap >= Duration::from_millis(1900),
            "second call started {gap:?} after the first"
        );
    }

    #[test]
    fn decide_wait_no_state_no_last_request_is_zero() {
        let now = Instant::now();
        assert_eq!(decide_wait(None, None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_burst_guard_applies_when_recent_request() {
        let now = Instant::now();
        let last = now - Duration::from_millis(500);
        let s = state(30, 25, None);
        let w = decide_wait(Some(s), Some(last), now);
        assert!(w >= Duration::from_millis(1500), "got {:?}", w);
        assert!(w <= Duration::from_millis(2000), "got {:?}", w);
    }

    #[test]
    fn decide_wait_burst_guard_zero_when_enough_elapsed() {
        let now = Instant::now();
        let last = now - Duration::from_secs(10);
        let s = state(30, 25, None);
        assert_eq!(decide_wait(Some(s), Some(last), now), Duration::ZERO);
    }

    /// Pin Fix C — the live-observed AL header lie where `Limit: 90`
    /// is reported while AL is actually enforcing the documented 30
    /// req/min degraded cap. Decide_wait must clamp the header value
    /// down to ANILIST_LIMIT_FALLBACK (30) so spacing stays at
    /// `min_inter_request(30) ≈ 2200ms` instead of paced-against-90's
    /// 733ms. Without the clamp, a 27-series sweep over-fires 3x and
    /// eats 429s every 30-60s.
    ///
    /// Setup: state with limit=90 (the lie), remaining high (no
    /// window-flip), no last request — so the function reduces to
    /// the limit→min_spacing path. The wait should be the
    /// degraded-equivalent value.
    #[test]
    fn decide_wait_clamps_limit_to_fallback_when_header_reports_higher() {
        let now = Instant::now();
        let s = state(90, 50, None);
        // No last request → burst_wait would be 0 even if we paced
        // against 90; need a recent last_request to expose the
        // spacing decision.
        let last = now - Duration::from_millis(0);
        let w = decide_wait(Some(s), Some(last), now);
        // At limit=30 (clamped), min_inter_request = 2200ms. Burst
        // wait with 0 elapsed = 2200ms. If the clamp didn't fire we
        // would have paced against 90 → 733ms.
        assert!(
            w >= Duration::from_millis(2000),
            "expected ~2200ms (limit clamped to 30) but got {:?} — clamp may have regressed back to header-trusts",
            w
        );
        assert!(
            w <= Duration::from_millis(2500),
            "expected ~2200ms (limit clamped to 30) but got {:?}",
            w
        );
    }

    #[test]
    fn decide_wait_window_flip_fires_when_remaining_low_and_reset_in_future() {
        let now = Instant::now();
        let s = state(30, 2, Some(now + Duration::from_secs(30)));
        let w = decide_wait(Some(s), None, now);
        assert!(w >= Duration::from_secs(30), "got {:?}", w);
        assert!(w <= Duration::from_secs(32), "got {:?}", w);
    }

    /// Pin the exact safety margin so a future "shorten the wait"
    /// refactor can't silently regress to the +1s value that was
    /// observed live to land at the AL window boundary and trip a
    /// fresh 429. If you change the margin, change `COOLDOWN_SAFETY_MARGIN`
    /// and update this assertion.
    #[test]
    fn decide_wait_window_flip_uses_cooldown_safety_margin() {
        let now = Instant::now();
        let s = state(30, 0, Some(now + Duration::from_secs(30)));
        let w = decide_wait(Some(s), None, now);
        assert_eq!(w, Duration::from_secs(30) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn decide_wait_stale_reset_falls_through_to_burst_guard() {
        let now = Instant::now();
        let s = state(30, 0, Some(now - Duration::from_secs(5)));
        assert_eq!(decide_wait(Some(s), None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_no_reset_at_with_low_remaining_falls_to_burst_guard() {
        let now = Instant::now();
        let s = state(30, 0, None);
        assert_eq!(decide_wait(Some(s), None, now), Duration::ZERO);
    }

    #[test]
    fn decide_wait_window_flip_dominates_over_burst_guard() {
        let now = Instant::now();
        let last = now - Duration::from_millis(100);
        let s = state(30, 1, Some(now + Duration::from_secs(30)));
        let w = decide_wait(Some(s), Some(last), now);
        assert!(w >= Duration::from_secs(30), "got {:?}", w);
    }

    #[test]
    fn min_inter_request_scales_with_limit() {
        let degraded = min_inter_request(30);
        let normal = min_inter_request(90);
        assert!(degraded >= Duration::from_millis(2000));
        assert!(degraded <= Duration::from_millis(2500));
        assert!(normal >= Duration::from_millis(700));
        assert!(normal <= Duration::from_millis(800));
        assert_eq!(min_inter_request(0), Duration::from_secs(2));
    }

    #[test]
    fn cooldown_from_headers_prefers_x_ratelimit_reset() {
        use reqwest::header::HeaderMap;
        let mut h = HeaderMap::new();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        h.insert(
            "x-ratelimit-reset",
            (now_unix + 30).to_string().parse().unwrap(),
        );
        h.insert("retry-after", "999".parse().unwrap());
        let dur = cooldown_from_headers(&h, ANILIST_COOLDOWN_DEFAULT);
        let lower = Duration::from_secs(30) + COOLDOWN_SAFETY_MARGIN - Duration::from_secs(2);
        let upper = Duration::from_secs(30) + COOLDOWN_SAFETY_MARGIN + Duration::from_secs(2);
        assert!(dur >= lower && dur <= upper, "expected ~32s, got {:?}", dur);
    }

    #[test]
    fn cooldown_from_headers_falls_back_to_retry_after() {
        use reqwest::header::HeaderMap;
        let mut h = HeaderMap::new();
        h.insert("retry-after", "45".parse().unwrap());
        let dur = cooldown_from_headers(&h, ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, Duration::from_secs(45) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_from_headers_falls_back_to_default_when_no_headers() {
        use reqwest::header::HeaderMap;
        let h = HeaderMap::new();
        let dur = cooldown_from_headers(&h, Duration::from_secs(60));
        assert_eq!(dur, Duration::from_secs(60) + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn cooldown_from_headers_caps_runaway_reset_at_max() {
        use reqwest::header::HeaderMap;
        let mut h = HeaderMap::new();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        h.insert(
            "x-ratelimit-reset",
            (now_unix + 3600).to_string().parse().unwrap(),
        );
        let dur = cooldown_from_headers(&h, ANILIST_COOLDOWN_DEFAULT);
        assert_eq!(dur, ANILIST_COOLDOWN_MAX + COOLDOWN_SAFETY_MARGIN);
    }

    #[test]
    fn excerpt_is_char_boundary_safe() {
        let s: String = std::iter::repeat_n('日', 300).collect();
        let out = excerpt(&s);
        assert!(out.ends_with('…'));
    }
}
