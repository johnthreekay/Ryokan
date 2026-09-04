//! Wiremock coverage for the blocklist gate inside the RSS sync loop
//! (`services::rss::sync_once`). A release the misgrab sweep removed,
//! or the user failed, must never be re-grabbed from a feed: by hash
//! when the item carries one, by exact title for the same series when
//! it does not. Drives a real direct feed through the whole loop so
//! the gate is pinned where it sits, after matching and scoring and
//! before the client add, rather than in isolation.
//!
//! No env var to coordinate: the feed URL is a table row. Nyaa's own
//! RSS stays off (`rss_enabled` defaults to false) so the only source
//! is the wiremock feed.

use async_trait::async_trait;
use ryokan::models::direct_rss_feeds::{self, DirectRssFeedForm};
use ryokan::models::{grabbed_torrents, rss};
use ryokan::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
};
use ryokan::services::rss::sync_once;
use ryokan::test_support::{build_test_app_state, in_memory_pool, seed_series};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TITLE_BLOCKED: &str = "[Group] Blocklist Feed Show - 01 (1080p) [WEB].mkv";
const HASH_BLOCKED_TITLE: &str = "[Other] Blocklist Feed Show - 02 (1080p) [WEB].mkv";
const HASH_BLOCKED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CLEAN_TITLE: &str = "[Group] Blocklist Feed Show - 03 (1080p) [WEB].mkv";
const CLEAN_HASH: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// Three items for three episodes of one series: the first is
/// blocklisted by title (its download link carries no hash), the
/// second by hash (magnet only), the third is clean.
fn feed_body() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa">
<channel>
<item>
<title>{TITLE_BLOCKED}</title>
<link>https://feed.example/view/1</link>
<guid>guid-feed-1</guid>
<nyaa:downloadurl>https://feed.example/torrent/1.torrent</nyaa:downloadurl>
</item>
<item>
<title>{HASH_BLOCKED_TITLE}</title>
<link>https://feed.example/view/2</link>
<guid>guid-feed-2</guid>
<nyaa:magneturi>magnet:?xt=urn:btih:{HASH_BLOCKED}</nyaa:magneturi>
<nyaa:infohash>{HASH_BLOCKED}</nyaa:infohash>
</item>
<item>
<title>{CLEAN_TITLE}</title>
<link>https://feed.example/view/3</link>
<guid>guid-feed-3</guid>
<nyaa:magneturi>magnet:?xt=urn:btih:{CLEAN_HASH}</nyaa:magneturi>
<nyaa:infohash>{CLEAN_HASH}</nyaa:infohash>
</item>
</channel>
</rss>"#
    )
}

struct RecordingClient {
    add_calls: Mutex<Vec<(String, String)>>,
}

impl RecordingClient {
    fn add_calls(&self) -> Vec<(String, String)> {
        self.add_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl DownloadClient for RecordingClient {
    async fn test(&self) -> Result<String, String> {
        Ok("mock".into())
    }
    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        self.add_calls
            .lock()
            .unwrap()
            .push((url.to_string(), info_hash.to_string()));
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
        "QBittorrent"
    }
}

/// A series whose first twelve episodes are monitored, so every feed
/// item above is wanted.
async fn seed_monitored_series(db: &sqlx::SqlitePool) -> i64 {
    let id = seed_series(db, 9101, "Blocklist Feed Show").await;
    sqlx::query("UPDATE series SET episodes = 12, monitor_mode = 'all' WHERE id = ?")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    id
}

/// The blocklist is `grabbed_torrents.state = 'failed'`; the sweep and
/// the user both write it that way.
async fn blocklist(db: &sqlx::SqlitePool, series_id: i64, hash: &str, title: &str) {
    let id = ryokan::test_support::seed_grabbed_torrent(db, series_id, hash, title, &[1]).await;
    sqlx::query(
        "UPDATE grabbed_torrents SET state = 'failed', failure_reason = 'misgrab' WHERE id = ?",
    )
    .bind(id)
    .execute(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn rss_sync_rejects_blocklisted_releases_by_hash_and_by_title() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(feed_body())
                .insert_header("content-type", "application/rss+xml"),
        )
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    let series_id = seed_monitored_series(&db).await;
    direct_rss_feeds::insert(
        &db,
        DirectRssFeedForm {
            name: "TestFeed",
            url: &format!("{}/feed", mock.uri()),
            enabled: true,
            download_client_id: None,
            request_timeout_secs: None,
        },
    )
    .await
    .unwrap();
    // By title: a failed row without a hash for this series.
    blocklist(&db, series_id, "", TITLE_BLOCKED).await;
    // By hash: a failed row under a different name, any series.
    blocklist(&db, series_id, HASH_BLOCKED, "[Old] Some Other Name").await;

    let client = Arc::new(RecordingClient {
        add_calls: Mutex::new(Vec::new()),
    });
    let state = build_test_app_state(db.clone(), Some(client.clone() as Arc<dyn DownloadClient>));

    let summary = sync_once(&state, "manual").await.expect("sync runs");
    let decisions = rss::recent_decisions(&db, 10).await.unwrap();
    assert_eq!(summary.items_seen, 3, "{summary:?}");
    assert_eq!(summary.matched, 3, "{summary:?}");
    assert_eq!(summary.grabbed, 1, "{summary:?}\n{decisions:#?}");
    assert_eq!(summary.skipped, 2, "{summary:?}");

    let calls = client.add_calls();
    assert_eq!(
        calls.len(),
        1,
        "only the clean item reaches the client: {calls:?}"
    );
    assert_eq!(calls[0].1, CLEAN_HASH);

    let rejected: Vec<&rss::RssDecision> = decisions
        .iter()
        .filter(|d| d.decision == "rejected")
        .collect();
    assert_eq!(rejected.len(), 2, "{decisions:?}");
    for d in &rejected {
        assert!(
            d.reason.starts_with("Blocklisted release"),
            "reason names the blocklist: {}",
            d.reason
        );
        assert_eq!(d.series_title, "Blocklist Feed Show");
    }
    let mut rejected_titles: Vec<&str> = rejected.iter().map(|d| d.title.as_str()).collect();
    rejected_titles.sort();
    assert_eq!(rejected_titles, vec![TITLE_BLOCKED, HASH_BLOCKED_TITLE]);
    let grabbed: Vec<&rss::RssDecision> = decisions
        .iter()
        .filter(|d| d.decision == "grabbed")
        .collect();
    assert_eq!(grabbed.len(), 1);
    assert_eq!(grabbed[0].title, CLEAN_TITLE);

    // Only the clean grab left a pending row; the failed rows are as
    // seeded, and the sweep's tombstone keeps the rejected items from
    // being re-evaluated next tick.
    let pending = grabbed_torrents::get_all_pending(&db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash, CLEAN_HASH);
    let failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE state = 'failed'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(failed, 2);
}

#[tokio::test]
async fn rss_sync_grabs_the_same_items_when_nothing_is_blocklisted() {
    // The control for the test above: the same feed with an empty
    // blocklist grabs all three, which proves the fixture clears every
    // other gate and the rejections really come from the blocklist.
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(feed_body())
                .insert_header("content-type", "application/rss+xml"),
        )
        .mount(&mock)
        .await;

    let db = in_memory_pool().await;
    seed_monitored_series(&db).await;
    direct_rss_feeds::insert(
        &db,
        DirectRssFeedForm {
            name: "TestFeed",
            url: &format!("{}/feed", mock.uri()),
            enabled: true,
            download_client_id: None,
            request_timeout_secs: None,
        },
    )
    .await
    .unwrap();
    let client = Arc::new(RecordingClient {
        add_calls: Mutex::new(Vec::new()),
    });
    let state = build_test_app_state(db.clone(), Some(client.clone() as Arc<dyn DownloadClient>));

    let summary = sync_once(&state, "manual").await.expect("sync runs");
    assert_eq!(summary.grabbed, 3, "{summary:?}");
    assert_eq!(client.add_calls().len(), 3);
}
