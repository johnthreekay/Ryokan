//! Custom Format + SeaDex overlay + auto-search rescoring.
//!
//! Three entry points, all `pub(super)` because mod.rs's orchestration
//! code is the sole caller:
//!
//! - [`apply_cf_seadex_overlay`] — takes a base score, applies the CF
//!   and SeaDex contributions, emits one tracing::debug! per candidate,
//!   and returns `Some(final)` or `None` if below the floor.
//! - [`format_scoring_detail`] — stable log-line format for the breakdown.
//! - [`rescore_for_auto_search`] — the base-score computation itself.

use std::collections::HashSet;

use crate::models::config::Config;
use crate::services::custom_formats::{self, CompiledCustomFormat, EvalContext};
use crate::services::nyaa::SearchResult;
use crate::services::quality;
use crate::services::scoring::ScoreComponent;
use crate::services::seadex;
use crate::services::source::{self, ClassificationResult, Resolution, Source};

use super::{
    MatchKind, SearchTarget, distinctive_overlap_ratio, normalize_title, parse_release_numbers,
    season_mismatch, token_set,
};

/// Misgrab guardrails: a fuzzy title match loses to an otherwise-equal
/// verbatim one, and so does anything the fallback passes (extended
/// aliases, franchise roots) surfaced. Sized to beat the popularity
/// tiebreakers (seeder tiers spread 20, trusted 10) and lose to every
/// quality signal (source step 40, resolution step 60, preferred group
/// 30 per rank, episode match 40), so a fuzzy 1080p still outranks a
/// verbatim 720p and a fuzzy-only pool still grabs. The CF floor only
/// looks at the CF subtotal, so these can never drop a candidate.
pub(super) const FUZZY_MATCH_PENALTY: i32 = -25;
pub(super) const FALLBACK_PHASE_PENALTY: i32 = -10;

/// Apply the Custom Format + SeaDex overlay to a base score.
///
/// Returns `Some(final_score)` if the candidate survives the CF
/// minimum-score floor, or `None` if it should be dropped. The SeaDex
/// score bump is suppressed whenever the compiled CF set contains a
/// `SeaDexBestSpecification` — the user has taken ownership of that
/// number and double-counting would be a silent regression.
///
/// On the way through, emits one tracing::debug! line per candidate with
/// a CF-aware breakdown (plan §6.3). Operators who want to introspect
/// "why did X win / Y lose" can set
/// `RUST_LOG=ryokan::auto_search::scoring=debug`. The previous code
/// wrote to the DB log table here, but at 50-200 candidates per search
/// that was a sustained INSERT stream the `logs` UI flooded with rather
/// than aided.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_cf_seadex_overlay(
    base: i32,
    result: &SearchResult,
    classification: &ClassificationResult,
    cfs: &[CompiledCustomFormat],
    seadex_hashes: &HashSet<String>,
    seadex_boost_enabled: bool,
    minimum_score: i32,
) -> Option<i32> {
    apply_cf_seadex_overlay_with_breakdown(
        base,
        result,
        classification,
        cfs,
        seadex_hashes,
        seadex_boost_enabled,
        minimum_score,
    )
    .map(|(score, _)| score)
}

/// Same as [`apply_cf_seadex_overlay`] but also returns the per-CF and
/// SeaDex breakdown entries so the caller can fold them into the
/// `SearchResult`'s `score_breakdown` for UI display. Used by the
/// interactive search path where each candidate's breakdown needs to
/// stay in sync with its final score.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_cf_seadex_overlay_with_breakdown(
    base: i32,
    result: &SearchResult,
    classification: &ClassificationResult,
    cfs: &[CompiledCustomFormat],
    seadex_hashes: &HashSet<String>,
    seadex_boost_enabled: bool,
    minimum_score: i32,
) -> Option<(i32, Vec<ScoreComponent>)> {
    let ctx = EvalContext {
        result,
        classification,
        seadex_hashes,
    };
    // Use the breakdown variant so the log line can name which CFs
    // contributed. Per plan §6.3, production scoring normally uses the
    // scalar `total_cf_score`; the cost of the `Vec<(String, i32)>`
    // allocation is absorbed here because we're about to log anyway.
    let (cf, breakdown) = custom_formats::total_cf_score_with_breakdown(cfs, &ctx);
    let seadex_bonus = if seadex_boost_enabled
        && !result.info_hash.is_empty()
        && seadex_hashes.contains(&result.info_hash.to_ascii_lowercase())
    {
        seadex::SEADEX_SCORE_BOOST
    } else {
        0
    };

    let below_floor = cf < minimum_score;
    // saturating_add at the combine — base, cf, and seadex_bonus are
    // each i32 and any one of them can be ±10k+. With ~22 CFs all
    // matching positively plus the 10k SeaDex boost plus base, naive
    // `+` can wrap to a large negative and silently demote every
    // candidate below minimum_score.
    let final_score = base.saturating_add(cf).saturating_add(seadex_bonus);

    let detail =
        format_scoring_detail(base, cf, &breakdown, seadex_bonus, final_score, below_floor);
    // tracing::debug! instead of logger::debug — 50-200 candidates per
    // search × one debug row each meant a sustained INSERT stream into
    // the `logs` table on every auto-search. Terminal/container logs
    // are the right surface for this granularity of detail; operators
    // who want it can set RUST_LOG=ryokan::auto_search=debug.
    tracing::debug!(
        target: "ryokan::auto_search::scoring",
        title = %result.title,
        "{}",
        detail
    );

    if below_floor {
        return None;
    }

    let mut components: Vec<ScoreComponent> = breakdown
        .into_iter()
        .map(|(name, delta)| ScoreComponent {
            label: format!("CF: {name}"),
            delta,
            detail: None,
        })
        .collect();
    if seadex_bonus != 0 {
        components.push(ScoreComponent {
            label: "SeaDex Best".to_string(),
            delta: seadex_bonus,
            detail: Some("release flagged isBest by releases.moe".to_string()),
        });
    }

    Some((final_score, components))
}

/// Build the structured scoring detail string that lands in the
/// `logs.detail` column. Factored out of `apply_cf_seadex_overlay` so
/// the format is in one place and unit-testable. Matches the shape
/// documented in plan §6.3:
///
/// `base=85, cf=+420 [10bit x265 +200, FLAC audio +120, Preferred Groups: MTBB +100], seadex=0, final=505`
///
/// Negative contributions include the sign. An empty breakdown drops
/// the bracket section entirely ("cf=+0" with nothing inside would be
/// noisy). Candidates dropped by the CF minimum-score floor get a
/// trailing ` DROPPED(floor=N)` marker so log readers can tell filtered
/// candidates apart from surviving ones.
fn format_scoring_detail(
    base: i32,
    cf: i32,
    breakdown: &[(String, i32)],
    seadex_bonus: i32,
    final_score: i32,
    below_floor: bool,
) -> String {
    let cf_section = if breakdown.is_empty() {
        format!("cf={cf:+}")
    } else {
        let parts: Vec<String> = breakdown
            .iter()
            .map(|(name, score)| format!("{name} {score:+}"))
            .collect();
        format!("cf={:+} [{}]", cf, parts.join(", "))
    };
    let mut out = format!("base={base}, {cf_section}, seadex={seadex_bonus}, final={final_score}");
    if below_floor {
        out.push_str(" DROPPED(below minimum_score floor)");
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(super) fn rescore_for_auto_search(
    result: &SearchResult,
    classification: &ClassificationResult,
    config: &Config,
    aliases: &[String],
    target: &SearchTarget,
    expected_season: i32,
    is_finished: bool,
    finished_mode: quality::FinishedSeriesMode,
    preferred_source: Source,
    preferred_resolution: Resolution,
    cutoff_source: Source,
    cutoff_resolution: Resolution,
    absolute_offset: i32,
) -> i32 {
    rescore_for_auto_search_with_breakdown(
        result,
        classification,
        config,
        aliases,
        target,
        expected_season,
        is_finished,
        finished_mode,
        preferred_source,
        preferred_resolution,
        cutoff_source,
        cutoff_resolution,
        absolute_offset,
        false, // batch_search_mode — non-batch callers
    )
    .0
}

/// Same as [`rescore_for_auto_search`] but also returns the list of
/// score components added on top of the scraper's base score. Used by
/// the interactive search path so each candidate's breakdown in the UI
/// stays in sync with its final displayed score.
///
/// `batch_search_mode` is `true` when the caller is explicitly
/// collecting batch-only candidates (`collect_scored_batches_for_
/// target`, powering both interactive batch search and auto-search's
/// batch grab path). In that mode the single-target batch penalty is
/// suppressed — penalizing a batch for being a batch when the user
/// explicitly asked for batches is nonsense, and surfaced in the
/// breakdown as a confusing "-5 Batch Penalty" on every row.
#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
pub(super) fn rescore_for_auto_search_with_breakdown(
    result: &SearchResult,
    classification: &ClassificationResult,
    config: &Config,
    aliases: &[String],
    target: &SearchTarget,
    expected_season: i32,
    is_finished: bool,
    finished_mode: quality::FinishedSeriesMode,
    preferred_source: Source,
    preferred_resolution: Resolution,
    cutoff_source: Source,
    cutoff_resolution: Resolution,
    absolute_offset: i32,
    batch_search_mode: bool,
) -> (i32, Vec<ScoreComponent>) {
    let mut score = result.score;
    let mut parts: Vec<ScoreComponent> = Vec::new();
    let mut add =
        |parts: &mut Vec<ScoreComponent>, label: &str, delta: i32, detail: Option<String>| {
            if delta == 0 {
                return;
            }
            score += delta;
            parts.push(ScoreComponent {
                label: label.to_string(),
                delta,
                detail,
            });
        };
    let lower = result.title.to_lowercase();
    let normalized_title = normalize_title(&result.title);
    let title_tokens = token_set(&normalized_title);

    let best_overlap = aliases
        .iter()
        .map(|alias| {
            let normalized_alias = normalize_title(alias);
            if normalized_title.contains(&normalized_alias) {
                1.0
            } else {
                // Same ratio the title gate uses (#219), so the breakdown
                // and the gate agree on what "matched" means.
                distinctive_overlap_ratio(&title_tokens, &token_set(&normalized_alias))
            }
        })
        .fold(0.0f32, f32::max);
    let overlap_delta = (best_overlap * 40.0) as i32;
    add(
        &mut parts,
        "Title Alias Match",
        overlap_delta,
        Some(format!(
            "{:.0}% of best alias tokens matched",
            best_overlap * 100.0
        )),
    );

    // Misgrab guardrails: how the title matched. Always listed, even at
    // zero delta, so every breakdown says whether the match was
    // verbatim or fuzzy and which pass produced it.
    if let Some(p) = &result.match_provenance {
        let delta = match p.kind {
            MatchKind::Fuzzy => FUZZY_MATCH_PENALTY,
            MatchKind::Verbatim | MatchKind::SeadexCurated => 0,
        } + if p.phase.is_fallback() {
            FALLBACK_PHASE_PENALTY
        } else {
            0
        };
        let detail = Some(p.summary());
        if delta == 0 {
            parts.push(ScoreComponent {
                label: "Title Match Confidence".to_string(),
                delta: 0,
                detail,
            });
        } else {
            add(&mut parts, "Title Match Confidence", delta, detail);
        }
    }

    // Season mismatch penalty (explicit season markers like S03, "3rd Season")
    if season_mismatch(&result.title, expected_season) {
        add(
            &mut parts,
            "Season Mismatch",
            -100,
            Some(format!("release season ≠ expected S{expected_season:02}")),
        );
    }

    match target {
        SearchTarget::Single => {
            // Movie / Special / OVA bonus and Batch penalty both
            // assume the user is looking for a single-unit target.
            // In explicit batch-search mode every candidate is a batch
            // for the same series, so both signals are meaningless
            // and would uniformly lift or lower the whole slate. Gate
            // both on `!batch_search_mode` so batch-grab rankings are
            // driven by quality + alias match + seeders, not by the
            // presence of "Movie" / "OVA" keywords in the batch title.
            if !batch_search_mode
                && (lower.contains("movie") || lower.contains("special") || lower.contains("ova"))
            {
                add(&mut parts, "Movie / Special / OVA", 8, None);
            }
            if result.is_batch && !batch_search_mode {
                add(&mut parts, "Batch Penalty (single target)", -5, None);
            }
        }
        SearchTarget::Episode(ep) => {
            if result.is_batch {
                if !batch_search_mode {
                    // Same gate the SearchTarget::Single arm uses: the
                    // user explicitly asked for batches when batch_search_mode
                    // is on, so penalizing every candidate -20 for being a
                    // batch is uniform across the slate (doesn't change
                    // ranking) but produces a confusing "Batch Penalty
                    // (episode target): single-episode grab preferred"
                    // line on every row in the breakdown. Suppress.
                    add(
                        &mut parts,
                        "Batch Penalty (episode target)",
                        -20,
                        Some("single-episode grab preferred".to_string()),
                    );
                }
            } else {
                add(&mut parts, "Single-Episode Target", 10, None);
            }
            let parsed = parse_release_numbers(&result.title);
            let relative_match = parsed.contains(ep);
            let absolute_match =
                absolute_offset > 0 && parsed.contains(&ep.saturating_add(absolute_offset));
            if relative_match || absolute_match {
                add(
                    &mut parts,
                    "Episode Number Match",
                    40,
                    Some(format!("release covers episode {ep}")),
                );
            } else if absolute_offset > 0 && !parsed.is_empty() {
                // #30 — franchise-alias fallback surfaces candidates
                // whose parsed number matches neither target. Bury them.
                add(
                    &mut parts,
                    "Wrong Episode Number",
                    -1000,
                    Some("franchise-pass release doesn't match target ep".to_string()),
                );
            } else if absolute_offset > 0 && parsed.is_empty() {
                add(
                    &mut parts,
                    "Unparseable Episode Number",
                    -500,
                    Some("franchise-pass release with no parseable ep".to_string()),
                );
            }
        }
    }

    let group_bonus = quality::preferred_group_bonus(
        &result.group,
        &quality::parse_group_list(&config.preferred_groups),
    );
    add(&mut parts, "Preferred Group (auto)", group_bonus, None);

    // Classification-aware quality scoring.
    let classification_delta = source::score_classification(
        classification,
        preferred_source,
        preferred_resolution,
        cutoff_source,
        cutoff_resolution,
    );
    add(
        &mut parts,
        "Source / Resolution Fit",
        classification_delta,
        Some(format!(
            "{} {}",
            classification.source.as_str(),
            classification.resolution.as_str()
        )),
    );

    // For finished series with BD preference, give BD releases a significant boost.
    if is_finished
        && finished_mode == quality::FinishedSeriesMode::PreferBd
        && classification.source == Source::BluRay
    {
        add(
            &mut parts,
            "Finished Series BD Bonus",
            35,
            Some("finished series + prefer_bd + BluRay source".to_string()),
        );
    }

    (score, parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_scoring_detail_matches_plan_docs_example() {
        // Shape documented in plan §6.3 — the scoring log entry should
        // read like this exact format so users grepping the logs can
        // rely on a stable column layout. The CF names below ("10bit
        // x265", "FLAC audio", "Preferred Groups: MTBB") are reproduced
        // verbatim from the plan doc example; they're opaque label
        // strings passed through by the formatter, not claims about
        // any release group's actual naming scheme.
        let breakdown = vec![
            ("10bit x265".to_string(), 200),
            ("FLAC audio".to_string(), 120),
            ("Preferred Groups: MTBB".to_string(), 100),
        ];
        let s = format_scoring_detail(85, 420, &breakdown, 0, 505, false);
        assert_eq!(
            s,
            "base=85, cf=+420 [10bit x265 +200, FLAC audio +120, Preferred Groups: MTBB +100], seadex=0, final=505"
        );
    }
    #[test]
    fn format_scoring_detail_empty_breakdown_drops_bracket_section() {
        // No CFs matched → the bracket section is noise. Just show the
        // scalar cf= total.
        let s = format_scoring_detail(50, 0, &[], 0, 50, false);
        assert_eq!(s, "base=50, cf=+0, seadex=0, final=50");
    }
    #[test]
    fn format_scoring_detail_negative_cf_has_sign_and_marks_drop() {
        let breakdown = vec![("Casual group penalty".to_string(), -1000)];
        let s = format_scoring_detail(20, -1000, &breakdown, 0, -980, true);
        assert_eq!(
            s,
            "base=20, cf=-1000 [Casual group penalty -1000], seadex=0, final=-980 DROPPED(below minimum_score floor)"
        );
    }
    #[test]
    fn format_scoring_detail_surfaces_seadex_bonus() {
        // SeaDex bonus is the only non-CF overlay; make sure it shows
        // up in the final line so the log reader can tell "SeaDex hit"
        // apart from "CF scoring pushed this above everything else."
        let breakdown = vec![("x265".to_string(), 300)];
        let s = format_scoring_detail(60, 300, &breakdown, 10000, 10360, false);
        assert_eq!(s, "base=60, cf=+300 [x265 +300], seadex=10000, final=10360");
    }

    // ----- apply_cf_seadex_overlay_with_breakdown -----

    fn candidate(title: &str, info_hash: &str) -> SearchResult {
        crate::services::custom_formats::test_helpers::candidate(title, "GroupX", 0, info_hash)
    }

    fn cls(source: Source, resolution: Resolution) -> ClassificationResult {
        crate::services::custom_formats::test_helpers::classification(source, resolution)
    }

    #[test]
    fn overlay_passes_through_base_when_no_cfs_or_seadex() {
        let r = candidate("[GroupX] Show 01 [1080p].mkv", "abc");
        let c = cls(Source::Web, Resolution::R1080p);
        let seadex = HashSet::new();

        let (final_score, parts) =
            apply_cf_seadex_overlay_with_breakdown(100, &r, &c, &[], &seadex, true, 0).unwrap();
        assert_eq!(final_score, 100);
        assert!(parts.is_empty());
    }

    #[test]
    fn overlay_drops_candidate_below_minimum_score_floor() {
        // Empty CF list ⇒ cf=0. With minimum_score=1, 0 < 1 ⇒ drop.
        let r = candidate("garbage", "abc");
        let c = cls(Source::Unknown, Resolution::Unknown);
        let seadex = HashSet::new();
        let result = apply_cf_seadex_overlay_with_breakdown(50, &r, &c, &[], &seadex, true, 1);
        assert!(result.is_none());
    }

    #[test]
    fn overlay_adds_seadex_bonus_on_hash_match_when_enabled() {
        let hash = "DEADBEEF";
        let r = candidate("title", hash);
        let c = cls(Source::Web, Resolution::R1080p);
        let mut seadex = HashSet::new();
        seadex.insert(hash.to_ascii_lowercase());

        let (final_score, parts) =
            apply_cf_seadex_overlay_with_breakdown(20, &r, &c, &[], &seadex, true, 0).unwrap();
        assert_eq!(
            final_score,
            20 + crate::services::seadex::SEADEX_SCORE_BOOST
        );
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].label, "SeaDex Best");
        assert_eq!(parts[0].delta, crate::services::seadex::SEADEX_SCORE_BOOST);
    }

    #[test]
    fn overlay_skips_seadex_bonus_when_disabled() {
        let hash = "DEADBEEF";
        let r = candidate("title", hash);
        let c = cls(Source::Web, Resolution::R1080p);
        let mut seadex = HashSet::new();
        seadex.insert(hash.to_ascii_lowercase());

        let (final_score, parts) = apply_cf_seadex_overlay_with_breakdown(
            20,
            &r,
            &c,
            &[],
            &seadex,
            /*enabled*/ false,
            0,
        )
        .unwrap();
        assert_eq!(final_score, 20);
        assert!(parts.is_empty());
    }

    #[test]
    fn overlay_skips_seadex_when_info_hash_empty_even_if_set_membership_would_match() {
        // SearchResults from some upstreams have empty info_hash. The
        // boost predicate explicitly guards against that — an empty
        // string membership check would always be false, but the
        // explicit `is_empty()` short-circuit makes the intent clear
        // and prevents future regressions if the seadex set ever
        // included an empty-string sentinel.
        let r = candidate("title", ""); // empty hash
        let c = cls(Source::Web, Resolution::R1080p);
        let mut seadex = HashSet::new();
        seadex.insert(String::new()); // pathological membership
        let (final_score, parts) =
            apply_cf_seadex_overlay_with_breakdown(20, &r, &c, &[], &seadex, true, 0).unwrap();
        assert_eq!(final_score, 20);
        assert!(parts.is_empty());
    }

    // ----- rescore_for_auto_search_with_breakdown -----

    fn default_config() -> Config {
        Config::default()
    }

    fn rescore(
        result: &SearchResult,
        classification: &ClassificationResult,
        target: &SearchTarget,
        is_finished: bool,
        finished_mode: quality::FinishedSeriesMode,
        absolute_offset: i32,
        batch_search_mode: bool,
    ) -> (i32, Vec<ScoreComponent>) {
        rescore_for_auto_search_with_breakdown(
            result,
            classification,
            &default_config(),
            &["Show".to_string()],
            target,
            1,
            is_finished,
            finished_mode,
            Source::Web,
            Resolution::R1080p,
            Source::Hdtv,
            Resolution::R720p,
            absolute_offset,
            batch_search_mode,
        )
    }

    #[test]
    fn rescore_episode_target_rewards_episode_match_and_single_episode_bonus() {
        let mut r = candidate("[GroupX] Show - 05 [1080p].mkv", "h1");
        r.score = 100;
        r.is_batch = false;
        let c = cls(Source::Web, Resolution::R1080p);
        let (score, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(5),
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            0,
            false,
        );

        let labels: Vec<_> = parts.iter().map(|p| p.label.as_str()).collect();
        assert!(labels.contains(&"Single-Episode Target"), "{labels:?}");
        assert!(labels.contains(&"Episode Number Match"), "{labels:?}");
        assert!(labels.contains(&"Title Alias Match"), "{labels:?}");
        // Score climbed from 100 by alias + single-ep + episode-match
        // contributions (≥ 50 minimum: 10 + 40).
        assert!(score >= 100 + 10 + 40, "got {score}");
    }

    #[test]
    fn rescore_episode_target_penalizes_batch_when_not_batch_search_mode() {
        let mut r = candidate("[GroupX] Show 01-12 Batch [1080p].mkv", "h1");
        r.score = 0;
        r.is_batch = true;
        let c = cls(Source::Web, Resolution::R1080p);
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(5),
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            0,
            /* batch_search_mode */ false,
        );
        let batch_pen = parts
            .iter()
            .find(|p| p.label.starts_with("Batch Penalty"))
            .expect("expected batch penalty in non-batch-search mode");
        assert_eq!(batch_pen.delta, -20);
    }

    #[test]
    fn rescore_suppresses_batch_penalty_in_batch_search_mode() {
        let mut r = candidate("[GroupX] Show 01-12 Batch [1080p].mkv", "h1");
        r.score = 0;
        r.is_batch = true;
        let c = cls(Source::Web, Resolution::R1080p);
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(5),
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            0,
            /* batch_search_mode */ true,
        );
        assert!(
            !parts.iter().any(|p| p.label.starts_with("Batch Penalty")),
            "batch_search_mode must suppress batch penalty: {parts:?}"
        );
    }

    #[test]
    fn rescore_buries_franchise_pass_release_with_wrong_episode_number() {
        // absolute_offset > 0 + parsed numbers don't match either
        // relative or absolute target ⇒ -1000 "Wrong Episode Number"
        // burial. This is the franchise-alias guard from #30.
        let mut r = candidate("[GroupX] Other Show - 99 [1080p].mkv", "h1");
        r.score = 0;
        r.is_batch = false;
        let c = cls(Source::Web, Resolution::R1080p);
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(5),
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            10,
            false,
        );
        let pen = parts
            .iter()
            .find(|p| p.label == "Wrong Episode Number")
            .expect("expected -1000 burial");
        assert_eq!(pen.delta, -1000);
    }

    #[test]
    fn rescore_buries_unparseable_episode_in_franchise_pass() {
        // absolute_offset > 0 + no parseable numbers ⇒ -500 "Unparseable
        // Episode Number" — softer than the wrong-number burial because
        // a movie release titled with no episode marker can show up here.
        let mut r = candidate("[GroupX] Show Movie [1080p].mkv", "h1");
        r.score = 0;
        r.is_batch = false;
        let c = cls(Source::Web, Resolution::R1080p);
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(5),
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            10,
            false,
        );
        let pen = parts
            .iter()
            .find(|p| p.label == "Unparseable Episode Number")
            .expect("expected -500 burial");
        assert_eq!(pen.delta, -500);
    }

    #[test]
    fn rescore_single_target_movie_bonus_outside_batch_mode() {
        let mut r = candidate("[GroupX] Show The Movie [1080p].mkv", "h1");
        r.score = 0;
        r.is_batch = false;
        let c = cls(Source::Web, Resolution::R1080p);
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Single,
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            0,
            false,
        );
        let bonus = parts
            .iter()
            .find(|p| p.label == "Movie / Special / OVA")
            .expect("movie bonus");
        assert_eq!(bonus.delta, 8);
    }

    #[test]
    fn rescore_finished_series_bd_bonus_only_with_prefer_bd_mode_and_bluray_source() {
        let mut r = candidate("[GroupX] Show 01 BD [1080p].mkv", "h1");
        r.score = 0;
        let c = cls(Source::BluRay, Resolution::R1080p);

        // Wrong mode → no BD bonus.
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(1),
            true,
            quality::FinishedSeriesMode::SameAsAiring,
            0,
            false,
        );
        assert!(!parts.iter().any(|p| p.label == "Finished Series BD Bonus"));

        // PreferBd + BluRay + finished → bonus fires.
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(1),
            true,
            quality::FinishedSeriesMode::PreferBd,
            0,
            false,
        );
        let bd_bonus = parts
            .iter()
            .find(|p| p.label == "Finished Series BD Bonus")
            .expect("BD bonus");
        assert_eq!(bd_bonus.delta, 35);

        // PreferBd + BluRay but airing (is_finished=false) → no bonus.
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(1),
            false,
            quality::FinishedSeriesMode::PreferBd,
            0,
            false,
        );
        assert!(!parts.iter().any(|p| p.label == "Finished Series BD Bonus"));
    }

    #[test]
    fn rescore_season_mismatch_applies_minus_100() {
        // Aliases include "Show"; expected season is 1 but the title
        // says S03 — the explicit season-mismatch penalty fires.
        let mut r = candidate("[GroupX] Show S03E05 [1080p].mkv", "h1");
        r.score = 0;
        let c = cls(Source::Web, Resolution::R1080p);
        let (_, parts) = rescore_for_auto_search_with_breakdown(
            &r,
            &c,
            &default_config(),
            &["Show".to_string()],
            &SearchTarget::Episode(5),
            1,
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            Source::Web,
            Resolution::R1080p,
            Source::Hdtv,
            Resolution::R720p,
            0,
            false,
        );
        let pen = parts
            .iter()
            .find(|p| p.label == "Season Mismatch")
            .expect("season-mismatch penalty");
        assert_eq!(pen.delta, -100);
    }

    // ----- Title Match Confidence (misgrab guardrails) -----

    fn stamped(
        title: &str,
        kind: MatchKind,
        phase: crate::services::auto_search::MatchPhase,
    ) -> SearchResult {
        let mut r = candidate(title, "h1");
        r.score = 100;
        r.is_batch = false;
        r.match_provenance = Some(crate::services::auto_search::MatchProvenance {
            phase,
            kind,
            alias: "Show".to_string(),
            ratio: 1.0,
        });
        r
    }

    fn confidence_delta(parts: &[ScoreComponent]) -> i32 {
        parts
            .iter()
            .find(|p| p.label == "Title Match Confidence")
            .expect("confidence line is always present for a stamped candidate")
            .delta
    }

    #[test]
    fn verbatim_candidate_outranks_equal_fuzzy_candidate() {
        use crate::services::auto_search::MatchPhase;
        let c = cls(Source::Web, Resolution::R1080p);
        let title = "[GroupX] Show - 05 [1080p].mkv";
        let verbatim = stamped(title, MatchKind::Verbatim, MatchPhase::Primary);
        let fuzzy = stamped(title, MatchKind::Fuzzy, MatchPhase::Primary);
        let run = |r: &SearchResult| {
            rescore(
                r,
                &c,
                &SearchTarget::Episode(5),
                false,
                quality::FinishedSeriesMode::SameAsAiring,
                0,
                false,
            )
        };
        let (score_v, parts_v) = run(&verbatim);
        let (score_f, parts_f) = run(&fuzzy);
        assert_eq!(
            score_v - score_f,
            -FUZZY_MATCH_PENALTY,
            "gap must be the fuzzy penalty"
        );
        assert_eq!(confidence_delta(&parts_v), 0);
        assert_eq!(confidence_delta(&parts_f), FUZZY_MATCH_PENALTY);
        let detail = parts_f
            .iter()
            .find(|p| p.label == "Title Match Confidence")
            .and_then(|p| p.detail.clone())
            .unwrap_or_default();
        assert!(detail.starts_with("Fuzzy alias match"), "{detail}");
    }

    #[test]
    fn fallback_phase_penalty_stacks_with_fuzzy() {
        use crate::services::auto_search::MatchPhase;
        let c = cls(Source::Web, Resolution::R1080p);
        let title = "[GroupX] Show - 05 [1080p].mkv";
        let run = |r: &SearchResult| {
            rescore(
                r,
                &c,
                &SearchTarget::Episode(5),
                false,
                quality::FinishedSeriesMode::SameAsAiring,
                0,
                false,
            )
            .1
        };
        assert_eq!(
            confidence_delta(&run(&stamped(
                title,
                MatchKind::Fuzzy,
                MatchPhase::Extended
            ))),
            FUZZY_MATCH_PENALTY + FALLBACK_PHASE_PENALTY
        );
        assert_eq!(
            confidence_delta(&run(&stamped(
                title,
                MatchKind::Verbatim,
                MatchPhase::Franchise
            ))),
            FALLBACK_PHASE_PENALTY
        );
        assert_eq!(
            confidence_delta(&run(&stamped(
                title,
                MatchKind::Verbatim,
                MatchPhase::BdProbe
            ))),
            0
        );
    }

    #[test]
    fn seadex_curated_gets_no_confidence_penalty() {
        use crate::services::auto_search::MatchPhase;
        let c = cls(Source::Web, Resolution::R1080p);
        let r = stamped(
            "[GroupX] Show - 05 [1080p].mkv",
            MatchKind::SeadexCurated,
            MatchPhase::SeadexSeed,
        );
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(5),
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            0,
            false,
        );
        assert_eq!(confidence_delta(&parts), 0);
    }

    #[test]
    fn unstamped_candidate_has_no_confidence_line() {
        let mut r = candidate("[GroupX] Show - 05 [1080p].mkv", "h1");
        r.score = 100;
        let c = cls(Source::Web, Resolution::R1080p);
        let (_, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(5),
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            0,
            false,
        );
        assert!(parts.iter().all(|p| p.label != "Title Match Confidence"));
    }

    #[test]
    fn breakdown_deltas_sum_to_the_score_change() {
        use crate::services::auto_search::MatchPhase;
        let c = cls(Source::Web, Resolution::R1080p);
        let r = stamped(
            "[GroupX] Show - 05 [1080p].mkv",
            MatchKind::Fuzzy,
            MatchPhase::Extended,
        );
        let (score, parts) = rescore(
            &r,
            &c,
            &SearchTarget::Episode(5),
            false,
            quality::FinishedSeriesMode::SameAsAiring,
            0,
            false,
        );
        let sum: i32 = parts.iter().map(|p| p.delta).sum();
        assert_eq!(score - r.score, sum, "{parts:?}");
    }

    #[test]
    fn alias_overlap_uses_distinctive_tokens() {
        // Issue #219 in miniature: "the animation" overlaps the alias
        // but carries no identity. The raw token ratio gave this +26;
        // the distinctive ratio gives nothing, so the component is
        // omitted (zero deltas are not listed).
        let c = cls(Source::Web, Resolution::R1080p);
        let aliases = ["Risa THE ANIMATION".to_string()];
        let score_for = |title: &str| {
            let mut r = candidate(title, "h1");
            r.score = 0;
            rescore_for_auto_search_with_breakdown(
                &r,
                &c,
                &default_config(),
                &aliases,
                &SearchTarget::Single,
                1,
                false,
                quality::FinishedSeriesMode::SameAsAiring,
                Source::Web,
                Resolution::R1080p,
                Source::Hdtv,
                Resolution::R720p,
                0,
                false,
            )
            .1
        };
        let generic = score_for("[GroupX] Other Show The Animation - 01 [1080p].mkv");
        assert!(
            generic.iter().all(|p| p.label != "Title Alias Match"),
            "{generic:?}"
        );
        let distinctive = score_for("[GroupX] Risa Something - 01 [1080p].mkv");
        let alias_part = distinctive
            .iter()
            .find(|p| p.label == "Title Alias Match")
            .expect("the distinctive token alone is a full match");
        assert_eq!(alias_part.delta, 40);
    }
}
