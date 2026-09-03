use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::models::monitoring::MonitorMode;

#[derive(Debug, Clone, Serialize)]
pub struct Series {
    pub id: i64,
    pub anilist_id: i64,
    pub mal_id: Option<i64>,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    /// Year the series *finished* airing, when AniList has an explicit
    /// end date. Used by Layer 4 (temporal inference) to distinguish
    /// "finished recently" from "started years ago and is still going."
    /// `None` for currently-airing shows and for metadata providers
    /// that don't supply an end date.
    pub end_year: Option<i32>,
    /// AniList `isAdult` (issue #219). Stamped by the metadata refresh
    /// (`set_is_adult`), so a freshly added row reads `false` until its
    /// first refresh lands. Nyaa lists adult releases on sukebei, which
    /// Ryokan does not search; the flag exists so the UI and the
    /// auto-search log can say why nothing turns up.
    pub is_adult: bool,
    pub folder_name: String,
    pub monitor_mode: String,
    /// Phase 4 per-series upgrade toggle. When false the upgrade scanner
    /// skips this series entirely, even if a higher-quality release is
    /// available. Defaults to true to preserve historical behavior.
    pub allow_upgrades: bool,
    /// Issue #28 — per-series PT upgrade opt-in. When false (the
    /// default), the upgrade sweep won't grab a private-tracker release
    /// for this series even if it's the top-scoring candidate. The
    /// initial-grab and manual-search paths aren't affected (those are
    /// user-driven). The flag exists so a user can have torznab PT
    /// indexers configured (for manual searches and initial grabs) but
    /// not have the background upgrade sweep silently re-grab existing
    /// episodes from a PT and rack up Hit-and-Runs / ratio liability
    /// without their knowledge.
    pub allow_pt_upgrades: bool,
    /// #23 — Per-series custom Nyaa query tokens appended after the
    /// title aliases. Overrides the global
    /// `config.default_custom_query_tokens` when non-empty.
    pub custom_query_tokens: String,
    /// #23 — Per-series Nyaa uploader restriction. When non-empty,
    /// Ryokan sets `?u=<name>` on every search for this series so only
    /// that account's uploads come back. Overrides the global
    /// `config.default_restrict_to_uploader`.
    pub restrict_to_uploader: String,
    /// #30 — Cumulative episode count across the shortest TV-format
    /// PREQUEL chain up to this series. Zero for first-season series
    /// and for cases where no prequel data is cached. Search accepts a
    /// Nyaa release if its parsed episode number matches either the
    /// relative target (AL's own numbering) OR `target + offset` (the
    /// absolute number a SubsPlease-style release would use).
    pub cumulative_prior_episodes: i32,
    /// #62 — `1` when the user has manually pinned this series's
    /// `monitor_mode` through the per-series UI. The watch-list sync
    /// skips both the merge-step monitor_mode update and the removal-
    /// detection downgrade for these rows, so a pinned mode stays
    /// pinned across syncs. Cleared when the user picks "Sync from
    /// AL/MAL" from the dropdown.
    pub monitor_mode_manual_override: bool,
    /// #62 — user's personal score on their linked AL/MAL
    /// account. `None` for manually-added series (no linked account
    /// fed a score in) and for sync-imported series the user hasn't
    /// rated. The render helper in `services::user_score` formats
    /// this per the account's `score_format` and never shows
    /// `You: 0` for 0.0 (AL's "unrated" sentinel).
    pub user_score: Option<f64>,
    /// ISO-8601 string the SQLite `DEFAULT CURRENT_TIMESTAMP` writes
    /// at insert time (e.g. `"2026-04-25 12:34:56"`). Surfaced so the
    /// library page's "oldest first" sort can `sort_by_key` against
    /// this column directly rather than relying on a `library.reverse()`
    /// of the SQL default `ORDER BY added_at DESC` — that pattern broke
    /// silently any time a future caller's needs reshaped the query's
    /// default ordering. ISO-8601 sorts lexicographically, so a plain
    /// `String` comparison gives chronological order without a parse.
    pub added_at: String,
}

impl Series {
    pub fn monitor_mode_enum(&self) -> MonitorMode {
        MonitorMode::from_str(&self.monitor_mode)
    }

    /// #62 — render the "You: X" badge for this series using
    /// the given `score_format` (typically the linked
    /// `external_accounts.score_format`). Returns `None` when no
    /// account is linked, the user hasn't rated this series, or the
    /// score is the unrated sentinel. The returned
    /// [`FormattedUserScore`] is either plain text (numbers, stars)
    /// or a smiley enum the template renders as inline SVG. Library
    /// cards call this from Askama directly so each row formats
    /// consistently with the detail-page badge.
    pub fn user_score_display(
        &self,
        score_format: &str,
    ) -> Option<crate::services::user_score::FormattedUserScore> {
        crate::services::user_score::format_user_score(self.user_score, score_format)
    }
}

fn map_series_row(row: sqlx::sqlite::SqliteRow) -> Series {
    Series {
        is_adult: row
            .try_get::<i64, _>("is_adult")
            .map(|v| v != 0)
            .unwrap_or(false),
        id: row.get("id"),
        anilist_id: row.get("anilist_id"),
        mal_id: row.try_get("mal_id").ok(),
        title: row.get("title"),
        title_romaji: row.get("title_romaji"),
        title_english: row.get("title_english"),
        title_native: row.get("title_native"),
        cover_url: row.get("cover_url"),
        format: row.get("format"),
        status: row.get("status"),
        episodes: row.get("episodes"),
        season_year: row.try_get("season_year").ok().flatten(),
        end_year: row.try_get("end_year").ok().flatten(),
        folder_name: row.get("folder_name"),
        monitor_mode: row
            .try_get("monitor_mode")
            .unwrap_or_else(|_| "future".to_string()),
        // Default to true so series from before the column existed (migration
        // backfills via ADD COLUMN DEFAULT 1) opt *in* to upgrades.
        allow_upgrades: row
            .try_get::<i64, _>("allow_upgrades")
            .map(|v| v != 0)
            .unwrap_or(true),
        // Default to false (opt-in). Pre-#28 grabs were all Nyaa-direct,
        // which is non-PT, so this flag changing default-off doesn't
        // affect any existing behavior — it only gates new PT-sourced
        // upgrades against existing libraries.
        allow_pt_upgrades: row
            .try_get::<i64, _>("allow_pt_upgrades")
            .map(|v| v != 0)
            .unwrap_or(false),
        custom_query_tokens: row.try_get("custom_query_tokens").unwrap_or_default(),
        restrict_to_uploader: row.try_get("restrict_to_uploader").unwrap_or_default(),
        cumulative_prior_episodes: row
            .try_get::<i32, _>("cumulative_prior_episodes")
            .unwrap_or(0),
        monitor_mode_manual_override: row
            .try_get::<i64, _>("monitor_mode_manual_override")
            .map(|v| v != 0)
            .unwrap_or(false),
        user_score: row.try_get::<Option<f64>, _>("user_score").unwrap_or(None),
        added_at: row.try_get("added_at").unwrap_or_default(),
    }
}

/// Get all tracked series, ordered by most recently added.
pub async fn get_all(db: &SqlitePool) -> Result<Vec<Series>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades, allow_pt_upgrades, custom_query_tokens, restrict_to_uploader, cumulative_prior_episodes, monitor_mode_manual_override, user_score, is_adult, added_at FROM series ORDER BY added_at DESC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(map_series_row).collect())
}

pub async fn get_by_id(db: &SqlitePool, id: i64) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades, allow_pt_upgrades, custom_query_tokens, restrict_to_uploader, cumulative_prior_episodes, monitor_mode_manual_override, user_score, is_adult, added_at FROM series WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

pub async fn get_by_anilist_id(
    db: &SqlitePool,
    anilist_id: i64,
) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades, allow_pt_upgrades, custom_query_tokens, restrict_to_uploader, cumulative_prior_episodes, monitor_mode_manual_override, user_score, is_adult, added_at FROM series WHERE anilist_id = ?",
    )
    .bind(anilist_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

/// Batch lookup keyed by AniList id. Returns a map of `anilist_id ->
/// Series` for the rows that exist; ids with no row are absent. Used
/// by the Sonarr/Radarr search-result fan-outs so a 10-result AL page
/// turns into one DB round-trip instead of N. Empty input returns an
/// empty map without hitting the DB.
pub async fn get_by_anilist_ids(
    db: &SqlitePool,
    anilist_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Series>, sqlx::Error> {
    if anilist_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = vec!["?"; anilist_ids.len()].join(",");
    let sql = format!(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades, allow_pt_upgrades, custom_query_tokens, restrict_to_uploader, cumulative_prior_episodes, monitor_mode_manual_override, user_score, is_adult, added_at FROM series WHERE anilist_id IN ({placeholders})"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in anilist_ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(map_series_row)
        .map(|s| (s.anilist_id, s))
        .collect())
}

pub async fn get_by_mal_id(db: &SqlitePool, mal_id: i64) -> Result<Option<Series>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades, allow_pt_upgrades, custom_query_tokens, restrict_to_uploader, cumulative_prior_episodes, monitor_mode_manual_override, user_score, is_adult, added_at FROM series WHERE mal_id = ?",
    )
    .bind(mal_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(map_series_row))
}

/// Core metadata bundle shared by `upsert` and `refresh_core_metadata`.
///
/// Collapsing the 11 scalar args into a named struct closes a real
/// correctness hole: four of the fields (`title`, `title_romaji`,
/// `title_english`, `title_native`) are all `&str` and sit next to
/// each other in the call order. Positional callers could swap any
/// two of them and neither the compiler nor the SQL would object —
/// the wrong string would just silently end up in the wrong column.
/// Named fields force callsites to be explicit.
pub struct SeriesCore<'a> {
    pub anilist_id: i64,
    pub mal_id: Option<i64>,
    pub title: &'a str,
    pub title_romaji: &'a str,
    pub title_english: &'a str,
    pub title_native: &'a str,
    pub cover_url: &'a str,
    pub format: &'a str,
    pub status: &'a str,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    /// Year the series finished airing, or `None` for currently-airing or
    /// unknown. Populated from AniList's `endDate.year` on the metadata
    /// fetch path; callers that build SeriesCore from providers without
    /// an end-date concept pass `None`. `upsert`/`refresh_core_metadata`
    /// use `COALESCE(?, end_year)` so a later fetch that *does* carry
    /// the year can fill in the gap without clobbering a previously-set
    /// value.
    pub end_year: Option<i32>,
}

/// Insert or update a series based on AniList/MAL provider identity.
pub async fn upsert(db: &SqlitePool, core: SeriesCore<'_>) -> Result<(i64, bool), sqlx::Error> {
    if let Some(mid) = core.mal_id
        && let Some(existing) = get_by_mal_id(db, mid).await?
    {
        sqlx::query(
            r#"
                UPDATE series
                SET anilist_id = ?,
                    mal_id = ?,
                    title = ?,
                    title_romaji = ?,
                    title_english = ?,
                    title_native = ?,
                    cover_url = ?,
                    format = ?,
                    status = ?,
                    episodes = ?,
                    season_year = COALESCE(?, season_year),
                    end_year = COALESCE(?, end_year),
                    monitor_mode = COALESCE(NULLIF(monitor_mode, ''), ?)
                WHERE id = ?
                "#,
        )
        .bind(core.anilist_id)
        .bind(mid)
        .bind(core.title)
        .bind(core.title_romaji)
        .bind(core.title_english)
        .bind(core.title_native)
        .bind(core.cover_url)
        .bind(core.format)
        .bind(core.status)
        .bind(core.episodes)
        .bind(core.season_year)
        .bind(core.end_year)
        .bind(default_monitor_mode(core.status).as_str())
        .bind(existing.id)
        .execute(db)
        .await?;
        return Ok((existing.id, false));
    }

    if let Some(existing) = get_by_anilist_id(db, core.anilist_id).await? {
        sqlx::query(
            r#"
            UPDATE series
            SET mal_id = COALESCE(?, mal_id),
                title = ?,
                title_romaji = ?,
                title_english = ?,
                title_native = ?,
                cover_url = ?,
                format = ?,
                status = ?,
                episodes = ?,
                season_year = COALESCE(?, season_year),
                end_year = COALESCE(?, end_year),
                monitor_mode = COALESCE(NULLIF(monitor_mode, ''), ?)
            WHERE id = ?
            "#,
        )
        .bind(core.mal_id)
        .bind(core.title)
        .bind(core.title_romaji)
        .bind(core.title_english)
        .bind(core.title_native)
        .bind(core.cover_url)
        .bind(core.format)
        .bind(core.status)
        .bind(core.episodes)
        .bind(core.season_year)
        .bind(core.end_year)
        .bind(default_monitor_mode(core.status).as_str())
        .bind(existing.id)
        .execute(db)
        .await?;
        return Ok((existing.id, false));
    }

    // Folder name from the series-folder template (#124), rendered in
    // the preferred title language. Applied once here and persisted;
    // later template or language changes never rename existing folders.
    let folder = {
        let prefs = crate::models::config::get_naming_prefs(db).await;
        let names = crate::services::naming::SeriesNames {
            title: core.title,
            romaji: core.title_romaji,
            english: core.title_english,
            native: core.title_native,
            year: core.season_year,
        };
        crate::services::naming::series_folder(
            &prefs.series_folder_format,
            &prefs.title_language,
            &names,
        )
    };

    let result = sqlx::query(
        r#"
        INSERT INTO series (anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(core.anilist_id)
    .bind(core.mal_id)
    .bind(core.title)
    .bind(core.title_romaji)
    .bind(core.title_english)
    .bind(core.title_native)
    .bind(core.cover_url)
    .bind(core.format)
    .bind(core.status)
    .bind(core.episodes)
    .bind(core.season_year)
    .bind(core.end_year)
    .bind(&folder)
    .bind(default_monitor_mode(core.status).as_str())
    .execute(db)
    .await?;

    Ok((result.last_insert_rowid(), true))
}

/// Remove a series by its database ID.
pub async fn remove(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM series_metadata_cache WHERE series_id = ?")
        .bind(id)
        .execute(db)
        .await
        .ok();

    sqlx::query("DELETE FROM series_relations_cache WHERE series_id = ?")
        .bind(id)
        .execute(db)
        .await
        .ok();
    sqlx::query("DELETE FROM series_episode_metadata WHERE series_id = ?")
        .bind(id)
        .execute(db)
        .await
        .ok();

    // Detach the rss_seen audit trail from this series before the
    // DELETE below, or the final series delete will fail with
    // "FOREIGN KEY constraint failed". Every other child table that
    // references series(id) has `ON DELETE CASCADE`, but rss_seen is
    // declared `NO ACTION` on purpose — the historical log of which
    // RSS items matched which series is useful to keep after a
    // removal, and `series_title` is stored alongside `series_id` on
    // each rss_seen row precisely so the audit trail survives the
    // FK being broken. sqlx enables `PRAGMA foreign_keys = ON` by
    // default, so even though nothing in this codebase asks for it,
    // the enforcement is live and a series with any RSS matches
    // cannot be deleted without first NULL-ing this reference.
    //
    // `.ok()` because this is a best-effort audit-trail cleanup and
    // a failure here shouldn't block the removal — if it really did
    // fail, the series DELETE below would surface the same error
    // via the FK violation and the caller would see it anyway.
    sqlx::query("UPDATE rss_seen SET series_id = NULL WHERE series_id = ?")
        .bind(id)
        .execute(db)
        .await
        .ok();

    sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Update the folder name mapping for a series.
pub async fn update_folder(db: &SqlitePool, id: i64, folder_name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET folder_name = ? WHERE id = ?")
        .bind(folder_name)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn refresh_core_metadata(
    db: &SqlitePool,
    id: i64,
    core: SeriesCore<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE series
        SET anilist_id = ?,
            mal_id = COALESCE(?, mal_id),
            title = ?,
            title_romaji = ?,
            title_english = ?,
            title_native = ?,
            cover_url = ?,
            format = ?,
            status = ?,
            episodes = ?,
            season_year = COALESCE(?, season_year),
            end_year = COALESCE(?, end_year)
        WHERE id = ?
        "#,
    )
    .bind(core.anilist_id)
    .bind(core.mal_id)
    .bind(core.title)
    .bind(core.title_romaji)
    .bind(core.title_english)
    .bind(core.title_native)
    .bind(core.cover_url)
    .bind(core.format)
    .bind(core.status)
    .bind(core.episodes)
    .bind(core.season_year)
    .bind(core.end_year)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get_unreconciled_fallbacks(db: &SqlitePool) -> Result<Vec<Series>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, anilist_id, mal_id, title, title_romaji, title_english, title_native, cover_url, format, status, episodes, season_year, end_year, folder_name, monitor_mode, allow_upgrades, allow_pt_upgrades, custom_query_tokens, restrict_to_uploader, cumulative_prior_episodes, monitor_mode_manual_override, user_score, is_adult, added_at FROM series WHERE mal_id IS NOT NULL AND anilist_id < 0 ORDER BY added_at DESC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(map_series_row).collect())
}

pub async fn update_monitor_mode(
    db: &SqlitePool,
    id: i64,
    monitor_mode: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
        .bind(monitor_mode)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Set or clear the manual-override flag for a series's monitor_mode.
/// Called by `set_monitoring` (sets to 1 when the user picks an
/// explicit mode; clears to 0 when the user picks "Sync from AL/MAL").
/// The watch-list sync's merge step + removal-detection pass both
/// skip series where this is 1.
pub async fn update_monitor_mode_manual_override(
    db: &SqlitePool,
    id: i64,
    flag: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET monitor_mode_manual_override = ? WHERE id = ?")
        .bind(if flag { 1_i64 } else { 0_i64 })
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// #62 — write the user's personal score from the linked
/// AL/MAL account. AL's POINT_10_DECIMAL format stores fractional
/// values, so the column is REAL. Stored as `0.0` for unrated
/// (matching AL's "0 means no score" convention); the render helper
/// in `services::user_score` treats 0.0 the same as NULL and never
/// shows `You: 0`.
pub async fn update_user_score(
    db: &SqlitePool,
    id: i64,
    score: Option<f64>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET user_score = ? WHERE id = ?")
        .bind(score)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Atomic write of `monitor_mode` + `monitor_mode_manual_override` in
/// a single SQLite UPDATE so a partial write can't leave the row in
/// the surprise state "new mode without the pin flag" — which would
/// silently let the next sync tick overwrite the user's choice.
/// Used by the explicit-mode branch of `set_monitoring`.
pub async fn update_monitor_mode_with_override(
    db: &SqlitePool,
    id: i64,
    monitor_mode: &str,
    flag: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE series SET monitor_mode = ?, monitor_mode_manual_override = ? WHERE id = ?",
    )
    .bind(monitor_mode)
    .bind(if flag { 1_i64 } else { 0_i64 })
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// #62 — stamp the external account that most-recently synced
/// this series. Called on every successful merge action (Created,
/// MonitorUpdated, Unchanged) so the marker stays current even if
/// the user manually adds a series that later appears on their AL
/// list. The marker is what enables removal detection on full-resync:
/// a sync-marked series whose AL id is NOT in the current fetch gets
/// downgraded to monitor_mode=None.
pub async fn stamp_synced_from(
    db: &SqlitePool,
    id: i64,
    account_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET synced_from_external_account_id = ? WHERE id = ?")
        .bind(account_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Lightweight projection used by the watch-list-sync removal pass.
/// Returns one row per `series` that was last synced from
/// `account_id`; the consumer compares each row's `anilist_id`
/// against the IDs in the current fetch to find missing entries.
#[derive(Debug, Clone)]
pub struct SyncedSeriesRow {
    pub id: i64,
    pub anilist_id: i64,
    pub monitor_mode: String,
    pub monitor_mode_manual_override: bool,
}

pub async fn list_synced_from(
    db: &SqlitePool,
    account_id: i64,
) -> Result<Vec<SyncedSeriesRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, anilist_id, monitor_mode, monitor_mode_manual_override FROM series \
         WHERE synced_from_external_account_id = ?",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SyncedSeriesRow {
            id: r.get("id"),
            anilist_id: r.get("anilist_id"),
            monitor_mode: r
                .try_get("monitor_mode")
                .unwrap_or_else(|_| "future".to_string()),
            monitor_mode_manual_override: r
                .try_get::<i64, _>("monitor_mode_manual_override")
                .map(|v| v != 0)
                .unwrap_or(false),
        })
        .collect())
}

/// Toggle the per-series upgrade opt-in. When false the upgrade scanner
/// in `services::upgrade` skips this series entirely.
pub async fn update_allow_upgrades(
    db: &SqlitePool,
    id: i64,
    allow: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET allow_upgrades = ? WHERE id = ?")
        .bind(if allow { 1_i64 } else { 0_i64 })
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Issue #28 — toggle the per-series PT upgrade opt-in. When
/// false (the default), the upgrade sweep won't accept a private-
/// tracker release as the chosen upgrade for this series. Initial
/// grabs and manual-search grabs aren't gated.
/// Stamp AniList's `isAdult` on the row. Called from the metadata
/// refresh, which is the only path that holds a full `AnimeDetail`.
pub async fn set_is_adult(db: &SqlitePool, id: i64, is_adult: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET is_adult = ? WHERE id = ?")
        .bind(is_adult as i64)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn update_allow_pt_upgrades(
    db: &SqlitePool,
    id: i64,
    allow: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET allow_pt_upgrades = ? WHERE id = ?")
        .bind(if allow { 1_i64 } else { 0_i64 })
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// #30 — Store the cumulative-prior-episodes offset computed from the
/// cached PREQUEL chain. Called by `metadata_sync::refresh_series_metadata`
/// after relations are cached, and by `handlers/library::add_series` so
/// the first interactive search works without waiting on the next
/// refresh sweep.
pub async fn update_cumulative_prior_episodes(
    db: &SqlitePool,
    id: i64,
    offset: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET cumulative_prior_episodes = ? WHERE id = ?")
        .bind(offset)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// #23 — Update the per-series search overrides. Empty strings clear
/// the override and make the series fall back to the global defaults
/// in `config.default_custom_query_tokens` / `default_restrict_to_uploader`.
pub async fn update_search_overrides(
    db: &SqlitePool,
    id: i64,
    custom_query_tokens: &str,
    restrict_to_uploader: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE series SET custom_query_tokens = ?, restrict_to_uploader = ? WHERE id = ?")
        .bind(custom_query_tokens.trim())
        .bind(restrict_to_uploader.trim())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub fn default_monitor_mode(status: &str) -> MonitorMode {
    let upper = status.trim().to_ascii_uppercase();
    match upper.as_str() {
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED" => MonitorMode::Missing,
        _ => MonitorMode::Future,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the "Remove from Library → generic error, no
    /// log" bug. `rss_seen` is the only table referencing `series(id)`
    /// without `ON DELETE CASCADE`; once RSS sync has matched any item
    /// to a series, the parent DELETE fails the FK constraint (sqlx
    /// enables `PRAGMA foreign_keys = ON` by default) and the remove
    /// handler returns 500 with "FOREIGN KEY constraint failed" — a
    /// message the frontend swallows, showing a generic "Error" on the
    /// button.
    ///
    /// The fix NULL-es out `rss_seen.series_id` inside `series::remove`
    /// before the parent DELETE, detaching the audit trail so the
    /// constraint is satisfied while preserving the rss_seen rows
    /// themselves (they keep `series_title` so the historical log
    /// remains useful).
    #[tokio::test]
    async fn remove_succeeds_when_rss_seen_rows_reference_series() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("full migrate");

        // Seed a minimal series row. Only `anilist_id` and `title` are
        // NOT NULL; everything else takes its column default.
        let series_id: i64 =
            sqlx::query_scalar("INSERT INTO series (anilist_id, title) VALUES (?, ?) RETURNING id")
                .bind(188388_i64)
                .bind("DIGIMON BEATBREAK")
                .fetch_one(&db)
                .await
                .expect("insert series");

        // Seed a couple of rss_seen rows that reference it — one
        // "grabbed", one "skipped" — so the audit trail has meaningful
        // content to survive the removal. The `item_key` UNIQUE index
        // forces the keys to differ.
        for (i, decision) in ["grabbed", "skipped"].iter().enumerate() {
            sqlx::query(
                "INSERT INTO rss_seen (item_key, title, link, series_id, series_title, group_name, is_batch, decision, reason)
                 VALUES (?, ?, ?, ?, ?, ?, 0, ?, '')",
            )
            .bind(format!("beatbreak-item-{}", i))
            .bind("[Judas] Digimon Beatbreak - S01E26")
            .bind("https://nyaa.si/view/stub")
            .bind(series_id)
            .bind("DIGIMON BEATBREAK")
            .bind("Judas")
            .bind(*decision)
            .execute(&db)
            .await
            .expect("insert rss_seen");
        }

        // Before the fix: this fails with "FOREIGN KEY constraint failed"
        // because the rss_seen rows still point at series_id with no
        // CASCADE to clear them. After the fix: the UPDATE inside
        // series::remove NULL-es those rows first, then the parent
        // DELETE succeeds cleanly.
        remove(&db, series_id)
            .await
            .expect("series::remove should succeed");

        // The series row is gone.
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .expect("count series");
        assert_eq!(remaining, 0, "series row should be deleted");

        // The rss_seen rows SURVIVE — the audit trail is preserved on
        // purpose — but their series_id is now NULL, and series_title
        // still carries the human-readable label for after-the-fact
        // inspection.
        let rss_rows: Vec<(Option<i64>, String, String)> =
            sqlx::query_as("SELECT series_id, series_title, decision FROM rss_seen ORDER BY id")
                .fetch_all(&db)
                .await
                .expect("fetch rss_seen");
        assert_eq!(rss_rows.len(), 2, "rss_seen rows should survive removal");
        for (id, title, _decision) in &rss_rows {
            assert!(id.is_none(), "series_id should be NULL after removal");
            assert_eq!(title, "DIGIMON BEATBREAK", "series_title preserved");
        }
    }

    #[tokio::test]
    async fn set_is_adult_round_trips_through_the_row() {
        // Issue #219 — the column defaults to 0 for existing rows and
        // is stamped only by the metadata refresh.
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("full migrate");
        let id: i64 =
            sqlx::query_scalar("INSERT INTO series (anilist_id, title) VALUES (?, ?) RETURNING id")
                .bind(21521_i64)
                .bind("Kowaremono: Risa THE ANIMATION")
                .fetch_one(&db)
                .await
                .expect("insert series");

        let row = get_by_id(&db, id).await.expect("query").expect("row");
        assert!(!row.is_adult, "fresh rows read as not adult");

        set_is_adult(&db, id, true).await.expect("stamp");
        let row = get_by_id(&db, id).await.expect("query").expect("row");
        assert!(row.is_adult);

        set_is_adult(&db, id, false).await.expect("clear");
        let row = get_by_id(&db, id).await.expect("query").expect("row");
        assert!(!row.is_adult);
    }
}
