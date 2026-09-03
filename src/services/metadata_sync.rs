use std::collections::{HashMap, HashSet, VecDeque};

use sqlx::SqlitePool;

use crate::models::log::LogCategory;
use crate::models::{config, local_metadata, metadata_cache, series};
use crate::services::{anilist, artwork, jikan, kitsu, logger};

const MAX_RELATION_TREE_NODES: usize = 64;

/// `true` when `detail` is the canonical AL row for `tracked` — used to
/// decide whether to replace the stored `anilist_id` column with
/// `detail.id`. Strict: anything other than an exact AL-id match (or a
/// MAL-only tracked series) keeps the existing `tracked.anilist_id`
/// untouched so the next refresh tries AL again instead of locking the
/// row into a fallback provider's id space. See `is_trustworthy_write`
/// for the looser "write this data" gate.
fn is_authoritative_detail(tracked: &series::Series, detail: &anilist::AnimeDetail) -> bool {
    if tracked.anilist_id <= 0 {
        return true;
    }
    detail.id > 0 && detail.id == tracked.anilist_id
}

/// Map a fetched `detail` to the `LogCategory` of the provider that
/// produced it, so per-series success / fallback log rows show up
/// under the correct System → Logs filter.
///
///   - `detail.id < 0`  → Jikan (the negative `-mal_id` sentinel that
///     `services::jikan::*` stamps on every response).
///   - `detail.id > 0 && detail.id == tracked.anilist_id` → AniList
///     (canonical AL response for an AL-tracked series).
///   - everything else (positive id that doesn't equal the tracked
///     anilist_id, or any positive id when tracked has no AL id) →
///     Kitsu, since Kitsu's `to_anime_detail` stamps its own positive
///     id space which can't collide with AL or MAL.
///
/// Without this dispatch, every metadata-related log row was filed
/// under `LogCategory::AniList` even when AL was the unhealthy
/// provider and Jikan / Kitsu carried the load — making the System →
/// Logs Jikan/Kitsu filters dead-letter dropdowns and obscuring which
/// provider was actually responsible for each line during an outage.
///
/// **Caller invariant**: `tracked.anilist_id` must be non-zero. The
/// external-sync normalize pipeline uses `0` as an *intermediate*
/// sentinel during the MAL→AL merge, but by the time the metadata
/// sweep reads `tracked.anilist_id` it's been resolved to either a
/// positive AL id or a negative MAL fallback marker. A zero here
/// would mis-route an AL canonical response to Kitsu, so the
/// debug_assert in this function pins the contract.
fn provider_category_for_detail(
    tracked: &series::Series,
    detail: &anilist::AnimeDetail,
) -> LogCategory {
    debug_assert!(
        tracked.anilist_id != 0,
        "provider_category_for_detail requires a resolved tracked.anilist_id \
         (positive AL id or negative MAL sentinel); got 0, which is the \
         external-sync intermediate state and should never reach the \
         metadata sweep"
    );
    if detail.id < 0 {
        LogCategory::Jikan
    } else if detail.id > 0 && detail.id == tracked.anilist_id {
        LogCategory::AniList
    } else {
        LogCategory::Kitsu
    }
}

/// `true` when `detail` is *trustworthy enough to write* to the row
/// (core metadata, relations, episode cache), even if it isn't the
/// canonical AL detail. Looser than `is_authoritative_detail` because
/// it treats Jikan's negative-id fallback (`detail.id = -mal_id`) as
/// trustworthy — `mal_id` is an exact lookup, not a fuzzy title match,
/// so the data is correct just from a different provider.
///
/// Used so the periodic refresh writes MAL data through during an AL
/// outage instead of taking the "Preserving cached AniList relations"
/// branch and pinning every series to whatever stale row was last
/// written before AL went down.
///
/// Kitsu's title-fuzz fallback returns a positive id from Kitsu's own
/// id space that won't equal `tracked.anilist_id`, so the function
/// correctly rejects those — that's the sequel-mismatch defense the
/// `is_authoritative_detail` check was built around, preserved here.
fn is_trustworthy_write(tracked: &series::Series, detail: &anilist::AnimeDetail) -> bool {
    if is_authoritative_detail(tracked, detail) {
        return true;
    }
    detail.id < 0
}

fn title_candidates_for_series(tracked: &series::Series) -> Vec<String> {
    let mut titles = vec![
        tracked.title.clone(),
        tracked.title_romaji.clone(),
        tracked.title_english.clone(),
        tracked.title_native.clone(),
    ];
    titles.retain(|t| !t.trim().is_empty());
    titles
}

async fn fetch_live_detail(
    tracked: &series::Series,
    force_mal_fallback: bool,
) -> Result<anilist::AnimeDetail, String> {
    fetch_live_detail_for_ids(
        tracked.anilist_id,
        tracked.mal_id,
        &title_candidates_for_series(tracked),
        tracked.episodes,
        force_mal_fallback,
    )
    .await
}

async fn fetch_live_detail_for_ids(
    provider_id: i64,
    mal_id: Option<i64>,
    title_candidates: &[String],
    episode_count: Option<i32>,
    force_mal_fallback: bool,
) -> Result<anilist::AnimeDetail, String> {
    // Fallback policy: when an entry has a real AniList ID and the user
    // hasn't explicitly opted into MAL, MAL is only used when AL is
    // genuinely *down* (5xx, network error, parse failure, etc.). A 429
    // rate-limit means AL is responding but throttling us — we surface
    // an Err so the caller can defer-and-retry, preserving AL fidelity
    // instead of silently substituting MAL data. Persistent rate-limits
    // are the user's own library not getting fully refreshed; we'd
    // rather leave the previous AL data in place than overwrite it with
    // a MAL approximation.
    if provider_id > 0 && !force_mal_fallback {
        if anilist::anilist_cooldown_active() {
            return Err(format!(
                "AniList rate-limit cooldown active for provider_id={} \
                 (no MAL/Kitsu fallback)",
                provider_id
            ));
        }
        match anilist::get_anime_detail_with_options(provider_id, mal_id, false).await {
            Ok(detail) => return Ok(detail),
            Err(err) => {
                if anilist::is_rate_limit_error(&err) {
                    tracing::warn!(
                        target: "ryokan::metadata_sync",
                        provider_id,
                        mal_id = ?mal_id,
                        error = %err,
                        "AniList rate-limited (no MAL/Kitsu fallback)"
                    );
                    return Err(err);
                }
                tracing::warn!(
                    target: "ryokan::metadata_sync",
                    provider_id,
                    mal_id = ?mal_id,
                    error = %err,
                    "AniList detail fetch failed; falling back to MAL/Kitsu"
                );
            }
        }
    }

    if let Some(mid) = mal_id {
        match jikan::get_anime_detail_cached(mid).await {
            Ok(detail) => return Ok(detail),
            Err(err) => {
                tracing::warn!(
                    target: "ryokan::metadata_sync",
                    mal_id = mid,
                    error = %err,
                    "Jikan/MAL detail fetch failed; falling back to Kitsu"
                );
            }
        }

        // Kitsu can resolve a MAL id directly via its mappings filter
        // in one round-trip — try that before the multi-query title-fuzz
        // path, which costs 1–4 requests and risks a sequel false match.
        match kitsu::get_anime_detail_by_mal_id(mid).await {
            Ok(Some(detail)) => return Ok(detail),
            Ok(None) => {
                tracing::debug!(
                    target: "ryokan::metadata_sync",
                    mal_id = mid,
                    "Kitsu has no mapping for this MAL id; falling back to title fuzz"
                );
            }
            Err(err) => {
                tracing::warn!(
                    target: "ryokan::metadata_sync",
                    mal_id = mid,
                    error = %err,
                    "Kitsu mapping lookup failed; falling back to title fuzz"
                );
            }
        }
    }

    if !title_candidates.is_empty() {
        return kitsu::get_anime_detail_by_titles(title_candidates, None, episode_count).await;
    }

    anilist::get_anime_detail_with_options(provider_id, mal_id, force_mal_fallback).await
}

fn episode_needs_kitsu_backfill<F>(ep_count: i32, mut has_jikan_title: F) -> bool
where
    F: FnMut(i32) -> bool,
{
    if ep_count <= 1 {
        return false;
    }

    (1..=ep_count).any(|ep_num| !has_jikan_title(ep_num))
}

async fn build_episode_cache(
    db: &SqlitePool,
    detail: &anilist::AnimeDetail,
    force_kitsu_fallback: bool,
) -> Vec<local_metadata::CachedEpisodeMetadata> {
    // Use the effective count so airing series (episodes=null on AniList)
    // still get an episode cache built from `nextAiringEpisode - 1`. Without
    // this, shows like One Piece end up with zero rows in
    // `series_episode_metadata`, which in turn leaves `episode_monitor_state`
    // empty and breaks the monitoring UI.
    let ep_count = detail.effective_episode_count();
    let episodic_format = !matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA");
    let should_fetch_jikan = episodic_format || ep_count > 1;

    let mut jikan_eps = if should_fetch_jikan {
        jikan::fetch_episode_titles_for_detail(db, detail).await
    } else {
        HashMap::new()
    };

    let kitsu_titles = vec![
        detail.title_english.clone(),
        detail.title_romaji.clone(),
        detail.title_native.clone(),
    ];
    let should_try_kitsu = ep_count > 1
        && (force_kitsu_fallback
            || episode_needs_kitsu_backfill(ep_count, |ep_num| {
                jikan_eps
                    .get(&ep_num)
                    .map(|info| !info.title.trim().is_empty())
                    .unwrap_or(false)
            }));

    let kitsu_eps = if should_try_kitsu {
        kitsu::fetch_episode_titles_fallback(db, &kitsu_titles, detail.season_year, Some(ep_count))
            .await
    } else {
        HashMap::new()
    };

    let mut merged = Vec::new();
    for ep_num in 1..=ep_count {
        let fallback_title = if ep_count <= 1 {
            if !detail.title_english.trim().is_empty() {
                detail.title_english.clone()
            } else if !detail.title_romaji.trim().is_empty() {
                detail.title_romaji.clone()
            } else {
                detail.title_native.clone()
            }
        } else {
            String::new()
        };

        let local = if force_kitsu_fallback {
            kitsu_eps
                .get(&ep_num)
                .map(|kitsu| {
                    (
                        if !kitsu.title.trim().is_empty() {
                            kitsu.title.clone()
                        } else {
                            fallback_title.clone()
                        },
                        kitsu.aired.clone(),
                        "kitsu".to_string(),
                    )
                })
                .or_else(|| {
                    jikan_eps.get(&ep_num).map(|j| {
                        (
                            if !j.title.trim().is_empty() {
                                j.title.clone()
                            } else {
                                fallback_title.clone()
                            },
                            j.aired.clone(),
                            "jikan".to_string(),
                        )
                    })
                })
        } else {
            jikan_eps
                .remove(&ep_num)
                .map(|j| {
                    (
                        if !j.title.trim().is_empty() {
                            j.title
                        } else {
                            fallback_title.clone()
                        },
                        j.aired,
                        "jikan".to_string(),
                    )
                })
                .or_else(|| {
                    kitsu_eps.get(&ep_num).map(|kitsu| {
                        (
                            if !kitsu.title.trim().is_empty() {
                                kitsu.title.clone()
                            } else {
                                fallback_title.clone()
                            },
                            kitsu.aired.clone(),
                            "kitsu".to_string(),
                        )
                    })
                })
        };

        let (title, aired, source) =
            local.unwrap_or((fallback_title.clone(), String::new(), "series".to_string()));
        merged.push(local_metadata::CachedEpisodeMetadata {
            episode_number: ep_num,
            title: title.clone(),
            title_romaji: title.clone(),
            title_english: title.clone(),
            title_native: title,
            aired,
            source,
        });
    }

    merged
}

async fn cache_provider_detail(
    db: &SqlitePool,
    cache_provider_id: i64,
    detail: &anilist::AnimeDetail,
    force_kitsu_fallback: bool,
) -> Result<(), String> {
    if cache_provider_id == 0 {
        return Ok(());
    }

    metadata_cache::upsert_provider(db, cache_provider_id, detail.id_mal, detail)
        .await
        .map_err(|e| e.to_string())?;
    local_metadata::replace_relations_for_provider(db, cache_provider_id, detail)
        .await
        .map_err(|e| e.to_string())?;
    let merged = build_episode_cache(db, detail, force_kitsu_fallback).await;
    local_metadata::replace_episode_metadata_for_provider(db, cache_provider_id, &merged)
        .await
        .map_err(|e| e.to_string())?;

    artwork::cache_provider_detail_artwork(db, cache_provider_id, detail.id_mal, detail).await;
    for related in &detail.relations {
        artwork::cache_provider_relation_artwork(
            db,
            cache_provider_id,
            related.id,
            related.id_mal,
            &related.cover_url,
        )
        .await;
    }
    Ok(())
}

async fn hydrate_relation_tree(
    db: &SqlitePool,
    root_provider_id: i64,
    root_detail: &anilist::AnimeDetail,
    force_mal_fallback: bool,
    force_kitsu_fallback: bool,
) {
    // Trees stay strictly separate. AL mode walks AL's relation graph
    // (positive AL IDs only); MAL mode walks MAL's, where Jikan stamps
    // each card with `id = -mal_id` as a "no AL mapping" sentinel. Mode
    // is determined by which provider gave us the root: positive root
    // detail id = AL, negative = MAL fallback (or user opted into MAL).
    // We never interleave the two — a MAL fallback mid-walk would
    // pollute AL's graph with MAL-only siblings (e.g. JoJo Part 6 is 3
    // entries on MAL but 2 on AL).
    //
    // Rate-limited AL relations defer-and-retry within the walk, but we
    // never substitute MAL on rate-limit; "MAL only when AL is down"
    // means non-rate-limit errors (5xx/network), which fetch_live_detail
    // already handles inline before this walker is even called.
    const MAX_AL_RETRY_ROUNDS: usize = 3;
    const COOLDOWN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    // Hard ceiling on total cooldown waits across all retry rounds for
    // a single hydration. With MAX_AL_RETRY_ROUNDS=3 and AniList's
    // 300s ANILIST_COOLDOWN_MAX, the worst-case naïve wait is ~15
    // minutes — long enough that a manual rebuild click feels broken
    // during a sustained AL outage. Cap at 5 minutes total so the
    // request returns predictably; remaining deferred relations get
    // picked up by the next periodic refresh.
    const MAX_HYDRATION_WAIT_TOTAL: std::time::Duration = std::time::Duration::from_secs(300);

    if root_provider_id == 0 {
        return;
    }

    let mal_mode = force_mal_fallback || root_detail.id < 0;

    let mut seen: HashSet<i64> = HashSet::new();
    let mut queue: VecDeque<(i64, Option<i64>)> = VecDeque::new();
    let mut deferred: VecDeque<(i64, Option<i64>)> = VecDeque::new();
    queue.push_back((root_provider_id, root_detail.id_mal));
    seen.insert(root_provider_id);
    let mut processed = 0usize;
    let mut al_round = 0usize;
    let mut total_cooldown_wait = std::time::Duration::ZERO;

    loop {
        // Pre-batch this round's pending AL ids into a single
        // `Page(media(id_in:[]))` call so the per-id fetch loop below
        // becomes a sequence of DETAIL_CACHE hits instead of N
        // sequential GraphQL round-trips. Skipped in MAL mode (Jikan
        // has no batch endpoint) and when AL is in cooldown (the
        // batch helper would short-circuit anyway, so don't even ask).
        if !mal_mode && !anilist::anilist_cooldown_active() {
            let pending_ids: Vec<i64> = queue
                .iter()
                .filter(|(id, _)| *id > 0 && *id != root_provider_id)
                .map(|(id, _)| *id)
                .collect();
            if !pending_ids.is_empty()
                && let Err(e) = anilist::get_anime_details_batch(&pending_ids).await
            {
                // Best-effort prefetch: a failure here just means
                // the per-id loop below pays the historical cost.
                // Cooldown / 429 has been recorded by the batch
                // helper already; the per-id loop will defer too.
                tracing::debug!(
                    target: "ryokan::metadata_sync",
                    error = %e,
                    "AniList batch prefetch failed; falling back to per-id"
                );
            }
        }

        while let Some((provider_id, mal_id)) = queue.pop_front() {
            if processed >= MAX_RELATION_TREE_NODES {
                break;
            }

            let detail = if provider_id == root_provider_id {
                root_detail.clone()
            } else {
                match fetch_live_detail_for_ids(
                    provider_id,
                    mal_id,
                    &Vec::new(),
                    None,
                    force_mal_fallback,
                )
                .await
                {
                    Ok(detail) => detail,
                    Err(err) => {
                        // Only AL rate-limits are worth retrying. MAL
                        // failures or genuine AL-down errors are already
                        // terminal by the time this returns.
                        if anilist::is_rate_limit_error(&err) && !mal_mode {
                            deferred.push_back((provider_id, mal_id));
                        }
                        continue;
                    }
                }
            };

            processed += 1;
            let _ = cache_provider_detail(db, provider_id, &detail, force_kitsu_fallback).await;

            for related in &detail.relations {
                let id_valid = if mal_mode {
                    related.id != 0
                } else {
                    related.id > 0
                };
                if id_valid
                    && matches!(related.media_type.as_str(), "ANIME" | "MUSIC")
                    && seen.insert(related.id)
                {
                    queue.push_back((related.id, related.id_mal));
                }
            }
        }

        let wait_budget_exhausted = total_cooldown_wait >= MAX_HYDRATION_WAIT_TOTAL;
        if deferred.is_empty() || al_round >= MAX_AL_RETRY_ROUNDS || wait_budget_exhausted {
            // Anything still deferred after the retry budget is left out
            // of this sweep's cache — the next periodic refresh picks it
            // up. We don't substitute MAL on rate-limit (would mix trees
            // and downgrade fidelity).
            if !deferred.is_empty() {
                tracing::warn!(
                    target: "ryokan::metadata_sync",
                    root_provider_id,
                    dropped = deferred.len(),
                    retry_rounds = al_round,
                    cooldown_wait_secs = total_cooldown_wait.as_secs(),
                    wait_budget_exhausted,
                    "relation hydration left {} relations unfetched after AniList \
                     retry budget exhausted; next sweep will retry",
                    deferred.len()
                );
            }
            break;
        }

        let wait_start = std::time::Instant::now();
        let remaining_budget = MAX_HYDRATION_WAIT_TOTAL.saturating_sub(total_cooldown_wait);
        while anilist::anilist_cooldown_active() {
            // Stop polling if the per-hydration wait budget is exhausted —
            // the next iteration of the outer loop will drop the
            // remaining deferred and exit. Without this guard a
            // pathological AL outage could pin the sweep for ~15 min.
            if wait_start.elapsed() >= remaining_budget {
                break;
            }
            tokio::time::sleep(COOLDOWN_POLL_INTERVAL).await;
        }
        total_cooldown_wait += wait_start.elapsed();

        queue.append(&mut deferred);
        al_round += 1;
    }
}

async fn refresh_series_metadata_inner(
    db: &SqlitePool,
    tracked: &series::Series,
    force_mal_fallback: bool,
    allow_degraded_cache_rebuild: bool,
) -> Result<anilist::AnimeDetail, String> {
    let detail = fetch_live_detail(tracked, force_mal_fallback).await?;
    let authoritative_detail = is_authoritative_detail(tracked, &detail);
    let trustworthy_write = is_trustworthy_write(tracked, &detail);

    let force_kitsu_fallback = config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|cfg| cfg.force_kitsu_fallback)
        .unwrap_or(false);

    // Strict: only replace the row's `anilist_id` column when we got
    // the canonical AL detail back. MAL fallbacks (negative `detail.id`)
    // and Kitsu title-fuzz mismatches both leave the existing
    // `tracked.anilist_id` in place so the next refresh tries AL again.
    let stored_anilist_id = if authoritative_detail {
        detail.id
    } else {
        tracked.anilist_id
    };

    if trustworthy_write || allow_degraded_cache_rebuild {
        let primary_title = if !detail.title_english.trim().is_empty() {
            &detail.title_english
        } else {
            &detail.title_romaji
        };
        series::refresh_core_metadata(
            db,
            tracked.id,
            series::SeriesCore {
                anilist_id: stored_anilist_id,
                mal_id: detail.id_mal,
                title: primary_title,
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
        .map_err(|e| e.to_string())?;
        series::set_is_adult(db, tracked.id, detail.is_adult)
            .await
            .map_err(|e| e.to_string())?;

        metadata_cache::upsert(db, tracked.id, stored_anilist_id, detail.id_mal, &detail)
            .await
            .map_err(|e| e.to_string())?;

        // #62 — extract genres into the per-row side table for
        // the library filter. Best-effort: a write failure here logs
        // but doesn't fail the whole metadata refresh — the cache
        // upsert above is the canonical source and the side table
        // can be rebuilt from it on the next tick.
        if let Err(e) =
            crate::models::series_genres::replace_for_series(db, tracked.id, &detail.genres).await
        {
            tracing::warn!(
                "series_genres::replace_for_series failed for series_id={}: {e}",
                tracked.id
            );
        }

        artwork::cache_series_detail_artwork(db, tracked.id, &detail).await;
        for related in detail
            .relations
            .iter()
            .filter(|r| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
        {
            artwork::cache_relation_artwork(
                db,
                tracked.id,
                related.id,
                related.id_mal,
                &related.cover_url,
            )
            .await;
        }

        local_metadata::replace_relations_for_series(db, tracked.id, &detail)
            .await
            .map_err(|e| e.to_string())?;

        cache_provider_detail(db, stored_anilist_id, &detail, force_kitsu_fallback).await?;
        hydrate_relation_tree(
            db,
            stored_anilist_id,
            &detail,
            force_mal_fallback,
            force_kitsu_fallback,
        )
        .await;

        // #30 — With the franchise graph freshly cached, walk the PREQUEL
        // chain and store the cumulative prior-cour episode count on
        // the series row. Search reads this at query time to match
        // absolute-numbered Nyaa titles against relative-numbered AL
        // episodes. Must run AFTER hydrate_relation_tree so the cache
        // covers the whole chain, not just the immediate neighbors.
        let cumulative =
            local_metadata::compute_cumulative_prior_episodes(db, stored_anilist_id).await;
        if let Err(err) = series::update_cumulative_prior_episodes(db, tracked.id, cumulative).await
        {
            tracing::warn!(
                target: "ryokan::metadata_sync",
                series_id = tracked.id,
                "failed to persist cumulative_prior_episodes: {err}"
            );
        }

        if !authoritative_detail {
            logger::info(
                db,
                provider_category_for_detail(tracked, &detail),
                &format!(
                    "Rebuilt cached metadata from fallback source for {}",
                    tracked.title
                ),
                &format!(
                    "provider_detail_id={}, preserved_anilist_id={}, mal_id={:?}",
                    detail.id, tracked.anilist_id, detail.id_mal
                ),
            )
            .await;
        }
    } else {
        logger::info(
            db,
            LogCategory::AniList,
            &format!("Preserving cached AniList relations for {}", tracked.title),
            &format!(
                "degraded provider detail id={} anilist_id={}",
                detail.id, tracked.anilist_id
            ),
        )
        .await;
    }

    let merged = build_episode_cache(db, &detail, force_kitsu_fallback).await;
    local_metadata::replace_episode_metadata(db, tracked.id, &merged)
        .await
        .map_err(|e| e.to_string())?;

    Ok(detail)
}

pub async fn refresh_series_metadata(
    db: &SqlitePool,
    tracked: &series::Series,
    force_mal_fallback: bool,
) -> Result<anilist::AnimeDetail, String> {
    refresh_series_metadata_inner(db, tracked, force_mal_fallback, false).await
}

/// Shared sweep driver for the manual rebuild and the periodic refresh.
/// `rebuild_artifacts = true` runs the full rebuild path (re-derives
/// episode metadata, artwork, etc. via refresh_series_metadata_inner's
/// `rebuild` flag); `false` runs the lighter periodic refresh.
///
/// Defer-and-retry policy: when a series is rate-limited by AniList,
/// it's parked in `deferred` instead of counted as `failed`. After the
/// main pass completes, the helper waits for the AniList cooldown to
/// clear and re-runs the deferred series. This is what makes the
/// manual rebuild button do what its name promises — a sweep that hits
/// rate limiting won't leave the user with stale or substituted data
/// for half their library; it'll finish what it started.
///
/// Bounded by `MAX_RETRY_ROUNDS` so a sustained AniList outage doesn't
/// pin the sweep forever; anything still deferred at the end counts as
/// failed and the next periodic refresh will pick it up.
async fn run_metadata_sweep(db: &SqlitePool, rebuild_artifacts: bool) -> (usize, usize) {
    const MAX_RETRY_ROUNDS: usize = 3;
    const COOLDOWN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    // Inter-series spacing: AniList allows 30 req/min for anonymous
    // clients, but in practice sustained bursts trip rate limits
    // even when the per-minute average is low. A 1-second sleep
    // between iterations paces the sweep at ~50 req/min worst-case
    // (one request every 1 + call-duration seconds, where the call
    // itself takes 0.5–2s for a typical entry), which empirically
    // stays under the rate limit on a small library.
    const INTER_SERIES_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
    // Hard ceiling on total cooldown waits across all retry rounds for
    // a single sweep. Without this, a sustained AL outage where
    // concurrent callers (library page renders, RSS-driven scoring,
    // transitive relation walks) keep observing 5xx responses
    // re-arms the cooldown every ~60s — faster than the poll
    // interval clears it — so the inner wait loop spins forever and
    // the manual `metadata_rebuild` task row stays at status='running'
    // until the process restarts. Mirrors the matching cap in
    // `hydrate_relation_tree`.
    const MAX_COOLDOWN_WAIT_TOTAL: std::time::Duration = std::time::Duration::from_secs(300);

    let sweep_label = if rebuild_artifacts {
        "Cached metadata rebuild"
    } else {
        "Metadata refresh"
    };
    let per_series_fail_label = if rebuild_artifacts {
        "Failed to rebuild cached metadata"
    } else {
        "Metadata refresh failed"
    };

    let tracked = match series::get_all(db).await {
        Ok(items) => items,
        Err(err) => {
            logger::error(
                db,
                LogCategory::AniList,
                &format!("{} sweep failed", sweep_label),
                &err.to_string(),
            )
            .await;
            return (0, 1);
        }
    };

    let force_mal_fallback = crate::models::config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.force_mal_fallback)
        .unwrap_or(false);

    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut deferred: Vec<series::Series> = Vec::new();

    let should_defer = |tracked: &series::Series| -> bool {
        anilist::anilist_cooldown_active() && tracked.anilist_id > 0 && !force_mal_fallback
    };

    // ── Main pass ────────────────────────────────────────────────────────
    let total = tracked.len();
    for (idx, tracked) in tracked.into_iter().enumerate() {
        // Pre-check: if AL cooldown is already active, don't burn the
        // call — defer immediately. (Saves a guaranteed-to-fail HTTP
        // round trip per series.)
        if should_defer(&tracked) {
            deferred.push(tracked);
            if idx + 1 < total {
                tokio::time::sleep(INTER_SERIES_DELAY).await;
            }
            continue;
        }

        match refresh_series_metadata_inner(db, &tracked, force_mal_fallback, rebuild_artifacts)
            .await
        {
            Ok(detail) => {
                succeeded += 1;
                if rebuild_artifacts {
                    logger::info(
                        db,
                        provider_category_for_detail(&tracked, &detail),
                        &format!("Rebuilt cached metadata for {}", tracked.title),
                        &format!(
                            "provider_id={}, anilist_id={}, mal_id={:?}, episodes={:?}",
                            detail.id, tracked.anilist_id, detail.id_mal, detail.episodes
                        ),
                    )
                    .await;
                }
            }
            Err(err) => {
                // Post-check: this call may have been the 429 that just
                // tripped the cooldown. If so, defer instead of failing
                // so the retry round picks it up.
                if should_defer(&tracked) {
                    deferred.push(tracked);
                } else {
                    failed += 1;
                    logger::warn(
                        db,
                        LogCategory::AniList,
                        &format!("{} for {}", per_series_fail_label, tracked.title),
                        &err,
                    )
                    .await;
                }
            }
        }
        if idx + 1 < total {
            tokio::time::sleep(INTER_SERIES_DELAY).await;
        }
    }

    // ── Retry rounds for deferred series ─────────────────────────────────
    let mut round = 0;
    let mut total_cooldown_wait = std::time::Duration::ZERO;
    while !deferred.is_empty() && round < MAX_RETRY_ROUNDS {
        if anilist::anilist_cooldown_active() {
            let remaining_budget = MAX_COOLDOWN_WAIT_TOTAL.saturating_sub(total_cooldown_wait);
            if remaining_budget.is_zero() {
                logger::warn(
                    db,
                    LogCategory::AniList,
                    &format!(
                        "{}: cooldown wait budget exhausted with {} series still deferred; \
                         marking remaining as failed",
                        sweep_label,
                        deferred.len()
                    ),
                    "",
                )
                .await;
                break;
            }
            logger::info(
                db,
                LogCategory::AniList,
                &format!(
                    "{}: waiting for AniList cooldown ({} series deferred, retry round {})",
                    sweep_label,
                    deferred.len(),
                    round + 1
                ),
                "",
            )
            .await;
            let wait_start = std::time::Instant::now();
            while anilist::anilist_cooldown_active() {
                if wait_start.elapsed() >= remaining_budget {
                    break;
                }
                tokio::time::sleep(COOLDOWN_POLL_INTERVAL).await;
            }
            total_cooldown_wait += wait_start.elapsed();
        }

        let to_retry = std::mem::take(&mut deferred);
        let total = to_retry.len();
        for (idx, tracked) in to_retry.into_iter().enumerate() {
            match refresh_series_metadata_inner(db, &tracked, force_mal_fallback, rebuild_artifacts)
                .await
            {
                Ok(detail) => {
                    succeeded += 1;
                    if rebuild_artifacts {
                        logger::info(
                            db,
                            provider_category_for_detail(&tracked, &detail),
                            &format!("Rebuilt cached metadata for {}", tracked.title),
                            &format!(
                                "provider_id={}, anilist_id={}, mal_id={:?}, episodes={:?}",
                                detail.id, tracked.anilist_id, detail.id_mal, detail.episodes
                            ),
                        )
                        .await;
                    }
                }
                Err(err) => {
                    if should_defer(&tracked) {
                        deferred.push(tracked);
                    } else {
                        failed += 1;
                        logger::warn(
                            db,
                            LogCategory::AniList,
                            &format!("{} for {}", per_series_fail_label, tracked.title),
                            &err,
                        )
                        .await;
                    }
                }
            }
            if idx + 1 < total {
                tokio::time::sleep(INTER_SERIES_DELAY).await;
            }
        }
        round += 1;
    }

    // Anything still deferred after MAX_RETRY_ROUNDS counts as failed —
    // at that point AniList is sustainedly unavailable and we should
    // surface that rather than spin. Next periodic refresh will pick it up.
    if !deferred.is_empty() {
        failed += deferred.len();
        for tracked in &deferred {
            logger::warn(
                db,
                LogCategory::AniList,
                &format!(
                    "{} skipped after {} retry rounds: {}",
                    sweep_label, MAX_RETRY_ROUNDS, tracked.title
                ),
                "AniList still rate-limited; will retry on next sweep",
            )
            .await;
        }
    }

    // Match the previous summary-log behaviour: rebuild always logs;
    // refresh only logs when something happened.
    if rebuild_artifacts || succeeded > 0 || failed > 0 {
        let detail = if rebuild_artifacts {
            format!("rebuilt={}, skipped=0, failed={}", succeeded, failed)
        } else {
            format!("refreshed={}, failed={}", succeeded, failed)
        };
        logger::info(
            db,
            LogCategory::AniList,
            &format!("{} sweep complete", sweep_label),
            &detail,
        )
        .await;
    }

    (succeeded, failed)
}

pub async fn rebuild_cached_metadata_for_all(db: &SqlitePool) -> (usize, usize, usize) {
    let (rebuilt, failed) = run_metadata_sweep(db, true).await;
    // The middle "skipped" counter has been zero for a while — the
    // sweep doesn't have a "skip without trying" branch — but the
    // tuple shape is part of the handler contract so keep it.
    (rebuilt, 0, failed)
}

pub async fn refresh_all_series_metadata(db: &SqlitePool) -> (usize, usize) {
    run_metadata_sweep(db, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series_fixture(anilist_id: i64) -> series::Series {
        series::Series {
            is_adult: false,
            id: 1,
            anilist_id,
            mal_id: None,
            title: String::new(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: String::new(),
            episodes: None,
            season_year: None,
            end_year: None,
            folder_name: String::new(),
            monitor_mode: "all".into(),
            allow_upgrades: true,
            allow_pt_upgrades: false,
            custom_query_tokens: String::new(),
            restrict_to_uploader: String::new(),
            cumulative_prior_episodes: 0,
            monitor_mode_manual_override: false,
            user_score: None,
            added_at: String::new(),
        }
    }

    fn detail_fixture(id: i64) -> anilist::AnimeDetail {
        anilist::AnimeDetail {
            is_adult: false,
            id,
            id_mal: None,
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: String::new(),
            status: String::new(),
            status_display: String::new(),
            episodes: None,
            duration: None,
            season: String::new(),
            season_year: None,
            end_year: None,
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

    // ── is_authoritative_detail ──────────────────────────────────────

    #[test]
    fn jikan_fallback_negative_id_is_always_authoritative() {
        // Negative-AL-id sentinel (`series.anilist_id = -mal_id` for
        // MAL-fallback series): no AL-id round-trip is even possible
        // for these, so we treat the response as authoritative
        // unconditionally rather than rejecting it for an id-mismatch
        // that isn't physically meaningful.
        let mut tracked = series_fixture(-12345);
        // Zero is also covered: any non-positive id means "no AL id."
        let detail = detail_fixture(0);
        assert!(is_authoritative_detail(&tracked, &detail));

        tracked.anilist_id = 0;
        assert!(is_authoritative_detail(&tracked, &detail));
    }

    #[test]
    fn detail_with_matching_id_is_authoritative() {
        let tracked = series_fixture(1234);
        let detail = detail_fixture(1234);
        assert!(is_authoritative_detail(&tracked, &detail));
    }

    #[test]
    fn detail_with_mismatched_id_is_not_authoritative() {
        // Title-fuzz fallback path (Kitsu by titles) can return a
        // sequel — same name, different id. Reject it so the caller
        // doesn't overwrite the canonical row's metadata.
        let tracked = series_fixture(1234);
        let detail = detail_fixture(9999);
        assert!(!is_authoritative_detail(&tracked, &detail));
    }

    #[test]
    fn detail_with_zero_id_is_not_authoritative_for_real_al_series() {
        // Some upstreams return an empty result with id=0 — explicitly
        // rejected even when tracked is a real positive-id AL series.
        let tracked = series_fixture(1234);
        let detail = detail_fixture(0);
        assert!(!is_authoritative_detail(&tracked, &detail));
    }

    // ── is_trustworthy_write ─────────────────────────────────────────

    /// Regression for the 2026-04-28 AL outage. When AL is Unavailable
    /// and Jikan succeeds, `fetch_live_detail` returns an `AnimeDetail`
    /// whose id is the negative MAL sentinel (`-mal_id`).
    /// `is_authoritative_detail` correctly rejects these (so the row's
    /// `anilist_id` column doesn't get clobbered with the negative
    /// sentinel), but the looser `is_trustworthy_write` accepts them
    /// so the periodic refresh writes the MAL data through instead of
    /// leaving the user's row pinned to whatever stale data was
    /// written before AL went down.
    #[test]
    fn negative_id_jikan_fallback_is_trustworthy_for_write() {
        let tracked = series_fixture(1234);
        // Detail from a Jikan fallback during AL outage — id = -mal_id.
        let detail = detail_fixture(-5678);
        // Strict authoritative check still rejects (the row's anilist_id
        // column should NOT be overwritten with -5678).
        assert!(!is_authoritative_detail(&tracked, &detail));
        // But the looser write gate accepts it so the data lands in
        // metadata_cache + relations + episode rows.
        assert!(
            is_trustworthy_write(&tracked, &detail),
            "MAL fallback (exact mal_id lookup, not fuzzy) must be \
             trustworthy enough to overwrite stale AL data on a refresh"
        );
    }

    // ── provider_category_for_detail ─────────────────────────────────

    #[test]
    fn provider_category_routes_negative_id_to_jikan() {
        // -mal_id sentinel from a Jikan/MAL fallback during AL outage.
        let tracked = series_fixture(1234);
        let detail = detail_fixture(-5678);
        assert_eq!(
            provider_category_for_detail(&tracked, &detail),
            LogCategory::Jikan
        );
    }

    #[test]
    fn provider_category_routes_matching_positive_id_to_anilist() {
        let tracked = series_fixture(1234);
        let detail = detail_fixture(1234);
        assert_eq!(
            provider_category_for_detail(&tracked, &detail),
            LogCategory::AniList
        );
    }

    #[test]
    fn provider_category_routes_mismatched_positive_id_to_kitsu() {
        // Kitsu's title-fuzz / by-mal-id paths stamp Kitsu's own id
        // space — positive but not equal to tracked.anilist_id.
        let tracked = series_fixture(1234);
        let detail = detail_fixture(9999);
        assert_eq!(
            provider_category_for_detail(&tracked, &detail),
            LogCategory::Kitsu
        );
    }

    #[test]
    fn provider_category_for_mal_only_series_uses_jikan() {
        // MAL-only tracked series (anilist_id <= 0) with the matching
        // negative sentinel back from Jikan still routes to Jikan.
        let tracked = series_fixture(-12345);
        let detail = detail_fixture(-12345);
        assert_eq!(
            provider_category_for_detail(&tracked, &detail),
            LogCategory::Jikan
        );
    }

    #[test]
    fn provider_category_for_zero_id_routes_to_kitsu_not_jikan() {
        // Pin the `detail.id < 0` sign-boundary at line 62. Mutating to
        // `<=` would route id=0 to Jikan instead of falling through to
        // Kitsu — wrong because id=0 isn't the negative-mal_id sentinel
        // Jikan responses use, it's an empty / sentinel-error value
        // from a provider that doesn't fit either AL or MAL shape.
        let tracked = series_fixture(1234);
        let detail = detail_fixture(0);
        assert_eq!(
            provider_category_for_detail(&tracked, &detail),
            LogCategory::Kitsu,
            "detail.id=0 must NOT route to Jikan — that's the < vs <= boundary"
        );
    }

    #[test]
    fn is_trustworthy_write_rejects_zero_id_detail() {
        // Pin the `detail.id < 0` sign-boundary at line 91. Mutating to
        // `<=` would treat id=0 as trustworthy and write zeroed-out
        // metadata over a real series row.
        let tracked = series_fixture(1234);
        let detail = detail_fixture(0);
        // Sanity: not authoritative (already pinned by an earlier test).
        assert!(!is_authoritative_detail(&tracked, &detail));
        assert!(
            !is_trustworthy_write(&tracked, &detail),
            "detail.id=0 must NOT be trustworthy — that's the < vs <= boundary"
        );
    }

    /// Kitsu's title-fuzz fallback (the multi-query path used when the
    /// series has no MAL id) returns a positive id from Kitsu's own id
    /// space. If that id differs from tracked.anilist_id, the match
    /// could be a sequel — silently overwriting the row's metadata
    /// would corrupt it. Both gates must reject this case.
    #[test]
    fn kitsu_title_fuzz_mismatched_id_is_not_trustworthy() {
        let tracked = series_fixture(1234);
        let detail = detail_fixture(9999); // positive but mismatched
        assert!(!is_authoritative_detail(&tracked, &detail));
        assert!(
            !is_trustworthy_write(&tracked, &detail),
            "positive id ≠ tracked.anilist_id is a Kitsu sequel-match \
             risk — must not overwrite the row"
        );
    }

    // ── title_candidates_for_series ──────────────────────────────────

    #[test]
    fn title_candidates_drop_empty_and_whitespace_entries() {
        let mut s = series_fixture(1);
        s.title = "Show".into();
        s.title_romaji = "".into();
        s.title_english = "   ".into(); // whitespace-only → dropped
        s.title_native = "ショウ".into();
        let candidates = title_candidates_for_series(&s);
        assert_eq!(candidates, vec!["Show".to_string(), "ショウ".to_string()]);
    }

    #[test]
    fn title_candidates_returns_empty_when_all_titles_blank() {
        // No fuzz-match seed available — caller is expected to skip
        // the title-fuzz path entirely on an empty list.
        let s = series_fixture(1);
        assert!(title_candidates_for_series(&s).is_empty());
    }

    // ── episode_needs_kitsu_backfill ─────────────────────────────────

    #[test]
    fn episode_needs_kitsu_backfill_short_circuits_for_single_episode() {
        // ep_count <= 1 → never need Kitsu (movies, OVAs, etc. don't
        // need per-episode title backfill). The closure is never even
        // consulted; we mark it `unreachable!` to prove that.
        assert!(!episode_needs_kitsu_backfill(0, |_| unreachable!()));
        assert!(!episode_needs_kitsu_backfill(1, |_| unreachable!()));
    }

    #[test]
    fn episode_needs_kitsu_backfill_false_when_jikan_has_all_titles() {
        let result = episode_needs_kitsu_backfill(12, |_n| true);
        assert!(!result);
    }

    #[test]
    fn episode_needs_kitsu_backfill_true_when_any_title_missing() {
        // Episode 7 missing a title → fall back to Kitsu.
        let result = episode_needs_kitsu_backfill(12, |n| n != 7);
        assert!(result);
    }

    #[test]
    fn episode_needs_kitsu_backfill_true_when_all_titles_missing() {
        let result = episode_needs_kitsu_backfill(5, |_| false);
        assert!(result);
    }

    // ── run_metadata_sweep / refresh_all_series_metadata ─────────────

    #[tokio::test]
    async fn refresh_all_series_metadata_returns_zero_zero_on_empty_db() {
        // No series rows → run_metadata_sweep iterates an empty list
        // and returns (0 refreshed, 0 failed). The shape of the tuple
        // is part of the handler contract for `system::api_metadata_refresh`.
        let db = crate::test_support::in_memory_pool().await;
        let (refreshed, failed) = refresh_all_series_metadata(&db).await;
        assert_eq!(refreshed, 0);
        assert_eq!(failed, 0);
    }

    #[tokio::test]
    async fn rebuild_cached_metadata_for_all_returns_triple_zero_on_empty_db() {
        // Three-tuple shape (rebuilt, skipped, failed) — kept for
        // handler contract even though the middle slot has been
        // hard-coded zero since the sweep refactor.
        let db = crate::test_support::in_memory_pool().await;
        let (rebuilt, skipped, failed) = rebuild_cached_metadata_for_all(&db).await;
        assert_eq!(rebuilt, 0);
        assert_eq!(skipped, 0);
        assert_eq!(failed, 0);
    }
}
