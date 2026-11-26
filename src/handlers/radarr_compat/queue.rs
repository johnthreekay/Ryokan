//! `GET /radarr/api/v3/queue` (#325): what is downloading, in Radarr's
//! shape, for Seerr's download-sync job. The join against the download
//! clients and the reasons behind it are in `handlers::arr_queue`.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Serialize;

use crate::AppState;
use crate::handlers::arr_queue::{self, QueueBase, QueuePage, QueueQuery};
use crate::models::series;
use crate::services::{anibridge, datetime};

/// One download. `movieId` is the library row's id, as `list_movies`
/// reports every tracked series.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrQueueRecord {
    pub movie_id: i64,
    #[serde(flatten)]
    pub base: QueueBase,
    pub id: i64,
}

/// GET /radarr/api/v3/queue
pub async fn queue(
    State(state): State<AppState>,
    Query(query): Query<QueueQuery>,
) -> Result<Json<QueuePage<RadarrQueueRecord>>, (StatusCode, String)> {
    let entries = arr_queue::load_entries(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if entries.is_empty() {
        return Ok(Json(query.page(Vec::new())));
    }

    // Rows that are one TMDB movie share a download, for the reason
    // rows of one TVDB show do on the Sonarr side. 0 is "no film
    // mapping", which shares with nothing.
    anibridge::ensure_loaded().await;
    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut movie_by_row = BTreeMap::new();
    for s in &tracked {
        let movie = anibridge::resolve_tmdb_movie_id(s.anilist_id, s.mal_id).await;
        movie_by_row.insert(s.id, movie);
    }

    let now = datetime::now();
    let mut records = Vec::new();
    for entry in &entries {
        if !movie_by_row.contains_key(&entry.grab.series_id) {
            continue;
        }
        let base = entry.base(now);
        for row in arr_queue::rows_sharing_show(entry.grab.series_id, &movie_by_row) {
            records.push(RadarrQueueRecord {
                movie_id: row,
                id: arr_queue::record_id(&base.download_id, row, 0),
                base: base.clone(),
            });
        }
    }

    Ok(Json(query.page(records)))
}
