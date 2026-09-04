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
    /// Kitsu metadata provider — third in the AL → Jikan → Kitsu
    /// fallback chain. Used by `metadata_sync` to attribute per-series
    /// fallback rows to whichever provider actually carried the load,
    /// instead of stuffing every fallback success into AniList. Both
    /// the by-mal-id lookup and the title-fuzz path emit under this
    /// category.
    Kitsu,
    /// Download-client family — qBittorrent, Deluge, Transmission,
    /// rTorrent, SAB. Was previously `QBit` (and persisted as the
    /// `qbit` category string) when qBittorrent was the only client;
    /// after the multi-client refactor it became a misleading name.
    /// Renamed to `DownloadClient` and persisted as `download_client`;
    /// the migration in `migrations::migrate` rewrites existing
    /// `category = 'qbit'` log rows to `category = 'download_client'`.
    DownloadClient,
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
    /// External-account integration (issue #62). OAuth link / unlink
    /// lifecycle, token refresh, per-tick sync telemetry, mapping
    /// failures on MAL → AL lookups. Users read this category on the
    /// System → Logs page to diagnose "my AL sync stopped working"
    /// or "a MAL entry didn't import."
    ExternalSync,
    /// multi-rss commit H — RSS sync telemetry across Nyaa,
    /// indexer-RSS, and direct feeds. Replaces the old
    /// `LogCategory::System` rows that legacy RSS code wrote, so
    /// users diagnosing "is my SubsPlease feed actually polling"
    /// filter on `Rss` rather than wading through general System
    /// chatter. Plan decisions #10 and #11.
    Rss,
    /// Outbound notification dispatch (issue #118). Per-provider send
    /// outcomes — successful sends emit at info, send-side failures
    /// (timeout, transport error, receiver-returned-error) emit at warn
    /// with the provider name + event kind. Distinct from `System`
    /// because users diagnosing "did my Discord webhook fire" want to
    /// filter to just the dispatch chatter rather than wade through
    /// startup / migration / supervisor noise.
    Notifications,
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
            LogCategory::Kitsu => "kitsu",
            LogCategory::DownloadClient => "download_client",
            LogCategory::Jellyfin => "jellyfin",
            LogCategory::Media => "media",
            LogCategory::Library => "library",
            LogCategory::Auth => "auth",
            LogCategory::System => "system",
            LogCategory::PostProcess => "post_process",
            LogCategory::Quality => "quality",
            LogCategory::Scoring => "scoring",
            LogCategory::ExternalSync => "external_sync",
            LogCategory::Rss => "rss",
            LogCategory::Notifications => "notifications",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "search" => Some(LogCategory::Search),
            "grab" => Some(LogCategory::Grab),
            "auto_search" => Some(LogCategory::AutoSearch),
            "nyaa" => Some(LogCategory::Nyaa),
            "anilist" => Some(LogCategory::AniList),
            "jikan" => Some(LogCategory::Jikan),
            "kitsu" => Some(LogCategory::Kitsu),
            // Accept the legacy "qbit" string so any pre-rename URL
            // params (bookmarks, deep links) still resolve to the
            // renamed variant. New rows persist as "download_client".
            "qbit" | "download_client" => Some(LogCategory::DownloadClient),
            "jellyfin" => Some(LogCategory::Jellyfin),
            "media" => Some(LogCategory::Media),
            "library" => Some(LogCategory::Library),
            "auth" => Some(LogCategory::Auth),
            "system" => Some(LogCategory::System),
            "post_process" => Some(LogCategory::PostProcess),
            "quality" => Some(LogCategory::Quality),
            "scoring" => Some(LogCategory::Scoring),
            "external_sync" => Some(LogCategory::ExternalSync),
            "rss" => Some(LogCategory::Rss),
            "notifications" => Some(LogCategory::Notifications),
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
            LogCategory::Kitsu => "Kitsu",
            LogCategory::DownloadClient => "Download Client",
            LogCategory::Jellyfin => "Jellyfin",
            LogCategory::Media => "Media",
            LogCategory::Library => "Library",
            LogCategory::Auth => "Auth",
            LogCategory::System => "System",
            LogCategory::PostProcess => "Post-Process",
            LogCategory::Quality => "Quality",
            LogCategory::Scoring => "Scoring",
            LogCategory::ExternalSync => "External Sync",
            LogCategory::Rss => "RSS",
            LogCategory::Notifications => "Notifications",
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
    /// The category's display name (`Download Client` for
    /// `download_client`); the slug itself for anything unknown.
    pub category_label: String,
    pub message: String,
    pub detail: String,
}

/// Display name for a stored category slug, falling back to the slug.
pub fn category_display_label(slug: &str) -> String {
    LogCategory::from_str(slug)
        .map(|c| c.label().to_string())
        .unwrap_or_else(|| slug.to_string())
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
                category_label: category_display_label(&category),
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

/// Most recent log ID — only consumed by `handlers::system::endpoint_tests`
/// as a cursor before seeding more rows; `#[allow(dead_code)]` because
/// non-test builds don't compile the test module.
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
                category_label: category_display_label(&category),
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
    let mut q = sqlx::query_as::<_, (i64, String, String, String, String, String)>(
        sqlx::AssertSqlSafe(sql),
    );
    for b in binds {
        q = match b {
            BindValue::Int(i) => q.bind(*i),
            BindValue::Text(s) => q.bind(s.clone()),
        };
    }
    q.fetch_all(db).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    // ── LogLevel taxonomy ────────────────────────────────────────────

    #[test]
    fn log_level_round_trips_through_as_str_and_from_str() {
        // The taxonomy is the canonical wire format on the `logs.level`
        // column, so as_str ↔ from_str must be a true round-trip for
        // every variant — a renamed variant that only updated one side
        // would silently turn writes into the from_str default of Info.
        for lvl in [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ] {
            assert_eq!(LogLevel::from_str(lvl.as_str()), lvl);
        }
    }

    #[test]
    fn log_level_from_str_is_case_insensitive() {
        assert_eq!(LogLevel::from_str("WARN"), LogLevel::Warn);
        assert_eq!(LogLevel::from_str("Error"), LogLevel::Error);
        assert_eq!(LogLevel::from_str("DeBuG"), LogLevel::Debug);
    }

    #[test]
    fn log_level_from_str_unknown_value_coerces_to_info() {
        // Per CLAUDE.md: `RYOKAN_DB_LOG_LEVEL` defaults to Info on an
        // unknown value, and from_str is the read-side helper. Any
        // future variant addition that updates `as_str` but forgets
        // `from_str` would silently regress to this default — pinned
        // here so the diff is loud.
        assert_eq!(LogLevel::from_str(""), LogLevel::Info);
        assert_eq!(LogLevel::from_str("verbose"), LogLevel::Info);
        assert_eq!(LogLevel::from_str("info"), LogLevel::Info);
    }

    #[test]
    fn log_level_severity_is_strictly_increasing() {
        // The write-side floor compares severities, so a non-monotonic
        // ordering would let `Trace` writes through past a `Warn` floor.
        let chain = [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ];
        for w in chain.windows(2) {
            assert!(
                w[0].severity() < w[1].severity(),
                "{:?} severity must precede {:?}",
                w[0],
                w[1]
            );
        }
    }

    // ── LogCategory taxonomy ─────────────────────────────────────────

    fn all_categories() -> Vec<LogCategory> {
        vec![
            LogCategory::Search,
            LogCategory::Grab,
            LogCategory::AutoSearch,
            LogCategory::Nyaa,
            LogCategory::AniList,
            LogCategory::Jikan,
            LogCategory::Kitsu,
            LogCategory::DownloadClient,
            LogCategory::Jellyfin,
            LogCategory::Media,
            LogCategory::Library,
            LogCategory::Auth,
            LogCategory::System,
            LogCategory::PostProcess,
            LogCategory::Quality,
            LogCategory::Scoring,
            LogCategory::ExternalSync,
            LogCategory::Rss,
            LogCategory::Notifications,
        ]
    }

    #[test]
    fn log_category_round_trips_through_as_str_and_from_str() {
        // Same invariant as LogLevel: every variant's wire string
        // must parse back to itself. Adding a new variant requires
        // updating both arms or this test fails — pinned per
        // CLAUDE.md "Adding a new variant requires updating all
        // three of the as_str / from_str / display-name match arms."
        for cat in all_categories() {
            assert_eq!(LogCategory::from_str(cat.as_str()), Some(cat));
        }
    }

    #[test]
    fn log_category_from_str_unknown_returns_none() {
        // Distinct from LogLevel — a bad category isn't coerced to a
        // default, it's rejected. The handler-side filter then no-ops
        // rather than silently filtering on the wrong column value.
        assert!(LogCategory::from_str("not_a_category").is_none());
        assert!(LogCategory::from_str("").is_none());
    }

    #[test]
    fn log_category_from_str_is_case_insensitive() {
        assert_eq!(LogCategory::from_str("ANILIST"), Some(LogCategory::AniList));
        assert_eq!(
            LogCategory::from_str("Auto_Search"),
            Some(LogCategory::AutoSearch)
        );
    }

    #[test]
    fn log_category_download_client_renames_from_qbit() {
        // The `QBit` variant was renamed to `DownloadClient` after the
        // multi-client refactor since it covers Deluge / Transmission
        // / rTorrent / SAB too. Persisted wire string flipped from
        // "qbit" to "download_client" with a one-shot migration in
        // `migrations::migrate` rewriting existing rows; the legacy
        // string still parses for backward-compat URL params.
        assert_eq!(LogCategory::DownloadClient.label(), "Download Client");
        assert_eq!(LogCategory::DownloadClient.as_str(), "download_client");
        assert_eq!(
            LogCategory::from_str("qbit"),
            Some(LogCategory::DownloadClient),
            "legacy 'qbit' wire string must still parse to DownloadClient"
        );
    }

    #[test]
    fn log_category_labels_are_human_readable_for_each_variant() {
        // Every variant must have a non-empty UI label — a missing
        // arm in `label()` would cause a Rust compile-time
        // exhaustiveness error on the match, but a regression where
        // someone renames a label to "" wouldn't. Pin every label.
        for cat in all_categories() {
            let label = cat.label();
            assert!(!label.is_empty(), "{:?} label must be non-empty", cat);
        }
    }

    // ── insert / count / latest_id ───────────────────────────────────

    #[tokio::test]
    async fn insert_then_count_round_trips_through_logs_table() {
        let db = in_memory_pool().await;
        assert_eq!(count(&db).await.unwrap(), 0);

        insert(&db, LogLevel::Info, LogCategory::System, "boot", "ok")
            .await
            .unwrap();
        insert(
            &db,
            LogLevel::Warn,
            LogCategory::AutoSearch,
            "throttled",
            "AL 429",
        )
        .await
        .unwrap();

        assert_eq!(count(&db).await.unwrap(), 2);
        assert!(latest_id(&db).await.unwrap() > 0);
    }

    #[tokio::test]
    async fn latest_id_returns_zero_on_empty_table() {
        let db = in_memory_pool().await;
        assert_eq!(latest_id(&db).await.unwrap(), 0);
    }

    // ── query / entries_after with filters ───────────────────────────

    async fn seed_three_levels(db: &SqlitePool) {
        insert(db, LogLevel::Debug, LogCategory::Search, "d", "")
            .await
            .unwrap();
        insert(db, LogLevel::Info, LogCategory::Grab, "i", "")
            .await
            .unwrap();
        insert(db, LogLevel::Error, LogCategory::Nyaa, "e", "")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn query_returns_newest_first_with_no_filters() {
        let db = in_memory_pool().await;
        seed_three_levels(&db).await;
        let rows = query(&db, &LogQuery::default()).await.unwrap();
        // ORDER BY id DESC — last insert (Error) wins.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].level, "error");
        assert_eq!(rows[2].level, "debug");
    }

    #[tokio::test]
    async fn query_level_filter_uses_at_or_above_semantics() {
        // level=warn ⇒ warn + error pass through, info/debug/trace are dropped.
        let db = in_memory_pool().await;
        seed_three_levels(&db).await;
        let q = LogQuery {
            level: Some("warn".into()),
            ..Default::default()
        };
        let rows = query(&db, &q).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, "error");
    }

    #[tokio::test]
    async fn query_category_filter_is_exact_match() {
        let db = in_memory_pool().await;
        seed_three_levels(&db).await;
        let q = LogQuery {
            category: Some("grab".into()),
            ..Default::default()
        };
        let rows = query(&db, &q).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].category, "grab");
    }

    #[tokio::test]
    async fn query_search_filter_matches_message_or_detail() {
        let db = in_memory_pool().await;
        insert(&db, LogLevel::Info, LogCategory::Grab, "fizz", "qux")
            .await
            .unwrap();
        insert(&db, LogLevel::Info, LogCategory::Grab, "wibble", "fizz-bar")
            .await
            .unwrap();
        insert(&db, LogLevel::Info, LogCategory::Grab, "no", "match")
            .await
            .unwrap();

        let q = LogQuery {
            search: Some("fizz".into()),
            ..Default::default()
        };
        let rows = query(&db, &q).await.unwrap();
        assert_eq!(rows.len(), 2, "both rows containing 'fizz' must match");
    }

    #[tokio::test]
    async fn query_before_id_paginates_descending() {
        let db = in_memory_pool().await;
        for i in 0..5 {
            insert(
                &db,
                LogLevel::Info,
                LogCategory::Search,
                &format!("m{i}"),
                "",
            )
            .await
            .unwrap();
        }
        let head = query(
            &db,
            &LogQuery {
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(head.len(), 2);
        let cursor = head.last().unwrap().id;
        let next = query(
            &db,
            &LogQuery {
                limit: 2,
                before_id: Some(cursor),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // All `next` ids must be strictly less than the cursor.
        assert!(next.iter().all(|r| r.id < cursor));
    }

    #[tokio::test]
    async fn entries_after_id_returns_only_newer_rows() {
        let db = in_memory_pool().await;
        insert(&db, LogLevel::Info, LogCategory::Search, "first", "")
            .await
            .unwrap();
        let after = latest_id(&db).await.unwrap();
        insert(&db, LogLevel::Info, LogCategory::Search, "second", "")
            .await
            .unwrap();
        insert(&db, LogLevel::Info, LogCategory::Search, "third", "")
            .await
            .unwrap();

        let rows = entries_after(&db, after, 100, None, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.id > after));
    }

    #[tokio::test]
    async fn entries_after_respects_level_and_category_filters() {
        let db = in_memory_pool().await;
        insert(&db, LogLevel::Info, LogCategory::Grab, "i-grab", "")
            .await
            .unwrap();
        insert(&db, LogLevel::Warn, LogCategory::Grab, "w-grab", "")
            .await
            .unwrap();
        insert(&db, LogLevel::Warn, LogCategory::Search, "w-search", "")
            .await
            .unwrap();

        // level=warn + category=grab ⇒ only the second row.
        let rows = entries_after(&db, 0, 100, Some("warn"), Some("grab"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message, "w-grab");
    }

    #[tokio::test]
    async fn entries_after_treats_empty_category_as_unfiltered() {
        // The `filter(|c| !c.is_empty())` arm makes an empty-string
        // category equivalent to None — the handler passes "" when
        // the user clears the dropdown rather than re-stringifying
        // `Option<String>` into None at every layer.
        let db = in_memory_pool().await;
        insert(&db, LogLevel::Info, LogCategory::Search, "s", "")
            .await
            .unwrap();
        insert(&db, LogLevel::Info, LogCategory::Grab, "g", "")
            .await
            .unwrap();
        let rows = entries_after(&db, 0, 100, None, Some("")).await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    // ── cleanup ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn cleanup_removes_only_aged_rows() {
        let db = in_memory_pool().await;
        insert(&db, LogLevel::Info, LogCategory::Search, "fresh", "")
            .await
            .unwrap();
        insert(&db, LogLevel::Info, LogCategory::Search, "old", "")
            .await
            .unwrap();
        // Roll the second row's timestamp 60 days into the past.
        sqlx::query(
            "UPDATE logs SET timestamp = datetime('now', '-60 days') WHERE message = 'old'",
        )
        .execute(&db)
        .await
        .unwrap();

        let removed = cleanup(&db, 30).await.unwrap();
        assert_eq!(removed, 1);

        let remaining = query(&db, &LogQuery::default()).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].message, "fresh");
    }

    // ── levels_at_or_above ──────────────────────────────────────────

    #[test]
    fn levels_at_or_above_includes_self_and_higher() {
        assert_eq!(levels_at_or_above("warn"), vec!["warn", "error"]);
        assert_eq!(levels_at_or_above("ERROR"), vec!["error"]);
        assert_eq!(
            levels_at_or_above("trace"),
            vec!["trace", "debug", "info", "warn", "error"]
        );
    }

    #[test]
    fn levels_at_or_above_unknown_falls_back_to_all_levels() {
        // `position()` returning None ⇒ idx = 0 ⇒ all five levels.
        // Pinned so a future change to "drop everything on unknown
        // input" doesn't slip through silently and silently drop
        // every log row past the filter.
        assert_eq!(
            levels_at_or_above("garbage"),
            vec!["trace", "debug", "info", "warn", "error"]
        );
    }
}
