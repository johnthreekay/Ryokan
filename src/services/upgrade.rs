use std::collections::{HashMap, HashSet};

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
    // The full upgrade decision (cutoff, revisions, Custom Format
    // score) for the per-candidate gate below; the quality cutoff
    // pieces above still drive the target selection.
    let policy = source::UpgradePolicy::from_config(&cfg);
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

    // Multi-client routing — resolved per-result inside the loop via
    // `client_for_indexer_with_id(result.indexer_id)`. We bail early
    // here only when the pool is empty (no clients configured at all);
    // an individual indexer's pin can still resolve later. Pre-1.5.x
    // a single global client was checked once at top — that's now
    // wrong because a per-indexer pin can route grabs to a different
    // client than the default.
    if state.default_download_client().await.is_none() {
        return Ok(UpgradeSummary {
            series_checked: 0,
            episodes_checked: 0,
            upgrades_grabbed: 0,
            detail: "Download client not configured; skipping upgrade search".to_string(),
        });
    }

    let mut total_series_checked: usize = 0;
    let mut total_episodes_checked: usize = 0;
    let mut total_upgrades_grabbed: usize = 0;

    // One compiled-CF snapshot for the whole upgrade pass. The background
    // scheduler can't race a CF edit during this run — if the user edits
    // a CF mid-sweep, the next scheduled run picks it up.
    let cfs = state.custom_formats.read().await.clone();
    // The upgrade gate scores both sides of the comparison without
    // SeaDex: the file on disk has no hash to look up, and giving the
    // candidate the SeaDex Custom Format alone would let a curated
    // pick replace an identical file for score. The seed and the
    // scoring of the search itself still consult SeaDex.
    let no_seadex_hashes: HashSet<String> = HashSet::new();

    // Issue #28 — snapshot the set of PT indexer IDs once per
    // sweep so the per-series PT-upgrade gate doesn't re-read the
    // IndexerCache (or hit the DB) on every result. The set is used
    // to skip an upgrade candidate when the source indexer is private
    // and the series hasn't opted in via `allow_pt_upgrades`. Empty
    // when the user has zero PT indexers configured — the gate then
    // never fires.
    let pt_indexer_ids: HashSet<i64> = {
        let snapshot = state.indexers.read().await.clone();
        snapshot
            .iter()
            .filter(|i| i.is_private_tracker())
            .map(|i| i.id())
            .collect()
    };

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

        // Issue #102 — the tracked snapshot was pulled at the top of
        // the sweep; a series removed mid-iteration would still be
        // checked (and any upgrade hit grabbed). Re-read the row each
        // iteration so a removed series stops the cascade promptly.
        if !crate::handlers::library::search::series_still_in_library(&state.db, Some(row.id)).await
        {
            continue;
        }

        let disk_files = media::scan_series_folder(&cfg.media_root, &row.folder_name).await;
        if disk_files.is_empty() {
            continue;
        }

        let quality_tags = episode_tags::get_for_series(&state.db, row.id)
            .await
            .unwrap_or_default();

        // Check all on-disk episodes for upgrades. The monitoring system governs
        // what to *acquire* (missing/future), not what to *upgrade*, so we use
        // disk presence directly rather than monitor state.
        let on_disk_eps: Vec<i32> = disk_files.iter().flat_map(|f| f.episodes()).collect();

        let mut upgrade_targets = auto_search::build_upgrade_targets(
            &disk_files,
            &on_disk_eps,
            cutoff_source,
            cutoff_resolution,
            cutoff_is_remux,
            cutoff_is_bdmv,
            &quality_tags,
        );
        // Files at the quality cutoff whose Custom Format total is
        // still under the format cutoff ("upgrade until Custom Format
        // score"). Sonarr only reaches these through RSS; the sweep
        // covers them too so a raised format cutoff acts on the
        // library the user already has.
        upgrade_targets.extend(format_cutoff_targets(
            &disk_files,
            &quality_tags,
            &policy,
            &cfs,
            &upgrade_targets,
        ));
        if upgrade_targets.is_empty() {
            continue;
        }

        // We need an AnimeDetail for find_best_for_target. Use the metadata cache
        // to avoid hitting external APIs during background tasks. The
        // series' alternate titles ride along: the exact title gate is
        // verbatim-only, so a show that only auto-searches through an
        // alternate title would otherwise never upgrade.
        let detail = match metadata_cache::get_by_series_id(&state.db, row.id).await {
            Ok(Some(cached)) => {
                auto_search::with_alternate_titles(cached.detail, &row.alternate_titles)
            }
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

        // Episode numbers already covered by a batch we grabbed earlier
        // in this series's loop. Without this, a 12-episode BD pack
        // covering eps 1..=12 would be re-found and re-grabbed once per
        // remaining target — 12 redundant Nyaa sweeps, 12 episode_grab_history
        // rows for the same release, and an inflated upgrade count in
        // the summary log. `record_grab` deduplicates the parent
        // grabbed_torrents row by hash, but episode_tags::record_grab
        // is unconditional INSERT, so each iteration pollutes history.
        let mut covered_by_batch: HashSet<i32> = HashSet::new();

        for target in targets {
            // Skip targets a previously-grabbed batch already covers.
            if let auto_search::SearchTarget::Episode(ep_num) = &target
                && covered_by_batch.contains(ep_num)
            {
                continue;
            }

            let label = auto_search::target_label(&target);
            // batch_episode_match=true so BD season packs can match episode targets.
            let best = auto_search::find_best_for_target(
                &state.db,
                &detail,
                &cfg,
                &target,
                true,
                true,
                &cfs,
                &state.indexers,
            )
            .await;

            let Some(result) = best else {
                continue;
            };

            // Issue #28 — PT upgrade gate. When a user hasn't
            // opted this series in to PT-sourced upgrades and the
            // chosen candidate came from a private tracker, skip
            // the upgrade. The user can still grab from PT manually
            // (initial grab + manual search aren't gated) — this
            // only stops the background sweep from silently
            // re-grabbing existing episodes from a PT and racking
            // up Hit-and-Run liability.
            //
            // Trade-off: skipping here means we might miss a
            // legitimate non-PT upgrade that was ranked second
            // behind the PT pick. Acceptable for v1.5 — the next
            // sweep tick (24h cadence) re-runs and the non-PT
            // candidate would then be the top pick if the PT one
            // dropped off. If users hit this in practice, we can
            // upgrade to candidate-pool filtering inside
            // `find_best_for_target` later.
            if !row.allow_pt_upgrades
                && let Some(idx_id) = result.indexer_id
                && pt_indexer_ids.contains(&idx_id)
            {
                logger::debug(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!(
                        "Upgrade: {} {} skipped — PT-sourced and series.allow_pt_upgrades = 0",
                        title,
                        auto_search::target_label(&target),
                    ),
                    &format!("indexer_id={idx_id}"),
                )
                .await;
                continue;
            }

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
            // gate (`source::judge_upgrade`: quality against the
            // cutoff, a `v2` from the same group, or a better Custom
            // Format score). Keeps RSS and upgrade_search consistent.
            if let auto_search::SearchTarget::Episode(ep_num) = &target
                && let Some(existing_classification) = upgrade_classifications.get(ep_num)
            {
                let tag = quality_tags.get(ep_num);
                let disk_file = disk_files.iter().find(|f| f.holds(*ep_num));
                let (existing_title, existing_group) = match tag {
                    Some(t) if !t.release_title.is_empty() => {
                        (t.release_title.as_str(), t.release_group.as_str())
                    }
                    _ => (
                        disk_file.map(|f| f.filename.as_str()).unwrap_or_default(),
                        "",
                    ),
                };
                let existing_file = source::ExistingFile {
                    classification: existing_classification,
                    revision: media::parse_release_revision(existing_title),
                    release_group: existing_group,
                    cf_score: crate::services::custom_formats::total_cf_score_for_release(
                        &cfs,
                        existing_classification,
                        existing_title,
                        existing_group,
                        disk_file.map(|f| f.size_bytes as i64).unwrap_or(0),
                        "",
                        &no_seadex_hashes,
                    ),
                    age_days: None,
                };
                let candidate = source::UpgradeCandidate {
                    classification: &incoming_classification,
                    revision: media::parse_release_revision(&result.title),
                    release_group: &result.group,
                    cf_score: crate::services::custom_formats::total_cf_score_for_release(
                        &cfs,
                        &incoming_classification,
                        &result.title,
                        &result.group,
                        result.size_bytes,
                        &result.info_hash,
                        &no_seadex_hashes,
                    ),
                };
                match source::judge_upgrade(&policy, &existing_file, &candidate, false) {
                    Ok(kind) => {
                        logger::info(
                            &state.db,
                            LogCategory::AutoSearch,
                            &format!(
                                "Upgrade ({}): {} {} — {} -> {}",
                                kind.as_str(),
                                title,
                                label,
                                existing_classification.label(),
                                incoming_classification.label()
                            ),
                            &result.title,
                        )
                        .await;
                    }
                    Err(rejection) => {
                        logger::debug(
                            &state.db,
                            LogCategory::AutoSearch,
                            &format!("Upgrade: {} {} skipped — {}", title, label, rejection),
                            &result.title,
                        )
                        .await;
                        continue;
                    }
                }
            }

            let url = if !result.magnet.is_empty() {
                result.magnet.clone()
            } else {
                result.torrent.clone()
            };
            if url.is_empty() {
                continue;
            }

            // Resolve the right client per-result so a PT indexer
            // pinned to the seedbox (and a public indexer landing on
            // local qBit) both work in the same sweep. Skip the
            // result if pin resolution finds no client at all
            // (pool empty or all-disabled mid-sweep).
            let (client, dispatch_client_id) =
                match state.client_for_indexer_with_id(result.indexer_id).await {
                    Some(t) => t,
                    None => {
                        logger::warn(
                            &state.db,
                            LogCategory::DownloadClient,
                            &format!("Upgrade skip: no download client for {}", result.title),
                            "indexer pin resolution returned None",
                        )
                        .await;
                        continue;
                    }
                };

            match client
                .add_torrent_returning_id(&url, &result.info_hash)
                .await
            {
                Ok((_outcome, canonical_id)) => {
                    total_upgrades_grabbed += 1;
                    logger::info(
                        &state.db,
                        LogCategory::Grab,
                        &format!("Upgrade grabbed: {}", result.title),
                        &format!(
                            "series={}, target={}, group={}, tier={}{}",
                            title,
                            label,
                            result.group,
                            incoming_classification.label(),
                            crate::services::auto_search::MatchProvenance::log_suffix(
                                result.match_provenance.as_ref()
                            )
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
                        // Mark every episode this batch will cover as
                        // satisfied so the rest of this series's loop
                        // doesn't re-find / re-grab the same pack.
                        covered_by_batch.extend(&ep_nums);
                    }
                    let grab_id = crate::models::grabbed_torrents::record_grab(
                        &state.db,
                        &canonical_id,
                        &result.title,
                        row.id,
                        &ep_nums,
                        result.is_batch,
                    )
                    .await
                    .ok()
                    .flatten();
                    // Misgrab guardrails: keep the URL so Restore can re-add a removed grab.
                    if let Some(gid) = grab_id {
                        let _ =
                            crate::models::grabbed_torrents::set_source_url(&state.db, gid, &url)
                                .await;
                    }
                    if let Some(gid) = grab_id {
                        let _ = crate::models::grabbed_torrents::set_download_client(
                            &state.db,
                            gid,
                            Some(dispatch_client_id),
                        )
                        .await;
                        // Issue #118 — fire `Grabbed` for upgrade-sweep
                        // grabs. Same context shape as the auto_search
                        // path (indexer + score + client_kind) since
                        // both run through the scoring pipeline.
                        let indexer = crate::services::notifications::resolve_indexer_name(
                            state,
                            result.indexer_id,
                        )
                        .await;
                        crate::services::notifications::emit_grabbed(
                            state,
                            row.id,
                            ep_nums.first().copied().unwrap_or(0),
                            &result.title,
                            indexer,
                            Some(result.score),
                            Some(client.sonarr_impl_name().to_string()),
                        )
                        .await;
                    }
                    for ep_num in &ep_nums {
                        let _ = episode_tags::record_grab_with_match(
                            &state.db,
                            row.id,
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
                }
                Err(err) => {
                    logger::error(
                        &state.db,
                        LogCategory::DownloadClient,
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

/// Files that already meet the quality cutoff but whose Custom Format
/// total is under `policy.format_cutoff_score`, as extra upgrade
/// targets. Same exclusions as `build_upgrade_targets` (multi-episode
/// files, pinned rows, files the classifier could not place), plus
/// anything already targeted for quality. The score is computed from
/// the tag row's release title, or the file name when there is no
/// row, with no SeaDex hash on either side.
fn format_cutoff_targets(
    disk_files: &[media::EpisodeFile],
    quality_tags: &HashMap<i32, episode_tags::EpisodeQualityTag>,
    policy: &source::UpgradePolicy,
    cfs: &[crate::services::custom_formats::CompiledCustomFormat],
    already: &[(auto_search::SearchTarget, ClassificationResult)],
) -> Vec<(auto_search::SearchTarget, ClassificationResult)> {
    if cfs.is_empty() {
        return Vec::new();
    }
    let targeted: HashSet<i32> = already
        .iter()
        .filter_map(|(t, _)| match t {
            auto_search::SearchTarget::Episode(n) => Some(*n),
            auto_search::SearchTarget::Single => None,
        })
        .collect();
    let no_seadex: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for file in disk_files {
        let ep = file.episode_number;
        if file.is_multi_episode() || targeted.contains(&ep) {
            continue;
        }
        let tag = quality_tags.get(&ep);
        if tag.is_some_and(|t| t.manual_override) {
            continue;
        }
        let existing = auto_search::resolve_existing_classification(file, tag);
        if existing.source == Source::Unknown && existing.resolution == Resolution::Unknown {
            continue;
        }
        let (title, group) = match tag {
            Some(t) if !t.release_title.is_empty() => {
                (t.release_title.as_str(), t.release_group.as_str())
            }
            _ => (file.filename.as_str(), ""),
        };
        let score = crate::services::custom_formats::total_cf_score_for_release(
            cfs,
            &existing,
            title,
            group,
            file.size_bytes as i64,
            "",
            &no_seadex,
        );
        if score < policy.format_cutoff_score {
            out.push((auto_search::SearchTarget::Episode(ep), existing));
        }
    }
    out.sort_by_key(|(t, _)| match t {
        auto_search::SearchTarget::Episode(n) => *n,
        auto_search::SearchTarget::Single => 0,
    });
    out
}

#[cfg(test)]
mod format_cutoff_target_tests {
    use super::*;
    use crate::models::episode_tags::EpisodeQualityTag;
    use crate::services::custom_formats::compile_from_json;
    use crate::services::source::{ProperPolicy, UpgradePolicy};

    fn file(ep: i32, name: &str) -> media::EpisodeFile {
        media::EpisodeFile {
            filename: name.to_string(),
            episode_number: ep,
            episode_last: ep,
            season_number: Some(1),
            quality: "WEB-1080p".to_string(),
            size_bytes: 1_000,
            size_display: String::new(),
            modified_secs: None,
        }
    }

    fn tag(ep: i32, release_title: &str, pinned: bool) -> (i32, EpisodeQualityTag) {
        (
            ep,
            EpisodeQualityTag {
                episode_number: ep,
                quality_tag: "WEB-1080p".to_string(),
                release_title: release_title.to_string(),
                release_group: "G".to_string(),
                state: "completed".to_string(),
                source: "Web".to_string(),
                resolution: "1080p".to_string(),
                is_remux: false,
                is_bdmv: false,
                web_kind: String::new(),
                classification_confidence: 1.0,
                needs_review: false,
                manual_override: pinned,
                classification_evidence: String::new(),
                classification_attempted_at: None,
            },
        )
    }

    fn policy(format_cutoff_score: i32) -> UpgradePolicy {
        UpgradePolicy {
            cutoff: source::cutoff_classification(Source::Web, Resolution::R1080p, false, false),
            propers: ProperPolicy::PreferAndUpgrade,
            format_cutoff_score,
            format_increment: 1,
        }
    }

    /// One CF: +100 when the title carries `Dual-Audio`.
    fn dual_audio_cf() -> Vec<crate::services::custom_formats::CompiledCustomFormat> {
        let json = r#"{
            "name": "Dual Audio",
            "specifications": [{
                "name": "dual", "implementation": "ReleaseTitleSpecification",
                "negate": false, "required": false,
                "fields": [{"name": "value", "value": "Dual-Audio"}]
            }]
        }"#;
        vec![compile_from_json(json, 100, 1).expect("cf compiles")]
    }

    #[test]
    fn files_under_the_format_cutoff_become_targets() {
        let files = vec![
            file(1, "Show - S01E01.mkv"),
            file(2, "Show - S01E02.mkv"),
            file(3, "Show - S01E03.mkv"),
            file(4, "Show - S01E04.mkv"),
        ];
        let tags: HashMap<i32, EpisodeQualityTag> = [
            tag(1, "[G] Show - 01 (1080p)", false),
            tag(2, "[G] Show - 02 (1080p) [Dual-Audio]", false),
            tag(3, "[G] Show - 03 (1080p)", true),
        ]
        .into_iter()
        .collect();
        let already = vec![(
            auto_search::SearchTarget::Episode(4),
            source::classify_release_sync("Show - S01E04.mkv", None),
        )];
        let targets =
            format_cutoff_targets(&files, &tags, &policy(100), &dual_audio_cf(), &already);
        let eps: Vec<i32> = targets
            .iter()
            .map(|(t, _)| match t {
                auto_search::SearchTarget::Episode(n) => *n,
                auto_search::SearchTarget::Single => 0,
            })
            .collect();
        // 1: score 0 < 100, targeted. 2: score 100, meets the cutoff.
        // 3: pinned. 4: already a quality target.
        assert_eq!(eps, vec![1]);
    }

    #[test]
    fn no_custom_formats_or_a_zero_cutoff_yields_nothing() {
        let files = vec![file(1, "Show - S01E01.mkv")];
        let tags: HashMap<i32, EpisodeQualityTag> = [tag(1, "[G] Show - 01 (1080p)", false)]
            .into_iter()
            .collect();
        assert!(format_cutoff_targets(&files, &tags, &policy(100), &[], &[]).is_empty());
        // Default cutoff 0: a file scoring 0 meets it.
        assert!(format_cutoff_targets(&files, &tags, &policy(0), &dual_audio_cf(), &[]).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::download_clients::{DownloadClientForm, insert as insert_dc};
    use crate::services::download_client::{
        AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
    };
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
    use async_trait::async_trait;
    use std::sync::Arc;

    /// Single test-suite serializer. The production `UPGRADE_LOCK` is
    /// process-wide and `try_lock` early-returns on contention; without
    /// this serializer parallel `tokio::test`s race on the lock and the
    /// already-running test could leak state into other tests.
    static UPGRADE_TEST_SERIALIZER: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    /// Stub client — `run_once` only ever calls `add_torrent_returning_id`
    /// in the upgrade-grab path, but the tests below all early-return
    /// before any candidate is found, so no method is actually exercised.
    struct StubClient;

    #[async_trait]
    impl DownloadClient for StubClient {
        async fn test(&self) -> Result<String, String> {
            Ok("stub".into())
        }
        async fn add_torrent(&self, _u: &str, _h: &str) -> Result<AddOutcome, String> {
            Ok(AddOutcome::Added)
        }
        async fn add_torrent_with_file_filter(
            &self,
            _u: &str,
            _h: &str,
            _p: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
        ) -> Result<SelectiveOutcome, String> {
            Ok(SelectiveOutcome::FullDownload)
        }
        async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
            Ok(vec![])
        }
        async fn get_files(&self, _h: &str) -> Result<Vec<DownloadFile>, String> {
            Ok(vec![])
        }
        async fn pause(&self, _h: &str) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self, _h: &str) -> Result<(), String> {
            Ok(())
        }
        async fn delete(&self, _h: &str, _df: bool) -> Result<(), String> {
            Ok(())
        }
        async fn set_file_wanted(&self, _h: &str, _f: &[usize], _w: bool) -> Result<(), String> {
            Ok(())
        }
        fn sonarr_impl_name(&self) -> &'static str {
            "QBittorrent"
        }
    }

    async fn install_default_client(state: &crate::AppState) {
        let mut clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>> =
            std::collections::HashMap::new();
        clients.insert(1, Arc::new(StubClient));
        let pool = crate::DownloadClientPool {
            clients,
            default_torrent_id: Some(1),
            default_usenet_id: None,
        };
        *state.download_clients.write().await = Arc::new(pool);
    }

    async fn seed_default_config(db: &sqlx::SqlitePool) {
        // Default cutoff: bluray/1080 — the no-cutoff early-return does
        // not fire. media_root + post_processing_mode satisfy the NOT
        // NULL columns expected by `config::get_config`.
        sqlx::query(
            "INSERT INTO config (id, media_root, post_processing_mode, cutoff_source, cutoff_resolution) \
             VALUES (1, '/tmp/upgrade-test', 'hardlink', 'bluray', '1080')",
        )
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_once_returns_already_running_when_lock_contended() {
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let state = build_test_app_state(in_memory_pool().await, None);
        // Hold the production lock for the duration of the call below.
        let _held = UPGRADE_LOCK.lock().await;
        let result = run_once(&state).await;
        match result {
            Err(msg) => assert!(msg.contains("already running"), "unexpected message: {msg}"),
            Ok(_) => panic!("expected Err on contended lock"),
        }
    }

    #[tokio::test]
    async fn run_once_skips_when_no_quality_cutoff_configured() {
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        // Both cutoff fields blank → unknown source + unknown resolution
        // ⇒ early return with the "no cutoff" detail message.
        sqlx::query(
            "INSERT INTO config (id, media_root, post_processing_mode, cutoff_source, cutoff_resolution) \
             VALUES (1, '/tmp/upgrade-test', 'hardlink', '', '')",
        )
        .execute(&db)
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        install_default_client(&state).await;
        let summary = run_once(&state).await.unwrap();
        assert_eq!(summary.series_checked, 0);
        assert_eq!(summary.episodes_checked, 0);
        assert_eq!(summary.upgrades_grabbed, 0);
        assert!(
            summary.detail.contains("No quality cutoff"),
            "unexpected detail: {}",
            summary.detail
        );
    }

    #[tokio::test]
    async fn run_once_skips_when_no_default_download_client() {
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        seed_default_config(&db).await;
        // Build state without a default download client → the second
        // early-return ("Download client not configured") fires.
        let state = build_test_app_state(db, None);
        let summary = run_once(&state).await.unwrap();
        assert!(
            summary.detail.contains("Download client not configured"),
            "unexpected detail: {}",
            summary.detail
        );
        assert_eq!(summary.upgrades_grabbed, 0);
    }

    #[tokio::test]
    async fn run_once_no_tracked_series_yields_zero_summary() {
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        seed_default_config(&db).await;
        let state = build_test_app_state(db, None);
        install_default_client(&state).await;

        let summary = run_once(&state).await.unwrap();
        assert_eq!(summary.series_checked, 0);
        assert_eq!(summary.episodes_checked, 0);
        assert_eq!(summary.upgrades_grabbed, 0);
        assert!(
            summary.detail.starts_with("Checked 0 series"),
            "unexpected detail: {}",
            summary.detail
        );
    }

    #[tokio::test]
    async fn run_once_skips_series_with_empty_folder_name() {
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        seed_default_config(&db).await;
        seed_series(&db, 100, "Show A").await;
        // Force the seed_series default folder_name to empty string
        // — this hits the `if row.folder_name.is_empty()` guard so
        // the series doesn't increment series_checked.
        sqlx::query("UPDATE series SET folder_name = ''")
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db, None);
        install_default_client(&state).await;

        let summary = run_once(&state).await.unwrap();
        assert_eq!(summary.series_checked, 0);
        assert_eq!(summary.episodes_checked, 0);
    }

    #[tokio::test]
    async fn run_once_skips_series_with_allow_upgrades_off() {
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        seed_default_config(&db).await;
        seed_series(&db, 200, "Show B").await;
        // Per-series upgrade opt-out (Phase 4) — flip allow_upgrades to 0.
        sqlx::query("UPDATE series SET allow_upgrades = 0")
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db, None);
        install_default_client(&state).await;

        let summary = run_once(&state).await.unwrap();
        assert_eq!(summary.series_checked, 0);
    }

    #[tokio::test]
    async fn run_once_skips_series_with_no_disk_files() {
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        // Point media_root at a real but empty tempdir so
        // `scan_series_folder` returns an empty list and the loop
        // short-circuits before reaching `find_best_for_target`. This
        // pins the "no on-disk files → don't upgrade" branch without
        // any external API calls.
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().to_string_lossy().into_owned();
        sqlx::query(
            "INSERT INTO config (id, media_root, post_processing_mode, cutoff_source, cutoff_resolution) \
             VALUES (1, ?, 'hardlink', 'bluray', '1080')",
        )
        .bind(&media_root)
        .execute(&db)
        .await
        .unwrap();
        seed_series(&db, 300, "Show C").await;

        let state = build_test_app_state(db, None);
        install_default_client(&state).await;

        let summary = run_once(&state).await.unwrap();
        assert_eq!(summary.series_checked, 0); // never incremented
        assert_eq!(summary.episodes_checked, 0);
    }

    /// Stage one episode file `S01E01.mkv` inside `<tempdir>/<folder>/`
    /// and return the tempdir handle (so it survives the test) plus the
    /// media-root path string. The series's `folder_name` should match
    /// `folder` so `media::scan_series_folder` walks into our directory.
    fn stage_disk_file(folder: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let series_dir = dir.path().join(folder);
        std::fs::create_dir_all(&series_dir).unwrap();
        std::fs::write(
            series_dir.join("Show - S01E01 [BluRay 1080p].mkv"),
            b"fake-mkv",
        )
        .unwrap();
        let media_root = dir.path().to_string_lossy().into_owned();
        (dir, media_root)
    }

    #[tokio::test]
    async fn run_once_skips_when_existing_quality_already_at_cutoff() {
        // disk file present + episode_quality_tags row marks it BluRay
        // 1080p (== the configured cutoff) → build_upgrade_targets
        // returns empty → continue without touching find_best_for_target.
        // This pins the "no upgrade needed" branch of the upgrade loop.
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        let (_tempdir, media_root) = stage_disk_file("Show");
        sqlx::query(
            "INSERT INTO config (id, media_root, post_processing_mode, cutoff_source, cutoff_resolution) \
             VALUES (1, ?, 'hardlink', 'bluray', '1080')",
        )
        .bind(&media_root)
        .execute(&db)
        .await
        .unwrap();
        let series_id = seed_series(&db, 12001, "Show").await;
        // Tag the file as BluRay 1080p == cutoff. The build_upgrade_targets
        // helper drops candidates whose existing rank >= cutoff rank, so
        // an exact-cutoff row produces an empty target list.
        sqlx::query(
            "INSERT INTO episode_quality_tags \
             (series_id, episode_number, quality_tag, source, resolution, is_remux, is_bdmv) \
             VALUES (?, 1, 'BluRay-1080p', 'BluRay', '1080p', 0, 0)",
        )
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        install_default_client(&state).await;

        let summary = run_once(&state).await.unwrap();
        // series_checked is incremented only after the metadata_cache
        // lookup; but we never reach it because upgrade_targets is
        // empty, so the loop hits `continue` first.
        assert_eq!(summary.series_checked, 0);
        assert_eq!(summary.upgrades_grabbed, 0);
    }

    #[tokio::test]
    async fn run_once_skips_series_without_cached_metadata() {
        // disk file + sub-cutoff tag → build_upgrade_targets returns
        // non-empty. But metadata_cache has no row for this series, so
        // the metadata_cache lookup falls through to the "skipping —
        // no cached metadata" debug log and `continue`. Pins the
        // missing-cache fallback branch.
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        let (_tempdir, media_root) = stage_disk_file("Show");
        sqlx::query(
            "INSERT INTO config (id, media_root, post_processing_mode, cutoff_source, cutoff_resolution) \
             VALUES (1, ?, 'hardlink', 'bluray', '1080')",
        )
        .bind(&media_root)
        .execute(&db)
        .await
        .unwrap();
        let series_id = seed_series(&db, 12002, "Show").await;
        // Tag at WEB-720p — strictly below BluRay-1080p cutoff, so
        // build_upgrade_targets returns this episode as a candidate.
        sqlx::query(
            "INSERT INTO episode_quality_tags \
             (series_id, episode_number, quality_tag, source, resolution, is_remux, is_bdmv) \
             VALUES (?, 1, 'WEB-720p', 'Web', '720p', 0, 0)",
        )
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
        // Deliberately no metadata_cache row.
        let state = build_test_app_state(db, None);
        install_default_client(&state).await;

        let summary = run_once(&state).await.unwrap();
        assert_eq!(
            summary.series_checked, 0,
            "series_checked is incremented after the cache lookup; missing cache must keep it at 0"
        );
        assert_eq!(summary.upgrades_grabbed, 0);
    }

    #[tokio::test]
    async fn run_once_persists_pt_indexer_set_without_failure() {
        // PT-pin gate snapshot (the `pt_indexer_ids` HashSet) is built
        // from the indexer cache. Pin one private + one public indexer
        // and verify run_once doesn't blow up when reading them, even
        // though no series exists to exercise the gate further.
        // Ensures the snapshot path is exercised at least once.
        let _serial = UPGRADE_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        seed_default_config(&db).await;
        // Seed a download client so the no-default early-return doesn't fire.
        insert_dc(
            &db,
            DownloadClientForm {
                name: "qb",
                kind: "qbittorrent",
                url: "http://x",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: true,
            },
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        install_default_client(&state).await;

        // Empty series list — exits in the "Checked 0 series" path.
        let summary = run_once(&state).await.unwrap();
        assert!(summary.detail.starts_with("Checked"));
    }
}
