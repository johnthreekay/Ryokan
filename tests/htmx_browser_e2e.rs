//! Browser-driving e2e tests for the HTMX migration (issue #129).
//!
//! These tests spin up a real Ryokan-like axum server on a random port,
//! point a real browser at it via WebDriver, and assert the post-click
//! DOM state. Unlike the inline handler tests in `htmx_settings_delete.rs`
//! / `crud.rs::tests`, this proves the *full* loop end-to-end:
//!
//!   • the static htmx script loads from `/static/vendor/`
//!   • `hx-vals` form-encoding round-trips to the `Form` extractor
//!   • the handler's HTML response actually swaps into the DOM at
//!     `hx-target` with `hx-swap=outerHTML`
//!
//! Pure handler unit tests can verify each piece in isolation, but a
//! regression in any of htmx-script-loaded / form-encoding-correct /
//! response-shape-matches only surfaces when the three meet in a
//! browser. Hence this scaffold.
//!
//! ## Running locally
//!
//! 1. Install geckodriver + a Firefox-family browser (LibreWolf works).
//!    Arch: `sudo pacman -S geckodriver`. Confirm with `geckodriver --version`.
//! 2. Start geckodriver on a port the harness will dial:
//!    `geckodriver --port=4444 &`
//!    (Override with `RYOKAN_WEBDRIVER_URL=http://...` if you run it
//!    elsewhere or want chromedriver instead.)
//! 3. Run the suite:
//!    `cargo test --features browser-e2e --test htmx_browser_e2e`
//!
//! Tests are NOT enabled in CI: they require an out-of-band driver +
//! browser binary, and the goal is local-iteration speed during the
//! HTMX migration. Browser e2e is now considered general-purpose
//! infrastructure for future template-driven UI work (#125 bulk ops,
//! #121 notification settings, #116 calendar, Phase 7 rework, etc.) —
//! it is no longer scheduled for removal post-#129.
//!
//! ## When the test gracefully skips
//!
//! Connecting to `RYOKAN_WEBDRIVER_URL` (default `http://localhost:4444`)
//! is treated as an environmental precondition. If the connect fails —
//! geckodriver isn't running, or the browser isn't installed — each
//! test prints a one-line note and returns OK rather than failing the
//! suite. That keeps `cargo test --features browser-e2e` runnable on a
//! laptop without geckodriver pre-started, which matches how CI-gated
//! `live_smoke` tests (`RYOKAN_QBIT_E2E` etc.) opt into a real daemon.

use std::time::Duration;

use fantoccini::Locator;
use ryokan::test_support::{build_test_app_state, in_memory_pool, logged_in_session};

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{spawn_app, try_connect_browser};

#[tokio::test]
async fn episode_monitor_button_swaps_in_browser() {
    // Setup: a series + one monitored episode in the DB. The fixture
    // page renders the partial directly (no series-detail page render
    // needed) so the test isolates the htmx swap from page concerns.
    let db = in_memory_pool().await;
    let series_id = ryokan::test_support::seed_series(&db, 12345, "BrowserTest Anime").await;
    ryokan::models::monitoring::set_episode_monitored(&db, series_id, 1, true)
        .await
        .expect("seed monitored episode");
    let state = build_test_app_state(db.clone(), None);

    // Authenticated browsing session: write a row to `sessions` and
    // then preload the cookie via WebDriver before navigating, so the
    // first page load already passes `require_auth`.
    let (_state2, cookie_value) = logged_in_session(&db).await;
    let token = cookie_value
        .strip_prefix("session=")
        .expect("cookie helper returns session=<hex>")
        .to_string();

    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    // Drive the test, but always close the browser at the end — even
    // on assertion panics — so a stuck session doesn't leave a hung
    // browser process around.
    let result = async {
        // WebDriver requires a page in the target domain to be loaded
        // before `add_cookie` will accept a cookie scoped to it. Land
        // on /login first (always serves; no auth) and drop the
        // session cookie in.
        let base = format!("http://{addr}");
        client.goto(&format!("{base}/login")).await?;
        let raw = format!("session={token}; Path=/; SameSite=Lax");
        let cookie = fantoccini::cookies::Cookie::parse(raw)?;
        client.add_cookie(cookie).await?;

        // Now load the test fixture page that renders the partial.
        client
            .goto(&format!(
                "{base}/__test/episode-monitor-fixture?series_id={series_id}&episode_number=1"
            ))
            .await?;

        // Pre-click assertion: button reads "Yes" because we seeded
        // `monitored = true`.
        let button = client.find(Locator::Css("button.ep-mon-btn")).await?;
        let pre_text = button.text().await?;
        assert_eq!(
            pre_text.trim(),
            "Yes",
            "fixture should render the seeded monitored=true state"
        );
        let pre_class = button.attr("class").await?.unwrap_or_default();
        assert!(
            pre_class.contains("ep-mon-yes"),
            "pre-click button should carry ep-mon-yes class; got `{pre_class}`"
        );

        // Click. htmx fires the POST, the handler returns a fresh
        // button partial with `monitored = false`, and
        // `hx-swap=outerHTML` replaces the element. WebDriver hands
        // back stale-element after a swap — re-query rather than
        // reusing the old handle.
        button.click().await?;

        // Wait for the swap to land.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let btn = client.find(Locator::Css("button.ep-mon-btn")).await?;
            let text = btn.text().await?.trim().to_string();
            let class = btn.attr("class").await?.unwrap_or_default();
            if text == "No" && class.contains("ep-mon-no") {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "button did not swap to monitored=false within 3s; \
                     last text={text:?}, class={class:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    let _ = client.close().await;
    result.expect("episode monitor browser test");

    // Side-effect verification: the handler renders the partial from
    // the request's `monitored` value, so a no-op handler that skips
    // the DB write would still return a "monitored=false" button and
    // the browser test above would pass. Read the row back to confirm.
    let states = ryokan::models::monitoring::get_series_states(&db, series_id)
        .await
        .expect("read episode states");
    let ep1 = states.iter().find(|r| r.episode_number == 1);
    assert_eq!(
        ep1.map(|r| r.monitored),
        Some(false),
        "DB row must reflect the post-click state; got {ep1:?}"
    );
}

/// The two `hx-on::` shapes the series page relies on, under htmx 4's
/// colon-separated event names: a rejected POST reverts the checkbox
/// through `hx-on::response:error`, and `hx-on::after:request` reads
/// the outcome from `event.detail.ctx.response.status` (htmx 4 has no
/// `detail.successful`). Under htmx 2 spellings these attributes
/// silently bind to events that never fire.
#[tokio::test]
async fn hx_on_handlers_fire_under_htmx_4_event_names() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let (_state2, cookie_value) = logged_in_session(&db).await;
    let token = cookie_value
        .strip_prefix("session=")
        .expect("cookie helper returns session=<hex>")
        .to_string();
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        let base = format!("http://{addr}");
        client.goto(&format!("{base}/login")).await?;
        let raw = format!("session={token}; Path=/; SameSite=Lax");
        client
            .add_cookie(fantoccini::cookies::Cookie::parse(raw)?)
            .await?;
        client.goto(&format!("{base}/__test/hx-on-fixture")).await?;

        // Rejected POST: the change handler flips the box, the server
        // says 400, the response:error handler flips it back.
        client
            .find(Locator::Id("hx-on-toggle"))
            .await?
            .click()
            .await?;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let status = client
                .execute("return window.__hxOnErrorStatus || null;", vec![])
                .await?;
            if status.as_i64() == Some(400) {
                break;
            }
            if std::time::Instant::now() > deadline {
                let errors = client
                    .execute("return window.__ryokanFixtureErrors || [];", vec![])
                    .await?;
                panic!("hx-on::response:error never fired for the rejected POST; fixture errors: {errors}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let checked = client
            .find(Locator::Id("hx-on-toggle"))
            .await?
            .prop("checked")
            .await?;
        assert_eq!(
            checked.as_deref(),
            Some("true"),
            "the response:error handler must revert the checkbox to its server-side state"
        );

        // Accepted POST: after:request sees the 200.
        client
            .find(Locator::Id("hx-on-accept"))
            .await?
            .click()
            .await?;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let ok = client
                .execute("return window.__hxOnAfterRequestOk === true;", vec![])
                .await?;
            if ok.as_bool() == Some(true) {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("hx-on::after:request never saw a < 400 status for the accepted POST");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    let _ = client.close().await;
    result.expect("hx-on browser test");
}
