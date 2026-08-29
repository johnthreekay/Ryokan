//! hx-boost rollout — Phase D browser-e2e coverage.
//!
//! Phase D scope per the hx-boost rollout plan:
//!   - `<body hx-boost="true">` covers every plain `<a>` and `<form>`
//!     site-wide
//!   - `<a href="/logout" hx-boost="false">` opt-out
//!   - `htmx.config.historyEnableCache = false` so back/forward refetch
//!     dynamic pages instead of restoring stale snapshots
//!
//! Tests pin the body-wide invariants Phase A's narrow opt-in didn't
//! exercise: pentagon nav, back/forward navigation, the logout opt-out,
//! and session-expiry middleware redirects (the `htmx_aware_redirect_from_req`
//! path in `require_auth`).
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

async fn set_desktop_viewport(client: &fantoccini::Client) -> Result<(), String> {
    // Desktop width so the top-nav `.nav-links` is visible. The
    // mobile-tabbar (display:none above 640px) doesn't matter for
    // pentagon-nav coverage; the desktop nav already links to all
    // five pages and is visible at 1280×900.
    client
        .set_window_rect(0, 0, 1280, 900)
        .await
        .map_err(|e| format!("set_window_rect: {e}"))
}

/// Click a top-nav link and wait for the URL to update. Top nav uses
/// the desktop `.nav-links` slot, all five pages reachable.
async fn click_top_nav(
    client: &fantoccini::Client,
    href: &str,
    expected_path: &str,
) -> Result<(), String> {
    let sel = format!(".nav-links a[href=\"{href}\"]");
    let link = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(&sel))
        .await
        .map_err(|e| format!("nav link {href}: {e}"))?;
    link.click()
        .await
        .map_err(|e| format!("click {href}: {e}"))?;
    wait_for_path(client, expected_path, Duration::from_secs(5)).await
}

/// **pentagon-nav (full 5×4)** — every directed pair of the five
/// top-level pages, in both directions. With `<body hx-boost="true">`
/// every transition should be a body+head diff swap, not a real
/// document load — proven by a window-scoped marker that survives
/// boosted swaps but a real nav wipes.
///
/// Earlier draft of this test ran one 5-hop chain; the reviewer
/// flagged that an "X→Y is unstyled but only navigating in that
/// direction" regression could slip through. The 20-transition
/// matrix runs in ~5s wall-clock against a real LibreWolf and
/// catches per-direction asymmetries.
#[tokio::test]
async fn boost_navigates_pentagon_via_body_level_opt_in() {
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

    // Plant a window marker — boosted nav preserves window scope, so
    // if every transition keeps the marker alive, we know each hop
    // went through htmx (a real document load would have wiped it).
    client
        .execute("window.__phaseDMarker = 'boosted';", vec![])
        .await
        .expect("plant marker");

    // (path, label) pairs. Every directed pair (origin, dest) where
    // origin != dest gets one boosted click. After each, the marker
    // is re-checked — fast-fail diagnostic naming the broken hop.
    let pages = [
        ("/", "library"),
        ("/search", "search"),
        ("/downloads", "downloads"),
        ("/settings", "settings"),
        ("/system", "system"),
    ];

    for &(_, origin_label) in &pages {
        // Land on origin via direct goto — a fresh boosted nav per
        // pair starts each transition from a known state.
        // (Skip the goto on the first iteration since we're
        //  already on /; the boost-click below covers the move.)
        for &(dest_path, dest_label) in &pages {
            if origin_label == dest_label {
                continue;
            }
            // Navigate to the origin via boost (or direct, doesn't
            // matter — we're testing the final hop).
            let origin_path = pages
                .iter()
                .find(|p| p.1 == origin_label)
                .map(|p| p.0)
                .unwrap();
            click_top_nav(&client, origin_path, origin_path)
                .await
                .unwrap_or_else(|e| panic!("nav to origin {origin_label}: {e}"));
            // Now the discriminating boost-click: origin → dest.
            click_top_nav(&client, dest_path, dest_path)
                .await
                .unwrap_or_else(|e| panic!("nav {origin_label}→{dest_label}: {e}"));

            let marker = client
                .execute("return window.__phaseDMarker || null;", vec![])
                .await
                .expect("read marker");
            let marker_str = marker.as_str().unwrap_or("");
            assert_eq!(
                marker_str, "boosted",
                "transition {origin_label}→{dest_label}: window marker \
                 was wiped — that hop did a real document load instead \
                 of boost-swapping. Got marker={marker_str:?}"
            );
        }
    }
    // Release the single geckodriver session for the next test.
    let _ = client.close().await;
}

/// **logout-opt-out** — `<a href="/logout" hx-boost="false">` must do
/// a real document navigation, not a boosted swap. The logout flow
/// hits a 303 to /login, the auth middleware redirects, the session
/// cookie clears via `Set-Cookie: Max-Age=0` — none of that is
/// boost-friendly. Verify by planting a window marker pre-click; a
/// real nav wipes it.
#[tokio::test]
async fn logout_link_opt_out_does_real_document_nav() {
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

    client
        .execute("window.__logoutMarker = 'present';", vec![])
        .await
        .expect("plant marker");

    let logout_link = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("a[href=\"/logout\"]"))
        .await
        .expect("logout link");
    logout_link.click().await.expect("click logout");

    // Logout → 303 → /login. Wait for the URL to settle.
    wait_for_path(&client, "/login", Duration::from_secs(5))
        .await
        .expect("logout redirected to /login");

    // The marker should be GONE — `hx-boost="false"` made the click
    // a real document nav, which destroys the prior window scope.
    let marker = client
        .execute("return typeof window.__logoutMarker;", vec![])
        .await
        .expect("read marker");
    let typeof_str = marker.as_str().unwrap_or("");
    assert_eq!(
        typeof_str, "undefined",
        "after clicking logout, the prior window's marker should be gone \
         (real document nav). Got typeof={typeof_str:?} — boost may have \
         intercepted the click despite the hx-boost=\"false\" opt-out"
    );
}

/// **session-expiry** — the `require_auth` middleware redirect to
/// `/login` must work cleanly under boost. Phase C wired
/// `htmx_aware_redirect_from_req` into `require_auth`; this test
/// pins the end-to-end behavior.
///
/// Setup: legitimate session, then nuke it from the DB mid-session.
/// Click a boosted top-nav link. Expected: htmx receives a 200 +
/// `HX-Redirect: /login` from the middleware, triggers a real
/// `window.location` navigation, and the login form renders. Without
/// the Phase C migration, the middleware's bare 303 would get
/// fetched-and-swapped by boost and the login HTML would inline-swap
/// into the prior page's body (nesting).
#[tokio::test]
async fn boosted_nav_to_protected_page_with_invalidated_session_lands_on_login() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session_token = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    set_desktop_viewport(&client)
        .await
        .expect("desktop viewport");
    open_with_session(&client, addr, &session_token, "/")
        .await
        .expect("open library");
    let _ = assert_htmx_loaded(&client).await;

    // Wipe the session from the DB so the middleware's
    // `validate_session` returns Ok(None) → redirect-to-login.
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(&session_token)
        .execute(&db)
        .await
        .expect("delete session");

    // Boost-click any top-nav link. Middleware refuses, returns
    // 200 + HX-Redirect: /login. htmx triggers real nav.
    let settings_link = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(".nav-links a[href=\"/settings\"]"))
        .await
        .expect("settings link");
    settings_link.click().await.expect("click");

    wait_for_path(&client, "/login", Duration::from_secs(5))
        .await
        .expect("invalidated-session click should land on /login");

    // Confirm the login form is rendered.
    client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("form[action=\"/login\"]"))
        .await
        .expect("login form visible at /login");

    // Discriminating assertion — if the auth middleware had returned
    // a bare 303 (the pre-Phase-C shape), boost would have followed
    // it transparently via fetch and inline-swapped /login's body
    // into the prior page's main. URL would still settle on /login;
    // login form would still be present (it's in the swapped HTML);
    // the URL+form-only assertions above would pass even though
    // boost was nesting.
    //
    // What CAN'T survive a nested swap: the prior page's `<nav class="nav">`
    // topbar. /login is a standalone template (`templates/login.html`,
    // doesn't extend base.html) so it doesn't render the topbar.
    // A real nav to /login → topbar gone. A nested boost-follow
    // would leave the prior page's topbar in place because boost's
    // default target is body innerHTML and the topbar lives inside
    // the body of the prior page.
    let topbar_count = client
        .execute(
            "return document.querySelectorAll('nav.nav').length;",
            vec![],
        )
        .await
        .expect("count nav.nav");
    let count = topbar_count.as_u64().unwrap_or(99);
    assert_eq!(
        count, 0,
        "post-redirect /login must be a real document load — found \
         {count} nav.nav element(s) leftover from the prior page, \
         which means the auth middleware's redirect was followed \
         via boost (fetch + inline swap) instead of `HX-Redirect` \
         triggering a real window.location. Phase C's \
         `htmx_aware_redirect_from_req` may have regressed."
    );
    let _ = client.close().await;
}

/// **history-refetches** — browser-back must refetch the page, never
/// restore a stale snapshot (a Downloads queue captured on the prior
/// visit). htmx 2 needed `historyEnableCache: false` for that; htmx 4
/// has no snapshot cache and refetches whenever `config.history` is
/// truthy (the default), unless someone loads the `hx-history-cache`
/// extension or sets `history: false`.
///
/// We can't easily drive a back/forward in fantoccini and prove the
/// fetch happened (would need network instrumentation that
/// geckodriver doesn't expose cleanly). Instead pin the config: if a
/// future edit disables history or re-adds a cache, this catches it.
#[tokio::test]
async fn history_cache_is_disabled() {
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

    // Wait for htmx to load, then pin the history config: managed
    // history on (so back / forward refetch through htmx) and no
    // history-cache extension registered.
    wait_for_js_truthy(
        &client,
        "window.htmx && window.htmx.config && window.htmx.config.history === true \
         && typeof window.htmx.config.historyEnableCache === 'undefined' \
         && !document.querySelector('script[src*=\"hx-history-cache\"]')",
        Duration::from_secs(5),
    )
    .await
    .expect(
        "htmx.config.history must stay true with no history-cache extension \
         loaded (htmx 4 refetches on back / forward by default). Got: htmx \
         loaded but the config was changed or a cache extension is present.",
    );
    let _ = client.close().await;
}
