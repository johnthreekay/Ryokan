//! Issue #228: removing finished downloads from their client.
//!
//! The import-time half drives the full `run_once` path against a real
//! temp filesystem with a recording mock client, so the assertions are
//! about what the client was told after a real import. The sweep half
//! seeds imported rows and canned client listings and checks
//! `sweep_finished_seeds_now`'s tally plus the `client_removed_at`
//! stamp.

use crate::models::download_clients::{
    DownloadClientForm, insert as insert_dc, set_remove_completed,
};
use crate::models::grabbed_torrents;
use crate::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};
use crate::services::post_processing;
use crate::services::post_processing::client_cleanup::SweepReport;
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::POST_PROC_TEST_SERIALIZER;

/// Mock that records every `delete` and can pose as either protocol.
struct CleanupClient {
    items: Vec<DownloadItem>,
    files: Vec<DownloadFile>,
    protocol: &'static str,
    list_fails: bool,
    deletes: Mutex<Vec<(String, bool)>>,
}

impl CleanupClient {
    fn torrent(items: Vec<DownloadItem>, files: Vec<DownloadFile>) -> Self {
        Self {
            items,
            files,
            protocol: "torrent",
            list_fails: false,
            deletes: Mutex::new(Vec::new()),
        }
    }

    fn usenet(items: Vec<DownloadItem>, files: Vec<DownloadFile>) -> Self {
        Self {
            protocol: "usenet",
            ..Self::torrent(items, files)
        }
    }

    fn failing_list() -> Self {
        Self {
            list_fails: true,
            ..Self::torrent(Vec::new(), Vec::new())
        }
    }

    fn deletes(&self) -> Vec<(String, bool)> {
        self.deletes.lock().unwrap().clone()
    }
}

#[async_trait]
impl DownloadClient for CleanupClient {
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
        if self.list_fails {
            Err("simulated list failure".into())
        } else {
            Ok(self.items.clone())
        }
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
    async fn delete(&self, hash: &str, delete_files: bool) -> Result<(), String> {
        self.deletes
            .lock()
            .unwrap()
            .push((hash.to_string(), delete_files));
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
    fn protocol(&self) -> &'static str {
        self.protocol
    }
}

fn item(
    hash: &str,
    save_path: &Path,
    state_kind: DownloadItemState,
    seeding_done: bool,
) -> DownloadItem {
    DownloadItem {
        hash: hash.to_string(),
        name: format!("item-{hash}"),
        size: 1000,
        progress: 1.0,
        dlspeed: 0,
        state: format!("{state_kind:?}"),
        category: "anime".to_string(),
        eta: 0,
        save_path: save_path.to_string_lossy().into_owned(),
        content_path: String::new(),
        state_kind,
        seeding_done,
    }
}

const FILE: &str = "[Sub] Show - 01.mkv";

fn temp_dirs(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("ryokan-cleanup-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let downloads = root.join("downloads").join("job");
    let media = root.join("media");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&media).unwrap();
    (root, downloads, media)
}

/// Post-processing on with the given mode, plus one download-client row
/// (id 1, matching the mock the pool holds at id 1) whose "Remove
/// completed downloads" switch is `remove`.
async fn seed_config(db: &sqlx::SqlitePool, media_root: &Path, mode: &str, remove: bool) {
    sqlx::query(
        "INSERT INTO config (id, post_processing_enabled, media_root, post_processing_mode) \
         VALUES (1, 1, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET post_processing_enabled = 1, media_root = excluded.media_root, \
             post_processing_mode = excluded.post_processing_mode",
    )
    .bind(media_root.to_string_lossy().as_ref())
    .bind(mode)
    .execute(db)
    .await
    .unwrap();
    let id = insert_dc(
        db,
        DownloadClientForm {
            name: "mock",
            kind: "qbittorrent",
            url: "http://mock",
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
    assert_eq!(id, 1, "the row must line up with the pool's id 1");
    set_remove_completed(db, id, remove).await.unwrap();
}

async fn client_removed_at(db: &sqlx::SqlitePool, id: i64) -> Option<String> {
    grabbed_torrents::client_removed_at(db, id).await
}

/// Full import of one file through `run_once` with the mock at pool id 1.
async fn live_import(
    tag: &str,
    hash: &str,
    mode: &str,
    remove: bool,
    usenet: bool,
) -> (
    PathBuf,
    PathBuf,
    PathBuf,
    sqlx::SqlitePool,
    i64,
    Arc<CleanupClient>,
) {
    let (root, downloads, media) = temp_dirs(tag);
    std::fs::write(downloads.join(FILE), b"fake video bytes").unwrap();
    let db = in_memory_pool().await;
    seed_config(&db, &media, mode, remove).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let grab_id =
        grabbed_torrents::record_grab(&db, hash, "[Sub] Show - 01", series_id, &[1], false)
            .await
            .unwrap()
            .unwrap();
    grabbed_torrents::set_download_client(&db, grab_id, Some(1))
        .await
        .unwrap();
    let items = vec![item(hash, &downloads, DownloadItemState::Seeding, false)];
    let files = vec![DownloadFile {
        name: FILE.to_string(),
        size: 16,
        progress: 1.0,
        wanted: true,
    }];
    let client = Arc::new(if usenet {
        CleanupClient::usenet(items, files)
    } else {
        CleanupClient::torrent(items, files)
    });
    let state = build_test_app_state(db.clone(), Some(client.clone()));
    post_processing::run_once(&state).await;
    (root, downloads, media, db, grab_id, client)
}

fn media_files(media: &Path) -> usize {
    walkdir::WalkDir::new(media)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "mkv"))
        .count()
}

#[tokio::test]
async fn move_mode_import_removes_the_torrent_with_files() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media, db, grab_id, client) =
        live_import("move", "movehash", "move", true, false).await;

    assert_eq!(media_files(&media), 1, "the episode landed in the library");
    assert!(
        !downloads.join(FILE).exists(),
        "move mode consumed the source"
    );
    assert_eq!(client.deletes(), vec![("movehash".to_string(), true)]);
    assert!(client_removed_at(&db, grab_id).await.is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn hardlink_import_leaves_the_torrent_seeding() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media, db, grab_id, client) =
        live_import("hardlink", "hlhash", "hardlink", true, false).await;

    assert_eq!(media_files(&media), 1);
    assert!(downloads.join(FILE).exists(), "the source keeps seeding");
    assert!(
        client.deletes().is_empty(),
        "hardlink mode waits for the sweep"
    );
    assert!(client_removed_at(&db, grab_id).await.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn usenet_import_removes_the_job_and_its_leftover_folder() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media, db, grab_id, client) =
        live_import("usenet", "SABnzbd_nzo_abc", "hardlink", true, true).await;

    assert_eq!(media_files(&media), 1);
    assert_eq!(
        client.deletes(),
        vec![("SABnzbd_nzo_abc".to_string(), true)]
    );
    assert!(
        !downloads.join(FILE).exists(),
        "the stamped source is unlinked even when SAB's del_files no-ops"
    );
    assert!(!downloads.exists(), "the empty job folder is pruned");
    assert!(
        downloads.parent().unwrap().exists(),
        "the complete root itself is never removed"
    );
    assert!(client_removed_at(&db, grab_id).await.is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn client_switch_off_keeps_move_mode_torrents_in_the_client() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, _downloads, media, db, grab_id, client) =
        live_import("off", "offhash", "move", false, false).await;

    assert_eq!(media_files(&media), 1);
    assert!(client.deletes().is_empty());
    assert!(client_removed_at(&db, grab_id).await.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

// ── The finished-seed sweep ──────────────────────────────────────────

async fn seed_imported(db: &sqlx::SqlitePool, hash: &str) -> i64 {
    let series_id = match sqlx::query_scalar::<_, i64>("SELECT id FROM series WHERE anilist_id = 1")
        .fetch_optional(db)
        .await
        .unwrap()
    {
        Some(id) => id,
        None => seed_series(db, 1, "Show").await,
    };
    let id = grabbed_torrents::record_grab(db, hash, "[Sub] Show - 01", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::mark_imported(db, id).await.unwrap();
    grabbed_torrents::set_download_client(db, id, Some(1))
        .await
        .unwrap();
    id
}

async fn sweep(db: &sqlx::SqlitePool, client: Arc<CleanupClient>) -> SweepReport {
    let state = build_test_app_state(db.clone(), Some(client));
    post_processing::sweep_finished_seeds_now(&state)
        .await
        .unwrap()
}

#[tokio::test]
async fn sweep_removes_an_item_whose_seeding_rules_are_met() {
    let db = in_memory_pool().await;
    seed_config(&db, Path::new("/tmp/x"), "hardlink", true).await;
    let id = seed_imported(&db, "donehash").await;
    let client = Arc::new(CleanupClient::torrent(
        vec![item(
            "donehash",
            Path::new("/dl"),
            DownloadItemState::PausedComplete,
            true,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.removed, 1, "{report:?}");
    assert_eq!(client.deletes(), vec![("donehash".to_string(), true)]);
    assert!(client_removed_at(&db, id).await.is_some());
}

#[tokio::test]
async fn sweep_waits_while_an_item_is_still_seeding() {
    let db = in_memory_pool().await;
    seed_config(&db, Path::new("/tmp/x"), "hardlink", true).await;
    let id = seed_imported(&db, "seedhash").await;
    let client = Arc::new(CleanupClient::torrent(
        vec![item(
            "seedhash",
            Path::new("/dl"),
            DownloadItemState::Seeding,
            false,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.waiting, 1, "{report:?}");
    assert!(client.deletes().is_empty());
    assert!(client_removed_at(&db, id).await.is_none());
}

#[tokio::test]
async fn sweep_stamps_an_item_the_client_no_longer_has() {
    let db = in_memory_pool().await;
    seed_config(&db, Path::new("/tmp/x"), "hardlink", true).await;
    let id = seed_imported(&db, "gonehash").await;
    let client = Arc::new(CleanupClient::torrent(vec![], vec![]));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.gone, 1, "{report:?}");
    assert!(client.deletes().is_empty(), "nothing to delete");
    assert!(client_removed_at(&db, id).await.is_some());
    // Stamped rows leave the work list: a second pass finds nothing.
    let again = sweep(&db, client).await;
    assert_eq!(again, SweepReport::default());
}

#[tokio::test]
async fn sweep_removes_move_mode_rows_whose_sources_left_their_folder() {
    // The move-mode rule: the row was imported in move mode (its own
    // stamp, or the current mode for a row stamped before 1.9.3), its
    // stamped source files are gone, and their folder is still there.
    let db = in_memory_pool().await;
    let (root, downloads, _media) = temp_dirs("sweep-move");
    seed_config(&db, Path::new("/tmp/x"), "hardlink", true).await;
    let stamped_move = seed_imported(&db, "stampedmove").await;
    grabbed_torrents::stamp_import_mode(&db, stamped_move, "move")
        .await
        .unwrap();
    let legacy = seed_imported(&db, "legacymove").await;
    for id in [stamped_move, legacy] {
        grabbed_torrents::stamp_imported_source_paths(
            &db,
            id,
            &[downloads.join(FILE).to_string_lossy().into_owned()],
        )
        .await
        .unwrap();
    }
    let client = Arc::new(CleanupClient::torrent(
        vec![
            item("stampedmove", &downloads, DownloadItemState::Seeding, false),
            item("legacymove", &downloads, DownloadItemState::Seeding, false),
        ],
        vec![],
    ));

    // Current mode is hardlink: only the row stamped as move qualifies.
    let report = sweep(&db, client.clone()).await;
    assert_eq!((report.removed, report.waiting), (1, 1), "{report:?}");
    assert_eq!(client.deletes(), vec![("stampedmove".to_string(), true)]);
    assert!(client_removed_at(&db, stamped_move).await.is_some());
    assert!(client_removed_at(&db, legacy).await.is_none());

    // Switching the current mode to move makes the unstamped legacy row
    // qualify too.
    sqlx::query("UPDATE config SET post_processing_mode = 'move' WHERE id = 1")
        .execute(&db)
        .await
        .unwrap();
    let report = sweep(&db, client.clone()).await;
    assert_eq!(report.removed, 1, "{report:?}");
    assert!(client_removed_at(&db, legacy).await.is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn sweep_waits_on_an_errored_item_and_on_a_missing_source_folder() {
    // An errored state is never a reason on its own, and a stamped
    // source whose folder is gone reads as a dropped mount, not a moved
    // file, even in move mode.
    let db = in_memory_pool().await;
    let (root, downloads, _media) = temp_dirs("sweep-errored");
    std::fs::write(downloads.join(FILE), b"x").unwrap();
    seed_config(&db, Path::new("/tmp/x"), "move", true).await;
    let errored = seed_imported(&db, "errhash").await;
    grabbed_torrents::stamp_imported_source_paths(
        &db,
        errored,
        &[downloads.join(FILE).to_string_lossy().into_owned()],
    )
    .await
    .unwrap();
    let unmounted = seed_imported(&db, "unmounted").await;
    grabbed_torrents::stamp_imported_source_paths(
        &db,
        unmounted,
        &["/nonexistent/ryokan-test/job/Show - 01.mkv".to_string()],
    )
    .await
    .unwrap();
    let client = Arc::new(CleanupClient::torrent(
        vec![
            item("errhash", &downloads, DownloadItemState::Errored, false),
            item("unmounted", &downloads, DownloadItemState::Seeding, false),
        ],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.waiting, 2, "{report:?}");
    assert!(client.deletes().is_empty());
    assert!(client_removed_at(&db, errored).await.is_none());
    assert!(client_removed_at(&db, unmounted).await.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn sweep_keeps_a_hardlink_row_after_the_mode_is_switched_to_move() {
    let db = in_memory_pool().await;
    let (root, downloads, _media) = temp_dirs("sweep-switched");
    seed_config(&db, Path::new("/tmp/x"), "move", true).await;
    let id = seed_imported(&db, "wasHardlink").await;
    grabbed_torrents::stamp_import_mode(&db, id, "hardlink")
        .await
        .unwrap();
    grabbed_torrents::stamp_imported_source_paths(
        &db,
        id,
        &[downloads.join(FILE).to_string_lossy().into_owned()],
    )
    .await
    .unwrap();
    let client = Arc::new(CleanupClient::torrent(
        vec![item(
            "wasHardlink",
            &downloads,
            DownloadItemState::Seeding,
            false,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.waiting, 1, "{report:?}");
    assert!(client.deletes().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn sweep_ignores_rows_advanced_without_an_import() {
    // Post-processing off marks a finished download 'imported' with no
    // imported_at: the download is the user's only copy.
    let db = in_memory_pool().await;
    seed_config(&db, Path::new("/tmp/x"), "hardlink", true).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let id =
        grabbed_torrents::record_grab(&db, "noimport", "[Sub] Show - 01", series_id, &[1], false)
            .await
            .unwrap()
            .unwrap();
    grabbed_torrents::mark_completed_no_import(&db, id)
        .await
        .unwrap();
    grabbed_torrents::set_download_client(&db, id, Some(1))
        .await
        .unwrap();
    let client = Arc::new(CleanupClient::torrent(
        vec![item(
            "noimport",
            Path::new("/dl"),
            DownloadItemState::PausedComplete,
            true,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report, SweepReport::default());
    assert!(client.deletes().is_empty());
    assert!(client_removed_at(&db, id).await.is_none());
}

#[tokio::test]
async fn sweep_never_touches_a_partial_import() {
    // The episodes that failed are still only in the download folder;
    // move mode would otherwise read the succeeded files' absence as
    // "sources gone" and delete the lot.
    let db = in_memory_pool().await;
    let (root, downloads, _media) = temp_dirs("sweep-partial");
    seed_config(&db, Path::new("/tmp/x"), "move", true).await;
    let id = seed_imported(&db, "partialhash").await;
    grabbed_torrents::stamp_import_mode(&db, id, "partial")
        .await
        .unwrap();
    grabbed_torrents::stamp_imported_source_paths(
        &db,
        id,
        &[downloads.join(FILE).to_string_lossy().into_owned()],
    )
    .await
    .unwrap();
    let client = Arc::new(CleanupClient::torrent(
        vec![item(
            "partialhash",
            &downloads,
            DownloadItemState::PausedComplete,
            true,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report, SweepReport::default());
    assert!(client.deletes().is_empty());
    assert!(client_removed_at(&db, id).await.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn sweep_does_nothing_while_post_processing_is_off() {
    let db = in_memory_pool().await;
    seed_config(&db, Path::new("/tmp/x"), "hardlink", true).await;
    let id = seed_imported(&db, "ppoff").await;
    sqlx::query("UPDATE config SET post_processing_enabled = 0 WHERE id = 1")
        .execute(&db)
        .await
        .unwrap();
    let client = Arc::new(CleanupClient::torrent(
        vec![item(
            "ppoff",
            Path::new("/dl"),
            DownloadItemState::PausedComplete,
            true,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report, SweepReport::default());
    assert!(client.deletes().is_empty());
    assert!(client_removed_at(&db, id).await.is_none());
}

#[tokio::test]
async fn sweep_never_uses_the_missing_source_rule_outside_move_mode() {
    // A hardlink-mode source that is missing while its folder exists
    // (someone cleaned the download folder) is not a reason either.
    let db = in_memory_pool().await;
    let (root, downloads, _media) = temp_dirs("sweep-hardlink-missing");
    seed_config(&db, Path::new("/tmp/x"), "hardlink", true).await;
    let id = seed_imported(&db, "mounthash").await;
    grabbed_torrents::stamp_imported_source_paths(
        &db,
        id,
        &[downloads.join(FILE).to_string_lossy().into_owned()],
    )
    .await
    .unwrap();
    let client = Arc::new(CleanupClient::torrent(
        vec![item(
            "mounthash",
            &downloads,
            DownloadItemState::Seeding,
            false,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.waiting, 1, "{report:?}");
    assert!(client.deletes().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn sweep_removes_legacy_usenet_jobs_and_their_sources() {
    let db = in_memory_pool().await;
    let (root, downloads, media) = temp_dirs("sweep-usenet");
    let source = downloads.join(FILE);
    std::fs::write(&source, b"x").unwrap();
    seed_config(&db, &media, "hardlink", true).await;
    let id = seed_imported(&db, "SABnzbd_nzo_old").await;
    grabbed_torrents::stamp_imported_source_paths(
        &db,
        id,
        &[source.to_string_lossy().into_owned()],
    )
    .await
    .unwrap();
    let client = Arc::new(CleanupClient::usenet(
        vec![item(
            "SABnzbd_nzo_old",
            &downloads,
            DownloadItemState::PausedComplete,
            false,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.removed, 1, "{report:?}");
    assert_eq!(
        client.deletes(),
        vec![("SABnzbd_nzo_old".to_string(), true)]
    );
    assert!(!source.exists(), "SAB's leftover source is unlinked");
    assert!(!downloads.exists(), "the job folder is pruned");
    assert!(client_removed_at(&db, id).await.is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn sweep_does_nothing_when_the_client_switch_is_off() {
    let db = in_memory_pool().await;
    seed_config(&db, Path::new("/tmp/x"), "hardlink", false).await;
    let id = seed_imported(&db, "offhash2").await;
    let client = Arc::new(CleanupClient::torrent(
        vec![item(
            "offhash2",
            Path::new("/dl"),
            DownloadItemState::PausedComplete,
            true,
        )],
        vec![],
    ));

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.disabled, 1, "{report:?}");
    assert_eq!(report.removed, 0);
    assert!(client.deletes().is_empty());
    assert!(client_removed_at(&db, id).await.is_none());
}

#[tokio::test]
async fn sweep_leaves_rows_alone_when_the_client_cannot_be_listed() {
    let db = in_memory_pool().await;
    seed_config(&db, Path::new("/tmp/x"), "hardlink", true).await;
    let id = seed_imported(&db, "downhash").await;
    let client = Arc::new(CleanupClient::failing_list());

    let report = sweep(&db, client.clone()).await;

    assert_eq!(report.waiting, 1, "{report:?}");
    assert!(client.deletes().is_empty());
    assert!(
        client_removed_at(&db, id).await.is_none(),
        "an unreachable client must not read as 'item gone'"
    );
}
