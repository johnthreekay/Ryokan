//! Phase 1 completion (per-tab settings subforms) browser e2e tests.
//!
//! Each Phase 1 split (General / Quality / Integrations) replaced the
//! bulk `<form action="/settings">` that posted ALL three tabs'
//! fields together with a per-tab `<form hx-post="/settings/<tab>">`
//! that posts only its own tab's fields. The split's load-bearing
//! invariants:
//!
//!   1. **Persistence** — saving from a tab's subform writes that tab's
//!      fields to `config`.
//!   2. **In-place swap** — HTMX swaps the form region without a full
//!      page navigation; URL stays at `/settings?tab=<tab>` and the
//!      "Settings saved." toast / alert renders in the swapped region.
//!   3. **Cross-tab isolation** — saving from one tab doesn't clobber
//!      the other two tabs' fields. This is the property the per-tab
//!      split was supposed to give us; the legacy bulk form would
//!      submit empty defaults for any field whose tab panel wasn't
//!      currently active, which silently re-saved with empty values.
//!
//! Each test seeds a config row with distinct values across all three
//! tabs, navigates to one tab, mutates one field, submits, and asserts
//! all three properties.
//!
//! Skips gracefully when the WebDriver endpoint is unreachable —
//! same shape as the other browser-e2e tests in this directory.

use std::time::Duration;

use ryokan::models::config::{self, Config};
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use sqlx::SqlitePool;

#[path = "common/browser_e2e.rs"]
mod browser_e2e;
use browser_e2e::{
    assert_htmx_handled_in_place, assert_htmx_loaded, open_with_session, seed_user_session,
    spawn_app, try_connect_browser, wait_until_substring,
};

// ─── Seeding ────────────────────────────────────────────────────────────

/// Seed a `config` row with distinct, non-default values for each
/// tab's fields so the post-save assertions can tell which fields
/// changed and which got carried through. The choices are arbitrary
/// but each one is *different from the form-default empty / coerced
/// value* — so a regression that re-saves with the form's default
/// (rather than the existing) value flips the field and the test
/// fails loudly.
async fn seed_config_distinct(db: &SqlitePool) {
    let cfg = Config {
        // Integrations-tab fields.
        jellyfin_url: "http://seed-integrations.local:8096".to_string(),
        jellyfin_api_key: "seed-jellyfin-key".to_string(),
        sonarr_enabled: true,
        sonarr_api_key: "seed-sonarr-key-0123456789".to_string(),
        radarr_enabled: false,
        radarr_api_key: "seed-radarr-key-0123456789".to_string(),
        grab_preview_mode: "never".to_string(),
        external_sync_interval_minutes: 90,

        // Quality-tab fields.
        preferred_groups: "SeedGroup1, SeedGroup2".to_string(),
        blocked_groups: "SeedBlocked".to_string(),
        preferred_source: "bluray".to_string(),
        preferred_resolution: "720".to_string(),
        cutoff_source: "bluray_remux".to_string(),
        cutoff_resolution: "1080".to_string(),
        finished_series_quality: "bd_only".to_string(),
        prefer_subs: false,
        upgrade_search_enabled: true,
        seadex_enabled: true,
        default_custom_query_tokens: "seed query".to_string(),
        default_restrict_to_uploader: "SeedUploader".to_string(),

        // General-tab fields.
        media_root: "/srv/seed-media".to_string(),
        title_language: "native".to_string(),
        rss_enabled: false,
        rss_interval_minutes: 33,
        disable_nyaa_rss: true,
        post_processing_enabled: false,
        post_processing_mode: "copy".to_string(),
        search_on_monitoring_change: true,

        ..Config::default()
    };
    config::save_config(db, &cfg)
        .await
        .expect("seed config row");
}

/// Convenience: navigate to /settings?tab=<name>.
async fn open_tab(
    client: &fantoccini::Client,
    addr: std::net::SocketAddr,
    session_token: &str,
    tab: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    open_with_session(client, addr, session_token, &format!("/settings?tab={tab}")).await
}

/// Submit the visible subform's first `button[type="submit"]` and
/// wait for the "Settings saved." alert to render in the swapped
/// region. Captures the swap-completion signal — without waiting,
/// the test could read the DB / DOM before HTMX has swung the
/// response into place.
async fn submit_and_wait_for_save(
    client: &fantoccini::Client,
    region_selector: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    client
        .execute(
            r#"
            const region = document.querySelector(arguments[0]);
            if (!region) throw new Error('region not found: ' + arguments[0]);
            const btn = region.querySelector('button[type="submit"]');
            if (!btn) throw new Error('no submit button in ' + arguments[0]);
            btn.click();
            "#,
            vec![serde_json::json!(region_selector)],
        )
        .await?;
    wait_until_substring(
        client,
        &format!("{region_selector} .alert-success"),
        "Settings saved",
        Duration::from_secs(5),
    )
    .await
}

// ─── General tab ────────────────────────────────────────────────────────

#[tokio::test]
async fn general_save_persists_swaps_in_place_and_isolates_other_tabs() {
    let db = in_memory_pool().await;
    seed_config_distinct(&db).await;
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
        open_tab(&client, addr, &token, "general").await?;
        assert_htmx_loaded(&client).await?;

        // Mutate: title_language native → english. Verifies that the
        // form's coerce-on-save logic picks up the user's choice
        // rather than just re-saving the existing value.
        client
            .execute(
                r#"
                const sel = document.getElementById('title_language');
                if (!sel) throw new Error('title_language select not found');
                sel.value = 'english';
                sel.dispatchEvent(new Event('change', { bubbles: true }));
                "#,
                vec![],
            )
            .await?;

        submit_and_wait_for_save(&client, "#settings-general-region").await?;

        // (1) URL stayed at /settings?tab=general — no full-page nav.
        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=general"))
            .await?;

        // (2) DB persisted the new title_language.
        let cfg = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(
            cfg.title_language, "english",
            "title_language should reflect the form mutation"
        );

        // (3) Cross-tab isolation: Integrations + Quality fields
        //     stayed at their seeded values. This is the property
        //     the per-tab split was supposed to deliver.
        assert_eq!(cfg.jellyfin_url, "http://seed-integrations.local:8096");
        assert_eq!(cfg.jellyfin_api_key, "seed-jellyfin-key");
        assert!(cfg.sonarr_enabled);
        assert_eq!(cfg.sonarr_api_key, "seed-sonarr-key-0123456789");
        assert_eq!(cfg.preferred_groups, "SeedGroup1, SeedGroup2");
        assert_eq!(cfg.preferred_resolution, "720");
        assert_eq!(cfg.cutoff_source, "bluray_remux");
        assert_eq!(cfg.finished_series_quality, "bd_only");
        assert_eq!(cfg.default_custom_query_tokens, "seed query");
        assert_eq!(cfg.default_restrict_to_uploader, "SeedUploader");
        assert_eq!(cfg.grab_preview_mode, "never");
        assert_eq!(cfg.external_sync_interval_minutes, 90);

        // Other General fields stayed at their seeded values too —
        // only the title_language mutation should land.
        assert_eq!(cfg.media_root, "/srv/seed-media");
        assert!(!cfg.rss_enabled);
        assert_eq!(cfg.rss_interval_minutes, 33);
        assert!(cfg.disable_nyaa_rss);
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("general tab subform save");
}

// ─── Quality tab ────────────────────────────────────────────────────────

#[tokio::test]
async fn quality_save_persists_swaps_in_place_and_isolates_other_tabs() {
    let db = in_memory_pool().await;
    seed_config_distinct(&db).await;
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
        open_tab(&client, addr, &token, "quality").await?;
        assert_htmx_loaded(&client).await?;

        // Mutate: preferred_resolution 720 → 1080. (Both seeded into
        // distinct values for preferred vs cutoff so we can detect
        // any cross-field bleeding in the handler.)
        client
            .execute(
                r#"
                const sel = document.getElementById('preferred_resolution');
                if (!sel) throw new Error('preferred_resolution select not found');
                sel.value = '1080';
                sel.dispatchEvent(new Event('change', { bubbles: true }));
                "#,
                vec![],
            )
            .await?;

        submit_and_wait_for_save(&client, "#settings-quality-region").await?;

        assert_htmx_handled_in_place(&client, &format!("http://{addr}/settings?tab=quality"))
            .await?;

        let cfg = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(cfg.preferred_resolution, "1080");

        // Cross-tab isolation.
        assert_eq!(cfg.jellyfin_url, "http://seed-integrations.local:8096");
        assert!(cfg.sonarr_enabled);
        assert_eq!(cfg.grab_preview_mode, "never");
        assert_eq!(cfg.external_sync_interval_minutes, 90);
        assert_eq!(cfg.title_language, "native");
        assert_eq!(cfg.media_root, "/srv/seed-media");
        assert!(!cfg.rss_enabled);
        assert_eq!(cfg.post_processing_mode, "copy");

        // Other Quality fields stayed at their seeded values — only
        // the preferred_resolution mutation should land.
        assert_eq!(cfg.preferred_groups, "SeedGroup1, SeedGroup2");
        assert_eq!(cfg.blocked_groups, "SeedBlocked");
        assert_eq!(cfg.preferred_source, "bluray");
        assert_eq!(cfg.cutoff_source, "bluray_remux");
        assert_eq!(cfg.cutoff_resolution, "1080");
        assert_eq!(cfg.finished_series_quality, "bd_only");
        assert!(!cfg.prefer_subs);
        assert!(cfg.upgrade_search_enabled);
        assert!(cfg.seadex_enabled);
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("quality tab subform save");
}

// ─── Integrations tab ───────────────────────────────────────────────────

#[tokio::test]
async fn integrations_save_persists_swaps_in_place_and_isolates_other_tabs() {
    let db = in_memory_pool().await;
    seed_config_distinct(&db).await;
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
        open_tab(&client, addr, &token, "integrations").await?;
        assert_htmx_loaded(&client).await?;

        // Mutate: jellyfin_url. Use a localhost-shaped value that
        // will fail the connection test (port 1 is reserved /
        // unbound) so the integrations side effect surfaces a
        // failure notice in the toast — verifies the side effect
        // fires without depending on a live Jellyfin instance.
        // A new address needs the key typed again: the stored key is
        // write-only and never sent to an address it wasn't saved for.
        client
            .execute(
                r#"
                const inp = document.getElementById('jellyfin_url');
                if (!inp) throw new Error('jellyfin_url input not found');
                inp.value = 'http://127.0.0.1:1';
                inp.dispatchEvent(new Event('input', { bubbles: true }));
                const key = document.getElementById('jellyfin_api_key');
                if (!key) throw new Error('jellyfin_api_key input not found');
                if (key.value !== '') throw new Error('the stored key rendered into the page');
                key.value = 'retyped-jellyfin-key';
                "#,
                vec![],
            )
            .await?;

        submit_and_wait_for_save(&client, "#settings-integrations-region").await?;

        assert_htmx_handled_in_place(
            &client,
            &format!("http://{addr}/settings?tab=integrations"),
        )
        .await?;

        let cfg = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(cfg.jellyfin_url, "http://127.0.0.1:1");

        // Cross-tab isolation.
        assert_eq!(cfg.preferred_resolution, "720");
        assert_eq!(cfg.preferred_source, "bluray");
        assert_eq!(cfg.finished_series_quality, "bd_only");
        assert_eq!(cfg.default_custom_query_tokens, "seed query");
        assert_eq!(cfg.title_language, "native");
        assert_eq!(cfg.media_root, "/srv/seed-media");
        assert!(!cfg.rss_enabled);
        assert_eq!(cfg.post_processing_mode, "copy");

        // Integrations-tab side effect: failed Jellyfin connection
        // surfaces as a "connection failed" string somewhere in the
        // alert. We keyed the wait-for-save on "Settings saved" so
        // the success alert is already there; the failure-side
        // text appears alongside ("Settings saved. Jellyfin
        // connection failed: …").
        let alert_text: String = client
            .execute(
                r#"
                const el = document.querySelector('#settings-integrations-region .alert-success');
                return el ? (el.textContent || '') : '';
                "#,
                vec![],
            )
            .await?
            .as_str()
            .unwrap_or("")
            .to_string();
        if !alert_text.to_lowercase().contains("jellyfin connection failed") {
            return Err(format!(
                "expected Jellyfin connection-test failure notice in save toast — got `{alert_text}`"
            )
            .into());
        }

        assert_eq!(cfg.jellyfin_api_key, "retyped-jellyfin-key");
        // Other Integrations fields stayed at their seeded values.
        assert!(cfg.sonarr_enabled);
        assert_eq!(cfg.sonarr_api_key, "seed-sonarr-key-0123456789");
        assert!(!cfg.radarr_enabled);
        assert_eq!(cfg.radarr_api_key, "seed-radarr-key-0123456789");
        assert_eq!(cfg.grab_preview_mode, "never");
        assert_eq!(cfg.external_sync_interval_minutes, 90);
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("integrations tab subform save");
}

// ─── Form-shape regression guards ───────────────────────────────────────

/// Regression guard: each tab's subform must POST to `/settings/<tab>`,
/// not the legacy `/settings` bulk handler. A typo in the partial
/// (e.g. `hx-post="/settings"` instead of `hx-post="/settings/general"`)
/// would silently route through the legacy bulk handler — which still
/// works thanks to the field-by-field `tab` checks, but defeats the
/// per-tab split's whole point. Pin the wire-up.
#[tokio::test]
async fn each_subform_targets_its_dedicated_endpoint() {
    let db = in_memory_pool().await;
    seed_config_distinct(&db).await;
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
        for tab in ["integrations", "quality", "general"] {
            open_tab(&client, addr, &token, tab).await?;
            let region = format!("#settings-{tab}-region");
            let action_attrs: serde_json::Value = client
                .execute(
                    r#"
                    const region = document.querySelector(arguments[0]);
                    if (!region) return null;
                    const form = region.querySelector('form');
                    if (!form) return null;
                    return {
                        action: form.getAttribute('action') || '',
                        hxPost: form.getAttribute('hx-post') || '',
                    };
                    "#,
                    vec![serde_json::json!(region)],
                )
                .await?;

            let want = format!("/settings/{tab}");
            let action = action_attrs
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let hx_post = action_attrs
                .get("hxPost")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if action != want {
                return Err(format!(
                    "tab={tab}: form action `{action}` != expected `{want}` — \
                     subform is wired to the wrong endpoint"
                )
                .into());
            }
            if hx_post != want {
                return Err(format!(
                    "tab={tab}: form hx-post `{hx_post}` != expected `{want}` — \
                     subform's HTMX target is wrong"
                )
                .into());
            }
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let _ = client.close().await;
    result.expect("subform endpoint wiring");
}
