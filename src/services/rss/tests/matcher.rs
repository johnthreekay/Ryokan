//! `best_series_match` reads episodes through `release_title`: a number
//! inside a series' name is never an episode, a bare number right after
//! it is, an absolute number past `cumulative_prior_episodes` maps to the
//! series' own numbering, and an exact tie no longer goes to list order.

use super::super::*;

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
