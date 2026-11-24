//! Direct-handler integration tests for the modal-form endpoints
//! that landed alongside the SAB cleanup PR. Same pattern as
//! `tests/htmx_settings_delete.rs`: invoke the handler with a
//! constructed `State + Form` (or via the test-app router for
//! header-bearing responses), assert on the response shape.
//!
//! Why direct-handler instead of browser-e2e: the browser-e2e
//! variants we tried (now `tests/browser/settings-modals.spec.js`)
//! were observably flaky against geckodriver — in-page `fetch`
//! occasionally dropped the response body, and Firefox's plaintext
//! render of an empty 200 short-circuited the assertion. These
//! handlers don't exercise JS-driven interactions (no click handler,
//! no `htmx.ajax`, no in-page modal toggle) — they're plain
//! `Form<T>` POST / `State<AppState>` GET handlers. Direct invocation
//! covers the wire shape deterministically and is what the existing
//! `tests/htmx_settings_delete.rs` file uses for similar reasons.

use axum::body::to_bytes;
use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use axum::extract::Path as AxumPath;
use ryokan::handlers::settings::download_clients::{
    settings_download_clients_add_form, settings_download_clients_edit_form,
};
use ryokan::handlers::settings::indexers::{
    IndexerStatelessTestForm, settings_indexers_edit_form, settings_indexers_test_stateless,
};
use ryokan::models::download_clients::{DownloadClientForm, insert as insert_dc};
use ryokan::test_support::{build_test_app_state, in_memory_pool};

/// `POST /api/indexers/test` with empty kind / unknown kind / invalid
/// URL is the fast-fail path. The handler must still return 200 with
/// an `HX-Trigger: {"ryokan-indexer-test-result": ...}` header so the
/// toast helper in `static/js/settings.js` fires. A regression that
/// drops the trigger header on the validation paths would leave the
/// user staring at a hung Test button with no feedback.
#[tokio::test]
async fn indexer_stateless_test_empty_url_returns_hx_trigger() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let resp = settings_indexers_test_stateless(
        State(state),
        Form(IndexerStatelessTestForm {
            id: None,
            kind: "torznab".to_string(),
            url: String::new(), // empty — triggers the early-fail path
            api_key: "k".to_string(),
        }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    let trigger = resp
        .headers()
        .get("HX-Trigger")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        trigger.contains("ryokan-indexer-test-result"),
        "validation-fail path must still set HX-Trigger; got: {trigger:?}"
    );
    assert!(
        trigger.contains("\"ok\":false"),
        "validation-fail must report ok:false in the trigger payload; got: {trigger:?}"
    );
}

/// Same shape, an invalid `kind` value goes through a different
/// validation branch. Both branches share the same trigger-emit code
/// (`indexer_test_trigger`) but pin both to catch a future refactor
/// that splits the response paths.
#[tokio::test]
async fn indexer_stateless_test_unknown_kind_returns_hx_trigger() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let resp = settings_indexers_test_stateless(
        State(state),
        Form(IndexerStatelessTestForm {
            id: None,
            kind: "nonsense".to_string(),
            url: "http://example.local/torznab".to_string(),
            api_key: String::new(),
        }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    let trigger = resp
        .headers()
        .get("HX-Trigger")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        trigger.contains("ryokan-indexer-test-result"),
        "unknown-kind validation must still set HX-Trigger; got: {trigger:?}"
    );
}

/// `GET /api/download-clients/add-form` renders the partial with
/// `data-first-torrent` / `data-first-usenet` attributes computed from
/// the current pool. When a torrent client already exists but no
/// usenet does, the SAB row's Default checkbox should be pre-checked
/// (first-of-protocol). The `data-first-*` attrs in the rendered
/// markup are the wire-level pin: the JS in `static/js/settings.js`
/// reads them at modal-open time to decide whether to flip the
/// checkbox. A typo on either side breaks the UX silently.
#[tokio::test]
async fn add_dl_client_form_marks_first_of_protocol_correctly() {
    let db = in_memory_pool().await;
    insert_dc(
        &db,
        DownloadClientForm {
            name: "Test qBit",
            kind: "qbittorrent",
            url: "http://qbit.local",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .expect("seed qbit");

    let state = build_test_app_state(db, None);
    let resp = settings_download_clients_add_form(State(state))
        .await
        .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let html = std::str::from_utf8(&body_bytes).expect("utf-8");

    assert!(
        html.contains("data-first-torrent=\"0\""),
        "torrent default already exists → data-first-torrent must be \"0\"; body:\n{html}"
    );
    assert!(
        html.contains("data-first-usenet=\"1\""),
        "no usenet client yet → data-first-usenet must be \"1\" so SAB Add gets default-checked; body:\n{html}"
    );
}

/// Empty pool — both per-protocol "no current default" probes return
/// `true`, so both data-* attributes should be `"1"`. Pin so a
/// regression that swaps the `is_default` filter for `enabled` (or
/// similar) doesn't silently flip the first-row default behavior.
#[tokio::test]
async fn add_dl_client_form_marks_first_of_both_protocols_when_pool_empty() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let resp = settings_download_clients_add_form(State(state))
        .await
        .into_response();

    let body_bytes = to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let html = std::str::from_utf8(&body_bytes).expect("utf-8");

    assert!(html.contains("data-first-torrent=\"1\""));
    assert!(html.contains("data-first-usenet=\"1\""));
}

/// `GET /settings/download-clients/{id}/edit-form` for an unknown id
/// must return **200** with an inline error partial — NOT a 404.
///
/// Why: the JS-side `openDcEditModal` invokes `htmx.ajax()` to swap the
/// response into `#dc-modal-body`. htmx 2.x's default error policy
/// skips the swap on any non-2xx, so a 404 leaves the modal showing
/// the *previous* form body while the modal title reads "Editing
/// FooClient" — silent breakage with no error indicator.
///
/// The handler now returns `ModalErrorPartial` (a 200 with a visible
/// error blurb in the modal body); this test pins that contract so a
/// future refactor can't reintroduce the 404 path.
#[tokio::test]
async fn dc_edit_form_unknown_id_returns_200_with_error_partial() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let resp = settings_download_clients_edit_form(State(state), AxumPath(99_999))
        .await
        .into_response();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "stale-id edit-form fetch must return 200 (htmx skips swap on non-2xx, leaving silent UI breakage)"
    );
    let body_bytes = to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let html = std::str::from_utf8(&body_bytes).expect("utf-8");
    assert!(
        html.contains("no longer exists") || html.contains("Failed to load"),
        "modal-error partial must explain the failure visibly; got: {html}"
    );
}

/// Symmetric coverage for `GET /settings/indexers/{id}/edit-form` —
/// same htmx-swap-on-non-2xx silent-failure shape, same fix
/// (`ModalErrorPartial` returned with status 200), same regression
/// guard.
#[tokio::test]
async fn indexer_edit_form_unknown_id_returns_200_with_error_partial() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let resp = settings_indexers_edit_form(State(state), AxumPath(99_999))
        .await
        .into_response();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "stale-id edit-form fetch must return 200 — htmx 2.x skips swap on 4xx/5xx"
    );
    let body_bytes = to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let html = std::str::from_utf8(&body_bytes).expect("utf-8");
    assert!(
        html.contains("no longer exists") || html.contains("Failed to load"),
        "modal-error partial must explain the failure visibly; got: {html}"
    );
}
