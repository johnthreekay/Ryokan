//! Throttle tier + failure-bookkeeping coverage for `login_check`,
//! `login_record_failure`, `login_clear`, and `sweep_login_failures`.
//!
//! `LOGIN_FAILURES` is a process-global `Mutex<HashMap<String, _>>`.
//! Parallel tests can safely share it as long as each test uses its
//! own unique key namespace for the MUTATIONS — the keys are
//! strings, buckets are created on demand, and per-key operations
//! don't affect other keys.
//!
//! The one wrinkle: `sweep_login_failures()` iterates EVERY bucket
//! in the map and prunes stale entries regardless of key. So any
//! test that calls `sweep_login_failures()` races against other
//! concurrent tests that happen to hold stale entries in their own
//! buckets mid-setup. Observed in CI (2026-04-24) on the dev branch
//! where `sweep_login_failures_drops_buckets_that_fully_expire`
//! running in parallel swept the partial-buckets test's stale entry
//! between its two `seed_login_failure_for_test` calls, making its
//! post-seed count assertion fail with 1 instead of 2.
//!
//! Fix: `SWEEP_TEST_LOCK`. Every test that either calls sweep OR
//! seeds a stale-time-stamped entry acquires this lock for its
//! duration. Tests that only touch fresh entries / their own
//! record-count paths still run in parallel.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::handlers::auth::{
    LOGIN_HARD_CAP, LOGIN_MAX_FAILURES, LOGIN_WINDOW, LoginCheck, login_check, login_clear,
    login_failure_count_for_test, login_record_failure, seed_login_failure_for_test,
    sweep_login_failures,
};

/// Serializes tests that touch sweep-visible state (stale entries or
/// `sweep_login_failures()` calls). See module docstring for the CI
/// race that motivated this. `unwrap_or_else(|p| p.into_inner())`
/// recovers from a prior test's panic so one failure doesn't poison
/// the lock for every subsequent run.
static SWEEP_TEST_LOCK: Mutex<()> = Mutex::new(());

// ─── login_check tier thresholds ───────────────────────────────────

#[test]
fn login_check_returns_allow_when_no_failures_recorded() {
    let key = "test:login_check_returns_allow_when_no_failures_recorded";
    assert_eq!(login_check(key), LoginCheck::Allow);
}

#[test]
fn login_check_returns_allow_just_under_soft_cap() {
    let key = "test:login_check_returns_allow_just_under_soft_cap";
    for _ in 0..(LOGIN_MAX_FAILURES - 1) {
        login_record_failure(key);
    }
    assert_eq!(login_check(key), LoginCheck::Allow);
}

#[test]
fn login_check_flips_to_soft_throttle_at_the_soft_cap() {
    let key = "test:login_check_flips_to_soft_throttle_at_the_soft_cap";
    for _ in 0..LOGIN_MAX_FAILURES {
        login_record_failure(key);
    }
    assert_eq!(login_check(key), LoginCheck::SoftThrottled);
}

#[test]
fn login_check_stays_soft_throttled_just_under_hard_cap() {
    let key = "test:login_check_stays_soft_throttled_just_under_hard_cap";
    for _ in 0..(LOGIN_HARD_CAP - 1) {
        login_record_failure(key);
    }
    assert_eq!(login_check(key), LoginCheck::SoftThrottled);
}

#[test]
fn login_check_flips_to_hard_throttle_at_the_hard_cap() {
    let key = "test:login_check_flips_to_hard_throttle_at_the_hard_cap";
    for _ in 0..LOGIN_HARD_CAP {
        login_record_failure(key);
    }
    assert_eq!(login_check(key), LoginCheck::HardThrottled);
}

// ─── Bookkeeping side effects ──────────────────────────────────────

#[test]
fn login_record_failure_increments_the_recorded_count() {
    let key = "test:login_record_failure_increments_the_recorded_count";
    assert_eq!(login_failure_count_for_test(key), 0);
    login_record_failure(key);
    assert_eq!(login_failure_count_for_test(key), 1);
    login_record_failure(key);
    assert_eq!(login_failure_count_for_test(key), 2);
}

#[test]
fn login_clear_removes_all_recorded_failures_for_the_key() {
    let key = "test:login_clear_removes_all_recorded_failures_for_the_key";
    for _ in 0..3 {
        login_record_failure(key);
    }
    assert_eq!(login_failure_count_for_test(key), 3);
    login_clear(key);
    assert_eq!(login_failure_count_for_test(key), 0);
}

#[test]
fn login_clear_on_unseen_key_is_a_noop() {
    let key = "test:login_clear_on_unseen_key_is_a_noop";
    // No record ever written; clearing should not panic or error.
    login_clear(key);
    assert_eq!(login_failure_count_for_test(key), 0);
}

// ─── Per-key isolation ─────────────────────────────────────────────

#[test]
fn recording_failures_on_one_key_does_not_affect_another() {
    let key_a = "test:recording_failures_on_one_key_does_not_affect_another:A";
    let key_b = "test:recording_failures_on_one_key_does_not_affect_another:B";
    for _ in 0..LOGIN_MAX_FAILURES {
        login_record_failure(key_a);
    }
    assert_eq!(login_check(key_a), LoginCheck::SoftThrottled);
    // B never recorded anything → still Allow.
    assert_eq!(login_check(key_b), LoginCheck::Allow);
}

#[test]
fn clearing_one_key_does_not_clear_another() {
    let key_a = "test:clearing_one_key_does_not_clear_another:A";
    let key_b = "test:clearing_one_key_does_not_clear_another:B";
    login_record_failure(key_a);
    login_record_failure(key_b);
    login_record_failure(key_b);
    login_clear(key_a);
    assert_eq!(login_failure_count_for_test(key_a), 0);
    assert_eq!(login_failure_count_for_test(key_b), 2);
}

// ─── Sliding-window expiration ─────────────────────────────────────

#[test]
fn login_check_prunes_entries_older_than_the_window() {
    let _guard = SWEEP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let key = "test:login_check_prunes_entries_older_than_the_window";
    // Seed a failure at "60s + 1ms ago" — past the cutoff.
    let stale = Instant::now() - LOGIN_WINDOW - Duration::from_millis(1);
    seed_login_failure_for_test(key, stale);
    assert_eq!(login_failure_count_for_test(key), 1);
    // login_check's per-key sweep should drop the stale entry.
    assert_eq!(login_check(key), LoginCheck::Allow);
    assert_eq!(
        login_failure_count_for_test(key),
        0,
        "stale entry should have been pruned by login_check"
    );
}

#[test]
fn login_check_keeps_entries_inside_the_window() {
    let key = "test:login_check_keeps_entries_inside_the_window";
    // Seed at "30s ago" — well inside the 60s window.
    let fresh = Instant::now() - Duration::from_secs(30);
    seed_login_failure_for_test(key, fresh);
    assert_eq!(login_check(key), LoginCheck::Allow);
    assert_eq!(
        login_failure_count_for_test(key),
        1,
        "fresh entry should NOT have been pruned"
    );
}

#[test]
fn sweep_login_failures_drops_buckets_that_fully_expire() {
    let _guard = SWEEP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let key = "test:sweep_login_failures_drops_buckets_that_fully_expire";
    let stale = Instant::now() - LOGIN_WINDOW - Duration::from_millis(1);
    seed_login_failure_for_test(key, stale);
    seed_login_failure_for_test(key, stale);
    assert_eq!(login_failure_count_for_test(key), 2);
    sweep_login_failures();
    assert_eq!(
        login_failure_count_for_test(key),
        0,
        "sweep should prune the stale entries AND drop the empty bucket"
    );
}

#[test]
fn sweep_login_failures_preserves_partial_buckets() {
    let _guard = SWEEP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let key = "test:sweep_login_failures_preserves_partial_buckets";
    let stale = Instant::now() - LOGIN_WINDOW - Duration::from_millis(1);
    let fresh = Instant::now() - Duration::from_secs(10);
    seed_login_failure_for_test(key, stale);
    seed_login_failure_for_test(key, fresh);
    assert_eq!(login_failure_count_for_test(key), 2);
    sweep_login_failures();
    assert_eq!(
        login_failure_count_for_test(key),
        1,
        "sweep should prune only the stale entry, keeping the fresh one"
    );
}

// ─── Reservation, bounded keys, device cookies ────────────────────

#[test]
fn login_attempt_counts_each_attempt_as_it_starts() {
    // A burst of parallel logins used to pass the check before any
    // failure was recorded (the record came after bcrypt). Each attempt
    // now reserves its slot under the lock.
    let key = "test:reserve:burst";
    login_clear(key);
    let tiers: Vec<LoginCheck> = (0..LOGIN_MAX_FAILURES + 2)
        .map(|_| crate::handlers::auth::login_attempt(&[key.to_string()])[0])
        .collect();
    assert!(
        tiers[..LOGIN_MAX_FAILURES]
            .iter()
            .all(|t| *t == LoginCheck::Allow)
    );
    assert_eq!(tiers[LOGIN_MAX_FAILURES], LoginCheck::SoftThrottled);
    login_clear(key);
}

#[test]
fn login_attempt_stops_growing_a_bucket_at_the_hard_cap() {
    let key = "test:reserve:cap";
    login_clear(key);
    for _ in 0..LOGIN_HARD_CAP * 3 {
        crate::handlers::auth::login_attempt(&[key.to_string()]);
    }
    assert_eq!(login_failure_count_for_test(key), LOGIN_HARD_CAP);
    login_clear(key);
}

#[test]
fn a_throttled_client_cycling_usernames_adds_no_buckets() {
    // Each request with a fresh username used to create a bucket even
    // once the client's own bucket was throttled, so the map grew by a
    // bucket per request until the hourly sweep.
    let ip = "test:cycle:ip".to_string();
    login_clear(&ip);
    let users: Vec<String> = (0..LOGIN_HARD_CAP * 2)
        .map(|i| format!("test:cycle:user:{i}"))
        .collect();
    let tiers: Vec<Vec<LoginCheck>> = users
        .iter()
        .map(|user| crate::handlers::auth::login_attempt(&[user.clone(), ip.clone()]))
        .collect();
    // The first attempts get a verdict and count against both buckets.
    for user in &users[..LOGIN_MAX_FAILURES] {
        assert_eq!(login_failure_count_for_test(user), 1, "{user}");
    }
    // After that the client is throttled and nothing new is created...
    for user in &users[LOGIN_MAX_FAILURES..] {
        assert_eq!(login_failure_count_for_test(user), 0, "{user}");
    }
    // ...while its own bucket still climbs to the hard cap.
    assert_eq!(tiers[LOGIN_MAX_FAILURES][1], LoginCheck::SoftThrottled);
    assert_eq!(tiers[LOGIN_HARD_CAP][1], LoginCheck::HardThrottled);
    assert_eq!(login_failure_count_for_test(&ip), LOGIN_HARD_CAP);
    login_clear(&ip);
    for user in &users {
        login_clear(user);
    }
}

#[test]
fn a_throttled_attempt_still_counts_against_existing_buckets() {
    // A username already being guessed keeps counting when the request
    // comes from a throttled client, as it would from any other client.
    let ip = "test:existing:ip".to_string();
    let user = "test:existing:user".to_string();
    login_clear(&ip);
    login_clear(&user);
    for _ in 0..LOGIN_MAX_FAILURES {
        login_record_failure(&ip);
    }
    login_record_failure(&user);
    let tiers = crate::handlers::auth::login_attempt(&[user.clone(), ip.clone()]);
    assert_eq!(tiers, vec![LoginCheck::Allow, LoginCheck::SoftThrottled]);
    assert_eq!(login_failure_count_for_test(&user), 2);
    assert_eq!(login_failure_count_for_test(&ip), LOGIN_MAX_FAILURES + 1);
    login_clear(&ip);
    login_clear(&user);
}

#[test]
fn the_username_bucket_is_a_fixed_size_whatever_the_username() {
    let long = "a".repeat(2 << 20);
    let key = crate::handlers::auth::user_bucket_key(&long);
    assert_eq!(key.len(), 2 + 64, "u: plus a SHA-256");
    assert_eq!(
        crate::handlers::auth::user_bucket_key(" Admin "),
        crate::handlers::auth::user_bucket_key("admin"),
        "trimmed and case-folded like the lookup"
    );
}

/// One login POST. `LOGIN_FAILURES` is process-wide and plain
/// `cargo test` runs these in one process, so each test logs in as its
/// own username from its own addresses: a shared username let one
/// test's failures throttle the next.
async fn login(
    state: crate::AppState,
    username: &str,
    peer: &str,
    device: Option<&str>,
    password: &str,
) -> axum::response::Response {
    let mut headers = axum::http::HeaderMap::new();
    if let Some(token) = device {
        headers.insert(
            axum::http::header::COOKIE,
            format!("ryokan_device={token}").parse().unwrap(),
        );
    }
    crate::handlers::auth::login_submit(
        axum::extract::State(state),
        axum::extract::ConnectInfo(peer.parse().unwrap()),
        headers,
        axum::Form(crate::handlers::auth::LoginForm {
            username: username.into(),
            password: password.into(),
        }),
    )
    .await
}

#[tokio::test]
async fn someone_elses_failed_logins_do_not_lock_out_a_known_device() {
    let user = "lockout-admin";
    let db = crate::test_support::in_memory_pool().await;
    let user_id = crate::models::user::create_user(&db, user, "correct-horse-1")
        .await
        .unwrap();
    let device = crate::models::login_device::create(&db, user_id)
        .await
        .unwrap();
    let state = crate::test_support::build_test_app_state(db, None);

    // Someone else fails as the admin until the username is throttled.
    for _ in 0..=LOGIN_MAX_FAILURES {
        login(state.clone(), user, "203.0.113.7:5000", None, "wrong").await;
    }
    // A browser that never logged in is refused even with the password...
    let stranger = login(
        state.clone(),
        user,
        "198.51.100.9:5000",
        None,
        "correct-horse-1",
    )
    .await;
    assert_eq!(
        stranger.status(),
        axum::http::StatusCode::OK,
        "throttled page"
    );
    // ...the admin's own browser is not.
    let admin = login(
        state.clone(),
        user,
        "198.51.100.9:5000",
        Some(&device),
        "correct-horse-1",
    )
    .await;
    assert_eq!(admin.status(), axum::http::StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn only_a_successful_login_marks_a_device_used() {
    // The device lookup used to be an UPDATE, so every login carrying
    // the cookie (throttled ones included) cost a write.
    let user = "touch-admin";
    let db = crate::test_support::in_memory_pool().await;
    let user_id = crate::models::user::create_user(&db, user, "correct-horse-1")
        .await
        .unwrap();
    let device = crate::models::login_device::create(&db, user_id)
        .await
        .unwrap();
    sqlx::query("UPDATE login_devices SET last_used_at = strftime('%s', 'now') - 864000")
        .execute(&db)
        .await
        .unwrap();
    let last_used = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT strftime('%s', 'now') - last_used_at FROM login_devices",
        )
        .fetch_one(&db)
        .await
        .unwrap()
    };
    let state = crate::test_support::build_test_app_state(db.clone(), None);

    let failed = login(
        state.clone(),
        user,
        "198.51.100.40:5000",
        Some(&device),
        "wrong",
    )
    .await;
    assert_eq!(failed.status(), axum::http::StatusCode::OK);
    assert!(
        last_used().await >= 864000,
        "a failed attempt writes nothing"
    );

    let ok = login(
        state,
        user,
        "198.51.100.40:5000",
        Some(&device),
        "correct-horse-1",
    )
    .await;
    assert_eq!(ok.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(last_used().await < 60, "a login restarts the device's age");
}

#[tokio::test]
async fn a_throttled_username_logs_once_per_window_whatever_the_client() {
    // The damping was keyed on the client address, so a username
    // throttled by attempts from many addresses logged a row for each.
    let user = "damped-admin";
    let db = crate::test_support::in_memory_pool().await;
    crate::models::user::create_user(&db, user, "correct-horse-1")
        .await
        .unwrap();
    let state = crate::test_support::build_test_app_state(db.clone(), None);
    for i in 0..LOGIN_MAX_FAILURES + 3 {
        let peer = format!("198.51.100.{}:5000", 30 + i);
        login(state.clone(), user, &peer, None, "wrong").await;
    }
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE message LIKE 'Login rate-limited%'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn a_first_login_from_a_browser_sets_the_device_cookie() {
    let user = "first-login-admin";
    let db = crate::test_support::in_memory_pool().await;
    crate::models::user::create_user(&db, user, "correct-horse-1")
        .await
        .unwrap();
    let state = crate::test_support::build_test_app_state(db, None);
    let response = login(state, user, "198.51.100.10:5000", None, "correct-horse-1").await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let cookies: Vec<String> = response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("ryokan_device=") && c.contains("HttpOnly")),
        "{cookies:?}"
    );
}

#[tokio::test]
async fn a_stale_radarr_key_does_not_throttle_sonarr_calls() {
    // Seerr calls both shims from one address, and they shared one
    // `key:<ip>` bucket.
    use tower::ServiceExt;
    let db = crate::test_support::in_memory_pool().await;
    let cfg = crate::models::config::Config {
        sonarr_enabled: true,
        sonarr_api_key: "sonarr-key-0123456789abcdef".into(),
        radarr_enabled: true,
        radarr_api_key: "radarr-key-0123456789abcdef".into(),
        ..crate::models::config::Config::default()
    };
    crate::models::config::save_config(&db, &cfg).await.unwrap();
    let state = crate::test_support::build_test_app_state(db, None);
    let peer: std::net::SocketAddr = "198.51.100.60:5000".parse().unwrap();
    let call = |app: axum::Router, uri: &'static str, key: &'static str| async move {
        let req = axum::http::Request::builder()
            .uri(uri)
            .header("x-api-key", key)
            .extension(axum::extract::ConnectInfo(peer))
            .body(axum::body::Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    };
    let radarr = crate::test_support::radarr_router(state.clone());
    for _ in 0..LOGIN_MAX_FAILURES {
        call(
            radarr.clone(),
            "/radarr/api/v3/system/status",
            "stale-radarr-key",
        )
        .await;
    }
    assert_eq!(
        call(radarr, "/radarr/api/v3/system/status", "stale-radarr-key").await,
        axum::http::StatusCode::TOO_MANY_REQUESTS
    );
    let sonarr = crate::test_support::sonarr_router(state);
    assert_eq!(
        call(
            sonarr,
            "/api/v3/system/status",
            "sonarr-key-0123456789abcdef"
        )
        .await,
        axum::http::StatusCode::OK
    );
}
