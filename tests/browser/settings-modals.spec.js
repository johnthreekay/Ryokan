// The indexer Edit-card -> modal flow. That flow is JS-driven
// (`openIndexerEditModal` calls `htmx.ajax`), so the browser layer adds
// value: a typo in the JS, a missing data attribute on the card, or htmx
// itself failing to load would all break this in production but pass at
// the handler layer.
//
// The two non-JS-interactive checks that were here (indexer Test
// endpoint HX-Trigger header + DC add-form data-first-* attributes)
// moved to `tests/htmx_settings_modal_handlers.rs` since they validate
// server-rendered output and don't benefit from a real browser. The
// browser variants were observably flaky against geckodriver (in-page
// `fetch` and `goto`-then-`source` both intermittently returned empty
// bodies under parallel test execution); the handler-direct variant is
// deterministic.

import { test, expect, exec, assertHtmxLoaded } from './harness.js';

// Clicking the Edit button on an indexer card fires an
// `hx-get="/settings/indexers/{id}/edit-form"` that swaps the modal
// body. The handler must return a populated form (Name, URL, API Key,
// Priority, etc.) bound to the row's existing values, not a blank
// shell. Pin the wire shape so a regex-replace refactor of the partial
// doesn't silently nullify the populated values.
test('indexer_edit_modal_loads_populated_form', async ({ app, page }) => {
  const indexerId = await app.seed('indexer', { name: 'Test Indexer Alpha', api_key: 'k' });

  await page.goto('/settings?tab=indexers');
  // The Rust original computed this and threw the result away
  // (`let _ =`). Enforced here: the card click below goes through
  // `htmx.ajax`, so the test cannot pass without htmx anyway.
  await assertHtmxLoaded(page);

  // Click the indexer card body: the whole card is a click target
  // (`role="button"` on the body div with `data-indexer-id`), and
  // `openIndexerEditModal` fetches the edit-form partial via `htmx.ajax`
  // into `#indexer-modal-body`.
  const cardBody = page.locator(`[data-indexer-id="${indexerId}"]`).first();
  await cardBody.waitFor({ state: 'attached', timeout: 5000 });
  await cardBody.click();

  // Modal body should populate with the indexer's name pre-filled.
  const nameInput = page.locator('#indexer-modal-body input[name="name"]').first();
  await nameInput.waitFor({ state: 'attached', timeout: 5000 });
  const nameValue = await nameInput.evaluate((el) => el.value);
  expect(
    nameValue,
    'edit modal must populate Name from the indexer row, not render blank',
  ).toBe('Test Indexer Alpha');
});

// A stale-id edit-form fetch must render the inline modal-error
// partial, NOT silently leave the modal body unchanged. Pins the
// browser-side observable behavior of the "always-200 with error
// rendered inline" pattern shipped in PR `0b1757c`.
//
// Why browser-driven: handler-level tests (in
// `tests/htmx_settings_modal_handlers.rs`) already pin the response
// wire shape (200 + error string in body). The browser test adds value
// by verifying that:
//   1. the JS-driven `htmx.ajax()` call from `openIndexerEditModal`
//      actually swaps the response into `#indexer-modal-body` (htmx 2.x's
//      default policy skips the swap on non-2xx: a future regression that
//      puts the handler back to 404 would surface here as "modal body
//      unchanged" while the handler test passes if it checks only the
//      wire shape on 200).
//   2. the swap target id (`#indexer-modal-body`) actually exists in the
//      rendered settings page; a typo in either side would break the
//      test deterministically.
test('indexer_edit_modal_stale_id_renders_error_partial', async ({ app, page }) => {
  // Seed a real indexer so the page has something to render; the
  // stale-id modal trigger uses a hardcoded 99999 below.
  await app.seed('indexer', { name: 'Real Indexer', api_key: 'k' });

  await page.goto('/settings?tab=indexers');
  // Enforced, as above.
  await assertHtmxLoaded(page);

  // Bypass the card click and JS-trigger the edit-modal opener directly
  // with a known-stale id (99999). Functionally equivalent to the user
  // clicking Edit on a card that was deleted in another tab before their
  // click landed: the row no longer exists.
  const opened = await exec(
    page,
    `
            if (typeof window.openIndexerEditModal !== 'function') {
                return {ok:false, err:'openIndexerEditModal missing — settings.js not loaded'};
            }
            window.openIndexerEditModal(99999, 'Stale Indexer');
            return {ok:true};
            `,
  );
  expect(
    opened?.ok,
    `openIndexerEditModal entry-point must exist; got: ${JSON.stringify(opened)}`,
  ).toBe(true);

  // Wait for the modal body to populate with the error partial. The
  // error blurb's copy comes from the handler in
  // `src/handlers/settings/indexers.rs`; the first sentence is the
  // load-bearing user-visible string we pin.
  const modalBody = page.locator('#indexer-modal-body');
  await modalBody.waitFor({ state: 'attached', timeout: 5000 });
  // Poll the body's text until the error blurb arrives: the htmx.ajax()
  // round trip is async and the test must not race against it. The
  // original polled 20 times, 100 ms apart.
  await expect
    .poll(() => modalBody.innerText().catch(() => ''), {
      timeout: 2000,
      intervals: [100],
      message:
        'stale-id edit-form must render the inline error partial; modal body never showed ' +
        'the expected error string',
    })
    .toContain('no longer exists');
});
