use std::collections::HashSet;
use std::sync::LazyLock;

use regex_lite::Regex;
use serde::{Deserialize, Serialize};

use crate::services::custom_formats::{self, CompiledCustomFormat, EvalContext};
use crate::services::nyaa::{SearchOptions, SearchResult};
use crate::services::source::{ClassificationResult, Resolution, Source, WebKind};

// Word-boundary "dub" / "dubbed" — anchors prevent the prior bare-
// substring match from false-positiving on "redub", "dubsoon",
// "dubbing", or release tags that happen to contain those bytes.
static DUB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:dub|dubbed)\b").expect("dub regex compiles"));

/// One line of a score breakdown: what fired, what it contributed,
/// and an optional human-readable detail (which group matched, how
/// many seeders, what threshold crossed, etc.). Surfaced on the
/// /api/search response and persisted alongside grab history so the
/// "why this score" UI can show users exactly what happened.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ScoreComponent {
    /// Short label — displayed as the left-column "what" in the UI
    /// breakdown table (e.g. "Seeders", "Preferred Group",
    /// "Resolution Match").
    pub label: String,
    /// Signed point contribution. Positive = bonus, negative =
    /// penalty. Sum of all `delta`s in a breakdown equals the total
    /// score — invariant pinned by tests.
    pub delta: i32,
    /// Optional free-text detail for the UI tooltip / secondary row
    /// ("3 of N preferred", "1000+ seeders", etc.). Keeps the label
    /// concise while still giving users the full picture.
    pub detail: Option<String>,
}

impl ScoreComponent {
    fn new(label: &str, delta: i32, detail: Option<String>) -> Self {
        Self {
            label: label.to_string(),
            delta,
            detail,
        }
    }
}

/// Score a search result based on multiple factors.
/// `prefer_subs` controls whether dual audio/dub releases are penalized (default true).
#[allow(dead_code)]
pub fn score_result(r: &SearchResult, opts: &SearchOptions) -> i32 {
    score_result_with_sub_pref(r, opts, true)
}

/// Scalar score. Delegates to `score_result_with_breakdown` and
/// discards the component list — use the breakdown variant directly
/// when you need both total and per-component detail.
pub fn score_result_with_sub_pref(
    r: &SearchResult,
    opts: &SearchOptions,
    prefer_subs: bool,
) -> i32 {
    score_result_with_breakdown(r, opts, prefer_subs).0
}

/// Same total as `score_result_with_sub_pref`, plus the ordered
/// list of components that contributed to the score. Invariant:
/// `breakdown.iter().map(|c| c.delta).sum::<i32>() == total`.
///
/// Components are emitted in the evaluation order (seeders first,
/// then group, resolution, and so on). Zero-delta checks are
/// omitted — a "no preferred group configured, didn't penalize"
/// non-event doesn't add noise to the UI. The invariant holds
/// because we only push when we actually mutate `score`.
#[allow(clippy::cognitive_complexity)]
pub fn score_result_with_breakdown(
    r: &SearchResult,
    opts: &SearchOptions,
    prefer_subs: bool,
) -> (i32, Vec<ScoreComponent>) {
    let mut total: i32 = 0;
    let mut parts: Vec<ScoreComponent> = Vec::new();
    let mut add = |label: &str, delta: i32, detail: Option<String>| {
        total += delta;
        parts.push(ScoreComponent::new(label, delta, detail));
    };

    // Seeders.
    if r.seeders > 100 {
        add("Seeders", 30, Some(format!("{} seeders (>100)", r.seeders)));
    } else if r.seeders > 50 {
        add("Seeders", 25, Some(format!("{} seeders (>50)", r.seeders)));
    } else if r.seeders > 10 {
        add("Seeders", 20, Some(format!("{} seeders (>10)", r.seeders)));
    } else if r.seeders > 0 {
        add("Seeders", 10, Some(format!("{} seeders", r.seeders)));
    } else {
        add("Seeders", -10, Some("zero seeders".to_string()));
    }

    // Preferred group. Earlier entries are stronger preferences.
    if !opts.preferred_groups.is_empty() {
        if !r.group.is_empty() {
            let mut matched_index = None;
            for (idx, g) in opts.preferred_groups.iter().enumerate() {
                if g.eq_ignore_ascii_case(&r.group) {
                    matched_index = Some(idx as i32);
                    break;
                }
            }
            if let Some(idx) = matched_index {
                let delta = 140 - (idx * 20);
                add(
                    "Preferred Group",
                    delta,
                    Some(format!("[{}] rank {} of preferred list", r.group, idx + 1)),
                );
            } else {
                add(
                    "Non-Preferred Group",
                    -15,
                    Some(format!("[{}] not in preferred list", r.group)),
                );
            }
        } else {
            add(
                "No Group Tag",
                -10,
                Some("release title has no [Group] prefix".to_string()),
            );
        }
    }

    // Preferred resolution.
    if !opts.preferred_resolution.is_empty() && r.resolution == opts.preferred_resolution {
        add(
            "Preferred Resolution",
            20,
            Some(format!("{} matches preferred", r.resolution)),
        );
    }

    // Batch bonus.
    if r.is_batch {
        add("Batch Release", 15, None);
    }

    // Trusted bonus.
    if r.is_trusted {
        add("Trusted Uploader", 10, None);
    }

    // Encoding/source quality.
    let lower = r.title.to_lowercase();
    if lower.contains("10bit")
        || lower.contains("10-bit")
        || lower.contains("x265")
        || lower.contains("hevc")
        || lower.contains("bluray")
        || lower.contains("blu-ray")
        || lower.contains("bdrip")
        || lower.contains(" bd ")
        || lower.starts_with("bd ")
        || lower.contains("[bd")
        || lower.contains("(bd")
    {
        add(
            "Encoding / Source Quality",
            5,
            Some("10bit / x265 / HEVC / BluRay keyword in title".to_string()),
        );
    }

    // Dub vs Sub scoring.
    //
    // Detecting the bare substring `"multi"` false-positived on titles
    // that contained words like "multimedia" or group/release tags
    // ending in "multi" — those got tagged as dual-audio and shifted
    // under the sub/dub preference logic, nudging scoring in the wrong
    // direction. Tighten to the actual release-naming conventions for
    // multi-audio releases.
    let is_dual = lower.contains("dual audio")
        || lower.contains("dual.audio")
        || lower.contains("multi audio")
        || lower.contains("multi.audio")
        || lower.contains("multi-audio")
        || lower.contains("multiaudio");
    // Match "dub"/"dubbed" only as whole words. The earlier `multi`
    // tightening missed this companion case — bare contains("dub")
    // would fire on "redub", "dubsoon", and any release tag whose bytes
    // happened to include "dub". `english dub` stays as a literal
    // substring because the space anchors it.
    let is_dub = is_dual || DUB_RE.is_match(&lower) || lower.contains("english dub");
    if prefer_subs {
        if is_dub {
            add(
                "Dub / Dual Audio Penalty",
                -15,
                Some("user prefers subs; release flagged as dub/dual".to_string()),
            );
        }
    } else if is_dub {
        add(
            "Dub / Dual Audio Bonus",
            15,
            Some("user prefers dubs".to_string()),
        );
    }

    // Downloads popularity.
    if r.downloads > 10000 {
        add(
            "Downloads",
            15,
            Some(format!("{} downloads (>10k)", r.downloads)),
        );
    } else if r.downloads > 5000 {
        add(
            "Downloads",
            10,
            Some(format!("{} downloads (>5k)", r.downloads)),
        );
    } else if r.downloads > 1000 {
        add(
            "Downloads",
            5,
            Some(format!("{} downloads (>1k)", r.downloads)),
        );
    }

    // Small batch bonus (under ~25GB).
    if r.is_batch && r.size_bytes > 0 && r.size_bytes < 25 * 1024 * 1024 * 1024 {
        add("Compact Batch", 10, Some("batch under 25 GiB".to_string()));
    }

    (total, parts)
}

/// Rehydrate a `ClassificationResult` from the already-populated source
/// fields on a `SearchResult`. The scraper stores `source` / `resolution`
/// / `web_kind` as display strings plus `is_remux` / `is_bdmv` booleans;
/// the CF evaluator wants the typed enums. `evidence` / `confidence` /
/// `needs_review` / `decision_rule` aren't available at manual-search
/// time, so they default — CF evaluation doesn't read them.
fn classification_from_search_result(r: &SearchResult) -> ClassificationResult {
    let web_kind = if r.web_kind.is_empty() {
        WebKind::Unknown
    } else {
        WebKind::from_str(&r.web_kind)
    };
    ClassificationResult {
        source: Source::from_str(&r.source),
        resolution: Resolution::from_str(&r.resolution),
        is_remux: r.is_remux,
        web_kind,
        is_bdmv: r.is_bdmv,
        confidence: 1.0,
        needs_review: false,
        evidence: Vec::new(),
        decision_rule: crate::services::source::DecisionRule::Empty,
    }
}

/// Evaluate the compiled CF set against each result in `results`,
/// adding matching CF contributions to both `result.score` and
/// `result.score_breakdown`. Used by the manual-search path so the
/// "why this score" expander shows CF deltas alongside the base rules.
///
/// `seadex_hashes` can be empty — the manual search has no series
/// context, so SeaDex specs simply never fire. That's fine for now;
/// the manual search isn't the SeaDex surface.
///
/// Appends one `ScoreComponent` per matching CF with a non-zero score,
/// labeled `"CF: <name>"` so the UI can distinguish them from the
/// base-score rules at a glance.
pub fn apply_cf_breakdown(
    results: &mut [SearchResult],
    cfs: &[CompiledCustomFormat],
    seadex_hashes: &HashSet<String>,
) {
    if cfs.is_empty() {
        return;
    }
    for r in results.iter_mut() {
        let classification = classification_from_search_result(r);
        // Borrowed ctx needs the result to live for the whole call,
        // but we're about to mutate the result's score. Capture the
        // breakdown first with an immutable borrow, drop it, then
        // mutate.
        let (cf_total, breakdown) = {
            let ctx = EvalContext {
                result: r,
                classification: &classification,
                seadex_hashes,
            };
            custom_formats::total_cf_score_with_breakdown(cfs, &ctx)
        };
        if cf_total == 0 && breakdown.is_empty() {
            continue;
        }
        r.score = r.score.saturating_add(cf_total);
        for (name, delta) in breakdown {
            r.score_breakdown
                .push(ScoreComponent::new(&format!("CF: {name}"), delta, None));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::nyaa::{SearchOptions, SearchResult};
    use rstest::rstest;

    fn result(seeders: i32, title: &str) -> SearchResult {
        SearchResult {
            match_provenance: None,
            title: title.to_string(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes: 0,
            seeders,
            leechers: 0,
            downloads: 0,
            group: String::new(),
            resolution: "1080p".to_string(),
            quality_label: String::new(),
            source: String::new(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: false,
            is_trusted: false,
            score: 0,
            info_hash: String::new(),
            score_breakdown: Vec::new(),
            upload_date: String::new(),
            indexer_id: None,
            indexer_name: String::new(),
        }
    }

    fn opts() -> SearchOptions {
        SearchOptions::default()
    }

    #[test]
    fn breakdown_sum_equals_total_always() {
        // Invariant: components[].delta.sum() == total_score.
        // Exercise a handful of realistic shapes to pin it.
        let cases: Vec<(SearchResult, SearchOptions, bool)> = vec![
            (
                result(150, "[SubsPlease] Frieren - 01 (1080p)"),
                opts(),
                true,
            ),
            (result(0, "No Seeders Release"), opts(), true),
            (
                {
                    let mut r = result(55, "[Kaizoku] Series - Batch [1080p BluRay x265]");
                    r.is_batch = true;
                    r.is_trusted = true;
                    r.size_bytes = 10 * 1024 * 1024 * 1024;
                    r.downloads = 15_000;
                    r.group = "Kaizoku".to_string();
                    r
                },
                SearchOptions {
                    preferred_groups: vec!["Kaizoku".to_string(), "smol".to_string()],
                    preferred_resolution: "1080p".to_string(),
                    ..SearchOptions::default()
                },
                true,
            ),
            (
                {
                    let mut r = result(5, "[Group] Series - 01 Dual Audio (1080p)");
                    r.group = "Group".to_string();
                    r
                },
                SearchOptions {
                    preferred_groups: vec!["smol".to_string()],
                    preferred_resolution: "1080p".to_string(),
                    ..SearchOptions::default()
                },
                true,
            ),
        ];
        for (r, opts_case, prefer_subs) in cases {
            let (total, parts) = score_result_with_breakdown(&r, &opts_case, prefer_subs);
            let sum: i32 = parts.iter().map(|c| c.delta).sum();
            assert_eq!(
                total, sum,
                "invariant violated for {:?} — total={} sum={} parts={:?}",
                r.title, total, sum, parts
            );
            // Every component should have a non-zero delta (we don't
            // emit no-op entries).
            for p in &parts {
                assert_ne!(p.delta, 0, "zero-delta component: {:?}", p);
            }
        }
    }

    #[test]
    fn scalar_score_matches_breakdown_total() {
        // The two public APIs must agree on the total.
        let mut r = result(75, "[Group] Cool Series - 01 (1080p) [BD].mkv");
        r.group = "Group".to_string();
        r.is_batch = false;
        r.downloads = 3000;
        let opts = SearchOptions {
            preferred_groups: vec!["Group".to_string()],
            preferred_resolution: "1080p".to_string(),
            ..SearchOptions::default()
        };
        let scalar = score_result_with_sub_pref(&r, &opts, true);
        let (breakdown_total, _) = score_result_with_breakdown(&r, &opts, true);
        assert_eq!(scalar, breakdown_total);
    }

    // ─── Boundary-pinning tests for `score_result_with_breakdown` ───
    //
    // Mutation-testing audit (mutants.out.pre-pull) found every comparison
    // operator in the seeders ladder (lines 89/91/93/95), the downloads
    // ladder (lines 211/217/223), and the compact-batch guard (line 232)
    // survived a hostile mutation. Existing tests asserted the
    // sum-of-deltas invariant but never that crossing a threshold
    // produced a different score band — so `>` flipped to `<`/`==`/`>=`
    // would not fail any test.
    //
    // Each rstest case below pins the value on each side of a threshold
    // and asserts the resulting `ScoreComponent.delta` matches the band
    // the production code intends. Showing each (input, expected_delta)
    // case as its own test name makes mutation-testing failure messages
    // point straight at the broken band. See mutants.out/PLAN.md Item 2.

    /// Find a component by label. None when the band's "no contribution"
    /// case fires (e.g. zero-downloads doesn't push a Downloads entry).
    fn delta_of(parts: &[ScoreComponent], label: &str) -> Option<i32> {
        parts.iter().find(|c| c.label == label).map(|c| c.delta)
    }

    #[rstest]
    #[case(0, -10)] // zero seeders penalty (else branch, line 98)
    #[case(1, 10)] // r.seeders > 0 → +10 (line 95)
    #[case(10, 10)] // 10 is NOT > 10, still in the >0 band → +10
    #[case(11, 20)] // crosses > 10 boundary → +20 (line 93)
    #[case(50, 20)] // 50 is NOT > 50, still in the >10 band → +20
    #[case(51, 25)] // crosses > 50 boundary → +25 (line 91)
    #[case(100, 25)] // 100 is NOT > 100, still in the >50 band → +25
    #[case(101, 30)] // crosses > 100 boundary → +30 (line 89)
    fn seeders_band_pins_each_threshold_boundary(
        #[case] seeders: i32,
        #[case] expected_delta: i32,
    ) {
        let r = result(seeders, "[G] Show - 01.mkv");
        let (_total, parts) = score_result_with_breakdown(&r, &opts(), true);
        assert_eq!(
            delta_of(&parts, "Seeders"),
            Some(expected_delta),
            "seeders={seeders} should land in band with delta={expected_delta}"
        );
    }

    #[rstest]
    #[case(0, None)] // no Downloads component when below all thresholds
    #[case(1000, None)] // 1000 is NOT > 1000 — still no entry
    #[case(1001, Some(5))] // crosses > 1000 → +5 (line 223)
    #[case(5000, Some(5))] // 5000 NOT > 5000, still in >1000 band
    #[case(5001, Some(10))] // crosses > 5000 → +10 (line 217)
    #[case(10000, Some(10))] // 10000 NOT > 10000, still in >5000 band
    #[case(10001, Some(15))] // crosses > 10000 → +15 (line 211)
    fn downloads_band_pins_each_threshold_boundary(
        #[case] downloads: i32,
        #[case] expected: Option<i32>,
    ) {
        // Hold seeders constant in the +20 band so the rest of the
        // breakdown is stable across cases — only the Downloads entry
        // varies. Title is the same; opts() defaults are empty so the
        // group/resolution/CF branches don't fire.
        let mut r = result(25, "[G] Show - 01.mkv");
        r.downloads = downloads;
        let (_total, parts) = score_result_with_breakdown(&r, &opts(), true);
        assert_eq!(
            delta_of(&parts, "Downloads"),
            expected,
            "downloads={downloads} should produce {:?}",
            expected
        );
    }

    /// 25 GiB — the literal upper bound on the compact-batch size guard
    /// at line 232. Spelled out as a const so the mutation that flips
    /// `*` to `+` (which collapses the constant to 25 + 1024 + 1024 +
    /// 1024 = ~3K bytes) is observably broken even by the "1 byte"
    /// boundary case below.
    const TWENTY_FIVE_GIB: i64 = 25 * 1024 * 1024 * 1024;

    #[rstest]
    // is_batch=false: never emits Compact Batch regardless of size.
    // Pins the leading `r.is_batch &&` — `||` would let size_bytes
    // alone trigger the bonus.
    #[case(false, 1, None)]
    #[case(false, 5_000_000_000, None)]
    // is_batch=true with size_bytes=0: NOT > 0 → no emit. Pins the
    // `r.size_bytes > 0` guard. Mutating `>` to `>=` would emit at 0.
    #[case(true, 0, None)]
    // is_batch=true with size_bytes=1: just over 0, well under 25 GiB
    // → emit.
    #[case(true, 1, Some(10))]
    // Just under 25 GiB (the upper bound) → emit.
    #[case(true, TWENTY_FIVE_GIB - 1, Some(10))]
    // Exactly 25 GiB: NOT < 25 GiB → no emit. Pins `<` against `<=`.
    #[case(true, TWENTY_FIVE_GIB, None)]
    // Over 25 GiB → no emit.
    #[case(true, TWENTY_FIVE_GIB + 1, None)]
    fn compact_batch_pins_size_thresholds_and_is_batch_guard(
        #[case] is_batch: bool,
        #[case] size_bytes: i64,
        #[case] expected: Option<i32>,
    ) {
        let mut r = result(25, "[G] Show - Batch.mkv");
        r.is_batch = is_batch;
        r.size_bytes = size_bytes;
        let (_total, parts) = score_result_with_breakdown(&r, &opts(), true);
        assert_eq!(
            delta_of(&parts, "Compact Batch"),
            expected,
            "is_batch={is_batch} size_bytes={size_bytes} should produce {:?}",
            expected
        );
    }

    // ─── Stretch tests from PLAN.md Item 2 deferred ─────────────────
    //
    // The original boundary-table tests pinned the seeders / downloads /
    // compact-batch comparison ladders. These additions extend the
    // coverage to the encoding-keyword chain, dual-audio detection,
    // preferred-resolution match, and the legacy `score_result` wrapper.
    // Each test isolates one branch so mutating one `||` to `&&` or
    // dropping one `!` produces an observable score change.

    #[rstest]
    // Each row is a release-title fragment that triggers ONE keyword
    // arm in the encoding chain (lines 155-166). Using isolated
    // fragments — rather than a soup of every keyword — pins each
    // `||` operator: a mutation flipping one to `&&` would require
    // ALL keywords in the chain, breaking the case that was relying
    // on this single match.
    #[case::ten_bit_word("[G] Show 01 10bit.mkv")]
    #[case::ten_bit_hyphen("[G] Show 01 10-bit.mkv")]
    #[case::x265("[G] Show 01 x265.mkv")]
    #[case::hevc("[G] Show 01 HEVC.mkv")]
    #[case::bluray("[G] Show 01 BluRay.mkv")]
    #[case::blu_ray_hyphen("[G] Show 01 Blu-Ray.mkv")]
    #[case::bdrip("[G] Show 01 BDRip.mkv")]
    #[case::space_bd_space("[G] Show 01 BD 1080p.mkv")]
    #[case::starts_with_bd("BD Show 01.mkv")]
    #[case::bracket_bd("[BD] Show 01.mkv")]
    #[case::paren_bd("(BD) Show 01.mkv")]
    fn encoding_keyword_each_alone_triggers_quality_bonus(#[case] title: &str) {
        // Each isolated keyword must produce the +5 "Encoding / Source
        // Quality" component. With seeders=0 (not in any seeder band),
        // the only positive delta will be this bonus, plus -10 zero-
        // seeders. Total should be -5.
        let mut r = result(0, title);
        r.is_batch = false;
        r.is_trusted = false;
        r.downloads = 0;
        let (_total, parts) = score_result_with_breakdown(&r, &opts(), true);
        let encoding = parts
            .iter()
            .find(|c| c.label == "Encoding / Source Quality");
        assert!(
            encoding.is_some(),
            "title {title:?} must trigger the encoding-quality bonus"
        );
        assert_eq!(encoding.unwrap().delta, 5);
    }

    #[test]
    fn encoding_keyword_no_match_skips_quality_bonus() {
        // Sanity: a release with NO encoding token in the title gets
        // no Encoding component at all. Pins the negative case so the
        // chain stays a guarded gate — without this, an `||` to `&&`
        // mutation flipping every match arm to AND-required would
        // never fire and look like a no-op.
        let r = result(0, "[G] Show 01 (1080p).mkv");
        let (_total, parts) = score_result_with_breakdown(&r, &opts(), true);
        assert!(
            parts.iter().all(|c| c.label != "Encoding / Source Quality"),
            "no encoding keyword in title — bonus must not fire"
        );
    }

    #[rstest]
    // Each case isolates one of the six dual-audio variants at lines
    // 182-187. Same `||`-chain shape as the encoding-keyword test.
    #[case::dual_audio_space("[G] Show 01 Dual Audio.mkv")]
    #[case::dual_audio_dot("[G] Show 01 Dual.Audio.mkv")]
    #[case::multi_audio_space("[G] Show 01 Multi Audio.mkv")]
    #[case::multi_audio_dot("[G] Show 01 Multi.Audio.mkv")]
    #[case::multi_audio_hyphen("[G] Show 01 Multi-Audio.mkv")]
    #[case::multiaudio("[G] Show 01 Multiaudio.mkv")]
    fn dual_audio_keyword_each_alone_triggers_dub_penalty(#[case] title: &str) {
        let r = result(0, title);
        let (_total, parts) = score_result_with_breakdown(&r, &opts(), true);
        let dub = parts.iter().find(|c| c.label == "Dub / Dual Audio Penalty");
        assert!(
            dub.is_some(),
            "title {title:?} must trigger the dub/dual penalty"
        );
        assert_eq!(dub.unwrap().delta, -15, "dub penalty must be exactly -15");
    }

    #[rstest]
    // The is_dub final assembly at line 193:
    //     is_dual || DUB_RE.is_match(&lower) || lower.contains("english dub")
    // Pin the two `||` operators by exercising each path individually
    // without the dual-audio case (already covered above).
    #[case::dub_word("[G] Show 01 Dub.mkv")]
    #[case::dubbed_word("[G] Show 01 Dubbed.mkv")]
    #[case::english_dub_literal("[G] Show 01 English Dub.mkv")]
    fn dub_word_or_english_dub_phrase_triggers_penalty(#[case] title: &str) {
        let r = result(0, title);
        let (_total, parts) = score_result_with_breakdown(&r, &opts(), true);
        assert!(
            parts.iter().any(|c| c.label == "Dub / Dual Audio Penalty"),
            "title {title:?} must be detected as dub/dual"
        );
    }

    #[test]
    fn dub_inverts_to_bonus_when_user_prefers_dubs() {
        // prefer_subs=false flips the penalty into a bonus. Pin exact
        // values on both sides so a `delete -` on either branch is
        // observable (line 198 for the penalty side).
        let r = result(0, "[G] Show 01 English Dub.mkv");

        let (_, parts_subs) = score_result_with_breakdown(&r, &opts(), true);
        let penalty = parts_subs
            .iter()
            .find(|c| c.label == "Dub / Dual Audio Penalty")
            .expect("penalty present");
        assert_eq!(penalty.delta, -15);

        let (_, parts_dubs) = score_result_with_breakdown(&r, &opts(), false);
        let bonus = parts_dubs
            .iter()
            .find(|c| c.label == "Dub / Dual Audio Bonus")
            .expect("bonus present");
        assert_eq!(bonus.delta, 15);
    }

    #[rstest]
    // Pin all three branches of `if !opts.preferred_resolution.is_empty()
    // && r.resolution == opts.preferred_resolution` at line 135:
    //   * empty preferred → no bonus regardless of release resolution
    //     (catches `delete !`)
    //   * non-empty preferred matching release → +20 (catches `==`/`!=`)
    //   * non-empty preferred mismatched against release → no bonus
    //     (catches `&&`/`||`)
    #[case::empty_preferred_skipped("", "1080p", false)]
    #[case::matching_preferred_adds_20("1080p", "1080p", true)]
    #[case::mismatched_preferred_skipped("720p", "1080p", false)]
    fn preferred_resolution_match_pins_guard_and_equality(
        #[case] preferred: &str,
        #[case] release_resolution: &str,
        #[case] expect_bonus: bool,
    ) {
        let mut r = result(0, "[G] Show - 01");
        r.resolution = release_resolution.to_string();
        let opts_case = SearchOptions {
            preferred_resolution: preferred.to_string(),
            ..SearchOptions::default()
        };
        let (_total, parts) = score_result_with_breakdown(&r, &opts_case, true);
        let pr = parts.iter().find(|c| c.label == "Preferred Resolution");
        if expect_bonus {
            assert_eq!(
                pr.expect("Preferred Resolution must be present").delta,
                20,
                "matching preferred must add exactly +20"
            );
        } else {
            assert!(
                pr.is_none(),
                "no Preferred Resolution component when guard is closed"
            );
        }
    }

    #[test]
    fn non_preferred_group_penalty_is_exactly_minus_15() {
        // Pin line 121's `delete -` on -15. The release has a
        // [Group] tag that's NOT in the preferred list, so the
        // "Non-Preferred Group" branch fires.
        let mut r = result(50, "[OtherGroup] Show - 01.mkv");
        r.group = "OtherGroup".to_string();
        let opts_case = SearchOptions {
            preferred_groups: vec!["Kaizoku".to_string(), "smol".to_string()],
            ..SearchOptions::default()
        };
        let (_, parts) = score_result_with_breakdown(&r, &opts_case, true);
        let np = parts
            .iter()
            .find(|c| c.label == "Non-Preferred Group")
            .expect("Non-Preferred Group present");
        assert_eq!(np.delta, -15, "non-preferred-group penalty must be -15");
    }

    #[test]
    fn no_group_tag_penalty_is_exactly_minus_10() {
        // Pin line 128's `delete -` on -10. The release has an empty
        // group field, so the "No Group Tag" branch fires.
        let mut r = result(50, "Show - 01.mkv");
        r.group = String::new();
        let opts_case = SearchOptions {
            preferred_groups: vec!["Kaizoku".to_string()],
            ..SearchOptions::default()
        };
        let (_, parts) = score_result_with_breakdown(&r, &opts_case, true);
        let ng = parts
            .iter()
            .find(|c| c.label == "No Group Tag")
            .expect("No Group Tag present");
        assert_eq!(ng.delta, -10, "no-group-tag penalty must be -10");
    }

    #[test]
    fn score_result_wrapper_delegates_to_with_sub_pref_true() {
        // Pin line 52's `pub fn score_result(...) -> i32` against the
        // three return-substitution mutations (0 / 1 / -1). The wrapper
        // is a one-liner — the cheapest assertion is "calling it
        // produces the same value as calling the explicit prefer_subs
        // form with true."
        let mut r = result(75, "[Group] Show - 01 (1080p) [BluRay].mkv");
        r.group = "Group".to_string();
        r.downloads = 3000;
        let opts_case = SearchOptions {
            preferred_groups: vec!["Group".to_string()],
            preferred_resolution: "1080p".to_string(),
            ..SearchOptions::default()
        };
        let via_wrapper = score_result(&r, &opts_case);
        let via_explicit = score_result_with_sub_pref(&r, &opts_case, true);
        assert_eq!(via_wrapper, via_explicit);
        // And the value is non-trivial — replacing with 0/1/-1 would
        // not match any of those.
        assert!(
            via_wrapper.abs() > 1,
            "wrapper score must be non-trivial (got {via_wrapper})"
        );
    }

    #[test]
    fn apply_cf_breakdown_noop_with_empty_cf_list() {
        let mut r = result(30, "[Group] Series - 01 (1080p)");
        r.score = 42;
        let before_breakdown = r.score_breakdown.len();
        let mut batch = vec![r];
        apply_cf_breakdown(&mut batch, &[], &HashSet::new());
        assert_eq!(batch[0].score, 42);
        assert_eq!(batch[0].score_breakdown.len(), before_breakdown);
    }

    #[test]
    fn apply_cf_breakdown_appends_cf_prefixed_entries_and_bumps_score() {
        // Compile a tiny CF that matches any release whose title
        // contains "x265". Using the real parser so this test stays
        // honest about how CF scoring actually fires.
        let cf = crate::services::custom_formats::compile_from_json(
            r#"{
                "name": "x265 bonus",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                }]
            }"#,
            50,
            1,
        )
        .expect("test CF compiles");

        let mut hit = result(10, "[Group] Series - 01 (1080p) [x265].mkv");
        hit.score = 20;
        let base_breakdown_len = hit.score_breakdown.len();

        let mut miss = result(10, "[Group] Series - 02 (1080p).mkv");
        miss.score = 20;

        let mut batch = vec![hit, miss];
        apply_cf_breakdown(&mut batch, std::slice::from_ref(&cf), &HashSet::new());

        // Hit: score bumped, one new "CF: x265 bonus" entry.
        assert_eq!(batch[0].score, 70);
        assert_eq!(batch[0].score_breakdown.len(), base_breakdown_len + 1);
        let added = batch[0].score_breakdown.last().expect("new entry");
        assert_eq!(added.label, "CF: x265 bonus");
        assert_eq!(added.delta, 50);

        // Miss: untouched.
        assert_eq!(batch[1].score, 20);
    }

    #[test]
    fn preferred_group_rank_appears_in_detail() {
        let mut r = result(10, "[Beatrice-Raws] Series - 01 (1080p)");
        r.group = "Beatrice-Raws".to_string();
        let opts = SearchOptions {
            preferred_groups: vec!["smol".to_string(), "Beatrice-Raws".to_string()],
            preferred_resolution: String::new(),
            ..SearchOptions::default()
        };
        let (_, parts) = score_result_with_breakdown(&r, &opts, true);
        let group_comp = parts
            .iter()
            .find(|c| c.label == "Preferred Group")
            .expect("preferred group component missing");
        assert_eq!(group_comp.delta, 120); // 140 - (1 * 20)
        assert!(
            group_comp
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("rank 2")
        );
    }
}
