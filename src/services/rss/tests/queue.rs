//! Sonarr's queue check (`judge_queued_revision`) and the history it
//! reads (`CanonicalHistory`): a `v2` posting while its `v1` is still
//! downloading is judged against the queued release, since the file
//! gate sees disk files only. Before this check the `v2` counted as a
//! new episode and was grabbed whatever the proper policy said.

use super::super::*;
use crate::services::source::{ProperPolicy, Resolution, Source, UpgradePolicy};

fn series_named(title: &str) -> series::Series {
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
        alternate_titles: String::new(),
        cumulative_prior_episodes: 0,
        monitor_mode_manual_override: false,
        user_score: None,
        added_at: String::new(),
    }
}

fn meta() -> Vec<SeriesMeta> {
    vec![SeriesMeta::from_series(&series_named("Test Series"))]
}

fn item(title: &str) -> RssItem {
    RssItem {
        title: title.to_string(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: extract_group(title),
        resolution: "1080".to_string(),
        is_batch: false,
        source: RssSource::Nyaa,
    }
}

/// The match the sweep would build for a feed item naming "Test Series".
fn found_for(title: &str) -> MatchResult {
    best_series_match(&item(title), &meta()).expect("the title names the series")
}

fn policy(propers: ProperPolicy) -> UpgradePolicy {
    UpgradePolicy {
        cutoff: source::cutoff_classification(Source::BluRay, Resolution::R1080p, false, false),
        propers,
        format_cutoff_score: 0,
        format_increment: 1,
    }
}

#[test]
fn the_history_keeps_a_v2_apart_from_its_v1_and_still_finds_the_v1() {
    let v1_title = "[Group] Test Series - 05 (1080p)";
    let v1 = found_for(v1_title);
    let v2 = found_for("[Group] Test Series - 05v2 (1080p)");
    let mut history = CanonicalHistory::default();
    history.insert(&v1, false, v1_title);

    assert!(history.contains(&v1, false, 1), "the v1 itself is history");
    assert!(!history.contains(&v2, false, 2), "the v2 is a new release");
    assert_eq!(
        history.lower_revision(&v2, false, 2),
        Some(&HistoryRelease {
            revision: 1,
            group: "Group".to_string(),
        }),
        "the queued v1 is what the v2 is judged against"
    );
    assert_eq!(
        history.lower_revision(&v1, false, 1),
        None,
        "nothing below a v1"
    );
    let other = found_for("[Group] Test Series - 06v2 (1080p)");
    assert_eq!(
        history.lower_revision(&other, false, 2),
        None,
        "another episode is another item"
    );
}

#[test]
fn insert_title_matches_the_library_the_way_the_sweep_does() {
    let mut history = CanonicalHistory::default();
    history.insert_title("[Group] Test Series - 05 (1080p)", &meta());
    history.insert_title("[Group] Some Other Show - 05 (1080p)", &meta());
    let v2 = found_for("[Group] Test Series - 05v2 (1080p)");
    assert_eq!(
        history.lower_revision(&v2, false, 2).map(|r| r.revision),
        Some(1),
        "a grabbed title or a queued torrent name lands under its item"
    );
    assert!(
        !history.contains(&v2, false, 2),
        "a title naming no series inserts nothing, and the v2 is not the v1"
    );
}

#[test]
fn judge_queued_revision_needs_prefer_and_upgrade_and_the_same_group() {
    let queued = HistoryRelease {
        revision: 1,
        group: "SubsPlease".to_string(),
    };
    assert_eq!(
        judge_queued_revision(
            &policy(ProperPolicy::PreferAndUpgrade),
            &queued,
            "subsplease"
        ),
        Ok(()),
        "same group, case aside, under prefer-and-upgrade"
    );
    let disabled =
        judge_queued_revision(&policy(ProperPolicy::DoNotUpgrade), &queued, "SubsPlease")
            .expect_err("prefer, do not upgrade keeps the queued v1");
    assert!(
        disabled.contains("revision upgrades are disabled"),
        "{disabled}"
    );
    let ignored = judge_queued_revision(&policy(ProperPolicy::DoNotPrefer), &queued, "SubsPlease")
        .expect_err("do not prefer keeps the queued v1 too");
    assert!(ignored.contains("already queued"), "{ignored}");
    let other_group = judge_queued_revision(
        &policy(ProperPolicy::PreferAndUpgrade),
        &queued,
        "Erai-raws",
    )
    .expect_err("another group's v2 is not a fix of the queued release");
    assert!(other_group.contains("same group"), "{other_group}");
    let unknown = judge_queued_revision(&policy(ProperPolicy::PreferAndUpgrade), &queued, "")
        .expect_err("an unknown group cannot pass the group rule");
    assert!(unknown.contains("unknown"), "{unknown}");
}
