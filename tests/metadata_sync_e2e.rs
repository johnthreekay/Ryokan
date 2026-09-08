//! Wiremock-driven coverage for `services::metadata_sync`. Pre-this-file
//! the only direct unit coverage was the pure helpers
//! (`is_authoritative_detail`, `title_candidates_for_series`,
//! `episode_needs_kitsu_backfill`) plus an empty-DB sweep contract.
//! These tests stand up a wiremock AL endpoint and exercise the full
//! `refresh_series_metadata` orchestration: AL fetch → series row
//! refresh → metadata_cache upsert → relations + episode cache merge.
//!
//! Mirrors the shape of `tests/external_sync_e2e.rs`, which uses the
//! same `RYOKAN_ANILIST_API_BASE` seam. A shared `tokio::sync::Mutex`
//! serializer prevents tests in this binary from racing on the
//! process-wide env var; tests in *other* binaries get their own
//! process so the seam doesn't leak across test crates.
//!
//! Each scenario is tuned to keep the wiremock self-contained:
//!   * MOVIE format + `episodes: 1` → `services::metadata_sync::
//!     build_episode_cache` early-returns without hitting Jikan, so we
//!     don't need a JIKAN_API_BASE override.
//!   * Empty `relations.edges` → `hydrate_relation_tree` fans out zero
//!     follow-up requests.
//!   * `idMal: null` + empty title fields → the AL-failure paths
//!     (5xx, rate-limit) don't fall back to Jikan / Kitsu.

use ryokan::models::{local_metadata, metadata_cache, series};
use ryokan::services::{anilist, metadata_sync};
use ryokan::test_support::in_memory_pool;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::LazyLock;
use tokio::sync::Mutex;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One-at-a-time gate around `RYOKAN_ANILIST_API_BASE` writes so
/// tokio's parallel test scheduler can't race two tests on the
/// process-wide env var.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Detail response for AL `Media(id: $id, idMal: $idMal)`. MOVIE
/// format + 1 episode keeps the downstream `build_episode_cache` from
/// reaching Jikan — see the module header.
fn media_detail_response(id: i64) -> serde_json::Value {
    json!({
        "data": {
            "Media": {
                "id": id,
                "idMal": null,
                "title": {
                    "romaji": "Test Movie",
                    "english": "Test Movie EN",
                    "native": "テスト"
                },
                "synonyms": [],
                "coverImage": {
                    "large": "https://example/cover.jpg",
                    "extraLarge": "https://example/cover-xl.jpg"
                },
                "bannerImage": "https://example/banner.jpg",
                "format": "MOVIE",
                "status": "FINISHED",
                "episodes": 1,
                "duration": 120,
                "season": null,
                "seasonYear": 2020,
                "endDate": { "year": 2020 },
                "description": "Self-contained AL fixture for metadata_sync.",
                "genres": ["Action", "Drama"],
                "averageScore": 75,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

/// Seed a single tracked series row and return its id. Title fields
/// are deliberately blank so the failure-path fallback to Kitsu's
/// title-fuzz search no-ops (no candidates → final AL retry → same
/// Err returned).
async fn seed_minimal_series(db: &SqlitePool, anilist_id: i64) -> i64 {
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name, status, format) \
         VALUES (?, '', '', '', 'FINISHED', 'MOVIE')",
    )
    .bind(anilist_id)
    .execute(db)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM series WHERE anilist_id = ?")
        .bind(anilist_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// TV variant of the seed helper. Format = TV so build_episode_cache's
/// `episodic_format` gate fires and the function reaches the
/// jikan/kitsu episode-titles fetch (the surface this file's TV
/// fixture wants to exercise).
async fn seed_minimal_series_tv(db: &SqlitePool, anilist_id: i64) -> i64 {
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name, status, format) \
         VALUES (?, '', '', '', 'FINISHED', 'TV')",
    )
    .bind(anilist_id)
    .execute(db)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM series WHERE anilist_id = ?")
        .bind(anilist_id)
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn refresh_series_metadata_writes_cache_on_happy_al_response() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    // The Referer matcher pins the header on the wire: AniList's
    // "temporarily disabled" 403 refuses any request without one, so a
    // call site that bypasses `anilist_post` would 404 here instead of
    // quietly landing on the MAL fallback in production.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("referer", anilist::ANILIST_REFERER))
        .and(body_string_contains("Media(id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(media_detail_response(2026)))
        .mount(&mock)
        .await;
    // Jikan must NOT be called for MOVIE+episodes=1: episodic_format
    // is false AND ep_count > 1 is false, so should_fetch_jikan is
    // false. Pins line 232's `delete !` on episodic_format (mutation
    // would set it to true, triggering Jikan) and line 233's `> with
    // >=` (mutation would let ep_count=1 satisfy the gate).
    Mock::given(method("GET"))
        .and(path("/anime/1/episodes")) // unreached path, just a sentinel
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;
    // Wide-path Jikan match — any /anime/*/episodes call should fail.
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"^/anime/\d+/episodes$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;
    // Kitsu must NOT be called either (ep_count > 1 is false).
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;

    // SAFETY: serialized via ENV_LOCK; no concurrent reader/writer
    // within this binary, and other test binaries get their own
    // process.
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 2026).await;
    let tracked = series::get_by_id(&db, series_id)
        .await
        .unwrap()
        .expect("seeded series exists");

    let detail = metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("refresh should succeed against the wiremock fixture");

    assert_eq!(detail.id, 2026);
    assert_eq!(detail.title_english, "Test Movie EN");
    assert_eq!(detail.format, "MOVIE");

    // metadata_cache row written inline by refresh_series_metadata_inner.
    let cached = metadata_cache::get_by_series_id(&db, series_id)
        .await
        .unwrap()
        .expect("metadata_cache row should exist");
    assert_eq!(cached.detail.id, 2026);
    assert_eq!(cached.detail.title_english, "Test Movie EN");

    // series_genres side table populated from detail.genres.
    let genres: Vec<String> =
        sqlx::query_scalar("SELECT genre FROM series_genres WHERE series_id = ? ORDER BY genre")
            .bind(series_id)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(genres, vec!["Action".to_string(), "Drama".to_string()]);

    // Series core columns refreshed from the AL detail.
    let refreshed = series::get_by_id(&db, series_id)
        .await
        .unwrap()
        .expect("series row");
    assert_eq!(refreshed.title_english, "Test Movie EN");
    assert_eq!(refreshed.season_year, Some(2020));
    assert_eq!(refreshed.end_year, Some(2020));
    assert_eq!(refreshed.episodes, Some(1));

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_returns_error_on_al_5xx_with_no_fallback_signal() {
    // 5xx is the "AL is down" case — `fetch_live_detail` would
    // normally fall back to MAL/Kitsu, but a series with no mal_id
    // and no title candidates has nowhere to fall back to. The final
    // arm re-calls AL, hits the same 5xx, and returns Err.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 7777).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    let result = metadata_sync::refresh_series_metadata(&db, &tracked, false).await;
    assert!(result.is_err(), "5xx without fallback signal must Err");

    // No cache row was written when the fetch failed.
    let cached = metadata_cache::get_by_series_id(&db, series_id)
        .await
        .unwrap();
    assert!(cached.is_none(), "5xx must not poison metadata_cache");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_returns_rate_limit_error_on_al_429() {
    // 429 is the load-bearing case the AL state machine guards
    // against — a rate-limit must NOT silently substitute MAL data
    // (would burn through Jikan's 3 req/s budget on every refresh
    // sweep). Caller gets back an `is_rate_limit_error`-tagged Err
    // and the metadata refresh defers.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    // AL signals the throttle via X-RateLimit-Remaining=0 +
    // X-RateLimit-Reset=<epoch> on 429. The state machine reads those
    // headers and computes the cooldown.
    let reset_at = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60) as i64;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("X-RateLimit-Limit", "30")
                .insert_header("X-RateLimit-Remaining", "0")
                .insert_header("X-RateLimit-Reset", reset_at.to_string()),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 8888).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    let result = metadata_sync::refresh_series_metadata(&db, &tracked, false).await;
    let err = result.expect_err("429 must surface as Err");
    assert!(
        anilist::is_rate_limit_error(&err),
        "expected rate-limit-tagged error, got: {err}"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

/// MOVIE-format AL response with caller-controlled title fields. The
/// existing `media_detail_response` hard-codes "Test Movie" / "Test
/// Movie EN" / "テスト" — handy for the happy-path test, but unable
/// to exercise the `if !detail.title_english.trim().is_empty() ...
/// else if !detail.title_romaji ...` fallback chain in
/// `build_episode_cache`. This lets a test populate any subset of
/// the three title slots.
fn movie_detail_response_with_titles(
    id: i64,
    romaji: &str,
    english: &str,
    native: &str,
) -> serde_json::Value {
    json!({
        "data": {
            "Media": {
                "id": id,
                "idMal": null,
                "title": {
                    "romaji": romaji,
                    "english": english,
                    "native": native,
                },
                "synonyms": [],
                "coverImage": {
                    "large": "https://example/cover.jpg",
                    "extraLarge": "https://example/cover-xl.jpg"
                },
                "bannerImage": "https://example/banner.jpg",
                "format": "MOVIE",
                "status": "FINISHED",
                "episodes": 1,
                "duration": 120,
                "season": null,
                "seasonYear": 2020,
                "endDate": { "year": 2020 },
                "description": "Title-fallback fixture for build_episode_cache.",
                "genres": [],
                "averageScore": null,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

/// AL fixture for a TV-format show that build_episode_cache will
/// follow up on with a Jikan episodes fetch. `idMal` is set so the
/// `should_fetch_jikan` branch fires; `episodes: 3` keeps the response
/// payload + downstream loops short.
fn tv_media_detail_response(id: i64, mal_id: i64) -> serde_json::Value {
    tv_media_detail_response_with_format(id, mal_id, "TV", 3)
}

/// Caller-controlled variant of `tv_media_detail_response`. Lets tests
/// pin specific (format, episode-count) combinations needed to exercise
/// the boolean assembly of `should_fetch_jikan` (line 233 of
/// build_episode_cache: `episodic_format || ep_count > 1`).
fn tv_media_detail_response_with_format(
    id: i64,
    mal_id: i64,
    format: &str,
    episodes: i32,
) -> serde_json::Value {
    json!({
        "data": {
            "Media": {
                "id": id,
                "idMal": mal_id,
                "title": {
                    "romaji": "Test TV Show",
                    "english": "Test TV Show EN",
                    "native": "テスト"
                },
                "synonyms": [],
                "coverImage": {
                    "large": "https://example/cover.jpg",
                    "extraLarge": "https://example/cover-xl.jpg"
                },
                "bannerImage": "https://example/banner.jpg",
                "format": format,
                "status": "FINISHED",
                "episodes": episodes,
                "duration": 24,
                "season": "WINTER",
                "seasonYear": 2024,
                "endDate": { "year": 2024 },
                "description": "TV-shape fixture for the build_episode_cache path.",
                "genres": ["Action"],
                "averageScore": 80,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

#[tokio::test]
async fn refresh_series_metadata_tv_format_merges_jikan_episode_titles() {
    // Drives the build_episode_cache path that the existing happy-path
    // test (MOVIE format) deliberately skips. With format=TV +
    // idMal=Some(...), `should_fetch_jikan` fires and the function
    // hits Jikan for the per-episode titles. Pins the
    //
    //     jikan_eps.remove(&ep_num).map(...).or_else(...)
    //
    // merge ladder at line ~304: the Jikan title takes precedence
    // when present, the fallback chain only fires when Jikan is empty.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(tv_media_detail_response(5555, 99999)),
        )
        .mount(&mock)
        .await;

    // Jikan episodes fixture — 3 episodes with distinct titles so the
    // assertions can verify the per-episode title round-trips. The
    // .expect(1..) pins line 233's `should_fetch_jikan = episodic_format
    // || ep_count > 1` boolean: a mutation flipping `||` to `&&` would
    // need BOTH conditions true, but TV+ep_count=3 has both anyway —
    // so the call-count distinguishes the gate from a `delete !` that
    // inverts episodic_format.
    Mock::given(method("GET"))
        .and(path("/anime/99999/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "mal_id": 1, "episode_id": 1, "title": "Pilot Episode",
                  "aired": "2024-01-01T00:00:00+00:00" },
                { "mal_id": 2, "episode_id": 2, "title": "Second Steps",
                  "aired": "2024-01-08T00:00:00+00:00" },
                { "mal_id": 3, "episode_id": 3, "title": "Resolution",
                  "aired": "2024-01-15T00:00:00+00:00" },
            ]
        })))
        .expect(1..)
        .mount(&mock)
        .await;
    // Kitsu must NOT be called: with all Jikan titles populated,
    // episode_needs_kitsu_backfill returns false → should_try_kitsu
    // is false → no Kitsu fetch. Pins line 251's
    // `.map(|info| !info.title.trim().is_empty())` closure: a
    // mutation `delete !` would invert it (true when title IS empty),
    // making episode_needs_kitsu_backfill return true even with all
    // titles present, triggering an unwanted Kitsu fetch that
    // .expect(0) catches.
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series_tv(&db, 5555).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    let detail = metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("TV refresh should succeed against the wiremock fixture");
    assert_eq!(detail.id, 5555);
    assert_eq!(detail.format, "TV");
    assert_eq!(detail.id_mal, Some(99999));

    // The episode cache must round-trip the Jikan titles.
    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    assert_eq!(episodes.len(), 3, "all 3 episodes must be cached");
    assert_eq!(
        episodes.get(&1).map(|e| e.title.as_str()),
        Some("Pilot Episode")
    );
    assert_eq!(
        episodes.get(&2).map(|e| e.title.as_str()),
        Some("Second Steps")
    );
    assert_eq!(
        episodes.get(&3).map(|e| e.title.as_str()),
        Some("Resolution")
    );
    // Source label must be `jikan` since that's where the title came
    // from. Pins the per-episode source-attribution column the UI
    // uses to badge "from MAL/Jikan" vs "from AL/Kitsu/series".
    assert_eq!(
        episodes.get(&1).map(|e| e.source.as_str()),
        Some("jikan"),
        "Jikan-sourced episodes must be tagged accordingly"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_movie_with_single_episode_skips_jikan_fetch() {
    // Pins line 232's `delete !` on `episodic_format = !matches!(...)`
    // and line 233's `> with >=` on `ep_count > 1`. Both control
    // should_fetch_jikan = episodic_format || ep_count > 1.
    //
    // With MOVIE+episodes=1: original gate is false || false = false;
    // mutated either side flips at least one operand to true. To
    // observe the difference, the AL response must carry idMal=Some(_)
    // so that fetch_live_detail_for_ids reaches Jikan via mal_id when
    // the gate fires (without idMal, fetch_episode_titles_for_detail
    // short-circuits regardless of the gate, hiding the mutation).
    //
    // The .expect(0) on the Jikan mock is the assertion: any unwanted
    // call (mutation triggers the gate) fails the test on mock drop.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    let mut detail = media_detail_response(7200);
    // Set idMal so the gate's outcome is observable via Jikan calls.
    detail["data"]["Media"]["idMal"] = serde_json::json!(72001);
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(detail))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"^/anime/\d+/episodes$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 7200).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("MOVIE+1ep refresh succeeds without Jikan fetch");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_movie_format_with_multi_episodes_still_fetches_jikan() {
    // Pins line 233's OR operator: `should_fetch_jikan = episodic_format
    // || ep_count > 1`. A mutation `||→&&` would require BOTH conditions
    // (episodic AND ep_count>1). MOVIE format makes episodic_format
    // false; episodes=3 makes ep_count>1 true. Original: should_fetch
    // = false || true = true → Jikan called. Mutated: false && true =
    // false → Jikan NOT called.
    //
    // Also pins line 233's `>` operator on ep_count: `replace > with
    // <=/==/<` would skip the Jikan fetch for ep_count=3.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            tv_media_detail_response_with_format(7100, 71001, "MOVIE", 3),
        ))
        .mount(&mock)
        .await;
    // Jikan MUST be called. Mutations to either OR or > would skip it.
    Mock::given(method("GET"))
        .and(path("/anime/71001/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "mal_id": 1, "episode_id": 1, "title": "Part One",
                  "aired": "2024-01-01T00:00:00+00:00" },
                { "mal_id": 2, "episode_id": 2, "title": "Part Two",
                  "aired": "2024-01-08T00:00:00+00:00" },
                { "mal_id": 3, "episode_id": 3, "title": "Part Three",
                  "aired": "2024-01-15T00:00:00+00:00" },
            ]
        })))
        .expect(1..)
        .mount(&mock)
        .await;
    // Kitsu must NOT be called (ep_count > 1 AND Jikan returned all
    // titles → no backfill needed).
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 7100).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("MOVIE+ep_count>1 refresh succeeds via Jikan path");

    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    assert_eq!(episodes.len(), 3);
    assert_eq!(
        episodes.get(&1).map(|e| e.title.as_str()),
        Some("Part One"),
        "MOVIE+multi-episode must hit the Jikan-first merge ladder"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_tv_format_falls_back_to_series_title_when_jikan_empty() {
    // When Jikan returns no episodes (e.g. the show isn't indexed in
    // MAL despite an idMal being set), build_episode_cache's per-
    // episode loop falls through to the empty `local` branch and the
    // fallback_title kicks in. For TV format with episodes > 1, the
    // fallback_title is empty (line ~272: `String::new()` because the
    // ep_count > 1 branch doesn't compute one). This pins the empty-
    // title default — episodes still get cached, but with empty
    // titles instead of Jikan ones.
    //
    // The mutation surface here is the `if ep_count <= 1` guard at
    // line 264 of metadata_sync.rs and the fallback ladder around it.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(tv_media_detail_response(6666, 88888)),
        )
        .mount(&mock)
        .await;
    // Jikan returns empty data — no episodes cached upstream. Pinning
    // `.expect(1..)` doubles as a check that should_fetch_jikan still
    // fires for TV+ep_count>1.
    Mock::given(method("GET"))
        .and(path("/anime/88888/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(1..)
        .mount(&mock)
        .await;
    // Kitsu has no mapping for the MAL id — without this, the fallback
    // hits real kitsu.io. .expect(1..) pins line 247's
    // `force_kitsu_fallback || backfill` OR + line 251's closure: when
    // Jikan returns empty, all closure results are false → backfill
    // needed → Kitsu IS consulted. Since #235 that consultation is the
    // `/mappings` lookup by MAL id; the title search below must stay
    // untouched when a MAL id is known.
    Mock::given(method("GET"))
        .and(path("/mappings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [], "included": []})))
        .expect(1..)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series_tv(&db, 6666).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("TV refresh succeeds even when Jikan has no episodes");

    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    assert_eq!(episodes.len(), 3, "episode rows must still be created");
    // Empty titles, source tagged as the series-level fallback.
    for ep_num in 1..=3 {
        let ep = episodes.get(&ep_num).expect("episode row present");
        assert!(
            ep.title.is_empty(),
            "TV episode without Jikan title must be empty (got {:?})",
            ep.title
        );
        assert_eq!(
            ep.source.as_str(),
            "series",
            "fallback rows must be tagged 'series'"
        );
    }

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_movie_falls_back_to_romaji_when_english_empty() {
    // Pin line 265's `if !detail.title_english.trim().is_empty()` guard
    // in build_episode_cache. With title_english = "" and a non-empty
    // title_romaji, the original code falls through to the romaji
    // branch (line 267) and writes "Test Romaji" as the episode
    // title. A `delete !` mutation flips the guard to "fire when
    // title_english IS empty," which would write the empty string.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(movie_detail_response_with_titles(
                7001,
                "Test Romaji",
                "",
                "テスト",
            )),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 7001).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("refresh");

    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    let ep1 = episodes.get(&1).expect("episode 1 present");
    assert_eq!(
        ep1.title, "Test Romaji",
        "empty title_english must fall through to title_romaji"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_series_metadata_movie_falls_back_to_native_when_english_and_romaji_empty() {
    // Pin line 267's `if !detail.title_romaji.trim().is_empty()` guard.
    // With both english and romaji empty, the chain falls through to
    // the title_native else-arm (line 270). A `delete !` mutation on
    // line 267 would prefer the empty title_romaji over the populated
    // title_native.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(movie_detail_response_with_titles(
                7002,
                "",
                "",
                "テスト",
            )),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series(&db, 7002).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("refresh");

    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .expect("episode-map fetch");
    let ep1 = episodes.get(&1).expect("episode 1 present");
    assert_eq!(
        ep1.title, "テスト",
        "both english and romaji empty must fall through to title_native"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_all_series_metadata_skips_when_anilist_is_unreachable() {
    // Drives `run_metadata_sweep` end-to-end with one tracked series
    // whose AL fetch fails. `run_metadata_sweep` MUST NOT panic on a
    // failing series; it counts the failure and continues. The
    // sweep's (refreshed, failed) tuple is part of the
    // `system::api_metadata_refresh` handler contract.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    seed_minimal_series(&db, 9999).await;

    let (refreshed, failed) = metadata_sync::refresh_all_series_metadata(&db)
        .await
        .expect("no other sweep holds the lock");
    assert_eq!(refreshed, 0);
    assert_eq!(failed, 1);

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

/// One `episode_cache` row shaped like the Jikan negative-cache
/// sentinel (`episode_number = 0`, `title = "__RYOKAN_EMPTY__"`), the
/// state a `data: []` Tenrai answer leaves behind for seven days.
async fn seed_jikan_sentinel(db: &SqlitePool, mal_id: i64) {
    sqlx::query(
        "INSERT INTO episode_cache (mal_id, episode_number, title, aired) \
         VALUES (?, 0, '__RYOKAN_EMPTY__', '')",
    )
    .bind(mal_id)
    .execute(db)
    .await
    .unwrap();
}

async fn jikan_sentinel_rows(db: &SqlitePool, mal_id: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM episode_cache WHERE mal_id = ? AND episode_number = 0")
        .bind(mal_id)
        .fetch_one(db)
        .await
        .unwrap()
}

fn jikan_three_episodes() -> serde_json::Value {
    json!({
        "data": [
            { "mal_id": 1, "episode_id": 1, "title": "Pilot Episode",
              "aired": "2024-01-01T00:00:00+00:00" },
            { "mal_id": 2, "episode_id": 2, "title": "Second Steps",
              "aired": "2024-01-08T00:00:00+00:00" },
            { "mal_id": 3, "episode_id": 3, "title": "Resolution",
              "aired": "2024-01-15T00:00:00+00:00" },
        ]
    })
}

#[tokio::test]
async fn manual_rebuild_drops_the_jikan_negative_cache_and_refetches_titles() {
    // #235: a sentinel left by an empty Tenrai answer survived every
    // manual rebuild for its 7-day TTL, so "Rebuild metadata cache"
    // kept serving the Kitsu fallback's titles. The rebuild path now
    // drops the series' Jikan episode cache first and reads Tenrai
    // again; `.expect(1..)` on the episodes mock is the assertion.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(tv_media_detail_response(5556, 88888)),
        )
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/88888/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jikan_three_episodes()))
        .expect(1..)
        .mount(&mock)
        .await;
    // Neither Kitsu surface may be touched once Tenrai has titles.
    Mock::given(method("GET"))
        .and(path("/mappings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [], "included": []})))
        .expect(0)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series_tv(&db, 5556).await;
    seed_jikan_sentinel(&db, 88888).await;

    let (rebuilt, skipped, failed) = metadata_sync::rebuild_cached_metadata_for_all(&db)
        .await
        .expect("no other sweep holds the lock");
    assert_eq!((rebuilt, skipped, failed), (1, 0, 0));

    assert_eq!(
        jikan_sentinel_rows(&db, 88888).await,
        0,
        "the rebuild must drop the negative-cache sentinel"
    );
    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .unwrap();
    assert_eq!(episodes.len(), 3);
    assert_eq!(
        episodes
            .get(&1)
            .map(|e| (e.title.as_str(), e.source.as_str())),
        Some(("Pilot Episode", "jikan"))
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn refresh_with_a_jikan_sentinel_takes_kitsu_titles_by_mal_id_not_by_title() {
    // The periodic refresh still honors the sentinel (no Tenrai call),
    // so the Kitsu fallback runs. It must resolve the entry through
    // Kitsu's MAL-id mapping and never the title search: #235's wrong
    // titles came from the fuzz picking "Gabriel DropOut Specials"
    // for "Dropout". `.expect(0)` on `/anime` is the assertion.
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(tv_media_detail_response(5557, 77777)),
        )
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/77777/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jikan_three_episodes()))
        .expect(0)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/mappings"))
        .and(wiremock::matchers::query_param(
            "filter[externalId]",
            "77777",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "id": "1", "type": "mappings" }],
            "included": [{
                "id": "4242",
                "type": "anime",
                "attributes": {
                    "canonicalTitle": "Right Show",
                    "titles": { "en_jp": "Right Show" },
                    "subtype": "TV",
                    "status": "finished",
                    "episodeCount": 3,
                    "startDate": "2024-01-01"
                }
            }]
        })))
        .expect(1..)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/4242/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "id": "1", "type": "episodes", "attributes": {
                    "canonicalTitle": "Kitsu One", "number": 1, "relativeNumber": 1,
                    "airDate": "2024-01-01" } },
                { "id": "2", "type": "episodes", "attributes": {
                    "canonicalTitle": "Kitsu Two", "number": 2, "relativeNumber": 2,
                    "airDate": "2024-01-08" } },
                { "id": "3", "type": "episodes", "attributes": {
                    "canonicalTitle": "Kitsu Three", "number": 3, "relativeNumber": 3,
                    "airDate": "2024-01-15" } }
            ],
            "links": {}
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        std::env::set_var("JIKAN_API_BASE", mock.uri());
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let series_id = seed_minimal_series_tv(&db, 5557).await;
    seed_jikan_sentinel(&db, 77777).await;
    let tracked = series::get_by_id(&db, series_id).await.unwrap().unwrap();

    metadata_sync::refresh_series_metadata(&db, &tracked, false)
        .await
        .expect("refresh should succeed against the wiremock fixture");

    assert_eq!(
        jikan_sentinel_rows(&db, 77777).await,
        1,
        "a plain refresh keeps the sentinel; only the rebuild drops it"
    );
    let episodes = local_metadata::get_episode_map_for_series(&db, series_id)
        .await
        .unwrap();
    assert_eq!(episodes.len(), 3);
    assert_eq!(
        episodes
            .get(&2)
            .map(|e| (e.title.as_str(), e.source.as_str())),
        Some(("Kitsu Two", "kitsu"))
    );
    // The hand-off is visible in System → Logs.
    let kitsu_lines: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT level, message, detail FROM logs WHERE category = 'kitsu' ORDER BY id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert!(!kitsu_lines.is_empty(), "expected a Kitsu hand-off line");
    assert_eq!(kitsu_lines[0].0, "info");
    assert_eq!(
        kitsu_lines[0].1,
        "Episode titles for Test TV Show EN came from Kitsu"
    );
    assert!(
        kitsu_lines[0].2.contains("mal_id=Some(77777)")
            && kitsu_lines[0].2.contains("reason=mal_empty"),
        "{}",
        kitsu_lines[0].2
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        std::env::remove_var("JIKAN_API_BASE");
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
    anilist::reset_state_for_tests();
}
