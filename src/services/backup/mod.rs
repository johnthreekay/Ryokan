//! Backup and restore (issue #126).
//!
//! A backup is a `tar.gz` holding `manifest.json`, a `VACUUM INTO`
//! snapshot of the database as `ryokan.db`, the AEAD key file
//! `.ryokan-key` when one exists (skipped for a sanitized backup), and
//! optionally the artwork cache under `artwork/`.
//!
//! `VACUUM INTO` rather than `fs::copy`: the database runs in WAL mode,
//! so a plain copy of `ryokan.db` misses everything still sitting in
//! `ryokan.db-wal` and silently produces a stale snapshot. `VACUUM
//! INTO` writes a consistent, WAL-free file from the live connection.
//!
//! Restore is stage-then-restart. [`stage_restore`] validates an
//! uploaded archive, takes an automatic pre-restore backup of the
//! current state, and parks the extracted files under
//! `<data dir>/.ryokan-restore-pending/`. [`apply_pending_restore`]
//! runs at the next boot, **before** the connection pool opens, and
//! swaps the files into place, keeping the previous database beside the
//! restored one as `ryokan.db.pre-restore-<ts>`. The pending directory's
//! existence is the lock: a second upload while one is staged is
//! refused, and cancelling is a directory removal.
//!
//! A backup is high-trust: it carries the encryption key, the encrypted
//! OAuth tokens, every stored password, and the activity log. The
//! sanitized variant (for support shares) runs the same scrubber as
//! `--sanitize-db-for-debug`, trims the log table, and omits the key
//! and hostname.

use std::fmt;
use std::fs;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::models::config::Config;
use crate::services::{artwork, crypto, paths, sanitize};

#[cfg(test)]
mod tests;

/// Serializes the scheduled task, the download endpoint, and the
/// pre-restore backup. `try_lock` so a manual download during the
/// scheduled run gets "busy" instead of queuing.
pub static BACKUP_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Directory under the data dir whose presence means "apply at boot".
pub const PENDING_DIR_NAME: &str = ".ryokan-restore-pending";
/// Work dirs under the data dir. Their contents live exactly as long as
/// one backup / one upload; [`sweep_work_dirs`] clears what a crash
/// stranded.
pub const BACKUP_WORK_DIR_NAME: &str = ".backup-tmp";
pub const RESTORE_WORK_DIR_NAME: &str = ".restore-staging-tmp";
/// Hard cap on an uploaded archive. Artwork-inclusive backups of a
/// large library run to a few gigabytes; anything past this is not a
/// Ryokan backup.
pub const MAX_UPLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Log rows a sanitized backup keeps.
pub const SANITIZED_LOG_ROWS: i64 = 1000;
const DB_MAGIC: &[u8] = b"SQLite format 3\0";

/// The backup folder used when the setting is empty, as an absolute
/// path for the Settings field's placeholder (`/data/backups` in the
/// default image, `/config/backups` with `RYOKAN_DATA_DIR=/config`).
pub fn default_backup_dir_display() -> String {
    let dir = paths::db_dir().join("backups");
    std::path::absolute(&dir)
        .unwrap_or(dir)
        .display()
        .to_string()
}

/// Where the pieces of an install live. Built once from the
/// environment at boot; tests construct one over a temp dir.
#[derive(Clone, Debug)]
pub struct BackupPaths {
    /// Parent of the live database. Work dirs, the pending-restore dir,
    /// and the default backup folder live under it.
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub key_path: PathBuf,
    pub artwork_dir: PathBuf,
}

impl BackupPaths {
    pub fn from_env() -> Self {
        Self {
            data_dir: paths::db_dir(),
            db_path: paths::live_db_path(),
            key_path: crypto::key_file_path(),
            artwork_dir: artwork::media_cache_dir(),
        }
    }

    pub fn pending_dir(&self) -> PathBuf {
        self.data_dir.join(PENDING_DIR_NAME)
    }

    /// The configured backup folder, or `<data dir>/backups`.
    pub fn backup_dir(&self, configured: &str) -> PathBuf {
        let configured = configured.trim();
        if configured.is_empty() {
            self.data_dir.join("backups")
        } else {
            PathBuf::from(configured)
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BackupManifest {
    pub ryokan_version: String,
    /// Unix seconds.
    pub backup_timestamp: i64,
    /// `MAX(id)` of `schema_migrations` at backup time. Restore refuses
    /// a backup whose value is above the running binary's: a newer
    /// schema can't be read by an older build. Monotonic, unlike the
    /// version string.
    pub max_migration_id: i64,
    pub includes_artwork: bool,
    pub includes_key: bool,
    pub sanitized: bool,
    /// Diagnostic only; omitted from sanitized backups.
    pub hostname: Option<String>,
    pub db_size_bytes: u64,
    pub artwork_size_bytes: u64,
}

impl BackupManifest {
    pub fn timestamp_label(&self) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp(self.backup_timestamp, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| self.backup_timestamp.to_string())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackupOptions {
    pub include_artwork: bool,
    pub sanitize: bool,
}

#[derive(Debug)]
pub enum BackupError {
    /// Another backup (scheduled, download, or pre-restore) is running.
    Busy,
    /// The disk-space precheck failed. Both values in bytes.
    NoSpace {
        needed: u64,
        free: u64,
    },
    Other(String),
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackupError::Busy => write!(f, "A backup is already running."),
            BackupError::NoSpace { needed, free } => write!(
                f,
                "Not enough free disk space for a backup: about {} needed, {} free.",
                human_bytes(*needed),
                human_bytes(*free)
            ),
            BackupError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<String> for BackupError {
    fn from(e: String) -> Self {
        BackupError::Other(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupKind {
    /// `ryokan-backup-<ts>.tar.gz`: scheduled runs and "back up now".
    Scheduled,
    /// `auto-pre-restore-<ts>.tar.gz`: taken by [`stage_restore`] before
    /// a restore is staged. Never pruned by retention.
    PreRestore,
}

impl BackupKind {
    fn prefix(self) -> &'static str {
        match self {
            BackupKind::Scheduled => "ryokan-backup-",
            BackupKind::PreRestore => "auto-pre-restore-",
        }
    }
}

pub fn backup_file_name(kind: BackupKind, timestamp: i64) -> String {
    format!("{}{}.tar.gz", kind.prefix(), timestamp)
}

/// `(kind, timestamp)` for a file name this module produced; `None` for
/// anything else, which also makes it the allow-list for the download
/// and delete endpoints.
pub fn parse_backup_name(name: &str) -> Option<(BackupKind, i64)> {
    for kind in [BackupKind::Scheduled, BackupKind::PreRestore] {
        if let Some(rest) = name.strip_prefix(kind.prefix())
            && let Some(digits) = rest.strip_suffix(".tar.gz")
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(ts) = digits.parse::<i64>()
        {
            return Some((kind, ts));
        }
    }
    None
}

#[derive(Clone, Debug)]
pub struct BackupFile {
    pub name: String,
    pub path: PathBuf,
    pub kind: BackupKind,
    pub timestamp: i64,
    pub size_bytes: u64,
}

/// Backups in `dir`, newest first. A missing dir is an empty list.
pub fn list_backups(dir: &Path) -> Vec<BackupFile> {
    let mut out: Vec<BackupFile> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (kind, timestamp) = parse_backup_name(&name)?;
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(BackupFile {
                name,
                path: e.path(),
                kind,
                timestamp,
                size_bytes: meta.len(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.name.cmp(&a.name)));
    out
}

/// Timestamp of the newest scheduled-kind backup in `dir`.
pub fn newest_backup_timestamp(dir: &Path) -> Option<i64> {
    list_backups(dir)
        .into_iter()
        .filter(|b| b.kind == BackupKind::Scheduled)
        .map(|b| b.timestamp)
        .max()
}

/// Delete scheduled-kind backups beyond the newest `keep`. Pre-restore
/// backups are never touched: they are the escape hatch. Returns the
/// names removed.
pub fn prune_backups(dir: &Path, keep: usize) -> Result<Vec<String>, String> {
    let scheduled: Vec<BackupFile> = list_backups(dir)
        .into_iter()
        .filter(|b| b.kind == BackupKind::Scheduled)
        .collect();
    let mut removed = Vec::new();
    for old in scheduled.into_iter().skip(keep.max(1)) {
        fs::remove_file(&old.path).map_err(|e| format!("remove {}: {e}", old.path.display()))?;
        removed.push(old.name);
    }
    Ok(removed)
}

/// Free bytes on the filesystem holding `path` (the nearest existing
/// ancestor). `None` when the platform or the path can't answer.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut probe = path;
    while !probe.exists() {
        probe = probe.parent()?;
    }
    let st = nix::sys::statvfs::statvfs(probe).ok()?;
    Some(st.blocks_available() as u64 * st.fragment_size() as u64)
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

pub fn human_bytes(bytes: u64) -> String {
    crate::services::recycle::human_bytes(bytes)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random_id() -> String {
    let bytes: [u8; 8] = rand::random();
    hex::encode(bytes)
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// `VACUUM INTO <path>`: a consistent, WAL-free snapshot of the live
/// database. The path is spliced as an escaped literal: SQLite reads a
/// bound parameter here as an empty string and quietly runs a plain
/// `VACUUM` instead, producing no file at all. The path is one this
/// module built under the data dir, never user input.
pub(crate) async fn vacuum_into(db: &SqlitePool, path: &Path) -> Result<(), String> {
    let literal = path.to_string_lossy().replace('\'', "''");
    sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{literal}'")))
        .execute(db)
        .await
        .map_err(|e| format!("VACUUM INTO failed: {e}"))?;
    if !path.is_file() {
        return Err(format!(
            "VACUUM INTO reported success but wrote nothing at {}",
            path.display()
        ));
    }
    Ok(())
}

/// `MAX(id)` of `schema_migrations`, 0 when the table is empty or
/// missing.
pub async fn max_migration_id(db: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(id) FROM schema_migrations")
        .fetch_one(db)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// Write a backup of the live database to `out` (the final `.tar.gz`
/// path; the parent is created). The archive is assembled at
/// `<out>.partial` and renamed into place, so a reader never sees a
/// half-written file.
pub async fn create_backup(
    db: &SqlitePool,
    paths: &BackupPaths,
    opts: BackupOptions,
    out: &Path,
) -> Result<BackupManifest, BackupError> {
    let _guard = BACKUP_LOCK.try_lock().map_err(|_| BackupError::Busy)?;

    let db_size = fs::metadata(&paths.db_path).map(|m| m.len()).unwrap_or(0);
    let artwork_size = if opts.include_artwork && paths.artwork_dir.is_dir() {
        dir_size(&paths.artwork_dir)
    } else {
        0
    };
    // The work dir briefly holds the snapshot (plus the scrubbed copy
    // when sanitizing); the archive lands at `out`, which can sit on
    // another filesystem (a mounted share is the point of a backup
    // folder). Both get checked, before compression helps.
    let work_needed = db_size.saturating_mul(if opts.sanitize { 3 } else { 2 });
    let out_needed = db_size.saturating_add(artwork_size);
    let out_dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| paths.data_dir.clone());
    for (dir, needed) in [(&paths.data_dir, work_needed), (&out_dir, out_needed)] {
        if let Some(free) = free_bytes(dir)
            && free < needed
        {
            return Err(BackupError::NoSpace { needed, free });
        }
    }

    let work = paths.data_dir.join(BACKUP_WORK_DIR_NAME).join(random_id());
    fs::create_dir_all(&work).map_err(|e| format!("create {}: {e}", work.display()))?;
    let result = build_backup(db, paths, opts, out, &work, artwork_size).await;
    let _ = fs::remove_dir_all(&work);
    result
}

async fn build_backup(
    db: &SqlitePool,
    paths: &BackupPaths,
    opts: BackupOptions,
    out: &Path,
    work: &Path,
    artwork_size: u64,
) -> Result<BackupManifest, BackupError> {
    let snapshot = work.join("ryokan.db");
    vacuum_into(db, &snapshot).await?;
    drop_sessions(&snapshot).await?;

    let db_for_archive = if opts.sanitize {
        let scrubbed = work.join("ryokan-sanitized.db");
        sanitize::run_sanitize(&snapshot, &scrubbed)
            .await
            .map_err(|e| format!("sanitize failed: {e}"))?;
        trim_logs(&scrubbed).await?;
        scrubbed
    } else {
        snapshot
    };

    let include_key = !opts.sanitize && paths.key_path.is_file();
    let include_artwork = opts.include_artwork && paths.artwork_dir.is_dir();
    let manifest = BackupManifest {
        ryokan_version: env!("CARGO_PKG_VERSION").to_string(),
        backup_timestamp: now_unix(),
        max_migration_id: max_migration_id(db).await,
        includes_artwork: include_artwork,
        includes_key: include_key,
        sanitized: opts.sanitize,
        hostname: if opts.sanitize { None } else { hostname() },
        db_size_bytes: fs::metadata(&db_for_archive).map(|m| m.len()).unwrap_or(0),
        artwork_size_bytes: if include_artwork { artwork_size } else { 0 },
    };

    let out = out.to_path_buf();
    let key = include_key.then(|| paths.key_path.clone());
    let art = include_artwork.then(|| paths.artwork_dir.clone());
    let manifest_for_task = manifest.clone();
    tokio::task::spawn_blocking(move || {
        write_archive(
            &out,
            &manifest_for_task,
            &db_for_archive,
            key.as_deref(),
            art.as_deref(),
        )
    })
    .await
    .map_err(|e| format!("backup task panicked: {e}"))??;

    Ok(manifest)
}

/// Keep only the newest [`SANITIZED_LOG_ROWS`] log rows in a scrubbed
/// copy, then compact it.
/// Create `path` readable by its owner only (0600 on Unix). A full backup
/// holds the encryption key and every credential, and the backup folder
/// is user-configurable (a NAS share, a folder other containers mount),
/// so the archive must not inherit a world-readable umask. A leftover
/// file is removed first: the mode applies only when the file is created.
fn create_private(path: &Path) -> std::io::Result<fs::File> {
    let _ = fs::remove_file(path);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Delete the snapshot's session rows. Each is a working admin cookie for
/// up to 7 days to anyone who can read the archive, and restore deletes
/// them anyway. `secure_delete` zeroes the freed rows, so the tokens
/// don't linger in free pages either.
async fn drop_sessions(db_path: &Path) -> Result<(), String> {
    let url = format!("sqlite://{}?mode=rw", db_path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .map_err(|e| format!("open snapshot: {e}"))?;
    sqlx::query("PRAGMA secure_delete = ON")
        .execute(&pool)
        .await
        .map_err(|e| format!("secure_delete: {e}"))?;
    // An old database without the table has nothing to drop.
    let _ = sqlx::query("DELETE FROM sessions").execute(&pool).await;
    pool.close().await;
    Ok(())
}

async fn trim_logs(db_path: &Path) -> Result<(), String> {
    let url = format!("sqlite://{}?mode=rw", db_path.display());
    let pool = SqlitePool::connect(&url)
        .await
        .map_err(|e| format!("open sanitized copy: {e}"))?;
    sqlx::query("DELETE FROM logs WHERE id NOT IN (SELECT id FROM logs ORDER BY id DESC LIMIT ?)")
        .bind(SANITIZED_LOG_ROWS)
        .execute(&pool)
        .await
        .map_err(|e| format!("trim logs: {e}"))?;
    sqlx::query("VACUUM")
        .execute(&pool)
        .await
        .map_err(|e| format!("vacuum sanitized copy: {e}"))?;
    pool.close().await;
    Ok(())
}

fn write_archive(
    out: &Path,
    manifest: &BackupManifest,
    db_file: &Path,
    key_file: Option<&Path>,
    artwork_dir: Option<&Path>,
) -> Result<(), String> {
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let mut partial = out.as_os_str().to_os_string();
    partial.push(".partial");
    let partial = PathBuf::from(partial);

    let result = (|| -> Result<(), String> {
        let file =
            create_private(&partial).map_err(|e| format!("create {}: {e}", partial.display()))?;
        let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        archive.follow_symlinks(false);

        let manifest_bytes =
            serde_json::to_vec_pretty(manifest).map_err(|e| format!("manifest: {e}"))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(manifest.backup_timestamp.max(0) as u64);
        header.set_cksum();
        archive
            .append_data(&mut header, "manifest.json", manifest_bytes.as_slice())
            .map_err(|e| format!("append manifest: {e}"))?;
        archive
            .append_path_with_name(db_file, "ryokan.db")
            .map_err(|e| format!("append database: {e}"))?;
        if let Some(key) = key_file {
            archive
                .append_path_with_name(key, ".ryokan-key")
                .map_err(|e| format!("append key: {e}"))?;
        }
        if let Some(dir) = artwork_dir {
            archive
                .append_dir_all("artwork", dir)
                .map_err(|e| format!("append artwork: {e}"))?;
        }
        let encoder = archive
            .into_inner()
            .map_err(|e| format!("finish archive: {e}"))?;
        let mut writer = encoder.finish().map_err(|e| format!("finish gzip: {e}"))?;
        writer.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    fs::rename(&partial, out).map_err(|e| {
        let _ = fs::remove_file(&partial);
        format!("rename {} into place: {e}", partial.display())
    })
}

// ── Scheduled runs ──────────────────────────────────────────────────

/// Interval for a `config.backup_schedule` value; `None` = disabled.
pub fn schedule_interval(schedule: &str) -> Option<std::time::Duration> {
    match schedule {
        "daily" => Some(std::time::Duration::from_secs(24 * 60 * 60)),
        "weekly" => Some(std::time::Duration::from_secs(7 * 24 * 60 * 60)),
        _ => None,
    }
}

/// Whether a scheduled backup is due: no scheduled backup in `dir`
/// yet, or the newest is older than `interval` (minus a few minutes so
/// an hourly tick landing just short of a day doesn't slip a whole
/// tick).
pub fn is_due(dir: &Path, interval: std::time::Duration) -> bool {
    let grace = 5 * 60;
    match newest_backup_timestamp(dir) {
        None => true,
        Some(ts) => now_unix() - ts >= interval.as_secs() as i64 - grace,
    }
}

#[derive(Clone, Debug)]
pub struct FolderRun {
    pub file_name: String,
    pub dir: PathBuf,
    pub manifest: BackupManifest,
    pub pruned: Vec<String>,
}

/// One scheduled-style run: a `ryokan-backup-<ts>.tar.gz` into the
/// configured folder (artwork per `config.backup_include_artwork`),
/// then retention pruning. Shared by the supervised task and the
/// "Save to backup folder" / Run-now buttons.
pub async fn run_to_folder(
    db: &SqlitePool,
    paths: &BackupPaths,
    cfg: &Config,
) -> Result<FolderRun, BackupError> {
    let dir = paths.backup_dir(&cfg.backup_directory);
    let file_name = backup_file_name(BackupKind::Scheduled, now_unix());
    let manifest = create_backup(
        db,
        paths,
        BackupOptions {
            include_artwork: cfg.backup_include_artwork,
            sanitize: false,
        },
        &dir.join(&file_name),
    )
    .await?;
    let keep = cfg.backup_retention_count.clamp(1, 365) as usize;
    let pruned = prune_backups(&dir, keep).map_err(BackupError::Other)?;
    Ok(FolderRun {
        file_name,
        dir,
        manifest,
        pruned,
    })
}

// ── Restore ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum RestoreError {
    /// A restore is already staged; restart to apply it or cancel it.
    Pending,
    /// The upload is not a Ryokan backup, or is damaged.
    Invalid(String),
    /// The backup comes from a newer Ryokan than this one.
    Incompatible(String),
    Other(String),
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::Pending => write!(
                f,
                "A restore is already staged. Restart Ryokan to apply it, or cancel it first."
            ),
            RestoreError::Invalid(e) => write!(f, "This file is not a usable Ryokan backup: {e}"),
            RestoreError::Incompatible(e) => write!(f, "{e}"),
            RestoreError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<String> for RestoreError {
    fn from(e: String) -> Self {
        RestoreError::Other(e)
    }
}

#[derive(Clone, Debug)]
pub struct StagedRestore {
    pub manifest: BackupManifest,
    /// File name of the automatic backup taken before staging.
    pub pre_restore_backup: String,
    pub warnings: Vec<String>,
}

/// Validate an uploaded archive and stage it for the next boot. Takes
/// a pre-restore backup of the current state into `backup_dir` first,
/// so a wrong upload is always recoverable. `upload` is deleted when
/// this returns, success or not.
pub async fn stage_restore(
    db: &SqlitePool,
    paths: &BackupPaths,
    backup_dir: &Path,
    upload: &Path,
) -> Result<StagedRestore, RestoreError> {
    let result = stage_restore_inner(db, paths, backup_dir, upload).await;
    let _ = fs::remove_file(upload);
    result
}

async fn stage_restore_inner(
    db: &SqlitePool,
    paths: &BackupPaths,
    backup_dir: &Path,
    upload: &Path,
) -> Result<StagedRestore, RestoreError> {
    let pending = paths.pending_dir();
    if pending.exists() {
        return Err(RestoreError::Pending);
    }
    let staging = paths.data_dir.join(RESTORE_WORK_DIR_NAME).join(random_id());
    fs::create_dir_all(&staging).map_err(|e| format!("create {}: {e}", staging.display()))?;

    let result = stage_into(db, paths, backup_dir, upload, &staging, &pending).await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

async fn stage_into(
    db: &SqlitePool,
    paths: &BackupPaths,
    backup_dir: &Path,
    upload: &Path,
    staging: &Path,
    pending: &Path,
) -> Result<StagedRestore, RestoreError> {
    {
        let upload = upload.to_path_buf();
        let staging = staging.to_path_buf();
        tokio::task::spawn_blocking(move || extract_archive(&upload, &staging))
            .await
            .map_err(|e| format!("extract task panicked: {e}"))??;
    }

    let manifest = read_manifest(staging)?;
    check_database_file(&staging.join("ryokan.db"))?;

    let running = max_migration_id(db).await;
    if manifest.max_migration_id > running {
        return Err(RestoreError::Incompatible(format!(
            "This backup was made by a newer Ryokan (schema {} vs {} here). Update Ryokan first.",
            manifest.max_migration_id, running
        )));
    }
    if version_tuple(&manifest.ryokan_version) > version_tuple(env!("CARGO_PKG_VERSION")) {
        return Err(RestoreError::Incompatible(format!(
            "This backup was made by Ryokan {} and this is {}. Update Ryokan first.",
            manifest.ryokan_version,
            env!("CARGO_PKG_VERSION")
        )));
    }

    prepare_staged_database(&staging.join("ryokan.db"), manifest.sanitized).await?;

    let mut warnings = Vec::new();
    if !staging.join(".ryokan-key").is_file() {
        warnings.push(
            "This backup has no encryption key. Linked AniList and MyAnimeList accounts will need to be linked again after the restore."
                .to_string(),
        );
    }
    if manifest.sanitized {
        warnings.push(
            "This is a sanitized backup. Its passwords, API keys, and account tokens were removed when it was made. After the restore, create the admin account again on the first page load, enter the client passwords and API keys again, create new scoped API keys, and link AniList and MyAnimeList again."
                .to_string(),
        );
    }

    // The escape hatch: whatever is running now, saved before anything
    // is staged. Artwork is regenerable, so it stays out.
    let pre_name = backup_file_name(BackupKind::PreRestore, now_unix());
    create_backup(
        db,
        paths,
        BackupOptions::default(),
        &backup_dir.join(&pre_name),
    )
    .await
    .map_err(|e| match e {
        BackupError::Busy => RestoreError::Other(
            "A backup is running right now. Try the restore again when it finishes.".to_string(),
        ),
        other => RestoreError::Other(format!("Pre-restore backup failed: {other}")),
    })?;

    if pending.exists() {
        return Err(RestoreError::Pending);
    }
    fs::rename(staging, pending)
        .map_err(|e| format!("stage restore at {}: {e}", pending.display()))?;

    Ok(StagedRestore {
        manifest,
        pre_restore_backup: pre_name,
        warnings,
    })
}

/// Unpack `archive` into `into`, accepting only the entries a Ryokan
/// backup contains. `unpack_in` refuses anything that would land
/// outside `into`.
/// Most entries a restore unpacks (artwork blobs are most of them).
const RESTORE_MAX_ENTRIES: usize = 500_000;
/// Most bytes a restore unpacks: well past any real database plus
/// artwork, short of filling a disk on a gzip bomb.
const RESTORE_MAX_TOTAL_BYTES: u64 = 64 << 30;

fn extract_archive(archive: &Path, into: &Path) -> Result<(), RestoreError> {
    let file =
        fs::File::open(archive).map_err(|e| RestoreError::Other(format!("open upload: {e}")))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut tar = tar::Archive::new(decoder);
    let entries = tar
        .entries()
        .map_err(|e| RestoreError::Invalid(format!("not a gzip tar archive ({e})")))?;
    let mut saw_any = false;
    let (mut count, mut total) = (0usize, 0u64);
    for entry in entries {
        let mut entry =
            entry.map_err(|e| RestoreError::Invalid(format!("damaged archive ({e})")))?;
        // Regular files and directories only: `unpack_in` creates a
        // symlink entry with any target, so a `ryokan.db` symlink to the
        // live database had the staging checks (and the session purge)
        // run against the live file.
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(RestoreError::Invalid(
                "the archive holds a link or special file, which a Ryokan backup never has"
                    .to_string(),
            ));
        }
        // Size and count caps, so a crafted archive can't fill the
        // filesystem the live database shares before any check runs.
        let size = entry.header().size().unwrap_or(u64::MAX);
        count += 1;
        total = total.saturating_add(size);
        if count > RESTORE_MAX_ENTRIES || total > RESTORE_MAX_TOTAL_BYTES {
            return Err(RestoreError::Invalid(
                "the archive is larger than any Ryokan backup".to_string(),
            ));
        }
        let path = entry
            .path()
            .map_err(|e| RestoreError::Invalid(format!("bad entry path ({e})")))?
            .into_owned();
        let mut components = path
            .components()
            .filter(|c| !matches!(c, Component::CurDir));
        let first = match components.next() {
            Some(Component::Normal(name)) => name.to_string_lossy().into_owned(),
            _ => {
                return Err(RestoreError::Invalid(format!(
                    "unexpected entry '{}'",
                    path.display()
                )));
            }
        };
        if components.any(|c| !matches!(c, Component::Normal(_))) {
            return Err(RestoreError::Invalid(format!(
                "unexpected entry '{}'",
                path.display()
            )));
        }
        let allowed = matches!(
            first.as_str(),
            "manifest.json" | "ryokan.db" | ".ryokan-key" | "artwork"
        );
        if !allowed {
            return Err(RestoreError::Invalid(format!(
                "unexpected entry '{}'",
                path.display()
            )));
        }
        let entry_cap = match first.as_str() {
            "manifest.json" => 1 << 20,
            ".ryokan-key" => 64,
            _ => u64::MAX,
        };
        if size > entry_cap {
            return Err(RestoreError::Invalid(format!(
                "'{}' is larger than a Ryokan backup's",
                path.display()
            )));
        }
        let unpacked = entry.unpack_in(into).map_err(|e| {
            RestoreError::Invalid(format!("could not unpack '{}': {e}", path.display()))
        })?;
        if !unpacked {
            return Err(RestoreError::Invalid(format!(
                "entry '{}' escapes the staging directory",
                path.display()
            )));
        }
        saw_any = true;
    }
    if !saw_any {
        return Err(RestoreError::Invalid("the archive is empty".to_string()));
    }
    // A key of any other length passed staging and then failed every
    // boot (a key-load failure panics at startup by design).
    let key = into.join(".ryokan-key");
    if key.exists() && fs::metadata(&key).map(|m| m.len()).unwrap_or(0) != 32 {
        return Err(RestoreError::Invalid(
            "the archive's encryption key is not 32 bytes".to_string(),
        ));
    }
    Ok(())
}

fn read_manifest(dir: &Path) -> Result<BackupManifest, RestoreError> {
    let raw = fs::read(dir.join("manifest.json"))
        .map_err(|_| RestoreError::Invalid("manifest.json is missing".to_string()))?;
    serde_json::from_slice(&raw)
        .map_err(|e| RestoreError::Invalid(format!("manifest.json is unreadable ({e})")))
}

fn check_database_file(path: &Path) -> Result<(), RestoreError> {
    let mut head = [0u8; 16];
    let mut file = fs::File::open(path)
        .map_err(|_| RestoreError::Invalid("ryokan.db is missing".to_string()))?;
    use std::io::Read;
    file.read_exact(&mut head)
        .map_err(|_| RestoreError::Invalid("ryokan.db is not a SQLite database".to_string()))?;
    if head != DB_MAGIC {
        return Err(RestoreError::Invalid(
            "ryokan.db is not a SQLite database".to_string(),
        ));
    }
    Ok(())
}

/// Integrity-check the staged database and drop its sessions so a
/// leaked backup can never hand out live logins on restore.
async fn prepare_staged_database(path: &Path, sanitized: bool) -> Result<(), RestoreError> {
    let url = format!("sqlite://{}?mode=rw", path.display());
    let pool = SqlitePool::connect(&url)
        .await
        .map_err(|e| RestoreError::Invalid(format!("ryokan.db could not be opened ({e})")))?;
    let verdict: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&pool)
        .await
        .map_err(|e| RestoreError::Invalid(format!("integrity check failed ({e})")))?;
    if verdict != "ok" {
        pool.close().await;
        return Err(RestoreError::Invalid(format!(
            "integrity check reported: {verdict}"
        )));
    }
    let has_config: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'config'",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0);
    if has_config == 0 {
        pool.close().await;
        return Err(RestoreError::Invalid(
            "ryokan.db has no config table, so it is not a Ryokan database".to_string(),
        ));
    }
    // Ryokan's schema has no views or triggers. A crafted database used
    // one to survive the restore: `sessions` as a view over a hidden
    // table with an INSTEAD OF trigger turned the purge below into a
    // no-op and kept a planted login valid even past RYOKAN_RESET_AUTH.
    let foreign_objects: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type IN ('view', 'trigger')")
            .fetch_one(&pool)
            .await
            .unwrap_or(1);
    if foreign_objects > 0 {
        pool.close().await;
        return Err(RestoreError::Invalid(
            "ryokan.db holds views or triggers, which a Ryokan database never has".to_string(),
        ));
    }
    if sanitized && let Err(e) = crate::services::sanitize::clear_placeholders(&pool).await {
        pool.close().await;
        return Err(RestoreError::Invalid(format!(
            "could not clear the sanitized backup's placeholders ({e})"
        )));
    }
    // Ignore a missing table: a very old backup predates sessions.
    let _ = sqlx::query("DELETE FROM sessions").execute(&pool).await;
    pool.close().await;
    Ok(())
}

fn version_tuple(v: &str) -> (u64, u64, u64) {
    let mut parts = v
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// The staged restore's manifest, when one is waiting for a restart.
pub fn pending_restore(paths: &BackupPaths) -> Option<BackupManifest> {
    read_manifest(&paths.pending_dir()).ok()
}

/// Remove a staged restore. `Ok(false)` when nothing was staged.
pub fn cancel_pending_restore(paths: &BackupPaths) -> Result<bool, String> {
    let pending = paths.pending_dir();
    if !pending.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&pending).map_err(|e| format!("remove {}: {e}", pending.display()))?;
    Ok(true)
}

#[derive(Clone, Debug)]
pub struct RestoreApplied {
    pub manifest: BackupManifest,
    /// Where the previous database went (`ryokan.db.pre-restore-<ts>`).
    pub previous_db: PathBuf,
    pub key_replaced: bool,
    pub artwork_replaced: bool,
    /// Steps after the database swap that did not go to plan. The
    /// restore is applied regardless; these are for the boot log.
    pub warnings: Vec<String>,
}

/// Boot-time half of restore. Runs before the pool opens. Validates the
/// staged directory again (the restart window is when it could have
/// been tampered with), then swaps in this order:
///
/// 1. the key (highest value, smallest file; nothing else has moved
///    yet if it fails, so the error is a clean "nothing applied"),
/// 2. the database, moving the live one aside as
///    `<db>.pre-restore-<ts>` with any `-wal` / `-shm` companions so a
///    leftover WAL can never be replayed onto the restored file; a
///    failure here puts the live database and the key back,
/// 3. the artwork cache, which can only produce a warning: from here
///    on the restore is applied and never rolled back.
///
/// `Err` therefore always means "nothing changed, the staged directory
/// is left for inspection".
pub fn apply_pending_restore(paths: &BackupPaths) -> Result<Option<RestoreApplied>, String> {
    let pending = paths.pending_dir();
    if !pending.is_dir() {
        return Ok(None);
    }
    let manifest = read_manifest(&pending).map_err(|e| e.to_string())?;
    let staged_db = pending.join("ryokan.db");
    check_database_file(&staged_db).map_err(|e| e.to_string())?;

    let suffix = format!(".pre-restore-{}", now_unix());

    // 1. Key.
    let staged_key = pending.join(".ryokan-key");
    let key_aside = suffixed(&paths.key_path, &suffix);
    let key_replaced = if staged_key.is_file() {
        if paths.key_path.is_file() {
            fs::rename(&paths.key_path, &key_aside)
                .map_err(|e| format!("move {} aside: {e}", paths.key_path.display()))?;
        }
        if let Some(parent) = paths.key_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = move_file(&staged_key, &paths.key_path) {
            if key_aside.exists() {
                let _ = fs::rename(&key_aside, &paths.key_path);
            }
            return Err(format!("place restored key: {e}"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&paths.key_path, fs::Permissions::from_mode(0o600));
        }
        true
    } else {
        false
    };
    let undo_key = || {
        if key_replaced {
            let _ = fs::rename(&paths.key_path, &staged_key);
            if key_aside.exists() {
                let _ = fs::rename(&key_aside, &paths.key_path);
            }
        }
    };

    // 2. Database.
    let previous_db = suffixed(&paths.db_path, &suffix);
    let mut moved_aside: Vec<(PathBuf, PathBuf)> = Vec::new();
    for companion in ["", "-wal", "-shm"] {
        let live = suffixed(&paths.db_path, companion);
        if live.exists() {
            let aside = suffixed(&previous_db, companion);
            if let Err(e) = fs::rename(&live, &aside) {
                for (live, aside) in moved_aside.iter().rev() {
                    let _ = fs::rename(aside, live);
                }
                undo_key();
                return Err(format!("move {} aside: {e}", live.display()));
            }
            moved_aside.push((live, aside));
        }
    }
    if let Some(parent) = paths.db_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = move_file(&staged_db, &paths.db_path) {
        for (live, aside) in moved_aside.iter().rev() {
            let _ = fs::rename(aside, live);
        }
        undo_key();
        return Err(format!("place restored database: {e}"));
    }

    // 3. Artwork, and the staging dir itself: warnings only.
    let mut warnings = Vec::new();
    let staged_art = pending.join("artwork");
    let mut artwork_replaced = false;
    if staged_art.is_dir() {
        let placed = (|| -> Result<(), String> {
            if paths.artwork_dir.is_dir() {
                fs::rename(&paths.artwork_dir, suffixed(&paths.artwork_dir, &suffix))
                    .map_err(|e| format!("move artwork aside: {e}"))?;
            }
            if let Some(parent) = paths.artwork_dir.parent() {
                let _ = fs::create_dir_all(parent);
            }
            move_tree(&staged_art, &paths.artwork_dir)
        })();
        match placed {
            Ok(()) => artwork_replaced = true,
            Err(e) => warnings.push(format!(
                "The artwork cache was not replaced ({e}). Missing artwork is fetched again on demand."
            )),
        }
    }
    if let Err(e) = fs::remove_dir_all(&pending) {
        warnings.push(format!(
            "The staged directory {} could not be removed ({e}). Remove it by hand; until then every boot reports the restore as missing its database.",
            pending.display()
        ));
    }
    Ok(Some(RestoreApplied {
        manifest,
        previous_db,
        key_replaced,
        artwork_replaced,
        warnings,
    }))
}

/// Remove whatever a crash left in the two work dirs. Their contents
/// are only ever meaningful during one backup or one upload, both of
/// which are over once the process restarts. Returns the dirs cleared.
pub fn sweep_work_dirs(paths: &BackupPaths) -> Vec<PathBuf> {
    let mut cleared = Vec::new();
    for name in [BACKUP_WORK_DIR_NAME, RESTORE_WORK_DIR_NAME] {
        let dir = paths.data_dir.join(name);
        let has_entries = fs::read_dir(&dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if has_entries && fs::remove_dir_all(&dir).is_ok() {
            cleared.push(dir);
        }
    }
    cleared
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Rename, falling back to copy + remove across filesystems.
fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    fs::copy(from, to).map_err(|e| format!("copy {} to {}: {e}", from.display(), to.display()))?;
    fs::remove_file(from).map_err(|e| format!("remove {}: {e}", from.display()))?;
    Ok(())
}

fn move_tree(from: &Path, to: &Path) -> Result<(), String> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(from).follow_links(false) {
        let entry = entry.map_err(|e| format!("walk {}: {e}", from.display()))?;
        let rel = entry.path().strip_prefix(from).map_err(|e| e.to_string())?;
        let dest = to.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            fs::copy(entry.path(), &dest)
                .map_err(|e| format!("copy {}: {e}", entry.path().display()))?;
        }
    }
    fs::remove_dir_all(from).map_err(|e| format!("remove {}: {e}", from.display()))?;
    Ok(())
}
