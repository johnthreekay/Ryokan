use sqlx::{Row, SqlitePool};

use crate::services::anilist::AnimeDetail;

pub const METADATA_REFRESH_INTERVAL_HOURS: i64 = 12;

/// How long a row may go without a successful refresh before the series
/// page warns that its metadata may be out of date. Deliberately much
/// wider than `METADATA_REFRESH_INTERVAL_HOURS`: a row is past the
/// refresh window between any two sweeps, so a banner keyed on
/// `is_fresh` showed on nearly every page view. A week with no
/// successful refresh is the shape of a provider outage (or Ryokan
/// being off), which is what the banner is for.
pub const METADATA_STALE_WARNING_DAYS: i64 = 7;

#[derive(Debug, Clone)]
pub struct CachedSeriesMetadata {
    pub provider_id: i64,
    pub detail: AnimeDetail,
    pub cached_at: String,
    /// Within `METADATA_REFRESH_INTERVAL_HOURS`; the readers that decide
    /// whether to fetch again key on this.
    pub is_fresh: bool,
    /// Older than `METADATA_STALE_WARNING_DAYS`; the series page's
    /// warning banner keys on this. `!is_fresh` is not "stale".
    pub is_stale: bool,
}

pub async fn get_by_series_id(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Option<CachedSeriesMetadata>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT provider_id, detail_json, cached_at,
               CASE
                   WHEN cached_at >= datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_fresh,
               CASE
                   WHEN cached_at < datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_stale
        FROM series_metadata_cache
        WHERE series_id = ?
        "#,
    )
    .bind(format!("-{} hours", METADATA_REFRESH_INTERVAL_HOURS))
    .bind(format!("-{} days", METADATA_STALE_WARNING_DAYS))
    .bind(series_id)
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let detail_json: String = row.get("detail_json");
    let detail: AnimeDetail =
        serde_json::from_str(&detail_json).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    Ok(Some(CachedSeriesMetadata {
        provider_id: row.get("provider_id"),
        detail,
        cached_at: row.get("cached_at"),
        is_fresh: row.get::<i64, _>("is_fresh") != 0,
        is_stale: row.get::<i64, _>("is_stale") != 0,
    }))
}

pub async fn upsert(
    db: &SqlitePool,
    series_id: i64,
    provider_id: i64,
    mal_id: Option<i64>,
    detail: &AnimeDetail,
) -> Result<(), sqlx::Error> {
    let detail_json =
        serde_json::to_string(detail).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query(
        r#"
        INSERT INTO series_metadata_cache (series_id, provider_id, mal_id, detail_json, cached_at)
        VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(series_id) DO UPDATE SET
            provider_id = excluded.provider_id,
            mal_id = excluded.mal_id,
            detail_json = excluded.detail_json,
            cached_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(series_id)
    .bind(provider_id)
    .bind(mal_id)
    .bind(detail_json)
    .execute(db)
    .await?;

    Ok(())
}

pub async fn get_by_provider_id(
    db: &SqlitePool,
    provider_id: i64,
) -> Result<Option<CachedSeriesMetadata>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT provider_id, detail_json, cached_at,
               CASE
                   WHEN cached_at >= datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_fresh,
               CASE
                   WHEN cached_at < datetime('now', ?) THEN 1
                   ELSE 0
               END AS is_stale
        FROM provider_metadata_cache
        WHERE provider_id = ?
        "#,
    )
    .bind(format!("-{} hours", METADATA_REFRESH_INTERVAL_HOURS))
    .bind(format!("-{} days", METADATA_STALE_WARNING_DAYS))
    .bind(provider_id)
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let detail_json: String = row.get("detail_json");
    let detail: AnimeDetail =
        serde_json::from_str(&detail_json).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    Ok(Some(CachedSeriesMetadata {
        provider_id: row.get("provider_id"),
        detail,
        cached_at: row.get("cached_at"),
        is_fresh: row.get::<i64, _>("is_fresh") != 0,
        is_stale: row.get::<i64, _>("is_stale") != 0,
    }))
}

pub async fn upsert_provider(
    db: &SqlitePool,
    provider_id: i64,
    mal_id: Option<i64>,
    detail: &AnimeDetail,
) -> Result<(), sqlx::Error> {
    let detail_json =
        serde_json::to_string(detail).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query(
        r#"
        INSERT INTO provider_metadata_cache (provider_id, mal_id, detail_json, cached_at)
        VALUES (?, ?, ?, CURRENT_TIMESTAMP)
        ON CONFLICT(provider_id) DO UPDATE SET
            mal_id = excluded.mal_id,
            detail_json = excluded.detail_json,
            cached_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(provider_id)
    .bind(mal_id)
    .bind(detail_json)
    .execute(db)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{in_memory_pool, seed_series};

    fn detail(id: i64) -> AnimeDetail {
        AnimeDetail {
            is_adult: false,
            id,
            id_mal: None,
            title_romaji: "Cache Test".to_string(),
            title_english: "Cache Test".to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes: Some(12),
            duration: Some(24),
            season: String::new(),
            season_year: Some(2015),
            end_year: Some(2015),
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    async fn backdate(db: &SqlitePool, table: &str, key_col: &str, key: i64, modifier: &str) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET cached_at = datetime('now', ?) WHERE {key_col} = ?"
        )))
        .bind(modifier)
        .bind(key)
        .execute(db)
        .await
        .expect("backdate cached_at");
    }

    // The two flags answer different questions: `is_fresh` is the
    // 12-hour refresh window, `is_stale` the week-long warning window.
    // A row between the two (past the refresh window, well short of a
    // week) is the normal state between sweeps and must warn nobody.
    #[tokio::test]
    async fn provider_row_is_stale_only_after_the_warning_window() {
        let db = in_memory_pool().await;
        upsert_provider(&db, 1535, None, &detail(1535))
            .await
            .expect("upsert");

        let row = get_by_provider_id(&db, 1535).await.unwrap().unwrap();
        assert!(row.is_fresh, "just written: fresh");
        assert!(!row.is_stale, "just written: not stale");

        backdate(
            &db,
            "provider_metadata_cache",
            "provider_id",
            1535,
            "-13 hours",
        )
        .await;
        let row = get_by_provider_id(&db, 1535).await.unwrap().unwrap();
        assert!(!row.is_fresh, "13 hours: past the refresh window");
        assert!(!row.is_stale, "13 hours: nowhere near the warning window");

        backdate(
            &db,
            "provider_metadata_cache",
            "provider_id",
            1535,
            "-6 days",
        )
        .await;
        let row = get_by_provider_id(&db, 1535).await.unwrap().unwrap();
        assert!(!row.is_stale, "6 days: still short of the warning window");

        backdate(
            &db,
            "provider_metadata_cache",
            "provider_id",
            1535,
            "-8 days",
        )
        .await;
        let row = get_by_provider_id(&db, 1535).await.unwrap().unwrap();
        assert!(!row.is_fresh, "8 days: past the refresh window");
        assert!(row.is_stale, "8 days: past the warning window");
    }

    #[tokio::test]
    async fn series_row_carries_the_same_flags() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1535, "Cache Test").await;
        upsert(&db, series_id, 1535, None, &detail(1535))
            .await
            .expect("upsert");

        let row = get_by_series_id(&db, series_id).await.unwrap().unwrap();
        assert!(row.is_fresh);
        assert!(!row.is_stale);

        backdate(
            &db,
            "series_metadata_cache",
            "series_id",
            series_id,
            "-8 days",
        )
        .await;
        let row = get_by_series_id(&db, series_id).await.unwrap().unwrap();
        assert!(!row.is_fresh);
        assert!(row.is_stale);
    }
}
