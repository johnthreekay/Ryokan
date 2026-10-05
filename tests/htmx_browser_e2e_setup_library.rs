//! Browser-e2e for the first-run library step at `/setup/library`.
//!
//! The page is a boosted form plus the Integrations tab's htmx Jellyfin
//! Test button, so only a browser shows that Save lands on the library
//! (via `HX-Redirect`, not a nested render), that Test answers in place,
//! and that Skip leaves the settings alone.
//!
//! Skips gracefully when WebDriver/geckodriver is unreachable.

use fantoccini::Locator;
use ryokan::models::config;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_loaded, open_with_session, seed_user_session, spawn_app, try_connect_browser,
    wait_for_path,
};

async fn fresh_state() -> (sqlx::SqlitePool, ryokan::AppState, String) {
    let db = in_memory_pool().await;
    config::save_config(&db, &config::Config::default())
        .await
        .expect("seed config");
    let session = seed_user_session(&db).await;
    let state = build_test_app_state(db.clone(), None);
    (db, state, session)
}

#[tokio::test]
async fn saving_the_library_step_lands_on_the_library_with_post_processing_on() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };
    let (db, state, session) = fresh_state().await;
    let addr = spawn_app(state).await;
    let root = tempfile::tempdir().expect("media root");
    let root_path = root.path().to_str().unwrap().to_string();

    open_with_session(&client, addr, &session, "/setup/library")
        .await
        .expect("open setup step");
    assert_htmx_loaded(&client).await.expect("htmx loaded");

    // The Jellyfin Test answers in place, from this page's form.
    client
        .execute(
            r#"
            document.getElementById('jellyfin_url').value = 'http://127.0.0.1:1';
            document.getElementById('jellyfin_api_key').value = 'some-key';
            "#,
            vec![],
        )
        .await
        .expect("fill jellyfin");
    client
        .find(Locator::XPath("//button[normalize-space()='Test']"))
        .await
        .expect("test button")
        .click()
        .await
        .expect("click test");
    let mut tested = String::new();
    for _ in 0..100 {
        tested = client
            .execute(
                "return document.getElementById('jellyfin-test-result').textContent.trim();",
                vec![],
            )
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        if !tested.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !tested.is_empty(),
        "the Test button must render its result in place"
    );
    assert_eq!(
        client.current_url().await.unwrap().path(),
        "/setup/library",
        "Test stays on the page"
    );

    // Save without Jellyfin.
    client
        .execute(
            r#"
            document.getElementById('jellyfin_url').value = '';
            document.getElementById('jellyfin_api_key').value = '';
            document.getElementById('media_root').value = arguments[0];
            document.getElementById('post_processing_mode').value = 'copy';
            "#,
            vec![serde_json::json!(root_path)],
        )
        .await
        .expect("fill the form");
    client
        .find(Locator::XPath(
            "//button[normalize-space()='Save and continue']",
        ))
        .await
        .expect("save button")
        .click()
        .await
        .expect("click save");
    wait_for_path(&client, "/", Duration::from_secs(5))
        .await
        .expect("Save lands on the library");
    // A real navigation, not the library nested inside the setup page.
    let nested = client
        .execute(
            "return document.querySelectorAll('.setup-library').length;",
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(nested.as_i64(), Some(0), "the setup page is gone");

    let cfg = config::get_config(&db).await.unwrap().unwrap();
    assert_eq!(cfg.media_root, root_path);
    assert!(cfg.post_processing_enabled);
    assert_eq!(cfg.post_processing_mode, "copy");

    let _ = client.close().await;
}

#[tokio::test]
async fn skipping_the_library_step_changes_nothing() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };
    let (db, state, session) = fresh_state().await;
    let addr = spawn_app(state).await;

    open_with_session(&client, addr, &session, "/setup/library")
        .await
        .expect("open setup step");
    assert_htmx_loaded(&client).await.expect("htmx loaded");
    client
        .execute(
            "document.getElementById('media_root').value = '/somewhere';",
            vec![],
        )
        .await
        .expect("type a path");
    client
        .find(Locator::LinkText("Skip for now"))
        .await
        .expect("skip link")
        .click()
        .await
        .expect("click skip");
    wait_for_path(&client, "/", Duration::from_secs(5))
        .await
        .expect("Skip lands on the library");

    let cfg = config::get_config(&db).await.unwrap().unwrap();
    assert!(cfg.media_root.is_empty());
    assert!(!cfg.post_processing_enabled);

    let _ = client.close().await;
}
