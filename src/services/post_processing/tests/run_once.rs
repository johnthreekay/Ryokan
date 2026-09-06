//! Multi-client fan-out tests for `run_once`. Covers
//! PR 109 (delete-cascade NULL-out) + PR 110 (in-loop NULL-cleanup
//! when the stamped client resolves to None) all touched the
//! resolution path that sits between `get_all_pending` and
//! `import_torrent`. Pre-this-test-file the only direct unit coverage
//! was the model-layer round-trip
//! (`set_download_client_round_trips_through_get_all_pending`),
//! which pins the schema column but not the behavior of the
//! per-grab fan-out.
//!
//! These tests sit at the same layer as the existing
//! `grab_sweep::tests::sweep_dispatches_to_pinned_client_not_default`
//! one — build a real `AppState` with a multi-client pool of
//! `RecordingClient`s, seed `pending_grabs` rows with specific
//! `download_client_id` stamps, drive `run_once`, and assert which
//! clients received `list_scoped` + the post-condition on the
//! grabbed_torrents row.
//!
//! Caveat: `cfg.post_processing_enabled` is left at the default
//! `false` so `run_once` early-returns into
//! `advance_state_without_import` before any fan-out happens. To
//! cover the fan-out path proper we set `post_processing_enabled =
//! true` AND `media_root` non-empty, and arrange the mock client
//! to return torrents whose `state_kind` is *not* complete — that
//! way `run_once` reaches the per-grab match block but never calls
//! `import_torrent`, which would touch the real filesystem.

use crate::models::download_clients::{DownloadClientForm, insert as insert_dc};
use crate::models::grabbed_torrents;
use crate::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};
use crate::services::post_processing;
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use async_trait::async_trait;

use super::POST_PROC_TEST_SERIALIZER;
use std::sync::Arc;
use std::sync::Mutex;

/// Recording mock that captures `list_scoped` calls and returns a
/// canned set of torrents per client. Mirrors the shape of
/// `grab_sweep::tests::RecordingClient` but tuned for what
/// `run_once` exercises (list_scoped only — the test never reaches
/// the import path).
struct RecordingClient {
    list_calls: Mutex<u32>,
    list_fails: bool,
    /// Canned response for `list_scoped`. Each entry maps to one
    /// `DownloadItem` returned with the given hash + state.
    canned: Vec<DownloadItem>,
}

impl RecordingClient {
    fn new(canned: Vec<DownloadItem>) -> Self {
        Self {
            list_calls: Mutex::new(0),
            list_fails: false,
            canned,
        }
    }

    fn failing() -> Self {
        Self {
            list_calls: Mutex::new(0),
            list_fails: true,
            canned: Vec::new(),
        }
    }

    fn list_call_count(&self) -> u32 {
        *self.list_calls.lock().unwrap()
    }
}

#[async_trait]
impl DownloadClient for RecordingClient {
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
        *self.list_calls.lock().unwrap() += 1;
        if self.list_fails {
            Err("simulated list_scoped failure".into())
        } else {
            Ok(self.canned.clone())
        }
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

fn fake_torrent(hash: &str, state_kind: DownloadItemState) -> DownloadItem {
    DownloadItem {
        hash: hash.to_string(),
        name: format!("torrent-{hash}"),
        size: 1000,
        progress: 0.5,
        dlspeed: 0,
        state: format!("{state_kind:?}"),
        category: "anime".to_string(),
        eta: 0,
        save_path: String::new(),
        content_path: String::new(),
        state_kind,
        seeding_done: false,
    }
}

/// Seed minimum config so `run_once` doesn't early-return at the
/// disabled / empty-media-root gate. We deliberately keep
/// `post_processing_enabled = 1` and a non-empty `media_root` so
/// the fan-out path runs, but every mock torrent reports
/// `state_kind = Downloading` (incomplete) — that means `run_once`
/// hits the `if !torrent.state_kind.is_complete() { continue; }`
/// guard for every match and never enters `import_torrent`.
async fn seed_config(db: &sqlx::SqlitePool) {
    sqlx::query(
        "INSERT INTO config (id, post_processing_enabled, media_root) \
         VALUES (1, 1, '/tmp/test-media-root') \
         ON CONFLICT(id) DO UPDATE SET \
             post_processing_enabled = 1, \
             media_root = '/tmp/test-media-root'",
    )
    .execute(db)
    .await
    .expect("seed config row");
}

async fn install_pool(
    state: &crate::AppState,
    entries: Vec<(i64, Arc<dyn DownloadClient>, bool)>, // (id, client, is_default)
) {
    let mut clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>> =
        std::collections::HashMap::new();
    // Tests in this module exercise torrent-side post-processing; the
    // mock `DownloadClient` fixtures don't carry a protocol, so any
    // `is_default = true` entry here pins the torrent default. A
    // future usenet-flavored fixture should mint its own pool that
    // populates `default_usenet_id` instead.
    let mut default_torrent_id = None;
    for (id, c, is_default) in entries {
        if is_default {
            default_torrent_id = Some(id);
        }
        clients.insert(id, c);
    }
    let pool = crate::DownloadClientPool {
        clients,
        default_torrent_id,
        default_usenet_id: None,
    };
    *state.download_clients.write().await = Arc::new(pool);
}

#[tokio::test]
async fn run_once_fans_out_list_scoped_per_pinned_client() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Two pending grabs, each pinned to a different client. Both
    // clients should receive exactly one `list_scoped` call. Pre-PR-F
    // the loop fanned out only against the default — only one of the
    // two clients would have been touched.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show A").await;
    // Grab pinned to client-id 2 (Seedbox), hash deadbeef0
    let g1 = grabbed_torrents::record_grab(&db, "deadbeef0", "rel-1", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g1, Some(2))
        .await
        .unwrap();
    // Grab pinned to client-id 3 (alternate), hash deadbeef1
    let g2 = grabbed_torrents::record_grab(&db, "deadbeef1", "rel-2", series_id, &[2], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g2, Some(3))
        .await
        .unwrap();
    // Seed download_clients rows so the pool can resolve.
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default qbit",
            kind: "qbittorrent",
            url: "http://qbit",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "seedbox",
            kind: "deluge",
            url: "http://seedbox",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        },
    )
    .await
    .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "alt",
            kind: "transmission",
            url: "http://alt",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    let seedbox = Arc::new(RecordingClient::new(vec![fake_torrent(
        "deadbeef0",
        DownloadItemState::Downloading,
    )]));
    let alt = Arc::new(RecordingClient::new(vec![fake_torrent(
        "deadbeef1",
        DownloadItemState::Downloading,
    )]));
    install_pool(
        &state,
        vec![
            (1, default_client.clone() as Arc<dyn DownloadClient>, true),
            (2, seedbox.clone() as Arc<dyn DownloadClient>, false),
            (3, alt.clone() as Arc<dyn DownloadClient>, false),
        ],
    )
    .await;

    post_processing::run_once(&state).await;

    // Only the two pinned clients should have received list_scoped —
    // the default never sees the call because no pending grab points
    // at it (both grabs carry explicit pins).
    assert_eq!(
        seedbox.list_call_count(),
        1,
        "seedbox (id=2) must receive its single fan-out list_scoped call"
    );
    assert_eq!(
        alt.list_call_count(),
        1,
        "alt (id=3) must receive its single fan-out list_scoped call"
    );
    assert_eq!(
        default_client.list_call_count(),
        0,
        "default (id=1) must NOT see list_scoped — no pending grab references it. \
         Pre-PR-F the fan-out hit only the default; this assertion catches a regression \
         that would silently drop pinned-client grabs back onto the default."
    );
}

#[tokio::test]
async fn run_once_cleans_orphan_stamps_when_no_default_client_exists() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // PR 112 review #3 (4th pass) — the pre-pass orphan cleanup at
    // post_processing/mod.rs exists specifically for the case where
    // every pending grab points at a gone client AND there's no
    // default to fall back to. The pre-existing
    // `run_once_nulls_stamp_when_client_id_no_longer_in_pool` test
    // seeds a default, which masks this code path entirely (the
    // fan-out's `else if let Some(id) = default_id_opt` branch
    // saves the day). Without this test, a future refactor that
    // moves the cleanup back inside the loop would silently regress
    // — the grab would stay orphaned forever and never reach the
    // stale-grab pruning path.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g =
        grabbed_torrents::record_grab(&db, "all_orphan_no_default", "rel", series_id, &[1], false)
            .await
            .unwrap()
            .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(999))
        .await
        .unwrap();

    let state = build_test_app_state(db.clone(), None);
    // Empty pool: no clients, no default. This is the genuine
    // "all-orphans-no-default" shape the pre-pass defends.
    install_pool(&state, Vec::new()).await;

    post_processing::run_once(&state).await;

    let stamp_after: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(
        stamp_after.is_none(),
        "stamp must be NULLed even when no default client exists. \
         Without the pre-pass cleanup the fan-out's `clients.is_empty()` \
         early-return would skip the per-loop NULL and orphan the grab forever."
    );
}

#[tokio::test]
async fn run_once_nulls_stamp_when_client_id_no_longer_in_pool() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // A grab stamped with `download_client_id = 999` (a deleted /
    // never-existed client). The PR 110 in-loop cleanup must NULL the
    // stamp so the next pass falls through to default and the grab
    // can either match or hit the 60s stale path. Pre-fix the grab
    // was orphaned forever (run_once `continue`s past the stale check
    // on resolve-fail).
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "orphaned", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(999))
        .await
        .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    // The grab's stamp must have been NULLed so a later pass falls
    // through to default + the stale path.
    let stamp_after: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(
        stamp_after.is_none(),
        "stamp must be NULLed when the referenced client is gone from the pool. \
         Pre-PR-110 fix: the grab orphaned forever (continue past the stale check)."
    );
}

#[tokio::test]
async fn run_once_does_not_null_stamp_on_transient_list_scoped_failure() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Sister case to the orphan-cleanup test: when a stamped client
    // IS in the pool but its `list_scoped` fails this pass (network
    // glitch, transient 5xx), the stamp must stay so the next pass
    // retries the same client. Pre-PR-110 the in-loop cleanup
    // didn't distinguish these cases — a refactor that NULLs on every
    // resolve-fail would silently fall back to default, which is the
    // opposite of what we want for transient failures.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "transient", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(2))
        .await
        .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "flaky",
            kind: "deluge",
            url: "http://flaky",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    let flaky = Arc::new(RecordingClient::failing());
    install_pool(
        &state,
        vec![
            (1, default_client.clone() as Arc<dyn DownloadClient>, true),
            (2, flaky.clone() as Arc<dyn DownloadClient>, false),
        ],
    )
    .await;

    post_processing::run_once(&state).await;

    // The flaky client was queried (and failed), the stamp was NOT
    // NULLed because the client is still in the pool — only the
    // not-in-pool case clears the stamp.
    assert_eq!(flaky.list_call_count(), 1);
    let stamp_after: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        stamp_after,
        Some(2),
        "stamp must survive a transient list_scoped failure so the next pass retries \
         the same client. NULLing here would silently fall back to default."
    );
}

#[tokio::test]
async fn run_once_isolates_failures_per_client() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Two pinned grabs on two different clients; one client's
    // list_scoped fails, the other's succeeds. The successful
    // client's grab must still be processed — failures don't poison
    // the cross-client fan-out. Pre-PR-F this couldn't even happen
    // (single client meant one failure killed the whole pass).
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g_ok = grabbed_torrents::record_grab(&db, "okhash", "ok-rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g_ok, Some(2))
        .await
        .unwrap();
    let g_fail = grabbed_torrents::record_grab(&db, "failhash", "fail-rel", series_id, &[2], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g_fail, Some(3))
        .await
        .unwrap();
    for (name, kind, url, is_default) in [
        ("default", "qbittorrent", "http://q", true),
        ("ok-client", "deluge", "http://ok", false),
        ("fail-client", "transmission", "http://fail", false),
    ] {
        insert_dc(
            &db,
            DownloadClientForm {
                name,
                kind,
                url,
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default,
            },
        )
        .await
        .unwrap();
    }

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    let ok_client = Arc::new(RecordingClient::new(vec![fake_torrent(
        "okhash",
        DownloadItemState::Downloading,
    )]));
    let fail_client = Arc::new(RecordingClient::failing());
    install_pool(
        &state,
        vec![
            (1, default_client.clone() as Arc<dyn DownloadClient>, true),
            (2, ok_client.clone() as Arc<dyn DownloadClient>, false),
            (3, fail_client.clone() as Arc<dyn DownloadClient>, false),
        ],
    )
    .await;

    post_processing::run_once(&state).await;

    // Both pinned clients were queried (fan-out ran in full); the
    // failing client's grab is left pending (stamp survives, no
    // stale-mark since the client IS in the pool); the successful
    // client's grab matched its torrent and reached the
    // is_complete() guard (which short-circuits since the mock
    // torrent is `Downloading`, never `Seeding`).
    assert_eq!(ok_client.list_call_count(), 1, "ok client must run");
    assert_eq!(fail_client.list_call_count(), 1, "fail client must run");
    // Both grabs survive — neither matched a complete torrent, but
    // that's the in-flight expectation. Stamps stay.
    let ok_stamp: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g_ok)
            .fetch_one(&db)
            .await
            .unwrap();
    let fail_stamp: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g_fail)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        ok_stamp,
        Some(2),
        "ok grab stamp survives — its client is healthy"
    );
    assert_eq!(
        fail_stamp,
        Some(3),
        "fail grab stamp survives — transient list_scoped error doesn't NULL"
    );
}

// ─── Disabled / empty-media-root early-return paths ──────────────
//
// `run_once` has two early-return gates before the per-grab fan-out
// kicks in: `post_processing_enabled = false` and `media_root = ""`.
// Both dispatch to `advance_state_without_import`, which still
// fans out `list_scoped` (so the UI's "Importing…" spinner can
// clear when the client reports complete) but never moves any
// files. Distinguishing observable: a Downloading-state torrent in
// disabled mode leaves the grab `pending` — the import branch
// (which only runs when the toggle is on) is the only one that
// could transition state on a complete torrent.

#[tokio::test]
async fn run_once_with_post_processing_disabled_leaves_pending_grab_unchanged() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let db = in_memory_pool().await;
    sqlx::query(
        "INSERT INTO config (id, post_processing_enabled, media_root) VALUES (1, 0, '/tmp/x')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "h", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(1))
        .await
        .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(vec![fake_torrent(
        "h",
        DownloadItemState::Downloading,
    )]));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    // Pin the disabled-gate dispatch by exercising the
    // advance_state_without_import branch and asserting the grab
    // stayed pending. A regression that flipped the gate inverted
    // would route through the import path which (on a Downloading
    // torrent) hits the `is_complete()` `continue` and leaves
    // pending too — but the path itself differs, and lifting the
    // gate test catches earlier in the chain. Coverage-wise: this
    // is the only test exercising the disabled-mode dispatch.
    let final_state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(g)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(final_state, "pending");
}

#[tokio::test]
async fn run_once_with_empty_media_root_leaves_pending_grab_unchanged() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // media_root = '' is the second arm of the gate. The handler
    // explicitly treats it as the disabled case (mod.rs:1458-1463) —
    // even with the toggle on, no media_root means the import path
    // can't possibly run. Both conditions route to the same
    // advance_state_without_import dispatch.
    let db = in_memory_pool().await;
    sqlx::query("INSERT INTO config (id, post_processing_enabled, media_root) VALUES (1, 1, '')")
        .execute(&db)
        .await
        .unwrap();
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "h", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(1))
        .await
        .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(vec![fake_torrent(
        "h",
        DownloadItemState::Downloading,
    )]));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    let final_state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(g)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(final_state, "pending");
}

// ─── 60s grace + missing-from-client cleanup ─────────────────────

#[tokio::test]
async fn run_once_marks_grab_removed_when_client_loses_torrent_and_grab_is_stale() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // The pre-multi-client comment at mod.rs:1717 frames this case
    // as "user deleted ep 9 from the client and the row is still
    // pending." After 60s with no matching torrent in any client's
    // list_scoped response, the grab transitions to 'removed' and
    // the per-episode tags get cleared. Pin both halves so a
    // refactor that flipped the grace direction (`<` vs `<=`) or
    // inverted the missing-torrent guard can't silently regress
    // into "stuck pending forever."
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "h", "rel", series_id, &[3], false)
        .await
        .unwrap()
        .unwrap();
    // Backdate grabbed_at so grab_is_stale(_, 60) returns true.
    sqlx::query(
        "UPDATE grabbed_torrents SET grabbed_at = datetime('now', '-5 minutes') WHERE id = ?",
    )
    .bind(g)
    .execute(&db)
    .await
    .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    // Empty list_scoped → grab has no matching torrent.
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    let final_state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(g)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        final_state, "removed",
        "stale grab + no matching torrent → mark_removed; not 'pending' (would re-search forever) and not 'failed' (would blocklist)"
    );
}

#[tokio::test]
async fn run_once_does_not_mark_fresh_grab_removed_when_client_has_not_seen_it_yet() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // The 60s grace exists for the legitimate case where the
    // download client hasn't picked up the torrent yet (slow first
    // poll after add_torrent). A fresh grab missing from the
    // client's list must stay pending so the next pass can find
    // it. Pin the grace contract — flipping the comparison would
    // mark fresh grabs as removed and the user's just-clicked
    // grab would silently disappear within seconds.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "fresh", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    // Default `grabbed_at = CURRENT_TIMESTAMP` → grab_is_stale=false.
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    let final_state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(g)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        final_state, "pending",
        "fresh grab missing from client must stay 'pending' — the 60s grace is load-bearing for slow first polls"
    );
}

// ─── Errored / failed torrent state ───────────────────────────────

#[tokio::test]
async fn run_once_marks_grab_failed_when_client_reports_torrent_in_error_state() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // A torrent that the client reports as `Error` / `Failed` /
    // `MissingFiles` (any `state_kind.is_errored()`) → mark_failed.
    // Pre-multi-client this branch logged a qBit-specific tag; now
    // the message normalizes across all five clients via state_kind
    // slug. The `is_errored()` -> `mark_failed` chain itself is
    // covered here — a refactor that fell through to the
    // is_complete() guard would silently leave error-state grabs
    // pending forever.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "errhash", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    // The `Errored` variant is the sole `is_errored()` mapping
    // — every wire-level failure (qBit `error`, SAB `Failed`,
    // Transmission status<0, etc.) lands here in the normalized
    // state_kind enum.
    let default_client = Arc::new(RecordingClient::new(vec![fake_torrent(
        "errhash",
        DownloadItemState::Errored,
    )]));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    let final_state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(g)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        final_state, "failed",
        "errored torrent must transition to 'failed', not stay 'pending' or get marked 'removed'"
    );
}

#[tokio::test]
async fn run_once_falls_back_to_default_for_null_stamps() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Legacy / unstamped grab (download_client_id IS NULL). The
    // resolution chain must fall through to default — preserves
    // pre-multi-client behavior for upgraders whose existing grabs
    // never went through `set_download_client`.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let _g = grabbed_torrents::record_grab(&db, "legacy", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    // Deliberately do NOT call set_download_client — leave column NULL.
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(vec![fake_torrent(
        "legacy",
        DownloadItemState::Downloading,
    )]));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    // Default client received the call (NULL stamp → default
    // fall-through), and the grab's stamp stays NULL since the
    // client IS in the pool (transient or fall-through, not deleted).
    assert_eq!(default_client.list_call_count(), 1);
}

// ─── Path-traversal rejection (issue #117) ───────────────────────
//
// `DownloadClient::get_files` reflects torrent metadata, which is
// attacker-controlled. A malicious file-list entry like `/etc/passwd`
// or `../../escape.mkv` would resolve outside the configured source
// base via `Path::join`, and the import op (hardlink / copy / move)
// would touch host files the user never grabbed. The validator on
// `import_torrent`'s file loop must reject those entries before the
// `Path::join` and the `do_file_op` call. This test drives the full
// `run_once` → `import_torrent` chain against a mock client that
// reports a torrent with a mixed legit-plus-malicious file list and
// asserts:
//   1. the legit file imports normally into media_root;
//   2. each malicious entry produces a "Rejected suspicious file-list
//      entry" log row in the `logs` table (so System → Logs surfaces
//      the rejection);
//   3. the destination season directory contains exactly one file
//      (the legit one) — no escape attempt managed to land a hardlink
//      that points outside source_base.
//
// Distinct from the unit tests on `validate_relative_path_fragment`
// in `tests/filenames.rs` — those pin the rejection rules per-input;
// this test pins that the rules are actually wired into the import
// loop, that the rejection logs through `LogCategory::PostProcess`,
// and that legitimate files in the same torrent still import.

/// Mock client tuned for the rejection-path test: returns a single
/// canned complete torrent + a canned `get_files` payload so the test
/// can probe how `import_torrent` handles a mixed legit + malicious
/// file list.
struct ImportingClient {
    torrent: DownloadItem,
    files: Vec<DownloadFile>,
}

#[async_trait]
impl DownloadClient for ImportingClient {
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
        Ok(vec![self.torrent.clone()])
    }
    async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(self.files.clone())
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

#[tokio::test]
async fn run_once_rejects_malicious_filelist_entries_but_imports_legit_siblings() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;

    // Two tempdirs on the same filesystem so hardlink mode (the
    // default) succeeds without an EXDEV-fallback path. media_root
    // is the destination; source_dir is what the mock torrent reports
    // as its `save_path` and `content_path`.
    let media_root = tempfile::TempDir::new().expect("media_root tempdir");
    let source_dir = tempfile::TempDir::new().expect("source tempdir");
    let media_root_path = media_root.path().to_string_lossy().to_string();
    let source_path = source_dir.path().to_string_lossy().to_string();

    // Place a real legit file inside source_dir. The import loop will
    // hardlink this into media_root/<folder_name>/Season 01/.
    let legit_filename = "Show Title - 01.mkv";
    std::fs::write(source_dir.path().join(legit_filename), b"legit content")
        .expect("write legit file");

    // Seed config with the post-processing toggle on + the real
    // tempdir as media_root so `run_once` reaches `import_torrent`.
    let db = in_memory_pool().await;
    sqlx::query(
        "INSERT INTO config (id, post_processing_enabled, media_root, post_processing_mode) \
         VALUES (1, 1, ?, 'hardlink')",
    )
    .bind(&media_root_path)
    .execute(&db)
    .await
    .expect("seed config row");

    let series_id = seed_series(&db, 1, "Show Title").await;
    let g =
        grabbed_torrents::record_grab(&db, "deadbeef", "Show Title - 01", series_id, &[1], false)
            .await
            .unwrap()
            .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(1))
        .await
        .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();

    // Complete torrent (state_kind = Seeding) so the import branch in
    // run_once actually runs. save_path / content_path point at the
    // real source tempdir so the import loop's `source_base`
    // resolution lands at a directory we control.
    let torrent = DownloadItem {
        hash: "deadbeef".into(),
        name: "Show Title - 01".into(),
        size: 13,
        progress: 1.0,
        dlspeed: 0,
        state: "seeding".into(),
        category: "anime".into(),
        eta: 0,
        save_path: source_path.clone(),
        content_path: source_path.clone(),
        state_kind: DownloadItemState::Seeding,
        seeding_done: false,
    };

    // The malicious entries — each `.mkv` so they survive the
    // is_video_file filter and reach the validator. progress=1.0 so
    // they pass the "complete files only" guard. Together they cover
    // each rejection arm:
    //   * absolute path (RootDir) — issue's headline one-shot bypass
    //   * relative parent traversal (ParentDir at the start)
    //   * mid-path parent traversal (ParentDir after a Normal)
    //   * Windows-style absolute (rejected via the explicit '\' guard
    //     since on Unix `Path::components` would otherwise treat this
    //     as a single Normal component)
    let malicious_entries = [
        "/tmp/ryokan-issue-117-absolute-leak.mkv",
        "../escape-via-parent.mkv",
        "subdir/../../mid-path-escape.mkv",
        "C:\\Windows\\System32\\config\\sam.mkv",
    ];
    let mut files: Vec<DownloadFile> = malicious_entries
        .iter()
        .map(|name| DownloadFile {
            name: (*name).to_string(),
            size: 1,
            progress: 1.0,
            wanted: true,
        })
        .collect();
    files.push(DownloadFile {
        name: legit_filename.to_string(),
        size: 13,
        progress: 1.0,
        wanted: true,
    });

    let state = build_test_app_state(db.clone(), None);
    let client = Arc::new(ImportingClient {
        torrent,
        files: files.clone(),
    });
    install_pool(
        &state,
        vec![(1, client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    // ── 1. Legit file landed under media_root/Show Title/Season 01/.
    //    Folder name comes from `seed_series` (which writes the title
    //    as the folder_name); season-dir name is "Season 01" per
    //    `load_series_import_ctx`.
    let season_dir = media_root.path().join("Show Title").join("Season 01");
    assert!(
        season_dir.is_dir(),
        "expected season dir at {} after import",
        season_dir.display()
    );
    let season_entries: Vec<_> = std::fs::read_dir(&season_dir)
        .expect("read season dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .collect();
    let mkv_entries: Vec<String> = season_entries
        .iter()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.to_lowercase().ends_with(".mkv"))
        .collect();
    assert_eq!(
        mkv_entries.len(),
        1,
        "expected exactly one .mkv in {} (just the legit file), found {:?}",
        season_dir.display(),
        mkv_entries
    );

    // ── 2. Each rejected entry produced a PostProcess warn-or-higher
    //    log row whose message identifies the bad name. We only have
    //    insert/timestamp on the `logs` table (no programmatic detail
    //    matchers), so we query everything with category=PostProcess
    //    and grep the messages for both the rejection prefix and the
    //    offending name.
    // `LogCategory::as_str` writes the snake_case slug (`post_process`)
    // — query the column verbatim, not the enum-variant identifier.
    let log_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT level, message FROM logs WHERE category = 'post_process' ORDER BY id",
    )
    .fetch_all(&db)
    .await
    .expect("query logs");
    for entry in &malicious_entries {
        let hit = log_rows.iter().any(|(_lvl, msg)| {
            msg.contains("Rejected suspicious file-list entry") && msg.contains(entry)
        });
        assert!(
            hit,
            "expected a 'Rejected suspicious file-list entry' log for {entry:?}; \
             got messages: {:?}",
            log_rows.iter().map(|(_, m)| m).collect::<Vec<_>>()
        );
    }

    // ── 3. Each malicious entry's basename must NOT have leaked into
    //    media_root or anywhere inside the source's parent. Catches a
    //    regression where the validator accepted but the join
    //    silently produced an in-bounds path (e.g. legit-rooted
    //    subdir traversal that lands back inside media_root via
    //    Path::join collapse). A `walkdir`-style check here would be
    //    heavier; checking the season dir + the source's parent is
    //    enough to surface the attempted leaks the malicious entries
    //    target.
    let source_parent = source_dir.path().parent().expect("tempdir has a parent");
    for marker in [
        "ryokan-issue-117-absolute-leak.mkv",
        "escape-via-parent.mkv",
        "mid-path-escape.mkv",
        "sam.mkv",
    ] {
        assert!(
            !source_parent.join(marker).exists(),
            "malicious filename {marker:?} leaked next to source tempdir at {}",
            source_parent.display()
        );
        assert!(
            !season_dir.join(marker).exists(),
            "malicious filename {marker:?} leaked into season dir {}",
            season_dir.display()
        );
    }

    // ── 4. Sanity: the grab itself transitioned to imported. A
    //    failure here would mean the rejection path ate the legit
    //    sibling too (or some unrelated guard fired) — the rejection
    //    logic must `continue` past only the malicious entries.
    let final_state: String = sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(g)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        final_state, "imported",
        "grab should land in 'imported' once the legit sibling completes; \
         a 'pending' or 'failed' here means rejection blocked the legit file too"
    );
}

// ── Import robustness (#205): the stall timer ────────────────────────
//
// A torrent the client reports complete, with an empty file list and
// no directory Ryokan can walk, comes back `ImportOutcome::NotReady`
// on every tick. These tests drive that shape through `run_once` and
// pin the escalation: stamped on the first complete sighting, failed
// only once the stamp is older than `config.import_stall_hours`,
// never when the setting is 0.

async fn seed_stalled_grab(db: &sqlx::SqlitePool, hash: &str) -> i64 {
    seed_config(db).await;
    let series_id = seed_series(db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(db, hash, "[G] Show - 01", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    insert_dc(
        db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();
    g
}

async fn backdate_completed_seen(db: &sqlx::SqlitePool, id: i64, hours: i64) {
    sqlx::query("UPDATE grabbed_torrents SET completed_seen_at = datetime('now', ?) WHERE id = ?")
        .bind(format!("-{hours} hours"))
        .bind(id)
        .execute(db)
        .await
        .unwrap();
}

async fn state_and_reason(db: &sqlx::SqlitePool, id: i64) -> (String, String, Option<String>) {
    sqlx::query_as(
        "SELECT state, failure_reason, completed_seen_at FROM grabbed_torrents WHERE id = ?",
    )
    .bind(id)
    .fetch_one(db)
    .await
    .unwrap()
}

/// One `run_once` tick against a client that reports `hash` complete.
/// `build_test_app_state` pins `start_time` to 2024, so the boot grace
/// never applies here; `run_with_complete_torrent_booted_at` opts in.
async fn run_with_complete_torrent(db: &sqlx::SqlitePool, hash: &str) {
    let state = build_test_app_state(db.clone(), None);
    run_tick_with_complete_torrent(&state, hash).await;
}

async fn run_with_complete_torrent_booted_at(
    db: &sqlx::SqlitePool,
    hash: &str,
    start_time: chrono::DateTime<chrono::Utc>,
) {
    let mut state = build_test_app_state(db.clone(), None);
    state.start_time = start_time;
    run_tick_with_complete_torrent(&state, hash).await;
}

async fn run_tick_with_complete_torrent(state: &crate::AppState, hash: &str) {
    let client = Arc::new(RecordingClient::new(vec![fake_torrent(
        hash,
        DownloadItemState::Seeding,
    )]));
    install_pool(
        state,
        vec![(1, client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;
    post_processing::run_once(state).await;
}

#[tokio::test]
async fn run_once_stamps_completed_seen_and_keeps_a_fresh_not_ready_grab_pending() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let db = in_memory_pool().await;
    let g = seed_stalled_grab(&db, "stallfresh").await;

    run_with_complete_torrent(&db, "stallfresh").await;

    let (state, reason, seen) = state_and_reason(&db, g).await;
    assert_eq!(state, "pending", "a just-completed grab is not a stall");
    assert_eq!(reason, "");
    assert!(seen.is_some(), "the first complete sighting is stamped");
}

#[tokio::test]
async fn run_once_fails_a_grab_complete_for_longer_than_the_stall_window() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let db = in_memory_pool().await;
    let g = seed_stalled_grab(&db, "stallold").await;
    backdate_completed_seen(&db, g, 25).await;

    run_with_complete_torrent(&db, "stallold").await;

    let (state, reason, seen) = state_and_reason(&db, g).await;
    assert_eq!(state, "failed", "25h past the 24h default is a stall");
    assert_eq!(reason, post_processing::IMPORT_STALLED_REASON);
    assert!(seen.is_some());
    assert!(
        grabbed_torrents::is_blocklisted(&db, "stallold")
            .await
            .unwrap(),
        "the failed row is the blocklist entry"
    );
    let logged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM logs WHERE level = 'warn' AND message LIKE 'Import gave up on%'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(logged, 1, "one PostProcess warn names the reason");
}

#[tokio::test]
async fn run_once_never_escalates_when_the_stall_window_is_zero() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let db = in_memory_pool().await;
    let g = seed_stalled_grab(&db, "stalloff").await;
    backdate_completed_seen(&db, g, 25).await;
    sqlx::query("UPDATE config SET import_stall_hours = 0 WHERE id = 1")
        .execute(&db)
        .await
        .unwrap();

    run_with_complete_torrent(&db, "stalloff").await;

    let (state, reason, seen) = state_and_reason(&db, g).await;
    assert_eq!(state, "pending", "0 keeps the retry-forever behavior");
    assert_eq!(reason, "");
    let seen = seen.expect("stamp kept");
    assert!(
        crate::services::post_processing::grab_is_stale(&seen, 24 * 3600),
        "the tick must not overwrite the original sighting: {seen}"
    );
}

#[tokio::test]
async fn run_once_measures_the_stall_from_completion_not_from_the_grab() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // A download that took two days is not a stall: grabbed_at is old,
    // completed_seen_at is fresh.
    let db = in_memory_pool().await;
    let g = seed_stalled_grab(&db, "slowdl").await;
    sqlx::query(
        "UPDATE grabbed_torrents SET grabbed_at = datetime('now', '-48 hours') WHERE id = ?",
    )
    .bind(g)
    .execute(&db)
    .await
    .unwrap();

    run_with_complete_torrent(&db, "slowdl").await;

    let (state, _, seen) = state_and_reason(&db, g).await;
    assert_eq!(state, "pending");
    assert!(seen.is_some());
}

#[tokio::test]
async fn run_once_holds_the_stall_timer_during_the_boot_grace() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Ryokan was off for longer than the window with this grab already
    // stamped: the stamp is 25h old, but this process has retried it zero
    // times. The first tick after boot must not be the one that fails it.
    let db = in_memory_pool().await;
    let g = seed_stalled_grab(&db, "stallboot").await;
    backdate_completed_seen(&db, g, 25).await;

    run_with_complete_torrent_booted_at(&db, "stallboot", chrono::Utc::now()).await;

    let (state, reason, _) = state_and_reason(&db, g).await;
    assert_eq!(state, "pending", "no escalation inside the boot grace");
    assert_eq!(reason, "");

    // Once the grace is over the same grab fails on the next tick.
    let booted = chrono::Utc::now()
        - chrono::Duration::seconds(post_processing::IMPORT_STALL_BOOT_GRACE_SECS + 1);
    run_with_complete_torrent_booted_at(&db, "stallboot", booted).await;

    let (state, reason, _) = state_and_reason(&db, g).await;
    assert_eq!(state, "failed", "past the grace the 25h stall is judged");
    assert_eq!(reason, post_processing::IMPORT_STALLED_REASON);
}
