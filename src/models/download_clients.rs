//! Multi-client routing — one row per *configured* download
//! client. Replaces the pre-multi-client single-slot config
//! (`config.active_client` + per-kind credentials columns) with
//! a row-per-client shape so a user can run "Local qBit" +
//! "Seedbox Deluge" + "NzbGeek SAB" simultaneously, and pin
//! individual indexers (or the built-in Nyaa search) to specific
//! clients.
//!
//! Pin resolution at grab time:
//! 1. `indexer.download_client_id` if the grab is attributable
//!    to a torznab indexer row.
//! 2. `config.nyaa_download_client_id` for built-in Nyaa hits.
//! 3. The row marked `is_default = 1`.
//! 4. Otherwise — surface "no download client configured."
//!
//! The `kind` column matches the values
//! `services::download_client::build_torrent_client` /
//! `build_usenet_client` accept (`"qbittorrent" | "deluge" |
//! "transmission" | "rtorrent"` for now; SAB lands later).
//! Validation lives at the form layer; reads here trust the DB
//! to hold a known value.

use sqlx::{Row, SqlitePool};

/// Row shape for `download_clients`. Mirrors the schema 1:1; no
/// derived columns.
#[derive(Debug, Clone)]
pub struct DownloadClientRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub username: String,
    pub password: String,
    pub label: String,
    pub download_path: String,
    pub enabled: bool,
    pub is_default: bool,
    /// Issue #228: remove an imported download from this client (files
    /// included) once it has finished seeding; usenet jobs and move-mode
    /// torrents go right after import. Sonarr's per-client "Remove
    /// Completed". Default on; set through `set_remove_completed`, not
    /// the upsert form, so the many `DownloadClientForm` call sites
    /// stay as they are.
    pub remove_completed: bool,
}

/// Insert/update payload — `&str` rather than `String` so the
/// caller can pass borrowed slices without an extra clone. Same
/// shape as the trait constructors expect.
pub struct DownloadClientForm<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub url: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    pub label: &'a str,
    pub download_path: &'a str,
    pub enabled: bool,
    pub is_default: bool,
}

const SELECT_COLS: &str = "id, name, kind, url, username, password, label, \
                           download_path, enabled, is_default, remove_completed";

/// Wire-protocol family for a download-client kind. Mirrors
/// `services::download_client::protocol_for_client_kind` — duplicated
/// at the model layer because services depends on models, so we can't
/// reach upward without a circular dep. Keep the two in sync; both
/// derive from the same finite set of known kinds.
///
/// Drives the per-protocol uniqueness invariant for `is_default`: at
/// most one torrent client AND at most one usenet client may carry
/// the flag at any time. Indexers without an explicit pin route to
/// the default of their own protocol (torznab → torrent default,
/// newznab → usenet default).
pub fn protocol_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "qbittorrent" | "deluge" | "transmission" | "rtorrent" => Some("torrent"),
        "sabnzbd" => Some("usenet"),
        _ => None,
    }
}

fn map_row(r: sqlx::sqlite::SqliteRow) -> DownloadClientRow {
    DownloadClientRow {
        id: r.get("id"),
        name: r.get("name"),
        kind: r.get("kind"),
        url: r.get("url"),
        username: r.try_get("username").unwrap_or_default(),
        password: r.try_get("password").unwrap_or_default(),
        label: r.try_get("label").unwrap_or_default(),
        download_path: r.try_get("download_path").unwrap_or_default(),
        enabled: r
            .try_get::<i64, _>("enabled")
            .map(|v| v != 0)
            .unwrap_or(true),
        is_default: r
            .try_get::<i64, _>("is_default")
            .map(|v| v != 0)
            .unwrap_or(false),
        remove_completed: r
            .try_get::<i64, _>("remove_completed")
            .map(|v| v != 0)
            .unwrap_or(true),
    }
}

pub async fn list_all(db: &SqlitePool) -> Result<Vec<DownloadClientRow>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLS} FROM download_clients ORDER BY is_default DESC, name COLLATE NOCASE"
    )))
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(map_row).collect())
}

/// Used by the cache builder — only enabled rows get
/// instantiated as live trait impls. Disabled rows survive in
/// the DB so a user can toggle them back on without re-entering
/// credentials.
pub async fn list_enabled(db: &SqlitePool) -> Result<Vec<DownloadClientRow>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLS} FROM download_clients WHERE enabled = 1 ORDER BY id"
    )))
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(map_row).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<DownloadClientRow>, sqlx::Error> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLS} FROM download_clients WHERE id = ?"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(map_row))
}

/// The current default client (any protocol), if any. NULL when
/// no client has been added yet (fresh install) or when a manual
/// DB edit cleared every `is_default = 1`. With the per-protocol
/// invariant the result is non-deterministic when both a torrent
/// and a usenet default exist — callers that care about protocol
/// should use [`get_default_for_protocol`] instead.
pub async fn get_default(db: &SqlitePool) -> Result<Option<DownloadClientRow>, sqlx::Error> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLS} FROM download_clients WHERE is_default = 1 ORDER BY id LIMIT 1"
    )))
    .fetch_optional(db)
    .await?;
    Ok(row.map(map_row))
}

/// The current default client for the given protocol (`"torrent"`
/// or `"usenet"`), if any. Returns NULL when nothing of that
/// protocol is configured or marked default. The pool builder uses
/// this on rebuild to populate `default_torrent_id` /
/// `default_usenet_id` independently.
pub async fn get_default_for_protocol(
    db: &SqlitePool,
    protocol: &str,
) -> Result<Option<DownloadClientRow>, sqlx::Error> {
    let kinds: &[&str] = match protocol {
        "torrent" => &["qbittorrent", "deluge", "transmission", "rtorrent"],
        "usenet" => &["sabnzbd"],
        _ => return Ok(None),
    };
    let placeholders = vec!["?"; kinds.len()].join(", ");
    let sql = format!(
        "SELECT {SELECT_COLS} FROM download_clients \
         WHERE is_default = 1 AND kind IN ({placeholders}) \
         ORDER BY id LIMIT 1"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for k in kinds {
        q = q.bind(*k);
    }
    let row = q.fetch_optional(db).await?;
    Ok(row.map(map_row))
}

/// Insert a new row. If `form.is_default` is true, every other
/// row OF THE SAME PROTOCOL gets its `is_default` cleared in the
/// same transaction so the invariant "exactly one row of each
/// protocol has is_default = 1" stays recoverable. Cross-protocol
/// defaults coexist (one torrent + one usenet default at a time)
/// so a torznab indexer with no pin routes to the torrent default
/// and a newznab indexer routes to the usenet default. Returns
/// the new row's id.
pub async fn insert(db: &SqlitePool, form: DownloadClientForm<'_>) -> Result<i64, sqlx::Error> {
    let mut tx = db.begin().await?;
    if form.is_default {
        clear_other_defaults_in_protocol(&mut tx, form.kind, None).await?;
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO download_clients
             (name, kind, url, username, password, label, download_path, enabled, is_default)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(form.name.trim())
    .bind(form.kind)
    .bind(form.url.trim())
    .bind(form.username.trim())
    // Don't `.trim()` password — leading/trailing whitespace can be
    // intentional (passphrase generators, rare but real) and silently
    // dropping it would lock a user out of their own client.
    .bind(form.password)
    .bind(form.label.trim())
    .bind(form.download_path.trim().trim_end_matches('/'))
    .bind(if form.enabled { 1_i64 } else { 0_i64 })
    .bind(if form.is_default { 1_i64 } else { 0_i64 })
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(
    db: &SqlitePool,
    id: i64,
    form: DownloadClientForm<'_>,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    if form.is_default {
        clear_other_defaults_in_protocol(&mut tx, form.kind, Some(id)).await?;
    }
    sqlx::query(
        "UPDATE download_clients
         SET name = ?, kind = ?, url = ?, username = ?, password = ?, label = ?,
             download_path = ?, enabled = ?, is_default = ?,
             updated_at = strftime('%s','now')
         WHERE id = ?",
    )
    .bind(form.name.trim())
    .bind(form.kind)
    .bind(form.url.trim())
    .bind(form.username.trim())
    .bind(form.password)
    .bind(form.label.trim())
    .bind(form.download_path.trim().trim_end_matches('/'))
    .bind(if form.enabled { 1_i64 } else { 0_i64 })
    .bind(if form.is_default { 1_i64 } else { 0_i64 })
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Issue #228: the per-client "Remove completed downloads" switch.
pub async fn set_remove_completed(db: &SqlitePool, id: i64, on: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE download_clients SET remove_completed = ?, updated_at = strftime('%s','now') \
         WHERE id = ?",
    )
    .bind(if on { 1_i64 } else { 0_i64 })
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Delete the row and NULL out every dangling pin in one
/// transaction. Pins live on `indexers.download_client_id`,
/// `config.nyaa_download_client_id`, and
/// `grabbed_torrents.download_client_id`. Without the NULL-out,
/// FK-less SQLite would leave dangling ids that resolve to None
/// at routing time (silent fall-through to default; surprising)
/// and the row would still appear in queries that join on the
/// pin. The `grabbed_torrents` NULL-out specifically prevents
/// pending grabs from getting orphaned forever — `run_once`
/// short-circuits when it can't resolve the stamped id, so a
/// stale stamp would skip both the import path AND the 60s
/// stale-mark grace window. NULLing the stamp lets the next
/// post-processing pass either match the grab against the
/// current default's `list_scoped` (unlikely — wrong client) or
/// fall through to the stale path and mark it `removed`.
///
/// If the deleted row was the default, **auto-promotes the lowest-id
/// remaining row to default in the same transaction**. Picking by
/// min(id) is deterministic (oldest survivor wins) and avoids leaving
/// the system in a "no default until the user picks one" state where
/// every grab would fail until the user manually intervenes. Promotion
/// is skipped when no rows remain (deleting the last client) — caller
/// is on their own to add a new one.
pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    // Read the deleted row's (default-flag, kind) BEFORE the row is
    // gone so we know whether (and within which protocol) to promote
    // a replacement after the DELETE commits. None means "row didn't
    // exist anymore" (race with a parallel delete); skip the
    // promotion check entirely in that case.
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT is_default, kind FROM download_clients WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    sqlx::query("UPDATE indexers SET download_client_id = NULL WHERE download_client_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE config SET nyaa_download_client_id = NULL WHERE nyaa_download_client_id = ?",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE grabbed_torrents SET download_client_id = NULL WHERE download_client_id = ?",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM download_clients WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // Auto-promote: only when the row we just deleted was the default
    // AND its protocol still has a surviving row. Picking by min(id)
    // is deterministic (oldest survivor wins) and scoped per-protocol
    // so deleting a torrent default doesn't grab a usenet row (or
    // vice versa). When no row of the same protocol survives, there's
    // nothing to promote — the user is on their own to add another
    // client of that protocol.
    if let Some((flag, kind)) = row
        && flag != 0
    {
        let kinds: &[&str] = match protocol_for_kind(&kind) {
            Some("torrent") => &["qbittorrent", "deluge", "transmission", "rtorrent"],
            Some("usenet") => &["sabnzbd"],
            _ => &[],
        };
        if !kinds.is_empty() {
            let placeholders = vec!["?"; kinds.len()].join(", ");
            let sql =
                format!("SELECT MIN(id) FROM download_clients WHERE kind IN ({placeholders})");
            let mut q = sqlx::query_scalar::<_, Option<i64>>(sqlx::AssertSqlSafe(sql));
            for k in kinds {
                q = q.bind(*k);
            }
            let next_default: Option<i64> = q.fetch_one(&mut *tx).await?;
            if let Some(next_id) = next_default {
                sqlx::query(
                    "UPDATE download_clients SET is_default = 1, updated_at = strftime('%s','now') WHERE id = ?",
                )
                .bind(next_id)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Mark `id` as the default for its own protocol (torrent or
/// usenet) and clear every other same-protocol row's flag. The
/// other-protocol default is left alone so a one-click "Set
/// default" on a SAB row doesn't quietly clear the torrent
/// default at the same time. Idempotent at the `is_default` value
/// level (a re-call on an already-default row leaves the flag at
/// 1); `updated_at` is bumped on every call regardless.
pub async fn set_default(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    // Look up the row's kind to scope the clear by protocol.
    // Missing row (concurrent delete) → bail without touching
    // anything; the caller's UPDATE below would be a no-op too.
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM download_clients WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(kind) = kind else {
        tx.commit().await?;
        return Ok(());
    };
    clear_other_defaults_in_protocol(&mut tx, &kind, Some(id)).await?;
    sqlx::query(
        "UPDATE download_clients SET is_default = 1, updated_at = strftime('%s','now') WHERE id = ?",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Clear `is_default` on every row that shares `kind`'s protocol,
/// optionally excluding `keep_id` (used on the upsert path so the
/// just-flagged row doesn't get cleared by its own protocol-mates
/// loop). Unknown kinds (mapped to None by `protocol_for_kind`)
/// are no-ops — `protocol_for_client_kind`'s permissive contract.
async fn clear_other_defaults_in_protocol(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    keep_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    let kinds: &[&str] = match protocol_for_kind(kind) {
        Some("torrent") => &["qbittorrent", "deluge", "transmission", "rtorrent"],
        Some("usenet") => &["sabnzbd"],
        _ => return Ok(()),
    };
    let placeholders = vec!["?"; kinds.len()].join(", ");
    let sql = match keep_id {
        Some(_) => format!(
            "UPDATE download_clients SET is_default = 0 \
             WHERE is_default = 1 AND kind IN ({placeholders}) AND id != ?"
        ),
        None => format!(
            "UPDATE download_clients SET is_default = 0 \
             WHERE is_default = 1 AND kind IN ({placeholders})"
        ),
    };
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for k in kinds {
        q = q.bind(*k);
    }
    if let Some(id) = keep_id {
        q = q.bind(id);
    }
    q.execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    fn form<'a>(name: &'a str, kind: &'a str, url: &'a str) -> DownloadClientForm<'a> {
        DownloadClientForm {
            name,
            kind,
            url,
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        }
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() {
        let db = in_memory_pool().await;
        let id = insert(
            &db,
            form("Local qBit", "qbittorrent", "http://localhost:8080"),
        )
        .await
        .unwrap();
        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Local qBit");
        assert_eq!(row.kind, "qbittorrent");
        assert_eq!(row.url, "http://localhost:8080");
        assert!(row.enabled);
        assert!(!row.is_default);
    }

    #[tokio::test]
    async fn insert_with_is_default_clears_prior_default() {
        let db = in_memory_pool().await;
        let mut f = form("First", "qbittorrent", "http://1");
        f.is_default = true;
        let first = insert(&db, f).await.unwrap();

        let mut f2 = form("Second", "deluge", "http://2");
        f2.is_default = true;
        let second = insert(&db, f2).await.unwrap();

        // Only `second` should still be default.
        let first_row = get_by_id(&db, first).await.unwrap().unwrap();
        let second_row = get_by_id(&db, second).await.unwrap().unwrap();
        assert!(!first_row.is_default, "first must lose its default flag");
        assert!(second_row.is_default);

        let default_row = get_default(&db).await.unwrap().unwrap();
        assert_eq!(default_row.id, second);
    }

    #[tokio::test]
    async fn set_default_is_idempotent_and_unique() {
        let db = in_memory_pool().await;
        let a = insert(&db, form("A", "qbittorrent", "http://a"))
            .await
            .unwrap();
        let b = insert(&db, form("B", "deluge", "http://b")).await.unwrap();

        set_default(&db, a).await.unwrap();
        set_default(&db, a).await.unwrap(); // idempotent
        let default_row = get_default(&db).await.unwrap().unwrap();
        assert_eq!(default_row.id, a);

        set_default(&db, b).await.unwrap();
        // Only one row has is_default = 1.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM download_clients WHERE is_default = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn delete_nulls_out_indexer_and_nyaa_pins() {
        let db = in_memory_pool().await;
        let id = insert(&db, form("X", "qbittorrent", "http://x"))
            .await
            .unwrap();

        // Create an indexer row pinned to this client.
        sqlx::query(
            "INSERT INTO indexers (name, kind, url, api_key, download_client_id) \
             VALUES ('AB', 'torznab', 'http://prowlarr/1/api', 'k', ?)",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();

        // Pin Nyaa to it as well (config row needs to exist first).
        sqlx::query("INSERT INTO config (id) VALUES (1)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE config SET nyaa_download_client_id = ? WHERE id = 1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();

        delete(&db, id).await.unwrap();

        // Pin columns are NULL.
        let indexer_pin: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM indexers WHERE name = 'AB'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(
            indexer_pin.is_none(),
            "indexer pin must be NULLed on delete"
        );

        let nyaa_pin: Option<i64> =
            sqlx::query_scalar("SELECT nyaa_download_client_id FROM config WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(nyaa_pin.is_none(), "Nyaa pin must be NULLed on delete");

        // Row itself is gone.
        assert!(get_by_id(&db, id).await.unwrap().is_none());
    }

    /// Pre-PR-109-review-2 regression: pending grabs stamped to a
    /// soon-to-be-deleted client used to keep their stamp after
    /// `delete()`, which orphaned the grab forever in
    /// `post_processing::run_once` (the loop's `clients.get(&id)`
    /// returned None, the `continue` skipped past the 60s stale
    /// check, and the grab stayed `pending` indefinitely). Lock the
    /// fix by inserting a pending grab + deleting its client + asserting
    /// the column is NULL afterward. A null stamp lets the next
    /// post-processing pass fall through to default and reach the
    /// stale path.
    #[tokio::test]
    async fn delete_nulls_out_grabbed_torrents_stamp() {
        use crate::models::series::{self, SeriesCore};

        let db = in_memory_pool().await;
        let id = insert(&db, form("X", "qbittorrent", "http://x"))
            .await
            .unwrap();

        // Seed a series + a pending grab stamped to this client.
        let (series_id, _) = series::upsert(
            &db,
            SeriesCore {
                anilist_id: 1,
                mal_id: None,
                title: "Show",
                title_romaji: "Show",
                title_english: "Show",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2024),
                end_year: Some(2024),
            },
        )
        .await
        .expect("series upsert");
        let grab_id = crate::models::grabbed_torrents::record_grab(
            &db,
            "deadbeef",
            "[Group] Show - 01.mkv",
            series_id,
            &[1],
            false,
        )
        .await
        .expect("record")
        .expect("inserted");
        crate::models::grabbed_torrents::set_download_client(&db, grab_id, Some(id))
            .await
            .expect("stamp");

        // Pre-condition.
        let stamped: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
                .bind(grab_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stamped, Some(id));

        // Delete the client — the cascade should NULL the stamp.
        delete(&db, id).await.unwrap();

        let stamp_after: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
                .bind(grab_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(
            stamp_after.is_none(),
            "grabbed_torrents.download_client_id must be NULLed on delete \
             so post_processing falls through to default and reaches the \
             stale-mark path; otherwise pending grabs orphan forever"
        );
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled_rows() {
        let db = in_memory_pool().await;
        insert(&db, form("On", "qbittorrent", "http://a"))
            .await
            .unwrap();
        let mut off = form("Off", "deluge", "http://b");
        off.enabled = false;
        insert(&db, off).await.unwrap();

        let enabled = list_enabled(&db).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "On");
    }

    #[tokio::test]
    async fn update_round_trip() {
        let db = in_memory_pool().await;
        let id = insert(&db, form("Initial", "qbittorrent", "http://1"))
            .await
            .unwrap();
        let mut f = form("Renamed", "deluge", "http://2");
        f.username = "u";
        f.password = "p";
        f.label = "ryokan";
        update(&db, id, f).await.unwrap();
        let row = get_by_id(&db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Renamed");
        assert_eq!(row.kind, "deluge");
        assert_eq!(row.url, "http://2");
        assert_eq!(row.username, "u");
        assert_eq!(row.password, "p");
        assert_eq!(row.label, "ryokan");
    }

    #[tokio::test]
    async fn list_all_orders_default_first_then_alphabetical() {
        let db = in_memory_pool().await;
        insert(&db, form("zeta", "qbittorrent", "http://z"))
            .await
            .unwrap();
        insert(&db, form("alpha", "deluge", "http://a"))
            .await
            .unwrap();
        let mut def = form("middle", "transmission", "http://m");
        def.is_default = true;
        insert(&db, def).await.unwrap();

        let rows = list_all(&db).await.unwrap();
        assert_eq!(rows[0].name, "middle"); // default first
        assert_eq!(rows[1].name, "alpha"); // then case-insensitive name
        assert_eq!(rows[2].name, "zeta");
    }

    /// Deleting the current default auto-promotes the lowest-id
    /// surviving row so the system never lands in a "no default"
    /// state when an alternative exists. Caught a real user-reported
    /// bug where deleting the active client left RSS / auto-search
    /// without a target until the user manually set a new default.
    #[tokio::test]
    async fn delete_default_auto_promotes_lowest_id_survivor() {
        let db = in_memory_pool().await;
        let mut f = form("Default", "qbittorrent", "http://default");
        f.is_default = true;
        let default_id = insert(&db, f).await.unwrap();
        let second_id = insert(&db, form("Second", "deluge", "http://2"))
            .await
            .unwrap();
        let third_id = insert(&db, form("Third", "transmission", "http://3"))
            .await
            .unwrap();
        // Sanity: only `default_id` is the default before delete.
        assert_eq!(get_default(&db).await.unwrap().unwrap().id, default_id);

        delete(&db, default_id).await.unwrap();

        let new_default = get_default(&db).await.unwrap().expect(
            "auto-promote must elect a new default when the deleted row was default and \
             survivors exist",
        );
        // Lowest surviving id wins (deterministic, oldest-survivor).
        assert_eq!(
            new_default.id,
            second_id.min(third_id),
            "lowest-id surviving row should have been promoted to default"
        );

        // And exactly one row carries is_default = 1.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM download_clients WHERE is_default = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    /// Deleting a non-default row leaves the existing default untouched.
    /// Counterpart to the auto-promote test — proves promotion only
    /// fires when the deleted row was the default, not on every delete.
    #[tokio::test]
    async fn delete_non_default_leaves_existing_default_intact() {
        let db = in_memory_pool().await;
        let mut f = form("Default", "qbittorrent", "http://default");
        f.is_default = true;
        let default_id = insert(&db, f).await.unwrap();
        let other_id = insert(&db, form("Other", "deluge", "http://other"))
            .await
            .unwrap();

        delete(&db, other_id).await.unwrap();

        let still_default = get_default(&db).await.unwrap().unwrap();
        assert_eq!(still_default.id, default_id);
    }

    /// Deleting the last remaining row (which happens to be the default)
    /// leaves the table empty and no default — auto-promote is a no-op
    /// when there's nothing to promote.
    #[tokio::test]
    async fn delete_only_default_leaves_empty_table_with_no_default() {
        let db = in_memory_pool().await;
        let mut f = form("Only", "qbittorrent", "http://only");
        f.is_default = true;
        let id = insert(&db, f).await.unwrap();
        delete(&db, id).await.unwrap();

        let rows = list_all(&db).await.unwrap();
        assert!(rows.is_empty());
        assert!(get_default(&db).await.unwrap().is_none());
    }

    /// Per-protocol default invariant: a torrent default and a usenet
    /// default coexist because they route disjoint indexer kinds.
    /// Marking a SAB row as default must NOT clear the torrent default,
    /// and vice versa. Counterpart of the legacy "exactly one default"
    /// test that the prior code shape enforced.
    #[tokio::test]
    async fn torrent_and_usenet_defaults_coexist() {
        let db = in_memory_pool().await;
        // Add a default qBit (torrent).
        let mut t = form("qBit", "qbittorrent", "http://qbit");
        t.is_default = true;
        let qbit_id = insert(&db, t).await.unwrap();
        // Add a default SAB (usenet).
        let mut u = form("SAB", "sabnzbd", "http://sab");
        u.is_default = true;
        let sab_id = insert(&db, u).await.unwrap();

        // Both rows should still be default — different protocols.
        let qbit_row = get_by_id(&db, qbit_id).await.unwrap().unwrap();
        let sab_row = get_by_id(&db, sab_id).await.unwrap().unwrap();
        assert!(
            qbit_row.is_default,
            "torrent default must survive marking a usenet row default"
        );
        assert!(
            sab_row.is_default,
            "usenet default must survive marking a torrent row default"
        );

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM download_clients WHERE is_default = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            count, 2,
            "exactly two rows carry is_default = 1 (one per protocol)"
        );
    }

    /// Marking a second torrent client as default DOES clear the prior
    /// torrent default (same-protocol invariant). Same-protocol unique-
    /// ness is the only mutex; cross-protocol entries are independent.
    #[tokio::test]
    async fn second_torrent_default_clears_prior_torrent_default() {
        let db = in_memory_pool().await;
        let mut a = form("qBit", "qbittorrent", "http://qbit");
        a.is_default = true;
        let qbit_id = insert(&db, a).await.unwrap();
        let mut b = form("Deluge", "deluge", "http://deluge");
        b.is_default = true;
        let deluge_id = insert(&db, b).await.unwrap();

        let qbit_row = get_by_id(&db, qbit_id).await.unwrap().unwrap();
        let deluge_row = get_by_id(&db, deluge_id).await.unwrap().unwrap();
        assert!(
            !qbit_row.is_default,
            "prior torrent default must be cleared"
        );
        assert!(deluge_row.is_default, "new torrent default must take over");
    }

    /// `get_default_for_protocol` returns the right row per protocol
    /// when both protocol families have a default configured.
    #[tokio::test]
    async fn get_default_for_protocol_returns_per_protocol_row() {
        let db = in_memory_pool().await;
        let mut t = form("qBit", "qbittorrent", "http://qbit");
        t.is_default = true;
        let qbit_id = insert(&db, t).await.unwrap();
        let mut u = form("SAB", "sabnzbd", "http://sab");
        u.is_default = true;
        let sab_id = insert(&db, u).await.unwrap();

        let torrent_default = get_default_for_protocol(&db, "torrent")
            .await
            .unwrap()
            .expect("torrent default must resolve");
        assert_eq!(torrent_default.id, qbit_id);

        let usenet_default = get_default_for_protocol(&db, "usenet")
            .await
            .unwrap()
            .expect("usenet default must resolve");
        assert_eq!(usenet_default.id, sab_id);
    }

    /// Auto-promote on delete is per-protocol: deleting the torrent
    /// default elects another torrent row, not a usenet survivor (which
    /// would silently route torznab grabs through SAB and trip the
    /// protocol guard at add time).
    #[tokio::test]
    async fn delete_torrent_default_promotes_torrent_survivor_not_usenet() {
        let db = in_memory_pool().await;
        // Surviving usenet first so its id is < the torrent survivor;
        // a naive lowest-id-overall promotion would pick this row.
        let mut u = form("SAB", "sabnzbd", "http://sab");
        u.is_default = true;
        let sab_id = insert(&db, u).await.unwrap();
        let mut t = form("qBitDefault", "qbittorrent", "http://qbit-default");
        t.is_default = true;
        let qbit_default_id = insert(&db, t).await.unwrap();
        let qbit_survivor_id = insert(&db, form("qBitSurvivor", "deluge", "http://deluge"))
            .await
            .unwrap();

        delete(&db, qbit_default_id).await.unwrap();

        let promoted = get_default_for_protocol(&db, "torrent")
            .await
            .unwrap()
            .expect("a torrent survivor must be promoted");
        assert_eq!(
            promoted.id, qbit_survivor_id,
            "torrent default must auto-promote to a torrent row, not the usenet row"
        );
        // SAB must remain the usenet default — its protocol wasn't
        // touched by the delete.
        let usenet_default = get_default_for_protocol(&db, "usenet")
            .await
            .unwrap()
            .expect("usenet default must remain after a torrent delete");
        assert_eq!(usenet_default.id, sab_id);
    }
}
