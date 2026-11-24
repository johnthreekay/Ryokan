// Per-row deletes on the real `/settings` page (not a fixture).
//
// Each test seeds real rows, opens the tab, works the row's delete
// form, and asserts on the DOM afterwards. This is the layer where
// template-attribute drift, the `htmx:confirm` bridge in `base.js`,
// and the swap itself all matter at once.
//
//   - Indexers: confirm-then-delete removes the row.
//   - Indexers: cancel leaves the row alone (the "cancel silently
//     submitted" bug fixed by the htmx:confirm bridge).
//   - Download clients: deleting the default promotes the next row,
//     and the badge shows it after a reload.
//   - Custom formats: deleting the last one sends `HX-Refresh` so the
//     empty-state call to action renders.
//   - Groups: a direct delete with no confirm modal.

import {
  test,
  expect,
  exec,
  assertDomContains,
  assertHtmxHandledInPlace,
  assertHtmxLoaded,
  assertModalText,
  clickDeleteFor,
  waitForConfirmModal,
  waitForRowRemoved,
} from './harness.js';

test('indexers_delete_confirm_removes_row', async ({ app, page }) => {
  // Two indexers, so the test can also assert the survivor stays in
  // the DOM. Without that, an over-broad `hx-target` (`closest div`
  // swapping the whole grid) passes.
  await app.seed('indexer', { name: 'Phase1Test-IndexerA' });
  const survivor = await app.seed('indexer', { name: 'Phase1Test-IndexerSurvivor' });

  await page.goto('/settings?tab=indexers');
  // The card grid renders the delete control as `.btn-icon-danger`.
  await page.locator('button[type="submit"].btn-icon-danger').first().waitFor({ state: 'attached' });
  await clickDeleteFor(page, 'Phase1Test-IndexerA');
  await waitForConfirmModal(page, 2000);

  // The row form's `data-ryokan-confirm-*` attributes flow through
  // `ryokanConfirmFromAttrs` in base.js into the modal's title and
  // body. A wrong attribute name or modal element id would leave the
  // default copy ("Confirm" / "Are you sure?") there instead.
  await assertModalText(page, 'title', 'Delete indexer?');
  await assertModalText(page, 'body', 'Phase1Test-IndexerA');

  await page.locator('#ryokan-confirm-yes').click();

  await waitForRowRemoved(page, 'Phase1Test-IndexerA', 3000);
  await assertDomContains(page, 'Phase1Test-IndexerSurvivor');
  await assertHtmxHandledInPlace(page, `${app.baseURL}/settings?tab=indexers`);

  // The delete swapped `#indexer-section` (modal included) by
  // outerHTML. settings.js re-binds the fresh modal's backdrop-click
  // listener from `htmx:after:swap`, which htmx 4 dispatches on the
  // request's source element (already detached here), so the listener
  // has to find the swapped region through `ryokanSwapTargetId`, not
  // `ev.target`. Open the survivor's edit modal, click the backdrop,
  // expect it to close.
  await page.locator(`[data-indexer-id="${survivor}"]`).first().click();
  await page.locator('#indexer-modal-body input[name="name"]').first().waitFor({ state: 'attached', timeout: 5000 });
  await exec(page, "document.getElementById('indexer-modal').click();");
  await expect
    .poll(() => exec(page, "return document.getElementById('indexer-modal').style.display;"), {
      timeout: 2000,
      message:
        'backdrop click did not close the re-rendered indexer modal; ' +
        'the htmx:after:swap rebind for #indexer-section did not run',
    })
    .toBe('none');

  const remaining = await app.sql('SELECT name FROM indexers ORDER BY priority ASC, id ASC');
  expect(remaining, 'exactly one indexer (the survivor) must remain').toEqual([
    { name: 'Phase1Test-IndexerSurvivor' },
  ]);
});

test('indexers_delete_cancel_keeps_row', async ({ app, page }) => {
  // The "cancel modal silently submitted" bug: htmx's submit listener
  // fired before base.js's, so the request was already in flight when
  // `preventDefault()` ran. Fixed by the `htmx:confirm` event bridge.
  await app.seed('indexer', { name: 'Phase1Test-IndexerB' });

  await page.goto('/settings?tab=indexers');
  // htmx has to be loaded before the cancel path means anything:
  // without it there is no modal to cancel and the form submits
  // natively, deleting the row.
  await assertHtmxHandledInPlace(page, `${app.baseURL}/settings?tab=indexers`);

  await clickDeleteFor(page, 'Phase1Test-IndexerB');
  await waitForConfirmModal(page, 2000);
  await page.locator('#ryokan-confirm-no').click();

  // Give htmx a beat to misbehave if the bridge is broken.
  await page.waitForTimeout(300);

  const stillPresent = await exec(
    page,
    `return Array.from(document.querySelectorAll('tr, article'))
        .some(el => el.textContent.includes('Phase1Test-IndexerB'));`,
  );
  expect(stillPresent, 'cancel must leave the row in the DOM; modal-bridge regression?').toBe(true);

  const remaining = await app.sql('SELECT name FROM indexers');
  expect(remaining.length, 'cancel must NOT delete the indexer row').toBe(1);
});

test('download_clients_delete_default_auto_promotes_next', async ({ app, page }) => {
  await app.seed('download_client', { name: 'Phase1Test-DcA', is_default: true });
  await app.seed('download_client', { name: 'Phase1Test-DcB', is_default: false });

  await page.goto('/settings?tab=downloads');
  await clickDeleteFor(page, 'Phase1Test-DcA');
  await waitForConfirmModal(page, 2000);
  await page.locator('#ryokan-confirm-yes').click();

  await waitForRowRemoved(page, 'Phase1Test-DcA', 3000);
  await assertDomContains(page, 'Phase1Test-DcB');
  await assertHtmxHandledInPlace(page, `${app.baseURL}/settings?tab=downloads`);

  // Reload to re-render the default badges: the card swap alone does
  // not repaint sibling cards, so the promotion shows in the DOM only
  // after a fresh page load.
  await page.goto('/settings?tab=downloads');

  // The DB check below confirms B's `is_default = 1`, but a template
  // that stopped rendering the badge would pass it and still break the
  // UI. The picker is `<article class="dc-card">`.
  const badgeOnSurvivor = await exec(
    page,
    `const card = Array.from(document.querySelectorAll('article.dc-card'))
        .find(c => c.textContent.includes('Phase1Test-DcB'));
    if (!card) return false;
    return card.textContent.toLowerCase().includes('default');`,
  );
  expect(
    badgeOnSurvivor,
    'surviving DC card does not show the `default` badge after auto-promote: ' +
      "template render path didn't reflect the DB change?",
  ).toBe(true);

  const surviving = await app.sql('SELECT name, is_default FROM download_clients');
  expect(
    surviving,
    'deleted A should leave only B, auto-promoted to default',
  ).toEqual([{ name: 'Phase1Test-DcB', is_default: 1 }]);
});

test('custom_formats_delete_last_triggers_hx_refresh', async ({ app, page }) => {
  // Deleting the only Custom Format should send `HX-Refresh: true` so
  // the empty-state call to action ("Install bundled defaults")
  // renders. A per-row swap would leave an empty <tbody> with none.
  await app.seed('custom_format', { name: 'Phase1Test-CfSolo' });

  await page.goto('/settings?tab=custom_formats');
  // HX-Refresh navigates on purpose, so the stay-in-place URL check
  // does not apply; just check htmx is loaded.
  await assertHtmxLoaded(page);

  await clickDeleteFor(page, 'Phase1Test-CfSolo');
  await waitForConfirmModal(page, 2000);

  // The per-CF delete form once used `data-ryokan-confirm-label`,
  // which `ryokanConfirmFromAttrs` ignores in favor of
  // `data-ryokan-confirm-yes`, so the button read the default "Yes".
  await assertModalText(page, 'yes', 'Delete');

  await page.locator('#ryokan-confirm-yes').click();

  await waitForRowRemoved(page, 'Phase1Test-CfSolo', 5000);
  await expect
    .poll(
      async () => {
        try {
          return await exec(
            page,
            "return document.body.textContent.toLowerCase().includes('install bundled defaults');",
          );
        } catch {
          // The refresh is tearing the old document down.
          return false;
        }
      },
      { timeout: 3000, message: 'empty-state CTA did not appear after HX-Refresh' },
    )
    .toBe(true);
});

test('groups_delete_removes_row_directly', async ({ app, page }) => {
  // Groups have no confirm modal on the row's delete form on purpose
  // (the row is cheap to recreate). Click, htmx posts, the row swap
  // removes the row.
  await app.seed('group', { name: 'Phase1Test-GroupDoomed' });
  await app.seed('group', { name: 'Phase1Test-GroupSurvivor' });

  await page.goto('/settings?tab=groups');
  await clickDeleteFor(page, 'Phase1Test-GroupDoomed');
  await waitForRowRemoved(page, 'Phase1Test-GroupDoomed', 3000);
  await assertDomContains(page, 'Phase1Test-GroupSurvivor');
  await assertHtmxHandledInPlace(page, `${app.baseURL}/settings?tab=groups`);

  const doomed = await app.sql(
    'SELECT group_name FROM group_source_map WHERE group_name = ?',
    'Phase1Test-GroupDoomed',
  );
  expect(doomed, 'doomed group must be removed from DB').toEqual([]);
  const survivor = await app.sql(
    'SELECT group_name FROM group_source_map WHERE group_name = ?',
    'Phase1Test-GroupSurvivor',
  );
  expect(survivor.length, 'survivor group must still be in DB').toBe(1);
});
