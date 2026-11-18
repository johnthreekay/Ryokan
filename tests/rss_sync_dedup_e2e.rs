//! Wiremock coverage for the RSS sync loop's own dedup
//! (`services::rss::sync_once`): an item grabbed by one sync must stay
//! grabbed for as long as it sits in the feed.
//!
//! Same shape as `rss_sync_blocklist_e2e.rs`: a direct feed row points
//! at the mock, Nyaa's own RSS is switched off in the seeded config.

use async_trait::async_trait;
use ryokan::models::direct_rss_feeds::{self, DirectRssFeedForm};
use ryokan::models::rss;
use ryokan::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
};
use ryokan::services::rss::sync_once;
use ryokan::test_support::{build_test_app_state, in_memory_pool, seed_series};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `sync_once` takes the process-wide `RSS_SYNC_LOCK` with `try_lock`;
/// tests sharing a process (plain `cargo test`) hold this while they
/// sync, as in `rss_sync_blocklist_e2e.rs`.
static SYNC_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const HASHED_TITLE: &str = "[Group] Dedup Feed Show - 01 (1080p) [WEB].mkv";
const FEED_HASH: &str = "dddddddddddddddddddddddddddddddddddddddd";
const HASHLESS_TITLE: &str = "[Group] Dedup Feed Show - 02 (1080p) [WEB].mkv";

/// Two items for two episodes. The first is Nyaa's own shape: a
/// `.torrent` download link plus the info hash in its own tag. The
/// second carries no hash anywhere, so nothing but `rss_seen` can
/// dedup it.
fn feed_body() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa">
<channel>
<item>
<title>{HASHED_TITLE}</title>
<link>https://feed.example/view/1</link>
<guid>guid-dedup-1</guid>
<nyaa:downloadurl>https://feed.example/torrent/1.torrent</nyaa:downloadurl>
<nyaa:infoHash>{FEED_HASH}</nyaa:infoHash>
</item>
<item>
<title>{HASHLESS_TITLE}</title>
<link>https://feed.example/view/2</link>
<guid>guid-dedup-2</guid>
<nyaa:downloadurl>https://feed.example/torrent/2.torrent</nyaa:downloadurl>
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
    /// Empty on purpose: the client's queue is one of the later gates
    /// (`CanonicalHistory`), and a user whose finished torrents are
    /// removed from the client has nothing there either.
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

async fn seed(db: &sqlx::SqlitePool, feed_url: &str) {
    ryokan::models::config::save_config(
        db,
        &ryokan::models::config::Config {
            rss_enabled: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed config with the Nyaa feed off");
    let id = seed_series(db, 9201, "Dedup Feed Show").await;
    sqlx::query("UPDATE series SET episodes = 12, monitor_mode = 'all' WHERE id = ?")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    direct_rss_feeds::insert(
        db,
        DirectRssFeedForm {
            name: "TestFeed",
            url: feed_url,
            enabled: true,
            download_client_id: None,
            request_timeout_secs: None,
        },
    )
    .await
    .unwrap();
}

async fn mock_feed() -> MockServer {
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
    mock
}

/// A8-3. The feed keeps serving the same two items; only the first
/// sync may grab them. The "already grabbed" skip used to overwrite
/// the `grabbed` row with `skipped`, which took the item out of the
/// dedup set, so syncs alternated between grabbing and skipping.
#[tokio::test]
async fn an_item_grabbed_once_is_not_grabbed_again_while_it_stays_in_the_feed() {
    let _serial = SYNC_SERIAL.lock().await;
    let mock = mock_feed().await;
    let db = in_memory_pool().await;
    seed(&db, &format!("{}/feed", mock.uri())).await;
    let client = Arc::new(RecordingClient {
        add_calls: Mutex::new(Vec::new()),
    });
    let state = build_test_app_state(db.clone(), Some(client.clone() as Arc<dyn DownloadClient>));

    let first = sync_once(&state, "manual").await.expect("first sync");
    assert_eq!(first.grabbed, 2, "{first:?}");
    assert_eq!(client.add_calls().len(), 2);

    for round in 2..=4 {
        let again = sync_once(&state, "manual").await.expect("later sync");
        assert_eq!(again.grabbed, 0, "sync {round}: {again:?}");
        assert_eq!(again.skipped, 2, "sync {round}: {again:?}");
        assert_eq!(
            client.add_calls().len(),
            2,
            "sync {round} sent a release to the client again: {:?}",
            client.add_calls()
        );
        // The log keeps saying what happened to the item, and for
        // which series.
        let decisions = rss::recent_decisions(&db, 10).await.unwrap();
        assert_eq!(decisions.len(), 2, "{decisions:#?}");
        for d in &decisions {
            assert_eq!(d.decision, "grabbed", "sync {round}: {d:?}");
            assert_eq!(d.series_title, "Dedup Feed Show");
        }
    }

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(rows, 2, "one grab row per release");
}
