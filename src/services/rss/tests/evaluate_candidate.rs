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
        alternate_titles: String::new(),
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

fn bluray_policy() -> source::UpgradePolicy {
    source::UpgradePolicy {
        cutoff: bluray_cutoff(),
        propers: source::ProperPolicy::PreferAndUpgrade,
        format_cutoff_score: 0,
        format_increment: 1,
    }
}

/// Gate for an item with no Custom Formats in play: the item's own
/// revision and group, a zero CF total, and no clock (file ages read
/// as unknown).
fn gate<'a>(
    policy: &'a source::UpgradePolicy,
    seadex: &'a HashSet<String>,
    item: &'a RssItem,
) -> UpgradeGate<'a> {
    UpgradeGate {
        policy,
        cfs: &[],
        seadex_hashes: seadex,
        now_secs: 0,
        incoming_revision: media::parse_release_revision(&item.title),
        incoming_group: &item.group,
        incoming_cf_score: 0,
    }
}

fn disk_file(ep: i32, quality: &str) -> media::EpisodeFile {
    media::EpisodeFile {
        filename: format!("Test Series - S01E{:02}.mkv", ep),
        episode_number: ep,
        episode_last: ep,
        season_number: Some(1),
        quality: quality.to_string(),
        size_bytes: 1_000_000_000,
        size_display: String::new(),
        modified_secs: None,
        is_special: false,
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
    let policy = bluray_policy();
    let seadex: HashSet<String> = HashSet::new();
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
        &gate(&policy, &seadex, &item),
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
    let policy = bluray_policy();
    let seadex: HashSet<String> = HashSet::new();
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
        &gate(&policy, &seadex, &item),
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
    let policy = bluray_policy();
    let seadex: HashSet<String> = HashSet::new();
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
        &gate(&policy, &seadex, &item),
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
    let policy = bluray_policy();
    let seadex: HashSet<String> = HashSet::new();
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
        &gate(&policy, &seadex, &item),
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
    let policy = bluray_policy();
    let seadex: HashSet<String> = HashSet::new();
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
        &gate(&policy, &seadex, &item),
        &quality_tags,
    );
    assert!(decision.reject_reason.is_some());
}

// ── Release revisions (Sonarr's propers / anime `v2`) ────────────────

fn v2_fixture(
    existing_group: &str,
    incoming_group: &str,
) -> (
    series::Series,
    RssItem,
    ClassificationResult,
    Vec<media::EpisodeFile>,
    HashSet<i32>,
    HashMap<i32, EpisodeQualityTag>,
) {
    let found = series("RELEASING");
    let mut item = item_with(
        &format!("[{incoming_group}] Test Series - 01v2 (1080p) [ABCD1234].mkv"),
        false,
    );
    item.group = incoming_group.to_string();
    let incoming = classification(Source::Web, Resolution::R1080p, false, false);
    let disk = vec![disk_file(1, "WEB-1080p")];
    let parsed_eps: HashSet<i32> = [1].into_iter().collect();
    let (ep, mut tag) = web_tag(1);
    tag.state = "completed".to_string();
    tag.release_title = format!("[{existing_group}] Test Series - 01 (1080p) [ABCD1234].mkv");
    tag.release_group = existing_group.to_string();
    let quality_tags: HashMap<i32, EpisodeQualityTag> = [(ep, tag)].into_iter().collect();
    (found, item, incoming, disk, parsed_eps, quality_tags)
}

#[test]
fn v2_from_the_same_group_replaces_the_file_past_the_cutoff() {
    // Cutoff WEB-1080p is met by the file on disk; the same group's
    // v2 still replaces it (a revision upgrade bypasses cutoff-met).
    let policy = source::UpgradePolicy {
        cutoff: source::cutoff_classification(Source::Web, Resolution::R1080p, false, false),
        ..bluray_policy()
    };
    let seadex: HashSet<String> = HashSet::new();
    let (found, item, incoming, disk, parsed_eps, quality_tags) =
        v2_fixture("SubsPlease", "SubsPlease");
    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &gate(&policy, &seadex, &item),
        &quality_tags,
    );
    assert_eq!(decision.reject_reason, None);
    assert!(decision.is_upgrade);
    assert_eq!(decision.new_episode_count, 1);
}

#[test]
fn v2_from_another_group_is_rejected_with_the_gate_reason() {
    let policy = bluray_policy();
    let seadex: HashSet<String> = HashSet::new();
    let (found, item, incoming, disk, parsed_eps, quality_tags) =
        v2_fixture("SubsPlease", "Erai-raws");
    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &gate(&policy, &seadex, &item),
        &quality_tags,
    );
    let reason = decision.reject_reason.expect("rejected");
    assert!(
        reason.contains("different release group"),
        "reason should name the group rule: {reason}"
    );
}

#[test]
fn v2_is_no_upgrade_when_revisions_are_not_preferred() {
    let policy = source::UpgradePolicy {
        propers: source::ProperPolicy::DoNotPrefer,
        ..bluray_policy()
    };
    let seadex: HashSet<String> = HashSet::new();
    let (found, item, incoming, disk, parsed_eps, quality_tags) =
        v2_fixture("SubsPlease", "SubsPlease");
    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &gate(&policy, &seadex, &item),
        &quality_tags,
    );
    let reason = decision.reject_reason.expect("rejected");
    assert!(
        reason.contains("Custom Format score does not improve"),
        "falls through to the score compare: {reason}"
    );
}

#[test]
fn v2_for_an_old_file_is_rejected_through_rss() {
    let policy = bluray_policy();
    let seadex: HashSet<String> = HashSet::new();
    let (found, item, incoming, mut disk, parsed_eps, quality_tags) =
        v2_fixture("SubsPlease", "SubsPlease");
    // File landed 30 days before "now".
    disk[0].modified_secs = Some(0);
    let mut g = gate(&policy, &seadex, &item);
    g.now_secs = 30 * 86_400;
    let decision = evaluate_candidate(
        &found,
        &item,
        &incoming,
        &disk,
        &parsed_eps,
        &g,
        &quality_tags,
    );
    let reason = decision.reject_reason.expect("rejected");
    assert!(reason.contains("older than 7 days"), "{reason}");
}
