// The basic htmx loop, end to end in a real browser:
//
//   - the static htmx script loads from `/static/vendor/`
//   - `hx-vals` form-encoding round-trips to the `Form` extractor
//   - the handler's HTML response swaps into the DOM at `hx-target`
//     with `hx-swap=outerHTML`
//
// Handler unit tests check each piece alone. A regression in any of
// script-loaded / form-encoding / response-shape only shows when the
// three meet in a browser.

import { test, expect, exec, fixtureErrors } from './harness.js';

test('episode_monitor_button_swaps_in_browser', async ({ app, page }) => {
  // A series with one monitored episode. The fixture page renders the
  // partial directly, so the test isolates the swap from page concerns.
  const seriesId = await app.seed('series', { anilist_id: 12345, title: 'BrowserTest Anime' });
  await app.seed('episode_monitored', { series_id: seriesId, episode: 1, monitored: true });

  await page.goto(`/__test/episode-monitor-fixture?series_id=${seriesId}&episode_number=1`);

  // Before the click the button reads "Yes": monitored = true was seeded.
  const button = page.locator('button.ep-mon-btn');
  expect((await button.innerText()).trim(), 'fixture should render the seeded monitored=true state').toBe('Yes');
  expect(
    await button.getAttribute('class'),
    'pre-click button should carry ep-mon-yes class',
  ).toContain('ep-mon-yes');

  // Click. htmx posts, the handler returns a fresh button partial with
  // monitored = false, and outerHTML replaces the element.
  await button.click();

  await expect(async () => {
    const swapped = page.locator('button.ep-mon-btn');
    expect((await swapped.innerText()).trim()).toBe('No');
    expect(await swapped.getAttribute('class')).toContain('ep-mon-no');
  }, 'button did not swap to monitored=false within 3s').toPass({ timeout: 3000 });

  // The handler renders the partial from the request's `monitored`
  // value, so a handler that skipped the write would still return a
  // "monitored=false" button. Read the row back.
  const rows = await app.sql(
    'SELECT monitored FROM episode_monitor_state WHERE series_id = ? AND episode_number = ?',
    seriesId,
    1,
  );
  expect(rows, 'DB row must reflect the post-click state').toEqual([{ monitored: 0 }]);
});

// The two `hx-on::` shapes the series page relies on, under htmx 4's
// colon-separated event names: a rejected POST reverts the checkbox
// through `hx-on::response:error`, and `hx-on::after:request` reads the
// outcome from `event.detail.ctx.response.status` (htmx 4 has no
// `detail.successful`). Under htmx 2 spellings these attributes bind to
// events that never fire.
test('hx_on_handlers_fire_under_htmx_4_event_names', async ({ page }) => {
  await page.goto('/__test/hx-on-fixture');

  // Rejected POST: the change handler flips the box, the server says
  // 400, the response:error handler flips it back.
  await page.locator('#hx-on-toggle').click();
  let status = null;
  for (const deadline = Date.now() + 3000; ; ) {
    status = await exec(page, 'return window.__hxOnErrorStatus || null;');
    if (status === 400 || Date.now() > deadline) break;
    await page.waitForTimeout(50);
  }
  expect(
    status,
    'hx-on::response:error never fired for the rejected POST; fixture errors: ' +
      JSON.stringify(await fixtureErrors(page)),
  ).toBe(400);

  expect(
    await page.locator('#hx-on-toggle').evaluate((el) => el.checked),
    'the response:error handler must revert the checkbox to its server-side state',
  ).toBe(true);

  // Accepted POST: after:request sees the 200.
  await page.locator('#hx-on-accept').click();
  await expect
    .poll(() => exec(page, 'return window.__hxOnAfterRequestOk === true;'), {
      timeout: 3000,
      message: 'hx-on::after:request never saw a < 400 status for the accepted POST',
    })
    .toBe(true);
});
