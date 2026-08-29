//! Shared browser-e2e harness for the `tests/htmx_browser_e2e*.rs`
//! suite. Each integration test target under `tests/` is its own
//! binary crate, so common code lives in `tests/common/` and gets
//! pulled in via:
//!
//! ```ignore
//! #[path = "common/browser_e2e.rs"]
//! mod browser_e2e;
//! use browser_e2e::*;
//! ```
//!
//! Cargo skips files under `tests/common/` for test-binary discovery
//! (it only takes top-level `.rs` files in `tests/`), so this module
//! never produces a "0 tests passed" line.
//!
//! ## What's in here
//!
//! - **`spawn_app`** — bind a random local port + serve the
//!   `e2e_browser_app` router from `test_support` in a background
//!   tokio task.
//! - **`try_connect_browser`** — connect to geckodriver via
//!   `RYOKAN_WEBDRIVER_URL` (default `http://localhost:4444`).
//!   Honors `RYOKAN_BROWSER_BIN` and `RYOKAN_BROWSER_HEADLESS=0`.
//!   Auto-detects Firefox / Firefox-ESR / LibreWolf in PATH and
//!   passes the binary path through `moz:firefoxOptions.binary`.
//! - **`librewolf_shim`** — geckodriver only accepts binaries whose
//!   `--version` output starts with "Mozilla Firefox." LibreWolf
//!   prints "Mozilla LibreWolf X.Y" instead, which trips a
//!   "binary is not a Firefox executable" hard-fail. The shim is a
//!   wrapper script written to a per-PID tempfile that fakes the
//!   version line and execs LibreWolf for everything else. Built
//!   in once geckodriver/Mozilla land first-class LibreWolf
//!   support; until then this is the workaround.
//! - **`open_with_session`** — plant a `session=<token>` cookie via
//!   WebDriver `add_cookie`, then navigate to the requested path.
//!   Cookie matches the production shape (`Path=/`; `SameSite=Lax`)
//!   so modern browsers accept it without `Secure`.
//! - **`seed_user_session`** — write a user + session row directly
//!   via `models::user` + `models::session` and return the hex
//!   token. Bypasses `/login` so the helper doesn't depend on the
//!   throttle / CSRF middleware being configured.
//! - **Assertion helpers**:
//!   - `assert_htmx_loaded` — `window.htmx` is defined.
//!   - `assert_htmx_handled_in_place` — htmx loaded AND the URL
//!     didn't change (catches the form-POST fallback redirecting
//!     after htmx failed to load — a real false positive caught
//!     during the Phase 1.5 audit).
//!   - `assert_modal_text` — modal title / body / yes-button /
//!     no-button text contains an expected substring (catches
//!     `data-ryokan-confirm-*` attribute drift like the
//!     `-label` vs `-yes` typo from PR 131 review).
//!   - `assert_dom_contains` — some node in the DOM still contains
//!     a marker (catches over-broad swap targets like
//!     `closest div` swallowing siblings).
//!   - `wait_for_row_removed` — poll for a marker to disappear.
//!   - `wait_for_confirm_modal` — poll for the confirm modal to
//!     become visible.
//!   - `wait_until_substring` — poll for a selector's text to
//!     contain a substring (specific-content variant; the loose
//!     `wait_until_text_present` was deleted during the audit
//!     because "any text present" is too easy a bar).
//! - **`click_delete_for`** — JS-driven submit-button click on a
//!   row whose `<form>` either has the marker in its
//!   `data-ryokan-confirm-body` attribute or in the surrounding
//!   `<tr>` text. Avoids row-index dependency that breaks under
//!   concurrent test data.
//!
//! ## Conventions for new browser-e2e tests
//!
//! Per the PR 131 audit findings, every row-mutation test should
//! assert at minimum:
//!
//! 1. `assert_htmx_handled_in_place(...)` — catches form-POST
//!    fallback masquerading as an htmx swap when htmx fails to load.
//! 2. `assert_dom_contains(survivor)` — catches over-broad swap
//!    targets that swallow neighbors. Seed at least 2 rows.
//! 3. DB-side side-effect verification — the partial response is
//!    rendered from the request payload, so a no-op handler still
//!    returns a "successful" partial; only a DB read can confirm
//!    the side effect actually landed.
//! 4. `assert_modal_text(slot, expected)` — catches
//!    `data-ryokan-confirm-*` attribute typos that fall through
//!    to default copy.
//!
//! When in doubt, add the assertion and mutation-test it (revert
//! the corresponding production code, confirm the test fails with
//! a clear diagnostic, revert your revert).

// Each test binary that pulls this module via `#[path = "common/browser_e2e.rs"] mod`
// only uses a subset of the helpers — but the file is compiled in
// full per binary (it's not a separate crate). Without this allow,
// every binary emits `dead_code` warnings for the helpers it doesn't
// happen to call. The module is shared infrastructure; "unused in
// this binary" isn't actionable signal.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use fantoccini::ClientBuilder;
use sqlx::SqlitePool;

// ─── Server spawn ──────────────────────────────────────────────────

pub async fn spawn_app(state: ryokan::AppState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("local_addr");
    let app = ryokan::test_support::e2e_browser_app(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    // Tiny settle so the listener is in `accept()` before the browser
    // dials it. axum::serve returns from bind synchronously, but a
    // first connect-immediately-after-bind on some kernels eats the SYN.
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

// ─── WebDriver connect + binary resolution ─────────────────────────

pub async fn try_connect_browser() -> Result<fantoccini::Client, String> {
    let url = std::env::var("RYOKAN_WEBDRIVER_URL")
        .unwrap_or_else(|_| "http://localhost:4444".to_string());

    let mut caps = serde_json::Map::new();
    let headless = std::env::var("RYOKAN_BROWSER_HEADLESS")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let mut firefox_opts = serde_json::Map::new();
    if headless {
        firefox_opts.insert("args".to_string(), serde_json::json!(["-headless"]));
    }
    if let Some(bin) = resolve_browser_binary() {
        firefox_opts.insert("binary".to_string(), serde_json::json!(bin));
    }
    caps.insert(
        "moz:firefoxOptions".to_string(),
        serde_json::Value::Object(firefox_opts),
    );

    // `rustls()` rather than `native()`: fantoccini is built with
    // `default-features = false, features = ["rustls-tls"]` so the
    // native-tls / openssl stack stays out of the lock (see Cargo.toml).
    // geckodriver holds one session; the previous test's `close()` can
    // still be tearing it down when the next test connects. Retry that
    // one condition briefly instead of skipping.
    let mut last_err = String::new();
    for attempt in 0..8 {
        let mut builder =
            ClientBuilder::rustls().map_err(|e| format!("WebDriver TLS connector: {e}"))?;
        builder.capabilities(caps.clone());
        match builder.connect(&url).await {
            Ok(client) => return Ok(client),
            Err(e) => {
                last_err = format!("WebDriver at {url} unavailable: {e}");
                if !last_err.contains("Session is already started") || attempt == 7 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
    }
    Err(last_err)
}

/// Script errors a fixture page captured (`window.__ryokanFixtureErrors`,
/// installed by every `__test/*` fixture before the vendored scripts).
/// Include this in assertion messages so "the toast never appeared"
/// says whether a script threw first.
pub async fn fixture_errors(client: &fantoccini::Client) -> Vec<String> {
    client
        .execute("return window.__ryokanFixtureErrors || [];", vec![])
        .await
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn resolve_browser_binary() -> Option<String> {
    if let Ok(explicit) = std::env::var("RYOKAN_BROWSER_BIN")
        && !explicit.is_empty()
    {
        return Some(explicit);
    }
    for candidate in ["firefox", "firefox-esr"] {
        if let Some(path) = which(candidate) {
            return Some(path);
        }
    }
    if let Some(librewolf) = which("librewolf") {
        return Some(librewolf_shim(&librewolf));
    }
    None
}

fn librewolf_shim(librewolf_path: &str) -> String {
    let wrapper_dir = std::env::temp_dir().join("ryokan-librewolf-shim");
    std::fs::create_dir_all(&wrapper_dir).expect("create shim dir");
    // PID in the filename so the test binaries don't race on the
    // file body when `cargo test` runs them in parallel.
    let wrapper_path = wrapper_dir.join(format!("firefox-shim-{}.sh", std::process::id()));
    let body = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ] || [ \"$1\" = \"-version\" ]; then\n\
         \techo \"Mozilla Firefox 149.0\"\n\
         \texit 0\n\
         fi\n\
         exec {} \"$@\"\n",
        shell_quote(librewolf_path),
    );
    std::fs::write(&wrapper_path, body).expect("write shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&wrapper_path)
            .expect("stat shim")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&wrapper_path, perms).expect("chmod shim");
    }
    wrapper_path.to_string_lossy().into_owned()
}

fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

// ─── Session + navigation ─────────────────────────────────────────

pub async fn seed_user_session(db: &SqlitePool) -> String {
    let user_id =
        ryokan::models::user::create_user(db, "browser-e2e-user", "hunter2-test-password")
            .await
            .expect("create test user");
    ryokan::models::session::create_session(db, user_id)
        .await
        .expect("create session")
}

/// Plant the session cookie + navigate to `path`. Cookie matches
/// production shape (`Path=/; SameSite=Lax`); browsers accept it
/// without `Secure` over HTTP because the SameSite isn't `None`.
pub async fn open_with_session(
    client: &fantoccini::Client,
    addr: SocketAddr,
    session_token: &str,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = format!("http://{addr}");
    client.goto(&format!("{base}/login")).await?;
    let raw = format!("session={session_token}; Path=/; SameSite=Lax");
    let cookie = fantoccini::cookies::Cookie::parse(raw)?;
    client.add_cookie(cookie).await?;
    client.goto(&format!("{base}{path}")).await?;
    Ok(())
}

// ─── Assertions ───────────────────────────────────────────────────

/// Assert `window.htmx` is defined — i.e. the vendored script
/// actually loaded. Use this on tests where the htmx-handled action
/// legitimately changes the URL (e.g. HX-Refresh), so the
/// stay-in-place URL check in `assert_htmx_handled_in_place` doesn't
/// apply.
pub async fn assert_htmx_loaded(
    client: &fantoccini::Client,
) -> Result<(), Box<dyn std::error::Error>> {
    let htmx_loaded: bool = client
        .execute(
            r#"return typeof window.htmx === 'object' && !!window.htmx;"#,
            vec![],
        )
        .await?
        .as_bool()
        .unwrap_or(false);
    if !htmx_loaded {
        return Err("window.htmx is undefined — vendored script failed to load".into());
    }
    Ok(())
}

/// Assert htmx loaded AND the last action did NOT redirect-navigate.
/// Without this check, every row-mutation test would silently pass
/// under "htmx failed to load" — the form's `action="..."` +
/// `method="post"` fallback gets a 303 redirect → page reloads →
/// row vanishes regardless of htmx working. Caught during a
/// mutation-testing audit; pinned here as a defaults-on guard.
pub async fn assert_htmx_handled_in_place(
    client: &fantoccini::Client,
    expected_url_prefix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let htmx_loaded: bool = client
        .execute(
            r#"return typeof window.htmx === 'object' && !!window.htmx;"#,
            vec![],
        )
        .await?
        .as_bool()
        .unwrap_or(false);
    if !htmx_loaded {
        return Err(
            "window.htmx is undefined — vendored script failed to load and the form-POST \
             fallback handled the request"
                .into(),
        );
    }
    let url = client.current_url().await?;
    let url_str = url.as_str();
    if url_str.contains("msg=") || url_str.contains("err=") {
        return Err(format!(
            "URL contains a flash query param ({url_str}) — the form-POST fallback redirected \
             instead of htmx swapping in place"
        )
        .into());
    }
    if !url_str.starts_with(expected_url_prefix) {
        return Err(format!(
            "URL changed from {expected_url_prefix} → {url_str} — page navigated when it should \
             have swapped in place"
        )
        .into());
    }
    Ok(())
}

/// Assert that the confirm modal's title / body / yes / no slot
/// contains the given substring. `slot` is "title" | "body" | "yes"
/// | "no" — maps to the `#ryokan-confirm-{slot}` element in
/// `templates/base.html`. Used to verify that a row form's
/// `data-ryokan-confirm-*` attrs round-trip into the modal copy.
/// Caught the `data-ryokan-confirm-label` typo bug from PR 131
/// review.
pub async fn assert_modal_text(
    client: &fantoccini::Client,
    slot: &str,
    expected_substring: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = format!("ryokan-confirm-{slot}");
    let actual: String = client
        .execute(
            r#"
            const id = arguments[0];
            const el = document.getElementById(id);
            return el ? (el.textContent || '') : '';
            "#,
            vec![serde_json::json!(id)],
        )
        .await?
        .as_str()
        .unwrap_or("")
        .to_string();
    if !actual.contains(expected_substring) {
        return Err(format!(
            "modal {slot} did not contain `{expected_substring}` — got `{actual}`"
        )
        .into());
    }
    Ok(())
}

/// Assert that some node in the DOM contains `marker` as part of its
/// text content. Companion to `wait_for_row_removed` for the
/// survivor-row check pattern: after a delete, the deleted row should
/// be gone AND adjacent rows should still be there. Without this, a
/// stray `hx-target="closest div"` (which would swap the entire
/// containing div) silently passes — the deleted marker disappears
/// but so do all its siblings.
pub async fn assert_dom_contains(
    client: &fantoccini::Client,
    marker: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let present: bool = client
        .execute(
            r#"
            const marker = arguments[0];
            return document.body.textContent.includes(marker);
            "#,
            vec![serde_json::json!(marker)],
        )
        .await?
        .as_bool()
        .unwrap_or(false);
    if !present {
        return Err(format!(
            "expected DOM to still contain `{marker}` — over-broad swap target swallowed it?"
        )
        .into());
    }
    Ok(())
}

// ─── Polling helpers ──────────────────────────────────────────────

pub async fn wait_for_row_removed(
    client: &fantoccini::Client,
    unique_marker: &str,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let still_present: bool = client
            .execute(
                r#"
                const marker = arguments[0];
                // Rows are <tr> in the table-shaped sections and
                // <article> in the card grids (indexers). The confirm
                // modal also quotes the marker, so never scan the whole
                // body.
                return Array.from(document.querySelectorAll('tr, article'))
                    .some(el => el.textContent.includes(marker));
                "#,
                vec![serde_json::json!(unique_marker)],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if !still_present {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "row containing `{unique_marker}` was not removed within {timeout:?}"
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub async fn wait_for_confirm_modal(
    client: &fantoccini::Client,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let visible: bool = client
            .execute(
                r#"
                const m = document.getElementById('ryokan-confirm-modal');
                if (!m) return false;
                return getComputedStyle(m).display !== 'none';
                "#,
                vec![],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if visible {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("confirm modal did not appear within {timeout:?}").into());
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

pub async fn wait_until_substring(
    client: &fantoccini::Client,
    selector: &str,
    substring: &str,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let present: bool = client
            .execute(
                r#"
                const sel = arguments[0];
                const sub = arguments[1];
                const el = document.querySelector(sel);
                if (!el) return false;
                return (el.textContent || '').includes(sub);
                "#,
                vec![serde_json::json!(selector), serde_json::json!(substring)],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if present {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(
                format!("`{selector}` did not contain `{substring}` within {timeout:?}").into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll `client.current_url()` until its path matches
/// `expected_path` or the timeout elapses. Boosted nav is async
/// (htmx swaps body, then pushState's the URL), so a synchronous
/// URL check right after `.click()` would race the URL update.
/// Used by the boost-phase test files.
pub async fn wait_for_path(
    client: &fantoccini::Client,
    expected_path: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let current = client
            .current_url()
            .await
            .map_err(|e| format!("current_url: {e}"))?;
        if current.path() == expected_path {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for path={expected_path:?} (current path: {:?})",
                current.path()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll a JavaScript expression until it returns truthy or timeout
/// elapses. The wrapper coerces the result through `!!(...)` so any
/// truthy / falsy JS value works as a predicate. Replaces fixed-
/// `tokio::time::sleep` patterns that were observed flaky under
/// parallel test execution. Used by the boost-phase test files.
pub async fn wait_for_js_truthy(
    client: &fantoccini::Client,
    expr: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let result = client
            .execute(&format!("return !!({expr});"), vec![])
            .await
            .map_err(|e| format!("execute {expr:?}: {e}"))?;
        if result.as_bool().unwrap_or(false) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for JS expr to be truthy: {expr:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ─── Click helpers ────────────────────────────────────────────────

/// Find a `<form>` whose `data-ryokan-confirm-body` attribute (or a
/// surrounding `<tr>` text) contains the marker, and click its
/// submit button. Avoids row-index dependency.
pub async fn click_delete_for(
    client: &fantoccini::Client,
    unique_marker: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    client
        .execute(
            r#"
            const marker = arguments[0];
            const forms = Array.from(document.querySelectorAll('form'));
            const target = forms.find(f =>
                (f.getAttribute('data-ryokan-confirm-body') || '').includes(marker)
                || f.closest('tr, article')?.textContent.includes(marker));
            if (!target) throw new Error('no delete form found for marker: ' + marker);
            const btn = target.querySelector('button[type="submit"]');
            if (!btn) throw new Error('delete form has no submit button');
            btn.click();
            "#,
            vec![serde_json::json!(unique_marker)],
        )
        .await?;
    Ok(())
}
