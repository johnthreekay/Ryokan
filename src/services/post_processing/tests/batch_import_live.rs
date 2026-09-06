//! Live end-to-end batch-import proof for the PR #198 guards. Unlike
//! the `run_once.rs` fan-out tests (which stop short of
//! `import_torrent`), these drive the FULL import path — readiness
//! gate → batch preflight → import loop → real `do_file_op` calls on
//! a real temp filesystem — with only the download client mocked.
//!
//! Guards under proof:
//!   1. Unparseable extras (NCOP/NCED) are skipped while the
//!      parseable episodes import, including a dot-delimited name
//!      whose `H.264` token must not mis-parse as episode 264.
//!   2. Two files resolving to one destination slot fail the whole
//!      grab before any mutation (nothing lands in the library,
//!      sources untouched).
//!   3. An incomplete wanted video holds the entire batch in
//!      `pending` (readiness gate) rather than importing a subset.
//!   4. `E05` + `E05v2` in one pack imports the v2 and skips the v1
//!      (issue #204).
//!   5. PV / bare-SP / recap extras from the pinned corpus packs are
//!      skipped instead of colliding with real slots (issue #203).
//!   6. An upgrade lands the new file before the old one is retired,
//!      so a failed placement leaves the library and the old grab
//!      untouched (issue #202).

use crate::models::grabbed_torrents;
use crate::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};
use crate::services::post_processing;
use crate::test_support::{
    build_test_app_state, in_memory_pool, seed_grabbed_torrent, seed_series,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::POST_PROC_TEST_SERIALIZER;

/// Minimal mock: one canned complete torrent plus a canned file list.
/// Everything else is inert. `save_path` points at a real temp dir the
/// test populated, so `import_torrent` walks and moves real files.
struct BatchClient {
    torrent: DownloadItem,
    files: Vec<DownloadFile>,
    /// `delete` calls seen, so an upgrade test can assert the old
    /// torrent left the client (or, on a failed placement, did not).
    deleted: AtomicUsize,
}

impl BatchClient {
    fn new(torrent: DownloadItem, files: Vec<DownloadFile>) -> Self {
        Self {
            torrent,
            files,
            deleted: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl DownloadClient for BatchClient {
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
        self.deleted.fetch_add(1, Ordering::SeqCst);
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

fn complete_torrent(hash: &str, save_path: &Path) -> DownloadItem {
    DownloadItem {
        hash: hash.to_string(),
        name: format!("torrent-{hash}"),
        size: 1000,
        progress: 1.0,
        dlspeed: 0,
        state: "seeding".to_string(),
        category: "anime".to_string(),
        eta: 0,
        save_path: save_path.to_string_lossy().into_owned(),
        content_path: String::new(),
        state_kind: DownloadItemState::Seeding,
        seeding_done: false,
    }
}

fn complete_file(name: &str) -> DownloadFile {
    DownloadFile {
        name: name.to_string(),
        size: 100,
        progress: 1.0,
        wanted: true,
    }
}

/// Fresh per-test temp tree: `<tmp>/ryokan-live-<tag>-<pid>/{downloads,media}`.
/// Any stale tree from a crashed prior run is cleared first.
fn temp_dirs(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("ryokan-live-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let downloads = root.join("downloads");
    let media = root.join("media");
    std::fs::create_dir_all(&downloads).expect("create downloads dir");
    std::fs::create_dir_all(&media).expect("create media dir");
    (root, downloads, media)
}

async fn seed_config(db: &sqlx::SqlitePool, media_root: &Path) {
    sqlx::query(
        "INSERT INTO config (id, post_processing_enabled, media_root) \
         VALUES (1, 1, ?) \
         ON CONFLICT(id) DO UPDATE SET post_processing_enabled = 1, media_root = excluded.media_root",
    )
    .bind(media_root.to_string_lossy().as_ref())
    .execute(db)
    .await
    .expect("seed config row");
}

/// Recursively collect basenames of files with the given extension.
fn collect_files(root: &Path, ext: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

async fn grab_state(db: &sqlx::SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await
        .expect("fetch grab state")
}

#[tokio::test]
async fn live_batch_import_skips_extras_and_imports_episodes() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("extras");

    // Real files on disk, shapes lifted from the scraped Nyaa corpus:
    // three dash-form episodes, one dot-form episode whose `H.264`
    // token must not mis-parse as E264, and two NC extras that parse
    // to `None` by design.
    let names = [
        "[Moozzi2] Anne of Green Gables - 01 (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables - 02 (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables - 03 (BD 1440x1080 x.265 Flac).mkv",
        "Anne.of.Green.Gables.04.BD.1080p.H.264.mkv",
        "[Moozzi2] Anne of Green Gables [SP01] NCOP (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables [SP02] NCED (BD 1440x1080 x.265 Flac).mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9001, "Anne of Green Gables").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-extras",
        "[Moozzi2] Anne of Green Gables (TV + SP)",
        series_id,
        &[1, 2, 3, 4],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-extras", &downloads),
        names.iter().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    let imported = collect_files(&media, "mkv");
    assert_eq!(
        imported,
        vec![
            "Anne of Green Gables - S01E01.mkv".to_string(),
            "Anne of Green Gables - S01E02.mkv".to_string(),
            "Anne of Green Gables - S01E03.mkv".to_string(),
            "Anne of Green Gables - S01E04.mkv".to_string(),
        ],
        "episodes 1-4 import (dot-form E04 must not become E264); NC extras stay out"
    );
    // Hardlink mode: sources remain for continued seeding.
    assert_eq!(collect_files(&downloads, "mkv").len(), 6);
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_prefers_highest_version_in_slot() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("version");

    // `05` and `05v2` both resolve to episode 5 (issue #204): the v2
    // lands, the v1 is skipped, and the unambiguous E06 imports.
    let names = [
        ("Show - 05 (720p).mkv", b"v1 bytes".as_slice()),
        ("Show - 05v2 (1080p).mkv", b"v2 bytes".as_slice()),
        ("Show - 06 (1080p).mkv", b"e6 bytes".as_slice()),
    ];
    for (name, bytes) in &names {
        std::fs::write(downloads.join(name), bytes).expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9002, "Show").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-version",
        "Show 05-06 pack",
        series_id,
        &[5, 6],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-version", &downloads),
        names.iter().map(|(n, _)| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        vec![
            "Show - S01E05.mkv".to_string(),
            "Show - S01E06.mkv".to_string()
        ],
        "v2 wins the E05 slot; E06 imports alongside"
    );
    let e05 = std::fs::read(
        media
            .join("Show")
            .join("Season 01")
            .join("Show - S01E05.mkv"),
    )
    .expect("read E05");
    assert_eq!(
        e05, b"v2 bytes",
        "the higher version is the one that landed"
    );
    assert_eq!(
        collect_files(&downloads, "mkv").len(),
        3,
        "sources untouched (hardlink mode); the skipped v1 stays put"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_fails_closed_on_slot_collision() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("collision");

    // Two unversioned files resolve to episode 5 — a true duplicate
    // (or a mis-parse), so the whole grab must fail before ANY
    // mutation, including the unambiguous E06.
    let names = [
        "Show - 05 (720p).mkv",
        "Show - 05 (1080p).mkv",
        "Show - 06 (1080p).mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9005, "Show").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-collision",
        "Show 05-06 pack",
        series_id,
        &[5, 6],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-collision", &downloads),
        names.iter().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        Vec::<String>::new(),
        "duplicate destination must abort before any file lands"
    );
    assert_eq!(
        collect_files(&downloads, "mkv").len(),
        3,
        "sources untouched"
    );
    assert_eq!(grab_state(&db, grab_id).await, "failed");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_skips_corpus_pack_extras() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("corpus-extras");

    // Issue #203: the Erai-raws 86 pack's `SP` and `07_5` recap and
    // Moozzi2's `PV` files used to collide with E02 / E07 / E01 and
    // fail the whole pack closed. Now they are skipped and logged.
    let names = [
        "[Erai-raws] 86 Eighty-Six Part 2 - 02 [480p][Multiple Subtitle][2E7DEB0E].mkv",
        "[Erai-raws] 86 Eighty-Six Part 2 - 07 [480p][Multiple Subtitle][E9DFD7E8].mkv",
        "[Erai-raws] 86 Eighty-Six Part 2 - 07_5 [480p][Multiple Subtitle][3CC7C577].mkv",
        "[Erai-raws] 86 Eighty-Six Part 2 - SP [480p][Multiple Subtitle][FDBE49E5].mkv",
        "[Erai-raws] 86 Eighty-Six Part 2 - Character PV 1 [480p][ABCDEF01].mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9006, "86 Eighty-Six Part 2").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-corpus-extras",
        "[Erai-raws] 86 Eighty-Six Part 2 - 01 ~ 12 [480p][BATCH]",
        series_id,
        &[2, 7],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-corpus-extras", &downloads),
        names.iter().map(|n| complete_file(n)).collect(),
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        vec![
            "86 Eighty-Six Part 2 - S01E02.mkv".to_string(),
            "86 Eighty-Six Part 2 - S01E07.mkv".to_string(),
        ],
        "episodes import; SP / recap / PV extras stay out"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

/// Library state for an upgrade test: `<media>/Show/Season 01/Show -
/// S01E05.mkv` holding `old bytes`, backed by an imported grab row for
/// episode 5 so `find_imported_for_episode` sees a real predecessor.
async fn seed_imported_e05(db: &sqlx::SqlitePool, media: &Path, series_id: i64) -> (PathBuf, i64) {
    let season_dir = media.join("Show").join("Season 01");
    std::fs::create_dir_all(&season_dir).expect("create season dir");
    let dest = season_dir.join("Show - S01E05.mkv");
    std::fs::write(&dest, b"old bytes").expect("write old file");
    seed_grabbed_torrent(db, series_id, "oldhash-e05", "Show - 05 (720p)", &[5]).await;
    // Looked up by hash: `last_insert_rowid()` is per-connection and the
    // pool may answer the follow-up SELECT from a different one.
    sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE hash = 'oldhash-e05'")
        .execute(db)
        .await
        .expect("mark old grab imported");
    let old_grab_id: i64 =
        sqlx::query_scalar("SELECT id FROM grabbed_torrents WHERE hash = 'oldhash-e05'")
            .fetch_one(db)
            .await
            .expect("old grab id");
    (dest, old_grab_id)
}

#[tokio::test]
async fn live_upgrade_keeps_old_file_when_placement_fails() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("upgrade-fail");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9007, "Show").await;
    let (dest, old_grab_id) = seed_imported_e05(&db, &media, series_id).await;

    // Block the staging path with a directory so the placement fails
    // (hardlink returns EEXIST, the copy fallback EISDIR) before the
    // old file is touched. Issue #202: the pre-fix order had already
    // deleted the old file and the old torrent by this point.
    std::fs::create_dir_all(post_processing::staging_path(&dest)).expect("block staging path");

    std::fs::write(downloads.join("Show - 05v2 (1080p).mkv"), b"new bytes").expect("write source");
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-upgrade-fail",
        "Show - 05v2 (1080p)",
        series_id,
        &[5],
        false,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-upgrade-fail", &downloads),
        vec![complete_file("Show - 05v2 (1080p).mkv")],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));

    post_processing::run_once(&state).await;

    assert_eq!(
        std::fs::read(&dest).expect("old file still readable"),
        b"old bytes",
        "a failed placement must leave the old file exactly as it was"
    );
    assert_eq!(
        collect_files(&media, "mkv"),
        vec!["Show - S01E05.mkv".to_string()]
    );
    assert_eq!(
        grab_state(&db, old_grab_id).await,
        "imported",
        "the old grab is not marked replaced"
    );
    assert_eq!(
        client.deleted.load(Ordering::SeqCst),
        0,
        "the old torrent stays in the client"
    );
    assert_eq!(grab_state(&db, grab_id).await, "failed");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_upgrade_swaps_new_file_in_after_placement() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("upgrade-ok");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9008, "Show").await;
    let (dest, old_grab_id) = seed_imported_e05(&db, &media, series_id).await;

    std::fs::write(downloads.join("Show - 05v2 (1080p).mkv"), b"new bytes").expect("write source");
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-upgrade-ok",
        "Show - 05v2 (1080p)",
        series_id,
        &[5],
        false,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-upgrade-ok", &downloads),
        vec![complete_file("Show - 05v2 (1080p).mkv")],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));

    post_processing::run_once(&state).await;

    assert_eq!(
        std::fs::read(&dest).expect("new file readable"),
        b"new bytes",
        "the upgrade replaced the old file in place"
    );
    assert_eq!(
        collect_files(&media, "mkv"),
        vec!["Show - S01E05.mkv".to_string()]
    );
    assert!(
        !post_processing::staging_path(&dest).exists(),
        "no staged file is left behind after the swap"
    );
    assert_eq!(grab_state(&db, old_grab_id).await, "replaced");
    assert_eq!(
        client.deleted.load(Ordering::SeqCst),
        1,
        "the old torrent leaves the client after the swap"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_waits_for_incomplete_wanted_video() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("notready");

    let names = ["Show - 01 (1080p).mkv", "Show - 02 (1080p).mkv"];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9003, "Show").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-notready",
        "Show pack",
        series_id,
        &[1, 2],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    // E02 is wanted but only half done — the whole batch must wait.
    let mut files: Vec<DownloadFile> = names.iter().map(|n| complete_file(n)).collect();
    files[1].progress = 0.5;

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-notready", &downloads),
        files,
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        Vec::<String>::new(),
        "no partial import while a wanted video is incomplete"
    );
    assert_eq!(grab_state(&db, grab_id).await, "pending");

    let _ = std::fs::remove_dir_all(&root);
}

// ── Naming templates (issue #124) ───────────────────────────────────

#[tokio::test]
async fn live_import_names_files_from_the_templates() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("naming");

    let name = "[SubsPlease] Show - 05 (1080p WEB-DL) [ABCD1234].mkv";
    std::fs::write(downloads.join(name), b"bytes").expect("write source");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    sqlx::query(
        "UPDATE config SET season_folder_format = 'S{season.number:00}', \
         episode_file_format = '[{group}] {series.title} - {episode.number:00} [{quality.full}]{ext}' \
         WHERE id = 1",
    )
    .execute(&db)
    .await
    .expect("set templates");
    let series_id = seed_series(&db, 9009, "Show").await;
    let grab_id =
        grabbed_torrents::record_grab(&db, "livehash-naming", name, series_id, &[5], false)
            .await
            .unwrap()
            .unwrap();

    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-naming", &downloads),
        vec![complete_file(name)],
    ));
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    let season_dir = media.join("Show").join("S01");
    assert!(season_dir.is_dir(), "season folder follows its template");
    assert_eq!(
        collect_files(&media, "mkv"),
        vec!["[SubsPlease] Show - 05 [1080p WEB-DL].mkv".to_string()],
        "group and quality tokens read the filename when no tag row exists"
    );
    assert!(
        season_dir
            .join("[SubsPlease] Show - 05 [1080p WEB-DL].nfo")
            .is_file(),
        "the NFO shares the rendered stem"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_upgrade_finds_the_old_file_named_under_another_template() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("naming-upgrade");

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9010, "Show").await;
    // The library holds E05 under an older, dash-style template. The
    // default template now names it `Show - S01E05.mkv`; the existing
    // file must still be recognized as the slot's occupant.
    let season_dir = media.join("Show").join("Season 01");
    std::fs::create_dir_all(&season_dir).expect("season dir");
    let old = season_dir.join("[Old] Show - 05 [720p].mkv");
    std::fs::write(&old, b"old bytes").expect("old file");
    seed_grabbed_torrent(
        &db,
        series_id,
        "oldhash-naming",
        "[Old] Show - 05 [720p]",
        &[5],
    )
    .await;
    sqlx::query("UPDATE grabbed_torrents SET state = 'imported' WHERE hash = 'oldhash-naming'")
        .execute(&db)
        .await
        .unwrap();

    std::fs::write(downloads.join("Show - 05v2 (1080p).mkv"), b"new bytes").expect("source");
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-naming-upgrade",
        "Show - 05v2 (1080p)",
        series_id,
        &[5],
        false,
    )
    .await
    .unwrap()
    .unwrap();
    let client = Arc::new(BatchClient::new(
        complete_torrent("livehash-naming-upgrade", &downloads),
        vec![complete_file("Show - 05v2 (1080p).mkv")],
    ));
    let state = build_test_app_state(db.clone(), Some(client.clone()));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        vec!["Show - S01E05.mkv".to_string()],
        "the old file is retired and the new one lands under the current template"
    );
    assert!(!old.exists());
    assert_eq!(client.deleted.load(Ordering::SeqCst), 1);
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}
