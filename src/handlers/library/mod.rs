use askama::Template;
use serde::{Deserialize, Serialize};

use crate::models::series;
use crate::services::anilist;

pub mod bulk;
pub mod cleanup;
pub mod crud;
pub mod episodes;
pub mod misgrabs;
pub mod pages;
pub mod reconcile;
pub mod recycle;
pub mod search;

/// Per-card completeness summary for the library grid's status bar.
/// Answers exactly one question — "do I have what's aired?" — in
/// three states, reusing the episode table's color vocabulary
/// (green complete / red missing-and-actionable / accent in-flight).
/// Airing status deliberately isn't encoded here: the card's
/// RELEASING/FINISHED text chip already carries it.
#[derive(Debug, Clone, Default)]
pub struct CardProgress {
    /// `complete` | `missing` | `downloading` | `idle` (nothing
    /// aired yet, or the series has no episode data at all).
    pub state: String,
    /// Distinct episodes on disk or with a completed grab tag.
    pub downloaded: i64,
    /// Episodes aired so far (cached air dates <= today), falling
    /// back to the total episode count when no air dates are cached
    /// (honest fallback for Jikan-added series with sparse data).
    pub aired: i64,
    /// Fill percentage, `downloaded / aired` clamped to 0..=100.
    pub pct: i64,
    /// False when `monitor_mode == "none"` — the bar renders dimmed:
    /// "I'm ignoring this" is a modifier, not a fourth color.
    pub monitored: bool,
    /// Pre-built tooltip text for the bar.
    pub label: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    page: String,
    /// One entry per rendered card: the series plus its completeness
    /// summary. Tupled (rather than a wrapper struct) so the card
    /// template's dense `s.*` field references stay untouched.
    cards: Vec<(series::Series, CardProgress)>,
    title_language: String,
    /// #62 — the linked external account's `score_format`,
    /// used by `Series::user_score_display(...)` to render per-row
    /// "You: X" badges. Empty string when no account is linked, in
    /// which case `user_score_display` always returns `None` and
    /// the template renders no badge.
    score_format: String,
    /// #62 — AL custom-list names with member counts, across the
    /// whole library. Powers the scope-chip row under the title.
    /// Empty when no memberships have synced yet, in which case the
    /// template hides the chip row's list entries (the "All" chip
    /// alone would be noise).
    list_counts: Vec<(String, i64)>,
    /// Currently-active filter value from `?list=<name>`. Empty
    /// means "no filter, show everything." The handler has already
    /// applied the filter to `library`; this field drives which
    /// chip renders as active.
    custom_list_filter: String,
    /// Currently-active library search query from `?search=<text>`.
    /// Echoed back so the input's `value` persists across
    /// navigations.
    search_query: String,
    /// Canonical sort value (`recent` / `oldest` / `title_asc` /
    /// `title_desc` / `score` / `score_asc`). The handler has
    /// already applied it to `library`; chips carry it through
    /// their hrefs so switching scope keeps the ordering.
    sort_value: String,
    /// Sort UI decomposition of `sort_value`: which key the select
    /// shows (`recent` / `title` / `score`)...
    sort_key: String,
    /// ...and whether the direction toggle points descending. The
    /// key+direction pair recomposes to the canonical value in
    /// static/js/index.js (librarySortNavigate).
    sort_desc: bool,
    /// Whole-library size, computed before any filter is applied —
    /// the identity row describes the collection, not the current
    /// view (chips carry the per-scope counts).
    total_count: usize,
    /// How many of those are currently airing (AL `RELEASING` /
    /// MAL `CURRENTLY_AIRING`). Renders as "· N airing" next to the
    /// total; hidden at zero.
    airing_count: usize,
    /// Recycle bin configured (#123): switches the bulk-delete modal's
    /// "cannot be undone" copy to "moves to the recycle bin".
    recycle_enabled: bool,
    /// Entries currently in the bin (cached a minute). The toolbar's
    /// bin control renders only when this is non-zero, with the count as
    /// a badge, so the control exists exactly when there is something to
    /// get back.
    recycle_count: u64,
}

/// Issue #219 — an adult title (AniList `isAdult`) can only be found
/// through a configured torznab / newznab indexer: Nyaa lists adult
/// releases on sukebei, which Ryokan does not search. The series page
/// banner and the auto-search toast both key off this so the two
/// warnings can't drift apart.
pub(crate) fn adult_needs_indexer(is_adult: bool, indexer_count: usize) -> bool {
    is_adult && indexer_count == 0
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
    /// but non-imported torrents exist in the download client's folder.
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
    /// Issue #106 — true when `metadata_refreshed_at` is older than
    /// `METADATA_REFRESH_INTERVAL_HOURS`. Drives a "Metadata may be
    /// out of date" warning banner on the page; usually a sign the
    /// upstream provider chain (AniList → Jikan → Kitsu) has been
    /// unavailable longer than the refresh window. Existing read
    /// path already serves the cached row regardless of staleness;
    /// this flag just makes the situation visible to the user.
    metadata_is_stale: bool,
    /// Issue #219 — true when AniList marks the title adult and no
    /// indexer is configured. Drives the warning banner: Nyaa lists
    /// adult releases on sukebei, which Ryokan does not search, so the
    /// series is unsearchable until an adult-capable indexer exists.
    adult_without_indexers: bool,
    /// Recycle bin configured (#123): the remove-series modal and the
    /// per-episode delete confirm say "moves to the recycle bin"
    /// instead of "cannot be undone."
    recycle_enabled: bool,
    /// Raw `monitor_mode` (`all` / `future` / `missing` / `existing` /
    /// `none`). Distinct from `monitor_mode_select_value` which encodes
    /// the dropdown's display state (the latter says `"sync"` when the
    /// series is following AL/MAL with no manual override).
    #[allow(dead_code)]
    monitor_mode: String,
    monitor_mode_label: String,
    /// #62 — `true` when the user has manually pinned monitor_mode
    /// through the dropdown (sync's merge step skips the row). Drives
    /// a small "pinned" hint next to the dropdown.
    monitor_mode_manual_override: bool,
    /// #62 — `true` when an external account is linked AND this
    /// series carries a `synced_from_external_account_id`. Gates the
    /// "Sync from AL/MAL" dropdown option; for manually-added series
    /// not on the user's list, the option doesn't make sense.
    can_sync_from_external_account: bool,
    /// "AniList" or "MyAnimeList" — used in the dropdown option label
    /// so the user sees provider-specific copy. Empty when no account
    /// is linked.
    sync_provider_label: String,
    /// #62 — value the dropdown should treat as "selected" so
    /// only one option highlights. Equals `"sync"` when the series
    /// is following the external account (sync-tracked + override
    /// cleared); otherwise equals `monitor_mode`.
    monitor_mode_select_value: String,
    /// #62 — pre-rendered "You: X" badge for the detail page,
    /// formatted per the linked account's `score_format`. `None`
    /// when no account is linked, the user hasn't rated this
    /// series, or the score is the unrated sentinel. The variant
    /// (Text vs. Smiley) drives whether the template renders a
    /// string or an inline SVG outline face.
    user_score_display: Option<crate::services::user_score::FormattedUserScore>,
    /// #62 — AL custom-list names this series belongs to.
    /// Sorted alphabetically by the model layer. Empty when no
    /// memberships are recorded (no account linked, or sync hasn't
    /// found this series in any custom list); the template hides
    /// the badge row in that case.
    custom_list_memberships: Vec<String>,
    monitored_count: i32,
    all_monitored: bool,
    /// Phase 4: series-level upgrade opt-in. Rendered as a checkbox on the
    /// series detail page; toggled via POST /api/library/allow-upgrades.
    allow_upgrades: bool,
    /// Issue #28 — per-series PT upgrade opt-in. Default off. The
    /// upgrade sweep skips a candidate when its source indexer is private
    /// and this is false. Toggled via POST /api/library/allow-pt-upgrades.
    /// Rendered inside an "Advanced" collapsible since most users won't
    /// flip it.
    allow_pt_upgrades: bool,
    /// #23 — Per-series custom Nyaa query tokens. Empty string means
    /// "use the global default in config." Rendered in the Advanced
    /// search panel on the series detail page.
    custom_query_tokens: String,
    /// #23 — Per-series Nyaa uploader restriction. Empty string means
    /// "use the global default in config."
    restrict_to_uploader: String,
    /// Alternate titles the user added for this series, one per line.
    alternate_titles: String,
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
    /// `grab_preview_mode` from config (`batches_only` or `never`).
    /// Surfaced to the series page's interactive-search JS so batch
    /// grabs can route through the file picker per #83, with the same
    /// opt-out semantics as the main search page.
    grab_preview_mode: String,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    page: String,
    title: String,
    message: String,
    detail: String,
    title_language: String,
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
    /// Raw byte count for the on-disk file; 0 when the episode isn't
    /// in the library root yet. Exposed alongside `size_display` so
    /// JS callers can recompute aggregates (e.g. the season-size
    /// span) live without re-fetching the page — `format_size` and
    /// the JS `formatBytes` helper agree on the rendering.
    pub size_bytes: i64,
    pub filename: String,
    pub can_auto_search: bool,
    pub monitored: bool,
    /// True when the episode has no file AND hasn't aired yet: its
    /// air date parses to a future date, or the air date is unknown
    /// while the series is still airing/upcoming (anything but
    /// FINISHED / FINISHED_AIRING / CANCELLED). Splits the no-file
    /// display state in two: "Missing" (red, actionable — the episode
    /// aired and we don't have it) vs "Unaired" (neutral — nothing is
    /// wrong, there's just nothing to grab yet). Mirrors Sonarr's
    /// Missing-vs-Unaired episode split.
    pub unaired: bool,
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
    /// Title-language hint for the HTMX partial response. The browser
    /// reads `localStorage.titleLanguage` (set by the title-switcher) and
    /// forwards it via `hx-vals` so the server-rendered card picks the
    /// same title the rest of the page is using; `None` falls back to
    /// the config-level `title_language`. Ignored on the JSON path.
    #[serde(default)]
    pub lang: Option<String>,
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

/// Issue #28 — per-series PT upgrade opt-in form. Same shape
/// as [`SetAllowUpgradesForm`] but toggles the second-axis flag
/// (`series.allow_pt_upgrades`) that gates whether the upgrade
/// sweep is allowed to grab a private-tracker release for this
/// series. Default off; user opts in per series.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetAllowPtUpgradesForm {
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
    /// Alternate titles, one per line. Empty string clears.
    #[serde(default)]
    alternate_titles: String,
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

#[cfg(test)]
mod adult_indexer_tests {
    use super::adult_needs_indexer;

    #[test]
    fn adult_needs_indexer_only_when_adult_and_none_configured() {
        assert!(adult_needs_indexer(true, 0));
        assert!(!adult_needs_indexer(true, 1));
        assert!(!adult_needs_indexer(false, 0));
        assert!(!adult_needs_indexer(false, 3));
    }
}
