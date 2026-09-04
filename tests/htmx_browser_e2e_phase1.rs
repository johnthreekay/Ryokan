//! Phase 1 (per-row settings deletes) browser e2e tests.
//!
//! Each test seeds real DB rows, navigates the browser to the real
//! `/settings` page (not a fixture), exercises the row's delete form,
//! and asserts on the post-action DOM. This covers the same surface
//! as `tests/htmx_settings_delete.rs` but at the layer where
//! template-attribute drift, the `htmx:confirm` bridge in `base.js`,
//! and the actual swap behavior all matter.
//!
//! Skips gracefully when the WebDriver endpoint is unreachable —
//! see `tests/htmx_browser_e2e.rs` for the run instructions.
//!
//! Test plan covered here:
//!
//! - Indexers: confirm-then-delete removes the row from the DOM.
//! - Indexers: cancel-on-confirm leaves the row alone (regression guard
//!   for the "cancel modal silently submitted" bug fixed by the
//!   htmx:confirm bridge in `base.js`).
//! - Download clients: deleting the default auto-promotes the next-
//!   lowest-id row to default — verifies the `was_default` + `MIN(id)`
//!   promotion path in `models::download_clients::delete` end-to-end
//!   (the "Default" badge appears on the surviving row after reload).
//! - Custom formats: deleting the *last* CF triggers `HX-Refresh` so
//!   the empty-state CTA appears (per-row swap can't inject an empty-
//!   state since the empty-state lives outside the table loop).
//! - Groups: row delete (no confirm modal — direct htmx swap).

use std::time::Duration;

use fantoccini::Locator;
use ryokan::models::custom_formats as cf_model;
use ryokan::models::download_clients::{DownloadClientForm, insert as insert_download_client};
use ryokan::models::group_source_map;
use ryokan::models::indexers::{IndexerForm, insert as insert_indexer};
use ryokan::services::source::Source;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use sqlx::SqlitePool;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_dom_contains, assert_htmx_handled_in_place, assert_htmx_loaded, assert_modal_text,
    click_delete_for, open_with_session, seed_user_session, spawn_app, try_connect_browser,
    wait_for_confirm_modal, wait_for_row_removed,
};

// ─── File-local seed helpers ──────────────────────────────────────

async fn seed_indexer(db: &SqlitePool, name: &str) -> i64 {
    insert_indexer(
        db,
        IndexerForm {
            name,
            kind: "torznab",
            url: "https://example.com/torznab",
            api_key: "abc",
            priority: 25,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 1,
            request_timeout_secs: None,
            download_client_id: None,
            rss_enabled: false,
            categories: "",
        },
    )
    .await
    .expect("seed indexer")
}

async fn seed_dl_client(db: &SqlitePool, name: &str, is_default: bool) -> i64 {
    insert_download_client(
        db,
        DownloadClientForm {
            name,
            kind: "qbittorrent",
            url: "http://qbit.local",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default,
        },
    )
    .await
    .expect("seed download client")
}

async fn seed_cf(db: &SqlitePool, name: &str) -> i64 {
    cf_model::insert(db, name, None, "{}", 0, "manual")
        .await
        .expect("seed custom format")
}

async fn seed_group(db: &SqlitePool, name: &str) {
    group_source_map::upsert_user_edit(db, name, Source::BluRay, 1.0, "test seed")
        .await
        .expect("seed group");
}

// Convenience wrapper: every Phase 1 test starts on /settings?tab=<name>.
async fn open_settings(
    client: &fantoccini::Client,
    addr: std::net::SocketAddr,
    session_token: &str,
    tab: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    open_with_session(client, addr, session_token, &format!("/settings?tab={tab}")).await
}

// ─── Tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn indexers_delete_confirm_removes_row() {
    // Two indexers seeded so the test can also assert the survivor
    // row stays in the DOM. Without that, an over-broad `hx-target`
    // (e.g. `closest div` swapping the whole table) passes silently.
    let db = in_memory_pool().await;
    let _doomed = seed_indexer(&db, "Phase1Test-IndexerA").await;
    let _survivor = seed_indexer(&db, "Phase1Test-IndexerSurvivor").await;
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
        open_settings(&client, addr, &token, "indexers").await?;
        client
            .find(Locator::Css("button[type=\"submit\"].btn-danger"))
            .await?;
        click_delete_for(&client, "Phase1Test-IndexerA").await?;
        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        // Modal-copy regression guard: the row form's
        // `data-ryokan-confirm-*` attrs flow through `base.js`'s
        // `ryokanConfirmFromAttrs` → modal title/body. If that
        // pipeline regresses (e.g. wrong attr name, modal element
        // ID typo), the modal would render with default copy
        // ("Confirm" / "Are you sure?") instead of the indexer-
        // specific text.
        assert_modal_text(&client, "title", "Delete indexer?").await?;
        assert_modal_text(&client, "body", "Phase1Test-IndexerA").await?;
        client
            .find(Locator::Id("ryokan-confirm-yes"))
            .await?
            .click()
            .await?;
        wait_for_row_removed(&client, "Phase1Test-IndexerA", Duration::from_secs(3)).await?;
        assert_dom_contains(&client, "Phase1Test-IndexerSurvivor").await?;
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=indexers"))
            .await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("indexers delete confirm");

    let remaining = ryokan::models::indexers::list_all(&db)
        .await
        .expect("list indexers");
    assert_eq!(
        remaining.len(),
        1,
        "exactly one indexer (the survivor) must remain; got {remaining:?}"
    );
    assert_eq!(remaining[0].name, "Phase1Test-IndexerSurvivor");
}

#[tokio::test]
async fn indexers_delete_cancel_keeps_row() {
    // Regression guard for the "cancel modal silently submitted" bug.
    // Earlier shape: htmx's submit listener fired before base.js's,
    // so the AJAX was already in flight by the time `preventDefault()`
    // ran. Fixed by switching to the `htmx:confirm` event bridge.
    let db = in_memory_pool().await;
    let _id = seed_indexer(&db, "Phase1Test-IndexerB").await;
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
        open_settings(&client, addr, &token, "indexers").await?;
        // Sanity: htmx must be loaded before we drive the cancel
        // path — without htmx, there's no modal to cancel and the
        // form would submit natively, deleting the row.
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=indexers"))
            .await?;
        click_delete_for(&client, "Phase1Test-IndexerB").await?;
        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        client
            .find(Locator::Id("ryokan-confirm-no"))
            .await?
            .click()
            .await?;
        // Give htmx a beat to misbehave if the bridge is broken.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let still_present: bool = client
            .execute(
                r#"
                return Array.from(document.querySelectorAll('tr'))
                    .some(tr => tr.textContent.includes('Phase1Test-IndexerB'));
                "#,
                vec![],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if !still_present {
            return Err("cancel must leave the row in the DOM; modal-bridge regression?".into());
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("indexers delete cancel");

    let remaining = ryokan::models::indexers::list_all(&db)
        .await
        .expect("list indexers");
    assert_eq!(remaining.len(), 1, "cancel must NOT delete the indexer row");
}

#[tokio::test]
async fn download_clients_delete_default_auto_promotes_next() {
    let db = in_memory_pool().await;
    let _id_a = seed_dl_client(&db, "Phase1Test-DcA", true).await;
    let _id_b = seed_dl_client(&db, "Phase1Test-DcB", false).await;
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
        open_settings(&client, addr, &token, "downloads").await?;
        click_delete_for(&client, "Phase1Test-DcA").await?;
        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        client
            .find(Locator::Id("ryokan-confirm-yes"))
            .await?
            .click()
            .await?;
        wait_for_row_removed(&client, "Phase1Test-DcA", Duration::from_secs(3)).await?;
        assert_dom_contains(&client, "Phase1Test-DcB").await?;
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=downloads"))
            .await?;
        // Reload to re-render the default badges (the card swap alone
        // doesn't repaint sibling cards; auto-promotion is observable
        // in the DOM only after a fresh page load).
        let base = format!("http://{addr}");
        client
            .goto(&format!("{base}/settings?tab=downloads"))
            .await?;
        // DOM-side verification of the auto-promote (the DB-side
        // assertion below confirms B's `is_default = 1`, but a
        // template regression that doesn't re-render the badge would
        // pass the DB check and silently break the UI). Picker switched
        // from <tr> rows to <article class="dc-card"> in the
        // download-clients-tab redesign — match the card element.
        let badge_on_survivor: bool = client
            .execute(
                r#"
                const card = Array.from(document.querySelectorAll('article.dc-card'))
                    .find(c => c.textContent.includes('Phase1Test-DcB'));
                if (!card) return false;
                return card.textContent.toLowerCase().includes('default');
                "#,
                vec![],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if !badge_on_survivor {
            return Err(
                "surviving DC card does not show the `default` badge after auto-promote — \
                 template render path didn't reflect the DB change?"
                    .into(),
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("download clients delete-default + auto-promote");

    let surviving = ryokan::models::download_clients::list_all(&db)
        .await
        .expect("list download clients");
    assert_eq!(surviving.len(), 1, "deleted A should leave only B");
    assert_eq!(surviving[0].name, "Phase1Test-DcB");
    assert!(
        surviving[0].is_default,
        "B should have been auto-promoted to default after A's delete; got {surviving:?}"
    );
}

#[tokio::test]
async fn custom_formats_delete_last_triggers_hx_refresh() {
    // Deleting the only CF should send `HX-Refresh: true` so the
    // empty-state CTA ("Install bundled defaults") renders.
    // Otherwise per-row swap leaves an empty <tbody> with no CTA.
    let db = in_memory_pool().await;
    let _id = seed_cf(&db, "Phase1Test-CfSolo").await;
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
        open_settings(&client, addr, &token, "custom_formats").await?;
        // URL discrimination doesn't work here because HX-Refresh
        // legitimately navigates — so just check htmx is loaded.
        assert_htmx_loaded(&client).await?;
        click_delete_for(&client, "Phase1Test-CfSolo").await?;
        wait_for_confirm_modal(&client, Duration::from_secs(2)).await?;
        // Regression guard for PR 131 review (bug 1): the per-CF
        // delete form previously used `data-ryokan-confirm-label`,
        // which `base.js`'s `ryokanConfirmFromAttrs` ignores in favor
        // of `data-ryokan-confirm-yes`. The Yes button rendered as
        // the default "Yes" instead of "Delete." Pin the post-fix
        // text so an attribute-name regression triggers here.
        assert_modal_text(&client, "yes", "Delete").await?;
        client
            .find(Locator::Id("ryokan-confirm-yes"))
            .await?
            .click()
            .await?;
        wait_for_row_removed(&client, "Phase1Test-CfSolo", Duration::from_secs(5)).await?;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let cta_present: bool = client
                .execute(
                    r#"
                    return document.body.textContent.toLowerCase()
                        .includes('install bundled defaults');
                    "#,
                    vec![],
                )
                .await?
                .as_bool()
                .unwrap_or(false);
            if cta_present {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err("empty-state CTA did not appear after HX-Refresh".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("custom formats delete last + HX-Refresh");
}

#[tokio::test]
async fn groups_delete_removes_row_directly() {
    // Groups deliberately have NO confirm-modal wiring on the row's
    // delete form (the row is cheap to recreate; modal would be
    // friction). Click the button → htmx fires the POST directly →
    // row swap removes the row.
    let db = in_memory_pool().await;
    seed_group(&db, "Phase1Test-GroupDoomed").await;
    seed_group(&db, "Phase1Test-GroupSurvivor").await;
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
        open_settings(&client, addr, &token, "groups").await?;
        click_delete_for(&client, "Phase1Test-GroupDoomed").await?;
        wait_for_row_removed(&client, "Phase1Test-GroupDoomed", Duration::from_secs(3)).await?;
        assert_dom_contains(&client, "Phase1Test-GroupSurvivor").await?;
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=groups"))
            .await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("groups delete");

    let doomed = ryokan::models::group_source_map::get(&db, "Phase1Test-GroupDoomed")
        .await
        .expect("query doomed group");
    assert!(doomed.is_none(), "doomed group must be removed from DB");
    let survivor = ryokan::models::group_source_map::get(&db, "Phase1Test-GroupSurvivor")
        .await
        .expect("query survivor group");
    assert!(survivor.is_some(), "survivor group must still be in DB");
}
