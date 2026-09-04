//! Payload shape + dispatch for `POST /api/webhook/autobrr`.
//! Pins the validation, dedup, indexer-match, and series-match
//! branches so a future refactor can't silently drop one path.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::SqlitePool;
use tower::ServiceExt;

use crate::test_support::{
    autobrr_webhook_router, build_test_app_state, in_memory_pool, seed_autobrr_enabled,
};

const KEY: &str = "test-autobrr-key-abcdef";

async fn post_payload(app: axum::Router, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/webhook/autobrr")
        .header("content-type", "application/json")
        .header("x-api-key", KEY)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn seed_indexer(db: &SqlitePool, name: &str) -> i64 {
    use crate::models::indexers::{IndexerForm, KIND_TORZNAB, insert};
    insert(
        db,
        IndexerForm {
            name,
            kind: KIND_TORZNAB,
            url: "https://prowlarr.local/1/api",
            api_key: "k",
            priority: 25,
            enabled: true,
            is_private_tracker: true,
            seed_ratio: Some(2.0),
            seed_time_minutes: None,
            min_seeders: 0,
            request_timeout_secs: None,
            download_client_id: None,
            rss_enabled: false,
            categories: "",
        },
    )
    .await
    .unwrap()
}

/// Reload `state.indexers` from the test DB via the same helper
/// the production Settings handlers call after upsert/delete
/// (PR #108 review round 2 #2). Keeps tests honest about
/// exercising the same swap-on-write code path.
async fn rebuild_indexer_cache(state: &crate::AppState) {
    crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
}

async fn seed_series(db: &SqlitePool) -> i64 {
    use crate::models::series::{SeriesCore, upsert};
    let (id, _) = upsert(
        db,
        SeriesCore {
            anilist_id: 1,
            mal_id: None,
            title: "Test Show",
            title_romaji: "Test Show",
            title_english: "Test Show",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2024),
            end_year: Some(2024),
        },
    )
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn empty_torrent_name_returns_400() {
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "", "info_hash": "h", "magnet_uri": "m", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("torrent_name"), "body: {body}");
}

#[tokio::test]
async fn no_download_url_returns_400() {
    // Both magnet_uri and torrent_url empty — handler can't
    // dispatch. Pin the 400 + the message hint.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Show", "info_hash": "h", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("magnet_uri") || body.contains("torrent_url"));
}

#[tokio::test]
async fn malformed_json_returns_400() {
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name":"#; // unclosed
    let (status, _) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_indexer_skips_with_200() {
    // autobrr push for an indexer Ryokan doesn't have configured.
    // Per the plan: log + skip rather than grab with default rules.
    // 200 with status=skipped so autobrr doesn't retry.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Show", "info_hash": "h", "magnet_uri": "magnet:m", "indexer": "UnknownIndexer"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"status\":\"skipped\""), "body: {body}");
    assert!(body.contains("indexer not configured"), "body: {body}");
}

#[tokio::test]
async fn no_tracked_series_skips_with_200() {
    // Indexer matches but no series in the library matches the
    // release title. Skip with 200, log it.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    seed_indexer(&db, "Nyaa").await;
    let state = build_test_app_state(db, None);
    rebuild_indexer_cache(&state).await;
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Some Random Title", "info_hash": "h", "magnet_uri": "magnet:m", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"status\":\"skipped\""), "body: {body}");
    assert!(body.contains("no tracked series"), "body: {body}");
}

#[tokio::test]
async fn duplicate_hash_skips_with_200() {
    // Hash already exists in grabbed_torrents in `pending` state.
    // The handler must skip without dispatching.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    seed_indexer(&db, "Nyaa").await;
    let series_id = seed_series(&db).await;
    crate::models::grabbed_torrents::record_grab(
        &db,
        "deadbeef00",
        "Test Show - 01",
        series_id,
        &[1],
        false,
    )
    .await
    .unwrap();
    let state = build_test_app_state(db, None);
    rebuild_indexer_cache(&state).await;
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Test Show - 01", "info_hash": "deadbeef00", "magnet_uri": "magnet:m", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("duplicate hash"), "body: {body}");
}

#[tokio::test]
async fn blocklisted_hash_skips_with_200() {
    // Hash exists in `grabbed_torrents` in `failed` state — i.e.
    // the user blocklisted it. `is_known_hash` filters to
    // pending/imported and would let this through; the handler's
    // separate `is_blocklisted` check must catch it so an IRC
    // re-announce can't silently re-grab a blocklisted release.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    seed_indexer(&db, "Nyaa").await;
    let series_id = seed_series(&db).await;
    let grab_id = crate::models::grabbed_torrents::record_grab(
        &db,
        "deadbeef99",
        "Test Show - 01",
        series_id,
        &[1],
        false,
    )
    .await
    .unwrap()
    .expect("record_grab returns id");
    crate::models::grabbed_torrents::mark_failed(&db, grab_id)
        .await
        .unwrap();
    let state = build_test_app_state(db, None);
    rebuild_indexer_cache(&state).await;
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Test Show - 01", "info_hash": "deadbeef99", "magnet_uri": "magnet:m", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"status\":\"skipped\""), "body: {body}");
    assert!(body.contains("blocklisted"), "body: {body}");
}

#[tokio::test]
async fn no_download_client_returns_503() {
    // Indexer + series both match but no download client is
    // configured. The grab can't be dispatched — return 503 so
    // autobrr can retry once the user wires up a client.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    seed_indexer(&db, "Nyaa").await;
    seed_series(&db).await;
    let state = build_test_app_state(db, None);
    rebuild_indexer_cache(&state).await;
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Test Show - 01 [BD 1080p]", "info_hash": "feedface", "magnet_uri": "magnet:m", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("download client"), "body: {body}");
}
