//! Wiremock-driven coverage for the Sonarr/Radarr shim's resource-tier
//! `add_series` / `add_movie` endpoints. Inline tests in
//! `handlers/{sonarr,radarr}_compat/tests/{series,movie}.rs` cover the
//! rest of the shim surface (lookup stub, list, get, update, command);
//! this file fills in the most-hit Seerr path: `POST /series` and
//! `POST /movie` with a valid TVDB/TMDB id mapped through anibridge.
//!
//! Self-contained orchestration: anibridge's TVDB/TMDB index is seeded
//! via `anibridge::seed_external_mappings_for_tests` (no on-disk
//! cache fetch, no network), then AL's GraphQL detail endpoint is
//! redirected to a wiremock server via `RYOKAN_ANILIST_API_BASE`.
//! This is the same shape as `tests/external_sync_e2e.rs` and
//! `tests/metadata_sync_e2e.rs` — three integration-test crates each
//! get their own process so the env-var override doesn't bleed across
//! workspace tests.
//!
//! Tests within this binary share an env-var serializer to keep the
//! `RYOKAN_ANILIST_API_BASE` write race-free.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ryokan::models::series;
use ryokan::services::anibridge;
use ryokan::services::anilist;
use ryokan::test_support::{
    build_test_app_state, in_memory_pool, radarr_router_with_movie, seed_radarr_enabled,
    seed_sonarr_enabled, sonarr_router_with_series,
};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::LazyLock;
use tokio::sync::Mutex;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SONARR_KEY: &str = "test-sonarr-key-arr-e2e-1";
const RADARR_KEY: &str = "test-radarr-key-arr-e2e-1";

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// AL Media-detail response. Self-contained shape (MOVIE format,
/// `episodes: 1`, empty relations) so `metadata_sync::build_episode_cache`
/// and `hydrate_relation_tree` don't fan out to Jikan or to extra AL
/// queries.
fn media_detail_response(id: i64, title: &str) -> serde_json::Value {
    json!({
        "data": {
            "Media": {
                "id": id,
                "idMal": null,
                "title": {
                    "romaji": title,
                    "english": title,
                    "native": title,
                },
                "synonyms": [],
                "coverImage": {
                    "large": "https://example/cover.jpg",
                    "extraLarge": "https://example/cover-xl.jpg",
                },
                "bannerImage": "https://example/banner.jpg",
                "format": "MOVIE",
                "status": "FINISHED",
                "episodes": 1,
                "duration": 120,
                "season": null,
                "seasonYear": 2022,
                "endDate": { "year": 2022 },
                "description": "Wiremock fixture.",
                "genres": [],
                "averageScore": null,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

async fn seed_sonarr_state(db: &SqlitePool) {
    seed_sonarr_enabled(db, SONARR_KEY).await;
}

async fn seed_radarr_state(db: &SqlitePool) {
    seed_radarr_enabled(db, RADARR_KEY).await;
}

async fn post_json(
    app: axum::Router,
    uri: &str,
    api_key: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-api-key", api_key)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

// ─── Sonarr add_series ──────────────────────────────────────────────

#[tokio::test]
async fn sonarr_add_series_creates_db_row_and_returns_payload() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;

    // Seed anibridge: TVDB 4242 season 1 → AL 88888, the season the
    // request below monitors.
    anibridge::seed_external_mappings_for_tests(
        &[(4242, 1, Some(88888), None)],
        &[(4242, 1, Some(88888), None)],
    )
    .await;

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(media_detail_response(88888, "Seerr Add")),
        )
        .mount(&mock)
        .await;

    // SAFETY: ENV_LOCK serializes env-var access within this binary;
    // each `tests/*.rs` file is its own process, so cross-binary
    // races aren't possible.
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let app = sonarr_router_with_series(state);

    let (status, body) = post_json(
        app,
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 4242,
            "title": "Seerr Add",
            "seasons": [{"seasonNumber": 1, "monitored": true}],
            "monitored": true,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // Response carries the Sonarr-shape payload built from the
    // newly-inserted series row.
    assert_eq!(body["title"], "Seerr Add");
    assert_eq!(body["seriesType"], "anime");

    // DB-side: the `series` row landed with the AL id from the mapping.
    let row = series::get_by_anilist_id(&db, 88888)
        .await
        .unwrap()
        .expect("series row should exist after add_series");
    assert_eq!(row.title_english, "Seerr Add");
    assert_eq!(row.format, "MOVIE");
    // Monitor mode pinned to "all" because seasons[0].monitored=true.
    assert_eq!(row.monitor_mode, "all");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn sonarr_add_series_returns_400_when_no_mapping_and_no_title() {
    let _gate = ENV_LOCK.lock().await;
    anibridge::clear_cache_for_tests().await;
    // Seed empty anibridge so lookup_by_tvdb / lookup_by_tmdb both
    // return empty Vecs — the title-fallback branch fires, but with
    // an empty `title` field the handler returns 400 before any AL
    // call.
    anibridge::seed_external_mappings_for_tests(&[], &[]).await;

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let state = build_test_app_state(db, None);
    let app = sonarr_router_with_series(state);

    let (status, _body) = post_json(
        app,
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 9999,
            "title": "",
            "seasons": [],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn sonarr_add_series_pins_monitor_mode_to_none_when_seerr_unmonitors() {
    // Seerr can send `seasons: [{monitored: false}]` to add but not
    // monitor. The handler maps that to MonitorMode::None on the
    // upserted row.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(
        &[(5555, 1, Some(77777), None)],
        &[(5555, 1, Some(77777), None)],
    )
    .await;

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(media_detail_response(77777, "Unmonitored Add")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let app = sonarr_router_with_series(state);

    let (status, _body) = post_json(
        app,
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 5555,
            "title": "Unmonitored Add",
            "seasons": [{"seasonNumber": 1, "monitored": false}],
            "monitored": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = series::get_by_anilist_id(&db, 77777)
        .await
        .unwrap()
        .expect("series row");
    assert_eq!(row.monitor_mode, "none");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

async fn get_json(app: axum::Router, uri: &str, api_key: &str) -> serde_json::Value {
    let req = Request::builder()
        .uri(uri)
        .header("x-api-key", api_key)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn sonarr_series_report_the_tvdb_id_seerr_resolves_them_by() {
    // Seerr's Sonarr scan resolves every listed series by `tvdbId`, and
    // asks `GET /series?tvdbId=` before declining a request whose show
    // it didn't see. The shim reported the TMDB id there and ignored the
    // filter, so every request Seerr sent was declined on the next scan.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(
        &[(4242, 2, Some(88888), None)],
        &[(9191, 1, Some(88888), None)],
    )
    .await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(media_detail_response(88888, "Seerr Add")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let app = sonarr_router_with_series(build_test_app_state(db.clone(), None));
    let (status, added) = post_json(
        app.clone(),
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 4242,
            "title": "Seerr Add",
            "seasons": [{"seasonNumber": 2, "monitored": true}],
            "monitored": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(added["tvdbId"], 4242);

    let listed = get_json(app.clone(), "/api/v3/series", SONARR_KEY).await;
    assert_eq!(listed[0]["tvdbId"], 4242, "the TVDB id, never the TMDB one");
    assert_eq!(listed[0]["tmdbId"], 9191);
    assert_eq!(
        listed[0]["seasons"][0]["seasonNumber"], 2,
        "the TVDB season"
    );
    let hit = get_json(app.clone(), "/api/v3/series?tvdbId=4242", SONARR_KEY).await;
    assert_eq!(hit.as_array().unwrap().len(), 1);
    let miss = get_json(app, "/api/v3/series?tvdbId=9191", SONARR_KEY).await;
    assert_eq!(
        miss.as_array().unwrap().len(),
        0,
        "a TMDB id is not a TVDB id"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn a_series_added_in_ryokan_reports_its_mapped_tvdb_id() {
    let _gate = ENV_LOCK.lock().await;
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(
        &[(4242, 1, Some(88888), None)],
        &[(9191, 1, Some(88888), None)],
    )
    .await;
    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 88888,
            mal_id: None,
            title: "Added Here",
            title_romaji: "Added Here",
            title_english: "Added Here",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2024),
            end_year: None,
        },
    )
    .await
    .unwrap();
    let app = sonarr_router_with_series(build_test_app_state(db, None));
    let listed = get_json(app, "/api/v3/series", SONARR_KEY).await;
    assert_eq!(listed[0]["tvdbId"], 4242);
    assert_eq!(listed[0]["tmdbId"], 9191);
    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn a_tvdb_id_that_only_matches_some_animes_tmdb_id_is_not_that_anime() {
    // The TVDB-to-TMDB fallback read an unmapped TVDB id as a TMDB id,
    // so a show the mappings don't know could add an unrelated anime.
    let _gate = ENV_LOCK.lock().await;
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(&[], &[(7070, 1, Some(55555), None)]).await;
    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let app = sonarr_router_with_series(build_test_app_state(db.clone(), None));

    let found = get_json(
        app.clone(),
        "/api/v3/series/lookup?term=tvdb:7070",
        SONARR_KEY,
    )
    .await;
    assert_eq!(found[0]["title"], "TVDB:7070", "the unmapped stub");
    let (status, _) = post_json(
        app,
        "/api/v3/series",
        SONARR_KEY,
        json!({"tvdbId": 7070, "title": "", "seasons": []}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no mapping and no title");
    assert!(
        series::get_by_anilist_id(&db, 55555)
            .await
            .unwrap()
            .is_none()
    );
    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn a_show_the_mappings_lack_keeps_the_tvdb_id_seerr_added_it_under() {
    // No mapping: the add finds the series by title, and the shim has
    // nothing to report as its TVDB id but the one Seerr sent, which it
    // now stores. Reported as 0, Seerr declined the request on its scan.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(&[], &[]).await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "Page": { "media": [{
                "id": 33333,
                "idMal": null,
                "title": { "romaji": "Unmapped Show", "english": "Unmapped Show", "native": "" },
                "coverImage": { "large": "https://example/cover.jpg" },
                "format": "TV",
                "status": "FINISHED",
                "episodes": 12,
                "seasonYear": 2024,
                "averageScore": 80,
            }] } }
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(media_detail_response(33333, "Unmapped Show")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    let app = sonarr_router_with_series(build_test_app_state(db, None));
    let (status, body) = post_json(
        app.clone(),
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 818181,
            "title": "Unmapped Show",
            "seasons": [{"seasonNumber": 3, "monitored": true}],
            "monitored": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let found = get_json(app, "/api/v3/series?tvdbId=818181", SONARR_KEY).await;
    assert_eq!(found.as_array().unwrap().len(), 1, "{found}");
    assert_eq!(found[0]["seasons"][0]["seasonNumber"], 3);

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn a_specials_entry_is_season_zero_of_its_show() {
    // TVDB files OVAs and minis under season 0, Specials, which Seerr
    // ignores. Clamped to 1, such an entry read as season 1 of its
    // parent show, and an OVA with its files on disk marked that season
    // Available. Season 0 is not an unscoped catch-all either: a
    // request for a season the mappings lack used to add the OVA.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(
        &[(4242, 1, Some(88888), None), (4242, 0, Some(88889), None)],
        &[],
    )
    .await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(media_detail_response(88888, "The Show")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_sonarr_state(&db).await;
    series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 88889,
            mal_id: None,
            title: "The Show OVA",
            title_romaji: "The Show OVA",
            title_english: "The Show OVA",
            title_native: "",
            cover_url: "",
            format: "OVA",
            status: "FINISHED",
            episodes: Some(2),
            season_year: Some(2024),
            end_year: None,
        },
    )
    .await
    .unwrap();
    let app = sonarr_router_with_series(build_test_app_state(db, None));

    let listed = get_json(app.clone(), "/api/v3/series", SONARR_KEY).await;
    assert_eq!(listed[0]["tvdbId"], 4242);
    assert_eq!(listed[0]["seasons"][0]["seasonNumber"], 0, "{listed}");
    assert_eq!(listed[0]["statistics"]["seasonCount"], 0);

    let found = get_json(
        app.clone(),
        "/api/v3/series/lookup?term=tvdb:4242",
        SONARR_KEY,
    )
    .await;
    let seasons: Vec<i64> = found[0]["seasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["seasonNumber"].as_i64().unwrap())
        .collect();
    assert_eq!(seasons, vec![0, 1], "{found}");
    assert_eq!(found[0]["statistics"]["seasonCount"], 1);

    // Season 3 isn't mapped: no Specials stand-in, so with no title to
    // search by the add is refused.
    let (status, _) = post_json(
        app,
        "/api/v3/series",
        SONARR_KEY,
        json!({
            "tvdbId": 4242,
            "title": "",
            "seasons": [{"seasonNumber": 3, "monitored": true}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

// ─── Radarr add_movie ──────────────────────────────────────────────

#[tokio::test]
async fn radarr_add_movie_creates_db_row_and_returns_payload() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(&[], &[(6060, 0, Some(11111), None)]).await;

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(media_detail_response(11111, "Seerr Movie")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_radarr_state(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let app = radarr_router_with_movie(state);

    let (status, body) = post_json(
        app,
        "/radarr/api/v3/movie",
        RADARR_KEY,
        json!({
            "tmdbId": 6060,
            "title": "Seerr Movie",
            "monitored": true,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Seerr Movie");
    // Both rating slots present (Seerr renders whichever it reads).
    assert!(body["ratings"]["imdb"]["value"].is_f64());
    assert!(body["ratings"]["tmdb"]["value"].is_f64());

    let row = series::get_by_anilist_id(&db, 11111)
        .await
        .unwrap()
        .expect("series row should exist after add_movie");
    assert_eq!(row.title_english, "Seerr Movie");
    assert_eq!(row.format, "MOVIE");
    assert_eq!(row.monitor_mode, "all");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn radarr_add_movie_returns_400_when_no_mapping_and_no_title() {
    let _gate = ENV_LOCK.lock().await;
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(&[], &[]).await;

    let db = in_memory_pool().await;
    seed_radarr_state(&db).await;
    let state = build_test_app_state(db, None);
    let app = radarr_router_with_movie(state);

    let (status, _body) = post_json(
        app,
        "/radarr/api/v3/movie",
        RADARR_KEY,
        json!({
            "tmdbId": 9999,
            "title": "",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn radarr_add_movie_pins_monitor_mode_to_none_when_seerr_unmonitors() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
    anibridge::seed_external_mappings_for_tests(&[], &[(7070, 0, Some(22222), None)]).await;

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(media_detail_response(22222, "Unmonitored Movie")),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_radarr_state(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let app = radarr_router_with_movie(state);

    let (status, _body) = post_json(
        app,
        "/radarr/api/v3/movie",
        RADARR_KEY,
        json!({
            "tmdbId": 7070,
            "title": "Unmonitored Movie",
            "monitored": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = series::get_by_anilist_id(&db, 22222)
        .await
        .unwrap()
        .expect("series row");
    assert_eq!(row.monitor_mode, "none");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
    anibridge::clear_cache_for_tests().await;
}
