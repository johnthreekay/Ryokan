//! Torznab/newznab indexer registry (issue #28).
//!
//! Configured indexers live in the `indexers` table; the search
//! pipeline reads them at fan-out time and dispatches concurrent
//! queries via the [`crate::services::indexers::Indexer`] trait.
//! Nyaa stays out-of-band per plan decision #1 — it never gets a
//! row here.
//!
//! Initial scope was schema + CRUD only; the `TorznabIndexer` trait
//! impl that consumes these rows, the caps-probe path that populates
//! `caps_json` / `caps_refreshed_at`, and the wiring of `seed_ratio` /
//! `seed_time_minutes` / `min_seeders` into the `DownloadClient`
//! trait's per-torrent seed rules all landed in follow-up changes.
//! Some columns are still written by the Settings form ahead of the
//! code that reads them.

use serde::Serialize;
use sqlx::{Row, SqlitePool};

/// Indexer protocol kind. The wire format for torznab and newznab
/// is identical; the value distinguishes them only for category-
/// mapping (BitTorrent vs NZB) and download-client routing under
/// the torrent-vs-usenet split. Kept as `String` at the
/// boundary because `kind` is read directly into the row struct;
/// callers that need to branch on it can `.as_str()` and match.
pub const KIND_TORZNAB: &str = "torznab";
pub const KIND_NEWZNAB: &str = "newznab";

#[derive(Debug, Clone, Serialize)]
pub struct Indexer {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub api_key: String,
    /// Sonarr convention: lower = preferred. Range 1-50, default 25.
    /// Drives auto-search dedup attribution + interactive search row
    /// tiebreaks + fan-out concurrency order.
    pub priority: i32,
    pub enabled: bool,
    pub is_private_tracker: bool,
    pub seed_ratio: Option<f64>,
    pub seed_time_minutes: Option<i64>,
    pub min_seeders: i32,
    /// Per-indexer override of the default search timeout. `None`
    /// means use the process default (30s, overridable via
    /// `RYOKAN_INDEXER_DEFAULT_TIMEOUT_SECS`).
    pub request_timeout_secs: Option<i64>,
    /// Multi-client routing pin — id of the row in
    /// `download_clients` this indexer routes to. `None` means
    /// "fall through to the default client" at grab time.
    pub download_client_id: Option<i64>,
    /// Multi-RSS — when true, this indexer participates in the
    /// 60s RSS sync fan-out via its torznab/newznab `?t=tvsearch`
    /// endpoint (Option B). Default false so the existing search-
    /// only fan-out is unaffected; users opt in per-row in
    /// Settings → Indexers. Distinct from `enabled`, which gates
    /// the search-time fan-out — an indexer can be search-only
    /// (the historical default) or RSS-enabled too.
    pub rss_enabled: bool,
    /// multi-rss commit E — observability fields populated by the
    /// sync fan-out on every poll attempt. The Settings UI renders
    /// these inline as the "Poll RSS" column status chip.
    /// Comma-separated torznab category ids to ask this indexer for on
    /// every search and poll; blank means automatic.
    pub categories: String,
    pub rss_last_polled_at: Option<i64>,
    /// Most recent RSS poll error, if any. Empty when the last
    /// poll succeeded — UI uses non-empty as the "✗ pill" signal.
    pub rss_last_poll_error: String,
    /// Item count from the last successful RSS poll. Reset to 0
    /// on failure so the chip doesn't lie about a stale count
    /// alongside a fresh error.
    pub rss_last_item_count: i32,
    /// Cached caps response body. Empty until the first probe
    /// succeeds. Read with a 7-day TTL — stale caps trigger a
    /// transparent re-fetch on next read.
    pub caps_json: String,
    pub caps_refreshed_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Input for [`insert`] / [`update`] — caller supplies all the
/// user-editable fields; ID + timestamps + caps cache are managed
/// by this module.
#[derive(Debug, Clone)]
pub struct IndexerForm<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub url: &'a str,
    pub api_key: &'a str,
    pub priority: i32,
    pub enabled: bool,
    pub is_private_tracker: bool,
    pub seed_ratio: Option<f64>,
    pub seed_time_minutes: Option<i64>,
    pub min_seeders: i32,
    pub request_timeout_secs: Option<i64>,
    /// Multi-client routing pin. `None` = use the default
    /// download client at grab time.
    pub download_client_id: Option<i64>,
    /// Multi-RSS — opt this indexer into the per-tick RSS
    /// fan-out via its `?t=tvsearch` (torznab) or `?t=search` /
    /// `?t=tvsearch` (newznab) endpoint. Default false.
    pub rss_enabled: bool,
    pub categories: &'a str,
}

const SELECT_COLUMNS: &str = "id, name, kind, url, api_key, priority, enabled, \
    is_private_tracker, seed_ratio, seed_time_minutes, min_seeders, request_timeout_secs, \
    download_client_id, rss_enabled, categories, rss_last_polled_at, rss_last_poll_error, \
    rss_last_item_count, caps_json, caps_refreshed_at, created_at, updated_at";

fn row_to_indexer(row: &sqlx::sqlite::SqliteRow) -> Indexer {
    // Nullable columns explicitly typed as `Option<T>` so sqlx
    // doesn't fall back to T::default() (0.0 for f64, 0 for i64)
    // when the column is NULL — `try_get::<f64, _>` on a NULL row
    // returns Err, which `.ok()` would convert to None, but the
    // type-inferred `try_get` infers T from the field type and
    // produces Some(0.0)/Some(0) for NULLs. The explicit
    // `Option<T>` form is the unambiguous one.
    Indexer {
        id: row.try_get("id").unwrap_or(0),
        name: row.try_get("name").unwrap_or_default(),
        kind: row.try_get("kind").unwrap_or_default(),
        url: row.try_get("url").unwrap_or_default(),
        api_key: row.try_get("api_key").unwrap_or_default(),
        priority: row.try_get("priority").unwrap_or(25),
        enabled: row.try_get::<i64, _>("enabled").unwrap_or(0) != 0,
        is_private_tracker: row.try_get::<i64, _>("is_private_tracker").unwrap_or(0) != 0,
        seed_ratio: row.try_get::<Option<f64>, _>("seed_ratio").unwrap_or(None),
        seed_time_minutes: row
            .try_get::<Option<i64>, _>("seed_time_minutes")
            .unwrap_or(None),
        min_seeders: row.try_get("min_seeders").unwrap_or(1),
        request_timeout_secs: row
            .try_get::<Option<i64>, _>("request_timeout_secs")
            .unwrap_or(None),
        download_client_id: row
            .try_get::<Option<i64>, _>("download_client_id")
            .unwrap_or(None),
        rss_enabled: row.try_get::<i64, _>("rss_enabled").unwrap_or(0) != 0,
        categories: row.try_get("categories").unwrap_or_default(),
        rss_last_polled_at: row
            .try_get::<Option<i64>, _>("rss_last_polled_at")
            .unwrap_or(None),
        rss_last_poll_error: row.try_get("rss_last_poll_error").unwrap_or_default(),
        rss_last_item_count: row.try_get("rss_last_item_count").unwrap_or(0),
        caps_json: row.try_get("caps_json").unwrap_or_default(),
        caps_refreshed_at: row
            .try_get::<Option<i64>, _>("caps_refreshed_at")
            .unwrap_or(None),
        created_at: row.try_get("created_at").unwrap_or(0),
        updated_at: row.try_get("updated_at").unwrap_or(0),
    }
}

/// All indexer rows ordered by `priority` ascending, tiebreaking by
/// `id`. Mirrors the order auto-search uses for fan-out concurrency.
pub async fn list_all(db: &SqlitePool) -> Result<Vec<Indexer>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLUMNS} FROM indexers ORDER BY priority ASC, id ASC"
    )))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_indexer).collect())
}

/// Enabled indexers only — what the search pipeline iterates over.
/// Disabled rows stay in the DB so the user's config isn't lost when
/// they pause a flaky indexer; this filter just skips them at search
/// time.
pub async fn list_enabled(db: &SqlitePool) -> Result<Vec<Indexer>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLUMNS} FROM indexers WHERE enabled = 1 ORDER BY priority ASC, id ASC"
    )))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_indexer).collect())
}

pub async fn list_rss_enabled(db: &SqlitePool) -> Result<Vec<Indexer>, sqlx::Error> {
    // Multi-RSS — indexers opted into the RSS sync fan-out.
    // Both `enabled` and `rss_enabled` must be true: a user can
    // pause an indexer entirely (enabled=0) without losing the
    // RSS opt-in, and a user can keep an indexer search-only
    // (rss_enabled=0) without disabling search.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLUMNS} FROM indexers \
         WHERE enabled = 1 AND rss_enabled = 1 \
         ORDER BY priority ASC, id ASC"
    )))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_indexer).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<Indexer>, sqlx::Error> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLUMNS} FROM indexers WHERE id = ?"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(row_to_indexer))
}

pub async fn insert(db: &SqlitePool, form: IndexerForm<'_>) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO indexers \
         (name, kind, url, api_key, priority, enabled, is_private_tracker, \
          seed_ratio, seed_time_minutes, min_seeders, request_timeout_secs, \
          download_client_id, rss_enabled, categories) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(form.name)
    .bind(form.kind)
    .bind(form.url)
    .bind(form.api_key)
    .bind(form.priority)
    .bind(form.enabled as i64)
    .bind(form.is_private_tracker as i64)
    .bind(form.seed_ratio)
    .bind(form.seed_time_minutes)
    .bind(form.min_seeders)
    .bind(form.request_timeout_secs)
    .bind(form.download_client_id)
    .bind(form.rss_enabled as i64)
    .bind(normalize_category_list(form.categories))
    .execute(db)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn update(db: &SqlitePool, id: i64, form: IndexerForm<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE indexers SET \
         name = ?, kind = ?, url = ?, api_key = ?, priority = ?, enabled = ?, \
         is_private_tracker = ?, seed_ratio = ?, seed_time_minutes = ?, min_seeders = ?, \
         request_timeout_secs = ?, download_client_id = ?, rss_enabled = ?, \
         categories = ?, updated_at = strftime('%s','now') \
         WHERE id = ?",
    )
    .bind(form.name)
    .bind(form.kind)
    .bind(form.url)
    .bind(form.api_key)
    .bind(form.priority)
    .bind(form.enabled as i64)
    .bind(form.is_private_tracker as i64)
    .bind(form.seed_ratio)
    .bind(form.seed_time_minutes)
    .bind(form.min_seeders)
    .bind(form.request_timeout_secs)
    .bind(form.download_client_id)
    .bind(form.rss_enabled as i64)
    .bind(normalize_category_list(form.categories))
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// PR #107 round-3 review fix #3: delete the indexer row and
/// NULL out FK columns in dependent tables atomically.
///
/// SQLite can't add a real `FOREIGN KEY ... ON DELETE SET NULL`
/// constraint via `ALTER TABLE` after the parent column has shipped,
/// so the SET NULL behavior the migration comment promises has to
/// be enforced at the application layer. Doing it inside a
/// transaction keeps the three statements as one logical operation:
/// either every dependent row is NULL'd and the indexer is gone,
/// or nothing changed.
///
/// The `_ = ?.execute(...)` shape swallowed individual statement
/// errors before; now the `?` operator on each statement aborts
/// the transaction and surfaces the error to the caller, which
/// the handler logs.
/// multi-rss commit E — record observability metrics from a sync-
/// tick poll attempt. Called by the fan-out (commit F) after every
/// `fetch_indexer_rss` call — success or failure. `error` is
/// empty on success, populated on failure. `item_count` is reset
/// to 0 on failure so the chip doesn't lie about a stale count
/// alongside a fresh error. Mirrors the per-feed
/// `direct_rss_feeds::record_poll_metrics` shape.
pub async fn record_rss_poll_metrics(
    db: &SqlitePool,
    id: i64,
    item_count: i32,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE indexers SET \
         rss_last_polled_at = strftime('%s','now'), \
         rss_last_poll_error = ?, rss_last_item_count = ? \
         WHERE id = ?",
    )
    .bind(error)
    .bind(item_count)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE grabbed_torrents SET indexer_id = NULL WHERE indexer_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE pending_grabs SET indexer_id = NULL WHERE indexer_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM indexers WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Update the cached caps response and bump `caps_refreshed_at` to
/// the current Unix timestamp. Called by the caps probe after a
/// successful `t=caps` fetch.
pub async fn update_caps(db: &SqlitePool, id: i64, caps_json: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE indexers SET caps_json = ?, caps_refreshed_at = strftime('%s','now'), \
         updated_at = strftime('%s','now') WHERE id = ?",
    )
    .bind(caps_json)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Parse a comma or space separated list of torznab category ids;
/// junk and duplicates dropped, order kept.
pub fn parse_category_list(raw: &str) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    for tok in raw.split(|c: char| c == ',' || c.is_whitespace() || c == ';') {
        if let Ok(n) = tok.trim().parse::<i32>()
            && n > 0
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    out
}

/// The stored form of the categories field.
pub fn normalize_category_list(raw: &str) -> String {
    parse_category_list(raw)
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

impl Indexer {
    pub fn category_list(&self) -> Vec<i32> {
        parse_category_list(&self.categories)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn fresh_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        db
    }

    fn sample_form<'a>() -> IndexerForm<'a> {
        IndexerForm {
            name: "Test Indexer",
            kind: KIND_TORZNAB,
            url: "https://prowlarr.local/1/api",
            api_key: "secret",
            priority: 25,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 1,
            request_timeout_secs: None,
            download_client_id: None,
            rss_enabled: false,
            categories: "",
        }
    }

    #[tokio::test]
    async fn insert_then_get_round_trips_fields() {
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();
        let row = get_by_id(&db, id).await.unwrap().expect("row exists");
        assert_eq!(row.name, "Test Indexer");
        assert_eq!(row.kind, KIND_TORZNAB);
        assert_eq!(row.url, "https://prowlarr.local/1/api");
        assert_eq!(row.api_key, "secret");
        assert_eq!(row.priority, 25);
        assert!(row.enabled);
        assert!(!row.is_private_tracker);
        assert_eq!(row.seed_ratio, None);
        assert_eq!(row.min_seeders, 1);
    }

    #[tokio::test]
    async fn list_all_orders_by_priority_ascending() {
        // Sonarr convention: lower priority = preferred. The fan-out
        // path iterates this order, so a regression that flipped it
        // would silently move a less-preferred indexer to the front.
        let db = fresh_db().await;
        let mut high_prio = sample_form();
        high_prio.name = "High Priority";
        high_prio.priority = 5;
        let mut low_prio = sample_form();
        low_prio.name = "Low Priority";
        low_prio.priority = 50;
        // Insert low first so order isn't accidentally insertion-order.
        insert(&db, low_prio).await.unwrap();
        insert(&db, high_prio).await.unwrap();

        let rows = list_all(&db).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "High Priority");
        assert_eq!(rows[1].name, "Low Priority");
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled_rows() {
        let db = fresh_db().await;
        let mut on = sample_form();
        on.name = "On";
        on.enabled = true;
        let mut off = sample_form();
        off.name = "Off";
        off.enabled = false;
        insert(&db, on).await.unwrap();
        insert(&db, off).await.unwrap();

        let enabled = list_enabled(&db).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "On");
    }

    #[tokio::test]
    async fn record_rss_poll_metrics_writes_success_then_resets_count_on_failure() {
        // Mirrors the per-feed `record_poll_metrics` test in
        // direct_rss_feeds — same contract: success path stamps
        // the polled timestamp + clears the error; failure path
        // resets item_count to 0 so the chip doesn't lie.
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();

        record_rss_poll_metrics(&db, id, 18, "").await.unwrap();
        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert!(row.rss_last_polled_at.unwrap_or(0) > 0);
        assert_eq!(row.rss_last_poll_error, "");
        assert_eq!(row.rss_last_item_count, 18);

        record_rss_poll_metrics(&db, id, 0, "503 upstream")
            .await
            .unwrap();
        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.rss_last_item_count, 0);
        assert_eq!(row.rss_last_poll_error, "503 upstream");
    }

    #[tokio::test]
    async fn list_rss_enabled_requires_both_enabled_and_rss_enabled() {
        // Multi-RSS: the RSS fan-out filter is conjunctive —
        // an indexer must have BOTH enabled=1 AND rss_enabled=1 to
        // contribute to the per-tick fan-out. A user who paused an
        // indexer entirely (enabled=0) shouldn't see its feed
        // continue running just because rss_enabled=1, and a
        // search-only indexer (rss_enabled=0) shouldn't get pulled
        // in just because enabled=1.
        let db = fresh_db().await;
        let mut both = sample_form();
        both.name = "Both";
        both.enabled = true;
        both.rss_enabled = true;
        let mut search_only = sample_form();
        search_only.name = "SearchOnly";
        search_only.enabled = true;
        search_only.rss_enabled = false;
        let mut paused = sample_form();
        paused.name = "Paused";
        paused.enabled = false;
        paused.rss_enabled = true;

        insert(&db, both).await.unwrap();
        insert(&db, search_only).await.unwrap();
        insert(&db, paused).await.unwrap();

        let rss = list_rss_enabled(&db).await.unwrap();
        assert_eq!(rss.len(), 1);
        assert_eq!(rss[0].name, "Both");
    }

    #[tokio::test]
    async fn update_changes_fields_and_bumps_updated_at() {
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();
        let original_updated_at = get_by_id(&db, id).await.unwrap().unwrap().updated_at;
        // strftime resolution is 1s; sleep ensures the bump is visible.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        let mut edited = sample_form();
        edited.name = "Renamed";
        edited.priority = 10;
        edited.enabled = false;
        update(&db, id, edited).await.unwrap();

        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Renamed");
        assert_eq!(row.priority, 10);
        assert!(!row.enabled);
        assert!(
            row.updated_at >= original_updated_at,
            "updated_at must not regress"
        );
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();
        assert!(get_by_id(&db, id).await.unwrap().is_some());
        delete(&db, id).await.unwrap();
        assert!(get_by_id(&db, id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nulls_out_grabbed_torrents_indexer_id() {
        // PR #107 round-3 review fix #4: regression for the
        // SET-NULL transaction. Insert an indexer + a grab row
        // pointing at it, delete the indexer, assert the grab row's
        // indexer_id is NULL (not orphaned, not deleted).
        let db = fresh_db().await;
        let indexer_id = insert(&db, sample_form()).await.unwrap();

        // First we need a series row for grabbed_torrents to FK to.
        // Use the test_support seed helper if available; otherwise
        // a minimal raw insert.
        let series_id: i64 = sqlx::query_scalar(
            "INSERT INTO series (anilist_id, title, title_romaji, folder_name) \
             VALUES (1, 't', 't', 'f') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO grabbed_torrents (hash, torrent_name, series_id, indexer_id) \
             VALUES ('h', 't', ?, ?)",
        )
        .bind(series_id)
        .bind(indexer_id)
        .execute(&db)
        .await
        .unwrap();

        delete(&db, indexer_id).await.expect("delete must succeed");

        // Indexer row gone.
        assert!(get_by_id(&db, indexer_id).await.unwrap().is_none());

        // Grab row stays, but its indexer_id is NULL.
        let leftover_indexer_id: Option<i64> =
            sqlx::query_scalar("SELECT indexer_id FROM grabbed_torrents WHERE hash = 'h'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            leftover_indexer_id, None,
            "delete must NULL out grabbed_torrents.indexer_id, not orphan it"
        );
    }

    #[tokio::test]
    async fn update_preserves_optional_fields_when_form_round_trips() {
        // PR #107 round-3 review fix #4: regression for the
        // template-field-missing bug from round 2. If a future
        // template tweak drops one of the optional inputs, an
        // edit-without-changes would NULL the column. The model-
        // layer round-trip catches it: pass the same form values
        // through update() twice and assert nothing drifts.
        let db = fresh_db().await;
        let mut full = sample_form();
        full.seed_ratio = Some(2.5);
        full.seed_time_minutes = Some(120);
        full.request_timeout_secs = Some(60);
        let id = insert(&db, full).await.unwrap();

        // Re-issue an update with the same shape — simulates the
        // user clicking Save with no edits.
        let mut same = sample_form();
        same.seed_ratio = Some(2.5);
        same.seed_time_minutes = Some(120);
        same.request_timeout_secs = Some(60);
        update(&db, id, same).await.unwrap();

        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.seed_ratio, Some(2.5));
        assert_eq!(row.seed_time_minutes, Some(120));
        assert_eq!(row.request_timeout_secs, Some(60));
    }

    #[tokio::test]
    async fn update_caps_persists_json_and_timestamp() {
        let db = fresh_db().await;
        let id = insert(&db, sample_form()).await.unwrap();
        // Fresh insert has no caps cached yet.
        let pre = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(pre.caps_json, "");
        assert!(pre.caps_refreshed_at.is_none());

        update_caps(&db, id, r#"{"limits":{"max":100}}"#)
            .await
            .unwrap();
        let post = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(post.caps_json, r#"{"limits":{"max":100}}"#);
        assert!(post.caps_refreshed_at.is_some());
    }
}
