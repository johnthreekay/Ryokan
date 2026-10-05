//! Where Ryokan keeps its own state (issue #259).
//!
//! Everything Ryokan writes for itself (the SQLite database, the AEAD
//! key file, the artwork and anibridge caches, backups, and the
//! reset-auth sentinel) lives under one data directory:
//! `RYOKAN_DATA_DIR`, else the CWD-relative `data` that `cargo run`
//! has always used. The Docker image sets `RYOKAN_DATA_DIR=/data`, so
//! moving the state elsewhere (`/config`, freeing `/data` for a shared
//! media mount) is one variable plus the matching volume.
//!
//! The per-path variables (`DATABASE_URL`, `RYOKAN_KEY_FILE_PATH`,
//! `RYOKAN_MEDIA_CACHE_DIR`, `RYOKAN_ANIBRIDGE_CACHE_DIR`) still win
//! over the derived defaults, so an install that set them keeps its
//! layout. The resolvers below are pure over their inputs and read the
//! environment only in the thin wrappers, which keeps them testable
//! without touching process-wide env state.

use std::path::{Path, PathBuf};

/// Data directory when `RYOKAN_DATA_DIR` is unset: CWD-relative, the
/// repo's gitignored `data/` under `cargo run`.
pub const DATA_DIR_DEFAULT: &str = "data";
/// Database file name inside the data directory.
pub const DB_FILE_NAME: &str = "ryokan.db";
/// Password-recovery sentinel, next to the database (#22).
pub const RESET_AUTH_SENTINEL: &str = ".reset-auth";

/// A set, non-blank environment variable. Blank counts as unset so a
/// compose file's `FOO=` line behaves like no line at all.
pub fn env_nonblank(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// The data directory: `RYOKAN_DATA_DIR`, else [`DATA_DIR_DEFAULT`].
pub fn data_dir() -> PathBuf {
    resolve_data_dir(env_nonblank("RYOKAN_DATA_DIR"))
}

fn resolve_data_dir(raw: Option<String>) -> PathBuf {
    raw.map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DATA_DIR_DEFAULT))
}

/// `DATABASE_URL` when set. `None` means the database lives at
/// [`default_db_path`].
pub fn database_url() -> Option<String> {
    env_nonblank("DATABASE_URL")
}

/// `<data dir>/ryokan.db`, the database when `DATABASE_URL` is unset.
pub fn default_db_path() -> PathBuf {
    data_dir().join(DB_FILE_NAME)
}

/// The live database path: the file part of `DATABASE_URL` when it is
/// a plain `sqlite://<path>` URL (query string stripped), else
/// [`default_db_path`]. Shared by backup / restore and the
/// `--sanitize-db-for-debug` CLI.
pub fn live_db_path() -> PathBuf {
    resolve_live_db_path(database_url().as_deref(), &data_dir())
}

fn resolve_live_db_path(database_url: Option<&str>, data_dir: &Path) -> PathBuf {
    if let Some(url) = database_url {
        let without_scheme = url
            .strip_prefix("sqlite://")
            .or_else(|| url.strip_prefix("sqlite:"))
            .unwrap_or(url);
        let path_part = without_scheme.split('?').next().unwrap_or(without_scheme);
        if !path_part.is_empty() {
            return PathBuf::from(path_part);
        }
    }
    data_dir.join(DB_FILE_NAME)
}

/// The directory holding the live database. Backup work dirs, the
/// pending-restore dir, the default backup folder, and the reset-auth
/// sentinel live here, so they share the database's filesystem even
/// when `DATABASE_URL` points somewhere other than the data directory.
pub fn db_dir() -> PathBuf {
    parent_or(&live_db_path(), data_dir())
}

fn parent_or(path: &Path, fallback: PathBuf) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or(fallback)
}

/// `<db dir>/.reset-auth`.
pub fn reset_auth_sentinel() -> PathBuf {
    db_dir().join(RESET_AUTH_SENTINEL)
}

/// `var` as a path when set and non-blank, else `data_dir()/default_rel`.
/// The shape every per-path override shares.
pub fn override_or_data_path(var: &str, default_rel: &str) -> PathBuf {
    resolve_override(env_nonblank(var), &data_dir(), default_rel)
}

fn resolve_override(raw: Option<String>, data_dir: &Path, default_rel: &str) -> PathBuf {
    raw.map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join(default_rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_defaults_to_cwd_relative_data() {
        assert_eq!(resolve_data_dir(None), PathBuf::from("data"));
        assert_eq!(
            resolve_data_dir(Some("/config".into())),
            PathBuf::from("/config")
        );
    }

    #[test]
    fn live_db_path_follows_database_url_then_data_dir() {
        let config = Path::new("/config");
        assert_eq!(
            resolve_live_db_path(None, config),
            PathBuf::from("/config/ryokan.db")
        );
        assert_eq!(
            resolve_live_db_path(Some("sqlite:///data/ryokan.db?mode=rwc"), config),
            PathBuf::from("/data/ryokan.db"),
            "an explicit DATABASE_URL wins over the data dir"
        );
        assert_eq!(
            resolve_live_db_path(Some("sqlite://data/ryokan.db"), config),
            PathBuf::from("data/ryokan.db")
        );
        assert_eq!(
            resolve_live_db_path(Some("sqlite:/srv/r.db"), config),
            PathBuf::from("/srv/r.db")
        );
        assert_eq!(
            resolve_live_db_path(Some("sqlite://?mode=rwc"), config),
            PathBuf::from("/config/ryokan.db"),
            "a URL with no file part falls back to the data dir"
        );
    }

    #[test]
    fn db_dir_is_the_database_parent() {
        assert_eq!(
            parent_or(Path::new("/config/ryokan.db"), PathBuf::from("data")),
            PathBuf::from("/config")
        );
        assert_eq!(
            parent_or(Path::new("ryokan.db"), PathBuf::from("data")),
            PathBuf::from("data"),
            "a bare file name has an empty parent; fall back"
        );
    }

    #[test]
    fn overrides_win_over_the_data_dir() {
        let config = Path::new("/config");
        assert_eq!(
            resolve_override(None, config, "cache/artwork"),
            PathBuf::from("/config/cache/artwork")
        );
        assert_eq!(
            resolve_override(Some("/data/cache/artwork".into()), config, "cache/artwork"),
            PathBuf::from("/data/cache/artwork")
        );
    }
}

/// Whether opening the database creates a missing file: always for the
/// default path (`main.rs` sets `create_if_missing`), and for an
/// explicit `DATABASE_URL` only when it asks with `mode=rwc`, as sqlx
/// reads it. Without that, a missing database (an unmounted volume, a
/// typo) fails the boot instead of starting empty with `/setup` open.
pub fn opening_creates_db(database_url: Option<&str>) -> bool {
    let Some(url) = database_url else {
        return true;
    };
    url.split_once('?')
        .is_some_and(|(_, query)| query.split('&').any(|pair| pair == "mode=rwc"))
}

/// Keep the database and its `-wal` / `-shm` files private to Ryokan's
/// user (0600). SQLite created them with the umask's mode (0644 on most
/// systems), and they hold session hashes, download-client passwords,
/// indexer and *arr keys: anyone else on a shared host could read them.
/// When opening will create the database anyway (`create`, see
/// [`opening_creates_db`]), a missing one is created 0600 here first so
/// SQLite inherits the mode for its WAL files; existing ones are
/// tightened either way. Best effort: a failure is logged and boot
/// continues.
#[cfg(unix)]
pub fn make_db_private(db_path: &Path, create: bool) {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if create
        && !db_path.exists()
        && let Err(e) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(db_path)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        tracing::warn!("Couldn't pre-create {} as 0600: {e}", db_path.display());
    }
    let mut paths = vec![db_path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        paths.push(PathBuf::from(name));
    }
    for path in paths {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.permissions().mode() & 0o077 != 0
            && let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!("Couldn't make {} private: {e}", path.display());
        }
    }
}

#[cfg(not(unix))]
pub fn make_db_private(_db_path: &Path, _create: bool) {}

#[cfg(all(test, unix))]
mod db_private_tests {
    use super::{make_db_private, opening_creates_db};
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_database_and_its_wal_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ryokan.db");
        // A fresh install: created 0600 before SQLite opens it.
        make_db_private(&db, true);
        assert_eq!(mode(&db), 0o600);
        // An existing install: 0644 files are tightened.
        let wal = tmp.path().join("ryokan.db-wal");
        std::fs::write(&wal, b"").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o644)).unwrap();
        make_db_private(&db, false);
        assert_eq!(mode(&db), 0o600);
        assert_eq!(mode(&wal), 0o600);
    }

    #[test]
    fn a_database_url_that_does_not_create_gets_no_file() {
        // Pre-creating it booted an explicit `DATABASE_URL` on a fresh
        // empty database (with `/setup` open) where sqlx would have
        // refused a missing file.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ryokan.db");
        make_db_private(&db, false);
        assert!(!db.exists());

        assert!(opening_creates_db(None), "the default path is created");
        assert!(opening_creates_db(Some(
            "sqlite:///data/ryokan.db?mode=rwc"
        )));
        assert!(opening_creates_db(Some(
            "sqlite:///data/ryokan.db?cache=shared&mode=rwc"
        )));
        for url in [
            "sqlite:///data/ryokan.db",
            "sqlite:///data/ryokan.db?mode=rw",
            "sqlite::memory:",
            "sqlite://?mode=memory",
        ] {
            assert!(!opening_creates_db(Some(url)), "{url}");
        }
    }
}
