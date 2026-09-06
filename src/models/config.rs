use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Legacy single-client discriminator. Pre-multi-client routing
    /// this picked which concrete `DownloadClient` impl `AppState`
    /// initialized at startup. Today the multi-client pool reads from
    /// the `download_clients` table instead (see `main.rs:529` and
    /// `services::download_client::rebuild_clients_cache`); this field
    /// is kept persisted only so the legacy Settings → Connections form
    /// can round-trip a value during the migration window. Accepts
    /// `"qbittorrent" | "deluge" | "transmission" | "rtorrent" | "sabnzbd"`;
    /// unknown strings coerce to `qbittorrent` on save.
    pub active_client: String,
    pub qbit_url: String,
    pub qbit_user: String,
    pub qbit_pass: String,
    pub qbit_category: String,
    /// Where Ryokan reads qBit's completed files from its own
    /// filesystem. Overrides whatever qBit itself reports as
    /// `save_path` — needed when qBit runs in a container (it sees
    /// `/downloads`, Ryokan-on-host sees e.g. `/home/user/downloads`)
    /// or on a seedbox (Ryokan reads via SSHFS/NFS mount).
    pub qbit_download_path: String,
    /// Deluge Web UI base URL (e.g. `http://host:8112`). The
    /// DelugeClient impl appends `/json` internally.
    pub deluge_url: String,
    pub deluge_password: String,
    /// Scoping label set on every Ryokan-owned torrent in Deluge.
    /// Defaults to `"ryokan"`; users can override if running multiple
    /// Ryokan instances against one Deluge.
    pub deluge_label: String,
    /// Per-client counterpart to `qbit_download_path` — where Ryokan
    /// reads Deluge's completed files from its own filesystem.
    /// Shape mirrors qbit_download_path exactly.
    pub deluge_download_path: String,
    /// Transmission RPC base URL (e.g. `http://host:9091`). The
    /// TransmissionClient impl appends `/transmission/rpc` internally.
    pub transmission_url: String,
    pub transmission_user: String,
    pub transmission_password: String,
    /// Scoping label set on every Ryokan-owned torrent in Transmission.
    /// Defaults to `"ryokan"`; users can override if running multiple
    /// Ryokan instances against one daemon.
    pub transmission_label: String,
    /// Per-client counterpart to `qbit_download_path`. Same shape.
    pub transmission_download_path: String,
    /// rtorrent XML-RPC endpoint URL (e.g.
    /// `http://seedbox.example.com/RPC2`). Taken verbatim — deployment
    /// shape varies too much (standalone rtorrent-xmlrpc-bin, ruTorrent
    /// bundled nginx, seedbox reverse-proxy) to infer a default path,
    /// so no auto-appended suffix the way DelugeClient appends `/json`.
    pub rtorrent_url: String,
    pub rtorrent_user: String,
    pub rtorrent_password: String,
    /// Scoping label stored in rtorrent's `custom1` field (ruTorrent's
    /// convention). Defaults to `"ryokan"`.
    pub rtorrent_label: String,
    /// Per-client counterpart to `qbit_download_path`. Same shape.
    pub rtorrent_download_path: String,
    pub jellyfin_url: String,
    pub jellyfin_api_key: String,
    pub preferred_groups: String,
    pub blocked_groups: String,
    pub preferred_resolution: String,
    pub preferred_source: String,
    pub cutoff_source: String,
    pub cutoff_resolution: String,
    /// Legacy combined preferred-quality field. Kept one release for
    /// rollback; read paths should prefer `preferred_source` +
    /// `preferred_resolution`.
    pub quality_profile: String,
    /// Legacy combined cutoff field. See `quality_profile`.
    pub quality_cutoff: String,
    pub finished_series_quality: String,
    pub media_root: String,
    pub title_language: String,
    pub force_mal_fallback: bool,
    pub rss_enabled: bool,
    pub rss_interval_minutes: i32,
    /// multi-rss commit E — master kill switch for the whole RSS
    /// sync loop. Off = no fetches at all (Nyaa + indexer-RSS +
    /// direct feeds), regardless of the per-source flags. Default
    /// `true`; existing installs keep their behavior. Distinct
    /// from `rss_enabled`, which retains its v1 semantics
    /// (Nyaa-only flag) — see plan decision #8.
    pub rss_master_enabled: bool,
    /// opt-out for Nyaa-specific RSS polling without
    /// disabling indexer-RSS (torznab/newznab) feeds. The user has
    /// other indexers configured and only wants those polled —
    /// before this flag, the only way to skip Nyaa was to disable
    /// `rss_enabled` entirely, which also killed the legacy v1 path
    /// the user might still want for non-Nyaa work. Default `false`
    /// so existing installs keep polling Nyaa.
    pub disable_nyaa_rss: bool,
    pub force_kitsu_fallback: bool,
    pub post_processing_enabled: bool,
    pub post_processing_mode: String,
    pub auto_grab_on_add: bool,
    /// v1.3.0 UX pass — when true, any update to a series's
    /// monitoring mode triggers an auto-search over the newly-
    /// monitored-and-airable episodes. Default off to preserve
    /// existing behavior.
    pub search_on_monitoring_change: bool,
    pub prefer_subs: bool,
    pub allow_non_english: bool,
    pub sonarr_enabled: bool,
    pub sonarr_api_key: String,
    pub radarr_enabled: bool,
    pub radarr_api_key: String,
    /// Issue #28 — API key for the autobrr push webhook at
    /// `POST /api/webhook/autobrr`. Empty string disables the
    /// webhook entirely (the route returns 503 + Retry-After).
    /// Generated via the Settings → Connections → autobrr panel
    /// when the user clicks "Generate key"; the user pastes the
    /// key into autobrr's Webhook action config.
    pub autobrr_api_key: String,
    pub upgrade_search_enabled: bool,
    /// Floor applied to `total_cf_score` after Custom Formats evaluation.
    /// `i32::MIN` (the default) means no floor. Raised by the user via
    /// the Custom Formats settings page to reject candidates whose
    /// summed CF score falls below the threshold.
    pub custom_format_minimum_score: i32,
    /// Apply the hardcoded SeaDex "best release" score boost
    /// (`SEADEX_SCORE_BOOST = 10_000`) at scoring time. Off by default.
    /// Suppressed automatically when the user has any
    /// `Ryokan.SeaDexBestSpecification` Custom Format installed — that
    /// CF replaces the hardcoded boost with a user-controlled score.
    pub seadex_enabled: bool,
    /// #23 — Global default extra tokens appended to every Nyaa query
    /// (e.g. `bd 1080p`). Per-series override on `series` takes
    /// precedence when set. Empty means no tokens.
    pub default_custom_query_tokens: String,
    /// #23 — Global default Nyaa **uploader** restriction. When
    /// non-empty, Ryokan sets `?u=<name>` on every Nyaa search so only
    /// that account's uploads come back. Much tighter than a
    /// `[Group]` title-contains filter: no third-party re-uploads,
    /// no filename-token false matches. Trade-off is that the name
    /// must match an actual Nyaa account — groups without a dedicated
    /// account (HorribleSubs, etc.) will return zero results and the
    /// user has to clear the field. Per-series override takes
    /// precedence. Empty means no restriction.
    pub default_restrict_to_uploader: String,
    /// Interactive file-picker trigger policy (issue #83). One of
    /// `"batches_only"` (default — multi-file torrents open the modal,
    /// single-file skip it) or `"never"` (no modal ever, legacy 1.3.0
    /// behavior). `"always"` is deliberately not a valid value per
    /// plan decision — single-file releases have nothing to pick. Any
    /// other string coerces to `batches_only` on save.
    pub grab_preview_mode: String,
    /// Issue #62 — watch-list sync cadence in minutes. Default
    /// 30 (decision #5). Valid range 15..=10080 (15 min .. 7 days);
    /// the supervised task clamps on read so a hand-edited DB row
    /// can't push the cadence into "rate-limit-pressure" or
    /// "effectively-disabled" territory. The settings save handler
    /// also enforces the range on input. No-op when no external
    /// account is linked, regardless of the value.
    pub external_sync_interval_minutes: i32,
    /// Multi-client routing — id of the `download_clients` row to
    /// route built-in Nyaa search hits through. `None` falls back
    /// to whichever row holds `is_default = 1`. Surfaced as a
    /// dropdown on the Indexers tab; stored alongside the row's
    /// `download_client_id` pin to preserve the same shape.
    pub nyaa_download_client_id: Option<i64>,
    /// Manual search page → grab. When ON, a grab from the search
    /// page that doesn't match an existing library series triggers
    /// an anitomy-parse → AL search → series auto-add path
    /// (`services::library_link::resolve_or_add_series_for_grab`).
    /// When OFF, a no-match grab succeeds in the download client
    /// but no library row is created (legacy 1.0–1.6 behavior).
    /// Default `true` so the search-page Grab button "just works"
    /// without a manual library-add round-trip.
    pub manual_search_auto_add: bool,
    /// Misgrab guardrails: when the files inside a grab clearly name a
    /// different series, remove the download from the client, blocklist
    /// the release, notify, and search again. Off keeps the download in
    /// the client and only flags it on System > Misgrabs.
    pub misgrab_auto_remove: bool,
    /// Import robustness (#205): hours a grab may sit "complete" in the
    /// download client without post-processing finding an importable
    /// video before it is marked failed. Measured from
    /// `grabbed_torrents.completed_seen_at`, not from the grab. `0`
    /// keeps the pre-#205 behavior (retry forever). Default 24.
    pub import_stall_hours: i64,
    /// Recycle bin root (#123). Empty = recycle disabled, every library
    /// delete is a permanent unlink. When set, episode / series-folder
    /// deletes and upgrade-replaced files move to
    /// `<recycle_bin_path>/<YYYY-MM-DD>/<entry_id>/` instead (see
    /// `services::recycle`). Same filesystem as `media_root` makes the
    /// move an instant rename; cross-filesystem degrades to copy+unlink.
    pub recycle_bin_path: String,
    /// Days a recycled entry survives before the hourly `cleanup` task
    /// purges its date bucket. `0` disables auto-purge (forever-recycle,
    /// manual "Empty recycle bin" only). Default 14.
    pub recycle_bin_age_days: i64,
    /// Naming templates (#124). See `services::naming` for the token
    /// language. The series-folder template applies once, at add time
    /// (`series::upsert`); the other two on every import. Empty is
    /// treated as the default at every read site.
    pub series_folder_format: String,
    pub season_folder_format: String,
    pub episode_file_format: String,
    /// Scheduled backups (#126): `disabled` / `daily` / `weekly`.
    pub backup_schedule: String,
    /// Empty = `<data dir>/backups`.
    pub backup_directory: String,
    pub backup_retention_count: i64,
    pub backup_include_artwork: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            active_client: "qbittorrent".to_string(),
            qbit_url: String::new(),
            qbit_user: String::new(),
            qbit_pass: String::new(),
            qbit_category: String::new(),
            qbit_download_path: String::new(),
            deluge_url: String::new(),
            deluge_password: String::new(),
            deluge_label: "ryokan".to_string(),
            deluge_download_path: String::new(),
            transmission_url: String::new(),
            transmission_user: String::new(),
            transmission_password: String::new(),
            transmission_label: "ryokan".to_string(),
            transmission_download_path: String::new(),
            rtorrent_url: String::new(),
            rtorrent_user: String::new(),
            rtorrent_password: String::new(),
            rtorrent_label: "ryokan".to_string(),
            rtorrent_download_path: String::new(),
            jellyfin_url: String::new(),
            jellyfin_api_key: String::new(),
            preferred_groups: String::new(),
            blocked_groups: String::new(),
            preferred_resolution: "1080".to_string(),
            preferred_source: "web".to_string(),
            cutoff_source: "bluray".to_string(),
            cutoff_resolution: "1080".to_string(),
            quality_profile: "web_1080".to_string(),
            quality_cutoff: "bd_1080".to_string(),
            finished_series_quality: "prefer_bd".to_string(),
            media_root: String::new(),
            title_language: "english".to_string(),
            force_mal_fallback: false,
            rss_enabled: false,
            rss_interval_minutes: 15,
            rss_master_enabled: true,
            disable_nyaa_rss: false,
            force_kitsu_fallback: false,
            post_processing_enabled: false,
            post_processing_mode: "hardlink".to_string(),
            auto_grab_on_add: true,
            search_on_monitoring_change: false,
            prefer_subs: true,
            allow_non_english: false,
            sonarr_enabled: false,
            sonarr_api_key: String::new(),
            radarr_enabled: false,
            radarr_api_key: String::new(),
            autobrr_api_key: String::new(),
            upgrade_search_enabled: false,
            custom_format_minimum_score: i32::MIN,
            seadex_enabled: false,
            default_custom_query_tokens: String::new(),
            default_restrict_to_uploader: String::new(),
            grab_preview_mode: "batches_only".to_string(),
            external_sync_interval_minutes: 30,
            nyaa_download_client_id: None,
            manual_search_auto_add: true,
            misgrab_auto_remove: true,
            import_stall_hours: 24,
            recycle_bin_path: String::new(),
            recycle_bin_age_days: 14,
            series_folder_format: crate::services::naming::DEFAULT_SERIES_FOLDER_FORMAT.to_string(),
            season_folder_format: crate::services::naming::DEFAULT_SEASON_FOLDER_FORMAT.to_string(),
            episode_file_format: crate::services::naming::DEFAULT_EPISODE_FILE_FORMAT.to_string(),
            backup_schedule: "disabled".to_string(),
            backup_directory: String::new(),
            backup_retention_count: 7,
            backup_include_artwork: false,
        }
    }
}

#[derive(Debug, FromRow)]
struct ConfigRow {
    active_client: String,
    qbit_url: String,
    qbit_user: String,
    qbit_pass: String,
    qbit_category: String,
    qbit_download_path: String,
    deluge_url: String,
    deluge_password: String,
    deluge_label: String,
    deluge_download_path: String,
    transmission_url: String,
    transmission_user: String,
    transmission_password: String,
    transmission_label: String,
    transmission_download_path: String,
    rtorrent_url: String,
    rtorrent_user: String,
    rtorrent_password: String,
    rtorrent_label: String,
    rtorrent_download_path: String,
    jellyfin_url: String,
    jellyfin_api_key: String,
    preferred_groups: String,
    blocked_groups: String,
    preferred_resolution: String,
    preferred_source: String,
    cutoff_source: String,
    cutoff_resolution: String,
    quality_profile: String,
    quality_cutoff: String,
    finished_series_quality: String,
    media_root: String,
    title_language: String,
    force_mal_fallback: i64,
    rss_enabled: i64,
    rss_interval_minutes: i64,
    rss_master_enabled: i64,
    disable_nyaa_rss: i64,
    force_kitsu_fallback: i64,
    post_processing_enabled: i64,
    post_processing_mode: String,
    auto_grab_on_add: i64,
    search_on_monitoring_change: i64,
    prefer_subs: i64,
    allow_non_english: i64,
    sonarr_enabled: i64,
    sonarr_api_key: String,
    radarr_enabled: i64,
    radarr_api_key: String,
    autobrr_api_key: String,
    upgrade_search_enabled: i64,
    custom_format_minimum_score: i64,
    seadex_enabled: i64,
    default_custom_query_tokens: String,
    default_restrict_to_uploader: String,
    grab_preview_mode: String,
    external_sync_interval_minutes: i64,
    nyaa_download_client_id: Option<i64>,
    manual_search_auto_add: i64,
    misgrab_auto_remove: i64,
    import_stall_hours: i64,
    recycle_bin_path: String,
    recycle_bin_age_days: i64,
    series_folder_format: String,
    season_folder_format: String,
    episode_file_format: String,
    backup_schedule: String,
    backup_directory: String,
    backup_retention_count: i64,
    backup_include_artwork: i64,
}

/// Cheap title-language lookup with a safe default. Used by every page
/// template whose `base.html`-extending render needs to bake the user's
/// preference into the pre-paint FOUC guard. Returns `"english"` on any
/// error (DB transient failure, no config row yet during first-run setup,
/// pre-auth pages where no config exists) so the inline script's
/// `data-title-language` attribute always has a non-empty value — the
/// CSS title-switcher selector is keyed off it.
pub async fn get_title_language(db: &SqlitePool) -> String {
    sqlx::query_scalar::<_, String>("SELECT title_language FROM config WHERE id = 1")
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "english".to_string())
}

/// Get the singleton config row.
/// The two config values the series-folder template needs at add time.
/// `series::upsert` reads this itself so its fifteen call sites don't
/// have to thread a `Config` through. Defaults on any error (no row yet
/// during first-run, transient failure) so an add never fails on naming.
#[derive(Clone, Debug)]
pub struct NamingPrefs {
    pub title_language: String,
    pub series_folder_format: String,
}

pub async fn get_naming_prefs(db: &SqlitePool) -> NamingPrefs {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT title_language, series_folder_format FROM config WHERE id = 1")
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    let defaults = Config::default();
    match row {
        Some((lang, fmt)) => NamingPrefs {
            title_language: if lang.is_empty() {
                defaults.title_language
            } else {
                lang
            },
            series_folder_format: if fmt.trim().is_empty() {
                defaults.series_folder_format
            } else {
                fmt
            },
        },
        None => NamingPrefs {
            title_language: defaults.title_language,
            series_folder_format: defaults.series_folder_format,
        },
    }
}

pub async fn get_config(db: &SqlitePool) -> Result<Option<Config>, sqlx::Error> {
    let row: Option<ConfigRow> = sqlx::query_as(
        "SELECT active_client, qbit_url, qbit_user, qbit_pass, qbit_category, qbit_download_path, deluge_url, deluge_password, deluge_label, deluge_download_path, transmission_url, transmission_user, transmission_password, transmission_label, transmission_download_path, rtorrent_url, rtorrent_user, rtorrent_password, rtorrent_label, rtorrent_download_path, jellyfin_url, jellyfin_api_key, preferred_groups, blocked_groups, preferred_resolution, preferred_source, cutoff_source, cutoff_resolution, quality_profile, quality_cutoff, finished_series_quality, media_root, title_language, force_mal_fallback, rss_enabled, rss_interval_minutes, rss_master_enabled, disable_nyaa_rss, force_kitsu_fallback, post_processing_enabled, post_processing_mode, auto_grab_on_add, search_on_monitoring_change, prefer_subs, allow_non_english, sonarr_enabled, sonarr_api_key, radarr_enabled, radarr_api_key, autobrr_api_key, upgrade_search_enabled, custom_format_minimum_score, seadex_enabled, default_custom_query_tokens, default_restrict_to_uploader, grab_preview_mode, external_sync_interval_minutes, nyaa_download_client_id, manual_search_auto_add, recycle_bin_path, recycle_bin_age_days, series_folder_format, season_folder_format, episode_file_format, backup_schedule, backup_directory, backup_retention_count, backup_include_artwork, misgrab_auto_remove, import_stall_hours FROM config WHERE id = 1",
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(|r| Config {
        active_client: r.active_client,
        qbit_url: r.qbit_url,
        qbit_user: r.qbit_user,
        qbit_pass: r.qbit_pass,
        qbit_category: r.qbit_category,
        qbit_download_path: r.qbit_download_path,
        deluge_url: r.deluge_url,
        deluge_password: r.deluge_password,
        deluge_label: r.deluge_label,
        deluge_download_path: r.deluge_download_path,
        transmission_url: r.transmission_url,
        transmission_user: r.transmission_user,
        transmission_password: r.transmission_password,
        transmission_label: r.transmission_label,
        transmission_download_path: r.transmission_download_path,
        rtorrent_url: r.rtorrent_url,
        rtorrent_user: r.rtorrent_user,
        rtorrent_password: r.rtorrent_password,
        rtorrent_label: r.rtorrent_label,
        rtorrent_download_path: r.rtorrent_download_path,
        jellyfin_url: r.jellyfin_url,
        jellyfin_api_key: r.jellyfin_api_key,
        preferred_groups: r.preferred_groups,
        blocked_groups: r.blocked_groups,
        preferred_resolution: r.preferred_resolution,
        preferred_source: r.preferred_source,
        cutoff_source: r.cutoff_source,
        cutoff_resolution: r.cutoff_resolution,
        quality_profile: r.quality_profile,
        quality_cutoff: r.quality_cutoff,
        finished_series_quality: r.finished_series_quality,
        media_root: r.media_root,
        title_language: r.title_language,
        force_mal_fallback: r.force_mal_fallback != 0,
        rss_enabled: r.rss_enabled != 0,
        rss_interval_minutes: r.rss_interval_minutes as i32,
        rss_master_enabled: r.rss_master_enabled != 0,
        disable_nyaa_rss: r.disable_nyaa_rss != 0,
        force_kitsu_fallback: r.force_kitsu_fallback != 0,
        post_processing_enabled: r.post_processing_enabled != 0,
        post_processing_mode: r.post_processing_mode,
        auto_grab_on_add: r.auto_grab_on_add != 0,
        search_on_monitoring_change: r.search_on_monitoring_change != 0,
        prefer_subs: r.prefer_subs != 0,
        allow_non_english: r.allow_non_english != 0,
        sonarr_enabled: r.sonarr_enabled != 0,
        sonarr_api_key: r.sonarr_api_key,
        radarr_enabled: r.radarr_enabled != 0,
        radarr_api_key: r.radarr_api_key,
        autobrr_api_key: r.autobrr_api_key,
        upgrade_search_enabled: r.upgrade_search_enabled != 0,
        custom_format_minimum_score: r.custom_format_minimum_score as i32,
        seadex_enabled: r.seadex_enabled != 0,
        default_custom_query_tokens: r.default_custom_query_tokens,
        default_restrict_to_uploader: r.default_restrict_to_uploader,
        grab_preview_mode: r.grab_preview_mode,
        external_sync_interval_minutes: r.external_sync_interval_minutes as i32,
        nyaa_download_client_id: r.nyaa_download_client_id,
        manual_search_auto_add: r.manual_search_auto_add != 0,
        misgrab_auto_remove: r.misgrab_auto_remove != 0,
        import_stall_hours: r.import_stall_hours,
        recycle_bin_path: r.recycle_bin_path,
        recycle_bin_age_days: r.recycle_bin_age_days,
        series_folder_format: r.series_folder_format,
        season_folder_format: r.season_folder_format,
        episode_file_format: r.episode_file_format,
        backup_schedule: r.backup_schedule,
        backup_directory: r.backup_directory,
        backup_retention_count: r.backup_retention_count,
        backup_include_artwork: r.backup_include_artwork != 0,
    }))
}

/// Upsert the config row.
pub async fn save_config(db: &SqlitePool, config: &Config) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO config (id, active_client, qbit_url, qbit_user, qbit_pass, qbit_category, qbit_download_path, deluge_url, deluge_password, deluge_label, deluge_download_path, transmission_url, transmission_user, transmission_password, transmission_label, transmission_download_path, rtorrent_url, rtorrent_user, rtorrent_password, rtorrent_label, rtorrent_download_path, jellyfin_url, jellyfin_api_key, preferred_groups, blocked_groups, preferred_resolution, preferred_source, cutoff_source, cutoff_resolution, quality_profile, quality_cutoff, finished_series_quality, media_root, title_language, force_mal_fallback, rss_enabled, rss_interval_minutes, rss_master_enabled, disable_nyaa_rss, force_kitsu_fallback, post_processing_enabled, post_processing_mode, auto_grab_on_add, search_on_monitoring_change, prefer_subs, allow_non_english, sonarr_enabled, sonarr_api_key, radarr_enabled, radarr_api_key, autobrr_api_key, upgrade_search_enabled, custom_format_minimum_score, seadex_enabled, default_custom_query_tokens, default_restrict_to_uploader, grab_preview_mode, external_sync_interval_minutes, manual_search_auto_add, recycle_bin_path, recycle_bin_age_days, series_folder_format, season_folder_format, episode_file_format, backup_schedule, backup_directory, backup_retention_count, backup_include_artwork, misgrab_auto_remove, import_stall_hours)
        VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            active_client = excluded.active_client,
            qbit_url = excluded.qbit_url,
            qbit_user = excluded.qbit_user,
            qbit_pass = excluded.qbit_pass,
            qbit_category = excluded.qbit_category,
            qbit_download_path = excluded.qbit_download_path,
            deluge_url = excluded.deluge_url,
            deluge_password = excluded.deluge_password,
            deluge_label = excluded.deluge_label,
            deluge_download_path = excluded.deluge_download_path,
            transmission_url = excluded.transmission_url,
            transmission_user = excluded.transmission_user,
            transmission_password = excluded.transmission_password,
            transmission_label = excluded.transmission_label,
            transmission_download_path = excluded.transmission_download_path,
            rtorrent_url = excluded.rtorrent_url,
            rtorrent_user = excluded.rtorrent_user,
            rtorrent_password = excluded.rtorrent_password,
            rtorrent_label = excluded.rtorrent_label,
            rtorrent_download_path = excluded.rtorrent_download_path,
            jellyfin_url = excluded.jellyfin_url,
            jellyfin_api_key = excluded.jellyfin_api_key,
            preferred_groups = excluded.preferred_groups,
            blocked_groups = excluded.blocked_groups,
            preferred_resolution = excluded.preferred_resolution,
            preferred_source = excluded.preferred_source,
            cutoff_source = excluded.cutoff_source,
            cutoff_resolution = excluded.cutoff_resolution,
            quality_profile = excluded.quality_profile,
            quality_cutoff = excluded.quality_cutoff,
            finished_series_quality = excluded.finished_series_quality,
            media_root = excluded.media_root,
            title_language = excluded.title_language,
            force_mal_fallback = excluded.force_mal_fallback,
            rss_enabled = excluded.rss_enabled,
            rss_interval_minutes = excluded.rss_interval_minutes,
            rss_master_enabled = excluded.rss_master_enabled,
            disable_nyaa_rss = excluded.disable_nyaa_rss,
            force_kitsu_fallback = excluded.force_kitsu_fallback,
            post_processing_enabled = excluded.post_processing_enabled,
            post_processing_mode = excluded.post_processing_mode,
            auto_grab_on_add = excluded.auto_grab_on_add,
            search_on_monitoring_change = excluded.search_on_monitoring_change,
            prefer_subs = excluded.prefer_subs,
            allow_non_english = excluded.allow_non_english,
            sonarr_enabled = excluded.sonarr_enabled,
            sonarr_api_key = excluded.sonarr_api_key,
            radarr_enabled = excluded.radarr_enabled,
            radarr_api_key = excluded.radarr_api_key,
            autobrr_api_key = excluded.autobrr_api_key,
            upgrade_search_enabled = excluded.upgrade_search_enabled,
            custom_format_minimum_score = excluded.custom_format_minimum_score,
            seadex_enabled = excluded.seadex_enabled,
            default_custom_query_tokens = excluded.default_custom_query_tokens,
            default_restrict_to_uploader = excluded.default_restrict_to_uploader,
            grab_preview_mode = excluded.grab_preview_mode,
            external_sync_interval_minutes = excluded.external_sync_interval_minutes,
            manual_search_auto_add = excluded.manual_search_auto_add,
            recycle_bin_path = excluded.recycle_bin_path,
            recycle_bin_age_days = excluded.recycle_bin_age_days,
            series_folder_format = excluded.series_folder_format,
            season_folder_format = excluded.season_folder_format,
            episode_file_format = excluded.episode_file_format,
            backup_schedule = excluded.backup_schedule,
            backup_directory = excluded.backup_directory,
            backup_retention_count = excluded.backup_retention_count,
            backup_include_artwork = excluded.backup_include_artwork,
            misgrab_auto_remove = excluded.misgrab_auto_remove,
            import_stall_hours = excluded.import_stall_hours
        "#,
    )
    .bind(&config.active_client)
    .bind(&config.qbit_url)
    .bind(&config.qbit_user)
    .bind(&config.qbit_pass)
    .bind(&config.qbit_category)
    .bind(&config.qbit_download_path)
    .bind(&config.deluge_url)
    .bind(&config.deluge_password)
    .bind(&config.deluge_label)
    .bind(&config.deluge_download_path)
    .bind(&config.transmission_url)
    .bind(&config.transmission_user)
    .bind(&config.transmission_password)
    .bind(&config.transmission_label)
    .bind(&config.transmission_download_path)
    .bind(&config.rtorrent_url)
    .bind(&config.rtorrent_user)
    .bind(&config.rtorrent_password)
    .bind(&config.rtorrent_label)
    .bind(&config.rtorrent_download_path)
    .bind(&config.jellyfin_url)
    .bind(&config.jellyfin_api_key)
    .bind(&config.preferred_groups)
    .bind(&config.blocked_groups)
    .bind(&config.preferred_resolution)
    .bind(&config.preferred_source)
    .bind(&config.cutoff_source)
    .bind(&config.cutoff_resolution)
    .bind(&config.quality_profile)
    .bind(&config.quality_cutoff)
    .bind(&config.finished_series_quality)
    .bind(&config.media_root)
    .bind(&config.title_language)
    .bind(if config.force_mal_fallback { 1_i64 } else { 0_i64 })
    .bind(if config.rss_enabled { 1_i64 } else { 0_i64 })
    .bind(config.rss_interval_minutes as i64)
    .bind(if config.rss_master_enabled { 1_i64 } else { 0_i64 })
    .bind(if config.disable_nyaa_rss { 1_i64 } else { 0_i64 })
    .bind(if config.force_kitsu_fallback { 1_i64 } else { 0_i64 })
    .bind(if config.post_processing_enabled { 1_i64 } else { 0_i64 })
    .bind(&config.post_processing_mode)
    .bind(if config.auto_grab_on_add { 1_i64 } else { 0_i64 })
    .bind(if config.search_on_monitoring_change {
        1_i64
    } else {
        0_i64
    })
    .bind(if config.prefer_subs { 1_i64 } else { 0_i64 })
    .bind(if config.allow_non_english { 1_i64 } else { 0_i64 })
    .bind(if config.sonarr_enabled { 1_i64 } else { 0_i64 })
    .bind(&config.sonarr_api_key)
    .bind(if config.radarr_enabled { 1_i64 } else { 0_i64 })
    .bind(&config.radarr_api_key)
    .bind(&config.autobrr_api_key)
    .bind(if config.upgrade_search_enabled { 1_i64 } else { 0_i64 })
    .bind(config.custom_format_minimum_score as i64)
    .bind(if config.seadex_enabled { 1_i64 } else { 0_i64 })
    .bind(&config.default_custom_query_tokens)
    .bind(&config.default_restrict_to_uploader)
    .bind(&config.grab_preview_mode)
    .bind(config.external_sync_interval_minutes as i64)
    .bind(if config.manual_search_auto_add {
        1_i64
    } else {
        0_i64
    })
    .bind(&config.recycle_bin_path)
    .bind(config.recycle_bin_age_days)
    .bind(&config.series_folder_format)
    .bind(&config.season_folder_format)
    .bind(&config.episode_file_format)
    .bind(&config.backup_schedule)
    .bind(&config.backup_directory)
    .bind(config.backup_retention_count)
    .bind(if config.backup_include_artwork {
        1_i64
    } else {
        0_i64
    })
    .bind(if config.misgrab_auto_remove { 1_i64 } else { 0_i64 })
    .bind(config.import_stall_hours)
    .execute(db)
    .await?;

    Ok(())
}
