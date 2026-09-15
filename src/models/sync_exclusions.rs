//! Watch-list sync exclusions: Sonarr's import-list exclusions. A
//! series removed with "keep it off my watch-list sync" lands here,
//! and `external_sync::merge` skips a list entry that matches until
//! the row is deleted from Settings → Integrations.
//!
//! Keyed by AniList id with a MAL id beside it, since the MAL fallback
//! path (`merge_jikan_fallback_entries`) only has the MAL id. Either
//! id matching is enough.

use std::collections::HashSet;

use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct SyncExclusion {
    pub id: i64,
    pub anilist_id: Option<i64>,
    pub mal_id: Option<i64>,
    pub title: String,
    pub created_at: String,
}

/// Record an exclusion. A positive AniList id or a MAL id is required
/// (a MAL-only series carries the negative sentinel as its AniList id,
/// which is dropped here). Idempotent: a row with both ids is left
/// alone; a row with only one of them gains the other, so the merge
/// loop that has only the MAL id (the Jikan fallback) and the one that
/// has only the AniList id both match it; the partial unique indexes
/// make a concurrent double insert a no-op. `Ok(true)` when a row was
/// written or completed.
pub async fn add(
    db: &SqlitePool,
    anilist_id: i64,
    mal_id: Option<i64>,
    title: &str,
) -> Result<bool, sqlx::Error> {
    let anilist = (anilist_id > 0).then_some(anilist_id);
    let mal = mal_id.filter(|m| *m > 0);
    if anilist.is_none() && mal.is_none() {
        return Ok(false);
    }
    if let Some(a) = anilist {
        let by_anilist: Option<(i64, Option<i64>)> =
            sqlx::query_as("SELECT id, mal_id FROM external_sync_exclusions WHERE anilist_id = ?")
                .bind(a)
                .fetch_optional(db)
                .await?;
        if let Some((id, existing_mal)) = by_anilist {
            if existing_mal.is_none()
                && let Some(m) = mal
            {
                let result = sqlx::query(
                    "UPDATE OR IGNORE external_sync_exclusions SET mal_id = ? \
                     WHERE id = ? AND mal_id IS NULL",
                )
                .bind(m)
                .bind(id)
                .execute(db)
                .await?;
                return Ok(result.rows_affected() > 0);
            }
            return Ok(false);
        }
    }
    if let Some(m) = mal {
        let by_mal: Option<i64> =
            sqlx::query_scalar("SELECT id FROM external_sync_exclusions WHERE mal_id = ?")
                .bind(m)
                .fetch_optional(db)
                .await?;
        if let Some(id) = by_mal {
            if let Some(a) = anilist {
                sqlx::query(
                    "UPDATE external_sync_exclusions SET anilist_id = ? WHERE id = ? AND anilist_id IS NULL",
                )
                .bind(a)
                .bind(id)
                .execute(db)
                .await?;
                return Ok(true);
            }
            return Ok(false);
        }
    }
    let result = sqlx::query(
        "INSERT OR IGNORE INTO external_sync_exclusions (anilist_id, mal_id, title) VALUES (?, ?, ?)",
    )
    .bind(anilist)
    .bind(mal)
    .bind(title)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Drop every exclusion that matches either id: a series added back
/// by hand is wanted again.
pub async fn delete_for_ids(
    db: &SqlitePool,
    anilist_id: i64,
    mal_id: Option<i64>,
) -> Result<u64, sqlx::Error> {
    let anilist = (anilist_id > 0).then_some(anilist_id);
    let mal = mal_id.filter(|m| *m > 0);
    if anilist.is_none() && mal.is_none() {
        return Ok(0);
    }
    let result = sqlx::query(
        "DELETE FROM external_sync_exclusions \
         WHERE (? IS NOT NULL AND anilist_id = ?) OR (? IS NOT NULL AND mal_id = ?)",
    )
    .bind(anilist)
    .bind(anilist)
    .bind(mal)
    .bind(mal)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

pub async fn list(db: &SqlitePool) -> Result<Vec<SyncExclusion>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, anilist_id, mal_id, title, created_at \
         FROM external_sync_exclusions ORDER BY title COLLATE NOCASE, id",
    )
    .fetch_all(db)
    .await
}

pub async fn delete(db: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM external_sync_exclusions WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Every excluded id, loaded once per sync so the merge loop does one
/// query instead of one per entry.
#[derive(Debug, Default, Clone)]
pub struct ExclusionSet {
    anilist: HashSet<i64>,
    mal: HashSet<i64>,
}

impl ExclusionSet {
    pub fn is_empty(&self) -> bool {
        self.anilist.is_empty() && self.mal.is_empty()
    }

    /// True when either id is excluded. `anilist_id` may be the
    /// negative MAL sentinel, which never matches.
    pub fn contains(&self, anilist_id: i64, mal_id: Option<i64>) -> bool {
        (anilist_id > 0 && self.anilist.contains(&anilist_id))
            || mal_id.is_some_and(|m| m > 0 && self.mal.contains(&m))
    }
}

pub async fn load_set(db: &SqlitePool) -> ExclusionSet {
    let rows: Vec<(Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT anilist_id, mal_id FROM external_sync_exclusions")
            .fetch_all(db)
            .await
            .unwrap_or_default();
    let mut set = ExclusionSet::default();
    for (anilist, mal) in rows {
        if let Some(a) = anilist {
            set.anilist.insert(a);
        }
        if let Some(m) = mal {
            set.mal.insert(m);
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    #[tokio::test]
    async fn add_list_match_delete_round_trip() {
        let db = in_memory_pool().await;
        assert!(add(&db, 100, Some(200), "Show").await.unwrap());
        // Same ids again: no duplicate.
        assert!(!add(&db, 100, None, "Show").await.unwrap());
        assert!(!add(&db, -1, Some(200), "Show").await.unwrap());
        assert!(!add(&db, 100, Some(200), "Show").await.unwrap());
        // An AniList-only row gains the MAL id when the series is
        // removed again with both known, so the MAL fallback merge
        // loop matches it too.
        assert!(add(&db, 400, None, "Half").await.unwrap());
        assert!(!load_set(&db).await.contains(-400, Some(401)));
        assert!(add(&db, 400, Some(401), "Half").await.unwrap());
        assert!(load_set(&db).await.contains(-400, Some(401)));
        assert!(!add(&db, 400, Some(401), "Half").await.unwrap());
        assert_eq!(delete_for_ids(&db, 0, Some(401)).await.unwrap(), 1);
        // A MAL-only series (negative AniList sentinel) keys by MAL id.
        assert!(add(&db, -300, Some(300), "Other").await.unwrap());
        // Nothing to key on.
        assert!(!add(&db, 0, None, "Nothing").await.unwrap());
        let rows = list(&db).await.unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[1].title, "Show");
        let set = load_set(&db).await;
        assert!(set.contains(100, None));
        assert!(set.contains(-1, Some(200)));
        assert!(set.contains(-300, Some(300)));
        assert!(!set.contains(101, Some(201)));
        assert!(
            !set.contains(-100, None),
            "the negative sentinel never matches"
        );
        let show = rows.iter().find(|r| r.title == "Show").unwrap();
        assert!(delete(&db, show.id).await.unwrap());
        assert!(!delete(&db, show.id).await.unwrap());
        assert!(!load_set(&db).await.contains(100, Some(200)));
        // A MAL-only row gains the AniList id when the series is
        // removed again with both known, and delete_for_ids clears by
        // either id.
        assert!(add(&db, 500, Some(300), "Other").await.unwrap());
        let set = load_set(&db).await;
        assert!(set.contains(500, None));
        assert!(set.contains(-300, Some(300)));
        assert_eq!(delete_for_ids(&db, 500, None).await.unwrap(), 1);
        assert!(!load_set(&db).await.contains(-300, Some(300)));
    }
}
