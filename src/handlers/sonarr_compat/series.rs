//! /series/lookup, /series, /series/{id}, /command — the real Sonarr
//! resource surface.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde_json;

use crate::AppState;
use crate::handlers::arr_shared::LookupQuery;
use crate::models::log::LogCategory;
use crate::models::{config, monitoring, series};
use crate::services::{anibridge, anilist, logger, monitoring as monitoring_service};

use super::helpers::{
    build_sonarr_series_from_search, build_sonarr_series_from_tracked, cached_detail_for,
    lookup_by_external_id, shim_ids,
};
use super::types::{AddSeriesBody, CommandBody, SonarrSeries, UpdateSeriesBody};

pub async fn series_lookup(
    State(state): State<AppState>,
    Query(params): Query<LookupQuery>,
) -> Result<Json<Vec<SonarrSeries>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    if let Some(tvdb_id_str) = params.term.strip_prefix("tvdb:") {
        // TVDB ID lookup through the anibridge TVDB index.
        let tvdb_id: i64 = tvdb_id_str
            .trim()
            .parse()
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid ID".to_string()))?;

        return lookup_by_external_id(&state, &cfg, tvdb_id).await;
    }

    // Title search — search AniList and return results in Sonarr format.
    let results = anilist::search_anime(&params.term)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    // One batched DB read keyed on every AL id we got back, instead of
    // N sequential round-trips per result. Seerr polls this endpoint
    // every connection-test and every search submission.
    let result_ids: Vec<i64> = results.iter().map(|r| r.id).collect();
    let db_by_id = series::get_by_anilist_ids(&state.db, &result_ids)
        .await
        .unwrap_or_default();

    let mut sonarr_results = Vec::new();
    for r in results {
        let db_series = db_by_id.get(&r.id);

        let stored = match db_series {
            Some(s) => series::tvdb_ids(&state.db, s.id).await.ok().flatten(),
            None => None,
        };
        let ids = shim_ids(stored, r.id, r.id_mal).await;
        let title = if !r.title_english.is_empty() {
            &r.title_english
        } else {
            &r.title_romaji
        };

        sonarr_results.push(build_sonarr_series_from_search(&r, title, ids, db_series, &cfg).await);
    }

    Ok(Json(sonarr_results))
}

/// GET /api/v3/series — list all tracked series.
/// `GET /api/v3/series` query. Sonarr filters by `tvdbId`; Seerr asks
/// with it before resetting a request whose series it didn't see in a
/// scan, so a list that ignored it read as "not in Sonarr".
#[derive(Debug, Default, serde::Deserialize)]
pub struct SeriesListQuery {
    #[serde(rename = "tvdbId")]
    pub tvdb_id: Option<i64>,
}

pub async fn list_series(
    State(state): State<AppState>,
    Query(query): Query<SeriesListQuery>,
) -> Result<Json<Vec<SonarrSeries>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let stored = series::all_tvdb_ids(&state.db).await.unwrap_or_default();
    let mut results = Vec::new();
    for s in &tracked {
        let ids = shim_ids(stored.get(&s.id).copied(), s.anilist_id, s.mal_id).await;
        if query.tvdb_id.is_some_and(|want| want != ids.tvdb_id) {
            continue;
        }
        let detail = cached_detail_for(&state.db, s.id).await;
        results.push(build_sonarr_series_from_tracked(s, detail.as_ref(), ids, &cfg).await);
    }

    Ok(Json(results))
}

/// GET /api/v3/series/{id} — get a single tracked series by Ryokan's internal ID.
pub async fn get_series(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<SonarrSeries>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let s = series::get_by_id(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Series not found".to_string()))?;

    let stored = series::tvdb_ids(&state.db, s.id).await.ok().flatten();
    let ids = shim_ids(stored, s.anilist_id, s.mal_id).await;
    let detail = cached_detail_for(&state.db, s.id).await;
    Ok(Json(
        build_sonarr_series_from_tracked(&s, detail.as_ref(), ids, &cfg).await,
    ))
}

/// POST /api/v3/series — add a new series.
///
/// Seerr sends tvdbId which is a real TVDB ID. We map it to AniList/MAL via
/// anibridge, add the series to Ryokan's library, and return a Sonarr-format response.
pub async fn add_series(
    State(state): State<AppState>,
    Json(body): Json<AddSeriesBody>,
) -> Result<Json<SonarrSeries>, (StatusCode, String)> {
    let tvdb_id = body.tvdb_id.unwrap_or(0);

    // Extract which season Seerr is requesting. Seerr marks exactly one season as
    // monitored per request, so .max() effectively picks the single monitored season.
    let requested_season = body.seasons.as_ref().and_then(|seasons| {
        seasons
            .iter()
            .filter(|s| s.monitored && s.season_number > 0)
            .map(|s| s.season_number)
            .max()
    });

    tracing::info!(
        "Seerr add_series: tvdb_id={}, title={:?}, requested_season={:?}, seasons={:?}",
        tvdb_id,
        body.title,
        requested_season,
        body.seasons,
    );

    // Resolve TVDB + season → AniList/MAL IDs via anibridge.
    anibridge::ensure_loaded().await;
    // No TMDB fallback: Seerr sends real TVDB ids, and one that equals
    // some anime's TMDB id would add the wrong show. An unmapped id goes
    // to the title search below.
    let mut anime_ids = anibridge::lookup_by_tvdb(tvdb_id, requested_season).await;

    // #26 — TMDB often models multi-cour anime as one flat season
    // (JJK, Bleach TYBW, Demon Slayer) so Seerr requests "season 1"
    // even when it covers 2–3 AL entries. Sort by AL ID ascending so
    // the earliest-aired cour is the "primary" we return to Seerr —
    // AL IDs are assigned chronologically at entry-creation time, so
    // this picks the right cour as the face of the request even though
    // all siblings get added below.
    anime_ids.sort_by_key(|a| a.anilist_id.unwrap_or(i64::MAX));

    tracing::info!(
        "Anibridge resolved TVDB {} season {:?} → {} entries: {:?}",
        tvdb_id,
        requested_season,
        anime_ids.len(),
        anime_ids,
    );

    // #26 — Squashed-merge detection. >1 AL entry means TMDB collapsed
    // multiple cours into a single season; we fan out one Ryokan series
    // per AL entry so a single Seerr request seeds all the relevant
    // cours at once. Single-entry adds skip the fan-out path and behave
    // exactly as before.
    let is_squashed_merge = anime_ids.len() > 1;

    // Squashed-merge regrab shortcut: if every AL entry the anibridge
    // mapping returned is already present in Ryokan's DB, skip all
    // AL/Jikan detail fetches — the user has already added this
    // franchise once, Seerr is just re-asking. We still re-apply
    // monitoring and re-trigger auto-search below so a regrab from the
    // Seerr UI actually does something.
    let existing_siblings: Vec<Option<series::Series>> = {
        let mut v = Vec::with_capacity(anime_ids.len());
        for ids in &anime_ids {
            let row = if let Some(al) = ids.anilist_id {
                series::get_by_anilist_id(&state.db, al)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            } else if let Some(mal) = ids.mal_id {
                series::get_by_mal_id(&state.db, mal)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            } else {
                None
            };
            v.push(row);
        }
        v
    };
    let all_exist = !anime_ids.is_empty() && existing_siblings.iter().all(|o| o.is_some());

    // Gather the series rows we ultimately apply monitoring + auto-search
    // to. For the regrab path this is populated from `existing_siblings`
    // directly. For the fresh (or partial-fresh) path each missing entry
    // takes the AL/Jikan fetch + upsert branch below.
    let mut processed: Vec<series::Series> = Vec::new();
    // Used purely for the "Added via Seerr: <title>" log line so a regrab
    // doesn't claim to have "added" anything.
    let mut newly_added = 0_usize;

    if all_exist {
        for row in existing_siblings.iter().flatten() {
            processed.push(row.clone());
        }
        if is_squashed_merge {
            logger::info(
                &state.db,
                LogCategory::Library,
                &format!(
                    "Seerr regrab: TVDB {} season {:?} already mapped to {} existing series — skipping AL fetch",
                    tvdb_id, requested_season, processed.len()
                ),
                &format!("series_ids={:?}", processed.iter().map(|s| s.id).collect::<Vec<_>>()),
            ).await;
        }
    } else if !anime_ids.is_empty()
        && anime_ids
            .iter()
            .any(|a| a.anilist_id.is_some() || a.mal_id.is_some())
    {
        if is_squashed_merge {
            let al_ids: Vec<i64> = anime_ids.iter().filter_map(|a| a.anilist_id).collect();
            logger::info(
                &state.db,
                LogCategory::Library,
                &format!(
                    "Seerr add: TVDB {} season {:?} maps to {} AniList entries — fanning out",
                    tvdb_id,
                    requested_season,
                    anime_ids.len()
                ),
                &format!("al_ids_sorted={:?}", al_ids),
            )
            .await;
        }

        // Pre-batch every still-missing AniList id into one
        // `Page(media(id_in:[]))` call so the per-entry loop below
        // hits DETAIL_CACHE instead of issuing N sequential GraphQL
        // queries. A 7-entry fan-out becomes 1 round-trip + 7 cache
        // hits instead of 7 throttled-serial round-trips.
        let prefetch_ids: Vec<i64> = anime_ids
            .iter()
            .zip(existing_siblings.iter())
            .filter_map(|(ids, existing)| {
                if existing.is_some() {
                    None
                } else {
                    ids.anilist_id.filter(|id| *id > 0)
                }
            })
            .collect();
        if !prefetch_ids.is_empty()
            && let Err(e) = anilist::get_anime_details_batch(&prefetch_ids).await
        {
            tracing::debug!("Seerr add: AL batch prefetch failed (per-id loop will retry): {e}");
        }

        for (ids, existing) in anime_ids.iter().zip(existing_siblings) {
            // Skip entries that already exist — regrab-within-fresh case.
            // A partial fan-out (user added JJK S1 manually last month, now
            // Seerr adds JJK) shouldn't re-fetch S1's detail.
            if let Some(row) = existing {
                processed.push(row);
                continue;
            }
            if ids.anilist_id.is_none() && ids.mal_id.is_none() {
                continue;
            }

            let detail_result = if let Some(al_id) = ids.anilist_id {
                match anilist::get_anime_detail(al_id).await {
                    Ok(d) => Ok(d),
                    Err(_) if ids.mal_id.is_some() => {
                        crate::services::jikan::get_anime_detail_cached(ids.mal_id.unwrap()).await
                    }
                    Err(e) => Err(e),
                }
            } else if let Some(mal_id) = ids.mal_id {
                crate::services::jikan::get_anime_detail_cached(mal_id).await
            } else {
                unreachable!("checked above");
            };

            let detail = match detail_result {
                Ok(d) => d,
                Err(e) => {
                    // In a squashed fan-out, a single sibling's detail
                    // fetch failing shouldn't nuke the whole add — log
                    // it and keep going. For a single-entry add this
                    // leaves `processed` empty and we fall through to
                    // the error return below.
                    logger::warn(
                        &state.db,
                        LogCategory::Library,
                        &format!(
                            "Seerr add: skipped AL entry {:?} / MAL {:?} after detail fetch failure",
                            ids.anilist_id, ids.mal_id
                        ),
                        &e,
                    ).await;
                    continue;
                }
            };

            let title = if !detail.title_english.is_empty() {
                &detail.title_english
            } else {
                &detail.title_romaji
            };
            let (id, _created) = series::upsert(
                &state.db,
                series::SeriesCore {
                    anilist_id: detail.id,
                    mal_id: detail.id_mal,
                    title,
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
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            let s = series::get_by_id(&state.db, id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .ok_or((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Series not found after insert".to_string(),
                ))?;
            processed.push(s);
            newly_added += 1;
        }
    } else {
        // No anibridge mapping — fall back to AniList title search.
        // Single-path only: we have nothing to fan out over.
        let search_title = body.title.as_deref().unwrap_or("");
        if search_title.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("No mapping for TVDB ID {} and no title provided", tvdb_id),
            ));
        }
        tracing::info!(
            "No anibridge mapping for TVDB {}; searching AniList for '{}'",
            tvdb_id,
            search_title
        );

        let results = anilist::search_anime(search_title)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

        let best = results.first().ok_or((
            StatusCode::NOT_FOUND,
            format!("No AniList results for '{}'", search_title),
        ))?;

        let detail = anilist::get_anime_detail(best.id)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

        let title = if !detail.title_english.is_empty() {
            &detail.title_english
        } else {
            &detail.title_romaji
        };
        let (id, _created) = series::upsert(
            &state.db,
            series::SeriesCore {
                anilist_id: detail.id,
                mal_id: detail.id_mal,
                title,
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let s = series::get_by_id(&state.db, id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Series not found after insert".to_string(),
            ))?;
        processed.push(s);
        newly_added += 1;
    }

    if processed.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("No usable AniList/MAL mapping for TVDB ID {}", tvdb_id),
        ));
    }

    // Remember the TVDB show Seerr knows these series by, so the shim
    // reports it even for a show the mappings don't have (the title
    // search above). The season is the one requested, else the
    // mappings' season for the entry (0 for Specials), else 1.
    if tvdb_id > 0 {
        for s in &processed {
            let season = match requested_season {
                Some(season) => season,
                None => anibridge::resolve_tvdb(s.anilist_id, s.mal_id)
                    .await
                    .filter(|(mapped, _)| *mapped == tvdb_id)
                    .map(|(_, season)| season)
                    .unwrap_or(1),
            };
            let _ = series::set_tvdb_ids(&state.db, s.id, tvdb_id, season).await;
        }
    }

    // Set monitoring based on what Seerr requested. Applied to every
    // sibling in a fan-out — Seerr's "monitor this season" intent covers
    // the whole squashed merge, so each fanned-out cour inherits it.
    let should_monitor = if let Some(ref seasons) = body.seasons {
        if let Some(req_s) = requested_season {
            seasons
                .iter()
                .any(|s| s.season_number == req_s && s.monitored)
        } else {
            body.monitored.unwrap_or(true)
        }
    } else {
        body.monitored.unwrap_or(true)
    };
    let monitor_mode = if should_monitor {
        monitoring::MonitorMode::All
    } else {
        monitoring::MonitorMode::None
    };
    for s in &processed {
        let _ = monitoring_service::apply_monitor_mode(&state.db, s.id, monitor_mode).await;
    }

    if newly_added > 0 {
        let primary_title = &processed[0].title;
        let added_ids: Vec<i64> = processed.iter().map(|s| s.id).collect();
        logger::info(
            &state.db,
            LogCategory::Library,
            &format!(
                "Added via Seerr: {}{}",
                primary_title,
                if is_squashed_merge {
                    format!(
                        " (+{} sibling cour{})",
                        processed.len().saturating_sub(1),
                        if processed.len() == 2 { "" } else { "s" }
                    )
                } else {
                    String::new()
                }
            ),
            &format!(
                "tvdb_id={}, series_ids={:?}, newly_added={}",
                tvdb_id, added_ids, newly_added
            ),
        )
        .await;
    }

    // Auto-search if requested. Each sibling in a fan-out gets its own
    // spawned auto-search — the grab-time hydration hook inside
    // `auto_search_series` will lazily walk each series' PREQUEL chain
    // and set `cumulative_prior_episodes` on first run so absolute-
    // numbered releases route to the right cour.
    if body
        .add_options
        .as_ref()
        .and_then(|o| o.search_for_missing_episodes)
        .unwrap_or(false)
    {
        for s in &processed {
            super::super::library::search::queue_shim_search(
                state.clone(),
                s.id,
                std::time::Duration::from_secs(2),
            );
        }
    }

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    // Return the primary (earliest AL ID) as the Sonarr payload. Seerr
    // keys off tvdb_id, not the Ryokan series id, so a single response
    // representing the whole request is what it expects — the sibling
    // cours exist in Ryokan's DB but don't need to be reflected in the
    // Sonarr response shape.
    let primary = &processed[0];
    let detail = cached_detail_for(&state.db, primary.id).await;
    let stored = series::tvdb_ids(&state.db, primary.id).await.ok().flatten();
    let ids = shim_ids(stored, primary.anilist_id, primary.mal_id).await;
    Ok(Json(
        build_sonarr_series_from_tracked(primary, detail.as_ref(), ids, &cfg).await,
    ))
}

/// PUT /api/v3/series — update an existing series.
pub async fn update_series(
    State(state): State<AppState>,
    Json(body): Json<UpdateSeriesBody>,
) -> Result<Json<SonarrSeries>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let s = series::get_by_id(&state.db, body.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Series not found".to_string()))?;

    // Update monitoring.
    if let Some(monitored) = body.monitored {
        let mode = if monitored {
            monitoring::MonitorMode::All
        } else {
            monitoring::MonitorMode::None
        };
        let _ = monitoring_service::apply_monitor_mode(&state.db, s.id, mode).await;
    }

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Updated via Seerr: {}", s.title),
        &format!("id={}, monitored={:?}", s.id, body.monitored),
    )
    .await;

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    let stored = series::tvdb_ids(&state.db, s.id).await.ok().flatten();
    let ids = shim_ids(stored, s.anilist_id, s.mal_id).await;
    let detail = cached_detail_for(&state.db, s.id).await;
    Ok(Json(
        build_sonarr_series_from_tracked(&s, detail.as_ref(), ids, &cfg).await,
    ))
}

/// POST /api/v3/command — execute a command. Seerr sends SeriesSearch.
pub async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<CommandBody>,
) -> Json<serde_json::Value> {
    let name = body.name.unwrap_or_default();

    if name == "SeriesSearch"
        && let Some(series_id) = body.series_id
    {
        super::super::library::search::queue_shim_search(
            state.clone(),
            series_id,
            std::time::Duration::ZERO,
        );
    }

    Json(serde_json::json!({
        "id": 1,
        "name": name,
        "commandName": name,
        "status": "queued",
        "queued": chrono::Utc::now().to_rfc3339(),
    }))
}
