//! Source classification types, signal aggregation, and the pre-download
//! orchestrator.
//!
//! This module defines the primitive types used by the classification pipeline
//! and the aggregator that folds layer evidence into a final result. Individual
//! layer implementations live in sibling modules (`source_filename`,
//! `source_groups`, `source_description`, `source_temporal`,
//! `source_ffprobe`, and `source_dir`).
//!
//! The pipeline is split into two phases:
//! - **Pre-download** (layers 1–4): runs against a torrent title before the
//!   grab decision. Cheap, filename+DB only. Entry point:
//!   [`classify_release`]. Layer 2 (description scraping) does make one
//!   HTTP round trip to Nyaa, but only for the ambiguous tail that L1+L3
//!   couldn't resolve, and even that is cached by info_hash and
//!   rate-limited to one request per second process-wide. Layer 4
//!   (temporal inference) is a pure synchronous function and runs
//!   whenever the caller supplies airing metadata.
//! - **Post-download** (layers 5–6): runs against the on-disk file after
//!   import. Reads container metadata via ffprobe (L5) and walks the
//!   series directory for BD-exclusive markers like `BDMV/`, NCOP/NCED
//!   files, `Specials/` subdirectories (L6). Entry point:
//!   [`classify_post_download`]. Re-runs the cheap pre-download layers
//!   (L1/L3/L4 — L2 is skipped since there's no view URL after import)
//!   so the aggregator sees all evidence in one pass; ffprobe's observed
//!   resolution overrides the filename-parsed value when set.
//!
//! Both phases produce the same [`ClassificationResult`] type, so the two
//! phases can confirm/override each other via the same aggregator.

use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::services::source_description::classify_description;
use crate::services::source_dir::classify_dir;
use crate::services::source_ffprobe::classify_ffprobe;
use crate::services::source_filename::classify_filename;
use crate::services::source_groups::classify_group;
use crate::services::source_temporal::classify_temporal;

mod types;
pub use types::*;

// ───────────────────────────────────────────────────────────────────────────
// Aggregator
// ───────────────────────────────────────────────────────────────────────────

/// Strong-signal threshold: any single evidence at or above this confidence
/// wins outright without further arbitration.
const STRONG_THRESHOLD: f32 = 0.90;

/// Minimum lead of the winner over runner-up when adding up evidence. If the
/// top two are within this gap, it's treated as conflicting and flagged for
/// review rather than silently picking the winner.
const MIN_LEAD: f32 = 0.30;

/// Any signal above this threshold is considered "strong enough that a
/// conflict matters." Used by the review-detection rule.
const CONFLICT_THRESHOLD: f32 = 0.70;

/// Minimum total evidence mass to avoid falling back to Unknown.
const MIN_TOTAL: f32 = 0.50;

/// Per-origin saturation cap used inside rule 2 (per-source sum). Any single
/// origin's contribution to a source's sum is clamped at this value, so a
/// layer that fires multiple corroborating sub-signals (e.g. filename
/// emitting both a "BDRip" keyword *and* a FLAC audio codec hit for the
/// same source) can't alone swamp a contradicting ground-truth layer.
///
/// 1.3 preserves some intra-layer stacking — a title that correctly labels
/// itself with multiple agreeing tokens is legitimately stronger evidence
/// than a single token — while keeping the total below what an opposing
/// strong signal plus its own origin can still outweigh. Combined with the
/// ground-truth veto (see `aggregate` rule 5), a mislabeled filename with
/// stacked signals no longer silently beats an ffprobe codec contradiction.
const ORIGIN_MAX: f32 = 1.3;

/// Relative priority of a classification layer's origin string when
/// breaking ties in the aggregator. Filename (Layer 1) sits at the top
/// because the torrent title is the primary source of truth the user
/// and the release group both stand behind — the post-download layers
/// are ground-truth observations of the actual file bytes, but the
/// design intent (see `source_filename`'s "confidence budget" docblock)
/// is that filename drives the decision and the other layers
/// *supplement* its verdict rather than replace it.
///
/// Used only by rule 1's tie-breaker today. Rule 2's per-source sum
/// intentionally treats every origin equally so multiple weak
/// corroborating signals can still outvote a single weak filename guess
/// when filename didn't land a strong signal — that's the aggregator
/// working as designed.
fn origin_priority(origin: Origin) -> u8 {
    // Exhaustive match — adding a new Origin variant forces this to be
    // updated, preventing the silent "drops to 0" demotion that the
    // previous wildcard arm hid.
    match origin {
        Origin::Filename => 5,
        Origin::Ffprobe => 4,
        Origin::Dir => 4,
        Origin::Description => 3,
        Origin::Group => 2,
        Origin::Temporal => 1,
    }
}

/// Fold a list of evidence into a final classification result.
///
/// Rules, applied in order:
/// 1. If all strong signals (confidence ≥ `STRONG_THRESHOLD`, 0.90) agree
///    on a single source **and** the per-source sum from rule 2 picks
///    that same source as the leader, pick the highest-confidence strong
///    signal. On exact ties among strong signals, prefer the filename
///    layer origin — the torrent title is the primary source of truth
///    and other layers *supplement* it.
///
///    The rule-2 cross-check exists to defend against a single strong
///    layer signal overruling the aggregate weight of the other layers.
///    If a user manually seeds a mixed-source group at 0.95 BluRay while
///    filename/ffprobe/temporal all sub-strongly point at Web (≈
///    0.75–0.85 each), rule 2's summed evidence clearly favours Web —
///    so the strong single-signal shortcut would be *wrong* to fire.
///    Falling through lets the accumulated sub-strong evidence win the
///    rule-2 path, which is the aggregator's designed behaviour.
///
///    If the strong signals disagree among themselves (e.g. filename
///    says WEB-DL at 0.95, directory walk found BDMV/ at 0.95), rule 1
///    also falls through so rule 4 can detect the conflict and flag
///    `needs_review`. Without this guard, two disagreeing strong signals
///    would be silently resolved by source rank alone, masking a
///    conflict a human should see.
/// 2. Otherwise, sum confidences per source across all evidence — but first
///    clamp each *origin's* contribution per source at `ORIGIN_MAX`. This
///    prevents any single layer from stacking so many corroborating
///    sub-signals that it can't be outweighed by a contradicting
///    ground-truth layer. The source with the highest capped total is
///    provisionally the winner.
/// 3. If the total evidence mass for the best source is below
///    `MIN_TOTAL (0.50)`, classify as `Source::Unknown` with
///    `needs_review = true`.
/// 4. If the leader leads the runner-up by less than `MIN_LEAD (0.30)` and
///    at least one evidence ≥ `CONFLICT_THRESHOLD (0.70)` disagrees with the
///    leader, flag `needs_review = true` while still returning the best guess.
/// 5. Ground-truth veto: if any `ffprobe` or `dir` evidence at
///    `STRONG_THRESHOLD` (≥0.90) disagrees with the winning source, force
///    `needs_review = true` regardless of lead. Filename/description/group
///    are *labels* applied by humans; ffprobe/dir are *measurements* of
///    the actual bytes on disk. A strong label-vs-measurement conflict is
///    exactly the case a human should look at, and the capped sum can
///    otherwise let a mislabeled release with stacked intra-layer signals
///    silently outvote the ground truth.
/// 6. Otherwise: return the winner with `needs_review = false`.
///
/// Note: this function does not set `resolution` or `is_remux`. Those come
/// from the individual layer outputs (resolution is observed, not aggregated)
/// and are set by the orchestrating `classify_*` functions. The returned
/// result has `resolution = Unknown` and `is_remux = false`; callers
/// overwrite those fields.
pub fn aggregate(evidence: &[SourceEvidence]) -> ClassificationResult {
    if evidence.is_empty() {
        return ClassificationResult::unknown();
    }

    // Rule 2's per-source sum is computed *first* so rule 1 can
    // cross-check its strong-signal decision against it. We build a
    // `(source, origin) -> sum` map, clamp each bucket at ORIGIN_MAX,
    // then re-sum per source. A filename layer firing BDRip (0.95) +
    // FLAC (0.85) = 1.80 for BluRay gets clamped to 1.30 here, leaving
    // room for a contradicting ground-truth signal to still win or at
    // least trip rule 4/5.
    let mut per_origin: HashMap<(Source, Origin), f32> = HashMap::new();
    for e in evidence {
        *per_origin.entry((e.source, e.origin)).or_insert(0.0) += e.confidence;
    }
    let mut sums: HashMap<Source, f32> = HashMap::new();
    for ((source, _origin), sum) in per_origin {
        *sums.entry(source).or_insert(0.0) += sum.min(ORIGIN_MAX);
    }

    let mut ranked: Vec<(Source, f32)> = sums.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.rank().cmp(&a.0.rank()))
    });

    let (leader, evidence_mass) = ranked[0];
    let runner_sum = ranked.get(1).map(|(_, s)| *s).unwrap_or(0.0);
    let lead = evidence_mass - runner_sum;

    // Rule 1: strong single-signal shortcut. Only fires when every
    // strong signal agrees on the same source **and** the rule-2
    // per-source sum picks that source as the leader. The rule-2
    // cross-check catches cases where one strong single-layer signal
    // (e.g. a user-seeded group mapping) would otherwise override
    // multiple corroborating sub-strong signals from other layers. If
    // the cross-check fails, we fall through to rule 2+3+4+5 — the
    // same path disagreeing strong signals take — so the accumulated
    // sub-strong evidence decides.
    let strong: Vec<&SourceEvidence> = evidence
        .iter()
        .filter(|e| e.confidence >= STRONG_THRESHOLD)
        .collect();
    if !strong.is_empty() {
        let first_source = strong[0].source;
        let all_agree = strong.iter().all(|e| e.source == first_source);
        if all_agree && leader == first_source {
            // All strong signals point at the same source AND rule 2's
            // accumulated sums agree it's the leader. Pick the
            // highest-confidence strong signal; ties broken by origin
            // priority so filename (L1) wins over other origins when
            // equal. This makes the filename the effective tie-breaker
            // — the "primary source of truth" in the user's mental
            // model.
            let winner = strong
                .iter()
                .max_by(|a, b| {
                    a.confidence
                        .partial_cmp(&b.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| origin_priority(a.origin).cmp(&origin_priority(b.origin)))
                })
                .expect("strong is non-empty");
            return ClassificationResult {
                source: winner.source,
                resolution: Resolution::Unknown,
                is_remux: false,
                web_kind: WebKind::Unknown,
                is_bdmv: false,
                confidence: winner.confidence,
                needs_review: false,
                evidence: evidence.to_vec(),
                decision_rule: DecisionRule::Rule1Strong,
            };
        }
        // Either the strong signals disagree among themselves, or
        // rule 2's accumulated sum picks a different leader than
        // the strong source. Fall through to rule 2+4 so the
        // conflict is resolved by accumulated evidence and flagged
        // for review if the runner-up is close.
    }

    // Rule 3: total mass too weak, fall back to Unknown.
    if evidence_mass < MIN_TOTAL {
        return ClassificationResult {
            source: Source::Unknown,
            resolution: Resolution::Unknown,
            is_remux: false,
            web_kind: WebKind::Unknown,
            is_bdmv: false,
            confidence: evidence_mass,
            needs_review: true,
            evidence: evidence.to_vec(),
            decision_rule: DecisionRule::Rule3Weak,
        };
    }

    // Rule 4: detect strong conflict. If runner-up has a signal ≥
    // CONFLICT_THRESHOLD and the lead is small, flag for review.
    let has_strong_conflict = evidence
        .iter()
        .any(|e| e.confidence >= CONFLICT_THRESHOLD && e.source != leader);
    let rule4_fires = has_strong_conflict && lead < MIN_LEAD;

    // Rule 5: ground-truth veto. If any ffprobe/dir observation at or
    // above STRONG_THRESHOLD disagrees with the winner, flag for review
    // even when the lead is clean. The cap in rule 2 lets a label layer
    // win overall while an opposing measurement layer still carries a
    // strong individual signal — that combination is precisely the
    // "human should look" case.
    let rule5_fires = evidence.iter().any(|e| {
        e.confidence >= STRONG_THRESHOLD
            && e.source != leader
            && matches!(e.origin, Origin::Ffprobe | Origin::Dir)
    });

    // Rule precedence for the stored tag: rule 5 is more specific than
    // rule 4 (a strong ground-truth measurement beating the aggregator
    // is a distinct, more actionable failure mode than a close intra-
    // label conflict), so it wins when both fire. Both rules set
    // needs_review; the rule tag is purely for logging/audit.
    let (decision_rule, needs_review) = if rule5_fires {
        (DecisionRule::Rule5GroundTruthVeto, true)
    } else if rule4_fires {
        (DecisionRule::Rule4Conflict, true)
    } else {
        (DecisionRule::Rule2Sum, false)
    };

    // If the lead is large enough, it's a clean win regardless of conflicts.
    // If the lead is small but no strong conflict, still call it a win.
    ClassificationResult {
        source: leader,
        resolution: Resolution::Unknown,
        is_remux: false,
        web_kind: WebKind::Unknown,
        is_bdmv: false,
        // Normalize the confidence to a 0..1 share of evidence mass. A
        // leader with 1.5 out of 2.0 total mass gets 0.75; a dominant
        // leader with 1.8 out of 2.0 gets 0.9. NOTE: this is *not* a
        // probability — it's a relative share of the (capped) per-source
        // evidence mass. See the field docstring on `confidence`.
        confidence: evidence_mass / (evidence_mass + runner_sum).max(evidence_mass),
        needs_review,
        evidence: evidence.to_vec(),
        decision_rule,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Orchestrator
// ───────────────────────────────────────────────────────────────────────────

/// Per-listing Nyaa metadata used to drive Layers 2 and 4.
///
/// Callers that have access to the full Nyaa listing — auto-search, RSS,
/// upgrade detection, library handlers — pass this alongside the title so
/// the classifier can escalate to description parsing when the cheaper
/// layers can't reach a confident verdict, and so Layer 4 can reason
/// about batch vs. single-episode releases. Callers without a view URL
/// (on-disk filename reparsing, synthesized cutoffs) pass `None` and
/// Layer 2 is skipped entirely.
#[derive(Debug, Clone, Copy)]
pub struct NyaaContext<'a> {
    /// BitTorrent info_hash — stable, content-addressed cache key for
    /// description bodies.
    pub info_hash: &'a str,
    /// Full Nyaa view-page URL (`https://nyaa.si/view/{id}`). Only fetched
    /// on a cache miss, and only when L1+L3 came back unconfident.
    pub view_url: &'a str,
    /// Whether this listing is a batch/season pack (vs. a single weekly
    /// episode). Used by Layer 4 temporal inference.
    pub is_batch: bool,
}

/// Series-level metadata used to drive Layer 4 (temporal inference).
///
/// Callers that know the tracked-series row for the title being classified
/// pass this so the temporal layer can reason about airing status. Callers
/// that don't (e.g. pure filename reparsing of arbitrary on-disk files)
/// pass `None` and Layer 4 is skipped.
#[derive(Debug, Clone, Copy)]
pub struct SeriesContext<'a> {
    /// Raw AniList-style status string: `"RELEASING"`, `"FINISHED"`,
    /// `"CANCELLED"`, `"NOT_YET_RELEASED"`, `"HIATUS"`. Match is
    /// case-insensitive inside the layer.
    pub status: &'a str,
    /// Year the series started airing — used as a coarse fallback when
    /// `end_year` is not populated (e.g. currently-airing shows or
    /// providers that don't carry an end date).
    pub season_year: Option<i32>,
    /// Year the series finished airing, when known. Layer 4's
    /// "finished 1+ year ago" rules prefer this over `season_year` so a
    /// series that started in 2015 but only wrapped last year isn't
    /// treated as if it's been off the air for a decade. `None` for
    /// currently-airing shows and any metadata provider that doesn't
    /// expose an end date.
    pub end_year: Option<i32>,
}

/// Run the pre-download classification pipeline against a release title.
///
/// Fuses Layer 1 (filename tokens via anitomy), Layer 3 (release group
/// identity table), Layer 4 (temporal inference — when `series` is
/// provided), and — when L1+L3 come back unconfident — Layer 2 (Nyaa
/// description scraping). The post-download layers (ffprobe, directory
/// walk) are not wired in yet — they'll append additional evidence via
/// the same aggregator in later phases.
///
/// `resolution_hint` is an externally-supplied resolution string (typically
/// the `resolution` column scraped from a Nyaa listing). It is **only**
/// consulted for the resolution field when Layer 1 couldn't determine one
/// from the title tokens itself; it never participates in source aggregation.
/// This keeps the provenance of the two resolution sources distinct, so a
/// listing whose header claims 1080p while the title parses to 720p keeps
/// the title's value.
///
/// `nyaa` is the per-listing metadata needed for Layer 2 (description
/// fetch) and for the batch-vs-single dimension of Layer 4. Pass `None`
/// to skip the description-scraping escalation entirely. When supplied,
/// the classifier only consults Layer 2 if the L1+L3 pass didn't land a
/// confident verdict (i.e. `source == Unknown` or `needs_review == true`),
/// so confident classifications never touch the network.
///
/// `series` is the tracked-series context needed for Layer 4 (temporal
/// inference). Unlike Layer 2, Layer 4 is a pure synchronous function
/// and runs unconditionally when supplied — its signals are capped at
/// 0.75 confidence so they can't override the stronger layers, only act
/// as a tiebreaker. Pass `None` when no tracked-series row is available
/// (e.g. arbitrary on-disk filename reparsing).
pub async fn classify_release(
    db: &SqlitePool,
    title: &str,
    resolution_hint: Option<&str>,
    nyaa: Option<NyaaContext<'_>>,
    series: Option<SeriesContext<'_>>,
) -> ClassificationResult {
    let filename = classify_filename(title);

    let mut evidence = filename.evidence.clone();
    let mut group_web_kind = WebKind::Unknown;
    if let Some(group) = filename.release_group.as_deref()
        && let Some(group_cls) = classify_group(db, group).await
    {
        evidence.push(group_cls.evidence);
        group_web_kind = group_cls.web_kind;
    }

    // Layer 4 — temporal inference. Pure synchronous function with no I/O,
    // so unlike Layer 2 we run it up-front alongside L1+L3 whenever a
    // SeriesContext is supplied. The signal is deliberately weak (<=0.75)
    // so it acts as a tiebreaker rather than a decision maker.
    if let Some(series_ctx) = series {
        let is_batch = nyaa.map(|n| n.is_batch).unwrap_or(false);
        let today_year = current_year();
        if let Some(temporal_ev) = classify_temporal(
            series_ctx.status,
            series_ctx.season_year,
            series_ctx.end_year,
            is_batch,
            today_year,
        ) {
            evidence.push(temporal_ev);
        }
    }

    let mut result = aggregate(&evidence);

    // Layer 2 escalation: runs when the cheap L1+L3+L4 pass couldn't produce
    // a confident verdict, OR when the winning source is backed *only* by
    // filename-origin evidence while other layers emitted evidence for
    // different sources. The second case catches "confidently mislabeled"
    // releases that the old gate missed: a title with strong filename
    // tokens but no corroborating group/temporal signal is the exact
    // shape of a mislabeled release, and the description fetch is cheap
    // enough (cached per info_hash, rate-limited process-wide) to double-
    // check. Confident, corroborated classifications still skip the
    // network fetch entirely, preserving the fast path for the 90%+ of
    // releases that classify cleanly.
    if let Some(ctx) = nyaa {
        let only_filename_backs_winner = !evidence.is_empty()
            && evidence
                .iter()
                .filter(|e| e.source == result.source)
                .all(|e| e.origin == Origin::Filename)
            && evidence.iter().any(|e| e.origin != Origin::Filename);
        if result.source == Source::Unknown || result.needs_review || only_filename_backs_winner {
            let extra = classify_description(db, ctx.info_hash, ctx.view_url).await;
            if !extra.is_empty() {
                evidence.extend(extra);
                result = aggregate(&evidence);
            }
        }
    }

    // Resolution is observed, not aggregated. Prefer the filename parse —
    // it looked at the actual title tokens — and only fall back to the
    // external hint when Layer 1 came up empty.
    result.resolution = if filename.resolution != Resolution::Unknown {
        filename.resolution
    } else if let Some(hint) = resolution_hint {
        Resolution::from_str(hint)
    } else {
        Resolution::Unknown
    };
    result.is_remux = filename.is_remux;
    result.is_bdmv = filename.is_bdmv;
    // Web sub-classification only carries through when the aggregator
    // also landed on Source::Web — propagating WebKind onto a BluRay
    // verdict would be nonsensical. The filename's web_kind wins when
    // present (an explicit `WEB-DL` / `WEBRip` token is the most
    // specific signal we have); otherwise fall back to the group
    // table's hint (e.g. SubsPlease → WEB-DL) so SubsPlease-style
    // releases without any source token in the filename stop
    // labelling as the generic "WEB" tier.
    if result.source == Source::Web {
        result.web_kind = if filename.web_kind != WebKind::Unknown {
            filename.web_kind
        } else {
            group_web_kind
        };
    }

    log_classification("classify_release", title, &result);
    log_classification_to_db(db, "classify_release", title, &result).await;

    result
}

/// Run the post-download classification pipeline against a file that has
/// landed on disk. Used by `post_processing::import_torrent` right after
/// a successful `do_file_op` — at that point the file is at its final
/// library path and its sibling directory layout is stable.
///
/// Fuses the pre-download layers that are still cheap to re-run (Layer 1
/// filename, Layer 3 group, Layer 4 temporal) with the two post-download
/// layers (Layer 5 ffprobe, Layer 6 directory walk). Layer 2 is skipped
/// because the Nyaa view URL is no longer in hand once the torrent is
/// gone — and the post-download layers produce much stronger signals
/// anyway.
///
/// Arguments:
/// - `file_path` — absolute path to the landed video file (used by L5
///   to shell out to ffprobe).
/// - `series_root` — the top-level series directory under `media_root`
///   (used by L6 to look for `BDMV/`, `Scans/`, etc.). Pass `None` to
///   skip the directory walk — e.g. when classifying a one-off file
///   that isn't inside a managed library folder.
/// - `original_title` — the torrent title or filename to run L1 against.
///   Callers that have the original torrent title (via `grabbed_torrents`)
///   should pass that; callers reclassifying an externally-imported file
///   pass the filename itself.
/// - `series` — optional tracked-series context for Layer 4. Skipped
///   when absent.
/// - `is_batch` — whether the original grab was a batch/season pack.
///   Callers that have a `grabbed_torrents` row should read it from
///   `grabbed_torrents::get_is_batch_by_name`; callers classifying an
///   externally-imported file (that Ryokan never grabbed) pass `false`
///   because there's no batch signal to feed Layer 4 with.
///
/// Resolution observed by ffprobe takes precedence over the filename
/// parse — it's a direct measurement of the actual media, whereas L1 is
/// only inferring from tokens.
pub async fn classify_post_download(
    db: &SqlitePool,
    file_path: &std::path::Path,
    series_root: Option<&std::path::Path>,
    original_title: &str,
    series: Option<SeriesContext<'_>>,
    is_batch: bool,
) -> ClassificationResult {
    // L1 — filename tokens.
    let filename = classify_filename(original_title);
    let mut evidence = filename.evidence.clone();

    // L3 — release group identity.
    let mut group_web_kind = WebKind::Unknown;
    if let Some(group) = filename.release_group.as_deref()
        && let Some(group_cls) = classify_group(db, group).await
    {
        evidence.push(group_cls.evidence);
        group_web_kind = group_cls.web_kind;
    }

    // L4 — temporal inference (when we have series context). Post-download
    // now feeds back the original `is_batch` flag from `grabbed_torrents`
    // so the "finished 1+ year ago + batch → BluRay" rule still fires on
    // library-sweep reclassifies. Externally-imported files that Ryokan
    // never grabbed pass `false` because there's no batch signal.
    if let Some(series_ctx) = series
        && let Some(temporal_ev) = classify_temporal(
            series_ctx.status,
            series_ctx.season_year,
            series_ctx.end_year,
            is_batch,
            current_year(),
        )
    {
        evidence.push(temporal_ev);
    }

    // L5 — ffprobe stream analysis. Strongest post-download signal. Returns
    // an empty classification on any failure (missing binary, unreadable
    // file, malformed JSON) so we can always safely extend the evidence.
    let ffprobe = classify_ffprobe(db, file_path).await;
    evidence.extend(ffprobe.evidence);

    // L6 — directory walk. Only runs when a series_root is supplied; for
    // one-off file reclassifications we skip it.
    if let Some(root) = series_root {
        evidence.extend(classify_dir(root));
    }

    let mut result = aggregate(&evidence);

    // Resolution precedence post-download:
    //   1. ffprobe's measured dimensions (ground truth)
    //   2. filename parse
    //   3. Unknown
    result.resolution = if let Some(observed) = ffprobe.resolution {
        observed
    } else if filename.resolution != Resolution::Unknown {
        filename.resolution
    } else {
        Resolution::Unknown
    };
    result.is_remux = filename.is_remux;
    result.is_bdmv = filename.is_bdmv;
    // Same precedence as classify_release: filename's explicit WEB-DL
    // / WEBRip token wins, otherwise fall back to the group table's
    // hint.
    if result.source == Source::Web {
        result.web_kind = if filename.web_kind != WebKind::Unknown {
            filename.web_kind
        } else {
            group_web_kind
        };
    }

    log_classification("classify_post_download", original_title, &result);
    log_classification_to_db(db, "classify_post_download", original_title, &result).await;

    result
}

/// Log a completed classification at DEBUG, with the full evidence trail
/// flattened onto a single line so it's grep-friendly. Per the plan doc
/// Logging section, `needs_review=true` results additionally log at INFO
/// so they surface in the default log level without requiring debug filters.
///
/// Format:
/// ```text
/// [source] classify_release: "Foo" → BluRay/1080p (conf=0.92) [layer1:BluRay:0.95 "BD tag"] [group:BluRay:0.90 "SubsPlease"]
/// ```
fn log_classification(phase: &str, title: &str, result: &ClassificationResult) {
    let trail = evidence_trail(result);
    tracing::debug!(
        target: "ryokan::source",
        "[source] {}: {:?} → {} (conf={:.2}) [rule={}] {}",
        phase,
        title,
        result.label(),
        result.confidence,
        result.decision_rule.as_str(),
        trail,
    );
    if result.needs_review {
        tracing::info!(
            target: "ryokan::source",
            "[source] {}: {:?} needs review → {} (conf={:.2}) [rule={}] {}",
            phase,
            title,
            result.label(),
            result.confidence,
            result.decision_rule.as_str(),
            trail,
        );
    }
}

/// Mirror of [`log_classification`] that writes through the DB-backed
/// logger so per-decision traces show up in the UI logs page under the
/// `Quality` category. Called by the orchestrating `classify_*`
/// functions after they finalize a result. Always emits at the DEBUG
/// level — confident classifications are noisy, so the user has to
/// opt in by raising the log filter.
///
/// `needs_review` results additionally emit at INFO so they're visible
/// without flipping debug on, mirroring the tracing behavior. The detail
/// field carries the per-layer evidence breakdown formatted one entry
/// per line so the logs page wraps it cleanly.
async fn log_classification_to_db(
    db: &SqlitePool,
    phase: &str,
    title: &str,
    result: &ClassificationResult,
) {
    // Only persist to the DB log table when the row actually needs
    // the user's attention — needs_review is the one that drives the
    // Needs-Review UI. Routine classification chatter is handled by
    // the synchronous `log_classification` tracing helper called
    // alongside this, so we aren't losing visibility — just not paying
    // a per-candidate INSERT (50-200 candidates per search × 2 calls
    // per candidate through classify_release + classify_post_download).
    if !result.needs_review {
        return;
    }
    let detail = format!(
        "phase={}\nrule={}\nlabel={}\nconfidence={:.2}\nresolution={}\nis_remux={}\nis_bdmv={}\nweb_kind={}\nevidence:\n{}",
        phase,
        result.decision_rule.as_str(),
        result.label(),
        result.confidence,
        result.resolution.as_str(),
        result.is_remux,
        result.is_bdmv,
        result.web_kind.as_str(),
        result
            .evidence
            .iter()
            .map(|e| format!(
                "  - {}: {} (conf={:.2}) — {}",
                e.origin,
                e.source.as_str(),
                e.confidence,
                e.detail
            ))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let summary = format!("Classified \"{}\" → {}", title, result.label());
    crate::services::logger::info(
        db,
        crate::models::log::LogCategory::Quality,
        &format!("Needs review: {}", summary),
        &detail,
    )
    .await;
}

/// Formatted single-line evidence trail. Shared by both the tracing
/// logger and the DB logger so the on-disk format stays consistent.
fn evidence_trail(result: &ClassificationResult) -> String {
    result
        .evidence
        .iter()
        .map(|e| {
            format!(
                "[{}:{}:{:.2} {:?}]",
                e.origin,
                e.source.as_str(),
                e.confidence,
                e.detail
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Current calendar year as an `i32`, injected into Layer 4 so the
/// module itself stays a pure function (easier to unit-test). Uses
/// `chrono::Utc::now()` which is already a dependency of the project.
fn current_year() -> i32 {
    use chrono::Datelike;
    chrono::Utc::now().year()
}

// ───────────────────────────────────────────────────────────────────────────
// Sync classification helpers
// ───────────────────────────────────────────────────────────────────────────

/// Classify a release using filename evidence only (no DB lookup, no group
/// identity layer). Used by on-disk upgrade detection where we want to
/// classify many already-tagged episodes without doing one DB round-trip
/// per episode.
///
/// This is a weaker signal than [`classify_release`] — it can't see group
/// identity — but it's entirely synchronous and produces a result of the
/// same shape so callers can compare ranks directly.
pub fn classify_release_sync(title: &str, resolution_hint: Option<&str>) -> ClassificationResult {
    use crate::services::source_filename::classify_filename;
    let filename = classify_filename(title);
    let mut result = aggregate(&filename.evidence);
    result.resolution = if filename.resolution != Resolution::Unknown {
        filename.resolution
    } else if let Some(hint) = resolution_hint {
        Resolution::from_str(hint)
    } else {
        Resolution::Unknown
    };
    result.is_remux = filename.is_remux;
    result.is_bdmv = filename.is_bdmv;
    if result.source == Source::Web {
        result.web_kind = filename.web_kind;
    }
    result
}

/// Rehydrate a `ClassificationResult` from already-stored DB columns, e.g.
/// `episode_quality_tags`. Used by upgrade detection to compare an on-disk
/// episode's persisted classification against the incoming release without
/// re-parsing the original title.
///
/// The evidence trail is not stored in the DB, so the returned result has
/// an empty `evidence` vec. That's fine for rank comparison — evidence is
/// only consumed by auditing code paths.
pub fn classification_from_stored_full(
    source: &str,
    resolution: &str,
    is_remux: bool,
    is_bdmv: bool,
    web_kind: WebKind,
    confidence: f32,
    needs_review: bool,
) -> ClassificationResult {
    ClassificationResult {
        source: Source::from_str(source),
        resolution: Resolution::from_str(resolution),
        is_remux,
        web_kind,
        is_bdmv,
        confidence,
        needs_review,
        evidence: Vec::new(),
        // Rehydrated results aren't produced by the live aggregator,
        // so the firing rule isn't recoverable. Default to Empty.
        decision_rule: DecisionRule::Empty,
    }
}

/// Build a synthetic `ClassificationResult` representing the user's quality
/// cutoff. Used so upgrade-detection can compare real releases against the
/// cutoff using the same rank tuple comparison as everything else.
///
/// `is_remux`/`is_bdmv` feed into `bluray_tier()` so a "BD Remux" or
/// "BD-Raw (BDMV)" cutoff can out-rank a plain BluRay encode at the same
/// resolution. Only meaningful when `cutoff_source == Source::BluRay`.
pub fn cutoff_classification(
    cutoff_source: Source,
    cutoff_resolution: Resolution,
    is_remux: bool,
    is_bdmv: bool,
) -> ClassificationResult {
    ClassificationResult {
        source: cutoff_source,
        resolution: cutoff_resolution,
        is_remux,
        web_kind: WebKind::Unknown,
        is_bdmv,
        confidence: 1.0,
        needs_review: false,
        evidence: Vec::new(),
        // Synthesized cutoff sentinel — not a live classification,
        // so there's no firing rule to record.
        decision_rule: DecisionRule::Empty,
    }
}

/// Parse the stored `cutoff_source` config string into `(Source, is_remux,
/// is_bdmv)`. Recognizes the BluRay sub-tier values "bluray_remux" and
/// "bluray_bdmv" (persisted when the user picks "BD Remux" or "BD RAW" in
/// settings), and falls back to `Source::from_str` for the plain variants.
pub fn parse_cutoff_source(s: &str) -> (Source, bool, bool) {
    match s {
        "bluray_remux" => (Source::BluRay, true, false),
        "bluray_bdmv" => (Source::BluRay, false, true),
        other => (Source::from_str(other), false, false),
    }
}

/// Policy gate for quality upgrades. Returns `true` when `incoming`
/// is an allowed replacement for `existing`.
///
/// Rules:
/// 1. `incoming.rank() > existing.rank()` — strict tuple comparison
///    on `(resolution, source, bluray_tier, web_kind)`. Sidegrades and
///    downgrades are never upgrades.
/// 2. **No non-BDMV → BDMV crossings.** A plain BluRay or Remux row
///    on disk does not get auto-upgraded to a BDMV (disc-structure)
///    release even though the rank tuple ranks BDMV above Remux.
///    Rationale: BDMV file sizes are roughly 10× Remux, playback
///    through the disc menu structure is worse on every frontend
///    (Jellyfin, Plex, etc.), and most users treat Remux as the de-
///    facto ceiling. Manual search is the only escape hatch — it
///    bypasses this helper entirely because the user is grabbing
///    intentionally. Note that a user's quality cutoff being
///    `bluray_bdmv` does **not** relax this rule; the cutoff gate
///    runs upstream of this helper and only controls *whether* the
///    episode is eligible for upgrade at all, not *which* upgrades
///    are allowed. Fresh grabs of BDMV (no existing row on disk) are
///    unaffected because they route through the missing-episode
///    path, not the upgrade path.
///
/// Used by both RSS's per-item gate (`episode_is_upgradeable`) and the
/// `upgrade_search` background task's candidate verification. Keeping
/// the rule in one place ensures the two paths stay consistent.
pub fn is_valid_upgrade(existing: &ClassificationResult, incoming: &ClassificationResult) -> bool {
    if incoming.rank() <= existing.rank() {
        return false;
    }
    if !existing.is_bdmv && incoming.is_bdmv {
        return false;
    }
    true
}

#[cfg(test)]
mod upgrade_policy_tests {
    use super::*;

    fn c(source: Source, res: Resolution, is_remux: bool, is_bdmv: bool) -> ClassificationResult {
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

    #[test]
    fn upgrade_web_to_bluray_same_resolution_is_allowed() {
        let existing = c(Source::Web, Resolution::R1080p, false, false);
        let incoming = c(Source::BluRay, Resolution::R1080p, false, false);
        assert!(is_valid_upgrade(&existing, &incoming));
    }

    #[test]
    fn upgrade_plain_bluray_to_remux_is_allowed() {
        let existing = c(Source::BluRay, Resolution::R1080p, false, false);
        let incoming = c(Source::BluRay, Resolution::R1080p, true, false);
        assert!(is_valid_upgrade(&existing, &incoming));
    }

    #[test]
    fn upgrade_resolution_bump_is_allowed() {
        let existing = c(Source::BluRay, Resolution::R720p, false, false);
        let incoming = c(Source::Web, Resolution::R1080p, false, false);
        assert!(is_valid_upgrade(&existing, &incoming));
    }

    #[test]
    fn upgrade_remux_to_bdmv_is_blocked() {
        let existing = c(Source::BluRay, Resolution::R1080p, true, false);
        let incoming = c(Source::BluRay, Resolution::R1080p, false, true);
        assert!(!is_valid_upgrade(&existing, &incoming));
    }

    #[test]
    fn upgrade_plain_bluray_to_bdmv_is_blocked() {
        let existing = c(Source::BluRay, Resolution::R1080p, false, false);
        let incoming = c(Source::BluRay, Resolution::R1080p, false, true);
        assert!(!is_valid_upgrade(&existing, &incoming));
    }

    #[test]
    fn upgrade_bdmv_to_better_bdmv_is_allowed() {
        let existing = c(Source::BluRay, Resolution::R1080p, false, true);
        let incoming = c(Source::BluRay, Resolution::R2160p, false, true);
        assert!(is_valid_upgrade(&existing, &incoming));
    }

    #[test]
    fn sidegrade_is_blocked() {
        let existing = c(Source::BluRay, Resolution::R1080p, true, false);
        let incoming = c(Source::BluRay, Resolution::R1080p, true, false);
        assert!(!is_valid_upgrade(&existing, &incoming));
    }

    #[test]
    fn downgrade_is_blocked() {
        let existing = c(Source::BluRay, Resolution::R1080p, true, false);
        let incoming = c(Source::Web, Resolution::R1080p, false, false);
        assert!(!is_valid_upgrade(&existing, &incoming));
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Finished-series filters
// ───────────────────────────────────────────────────────────────────────────

/// Filename-only BluRay heuristic. Runs Layer 1 (anitomy tokens) and the
/// aggregator without touching the DB, then checks whether the dominant
/// source is BluRay. Used by inline collection-time filters that need a
/// yes/no answer before the full DB-backed `classify_release` pass runs.
///
/// This is intentionally a weaker signal than `classify_release` — it can't
/// see group identity — but it's cheap enough to run inline during collection.
pub fn looks_like_bluray_filename(title: &str) -> bool {
    use crate::services::source_filename::classify_filename;
    let filename = classify_filename(title);
    // If the filename tokens alone already resolve to BluRay, that's a yes.
    // Otherwise, aggregate the evidence just in case (an ambiguous pile of
    // weak BluRay signals might still aggregate to BluRay).
    let agg = aggregate(&filename.evidence);
    agg.source == Source::BluRay
}

/// Does this classification pass a "BluRay only" finished-series filter?
/// Unknown sources are allowed through so that releases we couldn't confidently
/// classify aren't silently filtered out — they'll still compete on score.
pub fn passes_bd_only_filter(c: &ClassificationResult) -> bool {
    matches!(c.source, Source::BluRay | Source::Unknown)
}

// ───────────────────────────────────────────────────────────────────────────
// Scoring
// ───────────────────────────────────────────────────────────────────────────

/// Score a classification result against the user's preferred/cutoff
/// source+resolution. Returns a score delta to be added to the baseline from
/// `scoring::score_result_with_sub_pref`.
///
/// The two dimensions (source and resolution) are scored separately and
/// summed, with resolution carrying a heavier weight because it dominates the
/// ranking tuple in `ClassificationResult::rank`.
///
/// Scoring rules:
/// - Unknown on both dimensions → small penalty, don't filter out.
/// - At or above preferred → bonus, exact match earns extra.
/// - Below preferred → penalty proportional to the gap.
/// - At or above cutoff → small bonus.
/// - Remux is a small premium modifier when BluRay is the preferred source.
/// - `needs_review` classifications get a small penalty so confident
///   alternatives are preferred when available.
pub fn score_classification(
    c: &ClassificationResult,
    preferred_source: Source,
    preferred_resolution: Resolution,
    cutoff_source: Source,
    cutoff_resolution: Resolution,
) -> i32 {
    // Completely unknown release — keep the legacy -5 behavior.
    if c.source == Source::Unknown && c.resolution == Resolution::Unknown {
        return -5;
    }

    let mut score: i32 = 0;

    // ── Resolution (dominates) ────────────────────────────────────────────
    if c.resolution != Resolution::Unknown {
        let det = c.resolution.rank() as i32;
        let pref = preferred_resolution.rank() as i32;
        if det >= pref {
            score += 25;
            if det == pref {
                score += 15;
            }
        } else {
            let gap = pref - det;
            score -= 10 + gap * 10;
        }
        if det >= cutoff_resolution.rank() as i32 {
            score += 10;
        }
    } else {
        score -= 10;
    }

    // ── Source ────────────────────────────────────────────────────────────
    if c.source != Source::Unknown {
        let det = c.source.rank() as i32;
        let pref = preferred_source.rank() as i32;
        if det >= pref {
            score += 15;
            if det == pref {
                score += 10;
            }
        } else {
            let gap = pref - det;
            score -= 5 + gap * 5;
        }
        if det >= cutoff_source.rank() as i32 {
            score += 5;
        }
    } else {
        score -= 5;
    }

    // ── Remux / BDMV premiums ─────────────────────────────────────────────
    // Both are only a bonus when the user actually wants BluRay-grade
    // fidelity. Penalizing them otherwise would be wrong (still valid
    // BluRay sources, just overkill), so we simply don't reward them.
    // BDMV gets a slightly larger bump than Remux because it's the
    // higher-fidelity tier in the BluRay sub-ranking.
    if preferred_source.rank() >= Source::BluRay.rank() {
        if c.is_bdmv {
            score += 7;
        } else if c.is_remux {
            score += 5;
        }
    }

    // ── Web sub-tier preference ───────────────────────────────────────────
    // When the user prefers WEB-grade quality, lightly favor WEB-DL over
    // WEBRip — the underlying tier ordering already reflects this in
    // `rank()`, but the score path is what `score_result` adds to, so we
    // mirror it here so a WEB-DL release nudges ahead in mixed result
    // sets sorted by score rather than by classification rank.
    if c.source == Source::Web {
        match c.web_kind {
            WebKind::WebDl => score += 3,
            WebKind::WebRip => score -= 1,
            WebKind::Unknown => {}
        }
    }

    // ── Needs-review penalty ──────────────────────────────────────────────
    // Prefer confidently-classified releases when they exist. Small value so
    // it can't flip a clearly better release.
    if c.needs_review {
        score -= 3;
    }

    score
}

// ───────────────────────────────────────────────────────────────────────────
// Shared text helpers
// ───────────────────────────────────────────────────────────────────────────

/// Case-insensitive whole-word search. Returns `true` when `needle` appears
/// in `haystack` with non-alphabetic characters (or string boundaries) on
/// both sides.
///
/// Shared between Layer 1 (filename tokens) and Layer 6 (directory walk) so
/// the two call sites can't drift apart — both layers need a "does this
/// token appear as a whole word?" check, and both want digits, punctuation,
/// and whitespace to count as boundaries. That's why the boundary test is
/// `!is_ascii_alphabetic` rather than `!is_ascii_alphanumeric`: filename tags
/// like `x264_2flac` should match `flac` (digit before is a boundary), and
/// directory entries like `NCOP01.mkv` should match `NCOP` (digit after is a
/// boundary). Alphabetic neighbors stay non-boundary so `SyncopationVol01`
/// doesn't match `NCOP`.
pub(crate) fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if hb.len() < nb.len() {
        return false;
    }
    let mut i = 0;
    while i + nb.len() <= hb.len() {
        if hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
            let left_ok = i == 0 || !hb[i - 1].is_ascii_alphabetic();
            let right_ok = i + nb.len() == hb.len() || !hb[i + nb.len()].is_ascii_alphabetic();
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(src: Source, conf: f32, origin: Origin) -> SourceEvidence {
        SourceEvidence::new(src, conf, origin, "")
    }

    #[test]
    fn rank_ordering_matches_expectations() {
        // Source ordering.
        assert!(Source::BluRay.rank() > Source::Web.rank());
        assert!(Source::Web.rank() > Source::Dvd.rank());
        assert!(Source::Dvd.rank() > Source::Hdtv.rank());
        assert!(Source::Hdtv.rank() > Source::Tv.rank());

        // Resolution ordering.
        assert!(Resolution::R2160p.rank() > Resolution::R1080p.rank());
        assert!(Resolution::R1080p.rank() > Resolution::R720p.rank());
    }

    #[test]
    fn resolution_from_int_maps_sonarr_values() {
        assert_eq!(Resolution::from_int(480), Resolution::R480p);
        assert_eq!(Resolution::from_int(576), Resolution::R576p);
        assert_eq!(Resolution::from_int(720), Resolution::R720p);
        assert_eq!(Resolution::from_int(1080), Resolution::R1080p);
        assert_eq!(Resolution::from_int(2160), Resolution::R2160p);
        // Sonarr's 360 and 540 variants fold to Unknown — Ryokan doesn't
        // classify those tiers and anime rarely ships at either height.
        assert_eq!(Resolution::from_int(360), Resolution::Unknown);
        assert_eq!(Resolution::from_int(540), Resolution::Unknown);
        // Nonsense input, including Sonarr's sentinel 0, also folds to Unknown.
        assert_eq!(Resolution::from_int(0), Resolution::Unknown);
        assert_eq!(Resolution::from_int(-1), Resolution::Unknown);
        assert_eq!(Resolution::from_int(9999), Resolution::Unknown);
    }

    #[test]
    fn classification_rank_prefers_resolution_over_source() {
        // Web-1080p should outrank BluRay-720p (resolution dominates).
        let web_1080 = ClassificationResult {
            source: Source::Web,
            resolution: Resolution::R1080p,
            is_remux: false,
            web_kind: WebKind::Unknown,
            is_bdmv: false,
            confidence: 1.0,
            needs_review: false,
            evidence: vec![],
            decision_rule: DecisionRule::Empty,
        };
        let bd_720 = ClassificationResult {
            source: Source::BluRay,
            resolution: Resolution::R720p,
            is_remux: false,
            web_kind: WebKind::Unknown,
            is_bdmv: false,
            confidence: 1.0,
            needs_review: false,
            evidence: vec![],
            decision_rule: DecisionRule::Empty,
        };
        assert!(web_1080.rank() > bd_720.rank());
    }

    #[test]
    fn bluray_tier_orders_plain_lt_remux_lt_bdmv() {
        let plain = ClassificationResult {
            source: Source::BluRay,
            resolution: Resolution::R1080p,
            is_remux: false,
            is_bdmv: false,
            web_kind: WebKind::Unknown,
            confidence: 1.0,
            needs_review: false,
            evidence: vec![],
            decision_rule: DecisionRule::Empty,
        };
        let remux = ClassificationResult {
            is_remux: true,
            ..plain.clone()
        };
        let bdmv = ClassificationResult {
            is_bdmv: true,
            ..plain.clone()
        };
        // Same source/resolution, the bluray_tier slot in rank() breaks the tie.
        assert!(remux.rank() > plain.rank(), "remux > plain encode");
        assert!(bdmv.rank() > remux.rank(), "BDMV > remux");
        // BDMV wins even when both flags are set.
        let both = ClassificationResult {
            is_remux: true,
            is_bdmv: true,
            ..plain.clone()
        };
        assert_eq!(both.bluray_tier(), 2);
        assert_eq!(both.label(), "BD-1080p RAW");
    }

    #[test]
    fn web_dl_outranks_webrip_at_same_resolution() {
        let webrip = ClassificationResult {
            source: Source::Web,
            resolution: Resolution::R1080p,
            is_remux: false,
            is_bdmv: false,
            web_kind: WebKind::WebRip,
            confidence: 1.0,
            needs_review: false,
            evidence: vec![],
            decision_rule: DecisionRule::Empty,
        };
        let webdl = ClassificationResult {
            web_kind: WebKind::WebDl,
            ..webrip.clone()
        };
        assert!(webdl.rank() > webrip.rank());
        assert_eq!(webrip.label(), "WEBRip-1080p");
        // WebDl collapses to bare "WEB" at the label layer (issue #48);
        // the rank tiebreaker above still distinguishes it from
        // Unknown so CF matching and scoring continue to work.
        assert_eq!(webdl.label(), "WEB-1080p");
    }

    #[test]
    fn aggregate_empty_evidence_is_unknown() {
        let result = aggregate(&[]);
        assert_eq!(result.source, Source::Unknown);
        assert!(result.needs_review);
    }

    #[test]
    fn aggregate_rule_1_strong_signal_wins_immediately() {
        // Single strong signal ≥ 0.90 short-circuits the rest.
        let evidence = vec![
            ev(Source::Web, 0.95, Origin::Filename),
            ev(Source::BluRay, 0.40, Origin::Group),
            ev(Source::BluRay, 0.30, Origin::Temporal),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web);
        assert!(!result.needs_review);
        // Confidence should be the strong signal's own confidence.
        assert!((result.confidence - 0.95).abs() < 1e-4);
    }

    #[test]
    fn aggregate_rule_2_highest_sum_wins_with_clear_lead() {
        // No single strong signal, but two weak Web signals outweigh one
        // moderate BluRay signal by more than MIN_LEAD.
        let evidence = vec![
            ev(Source::Web, 0.60, Origin::Filename),
            ev(Source::Web, 0.55, Origin::Group),
            ev(Source::BluRay, 0.50, Origin::Temporal),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web);
        assert!(!result.needs_review);
    }

    #[test]
    fn aggregate_rule_3_conflict_flags_for_review() {
        // Runner-up has a signal ≥ CONFLICT_THRESHOLD (0.70) and the lead is
        // less than MIN_LEAD (0.30) — should pick a winner but flag for
        // review.
        let evidence = vec![
            ev(Source::Web, 0.75, Origin::Filename),
            ev(Source::BluRay, 0.70, Origin::Ffprobe),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web); // Higher sum wins.
        assert!(result.needs_review, "conflict should trigger review");
    }

    #[test]
    fn aggregate_rule_4_all_weak_falls_to_unknown() {
        // Every signal below MIN_TOTAL (0.50) — total mass too weak.
        let evidence = vec![
            ev(Source::Web, 0.20, Origin::Temporal),
            ev(Source::BluRay, 0.15, Origin::Temporal),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Unknown);
        assert!(result.needs_review);
    }

    #[test]
    fn aggregate_clean_win_despite_weak_conflict() {
        // Strong leader, weak opposing signal — no review needed.
        let evidence = vec![
            ev(Source::Web, 0.95, Origin::Filename),
            ev(Source::BluRay, 0.30, Origin::Temporal),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web);
        assert!(!result.needs_review);
    }

    #[test]
    fn aggregate_rule_1_filename_wins_tie_against_ffprobe() {
        // Two strong signals agreeing on BluRay but tied at 0.95.
        // Filename origin should win the tiebreaker — "filename is the
        // primary source of truth."
        let evidence = vec![
            ev(Source::BluRay, 0.95, Origin::Ffprobe),
            ev(Source::BluRay, 0.95, Origin::Filename),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::BluRay);
        // Both strong signals agreed, so the short-circuit fires and
        // the winner's confidence is the per-signal confidence, not a
        // per-source sum.
        assert!((result.confidence - 0.95).abs() < 1e-4);
        assert!(!result.needs_review);
    }

    #[test]
    fn aggregate_origin_cap_prevents_filename_stacking_from_swamping_ffprobe() {
        // A mislabeled release: filename emits a strong keyword +
        // stacked audio codec evidence for BluRay, ffprobe finds a
        // strong Web signal. Both categories have strong signals, so
        // rule 1 falls through to rule 2. Without the per-origin cap,
        // filename would sum to 0.95 + 0.85 = 1.80 for BluRay and
        // outweigh Web's 0.90 by 0.90 — a clean win with no review.
        // With ORIGIN_MAX = 1.3, filename caps to 1.30 and the
        // ground-truth veto (rule 5) flags the ffprobe conflict.
        let evidence = vec![
            ev(Source::BluRay, 0.95, Origin::Filename), // strong keyword
            ev(Source::BluRay, 0.85, Origin::Filename), // stacked audio codec
            ev(Source::Web, 0.90, Origin::Ffprobe),     // strong ground truth
        ];
        let result = aggregate(&evidence);
        // BluRay still wins the sum (1.30 vs 0.90 — a 0.40 clean lead).
        assert_eq!(result.source, Source::BluRay);
        // But the veto fires because ffprobe's Web signal is strong.
        assert!(
            result.needs_review,
            "strong ffprobe disagreement must flag review even when capped filename sum wins"
        );
    }

    #[test]
    fn aggregate_ground_truth_veto_fires_on_dir_disagreement() {
        // Filename says Web strongly; directory walk found a BDMV/
        // folder at ground truth. Rule 1 falls through (disagreeing
        // strong signals). Rule 2: Web sums to 1.65, BluRay sums to
        // 0.95 — Web wins by 0.70, a clean lead. But rule 5's veto
        // fires because the disagreeing signal is from the dir layer.
        let evidence = vec![
            ev(Source::Web, 0.95, Origin::Filename),
            ev(Source::Web, 0.70, Origin::Group),
            ev(Source::BluRay, 0.95, Origin::Dir),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web);
        assert!(
            result.needs_review,
            "strong dir disagreement must flag review"
        );
    }

    #[test]
    fn aggregate_ground_truth_veto_does_not_fire_on_weak_disagreement() {
        // A ground-truth layer with a sub-threshold signal (< 0.90) does
        // not trigger the veto — only strong measurements do.
        let evidence = vec![
            ev(Source::BluRay, 0.85, Origin::Filename),
            ev(Source::BluRay, 0.70, Origin::Group),
            ev(Source::Web, 0.80, Origin::Ffprobe), // below STRONG_THRESHOLD
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::BluRay);
        assert!(
            !result.needs_review,
            "sub-threshold ground-truth should not fire the veto"
        );
    }

    #[test]
    fn aggregate_origin_cap_does_not_restrict_cross_origin_corroboration() {
        // Two different origins each emitting at 0.85 for the same
        // source are not capped — they're different origins, so their
        // contributions stack as normal. Verifies the cap is per-origin,
        // not a global ceiling.
        let evidence = vec![
            ev(Source::BluRay, 0.85, Origin::Filename),
            ev(Source::BluRay, 0.85, Origin::Ffprobe),
            ev(Source::Web, 0.40, Origin::Temporal),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::BluRay);
        assert!(!result.needs_review);
    }

    #[test]
    fn aggregate_decision_rule_tagged_per_branch() {
        // Empty evidence → Empty.
        assert_eq!(aggregate(&[]).decision_rule, DecisionRule::Empty);

        // Single strong signal → Rule1Strong.
        let rule1 = aggregate(&[ev(Source::Web, 0.95, Origin::Filename)]);
        assert_eq!(rule1.decision_rule, DecisionRule::Rule1Strong);

        // Clean sum win with no strong signals → Rule2Sum.
        let rule2 = aggregate(&[
            ev(Source::Web, 0.60, Origin::Filename),
            ev(Source::Web, 0.55, Origin::Group),
            ev(Source::BluRay, 0.50, Origin::Temporal),
        ]);
        assert_eq!(rule2.decision_rule, DecisionRule::Rule2Sum);

        // Total mass below MIN_TOTAL → Rule3Weak.
        let rule3 = aggregate(&[
            ev(Source::Web, 0.20, Origin::Temporal),
            ev(Source::BluRay, 0.15, Origin::Temporal),
        ]);
        assert_eq!(rule3.decision_rule, DecisionRule::Rule3Weak);

        // Close strong conflict, no ground-truth layer → Rule4Conflict.
        let rule4 = aggregate(&[
            ev(Source::Web, 0.75, Origin::Filename),
            ev(Source::BluRay, 0.70, Origin::Group),
        ]);
        assert_eq!(rule4.decision_rule, DecisionRule::Rule4Conflict);

        // Ground-truth veto beats rule 4 when both would fire.
        let rule5 = aggregate(&[
            ev(Source::BluRay, 0.95, Origin::Filename),
            ev(Source::BluRay, 0.85, Origin::Filename),
            ev(Source::Web, 0.90, Origin::Ffprobe),
        ]);
        assert_eq!(rule5.decision_rule, DecisionRule::Rule5GroundTruthVeto);
    }

    #[test]
    fn aggregate_rule_1_yields_to_rule_2_when_sums_overrule_strong_signal() {
        // The user-seeded mixed-source case: a group layer fires a strong
        // BluRay signal (0.95) from a user-edited group_source_map row,
        // but ffprobe and temporal both emit sub-strong Web signals
        // (0.85 + 0.75 = 1.60 summed). Before the rule-1 cross-check,
        // rule 1 short-circuited on the lone strong signal and returned
        // BluRay regardless, silently burying the (correct) Web evidence.
        // Now rule 1 checks rule 2's leader before firing — if rule 2
        // picks a different source, rule 1 falls through and rule 2
        // decides.
        let evidence = vec![
            ev(Source::BluRay, 0.95, Origin::Group),
            ev(Source::Web, 0.85, Origin::Ffprobe),
            ev(Source::Web, 0.75, Origin::Temporal),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web);
        // Clean rule-2 win: Web sums to 1.60, BluRay to 0.95, lead 0.65
        // which is comfortably above MIN_LEAD (0.30). ffprobe's 0.85 is
        // below STRONG_THRESHOLD so rule 5 stays silent. No review flag.
        assert!(
            !result.needs_review,
            "clean rule-2 win with sub-threshold ground truth should not flag review: {:#?}",
            result.evidence,
        );
        assert_eq!(result.decision_rule, DecisionRule::Rule2Sum);
    }

    #[test]
    fn aggregate_rule_1_disagreement_falls_through_and_flags_review() {
        // Two strong signals disagreeing — filename says WEB-DL at 0.95,
        // directory walk found BDMV/ at 0.95. Previously rule 1 silently
        // picked BluRay via source-rank tiebreaker. Now it falls through
        // to rule 2+4 so the conflict gets flagged.
        let evidence = vec![
            ev(Source::Web, 0.95, Origin::Filename),
            ev(Source::BluRay, 0.95, Origin::Dir),
        ];
        let result = aggregate(&evidence);
        // Rule 2 ties the sums, rule 4 detects the strong opposing
        // signal with zero lead, and flags for review.
        assert!(
            result.needs_review,
            "disagreeing strong signals must flag needs_review"
        );
    }

    // ─── Boundary tests for aggregator thresholds ──────────────────────────
    //
    // The aggregator is full of `>=` / `<` comparisons against tuned
    // constants (STRONG_THRESHOLD=0.90, CONFLICT_THRESHOLD=0.70,
    // MIN_LEAD=0.30, MIN_TOTAL=0.50, ORIGIN_MAX=1.30). A refactor that
    // flipped one of those to `>` or `<=` would shift the behavior in
    // ways the happy-path tests above wouldn't catch. These tests pin
    // the exact boundary in each direction so a mis-flipped inequality
    // fails loudly instead of silently.

    #[test]
    fn boundary_strong_threshold_exactly_090_fires_rule_1() {
        // 0.90 is the strong threshold; the filter uses `>= 0.90`, so an
        // evidence record at exactly 0.90 must take the rule-1 path.
        let result = aggregate(&[ev(Source::Web, 0.90, Origin::Filename)]);
        assert_eq!(result.decision_rule, DecisionRule::Rule1Strong);
        assert_eq!(result.source, Source::Web);
        assert!(!result.needs_review);
    }

    #[test]
    fn boundary_strong_threshold_just_below_falls_through_to_rule_2() {
        // 0.89 is below 0.90 so it's *not* strong. With no other evidence
        // the record still wins via rule 2's sum path, but the decision
        // rule is Rule2Sum (not Rule1Strong). This is what catches a
        // hypothetical `> 0.90` flip in the strong filter.
        let result = aggregate(&[ev(Source::Web, 0.89, Origin::Filename)]);
        assert_eq!(result.decision_rule, DecisionRule::Rule2Sum);
        assert_eq!(result.source, Source::Web);
    }

    #[test]
    fn boundary_conflict_threshold_exactly_070_can_trigger_rule_4() {
        // Runner-up sitting at exactly 0.70 is eligible for rule-4
        // conflict detection (the filter is `>= 0.70`). With a small
        // lead, rule 4 must fire.
        let result = aggregate(&[
            ev(Source::Web, 0.75, Origin::Filename),
            ev(Source::BluRay, 0.70, Origin::Group),
        ]);
        assert!(result.needs_review);
        assert_eq!(result.decision_rule, DecisionRule::Rule4Conflict);
    }

    #[test]
    fn boundary_conflict_threshold_just_below_does_not_trigger_rule_4() {
        // Runner-up at 0.69 is below the conflict threshold, so even a
        // narrow lead stays a clean rule-2 win.
        let result = aggregate(&[
            ev(Source::Web, 0.75, Origin::Filename),
            ev(Source::BluRay, 0.69, Origin::Group),
        ]);
        assert!(!result.needs_review);
        assert_eq!(result.decision_rule, DecisionRule::Rule2Sum);
    }

    #[test]
    fn boundary_min_total_exactly_050_clears_rule_3() {
        // Sum to exactly 0.50: the test is `< MIN_TOTAL`, so 0.50 must
        // NOT be weak enough for the Unknown fallback.
        let result = aggregate(&[
            ev(Source::Web, 0.30, Origin::Filename),
            ev(Source::Web, 0.20, Origin::Group),
        ]);
        assert_ne!(result.decision_rule, DecisionRule::Rule3Weak);
        assert_eq!(result.source, Source::Web);
    }

    #[test]
    fn boundary_min_total_just_below_triggers_rule_3() {
        // Sum to 0.49 — strictly below MIN_TOTAL, so rule 3 fires.
        let result = aggregate(&[
            ev(Source::Web, 0.30, Origin::Filename),
            ev(Source::Web, 0.19, Origin::Group),
        ]);
        assert_eq!(result.decision_rule, DecisionRule::Rule3Weak);
        assert_eq!(result.source, Source::Unknown);
        assert!(result.needs_review);
    }

    #[test]
    fn boundary_min_lead_exactly_030_clears_rule_4() {
        // Lead of exactly 0.30 is the boundary for rule 4 (`lead <
        // MIN_LEAD`). Strong runner-up present, but the lead is big
        // enough that rule 4 must NOT fire.
        //
        // Web sums to 1.00 (filename 0.30 + group 0.70), BluRay sums
        // to 0.70 (ffprobe 0.70). Lead = 0.30 exactly. BluRay's 0.70
        // is ≥ CONFLICT_THRESHOLD so the conflict check is armed; it's
        // the lead that keeps rule 4 silent.
        //
        // BluRay's 0.70 is below STRONG_THRESHOLD (0.90), so the
        // ground-truth veto rule 5 also stays silent.
        let result = aggregate(&[
            ev(Source::Web, 0.30, Origin::Filename),
            ev(Source::Web, 0.70, Origin::Group),
            ev(Source::BluRay, 0.70, Origin::Ffprobe),
        ]);
        assert_eq!(result.source, Source::Web);
        assert!(
            !result.needs_review,
            "lead of exactly 0.30 should clear rule 4 — the check is `lead < MIN_LEAD`"
        );
        assert_eq!(result.decision_rule, DecisionRule::Rule2Sum);
    }

    #[test]
    fn boundary_min_lead_just_below_triggers_rule_4() {
        // Lead of 0.29 — strictly below MIN_LEAD. Rule 4 must fire
        // because there's a conflicting runner-up at ≥ 0.70.
        // Web sums to 0.99, BluRay sums to 0.70, lead = 0.29.
        let result = aggregate(&[
            ev(Source::Web, 0.29, Origin::Filename),
            ev(Source::Web, 0.70, Origin::Group),
            ev(Source::BluRay, 0.70, Origin::Ffprobe),
        ]);
        assert!(result.needs_review);
        assert_eq!(result.decision_rule, DecisionRule::Rule4Conflict);
    }

    #[test]
    fn boundary_origin_cap_exactly_130_kept_intact() {
        // A single origin summing to exactly 1.30 hits `sum.min(1.30)`
        // and stays at 1.30 — the cap is idempotent at the boundary.
        // Runner-up at 0.40 leaves a 0.90 lead, a clean rule-2 win with
        // no conflict.
        let result = aggregate(&[
            ev(Source::BluRay, 0.65, Origin::Filename),
            ev(Source::BluRay, 0.65, Origin::Filename),
            ev(Source::Web, 0.40, Origin::Temporal),
        ]);
        assert_eq!(result.source, Source::BluRay);
        assert_eq!(result.decision_rule, DecisionRule::Rule2Sum);
    }

    #[test]
    fn boundary_origin_cap_above_130_is_clipped() {
        // A filename origin that would naturally sum to 1.80 (0.95 +
        // 0.85) must be clipped to 1.30. With a competing ffprobe
        // signal at 0.90, the *uncapped* sum would leave BluRay ahead
        // by 0.90 — a clean win with no review. With the cap, BluRay
        // sums to only 1.30, still wins by 0.40, but the strong ffprobe
        // disagreement trips the ground-truth veto (rule 5).
        //
        // This is the exact regression guarded by Issue 1's origin cap,
        // and it's a good anchor for verifying the cap is actually
        // clipping — a flip to `< 1.30` / `> 1.30` on the comparison
        // would change the behavior here.
        let result = aggregate(&[
            ev(Source::BluRay, 0.95, Origin::Filename),
            ev(Source::BluRay, 0.85, Origin::Filename),
            ev(Source::Web, 0.90, Origin::Ffprobe),
        ]);
        assert_eq!(result.source, Source::BluRay);
        assert!(result.needs_review);
        assert_eq!(result.decision_rule, DecisionRule::Rule5GroundTruthVeto);
    }

    #[test]
    fn boundary_origin_cap_is_per_source_per_origin() {
        // The cap is keyed on `(source, origin)`, not just origin. A
        // filename layer emitting 0.90 for BluRay and 0.90 for Web
        // should contribute the full 0.90 to each source — not split
        // 1.30 across them or clip one of them.
        let result = aggregate(&[
            ev(Source::BluRay, 0.90, Origin::Filename),
            ev(Source::Web, 0.90, Origin::Filename),
        ]);
        // Rule 1 falls through because the two strong signals disagree.
        // Rule 2 sees BluRay=0.90 and Web=0.90 — a tie. Source::rank
        // tiebreaks to the higher-ranked source. This test doesn't
        // care which wins — it cares that rule 5 fires because both
        // are strong and disagreeing. Actually neither origin is
        // ffprobe/dir here so rule 5 stays silent; we get rule 4
        // instead because the lead is zero and both are ≥ 0.70.
        assert!(result.needs_review);
        assert_eq!(result.decision_rule, DecisionRule::Rule4Conflict);
    }

    // ─────────────────────────────────────────────────────────────────────
    // End-to-end: the full pre-download pipeline against a real DB.
    // ─────────────────────────────────────────────────────────────────────
    //
    // Every other test in this module pokes the aggregator directly with
    // hand-constructed SourceEvidence records. This one exercises the
    // composition: `classify_release` runs Layer 1 (filename), Layer 3
    // (group lookup — real SQLite hit), Layer 4 (temporal), and the Layer 2
    // escalation gate in a single call, then writes its audit log to the
    // real `logs` table. It's the only test that would catch a regression
    // where one of those layers stops being wired into `classify_release`.

    #[tokio::test]
    async fn end_to_end_pipeline_classifies_confidently_corroborated_release() {
        // Full app migration — classify_release writes to the logs table
        // on every call, so we can't get away with a subset of the schema.
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("full migrate");

        // `migrate()` already seeds the group_source_map with VCB-Studio,
        // but make the test's dependency on that seed explicit so a future
        // reshuffle of the seed list doesn't silently break the test.
        crate::models::group_source_map::upsert_user_edit(
            &db,
            "VCB-Studio",
            Source::BluRay,
            0.95,
            "e2e test fixture",
        )
        .await
        .expect("seed group");

        // Title with a BluRay-shaped filename (BDRip keyword, 1080p,
        // x265 HEVC, FLAC audio) and a known BD-only encoder as the
        // release group. Filename + group + temporal will all agree on
        // BluRay, so the L2 escalation gate stays shut and the test
        // doesn't need to mock any network layer.
        let title = "[VCB-Studio] Demo Show - 01 [Ma10p_1080p][x265_flac].mkv";
        let nyaa = NyaaContext {
            // Empty strings make classify_description a no-op even if the
            // escalation gate somehow fires, so the test stays hermetic.
            info_hash: "",
            view_url: "",
            is_batch: true,
        };
        let series = SeriesContext {
            status: "FINISHED",
            season_year: Some(2020),
            end_year: Some(2020),
        };

        let result = classify_release(&db, title, None, Some(nyaa), Some(series)).await;

        // Full pipeline lands on BluRay-1080p with a clean verdict.
        assert_eq!(
            result.source,
            Source::BluRay,
            "expected BluRay, got {:?} — evidence: {:#?}",
            result.source,
            result.evidence,
        );
        assert_eq!(result.resolution, Resolution::R1080p);
        assert!(
            !result.needs_review,
            "clean multi-layer agreement should not flag review: {:#?}",
            result.evidence,
        );
        // Rule 1 (strong single) or Rule 2 (clean sum) — both acceptable
        // as "confident verdict". We specifically assert *not* Empty /
        // Rule3Weak / Rule4Conflict / Rule5GroundTruthVeto.
        assert!(
            matches!(
                result.decision_rule,
                DecisionRule::Rule1Strong | DecisionRule::Rule2Sum
            ),
            "unexpected decision rule: {:?}",
            result.decision_rule,
        );

        // The evidence trail must include contributions from each layer
        // that was wired in — proving the composition actually happened
        // instead of a single-layer short-circuit.
        let origins: std::collections::HashSet<_> =
            result.evidence.iter().map(|e| e.origin).collect();
        assert!(
            origins.contains(&Origin::Filename),
            "filename evidence missing: {:#?}",
            result.evidence,
        );
        assert!(
            origins.contains(&Origin::Group),
            "group evidence missing — Layer 3 lookup didn't fire: {:#?}",
            result.evidence,
        );
        assert!(
            origins.contains(&Origin::Temporal),
            "temporal evidence missing — Layer 4 didn't fire: {:#?}",
            result.evidence,
        );

        // The DB log assertion that lived here was dropped along with
        // the per-candidate INSERT in log_classification_to_db: that
        // function now only persists rows where `needs_review` is true,
        // and a confidently-corroborated release by definition isn't
        // one. The classifier's structural correctness is what this
        // test exercises; the side-effect-to-logs contract belongs in
        // a needs_review-specific test (covered separately).
    }
}
