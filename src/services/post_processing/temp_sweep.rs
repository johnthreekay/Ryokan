//! Orphaned temporary-file sweep (issue #205).
//!
//! `do_file_op`'s cross-filesystem move writes `<dest>.ryokan-tmp`, and
//! an upgrade lands at `.<basename>.ryokan-new` (`staging_path`), before
//! the final rename onto the destination. Both are removed in-process
//! when the copy fails, but a crash, an OOM kill, or a container stop
//! mid-copy leaves the file in the season folder for good, where a media
//! server indexes it as junk. The hourly `cleanup` task calls
//! [`sweep_orphaned_temp_files`] to clear them.
//!
//! Two phases, because timestamps alone cannot tell a leftover from a
//! file being staged right now. `.ryokan-tmp` is always written by
//! `fs::copy`, so its mtime is fresh while the copy runs; but a
//! `.ryokan-new` made by `fs::hard_link` (hardlink mode) or a same-fs
//! `fs::rename` (move mode) shares the source's inode and reads as old as
//! the download it came from, the second it is staged. So the walk
//! (`spawn_blocking`, no symlink following) only collects files older
//! than [`ORPHAN_MIN_AGE`] by mtime, and the removals run under
//! `POST_PROC_LOCK` and `IMPORT_LOCK`, the two locks every writer of these
//! names holds: with both taken nothing can be mid-copy, so whatever is
//! still there is a crash leftover. When either lock is busy the hour is
//! skipped rather than waited for.
//!
//! What "remove" means depends on the suffix. A `.ryokan-tmp` is a
//! partial copy, or at worst a duplicate of a source that is still in
//! place (the move path unlinks the source only after the rename), so it
//! is a direct unlink. A `.ryokan-new` is a complete file and in move mode
//! can be the only copy of the episode, so it goes through
//! `recycle::recycle` like any other library delete.
//!
//! Not covered: the directory-form `<dst>.ryokan-tmp` that a crashed
//! cross-filesystem recycle *restore* can leave in the media root (its
//! files carry normal names), and anything inside the recycle bin, which
//! is excluded when it sits under the media root because a recycled
//! leftover belongs to a manifest.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sqlx::SqlitePool;
use walkdir::WalkDir;

use super::POST_PROC_LOCK;
use crate::services::manual_import::import::IMPORT_LOCK;
use crate::services::recycle::{self, RecycleKind, RecycleOutcome};

/// File-name suffixes the import paths use for in-flight copies.
pub const TEMP_FILE_SUFFIXES: &[&str] = &[".ryokan-tmp", ".ryokan-new"];

/// A temp file younger than this (by mtime) is left alone even under the
/// locks: a fresh leftover belongs to an import that is about to retry.
pub const ORPHAN_MIN_AGE: Duration = Duration::from_secs(2 * 60 * 60);

/// Nothing the import paths write sits deeper than
/// `<root>/<series>/<season>/<file>`; one level of slack.
const MAX_WALK_DEPTH: usize = 4;

/// Cap on the walk / remove errors kept in the report, so an unreadable
/// subtree with thousands of entries does not bloat a log line.
const MAX_REPORTED_ERRORS: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TempKind {
    /// `<dest>.ryokan-tmp`: the cross-filesystem move's partial copy.
    PartialCopy,
    /// `.<basename>.ryokan-new`: the upgrade's place-then-swap staging
    /// file (issue #202).
    UpgradeStaging,
}

/// Which temp shape `name` is, if any.
pub fn temp_kind(name: &str) -> Option<TempKind> {
    if name.ends_with(".ryokan-tmp") {
        Some(TempKind::PartialCopy)
    } else if name.ends_with(".ryokan-new") {
        Some(TempKind::UpgradeStaging)
    } else {
        None
    }
}

/// Does `name` carry one of the import paths' temp suffixes?
pub fn is_temp_file_name(name: &str) -> bool {
    temp_kind(name).is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TempCandidate {
    pub path: PathBuf,
    pub kind: TempKind,
    /// The top-level folder under the media root the file sits in (the
    /// series folder for anything the import paths write); empty for a
    /// file directly in the root. Names the recycle-bin entry for a
    /// `.ryokan-new`, which has no series row to point at.
    pub series_folder: String,
}

#[derive(Debug, Default)]
pub struct TempSweepReport {
    /// Files removed (unlinked or recycled), in walk order.
    pub removed: Vec<PathBuf>,
    /// How many of `removed` went to the recycle bin rather than being
    /// unlinked.
    pub recycled: usize,
    /// Bytes freed. A `.ryokan-new` moved to the recycle bin still
    /// occupies the disk there and counts nothing; neither does a file
    /// whose inode has other links (a hardlinked `.ryokan-new`).
    pub bytes: u64,
    /// Temp files left alone because they are younger than the floor.
    pub kept_recent: usize,
    /// An import held one of the locks; nothing was removed this pass.
    pub skipped_busy: bool,
    /// Walk and unlink errors (capped at [`MAX_REPORTED_ERRORS`]).
    pub errors: Vec<String>,
}

impl TempSweepReport {
    fn note_error(&mut self, msg: String) {
        if self.errors.len() < MAX_REPORTED_ERRORS {
            self.errors.push(msg);
        }
    }
}

/// Everything the walk found, before any lock is taken.
#[derive(Debug, Default)]
pub(crate) struct TempScan {
    pub candidates: Vec<TempCandidate>,
    pub kept_recent: usize,
    pub errors: Vec<String>,
}

/// Sweep `media_root` for import leftovers older than `min_age`. The
/// recycle bin at `recycle_bin_path` (empty = none) is excluded from the
/// walk and receives every `.ryokan-new`. An empty or missing root is a
/// no-op; a root that exists but is not a directory is an error.
pub async fn sweep_orphaned_temp_files(
    db: &SqlitePool,
    media_root: &str,
    recycle_bin_path: &str,
    min_age: Duration,
) -> Result<TempSweepReport, String> {
    let root = media_root.trim().to_string();
    let mut report = TempSweepReport::default();
    if root.is_empty() {
        return Ok(report);
    }
    let bin = recycle_bin_path.trim().to_string();
    let exclude: Vec<PathBuf> = if bin.is_empty() {
        Vec::new()
    } else {
        vec![PathBuf::from(&bin)]
    };

    let scan = tokio::task::spawn_blocking(move || {
        find_candidates(Path::new(&root), &exclude, min_age, SystemTime::now())
    })
    .await
    .map_err(|e| format!("temp sweep walk panicked: {e}"))??;
    report.kept_recent = scan.kept_recent;
    for e in scan.errors {
        report.note_error(e);
    }
    if scan.candidates.is_empty() {
        return Ok(report);
    }

    // Both writers of these names take one of these locks for the whole
    // placement; holding both proves nothing is mid-copy.
    let Ok(_post_proc) = POST_PROC_LOCK.try_lock() else {
        report.skipped_busy = true;
        return Ok(report);
    };
    let Ok(_manual_import) = IMPORT_LOCK.try_lock() else {
        report.skipped_busy = true;
        return Ok(report);
    };

    // Re-check under the locks: an import may have finished (renaming its
    // staging file away) between the walk and now, and the candidate
    // path could since have been reused for something newer.
    let candidates = scan.candidates;
    let (partials, stagings, mut recheck_errors): (Vec<_>, Vec<_>, Vec<String>) =
        tokio::task::spawn_blocking(move || {
            let now = SystemTime::now();
            let mut partials = Vec::new();
            let mut stagings = Vec::new();
            let mut errors = Vec::new();
            for c in candidates {
                match still_eligible(&c.path, min_age, now) {
                    Ok(true) => match c.kind {
                        TempKind::PartialCopy => partials.push(c.path),
                        TempKind::UpgradeStaging => stagings.push((c.path, c.series_folder)),
                    },
                    Ok(false) => {}
                    Err(e) => errors.push(e),
                }
            }
            // Partial copies are never the only copy of anything: unlink
            // them here, in the same blocking hop as the re-stat.
            let mut removed = Vec::new();
            for path in partials {
                let bytes = reclaimable_bytes(&path);
                match std::fs::remove_file(&path) {
                    Ok(()) => removed.push((path, bytes)),
                    Err(e) => errors.push(format!("remove {}: {e}", path.display())),
                }
            }
            (removed, stagings, errors)
        })
        .await
        .map_err(|e| format!("temp sweep removal panicked: {e}"))?;
    for (path, bytes) in partials {
        report.bytes += bytes;
        report.removed.push(path);
    }
    for (path, series_folder) in stagings {
        let bytes = reclaimable_bytes(&path);
        match recycle::recycle(db, &bin, RecycleKind::Episode, None, &series_folder, &path).await {
            // The bytes moved with the file: nothing was freed.
            Ok(RecycleOutcome::Recycled { .. }) => {
                report.recycled += 1;
                report.removed.push(path);
            }
            Ok(RecycleOutcome::DirectDeleted) => {
                report.bytes += bytes;
                report.removed.push(path);
            }
            Ok(RecycleOutcome::Missing) => {}
            Err(e) => recheck_errors.push(format!("recycle {}: {e}", path.display())),
        }
    }
    for e in recheck_errors {
        report.note_error(e);
    }
    Ok(report)
}

/// The walk behind [`sweep_orphaned_temp_files`]: every temp-named
/// regular file under `root` (not entering `exclude`, never following
/// symlinks) whose mtime is at least `min_age` before `now`. `now` is a
/// parameter so tests can pin the clock. Removes nothing.
pub(crate) fn find_candidates(
    root: &Path,
    exclude: &[PathBuf],
    min_age: Duration,
    now: SystemTime,
) -> Result<TempScan, String> {
    let mut scan = TempScan::default();
    // `metadata` follows a symlinked root on purpose: `/media/anime`
    // pointing at a mount is a normal layout.
    match std::fs::metadata(root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(format!("{} is not a directory", root.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(scan),
        Err(e) => return Err(format!("read {}: {e}", root.display())),
    }
    // Canonical forms so a bin reached through a symlink or given with a
    // different spelling still matches the walked paths; a path that
    // does not resolve (bin not created yet) stays as given.
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let exclude: Vec<PathBuf> = exclude
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    let mut errors = Vec::new();
    let walker = WalkDir::new(&root)
        .follow_links(false)
        .max_depth(MAX_WALK_DEPTH)
        .into_iter()
        .filter_entry(|e| !exclude.iter().any(|ex| e.path().starts_with(ex)));
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        // Symlinks are skipped whatever they point at: a link named like
        // a temp file is not a partial copy, and removing what it points
        // at could reach outside the library.
        if entry.path_is_symlink() || !entry.file_type().is_file() {
            continue;
        }
        let Some(kind) = entry.file_name().to_str().and_then(temp_kind) else {
            continue;
        };
        let modified = match entry
            .metadata()
            .map_err(|e| e.to_string())
            .and_then(|m| m.modified().map_err(|e| e.to_string()))
        {
            Ok(m) => m,
            Err(e) => {
                errors.push(format!("stat {}: {e}", entry.path().display()));
                continue;
            }
        };
        // A clock that went backwards reads as age zero: keep the file.
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age < min_age {
            scan.kept_recent += 1;
            continue;
        }
        let series_folder = entry
            .path()
            .strip_prefix(&root)
            .ok()
            .and_then(Path::parent)
            .and_then(|rel| rel.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        scan.candidates.push(TempCandidate {
            path: entry.path().to_path_buf(),
            kind,
            series_folder,
        });
    }
    errors.truncate(MAX_REPORTED_ERRORS);
    scan.errors = errors;
    Ok(scan)
}

/// Is `path` still a regular file (not a symlink) older than `min_age`?
/// `Ok(false)` when it is gone or no longer qualifies.
fn still_eligible(path: &Path, min_age: Duration, now: SystemTime) -> Result<bool, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    };
    if !meta.is_file() {
        return Ok(false);
    }
    let modified = meta
        .modified()
        .map_err(|e| format!("mtime {}: {e}", path.display()))?;
    let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
    Ok(age >= min_age)
}

/// Bytes unlinking `path` would free: its size when it is the inode's
/// only link, otherwise zero (a hardlinked `.ryokan-new` shares the
/// download's blocks).
fn reclaimable_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return 0;
        }
    }
    meta.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::post_processing::tests::POST_PROC_TEST_SERIALIZER;
    use crate::test_support::in_memory_pool;
    use std::fs;

    const OLD: Duration = Duration::from_secs(3 * 3600);

    fn write_with_age(path: &Path, age: Duration, now: SystemTime) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"partial").unwrap();
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(now - age).unwrap();
    }

    #[test]
    fn temp_suffixes_map_to_their_kinds() {
        assert_eq!(
            temp_kind("Show - S01E01.mkv.ryokan-tmp"),
            Some(TempKind::PartialCopy)
        );
        assert_eq!(
            temp_kind(".Show - S01E01.mkv.ryokan-new"),
            Some(TempKind::UpgradeStaging)
        );
        assert_eq!(temp_kind("Show - S01E01.mkv"), None);
        assert!(!is_temp_file_name("ryokan-tmp"));
        assert!(is_temp_file_name("x.ryokan-new"));
    }

    #[test]
    fn walk_finds_old_temp_files_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let now = SystemTime::now();
        let old_tmp = root.join("Show/Season 01/Show - S01E01.mkv.ryokan-tmp");
        let fresh_tmp = root.join("Show/Season 01/Show - S01E03.mkv.ryokan-tmp");
        let video = root.join("Show/Season 01/Show - S01E04.mkv");
        write_with_age(&old_tmp, OLD, now);
        write_with_age(&fresh_tmp, Duration::from_secs(10 * 60), now);
        write_with_age(&video, Duration::from_secs(30 * 24 * 3600), now);
        // A staging file hardlinked from an old download reads old the
        // moment it is made; the walk must list it (the locks, not the
        // clock, decide whether it is a leftover).
        let old_new = root.join("Show/Season 01/.Show - S01E02.mkv.ryokan-new");
        fs::hard_link(&video, &old_new).unwrap();

        let scan = find_candidates(root, &[], ORPHAN_MIN_AGE, now).unwrap();

        let found: Vec<&Path> = scan.candidates.iter().map(|c| c.path.as_path()).collect();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.contains(&old_tmp.as_path()));
        assert!(found.contains(&old_new.as_path()));
        assert!(
            scan.candidates.iter().all(|c| c.series_folder == "Show"),
            "the series folder names the bin entry: {:?}",
            scan.candidates
        );
        assert_eq!(scan.kept_recent, 1, "the fresh tmp is a copy in progress");
        assert!(scan.errors.is_empty(), "{:?}", scan.errors);
        assert!(old_tmp.exists(), "the walk removes nothing");
        assert!(video.exists());
    }

    #[test]
    fn walk_skips_excluded_subtrees_symlinks_and_deep_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let now = SystemTime::now();
        let recycled = root.join(".recycle/2026-09-01/abcd1234/.Show - S01E01.mkv.ryokan-new");
        write_with_age(&recycled, OLD, now);
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("elsewhere.mkv.ryokan-tmp");
        write_with_age(&target, OLD, now);
        let link = root.join("Show/link.mkv.ryokan-tmp");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let deep = root.join("a/b/c/d/e/deep.mkv.ryokan-tmp");
        write_with_age(&deep, OLD, now);

        let scan = find_candidates(root, &[root.join(".recycle")], ORPHAN_MIN_AGE, now).unwrap();

        assert!(scan.candidates.is_empty(), "{:?}", scan.candidates);
        assert!(recycled.exists() && target.exists() && link.exists() && deep.exists());
    }

    #[test]
    fn missing_root_is_a_no_op_and_a_file_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let scan = find_candidates(&missing, &[], ORPHAN_MIN_AGE, SystemTime::now()).unwrap();
        assert!(scan.candidates.is_empty());
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(find_candidates(&file, &[], ORPHAN_MIN_AGE, SystemTime::now()).is_err());
    }

    #[tokio::test]
    async fn sweep_unlinks_partials_and_recycles_staging_files() {
        let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bin = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old_tmp = root.join("Show/Season 01/Show - S01E01.mkv.ryokan-tmp");
        let old_new = root.join("Show/Season 01/.Show - S01E02.mkv.ryokan-new");
        let fresh_new = root.join("Show/Season 01/.Show - S01E03.mkv.ryokan-new");
        write_with_age(&old_tmp, OLD, now);
        write_with_age(&old_new, OLD, now);
        write_with_age(&fresh_new, Duration::from_secs(60), now);

        let report = sweep_orphaned_temp_files(
            &db,
            root.to_str().unwrap(),
            bin.path().to_str().unwrap(),
            ORPHAN_MIN_AGE,
        )
        .await
        .unwrap();

        assert!(!report.skipped_busy);
        assert_eq!(report.removed.len(), 2, "{:?}", report.removed);
        assert_eq!(report.recycled, 1, "the staging file goes through the bin");
        assert_eq!(
            report.bytes, 7,
            "only the unlinked partial is freed; the recycled file still sits in the bin"
        );
        assert_eq!(report.kept_recent, 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!old_tmp.exists() && !old_new.exists());
        assert!(fresh_new.exists());
        let bin_entries = fs::read_dir(bin.path()).unwrap().count();
        assert_eq!(bin_entries, 1, "one date bucket in the bin");
        let entries = recycle::list_entries(bin.path().to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].manifest.series_title, "Show",
            "the recycle page names the entry by its series folder"
        );
    }

    #[tokio::test]
    async fn sweep_without_a_bin_deletes_staging_files_directly() {
        let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let old_new = dir
            .path()
            .join("Show/Season 01/.Show - S01E02.mkv.ryokan-new");
        write_with_age(&old_new, OLD, SystemTime::now());

        let report =
            sweep_orphaned_temp_files(&db, dir.path().to_str().unwrap(), "", ORPHAN_MIN_AGE)
                .await
                .unwrap();

        assert_eq!(report.removed, vec![old_new.clone()]);
        assert_eq!(report.recycled, 0);
        assert_eq!(report.bytes, 7, "a direct delete frees the bytes");
        assert!(!old_new.exists());
    }

    #[tokio::test]
    async fn sweep_skips_the_pass_while_an_import_holds_a_lock() {
        let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let old_tmp = dir
            .path()
            .join("Show/Season 01/Show - S01E01.mkv.ryokan-tmp");
        write_with_age(&old_tmp, OLD, SystemTime::now());

        for lock in [&*POST_PROC_LOCK, &*IMPORT_LOCK] {
            let _held = lock.lock().await;
            let report =
                sweep_orphaned_temp_files(&db, dir.path().to_str().unwrap(), "", ORPHAN_MIN_AGE)
                    .await
                    .unwrap();
            assert!(report.skipped_busy);
            assert!(report.removed.is_empty());
            assert!(
                old_tmp.exists(),
                "nothing is removed while an import may be mid-copy"
            );
        }
    }

    #[tokio::test]
    async fn sweep_treats_an_empty_root_as_disabled() {
        let db = in_memory_pool().await;
        let report = sweep_orphaned_temp_files(&db, "   ", "", ORPHAN_MIN_AGE)
            .await
            .unwrap();
        assert!(report.removed.is_empty() && !report.skipped_busy);
    }
}
