//! Unit coverage for the `evaluate_candidate` decision tree. The
//! function is synchronous and takes a pre-computed
//! `ClassificationResult`, so these tests drive it directly with
//! in-memory fixtures — no DB or network.

use super::super::*;
use crate::models::episode_tags::EpisodeQualityTag;
use crate::services::source::DecisionRule;

fn series(status: &str) -> series::Series {
    series::Series {
        is_adult: false,
        id: 1,
        anilist_id: 101,
        mal_id: None,
        title: "Test Series".to_string(),
        title_romaji: String::new(),
        title_english: String::new(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "TV".to_string(),
        status: status.to_string(),
        episodes: Some(12),
        season_year: None,
        end_year: None,
        folder_name: "Test Series".to_string(),
        monitor_mode: "all".to_string(),
        allow_upgrades: true,
        allow_pt_upgrades: false,
        custom_query_tokens: String::new(),
        restrict_to_uploader: String::new(),
        cumulative_prior_episodes: 0,
        monitor_mode_manual_override: false,
        user_score: None,
        added_at: String::new(),
    }
}

fn item_with(title: &str, is_batch: bool) -> RssItem {
    RssItem {
        title: title.to_string(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: String::new(),
        resolution: "1080".to_string(),
        is_batch,
        source: RssSource::Nyaa,
    }
}

fn classification(
    src: Source,
    res: Resolution,
    is_remux: bool,
    is_bdmv: bool,
) -> ClassificationResult {
    ClassificationResult {
        source: src,
        resolution: res,
        is_remux,
        web_kind: source::WebKind::Unknown,
        is_bdmv,
        confidence: 1.0,
        needs_review: false,
        evidence: Vec::new(),
        decision_rule: DecisionRule::Empty,
    }
}

fn bluray_cutoff() -> ClassificationResult {
    source::cutoff_classification(Source::BluRay, Resolution::R1080p, false, false)
}

fn disk_file(ep: i32, quality: &str) -> media::EpisodeFile {
    media::EpisodeFile {
        filename: format!("Test Series - S01E{:02}.mkv", ep),
        episode_number: ep,
        season_number: Some(1),
        quality: quality.to_string(),
        size_bytes: 1_000_000_000,
        size_display: String::new(),
    }
}

fn web_tag(ep: i32) -> (i32, EpisodeQualityTag) {
    (
        ep,
        EpisodeQualityTag {
            episode_number: ep,
            quality_tag: "WEB-1080p".to_string(),
            release_title: String::new(),
            release_group: String::new(),
            state: "imported".to_string(),
            source: "Web".to_string(),
            resolution: "1080p".to_string(),
            is_remux: false,
            is_bdmv: false,
            web_kind: String::new(),
            classification_confidence: 0.9,
            needs_review: false,
            manual_override: false,
            classification_evidence: String::new(),
            classification_attempted_at: None,
        },
    )
}

#[test]
fn batch_with_partial_coverage_is_rejected() {
    // Pack covers eps 1..=3. Ep 1 on disk as WEB-1080p (upgradeable
    // to BluRay-1080p incoming). Ep 2 on disk as BluRay-1080p (at
    // cutoff — not upgradeable). Ep 3 missing. Covered=3,
    // actionable=2 (missing + upgrade), so the mixed-coverage
    // rejection fires.
    let cutoff = bluray_cutoff();
    let incoming = classification(Source::BluRay, Resolution::R1080p, false, false);
    let found = series("RELEASING");
    let item = item_with("[Group] Test Series Season 1 (BD 1080p)", true);
    let disk = vec![disk_file(1, "WEB-1080p"), disk_file(2, "BluRay-1080p")];
    let parsed_eps: HashSet<i32> = [1, 2, 3].into_iter().collect();
    let quality_tags: HashMap<i32, EpisodeQualityTag> = [web_tag(1), {
        let (ep, mut tag) = web_tag(2);
        tag.source = "BluRay".to_string();
        tag.quality_tag = "BluRay-1080p".to_string();
        (ep, tag)
    }]
    .into_iter()
    .collect();

    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &cutoff,
        &quality_tags,
    );
    assert!(
        decision.reject_reason.is_some(),
        "expected rejection for mixed coverage"
    );
    let reason = decision.reject_reason.unwrap();
    assert!(
        reason.contains("would overwrite"),
        "reject reason should mention overwrite risk: {reason}"
    );
}

#[test]
fn batch_with_full_coverage_is_accepted() {
    // Same shape as above but all covered episodes actionable
    // (1 upgradeable from WEB, 2 and 3 missing).
    let cutoff = bluray_cutoff();
    let incoming = classification(Source::BluRay, Resolution::R1080p, false, false);
    let found = series("RELEASING");
    let item = item_with("[Group] Test Series Season 1 (BD 1080p)", true);
    let disk = vec![disk_file(1, "WEB-1080p")];
    let parsed_eps: HashSet<i32> = [1, 2, 3].into_iter().collect();
    let quality_tags: HashMap<i32, EpisodeQualityTag> = [web_tag(1)].into_iter().collect();

    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &cutoff,
        &quality_tags,
    );
    assert!(
        decision.reject_reason.is_none(),
        "expected acceptance when all covered episodes actionable; got: {:?}",
        decision.reject_reason
    );
    assert_eq!(decision.new_episode_count, 3);
}

#[test]
fn finished_batch_no_range_with_disk_content_is_rejected() {
    // Series is finished, item is a batch with no parsed range
    // (parsed_eps empty). Existing episodes on disk. Should
    // reject because we can't verify overwrite safety without a
    // range to check per-episode.
    let cutoff = bluray_cutoff();
    let incoming = classification(Source::BluRay, Resolution::R1080p, false, false);
    let found = series("FINISHED");
    let item = item_with("[Group] Test Series Season 1 (BD 1080p)", true);
    let disk = vec![disk_file(1, "WEB-1080p"), disk_file(2, "WEB-1080p")];
    let parsed_eps: HashSet<i32> = HashSet::new();
    let quality_tags: HashMap<i32, EpisodeQualityTag> = HashMap::new();

    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &cutoff,
        &quality_tags,
    );
    assert!(
        decision.reject_reason.is_some(),
        "expected rejection for finished-series batch with existing disk content"
    );
    let reason = decision.reject_reason.unwrap();
    assert!(
        reason.contains("episode range is unknown"),
        "reject reason should mention unknown range: {reason}"
    );
}

#[test]
fn finished_batch_no_range_with_empty_disk_is_accepted() {
    // Same as above but empty disk — this is the intentional
    // BD-batch convenience path for fresh adds. Should accept.
    let cutoff = bluray_cutoff();
    let incoming = classification(Source::BluRay, Resolution::R1080p, false, false);
    let found = series("FINISHED");
    let item = item_with("[Group] Test Series Season 1 (BD 1080p)", true);
    let disk: Vec<media::EpisodeFile> = Vec::new();
    let parsed_eps: HashSet<i32> = HashSet::new();
    let quality_tags: HashMap<i32, EpisodeQualityTag> = HashMap::new();

    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &cutoff,
        &quality_tags,
    );
    assert!(
        decision.reject_reason.is_none(),
        "expected acceptance for finished-series fresh-add batch; got: {:?}",
        decision.reject_reason
    );
}

#[test]
fn airing_batch_no_range_is_rejected() {
    // Airing series + batch without parsed range = not enough
    // signal to grab safely. The is_finished_status branch
    // doesn't fire so this hits the "batch doesn't include
    // monitored episodes" reject.
    let cutoff = bluray_cutoff();
    let incoming = classification(Source::Web, Resolution::R1080p, false, false);
    let found = series("RELEASING");
    let item = item_with("[Group] Test Series Season 1 (WEB 1080p)", true);
    let disk: Vec<media::EpisodeFile> = Vec::new();
    let parsed_eps: HashSet<i32> = HashSet::new();
    let quality_tags: HashMap<i32, EpisodeQualityTag> = HashMap::new();

    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &cutoff,
        &quality_tags,
    );
    assert!(decision.reject_reason.is_some());
}
