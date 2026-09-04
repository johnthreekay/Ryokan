//! The RSS sync takes a release only when its title names a library
//! series exactly (`alias_score >= 1.0` from `best_series_match`); a
//! token-overlap match is rejected with "Title does not name the
//! series exactly". These pin the threshold the loop relies on and
//! that an alternate title turns a rejected item into an exact one.

use super::super::*;

fn series_named(title: &str, alternate_titles: &str) -> series::Series {
    series::Series {
        is_adult: false,
        id: 1,
        anilist_id: 101,
        mal_id: None,
        title: title.to_string(),
        title_romaji: title.to_string(),
        title_english: String::new(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "TV".to_string(),
        status: "RELEASING".to_string(),
        episodes: Some(12),
        season_year: None,
        end_year: None,
        folder_name: title.to_string(),
        monitor_mode: "all".to_string(),
        allow_upgrades: true,
        allow_pt_upgrades: false,
        custom_query_tokens: String::new(),
        restrict_to_uploader: String::new(),
        alternate_titles: alternate_titles.to_string(),
        cumulative_prior_episodes: 0,
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

#[test]
fn verbatim_title_scores_exact_and_reordered_words_do_not() {
    let meta = vec![SeriesMeta::from_series(&series_named(
        "Auto Search Show Deluxe",
        "",
    ))];
    let exact = best_series_match(&item("[Group] Auto Search Show Deluxe - 01 (1080p)"), &meta)
        .expect("verbatim title matches");
    assert!(exact.alias_score >= 1.0, "{}", exact.alias_score);

    let reordered = best_series_match(&item("[Group] Deluxe Auto Search Show - 01 (1080p)"), &meta);
    if let Some(found) = reordered {
        assert!(
            found.alias_score < 1.0,
            "same words in another order must stay below the exact threshold, got {}",
            found.alias_score
        );
    }
}

#[test]
fn alternate_title_makes_the_reordered_name_exact() {
    let meta = vec![SeriesMeta::from_series(&series_named(
        "Auto Search Show Deluxe",
        "Deluxe Auto Search Show",
    ))];
    let found = best_series_match(&item("[Group] Deluxe Auto Search Show - 01 (1080p)"), &meta)
        .expect("alternate title matches");
    assert!(found.alias_score >= 1.0, "{}", found.alias_score);
}
