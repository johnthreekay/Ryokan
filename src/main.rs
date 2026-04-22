mod handlers;
mod models;
mod services;

use axum::http::{HeaderValue, header};
use axum::{
    Router,
    extract::{DefaultBodyLimit, FromRef},
    middleware,
    routing::{get, post},
};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use services::{
    custom_formats::{self, CompiledCfCache},
    download_client::DownloadClient,
    jellyfin::JellyfinClient,
    progress::ProgressRegistry,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Ryokan API",
        version = "0.1.0",
        description = "Self-hosted anime PVR — search, download, and manage your anime library.",
    ),
    paths(
        // Library
        handlers::library::search::anilist_search,
        handlers::library::search::api_series_detail,
        handlers::library::crud::add_series,
        handlers::library::crud::remove_series,
        handlers::library::crud::reconcile_fallbacks,
        handlers::library::crud::set_folder,
        handlers::library::crud::set_monitoring,
        handlers::library::crud::set_episode_monitoring,
        handlers::library::crud::set_allow_upgrades,
        handlers::library::crud::set_search_overrides,
        handlers::library::crud::set_manual_override,
        handlers::library::crud::reclassify_episode,
        handlers::library::crud::list_folders,
        handlers::library::search::auto_search_series,
        handlers::library::search::auto_search_episode,
        handlers::library::search::search_batch_releases,
        handlers::library::search::interactive_search_episode,
        handlers::library::search::interactive_search_batches,
        handlers::library::search::grab_interactive_result,
        handlers::library::search::grab_batch_result,
        handlers::library::episodes::delete_episode_file,
        handlers::library::episodes::cancel_pending_episode,
        handlers::library::episodes::get_episode_grab_history,
        handlers::library::episodes::mark_episode_failed,
        handlers::library::episodes::episode_download_progress,
        handlers::library::episodes::series_episodes_json,
        // Search
        handlers::search::search_page_api,
        handlers::search::grab_release,
        handlers::search::get_torrents,
        // Downloads
        handlers::downloads::api_pause_torrent,
        handlers::downloads::api_resume_torrent,
        handlers::downloads::api_delete_torrent,
        handlers::downloads::api_blocklist_remove,
        // System
        handlers::settings::api_health,
        handlers::settings::qbit_test,
        handlers::settings::jellyfin_test,
        handlers::settings::jellyfin_refresh,
        // Settings — Custom Formats
        handlers::settings::custom_formats::settings_custom_formats_upsert,
        handlers::settings::custom_formats::settings_custom_formats_delete,
        handlers::settings::custom_formats::settings_custom_formats_minimum_score,
        handlers::settings::custom_formats::settings_custom_formats_import,
        handlers::settings::custom_formats::settings_custom_formats_import_resolve,
        handlers::settings::custom_formats::settings_custom_formats_install_defaults,
        handlers::settings::custom_formats::settings_custom_formats_reset_defaults,
        handlers::settings::custom_formats::settings_custom_formats_export,
        handlers::settings::custom_formats::settings_custom_formats_test,
        handlers::system::api_logs_poll,
        handlers::system::api_logs_clear,
        handlers::system::api_logs_client,
        handlers::progress::poll_progress,
        handlers::system::api_rss_sync,
        handlers::system::api_rss_clear_history,
        handlers::system::api_force_metadata_refresh,
        handlers::system::api_force_cleanup,
        handlers::system::api_force_post_processing,
        handlers::system::api_force_library_classify,
        handlers::system::api_force_upgrade_search,
        handlers::system::api_rebuild_cached_metadata,
        handlers::system::api_anibridge_reload,
    ),
    components(schemas(
        services::anilist::AnimeEntry,
        services::anilist::AnimeDetail,
        services::anilist::RelatedEntry,
        services::anilist::StreamingEpisode,
        services::nyaa::SearchResult,
        services::nyaa::SearchResponse,
        services::download_client::DownloadItem,
        services::auto_search::AutoSearchReport,
        services::auto_search::AutoSearchHit,
        services::progress::ProgressEvent,
        services::progress::ProgressPoll,
        models::log::LogEntry,
        models::episode_tags::GrabHistoryEntry,
        handlers::system::ClientLogForm,
        handlers::library::AddSeriesForm,
        handlers::library::RemoveSeriesForm,
        handlers::library::SetFolderForm,
        handlers::library::SetMonitoringForm,
        handlers::library::SetEpisodeMonitoringForm,
        handlers::library::SetAllowUpgradesForm,
        handlers::library::SetManualOverrideForm,
        handlers::library::BulkManualOverrideForm,
        handlers::library::ReclassifyEpisodeForm,
        handlers::library::MarkEpisodeFailedForm,
        handlers::library::episodes::EpisodeProgress,
        handlers::search::GrabForm,
        handlers::downloads::TorrentActionForm,
        handlers::downloads::TorrentDeleteForm,
        handlers::downloads::BlocklistRemoveForm,
        handlers::settings::QbitTestForm,
        handlers::settings::JellyfinTestForm,
        handlers::settings::custom_formats::CustomFormatUpsertForm,
        handlers::settings::custom_formats::CfTestRequest,
        handlers::settings::custom_formats::CustomFormatDeleteForm,
        handlers::settings::custom_formats::CustomFormatMinScoreForm,
        handlers::settings::custom_formats::CustomFormatImportForm,
    )),
    tags(
        (name = "Library", description = "Anime library management — add, remove, search, and monitor series"),
        (name = "Search", description = "Nyaa torrent search and grabbing"),
        (name = "Downloads", description = "qBittorrent download management"),
        (name = "System", description = "Health checks, logs, RSS sync, and background tasks"),
        (name = "Settings", description = "Settings management — Custom Formats CRUD, import/export, and scoring thresholds"),
    ),
)]
struct ApiDoc;

/// Shared application state available to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub download_client: Arc<RwLock<Option<Arc<dyn DownloadClient>>>>,
    pub jellyfin: Arc<RwLock<Option<JellyfinClient>>>,
    /// Compiled Custom Formats, loaded once at startup and rebuilt on
    /// CF create/update/delete via `custom_formats::rebuild_cf_cache`.
    /// Outer `RwLock` owns swap; the inner `Arc<Vec<_>>` is cheap-cloned
    /// out on the scoring hot path so the read lock releases before the
    /// per-candidate evaluation loop begins.
    pub custom_formats: CompiledCfCache,
    /// In-memory progress registry for long-running user-triggered jobs
    /// (currently the manual auto-search). The frontend mints an opaque
    /// `progress_id`, the trigger handler binds it via
    /// `register(...).await`, and the polling endpoint at
    /// `/api/progress/{id}` drains buffered events. See
    /// `services::progress` for the full lifecycle.
    pub progress: ProgressRegistry,
    /// Flip-to-true-once cache of `user::has_users`. The auth middleware
    /// runs on every protected request and was firing a `SELECT COUNT(*)
    /// FROM users` query for each one just to decide whether to redirect
    /// to `/setup`. Because Ryokan never deletes the admin account, once
    /// this flag is true it stays true for the life of the process, and
    /// the check becomes a lock-free atomic load. While false, the
    /// middleware still hits the DB on the setup-pending path so a fresh
    /// `/setup` submission is picked up on the very next request.
    pub users_exist: Arc<std::sync::atomic::AtomicBool>,
}

// Allow handlers to extract SqlitePool directly from AppState.
impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> SqlitePool {
        state.db.clone()
    }
}

/// Run a supervising loop around a background tick future.
///
/// `make_fut` is called once per respawn. The returned future is run on
/// its own nested `tokio::spawn`, so if the inner task panics tokio
/// catches the unwind at the task boundary and surfaces it as a
/// `JoinError` — we log it, sleep briefly, and respawn. Without this
/// supervising layer a stray `.unwrap()` or overflow inside any one
/// background task would silently kill the task for the rest of the
/// process lifetime, leaving the operator with a "task X stopped firing
/// three days ago" mystery bug.
///
/// `name` is used purely in the log line so the operator can tell which
/// task misbehaved.
/// Build a `Router` that registers the same handler at multiple
/// path aliases. Used by the Sonarr/Radarr compat router setup to
/// collapse case-variant doublings (`qualityprofile` vs
/// `qualityProfile`, etc. — Seerr ships both spellings depending on
/// version) into one logical line per endpoint.
fn aliased(paths: &[&str], handler: axum::routing::MethodRouter<AppState>) -> Router<AppState> {
    let mut router = Router::new();
    for path in paths {
        router = router.route(path, handler.clone());
    }
    router
}

async fn supervise<F, Fut>(name: &'static str, mut make_fut: F) -> !
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    // Exponential backoff for crash loops. A task that exits in <60s
    // before its next restart is considered "unhealthy" — double the
    // backoff (capped at MAX) so a stuck-on-startup task can't spam
    // logs at 12 restarts/minute. A task that runs for ≥60s before
    // exiting resets the backoff to MIN, so a one-off transient
    // failure doesn't punish the next restart.
    const MIN_BACKOFF: Duration = Duration::from_secs(5);
    const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
    const HEALTHY_RUNTIME: Duration = Duration::from_secs(60);

    let mut backoff = MIN_BACKOFF;
    loop {
        let started = Instant::now();
        let handle = tokio::spawn(make_fut());
        match handle.await {
            Err(e) if e.is_panic() => {
                tracing::error!("Background task '{}' panicked: {:?}", name, e);
            }
            Err(e) => {
                tracing::error!("Background task '{}' join error: {:?}", name, e);
            }
            Ok(()) => {
                tracing::warn!("Background task '{}' exited normally", name);
            }
        }

        if started.elapsed() >= HEALTHY_RUNTIME {
            backoff = MIN_BACKOFF;
        } else {
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
        tracing::warn!("supervise '{}': restarting in {:?}", name, backoff);
        tokio::time::sleep(backoff).await;
    }
}

#[tokio::main]
async fn main() {
    // Initialize tracing.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ryokan=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // #3b — write-side floor for the DB-backed `logs` table. Separate
    // from RUST_LOG (which controls what reaches the console). Default
    // Info keeps existing behavior; raise to `warn` or `error` to
    // shrink the System → Logs table; lower to `debug`/`trace` when
    // diagnosing. Read once at startup; no runtime toggle.
    if let Ok(raw) = std::env::var("RYOKAN_DB_LOG_LEVEL") {
        let level = models::log::LogLevel::from_str(raw.trim());
        services::logger::set_min_db_log_level(level);
        tracing::info!(min_db_log_level = level.as_str(), "DB log floor set");
    }

    // Database setup.
    // For local `cargo run`, default to a project-local ./data directory. Docker can
    // still override this with DATABASE_URL=sqlite:///data/ryokan.db?mode=rwc.
    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        let _ = std::fs::create_dir_all("data");
        "sqlite://data/ryokan.db?mode=rwc".to_string()
    });

    // WAL mode lets readers run concurrently with a writer, which matters a
    // lot here: seven background tasks (rss_sync, post_processing, cleanup,
    // library_classify, metadata_refresh, upgrade_search, anibridge_refresh)
    // all share this pool with the request path. In the default DELETE
    // journal mode every writer takes a whole-database lock and stalls the
    // next page load behind whatever scheduled_tasks row update or log insert
    // happens to be running. `synchronous=NORMAL` is safe under WAL (durable
    // across application crashes; the usual caveat is only crash-safe across
    // OS power loss, which matches what Sonarr/Radarr ship). The pragmas
    // below size the page cache and enable mmap'd reads so hot tables stay
    // in memory on subsequent lookups.
    let connect_opts = SqliteConnectOptions::from_str(&database_url)
        .expect("Invalid DATABASE_URL")
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .pragma("cache_size", "-65536") // ~64MB page cache (negative = KB)
        .pragma("temp_store", "MEMORY")
        .pragma("mmap_size", "268435456"); // 256MB memory-mapped region

    let db = SqlitePoolOptions::new()
        .max_connections(16)
        .connect_with(connect_opts)
        .await
        .expect("Failed to connect to database");

    // Run migrations.
    models::migrate(&db)
        .await
        .expect("Failed to run migrations");

    // Password-recovery boot path (#22). When RYOKAN_RESET_AUTH=1 or
    // --reset-auth is passed AND a `data/.reset-auth` sentinel file exists,
    // wipe users + sessions before the router mounts. `has_users()` then
    // returns false and `/setup` re-renders, letting the locked-out user
    // re-create the admin account.
    //
    // The sentinel file is the footgun guard: without it, a stuck-on
    // env var in a compose file would wipe auth on every boot. Users
    // touch the sentinel for a one-shot recovery, then remove it after
    // logging back in. Config (Jellyfin / qBit / media_root) is NOT
    // touched — only the admin account needs to be reset.
    let reset_auth_requested = std::env::var("RYOKAN_RESET_AUTH")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || std::env::args().any(|a| a == "--reset-auth");
    if reset_auth_requested {
        let sentinel = std::path::Path::new("data/.reset-auth");
        if sentinel.exists() {
            tracing::warn!(
                "RYOKAN_RESET_AUTH is set and data/.reset-auth sentinel present; \
                 wiping users and sessions. Remove the sentinel and unset the \
                 env var after logging back in."
            );
            if let Err(e) = models::user::reset_all(&db).await {
                tracing::error!("reset_all failed: {e}");
            }
        } else {
            tracing::warn!(
                "RYOKAN_RESET_AUTH is set but data/.reset-auth sentinel is missing; \
                 refusing to reset auth. See /forgot-password for the recovery recipe."
            );
        }
    }

    // Warm the bcrypt dummy-hash LazyLock so the first failed-username login
    // probe doesn't pay a cold-start ~50ms extra on top of the normal bcrypt
    // cost — that extra delay on the very first probe would itself be a
    // timing side channel distinguishing "cold process" from "warm process".
    // Run it in a blocking task so the ~50ms bcrypt::hash doesn't stall the
    // runtime worker during startup.
    let _ = tokio::task::spawn_blocking(models::user::warm_timing_equalizer).await;

    // Warm the Custom Formats cache from disk. Parse failures are logged
    // inside `load_compiled_cfs` and skipped — startup never aborts over
    // a bad CF row, so a corrupted import can't take the server down.
    let cf_cache: CompiledCfCache = Arc::new(RwLock::new(Arc::new(
        custom_formats::load_compiled_cfs(&db).await,
    )));

    // Warm the SeaDex lookup cache from SQLite so a restart doesn't
    // re-hit releases.moe for every series in the library on the next
    // RSS sweep. Failures are logged and ignored — a cold cache just
    // means the first lookup per series pays the round-trip again.
    services::auto_search::seadex_warm_cache_from_db(&db).await;

    // Prime the `users_exist` cache at startup so a running instance with
    // an existing admin account never pays the `SELECT COUNT(*) FROM users`
    // cost on the auth hot path.
    let users_exist_initial = models::user::has_users(&db).await.unwrap_or(false);
    let users_exist = Arc::new(std::sync::atomic::AtomicBool::new(users_exist_initial));

    // Build shared state.
    let state = AppState {
        db: db.clone(),
        download_client: Arc::new(RwLock::new(None)),
        jellyfin: Arc::new(RwLock::new(None)),
        custom_formats: cf_cache,
        progress: ProgressRegistry::new(),
        users_exist,
    };

    // Initialize download client from saved config. Branches on
    // `config.active_client` — the per-client credential columns
    // (qbit_*, deluge_*) coexist on the same `config` row so a user
    // can switch between them in Settings without losing the other's
    // setup. Phase 3+ will add transmission/rtorrent arms here.
    if let Ok(Some(config)) = models::config::get_config(&db).await {
        let client = services::download_client::build_download_client(&config);
        if client.is_some() {
            *state.download_client.write().await = client;
        }
        if !config.jellyfin_url.is_empty() && !config.jellyfin_api_key.is_empty() {
            let client = JellyfinClient::new(&config.jellyfin_url, &config.jellyfin_api_key);
            *state.jellyfin.write().await = Some(client);
        }
    }

    // Routes that don't require auth. The CSRF layer applies to POSTs here
    // so a drive-by cross-origin /setup or /login submission is rejected
    // before touching the handler — the GET paths skip the check because
    // safe methods return Ok(()) from verify_same_origin.
    let public_routes = Router::new()
        .route(
            "/login",
            get(handlers::auth::login_page).post(handlers::auth::login_submit),
        )
        .route(
            "/setup",
            get(handlers::auth::setup_page).post(handlers::auth::setup_submit),
        )
        // #39 — Account recovery page. Linked from /login's "Forgot
        // password?" so a locked-out user can read the recipe without
        // needing to authenticate first. Dedicated template so they
        // don't see the authed top-nav / Logout link they can't use.
        .route(
            "/forgot-password",
            get(handlers::auth::forgot_password_page),
        )
        .layer(middleware::from_fn(handlers::auth::csrf_public));

    // Routes that require auth.
    let protected_routes = Router::new()
        .route("/", get(handlers::library::pages::index))
        .route(
            "/library/review",
            get(handlers::library::pages::needs_review_page),
        )
        .route(
            "/series/{anilist_id}",
            get(handlers::library::pages::series_detail),
        )
        .route(
            "/search",
            get(handlers::search::search_page).post(handlers::search::search_submit),
        )
        .route(
            "/api/anilist/search",
            get(handlers::library::search::anilist_search),
        )
        .route(
            "/api/library/add",
            post(handlers::library::crud::add_series),
        )
        .route(
            "/api/library/remove",
            post(handlers::library::crud::remove_series),
        )
        .route(
            "/api/library/reconcile-fallbacks",
            post(handlers::library::crud::reconcile_fallbacks),
        )
        .route(
            "/api/series/{anilist_id}",
            get(handlers::library::search::api_series_detail),
        )
        .route(
            "/api/library/folder",
            post(handlers::library::crud::set_folder),
        )
        .route(
            "/api/library/monitoring",
            post(handlers::library::crud::set_monitoring),
        )
        .route(
            "/api/library/episode-monitoring",
            post(handlers::library::crud::set_episode_monitoring),
        )
        .route(
            "/api/library/allow-upgrades",
            post(handlers::library::crud::set_allow_upgrades),
        )
        .route(
            "/api/library/search-overrides",
            post(handlers::library::crud::set_search_overrides),
        )
        .route(
            "/api/library/manual-override",
            post(handlers::library::crud::set_manual_override),
        )
        .route(
            "/api/library/bulk-manual-override",
            post(handlers::library::crud::bulk_manual_override),
        )
        .route(
            "/api/library/reclassify-episode",
            post(handlers::library::crud::reclassify_episode),
        )
        .route(
            "/api/series/{anilist_id}/auto-search",
            post(handlers::library::search::auto_search_series),
        )
        .route(
            "/api/series/{anilist_id}/auto-search/{episode_number}",
            post(handlers::library::search::auto_search_episode),
        )
        .route(
            "/api/series/{anilist_id}/search-batch",
            post(handlers::library::search::search_batch_releases),
        )
        .route(
            "/api/series/{anilist_id}/interactive-search/{episode_number}",
            get(handlers::library::search::interactive_search_episode),
        )
        .route(
            "/api/series/{anilist_id}/interactive-search-batch",
            get(handlers::library::search::interactive_search_batches),
        )
        .route(
            "/api/series/{anilist_id}/grab/{episode_number}",
            post(handlers::library::search::grab_interactive_result),
        )
        .route(
            "/api/series/{anilist_id}/grab-batch",
            post(handlers::library::search::grab_batch_result),
        )
        .route(
            "/api/series/{anilist_id}/delete-file/{episode_number}",
            post(handlers::library::episodes::delete_episode_file),
        )
        .route(
            "/api/series/{anilist_id}/cancel-pending/{episode_number}",
            post(handlers::library::episodes::cancel_pending_episode),
        )
        .route(
            "/api/series/{anilist_id}/grab-history/{episode_number}",
            get(handlers::library::episodes::get_episode_grab_history),
        )
        .route(
            "/api/series/{anilist_id}/mark-failed/{episode_number}",
            post(handlers::library::episodes::mark_episode_failed),
        )
        .route(
            "/api/series/{anilist_id}/download-progress",
            get(handlers::library::episodes::episode_download_progress),
        )
        .route(
            "/api/series/{anilist_id}/episodes",
            get(handlers::library::episodes::series_episodes_json),
        )
        .route(
            "/api/library/folders",
            get(handlers::library::crud::list_folders),
        )
        .route("/api/grab", post(handlers::search::grab_release))
        .route("/api/search/page", get(handlers::search::search_page_api))
        .route("/api/torrents", get(handlers::search::get_torrents))
        .route("/downloads", get(handlers::downloads::downloads_page))
        .route(
            "/api/downloads/pause",
            post(handlers::downloads::api_pause_torrent),
        )
        .route(
            "/api/downloads/resume",
            post(handlers::downloads::api_resume_torrent),
        )
        .route(
            "/api/downloads/delete",
            post(handlers::downloads::api_delete_torrent),
        )
        .route(
            "/api/downloads/blocklist/remove",
            post(handlers::downloads::api_blocklist_remove),
        )
        .route(
            "/settings",
            get(handlers::settings::settings_page).post(handlers::settings::settings_submit),
        )
        .route(
            "/settings/groups",
            post(handlers::settings::settings_groups_upsert),
        )
        .route(
            "/settings/groups/delete",
            post(handlers::settings::settings_groups_delete),
        )
        .route(
            "/settings/custom-formats/upsert",
            post(handlers::settings::custom_formats::settings_custom_formats_upsert),
        )
        .route(
            "/settings/custom-formats/delete",
            post(handlers::settings::custom_formats::settings_custom_formats_delete),
        )
        .route(
            "/settings/custom-formats/minimum-score",
            post(handlers::settings::custom_formats::settings_custom_formats_minimum_score),
        )
        // 256 KiB is generous for TRaSH-Guides anime CF JSON (the
        // entire vendored set is ~70 KiB) but well below axum's 2 MiB
        // default — keeps the hidden-field re-echo on the collision
        // review page bounded so a pasted multi-MiB payload doesn't
        // render a multi-MiB hidden form field.
        .route(
            "/settings/custom-formats/import",
            post(handlers::settings::custom_formats::settings_custom_formats_import)
                .layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route(
            "/settings/custom-formats/import-resolve",
            post(handlers::settings::custom_formats::settings_custom_formats_import_resolve)
                .layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route(
            "/settings/custom-formats/install-defaults",
            post(handlers::settings::custom_formats::settings_custom_formats_install_defaults),
        )
        .route(
            "/settings/custom-formats/reset-defaults",
            post(handlers::settings::custom_formats::settings_custom_formats_reset_defaults),
        )
        .route(
            "/settings/custom-formats/export",
            get(handlers::settings::custom_formats::settings_custom_formats_export),
        )
        .route(
            "/api/custom-formats/test",
            post(handlers::settings::custom_formats::settings_custom_formats_test),
        )
        .route("/api/qbit/test", post(handlers::settings::qbit_test))
        .route(
            "/api/jellyfin/test",
            post(handlers::settings::jellyfin_test),
        )
        .route("/api/health", get(handlers::settings::api_health))
        .route(
            "/api/jellyfin/refresh",
            post(handlers::settings::jellyfin_refresh),
        )
        .route(
            "/system",
            get(handlers::system::system_page).post(handlers::system::debug_settings_submit),
        )
        .route("/api/rss/sync", post(handlers::system::api_rss_sync))
        .route(
            "/api/rss/clear-history",
            post(handlers::system::api_rss_clear_history),
        )
        .route(
            "/api/tasks/metadata-refresh",
            post(handlers::system::api_force_metadata_refresh),
        )
        .route(
            "/api/tasks/cleanup",
            post(handlers::system::api_force_cleanup),
        )
        .route(
            "/api/tasks/post-processing",
            post(handlers::system::api_force_post_processing),
        )
        .route(
            "/api/tasks/library-classify",
            post(handlers::system::api_force_library_classify),
        )
        .route(
            "/api/tasks/upgrade-search",
            post(handlers::system::api_force_upgrade_search),
        )
        .route(
            "/api/system/rebuild-anilist-cache",
            post(handlers::system::api_rebuild_cached_metadata),
        )
        .route(
            "/api/system/reload-anibridge",
            post(handlers::system::api_anibridge_reload),
        )
        .route("/help", get(handlers::help::help_page))
        .route("/api/logs/poll", get(handlers::system::api_logs_poll))
        .route("/api/logs/clear", post(handlers::system::api_logs_clear))
        .route("/api/logs/client", post(handlers::system::api_logs_client))
        .route(
            "/api/progress/{job_id}",
            get(handlers::progress::poll_progress),
        )
        .route("/media/art/{cache_key}", get(handlers::media::artwork))
        .route("/logout", get(handlers::auth::logout))
        // SwaggerUI/OpenAPI live behind the auth wall: the OpenAPI doc
        // describes the entire route surface and form schemas, including
        // the rate-limited /login and /setup shapes. Exposing it
        // unauthenticated would hand a passing scanner a complete map of
        // the application before any auth check fires.
        .merge(SwaggerUi::new("/api-docs").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::auth::require_auth,
        ));

    // Sonarr v3 API compatibility layer for Seerr integration.
    // Authenticated via ?apikey= query parameter, not cookies.
    //
    // `aliased` collapses the Sonarr-API case-variant doublings (Seerr
    // sometimes sends `qualityprofile`, sometimes `qualityProfile`,
    // similarly for rootfolder/rootFolder and languageprofile/
    // languageProfile) into one line per logical endpoint. Adding a
    // third alias is a string change, not another `.route(...)` line
    // that future-me has to remember to keep in sync with the first.
    let sonarr_routes = Router::new()
        .route(
            "/api/v3/system/status",
            get(handlers::sonarr_compat::system_status),
        )
        .merge(aliased(
            &["/api/v3/qualityprofile", "/api/v3/qualityProfile"],
            get(handlers::sonarr_compat::quality_profiles),
        ))
        .merge(aliased(
            &["/api/v3/rootfolder", "/api/v3/rootFolder"],
            get(handlers::sonarr_compat::root_folders),
        ))
        .merge(aliased(
            &["/api/v3/languageprofile", "/api/v3/languageProfile"],
            get(handlers::sonarr_compat::language_profiles),
        ))
        .route(
            "/api/v3/tag",
            get(handlers::sonarr_compat::list_tags).post(handlers::sonarr_compat::create_tag),
        )
        .merge(aliased(
            &["/api/v3/downloadclient", "/api/v3/downloadClient"],
            get(handlers::sonarr_compat::list_download_clients),
        ))
        .route(
            "/api/v3/series",
            get(handlers::sonarr_compat::list_series)
                .post(handlers::sonarr_compat::add_series)
                .put(handlers::sonarr_compat::update_series),
        )
        .route(
            "/api/v3/series/{id}",
            get(handlers::sonarr_compat::get_series),
        )
        .route(
            "/api/v3/series/lookup",
            get(handlers::sonarr_compat::series_lookup),
        )
        .route(
            "/api/v3/command",
            post(handlers::sonarr_compat::execute_command),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::sonarr_compat::require_api_key,
        ));

    // Radarr v3 API compatibility layer for Seerr integration (anime movies).
    // Mounted under /radarr/ prefix — Seerr uses URL Base "/radarr" to route here.
    let radarr_routes = Router::new()
        .route(
            "/radarr/api/v3/system/status",
            get(handlers::radarr_compat::system_status),
        )
        .merge(aliased(
            &[
                "/radarr/api/v3/qualityprofile",
                "/radarr/api/v3/qualityProfile",
            ],
            get(handlers::radarr_compat::quality_profiles),
        ))
        .merge(aliased(
            &["/radarr/api/v3/rootfolder", "/radarr/api/v3/rootFolder"],
            get(handlers::radarr_compat::root_folders),
        ))
        .route(
            "/radarr/api/v3/tag",
            get(handlers::radarr_compat::list_tags).post(handlers::radarr_compat::create_tag),
        )
        .merge(aliased(
            &[
                "/radarr/api/v3/downloadclient",
                "/radarr/api/v3/downloadClient",
            ],
            get(handlers::radarr_compat::list_download_clients),
        ))
        .route(
            "/radarr/api/v3/movie",
            get(handlers::radarr_compat::list_movies)
                .post(handlers::radarr_compat::add_movie)
                .put(handlers::radarr_compat::update_movie),
        )
        .route(
            "/radarr/api/v3/movie/{id}",
            get(handlers::radarr_compat::get_movie),
        )
        .route(
            "/radarr/api/v3/movie/lookup",
            get(handlers::radarr_compat::movie_lookup),
        )
        .route(
            "/radarr/api/v3/command",
            post(handlers::radarr_compat::execute_command),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::radarr_compat::require_api_key,
        ));

    // Brotli/gzip compression. The series detail template is ~80KB of HTML
    // and style.css is ~64KB — both highly compressible (lots of repeated
    // tokens, whitespace), and they ship on every page navigation. Axum
    // negotiates via the client's Accept-Encoding automatically; if the
    // client doesn't advertise support, the body is passed through
    // unchanged.
    let compression = CompressionLayer::new().br(true).gzip(true);

    // Long-lived Cache-Control on /static/*. Without an explicit header the
    // browser falls back to heuristic freshness and tends to fire a
    // conditional GET on every navigation — a 304 still costs a full round
    // trip, which shows up as a visible stall when tabbing between pages.
    // One hour is conservative enough that a `cargo run` during development
    // still picks up edited CSS after a hard reload, but long enough that
    // the production case (topbar navigation) never re-validates.
    let static_cache_control = SetResponseHeaderLayer::if_not_present(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    let static_service = ServeDir::new("static");

    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .merge(sonarr_routes)
        .merge(radarr_routes)
        .nest_service(
            "/static",
            tower::ServiceBuilder::new()
                .layer(static_cache_control)
                .service(static_service),
        )
        .layer(compression)
        .with_state(state.clone());

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8978".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind");

    tracing::info!("Ryokan listening on {}", addr);

    // Register background task definitions for the System > Scheduled Tasks tab.
    let _ = models::scheduled_tasks::touch_definition(
        &db,
        "rss_sync",
        "RSS sync",
        "Every N minutes",
        false,
    )
    .await;
    let _ = models::scheduled_tasks::touch_definition(
        &db,
        "metadata_refresh",
        "Metadata refresh",
        "Every 12 hours",
        true,
    )
    .await;
    // Manual full rebuild is a distinct operation from the periodic
    // refresh — keep them on separate keys so a manual rebuild doesn't
    // clobber the refresh's audit trail (and vice versa) when the
    // two overlap.
    let _ = models::scheduled_tasks::touch_definition(
        &db,
        "metadata_rebuild",
        "Metadata cache rebuild",
        "Manual",
        false,
    )
    .await;
    let _ =
        models::scheduled_tasks::touch_definition(&db, "cleanup", "Cleanup", "Every 1 hour", true)
            .await;
    let _ = models::scheduled_tasks::touch_definition(
        &db,
        "post_processing",
        "Post-processing",
        "Every 1 minute (when enabled)",
        false,
    )
    .await;
    let upgrade_enabled = models::config::get_config(&db)
        .await
        .ok()
        .flatten()
        .map(|c| c.upgrade_search_enabled)
        .unwrap_or(false);
    let _ = models::scheduled_tasks::touch_definition(
        &db,
        "upgrade_search",
        "Quality upgrade search",
        "Every 24 hours (when enabled)",
        upgrade_enabled,
    )
    .await;
    let _ = models::scheduled_tasks::touch_definition(
        &db,
        "anibridge_refresh",
        "Anibridge mappings refresh",
        "Every 24 hours",
        true,
    )
    .await;
    let _ = models::scheduled_tasks::touch_definition(
        &db,
        "library_classify",
        "Library classify sweep",
        "Every 6 hours",
        true,
    )
    .await;

    // Pre-load anibridge mappings so the first Seerr request doesn't block on download.
    tokio::spawn(async {
        services::anibridge::ensure_loaded().await;
    });

    // Log startup to the database.
    services::logger::info(
        &db,
        models::log::LogCategory::System,
        "Ryokan started",
        &format!("Listening on {}", addr),
    )
    .await;

    // Background task: RSS auto-sync. Wrapped in `supervise` so a panic
    // inside sync_once is logged and the loop restarts rather than going
    // silent for the rest of the process lifetime.
    {
        let rss_state = state.clone();
        tokio::spawn(async move {
            supervise("rss_sync", move || {
                let inner_state = rss_state.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                    // Seed `minutes_since_last` from the persisted
                    // `scheduled_task_runs.last_finished_at` row so a
                    // restart doesn't force an immediate re-run. If RSS
                    // synced 3 minutes ago in a prior process and the
                    // configured interval is 10 minutes, we should wait
                    // 7 more minutes, not 0. `None` means "never run" —
                    // fall back to the old 10_000 sentinel so first-time
                    // setups still fire on the first tick.
                    let mut minutes_since_last: i64 =
                        models::scheduled_tasks::minutes_since_last_finished(
                            &inner_state.db,
                            "rss_sync",
                        )
                        .await
                        .unwrap_or(10_000);
                    let mut consecutive_errors: i64 = 0;
                    loop {
                        interval.tick().await;
                        minutes_since_last += 1;

                        let cfg = match models::config::get_config(&inner_state.db).await {
                            Ok(Some(cfg)) => cfg,
                            _ => continue,
                        };

                        let _ = models::scheduled_tasks::touch_definition(
                            &inner_state.db,
                            "rss_sync",
                            "RSS sync",
                            &format!("Every {} minutes", cfg.rss_interval_minutes.clamp(1, 60)),
                            cfg.rss_enabled,
                        )
                        .await;
                        if !cfg.rss_enabled {
                            continue;
                        }

                        let every = (cfg.rss_interval_minutes as i64).clamp(1, 60);
                        // Exponential backoff on consecutive errors: skip 2^errors extra intervals (capped at 32)
                        let backoff = if consecutive_errors > 0 {
                            every * (1i64 << consecutive_errors.min(5))
                        } else {
                            every
                        };
                        if minutes_since_last < backoff {
                            continue;
                        }

                        minutes_since_last = 0;
                        let _ = models::scheduled_tasks::mark_started(
                            &inner_state.db,
                            "rss_sync",
                            "Automatic RSS sync started",
                        )
                        .await;
                        match services::rss::sync_once(&inner_state, "auto").await {
                            Ok(summary) => {
                                consecutive_errors = 0;
                                let _ = models::scheduled_tasks::mark_finished(
                                    &inner_state.db,
                                    "rss_sync",
                                    "ok",
                                    &summary.detail,
                                )
                                .await;
                            }
                            Err(err) => {
                                consecutive_errors += 1;
                                let _ = models::scheduled_tasks::mark_finished(
                                    &inner_state.db,
                                    "rss_sync",
                                    "error",
                                    &err,
                                )
                                .await;
                                services::logger::error(
                                    &inner_state.db,
                                    models::log::LogCategory::System,
                                    "Auto RSS sync failed",
                                    &format!(
                                        "{} (backoff: {} consecutive errors)",
                                        err, consecutive_errors
                                    ),
                                )
                                .await;
                            }
                        }
                    }
                }
            })
            .await;
        });
    }

    // Background task: refresh cached series metadata every 12 hours.
    // The startup delay is computed from `scheduled_task_runs.last_finished_at`
    // so a restart mid-window doesn't re-fire the sweep — the whole
    // point of a 12h cadence is that it's expensive. Previously the
    // bare `interval.tick()` fired on the first call and kicked off a
    // fresh sweep on every `cargo run`.
    {
        let metadata_db = db.clone();
        tokio::spawn(async move {
            supervise("metadata_refresh", move || {
                let db = metadata_db.clone();
                async move {
                    let period = std::time::Duration::from_secs(12 * 60 * 60);
                    let delay = models::scheduled_tasks::duration_until_next_run(
                        &db,
                        "metadata_refresh",
                        period,
                    )
                    .await;
                    tokio::time::sleep(delay).await;
                    loop {
                        let _ = models::scheduled_tasks::mark_started(
                            &db,
                            "metadata_refresh",
                            "Refreshing tracked series metadata",
                        )
                        .await;
                        let (refreshed, failed) =
                            services::metadata_sync::refresh_all_series_metadata(&db).await;
                        let status = if failed > 0 { "warn" } else { "ok" };
                        let detail = format!("refreshed={}, failed={}", refreshed, failed);
                        let _ = models::scheduled_tasks::mark_finished(
                            &db,
                            "metadata_refresh",
                            status,
                            &detail,
                        )
                        .await;
                        tokio::time::sleep(period).await;
                    }
                }
            })
            .await;
        });
    }

    // Background task: clean up logs and old RSS decisions older than 30 days every hour.
    {
        let cleanup_db = db.clone();
        tokio::spawn(async move {
            supervise("cleanup", move || {
                let cleanup_db = cleanup_db.clone();
                async move {
                    let period = std::time::Duration::from_secs(3600);
                    // Honors persisted last-run so a restart 10 minutes after
                    // the previous sweep waits 50 more, not a fresh hour. Each
                    // pass is cheap (indexed DELETEs) but the consistency with
                    // the other scheduled tasks is worth more than the tiny
                    // CPU savings.
                    let delay = models::scheduled_tasks::duration_until_next_run(
                        &cleanup_db,
                        "cleanup",
                        period,
                    )
                    .await;
                    tokio::time::sleep(delay).await;
                    loop {
                        let _ = models::scheduled_tasks::mark_started(
                            &cleanup_db,
                            "cleanup",
                            "Pruning logs and RSS decision history",
                        )
                        .await;
                        let mut cleanup_errors = Vec::new();
                        match models::log::cleanup(&cleanup_db, 30).await {
                            Ok(deleted) if deleted > 0 => {
                                tracing::debug!("Cleaned up {} old log entries", deleted);
                            }
                            Err(e) => {
                                cleanup_errors.push(format!("logs: {}", e));
                                tracing::error!("Log cleanup failed: {}", e);
                            }
                            _ => {}
                        }
                        // Prune old RSS decisions (keep grabbed forever, prune skipped/rejected after 30 days).
                        match models::rss::cleanup_old_decisions(&cleanup_db, 30).await {
                            Ok(deleted) if deleted > 0 => {
                                tracing::debug!("Cleaned up {} old RSS decisions", deleted);
                            }
                            Err(e) => {
                                cleanup_errors.push(format!("rss: {}", e));
                                tracing::error!("RSS decision cleanup failed: {}", e);
                            }
                            _ => {}
                        }
                        // Prune cold Nyaa description cache rows. `cached_at` is only
                        // refreshed on cache miss (live fetch), not on cache hits, so
                        // this evicts rows that haven't triggered a network fetch in
                        // 90 days. Consequence is a forced re-fetch the next time the
                        // row is needed, not lost data.
                        match models::nyaa_description_cache::cleanup(&cleanup_db, 90).await {
                            Ok(deleted) if deleted > 0 => {
                                tracing::debug!(
                                    "Cleaned up {} old nyaa description cache rows",
                                    deleted
                                );
                            }
                            Err(e) => {
                                cleanup_errors.push(format!("nyaa_description_cache: {}", e));
                                tracing::error!("Nyaa description cache cleanup failed: {}", e);
                            }
                            _ => {}
                        }
                        // Prune stale media probe cache rows. These are keyed by
                        // filesystem path, so deleted / renamed files leave rows
                        // that nothing will ever re-touch — the hourly sweep is the
                        // only eviction path. Consequence for still-live files is a
                        // single re-probe after the TTL expires.
                        match models::media_probe_cache::cleanup(&cleanup_db, 90).await {
                            Ok(deleted) if deleted > 0 => {
                                tracing::debug!(
                                    "Cleaned up {} old media probe cache rows",
                                    deleted
                                );
                            }
                            Err(e) => {
                                cleanup_errors.push(format!("media_probe_cache: {}", e));
                                tracing::error!("Media probe cache cleanup failed: {}", e);
                            }
                            _ => {}
                        }
                        // Prune orphan artwork (image_refs whose parent series
                        // is gone, and image_blobs/files no ref references after
                        // 7 days). Without this the cache only ever grows —
                        // every removed series leaves rows pointing at on-disk
                        // blob files that nothing will ever touch again.
                        match models::artwork_cache::cleanup_orphans(&cleanup_db, 7).await {
                            Ok((refs, blobs)) if refs > 0 || blobs > 0 => {
                                tracing::debug!(
                                    "Cleaned up {} orphan artwork refs and {} orphan blobs",
                                    refs,
                                    blobs
                                );
                            }
                            Err(e) => {
                                cleanup_errors.push(format!("artwork_cache: {}", e));
                                tracing::error!("Artwork cleanup failed: {}", e);
                            }
                            _ => {}
                        }
                        // Prune expired session rows. `validate_session` already
                        // rejects rows older than 7 days, but without this sweep
                        // the sessions table grows unbounded — every login leaves
                        // a permanent row. 7 days matches the cookie Max-Age.
                        match models::session::cleanup(&cleanup_db, 7).await {
                            Ok(deleted) if deleted > 0 => {
                                tracing::debug!("Cleaned up {} expired session rows", deleted);
                            }
                            Err(e) => {
                                cleanup_errors.push(format!("sessions: {}", e));
                                tracing::error!("Session cleanup failed: {}", e);
                            }
                            _ => {}
                        }
                        // Prune idle LOGIN_FAILURES entries. The per-request sweep
                        // in `login_check` only touches keys actively being hit,
                        // so IPs / usernames that failed once and then went quiet
                        // would linger until the process restarts. Hourly global
                        // sweep keeps the map bounded.
                        handlers::auth::sweep_login_failures();
                        let status = if cleanup_errors.is_empty() {
                            "ok"
                        } else {
                            "warn"
                        };
                        let detail = if cleanup_errors.is_empty() {
                            "Cleanup completed".to_string()
                        } else {
                            cleanup_errors.join("; ")
                        };
                        let _ = models::scheduled_tasks::mark_finished(
                            &cleanup_db,
                            "cleanup",
                            status,
                            &detail,
                        )
                        .await;
                        tokio::time::sleep(period).await;
                    }
                }
            })
            .await;
        });
    }

    // Background task: post-processing — move/rename completed downloads every minute.
    {
        let pp_state = state.clone();
        tokio::spawn(async move {
            supervise("post_processing", move || {
                let pp_state = pp_state.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                    loop {
                        interval.tick().await;
                        let enabled = models::config::get_config(&pp_state.db)
                            .await
                            .ok()
                            .flatten()
                            .map(|c| c.post_processing_enabled)
                            .unwrap_or(false);
                        let _ = models::scheduled_tasks::touch_definition(
                            &pp_state.db,
                            "post_processing",
                            "Post-processing",
                            "Every 1 minute (when enabled)",
                            enabled,
                        )
                        .await;
                        // Call run_once unconditionally so the #14 lightweight
                        // `advance_state_without_import` sweep can fire when
                        // post-processing is disabled. run_once internally
                        // branches on cfg.post_processing_enabled to choose
                        // between the full import flow and the state-only
                        // advance.
                        let _ = models::scheduled_tasks::mark_started(
                            &pp_state.db,
                            "post_processing",
                            "Checking for completed downloads",
                        )
                        .await;
                        services::post_processing::run_once(&pp_state).await;
                        let _ = models::scheduled_tasks::mark_finished(
                            &pp_state.db,
                            "post_processing",
                            "ok",
                            "",
                        )
                        .await;
                    }
                }
            })
            .await;
        });
    }

    // Background task: library classify sweep every 6 hours. Re-runs the
    // classifier against any on-disk files that are still tagged empty or
    // "unknown" so the library self-heals when earlier low-confidence
    // filename-only results can now be resolved with ffprobe. The 6-hour
    // cadence is slow enough that ffprobe cost stays trivial and fast
    // enough that a new unknown row upgrades the same day.
    //
    // The initial delay honors the persisted `last_finished_at` so a
    // restart mid-window resumes the prior schedule instead of always
    // waiting a fresh 6 hours. Previously the skip-tick pattern meant
    // a process that restarts every 5 hours would never actually
    // classify anything.
    {
        let classify_state = state.clone();
        tokio::spawn(async move {
            supervise("library_classify", move || {
                let classify_state = classify_state.clone();
                async move {
                    let period = std::time::Duration::from_secs(6 * 60 * 60);
                    // Even when the persisted timer says we're overdue,
                    // nudge a minimum startup delay so a big ffprobe sweep
                    // doesn't race the rest of initialization on a cold
                    // boot. Five minutes gives the rest of main.rs time
                    // to settle and the user time to see the library
                    // index render before we start hammering the disk.
                    const MIN_STARTUP_DELAY: std::time::Duration =
                        std::time::Duration::from_secs(5 * 60);
                    let delay = models::scheduled_tasks::duration_until_next_run(
                        &classify_state.db,
                        "library_classify",
                        period,
                    )
                    .await
                    .max(MIN_STARTUP_DELAY);
                    tokio::time::sleep(delay).await;
                    loop {
                        let _ = models::scheduled_tasks::touch_definition(
                            &classify_state.db,
                            "library_classify",
                            "Library classify sweep",
                            "Every 6 hours",
                            true,
                        )
                        .await;
                        let _ = models::scheduled_tasks::mark_started(
                            &classify_state.db,
                            "library_classify",
                            "Re-classifying unknown / unclassified files",
                        )
                        .await;
                        let report = services::post_processing::scan_library_for_unclassified(
                            &classify_state,
                        )
                        .await;
                        let detail = format!(
                            "series={}, files_scanned={}, classified={}, needs_review={}",
                            report.series_scanned,
                            report.files_scanned,
                            report.files_classified,
                            report.files_needing_review,
                        );
                        let _ = models::scheduled_tasks::mark_finished(
                            &classify_state.db,
                            "library_classify",
                            "ok",
                            &detail,
                        )
                        .await;
                        tokio::time::sleep(period).await;
                    }
                }
            })
            .await;
        });
    }

    // Background task: quality upgrade search every 24 hours (when enabled).
    // Honors the persisted last-run time so a restart doesn't re-kick a
    // potentially-30-minute sweep that just finished an hour ago.
    {
        let upgrade_state = state.clone();
        tokio::spawn(async move {
            supervise("upgrade_search", move || {
                let upgrade_state = upgrade_state.clone();
                async move {
                    let period = std::time::Duration::from_secs(24 * 60 * 60);
                    let delay = models::scheduled_tasks::duration_until_next_run(
                        &upgrade_state.db,
                        "upgrade_search",
                        period,
                    )
                    .await;
                    tokio::time::sleep(delay).await;
                    loop {
                        let enabled = models::config::get_config(&upgrade_state.db)
                            .await
                            .ok()
                            .flatten()
                            .map(|c| c.upgrade_search_enabled)
                            .unwrap_or(false);
                        let _ = models::scheduled_tasks::touch_definition(
                            &upgrade_state.db,
                            "upgrade_search",
                            "Quality upgrade search",
                            "Every 24 hours (when enabled)",
                            enabled,
                        )
                        .await;
                        if enabled {
                            let _ = models::scheduled_tasks::mark_started(
                                &upgrade_state.db,
                                "upgrade_search",
                                "Searching for quality upgrades",
                            )
                            .await;
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(30 * 60),
                                services::upgrade::run_once(&upgrade_state),
                            )
                            .await
                            {
                                Ok(Ok(summary)) => {
                                    let _ = models::scheduled_tasks::mark_finished(
                                        &upgrade_state.db,
                                        "upgrade_search",
                                        "ok",
                                        &summary.detail,
                                    )
                                    .await;
                                }
                                Ok(Err(err)) => {
                                    let _ = models::scheduled_tasks::mark_finished(
                                        &upgrade_state.db,
                                        "upgrade_search",
                                        "error",
                                        &err,
                                    )
                                    .await;
                                    services::logger::error(
                                        &upgrade_state.db,
                                        models::log::LogCategory::System,
                                        "Upgrade search failed",
                                        &err,
                                    )
                                    .await;
                                }
                                Err(_) => {
                                    let _ = models::scheduled_tasks::mark_finished(
                                        &upgrade_state.db,
                                        "upgrade_search",
                                        "error",
                                        "Timed out after 30 minutes",
                                    )
                                    .await;
                                    services::logger::error(
                                        &upgrade_state.db,
                                        models::log::LogCategory::System,
                                        "Upgrade search timed out",
                                        "Exceeded 30-minute limit",
                                    )
                                    .await;
                                }
                            }
                        }
                        // Always sleep the full period before re-checking,
                        // whether or not the task ran this iteration. When
                        // the toggle is off we still wake every 24h to
                        // touch the definition row and pick up a flip.
                        tokio::time::sleep(period).await;
                    }
                }
            })
            .await;
        });
    }

    // Background task: Anibridge mappings refresh (every 24 hours).
    // Honors the persisted last-run time so a restart 2h after a
    // successful refresh waits 22h, not a fresh 24h. Combined with
    // the on-disk mappings cache in `services::anibridge`, startup
    // consistently avoids re-pulling the ~20MB mappings JSON from
    // GitHub when a recent copy exists.
    {
        let anibridge_db = db.clone();
        tokio::spawn(async move {
            supervise("anibridge_refresh", move || {
                let anibridge_db = anibridge_db.clone();
                async move {
                    // Share the interval with the on-disk cache TTL in
                    // `services::anibridge` so the bg task cadence and
                    // startup freshness check can't drift apart.
                    let period = services::anibridge::REFRESH_INTERVAL;
                    let delay = models::scheduled_tasks::duration_until_next_run(
                        &anibridge_db,
                        "anibridge_refresh",
                        period,
                    )
                    .await;
                    tokio::time::sleep(delay).await;
                    loop {
                        let _ = models::scheduled_tasks::mark_started(
                            &anibridge_db,
                            "anibridge_refresh",
                            "Refreshing anibridge mappings",
                        )
                        .await;
                        if services::anibridge::reload().await {
                            let _ = models::scheduled_tasks::mark_finished(
                                &anibridge_db,
                                "anibridge_refresh",
                                "ok",
                                "Mappings refreshed",
                            )
                            .await;
                        } else {
                            let _ = models::scheduled_tasks::mark_finished(
                                &anibridge_db,
                                "anibridge_refresh",
                                "error",
                                "Failed to download mappings",
                            )
                            .await;
                        }
                        tokio::time::sleep(period).await;
                    }
                }
            })
            .await;
        });
    }

    // Background task: sweep finished progress jobs out of the in-memory
    // registry. Jobs are kept for 60s past their terminal event so a
    // frontend that's mid-poll still sees the final state on its next
    // tick, then dropped. The sweep itself is cheap (one mutex acquire,
    // a HashMap retain) so a 30s tick is fine.
    {
        let progress_state = state.clone();
        tokio::spawn(async move {
            supervise("progress_sweep", move || {
                let progress = progress_state.progress.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                    loop {
                        interval.tick().await;
                        progress.sweep(std::time::Duration::from_secs(60)).await;
                    }
                }
            })
            .await;
        });
    }

    // Use `into_make_service_with_connect_info::<SocketAddr>()` so the auth
    // handler can pull the true client socket address via
    // `ConnectInfo<SocketAddr>`. This is the ground-truth IP the rate limiter
    // uses whenever RYOKAN_TRUSTED_PROXY is unset — without it, the only
    // source of an IP is the (spoofable) X-Forwarded-For / X-Real-IP headers.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("Server error");
}
