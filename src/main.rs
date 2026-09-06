// Module tree lives in `src/lib.rs` so integration tests under `tests/`
// can exercise handlers without spawning a binary. `main.rs` is just
// the boot entry point — everything else rides the library crate.
use ryokan::{AppState, handlers, models, services};

use axum::http::{HeaderValue, header};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use services::{
    custom_formats::{self, CompiledCfCache},
    jellyfin::JellyfinClient,
    progress::ProgressRegistry,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Ryokan API",
        description = "Self-hosted anime PVR: search, download, and manage your anime library.",
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
        handlers::library::crud::set_allow_pt_upgrades,
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
        handlers::library::misgrabs::restore_misgrab,
        handlers::library::misgrabs::dismiss_misgrab,
        // System
        handlers::settings::api_health,
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
        // Settings — Indexers (issue #28)
        handlers::settings::indexers::settings_indexers_upsert,
        handlers::settings::indexers::settings_indexers_delete,
        // Settings — Download clients (multi-client refactor)
        handlers::settings::download_clients::settings_download_clients_upsert,
        handlers::settings::download_clients::settings_download_clients_delete,
        handlers::settings::download_clients::settings_download_clients_set_default,
        handlers::settings::download_clients::settings_download_clients_test,
        handlers::settings::download_clients::settings_download_clients_section,
        handlers::settings::download_clients::settings_download_clients_edit_form,
        handlers::settings::download_clients::settings_download_clients_add_form,
        handlers::settings::download_clients::settings_download_clients_status,
        handlers::settings::indexers::settings_indexers_nyaa_form,
        handlers::settings::indexers::settings_indexers_nyaa_save,
        // Settings — autobrr API key rotation (issue #28)
        handlers::settings::autobrr_key::settings_autobrr_regenerate_key,
        // Webhooks (issue #28)
        handlers::webhook::autobrr::webhook_autobrr,
        handlers::system::api_logs_poll,
        handlers::system::api_logs_clear,
        handlers::system::api_logs_export,
        handlers::system::api_logs_client,
        handlers::system::api_force_external_sync,
        handlers::progress::poll_progress,
        handlers::system::api_rss_sync,
        handlers::system::api_rss_clear_history,
        handlers::system::api_force_metadata_refresh,
        handlers::system::api_force_airing_refresh,
        handlers::system::api_force_cleanup,
        handlers::system::api_force_post_processing,
        handlers::system::api_force_library_classify,
        handlers::system::api_force_upgrade_search,
        handlers::system::api_rebuild_cached_metadata,
        handlers::system::api_anibridge_reload,
        handlers::system::api_system_tasks,
        // Grab (issue #83 interactive file picker)
        handlers::grab::grab_preview,
        handlers::grab::grab_preview_status,
        handlers::grab::grab_heartbeat,
        handlers::grab::grab_confirm,
        handlers::grab::grab_cancel,
        // Library bulk actions + recycle bin (#123)
        handlers::library::bulk::bulk_delete,
        handlers::library::bulk::bulk_monitor,
        handlers::library::crud::bulk_manual_override,
        handlers::library::recycle::restore,
        handlers::library::recycle::purge_entry,
        handlers::library::recycle::empty,
        // Progress stream
        handlers::progress::stream_progress,
        // Settings: indexer section, forms, and tests
        handlers::settings::indexers::settings_indexers_section,
        handlers::settings::indexers::settings_indexers_add_form,
        handlers::settings::indexers::settings_indexers_edit_form,
        handlers::settings::indexers::settings_indexers_test_rss,
        handlers::settings::indexers::settings_indexers_test_stateless,
        // Settings: direct RSS feeds
        handlers::settings::direct_rss_feeds::settings_direct_rss_feeds_upsert,
        handlers::settings::direct_rss_feeds::settings_direct_rss_feeds_delete,
        handlers::settings::direct_rss_feeds::settings_direct_rss_feeds_test,
        // Settings: naming preview (#124)
        handlers::settings::naming::naming_preview,
        // Scoped API keys (#114)
        handlers::api_keys::list,
        handlers::api_keys::create,
        handlers::api_keys::toggle,
        handlers::api_keys::delete,
        handlers::api_keys::reveal,
        // Calendar feed (#116)
        handlers::calendar::ical_feed,
        // Notifications (#118)
        handlers::notifications::test_provider,
        // Backup / restore (#126)
        handlers::system::backup::api_backup_download,
        handlers::system::backup::api_backup_run,
        handlers::system::backup::api_backup_file,
        handlers::system::backup::backup_file_delete,
        handlers::system::backup::api_restore_upload,
        handlers::system::backup::restore_cancel,
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
        services::task_registry::TaskSnapshot,
        handlers::system::SystemTasksResponse,
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
        handlers::settings::JellyfinTestForm,
        handlers::settings::download_clients::DownloadClientUpsertForm,
        handlers::settings::download_clients::DownloadClientIdForm,
        handlers::settings::download_clients::DownloadClientTestForm,
        handlers::settings::indexers::NyaaSettingsForm,
        handlers::settings::custom_formats::CustomFormatUpsertForm,
        handlers::settings::custom_formats::CfTestRequest,
        handlers::settings::custom_formats::CustomFormatDeleteForm,
        handlers::settings::custom_formats::CustomFormatMinScoreForm,
        handlers::settings::custom_formats::CustomFormatImportForm,
        handlers::grab::GrabPreviewForm,
        handlers::grab::GrabPreviewCreated,
        handlers::grab::GrabPreviewStatus,
        handlers::grab::PreviewFile,
        handlers::grab::GrabConfirmForm,
        handlers::grab::GrabConfirmResult,
        handlers::grab::GrabCancelForm,
    )),
    tags(
        (name = "Library", description = "Anime library management: add, remove, search, and monitor series"),
        (name = "Search", description = "Nyaa torrent search and grabbing"),
        (name = "Downloads", description = "Download queue management across the configured download clients"),
        (name = "System", description = "Health checks, logs, RSS sync, and background tasks"),
        (name = "Settings", description = "Settings management: Custom Formats CRUD, import/export, and scoring thresholds"),
        (name = "Grab", description = "Interactive file-picker grab flow (#83): preview, heartbeat, confirm, cancel"),
        (name = "Backup", description = "Backup and restore of the database, encryption key, and artwork (#126)"),
        (name = "Calendar", description = "Airing calendar feed for scoped API keys (#116)"),
        (name = "Webhook", description = "Inbound push receivers such as autobrr"),
    ),
)]
struct ApiDoc;

/// The OpenAPI document with the version stamped from `Cargo.toml`.
/// utoipa's `info(version = ...)` only takes a literal, and a hardcoded
/// string drifted to `0.1.0` while the crate sat at 1.x.
fn api_doc() -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    doc.info.version = env!("CARGO_PKG_VERSION").to_string();
    doc
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

async fn supervise<F, Fut>(
    registry: &services::task_registry::TaskRegistry,
    name: &'static str,
    mut make_fut: F,
) -> !
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

    // Register once and hold the per-task `Arc<TaskState>` for the
    // life of the loop. Status mutations are atomics on the shared
    // state — `/api/system/tasks` snapshot reads don't contend.
    let task_state = registry.register(name).await;

    let mut backoff = MIN_BACKOFF;
    loop {
        let started = Instant::now();
        task_state.mark_started(unix_now());
        let handle = tokio::spawn(make_fut());
        let exit_kind = match handle.await {
            Err(e) if e.is_panic() => {
                tracing::error!("Background task '{}' panicked: {:?}", name, e);
                services::task_registry::ExitKind::Panic
            }
            Err(e) => {
                tracing::error!("Background task '{}' join error: {:?}", name, e);
                services::task_registry::ExitKind::JoinError
            }
            Ok(()) => {
                tracing::warn!("Background task '{}' exited normally", name);
                services::task_registry::ExitKind::Normal
            }
        };
        task_state.mark_exited(unix_now(), exit_kind);

        if started.elapsed() >= HEALTHY_RUNTIME {
            backoff = MIN_BACKOFF;
        } else {
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
        tracing::warn!("supervise '{}': restarting in {:?}", name, backoff);
        task_state.mark_backoff(backoff.as_millis() as u64);
        tokio::time::sleep(backoff).await;
    }
}

/// Wall-clock unix-seconds reader. Cheap; called once per supervise
/// transition (start / exit).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `--sanitize-db-for-debug` entrypoint. Copies the live DB to a
/// sibling file with all tokens / passwords / session cookies / user
/// password hashes blanked, then prints the destination path. Used
/// by users preparing a DB dump for a bug report so they don't leak
/// live credentials.
///
/// Defaults to operating on `data/ryokan.db` → `data/ryokan-sanitized.db`,
/// matching the repo's gitignored `data/` convention. Respects
/// `DATABASE_URL` only if it points at a plain file path (no HTTP /
/// remote SQLite variants) — bugged-DB sharing only makes sense for
/// the local file case.
async fn run_sanitize_cli() {
    let live = resolve_live_db_path();
    // Output lands in the CWD, not next to the live DB. If a user
    // configured `DATABASE_URL=/etc/ryokan/db.sqlite`, dropping the
    // sanitized copy at `/etc/ryokan/db-sanitized.sqlite` puts it
    // squarely in path of any system backup rotation that scans
    // `/etc`. CWD is where `cargo run` and `docker exec` land by
    // default — predictable and user-controlled.
    let stem = live
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("ryokan");
    let out = std::path::PathBuf::from(format!("{stem}-sanitized.db"));

    match services::sanitize::run_sanitize(&live, &out).await {
        Ok(summary) => {
            println!("{summary}");
        }
        Err(e) => {
            eprintln!("sanitize failed: {e}");
            std::process::exit(1);
        }
    }
}

fn resolve_live_db_path() -> std::path::PathBuf {
    services::backup::live_db_path()
}

#[tokio::main]
async fn main() {
    // One-shot CLI modes run BEFORE tracing init so their output isn't
    // interleaved with startup log lines. Each mode exits the process
    // on completion; falling through means "boot the server normally."
    if std::env::args().any(|a| a == "--sanitize-db-for-debug") {
        run_sanitize_cli().await;
        std::process::exit(0);
    }

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
    // lot here: every supervised background task (grep `supervise(&` for the
    // canonical names — currently 11) shares this pool with the request
    // path. In the default DELETE
    // journal mode every writer takes a whole-database lock and stalls the
    // next page load behind whatever scheduled_tasks row update or log insert
    // happens to be running. `synchronous=NORMAL` is safe under WAL (durable
    // across application crashes; the usual caveat is only crash-safe across
    // OS power loss, which matches what Sonarr/Radarr ship). The pragmas
    // below size the page cache and enable mmap'd reads so hot tables stay
    // in memory on subsequent lookups.
    // Issue #126: a restore staged from System → Backup is applied
    // here, before the pool opens, by swapping the staged files into
    // place. The previous database stays beside the restored one as
    // `ryokan.db.pre-restore-<ts>`. A failure leaves the staged
    // directory for inspection and boots the current data.
    let backup_paths = services::backup::BackupPaths::from_env();
    let restore_applied = match services::backup::apply_pending_restore(&backup_paths) {
        Ok(applied) => applied,
        Err(e) => {
            tracing::error!(
                "Staged restore was NOT applied: {e}. The staged files stay in {} for inspection; \
                 cancel the restore from System → Backup or fix the directory and restart.",
                backup_paths.pending_dir().display()
            );
            None
        }
    };
    if let Some(applied) = &restore_applied {
        tracing::warn!(
            "Restore applied from a backup made {}: previous database kept at {}{}{}",
            applied.manifest.timestamp_label(),
            applied.previous_db.display(),
            if applied.key_replaced {
                "; encryption key replaced"
            } else {
                ""
            },
            if applied.artwork_replaced {
                "; artwork cache replaced"
            } else {
                ""
            },
        );
    }

    if let Some(applied) = &restore_applied {
        for warning in &applied.warnings {
            tracing::warn!("Restore: {warning}");
        }
    }
    // Whatever a crash left in the backup / upload work dirs is dead
    // weight now (a half-written multi-GB upload, a vacuum snapshot).
    for dir in services::backup::sweep_work_dirs(&backup_paths) {
        tracing::info!("Cleared stranded backup work dir {}", dir.display());
    }

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

    if let Some(applied) = restore_applied {
        services::logger::info(
            &db,
            models::log::LogCategory::System,
            "Restore applied at startup",
            &format!(
                "backup from {} (Ryokan {}); previous database kept at {}",
                applied.manifest.timestamp_label(),
                applied.manifest.ryokan_version,
                applied.previous_db.display()
            ),
        )
        .await;
        for warning in &applied.warnings {
            services::logger::warn(
                &db,
                models::log::LogCategory::System,
                "Restore applied with a warning",
                warning,
            )
            .await;
        }
    }

    // #62 — one-shot genre backfill from existing
    // series_metadata_cache rows so the library filter dropdown
    // lights up immediately on first boot after upgrade. Idempotent
    // via the `schema_migrations` ledger; subsequent boots are a
    // single COUNT(*) probe and return.
    if let Err(e) = models::series_genres::backfill_from_metadata_cache_once(&db).await {
        tracing::warn!("series_genres backfill failed (filter dropdown may be empty): {e}");
    }

    // Boot-time recovery: any `scheduled_task_runs` row left at
    // last_status='running' is necessarily stranded — the task that
    // wrote it lived in a prior process incarnation that has since
    // gone away. Mark them as 'error' with a user-facing detail so
    // the System → Scheduled Tasks UI doesn't keep showing them as
    // in-flight forever. See `recover_stuck_running` for the full
    // backstory.
    match models::scheduled_tasks::recover_stuck_running(&db).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            target: "ryokan::scheduled_tasks",
            "recovered {n} stuck 'running' scheduled-task row(s) at startup"
        ),
        Err(e) => tracing::warn!(
            target: "ryokan::scheduled_tasks",
            "stuck-task recovery failed (rows may stay at 'running'): {e}"
        ),
    }

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

    // Warm the AEAD encryption-key LazyLock (issue #62). Same
    // pattern as `warm_timing_equalizer`: pays the cold-start cost
    // (env-var parse, file read, possible first-run key generation
    // with a 0600 chmod) at boot rather than during the user's first
    // OAuth `/submit`. Wrapped in `spawn_blocking` because the
    // first-run path may write to disk.
    let _ = tokio::task::spawn_blocking(services::crypto::warm_key).await;

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

    // PR #107 review fix #4: build the indexer cache at startup.
    // Failed instantiations (empty URL, reqwest build) are logged
    // and dropped via `services::indexers::rebuild_cache` so the
    // surviving rows still fan out.
    let indexers = services::indexers::rebuild_cache(&db).await;

    // Build shared state.
    let download_clients: ryokan::DownloadClientsCache =
        Arc::new(RwLock::new(Arc::new(ryokan::DownloadClientPool::default())));
    let state = AppState {
        db: db.clone(),
        download_clients: download_clients.clone(),
        jellyfin: Arc::new(RwLock::new(None)),
        custom_formats: cf_cache,
        indexers,
        progress: ProgressRegistry::new(),
        users_exist,
        interactive_search_cache: services::interactive_search_cache::new(),
        oauth_state: services::oauth_state::new(),
        start_time: chrono::Utc::now(),
        tasks: services::task_registry::TaskRegistry::new(),
        dc_status_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        notification_providers: services::notifications::empty_cache(),
        import_sessions: services::manual_import::session::new_store(),
    };

    // Initialize the multi-client pool from `download_clients` rows.
    // Pre-multi-client installs ran their migration's seed-default
    // backfill at startup (above), so by the time this runs there
    // should be either zero rows (fresh install — pool stays empty
    // until the user adds a row in Settings → Connections →
    // Downloads) or one row with `is_default = 1` mirroring the
    // legacy `active_client` choice.
    services::download_client::rebuild_clients_cache(&state.download_clients, &db).await;
    // Issue #119 — initialize the notification providers snapshot
    // from the `notification_providers` table. Empty until the user
    // adds a row in Settings → Notifications; the dispatcher's
    // `is_empty()` early-return makes every hook a no-op until then.
    services::notifications::rebuild_notification_providers_cache(
        &state.notification_providers,
        &db,
    )
    .await;
    // Pre-warm the DC status cache in the background so the first
    // visit to Settings → Connections renders the probed pills
    // server-side instead of flashing through the "Probing…"
    // placeholder. Spawned because a slow probe (SAB on a
    // tarpitting tracker, an unreachable seedbox) shouldn't delay
    // the listener bind by ~5s. Subsequent visits within
    // DC_STATUS_CACHE_TTL pick up the warmed entries.
    {
        let pool_cache = state.download_clients.clone();
        let status_cache = state.dc_status_cache.clone();
        tokio::spawn(async move {
            handlers::settings::download_clients::prewarm_dc_status_cache(
                &pool_cache,
                &status_cache,
            )
            .await;
        });
    }
    if let Ok(Some(config)) = models::config::get_config(&db).await
        && !config.jellyfin_url.is_empty()
        && !config.jellyfin_api_key.is_empty()
    {
        let client = JellyfinClient::new(&config.jellyfin_url, &config.jellyfin_api_key);
        *state.jellyfin.write().await = Some(client);
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
        .route("/library/recycle", get(handlers::library::recycle::page))
        // #122 manual import wizard. Form-POST + server render; the
        // per-group override controls swap a single card via HTMX.
        .route(
            "/system/import",
            get(handlers::system::manual_import::page).post(handlers::system::manual_import::start),
        )
        .route(
            "/system/import/{session_id}/group/{idx}",
            post(handlers::system::manual_import::group_action),
        )
        .route(
            "/system/import/{session_id}/group/{idx}/candidates",
            get(handlers::system::manual_import::picker_candidates),
        )
        .route(
            "/system/import/{session_id}/discard",
            post(handlers::system::manual_import::discard),
        )
        .route(
            "/system/import/{session_id}/confirm",
            post(handlers::system::manual_import::confirm),
        )
        .route(
            "/system/import/{session_id}/cancel",
            post(handlers::system::manual_import::cancel),
        )
        .route(
            "/api/library/recycle/empty",
            post(handlers::library::recycle::empty),
        )
        .route(
            "/api/library/recycle/{entry_id}/restore",
            post(handlers::library::recycle::restore),
        )
        .route(
            "/api/library/recycle/{entry_id}/purge",
            post(handlers::library::recycle::purge_entry),
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
            "/api/library/allow-pt-upgrades",
            post(handlers::library::crud::set_allow_pt_upgrades),
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
            "/api/library/bulk/monitor",
            post(handlers::library::bulk::bulk_monitor),
        )
        .route(
            "/api/library/bulk/delete",
            post(handlers::library::bulk::bulk_delete),
        )
        // Issue #114 — scoped API keys CRUD. All endpoints are
        // cookie-auth gated (the `protected_routes` group's
        // require_auth middleware wraps them). The plaintext is
        // surfaced exactly once on `create` and never again.
        .route(
            "/api/api-keys",
            get(handlers::api_keys::list).post(handlers::api_keys::create),
        )
        .route(
            "/api/api-keys/{id}/toggle",
            post(handlers::api_keys::toggle),
        )
        .route("/api/api-keys/{id}/reveal", get(handlers::api_keys::reveal))
        .route(
            "/api/api-keys/{id}/delete",
            post(handlers::api_keys::delete),
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
        // Issue #83 interactive file picker — new endpoints.
        // `/api/grab/preview` deliberately uses a different path than
        // the legacy `/api/grab` so existing clients keep working.
        .route("/api/grab/preview", post(handlers::grab::grab_preview))
        .route(
            "/api/grab/preview/{preview_id}",
            get(handlers::grab::grab_preview_status),
        )
        .route(
            "/api/grab/heartbeat/{preview_id}",
            post(handlers::grab::grab_heartbeat),
        )
        .route("/api/grab/confirm", post(handlers::grab::grab_confirm))
        .route("/api/grab/cancel", post(handlers::grab::grab_cancel))
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
            "/api/library/misgrabs/{id}/restore",
            post(handlers::library::misgrabs::restore_misgrab),
        )
        .route(
            "/api/library/misgrabs/{id}/dismiss",
            post(handlers::library::misgrabs::dismiss_misgrab),
        )
        .route(
            "/settings",
            get(handlers::settings::settings_page).post(handlers::settings::settings_submit),
        )
        // Issue #129 completion — per-tab subform handlers.
        // Each tab POSTs only its own fields to its dedicated route;
        // the legacy `/settings` POST above is the no-UI fallback for
        // any external bookmark or script still hitting the bulk
        // endpoint.
        .route(
            "/settings/general",
            post(handlers::settings::settings_general_submit),
        )
        // Issue #124: live preview for the naming templates on the
        // General tab. JSON in, JSON out, saves nothing.
        .route(
            "/api/settings/naming-preview",
            post(handlers::settings::naming::naming_preview),
        )
        // Issue #126: backup / restore. Cookie-auth only (protected
        // group): a backup carries the encryption key and every stored
        // password. The upload is a raw gzip body streamed to disk, so
        // the default 2 MB body limit is lifted for that one route.
        .route(
            "/api/backup/download",
            get(handlers::system::backup::api_backup_download),
        )
        .route(
            "/api/backup/run",
            post(handlers::system::backup::api_backup_run),
        )
        .route(
            "/api/tasks/backup",
            post(handlers::system::backup::api_backup_run),
        )
        .route(
            "/system/backup/run",
            post(handlers::system::backup::backup_run_form),
        )
        .route(
            "/api/backup/files/{name}",
            get(handlers::system::backup::api_backup_file),
        )
        .route(
            "/api/backup/files/{name}/delete",
            post(handlers::system::backup::backup_file_delete),
        )
        .route(
            "/api/restore/upload",
            post(handlers::system::backup::api_restore_upload).layer(
                axum::extract::DefaultBodyLimit::max(services::backup::MAX_UPLOAD_BYTES as usize),
            ),
        )
        .route(
            "/api/restore/cancel",
            post(handlers::system::backup::restore_cancel),
        )
        .route(
            "/settings/quality",
            post(handlers::settings::settings_quality_submit),
        )
        .route(
            "/settings/integrations",
            post(handlers::settings::settings_integrations_submit),
        )
        // Issue #62: AL + MAL OAuth endpoints. `start` GETs
        // redirect the user to the provider; `submit` POSTs accept
        // the pasted token/code, validate, and persist via
        // `external_accounts::link`. `unlink` drops the current row.
        .route(
            "/settings/oauth/anilist/start",
            get(handlers::oauth::anilist_start),
        )
        .route(
            "/settings/oauth/anilist/submit",
            post(handlers::oauth::anilist_submit),
        )
        .route("/settings/oauth/mal/start", get(handlers::oauth::mal_start))
        .route(
            "/settings/oauth/mal/submit",
            post(handlers::oauth::mal_submit),
        )
        .route(
            "/settings/oauth/preferences",
            post(handlers::oauth::update_preferences),
        )
        .route("/settings/oauth/unlink", post(handlers::oauth::unlink))
        .route("/settings/oauth/sync-now", post(handlers::oauth::sync_now))
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
            "/settings/indexers/upsert",
            post(handlers::settings::indexers::settings_indexers_upsert),
        )
        .route(
            "/settings/indexers/delete",
            post(handlers::settings::indexers::settings_indexers_delete),
        )
        .route(
            "/settings/indexers/section",
            get(handlers::settings::indexers::settings_indexers_section),
        )
        .route(
            "/settings/indexers/add-form",
            get(handlers::settings::indexers::settings_indexers_add_form),
        )
        .route(
            "/settings/indexers/{id}/edit-form",
            get(handlers::settings::indexers::settings_indexers_edit_form),
        )
        .route(
            "/settings/indexers/nyaa",
            post(handlers::settings::indexers::settings_indexers_nyaa_save),
        )
        .route(
            "/settings/indexers/nyaa/edit-form",
            get(handlers::settings::indexers::settings_indexers_nyaa_form),
        )
        .route(
            "/settings/indexers/test-rss",
            post(handlers::settings::indexers::settings_indexers_test_rss),
        )
        .route(
            "/api/indexers/test",
            post(handlers::settings::indexers::settings_indexers_test_stateless),
        )
        .route(
            "/settings/direct-rss-feeds/upsert",
            post(handlers::settings::direct_rss_feeds::settings_direct_rss_feeds_upsert),
        )
        .route(
            "/settings/direct-rss-feeds/delete",
            post(handlers::settings::direct_rss_feeds::settings_direct_rss_feeds_delete),
        )
        .route(
            "/settings/direct-rss-feeds/test",
            post(handlers::settings::direct_rss_feeds::settings_direct_rss_feeds_test),
        )
        .route(
            "/settings/download-clients/upsert",
            post(handlers::settings::download_clients::settings_download_clients_upsert),
        )
        .route(
            "/settings/download-clients/delete",
            post(handlers::settings::download_clients::settings_download_clients_delete),
        )
        .route(
            "/settings/download-clients/set-default",
            post(handlers::settings::download_clients::settings_download_clients_set_default),
        )
        .route(
            "/api/download-clients/test",
            post(handlers::settings::download_clients::settings_download_clients_test),
        )
        // Issue #119 — webhook provider test endpoint. Synthesizes a
        // Health event and POSTs to the targeted provider, returning
        // the receiver's HTTP status + body inline so users can debug
        // from the Settings UI without opening browser devtools.
        // Future per-provider CRUD endpoints land alongside this in
        // `handlers::notifications`.
        .route(
            "/api/notifications/{id}/test",
            post(handlers::notifications::test_provider),
        )
        // Issue gh-121 — notification provider CRUD via form-POST.
        // System → Notifications tab; cache rebuild happens inside
        // the upsert / delete handlers so the very next dispatch
        // sees the new shape.
        .route(
            "/system/notifications/upsert",
            post(handlers::system::notifications::notifications_upsert),
        )
        .route(
            "/system/notifications/delete",
            post(handlers::system::notifications::notifications_delete),
        )
        .route(
            "/system/notifications/{id}/edit-form",
            get(handlers::system::notifications::notifications_edit_form),
        )
        // gh-121 card+modal frontend — section refresh + add-form body
        // routes serve the HTMX swap targets used by the upsert/delete
        // post-save fragment-render and the add-modal click handler.
        .route(
            "/system/notifications/section",
            get(handlers::system::notifications::notifications_section),
        )
        .route(
            "/system/notifications/add-form",
            get(handlers::system::notifications::notifications_add_form),
        )
        .route(
            "/api/download-clients/section",
            get(handlers::settings::download_clients::settings_download_clients_section),
        )
        .route(
            "/settings/download-clients/{id}/edit-form",
            get(handlers::settings::download_clients::settings_download_clients_edit_form),
        )
        .route(
            "/api/download-clients/add-form",
            get(handlers::settings::download_clients::settings_download_clients_add_form),
        )
        .route(
            "/api/download-clients/{id}/status",
            get(handlers::settings::download_clients::settings_download_clients_status),
        )
        .route(
            "/settings/autobrr/regenerate-key",
            post(handlers::settings::autobrr_key::settings_autobrr_regenerate_key),
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
            "/api/tasks/airing-refresh",
            post(handlers::system::api_force_airing_refresh),
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
            "/api/tasks/external-sync",
            post(handlers::system::api_force_external_sync),
        )
        .route(
            "/api/system/rebuild-anilist-cache",
            post(handlers::system::api_rebuild_cached_metadata),
        )
        .route(
            "/api/system/reload-anibridge",
            post(handlers::system::api_anibridge_reload),
        )
        .route("/api/system/tasks", get(handlers::system::api_system_tasks))
        // Issue #116 — in-app calendar page. Cookie-auth gated
        // (rest of protected_routes); the iCal feed at
        // /api/calendar.ics is the parallel scoped-key surface
        // wired via `calendar_routes` further down. The same
        // handler serves full-page renders and HTMX partial
        // swaps (it branches on HxRequest), so there's no
        // separate JSON route.
        .route("/calendar", get(handlers::calendar::page))
        .route("/api/logs/poll", get(handlers::system::api_logs_poll))
        .route("/api/logs/clear", post(handlers::system::api_logs_clear))
        .route("/api/logs/export", get(handlers::system::api_logs_export))
        .route("/api/logs/client", post(handlers::system::api_logs_client))
        .route(
            "/api/progress/{job_id}",
            get(handlers::progress::poll_progress),
        )
        .route(
            "/api/progress/{job_id}/stream",
            get(handlers::progress::stream_progress),
        )
        .route("/media/art/{cache_key}", get(handlers::media::artwork))
        .route("/logout", get(handlers::auth::logout))
        // SwaggerUI/OpenAPI live behind the auth wall: the OpenAPI doc
        // describes the entire route surface and form schemas, including
        // the rate-limited /login and /setup shapes. Exposing it
        // unauthenticated would hand a passing scanner a complete map of
        // the application before any auth check fires.
        .merge(SwaggerUi::new("/api-docs").url("/api-docs/openapi.json", api_doc()))
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

    // Issue #28 — autobrr push webhook. Lives outside the
    // cookie-auth layer because autobrr authenticates via the
    // Ryokan-issued API key in `X-Api-Key` (or `?apikey=`).
    // Unlike the arr-compat shims, autobrr's check is inside the
    // handler itself rather than a middleware layer — the handler
    // also reads the body to make grab decisions, so the auth +
    // grab dispatch live in the same function.
    let webhook_routes = Router::new()
        .route(
            "/api/webhook/autobrr",
            post(handlers::webhook::autobrr::webhook_autobrr),
        )
        .with_state(state.clone());

    // Issue #115 — iCal calendar feed. Lives in its own router
    // group so it can carry the `require_calendar_scope`
    // middleware (scoped-API-key auth from #114) instead of
    // cookie-auth — calendar subscribers (Google Calendar / Apple
    // Calendar / Thunderbird) can't carry cookies.
    let calendar_routes = Router::new()
        .route("/api/calendar.ics", get(handlers::calendar::ical_feed))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::scoped_auth::require_calendar_scope,
        ))
        .with_state(state.clone());

    // Brotli/gzip compression. The series detail template is ~80KB of HTML
    // and style.css is ~64KB — both highly compressible (lots of repeated
    // tokens, whitespace), and they ship on every page navigation. Axum
    // negotiates via the client's Accept-Encoding automatically; if the
    // client doesn't advertise support, the body is passed through
    // unchanged.
    // Archives are already gzip: re-encoding them wastes CPU on the
    // slowest responses the app serves and, until the download stream
    // was fused, tripped the layer's end-of-body re-poll into a panic.
    // The predicate keeps the defaults (size floor, images, SSE) and
    // adds `application/gzip`.
    let compression = CompressionLayer::new()
        .br(true)
        .gzip(true)
        .compress_when(DefaultPredicate::new().and(NotForContentType::new("application/gzip")));

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
        .merge(calendar_routes)
        .merge(webhook_routes)
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
        "airing_refresh",
        "Episode air-date refresh",
        "Every 12 hours",
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
            let registry = rss_state.tasks.clone();
            supervise(&registry, "rss_sync", move || {
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
        let task_registry_metadata_refresh = state.tasks.clone();
        tokio::spawn(async move {
            supervise(
                &task_registry_metadata_refresh,
                "metadata_refresh",
                move || {
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
                },
            )
            .await;
        });
    }

    // Background task: refresh stamped episode air dates every 12
    // hours so the calendar reads from a freshly-warmed local table
    // instead of round-tripping to AniList per-request. Mirrors
    // Sonarr's `Episode.AirDateUtc` shape — one stamp at refresh
    // time, then the calendar's hot path is a plain SQL range scan
    // against `idx_episode_airings_at`. The refresh service holds
    // its own lock so a manual-trigger handler can't race the
    // scheduled tick. Startup delay is computed from
    // `scheduled_task_runs.last_finished_at` so a process restart
    // mid-window doesn't re-fire the sweep.
    {
        let airing_db = db.clone();
        let task_registry_airing = state.tasks.clone();
        tokio::spawn(async move {
            supervise(&task_registry_airing, "airing_refresh", move || {
                let db = airing_db.clone();
                async move {
                    let period = services::airing_refresh::REFRESH_INTERVAL;
                    let delay = models::scheduled_tasks::duration_until_next_run(
                        &db,
                        "airing_refresh",
                        period,
                    )
                    .await;
                    tokio::time::sleep(delay).await;
                    loop {
                        // Hold the process-wide lock for the duration
                        // of the run so a manual-trigger doesn't
                        // double up.
                        let _guard = services::airing_refresh::AIRING_REFRESH_LOCK.lock().await;
                        let _ = models::scheduled_tasks::mark_started(
                            &db,
                            "airing_refresh",
                            "Refreshing episode air dates",
                        )
                        .await;
                        match services::airing_refresh::refresh_all(&db).await {
                            Ok(summary) => {
                                let status = if summary.al_failures > 0 {
                                    "warn"
                                } else {
                                    "ok"
                                };
                                let _ = models::scheduled_tasks::mark_finished(
                                    &db,
                                    "airing_refresh",
                                    status,
                                    &summary.detail(),
                                )
                                .await;
                            }
                            Err(err) => {
                                let _ = models::scheduled_tasks::mark_finished(
                                    &db,
                                    "airing_refresh",
                                    "error",
                                    &err,
                                )
                                .await;
                            }
                        }
                        drop(_guard);
                        tokio::time::sleep(period).await;
                    }
                }
            })
            .await;
        });
    }

    // Background task: clean up logs and old RSS decisions older than 30 days every hour.
    // Issue #126: scheduled backups. Hourly tick; a run happens when
    // the newest scheduled backup in the folder is older than the
    // configured cadence, so a restart never triggers an extra one and
    // a missed day is caught up on the next tick. The folder is the
    // state; nothing about "last backup" is stored in the DB.
    {
        let backup_db = db.clone();
        let task_registry_backup = state.tasks.clone();
        tokio::spawn(async move {
            supervise(&task_registry_backup, "backup", move || {
                let db = backup_db.clone();
                async move {
                    let touch = |db: sqlx::SqlitePool, enabled: bool| async move {
                        let _ = models::scheduled_tasks::touch_definition(
                            &db,
                            "backup",
                            "Backup",
                            "Daily or weekly (Settings → General)",
                            enabled,
                        )
                        .await;
                    };
                    let initial = models::config::get_config(&db)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    touch(db.clone(), initial.backup_schedule != "disabled").await;
                    // Let boot settle before the first check.
                    tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                    loop {
                        let cfg = models::config::get_config(&db)
                            .await
                            .ok()
                            .flatten()
                            .unwrap_or_default();
                        touch(db.clone(), cfg.backup_schedule != "disabled").await;
                        if let Some(interval) =
                            services::backup::schedule_interval(&cfg.backup_schedule)
                        {
                            let paths = services::backup::BackupPaths::from_env();
                            let dir = paths.backup_dir(&cfg.backup_directory);
                            if services::backup::is_due(&dir, interval) {
                                let _ = models::scheduled_tasks::mark_started(
                                    &db,
                                    "backup",
                                    "Scheduled backup",
                                )
                                .await;
                                match services::backup::run_to_folder(&db, &paths, &cfg).await {
                                    Ok(run) => {
                                        let detail = format!(
                                            "{} written to {}{}",
                                            run.file_name,
                                            run.dir.display(),
                                            if run.pruned.is_empty() {
                                                String::new()
                                            } else {
                                                format!(", pruned {}", run.pruned.join(", "))
                                            }
                                        );
                                        services::logger::info(
                                            &db,
                                            models::log::LogCategory::System,
                                            "Scheduled backup complete",
                                            &detail,
                                        )
                                        .await;
                                        let _ = models::scheduled_tasks::mark_finished(
                                            &db, "backup", "ok", &detail,
                                        )
                                        .await;
                                    }
                                    Err(e) => {
                                        let msg = e.to_string();
                                        services::logger::error(
                                            &db,
                                            models::log::LogCategory::System,
                                            "Scheduled backup failed",
                                            &msg,
                                        )
                                        .await;
                                        let _ = models::scheduled_tasks::mark_finished(
                                            &db, "backup", "error", &msg,
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    }
                }
            })
            .await;
        });
    }

    {
        let cleanup_db = db.clone();
        let task_registry_cleanup = state.tasks.clone();
        tokio::spawn(async move {
            supervise(&task_registry_cleanup, "cleanup", move || {
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
                        let cleanup_cfg = models::config::get_config(&cleanup_db)
                            .await
                            .ok()
                            .flatten();
                        // Recycle-bin purge (#123). Date buckets older than
                        // `recycle_bin_age_days` are dropped; 0 disables the
                        // sweep and an empty path means no bin at all.
                        if let Some(cfg) = cleanup_cfg
                            .as_ref()
                            .filter(|c| !c.recycle_bin_path.trim().is_empty())
                        {
                            match services::recycle::purge_old(
                                &cfg.recycle_bin_path,
                                cfg.recycle_bin_age_days,
                            )
                            .await
                            {
                                Ok(report) if report.entries > 0 || report.date_dirs > 0 => {
                                    services::logger::info(
                                        &cleanup_db,
                                        models::log::LogCategory::System,
                                        &format!(
                                            "Recycle bin purge removed {} entr{} ({})",
                                            report.entries,
                                            if report.entries == 1 { "y" } else { "ies" },
                                            services::recycle::human_bytes(report.bytes)
                                        ),
                                        &format!(
                                            "age_days={} date_dirs={} bytes={}",
                                            cfg.recycle_bin_age_days,
                                            report.date_dirs,
                                            report.bytes
                                        ),
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    cleanup_errors.push(format!("recycle purge: {}", e));
                                    tracing::error!("Recycle bin purge failed: {}", e);
                                }
                                _ => {}
                            }
                        }
                        // Orphaned temp-file sweep (#205). An import that
                        // died mid-copy leaves `<dest>.ryokan-tmp` or
                        // `.<name>.ryokan-new` in the season folder; nothing
                        // else ever looks at those again. The sweep removes
                        // only under both import locks and skips the hour
                        // when an import is running.
                        if let Some(cfg) = cleanup_cfg
                            .as_ref()
                            .filter(|c| !c.media_root.trim().is_empty())
                        {
                            use services::post_processing::temp_sweep;
                            match temp_sweep::sweep_orphaned_temp_files(
                                &cleanup_db,
                                &cfg.media_root,
                                &cfg.recycle_bin_path,
                                temp_sweep::ORPHAN_MIN_AGE,
                            )
                            .await
                            {
                                Ok(report) => {
                                    if report.skipped_busy {
                                        tracing::debug!(
                                            "Temp-file sweep skipped: an import is running; next hour"
                                        );
                                    }
                                    if !report.removed.is_empty() {
                                        let listed: Vec<String> = report
                                            .removed
                                            .iter()
                                            .take(10)
                                            .map(|p| p.display().to_string())
                                            .collect();
                                        // `bytes` is what was freed: a file moved
                                        // to the bin still occupies the disk there,
                                        // so an all-recycled pass reports the move
                                        // and no size.
                                        let mut notes = Vec::new();
                                        if report.bytes > 0 {
                                            notes.push(format!(
                                                "{} freed",
                                                services::recycle::human_bytes(report.bytes)
                                            ));
                                        }
                                        if report.recycled > 0 {
                                            notes.push(format!(
                                                "{} moved to the recycle bin",
                                                report.recycled
                                            ));
                                        }
                                        let notes = if notes.is_empty() {
                                            String::new()
                                        } else {
                                            format!(" ({})", notes.join(", "))
                                        };
                                        services::logger::info(
                                            &cleanup_db,
                                            models::log::LogCategory::PostProcess,
                                            &format!(
                                                "Removed {} leftover temporary file{} from the media library{}",
                                                report.removed.len(),
                                                if report.removed.len() == 1 { "" } else { "s" },
                                                notes
                                            ),
                                            &format!(
                                                "left by an import that stopped mid-copy; older than {}h; kept_recent={} files={}{}",
                                                temp_sweep::ORPHAN_MIN_AGE.as_secs() / 3600,
                                                report.kept_recent,
                                                listed.join(", "),
                                                if report.removed.len() > listed.len() {
                                                    ", ..."
                                                } else {
                                                    ""
                                                }
                                            ),
                                        )
                                        .await;
                                    }
                                    if !report.errors.is_empty() {
                                        let joined = report.errors.join("; ");
                                        cleanup_errors.push(format!("temp sweep: {joined}"));
                                        services::logger::warn(
                                            &cleanup_db,
                                            models::log::LogCategory::PostProcess,
                                            &format!(
                                                "Temp-file sweep hit {} error{}",
                                                report.errors.len(),
                                                if report.errors.len() == 1 { "" } else { "s" }
                                            ),
                                            &joined,
                                        )
                                        .await;
                                    }
                                }
                                Err(e) => {
                                    cleanup_errors.push(format!("temp sweep: {}", e));
                                    services::logger::error(
                                        &cleanup_db,
                                        models::log::LogCategory::PostProcess,
                                        "Temp-file sweep failed",
                                        &e,
                                    )
                                    .await;
                                }
                            }
                        }
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
            let registry = pp_state.tasks.clone();
            supervise(&registry, "post_processing", move || {
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
                        // Issue #228: remove imported torrents whose
                        // client reports seeding finished (throttled to
                        // every 5 minutes inside).
                        services::post_processing::sweep_finished_seeds(&pp_state).await;
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
            let registry = classify_state.tasks.clone();
            supervise(&registry, "library_classify", move || {
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
                        // Hold the process-wide lock so a Run-now
                        // click during this 6h tick blocks instead
                        // of interleaving and flipping the row's
                        // status between two concurrent writes.
                        let _guard = services::post_processing::LIBRARY_CLASSIFY_LOCK
                            .lock()
                            .await;
                        let report = services::post_processing::scan_library_for_unclassified(
                            &classify_state,
                        )
                        .await;
                        drop(_guard);
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
            let registry = upgrade_state.tasks.clone();
            supervise(&registry, "upgrade_search", move || {
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
        let task_registry_anibridge_refresh = state.tasks.clone();
        tokio::spawn(async move {
            supervise(
                &task_registry_anibridge_refresh,
                "anibridge_refresh",
                move || {
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
                },
            )
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
            let registry = progress_state.tasks.clone();
            supervise(&registry, "progress_sweep", move || {
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

    // Background task: auto-commit or evict stale `pending_grabs`
    // rows (issue #83, plan decision #3). A walkaway tab's torrent
    // is still a user-intended download — the sweep marks every
    // file wanted and resumes the torrent. See
    // `services::grab_sweep` module docstring for the full
    // per-row flow, including the error-row and no-metadata
    // branches that skip auto-commit but still delete the row.
    {
        let grab_sweep_state = state.clone();
        tokio::spawn(async move {
            let registry = grab_sweep_state.tasks.clone();
            supervise(&registry, "grab_sweep", move || {
                let state = grab_sweep_state.clone();
                async move {
                    let mut interval = tokio::time::interval(services::grab_sweep::SWEEP_INTERVAL);
                    loop {
                        interval.tick().await;
                        if let Err(e) = services::grab_sweep::sweep_once(&state).await {
                            tracing::warn!(
                                target: "ryokan::grab_sweep",
                                error = %e,
                                "sweep_once failed; will retry on next tick"
                            );
                        }
                    }
                }
            })
            .await;
        });
    }

    // Misgrab guardrails: verify unchecked grabs against their file
    // list and remediate detected misgrabs (delete, blocklist, notify,
    // re-search). Same one-tick-per-call shape as grab_sweep.
    {
        let misgrab_state = state.clone();
        tokio::spawn(async move {
            let registry = misgrab_state.tasks.clone();
            supervise(&registry, "misgrab_sweep", move || {
                let state = misgrab_state.clone();
                async move {
                    let mut interval = tokio::time::interval(services::misgrab::SWEEP_INTERVAL);
                    loop {
                        interval.tick().await;
                        match services::misgrab::sweep_once(&state).await {
                            Ok(summary) if summary.misgrabs > 0 || summary.remediated > 0 => {
                                tracing::info!(
                                    target: "ryokan::misgrab_sweep",
                                    verified = summary.verified,
                                    misgrabs = summary.misgrabs,
                                    unverifiable = summary.unverifiable,
                                    remediated = summary.remediated,
                                    "misgrab sweep tick"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(
                                    target: "ryokan::misgrab_sweep",
                                    error = %e,
                                    "sweep_once failed; will retry on next tick"
                                );
                            }
                        }
                    }
                }
            })
            .await;
        });
    }

    // Issue #62 — watch-list sync. One of the supervised tasks.
    // Same minute-tick + minutes_since_last cadence pattern as
    // rss_sync (so a process restart respects the persisted
    // last-finished timestamp instead of forcing an immediate
    // re-run). The interval is user-configurable on Settings →
    // Integrations and is clamped on read so a hand-edited DB row
    // can't push the cadence outside 15..=10080 minutes.
    //
    // The actual fetch + merge is a no-op stub at this commit;
    // subsequent commits build the AL `MediaListCollection` query,
    // MAL animelist + token-refresh, and the staging-table merge.
    {
        let ext_sync_state = state.clone();
        tokio::spawn(async move {
            let registry = ext_sync_state.tasks.clone();
            supervise(&registry, "external_sync", move || {
                let state = ext_sync_state.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                    let mut minutes_since_last: i64 =
                        services::external_sync::minutes_since_last_run(&state.db).await;
                    let mut consecutive_errors: i64 = 0;
                    loop {
                        interval.tick().await;
                        minutes_since_last += 1;

                        let cfg = match models::config::get_config(&state.db).await {
                            Ok(Some(cfg)) => cfg,
                            _ => continue,
                        };

                        let every = (cfg.external_sync_interval_minutes as i64).clamp(15, 10080);

                        // Reflect link status in the task row so the
                        // System → Scheduled Tasks UI shows "disabled"
                        // when no account is connected. The supervised
                        // loop still runs (so a fresh link picks up on
                        // the next minute tick) but the row's enabled
                        // flag is the right signal that this task is
                        // dormant by design rather than off due to a
                        // user toggle.
                        let has_linked =
                            services::external_sync::has_linked_account(&state.db).await;
                        let _ = models::scheduled_tasks::touch_definition(
                            &state.db,
                            "external_sync",
                            "External account sync",
                            &format!("Every {} minutes", every),
                            has_linked,
                        )
                        .await;

                        // Exponential backoff on consecutive errors:
                        // skip 2^errors extra intervals (errors capped
                        // at 5 → max 32x multiplier). Outer ceiling
                        // caps the wait at max(every, 24h) so a 7-day
                        // cadence with five errors doesn't push the
                        // next retry seven months out — `.max(every)`
                        // prevents the ceiling from retrying SOONER
                        // than the configured cadence.
                        const SUPERVISED_BACKOFF_CEILING_MIN: i64 = 24 * 60;
                        let backoff = if consecutive_errors > 0 {
                            (every.saturating_mul(1i64 << consecutive_errors.min(5)))
                                .min(SUPERVISED_BACKOFF_CEILING_MIN.max(every))
                        } else {
                            every
                        };
                        if minutes_since_last < backoff {
                            continue;
                        }

                        // No linked account → don't churn
                        // scheduled_task_runs with one "no account"
                        // row per cadence interval. Leave the counter
                        // alone so the next 1-minute tick re-checks
                        // `has_linked_account` immediately (mins is
                        // already past `backoff` here). Resetting to
                        // 0 would force a fresh `every`-minute wait
                        // after the user actually links — bad UX,
                        // and the comment that used to be here lied
                        // about it.
                        if !has_linked {
                            continue;
                        }

                        minutes_since_last = 0;
                        let _ = models::scheduled_tasks::mark_started(
                            &state.db,
                            "external_sync",
                            "Watch-list sync started",
                        )
                        .await;

                        match services::external_sync::tick_once(&state).await {
                            Ok(detail) => {
                                consecutive_errors = 0;
                                let _ = models::scheduled_tasks::mark_finished(
                                    &state.db,
                                    "external_sync",
                                    "ok",
                                    &detail,
                                )
                                .await;
                            }
                            Err(err) => {
                                consecutive_errors += 1;
                                let _ = models::scheduled_tasks::mark_finished(
                                    &state.db,
                                    "external_sync",
                                    "error",
                                    &err,
                                )
                                .await;
                                services::logger::error(
                                    &state.db,
                                    models::log::LogCategory::ExternalSync,
                                    "External sync failed",
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

#[cfg(test)]
mod api_doc_tests {
    use super::api_doc;

    #[test]
    fn version_follows_cargo_toml() {
        assert_eq!(api_doc().info.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn once_missing_routes_are_documented() {
        let doc = api_doc();
        for path in [
            "/api/backup/download",
            "/api/restore/upload",
            "/api/library/recycle/{entry_id}/restore",
            "/api/calendar.ics",
            "/api/notifications/{id}/test",
            "/api/settings/naming-preview",
            "/api/api-keys",
        ] {
            assert!(
                doc.paths.paths.contains_key(path),
                "{path} missing from the OpenAPI document"
            );
        }
    }
}
