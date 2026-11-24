//! Refresh stamp upcoming episode air dates into the local
//! `episode_airings` table.
//!
//! Two entry points:
//! - [`refresh_all`] walks every positive-AL-id series in the
//!   library, batches the AL `Page.airingSchedules` query, and
//!   upserts the result. Called every 12h by the
//!   `airing_refresh` supervised task in `main.rs`.
//! - [`refresh_for_series`] does the same for a single series.
//!   Called inline from the library add path so a freshly-added
//!   series shows up in the calendar without waiting for the
//!   next 12h tick.
//!
//! ## Why we don't fetch on demand anymore
//!
//! The previous calendar implementation hit
//! `airing_schedules::fetch_airing_schedules` per-request and held
//! a 15-min in-process cache. AL's degraded budget on this query
//! is 30/min, so a fleet of iCal subscribers polling every 15 min
//! could drain the budget regardless of cache. Sonarr's
//! `Episode.AirDateUtc` shape — stamp at refresh time, serve from
//! DB on the hot path — is strictly better at zero per-request AL
//! cost. This module is the stamping side; `services::calendar` is
//! the read side.

use std::sync::LazyLock;
use std::time::Duration;

use sqlx::Row;
use sqlx::SqlitePool;
use tokio::sync::Mutex as TokioMutex;

use crate::models::episode_airings::{self, EpisodeAiring};
use crate::services::anilist::airing_schedules;

/// Default forward window the refresh stamps. 90 days matches the
/// iCal feed's `?days` cap. Anything past 90 days is best handled
/// by the next 12h refresh tick anyway — AL's air dates that far
/// out are typically TBA / placeholder and shift before they fire.
const FORWARD_WINDOW_DAYS: i64 = 90;

/// Past-window. Calendar's `this_week` view only needs +0 days,
/// but allowing a small back-window means past episodes that
/// shifted to "today" still render correctly when their original
/// stamp was a few hours behind.
const BACKWARD_WINDOW_DAYS: i64 = 1;

/// How long to retain past airings in the DB before pruning. Two
/// weeks is enough for the calendar's "this week" / "next week"
/// views to never see a hole, plus a buffer for anyone hitting a
/// stale URL.
const RETAIN_PAST_DAYS: i64 = 14;

/// Process-wide lock so the supervised loop and a manual refresh
/// trigger don't run side-by-side and double-stamp / 429 the AL
/// budget. `try_lock` shape matches `RSS_SYNC_LOCK` /
/// `EXTERNAL_SYNC_LOCK` — a manual run while the scheduled run is
/// in flight returns "already running" rather than queuing.
pub static AIRING_REFRESH_LOCK: LazyLock<TokioMutex<()>> = LazyLock::new(|| TokioMutex::new(()));

/// Aggregate result of a refresh sweep — surfaced in the System →
/// Tasks page via `scheduled_tasks::mark_finished` detail string.
#[derive(Debug, Default, Clone)]
pub struct RefreshSummary {
    pub series_scanned: usize,
    pub airings_upserted: usize,
    pub airings_pruned: u64,
    pub al_failures: usize,
}

impl RefreshSummary {
    pub fn detail(&self) -> String {
        format!(
            "series={}, upserted={}, pruned={}, failures={}",
            self.series_scanned, self.airings_upserted, self.airings_pruned, self.al_failures,
        )
    }
}

/// Refresh airings for every positive-AL-id series in the library.
/// Returns a summary regardless of partial AL failures — the lock
/// is held by the caller (the supervised task or the manual
/// trigger handler), so internally we don't try_lock again.
pub async fn refresh_all(db: &SqlitePool) -> Result<RefreshSummary, String> {
    let series = load_series_ids(db).await?;
    refresh_inner(db, &series).await
}

/// Refresh airings for a single series. Used by the library add
/// path so freshly-added series show up in the calendar without
/// waiting for the next 12h tick. Quiet on AL failures — the
/// background task will pick the series up on its next sweep.
pub async fn refresh_for_series(db: &SqlitePool, series_id: i64) -> Result<RefreshSummary, String> {
    let row: Option<i64> =
        sqlx::query_scalar("SELECT anilist_id FROM series WHERE id = ? AND anilist_id > 0")
            .bind(series_id)
            .fetch_optional(db)
            .await
            .map_err(|e| format!("series lookup: {e}"))?;
    let Some(anilist_id) = row else {
        // Series doesn't exist or is a Jikan-fallback (negative AL
        // id sentinel). Either way there's nothing to fetch — AL
        // can't serve airings for these, and the caller treats this
        // as a soft no-op.
        return Ok(RefreshSummary::default());
    };
    refresh_inner(
        db,
        &[SeriesRef {
            id: series_id,
            anilist_id: anilist_id as i32,
        }],
    )
    .await
}

#[derive(Debug, Clone, Copy)]
struct SeriesRef {
    id: i64,
    anilist_id: i32,
}

async fn load_series_ids(db: &SqlitePool) -> Result<Vec<SeriesRef>, String> {
    let rows = sqlx::query("SELECT id, anilist_id FROM series WHERE anilist_id > 0")
        .fetch_all(db)
        .await
        .map_err(|e| format!("series id query: {e}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: i64 = r.try_get("id").unwrap_or(0);
        let anilist_id: i64 = r.try_get("anilist_id").unwrap_or(0);
        if id > 0 && anilist_id > 0 {
            out.push(SeriesRef {
                id,
                anilist_id: anilist_id as i32,
            });
        }
    }
    Ok(out)
}

async fn refresh_inner(db: &SqlitePool, series: &[SeriesRef]) -> Result<RefreshSummary, String> {
    let mut summary = RefreshSummary {
        series_scanned: series.len(),
        ..Default::default()
    };
    if series.is_empty() {
        // Still prune so a fully-emptied library doesn't keep stale rows.
        summary.airings_pruned = prune(db).await.unwrap_or(0);
        return Ok(summary);
    }

    let now = crate::services::datetime::now_unix();
    let from = now - BACKWARD_WINDOW_DAYS * 86_400;
    let to = now + FORWARD_WINDOW_DAYS * 86_400;

    let ids: Vec<i32> = series.iter().map(|s| s.anilist_id).collect();
    let schedules = match airing_schedules::fetch_airing_schedules(&ids, from, to).await {
        Ok(s) => s,
        Err(e) => {
            // Surface the AL failure in the summary but don't abort
            // the prune step — old rows should still age out even
            // if we couldn't fetch new ones.
            summary.al_failures = 1;
            tracing::warn!(target: "ryokan::airing_refresh", "airingSchedules fetch failed: {e}");
            summary.airings_pruned = prune(db).await.unwrap_or(0);
            return Ok(summary);
        }
    };

    // Group the flat schedule list by AL id so we can run one
    // upsert transaction per series.
    use std::collections::HashMap;
    let mut by_anilist_id: HashMap<i32, Vec<EpisodeAiring>> = HashMap::new();
    for s in schedules {
        // Map AL id back to Ryokan series_id. Drop schedules for
        // ids we didn't ask about (defensive — `mediaId_in` should
        // restrict, but AL has surprised us before).
        let Some(series_ref) = series.iter().find(|sr| sr.anilist_id == s.media_id) else {
            continue;
        };
        by_anilist_id
            .entry(s.media_id)
            .or_default()
            .push(EpisodeAiring {
                series_id: series_ref.id,
                episode: s.episode,
                airing_at: s.airing_at,
                duration_minutes: s.duration_minutes.unwrap_or(24),
            });
    }

    for (al_id, rows) in by_anilist_id {
        // The first row carries the series_id; all rows for one al
        // id share it.
        let series_id = rows[0].series_id;
        match episode_airings::upsert_for_series(db, series_id, &rows).await {
            Ok(()) => summary.airings_upserted += rows.len(),
            Err(e) => {
                summary.al_failures += 1;
                tracing::warn!(
                    target: "ryokan::airing_refresh",
                    "upsert failed for series_id={series_id} al_id={al_id}: {e}"
                );
            }
        }
    }

    summary.airings_pruned = prune(db).await.unwrap_or(0);
    Ok(summary)
}

async fn prune(db: &SqlitePool) -> Result<u64, String> {
    let cutoff = crate::services::datetime::now_unix() - RETAIN_PAST_DAYS * 86_400;
    episode_airings::prune_older_than(db, cutoff)
        .await
        .map_err(|e| format!("airings prune: {e}"))
}

/// Span of one refresh tick — used to seed the supervised task's
/// startup sleep so a process restart mid-window doesn't re-fire
/// the sweep. Mirrors `metadata_refresh`'s 12h cadence.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::test_support::in_memory_pool().await
    }

    #[tokio::test]
    async fn refresh_with_empty_library_runs_prune_and_returns_empty_summary() {
        let pool = pool().await;
        let summary = refresh_all(&pool).await.unwrap();
        assert_eq!(summary.series_scanned, 0);
        assert_eq!(summary.airings_upserted, 0);
        assert_eq!(summary.al_failures, 0);
    }

    #[tokio::test]
    async fn refresh_for_negative_id_series_is_noop() {
        let pool = pool().await;
        sqlx::query("INSERT INTO series (anilist_id, title, monitor_mode) VALUES (?, ?, 'all')")
            .bind(-12345_i64)
            .bind("MAL-only")
            .execute(&pool)
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = -12345")
            .fetch_one(&pool)
            .await
            .unwrap();
        let summary = refresh_for_series(&pool, id).await.unwrap();
        // No AL fetch, no upsert, no failure.
        assert_eq!(summary.airings_upserted, 0);
        assert_eq!(summary.al_failures, 0);
    }

    #[test]
    fn summary_detail_string_is_human_readable() {
        let s = RefreshSummary {
            series_scanned: 17,
            airings_upserted: 42,
            airings_pruned: 3,
            al_failures: 1,
        };
        assert_eq!(s.detail(), "series=17, upserted=42, pruned=3, failures=1");
    }
}
