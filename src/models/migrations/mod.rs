//! Database schema migrations.
//!
//! All migrations are idempotent SQL in code (CREATE TABLE IF NOT EXISTS,
//! ALTER TABLE … ADD COLUMN with `.ok()` to silently absorb already-
//! applied columns). There are no migration files on disk — that's
//! deliberate per CLAUDE.md's "idempotent SQL in code, no on-disk
//! migration files" convention. One-shot migrations that can't self-
//! guard (data rewrites) live next to their model module and register
//! themselves in the `schema_migrations` table.

use sqlx::{Row, SqlitePool};

use super::{custom_formats, group_source_map, media_probe_cache, nyaa_description_cache};

/// Check whether `column` exists on `table`. Used inside [`migrate`] to
/// gate idempotent ALTER chains whose "ADD COLUMN then RENAME" shape
/// would otherwise leave vestigial columns on fresh installs (the ADD
/// succeeds unconditionally because `.ok()` swallows the
/// already-migrated case, then the RENAME silently no-ops when the
/// target already exists). By asking SQLite directly, we can skip the
/// ADD step entirely on installs where the current column name already
/// exists.
async fn column_exists(db: &SqlitePool, table: &str, column: &str) -> bool {
    // PRAGMA doesn't accept bound parameters, but `table` is a hardcoded
    // string literal from our own migration code — no user input — so
    // inline interpolation is safe.
    let sql = format!("PRAGMA table_info({})", table);
    let Ok(rows) = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(db).await else {
        return false;
    };
    rows.iter()
        .any(|r| r.try_get::<String, _>("name").ok().as_deref() == Some(column))
}

/// Recover any of the four possible states a column-rename migration
/// can leave a user's DB in when the first attempt is broken.
///
/// State matrix:
///
/// | legacy | new | action                                             |
/// |--------|-----|----------------------------------------------------|
/// |   ✓    | ✓   | copy legacy→new (only when new still the default), drop legacy |
/// |   ✓    | ✗   | rename legacy→new                                  |
/// |   ✗    | ✓   | no-op                                              |
/// |   ✗    | ✗   | add new (caller-supplied declaration)              |
///
/// The "both columns exist" row is the one PR #37's first migration
/// attempt produced: it ran ADD-then-RENAME, so ADD succeeded, RENAME
/// hit "duplicate column" → `.ok()` → data stranded in the legacy
/// column alongside an empty new column.
///
/// `legacy` / `new` / `add_decl` / `default_predicate` are all
/// hardcoded literals from the callers in `migrate()`, so inline
/// interpolation into the SQL is safe (no user input reaches PRAGMA
/// or ALTER TABLE here).
///
/// `add_decl` is the column declaration used when neither column
/// exists (fresh install) — e.g. `"TEXT NOT NULL DEFAULT ''"` for
/// string renames, `"INTEGER NOT NULL DEFAULT 0"` for boolean flag
/// renames. `default_predicate` is the WHERE-clause fragment that
/// identifies "new column still has its default value" so the
/// recovery copy doesn't clobber a value the user / a later
/// migration pass legitimately wrote — `"= ''"` for strings,
/// `"= 0"` for integer flags.
async fn reconcile_column_rename_typed(
    db: &SqlitePool,
    table: &str,
    legacy: &str,
    new: &str,
    add_decl: &str,
    default_predicate: &str,
) {
    let legacy_exists = column_exists(db, table, legacy).await;
    let new_exists = column_exists(db, table, new).await;

    match (legacy_exists, new_exists) {
        (true, true) => {
            // Recovery path for the PR #37 half-migrated state.
            // Copy legacy→new where new is still the default. Guard
            // on the type-appropriate predicate so a later pass that
            // legitimately set `new` via UPDATE isn't overwritten
            // from the stale legacy value.
            let copy = format!(
                "UPDATE {table} SET {new} = {legacy} WHERE {new} {default_predicate} AND {legacy} IS NOT NULL"
            );
            let _ = sqlx::query(sqlx::AssertSqlSafe(copy)).execute(db).await;

            // SQLite ≥ 3.35 supports DROP COLUMN. Silently absorb
            // if it fails — in that case the legacy column stays,
            // duplicating data, but the new column has the live
            // value and that's the one the app reads.
            let drop = format!("ALTER TABLE {table} DROP COLUMN {legacy}");
            let _ = sqlx::query(sqlx::AssertSqlSafe(drop)).execute(db).await;
        }
        (true, false) => {
            // Clean pre-PR-#37 DB with only the legacy name.
            let rename = format!("ALTER TABLE {table} RENAME COLUMN {legacy} TO {new}");
            let _ = sqlx::query(sqlx::AssertSqlSafe(rename)).execute(db).await;
        }
        (false, true) => {
            // Already migrated, nothing to do.
        }
        (false, false) => {
            // Fresh install — ADD with caller-supplied declaration.
            let add = format!("ALTER TABLE {table} ADD COLUMN {new} {add_decl}");
            let _ = sqlx::query(sqlx::AssertSqlSafe(add)).execute(db).await;
        }
    }
}

/// String-column rename — backwards-compatible wrapper for the
/// pre-existing call sites that all carried the `TEXT NOT NULL DEFAULT
/// ''` shape.
async fn reconcile_column_rename(db: &SqlitePool, table: &str, legacy: &str, new: &str) {
    reconcile_column_rename_typed(db, table, legacy, new, "TEXT NOT NULL DEFAULT ''", "= ''").await
}

/// Run all database migrations.
pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            token TEXT PRIMARY KEY,
            user_id INTEGER NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (user_id) REFERENCES users(id)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            qbit_url TEXT NOT NULL DEFAULT '',
            qbit_user TEXT NOT NULL DEFAULT '',
            qbit_pass TEXT NOT NULL DEFAULT '',
            qbit_category TEXT NOT NULL DEFAULT '',
            jellyfin_host TEXT NOT NULL DEFAULT '',
            jellyfin_port TEXT NOT NULL DEFAULT '',
            jellyfin_api_key TEXT NOT NULL DEFAULT '',
            jellyfin_use_ssl INTEGER NOT NULL DEFAULT 0,
            jellyfin_url TEXT NOT NULL DEFAULT '',
            preferred_groups TEXT NOT NULL DEFAULT '',
            blocked_groups TEXT NOT NULL DEFAULT '',
            preferred_resolution TEXT NOT NULL DEFAULT '1080',
            quality_profile TEXT NOT NULL DEFAULT 'web_1080',
            quality_cutoff TEXT NOT NULL DEFAULT 'bd_1080',
            finished_series_quality TEXT NOT NULL DEFAULT 'prefer_bd',
            media_root TEXT NOT NULL DEFAULT '',
            title_language TEXT NOT NULL DEFAULT 'english',
            force_mal_fallback INTEGER NOT NULL DEFAULT 0,
            rss_enabled INTEGER NOT NULL DEFAULT 0,
            rss_interval_minutes INTEGER NOT NULL DEFAULT 15,
            force_kitsu_fallback INTEGER NOT NULL DEFAULT 0,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("ALTER TABLE config ADD COLUMN title_language TEXT NOT NULL DEFAULT 'english'")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_use_ssl INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_port TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_host TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN quality_profile TEXT NOT NULL DEFAULT 'web_1080'")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN quality_cutoff TEXT NOT NULL DEFAULT 'bd_1080'")
        .execute(db)
        .await
        .ok();

    sqlx::query(
        "ALTER TABLE config ADD COLUMN finished_series_quality TEXT NOT NULL DEFAULT 'prefer_bd'",
    )
    .execute(db)
    .await
    .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN force_mal_fallback INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN blocked_groups TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN rss_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Default bumped from 5 → 15 minutes (2026-05-03). Five-minute
    // polling rate-limited non-Nyaa RSS feeds (SubsPlease + similar
    // direct-feed publishers) reliably enough that the conservative
    // floor pays off — direct RSS sources tend to enforce stricter
    // polling caps than Nyaa, and the 10-minute extra latency on
    // catch-up doesn't matter for an anime PVR. Existing installs
    // keep whatever value the column already holds; only a fresh-
    // ALTER on a pre-column install picks up the new default.
    sqlx::query("ALTER TABLE config ADD COLUMN rss_interval_minutes INTEGER NOT NULL DEFAULT 15")
        .execute(db)
        .await
        .ok();

    // Migrate old jellyfin_host/port/ssl into jellyfin_url for existing DBs.
    sqlx::query("ALTER TABLE config ADD COLUMN jellyfin_url TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // If jellyfin_url is empty but jellyfin_host is set, build URL from legacy columns.
    sqlx::query(
        r#"
        UPDATE config SET jellyfin_url =
            CASE
                WHEN jellyfin_url != '' THEN jellyfin_url
                WHEN jellyfin_host = '' THEN ''
                WHEN jellyfin_use_ssl = 1 AND jellyfin_port != '' THEN 'https://' || jellyfin_host || ':' || jellyfin_port
                WHEN jellyfin_use_ssl = 1 THEN 'https://' || jellyfin_host
                WHEN jellyfin_port != '' THEN 'http://' || jellyfin_host || ':' || jellyfin_port
                ELSE 'http://' || jellyfin_host
            END
        WHERE id = 1 AND jellyfin_url = '' AND jellyfin_host != ''
        "#,
    )
    .execute(db)
    .await
    .ok();

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            anilist_id INTEGER NOT NULL UNIQUE,
            mal_id INTEGER UNIQUE,
            title TEXT NOT NULL,
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            cover_url TEXT NOT NULL DEFAULT '',
            format TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT '',
            episodes INTEGER,
            folder_name TEXT NOT NULL DEFAULT '',
            added_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("ALTER TABLE series ADD COLUMN title_native TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN mal_id INTEGER")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN title_romaji TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN title_english TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN cover_url TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN format TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN status TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN episodes INTEGER")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN folder_name TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE series ADD COLUMN monitor_mode TEXT NOT NULL DEFAULT 'future'")
        .execute(db)
        .await
        .ok();

    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_series_mal_id ON series (mal_id) WHERE mal_id IS NOT NULL")
        .execute(db)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS episode_cache (
            mal_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            aired TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (mal_id, episode_number)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kitsu_episode_cache (
            kitsu_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            aired TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (kitsu_id, episode_number)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS episode_monitor_state (
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            monitored INTEGER NOT NULL DEFAULT 0,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (series_id, episode_number),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS rss_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            finished_at DATETIME,
            trigger_source TEXT NOT NULL DEFAULT 'manual',
            status TEXT NOT NULL DEFAULT 'running',
            items_seen INTEGER NOT NULL DEFAULT 0,
            matched INTEGER NOT NULL DEFAULT 0,
            grabbed INTEGER NOT NULL DEFAULT 0,
            skipped INTEGER NOT NULL DEFAULT 0,
            detail TEXT NOT NULL DEFAULT ''
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS rss_seen (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            item_key TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL DEFAULT '',
            link TEXT NOT NULL DEFAULT '',
            series_id INTEGER,
            series_title TEXT NOT NULL DEFAULT '',
            group_name TEXT NOT NULL DEFAULT '',
            is_batch INTEGER NOT NULL DEFAULT 0,
            decision TEXT NOT NULL DEFAULT 'skipped',
            reason TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL DEFAULT 'rss',
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (series_id) REFERENCES series(id)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series_relations_cache (
            series_id INTEGER NOT NULL,
            related_provider_id INTEGER NOT NULL,
            related_mal_id INTEGER,
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            cover_url TEXT NOT NULL DEFAULT '',
            format TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT '',
            episodes INTEGER,
            relation_type TEXT NOT NULL DEFAULT '',
            season_year INTEGER,
            media_type TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (series_id, related_provider_id, relation_type),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series_episode_metadata (
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            aired TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (series_id, episode_number),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS provider_metadata_cache (
            provider_id INTEGER PRIMARY KEY,
            mal_id INTEGER,
            detail_json TEXT NOT NULL,
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS provider_relations_cache (
            provider_id INTEGER NOT NULL,
            related_provider_id INTEGER NOT NULL,
            related_mal_id INTEGER,
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            cover_url TEXT NOT NULL DEFAULT '',
            format TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT '',
            episodes INTEGER,
            relation_type TEXT NOT NULL DEFAULT '',
            season_year INTEGER,
            media_type TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (provider_id, related_provider_id, relation_type)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS provider_episode_metadata (
            provider_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            title_romaji TEXT NOT NULL DEFAULT '',
            title_english TEXT NOT NULL DEFAULT '',
            title_native TEXT NOT NULL DEFAULT '',
            aired TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL DEFAULT '',
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (provider_id, episode_number)
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
            level TEXT NOT NULL DEFAULT 'info',
            category TEXT NOT NULL DEFAULT 'system',
            message TEXT NOT NULL DEFAULT '',
            detail TEXT NOT NULL DEFAULT ''
        )
        "#,
    )
    .execute(db)
    .await?;

    // Index for efficient log queries.
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_timestamp ON logs (timestamp DESC)")
        .execute(db)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_level_cat ON logs (level, category)")
        .execute(db)
        .await?;

    // ── Legacy migrations (kept for existing DB compat) ────────────────
    // tmdb_api_key and plex_mappings_* are no longer used but columns
    // remain in existing databases.
    sqlx::query("ALTER TABLE config ADD COLUMN tmdb_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Rename `force_tmdb_fallback` → `force_kitsu_fallback` via the
    // four-state reconciler. The pre-fix path was the same ADD-then-
    // RENAME-with-.ok() pattern that the file_name + restrict_to_*
    // renames moved away from: on a post-migrated install the ADD
    // re-created the legacy column as a vestigial INTEGER alongside
    // the new one (RENAME silently failed against the existing
    // target), leaving a stray column nothing read. The typed
    // reconciler dispatches on the (legacy, new) presence matrix so
    // each starting state moves to the correct end state without
    // creating ghost columns.
    //
    // Cleanup of pre-existing vestiges: DBs that already passed
    // through the v1 ADD-then-RENAME-with-.ok() pattern carry the
    // stray `force_tmdb_fallback` INTEGER alongside the live
    // `force_kitsu_fallback` column. The (true, true) arm of the
    // reconciler DROPs the legacy column on next boot, so a long-
    // running install picks up the cleanup automatically — no
    // operator action required.
    reconcile_column_rename_typed(
        db,
        "config",
        "force_tmdb_fallback",
        "force_kitsu_fallback",
        "INTEGER NOT NULL DEFAULT 0",
        "= 0",
    )
    .await;

    sqlx::query("ALTER TABLE config ADD COLUMN plex_mappings_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query(
        "ALTER TABLE config ADD COLUMN plex_mappings_auto_refresh INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN post_processing_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query(
        "ALTER TABLE config ADD COLUMN post_processing_mode TEXT NOT NULL DEFAULT 'hardlink'",
    )
    .execute(db)
    .await
    .ok();

    // The path where qBittorrent downloads live, as seen by Ryokan.
    // When qBit runs in Docker its internal save_path (e.g. /downloads/) differs
    // from where the host (or Ryokan) can actually read the files.
    sqlx::query("ALTER TABLE config ADD COLUMN qbit_download_path TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS grabbed_torrents (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            hash TEXT NOT NULL DEFAULT '',
            torrent_name TEXT NOT NULL DEFAULT '',
            series_id INTEGER NOT NULL,
            episode_numbers TEXT NOT NULL DEFAULT '[]',
            state TEXT NOT NULL DEFAULT 'pending',
            grabbed_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            imported_at DATETIME,
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_grabbed_torrents_state ON grabbed_torrents (state)",
    )
    .execute(db)
    .await?;

    // Many hot-path queries filter on series_id (find_imported_for_episode,
    // get_all_for_series, mark_failed_by_name, etc.) and the prior schema
    // had no index covering it — every lookup did a full table scan. Sort
    // key lets get_all_for_series / get_blocked / get_all_with_series read
    // in chronological order without a separate sort.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_grabbed_torrents_series ON grabbed_torrents (series_id, grabbed_at DESC)",
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_grabbed_torrents_hash ON grabbed_torrents (hash) WHERE hash != ''")
        .execute(db)
        .await?;

    // One-time backfill: deduplicate active grabs sharing a hash before
    // creating the unique index below. Pre-fix race in record_grab could
    // produce duplicate pending/imported rows for the same hash; the
    // unique index would otherwise refuse to create. Keeps the oldest
    // row per hash (lowest id), drops the rest. Idempotent — a second
    // boot finds no duplicates and the DELETE no-ops.
    sqlx::query(
        r#"DELETE FROM grabbed_torrents
           WHERE hash != ''
             AND state IN ('pending', 'imported')
             AND id NOT IN (
                 SELECT MIN(id) FROM grabbed_torrents
                 WHERE hash != '' AND state IN ('pending', 'imported')
                 GROUP BY hash
             )"#,
    )
    .execute(db)
    .await?;

    // Partial UNIQUE index that backs the atomic dedup in record_grab's
    // INSERT OR IGNORE. Restricted to active states so a hash that's
    // been blocklisted ('failed') or removed can still be re-recorded
    // — preserving the prior SELECT's `state IN ('pending', 'imported')`
    // filter as the dedup window.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_grabbed_torrents_hash_active
         ON grabbed_torrents (hash)
         WHERE hash != '' AND state IN ('pending', 'imported')",
    )
    .execute(db)
    .await?;

    // Per-file series routing for multi-series batch releases. A
    // megapack that covers e.g. JoJo S1-S5 gets one row per sibling
    // in this table, each carrying the file indices (into the
    // torrent's canonical file list) that belong to that sibling and
    // the episode numbers those files represent. The parent series
    // (the one the user actually searched for) also gets a row here
    // covering unclaimed files. Legacy single-series grabs that
    // predate Phase 2 have no row here and are handled by a
    // fall-through path in post_processing that treats the
    // grabbed_torrents.series_id as the sole route.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS grabbed_torrent_series (
            grab_id INTEGER NOT NULL,
            series_id INTEGER NOT NULL,
            file_indices TEXT NOT NULL DEFAULT '[]',
            episode_numbers TEXT NOT NULL DEFAULT '[]',
            matched_subtitle TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (grab_id, series_id),
            FOREIGN KEY (grab_id) REFERENCES grabbed_torrents(id) ON DELETE CASCADE,
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_grabbed_torrent_series_series ON grabbed_torrent_series (series_id)")
        .execute(db)
        .await?;

    // Per-route episode offset for the Phase 2 auto-expand path.
    // Applied by post_processing at rename time to convert a file's
    // absolute episode number into the sibling's arc-local episode
    // number (e.g. smol Monogatari batch: E14 → E01 of Owari S2 with
    // offset 13, NoobSubs JoJo: E25 → E01 of Egypt-hen with offset 24).
    // Non-offset siblings (filenames numbered arc-local from 1) get
    // offset 0, matching the legacy default for rows written before
    // this column existed.
    sqlx::query(
        "ALTER TABLE grabbed_torrent_series ADD COLUMN episode_offset INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // Interactive file-picker scratch state (issue #83). One row per
    // open modal. Created when the user hits Grab, deleted on
    // confirm / cancel / TTL-sweep auto-commit. The TTL sweep (see
    // services::grab_sweep) runs every minute and auto-commits rows
    // whose heartbeat is stale, converting them into normal
    // `grabbed_torrents` rows with all files wanted. No FK to
    // `series` — series_id is nullable because interactive grabs can
    // target bare magnet URLs before a series is selected — but when
    // present it matches the target series for post-confirm
    // sibling-auto-expand routing.
    //
    // `release_metadata_json` stashes the `SearchResult`-shaped
    // payload the modal needs to render before the torrent's file
    // list arrives (title, size, seeders, ...). `file_list_json` is
    // populated once `wait_for_metadata` returns — until then the
    // modal polls `GET /api/grab/preview/{id}` and sees
    // `status: fetching_metadata`.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS pending_grabs (
            preview_id TEXT PRIMARY KEY,
            info_hash TEXT NOT NULL DEFAULT '',
            client_kind TEXT NOT NULL DEFAULT '',
            indexer_id INTEGER,
            series_id INTEGER,
            created_at INTEGER NOT NULL,
            heartbeat_at INTEGER NOT NULL,
            file_list_json TEXT NOT NULL DEFAULT '',
            release_metadata_json TEXT NOT NULL DEFAULT '',
            -- Empty = no metadata-fetch error yet; non-empty = human-
            -- readable failure that GET preview promotes to status=error.
            error_message TEXT NOT NULL DEFAULT '',
            -- 1 when add_torrent_paused returned Added for this preview,
            -- 0 when AlreadyPresent. grab_cancel gates its destructive
            -- delete(hash, with_files=true) on this flag so a cancel on
            -- a pre-existing torrent doesn't nuke prior-grab data. The
            -- ALTER TABLE below is idempotency for upgraders; fresh
            -- installs pick up the column from this CREATE.
            we_added_torrent INTEGER NOT NULL DEFAULT 1
        )
        "#,
    )
    .execute(db)
    .await?;

    // Sweep query filters on `heartbeat_at < now - TTL`; cheap index
    // makes the per-minute tick near-free even as the table grows
    // during heavy modal use.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_pending_grabs_heartbeat ON pending_grabs (heartbeat_at)",
    )
    .execute(db)
    .await?;

    // Pre-modal same-hash dedup check needs a fast lookup from
    // info_hash to "is there already an open modal for this torrent?"
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_pending_grabs_hash ON pending_grabs (info_hash) WHERE info_hash != ''",
    )
    .execute(db)
    .await?;

    // Idempotency guards for databases created before these columns
    // were added to the CREATE TABLE above. Fresh installs pick the
    // columns up from CREATE and these ALTERs silently no-op; existing
    // installs get the columns added with the defaults that match the
    // pre-fix behavior (error_message empty; we_added_torrent=1 so the
    // cancel path behaves conservatively). Column semantics are
    // documented on the CREATE TABLE; keeping the doc in one place so
    // future readers don't have to cross-reference the ALTER history.
    sqlx::query("ALTER TABLE pending_grabs ADD COLUMN error_message TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE pending_grabs ADD COLUMN we_added_torrent INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // Multi-client refactor follow-up — capture which `download_clients`
    // row the preview's `add_torrent_paused` call landed on so the
    // confirm path resumes against the same client. Pre-fix the
    // confirm path used `default_download_client()`, which silently
    // routed selective-narrow + resume to the wrong client when the
    // preview had been pinned to a non-default. NULL on legacy rows
    // and on previews where pin resolution returned the default.
    sqlx::query("ALTER TABLE pending_grabs ADD COLUMN download_client_id INTEGER")
        .execute(db)
        .await
        .ok();

    // Issue #62 — external AL/MAL account linkage. One row per
    // linked provider (decision #10 limits this to one row total at
    // any time; the "at most one" invariant is enforced in the
    // `external_accounts` model's `link` function rather than in the
    // schema so unlink-and-relink flows don't need migration-level
    // logic). Access + refresh tokens are AEAD-encrypted via
    // `services::crypto` before insert — plaintext never touches the
    // DB. `refresh_token_encrypted` is empty-blob for AniList (which
    // uses implicit grant and has no refresh token); MAL populates it.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS external_accounts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            -- 'anilist' or 'mal'. CHECK keeps a typo from leaking into
            -- the sync task's provider-dispatch match arms.
            provider TEXT NOT NULL CHECK (provider IN ('anilist', 'mal')),
            -- Provider's stable, rename-immune user identifier.
            -- AL: Viewer.id (numeric, stored as string). MAL: id from
            -- `GET /v2/users/@me?fields=id` (also numeric, also stored
            -- as string) — MAL usernames are user-mutable, so keying
            -- on the numeric id is what makes re-link survive a
            -- rename on the provider side.
            provider_user_id TEXT NOT NULL,
            username TEXT NOT NULL DEFAULT '',
            access_token_encrypted BLOB NOT NULL,
            refresh_token_encrypted BLOB NOT NULL DEFAULT x'',
            -- MAL access tokens expire in 30 days; AL implicit tokens
            -- are fixed 1-year. NULL means "no known expiry, don't
            -- preemptively refresh" (the AL case).
            access_token_expires_at INTEGER,
            -- AL's POINT_3 / POINT_5 / POINT_10 / POINT_10_DECIMAL /
            -- POINT_100 per the user's Viewer.mediaListOptions.scoreFormat.
            -- Empty string pre-fetch. MAL hardcodes 'POINT_10'.
            score_format TEXT NOT NULL DEFAULT '',
            -- Delta sync cursor. NULL until the first successful sync.
            list_last_synced_at INTEGER,
            -- Separate cursor for the weekly full-resync backstop
            -- (decision #4). NULL until the first full resync completes.
            list_full_resync_at INTEGER,
            linked_at INTEGER NOT NULL,
            -- Per-list opt-in checkboxes surfaced on Settings →
            -- Connections → External Accounts. Watching + PTW default
            -- on; Paused / Dropped / Completed default off (issue body).
            import_watching INTEGER NOT NULL DEFAULT 1,
            import_planning INTEGER NOT NULL DEFAULT 1,
            import_paused INTEGER NOT NULL DEFAULT 0,
            import_dropped INTEGER NOT NULL DEFAULT 0,
            import_completed INTEGER NOT NULL DEFAULT 0,
            -- Per-account opt-in: skip episodes the user has already
            -- watched on the provider (using the `progress` field).
            -- Default off per decision #7 — airing-series case makes
            -- on-by-default silently broken.
            skip_already_watched INTEGER NOT NULL DEFAULT 0,
            -- Exactly one row per provider (AL / MAL are separate
            -- rows even though the one-at-a-time invariant means only
            -- one will exist in practice). Enforces idempotence on
            -- re-link of the same provider.
            UNIQUE (provider)
        )
        "#,
    )
    .execute(db)
    .await?;

    // tmdb_id on series is a leftover from before the Kitsu migration;
    // the column is harmless to keep for existing databases.
    sqlx::query("ALTER TABLE series ADD COLUMN tmdb_id INTEGER")
        .execute(db)
        .await
        .ok();

    // #62 — track which external_account most-recently synced
    // each series. NULL for manually-added series + pre-PR-B rows.
    // Used by sync's removal-detection pass: on full-resync, series
    // marked with a sync source whose AL id is NOT in the current
    // fetch get monitor_mode downgraded to None (the user removed
    // them from their AL/MAL list). ON DELETE SET NULL so unlinking
    // an account doesn't cascade-drop the imported series rows.
    sqlx::query(
        "ALTER TABLE series ADD COLUMN synced_from_external_account_id INTEGER \
         REFERENCES external_accounts(id) ON DELETE SET NULL",
    )
    .execute(db)
    .await
    .ok();

    // #62 — pinned monitor_mode flag. Set when the user changes
    // monitor_mode through the per-series UI; cleared when the user
    // picks "Sync from AL/MAL" from the same dropdown. The
    // watch-list sync's merge step skips updating monitor_mode on
    // rows where this is 1, and the removal-detection pass skips
    // them too (a manually-pinned series stays pinned even when the
    // user removes it from their AL list — they explicitly chose
    // this monitor mode). Mirrors the
    // `episode_quality_tags.manual_override` pattern used by the
    // upgrade sweep.
    sqlx::query(
        "ALTER TABLE series ADD COLUMN monitor_mode_manual_override INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // #62 — user's personal score on the linked AL/MAL account
    // for this series. NULL means "no linked account" or "unrated"
    // (the watch-list sync writes 0.0 for unrated entries; the read
    // path treats 0.0 the same as NULL when rendering — never shows
    // "You: 0"). REAL because AL's POINT_10_DECIMAL format stores
    // fractional values; integer formats store as e.g. 8.0 and the
    // render helper formats them back as integers.
    sqlx::query("ALTER TABLE series ADD COLUMN user_score REAL")
        .execute(db)
        .await
        .ok();

    // #62 — AL custom-list membership. AL groups list entries
    // into status buckets (CURRENT/PLANNING/etc.) plus zero or more
    // user-named custom lists; a series can belong to many at once.
    // The sync engine pulls per-entry membership in the same
    // GraphQL response and reconciles this side table on every
    // merge action.
    //
    // ON DELETE CASCADE so removing a series wipes its membership
    // rows without a hand-tracked cleanup. UNIQUE (series_id, provider,
    // list_name) enforces no-dup membership per (series, account).
    // `provider` is on the row even though only AL emits these today
    // (decision-doc baseline); a hypothetical future provider with
    // its own custom-list concept gets a parallel namespace.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series_custom_lists (
            series_id INTEGER NOT NULL,
            provider TEXT NOT NULL,
            list_name TEXT NOT NULL,
            PRIMARY KEY (series_id, provider, list_name),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_series_custom_lists_list_name \
         ON series_custom_lists (list_name)",
    )
    .execute(db)
    .await?;

    // #62 — genre side table for the library filter dropdown.
    // Genres come from AL/Jikan AnimeDetail.genres (already cached
    // in series_metadata_cache); we extract them into their own
    // table on every metadata refresh + sync merge so the filter +
    // autocomplete reads can hit a small indexed scan instead of
    // unmarshalling JSON for every series. Provider-agnostic — both
    // AL and Jikan (MAL) emit the same genre vocabulary, so unlike
    // custom_lists this table doesn't carry a `provider` column.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series_genres (
            series_id INTEGER NOT NULL,
            genre TEXT NOT NULL,
            PRIMARY KEY (series_id, genre),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_series_genres_genre \
         ON series_genres (genre)",
    )
    .execute(db)
    .await?;

    // #62 — count of MAL→AL mapping failures from the most
    // recent sync run. Surfaces on the Settings → External Accounts
    // card as a "N series couldn't be mapped to AniList" banner so
    // the user knows which subset of their MAL list is on the
    // negated-id sentinel path (no SeaDex keying, etc.). Set on
    // every successful MAL sync; AL syncs leave the column at 0.
    sqlx::query(
        "ALTER TABLE external_accounts ADD COLUMN last_sync_deferred_count INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // #62 — sticky flag set when a sync tick fails because
    // the auth token was rejected (AL 401/403 or MAL refresh-token
    // dead). Cleared on the next successful tick. Drives the
    // "Re-link required" banner on the External Accounts card so
    // a user whose AL token expired (1-year TTL) doesn't have to
    // dig through System → Logs to figure out why their sync
    // stopped working.
    sqlx::query(
        "ALTER TABLE external_accounts ADD COLUMN last_sync_auth_failed INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS series_metadata_cache (
            series_id INTEGER PRIMARY KEY,
            provider_id INTEGER NOT NULL,
            mal_id INTEGER,
            detail_json TEXT NOT NULL,
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_series_metadata_cache_cached_at ON series_metadata_cache (cached_at DESC)")
        .execute(db)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS artwork_cache (
            cache_key TEXT PRIMARY KEY,
            parent_kind TEXT NOT NULL DEFAULT '',
            parent_id INTEGER,
            image_kind TEXT NOT NULL DEFAULT '',
            source_url TEXT NOT NULL DEFAULT '',
            local_path TEXT NOT NULL DEFAULT '',
            content_type TEXT NOT NULL DEFAULT '',
            last_write INTEGER NOT NULL DEFAULT 0,
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (parent_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_artwork_cache_parent ON artwork_cache (parent_kind, parent_id, image_kind)")
        .execute(db)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS image_blobs (
            blob_hash TEXT PRIMARY KEY,
            local_path TEXT NOT NULL DEFAULT '',
            content_type TEXT NOT NULL DEFAULT '',
            byte_size INTEGER NOT NULL DEFAULT 0,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS image_refs (
            cache_key TEXT PRIMARY KEY,
            parent_kind TEXT NOT NULL DEFAULT '',
            parent_id INTEGER,
            image_kind TEXT NOT NULL DEFAULT '',
            source_url TEXT NOT NULL DEFAULT '',
            blob_hash TEXT NOT NULL,
            last_write INTEGER NOT NULL DEFAULT 0,
            cached_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (blob_hash) REFERENCES image_blobs(blob_hash) ON DELETE CASCADE,
            FOREIGN KEY (parent_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_image_refs_parent ON image_refs (parent_kind, parent_id, image_kind)")
        .execute(db)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_image_refs_blob_hash ON image_refs (blob_hash)")
        .execute(db)
        .await?;

    // Episode quality tags: store the latest grabbed release for each episode.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS episode_quality_tags (
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            quality_tag TEXT NOT NULL DEFAULT '',
            release_title TEXT NOT NULL DEFAULT '',
            release_group TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT 'grabbed',
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (series_id, episode_number),
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    // Full grab history per episode (all grabs, with state tracking for failed marks).
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS episode_grab_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            quality_tag TEXT NOT NULL DEFAULT '',
            release_title TEXT NOT NULL DEFAULT '',
            release_group TEXT NOT NULL DEFAULT '',
            file_name TEXT NOT NULL DEFAULT '',
            size_bytes INTEGER NOT NULL DEFAULT 0,
            is_batch INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL DEFAULT 'grabbed',
            grabbed_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_episode_grab_history_series ON episode_grab_history (series_id, episode_number, grabbed_at DESC)")
        .execute(db)
        .await?;

    // On-disk *post-processed* file name for this episode. Seeded from the
    // Nyaa release title at grab time, then overwritten at post-process
    // time with the final Sonarr-style filename Ryokan renamed the imported
    // file to (e.g. `Jujutsu Kaisen - S01E06 - Hidden Inventory.mkv`). The
    // episode detail modal reads this column so each grab-history row
    // shows the per-episode file name — distinct from the batch torrent's
    // release title, which is already in `release_title`. Historically
    // this column was called `torrent_name`.
    //
    // Upgrade path: check for `file_name` first — a fresh install gets
    // it from CREATE TABLE above, and an already-migrated install has
    // it from a prior rename. If it's missing we're on a legacy
    // `torrent_name` install and need to rename. The defensive ADD
    // covers the corner case where neither column is present (which
    // shouldn't happen, but keeps downstream writes safe).
    if !column_exists(db, "episode_grab_history", "file_name").await {
        // Two paths for the legacy schema:
        //
        //  - `torrent_name` exists  → RENAME it to `file_name`. Propagate
        //    any failure from the RENAME instead of swallowing with .ok():
        //    the previous code paired the RENAME with an unconditional ADD
        //    so a transient RENAME failure (DB lock, FK quirk, I/O hiccup)
        //    would leave an empty `file_name` column on top of intact
        //    `torrent_name` data and the next boot would think the
        //    migration was already done. Refusing to start with a real
        //    error is preferable to silent data loss.
        //
        //  - `torrent_name` is also missing → defensive ADD for the
        //    corrupted-schema corner case. Without `torrent_name` to
        //    rename from there is no data to lose, so the ADD is safe.
        if column_exists(db, "episode_grab_history", "torrent_name").await {
            sqlx::query("ALTER TABLE episode_grab_history RENAME COLUMN torrent_name TO file_name")
                .execute(db)
                .await?;
        } else {
            sqlx::query(
                "ALTER TABLE episode_grab_history ADD COLUMN file_name TEXT NOT NULL DEFAULT ''",
            )
            .execute(db)
            .await?;
        }
    }

    // Episode-file size. For non-batch grabs this gets refined to the
    // imported file's size at post-process time. For batch grabs it
    // stays as the whole torrent's total reported at grab time — the
    // episode detail modal surfaces that as "this episode came from an
    // X GiB batch". The CASE guard in `mark_grab_history_completed`
    // enforces this asymmetry.
    sqlx::query(
        "ALTER TABLE episode_grab_history ADD COLUMN size_bytes INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // is_batch marker — needed at read time so the UI can decide whether
    // to surface `size_bytes` as "whole batch" or "single file". It's
    // also what `mark_grab_history_completed` uses to decide whether to
    // refine `size_bytes` on import (non-batch only).
    sqlx::query("ALTER TABLE episode_grab_history ADD COLUMN is_batch INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // auto_grab_on_add: whether to automatically search for monitored episodes after adding a series.
    sqlx::query("ALTER TABLE config ADD COLUMN auto_grab_on_add INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // search_on_monitoring_change (v1.3.0 UX pass): when true, any
    // update to a series's monitoring mode triggers a background
    // auto-search over the newly-monitored-and-airable episodes.
    // Default off to preserve existing behavior on upgrade.
    sqlx::query(
        "ALTER TABLE config ADD COLUMN search_on_monitoring_change INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // prefer_subs: when true (default), penalize dual audio / dub releases in scoring.
    sqlx::query("ALTER TABLE config ADD COLUMN prefer_subs INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // allow_non_english: when false (default), auto-search/RSS uses English-translated Nyaa category.
    sqlx::query("ALTER TABLE config ADD COLUMN allow_non_english INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Sonarr API compatibility layer for Seerr integration.
    sqlx::query("ALTER TABLE config ADD COLUMN sonarr_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN sonarr_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // season_year on series for Sonarr/Radarr compat year field.
    sqlx::query("ALTER TABLE series ADD COLUMN season_year INTEGER")
        .execute(db)
        .await
        .ok();

    // end_year lets Layer 4 (temporal inference) reason about how long a
    // *finished* series has been off-air, rather than treating season_year
    // (the start year) as the finish proxy. Long-running shows can finish
    // years after their season_year; without end_year the "finished 1+
    // year ago" rule would fire immediately for every ep of a decade-long
    // run. Populated from AniList's endDate.year where available.
    sqlx::query("ALTER TABLE series ADD COLUMN end_year INTEGER")
        .execute(db)
        .await
        .ok();

    // Phase 4: per-series upgrade toggle. When 0, the upgrade scanner
    // skips this series entirely — user opts out of re-grabs even if a
    // higher-quality release appears. Default 1 preserves prior behavior.
    sqlx::query("ALTER TABLE series ADD COLUMN allow_upgrades INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // Issue #28 — per-series PT upgrade opt-in. Default 0 (off).
    // The upgrade sweep skips a candidate when the source indexer is
    // a private tracker (`indexers.is_private_tracker = 1`) and this
    // flag is 0. Initial / manual / interactive grabs aren't gated —
    // the user explicitly chose those. The flag only affects the
    // background upgrade sweep, which can re-grab existing episodes
    // from PTs without the user's knowledge if left default-on.
    sqlx::query("ALTER TABLE series ADD COLUMN allow_pt_upgrades INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Radarr API compatibility layer for Seerr integration (anime movies).
    sqlx::query("ALTER TABLE config ADD COLUMN radarr_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN radarr_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN upgrade_search_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Classification pipeline: release group → source map (Phase 1a).
    // Creates the table and seeds the built-in defaults. Idempotent.
    group_source_map::migrate(db).await?;

    // Layer 2 description cache — scraped Nyaa description bodies keyed by
    // torrent info_hash, populated on the low-confidence classifier path.
    nyaa_description_cache::migrate(db).await?;

    // Layer 5 ffprobe cache — cached ffprobe JSON keyed by (path, mtime, size).
    // Populated after imports land so re-classifications (library scans,
    // upgrade checks) don't re-shell out to ffprobe for the same file.
    media_probe_cache::migrate(db).await?;

    // Sonarr-v4-compatible Custom Formats. Two tables: one for CF
    // definitions (raw JSON preserved for byte-perfect re-export) and
    // one for (custom_format_id, profile_id) → score. V1 hardcodes
    // profile_id = 1 everywhere.
    custom_formats::migrate(db).await?;

    // Upgrade path for databases that were created before the `origin`
    // column shipped. Fresh installs already got this column from the
    // CREATE TABLE in `custom_formats::migrate`; the ALTER here is a
    // no-op on those and adds the column on legacy databases. Legal
    // values: `manual`, `import`, `defaults`. Pre-existing rows default
    // to `manual` — anyone who already installed defaults before this
    // column shipped can use the Reset button to relabel them.
    sqlx::query("ALTER TABLE custom_formats ADD COLUMN origin TEXT NOT NULL DEFAULT 'manual'")
        .execute(db)
        .await
        .ok();

    // ── Phase 1b: classification columns on episode_quality_tags ─────────
    // These record the ClassificationResult at grab time so later scoring,
    // upgrade detection, and UI review workflows can read structured source
    // / resolution / remux data instead of parsing the legacy quality_tag
    // string. Defaults are empty/zero for rows that predate Phase 1b; the
    // legacy quality_tag column remains populated for backwards compat.
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN source TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN resolution TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN is_remux INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN classification_confidence REAL NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    sqlx::query(
        "ALTER TABLE episode_quality_tags ADD COLUMN needs_review INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();
    sqlx::query(
        "ALTER TABLE episode_quality_tags ADD COLUMN manual_override INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // Sonarr-parity sub-classification columns:
    //  - is_bdmv: distinguishes BD-RAW / BDMV (full disc structure) from
    //    a plain BluRay encode or a Remux. Mutually exclusive with
    //    is_remux at the label level.
    //  - web_kind: distinguishes WEB-DL from WEBRip when the filename was
    //    specific enough to tell. Stored as the canonical string ("WEB-DL",
    //    "WEBRip", or "" for legacy bare-WEB rows).
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN is_bdmv INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    // is_batch on grabbed_torrents lets the post-download classifier
    // re-run Layer 4 (temporal inference) with the original batch flag
    // that the pre-download call used, rather than hardcoding `false`
    // and losing the "finished 1+ year ago + batch → BluRay" signal
    // after import.
    sqlx::query("ALTER TABLE grabbed_torrents ADD COLUMN is_batch INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Sonarr-parity dual-path tracking (#14 follow-up, renamed to
    // `client_content_path` in #63). Stamped from the download
    // client's native content path (qBit ≥ 2.6.1) or `save_path`
    // fallback the moment the client reports the torrent complete,
    // independent of whether post-processing has moved the file into
    // the library. The `reconcile_column_rename` call below handles
    // all four DB states a column-rename can leave behind:
    //   - fresh install → adds `client_content_path` (empty default)
    //   - pre-#63 upgrade with only legacy → renames qbit_content_path
    //   - post-#63 steady state → no-op
    //   - (rare) both columns present → copy + drop legacy
    // Note: we deliberately do NOT `ADD COLUMN qbit_content_path` as a
    // prerequisite. Doing so would tip every post-migration boot into
    // the `(true, true)` branch, which triggers a SQLite `DROP COLUMN`
    // — and SQLite ≥ 3.35 implements DROP COLUMN as a full-table
    // rewrite, so leaving the legacy ADD in place rewrites
    // `grabbed_torrents` on every startup.

    // ── #63 — pluggable download clients ───────────────────────
    //
    // `client_type` discriminator on each grab row so a future
    // multi-client config (Phase 2+) can route per-client operations
    // correctly. Existing rows backfill to 'qbittorrent' since that
    // was the only option pre-1.2.0. Uses TEXT NOT NULL DEFAULT so
    // fresh rows and historical rows are both deterministic.
    sqlx::query(
        "ALTER TABLE grabbed_torrents ADD COLUMN client_type TEXT NOT NULL DEFAULT 'qbittorrent'",
    )
    .execute(db)
    .await
    .ok();

    // `replaced_by_grab_id` — nullable back-pointer set when an
    // upgrade-driven import supersedes this grab. Paired with a new
    // `state='replaced'` value (distinct from `removed`, which is the
    // generic "gone from download client" state reserved for user
    // cancels and cleanup). No FK: an `ON DELETE SET NULL` would need
    // a schema rebuild under SQLite and pruning a new grab shouldn't
    // dangle old replaced rows in a way that breaks queries — the
    // history handler tolerates `NULL` here. History filter and the
    // replaced-by tooltip in the Downloads tab key off this column to
    // show the replacement chain.
    sqlx::query("ALTER TABLE grabbed_torrents ADD COLUMN replaced_by_grab_id INTEGER")
        .execute(db)
        .await
        .ok();

    // `active_client` on config — lowercase-snake discriminator for
    // the download client currently in use. Phase 1 only has
    // 'qbittorrent'; Phase 2+ will branch on this at AppState init.
    // The qBit credential columns (qbit_url, qbit_user, qbit_pass,
    // qbit_category, qbit_download_path) stay flat on `config` for
    // now — Phase 2 will namespace them into per-client blocks. The
    // idempotent DEFAULT ensures existing installs upgrade cleanly:
    // a fresh row and a pre-upgrade row both end up with
    // active_client = 'qbittorrent' (the only choice pre-1.2.0).
    sqlx::query("ALTER TABLE config ADD COLUMN active_client TEXT NOT NULL DEFAULT 'qbittorrent'")
        .execute(db)
        .await
        .ok();

    // #63 — Deluge credentials + label. The label is Deluge's
    // scoping mechanism (Ryokan sets it per-grab via
    // `label.set_torrent`) and defaults to "ryokan" at trait-impl
    // construction when empty here. Same base-URL pattern as qBit
    // (the `/json` suffix is appended inside the DelugeClient impl).
    sqlx::query("ALTER TABLE config ADD COLUMN deluge_url TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN deluge_password TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN deluge_label TEXT NOT NULL DEFAULT 'ryokan'")
        .execute(db)
        .await
        .ok();

    // #63 — Per-client download path, same shape as the
    // long-standing `qbit_download_path`. Ryokan reads the client's
    // completed files from `<client>_download_path`; the client's
    // own reported save_path (container-internal, or on a seedbox)
    // isn't reachable from Ryokan's process without translation.
    // Single-field-per-client is simpler than the prefix-pair
    // `remote_path_remote` / `remote_path_local` design from the
    // initial Phase 2 sketch — matches user mental model ("where
    // does Ryokan see Deluge's downloads?") and parallels how
    // `qbit_download_path` has always worked.
    sqlx::query("ALTER TABLE config ADD COLUMN deluge_download_path TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // #63 — Transmission credentials + label + download_path.
    // Transmission uses HTTP Basic auth (user + password) rather than
    // Deluge's password-only model. Native 4.x `labels: [String]` on
    // `torrent-add`/`torrent-get` is our scoping mechanism.
    sqlx::query("ALTER TABLE config ADD COLUMN transmission_url TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN transmission_user TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN transmission_password TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN transmission_label TEXT NOT NULL DEFAULT 'ryokan'")
        .execute(db)
        .await
        .ok();
    sqlx::query(
        "ALTER TABLE config ADD COLUMN transmission_download_path TEXT NOT NULL DEFAULT ''",
    )
    .execute(db)
    .await
    .ok();

    // #63 — rtorrent credentials + label + download_path.
    // URL is the full XML-RPC endpoint (e.g. `http://host:8081/RPC2`)
    // taken verbatim — deployment shape varies too much to infer a
    // default path suffix. Scoping label stored in rtorrent's
    // `custom1` field (ruTorrent convention).
    sqlx::query("ALTER TABLE config ADD COLUMN rtorrent_url TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN rtorrent_user TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN rtorrent_password TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN rtorrent_label TEXT NOT NULL DEFAULT 'ryokan'")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN rtorrent_download_path TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Phase 2.1 — columns added on the initial Phase 2 sketch of a
    // global `remote_path_remote` / `remote_path_local` pair.
    // Retained as dead columns (dropping would force a full-table
    // rewrite on every boot, per the code-review finding that bit
    // Phase 1 on the qbit_content_path rename). The `Config` struct
    // no longer reads these fields; they sit unused until a future
    // DROP COLUMN ever lands.
    sqlx::query("ALTER TABLE config ADD COLUMN remote_path_remote TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN remote_path_local TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // One-time data migration: preserve any `remote_path_local`
    // values users set under the Phase 2 sketch (only really
    // reachable on the dev branch pre-merge). Copy into whichever
    // per-client download_path is active at migration time. Guarded
    // by target-field-empty so re-running on already-migrated DBs
    // is a no-op (the per-client field is authoritative once set).
    let _ = sqlx::query(
        "UPDATE config
         SET deluge_download_path = remote_path_local
         WHERE active_client = 'deluge'
           AND remote_path_local <> ''
           AND deluge_download_path = ''",
    )
    .execute(db)
    .await;
    let _ = sqlx::query(
        "UPDATE config
         SET qbit_download_path = remote_path_local
         WHERE active_client = 'qbittorrent'
           AND remote_path_local <> ''
           AND qbit_download_path = ''",
    )
    .execute(db)
    .await;

    // Rename `qbit_content_path` → `client_content_path` so the field
    // name reflects the trait abstraction. Uses the same state-matrix
    // reconciler as the PR #37 rename so half-migrated DBs survive.
    reconcile_column_rename(
        db,
        "grabbed_torrents",
        "qbit_content_path",
        "client_content_path",
    )
    .await;

    // ── #63 follow-up — legacy base32 hash backfill ────────────
    //
    // Phase 0 canonicalized `extract_hash` to lowercase hex going
    // forward, but any pre-Phase-0 rows with 32-char base32 values in
    // `grabbed_torrents.hash` remain as-is. Non-qBit clients normalize
    // to hex internally, so those legacy rows won't match the client's
    // reported hash once a non-qBit impl is active. Backfill scans for
    // 32-char hash rows, decodes via the base32 helper, and rewrites
    // them to lowercase hex.
    //
    // Gated by a `config` flag so a partial run or one-time upgrade
    // doesn't re-run on every boot. Nyaa magnets are overwhelmingly
    // hex so the affected row count should be near zero in practice,
    // but the backfill unifies the partial UNIQUE index on (hash)
    // once and forever. The `base32_backfill_done` config column the
    // first version of this migration added is intentionally left in
    // place on existing DBs (DROP COLUMN is risky and the column is
    // harmless): the SELECT below already self-gates by length, so
    // the bespoke flag was redundant from the start. Per CLAUDE.md
    // ("Do NOT invent a per-migration config flag — that's what
    // `schema_migrations` is for"), one-shot data rewrites that need
    // a guard go through `schema_migrations`; this one doesn't need
    // any guard at all because base32 hashes are 32 chars and hex
    // hashes are 40, so once a row is converted it never matches the
    // SELECT again. Re-running on a fully-migrated DB is a no-op.
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, hash FROM grabbed_torrents WHERE LENGTH(hash) = 32")
            .fetch_all(db)
            .await
            .unwrap_or_default();

    for (id, b32_hash) in rows {
        if let Some(bytes) = crate::services::nyaa::base32_decode_infohash(&b32_hash) {
            let hex_hash = hex::encode(bytes);
            let _ = sqlx::query("UPDATE grabbed_torrents SET hash = ? WHERE id = ?")
                .bind(&hex_hash)
                .bind(id)
                .execute(db)
                .await;
        }
    }

    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN web_kind TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    // Persist the full evidence trail as JSON so the Needs-Review UI can
    // audit *why* a row was flagged without re-classifying. Stores a
    // serialized `Vec<SourceEvidence>` — empty string for legacy rows and
    // for rehydrated/synthetic classifications that don't carry a live
    // trail.
    sqlx::query(
        "ALTER TABLE episode_quality_tags ADD COLUMN classification_evidence TEXT NOT NULL DEFAULT ''",
    )
    .execute(db)
    .await
    .ok();

    // ── Issue #53: stamp post-classify attempts so the library scan
    //                doesn't loop-retry rows whose source layer can't
    //                produce a confident verdict ──────────────────────
    // NULL = "the full-pipeline classifier (ffprobe + dir + group +
    // temporal + filename) has never been run against this row's file".
    // CURRENT_TIMESTAMP after every `update_classification` write and
    // after every `record_grab` write that came from
    // `scan_library_for_unclassified` or post-processing's
    // `classify_post_download`. Grab-time `record_grab` writes leave it
    // NULL on the INSERT path — they're filename-only and aren't a
    // "real" attempt. On the ON CONFLICT UPDATE path, grab-time
    // `record_grab` preserves whatever value was already there (no
    // SET line for the column), so a re-grab after a prior classify
    // keeps the attempt stamp intact — which is what we want; a
    // re-grab of a file we already probed shouldn't reopen it to
    // another sweep retry.
    //
    // The 6h library sweep skips rows where the source is empty or
    // "unknown" AND this column IS NOT NULL: the classifier already
    // tried with the file in hand and couldn't decide, so re-running
    // ffprobe on the same bytes won't change the verdict and just
    // wastes CPU and IO every six hours.
    sqlx::query("ALTER TABLE episode_quality_tags ADD COLUMN classification_attempted_at TEXT")
        .execute(db)
        .await
        .ok();

    // Run the seed-drift episode reset *after* the Phase 1b ALTER TABLE
    // block above. The companion `reconcile_seed_drift` runs earlier
    // inside `group_source_map::migrate` (where it only touches the
    // group_source_map table, which is fully migrated by that point),
    // but the episode reset references columns added immediately above
    // (`source`, `classification_attempted_at`, etc.) so it must wait
    // until those exist on a fresh database boot.
    group_source_map::reconcile_episode_seed_drift(db).await?;

    // ── Phase 1b: split quality_profile/quality_cutoff into explicit source
    //             and resolution fields ──────────────────────────────────
    // preferred_resolution already exists and stores a bare resolution
    // string ("1080", "720", …) — it's migrated in place and is now the
    // authoritative preferred-resolution field. The three new columns cover
    // the bits that didn't exist before. Legacy quality_profile and
    // quality_cutoff are kept for one release as a rollback hatch.
    // Floor for total_cf_score after the CF pipeline sums a candidate's
    // matching formats. Default `-2147483648` (= i32::MIN) means "no
    // floor" — the user opts in by raising it via the Custom Formats
    // settings page. Read paths fall back to this sentinel when the
    // column is present but the row predates it.
    sqlx::query("ALTER TABLE config ADD COLUMN custom_format_minimum_score INTEGER NOT NULL DEFAULT -2147483648")
        .execute(db)
        .await
        .ok();

    // SeaDex "best release" boost toggle. Default OFF so upgrades don't
    // kick in silently for existing installs on first run after the
    // feature ships. Suppressed at scoring time when the user already
    // has a SeaDexBestSpecification CF (avoids double-counting).
    sqlx::query("ALTER TABLE config ADD COLUMN seadex_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    sqlx::query("ALTER TABLE config ADD COLUMN preferred_source TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN cutoff_source TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN cutoff_resolution TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Backfill the new columns from the legacy combined fields. Only runs
    // for existing rows where the new columns are still empty, so a fresh
    // install (which uses Default::default values) is left alone.
    sqlx::query(
        r#"
        UPDATE config SET preferred_source = CASE
            WHEN quality_profile LIKE 'web_%' THEN 'web'
            WHEN quality_profile LIKE 'bd_%' THEN 'bluray'
            WHEN quality_profile LIKE 'remux_%' THEN 'bluray'
            WHEN quality_profile = 'dvd' THEN 'dvd'
            ELSE 'web'
        END
        WHERE id = 1 AND preferred_source = ''
        "#,
    )
    .execute(db)
    .await
    .ok();

    sqlx::query(
        r#"
        UPDATE config SET cutoff_source = CASE
            WHEN quality_cutoff LIKE 'web_%' THEN 'web'
            WHEN quality_cutoff LIKE 'bd_%' THEN 'bluray'
            WHEN quality_cutoff LIKE 'remux_%' THEN 'bluray'
            WHEN quality_cutoff = 'dvd' THEN 'dvd'
            ELSE 'bluray'
        END
        WHERE id = 1 AND cutoff_source = ''
        "#,
    )
    .execute(db)
    .await
    .ok();

    sqlx::query(
        r#"
        UPDATE config SET cutoff_resolution = CASE
            WHEN quality_cutoff LIKE '%_480' OR quality_cutoff = 'dvd' THEN '480'
            WHEN quality_cutoff LIKE '%_720' THEN '720'
            WHEN quality_cutoff LIKE '%_1080' THEN '1080'
            WHEN quality_cutoff LIKE '%_2160' THEN '2160'
            ELSE '1080'
        END
        WHERE id = 1 AND cutoff_resolution = ''
        "#,
    )
    .execute(db)
    .await
    .ok();

    // Rewrite denormalized `quality_tag` strings on pre-existing
    // `episode_quality_tags` / `episode_grab_history` rows to match the
    // Sonarr-parity label format the classifier now emits:
    // `BD-1080p`, `BD-1080p Remux`, `BD-1080p RAW`, `WEB-1080p`,
    // `WEBRip-1080p`, `HDTV-1080p`, `DVD-480p`, etc. This migration
    // bridges three prior schemas: (1) very old space-joined rows
    // like "BluRay 1080p" / "WEB-DL 1080p", (2) the intermediate
    // dash-joined rename (`BD-Remux-1080p`, `BD-RAW-1080p`,
    // `WEBRIP-1080p`) that shipped briefly before the Sonarr-parity
    // reorder landed, and (3) the post-#48 `WEBDL-1080p` intermediate
    // that was subsequently unified into bare `WEB-1080p`.
    //
    // `episode_quality_tags` has the structured source/resolution/
    // web_kind/is_remux/is_bdmv columns, so we regenerate `quality_tag`
    // directly from ground truth — always correct regardless of which
    // label format happened to be in the column. `episode_grab_history`
    // doesn't carry the structured columns (it's a grab-time audit
    // trail, not a classification store), so we fall back to ordered
    // REPLACE statements on known legacy patterns. Fully idempotent:
    // the regen overwrites with the same value on re-runs, and the
    // REPLACE chain no-ops once its source patterns are gone.
    // The CASE is duplicated in SET and WHERE on purpose: gating the
    // UPDATE means SQLite only writes rows whose quality_tag would
    // actually change, so a boot on an already-migrated database does
    // zero WAL writes here instead of dirtying every row in the table.
    // Without the gate, every boot churned the WAL and held the write
    // lock long enough to delay the very first incoming request after
    // startup.
    //
    // MAINTENANCE: any edit to the SET CASE must be mirrored in the
    // WHERE CASE below (and vice versa). Diverging the two is a
    // correctness bug — the WHERE's job is to match the SET's output
    // exactly, so the gate only skips rows that truly don't need the
    // rewrite.
    sqlx::query(
        r#"
        UPDATE episode_quality_tags SET quality_tag = CASE
            WHEN TRIM(COALESCE(source, '')) = ''
              OR LOWER(source) = 'unknown' THEN
                CASE WHEN COALESCE(resolution, '') IN ('', 'Unknown')
                     THEN 'Unknown' ELSE resolution END
            ELSE
                (CASE
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd') THEN 'BD'
                    WHEN LOWER(source) = 'web' THEN
                        CASE
                            WHEN LOWER(COALESCE(web_kind, '')) IN ('webrip', 'web-rip', 'web.rip') THEN 'WEBRip'
                            ELSE 'WEB'
                        END
                    ELSE UPPER(source)
                END)
                || CASE WHEN COALESCE(resolution, '') IN ('', 'Unknown')
                        THEN '' ELSE '-' || resolution END
                || CASE
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd')
                         AND COALESCE(is_bdmv, 0) = 1 THEN ' RAW'
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd')
                         AND COALESCE(is_remux, 0) = 1 THEN ' Remux'
                    ELSE ''
                END
        END
        WHERE COALESCE(quality_tag, '') <> CASE
            WHEN TRIM(COALESCE(source, '')) = ''
              OR LOWER(source) = 'unknown' THEN
                CASE WHEN COALESCE(resolution, '') IN ('', 'Unknown')
                     THEN 'Unknown' ELSE resolution END
            ELSE
                (CASE
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd') THEN 'BD'
                    WHEN LOWER(source) = 'web' THEN
                        CASE
                            WHEN LOWER(COALESCE(web_kind, '')) IN ('webrip', 'web-rip', 'web.rip') THEN 'WEBRip'
                            ELSE 'WEB'
                        END
                    ELSE UPPER(source)
                END)
                || CASE WHEN COALESCE(resolution, '') IN ('', 'Unknown')
                        THEN '' ELSE '-' || resolution END
                || CASE
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd')
                         AND COALESCE(is_bdmv, 0) = 1 THEN ' RAW'
                    WHEN LOWER(source) IN ('bluray', 'blu-ray', 'bd')
                         AND COALESCE(is_remux, 0) = 1 THEN ' Remux'
                    ELSE ''
                END
        END
        "#,
    )
    .execute(db)
    .await
    .ok();

    // `episode_grab_history` replacements. Two-pass approach, since
    // SQLite REPLACE is a dumb substring swap and can't reorder tokens
    // around a variable-width resolution in one shot:
    //
    //   Pass A — normalize legacy space-joined tokens to the
    //            intermediate dash-joined form ("BluRay BDMV 1080p" →
    //            "BD-RAW-1080p", "WEB-DL 1080p" → "WEB-1080p", etc.).
    //            Only the BluRay BDMV/Remux/plain and WEB variants need
    //            ordering care: the qualified BluRay patterns must fire
    //            before the generic "BluRay " prefix is stripped.
    //
    //   Pass B — reorder `BD-{RAW|Remux}-{res}` into the final
    //            Sonarr-parity `BD-{res} {RAW|Remux}` form. REPLACE
    //            needs one entry per supported resolution because it
    //            can't swap tokens generically.
    //
    // Straggler entries at the end:
    //  - "WEBRIP-" → "WEBRip-" fixes the all-caps form from the pre-
    //    Sonarr-parity rename pass.
    //  - `WEBDL-<res>` → `WEB-<res>` (one per `Resolution::as_str()`
    //    output: 480p/576p/720p/1080p/2160p) catches DBs that booted
    //    the intermediate `WEBDL-` build between issue #48's
    //    unification and this migration. Add a new resolution here
    //    if the `Resolution` enum ever gains one.
    for (old, new) in [
        // ── Pass A: legacy space-joined → intermediate dash form ──
        ("BluRay BDMV ", "BD-RAW-"),
        ("BluRay Remux ", "BD-Remux-"),
        ("BluRay ", "BD-"),
        // WebDl collapses to the bare "WEB" label (issue #48), so
        // legacy "WEB-DL 1080p" strings rewrite straight to the new
        // unified form, skipping the old "WEBDL-" intermediate.
        ("WEB-DL ", "WEB-"),
        ("WEBRip ", "WEBRip-"),
        ("Web ", "WEB-"),
        ("HDTV ", "HDTV-"),
        ("DVD ", "DVD-"),
        ("TV ", "TV-"),
        // ── Pass B: intermediate dash form → Sonarr-parity reorder ──
        ("BD-RAW-480p", "BD-480p RAW"),
        ("BD-RAW-576p", "BD-576p RAW"),
        ("BD-RAW-720p", "BD-720p RAW"),
        ("BD-RAW-1080p", "BD-1080p RAW"),
        ("BD-RAW-2160p", "BD-2160p RAW"),
        ("BD-Remux-480p", "BD-480p Remux"),
        ("BD-Remux-576p", "BD-576p Remux"),
        ("BD-Remux-720p", "BD-720p Remux"),
        ("BD-Remux-1080p", "BD-1080p Remux"),
        ("BD-Remux-2160p", "BD-2160p Remux"),
        // Case-fix stragglers from the intermediate all-caps form.
        ("WEBRIP-", "WEBRip-"),
        // Issue #48: collapse any stored `WEBDL-<res>` strings
        // (written by prior builds) to `WEB-<res>`. Needs one entry
        // per resolution because REPLACE is a dumb substring swap.
        ("WEBDL-480p", "WEB-480p"),
        ("WEBDL-576p", "WEB-576p"),
        ("WEBDL-720p", "WEB-720p"),
        ("WEBDL-1080p", "WEB-1080p"),
        ("WEBDL-2160p", "WEB-2160p"),
    ] {
        let like_pat = format!("%{}%", old);
        let _ = sqlx::query(
            "UPDATE episode_grab_history
             SET quality_tag = REPLACE(quality_tag, ?, ?)
             WHERE quality_tag LIKE ?",
        )
        .bind(old)
        .bind(new)
        .bind(like_pat)
        .execute(db)
        .await;
    }

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS scheduled_task_runs (
            task_key TEXT PRIMARY KEY,
            display_name TEXT NOT NULL DEFAULT '',
            schedule_label TEXT NOT NULL DEFAULT '',
            enabled INTEGER NOT NULL DEFAULT 1,
            last_started_at DATETIME,
            last_finished_at DATETIME,
            last_status TEXT NOT NULL DEFAULT 'idle',
            last_detail TEXT NOT NULL DEFAULT '',
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    // Backfill folder_name for any existing series that have an empty value.
    // Uses English title → Romaji → title, with filesystem-unsafe chars sanitized.
    let empty_folder_rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT id, title, title_romaji, title_english FROM series WHERE folder_name = '' OR folder_name IS NULL",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    for (id, title, title_romaji, title_english) in &empty_folder_rows {
        let best = if !title_english.is_empty() {
            title_english.as_str()
        } else if !title_romaji.is_empty() {
            title_romaji.as_str()
        } else {
            title.as_str()
        };
        let folder = crate::services::media::sanitize_folder_name(best);
        if !folder.is_empty() {
            let _ = sqlx::query("UPDATE series SET folder_name = ? WHERE id = ?")
                .bind(&folder)
                .bind(id)
                .execute(db)
                .await;
        }
    }

    // #23 — Custom search tokens + release-group restriction.
    // Global defaults live on `config`; per-series overrides live on
    // `series`. Both are plain text for flexibility — the user pastes
    // whatever Nyaa query syntax they want (`bd`, `1080p`, `h.264`)
    // and the nyaa query builder appends it verbatim after the title.
    // Empty string means "no override / no tokens", which is the
    // existing behavior.
    sqlx::query(
        "ALTER TABLE config ADD COLUMN default_custom_query_tokens TEXT NOT NULL DEFAULT ''",
    )
    .execute(db)
    .await
    .ok();
    sqlx::query("ALTER TABLE series ADD COLUMN custom_query_tokens TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Rename `*_to_group` → `*_to_uploader` with full recovery for the
    // DBs that landed in a half-migrated state from PR #37's first-
    // pass migration (which added the new column before renaming, so
    // `ADD` succeeded and the subsequent `RENAME` failed as "duplicate
    // column" — leaving the user's uploader value stranded in an
    // orphan legacy column alongside an empty new one).
    //
    // `reconcile_column_rename` handles the four possible
    // states — legacy-only, new-only, both, neither — in the order
    // that makes each a one-shot forward move without data loss.
    reconcile_column_rename(
        db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;
    reconcile_column_rename(db, "series", "restrict_to_group", "restrict_to_uploader").await;

    // #30 — Cumulative episode count of the shortest TV-format PREQUEL
    // chain. Used at search time to accept absolute-numbered Nyaa
    // releases against a relative-numbered AL cour target (e.g. target
    // JJK S3 E9 matches "[SubsPlease] Jujutsu Kaisen - 56" because
    // S1(24) + S2(23) = 47 and 47 + 9 = 56). Populated by
    // `metadata_sync::refresh_series_metadata` after the relation graph
    // has been cached, and again at library-add time so first-searches
    // don't wait for the next refresh sweep.
    sqlx::query(
        "ALTER TABLE series ADD COLUMN cumulative_prior_episodes INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // SeaDex lookup cache, persisted across restarts. The in-memory cache
    // in `services::auto_search` already de-duplicates within a process,
    // but cold-boot RSS sweeps were re-fetching every series's SeaDex
    // entry on the first 24h cycle after every restart. Persisting the
    // 24h window to SQLite means a restart picks up where the cache left
    // off. Error-marked entries (5-min TTL) are deliberately NOT persisted
    // — they reflect upstream health, which restart should re-probe.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS seadex_lookup_cache (
            anilist_id INTEGER PRIMARY KEY,
            payload_json TEXT NOT NULL,
            cached_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(db)
    .await?;

    // Issue #83 — interactive file-picker trigger policy. Values:
    // `batches_only` (default) or `never`. Pre-existing DBs get the
    // default on-read through the `NOT NULL DEFAULT 'batches_only'`
    // column, so no per-row backfill is needed.
    sqlx::query(
        "ALTER TABLE config ADD COLUMN grab_preview_mode TEXT NOT NULL DEFAULT 'batches_only'",
    )
    .execute(db)
    .await
    .ok();

    // Issue #62 — watch-list sync interval in minutes. Default
    // 30 (decision #5). Range 15..=10080 enforced at the settings-
    // save handler and clamped again on read by the supervised task,
    // so a hand-edited DB row can't push the cadence into a value
    // that would pressure provider rate limits or effectively
    // disable sync.
    sqlx::query(
        "ALTER TABLE config ADD COLUMN external_sync_interval_minutes INTEGER NOT NULL DEFAULT 30",
    )
    .execute(db)
    .await
    .ok();

    // Issue #28 — torznab/newznab indexer registry. Foundation
    // for v1.5's multi-indexer support; the TorznabIndexer impl
    // that consumes these rows lands alongside it. Schema:
    //   - `kind` is `'torznab' | 'newznab'`. Nyaa stays out-of-band
    //     (decision #1) and never gets a row here.
    //   - `priority` follows Sonarr's convention (lower = preferred,
    //     range 1-50, default 25). Drives auto-search dedup
    //     attribution + interactive search row tiebreaks + fan-out
    //     order (decision #3 + plan §"Indexer priority semantics").
    //   - `is_private_tracker` is explicit-with-smart-defaults
    //     (decision #4). The settings form pre-fills via Prowlarr's
    //     native `/api/v1/indexer` privacy field when the URL is
    //     Prowlarr; Jackett / raw torznab pre-fill `private` as the
    //     safe fallback. User confirms before save.
    //   - `caps_json` / `caps_refreshed_at` cache the indexer's
    //     `t=caps` response with a 7-day lazy TTL (decision #6).
    //   - `request_timeout_secs` is per-indexer override over the
    //     30s default (decision #7); NULL means use the default,
    //     overridable via RYOKAN_INDEXER_DEFAULT_TIMEOUT_SECS env.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS indexers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            url TEXT NOT NULL,
            api_key TEXT NOT NULL DEFAULT '',
            priority INTEGER NOT NULL DEFAULT 25,
            enabled INTEGER NOT NULL DEFAULT 1,
            is_private_tracker INTEGER NOT NULL DEFAULT 0,
            seed_ratio REAL,
            seed_time_minutes INTEGER,
            min_seeders INTEGER NOT NULL DEFAULT 1,
            request_timeout_secs INTEGER,
            caps_json TEXT NOT NULL DEFAULT '',
            caps_refreshed_at INTEGER,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        )
        "#,
    )
    .execute(db)
    .await?;

    // Issue #28 — `grabbed_torrents.indexer_id` records which
    // indexer surfaced each grab. Nullable, no real FK: SQLite
    // can't add a FOREIGN KEY constraint via ALTER TABLE, so the
    // column is structurally unconstrained. The
    // `settings_indexers_delete` handler (PR #107 round-2 fix #3)
    // NULLs out matching rows explicitly to keep grab history
    // readable post-delete. NULL means either "pre-#28 grab,
    // assume Nyaa" or "indexer was deleted after this grab" —
    // both shapes are equivalent for the upgrade sweep, which
    // treats NULL as "no per-indexer rules apply."
    sqlx::query("ALTER TABLE grabbed_torrents ADD COLUMN indexer_id INTEGER")
        .execute(db)
        .await
        .ok();

    // Multi-client refactor — track which `download_clients` row the grab
    // was dispatched to. Drives post-processing's `list_scoped` /
    // `get_files` routing so a torrent landed on the seedbox isn't
    // hunted for on the local qBit. NULL on legacy rows (pre-multi-client)
    // and on grabs whose pin resolution returned None (pool empty
    // before user added a client). Post-processing falls back to the
    // current default on NULL.
    sqlx::query("ALTER TABLE grabbed_torrents ADD COLUMN download_client_id INTEGER")
        .execute(db)
        .await
        .ok();

    // Issue #28 — `grabbed_torrents.respect_seed_rules` flags
    // grabs whose torrents have per-torrent seed-ratio / seed-time
    // rules applied at add time. Delete paths (manual delete,
    // upgrade-replacement) skip torrents with this flag so the
    // client can finish seeding to the per-tracker target before
    // teardown. Nyaa grabs default 0; PT grabs default 1.
    sqlx::query(
        "ALTER TABLE grabbed_torrents ADD COLUMN respect_seed_rules INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // Source-side paths recorded at import time. JSON array of the
    // file paths Ryokan hardlinked / copied / moved FROM during
    // post-processing, in the local-translated form (Ryokan's view
    // of the path, not the client's). Used by the per-episode delete
    // and series-remove paths to clean up the original files when
    // SAB's `del_files=1` doesn't reach them — SAB's reported
    // history `storage` can be the parent complete dir while the
    // actual extracted .mkv lives in a subfolder created by the rar
    // archive contents. The inode-based fallback in
    // `delete_episode_file` covers hardlink mode; this column
    // covers copy mode (different inodes) and move mode (source no
    // longer at SAB's path) uniformly. NULL on legacy rows and on
    // grabs whose import didn't succeed.
    sqlx::query("ALTER TABLE grabbed_torrents ADD COLUMN imported_source_paths TEXT")
        .execute(db)
        .await
        .ok();

    // Issue #28 — per-series PT-upgrade opt-in. The UI + sweep
    // filter land in a later change; the column lands here up front
    // so the search code can filter on it without a chained
    // migration. Default FALSE so a user upgrading from 1.4.x sees
    // no change in behavior.
    sqlx::query("ALTER TABLE series ADD COLUMN allow_pt_upgrades INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // Issue #28 — autobrr push endpoint API key. Empty string
    // until the user generates one via Settings → Connections →
    // autobrr. Empty disables the webhook entirely; the
    // `/api/webhook/autobrr` middleware rejects when the key is
    // empty so a fresh install doesn't accept anonymous pushes.
    sqlx::query("ALTER TABLE config ADD COLUMN autobrr_api_key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Multi-client routing — one row per *configured* client, not
    // one per kind. A user can run "Local qBit" + "Seedbox Deluge"
    // simultaneously, with per-indexer pinning so AnimeBytes grabs
    // route to the seedbox while Nyaa stays on the local box. The
    // legacy `config.active_client + qbit_url etc.` columns stay
    // for one release as rollback safety; runtime reads from this
    // table exclusively after the schema_migrations-gated backfill
    // below seeds the first row.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS download_clients (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            url TEXT NOT NULL,
            username TEXT NOT NULL DEFAULT '',
            password TEXT NOT NULL DEFAULT '',
            label TEXT NOT NULL DEFAULT '',
            download_path TEXT NOT NULL DEFAULT '',
            enabled INTEGER NOT NULL DEFAULT 1,
            is_default INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        )",
    )
    .execute(db)
    .await
    .ok();

    // Per-indexer client pinning. NULL means "use the row marked
    // is_default=1 in download_clients." `ON DELETE SET NULL`
    // can't be added via ALTER on SQLite, so the
    // `settings_download_clients_delete` handler NULLs matching
    // `indexers.download_client_id` rows in the same transaction
    // (mirrors how the indexer-delete handler NULLs
    // `grabbed_torrents.indexer_id`).
    sqlx::query("ALTER TABLE indexers ADD COLUMN download_client_id INTEGER")
        .execute(db)
        .await
        .ok();

    // Nyaa is out-of-band (no `indexers` row), so its pin lives on
    // the singleton config row. Same NULL-means-default semantics.
    sqlx::query("ALTER TABLE config ADD COLUMN nyaa_download_client_id INTEGER")
        .execute(db)
        .await
        .ok();

    // Backfill — read the legacy `config.active_client` discriminator
    // + the per-kind URL/credentials columns and seed one
    // `download_clients` row marked is_default=1. Idempotent via
    // `schema_migrations` so a user who later adds more clients
    // doesn't get the legacy row re-created on every boot.
    crate::models::group_source_map::ensure_schema_migrations_table(db)
        .await
        .ok();
    if !crate::models::group_source_map::migration_already_applied(
        db,
        "multi_client_seed_default_v1",
    )
    .await
    .unwrap_or(false)
    {
        // Read the legacy fields per-kind. Each block falls back to
        // empty defaults on any failure (column missing from a
        // partially-applied earlier migration, hand-edited schema,
        // transient I/O). Pre-fix this whole block was gated on a
        // 4-arm `if let (Some, Some, Some, Some, Some)` — any one
        // failure skipped the entire backfill **and** didn't mark
        // the migration applied, so it retried every boot. Now each
        // arm degrades independently and the marker fires once
        // we've gotten this far, regardless of which legacy slots
        // were readable. Per the code-review correction.
        let legacy_qbit = sqlx::query_as::<_, (String, String, String, String, String, String)>(
            "SELECT active_client, qbit_url, qbit_user, qbit_pass, qbit_category, qbit_download_path \
             FROM config WHERE id = 1",
        )
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        let legacy_deluge = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT deluge_url, deluge_password, deluge_label, deluge_download_path \
             FROM config WHERE id = 1",
        )
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        let legacy_tx = sqlx::query_as::<_, (String, String, String, String, String)>(
            "SELECT transmission_url, transmission_user, transmission_password, \
                    transmission_label, transmission_download_path \
             FROM config WHERE id = 1",
        )
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        let legacy_rt = sqlx::query_as::<_, (String, String, String, String, String)>(
            "SELECT rtorrent_url, rtorrent_user, rtorrent_password, rtorrent_label, \
                    rtorrent_download_path \
             FROM config WHERE id = 1",
        )
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

        let (active, q_url, q_user, q_pass, q_cat, q_dp) = legacy_qbit;
        let (d_url, d_pass, d_label, d_dp) = legacy_deluge;
        let (t_url, t_user, t_pass, t_label, t_dp) = legacy_tx;
        let (r_url, r_user, r_pass, r_label, r_dp) = legacy_rt;

        let (name, kind, url, username, password, label, download_path) = match active.as_str() {
            "deluge" => (
                "Deluge",
                "deluge",
                d_url,
                String::new(),
                d_pass,
                d_label,
                d_dp,
            ),
            "transmission" => (
                "Transmission",
                "transmission",
                t_url,
                t_user,
                t_pass,
                t_label,
                t_dp,
            ),
            "rtorrent" => ("rTorrent", "rtorrent", r_url, r_user, r_pass, r_label, r_dp),
            _ => (
                "qBittorrent",
                "qbittorrent",
                q_url,
                q_user,
                q_pass,
                q_cat,
                q_dp,
            ),
        };
        // Mark the migration applied in its own transaction, separate
        // from the seed insert below, so a failure of the seed tx
        // doesn't trap us in a boot-loop where the read fires every
        // restart. The marker tx itself is still best-effort
        // (`if let Ok(mut tx) = db.begin().await`) — `db.begin` doesn't
        // typically transient-fail in a way the next attempt would
        // succeed at, so unwrapping wouldn't change behavior in
        // practice; we leave it gated to keep the migration path
        // panic-free under any startup contention. The seed itself is
        // also best-effort — a missed legacy URL re-creates the
        // pre-multi-client gap, but the user can fix that from
        // Settings → Connections; trapping startup behind the seed
        // transaction would be worse.
        if let Ok(mut tx) = db.begin().await {
            let _ = crate::models::group_source_map::mark_migration_applied(
                &mut tx,
                "multi_client_seed_default_v1",
            )
            .await;
            let _ = tx.commit().await;
        }
        // Only seed when the legacy slot was actually configured
        // (URL non-empty). A fresh-install user who never picked a
        // client gets no auto-seeded row; they'll add one via the
        // new Settings UI.
        if !url.is_empty()
            && let Ok(mut tx) = db.begin().await
        {
            let _ = sqlx::query(
                "INSERT INTO download_clients
                 (name, kind, url, username, password, label, download_path, enabled, is_default)
                 VALUES (?, ?, ?, ?, ?, ?, ?, 1, 1)",
            )
            .bind(name)
            .bind(kind)
            .bind(url)
            .bind(username)
            .bind(password)
            .bind(label)
            .bind(download_path)
            .execute(&mut *tx)
            .await;
            let _ = tx.commit().await;
        }
    }

    // Multi-RSS — user-configured RSS feeds (Option A). Custom
    // feeds beyond Nyaa-direct: per-uploader Nyaa filters, SubsPlease's
    // direct per-quality feeds, indexer-of-the-week aggregators, etc.
    // The sync loop fetches every enabled row each tick and merges
    // items into the same `rss_seen` dedup pool that already keys on
    // info_hash / GUID. `download_client_id` lets a feed pin to a
    // specific client (e.g. a public-feed grab routes to local qBit
    // while a PT-indexer-RSS feed routes to the seedbox); NULL falls
    // through to the default at grab time.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS rss_feeds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            url TEXT NOT NULL UNIQUE,
            enabled INTEGER NOT NULL DEFAULT 1,
            download_client_id INTEGER REFERENCES download_clients(id) ON DELETE SET NULL,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        )
        "#,
    )
    .execute(db)
    .await?;

    // Multi-RSS — Option B: let an enabled torznab/newznab indexer
    // contribute its `?t=tvsearch&extended=1` (or `&t=search` newznab
    // RSS) endpoint to the per-tick fan-out. Default 0 (off) so the
    // existing search-only indexer fan-out is unaffected; users opt
    // in per-indexer via the Settings → Indexers row toggle.
    sqlx::query("ALTER TABLE indexers ADD COLUMN rss_enabled INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // multi-rss commit E — observability fields on each per-source
    // row so the Settings UI can render "last polled 3m ago, 18
    // items" inline. Nyaa is the singleton out-of-band path and
    // has no row to write to; users see Nyaa status via System →
    // Logs filtered to LogCategory::Rss (decision #11).
    sqlx::query("ALTER TABLE indexers ADD COLUMN rss_last_polled_at INTEGER")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE indexers ADD COLUMN rss_last_poll_error TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE indexers ADD COLUMN rss_last_item_count INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();
    // Per-indexer category override (comma-separated torznab ids). Blank
    // asks for what the series needs (5070, plus Movies or XXX by
    // format and adult flag) and falls back to what the indexer's caps
    // report; a value is sent as written, Sonarr-style.
    sqlx::query("ALTER TABLE indexers ADD COLUMN categories TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // multi-rss commit E — rename rss_feeds → direct_rss_feeds
    // (the latter matches the plan's terminology and disambiguates
    // from the indexer-RSS path). SQLite's RENAME preserves all
    // columns including the FK on download_client_id.
    sqlx::query("ALTER TABLE rss_feeds RENAME TO direct_rss_feeds")
        .execute(db)
        .await
        .ok();
    // Per-feed observability + protocol detection columns.
    // `detected_protocol` is populated by the Test button (commit
    // G) — empty until the first successful test, after which the
    // pin save path enforces protocol match. `request_timeout_secs`
    // mirrors the per-indexer override.
    sqlx::query("ALTER TABLE direct_rss_feeds ADD COLUMN request_timeout_secs INTEGER")
        .execute(db)
        .await
        .ok();
    sqlx::query(
        "ALTER TABLE direct_rss_feeds ADD COLUMN detected_protocol TEXT NOT NULL DEFAULT ''",
    )
    .execute(db)
    .await
    .ok();
    sqlx::query("ALTER TABLE direct_rss_feeds ADD COLUMN last_polled_at INTEGER")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE direct_rss_feeds ADD COLUMN last_poll_error TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query(
        "ALTER TABLE direct_rss_feeds ADD COLUMN last_item_count INTEGER NOT NULL DEFAULT 0",
    )
    .execute(db)
    .await
    .ok();

    // multi-rss commit E — per-source dedup scoping on rss_seen.
    // Without this, three sources can produce identical numeric
    // GUIDs (different sites' internal IDs) and a SubsPlease item
    // would silently dedup against an unrelated Nyaa item.
    //
    //   (source = 'nyaa',    source_id = NULL) → legacy Nyaa entries
    //   (source = 'indexer', source_id = N)   → indexer N's RSS
    //   (source = 'direct',  source_id = N)   → direct_rss_feeds N
    //
    // The `source` column already exists on `rss_seen` (added
    // pre-multi-rss with DEFAULT 'rss'); we repurpose its
    // vocabulary rather than colliding on a duplicate ADD COLUMN.
    // Backfill legacy `'rss'` values to `'nyaa'` so the dedup
    // query keys consistently. `source_id` is the new column.
    sqlx::query("UPDATE rss_seen SET source = 'nyaa' WHERE source = 'rss'")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE rss_seen ADD COLUMN source_id INTEGER")
        .execute(db)
        .await
        .ok();
    // SQLite treats NULL as distinct in the index, so
    // `source = 'nyaa' AND source_id IS NULL AND item_key = ?`
    // uses the index correctly without a COALESCE fallback.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_rss_seen_source_key \
         ON rss_seen (source, source_id, item_key)",
    )
    .execute(db)
    .await
    .ok();

    // multi-rss commit E — master kill switch for the whole RSS
    // sync. Off = no fetches at all (Nyaa + indexers + direct).
    // Default 1 so existing installs keep their current behavior.
    // `config.rss_enabled` retains its v1 semantics (Nyaa-only
    // flag); see decision #8.
    sqlx::query("ALTER TABLE config ADD COLUMN rss_master_enabled INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // Nyaa-specific RSS opt-out. Default 0 so existing
    // installs keep polling Nyaa; user flips on when they only want
    // indexer-RSS / direct-RSS feeds polled. Distinct from
    // `rss_master_enabled` (which kills the whole sync) and
    // `rss_enabled` (which retains its v1 semantics — Nyaa-only flag).
    sqlx::query("ALTER TABLE config ADD COLUMN disable_nyaa_rss INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    // 2026-04-28 — `LogCategory::QBit` → `LogCategory::DownloadClient`
    // rename. The variant covered every torrent client (and SAB) since
    // the multi-client refactor, but the persisted wire string was
    // still `qbit`, so System → Logs filtered to "qBittorrent" was
    // showing Deluge / Transmission / rTorrent / SAB rows too. Rewrite
    // existing rows in place; the new code persists the new string,
    // so the UPDATE is a one-shot. Gated through the schema_migrations
    // ledger so it doesn't run a per-boot table scan over the logs
    // table (which is bounded by cleanup but still grows between
    // rotations). Failure to apply is non-fatal — old rows stay under
    // the legacy filter and `from_str` still accepts "qbit" as a
    // backward-compat alias.
    {
        use crate::models::group_source_map::{
            ensure_schema_migrations_table, mark_migration_applied, migration_already_applied,
        };
        const ID: &str = "logs_category_qbit_to_download_client_v1";
        ensure_schema_migrations_table(db).await.ok();
        if !migration_already_applied(db, ID).await.unwrap_or(false) {
            // Run the UPDATE + ledger insert in one transaction so a
            // crash mid-migration doesn't leave the rows half-rewritten
            // with the ledger marked applied. SQLite's default journal
            // mode is fine here — this is one UPDATE + one INSERT.
            if let Ok(mut tx) = db.begin().await {
                let upd = sqlx::query(
                    "UPDATE logs SET category = 'download_client' WHERE category = 'qbit'",
                )
                .execute(&mut *tx)
                .await;
                if upd.is_ok() {
                    let _ = mark_migration_applied(&mut tx, ID).await;
                    let _ = tx.commit().await;
                }
            }
        }
    }

    // Manual search → grab auto-add toggle. Default 1 (ON) so the
    // search-page Grab button auto-adds matched series via AL when no
    // existing library row matches — pre-1.7 the no-match path was a
    // silent no-op (grab succeeded in the download client but no
    // library row was created). Users who want the legacy behavior can
    // flip it off in Settings → General.
    sqlx::query("ALTER TABLE config ADD COLUMN manual_search_auto_add INTEGER NOT NULL DEFAULT 1")
        .execute(db)
        .await
        .ok();

    // Issue #118 — outbound notification provider rows. One row per
    // configured provider (webhook / Discord / future), keyed by id.
    // `kind` is the trait-impl discriminator (`"webhook"` / `"discord"`)
    // and tells `services::notifications::rebuild_notification_providers_cache`
    // which trait impl to construct. `config_json` is provider-shape-
    // specific (URL + headers for webhook; webhook URL + username/avatar
    // for Discord). `enabled` is the row-level kill switch the Settings
    // UI flips. Schema is intentionally minimal — per-event toggling
    // lives in `notification_settings` rather than denormalized columns
    // here so the matrix stays sparse and adding a new event variant
    // is one-line.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS notification_providers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            config_json TEXT NOT NULL DEFAULT '{}',
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        )",
    )
    .execute(db)
    .await
    .ok();

    // Per-(provider, event_kind) opt-in matrix. `event_kind` is the
    // serde discriminator string (`"Grabbed"`, `"Imported"`, etc.) —
    // stored as TEXT because we need stable identifiers across schema
    // changes and don't want a hard FK to a Rust enum. Default-on
    // policy is applied at provider-creation time in the settings
    // handler (Grabbed / Imported / ImportFailed / ExternalSyncReLinkRequired
    // get rows seeded with enabled=1; everything else either gets
    // enabled=0 or simply isn't seeded — both shapes mean "don't fire").
    // ON DELETE CASCADE so removing a provider purges its matrix in one
    // step rather than the settings handler having to chain a delete.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS notification_settings (
            provider_id INTEGER NOT NULL REFERENCES notification_providers(id) ON DELETE CASCADE,
            event_kind  TEXT    NOT NULL,
            enabled     INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (provider_id, event_kind)
        )",
    )
    .execute(db)
    .await
    .ok();

    // Issue #114 — scoped API keys. `key` stores the plaintext
    // (UNIQUE-indexed for both fast O(log N) auth-path lookup and
    // collision dedup against 32-byte CSPRNG output). Plaintext
    // matches the storage shape of every other Ryokan integration
    // key (`config.sonarr_api_key`, `config.autobrr_api_key`,
    // `config.jellyfin_api_key` — all TEXT plaintext). Encrypting
    // just this one would be a defense-in-depth illusion when
    // those plaintext keys live alongside it in the same DB.
    //
    // `scopes` is a JSON array of scope strings (see
    // `models::api_key::ALL_SCOPES`); stored as TEXT because the
    // set is small enough that a real JSON1 query isn't worth the
    // cost. `last_used_at` is best-effort — touched after a
    // successful request match so users can identify abandoned
    // keys for cleanup.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS api_keys (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            key TEXT NOT NULL DEFAULT '' UNIQUE,
            scopes TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            last_used_at INTEGER,
            enabled INTEGER NOT NULL DEFAULT 1
        )
        "#,
    )
    .execute(db)
    .await?;

    // Idempotency for upgraders from the earlier hash+encrypted
    // schema. The new `key` column starts empty for those rows,
    // which means pre-rewrite keys can't authenticate against the
    // new code path — they're effectively invalidated and need to
    // be deleted + recreated. Acceptable in unmerged-dev land;
    // documented in the commit so any local tester knows to wipe
    // their `api_keys` rows after upgrading.
    sqlx::query("ALTER TABLE api_keys ADD COLUMN key TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    // Lookup index on the plaintext key so the per-request
    // middleware path (`SELECT ... WHERE key = ?`) is O(log N).
    // The UNIQUE on the column already creates an index, so this
    // CREATE INDEX is technically redundant — kept as a defensive
    // statement in case a future schema change drops the UNIQUE
    // constraint without remembering to add the index back.
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_api_keys_key ON api_keys (key)")
        .execute(db)
        .await
        .ok();

    // Issue #115/#116 follow-up — local cache of AniList airing
    // schedules. Sonarr stamps episode air dates on its local
    // `Episode.AirDateUtc` column at series-refresh time and serves
    // the calendar from a plain SQL range scan, never hitting the
    // upstream metadata provider on the hot path. We mirror that:
    // a 12h `airing_refresh` supervised task pulls AL's
    // `Page.airingSchedules` for every positive-AL-id series and
    // writes upcoming episodes here. The calendar then reads from
    // this table joined against `series` instead of round-tripping
    // to AL per-request, preserving the 30/min degraded budget.
    //
    // Series removal cascades (FK) so deleted series automatically
    // drop their stamped airings.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS episode_airings (
            series_id        INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
            episode          INTEGER NOT NULL,
            airing_at        INTEGER NOT NULL,
            duration_minutes INTEGER NOT NULL DEFAULT 24,
            refreshed_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            PRIMARY KEY (series_id, episode)
        )",
    )
    .execute(db)
    .await
    .ok();
    // Range-scan index for the calendar's primary `WHERE airing_at
    // BETWEEN @from AND @to` query. Mirrors Sonarr's
    // `idx_episodes_air_date_utc`.
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_episode_airings_at ON episode_airings (airing_at)")
        .execute(db)
        .await
        .ok();

    // Recycle bin (#123). Empty path = disabled (permanent deletes, the
    // pre-1.8 behavior); age 0 = never auto-purge.
    sqlx::query("ALTER TABLE config ADD COLUMN recycle_bin_path TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();
    sqlx::query("ALTER TABLE config ADD COLUMN recycle_bin_age_days INTEGER NOT NULL DEFAULT 14")
        .execute(db)
        .await
        .ok();

    // Naming templates (#124). Defaults are the pre-#124 hardcoded
    // layout, so existing installs see no change. Built with format!
    // so the column default can never drift from the constant.
    for (column, default) in [
        (
            "series_folder_format",
            crate::services::naming::DEFAULT_SERIES_FOLDER_FORMAT,
        ),
        (
            "season_folder_format",
            crate::services::naming::DEFAULT_SEASON_FOLDER_FORMAT,
        ),
        (
            "episode_file_format",
            crate::services::naming::DEFAULT_EPISODE_FILE_FORMAT,
        ),
    ] {
        // A default containing an apostrophe would otherwise break
        // the statement, and `.ok()` would hide that until every
        // `get_config` SELECT failed on the missing column.
        let default = default.replace('\'', "''");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE config ADD COLUMN {column} TEXT NOT NULL DEFAULT '{default}'"
        )))
        .execute(db)
        .await
        .ok();
    }

    // Scheduled backups (#126). Disabled by default: users with their
    // own backup pipeline for the data dir shouldn't get a second copy.
    for sql in [
        "ALTER TABLE config ADD COLUMN backup_schedule TEXT NOT NULL DEFAULT 'disabled'",
        "ALTER TABLE config ADD COLUMN backup_directory TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE config ADD COLUMN backup_retention_count INTEGER NOT NULL DEFAULT 7",
        "ALTER TABLE config ADD COLUMN backup_include_artwork INTEGER NOT NULL DEFAULT 0",
    ] {
        sqlx::query(sql).execute(db).await.ok();
    }

    // Issue #219 — AniList `isAdult`. Stamped by the metadata refresh
    // (`series::set_is_adult`); default 0 so existing rows read as
    // not-adult until their next refresh. Nyaa lists adult releases on
    // sukebei, which Ryokan does not search, so the flag mostly serves
    // to explain an empty auto-search.
    sqlx::query("ALTER TABLE series ADD COLUMN is_adult INTEGER NOT NULL DEFAULT 0")
        .execute(db)
        .await
        .ok();

    Ok(())
}

#[cfg(test)]
mod tests;
