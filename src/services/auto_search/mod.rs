use std::collections::HashSet;

use futures_util::stream::{self, StreamExt};

use sqlx::SqlitePool;

use crate::models::config::Config;
use crate::services::custom_formats::CompiledCustomFormat;
use crate::services::source::{self, ClassificationResult, Resolution, Source};
use crate::services::{
    anilist::AnimeDetail,
    media,
    nyaa::{self, SearchOptions, SearchResult},
    quality,
};

// ── Pre-compiled regexes for parse_release_numbers ─────────────────────────

mod aliases;
mod pack_detection;
mod provenance;
mod release_parse;
mod scoring;
mod seadex_lookup;
mod search_target;

use aliases::{SiblingRejectPrecompute, sibling_match_rejects};
pub use aliases::{
    classify_match, collect_aliases, collect_extended_aliases, collect_sibling_aliases,
    dedupe_strings, distinctive_overlap_ratio, matches_target, normalize_title,
    sequel_variant_aliases, token_overlap_ratio, token_set,
};
pub use pack_detection::{
    TRANSITIVE_WALK_MAX_FETCHES, detect_sibling_entries_in_pack,
    expand_parent_with_transitive_relations, is_transitive_walk_source,
};
pub use provenance::{AliasMatch, MatchKind, MatchPhase, MatchProvenance, history_summary};
pub(crate) use release_parse::is_media_filename;
pub use release_parse::{
    has_selective_discriminator, infer_season_from_detail, parse_release_numbers,
    pick_wanted_file_indices,
};
use release_parse::{
    normalize_subtitle, season_mismatch, trailing_subtitle_of, within_episode_slack,
};
use scoring::{
    apply_cf_seadex_overlay, apply_cf_seadex_overlay_with_breakdown, rescore_for_auto_search,
    rescore_for_auto_search_with_breakdown,
};
use seadex_lookup::{fetch_seadex_payload, is_seadex_match, seadex_gates};
pub use seadex_lookup::{prewarm_seadex_negative, seadex_warm_cache_from_db};
pub use search_target::{
    SearchTarget, build_missing_targets, build_monitored_targets, build_upgrade_targets,
};

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct AutoSearchHit {
    pub target_label: String,
    pub release_title: String,
    pub release_group: String,
    pub quality_tier: String,
    pub url: String,
    pub score: i32,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct AutoSearchReport {
    pub grabbed: Vec<AutoSearchHit>,
    pub skipped: Vec<String>,
    pub quality_profile: String,
    /// Set when the per-target loop was stopped by an external
    /// signal (currently: series removed from library — see issue
    /// #102). The wrapper `emit_auto_search_terminal` short-circuits
    /// on this so the cascade-stop path's "cancelled" toast doesn't
    /// get overwritten by a generic terminal event.
    #[serde(default)]
    pub cancelled: bool,
    /// Issue #219 — advisory lines the terminal toast shows above the
    /// per-target reasons (currently: adult title with no indexer
    /// configured). Carried on the report rather than emitted mid-search
    /// because the sticky progress toast shows one event at a time.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Return all scored candidates for an episode target without grabbing anything.
/// Used by the interactive search feature. More permissive than auto-search:
/// allows batch results and uses relaxed title matching so users see a broader
/// set of candidates to choose from.
pub async fn find_all_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    _allow_batch: bool,
    cfs: &[CompiledCustomFormat],
    indexers: &crate::IndexerCache,
) -> Vec<SearchResult> {
    // Snapshot the indexer cache once. Same shape as the auto-search
    // entry points: we clone the inner Arc<Vec<...>> under the read
    // lock and release it before any HTTP work begins so a slow indexer
    // can't pin the lock against a concurrent Settings → Indexers save.
    let indexers_snapshot = indexers.read().await.clone();
    let indexer_slice: &[std::sync::Arc<dyn crate::services::indexers::Indexer>] =
        indexers_snapshot.as_slice();
    let (aliases, canonical_aliases, variant_aliases) = collect_aliases_with_variants(detail);
    let series_ctx = resolve_search_overrides(db, detail, config).await;
    let queries = append_custom_tokens(
        build_queries_mixed(
            &canonical_aliases,
            &variant_aliases,
            target,
            !series_ctx.restrict_user.is_empty(),
        ),
        &series_ctx.custom_tokens,
    );
    let preferred_groups = quality::parse_group_list(&config.preferred_groups);
    let preferred_res = preferred_resolution_search_value(config);
    let is_finished = detail.is_finished();
    let finished_mode = quality::FinishedSeriesMode::from_str(&config.finished_series_quality);
    let preferred_source_enum = Source::from_str(&config.preferred_source);
    let preferred_resolution_enum = Resolution::from_str(&config.preferred_resolution);
    // Scoring only looks at Source rank, so drop the BluRay sub-tier.
    let (cutoff_source_enum, _, _) = source::parse_cutoff_source(&config.cutoff_source);
    let cutoff_resolution_enum = Resolution::from_str(&config.cutoff_resolution);

    // Single SeaDex lookup per entry-point call, reused across every
    // candidate in the loop below. `seadex_gates` decides whether a
    // lookup is needed and whether the hardcoded boost is active —
    // it's suppressed automatically when the user has a
    // `SeaDexBestSpecification` CF to avoid double counting.
    let (seadex_needs_lookup, seadex_boost_enabled) = seadex_gates(config, cfs);
    let seadex_payload = fetch_seadex_payload(
        db,
        seadex_needs_lookup,
        detail.id,
        display_title(detail),
        &preferred_groups,
        &preferred_res,
        true,
    )
    .await;
    let seadex_hashes = seadex_payload.hashes;

    let expected_season = infer_season_from_detail(detail);
    let sibling_aliases = collect_sibling_aliases(detail, &aliases);
    let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &sibling_aliases);
    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    // Seed the candidate pool with SeaDex-curated releases so they're
    // guaranteed to show up in the interactive search UI even when
    // Nyaa's text search would miss them entirely (smol-style
    // megapacks titled by season rather than entry).
    for mut result in seadex_payload.candidates {
        let dedupe_key = if !result.info_hash.is_empty() {
            result.info_hash.clone()
        } else {
            result.title.to_lowercase()
        };
        if seen.insert(dedupe_key) {
            result.match_provenance = Some(MatchProvenance::seadex(MatchPhase::SeadexSeed));
            candidates.push(result);
        }
    }

    let ctx = InteractiveQueryCtx {
        phase: MatchPhase::Primary,
        aliases: &aliases,
        sibling_precompute: &sibling_precompute,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        expected_season,
        seadex_hashes: &seadex_hashes,
        restrict_user: &series_ctx.restrict_user,
        absolute_offset: series_ctx.absolute_offset,
        categories: &categories,
        indexers: indexer_slice,
    };

    // Interactive search: allow batch results so user can see & pick them,
    // but filter by season and episode to avoid showing wrong-season results.
    run_queries_interactive(&queries, ctx, &mut seen, &mut candidates).await;

    // Try extended aliases if primary queries found nothing. Extended
    // aliases expand the own-side of the sibling-rejection comparison,
    // so rebuild the precompute with the full alias list — otherwise
    // an extended alias that legitimately overlaps with a sibling (e.g.
    // a synonym that happens to share tokens) would look like a sibling
    // win and get rejected.
    if candidates.is_empty() {
        let extended = collect_extended_aliases(detail);
        if !extended.is_empty() {
            let ext_queries = append_custom_tokens(
                build_queries_from_aliases(&extended, target, !series_ctx.restrict_user.is_empty()),
                &series_ctx.custom_tokens,
            );
            let all_aliases = [aliases.clone(), extended].concat();
            let ext_precompute = SiblingRejectPrecompute::build(&all_aliases, &sibling_aliases);
            let ext_ctx = InteractiveQueryCtx {
                phase: MatchPhase::Extended,
                aliases: &all_aliases,
                sibling_precompute: &ext_precompute,
                ..ctx
            };
            run_queries_interactive(&ext_queries, ext_ctx, &mut seen, &mut candidates).await;
        }
    }

    // #23 follow-up — When a Nyaa uploader restriction is active, every
    // Nyaa request is already scoped to `/user/<name>`, so a
    // preferred-group-prefixed query like "Erai-raws <title>" against the
    // SubsPlease user page can only return uploads SubsPlease happened to
    // name with "Erai-raws" in them — effectively never. Skip the whole
    // pass to avoid paying N × round-trip cost for zero coverage.
    if !preferred_groups.is_empty() && series_ctx.restrict_user.is_empty() {
        let group_queries = append_custom_tokens(
            build_group_queries(detail, target, &preferred_groups),
            &series_ctx.custom_tokens,
        );
        run_queries_interactive(
            &group_queries,
            InteractiveQueryCtx {
                phase: MatchPhase::PreferredGroup,
                ..ctx
            },
            &mut seen,
            &mut candidates,
        )
        .await;
    }

    // #30: franchise-root aliases + absolute episode number.
    // SubsPlease-style releases ("[SubsPlease] Jujutsu Kaisen - 56" for
    // JJK S3 E9) use the base franchise title and an absolute episode
    // number. Phase 1 drops them at the alias-match step because the
    // cour-specific aliases ("JJK: Shimetsu Kaiyuu - Zenpen",
    // "JUJUTSU KAISEN Season 3: The Culling Game Part 1") share only
    // 2 tokens with the release, below the 0.5 overlap threshold.
    //
    // This pass runs with a different ctx that treats franchise aliases
    // as the own-side, the computed absolute episode as the target, and
    // an empty sibling set (the base franchise name trivially substring-
    // matches every sibling in the graph, so re-using Phase 1's sibling
    // list would reject every absolute-numbered release).
    let franchise_precompute;
    let absolute_target;
    if series_ctx.absolute_offset > 0
        && !series_ctx.franchise_aliases.is_empty()
        && let SearchTarget::Episode(ep) = target
    {
        absolute_target = SearchTarget::Episode(ep.saturating_add(series_ctx.absolute_offset));
        franchise_precompute = SiblingRejectPrecompute::build(&series_ctx.franchise_aliases, &[]);
        let franchise_queries = append_custom_tokens(
            build_queries_from_aliases(
                &series_ctx.franchise_aliases,
                &absolute_target,
                !series_ctx.restrict_user.is_empty(),
            ),
            &series_ctx.custom_tokens,
        );
        let franchise_ctx = InteractiveQueryCtx {
            phase: MatchPhase::Franchise,
            aliases: &series_ctx.franchise_aliases,
            sibling_precompute: &franchise_precompute,
            target: &absolute_target,
            // `target` already carries the absolute number, so no
            // secondary offset on top of that.
            absolute_offset: 0,
            ..ctx
        };
        run_queries_interactive(
            &franchise_queries,
            franchise_ctx,
            &mut seen,
            &mut candidates,
        )
        .await;
    }

    // Interactive search is user-driven — we want to *show* the
    // CF-filtered candidates even when they'd be dropped by an
    // auto-search path, so the minimum_score floor is suppressed here
    // (passed as `i32::MIN`). The CF score still contributes to ranking
    // so the user sees the same ordering the auto-picker would have used.
    let mut scored: Vec<SearchResult> = Vec::with_capacity(candidates.len());
    for mut c in candidates.drain(..) {
        let classification = source::classify_release(
            db,
            &c.title,
            Some(&c.resolution),
            Some(source::NyaaContext {
                info_hash: &c.info_hash,
                view_url: &c.link,
                is_batch: c.is_batch,
            }),
            Some(source::SeriesContext {
                status: &detail.status,
                season_year: detail.season_year,
                end_year: detail.end_year,
            }),
        )
        .await;
        // Interactive search uses the breakdown variants so each
        // candidate's `score_breakdown` stays in sync with its final
        // displayed score — the UI expander wants the full trail of
        // alias match / season penalty / CF contributions visible.
        let (base, mut auto_parts) = rescore_for_auto_search_with_breakdown(
            &c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
            series_ctx.absolute_offset,
            false, // batch_search_mode — episode target, single-unit penalty applies
        );
        // No CF floor on the interactive path — see comment above.
        if let Some((final_score, cf_parts)) = apply_cf_seadex_overlay_with_breakdown(
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            i32::MIN,
        ) {
            c.score = final_score;
            c.score_breakdown.append(&mut auto_parts);
            c.score_breakdown.extend(cf_parts);
            scored.push(c);
        }
    }

    scored.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    scored
}

#[allow(clippy::too_many_arguments)]
pub async fn find_best_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    allow_batch: bool,
    batch_episode_match: bool,
    cfs: &[CompiledCustomFormat],
    indexers_cache: &crate::IndexerCache,
) -> Option<SearchResult> {
    collect_scored_for_target(
        db,
        detail,
        config,
        target,
        allow_batch,
        batch_episode_match,
        cfs,
        indexers_cache,
    )
    .await
    .into_iter()
    .next()
}

/// Same multi-phase auto-search as `find_best_for_target`, but picks the
/// best *batch* release instead of the best overall. Two things had to
/// change relative to the pre-existing `best + filter(is_batch)` approach
/// that this function replaces:
///
/// 1. Filtering to `is_batch` happens *before* selection. The old code
///    picked the overall best scored candidate and then filtered, which
///    returned `None` whenever the top-scored result was a single-episode
///    weekly release — i.e. for almost every popular currently- or
///    recently-finished show.
/// 2. An extra batch-probe query phase runs alongside the standard query
///    sweep. Nyaa page 1 for a plain title query on a popular show is
///    dominated by weekly single-episode uploads; batches get pushed off
///    the first page entirely. The "X batch" / "X complete" / "X 01-"
///    probes funnel toward listings whose titles carry those tokens, so
///    batches surface even when the generic queries would miss them.
pub async fn find_best_batch_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    cfs: &[CompiledCustomFormat],
    indexers_cache: &crate::IndexerCache,
) -> Option<SearchResult> {
    collect_scored_batches_for_target(db, detail, config, target, cfs, indexers_cache)
        .await
        .into_iter()
        .next()
}

/// Collection + scoring variant focused on batch releases.
///
/// Runs the same Phase 1/1.5/2/3 query sweep as the standard auto-search
/// but augments it with `quality::batch_probe_queries` to surface batches
/// that generic queries would miss on Nyaa page 1. Non-batch candidates
/// are dropped before scoring, so the returned `Vec` only contains batch
/// releases sorted by score descending.
pub async fn collect_scored_batches_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    cfs: &[CompiledCustomFormat],
    indexers_cache: &crate::IndexerCache,
) -> Vec<SearchResult> {
    let (aliases, canonical_aliases, variant_aliases) = collect_aliases_with_variants(detail);
    let series_ctx = resolve_search_overrides(db, detail, config).await;
    let preferred_groups = quality::parse_group_list(&config.preferred_groups);
    let preferred_res = preferred_resolution_search_value(config);
    let is_finished = detail.is_finished();
    let finished_mode = quality::FinishedSeriesMode::from_str(&config.finished_series_quality);
    let preferred_source_enum = Source::from_str(&config.preferred_source);
    let preferred_resolution_enum = Resolution::from_str(&config.preferred_resolution);
    let (cutoff_source_enum, _, _) = source::parse_cutoff_source(&config.cutoff_source);
    let cutoff_resolution_enum = Resolution::from_str(&config.cutoff_resolution);

    let (seadex_needs_lookup, seadex_boost_enabled) = seadex_gates(config, cfs);
    let seadex_payload = fetch_seadex_payload(
        db,
        seadex_needs_lookup,
        detail.id,
        display_title(detail),
        &preferred_groups,
        &preferred_res,
        true,
    )
    .await;
    let seadex_hashes = seadex_payload.hashes;

    let expected_season = infer_season_from_detail(detail);
    let sibling_aliases = collect_sibling_aliases(detail, &aliases);
    let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &sibling_aliases);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    // Seed with SeaDex-curated candidates fetched directly from their
    // view URLs. See `find_all_for_target` for the rationale — the
    // text-query sweep can't find batches whose titles don't carry
    // the target's alias tokens.
    for mut result in seadex_payload.candidates {
        let dedupe_key = if !result.info_hash.is_empty() {
            result.info_hash.clone()
        } else {
            result.title.to_lowercase()
        };
        if seen.insert(dedupe_key) {
            result.match_provenance = Some(MatchProvenance::seadex(MatchPhase::SeadexSeed));
            candidates.push(result);
        }
    }

    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);
    let indexers_arc = indexers_cache.read().await.clone();
    let indexers: &[std::sync::Arc<dyn crate::services::indexers::Indexer>] = &indexers_arc[..];

    let ctx = AutoQueryCtx {
        phase: MatchPhase::Primary,
        aliases: &aliases,
        sibling_precompute: &sibling_precompute,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        allow_batch: true,
        expected_season,
        categories: &categories,
        batch_episode_match: false,
        seadex_hashes: &seadex_hashes,
        restrict_user: &series_ctx.restrict_user,
        absolute_offset: series_ctx.absolute_offset,
        indexers,
    };

    // Standard query sweep — picks up any batches that happen to surface
    // on Nyaa page 1 alongside the singles.
    let queries = append_custom_tokens(
        build_queries_mixed(
            &canonical_aliases,
            &variant_aliases,
            target,
            !series_ctx.restrict_user.is_empty(),
        ),
        &series_ctx.custom_tokens,
    );
    run_queries(&queries, ctx, &mut seen, &mut candidates).await;

    // Batch-targeted probes — the important addition for this function.
    // Explicit "batch" / "complete" keywords push the Nyaa search toward
    // listings that wouldn't appear on page 1 for a plain title query.
    let batch_queries = append_custom_tokens(
        quality::batch_probe_queries(&aliases),
        &series_ctx.custom_tokens,
    );
    run_queries(
        &batch_queries,
        AutoQueryCtx {
            phase: MatchPhase::BatchProbe,
            ..ctx
        },
        &mut seen,
        &mut candidates,
    )
    .await;

    // Preferred-group queries, scoped to batches. Same fallback rule as
    // `collect_scored_for_target`: only fire if no preferred-group hit
    // has surfaced yet.
    let has_preferred_hit = !preferred_groups.is_empty()
        && candidates.iter().any(|c| {
            preferred_groups
                .iter()
                .any(|g| g.eq_ignore_ascii_case(&c.group))
        });
    // #23 follow-up — see the note in `find_all_for_target`. Preferred-
    // group queries are redundant when the `/user/<name>` scope is
    // already active.
    if !has_preferred_hit && !preferred_groups.is_empty() && series_ctx.restrict_user.is_empty() {
        let group_queries = append_custom_tokens(
            build_group_queries(detail, target, &preferred_groups),
            &series_ctx.custom_tokens,
        );
        run_queries(
            &group_queries,
            AutoQueryCtx {
                phase: MatchPhase::PreferredGroup,
                ..ctx
            },
            &mut seen,
            &mut candidates,
        )
        .await;
    }

    // Drop non-batches before the classify/rescore pass so we don't pay
    // the classification cost on candidates we're going to throw away.
    // SeaDex-curated candidates are exempt: the curator has already
    // blessed the release for this entry, and `detect_batch` misses
    // title forms like Roman-numeral season markers ("Mob Psycho 100
    // III") which are common in SeaDex picks. Without this exemption,
    // a curated full-season BD pack gets dropped here before it can be
    // scored.
    candidates.retain(|c| c.is_batch || is_seadex_match(&c.info_hash, &seadex_hashes));

    let mut scored: Vec<SearchResult> = Vec::with_capacity(candidates.len());
    for mut c in candidates.drain(..) {
        let classification = source::classify_release(
            db,
            &c.title,
            Some(&c.resolution),
            Some(source::NyaaContext {
                info_hash: &c.info_hash,
                view_url: &c.link,
                is_batch: c.is_batch,
            }),
            Some(source::SeriesContext {
                status: &detail.status,
                season_year: detail.season_year,
                end_year: detail.end_year,
            }),
        )
        .await;

        if is_finished
            && finished_mode == quality::FinishedSeriesMode::BdOnly
            && !source::passes_bd_only_filter(&classification)
        {
            continue;
        }

        // `collect_scored_batches_for_target` feeds both the user-facing
        // `interactive_search_batches` and the auto-grab
        // `find_best_batch_for_target`. Populating the breakdown here
        // costs a small Vec allocation per candidate on the auto path
        // too, which is cheap enough vs. the classify+network work that
        // already dominates the per-candidate cost.
        let (base, mut auto_parts) = rescore_for_auto_search_with_breakdown(
            &c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
            series_ctx.absolute_offset,
            true, // batch_search_mode — every candidate is a batch here
        );
        if let Some((final_score, cf_parts)) = apply_cf_seadex_overlay_with_breakdown(
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            config.custom_format_minimum_score,
        ) {
            c.score = final_score;
            c.score_breakdown.append(&mut auto_parts);
            c.score_breakdown.extend(cf_parts);
            scored.push(c);
        }
    }

    scored.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    scored
}

/// Internal: run the full auto-search query sweep (Phase 1 primary →
/// Phase 1.5 extended aliases → Phase 2 preferred-group queries →
/// Phase 3 BD probe), classify each candidate exactly once, filter via
/// the BdOnly rule, rescore, and return the sorted `Vec<SearchResult>`.
///
/// Factored out so `find_best_for_target` (picks the top result) and
/// `find_best_batch_for_target` (picks the top batch) can share the
/// expensive collection pass. Filtering to batches post-sort gives the
/// same answer as filtering pre-scoring because `rescore_for_auto_search`
/// applies its per-target batch bump uniformly inside each target kind.
#[allow(clippy::too_many_arguments)]
async fn collect_scored_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    allow_batch: bool,
    batch_episode_match: bool,
    cfs: &[CompiledCustomFormat],
    indexers_cache: &crate::IndexerCache,
) -> Vec<SearchResult> {
    let (aliases, canonical_aliases, variant_aliases) = collect_aliases_with_variants(detail);
    let series_ctx = resolve_search_overrides(db, detail, config).await;
    let queries = append_custom_tokens(
        build_queries_mixed(
            &canonical_aliases,
            &variant_aliases,
            target,
            !series_ctx.restrict_user.is_empty(),
        ),
        &series_ctx.custom_tokens,
    );
    let preferred_groups = quality::parse_group_list(&config.preferred_groups);
    let preferred_res = preferred_resolution_search_value(config);
    let is_finished = detail.is_finished();
    let finished_mode = quality::FinishedSeriesMode::from_str(&config.finished_series_quality);
    let preferred_source_enum = Source::from_str(&config.preferred_source);
    let preferred_resolution_enum = Resolution::from_str(&config.preferred_resolution);
    // Scoring only looks at Source rank, so drop the BluRay sub-tier.
    let (cutoff_source_enum, _, _) = source::parse_cutoff_source(&config.cutoff_source);
    let cutoff_resolution_enum = Resolution::from_str(&config.cutoff_resolution);

    let (seadex_needs_lookup, seadex_boost_enabled) = seadex_gates(config, cfs);
    let seadex_payload = fetch_seadex_payload(
        db,
        seadex_needs_lookup,
        detail.id,
        display_title(detail),
        &preferred_groups,
        &preferred_res,
        true,
    )
    .await;
    let seadex_hashes = seadex_payload.hashes;

    let expected_season = infer_season_from_detail(detail);
    let sibling_aliases = collect_sibling_aliases(detail, &aliases);
    let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &sibling_aliases);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    // Seed with SeaDex-curated candidates fetched directly from their
    // view URLs — this is how the smol Kizumonogatari pack (titled
    // `[smol] Monogatari (Season 9) ...`) gets into the pool for a
    // Kizumonogatari Part 2 target whose text queries would never
    // match the smol filename.
    //
    // Per-episode auto-search targets (`allow_batch=false`) mean "don't
    // add batches in this search" — the user has explicitly opted out
    // of batch grabs for episode search. Without this filter, every
    // episode search on a SeaDex-curated series with a megapack
    // top-hit would resurrect that batch into the candidate pool,
    // bypassing the setting. SeaDex curation does not override the
    // user's batch-allowed policy; it only overrides the heuristic
    // title-matching gate inside `run_queries`.
    for mut result in seadex_payload.candidates {
        if !allow_batch && result.is_batch {
            continue;
        }
        let dedupe_key = if !result.info_hash.is_empty() {
            result.info_hash.clone()
        } else {
            result.title.to_lowercase()
        };
        if seen.insert(dedupe_key) {
            result.match_provenance = Some(MatchProvenance::seadex(MatchPhase::SeadexSeed));
            candidates.push(result);
        }
    }

    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);
    let indexers_arc = indexers_cache.read().await.clone();
    let indexers: &[std::sync::Arc<dyn crate::services::indexers::Indexer>] = &indexers_arc[..];

    let ctx = AutoQueryCtx {
        phase: MatchPhase::Primary,
        aliases: &aliases,
        sibling_precompute: &sibling_precompute,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        allow_batch,
        expected_season,
        categories: &categories,
        batch_episode_match,
        seadex_hashes: &seadex_hashes,
        restrict_user: &series_ctx.restrict_user,
        absolute_offset: series_ctx.absolute_offset,
        indexers,
    };

    // Phase 1: standard queries (primary aliases + episode variants).
    run_queries(&queries, ctx, &mut seen, &mut candidates).await;

    // Phase 1.5: if no candidates, try extended aliases (synonyms +
    // decomposed sub-phrases). Rebuild the sibling precompute with the
    // full alias list so own-vs-sibling overlap comparisons see the
    // extended aliases too.
    if candidates.is_empty() {
        let extended = collect_extended_aliases(detail);
        if !extended.is_empty() {
            let ext_queries = append_custom_tokens(
                build_queries_from_aliases(&extended, target, !series_ctx.restrict_user.is_empty()),
                &series_ctx.custom_tokens,
            );
            let all_aliases = [aliases.clone(), extended].concat();
            let ext_precompute = SiblingRejectPrecompute::build(&all_aliases, &sibling_aliases);
            let ext_ctx = AutoQueryCtx {
                phase: MatchPhase::Extended,
                aliases: &all_aliases,
                sibling_precompute: &ext_precompute,
                ..ctx
            };
            run_queries(&ext_queries, ext_ctx, &mut seen, &mut candidates).await;
        }
    }

    // Phase 2: if no candidate from a preferred group, try group-prefixed queries.
    let has_preferred_hit = !preferred_groups.is_empty()
        && candidates.iter().any(|c| {
            preferred_groups
                .iter()
                .any(|g| g.eq_ignore_ascii_case(&c.group))
        });

    // #23 follow-up — see the note in `find_all_for_target`. Preferred-
    // group queries are redundant when the `/user/<name>` scope is
    // already active.
    if !has_preferred_hit && !preferred_groups.is_empty() && series_ctx.restrict_user.is_empty() {
        let group_queries = append_custom_tokens(
            build_group_queries(detail, target, &preferred_groups),
            &series_ctx.custom_tokens,
        );
        run_queries(
            &group_queries,
            AutoQueryCtx {
                phase: MatchPhase::PreferredGroup,
                ..ctx
            },
            &mut seen,
            &mut candidates,
        )
        .await;
    }

    // Phase 3: for finished series with BD preference, probe for BD releases.
    // The "any BD candidate" check uses a filename-only heuristic so we can
    // decide before running the full classification pass.
    if is_finished && finished_mode != quality::FinishedSeriesMode::SameAsAiring {
        let has_bd_candidate = candidates
            .iter()
            .any(|c| source::looks_like_bluray_filename(&c.title));

        if !has_bd_candidate {
            let bd_queries = append_custom_tokens(
                quality::bd_probe_queries(&aliases),
                &series_ctx.custom_tokens,
            );
            run_queries(
                &bd_queries,
                AutoQueryCtx {
                    phase: MatchPhase::BdProbe,
                    ..ctx
                },
                &mut seen,
                &mut candidates,
            )
            .await;
        }
    }

    // #30: franchise-root aliases + absolute episode number.
    // Mirrors the interactive path — see the equivalent block in
    // `find_all_for_target` for the full rationale. SubsPlease-style
    // absolute-numbered releases for sequel cours ("Jujutsu Kaisen -
    // 56" for JJK S3 E9) need this pass to surface, otherwise the
    // cour-specific aliases reject them at the overlap threshold even
    // when Phase 1 queried for the absolute number.
    let franchise_precompute;
    let absolute_target;
    if series_ctx.absolute_offset > 0
        && !series_ctx.franchise_aliases.is_empty()
        && let SearchTarget::Episode(ep) = target
    {
        absolute_target = SearchTarget::Episode(ep.saturating_add(series_ctx.absolute_offset));
        franchise_precompute = SiblingRejectPrecompute::build(&series_ctx.franchise_aliases, &[]);
        let franchise_queries = append_custom_tokens(
            build_queries_from_aliases(
                &series_ctx.franchise_aliases,
                &absolute_target,
                !series_ctx.restrict_user.is_empty(),
            ),
            &series_ctx.custom_tokens,
        );
        let franchise_ctx = AutoQueryCtx {
            phase: MatchPhase::Franchise,
            aliases: &series_ctx.franchise_aliases,
            sibling_precompute: &franchise_precompute,
            target: &absolute_target,
            absolute_offset: 0,
            ..ctx
        };
        run_queries(
            &franchise_queries,
            franchise_ctx,
            &mut seen,
            &mut candidates,
        )
        .await;
    }

    // Classify + filter + rescore in one pass. Each candidate is classified
    // exactly once, and both the BdOnly filter and the classification-aware
    // scoring reuse that single result.
    let mut scored: Vec<SearchResult> = Vec::with_capacity(candidates.len());
    for mut c in candidates.drain(..) {
        let classification = source::classify_release(
            db,
            &c.title,
            Some(&c.resolution),
            Some(source::NyaaContext {
                info_hash: &c.info_hash,
                view_url: &c.link,
                is_batch: c.is_batch,
            }),
            Some(source::SeriesContext {
                status: &detail.status,
                season_year: detail.season_year,
                end_year: detail.end_year,
            }),
        )
        .await;

        // BdOnly filter: drop non-BluRay releases for finished series when the
        // user has asked for BD only. Unknown sources get a pass.
        if is_finished
            && finished_mode == quality::FinishedSeriesMode::BdOnly
            && !source::passes_bd_only_filter(&classification)
        {
            continue;
        }

        let base = rescore_for_auto_search(
            &c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
            series_ctx.absolute_offset,
        );
        if let Some(final_score) = apply_cf_seadex_overlay(
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            config.custom_format_minimum_score,
        ) {
            c.score = final_score;
            scored.push(c);
        }
    }

    scored.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    scored
}

/// Shared context for `run_queries` — everything that stays constant
/// across the multi-phase query sweep inside `find_best_for_target`.
/// Bundling these into a struct (and away from the positional arg list)
/// closes a real foot-gun: the function used to take four back-to-back
/// `&[String]` slices (queries, aliases, preferred_groups, categories)
/// that the compiler would happily let you shuffle into the wrong
/// order. Named fields make the swap impossible. Derive `Copy` so the
/// Phase 1.5 alias override can reuse most fields via
/// `AutoQueryCtx { aliases: &all_aliases, ..ctx }`.
#[derive(Clone, Copy)]
struct AutoQueryCtx<'a> {
    /// Which query pass this context runs; stamped on every candidate.
    phase: MatchPhase,
    aliases: &'a [String],
    /// Precomputed token sets for own + sibling aliases, used by
    /// [`sibling_match_rejects`] to reject a release that looks MORE
    /// like a sequel/prequel/side-story than the target. Built once
    /// at the top of the collect function so the ~50-candidates ×
    /// ~5-siblings normalize/tokenize loop runs once per sweep
    /// instead of once per candidate. See `collect_sibling_aliases`
    /// for the JJK S1→S3 motivating case.
    sibling_precompute: &'a SiblingRejectPrecompute,
    preferred_groups: &'a [String],
    preferred_resolution: &'a str,
    target: &'a SearchTarget,
    allow_batch: bool,
    expected_season: i32,
    categories: &'a [String],
    batch_episode_match: bool,
    /// Lowercase info hashes SeaDex has flagged as "best" for this
    /// target's AniList ID. A candidate whose hash is in this set
    /// bypasses the title/season/episode heuristic filters — SeaDex
    /// has already confirmed the release by AniList ID, so any
    /// title-based check is strictly inferior. Without this bypass,
    /// a smol/neoDESU-style release titled `Monogatari (Season 9)`
    /// would be rejected for a Kizumonogatari Part 2 target because
    /// `parse_release_season` would see "Season 9" and disagree
    /// with the Part-2 expected season.
    seadex_hashes: &'a HashSet<String>,
    /// #23 — Nyaa uploader name to restrict searches to. Goes straight
    /// into `SearchOptions.user`, which Nyaa translates to `?u=<name>`
    /// — server-side filter, so fewer/faster responses. Empty string
    /// means no restriction. Resolved from the per-series override or
    /// the global default at the entry point.
    restrict_user: &'a str,
    /// #30 — Cumulative episode count across the shortest TV-format
    /// PREQUEL chain up to this target. Allows an episode-filter match
    /// on either the relative number (target_ep, AL's own numbering)
    /// OR the absolute number (target_ep + absolute_offset, which is
    /// what SubsPlease-style TV releases use for sequel cours). Zero
    /// for first-season entries and for series whose relation cache
    /// hasn't populated yet, which collapses to the legacy
    /// strict-relative behavior.
    absolute_offset: i32,
    /// Issue #28 — torznab/newznab indexers to fan out to
    /// alongside the Nyaa-direct fetch. Loaded once at the top of
    /// `collect_scored_for_target` so the per-`run_queries` call
    /// doesn't re-read the DB. Empty slice = Nyaa-only behavior
    /// (the v1.4 default; what every install sees until the user
    /// adds a row in Settings → Indexers).
    indexers: &'a [std::sync::Arc<dyn crate::services::indexers::Indexer>],
}

/// Same idea, but for the interactive-search helper which has a
/// smaller shared context and no batch override.
#[derive(Clone, Copy)]
struct InteractiveQueryCtx<'a> {
    /// Which query pass this context runs; stamped on every candidate.
    phase: MatchPhase,
    aliases: &'a [String],
    sibling_precompute: &'a SiblingRejectPrecompute,
    preferred_groups: &'a [String],
    preferred_resolution: &'a str,
    target: &'a SearchTarget,
    expected_season: i32,
    /// Configured torznab/newznab indexers to fan out to, snapshot-
    /// cloned out of `IndexerCache` once at the entry point. Empty
    /// slice = Nyaa-only behavior (matches the auto-search
    /// `AutoQueryCtx::indexers` contract). Without this the
    /// interactive picker per-episode flow only surfaces Nyaa-direct
    /// results — torznab matches that auto-search and the batch
    /// picker would surface stayed invisible.
    indexers: &'a [std::sync::Arc<dyn crate::services::indexers::Indexer>],
    /// Nyaa category filter set — one of `1_2` (English-translated),
    /// `1_0` (Anime All, includes raws/foreign subs), or the MUSIC
    /// pair. Computed from `config.allow_non_english` at the entry
    /// point via `quality::nyaa_categories_for_format`. Previously the
    /// interactive path hardcoded `1_0`, which silently leaked raw
    /// Japanese releases and non-English-sub foreign releases into
    /// results even when the user had left "Allow non-English" off.
    categories: &'a [String],
    /// See the note on `AutoQueryCtx::seadex_hashes`. The interactive
    /// path's consequences for failing the bypass are more severe
    /// than the auto path: `run_queries_interactive` applies
    /// `season_mismatch` *unconditionally*, including for Single
    /// (movie) targets, where the auto path's `matches_target`
    /// skips it. That's why the smol Kizumonogatari II release
    /// vanished from interactive search even though auto search
    /// surfaced it.
    seadex_hashes: &'a HashSet<String>,
    /// #23 — see `AutoQueryCtx::restrict_user`.
    restrict_user: &'a str,
    /// #30 — see `AutoQueryCtx::absolute_offset`.
    absolute_offset: i32,
}

/// Run a set of queries against Nyaa page 1, collecting valid candidates.
///
/// Queries run concurrently under the process-wide `nyaa::NYAA_CONCURRENCY`
/// semaphore — `buffer_unordered` starts up to `NYAA_BUFFER` futures in
/// parallel, each acquires a Nyaa permit before its HTTP request, so the
/// actual in-flight outbound request count never exceeds
/// `nyaa::NYAA_MAX_CONCURRENCY` regardless of how many `run_queries`
/// callers are active simultaneously. The buffer is larger than the
/// semaphore so a freed permit is always handed off to an already-
/// polling future, keeping the pipeline saturated.
///
/// Response ordering changes vs. the previous sequential loop — two
/// queries that surface the same release may resolve in either order,
/// so the `seen.insert` dedup now attributes the "winning" copy to
/// whichever query happened to land first. Downstream code scores
/// candidates independently and sorts by score, so the attribution
/// shift is invisible in the final output.
async fn run_queries(
    queries: &[String],
    ctx: AutoQueryCtx<'_>,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<SearchResult>,
) {
    let opts_list: Vec<SearchOptions> = ctx
        .categories
        .iter()
        .flat_map(|category| {
            queries.iter().map(move |query| SearchOptions {
                query: query.clone(),
                category: category.clone(),
                filter: "0".to_string(),
                user: ctx.restrict_user.to_string(),
                preferred_groups: ctx.preferred_groups.to_vec(),
                preferred_resolution: ctx.preferred_resolution.to_string(),
                prefer_subs: true,
            })
        })
        .collect();

    let responses: Vec<_> = stream::iter(opts_list)
        .map(|opts| async move { nyaa::search(&opts, 1).await })
        .buffer_unordered(nyaa::NYAA_BUFFER)
        .collect()
        .await;

    // Issue #28 — fan out to configured torznab/newznab
    // indexers concurrently with the Nyaa stream. The indexer
    // results land in the same `candidates` Vec via the same
    // dedup/match pipeline; downstream scoring sees a unified
    // pool. Empty `ctx.indexers` (the v1.4 default) skips this
    // pass entirely so behavior is unchanged for users who
    // haven't configured any indexers.
    //
    // PR #107 review fix #1+#3: collect raw `Release` records
    // (not pre-converted to SearchResult) so `dedup_for_auto_search`
    // can apply priority-based attribution + max-seeders aggregation
    // across indexers reporting the same infohash. Per-query
    // fan-outs run concurrently via `buffer_unordered` so a slow
    // indexer holds up only its own slot, not subsequent queries.
    let indexer_releases: Vec<crate::services::indexers::Release> = if ctx.indexers.is_empty() {
        Vec::new()
    } else {
        let outcome_streams: Vec<_> = stream::iter(queries.iter().cloned())
            .map(|query| async move {
                let search_query = crate::services::indexers::SearchQuery {
                    q: query.clone(),
                    categories: Vec::new(),
                    limit: None,
                    offset: None,
                };
                (
                    query,
                    crate::services::indexers::fan_out_search(ctx.indexers, &search_query).await,
                )
            })
            .buffer_unordered(nyaa::NYAA_BUFFER)
            .collect()
            .await;
        let mut releases: Vec<crate::services::indexers::Release> = Vec::new();
        for (query, outcomes) in outcome_streams {
            for outcome in outcomes {
                match outcome.result {
                    Ok(rs) => releases.extend(rs),
                    Err(e) => {
                        tracing::debug!(
                            "indexer fan-out failed for '{}' on indexer #{} ({}): {}",
                            query,
                            outcome.indexer_id,
                            outcome.indexer_name,
                            e
                        );
                    }
                }
            }
        }
        // Cross-indexer + cross-query dedup with priority attribution
        // (decision #3): when two indexers report the same infohash,
        // the lowest-priority-number indexer wins attribution and
        // seeders aggregate via `max`. Without this the per-`run_queries`
        // `seen` HashSet (further down) would attribute by HashMap
        // iteration order — nondeterministic.
        crate::services::indexers::dedup_for_auto_search(releases)
    };
    let indexer_responses: Vec<crate::services::nyaa::SearchResult> = indexer_releases
        .into_iter()
        .map(|r| r.into_search_result())
        .collect();

    for resp in responses {
        let results = match resp {
            Ok(v) => v.results,
            Err(_) => continue,
        };
        for mut result in results {
            let dedupe_key = if !result.info_hash.is_empty() {
                result.info_hash.clone()
            } else {
                result.title.to_lowercase()
            };
            if !seen.insert(dedupe_key) {
                continue;
            }
            // SeaDex trusts its AniList-ID-based curation over any
            // title heuristic. A hash match here means the release
            // is the community-curated best for this series, even
            // if its Nyaa title carries a season marker that would
            // otherwise fail `matches_target` (e.g. smol's
            // `Monogatari (Season 9)` release for a Kizumonogatari
            // Part 2 target).
            //
            // Batch filter runs unconditionally even for SeaDex
            // matches: an episode-search target with `allow_batch=
            // false` is an explicit "don't pull batches during
            // per-episode search" request from the user, and
            // silently letting SeaDex-curated batches through would
            // bypass that setting. SeaDex bypasses *heuristic* title
            // matching, not the user's batch-allowed policy.
            if !ctx.allow_batch && result.is_batch {
                continue;
            }
            let provenance = if is_seadex_match(&result.info_hash, ctx.seadex_hashes) {
                tracing::debug!(
                    "seadex: bypassing heuristic filters for SeaDex-best release title={:?} hash={}",
                    result.title,
                    result.info_hash
                );
                MatchProvenance::seadex(ctx.phase)
            } else {
                match classify_match(
                    &result.title,
                    ctx.aliases,
                    ctx.sibling_precompute,
                    ctx.target,
                    ctx.expected_season,
                    ctx.batch_episode_match && result.is_batch,
                    ctx.absolute_offset,
                ) {
                    Some(m) => m.into_provenance(ctx.phase),
                    None => continue,
                }
            };
            result.match_provenance = Some(provenance);
            candidates.push(result);
        }
    }

    // Indexer results go through the same dedup + match gate as
    // Nyaa results so downstream scoring is uniform. The dedup
    // key (info_hash | title) catches the case of an indexer
    // surfacing a release Nyaa already returned via Prowlarr's
    // Nyaa indexer — no double-counting.
    for mut result in indexer_responses {
        let dedupe_key = if !result.info_hash.is_empty() {
            result.info_hash.clone()
        } else {
            result.title.to_lowercase()
        };
        if !seen.insert(dedupe_key) {
            continue;
        }
        if !ctx.allow_batch && result.is_batch {
            continue;
        }
        let provenance = if is_seadex_match(&result.info_hash, ctx.seadex_hashes) {
            MatchProvenance::seadex(ctx.phase)
        } else {
            match classify_match(
                &result.title,
                ctx.aliases,
                ctx.sibling_precompute,
                ctx.target,
                ctx.expected_season,
                ctx.batch_episode_match && result.is_batch,
                ctx.absolute_offset,
            ) {
                Some(m) => m.into_provenance(ctx.phase),
                None => continue,
            }
        };
        result.match_provenance = Some(provenance);
        candidates.push(result);
    }
}

/// Run queries for interactive search with relaxed matching.
/// Uses relaxed alias matching (0.5 threshold) but still filters by season
/// and episode to avoid showing results from wrong seasons. Allows batch
/// results so users can see and pick them.
async fn run_queries_interactive(
    queries: &[String],
    ctx: InteractiveQueryCtx<'_>,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<SearchResult>,
) {
    let opts_list: Vec<SearchOptions> = ctx
        .categories
        .iter()
        .flat_map(|category| {
            queries.iter().map(move |query| SearchOptions {
                query: query.clone(),
                category: category.clone(),
                filter: "0".to_string(),
                user: ctx.restrict_user.to_string(),
                preferred_groups: ctx.preferred_groups.to_vec(),
                preferred_resolution: ctx.preferred_resolution.to_string(),
                prefer_subs: true,
            })
        })
        .collect();

    let responses: Vec<_> = stream::iter(opts_list)
        .map(|opts| async move { nyaa::search(&opts, 1).await })
        .buffer_unordered(nyaa::NYAA_BUFFER)
        .collect()
        .await;

    // Fan out to configured torznab/newznab indexers in parallel with
    // the Nyaa stream. Empty `ctx.indexers` skips the pass entirely;
    // users without indexer rows configured see Nyaa-only results,
    // identical to v1.4 behavior. Helper is named so it can be
    // tested in isolation against a wiremock'd torznab/newznab.
    let indexer_responses: Vec<SearchResult> =
        fan_out_indexers_for_interactive(queries, ctx.indexers).await;

    for resp in responses {
        let results = match resp {
            Ok(v) => v.results,
            Err(_) => continue,
        };
        for result in results {
            apply_interactive_filter_and_push(result, &ctx, seen, candidates);
        }
    }

    // Run torznab/newznab indexer results through the same relaxed-
    // alias / season / episode gate as Nyaa results. Dedup-key collision
    // (same infohash returned by both Nyaa and a torznab Prowlarr
    // mirror) takes the first-seen — Nyaa wins because it ran first
    // above, which is fine: per-component scoring is identical.
    for result in indexer_responses {
        apply_interactive_filter_and_push(result, &ctx, seen, candidates);
    }
}

/// Fan out interactive-search queries to configured torznab/newznab
/// indexers, dedup the resulting Releases (cross-indexer + cross-
/// query), and convert each survivor to the `SearchResult` shape so
/// the caller can run the same relaxed-alias / season / episode gate
/// it runs over Nyaa results. Empty `indexers` returns an empty Vec
/// without firing any HTTP — preserves the v1.4 Nyaa-only baseline
/// for users who haven't configured any torznab/newznab rows.
///
/// Pulled out of `run_queries_interactive` so the indexer-side of
/// the bug fix can be tested directly against a wiremock without
/// also having to mock Nyaa. The auto-search path's `run_queries`
/// has the same shape inline; deduping the two would require
/// threading a `seen` HashSet through, and the savings aren't worth
/// the indirection.
async fn fan_out_indexers_for_interactive(
    queries: &[String],
    indexers: &[std::sync::Arc<dyn crate::services::indexers::Indexer>],
) -> Vec<SearchResult> {
    if indexers.is_empty() {
        return Vec::new();
    }
    let outcome_streams: Vec<_> = stream::iter(queries.iter().cloned())
        .map(|query| async move {
            let search_query = crate::services::indexers::SearchQuery {
                q: query.clone(),
                categories: Vec::new(),
                limit: None,
                offset: None,
            };
            crate::services::indexers::fan_out_search(indexers, &search_query).await
        })
        .buffer_unordered(nyaa::NYAA_BUFFER)
        .collect()
        .await;
    let mut releases: Vec<crate::services::indexers::Release> = Vec::new();
    for outcomes in outcome_streams {
        for outcome in outcomes {
            match outcome.result {
                Ok(rs) => releases.extend(rs),
                Err(e) => {
                    tracing::debug!(
                        "interactive indexer fan-out failed for indexer #{} ({}): {}",
                        outcome.indexer_id,
                        outcome.indexer_name,
                        e
                    );
                }
            }
        }
    }
    crate::services::indexers::dedup_for_auto_search(releases)
        .into_iter()
        .map(|r| r.into_search_result())
        .collect()
}

/// Per-result interactive filter. Pulled out of `run_queries_interactive`
/// so both the Nyaa loop and the indexer-fan-out loop apply the
/// same relaxed-alias / sibling-rejection / season / episode gate
/// without code duplication.
fn apply_interactive_filter_and_push(
    mut result: SearchResult,
    ctx: &InteractiveQueryCtx<'_>,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<SearchResult>,
) {
    // Dedup is namespaced by source: Nyaa-direct results dedup
    // against each other (Nyaa returns the same release across
    // multiple alias-query passes), and indexer results dedup
    // per-indexer (`<id>:<hash>`). A release that surfaces from
    // BOTH Nyaa and an indexer (e.g. a Prowlarr Nyaa-mirror, or a
    // tracker that re-uploads public releases) shows up as TWO
    // rows — once attributed to Nyaa, once to the indexer — so the
    // user can pick a preferred tracker.
    //
    // This implements the "interactive search policy (decision
    // #3)" called out in `services::indexers::dedup_for_auto_search`'s
    // doc comment: per-(indexer, infohash) rows so the user has
    // actual attribution to pick from. The pre-fix dedup keyed
    // only on `info_hash`, which silently collapsed indexer rows
    // into their Nyaa twin and made the new Indexer column always
    // read "Nyaa" for any release Nyaa also carried — exactly the
    // symptom that surfaced post-rollout when nekoBT searches
    // returned successful responses but the modal showed no
    // nekoBT-attributed rows.
    let source_tag = match result.indexer_id {
        Some(id) => id.to_string(),
        None => "nyaa".to_string(),
    };
    let dedupe_key = if !result.info_hash.is_empty() {
        format!("{source_tag}:{}", result.info_hash)
    } else {
        format!("{source_tag}:{}", result.title.to_lowercase())
    };
    if !seen.insert(dedupe_key) {
        return;
    }
    // SeaDex trusts its AniList-ID-based curation over any title
    // heuristic. If this hash is in the set, skip all alias / season /
    // episode checks below — the unconditional `season_mismatch` in
    // particular drops releases like smol's `Monogatari (Season 9)`
    // for a Kizumonogatari Part 2 target, even though SeaDex has
    // already confirmed the AniList ID match.
    if is_seadex_match(&result.info_hash, ctx.seadex_hashes) {
        tracing::debug!(
            "seadex: bypassing heuristic filters for SeaDex-best release title={:?} hash={}",
            result.title,
            result.info_hash
        );
        result.match_provenance = Some(MatchProvenance::seadex(ctx.phase));
        candidates.push(result);
        return;
    }
    // Relaxed alias matching: lower threshold than auto search. The
    // fuzzy half scores distinctive tokens only (#219) so a "The
    // Animation" release can't ride in on the format words alone.
    let normalized_title = normalize_title(&result.title);
    let title_tokens = token_set(&normalized_title);
    // Relaxed alias matching: lower threshold than auto search and no
    // surplus budget, so users see a broader set of candidates to pick
    // from. The fuzzy half scores distinctive tokens only (#219).
    let Some(alias_match) = aliases::best_alias_match(
        &normalized_title,
        &title_tokens,
        ctx.aliases,
        aliases::RELAXED_ALIAS_POLICY,
    ) else {
        return;
    };
    // Sibling rejection: same sequel/prequel guard as the auto path —
    // a release that matches a sibling more tightly than us is almost
    // certainly for the sibling.
    if sibling_match_rejects(&normalized_title, &title_tokens, ctx.sibling_precompute) {
        return;
    }
    // Season check: reject results clearly from a different season
    if season_mismatch(&result.title, ctx.expected_season) {
        return;
    }
    // Episode check for single-episode targets (allow batches through).
    // #30 — A release passes if its parsed number matches either the
    // relative target (AL's per-cour numbering) OR the absolute number
    // `target + absolute_offset` (what SubsPlease-style TV releases use
    // for sequel cours, e.g. JJK S3 E9 shipped as "Jujutsu Kaisen -
    // 56" with offset 47). When offset is 0 this collapses to the
    // legacy strict-relative behavior.
    if let SearchTarget::Episode(target_ep) = ctx.target
        && !result.is_batch
    {
        let parsed = parse_release_numbers(&result.title);
        if !parsed.is_empty() && !episode_match(&parsed, *target_ep, ctx.absolute_offset) {
            return;
        }
    }
    result.match_provenance = Some(alias_match.into_provenance(ctx.phase));
    candidates.push(result);
}

/// #30 — Episode-filter acceptance check. A release's parsed episode
/// numbers match the target when they carry either the relative target
/// number (AL's own per-cour numbering) or the absolute number derived
/// by adding the cumulative prior-cour episode count. `offset == 0`
/// reduces to the strict-relative path used for first-season entries
/// and for series whose relation cache hasn't populated yet.
pub(super) fn episode_match(parsed: &HashSet<i32>, target_ep: i32, absolute_offset: i32) -> bool {
    if parsed.contains(&target_ep) {
        return true;
    }
    if absolute_offset > 0 {
        let absolute = target_ep.saturating_add(absolute_offset);
        if parsed.contains(&absolute) {
            return true;
        }
    }
    false
}

/// Build group-prefixed queries for the fallback search.
/// e.g. "SubsPlease Jujutsu Kaisen - 01", "SubsPlease Jujutsu Kaisen 01"
fn build_group_queries(
    detail: &AnimeDetail,
    target: &SearchTarget,
    preferred_groups: &[String],
) -> Vec<String> {
    // Skip `collect_aliases_with_variants` here — that helper also
    // builds the combined own+variant list (for `matches_target`'s
    // alias pool), which `build_group_queries` never consumes. Fetch
    // the two pieces we actually need directly and leave the combined
    // allocation to the call sites that use it.
    let canonical_aliases = collect_aliases(detail);
    let variant_aliases = sequel_variant_aliases(&canonical_aliases);
    let mut queries = Vec::new();

    for group in preferred_groups {
        for alias in &canonical_aliases {
            match target {
                SearchTarget::Single => {
                    queries.push(format!("{} {}", group, alias));
                }
                SearchTarget::Episode(ep) => {
                    queries.push(format!("{} {} - {:02}", group, alias, ep));
                    queries.push(format!("{} {} {:02}", group, alias, ep));
                }
            }
        }
        // Variants run collapsed: one query per (group × variant). Every
        // Nyaa query is a sequential HTTP round-trip, and the two-form
        // fan-out on canonical aliases already covers the hyphen vs
        // bare-number convention; variants emit the zero-padded form
        // only because it's the most common release-group shape.
        for variant in &variant_aliases {
            match target {
                SearchTarget::Single => {
                    queries.push(format!("{} {}", group, variant));
                }
                SearchTarget::Episode(ep) => {
                    queries.push(format!("{} {} {:02}", group, variant, ep));
                }
            }
        }
    }

    dedupe_strings(queries)
}

/// Resolve the best-available classification for an on-disk episode. Public
/// so RSS upgrade detection can use the same hydration order.
pub fn resolve_existing_classification(
    file: &media::EpisodeFile,
    tag: Option<&crate::models::episode_tags::EpisodeQualityTag>,
) -> ClassificationResult {
    if let Some(tag) = tag {
        if !tag.source.is_empty() || !tag.resolution.is_empty() {
            return source::classification_from_stored_full(
                &tag.source,
                &tag.resolution,
                tag.is_remux,
                tag.is_bdmv,
                source::WebKind::from_str(&tag.web_kind),
                tag.classification_confidence,
                tag.needs_review,
            );
        }
        if !tag.release_title.is_empty() {
            return source::classify_release_sync(&tag.release_title, None);
        }
    }
    // No usable tag — fall back to the on-disk filename + parsed quality.
    source::classify_release_sync(&file.filename, Some(&file.quality))
}

pub fn target_label(target: &SearchTarget) -> String {
    match target {
        SearchTarget::Single => "Single".to_string(),
        SearchTarget::Episode(ep) => format!("Episode {}", ep),
    }
}

/// Per-series search context resolved from the `series` row (with
/// fallbacks to global `config` defaults for the user-controlled
/// overrides). One DB hit per entry-point call for the series row
/// plus one extra when `absolute_offset > 0` to walk the franchise
/// root titles.
struct SeriesSearchCtx {
    /// #23 — Extra tokens appended verbatim to every Nyaa query after
    /// the title aliases. Empty means no extra tokens.
    custom_tokens: String,
    /// #23 — Nyaa uploader name (`?u=<name>`) server-side filter.
    /// Empty means no restriction.
    restrict_user: String,
    /// #30 — Cumulative TV-cour episode count for the entry's PREQUEL
    /// chain. Zero for first-season entries and for series whose
    /// relation cache hasn't populated yet. Used by the episode filter
    /// to accept absolute-numbered Nyaa releases against a
    /// relative-numbered AL target.
    absolute_offset: i32,
    /// #30 — Titles of every TV-format ancestor on the PREQUEL chain.
    /// Used to build queries like `Jujutsu Kaisen 56` that a Nyaa text
    /// search will actually match against a SubsPlease-shaped release
    /// title. The cour-specific AL titles (e.g. "JUJUTSU KAISEN Season
    /// 3: The Culling Game Part 1", "Jujutsu Kaisen: Shimetsu Kaiyuu
    /// Zenpen") don't appear in SubsPlease release names, so without
    /// these franchise-root titles the absolute-numbered release is
    /// never in the candidate pool — loosening the filter alone is
    /// not enough. Empty for first-season entries.
    franchise_aliases: Vec<String>,
}

/// Resolve per-series search overrides + the cumulative-prior-episodes
/// offset, falling back to global defaults from `config`. Per-series
/// user overrides (`#23`) win when non-empty; the `#30` offset and
/// franchise aliases have no global default (both are derived from
/// the per-series relation cache).
async fn resolve_search_overrides(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
) -> SeriesSearchCtx {
    let row = crate::models::series::get_by_anilist_id(db, detail.id)
        .await
        .ok()
        .flatten();
    match row {
        Some(s) => resolve_search_overrides_from_row_async(db, &s, config).await,
        None => SeriesSearchCtx {
            custom_tokens: config.default_custom_query_tokens.clone(),
            restrict_user: config.default_restrict_to_uploader.clone(),
            // No series row means the entry isn't in the library yet;
            // no relation cache to pull an offset from, so the filter
            // stays strict-relative. This only affects provisional
            // Sonarr-shim searches for unadded series.
            absolute_offset: 0,
            franchise_aliases: Vec::new(),
        },
    }
}

/// Async entry-point variant — hits the DB for franchise aliases when
/// the series has a non-zero offset. The sync test variant below is
/// kept for unit tests that don't need the alias lookup.
async fn resolve_search_overrides_from_row_async(
    db: &SqlitePool,
    series: &crate::models::series::Series,
    config: &Config,
) -> SeriesSearchCtx {
    let mut ctx = resolve_search_overrides_from_row(series, config);
    if ctx.absolute_offset > 0 && series.anilist_id != 0 {
        ctx.franchise_aliases =
            crate::models::local_metadata::resolve_franchise_aliases(db, series.anilist_id).await;
    }
    ctx
}

fn resolve_search_overrides_from_row(
    series: &crate::models::series::Series,
    config: &Config,
) -> SeriesSearchCtx {
    let custom_tokens = if series.custom_query_tokens.is_empty() {
        config.default_custom_query_tokens.clone()
    } else {
        series.custom_query_tokens.clone()
    };
    let restrict_user = if series.restrict_to_uploader.is_empty() {
        config.default_restrict_to_uploader.clone()
    } else {
        series.restrict_to_uploader.clone()
    };
    SeriesSearchCtx {
        custom_tokens,
        restrict_user,
        absolute_offset: series.cumulative_prior_episodes.max(0),
        // Left empty in the sync variant — callers that need them use
        // the async variant. Tests pin the sync variant's behavior on
        // the other fields only.
        franchise_aliases: Vec::new(),
    }
}

/// #23 — Append user-supplied custom query tokens to every query in
/// the list. Empty tokens is a no-op so the common path stays
/// allocation-free. Tokens are appended verbatim — users can pass any
/// Nyaa query syntax (quoted phrases, minus-prefix exclusions, etc.)
/// that `build_queries_from_aliases` didn't generate.
fn append_custom_tokens(queries: Vec<String>, tokens: &str) -> Vec<String> {
    let trimmed = tokens.trim();
    if trimmed.is_empty() {
        return queries;
    }
    queries
        .into_iter()
        .map(|q| format!("{} {}", q, trimmed))
        .collect()
}

/// Build the Nyaa text-query variants for each alias. The full sweep
/// emits four variants per alias for Episode targets (`title 9`,
/// `title - 09`, `title 09`, `"title" 09`) to cover punctuation and
/// padding conventions across uploaders, plus two variants for Single
/// targets (bare + phrase-match).
///
/// #23 follow-up — When a Nyaa uploader restriction (`?u=<name>`) is
/// active those variants collapse to the same token set against a
/// single uploader's catalog: Nyaa's tokenizer ignores punctuation,
/// and the phrase-match variant narrows a result set that's already
/// narrowed by the server-side user filter. Running all four in
/// sequence burned 15–25s per sweep for no additional coverage.
/// `collapsed = true` emits a single canonical variant per alias —
/// the zero-padded episode form (`title 09`) for Episode targets, the
/// bare alias for Single targets — cutting the per-alias query count
/// 4→1 (Episode) and 2→1 (Single).
/// Compute canonical aliases, variant aliases, and the combined match
/// list for a detail. The canonical aliases run through the full query
/// fan-out (`build_queries_from_aliases` with the call-site's `collapsed`
/// flag); the variants run through a single-query collapsed path to keep
/// the per-sweep Nyaa HTTP round-trip count bounded — each query is a
/// sequential network hit inside `run_queries`, so a 6-extra-aliases ×
/// 4-queries-per-alias fan-out doubled the end-to-end search latency
/// visibly in the wild (issue #84 follow-up).
fn collect_aliases_with_variants(detail: &AnimeDetail) -> (Vec<String>, Vec<String>, Vec<String>) {
    let canonical = collect_aliases(detail);
    let variants = sequel_variant_aliases(&canonical);
    let combined = if variants.is_empty() {
        canonical.clone()
    } else {
        dedupe_strings(canonical.iter().chain(variants.iter()).cloned().collect())
    };
    (combined, canonical, variants)
}

/// Build the per-sweep query list — canonical aliases at full fan-out,
/// variants collapsed to one query each. See the docstring on
/// `collect_aliases_with_variants` for the latency rationale.
fn build_queries_mixed(
    canonical: &[String],
    variants: &[String],
    target: &SearchTarget,
    collapsed: bool,
) -> Vec<String> {
    let mut queries = build_queries_from_aliases(canonical, target, collapsed);
    if !variants.is_empty() {
        queries.extend(build_queries_from_aliases(variants, target, true));
    }
    dedupe_strings(queries)
}

fn build_queries_from_aliases(
    aliases: &[String],
    target: &SearchTarget,
    collapsed: bool,
) -> Vec<String> {
    let mut queries = Vec::new();

    for alias in aliases {
        match target {
            SearchTarget::Single => {
                queries.push(alias.clone());
                if !collapsed {
                    queries.push(format!("\"{}\"", alias));
                }
            }
            SearchTarget::Episode(ep) => {
                if collapsed {
                    queries.push(format!("{} {:02}", alias, ep));
                } else {
                    queries.push(format!("{} {}", alias, ep));
                    queries.push(format!("{} - {:02}", alias, ep));
                    queries.push(format!("{} {:02}", alias, ep));
                    queries.push(format!("\"{}\" {:02}", alias, ep));
                }
            }
        }
    }

    dedupe_strings(queries)
}

/// Human-readable short label for a series, used in SeaDex lookup log
/// rows. Prefers the English title and falls back to romaji so users
/// browsing the Log Viewer see the same title the Auto Search banner
/// uses.
fn display_title(detail: &AnimeDetail) -> &str {
    if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    }
}

/// Map the config's resolution preference to the bare-number string form that
/// Nyaa search options expect ("480", "720", "1080", "2160").
fn preferred_resolution_search_value(config: &Config) -> String {
    match Resolution::from_str(&config.preferred_resolution) {
        Resolution::R480p => "480".to_string(),
        Resolution::R576p => "576".to_string(),
        Resolution::R720p => "720".to_string(),
        Resolution::R1080p => "1080".to_string(),
        Resolution::R2160p => "2160".to_string(),
        Resolution::Unknown => "1080".to_string(),
    }
}

/// Numbers that look like episode numbers but are actually technical metadata.
#[cfg(test)]
mod tests {
    use super::*;

    // ── fan_out_indexers_for_interactive ─────────────────────────────
    //
    // Regression coverage for the interactive-search bug where torznab/
    // newznab indexers were wired up correctly for auto-search and RSS
    // but completely skipped on the per-episode interactive picker.
    // `find_all_for_target` didn't accept an indexer cache and
    // `run_queries_interactive` didn't fan out — every result came
    // straight from Nyaa and the user saw no nekoBT hits even when
    // logs proved the indexer WAS being queried (by auto-search /
    // RSS, on a separate code path). These tests pin the helper that
    // closed the gap.

    /// Empty indexer slice = no-op + no HTTP. Preserves the v1.4
    /// Nyaa-only baseline for users who haven't configured any
    /// torznab/newznab rows. Without this guard, every interactive
    /// search would pay a futures-iter setup cost and the bug-fix
    /// would have landed as a regression for the majority of users.
    #[tokio::test]
    async fn fan_out_skips_when_no_indexers_configured() {
        let result = fan_out_indexers_for_interactive(
            &["any query".to_string()],
            &[], // no indexers configured
        )
        .await;
        assert!(
            result.is_empty(),
            "empty indexer slice must short-circuit to an empty Vec without firing any HTTP"
        );
    }

    /// Torznab indexer end-to-end: configure a wiremock'd torznab
    /// indexer, call the helper, assert that the resulting
    /// SearchResult carries the indexer's name (so the UI's
    /// "Indexer" column attributes it correctly).
    #[tokio::test]
    async fn fan_out_torznab_indexer_results_carry_indexer_name() {
        use crate::services::indexers::torznab::TorznabIndexer;
        use std::sync::Arc;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
<item>
  <title>[nekoBT] Test Show - 01</title>
  <guid>g1</guid>
  <enclosure url="http://server/dl/abc" length="1000000000" type="application/x-bittorrent"/>
  <torznab:attr name="seeders" value="42"/>
  <torznab:attr name="leechers" value="0"/>
  <torznab:attr name="infohash" value="ABCDEF1234567890"/>
</item>
</channel>
</rss>"#,
            ))
            .mount(&server)
            .await;

        let row = crate::models::indexers::Indexer {
            id: 7,
            name: "nekoBT".to_string(),
            kind: crate::models::indexers::KIND_TORZNAB.to_string(),
            url: format!("{}/api", server.uri()),
            api_key: "k".to_string(),
            priority: 25,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 0,
            request_timeout_secs: Some(5),
            download_client_id: None,
            rss_enabled: false,
            rss_last_polled_at: None,
            rss_last_poll_error: String::new(),
            rss_last_item_count: 0,
            caps_json: String::new(),
            caps_refreshed_at: None,
            created_at: 0,
            updated_at: 0,
        };
        let indexer = TorznabIndexer::from_row_arc(&row).expect("indexer must build");
        let indexers: Vec<Arc<dyn crate::services::indexers::Indexer>> = vec![indexer];

        let results = fan_out_indexers_for_interactive(&["Test Show".to_string()], &indexers).await;
        assert_eq!(
            results.len(),
            1,
            "expected one result from the wiremock'd torznab"
        );
        assert_eq!(results[0].title, "[nekoBT] Test Show - 01");
        assert_eq!(
            results[0].indexer_name, "nekoBT",
            "indexer_name must propagate end-to-end (Release → SearchResult) so the \
             UI 'Indexer' column attributes the row to nekoBT"
        );
        assert_eq!(results[0].indexer_id, Some(7));
    }

    /// Per-source dedup invariant: a release that surfaces from
    /// BOTH Nyaa and an indexer with the same infohash must appear
    /// as TWO rows in the candidate pool, one per source. This is
    /// the explicit "interactive search policy (decision #3)" from
    /// `services::indexers::dedup_for_auto_search`'s doc.
    ///
    /// Pre-fix the dedup keyed only on `info_hash`, which silently
    /// merged the indexer row into its Nyaa twin. Result: the
    /// "Indexer" column always read "Nyaa" for any release Nyaa
    /// also carried. nekoBT-attributable rows became invisible to
    /// the user even when nekoBT was returning identical results.
    /// The user reported this verbatim post-fan-out rollout.
    #[tokio::test]
    async fn interactive_filter_dedup_is_per_source_not_per_hash() {
        // Build a HashSet + candidates Vec by hand and exercise
        // `apply_interactive_filter_and_push` directly with two
        // SearchResults sharing an infohash but differing in
        // `indexer_id` (None = Nyaa, Some(7) = nekoBT). Pre-fix
        // both went through the same `seen.insert(hash)` and the
        // second got skipped; post-fix the source-namespaced key
        // lets both through.
        use std::collections::HashSet;
        let nyaa_result = SearchResult {
            match_provenance: None,
            title: "[smol] Nisemonogatari".to_string(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes: 0,
            seeders: 50,
            leechers: 0,
            downloads: 0,
            group: "smol".to_string(),
            resolution: "1080".to_string(),
            quality_label: String::new(),
            source: String::new(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: true,
            is_trusted: true,
            score: 100,
            info_hash: "deadbeef".to_string(),
            score_breakdown: Vec::new(),
            upload_date: String::new(),
            indexer_id: None,
            indexer_name: String::new(),
        };
        let mut indexer_result = nyaa_result.clone();
        indexer_result.indexer_id = Some(7);
        indexer_result.indexer_name = "nekoBT".to_string();

        // Build a permissive ctx so the alias / season / episode
        // gate downstream of the dedup doesn't reject anything.
        let aliases = vec!["Monogatari".to_string(), "Nisemonogatari".to_string()];
        let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &[]);
        let preferred_groups: Vec<String> = Vec::new();
        let target = SearchTarget::Single;
        let seadex_hashes = std::collections::HashSet::new();
        let categories = vec!["1_2".to_string()];
        let ctx = InteractiveQueryCtx {
            phase: MatchPhase::Primary,
            aliases: &aliases,
            sibling_precompute: &sibling_precompute,
            preferred_groups: &preferred_groups,
            preferred_resolution: "1080p",
            target: &target,
            expected_season: 0,
            seadex_hashes: &seadex_hashes,
            restrict_user: "",
            absolute_offset: 0,
            categories: &categories,
            indexers: &[],
        };

        let mut seen: HashSet<String> = HashSet::new();
        let mut candidates: Vec<SearchResult> = Vec::new();

        apply_interactive_filter_and_push(nyaa_result, &ctx, &mut seen, &mut candidates);
        apply_interactive_filter_and_push(indexer_result, &ctx, &mut seen, &mut candidates);

        assert_eq!(
            candidates.len(),
            2,
            "Same infohash from Nyaa + nekoBT must produce two rows so the user \
             can pick a preferred source. A regression here would hide every \
             nekoBT-attributed row whose hash also lives on Nyaa."
        );
        assert!(
            candidates.iter().any(|c| c.indexer_name.is_empty()),
            "Nyaa-direct row (empty indexer_name) must be present"
        );
        assert!(
            candidates.iter().any(|c| c.indexer_name == "nekoBT"),
            "nekoBT-attributed row must be present"
        );
    }

    /// Counterpart: the SAME source returning the SAME hash twice
    /// (which happens because `run_queries_interactive` runs N
    /// alias-prefixed queries against Nyaa and they overlap) MUST
    /// still dedup. Without this branch surviving the per-source
    /// rewrite, every Nyaa result would land 3-5x in the candidate
    /// pool — the user would see massive duplication.
    #[tokio::test]
    async fn interactive_filter_still_dedups_within_a_single_source() {
        use std::collections::HashSet;
        let nyaa_result = SearchResult {
            match_provenance: None,
            title: "[smol] Nisemonogatari".to_string(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes: 0,
            seeders: 50,
            leechers: 0,
            downloads: 0,
            group: "smol".to_string(),
            resolution: "1080".to_string(),
            quality_label: String::new(),
            source: String::new(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: true,
            is_trusted: true,
            score: 100,
            info_hash: "deadbeef".to_string(),
            score_breakdown: Vec::new(),
            upload_date: String::new(),
            indexer_id: None,
            indexer_name: String::new(),
        };

        let aliases = vec!["Monogatari".to_string()];
        let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &[]);
        let preferred_groups: Vec<String> = Vec::new();
        let target = SearchTarget::Single;
        let seadex_hashes = std::collections::HashSet::new();
        let categories = vec!["1_2".to_string()];
        let ctx = InteractiveQueryCtx {
            phase: MatchPhase::Primary,
            aliases: &aliases,
            sibling_precompute: &sibling_precompute,
            preferred_groups: &preferred_groups,
            preferred_resolution: "1080p",
            target: &target,
            expected_season: 0,
            seadex_hashes: &seadex_hashes,
            restrict_user: "",
            absolute_offset: 0,
            categories: &categories,
            indexers: &[],
        };

        let mut seen: HashSet<String> = HashSet::new();
        let mut candidates: Vec<SearchResult> = Vec::new();

        apply_interactive_filter_and_push(nyaa_result.clone(), &ctx, &mut seen, &mut candidates);
        apply_interactive_filter_and_push(nyaa_result, &ctx, &mut seen, &mut candidates);

        assert_eq!(
            candidates.len(),
            1,
            "Two Nyaa hits with identical infohash must collapse to one row \
             (otherwise alias-query overlap floods the candidate pool)"
        );
    }

    /// Newznab variant — identical wire format, identical client,
    /// just `kind = "newznab"`. Locks in that the indexer-name
    /// plumbing isn't accidentally torznab-specific. Without this,
    /// a future refactor that special-cased torznab would break
    /// usenet attribution silently.
    #[tokio::test]
    async fn fan_out_newznab_indexer_results_carry_indexer_name() {
        use crate::services::indexers::torznab::TorznabIndexer;
        use std::sync::Arc;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<?xml version="1.0"?>
<rss version="2.0">
<channel>
<item>
  <title>Test.Show.S01E01.WEB-DL</title>
  <guid>nzb-g1</guid>
  <enclosure url="http://server/nzb/abc" length="500000000" type="application/x-nzb"/>
  <torznab:attr name="size" value="500000000"/>
</item>
</channel>
</rss>"#,
            ))
            .mount(&server)
            .await;

        let row = crate::models::indexers::Indexer {
            id: 9,
            name: "NZBGeek".to_string(),
            kind: crate::models::indexers::KIND_NEWZNAB.to_string(),
            url: format!("{}/api", server.uri()),
            api_key: "k".to_string(),
            priority: 30,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 0,
            request_timeout_secs: Some(5),
            download_client_id: None,
            rss_enabled: false,
            rss_last_polled_at: None,
            rss_last_poll_error: String::new(),
            rss_last_item_count: 0,
            caps_json: String::new(),
            caps_refreshed_at: None,
            created_at: 0,
            updated_at: 0,
        };
        let indexer = TorznabIndexer::from_row_arc(&row).expect("indexer must build");
        let indexers: Vec<Arc<dyn crate::services::indexers::Indexer>> = vec![indexer];

        let results = fan_out_indexers_for_interactive(&["Test Show".to_string()], &indexers).await;
        assert_eq!(
            results.len(),
            1,
            "expected one result from the wiremock'd newznab"
        );
        assert_eq!(
            results[0].indexer_name, "NZBGeek",
            "newznab indexers must surface their name through the same plumbing as \
             torznab — the UI Indexer column needs to attribute usenet hits too"
        );
        assert_eq!(results[0].indexer_id, Some(9));
    }

    // ── detect_sibling_entries_in_pack ──────────────────────────────

    fn pinned_720p_web_tag(
        manual_override: bool,
    ) -> crate::models::episode_tags::EpisodeQualityTag {
        crate::models::episode_tags::EpisodeQualityTag {
            episode_number: 1,
            quality_tag: "WEB-720p".to_string(),
            release_title: "[Group] Show - 01 [WEB-DL 720p].mkv".to_string(),
            release_group: "Group".to_string(),
            state: "completed".to_string(),
            source: "Web".to_string(),
            resolution: "720p".to_string(),
            is_remux: false,
            is_bdmv: false,
            web_kind: "WEBDL".to_string(),
            classification_confidence: 1.0,
            needs_review: false,
            manual_override,
            classification_evidence: String::new(),
            classification_attempted_at: None,
        }
    }

    fn dummy_720p_episode_file(episode_number: i32) -> media::EpisodeFile {
        media::EpisodeFile {
            filename: "[Group] Show - 01 [WEB-DL 720p].mkv".to_string(),
            episode_number,
            season_number: None,
            quality: "720p".to_string(),
            size_bytes: 0,
            size_display: String::new(),
        }
    }

    // Regression: build_upgrade_targets must skip rows the user has pinned
    // via manual override. Otherwise the upgrade sweep selects a "better"
    // release, post-processing replaces the on-disk file, and the
    // manual_override SQL guards on record_grab / update_classification
    // silently drop the tag write — the user loses their pinned file with
    // no audit trail.
    #[test]
    fn build_upgrade_targets_skips_manual_override_rows() {
        let file = dummy_720p_episode_file(1);
        let mut tags = std::collections::HashMap::new();
        tags.insert(1_i32, pinned_720p_web_tag(true));

        let targets = build_upgrade_targets(
            &[file],
            &[1],
            Source::BluRay,
            Resolution::R1080p,
            false,
            false,
            &tags,
        );
        assert!(
            targets.is_empty(),
            "manual_override row should be skipped, got {} target(s)",
            targets.len()
        );
    }

    // Sanity check the regression test: with the same file but
    // manual_override = false, the upgrade target IS produced. Confirms the
    // skip is the new behavior, not an unrelated "everything skips" bug.
    #[test]
    fn build_upgrade_targets_yields_target_when_not_manual_override() {
        let file = dummy_720p_episode_file(1);
        let mut tags = std::collections::HashMap::new();
        tags.insert(1_i32, pinned_720p_web_tag(false));

        let targets = build_upgrade_targets(
            &[file],
            &[1],
            Source::BluRay,
            Resolution::R1080p,
            false,
            false,
            &tags,
        );
        assert_eq!(targets.len(), 1, "auto-classified row should be upgraded");
    }

    // ── #23 — Search override resolver + token append ──────────────────────

    fn series_with_overrides(tokens: &str, user: &str) -> crate::models::series::Series {
        crate::models::series::Series {
            is_adult: false,
            id: 1,
            anilist_id: 1,
            mal_id: None,
            title: String::new(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: String::new(),
            episodes: None,
            season_year: None,
            end_year: None,
            folder_name: String::new(),
            monitor_mode: "future".to_string(),
            allow_upgrades: true,
            allow_pt_upgrades: false,
            custom_query_tokens: tokens.to_string(),
            restrict_to_uploader: user.to_string(),
            cumulative_prior_episodes: 0,
            monitor_mode_manual_override: false,
            user_score: None,
            added_at: String::new(),
        }
    }

    fn cfg_with_defaults(tokens: &str, user: &str) -> Config {
        Config {
            default_custom_query_tokens: tokens.to_string(),
            default_restrict_to_uploader: user.to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn resolve_overrides_per_series_wins_over_global() {
        let series = series_with_overrides("bd 1080p", "SubsPlease");
        let cfg = cfg_with_defaults("web 720p", "Erai-raws");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(ctx.custom_tokens, "bd 1080p");
        assert_eq!(ctx.restrict_user, "SubsPlease");
    }

    #[test]
    fn resolve_overrides_falls_back_to_global_when_series_blank() {
        let series = series_with_overrides("", "");
        let cfg = cfg_with_defaults("web 720p", "Erai-raws");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(ctx.custom_tokens, "web 720p");
        assert_eq!(ctx.restrict_user, "Erai-raws");
    }

    #[test]
    fn resolve_overrides_per_field_independent_fallback() {
        // One field set, the other blank — blank inherits, set wins.
        let series = series_with_overrides("", "SubsPlease");
        let cfg = cfg_with_defaults("web 720p", "Erai-raws");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(
            ctx.custom_tokens, "web 720p",
            "blank field should inherit global"
        );
        assert_eq!(
            ctx.restrict_user, "SubsPlease",
            "set field should beat global"
        );
    }

    #[test]
    fn resolve_overrides_surfaces_absolute_offset_from_series_row() {
        // #30 — series row carries the cached prior-cour episode count,
        // resolver lifts it verbatim onto the context used by the query
        // sweep.
        let mut series = series_with_overrides("", "");
        series.cumulative_prior_episodes = 47; // e.g. JJK S3 = S1(24) + S2(23)
        let cfg = cfg_with_defaults("", "");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(ctx.absolute_offset, 47);
    }

    #[test]
    fn resolve_overrides_negative_offset_clamped_to_zero() {
        // Defensive: a bad write somewhere upstream mustn't produce
        // negative episode numbers at the filter layer.
        let mut series = series_with_overrides("", "");
        series.cumulative_prior_episodes = -5;
        let cfg = cfg_with_defaults("", "");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(ctx.absolute_offset, 0);
    }

    #[test]
    fn append_tokens_is_noop_when_empty() {
        let qs = vec!["Frieren 01".to_string(), "Frieren - 01".to_string()];
        assert_eq!(append_custom_tokens(qs.clone(), ""), qs);
        assert_eq!(append_custom_tokens(qs.clone(), "   "), qs);
    }

    #[test]
    fn append_tokens_adds_to_each_query() {
        let qs = vec!["Frieren 01".to_string(), "Frieren - 01".to_string()];
        let out = append_custom_tokens(qs, "bd 1080p");
        assert_eq!(
            out,
            vec![
                "Frieren 01 bd 1080p".to_string(),
                "Frieren - 01 bd 1080p".to_string(),
            ]
        );
    }

    // ── #23 follow-up — collapsed query variants when ?u= is active ────

    #[test]
    fn build_queries_full_mode_emits_four_episode_variants() {
        // Regression pin. The full sweep is what runs when no Nyaa
        // uploader filter is set; dropping any of these variants would
        // silently break coverage for uploaders that skip padding,
        // use a specific separator, etc.
        let aliases = vec!["Frieren".to_string()];
        let out = build_queries_from_aliases(&aliases, &SearchTarget::Episode(9), false);
        assert_eq!(
            out.len(),
            4,
            "full-sweep episode target should emit 4 per alias, got {out:?}"
        );
        assert!(out.contains(&"Frieren 9".to_string()));
        assert!(out.contains(&"Frieren - 09".to_string()));
        assert!(out.contains(&"Frieren 09".to_string()));
        assert!(out.contains(&"\"Frieren\" 09".to_string()));
    }

    #[test]
    fn build_queries_collapsed_mode_emits_one_episode_variant() {
        // With /user/<name> scope active, extra variants all return the
        // same uploader's catalog so we drop from 4→1 to cut wall-time.
        let aliases = vec!["Frieren".to_string()];
        let out = build_queries_from_aliases(&aliases, &SearchTarget::Episode(9), true);
        assert_eq!(out, vec!["Frieren 09".to_string()]);
    }

    #[test]
    fn build_queries_collapsed_mode_emits_one_single_variant() {
        let aliases = vec!["Jujutsu Kaisen 0".to_string()];
        let out = build_queries_from_aliases(&aliases, &SearchTarget::Single, true);
        assert_eq!(out, vec!["Jujutsu Kaisen 0".to_string()]);
    }

    #[test]
    fn build_queries_collapsed_scales_with_alias_count() {
        // One variant per alias — the case-insensitive dedupe on
        // `dedupe_strings` collapses romaji/english when they share a
        // lowercase key ("Jujutsu Kaisen" and "JUJUTSU KAISEN"), so a
        // typical three-field AL detail still produces two distinct
        // collapsed queries.
        let aliases = vec![
            "Jujutsu Kaisen".to_string(),
            "JUJUTSU KAISEN".to_string(),
            "呪術廻戦".to_string(),
        ];
        let out = build_queries_from_aliases(&aliases, &SearchTarget::Episode(56), true);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|q| q.ends_with(" 56")));
    }

    // ─── Tier 2 deferred — small testable helpers ───────────────────
    //
    // The audit flagged 139 missed mutants in this file. Most are in
    // `find_all_for_target`, `collect_scored_for_target`, and
    // `collect_scored_batches_for_target` — all of which call
    // `run_queries`/`run_queries_interactive` which makes live HTTP
    // requests to Nyaa and torznab indexers. Pinning those needs a
    // wiremock-Nyaa harness that doesn't exist yet (significant lift,
    // out of scope for this commit).
    //
    // The pure-function helpers ARE testable end-to-end without
    // network mocking. This block pins them.

    fn detail(id: i64, romaji: &str, english: &str, native: &str) -> AnimeDetail {
        AnimeDetail {
            is_adult: false,
            id,
            id_mal: None,
            title_romaji: romaji.into(),
            title_english: english.into(),
            title_native: native.into(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: "Finished".into(),
            episodes: Some(12),
            duration: Some(24),
            season: String::new(),
            season_year: Some(2024),
            end_year: Some(2024),
            description: String::new(),
            genres: vec![],
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: vec![],
            streaming_episodes: vec![],
            relations: vec![],
        }
    }

    #[test]
    fn display_title_prefers_english_when_present() {
        // Pin the English-first preference (line 1669). The
        // `replace -> "" / "xyzzy"` mutations are caught by asserting
        // the exact returned string, and `delete !` is caught by the
        // English-empty branch in the second test.
        let d = detail(
            1,
            "Sousou no Frieren",
            "Frieren: Beyond Journey's End",
            "葬送のフリーレン",
        );
        assert_eq!(display_title(&d), "Frieren: Beyond Journey's End");
    }

    #[test]
    fn display_title_falls_back_to_romaji_when_english_empty() {
        // Empty title_english → `delete !` mutation at line 1669:8
        // would invert the guard and return romaji even when English
        // is present. Pin the empty-english fallback.
        let d = detail(2, "Sousou no Frieren", "", "葬送のフリーレン");
        assert_eq!(display_title(&d), "Sousou no Frieren");
    }

    #[rstest::rstest]
    #[case::ep_target_relative(5, 5, 0, true)] // direct hit on relative number
    #[case::ep_target_absolute_offset(52, 5, 47, true)] // parsed=52 = target(5)+offset(47)
    #[case::ep_neither_relative_nor_absolute(99, 5, 0, false)] // no match
    #[case::offset_not_active_returns_false_for_absolute(52, 5, 0, false)] // offset=0 → no absolute branch
    fn episode_match_pins_target_and_absolute_branches(
        #[case] parsed_value: i32,
        #[case] target_ep: i32,
        #[case] absolute_offset: i32,
        #[case] expected: bool,
    ) {
        // Pin line 1376's `absolute_offset > 0` guard. Mutating to
        // `>= 0` would activate the absolute-number branch even when
        // offset is zero, which is what the function explicitly avoids
        // (the offset=0 case must collapse to the strict-relative
        // path used for first-season entries).
        let mut parsed = HashSet::new();
        parsed.insert(parsed_value);
        assert_eq!(episode_match(&parsed, target_ep, absolute_offset), expected);
    }

    #[rstest::rstest]
    // Each band of `Resolution::from_str` mapped to the corresponding
    // bare-number string the Nyaa search pipeline expects. Pins both
    // return-substitution mutations at line 1679 (the function-level
    // replacement to `""` or `"xyzzy"`) and the Unknown→1080 default
    // (line 1685).
    #[case::r480("480p", "480")]
    #[case::r576("576p", "576")]
    #[case::r720("720p", "720")]
    #[case::r1080("1080p", "1080")]
    #[case::r2160("2160p", "2160")]
    #[case::unknown_defaults_to_1080("garbage", "1080")]
    #[case::empty_defaults_to_1080("", "1080")]
    fn preferred_resolution_search_value_maps_each_band(
        #[case] preferred: &str,
        #[case] expected: &str,
    ) {
        let cfg = Config {
            preferred_resolution: preferred.into(),
            ..Default::default()
        };
        assert_eq!(preferred_resolution_search_value(&cfg), expected);
    }

    #[test]
    fn build_queries_mixed_returns_canonical_only_when_variants_empty() {
        // Pin line 1627's `!variants.is_empty()` guard. With empty
        // variants, the function MUST return the canonical query set
        // unchanged. Mutating `delete !` would call
        // `build_queries_from_aliases` with empty variants, which is
        // a no-op (returns empty) but the extend would still happen —
        // observationally identical, so this test pins the result
        // shape only.
        let canonical = vec!["Show".to_string()];
        let variants: Vec<String> = vec![];
        let out = build_queries_mixed(&canonical, &variants, &SearchTarget::Episode(1), true);
        // collapsed=true with one alias and Episode(1) → one query.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "Show 01");
    }

    #[test]
    fn build_queries_mixed_includes_variant_queries_when_present() {
        // Variants run collapsed=true regardless of the call-site's
        // `collapsed` flag (so a 4-variant fan-out doesn't multiply
        // the canonical 4-form sweep by 4 again). Pin both the
        // canonical AND variant query presence so the function-level
        // return-substitution mutations (`vec![]`, `vec!["xyzzy"]`,
        // `vec![String::new()]`) at line 1626 all flip MISSED → CAUGHT.
        let canonical = vec!["Show".to_string()];
        let variants = vec!["Show 2".to_string()];
        let out = build_queries_mixed(&canonical, &variants, &SearchTarget::Single, false);
        // Canonical Single non-collapsed: bare + quoted = 2.
        // Variants collapsed=true: bare = 1.
        // Total = 3 (no overlap because the strings differ).
        assert_eq!(out.len(), 3, "got {out:?}");
        assert!(out.iter().any(|q| q == "Show"));
        assert!(out.iter().any(|q| q == "\"Show\""));
        assert!(out.iter().any(|q| q == "Show 2"));
    }

    #[test]
    fn collect_aliases_with_variants_returns_non_empty_combined_canonical_variants() {
        // The mutation surface (12 substitutions of the 3-tuple
        // return) all collapse to either an empty Vec, a single-
        // entry-with-empty-string Vec, or a single-entry-with-"xyzzy"
        // Vec. Asserting the function returns NON-empty content with
        // recognizable substrings of the input AnimeDetail kills all
        // 12 substitutions.
        let d = detail(
            1,
            "Sousou no Frieren",
            "Frieren: Beyond Journey's End",
            "葬送のフリーレン",
        );
        let (combined, canonical, variants) = collect_aliases_with_variants(&d);

        assert!(
            !combined.is_empty(),
            "combined alias list must not be empty"
        );
        assert!(
            !canonical.is_empty(),
            "canonical alias list must not be empty"
        );
        // Combined must include canonical entries.
        for c in &canonical {
            assert!(
                combined.iter().any(|x| x == c),
                "combined missing canonical alias {c:?}"
            );
        }
        // Variants are derived from canonical sequel-numbering, so they
        // may be empty for a one-shot title (no S2 / Part variants
        // detected). Just assert the field exists with its own type.
        let _: Vec<String> = variants;

        // Sanity: at least one alias contains a substring of the
        // input title fields, so the substitution-to-"xyzzy" or
        // empty-string would fail this check.
        assert!(
            canonical
                .iter()
                .any(|a| a.contains("Frieren") || a.contains("Sousou")),
            "canonical must carry a recognizable token from the input"
        );
    }

    #[test]
    fn interactive_filter_stamps_provenance_kind_and_phase() {
        use std::collections::HashSet;
        fn mk(title: &str, hash: &str) -> SearchResult {
            SearchResult {
                match_provenance: None,
                title: title.to_string(),
                link: String::new(),
                magnet: String::new(),
                torrent: String::new(),
                size: String::new(),
                size_bytes: 0,
                seeders: 10,
                leechers: 0,
                downloads: 0,
                group: "G".to_string(),
                resolution: "1080".to_string(),
                quality_label: String::new(),
                source: String::new(),
                web_kind: String::new(),
                is_remux: false,
                is_bdmv: false,
                is_batch: false,
                is_trusted: false,
                score: 0,
                info_hash: hash.to_string(),
                score_breakdown: Vec::new(),
                upload_date: String::new(),
                indexer_id: None,
                indexer_name: String::new(),
            }
        }
        let aliases = vec!["Sousou no Frieren".to_string()];
        let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &[]);
        let preferred_groups: Vec<String> = Vec::new();
        let target = SearchTarget::Single;
        let seadex_hashes: HashSet<String> = ["cafebabe".to_string()].into_iter().collect();
        let categories = vec!["1_2".to_string()];
        let ctx = InteractiveQueryCtx {
            phase: MatchPhase::Extended,
            aliases: &aliases,
            sibling_precompute: &sibling_precompute,
            preferred_groups: &preferred_groups,
            preferred_resolution: "1080p",
            target: &target,
            expected_season: 0,
            seadex_hashes: &seadex_hashes,
            restrict_user: "",
            absolute_offset: 0,
            categories: &categories,
            indexers: &[],
        };
        let mut seen: HashSet<String> = HashSet::new();
        let mut candidates: Vec<SearchResult> = Vec::new();

        apply_interactive_filter_and_push(
            mk("[G] Sousou no Frieren - 01 [1080p]", "aaaa"),
            &ctx,
            &mut seen,
            &mut candidates,
        );
        // "sousou frieren 02": 2 of 2 distinctive tokens but not a
        // substring, so fuzzy at 1.0 under the relaxed policy.
        apply_interactive_filter_and_push(
            mk("[G] Sousou Frieren - 02 [1080p]", "bbbb"),
            &ctx,
            &mut seen,
            &mut candidates,
        );
        apply_interactive_filter_and_push(
            mk("[smol] Something Else Entirely", "cafebabe"),
            &ctx,
            &mut seen,
            &mut candidates,
        );
        apply_interactive_filter_and_push(
            mk("[G] Unrelated Show - 01", "dddd"),
            &ctx,
            &mut seen,
            &mut candidates,
        );

        assert_eq!(candidates.len(), 3, "the unrelated title must be rejected");
        let p0 = candidates[0].match_provenance.as_ref().expect("stamped");
        assert_eq!(
            (p0.kind, p0.phase),
            (MatchKind::Verbatim, MatchPhase::Extended)
        );
        assert_eq!(p0.alias, "Sousou no Frieren");
        let p1 = candidates[1].match_provenance.as_ref().expect("stamped");
        assert_eq!(p1.kind, MatchKind::Fuzzy);
        assert!((p1.ratio - 1.0).abs() < f32::EPSILON);
        let p2 = candidates[2].match_provenance.as_ref().expect("stamped");
        assert_eq!(p2.kind, MatchKind::SeadexCurated);
        assert!(p2.alias.is_empty());
    }
}
