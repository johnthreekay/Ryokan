use super::*;

/// Regression: a legacy install has `episode_grab_history.torrent_name`
/// populated with the release title for every historical grab. The
/// column-rename path previously ran
///   RENAME torrent_name → file_name (.ok())
///   ADD COLUMN file_name TEXT (.ok())
/// back-to-back. If the RENAME failed for any reason (DB lock, FK
/// quirk, I/O hiccup) the subsequent ADD silently created an empty
/// `file_name` column on top of intact `torrent_name` data and every
/// prior row's release title was effectively lost — `.ok()` on both
/// statements meant no log line, no error, nothing to alert the
/// operator.
///
/// This test exercises the happy path: pre-create the table with the
/// legacy schema, stuff a row into it, run migrate, confirm the row's
/// file_name now carries what torrent_name held.
#[tokio::test]
async fn migrate_renames_legacy_torrent_name_to_file_name_preserving_data() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");

    // Pre-create episode_grab_history with the legacy schema (column
    // is `torrent_name`, no `file_name`). CREATE TABLE IF NOT EXISTS
    // inside migrate() will then skip this table and migrate() will
    // reach the rename branch under test.
    sqlx::query(
        r#"
        CREATE TABLE episode_grab_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            series_id INTEGER NOT NULL,
            episode_number INTEGER NOT NULL,
            quality_tag TEXT NOT NULL DEFAULT '',
            release_title TEXT NOT NULL DEFAULT '',
            release_group TEXT NOT NULL DEFAULT '',
            torrent_name TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT 'grabbed',
            grabbed_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(&db)
    .await
    .expect("pre-create legacy table");

    sqlx::query(
        "INSERT INTO episode_grab_history
             (series_id, episode_number, quality_tag, release_title, release_group, torrent_name)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(1_i64)
    .bind(1_i32)
    .bind("WEBDL-1080p")
    .bind("[Group] Show - 01 [WEB-DL 1080p].mkv")
    .bind("Group")
    .bind("[Group] Show - 01 [WEB-DL 1080p].mkv")
    .execute(&db)
    .await
    .expect("insert legacy row");

    migrate(&db).await.expect("migrate must succeed");

    // After migrate, the data that lived in `torrent_name` must now be
    // in `file_name`. If the rename failed and the defensive ADD
    // branch ran instead, this value would be empty (the default).
    let file_name: String =
        sqlx::query_scalar("SELECT file_name FROM episode_grab_history WHERE id = 1")
            .fetch_one(&db)
            .await
            .expect("row 1 must still exist");
    assert_eq!(file_name, "[Group] Show - 01 [WEB-DL 1080p].mkv");

    // And the old column should no longer be there (RENAME moved it,
    // didn't duplicate it).
    assert!(!column_exists(&db, "episode_grab_history", "torrent_name").await);
}

/// PR #37's first migration attempt ran ADD-then-RENAME for the
/// `restrict_to_group` → `restrict_to_uploader` rename, so any DB
/// that booted that build ended up with both columns: the legacy
/// one populated with the user's uploader value, the new one
/// empty. The fix for that ships the recovery pass tested here
/// — on a DB with both columns present, the user's value must
/// land in the new column and the legacy column must drop.
#[tokio::test]
async fn reconcile_rename_recovers_half_migrated_restrict_to_group() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");

    // Simulate the PR #37 v1 broken state: pre-create `config`
    // with BOTH columns, legacy populated, new empty.
    sqlx::query(
        r#"CREATE TABLE config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            default_restrict_to_group     TEXT NOT NULL DEFAULT '',
            default_restrict_to_uploader  TEXT NOT NULL DEFAULT ''
        )"#,
    )
    .execute(&db)
    .await
    .expect("create legacy config");
    sqlx::query(
        "INSERT INTO config (id, default_restrict_to_group, default_restrict_to_uploader)
         VALUES (1, 'SubsPlease', '')",
    )
    .execute(&db)
    .await
    .expect("seed legacy row");

    reconcile_column_rename(
        &db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;

    let uploader: String =
        sqlx::query_scalar("SELECT default_restrict_to_uploader FROM config WHERE id = 1")
            .fetch_one(&db)
            .await
            .expect("fetch uploader");
    assert_eq!(
        uploader, "SubsPlease",
        "user's uploader value must be copied forward into the new column"
    );

    // Legacy column should be gone after the reconcile.
    assert!(
        !column_exists(&db, "config", "default_restrict_to_group").await,
        "orphan legacy column must be dropped once data has been copied"
    );
}

/// Legacy-only state (DB migrated from a build predating PR #37):
/// rename the column in place, keep the data.
#[tokio::test]
async fn reconcile_rename_brings_legacy_column_forward() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");

    sqlx::query(
        r#"CREATE TABLE config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            default_restrict_to_group TEXT NOT NULL DEFAULT ''
        )"#,
    )
    .execute(&db)
    .await
    .expect("create legacy config");
    sqlx::query("INSERT INTO config (id, default_restrict_to_group) VALUES (1, 'SubsPlease')")
        .execute(&db)
        .await
        .expect("seed legacy row");

    reconcile_column_rename(
        &db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;

    let uploader: String =
        sqlx::query_scalar("SELECT default_restrict_to_uploader FROM config WHERE id = 1")
            .fetch_one(&db)
            .await
            .expect("fetch uploader");
    assert_eq!(uploader, "SubsPlease");
    assert!(!column_exists(&db, "config", "default_restrict_to_group").await);
}

/// Both columns, new column already populated — user's live value
/// must win over the stale legacy value. Edge case: the old
/// rename attempt was half-successful somehow (or a user
/// manually edited the new column).
#[tokio::test]
async fn reconcile_rename_does_not_overwrite_populated_new_column() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");

    sqlx::query(
        r#"CREATE TABLE config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            default_restrict_to_group     TEXT NOT NULL DEFAULT '',
            default_restrict_to_uploader  TEXT NOT NULL DEFAULT ''
        )"#,
    )
    .execute(&db)
    .await
    .expect("create legacy config");
    sqlx::query(
        "INSERT INTO config (id, default_restrict_to_group, default_restrict_to_uploader)
         VALUES (1, 'StaleLegacy', 'LiveNew')",
    )
    .execute(&db)
    .await
    .expect("seed row");

    reconcile_column_rename(
        &db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;

    let uploader: String =
        sqlx::query_scalar("SELECT default_restrict_to_uploader FROM config WHERE id = 1")
            .fetch_one(&db)
            .await
            .expect("fetch uploader");
    assert_eq!(
        uploader, "LiveNew",
        "non-empty new column must not be overwritten by stale legacy"
    );
}

/// Fresh install — neither column exists yet. Reconcile must
/// ADD the new column with the empty default.
#[tokio::test]
async fn reconcile_rename_adds_new_column_on_fresh_install() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");

    sqlx::query(
        r#"CREATE TABLE config (
            id INTEGER PRIMARY KEY CHECK (id = 1)
        )"#,
    )
    .execute(&db)
    .await
    .expect("create bare config");
    sqlx::query("INSERT INTO config (id) VALUES (1)")
        .execute(&db)
        .await
        .expect("seed empty row");

    reconcile_column_rename(
        &db,
        "config",
        "default_restrict_to_group",
        "default_restrict_to_uploader",
    )
    .await;

    assert!(column_exists(&db, "config", "default_restrict_to_uploader").await);
    let uploader: String =
        sqlx::query_scalar("SELECT default_restrict_to_uploader FROM config WHERE id = 1")
            .fetch_one(&db)
            .await
            .expect("fetch uploader");
    assert_eq!(uploader, "", "fresh install starts with the default empty");
}

// ─── Idempotency + schema shape ──────────────────────────

async fn fresh_migrated_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    migrate(&pool).await.expect("migrate must succeed");
    pool
}

async fn table_exists(db: &SqlitePool, table: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .bind(table)
    .fetch_one(db)
    .await
    .unwrap_or(0)
        > 0
}

#[tokio::test]
async fn migrate_on_empty_db_succeeds() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    migrate(&db).await.expect("first migrate should succeed");
}

#[tokio::test]
async fn migrate_is_idempotent_on_second_invocation() {
    // The CREATE TABLE IF NOT EXISTS + ALTER TABLE … ADD COLUMN
    // with .ok() pattern is the whole point of in-code migrations
    // — running migrate() twice on the same pool must not error.
    // A refactor that swaps in a stricter IF NOT EXISTS variant
    // (or forgets .ok() on a new ALTER) would trip this test.
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    migrate(&db).await.expect("first migrate");
    migrate(&db)
        .await
        .expect("second migrate must also succeed");
}

#[tokio::test]
async fn migrate_creates_core_tables() {
    // Spot-check the load-bearing tables — adding a new one is
    // fine, but silently dropping one of these is the kind of
    // regression that lives undetected until a user reports data
    // loss. Limit the list to a handful of foundational ones
    // rather than every single table to avoid churn noise when
    // schema evolves.
    let db = fresh_migrated_pool().await;
    for table in [
        "users",
        "sessions",
        "config",
        "series",
        "grabbed_torrents",
        "grabbed_torrent_series",
        "episode_quality_tags",
        "episode_grab_history",
        "rss_seen",
        "logs",
    ] {
        assert!(
            table_exists(&db, table).await,
            "core table `{table}` missing after migrate"
        );
    }
}

#[tokio::test]
async fn foreign_keys_pragma_is_enabled_after_migrate() {
    // sqlx enables `PRAGMA foreign_keys = ON` by default, but
    // that default is a design dependency several migrations and
    // models rely on (rss_seen NO ACTION handling, series
    // CASCADE, etc.). Pinning here so a future sqlx upgrade that
    // changed the default would fail this test loudly rather
    // than silently corrupting child-table state.
    let db = fresh_migrated_pool().await;
    let pragma: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&db)
        .await
        .expect("PRAGMA foreign_keys should read");
    assert_eq!(pragma, 1, "foreign_keys pragma must be ON");
}

#[tokio::test]
async fn deleting_a_series_cascades_to_grabbed_torrents() {
    // Per the schema, grabbed_torrents.series_id has ON DELETE
    // CASCADE. Removing a series must take its grabs with it or
    // the DB ends up with orphaned grab rows that lookup paths
    // fail on.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (1, 'Show', 'Show', 'show')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 1")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO grabbed_torrents (series_id, hash, torrent_name, episode_numbers, state) \
         VALUES (?, 'h1', 'name', '[1]', 'pending')",
    )
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();

    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .expect("series delete should succeed");
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE series_id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        remaining, 0,
        "grabbed_torrents row must CASCADE with series"
    );
}

#[tokio::test]
async fn deleting_a_series_cascades_to_episode_quality_tags() {
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (2, 'Show', 'Show', 'show')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 2")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO episode_quality_tags (series_id, episode_number, quality_tag) \
         VALUES (?, 1, 'WEBDL-1080p')",
    )
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();

    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM episode_quality_tags WHERE series_id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn rss_seen_is_no_action_not_cascade_on_series() {
    // The FK policy for `rss_seen.series_id` is deliberately
    // `NO ACTION`, not CASCADE — the audit trail survives a
    // series deletion. series::remove is responsible for
    // NULL-ing out rss_seen.series_id BEFORE the series row
    // delete to satisfy the FK constraint. This test exercises
    // the "survive the delete" half of that contract — setting
    // series_id = NULL first, then deleting the series row,
    // then confirming rss_seen still has its bookkeeping row.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (3, 'Show', 'Show', 'show')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 3")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO rss_seen (item_key, series_id, series_title) VALUES (?, ?, ?)")
        .bind("guid-keep")
        .bind(series_id)
        .bind("Show")
        .execute(&db)
        .await
        .unwrap();

    // Per series::remove: NULL out the FK first, THEN delete the
    // series row. Without this two-step, the DELETE errors on FK
    // constraint failure.
    sqlx::query("UPDATE rss_seen SET series_id = NULL WHERE series_id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .expect("delete after NULL-out should succeed");

    // The audit row survives — same guid, series_id now NULL,
    // series_title kept for reference.
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rss_seen WHERE item_key = 'guid-keep'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        remaining, 1,
        "rss_seen audit row must survive series delete"
    );
}

#[tokio::test]
async fn direct_series_delete_without_null_out_fails_fk_constraint() {
    // The counter-test: attempting to delete a series without
    // first NULL-ing out `rss_seen.series_id` must fail with a
    // FK constraint error. Pins the invariant that series::remove
    // relies on — if a future refactor drops the NO ACTION policy
    // on rss_seen, this test catches it.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (4, 'Show', 'Show', 'show')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 4")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO rss_seen (item_key, series_id, series_title) VALUES (?, ?, ?)")
        .bind("guid-fail")
        .bind(series_id)
        .bind("Show")
        .execute(&db)
        .await
        .unwrap();

    let result = sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await;
    assert!(
        result.is_err(),
        "delete without NULL-out must fail FK constraint (got {result:?})"
    );
}

#[tokio::test]
async fn migrate_creates_schema_migrations_table() {
    let db = fresh_migrated_pool().await;
    // Not populated by `migrate()` directly — created on first
    // use by `ensure_schema_migrations_table` in group_source_map.
    // Run that seed path to ensure the table exists + is
    // writable.
    group_source_map::seed_defaults(&db)
        .await
        .expect("seed_defaults should succeed");
    assert!(
        table_exists(&db, "schema_migrations").await,
        "schema_migrations table should exist after seed pass"
    );
}

/// Stronger companion to `migrate_is_idempotent_on_second_invocation`
/// — that test only proves the second call doesn't *error*. This one
/// proves it doesn't silently *mutate* user-set values that the
/// initial migration backfilled.
///
/// The Jellyfin URL backfill is the load-bearing case: it derives
/// `jellyfin_url` from the legacy `jellyfin_host`/`jellyfin_port`/
/// `jellyfin_use_ssl` columns when `jellyfin_url` is empty. Without
/// the `WHERE jellyfin_url = ''` gate, a second migration call after
/// the user customized the derived URL would clobber it back to the
/// host-derived form. The gate is the actual idempotency guarantee
/// for every other UPDATE-style backfill in `migrate()`; this test
/// pins it for the most user-impactful one.
#[tokio::test]
async fn migrate_does_not_overwrite_user_values_on_second_invocation() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    migrate(&db).await.expect("first migrate");

    // migrate() doesn't seed the config row (`save_config` does that
    // on first settings-page write). Seed a minimal row directly so
    // the test exercises the user-customized-URL → second-migrate
    // path. Only the four jellyfin_* columns the backfill cares
    // about need real values.
    sqlx::query(
        "INSERT INTO config (id, jellyfin_url, jellyfin_host, jellyfin_port, jellyfin_use_ssl) \
         VALUES (1, 'https://my.real.jellyfin.example/jf', 'derived.example', '8096', 0)",
    )
    .execute(&db)
    .await
    .expect("seed config row with user-customized jellyfin_url");

    migrate(&db).await.expect("second migrate must succeed");

    let url: String = sqlx::query_scalar("SELECT jellyfin_url FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .expect("read jellyfin_url back");
    assert_eq!(
        url, "https://my.real.jellyfin.example/jf",
        "second migrate must not overwrite the user's custom jellyfin_url"
    );
}

/// Pin behavior of the typed-rename helper for the
/// `force_tmdb_fallback` → `force_kitsu_fallback` recovery path.
/// PR #37's regression shape (ADD-then-RENAME with `.ok()`) used to
/// leave a stray INTEGER column alongside the new one on a post-
/// migrated install, with the user's enable/disable bit stranded in
/// either column depending on which migrate() build ran first.
/// `reconcile_column_rename_typed` collapses every starting state to
/// "new column exists, value preserved, legacy column dropped".
#[tokio::test]
async fn reconcile_typed_rename_recovers_half_migrated_integer_column() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");

    // Simulate the half-migrated state: BOTH columns present, user's
    // bit in the legacy one, new one still at the default 0.
    sqlx::query(
        r#"CREATE TABLE config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            force_tmdb_fallback   INTEGER NOT NULL DEFAULT 0,
            force_kitsu_fallback  INTEGER NOT NULL DEFAULT 0
        )"#,
    )
    .execute(&db)
    .await
    .expect("create legacy config");
    sqlx::query(
        "INSERT INTO config (id, force_tmdb_fallback, force_kitsu_fallback) VALUES (1, 1, 0)",
    )
    .execute(&db)
    .await
    .expect("seed legacy bit");

    reconcile_column_rename_typed(
        &db,
        "config",
        "force_tmdb_fallback",
        "force_kitsu_fallback",
        "INTEGER NOT NULL DEFAULT 0",
        "= 0",
    )
    .await;

    let kitsu: i64 = sqlx::query_scalar("SELECT force_kitsu_fallback FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .expect("read new column");
    assert_eq!(
        kitsu, 1,
        "user's enable bit must move from legacy → new column"
    );
    assert!(
        !column_exists(&db, "config", "force_tmdb_fallback").await,
        "legacy column must be dropped, not duplicated"
    );
}

/// The fresh-install path: neither column exists yet, the typed
/// helper must add the new one with the caller-supplied INTEGER
/// declaration (not the TEXT default the string-flavored helper
/// uses).
#[tokio::test]
async fn reconcile_typed_rename_adds_integer_column_on_fresh_install() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    sqlx::query("CREATE TABLE config (id INTEGER PRIMARY KEY CHECK (id = 1))")
        .execute(&db)
        .await
        .expect("create empty config");
    sqlx::query("INSERT INTO config (id) VALUES (1)")
        .execute(&db)
        .await
        .expect("seed config row");

    reconcile_column_rename_typed(
        &db,
        "config",
        "force_tmdb_fallback",
        "force_kitsu_fallback",
        "INTEGER NOT NULL DEFAULT 0",
        "= 0",
    )
    .await;

    assert!(column_exists(&db, "config", "force_kitsu_fallback").await);
    let kitsu: i64 = sqlx::query_scalar("SELECT force_kitsu_fallback FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .expect("read new column");
    assert_eq!(kitsu, 0, "fresh-install default must be the integer 0");
    assert!(
        !column_exists(&db, "config", "force_tmdb_fallback").await,
        "fresh-install path must not create the legacy column",
    );
}

// ─── expansion (2026-04-28) ────────────────────────────────────
// Added to bring migrations test density up to the planned ~40 mark.
// Categories:
//   • Per-table-group existence (cache, seed, external-account tables)
//   • Default values land on fresh install
//   • Additional FK cascade + SET NULL behaviors not covered above
//   • Schema invariants (UNIQUE + CHECK constraints fire as designed)
//   • Index existence (the partial UNIQUE indexes are load-bearing
//     for hash-dedup + mal_id collision prevention)

#[tokio::test]
async fn migrate_creates_metadata_cache_tables() {
    // Provider chain (AL → Jikan/MAL → Kitsu) leans on these caches
    // hard. Dropping one of them turns the metadata fallback path
    // into an unbounded re-fetch loop.
    let db = fresh_migrated_pool().await;
    for table in [
        "episode_cache",
        "kitsu_episode_cache",
        "provider_metadata_cache",
        "provider_relations_cache",
        "provider_episode_metadata",
        "series_relations_cache",
        "series_episode_metadata",
        "media_probe_cache",
        "nyaa_description_cache",
    ] {
        assert!(
            table_exists(&db, table).await,
            "metadata cache table `{table}` missing after migrate"
        );
    }
}

#[tokio::test]
async fn migrate_creates_seed_and_settings_tables() {
    let db = fresh_migrated_pool().await;
    for table in [
        "custom_formats",
        "custom_format_scores",
        "group_source_map",
        "schema_migrations",
        "indexers",
        "download_clients",
        "scheduled_task_runs",
        "seadex_lookup_cache",
        "episode_monitor_state",
    ] {
        assert!(
            table_exists(&db, table).await,
            "seed/settings table `{table}` missing after migrate"
        );
    }
}

#[tokio::test]
async fn migrate_creates_external_account_tables() {
    // #62 (AL/MAL link). external_accounts holds the encrypted token
    // per linked provider; series_custom_lists records per-series
    // membership in each list. Both load-bearing for the watch-list
    // sync background task.
    let db = fresh_migrated_pool().await;
    for table in ["external_accounts", "series_custom_lists", "series_genres"] {
        assert!(
            table_exists(&db, table).await,
            "external-account table `{table}` missing after migrate"
        );
    }
}

#[tokio::test]
async fn migrate_creates_artwork_cache_tables() {
    // Artwork pipeline writes through these three tables; missing
    // any of them silently breaks cover/banner downloads.
    let db = fresh_migrated_pool().await;
    for table in ["artwork_cache", "image_blobs", "image_refs"] {
        assert!(
            table_exists(&db, table).await,
            "artwork table `{table}` missing after migrate"
        );
    }
}

// ─── Default-value tests ─────────────────────────────────────────────
//
// Each test reads the default that a fresh `INSERT INTO config (id)
// VALUES (1)` (only the singleton row) produces for one column. A
// regression that flips a default in the wrong direction (e.g.,
// title_language → 'native' on first boot) would silently surprise
// users.

async fn insert_default_config_row(db: &SqlitePool) {
    sqlx::query("INSERT OR IGNORE INTO config (id) VALUES (1)")
        .execute(db)
        .await
        .expect("seed default config row");
}

#[tokio::test]
async fn migrate_default_title_language_is_english() {
    let db = fresh_migrated_pool().await;
    insert_default_config_row(&db).await;
    let v: String = sqlx::query_scalar("SELECT title_language FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(v, "english", "title_language default must be `english`");
}

#[tokio::test]
async fn migrate_default_finished_series_quality_is_prefer_bd() {
    let db = fresh_migrated_pool().await;
    insert_default_config_row(&db).await;
    let v: String = sqlx::query_scalar("SELECT finished_series_quality FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(v, "prefer_bd");
}

#[tokio::test]
async fn migrate_default_jellyfin_use_ssl_is_zero() {
    let db = fresh_migrated_pool().await;
    insert_default_config_row(&db).await;
    let v: i64 = sqlx::query_scalar("SELECT jellyfin_use_ssl FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(v, 0);
}

#[tokio::test]
async fn migrate_default_rss_interval_minutes_is_fifteen() {
    // Bumped from 5 → 15 on 2026-05-03 because non-Nyaa RSS feeds
    // (SubsPlease + similar direct-feed publishers) consistently
    // rate-limit at the five-minute cadence. See the migration
    // doc-comment at `migrations::mod::migrate` for the full
    // rationale; this test pins the fresh-install default so a
    // future quiet revert to 5 fails loudly.
    let db = fresh_migrated_pool().await;
    insert_default_config_row(&db).await;
    let v: i64 = sqlx::query_scalar("SELECT rss_interval_minutes FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(v, 15);
}

#[tokio::test]
async fn migrate_default_quality_profile_is_web_1080() {
    let db = fresh_migrated_pool().await;
    insert_default_config_row(&db).await;
    let v: String = sqlx::query_scalar("SELECT quality_profile FROM config WHERE id = 1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(v, "web_1080");
}

#[tokio::test]
async fn migrate_default_series_monitor_mode_is_future() {
    // series.monitor_mode default is read on every series insert
    // that doesn't override it. A regression that flipped this to
    // "all" would silently auto-grab the back-catalogue on every
    // Add Series.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (777, 'Default Show', 'Default Show', 'default')",
    )
    .execute(&db)
    .await
    .unwrap();
    let v: String = sqlx::query_scalar("SELECT monitor_mode FROM series WHERE anilist_id = 777")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(v, "future");
}

// ─── Additional FK cascade behavior ──────────────────────────────────

#[tokio::test]
async fn deleting_a_series_cascades_to_episode_grab_history() {
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (10, 'S', 'S', 's')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 10")
        .fetch_one(&db)
        .await
        .unwrap();
    // Need a grabbed_torrent for the FK chain.
    sqlx::query(
        "INSERT INTO episode_grab_history (series_id, episode_number, release_title) \
         VALUES (?, 1, 'fixture release')",
    )
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();

    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .expect("series delete should succeed");

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM episode_grab_history WHERE series_id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn deleting_a_series_cascades_to_episode_monitor_state() {
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (11, 'S', 'S', 's')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 11")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO episode_monitor_state (series_id, episode_number, monitored) \
         VALUES (?, 1, 1)",
    )
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();

    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM episode_monitor_state WHERE series_id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn deleting_a_grabbed_torrent_cascades_to_grabbed_torrent_series() {
    // The auto-expand sibling-routing rows (grabbed_torrent_series)
    // belong to a single grab and have no meaning if the parent
    // grab vanishes — so the FK cascades. Tested explicitly here
    // because removing this cascade would silently leave ghost
    // route rows that post-processing would later try to act on.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (12, 'S', 'S', 's')",
    )
    .execute(&db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = 12")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO grabbed_torrents (series_id, hash, torrent_name, episode_numbers, state) \
         VALUES (?, 'h3', 'name', '[1]', 'pending')",
    )
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();
    let grab_id: i64 = sqlx::query_scalar("SELECT id FROM grabbed_torrents WHERE hash = 'h3'")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO grabbed_torrent_series (grab_id, series_id, file_indices, episode_numbers) \
         VALUES (?, ?, '[0]', '[1]')",
    )
    .bind(grab_id)
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();

    sqlx::query("DELETE FROM grabbed_torrents WHERE id = ?")
        .bind(grab_id)
        .execute(&db)
        .await
        .unwrap();

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrent_series WHERE grab_id = ?")
            .bind(grab_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn deleting_an_external_account_sets_synced_series_pointer_to_null() {
    // ON DELETE SET NULL on series.synced_from_external_account_id
    // — unlinking an AL/MAL account must NOT cascade-drop the
    // imported series rows. Just clears the pointer so the user
    // keeps their library.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO external_accounts (provider, provider_user_id, username, \
         access_token_encrypted, linked_at) \
         VALUES ('anilist', '1001', 'tester', X'00', 0)",
    )
    .execute(&db)
    .await
    .unwrap();
    let acct_id: i64 =
        sqlx::query_scalar("SELECT id FROM external_accounts WHERE provider_user_id = '1001'")
            .fetch_one(&db)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name, \
         synced_from_external_account_id) VALUES (20, 'S', 'S', 's', ?)",
    )
    .bind(acct_id)
    .execute(&db)
    .await
    .unwrap();

    sqlx::query("DELETE FROM external_accounts WHERE id = ?")
        .bind(acct_id)
        .execute(&db)
        .await
        .expect("delete external account");

    // Series row survives, pointer is NULL.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM series WHERE anilist_id = 20")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 1, "series row must survive external_account delete");
    let pointer: Option<i64> = sqlx::query_scalar(
        "SELECT synced_from_external_account_id FROM series WHERE anilist_id = 20",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(
        pointer.is_none(),
        "synced_from_external_account_id must be NULL after account delete; got {pointer:?}"
    );
}

// ─── Schema invariants — UNIQUE + CHECK constraints ─────────────────

#[tokio::test]
async fn config_id_check_constraint_enforces_singleton() {
    // `config` has CHECK (id = 1) — only ever one config row. A
    // refactor that drops the check would let a test write id=2 and
    // silently shadow the real config until a query fetched the
    // wrong id.
    let db = fresh_migrated_pool().await;
    insert_default_config_row(&db).await;
    let res = sqlx::query("INSERT INTO config (id) VALUES (2)")
        .execute(&db)
        .await;
    assert!(
        res.is_err(),
        "config id=2 must be rejected by CHECK constraint"
    );
}

#[tokio::test]
async fn users_username_unique_constraint_rejects_duplicate() {
    let db = fresh_migrated_pool().await;
    sqlx::query("INSERT INTO users (username, password_hash) VALUES ('admin', 'h')")
        .execute(&db)
        .await
        .expect("first user inserts");
    let res = sqlx::query("INSERT INTO users (username, password_hash) VALUES ('admin', 'h2')")
        .execute(&db)
        .await;
    assert!(res.is_err(), "duplicate username must be rejected");
}

#[tokio::test]
async fn series_anilist_id_unique_rejects_duplicate() {
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (50, 'A', 'A', 'a')",
    )
    .execute(&db)
    .await
    .unwrap();
    let res = sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
         VALUES (50, 'B', 'B', 'b')",
    )
    .execute(&db)
    .await;
    assert!(
        res.is_err(),
        "duplicate anilist_id must be rejected by UNIQUE"
    );
}

#[tokio::test]
async fn series_mal_id_partial_unique_index_allows_multiple_nulls() {
    // The mal_id partial UNIQUE index excludes NULL rows
    // (`WHERE mal_id IS NOT NULL`), so multiple AL-only series
    // (no MAL mapping) can all live with mal_id IS NULL. A naive
    // UNIQUE without the partial-index clause would reject every
    // 2nd insert. Pin so a future migration that "tightens" the
    // index by dropping the WHERE clause fails this test loudly.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, mal_id, title, title_romaji, folder_name) \
         VALUES (60, NULL, 'A', 'A', 'a')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO series (anilist_id, mal_id, title, title_romaji, folder_name) \
         VALUES (61, NULL, 'B', 'B', 'b')",
    )
    .execute(&db)
    .await
    .expect("second NULL mal_id must be accepted");
}

#[tokio::test]
async fn series_mal_id_unique_rejects_duplicate_non_null() {
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO series (anilist_id, mal_id, title, title_romaji, folder_name) \
         VALUES (70, 1234, 'A', 'A', 'a')",
    )
    .execute(&db)
    .await
    .unwrap();
    let res = sqlx::query(
        "INSERT INTO series (anilist_id, mal_id, title, title_romaji, folder_name) \
         VALUES (71, 1234, 'B', 'B', 'b')",
    )
    .execute(&db)
    .await;
    assert!(
        res.is_err(),
        "duplicate non-NULL mal_id must be rejected by partial UNIQUE index"
    );
}

#[tokio::test]
async fn external_accounts_provider_check_rejects_unknown_provider() {
    // Schema-level guard against typos. The application-layer
    // `link()` call also enforces "at most one provider," but the
    // CHECK is a second line of defense for the value itself.
    let db = fresh_migrated_pool().await;
    let res = sqlx::query(
        "INSERT INTO external_accounts (provider, provider_user_id, username, \
         access_token_encrypted, linked_at) \
         VALUES ('not-a-real-provider', '1', 'x', X'00', 0)",
    )
    .execute(&db)
    .await;
    assert!(
        res.is_err(),
        "unknown provider must be rejected by CHECK constraint"
    );
}

#[tokio::test]
async fn external_accounts_provider_unique_rejects_two_same_provider_rows() {
    // The application-layer `link()` is the primary at-most-one-of-
    // each guard, but the table also carries a `UNIQUE (provider)`
    // constraint as a backstop. Pin both so a refactor that drops
    // the UNIQUE doesn't silently let two anilist rows coexist.
    let db = fresh_migrated_pool().await;
    sqlx::query(
        "INSERT INTO external_accounts (provider, provider_user_id, username, \
         access_token_encrypted, linked_at) \
         VALUES ('anilist', '1', 'a', X'00', 0)",
    )
    .execute(&db)
    .await
    .unwrap();
    let res = sqlx::query(
        "INSERT INTO external_accounts (provider, provider_user_id, username, \
         access_token_encrypted, linked_at) \
         VALUES ('anilist', '2', 'b', X'00', 0)",
    )
    .execute(&db)
    .await;
    assert!(
        res.is_err(),
        "second `anilist` row must be rejected by UNIQUE (provider)"
    );
}

// ─── Index existence ────────────────────────────────────────────────

async fn index_exists(db: &SqlitePool, name: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
    )
    .bind(name)
    .fetch_one(db)
    .await
    .unwrap_or(0)
        > 0
}

#[tokio::test]
async fn migrate_creates_partial_unique_index_for_active_grab_dedup() {
    // `idx_grabbed_torrents_hash_active` backs the atomic dedup in
    // `record_grab`. Without this partial UNIQUE index, two
    // concurrent grab attempts for the same hash both succeed,
    // producing a duplicate grab row that the rest of the pipeline
    // mishandles.
    let db = fresh_migrated_pool().await;
    assert!(
        index_exists(&db, "idx_grabbed_torrents_hash_active").await,
        "active-grab dedup index missing"
    );
}

#[tokio::test]
async fn migrate_creates_partial_unique_index_for_series_mal_id() {
    let db = fresh_migrated_pool().await;
    assert!(
        index_exists(&db, "idx_series_mal_id").await,
        "series mal_id partial UNIQUE index missing"
    );
}

#[tokio::test]
async fn episode_grab_history_gains_match_provenance_columns() {
    let db = fresh_migrated_pool().await;
    for col in ["match_kind", "match_phase", "matched_alias", "match_ratio"] {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('episode_grab_history') WHERE name = ?",
        )
        .bind(col)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 1, "column {col} missing");
    }
    // Running the migration again must be a no-op.
    crate::models::migrations::migrate(&db)
        .await
        .expect("second migrate is idempotent");
}

#[tokio::test]
async fn import_robustness_columns_exist_with_defaults() {
    // #205: the stall timer's stamp on grabs and its window on config.
    let db = fresh_migrated_pool().await;
    for (table, col) in [
        ("grabbed_torrents", "completed_seen_at"),
        ("config", "import_stall_hours"),
    ] {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
            .bind(table)
            .bind(col)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1, "{table}.{col} missing");
    }
    let cfg = crate::models::config::get_config(&db)
        .await
        .unwrap()
        .unwrap_or_default();
    assert_eq!(cfg.import_stall_hours, 24, "default window is a day");
    crate::models::migrations::migrate(&db)
        .await
        .expect("second migrate is idempotent");
}
