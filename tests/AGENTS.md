# tests/AGENTS.md

Most tests live inline as `#[cfg(test)] mod tests` in the source file they cover (unit tests, pure-function coverage, anything needing `pub(crate)` access). The top-level `tests/` directory is for **integration tests** that exercise only the public API: full-router `oneshot` tests, Sonarr/Radarr compat snapshots, end-to-end HTTP flows.

Each file under `tests/` is its own binary crate that imports `ryokan` as a library dep. The module tree is exposed via `src/lib.rs`; `src/main.rs` is thin (imports from the lib + boots the axum listener).

## `test-support` feature flag

Test-only helpers live in `src/test_support.rs`, gated:

```rust
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
```

Helpers exposed: `in_memory_pool`, `build_test_app_state`, `logged_in_session`, `handler_router`, `sonarr_router`, `radarr_router`, `seed_sonarr_enabled` / `seed_radarr_enabled`, plus seed helpers.

- Unit tests see it via `cfg(test)`.
- Integration tests opt in via `cargo test --features test-support` or `cargo nextest run --features test-support`.
- Each `[[test]]` target in `Cargo.toml` declares `required-features = ["test-support"]` so plain `cargo test` silently skips integration targets rather than failing the build.
- **The suite passes under both `cargo t` (nextest) and plain `cargo test --features test-support`; keep it that way.** nextest runs one process per test, `cargo test` one process per target with tests on parallel threads, so a test that touches process-wide state passes under nextest and fails for a contributor who has no nextest installed (#198 was one such report; about 45 tests failed this way until 2026-11). The patterns, each with a fix in place to copy: a `LazyLock` table keyed by an id (torznab cooldown: a unique id per fixture, `wiremock_tests/fixture.rs::next_fixture_id`; the wrong-API-key throttle: a client address per router, `test_support::with_test_peer`); a `try_lock` job lock (backup, misgrab sweep, RSS sync: a module-level `SERIAL` mutex around the call); a process-wide env var (`ENV_LOCK` held for the whole test); and the AniList search / detail memo caches (give each test its own show and id). CI runs nextest only, so run `cargo test --workspace --features test-support --no-fail-fast` once before a PR that adds tests around any of these.
- Release binaries / Docker images compile without the feature so test helpers don't ship to production.
- CI flips the flag on by default.

## Test-time dev-deps

- `wiremock` — HTTP-backed trait-method tests against mocked upstreams (download clients, Nyaa, providers).
- `insta` — JSON response-shape snapshots on the Sonarr/Radarr compat surface. `cargo insta review` is the explicit update path.
- `rstest` — parameterized `#[case]` tests.

All in `[dev-dependencies]` so release binaries don't pull them.

## Topic-split submodule pattern

When inline tests would push a source file past ~1500 LoC, tests move to a sibling submodule with per-topic files. Existing splits:

- `handlers/auth/tests/{throttle,csrf,sessions,proxy_headers,setup,timing_equalization,forgot_password,sanitize}.rs`
- `services/post_processing/tests/{file_ops,filenames,lock,batch_import_live,batch_preflight,grab_claims_episode,run_once,walk_video_files}.rs`
- `handlers/{sonarr_compat,radarr_compat}/tests/{auth,system,helpers,series|movie}.rs` (plus `snapshots/`)
- `services/download_client/{qbittorrent,deluge,transmission,rtorrent}/wiremock_tests/{fixture, auth|connect|session_handshake, add, list, files, control, seed_rules, hash_case}.rs` and `sabnzbd/wiremock_tests/{fixture,auth_test,category_create,add,list,control}.rs`

The download-client test dir is named `wiremock_tests/` (not `tests/`) to avoid colliding with the inline `#[cfg(test)] mod tests` block that still holds each client's pure-helper + `live_smoke` tests. Discoverable via `cargo test <module>::tests::...` or `<module>::wiremock_tests::...`.

## Env-gated live smokes

Each `services/download_client/*/mod.rs` impl ships a `#[ignore]`d `live_smoke` test that exercises the full trait surface against a real client on localhost. Run with `--ignored` *and* `RYOKAN_{QBIT,DELUGE,TRANSMISSION,RTORRENT,SAB}_E2E=1` set. CI never runs them. See `src/services/download_client/AGENTS.md` for per-client setup.

## Browser e2e

`tests/browser/*.spec.js` are Playwright Test specs in plain JavaScript. They drive real browsers (Chromium and Firefox) against the app and assert post-htmx-swap DOM state — the only layer where "htmx loaded + hx-vals form-encoded correctly + handler response shape correct + template attributes intact" is verified together end-to-end.

Originally scoped to the issue #129 HTMX migration; kept as **general-purpose browser e2e infrastructure** — the harness pays for itself the first time a template-attribute typo (like the `data-ryokan-confirm-label` vs `-yes` bug from PR 131) ships unnoticed by the handler-level test layer.

Until #319 these were Rust tests on `fantoccini` + geckodriver (`tests/htmx_browser_e2e*.rs`). They moved because fantoccini's `webdriver` crate kept `cookie` 0.16 in `Cargo.lock` with no patched release (RUSTSEC-2026-0594), and an optional dependency's tree is audited whether or not it is compiled. Each test kept its Rust function name as its title, so `git log -S <title>` still finds its history. The two stylesheet tests in `boost-phase-a` are the exception: they were renamed when their checks were rewritten, and their old names are in that file's header comment.

### Local run

```bash
scripts/browser-e2e.sh                       # the whole suite: 37 tests on two engines, about 20 s
scripts/browser-e2e.sh --project=chromium    # one engine (or firefox)
scripts/browser-e2e.sh settings-deletes      # one file
scripts/browser-e2e.sh -g cancel_keeps_row   # tests whose title matches
scripts/browser-e2e.sh --headed --workers=1  # watch it run
```

Needs Node.js and Playwright's Chromium and Firefox builds: `cd tests/browser && npx playwright install chromium firefox`, once per Playwright version (Playwright says so itself when a build is missing). `RYOKAN_CHROMIUM_BIN` launches a different Chromium build; `RYOKAN_BROWSER_HEADLESS=0` shows the window; `RYOKAN_E2E_WORKERS` sets how many files run side by side; `RUST_LOG=ryokan=debug` prints the app's log.

**Use the script, not a bare `npx playwright test`.** The script rebuilds the fixture host first. Askama templates and handlers are compiled into that binary, so a bare run after a template or Rust edit tests the old app. `static/` is served from disk: a JS or CSS edit needs no rebuild.

**Nothing skips.** A missing host binary, a missing browser build, and a `test.only` left in (`forbidOnly`) each fail the run. The Rust suite printed `[skip]` and passed when its driver was unreachable, which is how five rotted tests went unnoticed for months.

**Two engines, Playwright's own builds.** Every test runs once on Chromium and once on Firefox (the `projects` in `playwright.config.js`). Playwright cannot drive a stock Firefox or LibreWolf, which is what the Rust suite ran on; its Firefox build is the same Gecko engine with Playwright's automation patches.

**CI runs it**: the `Browser tests` job in `rust.yml`, on every branch push, both engines. With `CI` set a failed test is retried twice (the same allowance nextest's `ci` profile gives the Rust suite; a test that passed on retry is reported as flaky), and a failed run uploads its Playwright traces as the `browser-test-traces` artifact.

The one npm dependency, `@playwright/test`, is pinned exactly in `tests/browser/package.json`, and `package-lock.json` holds three packages with integrity hashes (`npm ci --ignore-scripts`). Dependabot tracks it (`npm` entry, `/tests/browser`). A Playwright bump changes the browser builds it expects, so run the install line again after one.

### How it fits together

- **Fixture host** (`tests/browser/host.rs`, binary `ryokan-e2e-host`, built only under `--features browser-e2e`). The tests cannot build an `AppState` or call a model function, so this does it for them. It serves a control API on a loopback port; `POST /apps` builds what one Rust test used to build in-process: `in_memory_pool()`, a logged-in user, `build_test_app_state`, and `test_support::e2e_browser_app` on its own port. `POST /apps/{id}/seed` seeds through the model functions (a new seed is a `Seed` variant there plus a line in the `harness.js` doc comment); `POST /apps/{id}/sql` runs one statement and returns its rows. The host prints its control URL as its first stdout line and exits when its stdin closes, so it cannot outlive the runner.
- **Harness** (`tests/browser/harness.js`). Import `test` from it instead of from `@playwright/test`. The `app` fixture is one fixture app per test (`app.seed`, `app.sql`, `app.baseURL`, `app.session`); the `page` fixture already carries the session cookie, resolves `page.goto('/settings')` against the app, and refuses requests to anything but the local app. One host process per Playwright worker: a file's tests run in order in one worker, files run side by side, so process-wide state in the app (throttle maps, job locks) is shared only by tests that run one after another.
- **Fixture pages and the router** stay in `src/test_support.rs` (the `e2e` module, same feature). A route a new test needs is mounted there.
- **`exec(page, body, ...args)`** runs a script body the way WebDriver's "execute script" did (`arguments[n]`, `return`). The ported tests keep their script bodies verbatim through it; a new test can use `page.evaluate(() => ...)`.
- **Locators re-query.** WebDriver handed back one element and went stale when it was replaced; a Playwright locator looks the element up again on every use. Use `.first()` where a selector can match several elements (a click on a multi-match locator throws).

### False-positive guards (mandatory for new row-mutation tests)

Per the PR 131 audit:

1. **`assertHtmxHandledInPlace(page, urlPrefix)`** — catches the form-POST fallback masquerading as an htmx swap when the vendored script fails to load. Asserts (a) `window.htmx` is defined, (b) the URL didn't redirect, (c) the URL carries no `msg=` / `err=` flash parameter, the signature of the form-POST fallback's redirect. Neighbor survival is the next guard's job. Where the action navigates on purpose (HX-Refresh, a boosted link), use `assertHtmxLoaded(page)`.
2. **`assertDomContains(page, survivor)`** — catches over-broad swap targets that swallow neighbors. Seed at least 2 rows in the fixture.
3. **DB-side side-effect verification** — the partial response is rendered from the request payload, so a no-op handler still returns a "successful" partial. Always read the row back with `app.sql(...)` and check it is actually gone / present.
4. **`assertModalText(page, slot, expected)`** — catches `data-ryokan-confirm-*` attribute typos that fall through to default copy.

When in doubt, add the assertion and **mutation-test it**: revert the corresponding production code, confirm the test fails with a clear diagnostic, revert your revert. (For a template or handler mutation, rerun through the script so the host is rebuilt.) The port was checked this way: a confirm attribute renamed in `base.js`, a changed pause payload, a missing `clearInterval`, the htmx script removed (32 of 37 tests fail; the five that pass do not use htmx), a handler that skips its delete (caught only by the DB read-back), and a changed confirm title in a template were each caught by the test meant to catch them.

**A check has to read something only the code under test can produce.** Three tests carried over from the Rust suite passed with their feature broken, and mutation is how that showed:

- `data_ts_rebinds_after_boosted_nav` looked for humanized timestamp text after a boosted nav, but the server already renders that text. It now pins the browser's clock three days ahead (`page.clock.setFixedTime`): the server writes "just now" and only the page's own refresh can produce "3d ago".
- The two stylesheet tests in `boost-phase-a` read rules that a shared sheet also satisfied (`.cf-section` had moved into `base.css`; a `.btn-pill` is also a `.btn`, which is rounded 6px without `pages/index.css`). Each now reads a value only its page's sheet gives: `.tabbed-sidebar` computing to `display: flex`, and the 999px pill radius.

## CI-enforced lints

- `tests/htmx_redirect_audit.rs` — every `Redirect::to` callsite must route through `htmx_aware_redirect`, sit inside an `if !is_htmx { ... }` arm, or be in the documented exceptions table. New handlers adding bare `Redirect::to` fail the lint.
- `tests/js_constants_audit.rs` — every `UPPER_SNAKE` name a script under `static/js` uses must be declared somewhere in `static/js` or the templates (`var` / `let` / `const` / `function`, or `window.NAME =`). The scripts share a page-global scope with no bundler, so a deleted constant only fails when its code path runs; 65b545e left every live episode-row update throwing that way.

## Migration discipline (browser-e2e specifically)

For HTMX rollouts: write the browser-e2e test against current behavior **first**, then change the code, then mutation-test the test, then hand-verify in Firefox + a private window. Handler-level tests caught zero of the 14 boost-rollout regressions in 2026-04 — the boost migrations need browser eyes, not handler-level confidence.
