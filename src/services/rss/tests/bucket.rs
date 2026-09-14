//! The within-sweep bucket (`logical_bucket_key`) and its winner
//! (`compare_candidates`): a `v1` and its `v2` deliver the same logical
//! item, so they share a bucket and one feed window listing both grabs
//! one, the later revision at equal score. The history key
//! (`canonical_episode_key`) keeps the revision, so a `v2` that arrives
//! after the `v1` was grabbed is not swallowed as already grabbed.

use super::super::*;
use crate::services::source::{DecisionRule, Resolution, Source};

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

/// A candidate the way the sweep builds one, through the real title
/// match against a series named "Test Series".
fn candidate(title: &str, score: i32) -> PendingCandidate {
    let meta = vec![SeriesMeta::from_series(&series_named("Test Series"))];
    let item = item(title);
    let found = best_series_match(&item, &meta).expect("the title names the series");
    PendingCandidate {
        item_key: title.to_string(),
        item,
        found,
        score,
        new_episode_count: 1,
        is_upgrade: false,
        classification: ClassificationResult {
            source: Source::Web,
            resolution: Resolution::R1080p,
            is_remux: false,
            web_kind: source::WebKind::Unknown,
            is_bdmv: false,
            confidence: 1.0,
            needs_review: false,
            evidence: Vec::new(),
            decision_rule: DecisionRule::Empty,
        },
    }
}

fn history_key(cand: &PendingCandidate) -> String {
    canonical_episode_key(
        &cand.found,
        cand.item.is_batch,
        media::parse_release_revision(&cand.item.title).version,
    )
}

#[test]
fn a_v1_and_its_v2_share_a_bucket_and_the_v2_wins_at_equal_score() {
    let v1 = candidate("[Group] Test Series - 05 (1080p)", 100);
    let v2 = candidate("[Group] Test Series - 05v2 (1080p)", 100);
    let key = logical_bucket_key(&v1);
    assert!(!key.is_empty(), "the episode resolved");
    assert_eq!(
        key,
        logical_bucket_key(&v2),
        "the revision is not part of the bucket key; both used to be grabbed"
    );
    assert_eq!(compare_candidates(&v2, &v1), Ordering::Greater);
    assert_eq!(compare_candidates(&v1, &v2), Ordering::Less);
    // Score still comes first: a better-scored v1 beats the v2.
    let better_v1 = candidate("[Group] Test Series - 05 (1080p)", 200);
    assert_eq!(compare_candidates(&better_v1, &v2), Ordering::Greater);
    // The history key keeps them apart, so a later v2 is not "already
    // grabbed" once the v1 is in the history.
    assert_ne!(history_key(&v1), history_key(&v2));
    assert!(history_key(&v1).ends_with("|v1"));
    assert!(history_key(&v2).ends_with("|v2"));
}

#[test]
fn another_episode_or_a_batch_is_another_bucket() {
    let e5 = candidate("[Group] Test Series - 05 (1080p)", 100);
    let e6 = candidate("[Group] Test Series - 06 (1080p)", 100);
    assert_ne!(logical_bucket_key(&e5), logical_bucket_key(&e6));
    let mut pack = candidate("[Group] Test Series - 01-12 (1080p) [Batch]", 100);
    pack.item.is_batch = true;
    assert!(logical_bucket_key(&pack).contains("|batch|"));
    assert!(logical_bucket_key(&e5).contains("|single|"));
}
