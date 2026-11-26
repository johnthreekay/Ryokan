//! `GET /radarr/api/v3/queue` (#325), through the shim's router and its
//! key check. The join against the download client is covered in
//! `handlers::arr_queue`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::handlers::arr_queue::test_client::{QueueClient, item};
use crate::services::anibridge;
use crate::services::download_client::{DownloadClient, DownloadItem, DownloadItemState};
use crate::test_support::{
    build_test_app_state, in_memory_pool, radarr_router_with_movie, seed_grabbed_torrent,
    seed_radarr_enabled, seed_series,
};

const KEY: &str = "test-radarr-key-01234567";
const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

async fn get_json(app: axum::Router, uri: &str) -> serde_json::Value {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-api-key", KEY)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {uri}");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("JSON")
}

fn client(items: Option<Vec<DownloadItem>>) -> Option<Arc<dyn DownloadClient>> {
    Some(Arc::new(QueueClient {
        items,
        protocol: "torrent",
        hangs: false,
    }))
}

#[tokio::test]
async fn an_empty_queue_is_still_the_paging_envelope() {
    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let app = radarr_router_with_movie(build_test_app_state(db, None));
    let body = get_json(app, "/radarr/api/v3/queue").await;
    insta::assert_json_snapshot!("radarr_queue_empty", body);
}

#[tokio::test]
async fn a_download_is_one_record_in_radarrs_shape() {
    let _gate = anibridge::TEST_CACHE_LOCK.lock().await;
    anibridge::seed_external_mappings_for_tests(&[], &[]).await;

    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let movie = seed_series(&db, 100, "Film").await;
    let grab = seed_grabbed_torrent(&db, movie, HASH, "[Group] Film [BD 1080p]", &[1]).await;
    sqlx::query("UPDATE grabbed_torrents SET grabbed_at = '2026-11-26 10:00:00' WHERE id = ?")
        .bind(grab)
        .execute(&db)
        .await
        .unwrap();
    let paused = item(
        HASH,
        "[Group] Film [BD 1080p].mkv",
        0.4,
        DownloadItemState::Paused,
    );
    let app = radarr_router_with_movie(build_test_app_state(db, client(Some(vec![paused]))));

    let body = get_json(app, "/radarr/api/v3/queue").await;
    assert_eq!(body["records"][0]["movieId"], movie);
    insta::assert_json_snapshot!("radarr_queue_record", body);

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn rows_that_are_one_tmdb_movie_share_a_download() {
    // A pack of several episodes is still one record here, and it is
    // listed under each library row the mappings file under the movie.
    let _gate = anibridge::TEST_CACHE_LOCK.lock().await;
    anibridge::seed_external_mappings_for_tests(&[], &[]).await;
    anibridge::seed_tmdb_movies_for_tests(&[(555, Some(100), None), (555, Some(200), None)]).await;

    let db = in_memory_pool().await;
    seed_radarr_enabled(&db, KEY).await;
    let part_one = seed_series(&db, 100, "Film Part 1").await;
    let part_two = seed_series(&db, 200, "Film Part 2").await;
    let unrelated = seed_series(&db, 300, "Series").await;
    seed_grabbed_torrent(&db, part_two, HASH, "[Group] Film Part 2", &[1, 2]).await;
    let downloading = item(HASH, "x", 0.5, DownloadItemState::Downloading);
    let app = radarr_router_with_movie(build_test_app_state(db, client(Some(vec![downloading]))));

    let body = get_json(app, "/radarr/api/v3/queue").await;
    let movies: Vec<i64> = body["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["movieId"].as_i64().unwrap())
        .collect();
    assert_eq!(movies, vec![part_two, part_one]);
    assert!(!movies.contains(&unrelated));

    anibridge::clear_cache_for_tests().await;
}
