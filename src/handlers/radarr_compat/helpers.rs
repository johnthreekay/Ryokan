//! Construction helpers for RadarrMovie payloads.

use axum::{Json, http::StatusCode};

use crate::AppState;
use crate::models::{config, metadata_cache, monitoring, series};
use crate::services::{anibridge, anilist, media};

use super::types::{RadarrImage, RadarrMovie, RadarrRatingValue, RadarrRatings};

// ── Helpers ────────────────────────────────────────────────────────────────

/// AL 0-100 / Jikan-x10 → Radarr's nested `RadarrRatings`. Radarr's
/// shape carries an `imdb` and a `tmdb` slot, both with the same
/// 0-10 scale Sonarr's flat shape uses; we populate both with the
/// same value so Seerr renders a rating regardless of which slot it
/// reads. `None` → zeroed; matches Sonarr's `ratings_from_score`.
pub(super) fn ratings_from_score(score: Option<i32>) -> RadarrRatings {
    let value = match score {
        Some(s) if s > 0 => f64::from(s) / 10.0,
        _ => 0.0,
    };
    RadarrRatings {
        imdb: RadarrRatingValue {
            votes: 0,
            value,
            rating_type: "user".to_string(),
        },
        tmdb: RadarrRatingValue {
            votes: 0,
            value,
            rating_type: "user".to_string(),
        },
    }
}

/// Mirror of `sonarr_compat::cached_detail_for`. Pulls the cached
/// `AnimeDetail` for a tracked series so callers that need the
/// average_score (or other metadata) can recover it without
/// duplicating the cache-miss handling at every call site.
pub(super) async fn cached_detail_for(
    db: &sqlx::SqlitePool,
    series_id: i64,
) -> Option<anilist::AnimeDetail> {
    metadata_cache::get_by_series_id(db, series_id)
        .await
        .ok()
        .flatten()
        .map(|c| c.detail)
}

pub(super) async fn lookup_by_tmdb_id(
    state: &AppState,
    cfg: &config::Config,
    tmdb_id: i64,
) -> Result<Json<Vec<RadarrMovie>>, (StatusCode, String)> {
    anibridge::ensure_loaded().await;
    let anime_ids = anibridge::lookup_by_tmdb_movie(tmdb_id).await;

    if anime_ids.is_empty() {
        tracing::warn!(
            "No anibridge mapping for TMDB ID {}; returning stub movie for Seerr",
            tmdb_id
        );
        return Ok(Json(vec![build_stub_movie(tmdb_id, cfg)]));
    }

    // Pre-batch every AniList id into one `Page(media(id_in:[]))` call
    // so the per-entry loop hits DETAIL_CACHE instead of issuing N
    // sequential GraphQL queries. A 7-entry TMDB fan-out becomes 1
    // round-trip + 7 cache hits instead of 7 throttled-serial round-trips.
    let prefetch_ids: Vec<i64> = anime_ids
        .iter()
        .filter_map(|ids| ids.anilist_id.filter(|id| *id > 0))
        .collect();
    if !prefetch_ids.is_empty()
        && let Err(e) = anilist::get_anime_details_batch(&prefetch_ids).await
    {
        tracing::debug!("Radarr fan-out: AL batch prefetch failed (per-id loop will retry): {e}");
    }

    // Same shape as the AL prefetch: one batched DB read keyed on every
    // AL id we'll look up below. Without this the per-id loop hits SQLite
    // N times for what is structurally a single `IN (…)` query.
    let db_by_id = series::get_by_anilist_ids(&state.db, &prefetch_ids)
        .await
        .unwrap_or_default();

    let mut results = Vec::new();
    for ids in &anime_ids {
        let detail = if let Some(al_id) = ids.anilist_id {
            match anilist::get_anime_detail(al_id).await {
                Ok(d) => d,
                Err(_) if ids.mal_id.is_some() => {
                    match crate::services::jikan::get_anime_detail_cached(ids.mal_id.unwrap()).await
                    {
                        Ok(d) => d,
                        Err(_) => continue,
                    }
                }
                Err(_) => continue,
            }
        } else if let Some(mal_id) = ids.mal_id {
            match crate::services::jikan::get_anime_detail_cached(mal_id).await {
                Ok(d) => d,
                Err(_) => continue,
            }
        } else {
            continue;
        };

        let db_series = if detail.id > 0 {
            db_by_id.get(&detail.id).cloned()
        } else {
            None
        };

        let title = if !detail.title_english.is_empty() {
            &detail.title_english
        } else {
            &detail.title_romaji
        };

        let search_result = anilist::AnimeEntry {
            id: detail.id,
            id_mal: detail.id_mal,
            title_romaji: detail.title_romaji.clone(),
            title_english: detail.title_english.clone(),
            title_native: detail.title_native.clone(),
            cover_url: detail.cover_url.clone(),
            format: detail.format.clone(),
            status: detail.status.clone(),
            status_display: String::new(),
            episodes: detail.episodes,
            season_year: detail.season_year,
            source: if detail.id > 0 { "anilist" } else { "mal" }.to_string(),
            average_score: detail.average_score,
        };

        results.push(
            build_radarr_movie_from_search(&search_result, title, tmdb_id, db_series.as_ref(), cfg)
                .await,
        );
    }

    Ok(Json(results))
}

pub(super) async fn build_radarr_movie_from_search(
    r: &anilist::AnimeEntry,
    title: &str,
    tmdb_id: i64,
    db_series: Option<&series::Series>,
    cfg: &config::Config,
) -> RadarrMovie {
    let is_in_library = db_series.is_some();
    let internal_id = db_series.map(|s| s.id).unwrap_or(0);

    let folder_name = db_series.map(|s| s.folder_name.clone()).unwrap_or_else(|| {
        crate::services::naming::series_folder(
            &cfg.series_folder_format,
            &cfg.title_language,
            &crate::services::naming::SeriesNames {
                title,
                ..crate::services::naming::SeriesNames::from_entry(r)
            },
        )
    });

    let has_file = if is_in_library {
        !media::scan_series_folder(&cfg.media_root, &folder_name)
            .await
            .is_empty()
    } else {
        false
    };

    let path = if cfg.media_root.is_empty() {
        format!("/media/{}", folder_name)
    } else {
        format!("{}/{}", cfg.media_root, folder_name)
    };

    let monitored = db_series
        .map(|s| s.monitor_mode_enum() != monitoring::MonitorMode::None)
        .unwrap_or(true);

    RadarrMovie {
        id: internal_id,
        title: title.to_string(),
        sort_title: title.to_lowercase(),
        status: map_status(&r.status),
        overview: String::new(),
        images: vec![RadarrImage {
            cover_type: "poster".to_string(),
            url: r.cover_url.clone(),
            remote_url: r.cover_url.clone(),
        }],
        remote_poster: r.cover_url.clone(),
        year: r.season_year.unwrap_or(0),
        path,
        quality_profile_id: 1,
        monitored,
        minimum_availability: "released".to_string(),
        runtime: 24,
        tmdb_id,
        imdb_id: String::new(),
        title_slug: format!("ryokan-{}", r.id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: if is_in_library {
            "2024-01-01T00:00:00Z".to_string()
        } else {
            String::new()
        },
        ratings: ratings_from_score(r.average_score),
        has_file,
        is_available: true,
        folder_name: folder_name.clone(),
        clean_title: title
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), ""),
        root_folder_path: cfg.media_root.clone(),
        movie_file: None,
    }
}

pub(super) async fn build_radarr_movie_from_tracked(
    s: &series::Series,
    detail: Option<&anilist::AnimeDetail>,
    tmdb_id: i64,
    cfg: &config::Config,
) -> RadarrMovie {
    let disk_files = media::scan_series_folder(&cfg.media_root, &s.folder_name).await;
    let has_file = !disk_files.is_empty();
    let monitored = s.monitor_mode_enum() != monitoring::MonitorMode::None;

    let path = if cfg.media_root.is_empty() {
        format!("/media/{}", s.folder_name)
    } else {
        format!("{}/{}", cfg.media_root, s.folder_name)
    };

    let title = if !s.title.is_empty() {
        &s.title
    } else {
        &s.title_romaji
    };

    RadarrMovie {
        id: s.id,
        title: title.to_string(),
        sort_title: title.to_lowercase(),
        status: map_status(&s.status),
        overview: String::new(),
        images: vec![RadarrImage {
            cover_type: "poster".to_string(),
            url: s.cover_url.clone(),
            remote_url: s.cover_url.clone(),
        }],
        remote_poster: s.cover_url.clone(),
        year: s.season_year.unwrap_or(0),
        path,
        quality_profile_id: 1,
        monitored,
        minimum_availability: "released".to_string(),
        runtime: 24,
        tmdb_id,
        imdb_id: String::new(),
        title_slug: format!("ryokan-{}", s.anilist_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: "2024-01-01T00:00:00Z".to_string(),
        ratings: ratings_from_score(detail.and_then(|d| d.average_score)),
        has_file,
        is_available: true,
        folder_name: s.folder_name.clone(),
        clean_title: title
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), ""),
        root_folder_path: cfg.media_root.clone(),
        movie_file: None,
    }
}

pub(super) fn map_status(anilist_status: &str) -> String {
    match anilist_status.to_uppercase().as_str() {
        "RELEASING" | "NOT_YET_RELEASED" => "announced".to_string(),
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED" => "released".to_string(),
        _ => "released".to_string(),
    }
}

pub(super) fn build_stub_movie(tmdb_id: i64, cfg: &config::Config) -> RadarrMovie {
    let path = if cfg.media_root.is_empty() {
        "/media/Unknown".to_string()
    } else {
        format!("{}/Unknown", cfg.media_root)
    };

    RadarrMovie {
        id: 0,
        title: format!("TMDB:{}", tmdb_id),
        sort_title: format!("tmdb:{}", tmdb_id),
        status: "released".to_string(),
        overview: String::new(),
        images: vec![],
        remote_poster: String::new(),
        year: 0,
        path,
        quality_profile_id: 1,
        monitored: true,
        minimum_availability: "released".to_string(),
        runtime: 0,
        tmdb_id,
        imdb_id: String::new(),
        title_slug: format!("tmdb-{}", tmdb_id),
        certification: String::new(),
        genres: vec!["Anime".to_string()],
        tags: vec![],
        added: String::new(),
        ratings: ratings_from_score(None),
        has_file: false,
        is_available: true,
        folder_name: String::new(),
        clean_title: String::new(),
        root_folder_path: cfg.media_root.clone(),
        movie_file: None,
    }
}
