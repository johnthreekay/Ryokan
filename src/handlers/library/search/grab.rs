//! Grab handlers for interactive + batch search results. Reach into
//! the auto-expand pipeline (`auto_expand_library_from_pack`) when a
//! batch grab lands so per-file routing works the same as the
//! auto-search batch path.

use axum::{
    extract::{Path, State},
    response::Json,
};

use crate::AppState;
use crate::models::episode_tags;
use crate::models::log::LogCategory;
use crate::services::{auto_expand::AutoExpandGrabContext, auto_search, logger};

use super::super::reconcile::resolve_series_context;
use super::auto_search::{auto_expand_library_from_pack, batch_episode_numbers};

/// Grab a specific batch release chosen from interactive batch search.
///
/// Mirrors `grab_interactive_result` but without an episode number —
/// batches cover a range of episodes — the episode list is resolved
/// from the release title via [`batch_episode_numbers`] at grab time
/// so per-episode `episode_tags::record_grab` writes land immediately
/// and the UI shows the batch's quality tier without waiting on
/// post-processing.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/grab-batch",
    tag = "Library",
    summary = "Grab a specific batch release",
    description = "Send a specific batch torrent release (chosen from interactive batch search) to qBittorrent for download.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Batch grabbed", body = serde_json::Value),
        (status = 400, description = "No URL provided or qBittorrent not configured"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn grab_batch_result(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row.as_ref().map(|s| s.id);
    let url = body["url"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let group = body["group"].as_str().unwrap_or("").to_string();
    let resolution = body["resolution"].as_str().unwrap_or("").to_string();
    let info_hash = body["info_hash"].as_str().unwrap_or("").to_string();
    let size_bytes = body["size_bytes"].as_i64().unwrap_or(0);
    // Multi-client routing — the search-result row carries `indexer_id`
    // (None for Nyaa-direct, Some for torznab/newznab fan-out). The
    // frontend round-trips it on grab so the dispatch routes through
    // the indexer's pin (or Nyaa pin for Nyaa-direct).
    let indexer_id: Option<i64> = body["indexer_id"].as_i64();

    if url.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "No URL provided".to_string(),
        ));
    }

    let resolved_client = if indexer_id.is_some() {
        state.client_for_indexer_with_id(indexer_id).await
    } else {
        let cfg = crate::models::config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        state
            .client_for_nyaa_with_id(cfg.nyaa_download_client_id)
            .await
    };
    let (qbit, dispatch_client_id) = resolved_client.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;

    // Same selective-file path as `grab_interactive_result`: narrow
    // a megapack to just the target if it has its own subtitle or
    // part number. Franchise roots (JoJo S1) deliberately fall
    // through so the multi-series auto-expand path below can route
    // each sibling's files into its own library entry instead.
    let wants_selective =
        !info_hash.is_empty() && auto_search::has_selective_discriminator(&detail);
    // `effective_hash` is the id Ryokan persists on `grabbed_torrents.hash`
    // and uses to reconcile the grab against the client's later state. For
    // BT clients it equals the precomputed v1 infohash from the magnet/.torrent
    // URL (default `add_torrent_returning_id` impl echoes it back). For SAB it
    // becomes the `nzo_id` returned from `mode=addurl`, which is the only id
    // SAB lets you key queue/history ops by — without this capture, SAB grabs
    // got `hash=""` persisted, the episode-progress poller never matched them
    // in `list_scoped`, and after 30s the reconcile loop marked them removed.
    let (selective_outcome, effective_hash): (Option<Vec<usize>>, String) = if wants_selective {
        let detail_clone = detail.clone();
        let mut pick =
            move |files: &[String]| auto_search::pick_wanted_file_indices(files, &detail_clone);
        match qbit
            .add_torrent_with_file_filter(&url, &info_hash, &mut pick)
            .await
        {
            // Selective branch echoes the precomputed info_hash since
            // `add_torrent_with_file_filter` doesn't return an id. SAB's
            // file-filter impl no-ops the filter and returns FullDownload,
            // so SAB grabs that route through here keep the v1 picker-path
            // limitation (documented in services/download_client/sabnzbd/mod.rs).
            // Practically rare — SAB grabs require info_hash to be empty
            // (NZBs have no infohash), and `wants_selective` requires
            // info_hash to be non-empty, so this branch shouldn't fire for
            // SAB in the first place.
            Ok(crate::services::download_client::SelectiveOutcome::Filtered(kept)) => {
                (Some(kept), info_hash.clone())
            }
            Ok(crate::services::download_client::SelectiveOutcome::FullDownload) => {
                (None, info_hash.clone())
            }
            Err(e) => {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Selective batch download failed, falling back to full grab: {}",
                        title
                    ),
                    &e,
                )
                .await;
                let (_, hash) = qbit
                    .add_torrent_returning_id(&url, &info_hash)
                    .await
                    .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
                (None, hash)
            }
        }
    } else {
        let (_, hash) = qbit
            .add_torrent_returning_id(&url, &info_hash)
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
        (None, hash)
    };

    // Classify so the log line carries the actual quality tier. Pass the
    // chosen release as a batch in NyaaContext (Layer 4 uses this for the
    // finished-series BluRay rule).
    let classification = crate::services::source::classify_release(
        &state.db,
        &title,
        Some(&resolution),
        Some(crate::services::source::NyaaContext {
            info_hash: &info_hash,
            view_url: "",
            is_batch: true,
        }),
        Some(crate::services::source::SeriesContext {
            status: &detail.status,
            season_year: detail.season_year,
            end_year: detail.end_year,
        }),
    )
    .await;

    let selective_suffix = match (&selective_outcome, wants_selective) {
        (Some(kept), _) => format!(", selective={}", kept.len()),
        (None, true) => ", selective=full(timeout)".to_string(),
        (None, false) => String::new(),
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!("Grabbed batch (interactive): {}", title),
        &format!(
            "group={}, tier={}{}",
            group,
            classification.label(),
            selective_suffix
        ),
    )
    .await;

    if let Some(sid) = series_id {
        // Parse episode list from the batch title so every covered
        // episode gets a per-episode `episode_quality_tags` row at
        // grab time. Same reasoning as in `search_batch_releases` —
        // without this, batch grabs leave every episode showing
        // UNKNOWN in the UI, and with post-processing disabled the
        // rows never get created at all.
        let ep_nums = batch_episode_numbers(&title, &detail);
        let grab_id = crate::models::grabbed_torrents::record_grab(
            &state.db,
            &effective_hash,
            &title,
            sid,
            &ep_nums,
            true,
        )
        .await
        .ok()
        .flatten();
        if let Some(gid) = grab_id {
            let _ = crate::models::grabbed_torrents::set_download_client(
                &state.db,
                gid,
                Some(dispatch_client_id),
            )
            .await;
            // Issue #118 — fire `Grabbed` for the interactive batch
            // path. Episode number is the lowest in the parsed range
            // (matches the existing single-i32 contract on the event).
            let indexer =
                crate::services::notifications::resolve_indexer_name(&state, indexer_id).await;
            crate::services::notifications::emit_grabbed(
                &state,
                sid,
                ep_nums.first().copied().unwrap_or(0),
                &title,
                indexer,
                None,
                Some(qbit.sonarr_impl_name().to_string()),
            )
            .await;
        }
        for ep_num in &ep_nums {
            let _ = episode_tags::record_grab(
                &state.db,
                sid,
                *ep_num,
                &classification,
                &title,
                &group,
                size_bytes,
                true,
            )
            .await;
        }
        // Phase 2 sibling auto-expand. Skip when selective narrowing
        // successfully applied — the user picked a specific sibling
        // (e.g. Stardust Crusaders) out of a megapack and the other
        // siblings' files are marked priority=0 in qBit and won't
        // land. Creating library entries for them would leave ghost
        // rows with no imported files. The `selective_outcome.is_none()
        // && wants_selective` fallback (filter timed out → full
        // download) still auto-expands because the whole pack is
        // actually downloading.
        let selective_narrowed = wants_selective && selective_outcome.is_some();
        if !selective_narrowed && let Some(grab_id) = grab_id {
            // Fire-and-forget so the HTTP handler doesn't block
            // up to ~60s on qBit metadata discovery. See the
            // matching spawn in `run_auto_search_targets_with_upgrades`.
            let db_task = state.db.clone();
            let qbit_task = qbit.clone();
            // `effective_hash` (the SAB nzo_id or the BT infohash)
            // is what `get_files` keys off in the auto-expand path
            // — passing the original BT-shape `info_hash` would be
            // empty for SAB grabs and miss the file list entirely.
            let info_hash_task = effective_hash.clone();
            let detail_task = detail.clone();
            let title_task = title.clone();
            let ep_nums_task = ep_nums.clone();
            let grab_ctx_task = AutoExpandGrabContext {
                classification: classification.clone(),
                release_group: group.clone(),
                size_bytes,
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

    Ok(Json(serde_json::json!({
        "ok": true,
        "title": title,
        "tier": classification.label(),
        "selective_files": selective_outcome,
    })))
}

/// Grab a specific release chosen from the interactive search.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/grab/{episode_number}",
    tag = "Library",
    summary = "Grab a specific release",
    description = "Send a specific torrent release (chosen from interactive search) to qBittorrent for download.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Release grabbed", body = serde_json::Value),
        (status = 400, description = "No URL provided or qBittorrent not configured"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn grab_interactive_result(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row.as_ref().map(|s| s.id);
    let url = body["url"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let group = body["group"].as_str().unwrap_or("").to_string();
    let resolution = body["resolution"].as_str().unwrap_or("").to_string();
    let info_hash = body["info_hash"].as_str().unwrap_or("").to_string();
    let size_bytes = body["size_bytes"].as_i64().unwrap_or(0);
    // Multi-client routing — the search-result row carries `indexer_id`
    // (None for Nyaa-direct, Some for torznab/newznab fan-out). The
    // frontend round-trips it on grab so the dispatch routes through
    // the indexer's pin (or Nyaa pin for Nyaa-direct).
    let indexer_id: Option<i64> = body["indexer_id"].as_i64();
    // Misgrab guardrails: the interactive table round-trips the match
    // provenance the search stamped on the row so the grab history and
    // the log say how this release matched. Absent or malformed means
    // an older page or a hand-built request; the grab still proceeds.
    let match_provenance: Option<auto_search::MatchProvenance> =
        serde_json::from_value(body["match_provenance"].clone()).ok();

    if url.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "No URL provided".to_string(),
        ));
    }

    let resolved_client = if indexer_id.is_some() {
        state.client_for_indexer_with_id(indexer_id).await
    } else {
        let cfg = crate::models::config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        state
            .client_for_nyaa_with_id(cfg.nyaa_download_client_id)
            .await
    };
    let (qbit, dispatch_client_id) = resolved_client.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;

    // If the target is a multi-part entry ("Kizumonogatari II") OR a
    // subtitled season of a franchise ("Stardust Crusaders"), try the
    // selective-file download path so a megapack release only pulls
    // the files the user is tracking. Franchise roots without their
    // own subtitle return `false` here and fall through to the plain
    // `add_torrent` path — interactive single-episode grabs don't
    // auto-expand the library (that's `grab_batch_result`'s job).
    let wants_selective =
        !info_hash.is_empty() && auto_search::has_selective_discriminator(&detail);
    // See `grab_batch_result` above for why `add_torrent_returning_id`
    // matters. Same SAB-nzo_id capture rationale applies to the
    // single-episode interactive grab path.
    let (selective_outcome, effective_hash): (Option<Vec<usize>>, String) = if wants_selective {
        let detail_clone = detail.clone();
        let mut pick =
            move |files: &[String]| auto_search::pick_wanted_file_indices(files, &detail_clone);
        match qbit
            .add_torrent_with_file_filter(&url, &info_hash, &mut pick)
            .await
        {
            Ok(crate::services::download_client::SelectiveOutcome::Filtered(kept)) => {
                (Some(kept), info_hash.clone())
            }
            Ok(crate::services::download_client::SelectiveOutcome::FullDownload) => {
                (None, info_hash.clone())
            }
            Err(e) => {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Selective download failed, falling back to full grab: {}",
                        title
                    ),
                    &e,
                )
                .await;
                let (_, hash) = qbit
                    .add_torrent_returning_id(&url, &info_hash)
                    .await
                    .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
                (None, hash)
            }
        }
    } else {
        let (_, hash) = qbit
            .add_torrent_returning_id(&url, &info_hash)
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
        (None, hash)
    };

    // Interactive grab: the frontend doesn't currently round-trip the Nyaa
    // view URL, so Layer 2 is skipped here. These grabs are user-initiated
    // and rarely land on the ambiguous tail that Layer 2 targets anyway.
    // Layer 4 still runs when we have a tracked series — it's a pure
    // function with no round-trip cost.
    let series_ctx = tracked_row
        .as_ref()
        .map(|s| crate::services::source::SeriesContext {
            status: &s.status,
            season_year: s.season_year,
            end_year: s.end_year,
        });
    let classification = crate::services::source::classify_release(
        &state.db,
        &title,
        Some(&resolution),
        None,
        series_ctx,
    )
    .await;
    let selective_suffix = match (&selective_outcome, wants_selective) {
        (Some(kept), _) => format!(", selective={}", kept.len()),
        (None, true) => ", selective=full(timeout)".to_string(),
        (None, false) => String::new(),
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!("Interactive grab: {}", title),
        &format!(
            "episode={}, group={}, tier={}{}{}",
            episode_number,
            group,
            classification.label(),
            selective_suffix,
            auto_search::MatchProvenance::log_suffix(match_provenance.as_ref())
        ),
    )
    .await;

    if let Some(sid) = series_id {
        // Interactive single-episode grab — not a batch by definition.
        let grab_id = crate::models::grabbed_torrents::record_grab(
            &state.db,
            &effective_hash,
            &title,
            sid,
            &[episode_number],
            false,
        )
        .await
        .ok()
        .flatten();
        if let Some(gid) = grab_id {
            let _ = crate::models::grabbed_torrents::set_download_client(
                &state.db,
                gid,
                Some(dispatch_client_id),
            )
            .await;
            // Issue #118 — fire `Grabbed` for the interactive
            // single-episode path. Indexer resolved from the
            // search-result row's `indexer_id`; score is None
            // (interactive search runs scoring on the candidate
            // list but the user picks the row, so the score
            // wasn't load-bearing for the grab decision).
            let indexer =
                crate::services::notifications::resolve_indexer_name(&state, indexer_id).await;
            crate::services::notifications::emit_grabbed(
                &state,
                sid,
                episode_number,
                &title,
                indexer,
                None,
                Some(qbit.sonarr_impl_name().to_string()),
            )
            .await;
        }
        let _ = episode_tags::record_grab_with_match(
            &state.db,
            sid,
            episode_number,
            &classification,
            &title,
            &group,
            size_bytes,
            false,
            match_provenance.as_ref(),
        )
        .await;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "selective_files": selective_outcome,
    })))
}
