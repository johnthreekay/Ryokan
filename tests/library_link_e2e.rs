//! End-to-end tests for `services::library_link::resolve_or_add_series_for_grab`.
//!
//! Each test stands up a wiremock server, points Ryokan at it via
//! `RYOKAN_ANILIST_API_BASE`, drives the resolver with a release
//! title, and asserts the outcome variant + side effects (series row
//! upserted vs not, etc.). The cheap-fuzzy-match path is also covered
//! here — that branch doesn't hit the wiremock at all but we want one
//! integration assertion that it short-circuits as expected.

use std::sync::LazyLock;

use ryokan::models::{config, series};
use ryokan::services::anilist;
use ryokan::services::library_link::{LibraryLinkOutcome, resolve_or_add_series_for_grab};
use ryokan::test_support::{build_test_app_state, in_memory_pool, seed_series};
use serde_json::json;
use tokio::sync::Mutex;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One-at-a-time gate around `RYOKAN_ANILIST_API_BASE` writes so
/// nextest's parallel-by-default scheduler can't race two tests on
/// the process-wide env var. Without this, two tests racing
/// `set_var` would overwrite each other's wiremock URIs and one
/// would route AL requests to the *other* test's mock — producing
/// nondeterministic flakes that nextest retries may or may not
/// absorb. Same pattern + rationale as
/// `tests/metadata_sync_e2e.rs::ENV_LOCK` and
/// `tests/external_sync_e2e.rs::ENV_LOCK`.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// AL `Page.media` search response shape — matches the
/// `query ($search) { Page(...) { media(search:..., sort: SEARCH_MATCH) }}`
/// query in `services::anilist::search_anime_with_options`. The
/// AnimeEntry deserializer reads `id`, `title`, `coverImage.large`,
/// `format`, `status`, `episodes`, `seasonYear`, `averageScore`.
fn search_response_for(id: i64, romaji: &str, english: &str, native: &str) -> serde_json::Value {
    json!({
        "data": {
            "Page": {
                "media": [
                    {
                        "id": id,
                        "idMal": null,
                        "title": {
                            "romaji": romaji,
                            "english": english,
                            "native": native,
                        },
                        "coverImage": { "large": "https://example/cover.jpg" },
                        "format": "TV",
                        "status": "RELEASING",
                        "episodes": 12,
                        "seasonYear": 2024,
                        "averageScore": 80,
                    }
                ]
            }
        }
    })
}

/// Empty AL search response — used to drive the NoMatch outcome
/// when the title parses but AL has nothing for it.
fn empty_search_response() -> serde_json::Value {
    json!({ "data": { "Page": { "media": [] } } })
}

/// AL `Media(id:...)` detail-fetch response. Required for the
/// auto-add path (case A) — the resolver fetches the full detail to
/// build a `SeriesCore` for upsert.
fn detail_response(id: i64, title: &str) -> serde_json::Value {
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
                    "extraLarge": "https://example/cover-xl.jpg"
                },
                "bannerImage": "https://example/banner.jpg",
                "format": "TV",
                "status": "RELEASING",
                "episodes": 12,
                "duration": 24,
                "season": "SPRING",
                "seasonYear": 2024,
                "endDate": { "year": null },
                "description": "Test detail.",
                "genres": ["Action"],
                "averageScore": 80,
                "nextAiringEpisode": null,
                "streamingEpisodes": [],
                "relations": { "edges": [] }
            }
        }
    })
}

/// Helper: install AL wiremock, set the env var, reset rate-limit
/// state. Returns the server so the caller can install per-test
/// mocks. Caller is responsible for unset-on-drop via a tear-down
/// at the end of each test.
async fn al_mock_server() -> MockServer {
    anilist::reset_state_for_tests();
    let mock = MockServer::start().await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }
    mock
}

fn teardown() {
    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn linked_existing_short_circuits_without_al_call() {
    let _gate = ENV_LOCK.lock().await;
    // Wiremock is live but no expectations set; if the resolver hits
    // it, the test still passes (any-method any-path falls back to
    // 404), but the assertion below — that the seeded fuzzy-matched
    // series came back — proves the cheap path fired first.
    let mock = al_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    seed_series(&db, 21202, "Mob Psycho 100 III").await;

    let outcome = resolve_or_add_series_for_grab(
        &state,
        "[Erai-raws] Mob Psycho 100 III - 12 [1080p].mkv",
        false,
    )
    .await;

    match outcome {
        LibraryLinkOutcome::LinkedExisting { series, .. } => {
            assert_eq!(series.anilist_id, 21202);
        }
        other => panic!(
            "expected LinkedExisting (fuzzy match), got {:?} — this means the cheap match didn't fire",
            other
        ),
    }
    teardown();
}

#[tokio::test]
async fn linked_by_anilist_finds_existing_series_via_al_id() {
    let _gate = ENV_LOCK.lock().await;
    let mock = al_mock_server().await;
    // Search returns id=999. The local series is seeded with that
    // id but a title the fuzzy matcher won't catch (different
    // wording so `match_library_title` misses), forcing the
    // resolver to fall through to the AL-ID lookup branch.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_response_for(
                999,
                "Bocchi the Rock!",
                "Bocchi the Rock!",
                "ぼっち・ざ・ろっく！",
            )),
        )
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    // Seed a series with the AL id but a wildly different title so
    // the RSS fuzzy matcher cannot link via title-similarity.
    seed_series(&db, 999, "ZZZZZZZZZZZZZZZZ").await;

    let outcome =
        resolve_or_add_series_for_grab(&state, "[Vodes] Bocchi the Rock! - 01 [1080p].mkv", false)
            .await;

    match outcome {
        LibraryLinkOutcome::LinkedByAnilist { series, .. } => {
            assert_eq!(
                series.anilist_id, 999,
                "AL-ID lookup must find the seeded row"
            );
        }
        other => panic!("expected LinkedByAnilist, got {:?}", other),
    }
    teardown();
}

#[tokio::test]
async fn auto_added_inserts_series_and_links_when_toggle_on() {
    let _gate = ENV_LOCK.lock().await;
    let mock = al_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_response_for(
                777,
                "Frieren: Beyond Journey's End",
                "Frieren: Beyond Journey's End",
                "葬送のフリーレン",
            )),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id:"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(detail_response(777, "Frieren: Beyond Journey's End")),
        )
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    // No seed — auto_add toggle defaults to ON.
    // Confirm pre-state: no row for id=777.
    assert!(
        series::get_by_anilist_id(&db, 777).await.unwrap().is_none(),
        "pre-condition: no series row for id=777"
    );

    let outcome =
        resolve_or_add_series_for_grab(&state, "[SubsPlease] Frieren - 01 [1080p].mkv", false)
            .await;

    match outcome {
        LibraryLinkOutcome::AutoAdded { series, .. } => {
            assert_eq!(series.anilist_id, 777);
        }
        other => panic!("expected AutoAdded, got {:?}", other),
    }
    // The upsert side effect must have actually landed.
    assert!(
        series::get_by_anilist_id(&db, 777).await.unwrap().is_some(),
        "AutoAdded outcome must have written a series row"
    );
    teardown();
}

#[tokio::test]
async fn auto_add_disabled_returns_disabled_outcome_without_upsert() {
    let _gate = ENV_LOCK.lock().await;
    let mock = al_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_response_for(
                555,
                "Some Series",
                "Some Series",
                "ある作品",
            )),
        )
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    // Flip the auto-add toggle off via direct config save.
    let mut cfg = config::get_config(&db).await.unwrap().unwrap_or_default();
    cfg.manual_search_auto_add = false;
    config::save_config(&db, &cfg).await.expect("save config");

    let outcome =
        resolve_or_add_series_for_grab(&state, "[Vodes] Some Series - 01 [1080p].mkv", false).await;

    match outcome {
        LibraryLinkOutcome::AutoAddDisabled { al_id, .. } => {
            assert_eq!(al_id, 555);
        }
        other => panic!("expected AutoAddDisabled, got {:?}", other),
    }
    // No upsert side effect.
    assert!(
        series::get_by_anilist_id(&db, 555).await.unwrap().is_none(),
        "AutoAddDisabled outcome must NOT have written a series row"
    );
    teardown();
}

#[tokio::test]
async fn ambiguous_match_refuses_unrelated_al_top_hit() {
    let _gate = ENV_LOCK.lock().await;
    // AL search returns a series whose title shares no substantive
    // tokens with the parsed title. Resolver should refuse to
    // auto-add and surface AmbiguousMatch.
    let mock = al_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_response_for(
                333,
                "Spy x Family",
                "Spy x Family",
                "スパイファミリー",
            )),
        )
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);

    let outcome = resolve_or_add_series_for_grab(
        &state,
        "[Group] Mob Psycho 100 III - 01 [1080p].mkv",
        false,
    )
    .await;

    match outcome {
        LibraryLinkOutcome::AmbiguousMatch {
            parsed_title,
            al_title,
        } => {
            assert!(parsed_title.to_lowercase().contains("mob"));
            assert_eq!(al_title, "Spy x Family");
        }
        other => panic!("expected AmbiguousMatch, got {:?}", other),
    }
    // No upsert side effect.
    assert!(
        series::get_by_anilist_id(&db, 333).await.unwrap().is_none(),
        "AmbiguousMatch outcome must NOT have written a series row"
    );
    teardown();
}

#[tokio::test]
async fn detail_fetch_failed_when_al_search_hits_but_detail_5xxs() {
    let _gate = ENV_LOCK.lock().await;
    // AL search returns a hit; the second-stage detail fetch fails
    // (HTTP 503 — transient outage between the two requests). The
    // resolver must surface DetailFetchFailed (not NoMatch) so the
    // toast can honestly say "matched on AniList but couldn't fetch
    // details" rather than implying the show isn't on AL at all.
    let mock = al_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_response_for(
                444,
                "Frieren: Beyond Journey's End",
                "Frieren: Beyond Journey's End",
                "葬送のフリーレン",
            )),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Media(id:"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    // Pre-state: no series row for id=444.
    assert!(
        series::get_by_anilist_id(&db, 444).await.unwrap().is_none(),
        "pre-condition: no series row for id=444"
    );

    let outcome =
        resolve_or_add_series_for_grab(&state, "[SubsPlease] Frieren - 01 [1080p].mkv", false)
            .await;

    match outcome {
        LibraryLinkOutcome::DetailFetchFailed { al_id, al_title } => {
            assert_eq!(al_id, 444);
            assert!(
                al_title.to_lowercase().contains("frieren"),
                "al_title should carry the matched series's display name; got {al_title:?}"
            );
        }
        other => panic!(
            "expected DetailFetchFailed (search hit + detail 5xx), got {:?}",
            other
        ),
    }
    // No upsert side effect — detail fetch failed before upsert.
    assert!(
        series::get_by_anilist_id(&db, 444).await.unwrap().is_none(),
        "DetailFetchFailed outcome must NOT have written a series row"
    );
    teardown();
}

#[tokio::test]
async fn no_match_when_al_search_empty() {
    let _gate = ENV_LOCK.lock().await;
    let mock = al_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_search_response()))
        .mount(&mock)
        .await;
    // Also short-circuit Jikan fallback paths so the AL search empty
    // response doesn't bounce through to the live Jikan API.
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": [], "pagination": {} })),
        )
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);

    let outcome = resolve_or_add_series_for_grab(
        &state,
        "[Group] Some Random Title That Does Not Exist - 01 [1080p].mkv",
        false,
    )
    .await;

    match outcome {
        LibraryLinkOutcome::NoMatch { parsed_title } => {
            assert!(
                parsed_title.is_some(),
                "the title should still parse even when AL has no match"
            );
        }
        other => panic!("expected NoMatch, got {:?}", other),
    }
    teardown();
}

#[tokio::test]
async fn a_mal_hit_shared_by_two_library_series_links_neither() {
    // #292: AniList down, the search falls back to MAL and finds MAL
    // 61469, which AniList splits into Steel Ball Run's 1st STAGE and
    // 2nd - 3rd STAGE. With both in the library the grab could be for
    // either; with one, that one is the series.
    let _gate = ENV_LOCK.lock().await;
    let mock = al_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "mal_id": 61469,
                "title": "Steel Ball Run",
                "title_english": "STEEL BALL RUN",
                "title_japanese": "",
                "type": "ONA",
                "status": "Currently Airing",
                "episodes": 12
            }]
        })))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("JIKAN_API_BASE", mock.uri());
    }

    let release = "[SubsPlease] Steel Ball Run - 07 (1080p).mkv";
    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let first = seed_series(&db, 190327, "ZZZZ first").await;
    let second = seed_series(&db, 210482, "ZZZZ second").await;
    for id in [first, second] {
        sqlx::query("UPDATE series SET mal_id = 61469 WHERE id = ?")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
    }
    let outcome = resolve_or_add_series_for_grab(&state, release, false).await;
    assert!(
        matches!(outcome, LibraryLinkOutcome::AmbiguousMatch { .. }),
        "{outcome:?}"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM series")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(rows, 2, "nothing is added beside them");

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    let only = seed_series(&db, 190327, "ZZZZ first").await;
    sqlx::query("UPDATE series SET mal_id = 61469 WHERE id = ?")
        .bind(only)
        .execute(&db)
        .await
        .unwrap();
    let outcome = resolve_or_add_series_for_grab(&state, release, false).await;

    unsafe {
        std::env::remove_var("JIKAN_API_BASE");
    }
    teardown();
    match outcome {
        LibraryLinkOutcome::LinkedByAnilist { series, .. } => assert_eq!(series.id, only),
        other => panic!("expected LinkedByAnilist, got {other:?}"),
    }
}
