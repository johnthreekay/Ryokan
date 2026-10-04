//! Recycle bin (#123).
//!
//! Every library delete funnels through [`recycle`]: instead of unlinking,
//! the file (plus its companions) or the whole series folder moves into
//! `<recycle_bin_path>/<YYYY-MM-DD>/<entry_id>/` next to a `manifest.json`
//! that records where it came from. [`restore`] puts an entry back,
//! [`purge_old`] runs from the hourly `cleanup` task and drops date buckets
//! older than `recycle_bin_age_days`, and [`delete_entry`] / [`empty`]
//! back the "Delete now" / "Empty recycle bin" buttons.
//!
//! Disabled: an empty `recycle_bin_path` means recycle is off and deletes
//! are permanent, with one `Library` info line per delete so "why is my
//! file gone" is answerable from System > Logs. A configured-but-unwritable
//! path **refuses the delete** (the file stays, the caller gets an error),
//! logs a `System` warn, and raises [`RECYCLE_UNWRITABLE`] so the recycle
//! page and the System page show a banner until the next successful
//! recycle clears it. That matches Sonarr's fail-closed behavior: a user
//! who configured a bin asked for a safety net, and a full disk or a bad
//! mount is exactly when they need it.
//!
//! Same-filesystem recycle is a rename (atomic, instant, hardlinks to the
//! seeding copy preserved). Cross-filesystem silently degrades to
//! copy-then-unlink through a `.ryokan-tmp` sibling so an interrupted copy
//! never leaves a partial file at the final name.

pub mod helpers;
pub mod manifest;
#[cfg(test)]
mod tests;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, Utc};
use sqlx::SqlitePool;

use crate::models::log::LogCategory;
use crate::services::logger;

pub use helpers::is_valid_entry_id;
pub use manifest::{MANIFEST_FILE, RecycleKind, RecycleManifest};

/// Set when a recycle attempt found `recycle_bin_path` configured but
/// unwritable and refused the delete; cleared by the next successful
/// recycle or a passing [`check_unwritable`] probe. Read by the recycle
/// and System page banners ("deletes are refused until this is fixed").
pub static RECYCLE_UNWRITABLE: AtomicBool = AtomicBool::new(false);

pub fn is_unwritable() -> bool {
    RECYCLE_UNWRITABLE.load(Ordering::Relaxed)
}

/// `(when, bin path, entry count)` behind [`cached_entry_count`]. The
/// Library page shows the bin control only when the bin is non-empty, and
/// walking the bin on every Library render would be a directory scan per
/// page load; one scan a minute is plenty, and every mutating call here
/// drops the cache so the badge never lags a delete or restore.
static ENTRY_COUNT_CACHE: Mutex<Option<(Instant, String, u64)>> = Mutex::new(None);
const ENTRY_COUNT_TTL: Duration = Duration::from_secs(60);

pub async fn cached_entry_count(bin_path: &str) -> u64 {
    let bin = bin_path.trim().to_string();
    if bin.is_empty() {
        return 0;
    }
    if let Some((at, cached_bin, n)) = ENTRY_COUNT_CACHE.lock().unwrap().as_ref()
        && cached_bin == &bin
        && at.elapsed() < ENTRY_COUNT_TTL
    {
        return *n;
    }
    let n = list_entries(&bin)
        .await
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    *ENTRY_COUNT_CACHE.lock().unwrap() = Some((Instant::now(), bin, n));
    n
}

fn invalidate_entry_count() {
    *ENTRY_COUNT_CACHE.lock().unwrap() = None;
}

/// `(when, bin path, unwritable)` behind [`check_unwritable`].
static PROBE_CACHE: Mutex<Option<(Instant, String, bool)>> = Mutex::new(None);
const PROBE_TTL: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Live health probe for the recycle page banner and the settings save
/// notice. Returns `true` when a bin is configured and cannot be written
/// right now, and keeps [`RECYCLE_UNWRITABLE`] in step with what it finds
/// so a fixed mount clears the warning without waiting for the next
/// successful recycle. Cached for 30s so page loads don't keep waking a
/// spun-down disk, and bounded by a 3s timeout so a hung network mount
/// reads as "unwritable" instead of hanging the page. An empty path is
/// "not configured," never "unwritable."
pub async fn check_unwritable(bin_path: &str) -> bool {
    let bin = bin_path.trim().to_string();
    if bin.is_empty() {
        return false;
    }
    if let Some((at, cached_bin, v)) = PROBE_CACHE.lock().unwrap().as_ref()
        && cached_bin == &bin
        && at.elapsed() < PROBE_TTL
    {
        return *v;
    }
    let unwritable = !matches!(
        tokio::time::timeout(PROBE_TIMEOUT, probe_writable(&bin)).await,
        Ok(Ok(()))
    );
    *PROBE_CACHE.lock().unwrap() = Some((Instant::now(), bin, unwritable));
    RECYCLE_UNWRITABLE.store(unwritable, Ordering::Relaxed);
    unwritable
}

/// Drop the probe cache (settings save changed the path).
pub fn invalidate_probe_cache() {
    *PROBE_CACHE.lock().unwrap() = None;
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecycleOutcome {
    /// Moved into the bin; `entry_id` names the new entry directory.
    Recycled { entry_id: String },
    /// Recycle disabled (empty path); the path was permanently deleted.
    DirectDeleted,
    /// Nothing at `path`; nothing to do.
    Missing,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    Restored {
        final_path: PathBuf,
    },
    /// Something already exists at the original location; nothing moved.
    ConflictAtTarget,
    /// The original location's parent (series folder for an episode,
    /// media root for a series folder) no longer exists; nothing moved.
    OriginalLocationMissing,
    /// The manifest's `original_path` doesn't sit under the current
    /// media root (bin moved between installs, or a tampered manifest);
    /// nothing moved. Restore never places files outside the library.
    OutsideMediaRoot,
}

/// One entry as listed from disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecycleEntry {
    pub entry_id: String,
    /// The `YYYY-MM-DD` bucket the entry lives under.
    pub date: String,
    pub dir: PathBuf,
    pub manifest: RecycleManifest,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgeReport {
    pub entries: u64,
    pub bytes: u64,
    pub date_dirs: u64,
}

/// Move `path` into the recycle bin (or permanently delete it when the bin
/// is disabled / unwritable). `series_id` / `series_title` only feed the
/// manifest for the list page; restore never depends on them.
pub async fn recycle(
    db: &SqlitePool,
    bin_path: &str,
    kind: RecycleKind,
    series_id: Option<i64>,
    series_title: &str,
    path: &Path,
) -> Result<RecycleOutcome, String> {
    let bin = bin_path.trim().to_string();
    let path_owned = path.to_path_buf();
    let title = series_title.to_string();

    let bin_for_task = bin.clone();
    let path_for_task = path_owned.clone();
    let inner = tokio::task::spawn_blocking(move || {
        recycle_blocking(
            &bin_for_task,
            kind,
            series_id,
            &title,
            &path_for_task,
            false,
        )
    })
    .await
    .map_err(|e| format!("recycle task panicked: {e}"))?;

    match inner {
        Ok(Inner::Recycled { entry_id, manifest }) => {
            RECYCLE_UNWRITABLE.store(false, Ordering::Relaxed);
            invalidate_entry_count();
            logger::info(
                db,
                LogCategory::Library,
                &format!(
                    "Recycled {} for '{}'",
                    kind.as_str().replace('_', " "),
                    manifest.series_title
                ),
                &format!(
                    "entry={} files={} bytes={} from={}",
                    entry_id,
                    manifest.files.len(),
                    manifest.size_bytes,
                    manifest.original_path
                ),
            )
            .await;
            Ok(RecycleOutcome::Recycled { entry_id })
        }
        Ok(Inner::DirectDeleted) => {
            logger::info(
                db,
                LogCategory::Library,
                "Recycle bin not configured. Deleted permanently.",
                &format!("path={}", path_owned.display()),
            )
            .await;
            Ok(RecycleOutcome::DirectDeleted)
        }
        Ok(Inner::Missing) => Ok(RecycleOutcome::Missing),
        Err(InnerErr::Unwritable(reason)) => {
            RECYCLE_UNWRITABLE.store(true, Ordering::Relaxed);
            logger::warn(
                db,
                LogCategory::System,
                "Recycle bin is not writable. Delete refused.",
                &format!(
                    "recycle_bin_path={} path={} error={}",
                    bin,
                    path_owned.display(),
                    reason
                ),
            )
            .await;
            Err(format!(
                "Recycle bin '{}' is not writable. Fix the path or clear it in Settings to delete permanently. ({})",
                bin, reason
            ))
        }
        Err(InnerErr::Io(msg)) => Err(msg),
    }
}

enum Inner {
    Recycled {
        entry_id: String,
        manifest: RecycleManifest,
    },
    DirectDeleted,
    Missing,
}

enum InnerErr {
    /// The bin itself couldn't be prepared; the delete is refused and the
    /// original stays in place.
    Unwritable(String),
    /// Moving the payload failed; the original is left in place (moved
    /// companions are rolled back best-effort) and the caller surfaces
    /// the error instead of deleting.
    Io(String),
}

/// Synchronous body of [`recycle`]. `force_copy` is a test seam that
/// exercises the cross-filesystem path without a second filesystem.
fn recycle_blocking(
    bin: &str,
    kind: RecycleKind,
    series_id: Option<i64>,
    series_title: &str,
    path: &Path,
    force_copy: bool,
) -> Result<Inner, InnerErr> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Inner::Missing),
        Err(e) => {
            return Err(InnerErr::Io(format!("stat {} failed: {e}", path.display())));
        }
    };
    match kind {
        RecycleKind::Episode if !meta.is_file() => {
            return Err(InnerErr::Io(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        RecycleKind::SeriesFolder if !meta.is_dir() => {
            return Err(InnerErr::Io(format!(
                "{} is not a directory",
                path.display()
            )));
        }
        _ => {}
    }

    if bin.is_empty() {
        permanent_delete(kind, path).map_err(|e| {
            InnerErr::Io(format!(
                "permanent delete of {} failed: {e}",
                path.display()
            ))
        })?;
        return Ok(Inner::DirectDeleted);
    }
    let bin = Path::new(bin);

    // Create the bin root first so the containment check below can
    // canonicalize it on the very first recycle; before this, a fresh
    // bin skipped the check and the rename failed with a bare EINVAL
    // after a `<bin>/<date>` skeleton had already been created inside
    // the series folder.
    let bin_preexisted = bin.is_dir();
    fs::create_dir_all(bin)
        .map_err(|e| InnerErr::Unwritable(format!("create {}: {e}", bin.display())))?;

    // A bin nested inside the folder being recycled would be moved into
    // itself; a path already inside the bin is a UI bug. Refuse both
    // rather than guess.
    if let (Ok(bin_canon), Ok(path_canon)) = (fs::canonicalize(bin), fs::canonicalize(path)) {
        if bin_canon.starts_with(&path_canon) {
            if !bin_preexisted {
                let _ = fs::remove_dir(bin);
            }
            return Err(InnerErr::Io(format!(
                "recycle bin {} lives inside {}. Refusing to recycle a folder into itself",
                bin_canon.display(),
                path_canon.display()
            )));
        }
        if path_canon.starts_with(&bin_canon) {
            return Err(InnerErr::Io(format!(
                "{} is already inside the recycle bin",
                path_canon.display()
            )));
        }
    }

    let date = Utc::now().format("%Y-%m-%d").to_string();
    let date_dir = bin.join(&date);
    fs::create_dir_all(&date_dir)
        .map_err(|e| InnerErr::Unwritable(format!("create {}: {e}", date_dir.display())))?;

    let (entry_id, entry_dir) = {
        let mut picked = None;
        for _ in 0..8 {
            let id = helpers::new_entry_id();
            let dir = date_dir.join(&id);
            match fs::create_dir(&dir) {
                Ok(()) => {
                    picked = Some((id, dir));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(InnerErr::Unwritable(format!(
                        "create {}: {e}",
                        dir.display()
                    )));
                }
            }
        }
        picked.ok_or_else(|| {
            InnerErr::Unwritable("could not allocate a unique entry id".to_string())
        })?
    };

    // Companions first, main payload last, so a mid-way failure is more
    // likely to have left the important file untouched.
    let mut sources: Vec<PathBuf> = match kind {
        RecycleKind::Episode => helpers::companions(path),
        RecycleKind::SeriesFolder => Vec::new(),
    };
    sources.push(path.to_path_buf());

    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for src in &sources {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = entry_dir.join(name);
        if let Err(e) = helpers::move_path(src, &dst, force_copy) {
            rollback(&moved, &entry_dir);
            helpers::remove_dir_if_empty(&date_dir);
            return Err(InnerErr::Io(format!(
                "recycle move {} -> {} failed: {e}",
                src.display(),
                dst.display()
            )));
        }
        moved.push((src.clone(), dst));
    }

    let size_bytes = moved.iter().map(|(_, d)| helpers::path_size(d)).sum();
    let manifest = RecycleManifest {
        kind,
        series_id,
        series_title: series_title.to_string(),
        original_path: path.display().to_string(),
        recycled_at: Utc::now().timestamp(),
        size_bytes,
        files: moved
            .iter()
            .filter_map(|(_, d)| d.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect(),
    };
    if let Err(e) = write_manifest(&entry_dir, &manifest) {
        rollback(&moved, &entry_dir);
        helpers::remove_dir_if_empty(&date_dir);
        return Err(InnerErr::Io(format!("write manifest failed: {e}")));
    }

    Ok(Inner::Recycled { entry_id, manifest })
}

/// Best-effort undo of a partially completed recycle: move everything
/// that already landed in the entry back to where it came from, then drop
/// the (now empty) entry directory.
fn rollback(moved: &[(PathBuf, PathBuf)], entry_dir: &Path) {
    for (src, dst) in moved.iter().rev() {
        let _ = helpers::move_path(dst, src, false);
    }
    let _ = fs::remove_dir_all(entry_dir);
}

fn permanent_delete(kind: RecycleKind, path: &Path) -> io::Result<()> {
    match kind {
        RecycleKind::Episode => {
            for c in helpers::companions(path) {
                let _ = fs::remove_file(&c);
            }
            fs::remove_file(path)
        }
        RecycleKind::SeriesFolder => fs::remove_dir_all(path),
    }
}

fn write_manifest(entry_dir: &Path, manifest: &RecycleManifest) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    let final_path = entry_dir.join(MANIFEST_FILE);
    let tmp = entry_dir.join(format!("{MANIFEST_FILE}.tmp"));
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &final_path)
}

fn read_manifest(entry_dir: &Path) -> io::Result<RecycleManifest> {
    let bytes = fs::read(entry_dir.join(MANIFEST_FILE))?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn parse_date_dir(name: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()
}

/// Every entry in the bin, newest first. Entries with a missing or
/// unparseable manifest are skipped (logged via tracing, not the DB, since
/// this runs on every list-page render).
pub async fn list_entries(bin_path: &str) -> Result<Vec<RecycleEntry>, String> {
    let bin = bin_path.trim().to_string();
    if bin.is_empty() {
        return Ok(Vec::new());
    }
    tokio::task::spawn_blocking(move || list_entries_blocking(Path::new(&bin)))
        .await
        .map_err(|e| format!("list task panicked: {e}"))?
}

fn list_entries_blocking(bin: &Path) -> Result<Vec<RecycleEntry>, String> {
    let date_dirs = match fs::read_dir(bin) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", bin.display())),
    };
    let mut out = Vec::new();
    for date_entry in date_dirs.flatten() {
        let date_name = date_entry.file_name().to_string_lossy().into_owned();
        if parse_date_dir(&date_name).is_none() {
            continue;
        }
        let Ok(entries) = fs::read_dir(date_entry.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if !is_valid_entry_id(&id) {
                continue;
            }
            let dir = entry.path();
            match read_manifest(&dir) {
                Ok(manifest) => out.push(RecycleEntry {
                    entry_id: id,
                    date: date_name.clone(),
                    dir,
                    manifest,
                }),
                Err(e) => {
                    tracing::warn!(
                        target: "ryokan::recycle",
                        entry = %dir.display(),
                        error = %e,
                        "skipping recycle entry with unreadable manifest",
                    );
                }
            }
        }
    }
    out.sort_by(|a, b| {
        b.manifest
            .recycled_at
            .cmp(&a.manifest.recycled_at)
            .then_with(|| a.entry_id.cmp(&b.entry_id))
    });
    Ok(out)
}

/// Locate one entry by id. Ids are validated before touching the
/// filesystem so a crafted segment can't walk out of the bin.
pub async fn find_entry(bin_path: &str, entry_id: &str) -> Result<Option<RecycleEntry>, String> {
    if !is_valid_entry_id(entry_id) {
        return Err("invalid recycle entry id".to_string());
    }
    let entries = list_entries(bin_path).await?;
    Ok(entries.into_iter().find(|e| e.entry_id == entry_id))
}

/// One normal path component: no separator, no `..`, not absolute.
fn is_plain_name(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) && !name.contains(['/', '\\'])
}

/// Put an entry back exactly where it came from and drop the entry.
/// `media_root` is the library root the manifest path must sit under;
/// delete guards its paths with a canonicalize + `starts_with` check and
/// restore holds the same line, so a stray or hand-edited manifest can't
/// turn restore into arbitrary file placement.
pub async fn restore(
    bin_path: &str,
    entry_id: &str,
    media_root: &str,
) -> Result<RestoreOutcome, String> {
    let Some(entry) = find_entry(bin_path, entry_id).await? else {
        return Err("recycle entry not found".to_string());
    };
    let media_root = media_root.trim().trim_end_matches('/').to_string();
    invalidate_entry_count();
    tokio::task::spawn_blocking(move || restore_blocking(&entry, &media_root))
        .await
        .map_err(|e| format!("restore task panicked: {e}"))?
}

/// Lexical containment: absolute, no `.`/`..` components, under `root`.
/// Canonicalizing isn't an option because the original path usually no
/// longer exists (that's why we're restoring it).
fn path_under_root(path: &Path, root: &str) -> bool {
    use std::path::Component;
    if root.is_empty() || !path.is_absolute() {
        return false;
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    path.starts_with(Path::new(root))
}

fn restore_blocking(entry: &RecycleEntry, media_root: &str) -> Result<RestoreOutcome, String> {
    let original = PathBuf::from(&entry.manifest.original_path);
    if !path_under_root(&original, media_root) {
        return Ok(RestoreOutcome::OutsideMediaRoot);
    }
    // Every listed name is a plain file name inside the entry. The
    // manifest is a file in the bin (a shared volume, a restored backup),
    // and a `..` or absolute name joined below moved anything on disk
    // into the library: only `original_path` was checked.
    if let Some(bad) = entry.manifest.files.iter().find(|f| !is_plain_name(f)) {
        return Err(format!(
            "manifest lists '{bad}', which is not a file name inside the entry"
        ));
    }
    // Nor a link: `recycle` refuses one, so a link in an entry was
    // planted there, and the cross-filesystem copy followed it and put
    // its target (any file Ryokan can read) into the library.
    if let Some(link) =
        entry.manifest.files.iter().find(|f| {
            fs::symlink_metadata(entry.dir.join(f)).is_ok_and(|m| m.file_type().is_symlink())
        })
    {
        return Err(format!(
            "'{link}' in the entry is a symbolic link, which the recycle bin never stores"
        ));
    }

    let targets: Vec<(PathBuf, PathBuf)> = match entry.manifest.kind {
        RecycleKind::Episode => {
            let Some(parent) = original.parent() else {
                return Err("manifest original_path has no parent".to_string());
            };
            if !parent.is_dir() {
                // The season dir may have been pruned when it emptied; the
                // series root going away means there's nothing to put the
                // file back into.
                let series_root_exists = parent.parent().map(|p| p.is_dir()).unwrap_or(false);
                if !series_root_exists {
                    return Ok(RestoreOutcome::OriginalLocationMissing);
                }
                fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            entry
                .manifest
                .files
                .iter()
                .map(|f| (entry.dir.join(f), parent.join(f)))
                .collect()
        }
        RecycleKind::SeriesFolder => {
            let Some(parent) = original.parent() else {
                return Err("manifest original_path has no parent".to_string());
            };
            if !parent.is_dir() {
                return Ok(RestoreOutcome::OriginalLocationMissing);
            }
            let Some(folder) = entry.manifest.files.first() else {
                return Err("manifest lists no folder".to_string());
            };
            vec![(entry.dir.join(folder), original.clone())]
        }
    };

    if targets.iter().any(|(_, dst)| dst.exists()) {
        return Ok(RestoreOutcome::ConflictAtTarget);
    }

    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (src, dst) in &targets {
        if let Err(e) = helpers::move_path(src, dst, false) {
            // Undo: move restored files back into the entry so the bin
            // stays consistent with its manifest.
            for (s, d) in moved.iter().rev() {
                let _ = helpers::move_path(d, s, false);
            }
            return Err(format!(
                "restore move {} -> {} failed: {e}",
                src.display(),
                dst.display()
            ));
        }
        moved.push((src.clone(), dst.clone()));
    }

    let _ = fs::remove_dir_all(&entry.dir);
    if let Some(date_dir) = entry.dir.parent() {
        helpers::remove_dir_if_empty(date_dir);
    }
    Ok(RestoreOutcome::Restored {
        final_path: original,
    })
}

/// Permanently delete one entry ("Delete now"). Returns bytes freed.
pub async fn delete_entry(bin_path: &str, entry_id: &str) -> Result<u64, String> {
    let Some(entry) = find_entry(bin_path, entry_id).await? else {
        return Err("recycle entry not found".to_string());
    };
    invalidate_entry_count();
    tokio::task::spawn_blocking(move || {
        let bytes = entry.manifest.size_bytes;
        fs::remove_dir_all(&entry.dir)
            .map_err(|e| format!("remove {}: {e}", entry.dir.display()))?;
        if let Some(date_dir) = entry.dir.parent() {
            helpers::remove_dir_if_empty(date_dir);
        }
        Ok(bytes)
    })
    .await
    .map_err(|e| format!("delete task panicked: {e}"))?
}

/// Drop every date bucket older than `age_days`. `age_days <= 0` disables
/// auto-purge (forever-recycle with manual cleanup) and returns an empty
/// report without touching disk. Non-date-named directories are ignored.
pub async fn purge_old(bin_path: &str, age_days: i64) -> Result<PurgeReport, String> {
    let bin = bin_path.trim().to_string();
    if bin.is_empty() || age_days <= 0 {
        return Ok(PurgeReport::default());
    }
    let today = Utc::now().date_naive();
    invalidate_entry_count();
    tokio::task::spawn_blocking(move || purge_blocking(Path::new(&bin), today, Some(age_days)))
        .await
        .map_err(|e| format!("purge task panicked: {e}"))?
}

/// Permanently delete every entry ("Empty recycle bin").
pub async fn empty(bin_path: &str) -> Result<PurgeReport, String> {
    let bin = bin_path.trim().to_string();
    if bin.is_empty() {
        return Ok(PurgeReport::default());
    }
    let today = Utc::now().date_naive();
    invalidate_entry_count();
    tokio::task::spawn_blocking(move || purge_blocking(Path::new(&bin), today, None))
        .await
        .map_err(|e| format!("empty task panicked: {e}"))?
}

/// `age_days = None` purges every date bucket; `Some(n)` only those whose
/// date is more than `n` days before `today`.
fn purge_blocking(
    bin: &Path,
    today: NaiveDate,
    age_days: Option<i64>,
) -> Result<PurgeReport, String> {
    let date_dirs = match fs::read_dir(bin) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PurgeReport::default()),
        Err(e) => return Err(format!("read {}: {e}", bin.display())),
    };
    let mut report = PurgeReport::default();
    for date_entry in date_dirs.flatten() {
        let name = date_entry.file_name().to_string_lossy().into_owned();
        let Some(date) = parse_date_dir(&name) else {
            continue;
        };
        if !date_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Some(age) = age_days
            && (today - date).num_days() <= age
        {
            continue;
        }
        let dir = date_entry.path();
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                if !is_valid_entry_id(&id) {
                    continue;
                }
                report.entries += 1;
                report.bytes += read_manifest(&e.path())
                    .map(|m| m.size_bytes)
                    .unwrap_or_else(|_| helpers::path_size(&e.path()));
            }
        }
        fs::remove_dir_all(&dir).map_err(|e| format!("remove {}: {e}", dir.display()))?;
        report.date_dirs += 1;
    }
    Ok(report)
}

/// Totals for the "Empty recycle bin" confirmation copy.
pub async fn summary(bin_path: &str) -> Result<(u64, u64), String> {
    let entries = list_entries(bin_path).await?;
    let bytes = entries.iter().map(|e| e.manifest.size_bytes).sum();
    Ok((entries.len() as u64, bytes))
}

/// Cheap writability probe for the settings page / list-page banner:
/// creates the bin directory if needed and touches a probe file.
pub async fn probe_writable(bin_path: &str) -> Result<(), String> {
    let bin = bin_path.trim().to_string();
    if bin.is_empty() {
        return Err("recycle bin path is empty".to_string());
    }
    tokio::task::spawn_blocking(move || {
        let bin = Path::new(&bin);
        fs::create_dir_all(bin).map_err(|e| format!("create {}: {e}", bin.display()))?;
        let probe = bin.join(".ryokan-write-probe");
        fs::write(&probe, b"").map_err(|e| format!("write {}: {e}", probe.display()))?;
        let _ = fs::remove_file(&probe);
        Ok(())
    })
    .await
    .map_err(|e| format!("probe task panicked: {e}"))?
}

/// `true` when `bin` and `media_root` sit on the same filesystem (so
/// recycle is an instant rename). Used for the settings-page hint only.
#[cfg(unix)]
pub fn same_filesystem(bin: &Path, media_root: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let a = fs::metadata(bin).ok()?;
    let b = fs::metadata(media_root).ok()?;
    Some(a.dev() == b.dev())
}

#[cfg(not(unix))]
pub fn same_filesystem(_bin: &Path, _media_root: &Path) -> Option<bool> {
    None
}

/// `1.2 GiB` / `340.0 MiB` style rendering for log lines and the list
/// page; binary units to match the series page's size display.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
