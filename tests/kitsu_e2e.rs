//! Wiremock coverage for `services::kitsu`'s episode-title fallback,
//! pointed at by `RYOKAN_KITSU_API_BASE`. Same env-var serializer
//! pattern as `tests/jikan_e2e.rs`: this crate is its own process, and
//! tests inside it share `ENV_LOCK`.
//!
//! Issue #235 is the reason this file exists. Kitsu's text search does
//! not surface its NSFW entries, so the title fuzz for "Dropout" (MAL
//! 31886) picked "Gabriel DropOut Specials" and that show's episode
//! titles were stamped onto the series. The fallback now resolves by
//! MAL id through `/mappings` and never by title: no MAL id, no Kitsu
//! episode titles.

use ryokan::services::kitsu;
use ryokan::test_support::in_memory_pool;
use serde_json::json;
use std::sync::LazyLock;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn anime_attributes(title: &str, episodes: i32, start: &str) -> serde_json::Value {
    json!({
        "canonicalTitle": title,
        "titles": { "en_jp": title },
        "abbreviatedTitles": [],
        "subtype": "OVA",
        "status": "finished",
        "episodeCount": episodes,
        "startDate": start,
        "nsfw": false
    })
}

fn mapping_response(kitsu_id: &str, title: &str) -> serde_json::Value {
    json!({
        "data": [{ "id": "1", "type": "mappings" }],
        "included": [{
            "id": kitsu_id,
            "type": "anime",
            "attributes": anime_attributes(title, 2, "2016-02-26")
        }]
    })
}

fn episodes_response(prefix: &str) -> serde_json::Value {
    json!({
        "data": [
            { "id": "1", "type": "episodes", "attributes": {
                "canonicalTitle": format!("{prefix} One"), "number": 1, "relativeNumber": 1,
                "airDate": "2016-02-26" } },
            { "id": "2", "type": "episodes", "attributes": {
                "canonicalTitle": format!("{prefix} Two"), "number": 2, "relativeNumber": 2,
                "airDate": "2016-03-25" } }
        ],
        "links": {}
    })
}

/// A title search that returns a *different* show which the scorer
/// would love: same episode count, title contained, adjacent year.
/// Mirrors the real "Gabriel DropOut Specials" result set.
async fn mount_tempting_title_search(mock: &MockServer, expect: u64) {
    Mock::given(method("GET"))
        .and(path("/anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "13294",
                "type": "anime",
                "attributes": anime_attributes("Gabriel DropOut Specials", 2, "2017-03-24")
            }]
        })))
        .expect(expect)
        .mount(mock)
        .await;
}

#[tokio::test]
async fn episode_fallback_resolves_by_mal_id_and_never_title_searches() {
    let _gate = ENV_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mappings"))
        .and(query_param("filter[externalSite]", "myanimelist/anime"))
        .and(query_param("filter[externalId]", "31886"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mapping_response("11518", "Dropout")),
        )
        // Once per call below: identity is resolved before the episode
        // cache is consulted. The `== 1` on the episodes mock is what
        // pins the second call as a cache hit.
        .expect(2)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/11518/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(episodes_response("Dropout")))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/anime/13294/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(episodes_response("Gabriel")))
        .expect(0)
        .mount(&mock)
        .await;
    mount_tempting_title_search(&mock, 0).await;
    unsafe {
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }
    let db = in_memory_pool().await;

    let eps = kitsu::fetch_episode_titles_fallback(&db, Some(31886)).await;
    assert_eq!(eps.get(&1).map(|e| e.title.as_str()), Some("Dropout One"));
    assert_eq!(eps.get(&2).map(|e| e.title.as_str()), Some("Dropout Two"));

    // Cached under the mapped id: a second call is served from the DB.
    let again = kitsu::fetch_episode_titles_fallback(&db, Some(31886)).await;
    assert_eq!(again.len(), 2);

    unsafe {
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
}

#[tokio::test]
async fn episode_fallback_with_a_mal_id_but_no_mapping_returns_nothing() {
    // Known identity, no Kitsu entry for it: blank beats a guess.
    let _gate = ENV_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mappings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [], "included": []})))
        .expect(1)
        .mount(&mock)
        .await;
    mount_tempting_title_search(&mock, 0).await;
    unsafe {
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }
    let db = in_memory_pool().await;

    let eps = kitsu::fetch_episode_titles_fallback(&db, Some(31886)).await;
    assert!(eps.is_empty());

    unsafe {
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
}

#[tokio::test]
async fn episode_fallback_with_a_failed_mapping_lookup_returns_nothing() {
    let _gate = ENV_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mappings"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&mock)
        .await;
    mount_tempting_title_search(&mock, 0).await;
    unsafe {
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }
    let db = in_memory_pool().await;

    let eps = kitsu::fetch_episode_titles_fallback(&db, Some(31886)).await;
    assert!(eps.is_empty());

    unsafe {
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
}

#[tokio::test]
async fn episode_fallback_without_a_mal_id_returns_nothing_and_never_searches() {
    // Kitsu's role is the outage fallback for MAL, and a title guess is
    // the one thing it must not do: with no MAL id there is no identity
    // to resolve, so there are no Kitsu episode titles. The tempting
    // search result stays unrequested.
    let _gate = ENV_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mappings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [], "included": []})))
        .expect(0)
        .mount(&mock)
        .await;
    mount_tempting_title_search(&mock, 0).await;
    Mock::given(method("GET"))
        .and(path("/anime/13294/episodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(episodes_response("Gabriel")))
        .expect(0)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }
    let db = in_memory_pool().await;

    assert!(
        kitsu::fetch_episode_titles_fallback(&db, None)
            .await
            .is_empty()
    );
    // `Some(0)` is the external-sync placeholder, not an id.
    assert!(
        kitsu::fetch_episode_titles_fallback(&db, Some(0))
            .await
            .is_empty()
    );

    unsafe {
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
}

#[tokio::test]
async fn detail_by_mal_id_carries_the_mal_id() {
    // The mapping is the MAL identity; the detail keeps it so the
    // episode path can ask Jikan and Kitsu by id instead of by title.
    let _gate = ENV_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mappings"))
        .and(query_param("filter[externalId]", "31886"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mapping_response("11518", "Dropout")),
        )
        .expect(1)
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_KITSU_API_BASE", mock.uri());
    }

    let detail = kitsu::get_anime_detail_by_mal_id(31886)
        .await
        .expect("mapping request succeeds")
        .expect("mapping found");
    assert_eq!(detail.id, 11518);
    assert_eq!(detail.id_mal, Some(31886));
    assert_eq!(detail.title_romaji, "Dropout");

    unsafe {
        std::env::remove_var("RYOKAN_KITSU_API_BASE");
    }
}
