//! Browser-e2e for the library's add-series "Monitor Episodes" dialog.
//!
//! 65b545e changed `/api/library/monitoring` to answer htmx with a
//! `ryokan-monitoring-changed` event instead of `HX-Refresh`, so the
//! series page could update in place. The add flow's dialog relied on
//! the refresh: Confirm saved the mode, but the dialog stayed open over
//! a library that still read "0 series". The handler test can't see
//! this; only a browser runs the button's `hx-on` wiring.
//!
//! Skips gracefully when WebDriver/geckodriver is unreachable.

use ryokan::test_support::{build_test_app_state, in_memory_pool, seed_series};
use std::time::Duration;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_loaded, open_with_session, seed_user_session, spawn_app, try_connect_browser,
};

#[tokio::test]
async fn confirming_the_monitor_dialog_saves_and_closes_it() {
    let client = match try_connect_browser().await {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[skip] {msg}");
            return;
        }
    };

    let db = in_memory_pool().await;
    let series_id = seed_series(&db, 154587, "Frieren").await;
    sqlx::query("UPDATE series SET monitor_mode = 'none' WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .expect("seed monitor mode");
    let state = build_test_app_state(db.clone(), None);
    let session = seed_user_session(&db).await;
    let addr = spawn_app(state).await;

    open_with_session(&client, addr, &session, "/")
        .await
        .expect("open library");
    assert_htmx_loaded(&client).await.expect("htmx loaded");
    // index.js is deferred; it declares `_pendingSeriesId = null`, so
    // setting it before the script ran would be overwritten.
    let mut page_script_ready = false;
    for _ in 0..50 {
        page_script_ready = client
            .execute(
                "return typeof confirmMonitoringVals === 'function';",
                vec![],
            )
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if page_script_ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(page_script_ready, "index.js never loaded");

    // The state `addSeries` leaves behind before it shows the dialog:
    // the new series' id pending, "All Episodes" selected. Set through
    // an injected <script>, which runs in the page: geckodriver runs
    // `execute` in a sandbox whose global assignments the page's own
    // functions never see. A marker on `window` survives until the
    // page reloads.
    client
        .execute(
            r#"
            const s = document.createElement('script');
            s.textContent = '_pendingSeriesId = ' + Number(arguments[0])
                + '; _selectedMonitorMode = "all";';
            document.head.appendChild(s);
            document.getElementById('monitor-modal').style.display = 'flex';
            window.__beforeConfirm = true;
            document.getElementById('monitor-confirm-btn').click();
            return true;
            "#,
            vec![serde_json::json!(series_id)],
        )
        .await
        .expect("confirm the dialog");

    let mut closed_by_reload = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let done = client
            .execute(
                r#"
                return document.readyState === 'complete'
                    && window.__beforeConfirm === undefined
                    && getComputedStyle(document.getElementById('monitor-modal')).display === 'none';
                "#,
                vec![],
            )
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if done {
            closed_by_reload = true;
            break;
        }
    }
    assert!(
        closed_by_reload,
        "Confirm must reload the library so the dialog closes and the new series shows"
    );

    let mode: String = sqlx::query_scalar("SELECT monitor_mode FROM series WHERE id = ?")
        .bind(series_id)
        .fetch_one(&db)
        .await
        .expect("read monitor mode");
    assert_eq!(mode, "all", "the chosen mode is saved");

    let _ = client.close().await;
}
