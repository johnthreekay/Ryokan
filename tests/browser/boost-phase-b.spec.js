// hx-boost rollout, Phase B browser coverage (per-page JS init
// lifecycle).
//
// Phase B scope per the rollout plan:
//   - `static/js/page_lifecycle.js` exposes `ryokanRegisterPageInit`
//   - `downloads.js` queue poller migrated to lifecycle (mount on
//     entry, unmount on exit)
//   - `system.js` logs poller migrated to lifecycle
//   - `base.js` `[data-ts]` refresh hooks into `htmx.onLoad`
//
// The polling-leak case is the highest-stakes regression Phase B
// prevents: a module-scope `setInterval` started once on initial
// document load runs forever and accumulates copies on every
// boosted re-entry. Tests 1 + 2 below are the explicit guard.

import {
  test,
  expect,
  assertHtmxLoaded,
  exec,
  waitForJsTruthy,
  waitForPath,
  withMessage,
} from './harness.js';

async function setMobileViewport(page) {
  await page.setViewportSize({ width: 480, height: 800 });
}

/**
 * Click the boosted mobile-tabbar link at `href` and wait for the URL
 * to update. Useful for chained navigations where each step must
 * finish before the next click.
 */
async function clickTabbarLink(page, href, expectedPath) {
  const link = page.locator(`.mobile-tabbar a[href="${href}"]`).first();
  await withMessage(link.waitFor({ state: 'attached', timeout: 5000 }), `link ${href}`);
  await withMessage(link.click(), `click ${href}`);
  await waitForPath(page, expectedPath, 5000);
}

// queue-poller-mounts: navigating into Downloads via the boosted
// mobile-tabbar must start the queue poller. The page-lifecycle
// helper's `mount` should set `window.__downloadsQueuePoller` to a
// non-null interval handle.
//
// Without `ryokanRegisterPageInit` (the legacy module-scope
// `setInterval` shape), boost-swaps wouldn't re-trigger the init
// block and the poller would never start on the second-or-later
// visit. This test fails LOUDLY in that case.
test('boosted_nav_into_downloads_starts_queue_poller', async ({ page }) => {
  await setMobileViewport(page);
  await page.goto('/');
  // The Rust original computed this and threw the result away
  // (`let _ =`). Enforced here.
  await assertHtmxLoaded(page);

  // Sanity: on Library, the Downloads-page poller handle is null.
  const initialTypeof = await withMessage(
    exec(page, 'return typeof window.__downloadsQueuePoller;'),
    'read pre-nav poller',
  );
  expect(
    initialTypeof === 'undefined' || initialTypeof === 'object',
    'before navigating to Downloads, the poller handle must be ' +
      `undefined OR null (=== object); got: ${JSON.stringify(initialTypeof)}`,
  ).toBe(true);

  // Boost-nav to Downloads. The lifecycle mount fires on htmx.onLoad
  // and starts setInterval(loadQueue, 5000).
  await withMessage(clickTabbarLink(page, '/downloads', '/downloads'), 'nav to downloads');

  // The mount runs in htmx.onLoad after the boosted swap settles. Poll
  // for the marker rather than fixed-sleep: settle timing varies under
  // parallel test execution.
  await withMessage(
    waitForJsTruthy(page, 'window.__downloadsQueuePoller != null', 5000),
    'after boosted nav into /downloads, window.__downloadsQueuePoller ' +
      'must be a live interval handle; got null/undefined, meaning the ' +
      "lifecycle mount didn't fire (or page_lifecycle.js didn't load)",
  );
});

// queue-poller-unmounts: leaving Downloads via boosted nav must stop
// the poller. Without this, ping-pong nav (Library -> Downloads ->
// Library -> Downloads -> ...) accumulates a new interval on every
// Downloads landing, none of which ever clear. Memory leak, and the
// polling rate compounds, hammering `/api/torrents` faster on every
// visit.
test('boosted_nav_out_of_downloads_clears_queue_poller', async ({ page }) => {
  await setMobileViewport(page);
  await page.goto('/downloads');
  // Enforced; the Rust original threw this result away.
  await assertHtmxLoaded(page);

  // Wait for downloads.js's defer execution to finish (signaled by the
  // immediate-reconcile setting __downloadsQueuePoller).
  await withMessage(
    waitForJsTruthy(page, 'window.__downloadsQueuePoller != null', 5000),
    'poller should be live on direct /downloads load',
  );

  // Boost-nav to Library. The unmount fires (Downloads-queue `check()`
  // returns falsy after the swap), clearing the interval and nulling
  // the handle.
  await withMessage(clickTabbarLink(page, '/', '/'), 'nav to library');

  // Poll for the unmount completion. Same shape as the mount-wait in
  // the previous test: converges fast and gives a clear timeout error
  // on a real leak.
  await withMessage(
    waitForJsTruthy(page, 'window.__downloadsQueuePoller === null', 5000),
    'after navigating away from /downloads, the queue poller ' +
      'handle must be null (cleared by lifecycle unmount); got ' +
      'non-null value, meaning the interval is still firing in ' +
      'the background: leak',
  );
});

// no-duplicate-pollers-after-pingpong: the most direct guard against
// the leak pattern. Navigate Downloads -> Library -> Downloads ->
// Library -> Downloads. After three Downloads landings the poller
// handle should be exactly one live interval, not three stacked. We
// assert the first two visits cleanly cleared their handles and only
// the third's handle is alive.
test('pingpong_nav_does_not_stack_pollers', async ({ page }) => {
  await setMobileViewport(page);
  await page.goto('/');
  // Enforced; the Rust original threw this result away.
  await assertHtmxLoaded(page);

  // Instrument both `setInterval` and `clearInterval` for the
  // `loadQueue` callback. The exact CALL count varies with how the
  // lifecycle module orchestrates mount/remount under head-support's
  // script re-execution semantics; what matters is the NET active
  // count = (setInterval calls) - (clearInterval calls on those
  // handles). For a healthy mount/unmount cycle the net at any moment
  // is 0 or 1, never more. That's the leak invariant.
  await withMessage(
    exec(
      page,
      `window.__loadQueueIntervalCalls__ = 0;
       window.__loadQueueClearedHandles__ = new Set();
       window.__loadQueueOpenedHandles__ = new Set();
       const origSet = window.setInterval;
       const origClear = window.clearInterval;
       window.setInterval = function (fn, ms) {
           const h = origSet(fn, ms);
           if (fn === window.loadQueue) {
               window.__loadQueueIntervalCalls__++;
               window.__loadQueueOpenedHandles__.add(h);
           }
           return h;
       };
       window.clearInterval = function (h) {
           if (window.__loadQueueOpenedHandles__.has(h)) {
               window.__loadQueueClearedHandles__.add(h);
           }
           return origClear(h);
       };`,
    ),
    'instrument set/clearInterval',
  );

  // 3 round trips + 1 final landing on /downloads
  for (let i = 0; i < 3; i++) {
    await withMessage(clickTabbarLink(page, '/downloads', '/downloads'), 'nav to downloads');
    await withMessage(
      waitForJsTruthy(page, 'window.__downloadsQueuePoller != null', 3000),
      'poller mounted',
    );
    await withMessage(clickTabbarLink(page, '/', '/'), 'nav to library');
    await withMessage(
      waitForJsTruthy(page, 'window.__downloadsQueuePoller === null', 3000),
      'poller cleared',
    );
  }
  await withMessage(clickTabbarLink(page, '/downloads', '/downloads'), 'final nav to downloads');
  await withMessage(
    waitForJsTruthy(page, 'window.__downloadsQueuePoller != null', 3000),
    'final poller mounted',
  );

  // The leak invariant: at this moment one poller is active.
  // Net live = openedSize - clearedSize. Must be 1.
  const net = await withMessage(
    exec(
      page,
      `return window.__loadQueueOpenedHandles__.size -
              window.__loadQueueClearedHandles__.size;`,
    ),
    'read net',
  );
  const n = Number.isInteger(net) ? net : -1;
  expect(
    n,
    'after 4 ping-pong navs ending on /downloads, exactly one ' +
      `loadQueue interval should be alive; got net=${n} live ` +
      "handles. A net > 1 means the unmount didn't fire on " +
      'every leave (memory leak). A net < 1 means the mount ' +
      "didn't fire on the final entry.",
  ).toBe(1);

  // And the live handle must be the current __downloadsQueuePoller.
  const activeMatches = await withMessage(
    exec(
      page,
      `return window.__loadQueueOpenedHandles__.has(window.__downloadsQueuePoller) &&
              !window.__loadQueueClearedHandles__.has(window.__downloadsQueuePoller);`,
    ),
    'compare active',
  );
  expect(
    activeMatches,
    'the currently-active __downloadsQueuePoller must be tracked ' +
      'as opened-but-not-cleared in our instrumentation',
  ).toBe(true);
});

// timestamp-rebind: `[data-ts]` nodes that arrive in a boosted swap are
// re-rendered by the page on that same swap. Before Phase B the refresh
// ran only on DOMContentLoaded and a 30s interval, so a fresh node kept
// whatever text it arrived with for up to 30s. Phase B hooks the
// refresh into `htmx.onLoad`.
//
// Marker: the Downloads History tab renders a `<span data-ts="...">`
// for each grab, and one grab is seeded. The server renders that span
// already humanized, with its own clock, so its text alone cannot show
// whether the page's refresh ran (the Rust version of this test checked
// only for "ago" / "now" and passed with the refresh disabled). So the
// two clocks are made to disagree: the browser is told it is three days
// later. The server writes "just now"; only the page's own refresh can
// turn that into "3d ago".
test('data_ts_rebinds_after_boosted_nav', async ({ app, page }) => {
  // Seed one grab so the Downloads history tab has a [data-ts] node to
  // assert against.
  const seriesId = await app.seed('series', { anilist_id: 1, title: 'Test Show' });
  await withMessage(
    app.seed('grab', {
      hash: 'abc123def456abc123def456abc123def4567890',
      name: '[Group] Test Show - 01.mkv',
      series_id: seriesId,
      episodes: [1],
      batch: false,
    }),
    'seed grab',
  );

  // `Date.now()` in the page is pinned three days ahead. Timers keep
  // running, so htmx and the queue poller behave as usual.
  const threeDaysMs = 3 * 24 * 60 * 60 * 1000;
  await page.clock.setFixedTime(Date.now() + threeDaysMs);

  await setMobileViewport(page);
  await page.goto('/');
  // Enforced; the Rust original threw this result away.
  await assertHtmxLoaded(page);

  // A window-scoped marker survives boosted swaps and dies with a full
  // document load. A full load would run the refresh from
  // DOMContentLoaded, which is not the path under test.
  await exec(page, "window.__dataTsBoostMarker = 'planted';");

  // Downloads through the boosted tab bar (the queue tab, which renders
  // no history rows), then the History tab link on that page.
  await withMessage(clickTabbarLink(page, '/downloads', '/downloads'), 'nav to downloads');

  const historyLink = page.locator('a[href="/downloads?tab=history"]').first();
  await withMessage(historyLink.waitFor({ state: 'attached', timeout: 5000 }), 'history tab link');
  await withMessage(historyLink.click(), 'click history');
  await withMessage(waitForPath(page, '/downloads', 5000), 'url updated');
  // Wait for the history table to render.
  await withMessage(
    page.locator('[data-ts]').first().waitFor({ state: 'attached', timeout: 5000 }),
    'data-ts span present',
  );

  const marker = await exec(page, 'return typeof window.__dataTsBoostMarker;');
  expect(
    marker,
    'both navigations must be boosted swaps: a full document load refreshes ' +
      'timestamps from DOMContentLoaded and would hide a broken htmx.onLoad hook',
  ).toBe('string');

  const readText = () =>
    exec(
      page,
      `const el = document.querySelector('[data-ts]');
       return el ? el.textContent.trim() : '';`,
    );
  await expect
    .poll(readText, {
      timeout: 3000,
      intervals: [50],
      message:
        '[data-ts] must be re-rendered by the page after a boosted nav: the server wrote ' +
        'its own "just now", and with the browser clock three days ahead the refresh in ' +
        "base.js turns it into \"3d ago\". Anything else means the `htmx.onLoad` hook didn't run it",
    })
    .toBe('3d ago');
});
