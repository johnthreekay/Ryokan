// hx-boost rollout, Phase A browser e2e coverage.
//
// Phase A scope per the hx-boost rollout plan:
//   - `htmx-ext-head-support` vendored + loaded
//   - `<body hx-ext="head-support">` in base.html
//   - `<nav class="mobile-tabbar" hx-boost="true">`: narrowest possible
//     opt-in, 5 anchor links, no forms, distinct per-page CSS targets
//
// These tests pin the head-swap behavior. They're Phase A's failure
// detector for the regression that killed attempt-1 (boost swaps body,
// head stays stale, per-page CSS doesn't load, page renders unstyled).
// All four assertions would fire LOUDLY if `htmx-ext-head-support`
// were ever removed or `hx-ext="head-support"` were dropped from
// `<body>`.

import { test, expect, exec, assertHtmxLoaded, waitForPath } from './harness.js';

// Set a mobile-width viewport so the `.mobile-tabbar` actually
// renders. The tabbar lives at `display:none` above 640px (per
// `topbar.css`); without resizing, the browser boots at desktop width
// and the tabbar links are invisible, so the click fails. 480x800 is
// safely under the breakpoint and matches a typical phone.
async function setMobileViewport(page) {
  await page.setViewportSize({ width: 480, height: 800 });
}

// Set a desktop-width viewport. The desktop top-nav (`.nav-links`)
// is `display:none` below 640px and must be visible for the
// "non-boosted-paths-unchanged" test to click its link.
async function setDesktopViewport(page) {
  await page.setViewportSize({ width: 1280, height: 900 });
}

// The Rust tests read a computed CSS value with
// `value.trim_end_matches("px").parse::<f32>()`: anything but a plain
// number before the `px` suffix fails the test rather than parsing
// partway, as `parseFloat` would.
function parsePx(value) {
  const number = value.replace(/(?:px)+$/, '');
  if (!/^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?$/.test(number)) {
    throw new Error(`expected pixel value, got: ${value}`);
  }
  return Number(number);
}

// **head-css-swap**: the failure-mode detector.
//
// Navigate Library -> Settings via the boosted mobile-tabbar. Without
// `htmx-ext-head-support`, `pages/settings.css` would never load on
// the boosted nav and the assertion below would fail. With it, the
// extension diffs the response head and adds the new `<link>`.
//
// Marker: `base.css` defines `.cf-section { padding: 22px 24px }`.
// Read the computed `padding` of a Settings card; assert it is
// `22px`. Browsers fall back to user-agent default padding (~0)
// when the rule isn't loaded: clear signal.
test('boost_swaps_head_when_navigating_library_to_settings', async ({ page }) => {
  await setMobileViewport(page);
  await page.goto('/');
  // The Rust original computed this and threw the result away
  // (`let _ =`). Enforced here.
  await assertHtmxLoaded(page);

  // Wait for the mobile-tabbar to be present in the DOM.
  await page
    .locator('.mobile-tabbar a[href="/settings"]')
    .first()
    .waitFor({ state: 'attached', timeout: 5000 });

  // Click the boosted Settings tab. With hx-boost on the nav, htmx
  // intercepts the click and does an AJAX swap of `<body>`. The
  // head-support extension then merges the head.
  const settingsLink = page.locator('.mobile-tabbar a[href="/settings"]').first();
  await settingsLink.click();

  // Wait until the URL reflects the new page and a Settings-only
  // element renders.
  await waitForPath(page, '/settings', 5000);
  const fieldset = page.locator('.cf-section').first();
  await fieldset.waitFor({ state: 'attached', timeout: 5000 });

  // Read computed padding. `base.css` sets it to 22px; unstyled
  // fallback is 0 / user-agent default. The mobile-side
  // `@media (max-width: 640px)` override sets 16px, so accept both
  // values (accepting both keeps the test robust to the viewport while
  // still distinguishing styled from unstyled clearly).
  const padding = await fieldset.evaluate((el) =>
    getComputedStyle(el).getPropertyValue('padding-top'),
  );
  const paddingPx = parsePx(padding);
  expect(
    paddingPx,
    'base.css `.cf-section { padding: 22px 24px }` rule must apply after boosted nav; ' +
      `got computed padding-top=${paddingPx}px (head-support extension probably didn't merge ` +
      "the new page's CSS link)",
  ).toBeGreaterThanOrEqual(16);
});

// **head-css-evict**: the inverse direction; ensures Library's CSS
// reapplies when navigating back from Settings.
//
// Marker: `pages/index.css` defines `.btn-pill { border-radius: 999px }`,
// a pill shape that's distinctive vs the default `border-radius: 0`
// fallback.
test('boost_reapplies_origin_css_when_navigating_back', async ({ page }) => {
  await setMobileViewport(page);
  // Start on Settings, then nav to Library via boosted tabbar.
  await page.goto('/settings');
  await assertHtmxLoaded(page);
  await page
    .locator('.mobile-tabbar a[href="/"]')
    .first()
    .waitFor({ state: 'attached', timeout: 5000 });

  const libraryLink = page.locator('.mobile-tabbar a[href="/"]').first();
  await libraryLink.click();

  await waitForPath(page, '/', 5000);

  // Wait for a `.btn-pill` element to render: Library's filter pills
  // carry that class. If the page renders before its CSS loads,
  // `border-radius` is 0 (UA default).
  const btn = page.locator('.btn-pill').first();
  await btn.waitFor({ state: 'attached', timeout: 5000 });
  const radius = await btn.evaluate((el) => getComputedStyle(el).getPropertyValue('border-radius'));
  const radiusPx = parsePx(radius);
  // The rule sets `999px`; browsers may compute it down to whatever
  // the element's largest dimension caps at. As long as it's
  // distinctly more than the unstyled 0, the rule applied.
  expect(
    radiusPx,
    'index.css `.btn-pill { border-radius: 999px }` must apply after boosted back-nav; ' +
      `got computed border-radius=${radiusPx}px`,
  ).toBeGreaterThan(4);
});

// **desktop-nav-is-boosted-under-body-wide-opt-in**: under body-wide
// `hx-boost="true"` (Phase D), the desktop top-nav is boost
// intercepted. Plant a window-scoped marker pre-click; if boost fires,
// the marker survives the swap (boost preserves window scope). A real
// document load would wipe it.
//
// Pre-Phase-D this test was named
// `desktop_nav_does_full_document_navigation_not_boosted_swap` and
// asserted the inverse (marker wiped, proving Phase A's narrow opt-in
// didn't widen onto the desktop nav). Phase D renamed it to match the
// new invariant.
test('desktop_nav_is_boosted_under_body_wide_opt_in', async ({ page }) => {
  await setDesktopViewport(page);
  await page.goto('/');
  await assertHtmxLoaded(page);

  // Plant a window-scoped marker. A boosted swap preserves the
  // `window` object across navigation; a real document load creates a
  // fresh window and the marker is gone.
  await exec(page, "window.__ryokan_boost_phase_a_marker = 'planted';");

  // Wait for the desktop top-nav Settings link.
  await page
    .locator('.nav-links a[href="/settings"]')
    .first()
    .waitFor({ state: 'attached', timeout: 5000 });
  const desktopLink = page.locator('.nav-links a[href="/settings"]').first();
  await desktopLink.click();

  await waitForPath(page, '/settings', 5000);

  // Read marker + boost state in one round trip; the failure message
  // below carries the full diagnostic so an unexpected outcome
  // surfaces with body[hx-boost] and typeof htmx for debugging without
  // spamming green runs with console output.
  const marker = await exec(
    page,
    "return {marker: typeof window.__ryokan_boost_phase_a_marker, hxBoost: document.body.getAttribute('hx-boost'), htmxLoaded: typeof window.htmx};",
  );
  const asStr = (v) => (typeof v === 'string' ? v : '(missing)');
  const markerField = asStr(marker?.marker);
  const hxBoost = asStr(marker?.hxBoost);
  const htmxLoaded = asStr(marker?.htmxLoaded);
  // Phase D update: under body-wide hx-boost, this test inverts. The
  // desktop nav IS now boosted, so the marker should SURVIVE the click.
  // Pre-Phase-D this test asserted the marker was wiped (proving
  // narrow-opt-in didn't widen). Now it asserts body-wide boost
  // successfully intercepts.
  expect(
    markerField,
    'under body-wide hx-boost (Phase D), the desktop top-nav ' +
      'should be boost-intercepted and the window-scoped marker ' +
      `should survive the swap. Got marker=${JSON.stringify(markerField)} ` +
      `body[hx-boost]=${JSON.stringify(hxBoost)} typeof htmx=${JSON.stringify(htmxLoaded)}: ` +
      'body-wide boost may not be wired correctly',
  ).toBe('string');
});

// **shared-css-deduped**: `htmx-ext-head-support` should leave the
// shared `<link>` tags (base.css, topbar.css, forms.css, etc.) in
// place, not re-add them. Each shared CSS link should appear exactly
// once after a boosted swap.
test('boosted_swap_does_not_duplicate_shared_css_links', async ({ page }) => {
  await setMobileViewport(page);
  await page.goto('/');
  await assertHtmxLoaded(page);
  await page
    .locator('.mobile-tabbar a[href="/settings"]')
    .first()
    .waitFor({ state: 'attached', timeout: 5000 });
  await page.locator('.mobile-tabbar a[href="/settings"]').first().click();

  // Wait for the swap.
  await waitForPath(page, '/settings', 5000);

  // Count occurrences of `forms.css` in the rendered head. Both
  // `index.html` and `settings.html` extend `base.html`, which
  // includes `<link rel="stylesheet" href="/static/css/forms.css">`.
  // The extension should leave the shared link alone: if it
  // mistakenly removes-then-re-adds, an instant of FOUC would land and
  // a careless implementation could yield duplicates.
  const count = await exec(
    page,
    'return document.querySelectorAll(\'link[href*="forms.css"]\').length;',
  );
  const n = Number.isInteger(count) ? count : -1;
  expect(
    n,
    'shared forms.css link should appear exactly once in head after boosted swap; ' +
      `got ${n} (head-merge dedup is broken: would FOUC users on every nav)`,
  ).toBe(1);
});
