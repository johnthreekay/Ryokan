//! Sonarr-v4-compatible Custom Formats.
//!
//! A Custom Format is a user-authored bundle of specifications that
//! either matches a release (CF score adds to the candidate's total)
//! or doesn't. Ryokan's CF model is a strict subset of Sonarr v4's —
//! same JSON shape, same match semantics — with a single Ryokan-only
//! addition (`Ryokan.SeaDexBestSpecification`) surfaced via the
//! `Ryokan.` namespace so Sonarr-safe exports can detect and skip it.
//!
//! This module owns the parser, the per-candidate evaluator, and the
//! DB-backed startup loader. It does **not** own cache invalidation or
//! the `AppState` plumbing — Phase 5 wires the compiled-CF cache onto
//! `AppState` and adds `rebuild_cf_cache`, and Phase 6 plugs
//! `total_cf_score` into the three `auto_search.rs` call sites.
//!
//! The critical piece of correctness here is §5.7 of the plan: the
//! match rule is NOT "all specs true" — it's group-by-type DidMatch
//! with a subtle required-hard-fail rule. See [`evaluate`] and the
//! worked examples in its unit tests for the exact semantics.

// A handful of spec fields (negate, required, source's raw Sonarr int)
// are parsed but not read by the current evaluator path — they're
// preserved so round-trip export matches Sonarr byte-for-byte and the
// semantics stay visible in the debugger. Scope the allow narrowly to
// this file rather than annotating each struct field.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::RwLock;

use super::nyaa::SearchResult;
use super::source::{ClassificationResult, Resolution, Source, WebKind};

mod evaluator;
mod parser;

pub use evaluator::{evaluate, total_cf_score_for_release, total_cf_score_with_breakdown};
pub use parser::compile_from_json;

// ───────────────────────────────────────────────────────────────────────────
// Types
// ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CompiledCustomFormat {
    pub id: i64,
    pub name: String,
    pub score: i32,
    pub specs: Vec<CompiledSpec>,
}

/// Every spec variant carries both `negate` and `required`, matching
/// Sonarr's `ICustomFormatSpecification` interface. `required` is
/// consumed by the group-by-type DidMatch rule in [`evaluate`] (§5.7),
/// not inside the per-spec kernel.
#[derive(Debug, Clone)]
pub struct CompiledSpec {
    pub kind: SpecKind,
    pub negate: bool,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub enum SpecKind {
    ReleaseTitle {
        regex: fancy_regex::Regex,
    },
    ReleaseGroup {
        regex: fancy_regex::Regex,
    },
    Size {
        min_bytes: i64,
        max_bytes: i64,
    },
    Resolution {
        value: Resolution,
    },
    /// Stores Sonarr's raw `QualitySource` integer (see plan §4.5).
    /// Dispatch happens inside `evaluate_spec_kernel` — one branch per
    /// supported Sonarr int, with `2` (TelevisionRaw) rejected at parse
    /// time.
    Source {
        sonarr_value: u8,
    },
    /// Ryokan-only: matches when the candidate's info_hash is in the
    /// SeaDex "best" hash set for the current anilist_id. Namespaced
    /// as `Ryokan.SeaDexBestSpecification` in exported JSON.
    SeaDexBest,
}

impl SpecKind {
    /// Group-by-type discriminator used by [`evaluate`]. Two specs
    /// belong to the same group iff this returns the same value.
    /// Mirrors Sonarr's `.GroupBy(t => t.GetType())`.
    pub fn type_tag(&self) -> u8 {
        match self {
            SpecKind::ReleaseTitle { .. } => 1,
            SpecKind::ReleaseGroup { .. } => 2,
            SpecKind::Size { .. } => 3,
            SpecKind::Resolution { .. } => 4,
            SpecKind::Source { .. } => 5,
            SpecKind::SeaDexBest => 6,
        }
    }
}

/// Evaluation context threaded through [`evaluate`] / [`total_cf_score`].
///
/// Holds borrowed references to the candidate, its classification, and
/// the (possibly empty) set of SeaDex best hashes for the current
/// anilist_id. Lifetimes keep everything non-allocating on the per-
/// candidate path.
pub struct EvalContext<'a> {
    pub result: &'a SearchResult,
    pub classification: &'a ClassificationResult,
    /// Lowercased info hashes that SeaDex has flagged as `isBest` for
    /// the current anilist_id. Empty when SeaDex is disabled, the entry
    /// is missing, or `pick_best` rejected every candidate.
    pub seadex_hashes: &'a HashSet<String>,
}

/// Shared cache container — an `RwLock` around an `Arc<Vec<...>>` so
/// evaluation code can cheap-clone the inner `Arc` and release the read
/// lock before iterating over candidates. Phase 5 adds a field of this
/// type to `AppState`; this alias is declared here so both sides agree
/// on the shape.
pub type CompiledCfCache = Arc<RwLock<Arc<Vec<CompiledCustomFormat>>>>;

// ───────────────────────────────────────────────────────────────────────────
// DB-backed startup loader
// ───────────────────────────────────────────────────────────────────────────

/// Load every CF row, join its V1-profile score, and compile each one.
/// A CF that fails to parse is logged at WARN and skipped; the rest of
/// the set still loads. Phase 5 wraps the returned Vec in an `Arc` and
/// stashes it on `AppState` at startup.
pub async fn load_compiled_cfs(db: &SqlitePool) -> Vec<CompiledCustomFormat> {
    let rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
        r#"
        SELECT cf.id, cf.name, cf.json, COALESCE(cfs.score, 0) AS score
        FROM custom_formats cf
        LEFT JOIN custom_format_scores cfs
               ON cfs.custom_format_id = cf.id
              AND cfs.profile_id = 1
        ORDER BY cf.id
        "#,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .filter_map(|(id, name, raw_json, score)| {
            match compile_from_json(&raw_json, score as i32, id) {
                Ok(cf) => Some(cf),
                Err(e) => {
                    tracing::warn!("custom_formats: skipping {name} (id={id}): {e}");
                    None
                }
            }
        })
        .collect()
}

/// Re-run `load_compiled_cfs` and atomically swap the compiled set into
/// the shared `CompiledCfCache`. Callers take the write lock only long
/// enough to replace the inner `Arc`; readers on the scoring hot path
/// clone the `Arc` out under the read lock and then release it — so a
/// reader arriving mid-swap blocks for at most the duration of the Arc
/// replacement (microseconds) before returning a consistent snapshot.
/// Used by the Custom Formats settings page after any create / update /
/// delete / import.
pub async fn rebuild_cf_cache(cache: &CompiledCfCache, db: &SqlitePool) {
    let fresh = Arc::new(load_compiled_cfs(db).await);
    *cache.write().await = fresh;
}

/// `true` if any compiled CF contains a `SeaDexBest` spec. Used to
/// suppress the hardcoded SeaDex score boost when the user has opted
/// into controlling that boost themselves through a Custom Format —
/// otherwise a candidate on SeaDex would earn both the CF score and
/// the hardcoded `SEADEX_SCORE_BOOST` bump, which is double counting.
pub fn has_seadex_cf(cfs: &[CompiledCustomFormat]) -> bool {
    cfs.iter().any(|cf| {
        cf.specs
            .iter()
            .any(|s| matches!(s.kind, SpecKind::SeaDexBest))
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Shared test helpers
// ───────────────────────────────────────────────────────────────────────────
//
// Used by the `tests` modules in both `parser.rs` and `evaluator.rs`.
// Kept here so the two test sides share one construction style for
// `SearchResult` / `ClassificationResult` / `EvalContext` fixtures.

#[cfg(test)]
pub(super) mod test_helpers {
    use super::*;
    use crate::services::source::DecisionRule;

    pub fn candidate(title: &str, group: &str, size_bytes: i64, info_hash: &str) -> SearchResult {
        SearchResult {
            match_provenance: None,
            title: title.to_string(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes,
            seeders: 0,
            leechers: 0,
            downloads: 0,
            group: group.to_string(),
            resolution: String::new(),
            quality_label: String::new(),
            source: String::new(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: false,
            is_trusted: false,
            score: 0,
            info_hash: info_hash.to_string(),
            score_breakdown: Vec::new(),
            upload_date: String::new(),
            indexer_id: None,
            indexer_name: String::new(),
        }
    }

    pub fn classification(source: Source, resolution: Resolution) -> ClassificationResult {
        ClassificationResult {
            source,
            resolution,
            is_remux: false,
            web_kind: WebKind::Unknown,
            is_bdmv: false,
            confidence: 1.0,
            needs_review: false,
            evidence: vec![],
            decision_rule: DecisionRule::Empty,
        }
    }

    pub fn ctx<'a>(
        result: &'a SearchResult,
        classification: &'a ClassificationResult,
        seadex: &'a HashSet<String>,
    ) -> EvalContext<'a> {
        EvalContext {
            result,
            classification,
            seadex_hashes: seadex,
        }
    }

    pub fn compile(raw: &str) -> CompiledCustomFormat {
        compile_from_json(raw, 100, 1).expect("fixture CF should compile")
    }
}
