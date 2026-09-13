//! The upgrade decision: is a candidate release allowed to replace the
//! file already on disk for an episode?
//!
//! One function, [`judge_upgrade`], mirrors the parts of Sonarr's
//! decision engine that look at the existing file:
//! `UpgradableSpecification.IsUpgradable` (quality, then revision, then
//! Custom Format score, each gated by its cutoff), the RSS-only
//! `ProperSpecification` (no revision upgrade for a file older than a
//! week), and `AnimeVersionUpgradeSpecification` (a revision must come
//! from the release group that made the file on disk). Ryokan's own
//! "no non-BDMV to BDMV crossing" rule from [`super::is_valid_upgrade`]
//! stays on top.
//!
//! Every automatic path that can replace a file goes through here: the
//! RSS gate (`rss::episode_is_upgradeable`), the upgrade sweep
//! (`services::upgrade`), and the manual-import preview's
//! would-replace outcome. Import itself trusts the grab that reached
//! it, the way it always has.

use super::{ClassificationResult, Resolution, Source};
use crate::models::config::Config;
use crate::services::media::ReleaseRevision;

/// Sonarr's "Propers and Repacks" setting, `config.proper_policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProperPolicy {
    /// Score a newer revision above the plain release and replace a
    /// file on disk with its revision. The default.
    #[default]
    PreferAndUpgrade,
    /// Score it higher when both are on offer, but never replace a
    /// file that is already on disk.
    DoNotUpgrade,
    /// Ignore revisions entirely: no score, no upgrade, and a v1
    /// arriving after a v2 is not rejected as a downgrade either.
    DoNotPrefer,
}

impl ProperPolicy {
    pub fn from_str(value: &str) -> Self {
        match value.trim() {
            "do_not_upgrade" => Self::DoNotUpgrade,
            "do_not_prefer" => Self::DoNotPrefer,
            _ => Self::PreferAndUpgrade,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreferAndUpgrade => "prefer_and_upgrade",
            Self::DoNotUpgrade => "do_not_upgrade",
            Self::DoNotPrefer => "do_not_prefer",
        }
    }

    /// Whether a newer revision counts for anything at all.
    pub fn prefers_revisions(self) -> bool {
        self != Self::DoNotPrefer
    }
}

/// Sonarr's `ProperSpecification` window: a revision arriving through
/// RSS replaces a file only while the file is this young. The search
/// paths (upgrade sweep, interactive) are not subject to it.
pub const PROPER_MAX_AGE_DAYS: i64 = 7;

/// The user's upgrade settings, read once per sweep or sync.
#[derive(Debug, Clone)]
pub struct UpgradePolicy {
    /// Quality cutoff: a file at or above this rank is only replaced by
    /// a revision or a better Custom Format score, never by quality.
    pub cutoff: ClassificationResult,
    pub propers: ProperPolicy,
    /// "Upgrade until Custom Format score": a file whose score reaches
    /// this is not replaced for score. Sonarr's `CutoffFormatScore`.
    pub format_cutoff_score: i32,
    /// The least a candidate's score has to gain over the file on disk
    /// for a score-only upgrade. Sonarr's `MinUpgradeFormatScore`.
    pub format_increment: i32,
}

impl UpgradePolicy {
    pub fn from_config(cfg: &Config) -> Self {
        let (cutoff_source, cutoff_is_remux, cutoff_is_bdmv) =
            super::parse_cutoff_source(&cfg.cutoff_source);
        Self {
            cutoff: super::cutoff_classification(
                cutoff_source,
                Resolution::from_str(&cfg.cutoff_resolution),
                cutoff_is_remux,
                cutoff_is_bdmv,
            ),
            propers: ProperPolicy::from_str(&cfg.proper_policy),
            format_cutoff_score: cfg.custom_format_cutoff_score,
            format_increment: cfg.custom_format_upgrade_increment,
        }
    }

    /// True when no quality cutoff is configured at all, which the
    /// upgrade sweep treats as "do not run".
    pub fn has_quality_cutoff(&self) -> bool {
        self.cutoff.source != Source::Unknown || self.cutoff.resolution != Resolution::Unknown
    }
}

/// What is on disk for the episode, as far as the tag row and the
/// file itself can say.
#[derive(Debug, Clone, Copy)]
pub struct ExistingFile<'a> {
    pub classification: &'a ClassificationResult,
    /// Parsed from the tag row's release title (the Nyaa title of the
    /// grab), never from the renamed library file.
    pub revision: ReleaseRevision,
    pub release_group: &'a str,
    /// Custom Format total for the release that produced the file.
    pub cf_score: i32,
    /// Age of the file for the RSS revision window. `None` when the
    /// caller has no file date (the manual-import preview).
    pub age_days: Option<i64>,
}

/// The release under consideration.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeCandidate<'a> {
    pub classification: &'a ClassificationResult,
    pub revision: ReleaseRevision,
    pub release_group: &'a str,
    pub cf_score: i32,
}

/// Why a candidate is an upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeKind {
    /// Better rank tuple while the file on disk is below the cutoff.
    Quality,
    /// Same quality, newer revision from the same group.
    Revision,
    /// Same quality, better Custom Format score.
    FormatScore,
}

impl UpgradeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quality => "quality",
            Self::Revision => "revision",
            Self::FormatScore => "custom format score",
        }
    }
}

/// Why a candidate is not an upgrade. `as_str` is the user-facing
/// reason, written into RSS decisions and upgrade-sweep debug lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeRejection {
    ExistingUnclassified,
    BdmvCrossing,
    LowerQuality,
    CutoffMet,
    RevisionUpgradesDisabled,
    RevisionGroupUnknown,
    RevisionGroupMismatch,
    RevisionForOldFile,
    LowerRevision,
    FormatScoreNotHigher,
    FormatCutoffMet,
    FormatIncrementNotMet,
}

impl UpgradeRejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExistingUnclassified => "Existing file quality is unknown",
            Self::BdmvCrossing => {
                "Existing file is not a BDMV release; BDMV upgrades are manual only"
            }
            Self::LowerQuality => "Existing file is higher quality",
            Self::CutoffMet => "Existing file meets the quality cutoff",
            Self::RevisionUpgradesDisabled => "Revision upgrades are disabled",
            Self::RevisionGroupUnknown => {
                "Release group is unknown; a revision must come from the group that made the file"
            }
            Self::RevisionGroupMismatch => "Revision is from a different release group",
            Self::RevisionForOldFile => "Revision for a file older than 7 days",
            Self::LowerRevision => "Existing file is a newer revision",
            Self::FormatScoreNotHigher => {
                "Custom Format score does not improve on the existing file"
            }
            Self::FormatCutoffMet => "Existing file meets the Custom Format cutoff",
            Self::FormatIncrementNotMet => {
                "Custom Format score gain is below the minimum increment"
            }
        }
    }
}

impl std::fmt::Display for UpgradeRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Decide whether `candidate` may replace `existing`.
///
/// The order is Sonarr's, read through `UpgradeDiskSpecification`:
///
/// 1. A file the classifier could not place is never replaced
///    automatically.
/// 2. Higher quality: allowed while the file is below the cutoff and
///    the candidate is not a BDMV crossing; rejected as "cutoff met"
///    otherwise, even when the Custom Format cutoff is unmet (only a
///    same-quality release can satisfy that one).
/// 3. Lower quality: rejected.
/// 4. Same quality, newer revision (with the proper policy on):
///    allowed when it comes from the same release group as the file,
///    the policy permits upgrades, and, through RSS, the file is not
///    older than [`PROPER_MAX_AGE_DAYS`]. The revision bypasses both
///    cutoffs.
/// 5. Same quality, older revision: rejected.
/// 6. Same quality: the Custom Format score has to be higher, below
///    the format cutoff on the existing side, and higher by at least
///    the configured increment.
///
/// `from_rss` turns on the age window. `existing.age_days` of `None`
/// never trips it.
pub fn judge_upgrade(
    policy: &UpgradePolicy,
    existing: &ExistingFile<'_>,
    candidate: &UpgradeCandidate<'_>,
    from_rss: bool,
) -> Result<UpgradeKind, UpgradeRejection> {
    let existing_cls = existing.classification;
    let candidate_cls = candidate.classification;
    if existing_cls.source == Source::Unknown && existing_cls.resolution == Resolution::Unknown {
        return Err(UpgradeRejection::ExistingUnclassified);
    }
    let existing_rank = existing_cls.rank();
    let candidate_rank = candidate_cls.rank();
    match candidate_rank.cmp(&existing_rank) {
        std::cmp::Ordering::Greater => {
            if !existing_cls.is_bdmv && candidate_cls.is_bdmv {
                return Err(UpgradeRejection::BdmvCrossing);
            }
            if existing_rank < policy.cutoff.rank() {
                Ok(UpgradeKind::Quality)
            } else {
                Err(UpgradeRejection::CutoffMet)
            }
        }
        std::cmp::Ordering::Less => Err(UpgradeRejection::LowerQuality),
        std::cmp::Ordering::Equal => {
            let prefer = policy.propers.prefers_revisions();
            if prefer && candidate.revision.is_newer_than(existing.revision) {
                if policy.propers == ProperPolicy::DoNotUpgrade {
                    return Err(UpgradeRejection::RevisionUpgradesDisabled);
                }
                let existing_group = existing.release_group.trim();
                let candidate_group = candidate.release_group.trim();
                if existing_group.is_empty() || candidate_group.is_empty() {
                    return Err(UpgradeRejection::RevisionGroupUnknown);
                }
                if !existing_group.eq_ignore_ascii_case(candidate_group) {
                    return Err(UpgradeRejection::RevisionGroupMismatch);
                }
                if from_rss && existing.age_days.is_some_and(|d| d > PROPER_MAX_AGE_DAYS) {
                    return Err(UpgradeRejection::RevisionForOldFile);
                }
                return Ok(UpgradeKind::Revision);
            }
            if prefer && existing.revision.is_newer_than(candidate.revision) {
                return Err(UpgradeRejection::LowerRevision);
            }
            if candidate.cf_score <= existing.cf_score {
                return Err(UpgradeRejection::FormatScoreNotHigher);
            }
            if existing.cf_score >= policy.format_cutoff_score {
                return Err(UpgradeRejection::FormatCutoffMet);
            }
            if candidate.cf_score < existing.cf_score.saturating_add(policy.format_increment) {
                return Err(UpgradeRejection::FormatIncrementNotMet);
            }
            Ok(UpgradeKind::FormatScore)
        }
    }
}

/// Same quality, newer revision, no group or age rule. The
/// manual-import preview's would-replace check, where the user sees
/// the outcome before anything moves.
pub fn is_revision_upgrade(
    existing: &ClassificationResult,
    existing_title: &str,
    incoming: &ClassificationResult,
    incoming_title: &str,
) -> bool {
    existing.rank() == incoming.rank()
        && crate::services::media::parse_release_revision(incoming_title).is_newer_than(
            crate::services::media::parse_release_revision(existing_title),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::source::{DecisionRule, WebKind};

    fn cls(source: Source, res: Resolution, is_remux: bool, is_bdmv: bool) -> ClassificationResult {
        ClassificationResult {
            source,
            resolution: res,
            is_remux,
            web_kind: WebKind::Unknown,
            is_bdmv,
            confidence: 1.0,
            needs_review: false,
            evidence: Vec::new(),
            decision_rule: DecisionRule::Empty,
        }
    }

    fn rev(version: u32) -> ReleaseRevision {
        ReleaseRevision {
            version,
            repack: false,
        }
    }

    fn policy(cutoff: ClassificationResult) -> UpgradePolicy {
        UpgradePolicy {
            cutoff,
            propers: ProperPolicy::PreferAndUpgrade,
            format_cutoff_score: 0,
            format_increment: 1,
        }
    }

    fn bd1080() -> ClassificationResult {
        cls(Source::BluRay, Resolution::R1080p, false, false)
    }

    fn web1080() -> ClassificationResult {
        cls(Source::Web, Resolution::R1080p, false, false)
    }

    fn existing<'a>(c: &'a ClassificationResult, version: u32, group: &'a str) -> ExistingFile<'a> {
        ExistingFile {
            classification: c,
            revision: rev(version),
            release_group: group,
            cf_score: 0,
            age_days: Some(1),
        }
    }

    fn candidate<'a>(
        c: &'a ClassificationResult,
        version: u32,
        group: &'a str,
    ) -> UpgradeCandidate<'a> {
        UpgradeCandidate {
            classification: c,
            revision: rev(version),
            release_group: group,
            cf_score: 0,
        }
    }

    #[test]
    fn quality_upgrade_below_cutoff_is_allowed() {
        let p = policy(bd1080());
        let (e, c) = (web1080(), bd1080());
        assert_eq!(
            judge_upgrade(&p, &existing(&e, 1, "A"), &candidate(&c, 1, "B"), false),
            Ok(UpgradeKind::Quality)
        );
    }

    #[test]
    fn quality_upgrade_at_cutoff_is_cutoff_met() {
        // Cutoff WEB-1080p, file WEB-1080p, candidate BD-1080p: Sonarr's
        // "existing file meets quality cutoff".
        let p = policy(web1080());
        let (e, c) = (web1080(), bd1080());
        assert_eq!(
            judge_upgrade(&p, &existing(&e, 1, "A"), &candidate(&c, 1, "B"), false),
            Err(UpgradeRejection::CutoffMet)
        );
    }

    #[test]
    fn lower_quality_is_rejected_even_as_a_revision() {
        let p = policy(bd1080());
        let (e, c) = (bd1080(), web1080());
        assert_eq!(
            judge_upgrade(&p, &existing(&e, 1, "A"), &candidate(&c, 2, "A"), false),
            Err(UpgradeRejection::LowerQuality)
        );
    }

    #[test]
    fn bdmv_crossing_stays_blocked() {
        let p = policy(cls(Source::BluRay, Resolution::R1080p, false, true));
        let e = cls(Source::BluRay, Resolution::R1080p, true, false);
        let c = cls(Source::BluRay, Resolution::R1080p, false, true);
        assert_eq!(
            judge_upgrade(&p, &existing(&e, 1, "A"), &candidate(&c, 1, "A"), false),
            Err(UpgradeRejection::BdmvCrossing)
        );
    }

    #[test]
    fn revision_from_same_group_upgrades_past_the_cutoff() {
        // File already meets the cutoff; a v2 from the same group still
        // replaces it (Sonarr: a revision upgrade bypasses cutoff-met).
        let p = policy(web1080());
        let (e, c) = (web1080(), web1080());
        assert_eq!(
            judge_upgrade(
                &p,
                &existing(&e, 1, "SubsPlease"),
                &candidate(&c, 2, "subsplease"),
                true
            ),
            Ok(UpgradeKind::Revision)
        );
    }

    #[test]
    fn revision_from_another_group_is_rejected() {
        let p = policy(web1080());
        let (e, c) = (web1080(), web1080());
        assert_eq!(
            judge_upgrade(
                &p,
                &existing(&e, 1, "SubsPlease"),
                &candidate(&c, 2, "Erai-raws"),
                true
            ),
            Err(UpgradeRejection::RevisionGroupMismatch)
        );
    }

    #[test]
    fn revision_with_unknown_group_on_either_side_is_rejected() {
        let p = policy(web1080());
        let (e, c) = (web1080(), web1080());
        assert_eq!(
            judge_upgrade(
                &p,
                &existing(&e, 1, ""),
                &candidate(&c, 2, "SubsPlease"),
                true
            ),
            Err(UpgradeRejection::RevisionGroupUnknown)
        );
        assert_eq!(
            judge_upgrade(
                &p,
                &existing(&e, 1, "SubsPlease"),
                &candidate(&c, 2, " "),
                true
            ),
            Err(UpgradeRejection::RevisionGroupUnknown)
        );
    }

    #[test]
    fn rss_revision_for_an_old_file_is_rejected_but_a_search_takes_it() {
        let p = policy(web1080());
        let (e, c) = (web1080(), web1080());
        let mut old = existing(&e, 1, "G");
        old.age_days = Some(PROPER_MAX_AGE_DAYS + 1);
        assert_eq!(
            judge_upgrade(&p, &old, &candidate(&c, 2, "G"), true),
            Err(UpgradeRejection::RevisionForOldFile)
        );
        assert_eq!(
            judge_upgrade(&p, &old, &candidate(&c, 2, "G"), false),
            Ok(UpgradeKind::Revision)
        );
        // Exactly seven days is still inside the window.
        old.age_days = Some(PROPER_MAX_AGE_DAYS);
        assert_eq!(
            judge_upgrade(&p, &old, &candidate(&c, 2, "G"), true),
            Ok(UpgradeKind::Revision)
        );
    }

    #[test]
    fn do_not_upgrade_policy_scores_but_never_replaces() {
        let mut p = policy(web1080());
        p.propers = ProperPolicy::DoNotUpgrade;
        let (e, c) = (web1080(), web1080());
        assert_eq!(
            judge_upgrade(&p, &existing(&e, 1, "G"), &candidate(&c, 2, "G"), false),
            Err(UpgradeRejection::RevisionUpgradesDisabled)
        );
    }

    #[test]
    fn do_not_prefer_policy_ignores_revisions_both_ways() {
        let mut p = policy(web1080());
        p.propers = ProperPolicy::DoNotPrefer;
        let (e, c) = (web1080(), web1080());
        // A v2 is not an upgrade ...
        assert_eq!(
            judge_upgrade(&p, &existing(&e, 1, "G"), &candidate(&c, 2, "G"), false),
            Err(UpgradeRejection::FormatScoreNotHigher)
        );
        // ... and a v1 after a v2 is not rejected as a downgrade; it
        // falls through to the score compare like any same-quality file.
        let mut better = candidate(&c, 1, "G");
        better.cf_score = 50;
        let mut on_disk = existing(&e, 2, "G");
        on_disk.cf_score = 10;
        p.format_cutoff_score = 100;
        assert_eq!(
            judge_upgrade(&p, &on_disk, &better, false),
            Ok(UpgradeKind::FormatScore)
        );
    }

    #[test]
    fn older_revision_is_a_downgrade() {
        let p = policy(web1080());
        let (e, c) = (web1080(), web1080());
        assert_eq!(
            judge_upgrade(&p, &existing(&e, 2, "G"), &candidate(&c, 1, "G"), false),
            Err(UpgradeRejection::LowerRevision)
        );
    }

    #[test]
    fn format_score_upgrade_needs_higher_score_below_cutoff_and_the_increment() {
        let mut p = policy(web1080());
        p.format_cutoff_score = 100;
        p.format_increment = 20;
        let (e, c) = (web1080(), web1080());
        let mut on_disk = existing(&e, 1, "A");
        on_disk.cf_score = 10;
        let mut cand = candidate(&c, 1, "B");
        // Equal score: not higher.
        cand.cf_score = 10;
        assert_eq!(
            judge_upgrade(&p, &on_disk, &cand, false),
            Err(UpgradeRejection::FormatScoreNotHigher)
        );
        // Higher but under the increment.
        cand.cf_score = 25;
        assert_eq!(
            judge_upgrade(&p, &on_disk, &cand, false),
            Err(UpgradeRejection::FormatIncrementNotMet)
        );
        cand.cf_score = 30;
        assert_eq!(
            judge_upgrade(&p, &on_disk, &cand, false),
            Ok(UpgradeKind::FormatScore)
        );
        // Existing at the format cutoff: no score upgrade at all.
        on_disk.cf_score = 100;
        cand.cf_score = 500;
        assert_eq!(
            judge_upgrade(&p, &on_disk, &cand, false),
            Err(UpgradeRejection::FormatCutoffMet)
        );
    }

    #[test]
    fn default_format_cutoff_of_zero_means_no_score_upgrades_for_a_clean_file() {
        // Sonarr's default CutoffFormatScore is 0: a file scoring 0
        // already meets it. A negative score is below it and can be
        // replaced by a better one.
        let p = policy(web1080());
        let (e, c) = (web1080(), web1080());
        let mut on_disk = existing(&e, 1, "A");
        let mut cand = candidate(&c, 1, "B");
        cand.cf_score = 100;
        assert_eq!(
            judge_upgrade(&p, &on_disk, &cand, false),
            Err(UpgradeRejection::FormatCutoffMet)
        );
        on_disk.cf_score = -50;
        cand.cf_score = 0;
        assert_eq!(
            judge_upgrade(&p, &on_disk, &cand, false),
            Ok(UpgradeKind::FormatScore)
        );
    }

    #[test]
    fn unclassified_existing_is_never_replaced() {
        let p = policy(web1080());
        let e = ClassificationResult::unknown();
        let c = bd1080();
        assert_eq!(
            judge_upgrade(&p, &existing(&e, 1, "A"), &candidate(&c, 1, "B"), false),
            Err(UpgradeRejection::ExistingUnclassified)
        );
    }

    #[test]
    fn proper_policy_round_trips() {
        for p in [
            ProperPolicy::PreferAndUpgrade,
            ProperPolicy::DoNotUpgrade,
            ProperPolicy::DoNotPrefer,
        ] {
            assert_eq!(ProperPolicy::from_str(p.as_str()), p);
        }
        assert_eq!(
            ProperPolicy::from_str("garbage"),
            ProperPolicy::PreferAndUpgrade
        );
    }
}
