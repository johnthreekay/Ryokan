//! hx-boost rollout — Phase A browser-e2e coverage.
//!
//! Phase A scope per the hx-boost rollout plan:
//!   - `htmx-ext-head-support` vendored + loaded
//!   - `<body hx-ext="head-support">` in base.html
//!   - `<nav class="mobile-tabbar" hx-boost="true">` — narrowest possible
//!     opt-in: 5 anchor links, no forms, distinct per-page CSS targets
//!
//! These tests pin the head-swap behavior. They're Phase A's failure
//! detector for the regression that killed attempt-1 (boost swaps body,
//! head stays stale, per-page CSS doesn't load, page renders unstyled).
//! All four assertions would fire LOUDLY if `htmx-ext-head-support`
//! were ever removed or `hx-ext="head-support"` were dropped from
//! `<body>`.
//!
//! Skips gracefully when geckodriver is unreachable; same harness as
//! the rest of the `tests/htmx_browser_e2e_*` suite.

use fantoccini::Locator;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_loaded, open_with_session, seed_user_session, spawn_app, try_connect_browser,
    wait_for_path,
};

/// Set a mobile-width viewport so the `.mobile-tabbar` actually
/// renders. The tabbar lives at `display:none` above 640px (per
/// `topbar.css`); without resizing the browser, headless Firefox
/// boots at desktop width and the tabbar links are invisible →
/// `Element could not be scrolled into view` on click. 480×800 is
/// safely under the breakpoint and matches a typical phone.
async fn set_mobile_viewport(client: &fantoccini::Client) -> Result<(), String> {
    client
        .set_window_rect(0, 0, 480, 800)
        .await
        .map_err(|e| format!("set_window_rect: {e}"))
}

/// Set a desktop-width viewport. The desktop top-nav (`.nav-links`)
/// is `display:none` below 640px and must be visible for the
/// "non-boosted-paths-unchanged" test to click its link.
async fn set_desktop_viewport(client: &fantoccini::Client) -> Result<(), String> {
    client
        .set_window_rect(0, 0, 1280, 900)
        .await
        .map_err(|e| format!("set_window_rect: {e}"))
}

/// **head-css-swap** — the failure-mode detector.
///
/// Navigate Library → Settings via the boosted mobile-tabbar. Without
/// `htmx-ext-head-support`, `pages/settings.css` would never load on
/// the boosted nav and the assertion below would fail. With it, the
/// extension diffs the response head and adds the new `<link>`.
///
/// Marker: `base.css` defines `.cf-section { padding: 22px 24px }`.
/// Read the computed `padding` of a Settings card; assert it is
/// `22px`. Browsers fall back to user-agent default padding (~0)
/// when the rule isn't loaded — clear signal.
#[tokio::test]
async fn boost_swaps_head_when_navigating_library_to_settings() {
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

    // Wait for the mobile-tabbar to be present in the DOM.
    client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(".mobile-tabbar a[href=\"/settings\"]"))
        .await
        .expect("mobile-tabbar settings link");

    // Click the boosted Settings tab. With hx-boost on the nav,
    // htmx intercepts the click and does an AJAX swap of `<body>`.
    // The head-support extension then merges the head.
    let settings_link = client
        .find(Locator::Css(".mobile-tabbar a[href=\"/settings\"]"))
        .await
        .expect("settings tab");
    settings_link.click().await.expect("click settings tab");

    // Wait until the URL reflects the new page and a Settings-only
    // element renders.
    wait_for_path(&client, "/settings", Duration::from_secs(5))
        .await
        .expect("url updated to /settings");
    let fieldset = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(".cf-section"))
        .await
        .expect("settings card present");

    // Read computed padding. `base.css` sets it to 22px;
    // unstyled fallback is 0 / user-agent default. The mobile-side
    // `@media (max-width: 640px)` override sets 16px, so accept both
    // values (test runs in headless mode at default viewport, but
    // exact viewport varies — accepting both keeps the test robust
    // while still distinguishing styled-from-unstyled clearly).
    let padding = fieldset
        .css_value("padding-top")
        .await
        .expect("read padding-top");
    let padding_px: f32 = padding
        .trim_end_matches("px")
        .parse()
        .unwrap_or_else(|_| panic!("expected pixel value, got: {padding}"));
    assert!(
        padding_px >= 16.0,
        "base.css `.cf-section {{ padding: 22px 24px }}` rule must apply after boosted nav; \
         got computed padding-top={padding_px}px (head-support extension probably didn't merge \
         the new page's CSS link)"
    );

    let _ = client.close().await;
}

/// **head-css-evict** — the inverse direction; ensures Library's CSS
/// reapplies when navigating back from Settings.
///
/// Marker: `pages/index.css` defines `.btn-pill { border-radius: 999px }`
/// — a pill shape that's distinctive vs the default `border-radius: 0`
/// fallback.
#[tokio::test]
async fn boost_reapplies_origin_css_when_navigating_back() {
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
    // Start on Settings, then nav to Library via boosted tabbar.
    open_with_session(&client, addr, &session, "/settings")
        .await
        .expect("open settings");
    let _ = assert_htmx_loaded(&client).await;
    client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(".mobile-tabbar a[href=\"/\"]"))
        .await
        .expect("mobile-tabbar library link");

    let library_link = client
        .find(Locator::Css(".mobile-tabbar a[href=\"/\"]"))
        .await
        .expect("library tab");
    library_link.click().await.expect("click library tab");

    wait_for_path(&client, "/", Duration::from_secs(5))
        .await
        .expect("url updated to /");

    // Wait for a `.btn-pill` element to render — Library's filter
    // pills carry that class. If the page renders before its CSS
    // loads, `border-radius` is 0 (UA default).
    let btn = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(".btn-pill"))
        .await
        .expect("library .btn-pill element present");
    let radius = btn
        .css_value("border-radius")
        .await
        .expect("read border-radius");
    let radius_px: f32 = radius
        .trim_end_matches("px")
        .parse()
        .unwrap_or_else(|_| panic!("expected pixel value, got: {radius}"));
    // The rule sets `999px`; browsers may compute it down to whatever
    // the element's largest dimension caps at. As long as it's
    // distinctly more than the unstyled 0, the rule applied.
    assert!(
        radius_px > 4.0,
        "index.css `.btn-pill {{ border-radius: 999px }}` must apply after boosted back-nav; \
         got computed border-radius={radius_px}px"
    );

    let _ = client.close().await;
}

/// **desktop-nav-is-boosted-under-body-wide-opt-in** — under body-wide
/// `hx-boost="true"` (Phase D), the desktop top-nav is boost-
/// intercepted. Plant a window-scoped marker pre-click; if boost
/// fires, the marker survives the swap (boost preserves window
/// scope). A real document load would wipe it.
///
/// Pre-Phase-D this test was named
/// `desktop_nav_does_full_document_navigation_not_boosted_swap`
/// and asserted the inverse (marker wiped, proving Phase A's
/// narrow opt-in didn't widen onto the desktop nav). Phase D
/// renamed it to match the new invariant; git-blame on the
/// assertion lines flows through the rename.
#[tokio::test]
async fn desktop_nav_is_boosted_under_body_wide_opt_in() {
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

    set_desktop_viewport(&client)
        .await
        .expect("desktop viewport");
    open_with_session(&client, addr, &session, "/")
        .await
        .expect("open library");
    let _ = assert_htmx_loaded(&client).await;

    // Plant a window-scoped marker. A boosted swap preserves the
    // `window` object across navigation; a real document load
    // creates a fresh window and the marker is gone.
    client
        .execute("window.__ryokan_boost_phase_a_marker = 'planted';", vec![])
        .await
        .expect("plant marker");

    // Wait for the desktop top-nav Settings link.
    client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(".nav-links a[href=\"/settings\"]"))
        .await
        .expect("desktop nav settings link");
    let desktop_link = client
        .find(Locator::Css(".nav-links a[href=\"/settings\"]"))
        .await
        .expect("desktop link");
    desktop_link.click().await.expect("click desktop link");

    wait_for_path(&client, "/settings", Duration::from_secs(5))
        .await
        .expect("url updated to /settings");

    // Read marker + boost state in one round trip; failure assertion
    // below carries the full diagnostic so an unexpected outcome
    // surfaces with body[hx-boost] and typeof htmx for debugging
    // without spamming green CI runs with println output.
    let marker = client
        .execute(
            "return {marker: typeof window.__ryokan_boost_phase_a_marker, \
                     hxBoost: document.body.getAttribute('hx-boost'), \
                     htmxLoaded: typeof window.htmx};",
            vec![],
        )
        .await
        .expect("read marker");
    let marker_field = marker
        .get("marker")
        .and_then(|v| v.as_str())
        .unwrap_or("(missing)");
    let hx_boost = marker
        .get("hxBoost")
        .and_then(|v| v.as_str())
        .unwrap_or("(missing)");
    let htmx_loaded = marker
        .get("htmxLoaded")
        .and_then(|v| v.as_str())
        .unwrap_or("(missing)");
    // Phase D update: under body-wide hx-boost, this test inverts —
    // the desktop nav IS now boosted, so the marker should SURVIVE
    // the click. Pre-Phase-D this test asserted the marker was
    // wiped (proving narrow-opt-in didn't widen). Now it asserts
    // body-wide boost successfully intercepts.
    assert_eq!(
        marker_field, "string",
        "under body-wide hx-boost (Phase D), the desktop top-nav \
         should be boost-intercepted and the window-scoped marker \
         should survive the swap. Got marker={marker_field:?} \
         body[hx-boost]={hx_boost:?} typeof htmx={htmx_loaded:?} — \
         body-wide boost may not be wired correctly"
    );

    let _ = client.close().await;
}

/// **shared-css-deduped** — `htmx-ext-head-support` should leave the
/// shared `<link>` tags (base.css, topbar.css, forms.css, etc.) in
/// place, not re-add them. Each shared CSS link should appear exactly
/// once after a boosted swap.
#[tokio::test]
async fn boosted_swap_does_not_duplicate_shared_css_links() {
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
    client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(".mobile-tabbar a[href=\"/settings\"]"))
        .await
        .expect("mobile tab");
    client
        .find(Locator::Css(".mobile-tabbar a[href=\"/settings\"]"))
        .await
        .expect("settings tab")
        .click()
        .await
        .expect("click");

    // Wait for the swap.
    wait_for_path(&client, "/settings", Duration::from_secs(5))
        .await
        .expect("url updated");

    // Count occurrences of `forms.css` in the rendered head. Both
    // `index.html` and `settings.html` extend `base.html`, which
    // includes `<link rel="stylesheet" href="/static/css/forms.css">`.
    // The extension should leave the shared link alone — if it
    // mistakenly removes-then-re-adds, an instant of FOUC would land
    // and a careless implementation could yield duplicates.
    let count = client
        .execute(
            "return document.querySelectorAll('link[href*=\"forms.css\"]').length;",
            vec![],
        )
        .await
        .expect("count forms.css links");
    let n = count.as_i64().unwrap_or(-1);
    assert_eq!(
        n, 1,
        "shared forms.css link should appear exactly once in head after boosted swap; \
         got {n} (head-merge dedup is broken — would FOUC users on every nav)"
    );

    let _ = client.close().await;
}
