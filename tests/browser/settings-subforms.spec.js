// Phase 1 completion (per-tab settings subforms).
//
// Each Phase 1 split (General / Quality / Integrations) replaced the
// bulk `<form action="/settings">` that posted ALL three tabs' fields
// together with a per-tab `<form hx-post="/settings/<tab>">` that posts
// only its own tab's fields. The split's load-bearing invariants:
//
//   1. Persistence: saving from a tab's subform writes that tab's
//      fields to `config`.
//   2. In-place swap: htmx swaps the form region without a full page
//      navigation; the URL stays at `/settings?tab=<tab>` and the
//      "Settings saved." toast / alert renders in the swapped region.
//   3. Cross-tab isolation: saving from one tab doesn't clobber the
//      other two tabs' fields. This is the property the per-tab split
//      was supposed to give us; the legacy bulk form would submit empty
//      defaults for any field whose tab panel wasn't currently active,
//      which silently re-saved with empty values.
//
// Each test seeds a config row with distinct values across all three
// tabs, navigates to one tab, mutates one field, submits, and asserts
// all three properties.

import {
  test,
  expect,
  exec,
  assertHtmxHandledInPlace,
  assertHtmxLoaded,
  waitUntilSubstring,
} from './harness.js';

// ─── Seeding ────────────────────────────────────────────────────────

// `app.seed('config_distinct')` writes a `config` row with distinct,
// non-default values for each tab's fields (`distinct_config()` in
// host.rs), so the post-save assertions can tell which fields changed
// and which got carried through. The choices are arbitrary but each one
// is different from the form-default empty / coerced value, so a
// regression that re-saves with the form's default (rather than the
// existing) value flips the field and the test fails loudly.

// `get_config` reads the row with `id = 1`; so does this. Booleans and
// integers come back as numbers.
async function getConfig(app) {
  const rows = await app.sql('SELECT * FROM config WHERE id = 1');
  expect(rows.length, 'config row').toBe(1);
  return rows[0];
}

/** Navigate to /settings?tab=<name>. */
async function openTab(page, tab) {
  await page.goto(`/settings?tab=${tab}`);
}

/**
 * Submit the visible subform's first `button[type="submit"]` and wait
 * for the "Settings saved." alert to render in the swapped region. This
 * captures the swap-completion signal: without waiting, the test could
 * read the DB / DOM before htmx has swung the response into place.
 */
async function submitAndWaitForSave(page, regionSelector) {
  await exec(
    page,
    `
            const region = document.querySelector(arguments[0]);
            if (!region) throw new Error('region not found: ' + arguments[0]);
            const btn = region.querySelector('button[type="submit"]');
            if (!btn) throw new Error('no submit button in ' + arguments[0]);
            btn.click();
            `,
    regionSelector,
  );
  await waitUntilSubstring(page, `${regionSelector} .alert-success`, 'Settings saved', 5000);
}

// ─── General tab ────────────────────────────────────────────────────

test('general_save_persists_swaps_in_place_and_isolates_other_tabs', async ({ app, page }) => {
  await app.seed('config_distinct');

  await openTab(page, 'general');
  await assertHtmxLoaded(page);

  // Mutate: title_language native -> english. Verifies that the form's
  // coerce-on-save logic picks up the user's choice rather than just
  // re-saving the existing value.
  await exec(
    page,
    `
                const sel = document.getElementById('title_language');
                if (!sel) throw new Error('title_language select not found');
                sel.value = 'english';
                sel.dispatchEvent(new Event('change', { bubbles: true }));
                `,
  );

  await submitAndWaitForSave(page, '#settings-general-region');

  // (1) URL stayed at /settings?tab=general: no full-page navigation.
  await assertHtmxHandledInPlace(page, `${app.baseURL}/settings?tab=general`);

  // (2) DB persisted the new title_language.
  const cfg = await getConfig(app);
  expect(cfg.title_language, 'title_language should reflect the form mutation').toBe('english');

  // (3) Cross-tab isolation: Integrations + Quality fields stayed at
  //     their seeded values. This is the property the per-tab split was
  //     supposed to deliver.
  expect(cfg.jellyfin_url).toBe('http://seed-integrations.local:8096');
  expect(cfg.jellyfin_api_key).toBe('seed-jellyfin-key');
  expect(cfg.sonarr_enabled).toBe(1);
  expect(cfg.sonarr_api_key).toBe('seed-sonarr-key-0123456789');
  expect(cfg.preferred_groups).toBe('SeedGroup1, SeedGroup2');
  expect(cfg.preferred_resolution).toBe('720');
  expect(cfg.cutoff_source).toBe('bluray_remux');
  expect(cfg.finished_series_quality).toBe('bd_only');
  expect(cfg.default_custom_query_tokens).toBe('seed query');
  expect(cfg.default_restrict_to_uploader).toBe('SeedUploader');
  expect(cfg.grab_preview_mode).toBe('never');
  expect(cfg.external_sync_interval_minutes).toBe(90);

  // Other General fields stayed at their seeded values too: only the
  // title_language mutation should land.
  expect(cfg.media_root).toBe('/srv/seed-media');
  expect(cfg.rss_enabled).toBe(0);
  expect(cfg.rss_interval_minutes).toBe(33);
  expect(cfg.disable_nyaa_rss).toBe(1);
});

// ─── Quality tab ────────────────────────────────────────────────────

test('quality_save_persists_swaps_in_place_and_isolates_other_tabs', async ({ app, page }) => {
  await app.seed('config_distinct');

  await openTab(page, 'quality');
  await assertHtmxLoaded(page);

  // Mutate: preferred_resolution 720 -> 1080. (Preferred and cutoff are
  // seeded with distinct values so any cross-field bleeding in the
  // handler shows.)
  await exec(
    page,
    `
                const sel = document.getElementById('preferred_resolution');
                if (!sel) throw new Error('preferred_resolution select not found');
                sel.value = '1080';
                sel.dispatchEvent(new Event('change', { bubbles: true }));
                `,
  );

  await submitAndWaitForSave(page, '#settings-quality-region');

  await assertHtmxHandledInPlace(page, `${app.baseURL}/settings?tab=quality`);

  const cfg = await getConfig(app);
  expect(cfg.preferred_resolution).toBe('1080');

  // Cross-tab isolation.
  expect(cfg.jellyfin_url).toBe('http://seed-integrations.local:8096');
  expect(cfg.sonarr_enabled).toBe(1);
  expect(cfg.grab_preview_mode).toBe('never');
  expect(cfg.external_sync_interval_minutes).toBe(90);
  expect(cfg.title_language).toBe('native');
  expect(cfg.media_root).toBe('/srv/seed-media');
  expect(cfg.rss_enabled).toBe(0);
  expect(cfg.post_processing_mode).toBe('copy');

  // Other Quality fields stayed at their seeded values: only the
  // preferred_resolution mutation should land.
  expect(cfg.preferred_groups).toBe('SeedGroup1, SeedGroup2');
  expect(cfg.blocked_groups).toBe('SeedBlocked');
  expect(cfg.preferred_source).toBe('bluray');
  expect(cfg.cutoff_source).toBe('bluray_remux');
  expect(cfg.cutoff_resolution).toBe('1080');
  expect(cfg.finished_series_quality).toBe('bd_only');
  expect(cfg.prefer_subs).toBe(0);
  expect(cfg.upgrade_search_enabled).toBe(1);
  expect(cfg.seadex_enabled).toBe(1);
});

// ─── Integrations tab ───────────────────────────────────────────────

test('integrations_save_persists_swaps_in_place_and_isolates_other_tabs', async ({ app, page }) => {
  await app.seed('config_distinct');

  await openTab(page, 'integrations');
  await assertHtmxLoaded(page);

  // Mutate: jellyfin_url. Use a localhost-shaped value that will fail
  // the connection test (port 1 is reserved / unbound) so the
  // integrations side effect surfaces a failure notice in the toast.
  // That verifies the side effect fires without depending on a live
  // Jellyfin instance. A new address needs the key typed again: the
  // stored key is write-only and never sent to an address it wasn't
  // saved for.
  await exec(
    page,
    `
                const inp = document.getElementById('jellyfin_url');
                if (!inp) throw new Error('jellyfin_url input not found');
                inp.value = 'http://127.0.0.1:1';
                inp.dispatchEvent(new Event('input', { bubbles: true }));
                const key = document.getElementById('jellyfin_api_key');
                if (!key) throw new Error('jellyfin_api_key input not found');
                if (key.value !== '') throw new Error('the stored key rendered into the page');
                key.value = 'retyped-jellyfin-key';
                `,
  );

  await submitAndWaitForSave(page, '#settings-integrations-region');

  await assertHtmxHandledInPlace(page, `${app.baseURL}/settings?tab=integrations`);

  const cfg = await getConfig(app);
  expect(cfg.jellyfin_url).toBe('http://127.0.0.1:1');

  // Cross-tab isolation.
  expect(cfg.preferred_resolution).toBe('720');
  expect(cfg.preferred_source).toBe('bluray');
  expect(cfg.finished_series_quality).toBe('bd_only');
  expect(cfg.default_custom_query_tokens).toBe('seed query');
  expect(cfg.title_language).toBe('native');
  expect(cfg.media_root).toBe('/srv/seed-media');
  expect(cfg.rss_enabled).toBe(0);
  expect(cfg.post_processing_mode).toBe('copy');

  // Integrations-tab side effect: the failed Jellyfin connection
  // surfaces as a "connection failed" string somewhere in the alert.
  // The wait-for-save keyed on "Settings saved", so the success alert is
  // already there; the failure-side text appears alongside ("Settings
  // saved. Jellyfin connection failed: ...").
  const alertResult = await exec(
    page,
    `
                const el = document.querySelector('#settings-integrations-region .alert-success');
                return el ? (el.textContent || '') : '';
                `,
  );
  const alertText = typeof alertResult === 'string' ? alertResult : '';
  expect(
    alertText.toLowerCase(),
    `expected Jellyfin connection-test failure notice in save toast; got \`${alertText}\``,
  ).toContain('jellyfin connection failed');

  expect(cfg.jellyfin_api_key).toBe('retyped-jellyfin-key');
  // Other Integrations fields stayed at their seeded values.
  expect(cfg.sonarr_enabled).toBe(1);
  expect(cfg.sonarr_api_key).toBe('seed-sonarr-key-0123456789');
  expect(cfg.radarr_enabled).toBe(0);
  expect(cfg.radarr_api_key).toBe('seed-radarr-key-0123456789');
  expect(cfg.grab_preview_mode).toBe('never');
  expect(cfg.external_sync_interval_minutes).toBe(90);
});

// ─── Form-shape regression guards ───────────────────────────────────

// Regression guard: each tab's subform must POST to `/settings/<tab>`,
// not the legacy `/settings` bulk handler. A typo in the partial (e.g.
// `hx-post="/settings"` instead of `hx-post="/settings/general"`) would
// silently route through the legacy bulk handler, which still works
// thanks to the field-by-field `tab` checks but defeats the per-tab
// split's whole point. Pin the wire-up.
test('each_subform_targets_its_dedicated_endpoint', async ({ app, page }) => {
  await app.seed('config_distinct');

  for (const tab of ['integrations', 'quality', 'general']) {
    await openTab(page, tab);
    const region = `#settings-${tab}-region`;
    const actionAttrs = await exec(
      page,
      `
                    const region = document.querySelector(arguments[0]);
                    if (!region) return null;
                    const form = region.querySelector('form');
                    if (!form) return null;
                    return {
                        action: form.getAttribute('action') || '',
                        hxPost: form.getAttribute('hx-post') || '',
                    };
                    `,
      region,
    );

    const want = `/settings/${tab}`;
    const action = typeof actionAttrs?.action === 'string' ? actionAttrs.action : '';
    const hxPost = typeof actionAttrs?.hxPost === 'string' ? actionAttrs.hxPost : '';
    if (action !== want) {
      throw new Error(
        `tab=${tab}: form action \`${action}\` != expected \`${want}\`: ` +
          'subform is wired to the wrong endpoint',
      );
    }
    if (hxPost !== want) {
      throw new Error(
        `tab=${tab}: form hx-post \`${hxPost}\` != expected \`${want}\`: ` +
          "subform's HTMX target is wrong",
      );
    }
  }
});
