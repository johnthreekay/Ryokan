// Phase 1.5 grab-bag (issue #129) browser tests.
//
// Two patterns under test:
//
// - **Row removal** (blocklist-remove): same shape as the Phase 1
//   settings deletes: confirm modal, click Yes, row stripped. Drives the
//   real `/downloads?tab=blocklist` page so any drift between the row
//   template and the production handler shows up.
// - **Inline-result swap** (jellyfin/test, jellyfin/refresh,
//   download-clients/test): button click, htmx posts the form, the
//   handler returns a small HTML fragment, which swaps into the result
//   span. Drives a fixture page in `test_support::e2e` that mirrors the
//   production button shape (see the fixture for the exact
//   `hx-include` / `hx-target` attrs).

import {
  test,
  expect,
  exec,
  assertDomContains,
  assertHtmxHandledInPlace,
  waitForConfirmModal,
  waitForJsTruthy,
  waitForRowRemoved,
  waitUntilSubstring,
} from './harness.js';

test('blocklist_remove_swaps_row_out_in_browser', async ({ app, page }) => {
  // Seed two blocklist rows. The test deletes the first and asserts:
  //   (a) the deleted row is gone from the DOM
  //   (b) the surviving row is STILL in the DOM
  //   (c) the surviving DB row is untouched
  // Without (b), a stray `hx-target="closest div"` (which would swap
  // the entire containing div) passes silently: caught during a
  // mutation-testing pass while writing the e2e suite.
  const seriesId = await app.seed('series', { anilist_id: 9001, title: 'BrowserBlock Anime' });
  await app.sql(
    'INSERT INTO grabbed_torrents (series_id, hash, torrent_name, episode_numbers, state) ' +
      "VALUES (?, ?, ?, '[1]', 'failed'), (?, ?, ?, '[2]', 'failed')",
    seriesId,
    'blockhash-1',
    'BrowserBlock.Doomed.Release',
    seriesId,
    'blockhash-2',
    'BrowserBlock.Survivor.Release',
  );

  await page.goto('/downloads?tab=blocklist');

  // Click the row's delete button. The form sits inside the row; we
  // identify it by the unique torrent name in the surrounding tr.
  await exec(
    page,
    `
                const marker = 'BrowserBlock.Doomed.Release';
                const tr = Array.from(document.querySelectorAll('tr'))
                    .find(r => r.textContent.includes(marker));
                if (!tr) throw new Error('blocklist row not found');
                const btn = tr.querySelector('button[type="submit"]');
                if (!btn) throw new Error('row has no delete button');
                btn.click();
                `,
  );

  await waitForConfirmModal(page, 2000);
  await page.locator('#ryokan-confirm-yes').click();

  await waitForRowRemoved(page, 'BrowserBlock.Doomed.Release', 3000);
  // Survivor row stays: guard against an over-broad swap target.
  await assertDomContains(page, 'BrowserBlock.Survivor.Release');
  await assertHtmxHandledInPlace(page, `${app.baseURL}/downloads`);

  // DB-side sanity.
  const remaining = await app.sql(
    'SELECT COUNT(*) AS n FROM grabbed_torrents WHERE hash = ?',
    'blockhash-1',
  );
  expect(remaining[0].n, 'row must actually be gone from the DB').toBe(0);
});

test('jellyfin_test_renders_failure_message_in_browser', async ({ page }) => {
  // Drives the connection-test fixture: bogus URL, the handler returns
  // the failure partial, htmx swaps it into #jellyfin-test-result.
  // Match on "127.0.0.1" specifically (the URL the fixture form sends):
  // a generic "any text present" check would miss a "rendered the
  // wrong template" regression.
  await page.goto('/__test/connection-test-fixture');
  await page.locator('#btn-jellyfin-test').click();
  await waitUntilSubstring(page, '#jellyfin-test-result', '127.0.0.1', 5000);
});

test('jellyfin_refresh_renders_not_configured_in_browser', async ({ page }) => {
  await page.goto('/__test/connection-test-fixture');
  await page.locator('#btn-jellyfin-refresh').click();
  await waitUntilSubstring(page, '#jellyfin-test-result', 'No media server connected', 3000);
});

// A queue item id that ends an inline `onclick` JS string. HTML
// escaping doesn't stop it there: the parser decodes `&#39;` back to
// `'` before the handler runs. The fixture host's fake download client
// (`QueueClient` in `host.rs`) serves one downloading item under this
// id and records the pauses it is asked for.
const HOSTILE_HASH = "x');window.__pwned=1;('";

test.describe(() => {
  test.use({ appOptions: { queueClient: true } });

  test('queue_row_actions_send_the_hash_as_data_not_code', async ({ app, page }) => {
    // The queue's buttons used to build `onclick="pauseTorrent('<id>')"`
    // from the client's id, so an id with a quote ran as script. Both
    // renderers (the server's first paint and renderQueue's poll) are
    // clicked, and each must post the id verbatim.
    await page.goto('/downloads');
    const pause = '#queue-container button[title="Pause"]';
    for (let round = 1; round <= 2; round++) {
      if (round === 2) {
        // Re-render through renderQueue, whose name cell has no title
        // attribute, and click its button.
        await exec(page, 'loadQueue();');
        await waitForJsTruthy(
          page,
          "!document.querySelector('#queue-container .dl-torrent-name').hasAttribute('title')",
          5000,
        );
      }
      await page.locator(pause).first().click();
      await expect
        .poll(async () => (await app.pauses()).length, {
          timeout: 5000,
          intervals: [50],
          message: `round ${round}: no pause reached the client`,
        })
        .toBeGreaterThanOrEqual(round);
    }
    const pwned = await exec(page, 'return typeof window.__pwned;');
    expect(pwned, `the id ran as script: ${JSON.stringify(pwned)}`).toBe('undefined');

    expect(await app.pauses()).toEqual([HOSTILE_HASH, HOSTILE_HASH]);
  });
});

test('download_clients_test_renders_url_required_in_browser', async ({ page }) => {
  // The DC form fixture has an empty URL field; the handler should
  // surface "URL required": an easy assertion that doesn't depend on a
  // real qBit-style daemon.
  await page.goto('/__test/connection-test-fixture');
  await page.locator('#btn-dc-test').click();
  await waitUntilSubstring(page, '.dc-test-result', 'URL required', 3000);
});
