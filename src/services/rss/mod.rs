use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

use regex_lite::Regex;

use crate::{
    AppState,
    models::log::LogCategory,
    models::{config, episode_tags, monitoring, rss, series},
    services::source::{self, ClassificationResult, Resolution, Source},
    services::{auto_search, logger, media, monitoring as monitoring_service, quality},
};

pub mod feed;
use feed::{build_item_key, detect_batch, extract_group, extract_resolution, fetch_feeds};

/// multi-rss commit F — flatten an `RssSource` to the
/// `(source_str, source_id_opt)` pair `rss_seen.source` +
/// `rss_seen.source_id` actually store. Used at every record_decision
/// callsite + the dedup-key check below.
fn source_dedup_key(s: &RssSource) -> (&'static str, Option<i64>) {
    match s {
        RssSource::Nyaa => ("nyaa", None),
        RssSource::Indexer { id, .. } => ("indexer", Some(*id)),
        RssSource::UserFeed { id, .. } => ("direct", Some(*id)),
    }
}

/// multi-rss commit F — resolve the per-item dispatch client based
/// on the item's source. Each source has its own pin column:
///   * Nyaa → `config.nyaa_download_client_id`
///   * Indexer → `indexers.download_client_id`
///   * UserFeed → `direct_rss_feeds.download_client_id`
///
/// Returns `None` when no client is configured at all (pool empty).
/// The caller treats this as a per-item reject; other items in the
/// same sync tick may still resolve to a client (e.g. if a single
/// indexer's pin points at a deleted client, that indexer's items
/// reject but everything else still flows).
///
/// `direct_feed_pins` is pre-loaded by the caller from the same
/// `list_enabled` query the fan-out used; passing it down avoids
/// an N×SELECT-per-item lookup inside the grab loop. Indexer + Nyaa
/// pins resolve through the in-memory caches already (`state
/// .indexers` / `state.download_clients`); only direct feeds
/// needed this dimension. PR 112 review #5.
async fn resolve_dispatch_for_item(
    state: &AppState,
    cfg: &config::Config,
    item: &RssItem,
    direct_feed_pins: &std::collections::HashMap<i64, Option<i64>>,
) -> Option<(
    std::sync::Arc<dyn crate::services::download_client::DownloadClient>,
    i64,
)> {
    match &item.source {
        RssSource::Nyaa => {
            state
                .client_for_nyaa_with_id(cfg.nyaa_download_client_id)
                .await
        }
        RssSource::Indexer { id, .. } => state.client_for_indexer_with_id(Some(*id)).await,
        RssSource::UserFeed { id, .. } => {
            // Direct-feed pin lookup via the pre-loaded HashMap —
            // no DB round-trip per item.
            let pin = direct_feed_pins.get(id).copied().unwrap_or(None);
            state.client_for_nyaa_with_id(pin).await
        }
    }
}

static RSS_SYNC_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

// ── Pre-compiled regexes ───────────────────────────────────────────────────
// Core-title normalisation
static RE_CORE_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:season\s*\d+|\d+(?:st|nd|rd|th)\s+season|s\d{1,2}(?:e\d{1,4})?|part\s*\d+|cour\s*\d+|final|end(?:ing)?s?)\b").unwrap()
});

// Season number extraction (tried in order)
static RE_SEASON_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)\bseason\s*(\d{1,2})\b").unwrap(),
        Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)\s+season\b").unwrap(),
        Regex::new(r"(?i)\bs(\d{1,2})\b").unwrap(),
        Regex::new(r"(?i)\bpart\s*(\d{1,2})\b").unwrap(),
        Regex::new(r"(?i)\bcour\s*(\d{1,2})\b").unwrap(),
    ]
});

// Season+episode range patterns
static RE_SEASON_EP_RANGE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
    Regex::new(r"(?i)\bs(\d{1,2})\s*e(\d{1,4})\s*[-~]\s*e?(\d{1,4})(?:v\d+)?\b").unwrap(),
    Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)\s+season\b\s*[-:]\s*(\d{1,4})\s*[-~]\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
    Regex::new(r"(?i)\bseason\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})\s*[-~]\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
    Regex::new(r"(?i)\bpart\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})\s*[-~]\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
    Regex::new(r"(?i)\bcour\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})\s*[-~]\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
]
});

// Season+episode single patterns
static RE_SEASON_EP_SINGLE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)\bs(\d{1,2})\s*e(\d{1,4})(?:v\d+)?\b").unwrap(),
        Regex::new(r"(?i)\bs(\d{1,2})[ ._-]*ep?(\d{1,4})(?:v\d+)?\b").unwrap(),
    ]
});

// Season+dash+episode patterns
static RE_SEASON_DASH: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
    Regex::new(r"(?i)\bs(\d{1,2})\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
    Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)\s+season\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
    Regex::new(r"(?i)\bseason\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
    Regex::new(r"(?i)\bpart\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
    Regex::new(r"(?i)\bcour\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
]
});

// Range pattern (no season prefix)
static RE_RANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{1,3})\s*[-~]\s*(\d{1,3})(?:v\d+)?\b").unwrap());

// Season-marker patterns that `parse_release` strips before running
// RE_RANGE and RE_ABSOLUTE against the title. Otherwise the digit in
// "Season 3" / "Part 1" / "S3" / "3rd Season" / "Cour 2" gets a
// second life as an absolute episode number when followed by `(` or
// `[` — e.g. "Season 3 (WEB 1080p ...)" yields episode 3 from the
// lone "3 (" substring even though that 3 is the season. Sonarr's
// parser avoids this by requiring specific anchor tokens (`- N (`,
// `E\d+`, etc.) for absolute-episode extraction; we achieve the same
// effect by masking the season tokens out of the search window.
//
// Masking is safe at this point because the season+episode combined
// patterns (RE_SEASON_EP_RANGE / _SINGLE / _DASH) have already run
// and either captured or returned early. If we reach the absolute-
// episode loop, no season+episode combined pattern matched, so the
// season digit has no episode counterpart to anchor to.
/// Pattern fragments for every season-marker phrasing RSS recognizes.
/// Shared between `RE_SEASON_MARKER_MASK` (this module) and
/// `RE_BATCH_SEASON_BRACKET` (in `feed.rs`) so a new phrasing only
/// needs to be added here — adding "Chapter N" or "Volume N" later
/// updates both the masking pass and the batch-detect anchor in one
/// place.
pub(super) const SEASON_TOKEN_FRAGMENTS: &[&str] = &[
    r"season\s*\d{1,2}",
    r"\d{1,2}(?:st|nd|rd|th)\s+season",
    r"part\s*\d{1,2}",
    r"cour\s*\d{1,2}",
    r"s\d{1,2}",
];

pub(super) static RE_SEASON_MARKER_MASK: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    SEASON_TOKEN_FRAGMENTS
        .iter()
        .map(|frag| Regex::new(&format!(r"(?i)\b{}\b", frag)).unwrap())
        .collect()
});

// Absolute episode patterns (tried in order)
static RE_ABSOLUTE: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "absolute_dash",
            Regex::new(r"(?i)(?:^|\s)-\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
        ),
        (
            "absolute",
            Regex::new(r"(?i)\bepisode\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
        ),
        (
            "absolute",
            Regex::new(r"(?i)\be(?:p\.?|pisode)?\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
        ),
        (
            "absolute",
            Regex::new(r"(?i)\b(\d{1,4})(?:v\d+)?\s*(?:\(|\[)").unwrap(),
        ),
        (
            "absolute_dash",
            Regex::new(r"(?i)\b-\s*(\d{1,4})(?:v\d+)?(?:\s+final|\s+end)?(?:\.[a-z0-9]{2,4}|$)")
                .unwrap(),
        ),
        (
            "absolute",
            Regex::new(r"(?i)\b(\d{1,4})(?:v\d+)?\s*(?:final|end)\b").unwrap(),
        ),
    ]
});

/// Multi-RSS — provenance attribution for each item the sync
/// loop sees. Carried alongside the item through dedup, scoring, and
/// grab so logs / grab-row routing can answer "which feed produced
/// this release". Also drives per-source download-client pin
/// resolution: a Nyaa-direct item routes through `config
/// .nyaa_download_client_id`; a `UserFeed` item through the feed's
/// own `download_client_id`; an `Indexer` item through the
/// indexer's `download_client_id`. Cross-feed dedup keeps
/// the highest-priority source when the same release shows up in
/// multiple places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RssSource {
    /// The historical Nyaa-direct path. Stays out-of-band per
    /// the codebase's "Nyaa is the protected hot path" rule —
    /// never gets a row in `rss_feeds` or `indexers`.
    Nyaa,
    /// User-configured RSS URL from the `rss_feeds` table
    /// (multi-rss Option A). Carries the row id so the grab
    /// path can re-read the row's `download_client_id` if
    /// needed without holding a borrow across the sync.
    UserFeed { id: i64, name: String },
    /// torznab/newznab indexer with `rss_enabled = 1` (multi-rss
    /// Option B). `kind` distinguishes torrent-vs-NZB at grab
    /// time so the protocol guard can route to the right
    /// download client; `id` is the `indexers` row id.
    Indexer { id: i64, name: String, kind: String },
}

impl RssSource {
    /// Short slug for log lines / `rss_seen.source` column. Stable
    /// strings so log-grep / DB queries don't break across releases.
    pub fn label(&self) -> String {
        match self {
            RssSource::Nyaa => "nyaa".to_string(),
            RssSource::UserFeed { name, .. } => format!("feed:{name}"),
            RssSource::Indexer { kind, name, .. } => format!("{kind}:{name}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RssItem {
    pub title: String,
    pub link: String,
    pub guid: String,
    pub torrent: String,
    pub magnet: String,
    pub info_hash: String,
    pub group: String,
    pub resolution: String,
    pub is_batch: bool,
    /// Multi-RSS — which feed produced this item. The legacy
    /// Nyaa-only sync writes `RssSource::Nyaa` everywhere; the
    /// multi-source fan-out populates this distinctly per
    /// feed.
    pub source: RssSource,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncSummary {
    pub items_seen: i32,
    pub matched: i32,
    pub grabbed: i32,
    pub skipped: i32,
    pub detail: String,
}

#[derive(Clone)]
struct ParsedRelease {
    normalized_title: String,
    core_title: String,
    collapsed_title: String,
    collapsed_core_title: String,
    season_hint: Option<i32>,
    season_relative_eps: HashSet<i32>,
    absolute_eps: HashSet<i32>,
    parse_mode: &'static str,
}

#[derive(Clone)]
struct MatchResult {
    series: series::Series,
    parsed: ParsedRelease,
    resolved_eps: HashSet<i32>,
    canonical_abs_eps: HashSet<i32>,
    family_key: String,
    alias_score: f32,
    resolution_mode: &'static str,
}

struct CandidateDecision {
    reject_reason: Option<String>,
    new_episode_count: i32,
    is_upgrade: bool,
}

#[derive(Clone)]
struct SeriesMeta {
    series: series::Series,
    aliases: Vec<String>,
    core_aliases: Vec<String>,
    collapsed_aliases: Vec<String>,
    collapsed_core_aliases: Vec<String>,
    season_num: Option<i32>,
}

#[derive(Clone)]
struct PendingCandidate {
    item: RssItem,
    item_key: String,
    found: MatchResult,
    score: i32,
    new_episode_count: i32,
    is_upgrade: bool,
    /// Full pre-disk classification of this item — filename layer,
    /// group-map, temporal, and description-body-when-ambiguous
    /// combined. Computed once in the upgrade gate and reused by the
    /// grab path so the same item isn't classified twice per cycle.
    classification: ClassificationResult,
}

impl SeriesMeta {
    fn from_series(series: &series::Series) -> Self {
        let mut alias_input = vec![
            series.title.clone(),
            series.title_romaji.clone(),
            series.title_english.clone(),
            series.title_native.clone(),
        ];
        alias_input.extend(series.alternate_title_list());
        let aliases = auto_search::dedupe_strings(alias_input);

        let season_num = aliases
            .iter()
            .find_map(|alias| parse_season_number(&auto_search::normalize_title(alias)));

        let mut expanded = aliases.clone();
        let mut core_aliases = Vec::new();
        let mut collapsed_aliases = Vec::new();
        let mut collapsed_core_aliases = Vec::new();

        for alias in &aliases {
            let normalized = auto_search::normalize_title(alias);
            if !normalized.is_empty() {
                expanded.push(normalized.clone());
                collapsed_aliases.push(collapse_alias(&normalized));
            }
            let core = normalize_core_title(&normalized);
            if !core.is_empty() {
                core_aliases.push(core.clone());
                expanded.push(core.clone());
                collapsed_core_aliases.push(collapse_alias(&core));
                if let Some(season) = season_num {
                    expanded.push(format!("{} s{}", core, season));
                    expanded.push(format!("{} season {}", core, season));
                    expanded.push(format!("{} {} season", core, ordinal_suffix(season)));
                }
            }
        }

        Self {
            series: series.clone(),
            aliases: auto_search::dedupe_strings(expanded),
            core_aliases: auto_search::dedupe_strings(core_aliases),
            collapsed_aliases: auto_search::dedupe_strings(collapsed_aliases),
            collapsed_core_aliases: auto_search::dedupe_strings(collapsed_core_aliases),
            season_num,
        }
    }
}

pub async fn sync_once(state: &AppState, trigger: &str) -> Result<SyncSummary, String> {
    let _guard = RSS_SYNC_LOCK
        .try_lock()
        .map_err(|_| "RSS sync is already running".to_string())?;

    // PR 112 review #3 — hoist the master-flag check up to here
    // so an off install doesn't write a `rss_runs` row + two log
    // rows every 60s tick. The cheap config read is a single
    // SELECT; we eat that to honor the kill switch's intent
    // ("don't poll, don't make noise"). Returning the same
    // SyncSummary shape the inner function would for the
    // master-off branch keeps callers' summary parsing stable.
    let cfg_master_off = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| !c.rss_master_enabled)
        .unwrap_or(false);
    if cfg_master_off {
        return Ok(SyncSummary {
            items_seen: 0,
            matched: 0,
            grabbed: 0,
            skipped: 0,
            detail: "RSS master switch is off; no sources polled".to_string(),
        });
    }

    let run_id = rss::start_run(&state.db, trigger)
        .await
        .map_err(|e| e.to_string())?;
    let result = match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        sync_once_inner(state, trigger),
    )
    .await
    {
        Ok(inner) => inner,
        Err(_) => Err("RSS sync timed out after 5 minutes".to_string()),
    };

    match &result {
        Ok(summary) => {
            let _ = rss::finish_run(
                &state.db,
                run_id,
                rss::RunSummary {
                    status: "ok",
                    items_seen: summary.items_seen,
                    matched: summary.matched,
                    grabbed: summary.grabbed,
                    skipped: summary.skipped,
                    detail: &summary.detail,
                },
            )
            .await;
        }
        Err(err) => {
            let _ = rss::finish_run(
                &state.db,
                run_id,
                rss::RunSummary {
                    status: "error",
                    items_seen: 0,
                    matched: 0,
                    grabbed: 0,
                    skipped: 0,
                    detail: err,
                },
            )
            .await;
        }
    }

    result
}

/// multi-rss commit F — fan out across every enabled source and
/// return the merged `Vec<RssItem>`. Each item carries its own
/// `RssSource` attribution (set by the per-source fetch helper)
/// so the rest of `sync_once_inner` is feed-source-blind on the
/// hot path.
///
/// Per-source error isolation: a failing source logs + writes its
/// `last_poll_error` row but doesn't poison the rest of the
/// fan-out. The success path writes `last_polled_at` +
/// `last_item_count` so the Settings UI's status chips reflect
/// the most recent run.
///
/// Sources fetched (in the order their items land in the merged
/// vec — relevant for the cross-source dedup tiebreak: first
/// occurrence wins):
///   1. Nyaa (when `cfg.rss_enabled` is on),
///   2. each `indexers` row with `enabled=1 AND rss_enabled=1`,
///   3. each `direct_rss_feeds` row with `enabled=1`.
///
/// The order matches the plan's "Nyaa is the protected hot path"
/// invariant — Nyaa's items go in first so a release surfaced on
/// both Nyaa and an indexer attributes to Nyaa for grab routing,
/// matching the v1 behavior.
async fn fetch_all_sources(
    state: &AppState,
    cfg: &config::Config,
    has_music_series: bool,
) -> Vec<RssItem> {
    let mut items: Vec<RssItem> = Vec::new();

    // 1. Nyaa — gate on `cfg.rss_enabled` (legacy v1 flag, plan
    //    decision #8 keeps its semantics) AND `!cfg.disable_nyaa_rss`
    //    (Nyaa-specific opt-out for users who only
    //    want indexer-RSS / direct-RSS feeds polled). Master flag
    //    has already been honored at the sync_once_inner top.
    if cfg.rss_enabled && !cfg.disable_nyaa_rss {
        match fetch_feeds(cfg.allow_non_english, has_music_series).await {
            Ok(nyaa_items) => items.extend(nyaa_items),
            Err(err) => {
                logger::warn(
                    &state.db,
                    LogCategory::Rss,
                    "Nyaa RSS fetch failed; skipping",
                    &err,
                )
                .await;
            }
        }
    }

    // 2. Indexer-RSS — every torznab/newznab indexer with
    //    `rss_enabled = 1`. Each fetch runs `Indexer::search()`
    //    with empty `q` against `?t=tvsearch&cat=5070`, which
    //    short-circuits on the existing rate-limit cooldown
    //    state machine in `services/indexers/torznab/client.rs`.
    let indexer_rows = crate::models::indexers::list_rss_enabled(&state.db)
        .await
        .unwrap_or_default();
    let indexer_snapshot = state.indexers.read().await.clone();
    for row in &indexer_rows {
        // Look up the live `Arc<dyn Indexer>` from the in-memory
        // cache so we don't re-instantiate the reqwest client per
        // tick. The cache is rebuilt on every indexer upsert (see
        // commit G's cache-invalidation hook), so a freshly-flipped
        // `rss_enabled` row is reachable on the next tick.
        let live = indexer_snapshot.iter().find(|i| i.id() == row.id).cloned();
        let Some(indexer) = live else {
            // DB has the row but cache is stale — log + skip.
            logger::debug(
                &state.db,
                LogCategory::Rss,
                "Indexer-RSS poll: live indexer cache missing row; will pick up after next rebuild",
                &format!("indexer_id={}", row.id),
            )
            .await;
            continue;
        };
        match crate::services::indexers::fetch_indexer_rss(&*indexer).await {
            Ok(fetched) => {
                let count = fetched.len() as i32;
                items.extend(fetched);
                let _ =
                    crate::models::indexers::record_rss_poll_metrics(&state.db, row.id, count, "")
                        .await;
            }
            Err(err) => {
                let _ =
                    crate::models::indexers::record_rss_poll_metrics(&state.db, row.id, 0, &err)
                        .await;
                logger::warn(
                    &state.db,
                    LogCategory::Rss,
                    &format!("Indexer-RSS poll failed: {}", row.name),
                    &err,
                )
                .await;
                // Opportunistic IndexerDown notification with per-id
                // 1h dedup. Suppress the cooldown shape since that's
                // the upstream's own rate-limit signaling and not an
                // "indexer is down" condition the user needs paged
                // about — once the cooldown lifts, the next tick
                // either succeeds (no event) or returns a real error
                // (real event). The string-prefix match here mirrors
                // the project-wide tag-prefix error convention; the
                // exact prefix is set in
                // `services/indexers/torznab/client.rs`'s 429 path.
                if !err.starts_with("Indexer rate-limited") {
                    crate::services::notifications::emit_indexer_down(state, row.id, &err).await;
                }
            }
        }
    }

    // 3. Direct feeds — every `direct_rss_feeds` row with
    //    `enabled = 1`. `feed::fetch_user_feed` already stamps
    //    `RssSource::UserFeed { id, name }` per the source we
    //    pass in. Each feed gets its own poll-metrics row write
    //    so the Settings UI's chip is accurate per-feed.
    let direct_rows = crate::models::direct_rss_feeds::list_enabled(&state.db)
        .await
        .unwrap_or_default();
    for row in &direct_rows {
        let source = RssSource::UserFeed {
            id: row.id,
            name: row.name.clone(),
        };
        match feed::fetch_user_feed(&row.url, source).await {
            Ok(fetched) => {
                let count = fetched.len() as i32;
                items.extend(fetched);
                let _ = crate::models::direct_rss_feeds::record_poll_metrics(
                    &state.db, row.id, count, "",
                )
                .await;
            }
            Err(err) => {
                let _ = crate::models::direct_rss_feeds::record_poll_metrics(
                    &state.db, row.id, 0, &err,
                )
                .await;
                logger::warn(
                    &state.db,
                    LogCategory::Rss,
                    &format!("Direct-RSS poll failed: {}", row.name),
                    &err,
                )
                .await;
            }
        }
    }

    items
}

async fn sync_once_inner(state: &AppState, trigger: &str) -> Result<SyncSummary, String> {
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    // multi-rss commit F — master kill switch. Off short-circuits
    // before any fetch fires; the supervised-task loop stays alive
    // so flipping the master back on takes effect within one tick
    // (no restart needed). Per the plan's truth table, the master
    // overrides every per-source flag.
    //
    // PR 112 review #3 — the outer `sync_once` already
    // short-circuits on the master flag before writing a
    // `rss_runs` row, so this branch is defense-in-depth (covers
    // a future caller that bypasses `sync_once`). The cheap
    // re-read is acceptable for the redundancy.
    if !cfg.rss_master_enabled {
        return Ok(SyncSummary {
            items_seen: 0,
            matched: 0,
            grabbed: 0,
            skipped: 0,
            detail: "RSS master switch is off; no sources polled".to_string(),
        });
    }

    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let has_music_series = tracked.iter().any(|s| s.format == "MUSIC");
    let items = fetch_all_sources(state, &cfg, has_music_series).await;
    for row in &tracked {
        let _ = monitoring_service::ensure_series_monitoring_rows(&state.db, row).await;
    }
    // multi-rss commit F — `nyaa_client` is used only for the
    // Nyaa-specific `load_canonical_history` lookup (description-
    // body fetches against nyaa.si). The per-item grab dispatch
    // resolves a per-source client via `resolve_dispatch_for_item`
    // inside the grab loop below — Nyaa items route through the
    // Nyaa pin, indexer-RSS items through the indexer's pin,
    // direct-feed items through the feed's own pin.
    let (nyaa_client, _nyaa_client_id) = match state
        .client_for_nyaa_with_id(cfg.nyaa_download_client_id)
        .await
    {
        Some((c, id)) => (Some(c), Some(id)),
        None => (None, None),
    };

    // One compiled-CF snapshot for the whole RSS pass so each item's
    // score reflects the user's CF profile. Without this thread-through
    // the auto-search path applied CFs but the RSS path silently
    // bypassed them — a 10-bit/x265/FLAC release the user explicitly
    // boosted via CF would tie or lose to a plain release on the
    // every-60s auto-grab path while ranking correctly in the manual
    // search UI. SeaDex hashes are passed empty for now: per-series
    // SeaDex lookups would add N round-trips per cycle and the
    // hardcoded `seadex_enabled` toggle bonus only matters when SeaDex
    // is consulted, which only the auto/upgrade paths do today.
    let cfs = state.custom_formats.read().await.clone();
    let empty_seadex_hashes: HashSet<String> = HashSet::new();

    // PR 112 review #5 — pre-load every direct feed's
    // `download_client_id` pin into a HashMap so the per-item
    // grab dispatcher resolves in-memory rather than firing one
    // SELECT per direct-fed item. The fan-out's `list_enabled`
    // call already pulled these rows from the DB; we re-query
    // here so the snapshot is fresh at grab time (a feed pin
    // edited mid-tick still gets honored). Single SELECT vs
    // N×items.
    let direct_feed_pins: HashMap<i64, Option<i64>> =
        crate::models::direct_rss_feeds::list_all(&state.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row.id, row.download_client_id))
            .collect();

    let whitelist = quality::parse_group_list(&cfg.preferred_groups);
    let blacklist = quality::parse_group_list(&cfg.blocked_groups);
    let all_meta: Vec<SeriesMeta> = tracked.iter().map(SeriesMeta::from_series).collect();
    let mut canonical_history =
        load_canonical_history(&state.db, nyaa_client.as_deref(), &all_meta).await;

    // Cache on-disk episode scans per folder to avoid repeated filesystem walks.
    let mut disk_cache: HashMap<String, Vec<media::EpisodeFile>> = HashMap::new();
    let mut monitored_cache: HashMap<i64, HashSet<i32>> = HashMap::new();
    let mut quality_tags_cache: HashMap<
        i64,
        HashMap<i32, crate::models::episode_tags::EpisodeQualityTag>,
    > = HashMap::new();

    let (cutoff_src, cutoff_is_remux, cutoff_is_bdmv) =
        source::parse_cutoff_source(&cfg.cutoff_source);
    let cutoff = source::cutoff_classification(
        cutoff_src,
        Resolution::from_str(&cfg.cutoff_resolution),
        cutoff_is_remux,
        cutoff_is_bdmv,
    );

    let mut items_seen = 0;
    let mut matched = 0;
    let mut grabbed = 0;
    let mut skipped = 0;
    let mut pending: Vec<PendingCandidate> = Vec::new();

    logger::info(
        &state.db,
        LogCategory::Rss,
        "RSS sync started",
        &format!("trigger={} items={}", trigger, items.len()),
    )
    .await;

    // One SELECT instead of N: pre-load every previously-grabbed
    // (item_key, source, source_id) triple so the per-item dedup
    // check is an in-memory HashSet lookup rather than a round-
    // trip per feed item. Multi-rss commit F: scoped variant pins
    // per-source so a SubsPlease item with `guid=12345` doesn't
    // collide with an unrelated Nyaa item that happens to share
    // the GUID.
    let already_grabbed = rss::grabbed_item_keys_scoped(&state.db)
        .await
        .map_err(|e| e.to_string())?;

    for item in items {
        items_seen += 1;
        let item_key = build_item_key(&item);
        let (src_str, src_id) = source_dedup_key(&item.source);
        if already_grabbed.contains(&(item_key.clone(), src_str.to_string(), src_id)) {
            skipped += 1;
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &item_key,
                    title: &item.title,
                    link: &item.link,
                    series_id: None,
                    series_title: "",
                    group_name: &item.group,
                    is_batch: item.is_batch,
                    decision: "skipped",
                    reason: "Already grabbed earlier; skipping duplicate RSS item",
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        }

        let Some(found) = best_series_match(&item, &all_meta) else {
            skipped += 1;
            let diag = build_match_diag(&item, None, 0);
            let reason = format!("No tracked series match | {}", diag);
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &item_key,
                    title: &item.title,
                    link: &item.link,
                    series_id: None,
                    series_title: "",
                    group_name: &item.group,
                    is_batch: item.is_batch,
                    decision: "skipped",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        };

        // Automatic paths take a release only when its title names the
        // series (a title or alias contained verbatim, or the same words);
        // a token-overlap match is left for interactive search.
        //
        // The check reads the winner `best_series_match` picked by its
        // season- and episode-adjusted score, on purpose. When that
        // winner is inexact and an exact match sits lower, the exact one
        // is the series the release fits worse (a season-less entry for
        // a `S2` release, a wrong episode range); grabbing it there would
        // be the wrong-season misgrab the adjustments exist to prevent.
        // Dropping the item is the safe answer, and the sequel-variant
        // aliases (`SeriesMeta::from_series`) keep the right season exact
        // in the common case.
        if found.alias_score < 1.0 {
            skipped += 1;
            let reason = format!(
                "Title does not name the series exactly | {}",
                build_match_diag(&item, Some(&found), 0)
            );
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &item_key,
                    title: &item.title,
                    link: &item.link,
                    series_id: Some(found.series.id),
                    series_title: &found.series.title,
                    group_name: &item.group,
                    is_batch: item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        }

        matched += 1;

        if group_matches_blacklist(&item.group, &blacklist) {
            skipped += 1;
            let reason = format!(
                "Blocked group: {} | {}",
                item.group,
                build_match_diag(&item, Some(&found), 0)
            );
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &item_key,
                    title: &item.title,
                    link: &item.link,
                    series_id: Some(found.series.id),
                    series_title: &found.series.title,
                    group_name: &item.group,
                    is_batch: item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        }

        if !whitelist.is_empty() && !group_matches_whitelist(&item.group, &whitelist) {
            skipped += 1;
            let reason = if item.group.trim().is_empty() {
                format!(
                    "Release group missing and whitelist is enabled | {}",
                    build_match_diag(&item, Some(&found), 0)
                )
            } else {
                format!(
                    "Group not in whitelist: {} | {}",
                    item.group,
                    build_match_diag(&item, Some(&found), 0)
                )
            };
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &item_key,
                    title: &item.title,
                    link: &item.link,
                    series_id: Some(found.series.id),
                    series_title: &found.series.title,
                    group_name: &item.group,
                    is_batch: item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        }

        let monitored_eps = if let Some(cached) = monitored_cache.get(&found.series.id) {
            cached.clone()
        } else {
            let values: HashSet<i32> =
                monitoring::get_monitored_episode_numbers(&state.db, found.series.id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
            monitored_cache.insert(found.series.id, values.clone());
            values
        };

        let actionable_eps: HashSet<i32> = found
            .resolved_eps
            .iter()
            .copied()
            .filter(|ep| monitored_eps.contains(ep))
            .collect();

        let disk_files = if let Some(cached) = disk_cache.get(&found.series.folder_name) {
            cached
        } else {
            let files = media::scan_series_folder(&cfg.media_root, &found.series.folder_name).await;
            disk_cache
                .entry(found.series.folder_name.clone())
                .or_insert(files)
        };
        let qtags = if let Some(cached) = quality_tags_cache.get(&found.series.id) {
            cached
        } else {
            let tags = episode_tags::get_for_series(&state.db, found.series.id)
                .await
                .unwrap_or_default();
            quality_tags_cache.entry(found.series.id).or_insert(tags)
        };
        // Classify the incoming release once per item using the full
        // pre-disk pipeline (filename + group-map + temporal +
        // description-body-when-ambiguous). Stashed on
        // `PendingCandidate` below so the grab path can reuse it for
        // `episode_tags::record_grab` without re-classifying.
        //
        // Description-body fetching inside `classify_release` is
        // already self-gated: clean L1+L3+L4 verdicts skip the HTTP
        // entirely; ambiguous items fetch once and cache via
        // `nyaa_description_cache` so downstream (scoring, grab) hits
        // the cache.
        let incoming_classification = source::classify_release(
            &state.db,
            &item.title,
            Some(&item.resolution),
            Some(source::NyaaContext {
                info_hash: &item.info_hash,
                view_url: &item.link,
                is_batch: item.is_batch,
            }),
            Some(source::SeriesContext {
                status: &found.series.status,
                season_year: found.series.season_year,
                end_year: found.series.end_year,
            }),
        )
        .await;
        let decision = evaluate_candidate(
            &found.series,
            &item,
            &incoming_classification,
            disk_files,
            &actionable_eps,
            &cutoff,
            qtags,
        );
        if let Some(reason) = decision.reject_reason {
            skipped += 1;
            let reason = format!("{} | {}", reason, build_match_diag(&item, Some(&found), 0));
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &item_key,
                    title: &item.title,
                    link: &item.link,
                    series_id: Some(found.series.id),
                    series_title: &found.series.title,
                    group_name: &item.group,
                    is_batch: item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        }

        let canonical_key = canonical_episode_key(&found, item.is_batch);
        if !canonical_key.is_empty() && canonical_history.contains(&canonical_key) {
            skipped += 1;
            let reason = format!(
                "Logical episode is already queued or was grabbed earlier | {}",
                build_match_diag(&item, Some(&found), 0)
            );
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &item_key,
                    title: &item.title,
                    link: &item.link,
                    series_id: Some(found.series.id),
                    series_title: &found.series.title,
                    group_name: &item.group,
                    is_batch: item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        }

        let score = score_candidate(
            &state.db,
            &cfg,
            &item,
            &found.series,
            &found.resolved_eps,
            found.alias_score,
            found.parsed.parse_mode,
            &cfs,
            &empty_seadex_hashes,
        )
        .await;
        pending.push(PendingCandidate {
            item,
            item_key,
            found,
            score,
            new_episode_count: decision.new_episode_count,
            is_upgrade: decision.is_upgrade,
            classification: incoming_classification,
        });
    }

    let mut bucket_best: HashMap<String, usize> = HashMap::new();
    for (idx, cand) in pending.iter().enumerate() {
        let bucket = logical_bucket_key(cand);
        match bucket_best.get(&bucket).copied() {
            Some(prev_idx) => {
                if compare_candidates(cand, &pending[prev_idx]) == Ordering::Greater {
                    bucket_best.insert(bucket, idx);
                }
            }
            None => {
                bucket_best.insert(bucket, idx);
            }
        }
    }

    // multi-rss commit F — top-level guard is now "any client at
    // all configured" (default exists). Per-item dispatch routing
    // happens inside the grab loop via `resolve_dispatch_for_item`.
    if state.default_download_client().await.is_none() {
        for cand in pending {
            skipped += 1;
            let reason = format!(
                "Download client is not configured | {}",
                build_match_diag(&cand.item, Some(&cand.found), cand.score)
            );
            let (src_str, src_id) = source_dedup_key(&cand.item.source);
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &cand.item_key,
                    title: &cand.item.title,
                    link: &cand.item.link,
                    series_id: Some(cand.found.series.id),
                    series_title: &cand.found.series.title,
                    group_name: &cand.item.group,
                    is_batch: cand.item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
        }
        let detail = format!(
            "Processed {} items • matched {} • grabbed {} • skipped {}",
            items_seen, matched, grabbed, skipped
        );
        logger::info(&state.db, LogCategory::Rss, "RSS sync finished", &detail).await;
        return Ok(SyncSummary {
            items_seen,
            matched,
            grabbed,
            skipped,
            detail,
        });
    }

    for (idx, cand) in pending.into_iter().enumerate() {
        let (cand_src_str, cand_src_id) = source_dedup_key(&cand.item.source);
        let bucket = logical_bucket_key(&cand);
        if bucket_best.get(&bucket).copied() != Some(idx) {
            skipped += 1;
            let reason = format!(
                "Lower score than selected candidate for the same logical episode | {}",
                build_match_diag(&cand.item, Some(&cand.found), cand.score)
            );
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &cand.item_key,
                    title: &cand.item.title,
                    link: &cand.item.link,
                    series_id: Some(cand.found.series.id),
                    series_title: &cand.found.series.title,
                    group_name: &cand.item.group,
                    is_batch: cand.item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: cand_src_str,
                    source_id: cand_src_id,
                },
            )
            .await;
            continue;
        }

        let grab_url = if !cand.item.torrent.is_empty() {
            cand.item.torrent.clone()
        } else if !cand.item.magnet.is_empty() {
            cand.item.magnet.clone()
        } else {
            cand.item.link.clone()
        };

        let info_hash = crate::services::nyaa::extract_hash(&grab_url);
        // multi-rss commit F — resolve the dispatch client per-item
        // based on the source's pin (Nyaa pin / indexer pin /
        // direct-feed pin). A pin pointing at a deleted client
        // returns None — record the per-item rejection without
        // shutting down the whole sync (other items in this tick
        // may have valid pins).
        let Some((client, dispatch_client_id)) =
            resolve_dispatch_for_item(state, &cfg, &cand.item, &direct_feed_pins).await
        else {
            skipped += 1;
            let reason = format!(
                "No download client resolves for source {} | {}",
                cand.item.source.label(),
                build_match_diag(&cand.item, Some(&cand.found), cand.score)
            );
            let (src_str, src_id) = source_dedup_key(&cand.item.source);
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &cand.item_key,
                    title: &cand.item.title,
                    link: &cand.item.link,
                    series_id: Some(cand.found.series.id),
                    series_title: &cand.found.series.title,
                    group_name: &cand.item.group,
                    is_batch: cand.item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        };
        // use the returning-id variant so SAB grabs persist
        // their `nzo_id` instead of the pre-computed BT-style hash.
        // For BT clients the returned id equals `info_hash` (default
        // impl), no behavior change.
        // Misgrab guardrails: the blocklist wins over RSS matching. A
        // release the sweep removed (or the user failed) is never
        // re-grabbed from a feed, by hash or by exact title.
        if crate::models::grabbed_torrents::is_blocklisted_release(
            &state.db,
            cand.found.series.id,
            &info_hash,
            &cand.item.title,
        )
        .await
        {
            skipped += 1;
            let reason = format!(
                "Blocklisted release | {}",
                build_match_diag(&cand.item, Some(&cand.found), cand.score)
            );
            let (src_str, src_id) = source_dedup_key(&cand.item.source);
            let _ = rss::record_decision(
                &state.db,
                rss::DecisionRecord {
                    item_key: &cand.item_key,
                    title: &cand.item.title,
                    link: &cand.item.link,
                    series_id: Some(cand.found.series.id),
                    series_title: &cand.found.series.title,
                    group_name: &cand.item.group,
                    is_batch: cand.item.is_batch,
                    decision: "rejected",
                    reason: &reason,
                    source: src_str,
                    source_id: src_id,
                },
            )
            .await;
            continue;
        }
        match client.add_torrent_returning_id(&grab_url, &info_hash).await {
            Ok((_outcome, canonical_id)) => {
                grabbed += 1;
                let action = if cand.is_upgrade { "upgrade" } else { "new" };
                let reason = if cand.item.is_batch {
                    format!(
                        "Accepted best batch candidate ({}) for {} episode(s) | {}",
                        action,
                        cand.new_episode_count.max(1),
                        build_match_diag(&cand.item, Some(&cand.found), cand.score)
                    )
                } else {
                    format!(
                        "Accepted best candidate ({}) for {} episode(s) | {}",
                        action,
                        cand.new_episode_count.max(1),
                        build_match_diag(&cand.item, Some(&cand.found), cand.score)
                    )
                };
                canonical_history.insert(canonical_episode_key(&cand.found, cand.item.is_batch));
                let _ = rss::record_decision(
                    &state.db,
                    rss::DecisionRecord {
                        item_key: &cand.item_key,
                        title: &cand.item.title,
                        link: &cand.item.link,
                        series_id: Some(cand.found.series.id),
                        series_title: &cand.found.series.title,
                        group_name: &cand.item.group,
                        is_batch: cand.item.is_batch,
                        decision: "grabbed",
                        reason: &reason,
                        source: cand_src_str,
                        source_id: cand_src_id,
                    },
                )
                .await;
                // Record for post-processing. Persist the canonical
                // id returned by the client (BT: matches info_hash;
                // SAB: nzo_id) so post-processing's match-by-hash
                // works for both.
                let ep_list: Vec<i32> = cand.found.resolved_eps.iter().copied().collect();
                let grab_id = crate::models::grabbed_torrents::record_grab(
                    &state.db,
                    &canonical_id,
                    &cand.item.title,
                    cand.found.series.id,
                    &ep_list,
                    cand.item.is_batch,
                )
                .await
                .ok()
                .flatten();
                // Misgrab guardrails: keep the URL so Restore can re-add a removed grab.
                if let Some(gid) = grab_id {
                    let _ =
                        crate::models::grabbed_torrents::set_source_url(&state.db, gid, &grab_url)
                            .await;
                }
                if let Some(gid) = grab_id {
                    let _ = crate::models::grabbed_torrents::set_download_client(
                        &state.db,
                        gid,
                        Some(dispatch_client_id),
                    )
                    .await;
                    // Issue #118 — fire `Grabbed`. RSS path doesn't
                    // run a scoring pass (feed → match-by-title is
                    // direct), so `score = None`. Indexer attribution
                    // is the source's name: indexer-RSS rows carry an
                    // indexers-table id we resolve via the cache;
                    // direct user-feed rows surface their feed name
                    // verbatim; Nyaa-direct stays None.
                    let indexer = match &cand.item.source {
                        RssSource::Nyaa => None,
                        RssSource::UserFeed { name, .. } => Some(name.clone()),
                        RssSource::Indexer { id, .. } => {
                            crate::services::notifications::resolve_indexer_name(state, Some(*id))
                                .await
                        }
                    };
                    crate::services::notifications::emit_grabbed(
                        state,
                        cand.found.series.id,
                        ep_list.first().copied().unwrap_or(0),
                        &cand.item.title,
                        indexer,
                        None,
                        Some(client.sonarr_impl_name().to_string()),
                    )
                    .await;
                }
                // Reuse the pre-disk classification computed earlier
                // during the upgrade gate (stashed on the pending
                // candidate). Saves a second DB + potential HTTP round
                // trip per grabbed item.
                let classification = &cand.classification;
                for ep_num in &ep_list {
                    // RSS items don't carry size info in the feed — the
                    // grab history row starts at 0 and post-processing
                    // fills it in with the actual imported file size at
                    // import time. For batches, every per-episode row
                    // of the pack carries the same pack-total zero here
                    // until post-processing refines to per-file size.
                    //
                    // `is_batch` is threaded through from the RSS item
                    // so episode_grab_history.is_batch correctly flags
                    // rows that came from a pack. Older comments here
                    // asserted RSS feeds only surface single-episode
                    // releases — that's no longer true: RSS now handles
                    // batches (see the evaluate_candidate batch branch)
                    // and the flag feeds the Needs Review UI and the
                    // post-processing sibling-routing safety net.
                    let _ = episode_tags::record_grab(
                        &state.db,
                        cand.found.series.id,
                        *ep_num,
                        classification,
                        &cand.item.title,
                        &cand.item.group,
                        0,
                        cand.item.is_batch,
                    )
                    .await;
                }

                // Grab-time sibling detection for batch grabs — without
                // this, a Monogatari-batch that actually contains
                // Owarimonogatari files has its per-sibling grab_history
                // rows transiently attributed to the parent series until
                // post-processing's import-time safety net re-routes
                // them. Files end up in the right folder either way,
                // but the series page reads grab history for progress
                // display so the UI looked wrong in the meantime.
                //
                // Only runs on batch grabs with a positive provider_id
                // (AniList-sourced series). Jikan-fallback series with
                // synthetic negative ids can't walk AL relations to
                // discover siblings, so auto_expand isn't useful there.
                // The `tokio::spawn` is fire-and-forget with a 180s
                // metadata wait inside — the RSS sync cycle finishes
                // long before this completes.
                if cand.item.is_batch
                    && let Some(grab_id) = grab_id
                    && cand.found.series.anilist_id > 0
                {
                    let db_task = state.db.clone();
                    let client_arc = client.clone();
                    let info_hash_task = cand.item.info_hash.clone();
                    let provider_id = cand.found.series.anilist_id;
                    let parent_series_id = cand.found.series.id;
                    let ep_list_task = ep_list.clone();
                    let title_task = cand.item.title.clone();
                    let grab_ctx_task = crate::services::auto_expand::AutoExpandGrabContext {
                        classification: classification.clone(),
                        release_group: cand.item.group.clone(),
                        size_bytes: 0,
                    };
                    tokio::spawn(async move {
                        // Cache-only detail lookup: if metadata hasn't
                        // been cached yet (unusual for a series the
                        // user has added) we fall back to letting
                        // post-processing handle sibling routing at
                        // import time.
                        let detail = match crate::models::metadata_cache::get_by_provider_id(
                            &db_task,
                            provider_id,
                        )
                        .await
                        {
                            Ok(Some(row)) => row.detail,
                            _ => return,
                        };
                        let files = match crate::services::download_client::wait_for_files(
                            &*client_arc,
                            &info_hash_task,
                            std::time::Duration::from_secs(180),
                        )
                        .await
                        {
                            Ok(files) => files,
                            Err(_) => return,
                        };
                        let filenames: Vec<String> = files.into_iter().map(|f| f.name).collect();
                        crate::services::auto_expand::expand_from_files(
                            &db_task,
                            &filenames,
                            &detail,
                            parent_series_id,
                            &ep_list_task,
                            grab_id,
                            &title_task,
                            &grab_ctx_task,
                        )
                        .await;
                    });
                }
            }
            Err(err) => {
                skipped += 1;
                let reason = format!(
                    "{} | {}",
                    err,
                    build_match_diag(&cand.item, Some(&cand.found), cand.score)
                );
                let _ = rss::record_decision(
                    &state.db,
                    rss::DecisionRecord {
                        item_key: &cand.item_key,
                        title: &cand.item.title,
                        link: &cand.item.link,
                        series_id: Some(cand.found.series.id),
                        series_title: &cand.found.series.title,
                        group_name: &cand.item.group,
                        is_batch: cand.item.is_batch,
                        decision: "error",
                        reason: &reason,
                        source: cand_src_str,
                        source_id: cand_src_id,
                    },
                )
                .await;
            }
        }
    }

    let detail = format!(
        "Processed {} items • matched {} • grabbed {} • skipped {}",
        items_seen, matched, grabbed, skipped
    );
    logger::info(&state.db, LogCategory::Rss, "RSS sync finished", &detail).await;
    Ok(SyncSummary {
        items_seen,
        matched,
        grabbed,
        skipped,
        detail,
    })
}

async fn load_canonical_history(
    db: &sqlx::SqlitePool,
    client: Option<&dyn crate::services::download_client::DownloadClient>,
    all_meta: &[SeriesMeta],
) -> HashSet<String> {
    let mut keys = HashSet::new();

    if let Ok(titles) = rss::grabbed_titles(db, 5000).await {
        for title in titles {
            if let Some(key) = canonical_key_for_title(&title, all_meta) {
                keys.insert(key);
            }
        }
    }

    if let Some(client) = client
        && let Ok(torrents) = client.list_scoped().await
    {
        for torrent in torrents {
            if let Some(key) = canonical_key_for_title(&torrent.name, all_meta) {
                keys.insert(key);
            }
        }
    }

    keys
}

/// Match a release title against every tracked series in the library
/// and return the best match's series id + resolved episode set, or
/// `None` if no series cleared the matcher's confidence threshold.
/// Used by the manual-search grab path (v1.3.0 plan item 6d) to
/// link grabs to existing library entries without re-implementing
/// the RSS matcher.
///
/// `is_batch` comes from the caller (the search UI already knows),
/// so we don't need to re-run `detect_batch` here. Returns the
/// resolved episode numbers so the grab recorder can populate
/// episode_grab_history correctly.
pub async fn match_library_title(
    db: &sqlx::SqlitePool,
    title: &str,
    is_batch: bool,
) -> Option<(series::Series, Vec<i32>)> {
    let all_series = series::get_all(db).await.ok()?;
    if all_series.is_empty() {
        return None;
    }
    let all_meta: Vec<SeriesMeta> = all_series.iter().map(SeriesMeta::from_series).collect();
    // Synthetic RssItem for the matcher — `source` doesn't matter
    // here because this path is title-only matching for the
    // post-grab attribution helper, not a sync-time fetch.
    let pseudo = RssItem {
        title: title.to_string(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: extract_group(title),
        resolution: extract_resolution(title),
        is_batch,
        source: RssSource::Nyaa,
    };
    let found = best_series_match(&pseudo, &all_meta)?;
    let eps: Vec<i32> = found.resolved_eps.iter().copied().collect();
    Some((found.series, eps))
}

fn canonical_key_for_title(title: &str, all_meta: &[SeriesMeta]) -> Option<String> {
    // Same synthetic-item pattern as `find_series_for_title` —
    // matcher inputs only, no real grab.
    let pseudo = RssItem {
        title: title.to_string(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: extract_group(title),
        resolution: extract_resolution(title),
        is_batch: detect_batch(title),
        source: RssSource::Nyaa,
    };
    let found = best_series_match(&pseudo, all_meta)?;
    let key = canonical_episode_key(&found, pseudo.is_batch);
    if key.is_empty() { None } else { Some(key) }
}

fn compare_candidates(a: &PendingCandidate, b: &PendingCandidate) -> Ordering {
    a.score
        .cmp(&b.score)
        .then_with(|| (!a.item.is_batch).cmp(&(!b.item.is_batch)))
        .then_with(|| resolution_rank(&a.item.resolution).cmp(&resolution_rank(&b.item.resolution)))
        .then_with(|| a.item.group.cmp(&b.item.group))
        .then_with(|| a.item.title.cmp(&b.item.title))
}

fn logical_bucket_key(cand: &PendingCandidate) -> String {
    canonical_episode_key(&cand.found, cand.item.is_batch)
}

fn canonical_episode_key(found: &MatchResult, is_batch: bool) -> String {
    let episode_key = if !found.canonical_abs_eps.is_empty() {
        format_episode_set(&found.canonical_abs_eps)
    } else {
        format_episode_set(&found.resolved_eps)
    };
    if episode_key == "none" {
        return String::new();
    }
    format!(
        "{}|{}|{}",
        found.family_key,
        if is_batch { "batch" } else { "single" },
        episode_key,
    )
}

#[allow(clippy::too_many_arguments)]
async fn score_candidate(
    db: &sqlx::SqlitePool,
    cfg: &config::Config,
    item: &RssItem,
    found: &series::Series,
    parsed_eps: &HashSet<i32>,
    alias_score: f32,
    parse_mode: &str,
    cfs: &[crate::services::custom_formats::CompiledCustomFormat],
    seadex_hashes: &HashSet<String>,
) -> i32 {
    let preferred_source = Source::from_str(&cfg.preferred_source);
    let preferred_resolution = Resolution::from_str(&cfg.preferred_resolution);
    // Scoring uses the coarse Source rank, so collapse any BluRay sub-tier
    // (bluray_remux/bluray_bdmv) back to plain BluRay here. Upgrade-detection
    // and anywhere else that needs the sub-tier already went through
    // `parse_cutoff_source` at their own call sites.
    let (cutoff_source, _, _) = source::parse_cutoff_source(&cfg.cutoff_source);
    let cutoff_resolution = Resolution::from_str(&cfg.cutoff_resolution);
    let finished_mode = quality::FinishedSeriesMode::from_str(&cfg.finished_series_quality);

    let classification = source::classify_release(
        db,
        &item.title,
        Some(&item.resolution),
        Some(source::NyaaContext {
            info_hash: &item.info_hash,
            view_url: &item.link,
            is_batch: item.is_batch,
        }),
        Some(source::SeriesContext {
            status: &found.status,
            season_year: found.season_year,
            end_year: found.end_year,
        }),
    )
    .await;
    let mut score = source::score_classification(
        &classification,
        preferred_source,
        preferred_resolution,
        cutoff_source,
        cutoff_resolution,
    );

    score += quality::preferred_group_bonus(
        &item.group,
        &quality::parse_group_list(&cfg.preferred_groups),
    );
    score += (alias_score * 50.0) as i32;

    if !item.is_batch {
        score += 25;
    } else if is_finished_status(&found.status)
        || finished_mode != quality::FinishedSeriesMode::SameAsAiring
    {
        score += 5;
    } else {
        score -= 15;
    }

    if item.resolution == cfg.preferred_resolution {
        score += 20;
    }
    if parsed_eps.is_empty() {
        score -= 60;
    } else {
        score += 15;
    }

    match parse_mode {
        "season_episode" | "season_dash_episode" | "season_episode_range" => score += 25,
        "absolute" | "absolute_dash" => score += 15,
        "range" => score += 10,
        _ => score -= 10,
    }

    // CF overlay — the auto-search and upgrade paths fold the user's
    // compiled Custom Formats into scoring at the equivalent layer; RSS
    // used to skip this and silently rank candidates by the heuristic
    // pieces above only. `total_cf_score_for_release` saturates the
    // sum across CFs so a 10k-magnitude TRaSH boost can't wrap to a
    // negative on overflow. RssItem doesn't carry size_bytes today, so
    // CFs with a Size spec (rare in TRaSH-anime) won't match — known
    // limitation, separable from this fix.
    score = score.saturating_add(crate::services::custom_formats::total_cf_score_for_release(
        cfs,
        &classification,
        &item.title,
        &item.group,
        0,
        &item.info_hash,
        seadex_hashes,
    ));

    score
}

fn resolution_rank(value: &str) -> i32 {
    match value.trim() {
        "2160" => 2160,
        "1080" => 1080,
        "720" => 720,
        "480" => 480,
        _ => 0,
    }
}

/// Decide whether an RSS candidate should be grabbed. `incoming` is
/// the pre-disk classification of the release (filename + group-map +
/// temporal + description-when-ambiguous) computed once by the caller
/// — passed in here for the upgrade gate and reused later by the
/// grab path for `episode_tags::record_grab`, so we only classify
/// each item once per cycle even when it reaches the grab stage.
///
/// Synchronous: no DB or HTTP inside this function. All the live
/// lookups happen in the caller before calling this. That also makes
/// the function unit-testable without a pool or mock client.
fn evaluate_candidate(
    found: &series::Series,
    item: &RssItem,
    incoming: &ClassificationResult,
    disk_files: &[media::EpisodeFile],
    parsed_eps: &HashSet<i32>,
    cutoff: &ClassificationResult,
    quality_tags: &HashMap<i32, crate::models::episode_tags::EpisodeQualityTag>,
) -> CandidateDecision {
    let existing_ep_numbers: HashSet<i32> = disk_files.iter().map(|f| f.episode_number).collect();

    if item.is_batch {
        if !parsed_eps.is_empty() {
            // Pack-level decision: accept only when every episode in
            // the pack's *covered* range is either missing from disk
            // or a genuine upgrade over what's on disk. This matches
            // the behavior we'd want Sonarr-style — per-episode upgrade
            // evaluation — without the complication of selective-file
            // download in RSS, because `do_file_op` in post-processing
            // imports *every* file from the torrent folder and
            // `fs::rename`/`fs::copy`/`fs::hard_link` silently overwrite
            // on conflict. Grabbing a pack where even one covered
            // episode would be a sidegrade or downgrade means that
            // episode gets clobbered at import time — possibly with a
            // worse-quality version from a different group. So the
            // conservative rule is "every covered episode in the pack
            // must be actionable" (missing or upgradeable).
            let new_count = parsed_eps
                .iter()
                .filter(|ep| !existing_ep_numbers.contains(ep))
                .count() as i32;
            let upgrade_count = parsed_eps
                .iter()
                .filter(|ep| {
                    existing_ep_numbers.contains(ep)
                        && episode_is_upgradeable(ep, disk_files, incoming, cutoff, quality_tags)
                })
                .count() as i32;
            let actionable = new_count + upgrade_count;
            let covered = parsed_eps.len() as i32;

            if actionable == 0 {
                return CandidateDecision {
                    reject_reason: Some(
                        "Batch episodes are already on disk at or above cutoff".to_string(),
                    ),
                    new_episode_count: 0,
                    is_upgrade: false,
                };
            }
            if actionable < covered {
                let not_actionable = covered - actionable;
                return CandidateDecision {
                    reject_reason: Some(format!(
                        "Batch would overwrite {} non-upgradeable episode(s) on disk (pack covers {} total, only {} are missing-or-upgradeable). Manual search bypasses this gate if you explicitly want to replace those episodes.",
                        not_actionable, covered, actionable
                    )),
                    new_episode_count: 0,
                    is_upgrade: false,
                };
            }
            return CandidateDecision {
                reject_reason: None,
                new_episode_count: actionable,
                is_upgrade: upgrade_count > 0 && new_count == 0,
            };
        }

        if is_finished_status(&found.status) {
            // Finished-series batch with no parsed range. The convenience
            // path is: user adds an old series, a BD batch shows up,
            // grab it. But we can only grab blindly when nothing is on
            // disk — otherwise `do_file_op` in post-processing would
            // silently overwrite existing episodes with whatever the
            // batch contains, with no per-episode upgrade check
            // possible (the pack's episode range is unknown). Safer to
            // reject and let the user grab intentionally via manual
            // search when they have existing episodes.
            if !existing_ep_numbers.is_empty() {
                return CandidateDecision {
                    reject_reason: Some(format!(
                        "Finished-series batch rejected: series has {} episode(s) on disk and the pack's episode range is unknown — can't verify whether the batch would overwrite them with worse quality. Manual search bypasses this gate if you explicitly want to replace those episodes.",
                        existing_ep_numbers.len()
                    )),
                    new_episode_count: 0,
                    is_upgrade: false,
                };
            }
            return CandidateDecision {
                reject_reason: None,
                new_episode_count: 0,
                is_upgrade: false,
            };
        }

        return CandidateDecision {
            reject_reason: Some("Batch release does not include monitored episodes".to_string()),
            new_episode_count: 0,
            is_upgrade: false,
        };
    }

    if parsed_eps.is_empty() {
        return CandidateDecision {
            reject_reason: Some("Resolved episode is not monitored".to_string()),
            new_episode_count: 0,
            is_upgrade: false,
        };
    }

    let new_count = parsed_eps
        .iter()
        .filter(|ep| !existing_ep_numbers.contains(ep))
        .count() as i32;
    let upgrade_count = parsed_eps
        .iter()
        .filter(|ep| {
            existing_ep_numbers.contains(ep)
                && episode_is_upgradeable(ep, disk_files, incoming, cutoff, quality_tags)
        })
        .count() as i32;
    let actionable = new_count + upgrade_count;

    if actionable == 0 {
        return CandidateDecision {
            reject_reason: Some("Episode is already on disk at or above cutoff".to_string()),
            new_episode_count: 0,
            is_upgrade: false,
        };
    }

    CandidateDecision {
        reject_reason: None,
        new_episode_count: actionable,
        is_upgrade: upgrade_count > 0 && new_count == 0,
    }
}

/// Check if an episode on disk is below the quality cutoff and the
/// already-classified incoming release would be an upgrade.
///
/// Caller is responsible for running the incoming classification once
/// per item (it's expensive enough — group-map DB lookup + potentially
/// a description fetch — that re-doing it per covered episode in a
/// batch would be wasteful). Existing side still classifies
/// per-episode because each row on disk can have its own
/// `episode_quality_tags` verdict.
fn episode_is_upgradeable(
    ep: &i32,
    disk_files: &[media::EpisodeFile],
    incoming: &ClassificationResult,
    cutoff: &ClassificationResult,
    quality_tags: &HashMap<i32, crate::models::episode_tags::EpisodeQualityTag>,
) -> bool {
    let Some(existing) = disk_files.iter().find(|f| f.episode_number == *ep) else {
        return false; // not on disk — not an "upgrade", it's a new episode
    };
    let existing_classification =
        auto_search::resolve_existing_classification(existing, quality_tags.get(ep));
    // If we can't place existing anywhere, be conservative and don't upgrade.
    if existing_classification.source == Source::Unknown
        && existing_classification.resolution == Resolution::Unknown
    {
        return false;
    }
    // Only upgrade if existing is below cutoff.
    if existing_classification.rank() >= cutoff.rank() {
        return false;
    }
    // Shared upgrade policy: strictly better on the rank tuple AND
    // not a non-BDMV → BDMV crossing. See `source::is_valid_upgrade`.
    source::is_valid_upgrade(&existing_classification, incoming)
}

fn is_finished_status(status: &str) -> bool {
    matches!(status, "FINISHED" | "FINISHED_AIRING")
}

fn best_series_match(item: &RssItem, all_meta: &[SeriesMeta]) -> Option<MatchResult> {
    let parsed = parse_release(item);
    let item_tokens = auto_search::token_set(&parsed.normalized_title);
    let item_core_tokens = auto_search::token_set(&parsed.core_title);

    let mut best: Option<(f32, MatchResult)> = None;

    let item_view = ItemView {
        normalized: &parsed.normalized_title,
        tokens: &item_tokens,
        core: &parsed.core_title,
        core_tokens: &item_core_tokens,
        collapsed: &parsed.collapsed_title,
        collapsed_core: &parsed.collapsed_core_title,
    };

    for meta in all_meta {
        let alias_set = AliasSet {
            aliases: &meta.aliases,
            core_aliases: &meta.core_aliases,
            collapsed_aliases: &meta.collapsed_aliases,
            collapsed_core_aliases: &meta.collapsed_core_aliases,
        };
        let alias_score = score_alias_overlap(&item_view, &alias_set);
        if alias_score < 0.82 {
            continue;
        }

        let siblings = related_family(meta, all_meta);
        let (resolved_eps, resolution_mode) = resolve_episode_numbers(&parsed, meta, &siblings);
        let canonical_abs_eps = canonical_absolute_numbers(meta, &siblings, &resolved_eps);
        let family_key = canonical_family_key(&siblings);

        let mut score = alias_score;
        if let Some(item_season) = parsed.season_hint {
            match meta.season_num {
                Some(season) if season == item_season => score += 0.55,
                Some(_) => score -= 0.45,
                None => score -= 0.10,
            }
        }
        if !parsed.season_relative_eps.is_empty() || !parsed.absolute_eps.is_empty() {
            if resolved_eps.is_empty() {
                score -= 0.45;
            } else {
                score += 0.22;
            }
        }
        if !canonical_abs_eps.is_empty() {
            score += 0.08;
        }

        if score < 0.88 {
            continue;
        }

        let result = MatchResult {
            series: meta.series.clone(),
            parsed: parsed.clone(),
            resolved_eps,
            canonical_abs_eps,
            family_key,
            alias_score,
            resolution_mode,
        };
        match &best {
            Some((best_score, _)) if *best_score >= score => {}
            _ => best = Some((score, result)),
        }
    }

    best.map(|(_, result)| result)
}

fn canonical_family_key(family: &[SeriesMeta]) -> String {
    let mut keys: Vec<String> = family
        .iter()
        .flat_map(|meta| meta.collapsed_core_aliases.iter().cloned())
        .filter(|value| !value.is_empty())
        .collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .next()
        .unwrap_or_else(|| "unknownfamily".to_string())
}

fn canonical_absolute_numbers(
    meta: &SeriesMeta,
    family: &[SeriesMeta],
    resolved_eps: &HashSet<i32>,
) -> HashSet<i32> {
    if resolved_eps.is_empty() {
        return HashSet::new();
    }
    let offset = family_offset_for(meta, family);
    resolved_eps.iter().map(|ep| ep + offset).collect()
}

fn family_offset_for(meta: &SeriesMeta, family: &[SeriesMeta]) -> i32 {
    let target_season = meta.season_num.unwrap_or(1);
    let mut offset = 0i32;
    for entry in family {
        let season = entry.season_num.unwrap_or(1);
        if season >= target_season {
            break;
        }
        offset += entry.series.episodes.unwrap_or(0).max(0);
    }
    offset
}

fn related_family<'a>(target: &'a SeriesMeta, all_meta: &'a [SeriesMeta]) -> Vec<SeriesMeta> {
    let mut related: Vec<SeriesMeta> = all_meta
        .iter()
        .filter(|meta| shares_core_alias(target, meta))
        .cloned()
        .collect();
    related.sort_by(compare_series_meta);
    related
}

fn shares_core_alias(a: &SeriesMeta, b: &SeriesMeta) -> bool {
    if a.series.id == b.series.id {
        return true;
    }
    for ac in &a.collapsed_core_aliases {
        for bc in &b.collapsed_core_aliases {
            if !ac.is_empty() && ac == bc {
                return true;
            }
        }
    }
    for ac in &a.core_aliases {
        for bc in &b.core_aliases {
            if ac == bc {
                return true;
            }
            let at = auto_search::token_set(ac);
            let bt = auto_search::token_set(bc);
            if auto_search::token_overlap_ratio(&at, &bt) >= 0.95
                && auto_search::token_overlap_ratio(&bt, &at) >= 0.95
            {
                return true;
            }
        }
    }
    false
}

fn compare_series_meta(a: &SeriesMeta, b: &SeriesMeta) -> Ordering {
    match (a.season_num, b.season_num) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.series.id.cmp(&b.series.id),
    }
}

/// Four parallel views of an RSS item title — normalized, core-only,
/// and a collapsed (alphanumeric-only) variant of each — grouped so
/// `score_alias_overlap` can take a single bundle instead of six
/// positional `&str`/`&HashSet` args. Four `&str`s that happen to be
/// next to each other in a signature is exactly the kind of thing the
/// compiler can't protect you from at the call site.
struct ItemView<'a> {
    normalized: &'a str,
    tokens: &'a HashSet<String>,
    core: &'a str,
    core_tokens: &'a HashSet<String>,
    collapsed: &'a str,
    collapsed_core: &'a str,
}

/// The four parallel alias lists on a `SeriesMeta`, bundled together
/// for the same reason as `ItemView`: four `&[String]` args in a row
/// is a positional-swap hazard.
struct AliasSet<'a> {
    aliases: &'a [String],
    core_aliases: &'a [String],
    collapsed_aliases: &'a [String],
    collapsed_core_aliases: &'a [String],
}

fn score_alias_overlap(item: &ItemView<'_>, meta: &AliasSet<'_>) -> f32 {
    let alias_max = meta
        .aliases
        .iter()
        .map(|alias| {
            let normalized_alias = auto_search::normalize_title(alias);
            let alias_tokens = auto_search::token_set(&normalized_alias);
            let mut score = 0.0f32;
            if !normalized_alias.is_empty()
                && (item.normalized.contains(&normalized_alias)
                    || normalized_alias.contains(item.normalized))
            {
                score = score.max(1.0);
            }
            let overlap_ab = auto_search::token_overlap_ratio(item.tokens, &alias_tokens);
            let overlap_ba = auto_search::token_overlap_ratio(&alias_tokens, item.tokens);
            score.max(overlap_ab.min(overlap_ba))
        })
        .fold(0.0f32, f32::max);

    let core_max = meta
        .core_aliases
        .iter()
        .map(|alias_core| {
            let core_tokens = auto_search::token_set(alias_core);
            let mut score = 0.0f32;
            if !alias_core.is_empty()
                && !item.core.is_empty()
                && (item.core.contains(alias_core) || alias_core.contains(item.core))
            {
                score = score.max(1.0);
            }
            let overlap_ab = auto_search::token_overlap_ratio(item.core_tokens, &core_tokens);
            let overlap_ba = auto_search::token_overlap_ratio(&core_tokens, item.core_tokens);
            score.max(overlap_ab.min(overlap_ba))
        })
        .fold(0.0f32, f32::max);

    let collapsed_max = meta
        .collapsed_aliases
        .iter()
        .chain(meta.collapsed_core_aliases.iter())
        .map(|alias| {
            if alias.is_empty() {
                return 0.0;
            }
            if item.collapsed == *alias
                || item.collapsed_core == *alias
                || item.collapsed.contains(alias)
                || alias.contains(item.collapsed_core)
            {
                1.0
            } else {
                0.0
            }
        })
        .fold(0.0f32, f32::max);

    alias_max.max(core_max).max(collapsed_max)
}

fn normalize_core_title(value: &str) -> String {
    RE_CORE_TITLE
        .replace_all(value, " ")
        .split_whitespace()
        .filter(|token| !matches!(*token, "season" | "part" | "cour"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn collapse_alias(value: &str) -> String {
    value.chars().filter(|ch| ch.is_alphanumeric()).collect()
}

fn parse_season_number(value: &str) -> Option<i32> {
    for re in RE_SEASON_PATTERNS.iter() {
        if let Some(value) = re
            .captures(value)
            .and_then(|caps| caps.get(1))
            .and_then(|m| m.as_str().parse::<i32>().ok())
        {
            return Some(value);
        }
    }
    None
}

fn parse_release(item: &RssItem) -> ParsedRelease {
    let normalized_title = auto_search::normalize_title(&item.title);
    let core_title = normalize_core_title(&normalized_title);
    let collapsed_title = collapse_alias(&normalized_title);
    let collapsed_core_title = collapse_alias(&core_title);
    let lower = item.title.to_lowercase();

    let mut season_hint = parse_season_number(&normalized_title);
    let mut season_relative_eps = HashSet::new();
    let mut absolute_eps = HashSet::new();
    let mut parse_mode = "unknown";

    // Season+episode range patterns
    for re in RE_SEASON_EP_RANGE.iter() {
        if let Some(caps) = re.captures(&lower) {
            season_hint = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .or(season_hint);
            let start = caps
                .get(2)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .unwrap_or(0);
            let end = caps
                .get(3)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .unwrap_or(0);
            if start > 0 && end >= start && end - start <= 200 {
                for ep in start..=end {
                    season_relative_eps.insert(ep);
                }
                parse_mode = "season_episode_range";
                return ParsedRelease {
                    normalized_title,
                    core_title,
                    collapsed_title,
                    collapsed_core_title,
                    season_hint,
                    season_relative_eps,
                    absolute_eps,
                    parse_mode,
                };
            }
        }
    }

    // Season+episode single patterns
    for re in RE_SEASON_EP_SINGLE.iter() {
        if let Some(caps) = re.captures(&lower) {
            season_hint = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .or(season_hint);
            if let Some(ep) = caps.get(2).and_then(|m| m.as_str().parse::<i32>().ok()) {
                season_relative_eps.insert(ep);
                parse_mode = "season_episode";
                return ParsedRelease {
                    normalized_title,
                    core_title,
                    collapsed_title,
                    collapsed_core_title,
                    season_hint,
                    season_relative_eps,
                    absolute_eps,
                    parse_mode,
                };
            }
        }
    }

    // Season+dash+episode patterns
    for re in RE_SEASON_DASH.iter() {
        if let Some(caps) = re.captures(&lower) {
            season_hint = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .or(season_hint);
            if let Some(ep) = caps.get(2).and_then(|m| m.as_str().parse::<i32>().ok()) {
                season_relative_eps.insert(ep);
                parse_mode = "season_dash_episode";
                return ParsedRelease {
                    normalized_title,
                    core_title,
                    collapsed_title,
                    collapsed_core_title,
                    season_hint,
                    season_relative_eps,
                    absolute_eps,
                    parse_mode,
                };
            }
        }
    }

    // Mask season markers out of the search window for the absolute-
    // episode and plain-range passes. See RE_SEASON_MARKER_MASK.
    let mut masked = lower.clone();
    for re in RE_SEASON_MARKER_MASK.iter() {
        masked = re.replace_all(&masked, " ").to_string();
    }

    // Plain range (no season prefix)
    if let Some(caps) = RE_RANGE.captures(&masked) {
        let start = caps
            .get(1)
            .and_then(|m| m.as_str().parse::<i32>().ok())
            .unwrap_or(0);
        let end = caps
            .get(2)
            .and_then(|m| m.as_str().parse::<i32>().ok())
            .unwrap_or(0);
        if start > 0 && end >= start && end - start <= 200 {
            for ep in start..=end {
                absolute_eps.insert(ep);
            }
            parse_mode = "range";
        }
    }

    // Absolute episode patterns (run against the season-masked title
    // so e.g. the "3" in "Season 3 (web ..." doesn't get picked up as
    // absolute episode 3 via the digit-before-paren pattern).
    if absolute_eps.is_empty() {
        for (mode, re) in RE_ABSOLUTE.iter() {
            for caps in re.captures_iter(&masked) {
                if let Some(value) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
                    absolute_eps.insert(value);
                }
            }
            if !absolute_eps.is_empty() {
                parse_mode = mode;
                break;
            }
        }
    }

    ParsedRelease {
        normalized_title,
        core_title,
        collapsed_title,
        collapsed_core_title,
        season_hint,
        season_relative_eps,
        absolute_eps,
        parse_mode,
    }
}

fn resolve_episode_numbers(
    parsed: &ParsedRelease,
    meta: &SeriesMeta,
    family: &[SeriesMeta],
) -> (HashSet<i32>, &'static str) {
    if let Some(item_season) = parsed.season_hint {
        match meta.season_num {
            Some(season) if season != item_season => {
                return (HashSet::new(), "season_hint_miss");
            }
            None if item_season > 1 => {
                return (HashSet::new(), "season_hint_miss");
            }
            _ => {}
        }
    }

    if !parsed.season_relative_eps.is_empty() {
        if parsed.season_hint.is_some() {
            return (parsed.season_relative_eps.clone(), "explicit_season");
        }
        let direct_fit = meta
            .series
            .episodes
            .map(|eps| {
                parsed
                    .season_relative_eps
                    .iter()
                    .all(|n| *n >= 1 && *n <= eps)
            })
            .unwrap_or(true);
        if direct_fit {
            return (parsed.season_relative_eps.clone(), "season_relative");
        }
    }

    if !parsed.absolute_eps.is_empty() {
        if parsed.season_hint.is_some() {
            let direct_fit = meta
                .series
                .episodes
                .map(|eps| parsed.absolute_eps.iter().all(|n| *n >= 1 && *n <= eps))
                .unwrap_or(true);
            if direct_fit {
                return (parsed.absolute_eps.clone(), "season_hint_relative");
            }
            return (HashSet::new(), "season_hint_abs_miss");
        }

        if let Some(target_season) = meta.season_num {
            let mut offset = 0i32;
            for entry in family {
                let season = entry.season_num.unwrap_or(1);
                if season >= target_season {
                    break;
                }
                offset += entry.series.episodes.unwrap_or(0).max(0);
            }
            let mut mapped = HashSet::new();
            for number in &parsed.absolute_eps {
                let relative = *number - offset;
                if relative < 1 {
                    return (HashSet::new(), "absolute_miss");
                }
                if let Some(total) = meta.series.episodes
                    && relative > total
                {
                    return (HashSet::new(), "absolute_miss");
                }
                mapped.insert(relative);
            }
            if !mapped.is_empty() {
                return (mapped, "absolute_mapped");
            }
        } else if meta
            .series
            .episodes
            .map(|eps| parsed.absolute_eps.iter().all(|n| *n >= 1 && *n <= eps))
            .unwrap_or(true)
        {
            return (parsed.absolute_eps.clone(), "absolute_direct");
        }
    }

    (HashSet::new(), "unresolved")
}

fn ordinal_suffix(value: i32) -> String {
    let suffix = match value % 100 {
        11..=13 => "th",
        _ => match value % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{}{}", value, suffix)
}

fn format_episode_set(values: &HashSet<i32>) -> String {
    let mut items: Vec<i32> = values.iter().copied().collect();
    items.sort_unstable();
    if items.is_empty() {
        return "none".to_string();
    }
    items
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn build_match_diag(item: &RssItem, found: Option<&MatchResult>, score: i32) -> String {
    let parsed = parse_release(item);
    let raw_numbers_str = format_episode_set(&parsed.absolute_eps);
    let season_numbers_str = format_episode_set(&parsed.season_relative_eps);
    let resolved_eps_str = found
        .map(|m| format_episode_set(&m.resolved_eps))
        .unwrap_or_else(|| "none".to_string());
    let canonical_abs_str = found
        .map(|m| format_episode_set(&m.canonical_abs_eps))
        .unwrap_or_else(|| "none".to_string());
    let series_label = found.map(|m| m.series.title.as_str()).unwrap_or("none");
    let explicit_season = parsed
        .season_hint
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".to_string());
    let resolution_mode = found.map(|m| m.resolution_mode).unwrap_or("none");
    let family_key = found.map(|m| m.family_key.as_str()).unwrap_or("none");
    format!(
        "series={} | family={} | group={} | batch={} | season={} | rel={} | abs={} | resolved={} | canonical_abs={} | score={} | parse={} | mode={} | core={}",
        series_label,
        family_key,
        if item.group.trim().is_empty() {
            "none"
        } else {
            item.group.trim()
        },
        item.is_batch,
        explicit_season,
        season_numbers_str,
        raw_numbers_str,
        resolved_eps_str,
        canonical_abs_str,
        score,
        parsed.parse_mode,
        resolution_mode,
        parsed.core_title
    )
}

fn group_matches_whitelist(group: &str, whitelist: &[String]) -> bool {
    whitelist
        .iter()
        .any(|wanted| wanted.eq_ignore_ascii_case(group.trim()))
}

fn group_matches_blacklist(group: &str, blacklist: &[String]) -> bool {
    blacklist
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(group.trim()))
}

#[cfg(test)]
mod tests;
