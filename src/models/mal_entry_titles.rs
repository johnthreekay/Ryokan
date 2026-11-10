//! #303 — The titles of a MAL entry that AniList split into several
//! entries. Steel Ball Run's 1st STAGE (AniList 190327) and 2nd - 3rd
//! STAGE (210482) are both MAL 61469, whose titles ("Steel Ball Run:
//! JoJo no Kimyou na Bouken", "STEEL BALL RUN JoJo's Bizarre Adventure")
//! neither AniList entry carries. Release groups follow the MAL entry,
//! so these are the names their releases use.
//!
//! One row per MAL id, shared by every part of the split. Written by
//! the metadata refresh (`metadata_sync`) for a series that
//! [`is_split_member`] says is one of the parts; read in one query by
//! the RSS matcher ([`all`]).

use std::collections::{HashMap, HashSet};

use sqlx::{Row, SqlitePool};

/// Store `titles` for `mal_id`, replacing what was there and stamping
/// `cached_at` now. Titles are trimmed; blank ones are dropped and later
/// case-insensitive repeats are skipped, so the first spelling wins. A
/// list with nothing left, or a non-positive MAL id, writes nothing and
/// keeps the stored row.
pub async fn set(db: &SqlitePool, mal_id: i64, titles: &[String]) -> Result<(), sqlx::Error> {
    if mal_id <= 0 {
        return Ok(());
    }
    let titles = clean_titles(titles);
    if titles.is_empty() {
        return Ok(());
    }
    let json = serde_json::to_string(&titles).unwrap_or_else(|_| "[]".to_string());
    sqlx::query(
        "INSERT INTO mal_entry_titles (mal_id, titles_json, cached_at) \
         VALUES (?, ?, CURRENT_TIMESTAMP) \
         ON CONFLICT(mal_id) DO UPDATE SET \
             titles_json = excluded.titles_json, \
             cached_at = excluded.cached_at",
    )
    .bind(mal_id)
    .bind(json)
    .execute(db)
    .await?;
    Ok(())
}

fn clean_titles(titles: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    titles
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty() && seen.insert(t.to_lowercase()))
        .map(str::to_string)
        .collect()
}

/// Every stored entry, keyed by MAL id. One query; a read error or a
/// row whose JSON does not parse reads as nothing, since the titles
/// only add matches.
pub async fn all(db: &SqlitePool) -> HashMap<i64, Vec<String>> {
    let rows = match sqlx::query("SELECT mal_id, titles_json FROM mal_entry_titles")
        .fetch_all(db)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!(target: "ryokan::mal_entry_titles", "read failed: {e}");
            return HashMap::new();
        }
    };
    rows.iter()
        .filter_map(|row| {
            let mal_id: i64 = row.try_get("mal_id").ok()?;
            let json: String = row.try_get("titles_json").ok()?;
            let titles: Vec<String> = serde_json::from_str(&json).ok()?;
            (!titles.is_empty()).then_some((mal_id, titles))
        })
        .collect()
}

/// Seconds since the row for `mal_id` was written; `None` when there is
/// no row (or the read failed). A row whose stamp does not parse reads
/// as `i64::MAX`, so it is due a refresh rather than fresh forever.
pub async fn age_secs(db: &SqlitePool, mal_id: i64) -> Option<i64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE( \
             CAST(strftime('%s', 'now') AS INTEGER) - CAST(strftime('%s', cached_at) AS INTEGER), \
             9223372036854775807) \
         FROM mal_entry_titles WHERE mal_id = ?",
    )
    .bind(mal_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
}

/// `true` when the AniList entry `anilist_id` is one part of a MAL entry
/// AniList split into several: its cached relations
/// (`provider_relations_cache`, written by the metadata refresh) name
/// another AniList entry carrying the same MAL id. Any relation type
/// counts, since the first part sees the second as a SEQUEL and the
/// second sees the first as a PREQUEL. A manga relation never counts:
/// its `related_mal_id` is a MAL *manga* id, which can equal an anime id.
///
/// `false` for a MAL-fallback row (negative id: its relations are
/// MAL's, which name no AniList entry), with no MAL id, and when the
/// relations are not cached. A pure cache read, like
/// `local_metadata::split_prior_episodes`.
pub async fn is_split_member(db: &SqlitePool, anilist_id: i64, mal_id: Option<i64>) -> bool {
    let Some(mal_id) = mal_id.filter(|m| *m > 0) else {
        return false;
    };
    if anilist_id <= 0 {
        return false;
    }
    sqlx::query_scalar::<_, i64>(
        "SELECT EXISTS ( \
             SELECT 1 FROM provider_relations_cache \
             WHERE provider_id = ? AND related_mal_id = ? \
               AND related_provider_id > 0 AND related_provider_id <> ? \
               AND media_type <> 'MANGA')",
    )
    .bind(anilist_id)
    .bind(mal_id)
    .bind(anilist_id)
    .fetch_one(db)
    .await
    .map(|n| n != 0)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    async fn relation(
        db: &SqlitePool,
        provider_id: i64,
        related_id: i64,
        related_mal: Option<i64>,
        relation_type: &str,
        media_type: &str,
    ) {
        sqlx::query(
            "INSERT INTO provider_relations_cache \
             (provider_id, related_provider_id, related_mal_id, format, relation_type, media_type) \
             VALUES (?, ?, ?, 'ONA', ?, ?)",
        )
        .bind(provider_id)
        .bind(related_id)
        .bind(related_mal)
        .bind(relation_type)
        .bind(media_type)
        .execute(db)
        .await
        .expect("insert relation");
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn set_and_all_round_trip() {
        let db = in_memory_pool().await;
        assert!(all(&db).await.is_empty());
        assert_eq!(age_secs(&db, 61469).await, None);

        set(
            &db,
            61469,
            &strings(&[
                "Steel Ball Run: JoJo no Kimyou na Bouken",
                "  ",
                "STEEL BALL RUN JoJo's Bizarre Adventure",
                "steel ball run: jojo no kimyou na bouken",
                "",
            ]),
        )
        .await
        .unwrap();
        set(&db, 1, &strings(&["Cowboy Bebop"])).await.unwrap();

        let stored = all(&db).await;
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored[&61469],
            strings(&[
                "Steel Ball Run: JoJo no Kimyou na Bouken",
                "STEEL BALL RUN JoJo's Bizarre Adventure",
            ]),
            "blank titles dropped, the first spelling of a repeat kept"
        );
        assert!(age_secs(&db, 61469).await.is_some_and(|a| a < 60));

        // A second write replaces the list and restamps the row.
        sqlx::query("UPDATE mal_entry_titles SET cached_at = datetime('now', '-8 days')")
            .execute(&db)
            .await
            .unwrap();
        assert!(age_secs(&db, 61469).await.is_some_and(|a| a > 7 * 86_400));
        set(&db, 61469, &strings(&["Steel Ball Run"]))
            .await
            .unwrap();
        assert_eq!(all(&db).await[&61469], strings(&["Steel Ball Run"]));
        assert!(age_secs(&db, 61469).await.is_some_and(|a| a < 60));

        // Nothing usable writes nothing and keeps what is stored.
        set(&db, 61469, &strings(&[" ", ""])).await.unwrap();
        set(&db, -61469, &strings(&["Negative"])).await.unwrap();
        set(&db, 0, &strings(&["Zero"])).await.unwrap();
        let stored = all(&db).await;
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[&61469], strings(&["Steel Ball Run"]));

        // An unreadable row is skipped, not an error for the rest.
        sqlx::query("INSERT INTO mal_entry_titles (mal_id, titles_json) VALUES (5, 'not json')")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(all(&db).await.len(), 2);
    }

    /// Steel Ball Run: 1st STAGE (190327) and 2nd - 3rd STAGE (210482)
    /// are both MAL 61469. Each part's relations name the other with
    /// that MAL id, as a SEQUEL from the first and a PREQUEL from the
    /// second.
    #[tokio::test]
    async fn both_parts_of_a_split_are_members() {
        let db = in_memory_pool().await;
        assert!(!is_split_member(&db, 210482, Some(61469)).await);
        relation(&db, 190327, 210482, Some(61469), "SEQUEL", "ANIME").await;
        relation(&db, 210482, 190327, Some(61469), "PREQUEL", "ANIME").await;
        // Unrelated relations beside them change nothing.
        relation(&db, 210482, 99, Some(52), "ADAPTATION", "MANGA").await;
        assert!(is_split_member(&db, 190327, Some(61469)).await);
        assert!(is_split_member(&db, 210482, Some(61469)).await);
        // Asked with another MAL id, or none, it is no split.
        assert!(!is_split_member(&db, 210482, Some(1)).await);
        assert!(!is_split_member(&db, 210482, None).await);
        assert!(!is_split_member(&db, 210482, Some(0)).await);
    }

    /// An ordinary sequel has its own MAL entry, so its relation carries
    /// another MAL id. A MAL-fallback row's negative id and a MAL-sourced
    /// relation (negative related id) never count, nor does a manga
    /// whose MAL manga id happens to equal the anime's.
    #[tokio::test]
    async fn an_ordinary_sequel_and_a_mal_fallback_row_are_not_members() {
        let db = in_memory_pool().await;
        // Stone Ocean (MAL 48661) before Steel Ball Run (MAL 61469).
        relation(&db, 190327, 131942, Some(48661), "PREQUEL", "ANIME").await;
        relation(&db, 131942, 190327, Some(61469), "SEQUEL", "ANIME").await;
        assert!(!is_split_member(&db, 190327, Some(61469)).await);
        assert!(!is_split_member(&db, 131942, Some(48661)).await);

        // A MAL-fallback row: negative provider id, MAL's relations.
        relation(&db, -61469, -48661, Some(48661), "PREQUEL", "ANIME").await;
        relation(&db, -61469, -61469, Some(61469), "OTHER", "ANIME").await;
        assert!(!is_split_member(&db, -61469, Some(61469)).await);
        // A relation naming the entry itself is no second part.
        relation(&db, 500, 500, Some(7), "OTHER", "ANIME").await;
        assert!(!is_split_member(&db, 500, Some(7)).await);
        // A MAL-sourced relation on a positive row names no AniList entry.
        relation(&db, 600, -8, Some(8), "SEQUEL", "ANIME").await;
        assert!(!is_split_member(&db, 600, Some(8)).await);
        // The source manga's MAL id is a manga id.
        relation(&db, 700, 701, Some(9), "SOURCE", "MANGA").await;
        assert!(!is_split_member(&db, 700, Some(9)).await);
    }
}
