//! Pure-helper coverage for the Sonarr shim — `ratings_from_score`
//! and `map_status` are tiny but load-bearing. Both feed every series
//! payload Seerr sees. A wrong rating denominator (Sonarr expects
//! 0-10, AL is 0-100) flashes an obviously-broken score in the
//! Seerr UI; a wrong `map_status` value silently corrupts Seerr's
//! "show ended" detection so finished anime never get marked as
//! such on its dashboard.

use crate::handlers::sonarr_compat::helpers::{map_status, ratings_from_score};

// ── ratings_from_score ───────────────────────────────────────────────

#[test]
fn ratings_from_score_none_renders_zeroed() {
    // The "metadata never refreshed" shape — Sonarr itself emits
    // this for new entries, so Seerr handles it cleanly.
    let r = ratings_from_score(None);
    assert_eq!(r.votes, 0);
    assert_eq!(r.value, 0.0);
}

#[test]
fn ratings_from_score_zero_or_negative_renders_zeroed() {
    // AL emits `averageScore: null` as Some(0) after our parser's
    // `unwrap_or(0)` fallback. Treat both 0 and the (theoretically
    // impossible) negative case as "no rating" rather than letting
    // a 0.0 value rendered in Seerr suggest a real bottom-of-scale
    // score.
    assert_eq!(ratings_from_score(Some(0)).value, 0.0);
    assert_eq!(ratings_from_score(Some(-5)).value, 0.0);
}

#[test]
fn ratings_from_score_divides_by_ten_for_zero_to_ten_scale() {
    // AL: 0-100 integer. Sonarr/Seerr: 0-10 float. Pin the
    // denominator at 10 — a regression that drops the divisor would
    // produce visibly broken 8500-style ratings in Seerr.
    assert_eq!(ratings_from_score(Some(85)).value, 8.5);
    assert_eq!(ratings_from_score(Some(100)).value, 10.0);
    assert_eq!(ratings_from_score(Some(1)).value, 0.1);
}

#[test]
fn ratings_from_score_votes_always_zero() {
    // We don't have a vote count from AL/MAL that maps cleanly to
    // Sonarr's notion. 0-with-a-non-zero-value is the shape Sonarr
    // itself emits for unrated newer entries, so Seerr handles it.
    for s in [None, Some(0), Some(50), Some(100)] {
        assert_eq!(ratings_from_score(s).votes, 0, "score {:?}", s);
    }
}

// ── map_status ───────────────────────────────────────────────────────

#[test]
fn map_status_releasing_to_continuing() {
    // AL's "RELEASING" + "NOT_YET_RELEASED" both map to Sonarr's
    // "continuing" — the show is still emitting episodes from
    // Sonarr's POV.
    assert_eq!(map_status("RELEASING"), "continuing");
    assert_eq!(map_status("NOT_YET_RELEASED"), "continuing");
}

#[test]
fn map_status_finished_to_ended() {
    // AL's "FINISHED" / "FINISHED_AIRING" / "CANCELLED" all map to
    // Sonarr's "ended". Cancelled is bundled with finished because
    // Sonarr has no separate concept for canceled-mid-air; both
    // mean "no more episodes coming."
    assert_eq!(map_status("FINISHED"), "ended");
    assert_eq!(map_status("FINISHED_AIRING"), "ended");
    assert_eq!(map_status("CANCELLED"), "ended");
}

#[test]
fn map_status_is_case_insensitive() {
    // The AL parser sometimes lower-cases the status field on
    // negative-id (Jikan-fallback) rows. Pin the case-insensitivity
    // so a parser tweak doesn't silently break the mapping.
    assert_eq!(map_status("releasing"), "continuing");
    assert_eq!(map_status("Finished"), "ended");
}

#[test]
fn map_status_unknown_defaults_to_continuing() {
    // Defensive default — better to mark an unknown status as
    // "continuing" (Sonarr keeps watching) than as "ended" (Sonarr
    // stops monitoring entirely). New AL status variants get a
    // safe landing without a code change.
    assert_eq!(map_status("HIATUS"), "continuing");
    assert_eq!(map_status(""), "continuing");
}

// ── build_stub_series ─────────────────────────────────────────────────

mod stub_series {
    use crate::handlers::sonarr_compat::helpers::build_stub_series;
    use crate::models::config::Config;

    fn cfg(media_root: &str) -> Config {
        Config {
            media_root: media_root.to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn stub_path_falls_back_to_media_unknown_when_root_empty() {
        // Stub is the fallback we hand back to Seerr when anibridge
        // can't resolve a TVDB id; the path must still look plausible
        // so Seerr's add-step doesn't bail. Empty media_root falls
        // back to "/media/Unknown" rather than producing an
        // empty-string path.
        let s = build_stub_series(12345, &cfg(""));
        assert_eq!(s.path, "/media/Unknown");
        assert_eq!(s.tvdb_id, 12345);
        assert_eq!(s.title, "TVDB:12345");
        assert_eq!(s.title_slug, "tvdb-12345");
        assert_eq!(s.id, 0); // not in our library yet
        assert_eq!(s.series_type, "anime");
        assert_eq!(s.runtime, 24);
    }

    #[test]
    fn stub_path_uses_configured_media_root_when_set() {
        let s = build_stub_series(7777, &cfg("/data/anime"));
        assert_eq!(s.path, "/data/anime/Unknown");
        assert_eq!(s.root_folder_path, "/data/anime");
    }

    #[test]
    fn stub_carries_zeroed_statistics_and_one_monitored_season() {
        let s = build_stub_series(99, &cfg(""));
        assert_eq!(s.seasons.len(), 1);
        assert_eq!(s.seasons[0].season_number, 1);
        assert!(s.seasons[0].monitored);
        assert_eq!(s.seasons[0].statistics.episode_count, 0);
        assert_eq!(s.statistics.season_count, 1);
        assert_eq!(s.ratings.value, 0.0);
    }
}

// ── cached_detail_for + build_sonarr_series_from_tracked ──────────────

mod from_tracked {
    use crate::handlers::sonarr_compat::helpers::{
        build_sonarr_series_from_tracked, cached_detail_for,
    };
    use crate::models::config::Config;
    use crate::models::{metadata_cache, series};
    use crate::services::anilist;
    use crate::test_support::{in_memory_pool, seed_series};

    fn detail_with_score(id: i64, score: Option<i32>) -> anilist::AnimeDetail {
        anilist::AnimeDetail {
            is_adult: false,
            id,
            id_mal: None,
            title_romaji: "Romaji".into(),
            title_english: "English".into(),
            title_native: String::new(),
            cover_url: "cover".into(),
            banner_url: String::new(),
            format: "TV".into(),
            status: "RELEASING".into(),
            status_display: String::new(),
            episodes: Some(12),
            duration: Some(24),
            season: String::new(),
            season_year: Some(2024),
            end_year: None,
            description: String::new(),
            genres: vec!["Action".into()],
            average_score: score,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: vec![],
            streaming_episodes: vec![],
            relations: vec![],
        }
    }

    #[tokio::test]
    async fn cached_detail_for_returns_none_on_miss() {
        let db = in_memory_pool().await;
        assert!(cached_detail_for(&db, 999).await.is_none());
    }

    #[tokio::test]
    async fn cached_detail_for_round_trips_through_metadata_cache() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1234, "Show").await;
        metadata_cache::upsert(
            &db,
            series_id,
            1234,
            None,
            &detail_with_score(1234, Some(85)),
        )
        .await
        .unwrap();
        let detail = cached_detail_for(&db, series_id).await.expect("hit");
        assert_eq!(detail.id, 1234);
        assert_eq!(detail.average_score, Some(85));
    }

    #[tokio::test]
    async fn build_from_tracked_uses_romaji_when_title_blank() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1, "Show Title").await;
        // Force `title` blank so the fallback branch fires; seed_series
        // wrote it non-empty by default.
        sqlx::query("UPDATE series SET title = '' WHERE id = ?")
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();
        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();

        let cfg = Config::default();
        let out = build_sonarr_series_from_tracked(&row, None, /*tmdb_id=*/ 5555, &cfg).await;
        assert_eq!(out.title, "Show Title", "expected romaji fallback");
        assert_eq!(out.tvdb_id, 5555);
        assert!(out.monitored);
        assert_eq!(out.seasons.len(), 1);
        // No detail provided ⇒ rating zero.
        assert_eq!(out.ratings.value, 0.0);
        // Empty media_root falls back to "/media/<folder>".
        assert!(out.path.starts_with("/media/"));
    }

    #[tokio::test]
    async fn build_from_tracked_propagates_average_score_via_detail() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 2, "Show").await;
        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        let detail = detail_with_score(2, Some(90));
        let cfg = Config::default();
        let out = build_sonarr_series_from_tracked(&row, Some(&detail), 0, &cfg).await;
        // 0-100 ÷ 10 = 9.0 (Sonarr's 0-10 scale).
        assert_eq!(out.ratings.value, 9.0);
    }

    #[tokio::test]
    async fn build_from_tracked_unmonitored_when_monitor_mode_is_none() {
        // monitor_mode = "none" must propagate into the SonarrSeries
        // root + season `monitored` flags so Seerr's UI mirrors it.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 3, "Show").await;
        sqlx::query("UPDATE series SET monitor_mode = 'none' WHERE id = ?")
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();
        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        let cfg = Config::default();
        let out = build_sonarr_series_from_tracked(&row, None, 0, &cfg).await;
        assert!(!out.monitored);
        assert!(!out.seasons[0].monitored);
    }
}

// ── build_sonarr_series_from_search ─────────────────────────────────

mod from_search {
    use crate::handlers::sonarr_compat::helpers::build_sonarr_series_from_search;
    use crate::models::config::Config;
    use crate::services::anilist::AnimeEntry;

    fn entry() -> AnimeEntry {
        AnimeEntry {
            id: 4242,
            id_mal: None,
            title_romaji: "Romaji".into(),
            title_english: "English".into(),
            title_native: String::new(),
            cover_url: "cover".into(),
            format: "TV".into(),
            status: "RELEASING".into(),
            status_display: String::new(),
            episodes: Some(12),
            season_year: Some(2023),
            source: String::new(),
            average_score: Some(75),
        }
    }

    #[tokio::test]
    async fn build_from_search_with_no_db_series_uses_zero_id_and_monitored_default() {
        // Lookup path before the user adds the show: id=0, default
        // monitored=true, no on-disk file count. Title slug encodes
        // the AL id so it survives renames.
        let cfg = Config::default();
        let out =
            build_sonarr_series_from_search(&entry(), "English Title", 9001, None, &cfg).await;
        assert_eq!(out.id, 0);
        assert!(out.monitored);
        assert_eq!(out.title, "English Title");
        assert_eq!(out.title_slug, "ryokan-4242");
        assert_eq!(out.tvdb_id, 9001);
        assert_eq!(out.year, 2023);
        assert_eq!(out.ratings.value, 7.5);
        // Empty media_root → /media/<sanitized-folder>.
        assert!(out.path.starts_with("/media/"));
    }

    #[tokio::test]
    async fn build_from_search_clean_title_strips_non_alphanumeric() {
        // `clean_title` is a normalized search key — punctuation and
        // spaces drop out, case folds to lowercase. Pin so a future
        // tweak to the regex doesn't accidentally start carrying
        // diacritics through.
        let cfg = Config::default();
        let out =
            build_sonarr_series_from_search(&entry(), "Foo: Bar - Part 2!", 0, None, &cfg).await;
        assert_eq!(out.clean_title, "foobarpart2");
    }
}

// ── lookup_by_external_id ─────────────────────────────────────────────

mod lookup {
    use crate::handlers::sonarr_compat::helpers::lookup_by_external_id;
    use crate::models::config::Config;
    use crate::test_support::{build_test_app_state, in_memory_pool};

    #[tokio::test]
    async fn lookup_returns_stub_when_anibridge_has_no_mapping_for_tvdb_id() {
        // Empty anibridge cache (tests don't load the real mappings)
        // guarantees the no-mapping branch fires. Sonarr/Seerr gets
        // the `TVDB:<id>` stub series back so the connection-test +
        // add-step flow can still proceed; the real resolution
        // happens later via title-search.
        let state = build_test_app_state(in_memory_pool().await, None);
        let cfg = Config::default();
        // 0 is a deliberately-impossible TVDB id; even the seeded
        // mappings (if any) won't match it.
        let result = lookup_by_external_id(&state, &cfg, 0).await.expect("Ok");
        let body = result.0;
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].title, "TVDB:0");
        assert_eq!(body[0].title_slug, "tvdb-0");
    }
}
