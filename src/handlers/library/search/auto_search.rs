//! Auto-search pipeline + auto-expand sibling-pack detection.
//!
//! These two flows are intertwined: `auto_expand_library_from_pack`
//! (called when a batch grab succeeds) routes per-file downloads to
//! sibling series, and on the way it kicks off a follow-up
//! `run_auto_search_targets_with_upgrades` for any sibling whose
//! cumulative-prior-episodes offset just got hydrated. The reverse
//! direction also exists: the auto-search inner loop calls
//! `auto_expand_library_from_pack` in a `tokio::spawn` after a
//! batch grab lands. Splitting them across two files would require
//! crossing the module boundary on every call edge, so they live
//! together.
//!
//! This module also owns the public auto-search HTTP handlers
//! (`auto_search_series`, `auto_search_episode`) and the per-handler
//! progress-event helpers (`display_title_for_progress`,
//! `emit_auto_search_terminal`).

use axum::{
    extract::{Path, Query, State},
    response::Json,
};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, monitoring, series};
use crate::services::{anilist, auto_search, logger, media, progress};

use super::super::reconcile::{maybe_hydrate_cumulative_offset, resolve_series_context};

/// Issue #102 — gate the per-target loop in
/// `run_auto_search_targets_with_upgrades` (and the parallel
/// per-series loop in `services::upgrade::run_once`) on whether the
/// caller's `series_id` still refers to a row in the `series` table.
/// Returns `true` when the loop should continue; `false` when the
/// caller should break and terminate the cascade.
///
/// `None` means the caller didn't bind the search to a tracked
/// series (typical for the search-before-add flow), so there's
/// nothing to cancel — return `true` and let the loop run.
pub(crate) async fn series_still_in_library(db: &SqlitePool, series_id: Option<i64>) -> bool {
    let Some(sid) = series_id else {
        return true;
    };
    series::get_by_id(db, sid).await.ok().flatten().is_some()
}

pub(super) fn batch_episode_numbers(title: &str, detail: &anilist::AnimeDetail) -> Vec<i32> {
    let mut ep_nums: Vec<i32> = auto_search::parse_release_numbers(title)
        .into_iter()
        .collect();
    if ep_nums.is_empty()
        && let Some(total) = detail.episodes
        && total > 0
        && total <= 1000
    {
        ep_nums = (1..=total).collect();
    }
    ep_nums.sort_unstable();
    ep_nums
}

// AutoExpandGrabContext + the core expansion logic live in
// `services::auto_expand` so `services::post_processing` can call the
// same routine as a fallback when the grab-time metadata wait here
// timed out. Re-export locally so call sites in this file stay terse.
use crate::services::auto_expand::{AutoExpandGrabContext, expand_from_files};

/// Grab-time outer orchestrator: wait for qBit metadata, then delegate
/// to [`services::auto_expand::expand_from_files`]. Failure here
/// (timeout, qBit error) is no longer load-bearing — post-processing
/// retries the same expansion at import time via
/// [`services::auto_expand::expand_from_files`], so a slow tracker that
/// can't deliver metadata in 3 minutes will still get sibling detection
/// once the torrent completes.
///
/// 180s ceiling (vs the 10s used by the interactive selective-narrowing
/// path) because this runs inside a `tokio::spawn` — blocking a few
/// minutes in the background is fine, the HTTP handler already
/// returned. A slow-DHT magnet or a public tracker under load can take
/// that long to fetch metadata.
#[allow(clippy::too_many_arguments)]
pub(super) async fn auto_expand_library_from_pack(
    db: &SqlitePool,
    client: std::sync::Arc<dyn crate::services::download_client::DownloadClient>,
    info_hash: &str,
    parent_detail: &anilist::AnimeDetail,
    parent_series_id: i64,
    parent_episode_numbers: &[i32],
    grab_id: i64,
    torrent_title: &str,
    grab_ctx: &AutoExpandGrabContext,
) -> usize {
    if parent_detail.id <= 0 || info_hash.is_empty() {
        return 0;
    }

    let files = match crate::services::download_client::wait_for_files(
        &*client,
        info_hash,
        std::time::Duration::from_secs(180),
    )
    .await
    {
        Ok(files) => files,
        Err(e) => {
            logger::info(
                db,
                LogCategory::Library,
                &format!(
                    "auto-expand: grab-time metadata wait failed for '{}', post-processing will retry at import time",
                    torrent_title
                ),
                &e,
            )
            .await;
            return 0;
        }
    };
    let filenames: Vec<String> = files.iter().map(|f| f.name.clone()).collect();

    expand_from_files(
        db,
        &filenames,
        parent_detail,
        parent_series_id,
        parent_episode_numbers,
        grab_id,
        torrent_title,
        grab_ctx,
    )
    .await
}

pub async fn run_auto_search_targets(
    state: &AppState,
    request_id: i64,
    targets: Vec<auto_search::SearchTarget>,
    allow_batch: bool,
    series_id: Option<i64>,
) -> Result<auto_search::AutoSearchReport, (axum::http::StatusCode, String)> {
    run_auto_search_targets_with_upgrades(
        state,
        request_id,
        targets,
        allow_batch,
        series_id,
        std::collections::HashMap::new(),
    )
    .await
}

/// Optional `?progress_id=<opaque>` query string the frontend appends to
/// auto-search trigger calls. The handler binds it to a fresh job in the
/// progress registry and the worker task emits stage events into it for
/// the sticky toast on the page.
#[derive(Deserialize, Default)]
pub struct AutoSearchQuery {
    pub progress_id: Option<String>,
}

/// Pick a user-facing title for progress toasts. Prefers the English
/// title, falling back to romaji — the same fallback the logger
/// already uses elsewhere in this handler.
pub(super) fn display_title_for_progress(detail: &anilist::AnimeDetail) -> &str {
    if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    }
}

/// Emit a terminal progress event summarizing the outcome of an
/// auto-search task. Called from inside the spawned task so the
/// `progress::EMITTER` task-local is in scope.
/// Issue #219 — the report's advisory notes go above the per-target
/// lines on the terminal toast, which is the one event that stays on
/// screen after the search ends.
fn with_notes(report: &auto_search::AutoSearchReport, lines: Vec<String>) -> String {
    report
        .notes
        .iter()
        .cloned()
        .chain(lines)
        .collect::<Vec<_>>()
        .join("\n")
}

async fn emit_auto_search_terminal(
    result: &Result<auto_search::AutoSearchReport, (axum::http::StatusCode, String)>,
) {
    match result {
        Ok(report) => {
            // PR #104 review: the cascade-stop path (issue #102 fix)
            // emits its own terminal "Auto-search cancelled" toast
            // before returning the partial report. Without this
            // short-circuit, the wrapper would emit ANOTHER terminal
            // event (`Nothing to search` / `Grabbed N`) that
            // immediately overwrites the cancel message in the UI.
            if report.cancelled {
                return;
            }
            let grabbed = report.grabbed.len();
            if grabbed > 0 {
                // Show titles for ≤3 grabs, otherwise just the count —
                // a 50-episode batch shouldn't paste a 50-line toast.
                let body = if grabbed <= 3 {
                    Some(
                        report
                            .grabbed
                            .iter()
                            .map(|h| format!("{}: {}", h.target_label, h.release_title))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                } else {
                    Some(format!("{} releases queued for download", grabbed))
                };
                progress::emit(
                    "done",
                    "success",
                    format!(
                        "Grabbed {} release{}",
                        grabbed,
                        if grabbed == 1 { "" } else { "s" }
                    ),
                    body,
                    true,
                )
                .await;
            } else if !report.skipped.is_empty() {
                progress::emit(
                    "done",
                    "warn",
                    "No releases grabbed",
                    Some(with_notes(report, report.skipped.clone())),
                    true,
                )
                .await;
            } else {
                progress::emit(
                    "done",
                    "warn",
                    "Nothing to search",
                    Some(with_notes(
                        report,
                        vec!["No targets matched the requested scope".to_string()],
                    )),
                    true,
                )
                .await;
            }
        }
        Err((_, msg)) => {
            progress::emit(
                "error",
                "error",
                "Auto search failed",
                Some(msg.clone()),
                true,
            )
            .await;
        }
    }
}

async fn run_auto_search_targets_with_upgrades(
    state: &AppState,
    request_id: i64,
    targets: Vec<auto_search::SearchTarget>,
    allow_batch: bool,
    series_id: Option<i64>,
    upgrade_classifications: std::collections::HashMap<
        i32,
        crate::services::source::ClassificationResult,
    >,
) -> Result<auto_search::AutoSearchReport, (axum::http::StatusCode, String)> {
    // Up-front configuration check — fail fast if NO client is
    // configured at all. The per-release dispatch below resolves the
    // *correct* client (torrent vs usenet) based on
    // `result.indexer_id` so an NZB grab from a SAB-pinned indexer
    // dispatches to SAB, not to qBit. A function-scope binding to
    // `default_download_client()` here would hard-route every grab
    // through the torrent default and silently mis-route NZBs into
    // qBit's `add_torrent` (which reports success on the URL fetch
    // but never produces an infohash → grab row lands with `hash=""`
    // → reconcile loop marks it removed after 30s).
    if state.default_download_client().await.is_none() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "Download client not configured".to_string(),
        ));
    }

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let title = if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Auto search started for {}", title),
        &format!("{} target(s), allow_batch={}", targets.len(), allow_batch),
    )
    .await;
    // Issue #219 — every Nyaa query for an adult title comes back
    // empty, and Phase 1.5's loosened aliases were what let an
    // unrelated release through. Say why in the log, and carry the
    // note on the terminal progress event: the sticky progress toast
    // shows one event at a time, so a mid-search warning would be
    // overwritten by the next "Searching" line before anyone saw it.
    let mut notes: Vec<String> = Vec::new();
    if detail.is_adult {
        let indexer_count = state.indexers.read().await.len();
        let no_indexer = crate::handlers::library::adult_needs_indexer(true, indexer_count);
        logger::warn(
            &state.db,
            LogCategory::AutoSearch,
            &format!("{} is marked adult", title),
            if no_indexer {
                "No indexer is configured, and Nyaa keeps adult releases on sukebei, which Ryokan does not search, so this search finds nothing."
            } else {
                "Nyaa keeps adult releases on sukebei, which Ryokan does not search. The configured indexers are asked for the adult category as well as anime."
            },
        )
        .await;
        if no_indexer {
            notes.push(
                "This title is marked adult and no indexer is configured. Nyaa keeps adult releases on sukebei, which Ryokan does not search."
                    .to_string(),
            );
        }
    }
    progress::emit(
        "search",
        "info",
        format!("Searching {}", title),
        Some(format!(
            "{} target{}",
            targets.len(),
            if targets.len() == 1 { "" } else { "s" }
        )),
        false,
    )
    .await;

    // Clone the compiled-CF Arc out from under the read lock so the
    // scoring loop below runs without holding it.
    let cfs = state.custom_formats.read().await.clone();

    let mut grabbed = Vec::new();
    let mut skipped = Vec::new();
    let total_targets = targets.len();
    for (idx, target) in targets.into_iter().enumerate() {
        // Issue #102 — auto-search of a "monitor all" series queues a
        // per-episode loop. If the user removes the series mid-loop
        // (typical stress-test discovery: notice the wrong show was
        // added, delete it from the library), the loop used to keep
        // grabbing episodes anyway because the targets vec was already
        // materialized. Re-check existence at the top of each iteration
        // so a removal stops the cascade promptly. Only applies when
        // the caller bound a series_id; the search-before-add flow
        // (series_id == None) is unaffected.
        if !series_still_in_library(&state.db, series_id).await {
            logger::info(
                &state.db,
                LogCategory::AutoSearch,
                &format!(
                    "Auto search cancelled for {}: series removed from library",
                    title
                ),
                &format!(
                    "{} of {} target(s) processed before cancel",
                    idx, total_targets
                ),
            )
            .await;
            progress::emit(
                "done",
                "warn",
                "Auto-search cancelled",
                Some("Series was removed from the library".to_string()),
                true,
            )
            .await;
            return Ok(auto_search::AutoSearchReport {
                notes: notes.clone(),
                grabbed,
                skipped,
                quality_profile: cfg.quality_profile,
                cancelled: true,
            });
        }
        let label = auto_search::target_label(&target);
        let is_upgrade = matches!(&target, auto_search::SearchTarget::Episode(n) if upgrade_classifications.contains_key(n));
        progress::emit(
            "search",
            "info",
            if total_targets > 1 {
                format!("[{}/{}] {}", idx + 1, total_targets, label)
            } else {
                format!("Searching: {}", label)
            },
            None,
            false,
        )
        .await;
        match auto_search::find_best_for_target(
            &state.db,
            &detail,
            &cfg,
            &target,
            allow_batch,
            is_upgrade,
            &cfs,
            &state.indexers,
        )
        .await
        {
            Some(result) => {
                // Classify up front so both upgrade-verification and log labels
                // read the same result.
                let incoming_classification = crate::services::source::classify_release(
                    &state.db,
                    &result.title,
                    Some(&result.resolution),
                    Some(crate::services::source::NyaaContext {
                        info_hash: &result.info_hash,
                        view_url: &result.link,
                        is_batch: result.is_batch,
                    }),
                    Some(crate::services::source::SeriesContext {
                        status: &detail.status,
                        season_year: detail.season_year,
                        end_year: detail.end_year,
                    }),
                )
                .await;

                // For upgrade targets, verify the found release is actually
                // better quality than what's already on disk.
                if let auto_search::SearchTarget::Episode(ep_num) = &target
                    && let Some(existing) = upgrade_classifications.get(ep_num)
                {
                    if incoming_classification.rank() <= existing.rank() {
                        logger::debug(
                            &state.db,
                            LogCategory::AutoSearch,
                            &format!(
                                "{}: skipped upgrade (incoming {} not better than existing {})",
                                label,
                                incoming_classification.label(),
                                existing.label()
                            ),
                            &result.title,
                        )
                        .await;
                        skipped.push(format!("{}: no quality upgrade available", label));
                        continue;
                    }
                    logger::info(
                        &state.db,
                        LogCategory::AutoSearch,
                        &format!(
                            "{}: upgrading from {} to {}",
                            label,
                            existing.label(),
                            incoming_classification.label()
                        ),
                        &result.title,
                    )
                    .await;
                }
                // For selective downloads, prefer the `.torrent` URL
                // over the magnet: qBit can parse metadata straight
                // from the file instead of waiting on DHT/trackers.
                let wants_selective = !result.info_hash.is_empty()
                    && auto_search::has_selective_discriminator(&detail);
                let url = if wants_selective && !result.torrent.is_empty() {
                    result.torrent.clone()
                } else if !result.magnet.is_empty() {
                    result.magnet.clone()
                } else {
                    result.torrent.clone()
                };
                // Per-release client routing: the indexer the release
                // came from carries a `download_client_id` pin in
                // multi-client setups. Resolve through
                // `client_for_indexer_with_id` so a torznab indexer
                // pinned to qBit dispatches to qBit and a newznab
                // indexer pinned to SAB dispatches to SAB. Without
                // this, NZB grabs from a SAB-pinned indexer routed
                // through `default_download_client()` (the *torrent*
                // default) and silently mis-fired into qBit. Returns
                // the resolved client + the persisted dispatch id so
                // the grab row can stamp `download_client_id` for
                // delete-routing later.
                let (qbit, dispatch_client_id) =
                    match state.client_for_indexer_with_id(result.indexer_id).await {
                        Some(pair) => pair,
                        None => {
                            logger::warn(
                            &state.db,
                            LogCategory::AutoSearch,
                            &format!(
                                "{}: indexer pin resolved to no client (or no client configured)",
                                label
                            ),
                            &result.title,
                        )
                        .await;
                            skipped.push(format!("{}: no download client for indexer", label));
                            continue;
                        }
                    };
                if url.is_empty() {
                    logger::warn(
                        &state.db,
                        LogCategory::AutoSearch,
                        &format!("{}: no magnet/torrent URL", label),
                        &result.title,
                    )
                    .await;
                    skipped.push(format!("{}: no magnet/torrent URL", label));
                    continue;
                }
                // Selective-file path for multi-part / multi-season
                // packs. `pick_wanted_file_indices` narrows by part
                // number (Kizumonogatari II in a Monogatari megapack)
                // or positive subtitle match (Stardust Crusaders in a
                // JoJo S1–S5 pack). The gate only runs this branch
                // when the detail has an actual discriminator to try,
                // so single-entry series fall through to the plain
                // add. Franchise roots without their own subtitle
                // (JoJo S1) also fall through — they're handled by
                // the multi-series auto-expand path below, which
                // downloads the full pack and routes each sibling's
                // files to its own library entry. On filter error,
                // fall back to a full add rather than dropping the
                // grab entirely.
                // `effective_hash` is the id Ryokan persists on
                // `grabbed_torrents.hash` — same rationale as the interactive
                // grab handler in handlers/library/search/grab.rs. BT clients
                // echo the precomputed info_hash via `add_torrent_returning_id`'s
                // default impl; SAB returns its `nzo_id`, which is the only
                // id SAB lets you key queue/history ops by. Without this
                // capture, SAB-routed grabs land with `hash=""` and the
                // post-processing reconcile loop marks them removed after
                // 30s when `list_scoped` can't match them.
                let selective_outcome: Result<(Option<Vec<usize>>, String), String> =
                    if wants_selective {
                        let detail_clone = detail.clone();
                        let info_hash_clone = result.info_hash.clone();
                        let mut pick = move |files: &[String]| {
                            auto_search::pick_wanted_file_indices(files, &detail_clone)
                        };
                        match qbit
                            .add_torrent_with_file_filter(&url, &info_hash_clone, &mut pick)
                            .await
                        {
                            // Selective branch echoes the precomputed
                            // info_hash since `add_torrent_with_file_filter`
                            // doesn't return an id. SAB grabs don't fire
                            // this branch in practice (require info_hash
                            // non-empty, which NZBs lack).
                            Ok(crate::services::download_client::SelectiveOutcome::Filtered(
                                kept,
                            )) => Ok((Some(kept), result.info_hash.clone())),
                            Ok(
                                crate::services::download_client::SelectiveOutcome::FullDownload,
                            ) => Ok((None, result.info_hash.clone())),
                            Err(e) => {
                                logger::warn(
                                    &state.db,
                                    LogCategory::Grab,
                                    &format!(
                                        "{}: selective download failed, falling back to full grab",
                                        label
                                    ),
                                    &e,
                                )
                                .await;
                                qbit.add_torrent_returning_id(&url, &result.info_hash)
                                    .await
                                    .map(|(_, hash)| (None, hash))
                            }
                        }
                    } else {
                        qbit.add_torrent_returning_id(&url, &result.info_hash)
                            .await
                            .map(|(_, hash)| (None, hash))
                    };
                match selective_outcome {
                    Ok((kept, effective_hash)) => {
                        let selective_suffix = match (&kept, wants_selective) {
                            (Some(ids), _) => format!(", selective={}", ids.len()),
                            (None, true) => ", selective=full(timeout)".to_string(),
                            (None, false) => String::new(),
                        };
                        logger::info(
                            &state.db,
                            LogCategory::Grab,
                            &format!("Grabbed: {}", result.title),
                            &format!(
                                "target={}, group={}, score={}, tier={}, batch={}{}{}",
                                label,
                                result.group,
                                result.score,
                                incoming_classification.label(),
                                result.is_batch,
                                selective_suffix,
                                crate::services::auto_search::MatchProvenance::log_suffix(
                                    result.match_provenance.as_ref()
                                )
                            ),
                        )
                        .await;
                        progress::emit(
                            "grab",
                            "success",
                            format!("Grabbed: {}", label),
                            Some(format!(
                                "{} [{}]",
                                result.title,
                                incoming_classification.label()
                            )),
                            false,
                        )
                        .await;
                        // Record for post-processing and episode quality tags.
                        if let Some(sid) = series_id {
                            let mut ep_nums: Vec<i32> = match &target {
                                auto_search::SearchTarget::Episode(n) => vec![*n],
                                auto_search::SearchTarget::Single => vec![1],
                            };
                            // For batch releases, parse all episode numbers from
                            // the title so every covered episode gets a grab tag.
                            if result.is_batch {
                                let parsed = auto_search::parse_release_numbers(&result.title);
                                if !parsed.is_empty() {
                                    ep_nums = parsed.into_iter().collect();
                                    ep_nums.sort_unstable();
                                }
                            }
                            // `record_grab` returns `None` on the
                            // empty-hash + FK-violation anomaly path
                            // documented on the model, or on a DB
                            // error. The torrent is already in the
                            // client at this point, so don't unwind —
                            // skip the seed-rule + attribution stamp
                            // and let the next reconcile pick it up.
                            let grab_id = crate::models::grabbed_torrents::record_grab(
                                &state.db,
                                &effective_hash,
                                &result.title,
                                sid,
                                &ep_nums,
                                result.is_batch,
                            )
                            .await
                            .ok()
                            .flatten();
                            // Misgrab guardrails: keep the URL so Restore can re-add a removed grab.
                            if let Some(gid) = grab_id {
                                let _ = crate::models::grabbed_torrents::set_source_url(
                                    &state.db, gid, &url,
                                )
                                .await;
                            }
                            // Issue #28 — apply per-indexer
                            // seed rules + stamp attribution.
                            // Nyaa grabs (indexer_id None) take the
                            // existing path (no seed-rule call,
                            // respect_seed_rules stays 0).
                            if let Some(gid) = grab_id {
                                let respected =
                                    crate::services::download_client::apply_indexer_seed_rules(
                                        &state.db,
                                        &*qbit,
                                        &effective_hash,
                                        result.indexer_id,
                                    )
                                    .await;
                                let _ = crate::models::grabbed_torrents::set_indexer_attribution(
                                    &state.db,
                                    gid,
                                    result.indexer_id,
                                    respected,
                                )
                                .await;
                                // Stamp the resolved download_client_id
                                // on the grab row so the per-grab
                                // routing in `delete_episode_file` /
                                // `cancel_pending_episode` (via
                                // `state.resolve_grab_client`) sends
                                // the eventual delete to the SAME
                                // client that received the add.
                                // Mirrors the manual grab handler in
                                // `handlers/library/search/grab.rs`.
                                let _ = crate::models::grabbed_torrents::set_download_client(
                                    &state.db,
                                    gid,
                                    Some(dispatch_client_id),
                                )
                                .await;
                                // Issue #118 — fire `Grabbed`. The
                                // auto_search path has full context:
                                // matched indexer (Nyaa-direct = None),
                                // total CF score from the scoring pass,
                                // dispatch client kind. This is the
                                // most-instrumented call site on the
                                // event taxonomy.
                                let indexer = crate::services::notifications::resolve_indexer_name(
                                    state,
                                    result.indexer_id,
                                )
                                .await;
                                crate::services::notifications::emit_grabbed(
                                    state,
                                    sid,
                                    ep_nums.first().copied().unwrap_or(0),
                                    &result.title,
                                    indexer,
                                    Some(result.score),
                                    Some(qbit.sonarr_impl_name().to_string()),
                                )
                                .await;
                            }
                            for ep_num in &ep_nums {
                                let _ = episode_tags::record_grab_with_match(
                                    &state.db,
                                    sid,
                                    *ep_num,
                                    &incoming_classification,
                                    &result.title,
                                    &result.group,
                                    result.size_bytes,
                                    result.is_batch,
                                    result.match_provenance.as_ref(),
                                )
                                .await;
                            }
                            // Phase 2 sibling auto-expand: when the
                            // grab is a batch covering a franchise
                            // (e.g. JoJo S1-S5), detect sibling
                            // entries in the file list and add them
                            // to the library so post-processing can
                            // route each file to the correct series.
                            // Only runs on fresh grabs (existing
                            // grab_id skips the route-write path).
                            //
                            // Skip auto-expand when selective narrowing
                            // successfully applied — the user is
                            // explicitly targeting one sibling inside a
                            // megapack (e.g. Stardust Crusaders in a
                            // JoJo pack), so the other siblings' files
                            // are marked priority=0 in qBit and will
                            // never land. Creating ghost library rows
                            // for them would leave dangling entries
                            // with no imported files. The
                            // `kept.is_none()` fallback path
                            // (selective filter timed out → full
                            // download) still auto-expands because the
                            // whole pack is actually downloading.
                            let selective_narrowed = wants_selective && kept.is_some();
                            if result.is_batch
                                && !selective_narrowed
                                && let Some(grab_id) = grab_id
                            {
                                // Fire-and-forget so the HTTP handler
                                // doesn't block up to ~60s waiting on
                                // the client to discover metadata (see
                                // the `wait_for_files` call inside
                                // `auto_expand_library_from_pack`).
                                // Failures here only affect post-
                                // processing routing, which already
                                // falls back to the parent series.
                                let db_task = state.db.clone();
                                let qbit_task = qbit.clone();
                                let info_hash_task = effective_hash.clone();
                                let detail_task = detail.clone();
                                let ep_nums_task = ep_nums.clone();
                                let title_task = result.title.clone();
                                let grab_ctx_task = AutoExpandGrabContext {
                                    classification: incoming_classification.clone(),
                                    release_group: result.group.clone(),
                                    size_bytes: result.size_bytes,
                                };
                                tokio::spawn(async move {
                                    auto_expand_library_from_pack(
                                        &db_task,
                                        qbit_task,
                                        &info_hash_task,
                                        &detail_task,
                                        sid,
                                        &ep_nums_task,
                                        grab_id,
                                        &title_task,
                                        &grab_ctx_task,
                                    )
                                    .await;
                                });
                            }
                        }
                    }
                    Err(e) => {
                        logger::error(
                            &state.db,
                            LogCategory::DownloadClient,
                            &format!("Failed to add torrent for {}", label),
                            &e,
                        )
                        .await;
                        return Err((axum::http::StatusCode::BAD_GATEWAY, e));
                    }
                }
                let queued_batch = result.is_batch;
                grabbed.push(auto_search::AutoSearchHit {
                    target_label: label.clone(),
                    release_title: result.title,
                    release_group: result.group,
                    quality_tier: incoming_classification.label(),
                    url,
                    score: result.score,
                });
                if queued_batch && allow_batch {
                    logger::info(
                        &state.db,
                        LogCategory::AutoSearch,
                        "Season pack queued; stopping episode search",
                        "",
                    )
                    .await;
                    skipped.push(
                        "Season pack queued; skipped additional episode searches".to_string(),
                    );
                    break;
                }
            }
            None => {
                logger::debug(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!("{}: no matching release found", label),
                    "",
                )
                .await;
                skipped.push(format!("{}: no matching release found", label));
            }
        }
    }

    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        &format!(
            "Auto search complete: {} grabbed, {} skipped",
            grabbed.len(),
            skipped.len()
        ),
        &format!("profile={}", cfg.quality_profile),
    )
    .await;

    Ok(auto_search::AutoSearchReport {
        notes,
        grabbed,
        skipped,
        quality_profile: cfg.quality_profile,
        cancelled: false,
    })
}

#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/auto-search",
    tag = "Library",
    summary = "Auto-search all episodes",
    description = "Automatically search and grab the best release for every monitored episode of a series.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Auto-search report", body = auto_search::AutoSearchReport),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn auto_search_series(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Query(q): Query<AutoSearchQuery>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let progress_handle = match progress::sanitize_progress_id(q.progress_id.as_deref()) {
        Some(id) => Some(state.progress.register(id).await),
        None => None,
    };
    if let Some(h) = &progress_handle {
        h.emit("start", "info", "Preparing auto-search…", None, false)
            .await;
    }
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let (tracked_row, provider_id, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let tracked = if let Some(row) = tracked_row {
        Some(row)
    } else if let Some(mid) = detail.id_mal {
        series::get_by_mal_id(&state.db, mid)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        series::get_by_anilist_id(&state.db, provider_id)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let tracked = maybe_hydrate_cumulative_offset(&state.db, tracked, &detail).await;
    let folder_name = tracked
        .as_ref()
        .map(|s| s.folder_name.clone())
        .unwrap_or_default();
    let existing_files = media::scan_series_folder(&cfg.media_root, &folder_name).await;
    let existing_eps: Vec<i32> = existing_files.iter().map(|f| f.episode_number).collect();

    let monitored_eps = if let Some(ref tracked_series) = tracked {
        monitoring::get_monitored_episode_numbers(&state.db, tracked_series.id)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        Vec::new()
    };

    let mut targets = if tracked.is_some() {
        auto_search::build_monitored_targets(&detail, &existing_eps, &monitored_eps)
    } else {
        auto_search::build_missing_targets(&detail, &existing_eps)
    };

    // Also include upgrade targets: episodes on disk below the quality cutoff.
    let (cutoff_source, cutoff_is_remux, cutoff_is_bdmv) =
        crate::services::source::parse_cutoff_source(&cfg.cutoff_source);
    let cutoff_resolution = crate::services::source::Resolution::from_str(&cfg.cutoff_resolution);
    let quality_tags = if let Some(ref t) = tracked {
        episode_tags::get_for_series(&state.db, t.id)
            .await
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    let upgrade_targets = auto_search::build_upgrade_targets(
        &existing_files,
        &monitored_eps,
        cutoff_source,
        cutoff_resolution,
        cutoff_is_remux,
        cutoff_is_bdmv,
        &quality_tags,
    );
    // Merge upgrade targets (avoid duplicates with missing targets).
    let existing_target_eps: std::collections::HashSet<i32> = targets
        .iter()
        .filter_map(|t| match t {
            auto_search::SearchTarget::Episode(n) => Some(*n),
            _ => None,
        })
        .collect();
    for (target, _) in &upgrade_targets {
        if let auto_search::SearchTarget::Episode(n) = target
            && !existing_target_eps.contains(n)
        {
            targets.push(target.clone());
        }
    }

    let target_summary = if targets.len() <= 5 {
        targets
            .iter()
            .map(auto_search::target_label)
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!("{} targets", targets.len())
    };
    let upgrade_count = upgrade_targets.len();
    let title_for_log = if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    logger::debug(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Missing targets for {}: {}", title_for_log, target_summary),
        &format!(
            "on_disk={}, monitored={}, upgradeable={}, total={:?}",
            existing_eps.len(),
            monitored_eps.len(),
            upgrade_count,
            detail.episodes
        ),
    )
    .await;
    let series_id_for_grab = tracked.as_ref().map(|s| s.id);
    // Build a map of existing episode classifications for upgrade verification in the search task.
    let upgrade_classifications: std::collections::HashMap<
        i32,
        crate::services::source::ClassificationResult,
    > = upgrade_targets
        .into_iter()
        .filter_map(|(t, classification)| match t {
            auto_search::SearchTarget::Episode(n) => Some((n, classification)),
            _ => None,
        })
        .collect();
    // Spawn as an independent task so the grab completes even if the client
    // disconnects. The spawned future is wrapped in `progress::scope` when a
    // progress handle was registered, so deep callees inside the search
    // pipeline can `progress::emit` into the toast without threading the
    // handle through every signature.
    let state_clone = state.clone();
    let progress_for_task = progress_handle.clone();
    let handle = tokio::spawn(progress::run_with_progress(progress_for_task, async move {
        let result = run_auto_search_targets_with_upgrades(
            &state_clone,
            request_id,
            targets,
            true,
            series_id_for_grab,
            upgrade_classifications,
        )
        .await;
        emit_auto_search_terminal(&result).await;
        result
    }));
    let report = handle.await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Search task failed: {}", e),
        )
    })??;
    Ok(Json(report))
}

#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/auto-search/{episode_number}",
    tag = "Library",
    summary = "Auto-search single episode",
    description = "Automatically search and grab the best release for a specific episode.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "Auto-search report", body = auto_search::AutoSearchReport),
        (status = 400, description = "Invalid episode for media type"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn auto_search_episode(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Query(q): Query<AutoSearchQuery>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let progress_handle = match progress::sanitize_progress_id(q.progress_id.as_deref()) {
        Some(id) => Some(state.progress.register(id).await),
        None => None,
    };
    if let Some(h) = &progress_handle {
        h.emit(
            "start",
            "info",
            format!("Searching episode {}…", episode_number),
            None,
            false,
        )
        .await;
    }
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id_for_grab: Option<i64> = tracked_row.as_ref().map(|s| s.id);

    if let Some(_tracked) = tracked_row {
        // Monitoring status does not block manual episode searches.
    } else if matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA")
        && episode_number != 1
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "Single-entry media can only search episode 1".to_string(),
        ));
    }

    logger::debug(
        &state.db,
        LogCategory::AutoSearch,
        &format!(
            "Episode search: series_ref={}, episode={}",
            request_id, episode_number
        ),
        "allow_batch=false",
    )
    .await;
    // Collapse to Single for single-entry media so movie/OVA/special
    // release titles (which don't carry episode numbers) aren't filtered
    // out by the Episode(n) matching rules.
    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);

    // Spawn as an independent task so the grab completes even if the client
    // disconnects. The spawn is wrapped in `progress::run_with_progress`
    // when a progress handle was registered above so deep callees can emit
    // into the user's sticky toast without threading the handle down.
    let state_clone = state.clone();
    let progress_for_task = progress_handle.clone();
    let handle = tokio::spawn(progress::run_with_progress(progress_for_task, async move {
        let result = run_auto_search_targets(
            &state_clone,
            request_id,
            vec![target],
            false,
            series_id_for_grab,
        )
        .await;
        emit_auto_search_terminal(&result).await;
        result
    }));
    let report = handle.await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Search task failed: {}", e),
        )
    })??;
    Ok(Json(report))
}
