//! Phase 1.5 grab-bag (issue #129) browser e2e tests.
//!
//! Two patterns under test:
//!
//! - **Row removal** (blocklist-remove): same shape as Phase 1
//!   settings deletes — confirm modal → click Yes → row stripped.
//!   Drives the real `/downloads?tab=blocklist` page so any drift
//!   between the row template and the production handler shows up.
//! - **Inline-result swap** (jellyfin/test, jellyfin/refresh,
//!   download-clients/test): button click → htmx posts the form →
//!   handler returns a small HTML fragment → swap into the result
//!   span. Drives a fixture page in `test_support::e2e` that mirrors
//!   the production button shape (see the fixture for the exact
//!   `hx-include` / `hx-target` attrs).
//!
//! Skips gracefully when the WebDriver endpoint is unreachable —
//! see `tests/htmx_browser_e2e.rs` for the run instructions.

use std::time::Duration;

use fantoccini::Locator;
use ryokan::test_support::{build_test_app_state, in_memory_pool, seed_series};

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_dom_contains, assert_htmx_handled_in_place, open_with_session, seed_user_session,
    spawn_app, try_connect_browser, wait_for_confirm_modal, wait_for_js_truthy,
    wait_for_row_removed, wait_until_substring,
};

#[tokio::test]
async fn blocklist_remove_swaps_row_out_in_browser() {
    // Seed two blocklist rows. The test deletes the first and asserts:
    //   (a) the deleted row is gone from the DOM
    //   (b) the surviving row is STILL in the DOM
    //   (c) the surviving DB row is untouched
    // Without (b), a stray `hx-target="closest div"` (which would swap
    // the entire containing div) passes silently — caught during a
    // mutation-testing pass while writing the e2e suite.
    let db = in_memory_pool().await;
    let series_id = seed_series(&db, 9001, "BrowserBlock Anime").await;
    sqlx::query(
        "INSERT INTO grabbed_torrents (series_id, hash, torrent_name, episode_numbers, state) \
         VALUES (?, ?, ?, '[1]', 'failed'), (?, ?, ?, '[2]', 'failed')",
    )
    .bind(series_id)
    .bind("blockhash-1")
    .bind("BrowserBlock.Doomed.Release")
    .bind(series_id)
    .bind("blockhash-2")
    .bind("BrowserBlock.Survivor.Release")
    .execute(&db)
    .await
    .expect("seed blocklist rows");

    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db.clone(), None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_with_session(&client, addr, &token, "/downloads?tab=blocklist").await?;

        // Click the row's delete button. The form sits inside the
        // row; we identify it by the unique torrent name in the
        // surrounding tr.
        client
            .execute(
                r#"
                const marker = 'BrowserBlock.Doomed.Release';
                const tr = Array.from(document.querySelectorAll('tr'))
                    .find(r => r.textContent.includes(marker));
                if (!tr) throw new Error('blocklist row not found');
                const btn = tr.querySelector('button[type="submit"]');
                if (!btn) throw new Error('row has no delete button');
                btn.click();
                "#,
                vec![],
            )
            .await?;

        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        client
            .find(Locator::Id("ryokan-confirm-yes"))
            .await?
            .click()
            .await?;

        wait_for_row_removed(
            &client,
            "BrowserBlock.Doomed.Release",
            Duration::from_secs(3),
        )
        .await?;
        // Survivor row stays — guard against over-broad swap target.
        assert_dom_contains(&client, "BrowserBlock.Survivor.Release").await?;
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/downloads")).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("blocklist remove e2e");

    // DB-side sanity.
    let remaining =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grabbed_torrents WHERE hash = ?")
            .bind("blockhash-1")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "row must actually be gone from the DB");
}

#[tokio::test]
async fn jellyfin_test_renders_failure_message_in_browser() {
    // Drives the connection-test fixture: bogus URL → handler returns
    // the failure partial → htmx swaps into #jellyfin-test-result.
    // Match on "127.0.0.1" specifically (the URL the fixture form
    // sends) — a generic "any text present" check would miss a
    // "rendered the wrong template" regression.
    let db = in_memory_pool().await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db, None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_with_session(&client, addr, &token, "/__test/connection-test-fixture").await?;
        client
            .find(Locator::Id("btn-jellyfin-test"))
            .await?
            .click()
            .await?;
        wait_until_substring(
            &client,
            "#jellyfin-test-result",
            "127.0.0.1",
            Duration::from_secs(5),
        )
        .await
    }
    .await;

    let _ = client.close().await;
    result.expect("jellyfin test e2e");
}

#[tokio::test]
async fn jellyfin_refresh_renders_not_configured_in_browser() {
    let db = in_memory_pool().await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db, None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_with_session(&client, addr, &token, "/__test/connection-test-fixture").await?;
        client
            .find(Locator::Id("btn-jellyfin-refresh"))
            .await?
            .click()
            .await?;
        wait_until_substring(
            &client,
            "#jellyfin-test-result",
            "not configured",
            Duration::from_secs(3),
        )
        .await
    }
    .await;

    let _ = client.close().await;
    result.expect("jellyfin refresh e2e");
}

/// A queue item id that ends an inline `onclick` JS string. HTML
/// escaping doesn't stop it there: the parser decodes `&#39;` back to
/// `'` before the handler runs.
const HOSTILE_HASH: &str = "x');window.__pwned=1;('";

/// One downloading queue item under [`HOSTILE_HASH`]; records pauses.
#[derive(Default)]
struct QueueClient {
    pauses: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ryokan::services::download_client::DownloadClient for QueueClient {
    async fn test(&self) -> Result<String, String> {
        Ok("mock".into())
    }
    async fn add_torrent(
        &self,
        _url: &str,
        _hash: &str,
    ) -> Result<ryokan::services::download_client::AddOutcome, String> {
        Ok(ryokan::services::download_client::AddOutcome::Added)
    }
    async fn add_torrent_with_file_filter(
        &self,
        _url: &str,
        _hash: &str,
        _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<ryokan::services::download_client::SelectiveOutcome, String> {
        Ok(ryokan::services::download_client::SelectiveOutcome::FullDownload)
    }
    async fn list_scoped(
        &self,
    ) -> Result<Vec<ryokan::services::download_client::DownloadItem>, String> {
        Ok(vec![ryokan::services::download_client::DownloadItem {
            hash: HOSTILE_HASH.to_string(),
            name: "Queue Show - 01".to_string(),
            size: 1 << 20,
            progress: 0.5,
            dlspeed: 0,
            state: "downloading".to_string(),
            category: "anime".to_string(),
            eta: 0,
            save_path: String::new(),
            content_path: String::new(),
            state_kind: ryokan::services::download_client::DownloadItemState::Downloading,
            seeding_done: false,
        }])
    }
    async fn get_files(
        &self,
        _hash: &str,
    ) -> Result<Vec<ryokan::services::download_client::DownloadFile>, String> {
        Ok(vec![])
    }
    async fn pause(&self, hash: &str) -> Result<(), String> {
        self.pauses.lock().unwrap().push(hash.to_string());
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

#[tokio::test]
async fn queue_row_actions_send_the_hash_as_data_not_code() {
    // The queue's buttons used to build `onclick="pauseTorrent('<id>')"`
    // from the client's id, so an id with a quote ran as script. Both
    // renderers (the server's first paint and renderQueue's poll) are
    // clicked, and each must post the id verbatim.
    let db = in_memory_pool().await;
    let token = seed_user_session(&db).await;
    let queue = std::sync::Arc::new(QueueClient::default());
    let state = build_test_app_state(db, Some(queue.clone()));
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result: Result<(), String> = async {
        open_with_session(&client, addr, &token, "/downloads")
            .await
            .map_err(|e| format!("open: {e}"))?;
        let pause = "#queue-container button[title=\"Pause\"]";
        for round in 1..=2 {
            if round == 2 {
                // Re-render through renderQueue, whose name cell has no
                // title attribute, and click its button.
                client
                    .execute("loadQueue();", vec![])
                    .await
                    .map_err(|e| format!("loadQueue: {e}"))?;
                wait_for_js_truthy(
                    &client,
                    "!document.querySelector('#queue-container .dl-torrent-name').hasAttribute('title')",
                    Duration::from_secs(5),
                )
                .await?;
            }
            client
                .find(Locator::Css(pause))
                .await
                .map_err(|e| format!("pause button: {e}"))?
                .click()
                .await
                .map_err(|e| format!("click: {e}"))?;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while queue.pauses.lock().unwrap().len() < round {
                if std::time::Instant::now() >= deadline {
                    return Err(format!("round {round}: no pause reached the client"));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let pwned = client
            .execute("return typeof window.__pwned;", vec![])
            .await
            .map_err(|e| format!("read marker: {e}"))?;
        if pwned.as_str() != Some("undefined") {
            return Err(format!("the id ran as script: {pwned:?}"));
        }
        Ok(())
    }
    .await;

    let _ = client.close().await;
    result.expect("queue actions e2e");
    assert_eq!(
        *queue.pauses.lock().unwrap(),
        vec![HOSTILE_HASH.to_string(), HOSTILE_HASH.to_string()]
    );
}

#[tokio::test]
async fn download_clients_test_renders_url_required_in_browser() {
    // The DC form fixture has an empty URL field; the handler should
    // surface "URL required" — easy assertion that doesn't depend on
    // a real qBit-style daemon.
    let db = in_memory_pool().await;
    let token = seed_user_session(&db).await;
    let state = build_test_app_state(db, None);
    let addr = spawn_app(state).await;

    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let result = async {
        open_with_session(&client, addr, &token, "/__test/connection-test-fixture").await?;
        client
            .find(Locator::Id("btn-dc-test"))
            .await?
            .click()
            .await?;
        wait_until_substring(
            &client,
            ".dc-test-result",
            "URL required",
            Duration::from_secs(3),
        )
        .await
    }
    .await;

    let _ = client.close().await;
    result.expect("dc test e2e");
}
