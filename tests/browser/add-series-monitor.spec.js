// The library's add-series "Monitor Episodes" dialog.
//
// 65b545e changed `/api/library/monitoring` to answer htmx with a
// `ryokan-monitoring-changed` event instead of `HX-Refresh`, so the
// series page could update in place. The add flow's dialog relied on
// the refresh: Confirm saved the mode, but the dialog stayed open over
// a library that still read "0 series". The handler test can't see
// this; only a browser runs the button's `hx-on` wiring.

import { test, expect, assertHtmxLoaded, exec, withMessage } from './harness.js';

test('confirming_the_monitor_dialog_saves_and_closes_it', async ({ app, page }) => {
  const seriesId = await app.seed('series', { anilist_id: 154587, title: 'Frieren' });
  await app.sql("UPDATE series SET monitor_mode = 'none' WHERE id = ?", seriesId);

  await page.goto('/');
  await withMessage(assertHtmxLoaded(page), 'htmx loaded');
  // index.js is deferred; it declares `_pendingSeriesId = null`, so
  // setting it before the script ran would be overwritten.
  await expect
    .poll(
      () =>
        exec(page, "return typeof confirmMonitoringVals === 'function';").catch(() => false),
      { timeout: 5000, intervals: [100], message: 'index.js never loaded' },
    )
    .toBe(true);

  // The state `addSeries` leaves behind before it shows the dialog: the
  // new series' id pending, "All Episodes" selected. Set through an
  // injected <script>, which runs in the page: geckodriver ran
  // `execute` in a sandbox whose global assignments the page's own
  // functions never saw (Playwright's evaluate runs in the page, but
  // the script body is kept as it was). A marker on `window` survives
  // until the page reloads.
  await withMessage(
    exec(
      page,
      `
            const s = document.createElement('script');
            s.textContent = '_pendingSeriesId = ' + Number(arguments[0])
                + '; _selectedMonitorMode = "all";';
            document.head.appendChild(s);
            document.getElementById('monitor-modal').style.display = 'flex';
            window.__beforeConfirm = true;
            document.getElementById('monitor-confirm-btn').click();
            return true;
            `,
      seriesId,
    ),
    'confirm the dialog',
  );

  // A script error (a navigation tearing the document down, or the
  // modal not there yet) counts as "not done yet".
  await expect
    .poll(
      () =>
        exec(
          page,
          `
                return document.readyState === 'complete'
                    && window.__beforeConfirm === undefined
                    && getComputedStyle(document.getElementById('monitor-modal')).display === 'none';
                `,
        ).catch(() => false),
      {
        timeout: 5000,
        intervals: [100],
        message: 'Confirm must reload the library so the dialog closes and the new series shows',
      },
    )
    .toBe(true);

  const rows = await app.sql('SELECT monitor_mode FROM series WHERE id = ?', seriesId);
  expect(rows.length, 'read monitor mode').toBe(1);
  expect(rows[0].monitor_mode, 'the chosen mode is saved').toBe('all');
});
