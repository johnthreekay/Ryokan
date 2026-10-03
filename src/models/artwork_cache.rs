use sqlx::{Row, SqlitePool};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ArtworkEntry {
    pub cache_key: String,
    pub local_path: String,
    pub content_type: String,
    pub last_write: i64,
}

pub async fn upsert_blob(
    db: &SqlitePool,
    blob_hash: &str,
    local_path: &str,
    content_type: &str,
    byte_size: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO image_blobs (blob_hash, local_path, content_type, byte_size, created_at)
        VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(blob_hash) DO UPDATE SET
            local_path = excluded.local_path,
            content_type = excluded.content_type,
            byte_size = excluded.byte_size
        "#,
    )
    .bind(blob_hash)
    .bind(local_path)
    .bind(content_type)
    .bind(byte_size)
    .execute(db)
    .await?;
    Ok(())
}

/// Returns the `local_path` stored for a given blob hash, or `None` if no
/// such blob row exists. Callers use this both as an existence check and
/// to verify the on-disk file is still where the DB says it is — older
/// builds wrote relative paths to this column, which break when the
/// runtime CWD changes. See `services::artwork::cache_image` for the
/// self-heal path.
pub async fn get_blob_path(
    db: &SqlitePool,
    blob_hash: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(r#"SELECT local_path FROM image_blobs WHERE blob_hash = ?"#)
        .bind(blob_hash)
        .fetch_optional(db)
        .await?;

    Ok(row.map(|r| r.get::<String, _>("local_path")))
}

/// Payload for `upsert_ref`. Named fields so callers can't swap the
/// four `&str` arguments (cache_key / parent_kind / image_kind /
/// source_url) without the compiler noticing — swapping two of those
/// silently corrupts the cache index, and the call arrives in a
/// background task where the error wouldn't surface until someone
/// notices wrong artwork.
pub struct RefUpsert<'a> {
    pub cache_key: &'a str,
    pub parent_kind: &'a str,
    pub parent_id: Option<i64>,
    pub image_kind: &'a str,
    pub source_url: &'a str,
    pub blob_hash: &'a str,
    pub last_write: i64,
}

pub async fn upsert_ref(db: &SqlitePool, r: RefUpsert<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO image_refs (
            cache_key, parent_kind, parent_id, image_kind,
            source_url, blob_hash, last_write, cached_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(cache_key) DO UPDATE SET
            parent_kind = excluded.parent_kind,
            parent_id = excluded.parent_id,
            image_kind = excluded.image_kind,
            source_url = excluded.source_url,
            blob_hash = excluded.blob_hash,
            last_write = excluded.last_write,
            cached_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(r.cache_key)
    .bind(r.parent_kind)
    .bind(r.parent_id)
    .bind(r.image_kind)
    .bind(r.source_url)
    .bind(r.blob_hash)
    .bind(r.last_write)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get(db: &SqlitePool, cache_key: &str) -> Result<Option<ArtworkEntry>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT ir.cache_key, ib.local_path, ib.content_type, ir.last_write
        FROM image_refs ir
        JOIN image_blobs ib ON ib.blob_hash = ir.blob_hash
        WHERE ir.cache_key = ?
        "#,
    )
    .bind(cache_key)
    .fetch_optional(db)
    .await?;

    Ok(row.map(|row| ArtworkEntry {
        cache_key: row.get("cache_key"),
        local_path: row.get("local_path"),
        content_type: row.get("content_type"),
        last_write: row.get("last_write"),
    }))
}

pub async fn get_local_url(
    db: &SqlitePool,
    cache_key: &str,
) -> Result<Option<String>, sqlx::Error> {
    Ok(get(db, cache_key)
        .await?
        .map(|e| format!("/media/art/{}?v={}", e.cache_key, e.last_write)))
}

/// Batch variant of `get_local_url`: fetches `/media/art/...` URLs for a
/// set of cache keys in a single SQL round trip. Used by list pages
/// (library index, needs-review, etc.) that previously fired one serial
/// query per row — for a 200-series library that was 200 sequential DB
/// hits just to render the covers.
///
/// Keys with no cached row are omitted from the returned map; callers
/// should fall back to the source URL in that case.
pub async fn get_local_urls_batch(
    db: &SqlitePool,
    cache_keys: &[String],
) -> Result<HashMap<String, String>, sqlx::Error> {
    if cache_keys.is_empty() {
        return Ok(HashMap::new());
    }

    // Build the `IN (?, ?, ...)` placeholder list at runtime. sqlx doesn't
    // expand slice bindings on SQLite, so we splice the placeholders into
    // the SQL string and bind each key individually. The key set comes
    // from trusted server-side format strings (`series-<id>-cover`), not
    // user input, so there's no injection surface here.
    let placeholders = std::iter::repeat_n("?", cache_keys.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        r#"
        SELECT ir.cache_key, ir.last_write
        FROM image_refs ir
        JOIN image_blobs ib ON ib.blob_hash = ir.blob_hash
        WHERE ir.cache_key IN ({})
        "#,
        placeholders
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for key in cache_keys {
        query = query.bind(key);
    }

    let rows = query.fetch_all(db).await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let key: String = row.get("cache_key");
        let last_write: i64 = row.get("last_write");
        out.insert(key.clone(), format!("/media/art/{}?v={}", key, last_write));
    }
    Ok(out)
}

/// Two-step prune of the artwork cache, called from the hourly cleanup
/// task. Returns `(refs_deleted, blobs_deleted)`. Without this the
/// `image_refs` and `image_blobs` tables — and the on-disk blob files
/// they point at — only ever grow: removing a series leaves orphan
/// rows, renaming a cover URL leaves the old blob behind, etc.
///
/// Step 1 — drop refs whose parent series no longer exists.
/// Step 2 — drop blobs (and the backing files) that no ref references
/// AND that haven't been written in `min_age_days` (the age gate
/// covers the small race in cache_image where upsert_blob runs before
/// upsert_ref — we don't want a concurrent prune to delete a blob
/// that's about to get its ref written).
pub async fn cleanup_orphans(
    db: &SqlitePool,
    min_age_days: i64,
) -> Result<(u64, u64), sqlx::Error> {
    // Step 1: orphan refs. Only series-keyed refs are pruned here —
    // any other parent_kind is left alone since we don't own its
    // identity table.
    let refs_deleted = sqlx::query(
        "DELETE FROM image_refs
         WHERE parent_kind IN ('series', 'series_relation')
           AND parent_id IS NOT NULL
           AND parent_id NOT IN (SELECT id FROM series)",
    )
    .execute(db)
    .await?
    .rows_affected();

    // Step 2: orphan blobs. DELETE … RETURNING in a single statement
    // so the file removal is *consequent on* the DB delete, not
    // concurrent with it. The earlier SELECT-then-remove_file-then-
    // DELETE order had a TOCTOU window where a concurrent cache_image
    // could land an upsert_ref between the SELECT and the DELETE: the
    // SELECT-stage decision to remove the file was based on "no refs",
    // but by DELETE time the new ref's NOT EXISTS check would fail and
    // the row would survive — leaving a live ref pointing at an
    // already-deleted on-disk file. With DELETE … RETURNING, only rows
    // whose NOT EXISTS check still holds at delete-commit time return
    // their local_path, and the file removal that follows can't beat a
    // concurrent ref to the punch.
    let age_filter = format!("-{} days", min_age_days);
    let deleted_rows = sqlx::query(
        r#"DELETE FROM image_blobs
           WHERE NOT EXISTS (SELECT 1 FROM image_refs r WHERE r.blob_hash = image_blobs.blob_hash)
             AND created_at < datetime('now', ?)
           RETURNING local_path"#,
    )
    .bind(&age_filter)
    .fetch_all(db)
    .await?;

    let blobs_deleted = deleted_rows.len() as u64;
    for row in &deleted_rows {
        let local_path: String = row.get("local_path");
        if !local_path.is_empty() {
            // Hourly task in an async path — match the rest of the
            // codebase's std::fs → tokio::fs migration so we don't
            // block the runtime executor on the unlink syscall.
            if tokio::fs::remove_file(&local_path).await.is_err()
                && let Some(moved) = crate::services::artwork::relocated_blob_path(&local_path)
            {
                // Row written before the data dir moved (#259).
                let _ = tokio::fs::remove_file(moved).await;
            }
        }
    }

    Ok((refs_deleted, blobs_deleted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{in_memory_pool, seed_series};

    fn ref_for<'a>(
        cache_key: &'a str,
        parent_id: Option<i64>,
        blob_hash: &'a str,
    ) -> RefUpsert<'a> {
        RefUpsert {
            cache_key,
            parent_kind: "series",
            parent_id,
            image_kind: "cover",
            source_url: "https://example/cover.jpg",
            blob_hash,
            last_write: 1234567890,
        }
    }

    // ── upsert_blob / get_blob_path ─────────────────────────────────

    #[tokio::test]
    async fn upsert_blob_then_get_path_round_trips() {
        let db = in_memory_pool().await;
        upsert_blob(&db, "hash-a", "/data/cache/a.jpg", "image/jpeg", 1024)
            .await
            .unwrap();
        let path = get_blob_path(&db, "hash-a").await.unwrap();
        assert_eq!(path.as_deref(), Some("/data/cache/a.jpg"));
    }

    #[tokio::test]
    async fn get_blob_path_returns_none_on_miss() {
        let db = in_memory_pool().await;
        assert!(get_blob_path(&db, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_blob_overwrites_path_on_conflict() {
        // Self-heal path — a blob row written by an older build with a
        // relative `local_path` gets corrected on the next cache_image
        // call. Pin the ON CONFLICT branch so a future refactor can't
        // silently drop it.
        let db = in_memory_pool().await;
        upsert_blob(&db, "hash-x", "old/relative.jpg", "image/jpeg", 1)
            .await
            .unwrap();
        upsert_blob(&db, "hash-x", "/data/cache/x.jpg", "image/png", 2048)
            .await
            .unwrap();
        let path = get_blob_path(&db, "hash-x").await.unwrap();
        assert_eq!(path.as_deref(), Some("/data/cache/x.jpg"));
    }

    // ── upsert_ref / get / get_local_url ────────────────────────────

    #[tokio::test]
    async fn upsert_ref_then_get_joins_through_image_blobs() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1, "Show").await;
        upsert_blob(&db, "h1", "/data/blob.jpg", "image/jpeg", 100)
            .await
            .unwrap();
        upsert_ref(&db, ref_for("series-1-cover", Some(series_id), "h1"))
            .await
            .unwrap();

        let entry = get(&db, "series-1-cover").await.unwrap().expect("hit");
        assert_eq!(entry.cache_key, "series-1-cover");
        assert_eq!(entry.local_path, "/data/blob.jpg");
        assert_eq!(entry.content_type, "image/jpeg");
        assert_eq!(entry.last_write, 1234567890);
    }

    #[tokio::test]
    async fn upsert_ref_with_null_parent_id_round_trips() {
        // parent_id can be NULL — refs that aren't series-keyed (e.g.
        // arbitrary cache_keys not tied to a tracked series id) skip
        // the FK constraint and live in the table on their own.
        let db = in_memory_pool().await;
        upsert_blob(&db, "h-orphan", "/data/o.jpg", "image/jpeg", 1)
            .await
            .unwrap();
        upsert_ref(&db, ref_for("misc:cover", None, "h-orphan"))
            .await
            .unwrap();
        assert!(get(&db, "misc:cover").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn get_local_url_includes_cache_busting_version_query() {
        // Format `/media/art/<key>?v=<last_write>` lets the browser
        // cache aggressively while still picking up new versions when
        // the upstream cover changes (last_write bumps). Pin the shape.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 42, "Show").await;
        upsert_blob(&db, "h2", "/data/x.jpg", "image/jpeg", 1)
            .await
            .unwrap();
        upsert_ref(&db, ref_for("series-42-cover", Some(series_id), "h2"))
            .await
            .unwrap();

        let url = get_local_url(&db, "series-42-cover")
            .await
            .unwrap()
            .expect("hit");
        assert_eq!(url, "/media/art/series-42-cover?v=1234567890");
    }

    #[tokio::test]
    async fn get_local_url_returns_none_for_missing_key() {
        let db = in_memory_pool().await;
        assert!(get_local_url(&db, "nope").await.unwrap().is_none());
    }

    // ── get_local_urls_batch ────────────────────────────────────────

    #[tokio::test]
    async fn get_local_urls_batch_handles_empty_input_without_db_round_trip() {
        let db = in_memory_pool().await;
        let result = get_local_urls_batch(&db, &[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_local_urls_batch_returns_one_url_per_known_key() {
        let db = in_memory_pool().await;
        let s1 = seed_series(&db, 1, "Show A").await;
        let s2 = seed_series(&db, 2, "Show B").await;
        upsert_blob(&db, "h1", "/data/a.jpg", "image/jpeg", 1)
            .await
            .unwrap();
        upsert_blob(&db, "h2", "/data/b.jpg", "image/jpeg", 1)
            .await
            .unwrap();
        let mut r1 = ref_for("series-1-cover", Some(s1), "h1");
        r1.last_write = 1000;
        let mut r2 = ref_for("series-2-cover", Some(s2), "h2");
        r2.last_write = 2000;
        upsert_ref(&db, r1).await.unwrap();
        upsert_ref(&db, r2).await.unwrap();

        let result = get_local_urls_batch(
            &db,
            &[
                "series-1-cover".into(),
                "series-2-cover".into(),
                "missing".into(),
            ],
        )
        .await
        .unwrap();
        // Missing keys are dropped — caller falls back to source URL.
        assert_eq!(result.len(), 2);
        assert_eq!(
            result.get("series-1-cover").unwrap(),
            "/media/art/series-1-cover?v=1000"
        );
        assert_eq!(
            result.get("series-2-cover").unwrap(),
            "/media/art/series-2-cover?v=2000"
        );
    }

    // ── cleanup_orphans ──────────────────────────────────────────────

    #[tokio::test]
    async fn cleanup_orphans_is_a_noop_under_steady_state_fk_enforcement() {
        // Step-1 prune (orphan refs whose parent series_id no longer
        // exists) is a defensive cleanup against historical pre-FK
        // orphan rows; with `PRAGMA foreign_keys = ON` (sqlx's
        // default) `image_refs.parent_id` is FK'd to `series.id` ON
        // DELETE CASCADE, so a series remove already drops its refs
        // in the same statement. There's no in-band way to produce
        // an orphan ref against an active pool — the WHERE NOT IN
        // (SELECT id FROM series) clause matches zero rows.
        //
        // What we *can* pin: the prune runs cleanly against a live
        // ref/blob pair without false-positively dropping it.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        upsert_blob(&db, "h-live", "/x.jpg", "image/jpeg", 1)
            .await
            .unwrap();
        upsert_ref(&db, ref_for("live", Some(series_id), "h-live"))
            .await
            .unwrap();

        let (refs_deleted, _blobs) = cleanup_orphans(&db, 30).await.unwrap();
        assert_eq!(refs_deleted, 0);
        assert!(get(&db, "live").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn cleanup_orphans_drops_aged_blobs_without_refs() {
        // Step 2: a blob with no ref AND created > min_age_days ago is
        // pruned. Use a real on-disk file in a tempdir so we can
        // verify the file is also removed.
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orphan.jpg");
        tokio::fs::write(&path, b"data").await.unwrap();
        let path_str = path.to_string_lossy().into_owned();
        upsert_blob(&db, "h-aged", &path_str, "image/jpeg", 4)
            .await
            .unwrap();
        // Backdate created_at past the cutoff.
        sqlx::query("UPDATE image_blobs SET created_at = datetime('now', '-60 days')")
            .execute(&db)
            .await
            .unwrap();

        let (_refs, blobs_deleted) = cleanup_orphans(&db, 30).await.unwrap();
        assert_eq!(blobs_deleted, 1);
        assert!(!path.exists(), "on-disk file must be removed");
        // Blob row gone too.
        assert!(get_blob_path(&db, "h-aged").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cleanup_orphans_keeps_recent_orphan_blobs_via_age_gate() {
        // The age gate covers the cache_image race: upsert_blob lands
        // first, upsert_ref follows. A concurrent prune in that window
        // would delete the just-written blob. Pin the gate so a
        // refactor that drops the age check doesn't reintroduce the
        // race.
        let db = in_memory_pool().await;
        upsert_blob(&db, "h-fresh", "/tmp/x.jpg", "image/jpeg", 1)
            .await
            .unwrap();
        // Don't backdate — created_at is now.
        let (_refs, blobs_deleted) = cleanup_orphans(&db, 1).await.unwrap();
        assert_eq!(blobs_deleted, 0);
        assert!(get_blob_path(&db, "h-fresh").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn cleanup_orphans_returns_zero_zero_on_empty_tables() {
        let db = in_memory_pool().await;
        let (refs, blobs) = cleanup_orphans(&db, 30).await.unwrap();
        assert_eq!(refs, 0);
        assert_eq!(blobs, 0);
    }
}
