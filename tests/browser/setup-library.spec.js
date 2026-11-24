// The first-run library step at `/setup/library`.
//
// The page is a boosted form plus the Integrations tab's htmx Jellyfin
// Test button, so only a browser shows that Save lands on the library
// (via `HX-Redirect`, not a nested render), that Test answers in place,
// and that Skip leaves the settings alone.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test, expect, assertHtmxLoaded, exec, waitForPath, withMessage } from './harness.js';

// `Config::default()` saved before the browser opens, as a fresh install
// has it, so a config row exists from the start and Skip is checked
// against that row rather than against no row at all.
async function freshState(app) {
  await app.seed('config_default');
}

/** `config::get_config(&db).await.unwrap().unwrap()`, the fields these tests read. */
async function getConfig(app) {
  const rows = await app.sql(
    'SELECT media_root, post_processing_enabled, post_processing_mode FROM config WHERE id = 1',
  );
  expect(rows.length, 'get_config returned no config row').toBe(1);
  return rows[0];
}

test('saving_the_library_step_lands_on_the_library_with_post_processing_on', async ({
  app,
  page,
}) => {
  await freshState(app);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ryokan-e2e-'));
  try {
    const rootPath = root;

    await page.goto('/setup/library');
    await withMessage(assertHtmxLoaded(page), 'htmx loaded');

    // The Jellyfin Test answers in place, from this page's form.
    await exec(
      page,
      `
            document.getElementById('jellyfin_url').value = 'http://127.0.0.1:1';
            document.getElementById('jellyfin_api_key').value = 'some-key';
            `,
    );
    await page.locator("xpath=//button[normalize-space()='Test']").first().click();
    // The Rust loop read the result up to 100 times, 100ms apart.
    await expect
      .poll(
        () =>
          exec(
            page,
            "return document.getElementById('jellyfin-test-result').textContent.trim();",
          ).then(
            (v) => (typeof v === 'string' ? v : ''),
            () => '',
          ),
        {
          timeout: 10_000,
          intervals: [100],
          message: 'the Test button must render its result in place',
        },
      )
      .not.toBe('');
    expect(new URL(page.url()).pathname, 'Test stays on the page').toBe('/setup/library');

    // Save without Jellyfin.
    await exec(
      page,
      `
            document.getElementById('jellyfin_url').value = '';
            document.getElementById('jellyfin_api_key').value = '';
            document.getElementById('media_root').value = arguments[0];
            document.getElementById('post_processing_mode').value = 'copy';
            `,
      rootPath,
    );
    await page.locator("xpath=//button[normalize-space()='Save and continue']").first().click();
    await withMessage(waitForPath(page, '/', 5000), 'Save lands on the library');
    // A real navigation, not the library nested inside the setup page.
    const nested = await exec(page, "return document.querySelectorAll('.setup-library').length;");
    expect(nested, 'the setup page is gone').toBe(0);

    const cfg = await getConfig(app);
    expect(cfg.media_root).toBe(rootPath);
    expect(cfg.post_processing_enabled).toBe(1);
    expect(cfg.post_processing_mode).toBe('copy');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('skipping_the_library_step_changes_nothing', async ({ app, page }) => {
  await freshState(app);

  await page.goto('/setup/library');
  await withMessage(assertHtmxLoaded(page), 'htmx loaded');
  await exec(page, "document.getElementById('media_root').value = '/somewhere';");
  await page.getByRole('link', { name: 'Skip for now', exact: true }).click();
  await withMessage(waitForPath(page, '/', 5000), 'Skip lands on the library');

  const cfg = await getConfig(app);
  expect(cfg.media_root).toBe('');
  expect(cfg.post_processing_enabled).toBe(0);
});
