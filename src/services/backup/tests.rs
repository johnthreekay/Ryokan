use super::*;
use std::collections::HashMap;
use std::io::Read;

/// A data dir laid out like a real install: a key file and an artwork
/// dir; the database is created by [`pool_at`].
fn temp_paths(tag: &str) -> BackupPaths {
    let root = std::env::temp_dir().join(format!("ryokan-backup-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let data = root.join("data");
    fs::create_dir_all(data.join("cache/artwork/blobs")).unwrap();
    fs::write(data.join(".ryokan-key"), [7u8; 32]).unwrap();
    fs::write(data.join("cache/artwork/blobs/aa.jpg"), b"jpeg bytes").unwrap();
    BackupPaths {
        data_dir: data.clone(),
        db_path: data.join("ryokan.db"),
        key_path: data.join(".ryokan-key"),
        artwork_dir: data.join("cache/artwork"),
    }
}

/// A migrated, file-backed pool at `paths.db_path`. File-backed on
/// purpose: `VACUUM INTO` writes nothing for sqlx's shared in-memory
/// database, and production is a file anyway.
async fn pool_at(paths: &BackupPaths) -> SqlitePool {
    let pool = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", paths.db_path.display()))
        .await
        .expect("open file db");
    crate::models::migrate(&pool).await.expect("migrate");
    pool
}

fn cleanup(paths: &BackupPaths) {
    if let Some(root) = paths.data_dir.parent() {
        let _ = fs::remove_dir_all(root);
    }
}

/// Every entry of a `.tar.gz` as `path → bytes`.
fn read_archive(path: &Path) -> HashMap<String, Vec<u8>> {
    let file = fs::File::open(path).expect("open archive");
    let mut tar = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
    let mut out = HashMap::new();
    for entry in tar.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        out.insert(name, bytes);
    }
    out
}

async fn count(db: &SqlitePool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
}

async fn open_file_db(path: &Path) -> SqlitePool {
    SqlitePool::connect(&format!("sqlite://{}?mode=rw", path.display()))
        .await
        .expect("open extracted db")
}

#[tokio::test]
async fn backup_holds_manifest_db_key_and_recent_writes() {
    let paths = temp_paths("full");
    let db = pool_at(&paths).await;
    crate::test_support::seed_series(&db, 1, "Frieren").await;

    let out = paths.data_dir.join("out.tar.gz");
    let manifest = create_backup(&db, &paths, BackupOptions::default(), &out)
        .await
        .expect("backup");
    assert!(manifest.includes_key);
    assert!(!manifest.includes_artwork);
    assert!(!manifest.sanitized);
    assert_eq!(manifest.ryokan_version, env!("CARGO_PKG_VERSION"));
    assert!(!fs::exists(paths.data_dir.join(BACKUP_WORK_DIR_NAME).join("x")).unwrap());

    let entries = read_archive(&out);
    let mut names: Vec<&String> = entries.keys().collect();
    names.sort();
    assert_eq!(names, vec![".ryokan-key", "manifest.json", "ryokan.db"]);
    assert_eq!(entries[".ryokan-key"], vec![7u8; 32]);
    let parsed: BackupManifest = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(parsed, manifest);

    // The snapshot is a real database carrying the row written just
    // before the backup: VACUUM INTO, not a stale file copy.
    let extracted = paths.data_dir.join("extracted.db");
    fs::write(&extracted, &entries["ryokan.db"]).unwrap();
    let copy = open_file_db(&extracted).await;
    assert_eq!(count(&copy, "SELECT COUNT(*) FROM series").await, 1);
    copy.close().await;

    // Work dir is gone once the archive is written.
    assert!(
        !paths.data_dir.join(BACKUP_WORK_DIR_NAME).exists()
            || fs::read_dir(paths.data_dir.join(BACKUP_WORK_DIR_NAME))
                .unwrap()
                .next()
                .is_none()
    );
    cleanup(&paths);
}

#[tokio::test]
async fn artwork_is_included_only_on_request() {
    let paths = temp_paths("artwork");
    let db = pool_at(&paths).await;
    let out = paths.data_dir.join("out.tar.gz");
    let manifest = create_backup(
        &db,
        &paths,
        BackupOptions {
            include_artwork: true,
            sanitize: false,
        },
        &out,
    )
    .await
    .expect("backup");
    assert!(manifest.includes_artwork);
    assert_eq!(manifest.artwork_size_bytes, "jpeg bytes".len() as u64);
    let entries = read_archive(&out);
    assert_eq!(entries["artwork/blobs/aa.jpg"], b"jpeg bytes");
    cleanup(&paths);
}

#[tokio::test]
async fn sanitized_backup_omits_key_and_hostname_and_trims_logs() {
    let paths = temp_paths("sanitized");
    let db = pool_at(&paths).await;
    for i in 0..(SANITIZED_LOG_ROWS + 50) {
        sqlx::query("INSERT INTO logs (level, category, message) VALUES ('info', 'system', ?)")
            .bind(format!("row {i}"))
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO config (id, qbit_pass) VALUES (1, 'hunter2') ON CONFLICT(id) DO UPDATE SET qbit_pass = 'hunter2'")
        .execute(&db)
        .await
        .unwrap();

    let out = paths.data_dir.join("support.tar.gz");
    let manifest = create_backup(
        &db,
        &paths,
        BackupOptions {
            include_artwork: false,
            sanitize: true,
        },
        &out,
    )
    .await
    .expect("backup");
    assert!(manifest.sanitized);
    assert!(!manifest.includes_key);
    assert!(manifest.hostname.is_none());

    let entries = read_archive(&out);
    assert!(!entries.contains_key(".ryokan-key"));
    let extracted = paths.data_dir.join("support.db");
    fs::write(&extracted, &entries["ryokan.db"]).unwrap();
    let copy = open_file_db(&extracted).await;
    assert_eq!(
        count(&copy, "SELECT COUNT(*) FROM logs").await,
        SANITIZED_LOG_ROWS
    );
    let pass: String = sqlx::query_scalar("SELECT qbit_pass FROM config WHERE id = 1")
        .fetch_one(&copy)
        .await
        .unwrap();
    assert_eq!(pass, "[REDACTED]");
    copy.close().await;
    cleanup(&paths);
}

#[tokio::test]
async fn busy_lock_refuses_a_second_backup() {
    let paths = temp_paths("busy");
    let db = pool_at(&paths).await;
    let _held = BACKUP_LOCK.lock().await;
    let err = create_backup(
        &db,
        &paths,
        BackupOptions::default(),
        &paths.data_dir.join("x.tar.gz"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BackupError::Busy));
    cleanup(&paths);
}

#[test]
fn backup_names_round_trip_and_strangers_are_rejected() {
    let name = backup_file_name(BackupKind::Scheduled, 1_700_000_000);
    assert_eq!(name, "ryokan-backup-1700000000.tar.gz");
    assert_eq!(
        parse_backup_name(&name),
        Some((BackupKind::Scheduled, 1_700_000_000))
    );
    assert_eq!(
        parse_backup_name("auto-pre-restore-5.tar.gz"),
        Some((BackupKind::PreRestore, 5))
    );
    for bad in [
        "ryokan-backup-.tar.gz",
        "ryokan-backup-12.tar",
        "../ryokan-backup-12.tar.gz",
        "ryokan-backup-12.tar.gz.partial",
        "notes.txt",
    ] {
        assert_eq!(parse_backup_name(bad), None, "{bad}");
    }
}

#[test]
fn prune_keeps_the_newest_and_spares_pre_restore_backups() {
    let paths = temp_paths("prune");
    let dir = paths.data_dir.join("backups");
    fs::create_dir_all(&dir).unwrap();
    for ts in [10, 20, 30, 40] {
        fs::write(dir.join(backup_file_name(BackupKind::Scheduled, ts)), b"x").unwrap();
    }
    fs::write(dir.join(backup_file_name(BackupKind::PreRestore, 5)), b"x").unwrap();
    fs::write(dir.join("unrelated.txt"), b"x").unwrap();

    let removed = prune_backups(&dir, 2).unwrap();
    assert_eq!(
        removed,
        vec![
            "ryokan-backup-20.tar.gz".to_string(),
            "ryokan-backup-10.tar.gz".to_string()
        ]
    );
    let names: Vec<String> = list_backups(&dir).into_iter().map(|b| b.name).collect();
    assert_eq!(
        names,
        vec![
            "ryokan-backup-40.tar.gz",
            "ryokan-backup-30.tar.gz",
            "auto-pre-restore-5.tar.gz"
        ]
    );
    assert_eq!(newest_backup_timestamp(&dir), Some(40));
    assert!(dir.join("unrelated.txt").exists());
    cleanup(&paths);
}

#[tokio::test]
async fn stage_restore_validates_stages_and_locks_until_cancelled() {
    let paths = temp_paths("stage");
    let db = pool_at(&paths).await;
    crate::test_support::seed_series(&db, 1, "Frieren").await;
    sqlx::query("INSERT INTO users (username, password_hash) VALUES ('u', 'h')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (token, user_id) VALUES ('live-session', 1)")
        .execute(&db)
        .await
        .unwrap();
    let upload = paths.data_dir.join("upload.tar.gz");
    create_backup(&db, &paths, BackupOptions::default(), &upload)
        .await
        .unwrap();
    let backup_dir = paths.data_dir.join("backups");

    let staged = stage_restore(&db, &paths, &backup_dir, &upload)
        .await
        .expect("stage");
    assert!(staged.warnings.is_empty(), "{:?}", staged.warnings);
    assert!(!upload.exists(), "the upload is consumed");
    let pending = paths.pending_dir();
    assert!(pending.join("manifest.json").is_file());
    assert!(pending.join(".ryokan-key").is_file());
    assert!(
        backup_dir.join(&staged.pre_restore_backup).is_file(),
        "pre-restore backup {} exists",
        staged.pre_restore_backup
    );
    let staged_db = open_file_db(&pending.join("ryokan.db")).await;
    assert_eq!(count(&staged_db, "SELECT COUNT(*) FROM sessions").await, 0);
    assert_eq!(count(&staged_db, "SELECT COUNT(*) FROM series").await, 1);
    staged_db.close().await;
    assert_eq!(
        pending_restore(&paths).map(|m| m.backup_timestamp),
        Some(staged.manifest.backup_timestamp)
    );

    // A second upload is refused while one is staged.
    let again = paths.data_dir.join("again.tar.gz");
    fs::write(&again, b"whatever").unwrap();
    assert!(matches!(
        stage_restore(&db, &paths, &backup_dir, &again).await,
        Err(RestoreError::Pending)
    ));

    assert!(cancel_pending_restore(&paths).unwrap());
    assert!(!pending.exists());
    assert!(!cancel_pending_restore(&paths).unwrap());
    cleanup(&paths);
}

#[tokio::test]
async fn stage_restore_rejects_non_backups_and_newer_schemas() {
    let paths = temp_paths("reject");
    let db = pool_at(&paths).await;
    let backup_dir = paths.data_dir.join("backups");

    let junk = paths.data_dir.join("junk.tar.gz");
    fs::write(&junk, b"not even gzip").unwrap();
    assert!(matches!(
        stage_restore(&db, &paths, &backup_dir, &junk).await,
        Err(RestoreError::Invalid(_))
    ));

    // A well-formed archive from a newer schema.
    let mut manifest = BackupManifest {
        ryokan_version: env!("CARGO_PKG_VERSION").to_string(),
        backup_timestamp: 1,
        max_migration_id: max_migration_id(&db).await + 1000,
        includes_artwork: false,
        includes_key: false,
        sanitized: false,
        hostname: None,
        db_size_bytes: 0,
        artwork_size_bytes: 0,
    };
    let snapshot = paths.data_dir.join("snap.db");
    vacuum_into(&db, &snapshot).await.unwrap();
    let newer = paths.data_dir.join("newer.tar.gz");
    write_archive(&newer, &manifest, &snapshot, None, None).unwrap();
    let err = stage_restore(&db, &paths, &backup_dir, &newer)
        .await
        .unwrap_err();
    assert!(matches!(err, RestoreError::Incompatible(_)), "{err}");
    assert!(!paths.pending_dir().exists());

    // Same schema, newer version string.
    manifest.max_migration_id = 0;
    manifest.ryokan_version = "99.0.0".to_string();
    let newer_version = paths.data_dir.join("newer-version.tar.gz");
    write_archive(&newer_version, &manifest, &snapshot, None, None).unwrap();
    assert!(matches!(
        stage_restore(&db, &paths, &backup_dir, &newer_version).await,
        Err(RestoreError::Incompatible(_))
    ));

    // An archive smuggling an extra file is refused whole.
    let extra = paths.data_dir.join("extra.tar.gz");
    {
        let file = fs::File::create(&extra).unwrap();
        let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        tar.append_path_with_name(&snapshot, "ryokan.db").unwrap();
        tar.append_path_with_name(&snapshot, "evil.sh").unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    let err = stage_restore(&db, &paths, &backup_dir, &extra)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("unexpected entry 'evil.sh'"),
        "{err}"
    );

    // A key-less backup stages with a warning.
    manifest.ryokan_version = env!("CARGO_PKG_VERSION").to_string();
    let keyless = paths.data_dir.join("keyless.tar.gz");
    write_archive(&keyless, &manifest, &snapshot, None, None).unwrap();
    let staged = stage_restore(&db, &paths, &backup_dir, &keyless)
        .await
        .unwrap();
    assert!(
        staged
            .warnings
            .iter()
            .any(|w| w.contains("no encryption key"))
    );
    cleanup(&paths);
}

#[test]
fn apply_pending_restore_swaps_files_and_keeps_the_previous_database() {
    let paths = temp_paths("apply");
    fs::write(&paths.db_path, b"OLD DATABASE").unwrap();
    fs::write(suffixed(&paths.db_path, "-wal"), b"old wal").unwrap();
    let pending = paths.pending_dir();
    fs::create_dir_all(pending.join("artwork/blobs")).unwrap();
    let manifest = BackupManifest {
        ryokan_version: "1.0.0".to_string(),
        backup_timestamp: 42,
        max_migration_id: 1,
        includes_artwork: true,
        includes_key: true,
        sanitized: false,
        hostname: None,
        db_size_bytes: 0,
        artwork_size_bytes: 0,
    };
    fs::write(
        pending.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut staged_db = DB_MAGIC.to_vec();
    staged_db.extend_from_slice(b"NEW DATABASE");
    fs::write(pending.join("ryokan.db"), &staged_db).unwrap();
    fs::write(pending.join(".ryokan-key"), [9u8; 32]).unwrap();
    fs::write(pending.join("artwork/blobs/bb.jpg"), b"new art").unwrap();

    let applied = apply_pending_restore(&paths)
        .expect("apply")
        .expect("something was pending");
    assert_eq!(applied.manifest, manifest);
    assert!(applied.key_replaced);
    assert!(applied.artwork_replaced);
    assert!(applied.warnings.is_empty(), "{:?}", applied.warnings);
    let kept_keys: Vec<_> = fs::read_dir(&paths.data_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".ryokan-key.pre-restore-"))
        .collect();
    assert_eq!(
        kept_keys.len(),
        1,
        "the previous key is kept aside: {kept_keys:?}"
    );
    assert_eq!(
        fs::read(paths.data_dir.join(&kept_keys[0])).unwrap(),
        vec![7u8; 32]
    );
    assert_eq!(fs::read(&paths.db_path).unwrap(), staged_db);
    assert_eq!(fs::read(&applied.previous_db).unwrap(), b"OLD DATABASE");
    assert_eq!(
        fs::read(suffixed(&applied.previous_db, "-wal")).unwrap(),
        b"old wal",
        "the old WAL moves with the old database"
    );
    assert!(!suffixed(&paths.db_path, "-wal").exists());
    assert_eq!(fs::read(&paths.key_path).unwrap(), vec![9u8; 32]);
    assert!(paths.artwork_dir.join("blobs/bb.jpg").is_file());
    assert!(!paths.artwork_dir.join("blobs/aa.jpg").exists());
    assert!(!pending.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&paths.key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    // Nothing pending → nothing happens.
    assert!(apply_pending_restore(&paths).unwrap().is_none());
    cleanup(&paths);
}

#[test]
fn apply_pending_restore_leaves_the_live_database_alone_on_a_bad_staging_dir() {
    let paths = temp_paths("apply-bad");
    fs::write(&paths.db_path, b"OLD DATABASE").unwrap();
    let pending = paths.pending_dir();
    fs::create_dir_all(&pending).unwrap();
    fs::write(pending.join("ryokan.db"), b"not sqlite").unwrap();
    // No manifest at all.
    let err = apply_pending_restore(&paths).unwrap_err();
    assert!(err.contains("manifest.json is missing"), "{err}");
    assert_eq!(fs::read(&paths.db_path).unwrap(), b"OLD DATABASE");
    assert!(pending.exists(), "left for inspection");
    cleanup(&paths);
}

#[test]
fn version_tuples_compare_numerically() {
    assert!(version_tuple("1.10.0") > version_tuple("1.9.0"));
    assert!(version_tuple("v2.0.0-rc1") > version_tuple("1.99.99"));
    assert_eq!(version_tuple("garbage"), (0, 0, 0));
}

#[test]
fn sweep_work_dirs_clears_stranded_temp_files_only() {
    let paths = temp_paths("sweep");
    let work = paths.data_dir.join(BACKUP_WORK_DIR_NAME).join("abc");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("ryokan.db"), b"half written").unwrap();
    fs::create_dir_all(paths.data_dir.join(RESTORE_WORK_DIR_NAME)).unwrap();
    let cleared = sweep_work_dirs(&paths);
    assert_eq!(cleared, vec![paths.data_dir.join(BACKUP_WORK_DIR_NAME)]);
    assert!(!paths.data_dir.join(BACKUP_WORK_DIR_NAME).exists());
    assert!(
        paths.data_dir.join(RESTORE_WORK_DIR_NAME).exists(),
        "an empty work dir is left alone"
    );
    assert!(
        paths.key_path.is_file(),
        "nothing outside the work dirs is touched"
    );
    cleanup(&paths);
}

#[tokio::test]
async fn backups_are_owner_only_and_carry_no_live_sessions() {
    let paths = temp_paths("private");
    let db = pool_at(&paths).await;
    crate::models::user::create_user(&db, "admin", "pw-pw-pw-pw")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sessions (token, user_id) VALUES ('live-session-token-0123456789', 1)",
    )
    .execute(&db)
    .await
    .unwrap();

    let out = paths.data_dir.join("out.tar.gz");
    create_backup(&db, &paths, BackupOptions::default(), &out)
        .await
        .expect("backup");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the archive holds the key; owner only");
    }
    let entries = read_archive(&out);
    let raw = String::from_utf8_lossy(&entries["ryokan.db"]).to_string();
    assert!(
        !raw.contains("live-session-token"),
        "no session token in the snapshot bytes"
    );
    let extracted = paths.data_dir.join("extracted.db");
    fs::write(&extracted, &entries["ryokan.db"]).unwrap();
    let copy = open_file_db(&extracted).await;
    assert_eq!(count(&copy, "SELECT COUNT(*) FROM sessions").await, 0);
    assert_eq!(
        count(&copy, "SELECT COUNT(*) FROM users").await,
        1,
        "the rest is a full backup"
    );
    copy.close().await;
    cleanup(&paths);
}

#[tokio::test]
async fn stage_restore_refuses_links_bad_keys_and_foreign_schema_objects() {
    let paths = temp_paths("distrust");
    let db = pool_at(&paths).await;
    let backup_dir = paths.data_dir.join("backups");
    let manifest = BackupManifest {
        ryokan_version: env!("CARGO_PKG_VERSION").to_string(),
        backup_timestamp: 1,
        max_migration_id: 0,
        includes_artwork: false,
        includes_key: false,
        sanitized: false,
        hostname: None,
        db_size_bytes: 0,
        artwork_size_bytes: 0,
    };
    let invalid = |r: Result<StagedRestore, RestoreError>, needle: &str| match r {
        Err(RestoreError::Invalid(msg)) => assert!(msg.contains(needle), "{msg}"),
        Err(other) => panic!("expected Invalid({needle}), got {other}"),
        Ok(_) => panic!("expected Invalid({needle}), got a staged restore"),
    };

    // 1. `ryokan.db` as a symlink (to the live database, say).
    let linked = paths.data_dir.join("linked.tar.gz");
    {
        let file = fs::File::create(&linked).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let body = serde_json::to_vec(&manifest).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "manifest.json", &body[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        tar.append_link(&mut link, "ryokan.db", "/data/ryokan.db")
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    invalid(
        stage_restore(&db, &paths, &backup_dir, &linked).await,
        "link",
    );

    // 2. A key that isn't 32 bytes: it would stage, then fail every boot.
    let snapshot = paths.data_dir.join("snap.db");
    vacuum_into(&db, &snapshot).await.unwrap();
    let short_key = paths.data_dir.join("short.key");
    fs::write(&short_key, [7u8; 16]).unwrap();
    let bad_key = paths.data_dir.join("bad-key.tar.gz");
    write_archive(&bad_key, &manifest, &snapshot, Some(&short_key), None).unwrap();
    invalid(
        stage_restore(&db, &paths, &backup_dir, &bad_key).await,
        "32 bytes",
    );

    // 3. A view or trigger, the shape that survived the session purge.
    let crafted = paths.data_dir.join("crafted.db");
    vacuum_into(&db, &crafted).await.unwrap();
    {
        let copy = open_file_db(&crafted).await;
        sqlx::query("CREATE VIEW planted AS SELECT 1")
            .execute(&copy)
            .await
            .unwrap();
        copy.close().await;
    }
    let with_view = paths.data_dir.join("view.tar.gz");
    write_archive(&with_view, &manifest, &crafted, None, None).unwrap();
    invalid(
        stage_restore(&db, &paths, &backup_dir, &with_view).await,
        "views or triggers",
    );

    assert!(!paths.pending_dir().exists(), "nothing was staged");
    cleanup(&paths);
}

/// A PAX `x` record giving the next entry's `size`. The length prefix
/// counts its own digits.
fn pax_size_record(size: u64) -> Vec<u8> {
    let body = format!(" size={size}\n");
    let mut len = body.len() + 1;
    loop {
        let record = format!("{len}{body}");
        if record.len() == len {
            return record.into_bytes();
        }
        len = record.len();
    }
}

/// An archive of one regular file `name` whose ustar header says 0 bytes
/// while a PAX record says `pax_size`, followed by `data`.
fn archive_with_pax_size(out: &Path, name: &str, pax_size: u64, data: &[u8]) {
    let file = fs::File::create(out).unwrap();
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    let record = pax_size_record(pax_size);
    let mut pax = tar::Header::new_ustar();
    pax.set_entry_type(tar::EntryType::XHeader);
    pax.set_path(format!("PaxHeaders/{name}")).unwrap();
    pax.set_size(record.len() as u64);
    pax.set_mode(0o644);
    pax.set_cksum();
    tar.append(&pax, record.as_slice()).unwrap();
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_path(name).unwrap();
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();
    // `append` writes whatever the reader yields, whatever the header says.
    tar.append(&header, data).unwrap();
    tar.into_inner().unwrap().finish().unwrap();
}

#[test]
fn extraction_caps_read_the_pax_size_not_the_header() {
    let paths = temp_paths("pax-size");
    let into = paths.data_dir.join("into");
    fs::create_dir_all(&into).unwrap();
    let invalid = |r: Result<(), RestoreError>, needle: &str| match r {
        Err(RestoreError::Invalid(msg)) => assert!(msg.contains(needle), "{msg}"),
        Err(other) => panic!("expected Invalid({needle}), got {other}"),
        Ok(()) => panic!("expected Invalid({needle}), got an extracted archive"),
    };

    // A 2 MiB manifest behind a ustar size of 0 passed the 1 MiB cap and
    // was unpacked whole.
    let mut manifest = b"{}".to_vec();
    manifest.resize(2 << 20, b' ');
    let big_manifest = paths.data_dir.join("big-manifest.tar.gz");
    archive_with_pax_size(
        &big_manifest,
        "manifest.json",
        manifest.len() as u64,
        &manifest,
    );
    invalid(
        extract_archive(&big_manifest, &into),
        "'manifest.json' is larger than a Ryokan backup's",
    );
    assert!(!into.join("manifest.json").exists(), "nothing was unpacked");

    // The total cap reads the same size, before any data is read.
    let huge = paths.data_dir.join("huge.tar.gz");
    archive_with_pax_size(&huge, "artwork/blob.jpg", RESTORE_MAX_TOTAL_BYTES + 1, b"");
    invalid(
        extract_archive(&huge, &into),
        "larger than any Ryokan backup",
    );
    cleanup(&paths);
}

#[cfg(unix)]
#[tokio::test]
async fn snapshots_and_staged_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let paths = temp_paths("modes");
    let db = pool_at(&paths).await;
    let backup_dir = paths.data_dir.join("backups");

    // The snapshot, and so the archive's header for it.
    let snapshot = paths.data_dir.join("snap.db");
    vacuum_into(&db, &snapshot).await.unwrap();
    assert_eq!(mode(&snapshot), 0o600);
    let out = paths.data_dir.join("out.tar.gz");
    create_backup(&db, &paths, BackupOptions::default(), &out)
        .await
        .unwrap();
    let file = fs::File::open(&out).unwrap();
    let mut tar = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
    let db_mode = tar
        .entries()
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.path().unwrap().as_ref() == Path::new("ryokan.db"))
        .map(|e| e.header().mode().unwrap() & 0o777);
    assert_eq!(db_mode, Some(0o600));

    // An archive whose headers say 0644 (one made before the snapshot
    // was created owner-only) still stages owner-only.
    let manifest = BackupManifest {
        ryokan_version: env!("CARGO_PKG_VERSION").to_string(),
        backup_timestamp: 1,
        max_migration_id: 0,
        includes_artwork: false,
        includes_key: true,
        sanitized: false,
        hostname: None,
        db_size_bytes: 0,
        artwork_size_bytes: 0,
    };
    let key = paths.data_dir.join("legacy.key");
    fs::write(&key, [7u8; 32]).unwrap();
    for file in [&snapshot, &key] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let legacy = paths.data_dir.join("legacy.tar.gz");
    write_archive(&legacy, &manifest, &snapshot, Some(&key), None).unwrap();
    stage_restore(&db, &paths, &backup_dir, &legacy)
        .await
        .expect("stage");
    let pending = paths.pending_dir();
    assert_eq!(mode(&pending.join("ryokan.db")), 0o600);
    assert_eq!(mode(&pending.join(".ryokan-key")), 0o600);
    cleanup(&paths);
}
