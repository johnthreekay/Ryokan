//! Browser-e2e coverage for the indexer Edit-card → modal flow.
//! That flow is JS-driven (`openIndexerEditModal` calls `htmx.ajax`),
//! so the browser layer adds value: a typo in the JS, a missing data
//! attribute on the card, or htmx itself failing to load would all
//! break this in production but pass at the handler layer.
//!
//! The two non-JS-interactive checks that were here (indexer Test
//! endpoint HX-Trigger header + DC add-form data-first-* attributes)
//! moved to `tests/htmx_settings_modal_handlers.rs` since they validate
//! server-rendered output and don't benefit from a real browser. The
//! browser-e2e variants were observably flaky against geckodriver
//! (in-page `fetch` + `goto`-then-`source` both intermittently
//! returned empty bodies under parallel test execution); the
//! handler-direct variant is deterministic.
//!
//! Skips gracefully when geckodriver/WebDriver is unreachable; see
//! `tests/htmx_browser_e2e.rs` for run instructions.

use fantoccini::Locator;
use ryokan::models::indexers::{IndexerForm, insert as insert_indexer};
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use sqlx::SqlitePool;
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_loaded, open_with_session, seed_user_session, spawn_app, try_connect_browser,
};

async fn seed_indexer(db: &SqlitePool, name: &str) -> i64 {
    insert_indexer(
        db,
        IndexerForm {
            name,
            kind: "torznab",
            url: "https://example.com/torznab",
            api_key: "k",
            priority: 25,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 1,
            request_timeout_secs: None,
            download_client_id: None,
            rss_enabled: false,
        },
    )
    .await
    .expect("seed indexer")
}

/// Clicking the Edit button on an indexer card fires an
/// `hx-get="/settings/indexers/{id}/edit-form"` that swaps the modal
/// body. The handler must return a populated form (Name, URL, API
/// Key, Priority, etc.) bound to the row's existing values, not a
/// blank shell. Pin the wire shape so a regex-replace refactor of the
/// partial doesn't silently nullify the populated values.
#[tokio::test]
async fn indexer_edit_modal_loads_populated_form() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let indexer_id = seed_indexer(&db, "Test Indexer Alpha").await;
    let addr = spawn_app(state).await;

    open_with_session(&client, addr, &session, "/settings?tab=indexers")
        .await
        .expect("open settings");
    let _ = assert_htmx_loaded(&client).await;

    // Click the indexer card body — the whole card is a click
    // target (`role="button"` on the body div with `data-indexer-id`),
    // and `openIndexerEditModal` fetches the edit-form partial via
    // `htmx.ajax` into `#indexer-modal-body`.
    let card_body = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css(&format!("[data-indexer-id=\"{indexer_id}\"]")))
        .await
        .expect("indexer card body");
    card_body.click().await.expect("click card");

    // Modal body should populate with the indexer's name pre-filled.
    let name_input = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("#indexer-modal-body input[name=\"name\"]"))
        .await
        .expect("name input present");
    let name_value = name_input.prop("value").await.expect("read value");
    assert_eq!(
        name_value.as_deref(),
        Some("Test Indexer Alpha"),
        "edit modal must populate Name from the indexer row, not render blank"
    );

    let _ = client.close().await;
}

/// Stale-id edit-form fetch must render the inline modal-error
/// partial, NOT silently leave the modal body unchanged. Pin the
/// browser-side observable behavior of the
/// "always-200 with error rendered inline" pattern shipped in
/// PR `0b1757c`.
///
/// Why browser-driven: handler-level tests (in
/// `tests/htmx_settings_modal_handlers.rs`) already pin the
/// response wire-shape (200 + error string in body). The browser-e2e
/// adds value by verifying that:
///   1. the JS-driven `htmx.ajax()` call from `openIndexerEditModal`
///      *actually swaps* the response into `#indexer-modal-body`
///      (htmx 2.x's default policy skips swap on non-2xx — a future
///      regression that puts the handler back to 404 would surface
///      here as "modal body unchanged" while the handler-test
///      passes if checking only the wire shape on 200).
///   2. the swap target id (`#indexer-modal-body`) actually exists
///      in the rendered settings page; a typo in either side would
///      break the test deterministically.
#[tokio::test]
async fn indexer_edit_modal_stale_id_renders_error_partial() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    // Seed a real indexer so the page has something to render — the
    // stale-id modal trigger uses a hardcoded 99999 below.
    let _ = seed_indexer(&db, "Real Indexer").await;
    let addr = spawn_app(state).await;

    open_with_session(&client, addr, &session, "/settings?tab=indexers")
        .await
        .expect("open settings");
    let _ = assert_htmx_loaded(&client).await;

    // Bypass card click and JS-trigger the edit-modal opener directly
    // with a known-stale id (99999). Functionally equivalent to the
    // user clicking Edit on a card that was deleted in another tab
    // before their click landed — the row no longer exists.
    let opened: serde_json::Value = client
        .execute(
            r#"
            if (typeof window.openIndexerEditModal !== 'function') {
                return {ok:false, err:'openIndexerEditModal missing — settings.js not loaded'};
            }
            window.openIndexerEditModal(99999, 'Stale Indexer');
            return {ok:true};
            "#,
            vec![],
        )
        .await
        .expect("invoke openIndexerEditModal");
    assert_eq!(
        opened.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "openIndexerEditModal entry-point must exist; got: {opened}"
    );

    // Wait for the modal body to populate with the error partial.
    // The error blurb's copy comes from the handler in
    // `src/handlers/settings/indexers.rs`; the first sentence is the
    // load-bearing user-visible string we pin.
    let modal_body = client
        .wait()
        .at_most(Duration::from_secs(5))
        .for_element(Locator::Css("#indexer-modal-body"))
        .await
        .expect("modal body present");
    // Poll the body's text until the error blurb arrives — the
    // htmx.ajax() round-trip is async and the test must not race
    // against it.
    let mut found = false;
    for _ in 0..20 {
        let text = modal_body.text().await.unwrap_or_default();
        if text.contains("no longer exists") {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found,
        "stale-id edit-form must render the inline error partial; modal body never showed the expected error string"
    );

    let _ = client.close().await;
}
