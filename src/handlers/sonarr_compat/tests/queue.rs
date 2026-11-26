//! `GET /api/v3/queue` (#325), through the shim's router and its key
//! check. The join against the download client is covered next to it
//! in `handlers::arr_queue`; these pin what Seerr's download-sync job
//! reads: the envelope, the record, and which `seriesId` a download is
//! reported under.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::SqlitePool;
use tower::ServiceExt;

use crate::handlers::arr_queue::test_client::{QueueClient, item};
use crate::services::anibridge;
use crate::services::download_client::{DownloadClient, DownloadItem, DownloadItemState};
use crate::test_support::{
    build_test_app_state, in_memory_pool, seed_grabbed_torrent, seed_series, seed_sonarr_enabled,
    sonarr_router_with_series,
};

const KEY: &str = "test-sonarr-key-01234567";
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

/// A series listed under `tvdb` / `season`, as one Seerr added.
async fn seed_listed(db: &SqlitePool, anilist_id: i64, title: &str, tvdb: i64, season: i32) -> i64 {
    let id = seed_series(db, anilist_id, title).await;
    sqlx::query("UPDATE series SET tvdb_id = ?, tvdb_season = ? WHERE id = ?")
        .bind(tvdb)
        .bind(season)
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    id
}

/// A pending grab with a fixed grab time, so `added` is stable.
async fn seed_grab(db: &SqlitePool, series_id: i64, hash: &str, name: &str, episodes: &[i32]) {
    let id = seed_grabbed_torrent(db, series_id, hash, name, episodes).await;
    sqlx::query("UPDATE grabbed_torrents SET grabbed_at = '2026-11-26 10:00:00' WHERE id = ?")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_empty_queue_is_still_the_paging_envelope() {
    // Seerr reads `response.data.records`; a bare `[]` would be
    // `undefined` there and fail the sync.
    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let app = sonarr_router_with_series(build_test_app_state(db, None));
    let body = get_json(app, "/api/v3/queue?includeEpisode=true").await;
    insta::assert_json_snapshot!("sonarr_queue_empty", body);
}

#[tokio::test]
async fn a_download_is_one_record_in_sonarrs_shape() {
    let _gate = anibridge::TEST_CACHE_LOCK.lock().await;
    anibridge::seed_tvdb_placements_for_tests(&[]).await;

    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let series_id = seed_listed(&db, 100, "Show", 4242, 2).await;
    seed_grab(&db, series_id, HASH, "[Group] Show - 03 [1080p]", &[3]).await;
    let downloading = item(
        HASH,
        "[Group] Show - 03 [1080p].mkv",
        0.25,
        DownloadItemState::Downloading,
    );
    let app = sonarr_router_with_series(build_test_app_state(db, client(Some(vec![downloading]))));

    let body = get_json(app.clone(), "/api/v3/queue?includeEpisode=true").await;
    assert_eq!(body["records"][0]["seriesId"], series_id);
    insta::assert_json_snapshot!("sonarr_queue_record", body);

    // Sonarr attaches the episode only when asked.
    let bare = get_json(app, "/api/v3/queue").await;
    assert!(bare["records"][0].get("episode").is_none(), "{bare}");
    assert_eq!(bare["records"][0]["seasonNumber"], 2);

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn a_download_is_reported_under_every_row_of_its_tvdb_show() {
    // Two cours are two library rows and one TVDB season. Seerr holds
    // one server id for the show, whichever row it read last, so the
    // second cour's download has to be listed under the first cour's
    // id too, with the episode numbered as TVDB numbers it.
    let _gate = anibridge::TEST_CACHE_LOCK.lock().await;
    anibridge::seed_tvdb_placements_for_tests(&[(200, 4242, 1, Some(12))]).await;

    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let first = seed_listed(&db, 100, "Show", 4242, 1).await;
    let second = seed_listed(&db, 200, "Show Part 2", 4242, 1).await;
    let other = seed_listed(&db, 300, "Other", 777, 1).await;
    seed_grab(&db, second, HASH, "[Group] Show Part 2 - 03", &[3]).await;
    let downloading = item(HASH, "x", 0.5, DownloadItemState::Downloading);
    let app = sonarr_router_with_series(build_test_app_state(db, client(Some(vec![downloading]))));

    let body = get_json(app, "/api/v3/queue?includeEpisode=true").await;
    let records = body["records"].as_array().unwrap();
    let listed: Vec<(i64, i64, i64)> = records
        .iter()
        .map(|r| {
            (
                r["seriesId"].as_i64().unwrap(),
                r["episode"]["seasonNumber"].as_i64().unwrap(),
                r["episode"]["episodeNumber"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(listed, vec![(second, 1, 15), (first, 1, 15)]);
    assert!(listed.iter().all(|(id, _, _)| *id != other));
    assert_eq!(body["totalRecords"], 2);
    // One download, so one `downloadId`, and a record id apiece.
    assert_eq!(records[0]["downloadId"], records[1]["downloadId"]);
    assert_ne!(records[0]["id"], records[1]["id"]);

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn an_entry_spanning_several_tvdb_seasons_reports_each_episode_in_its_own() {
    // One AniList entry, four TVDB seasons (Gintama's shape): 1-49,
    // 50-99, 100-150, 151-201. The row is listed under one season, but
    // an episode is reported where TVDB has it.
    let _gate = anibridge::TEST_CACHE_LOCK.lock().await;
    anibridge::seed_tvdb_placements_for_tests(&[
        (918, 79895, 1, Some(0)),
        (918, 79895, 2, Some(-49)),
        (918, 79895, 3, Some(-99)),
        (918, 79895, 4, Some(-150)),
    ])
    .await;

    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let series_id = seed_listed(&db, 918, "Long Show", 79895, 3).await;
    seed_grab(
        &db,
        series_id,
        HASH,
        "[Group] Long Show (pack)",
        &[10, 49, 50, 160],
    )
    .await;
    let downloading = item(HASH, "pack", 0.5, DownloadItemState::Downloading);
    let app = sonarr_router_with_series(build_test_app_state(db, client(Some(vec![downloading]))));

    let body = get_json(app, "/api/v3/queue?includeEpisode=true").await;
    let records = body["records"].as_array().unwrap();
    let slots: Vec<(i64, i64)> = records
        .iter()
        .map(|r| {
            assert_eq!(r["seasonNumber"], r["episode"]["seasonNumber"]);
            (
                r["episode"]["seasonNumber"].as_i64().unwrap(),
                r["episode"]["episodeNumber"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(slots, vec![(1, 10), (1, 49), (2, 1), (4, 10)]);
    let ids: std::collections::HashSet<i64> =
        records.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids.len(), 4, "s01e10 and s04e10 are two records");

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn a_pack_is_a_record_per_episode_sharing_one_download_id() {
    let _gate = anibridge::TEST_CACHE_LOCK.lock().await;
    anibridge::seed_tvdb_placements_for_tests(&[]).await;

    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let series_id = seed_listed(&db, 100, "Show", 4242, 1).await;
    seed_grab(
        &db,
        series_id,
        HASH,
        "[Group] Show (01-03) [BD]",
        &[1, 2, 3],
    )
    .await;
    let downloading = item(HASH, "pack", 0.5, DownloadItemState::Downloading);
    let app = sonarr_router_with_series(build_test_app_state(db, client(Some(vec![downloading]))));

    let body = get_json(app, "/api/v3/queue?includeEpisode=true").await;
    let records = body["records"].as_array().unwrap();
    let episodes: Vec<i64> = records
        .iter()
        .map(|r| r["episode"]["episodeNumber"].as_i64().unwrap())
        .collect();
    assert_eq!(episodes, vec![1, 2, 3]);
    let ids: std::collections::HashSet<i64> =
        records.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids.len(), 3);
    for r in records {
        assert_eq!(r["downloadId"], HASH.to_uppercase());
        // Each record carries the whole download's size, as Sonarr's do.
        assert_eq!(
            (r["size"].as_i64(), r["sizeleft"].as_i64()),
            (Some(1000), Some(500))
        );
    }

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn a_client_that_is_down_does_not_fail_the_sync() {
    let _gate = anibridge::TEST_CACHE_LOCK.lock().await;
    anibridge::seed_tvdb_placements_for_tests(&[]).await;

    let db = in_memory_pool().await;
    seed_sonarr_enabled(&db, KEY).await;
    let series_id = seed_listed(&db, 100, "Show", 4242, 1).await;
    seed_grab(&db, series_id, HASH, "[Group] Show - 03", &[3]).await;
    let app = sonarr_router_with_series(build_test_app_state(db, client(None)));

    let body = get_json(app, "/api/v3/queue?includeEpisode=true").await;
    let record = &body["records"][0];
    assert_eq!(record["status"], "downloadClientUnavailable");
    assert_eq!(record["size"], 0);
    assert!(record.get("estimatedCompletionTime").is_none(), "{record}");

    anibridge::clear_cache_for_tests().await;
}
