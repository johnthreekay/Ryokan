//! Pure-helper coverage for the Radarr shim. Radarr's nested
//! `RadarrRatings` shape duplicates the score across both `imdb`
//! and `tmdb` slots — Seerr reads whichever slot fits its render
//! path, so populating only one would render a missing-rating
//! state in some Seerr versions. Pin both slots.

use crate::handlers::radarr_compat::helpers::{map_status, ratings_from_score};

// ── ratings_from_score ───────────────────────────────────────────────

#[test]
fn ratings_from_score_none_zeros_both_slots() {
    let r = ratings_from_score(None);
    assert_eq!(r.imdb.value, 0.0);
    assert_eq!(r.imdb.votes, 0);
    assert_eq!(r.tmdb.value, 0.0);
    assert_eq!(r.tmdb.votes, 0);
}

#[test]
fn ratings_from_score_zero_or_negative_zeros_both_slots() {
    for s in [Some(0), Some(-1)] {
        let r = ratings_from_score(s);
        assert_eq!(r.imdb.value, 0.0);
        assert_eq!(r.tmdb.value, 0.0);
    }
}

#[test]
fn ratings_from_score_divides_by_ten_and_mirrors_to_both_slots() {
    // The same value lands in both slots so Seerr renders a
    // rating regardless of which slot it reads.
    let r = ratings_from_score(Some(85));
    assert_eq!(r.imdb.value, 8.5);
    assert_eq!(r.tmdb.value, 8.5);
}

#[test]
fn ratings_from_score_rating_type_user_in_both_slots() {
    // The rating-type label is hardcoded "user" — Radarr uses this
    // to distinguish IMDB's site rating from a per-user score; we
    // only have the AL community average, which is closest to a
    // user-rating shape.
    let r = ratings_from_score(Some(75));
    assert_eq!(r.imdb.rating_type, "user");
    assert_eq!(r.tmdb.rating_type, "user");
}

// ── map_status ───────────────────────────────────────────────────────

#[test]
fn map_status_releasing_to_announced() {
    // Radarr's vocabulary differs from Sonarr's — movies use
    // "announced" / "released" rather than "continuing" / "ended".
    assert_eq!(map_status("RELEASING"), "announced");
    assert_eq!(map_status("NOT_YET_RELEASED"), "announced");
}

#[test]
fn map_status_finished_to_released() {
    assert_eq!(map_status("FINISHED"), "released");
    assert_eq!(map_status("FINISHED_AIRING"), "released");
    assert_eq!(map_status("CANCELLED"), "released");
}

#[test]
fn map_status_is_case_insensitive() {
    assert_eq!(map_status("releasing"), "announced");
    assert_eq!(map_status("Finished"), "released");
}

#[test]
fn map_status_unknown_defaults_to_released() {
    // Movies default to "released" — for an unknown AL status,
    // assume the movie is out (Radarr's "released" state). The
    // Sonarr side defaults to "continuing" because TV is more
    // forgiving of an "in progress" assumption; the Radarr side
    // leans the other way because most anime "movies" tracked
    // here are theatrical releases that have already premiered.
    assert_eq!(map_status("HIATUS"), "released");
    assert_eq!(map_status(""), "released");
}

// ── build_stub_movie ─────────────────────────────────────────────────

mod stub_movie {
    use crate::handlers::radarr_compat::helpers::build_stub_movie;
    use crate::models::config::Config;

    fn cfg(media_root: &str) -> Config {
        Config {
            media_root: media_root.to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn stub_path_falls_back_when_media_root_empty() {
        // Mirror of the Sonarr stub fallback. Empty media_root →
        // "/media/Unknown" so Seerr's add-step doesn't see a blank
        // path. The folder_name slot is intentionally empty (the
        // movie has no real folder yet).
        let m = build_stub_movie(99, &cfg(""));
        assert_eq!(m.path, "/media/Unknown");
        assert_eq!(m.tmdb_id, 99);
        assert_eq!(m.title, "TMDB:99");
        assert_eq!(m.title_slug, "tmdb-99");
        assert_eq!(m.id, 0);
        assert_eq!(m.folder_name, "");
        assert_eq!(m.runtime, 0);
        assert!(m.is_available, "stub stays available so Seerr proceeds");
    }

    #[test]
    fn stub_path_uses_configured_media_root_when_set() {
        let m = build_stub_movie(101, &cfg("/data/movies"));
        assert_eq!(m.path, "/data/movies/Unknown");
        assert_eq!(m.root_folder_path, "/data/movies");
    }

    #[test]
    fn stub_carries_zeroed_ratings_and_monitored_default() {
        let m = build_stub_movie(1, &cfg(""));
        assert_eq!(m.ratings.imdb.value, 0.0);
        assert_eq!(m.ratings.tmdb.value, 0.0);
        assert!(m.monitored, "stub defaults to monitored=true");
        assert!(!m.has_file);
    }
}

// ── cached_detail_for + build_radarr_movie_from_tracked ─────────────

mod from_tracked {
    use crate::handlers::radarr_compat::helpers::{
        build_radarr_movie_from_tracked, cached_detail_for,
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
            format: "MOVIE".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: Some(1),
            duration: Some(120),
            season: String::new(),
            season_year: Some(2024),
            end_year: Some(2024),
            description: String::new(),
            genres: vec![],
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
        assert!(cached_detail_for(&db, 12345).await.is_none());
    }

    #[tokio::test]
    async fn cached_detail_for_round_trips_through_metadata_cache() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 555, "Movie").await;
        metadata_cache::upsert(&db, series_id, 555, None, &detail_with_score(555, Some(70)))
            .await
            .unwrap();
        let detail = cached_detail_for(&db, series_id).await.expect("hit");
        assert_eq!(detail.average_score, Some(70));
    }

    #[tokio::test]
    async fn build_from_tracked_uses_romaji_when_title_blank() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1, "Movie Title").await;
        sqlx::query("UPDATE series SET title = '' WHERE id = ?")
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();
        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();

        let cfg = Config::default();
        let out = build_radarr_movie_from_tracked(&row, None, /*tmdb_id=*/ 222, &cfg).await;
        assert_eq!(out.title, "Movie Title");
        assert_eq!(out.tmdb_id, 222);
        assert_eq!(out.title_slug, format!("ryokan-{}", row.anilist_id));
        assert!(out.monitored);
        // No detail provided ⇒ both rating slots zeroed.
        assert_eq!(out.ratings.imdb.value, 0.0);
        assert_eq!(out.ratings.tmdb.value, 0.0);
    }

    #[tokio::test]
    async fn build_from_tracked_propagates_score_to_both_rating_slots() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 2, "Movie").await;
        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        let detail = detail_with_score(2, Some(85));
        let cfg = Config::default();
        let out = build_radarr_movie_from_tracked(&row, Some(&detail), 0, &cfg).await;
        assert_eq!(out.ratings.imdb.value, 8.5);
        assert_eq!(out.ratings.tmdb.value, 8.5);
    }

    #[tokio::test]
    async fn build_from_tracked_unmonitored_when_monitor_mode_is_none() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 3, "Movie").await;
        sqlx::query("UPDATE series SET monitor_mode = 'none' WHERE id = ?")
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();
        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        let cfg = Config::default();
        let out = build_radarr_movie_from_tracked(&row, None, 0, &cfg).await;
        assert!(!out.monitored);
    }
}

// ── build_radarr_movie_from_search ──────────────────────────────────

mod from_search {
    use crate::handlers::radarr_compat::helpers::build_radarr_movie_from_search;
    use crate::models::config::Config;
    use crate::services::anilist::AnimeEntry;

    fn entry(score: Option<i32>) -> AnimeEntry {
        AnimeEntry {
            id: 8888,
            id_mal: None,
            title_romaji: "Romaji".into(),
            title_english: "English".into(),
            title_native: String::new(),
            cover_url: "cover".into(),
            format: "MOVIE".into(),
            status: "RELEASING".into(),
            status_display: String::new(),
            episodes: Some(1),
            season_year: Some(2025),
            source: String::new(),
            average_score: score,
        }
    }

    #[tokio::test]
    async fn build_from_search_with_no_db_series_uses_id_zero_and_blank_added() {
        let cfg = Config::default();
        let out =
            build_radarr_movie_from_search(&entry(Some(80)), "English Title", 333, None, &cfg)
                .await;
        assert_eq!(out.id, 0);
        // Blank `added` for not-yet-imported entries — Seerr uses the
        // empty string as a "library has it?" signal in some versions.
        assert_eq!(out.added, "");
        assert!(!out.has_file);
        assert!(out.monitored, "default to monitored=true pre-add");
        assert_eq!(out.tmdb_id, 333);
        assert_eq!(out.title_slug, "ryokan-8888");
        assert_eq!(out.year, 2025);
        assert_eq!(out.ratings.imdb.value, 8.0);
        assert_eq!(out.ratings.tmdb.value, 8.0);
    }

    #[tokio::test]
    async fn build_from_search_clean_title_strips_non_alphanumeric() {
        let cfg = Config::default();
        let out =
            build_radarr_movie_from_search(&entry(None), "Foo: Bar — Movie!", 0, None, &cfg).await;
        assert_eq!(out.clean_title, "foobarmovie");
    }
}

// ── lookup_by_tmdb_id ────────────────────────────────────────────────

mod lookup {
    use crate::handlers::radarr_compat::helpers::lookup_by_tmdb_id;
    use crate::models::config::Config;
    use crate::test_support::{build_test_app_state, in_memory_pool};

    #[tokio::test]
    async fn lookup_returns_stub_when_anibridge_has_no_mapping_for_tmdb_id() {
        // Empty anibridge cache (tests don't load the real mappings)
        // → no-mapping branch returns the `TMDB:<id>` stub so Seerr
        // can proceed to its add-step.
        let state = build_test_app_state(in_memory_pool().await, None);
        let cfg = Config::default();
        let result = lookup_by_tmdb_id(&state, &cfg, 0).await.expect("Ok");
        let body = result.0;
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].title, "TMDB:0");
        assert_eq!(body[0].title_slug, "tmdb-0");
    }
}
