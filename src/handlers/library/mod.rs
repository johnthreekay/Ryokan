use askama::Template;
use serde::{Deserialize, Serialize};

use crate::models::{episode_tags, series};
use crate::services::anilist;

pub mod crud;
pub mod episodes;
pub mod pages;
pub mod reconcile;
pub mod search;

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    page: String,
    library: Vec<series::Series>,
    title_language: String,
}

#[derive(Template)]
#[template(path = "needs_review.html")]
struct NeedsReviewTemplate {
    page: String,
    entries: Vec<episode_tags::NeedsReviewEntry>,
}

#[derive(Template)]
#[template(path = "series.html")]
struct SeriesTemplate {
    page: String,
    route_id: i64,
    detail: anilist::AnimeDetail,
    is_tracked: bool,
    db_id: Option<i64>,
    folder_name: String,
    media_root: String,
    episodes: Vec<Episode>,
    ep_total: i32,
    /// Count of episodes whose file is present under `media_root`.
    /// Used by the delete-confirmation copy ("N episode files will
    /// be deleted from disk") — stays literal even when downloaded
    /// but non-imported torrents exist in qBit's folder.
    on_disk_count: i32,
    /// Count of episodes considered "downloaded" for the season badge.
    /// Matches `Episode.downloaded` — on-disk plus state=completed —
    /// so the `12 / 12` badge updates when post-proc-off torrents
    /// finish, without misrepresenting the delete confirmation above.
    downloaded_count: i32,
    size_display: String,
    title_language: String,
    relation_groups: Vec<RelationGroup>,
    /// Link to anilist.co for this series when a real (positive) AL ID
    /// is known. Empty for Jikan-fallback series with a synthetic
    /// negative id (those have no real AniList entry to link to).
    anilist_url: String,
    /// Link to myanimelist.net for this series when a MAL id is known.
    /// Populated from `detail.id_mal` regardless of source — AL returns
    /// it directly, and the Jikan fallback path populates the same
    /// field on the detail struct. The synthetic-negative sentinel
    /// stored on `series.anilist_id` is never read here.
    mal_url: String,
    /// Last refresh timestamp from `provider_metadata_cache.cached_at`.
    /// Empty when we've never cached metadata (shouldn't normally
    /// happen for a series reaching this page — be defensive).
    metadata_refreshed_at: String,
    monitor_mode: String,
    monitor_mode_label: String,
    monitored_count: i32,
    all_monitored: bool,
    /// Phase 4: series-level upgrade opt-in. Rendered as a checkbox on the
    /// series detail page; toggled via POST /api/library/allow-upgrades.
    allow_upgrades: bool,
    /// #23 — Per-series custom Nyaa query tokens. Empty string means
    /// "use the global default in config." Rendered in the Advanced
    /// search panel on the series detail page.
    custom_query_tokens: String,
    /// #23 — Per-series Nyaa uploader restriction. Empty string means
    /// "use the global default in config."
    restrict_to_uploader: String,
    /// #23 — Global defaults, surfaced as placeholder hints so the user
    /// can see what the per-series field will inherit when left blank.
    default_custom_query_tokens: String,
    default_restrict_to_uploader: String,
    /// Whether post-processing (file move + rename + NFO) is enabled in
    /// config. Rendered into the page as a JS global so the episode-row
    /// poller knows whether to show "Importing…" between a 100%-download
    /// and the completion checkmark, or to skip straight to the
    /// checkmark when post-proc is off (#14).
    post_processing_enabled: bool,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    page: String,
    title: String,
    message: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Episode {
    pub number: i32,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub aired: String,
    pub on_disk: bool,
    /// Sonarr-parity split (#14): true when the episode's download is
    /// complete regardless of whether it's been imported into
    /// media_root. Specifically: `on_disk OR tag.state == "completed"`.
    /// Drives the series-page checkmark. Without this, turning
    /// post-processing off leaves the row stuck showing "missing" even
    /// after qBit finishes, because `on_disk` only reflects media_root
    /// presence. Mirrors Sonarr's Activity "downloaded" indicator, which
    /// is independent of the library-side `HasFile`.
    pub downloaded: bool,
    pub quality: String,
    pub quality_state: String, // "disk", "grabbed", "failed", or ""
    pub size_display: String,
    pub filename: String,
    pub can_auto_search: bool,
    pub monitored: bool,
    /// Phase 4 classification columns — exposed to the template so the
    /// manual override picker can pre-select the current values. The
    /// override dropdown's composite key (e.g. "bluray_remux", "web",
    /// "webrip") is derived from this quartet in the template JS.
    pub class_source: String,
    pub class_resolution: String,
    pub class_is_remux: bool,
    pub class_is_bdmv: bool,
    pub class_web_kind: String,
    pub manual_override: bool,
    pub needs_review: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationGroup {
    pub relation_type: String,
    pub label: String,
    pub entries: Vec<RelationCard>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationCard {
    pub id: i64,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub episodes: Option<i32>,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct AnilistSearchQuery {
    pub q: String,
    /// Per-search provider override. `"al"` forces AniList (with the
    /// usual MAL fallback if AL is unreachable), `"mal"` skips AniList
    /// and goes straight to Jikan/MAL. Anything else (or omitted) falls
    /// back to the global `force_mal_fallback` flag in `config`. This
    /// is the human-facing toggle for the Add Series modal; it does NOT
    /// affect the Sonarr/Radarr shim lookup endpoints, which always do
    /// AL-first with MAL only on AL failure.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AddSeriesForm {
    anilist_id: i64,
    mal_id: Option<i64>,
    title: String,
    title_romaji: String,
    title_english: String,
    title_native: String,
    cover_url: String,
    format: String,
    status: String,
    episodes: Option<i32>,
    season_year: Option<i32>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RemoveSeriesForm {
    id: i64,
    /// When true (the default for the "Remove from Library" button), the
    /// handler also tells qBittorrent to drop every torrent associated
    /// with the series and removes the series media folder from disk.
    /// Settable to false from API consumers (e.g. the Sonarr compat shim)
    /// that want to delete *only* the database tracking row.
    #[serde(default)]
    delete_files: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetFolderForm {
    series_id: i64,
    folder_name: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetMonitoringForm {
    series_id: i64,
    monitor_mode: String,
    auto_grab: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetEpisodeMonitoringForm {
    series_id: i64,
    episode_number: i32,
    monitored: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetAllowUpgradesForm {
    series_id: i64,
    allow: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetSearchOverridesForm {
    series_id: i64,
    /// Empty string clears the override and makes the series use the global
    /// `config.default_custom_query_tokens` default.
    #[serde(default)]
    custom_query_tokens: String,
    /// Nyaa uploader to restrict to (`?u=<name>`). Empty string clears.
    #[serde(default)]
    restrict_to_uploader: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetManualOverrideForm {
    series_id: i64,
    episode_number: i32,
    /// Empty string clears the override and reverts to classifier output.
    source: String,
    resolution: String,
    #[serde(default)]
    is_remux: bool,
    /// Sonarr-parity: BD-Raw / BDMV release flag, distinct from `is_remux`.
    /// Mutually exclusive at the label level — when both are set, BDMV wins.
    #[serde(default)]
    is_bdmv: bool,
    /// Sonarr-parity: WEB-DL vs WEBRip variant when `source == "Web"`.
    /// Empty string for legacy bare-WEB rows or non-Web sources.
    #[serde(default)]
    web_kind: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReclassifyEpisodeForm {
    pub series_id: i64,
    pub episode_number: i32,
}

/// Batch-apply manual overrides — used by the bulk-actions UI on
/// `/library/review` so a user can tag a selection of rows with the
/// same (or per-row) override in one transaction instead of N
/// round trips.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct BulkManualOverrideForm {
    pub items: Vec<SetManualOverrideForm>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct MarkEpisodeFailedForm {
    history_id: i64,
    #[serde(default)]
    blocklist: bool,
}
