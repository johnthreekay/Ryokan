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
- **Use `cargo t` (nextest), not bare `cargo test`, when judging a red run.** nextest runs one process per test; `cargo test` shares one process per target, so process-wide `LazyLock` state leaks between tests. Known instance: the torznab 429 test populates the per-indexer cooldown table, and ~9 later torznab tests then fail under `cargo test` while passing under `cargo t`. Those are runner artifacts, not regressions (this misled a contributor in #198).
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

`tests/htmx_browser_e2e*.rs` are gated on `--features browser-e2e` and drive a real Firefox/LibreWolf via WebDriver/`fantoccini` to assert post-htmx-swap DOM state — the only layer where "htmx loaded + hx-vals form-encoded correctly + handler response shape correct + template attributes intact" is verified together end-to-end.

Originally scoped to the issue #129 HTMX migration; kept as **general-purpose browser e2e infrastructure** — the harness pays for itself the first time a template-attribute typo (like the `data-ryokan-confirm-label` vs `-yes` bug from PR 131) ships unnoticed by the handler-level test layer.

### Local run

```bash
sudo pacman -S geckodriver       # Arch
scripts/browser-e2e.sh           # the whole suite, or: scripts/browser-e2e.sh htmx_browser_e2e_phase1
# RYOKAN_WEBDRIVER_PORT=4445 moves the driver the script starts; RYOKAN_BROWSER_BIN and
# RYOKAN_BROWSER_HEADLESS=0 reach the harness. RYOKAN_WEBDRIVER_URL only makes sense for a
# bare `cargo test` against a driver you started yourself.
```

**Use the script, not a bare `cargo test`.** geckodriver holds exactly one WebDriver session: tests within a binary run in parallel by default and a test that bails before `client.close()` leaks its session, after which every later test prints `[skip] … Session is already started` and passes with an `ok` verdict in 0.01s. That is how five rotted tests went unnoticed for months. The script runs one binary at a time with `--test-threads=1`, restarts the driver between binaries, and reports skips as a separate count and exits non-zero when any test skipped (`RYOKAN_E2E_ALLOW_SKIPS=1` downgrades that to a warning), so a run that never drove a browser cannot look green. Every test must end with `let _ = client.close().await;` on its happy path, and `try_connect_browser` retries briefly on the session-teardown race.

Tests skip (loudly, with `[skip]`) when the driver is unreachable. The `fantoccini` dep sits in `[dependencies]` as `optional = true` (Cargo doesn't allow optional dev-deps) and the e2e fixture handler + router builder live behind `#[cfg(feature = "browser-e2e")]` in `test_support.rs`.

### Shared harness

`tests/common/browser_e2e.rs`, pulled into each test binary via `#[path = "common/browser_e2e.rs"] mod browser_e2e;`. Provides the in-process server spawn (`spawn_app`), WebDriver connect, LibreWolf shim, assertion helpers (`assert_htmx_handled_in_place`, `assert_dom_contains`, `assert_modal_text`, `wait_for_*`).

### False-positive guards (mandatory for new row-mutation tests)

Per the PR 131 audit:

1. **`assert_htmx_handled_in_place`** — catches the form-POST fallback masquerading as an htmx swap when the vendored script fails to load. Asserts (a) `window.htmx` is defined, (b) the URL didn't redirect, (c) the URL carries no `msg=` / `err=` flash parameter, the signature of the form-POST fallback's redirect. Neighbor survival is the next guard's job.
2. **`assert_dom_contains(survivor)`** — catches over-broad swap targets that swallow neighbors. Seed at least 2 rows in the fixture.
3. **DB-side side-effect verification** — the partial response is rendered from the request payload, so a no-op handler still returns a "successful" partial. Always verify the row is actually gone / present in the DB.
4. **`assert_modal_text(slot, expected)`** — catches `data-ryokan-confirm-*` attribute typos that fall through to default copy.

When in doubt, add the assertion and **mutation-test it**: revert the corresponding production code, confirm the test fails with a clear diagnostic, revert your revert.

## CI-enforced lints

- `tests/htmx_redirect_audit.rs` — every `Redirect::to` callsite must route through `htmx_aware_redirect`, sit inside an `if !is_htmx { ... }` arm, or be in the documented exceptions table. New handlers adding bare `Redirect::to` fail the lint.

## Migration discipline (browser-e2e specifically)

For HTMX rollouts: write the browser-e2e test against current behavior **first**, then change the code, then mutation-test the test, then hand-verify in Firefox + a private window. Handler-level tests caught zero of the 14 boost-rollout regressions in 2026-04 — the boost migrations need browser eyes, not handler-level confidence.
