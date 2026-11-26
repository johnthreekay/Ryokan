//! The download queue both shims report (`GET /api/v3/queue` and
//! `GET /radarr/api/v3/queue`, #325).
//!
//! Seerr's download-sync job runs every minute for a server with
//! "Enable scan" on: it posts `RefreshMonitoredDownloads` to `/command`
//! (answered since the shim existed) and then reads `/queue`. With no
//! route it logged "Unable to get queue from Sonarr server" each minute
//! and showed no progress on a requested title.
//!
//! A record is a pending grab joined by hash to its item in the download
//! client. The grab row decides what is listed, so an imported download
//! leaves the queue while it is still seeding, as it does in Sonarr. The
//! client is the one post-processing would ask (`resolve_grab_client`),
//! so the two agree about where a download lives.
//!
//! Nothing here is cached: the job asks once a minute, and no client is
//! called at all while no grab is pending.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::UtcDateTime;

use crate::AppState;
use crate::models::grabbed_torrents::{self, GrabbedTorrent};
use crate::models::{download_clients, indexers};
use crate::services::datetime;
use crate::services::download_client::{DownloadClient, DownloadItem, DownloadItemState};

/// qBittorrent's "no estimate" (100 days). The Downloads page reads it
/// the same way (`downloads::format_eta`).
const ETA_UNKNOWN_SECS: i64 = 8_640_000;

/// Sonarr's page size when the caller names none is 10. Seerr names
/// none and reads only `records`, so against a real Sonarr it sees ten
/// downloads at most; here an unpaged request gets every record.
const DEFAULT_PAGE_SIZE: usize = 10;
const MAX_PAGE_SIZE: usize = 1000;

/// One download as the shims see it.
pub(crate) struct QueueEntry {
    pub grab: GrabbedTorrent,
    /// `None` when the grab's client did not answer.
    pub item: Option<DownloadItem>,
    pub client_name: String,
    pub protocol: &'static str,
    /// The indexer row's name, empty when the grab names none (Nyaa,
    /// a direct feed, an autobrr push that matched no row).
    pub indexer: String,
}

/// Every pending grab whose download can be placed: in its client's
/// queue, or in a client that did not answer. A grab whose client
/// answered without it (removed by hand, or never added) is left out,
/// as Sonarr drops a download its client no longer lists.
///
/// Ordered by time left, shortest first and unknown last, which is the
/// `sortKey` / `sortDirection` the page reports.
pub(crate) async fn load_entries(state: &AppState) -> Result<Vec<QueueEntry>, String> {
    let grabs = grabbed_torrents::get_all_pending(&state.db)
        .await
        .map_err(|e| e.to_string())?;

    // Legacy rows with no hash cannot be matched to an item.
    let mut placed: Vec<(GrabbedTorrent, i64)> = Vec::new();
    let mut clients: BTreeMap<i64, Arc<dyn DownloadClient>> = BTreeMap::new();
    for grab in grabs {
        if grab.hash.is_empty() {
            continue;
        }
        let Some((client_id, client)) = state
            .resolve_grab_client_with_id(grab.download_client_id, &grab.hash)
            .await
        else {
            continue;
        };
        clients.entry(client_id).or_insert(client);
        placed.push((grab, client_id));
    }
    if placed.is_empty() {
        return Ok(Vec::new());
    }

    // Only the clients a pending grab lives in, all at once: one slow
    // or dead client must not push the answer past Seerr's timeout.
    let lists = futures_util::future::join_all(
        clients
            .iter()
            .map(|(id, client)| async move { (*id, client.list_scoped().await) }),
    )
    .await;
    // `None` for a client that did not answer.
    let mut items: HashMap<i64, Option<HashMap<String, DownloadItem>>> = HashMap::new();
    for (id, list) in lists {
        match list {
            Ok(list) => {
                let by_hash = list
                    .into_iter()
                    .map(|item| (item.hash.to_lowercase(), item))
                    .collect();
                items.insert(id, Some(by_hash));
            }
            Err(e) => {
                // The Downloads page and post-processing already log an
                // unreachable client; a row a minute from here would
                // only repeat them.
                tracing::debug!("Queue shim: download client {id} did not answer: {e}");
                items.insert(id, None);
            }
        }
    }

    let client_names: HashMap<i64, String> = download_clients::list_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.id, row.name))
        .collect();
    let indexer_names: HashMap<i64, String> = indexers::list_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.id, row.name))
        .collect();

    let mut entries = Vec::new();
    for (grab, client_id) in placed {
        let item = match items.get(&client_id) {
            Some(Some(by_hash)) => match by_hash.get(&grab.hash.to_lowercase()) {
                Some(item) => Some(item.clone()),
                None => continue,
            },
            _ => None,
        };
        let client = &clients[&client_id];
        entries.push(QueueEntry {
            client_name: client_names
                .get(&client_id)
                .cloned()
                .unwrap_or_else(|| client.sonarr_impl_name().to_string()),
            protocol: client.protocol(),
            indexer: grab
                .indexer_id
                .and_then(|id| indexer_names.get(&id).cloned())
                .unwrap_or_default(),
            grab,
            item,
        });
    }
    entries.sort_by_key(|e| {
        let left = e.seconds_left();
        (left.is_none(), left.unwrap_or(0))
    });
    Ok(entries)
}

impl QueueEntry {
    /// Seconds until the download finishes: 0 once it has, `None` when
    /// the client gives no estimate or did not answer.
    fn seconds_left(&self) -> Option<i64> {
        let item = self.item.as_ref()?;
        if item.state_kind.is_complete() {
            return Some(0);
        }
        (item.eta > 0 && item.eta < ETA_UNKNOWN_SECS).then_some(item.eta)
    }

    /// The fields both shims report for this download.
    pub(crate) fn base(&self, now: UtcDateTime) -> QueueBase {
        let status = QueueStatus::of(self.item.as_ref());
        let (size, sizeleft) = match &self.item {
            Some(item) if item.state_kind.is_complete() => (item.size.max(0), 0),
            Some(item) => {
                let size = item.size.max(0);
                let left = (size as f64 * (1.0 - item.progress.clamp(0.0, 1.0))).round() as i64;
                (size, left.clamp(0, size))
            }
            None => (0, 0),
        };
        let left = self.seconds_left();
        QueueBase {
            size,
            title: match &self.item {
                Some(item) if !item.name.is_empty() => item.name.clone(),
                _ => self.grab.torrent_name.clone(),
            },
            sizeleft,
            timeleft: left.map(timespan),
            estimated_completion_time: left
                .and_then(|secs| now.checked_add(time::SignedDuration::seconds(secs)))
                .map(datetime::rfc3339),
            added: datetime::parse_sqlite(&self.grab.grabbed_at).map(datetime::rfc3339),
            status: status.status,
            tracked_download_status: status.tracked_status,
            tracked_download_state: status.tracked_state,
            status_messages: Vec::new(),
            error_message: status.error_message,
            download_id: download_id(&self.grab.hash),
            protocol: self.protocol,
            download_client: self.client_name.clone(),
            indexer: self.indexer.clone(),
        }
    }
}

/// Sonarr's `QueueResource` fields that do not depend on the kind of
/// media. Each shim flattens it into its own record.
///
/// `timeleft` and `estimatedCompletionTime` are left out when there is
/// no estimate, as Sonarr leaves out a null. Seerr builds a `Date` from
/// the field: a missing one is an invalid date its page shows as no
/// estimate, where a `null` would read as 1970.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueBase {
    pub size: i64,
    pub title: String,
    pub sizeleft: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeleft: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_completion_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<String>,
    pub status: &'static str,
    pub tracked_download_status: &'static str,
    pub tracked_download_state: &'static str,
    /// Always empty; present because Sonarr's own answer always has it.
    pub status_messages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<&'static str>,
    pub download_id: String,
    pub protocol: &'static str,
    pub download_client: String,
    pub indexer: String,
}

/// Sonarr's three status fields for one download.
struct QueueStatus {
    /// `QueueStatus`: what the client is doing with it.
    status: &'static str,
    /// `TrackedDownloadStatus`: `ok`, `warning` or `error`.
    tracked_status: &'static str,
    /// `TrackedDownloadState`: where it is on the way into the library.
    tracked_state: &'static str,
    error_message: Option<&'static str>,
}

impl QueueStatus {
    fn of(item: Option<&DownloadItem>) -> Self {
        use DownloadItemState as S;
        let plain = |status| Self {
            status,
            tracked_status: "ok",
            tracked_state: "downloading",
            error_message: None,
        };
        let Some(item) = item else {
            return Self {
                status: "downloadClientUnavailable",
                tracked_status: "warning",
                tracked_state: "downloading",
                error_message: Some("The download client did not answer"),
            };
        };
        match item.state_kind {
            S::Downloading | S::CheckingDownload => plain("downloading"),
            S::DownloadingQueued => plain("queued"),
            S::Paused => plain("paused"),
            // Sonarr reports a stalled torrent as a warning too.
            S::DownloadingStalled => Self {
                status: "warning",
                tracked_status: "warning",
                tracked_state: "downloading",
                error_message: Some("The download is stalled with no connections"),
            },
            S::Warning => Self {
                status: "warning",
                tracked_status: "warning",
                tracked_state: "downloading",
                error_message: Some("The download client reports a problem with this download"),
            },
            S::Errored => Self {
                status: "failed",
                tracked_status: "error",
                tracked_state: "failedPending",
                error_message: Some("The download client reports this download as failed"),
            },
            // Downloaded and still a pending grab: waiting for the next
            // post-processing pass.
            S::Seeding
            | S::SeedingStalled
            | S::SeedingQueued
            | S::CheckingSeed
            | S::PausedComplete => Self {
                status: "completed",
                tracked_status: "ok",
                tracked_state: "importPending",
                error_message: None,
            },
        }
    }
}

/// Seconds as a .NET `TimeSpan` (`hh:mm:ss`, days in front as `d.`),
/// the shape Sonarr writes `timeleft` in.
fn timespan(secs: i64) -> String {
    let secs = secs.max(0);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let clock = format!(
        "{:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    );
    if days > 0 {
        format!("{days}.{clock}")
    } else {
        clock
    }
}

/// The grab's hash as Sonarr writes a `downloadId`: a torrent info hash
/// in upper case, anything else (a SABnzbd `nzo_id`) as it is.
fn download_id(hash: &str) -> String {
    if matches!(hash.len(), 32 | 40 | 64) && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        hash.to_ascii_uppercase()
    } else {
        hash.to_string()
    }
}

/// A record id that stays the same from one poll to the next, as
/// Sonarr's does (it hashes the download id and the episode). 31-bit
/// FNV-1a, written out because `DefaultHasher` promises no stable
/// output.
pub(crate) fn record_id(download_id: &str, media_id: i64, episode: i32) -> i64 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in format!("{download_id}-{media_id}-{episode}").bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    i64::from(hash & 0x7fff_ffff)
}

/// The library rows a grab for `series_id` is reported under: its own,
/// then every other row of the same show, lowest id first.
///
/// `show_by_row` maps a row to the id the other app knows its show by
/// (TVDB show for Sonarr, TMDB movie for Radarr; 0 or absent when
/// unknown, which shares with nothing). Seerr keeps one server id per
/// title while Ryokan lists one entry per library row, and several rows
/// can be one show there (a season or a cour each): Seerr's scan keeps
/// the last row it read, an add answers with the earliest-aired one. A
/// download reported under its own row alone would show in Seerr only
/// when that row happened to be the one Seerr kept.
pub(crate) fn rows_sharing_show(series_id: i64, show_by_row: &BTreeMap<i64, i64>) -> Vec<i64> {
    let mut rows = vec![series_id];
    let show = show_by_row.get(&series_id).copied().unwrap_or(0);
    if show != 0 {
        rows.extend(
            show_by_row
                .iter()
                .filter(|(row, other)| **row != series_id && **other == show)
                .map(|(row, _)| *row),
        );
    }
    rows
}

/// `GET /queue` query. Every field is read as text and parsed
/// leniently: a value this shim does not understand falls back to the
/// default rather than a 400, which Seerr would log as a failed sync.
#[derive(Debug, Default, Deserialize)]
pub struct QueueQuery {
    page: Option<String>,
    #[serde(rename = "pageSize")]
    page_size: Option<String>,
    #[serde(rename = "includeEpisode")]
    include_episode: Option<String>,
}

impl QueueQuery {
    /// Sonarr attaches the episode only when asked (Seerr asks).
    pub(crate) fn include_episode(&self) -> bool {
        self.include_episode
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    }

    /// One page of `records`: all of them when the caller named no
    /// `pageSize`, else that page.
    pub(crate) fn page<T: Serialize>(&self, records: Vec<T>) -> QueuePage<T> {
        let total = records.len();
        let positive = |text: &Option<String>| {
            text.as_deref()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|n| *n > 0)
        };
        let (page, page_size, records) = match positive(&self.page_size) {
            Some(size) => {
                let size = size.min(MAX_PAGE_SIZE);
                let page = positive(&self.page).unwrap_or(1);
                let records = records
                    .into_iter()
                    .skip((page - 1).saturating_mul(size))
                    .take(size)
                    .collect();
                (page, size, records)
            }
            None => (1, total.max(DEFAULT_PAGE_SIZE), records),
        };
        QueuePage {
            page,
            page_size,
            sort_key: "timeleft",
            sort_direction: "ascending",
            total_records: total,
            records,
        }
    }
}

/// Sonarr's paging envelope. Seerr reads `records` only.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuePage<T: Serialize> {
    pub page: usize,
    pub page_size: usize,
    pub sort_key: &'static str,
    pub sort_direction: &'static str,
    pub total_records: usize,
    pub records: Vec<T>,
}

/// A download client for the shim queue tests: lists what it is given,
/// or fails.
#[cfg(test)]
pub(crate) mod test_client {
    use async_trait::async_trait;

    use crate::services::download_client::{
        AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
    };

    pub(crate) struct QueueClient {
        pub items: Option<Vec<DownloadItem>>,
        pub protocol: &'static str,
    }

    /// A 1,000-byte item at `progress` with no estimate.
    pub(crate) fn item(
        hash: &str,
        name: &str,
        progress: f64,
        kind: DownloadItemState,
    ) -> DownloadItem {
        DownloadItem {
            hash: hash.into(),
            name: name.into(),
            size: 1000,
            progress,
            dlspeed: 0,
            state: String::new(),
            category: "anime".into(),
            eta: 0,
            save_path: String::new(),
            content_path: String::new(),
            state_kind: kind,
            seeding_done: false,
        }
    }

    #[async_trait]
    impl DownloadClient for QueueClient {
        async fn test(&self) -> Result<String, String> {
            Ok("mock".into())
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
            self.items
                .clone()
                .ok_or_else(|| "connection refused".to_string())
        }
        async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
            Ok(vec![])
        }
        async fn pause(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
        async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
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
            if self.protocol == "usenet" {
                "Sabnzbd"
            } else {
                "QBittorrent"
            }
        }
        fn protocol(&self) -> &'static str {
            self.protocol
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_client::{QueueClient, item};
    use super::*;
    use crate::test_support::{
        build_test_app_state, in_memory_pool, seed_grabbed_torrent, seed_series,
    };

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn torrent_client(items: Option<Vec<DownloadItem>>) -> Arc<dyn DownloadClient> {
        Arc::new(QueueClient {
            items,
            protocol: "torrent",
        })
    }

    fn fixed_now() -> UtcDateTime {
        datetime::from_unix(1_800_000_000).unwrap()
    }

    #[tokio::test]
    async fn no_pending_grab_means_no_client_is_asked() {
        // A failing client would turn up as an unavailable entry if it
        // were asked for anything.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, Some(torrent_client(None)));
        assert!(load_entries(&state).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pending_grab_is_joined_to_its_item_by_hash_in_any_case() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        seed_grabbed_torrent(&db, series_id, HASH_A, "[G] Show - 03", &[3]).await;
        let listed = item(
            &HASH_A.to_uppercase(),
            "[G] Show - 03 [1080p].mkv",
            0.25,
            DownloadItemState::Downloading,
        );
        let state = build_test_app_state(db, Some(torrent_client(Some(vec![listed]))));

        let entries = load_entries(&state).await.unwrap();
        assert_eq!(entries.len(), 1);
        let base = entries[0].base(fixed_now());
        assert_eq!(base.title, "[G] Show - 03 [1080p].mkv");
        assert_eq!((base.size, base.sizeleft), (1000, 750));
        assert_eq!(base.status, "downloading");
        assert_eq!(base.tracked_download_state, "downloading");
        assert_eq!(base.download_id, HASH_A.to_uppercase());
        assert_eq!(base.protocol, "torrent");
        // No estimate from the client: both fields are left out.
        assert_eq!(base.timeleft, None);
        assert_eq!(base.estimated_completion_time, None);
        let json = serde_json::to_value(&base).unwrap();
        assert!(json.get("timeleft").is_none(), "{json}");
        assert!(json.get("estimatedCompletionTime").is_none(), "{json}");
    }

    #[tokio::test]
    async fn an_imported_grab_leaves_the_queue_while_it_still_seeds() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        let grab = seed_grabbed_torrent(&db, series_id, HASH_A, "[G] Show - 03", &[3]).await;
        sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE id = ?")
            .bind(grab)
            .execute(&db)
            .await
            .unwrap();
        let seeding = item(HASH_A, "[G] Show - 03", 1.0, DownloadItemState::Seeding);
        let state = build_test_app_state(db, Some(torrent_client(Some(vec![seeding]))));
        assert!(load_entries(&state).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_grab_its_client_no_longer_lists_is_left_out() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        seed_grabbed_torrent(&db, series_id, HASH_A, "[G] Show - 03", &[3]).await;
        let other = item(
            HASH_B,
            "something else",
            0.5,
            DownloadItemState::Downloading,
        );
        let state = build_test_app_state(db, Some(torrent_client(Some(vec![other]))));
        assert!(load_entries(&state).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_client_that_does_not_answer_keeps_its_grabs_listed_as_unavailable() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        seed_grabbed_torrent(&db, series_id, HASH_A, "[G] Show - 03", &[3]).await;
        let state = build_test_app_state(db, Some(torrent_client(None)));

        let entries = load_entries(&state).await.unwrap();
        assert_eq!(entries.len(), 1);
        let base = entries[0].base(fixed_now());
        assert_eq!(base.status, "downloadClientUnavailable");
        assert_eq!(base.tracked_download_status, "warning");
        // The grab's own name, and a size Seerr draws as 0%.
        assert_eq!(base.title, "[G] Show - 03");
        assert_eq!((base.size, base.sizeleft), (0, 0));
        assert_eq!(base.timeleft, None);
    }

    #[tokio::test]
    async fn a_grab_is_looked_up_in_the_client_it_was_sent_to() {
        // Two clients; the usenet one is down. The torrent grab is
        // listed from the client that answered, the usenet grab is
        // unavailable, and neither is read from the other's queue.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        seed_grabbed_torrent(&db, series_id, HASH_A, "[G] Show - 03", &[3]).await;
        let nzb = seed_grabbed_torrent(&db, series_id, "SABnzbd_nzo_abc", "Show.E04", &[4]).await;
        sqlx::query("UPDATE grabbed_torrents SET download_client_id = 2 WHERE id = ?")
            .bind(nzb)
            .execute(&db)
            .await
            .unwrap();

        let state = build_test_app_state(db, None);
        let mut pool = crate::DownloadClientPool {
            clients: std::collections::HashMap::new(),
            default_torrent_id: Some(1),
            default_usenet_id: Some(2),
        };
        let seeding = item(HASH_A, "[G] Show - 03", 1.0, DownloadItemState::Seeding);
        pool.clients.insert(1, torrent_client(Some(vec![seeding])));
        pool.clients.insert(
            2,
            Arc::new(QueueClient {
                items: None,
                protocol: "usenet",
            }),
        );
        *state.download_clients.write().await = Arc::new(pool);

        let entries = load_entries(&state).await.unwrap();
        let seen: Vec<(String, &str, &str)> = entries
            .iter()
            .map(|e| {
                let base = e.base(fixed_now());
                (base.download_id, base.status, base.protocol)
            })
            .collect();
        // Finished first (no time left), the unknown one last.
        assert_eq!(
            seen,
            vec![
                (HASH_A.to_uppercase(), "completed", "torrent"),
                (
                    "SABnzbd_nzo_abc".to_string(),
                    "downloadClientUnavailable",
                    "usenet"
                ),
            ]
        );
        let done = entries[0].base(fixed_now());
        assert_eq!(done.tracked_download_state, "importPending");
        assert_eq!(done.sizeleft, 0);
        assert_eq!(done.timeleft.as_deref(), Some("00:00:00"));
        assert_eq!(
            done.estimated_completion_time.as_deref(),
            Some("2027-01-15T08:00:00Z")
        );
    }

    #[tokio::test]
    async fn an_estimate_becomes_a_timespan_and_a_completion_time() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        seed_grabbed_torrent(&db, series_id, HASH_A, "[G] Show - 03", &[3]).await;
        seed_grabbed_torrent(&db, series_id, HASH_B, "[G] Show - 04", &[4]).await;
        let mut slow = item(HASH_A, "slow", 0.1, DownloadItemState::Downloading);
        slow.eta = 90_061;
        let mut quick = item(HASH_B, "quick", 0.9, DownloadItemState::Downloading);
        quick.eta = 754;
        let state = build_test_app_state(db, Some(torrent_client(Some(vec![slow, quick]))));

        let entries = load_entries(&state).await.unwrap();
        let bases: Vec<QueueBase> = entries.iter().map(|e| e.base(fixed_now())).collect();
        assert_eq!(bases[0].title, "quick", "shortest time left first");
        assert_eq!(bases[0].timeleft.as_deref(), Some("00:12:34"));
        assert_eq!(
            bases[0].estimated_completion_time.as_deref(),
            Some("2027-01-15T08:12:34Z")
        );
        assert_eq!(bases[1].timeleft.as_deref(), Some("1.01:01:01"));
    }

    #[test]
    fn every_client_state_maps_to_a_sonarr_status() {
        use DownloadItemState as S;
        let status = |kind| {
            let s = QueueStatus::of(Some(&item(HASH_A, "x", 0.5, kind)));
            (s.status, s.tracked_status, s.tracked_state)
        };
        assert_eq!(status(S::Downloading), ("downloading", "ok", "downloading"));
        assert_eq!(
            status(S::CheckingDownload),
            ("downloading", "ok", "downloading")
        );
        assert_eq!(
            status(S::DownloadingQueued),
            ("queued", "ok", "downloading")
        );
        assert_eq!(status(S::Paused), ("paused", "ok", "downloading"));
        assert_eq!(
            status(S::DownloadingStalled),
            ("warning", "warning", "downloading")
        );
        assert_eq!(status(S::Warning), ("warning", "warning", "downloading"));
        assert_eq!(status(S::Errored), ("failed", "error", "failedPending"));
        for done in [
            S::Seeding,
            S::SeedingStalled,
            S::SeedingQueued,
            S::CheckingSeed,
            S::PausedComplete,
        ] {
            assert_eq!(status(done), ("completed", "ok", "importPending"));
        }
    }

    #[test]
    fn qbittorrents_no_estimate_value_is_no_estimate() {
        let mut stuck = item(HASH_A, "x", 0.5, DownloadItemState::Downloading);
        stuck.eta = ETA_UNKNOWN_SECS;
        let entry = QueueEntry {
            grab: GrabbedTorrent {
                id: 1,
                hash: HASH_A.into(),
                torrent_name: "x".into(),
                series_id: 1,
                episode_numbers: vec![1],
                state: "pending".into(),
                grabbed_at: "2026-11-26 10:00:00".into(),
                is_batch: false,
                download_client_id: None,
                verification: None,
                misgrab_action: None,
                source_url: String::new(),
                indexer_id: None,
                respect_seed_rules: true,
            },
            item: Some(stuck),
            client_name: "qbit".into(),
            protocol: "torrent",
            indexer: String::new(),
        };
        let base = entry.base(fixed_now());
        assert_eq!(base.timeleft, None);
        assert_eq!(base.added.as_deref(), Some("2026-11-26T10:00:00Z"));
    }

    #[test]
    fn download_ids_follow_sonarr() {
        assert_eq!(download_id(HASH_A), HASH_A.to_uppercase());
        assert_eq!(download_id("SABnzbd_nzo_ab12cd"), "SABnzbd_nzo_ab12cd");
    }

    #[test]
    fn record_ids_are_stable_and_tell_records_apart() {
        let id = record_id("ABC", 7, 3);
        assert_eq!(id, record_id("ABC", 7, 3));
        assert!((0..=i64::from(i32::MAX)).contains(&id));
        assert_ne!(id, record_id("ABC", 8, 3));
        assert_ne!(id, record_id("ABC", 7, 4));
    }

    #[test]
    fn a_grab_is_reported_under_every_row_of_its_show() {
        let shows = BTreeMap::from([(1, 500), (2, 500), (3, 0), (4, 900), (5, 500)]);
        assert_eq!(rows_sharing_show(2, &shows), vec![2, 1, 5]);
        assert_eq!(rows_sharing_show(4, &shows), vec![4]);
        // No show known: its own row, never every other unknown one.
        assert_eq!(rows_sharing_show(3, &shows), vec![3]);
        assert_eq!(rows_sharing_show(9, &shows), vec![9]);
    }

    #[test]
    fn an_unpaged_request_gets_every_record() {
        let records: Vec<i32> = (1..=25).collect();
        let page = QueueQuery::default().page(records);
        assert_eq!((page.page, page.page_size, page.total_records), (1, 25, 25));
        assert_eq!(page.records.len(), 25);

        let empty = QueueQuery::default().page(Vec::<i32>::new());
        assert_eq!(
            (empty.page, empty.page_size, empty.total_records),
            (1, DEFAULT_PAGE_SIZE, 0)
        );
    }

    #[test]
    fn a_page_size_pages_and_a_bad_value_is_ignored() {
        let query = |page: &str, size: &str| QueueQuery {
            page: Some(page.into()),
            page_size: Some(size.into()),
            include_episode: None,
        };
        let records: Vec<i32> = (1..=25).collect();

        let second = query("2", "10").page(records.clone());
        assert_eq!(second.records, (11..=20).collect::<Vec<_>>());
        assert_eq!(
            (second.page, second.page_size, second.total_records),
            (2, 10, 25)
        );

        let past = query("9", "10").page(records.clone());
        assert!(past.records.is_empty());
        assert_eq!(past.total_records, 25);

        // Not a number, zero: read as no page size at all.
        assert_eq!(query("1", "lots").page(records.clone()).records.len(), 25);
        assert_eq!(query("1", "0").page(records).records.len(), 25);
    }

    #[test]
    fn the_episode_is_attached_only_when_asked() {
        let asked = |v: Option<&str>| {
            QueueQuery {
                include_episode: v.map(str::to_string),
                ..Default::default()
            }
            .include_episode()
        };
        assert!(asked(Some("true")));
        assert!(asked(Some("True")));
        assert!(!asked(Some("false")));
        assert!(!asked(None));
    }
}
