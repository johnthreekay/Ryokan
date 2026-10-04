//! `--sanitize-db-for-debug` CLI helper (issue #62).
//!
//! Produces a sanitized copy of the SQLite database with every token
//! and password column blanked out so a user can safely paste their
//! DB into a bug report. The live DB is never mutated — the copy is
//! a sibling file; the caller is responsible for deleting it when
//! done.
//!
//! Scope extends beyond the #62-specific `external_accounts` blobs
//! to every known secret column already in the DB (qBit / Deluge /
//! Transmission / rtorrent passwords, Jellyfin API key, Sonarr +
//! Radarr shim API keys). A sanitized DB has to be unconditionally
//! safe to share; leaving a legacy password plaintext would defeat
//! the feature for users who joined before #62.

use std::path::Path;

use sqlx::SqlitePool;

use crate::services::crypto::SANITIZED_SENTINEL;

/// Read `live_db` and produce a sanitized copy at `output`. Returns
/// the number of rows touched across the scrubbed tables so callers
/// can surface "N OAuth tokens + M config passwords blanked" output.
///
/// Implementation: shell-out-free — copies the file, opens a
/// read-write SqlitePool against the copy, runs UPDATE statements,
/// closes. Runs inside `tokio::task::spawn_blocking` at the caller
/// if sync context is inappropriate; this function is async only
/// because sqlx's query API is.
pub async fn run_sanitize(live_db: &Path, output: &Path) -> Result<SanitizeSummary, String> {
    if !live_db.exists() {
        return Err(format!(
            "DB not found at {} — nothing to sanitize",
            live_db.display()
        ));
    }

    // Detect a live SQLite WAL alongside the DB. Ryokan uses
    // journal_mode=WAL, so a running server has uncommitted writes
    // sitting in `<db>-wal` that aren't visible to a plain
    // `fs::copy`. A sanitized copy taken mid-run could miss the
    // most recent OAuth link, end up with a stale schema_version,
    // or worst-case land on a torn page boundary. Refuse with a
    // clear "stop the server first" rather than silently producing
    // a half-stale dump. The shutdown checkpoint flushes WAL into
    // the main DB file, so a stopped server has no `-wal` adjacent
    // (or one of zero size) and this check passes.
    //
    // Path derivation: SQLite's WAL filename is the database
    // filename with a literal `-wal` suffix appended (per the
    // sqlite docs). `Path::with_extension` would mishandle the
    // extensionless case (`data/ryokan` → `data/ryokan.db-wal`
    // instead of the correct `data/ryokan-wal`). Append on the
    // raw `OsString` so the rule matches exactly regardless of
    // extension shape.
    let mut wal_os = live_db.as_os_str().to_owned();
    wal_os.push("-wal");
    let wal_path = std::path::PathBuf::from(wal_os);
    if let Ok(meta) = std::fs::metadata(&wal_path)
        && meta.len() > 0
    {
        return Err(format!(
            "Active SQLite WAL detected at {} — stop the Ryokan server before running sanitize \
             so the WAL is checkpointed into the main DB file. A copy taken mid-run would miss \
             uncommitted writes.",
            wal_path.display()
        ));
    }

    // Delete any prior sanitized copy first so a repeat run doesn't
    // silently update a stale file under a SQLite write lock from a
    // prior aborted run.
    if output.exists() {
        std::fs::remove_file(output)
            .map_err(|e| format!("could not remove stale {}: {}", output.display(), e))?;
    }
    std::fs::copy(live_db, output).map_err(|e| {
        format!(
            "could not copy {} → {}: {}",
            live_db.display(),
            output.display(),
            e
        )
    })?;

    let url = format!("sqlite://{}?mode=rwc", output.display());
    let pool = SqlitePool::connect(&url)
        .await
        .map_err(|e| format!("open sanitized copy: {e}"))?;

    // Run migrations on the copy. Pre-#62 DBs don't have the
    // `external_accounts` table yet, but the CLI still needs to
    // produce a valid sanitized output against them (otherwise
    // users on an older install can't generate a safe debug dump).
    // Migrations are idempotent; running them against the copy is
    // a no-op when the schema is already current.
    crate::models::migrate(&pool)
        .await
        .map_err(|e| format!("migrate sanitized copy: {e}"))?;

    let sentinel: &[u8] = SANITIZED_SENTINEL;
    // `external_accounts` tokens — the primary #62 concern.
    let ext_rows = sqlx::query(
        "UPDATE external_accounts
            SET access_token_encrypted = ?,
                refresh_token_encrypted = ?",
    )
    .bind(sentinel)
    .bind(sentinel)
    .execute(&pool)
    .await
    .map_err(|e| format!("scrub external_accounts: {e}"))?
    .rows_affected();

    // Config-row secrets — plaintext columns that predate #62. All
    // null-safe UPDATEs: coalesce through NULL → empty so an unused
    // column stays empty rather than reading "[REDACTED]" in a DB
    // where it was never set.
    let cfg_rows = sqlx::query(
        "UPDATE config
            SET qbit_pass = CASE WHEN qbit_pass = '' THEN '' ELSE '[REDACTED]' END,
                deluge_password = CASE WHEN deluge_password = '' THEN '' ELSE '[REDACTED]' END,
                transmission_password = CASE WHEN transmission_password = '' THEN '' ELSE '[REDACTED]' END,
                rtorrent_password = CASE WHEN rtorrent_password = '' THEN '' ELSE '[REDACTED]' END,
                jellyfin_api_key = CASE WHEN jellyfin_api_key = '' THEN '' ELSE '[REDACTED]' END,
                sonarr_api_key = CASE WHEN sonarr_api_key = '' THEN '' ELSE '[REDACTED]' END,
                radarr_api_key = CASE WHEN radarr_api_key = '' THEN '' ELSE '[REDACTED]' END,
                autobrr_api_key = CASE WHEN autobrr_api_key = '' THEN '' ELSE '[REDACTED]' END,
                tmdb_api_key = CASE WHEN tmdb_api_key = '' THEN '' ELSE '[REDACTED]' END",
    )
    .execute(&pool)
    .await
    .map_err(|e| format!("scrub config secrets: {e}"))?
    .rows_affected();

    // Per-row secrets that live outside `config`: torznab/newznab API
    // keys, download-client passwords, and the scoped API keys (#114).
    // `api_keys.key` is UNIQUE, so the rowid keeps the placeholders
    // distinct.
    let indexer_rows = sqlx::query(
        "UPDATE indexers SET api_key = CASE WHEN api_key = '' THEN '' ELSE '[REDACTED]' END",
    )
    .execute(&pool)
    .await
    .map_err(|e| format!("scrub indexers: {e}"))?
    .rows_affected();
    let client_rows = sqlx::query(
        "UPDATE download_clients \
            SET password = CASE WHEN password = '' THEN '' ELSE '[REDACTED]' END",
    )
    .execute(&pool)
    .await
    .map_err(|e| format!("scrub download_clients: {e}"))?
    .rows_affected();
    let api_key_rows = sqlx::query("UPDATE api_keys SET key = '[REDACTED-key-' || rowid || ']'")
        .execute(&pool)
        .await
        .map_err(|e| format!("scrub api_keys: {e}"))?
        .rows_affected();

    // URLs that can carry credentials: a grab's recorded download link
    // (`?apikey=` on every torznab/newznab release, kept so Restore can
    // re-add it), a direct feed's address, and every link the RSS sync
    // has seen. The passkey can sit in the path too (AnimeBytes-style
    // `/feed/<passkey>`), so only scheme and host survive (`redact_url`).
    let url_rows = redact_url_column(&pool, "grabbed_torrents", "source_url").await?
        + redact_url_column(&pool, "direct_rss_feeds", "url").await?
        + redact_url_column(&pool, "rss_seen", "link").await?;

    // Notification providers: the Discord webhook URL carries its token,
    // a generic webhook its URL, HMAC secret and custom headers
    // (`Authorization: Bearer ...`). Restored, they simply can't send.
    let notification_rows = sqlx::query(
        "UPDATE notification_providers SET config_json = json_replace( \
             config_json, '$.url', '[REDACTED]', '$.webhook_url', '[REDACTED]', \
             '$.secret', '[REDACTED]', '$.headers', json('[]')) \
          WHERE json_valid(config_json)",
    )
    .execute(&pool)
    .await
    .map_err(|e| format!("scrub notification_providers: {e}"))?
    .rows_affected();

    // The last poll errors quote the request that failed.
    for sql in [
        "UPDATE indexers SET rss_last_poll_error = ''",
        "UPDATE direct_rss_feeds SET last_poll_error = ''",
    ] {
        sqlx::query(sql)
            .execute(&pool)
            .await
            .map_err(|e| format!("clear poll errors: {e}"))?;
    }

    // Log text: any URL or `apikey=` value in a message or detail.
    let log_rows = scrub_logs(&pool).await?;

    // `sessions.token` — cookie values double as DB session keys.
    // A sanitized DB handed to someone else shouldn't let them log
    // in as the user. `token` is the PRIMARY KEY, so using a
    // constant literal would UNIQUE-fail across multiple sessions;
    // append the rowid for distinctness.
    let session_rows =
        sqlx::query("UPDATE sessions SET token = '[REDACTED-session-' || rowid || ']'")
            .execute(&pool)
            .await
            .map_err(|e| format!("scrub sessions: {e}"))?
            .rows_affected();

    // `users.password_hash` — bcrypt is computationally hard to
    // reverse but still a non-zero information leak. A determined
    // attacker with leaked bcrypt hashes can run offline dictionary
    // attacks at ~10 guesses/sec per cost-10 hash.
    let user_rows = sqlx::query("UPDATE users SET password_hash = '[REDACTED]'")
        .execute(&pool)
        .await
        .map_err(|e| format!("scrub users: {e}"))?
        .rows_affected();

    // UPDATE leaves the old values in free pages (the bundled SQLite is
    // built without SECURE_DELETE), where anyone reading the file can
    // still find them. VACUUM rewrites the file without the free pages.
    sqlx::query("VACUUM")
        .execute(&pool)
        .await
        .map_err(|e| format!("vacuum sanitized copy: {e}"))?;
    pool.close().await;

    Ok(SanitizeSummary {
        external_accounts_tokens: ext_rows as usize,
        config_passwords: cfg_rows as usize,
        session_tokens: session_rows as usize,
        user_password_hashes: user_rows as usize,
        indexer_keys: indexer_rows as usize,
        client_passwords: client_rows as usize,
        api_keys: api_key_rows as usize,
        credential_urls: url_rows as usize,
        notification_configs: notification_rows as usize,
        log_rows: log_rows as usize,
        output_path: output.to_path_buf(),
    })
}

#[derive(Debug)]
pub struct SanitizeSummary {
    pub external_accounts_tokens: usize,
    pub config_passwords: usize,
    pub session_tokens: usize,
    pub user_password_hashes: usize,
    pub indexer_keys: usize,
    pub client_passwords: usize,
    pub api_keys: usize,
    pub credential_urls: usize,
    pub notification_configs: usize,
    pub log_rows: usize,
    pub output_path: std::path::PathBuf,
}

impl std::fmt::Display for SanitizeSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Sanitized DB written to: {}", self.output_path.display())?;
        writeln!(
            f,
            "  external_accounts rows blanked:  {}",
            self.external_accounts_tokens
        )?;
        writeln!(
            f,
            "  config rows with secrets scrubbed: {}",
            self.config_passwords
        )?;
        writeln!(
            f,
            "  session tokens redacted:         {}",
            self.session_tokens
        )?;
        writeln!(
            f,
            "  user password hashes redacted:   {}",
            self.user_password_hashes
        )?;
        writeln!(
            f,
            "  indexer API keys redacted:       {}",
            self.indexer_keys
        )?;
        writeln!(
            f,
            "  download client passwords:       {}",
            self.client_passwords
        )?;
        writeln!(f, "  API keys redacted:               {}", self.api_keys)?;
        writeln!(
            f,
            "  URLs with credentials scrubbed:  {}",
            self.credential_urls
        )?;
        writeln!(
            f,
            "  notification configs scrubbed:   {}",
            self.notification_configs
        )?;
        writeln!(f, "  log rows redacted:               {}", self.log_rows)?;
        write!(f, "Safe to share in bug reports.")
    }
}

/// `raw` with whatever could carry a credential removed, for log lines,
/// error messages and notifications. An http(s) URL keeps its scheme
/// and host (the path can hold a passkey, AnimeBytes-style, and the
/// query an `apikey=`); a magnet keeps its info-hash (a `tr=` tracker URL
/// can hold a passkey); anything else becomes `[redacted]`.
pub fn redact_url(raw: &str) -> String {
    let raw = raw.trim();
    if let Ok(url) = reqwest::Url::parse(raw) {
        match url.scheme() {
            "http" | "https" => {
                let host = url.host_str().unwrap_or("");
                return match url.port() {
                    Some(port) => format!("{}://{host}:{port}/[redacted]", url.scheme()),
                    None => format!("{}://{host}/[redacted]", url.scheme()),
                };
            }
            "magnet" => {
                let hash = crate::services::nyaa::extract_hash(raw);
                if !hash.is_empty() {
                    return format!("magnet:?xt=urn:btih:{hash}");
                }
            }
            _ => {}
        }
    }
    "[redacted]".to_string()
}

/// Run [`redact_url`] over `table.column`, skipping rows already blank.
/// A missing table (an old database) counts as nothing to do.
async fn redact_url_column(pool: &SqlitePool, table: &str, column: &str) -> Result<u64, String> {
    let (t, c) = (quote_ident(table), quote_ident(column));
    let rows: Vec<(i64, String)> = match sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT rowid, {c} FROM {t} WHERE {c} != ''"
    )))
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => return Ok(0),
    };
    let mut changed = 0;
    for (rowid, url) in rows {
        let redacted = redact_url(&url);
        if redacted != url {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {t} SET {c} = ? WHERE rowid = ?"
            )))
            .bind(&redacted)
            .bind(rowid)
            .execute(pool)
            .await
            .map_err(|e| format!("redact {table}.{column}: {e}"))?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// URLs and `apikey=` values in free text.
static RE_SECRETISH: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
    regex_lite::Regex::new(
        r#"(?i)\b(?:https?|magnet):[^\s"'<>()\[\]]+|\b(?:api_?key|passkey|token)=[^&\s"']+"#,
    )
    .expect("secretish regex compiles")
});

/// `text` with every URL passed through [`redact_url`] and every bare
/// `apikey=` / `passkey=` / `token=` value dropped.
pub fn redact_text(text: &str) -> String {
    RE_SECRETISH
        .replace_all(text, |caps: &regex_lite::Captures<'_>| {
            let hit = &caps[0];
            if hit.contains("://") || hit.to_ascii_lowercase().starts_with("magnet:") {
                redact_url(hit)
            } else {
                let key = hit.split('=').next().unwrap_or("key");
                format!("{key}=[redacted]")
            }
        })
        .into_owned()
}

/// Redact URLs and keys in every log row's message and detail.
async fn scrub_logs(pool: &SqlitePool) -> Result<u64, String> {
    let rows: Vec<(i64, String, String)> =
        match sqlx::query_as("SELECT id, message, detail FROM logs")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => rows,
            Err(_) => return Ok(0),
        };
    let mut changed = 0;
    for (id, message, detail) in rows {
        let (m, d) = (redact_text(&message), redact_text(&detail));
        if m != message || d != detail {
            sqlx::query("UPDATE logs SET message = ?, detail = ? WHERE id = ?")
                .bind(&m)
                .bind(&d)
                .bind(id)
                .execute(pool)
                .await
                .map_err(|e| format!("scrub logs: {e}"))?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// True for a value [`run_sanitize`] wrote in place of a secret
/// (`[REDACTED]`, `[REDACTED-key-N]`, ...). Key checks treat it as "no
/// key": restoring a sanitized backup used to leave `[REDACTED]` working
/// as the Sonarr / Radarr / autobrr key.
pub fn is_placeholder(value: &str) -> bool {
    value.trim_start().starts_with("[REDACTED")
}

/// Take a sanitized backup's placeholders out of a database that is
/// being restored, so none of them can act as a credential:
/// - scoped API keys are deleted (`key` is UNIQUE, and new keys are
///   needed anyway);
/// - users are deleted, since their hash is gone and nobody could log
///   in; with no user the first page load is `/setup`;
/// - linked accounts holding the sanitize sentinel are deleted, since
///   they can't be decrypted, shown, or unlinked;
/// - sessions are deleted (restore does that for every archive);
/// - every other text column still holding a placeholder becomes `""`.
///
/// Table and column names come from the archive, so they are quoted as
/// identifiers, never spliced raw.
pub(crate) async fn clear_placeholders(pool: &SqlitePool) -> Result<(), String> {
    // Sessions go first. `sessions.user_id` references `users` with no
    // ON DELETE action, and backups sanitized before every snapshot
    // dropped its sessions still hold `[REDACTED-session-N]` rows: the
    // users delete failed on the foreign key, the error was ignored,
    // and the loop below emptied the hash, leaving a login page that no
    // password opens. Each table may be missing from an old backup, but
    // any other failure stops the restore.
    delete_unless_table_missing(pool, "DELETE FROM sessions").await?;
    delete_unless_table_missing(pool, "DELETE FROM api_keys WHERE key LIKE '[REDACTED%'").await?;
    delete_unless_table_missing(
        pool,
        "DELETE FROM users WHERE password_hash LIKE '[REDACTED%'",
    )
    .await?;
    if let Err(e) = sqlx::query("DELETE FROM external_accounts WHERE access_token_encrypted = ?")
        .bind(SANITIZED_SENTINEL)
        .execute(pool)
        .await
        && !is_missing_table(&e)
    {
        return Err(format!("delete sanitized linked accounts: {e}"));
    }

    // `lower(type)`: SQLite reads the stored type case-insensitively.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE lower(type) = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("list tables: {e}"))?;
    for table in tables {
        let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
            .bind(&table)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("list columns of {table}: {e}"))?;
        for column in columns {
            let (t, c) = (quote_ident(&table), quote_ident(&column));
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {t} SET {c} = '' WHERE typeof({c}) = 'text' AND {c} LIKE '[REDACTED%'"
            )))
            .execute(pool)
            .await
            .map_err(|e| format!("clear placeholders in {table}.{column}: {e}"))?;
        }
    }
    Ok(())
}

/// Run a `DELETE`, treating a table the database doesn't have as
/// nothing to delete. Any other error (a foreign key that blocks the
/// delete, a damaged table) is returned: a purge that silently did
/// nothing is how a restored database kept a login it shouldn't have.
pub(crate) async fn delete_unless_table_missing(
    pool: &SqlitePool,
    sql: &'static str,
) -> Result<(), String> {
    match sqlx::query(sql).execute(pool).await {
        Err(e) if !is_missing_table(&e) => Err(format!("{sql}: {e}")),
        _ => Ok(()),
    }
}

pub(crate) fn is_missing_table(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.message().starts_with("no such table"))
}

/// `name` as a quoted SQL identifier.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn redact_url_keeps_only_what_carries_no_credential() {
        assert_eq!(
            redact_url("http://prowlarr:9696/1/download?apikey=secret&link=x"),
            "http://prowlarr:9696/[redacted]"
        );
        assert_eq!(
            redact_url("https://animebytes.tv/feed/rss_torrents_anime/0123passkey"),
            "https://animebytes.tv/[redacted]"
        );
        assert_eq!(
            redact_url(
                "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd&tr=https://t.example/0123passkey/announce"
            ),
            "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd"
        );
        assert_eq!(redact_url("/local/path.torrent"), "[redacted]");
    }

    #[tokio::test]
    async fn clear_placeholders_leaves_nothing_usable_as_a_credential() {
        let pool = crate::test_support::in_memory_pool().await;
        crate::test_support::seed_sonarr_enabled(&pool, "[REDACTED]").await;
        sqlx::query("UPDATE config SET autobrr_api_key = '[REDACTED]', jellyfin_api_key = 'kept-real-value'")
            .execute(&pool)
            .await
            .unwrap();
        for (i, key) in ["[REDACTED-key-1]", "[REDACTED-key-2]"].iter().enumerate() {
            sqlx::query("INSERT INTO api_keys (name, key, scopes) VALUES (?, ?, 'calendar')")
                .bind(format!("k{i}"))
                .bind(key)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO users (username, password_hash) VALUES ('admin', '[REDACTED]')")
            .execute(&pool)
            .await
            .unwrap();

        clear_placeholders(&pool).await.expect("clear");

        let (sonarr, autobrr, jellyfin): (String, String, String) = sqlx::query_as(
            "SELECT sonarr_api_key, autobrr_api_key, jellyfin_api_key FROM config WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((sonarr.as_str(), autobrr.as_str()), ("", ""));
        assert_eq!(jellyfin, "kept-real-value", "real values are left alone");
        let count = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count("SELECT COUNT(*) FROM api_keys").await, 0);
        assert_eq!(
            count("SELECT COUNT(*) FROM users").await,
            0,
            "first load is /setup"
        );
        assert!(is_placeholder("[REDACTED-key-3]") && !is_placeholder("real-key"));
    }

    #[tokio::test]
    async fn clear_placeholders_removes_users_that_still_have_redacted_sessions() {
        // Backups sanitized before every snapshot dropped its sessions
        // carry the admin's session as `[REDACTED-session-N]`. Its
        // foreign key blocked the users delete, and the user stayed with
        // an emptied hash: a login page no password opens.
        let pool = crate::test_support::in_memory_pool().await;
        sqlx::query("INSERT INTO users (username, password_hash) VALUES ('admin', '[REDACTED]')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sessions (token, user_id) VALUES ('[REDACTED-session-1]', 1), \
             ('[REDACTED-session-2]', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        clear_placeholders(&pool).await.expect("clear");

        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((users, sessions), (0, 0), "first load is /setup");
    }

    #[tokio::test]
    async fn clear_placeholders_reports_a_delete_that_fails() {
        // A table holding a foreign key into `sessions` blocks the purge.
        // The error used to be ignored, so the restore went ahead with
        // the session still valid.
        let pool = crate::test_support::in_memory_pool().await;
        sqlx::query("INSERT INTO users (username, password_hash) VALUES ('admin', 'h')")
            .execute(&pool)
            .await
            .unwrap();
        for sql in [
            "INSERT INTO sessions (token, user_id) VALUES ('planted', 1)",
            "CREATE TABLE anchor (token TEXT REFERENCES sessions(token))",
            "INSERT INTO anchor VALUES ('planted')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let err = clear_placeholders(&pool).await.unwrap_err();
        assert!(err.contains("FOREIGN KEY"), "{err}");
    }

    fn tmpdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ryokan-sanitize-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    async fn seed_live_db(path: &Path) {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        crate::models::migrate(&pool).await.unwrap();

        // Insert a config row with a password + API key populated.
        sqlx::query(
            "INSERT INTO config (id, qbit_pass, jellyfin_api_key)
             VALUES (1, 'topsecret-qbit', 'jf-key-abc') ON CONFLICT(id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed one user + session.
        sqlx::query("INSERT INTO users (username, password_hash) VALUES (?, ?)")
            .bind("admin")
            .bind("$2b$10$abcdefghijklmnopqrstuv")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (token, user_id) VALUES (?, 1)")
            .bind("cookie-token-xyz")
            .execute(&pool)
            .await
            .unwrap();

        // Secrets outside `config`: an indexer key, a client password,
        // a scoped API key, and two URLs with credentials in the query.
        sqlx::query(
            "INSERT INTO indexers (name, kind, url, api_key) \
             VALUES ('Prowlarr', 'torznab', 'http://prowlarr:9696/1/api', 'indexer-key-123')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO download_clients (name, kind, url, username, password) \
             VALUES ('qbit', 'qbittorrent', 'http://qbit:8080', 'admin', 'client-pass-456')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO api_keys (name, key) VALUES ('calendar', 'scoped-key-789')")
            .execute(&pool)
            .await
            .unwrap();
        let sid = crate::test_support::seed_series(&pool, 4242, "Sanitize Show").await;
        let gid = crate::test_support::seed_grabbed_torrent(
            &pool,
            sid,
            "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
            "[Group] Sanitize Show - 01",
            &[1],
        )
        .await;
        sqlx::query("UPDATE grabbed_torrents SET source_url = ? WHERE id = ?")
            .bind("http://prowlarr:9696/1/download?apikey=indexer-key-123&link=abc&file=x.torrent")
            .bind(gid)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO direct_rss_feeds (name, url, enabled) \
             VALUES ('Private', 'https://tracker.example/rss?passkey=feed-pass-000&cat=1', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed an external_accounts row via the real model helper so
        // the encrypted blobs match the live encrypt() output shape.
        crate::models::external_accounts::link(
            &pool,
            crate::models::external_accounts::LinkRequest {
                provider: crate::models::external_accounts::PROVIDER_MAL.to_string(),
                provider_user_id: "mal_user".to_string(),
                username: "mal_user".to_string(),
                access_token: "plaintext-access".to_string(),
                refresh_token: "plaintext-refresh".to_string(),
                access_token_expires_at: None,
                score_format: "POINT_10".to_string(),
            },
        )
        .await
        .unwrap();

        pool.close().await;
    }

    #[tokio::test]
    async fn sanitize_leaves_live_db_untouched() {
        // The live DB file must not be mutated — users running the
        // CLI on a production install still have tokens afterward.
        let dir = tmpdir();
        let live = dir.join("live.db");
        seed_live_db(&live).await;
        let pre_len = fs::metadata(&live).unwrap().len();

        let out = dir.join("sanitized.db");
        run_sanitize(&live, &out).await.unwrap();

        // Live DB size shouldn't change (the model is copy-before-
        // mutate). If we ever start mutating in place, the size /
        // mtime change would break this.
        let post_len = fs::metadata(&live).unwrap().len();
        assert_eq!(pre_len, post_len, "live DB must not be mutated");

        // Verify: live DB still has the plaintext config secret.
        let url = format!("sqlite://{}?mode=ro", live.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        let qbit_pass: String = sqlx::query_scalar("SELECT qbit_pass FROM config WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(qbit_pass, "topsecret-qbit");
        pool.close().await;

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitize_scrubs_notifications_feed_history_logs_and_free_pages() {
        let dir = tmpdir();
        let live = dir.join("live.db");
        seed_live_db(&live).await;
        {
            let url = format!("sqlite://{}?mode=rwc", live.display());
            let pool = SqlitePool::connect(&url).await.unwrap();
            for (kind, cfg) in [
                (
                    "discord",
                    r#"{"webhook_url":"https://discord.com/api/webhooks/1/discord-token-sekrit"}"#,
                ),
                (
                    "webhook",
                    r#"{"url":"https://hooks.example/in?token=hook-sekrit","secret":"hmac-sekrit","headers":[["Authorization","Bearer header-sekrit"]]}"#,
                ),
            ] {
                sqlx::query(
                    "INSERT INTO notification_providers (name, kind, config_json) VALUES (?, ?, ?)",
                )
                .bind(kind)
                .bind(kind)
                .bind(cfg)
                .execute(&pool)
                .await
                .unwrap();
            }
            sqlx::query("INSERT INTO rss_seen (item_key, link) VALUES ('k1', 'https://tracker.example/torrent/1/download/path-passkey-sekrit')")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO logs (message, detail) VALUES ('Failed to query download client id=2', 'SAB request failed: error sending request for url (http://sab:8080/api?apikey=sab-sekrit&mode=queue)')")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("UPDATE indexers SET rss_last_poll_error = 'indexer request failed: for url (http://p/api?apikey=poll-sekrit)'")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }
        let out = dir.join("sanitized.db");
        let summary = run_sanitize(&live, &out).await.unwrap();
        assert_eq!(summary.notification_configs, 2);
        assert!(summary.log_rows >= 1);

        // Nothing secret is left anywhere in the file, free pages
        // included: the bytes are read raw, not through SQLite.
        let bytes = fs::read(&out).unwrap();
        let haystack = String::from_utf8_lossy(&bytes);
        for secret in [
            "discord-token-sekrit",
            "hook-sekrit",
            "hmac-sekrit",
            "header-sekrit",
            "path-passkey-sekrit",
            "sab-sekrit",
            "poll-sekrit",
            "topsecret-qbit",
            "cookie-token-xyz",
        ] {
            assert!(!haystack.contains(secret), "{secret} survived in the file");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn redact_text_takes_urls_and_keys_out_of_free_text() {
        assert_eq!(
            redact_text("SAB request failed: for url (http://sab:8080/api?apikey=x&mode=queue)"),
            "SAB request failed: for url (http://sab:8080/[redacted])"
        );
        assert_eq!(
            redact_text("rejected apikey=abc123 token=zz"),
            "rejected apikey=[redacted] token=[redacted]"
        );
        assert_eq!(redact_text("no secrets here"), "no secrets here");
    }

    #[tokio::test]
    async fn sanitize_blanks_all_known_secret_columns() {
        let dir = tmpdir();
        let live = dir.join("live.db");
        seed_live_db(&live).await;
        let out = dir.join("sanitized.db");
        let summary = run_sanitize(&live, &out).await.unwrap();

        assert!(summary.external_accounts_tokens > 0);
        assert!(summary.config_passwords > 0);
        assert!(summary.session_tokens > 0);
        assert!(summary.user_password_hashes > 0);
        assert_eq!(summary.indexer_keys, 1);
        assert_eq!(summary.client_passwords, 1);
        assert_eq!(summary.api_keys, 1);
        assert_eq!(summary.credential_urls, 2);

        let url = format!("sqlite://{}?mode=ro", out.display());
        let pool = SqlitePool::connect(&url).await.unwrap();

        let qbit_pass: String = sqlx::query_scalar("SELECT qbit_pass FROM config WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(qbit_pass, "[REDACTED]");

        let indexer_key: String = sqlx::query_scalar("SELECT api_key FROM indexers")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(indexer_key, "[REDACTED]");
        let client_pass: String = sqlx::query_scalar("SELECT password FROM download_clients")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(client_pass, "[REDACTED]");
        let scoped: String = sqlx::query_scalar("SELECT key FROM api_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(scoped.starts_with("[REDACTED-key-"), "{scoped}");
        let source_url: String = sqlx::query_scalar("SELECT source_url FROM grabbed_torrents")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(source_url, "http://prowlarr:9696/[redacted]");
        let feed_url: String = sqlx::query_scalar("SELECT url FROM direct_rss_feeds")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(feed_url, "https://tracker.example/[redacted]");

        let jf_key: String = sqlx::query_scalar("SELECT jellyfin_api_key FROM config WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(jf_key, "[REDACTED]");

        let cookie: String = sqlx::query_scalar("SELECT token FROM sessions LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            cookie.starts_with("[REDACTED-session-") && cookie.ends_with(']'),
            "session token must be redacted: {cookie}"
        );

        let hash: String = sqlx::query_scalar("SELECT password_hash FROM users LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(hash, "[REDACTED]");

        // external_accounts blobs become the SANITIZED_SENTINEL byte
        // string. Reading them raw confirms the UPDATE landed.
        let access_blob: Vec<u8> =
            sqlx::query_scalar("SELECT access_token_encrypted FROM external_accounts LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(access_blob.as_slice(), SANITIZED_SENTINEL);
        let refresh_blob: Vec<u8> =
            sqlx::query_scalar("SELECT refresh_token_encrypted FROM external_accounts LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(refresh_blob.as_slice(), SANITIZED_SENTINEL);

        pool.close().await;
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitize_skips_empty_fields_instead_of_marking_redacted() {
        // An empty config field means the user never configured that
        // integration. The sanitized output should keep the field
        // empty — reading "[REDACTED]" where a field was always blank
        // would mislead a bug reporter.
        let dir = tmpdir();
        let live = dir.join("live.db");
        let url = format!("sqlite://{}?mode=rwc", live.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        crate::models::migrate(&pool).await.unwrap();
        // Default config row has empty strings across secret columns.
        sqlx::query("INSERT INTO config (id) VALUES (1) ON CONFLICT(id) DO NOTHING")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let out = dir.join("sanitized.db");
        run_sanitize(&live, &out).await.unwrap();

        let url = format!("sqlite://{}?mode=ro", out.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        let qbit_pass: String = sqlx::query_scalar("SELECT qbit_pass FROM config WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            qbit_pass, "",
            "empty field must stay empty, not become [REDACTED]"
        );
        pool.close().await;
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitize_handles_missing_live_db_cleanly() {
        let dir = tmpdir();
        let live = dir.join("does-not-exist.db");
        let out = dir.join("sanitized.db");
        let err = run_sanitize(&live, &out).await.unwrap_err();
        assert!(err.to_lowercase().contains("not found"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitize_overwrites_stale_output_on_rerun() {
        // Running the CLI twice in a row should produce a valid
        // sanitized output on the second run. Previously a stale
        // output file + SQLite lock could leave the second run with
        // a corrupted file.
        let dir = tmpdir();
        let live = dir.join("live.db");
        seed_live_db(&live).await;
        let out = dir.join("sanitized.db");
        run_sanitize(&live, &out).await.unwrap();
        // Second run — must not error.
        run_sanitize(&live, &out).await.unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
