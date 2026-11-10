//! `best_series_match` reads episodes through `release_title`: a number
//! inside a series' name is never an episode, a bare number right after
//! it is, an absolute number past `cumulative_prior_episodes` maps to the
//! series' own numbering, and an exact tie no longer goes to list order.

use super::super::*;
use crate::services::anilist::{AnimeDetail, RelatedEntry};
use crate::services::identify::{FranchiseGraph, GraphNode, OffsetRow};

fn series(id: i64, title: &str, episodes: i32, cumulative_prior_episodes: i32) -> series::Series {
    series::Series {
        is_adult: false,
        id,
        anilist_id: 100 + id,
        mal_id: None,
        title: title.to_string(),
        title_romaji: title.to_string(),
        title_english: String::new(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "TV".to_string(),
        status: "RELEASING".to_string(),
        episodes: Some(episodes),
        season_year: None,
        end_year: None,
        folder_name: title.to_string(),
        monitor_mode: "all".to_string(),
        allow_upgrades: true,
        allow_pt_upgrades: false,
        custom_query_tokens: String::new(),
        restrict_to_uploader: String::new(),
        alternate_titles: String::new(),
        cumulative_prior_episodes,
        cumulative_offset_source: String::new(),
        cumulative_offset_season: None,
        episode_numbering: String::new(),
        monitor_mode_manual_override: false,
        user_score: None,
        added_at: String::new(),
    }
}

fn item(title: &str) -> RssItem {
    RssItem {
        title: title.to_string(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: "Group".to_string(),
        resolution: "1080".to_string(),
        is_batch: false,
        source: RssSource::Nyaa,
    }
}

fn resolved(found: &MatchResult) -> Vec<i32> {
    let mut eps: Vec<i32> = found.resolved_eps.iter().copied().collect();
    eps.sort_unstable();
    eps
}

#[test]
fn a_number_in_the_series_name_is_not_tagged() {
    let meta = vec![SeriesMeta::from_series(&series(1, "Kaiju No. 8", 12, 0))];
    let found = best_series_match(
        &item("[SubsPlease] Kaiju No. 8 - 10 (1080p) [ABCD1234].mkv"),
        &meta,
    )
    .expect("Kaiju No. 8 matches");
    assert_eq!(resolved(&found), vec![10]);
}

#[test]
fn an_absolute_number_maps_through_the_series_offset() {
    // Jujutsu Kaisen's third season (47 episodes before it) ships as
    // `- 56`: its episode 9.
    let meta = vec![SeriesMeta::from_series(&series(
        1,
        "Jujutsu Kaisen",
        12,
        47,
    ))];
    let found = best_series_match(
        &item("[SubsPlease] Jujutsu Kaisen - 56 (1080p) [ABCD1234].mkv"),
        &meta,
    )
    .expect("Jujutsu Kaisen matches");
    assert_eq!(resolved(&found), vec![9]);
    assert_eq!(found.resolution_mode, "absolute_mapped");
}

#[test]
fn a_bare_number_after_the_series_name_is_its_episode() {
    // EternalAnimes' dashless weekly (#296).
    let meta = vec![SeriesMeta::from_series(&series(1, "Dark Gathering", 25, 0))];
    let found = best_series_match(
        &item("[EA]Dark_Gathering_24_[1920x1080][HEVC][831E9094].mkv"),
        &meta,
    )
    .expect("Dark Gathering matches");
    assert_eq!(resolved(&found), vec![24]);
    assert_eq!(found.parsed.parse_mode, "after_name");
}

#[test]
fn an_exact_tie_goes_to_the_oldest_row_not_list_order() {
    // Two rows with the same name (a re-add) score the same; the older
    // row wins whichever comes first in the list.
    let newer = SeriesMeta::from_series(&series(9, "Show Title", 12, 0));
    let older = SeriesMeta::from_series(&series(3, "Show Title", 12, 0));
    let found = best_series_match(
        &item("[Group] Show Title - 05 (1080p)"),
        &[newer.clone(), older.clone()],
    )
    .expect("matches");
    assert_eq!(found.series.id, 3);
    let found = best_series_match(&item("[Group] Show Title - 05 (1080p)"), &[older, newer])
        .expect("matches");
    assert_eq!(found.series.id, 3);
}

#[test]
fn a_later_season_pack_never_matches_a_series_named_by_no_season() {
    // Nothing on disk for a finished series makes the blind-batch path
    // grab whatever matches: another season's pack must not.
    let mut mob = series(1, "Mob Psycho 100", 12, 0);
    mob.status = "FINISHED".to_string();
    let mut kaiju = series(2, "Kaiju No. 8", 12, 0);
    kaiju.status = "FINISHED".to_string();
    let meta = vec![
        SeriesMeta::from_series(&mob),
        SeriesMeta::from_series(&kaiju),
    ];
    for title in [
        "[DIY] Mob Psycho 100 III (BD 1080p HEVC FLAC) [Dual-Audio]",
        "[Group] Kaiju No. 8 S2 (BD 1080p)",
    ] {
        let mut pack = item(title);
        pack.is_batch = true;
        assert!(best_series_match(&pack, &meta).is_none(), "{title}");
    }
    // Its own first season still matches.
    assert!(best_series_match(&item("[Group] Kaiju No. 8 S1 (BD 1080p)"), &meta).is_some());
}

#[test]
fn a_part_dash_episode_scores_as_a_season_dash_episode() {
    let item = item("[Group] Show Part 2 - 05 (1080p)");
    assert_eq!(parse_release(&item).parse_mode, "season_dash_episode");
}

#[test]
fn a_bracket_only_item_matches_no_series_by_its_empty_name() {
    // `[Group][Show][01][1080p]` normalizes to nothing, and an empty
    // title is contained in every alias: it used to score 1.0 against
    // every library row.
    let mut oshi = series(1, "Oshi no Ko", 11, 0);
    oshi.title_english = "[Oshi No Ko]".to_string();
    let meta = vec![
        SeriesMeta::from_series(&series(2, "Kaiju No. 8", 12, 0)),
        SeriesMeta::from_series(&oshi),
    ];
    assert!(best_series_match(&item("[Group][Show][01][1080p]"), &meta).is_none());
    // Where the parser reads a series title, the item matches by it.
    let found = best_series_match(&item("[SubsPlease][Oshi no Ko][01][1080p]"), &meta)
        .expect("the series title names Oshi no Ko");
    assert_eq!(found.series.id, 1);
    assert_eq!(resolved(&found), vec![1]);
    // The bracketed English title matches Oshi no Ko's releases only.
    let found = best_series_match(&item("[SubsPlease] Oshi no Ko - 05 (1080p)"), &meta)
        .expect("Oshi no Ko matches");
    assert_eq!(found.series.id, 1);
    assert!(
        best_series_match(&item("[SubsPlease] Kaiju No. 8 - 05 (1080p)"), &meta)
            .is_some_and(|found| found.series.id == 2)
    );
}

/// #303: Steel Ball Run 2nd - 3rd STAGE (offset 1 from its split) reads
/// every bare number absolute only, and 1st STAGE takes its own `- 01`.
#[test]
fn a_split_part_reads_feed_numbers_absolute_only() {
    let mut second = series(2, "Steel Ball Run", 11, 1);
    second.cumulative_offset_source = "split".to_string();
    second.cumulative_offset_season = Some(6);
    let first = series(1, "Steel Ball Run", 1, 0);
    let title = |n: &str| format!("[Erai-raws] Steel Ball Run - {n} [1080p NF WEB-DL AVC AAC]");

    let found = best_series_match(&item(&title("07")), &[SeriesMeta::from_series(&second)])
        .expect("- 07 matches 2nd - 3rd STAGE");
    assert_eq!(resolved(&found), vec![6]);
    assert_eq!(found.resolution_mode, "absolute_mapped");

    // `- 01` is no episode of the split part.
    let alone = best_series_match(&item(&title("01")), &[SeriesMeta::from_series(&second)]);
    assert!(
        alone.as_ref().is_none_or(|m| m.resolved_eps.is_empty()),
        "{:?}",
        alone.as_ref().map(|m| m.resolution_mode)
    );
    // With 1st STAGE tracked too, it is 1st STAGE's episode 1.
    let both = [
        SeriesMeta::from_series(&second),
        SeriesMeta::from_series(&first),
    ];
    let found = best_series_match(&item(&title("01")), &both).expect("1st STAGE takes - 01");
    assert_eq!(found.series.id, 1);
    assert_eq!(resolved(&found), vec![1]);
    // Its logical key is 1st STAGE's episode 1; 2nd - 3rd STAGE's `- 02`
    // (its episode 1) is another item, not a second copy of it.
    let first_key: Vec<i32> = found.canonical_abs_eps.iter().copied().collect();
    let found = best_series_match(&item(&title("02")), &both).expect("- 02 matches");
    assert_eq!(found.series.id, 2);
    assert_eq!(resolved(&found), vec![1]);
    let second_key: Vec<i32> = found.canonical_abs_eps.iter().copied().collect();
    assert_ne!(first_key, second_key);

    // TVDB's season 6 tags count on from 1st STAGE; a season marker
    // before a bare number is a bare number; another season's tag counts
    // within it.
    let alone = [SeriesMeta::from_series(&second)];
    for (title, eps) in [
        (
            "[DKB] Steel Ball Run: JoJo no Kimyou na Bouken - S06E06 [1080p]",
            vec![5],
        ),
        ("[Group] Steel Ball Run S1 - 07 [1080p]", vec![6]),
        ("[Group] Steel Ball Run S01E06 [1080p]", vec![6]),
    ] {
        let found = best_series_match(&item(title), &alone).expect(title);
        assert_eq!(resolved(&found), eps, "{title}");
    }

    // With 1st STAGE tracked under the same alias, a season-1-shaped
    // title still goes to the part that holds the episode: 1st STAGE has
    // one episode, so its explicit `S01E06` places nothing.
    for (title, eps) in [
        ("[Group] Steel Ball Run S1 - 07 [1080p]", vec![6]),
        ("[Group] Steel Ball Run S01E06 [1080p]", vec![6]),
    ] {
        let found = best_series_match(&item(title), &both).expect(title);
        assert_eq!(found.series.id, 2, "{title}");
        assert_eq!(resolved(&found), eps, "{title}");
    }

    // AniList numbering chosen on the series page: the offset is never
    // applied.
    let mut anilist = second.clone();
    anilist.episode_numbering = "anilist".to_string();
    let found = best_series_match(&item(&title("07")), &[SeriesMeta::from_series(&anilist)])
        .expect("- 07 matches");
    assert_eq!(resolved(&found), vec![7]);
}

// ── #303: Steel Ball Run under its real titles ─────────────────────────────

/// A vendored franchise graph (`tests/fixtures/franchises/<name>.json`).
fn franchise_graph(name: &str) -> FranchiseGraph {
    #[derive(serde::Deserialize)]
    struct Fixture {
        #[serde(flatten)]
        graph: FranchiseGraph,
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/franchises/{name}.json"));
    let text = std::fs::read_to_string(path).expect("franchise fixture");
    serde_json::from_str::<Fixture>(&text)
        .expect("franchise fixture parses")
        .graph
}

/// The vendored JoJo franchise graph.
fn jojo_graph() -> FranchiseGraph {
    franchise_graph("jojo")
}

fn graph_node(graph: &FranchiseGraph, id: i64) -> &GraphNode {
    graph.nodes.iter().find(|n| n.id == id).expect("node")
}

/// The AniList detail the metadata cache holds for entry `id`: its
/// titles, synonyms and its relations as the graph's edges list them.
fn cached_detail(graph: &FranchiseGraph, id: i64) -> AnimeDetail {
    let node = graph_node(graph, id);
    let relations = graph
        .edges
        .iter()
        .filter(|e| e.from == id)
        .map(|e| {
            let to = graph_node(graph, e.to);
            RelatedEntry {
                id: to.id,
                id_mal: to.mal_id,
                title_romaji: to.title_romaji.clone(),
                title_english: to.title_english.clone(),
                title_native: to.title_native.clone(),
                cover_url: String::new(),
                format: to.format.clone(),
                status: "FINISHED".to_string(),
                status_display: String::new(),
                episodes: to.episodes,
                relation_type: e.relation.clone(),
                season_year: to.season_year,
                media_type: "ANIME".to_string(),
            }
        })
        .collect();
    AnimeDetail {
        id,
        id_mal: node.mal_id,
        title_romaji: node.title_romaji.clone(),
        title_english: node.title_english.clone(),
        title_native: node.title_native.clone(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: node.format.clone(),
        status: "RELEASING".to_string(),
        status_display: String::new(),
        episodes: node.episodes,
        duration: None,
        season: String::new(),
        season_year: node.season_year,
        end_year: node.end_year,
        is_adult: false,
        description: String::new(),
        genres: Vec::new(),
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: node.synonyms.clone(),
        streaming_episodes: Vec::new(),
        relations,
    }
}

const SBR_1ST: i64 = 190327;
const SBR_2ND: i64 = 210482;

/// MAL 61469's titles, which `mal_entry_titles` stores for both parts.
fn sbr_mal_titles() -> Vec<String> {
    vec![
        "Steel Ball Run: JoJo no Kimyou na Bouken".to_string(),
        "STEEL BALL RUN JoJo's Bizarre Adventure".to_string(),
    ]
}

/// The library rows for 1st STAGE (id 1) and 2nd - 3rd STAGE (id 2) as
/// `series_metas` builds them: the MAL entry's titles as aliases when
/// `with_mal_titles`, the entry names and franchise words from the
/// cached detail, and the franchise.
fn sbr_metas(graph: &FranchiseGraph, with_mal_titles: bool) -> Vec<SeriesMeta> {
    let row = |id: i64, anilist_id: i64| {
        let node = graph_node(graph, anilist_id);
        let mut row = series(id, &node.title_romaji, node.episodes.unwrap_or(0), 0);
        row.anilist_id = anilist_id;
        row.mal_id = node.mal_id;
        row.title_english = node.title_english.clone();
        row.title_native = node.title_native.clone();
        row.format = node.format.clone();
        row
    };
    let first = row(1, SBR_1ST);
    let mut second = row(2, SBR_2ND);
    second.cumulative_prior_episodes = 1;
    second.cumulative_offset_source = "split".to_string();
    second.cumulative_offset_season = Some(6);
    let extra = if with_mal_titles {
        sbr_mal_titles()
    } else {
        Vec::new()
    };
    [first, second]
        .iter()
        .map(|row| {
            // The library row's stored offset stands in for the target's,
            // as `identify::load_graph` has it.
            let mut graph = graph.clone();
            if row.cumulative_prior_episodes > 0 {
                graph.offsets.push(OffsetRow {
                    id: row.anilist_id,
                    episodes: row.cumulative_prior_episodes,
                });
            }
            SeriesMeta::from_series_with(row, &extra)
                .with_detail(Some(&cached_detail(&graph, row.anilist_id)))
                .with_franchise(graph.build(row.anilist_id, &[]).map(Arc::new))
        })
        .collect()
}

/// The series id and episodes the sweep takes a title for, or `None`
/// when no row takes it, `identify` hands it to no row, or the winner's
/// name is not exact (the sweep's "Title does not name the series
/// exactly").
fn sweep_takes(title: &str, metas: &[SeriesMeta]) -> Option<(i64, Vec<i32>)> {
    let best = best_series_match(&item(title), metas);
    let identified = identified_series_match(&item(title), metas).found();
    assert_eq!(
        best.as_ref().map(|m| (m.series.id, resolved(m))),
        identified.as_ref().map(|m| (m.series.id, resolved(m))),
        "identify moved {title}"
    );
    identified
        .filter(|m| m.alias_score >= 1.0)
        .map(|m| (m.series.id, resolved(&m)))
}

#[test]
fn steel_ball_run_feed_titles_go_to_the_part_that_holds_them() {
    let graph = jojo_graph();
    let metas = sbr_metas(&graph, true);
    for (title, wanted) in [
        // The franchise first, the entry name after it: no alias holds
        // it whole, the entry name does.
        (
            "[Erai-raws] JoJo no Kimyou na Bouken: Steel Ball Run - 09 [1080p NF WEB-DL AVC AAC][MultiSub]",
            Some((2, vec![8])),
        ),
        (
            "[Erai-raws] JoJo no Kimyou na Bouken: Steel Ball Run - 01 [1080p NF WEB-DL AVC AAC][MultiSub]",
            Some((1, vec![1])),
        ),
        // MAL's romaji, TVDB's season 6 numbering.
        (
            "[DKB] Steel Ball Run: JoJo no Kimyou na Bouken - S06E10 [1080p][HEVC x265 10bit]",
            Some((2, vec![9])),
        ),
        // MAL's English.
        (
            "[SubsPlease] Steel Ball Run - JoJo's Bizarre Adventure - 11 (1080p)",
            Some((2, vec![10])),
        ),
        // Both rows' entry name; an explicit tag is taken as written and
        // only 2nd - 3rd STAGE holds an episode 9.
        (
            "[Judas] JoJo no Kimyou na Bouken - Steel Ball Run - S01E09 [1080p]",
            Some((2, vec![9])),
        ),
        // The franchise name with TVDB's season 6 tag is the split part
        // that counts in that season.
        (
            "[Feibanyama] JoJos Bizarre Adventure S06E08 [NF WebRip 1080p H265]",
            Some((2, vec![7])),
        ),
        (
            "[Judas] JoJo's Bizarre Adventure (JoJo no Kimyou na Bouken) - S06E04 [1080p]",
            Some((2, vec![3])),
        ),
        // Without the season it names no entry of the franchise, and
        // another season's tag is another part's.
        ("[Group] JoJo's Bizarre Adventure - 08 [1080p]", None),
        ("[Group] JoJo's Bizarre Adventure - S05E08 [1080p]", None),
    ] {
        assert_eq!(sweep_takes(title, &metas), wanted, "{title}");
    }

    // MAL's own titles are aliases of both parts: these name them by an
    // alias, not just by the entry name.
    for title in [
        "[DKB] Steel Ball Run: JoJo no Kimyou na Bouken - S06E10 [1080p][HEVC x265 10bit]",
        "[SubsPlease] Steel Ball Run - JoJo's Bizarre Adventure - 11 (1080p)",
    ] {
        for meta in &metas {
            assert!(alias_score(title, meta) >= 1.0, "{title}");
        }
        for meta in &sbr_metas(&graph, false) {
            assert!(alias_score(title, meta) < 1.0, "{title}");
        }
    }
}

/// `series_metas` reads a split part's MAL entry titles
/// (`mal_entry_titles`) and every row's cached detail from the database:
/// both parts take the MAL titles, a MAL-fallback row of the same MAL id
/// does not (its titles are MAL's already), and the cached details give
/// the entry names.
#[tokio::test]
async fn series_metas_reads_the_mal_entry_titles_and_the_cached_details() {
    use crate::models::{mal_entry_titles, metadata_cache};
    let db = crate::test_support::in_memory_pool().await;
    let graph = jojo_graph();
    let insert = |anilist_id: i64, title: String, episodes: i32, offset: i32| {
        let db = db.clone();
        async move {
            sqlx::query(
                "INSERT INTO series (anilist_id, mal_id, title, title_romaji, folder_name, format, \
                 episodes, cumulative_prior_episodes, cumulative_offset_source, cumulative_offset_season) \
                 VALUES (?, 61469, ?, ?, ?, 'ONA', ?, ?, ?, ?)",
            )
            .bind(anilist_id)
            .bind(&title)
            .bind(&title)
            .bind(&title)
            .bind(episodes)
            .bind(offset)
            .bind(if offset > 0 { "split" } else { "" })
            .bind((offset > 0).then_some(6))
            .execute(&db)
            .await
            .expect("insert series");
            sqlx::query_scalar::<_, i64>("SELECT id FROM series WHERE anilist_id = ?")
                .bind(anilist_id)
                .fetch_one(&db)
                .await
                .expect("series id")
        }
    };
    let title = |id: i64| graph_node(&graph, id).title_romaji.clone();
    let first = insert(SBR_1ST, title(SBR_1ST), 1, 0).await;
    let second = insert(SBR_2ND, title(SBR_2ND), 11, 1).await;
    let fallback = insert(-61469, "Some MAL Fallback Row".to_string(), 12, 0).await;
    for (series_id, anilist_id) in [(first, SBR_1ST), (second, SBR_2ND)] {
        metadata_cache::upsert(
            &db,
            series_id,
            anilist_id,
            Some(61469),
            &cached_detail(&graph, anilist_id),
        )
        .await
        .expect("cache detail");
    }
    mal_entry_titles::set(&db, 61469, &sbr_mal_titles())
        .await
        .expect("store MAL titles");

    let rows = series::get_all(&db).await.expect("rows");
    let metas = series_metas(&db, &rows).await;
    let meta = |id: i64| metas.iter().find(|m| m.series.id == id).expect("meta");
    let has_form = |m: &SeriesMeta, form: &str| m.alias_forms.iter().any(|f| f.normalized == form);
    for id in [first, second] {
        assert!(has_form(
            meta(id),
            "steel ball run jojo no kimyou na bouken"
        ));
        assert!(has_form(
            meta(id),
            "steel ball run jojo s bizarre adventure"
        ));
        assert!(
            meta(id)
                .entry_names
                .iter()
                .any(|n| n.normalized == "steel ball run")
        );
        assert!(meta(id).franchise_words.contains("bouken"));
    }
    assert!(!has_form(
        meta(fallback),
        "steel ball run jojo no kimyou na bouken"
    ));
    assert!(meta(fallback).entry_names.is_empty());

    let found = identified_series_match(
        &item("[DKB] Steel Ball Run: JoJo no Kimyou na Bouken - S06E10 [1080p][HEVC x265 10bit]"),
        &metas,
    )
    .found()
    .expect("matches");
    assert_eq!((found.series.id, resolved(&found)), (second, vec![9]));
    assert!(found.alias_score >= 1.0);
}

#[test]
fn an_entry_name_beside_franchise_words_names_the_entry() {
    // No MAL titles here: the entry name alone has to carry these.
    let graph = jojo_graph();
    let metas = sbr_metas(&graph, false);
    for meta in &metas {
        assert!(
            meta.entry_names
                .iter()
                .any(|n| n.normalized == "steel ball run"),
            "{:?}",
            meta.entry_names
        );
        assert!(
            !meta
                .entry_names
                .iter()
                .any(|n| n.normalized == "2nd 3rd stage"),
            "a stage marker is no entry name: {:?}",
            meta.entry_names
        );
    }
    for (title, wanted) in [
        (
            "[Erai-raws] JoJo no Kimyou na Bouken: Steel Ball Run - 09 [1080p NF WEB-DL AVC AAC][MultiSub]",
            Some((2, vec![8])),
        ),
        (
            "[Judas] JoJo no Kimyou na Bouken - Steel Ball Run - S01E09 [1080p]",
            Some((2, vec![9])),
        ),
        // The entry's words among others name another show.
        ("[Fan] Steel Ball Run Abridged - 09 [1080p]", None),
        (
            "[Fan] Some Other Show: Steel Ball Run Fan Cut - 09 [1080p]",
            None,
        ),
    ] {
        assert_eq!(sweep_takes(title, &metas), wanted, "{title}");
    }

    // The franchise name after the entry name. The franchise and entry
    // names are anchors too, so the number right after the franchise name
    // is the (absolute) episode: 2nd - 3rd STAGE's first.
    let title = "[Some-Stuffs] Steel Ball Run - JoJo's Bizarre Adventure 02";
    let found = best_series_match(&item(title), &metas).expect(title);
    assert_eq!((found.series.id, resolved(&found)), (2, vec![1]), "{title}");
    assert!(found.alias_score >= 1.0, "{}", found.alias_score);
    // `identify` reads no episode claim in a bare trailing number, and
    // both parts carry the synonym that names it: a tie with 1st STAGE,
    // which the library tracks, so the sweep takes it for neither.
    assert!(
        matches!(
            identified_series_match(&item(title), &metas),
            SeriesMatch::Shared { .. }
        ),
        "{title}"
    );
}

/// "Franchise Name: Entry Words Here", whose prequel is "Franchise Name:
/// Earlier Entry".
fn entry_and_prequel() -> FranchiseGraph {
    let node = |id: i64, romaji: &str| GraphNode {
        id,
        title_romaji: romaji.to_string(),
        format: "TV".to_string(),
        episodes: Some(12),
        ..GraphNode::default()
    };
    FranchiseGraph {
        nodes: vec![
            node(10, "Franchise Name: Earlier Entry"),
            node(11, "Franchise Name: Entry Words Here"),
        ],
        edges: vec![crate::services::identify::GraphEdge {
            from: 11,
            to: 10,
            relation: "PREQUEL".to_string(),
        }],
        ..FranchiseGraph::default()
    }
}

#[test]
fn a_real_alias_wins_a_tie_over_an_entry_name() {
    let graph = entry_and_prequel();
    let mut by_entry_name = series(1, "Franchise Name: Entry Words Here", 12, 0);
    by_entry_name.anilist_id = 11;
    let by_entry_name =
        SeriesMeta::from_series(&by_entry_name).with_detail(Some(&cached_detail(&graph, 11)));
    let by_alias = SeriesMeta::from_series(&series(2, "Entry Words Here", 12, 0));
    let title = "[Group] Entry Words Here - Franchise Name - 05 (1080p)";

    // Alone, the entry name names the series exactly.
    let found = best_series_match(&item(title), std::slice::from_ref(&by_entry_name))
        .expect("the entry name matches");
    assert!(found.alias_score >= 1.0, "{}", found.alias_score);
    assert_eq!(resolved(&found), vec![5]);
    // Without the detail there is no entry name, and the words alone
    // fall short of exact.
    let bare = SeriesMeta::from_series(&by_entry_name.series);
    assert!(
        best_series_match(&item(title), &[bare]).is_none_or(|m| m.alias_score < 1.0),
        "{title}"
    );

    // Beside a row whose own title is in the release, that row wins the
    // tie although it is the newer row.
    for metas in [
        [by_entry_name.clone(), by_alias.clone()],
        [by_alias.clone(), by_entry_name.clone()],
    ] {
        let found = best_series_match(&item(title), &metas).expect("matches");
        assert_eq!(found.series.id, 2, "{title}");
    }
}

// ── The item's title inside an alias ───────────────────────────────────────

/// `score_alias_overlap` for one title against one row.
fn alias_score(title: &str, meta: &SeriesMeta) -> f32 {
    let parsed = parse_release(&item(title));
    let tokens = auto_search::token_set(&parsed.normalized_title);
    let core_tokens = auto_search::token_set(&parsed.core_title);
    let view = ItemView {
        normalized: &parsed.normalized_title,
        tokens: &tokens,
        core: &parsed.core_title,
        core_tokens: &core_tokens,
        collapsed: &parsed.collapsed_title,
        collapsed_core: &parsed.collapsed_core_title,
    };
    score_alias_overlap(
        &view,
        &AliasSet {
            alias_forms: &meta.alias_forms,
            core_aliases: &meta.core_aliases,
            core_tokens: &meta.core_tokens,
            collapsed_aliases: &meta.collapsed_aliases,
            collapsed_core_aliases: &meta.collapsed_core_aliases,
            franchise_words: &meta.franchise_words,
            own_season_words: meta.own_season_words_for(parsed.season_hint),
        },
    )
}

#[test]
fn a_title_inside_an_alias_counts_only_on_word_boundaries_with_nothing_more_left() {
    // Season and part markers and generic words are all an alias may hold
    // beyond the title (the core forms already drop `2nd Season` and the
    // like; `II` and `the Movie` stay in them).
    let second = SeriesMeta::from_series(&series(1, "Mob Psycho 100 II", 13, 12));
    assert!(alias_score("[Group] Mob Psycho 100 (BD 1080p)", &second) >= 1.0);
    let movie = SeriesMeta::from_series(&series(1, "Kimetsu no Yaiba the Movie", 1, 0));
    assert!(alias_score("[Group] Kimetsu no Yaiba [01]", &movie) >= 1.0);
    // Any other word left over is the alias naming more than the title.
    let gekijouban = SeriesMeta::from_series(&series(1, "Gekijouban Kimetsu no Yaiba", 1, 0));
    assert!(alias_score("[Group] Kimetsu no Yaiba [01]", &gekijouban) < 1.0);

    // A number the alias holds is part of its name: "Mob Psycho" is not
    // "Mob Psycho 100 II", "Kaiju" is not "Kaiju No. 8", and "Ju No 8" is
    // no run of its words at all.
    assert!(alias_score("[Group] Mob Psycho (BD 1080p)", &second) < 1.0);
    let kaiju = SeriesMeta::from_series(&series(1, "Kaiju No. 8", 12, 0));
    assert!(alias_score("[Group] Kaiju [01][1080p]", &kaiju) < 0.82);
    assert!(alias_score("[Group] Ju No 8 [01][1080p]", &kaiju) < 0.82);

    // Spacing and punctuation aside: "ReZero" is the alias's "Re:Zero".
    let rezero = SeriesMeta::from_series(&series(
        1,
        "Re:Zero kara Hajimeru Isekai Seikatsu the Movie",
        1,
        0,
    ));
    assert!(alias_score("[Group] ReZero kara Hajimeru Isekai Seikatsu [01]", &rezero) >= 1.0);
}

#[test]
fn a_franchise_name_alone_is_not_inside_an_entry_alias() {
    // Used to score 1.0: the collapsed franchise name sat inside the
    // collapsed MAL English title with no word boundary to stop it.
    let graph = jojo_graph();
    let metas = sbr_metas(&graph, true);
    for meta in &metas {
        assert!(
            alias_score(
                "[Feibanyama] JoJos Bizarre Adventure S05E08 [NF WebRip 1080p H265]",
                meta
            ) < 0.82,
            "{}",
            meta.series.id
        );
        assert!(
            alias_score("[Group] JoJo's Bizarre Adventure [08][1080p]", meta) < 0.82,
            "{}",
            meta.series.id
        );
    }
}

#[test]
fn an_alias_may_hold_its_franchise_words_beyond_the_title() {
    // "Steel Ball Run" inside MAL's "Steel Ball Run: JoJo no Kimyou na
    // Bouken": the rest is the franchise's name, known from the detail.
    let graph = jojo_graph();
    let with_detail = &sbr_metas(&graph, true)[1];
    let title = "[Group] Steel Ball Run [05][1080p]";
    assert!(alias_score(title, with_detail) >= 1.0);
    // Without the detail the franchise's words are unknown.
    let no_detail = SeriesMeta::from_series_with(&with_detail.series, &sbr_mal_titles());
    assert!(alias_score(title, &no_detail) < 1.0);
}

// ── A season the series' own names give ─────────────────────────────────────

/// Library rows for entries of a vendored franchise graph, as
/// `series_metas` builds them from their cached details: `(library id,
/// AniList id)` pairs.
fn fixture_metas(graph: &FranchiseGraph, rows: &[(i64, i64)]) -> Vec<SeriesMeta> {
    rows.iter()
        .map(|&(id, anilist_id)| {
            let node = graph_node(graph, anilist_id);
            let mut row = series(id, &node.title_romaji, node.episodes.unwrap_or(0), 0);
            row.anilist_id = anilist_id;
            row.mal_id = node.mal_id;
            row.title_english = node.title_english.clone();
            row.title_native = node.title_native.clone();
            row.format = node.format.clone();
            row.episodes = node.episodes;
            SeriesMeta::from_series(&row)
                .with_detail(Some(&cached_detail(graph, anilist_id)))
                .with_franchise(graph.build(anilist_id, &[]).map(Arc::new))
        })
        .collect()
}

/// A sequel's weekly release often names only the franchise and the
/// season (`JUJUTSU KAISEN S03E08`). No alias of the third season holds
/// that name with nothing more left over, but its own names say season
/// 3 ("JUJUTSU KAISEN Season 3: The Culling Game Part 1"), so the rest
/// of an alias may be its entry name. The pre-#303 matcher took these
/// through any substring; the word-boundary rule lost them.
#[test]
fn a_season_the_series_names_lets_an_alias_hold_its_entry_name() {
    let graph = franchise_graph("jujutsu-kaisen");
    let metas = fixture_metas(&graph, &[(1, 113415), (2, 145064), (3, 172463)]);
    assert_eq!(metas[2].named_season, Some(3));
    for (title, wanted) in [
        (
            "[Breeze] JUJUTSU KAISEN S03E08 [1080p AV1] (weekly)",
            Some((3, vec![8])),
        ),
        (
            "[Anime Time] Jujutsu Kaisen - S03E01 [1080p][HEVC 10bit x265][AAC][Dual-Audio] [Multi Sub] [Weekly]",
            Some((3, vec![1])),
        ),
        (
            "[Breeze] JUJUTSU KAISEN S03 [1080p AV1][Dual Audio]",
            Some((3, vec![])),
        ),
        // Each season still takes its own.
        ("[Group] Jujutsu Kaisen S02E08 [1080p]", Some((2, vec![8]))),
    ] {
        assert_eq!(sweep_takes(title, &metas), wanted, "{title}");
    }
    // Without the season, or with another one, the entry name is the
    // alias naming more than the release.
    for title in [
        "[Group] Jujutsu Kaisen - 08 [1080p]",
        "[Group] Jujutsu Kaisen S02E08 [1080p]",
        "[Group] Jujutsu Kaisen S04E08 [1080p]",
    ] {
        assert!(alias_score(title, &metas[2]) < 1.0, "{title}");
    }

    let graph = franchise_graph("mushoku-tensei");
    let metas = fixture_metas(&graph, &[(1, 108465), (2, 146065), (3, 178789)]);
    let title = "[Judas] Mushoku Tensei (Jobless Reincarnation) - S03E11 [1080p][HEVC x265 10bit][Multi-Subs] (Weekly)";
    assert_eq!(sweep_takes(title, &metas), Some((3, vec![11])), "{title}");
}

/// The season's own number may be left over too: "Mairimashita!
/// Iruma-kun 4" is the fourth season by its English title.
#[test]
fn a_season_the_series_names_lets_an_alias_hold_its_number() {
    let mut first = series(1, "Mairimashita! Iruma-kun", 23, 0);
    first.title_english = "Welcome to Demon School! Iruma-kun".to_string();
    let mut fourth = series(4, "Mairimashita! Iruma-kun 4", 24, 0);
    fourth.title_english = "Welcome to Demon School! Iruma-kun Season 4".to_string();
    let metas = [
        SeriesMeta::from_series(&first),
        SeriesMeta::from_series(&fourth),
    ];
    let title = "[DKB] Mairimashita! Iruma-kun - S04E23 [1080p][HEVC x265 10bit][Dual-Audio][Multi-Subs][weekly]";
    assert_eq!(sweep_takes(title, &metas), Some((4, vec![23])), "{title}");
    // Another season's tag: the 4 is the alias naming more.
    let title = "[DKB] Mairimashita! Iruma-kun - S03E05 [1080p]";
    assert!(alias_score(title, &metas[1]) < 1.0, "{title}");
}

/// A season read only from `Part N` / `Cour N` is no season the series
/// names (a movie's second part, a split cour) and lets an alias hold no
/// more. A split part's TVDB season is not in its names either, but a
/// release tagged with it is the part's (#303).
#[test]
fn a_part_is_no_season_the_series_names_and_a_split_season_tag_is() {
    assert_eq!(
        named_season_marker("Made in Abyss: Mezameru Shinpi Part 2"),
        None
    );
    assert_eq!(
        release_title::season_marker("Made in Abyss: Mezameru Shinpi Part 2"),
        Some(2)
    );
    assert_eq!(
        named_season_marker("JUJUTSU KAISEN Season 3: The Culling Game Part 1"),
        Some(3)
    );
    assert_eq!(
        named_season_marker(
            "Dungeon ni Deai wo Motomeru no wa Machigatteiru Darou ka V: Houjou no Megami-hen"
        ),
        Some(5)
    );

    let movie = SeriesMeta::from_series(&series(1, "Made in Abyss: Mezameru Shinpi Part 2", 1, 0));
    assert_eq!((movie.season_num, movie.named_season), (Some(2), None));
    assert!(
        alias_score(
            "[Vodes] Made in Abyss - S02E01 (v0) [WEB 1080p HEVC]",
            &movie
        ) < 1.0
    );

    // Steel Ball Run 2nd - 3rd STAGE counts in TVDB's season 6, which
    // its names never say. The tag opens the rule for that part alone.
    let metas = sbr_metas(&jojo_graph(), true);
    assert_eq!(metas[1].series.numbering().tag_season, Some(6));
    assert_eq!(metas[1].named_season, None);
    let tagged = "[Feibanyama] JoJos Bizarre Adventure S06E08 [NF WebRip 1080p H265]";
    assert!(alias_score(tagged, &metas[1]) >= 1.0);
    assert!(alias_score(tagged, &metas[0]) < 0.82);
    // The AniList titles alone hold it too, before the MAL titles are stored.
    let anilist_only = sbr_metas(&jojo_graph(), false);
    assert!(alias_score(tagged, &anilist_only[1]) >= 1.0);
}

/// "Part 1 & 2" in a series' own title leaves its core whole: the
/// regex's `part\s*\d+` used to leave the "2", and the season 4 weekly
/// went to "4th Season Part 3" instead.
#[test]
fn a_list_of_parts_in_a_title_leaves_its_core_whole() {
    let mut both = series(
        1,
        "Tensei Shitara Slime Datta Ken 4th Season Part 1 & 2",
        24,
        0,
    );
    both.title_english = "That Time I Got Reincarnated as a Slime Season 4".to_string();
    let mut third = series(2, "Tensei Shitara Slime Datta Ken 4th Season Part 3", 0, 0);
    third.episodes = None;
    let metas = [
        SeriesMeta::from_series(&both),
        SeriesMeta::from_series(&third),
    ];
    assert!(
        metas[0]
            .core_aliases
            .iter()
            .any(|c| c == "tensei shitara slime datta ken"),
        "{:?}",
        metas[0].core_aliases
    );
    let title = "[DKB] Tensei shitara Slime Datta Ken - S04E23 [1080p][HEVC x265 10bit][Multi-Subs][weekly]";
    for meta in &metas {
        assert!(alias_score(title, meta) >= 1.0, "{}", meta.series.title);
    }
    // Both rows hold it exactly, as before the word-boundary rule; the
    // tie goes to the older row, as it did then.
    assert_eq!(sweep_takes(title, &metas), Some((1, vec![23])), "{title}");
    // A release title's part number is still its own.
    assert_eq!(normalize_core_title("show part 2 05"), "show 05");
}

/// Beside an entry name a release may write the franchise's words, glued
/// or spaced, and generic words; never a sequence marker, which names
/// another entry of it.
#[test]
fn only_franchise_and_generic_words_stand_beside_an_entry_name() {
    let graph = jojo_graph();
    let metas = sbr_metas(&graph, false);
    // "JoJos" is "JoJo s" glued.
    let title = "[Feibanyama] JoJos Bizarre Adventure Steel Ball Run - 05 [NF WebRip 1080p H265]";
    assert!(
        metas[1].named_by_entry_name(&parse_release(&item(title))),
        "{title}"
    );
    assert_eq!(sweep_takes(title, &metas), Some((2, vec![4])), "{title}");

    let graph = entry_and_prequel();
    let mut row = series(1, "Franchise Name: Entry Words Here", 12, 0);
    row.anilist_id = 11;
    let meta = SeriesMeta::from_series(&row).with_detail(Some(&cached_detail(&graph, 11)));
    let named = |title: &str| meta.named_by_entry_name(&parse_release(&item(title)));
    assert!(named(
        "[Group] Franchise Name - Entry Words Here - 05 [1080p]"
    ));
    assert!(named(
        "[Group] Franchise Name the Movie - Entry Words Here [1080p]"
    ));
    // "Princess Principal - Crown Handler Chapter 2" is not Chapter 1,
    // though its series title drops the 2; "Final" is the last season.
    assert!(!named(
        "[Group] Franchise Name - Entry Words Here Chapter 2 [BDMV]"
    ));
    assert!(!named(
        "[Group] Franchise Name Final - Entry Words Here - 05 [1080p]"
    ));
    assert!(!named(
        "[Group] Franchise Name II - Entry Words Here - 05 [1080p]"
    ));
    assert!(!named(
        "[Group] Franchise Name 2nd - Entry Words Here - 05 [1080p]"
    ));
}
