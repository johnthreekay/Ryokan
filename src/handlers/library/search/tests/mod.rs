use sqlx::SqlitePool;

use super::super::reconcile::maybe_hydrate_cumulative_offset;

use crate::models::{episode_tags, grabbed_torrents, series};
use crate::services::anilist;
use crate::services::auto_expand::{AutoExpandGrabContext, expand_from_files};

fn empty_anime_detail(id: i64, title_english: &str) -> anilist::AnimeDetail {
    anilist::AnimeDetail {
        is_adult: false,
        id,
        id_mal: None,
        title_romaji: title_english.to_string(),
        title_english: title_english.to_string(),
        title_native: String::new(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: "TV".to_string(),
        status: "FINISHED".to_string(),
        status_display: "Finished".to_string(),
        episodes: Some(26),
        duration: Some(24),
        season: String::new(),
        season_year: Some(2012),
        end_year: Some(2013),
        description: String::new(),
        genres: Vec::new(),
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: Vec::new(),
        streaming_episodes: Vec::new(),
        relations: Vec::new(),
    }
}

fn test_grab_ctx() -> AutoExpandGrabContext {
    AutoExpandGrabContext {
        classification: crate::services::source::ClassificationResult::unknown(),
        release_group: String::new(),
        size_bytes: 0,
    }
}

fn related_entry(id: i64, title_english: &str, episodes: Option<i32>) -> anilist::RelatedEntry {
    anilist::RelatedEntry {
        id,
        id_mal: None,
        title_romaji: title_english.to_string(),
        title_english: title_english.to_string(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "TV".to_string(),
        status: "FINISHED".to_string(),
        status_display: "Finished".to_string(),
        episodes,
        relation_type: "SIDE_STORY".to_string(),
        season_year: Some(2014),
        media_type: "ANIME".to_string(),
    }
}

/// End-to-end exercise of the Phase 2 auto-expand route writer.
/// Mirrors the real JoJo S1-S3 megapack case: the parent entry
/// ("JoJo's Bizarre Adventure") owns the Phantom Blood / Battle
/// Tendency files and a sibling relation ("Stardust Crusaders")
/// owns the S3 files. After the pure inner fn runs, we expect two
/// route rows to land in `grabbed_torrent_series` — one per series
/// — with the unclaimed (parent) files routing to the franchise
/// root.
///
/// The fn is split into outer (qBit metadata wait) + inner
/// (`_with_files`) precisely so this test can feed synthetic
/// filenames without spinning up qBittorrent.
#[tokio::test]
async fn auto_expand_routes_sibling_and_parent_files() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    // Seed the parent series row. The real grab path calls
    // series::upsert first, so this matches production flow.
    let (parent_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 801,
            mal_id: None,
            title: "JoJo's Bizarre Adventure",
            title_romaji: "JoJo no Kimyou na Bouken",
            title_english: "JoJo's Bizarre Adventure",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(26),
            season_year: Some(2012),
            end_year: Some(2013),
        },
    )
    .await
    .expect("parent upsert");

    // Record a grab row so there's a grab_id for the routes to
    // attach to. `record_grab` returns Ok(Some(id)) on fresh insert.
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "dummyhash0000000000000000000000000000000",
        "[Group] JoJo Megapack (BD 1080p)",
        parent_id,
        &[],
        true,
    )
    .await
    .expect("record_grab")
    .expect("grab row inserted");

    // Construct a parent AnimeDetail with one sibling relation.
    // The sibling title must carry an extractable trailing
    // subtitle ("Stardust Crusaders") for detect_sibling_entries
    // to find a needle to match.
    let mut parent_detail = empty_anime_detail(801, "JoJo's Bizarre Adventure");
    parent_detail.relations.push(related_entry(
        802,
        "JoJo's Bizarre Adventure: Stardust Crusaders",
        Some(24),
    ));

    // Two sibling files (match the Stardust Crusaders needle) and
    // two parent files (bare franchise title, no sibling subtitle).
    let filenames = vec![
        "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01 [BD 1080p].mkv".to_string(),
        "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 02 [BD 1080p].mkv".to_string(),
        "[Group] JoJo no Kimyou na Bouken - 01 [BD 1080p].mkv".to_string(),
        "[Group] JoJo no Kimyou na Bouken - 02 [BD 1080p].mkv".to_string(),
    ];

    let grab_ctx = test_grab_ctx();
    let added = expand_from_files(
        &db,
        &filenames,
        &parent_detail,
        parent_id,
        &[1, 2],
        grab_id,
        "[Group] JoJo Megapack (BD 1080p)",
        &grab_ctx,
    )
    .await;

    assert_eq!(added, 1, "one new sibling (Stardust Crusaders) expected");

    let routes = grabbed_torrents::get_series_routes(&db, grab_id)
        .await
        .expect("get_series_routes");
    assert_eq!(routes.len(), 2, "sibling route + parent route expected");

    // The sibling route: claims file indices 0 and 1, its series_id
    // differs from the parent, and the matched subtitle is the one
    // trailing_subtitle_of extracted from the relation title.
    let sibling_route = routes
        .iter()
        .find(|r| r.series_id != parent_id)
        .expect("sibling route present");
    assert_eq!(sibling_route.file_indices, vec![0, 1]);
    assert_eq!(sibling_route.matched_subtitle, "Stardust Crusaders");
    // Arc-local numbering (files E01, E02) → min_ep=1 ≤
    // parent_cap=26 → offset=0, and stored episode_numbers
    // equal the raw parsed values.
    assert_eq!(sibling_route.episode_offset, 0);
    assert_eq!(sibling_route.episode_numbers, vec![1, 2]);

    // The parent route: claims the unclaimed media files (2 and 3)
    // and reuses the caller-supplied episode numbers verbatim.
    let parent_route = routes
        .iter()
        .find(|r| r.series_id == parent_id)
        .expect("parent route present");
    assert_eq!(parent_route.file_indices, vec![2, 3]);
    assert_eq!(parent_route.episode_numbers, vec![1, 2]);
    // Parent routes always carry offset 0.
    assert_eq!(parent_route.episode_offset, 0);
}

/// Smol Monogatari-style batch: absolute episode numbering runs
/// across parent + sibling (E13 = last parent ep, E14 = first
/// sibling ep). The fallback path detects Owarimonogatari Second
/// Season via title-prefix matching AND the per-sibling offset
/// pass sets offset=13 so the route row's episode_numbers store
/// the effective (arc-local) 1..=7 instead of the raw 14..=20.
#[tokio::test]
async fn auto_expand_persists_episode_offset_for_absolute_numbered_batch() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (parent_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 21320,
            mal_id: None,
            title: "Owarimonogatari",
            title_romaji: "Owarimonogatari",
            title_english: "Owarimonogatari",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(13),
            season_year: Some(2015),
            end_year: Some(2015),
        },
    )
    .await
    .expect("parent upsert");

    let grab_id = grabbed_torrents::record_grab(
        &db,
        "owarismolhash00000000000000000000000000",
        "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
        parent_id,
        &[],
        true,
    )
    .await
    .expect("record_grab")
    .expect("grab inserted");

    // Parent AnimeDetail with a continuation relation. Use the
    // real title "Owarimonogatari Second Season" — no delimiter,
    // no 2-token trailing subtitle — so the subtitle path cannot
    // match and the fallback path's title-prefix rule must fire.
    let mut parent_detail = empty_anime_detail(21320, "Owarimonogatari");
    parent_detail.episodes = Some(13);
    parent_detail.relations.push(related_entry(
        21860,
        "Owarimonogatari Second Season",
        Some(7),
    ));

    // 13 parent files (S07E01..E13) + 7 sibling files (S07E14..E20).
    let mut filenames: Vec<String> = Vec::new();
    for n in 1..=13 {
        filenames.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p).mkv",
            n
        ));
    }
    for n in 14..=20 {
        filenames.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p).mkv",
            n
        ));
    }
    let parent_episode_numbers: Vec<i32> = (1..=13).collect();

    let grab_ctx = test_grab_ctx();
    let added = expand_from_files(
        &db,
        &filenames,
        &parent_detail,
        parent_id,
        &parent_episode_numbers,
        grab_id,
        "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
        &grab_ctx,
    )
    .await;

    assert_eq!(added, 1, "one new sibling (Owari S2) expected");

    let routes = grabbed_torrents::get_series_routes(&db, grab_id)
        .await
        .expect("get_series_routes");
    assert_eq!(routes.len(), 2, "sibling route + parent route expected");

    let sibling_route = routes
        .iter()
        .find(|r| r.series_id != parent_id)
        .expect("sibling route present");
    // Files 13..=19 (0-based indices) correspond to S07E14..E20.
    assert_eq!(sibling_route.file_indices, vec![13, 14, 15, 16, 17, 18, 19]);
    // The matched subtitle records the detection method for
    // operator inspection.
    assert!(
        sibling_route
            .matched_subtitle
            .starts_with("episode-range fallback")
    );
    // Absolute numbering → offset = parent_cap = 13.
    assert_eq!(sibling_route.episode_offset, 13);
    // Stored episode_numbers are effective (post-offset) values,
    // so a later `find_imported_for_episode(sibling, 1)` upgrade
    // query hits this route row correctly.
    assert_eq!(sibling_route.episode_numbers, vec![1, 2, 3, 4, 5, 6, 7]);

    let parent_route = routes
        .iter()
        .find(|r| r.series_id == parent_id)
        .expect("parent route present");
    assert_eq!(
        parent_route.file_indices,
        vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
    );
    assert_eq!(parent_route.episode_offset, 0);

    // Regression guard: auto-expand must also write per-episode
    // `episode_quality_tags` + `episode_grab_history` rows for the
    // newly-upserted sibling. Without these the sibling's series
    // page renders UNKNOWN with no progress bar until post-
    // processing backfills them (which, if the user has PP
    // disabled, never happens). Uses the effective (post-offset)
    // local episode numbers the route already stores.
    let sibling_id = sibling_route.series_id;
    let sibling_tags = episode_tags::get_for_series(&db, sibling_id)
        .await
        .expect("sibling quality tags");
    assert_eq!(
        sibling_tags.len(),
        7,
        "sibling should have 7 quality-tag rows (one per local ep 1..=7)"
    );
    for local_ep in 1..=7 {
        let tag = sibling_tags
            .get(&local_ep)
            .unwrap_or_else(|| panic!("sibling tag for local ep {} missing", local_ep));
        assert_eq!(tag.state, "grabbed");
        let history = episode_tags::get_grab_history(&db, sibling_id, local_ep)
            .await
            .expect("sibling grab history");
        assert_eq!(
            history.len(),
            1,
            "sibling local ep {} should have 1 grab-history row",
            local_ep
        );
    }
}

/// When the file list has no sibling matches, the inner fn is a
/// no-op: no sibling series get upserted and no route rows get
/// written. This exercises the early-return after
/// `detect_sibling_entries_in_pack` returns an empty vec — the
/// production path relies on that branch to avoid polluting the
/// library with ghost rows for regular single-series grabs.
#[tokio::test]
async fn auto_expand_noop_when_no_siblings_detected() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (parent_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 901,
            mal_id: None,
            title: "Sono Bisque Doll wa Koi wo Suru",
            title_romaji: "Sono Bisque Doll wa Koi wo Suru",
            title_english: "My Dress-Up Darling",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2022),
            end_year: Some(2022),
        },
    )
    .await
    .expect("parent upsert");

    let grab_id = grabbed_torrents::record_grab(
        &db,
        "dummyhash1111111111111111111111111111111",
        "[Group] My Dress-Up Darling S01 (BD 1080p)",
        parent_id,
        &[],
        true,
    )
    .await
    .expect("record_grab")
    .expect("grab row inserted");

    // No relations on the parent detail → no sibling candidates
    // even though the file list is full of media files.
    let parent_detail = empty_anime_detail(901, "My Dress-Up Darling");
    let filenames = vec![
        "[Group] My Dress-Up Darling - 01 [BD 1080p].mkv".to_string(),
        "[Group] My Dress-Up Darling - 02 [BD 1080p].mkv".to_string(),
    ];

    let grab_ctx = test_grab_ctx();
    let added = expand_from_files(
        &db,
        &filenames,
        &parent_detail,
        parent_id,
        &[1, 2],
        grab_id,
        "[Group] My Dress-Up Darling S01 (BD 1080p)",
        &grab_ctx,
    )
    .await;

    assert_eq!(added, 0);
    let routes = grabbed_torrents::get_series_routes(&db, grab_id)
        .await
        .expect("get_series_routes");
    assert!(
        routes.is_empty(),
        "no sibling → no routes, post-processing falls back to grab.series_id"
    );
}

/// #26 — Grab-time hydration gate must NOT fire when a series'
/// only PREQUEL is a movie (format = "MOVIE"). JJK S1's only
/// AL prequel is the JJK 0 movie; since absolute-numbered TV
/// releases don't count movies, the existing cumulative = 0 is
/// correct and we must not trigger an AL refresh on every
/// auto-search.
///
/// Verifies the gate returns the series unchanged (cumulative
/// still 0) WITHOUT attempting network I/O — if the gate were
/// wrong, refresh_series_metadata would be called and the test
/// would hit AL (and fail with a flaky network error).
#[tokio::test]
async fn cumulative_hydration_skips_movie_only_prequel() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 113415,
            mal_id: None,
            title: "Jujutsu Kaisen",
            title_romaji: "Jujutsu Kaisen",
            title_english: "Jujutsu Kaisen",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(24),
            season_year: Some(2020),
            end_year: Some(2021),
        },
    )
    .await
    .expect("upsert");

    let tracked = series::get_by_id(&db, series_id)
        .await
        .expect("get_by_id")
        .expect("series exists");
    assert_eq!(tracked.cumulative_prior_episodes, 0);

    let mut detail = empty_anime_detail(113415, "Jujutsu Kaisen");
    let mut jjk0 = related_entry(145064, "Jujutsu Kaisen 0", None);
    jjk0.relation_type = "PREQUEL".to_string();
    jjk0.format = "MOVIE".to_string();
    detail.relations.push(jjk0);

    let result = maybe_hydrate_cumulative_offset(&db, Some(tracked), &detail).await;
    let after = result.expect("series still returned");
    assert_eq!(
        after.cumulative_prior_episodes, 0,
        "movie-only prequel must not trigger hydration"
    );
}

/// #26 — Gate must short-circuit when cumulative is already
/// non-zero: a series that's been hydrated before (e.g. by the
/// periodic metadata_refresh sweep) should not re-hydrate on
/// every auto-search.
#[tokio::test]
async fn cumulative_hydration_skips_already_populated_series() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 145064,
            mal_id: None,
            title: "Jujutsu Kaisen S2",
            title_romaji: "Jujutsu Kaisen S2",
            title_english: "Jujutsu Kaisen S2",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(23),
            season_year: Some(2023),
            end_year: Some(2023),
        },
    )
    .await
    .expect("upsert");
    series::update_cumulative_prior_episodes(&db, series_id, 24)
        .await
        .expect("set cumulative");

    let tracked = series::get_by_id(&db, series_id)
        .await
        .expect("get_by_id")
        .expect("series exists");
    assert_eq!(tracked.cumulative_prior_episodes, 24);

    let mut detail = empty_anime_detail(145064, "Jujutsu Kaisen S2");
    let mut prev = related_entry(113415, "Jujutsu Kaisen", Some(24));
    prev.relation_type = "PREQUEL".to_string();
    prev.format = "TV".to_string();
    detail.relations.push(prev);

    let result = maybe_hydrate_cumulative_offset(&db, Some(tracked), &detail).await;
    let after = result.expect("series still returned");
    assert_eq!(
        after.cumulative_prior_episodes, 24,
        "populated cumulative short-circuits the gate"
    );
}

/// #206 — A curated anime-relations rule sets the offset without any
/// AniList walk, and it fires even when the detail lists no TV prequel
/// (Kai 2014's only prequel is a SPECIAL; that gap is what the rules
/// are for). The rule's source entry is already cached here, so the
/// hydration touches no network.
#[tokio::test]
async fn cumulative_hydration_takes_the_anime_relations_rule_first() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    // Attack on Titan Season 2: `16498:26-37 -> 20958:1-12`.
    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 20958,
            mal_id: Some(25777),
            title: "Attack on Titan Season 2",
            title_romaji: "Shingeki no Kyojin Season 2",
            title_english: "Attack on Titan Season 2",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2017),
            end_year: Some(2017),
        },
    )
    .await
    .expect("upsert");
    crate::models::metadata_cache::upsert_provider(
        &db,
        16498,
        Some(16498),
        &empty_anime_detail(16498, "Attack on Titan"),
    )
    .await
    .expect("cache the rule source");

    let tracked = series::get_by_id(&db, series_id)
        .await
        .expect("get_by_id")
        .expect("series exists");
    assert_eq!(tracked.cumulative_prior_episodes, 0);

    // No relations at all: the TV-prequel gate alone would skip this.
    let detail = empty_anime_detail(20958, "Attack on Titan Season 2");
    let result = maybe_hydrate_cumulative_offset(&db, Some(tracked), &detail).await;
    let after = result.expect("series still returned");
    assert_eq!(
        after.cumulative_prior_episodes, 25,
        "rule offset persisted without a TV prequel"
    );
}

/// Issue #45: full-scale JoJo Part 3 case. 48-episode BD megapack
/// with absolute continuous numbering (no per-cour arc markers in
/// the filenames) and Egypt-hen as a sibling of Stardust Crusaders
/// on AniList. Egypt-hen's trailing "subtitle" is a single token
/// ("Egypt-hen") so the subtitle path can't match — the
/// episode-range fallback picks it up via title-prefix matching.
///
/// Verifies that:
///   1. detection fires once (Egypt-hen sibling).
///   2. the sibling route carries files 24..=47 (0-based) = E25..E48.
///   3. episode_offset = 24 (parent_cap) so those map to local 1..=24.
///   4. the sibling gets 24 quality-tag + grab-history rows (not 0).
///   5. the parent route carries files 0..=23 = E01..=E24, offset 0.
#[tokio::test]
async fn auto_expand_jojo_part3_48ep_pack_maps_all_episodes() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    // Parent: JoJo Stardust Crusaders (24 eps).
    let (parent_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 20899,
            mal_id: None,
            title: "JoJo's Bizarre Adventure: Stardust Crusaders",
            title_romaji: "JoJo no Kimyou na Bouken: Stardust Crusaders",
            title_english: "JoJo's Bizarre Adventure: Stardust Crusaders",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(24),
            season_year: Some(2014),
            end_year: Some(2014),
        },
    )
    .await
    .expect("parent upsert");

    let grab_id = grabbed_torrents::record_grab(
        &db,
        "jojop3hash00000000000000000000000000000000",
        "[Group] JoJo's Bizarre Adventure Part 3 - Stardust Crusaders (BD 1080p 48 ep)",
        parent_id,
        &[],
        true,
    )
    .await
    .expect("record_grab")
    .expect("grab inserted");

    // Parent AnimeDetail with Egypt-hen as a sibling relation. Use
    // the real AL title form "... Stardust Crusaders - Egypt-hen";
    // the trailing single-token subtitle can't be extracted, so
    // detection falls through to the episode-range + title-prefix
    // path.
    let mut parent_detail =
        empty_anime_detail(20899, "JoJo's Bizarre Adventure: Stardust Crusaders");
    parent_detail.episodes = Some(24);
    parent_detail.relations.push(related_entry(
        22663,
        "JoJo's Bizarre Adventure: Stardust Crusaders - Egypt-hen",
        Some(24),
    ));

    // 48 absolute-numbered files (E01..E48).
    let filenames: Vec<String> = (1..=48)
        .map(|n| {
            format!(
                "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - {:02} [BD 1080p].mkv",
                n
            )
        })
        .collect();

    let grab_ctx = test_grab_ctx();
    let added = expand_from_files(
        &db,
        &filenames,
        &parent_detail,
        parent_id,
        &(1..=48).collect::<Vec<_>>(),
        grab_id,
        "[Group] JoJo P3 BD 48ep",
        &grab_ctx,
    )
    .await;

    assert_eq!(added, 1, "one new sibling (Egypt-hen) expected");

    let routes = grabbed_torrents::get_series_routes(&db, grab_id)
        .await
        .expect("get_series_routes");
    assert_eq!(routes.len(), 2, "sibling + parent route expected");

    let sibling_route = routes
        .iter()
        .find(|r| r.series_id != parent_id)
        .expect("sibling route present");
    let expected_sibling_files: Vec<usize> = (24..=47).collect();
    assert_eq!(
        sibling_route.file_indices, expected_sibling_files,
        "sibling owns files 24..=47 (E25..E48)"
    );
    assert_eq!(
        sibling_route.episode_offset, 24,
        "absolute numbering → offset = parent_cap = 24"
    );
    assert_eq!(
        sibling_route.episode_numbers,
        (1..=24).collect::<Vec<_>>(),
        "sibling's stored ep_nums are effective (post-offset) 1..=24"
    );

    let parent_route = routes
        .iter()
        .find(|r| r.series_id == parent_id)
        .expect("parent route present");
    let expected_parent_files: Vec<usize> = (0..=23).collect();
    assert_eq!(parent_route.file_indices, expected_parent_files);
    assert_eq!(parent_route.episode_offset, 0);

    // Sibling quality-tag + history rows exist for local 1..=24.
    let sibling_id = sibling_route.series_id;
    let sibling_tags = episode_tags::get_for_series(&db, sibling_id)
        .await
        .expect("sibling quality tags");
    assert_eq!(
        sibling_tags.len(),
        24,
        "sibling should have 24 quality-tag rows"
    );
    for local_ep in 1..=24 {
        let tag = sibling_tags
            .get(&local_ep)
            .unwrap_or_else(|| panic!("sibling tag for local ep {} missing", local_ep));
        assert_eq!(tag.state, "grabbed");
    }
}

/// Issue #45: Owarimonogatari BD with an AL/BD episode-count
/// disagreement. AL reports S1 = 12 eps (the aired ep 1 was a
/// 48-min merged episode) but the [smol] BD splits that back into
/// two ~24-min files, so the pack has 13 Owari S1 files + 7 Owari
/// S2 files.
///
/// This is the case the user flagged as "frustrating" in issue #45.
/// Verifies that:
///   1. the sibling side is unaffected by the mismatch — Owari S2
///      gets 7 files mapped to local 1..=7 via offset 13.
///   2. the parent side gets all 13 files (including the "extra"
///      ep 13 that AL doesn't know about), routed with offset 0.
///
/// The complementary UI fix lives in `pages::build_episodes` — see
/// `build_episodes_surfaces_on_disk_files_beyond_anilist_episode_count`.
#[tokio::test]
async fn auto_expand_owari_bd_split_with_anilist_count_mismatch() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    // Parent: Owarimonogatari. Use AL's reported count (12), NOT
    // the BD's file count (13). This is the whole point of the test.
    let (parent_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 21860,
            mal_id: None,
            title: "Owarimonogatari",
            title_romaji: "Owarimonogatari",
            title_english: "Owarimonogatari",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2015),
            end_year: Some(2015),
        },
    )
    .await
    .expect("parent upsert");

    let grab_id = grabbed_torrents::record_grab(
        &db,
        "owarialmismatch000000000000000000000000000",
        "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
        parent_id,
        &[],
        true,
    )
    .await
    .expect("record_grab")
    .expect("grab inserted");

    let mut parent_detail = empty_anime_detail(21860, "Owarimonogatari");
    parent_detail.episodes = Some(12); // AL's count, NOT the BD's.
    parent_detail.relations.push(related_entry(
        99423,
        "Owarimonogatari Second Season",
        Some(7),
    ));

    // 13 parent files (S07E01..E13) + 7 sibling files (S07E14..E20).
    // The parent's file 12 (E13) exists despite AL saying S1 has
    // only 12 episodes — this is the mismatch we're testing.
    let mut filenames: Vec<String> = Vec::new();
    for n in 1..=13 {
        filenames.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p).mkv",
            n
        ));
    }
    for n in 14..=20 {
        filenames.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p).mkv",
            n
        ));
    }

    let grab_ctx = test_grab_ctx();
    let added = expand_from_files(
        &db,
        &filenames,
        &parent_detail,
        parent_id,
        &(1..=12).collect::<Vec<_>>(),
        grab_id,
        "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
        &grab_ctx,
    )
    .await;

    assert_eq!(added, 1, "one new sibling (Owari S2) expected");

    let routes = grabbed_torrents::get_series_routes(&db, grab_id)
        .await
        .expect("get_series_routes");
    assert_eq!(routes.len(), 2, "sibling + parent route expected");

    // Sibling route: 7 files (S07E14..E20) mapped to local 1..=7.
    // `min_ep = 14`, parent_cap = 12 → offset = min_ep - 1 = 13.
    // Local = raw - offset: 14→1, 15→2, ..., 20→7.
    let sibling_route = routes
        .iter()
        .find(|r| r.series_id != parent_id)
        .expect("sibling route present");
    assert_eq!(
        sibling_route.file_indices,
        vec![13, 14, 15, 16, 17, 18, 19],
        "sibling owns E14..=E20 (0-based 13..=19)"
    );
    assert_eq!(
        sibling_route.episode_offset, 13,
        "min_ep(14) - 1 = 13, correctly larger than parent_cap(12)"
    );
    assert_eq!(sibling_route.episode_numbers, vec![1, 2, 3, 4, 5, 6, 7]);

    // Parent route: all 13 files including the "extra" E13 that
    // AL doesn't know about. Offset stays 0 — parent files use
    // their own local numbering.
    let parent_route = routes
        .iter()
        .find(|r| r.series_id == parent_id)
        .expect("parent route present");
    assert_eq!(
        parent_route.file_indices,
        (0..=12).collect::<Vec<_>>(),
        "parent owns all 13 files (E01..=E13), including the AL-overflow E13"
    );
    assert_eq!(parent_route.episode_offset, 0);
}

// ── Pure-helper coverage ─────────────────────────────────────────────
//
// Tests below cover the small pure helpers in `auto_search.rs` that
// don't need a DB or a download client. The async/DB-backed
// auto-expand tests above remain the load-bearing checks; these pin
// the small functions that feed them.

mod pure_helpers {
    use super::super::auto_search::{batch_episode_numbers, display_title_for_progress};
    use super::empty_anime_detail;

    // ── batch_episode_numbers ────────────────────────────────────────

    #[test]
    fn batch_episode_numbers_parses_ranges_from_release_titles() {
        // Real Nyaa batch shape — episode range gets parsed into
        // every number it covers.
        let detail = empty_anime_detail(1, "Show");
        let nums = batch_episode_numbers("[Group] Show 01-12 (BD 1080p)", &detail);
        assert_eq!(nums, (1..=12).collect::<Vec<_>>());
    }

    #[test]
    fn batch_episode_numbers_falls_back_to_anilist_count() {
        // When the title carries no parseable range, we fall back to
        // AL's reported episode count. The series has 26 episodes so
        // the fallback list is 1..=26.
        let detail = empty_anime_detail(1, "Show");
        // empty_anime_detail seeds episodes: Some(26).
        let nums = batch_episode_numbers("[Group] Show Complete BD", &detail);
        assert_eq!(nums, (1..=26).collect::<Vec<_>>());
    }

    #[test]
    fn batch_episode_numbers_no_fallback_when_episode_count_zero_or_missing() {
        // detail.episodes == None → the fallback arm shouldn't fire
        // (we'd produce an empty vec rather than risking a guess).
        let mut detail = empty_anime_detail(1, "Show");
        detail.episodes = None;
        assert!(batch_episode_numbers("[Group] Show Complete BD", &detail).is_empty());

        // detail.episodes == Some(0) is similarly defensive.
        detail.episodes = Some(0);
        assert!(batch_episode_numbers("[Group] Show Complete BD", &detail).is_empty());
    }

    #[test]
    fn batch_episode_numbers_no_fallback_when_episode_count_unreasonable() {
        // The 1000-episode cap exists so an AL parse glitch
        // (e.g. erroneous reported episodes count for One Piece)
        // can't blow up into a million-element vec. Pin the gate.
        let mut detail = empty_anime_detail(1, "Show");
        detail.episodes = Some(1001);
        assert!(batch_episode_numbers("[Group] Show Complete BD", &detail).is_empty());
    }

    #[test]
    fn batch_episode_numbers_returns_sorted_unique_values() {
        // The sort guarantee matters — downstream upgrade scans
        // bisect this list. An out-of-order vec would silently miss
        // upgrades. The parser handles dedup itself, but pin that
        // contract by feeding a title with overlapping ranges.
        let detail = empty_anime_detail(1, "Show");
        let nums = batch_episode_numbers("[Group] Show 03 + 01-05 BD", &detail);
        // Sorted ascending; dedup handled upstream.
        let mut sorted = nums.clone();
        sorted.sort_unstable();
        assert_eq!(nums, sorted, "result must come back sorted");
    }

    // ── display_title_for_progress ───────────────────────────────────

    #[test]
    fn display_title_for_progress_prefers_english() {
        let mut detail = empty_anime_detail(1, "Show");
        detail.title_english = "English Title".to_string();
        detail.title_romaji = "Romaji Title".to_string();
        assert_eq!(display_title_for_progress(&detail), "English Title");
    }

    #[test]
    fn display_title_for_progress_falls_back_to_romaji_when_english_empty() {
        let mut detail = empty_anime_detail(1, "Show");
        detail.title_english = String::new();
        detail.title_romaji = "Romaji Only".to_string();
        assert_eq!(display_title_for_progress(&detail), "Romaji Only");
    }

    #[test]
    fn display_title_for_progress_returns_empty_when_both_empty() {
        // Defensive — neither title is populated. The progress toast
        // gets an empty body but the call doesn't panic.
        let mut detail = empty_anime_detail(1, "Show");
        detail.title_english = String::new();
        detail.title_romaji = String::new();
        assert_eq!(display_title_for_progress(&detail), "");
    }
}

// ── series_still_in_library (issue #102) ──────────────────────────────
//
// The cascade-stop guard at the top of run_auto_search_targets_with_
// upgrades' per-target loop. Tests pin the three states the guard
// distinguishes: no-series-bound, present, and removed. Without the
// guard, removing a series mid-loop kept queueing per-episode grabs.

#[cfg(test)]
mod cascade_stop_tests {
    use sqlx::SqlitePool;

    use super::super::auto_search::series_still_in_library;
    use crate::models::series;

    async fn seed_series(db: &SqlitePool, anilist_id: i64, title: &str) -> i64 {
        let (id, _) = series::upsert(
            db,
            series::SeriesCore {
                anilist_id,
                mal_id: None,
                title,
                title_romaji: title,
                title_english: title,
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2020),
                end_year: Some(2020),
            },
        )
        .await
        .expect("upsert");
        id
    }

    #[tokio::test]
    async fn none_series_id_short_circuits_to_true() {
        // The search-before-add flow doesn't bind a series_id; the
        // guard must let it through unchanged. Otherwise every
        // anonymous "preview search" call would terminate after the
        // first target.
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        assert!(series_still_in_library(&db, None).await);
    }

    #[tokio::test]
    async fn present_series_returns_true() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let id = seed_series(&db, 12345, "Test Show").await;
        assert!(series_still_in_library(&db, Some(id)).await);
    }

    #[tokio::test]
    async fn removed_series_returns_false_so_loop_breaks() {
        // The load-bearing case: user removes the series mid-loop.
        // The next iteration's guard call must return false so the
        // outer loop stops queueing per-episode grabs.
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let id = seed_series(&db, 12345, "Removed Show").await;
        series::remove(&db, id).await.expect("remove");
        assert!(!series_still_in_library(&db, Some(id)).await);
    }

    #[tokio::test]
    async fn nonexistent_series_id_returns_false() {
        // Defensive: if the caller passes a bogus id (race between
        // `series_id_for_grab` resolution and the loop entry), the
        // guard should treat it as "not in library" rather than
        // erroring out.
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        assert!(!series_still_in_library(&db, Some(999_999)).await);
    }
}

// ─── Handler-level coverage for the four endpoints in mod.rs +
//     interactive.rs that don't need a full Nyaa wiremock harness:
//     the `api_series_detail` cache hit, the `anilist_search`
//     400-on-invalid-source branch, and the cache-hit short circuit
//     in both `interactive_search_*` endpoints. The Nyaa-backed
//     non-cached paths in interactive_* are covered indirectly by
//     `tests/auto_search_e2e.rs` (which tests `find_all_for_target`
//     directly with the same RYOKAN_NYAA_API_BASE seam); the handler
//     surface above that is just resolver + cache wiring.

#[cfg(test)]
mod handler_endpoints {
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::response::Json as AxumJson;

    use super::super::interactive::{interactive_search_batches, interactive_search_episode};
    use super::super::{anilist_search, api_series_detail};
    use super::empty_anime_detail;
    use crate::handlers::library::AnilistSearchQuery;
    use crate::services::interactive_search_cache;
    use crate::services::nyaa::SearchResult;
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

    fn empty_search_result(title: &str, info_hash: &str) -> SearchResult {
        SearchResult {
            match_provenance: None,
            title: title.into(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes: 0,
            seeders: 0,
            leechers: 0,
            downloads: 0,
            group: String::new(),
            resolution: String::new(),
            quality_label: String::new(),
            source: String::new(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: false,
            is_trusted: false,
            score: 0,
            info_hash: info_hash.into(),
            score_breakdown: Vec::new(),
            upload_date: String::new(),
            indexer_id: None,
            indexer_name: String::new(),
        }
    }

    // ─── api_series_detail ───────────────────────────────────────

    #[tokio::test]
    async fn api_series_detail_returns_cached_metadata_without_network() {
        // Cache hit short-circuits resolve_series_context inline at
        // line 263 of reconcile.rs without hitting AniList. Pin the
        // happy path: `(_, _, detail)` unpacks to the cached
        // AnimeDetail and the JSON body round-trips.
        let db = in_memory_pool().await;
        let anilist_id: i64 = 600;
        let series_id = seed_series(&db, anilist_id, "Cached Detail Show").await;
        let detail = empty_anime_detail(anilist_id, "Cached Detail Show");
        crate::models::metadata_cache::upsert(&db, series_id, anilist_id, None, &detail)
            .await
            .unwrap();

        let state = build_test_app_state(db, None);
        let AxumJson(returned) = api_series_detail(State(state), Path(anilist_id))
            .await
            .expect("cache-hit path must succeed without network");
        assert_eq!(returned.id, anilist_id);
        assert_eq!(returned.title_english, "Cached Detail Show");
        assert_eq!(returned.episodes, Some(26));
    }

    // ─── anilist_search invalid-source guard ────────────────────

    #[tokio::test]
    async fn anilist_search_returns_400_for_unrecognized_source_override() {
        // `?source=` accepts only "al", "mal", or omitted. A typo
        // (`?source=anilist`) should be a hard 400 — silently
        // falling through to the config default would mask the bug
        // and the toggle in the Add Series modal would look broken.
        // The 400 fires BEFORE any AniList HTTP call so this test
        // doesn't need network or wiremock.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let result = anilist_search(
            State(state),
            axum_htmx::HxRequest(false),
            Query(AnilistSearchQuery {
                q: "anything".into(),
                source: Some("anilist".into()),
                lang: None,
            }),
        )
        .await;
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(
                    body.contains("invalid source override"),
                    "the 400 must explain the bad value; got {body}"
                );
            }
            Ok(_) => panic!("invalid source must surface as 400, not Ok"),
        }
    }

    // ─── Interactive-search partial render (issue #166) ─────────

    /// HTMX path on `interactive_search_episode` returns a rendered
    /// partial; this test exercises the build+render directly so it
    /// doesn't need Nyaa or AL upstreams. Mirrors the AL-search test
    /// shape: assert each load-bearing field reaches the rendered
    /// HTML, then round-trip the `data-result` JSON to confirm the
    /// per-row Grab button can read its full SearchResult payload.
    #[test]
    fn interactive_search_partial_renders_table_and_data_result() {
        use crate::services::nyaa::SearchResult;
        use crate::services::scoring::ScoreComponent;
        use askama::Template;

        let mut hit = SearchResult {
            match_provenance: None,
            title: "[Group] Show - 03 [BD 1080p].mkv".into(),
            link: "https://nyaa.example/view/42".into(),
            magnet: "magnet:?xt=urn:btih:abc123".into(),
            torrent: "https://nyaa.example/download/42.torrent".into(),
            size: "1.4 GiB".into(),
            size_bytes: 1_500_000_000,
            seeders: 42,
            leechers: 3,
            downloads: 200,
            group: "Group".into(),
            resolution: "1080".into(),
            quality_label: "BD-1080p".into(),
            source: "BluRay".into(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: false,
            is_trusted: true,
            score: 92,
            info_hash: "abc123abc123abc123abc123abc123abc123abc1".into(),
            score_breakdown: vec![ScoreComponent {
                label: "Seeders".into(),
                delta: 10,
                detail: Some("42 seeders".into()),
            }],
            upload_date: String::new(),
            indexer_id: Some(7),
            indexer_name: "Example-Indexer".into(),
        };
        // Force a low-score row to confirm the score-class threshold.
        let mut low = hit.clone();
        low.score = 12;
        low.title = "[NoGroup] Show - 03.mkv".into();
        low.indexer_name = String::new(); // -> renders "Nyaa"
        low.is_trusted = false;
        low.indexer_id = None;
        hit.score = 92;

        let partial = super::super::interactive::test_helpers::build_partial_for_test(
            vec![hit.clone(), low.clone()],
            Some(3),
        );
        let html = partial.render().expect("partial renders");

        // High-score row class + the score badge value visible.
        assert!(
            html.contains("score-high"),
            "score>=80 must render the high band\n{html}"
        );
        // Low-score row class.
        assert!(
            html.contains("score-low"),
            "score<40 must render the low band\n{html}"
        );
        // Per-row Grab button uses the per-episode handler with the
        // episode number from grab_episode_number.
        assert!(
            html.contains("grabInteractiveResult(3, this)"),
            "per-episode flow must wire the Grab button to the episode handler\n{html}"
        );
        // Trusted tag rendered for the trusted hit.
        assert!(
            html.contains("trusted"),
            "is_trusted=true must surface the trusted tag\n{html}"
        );
        // Empty `indexer_name` falls back to "Nyaa" so the column never
        // renders blank.
        assert!(
            html.contains(">Nyaa<"),
            "empty indexer_name must fall back to Nyaa\n{html}"
        );

        // The Grab button's `data-result` carries the full SearchResult
        // JSON. Round-trip parse it to confirm `grabInteractiveResult`
        // can reconstruct the metadata it needs (url, title, group,
        // info_hash, indexer_id) without the prior `_isearchResults`
        // module-scope array.
        let needle = r#"data-result=""#;
        let start = html.find(needle).expect("data-result attr present") + needle.len();
        let end = start
            + html[start..]
                .find('"')
                .expect("data-result attr is double-quoted");
        let escaped = &html[start..end];
        let unescaped = escaped.replace("&#34;", "\"").replace("&amp;", "&");
        let parsed: serde_json::Value =
            serde_json::from_str(&unescaped).expect("data-result JSON parses");
        assert_eq!(
            parsed.get("info_hash").and_then(|v| v.as_str()),
            Some("abc123abc123abc123abc123abc123abc123abc1"),
            "data-result must round-trip info_hash; got {parsed}"
        );
        assert_eq!(
            parsed.get("indexer_id").and_then(|v| v.as_i64()),
            Some(7),
            "data-result must round-trip indexer_id; got {parsed}"
        );
    }

    /// Batch flow: same partial, `grab_episode_number = None` →
    /// Grab button calls the batch handler instead. Empty result
    /// list renders the batch-specific copy.
    #[test]
    fn interactive_search_partial_batch_flow_uses_batch_handler() {
        use askama::Template;

        let empty = super::super::interactive::test_helpers::build_partial_for_test(vec![], None);
        let html = empty.render().expect("renders empty");
        assert!(
            html.contains("No batch releases found."),
            "batch flow must use batch-specific empty copy\n{html}"
        );
    }

    // ─── HTMX partial render (issue #166) ────────────────────────

    /// The HxRequest branch in `anilist_search` calls
    /// `build_search_results_partial` with the AL hits and renders the
    /// `partials/library/anilist_search_results.html` template. This
    /// test exercises the build+render directly so it doesn't need an
    /// AL upstream — the upstream call is what `anilist_search` does
    /// before this code path runs, and its behavior is independent.
    ///
    /// Asserts every load-bearing field reaches the rendered HTML:
    ///   - title chosen by language (english here),
    ///   - external href shape (AL vs MAL → different host + id),
    ///   - format / episode / status badges,
    ///   - the `data-entry` JSON parses back to the same `id`, which
    ///     is what `addSeries(id, this)` in `static/js/index.js` reads.
    #[test]
    fn htmx_partial_renders_expected_markers_and_data_entry() {
        use crate::services::anilist::AnimeEntry;
        use askama::Template;

        let entries = vec![
            AnimeEntry {
                id: 21,
                id_mal: None,
                title_romaji: "Naruto".into(),
                title_english: "Naruto".into(),
                title_native: "ナルト".into(),
                cover_url: "https://cdn.example/cover-21.jpg".into(),
                format: "TV_SHORT".into(),
                status: "FINISHED".into(),
                status_display: "Finished".into(),
                episodes: Some(220),
                season_year: Some(2002),
                source: "al".into(),
                average_score: Some(78),
            },
            AnimeEntry {
                id: 100,
                id_mal: Some(9876),
                title_romaji: "Some MAL Show".into(),
                title_english: String::new(),
                title_native: String::new(),
                cover_url: String::new(),
                format: String::new(),
                status: "RELEASING".into(),
                status_display: String::new(),
                episodes: None,
                season_year: None,
                source: "mal".into(),
                average_score: None,
            },
        ];

        let partial = super::super::build_search_results_partial(entries, "english");
        let html = partial.render().expect("partial renders");

        // AL row: external link to anilist.co with the AL id, format
        // underscore replaced, episodes count rendered, status class
        // lowercased.
        assert!(
            html.contains(r#"href="https://anilist.co/anime/21""#),
            "AL row must link to anilist.co with the entry id\n{html}"
        );
        assert!(
            html.contains("TV SHORT"),
            "format underscore must render as space\n{html}"
        );
        assert!(html.contains("220 eps"), "episodes badge missing\n{html}");
        assert!(
            html.contains("tag-status-finished"),
            "status class must lowercase the AL enum\n{html}"
        );

        // MAL row: external link uses myanimelist.net + id_mal, episode
        // fallback "?", source label is "MAL". Empty `format` falls back
        // to "TBA" per the JS contract this replaced.
        assert!(
            html.contains(r#"href="https://myanimelist.net/anime/9876""#),
            "MAL row must link to MAL with id_mal\n{html}"
        );
        assert!(
            html.contains("TBA"),
            "empty format must fall back to TBA\n{html}"
        );

        // The AL row's `data-entry` attribute must round-trip the JSON
        // `addSeries` reads. Askama auto-escapes `"` to `&quot;`, so
        // strip-then-parse to confirm the embedded payload is intact.
        let needle = r#"data-entry=""#;
        let start = html.find(needle).expect("data-entry attr present") + needle.len();
        let end = start
            + html[start..]
                .find('"')
                .expect("data-entry attr is double-quoted");
        let escaped = &html[start..end];
        // Askama 0.16 escapes `"` as `&#34;` (numeric character
        // reference, not the named `&quot;` entity). Both forms are
        // valid HTML; pinning to `&#34;` here keeps the test honest if
        // a future Askama major flips back.
        let unescaped = escaped.replace("&#34;", "\"").replace("&amp;", "&");
        let parsed: serde_json::Value =
            serde_json::from_str(&unescaped).expect("data-entry JSON parses");
        assert_eq!(
            parsed.get("id").and_then(|v| v.as_i64()),
            Some(21),
            "data-entry must round-trip the entry id; got {parsed}"
        );
    }

    // ─── interactive_search_* cache short-circuits ──────────────

    #[tokio::test]
    async fn interactive_search_episode_returns_cached_results_when_present() {
        // Pre-populate the cache; the handler must short-circuit at
        // line 339 of interactive.rs and return without touching
        // the resolver, the config, or Nyaa. Pin the contract — a
        // refactor that flipped the cache-key shape (e.g. to
        // `(series_id, ep)` instead of `(request_id, Some(ep))`)
        // would break the lookup and silently re-fetch every poll.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);

        let request_id: i64 = 700;
        let episode: i32 = 5;
        let cache_key = (request_id, Some(episode));
        let seeded = vec![empty_search_result(
            "[Group] Cached Show - 05.mkv",
            "0123456789abcdef0123456789abcdef01234567",
        )];
        interactive_search_cache::insert(
            &state.interactive_search_cache,
            cache_key,
            seeded.clone(),
        );

        let resp = interactive_search_episode(
            State(state),
            axum_htmx::HxRequest(false),
            Path((request_id, episode)),
        )
        .await
        .expect("cache-hit path must succeed without resolver/Nyaa");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let results: Vec<SearchResult> =
            serde_json::from_slice(&bytes).expect("body parses as Vec<SearchResult>");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "[Group] Cached Show - 05.mkv");
    }

    // ─── grab_interactive_result + grab_batch_result happy path ──
    //
    // The two grab handlers gate on `resolve_series_context` (cache-
    // seeded so no AL traffic), then dispatch through
    // `client_for_nyaa_with_id` -> torrent default -> the recording
    // mock client below. Both write a `grabbed_torrents` row and
    // per-episode `episode_quality_tags` rows. These tests pin the
    // full handler chain end-to-end.

    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Minimal recording client for the grab tests. Captures every
    /// `add_torrent_returning_id` call and reports back via
    /// `add_calls()`. Other trait methods are no-ops since the grab
    /// handlers only touch the add path.
    struct GrabRecordingClient {
        add_calls: Mutex<Vec<(String, String)>>, // (url, info_hash)
    }

    impl GrabRecordingClient {
        fn new() -> Self {
            Self {
                add_calls: Mutex::new(Vec::new()),
            }
        }
        fn add_calls(&self) -> Vec<(String, String)> {
            self.add_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl crate::services::download_client::DownloadClient for GrabRecordingClient {
        async fn test(&self) -> Result<String, String> {
            Ok("mock".into())
        }
        async fn add_torrent(
            &self,
            url: &str,
            info_hash: &str,
        ) -> Result<crate::services::download_client::AddOutcome, String> {
            self.add_calls
                .lock()
                .unwrap()
                .push((url.to_string(), info_hash.to_string()));
            Ok(crate::services::download_client::AddOutcome::Added)
        }
        async fn add_torrent_with_file_filter(
            &self,
            _url: &str,
            _hash: &str,
            _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
        ) -> Result<crate::services::download_client::SelectiveOutcome, String> {
            Ok(crate::services::download_client::SelectiveOutcome::FullDownload)
        }
        async fn list_scoped(
            &self,
        ) -> Result<Vec<crate::services::download_client::DownloadItem>, String> {
            Ok(vec![])
        }
        async fn get_files(
            &self,
            _hash: &str,
        ) -> Result<Vec<crate::services::download_client::DownloadFile>, String> {
            Ok(vec![])
        }
        async fn pause(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
        async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
            Ok(())
        }
        async fn set_file_wanted(
            &self,
            _hash: &str,
            _files: &[usize],
            _wanted: bool,
        ) -> Result<(), String> {
            Ok(())
        }
        fn sonarr_impl_name(&self) -> &'static str {
            "QBittorrent"
        }
    }

    async fn install_grab_pool(
        state: &crate::AppState,
        client: Arc<dyn crate::services::download_client::DownloadClient>,
    ) {
        let mut clients: std::collections::HashMap<
            i64,
            Arc<dyn crate::services::download_client::DownloadClient>,
        > = std::collections::HashMap::new();
        clients.insert(1, client);
        let pool = crate::DownloadClientPool {
            clients,
            default_torrent_id: Some(1),
            default_usenet_id: None,
        };
        *state.download_clients.write().await = Arc::new(pool);
    }

    #[tokio::test]
    async fn grab_interactive_result_records_grab_and_writes_episode_tag() {
        // Cache-seeded series + recording torrent default -> the
        // handler resolves series context without network, picks the
        // torrent default for Nyaa-flavored grabs, calls
        // add_torrent_returning_id, persists a grabbed_torrents row,
        // writes the per-episode quality tag, and returns Json{ok}.
        // Pins the full happy-path chain. Pre-this-test grab.rs was
        // 0% covered.
        use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

        let db = in_memory_pool().await;
        let anilist_id: i64 = 800;
        let series_id = seed_series(&db, anilist_id, "Grab Show").await;
        let detail = empty_anime_detail(anilist_id, "Grab Show");
        crate::models::metadata_cache::upsert(&db, series_id, anilist_id, None, &detail)
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let client = Arc::new(GrabRecordingClient::new());
        install_grab_pool(
            &state,
            client.clone() as Arc<dyn crate::services::download_client::DownloadClient>,
        )
        .await;

        // Empty info_hash so wants_selective short-circuits to false
        // and we go straight through add_torrent_returning_id —
        // exercises the dominant path, not the rare selective one.
        let body = serde_json::json!({
            "url": "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            "title": "[Group] Grab Show - 03 (1080p) [WEB].mkv",
            "group": "Group",
            "resolution": "1080p",
            "info_hash": "",
            "size_bytes": 1_400_000_000_i64,
        });
        let result = super::super::grab::grab_interactive_result(
            axum::extract::State(state),
            axum::extract::Path((anilist_id, 3_i32)),
            axum::extract::Json(body),
        )
        .await;
        let axum::response::Json(resp) = result.expect("grab must succeed");
        assert_eq!(resp["ok"], true);

        // The recording client saw exactly one add_torrent call.
        let calls = client.add_calls();
        assert_eq!(calls.len(), 1, "exactly one add_torrent call expected");
        assert!(
            calls[0].0.starts_with("magnet:?xt=urn:btih:"),
            "url passed through verbatim"
        );

        // grabbed_torrents row exists, scoped to the series, with the
        // expected episode list and download_client_id stamp.
        let row: (i64, String, i64) = sqlx::query_as(
            "SELECT series_id, episode_numbers, download_client_id FROM grabbed_torrents WHERE torrent_name = ?",
        )
        .bind("[Group] Grab Show - 03 (1080p) [WEB].mkv")
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row.0, series_id);
        assert_eq!(row.1, "[3]");
        assert_eq!(row.2, 1, "stamped with the resolved client id");

        // episode_quality_tags row was written for the grabbed episode.
        let tag_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
        )
        .bind(series_id)
        .bind(3_i32)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(tag_count, 1);
    }

    #[tokio::test]
    async fn grab_interactive_result_returns_400_when_url_is_empty() {
        // The handler's url-empty guard is BEFORE the cache /
        // resolve / client resolution chain, but cache-seeded so we
        // can isolate the 400 surface. Pinning this branch protects
        // against a refactor that swallowed the empty-URL case as a
        // silent success.
        use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

        let db = in_memory_pool().await;
        let anilist_id: i64 = 801;
        let series_id = seed_series(&db, anilist_id, "Empty URL").await;
        let detail = empty_anime_detail(anilist_id, "Empty URL");
        crate::models::metadata_cache::upsert(&db, series_id, anilist_id, None, &detail)
            .await
            .unwrap();

        let state = build_test_app_state(db, None);
        let body = serde_json::json!({
            "url": "",
            "title": "[Group] Empty URL - 01.mkv",
        });
        let result = super::super::grab::grab_interactive_result(
            axum::extract::State(state),
            axum::extract::Path((anilist_id, 1_i32)),
            axum::extract::Json(body),
        )
        .await;
        match result {
            Err((status, body)) => {
                assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
                assert!(body.contains("No URL provided"));
            }
            Ok(_) => panic!("empty url must surface as 400, not Ok"),
        }
    }

    #[tokio::test]
    async fn grab_interactive_result_returns_400_when_no_download_client_configured() {
        // Cache hit + empty pool -> client_for_nyaa_with_id returns
        // None -> handler 400s with "Download client not configured".
        // Pins the resolved-client.ok_or branch which previously had
        // no direct test.
        use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

        let db = in_memory_pool().await;
        let anilist_id: i64 = 802;
        let series_id = seed_series(&db, anilist_id, "No Client").await;
        let detail = empty_anime_detail(anilist_id, "No Client");
        crate::models::metadata_cache::upsert(&db, series_id, anilist_id, None, &detail)
            .await
            .unwrap();

        // Default state has no download_clients pool entries.
        let state = build_test_app_state(db, None);
        let body = serde_json::json!({
            "url": "magnet:?xt=urn:btih:1111111111111111111111111111111111111111",
            "title": "[Group] No Client - 02.mkv",
        });
        let result = super::super::grab::grab_interactive_result(
            axum::extract::State(state),
            axum::extract::Path((anilist_id, 2_i32)),
            axum::extract::Json(body),
        )
        .await;
        match result {
            Err((status, body)) => {
                assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
                assert!(body.contains("Download client not configured"));
            }
            Ok(_) => panic!("no-client must surface as 400"),
        }
    }

    #[tokio::test]
    async fn grab_batch_result_records_per_episode_tags_with_anilist_count_fallback() {
        // Realistic complete-batch shape — title carries `(BD 1080p)`
        // but no episode range, so batch_episode_numbers falls back
        // to AnimeDetail.episodes (the AniList-reported count).
        // empty_anime_detail seeds episodes: Some(26), so the
        // handler fans out 26 per-episode quality tags. Pins the
        // fallback path that real-world batches actually exercise —
        // most release groups don't tag the range in the title.
        use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

        let db = in_memory_pool().await;
        let anilist_id: i64 = 803;
        let series_id = seed_series(&db, anilist_id, "Batch Show").await;
        let detail = empty_anime_detail(anilist_id, "Batch Show");
        crate::models::metadata_cache::upsert(&db, series_id, anilist_id, None, &detail)
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let client = Arc::new(GrabRecordingClient::new());
        install_grab_pool(
            &state,
            client.clone() as Arc<dyn crate::services::download_client::DownloadClient>,
        )
        .await;

        let body = serde_json::json!({
            "url": "magnet:?xt=urn:btih:2222222222222222222222222222222222222222",
            "title": "[Group] Batch Show (BD 1080p)",
            "group": "Group",
            "resolution": "1080p",
            "info_hash": "",
            "size_bytes": 12_000_000_000_i64,
        });
        let result = super::super::grab::grab_batch_result(
            axum::extract::State(state),
            axum::extract::Path(anilist_id),
            axum::extract::Json(body),
        )
        .await;
        let axum::response::Json(resp) = result.expect("batch grab must succeed");
        assert_eq!(resp["ok"], true);

        // is_batch=true on the grabbed_torrents row.
        let is_batch: i64 =
            sqlx::query_scalar("SELECT is_batch FROM grabbed_torrents WHERE torrent_name = ?")
                .bind("[Group] Batch Show (BD 1080p)")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(is_batch, 1, "batch grab must mark is_batch=true");

        // 26 per-episode quality tags written — empty_anime_detail
        // seeds episodes: Some(26), and the no-range title falls
        // through to that AL count.
        let tag_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM episode_quality_tags WHERE series_id = ?")
                .bind(series_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            tag_count, 26,
            "no-range batch title must fall back to AnimeDetail.episodes (26) \
             and record_grab fans out one tag per episode"
        );
    }

    #[tokio::test]
    async fn interactive_search_batches_returns_cached_results_when_present() {
        // Same shape as the per-episode test above, but the batch
        // variant uses `(request_id, None)` as the cache key. Pin
        // the None slot so a refactor that defaulted to `Some(0)`
        // wouldn't silently confuse batch and episode-1 caches.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);

        let request_id: i64 = 701;
        let cache_key = (request_id, None);
        let seeded = vec![
            empty_search_result(
                "[Group] Show - 01-12 Batch (1080p)",
                "abcdef0123456789abcdef0123456789abcdef01",
            ),
            empty_search_result(
                "[Group] Show - 01-24 Complete BD",
                "fedcba9876543210fedcba9876543210fedcba98",
            ),
        ];
        interactive_search_cache::insert(
            &state.interactive_search_cache,
            cache_key,
            seeded.clone(),
        );

        let resp =
            interactive_search_batches(State(state), axum_htmx::HxRequest(false), Path(request_id))
                .await
                .expect("cache-hit path must succeed");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let results: Vec<SearchResult> =
            serde_json::from_slice(&bytes).expect("body parses as Vec<SearchResult>");
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|r| r.title.contains("01-12")));
        assert!(results.iter().any(|r| r.title.contains("01-24")));
    }
}
