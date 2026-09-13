//! End-to-end coverage for the manual-import matcher's query ladder
//! (#122): when AniList returns nothing for the `title season N`
//! query, the bare title is tried next. Stands up a wiremock AniList
//! and points Ryokan at it via `RYOKAN_ANILIST_API_BASE`, the same
//! fixture shape as `tests/library_link_e2e.rs`.

use std::path::PathBuf;
use std::sync::LazyLock;

use ryokan::services::anilist;
use ryokan::services::manual_import::parse::TitleSource;
use ryokan::services::manual_import::{self, CandidateFile, SeriesGroup};
use serde_json::json;
use tokio::sync::Mutex;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn search_response(entries: &[(i64, &str, &str)]) -> serde_json::Value {
    let media: Vec<serde_json::Value> = entries
        .iter()
        .map(|(id, romaji, english)| {
            json!({
                "id": id,
                "idMal": null,
                "title": { "romaji": romaji, "english": english, "native": "" },
                "coverImage": { "large": "https://example/cover.jpg" },
                "format": "TV",
                "status": "FINISHED",
                "episodes": 12,
                "seasonYear": 2024,
                "averageScore": 80,
            })
        })
        .collect();
    json!({ "data": { "Page": { "media": media } } })
}

fn file(name: &str, ep: i32) -> CandidateFile {
    CandidateFile {
        path: PathBuf::from(name),
        rel_path: name.to_string(),
        file_name: name.to_string(),
        size_bytes: 1,
        parsed_title: Some("Enen no Shouboutai".into()),
        title_source: TitleSource::Filename,
        season: Some(3),
        episode: Some(ep),
        year: None,
        group: None,
        quality_label: "WEB-1080p".into(),
        selected: true,
        episode_count: 1,
        source_episode: None,
    }
}

fn group() -> SeriesGroup {
    SeriesGroup {
        key: "enen no shouboutai|s3".into(),
        parsed_title: "Enen no Shouboutai".into(),
        season: Some(3),
        tmdb_season: Some(3),
        year: None,
        query: "Enen no Shouboutai season 3".into(),
        files: vec![file(
            "[SubsPlease] Enen no Shouboutai S3 - 18 (1080p).mkv",
            18,
        )],
        candidates: Vec::new(),
        pick: None,
        low_confidence: false,
        search_error: None,
        skipped: false,
        existing: None,
        resolved_by_id: false,
        mapping_note: None,
        search_results: Vec::new(),
    }
}

#[tokio::test]
async fn empty_season_query_falls_back_to_the_bare_title_and_ranks_the_sequel() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    let mock = MockServer::start().await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }
    // The season-suffixed query comes back empty; the bare title
    // returns the franchise, sequel included.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("season 3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[])))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
            (1, "Enen no Shouboutai", "Fire Force"),
            (2, "Enen no Shouboutai: Ni no Shou", "Fire Force Season 2"),
            (3, "Enen no Shouboutai: San no Shou", "Fire Force Season 3"),
        ])))
        .with_priority(5)
        .mount(&mock)
        .await;

    let mut g = group();
    manual_import::search_and_rank(&mut g, true).await;
    assert!(g.search_error.is_none(), "{:?}", g.search_error);
    assert_eq!(
        g.query, "Enen no Shouboutai",
        "the query that matched is kept"
    );
    assert_eq!(
        g.picked().map(|e| e.id),
        Some(3),
        "season marker picks the sequel"
    );
    assert!(!g.low_confidence);

    // A typed re-search is searched as typed: no ladder.
    let mut g = group();
    g.query = "Enen no Shouboutai season 3".into();
    manual_import::search_and_rank(&mut g, false).await;
    assert!(g.candidates.is_empty());
    assert!(g.pick.is_none());

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

/// Detail response with an optional TV SEQUEL edge, in the shape
/// `services::anilist::get_anime_detail` parses.
fn detail_response(id: i64, title: &str, episodes: i32, sequel: Option<i64>) -> serde_json::Value {
    let edges: Vec<serde_json::Value> = sequel
        .map(|sid| {
            vec![json!({
                "relationType": "SEQUEL",
                "node": {
                    "id": sid, "idMal": null,
                    "title": { "romaji": format!("{title} Sequel"), "english": "", "native": "" },
                    "format": "TV", "status": "FINISHED", "episodes": 23,
                    "coverImage": { "large": "" }, "type": "ANIME", "seasonYear": 2023
                }
            })]
        })
        .unwrap_or_default();
    json!({ "data": { "Media": {
        "id": id, "idMal": null,
        "title": { "romaji": title, "english": title, "native": "" },
        "synonyms": [],
        "coverImage": { "large": "", "extraLarge": "" },
        "bannerImage": "",
        "format": "TV", "status": "FINISHED", "episodes": episodes, "duration": 24,
        "season": "FALL", "seasonYear": 2020, "endDate": { "year": 2021 },
        "description": "", "genres": [], "averageScore": 80,
        "nextAiringEpisode": null, "streamingEpisodes": [],
        "relations": { "edges": edges }
    }}})
}

#[tokio::test]
async fn absolute_numbering_walks_the_sequel_chain() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    let mock = MockServer::start().await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }
    // Search finds only the first entry; the chain is 101 → 102 → 103.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[(
            101,
            "Jujutsu Kaisen",
            "JUJUTSU KAISEN",
        )])))
        .mount(&mock)
        .await;
    for (id, eps, seq) in [(101, 24, Some(102)), (102, 23, Some(103)), (103, 24, None)] {
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_string_contains(format!("\"id\":{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail_response(
                id,
                "Jujutsu Kaisen",
                eps,
                seq,
            )))
            .mount(&mock)
            .await;
    }

    let mut files = vec![
        file("Jujutsu Kaisen - 10.mkv", 10),
        file("Jujutsu Kaisen - 55.mkv", 55),
        file("Jujutsu Kaisen - 30.mkv", 30),
    ];
    for f in &mut files {
        f.parsed_title = Some("Jujutsu Kaisen".into());
        f.season = None;
    }
    let mut g = group();
    g.parsed_title = "Jujutsu Kaisen".into();
    g.season = None;
    g.tmdb_season = None;
    g.query = "Jujutsu Kaisen".into();
    g.files = files;
    // The search entry advertises 24 episodes, so 55 and 30 overflow.
    manual_import::search_and_rank(&mut g, true).await;
    assert_eq!(g.picked().map(|e| e.id), Some(101));
    let mut out = Vec::new();
    for g in manual_import::mapping::apply_season_mapping(g).await {
        out.extend(manual_import::mapping::apply_absolute_numbering(g).await);
    }
    assert_eq!(
        out.len(),
        3,
        "{:?}",
        out.iter().map(|g| &g.mapping_note).collect::<Vec<_>>()
    );
    assert_eq!(out[0].picked().map(|e| e.id), Some(101));
    assert_eq!(out[0].files[0].episode, Some(10));
    assert_eq!(out[1].picked().map(|e| e.id), Some(102));
    assert_eq!(
        (out[1].files[0].episode, out[1].files[0].source_episode),
        (Some(6), Some(30))
    );
    assert_eq!(out[2].picked().map(|e| e.id), Some(103));
    assert_eq!(
        (out[2].files[0].episode, out[2].files[0].source_episode),
        (Some(8), Some(55))
    );
    assert_eq!(
        out[2].mapping_note.as_deref(),
        Some("Absolute numbering; episodes 55 to 55 through the sequel chain")
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

#[tokio::test]
async fn live_search_ranks_hits_and_pick_by_id_promotes_one() {
    use ryokan::services::manual_import::session;
    use ryokan::test_support::{build_test_app_state, in_memory_pool};

    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    let mock = MockServer::start().await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
            (7, "Bleach", "Bleach"),
            (
                8,
                "Bleach: Sennen Kessen-hen",
                "BLEACH: Thousand-Year Blood War",
            ),
        ])))
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let mut g = group();
    g.candidates = vec![];
    g.pick = None;
    let mut s = ryokan::services::manual_import::ImportSession::new(
        session::mint_id(),
        PathBuf::from("/src"),
        ryokan::services::manual_import::ImportMode::Hardlink,
        false,
        false,
    );
    s.status = ryokan::services::manual_import::SessionStatus::Ready;
    s.groups.push(g);
    let sid = s.id.clone();
    session::insert(&state.import_sessions, s);

    let hits = manual_import::live_search(&state, &sid, 0, "Bleach")
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    let stored = session::get(&state.import_sessions, &sid).unwrap();
    assert_eq!(
        stored.groups[0].search_results.len(),
        2,
        "kept for the pick"
    );
    assert!(stored.groups[0].pick.is_none(), "searching doesn't pick");

    manual_import::pick_by_id(&state, &sid, 0, 8).await.unwrap();
    let stored = session::get(&state.import_sessions, &sid).unwrap();
    assert_eq!(
        stored.groups[0].picked().map(|e| e.id),
        Some(8),
        "promoted from the search results"
    );
    let err = manual_import::pick_by_id(&state, &sid, 0, 9)
        .await
        .unwrap_err();
    assert!(err.contains("Unknown candidate"), "{err}");
    let err = manual_import::live_search(&state, &sid, 0, "  ")
        .await
        .unwrap_err();
    assert!(err.contains("Type a title"), "{err}");

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}
