use std::collections::{HashMap, HashSet};

use sqlx::{Row, SqlitePool};

use crate::services::anilist::{AnimeDetail, RelatedEntry};

#[derive(Debug, Clone)]
pub struct CachedEpisodeMetadata {
    pub episode_number: i32,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub aired: String,
    pub source: String,
}

fn relation_is_cacheable(rel: &RelatedEntry) -> bool {
    matches!(rel.media_type.as_str(), "ANIME" | "MUSIC")
}

pub fn normalize_relation_type(rel_type: &str) -> &str {
    match rel_type {
        // MAL/Jikan uses PARENT_STORY for entries that are effectively the franchise
        // parent. Normalizing this lets us dedupe it against incoming SIDE_STORY/PARENT
        // edges instead of rendering the same title twice under slightly different labels.
        "PARENT_STORY" => "PARENT",
        other => other,
    }
}

pub fn reverse_relation_type(rel_type: &str) -> Option<&'static str> {
    match normalize_relation_type(rel_type) {
        "PREQUEL" => Some("SEQUEL"),
        "SEQUEL" => Some("PREQUEL"),
        "SIDE_STORY" => Some("PARENT"),
        "PARENT" => Some("SIDE_STORY"),
        "CHARACTER" => Some("CHARACTER"),
        "SPIN_OFF" => Some("PARENT"),
        // MAL/Jikan "Summary" / "Full Story" links are often one-way editorial
        // pointers rather than true bidirectional graph edges. Reversing them makes
        // recap/compilation entries leak onto pages where MAL does not show them.
        "SUMMARY" => None,
        "FULL_STORY" => None,
        _ => None,
    }
}

fn relation_identity_key(provider_id: i64, mal_id: Option<i64>) -> String {
    if let Some(mal_id) = mal_id {
        format!("mal:{mal_id}")
    } else {
        format!("provider:{provider_id}")
    }
}

fn relation_dedupe_key(rel: &RelatedEntry) -> (String, String) {
    (
        relation_identity_key(rel.id, rel.id_mal),
        normalize_relation_type(&rel.relation_type).to_string(),
    )
}

async fn replace_relations_table(
    db: &SqlitePool,
    table: &str,
    key_col: &str,
    owner_id: i64,
    owner_detail: Option<&AnimeDetail>,
    relations: &[RelatedEntry],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let delete_sql = format!("DELETE FROM {table} WHERE {key_col} = ?");
    sqlx::query(sqlx::AssertSqlSafe(delete_sql))
        .bind(owner_id)
        .execute(&mut *tx)
        .await?;

    let insert_sql = format!(
        "INSERT INTO {table} ({key_col}, related_provider_id, related_mal_id, title_romaji, title_english, title_native, cover_url, format, status, episodes, relation_type, season_year, media_type) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT({key_col}, related_provider_id, relation_type) DO UPDATE SET related_mal_id=excluded.related_mal_id, title_romaji=excluded.title_romaji, title_english=excluded.title_english, title_native=excluded.title_native, cover_url=excluded.cover_url, format=excluded.format, status=excluded.status, episodes=excluded.episodes, season_year=excluded.season_year, media_type=excluded.media_type, cached_at=CURRENT_TIMESTAMP"
    );
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let owner_identity = owner_detail
        .map(|o| relation_identity_key(o.id, o.id_mal))
        .unwrap_or_default();
    for rel in relations.iter().filter(|r| relation_is_cacheable(r)) {
        // Skip self-references.
        if rel.id == owner_id {
            continue;
        }
        if !owner_identity.is_empty() && relation_identity_key(rel.id, rel.id_mal) == owner_identity
        {
            continue;
        }
        let key = relation_dedupe_key(rel);
        if !seen.insert(key) {
            continue;
        }
        sqlx::query(sqlx::AssertSqlSafe(insert_sql.as_str()))
            .bind(owner_id)
            .bind(rel.id)
            .bind(rel.id_mal)
            .bind(&rel.title_romaji)
            .bind(&rel.title_english)
            .bind(&rel.title_native)
            .bind(&rel.cover_url)
            .bind(&rel.format)
            .bind(&rel.status)
            .bind(rel.episodes)
            .bind(normalize_relation_type(&rel.relation_type))
            .bind(rel.season_year)
            .bind(&rel.media_type)
            .execute(&mut *tx)
            .await?;
    }

    if table == "provider_relations_cache"
        && let Some(owner) = owner_detail
    {
        for rel in relations.iter().filter(|r| relation_is_cacheable(r)) {
            let Some(reverse_type) = reverse_relation_type(&rel.relation_type) else {
                continue;
            };
            // Skip self-references — the owner shouldn't point back to itself.
            if rel.id == owner_id {
                continue;
            }
            // Also skip if the identity keys match (catches MAL ID overlap).
            if relation_identity_key(rel.id, rel.id_mal)
                == relation_identity_key(owner.id, owner.id_mal)
            {
                continue;
            }
            sqlx::query(sqlx::AssertSqlSafe(insert_sql.as_str()))
                .bind(rel.id)
                .bind(owner.id)
                .bind(owner.id_mal)
                .bind(&owner.title_romaji)
                .bind(&owner.title_english)
                .bind(&owner.title_native)
                .bind(&owner.cover_url)
                .bind(&owner.format)
                .bind(&owner.status)
                .bind(owner.episodes)
                .bind(reverse_type)
                .bind(owner.season_year)
                .bind("ANIME")
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

async fn get_relations_table(
    db: &SqlitePool,
    table: &str,
    key_col: &str,
    owner_id: i64,
) -> Result<Vec<RelatedEntry>, sqlx::Error> {
    let sql = format!(
        "SELECT related_provider_id, related_mal_id, title_romaji, title_english, title_native, cover_url, format, status, episodes, relation_type, season_year, media_type FROM {table} WHERE {key_col} = ? ORDER BY relation_type, related_provider_id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(owner_id)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(row_to_related).collect())
}

pub async fn get_incoming_relations_for_provider(
    db: &SqlitePool,
    provider_id: i64,
    mal_id: Option<i64>,
) -> Result<Vec<RelatedEntry>, sqlx::Error> {
    // When provider_id is negative (MAL-sourced, id = -mal_id), the related_provider_id
    // column stores the negated MAL ID and related_mal_id stores the positive MAL ID.
    // We match on both but must deduplicate since both can match the same row.
    // Important: for incoming relations, provider_relations_cache rows describe the *target*
    // in their title/cover columns because those columns are copied from the related entry at
    // insert time. Using those columns here makes the source relation card render as the current
    // series, which is exactly the self-card bug seen on Monogatari pages. Always hydrate the
    // source card from provider_metadata_cache instead.
    let rows = sqlx::query(
        r#"
        SELECT pr.provider_id AS source_provider_id,
               CASE
                   WHEN pm.mal_id IS NOT NULL THEN pm.mal_id
                   WHEN pr.provider_id < 0 THEN -pr.provider_id
                   ELSE NULL
               END AS source_mal_id,
               COALESCE(json_extract(pm.detail_json, '$.title_romaji'), '') AS title_romaji,
               COALESCE(json_extract(pm.detail_json, '$.title_english'), '') AS title_english,
               COALESCE(json_extract(pm.detail_json, '$.title_native'), '') AS title_native,
               COALESCE(json_extract(pm.detail_json, '$.cover_url'), '') AS cover_url,
               COALESCE(json_extract(pm.detail_json, '$.format'), '') AS format,
               COALESCE(json_extract(pm.detail_json, '$.status'), '') AS status,
               json_extract(pm.detail_json, '$.episodes') AS episodes,
               pr.relation_type AS relation_type,
               json_extract(pm.detail_json, '$.season_year') AS season_year,
               'ANIME' AS media_type
        FROM provider_relations_cache pr
        LEFT JOIN provider_metadata_cache pm ON pm.provider_id = pr.provider_id
        WHERE pr.related_provider_id = ?
           OR (? IS NOT NULL AND pr.related_mal_id = ?)
        GROUP BY pr.provider_id, pr.relation_type
        ORDER BY pr.relation_type, pr.provider_id
        "#,
    )
    .bind(provider_id)
    .bind(mal_id)
    .bind(mal_id)
    .fetch_all(db)
    .await?;

    let mut seen: HashSet<(String, String)> = HashSet::new();
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let rel_type: String = row.get("relation_type");
            let reverse = reverse_relation_type(&rel_type)?;
            let source_id: i64 = row.get("source_provider_id");
            let source_mal: Option<i64> = row.get::<Option<i64>, _>("source_mal_id");
            let key = (
                relation_identity_key(source_id, source_mal),
                reverse.to_string(),
            );
            if !seen.insert(key) {
                return None;
            }
            Some(RelatedEntry {
                id: source_id,
                id_mal: source_mal,
                title_romaji: row.get("title_romaji"),
                title_english: row.get("title_english"),
                title_native: row.get("title_native"),
                cover_url: row.get("cover_url"),
                format: row.get("format"),
                status: row.get("status"),
                status_display: row.get::<String, _>("status").replace('_', " "),
                episodes: row.get::<Option<i32>, _>("episodes"),
                relation_type: reverse.to_string(),
                season_year: row.get::<Option<i32>, _>("season_year"),
                media_type: row.get("media_type"),
            })
        })
        .collect())
}

fn row_to_related(row: sqlx::sqlite::SqliteRow) -> RelatedEntry {
    RelatedEntry {
        id: row.get("related_provider_id"),
        id_mal: row.get::<Option<i64>, _>("related_mal_id"),
        title_romaji: row.get("title_romaji"),
        title_english: row.get("title_english"),
        title_native: row.get("title_native"),
        cover_url: row.get("cover_url"),
        format: row.get("format"),
        status: row.get("status"),
        status_display: row.get::<String, _>("status").replace('_', " "),
        episodes: row.get::<Option<i32>, _>("episodes"),
        relation_type: row.get("relation_type"),
        season_year: row.get::<Option<i32>, _>("season_year"),
        media_type: row.get("media_type"),
    }
}

pub async fn replace_relations_for_series(
    db: &SqlitePool,
    series_id: i64,
    detail: &AnimeDetail,
) -> Result<(), sqlx::Error> {
    replace_relations_table(
        db,
        "series_relations_cache",
        "series_id",
        series_id,
        Some(detail),
        &detail.relations,
    )
    .await
}

pub async fn replace_relations_for_provider(
    db: &SqlitePool,
    provider_id: i64,
    detail: &AnimeDetail,
) -> Result<(), sqlx::Error> {
    replace_relations_table(
        db,
        "provider_relations_cache",
        "provider_id",
        provider_id,
        Some(detail),
        &detail.relations,
    )
    .await
}

pub async fn get_relations_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Vec<RelatedEntry>, sqlx::Error> {
    get_relations_table(db, "series_relations_cache", "series_id", series_id).await
}

pub async fn get_relations_for_provider(
    db: &SqlitePool,
    provider_id: i64,
) -> Result<Vec<RelatedEntry>, sqlx::Error> {
    get_relations_table(db, "provider_relations_cache", "provider_id", provider_id).await
}

/// #30 — Walk the PREQUEL chain backwards from `root_provider_id` and
/// collect the titles of every ancestor along the way. Used to build
/// franchise-root Nyaa queries so absolute-numbered releases surface
/// (e.g. `[SubsPlease] Jujutsu Kaisen - 56` for a JJK S3 E9 target —
/// the S3 AL entry's own titles carry the "Shimetsu Kaiyuu / Culling
/// Game Part 1" subtitle, which no SubsPlease release uses).
///
/// Returns up to a small set of unique titles. Same walk rules as
/// [`compute_cumulative_prior_episodes`]: TV-format only, pick the
/// larger episode-count branch, cycle-guarded. Titles come from
/// `provider_metadata_cache.detail_json` so they match what
/// `collect_aliases` would produce for that entry if it were fetched
/// directly.
pub async fn resolve_franchise_aliases(db: &SqlitePool, root_provider_id: i64) -> Vec<String> {
    const MAX_DEPTH: usize = 20;

    if root_provider_id == 0 {
        return Vec::new();
    }

    let mut out: Vec<String> = Vec::new();
    let mut current = root_provider_id;
    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(current);

    for _ in 0..MAX_DEPTH {
        let row = sqlx::query(
            "SELECT pr.related_provider_id, \
                    COALESCE(json_extract(pm.detail_json, '$.title_romaji'), '') AS title_romaji, \
                    COALESCE(json_extract(pm.detail_json, '$.title_english'), '') AS title_english, \
                    COALESCE(json_extract(pm.detail_json, '$.title_native'), '') AS title_native \
             FROM provider_relations_cache pr \
             LEFT JOIN provider_metadata_cache pm ON pm.provider_id = pr.related_provider_id \
             WHERE pr.provider_id = ? AND pr.relation_type = 'PREQUEL' AND pr.format = 'TV' \
             ORDER BY COALESCE(pr.episodes, 0) DESC \
             LIMIT 1",
        )
        .bind(current)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();

        let Some(row) = row else { break };
        let prev_id: i64 = row.get("related_provider_id");
        if !visited.insert(prev_id) {
            break;
        }
        for col in ["title_romaji", "title_english", "title_native"] {
            let t: String = row.try_get(col).unwrap_or_default();
            if !t.trim().is_empty() {
                out.push(t);
            }
        }
        current = prev_id;
    }

    // Lowercase-dedupe preserving first occurrence.
    let mut seen: HashSet<String> = HashSet::new();
    out.into_iter()
        .filter(|t| seen.insert(t.to_lowercase()))
        .collect()
}

/// #30 — Walk the PREQUEL chain backwards from `root_provider_id` through
/// `provider_relations_cache` and return the cumulative episode count.
///
/// Used to accept absolute-numbered Nyaa releases against a relative-numbered
/// AL cour target. `[SubsPlease] Jujutsu Kaisen - 56` is JJK S3 E9 because
/// S1 (24 episodes) + S2 (23 episodes) = 47, and 47 + 9 = 56. The cumulative
/// offset for S3 is therefore 47. First-season entries and entries whose
/// relation cache is empty return `0`.
///
/// Why this is a pure cache read:
/// - `metadata_sync::hydrate_relation_tree` already BFS-walks the whole
///   franchise graph and writes both forward and reverse edges to
///   `provider_relations_cache`.
/// - We never fetch AL here — this function is called on the
///   refresh/add-series hot path and must not fan out to the network.
///
/// Format filter:
/// - TV-only. SubsPlease-style absolute numbering is a TV release
///   convention; movies and specials are not part of the cour count
///   (`Jujutsu Kaisen 0` does not bump S3's absolute offset from 47 to
///   48). ONA is deliberately excluded for the same reason — most
///   ONAs are one-off series not folded into a TV franchise's
///   cumulative count.
///
/// Branching:
/// - When an entry has multiple TV PREQUELs (rare, but shows with both
///   a main prequel and a prequel side-story do exist), pick the one
///   with the largest episode count. Main shows always outlast
///   side-stories in episode count, so this picks the canonical chain
///   without extra metadata.
///
/// Cycle guard:
/// - Small cap on walk depth (20 hops) and a `visited` set. Relation
///   graphs have no legitimate reason to cycle, but a bad cache row
///   could in principle form a loop.
pub async fn compute_cumulative_prior_episodes(db: &SqlitePool, root_provider_id: i64) -> i32 {
    const MAX_DEPTH: usize = 20;

    if root_provider_id == 0 {
        return 0;
    }

    let mut offset: i32 = 0;
    let mut current = root_provider_id;
    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(current);

    for _ in 0..MAX_DEPTH {
        let row = sqlx::query(
            "SELECT related_provider_id, episodes \
             FROM provider_relations_cache \
             WHERE provider_id = ? AND relation_type = 'PREQUEL' AND format = 'TV' \
             ORDER BY COALESCE(episodes, 0) DESC \
             LIMIT 1",
        )
        .bind(current)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();

        let Some(row) = row else { break };
        let prev_id: i64 = row.get("related_provider_id");
        let prev_episodes: Option<i32> = row.get("episodes");

        if !visited.insert(prev_id) {
            break;
        }
        offset = offset.saturating_add(prev_episodes.unwrap_or(0).max(0));
        current = prev_id;
    }

    offset
}

async fn replace_episode_table(
    db: &SqlitePool,
    table: &str,
    key_col: &str,
    owner_id: i64,
    episodes: &[CachedEpisodeMetadata],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let delete_sql = format!("DELETE FROM {table} WHERE {key_col} = ?");
    sqlx::query(sqlx::AssertSqlSafe(delete_sql))
        .bind(owner_id)
        .execute(&mut *tx)
        .await?;
    let insert_sql = format!(
        "INSERT INTO {table} ({key_col}, episode_number, title, title_romaji, title_english, title_native, aired, source, cached_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)"
    );
    for ep in episodes {
        sqlx::query(sqlx::AssertSqlSafe(insert_sql.as_str()))
            .bind(owner_id)
            .bind(ep.episode_number)
            .bind(&ep.title)
            .bind(&ep.title_romaji)
            .bind(&ep.title_english)
            .bind(&ep.title_native)
            .bind(&ep.aired)
            .bind(&ep.source)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn get_episode_table(
    db: &SqlitePool,
    table: &str,
    key_col: &str,
    owner_id: i64,
) -> Result<HashMap<i32, CachedEpisodeMetadata>, sqlx::Error> {
    let sql = format!(
        "SELECT episode_number, title, title_romaji, title_english, title_native, aired, source FROM {table} WHERE {key_col} = ? ORDER BY episode_number ASC"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(owner_id)
        .fetch_all(db)
        .await?;
    let mut out = HashMap::new();
    for row in rows {
        let ep = CachedEpisodeMetadata {
            episode_number: row.get("episode_number"),
            title: row.get("title"),
            title_romaji: row.get("title_romaji"),
            title_english: row.get("title_english"),
            title_native: row.get("title_native"),
            aired: row.get("aired"),
            source: row.get("source"),
        };
        out.insert(ep.episode_number, ep);
    }
    Ok(out)
}

pub async fn replace_episode_metadata(
    db: &SqlitePool,
    series_id: i64,
    episodes: &[CachedEpisodeMetadata],
) -> Result<(), sqlx::Error> {
    replace_episode_table(
        db,
        "series_episode_metadata",
        "series_id",
        series_id,
        episodes,
    )
    .await
}

pub async fn replace_episode_metadata_for_provider(
    db: &SqlitePool,
    provider_id: i64,
    episodes: &[CachedEpisodeMetadata],
) -> Result<(), sqlx::Error> {
    replace_episode_table(
        db,
        "provider_episode_metadata",
        "provider_id",
        provider_id,
        episodes,
    )
    .await
}

pub async fn get_episode_map_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<HashMap<i32, CachedEpisodeMetadata>, sqlx::Error> {
    get_episode_table(db, "series_episode_metadata", "series_id", series_id).await
}

pub async fn get_episode_map_for_provider(
    db: &SqlitePool,
    provider_id: i64,
) -> Result<HashMap<i32, CachedEpisodeMetadata>, sqlx::Error> {
    get_episode_table(db, "provider_episode_metadata", "provider_id", provider_id).await
}

/// Per-series count of episodes that have aired, judged by the cached
/// air-date strings: a parseable leading `YYYY-MM-DD` that's today or
/// earlier. Powers the library cards' completeness bars ("do I have
/// what's aired?"), so the denominator must exclude unaired episodes
/// or a releasing series would read as perpetually incomplete.
/// Notes on the WHERE shape:
/// - `episode_number >= 1` skips the negative-cache sentinel row
///   (episode 0) the Jikan/Kitsu fallback writes.
/// - `length(aired) >= 10` skips the unknown markers ('' and '-').
/// - The substr comparison is lexicographic, which is chronological
///   for ISO dates; `date('now')` renders as `YYYY-MM-DD` in UTC —
///   same date-only, UTC semantics as the series page's
///   unaired-vs-missing split.
pub async fn aired_episode_counts(db: &SqlitePool) -> Result<HashMap<i64, i64>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, i64)>(
        "SELECT series_id, COUNT(*) FROM series_episode_metadata
         WHERE episode_number >= 1
           AND length(aired) >= 10
           AND substr(aired, 1, 10) <= date('now')
         GROUP BY series_id",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `aired_episode_counts` for one series, for the Wanted page's
/// per-series search menu, which must not scan the library per open.
pub async fn aired_episode_count(db: &SqlitePool, series_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM series_episode_metadata
         WHERE series_id = ?
           AND episode_number >= 1
           AND length(aired) >= 10
           AND substr(aired, 1, 10) <= date('now')",
    )
    .bind(series_id)
    .fetch_one(db)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh in-memory pool with the full migration applied. We need
    /// the whole migration because `provider_relations_cache` depends
    /// on earlier CREATE TABLE statements running first.
    async fn test_pool() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");
        db
    }

    async fn insert_prequel(
        db: &SqlitePool,
        provider_id: i64,
        related_id: i64,
        episodes: Option<i32>,
        format: &str,
    ) {
        sqlx::query(
            "INSERT INTO provider_relations_cache \
             (provider_id, related_provider_id, episodes, format, relation_type, media_type) \
             VALUES (?, ?, ?, ?, 'PREQUEL', 'ANIME')",
        )
        .bind(provider_id)
        .bind(related_id)
        .bind(episodes)
        .bind(format)
        .execute(db)
        .await
        .expect("insert prequel");
    }

    #[tokio::test]
    async fn cumulative_prior_is_zero_for_first_season_entry() {
        // No PREQUEL rows at all — offset must be 0 so the legacy
        // strict-relative filter behavior is preserved for single-cour
        // shows and for new adds before the relation cache is populated.
        let db = test_pool().await;
        assert_eq!(compute_cumulative_prior_episodes(&db, 1).await, 0);
    }

    #[tokio::test]
    async fn cumulative_prior_sums_tv_chain() {
        // JJK S3 (id=3) ← S2 (id=2, 23 ep) ← S1 (id=1, 24 ep).
        // Expected offset = 24 + 23 = 47.
        let db = test_pool().await;
        insert_prequel(&db, 3, 2, Some(23), "TV").await;
        insert_prequel(&db, 2, 1, Some(24), "TV").await;
        assert_eq!(compute_cumulative_prior_episodes(&db, 3).await, 47);
    }

    #[tokio::test]
    async fn cumulative_prior_skips_movie_prequels() {
        // JJK 0 (MOVIE) is a prequel to JJK S1 but SubsPlease absolute
        // numbering does NOT include it — S1 E1 = absolute 1, not 2.
        // The walker must filter by format = 'TV' to match that
        // convention.
        let db = test_pool().await;
        // S3 ← S2 (TV, 23) ← S1 (TV, 24) ← JJK 0 (MOVIE, 1)
        insert_prequel(&db, 3, 2, Some(23), "TV").await;
        insert_prequel(&db, 2, 1, Some(24), "TV").await;
        // Movie prequel present in the cache but different format.
        insert_prequel(&db, 1, 0, Some(1), "MOVIE").await;
        assert_eq!(compute_cumulative_prior_episodes(&db, 3).await, 47);
    }

    #[tokio::test]
    async fn cumulative_prior_handles_null_episodes() {
        // An entry with NULL episodes (still-airing prequel, or a
        // relation row written before AL populated the count) must not
        // crash the walker or produce a negative offset — it just
        // contributes zero.
        let db = test_pool().await;
        insert_prequel(&db, 2, 1, None, "TV").await;
        assert_eq!(compute_cumulative_prior_episodes(&db, 2).await, 0);
    }

    #[tokio::test]
    async fn cumulative_prior_picks_larger_tv_prequel_on_branch() {
        // Two TV PREQUELs from the same node (uncommon but possible
        // when both a main show and a prequel side-story point
        // backwards). The canonical main chain is always the entry
        // with more episodes, so the walker picks the higher count.
        let db = test_pool().await;
        insert_prequel(&db, 10, 5, Some(12), "TV").await; // side-story
        insert_prequel(&db, 10, 6, Some(24), "TV").await; // main
        assert_eq!(compute_cumulative_prior_episodes(&db, 10).await, 24);
    }

    async fn insert_provider_titles(
        db: &SqlitePool,
        provider_id: i64,
        romaji: &str,
        english: &str,
        native: &str,
    ) {
        let json = serde_json::json!({
            "title_romaji": romaji,
            "title_english": english,
            "title_native": native,
        });
        sqlx::query(
            "INSERT INTO provider_metadata_cache (provider_id, mal_id, detail_json) \
             VALUES (?, NULL, ?)",
        )
        .bind(provider_id)
        .bind(json.to_string())
        .execute(db)
        .await
        .expect("insert provider detail");
    }

    #[tokio::test]
    async fn franchise_aliases_returns_root_titles_via_prequel_chain() {
        // JJK S3 (id=3) ← S2 (id=2) ← S1 (id=1, the root).
        // Franchise aliases for S3 must include S1's titles so the
        // absolute-number query path can actually hit SubsPlease uploads
        // titled just "Jujutsu Kaisen".
        let db = test_pool().await;
        insert_prequel(&db, 3, 2, Some(23), "TV").await;
        insert_prequel(&db, 2, 1, Some(24), "TV").await;
        insert_provider_titles(
            &db,
            2,
            "Jujutsu Kaisen 2nd Season",
            "JUJUTSU KAISEN Season 2",
            "呪術廻戦 2期",
        )
        .await;
        insert_provider_titles(&db, 1, "Jujutsu Kaisen", "JUJUTSU KAISEN", "呪術廻戦").await;

        let aliases = resolve_franchise_aliases(&db, 3).await;
        // The root's romaji "Jujutsu Kaisen" must surface somewhere —
        // the english variant "JUJUTSU KAISEN" has the same lowercase
        // key so the case-insensitive dedupe can legitimately fold one
        // into the other, but the franchise-base alias must appear.
        assert!(
            aliases
                .iter()
                .any(|a| a.eq_ignore_ascii_case("Jujutsu Kaisen")),
            "franchise-root base title must appear, got {aliases:?}"
        );
        // Native title has a distinct lowercase key, so it must appear
        // as its own entry.
        assert!(
            aliases.iter().any(|a| a == "呪術廻戦"),
            "root native title must appear, got {aliases:?}"
        );
        // Intermediate S2 titles should also surface since SubsPlease /
        // similar might use S2-era aliases on pack titles during the
        // cross-cour transition.
        assert!(
            aliases
                .iter()
                .any(|a| a.contains("2nd Season") || a.contains("Season 2")),
            "intermediate S2 title should appear, got {aliases:?}"
        );
    }

    #[tokio::test]
    async fn franchise_aliases_empty_for_first_season_entry() {
        // No PREQUEL rows — nothing to surface.
        let db = test_pool().await;
        assert!(resolve_franchise_aliases(&db, 1).await.is_empty());
    }

    #[tokio::test]
    async fn franchise_aliases_dedupes_case_insensitive() {
        // Same title appearing in multiple ancestor language fields
        // (or at multiple chain depths) must collapse to one entry.
        let db = test_pool().await;
        insert_prequel(&db, 2, 1, Some(12), "TV").await;
        insert_provider_titles(&db, 1, "Same Title", "SAME TITLE", "").await;
        let aliases = resolve_franchise_aliases(&db, 2).await;
        assert_eq!(
            aliases.len(),
            1,
            "expected case-insensitive dedupe, got {aliases:?}"
        );
    }

    #[tokio::test]
    async fn cumulative_prior_terminates_on_cycle() {
        // Defensive: if a bad cache row points A → B → A, the walker
        // must stop rather than loop to MAX_DEPTH.
        let db = test_pool().await;
        insert_prequel(&db, 1, 2, Some(12), "TV").await;
        insert_prequel(&db, 2, 1, Some(24), "TV").await;
        // Start at 1: visit 2 (add 12), then 2's PREQUEL is 1 which is
        // already visited → stop. Offset should be 12, not unbounded.
        assert_eq!(compute_cumulative_prior_episodes(&db, 1).await, 12);
    }
}
