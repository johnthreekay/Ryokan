//! SeaDex inside the RSS sync loop (`services::rss::sync_once`). Two
//! feed items for the same episode compete for one grab; the one whose
//! hash is a SeaDex pick for the series has to win it while the
//! `seadex_enabled` switch is on, as it would win a search.
//!
//! No network: the pick is seeded into the persisted SeaDex cache and
//! warmed into memory the way boot does it, so the sync's lookup is a
//! cache hit and never reaches releases.moe. Nyaa's own feed is off in
//! the seeded config, so the only source is the wiremock feed.

use async_trait::async_trait;
use ryokan::models::direct_rss_feeds::{self, DirectRssFeedForm};
use ryokan::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
};
use ryokan::services::rss::sync_once;
use ryokan::test_support::{build_test_app_state, in_memory_pool, seed_series};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ANILIST_ID: i64 = 9201;
/// Ahead on every heuristic: the preferred 1080p.
const PLAIN_TITLE: &str = "[Group] SeaDex Feed Show - 04 (1080p) [WEB].mkv";
const PLAIN_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Behind on resolution, but the series' SeaDex pick.
const PICK_TITLE: &str = "[Group] SeaDex Feed Show - 04 (720p) [WEB].mkv";
const PICK_HASH: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn feed_body() -> String {
    let item = |n: u32, title: &str, hash: &str| {
        format!(
            r#"<item>
<title>{title}</title>
<link>https://feed.example/view/{n}</link>
<guid>guid-seadex-{n}</guid>
<nyaa:magneturi>magnet:?xt=urn:btih:{hash}</nyaa:magneturi>
<nyaa:infohash>{hash}</nyaa:infohash>
</item>"#
        )
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa">
<channel>
{}
{}
</channel>
</rss>"#,
        item(1, PLAIN_TITLE, PLAIN_HASH),
        item(2, PICK_TITLE, PICK_HASH)
    )
}

struct RecordingClient {
    add_calls: Mutex<Vec<String>>,
}

#[async_trait]
impl DownloadClient for RecordingClient {
    async fn test(&self) -> Result<String, String> {
        Ok("mock".into())
    }
    async fn add_torrent(&self, _url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        self.add_calls.lock().unwrap().push(info_hash.to_string());
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

/// Run one sync over the two-item feed with SeaDex on or off and return
/// the hashes the client was asked to add.
async fn sync_with_seadex(seadex_enabled: bool) -> Vec<String> {
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
    ryokan::models::config::save_config(
        &db,
        &ryokan::models::config::Config {
            rss_enabled: false,
            seadex_enabled,
            preferred_resolution: "1080".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("seed config with the Nyaa feed off");
    let id = seed_series(&db, ANILIST_ID, "SeaDex Feed Show").await;
    sqlx::query("UPDATE series SET episodes = 12, monitor_mode = 'all' WHERE id = ?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
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

    // The pick, cached as a search would have left it.
    let payload = serde_json::json!({ "hashes": [PICK_HASH], "candidates": [] });
    sqlx::query(
        "INSERT INTO seadex_lookup_cache (anilist_id, payload_json, cached_at) \
         VALUES (?, ?, strftime('%s', 'now'))",
    )
    .bind(ANILIST_ID)
    .bind(payload.to_string())
    .execute(&db)
    .await
    .unwrap();
    ryokan::services::auto_search::seadex_warm_cache_from_db(&db).await;

    let client = Arc::new(RecordingClient {
        add_calls: Mutex::new(Vec::new()),
    });
    let state = build_test_app_state(db.clone(), Some(client.clone() as Arc<dyn DownloadClient>));
    let summary = sync_once(&state, "manual").await.expect("sync runs");
    let decisions = ryokan::models::rss::recent_decisions(&db, 10)
        .await
        .unwrap();
    assert_eq!(summary.items_seen, 2, "{summary:?}");
    assert_eq!(summary.grabbed, 1, "{summary:?}\n{decisions:#?}");
    client.add_calls.lock().unwrap().clone()
}

#[tokio::test]
async fn rss_grabs_the_seadex_pick_over_a_better_scored_release() {
    assert_eq!(sync_with_seadex(true).await, vec![PICK_HASH.to_string()]);
}

#[tokio::test]
async fn rss_grabs_the_better_scored_release_with_seadex_off() {
    // The control: with the switch off nothing is looked up and the
    // pick has no bonus, so the 1080p release wins. This is what shows
    // the test above was decided by SeaDex and not by the fixture.
    assert_eq!(sync_with_seadex(false).await, vec![PLAIN_HASH.to_string()]);
}
