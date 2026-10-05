//! Scoped API keys (issue #114).
//!
//! Multiple per-purpose API keys with scope-based access control. Lets
//! users hand out narrow keys to integrations (calendar-only key for
//! an iCal subscriber, search-only key for a Recyclarr-equivalent)
//! without sharing a full admin key.
//!
//! ## Storage
//!
//! `key` is the plaintext (UNIQUE-indexed) — same storage shape as
//! every other Ryokan integration key (`config.sonarr_api_key`,
//! `config.autobrr_api_key`, `config.jellyfin_api_key`). Encrypting
//! just this one column would be a defense-in-depth illusion when
//! those plaintext keys live alongside it in the same DB; consistency
//! with the rest of the codebase wins over the GitHub-PAT pattern
//! we briefly tried.
//!
//! ## Scopes
//!
//! Five scope strings define the vocabulary up front so it's stable
//! from day one. Only `calendar` is wired in 1.7 (gates the iCal feed
//! at `/api/calendar.ics`); the others are reserved for future
//! consumers (Recyclarr-equivalent, etc.). See [`ALL_SCOPES`].
//!
//! Scope semantics:
//! - `calendar` — read-only access to the iCal feed.
//! - `search` — read-only access to manual search.
//! - `library:read` — read-only access to library state.
//! - `library:write` — full library mutation.
//! - `admin` — all-access; equivalent to holding every other scope.
//!
//! ## Auth model
//!
//! Endpoint wiring is **opt-in default-deny**: only routes explicitly
//! tagged with [`require_scope`](crate::handlers::scoped_auth::require_scope)
//! accept scoped-key auth. Untagged routes stay cookie-only — same
//! shape as today, no behavior change. This deliberately ducks the
//! cookie-OR-key composition problem until we have a dual-mode
//! endpoint to design against.

use sqlx::{Row, SqlitePool};

/// Every valid scope string. Source-of-truth for both the create-key
/// modal's checkbox list and the middleware's grant check. Adding a
/// new scope means adding it here, in the create-key UI's checkbox
/// row, and in `LogCategory::Auth`'s display copy if it warrants
/// special-case logging.
pub const ALL_SCOPES: &[&str] = &[
    "calendar",
    "search",
    "library:read",
    "library:write",
    "admin",
];

/// In-memory shape for an `api_keys` row. The plaintext key is NOT a
/// field here — the caller of [`create`] gets it as the second tuple
/// element exactly once and is responsible for showing it to the
/// user; subsequent reads of an [`ApiKey`] row from the DB never
/// surface the plaintext.
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub enabled: bool,
}

impl ApiKey {
    /// `true` when this row's scope set grants access to `required`.
    /// `admin` is the universal-grant scope; otherwise an exact match
    /// against the row's scope list. Scope strings are compared
    /// case-sensitively because they're internal vocabulary
    /// ([`ALL_SCOPES`]) — case-insensitive compare would imply the
    /// user-facing UI accepts capitalized variants, which it doesn't.
    pub fn grants(&self, required: &str) -> bool {
        if !self.enabled {
            return false;
        }
        self.scopes.iter().any(|s| s == "admin" || s == required)
    }

    /// Format `created_at` for the Settings card. UTC, "MMM DD, YYYY
    /// HH:MM" shape — readable without locale shenanigans, no
    /// "Loading..." → JS-hydrate flash. Single-user self-hosted PVR;
    /// the user knows their server's timezone.
    pub fn created_at_display(&self) -> String {
        format_unix_ts(self.created_at)
    }

    /// Same shape as [`created_at_display`] but with a `"never"`
    /// fallback for keys that have never been used. Treats `Some(0)`
    /// as "never" too — `touch_last_used` always writes a real epoch
    /// timestamp, but a restored backup or a hand-edited row could
    /// still produce that shape and "Jan 01, 1970 00:00" reads as a
    /// glitch rather than the intended never-used state.
    pub fn last_used_display(&self) -> String {
        match self.last_used_at {
            Some(ts) if ts > 0 => format_unix_ts(ts),
            _ => "never".to_string(),
        }
    }
}

fn format_unix_ts(secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.format("%b %d, %Y %H:%M").to_string())
        .unwrap_or_default()
}

/// Generate a 32-byte CSPRNG plaintext key as a 64-char hex string.
/// Same shape as [`session::generate_token`](super::session) — the
/// security properties are the same (32 bytes is well above the
/// 16-byte threshold for "computationally infeasible to brute-force"
/// against any realistic attacker model).
pub fn generate_key() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

/// Insert a new API key with the given name + scopes. Returns
/// `(row_id, plaintext_key)`. The plaintext is also persisted to
/// the `key` column so the Settings UI's per-card Show button can
/// surface it later; this matches the storage shape of every other
/// Ryokan integration key. Duplicate-name is permitted (users may
/// reasonably want two "iCal subscriber" keys for redundancy);
/// duplicate plaintext is rejected by the UNIQUE constraint, but
/// 32-byte CSPRNG output makes collisions astronomically unlikely.
///
/// Scope strings are validated against [`ALL_SCOPES`]; an unknown
/// scope returns an error rather than silently storing it (a typo
/// in the create-key form would otherwise produce a key that grants
/// nothing and is hard to debug).
pub async fn create(
    db: &SqlitePool,
    name: &str,
    scopes: &[String],
) -> Result<(i64, String), String> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err("Key name cannot be empty".to_string());
    }
    for scope in scopes {
        if !ALL_SCOPES.contains(&scope.as_str()) {
            return Err(format!("Unknown scope: {scope}"));
        }
    }
    let plaintext = generate_key();
    let scopes_json =
        serde_json::to_string(scopes).map_err(|e| format!("Failed to serialize scopes: {e}"))?;
    let row: (i64,) =
        sqlx::query_as("INSERT INTO api_keys (name, key, scopes) VALUES (?, ?, ?) RETURNING id")
            .bind(trimmed_name)
            .bind(&plaintext)
            .bind(&scopes_json)
            .fetch_one(db)
            .await
            .map_err(|e| format!("Failed to create API key: {e}"))?;
    Ok((row.0, plaintext))
}

/// Read the plaintext key for a given row id. Used by the Settings
/// UI's per-card Show button (cookie-auth gated through the regular
/// `require_auth` middleware). Returns `Ok(None)` for a missing row
/// OR a row with an empty `key` column (upgraders from the previous
/// hash+encrypted schema have no plaintext available; they need to
/// delete + recreate to recover the key value).
pub async fn get_plaintext(db: &SqlitePool, id: i64) -> Result<Option<String>, String> {
    let row: Option<(String,)> = sqlx::query_as("SELECT key FROM api_keys WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(|e| format!("Lookup failed: {e}"))?;
    Ok(row.and_then(|(k,)| if k.is_empty() { None } else { Some(k) }))
}

/// Read every row in the `api_keys` table, sorted newest-first. Used
/// by the Settings → API Keys tab to render the list. Doesn't expose
/// the hash — the model layer knows the column shape but the UI has
/// no use for it.
pub async fn list(db: &SqlitePool) -> Result<Vec<ApiKey>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, scopes, created_at, last_used_at, enabled \
         FROM api_keys \
         ORDER BY created_at DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(parse_row).collect())
}

/// Look up a key by its plaintext form. Returns `None` for a missing
/// row or a row with `enabled = 0` (disabled keys behave identically
/// to deleted keys from the request path's POV). The compare happens
/// via SQL on a UNIQUE-indexed column — no constant-time concern
/// because the key string is the unforgeable secret: a timing oracle
/// on the lookup leaks at most "this key exists in the DB," which
/// is the same answer a successful-vs-failed response conveys.
///
/// A sanitized backup's placeholder (`[REDACTED-key-1]`) is never a key,
/// even where a database still holds one: anyone can guess it.
pub async fn lookup_by_plaintext(
    db: &SqlitePool,
    plaintext: &str,
) -> Result<Option<ApiKey>, sqlx::Error> {
    if crate::services::sanitize::is_placeholder(plaintext) {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT id, name, scopes, created_at, last_used_at, enabled \
         FROM api_keys \
         WHERE key = ? AND enabled = 1",
    )
    .bind(plaintext)
    .fetch_optional(db)
    .await?;
    Ok(row.map(parse_row))
}

/// Stamp `last_used_at` after a successful request match. Best-effort:
/// callers ignore failures here so a DB hiccup on the audit-trail
/// side doesn't kill an otherwise-valid request. Called from the
/// `require_scope` middleware after the scope check passes, before
/// running the inner handler.
pub async fn touch_last_used(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE api_keys SET last_used_at = strftime('%s', 'now') WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Delete a key by id. Idempotent — already-deleted id returns Ok with
/// zero rows affected. Used by the Settings → API Keys delete button.
pub async fn delete(db: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM api_keys WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Toggle the `enabled` flag. Used by the Settings → API Keys tab so
/// users can revoke a key without losing its name/scopes/history (re-
/// enabling restores the row immediately). A disabled key still
/// occupies the UNIQUE(key_hash) slot — there's no resurrection-via-
/// re-create concern.
pub async fn set_enabled(db: &SqlitePool, id: i64, enabled: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE api_keys SET enabled = ? WHERE id = ?")
        .bind(if enabled { 1_i64 } else { 0_i64 })
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Common row → struct conversion. JSON parse failure on `scopes`
/// falls back to an empty list rather than dropping the row entirely
/// — a malformed `scopes` column shouldn't make a key disappear from
/// the Settings list, just render with no scope badges so the user
/// can delete it manually.
fn parse_row(row: sqlx::sqlite::SqliteRow) -> ApiKey {
    let scopes_json: String = row.try_get("scopes").unwrap_or_default();
    let scopes: Vec<String> = serde_json::from_str(&scopes_json).unwrap_or_default();
    let enabled: i64 = row.try_get("enabled").unwrap_or(0);
    ApiKey {
        id: row.try_get("id").unwrap_or(0),
        name: row.try_get("name").unwrap_or_default(),
        scopes,
        created_at: row.try_get("created_at").unwrap_or(0),
        last_used_at: row.try_get("last_used_at").ok(),
        enabled: enabled != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    #[tokio::test]
    async fn create_returns_plaintext_and_persists_it() {
        let pool = in_memory_pool().await;
        let (id, plaintext) = create(&pool, "Test", &["calendar".to_string()])
            .await
            .unwrap();
        assert!(id > 0);
        assert_eq!(plaintext.len(), 64, "32-byte hex = 64 chars");
        let stored: String = sqlx::query_scalar("SELECT key FROM api_keys WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, plaintext);
    }

    #[tokio::test]
    async fn get_plaintext_returns_the_stored_key() {
        let pool = in_memory_pool().await;
        let (id, plaintext) = create(&pool, "Test", &["calendar".to_string()])
            .await
            .unwrap();
        let revealed = get_plaintext(&pool, id).await.unwrap();
        assert_eq!(revealed, Some(plaintext));
    }

    #[tokio::test]
    async fn get_plaintext_returns_none_for_unknown_id() {
        let pool = in_memory_pool().await;
        let revealed = get_plaintext(&pool, 99_999).await.unwrap();
        assert!(revealed.is_none());
    }

    #[tokio::test]
    async fn get_plaintext_returns_none_for_upgrader_row_with_empty_key() {
        // Upgraders from the previous hash+encrypted schema get the
        // new `key` column with the default empty string — there's
        // no plaintext to recover for those rows. Pin that
        // get_plaintext returns Ok(None), distinct from "row not
        // found," so the handler can surface a "rotate to recover"
        // message rather than a generic 404.
        let pool = in_memory_pool().await;
        let (id, _) = create(&pool, "Test", &["calendar".to_string()])
            .await
            .unwrap();
        sqlx::query("UPDATE api_keys SET key = '' WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let revealed = get_plaintext(&pool, id).await.unwrap();
        assert!(revealed.is_none());
    }

    #[tokio::test]
    async fn create_rejects_unknown_scope() {
        let pool = in_memory_pool().await;
        let err = create(&pool, "Test", &["nonexistent".to_string()])
            .await
            .unwrap_err();
        assert!(err.contains("Unknown scope"));
    }

    #[tokio::test]
    async fn create_rejects_empty_name() {
        let pool = in_memory_pool().await;
        let err = create(&pool, "   ", &["calendar".to_string()])
            .await
            .unwrap_err();
        assert!(err.contains("cannot be empty"));
    }

    #[tokio::test]
    async fn lookup_by_plaintext_finds_enabled_key() {
        let pool = in_memory_pool().await;
        let (_, plaintext) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        let found = lookup_by_plaintext(&pool, &plaintext).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Cal");
    }

    #[tokio::test]
    async fn lookup_skips_disabled_key() {
        // Disabled keys behave identically to deleted keys from the
        // middleware's POV — pin the contract so flipping enabled
        // can't be a silent half-revoke.
        let pool = in_memory_pool().await;
        let (id, plaintext) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        set_enabled(&pool, id, false).await.unwrap();
        let found = lookup_by_plaintext(&pool, &plaintext).await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn lookup_returns_none_for_unknown_plaintext() {
        let pool = in_memory_pool().await;
        let _ = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        let found = lookup_by_plaintext(&pool, "not-a-real-key").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn lookup_never_matches_a_sanitized_placeholder() {
        // A sanitized database's keys read `[REDACTED-key-<rowid>]`, so
        // anyone could guess one where the placeholders were not cleared.
        let pool = in_memory_pool().await;
        let (id, _) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        sqlx::query("UPDATE api_keys SET key = '[REDACTED-key-' || rowid || ']' WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let placeholder = format!("[REDACTED-key-{id}]");
        let found = lookup_by_plaintext(&pool, &placeholder).await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn grants_admin_satisfies_any_scope() {
        // admin is the universal-grant scope. Pin it so a future
        // refactor that special-cases scope strings doesn't quietly
        // break the all-access semantics.
        let key = ApiKey {
            id: 1,
            name: "A".into(),
            scopes: vec!["admin".to_string()],
            created_at: 0,
            last_used_at: None,
            enabled: true,
        };
        assert!(key.grants("calendar"));
        assert!(key.grants("library:write"));
        assert!(key.grants("admin"));
    }

    #[tokio::test]
    async fn grants_exact_match_required_for_non_admin() {
        let key = ApiKey {
            id: 1,
            name: "A".into(),
            scopes: vec!["calendar".to_string()],
            created_at: 0,
            last_used_at: None,
            enabled: true,
        };
        assert!(key.grants("calendar"));
        assert!(!key.grants("search"));
        assert!(!key.grants("library:read"));
    }

    #[tokio::test]
    async fn grants_returns_false_when_disabled() {
        // Defense-in-depth: even though `lookup_by_plaintext` filters
        // disabled rows, callers that hold an `ApiKey` from another
        // path (tests, in-flight reload) should still see the
        // disabled flag honored.
        let key = ApiKey {
            id: 1,
            name: "A".into(),
            scopes: vec!["admin".to_string()],
            created_at: 0,
            last_used_at: None,
            enabled: false,
        };
        assert!(!key.grants("calendar"));
    }

    #[tokio::test]
    async fn touch_last_used_stamps_recent_timestamp() {
        let pool = in_memory_pool().await;
        let (id, _) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        // Initially NULL.
        let initial: Option<i64> =
            sqlx::query_scalar("SELECT last_used_at FROM api_keys WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(initial.is_none());
        touch_last_used(&pool, id).await.unwrap();
        let after: Option<i64> =
            sqlx::query_scalar("SELECT last_used_at FROM api_keys WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(after.is_some());
        let now = chrono::Utc::now().timestamp();
        assert!(
            (now - after.unwrap()).abs() < 5,
            "stamp should be within ~5s of now"
        );
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let pool = in_memory_pool().await;
        let (id, _) = create(&pool, "Cal", &["calendar".to_string()])
            .await
            .unwrap();
        delete(&pool, id).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn list_returns_newest_first() {
        let pool = in_memory_pool().await;
        let (a, _) = create(&pool, "first", &["calendar".to_string()])
            .await
            .unwrap();
        // Force a measurable gap so ORDER BY created_at DESC
        // produces a stable result on fast SQLite — without the
        // sleep, both rows can land in the same epoch second and
        // the ORDER BY tiebreaker is undefined.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let (b, _) = create(&pool, "second", &["search".to_string()])
            .await
            .unwrap();
        let keys = list(&pool).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].id, b);
        assert_eq!(keys[1].id, a);
    }

    #[test]
    fn generate_key_produces_distinct_values() {
        // 32 bytes of CSPRNG; collision is astronomically unlikely.
        let a = generate_key();
        let b = generate_key();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }
}
