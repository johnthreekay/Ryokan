// Browser e2e for the per-episode delete HX-Trigger listener shipped
// in PR `ac19049` (Phase 2 migration).
//
// Per `feedback_migration_discipline`: HTMX migrations need a browser
// e2e because handler-level tests can't observe the JS-side
// event-listener wiring. The fixture isolates the most failure-prone
// link in the per-episode delete flow under hx-boost, the
// `ryokan-episode-deleted` listener, and verifies it actually fires
// `updateEpisodeRow` to flip the row class.
//
// Failure modes this catches:
//   - Singleton guard regression (`window.__ryokanSeriesListeners`
//     stops gating, listener never re-attaches after boost re-execute).
//   - HX-Trigger payload shape change (event name / detail keys that
//     the listener doesn't recognize).
//   - `updateEpisodeRow` being renamed / removed / stripped of its
//     `state === 'deleted'` branch.
//
// The full delete *flow* (modal click, confirm bridge, POST,
// HX-Trigger header, swap) has handler-level coverage in
// `src/handlers/library/episodes.rs::tests::episodes_ci`. This fixture
// covers the JS-only middle leg that handler tests can't reach.

import { test, expect, exec } from './harness.js';

// Dispatching the `ryokan-episode-deleted` CustomEvent must trigger
// `updateEpisodeRow` to flip the row's class. This is the contract the
// singleton-guarded listener in `series.js` is supposed to keep wired
// across hx-boost re-executions.
test('ryokan_episode_deleted_event_flips_row_to_missing_state', async ({ page }) => {
  await page.goto('/__test/episode-delete-listener-fixture');

  // Confirm the precondition: the row exists with `ep-row-have`
  // class. If this assertion fails, the fixture page wasn't loaded
  // correctly and the post-event assertion would be ambiguous.
  const row = page.locator("tr[data-test-id='row-5']").first();
  await row.waitFor({ state: 'attached', timeout: 5000 });
  // A failed read counts as an empty class, as in the Rust test.
  const preClass = (await row.getAttribute('class').catch(() => null)) ?? '';
  expect(
    preClass,
    `precondition: row must start in ep-row-have state; got class='${preClass}'`,
  ).toContain('ep-row-have');

  // Wait for series.js to load + register its listener. The script is
  // `<script src=...>` (synchronous) so by the time the row is
  // queryable, the listener is attached. Belt-and-suspenders: poll
  // briefly until `window.__ryokanSeriesListeners` is set. (The Rust
  // test polled 30 times, 50ms apart; a failed read counted as false.)
  await expect
    .poll(() => exec(page, 'return !!window.__ryokanSeriesListeners;').catch(() => false), {
      timeout: 1500,
      intervals: [50],
      message:
        'series.js must register the singleton-guarded HX-Trigger listener; ' +
        '`window.__ryokanSeriesListeners` never became truthy. Likely a series.js ' +
        'load failure or the singleton-guard refactor lost its set-on-first-run.',
    })
    .toBe(true);

  // Dispatch the synthetic event. The handler emits this exact shape
  // from `episode_delete_trigger` in `src/handlers/library/episodes.rs`;
  // the listener parses `detail.episode_number` to identify the row.
  await exec(
    page,
    `
    const ev = new CustomEvent('ryokan-episode-deleted', {
        detail: {
            ok: true,
            episode_number: 5,
            message: 'Episode 5 file removed.',
        }
    });
    document.body.dispatchEvent(ev);
    return true;
    `,
  );

  // Poll for the class flip. The listener calls
  // `updateEpisodeRow(5, 'deleted')` synchronously; the only async
  // tail is `refreshEpisodeRows({force: true})`, which fails silently
  // against the fixture's stub `series-data` element (no `dbId`).
  // That's intentional: it doesn't affect the synchronous classList
  // mutation. (The Rust test polled 20 times, 100ms apart; a failed
  // read counted as an empty class.)
  await expect
    .poll(
      async () => {
        const postClass = (await row.getAttribute('class').catch(() => null)) ?? '';
        return postClass.includes('ep-row-missing') && !postClass.includes('ep-row-have');
      },
      {
        timeout: 2000,
        intervals: [100],
        message:
          'row must flip from `ep-row-have` to `ep-row-missing` after the HX-Trigger event; ' +
          "indicates the listener didn't fire OR `updateEpisodeRow` lost its 'deleted' branch",
      },
    )
    .toBe(true);

  // The class flips first, so the check above passed while the rest of
  // `updateEpisodeRow` threw: 65b545e dropped `STATUS_ICON_MISSING` and
  // every live row update died with a ReferenceError, leaving the ✓
  // icon and the old status text (and, after an import, a row stuck on
  // "Importing…"). The status cell and the page's error log pin the
  // whole function.
  const result = await exec(
    page,
    `
    const row = document.querySelector('[data-test-id="row-5"]');
    return [
        !!row.querySelector('.ep-col-status .ep-status-icon.ep-missing'),
        row.querySelector('.ep-col-quality').textContent.trim(),
        (window.__ryokanFixtureErrors || []).join(' | '),
    ];
    `,
  );
  const checks = Array.isArray(result) ? result : [];
  expect(checks[0], 'the status cell must show the missing icon').toBe(true);
  expect(checks[1], 'the status text must read Missing').toBe('Missing');
  expect(checks[2], 'the page must not throw while updating the row').toBe('');
});
