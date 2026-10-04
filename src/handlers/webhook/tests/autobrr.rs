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

    let body = r#"{"torrent_name": "", "info_hash": "aabbccddeeff00112233445566778899aabbccdd", "magnet_uri": "m", "indexer": "Nyaa"}"#;
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

    let body = r#"{"torrent_name": "Show", "info_hash": "aabbccddeeff00112233445566778899aabbccdd", "indexer": "Nyaa"}"#;
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
async fn no_tracked_series_skips_with_200() {
    // Indexer matches but no series in the library matches the
    // release title. Skip with 200, log it.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    seed_indexer(&db, "Nyaa").await;
    let state = build_test_app_state(db, None);
    rebuild_indexer_cache(&state).await;
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Some Random Title", "info_hash": "aabbccddeeff00112233445566778899aabbccdd", "magnet_uri": "magnet:m", "indexer": "Nyaa"}"#;
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
        "aabbccddeeff00112233445566778899aabbccdd",
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

    let body = r#"{"torrent_name": "Test Show - 01", "info_hash": "aabbccddeeff00112233445566778899aabbccdd", "magnet_uri": "magnet:m", "indexer": "Nyaa"}"#;
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
        "deadbeef99deadbeef99deadbeef99deadbeef99",
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

    let body = r#"{"torrent_name": "Test Show - 01", "info_hash": "deadbeef99deadbeef99deadbeef99deadbeef99", "magnet_uri": "magnet:m", "indexer": "Nyaa"}"#;
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

    let body = r#"{"torrent_name": "Test Show - 01 [BD 1080p]", "info_hash": "feedfacefeedfacefeedfacefeedfacefeedface", "magnet_uri": "magnet:m", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("download client"), "body: {body}");
}

#[tokio::test]
async fn an_info_hash_that_is_not_a_hash_returns_400() {
    // qBittorrent reads `all` as every torrent; it must never be stored.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let state = build_test_app_state(db, None);
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Test Show - 01", "info_hash": "all", "torrent_url": "https://tracker.example/t/1.torrent", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("info_hash"), "body: {body}");
}

/// Accepts every add; nothing else is called on the push path.
struct AcceptingClient;

#[async_trait::async_trait]
impl crate::services::download_client::DownloadClient for AcceptingClient {
    async fn test(&self) -> Result<String, String> {
        Ok("ok".into())
    }
    async fn add_torrent(
        &self,
        _u: &str,
        _h: &str,
    ) -> Result<crate::services::download_client::AddOutcome, String> {
        Ok(crate::services::download_client::AddOutcome::Added)
    }
    async fn add_torrent_with_file_filter(
        &self,
        _u: &str,
        _h: &str,
        _p: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<crate::services::download_client::SelectiveOutcome, String> {
        Ok(crate::services::download_client::SelectiveOutcome::FullDownload)
    }
    async fn list_scoped(
        &self,
    ) -> Result<Vec<crate::services::download_client::DownloadItem>, String> {
        Ok(vec![])
    }
    async fn get_files(
        &self,
        _h: &str,
    ) -> Result<Vec<crate::services::download_client::DownloadFile>, String> {
        Ok(vec![])
    }
    async fn pause(&self, _h: &str) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self, _h: &str) -> Result<(), String> {
        Ok(())
    }
    async fn delete(&self, _h: &str, _df: bool) -> Result<(), String> {
        Ok(())
    }
    async fn set_file_wanted(&self, _h: &str, _f: &[usize], _w: bool) -> Result<(), String> {
        Ok(())
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

#[tokio::test]
async fn a_push_without_a_hash_records_the_torrents_own() {
    // With no hash the grab could only be found in the client by name,
    // which a single-file torrent named after its file never matched.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let info = b"d6:lengthi1e4:name5:a.mkv12:piece lengthi16384e6:pieces0:e";
    let mut torrent = b"d4:info".to_vec();
    torrent.extend_from_slice(info);
    torrent.push(b'e');
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(info);
    let expected = hasher.digest().to_string();

    let tracker = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/t/1.torrent"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(torrent))
        .mount(&tracker)
        .await;

    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    seed_indexer(&db, "Nyaa").await;
    seed_series(&db).await;
    let state = build_test_app_state(db.clone(), Some(std::sync::Arc::new(AcceptingClient)));
    rebuild_indexer_cache(&state).await;
    let app = autobrr_webhook_router(state);

    let body = serde_json::json!({
        "torrent_name": "Test Show - 01 [1080p]",
        "torrent_url": format!("{}/t/1.torrent", tracker.uri()),
        "indexer": "Nyaa",
    })
    .to_string();
    let (status, body) = post_payload(app, &body).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let hash: String = sqlx::query_scalar("SELECT hash FROM grabbed_torrents")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(hash, expected);
}

#[tokio::test]
async fn a_magnet_push_without_a_hash_takes_the_magnets() {
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    seed_indexer(&db, "Nyaa").await;
    seed_series(&db).await;
    let state = build_test_app_state(db.clone(), Some(std::sync::Arc::new(AcceptingClient)));
    rebuild_indexer_cache(&state).await;
    let app = autobrr_webhook_router(state);
    let body = r#"{"torrent_name": "Test Show - 02", "magnet_uri": "magnet:?xt=urn:btih:C12FE1C06BBA254A9DC9F519B335AA7C1367A88A", "indexer": "Nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let hash: String = sqlx::query_scalar("SELECT hash FROM grabbed_torrents")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(hash, "c12fe1c06bba254a9dc9f519b335aa7c1367a88a");
}

#[tokio::test]
async fn a_torrent_name_over_the_title_cap_is_refused() {
    // A huge title only exists to stress the parsers behind it
    // (anitomy aborted the process on an ~8,000-character token).
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    let app = autobrr_webhook_router(build_test_app_state(db, None));
    let name = "1".repeat(crate::services::media::MAX_RELEASE_TITLE_BYTES + 1);
    let body = serde_json::json!({
        "torrent_name": name,
        "magnet_uri": "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a",
        "indexer": "Nyaa",
    })
    .to_string();
    let (status, body) = post_payload(app, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("too long"), "{body}");
}

#[tokio::test]
async fn an_unknown_indexer_grabs_through_the_default_client() {
    // autobrr sends its own identifier ("animebytes"), the user named
    // the Ryokan row something else. The push used to be skipped as
    // "indexer not configured"; it is grabbed with the default client
    // and no seed rules, the way Sonarr's push API takes any release.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    seed_indexer(&db, "Prowlarr AnimeBytes").await;
    seed_series(&db).await;
    let state = build_test_app_state(db.clone(), Some(std::sync::Arc::new(AcceptingClient)));
    rebuild_indexer_cache(&state).await;
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Test Show - 03 [1080p]", "magnet_uri": "magnet:?xt=urn:btih:D12FE1C06BBA254A9DC9F519B335AA7C1367A88A", "indexer": "animebytes"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("grabbed"), "body: {body}");
    let (indexer_id, client_id, seed_rules): (Option<i64>, Option<i64>, bool) = sqlx::query_as(
        "SELECT indexer_id, download_client_id, respect_seed_rules FROM grabbed_torrents",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(indexer_id, None);
    assert_eq!(client_id, Some(1), "the default torrent client");
    assert!(!seed_rules, "no indexer, no seed rules");
}

#[tokio::test]
async fn a_nyaa_push_follows_the_built_in_nyaa_client() {
    // Nyaa is built in, not an indexer row, so autobrr's "nyaa" never
    // matched one. It now routes like a Nyaa grab: the Nyaa client
    // setting first.
    let db = in_memory_pool().await;
    seed_autobrr_enabled(&db, KEY).await;
    // Only the Nyaa card's own save writes this column.
    sqlx::query("UPDATE config SET nyaa_download_client_id = 2 WHERE id = 1")
        .execute(&db)
        .await
        .unwrap();
    seed_series(&db).await;
    let state = build_test_app_state(db.clone(), None);
    {
        let mut clients: std::collections::HashMap<
            i64,
            std::sync::Arc<dyn crate::services::download_client::DownloadClient>,
        > = std::collections::HashMap::new();
        clients.insert(1, std::sync::Arc::new(AcceptingClient));
        clients.insert(2, std::sync::Arc::new(AcceptingClient));
        *state.download_clients.write().await = std::sync::Arc::new(crate::DownloadClientPool {
            clients,
            default_torrent_id: Some(1),
            default_usenet_id: None,
        });
    }
    let app = autobrr_webhook_router(state);

    let body = r#"{"torrent_name": "Test Show - 04 [1080p]", "magnet_uri": "magnet:?xt=urn:btih:E12FE1C06BBA254A9DC9F519B335AA7C1367A88A", "indexer": "nyaa"}"#;
    let (status, body) = post_payload(app, body).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let client_id: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(client_id, Some(2), "the client pinned for Nyaa");
}
