//! Each tier and the burden-of-proof rule, on hand-built franchises
//! shaped after real ones (episode counts and years as on AniList).

use super::*;

struct Node {
    id: i64,
    romaji: &'static str,
    english: &'static str,
    format: &'static str,
    episodes: i32,
    year: i32,
}

const fn tv(
    id: i64,
    romaji: &'static str,
    english: &'static str,
    episodes: i32,
    year: i32,
) -> Node {
    Node {
        id,
        romaji,
        english,
        format: "TV",
        episodes,
        year,
    }
}

const fn movie(id: i64, romaji: &'static str, year: i32) -> Node {
    Node {
        id,
        romaji,
        english: "",
        format: "MOVIE",
        episodes: 1,
        year,
    }
}

fn graph(nodes: &[Node], chain: &[(i64, i64)]) -> FranchiseGraph {
    FranchiseGraph {
        nodes: nodes
            .iter()
            .map(|n| GraphNode {
                id: n.id,
                title_romaji: n.romaji.to_string(),
                title_english: n.english.to_string(),
                format: n.format.to_string(),
                episodes: Some(n.episodes),
                season_year: Some(n.year),
                ..GraphNode::default()
            })
            .collect(),
        // `(entry, prequel)`.
        edges: chain
            .iter()
            .map(|&(from, to)| GraphEdge {
                from,
                to,
                relation: "PREQUEL".to_string(),
            })
            .collect(),
        ..FranchiseGraph::default()
    }
}

fn verdict(graph: &FranchiseGraph, target: i64, title: &str) -> Verdict {
    identify(&graph.build(target, &[]).expect("target is a node"), title)
}

#[track_caller]
fn assert_rival(v: Verdict, entry: i64, tier: Tier) {
    match v {
        Verdict::Rival(r) => assert_eq!((r.entry, r.tier), (entry, tier), "{}", r.reason),
        other => panic!("expected a {tier:?} rival {entry}, got {other:?}"),
    }
}

#[track_caller]
fn assert_ambiguous(v: Verdict, entry: i64) {
    match v {
        Verdict::Ambiguous { entry: e, .. } => assert_eq!(e, entry),
        other => panic!("expected ambiguous with {entry}, got {other:?}"),
    }
}

fn jjk() -> FranchiseGraph {
    graph(
        &[
            tv(1, "Jujutsu Kaisen", "JUJUTSU KAISEN", 24, 2020),
            tv(
                2,
                "Jujutsu Kaisen 2nd Season",
                "JUJUTSU KAISEN Season 2",
                23,
                2023,
            ),
            tv(
                3,
                "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen",
                "JUJUTSU KAISEN Season 3",
                12,
                2026,
            ),
            movie(4, "Gekijouban Jujutsu Kaisen 0", 2021),
        ],
        &[(2, 1), (3, 2), (2, 4)],
    )
}

#[test]
fn names_more_a_sequel_subtitle_vetoes_the_first_season() {
    assert_rival(
        verdict(
            &jjk(),
            1,
            "[SubsPlease] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 (1080p) [ABCD1234].mkv",
        ),
        3,
        Tier::NamesMore,
    );
    // The sequel itself takes it.
    assert_eq!(
        verdict(
            &jjk(),
            3,
            "[SubsPlease] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 (1080p)"
        ),
        Verdict::Target
    );
}

#[test]
fn an_absolute_number_only_the_target_holds_is_the_targets() {
    assert_eq!(
        verdict(
            &jjk(),
            3,
            "[SubsPlease] Jujutsu Kaisen - 56 (1080p) [ABCD1234].mkv"
        ),
        Verdict::Target
    );
    // The first season's own numbering is its own; a sequel whose names
    // all carry a marker the title doesn't state is no tie.
    assert_eq!(
        verdict(&jjk(), 1, "[SubsPlease] Jujutsu Kaisen - 05 (1080p)"),
        Verdict::Target
    );
}

#[test]
fn a_marker_convention_vetoes_a_part_named_only_by_its_marker() {
    let spy = graph(
        &[
            tv(1, "SPY×FAMILY", "SPY x FAMILY", 12, 2022),
            tv(2, "SPY×FAMILY Part 2", "SPY x FAMILY Part 2", 13, 2022),
            tv(3, "SPY×FAMILY Season 2", "SPY x FAMILY Season 2", 12, 2023),
        ],
        &[(2, 1), (3, 2)],
    );
    assert_rival(
        verdict(&spy, 2, "[SubsPlease] Spy x Family - 05 (1080p)"),
        1,
        Tier::Marker,
    );
    // Past the first part's count the number is Part 2's absolute one.
    assert_eq!(
        verdict(&spy, 2, "[SubsPlease] Spy x Family - 18 (1080p)"),
        Verdict::Target
    );
    // Stating the marker settles it for the part that carries it.
    assert_eq!(
        verdict(&spy, 2, "[SubsPlease] Spy x Family Part 2 - 05 (1080p)"),
        Verdict::Target
    );
}

fn jojo() -> FranchiseGraph {
    let mut g = graph(
        &[
            tv(
                1,
                "JoJo no Kimyou na Bouken",
                "JoJo's Bizarre Adventure (2012)",
                26,
                2012,
            ),
            tv(
                2,
                "JoJo no Kimyou na Bouken: Stardust Crusaders",
                "JoJo's Bizarre Adventure: Stardust Crusaders",
                24,
                2014,
            ),
            Node {
                id: 3,
                romaji: "JoJo no Kimyou na Bouken: Steel Ball Run",
                english: "JoJo's Bizarre Adventure: Steel Ball Run",
                format: "ONA",
                episodes: 11,
                year: 2026,
            },
        ],
        &[(2, 1), (3, 2)],
    );
    g.tvdb = vec![
        TvdbRow {
            id: 1,
            tvdb_id: 262954,
            season: 1,
            episode_offset: Some(0),
        },
        TvdbRow {
            id: 2,
            tvdb_id: 262954,
            season: 2,
            episode_offset: Some(0),
        },
        TvdbRow {
            id: 3,
            tvdb_id: 262954,
            season: 6,
            episode_offset: Some(0),
        },
    ];
    g
}

#[test]
fn names_fully_the_franchise_title_is_the_first_entrys() {
    // #300.
    assert_rival(
        verdict(&jojo(), 3, "[RH] JoJo no Kimyou na Bouken - 01-03 [1080p]"),
        1,
        Tier::NamesFully,
    );
}

#[test]
fn a_tvdb_season_and_show_year_keep_a_sequel_its_releases() {
    assert_eq!(
        verdict(
            &jojo(),
            3,
            "JoJo's Bizarre Adventure (2012) S06E04 1080p WEB H.264-Trix"
        ),
        Verdict::Target
    );
    assert_eq!(
        verdict(
            &jojo(),
            3,
            "JoJo no Kimyou na Bouken S06E03 1080p WEB H.264 -VARYG (Multi-Subs)"
        ),
        Verdict::Target
    );
}

#[test]
fn without_tvdb_rows_a_season_claim_confirms_no_rival() {
    // The same title with the TVDB rows missing: nothing can say season 6
    // is or isn't the 2012 series', so there is no veto.
    let mut g = jojo();
    g.tvdb.clear();
    assert!(!verdict(&g, 3, "JoJo's Bizarre Adventure (2012) S06E04 1080p WEB").is_veto());
}

fn fruits_basket() -> FranchiseGraph {
    graph(
        &[
            tv(1, "Fruits Basket", "Fruits Basket", 26, 2001),
            tv(2, "Fruits Basket (2019)", "Fruits Basket (2019)", 25, 2019),
            tv(
                3,
                "Fruits Basket 2nd Season",
                "Fruits Basket Season 2",
                25,
                2020,
            ),
        ],
        &[(3, 2)],
    )
}

#[test]
fn structure_a_year_the_target_misses_and_the_remake_fits() {
    assert_rival(
        verdict(
            &fruits_basket(),
            1,
            "[Leopard-Raws] Fruits Basket (2019) - 25 END (TX 1280x720 x264 AAC).mp4",
        ),
        2,
        Tier::Structure,
    );
    // No year: the remake and the original fit alike.
    assert_ambiguous(
        verdict(&fruits_basket(), 2, "[Group] Fruits Basket - 05 [1080p]"),
        1,
    );
}

#[test]
fn a_rival_whose_claims_are_unknown_never_vetoes() {
    // The remake's year missing from every field: the title's year
    // confirms nothing, so the 2001 series is not vetoed, only doubted.
    let mut g = fruits_basket();
    g.nodes[1].season_year = None;
    g.nodes[1].title_romaji = "Fruits Basket".to_string();
    g.nodes[1].title_english = "Fruits Basket".to_string();
    let v = verdict(
        &g,
        1,
        "[Leopard-Raws] Fruits Basket (2019) - 25 END (TX 1280x720)",
    );
    assert!(!v.is_veto(), "{v:?}");
}

#[test]
fn ties_between_same_named_entries_are_ambiguous() {
    let hxh = graph(
        &[
            tv(1, "HUNTER×HUNTER", "Hunter x Hunter", 62, 1999),
            tv(
                2,
                "HUNTER×HUNTER (2011)",
                "Hunter x Hunter (2011)",
                148,
                2011,
            ),
        ],
        &[],
    );
    assert_ambiguous(verdict(&hxh, 2, "[DP] Hunter x Hunter - 003 [DVD]"), 1);
    assert_ambiguous(verdict(&hxh, 1, "[DP] Hunter x Hunter - 003 [DVD]"), 2);

    let kaguya = graph(
        &[
            tv(
                1,
                "Kaguya-sama wa Kokurasetai: Tensai-tachi no Renai Zunousen",
                "Kaguya-sama: Love is War",
                12,
                2019,
            ),
            tv(
                2,
                "Kaguya-sama wa Kokurasetai?: Tensai-tachi no Renai Zunousen",
                "Kaguya-sama: Love is War?",
                12,
                2020,
            ),
        ],
        &[(2, 1)],
    );
    let title = "[HorribleSubs] Kaguya-sama wa Kokurasetai - 05 [1080p].mkv";
    assert_ambiguous(verdict(&kaguya, 2, title), 1);
    assert_ambiguous(verdict(&kaguya, 1, title), 2);
}

#[test]
fn another_show_sharing_a_word_is_neither() {
    let mushoku = graph(
        &[
            tv(
                1,
                "Mushoku Tensei: Isekai Ittara Honki Dasu",
                "Mushoku Tensei: Jobless Reincarnation",
                11,
                2021,
            ),
            tv(
                2,
                "Mushoku Tensei II: Isekai Ittara Honki Dasu",
                "Mushoku Tensei: Jobless Reincarnation Season 2",
                12,
                2023,
            ),
        ],
        &[(2, 1)],
    );
    assert_eq!(
        verdict(&mushoku, 1, "[Group] Mushoku no Eiyuu - 05 [1080p]"),
        Verdict::Neither
    );
    assert_eq!(
        verdict(&mushoku, 1, "[Group] Kaiju No. 8 - 05 [1080p]"),
        Verdict::Neither
    );
}

#[test]
fn a_movie_series_part_number_reads_as_its_episode() {
    let kizu = graph(
        &[
            movie(1, "Kizumonogatari I: Tetsuketsu-hen", 2016),
            movie(2, "Kizumonogatari II: Nekketsu-hen", 2016),
            movie(3, "Kizumonogatari III: Reiketsu-hen", 2017),
        ],
        &[(2, 1), (3, 2)],
    );
    let title = "(HI10)_Kizumonogatari_-_02_(BD_720p)_(ReinForce-Tactical).mkv";
    assert_rival(verdict(&kizu, 1, title), 2, Tier::Structure);
    assert_eq!(verdict(&kizu, 2, title), Verdict::Target);
}

#[test]
fn a_pack_across_seasons_spans() {
    let mut aot = graph(
        &[
            tv(1, "Shingeki no Kyojin", "Attack on Titan", 25, 2013),
            tv(
                2,
                "Shingeki no Kyojin Season 2",
                "Attack on Titan Season 2",
                12,
                2017,
            ),
            tv(
                3,
                "Shingeki no Kyojin Season 3",
                "Attack on Titan Season 3",
                12,
                2018,
            ),
        ],
        &[(2, 1), (3, 2)],
    );
    aot.nodes[0].title_native = "進撃の巨人".to_string();
    aot.nodes[1].title_native = "進撃の巨人 Season2".to_string();
    aot.nodes[2].title_native = "進撃の巨人 Season3".to_string();
    assert_eq!(
        verdict(&aot, 2, "[Group] 進撃の巨人 S1-S3 [BDRip 1080p]"),
        Verdict::Spans
    );
    // An absolute range across two seasons.
    assert_eq!(
        verdict(&aot, 1, "[Group] Shingeki no Kyojin 01-37 [BD 1080p]"),
        Verdict::Spans
    );
}

#[test]
fn a_curated_offset_places_a_special_in_the_absolute_numbering() {
    let mut aot = graph(
        &[
            tv(
                5,
                "Shingeki no Kyojin: The Final Season",
                "Attack on Titan Final Season",
                16,
                2020,
            ),
            tv(
                6,
                "Shingeki no Kyojin: The Final Season Part 2",
                "Attack on Titan Final Season Part 2",
                12,
                2022,
            ),
            Node {
                id: 7,
                romaji: "Shingeki no Kyojin: The Final Season - Kanketsu-hen",
                english: "Attack on Titan: The Final Season - The Final Chapters",
                format: "SPECIAL",
                episodes: 2,
                year: 2023,
            },
        ],
        &[(6, 5), (7, 6)],
    );
    aot.offsets = vec![
        OffsetRow {
            id: 5,
            episodes: 59,
        },
        OffsetRow {
            id: 6,
            episodes: 75,
        },
        OffsetRow {
            id: 7,
            episodes: 87,
        },
    ];
    aot.tvdb = [(5, 0), (6, 16), (7, 28)]
        .iter()
        .map(|&(id, offset)| TvdbRow {
            id,
            tvdb_id: 267440,
            season: 4,
            episode_offset: Some(offset),
        })
        .collect();
    let title = "[Group] Shingeki no Kyojin S04E88 [1080p]";
    assert_eq!(verdict(&aot, 7, title), Verdict::Target);
    assert_rival(verdict(&aot, 6, title), 7, Tier::Structure);
}

#[test]
fn names_are_compared_only_on_fields_both_entries_have() {
    // The target has no English title; the rival's English words can't
    // be held against it.
    let kny = graph(
        &[
            tv(1, "Kimetsu no Yaiba", "", 26, 2019),
            tv(
                2,
                "Kimetsu no Yaiba: Yuukaku-hen",
                "Demon Slayer: Kimetsu no Yaiba Entertainment District Arc",
                11,
                2021,
            ),
        ],
        &[(2, 1)],
    );
    let title = "[Group] Demon Slayer Kimetsu no Yaiba - 05 [1080p]";
    assert!(!verdict(&kny, 1, title).is_veto());
    // With an English title of its own, the target says as much.
    let mut full = kny.clone();
    full.nodes[0].title_english = "Demon Slayer: Kimetsu no Yaiba".to_string();
    assert_eq!(verdict(&full, 1, title), Verdict::Target);
}

#[test]
fn the_targets_alternate_titles_count_for_it() {
    let g = jojo();
    let franchise = g
        .build(3, &["JoJo no Kimyou na Bouken".to_string()])
        .unwrap();
    // The user said the franchise title is this series' name too.
    let v = identify(&franchise, "[RH] JoJo no Kimyou na Bouken - 01-03 [1080p]");
    assert!(!v.is_veto(), "{v:?}");
}

#[test]
fn a_title_with_no_name_is_neither() {
    assert_eq!(
        verdict(&jjk(), 1, "S02E04E05.720p.BluRay-FUTV"),
        Verdict::Neither
    );
}
