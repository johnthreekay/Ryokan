use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// Log levels, ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "trace" => LogLevel::Trace,
            "debug" => LogLevel::Debug,
            "warn" => LogLevel::Warn,
            "error" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }

    /// Ordinal severity (0–4) for comparison. Higher = more severe.
    /// Used by the write-side min-level filter on the `logs` table so
    /// a LogLevel::Debug emission is dropped when the configured floor
    /// is Info or above.
    pub fn severity(&self) -> u8 {
        match self {
            LogLevel::Trace => 0,
            LogLevel::Debug => 1,
            LogLevel::Info => 2,
            LogLevel::Warn => 3,
            LogLevel::Error => 4,
        }
    }
}

/// Categories for log entries, matching the major subsystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogCategory {
    Search,
    Grab,
    AutoSearch,
    Nyaa,
    AniList,
    Jikan,
    QBit,
    Jellyfin,
    Media,
    Library,
    Auth,
    System,
    PostProcess,
    /// Quality-tag classification pipeline (`source*` modules). Per-decision
    /// debug records emit under this category so the logs page can be
    /// filtered to just classification trace output.
    Quality,
    /// Per-candidate release scoring breakdown (`services/auto_search.rs`).
    /// Emits one debug row per scored candidate with the CF breakdown so
    /// users can introspect "why did release X win over release Y" — see
    /// plan §6.3 of `ryokan-custom-formats.md`.
    Scoring,
}

impl LogCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogCategory::Search => "search",
            LogCategory::Grab => "grab",
            LogCategory::AutoSearch => "auto_search",
            LogCategory::Nyaa => "nyaa",
            LogCategory::AniList => "anilist",
            LogCategory::Jikan => "jikan",
            LogCategory::QBit => "qbit",
            LogCategory::Jellyfin => "jellyfin",
            LogCategory::Media => "media",
            LogCategory::Library => "library",
            LogCategory::Auth => "auth",
            LogCategory::System => "system",
            LogCategory::PostProcess => "post_process",
            LogCategory::Quality => "quality",
            LogCategory::Scoring => "scoring",
        }
    }

    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "search" => Some(LogCategory::Search),
            "grab" => Some(LogCategory::Grab),
            "auto_search" => Some(LogCategory::AutoSearch),
            "nyaa" => Some(LogCategory::Nyaa),
            "anilist" => Some(LogCategory::AniList),
            "jikan" => Some(LogCategory::Jikan),
            "qbit" => Some(LogCategory::QBit),
            "jellyfin" => Some(LogCategory::Jellyfin),
            "media" => Some(LogCategory::Media),
            "library" => Some(LogCategory::Library),
            "auth" => Some(LogCategory::Auth),
            "system" => Some(LogCategory::System),
            "post_process" => Some(LogCategory::PostProcess),
            "quality" => Some(LogCategory::Quality),
            "scoring" => Some(LogCategory::Scoring),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            LogCategory::Search => "Search",
            LogCategory::Grab => "Grab",
            LogCategory::AutoSearch => "Auto Search",
            LogCategory::Nyaa => "Nyaa",
            LogCategory::AniList => "AniList",
            LogCategory::Jikan => "Jikan",
            LogCategory::QBit => "qBittorrent",
            LogCategory::Jellyfin => "Jellyfin",
            LogCategory::Media => "Media",
            LogCategory::Library => "Library",
            LogCategory::Auth => "Auth",
            LogCategory::System => "System",
            LogCategory::PostProcess => "Post-Process",
            LogCategory::Quality => "Quality",
            LogCategory::Scoring => "Scoring",
        }
    }
}

/// A single log entry as stored in the database.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct LogEntry {
    pub id: i64,
    pub timestamp: String,
    pub level: String,
    pub category: String,
    pub message: String,
    pub detail: String,
}

/// Query parameters for fetching logs.
pub struct LogQuery {
    pub level: Option<String>,
    pub category: Option<String>,
    pub search: Option<String>,
    pub limit: i64,
    pub before_id: Option<i64>,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            level: None,
            category: None,
            search: None,
            limit: 200,
            before_id: None,
        }
    }
}

/// Insert a log entry.
pub async fn insert(
    db: &SqlitePool,
    level: LogLevel,
    category: LogCategory,
    message: &str,
    detail: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO logs (level, category, message, detail) VALUES (?, ?, ?, ?)")
        .bind(level.as_str())
        .bind(category.as_str())
        .bind(message)
        .bind(detail)
        .execute(db)
        .await?;
    Ok(())
}

/// A single dynamically-bound parameter. Keeps numeric bindings
/// (`i64`) in their native type instead of round-tripping through
/// `String`, which matters for SQLite index usage: an integer column
/// compared against a text-bound value goes through NUMERIC-affinity
/// coercion and can lose its index, while a native `i64` bind is a
/// straight `INTEGER == INTEGER` compare. `LIMIT ?` and `id > ?` are
/// the two call sites that care.
enum BindValue {
    Int(i64),
    Text(String),
}

/// Query log entries with optional filters. Returns newest first.
pub async fn query(db: &SqlitePool, params: &LogQuery) -> Result<Vec<LogEntry>, sqlx::Error> {
    let mut sql =
        String::from("SELECT id, timestamp, level, category, message, detail FROM logs WHERE 1=1");
    let mut binds: Vec<BindValue> = Vec::new();

    if let Some(ref level) = params.level {
        // Filter to this level and above.
        let levels = levels_at_or_above(level);
        if !levels.is_empty() {
            let placeholders: Vec<&str> = levels.iter().map(|_| "?").collect();
            sql.push_str(&format!(" AND level IN ({})", placeholders.join(",")));
            for l in levels {
                binds.push(BindValue::Text(l));
            }
        }
    }

    if let Some(ref cat) = params.category {
        sql.push_str(" AND category = ?");
        binds.push(BindValue::Text(cat.clone()));
    }

    if let Some(ref search) = params.search {
        sql.push_str(" AND (message LIKE ? OR detail LIKE ?)");
        let pattern = format!("%{}%", search);
        binds.push(BindValue::Text(pattern.clone()));
        binds.push(BindValue::Text(pattern));
    }

    if let Some(before) = params.before_id {
        sql.push_str(" AND id < ?");
        binds.push(BindValue::Int(before));
    }

    sql.push_str(" ORDER BY id DESC LIMIT ?");
    binds.push(BindValue::Int(params.limit));

    // sqlx doesn't support dynamic bind lists easily, so we use query_as with raw SQL.
    // Build the query manually.
    let rows: Vec<(i64, String, String, String, String, String)> =
        build_dynamic_query(&sql, &binds, db).await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, timestamp, level, category, message, detail)| LogEntry {
                id,
                timestamp,
                level,
                category,
                message,
                detail,
            },
        )
        .collect())
}

/// Delete logs older than `days` days. Returns number of deleted rows.
pub async fn cleanup(db: &SqlitePool, days: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM logs WHERE timestamp < datetime('now', ?)")
        .bind(format!("-{} days", days))
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// Get total log count (for the UI).
pub async fn count(db: &SqlitePool) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM logs")
        .fetch_one(db)
        .await?;
    Ok(row.0)
}

/// Get the most recent log ID (for polling).
#[allow(dead_code)]
pub async fn latest_id(db: &SqlitePool) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT MAX(id) FROM logs")
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.0).unwrap_or(0))
}

/// Get new entries after a given ID (for live polling).
///
/// `level` applies "at or above" semantics (matching the logs page
/// dropdown — "warn" means warn + error). `category`, if supplied and
/// non-empty, is an exact match. Both are pushed into the SQL query
/// so SQLite does the filtering and the round trip carries only
/// matching rows. Previously the poll handler fetched 100 unfiltered
/// rows every 3s and dropped the misses in-memory — cheap per row
/// but wasteful when a user had (say) `level=error, category=grab`
/// set and nothing was going wrong at the moment.
pub async fn entries_after(
    db: &SqlitePool,
    after_id: i64,
    limit: i64,
    level: Option<&str>,
    category: Option<&str>,
) -> Result<Vec<LogEntry>, sqlx::Error> {
    let mut sql = String::from(
        "SELECT id, timestamp, level, category, message, detail FROM logs WHERE id > ?",
    );
    let mut binds: Vec<BindValue> = vec![BindValue::Int(after_id)];

    if let Some(l) = level {
        let levels = levels_at_or_above(l);
        if !levels.is_empty() {
            let placeholders: Vec<&str> = levels.iter().map(|_| "?").collect();
            sql.push_str(&format!(" AND level IN ({})", placeholders.join(",")));
            for lv in levels {
                binds.push(BindValue::Text(lv));
            }
        }
    }

    if let Some(cat) = category.filter(|c| !c.is_empty()) {
        sql.push_str(" AND category = ?");
        binds.push(BindValue::Text(cat.to_string()));
    }

    sql.push_str(" ORDER BY id DESC LIMIT ?");
    binds.push(BindValue::Int(limit));

    let rows = build_dynamic_query(&sql, &binds, db).await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, timestamp, level, category, message, detail)| LogEntry {
                id,
                timestamp,
                level,
                category,
                message,
                detail,
            },
        )
        .collect())
}

fn levels_at_or_above(level: &str) -> Vec<String> {
    let all = ["trace", "debug", "info", "warn", "error"];
    let idx = all
        .iter()
        .position(|l| l.eq_ignore_ascii_case(level))
        .unwrap_or(0);
    all[idx..].iter().map(|s| s.to_string()).collect()
}

/// Execute a dynamically-bound query. We chain `.bind()` calls in a
/// loop, dispatching on each bind's native type so that `i64` values
/// stay `INTEGER`-typed at the sqlite protocol level instead of being
/// stringified.
async fn build_dynamic_query(
    sql: &str,
    binds: &[BindValue],
    db: &SqlitePool,
) -> Result<Vec<(i64, String, String, String, String, String)>, sqlx::Error> {
    let mut q = sqlx::query_as::<_, (i64, String, String, String, String, String)>(sql);
    for b in binds {
        q = match b {
            BindValue::Int(i) => q.bind(*i),
            BindValue::Text(s) => q.bind(s.clone()),
        };
    }
    q.fetch_all(db).await
}
