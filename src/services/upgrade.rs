use std::collections::HashMap;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, metadata_cache, series};
use crate::services::source::{self, ClassificationResult, Resolution, Source};
use crate::services::{auto_search, logger, media};

static UPGRADE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

pub struct UpgradeSummary {
    pub series_checked: usize,
    pub episodes_checked: usize,
    pub upgrades_grabbed: usize,
    pub detail: String,
}

pub async fn run_once(state: &AppState) -> Result<UpgradeSummary, String> {
    let _guard = UPGRADE_LOCK
        .try_lock()
        .map_err(|_| "Upgrade search is already running".to_string())?;

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    let (cutoff_source, cutoff_is_remux, cutoff_is_bdmv) =
        source::parse_cutoff_source(&cfg.cutoff_source);
    let cutoff_resolution = Resolution::from_str(&cfg.cutoff_resolution);
    if cutoff_source == Source::Unknown && cutoff_resolution == Resolution::Unknown {
        return Ok(UpgradeSummary {
            series_checked: 0,
            episodes_checked: 0,
            upgrades_grabbed: 0,
            detail: "No quality cutoff configured; skipping upgrade search".to_string(),
        });
    }

    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| e.to_string())?;

    let client_opt = state.download_client.read().await.clone();
    let Some(client) = client_opt.as_ref() else {
        return Ok(UpgradeSummary {
            series_checked: 0,
            episodes_checked: 0,
            upgrades_grabbed: 0,
            detail: "Download client not configured; skipping upgrade search".to_string(),
        });
    };

    let mut total_series_checked: usize = 0;
    let mut total_episodes_checked: usize = 0;
    let mut total_upgrades_grabbed: usize = 0;

    // One compiled-CF snapshot for the whole upgrade pass. The background
    // scheduler can't race a CF edit during this run — if the user edits
    // a CF mid-sweep, the next scheduled run picks it up.
    let cfs = state.custom_formats.read().await.clone();

    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        "Upgrade search started",
        &format!("{} tracked series", tracked.len()),
    )
    .await;

    // SeaDex prewarm: collapses the typical "most series don't have a
    // SeaDex entry" tail of the per-series loop from N sequential
    // round-trips into ceil(N/50) batched OR-filter requests. Only
    // runs when SeaDex is actually consulted by the scoring path
    // (config toggle OR a SeaDex-using Custom Format installed).
    let seadex_will_be_consulted =
        cfg.seadex_enabled || crate::services::custom_formats::has_seadex_cf(&cfs);
    if seadex_will_be_consulted {
        let anilist_ids: Vec<i64> = tracked
            .iter()
            .filter(|r| r.allow_upgrades && !r.folder_name.is_empty() && r.anilist_id > 0)
            .map(|r| r.anilist_id)
            .collect();
        if !anilist_ids.is_empty() {
            auto_search::prewarm_seadex_negative(&state.db, &anilist_ids).await;
        }
    }

    for row in &tracked {
        // Skip series with no folder (not set up yet).
        if row.folder_name.is_empty() {
            continue;
        }

        // Phase 4: per-series upgrade opt-out. User can disable upgrades
        // for individual series via the series detail page toggle.
        if !row.allow_upgrades {
            continue;
        }

        let disk_files = media::scan_series_folder(&cfg.media_root, &row.folder_name);
        if disk_files.is_empty() {
            continue;
        }

        let quality_tags = episode_tags::get_for_series(&state.db, row.id)
            .await
            .unwrap_or_default();

        // Check all on-disk episodes for upgrades. The monitoring system governs
        // what to *acquire* (missing/future), not what to *upgrade*, so we use
        // disk presence directly rather than monitor state.
        let on_disk_eps: Vec<i32> = disk_files.iter().map(|f| f.episode_number).collect();

        let upgrade_targets = auto_search::build_upgrade_targets(
            &disk_files,
            &on_disk_eps,
            cutoff_source,
            cutoff_resolution,
            cutoff_is_remux,
            cutoff_is_bdmv,
            &quality_tags,
        );
        if upgrade_targets.is_empty() {
            continue;
        }

        // We need an AnimeDetail for find_best_for_target. Use the metadata cache
        // to avoid hitting external APIs during background tasks.
        let detail = match metadata_cache::get_by_series_id(&state.db, row.id).await {
            Ok(Some(cached)) => cached.detail,
            _ => {
                logger::debug(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!("Upgrade: skipping {} — no cached metadata", row.title),
                    "",
                )
                .await;
                continue;
            }
        };

        total_series_checked += 1;
        let title = if !detail.title_english.is_empty() {
            &detail.title_english
        } else {
            &detail.title_romaji
        };

        let mut upgrade_classifications: HashMap<i32, ClassificationResult> = HashMap::new();
        let mut targets: Vec<auto_search::SearchTarget> = Vec::with_capacity(upgrade_targets.len());
        for (t, c) in upgrade_targets {
            if let auto_search::SearchTarget::Episode(n) = &t {
                upgrade_classifications.insert(*n, c);
            }
            targets.push(t);
        }
        let target_count = targets.len();
        total_episodes_checked += target_count;

        logger::debug(
            &state.db,
            LogCategory::AutoSearch,
            &format!("Upgrade: checking {} ({} episodes)", title, target_count),
            "",
        )
        .await;

        for target in targets {
            let label = auto_search::target_label(&target);
            // batch_episode_match=true so BD season packs can match episode targets.
            let best = auto_search::find_best_for_target(
                &state.db, &detail, &cfg, &target, true, true, &cfs,
            )
            .await;

            let Some(result) = best else {
                continue;
            };

            // Classify the incoming release once; reused for upgrade verification,
            // grab logging, and episode tag persistence below.
            let incoming_classification = source::classify_release(
                &state.db,
                &result.title,
                Some(&result.resolution),
                Some(source::NyaaContext {
                    info_hash: &result.info_hash,
                    view_url: &result.link,
                    is_batch: result.is_batch,
                }),
                Some(source::SeriesContext {
                    status: &row.status,
                    season_year: row.season_year,
                    end_year: row.end_year,
                }),
            )
            .await;

            // Verify this is actually an upgrade via the shared policy
            // gate (strict rank improvement AND not a non-BDMV → BDMV
            // crossing — see source::is_valid_upgrade for the BDMV
            // rationale). Keeps RSS and upgrade_search consistent.
            if let auto_search::SearchTarget::Episode(ep_num) = &target
                && let Some(existing_classification) = upgrade_classifications.get(ep_num)
            {
                if !source::is_valid_upgrade(existing_classification, &incoming_classification) {
                    continue;
                }
                logger::info(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!(
                        "Upgrade: {} {} — {} -> {}",
                        title,
                        label,
                        existing_classification.label(),
                        incoming_classification.label()
                    ),
                    &result.title,
                )
                .await;
            }

            let url = if !result.magnet.is_empty() {
                result.magnet.clone()
            } else {
                result.torrent.clone()
            };
            if url.is_empty() {
                continue;
            }

            match client.add_torrent(&url, &result.info_hash).await {
                Ok(_) => {
                    total_upgrades_grabbed += 1;
                    logger::info(
                        &state.db,
                        LogCategory::Grab,
                        &format!("Upgrade grabbed: {}", result.title),
                        &format!(
                            "series={}, target={}, group={}, tier={}",
                            title,
                            label,
                            result.group,
                            incoming_classification.label()
                        ),
                    )
                    .await;

                    // Record for post-processing and quality tags.
                    let mut ep_nums: Vec<i32> = match &target {
                        auto_search::SearchTarget::Episode(n) => vec![*n],
                        auto_search::SearchTarget::Single => vec![1],
                    };
                    if result.is_batch {
                        let parsed = auto_search::parse_release_numbers(&result.title);
                        if !parsed.is_empty() {
                            ep_nums = parsed.into_iter().collect();
                            ep_nums.sort_unstable();
                        }
                    }
                    let _ = crate::models::grabbed_torrents::record_grab(
                        &state.db,
                        &result.info_hash,
                        &result.title,
                        row.id,
                        &ep_nums,
                        result.is_batch,
                    )
                    .await;
                    for ep_num in &ep_nums {
                        let _ = episode_tags::record_grab(
                            &state.db,
                            row.id,
                            *ep_num,
                            &incoming_classification,
                            &result.title,
                            &result.group,
                            result.size_bytes,
                            result.is_batch,
                        )
                        .await;
                    }
                }
                Err(err) => {
                    logger::error(
                        &state.db,
                        LogCategory::QBit,
                        &format!("Upgrade grab failed: {} {}", title, label),
                        &err,
                    )
                    .await;
                }
            }

            // Rate-limit between searches to avoid hammering Nyaa.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        // Rate-limit between series.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    let detail = format!(
        "Checked {} series, {} episodes, grabbed {} upgrades",
        total_series_checked, total_episodes_checked, total_upgrades_grabbed
    );
    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        "Upgrade search finished",
        &detail,
    )
    .await;

    Ok(UpgradeSummary {
        series_checked: total_series_checked,
        episodes_checked: total_episodes_checked,
        upgrades_grabbed: total_upgrades_grabbed,
        detail,
    })
}
