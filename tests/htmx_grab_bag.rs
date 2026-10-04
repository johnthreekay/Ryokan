//! Phase 1.5 grab-bag handler-level tests (issue #129) — direct
//! handler invocation, no browser. These verify the response shape
//! contract:
//!
//! - **blocklist-remove**: HTMX → empty 200 (so `hx-swap=outerHTML`
//!   strips the row); non-HTMX → 303 redirect with msg flash.
//! - **jellyfin/test, jellyfin/refresh, download-clients/test**: all
//!   return an HTML fragment with a `connection-test-msg` span,
//!   colored green on success and red on failure. Always 200 — htmx
//!   2.x's default error policy skips the swap on 4xx/5xx, so an
//!   error rendered as 502 would silently leave the spinner up.
//!
//! Browser-driven coverage (button click → DOM swap landed) lives in
//! `tests/htmx_browser_e2e_grab_bag.rs`.
//!
//! Same calling pattern as `tests/htmx_settings_delete.rs` — handlers
//! invoked directly with `State` + `HxRequest` + `Form` extractors.

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum_htmx::HxRequest;
use sqlx::SqlitePool;

use ryokan::handlers::downloads::{BlocklistRemoveForm, api_blocklist_remove};
use ryokan::handlers::settings::download_clients::{
    DownloadClientTestForm, settings_download_clients_test,
};
use ryokan::handlers::settings::{JellyfinTestForm, jellyfin_refresh, jellyfin_test};
use ryokan::test_support::{build_test_app_state, in_memory_pool, seed_series};

fn extract_location(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn read_body(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("body is utf-8")
}

/// Seed a `grabbed_torrents` row in `state='failed'` so it shows up
/// on the blocklist tab. Returns the row id. `series_id` is NOT NULL
/// in the production schema, so a series must exist first — pass a
/// pre-seeded id from `seed_series`.
async fn seed_blocklist_entry(db: &SqlitePool, series_id: i64, hash: &str, name: &str) -> i64 {
    sqlx::query(
        "INSERT INTO grabbed_torrents (series_id, hash, torrent_name, episode_numbers, state) \
         VALUES (?, ?, ?, '[1]', 'failed')",
    )
    .bind(series_id)
    .bind(hash)
    .bind(name)
    .execute(db)
    .await
    .expect("seed blocklist entry");
    sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
        .fetch_one(db)
        .await
        .expect("fetch row id")
}

// ─── blocklist-remove ──────────────────────────────────────────────

#[tokio::test]
async fn blocklist_remove_returns_empty_200_for_htmx_request() {
    let db = in_memory_pool().await;
    let series_id = seed_series(&db, 100, "BlockTest Anime").await;
    let id = seed_blocklist_entry(&db, series_id, "abc123", "Test.Release.S01E01").await;
    let state = build_test_app_state(db.clone(), None);

    let resp = api_blocklist_remove(
        State(state.clone()),
        HxRequest(true),
        Form(BlocklistRemoveForm { id }),
    )
    .await
    .expect("handler ok")
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert!(
        body.is_empty(),
        "HTMX blocklist-remove must return empty body so hx-swap=outerHTML strips the row; got: {body:?}"
    );

    // DB-side sanity: row is actually gone.
    let remaining =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents WHERE id = ?")
            .bind(id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "row must be deleted");
}

#[tokio::test]
async fn blocklist_remove_returns_redirect_for_non_htmx_request() {
    let db = in_memory_pool().await;
    let series_id = seed_series(&db, 101, "BlockTest Anime 2").await;
    let id = seed_blocklist_entry(&db, series_id, "def456", "Test.Release.S01E02").await;
    let state = build_test_app_state(db, None);

    let resp = api_blocklist_remove(
        State(state),
        HxRequest(false),
        Form(BlocklistRemoveForm { id }),
    )
    .await
    .expect("handler ok")
    .into_response();

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = extract_location(&resp).unwrap_or_default();
    assert!(
        location.starts_with("/downloads?tab=blocklist"),
        "non-HTMX redirect must land on the blocklist tab; got: {location}"
    );
    assert!(
        location.contains("msg="),
        "non-HTMX redirect must include success flash; got: {location}"
    );
}

// ─── connection-test fragments ─────────────────────────────────────

#[tokio::test]
async fn jellyfin_test_returns_red_message_on_unreachable_url() {
    // Use a deliberately bogus URL — `127.0.0.1:1` is reserved-low,
    // nothing listens, the connect fails fast. Asserts the failure
    // path renders an HTML fragment (not JSON), still 200, with the
    // red color class hint that the partial uses.
    let state = build_test_app_state(in_memory_pool().await, None);
    let resp = jellyfin_test(
        State(state),
        Form(JellyfinTestForm {
            jellyfin_url: "http://127.0.0.1:1".to_string(),
            jellyfin_api_key: "bogus".to_string(),
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "must be 200 even on connect failure — htmx 2.x skips the swap on non-2xx"
    );
    let body = read_body(resp).await;
    assert!(
        body.contains("connection-test-msg"),
        "response must use the shared partial; got: {body}"
    );
    assert!(
        body.contains("var(--red)"),
        "failure must use the red color hint; got: {body}"
    );
}

#[tokio::test]
async fn jellyfin_refresh_returns_red_message_when_not_configured() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);

    let resp = jellyfin_refresh(State(state)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert!(
        body.contains("Jellyfin not configured"),
        "missing-config message must surface in the swap target; got: {body}"
    );
    assert!(
        body.contains("var(--red)"),
        "missing-config is a failure-styled message; got: {body}"
    );
}

#[tokio::test]
async fn download_clients_test_fires_failure_trigger_on_empty_url() {
    // The Test-connection endpoint moved from rendering an inline
    // HTML span (which grew the modal-footer button row to fit long
    // error messages, jittering the layout) to firing an HX-Trigger
    // event the page-level JS converts into a toast. The body is
    // empty and the result rides on the `HX-Trigger` header as a
    // JSON payload — `{"ryokan-dc-test-result": {"ok": ..., "message": "..."}}`.
    let state = build_test_app_state(in_memory_pool().await, None);
    let resp = settings_download_clients_test(
        State(state),
        Form(DownloadClientTestForm {
            id: None,
            kind: "qbittorrent".to_string(),
            url: "  ".to_string(),
            username: String::new(),
            password: String::new(),
            label: String::new(),
        }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let trigger = resp
        .headers()
        .get("HX-Trigger")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        trigger.contains("ryokan-dc-test-result"),
        "test result must fire via HX-Trigger so the toast listener picks it up; got header: {trigger}"
    );
    assert!(
        trigger.contains("URL required"),
        "empty-URL message must travel in the HX-Trigger payload; got: {trigger}"
    );
    assert!(
        trigger.contains("\"ok\":false"),
        "empty-URL must mark the result failed so the toast renders red; got: {trigger}"
    );
}

#[tokio::test]
async fn download_clients_test_fires_failure_trigger_on_unknown_kind() {
    let state = build_test_app_state(in_memory_pool().await, None);
    let resp = settings_download_clients_test(
        State(state),
        Form(DownloadClientTestForm {
            id: None,
            kind: "telnet-rmn".to_string(),
            url: "http://127.0.0.1:1".to_string(),
            username: String::new(),
            password: String::new(),
            label: String::new(),
        }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let trigger = resp
        .headers()
        .get("HX-Trigger")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        trigger.contains("Unknown client kind"),
        "unknown-kind branch must travel in the HX-Trigger payload; got: {trigger}"
    );
    assert!(trigger.contains("\"ok\":false"));
}
