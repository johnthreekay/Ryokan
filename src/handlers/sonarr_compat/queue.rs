//! `GET /api/v3/queue` (#325): what is downloading, in Sonarr's shape,
//! for Seerr's download-sync job. The join against the download clients
//! and the reasons behind it are in `handlers::arr_queue`.

use std::collections::{BTreeMap, HashMap};

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Serialize;

use crate::AppState;
use crate::handlers::arr_queue::{self, QueueBase, QueuePage, QueueQuery};
use crate::models::series;
use crate::services::anibridge::{self, TvdbPlacement};
use crate::services::datetime;

use super::helpers::{ShimIds, shim_ids};

/// One episode of one download. A pack is one record per episode, all
/// sharing a `downloadId`, which Seerr folds into "Season N".
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SonarrQueueRecord {
    pub series_id: i64,
    pub episode_id: i64,
    pub season_number: i32,
    /// Sonarr's `includeEpisode`; absent for a grab that names no
    /// episode (a pick of specials only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<SonarrQueueEpisode>,
    #[serde(flatten)]
    pub base: QueueBase,
    pub id: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SonarrQueueEpisode {
    pub id: i64,
    pub series_id: i64,
    pub season_number: i32,
    pub episode_number: i32,
    pub absolute_episode_number: i32,
}

/// The TVDB season and episode of the entry's own episode `own`.
///
/// An AniList entry is not always one TVDB season. A second cour often
/// starts at `s01e13` (offset 12), and one long entry can be several
/// seasons: Gintama is seasons 1 to 4 of its TVDB show, with offsets 0,
/// -49, -99 and -150. Each placement turns `own` into a number in its
/// season, and the season the episode is in is the one where that
/// number is positive and smallest. With no usable placement on the
/// row's show it is the season the row is listed under, numbered as the
/// entry numbers it.
fn tvdb_slot(own: i32, ids: &ShimIds, placements: &[TvdbPlacement]) -> (i32, i32) {
    placements
        .iter()
        .filter(|p| p.tvdb_id == ids.tvdb_id && p.season > 0)
        .filter_map(|p| Some((p.season, own + p.episode_offset?)))
        .filter(|(_, number)| *number > 0)
        .min_by_key(|(_, number)| *number)
        .unwrap_or((ids.tvdb_season, own))
}

/// GET /api/v3/queue
pub async fn queue(
    State(state): State<AppState>,
    Query(query): Query<QueueQuery>,
) -> Result<Json<QueuePage<SonarrQueueRecord>>, (StatusCode, String)> {
    let entries = arr_queue::load_entries(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if entries.is_empty() {
        return Ok(Json(query.page(Vec::new())));
    }

    // Every row's ids exactly as `list_series` reports them, so a
    // record's `seriesId` is an id Seerr has seen. Without the mappings
    // a row with no stored TVDB id shares with nothing and its download
    // is still listed under its own id.
    anibridge::ensure_loaded().await;
    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stored = series::all_tvdb_ids(&state.db).await.unwrap_or_default();
    let mut rows = HashMap::new();
    let mut show_by_row = BTreeMap::new();
    for s in &tracked {
        let ids = shim_ids(stored.get(&s.id).copied(), s.anilist_id, s.mal_id).await;
        show_by_row.insert(s.id, ids.tvdb_id);
        rows.insert(s.id, (s, ids));
    }

    let now = datetime::now();
    let with_episode = query.include_episode();
    let mut records = Vec::new();
    for entry in &entries {
        let Some((series, ids)) = rows.get(&entry.grab.series_id) else {
            continue;
        };
        let base = entry.base(now);
        let placements = anibridge::tvdb_placements(series.anilist_id).await;
        // The same episodes under every row of the show: the season and
        // episode say what is downloading, the `seriesId` only which of
        // Ryokan's entries Seerr happens to hold for the show.
        for row in arr_queue::rows_sharing_show(series.id, &show_by_row) {
            if entry.grab.episode_numbers.is_empty() {
                records.push(SonarrQueueRecord {
                    series_id: row,
                    episode_id: 0,
                    season_number: ids.tvdb_season,
                    episode: None,
                    id: arr_queue::record_id(&base.download_id, row, 0),
                    base: base.clone(),
                });
            }
            for &own in &entry.grab.episode_numbers {
                let (season, number) = tvdb_slot(own, ids, &placements);
                let episode_id = arr_queue::record_id(&format!("episode-s{season}"), row, number);
                records.push(SonarrQueueRecord {
                    series_id: row,
                    episode_id,
                    season_number: season,
                    episode: with_episode.then_some(SonarrQueueEpisode {
                        id: episode_id,
                        series_id: row,
                        season_number: season,
                        episode_number: number,
                        absolute_episode_number: own + series.cumulative_prior_episodes.max(0),
                    }),
                    id: arr_queue::record_id(
                        &base.download_id,
                        row,
                        season.saturating_mul(10_000).saturating_add(number),
                    ),
                    base: base.clone(),
                });
            }
        }
    }

    Ok(Json(query.page(records)))
}
