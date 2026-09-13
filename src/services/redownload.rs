//! Search again after a download failed: Sonarr's
//! `RedownloadFailedDownloadService`.
//!
//! Sonarr fires it from the download-failed event, which only the
//! client's own failure report (or the user's mark-as-failed) raises;
//! an import failure is not a failed download there and is not one
//! here either (`import_stalled`, an all-files-failed import). The
//! misgrab sweep has its own gate and shares only the search itself.
//!
//! The failed row is already the blocklist entry, so the search cannot
//! pick the same release again; [`LOOP_BREAKER`] keeps a series whose
//! every grab fails in the client from searching forever.

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{
    config, grabbed_torrents, grabbed_torrents::GrabbedTorrent, metadata_cache, series,
};
use crate::services::{auto_search, logger};

/// Failed grabs per series within [`WINDOW_HOURS`] after which the
/// automatic re-search stops. The misgrab sweep has its own counter.
pub const LOOP_BREAKER: i64 = 3;
pub const WINDOW_HOURS: i64 = 24;

/// `grabbed_torrents.failure_reason` for a grab the download client
/// reported as failed (error state, missing files).
pub const CLIENT_ERROR_REASON: &str = "client_error";

/// Why the re-search did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Disabled,
    LoopBreaker(i64),
}

/// The decision on its own, for tests: the setting, then the loop
/// breaker.
pub fn decide(enabled: bool, recent_failures: i64) -> Result<(), Skip> {
    if !enabled {
        return Err(Skip::Disabled);
    }
    if recent_failures >= LOOP_BREAKER {
        return Err(Skip::LoopBreaker(recent_failures));
    }
    Ok(())
}

/// Search for a replacement after `grab` failed in the download
/// client. `why` is the short cause for the log lines ("a download
/// client error"). Honors `config.auto_redownload_failed` and the loop
/// breaker; the caller has already failed the grab, so the blocklist
/// holds the release.
pub async fn after_failed_download(state: &AppState, grab: &GrabbedTorrent, why: &str) {
    // Both reads fail closed: a database hiccup must not turn into an
    // unbounded run of searches.
    let enabled = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.auto_redownload_failed)
        .unwrap_or(false);
    let recent = grabbed_torrents::count_recent_failed(&state.db, grab.series_id, WINDOW_HOURS)
        .await
        .unwrap_or(LOOP_BREAKER);
    match decide(enabled, recent) {
        Ok(()) => search_replacement(state, grab, why).await,
        Err(Skip::Disabled) => {
            logger::debug(
                &state.db,
                LogCategory::AutoSearch,
                &format!(
                    "Not re-searching after {} for '{}': automatic re-search is off",
                    why, grab.torrent_name
                ),
                "",
            )
            .await;
        }
        Err(Skip::LoopBreaker(n)) => {
            logger::warn(
                &state.db,
                LogCategory::AutoSearch,
                &format!(
                    "Not re-searching after {} for '{}': {} failed grabs for this series in {}h",
                    why, grab.torrent_name, n, WINDOW_HOURS
                ),
                "the series needs a look; check the download client and the configured indexers",
            )
            .await;
        }
    }
}

/// Take a failed download out of the client, the way Sonarr's
/// "Remove Failed" does: the grab is blocklisted and a replacement is
/// coming, so the broken item only wastes space. Torrents under seed
/// rules stay (`respects_seed_rules`); usenet jobs go too. Failures
/// are logged and never block the re-search.
pub async fn remove_failed_from_client(state: &AppState, grab: &GrabbedTorrent) {
    let Some(client) = state
        .resolve_grab_client(grab.download_client_id, &grab.hash)
        .await
    else {
        return;
    };
    if client.protocol() != "usenet"
        && grabbed_torrents::respects_seed_rules(&state.db, &grab.hash).await
    {
        logger::info(
            &state.db,
            LogCategory::DownloadClient,
            &format!(
                "Keeping failed download '{}' in the client (respect_seed_rules)",
                grab.torrent_name
            ),
            &grab.hash,
        )
        .await;
        return;
    }
    if let Err(e) = client.delete(&grab.hash, true).await {
        logger::warn(
            &state.db,
            LogCategory::DownloadClient,
            &format!(
                "Failed to remove failed download '{}' from the client",
                grab.torrent_name
            ),
            &e,
        )
        .await;
    }
}

/// The search itself, detached: one episode gets an episode search,
/// anything else the series' auto-search (which covers every missing
/// monitored episode, Sonarr's season search included). Shared with the
/// misgrab sweep, which gates it on its own counter.
pub async fn search_replacement(state: &AppState, grab: &GrabbedTorrent, why: &str) {
    let Some(series_row) = series::get_by_id(&state.db, grab.series_id)
        .await
        .ok()
        .flatten()
    else {
        return;
    };
    let anilist_id = series_row.anilist_id;
    let series_id = grab.series_id;
    let single_episode = if !grab.is_batch && grab.episode_numbers.len() == 1 {
        Some(grab.episode_numbers[0])
    } else {
        None
    };
    let title = grab.torrent_name.clone();
    let why = why.to_string();
    let state = state.clone();
    tokio::spawn(async move {
        use crate::handlers::library::search::{
            AutoSearchQuery, auto_search_series, run_auto_search_targets,
        };
        let outcome = match single_episode {
            Some(ep) => {
                let target = match metadata_cache::get_by_series_id(&state.db, series_id).await {
                    Ok(Some(cached)) => auto_search::SearchTarget::for_episode(&cached.detail, ep),
                    _ => auto_search::SearchTarget::Episode(ep),
                };
                run_auto_search_targets(&state, anilist_id, vec![target], false, Some(series_id))
                    .await
                    .map(|r| r.grabbed.len())
            }
            None => auto_search_series(
                axum::extract::State(state.clone()),
                axum::extract::Path(anilist_id),
                axum::extract::Query(AutoSearchQuery::default()),
            )
            .await
            .map(|json| json.0.grabbed.len()),
        };
        match outcome {
            Ok(n) => {
                logger::info(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!(
                        "Re-search after {} for '{}' grabbed {} release(s)",
                        why, title, n
                    ),
                    &format!("series_id={series_id}"),
                )
                .await
            }
            Err((_, e)) => {
                logger::warn(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!("Re-search after {} for '{}' failed", why, title),
                    &e,
                )
                .await
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_honors_the_setting_then_the_loop_breaker() {
        assert_eq!(decide(false, 0), Err(Skip::Disabled));
        assert_eq!(decide(true, 0), Ok(()));
        assert_eq!(decide(true, LOOP_BREAKER - 1), Ok(()));
        assert_eq!(
            decide(true, LOOP_BREAKER),
            Err(Skip::LoopBreaker(LOOP_BREAKER))
        );
        assert_eq!(decide(false, LOOP_BREAKER), Err(Skip::Disabled));
    }
}
