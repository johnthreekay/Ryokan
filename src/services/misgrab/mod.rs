//! Misgrab guardrails: verify a grab against the files the download
//! client reports, and remediate the ones that turn out to be a
//! different series.
//!
//! Two halves. The **verdict** (`verdict.rs`) is pure and runs wherever
//! a file list first shows up: the grab-time metadata wait in
//! `auto_expand`, the post-processing import path, or the sweep below.
//! It is stamped once on the grab row (`grabbed_torrents.verification`)
//! and never flips on its own. **Remediation** (delete from the client,
//! blocklist, notify, re-search) runs only from the supervised
//! `misgrab_sweep`, the one place with `AppState` that covers all nine
//! grab paths and survives restarts.

pub mod verdict;

use std::time::Duration;

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::grabbed_torrents::{self, GrabbedTorrent};
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, metadata_cache, series};
use crate::services::anilist::AnimeDetail;
use crate::services::{auto_search, logger, notifications};

pub use verdict::{Verdict, VerdictInput, assess};

/// How often the sweep looks for unverified grabs and unhandled
/// misgrabs.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// A grab younger than this is left to its grab-time spawn.
pub const MIN_AGE_SECS: i64 = 20;
/// How long the sweep keeps asking the client for a file list before
/// giving up on a grab as unverifiable.
pub const METADATA_GRACE: Duration = Duration::from_secs(15 * 60);

/// The aliases a verdict is judged against.
#[derive(Debug, Clone, Default)]
pub struct AliasSet {
    pub own: Vec<String>,
    pub siblings: Vec<String>,
    pub expected_season: i32,
}

/// Own titles plus synonyms, and every related entry's titles plus the
/// arc subtitles sibling detection recognized in these file names.
pub fn aliases_from_detail(detail: &AnimeDetail, filenames: &[String]) -> AliasSet {
    let mut own = auto_search::collect_aliases(detail);
    own.extend(detail.synonyms.iter().cloned());
    let own = auto_search::dedupe_strings(own);
    let mut siblings = auto_search::collect_sibling_aliases(detail, &own);
    for rel in &detail.relations {
        for title in [&rel.title_romaji, &rel.title_english, &rel.title_native] {
            if !title.trim().is_empty() {
                siblings.push(title.clone());
            }
        }
    }
    for sibling in auto_search::detect_sibling_entries_in_pack(filenames, detail) {
        if !sibling.matched_subtitle.trim().is_empty() {
            siblings.push(sibling.matched_subtitle.clone());
        }
        for title in [
            &sibling.title_romaji,
            &sibling.title_english,
            &sibling.title_native,
        ] {
            if !title.trim().is_empty() {
                siblings.push(title.clone());
            }
        }
    }
    AliasSet {
        own,
        siblings: auto_search::dedupe_strings(siblings),
        expected_season: auto_search::infer_season_from_detail(detail),
    }
}

/// Aliases for a grab the sweep or the import path holds: the cached
/// metadata when there is one, else the series row's own titles (no
/// siblings, no season). `None` when the series row is gone.
pub async fn aliases_for_grab(
    db: &SqlitePool,
    grab: &GrabbedTorrent,
    filenames: &[String],
) -> Option<AliasSet> {
    if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, grab.series_id).await {
        return Some(aliases_from_detail(&cached.detail, filenames));
    }
    let row = series::get_by_id(db, grab.series_id).await.ok().flatten()?;
    let own = auto_search::dedupe_strings(vec![
        row.title.clone(),
        row.title_romaji.clone(),
        row.title_english.clone(),
        row.title_native.clone(),
    ]);
    Some(AliasSet {
        own,
        siblings: Vec::new(),
        expected_season: 0,
    })
}

/// Judge the file list and stamp the verdict once. A hash the user
/// restored is stamped `whitelisted` without being judged. Logs a
/// warning the first time a misgrab is recorded.
pub async fn assess_and_stamp(
    db: &SqlitePool,
    grab: &GrabbedTorrent,
    filenames: &[String],
    aliases: &AliasSet,
) -> Verdict {
    if grabbed_torrents::is_whitelisted_hash(db, &grab.hash).await {
        let verdict = Verdict::Verified {
            matched_file: String::new(),
            matched_alias: "whitelisted by the user".to_string(),
            notes: Vec::new(),
        };
        let detail = serde_json::to_string(&verdict.detail(filenames)).unwrap_or_default();
        let _ = grabbed_torrents::stamp_verification(db, grab.id, "whitelisted", &detail).await;
        return verdict;
    }
    let verdict = assess(&VerdictInput {
        own_aliases: &aliases.own,
        sibling_aliases: &aliases.siblings,
        filenames,
        expected_season: aliases.expected_season,
    });
    let detail = verdict.detail(filenames);
    let detail_json = serde_json::to_string(&detail).unwrap_or_default();
    let wrote = grabbed_torrents::stamp_verification(db, grab.id, verdict.as_str(), &detail_json)
        .await
        .unwrap_or(false);
    if wrote && verdict.is_misgrab() {
        logger::warn(
            db,
            LogCategory::Grab,
            &format!("Misgrab detected: '{}'", grab.torrent_name),
            &format!(
                "series_id={}, hash={}, files={:?}",
                grab.series_id, grab.hash, detail.files
            ),
        )
        .await;
    } else if wrote {
        logger::debug(
            db,
            LogCategory::Grab,
            &format!(
                "Grab verified as {} : '{}'",
                verdict.as_str(),
                grab.torrent_name
            ),
            &detail.reason,
        )
        .await;
    }
    verdict
}

/// Resolve the aliases for a grab and judge it. Used by the sweep and
/// the import path; the grab-time spawn already holds a detail and
/// calls `assess_and_stamp` directly.
pub async fn assess_grab(db: &SqlitePool, grab: &GrabbedTorrent, filenames: &[String]) -> Verdict {
    match aliases_for_grab(db, grab, filenames).await {
        Some(aliases) => assess_and_stamp(db, grab, filenames, &aliases).await,
        None => {
            let verdict = Verdict::Unverifiable {
                reason: "series row is gone",
            };
            let detail = serde_json::to_string(&verdict.detail(filenames)).unwrap_or_default();
            let _ =
                grabbed_torrents::stamp_verification(db, grab.id, verdict.as_str(), &detail).await;
            verdict
        }
    }
}

// ── Sweep and remediation ────────────────────────────────────────────

/// `try_lock` shape: the supervised tick and any manual trigger share
/// this so a second sweep returns "already running" instead of racing
/// the client deletes.
static MISGRAB_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// How long the sweep tolerates client errors on `get_files` before
/// giving up on a grab.
const CLIENT_ERROR_GRACE: Duration = Duration::from_secs(30 * 60);
/// Misgrabs per series in a day before the automatic re-search stops,
/// so a series whose every candidate is wrong cannot loop.
pub const RESEARCH_LOOP_BREAKER: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MisgrabAction {
    /// Deleted from the client (with data), blocklisted.
    Removed,
    /// Blocklisted, but the torrent stays because seed rules apply, the
    /// client is unreachable, or it is a usenet job.
    RemovedNoDelete,
    /// Auto-remove is off: flagged and held, nothing touched.
    Flagged,
}

impl MisgrabAction {
    pub fn as_str(self) -> &'static str {
        match self {
            MisgrabAction::Removed => "removed",
            MisgrabAction::RemovedNoDelete => "removed_no_delete",
            MisgrabAction::Flagged => "flagged",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MisgrabSweepSummary {
    pub verified: usize,
    pub misgrabs: usize,
    pub unverifiable: usize,
    pub remediated: usize,
}

/// Seconds since the row's `grabbed_at` (SQLite `CURRENT_TIMESTAMP`
/// form). `None` when the timestamp does not parse.
fn grab_age_secs(grab: &GrabbedTorrent) -> Option<i64> {
    let at = chrono::NaiveDateTime::parse_from_str(&grab.grabbed_at, "%Y-%m-%d %H:%M:%S").ok()?;
    Some((chrono::Utc::now().naive_utc() - at).num_seconds())
}

fn older_than(grab: &GrabbedTorrent, age: Duration) -> bool {
    grab_age_secs(grab).is_some_and(|s| s >= age.as_secs() as i64)
}

async fn stamp_unverifiable(
    db: &SqlitePool,
    grab: &GrabbedTorrent,
    reason: &'static str,
) -> Verdict {
    let verdict = Verdict::Unverifiable { reason };
    let detail = serde_json::to_string(&verdict.detail(&[])).unwrap_or_default();
    let _ = grabbed_torrents::stamp_verification(db, grab.id, verdict.as_str(), &detail).await;
    verdict
}

/// Ask the client for the grab's file list and judge it. `None` means
/// "not yet": metadata is still on its way, or the client is having a
/// moment, and the next tick tries again until the grace period ends.
pub async fn verify_pending_grab(state: &AppState, grab: &GrabbedTorrent) -> Option<Verdict> {
    let Some(client) = state
        .resolve_grab_client(grab.download_client_id, &grab.hash)
        .await
    else {
        if older_than(grab, METADATA_GRACE) {
            return Some(
                stamp_unverifiable(&state.db, grab, "no download client for this grab").await,
            );
        }
        return None;
    };
    if client.protocol() == "usenet" {
        // SAB reports article names, not media file names.
        return Some(stamp_unverifiable(&state.db, grab, "usenet jobs are not checked").await);
    }
    match client.get_files(&grab.hash).await {
        Ok(files) if files.is_empty() => {
            if older_than(grab, METADATA_GRACE) {
                Some(
                    stamp_unverifiable(&state.db, grab, "no metadata within the grace period")
                        .await,
                )
            } else {
                None
            }
        }
        Ok(files) => {
            let names: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
            Some(assess_grab(&state.db, grab, &names).await)
        }
        Err(e) => {
            if older_than(grab, CLIENT_ERROR_GRACE) {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Could not verify '{}': download client error",
                        grab.torrent_name
                    ),
                    &e,
                )
                .await;
                Some(stamp_unverifiable(&state.db, grab, "download client error").await)
            } else {
                None
            }
        }
    }
}

/// Whether the automatic re-search may run for the series right now.
pub async fn research_allowed(db: &SqlitePool, series_id: i64) -> bool {
    grabbed_torrents::count_recent_misgrabs(db, series_id, 24).await < RESEARCH_LOOP_BREAKER
}

/// Act on a detected misgrab. With auto-remove on: delete from the
/// client (unless seed rules apply), blocklist by hash and title, fail
/// the history rows, notify, and re-search. With it off: flag, notify,
/// and hold. `research` lets tests and Dismiss skip the re-search.
pub async fn remediate(
    state: &AppState,
    grab: &GrabbedTorrent,
    research: bool,
) -> Result<MisgrabAction, String> {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let action = if !cfg.misgrab_auto_remove {
        MisgrabAction::Flagged
    } else {
        let mut action = MisgrabAction::Removed;
        match state
            .resolve_grab_client(grab.download_client_id, &grab.hash)
            .await
        {
            Some(client) if client.protocol() != "usenet" => {
                if grabbed_torrents::respects_seed_rules(&state.db, &grab.hash).await {
                    logger::info(
                        &state.db,
                        LogCategory::DownloadClient,
                        &format!(
                            "Keeping misgrab '{}' in the client (respect_seed_rules)",
                            grab.torrent_name
                        ),
                        &grab.hash,
                    )
                    .await;
                    action = MisgrabAction::RemovedNoDelete;
                } else if let Err(e) = client.delete(&grab.hash, true).await {
                    logger::warn(
                        &state.db,
                        LogCategory::DownloadClient,
                        &format!(
                            "Failed to remove misgrab '{}' from the client",
                            grab.torrent_name
                        ),
                        &e,
                    )
                    .await;
                    action = MisgrabAction::RemovedNoDelete;
                }
            }
            Some(_) => action = MisgrabAction::RemovedNoDelete,
            None => {
                logger::warn(
                    &state.db,
                    LogCategory::DownloadClient,
                    &format!(
                        "No download client to remove misgrab '{}' from",
                        grab.torrent_name
                    ),
                    &grab.hash,
                )
                .await;
                action = MisgrabAction::RemovedNoDelete;
            }
        }
        grabbed_torrents::mark_failed_by_hash_with_reason(&state.db, &grab.hash, "misgrab")
            .await
            .map_err(|e| format!("blocklist misgrab: {e}"))?;
        let _ = episode_tags::mark_grab_failed_for_release(
            &state.db,
            grab.series_id,
            &grab.torrent_name,
        )
        .await;
        action
    };
    grabbed_torrents::set_misgrab_action(&state.db, grab.id, action.as_str())
        .await
        .map_err(|e| format!("record misgrab action: {e}"))?;

    let detail = grabbed_torrents::get_verification_detail(&state.db, grab.id).await;
    logger::warn(
        &state.db,
        LogCategory::Grab,
        &format!(
            "Misgrab {}: '{}'",
            match action {
                MisgrabAction::Removed => "removed and blocklisted",
                MisgrabAction::RemovedNoDelete => "blocklisted, torrent kept",
                MisgrabAction::Flagged => "flagged and held",
            },
            grab.torrent_name
        ),
        &format!(
            "series_id={}, hash={}, files={:?}",
            grab.series_id, grab.hash, detail.files
        ),
    )
    .await;
    notifications::emit_misgrabbed(
        state,
        grab.series_id,
        &grab.torrent_name,
        &grab.hash,
        detail.files,
        action.as_str(),
    )
    .await;

    if research && action != MisgrabAction::Flagged {
        schedule_research(state, grab).await;
    }
    Ok(action)
}

/// Kick off the search that fills the slot the misgrab vacated. The
/// blocklist snapshot in the collector keeps the same release from
/// winning again; the loop breaker keeps a hopeless series from
/// searching forever.
async fn schedule_research(state: &AppState, grab: &GrabbedTorrent) {
    if !research_allowed(&state.db, grab.series_id).await {
        logger::warn(
            &state.db,
            LogCategory::AutoSearch,
            &format!(
                "Not re-searching after misgrab '{}': {} misgrabs for this series in 24h",
                grab.torrent_name, RESEARCH_LOOP_BREAKER
            ),
            "the series needs a look; check its aliases and the configured indexers",
        )
        .await;
        return;
    }
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
                        "Re-search after misgrab '{}' grabbed {} release(s)",
                        title, n
                    ),
                    &format!("series_id={series_id}"),
                )
                .await
            }
            Err((_, e)) => {
                logger::warn(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!("Re-search after misgrab '{}' failed", title),
                    &e,
                )
                .await
            }
        }
    });
}

/// One tick: verify what has not been checked, remediate what has been
/// detected. Split from the interval loop so tests can drive a single
/// tick against a seeded database.
pub async fn sweep_once(state: &AppState) -> Result<MisgrabSweepSummary, String> {
    sweep_once_with(state, true).await
}

pub async fn sweep_once_with(
    state: &AppState,
    research: bool,
) -> Result<MisgrabSweepSummary, String> {
    let _guard = MISGRAB_LOCK
        .try_lock()
        .map_err(|_| "misgrab sweep is already running".to_string())?;
    let mut summary = MisgrabSweepSummary::default();
    let pending = grabbed_torrents::list_unverified_pending(&state.db, MIN_AGE_SECS)
        .await
        .map_err(|e| format!("list unverified grabs: {e}"))?;
    for grab in &pending {
        match verify_pending_grab(state, grab).await {
            Some(Verdict::Verified { .. }) => summary.verified += 1,
            Some(Verdict::Misgrab { .. }) => summary.misgrabs += 1,
            Some(Verdict::Unverifiable { .. }) => summary.unverifiable += 1,
            None => {}
        }
    }
    let unhandled = grabbed_torrents::list_unhandled_misgrabs(&state.db)
        .await
        .map_err(|e| format!("list misgrabs: {e}"))?;
    for grab in &unhandled {
        match remediate(state, grab, research).await {
            Ok(_) => summary.remediated += 1,
            Err(e) => {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    &format!("Misgrab remediation failed for '{}'", grab.torrent_name),
                    &e,
                )
                .await;
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        db
    }

    async fn seed(db: &SqlitePool, anilist_id: i64, title: &str, romaji: &str) -> i64 {
        let (id, _) = series::upsert(
            db,
            series::SeriesCore {
                anilist_id,
                mal_id: None,
                title,
                title_romaji: romaji,
                title_english: "",
                title_native: "",
                cover_url: "",
                format: "OVA",
                status: "FINISHED",
                episodes: Some(1),
                season_year: Some(2016),
                end_year: None,
            },
        )
        .await
        .unwrap();
        id
    }

    const GRISAIA: &str = "[Xonline] Grisaia Phantom Trigger The Animation - 02 (BD 1920p x.264-10Bit Flac) [02964F5A].mkv";

    #[tokio::test]
    async fn assess_grab_falls_back_to_series_titles_when_no_cache_and_stamps_once() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let id = grabbed_torrents::record_grab(&db, "abcd", "[Xonline] Grisaia", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        let grab = grabbed_torrents::get_by_id(&db, id).await.unwrap().unwrap();
        let files = vec![GRISAIA.to_string()];
        let verdict = assess_grab(&db, &grab, &files).await;
        assert!(verdict.is_misgrab(), "{verdict:?}");
        assert_eq!(
            grabbed_torrents::get_verification(&db, id).await.as_deref(),
            Some("misgrab")
        );
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE message LIKE 'Misgrab detected:%'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(n, 1);

        // A second look does not re-stamp or re-log.
        let again = assess_grab(&db, &grab, &files).await;
        assert!(again.is_misgrab());
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE message LIKE 'Misgrab detected:%'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(n, 1);

        let legit = vec![
            "[H-Enc] Kowaremono Risa The Animation 01-02/Kowaremono Risa The Animation - 01.mkv"
                .to_string(),
        ];
        let id2 =
            grabbed_torrents::record_grab(&db, "ef01", "[H-Enc] Kowaremono", sid, &[1], false)
                .await
                .unwrap()
                .unwrap();
        let grab2 = grabbed_torrents::get_by_id(&db, id2)
            .await
            .unwrap()
            .unwrap();
        let verdict = assess_grab(&db, &grab2, &legit).await;
        assert!(matches!(verdict, Verdict::Verified { .. }), "{verdict:?}");
        assert_eq!(
            grabbed_torrents::get_verification(&db, id2)
                .await
                .as_deref(),
            Some("verified")
        );
    }

    #[tokio::test]
    async fn assess_and_stamp_honors_whitelist_by_hash() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let old = grabbed_torrents::record_grab(&db, "feed", "[Xonline] Grisaia", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        grabbed_torrents::whitelist_by_hash(&db, "feed")
            .await
            .unwrap();
        let _ = old;
        // The restored torrent is a new row with the same hash.
        grabbed_torrents::mark_failed_by_hash_with_reason(&db, "feed", "misgrab")
            .await
            .unwrap();
        let fresh =
            grabbed_torrents::record_grab(&db, "feed", "[Xonline] Grisaia", sid, &[1], false)
                .await
                .unwrap()
                .unwrap();
        let grab = grabbed_torrents::get_by_id(&db, fresh)
            .await
            .unwrap()
            .unwrap();
        let aliases = AliasSet {
            own: vec!["Kowaremono: Risa THE ANIMATION".to_string()],
            siblings: Vec::new(),
            expected_season: 0,
        };
        let verdict = assess_and_stamp(&db, &grab, &[GRISAIA.to_string()], &aliases).await;
        assert!(matches!(verdict, Verdict::Verified { .. }), "{verdict:?}");
        assert_eq!(
            grabbed_torrents::get_verification(&db, fresh)
                .await
                .as_deref(),
            Some("whitelisted")
        );
    }

    // ── Sweep and remediation ────────────────────────────────────────

    use crate::services::download_client::{
        AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
    };
    use crate::test_support::build_test_app_state;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct FakeClient {
        files: Vec<DownloadFile>,
        files_error: bool,
        protocol: &'static str,
        delete_calls: Mutex<Vec<(String, bool)>>,
    }

    impl FakeClient {
        fn with_files(names: &[&str]) -> Self {
            FakeClient {
                files: names
                    .iter()
                    .map(|n| DownloadFile {
                        name: n.to_string(),
                        size: 1_000_000,
                        progress: 0.0,
                        wanted: true,
                    })
                    .collect(),
                files_error: false,
                protocol: "torrent",
                delete_calls: Mutex::new(Vec::new()),
            }
        }
        fn deletes(&self) -> Vec<(String, bool)> {
            self.delete_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DownloadClient for FakeClient {
        async fn test(&self) -> Result<String, String> {
            Ok("fake".into())
        }
        async fn add_torrent(&self, _url: &str, _hash: &str) -> Result<AddOutcome, String> {
            Ok(AddOutcome::Added)
        }
        async fn add_torrent_with_file_filter(
            &self,
            _url: &str,
            _hash: &str,
            _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
        ) -> Result<SelectiveOutcome, String> {
            Ok(SelectiveOutcome::FullDownload)
        }
        async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
            Ok(vec![])
        }
        async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
            if self.files_error {
                Err("simulated client error".into())
            } else {
                Ok(self.files.clone())
            }
        }
        async fn pause(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
        async fn delete(&self, hash: &str, delete_files: bool) -> Result<(), String> {
            self.delete_calls
                .lock()
                .unwrap()
                .push((hash.to_string(), delete_files));
            Ok(())
        }
        async fn set_file_wanted(
            &self,
            _hash: &str,
            _files: &[usize],
            _wanted: bool,
        ) -> Result<(), String> {
            Ok(())
        }
        fn sonarr_impl_name(&self) -> &'static str {
            "QBittorrent"
        }
        fn protocol(&self) -> &'static str {
            self.protocol
        }
    }

    const GRISAIA_FILES: &[&str] = &[
        "[Xonline] Grisaia Phantom Trigger The Animation/[Xonline] Grisaia Phantom Trigger The Animation - 01 (BD 1920p x.264-10Bit Flac) [2E112DAF].mkv",
        "[Xonline] Grisaia Phantom Trigger The Animation/[Xonline] Grisaia Phantom Trigger The Animation - 02 (BD 1920p x.264-10Bit Flac) [02964F5A].mkv",
    ];
    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A pending grab old enough for the sweep to look at.
    async fn seed_grab(db: &SqlitePool, sid: i64, name: &str, age_secs: i64) -> i64 {
        let id = grabbed_torrents::record_grab(db, HASH, name, sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE grabbed_torrents SET grabbed_at = datetime('now', ? || ' seconds') WHERE id = ?")
            .bind(format!("-{age_secs}"))
            .bind(id)
            .execute(db)
            .await
            .unwrap();
        id
    }

    async fn state_with(db: SqlitePool, client: Arc<FakeClient>) -> AppState {
        build_test_app_state(db, Some(client as Arc<dyn DownloadClient>))
    }

    #[tokio::test]
    async fn sweep_stamps_verified_when_files_match() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let id = seed_grab(&db, sid, "[H-Enc] Kowaremono Risa", 60).await;
        let client = Arc::new(FakeClient::with_files(&[
            "[H-Enc] Kowaremono Risa The Animation 01-02/Kowaremono Risa The Animation - 01.mkv",
        ]));
        let state = state_with(db.clone(), client.clone()).await;
        let summary = sweep_once_with(&state, false).await.unwrap();
        assert_eq!(summary.verified, 1);
        assert_eq!(summary.misgrabs, 0);
        assert_eq!(
            grabbed_torrents::get_verification(&db, id).await.as_deref(),
            Some("verified")
        );
        assert!(client.deletes().is_empty());
        // Second tick: nothing left to verify.
        let summary = sweep_once_with(&state, false).await.unwrap();
        assert_eq!(summary, MisgrabSweepSummary::default());
    }

    #[tokio::test]
    async fn sweep_removes_blocklists_and_marks_action_for_misgrab() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let id = seed_grab(
            &db,
            sid,
            "[Xonline] Grisaia Phantom Trigger The Animation",
            60,
        )
        .await;
        episode_tags::record_grab(
            &db,
            sid,
            1,
            &crate::services::source::ClassificationResult::unknown(),
            "[Xonline] Grisaia Phantom Trigger The Animation",
            "Xonline",
            0,
            false,
        )
        .await
        .unwrap();
        let client = Arc::new(FakeClient::with_files(GRISAIA_FILES));
        let state = state_with(db.clone(), client.clone()).await;

        let summary = sweep_once_with(&state, false).await.unwrap();
        assert_eq!(summary.misgrabs, 1);
        assert_eq!(summary.remediated, 1, "detected and remediated in one tick");
        assert_eq!(client.deletes(), vec![(HASH.to_string(), true)]);

        let row = grabbed_torrents::get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.verification.as_deref(), Some("misgrab"));
        assert_eq!(row.misgrab_action.as_deref(), Some("removed"));
        let reason: String =
            sqlx::query_scalar("SELECT failure_reason FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(reason, "misgrab");
        assert!(grabbed_torrents::is_blocklisted_release(&db, sid, HASH, "").await);
        assert!(
            grabbed_torrents::is_blocklisted_release(
                &db,
                sid,
                "",
                "[Xonline] Grisaia Phantom Trigger The Animation"
            )
            .await,
            "title is blocklisted for the series too"
        );
        let hist: String = sqlx::query_scalar(
            "SELECT state FROM episode_grab_history WHERE series_id = ? AND episode_number = 1",
        )
        .bind(sid)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(hist, "failed");
        assert_eq!(
            grabbed_torrents::list_misgrabs(&db, "romaji")
                .await
                .unwrap()
                .len(),
            1,
            "on the review tab"
        );
        assert!(
            grabbed_torrents::get_all_pending(&db)
                .await
                .unwrap()
                .is_empty()
        );
        let logged: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM logs WHERE message LIKE 'Misgrab removed and blocklisted:%'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(logged, 1);
    }

    #[tokio::test]
    async fn sweep_skips_delete_when_seed_rules_respected() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let id = seed_grab(
            &db,
            sid,
            "[Xonline] Grisaia Phantom Trigger The Animation",
            60,
        )
        .await;
        grabbed_torrents::set_indexer_attribution(&db, id, Some(1), true)
            .await
            .unwrap();
        let client = Arc::new(FakeClient::with_files(GRISAIA_FILES));
        let state = state_with(db.clone(), client.clone()).await;
        sweep_once_with(&state, false).await.unwrap();
        assert!(client.deletes().is_empty(), "seed rules keep the torrent");
        let row = grabbed_torrents::get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.misgrab_action.as_deref(), Some("removed_no_delete"));
    }

    #[tokio::test]
    async fn sweep_flags_only_when_auto_remove_off() {
        let db = pool().await;
        let cfg = config::Config {
            misgrab_auto_remove: false,
            ..Default::default()
        };
        config::save_config(&db, &cfg).await.unwrap();
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let id = seed_grab(
            &db,
            sid,
            "[Xonline] Grisaia Phantom Trigger The Animation",
            60,
        )
        .await;
        let client = Arc::new(FakeClient::with_files(GRISAIA_FILES));
        let state = state_with(db.clone(), client.clone()).await;
        let summary = sweep_once_with(&state, false).await.unwrap();
        assert_eq!((summary.misgrabs, summary.remediated), (1, 1));
        assert!(client.deletes().is_empty());
        let row = grabbed_torrents::get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.state, "pending", "flagged rows keep downloading");
        assert_eq!(row.misgrab_action.as_deref(), Some("flagged"));
        assert!(
            grabbed_torrents::get_all_pending(&db)
                .await
                .unwrap()
                .is_empty(),
            "but never import"
        );
        assert_eq!(
            grabbed_torrents::list_misgrabs(&db, "romaji")
                .await
                .unwrap()[0]
                .status_label(),
            "Held in client, not imported"
        );
    }

    #[tokio::test]
    async fn sweep_waits_for_metadata_then_gives_up_after_the_grace_period() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let young = seed_grab(&db, sid, "[G] Fresh", 60).await;
        let client = Arc::new(FakeClient::with_files(&[]));
        let state = state_with(db.clone(), client).await;
        let summary = sweep_once_with(&state, false).await.unwrap();
        assert_eq!(summary, MisgrabSweepSummary::default());
        assert_eq!(
            grabbed_torrents::get_verification(&db, young).await,
            None,
            "still waiting"
        );

        sqlx::query(
            "UPDATE grabbed_torrents SET grabbed_at = datetime('now', '-20 minutes') WHERE id = ?",
        )
        .bind(young)
        .execute(&db)
        .await
        .unwrap();
        let summary = sweep_once_with(&state, false).await.unwrap();
        assert_eq!(summary.unverifiable, 1);
        assert_eq!(
            grabbed_torrents::get_verification(&db, young)
                .await
                .as_deref(),
            Some("unverifiable")
        );
    }

    #[tokio::test]
    async fn sweep_skips_usenet_grabs() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let id = seed_grab(&db, sid, "[Xonline] Grisaia", 60).await;
        let mut client = FakeClient::with_files(GRISAIA_FILES);
        client.protocol = "usenet";
        let client = Arc::new(client);
        let state = state_with(db.clone(), client.clone()).await;
        let summary = sweep_once_with(&state, false).await.unwrap();
        assert_eq!(summary.unverifiable, 1);
        assert_eq!(summary.misgrabs, 0);
        assert_eq!(
            grabbed_torrents::get_verification(&db, id).await.as_deref(),
            Some("unverifiable")
        );
        assert!(client.deletes().is_empty());
    }

    #[tokio::test]
    async fn sweep_retries_on_client_error_before_giving_up() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let id = seed_grab(&db, sid, "[G] Flaky", 60).await;
        let mut client = FakeClient::with_files(GRISAIA_FILES);
        client.files_error = true;
        let state = state_with(db.clone(), Arc::new(client)).await;
        sweep_once_with(&state, false).await.unwrap();
        assert_eq!(grabbed_torrents::get_verification(&db, id).await, None);
        sqlx::query(
            "UPDATE grabbed_torrents SET grabbed_at = datetime('now', '-40 minutes') WHERE id = ?",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        sweep_once_with(&state, false).await.unwrap();
        assert_eq!(
            grabbed_torrents::get_verification(&db, id).await.as_deref(),
            Some("unverifiable")
        );
    }

    #[tokio::test]
    async fn research_allowed_is_false_after_three_recent_misgrabs() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        assert!(research_allowed(&db, sid).await);
        for i in 0..RESEARCH_LOOP_BREAKER {
            let id = grabbed_torrents::record_grab(
                &db,
                &format!("{i:040}"),
                &format!("[G] Wrong {i}"),
                sid,
                &[1],
                false,
            )
            .await
            .unwrap()
            .unwrap();
            grabbed_torrents::stamp_verification(&db, id, "misgrab", "{}")
                .await
                .unwrap();
            grabbed_torrents::mark_failed_by_hash_with_reason(&db, &format!("{i:040}"), "misgrab")
                .await
                .unwrap();
        }
        assert!(!research_allowed(&db, sid).await);
    }
}
