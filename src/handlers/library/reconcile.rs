//! Series-row resolution and metadata reconciliation.
//!
//! Split out of `handlers::library::mod` — these helpers translate a
//! request-time series id into (tracked row, provider id, AniList detail)
//! and handle the MAL→AniList reconciliation pass that upgrades
//! Jikan-sourced rows once the AniList ID becomes known.
//!
//! Exposes:
//! - `resolve_tracked_series` — cheap DB-only lookup for polling endpoints
//! - `resolve_series_context` — full path: cache + AniList + Jikan + Kitsu fallbacks
//! - `reconcile_all_fallback_entries` — bulk upgrade pass
//! - `populate_series_cover_urls` — shared cached-cover backfill for page lists
//! - `maybe_hydrate_cumulative_offset` — grab-time PREQUEL chain hydration
//! - `force_mal_fallback_enabled` / `force_kitsu_fallback_enabled` — config getters
//! - `ReconcileReport` — the handler return type for the bulk reconcile endpoint

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use serde::Serialize;
use sqlx::SqlitePool;

use crate::models::log::LogCategory;
use crate::models::{config, metadata_cache, series};
use crate::services::{anilist, jikan, kitsu, logger, metadata_sync};

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileReport {
    pub checked: usize,
    pub upgraded: usize,
    pub failed: usize,
}

pub(super) async fn force_mal_fallback_enabled(db: &SqlitePool) -> bool {
    config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.force_mal_fallback)
        .unwrap_or(false)
}

pub(super) async fn force_kitsu_fallback_enabled(db: &SqlitePool) -> bool {
    config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.force_kitsu_fallback)
        .unwrap_or(false)
}

/// #26 — Process-local set of series IDs that have had grab-time
/// PREQUEL-chain hydration attempted this process lifetime. Stops
/// `maybe_hydrate_cumulative_offset` from re-running on every
/// auto-search when a previous hydration legitimately yielded a
/// cumulative of 0 (or failed due to a transient AL rate-limit). A
/// process restart or the periodic `metadata_refresh` sweep will
/// re-try naturally.
static HYDRATED_CUMULATIVE: LazyLock<Mutex<HashSet<i64>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// #26 — Grab-time lazy hydration of `cumulative_prior_episodes`.
///
/// The Seerr fan-out path in `sonarr_compat::add_series` seeds new
/// sibling Ryokan series without walking each one's PREQUEL chain,
/// because fanning that out inline would stall the Seerr response
/// behind N × the hydration cost. Instead we defer the walk until the
/// first auto-search actually runs for a series — at which point we
/// need the offset for absolute-numbered release routing anyway, and
/// the cost is amortized against work the user is explicitly asking
/// for.
///
/// Gate: fires only when the stored cumulative is 0 AND the cached
/// detail advertises a TV PREQUEL edge. First-cour series with no TV
/// prequel (Attack on Titan S1, JJK S1 — whose only prequel is the
/// movie JJK 0, which the TV filter excludes) skip the walk since
/// cumulative=0 is already the correct answer.
///
/// Memoization: a process-local set ensures one hydration attempt per
/// series per process. Persistent AL rate-limit or a legitimately
/// zero-summing TV prequel chain (possible when an airing prequel
/// hasn't had its episode count confirmed yet) don't hammer this path
/// on every search.
pub(super) async fn maybe_hydrate_cumulative_offset(
    db: &SqlitePool,
    tracked: Option<series::Series>,
    detail: &anilist::AnimeDetail,
) -> Option<series::Series> {
    let t = tracked?;
    if t.cumulative_prior_episodes != 0 {
        return Some(t);
    }
    let has_tv_prequel = detail
        .relations
        .iter()
        .any(|r| r.relation_type == "PREQUEL" && r.media_type == "ANIME" && r.format == "TV");
    if !has_tv_prequel {
        return Some(t);
    }
    {
        let mut set = HYDRATED_CUMULATIVE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !set.insert(t.id) {
            return Some(t);
        }
    }
    let force_mal = force_mal_fallback_enabled(db).await;
    match metadata_sync::refresh_series_metadata(db, &t, force_mal).await {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                target: "ryokan::library",
                series_id = t.id,
                "grab-time PREQUEL hydration failed: {e}"
            );
            return Some(t);
        }
    }
    let refreshed = series::get_by_id(db, t.id).await.ok().flatten();
    if let Some(ref r) = refreshed {
        logger::info(
            db,
            LogCategory::AniList,
            &format!(
                "Hydrated PREQUEL chain for {}: cumulative_prior_episodes={}",
                r.title, r.cumulative_prior_episodes
            ),
            &format!(
                "series_id={}, anilist_id={}, prior={}",
                r.id, r.anilist_id, t.cumulative_prior_episodes
            ),
        )
        .await;
    }
    refreshed.or(Some(t))
}

async fn resolve_series_request(
    db: &SqlitePool,
    request_id: i64,
) -> Result<(Option<series::Series>, i64), sqlx::Error> {
    if let Some(row) = series::get_by_id(db, request_id).await? {
        Ok((Some(row.clone()), row.anilist_id))
    } else if let Some(row) = series::get_by_anilist_id(db, request_id).await? {
        Ok((Some(row.clone()), row.anilist_id))
    } else {
        Ok((None, request_id))
    }
}

/// Lean version of [`resolve_series_context`] for polling endpoints that
/// only need the tracked series row, never metadata.
///
/// `resolve_series_context` is the correct call for page loads — it
/// resolves the series, pulls cached metadata, kicks off a background
/// refresh if stale, and falls back through AniList / Jikan / Kitsu when
/// the cache misses. All of that is wasted work for an endpoint like
/// `episode_download_progress` or any other `/api/series/<id>/...` call
/// that only consults `tracked.id`. On a series page with an open
/// download, the progress poller fires every 5 seconds — three or four
/// unnecessary DB round-trips per poll per open tab add up fast.
///
/// This path does exactly one or two `series::get_by_*` queries (by
/// internal ID first, then by AniList ID as a fallback) and returns the
/// row. Callers that don't find a tracked row can treat it as "not in
/// library" without any further fallback.
pub(super) async fn resolve_tracked_series(
    db: &SqlitePool,
    request_id: i64,
) -> Result<Option<series::Series>, sqlx::Error> {
    if let Some(row) = series::get_by_id(db, request_id).await? {
        return Ok(Some(row));
    }
    series::get_by_anilist_id(db, request_id).await
}

async fn maybe_reconcile_mal_entry(
    db: &SqlitePool,
    db_series: Option<series::Series>,
) -> Option<(series::Series, anilist::AnimeDetail)> {
    let existing = db_series?;
    let mal_id = existing.mal_id?;
    if existing.anilist_id > 0 {
        return None;
    }

    // Single round-trip: AniList's `Media(idMal:)` query returns the same
    // full detail payload we'd get from `Media(id:)`, so the previous
    // "find then fetch detail" two-step has been collapsed.
    let detail = match anilist::find_anime_detail_by_mal_id(mal_id).await {
        Ok(Some(d)) => d,
        _ => return None,
    };

    let primary_title = if !detail.title_english.is_empty() {
        detail.title_english.clone()
    } else {
        detail.title_romaji.clone()
    };
    if series::upsert(
        db,
        series::SeriesCore {
            anilist_id: detail.id,
            mal_id: detail.id_mal,
            title: &primary_title,
            title_romaji: &detail.title_romaji,
            title_english: &detail.title_english,
            title_native: &detail.title_native,
            cover_url: &detail.cover_url,
            format: &detail.format,
            status: &detail.status,
            episodes: detail.episodes,
            season_year: detail.season_year,
            end_year: detail.end_year,
        },
    )
    .await
    .is_err()
    {
        return None;
    }

    let refreshed = match series::get_by_id(db, existing.id).await {
        Ok(Some(row)) => row,
        _ => return None,
    };

    // The series row now has the positive AniList id, but the metadata
    // cache (`series_metadata_cache`) still holds the old MAL-sourced
    // detail with `id < 0`. The series-detail page reads the cache
    // first and uses `detail.id < 0` to decide whether to render a
    // MyAnimeList vs AniList external link, so without this overwrite
    // the page keeps showing the MAL link until the cache TTL expires
    // (METADATA_REFRESH_INTERVAL_HOURS) or a manual rebuild fires.
    // Best-effort: a write failure here just leaves the stale cache to
    // expire on its own — reconciliation already updated the source of
    // truth (series.anilist_id), so the next refresh sweep will fix it.
    if let Err(e) =
        metadata_cache::upsert(db, refreshed.id, detail.id, detail.id_mal, &detail).await
    {
        tracing::warn!(
            "reconcile: failed to refresh series_metadata_cache for series_id={}: {}",
            refreshed.id,
            e
        );
    }

    Some((refreshed, detail))
}

/// The series row (when tracked), the provider id, and the detail every
/// search path works from. The series' alternate titles are folded into
/// the detail's synonyms here, once, on every path the resolver takes
/// (cache hit, live AniList fetch, MAL, Kitsu), so a title the user
/// added a minute ago counts before the first metadata sync.
pub(super) async fn resolve_series_context(
    db: &SqlitePool,
    request_id: i64,
) -> Result<(Option<series::Series>, i64, anilist::AnimeDetail), String> {
    let (row, provider_id, detail) = resolve_series_context_raw(db, request_id).await?;
    let alt = row
        .as_ref()
        .map(|s| s.alternate_titles.as_str())
        .unwrap_or("");
    let detail = crate::services::auto_search::with_alternate_titles(detail, alt);
    Ok((row, provider_id, detail))
}

async fn resolve_series_context_raw(
    db: &SqlitePool,
    request_id: i64,
) -> Result<(Option<series::Series>, i64, anilist::AnimeDetail), String> {
    let force_fallback = force_mal_fallback_enabled(db).await;
    let (resolved_row, mut provider_id) = resolve_series_request(db, request_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut db_series = resolved_row.clone();

    if let Some(ref tracked) = db_series {
        if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, tracked.id).await {
            if !cached.is_fresh {
                let db_clone = db.clone();
                let tracked_clone = tracked.clone();
                tokio::spawn(async move {
                    let force_fallback = crate::models::config::get_config(&db_clone)
                        .await
                        .ok()
                        .flatten()
                        .map(|c| c.force_mal_fallback)
                        .unwrap_or(false);
                    let _ = metadata_sync::refresh_series_metadata(
                        &db_clone,
                        &tracked_clone,
                        force_fallback,
                    )
                    .await;
                });
            }
            return Ok((db_series, cached.provider_id, cached.detail));
        }
        if tracked.anilist_id > 0 {
            if let Ok(Some(cached)) =
                metadata_cache::get_by_provider_id(db, tracked.anilist_id).await
            {
                return Ok((db_series, cached.provider_id, cached.detail));
            }
        } else if tracked.anilist_id < 0 {
            // MAL-sourced entry: check provider cache with the negative ID.
            if let Ok(Some(cached)) =
                metadata_cache::get_by_provider_id(db, tracked.anilist_id).await
            {
                return Ok((db_series, cached.provider_id, cached.detail));
            }
        }
    } else if provider_id != 0
        && let Ok(Some(cached)) = metadata_cache::get_by_provider_id(db, provider_id).await
    {
        return Ok((db_series, cached.provider_id, cached.detail));
    }

    let mal_hint = db_series.as_ref().and_then(|s| s.mal_id);
    let mut detail = match anilist::get_anime_detail_with_options(
        provider_id,
        mal_hint,
        force_fallback,
    )
    .await
    {
        Ok(d) => d,
        Err(e) => {
            if let Some((reconciled, upgraded_detail)) =
                maybe_reconcile_mal_entry(db, db_series.clone()).await
            {
                provider_id = reconciled.anilist_id;
                db_series = Some(reconciled);
                upgraded_detail
            } else {
                let fallback_mal_id =
                    mal_hint.or_else(|| db_series.as_ref().and_then(|s| s.mal_id));
                if let Some(mid) = fallback_mal_id {
                    let fallback_msg = format!(
                        "AniList detail failed for id={}; falling back to Jikan (mal_id={})",
                        provider_id, mid
                    );
                    logger::warn(db, LogCategory::AniList, &fallback_msg, &e).await;
                    if let Some(ref tracked) = db_series
                        && let Ok(Some(cached)) =
                            metadata_cache::get_by_series_id(db, tracked.id).await
                    {
                        logger::info(
                            db,
                            LogCategory::AniList,
                            &format!("Using cached metadata for {}", tracked.title),
                            &format!("cached_at={}", cached.cached_at),
                        )
                        .await;
                        return Ok((db_series, cached.provider_id, cached.detail));
                    }
                    match jikan::get_anime_detail_cached(mid).await {
                        Ok(detail) => detail,
                        Err(je) => {
                            if let Some(ref tracked) = db_series {
                                // Try Kitsu's MAL-mapping filter first (1 exact-match
                                // request) before falling back to the title-fuzz path
                                // (1–4 fuzzy requests).
                                if let Ok(Some(kitsu_detail)) =
                                    kitsu::get_anime_detail_by_mal_id(mid).await
                                {
                                    logger::warn(db, LogCategory::AniList, "AniList and MAL detail failed; using Kitsu fallback (mapping)", &tracked.title).await;
                                    return Ok((db_series, kitsu_detail.id, kitsu_detail));
                                }
                                let kitsu_titles = vec![
                                    tracked.title.clone(),
                                    tracked.title_romaji.clone(),
                                    tracked.title_english.clone(),
                                    tracked.title_native.clone(),
                                ];
                                if let Ok(kitsu_detail) = kitsu::get_anime_detail_by_titles(
                                    &kitsu_titles,
                                    None,
                                    tracked.episodes,
                                )
                                .await
                                {
                                    logger::warn(db, LogCategory::AniList, "AniList and MAL detail failed; using Kitsu fallback (titles)", &tracked.title).await;
                                    return Ok((db_series, kitsu_detail.id, kitsu_detail));
                                }
                            }
                            return Err(format!("{} (Jikan fallback also failed: {})", e, je));
                        }
                    }
                } else {
                    if let Some(ref tracked) = db_series {
                        if let Ok(Some(cached)) =
                            metadata_cache::get_by_series_id(db, tracked.id).await
                        {
                            logger::info(
                                db,
                                LogCategory::AniList,
                                &format!("Using cached metadata for {}", tracked.title),
                                &format!("cached_at={}", cached.cached_at),
                            )
                            .await;
                            return Ok((db_series, cached.provider_id, cached.detail));
                        }
                        // Prefer Kitsu's MAL-mapping filter when a MAL id is
                        // available — single exact-match request rather than the
                        // 1–4 fuzzy queries the title path issues.
                        if let Some(mid) = tracked.mal_id
                            && let Ok(Some(kitsu_detail)) =
                                kitsu::get_anime_detail_by_mal_id(mid).await
                        {
                            logger::warn(
                                db,
                                LogCategory::AniList,
                                "AniList and MAL detail failed; using Kitsu fallback (mapping)",
                                &tracked.title,
                            )
                            .await;
                            return Ok((db_series, kitsu_detail.id, kitsu_detail));
                        }
                        let kitsu_titles = vec![
                            tracked.title.clone(),
                            tracked.title_romaji.clone(),
                            tracked.title_english.clone(),
                            tracked.title_native.clone(),
                        ];
                        if let Ok(kitsu_detail) =
                            kitsu::get_anime_detail_by_titles(&kitsu_titles, None, tracked.episodes)
                                .await
                        {
                            logger::warn(
                                db,
                                LogCategory::AniList,
                                "AniList and MAL detail failed; using Kitsu fallback (titles)",
                                &tracked.title,
                            )
                            .await;
                            return Ok((db_series, kitsu_detail.id, kitsu_detail));
                        }
                    }
                    return Err(e);
                }
            }
        }
    };

    if !force_fallback
        && let Some((reconciled, upgraded_detail)) =
            maybe_reconcile_mal_entry(db, db_series.clone()).await
    {
        provider_id = reconciled.anilist_id;
        db_series = Some(reconciled);
        detail = upgraded_detail;
    }

    if db_series.is_none() {
        db_series = if let Some(mid) = detail.id_mal {
            series::get_by_mal_id(db, mid).await.ok().flatten()
        } else {
            series::get_by_anilist_id(db, detail.id)
                .await
                .ok()
                .flatten()
        };
    }

    if detail.id != 0 {
        let _ = metadata_cache::upsert_provider(db, detail.id, detail.id_mal, &detail).await;
    }
    if let Some(ref tracked) = db_series
        && should_persist_detail_cache(tracked.anilist_id, &detail)
    {
        let _ = metadata_cache::upsert(db, tracked.id, detail.id, detail.id_mal, &detail).await;
    }
    // NOTE: we intentionally do NOT pre-warm the Jikan episode cache here.
    // `build_episodes` calls `jikan::fetch_episode_titles_for_detail` itself
    // with the same (db, detail) arguments; calling it here too would double
    // the work on every page load (cache lookup + decode) for zero benefit.

    Ok((db_series, provider_id, detail))
}

fn should_persist_detail_cache(tracked_anilist_id: i64, detail: &anilist::AnimeDetail) -> bool {
    if tracked_anilist_id <= 0 {
        return true;
    }
    detail.id > 0 && detail.id == tracked_anilist_id
}

#[cfg(test)]
pub(crate) fn should_persist_detail_cache_for_test(
    tracked_anilist_id: i64,
    detail: &anilist::AnimeDetail,
) -> bool {
    should_persist_detail_cache(tracked_anilist_id, detail)
}

pub(super) async fn reconcile_all_fallback_entries(db: &SqlitePool) -> ReconcileReport {
    let rows = match series::get_unreconciled_fallbacks(db).await {
        Ok(rows) => rows,
        Err(_) => {
            return ReconcileReport {
                checked: 0,
                upgraded: 0,
                failed: 1,
            };
        }
    };

    let mut report = ReconcileReport {
        checked: rows.len(),
        upgraded: 0,
        failed: 0,
    };
    for row in rows {
        if maybe_reconcile_mal_entry(db, Some(row)).await.is_some() {
            report.upgraded += 1;
        } else {
            report.failed += 1;
        }
    }
    report
}

/// Fill each item's cover URL with the cached `/media/art/...` URL when
/// one exists for `series-<id>-cover`. Wraps the build-cache-keys →
/// batch-fetch → zip-write pattern shared by `index` and
/// `needs_review_page` — both used to fire one
/// `artwork::cached_or_source_url` per series, which on a 200-series
/// library was 200 sequential SQLite queries before the topbar even
/// rendered.
pub(crate) async fn populate_series_cover_urls<T, S, M>(
    db: &sqlx::SqlitePool,
    items: &mut [T],
    series_id_of: S,
    set_cover: M,
) where
    S: Fn(&T) -> i64,
    M: Fn(&mut T, String),
{
    if items.is_empty() {
        return;
    }
    let cache_keys: Vec<String> = items
        .iter()
        .map(|item| format!("series-{}-cover", series_id_of(item)))
        .collect();
    let Ok(url_map) = crate::models::artwork_cache::get_local_urls_batch(db, &cache_keys).await
    else {
        return;
    };
    for (item, key) in items.iter_mut().zip(cache_keys.iter()) {
        if let Some(url) = url_map.get(key) {
            set_cover(item, url.clone());
        }
    }
}
