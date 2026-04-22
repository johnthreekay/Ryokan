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

mod feed;
use feed::{build_item_key, detect_batch, extract_group, extract_resolution, fetch_feeds};

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
static RE_SEASON_MARKER_MASK: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)\bseason\s*\d{1,2}\b").unwrap(),
        Regex::new(r"(?i)\b\d{1,2}(?:st|nd|rd|th)\s+season\b").unwrap(),
        Regex::new(r"(?i)\bpart\s*\d{1,2}\b").unwrap(),
        Regex::new(r"(?i)\bcour\s*\d{1,2}\b").unwrap(),
        Regex::new(r"(?i)\bs\d{1,2}\b").unwrap(),
    ]
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
}

impl SeriesMeta {
    fn from_series(series: &series::Series) -> Self {
        let aliases = auto_search::dedupe_strings(vec![
            series.title.clone(),
            series.title_romaji.clone(),
            series.title_english.clone(),
            series.title_native.clone(),
        ]);

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

async fn sync_once_inner(state: &AppState, trigger: &str) -> Result<SyncSummary, String> {
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let has_music_series = tracked.iter().any(|s| s.format == "MUSIC");
    let items = fetch_feeds(cfg.allow_non_english, has_music_series).await?;
    for row in &tracked {
        let _ = monitoring_service::ensure_series_monitoring_rows(&state.db, row).await;
    }
    let client = state.download_client.read().await.clone();

    let whitelist = quality::parse_group_list(&cfg.preferred_groups);
    let blacklist = quality::parse_group_list(&cfg.blocked_groups);
    let all_meta: Vec<SeriesMeta> = tracked.iter().map(SeriesMeta::from_series).collect();
    let mut canonical_history =
        load_canonical_history(&state.db, client.as_deref(), &all_meta).await;

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
        LogCategory::System,
        "RSS sync started",
        &format!("trigger={} items={}", trigger, items.len()),
    )
    .await;

    // One SELECT instead of N: pre-load every previously-grabbed item_key
    // so the per-item dedup check is an in-memory HashSet lookup rather
    // than a round-trip per feed item. Nyaa typically returns ~100 items
    // per feed × multiple categories per sync, so this collapses 100+
    // sequential SELECTs into one.
    let already_grabbed = rss::grabbed_item_keys(&state.db)
        .await
        .map_err(|e| e.to_string())?;

    for item in items {
        items_seen += 1;
        let item_key = build_item_key(&item);
        if already_grabbed.contains(&item_key) {
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
                    source: "rss",
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
                    source: "rss",
                },
            )
            .await;
            continue;
        };

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
                    source: "rss",
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
                    source: "rss",
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

        let disk_files = disk_cache
            .entry(found.series.folder_name.clone())
            .or_insert_with(|| {
                media::scan_series_folder(&cfg.media_root, &found.series.folder_name)
            });
        let qtags = if let Some(cached) = quality_tags_cache.get(&found.series.id) {
            cached
        } else {
            let tags = episode_tags::get_for_series(&state.db, found.series.id)
                .await
                .unwrap_or_default();
            quality_tags_cache.entry(found.series.id).or_insert(tags)
        };
        let decision = evaluate_candidate(
            &state.db,
            &found.series,
            &item,
            disk_files,
            &actionable_eps,
            &cutoff,
            qtags,
        )
        .await;
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
                    source: "rss",
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
                    source: "rss",
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
        )
        .await;
        pending.push(PendingCandidate {
            item,
            item_key,
            found,
            score,
            new_episode_count: decision.new_episode_count,
            is_upgrade: decision.is_upgrade,
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

    let Some(client) = client.as_ref() else {
        for cand in pending {
            skipped += 1;
            let reason = format!(
                "Download client is not configured | {}",
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
                    source: "rss",
                },
            )
            .await;
        }
        let detail = format!(
            "Processed {} items • matched {} • grabbed {} • skipped {}",
            items_seen, matched, grabbed, skipped
        );
        logger::info(&state.db, LogCategory::System, "RSS sync finished", &detail).await;
        return Ok(SyncSummary {
            items_seen,
            matched,
            grabbed,
            skipped,
            detail,
        });
    };

    for (idx, cand) in pending.into_iter().enumerate() {
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
                    source: "rss",
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
        match client.add_torrent(&grab_url, &info_hash).await {
            Ok(_) => {
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
                        source: "rss",
                    },
                )
                .await;
                // Record for post-processing.
                let ep_list: Vec<i32> = cand.found.resolved_eps.iter().copied().collect();
                let grab_id = crate::models::grabbed_torrents::record_grab(
                    &state.db,
                    &cand.item.info_hash,
                    &cand.item.title,
                    cand.found.series.id,
                    &ep_list,
                    cand.item.is_batch,
                )
                .await
                .ok()
                .flatten();
                // Record quality tag + classification for episode status display.
                let classification = crate::services::source::classify_release(
                    &state.db,
                    &cand.item.title,
                    Some(&cand.item.resolution),
                    Some(crate::services::source::NyaaContext {
                        info_hash: &cand.item.info_hash,
                        view_url: &cand.item.link,
                        is_batch: cand.item.is_batch,
                    }),
                    Some(crate::services::source::SeriesContext {
                        status: &cand.found.series.status,
                        season_year: cand.found.series.season_year,
                        end_year: cand.found.series.end_year,
                    }),
                )
                .await;
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
                        &classification,
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
                        source: "rss",
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
    logger::info(&state.db, LogCategory::System, "RSS sync finished", &detail).await;
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

fn canonical_key_for_title(title: &str, all_meta: &[SeriesMeta]) -> Option<String> {
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

async fn score_candidate(
    db: &sqlx::SqlitePool,
    cfg: &config::Config,
    item: &RssItem,
    found: &series::Series,
    parsed_eps: &HashSet<i32>,
    alias_score: f32,
    parse_mode: &str,
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

async fn evaluate_candidate(
    db: &sqlx::SqlitePool,
    found: &series::Series,
    item: &RssItem,
    disk_files: &[media::EpisodeFile],
    parsed_eps: &HashSet<i32>,
    cutoff: &ClassificationResult,
    quality_tags: &HashMap<i32, crate::models::episode_tags::EpisodeQualityTag>,
) -> CandidateDecision {
    let existing_ep_numbers: HashSet<i32> = disk_files.iter().map(|f| f.episode_number).collect();
    // Classify the incoming release once per item using the full pre-
    // disk pipeline (filename + group-map + temporal + description).
    // Description-body fetching is already gated inside
    // `classify_release`: it fires only when L1+L3+L4 couldn't produce
    // a confident verdict, or when only the filename layer backs the
    // winner while other layers disagreed. Happy-path clean classifies
    // skip the HTTP entirely. Ambiguous items that do fetch cache the
    // result via `nyaa_description_cache`, so the downstream scoring
    // path and post-grab classification reuse the same value.
    let series_ctx = source::SeriesContext {
        status: &found.status,
        season_year: found.season_year,
        end_year: found.end_year,
    };
    let nyaa_ctx = source::NyaaContext {
        info_hash: &item.info_hash,
        view_url: &item.link,
        is_batch: item.is_batch,
    };
    let incoming_classification = source::classify_release(
        db,
        &item.title,
        Some(&item.resolution),
        Some(nyaa_ctx),
        Some(series_ctx),
    )
    .await;

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
                        && episode_is_upgradeable(
                            ep,
                            disk_files,
                            &incoming_classification,
                            cutoff,
                            quality_tags,
                        )
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
                        "Batch would overwrite {} non-upgradeable episode(s) on disk (pack covers {} total, only {} are missing-or-upgradeable). Grab intentionally via manual search if you want the pack.",
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
                        "Finished-series batch rejected: series has {} episode(s) on disk and the pack's episode range is unknown — can't verify whether the batch would overwrite them with worse quality. Grab via manual search if intentional.",
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
                && episode_is_upgradeable(
                    ep,
                    disk_files,
                    &incoming_classification,
                    cutoff,
                    quality_tags,
                )
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
mod parse_release_tests {
    use super::*;

    fn item(title: &str) -> RssItem {
        RssItem {
            title: title.to_string(),
            link: String::new(),
            guid: String::new(),
            torrent: String::new(),
            magnet: String::new(),
            info_hash: String::new(),
            group: extract_group(title),
            resolution: extract_resolution(title),
            is_batch: detect_batch(title),
        }
    }

    #[test]
    fn season_digit_is_not_parsed_as_absolute_episode() {
        // Regression: "[Kaizoku] Jujutsu Kaisen Season 3 (WEB 1080p HEVC
        // EAC-3) | The Culling Game Part 1" used to extract absolute
        // episode 3 from "season 3 (" via RE_ABSOLUTE's digit-before-
        // paren pattern. After the season-marker masking pass, "season
        // 3" and "part 1" are stripped from the absolute search window
        // so no spurious episode number survives.
        let parsed = parse_release(&item(
            "[Kaizoku] Jujutsu Kaisen Season 3 (WEB 1080p HEVC EAC-3) | The Culling Game Part 1",
        ));
        assert_eq!(parsed.season_hint, Some(3), "season_hint should be 3");
        assert!(
            parsed.absolute_eps.is_empty(),
            "absolute_eps should be empty, got {:?}",
            parsed.absolute_eps
        );
        assert!(
            parsed.season_relative_eps.is_empty(),
            "season_relative_eps should be empty, got {:?}",
            parsed.season_relative_eps
        );
    }

    #[test]
    fn hyphen_space_episode_still_parses_after_mask() {
        // Sanity: the standard "[Group] Series - 01 (1080p)" shape
        // should still resolve to absolute episode 1 after the mask
        // pass. The mask strips optional season tokens; there are none
        // here so the title passes through unchanged.
        let parsed = parse_release(&item("[SubsPlease] Frieren - 01 (1080p) [ABCD1234].mkv"));
        assert!(parsed.absolute_eps.contains(&1));
    }

    #[test]
    fn s3_prefix_does_not_leak_season_digit_to_episode() {
        // "[Group] Series S3 - 05 (1080p)" should extract season 3,
        // episode 5 — not both-3-and-5. Belongs to the season-dash
        // patterns, resolved before the absolute fallback runs, but
        // verify nothing regresses.
        let parsed = parse_release(&item("[Group] Cool Anime S3 - 05 (1080p)"));
        assert_eq!(parsed.season_hint, Some(3));
        assert!(
            parsed.season_relative_eps.contains(&5) || parsed.absolute_eps.contains(&5),
            "episode 5 should be resolved; got rel={:?} abs={:?}",
            parsed.season_relative_eps,
            parsed.absolute_eps
        );
    }

    #[test]
    fn nrd_season_marker_masked() {
        // "3rd Season" should not leak its "3" as an absolute episode.
        let parsed = parse_release(&item("[Group] Series 3rd Season (WEB 1080p)"));
        assert_eq!(parsed.season_hint, Some(3));
        assert!(parsed.absolute_eps.is_empty());
    }

    #[test]
    fn cour_marker_masked() {
        // "Cour 2" should not leak "2" to the absolute pass.
        let parsed = parse_release(&item("[Group] Series Cour 2 (WEB 1080p)"));
        assert!(parsed.absolute_eps.is_empty());
    }
}
