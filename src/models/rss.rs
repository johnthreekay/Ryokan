use serde::Serialize;
use sqlx::SqlitePool;

// sqlx row-tuple shapes for the `rss_runs` and `rss_seen` SELECTs.
// Aliased so the call-site annotations don't blow past Clippy's
// `type_complexity` lint and — more importantly — so a schema change
// has exactly one place to edit instead of two parallel tuple
// declarations that have to stay in lock-step.
type RssRunRow = (
    i64,
    String,
    Option<String>,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    String,
);

type RssDecisionRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
);

#[derive(Debug, Clone, Serialize)]
pub struct RssRun {
    pub id: i64,
    pub started_at: String,
    pub finished_at: String,
    pub trigger_source: String,
    pub status: String,
    pub items_seen: i32,
    pub matched: i32,
    pub grabbed: i32,
    pub skipped: i32,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RssDecision {
    pub id: i64,
    pub created_at: String,
    pub title: String,
    pub series_title: String,
    pub group_name: String,
    pub decision: String,
    pub reason: String,
    pub source: String,
    pub is_batch: bool,
}

pub async fn start_run(db: &SqlitePool, trigger_source: &str) -> Result<i64, sqlx::Error> {
    let result = sqlx::query("INSERT INTO rss_runs (trigger_source, status) VALUES (?, 'running')")
        .bind(trigger_source)
        .execute(db)
        .await?;
    Ok(result.last_insert_rowid())
}

/// Per-run counters that `finish_run` writes back to `rss_runs`. Named
/// fields instead of four positional `i32`s so callers can't swap
/// `matched` and `grabbed` (or similar) by mistake at the callsite —
/// the compiler won't catch it when every slot is the same type.
pub struct RunSummary<'a> {
    pub status: &'a str,
    pub items_seen: i32,
    pub matched: i32,
    pub grabbed: i32,
    pub skipped: i32,
    pub detail: &'a str,
}

pub async fn finish_run(
    db: &SqlitePool,
    id: i64,
    summary: RunSummary<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE rss_runs
           SET finished_at = CURRENT_TIMESTAMP,
               status = ?,
               items_seen = ?,
               matched = ?,
               grabbed = ?,
               skipped = ?,
               detail = ?
         WHERE id = ?"#,
    )
    .bind(summary.status)
    .bind(summary.items_seen)
    .bind(summary.matched)
    .bind(summary.grabbed)
    .bind(summary.skipped)
    .bind(summary.detail)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Returns every item_key in `rss_seen` with `decision = 'grabbed'`
/// as a `HashSet` so callers can do membership checks in O(1) without
/// a DB round-trip per lookup. The hourly cleanup task prunes rows
/// older than 30 days, so the working set stays bounded for any active
/// install.
///
/// Used by the RSS sync loop, which previously did one SELECT per
/// feed item — Nyaa returns ~100 items per feed × multiple categories,
/// so a single sync was ~100+ sequential round-trips against the same
/// table before any other work happened.
///
/// **Source-blind**: returns just the item_key, ignoring source +
/// source_id. Use [`grabbed_item_keys_scoped`] when the per-source
/// dedup scoping (multi-rss commit E) matters.
pub async fn grabbed_item_keys(
    db: &SqlitePool,
) -> Result<std::collections::HashSet<String>, sqlx::Error> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT item_key FROM rss_seen WHERE decision = 'grabbed'")
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|(k,)| k).collect())
}

/// multi-rss commit F — source-scoped variant of
/// [`grabbed_item_keys`]. Returns a set of `(item_key, source,
/// source_id)` triples so the sync loop's dedup check honors the
/// per-source scoping introduced in commit E.
///
/// Without this, three sources can produce identical numeric GUIDs
/// (different sites' internal IDs) and a SubsPlease item silently
/// dedups against an unrelated Nyaa item that happens to share the
/// GUID. The composite index `idx_rss_seen_source_key` covers this
/// query shape.
pub async fn grabbed_item_keys_scoped(
    db: &SqlitePool,
) -> Result<std::collections::HashSet<(String, String, Option<i64>)>, sqlx::Error> {
    let rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT item_key, source, source_id FROM rss_seen WHERE decision = 'grabbed'",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Payload for `record_decision`. Named fields instead of six `&str`s
/// in a row — swapping `title`/`link` or `decision`/`reason` at a
/// callsite was a silent-corruption risk the compiler couldn't catch,
/// and Clippy was already complaining about the arg count.
///
/// `source` + `source_id` (multi-rss commit F) carry the per-source
/// dedup scope: `('nyaa', None)` for the Nyaa-direct path,
/// `('indexer', Some(indexer_id))` for indexer-RSS,
/// `('direct', Some(feed_id))` for direct feeds. Without the
/// scoping, three sources can produce identical numeric GUIDs and
/// silently dedup against each other.
pub struct DecisionRecord<'a> {
    pub item_key: &'a str,
    pub title: &'a str,
    pub link: &'a str,
    pub series_id: Option<i64>,
    pub series_title: &'a str,
    pub group_name: &'a str,
    pub is_batch: bool,
    pub decision: &'a str,
    pub reason: &'a str,
    pub source: &'a str,
    pub source_id: Option<i64>,
}

/// Upserts the decision for `item_key`. A `grabbed` row is the dedup
/// tombstone [`grabbed_item_keys_scoped`] reads, so a later decision
/// that is not itself a grab never replaces it (the `WHERE` on the
/// conflict arm): overwriting it with "skipped" took the item out of
/// the dedup set, and the sync after that grabbed it again.
pub async fn record_decision(
    db: &SqlitePool,
    record: DecisionRecord<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO rss_seen
           (item_key, title, link, series_id, series_title, group_name, is_batch, decision, reason, source, source_id, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
           ON CONFLICT(item_key) DO UPDATE SET
               title = excluded.title,
               link = excluded.link,
               series_id = excluded.series_id,
               series_title = excluded.series_title,
               group_name = excluded.group_name,
               is_batch = excluded.is_batch,
               decision = excluded.decision,
               reason = excluded.reason,
               source = excluded.source,
               source_id = excluded.source_id,
               created_at = CURRENT_TIMESTAMP
           WHERE rss_seen.decision != 'grabbed' OR excluded.decision = 'grabbed'"#,
    )
    .bind(record.item_key)
    .bind(record.title)
    .bind(record.link)
    .bind(record.series_id)
    .bind(record.series_title)
    .bind(record.group_name)
    .bind(i64::from(record.is_batch))
    .bind(record.decision)
    .bind(record.reason)
    .bind(record.source)
    .bind(record.source_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn latest_run(db: &SqlitePool) -> Result<Option<RssRun>, sqlx::Error> {
    let row: Option<RssRunRow> = sqlx::query_as(
        r#"SELECT id, started_at, finished_at, trigger_source, status, items_seen, matched, grabbed, skipped, detail
           FROM rss_runs ORDER BY id DESC LIMIT 1"#,
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(
        |(
            id,
            started_at,
            finished_at,
            trigger_source,
            status,
            items_seen,
            matched,
            grabbed,
            skipped,
            detail,
        )| RssRun {
            id,
            started_at,
            finished_at: finished_at.unwrap_or_default(),
            trigger_source,
            status,
            items_seen: items_seen as i32,
            matched: matched as i32,
            grabbed: grabbed as i32,
            skipped: skipped as i32,
            detail,
        },
    ))
}

pub async fn recent_decisions(
    db: &SqlitePool,
    limit: i64,
) -> Result<Vec<RssDecision>, sqlx::Error> {
    recent_decisions_paginated(db, limit, None).await
}

/// Cursor-paginated variant of [`recent_decisions`]. Pass
/// `Some(before_id)` to fetch entries with `id < before_id`. Used by
/// the System → RSS tab's "Older →" pagination link.
/// Same row shape as `recent_decisions` so the template renders both
/// the first-page and the older-page paths through one loop.
pub async fn recent_decisions_paginated(
    db: &SqlitePool,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<RssDecision>, sqlx::Error> {
    let rows: Vec<RssDecisionRow> = if let Some(before) = before_id {
        sqlx::query_as(
            r#"SELECT id, created_at, title, series_title, group_name, decision, reason, source, is_batch
               FROM rss_seen WHERE id < ? ORDER BY id DESC LIMIT ?"#,
        )
        .bind(before)
        .bind(limit)
        .fetch_all(db)
        .await?
    } else {
        sqlx::query_as(
            r#"SELECT id, created_at, title, series_title, group_name, decision, reason, source, is_batch
               FROM rss_seen ORDER BY id DESC LIMIT ?"#,
        )
        .bind(limit)
        .fetch_all(db)
        .await?
    };

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                created_at,
                title,
                series_title,
                group_name,
                decision,
                reason,
                source,
                is_batch,
            )| RssDecision {
                id,
                created_at,
                title,
                series_title,
                group_name,
                decision,
                reason,
                source,
                is_batch: is_batch != 0,
            },
        )
        .collect())
}

pub async fn grabbed_titles(db: &SqlitePool, limit: i64) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"SELECT title FROM rss_seen WHERE decision = 'grabbed' ORDER BY id DESC LIMIT ?"#,
    )
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(|(title,)| title).collect())
}

/// Delete all RSS decisions older than `days` days (grabbed included).
/// Nyaa's RSS feed only holds items for ~2 days, so 30-day retention is
/// more than enough to prevent re-grabs while still allowing re-downloads
/// after a file is deleted from disk.
pub async fn cleanup_old_decisions(db: &SqlitePool, days: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"DELETE FROM rss_seen
           WHERE created_at < datetime('now', '-' || ? || ' days')"#,
    )
    .bind(days)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Clear all grab history so previously-grabbed items can be re-evaluated.
pub async fn clear_grab_history(db: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM rss_seen WHERE decision = 'grabbed'")
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// Clear all RSS decision history.
#[allow(dead_code)]
pub async fn clear_all_history(db: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM rss_seen").execute(db).await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    fn record<'a>(item_key: &'a str, title: &'a str, decision: &'a str) -> DecisionRecord<'a> {
        DecisionRecord {
            item_key,
            title,
            link: "https://nyaa.si/view/123",
            series_id: None,
            series_title: "Show",
            group_name: "GroupX",
            is_batch: false,
            decision,
            reason: "test",
            source: "nyaa",
            source_id: None,
        }
    }

    #[tokio::test]
    async fn start_run_then_finish_run_round_trips_summary() {
        let db = in_memory_pool().await;
        let id = start_run(&db, "scheduled").await.unwrap();
        finish_run(
            &db,
            id,
            RunSummary {
                status: "ok",
                items_seen: 100,
                matched: 5,
                grabbed: 3,
                skipped: 2,
                detail: "done",
            },
        )
        .await
        .unwrap();

        let run = latest_run(&db).await.unwrap().expect("a run");
        assert_eq!(run.id, id);
        assert_eq!(run.trigger_source, "scheduled");
        assert_eq!(run.status, "ok");
        assert_eq!(run.items_seen, 100);
        assert_eq!(run.matched, 5);
        assert_eq!(run.grabbed, 3);
        assert_eq!(run.skipped, 2);
        assert_eq!(run.detail, "done");
        assert!(!run.finished_at.is_empty());
    }

    #[tokio::test]
    async fn latest_run_returns_none_on_empty_table() {
        let db = in_memory_pool().await;
        assert!(latest_run(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn latest_run_returns_most_recent_by_id() {
        let db = in_memory_pool().await;
        let _first = start_run(&db, "scheduled").await.unwrap();
        let second = start_run(&db, "manual").await.unwrap();
        let run = latest_run(&db).await.unwrap().expect("latest");
        assert_eq!(run.id, second);
        assert_eq!(run.trigger_source, "manual");
        // Unfinished run reads back finished_at as empty string (the
        // NULL→default in `latest_run`'s map).
        assert!(run.finished_at.is_empty());
    }

    #[tokio::test]
    async fn record_decision_inserts_row_and_upserts_on_conflict() {
        let db = in_memory_pool().await;
        record_decision(&db, record("guid:1", "Title v1", "skipped"))
            .await
            .unwrap();
        // Re-record the same key with a different decision — ON
        // CONFLICT(item_key) overwrites in place.
        record_decision(&db, record("guid:1", "Title v2", "grabbed"))
            .await
            .unwrap();

        let recents = recent_decisions(&db, 10).await.unwrap();
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].title, "Title v2");
        assert_eq!(recents[0].decision, "grabbed");
    }

    #[tokio::test]
    async fn record_decision_never_downgrades_a_grabbed_row() {
        // A8-3: the sync's "already grabbed, skipping" write replaced
        // the grabbed row, so the item left the dedup set and every
        // second sync grabbed it again.
        let db = in_memory_pool().await;
        record_decision(&db, record("guid:1", "Grabbed title", "grabbed"))
            .await
            .unwrap();
        for later in ["skipped", "rejected", "failed"] {
            record_decision(&db, record("guid:1", "Later title", later))
                .await
                .unwrap();
        }

        let recents = recent_decisions(&db, 10).await.unwrap();
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].decision, "grabbed");
        assert_eq!(recents[0].title, "Grabbed title");
        assert!(grabbed_item_keys(&db).await.unwrap().contains("guid:1"));

        // A fresh grab of the same key still refreshes the row.
        record_decision(&db, record("guid:1", "Regrabbed title", "grabbed"))
            .await
            .unwrap();
        let recents = recent_decisions(&db, 10).await.unwrap();
        assert_eq!(recents[0].title, "Regrabbed title");
    }

    #[tokio::test]
    async fn grabbed_item_keys_returns_only_grabbed() {
        let db = in_memory_pool().await;
        record_decision(&db, record("k:1", "Grabbed", "grabbed"))
            .await
            .unwrap();
        record_decision(&db, record("k:2", "Skipped", "skipped"))
            .await
            .unwrap();
        record_decision(&db, record("k:3", "Failed", "failed"))
            .await
            .unwrap();

        let keys = grabbed_item_keys(&db).await.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys.contains("k:1"));
    }

    #[tokio::test]
    async fn recent_decisions_orders_desc_and_respects_limit() {
        let db = in_memory_pool().await;
        for i in 0..5 {
            record_decision(
                &db,
                record(&format!("k:{i}"), &format!("Title {i}"), "grabbed"),
            )
            .await
            .unwrap();
        }
        let rows = recent_decisions(&db, 3).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].title, "Title 4");
        assert_eq!(rows[2].title, "Title 2");
    }

    #[tokio::test]
    async fn recent_decisions_paginated_skips_entries_at_or_above_cursor() {
        // Cursor semantics: `before_id = N` returns rows with `id < N`,
        // not `id <= N`. Pin so a regression that flips the comparison
        // (and double-counts the boundary row across pages) fails here.
        let db = in_memory_pool().await;
        for i in 0..6 {
            record_decision(
                &db,
                record(&format!("k:{i}"), &format!("Title {i}"), "grabbed"),
            )
            .await
            .unwrap();
        }
        let first_page = recent_decisions_paginated(&db, 3, None).await.unwrap();
        assert_eq!(first_page.len(), 3);
        assert_eq!(first_page[0].title, "Title 5");
        assert_eq!(first_page[2].title, "Title 3");

        // Use the last row's id as the next-page cursor (the
        // `+1`-truncate trick the handler uses).
        let cursor = first_page.last().unwrap().id;
        let second_page = recent_decisions_paginated(&db, 3, Some(cursor))
            .await
            .unwrap();
        assert_eq!(second_page.len(), 3);
        assert_eq!(second_page[0].title, "Title 2");
        assert!(
            !second_page.iter().any(|r| r.title == "Title 3"),
            "boundary row at the cursor must NOT appear on the next page; got {second_page:?}"
        );
    }

    #[tokio::test]
    async fn recent_decisions_paginated_returns_empty_when_cursor_past_oldest() {
        let db = in_memory_pool().await;
        record_decision(&db, record("only", "Only", "grabbed"))
            .await
            .unwrap();
        let only = recent_decisions_paginated(&db, 10, None).await.unwrap();
        let cursor = only[0].id;
        let past = recent_decisions_paginated(&db, 10, Some(cursor))
            .await
            .unwrap();
        assert!(
            past.is_empty(),
            "cursor at the oldest row's id must return empty page (no `id < cursor`); got {past:?}"
        );
    }

    #[tokio::test]
    async fn grabbed_titles_returns_only_grabbed_in_recent_first_order() {
        let db = in_memory_pool().await;
        record_decision(&db, record("k:1", "First", "grabbed"))
            .await
            .unwrap();
        record_decision(&db, record("k:2", "Skipped", "skipped"))
            .await
            .unwrap();
        record_decision(&db, record("k:3", "Second", "grabbed"))
            .await
            .unwrap();

        let titles = grabbed_titles(&db, 10).await.unwrap();
        assert_eq!(titles, vec!["Second".to_string(), "First".to_string()]);
    }

    #[tokio::test]
    async fn cleanup_old_decisions_removes_only_aged_rows() {
        let db = in_memory_pool().await;
        // Three rows with rolled-back created_at: 5d, 35d, 60d.
        record_decision(&db, record("k:5d", "Recent", "grabbed"))
            .await
            .unwrap();
        record_decision(&db, record("k:35d", "Older", "grabbed"))
            .await
            .unwrap();
        record_decision(&db, record("k:60d", "Oldest", "grabbed"))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE rss_seen SET created_at = datetime('now', '-5 days') WHERE item_key = 'k:5d'",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE rss_seen SET created_at = datetime('now', '-35 days') WHERE item_key = 'k:35d'",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE rss_seen SET created_at = datetime('now', '-60 days') WHERE item_key = 'k:60d'",
        )
        .execute(&db)
        .await
        .unwrap();

        let removed = cleanup_old_decisions(&db, 30).await.unwrap();
        assert_eq!(removed, 2);

        let remaining = recent_decisions(&db, 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].title, "Recent");
    }

    #[tokio::test]
    async fn clear_grab_history_only_removes_grabbed_rows() {
        let db = in_memory_pool().await;
        record_decision(&db, record("k:1", "G", "grabbed"))
            .await
            .unwrap();
        record_decision(&db, record("k:2", "S", "skipped"))
            .await
            .unwrap();

        let removed = clear_grab_history(&db).await.unwrap();
        assert_eq!(removed, 1);

        let remaining = recent_decisions(&db, 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].decision, "skipped");
    }

    #[tokio::test]
    async fn clear_all_history_drops_every_row() {
        let db = in_memory_pool().await;
        record_decision(&db, record("k:1", "G", "grabbed"))
            .await
            .unwrap();
        record_decision(&db, record("k:2", "S", "skipped"))
            .await
            .unwrap();

        let removed = clear_all_history(&db).await.unwrap();
        assert_eq!(removed, 2);
        assert!(recent_decisions(&db, 10).await.unwrap().is_empty());
    }
}
