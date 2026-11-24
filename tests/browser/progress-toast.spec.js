// The SSE-driven progress toast (`ryokanProgressToast` in
// `static/js/base.js`). Locks the JS-side wiring shipped in `746dd5c`
// against the SSE endpoint at `/api/progress/{job_id}/stream` shipped
// in the same commit.
//
// Per `feedback_migration_discipline`: SSE migrations need a browser
// test because handler-level tests can't observe the JS-side
// `EventSource` / message-listener / toast.update / toast.finalize
// wiring. A handler test would catch a wire-shape regression but not a
// JS regression that, e.g., never opens the EventSource, ignores the
// `progress` event name, or fails to flip `lastTerminal` on the
// terminal event.
//
// The first test:
//   1. Pre-seeds 3 events into the registry via the fixture
//      `__test/progress-emit` endpoint (info, info, success terminal).
//   2. Opens the fixture page; `ryokanProgressToast` runs at
//      DOMContentLoaded and connects to the SSE stream.
//   3. Waits for the toast title to flip to the terminal event's title
//      ("All done"). That confirms: EventSource opened, all 3 events
//      arrived, terminal triggered finalize, the toast's content
//      reflects the final event's title/body.

import { test, expect, exec, fixtureErrors } from './harness.js';

/**
 * Pre-seed the fixture's 3-event sequence (info, info, success
 * terminal) into the app's progress registry under `progressId`.
 * POSTs without Origin / Referer are rejected by the same-origin CSRF
 * check.
 */
function emitProgress(app, progressId) {
  return fetch(`${app.baseURL}/__test/progress-emit?progress_id=${progressId}`, {
    method: 'POST',
    headers: { Cookie: `session=${app.session}`, Origin: app.baseURL },
  });
}

/**
 * Read `read()` up to `tries` times, `intervalMs` apart, until its text
 * satisfies `done`. Resolves to whether it did; asserts nothing, like
 * the bounded loops the Rust tests ran before their `assert!`.
 */
async function sawWithin(page, tries, intervalMs, read, done) {
  for (let i = 0; i < tries; i++) {
    const text = await read().catch(() => '');
    if (done(text)) return true;
    await page.waitForTimeout(intervalMs);
  }
  return false;
}

// SSE happy path: 3 events buffered, the toast walks through all three,
// and the terminal one triggers finalize. Pins the *visible* terminal
// title rather than the intermediate ones because intermediate events
// can race the EventSource handshake (they may arrive in one batch or
// in sequence depending on the browser's HTTP/1.1 chunk buffering); the
// terminal title is the load-bearing assertion that tells us the toast
// actually finished updating.
test('progress_toast_streams_buffered_events_and_finalizes_on_terminal', async ({ app, page }) => {
  // Mint a unique progress id per test run so concurrent tests can't
  // collide on the registry.
  const progressId = `p_test_${Date.now()}`;

  // Pre-seed events into the registry. This pre-emits the entire
  // 3-event sequence (terminal included) so by the time the EventSource
  // handshake completes, the buffer is full and the stream drains
  // synchronously then closes. That eliminates timing flakiness vs an
  // emit-as-you-go shape.
  const resp = await emitProgress(app, progressId);
  expect(resp.status, 'fixture pre-seed must succeed').toBe(200);

  // Open the fixture page with the progress_id baked in. The page's
  // inline script calls `ryokanProgressToast` at DOMContentLoaded,
  // which opens an EventSource against
  // `/api/progress/{progress_id}/stream`.
  await page.goto(`/__test/progress-toast-fixture?progress_id=${progressId}`);

  // The toast lives in `#ryokan-toast-stack` (created by base.js's
  // ryokanToast helper). Wait for the toast title to reach the terminal
  // event's title: proves all 3 events landed and the terminal
  // triggered toast.finalize().
  const stack = page.locator('#ryokan-toast-stack');
  await stack.waitFor({ state: 'attached', timeout: 5000 });

  const sawTerminal = await sawWithin(
    page,
    40,
    150,
    () => stack.innerText(),
    (text) => text.includes('All done'),
  );
  const errors = await fixtureErrors(page);
  expect(
    sawTerminal,
    "SSE toast must surface the terminal event title; toast stack never showed 'All done': " +
      "indicates EventSource didn't open, message listener didn't fire, or finalize() didn't run; " +
      `fixture script errors: ${JSON.stringify(errors)}`,
  ).toBe(true);
});

// Boost-revisit safety: open the toast, navigate away, come back. No
// orphan EventSource still firing, no listener accumulation. Catches
// the same regression class the lifecycle-helper dedup fix (`c91bb0f`)
// addressed for other modules: if the EventSource stays open after the
// page is gone or fires events into a stale closure, the symptom would
// be a second toast spawning on the new page.
//
// Skipped under boost specifically (which the fixture page doesn't have
// anyway, no `<body hx-boost>`) so this asserts the simpler "old toast
// stays gone after a fresh page load" property.
test('finished_progress_toast_follows_a_full_navigation_exactly_once', async ({ app, page }) => {
  // The toast runtime records live toasts in sessionStorage and
  // rebuilds them on the next full page load. A finished toast with
  // time left on its auto-dismiss must appear once on the next page
  // (not twice: the old follower is gone with the document, and the
  // record is consumed on restore).
  const progressId = `p_follow_${process.hrtime.bigint()}`;

  const resp = await emitProgress(app, progressId);
  expect(resp.status).toBe(200);

  await page.goto(`/__test/progress-toast-fixture?progress_id=${progressId}`);
  const stack = page.locator('#ryokan-toast-stack');
  await stack.waitFor({ state: 'attached', timeout: 5000 });
  await sawWithin(
    page,
    30,
    100,
    () => stack.innerText(),
    (text) => text.includes('All done'),
  );

  const newId = `p_follow_b_${progressId}`;
  await page.goto(`/__test/progress-toast-fixture?progress_id=${newId}`);

  const newStack = page.locator('#ryokan-toast-stack');
  await newStack.waitFor({ state: 'attached', timeout: 5000 });
  const text = await newStack.innerText().catch(() => '');
  expect(
    text.split('All done').length - 1,
    `the finished toast follows the user to the next page exactly once; got: ${JSON.stringify(text)}`,
  ).toBe(1);
  expect(
    text.includes('Initializing'),
    `the new page's own toast opens next to it; got: ${JSON.stringify(text)}`,
  ).toBe(true);
  // The restored toast is a plain finished toast: no spinner, and it
  // still dismisses on its own.
  const spinners = await exec(
    page,
    "return Array.from(document.querySelectorAll('.ryokan-toast')).map(function (t) { var s = t.querySelector('.ryokan-toast-spinner'); return getComputedStyle(s).display; });",
  );
  const states = Array.isArray(spinners) ? spinners : [];
  expect(
    states.filter((d) => d === 'none').length,
    `the finished toast shows no spinner; spinner states: ${JSON.stringify(states)}`,
  ).toBeGreaterThanOrEqual(1);
  const gone = await sawWithin(
    page,
    60,
    150,
    () => newStack.innerText(),
    (t) => !t.includes('All done'),
  );
  expect(gone, 'the restored toast keeps its auto-dismiss').toBe(true);
});

test('running_progress_toast_survives_a_boosted_swap_with_its_follower', async ({ app, page }) => {
  // A job still running when the user follows a boosted link: the
  // spinner toast is carried into the new body, its EventSource stays
  // open, and the events that arrive afterwards land on the carried
  // toast, which then finishes and drops the spinner.
  const progressId = `p_boost_${process.hrtime.bigint()}`;

  // Not seeded yet: the toast opens on "Initializing" with the spinner
  // and waits.
  await page.goto(`/__test/progress-toast-fixture?progress_id=${progressId}`);
  await page
    .locator('#ryokan-toast-stack .ryokan-toast')
    .first()
    .waitFor({ state: 'attached', timeout: 5000 });
  const spinnerBefore = await exec(
    page,
    "return getComputedStyle(document.querySelector('.ryokan-toast-spinner')).display;",
  );
  expect(
    typeof spinnerBefore === 'string' ? spinnerBefore : '',
    'a running progress toast shows its spinner',
  ).not.toBe('none');

  // Follow the boosted link. The new body carries the old toast.
  await page.locator('#fixture-nav').click();
  await page
    .locator('#fixture-page[data-fixture-progress$="-hop"]')
    .first()
    .waitFor({ state: 'attached', timeout: 5000 });
  const stack = page.locator('#ryokan-toast-stack .ryokan-toast').first();
  await stack.waitFor({ state: 'attached', timeout: 5000 });
  const text = await stack.innerText().catch(() => '');
  expect(
    text.includes('Initializing'),
    `the running toast is still on screen after the swap; got: ${JSON.stringify(text)}`,
  ).toBe(true);

  // Now the job produces its events; the carried follower must see
  // them.
  const resp = await emitProgress(app, progressId);
  expect(resp.status).toBe(200);

  const whole = page.locator('#ryokan-toast-stack');
  const sawTerminal = await sawWithin(
    page,
    40,
    150,
    () => whole.innerText(),
    (t) => t.includes('All done'),
  );
  const errors = await fixtureErrors(page);
  expect(
    sawTerminal,
    'the follower carried across the swap must receive the terminal event; ' +
      `fixture script errors: ${JSON.stringify(errors)}`,
  ).toBe(true);
  const count = await exec(
    page,
    "return document.querySelectorAll('#ryokan-toast-stack .ryokan-toast').length;",
  );
  expect(count, 'one toast, not a carried one plus a new one').toBe(1);
  const spinnerAfter = await exec(
    page,
    "return getComputedStyle(document.querySelector('.ryokan-toast-spinner')).display;",
  );
  expect(
    typeof spinnerAfter === 'string' ? spinnerAfter : '',
    'the spinner goes with the terminal event',
  ).toBe('none');
});
