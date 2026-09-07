//! Per-candidate CF matching and score summation.
//!
//! Two layers stacked here:
//!
//! 1. **Per-spec kernel** (`evaluate_spec_kernel` + negate wrapper):
//!    one spec against one candidate. Mirrors Sonarr's
//!    `IsSatisfiedByWithoutNegate` / `IsSatisfiedBy`.
//! 2. **CF-level matching** (`evaluate`): group-by-type DidMatch rule
//!    from Sonarr v4 §5.7. NOT "all specs true" and NOT "any spec
//!    true" — grouped OR-within-type, AND-across-types, with a
//!    required-hard-fail gate per group.
//!
//! The public API is `evaluate` + `total_cf_score_with_breakdown`
//! (with `total_cf_score` retained as `pub(super)` for the in-module
//! test matrix); the kernel stays private to keep callers from
//! accidentally bypassing the negate wrapper.

use std::collections::BTreeMap;

use super::{CompiledCustomFormat, CompiledSpec, EvalContext, Source, SpecKind, WebKind};

/// Raw per-spec match, pre-negate. Mirrors Sonarr's
/// `IsSatisfiedByWithoutNegate`.
fn evaluate_spec_kernel(spec: &CompiledSpec, ctx: &EvalContext) -> bool {
    match &spec.kind {
        // fancy-regex returns `Result<bool, Error>` because backtracking
        // can hit a step limit on pathological inputs. On error (step
        // limit exceeded, runtime failure) treat as non-match — a
        // Sonarr-compat CF should not be able to brick scoring for an
        // entire search just because one spec timed out.
        SpecKind::ReleaseTitle { regex } => regex.is_match(&ctx.result.title).unwrap_or(false),
        // `SearchResult::group` is a bare String. Empty means the Nyaa
        // scraper didn't find a `[Group]` prefix; an empty-string regex
        // still matches it, which is consistent with Sonarr's behavior.
        SpecKind::ReleaseGroup { regex } => regex.is_match(&ctx.result.group).unwrap_or(false),
        SpecKind::Size {
            min_bytes,
            max_bytes,
        } => {
            // Sonarr's SizeSpecification.cs uses strict-greater on the
            // lower bound and ≤ on the upper bound: `size > Min &&
            // size <= Max`. Mirror exactly.
            let s = ctx.result.size_bytes;
            s > *min_bytes && s <= *max_bytes
        }
        SpecKind::Resolution { value } => ctx.classification.resolution == *value,
        SpecKind::Source { sonarr_value } => {
            let c = ctx.classification;
            match sonarr_value {
                0 => c.source == Source::Unknown,
                1 => matches!(c.source, Source::Hdtv | Source::Tv),
                // Sonarr's `SourceSpecification` value 3 is strict
                // WebDl in vanilla Sonarr. Ryokan unifies WebDl and
                // bare Web at the label layer (issue #48) because the
                // filename-token-or-not asymmetry was confusing users.
                // The CF evaluator mirrors that: value 3 fires on any
                // Source::Web that isn't explicitly WebRip, so TRaSH
                // `anime-web-tier-*` CFs still match SubsPlease /
                // HorribleSubs / every other WEB release regardless
                // of whether the filename carried a `WEB-DL` token.
                // Value 4 stays strict WebRip so penalty CFs only fire
                // on the lower-quality sub-tier.
                3 => c.source == Source::Web && c.web_kind != WebKind::WebRip,
                4 => c.source == Source::Web && c.web_kind == WebKind::WebRip,
                5 => c.source == Source::Dvd,
                6 => c.source == Source::BluRay && !c.is_bdmv,
                7 => c.source == Source::BluRay && c.is_bdmv,
                // 2 (TelevisionRaw) and out-of-range are filtered at
                // parse time, so they never reach here.
                _ => false,
            }
        }
        SpecKind::SeaDexBest => {
            !ctx.result.info_hash.is_empty()
                && ctx
                    .seadex_hashes
                    .contains(&ctx.result.info_hash.to_ascii_lowercase())
        }
    }
}

/// Per-spec match with Sonarr's `Negate` applied. This is the input to
/// the group-by-type DidMatch rule in [`evaluate`] — never call the
/// kernel directly from there.
fn evaluate_spec(spec: &CompiledSpec, ctx: &EvalContext) -> bool {
    let raw = evaluate_spec_kernel(spec, ctx);
    if spec.negate { !raw } else { raw }
}

/// Does this CF match the candidate?
///
/// Implements Sonarr v4's group-by-type DidMatch rule verbatim. See
/// plan §5.7.1 for the Sonarr source snippet this mirrors, and §5.7.3
/// for worked examples.
///
/// The rule is NOT "all specs true" and NOT "any spec true". It's:
/// 1. Group specs by `type_tag()`.
/// 2. Within each group: match iff no `required=true` spec returned
///    false AND at least one spec returned true.
/// 3. CF matches iff every group matches.
pub fn evaluate(cf: &CompiledCustomFormat, ctx: &EvalContext) -> bool {
    // Vacuous-truth parity with Sonarr: a CF with zero specs produces
    // an empty groups list, and `empty.All(x => x.DidMatch)` is `true`
    // in LINQ (same as `.all()` in Rust). Real imports can't reach this
    // branch — `compile_from_json` rejects all-unsupported CFs — but
    // strict parity means we mirror Sonarr rather than second-guessing.
    if cf.specs.is_empty() {
        return true;
    }

    let mut groups: BTreeMap<u8, Vec<(&CompiledSpec, bool)>> = BTreeMap::new();
    for spec in &cf.specs {
        let matched = evaluate_spec(spec, ctx);
        groups
            .entry(spec.kind.type_tag())
            .or_default()
            .push((spec, matched));
    }

    groups.values().all(|group| {
        let any_required_failed = group.iter().any(|(s, m)| s.required && !m);
        let all_failed = group.iter().all(|(_, m)| !m);
        !(any_required_failed || all_failed)
    })
}

/// Sum the scores of every CF that matches the candidate. Non-matching
/// CFs contribute 0 regardless of their score sign. Used by the Phase 6
/// auto_search integration as a single-call overlay on `base_score`.
///
/// Saturating addition: SEADEX_SCORE_BOOST is 10_000, individual TRaSH
/// CFs ship up to ±10_000, and user-authored CFs can carry arbitrary
/// scores. Naive `.sum()` would wrap on overflow and silently demote
/// every candidate below `minimum_score`, dropping the entire search.
pub(super) fn total_cf_score(cfs: &[CompiledCustomFormat], ctx: &EvalContext) -> i32 {
    cfs.iter()
        .filter(|cf| evaluate(cf, ctx))
        .map(|cf| cf.score)
        .fold(0i32, i32::saturating_add)
}

/// Variant of [`total_cf_score`] for callers that have the candidate's
/// fields directly rather than a full [`SearchResult`]. Internally
/// builds a minimal `SearchResult` shim — only `title`, `group`,
/// `size_bytes`, and `info_hash` are read by any spec kernel (see
/// [`evaluate_spec_kernel`]); the rest are filled with defaults.
///
/// Currently used by the RSS sync path, which carries `RssItem` (a
/// strict subset of `SearchResult`'s fields) and would otherwise
/// have no way into the CF evaluator. Without this helper RSS
/// auto-grab silently bypasses every CF the user has configured —
/// only the auto-search and upgrade-search paths would respect them.
pub fn total_cf_score_for_release(
    cfs: &[CompiledCustomFormat],
    classification: &super::ClassificationResult,
    title: &str,
    group: &str,
    size_bytes: i64,
    info_hash: &str,
    seadex_hashes: &std::collections::HashSet<String>,
) -> i32 {
    let result = super::SearchResult {
        match_provenance: None,
        title: title.to_string(),
        group: group.to_string(),
        size_bytes,
        info_hash: info_hash.to_string(),
        link: String::new(),
        magnet: String::new(),
        torrent: String::new(),
        size: String::new(),
        seeders: 0,
        leechers: 0,
        downloads: 0,
        resolution: String::new(),
        quality_label: String::new(),
        source: String::new(),
        web_kind: String::new(),
        is_remux: false,
        is_bdmv: false,
        is_batch: false,
        is_trusted: false,
        score: 0,
        score_breakdown: Vec::new(),
        upload_date: String::new(),
        indexer_id: None,
        indexer_name: String::new(),
    };
    let ctx = EvalContext {
        result: &result,
        classification,
        seadex_hashes,
    };
    total_cf_score(cfs, &ctx)
}

/// Same total as [`total_cf_score`], but also returns the per-CF
/// breakdown of every matching CF with a non-zero score contribution,
/// in `custom_formats.id` order (the natural iteration order of the
/// compiled cache). Used by the scoring debug-log path in
/// `auto_search.rs` so the user-facing log row can list exactly which
/// CFs fired on each candidate (§6.3 of the plan). Production scoring
/// stays on the scalar [`total_cf_score`] variant for speed — only the
/// debug-log path pays the allocation.
pub fn total_cf_score_with_breakdown(
    cfs: &[CompiledCustomFormat],
    ctx: &EvalContext,
) -> (i32, Vec<(String, i32)>) {
    let mut total: i32 = 0;
    let mut breakdown: Vec<(String, i32)> = Vec::new();
    for cf in cfs {
        if !evaluate(cf, ctx) {
            continue;
        }
        // saturating_add — see total_cf_score for the overflow rationale.
        total = total.saturating_add(cf.score);
        // Zero-score matches are meaningful for CF authoring but add
        // noise to the debug line — skip them per the plan §6.3 wording
        // "every CF that matched with a nonzero score contribution."
        if cf.score != 0 {
            breakdown.push((cf.name.clone(), cf.score));
        }
    }
    (total, breakdown)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::super::test_helpers::{candidate, classification, compile, ctx};
    use super::super::{
        ClassificationResult, CompiledCustomFormat, Resolution, SearchResult, Source, WebKind,
        compile_from_json, has_seadex_cf,
    };
    use super::{evaluate, total_cf_score, total_cf_score_with_breakdown};

    // ── evaluate_spec_kernel / evaluate_spec ─────────────────────────────

    #[test]
    fn release_title_kernel_matches_case_insensitive() {
        let cf = compile(
            r#"{
                "name": "x265",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();

        let hit = candidate("[MTBB] Show - 01 [BD 1080p X265]", "MTBB", 0, "");
        let cls = classification(Source::BluRay, Resolution::R1080p);
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));

        let miss = candidate("[Judas] Show - 01 [1080p]", "Judas", 0, "");
        assert!(!evaluate(&cf, &ctx(&miss, &cls, &hashes)));
    }

    #[test]
    fn release_group_spec_matches_parsed_group_not_title() {
        // The plan's "win over Sonarr": `^MTBB$` anchored against the
        // parsed group field, not the whole title. Sonarr's older
        // behavior would fail to match because the title carries
        // surrounding characters.
        let cf = compile(
            r#"{
                "name": "MTBB only",
                "specifications": [{
                    "implementation": "ReleaseGroupSpecification",
                    "fields": [{"name": "value", "value": "^MTBB$"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::BluRay, Resolution::R1080p);

        let hit = candidate("[MTBB] Kizumonogatari - 01 (BD 1080p)", "MTBB", 0, "");
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));

        let miss = candidate("[NoobSubs] Kizumonogatari - 01", "NoobSubs", 0, "");
        assert!(!evaluate(&cf, &ctx(&miss, &cls, &hashes)));
    }

    #[test]
    fn size_spec_strict_lower_inclusive_upper() {
        // Match Sonarr: `size > min && size <= max`.
        let cf = compile(
            r#"{
                "name": "5 to 20 GB",
                "specifications": [{
                    "implementation": "SizeSpecification",
                    "fields": [
                        {"name": "min", "value": 5},
                        {"name": "max", "value": 20}
                    ]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::BluRay, Resolution::R1080p);
        const GB: i64 = 1024 * 1024 * 1024;

        // Exactly at the lower bound: strict-greater means NO match.
        let at_min = candidate("x", "", 5 * GB, "");
        assert!(!evaluate(&cf, &ctx(&at_min, &cls, &hashes)));

        // Just above the lower bound: match.
        let just_above = candidate("x", "", 5 * GB + 1, "");
        assert!(evaluate(&cf, &ctx(&just_above, &cls, &hashes)));

        // Exactly at the upper bound: inclusive means match.
        let at_max = candidate("x", "", 20 * GB, "");
        assert!(evaluate(&cf, &ctx(&at_max, &cls, &hashes)));

        // Above the upper bound: no match.
        let above = candidate("x", "", 20 * GB + 1, "");
        assert!(!evaluate(&cf, &ctx(&above, &cls, &hashes)));
    }

    #[test]
    fn resolution_spec_uses_classification_not_filename() {
        // The whole point: filename-parsed `SearchResult::resolution`
        // string is ignored; we compare against the pipeline's
        // structured `ClassificationResult::resolution`.
        let cf = compile(
            r#"{
                "name": "1080p only",
                "specifications": [{
                    "implementation": "ResolutionSpecification",
                    "fields": [{"name": "value", "value": 1080}]
                }]
            }"#,
        );
        let hashes = HashSet::new();

        let cand = candidate("cosmetic 720p in filename", "", 0, "");
        // The candidate's filename says 720p but the classifier decided
        // R1080p — the CF trusts the classifier.
        let cls = classification(Source::Web, Resolution::R1080p);
        assert!(evaluate(&cf, &ctx(&cand, &cls, &hashes)));

        let cls_720 = classification(Source::Web, Resolution::R720p);
        assert!(!evaluate(&cf, &ctx(&cand, &cls_720, &hashes)));
    }

    #[test]
    fn source_spec_webdl_vs_webrip_vs_bare_web() {
        let webdl_cf = compile(
            r#"{
                "name": "WEB-DL",
                "specifications": [{
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 3}]
                }]
            }"#,
        );
        let webrip_cf = compile(
            r#"{
                "name": "WEBRip",
                "specifications": [{
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 4}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cand = candidate("x", "", 0, "");

        let mut webdl = classification(Source::Web, Resolution::R1080p);
        webdl.web_kind = WebKind::WebDl;
        let mut webrip = classification(Source::Web, Resolution::R1080p);
        webrip.web_kind = WebKind::WebRip;
        let bare = classification(Source::Web, Resolution::R1080p); // Unknown

        assert!(evaluate(&webdl_cf, &ctx(&cand, &webdl, &hashes)));
        assert!(!evaluate(&webdl_cf, &ctx(&cand, &webrip, &hashes)));
        // Issue #48: bare-WEB matches the WebDl CF spec. The label
        // layer collapsed WebDl and bare Web into a single "WEB"
        // render, so extending value-3 matching to cover both keeps
        // TRaSH `anime-web-tier-*` CFs functional for SubsPlease /
        // HorribleSubs / every release whose filename doesn't carry a
        // `WEB-DL` token. Previously bare-WEB matched nothing in the
        // source-spec space and those releases silently scored zero.
        assert!(evaluate(&webdl_cf, &ctx(&cand, &bare, &hashes)));

        assert!(evaluate(&webrip_cf, &ctx(&cand, &webrip, &hashes)));
        assert!(!evaluate(&webrip_cf, &ctx(&cand, &webdl, &hashes)));
        // WebRip stays strict — only explicit-WebRip releases match
        // the value-4 CF, so penalty CFs fire only on the lower-
        // quality sub-tier.
        assert!(!evaluate(&webrip_cf, &ctx(&cand, &bare, &hashes)));
    }

    #[test]
    fn source_spec_bluray_vs_bluray_raw() {
        let bluray_cf = compile(
            r#"{
                "name": "BluRay",
                "specifications": [{
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 6}]
                }]
            }"#,
        );
        let bluray_raw_cf = compile(
            r#"{
                "name": "BluRay RAW",
                "specifications": [{
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 7}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cand = candidate("x", "", 0, "");

        let encode = classification(Source::BluRay, Resolution::R1080p);
        let mut bdmv = classification(Source::BluRay, Resolution::R1080p);
        bdmv.is_bdmv = true;

        assert!(evaluate(&bluray_cf, &ctx(&cand, &encode, &hashes)));
        assert!(!evaluate(&bluray_cf, &ctx(&cand, &bdmv, &hashes))); // BDMV excluded by !is_bdmv

        assert!(evaluate(&bluray_raw_cf, &ctx(&cand, &bdmv, &hashes)));
        assert!(!evaluate(&bluray_raw_cf, &ctx(&cand, &encode, &hashes)));
    }

    #[test]
    fn seadex_best_matches_lowercased_hash_set() {
        let cf = compile(
            r#"{
                "name": "SeaDex",
                "specifications": [{
                    "implementation": "Ryokan.SeaDexBestSpecification",
                    "fields": []
                }]
            }"#,
        );
        let mut hashes = HashSet::new();
        hashes.insert("abc123".to_string());
        let cls = classification(Source::BluRay, Resolution::R1080p);

        // Exact match.
        let in_set = candidate("x", "", 0, "abc123");
        assert!(evaluate(&cf, &ctx(&in_set, &cls, &hashes)));

        // Uppercase match via lowercasing on compare.
        let in_set_upper = candidate("x", "", 0, "ABC123");
        assert!(evaluate(&cf, &ctx(&in_set_upper, &cls, &hashes)));

        // Miss.
        let not_in_set = candidate("x", "", 0, "def456");
        assert!(!evaluate(&cf, &ctx(&not_in_set, &cls, &hashes)));

        // Empty hash never matches.
        let no_hash = candidate("x", "", 0, "");
        assert!(!evaluate(&cf, &ctx(&no_hash, &cls, &hashes)));

        // Empty set never matches.
        let empty = HashSet::new();
        assert!(!evaluate(&cf, &ctx(&in_set, &cls, &empty)));
    }

    // ── Group-by-type DidMatch rule (§5.7.3 worked examples) ─────────────

    #[test]
    fn example_a_single_spec_match() {
        let cf = compile(
            r#"{
                "name": "A",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::BluRay, Resolution::R1080p);
        let hit = candidate("[MTBB] Show - 01 [BD 1080p x265]", "MTBB", 0, "");
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));
    }

    #[test]
    fn example_b_or_within_same_type() {
        // Two ReleaseTitle specs in the same group — OR within type.
        let cf = compile(
            r#"{
                "name": "B",
                "specifications": [
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "fields": [{"name": "value", "value": "x265"}]
                    },
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "fields": [{"name": "value", "value": "HEVC"}]
                    }
                ]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);
        // Only HEVC hits — the other spec in the same group is false,
        // but OR within group means the whole group still matches.
        let hit = candidate("[Judas] Show - 01 [HEVC]", "Judas", 0, "");
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));
    }

    #[test]
    fn example_c_and_across_groups() {
        // ReleaseTitle ∧ Size — different type_tags, groups must all
        // match.
        let cf = compile(
            r#"{
                "name": "C",
                "specifications": [
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "fields": [{"name": "value", "value": "x265"}]
                    },
                    {
                        "implementation": "SizeSpecification",
                        "fields": [
                            {"name": "min", "value": 5},
                            {"name": "max", "value": 20}
                        ]
                    }
                ]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::BluRay, Resolution::R1080p);
        const GB: i64 = 1024 * 1024 * 1024;

        // Both groups match.
        let hit = candidate("[MTBB] Show - 01 [BD 1080p x265]", "MTBB", 12 * GB, "");
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));

        // Title group fails — CF fails even though size is in range.
        let title_miss = candidate("[SubsPlease] Show - 01 (1080p)", "SubsPlease", 6 * GB, "");
        assert!(!evaluate(&cf, &ctx(&title_miss, &cls, &hashes)));

        // Size group fails — CF fails even though title matches.
        let size_miss = candidate(
            "[MTBB] Show - 01 [BD 1080p x265]",
            "MTBB",
            1_200_000_000,
            "",
        );
        assert!(!evaluate(&cf, &ctx(&size_miss, &cls, &hashes)));
    }

    #[test]
    fn example_d_required_hard_gate_within_group() {
        // Two ReleaseTitle specs in the same group, one with required=true.
        // When the required spec fails, the whole group fails even though
        // the OR partner matched — required=true is a hard gate.
        let cf = compile(
            r#"{
                "name": "D",
                "specifications": [
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "required": true,
                        "fields": [{"name": "value", "value": "x265"}]
                    },
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "fields": [{"name": "value", "value": "HEVC"}]
                    }
                ]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);
        let hit = candidate("[Judas] Show - 01 [HEVC]", "Judas", 0, "");
        // Without required=true this matches (Example B); with it, no.
        assert!(!evaluate(&cf, &ctx(&hit, &cls, &hashes)));
    }

    #[test]
    fn example_e_negate_inverts_kernel() {
        let cf = compile(
            r#"{
                "name": "E",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "negate": true,
                    "fields": [{"name": "value", "value": "NoobSubs"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);

        // Kernel matches "NoobSubs", negate flips to false, CF fails.
        let noob = candidate("[NoobSubs] Show - 01 [8bit].mp4", "NoobSubs", 0, "");
        assert!(!evaluate(&cf, &ctx(&noob, &cls, &hashes)));

        // Kernel miss, negate flips to true, CF matches.
        let clean = candidate("[MTBB] Show - 01", "MTBB", 0, "");
        assert!(evaluate(&cf, &ctx(&clean, &cls, &hashes)));
    }

    #[test]
    fn example_f_negate_plus_required_blacklist_pattern() {
        // The standard TRaSH blacklist shape.
        let cf = compile(
            r#"{
                "name": "F",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "negate": true,
                    "required": true,
                    "fields": [{"name": "value", "value": "NoobSubs"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);

        let clean = candidate("[MTBB] Show - 01", "MTBB", 0, "");
        assert!(evaluate(&cf, &ctx(&clean, &cls, &hashes)));

        let noob = candidate("[NoobSubs] Show - 01", "NoobSubs", 0, "");
        assert!(!evaluate(&cf, &ctx(&noob, &cls, &hashes)));
    }

    // ── Score summation ──────────────────────────────────────────────────

    #[test]
    fn total_cf_score_sums_only_matching_cfs() {
        let cf_x265 = compile(
            r#"{
                "name": "x265",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                }]
            }"#,
        );
        let mut cf_x265 = cf_x265;
        cf_x265.score = 500;

        let cf_anti_noob = compile(
            r#"{
                "name": "anti noob",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "negate": true,
                    "required": true,
                    "fields": [{"name": "value", "value": "NoobSubs"}]
                }]
            }"#,
        );
        let mut cf_anti_noob = cf_anti_noob;
        cf_anti_noob.score = -1000;

        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);

        // MTBB x265: matches x265 (+500), matches anti-noob (-0? no,
        // anti-noob matches → its score -1000 also adds).
        let mtbb = candidate("[MTBB] Show - 01 [x265]", "MTBB", 0, "");
        let score = total_cf_score(
            &[cf_x265.clone(), cf_anti_noob.clone()],
            &ctx(&mtbb, &cls, &hashes),
        );
        assert_eq!(score, 500 + (-1000));

        // NoobSubs (no x265): anti-noob fires negatively → CF doesn't
        // match → no -1000 contribution. x265 also doesn't match.
        let noob = candidate("[NoobSubs] Show - 01", "NoobSubs", 0, "");
        let score = total_cf_score(&[cf_x265, cf_anti_noob], &ctx(&noob, &cls, &hashes));
        assert_eq!(score, 0);
    }

    #[test]
    fn total_cf_score_empty_list_is_zero() {
        let hashes = HashSet::new();
        let cand = candidate("x", "", 0, "");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        assert_eq!(total_cf_score(&[], &ctx(&cand, &cls, &hashes)), 0);
    }

    // ── Vacuous-truth parity ─────────────────────────────────────────────

    #[test]
    fn empty_specs_cf_matches_every_release_strict_sonarr_parity() {
        // compile_from_json rejects this shape, but the evaluator still
        // has to handle it for strict Sonarr parity (pathologically
        // hand-edited state). Construct one manually.
        let cf = CompiledCustomFormat {
            id: 1,
            name: "empty".to_string(),
            score: 42,
            specs: vec![],
        };
        let hashes = HashSet::new();
        let cand = candidate("anything", "anyone", 0, "");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        assert!(evaluate(&cf, &ctx(&cand, &cls, &hashes)));
    }

    // ── Breakdown variant ────────────────────────────────────────────────

    #[test]
    fn total_cf_score_with_breakdown_returns_matching_contributions() {
        // Three CFs: two that match the candidate with non-zero scores,
        // one that doesn't match. Expect both matches in the breakdown
        // in CF order, and the total == sum of the matching scores.
        let cfs = vec![
            compile_from_json(
                r#"{"name":"x265","specifications":[{"implementation":"ReleaseTitleSpecification","fields":[{"name":"value","value":"x265"}]}]}"#,
                300,
                1,
            )
            .unwrap(),
            compile_from_json(
                r#"{"name":"flac","specifications":[{"implementation":"ReleaseTitleSpecification","fields":[{"name":"value","value":"flac"}]}]}"#,
                150,
                2,
            )
            .unwrap(),
            compile_from_json(
                r#"{"name":"noob","specifications":[{"implementation":"ReleaseGroupSpecification","fields":[{"name":"value","value":"^NoobSubs$"}]}]}"#,
                -1000,
                3,
            )
            .unwrap(),
        ];
        let cand = candidate("[MTBB] Show - 01 (BD x265 FLAC)", "MTBB", 0, "");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        let hashes = HashSet::new();
        let (total, breakdown) = total_cf_score_with_breakdown(&cfs, &ctx(&cand, &cls, &hashes));
        assert_eq!(total, 300 + 150);
        assert_eq!(breakdown.len(), 2);
        assert_eq!(breakdown[0].0, "x265");
        assert_eq!(breakdown[0].1, 300);
        assert_eq!(breakdown[1].0, "flac");
        assert_eq!(breakdown[1].1, 150);
    }

    #[test]
    fn total_cf_score_with_breakdown_skips_zero_score_matches() {
        // A CF that matches but has score=0 contributes to the total
        // correctly (trivially) but is omitted from the breakdown per
        // plan §6.3's "nonzero score contribution" wording.
        let cf = compile_from_json(
            r#"{"name":"zero","specifications":[{"implementation":"ReleaseTitleSpecification","fields":[{"name":"value","value":"x265"}]}]}"#,
            0,
            1,
        )
        .unwrap();
        let cand = candidate("Show x265", "", 0, "");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        let hashes = HashSet::new();
        let (total, breakdown) = total_cf_score_with_breakdown(&[cf], &ctx(&cand, &cls, &hashes));
        assert_eq!(total, 0);
        assert!(breakdown.is_empty());
    }

    // ── Default CF library ───────────────────────────────────────────────

    #[test]
    fn default_seadex_cf_fires_on_seadex_hash_match() {
        // The first default CF is the SeaDex boost. Compiling it and
        // feeding it a candidate whose info_hash is in the SeaDex set
        // should trigger the match with a +10000 contribution — that's
        // the score that dominates every other CF in the stacked
        // hierarchy from plan §7.1.
        const DEFAULTS: &str = include_str!("../../../static/default_custom_formats.json");
        let value: serde_json::Value = serde_json::from_str(DEFAULTS).unwrap();
        let first = &value.as_array().unwrap()[0];
        let score = first.get("score").unwrap().as_i64().unwrap() as i32;
        assert_eq!(score, 10000, "SeaDex CF score must be +10000");
        let cf = compile_from_json(&first.to_string(), score, 1).unwrap();

        let mut hashes = HashSet::new();
        hashes.insert("deadbeef".to_string());
        let cand = candidate("[MTBB] Show - 01 (BD x265 FLAC)", "MTBB", 0, "deadbeef");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        assert!(evaluate(&cf, &ctx(&cand, &cls, &hashes)));
        assert!(has_seadex_cf(&[cf]));
    }

    #[test]
    fn default_penalize_8bit_mp4_spares_subsplease_mkv() {
        // Regression guard for plan §7.3 CF #7: the two-spec
        // `required=true` AND pattern must NOT fire on SubsPlease mkvs
        // even though they're 8-bit (no 10-bit marker). The `.mp4`
        // extension check is what protects them.
        const DEFAULTS: &str = include_str!("../../../static/default_custom_formats.json");
        let value: serde_json::Value = serde_json::from_str(DEFAULTS).unwrap();
        // Find the penalize-8bit-mp4 entry by name rather than index so
        // a future reordering doesn't turn this into a silent false.
        let entry = value
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("8-bit mp4"))
                    .unwrap_or(false)
            })
            .expect("default set must include the 8-bit mp4 penalty CF");
        let cf = compile_from_json(
            &entry.to_string(),
            entry.get("score").unwrap().as_i64().unwrap() as i32,
            1,
        )
        .unwrap();

        let hashes = HashSet::new();
        let cls = classification(Source::Unknown, Resolution::Unknown);

        // SubsPlease weekly, .mkv container: should NOT match.
        let sp = candidate("[SubsPlease] ShowX - 01 (1080p).mkv", "SubsPlease", 0, "");
        assert!(
            !evaluate(&cf, &ctx(&sp, &cls, &hashes)),
            "SubsPlease mkv must be spared by the 8-bit mp4 penalty"
        );

        // NoobSubs-style 8-bit mp4: SHOULD match (hits both specs).
        let noob = candidate("[NoobSubs] ShowX - 01 (1080p 8bit).mp4", "NoobSubs", 0, "");
        assert!(
            evaluate(&cf, &ctx(&noob, &cls, &hashes)),
            "NoobSubs 8-bit mp4 must be caught by the penalty"
        );

        // A 10-bit mp4 (rare but possible): the required=true negate-10bit
        // spec fails post-negate, so the penalty should NOT fire.
        let tenbit_mp4 = candidate(
            "[SomeGroup] ShowX - 01 (1080p 10bit).mp4",
            "SomeGroup",
            0,
            "",
        );
        assert!(
            !evaluate(&cf, &ctx(&tenbit_mp4, &cls, &hashes)),
            "10-bit mp4 must survive the penalty"
        );
    }

    // ── Phase 9 integration: Kizumonogatari regression ──────────────────

    /// Load every CF from `static/default_custom_formats.json` into the
    /// compiled form the runtime actually uses. Factored into a helper so
    /// the Kizumonogatari regression test and the benchmark smoke test
    /// build the same CF set.
    fn load_default_cfs() -> Vec<CompiledCustomFormat> {
        const DEFAULTS: &str = include_str!("../../../static/default_custom_formats.json");
        let value: serde_json::Value = serde_json::from_str(DEFAULTS).unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let score = entry.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                compile_from_json(&entry.to_string(), score, (i + 1) as i64).unwrap()
            })
            .collect()
    }

    /// Helper that bundles the (title, group, classification) triple
    /// most integration-test candidates need. Mirrors the real-world
    /// pipeline: a Nyaa `SearchResult` plus a `ClassificationResult`
    /// from the source module. Keeping both in a tuple keeps each
    /// candidate row on one grep-able line in the test body.
    fn make_fixture(
        title: &str,
        group: &str,
        source: Source,
        resolution: Resolution,
        info_hash: &str,
    ) -> (SearchResult, ClassificationResult) {
        let cand = candidate(title, group, 8 * 1024 * 1024 * 1024, info_hash);
        let cls = classification(source, resolution);
        (cand, cls)
    }

    /// Sum the matching CF scores for a candidate against the default
    /// set, with an empty SeaDex set. Mirrors the exact code path the
    /// auto-search scorer uses in production, minus the `base_score`
    /// layer (which is classification-driven and orthogonal to CF
    /// ordering for this regression test).
    fn score_against_defaults(
        cfs: &[CompiledCustomFormat],
        cand: &SearchResult,
        cls: &ClassificationResult,
    ) -> i32 {
        let hashes = HashSet::new();
        total_cf_score(cfs, &ctx(cand, cls, &hashes))
    }

    /// The core regression test for the bug where an 8-bit NoobSubs
    /// mp4 with high seeders could tie or beat a 10-bit BD release
    /// because the hardcoded `+5` quality-marker bonus was identical
    /// for both. With the default CF set installed, the BD release
    /// must win by a wide margin and the 8-bit mp4 release must sit
    /// at the bottom of the score ordering.
    ///
    /// We assert strict ordering of the whole fixture, not just the
    /// head-to-head — any future regression (a mis-applied negation,
    /// a dropped required spec, a bad regex) flips the sort order and
    /// fails the test with a message that names the regressed pair.
    ///
    /// **Fixture titles are deliberately synthetic.** `ReleaseGroup`
    /// CFs evaluate against `SearchResult.group` (see
    /// `SpecKind::ReleaseGroup` at ~L309), not the title, so the
    /// group identity comes from the `group` argument to
    /// `make_fixture`. The title string only needs to carry the
    /// tokens that `ReleaseTitleSpecification` CFs match on
    /// (`10bit`, `x265`, `hevc`, `flac`, `.mp4`). Nothing here
    /// claims to be any specific group's real-world filename format.
    #[test]
    fn kizumonogatari_regression_cf_ordering() {
        let cfs = load_default_cfs();
        assert_eq!(cfs.len(), 8, "default CF set must be 8 CFs");

        // Expected totals are computed from the bundled CF scores:
        //   Tier-S BD   = 1200 (S-Tier) + 600 (BD source)
        //               + 300 (10-bit/x265) + 150 (FLAC) = 2250
        //   WEB HEVC    = 100 (Judas) + 300 (hevc/10-bit) = 400
        //   WEB plain   = 500 (SubsPlease)
        //   WEB neutral = 0 (matches no CF)
        //
        // SubsPlease > Judas+HEVC is the weekly-ranking invariant from
        // #47: SubsPlease's h264 release must outrank a Judas x265
        // release even after the HEVC bonus lands.
        //
        // Groups are attached via the SearchResult.group field (the
        // 2nd argument to make_fixture), not the title. Titles are
        // opaque synthetic token blobs.
        let tier_s_bd = make_fixture(
            "fixture-bd-1080p-10bit-x265-flac.mkv",
            "MTBB",
            Source::BluRay,
            Resolution::R1080p,
            "aaaa",
        );
        let web_hevc = make_fixture(
            "fixture-web-1080p-hevc-10bit.mkv",
            "Judas",
            Source::Web,
            Resolution::R1080p,
            "bbbb",
        );
        let web_plain = make_fixture(
            "fixture-web-1080p.mkv",
            "SubsPlease",
            Source::Web,
            Resolution::R1080p,
            "cccc",
        );
        let web_neutral = make_fixture(
            "fixture-web-1080p.mkv",
            "Erai-raws",
            Source::Web,
            Resolution::R1080p,
            "dddd",
        );
        // Build the flat list of (label, candidate, classification,
        // expected) tuples used for both scoring and ordering checks.
        let fixture: Vec<(&str, &SearchResult, &ClassificationResult, i32)> = vec![
            ("Tier-S BD", &tier_s_bd.0, &tier_s_bd.1, 2250),
            ("WEB plain", &web_plain.0, &web_plain.1, 500),
            ("WEB HEVC", &web_hevc.0, &web_hevc.1, 400),
            ("WEB neutral", &web_neutral.0, &web_neutral.1, 0),
        ];

        // Per-candidate score assertion — each row's total must match
        // plan §7.2's score values. If any of these fails, the
        // failing row's label appears in the assertion message.
        for (label, cand, cls, expected) in &fixture {
            let got = score_against_defaults(&cfs, cand, cls);
            assert_eq!(
                got, *expected,
                "candidate `{label}` scored {got}, expected {expected}"
            );
        }

        // Strict ordering assertion — the fixture is already in
        // expected descending order, so the sort result should equal
        // the fixture order.
        let mut sorted: Vec<(&&str, i32)> = fixture
            .iter()
            .map(|(label, cand, cls, _)| (label, score_against_defaults(&cfs, cand, cls)))
            .collect();
        sorted.sort_by_key(|s| std::cmp::Reverse(s.1));
        let labels_in_score_order: Vec<&str> = sorted.iter().map(|(label, _)| **label).collect();
        assert_eq!(
            labels_in_score_order,
            vec!["Tier-S BD", "WEB plain", "WEB HEVC", "WEB neutral",],
            "default CF set must produce the expected regression ordering"
        );
    }

    /// Post-#12 pin: HorribleSubs (and NoobSubs) WEB releases must
    /// score 0 against the bundled defaults — neither penalised nor
    /// rewarded. The old `-1000` casual-group CF was removed because
    /// it conflated "unmaintained but technically fine" (HorribleSubs)
    /// with "low-effort re-encode" (NoobSubs). Users who want a
    /// HorribleSubs penalty install the TRaSH Guides `anime-web-tier-05`
    /// CF, which is shipped as a fixture but not part of the bundled
    /// defaults.
    #[test]
    fn casual_groups_unpenalised_by_bundled_defaults() {
        let cfs = load_default_cfs();
        let horrible = make_fixture(
            "fixture-web-1080p.mkv",
            "HorribleSubs",
            Source::Web,
            Resolution::R1080p,
            "eeee",
        );
        let noob_8bit = make_fixture(
            "fixture-web-1080p-8bit.mp4",
            "NoobSubs",
            Source::Web,
            Resolution::R1080p,
            "ffff",
        );
        assert_eq!(
            score_against_defaults(&cfs, &horrible.0, &horrible.1),
            0,
            "HorribleSubs WEB must not be penalised by bundled defaults after #12"
        );
        // NoobSubs with 8-bit mp4 still trips the 8-bit mp4 penalty
        // (-500) which is independent of the casual-group CF.
        assert_eq!(
            score_against_defaults(&cfs, &noob_8bit.0, &noob_8bit.1),
            -500,
            "NoobSubs 8-bit mp4 must only incur the 8-bit mp4 penalty, not the removed casual-group penalty"
        );
    }

    // ── Phase 9 benchmark smoke (§9) ──────────────────────────────────

    #[test]
    fn benchmark_100_candidates_8_cfs_under_15ms() {
        // Plan §9: compile the 8-CF default set once, score 100
        // candidates, hard threshold 15 ms wall-clock (three-sigma
        // headroom over the < 5 ms target). If this blows past 15 ms
        // something has regressed — allocation in the hot path, a
        // regex re-compile per candidate, quadratic behavior in
        // `evaluate`, etc.
        //
        // Debug builds are ~3-5x slower than release, so we only run
        // the hard assertion under release. In debug we just run the
        // loop to catch outright panics and print the timing.
        let cfs = load_default_cfs();

        // Five synthetic-token candidates reused round-robin to hit
        // 100 iterations. The mix exercises every branch of every
        // default CF, including the §5.7.3 Example D AND-across-specs
        // path for the 8-bit mp4 penalty. Titles carry only the CF
        // regex tokens — they do not mimic any specific group's real
        // filename format.
        let base_candidates: Vec<(SearchResult, ClassificationResult)> = vec![
            make_fixture(
                "fixture-bd-1080p-10bit-x265-flac.mkv",
                "MTBB",
                Source::BluRay,
                Resolution::R1080p,
                "hash0",
            ),
            make_fixture(
                "fixture-web-1080p-hevc-10bit.mkv",
                "Judas",
                Source::Web,
                Resolution::R1080p,
                "hash1",
            ),
            make_fixture(
                "fixture-web-1080p.mkv",
                "SubsPlease",
                Source::Web,
                Resolution::R1080p,
                "hash2",
            ),
            make_fixture(
                "fixture-web-1080p.mkv",
                "Erai-raws",
                Source::Web,
                Resolution::R1080p,
                "hash3",
            ),
            make_fixture(
                "fixture-web-1080p-8bit.mp4",
                "NoobSubs",
                Source::Unknown,
                Resolution::R1080p,
                "hash4",
            ),
        ];

        let hashes = HashSet::new();
        let iterations = 100usize;
        let start = std::time::Instant::now();
        let mut checksum: i32 = 0;
        for i in 0..iterations {
            let (cand, cls) = &base_candidates[i % base_candidates.len()];
            checksum = checksum.wrapping_add(total_cf_score(&cfs, &ctx(cand, cls, &hashes)));
        }
        let elapsed = start.elapsed();
        // Force the optimizer to keep the loop body.
        std::hint::black_box(checksum);

        let ms = elapsed.as_secs_f64() * 1000.0;
        eprintln!("cf-benchmark: {iterations} candidates × 8 CFs in {ms:.3} ms");

        // Hard threshold only in release. Debug timing is informational.
        if !cfg!(debug_assertions) {
            assert!(
                elapsed < std::time::Duration::from_millis(15),
                "CF evaluation over {iterations} candidates took {ms:.3} ms, \
                 exceeding the 15 ms regression threshold from plan §9"
            );
        }
    }
}
