//! Browser-e2e for the SSE-driven progress toast (`ryokanProgressToast`
//! in `static/js/base.js`). Locks the JS-side wiring shipped in
//! `746dd5c` against the SSE endpoint at
//! `/api/progress/{job_id}/stream` shipped in the same commit.
//!
//! Per `feedback_migration_discipline`: SSE migrations need a browser-
//! e2e because handler-level tests can't observe the JS-side
//! `EventSource` / message-listener / toast.update / toast.finalize
//! wiring. A handler test would catch a wire-shape regression but not
//! a JS regression that, e.g., never opens the EventSource, ignores
//! the `progress` event name, or fails to flip `lastTerminal` on the
//! terminal event.
//!
//! The test:
//!   1. Pre-seeds 3 events into the registry via the fixture
//!      `__test/progress-emit` endpoint (info → info → success terminal).
//!   2. Opens the fixture page; `ryokanProgressToast` runs at
//!      DOMContentLoaded and connects to the SSE stream.
//!   3. Waits for the toast title to flip to the terminal event's
//!      title ("All done"). That confirms: EventSource opened, all 3
//!      events arrived, terminal triggered finalize, the toast's
//!      content reflects the final event's title/body.
//!
//! Skips gracefully when WebDriver/geckodriver is unreachable; same
//! shape as the other browser-e2e files.

use fantoccini::Locator;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    fixture_errors, open_with_session, seed_user_session, spawn_app, try_connect_browser,
};

/// SSE happy path: 3 events buffered → toast walks through all three
/// → terminal triggers finalize. Pinning the *visible* terminal title
/// rather than the intermediate ones because intermediate events can
/// race the EventSource handshake (they may arrive in one batch or in
/// sequence depending on the browser's HTTP/1.1 chunk buffering); the
/// terminal title is the load-bearing assertion that tells us the
/// toast actually finished updating.
#[tokio::test]
async fn progress_toast_streams_buffered_events_and_finalizes_on_terminal() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    // Mint a unique progress id per test run so concurrent tests can't
    // collide on the registry.
    let progress_id = format!(
        "p_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    // Pre-seed events into the registry. This pre-emits the entire
    // 3-event sequence (incl. terminal) so by the time the EventSource
    // handshake completes, the buffer is full and the stream drains
    // synchronously then closes — eliminates timing flakiness vs an
    // emit-as-you-go shape.
    let pre_seed_url = format!(
        "http://{addr}/__test/progress-emit?progress_id={}",
        progress_id
    );
    let resp = reqwest::Client::new()
        .post(&pre_seed_url)
        .header("Cookie", format!("session={}", session))
        // POSTs without Origin / Referer are rejected by the same-origin CSRF check.
        .header("Origin", format!("http://{addr}"))
        .send()
        .await
        .expect("pre-seed events");
    assert_eq!(resp.status(), 200, "fixture pre-seed must succeed");

    // Open the fixture page with the progress_id baked in. The page's
    // inline script calls `ryokanProgressToast` at DOMContentLoaded,
    // which opens an EventSource against
    // `/api/progress/{progress_id}/stream`.
    let path = format!("/__test/progress-toast-fixture?progress_id={}", progress_id);
    open_with_session(&client, addr, &session, &path)
        .await
        .expect("open progress fixture");

    // The toast lives in `#ryokan-toast-stack` (created by base.js's
    // ryokanToast helper). Wait for the toast title to reach the
    // terminal event's title — proves all 3 events landed and the
    // terminal triggered toast.finalize().
    let stack = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("#ryokan-toast-stack"))
        .await
        .expect("toast stack present");

    let mut saw_terminal = false;
    for _ in 0..40 {
        let text = stack.text().await.unwrap_or_default();
        if text.contains("All done") {
            saw_terminal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let errors = fixture_errors(&client).await;
    assert!(
        saw_terminal,
        "SSE toast must surface the terminal event title; toast stack never showed 'All done' — indicates EventSource didn't open, message listener didn't fire, or finalize() didn't run; fixture script errors: {errors:?}"
    );

    let _ = client.close().await;
}

/// Boost-revisit safety: open the toast, navigate away, come back —
/// no orphan EventSource still firing, no listener accumulation.
/// Catches the same regression class the lifecycle-helper dedup fix
/// (`c91bb0f`) addressed for other modules — if the EventSource
/// stays open after the page is gone or fires events into a stale
/// closure, the symptom would be a second toast spawning on the
/// new page.
///
/// Skipped under boost specifically (which the fixture page doesn't
/// have anyway — no `<body hx-boost>`) so this asserts the simpler
/// "old toast stays gone after a fresh page load" property.
#[tokio::test]
async fn progress_toast_does_not_orphan_event_source_across_navigations() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    let progress_id = format!(
        "p_orphan_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    // Pre-seed terminal so the first visit's toast finalizes promptly.
    let resp = reqwest::Client::new()
        .post(format!(
            "http://{addr}/__test/progress-emit?progress_id={}",
            progress_id
        ))
        .header("Cookie", format!("session={}", session))
        // POSTs without Origin / Referer are rejected by the same-origin CSRF check.
        .header("Origin", format!("http://{addr}"))
        .send()
        .await
        .expect("pre-seed");
    assert_eq!(resp.status(), 200);

    // First visit: open fixture → toast fires → finalizes on terminal.
    open_with_session(
        &client,
        addr,
        &session,
        &format!("/__test/progress-toast-fixture?progress_id={}", progress_id),
    )
    .await
    .expect("first visit");
    let stack = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("#ryokan-toast-stack"))
        .await
        .expect("first visit toast stack");
    for _ in 0..30 {
        if stack.text().await.unwrap_or_default().contains("All done") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Second visit (different progress_id, no pre-seed): the OLD
    // toast's EventSource should already be closed (terminal flipped
    // `stopped = true` in the toast closure). Loading a fresh page
    // reuses the same EventSource constructor with a new id. If the
    // old EventSource leaked, we'd see two toasts in the stack on the
    // new page (only the new id's toast should show because the new
    // page has no pre-seeded events, so it stays at "Initializing").
    let new_id = format!("p_orphan_b_{}", progress_id);
    open_with_session(
        &client,
        addr,
        &session,
        &format!("/__test/progress-toast-fixture?progress_id={}", new_id),
    )
    .await
    .expect("second visit");

    let new_stack = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("#ryokan-toast-stack"))
        .await
        .expect("second visit toast stack");
    let text = new_stack.text().await.unwrap_or_default();
    // The new page should show ONE toast — its initial state. If the
    // OLD toast leaked, "All done" would still be visible.
    assert!(
        !text.contains("All done"),
        "second visit must not retain the prior visit's terminal toast text — indicates orphan EventSource or stale toast DOM; got: {text:?}"
    );

    let _ = client.close().await;
}
