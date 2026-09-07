use super::*;

// ── admit_log_event ──────────────────────────────────────────────
//
// The pure helper behind `check_client_log_rate`. Tested with an
// explicit clock + state so we can drive the sliding window
// without poking the process-wide `CLIENT_LOG_HITS` static.

fn t0() -> Instant {
    Instant::now()
}

#[test]
fn admit_log_event_admits_under_cap() {
    let mut hits = VecDeque::new();
    let now = t0();
    let window = Duration::from_secs(60);
    let max = 3;
    assert!(admit_log_event(&mut hits, now, window, max));
    assert!(admit_log_event(&mut hits, now, window, max));
    assert!(admit_log_event(&mut hits, now, window, max));
    // Three admitted, queue is full.
    assert_eq!(hits.len(), 3);
}

#[test]
fn admit_log_event_rejects_at_cap() {
    let mut hits = VecDeque::new();
    let now = t0();
    let window = Duration::from_secs(60);
    let max = 2;
    assert!(admit_log_event(&mut hits, now, window, max));
    assert!(admit_log_event(&mut hits, now, window, max));
    // Cap reached — third call must reject.
    assert!(!admit_log_event(&mut hits, now, window, max));
    // Queue stays at the cap; the rejected event is NOT recorded
    // (otherwise a sustained burst would push the window forward
    // forever and never let traffic in again).
    assert_eq!(hits.len(), 2);
}

#[test]
fn admit_log_event_drops_expired_timestamps_before_check() {
    let mut hits = VecDeque::new();
    let window = Duration::from_secs(60);
    let max = 2;

    let earlier = t0();
    // Seed two old hits manually.
    hits.push_back(earlier);
    hits.push_back(earlier);

    // Advance past the window — both should age out and the next
    // event admits cleanly.
    let now = earlier + Duration::from_secs(61);
    assert!(admit_log_event(&mut hits, now, window, max));
    // Only the just-admitted event remains.
    assert_eq!(hits.len(), 1);
}

#[test]
fn admit_log_event_keeps_in_window_timestamps() {
    let mut hits = VecDeque::new();
    let window = Duration::from_secs(60);
    let max = 2;
    let earlier = t0();
    hits.push_back(earlier);

    // Half a window later — the seeded hit is still in window so
    // the second event tips us up to cap; the third must reject.
    let now = earlier + Duration::from_secs(30);
    assert!(admit_log_event(&mut hits, now, window, max));
    assert!(!admit_log_event(&mut hits, now, window, max));
}

#[test]
fn admit_log_event_zero_max_rejects_everything() {
    // Defensive: a misconfigured cap of 0 must not admit any
    // events (rather than treating "0" as "no limit"). Pin the
    // ordering so a future "shortcut" optimization can't flip
    // the policy.
    let mut hits = VecDeque::new();
    assert!(!admit_log_event(
        &mut hits,
        t0(),
        Duration::from_secs(60),
        0
    ));
    assert!(hits.is_empty());
}

// ── normalize_system_tab ─────────────────────────────────────────
//
// The /system page lives behind a `?tab=` query param. The
// normalizer pins which strings are recognized; everything else
// collapses to "logs" so a stale bookmark doesn't render an empty
// page. Pinning every accepted value guards against a future
// refactor that drops a tab silently.

#[test]
fn normalize_system_tab_recognized_values_pass_through() {
    for tab in ["debug", "rss", "tasks", "review", "credits"] {
        assert_eq!(normalize_system_tab(Some(tab.to_string())), tab);
    }
}

#[test]
fn normalize_system_tab_unknown_or_missing_falls_back_to_logs() {
    assert_eq!(normalize_system_tab(None), "logs");
    assert_eq!(normalize_system_tab(Some("".to_string())), "logs");
    assert_eq!(normalize_system_tab(Some("garbage".to_string())), "logs");
}
