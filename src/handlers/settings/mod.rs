use askama::Template;
use axum::{
    Form, Json,
    extract::{Query, State},
    response::{Html, Redirect},
};
use serde::Deserialize;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, custom_formats as cf_model, group_source_map};
use crate::services::{
    custom_formats as cf_service,
    download_client::{DownloadClient, qbittorrent::QbitClient},
    jellyfin::JellyfinClient,
    logger,
    source::Source,
};

pub mod custom_formats;
use custom_formats::ImportReviewView;

/// View-model wrapper rendered on the Custom Formats tab. Surfaces
/// parse errors (so the user can spot broken CFs without tailing logs)
/// and carries the per-spec label list used by the card-grid UI to
/// render condition pills.
pub struct CustomFormatView {
    pub row: cf_model::CustomFormatRow,
    pub parse_error: Option<String>,
    /// Sonarr-style condition pills shown on the CF card. Extracted
    /// directly from the row's JSON `specifications[]` array (the
    /// compiled form drops the per-spec `name` field, which is exactly
    /// what the pill needs to render). Empty for parse-error rows; the
    /// template uses `.len()` for the count display too.
    pub spec_labels: Vec<SpecLabelView>,
}

pub struct SpecLabelView {
    pub name: String,
    pub implementation: String,
    pub negate: bool,
    pub required: bool,
}

/// Extract the per-spec labels used by CF card pills. Pulls
/// `name`/`implementation`/`negate`/`required` straight out of the
/// JSON — the compiled form at this layer already dropped the `name`
/// field, so re-parsing as a loose `Value` is the simplest path.
/// Returns an empty vec on any parse failure; the caller already
/// surfaces the parse error via `parse_error`.
fn extract_spec_labels(json: &str) -> Vec<SpecLabelView> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let specs = match value.get("specifications").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    specs
        .iter()
        .map(|s| {
            let name = s
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let implementation = s
                .get("implementation")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let negate = s.get("negate").and_then(|v| v.as_bool()).unwrap_or(false);
            let required = s.get("required").and_then(|v| v.as_bool()).unwrap_or(false);
            SpecLabelView {
                name,
                implementation,
                negate,
                required,
            }
        })
        .collect()
}

/// View-model wrapper rendered when the Custom Formats tab is in edit
/// mode. Holds the full row plus any `trash_description` extracted
/// from the row's JSON body. Plan §5.7.6 wants descriptions to persist
/// through round-trips and surface in the edit drawer so the user
/// keeps the Trash Guides context that originally shipped the CF.
pub struct CustomFormatEditView {
    pub row: cf_model::CustomFormatRow,
    pub trash_description: Option<String>,
}

/// Parse a stored CF's JSON body and return the `trash_description`
/// string if it's present, non-empty, and a string. Silently returns
/// `None` on parse error — the row itself still renders via the raw
/// `edit.json` textarea, so the description is a nice-to-have, not a
/// blocker.
fn extract_trash_description(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let desc = value.get("trash_description")?.as_str()?.trim();
    if desc.is_empty() {
        None
    } else {
        Some(desc.to_string())
    }
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    page: String,
    tab: String,
    config: config::Config,
    groups: Vec<group_source_map::GroupSourceEntry>,
    suggestions: Vec<group_source_map::GroupSuggestion>,
    custom_formats: Vec<CustomFormatView>,
    custom_format_edit: Option<CustomFormatEditView>,
    /// Pre-rendered string for the minimum-score input. Empty when the
    /// floor is the `i32::MIN` "no floor" sentinel. Computed here so the
    /// Askama template doesn't need to compare against an integer path.
    custom_format_min_score_display: String,
    /// Populated when the import flow hit a name collision. The CF tab
    /// renders a review block with per-collision radio buttons so the
    /// user can pick overwrite/rename/skip for each conflicting CF.
    /// See plan §6.2.
    custom_format_import_review: Option<ImportReviewView>,
    message: Option<String>,
    error: Option<String>,
    version: &'static str,
}

fn min_score_display(score: i32) -> String {
    if score == i32::MIN {
        String::new()
    } else {
        score.to_string()
    }
}

#[derive(Deserialize)]
pub struct SettingsQuery {
    tab: Option<String>,
    /// When the Custom Formats tab is active and `edit_id` is set, the
    /// upsert form prefills from the existing row so the user can fix
    /// the JSON in place rather than deleting and re-pasting.
    edit_id: Option<i64>,
    /// Optional flash message / error surfaced after a POST-redirect.
    /// Kept minimal — detailed validation errors skip the redirect path
    /// and re-render inline so the form state is preserved.
    msg: Option<String>,
    err: Option<String>,
}

#[derive(Deserialize)]
pub struct SettingsForm {
    tab: Option<String>,
    /// #63 Phase 2 — which download client is active. Accepted
    /// values: "qbittorrent" | "deluge". Settings save branches on
    /// this to construct the concrete trait impl.
    #[serde(default)]
    active_client: String,
    qbit_url: String,
    qbit_user: String,
    qbit_pass: String,
    qbit_category: String,
    qbit_download_path: String,
    #[serde(default)]
    deluge_url: String,
    #[serde(default)]
    deluge_password: String,
    #[serde(default)]
    deluge_label: String,
    #[serde(default)]
    deluge_download_path: String,
    #[serde(default)]
    transmission_url: String,
    #[serde(default)]
    transmission_user: String,
    #[serde(default)]
    transmission_password: String,
    #[serde(default)]
    transmission_label: String,
    #[serde(default)]
    transmission_download_path: String,
    #[serde(default)]
    rtorrent_url: String,
    #[serde(default)]
    rtorrent_user: String,
    #[serde(default)]
    rtorrent_password: String,
    #[serde(default)]
    rtorrent_label: String,
    #[serde(default)]
    rtorrent_download_path: String,
    jellyfin_url: String,
    jellyfin_api_key: String,
    preferred_groups: String,
    blocked_groups: String,
    preferred_source: String,
    preferred_resolution: String,
    cutoff_source: String,
    cutoff_resolution: String,
    finished_series_quality: String,
    media_root: String,
    title_language: String,
    rss_enabled: Option<String>,
    rss_interval_minutes: i32,
    post_processing_enabled: Option<String>,
    post_processing_mode: String,
    prefer_subs: String,
    sonarr_enabled: Option<String>,
    sonarr_api_key: Option<String>,
    radarr_enabled: Option<String>,
    radarr_api_key: Option<String>,
    upgrade_search_enabled: Option<String>,
    seadex_enabled: Option<String>,
    default_custom_query_tokens: Option<String>,
    default_restrict_to_uploader: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct QbitTestForm {
    qbit_url: String,
    qbit_user: String,
    qbit_pass: String,
    qbit_category: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct JellyfinTestForm {
    jellyfin_url: String,
    jellyfin_api_key: String,
}

fn normalize_settings_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("quality") => "quality".to_string(),
        Some("custom_formats") => "custom_formats".to_string(),
        Some("groups") => "groups".to_string(),
        Some("general") => "general".to_string(),
        _ => "integrations".to_string(),
    }
}

/// Load every CF row and annotate each one with its parsed spec count
/// (or the parse error string, if compilation fails). Used by the
/// Custom Formats tab to surface broken rows in the list view so the
/// user can find and fix them without trawling logs.
async fn load_custom_formats_view(db: &sqlx::SqlitePool) -> Vec<CustomFormatView> {
    let rows = cf_model::list_with_scores(db).await.unwrap_or_default();
    rows.into_iter()
        .map(|row| {
            let spec_labels = extract_spec_labels(&row.json);
            match cf_service::compile_from_json(&row.json, row.score as i32, row.id) {
                Ok(_) => CustomFormatView {
                    parse_error: None,
                    spec_labels,
                    row,
                },
                Err(e) => CustomFormatView {
                    parse_error: Some(e),
                    spec_labels,
                    row,
                },
            }
        })
        .collect()
}

async fn load_groups(db: &sqlx::SqlitePool) -> Vec<group_source_map::GroupSourceEntry> {
    group_source_map::list_all(db).await.unwrap_or_default()
}

/// Load group-map suggestions inferred from the user's manual overrides.
/// Threshold of 2 matches `compute_suggestions`' docstring rationale: a
/// single override is noise, two matching overrides is the smallest
/// pattern worth surfacing.
async fn load_suggestions(db: &sqlx::SqlitePool) -> Vec<group_source_map::GroupSuggestion> {
    group_source_map::compute_suggestions(db, 2)
        .await
        .unwrap_or_default()
}

/// Sanitize a user-entered download-client scoping label: trim
/// surrounding whitespace, strip any control characters (newlines,
/// tabs, NUL, etc.) that could otherwise survive through to the
/// client's own command parsers (rtorrent's `d.custom1.set="..."`
/// inline command string is the most vulnerable — a literal newline
/// in the label would terminate the command early and let the rest
/// be re-parsed as a separate command). Falls back to `"ryokan"` if
/// the sanitized value is empty.
fn sanitize_label(raw: &str) -> String {
    let filtered: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = filtered.trim();
    if trimmed.is_empty() {
        "ryokan".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Validate a form-submitted source string by round-tripping through
/// `Source::from_str`. Returns the canonical lowercase form on success, or
/// the supplied default when the value is unrecognized.
fn validate_source(value: &str, default: &str) -> String {
    use crate::services::source::Source;
    let parsed = Source::from_str(value);
    if parsed == Source::Unknown {
        default.to_string()
    } else {
        // Store the canonical lowercase form (e.g. "bluray", "web") so reads
        // via Source::from_str always succeed.
        parsed.as_str().to_ascii_lowercase()
    }
}

/// Validate a form-submitted cutoff-source string. Like `validate_source`
/// but also passes through the BluRay sub-tier markers "bluray_remux" and
/// "bluray_bdmv" so settings can store BD Remux / BD RAW as distinct
/// cutoffs. Reads go through `source::parse_cutoff_source`.
fn validate_cutoff_source(value: &str, default: &str) -> String {
    if value == "bluray_remux" || value == "bluray_bdmv" {
        return value.to_string();
    }
    validate_source(value, default)
}

/// Validate a form-submitted resolution string by round-tripping through
/// `Resolution::from_str`. Returns the bare numeric form ("1080", "720", …)
/// on success, or the supplied default when unrecognized.
fn validate_resolution(value: &str, default: &str) -> String {
    use crate::services::source::Resolution;
    let parsed = Resolution::from_str(value);
    if parsed == Resolution::Unknown {
        default.to_string()
    } else {
        // Strip the trailing 'p' for DB consistency ("1080" not "1080p").
        parsed.as_str().trim_end_matches('p').to_string()
    }
}

/// Build a fully-populated `SettingsTemplate` with the same loading
/// logic the main settings page uses. Extracted so the CF import
/// handler can re-render the settings page in place on a name
/// collision without duplicating every DB query the normal page
/// renderer runs. Callers override the `tab`, `edit_id`, `msg`, `err`,
/// and optional import-review fields to tailor the resulting page.
#[allow(clippy::too_many_arguments)]
async fn build_settings_template(
    state: &AppState,
    tab: Option<String>,
    edit_id: Option<i64>,
    msg: Option<String>,
    err: Option<String>,
    import_review: Option<ImportReviewView>,
) -> SettingsTemplate {
    // Fan out the four independent lookups — config row, release-group
    // table, suggestion panel, custom-format list — in parallel. The old
    // code issued them sequentially so the wall time was the sum of four
    // round trips even though none depends on the others.
    let (cfg_res, groups, suggestions, custom_formats) = tokio::join!(
        config::get_config(&state.db),
        load_groups(&state.db),
        load_suggestions(&state.db),
        load_custom_formats_view(&state.db),
    );
    let cfg = cfg_res.ok().flatten().unwrap_or_default();

    // Prefill the CF edit form only when the query param points at a row
    // that actually exists — stale edit links just fall through to the
    // "Add new" form, which is the safer default.
    let custom_format_edit = match edit_id {
        Some(id) => cf_model::get_by_id(&state.db, id)
            .await
            .ok()
            .flatten()
            .map(|row| {
                let trash_description = extract_trash_description(&row.json);
                CustomFormatEditView {
                    row,
                    trash_description,
                }
            }),
        None => None,
    };

    let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
    SettingsTemplate {
        page: "settings".to_string(),
        tab: normalize_settings_tab(tab),
        config: cfg,
        groups,
        suggestions,
        custom_formats,
        custom_format_edit,
        custom_format_min_score_display,
        custom_format_import_review: import_review,
        message: msg,
        error: err,
        version: env!("CARGO_PKG_VERSION"),
    }
}

pub async fn settings_page(
    State(state): State<AppState>,
    Query(params): Query<SettingsQuery>,
) -> Html<String> {
    let template = build_settings_template(
        &state,
        params.tab,
        params.edit_id,
        params.msg,
        params.err,
        None,
    )
    .await;
    Html(template.render().unwrap_or_default())
}

pub async fn settings_submit(
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Html<String> {
    // Load the existing config row once and derive every non-form
    // field from it. The previous code fetched it twice back-to-back
    // (once for force_mal_fallback, once for the rest), which was
    // harmless functionally but paid an extra SQLite round trip on
    // every settings save. `existing_cfg` feeds `force_mal_fallback`,
    // `force_kitsu_fallback`, the legacy quality tier columns, and
    // `auto_grab_on_add` / `allow_non_english` below.
    let existing_cfg = config::get_config(&state.db).await.ok().flatten();

    let current_force_mal_fallback = existing_cfg
        .as_ref()
        .map(|cfg| cfg.force_mal_fallback)
        .unwrap_or(false);
    let current_force_kitsu_fallback = existing_cfg
        .as_ref()
        .map(|cfg| cfg.force_kitsu_fallback)
        .unwrap_or(false);

    let cfg = config::Config {
        active_client: match form.active_client.trim() {
            "deluge" => "deluge".to_string(),
            "transmission" => "transmission".to_string(),
            "rtorrent" => "rtorrent".to_string(),
            // Any other value (including empty from pre-Phase-2 form
            // submissions) collapses to qbittorrent — preserves the
            // Phase 1 default and avoids accidentally switching users
            // onto a client they haven't configured.
            _ => "qbittorrent".to_string(),
        },
        qbit_url: form.qbit_url.trim().to_string(),
        qbit_user: form.qbit_user.trim().to_string(),
        qbit_pass: form.qbit_pass,
        qbit_category: form.qbit_category.trim().to_string(),
        qbit_download_path: form
            .qbit_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        deluge_url: form.deluge_url.trim().trim_end_matches('/').to_string(),
        deluge_password: form.deluge_password,
        deluge_label: sanitize_label(&form.deluge_label),
        deluge_download_path: form
            .deluge_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        transmission_url: form
            .transmission_url
            .trim()
            .trim_end_matches('/')
            .to_string(),
        transmission_user: form.transmission_user.trim().to_string(),
        transmission_password: form.transmission_password,
        transmission_label: sanitize_label(&form.transmission_label),
        transmission_download_path: form
            .transmission_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        rtorrent_url: form.rtorrent_url.trim().trim_end_matches('/').to_string(),
        rtorrent_user: form.rtorrent_user.trim().to_string(),
        rtorrent_password: form.rtorrent_password,
        rtorrent_label: sanitize_label(&form.rtorrent_label),
        rtorrent_download_path: form
            .rtorrent_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        jellyfin_url: form.jellyfin_url.trim().trim_end_matches('/').to_string(),
        jellyfin_api_key: form.jellyfin_api_key.trim().to_string(),
        preferred_groups: form.preferred_groups.trim().to_string(),
        blocked_groups: form.blocked_groups.trim().to_string(),
        preferred_source: validate_source(&form.preferred_source, "web"),
        preferred_resolution: validate_resolution(&form.preferred_resolution, "1080"),
        cutoff_source: validate_cutoff_source(&form.cutoff_source, "bluray"),
        cutoff_resolution: validate_resolution(&form.cutoff_resolution, "1080"),
        // Legacy combined tier columns — kept one release for rollback.
        // No longer user-editable; carried forward from the existing row.
        quality_profile: existing_cfg
            .as_ref()
            .map(|c| c.quality_profile.clone())
            .unwrap_or_else(|| "web_1080".to_string()),
        quality_cutoff: existing_cfg
            .as_ref()
            .map(|c| c.quality_cutoff.clone())
            .unwrap_or_else(|| "bd_1080".to_string()),
        finished_series_quality: match form.finished_series_quality.as_str() {
            "same" | "prefer_bd" | "bd_only" => form.finished_series_quality,
            _ => "prefer_bd".to_string(),
        },
        media_root: form.media_root.trim().trim_end_matches('/').to_string(),
        title_language: match form.title_language.as_str() {
            "romaji" | "english" | "native" => form.title_language,
            _ => "english".to_string(),
        },
        force_mal_fallback: current_force_mal_fallback,
        rss_enabled: form.rss_enabled.is_some(),
        rss_interval_minutes: form.rss_interval_minutes.clamp(1, 60),
        force_kitsu_fallback: current_force_kitsu_fallback,
        post_processing_enabled: form.post_processing_enabled.is_some(),
        post_processing_mode: match form.post_processing_mode.as_str() {
            "move" | "copy" | "hardlink" => form.post_processing_mode,
            _ => "hardlink".to_string(),
        },
        auto_grab_on_add: existing_cfg
            .as_ref()
            .map(|c| c.auto_grab_on_add)
            .unwrap_or(true),
        prefer_subs: form.prefer_subs == "1",
        allow_non_english: existing_cfg
            .as_ref()
            .map(|c| c.allow_non_english)
            .unwrap_or(false),
        sonarr_enabled: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.sonarr_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.sonarr_enabled)
                .unwrap_or(false)
        },
        sonarr_api_key: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.sonarr_api_key.unwrap_or_default().trim().to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.sonarr_api_key.clone())
                .unwrap_or_default()
        },
        radarr_enabled: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.radarr_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.radarr_enabled)
                .unwrap_or(false)
        },
        radarr_api_key: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.radarr_api_key.unwrap_or_default().trim().to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.radarr_api_key.clone())
                .unwrap_or_default()
        },
        upgrade_search_enabled: if form.tab.as_deref() == Some("quality") || form.tab.is_none() {
            form.upgrade_search_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.upgrade_search_enabled)
                .unwrap_or(false)
        },
        // Carried forward from the existing row — edited via the
        // dedicated Custom Formats tab's minimum-score form, not here.
        custom_format_minimum_score: existing_cfg
            .as_ref()
            .map(|c| c.custom_format_minimum_score)
            .unwrap_or(i32::MIN),
        seadex_enabled: if form.tab.as_deref() == Some("quality") || form.tab.is_none() {
            form.seadex_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.seadex_enabled)
                .unwrap_or(false)
        },
        // #23 — Search defaults live on the Quality tab alongside the
        // other search-scoped knobs. Preserve on other-tab saves.
        default_custom_query_tokens: if form.tab.as_deref() == Some("quality") || form.tab.is_none()
        {
            form.default_custom_query_tokens
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.default_custom_query_tokens.clone())
                .unwrap_or_default()
        },
        default_restrict_to_uploader: if form.tab.as_deref() == Some("quality")
            || form.tab.is_none()
        {
            form.default_restrict_to_uploader
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.default_restrict_to_uploader.clone())
                .unwrap_or_default()
        },
    };

    let active_tab = normalize_settings_tab(form.tab.clone());

    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(
            &state.db,
            LogCategory::System,
            "Failed to save settings",
            &e.to_string(),
        )
        .await;
        let groups = load_groups(&state.db).await;
        let suggestions = load_suggestions(&state.db).await;
        let custom_formats = load_custom_formats_view(&state.db).await;
        let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
        let template = SettingsTemplate {
            page: "settings".to_string(),
            tab: active_tab,
            config: cfg,
            groups,
            suggestions,
            custom_formats,
            custom_format_edit: None,
            custom_format_min_score_display,
            custom_format_import_review: None,
            message: None,
            error: Some(format!("Failed to save: {}", e)),
            version: env!("CARGO_PKG_VERSION"),
        };
        return Html(template.render().unwrap_or_default());
    }

    logger::info(&state.db, LogCategory::System, "Settings saved", "").await;
    let mut notices: Vec<String> = vec!["Settings saved.".to_string()];

    // #63 Phase 2 — client-switch handling. If the user changed
    // `active_client` on this save, any `grabbed_torrents` rows still
    // in `state='pending'` point at the OLD client (Ryokan is about
    // to stop talking to it). Three things need to happen, in order:
    //
    //   1. **Delete only the in-flight torrents from the OLD client.**
    //      An `state='pending'` row at this point is either
    //      mid-download OR complete-but-not-yet-imported. Deleting
    //      the mid-download ones is the user's intent ("cancel my
    //      in-flight grabs"). Deleting the completed-but-not-yet-
    //      imported ones would wipe finished files the user almost
    //      certainly wants to keep — they're identical to an already-
    //      imported torrent from the user's perspective; the only
    //      difference is post-processing didn't happen to run yet.
    //      So we gate the delete on `!is_complete()` from the old
    //      client's view.
    //
    //   2. **Mark ALL pending rows as `failed` regardless of delete
    //      outcome.** The old client is about to disappear from
    //      AppState; Ryokan can't track these grabs anymore. They
    //      need to drop out of the partial UNIQUE index on `(hash)
    //      WHERE state IN ('pending','imported')` so a re-grab in
    //      the new client can't dedupe against them. Completed
    //      torrents the user wants to keep stay on the old client's
    //      disk; Ryokan just forgets about them. The user can still
    //      see the files manually.
    //
    //   3. **Swap the AppState Arc** (further down in the
    //      `active_tab == "integrations"` block).
    //
    // Delete happens BEFORE the Arc swap because the read lock still
    // holds the OLD client's Arc at this point.
    if active_tab == "integrations"
        && let Some(old) = existing_cfg.as_ref()
        && old.active_client != cfg.active_client
    {
        let pending = crate::models::grabbed_torrents::get_all_pending(&state.db)
            .await
            .unwrap_or_default();

        if !pending.is_empty()
            && let Some(old_client) = state.download_client.read().await.clone()
        {
            // Build a hash → state_kind map from the old client's
            // current view. `list_scoped` returns only Ryokan-owned
            // torrents, which is a superset of our pending grabs in
            // the steady state. Failure to fetch == treat as empty
            // map (can't determine completion); we'll skip deletion
            // to be safe and let the user clean up manually, and
            // surface that in the UI notice so they know to look.
            let list_scoped_result = old_client.list_scoped().await;
            let list_scoped_failed = list_scoped_result.is_err();
            let states: std::collections::HashMap<
                String,
                crate::services::download_client::DownloadItemState,
            > = list_scoped_result
                .unwrap_or_default()
                .into_iter()
                .map(|t| (t.hash.to_lowercase(), t.state_kind))
                .collect();

            if list_scoped_failed {
                notices.push(format!(
                    "Couldn't reach {} to cancel in-flight torrents — verify and clean up manually if any were still downloading.",
                    old.active_client,
                ));
            }

            let mut deleted = 0usize;
            let mut skipped_complete = 0usize;
            let mut delete_failures: Vec<String> = Vec::new();
            for grab in &pending {
                if grab.hash.is_empty() {
                    continue;
                }
                let hash_lc = grab.hash.to_lowercase();
                // Only delete the torrent if the old client reports
                // it's NOT complete. Missing from the map (e.g. user
                // already deleted it manually in the old client, or
                // `list_scoped` failed) also skips deletion — we'd
                // rather leave a dangling download-client row than
                // delete a completed file the user wanted to keep.
                match states.get(&hash_lc) {
                    Some(state) if !state.is_complete() => {
                        match old_client.delete(&hash_lc, true).await {
                            Ok(()) => deleted += 1,
                            Err(e) => delete_failures.push(format!("{}: {}", grab.torrent_name, e)),
                        }
                    }
                    Some(_) => skipped_complete += 1,
                    None => {
                        // Not in the old client's list. Could be
                        // already manually removed, could be that
                        // list_scoped failed. Either way, skip
                        // deletion — no action we can take that's
                        // safer than leaving it alone.
                    }
                }
            }
            if deleted > 0 {
                logger::info(
                    &state.db,
                    LogCategory::System,
                    &format!(
                        "Cancelled {deleted} in-flight grab(s) from {} during client switch",
                        old.active_client
                    ),
                    "",
                )
                .await;
            }
            if skipped_complete > 0 {
                logger::info(
                    &state.db,
                    LogCategory::System,
                    &format!(
                        "Left {skipped_complete} completed grab(s) on {} intact during client switch — files preserved",
                        old.active_client
                    ),
                    "",
                )
                .await;
            }
            if !delete_failures.is_empty() {
                logger::warn(
                    &state.db,
                    LogCategory::System,
                    &format!(
                        "{} in-flight grab(s) could not be deleted from {} — DB state still flipped",
                        delete_failures.len(),
                        old.active_client
                    ),
                    &delete_failures.join("; "),
                )
                .await;
            }
        }

        let n = crate::models::grabbed_torrents::mark_all_pending_failed(&state.db)
            .await
            .unwrap_or(0);

        // Clear the per-episode "grabbed" UI state for every canceled
        // grab. `grabbed_torrents.state='failed'` alone isn't enough —
        // the series page reads `episode_quality_tags.state` for the
        // UI checkmark / badge, and the existing manual-cancel paths
        // (`handlers::library::episodes::cancel_pending_episode`,
        // `services::post_processing::run_once_inner` on stale-torrent
        // reconciliation) both call `episode_tags::clear_tags_for_removal`
        // alongside their mark-removed calls. The client-switch path
        // has to mirror that: without this, episodes sit forever in
        // "grabbed" state with no backing torrent.
        for grab in &pending {
            let _ = crate::models::episode_tags::clear_tags_for_removal(
                &state.db,
                grab.series_id,
                &grab.episode_numbers,
            )
            .await;
        }

        if n > 0 {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Marked {n} pending grabs as failed after client switch"),
                &format!("old={}, new={}", old.active_client, cfg.active_client),
            )
            .await;
            notices.push(format!(
                "Client changed from {} to {}; {n} pending grab{} released (in-flight downloads cancelled on {}; any completed files preserved).",
                old.active_client,
                cfg.active_client,
                if n == 1 { "" } else { "s" },
                old.active_client,
            ));
        }
    }

    if active_tab == "integrations" {
        let (client_label, configured, client_url) = match cfg.active_client.as_str() {
            "deluge" => (
                "Deluge",
                !cfg.deluge_url.is_empty(),
                cfg.deluge_url.as_str(),
            ),
            "transmission" => (
                "Transmission",
                !cfg.transmission_url.is_empty(),
                cfg.transmission_url.as_str(),
            ),
            "rtorrent" => (
                "rTorrent",
                !cfg.rtorrent_url.is_empty(),
                cfg.rtorrent_url.as_str(),
            ),
            _ => (
                "qBittorrent",
                !cfg.qbit_url.is_empty(),
                cfg.qbit_url.as_str(),
            ),
        };
        if configured {
            let client = crate::services::download_client::build_download_client(&cfg);
            if let Some(client) = client {
                match client.test().await {
                    Ok(version) => {
                        // `LogCategory::QBit` is reused here for
                        // every download client for now — log
                        // message body carries the real client name
                        // in "Connected to X Y.Z", so filtering by
                        // category still surfaces the events. A
                        // dedicated `LogCategory::DownloadClient`
                        // would be cleaner but churns every existing
                        // qBit log entry's category too; Phase 3
                        // cleanup.
                        logger::info(
                            &state.db,
                            LogCategory::QBit,
                            &format!("Connected to {client_label} {version}"),
                            client_url,
                        )
                        .await;
                        notices.push(format!("{client_label} connected ({version})."));
                        *state.download_client.write().await = Some(client);
                    }
                    Err(e) => {
                        logger::error(
                            &state.db,
                            LogCategory::QBit,
                            &format!("{client_label} connection failed"),
                            &e,
                        )
                        .await;
                        *state.download_client.write().await = None;
                        notices.push(format!("{client_label} connection failed: {e}."));
                    }
                }
            } else {
                *state.download_client.write().await = None;
            }
        } else {
            *state.download_client.write().await = None;
        }

        if !cfg.jellyfin_url.is_empty() && !cfg.jellyfin_api_key.is_empty() {
            let client = JellyfinClient::new(&cfg.jellyfin_url, &cfg.jellyfin_api_key);
            match client.test_connection().await {
                Ok(info) => {
                    let label = if info.server_name.trim().is_empty() {
                        format!("Jellyfin ({})", info.version)
                    } else {
                        format!(
                            "Jellyfin {} ({}) connected.",
                            info.server_name, info.version
                        )
                    };
                    logger::info(
                        &state.db,
                        LogCategory::Jellyfin,
                        &format!("{} connected", label),
                        &cfg.jellyfin_url,
                    )
                    .await;
                    notices.push(label);
                    *state.jellyfin.write().await = Some(client);
                }
                Err(e) => {
                    logger::error(&state.db, LogCategory::Jellyfin, "Connection failed", &e).await;
                    *state.jellyfin.write().await = None;
                    notices.push(format!("Jellyfin connection failed: {}.", e));
                }
            }
        } else {
            *state.jellyfin.write().await = None;
        }

        if !cfg.media_root.is_empty() && !std::path::Path::new(&cfg.media_root).is_dir() {
            notices.push(format!(
                "Warning: media root '{}' is not accessible.",
                cfg.media_root
            ));
        }
    }

    let groups = load_groups(&state.db).await;
    let suggestions = load_suggestions(&state.db).await;
    let custom_formats = load_custom_formats_view(&state.db).await;
    let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
    let template = SettingsTemplate {
        page: "settings".to_string(),
        tab: active_tab,
        config: cfg,
        groups,
        suggestions,
        custom_formats,
        custom_format_edit: None,
        custom_format_min_score_display,
        custom_format_import_review: None,
        // Joined with " " — not "<br>" — because the template now
        // auto-escapes `message`. Each notice is a complete sentence
        // ending in ".", so a space-joined run reads acceptably as a
        // single paragraph. Multi-notice POSTs are rare (only when
        // the user changes integration settings).
        message: Some(notices.join(" ")),
        error: None,
        version: env!("CARGO_PKG_VERSION"),
    };
    Html(template.render().unwrap_or_default())
}

// ─────────────────────────────────────────────────────────────────────────
// Release group source map CRUD
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GroupUpsertForm {
    group_name: String,
    source: String,
    confidence: Option<f32>,
    notes: Option<String>,
}

#[derive(Deserialize)]
pub struct GroupDeleteForm {
    group_name: String,
}

/// Upsert a user-edited row in `group_source_map`. Silently no-ops on an
/// empty group name or unknown source. Redirects back to the groups tab
/// regardless so the user sees the updated list.
pub async fn settings_groups_upsert(
    State(state): State<AppState>,
    Form(form): Form<GroupUpsertForm>,
) -> Redirect {
    let name = form.group_name.trim();
    if name.is_empty() {
        return Redirect::to("/settings?tab=groups");
    }
    let source = Source::from_str(&form.source);
    if source == Source::Unknown {
        return Redirect::to("/settings?tab=groups");
    }
    let confidence = form.confidence.unwrap_or(0.95).clamp(0.0, 1.0);
    let notes = form.notes.unwrap_or_default();
    let notes = notes.trim();

    match group_source_map::upsert_user_edit(&state.db, name, source, confidence, notes).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Group source updated: {}", name),
                source.as_str(),
            )
            .await;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Group source upsert failed",
                &e.to_string(),
            )
            .await;
        }
    }
    Redirect::to("/settings?tab=groups")
}

/// Delete a row from `group_source_map` by group name. Works on both seeded
/// and user-edited rows — seeded rows will be re-inserted on the next
/// startup via `seed_defaults`, so deletes of seeds are effectively a
/// one-session reset.
pub async fn settings_groups_delete(
    State(state): State<AppState>,
    Form(form): Form<GroupDeleteForm>,
) -> Redirect {
    let name = form.group_name.trim();
    if name.is_empty() {
        return Redirect::to("/settings?tab=groups");
    }
    match group_source_map::delete(&state.db, name).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Group source deleted: {}", name),
                "",
            )
            .await;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Group source delete failed",
                &e.to_string(),
            )
            .await;
        }
    }
    Redirect::to("/settings?tab=groups")
}

#[utoipa::path(
    post,
    path = "/api/qbit/test",
    tag = "System",
    summary = "Test qBittorrent connection",
    description = "Test connectivity to a qBittorrent instance with the provided credentials.",
    request_body = QbitTestForm,
    responses(
        (status = 200, description = "Connection successful", body = serde_json::Value),
        (status = 502, description = "Connection failed"),
    ),
)]
pub async fn qbit_test(
    Json(form): Json<QbitTestForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client: std::sync::Arc<dyn DownloadClient> = std::sync::Arc::new(QbitClient::new(
        form.qbit_url.trim(),
        form.qbit_user.trim(),
        &form.qbit_pass,
        form.qbit_category.as_deref().unwrap_or(""),
    ));

    match client.test().await {
        Ok(version) => Ok(Json(
            serde_json::json!({"ok": true, "message": format!("Connected to qBittorrent {}", version)}),
        )),
        Err(err) => Err((
            axum::http::StatusCode::BAD_GATEWAY,
            serde_json::json!({"ok": false, "message": err}).to_string(),
        )),
    }
}

#[utoipa::path(
    post,
    path = "/api/jellyfin/test",
    tag = "System",
    summary = "Test Jellyfin connection",
    description = "Test connectivity to a Jellyfin instance with the provided URL and API key.",
    request_body = JellyfinTestForm,
    responses(
        (status = 200, description = "Connection successful", body = serde_json::Value),
        (status = 502, description = "Connection failed"),
    ),
)]
pub async fn jellyfin_test(
    Json(form): Json<JellyfinTestForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = JellyfinClient::new(form.jellyfin_url.trim(), &form.jellyfin_api_key);

    match client.test_connection().await {
        Ok(info) => Ok(Json(serde_json::json!({
            "ok": true,
            "message": if info.server_name.trim().is_empty() {
                format!("Connected to Jellyfin {}", info.version)
            } else {
                format!("Connected to Jellyfin {} ({})", info.server_name, info.version)
            }
        }))),
        Err(err) => Err((
            axum::http::StatusCode::BAD_GATEWAY,
            serde_json::json!({"ok": false, "message": err}).to_string(),
        )),
    }
}

#[utoipa::path(
    get,
    path = "/api/health",
    tag = "System",
    summary = "Health check",
    description = "Returns connection status of the active download client and Jellyfin.",
    responses(
        (status = 200, description = "Health status", body = serde_json::Value),
    ),
)]
pub async fn api_health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let download_client_status = {
        let client = state.download_client.read().await.clone();
        match client {
            Some(c) => {
                // Emit `type` on both Ok and Err so the template JS
                // can route the Disconnected badge to the right
                // fieldset when test() fails (daemon down, wrong
                // creds). Without this, a configured-but-failing
                // client renders no badge at all.
                let impl_name = c.sonarr_impl_name();
                match c.test().await {
                    Ok(version) => serde_json::json!({
                        "ok": true,
                        "message": format!("{} {}", impl_name, version),
                        "type": impl_name,
                    }),
                    Err(e) => serde_json::json!({
                        "ok": false,
                        "message": e,
                        "type": impl_name,
                    }),
                }
            }
            None => serde_json::json!({"ok": false, "message": "Not configured"}),
        }
    };

    let jellyfin_status = {
        let client = state.jellyfin.read().await.clone();
        match client {
            Some(c) => match c.test_connection().await {
                Ok(info) => {
                    let label = if info.server_name.trim().is_empty() {
                        format!("Jellyfin {}", info.version)
                    } else {
                        format!("{} ({})", info.server_name, info.version)
                    };
                    serde_json::json!({"ok": true, "message": label})
                }
                Err(e) => serde_json::json!({"ok": false, "message": e}),
            },
            None => serde_json::json!({"ok": false, "message": "Not configured"}),
        }
    };

    Json(serde_json::json!({
        "download_client": download_client_status,
        "jellyfin": jellyfin_status,
    }))
}

#[utoipa::path(
    post,
    path = "/api/jellyfin/refresh",
    tag = "System",
    summary = "Refresh Jellyfin library",
    description = "Trigger a library scan in Jellyfin to pick up newly added media.",
    responses(
        (status = 200, description = "Library refresh triggered", body = serde_json::Value),
        (status = 400, description = "Jellyfin not configured"),
        (status = 502, description = "Refresh failed"),
    ),
)]
pub async fn jellyfin_refresh(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = {
        let jellyfin = state.jellyfin.read().await;
        jellyfin
            .as_ref()
            .ok_or((
                axum::http::StatusCode::BAD_REQUEST,
                "Jellyfin not configured".to_string(),
            ))?
            .clone()
    };

    client
        .refresh_library()
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    Ok(Json(
        serde_json::json!({"ok": true, "message": "Jellyfin library refresh queued"}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_label_strips_control_chars() {
        // A label with an embedded newline would survive through to
        // rtorrent's `d.custom1.set="..."` inline command and could
        // terminate the command early. sanitize_label strips any
        // control character before it can reach the wire.
        assert_eq!(sanitize_label("ryokan\nmalicious"), "ryokanmalicious");
        assert_eq!(sanitize_label("ry\tokan"), "ryokan");
        assert_eq!(sanitize_label("ryokan\0"), "ryokan");
        assert_eq!(sanitize_label("  ryokan  "), "ryokan");
    }

    #[test]
    fn sanitize_label_defaults_to_ryokan_when_empty_or_only_control() {
        assert_eq!(sanitize_label(""), "ryokan");
        assert_eq!(sanitize_label("   "), "ryokan");
        assert_eq!(sanitize_label("\n\t\r"), "ryokan");
    }

    #[test]
    fn sanitize_label_preserves_unicode_and_spaces() {
        // Only control characters are stripped — internal spaces and
        // non-ASCII characters (users' native-script labels) survive.
        assert_eq!(sanitize_label("anime batch"), "anime batch");
        assert_eq!(sanitize_label("アニメ"), "アニメ");
    }

    /// A Sonarr/Trash Guides CF that carries a `trash_description`
    /// should be surfaced verbatim so the edit drawer can render it.
    #[test]
    fn extract_trash_description_returns_string_when_present() {
        let json = serde_json::json!({
            "name": "Example",
            "trash_description": "This CF matches high-quality BluRay releases.",
            "specifications": []
        })
        .to_string();
        assert_eq!(
            extract_trash_description(&json),
            Some("This CF matches high-quality BluRay releases.".to_string())
        );
    }

    /// Absent, empty, whitespace-only, wrong-typed, or unparseable
    /// payloads should all return `None` so the template simply
    /// doesn't render the description block.
    #[test]
    fn extract_trash_description_returns_none_for_missing_or_invalid() {
        let no_field = serde_json::json!({"name": "X", "specifications": []}).to_string();
        assert_eq!(extract_trash_description(&no_field), None);

        let empty = serde_json::json!({"trash_description": ""}).to_string();
        assert_eq!(extract_trash_description(&empty), None);

        let whitespace = serde_json::json!({"trash_description": "   "}).to_string();
        assert_eq!(extract_trash_description(&whitespace), None);

        let wrong_type = serde_json::json!({"trash_description": 42}).to_string();
        assert_eq!(extract_trash_description(&wrong_type), None);

        assert_eq!(extract_trash_description("not json at all"), None);
    }
}
