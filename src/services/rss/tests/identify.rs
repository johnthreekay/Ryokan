//! The matcher's winner goes through `identify`: another entry of its
//! franchise that fits the release better takes it when it is a tracked
//! series the release also matches, and nothing does otherwise.

use super::super::*;
use crate::services::identify::{FranchiseGraph, GraphEdge, GraphNode};

fn series(id: i64, anilist_id: i64, title: &str, episodes: i32, year: i32) -> series::Series {
    series::Series {
        is_adult: false,
        id,
        anilist_id,
        mal_id: None,
        title: title.to_string(),
        title_romaji: title.to_string(),
        title_english: String::new(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "TV".to_string(),
        status: "FINISHED".to_string(),
        episodes: Some(episodes),
        season_year: Some(year),
        end_year: Some(year),
        folder_name: title.to_string(),
        monitor_mode: "all".to_string(),
        allow_upgrades: true,
        allow_pt_upgrades: false,
        custom_query_tokens: String::new(),
        restrict_to_uploader: String::new(),
        alternate_titles: String::new(),
        cumulative_prior_episodes: 0,
        monitor_mode_manual_override: false,
        user_score: None,
        added_at: String::new(),
    }
}

fn item(title: &str) -> RssItem {
    let fields = release_title::release_fields(title);
    RssItem {
        title: title.to_string(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: fields.group,
        resolution: fields.resolution,
        is_batch: fields.is_batch,
        source: RssSource::Nyaa,
    }
}

fn node(id: i64, romaji: &str, format: &str, episodes: i32, year: i32) -> GraphNode {
    GraphNode {
        id,
        title_romaji: romaji.to_string(),
        format: format.to_string(),
        episodes: Some(episodes),
        season_year: Some(year),
        end_year: Some(year),
        ..GraphNode::default()
    }
}

/// Jujutsu Kaisen (113415) and its third season (172463).
fn jjk() -> FranchiseGraph {
    FranchiseGraph {
        nodes: vec![
            node(113415, "Jujutsu Kaisen", "TV", 24, 2020),
            node(
                172463,
                "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen",
                "TV",
                12,
                2026,
            ),
        ],
        edges: vec![GraphEdge {
            from: 172463,
            to: 113415,
            relation: "PREQUEL".to_string(),
        }],
        ..FranchiseGraph::default()
    }
}

fn meta(row: &series::Series, graph: &FranchiseGraph) -> SeriesMeta {
    let franchise = graph.build(row.anilist_id, &[]).map(Arc::new);
    SeriesMeta::from_series(row).with_franchise(franchise)
}

const SEQUEL_RELEASE: &str =
    "[SubsPlease] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 (1080p) [ABCD1234].mkv";

#[test]
fn a_release_the_franchise_gives_an_untracked_entry_matches_nothing() {
    let first = series(1, 113415, "Jujutsu Kaisen", 24, 2020);
    let all = vec![meta(&first, &jjk())];
    let release = item(SEQUEL_RELEASE);
    // The plain matcher takes it for the first season (its core name is
    // inside the title)...
    assert!(best_series_match(&release, &all).is_some_and(|m| m.series.id == 1));
    // ...the franchise says it is the untracked third season's.
    match identified_series_match(&release, &all) {
        SeriesMatch::LooksLike { winner, rival } => {
            assert_eq!(winner.series.id, 1);
            assert_eq!(rival.entry, 172463);
        }
        _ => panic!("expected the third season to be named"),
    }
    // The first season's own releases still match.
    assert!(
        identified_series_match(&item("[SubsPlease] Jujutsu Kaisen - 06 (1080p)"), &all)
            .found()
            .is_some_and(|m| m.series.id == 1)
    );
}

#[test]
fn a_tracked_rival_takes_the_release() {
    let first = series(1, 113415, "Jujutsu Kaisen", 24, 2020);
    let third = series(
        2,
        172463,
        "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen",
        12,
        2026,
    );
    let graph = jjk();
    let all = vec![meta(&first, &graph), meta(&third, &graph)];
    let release = item(SEQUEL_RELEASE);
    // Both rows match it exactly; the oldest row wins the tie...
    assert!(best_series_match(&release, &all).is_some_and(|m| m.series.id == 1));
    // ...and the third season takes it from there.
    let found = identified_series_match(&release, &all)
        .found()
        .expect("the third season takes it");
    assert_eq!(found.series.id, 2);
    assert_eq!(found.resolved_eps.iter().copied().collect::<Vec<_>>(), [6]);
}

#[test]
fn a_year_only_the_remake_fits_drops_the_release_for_the_original() {
    let original = series(1, 120, "Fruits Basket", 26, 2001);
    let graph = FranchiseGraph {
        nodes: vec![
            node(120, "Fruits Basket", "TV", 26, 2001),
            node(105334, "Fruits Basket (2019)", "TV", 25, 2019),
        ],
        edges: vec![GraphEdge {
            from: 120,
            to: 105334,
            relation: "ALTERNATIVE".to_string(),
        }],
        ..FranchiseGraph::default()
    };
    let all = vec![meta(&original, &graph)];
    let remake = item("[Leopard-Raws] Fruits Basket (2019) - 25 END (TX 1280x720 x264 AAC).mp4");
    assert!(matches!(
        identified_series_match(&remake, &all),
        SeriesMatch::LooksLike { .. }
    ));
    // The history reads it the same way.
    let mut history = CanonicalHistory::default();
    history.insert_title(&remake.title, &all);
    assert!(history.items.is_empty());
    // The original's own releases still match.
    assert!(
        identified_series_match(&item("[Group] Fruits Basket - 25 [DVD]"), &all)
            .found()
            .is_some()
    );
}

#[test]
fn without_a_franchise_the_matcher_is_unchanged() {
    let first = series(1, 113415, "Jujutsu Kaisen", 24, 2020);
    let all = vec![SeriesMeta::from_series(&first)];
    let release = item(SEQUEL_RELEASE);
    assert_eq!(
        identified_series_match(&release, &all)
            .found()
            .map(|m| m.series.id),
        best_series_match(&release, &all).map(|m| m.series.id)
    );
}

/// Kaguya-sama's first two seasons: English titles that clean to the
/// same name (`Kaguya-sama: Love is War` and `... Love is War?`), and
/// both fit an episode 5.
fn kaguya() -> FranchiseGraph {
    let node = |id, romaji: &str, english: &str, year| GraphNode {
        title_english: english.to_string(),
        ..node(id, romaji, "TV", 12, year)
    };
    FranchiseGraph {
        nodes: vec![
            node(
                101921,
                "Kaguya-sama wa Kokurasetai: Tensai-tachi no Renai Zunousen",
                "Kaguya-sama: Love is War",
                2019,
            ),
            node(
                112641,
                "Kaguya-sama wa Kokurasetai?: Tensai-tachi no Renai Zunousen",
                "Kaguya-sama: Love is War?",
                2020,
            ),
        ],
        edges: vec![GraphEdge {
            from: 112641,
            to: 101921,
            relation: "PREQUEL".to_string(),
        }],
        ..FranchiseGraph::default()
    }
}

#[test]
fn a_release_two_tracked_series_fit_alike_goes_to_neither() {
    let mut first = series(
        1,
        101921,
        "Kaguya-sama wa Kokurasetai: Tensai-tachi no Renai Zunousen",
        12,
        2019,
    );
    first.title_english = "Kaguya-sama: Love is War".to_string();
    let mut second = series(
        2,
        112641,
        "Kaguya-sama wa Kokurasetai?: Tensai-tachi no Renai Zunousen",
        12,
        2020,
    );
    second.title_english = "Kaguya-sama: Love is War?".to_string();
    let graph = kaguya();
    let release = item("[Group] Kaguya-sama - Love is War - 05 [1080p].mkv");

    // One of them tracked: the tie is with an entry the library doesn't
    // have, and the release is the tracked season's.
    let alone = vec![meta(&first, &graph)];
    assert!(
        identified_series_match(&release, &alone)
            .found()
            .is_some_and(|m| m.series.id == 1)
    );

    // Both tracked: Sonarr's ambiguous release.
    let both = vec![meta(&first, &graph), meta(&second, &graph)];
    assert!(best_series_match(&release, &both).is_some());
    match identified_series_match(&release, &both) {
        SeriesMatch::Shared { other, .. } => assert!(other.starts_with("Kaguya-sama"), "{other}"),
        _ => panic!("expected the release to fit both tracked seasons"),
    }
}
