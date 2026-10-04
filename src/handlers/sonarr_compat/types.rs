//! Sonarr-shape response + request DTOs.
//!
//! The shared DTOs (QualityProfile, Tag, SystemStatus, DownloadClientEntry,
//! TagBody, LookupQuery) live in crate::handlers::arr_shared; this file
//! only has the Sonarr-specific ones.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── Sonarr-compatible types ────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrSeries {
    pub id: i64,
    pub title: String,
    pub sort_title: String,
    pub status: String,
    pub overview: String,
    pub network: String,
    pub air_time: String,
    pub images: Vec<SonarrImage>,
    pub remote_poster: String,
    pub seasons: Vec<SonarrSeason>,
    pub year: i32,
    pub path: String,
    pub profile_id: i32,
    pub language_profile_id: i32,
    pub season_folder: bool,
    pub monitored: bool,
    pub use_scene_numbering: bool,
    pub runtime: i32,
    pub tvdb_id: i64,
    /// Sonarr v4's `tmdbId`. Informational: Seerr keys series by
    /// `tvdbId`, which must never carry this id.
    pub tmdb_id: i64,
    pub tv_rage_id: i64,
    pub tv_maze_id: i64,
    pub first_aired: String,
    pub series_type: String,
    pub clean_title: String,
    pub imdb_id: String,
    pub title_slug: String,
    pub certification: String,
    pub genres: Vec<String>,
    pub tags: Vec<i32>,
    pub added: String,
    pub ratings: SonarrRatings,
    pub quality_profile_id: i32,
    pub root_folder_path: String,
    pub statistics: SonarrStatistics,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrImage {
    pub cover_type: String,
    pub url: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrSeason {
    pub season_number: i32,
    pub monitored: bool,
    pub statistics: SonarrSeasonStats,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrSeasonStats {
    pub episode_file_count: i32,
    pub episode_count: i32,
    pub total_episode_count: i32,
    pub size_on_disk: i64,
    pub percent_of_episodes: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrRatings {
    pub votes: i32,
    pub value: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SonarrStatistics {
    pub season_count: i32,
    pub episode_file_count: i32,
    pub episode_count: i32,
    pub total_episode_count: i32,
    pub size_on_disk: i64,
    pub percent_of_episodes: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootFolder {
    pub id: i32,
    pub path: String,
    pub free_space: i64,
    pub total_space: i64,
    pub unmapped_folders: Vec<()>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageProfile {
    pub id: i32,
    pub name: String,
}

// ── Request types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AddSeriesBody {
    pub tvdb_id: Option<i64>,
    pub title: Option<String>,
    pub quality_profile_id: Option<i32>,
    pub language_profile_id: Option<i32>,
    pub seasons: Option<Vec<SeasonInput>>,
    pub tags: Option<Vec<i32>>,
    pub season_folder: Option<bool>,
    pub monitored: Option<bool>,
    pub root_folder_path: Option<String>,
    pub series_type: Option<String>,
    pub add_options: Option<AddOptions>,
    // Extra fields Seerr may send — we preserve them on passthrough.
    pub title_slug: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SeasonInput {
    pub season_number: i32,
    pub monitored: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AddOptions {
    pub ignore_episodes_with_files: Option<bool>,
    pub search_for_missing_episodes: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct UpdateSeriesBody {
    pub id: i64,
    pub monitored: Option<bool>,
    pub seasons: Option<Vec<SeasonInput>>,
    pub tags: Option<Vec<i32>>,
    // Accept and ignore all other fields Seerr passes through.
    #[serde(flatten)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
pub struct CommandBody {
    pub name: Option<String>,
    #[serde(rename = "seriesId")]
    pub series_id: Option<i64>,
}
