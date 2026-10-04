//! Recycle-bin service tests (#123). Everything runs against a
//! `tempfile::TempDir`; the cross-filesystem path is exercised through the
//! `force_copy` seam on `recycle_blocking` / `move_path` because a second
//! filesystem isn't available in CI.

use super::helpers::{companions, is_valid_entry_id, move_path, verify_size, verify_tree_size};
use super::*;
use crate::test_support;
use std::fs;
use std::path::Path;

fn write(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

/// A series folder with one episode plus companions, and a sibling
/// episode whose stem is a prefix-extension of the first (`E07` vs
/// `E070`) so the companion sweep has something to get wrong.
fn seed_series(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let season = root.join("media").join("Show (2020)").join("Season 01");
    let video = season.join("Show - S01E07.mkv");
    write(&video, b"video-bytes-0123456789");
    write(&season.join("Show - S01E07.nfo"), b"<nfo/>");
    write(&season.join("Show - S01E07.en.srt"), b"1\n");
    write(&season.join("Show - S01E07-thumb.jpg"), b"jpg");
    write(&season.join("Show - S01E070.mkv"), b"other-episode");
    write(&season.join("Show - S01E070.nfo"), b"<nfo/>");
    write(&season.join("Show - S01E08.mkv"), b"eight");
    (season, video)
}

#[test]
fn companions_match_stem_plus_separator_only() {
    let tmp = tempfile::tempdir().unwrap();
    let (season, video) = seed_series(tmp.path());
    let found: Vec<String> = companions(&video)
        .into_iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        found,
        vec![
            "Show - S01E07-thumb.jpg".to_string(),
            "Show - S01E07.en.srt".to_string(),
            "Show - S01E07.nfo".to_string(),
        ]
    );
    // The prefix-extension sibling and its nfo must survive untouched.
    assert!(season.join("Show - S01E070.mkv").exists());
    assert!(!found.iter().any(|f| f.contains("E070")));
    assert!(!found.iter().any(|f| f.contains("E08")));
}

#[test]
fn entry_id_shape_is_enforced() {
    assert!(is_valid_entry_id("a4e2f1b8"));
    assert!(!is_valid_entry_id("../../etc"));
    assert!(!is_valid_entry_id("a4e2f1b"));
    assert!(!is_valid_entry_id("A4E2F1B8Z"));
    assert!(is_valid_entry_id(&super::helpers::new_entry_id()));
}

#[tokio::test]
async fn recycle_episode_same_fs_moves_video_and_companions_with_manifest() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let (season, video) = seed_series(tmp.path());
    let bin = tmp.path().join("recycle");

    let out = recycle(
        &db,
        bin.to_str().unwrap(),
        RecycleKind::Episode,
        Some(42),
        "Show",
        &video,
    )
    .await
    .unwrap();
    let RecycleOutcome::Recycled { entry_id } = out else {
        panic!("expected Recycled, got {out:?}");
    };
    assert!(is_valid_entry_id(&entry_id));

    // Originals gone, sibling episodes untouched.
    assert!(!video.exists());
    assert!(!season.join("Show - S01E07.nfo").exists());
    assert!(!season.join("Show - S01E07.en.srt").exists());
    assert!(!season.join("Show - S01E07-thumb.jpg").exists());
    assert!(season.join("Show - S01E070.mkv").exists());
    assert!(season.join("Show - S01E08.mkv").exists());

    let entries = list_entries(bin.to_str().unwrap()).await.unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.entry_id, entry_id);
    assert_eq!(e.manifest.kind, RecycleKind::Episode);
    assert_eq!(e.manifest.series_id, Some(42));
    assert_eq!(e.manifest.series_title, "Show");
    assert_eq!(e.manifest.original_path, video.display().to_string());
    assert_eq!(e.manifest.files.len(), 4);
    assert_eq!(e.manifest.files.last().unwrap(), "Show - S01E07.mkv");
    assert_eq!(e.manifest.size_bytes, 22 + 6 + 2 + 3);
    for f in &e.manifest.files {
        assert!(e.dir.join(f).is_file(), "{f} missing from entry dir");
    }
    assert!(e.dir.join(MANIFEST_FILE).is_file());
    assert!(!is_unwritable());
}

#[tokio::test]
async fn recycle_series_folder_moves_whole_directory() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_season, _video) = seed_series(tmp.path());
    let series_root = tmp.path().join("media").join("Show (2020)");
    write(&series_root.join("poster.jpg"), b"poster");
    let bin = tmp.path().join("recycle");

    let out = recycle(
        &db,
        bin.to_str().unwrap(),
        RecycleKind::SeriesFolder,
        Some(7),
        "Show",
        &series_root,
    )
    .await
    .unwrap();
    assert!(matches!(out, RecycleOutcome::Recycled { .. }));
    assert!(!series_root.exists());

    let entries = list_entries(bin.to_str().unwrap()).await.unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.manifest.kind, RecycleKind::SeriesFolder);
    assert_eq!(e.manifest.files, vec!["Show (2020)".to_string()]);
    let moved = e.dir.join("Show (2020)");
    assert!(moved.join("poster.jpg").is_file());
    assert!(moved.join("Season 01").join("Show - S01E07.mkv").is_file());
    assert!(moved.join("Season 01").join("Show - S01E08.mkv").is_file());
}

#[tokio::test]
async fn empty_bin_path_falls_through_to_permanent_delete() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let (season, video) = seed_series(tmp.path());
    let out = recycle(&db, "", RecycleKind::Episode, None, "Show", &video)
        .await
        .unwrap();
    assert_eq!(out, RecycleOutcome::DirectDeleted);
    assert!(!video.exists());
    // Companions go with the file on the permanent path too.
    assert!(!season.join("Show - S01E07.nfo").exists());
    assert!(!season.join("Show - S01E07.en.srt").exists());
    assert!(season.join("Show - S01E070.mkv").exists());
    assert!(!is_unwritable());
}

#[tokio::test]
async fn missing_path_reports_missing_without_touching_bin() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("recycle");
    let out = recycle(
        &db,
        bin.to_str().unwrap(),
        RecycleKind::Episode,
        None,
        "Show",
        &tmp.path().join("nope.mkv"),
    )
    .await
    .unwrap();
    assert_eq!(out, RecycleOutcome::Missing);
    assert!(!bin.exists());
}

#[test]
fn size_verification_catches_short_copies() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.mkv");
    let b = tmp.path().join("b.mkv");
    write(&a, b"0123456789");
    write(&b, b"0123456789");
    assert!(verify_size(&a, &b).is_ok());
    write(&b, b"01234");
    let err = verify_size(&a, &b).unwrap_err();
    assert!(err.to_string().contains("size mismatch"), "{err}");

    let s = tmp.path().join("src");
    let d = tmp.path().join("dst");
    write(&s.join("Season 01").join("ep.mkv"), b"abcdef");
    write(&d.join("Season 01").join("ep.mkv"), b"abcdef");
    assert!(verify_tree_size(&s, &d).is_ok());
    write(&d.join("Season 01").join("ep.mkv"), b"abc");
    assert!(verify_tree_size(&s, &d).is_err());
}

/// Note: `RECYCLE_UNWRITABLE` is process-global, so the flag assertions
/// here rely on nextest's process-per-test isolation (`cargo t`). Under
/// plain `cargo test` another recycle test can race them; see
/// tests/AGENTS.md on the runner difference.
#[tokio::test]
async fn unwritable_bin_refuses_delete_sets_flag_and_next_success_clears_it() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_season, video) = seed_series(tmp.path());
    // A bin whose parent is a regular file can't be created by anyone,
    // root included, so this stays deterministic in CI.
    let blocker = tmp.path().join("blocker");
    write(&blocker, b"not a dir");
    let bad_bin = blocker.join("recycle");

    let err = recycle(
        &db,
        bad_bin.to_str().unwrap(),
        RecycleKind::Episode,
        None,
        "Show",
        &video,
    )
    .await
    .unwrap_err();
    assert!(err.contains("not writable"), "{err}");
    assert!(
        video.exists(),
        "a refused delete must leave the file in place"
    );
    assert!(is_unwritable(), "unwritable flag must be raised");

    let other = tmp
        .path()
        .join("media/Show (2020)/Season 01/Show - S01E08.mkv");
    let good_bin = tmp.path().join("recycle");
    let out = recycle(
        &db,
        good_bin.to_str().unwrap(),
        RecycleKind::Episode,
        None,
        "Show",
        &other,
    )
    .await
    .unwrap();
    assert!(matches!(out, RecycleOutcome::Recycled { .. }));
    assert!(!is_unwritable(), "successful recycle must clear the flag");
}

#[test]
fn forced_copy_path_leaves_no_tmp_and_moves_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a").join("file.mkv");
    write(&src, b"payload");
    let dst = tmp.path().join("b").join("file.mkv");
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    move_path(&src, &dst, true).unwrap();
    assert!(!src.exists());
    assert_eq!(fs::read(&dst).unwrap(), b"payload");
    let leftovers: Vec<_> = fs::read_dir(dst.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec!["file.mkv".to_string()]);

    // Directory flavor: recursive copy through a tmp dir, then rename.
    let sdir = tmp.path().join("series");
    write(&sdir.join("Season 01").join("ep.mkv"), b"ep");
    write(&sdir.join("poster.jpg"), b"p");
    let ddir = tmp.path().join("bin").join("series");
    fs::create_dir_all(ddir.parent().unwrap()).unwrap();
    move_path(&sdir, &ddir, true).unwrap();
    assert!(!sdir.exists());
    assert_eq!(
        fs::read(ddir.join("Season 01").join("ep.mkv")).unwrap(),
        b"ep"
    );
    assert!(!tmp.path().join("bin").join("series.ryokan-tmp").exists());
}

#[test]
fn forced_copy_failure_leaves_nothing_at_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("file.mkv");
    write(&src, b"payload");
    // Destination parent is a file: the copy to `<dst>.ryokan-tmp` fails
    // and the source must survive untouched.
    let blocker = tmp.path().join("blocker");
    write(&blocker, b"x");
    let dst = blocker.join("file.mkv");
    assert!(move_path(&src, &dst, true).is_err());
    assert_eq!(fs::read(&src).unwrap(), b"payload");
    assert!(!dst.exists());
}

#[test]
fn recycle_blocking_force_copy_matches_rename_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (season, video) = seed_series(tmp.path());
    let bin = tmp.path().join("recycle");
    let out = recycle_blocking(
        bin.to_str().unwrap(),
        RecycleKind::Episode,
        Some(1),
        "Show",
        &video,
        true,
    );
    let Ok(Inner::Recycled { manifest, .. }) = out else {
        panic!("expected Recycled");
    };
    assert_eq!(manifest.files.len(), 4);
    assert!(!video.exists());
    assert!(!season.join("Show - S01E07.nfo").exists());
    // No stray tmp files anywhere under the bin.
    let mut stack = vec![bin.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".ryokan-tmp"), "tmp leftover: {name}");
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            }
        }
    }
}

#[tokio::test]
async fn refuses_to_recycle_a_folder_that_contains_the_bin() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let series_root = tmp.path().join("media").join("Show");
    write(&series_root.join("ep.mkv"), b"x");
    let bin = series_root.join(".recycle");
    fs::create_dir_all(&bin).unwrap();
    let err = recycle(
        &db,
        bin.to_str().unwrap(),
        RecycleKind::SeriesFolder,
        None,
        "Show",
        &series_root,
    )
    .await
    .unwrap_err();
    assert!(err.contains("into itself"), "{err}");
    assert!(series_root.join("ep.mkv").exists());
}

#[tokio::test]
async fn restore_episode_puts_files_back_and_drops_entry() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let media_root = tmp.path().join("media");
    let (season, video) = seed_series(tmp.path());
    let bin = tmp.path().join("recycle");
    let bin_s = bin.to_str().unwrap();
    let RecycleOutcome::Recycled { entry_id } =
        recycle(&db, bin_s, RecycleKind::Episode, Some(1), "Show", &video)
            .await
            .unwrap()
    else {
        panic!("expected Recycled");
    };
    // Simulate the season dir having been pruned after it emptied.
    fs::remove_dir_all(&season).unwrap();
    assert!(!season.exists());

    let out = restore(bin_s, &entry_id, media_root.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        out,
        RestoreOutcome::Restored {
            final_path: video.clone()
        }
    );
    assert_eq!(fs::read(&video).unwrap(), b"video-bytes-0123456789");
    assert!(season.join("Show - S01E07.nfo").is_file());
    assert!(season.join("Show - S01E07.en.srt").is_file());
    assert!(season.join("Show - S01E07-thumb.jpg").is_file());
    assert!(list_entries(bin_s).await.unwrap().is_empty());
    // Date bucket tidied away once empty.
    assert!(fs::read_dir(&bin).unwrap().next().is_none());
}

#[tokio::test]
async fn restore_conflict_and_missing_location_outcomes() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let media_root = tmp.path().join("media");
    let (_season, video) = seed_series(tmp.path());
    let bin = tmp.path().join("recycle");
    let bin_s = bin.to_str().unwrap();
    let RecycleOutcome::Recycled { entry_id } =
        recycle(&db, bin_s, RecycleKind::Episode, Some(1), "Show", &video)
            .await
            .unwrap()
    else {
        panic!("expected Recycled");
    };

    // User re-grabbed: something is back at the original path.
    write(&video, b"new grab");
    assert_eq!(
        restore(bin_s, &entry_id, media_root.to_str().unwrap())
            .await
            .unwrap(),
        RestoreOutcome::ConflictAtTarget
    );
    assert_eq!(fs::read(&video).unwrap(), b"new grab");
    assert_eq!(
        list_entries(bin_s).await.unwrap().len(),
        1,
        "entry must survive a refused restore"
    );

    // Series root gone entirely.
    fs::remove_dir_all(tmp.path().join("media").join("Show (2020)")).unwrap();
    assert_eq!(
        restore(bin_s, &entry_id, media_root.to_str().unwrap())
            .await
            .unwrap(),
        RestoreOutcome::OriginalLocationMissing
    );
    assert_eq!(list_entries(bin_s).await.unwrap().len(), 1);

    assert!(
        restore(bin_s, "deadbeef", media_root.to_str().unwrap())
            .await
            .is_err()
    );
    assert!(
        restore(bin_s, "../x", media_root.to_str().unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn restore_series_folder_round_trips() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let media_root = tmp.path().join("media");
    let (_season, video) = seed_series(tmp.path());
    let series_root = tmp.path().join("media").join("Show (2020)");
    let bin = tmp.path().join("recycle");
    let bin_s = bin.to_str().unwrap();
    let RecycleOutcome::Recycled { entry_id } = recycle(
        &db,
        bin_s,
        RecycleKind::SeriesFolder,
        Some(1),
        "Show",
        &series_root,
    )
    .await
    .unwrap() else {
        panic!("expected Recycled");
    };
    assert!(!series_root.exists());

    let out = restore(bin_s, &entry_id, media_root.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        out,
        RestoreOutcome::Restored {
            final_path: series_root.clone()
        }
    );
    assert_eq!(fs::read(&video).unwrap(), b"video-bytes-0123456789");

    // Folder present again: a second entry for the same folder refuses.
    let RecycleOutcome::Recycled { entry_id } = recycle(
        &db,
        bin_s,
        RecycleKind::SeriesFolder,
        Some(1),
        "Show",
        &series_root,
    )
    .await
    .unwrap() else {
        panic!("expected Recycled");
    };
    fs::create_dir_all(&series_root).unwrap();
    assert_eq!(
        restore(bin_s, &entry_id, media_root.to_str().unwrap())
            .await
            .unwrap(),
        RestoreOutcome::ConflictAtTarget
    );
}

fn seed_entry(bin: &Path, date: &str, id: &str, bytes: u64) {
    let dir = bin.join(date).join(id);
    fs::create_dir_all(&dir).unwrap();
    let m = RecycleManifest {
        kind: RecycleKind::Episode,
        series_id: None,
        series_title: "X".into(),
        original_path: "/nowhere/x.mkv".into(),
        recycled_at: 0,
        size_bytes: bytes,
        files: vec!["x.mkv".into()],
    };
    fs::write(dir.join(MANIFEST_FILE), serde_json::to_vec(&m).unwrap()).unwrap();
    fs::write(dir.join("x.mkv"), vec![0u8; bytes as usize]).unwrap();
}

#[test]
fn purge_respects_age_and_ignores_non_date_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path();
    let today = chrono::NaiveDate::from_ymd_opt(2026, 8, 28).unwrap();
    seed_entry(bin, "2026-01-01", "aaaaaaaa", 10);
    seed_entry(bin, "2026-01-01", "bbbbbbbb", 5);
    seed_entry(bin, "2026-07-29", "cccccccc", 7); // exactly 30 days: kept
    seed_entry(bin, "2026-07-28", "dddddddd", 1); // 31 days: purged
    seed_entry(bin, "2026-08-28", "eeeeeeee", 3);
    fs::create_dir_all(bin.join("not-a-date").join("stuff")).unwrap();
    fs::write(bin.join("stray.txt"), b"x").unwrap();

    let report = purge_blocking(bin, today, Some(30)).unwrap();
    assert_eq!(
        report,
        PurgeReport {
            entries: 3,
            bytes: 16,
            date_dirs: 2
        }
    );
    assert!(!bin.join("2026-01-01").exists());
    assert!(!bin.join("2026-07-28").exists());
    assert!(bin.join("2026-07-29").exists());
    assert!(bin.join("2026-08-28").exists());
    assert!(bin.join("not-a-date").exists());
    assert!(bin.join("stray.txt").exists());

    // age = None empties everything date-shaped.
    let report = purge_blocking(bin, today, None).unwrap();
    assert_eq!(report.entries, 2);
    assert_eq!(report.date_dirs, 2);
    assert!(bin.join("not-a-date").exists());
}

#[tokio::test]
async fn purge_old_with_zero_age_is_a_no_op_and_delete_entry_frees_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path();
    let bin_s = bin.to_str().unwrap();
    seed_entry(bin, "2000-01-01", "aaaaaaaa", 10);
    seed_entry(bin, "2000-01-01", "bbbbbbbb", 4);
    assert_eq!(purge_old(bin_s, 0).await.unwrap(), PurgeReport::default());
    assert!(bin.join("2000-01-01").exists());

    assert_eq!(summary(bin_s).await.unwrap(), (2, 14));
    assert_eq!(delete_entry(bin_s, "aaaaaaaa").await.unwrap(), 10);
    assert!(!bin.join("2000-01-01").join("aaaaaaaa").exists());
    assert!(bin.join("2000-01-01").join("bbbbbbbb").exists());
    assert_eq!(delete_entry(bin_s, "bbbbbbbb").await.unwrap(), 4);
    assert!(!bin.join("2000-01-01").exists(), "empty date bucket tidied");
    assert!(delete_entry(bin_s, "aaaaaaaa").await.is_err());

    seed_entry(bin, "2000-01-01", "cccccccc", 1);
    let report = empty(bin_s).await.unwrap();
    assert_eq!(report.entries, 1);
    assert_eq!(summary(bin_s).await.unwrap(), (0, 0));
}

#[tokio::test]
async fn list_entries_orders_newest_first_and_skips_broken_manifests() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path();
    let bin_s = bin.to_str().unwrap();
    seed_entry(bin, "2026-01-01", "aaaaaaaa", 1);
    seed_entry(bin, "2026-02-01", "bbbbbbbb", 1);
    // Bump recycled_at on the second so ordering is by timestamp.
    let dir = bin.join("2026-02-01").join("bbbbbbbb");
    let mut m: RecycleManifest =
        serde_json::from_slice(&fs::read(dir.join(MANIFEST_FILE)).unwrap()).unwrap();
    m.recycled_at = 100;
    fs::write(dir.join(MANIFEST_FILE), serde_json::to_vec(&m).unwrap()).unwrap();
    let broken = bin.join("2026-03-01").join("cccccccc");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join(MANIFEST_FILE), b"{not json").unwrap();

    let entries = list_entries(bin_s).await.unwrap();
    let ids: Vec<&str> = entries.iter().map(|e| e.entry_id.as_str()).collect();
    assert_eq!(ids, vec!["bbbbbbbb", "aaaaaaaa"]);
    assert_eq!(entries[0].date, "2026-02-01");
    assert!(find_entry(bin_s, "aaaaaaaa").await.unwrap().is_some());
    assert!(find_entry(bin_s, "cccccccc").await.unwrap().is_none());
    assert!(list_entries("").await.unwrap().is_empty());
    assert!(
        list_entries(bin.join("absent").to_str().unwrap())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn probe_writable_reports_both_ways() {
    let tmp = tempfile::tempdir().unwrap();
    let good = tmp.path().join("bin");
    assert!(probe_writable(good.to_str().unwrap()).await.is_ok());
    assert!(good.is_dir());
    assert!(!good.join(".ryokan-write-probe").exists());
    let blocker = tmp.path().join("blocker");
    fs::write(&blocker, b"x").unwrap();
    assert!(
        probe_writable(blocker.join("bin").to_str().unwrap())
            .await
            .is_err()
    );
    assert!(probe_writable("").await.is_err());
}

#[tokio::test]
async fn restore_refuses_paths_outside_the_media_root() {
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_season, video) = seed_series(tmp.path());
    let bin = tmp.path().join("recycle");
    let bin_s = bin.to_str().unwrap();
    let RecycleOutcome::Recycled { entry_id } =
        recycle(&db, bin_s, RecycleKind::Episode, Some(1), "Show", &video)
            .await
            .unwrap()
    else {
        panic!("expected Recycled");
    };
    // A different media root: the manifest path is not under it.
    let other_root = tmp.path().join("elsewhere");
    assert_eq!(
        restore(bin_s, &entry_id, other_root.to_str().unwrap())
            .await
            .unwrap(),
        RestoreOutcome::OutsideMediaRoot
    );
    // An empty media root can't vouch for anything.
    assert_eq!(
        restore(bin_s, &entry_id, "").await.unwrap(),
        RestoreOutcome::OutsideMediaRoot
    );
    assert_eq!(list_entries(bin_s).await.unwrap().len(), 1);
    // The real root restores.
    let media_root = tmp.path().join("media");
    assert!(matches!(
        restore(bin_s, &entry_id, media_root.to_str().unwrap())
            .await
            .unwrap(),
        RestoreOutcome::Restored { .. }
    ));
    assert!(video.is_file());
}

#[tokio::test]
async fn restore_refuses_a_manifest_naming_files_outside_the_entry() {
    // The manifest is a file in the bin; only `original_path` used to be
    // checked, so a planted `files` entry moved anything into the library.
    let db = test_support::in_memory_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_season, video) = seed_series(tmp.path());
    let bin = tmp.path().join("recycle");
    let bin_s = bin.to_str().unwrap();
    let RecycleOutcome::Recycled { entry_id } =
        recycle(&db, bin_s, RecycleKind::Episode, Some(1), "Show", &video)
            .await
            .unwrap()
    else {
        panic!("expected Recycled");
    };
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, b"not the bin's").unwrap();
    let entry = find_entry(bin_s, &entry_id).await.unwrap().unwrap();
    let manifest_path = entry.dir.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    for bad in ["../../../outside.txt", "/etc/hostname"] {
        manifest["files"] = serde_json::json!([bad]);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let media_root = tmp.path().join("media");
        let err = restore(bin_s, &entry_id, media_root.to_str().unwrap())
            .await
            .unwrap_err();
        assert!(err.contains("not a file name"), "{bad}: {err}");
    }
    assert!(outside.is_file(), "nothing was moved");
}
