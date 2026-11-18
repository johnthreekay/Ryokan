//! End-to-end watch-list sync test (issue #62).
//!
//! Stands up a wiremock server that pretends to be AL's GraphQL
//! endpoint, points Ryokan at it via `RYOKAN_ANILIST_API_BASE`, seeds
//! an encrypted `external_accounts` row, and runs `tick_once`. The
//! assertion is a real series row landing in the in-memory DB with
//! the right monitor mode + cached metadata — i.e. all the seams
//! line up: token decrypt → AL fetch → entries normalize → detail
//! batch → series upsert → metadata_cache write → monitor_mode write.
//!
//! Why wiremock instead of a `live_smoke`-style env-gated test
//! against real AL: live smokes need a maintained AL OAuth token +
//! a stable test list. Wiremock against a captured response shape
//! gives us regression coverage without operational debt.

use ryokan::models::external_accounts::{LinkRequest, PROVIDER_ANILIST};
use ryokan::models::monitoring::MonitorMode;
use ryokan::models::{external_accounts, series};
use ryokan::services::anilist;
use ryokan::services::external_sync;
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One-at-a-time gate around `RYOKAN_ANILIST_API_BASE`, which every
/// test here points at its own wiremock. The variable is process-wide,
/// so tests sharing a process (plain `cargo test`; nextest gives each
/// its own) sent their requests to each other's mock. Same pattern as
/// `tests/library_link_e2e.rs::ENV_LOCK`.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// AL `MediaListCollection` response: one entry, status CURRENT
/// (Watching), AL id 999, no custom lists.
fn media_list_collection_response() -> serde_json::Value {
    json!({
        "data": {
            "MediaListCollection": {
                "lists": [
                    {
                        "isCustomList": false,
                        "entries": [
                            {
                                "mediaId": 999,
                                "status": "CURRENT",
                                "progress": 4,
                                "score": 8.5,
                                "updatedAt": 1_700_000_000_i64,
                                "notes": "",
                                "customLists": {}
                            }
                        ]
                    }
                ],
                "user": {
                    "mediaListOptions": {
                        "scoreFormat": "POINT_10_DECIMAL"
                    }
                }
            }
        }
    })
}

/// AL detail-batch response shape for `Page.media`. Only the fields
/// AnimeDetail's deserializer requires; everything else falls through
/// to the `#[serde(default)]` arms.
fn page_media_response() -> serde_json::Value {
    json!({
        "data": {
            "Page": {
                "media": [
                    {
                        "id": 999,
                        "idMal": 12345,
                        "title": {
                            "romaji": "Test Series",
                            "english": "Test Series",
                            "native": "テストシリーズ"
                        },
                        "synonyms": [],
                        "coverImage": {
                            "large": "https://example/cover.jpg",
                            "extraLarge": "https://example/cover-xl.jpg"
                        },
                        "bannerImage": "https://example/banner.jpg",
                        "format": "TV",
                        "status": "RELEASING",
                        "episodes": 12,
                        "duration": 24,
                        "season": "SPRING",
                        "seasonYear": 2024,
                        "endDate": { "year": null },
                        "description": "An example series for the e2e test.",
                        "genres": ["Action"],
                        "averageScore": 80,
                        "nextAiringEpisode": null,
                        "streamingEpisodes": [],
                        "relations": { "edges": [] }
                    }
                ]
            }
        }
    })
}

#[tokio::test]
async fn watch_list_sync_imports_series_with_resolved_monitor_mode() {
    let _gate = ENV_LOCK.lock().await;
    // SAFETY: setting env vars in tests is single-threaded within
    // this test (no other test sets RYOKAN_ANILIST_API_BASE) and the
    // override is the entire point of the seam — no race risk in this
    // crate's test layout.
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;

    // Two POSTs land at the wiremock root: the watch-list fetch
    // (matched on `MediaListCollection` in the body) and the detail-
    // batch fetch (matched on `Page(perPage`). Each gets its own
    // response shape; wiremock dispatches by body match.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("MediaListCollection"))
        .respond_with(ResponseTemplate::new(200).set_body_json(media_list_collection_response()))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("Page(perPage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_media_response()))
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);

    // Seed an external_account with the AL provider, fake plaintext
    // tokens (link() encrypts them), and the AL user id we expect
    // back. provider_user_id is read by sync_anilist as the user we
    // fetch the list for.
    external_accounts::link(
        &db,
        LinkRequest {
            provider: PROVIDER_ANILIST.to_string(),
            provider_user_id: "42".to_string(),
            username: "e2e_user".to_string(),
            access_token: "fake-al-access-token".to_string(),
            refresh_token: String::new(),
            access_token_expires_at: None,
            score_format: "POINT_10".to_string(),
        },
    )
    .await
    .expect("seed external account");

    // Run one full sync iteration. Returns the supervised-loop
    // summary string on success.
    let summary = external_sync::tick_once(&state)
        .await
        .expect("tick_once should succeed against the wiremock fixture");
    assert!(
        summary.contains("created 1"),
        "summary should report one created series, got: {summary}"
    );

    // Series row landed with the AL detail data merged in.
    let row = series::get_by_anilist_id(&db, 999)
        .await
        .expect("series query")
        .expect("series row should exist after sync");
    assert_eq!(row.anilist_id, 999);
    assert_eq!(row.mal_id, Some(12345));
    assert_eq!(row.title_english, "Test Series");
    assert_eq!(row.format, "TV");
    assert_eq!(row.status, "RELEASING");
    assert_eq!(row.episodes, Some(12));

    // CURRENT + skip_already_watched=false (the seed default) →
    // monitor_mode = "all". Plan decision #6.
    assert_eq!(
        row.monitor_mode,
        MonitorMode::All.as_str(),
        "Watching status without skip-already-watched must map to All"
    );

    // metadata_cache row written inline by the merge step.
    let cached = ryokan::models::metadata_cache::get_by_series_id(&db, row.id)
        .await
        .expect("metadata_cache query")
        .expect("metadata_cache row should exist");
    assert_eq!(cached.detail.id, 999);
    assert_eq!(cached.detail.title_english, "Test Series");

    // Cursor stamped on success — the next tick filters by this
    // timestamp and won't refetch the same entries.
    let acct = external_accounts::get_current(&db)
        .await
        .expect("get_current")
        .expect("linked account should still exist");
    assert!(
        acct.list_last_synced_at.unwrap_or(0) > 0,
        "list_last_synced_at must advance on a successful tick"
    );
    assert!(
        acct.list_full_resync_at.unwrap_or(0) > 0,
        "first sync is a full sync; list_full_resync_at must also advance"
    );
    // The fixture's MediaListCollection response carries
    // user.mediaListOptions.scoreFormat = "POINT_10_DECIMAL"; the
    // sync MUST persist that on every tick so the user's
    // post-link POINT_X change takes effect on the next render.
    // The link seed above started at "POINT_10" — assert the
    // refresh actually happened.
    assert_eq!(
        acct.score_format, "POINT_10_DECIMAL",
        "sync must refresh score_format from the AL response"
    );

    // Series row carries user_score = 8.5 from the fixture entry's
    // `score: 8.5`. Renders via the user's POINT_10_DECIMAL format.
    assert_eq!(row.user_score, Some(8.5));
    let formatted =
        ryokan::services::user_score::format_user_score(row.user_score, &acct.score_format);
    let html = formatted
        .as_ref()
        .expect("badge should render")
        .render_html();
    assert_eq!(html, "8.5");

    // Auth-flag clear-on-success invariant: a successful tick after a
    // prior token-rejection must wipe `last_sync_auth_failed` so the
    // Settings UI's "Re-link required" banner stops firing. Pre-flip
    // the flag (simulating the prior failed tick that set it), run
    // another sync, and assert it clears. Without this regression
    // guard a stuck-true flag would silently keep showing the banner
    // forever; the detection-side tests pin only the failure path.
    external_accounts::update_last_sync_auth_failed(&db, acct.id, true)
        .await
        .expect("pre-flip last_sync_auth_failed for the regression guard");
    external_sync::tick_once(&state)
        .await
        .expect("second tick should succeed against the wiremock fixture");
    let acct_after = external_accounts::get_current(&db)
        .await
        .expect("get_current after second tick")
        .expect("linked account should still exist after second tick");
    assert!(
        !acct_after.last_sync_auth_failed,
        "successful tick must clear last_sync_auth_failed"
    );

    // Cleanup: clear the override so other tests in the same process
    // (sequential within a `cargo test` invocation but separate test
    // crates run in parallel) don't pick up our wiremock host.
    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

/// Pin the OAuth-shaped 400 -> auth-rejection branch added so the
/// "Re-link required" badge fires when the upstream identity layer
/// rejects a malformed/revoked access token at the HTTP edge (vs.
/// the GraphQL `errors[]` shape AL normally uses for token issues).
/// Without this branch the failure routed through "AniList
/// unavailable" which `is_auth_rejection` treats as transient — the
/// flag stayed false and the banner never fired.
#[tokio::test]
async fn watch_list_sync_flips_auth_failed_on_400_invalid_token_response() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":"invalid_token","error_description":"The access token is invalid"}"#,
        ))
        .mount(&mock)
        .await;

    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    external_accounts::link(
        &db,
        LinkRequest {
            provider: PROVIDER_ANILIST.to_string(),
            provider_user_id: "42".to_string(),
            username: "e2e_user".to_string(),
            access_token: "fake-al-access-token".to_string(),
            refresh_token: String::new(),
            access_token_expires_at: None,
            score_format: "POINT_10".to_string(),
        },
    )
    .await
    .expect("seed external account");

    let err = external_sync::tick_once(&state)
        .await
        .expect_err("400 + invalid_token must surface as Err");
    assert!(
        err.contains("AniList rejected the watch-list token"),
        "error string must carry the auth-rejection prefix so is_auth_rejection() matches; got: {err}"
    );
    assert!(
        err.contains("invalid_token"),
        "error must surface the OAuth error code for operator log; got: {err}"
    );

    // The sticky flag flipped to true — the Settings UI's
    // "Re-link required" badge keys off this column.
    let acct = external_accounts::get_current(&db)
        .await
        .expect("get_current")
        .expect("linked account should still exist");
    assert!(
        acct.last_sync_auth_failed,
        "auth-rejection 400 must flip last_sync_auth_failed; without this the Settings badge never fires"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}

/// Issue #118 follow-up — the `was_already_failed && write_ok` gate
/// in `services/external_sync/mod.rs` is what makes the re-link
/// notification fire **once per fail→fail-resolved cycle** instead
/// of every auth-rejection tick. Without the gate, an unattended
/// weekend with a dead AL token could spam Discord with 5-10
/// duplicate "Re-link required" pings as the supervised loop
/// keeps retrying.
///
/// Pre-seeds an account with `last_sync_auth_failed = true` (the
/// post-first-failure state), installs a recording provider in the
/// notification cache, runs `tick_once` against the same wiremock
/// 400 the prior test exercises, and asserts the recording
/// provider received **zero** notifications.
///
/// Belt-and-braces: the second sub-block exercises a fresh-fail
/// cycle (flag starts false) and asserts exactly one emit. Without
/// this counter-test, a wiring regression that silently suppressed
/// every emit would make the half-1 zero-count pass for the wrong
/// reason.
#[tokio::test]
async fn watch_list_sync_emits_relink_notification_only_on_false_to_true_transition() {
    let _gate = ENV_LOCK.lock().await;
    use async_trait::async_trait;
    use ryokan::services::notifications::{NotificationEvent, NotificationProvider};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Minimal `NotificationProvider` that records every dispatch.
    /// Lives inline because the `cfg(test)` mock in
    /// `services::notifications::tests` isn't reachable from
    /// integration tests, and a one-off recorder is smaller than
    /// promoting that mock to `cfg(any(test, feature = "test-support"))`.
    struct RecordingProvider {
        sent: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl NotificationProvider for RecordingProvider {
        fn id(&self) -> i64 {
            1
        }
        fn name(&self) -> &str {
            "recorder"
        }
        fn kind(&self) -> &'static str {
            "test"
        }
        async fn send(&self, _event: &NotificationEvent) -> Result<(), String> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // ---- Half 1: flag pre-flipped → no emit on this tick. ----
    {
        anilist::reset_state_for_tests();
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":"invalid_token","error_description":"The access token is invalid"}"#,
            ))
            .mount(&mock)
            .await;
        unsafe {
            std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        }

        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        external_accounts::link(
            &db,
            LinkRequest {
                provider: PROVIDER_ANILIST.to_string(),
                provider_user_id: "42".to_string(),
                username: "e2e_user".to_string(),
                access_token: "fake-al-access-token".to_string(),
                refresh_token: String::new(),
                access_token_expires_at: None,
                score_format: "POINT_10".to_string(),
            },
        )
        .await
        .expect("seed external account");
        // Pre-flip the flag — represents "this is the second-fail
        // in the same cycle." The transition gate must suppress.
        let acct_id = external_accounts::get_current(&db)
            .await
            .expect("get_current")
            .expect("linked account")
            .id;
        external_accounts::update_last_sync_auth_failed(&db, acct_id, true)
            .await
            .expect("pre-flip flag");

        // Seed `notification_settings` so the per-event matrix
        // doesn't default-deny `ExternalSyncReLinkRequired` and
        // mask the gate-test by short-circuiting for a different
        // reason.
        sqlx::query(
            "INSERT INTO notification_providers (id, name, kind, enabled, config_json) \
             VALUES (1, 'recorder', 'test', 1, '{}')",
        )
        .execute(&db)
        .await
        .expect("seed provider row");
        ryokan::services::notifications::store::seed_default_matrix(&db, 1)
            .await
            .expect("seed matrix");

        let sent = Arc::new(AtomicUsize::new(0));
        let providers: Vec<Arc<dyn NotificationProvider>> =
            vec![Arc::new(RecordingProvider { sent: sent.clone() })];
        *state.notification_providers.write().await = Arc::new(providers);

        let _ = external_sync::tick_once(&state)
            .await
            .expect_err("tick must still surface the auth-rejection Err");

        // Dispatcher fires `tokio::spawn`s; give them a moment to
        // drain. Shorter than `PROVIDER_SEND_TIMEOUT` so an
        // accidental dispatch surfaces here rather than getting
        // lost on a slow CI runner.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        assert_eq!(
            sent.load(Ordering::SeqCst),
            0,
            "transition gate must suppress the duplicate notification \
             when last_sync_auth_failed was already true entering the tick"
        );

        unsafe {
            std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        }
        anilist::reset_state_for_tests();
    }

    // ---- Half 2: fresh-fail cycle (flag starts false) → exactly one emit. ----
    {
        anilist::reset_state_for_tests();
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":"invalid_token","error_description":"The access token is invalid"}"#,
            ))
            .mount(&mock)
            .await;
        unsafe {
            std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
        }

        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        external_accounts::link(
            &db,
            LinkRequest {
                provider: PROVIDER_ANILIST.to_string(),
                provider_user_id: "42".to_string(),
                username: "e2e_user".to_string(),
                access_token: "fake-al-access-token".to_string(),
                refresh_token: String::new(),
                access_token_expires_at: None,
                score_format: "POINT_10".to_string(),
            },
        )
        .await
        .expect("seed external account");
        // Flag intentionally NOT pre-flipped — this is the first
        // auth-rejection in a clean cycle; the notification must
        // fire.

        sqlx::query(
            "INSERT INTO notification_providers (id, name, kind, enabled, config_json) \
             VALUES (1, 'recorder', 'test', 1, '{}')",
        )
        .execute(&db)
        .await
        .expect("seed provider row");
        ryokan::services::notifications::store::seed_default_matrix(&db, 1)
            .await
            .expect("seed matrix");

        let sent = Arc::new(AtomicUsize::new(0));
        let providers: Vec<Arc<dyn NotificationProvider>> =
            vec![Arc::new(RecordingProvider { sent: sent.clone() })];
        *state.notification_providers.write().await = Arc::new(providers);

        let _ = external_sync::tick_once(&state)
            .await
            .expect_err("tick must still surface the auth-rejection Err");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        assert_eq!(
            sent.load(Ordering::SeqCst),
            1,
            "fresh-fail cycle (flag starts false) must emit exactly once \
             — without this assertion the half-1 zero-emit pass could be \
             a false-positive caused by a wiring regression that suppresses \
             every emit"
        );

        let acct = external_accounts::get_current(&db)
            .await
            .expect("get_current")
            .expect("linked account");
        assert!(
            acct.last_sync_auth_failed,
            "the gate that emits also writes the flag — both must hold together"
        );

        unsafe {
            std::env::remove_var("RYOKAN_ANILIST_API_BASE");
        }
        anilist::reset_state_for_tests();
    }
}

/// Issue #62 + #118 — when a sync succeeds on a later tick
/// (user re-linked their account), `last_sync_auth_failed` must
/// clear back to false. This is the precondition for the transition
/// gate to ever fire again — without the clear, every subsequent
/// token-death would look like a continuation of the same fail
/// cycle and never re-emit.
#[tokio::test]
async fn watch_list_sync_clears_auth_failed_flag_on_successful_tick() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "MediaListCollection": {
                    "lists": [],
                    "user": {
                        "mediaListOptions": {"scoreFormat": "POINT_10"}
                    }
                }
            }
        })))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }

    let db = in_memory_pool().await;
    let state = build_test_app_state(db.clone(), None);
    external_accounts::link(
        &db,
        LinkRequest {
            provider: PROVIDER_ANILIST.to_string(),
            provider_user_id: "42".to_string(),
            username: "e2e_user".to_string(),
            access_token: "fake-al-access-token".to_string(),
            refresh_token: String::new(),
            access_token_expires_at: None,
            score_format: "POINT_10".to_string(),
        },
    )
    .await
    .expect("seed external account");
    // Pre-flip — represents "user just re-linked; the flag is
    // still set from the failed ticks before the re-link."
    let acct_id = external_accounts::get_current(&db)
        .await
        .unwrap()
        .unwrap()
        .id;
    external_accounts::update_last_sync_auth_failed(&db, acct_id, true)
        .await
        .expect("pre-flip flag");

    external_sync::tick_once(&state).await.expect("sync ok");

    let acct = external_accounts::get_current(&db)
        .await
        .expect("get_current")
        .expect("linked account");
    assert!(
        !acct.last_sync_auth_failed,
        "successful tick after a prior auth-rejection must clear last_sync_auth_failed; \
         without the clear, the next dead-token cycle silently re-uses the previous \
         transition gate state and never re-emits the re-link notification"
    );

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}
