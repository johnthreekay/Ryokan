// Boosted navigation and page stylesheets.
//
// A boosted navigation swaps `<body>` and leaves `<head>` alone. The
// first attempt at hx-boost died of exactly that: the new page arrived
// without its stylesheet and rendered unstyled. The fix that stuck is
// in `templates/base.html`: every `pages/*.css` sheet is linked on
// every page, so no head merge is needed (the htmx 2 head-support
// extension that once did it is gone).
//
// These tests pin what that buys. After a boosted navigation the
// destination page's own stylesheet is in force, the shared sheets are
// not linked twice, and the navigation really was boosted. Each
// stylesheet check reads a rule that only that page's sheet defines,
// so it fails if the sheet goes back to being linked per page.
//
// The first two tests were `boost_swaps_head_when_navigating_library_to_settings`
// and `boost_reapplies_origin_css_when_navigating_back` in the Rust
// suite. They were renamed when their checks were rewritten: the
// Settings one used to read a rule that had moved into a shared sheet.

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

// Library -> Settings through the boosted mobile tab bar.
//
// Marker: `pages/settings.css` is the only sheet with a rule for
// `.tabbed-sidebar`, and it makes the Settings section nav a flex row.
// Without that sheet a `<nav>` computes to `display: block`.
test('settings_page_css_is_in_force_after_boosted_nav_from_library', async ({ page }) => {
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

  // Click the boosted Settings tab: htmx intercepts the click and
  // swaps `<body>`.
  const settingsLink = page.locator('.mobile-tabbar a[href="/settings"]').first();
  await settingsLink.click();

  // Wait until the URL reflects the new page and a Settings-only
  // element renders.
  await waitForPath(page, '/settings', 5000);
  const sidebar = page.locator('.tabbed-sidebar').first();
  await sidebar.waitFor({ state: 'attached', timeout: 5000 });

  const display = await sidebar.evaluate((el) => getComputedStyle(el).getPropertyValue('display'));
  expect(
    display,
    'pages/settings.css `.tabbed-sidebar { display: flex }` must apply after a boosted nav ' +
      `into Settings; got computed display=${display} (is the sheet still linked from ` +
      'base.html on every page?)',
  ).toBe('flex');
});

// The other direction: Settings -> Library.
//
// Marker: `pages/index.css` is the only sheet with a rule for
// `.btn-pill`, and it gives the Library's pills a `border-radius` of
// `var(--radius-pill)` (999px). The pills are also `.btn`, which the
// shared forms sheet rounds to 6px, so the check asks for the pill
// radius itself: "more than a few pixels" passed without the sheet.
test('library_page_css_is_in_force_after_boosted_nav_from_settings', async ({ page }) => {
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

  // Wait for a `.btn-pill` element to render: the Library's pills
  // carry that class.
  const btn = page.locator('.btn-pill').first();
  await btn.waitFor({ state: 'attached', timeout: 5000 });
  const radius = await btn.evaluate((el) => getComputedStyle(el).getPropertyValue('border-radius'));
  const radiusPx = parsePx(radius);
  // With the sheet: 999px. Without it: the 6px every `.btn` has.
  expect(
    radiusPx,
    'pages/index.css `.btn-pill` border-radius (999px) must apply after a boosted nav into ' +
      `the Library; got computed border-radius=${radiusPx}px (is the sheet still linked ` +
      'from base.html on every page?)',
  ).toBeGreaterThan(100);
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

// **shared-css-deduped**: a boosted swap leaves the shared `<link>`
// tags (base.css, topbar.css, forms.css, etc.) alone. Each should
// appear exactly once afterwards.
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

  // Count the `forms.css` links in the document. Both `index.html`
  // and `settings.html` extend `base.html`, which links it once in
  // `<head>`. A second copy means the swap brought a stylesheet link
  // along (one moved into a page body, or a head merge switched back
  // on and re-adding what was already there).
  const count = await exec(
    page,
    'return document.querySelectorAll(\'link[href*="forms.css"]\').length;',
  );
  const n = Number.isInteger(count) ? count : -1;
  expect(
    n,
    'shared forms.css link should appear exactly once after a boosted swap; ' +
      `got ${n} (a duplicate would flash unstyled content on every navigation)`,
  ).toBe(1);
});
