//! Browser-e2e for the per-episode delete HX-Trigger listener
//! shipped in PR `ac19049` (Phase 2 migration).
//!
//! Per `feedback_migration_discipline`: HTMX migrations need a
//! browser-e2e because handler-level tests can't observe the JS-side
//! event-listener wiring. The fixture isolates the most failure-prone
//! link in the per-episode delete flow under hx-boost — the
//! `ryokan-episode-deleted` listener — and verifies it actually fires
//! `updateEpisodeRow` to flip the row class.
//!
//! Failure modes this catches:
//!   - Singleton guard regression (`window.__ryokanSeriesListeners`
//!     stops gating, listener never re-attaches after boost re-execute).
//!   - HX-Trigger payload shape change (event name / detail keys that
//!     the listener doesn't recognize).
//!   - `updateEpisodeRow` being renamed / removed / stripped of its
//!     `state === 'deleted'` branch.
//!
//! The full delete *flow* (modal click → confirm bridge → POST →
//! HX-Trigger header → swap) has handler-level coverage in
//! `src/handlers/library/episodes.rs::tests::episodes_ci`. This fixture
//! covers the JS-only middle leg that handler tests can't reach.
//!
//! Skips gracefully when WebDriver/geckodriver is unreachable.

use fantoccini::Locator;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{open_with_session, seed_user_session, spawn_app, try_connect_browser};

/// Dispatching the `ryokan-episode-deleted` CustomEvent must trigger
/// `updateEpisodeRow` to flip the row's class. This is the contract
/// the singleton-guarded listener in `series.js` is supposed to keep
/// wired across hx-boost re-executions.
#[tokio::test]
async fn ryokan_episode_deleted_event_flips_row_to_missing_state() {
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

    open_with_session(
        &client,
        addr,
        &session,
        "/__test/episode-delete-listener-fixture",
    )
    .await
    .expect("open fixture");

    // Confirm the precondition: the row exists with `ep-row-have`
    // class. If this assertion fails, the fixture page wasn't loaded
    // correctly and the post-event assertion would be ambiguous.
    let row = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("tr[data-test-id='row-5']"))
        .await
        .expect("row present");
    let pre_class = row
        .attr("class")
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    assert!(
        pre_class.contains("ep-row-have"),
        "precondition: row must start in ep-row-have state; got class='{pre_class}'"
    );

    // Wait for series.js to load + register its listener. The script
    // is `<script src=...>` (synchronous) so by the time the row is
    // queryable, the listener is attached. Belt-and-suspenders: poll
    // briefly until `window.__ryokanSeriesListeners` is set.
    let mut listener_ready = false;
    for _ in 0..30 {
        let v: serde_json::Value = client
            .execute("return !!window.__ryokanSeriesListeners;", vec![])
            .await
            .unwrap_or(serde_json::Value::Bool(false));
        if v.as_bool().unwrap_or(false) {
            listener_ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        listener_ready,
        "series.js must register the singleton-guarded HX-Trigger listener; \
         `window.__ryokanSeriesListeners` never became truthy. Likely a series.js \
         load failure or the singleton-guard refactor lost its set-on-first-run."
    );

    // Dispatch the synthetic event. The handler emits this exact
    // shape from `episode_delete_trigger` in
    // `src/handlers/library/episodes.rs`; the listener parses
    // `detail.episode_number` to identify the row.
    let _ = client
        .execute(
            r#"
            const ev = new CustomEvent('ryokan-episode-deleted', {
                detail: {
                    ok: true,
                    episode_number: 5,
                    message: 'Episode 5 file removed.',
                }
            });
            document.body.dispatchEvent(ev);
            return true;
            "#,
            vec![],
        )
        .await
        .expect("dispatch event");

    // Poll for the class flip. The listener calls
    // `updateEpisodeRow(5, 'deleted')` synchronously; the only async
    // tail is `refreshEpisodeRows({force: true})`, which fails
    // silently against the fixture's stub `series-data` element
    // (no `dbId`) — that's intentional, it doesn't affect the
    // synchronous classList mutation.
    let mut saw_missing = false;
    for _ in 0..20 {
        let post_class = row
            .attr("class")
            .await
            .unwrap_or_default()
            .unwrap_or_default();
        if post_class.contains("ep-row-missing") && !post_class.contains("ep-row-have") {
            saw_missing = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        saw_missing,
        "row must flip from `ep-row-have` to `ep-row-missing` after the HX-Trigger event; \
         indicates the listener didn't fire OR `updateEpisodeRow` lost its 'deleted' branch"
    );

    let _ = client.close().await;
}
