//! Fixture host for the browser tests in this folder.
//!
//! The tests are JavaScript (Playwright), so they cannot build an
//! `AppState` or call a model function the way a Rust test does. This
//! binary does that part for them. It serves a small control API on a
//! loopback port; each `POST /apps` builds what one Rust browser test
//! used to build in-process: a fresh in-memory database, a logged-in
//! user, the `test_support` state, and the fixture router
//! (`test_support::e2e_browser_app`) on its own port. The test then
//! drives a browser against that port and reads the database back
//! through `POST /apps/{id}/sql`.
//!
//! Test-only: the target needs the `browser-e2e` feature, so a release
//! build, the Docker image, and CI's test run never compile it.
//! `scripts/browser-e2e.sh` builds it and runs the suite.
//!
//! Protocol: the first line on stdout is `{"control":"http://127.0.0.1:<port>"}`.
//! The host exits when its stdin closes, so it cannot outlive the test
//! runner that started it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ryokan::models::config::{self, Config};
use ryokan::models::download_clients::DownloadClientForm;
use ryokan::models::indexers::IndexerForm;
use ryokan::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};
use ryokan::services::source::Source;
use ryokan::test_support::{build_test_app_state, e2e_browser_app, in_memory_pool, seed_series};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::sqlite::SqliteRow;
use sqlx::{Column, Row, SqlitePool, TypeInfo, ValueRef};

/// A queue item id that ends an inline `onclick` JS string. HTML
/// escaping doesn't stop it there: the parser decodes `&#39;` back to
/// `'` before the handler runs. `grab-bag.spec.js` expects this exact id.
const HOSTILE_HASH: &str = "x');window.__pwned=1;('";

/// One downloading queue item under [`HOSTILE_HASH`]; records pauses.
#[derive(Default)]
struct QueueClient {
    pauses: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl DownloadClient for QueueClient {
    async fn test(&self) -> Result<String, String> {
        Ok("mock".into())
    }
    async fn add_torrent(&self, _url: &str, _hash: &str) -> Result<AddOutcome, String> {
        Ok(AddOutcome::Added)
    }
    async fn add_torrent_with_file_filter(
        &self,
        _url: &str,
        _hash: &str,
        _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        Ok(SelectiveOutcome::FullDownload)
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(vec![DownloadItem {
            hash: HOSTILE_HASH.to_string(),
            name: "Queue Show - 01".to_string(),
            size: 1 << 20,
            progress: 0.5,
            dlspeed: 0,
            state: "downloading".to_string(),
            category: "anime".to_string(),
            eta: 0,
            save_path: String::new(),
            content_path: String::new(),
            state_kind: DownloadItemState::Downloading,
            seeding_done: false,
        }])
    }
    async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(vec![])
    }
    async fn pause(&self, hash: &str) -> Result<(), String> {
        self.pauses.lock().unwrap().push(hash.to_string());
        Ok(())
    }
    async fn resume(&self, _hash: &str) -> Result<(), String> {
        Ok(())
    }
    async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
        Ok(())
    }
    async fn set_file_wanted(
        &self,
        _hash: &str,
        _files: &[usize],
        _wanted: bool,
    ) -> Result<(), String> {
        Ok(())
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

/// One test's application: its database, its fake client if it asked
/// for one, and the task serving its router.
struct App {
    db: SqlitePool,
    queue: Option<Arc<QueueClient>>,
    server: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Default)]
struct Host {
    apps: Arc<Mutex<HashMap<u64, Arc<App>>>>,
    next_id: Arc<AtomicU64>,
}

type Failure = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> Failure {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(e: impl std::fmt::Display) -> Failure {
    (StatusCode::BAD_REQUEST, e.to_string())
}

impl Host {
    fn app(&self, id: u64) -> Result<Arc<App>, Failure> {
        self.apps
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or((StatusCode::NOT_FOUND, format!("no app {id}")))
    }
}

#[derive(Deserialize)]
struct CreateApp {
    /// Give the app the fake download client: one queue item whose id
    /// is [`HOSTILE_HASH`], pauses recorded for `GET /apps/{id}/pauses`.
    #[serde(default)]
    queue_client: bool,
}

async fn create_app(
    State(host): State<Host>,
    Json(req): Json<CreateApp>,
) -> Result<Json<Value>, Failure> {
    let db = in_memory_pool().await;
    let user_id =
        ryokan::models::user::create_user(&db, "browser-e2e-user", "hunter2-test-password")
            .await
            .map_err(internal)?;
    let session = ryokan::models::session::create_session(&db, user_id)
        .await
        .map_err(internal)?;
    let queue = req.queue_client.then(|| Arc::new(QueueClient::default()));
    let client = queue.clone().map(|q| q as Arc<dyn DownloadClient>);
    let state = build_test_app_state(db.clone(), client);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(internal)?;
    let addr = listener.local_addr().map_err(internal)?;
    let router = e2e_browser_app(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let id = host.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    host.apps
        .lock()
        .unwrap()
        .insert(id, Arc::new(App { db, queue, server }));
    Ok(Json(json!({
        "id": id,
        "base_url": format!("http://{addr}"),
        "session": session,
    })))
}

async fn drop_app(State(host): State<Host>, Path(id): Path<u64>) -> StatusCode {
    if let Some(app) = host.apps.lock().unwrap().remove(&id) {
        app.server.abort();
    }
    StatusCode::NO_CONTENT
}

/// The seeds the suite uses, each through the model function a Rust
/// test would call, so a seeded row has the shape the app writes.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Seed {
    Series {
        anilist_id: i64,
        title: String,
    },
    EpisodeMonitored {
        series_id: i64,
        episode: i32,
        monitored: bool,
    },
    Indexer {
        name: String,
        /// Defaults to `abc`.
        api_key: Option<String>,
    },
    DownloadClient {
        name: String,
        #[serde(default)]
        is_default: bool,
    },
    CustomFormat {
        name: String,
    },
    Group {
        name: String,
    },
    /// The `config` row a fresh install has: `Config::default()` saved.
    ConfigDefault,
    /// A `config` row with a distinct, non-default value in every field
    /// the three Settings sub-forms own, so a save that writes a form
    /// default over a field another tab owns shows up.
    ConfigDistinct,
    Grab {
        hash: String,
        name: String,
        series_id: i64,
        episodes: Vec<i32>,
        #[serde(default)]
        batch: bool,
    },
}

async fn seed(
    State(host): State<Host>,
    Path(id): Path<u64>,
    Json(seed): Json<Seed>,
) -> Result<Json<Value>, Failure> {
    let app = host.app(id)?;
    let db = &app.db;
    let row_id: Option<i64> = match seed {
        Seed::Series { anilist_id, title } => Some(seed_series(db, anilist_id, &title).await),
        Seed::EpisodeMonitored {
            series_id,
            episode,
            monitored,
        } => {
            ryokan::models::monitoring::set_episode_monitored(db, series_id, episode, monitored)
                .await
                .map_err(internal)?;
            None
        }
        Seed::Indexer { name, api_key } => Some(
            ryokan::models::indexers::insert(
                db,
                IndexerForm {
                    name: &name,
                    kind: "torznab",
                    url: "https://example.com/torznab",
                    api_key: api_key.as_deref().unwrap_or("abc"),
                    priority: 25,
                    enabled: true,
                    is_private_tracker: false,
                    seed_ratio: None,
                    seed_time_minutes: None,
                    min_seeders: 1,
                    request_timeout_secs: None,
                    download_client_id: None,
                    rss_enabled: false,
                    categories: "",
                },
            )
            .await
            .map_err(internal)?,
        ),
        Seed::DownloadClient { name, is_default } => Some(
            ryokan::models::download_clients::insert(
                db,
                DownloadClientForm {
                    name: &name,
                    kind: "qbittorrent",
                    url: "http://qbit.local",
                    username: "",
                    password: "",
                    label: "",
                    download_path: "",
                    enabled: true,
                    is_default,
                },
            )
            .await
            .map_err(internal)?,
        ),
        Seed::CustomFormat { name } => Some(
            ryokan::models::custom_formats::insert(db, &name, None, "{}", 0, "manual")
                .await
                .map_err(internal)?,
        ),
        Seed::Group { name } => {
            ryokan::models::group_source_map::upsert_user_edit(
                db,
                &name,
                Source::BluRay,
                1.0,
                "test seed",
            )
            .await
            .map_err(internal)?;
            None
        }
        Seed::ConfigDefault => {
            config::save_config(db, &Config::default())
                .await
                .map_err(internal)?;
            None
        }
        Seed::ConfigDistinct => {
            config::save_config(db, &distinct_config())
                .await
                .map_err(internal)?;
            None
        }
        Seed::Grab {
            hash,
            name,
            series_id,
            episodes,
            batch,
        } => ryokan::models::grabbed_torrents::record_grab(
            db, &hash, &name, series_id, &episodes, batch,
        )
        .await
        .map_err(internal)?,
    };
    Ok(Json(json!({ "id": row_id })))
}

fn distinct_config() -> Config {
    Config {
        // Integrations-tab fields.
        jellyfin_url: "http://seed-integrations.local:8096".to_string(),
        jellyfin_api_key: "seed-jellyfin-key".to_string(),
        sonarr_enabled: true,
        sonarr_api_key: "seed-sonarr-key-0123456789".to_string(),
        radarr_enabled: false,
        radarr_api_key: "seed-radarr-key-0123456789".to_string(),
        grab_preview_mode: "never".to_string(),
        external_sync_interval_minutes: 90,

        // Quality-tab fields.
        preferred_groups: "SeedGroup1, SeedGroup2".to_string(),
        blocked_groups: "SeedBlocked".to_string(),
        preferred_source: "bluray".to_string(),
        preferred_resolution: "720".to_string(),
        cutoff_source: "bluray_remux".to_string(),
        cutoff_resolution: "1080".to_string(),
        finished_series_quality: "bd_only".to_string(),
        prefer_subs: false,
        upgrade_search_enabled: true,
        seadex_enabled: true,
        default_custom_query_tokens: "seed query".to_string(),
        default_restrict_to_uploader: "SeedUploader".to_string(),

        // General-tab fields.
        media_root: "/srv/seed-media".to_string(),
        title_language: "native".to_string(),
        rss_enabled: false,
        rss_interval_minutes: 33,
        disable_nyaa_rss: true,
        post_processing_enabled: false,
        post_processing_mode: "copy".to_string(),
        search_on_monitoring_change: true,

        ..Config::default()
    }
}

#[derive(Deserialize)]
struct SqlRequest {
    sql: String,
    #[serde(default)]
    params: Vec<Value>,
}

/// Run one statement against the app's database and return its rows.
/// This is how a test reads a side effect back, and how it writes the
/// odd row no seed covers. The SQL comes from the test files in this
/// folder; nothing a browser or a user typed reaches it.
async fn sql(
    State(host): State<Host>,
    Path(id): Path<u64>,
    Json(req): Json<SqlRequest>,
) -> Result<Json<Value>, Failure> {
    let app = host.app(id)?;
    let mut query = sqlx::query(sqlx::AssertSqlSafe(req.sql));
    for param in req.params {
        query = match param {
            Value::Null => query.bind(None::<i64>),
            Value::Bool(b) => query.bind(b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => query.bind(i),
                None => query.bind(n.as_f64()),
            },
            Value::String(s) => query.bind(s),
            other => query.bind(other.to_string()),
        };
    }
    let rows = query.fetch_all(&app.db).await.map_err(bad_request)?;
    let rows: Vec<Value> = rows.iter().map(row_to_json).collect();
    Ok(Json(json!({ "rows": rows })))
}

fn row_to_json(row: &SqliteRow) -> Value {
    let mut out = serde_json::Map::new();
    for (i, column) in row.columns().iter().enumerate() {
        let value = match row.try_get_raw(i) {
            Ok(raw) if raw.is_null() => Value::Null,
            Ok(raw) => match raw.type_info().name() {
                "INTEGER" | "BOOLEAN" => row.try_get::<i64, _>(i).map(Value::from).ok(),
                "REAL" => row.try_get::<f64, _>(i).map(Value::from).ok(),
                "BLOB" => row
                    .try_get::<Vec<u8>, _>(i)
                    .map(|b| Value::from(hex::encode(b)))
                    .ok(),
                _ => row.try_get::<String, _>(i).map(Value::from).ok(),
            }
            .unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        out.insert(column.name().to_string(), value);
    }
    Value::Object(out)
}

#[derive(Deserialize)]
struct SessionRequest {
    token: String,
}

/// Delete a session row the way logging out elsewhere would. The
/// table stores a digest of the token, so a test cannot do this in SQL.
async fn delete_session(
    State(host): State<Host>,
    Path(id): Path<u64>,
    Json(req): Json<SessionRequest>,
) -> Result<StatusCode, Failure> {
    let app = host.app(id)?;
    ryokan::models::session::delete_session(&app.db, &req.token)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn session_valid(
    State(host): State<Host>,
    Path(id): Path<u64>,
    Json(req): Json<SessionRequest>,
) -> Result<Json<Value>, Failure> {
    let app = host.app(id)?;
    let user = ryokan::models::session::validate_session(&app.db, &req.token)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "valid": user.is_some() })))
}

/// The ids the fake download client was asked to pause, in order.
async fn pauses(State(host): State<Host>, Path(id): Path<u64>) -> Result<Json<Value>, Failure> {
    let app = host.app(id)?;
    let queue = app
        .queue
        .as_ref()
        .ok_or_else(|| bad_request("this app was created without queue_client"))?;
    let pauses = queue.pauses.lock().unwrap().clone();
    Ok(Json(json!({ "pauses": pauses })))
}

#[tokio::main]
async fn main() {
    // Quiet unless asked: `RUST_LOG=ryokan=debug` shows what the app
    // under test logs, on stderr so stdout stays the one protocol line.
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init();
    }

    let control = Router::new()
        .route("/apps", post(create_app))
        .route("/apps/{id}", delete(drop_app))
        .route("/apps/{id}/seed", post(seed))
        .route("/apps/{id}/sql", post(sql))
        .route("/apps/{id}/session/delete", post(delete_session))
        .route("/apps/{id}/session/valid", post(session_valid))
        .route("/apps/{id}/pauses", get(pauses))
        .with_state(Host::default());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the control port");
    let addr = listener.local_addr().expect("control address");
    println!("{}", json!({ "control": format!("http://{addr}") }));

    // Exit with the runner: it holds our stdin open and never writes.
    tokio::spawn(async {
        use tokio::io::AsyncReadExt;
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(_) => {}
            }
        }
    });

    axum::serve(listener, control)
        .await
        .expect("serve the control API");
}
