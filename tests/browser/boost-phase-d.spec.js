// hx-boost rollout, Phase D coverage.
//
// Phase D scope per the hx-boost rollout plan:
//   - `<body hx-boost="true">` covers every plain `<a>` and `<form>`
//     site-wide
//   - `<form id="logout-form" hx-boost="false">` opt-out
//   - `htmx.config.historyEnableCache = false` so back/forward refetch
//     dynamic pages instead of restoring stale snapshots
//
// Tests pin the body-wide invariants Phase A's narrow opt-in didn't
// exercise: pentagon nav, back/forward navigation, the logout opt-out,
// and session-expiry middleware redirects (the `htmx_aware_redirect_from_req`
// path in `require_auth`).

import {
  test,
  expect,
  assertHtmxLoaded,
  exec,
  waitForJsTruthy,
  waitForPath,
  withMessage,
} from './harness.js';

async function setDesktopViewport(page) {
  // Desktop width so the top-nav `.nav-links` is visible. The
  // mobile-tabbar (display:none above 640px) doesn't matter for
  // pentagon-nav coverage; the desktop nav already links to all
  // five pages and is visible at 1280x900.
  await page.setViewportSize({ width: 1280, height: 900 });
}

// Rust's `.expect(message)` on a helper's Result: the failure names
// what was being checked, then the helper's own error.
/**
 * Click a top-nav link and wait for the URL to update. Top nav uses
 * the desktop `.nav-links` slot, all five pages reachable.
 */
async function clickTopNav(page, href, expectedPath) {
  const link = page.locator(`.nav-links a[href="${href}"]`).first();
  await withMessage(link.waitFor({ state: 'attached', timeout: 5000 }), `nav link ${href}`);
  await withMessage(link.click(), `click ${href}`);
  await waitForPath(page, expectedPath, 5000);
}

// pentagon-nav (full 5x4): every directed pair of the five top-level
// pages, in both directions. With `<body hx-boost="true">` every
// transition should be a body+head diff swap, not a real document
// load, proven by a window-scoped marker that survives boosted swaps
// but a real nav wipes.
//
// Earlier draft of this test ran one 5-hop chain; the reviewer
// flagged that an "X->Y is unstyled but only navigating in that
// direction" regression could slip through. The 20-transition matrix
// runs in about 5s wall-clock in a real browser and catches
// per-direction asymmetries.
test('boost_navigates_pentagon_via_body_level_opt_in', async ({ page }) => {
  await setDesktopViewport(page);
  await page.goto('/');
  // The Rust original computed this and threw the result away
  // (`let _ =`). Enforced here.
  await assertHtmxLoaded(page);

  // Plant a window marker. Boosted nav preserves window scope, so if
  // every transition keeps the marker alive, we know each hop went
  // through htmx (a real document load would have wiped it).
  await exec(page, "window.__phaseDMarker = 'boosted';");

  // (path, label) pairs. Every directed pair (origin, dest) where
  // origin != dest gets one boosted click. After each, the marker is
  // re-checked: fast-fail diagnostic naming the broken hop.
  const pages = [
    ['/', 'library'],
    ['/search', 'search'],
    ['/downloads', 'downloads'],
    ['/settings', 'settings'],
    ['/system', 'system'],
  ];

  for (const [, originLabel] of pages) {
    // Land on origin via direct goto: a fresh boosted nav per pair
    // starts each transition from a known state.
    // (Skip the goto on the first iteration since we're already on /;
    //  the boost-click below covers the move.)
    for (const [destPath, destLabel] of pages) {
      if (originLabel === destLabel) {
        continue;
      }
      // Navigate to the origin via boost (or direct, doesn't matter:
      // we're testing the final hop).
      const originPath = pages.find((p) => p[1] === originLabel)[0];
      // Only when not already there. Clicking the link of the page we
      // are on starts a boosted reload whose path wait passes at once,
      // so the next click raced that reload's response and the late
      // response put the page back.
      const here = new URL(page.url()).pathname;
      if (here !== originPath) {
        await withMessage(
          clickTopNav(page, originPath, originPath),
          `nav to origin ${originLabel}`,
        );
      }
      // Now the discriminating boost-click: origin -> dest.
      await withMessage(
        clickTopNav(page, destPath, destPath),
        `nav ${originLabel}→${destLabel}`,
      );

      const marker = await exec(page, 'return window.__phaseDMarker || null;');
      const markerStr = typeof marker === 'string' ? marker : '';
      expect(
        markerStr,
        `transition ${originLabel}→${destLabel}: window marker ` +
          'was wiped: that hop did a real document load instead ' +
          `of boost-swapping. Got marker=${JSON.stringify(markerStr)}`,
      ).toBe('boosted');
    }
  }
});

// logout-opt-out: the Logout button submits
// `<form id="logout-form" method="post" action="/logout" hx-boost="false">`,
// which must do a real document navigation, not a boosted swap. The
// logout flow hits a 303 to /login, the auth middleware redirects, the
// session cookie clears via `Set-Cookie: Max-Age=0`; none of that is
// boost-friendly. Verify by planting a window marker pre-click; a real
// nav wipes it.
test('logout_button_opt_out_does_real_document_nav', async ({ page }) => {
  await setDesktopViewport(page);
  await page.goto('/');
  // Enforced; the Rust original threw this result away.
  await assertHtmxLoaded(page);

  await exec(page, "window.__logoutMarker = 'present';");

  const logoutButton = page.locator('button.nav-logout').first();
  await withMessage(
    logoutButton.waitFor({ state: 'attached', timeout: 5000 }),
    'logout button',
  );
  await logoutButton.click();

  // Logout -> 303 -> /login. Wait for the URL to settle.
  await withMessage(waitForPath(page, '/login', 5000), 'logout redirected to /login');

  // The marker should be GONE: `hx-boost="false"` made the click a
  // real document nav, which destroys the prior window scope.
  const marker = await exec(page, 'return typeof window.__logoutMarker;');
  const typeofStr = typeof marker === 'string' ? marker : '';
  expect(
    typeofStr,
    "after clicking logout, the prior window's marker should be gone " +
      `(real document nav). Got typeof=${JSON.stringify(typeofStr)}: boost may have ` +
      'intercepted the click despite the hx-boost="false" opt-out',
  ).toBe('undefined');
});

// session-expiry: the `require_auth` middleware redirect to `/login`
// must work cleanly under boost. Phase C wired
// `htmx_aware_redirect_from_req` into `require_auth`; this test pins
// the end-to-end behavior.
//
// Setup: legitimate session, then nuke it from the DB mid-session.
// Click a boosted top-nav link. Expected: htmx receives a 200 +
// `HX-Redirect: /login` from the middleware, triggers a real
// `window.location` navigation, and the login form renders. Without
// the Phase C migration, the middleware's bare 303 would get
// fetched-and-swapped by boost and the login HTML would inline-swap
// into the prior page's body (nesting).
test('boosted_nav_to_protected_page_with_invalidated_session_lands_on_login', async ({
  app,
  page,
}) => {
  await setDesktopViewport(page);
  await page.goto('/');
  // Enforced; the Rust original threw this result away.
  await assertHtmxLoaded(page);

  // Wipe the session from the DB so the middleware's
  // `validate_session` returns Ok(None) -> redirect-to-login. Through
  // the model (the fixture host calls it): the table holds the token's
  // hash, so a raw `WHERE token = ?` on the cookie value deletes
  // nothing.
  await app.deleteSession();
  expect(await app.sessionValid(), 'the session is gone').toBe(false);

  // Boost-click any top-nav link. Middleware refuses, returns
  // 200 + HX-Redirect: /login. htmx triggers real nav.
  const settingsLink = page.locator('.nav-links a[href="/settings"]').first();
  await withMessage(
    settingsLink.waitFor({ state: 'attached', timeout: 5000 }),
    'settings link',
  );
  await settingsLink.click();

  await withMessage(
    waitForPath(page, '/login', 5000),
    'invalidated-session click should land on /login',
  );

  // Confirm the login form is rendered.
  await withMessage(
    page.locator('form[action="/login"]').first().waitFor({ state: 'attached', timeout: 5000 }),
    'login form visible at /login',
  );

  // Discriminating assertion: if the auth middleware had returned a
  // bare 303 (the pre-Phase-C shape), boost would have followed it
  // transparently via fetch and inline-swapped /login's body into the
  // prior page's main. URL would still settle on /login; login form
  // would still be present (it's in the swapped HTML); the URL+form-only
  // assertions above would pass even though boost was nesting.
  //
  // What CAN'T survive a nested swap: the prior page's `<nav class="nav">`
  // topbar. /login is a standalone template (`templates/login.html`,
  // doesn't extend base.html) so it doesn't render the topbar. A real
  // nav to /login: topbar gone. A nested boost-follow would leave the
  // prior page's topbar in place because boost's default target is
  // body innerHTML and the topbar lives inside the body of the prior
  // page.
  const topbarCount = await exec(page, "return document.querySelectorAll('nav.nav').length;");
  const count = typeof topbarCount === 'number' ? topbarCount : 99;
  expect(
    count,
    'post-redirect /login must be a real document load: found ' +
      `${count} nav.nav element(s) leftover from the prior page, ` +
      "which means the auth middleware's redirect was followed " +
      'via boost (fetch + inline swap) instead of `HX-Redirect` ' +
      "triggering a real window.location. Phase C's " +
      '`htmx_aware_redirect_from_req` may have regressed.',
  ).toBe(0);
});

// history-refetches: browser-back must refetch the page, never restore
// a stale snapshot (a Downloads queue captured on the prior visit).
// htmx 2 needed `historyEnableCache: false` for that; htmx 4 has no
// snapshot cache and refetches whenever `config.history` is truthy
// (the default), unless someone loads the `hx-history-cache` extension
// or sets `history: false`.
//
// Driving a back/forward and proving the fetch happened would need
// network instrumentation the WebDriver suite this was written for
// didn't expose cleanly. Instead pin the config: if a future edit
// disables history or re-adds a cache, this catches it.
test('history_cache_is_disabled', async ({ page }) => {
  await setDesktopViewport(page);
  await page.goto('/');

  // Wait for htmx to load, then pin the history config: managed
  // history on (so back / forward refetch through htmx) and no
  // history-cache extension registered.
  await withMessage(
    waitForJsTruthy(
      page,
      'window.htmx && window.htmx.config && window.htmx.config.history === true ' +
        "&& typeof window.htmx.config.historyEnableCache === 'undefined' " +
        '&& !document.querySelector(\'script[src*="hx-history-cache"]\')',
      5000,
    ),
    'htmx.config.history must stay true with no history-cache extension ' +
      'loaded (htmx 4 refetches on back / forward by default). Got: htmx ' +
      'loaded but the config was changed or a cache extension is present.',
  );
});
