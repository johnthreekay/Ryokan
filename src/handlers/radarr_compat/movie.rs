//! /movie/lookup, /movie, /movie/{id}, /command — the Radarr resource
//! surface.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};

use crate::AppState;
use crate::handlers::arr_shared::LookupQuery;
use crate::models::log::LogCategory;
use crate::models::{config, monitoring, series};
use crate::services::{anibridge, anilist, logger, monitoring as monitoring_service};

use super::helpers::{
    build_radarr_movie_from_search, build_radarr_movie_from_tracked, cached_detail_for,
    lookup_by_tmdb_id,
};
use super::types::{AddMovieBody, RadarrCommandBody, RadarrMovie, UpdateMovieBody};

pub async fn movie_lookup(
    State(state): State<AppState>,
    Query(params): Query<LookupQuery>,
) -> Result<Json<Vec<RadarrMovie>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    if let Some(tmdb_id_str) = params.term.strip_prefix("tmdb:") {
        let tmdb_id: i64 = tmdb_id_str
            .trim()
            .parse()
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid TMDB ID".to_string()))?;

        return lookup_by_tmdb_id(&state, &cfg, tmdb_id).await;
    }

    // Title search fallback.
    let results = anilist::search_anime(&params.term)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    let mut movies = Vec::new();
    for r in results {
        let db_series = series::get_by_anilist_id(&state.db, r.id)
            .await
            .ok()
            .flatten();

        let tmdb_id = anibridge::resolve_tmdb_id(r.id, r.id_mal).await;
        let title = if !r.title_english.is_empty() {
            &r.title_english
        } else {
            &r.title_romaji
        };

        movies.push(
            build_radarr_movie_from_search(&r, title, tmdb_id, db_series.as_ref(), &cfg).await,
        );
    }

    Ok(Json(movies))
}

/// GET /radarr/api/v3/movie — list all tracked series as movies.
pub async fn list_movies(
    State(state): State<AppState>,
) -> Result<Json<Vec<RadarrMovie>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut results = Vec::new();
    for s in &tracked {
        let tmdb_id = anibridge::resolve_tmdb_id(s.anilist_id, s.mal_id).await;
        let detail = cached_detail_for(&state.db, s.id).await;
        results.push(build_radarr_movie_from_tracked(s, detail.as_ref(), tmdb_id, &cfg).await);
    }

    Ok(Json(results))
}

/// GET /radarr/api/v3/movie/{id}
pub async fn get_movie(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<RadarrMovie>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let s = series::get_by_id(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Movie not found".to_string()))?;

    let tmdb_id = anibridge::resolve_tmdb_id(s.anilist_id, s.mal_id).await;
    let detail = cached_detail_for(&state.db, s.id).await;
    Ok(Json(
        build_radarr_movie_from_tracked(&s, detail.as_ref(), tmdb_id, &cfg).await,
    ))
}

/// POST /radarr/api/v3/movie — add a new movie (anime).
pub async fn add_movie(
    State(state): State<AppState>,
    Json(body): Json<AddMovieBody>,
) -> Result<Json<RadarrMovie>, (StatusCode, String)> {
    let tmdb_id = body.tmdb_id.unwrap_or(0);

    anibridge::ensure_loaded().await;
    let anime_ids = anibridge::lookup_by_tmdb(tmdb_id, None).await;

    let detail = if let Some(ids) = anime_ids
        .first()
        .filter(|a| a.anilist_id.is_some() || a.mal_id.is_some())
    {
        if let Some(al_id) = ids.anilist_id {
            match anilist::get_anime_detail(al_id).await {
                Ok(d) => d,
                Err(_) if ids.mal_id.is_some() => {
                    crate::services::jikan::get_anime_detail_cached(ids.mal_id.unwrap())
                        .await
                        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
                }
                Err(e) => return Err((StatusCode::BAD_GATEWAY, e)),
            }
        } else if let Some(mal_id) = ids.mal_id {
            crate::services::jikan::get_anime_detail_cached(mal_id)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
        } else {
            return Err((
                StatusCode::BAD_GATEWAY,
                "No AniList or MAL ID available for TMDB mapping".to_string(),
            ));
        }
    } else {
        // No anibridge mapping — fall back to AniList title search.
        let search_title = body.title.as_deref().unwrap_or("");
        if search_title.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("No mapping for TMDB ID {} and no title provided", tmdb_id),
            ));
        }
        tracing::info!(
            "No anibridge mapping for TMDB {}; searching AniList for '{}'",
            tmdb_id,
            search_title
        );

        let results = anilist::search_anime(search_title)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

        let best = results.first().ok_or((
            StatusCode::NOT_FOUND,
            format!("No AniList results for '{}'", search_title),
        ))?;

        anilist::get_anime_detail(best.id)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
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

    let monitor_mode = if body.monitored.unwrap_or(true) {
        monitoring::MonitorMode::All
    } else {
        monitoring::MonitorMode::None
    };
    let _ = monitoring_service::apply_monitor_mode(&state.db, id, monitor_mode).await;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Added via Seerr (Radarr): {}", title),
        &format!("tmdb_id={}, provider_id={}, id={}", tmdb_id, detail.id, id),
    )
    .await;

    // Auto-search if requested.
    if body
        .add_options
        .as_ref()
        .and_then(|o| o.search_for_movie)
        .unwrap_or(false)
    {
        super::super::library::search::queue_shim_search(
            state.clone(),
            id,
            std::time::Duration::from_secs(2),
        );
    }

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    let s = series::get_by_id(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Movie not found after insert".to_string(),
        ))?;

    let detail = cached_detail_for(&state.db, s.id).await;
    Ok(Json(
        build_radarr_movie_from_tracked(&s, detail.as_ref(), tmdb_id, &cfg).await,
    ))
}

/// PUT /radarr/api/v3/movie — update an existing movie.
pub async fn update_movie(
    State(state): State<AppState>,
    Json(body): Json<UpdateMovieBody>,
) -> Result<Json<RadarrMovie>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let s = series::get_by_id(&state.db, body.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Movie not found".to_string()))?;

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
        &format!("Updated via Seerr (Radarr): {}", s.title),
        &format!("id={}, monitored={:?}", s.id, body.monitored),
    )
    .await;

    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    let tmdb_id = anibridge::resolve_tmdb_id(s.anilist_id, s.mal_id).await;
    let detail = cached_detail_for(&state.db, s.id).await;
    Ok(Json(
        build_radarr_movie_from_tracked(&s, detail.as_ref(), tmdb_id, &cfg).await,
    ))
}

/// POST /radarr/api/v3/command — execute a command. Seerr sends MoviesSearch.
pub async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<RadarrCommandBody>,
) -> Json<serde_json::Value> {
    let name = body.name.unwrap_or_default();

    if name == "MoviesSearch"
        && let Some(movie_ids) = body.movie_ids
    {
        for movie_id in movie_ids {
            super::super::library::search::queue_shim_search(
                state.clone(),
                movie_id,
                std::time::Duration::ZERO,
            );
        }
    }

    Json(serde_json::json!({
        "id": 1,
        "name": name,
        "commandName": name,
        "status": "queued",
        "queued": chrono::Utc::now().to_rfc3339(),
    }))
}
