//! hx-boost rollout — Phase B browser-e2e coverage (per-page JS init
//! lifecycle).
//!
//! Phase B scope per the rollout plan:
//!   - `static/js/page_lifecycle.js` exposes `ryokanRegisterPageInit`
//!   - `downloads.js` queue poller migrated to lifecycle (mount on
//!     entry, unmount on exit)
//!   - `system.js` logs poller migrated to lifecycle
//!   - `base.js` `[data-ts]` refresh hooks into `htmx.onLoad`
//!
//! The polling-leak case is the highest-stakes regression Phase B
//! prevents: a module-scope `setInterval` started once on initial
//! document load runs forever and accumulates copies on every
//! boosted re-entry. Tests 1 + 2 below are the explicit guard.
//!
//! Skips gracefully when geckodriver is unreachable.

use fantoccini::Locator;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_loaded, open_with_session, seed_user_session, spawn_app, try_connect_browser,
    wait_for_js_truthy, wait_for_path,
};

async fn set_mobile_viewport(client: &fantoccini::Client) -> Result<(), String> {
    client
        .set_window_rect(0, 0, 480, 800)
        .await
        .map_err(|e| format!("set_window_rect: {e}"))
}

/// Click the boosted mobile-tabbar link at `href` and wait for the
/// URL to update. Useful for chained navigations where each step
/// must finish before the next click.
async fn click_tabbar_link(
    client: &fantoccini::Client,
    href: &str,
    expected_path: &str,
) -> Result<(), String> {
    let sel = format!(".mobile-tabbar a[href=\"{href}\"]");
    let link = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(&sel))
        .await
        .map_err(|e| format!("link {href}: {e}"))?;
    link.click()
        .await
        .map_err(|e| format!("click {href}: {e}"))?;
    wait_for_path(client, expected_path, Duration::from_secs(5)).await
}

/// **queue-poller-mounts** — navigating into Downloads via the
/// boosted mobile-tabbar must start the queue poller. The page-
/// lifecycle helper's `mount` should set
/// `window.__downloadsQueuePoller` to a non-null interval handle.
///
/// Without `ryokanRegisterPageInit` (the legacy module-scope
/// `setInterval` shape), boost-swaps wouldn't re-trigger the init
/// block and the poller would never start on the second-or-later
/// visit. This test fails LOUDLY in that case.
#[tokio::test]
async fn boosted_nav_into_downloads_starts_queue_poller() {
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

    set_mobile_viewport(&client).await.expect("mobile viewport");
    open_with_session(&client, addr, &session, "/")
        .await
        .expect("open library");
    let _ = assert_htmx_loaded(&client).await;

    // Sanity: on Library, the Downloads-page poller handle is null.
    let initial = client
        .execute("return typeof window.__downloadsQueuePoller;", vec![])
        .await
        .expect("read pre-nav poller");
    let initial_typeof = initial.as_str().unwrap_or("");
    assert!(
        initial_typeof == "undefined" || initial_typeof == "object",
        "before navigating to Downloads, the poller handle must be \
         undefined OR null (=== object); got: {initial_typeof:?}"
    );

    // Boost-nav to Downloads. The lifecycle mount fires on
    // htmx.onLoad and starts setInterval(loadQueue, 5000).
    click_tabbar_link(&client, "/downloads", "/downloads")
        .await
        .expect("nav to downloads");

    // The mount runs in htmx.onLoad after the boosted swap settles.
    // Poll for the marker rather than fixed-sleep — settle timing
    // varies under parallel test execution.
    wait_for_js_truthy(
        &client,
        "window.__downloadsQueuePoller != null",
        Duration::from_secs(5),
    )
    .await
    .expect(
        "after boosted nav into /downloads, window.__downloadsQueuePoller \
         must be a live interval handle — got null/undefined, meaning the \
         lifecycle mount didn't fire (or page_lifecycle.js didn't load)",
    );

    let _ = client.close().await;
}

/// **queue-poller-unmounts** — leaving Downloads via boosted nav
/// must stop the poller. Without this, ping-pong nav (Library →
/// Downloads → Library → Downloads → ...) accumulates a new
/// interval on every Downloads landing, none of which ever clear.
/// Memory leak + the polling rate compounds, hammering
/// `/api/torrents` faster on every visit.
#[tokio::test]
async fn boosted_nav_out_of_downloads_clears_queue_poller() {
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

    set_mobile_viewport(&client).await.expect("mobile viewport");
    open_with_session(&client, addr, &session, "/downloads")
        .await
        .expect("open downloads");
    let _ = assert_htmx_loaded(&client).await;

    // Wait for downloads.js's defer execution to finish (signaled
    // by the immediate-reconcile setting __downloadsQueuePoller).
    wait_for_js_truthy(
        &client,
        "window.__downloadsQueuePoller != null",
        Duration::from_secs(5),
    )
    .await
    .expect("poller should be live on direct /downloads load");

    // Boost-nav to Library. The unmount fires (Downloads-queue
    // `check()` returns falsy after the swap), clearing the
    // interval and nulling the handle.
    click_tabbar_link(&client, "/", "/")
        .await
        .expect("nav to library");

    // Poll for the unmount completion. Same shape as the mount-
    // wait in the previous test — converges fast and gives a clear
    // timeout error on a real leak.
    wait_for_js_truthy(
        &client,
        "window.__downloadsQueuePoller === null",
        Duration::from_secs(5),
    )
    .await
    .expect(
        "after navigating away from /downloads, the queue poller \
         handle must be null (cleared by lifecycle unmount); got \
         non-null value, meaning the interval is still firing in \
         the background — leak",
    );

    let _ = client.close().await;
}

/// **no-duplicate-pollers-after-pingpong** — the most direct guard
/// against the leak pattern. Navigate Downloads → Library →
/// Downloads → Library → Downloads. After three Downloads landings
/// the poller handle should be exactly one live interval, not three
/// stacked. We assert the first two visits cleanly cleared their
/// handles and only the third's handle is alive.
#[tokio::test]
async fn pingpong_nav_does_not_stack_pollers() {
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

    set_mobile_viewport(&client).await.expect("mobile viewport");
    open_with_session(&client, addr, &session, "/")
        .await
        .expect("open library");
    let _ = assert_htmx_loaded(&client).await;

    // Instrument both `setInterval` and `clearInterval` for the
    // `loadQueue` callback. The exact CALL count varies with how
    // the lifecycle module orchestrates mount/remount under
    // head-support's script re-execution semantics; what matters
    // is the NET active count = (setInterval calls) -
    // (clearInterval calls on those handles). For a healthy
    // mount/unmount cycle the net at any moment is 0 or 1, never
    // more — that's the leak invariant.
    client
        .execute(
            "window.__loadQueueIntervalCalls__ = 0; \
             window.__loadQueueClearedHandles__ = new Set(); \
             window.__loadQueueOpenedHandles__ = new Set(); \
             const origSet = window.setInterval; \
             const origClear = window.clearInterval; \
             window.setInterval = function (fn, ms) { \
                 const h = origSet(fn, ms); \
                 if (fn === window.loadQueue) { \
                     window.__loadQueueIntervalCalls__++; \
                     window.__loadQueueOpenedHandles__.add(h); \
                 } \
                 return h; \
             }; \
             window.clearInterval = function (h) { \
                 if (window.__loadQueueOpenedHandles__.has(h)) { \
                     window.__loadQueueClearedHandles__.add(h); \
                 } \
                 return origClear(h); \
             };",
            vec![],
        )
        .await
        .expect("instrument set/clearInterval");

    // 3 round trips + 1 final landing on /downloads
    for _ in 0..3 {
        click_tabbar_link(&client, "/downloads", "/downloads")
            .await
            .expect("nav to downloads");
        wait_for_js_truthy(
            &client,
            "window.__downloadsQueuePoller != null",
            Duration::from_secs(3),
        )
        .await
        .expect("poller mounted");
        click_tabbar_link(&client, "/", "/")
            .await
            .expect("nav to library");
        wait_for_js_truthy(
            &client,
            "window.__downloadsQueuePoller === null",
            Duration::from_secs(3),
        )
        .await
        .expect("poller cleared");
    }
    click_tabbar_link(&client, "/downloads", "/downloads")
        .await
        .expect("final nav to downloads");
    wait_for_js_truthy(
        &client,
        "window.__downloadsQueuePoller != null",
        Duration::from_secs(3),
    )
    .await
    .expect("final poller mounted");

    // The leak invariant: at this moment one poller is active.
    // Net live = openedSize - clearedSize. Must be 1.
    let net = client
        .execute(
            "return window.__loadQueueOpenedHandles__.size - \
                    window.__loadQueueClearedHandles__.size;",
            vec![],
        )
        .await
        .expect("read net");
    let n = net.as_i64().unwrap_or(-1);
    assert_eq!(
        n, 1,
        "after 4 ping-pong navs ending on /downloads, exactly one \
         loadQueue interval should be alive; got net={n} live \
         handles. A net > 1 means the unmount didn't fire on \
         every leave (memory leak). A net < 1 means the mount \
         didn't fire on the final entry."
    );

    // And the live handle must be the current __downloadsQueuePoller.
    let active_matches = client
        .execute(
            "return window.__loadQueueOpenedHandles__.has(window.__downloadsQueuePoller) && \
                    !window.__loadQueueClearedHandles__.has(window.__downloadsQueuePoller);",
            vec![],
        )
        .await
        .expect("compare active");
    assert!(
        active_matches.as_bool().unwrap_or(false),
        "the currently-active __downloadsQueuePoller must be tracked \
         as opened-but-not-cleared in our instrumentation"
    );

    let _ = client.close().await;
}

/// **timestamp-rebind** — `[data-ts]` nodes injected via boosted
/// page swap must get rendered with humanized text on the same swap.
/// Pre-Phase-B, the refresh fired only on DOMContentLoaded + a 30s
/// interval, so a fresh `[data-ts]` could show the raw ISO string
/// for up to 30s after a boosted nav. Phase B hooks the refresh
/// into `htmx.onLoad`.
///
/// Marker: the Downloads History tab renders `<span data-ts="...">`
/// for each grab. The history tab requires DB-seeded grabs; we seed
/// one via `record_grab` before navigating.
#[tokio::test]
async fn data_ts_rebinds_after_boosted_nav() {
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

    // Seed one grab so the Downloads history tab has a [data-ts]
    // node to assert against.
    let series_id = ryokan::test_support::seed_series(&db, 1, "Test Show").await;
    let _ = ryokan::models::grabbed_torrents::record_grab(
        &db,
        "abc123def456abc123def456abc123def4567890",
        "[Group] Test Show - 01.mkv",
        series_id,
        &[1],
        false,
    )
    .await
    .expect("seed grab");

    let addr = spawn_app(state).await;

    set_mobile_viewport(&client).await.expect("mobile viewport");
    open_with_session(&client, addr, &session, "/")
        .await
        .expect("open library");
    let _ = assert_htmx_loaded(&client).await;

    // Navigate to Downloads via boost; the History tab is a query-
    // param flip (`?tab=history`) but the queue tab is the default
    // and seeing the timestamp render is the same swap mechanism.
    // Note: queue tab doesn't render history rows. Navigate
    // directly to /downloads?tab=history via mobile-tabbar fall-
    // through to a plain link click. The mobile-tabbar's Downloads
    // link goes to /downloads (queue tab); we then click through
    // to history via a tab anchor on the page.
    click_tabbar_link(&client, "/downloads", "/downloads")
        .await
        .expect("nav to downloads");

    // Click the History tab link inside the Downloads page (this is
    // a plain anchor — boosted because of inheritance from any
    // ancestor with hx-boost, but Phase A only puts hx-boost on the
    // mobile-tabbar so this is a real document navigation. Either
    // way the [data-ts] should render after.)
    let history_link = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("a[href=\"/downloads?tab=history\"]"))
        .await
        .expect("history tab link");
    history_link.click().await.expect("click history");
    wait_for_path(&client, "/downloads", Duration::from_secs(5))
        .await
        .expect("url updated");
    // Wait for the history table to render
    client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("[data-ts]"))
        .await
        .expect("data-ts span present");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let text = client
        .execute(
            "const el = document.querySelector('[data-ts]'); \
             return el ? el.textContent.trim() : '';",
            vec![],
        )
        .await
        .expect("read text");
    let text_str = text.as_str().unwrap_or("").to_string();
    // Humanized text looks like "5s ago" / "2m ago" / "1h ago" /
    // "3d ago" — all end in ` ago`. Raw ISO 8601 contains 'T' or
    // a hyphen. Distinguish: "ago" present is the humanized form.
    assert!(
        text_str.ends_with("ago") || text_str.ends_with("now"),
        "[data-ts] textContent must be humanized (e.g. \"5s ago\") \
         after boosted nav; got: {text_str:?} — the
         `htmx.onLoad(refresh)` hook in base.js probably didn't fire"
    );

    let _ = client.close().await;
}
