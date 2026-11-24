// Shared harness for the browser tests.
//
// What a test gets by importing `test` from here instead of from
// '@playwright/test':
//
//   app   One Ryokan fixture app for this test: its own in-memory
//         database, a logged-in user, its own port. `app.seed(...)` and
//         `app.sql(...)` reach into that database. Created by the
//         fixture host (host.rs), which the harness starts once per
//         worker.
//   page  Already carries the session cookie, and `page.goto('/settings')`
//         resolves against the app. Requests to anything but the local
//         app are refused, so a test never depends on the network.
//
// The assertion helpers below are the suite's false-positive guards.
// `tests/AGENTS.md` ("Browser e2e") says which ones a new row-mutation
// test has to call and why.

import { test as base, expect } from '@playwright/test';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const hostBinary =
  process.env.RYOKAN_E2E_HOST || path.join(repoRoot, 'target', 'debug', 'ryokan-e2e-host');

/** Start the fixture host and resolve once it has printed its control URL. */
function startHost() {
  if (!fs.existsSync(hostBinary)) {
    throw new Error(
      `fixture host not built: ${hostBinary}\n` +
        'Run scripts/browser-e2e.sh, or: cargo build --features browser-e2e --bin ryokan-e2e-host',
    );
  }
  // stdin stays open and unused: the host exits when it closes, so it
  // cannot outlive this process. stderr passes through for RUST_LOG.
  const child = spawn(hostBinary, [], { cwd: repoRoot, stdio: ['pipe', 'pipe', 'inherit'] });
  return new Promise((resolve, reject) => {
    const lines = createInterface({ input: child.stdout });
    child.once('error', reject);
    child.once('exit', (code) => reject(new Error(`fixture host exited early (code ${code})`)));
    lines.once('line', (line) => {
      try {
        resolve({ control: JSON.parse(line).control, child });
      } catch (e) {
        reject(new Error(`unexpected first line from the fixture host: ${line}`));
      }
    });
  });
}

async function control(base, method, route, body) {
  const response = await fetch(base + route, {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`fixture host ${method} ${route} -> ${response.status}: ${text}`);
  }
  return text ? JSON.parse(text) : null;
}

/** One test's fixture app. */
class App {
  constructor(controlUrl, info) {
    this.controlUrl = controlUrl;
    this.id = info.id;
    /** `http://127.0.0.1:<port>`, no trailing slash. */
    this.baseURL = info.base_url;
    /** The logged-in user's session token (the `session` cookie's value). */
    this.session = info.session;
  }

  /**
   * Seed a row through the model function the app itself uses.
   * Kinds: series {anilist_id, title}, episode_monitored {series_id,
   * episode, monitored}, indexer {name, api_key?}, download_client {name,
   * is_default}, custom_format {name}, group {name}, config_default,
   * config_distinct,
   * grab {hash, name, series_id, episodes, batch}.
   * Resolves to the new row's id where there is one.
   */
  async seed(kind, fields = {}) {
    const result = await control(this.controlUrl, 'POST', `/apps/${this.id}/seed`, {
      kind,
      ...fields,
    });
    return result.id;
  }

  /** Run one SQL statement with `?` parameters; resolves to its rows as objects. */
  async sql(statement, ...params) {
    const result = await control(this.controlUrl, 'POST', `/apps/${this.id}/sql`, {
      sql: statement,
      params,
    });
    return result.rows;
  }

  /** Delete a session row server-side, as a logout from elsewhere would. */
  async deleteSession(token = this.session) {
    await control(this.controlUrl, 'POST', `/apps/${this.id}/session/delete`, { token });
  }

  async sessionValid(token = this.session) {
    const result = await control(this.controlUrl, 'POST', `/apps/${this.id}/session/valid`, {
      token,
    });
    return result.valid;
  }

  /** Ids the fake download client was asked to pause (needs `appOptions.queueClient`). */
  async pauses() {
    const result = await control(this.controlUrl, 'GET', `/apps/${this.id}/pauses`);
    return result.pauses;
  }
}

export const test = base.extend({
  // One fixture host process per worker.
  host: [
    async ({}, use) => {
      const host = await startHost();
      await use(host);
      host.child.removeAllListeners('exit');
      host.child.kill();
    },
    { scope: 'worker' },
  ],

  // `test.use({ appOptions: { queueClient: true } })` gives the app the
  // fake download client: one queue item, pauses recorded.
  appOptions: [{}, { option: true }],

  app: async ({ host, appOptions }, use) => {
    const info = await control(host.control, 'POST', '/apps', {
      queue_client: !!appOptions.queueClient,
    });
    const app = new App(host.control, info);
    await use(app);
    await control(host.control, 'DELETE', `/apps/${app.id}`);
  },

  baseURL: async ({ app }, use) => {
    await use(app.baseURL);
  },

  context: async ({ context, app }, use) => {
    // Same shape the old WebDriver harness planted: Path=/, SameSite=Lax.
    await context.addCookies([
      { name: 'session', value: app.session, url: app.baseURL, sameSite: 'Lax' },
    ]);
    // Keep the browser off the network.
    await context.route(
      (url) => url.hostname !== '127.0.0.1' && url.hostname !== 'localhost',
      (route) => route.abort(),
    );
    await use(context);
  },
});

export { expect };

// ─── Script execution ───────────────────────────────────────────────

/**
 * Run a script body in the page the way WebDriver's "execute script"
 * does: the extra arguments arrive as `arguments[0]`, `arguments[1]`,
 * and `return` hands a value back. The suite was ported from WebDriver
 * with its script bodies unchanged; a new test can just as well call
 * `page.evaluate(() => ...)`.
 */
export function exec(page, body, ...args) {
  return page.evaluate(({ body, args }) => new Function(body)(...args), { body, args });
}

/**
 * Script errors a fixture page captured (`window.__ryokanFixtureErrors`,
 * installed by every `__test/*` fixture before the vendored scripts).
 * Put this in a failure message so "the toast never appeared" says
 * whether a script threw first.
 */
export async function fixtureErrors(page) {
  try {
    return await page.evaluate(() => window.__ryokanFixtureErrors || []);
  } catch {
    return [];
  }
}

/**
 * Await `promise`; when it rejects, fail with `<message>: <cause>`.
 * Gives a wait or a click the label a Rust `.expect("...")` gave it.
 */
export async function withMessage(promise, message) {
  try {
    return await promise;
  } catch (e) {
    throw new Error(`${message}: ${e.message}`);
  }
}

// ─── Assertions ─────────────────────────────────────────────────────

/**
 * `window.htmx` is defined, i.e. the vendored script loaded. Use this
 * where the htmx-handled action is allowed to change the URL
 * (HX-Refresh), so `assertHtmxHandledInPlace` does not apply.
 */
export async function assertHtmxLoaded(page) {
  const loaded = await page.evaluate(() => typeof window.htmx === 'object' && !!window.htmx);
  if (!loaded) {
    throw new Error('window.htmx is undefined: the vendored script failed to load');
  }
}

/**
 * htmx loaded AND the last action did not navigate. Without this, a
 * row-mutation test passes when htmx never loaded: the form's
 * `action` + `method="post"` fallback gets a 303, the page reloads, and
 * the row is gone all the same.
 */
export async function assertHtmxHandledInPlace(page, expectedUrlPrefix) {
  const loaded = await page.evaluate(() => typeof window.htmx === 'object' && !!window.htmx);
  if (!loaded) {
    throw new Error(
      'window.htmx is undefined: the vendored script failed to load and the form-POST ' +
        'fallback handled the request',
    );
  }
  const url = page.url();
  if (url.includes('msg=') || url.includes('err=')) {
    throw new Error(
      `URL contains a flash query param (${url}): the form-POST fallback redirected ` +
        'instead of htmx swapping in place',
    );
  }
  if (!url.startsWith(expectedUrlPrefix)) {
    throw new Error(
      `URL changed from ${expectedUrlPrefix} to ${url}: the page navigated when it should ` +
        'have swapped in place',
    );
  }
}

/**
 * The confirm modal's title / body / yes / no slot contains the text.
 * `slot` maps to `#ryokan-confirm-{slot}` in `templates/base.html`.
 * Catches a `data-ryokan-confirm-*` attribute typo that falls through
 * to the default copy.
 */
export async function assertModalText(page, slot, expectedSubstring) {
  const actual = await page.evaluate((id) => {
    const el = document.getElementById(id);
    return el ? el.textContent || '' : '';
  }, `ryokan-confirm-${slot}`);
  if (!actual.includes(expectedSubstring)) {
    throw new Error(`modal ${slot} did not contain \`${expectedSubstring}\`; got \`${actual}\``);
  }
}

/**
 * Some node still contains `marker`. The survivor check after a delete:
 * an over-broad swap target takes the neighbors out with the deleted
 * row, and only this notices.
 */
export async function assertDomContains(page, marker) {
  const present = await page.evaluate((m) => document.body.textContent.includes(m), marker);
  if (!present) {
    throw new Error(
      `expected DOM to still contain \`${marker}\`: over-broad swap target swallowed it?`,
    );
  }
}

// ─── Waiting ────────────────────────────────────────────────────────

async function waitFor(page, predicate, arg, timeout, describe) {
  try {
    await page.waitForFunction(predicate, arg, { timeout, polling: 50 });
  } catch (e) {
    if (String(e).includes('Timeout')) {
      throw new Error(`${describe} within ${timeout}ms`);
    }
    throw e;
  }
}

/** Wait until no `<tr>` / `<article>` contains the marker. */
export async function waitForRowRemoved(page, uniqueMarker, timeout = 5000) {
  // Rows are <tr> in the table-shaped sections and <article> in the
  // card grids (indexers). The confirm modal also quotes the marker, so
  // never scan the whole body.
  await waitFor(
    page,
    (marker) =>
      !Array.from(document.querySelectorAll('tr, article')).some((el) =>
        el.textContent.includes(marker),
      ),
    uniqueMarker,
    timeout,
    `row containing \`${uniqueMarker}\` was not removed`,
  );
}

export async function waitForConfirmModal(page, timeout = 3000) {
  await waitFor(
    page,
    () => {
      const m = document.getElementById('ryokan-confirm-modal');
      return !!m && getComputedStyle(m).display !== 'none';
    },
    undefined,
    timeout,
    'confirm modal did not appear',
  );
}

/** Wait until the first element matching `selector` contains `substring`. */
export async function waitUntilSubstring(page, selector, substring, timeout = 5000) {
  await waitFor(
    page,
    ([sel, sub]) => {
      const el = document.querySelector(sel);
      return !!el && (el.textContent || '').includes(sub);
    },
    [selector, substring],
    timeout,
    `\`${selector}\` did not contain \`${substring}\``,
  );
}

/**
 * Wait until the URL's path is `expectedPath`. A boosted navigation
 * swaps the body and then pushes the URL, so a check right after the
 * click would race it.
 */
export async function waitForPath(page, expectedPath, timeout = 5000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const current = new URL(page.url()).pathname;
    if (current === expectedPath) return;
    if (Date.now() >= deadline) {
      throw new Error(
        `timed out waiting for path=${JSON.stringify(expectedPath)} (current path: ${JSON.stringify(current)})`,
      );
    }
    await page.waitForTimeout(100);
  }
}

/** Wait until a JavaScript expression, evaluated in the page, is truthy. */
export async function waitForJsTruthy(page, expr, timeout = 5000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    let truthy = false;
    try {
      truthy = await page.evaluate((e) => !!new Function(`return (${e});`)(), expr);
    } catch (e) {
      // A navigation can tear the page down mid-evaluate; the next
      // round sees the new document.
      if (!/context was destroyed|navigation/i.test(String(e))) {
        throw new Error(`execute ${JSON.stringify(expr)}: ${e}`);
      }
    }
    if (truthy) return;
    if (Date.now() >= deadline) {
      throw new Error(`timed out waiting for JS expr to be truthy: ${JSON.stringify(expr)}`);
    }
    await page.waitForTimeout(50);
  }
}

// ─── Clicks ─────────────────────────────────────────────────────────

/**
 * Click the submit button of the `<form>` whose `data-ryokan-confirm-body`
 * (or surrounding row's text) contains the marker. Finds the row by
 * what it says, never by its position.
 */
export async function clickDeleteFor(page, uniqueMarker) {
  await page.evaluate((marker) => {
    const forms = Array.from(document.querySelectorAll('form'));
    const target = forms.find(
      (f) =>
        (f.getAttribute('data-ryokan-confirm-body') || '').includes(marker) ||
        f.closest('tr, article')?.textContent.includes(marker),
    );
    if (!target) throw new Error('no delete form found for marker: ' + marker);
    const btn = target.querySelector('button[type="submit"]');
    if (!btn) throw new Error('delete form has no submit button');
    btn.click();
  }, uniqueMarker);
}
