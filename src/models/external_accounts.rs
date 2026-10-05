//! AL / MAL external-account linkage (issue #62).
//!
//! Persists OAuth credentials + per-list import preferences for the
//! user's linked AniList or MyAnimeList account. Tokens are
//! AEAD-encrypted at rest via [`crate::services::crypto`]; plaintext
//! exists only briefly in memory, in the narrow window between
//! decryption (when building the outbound API call) and the request
//! actually firing.
//!
//! Plan decision #10: Ryokan supports at most one linked account at
//! a time. The schema has `UNIQUE(provider)` so AL and MAL can't
//! coexist as separate rows for the same provider, but the
//! "one-total" invariant lives in `link` — it rejects a second
//! provider when an account is already linked. Callers that want to
//! switch providers must `unlink` first.
//!
//! The model layer deliberately works in plaintext token strings at
//! its boundary. Callers (OAuth handlers, sync task) pass / receive
//! raw tokens; the encrypt/decrypt round-trip happens inside this
//! module so the call sites can't forget.

use sqlx::SqlitePool;

use crate::services::crypto;

pub const PROVIDER_ANILIST: &str = "anilist";
pub const PROVIDER_MAL: &str = "mal";

/// A linked external account, with tokens decrypted to plaintext for
/// outbound use. The DB-at-rest shape is always encrypted; this type
/// represents the in-memory post-decrypt view the sync task sees.
///
/// `Debug` is hand-implemented to redact `access_token` /
/// `refresh_token` so a stray `tracing::debug!("{acct:?}")` elsewhere
/// in the codebase can never leak a token into the `logs` table or
/// the tracing console. Adding new token-bearing fields requires
/// updating the redacted Debug impl below.
#[derive(Clone)]
pub struct ExternalAccount {
    pub id: i64,
    pub provider: String,
    pub provider_user_id: String,
    pub username: String,
    pub access_token: String,
    /// Empty for AL (implicit grant has no refresh token); populated
    /// for MAL.
    pub refresh_token: String,
    pub access_token_expires_at: Option<i64>,
    pub score_format: String,
    pub list_last_synced_at: Option<i64>,
    pub list_full_resync_at: Option<i64>,
    pub linked_at: i64,
    pub import_watching: bool,
    pub import_planning: bool,
    pub import_paused: bool,
    pub import_dropped: bool,
    pub import_completed: bool,
    pub skip_already_watched: bool,
    /// #62 — count of entries from the most recent sync that
    /// couldn't be mapped MAL→AL via anibridge. Always 0 for AL
    /// accounts. Surfaces on the Settings → External Accounts card
    /// as a banner so the user can see which subset of their MAL
    /// list is sitting on the negated-id sentinel path.
    pub last_sync_deferred_count: i64,
    /// #62 — sticky flag set when the most recent sync tick
    /// failed with an auth-rejection error (AL 401/403, MAL
    /// refresh-token dead). Cleared on the next successful tick.
    /// Drives the Settings UI's "Re-link required" banner — the
    /// only signal a user has that their otherwise-quiet sync has
    /// stopped working because of an expired token.
    pub last_sync_auth_failed: bool,
}

/// Input for [`link`] — the OAuth handler populates one of these
/// after a successful token exchange + Viewer fetch and hands it to
/// the model layer to persist.
///
/// `Debug` redacts the token fields, same rationale as
/// [`ExternalAccount`].
#[derive(Clone)]
pub struct LinkRequest {
    pub provider: String,
    pub provider_user_id: String,
    pub username: String,
    pub access_token: String,
    pub refresh_token: String,
    pub access_token_expires_at: Option<i64>,
    pub score_format: String,
}

impl std::fmt::Debug for ExternalAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalAccount")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("provider_user_id", &self.provider_user_id)
            .field("username", &self.username)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("access_token_expires_at", &self.access_token_expires_at)
            .field("score_format", &self.score_format)
            .field("list_last_synced_at", &self.list_last_synced_at)
            .field("list_full_resync_at", &self.list_full_resync_at)
            .field("linked_at", &self.linked_at)
            .field("import_watching", &self.import_watching)
            .field("import_planning", &self.import_planning)
            .field("import_paused", &self.import_paused)
            .field("import_dropped", &self.import_dropped)
            .field("import_completed", &self.import_completed)
            .field("skip_already_watched", &self.skip_already_watched)
            .field("last_sync_deferred_count", &self.last_sync_deferred_count)
            .field("last_sync_auth_failed", &self.last_sync_auth_failed)
            .finish()
    }
}

impl std::fmt::Debug for LinkRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkRequest")
            .field("provider", &self.provider)
            .field("provider_user_id", &self.provider_user_id)
            .field("username", &self.username)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("access_token_expires_at", &self.access_token_expires_at)
            .field("score_format", &self.score_format)
            .finish()
    }
}

/// Return the currently-linked account, or None if nothing is linked.
/// Decodes tokens with `services::crypto` — a decryption failure
/// (tampered blob, key rotation without a migration) surfaces as
/// `Err` rather than a silent None so the UI can report "re-link
/// required" instead of "not linked."
pub async fn get_current(db: &SqlitePool) -> Result<Option<ExternalAccount>, String> {
    current_raw(db)
        .await?
        .map(ExternalAccountRaw::into_plaintext)
        .transpose()
}

/// The linked account's row with its tokens still encrypted. `Err` is
/// a query failure only, so a caller can tell it apart from tokens
/// that no longer decrypt.
async fn current_raw(db: &SqlitePool) -> Result<Option<ExternalAccountRaw>, String> {
    // The one-at-a-time invariant means at most one row exists, so
    // `ORDER BY linked_at DESC LIMIT 1` is functionally equivalent to
    // a bare `LIMIT 1` today. The explicit ORDER BY is defensive: if
    // both UNIQUE(provider) and the `link()` transaction guard were
    // ever circumvented (manual DB edit, schema migration bug), the
    // most-recently-linked account is the right row to surface to the
    // UI. A bare LIMIT 1 would return whichever row sqlite happened
    // to scan first — implementation-defined and brittle.
    sqlx::query_as::<_, ExternalAccountRaw>(
        "SELECT id, provider, provider_user_id, username,
                access_token_encrypted, refresh_token_encrypted,
                access_token_expires_at, score_format,
                list_last_synced_at, list_full_resync_at, linked_at,
                import_watching, import_planning, import_paused,
                import_dropped, import_completed, skip_already_watched,
                last_sync_deferred_count, last_sync_auth_failed
           FROM external_accounts
          ORDER BY linked_at DESC
          LIMIT 1",
    )
    .fetch_optional(db)
    .await
    .map_err(|e| format!("external_accounts query failed: {e}"))
}

/// The linked account's id, provider and username, without touching
/// its tokens: what unlinking needs when the tokens no longer decrypt.
pub async fn current_row(db: &SqlitePool) -> Result<Option<(i64, String, String)>, String> {
    sqlx::query_as(
        "SELECT id, provider, username FROM external_accounts ORDER BY linked_at DESC LIMIT 1",
    )
    .fetch_optional(db)
    .await
    .map_err(|e| format!("external_accounts query failed: {e}"))
}

/// Link a new external account. Rejects when any account is already
/// linked (one-at-a-time invariant, decision #10) — callers must
/// `unlink` first to switch providers.
///
/// Re-link of the same provider is detected by `provider_user_id`
/// matching an existing row with the same provider; tokens +
/// score_format + username are updated in place instead of inserting
/// a duplicate, per decision #8. The returned id is the existing
/// row's, so callers can treat `link` as idempotent against the same
/// provider user.
pub async fn link(db: &SqlitePool, req: LinkRequest) -> Result<i64, String> {
    let now = current_unix_ts();

    let access_encrypted = crypto::encrypt(req.access_token.as_bytes())
        .map_err(|e| format!("encrypt access token: {e}"))?;
    let refresh_encrypted = crypto::encrypt(req.refresh_token.as_bytes())
        .map_err(|e| format!("encrypt refresh token: {e}"))?;

    // Two atomic statements, no explicit transaction. Each runs under
    // SQLite's per-statement write lock, so the read-check + write
    // are atomic without a BEGIN IMMEDIATE wrapper that has to be
    // hand-driven through COMMIT / ROLLBACK (and could leak an open
    // transaction back to the pool if the future is dropped between
    // the two).
    //
    // 1) UPDATE-then-RETURNING handles the re-link case. UNIQUE(provider)
    //    means at most one row per provider, so the `WHERE provider = ?
    //    AND provider_user_id = ?` predicate matches 0 or 1 row. A
    //    match returns the existing id; no rows means we fall through
    //    to step 2.
    //
    // 2) INSERT-WHERE-NOT-EXISTS-RETURNING handles fresh links. The
    //    inner `WHERE NOT EXISTS (... WHERE provider != ?)` enforces
    //    decision #10's one-at-a-time invariant against linking a
    //    second provider. The whole INSERT statement runs atomically
    //    under the write lock, so two concurrent links for different
    //    providers serialize: one sees an empty external_accounts and
    //    inserts; the second sees the first's row and inserts 0 rows.
    //    `RETURNING id` returns `None` in the 0-rows case, which we
    //    surface as the "already linked" error. Same-provider /
    //    different-user-id falls into UNIQUE(provider) at INSERT time
    //    (separate failure mode, surfaces the constraint error).
    let updated: Option<i64> = sqlx::query_scalar(
        "UPDATE external_accounts
            SET access_token_encrypted = ?,
                refresh_token_encrypted = ?,
                access_token_expires_at = ?,
                score_format = ?,
                username = ?
          WHERE provider = ? AND provider_user_id = ?
          RETURNING id",
    )
    .bind(&access_encrypted)
    .bind(&refresh_encrypted)
    .bind(req.access_token_expires_at)
    .bind(&req.score_format)
    .bind(&req.username)
    .bind(&req.provider)
    .bind(&req.provider_user_id)
    .fetch_optional(db)
    .await
    .map_err(|e| format!("external_accounts re-link UPDATE failed: {e}"))?;

    if let Some(id) = updated {
        return Ok(id);
    }

    // The NOT EXISTS guard rejects ANY existing row, not just
    // different-provider ones. The earlier UPDATE step already handled
    // the "re-link the same account" case (same provider AND same
    // user_id) idempotently — anything reaching this INSERT either has
    // no row yet (fresh link) or has a row that *isn't* a re-link
    // candidate. The pre-fix narrow guard (`WHERE provider != ?`) let
    // a same-provider-different-user link slip through to a UNIQUE(provider)
    // constraint hit, surfacing the cryptic SQL error
    // "external_accounts INSERT failed: UNIQUE constraint failed:
    // external_accounts.provider" instead of the friendly
    // "another account is already linked, unlink first" string. Widening
    // to "any row" routes both same-provider-different-user and
    // different-provider switches through the same friendly error.
    // An account whose tokens no longer decrypt (the key changed: a
    // restored key-less or sanitized backup, `RYOKAN_ENCRYPTION_KEY` set
    // over a stale key file) can't be shown, used or unlinked from
    // Settings, which renders it as "not linked". Refusing the new link
    // because of it left no way out, so it makes way for this one. Only
    // tokens that fail to decrypt count: a query error propagates, since
    // reading it as "stale" unlinked a valid account on a DB hiccup.
    if let Some(raw) = current_raw(db).await? {
        let id = raw.id;
        if raw.into_plaintext().is_err() {
            unlink(db, id).await?;
        }
    }

    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO external_accounts
            (provider, provider_user_id, username,
             access_token_encrypted, refresh_token_encrypted,
             access_token_expires_at, score_format, linked_at)
         SELECT ?, ?, ?, ?, ?, ?, ?, ?
          WHERE NOT EXISTS (SELECT 1 FROM external_accounts)
         RETURNING id",
    )
    .bind(&req.provider)
    .bind(&req.provider_user_id)
    .bind(&req.username)
    .bind(&access_encrypted)
    .bind(&refresh_encrypted)
    .bind(req.access_token_expires_at)
    .bind(&req.score_format)
    .bind(now)
    .fetch_optional(db)
    .await
    .map_err(|e| format!("external_accounts INSERT failed: {e}"))?;

    inserted.ok_or_else(|| {
        "Another external account is already linked; unlink it first before switching accounts."
            .into()
    })
}

/// Remove the linked account. Per decision #8, preserves any
/// imported series rows but wipes the per-account state: `user_score`
/// (which renders as "You: X" against the just-unlinked provider's
/// `score_format`) and the FK-on-`series` `synced_from_external_
/// account_id` (the ON DELETE SET NULL on the FK does this automatically
/// once the account row is gone). Custom-list memberships will get
/// the same treatment in a follow-up.
///
/// Without the user_score wipe, an unlink → re-link-different-provider
/// flow would render every prior AL POINT_100 score as a MAL POINT_10
/// integer (a literal `You: 85` for a series the user never rated on
/// the new account). Per the plan doc: "user scores [are] lost" on
/// re-link-different-account.
pub async fn unlink(db: &SqlitePool, id: i64) -> Result<(), String> {
    // All four steps inside one tx: a crash between them would otherwise
    // leave the account row dangling against half-cleaned per-account
    // state (user_score NULL'd but custom_lists still referencing the
    // unlinked provider, etc.).
    let mut tx = db
        .begin()
        .await
        .map_err(|e| format!("external_accounts unlink begin: {e}"))?;

    // Capture the provider before the row goes away so we can scope
    // the custom-list wipe below.
    let provider: Option<String> =
        sqlx::query_scalar("SELECT provider FROM external_accounts WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| format!("external_accounts read for provider: {e}"))?;

    // Order matters: clear user_score on rows synced from THIS
    // account BEFORE the account row goes away (the FK is set to
    // SET NULL on cascade, so after the DELETE we'd lose the join
    // key). Bounded to synced-from-this-account rows so a concurrent
    // unlink-from-other-provider doesn't wipe an unrelated account's
    // ratings.
    sqlx::query("UPDATE series SET user_score = NULL WHERE synced_from_external_account_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("user_score wipe failed: {e}"))?;

    // Drop the custom-list memberships that came from this provider.
    // Without this, the library page's "All custom lists" dropdown
    // keeps showing the unlinked account's list names, and a
    // re-link to a different account inherits stale memberships
    // until each affected series gets re-synced. Today's only
    // producer is AL, so unlinking AL effectively clears the table.
    if let Some(provider) = provider.as_deref() {
        crate::models::series_custom_lists::clear_for_provider(&mut tx, provider)
            .await
            .map_err(|e| format!("series_custom_lists wipe failed: {e}"))?;
    }

    sqlx::query("DELETE FROM external_accounts WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("external_accounts DELETE failed: {e}"))?;

    tx.commit()
        .await
        .map_err(|e| format!("external_accounts unlink commit: {e}"))?;
    Ok(())
}

/// Update the plaintext tokens on an existing row (post-refresh).
/// Encrypts on the way in so the caller keeps working in plaintext.
pub async fn update_tokens(
    db: &SqlitePool,
    id: i64,
    access_token: &str,
    refresh_token: &str,
    expires_at: Option<i64>,
) -> Result<(), String> {
    let access_encrypted =
        crypto::encrypt(access_token.as_bytes()).map_err(|e| format!("encrypt access: {e}"))?;
    let refresh_encrypted =
        crypto::encrypt(refresh_token.as_bytes()).map_err(|e| format!("encrypt refresh: {e}"))?;
    sqlx::query(
        "UPDATE external_accounts
            SET access_token_encrypted = ?,
                refresh_token_encrypted = ?,
                access_token_expires_at = ?
          WHERE id = ?",
    )
    .bind(access_encrypted)
    .bind(refresh_encrypted)
    .bind(expires_at)
    .bind(id)
    .execute(db)
    .await
    .map_err(|e| format!("update_tokens UPDATE failed: {e}"))?;
    Ok(())
}

/// Per-list import preferences, used by the Settings UI and the sync
/// task. Each field maps 1:1 to a checkbox on the External Accounts
/// card. Passed in bulk so a partial UI update doesn't produce
/// arbitrary combinations of "old checkbox on / new checkbox off."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportPreferences {
    pub import_watching: bool,
    pub import_planning: bool,
    pub import_paused: bool,
    pub import_dropped: bool,
    pub import_completed: bool,
    pub skip_already_watched: bool,
}

/// #62 — record the count of MAL→AL mapping failures from
/// the most recent sync. AL syncs always pass `0`. Read by the
/// Settings → External Accounts page handler to render the
/// "N series couldn't be mapped to AniList" banner.
pub async fn update_last_sync_deferred_count(
    db: &SqlitePool,
    id: i64,
    count: i64,
) -> Result<(), String> {
    sqlx::query("UPDATE external_accounts SET last_sync_deferred_count = ? WHERE id = ?")
        .bind(count)
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| format!("update_last_sync_deferred_count: {e}"))?;
    Ok(())
}

/// #62 — flip the auth-failure flag. Set to `true` from the
/// sync engine's auth-rejection branches (AL 401/403, MAL refresh
/// dead); cleared back to `false` on the next successful tick.
pub async fn update_last_sync_auth_failed(
    db: &SqlitePool,
    id: i64,
    flag: bool,
) -> Result<(), String> {
    sqlx::query("UPDATE external_accounts SET last_sync_auth_failed = ? WHERE id = ?")
        .bind(if flag { 1_i64 } else { 0_i64 })
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| format!("update_last_sync_auth_failed: {e}"))?;
    Ok(())
}

/// Refresh the `score_format` column. Called from the AL sync path
/// after each successful `fetch_media_list_collection` so a user
/// changing their POINT_X preference on AL post-link takes effect on
/// the next "You: X" badge render. No-op when `score_format` is
/// empty (treats "AL omitted the field on this response" as "leave
/// the known-good value alone").
pub async fn update_score_format(
    db: &SqlitePool,
    id: i64,
    score_format: &str,
) -> Result<(), String> {
    if score_format.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE external_accounts SET score_format = ? WHERE id = ?")
        .bind(score_format)
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| format!("update_score_format failed: {e}"))?;
    Ok(())
}

/// Stamp the watch-list sync cursor(s) after a successful tick.
/// `list_last_synced_at` is always written. `list_full_resync_at` is
/// also written when `was_full_sync = true` — the sync engine sets
/// this on the once-a-week backstop run so the next 6 days fall back
/// to delta mode.
///
/// `now_unix_ts` is taken as a parameter rather than read inside the
/// function so unit tests can pin a deterministic value without a
/// clock-injection layer.
pub async fn stamp_list_synced(
    db: &SqlitePool,
    id: i64,
    now_unix_ts: i64,
    was_full_sync: bool,
) -> Result<(), String> {
    if was_full_sync {
        sqlx::query(
            "UPDATE external_accounts
                SET list_last_synced_at = ?,
                    list_full_resync_at = ?
              WHERE id = ?",
        )
        .bind(now_unix_ts)
        .bind(now_unix_ts)
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| format!("stamp_list_synced (full) failed: {e}"))?;
    } else {
        sqlx::query(
            "UPDATE external_accounts
                SET list_last_synced_at = ?
              WHERE id = ?",
        )
        .bind(now_unix_ts)
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| format!("stamp_list_synced (delta) failed: {e}"))?;
    }
    Ok(())
}

pub async fn update_preferences(
    db: &SqlitePool,
    id: i64,
    prefs: ImportPreferences,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE external_accounts
            SET import_watching = ?,
                import_planning = ?,
                import_paused = ?,
                import_dropped = ?,
                import_completed = ?,
                skip_already_watched = ?
          WHERE id = ?",
    )
    .bind(prefs.import_watching)
    .bind(prefs.import_planning)
    .bind(prefs.import_paused)
    .bind(prefs.import_dropped)
    .bind(prefs.import_completed)
    .bind(prefs.skip_already_watched)
    .bind(id)
    .execute(db)
    .await
    .map_err(|e| format!("update_preferences UPDATE failed: {e}"))?;
    Ok(())
}

fn current_unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(sqlx::FromRow)]
struct ExternalAccountRaw {
    id: i64,
    provider: String,
    provider_user_id: String,
    username: String,
    access_token_encrypted: Vec<u8>,
    refresh_token_encrypted: Vec<u8>,
    access_token_expires_at: Option<i64>,
    score_format: String,
    list_last_synced_at: Option<i64>,
    list_full_resync_at: Option<i64>,
    linked_at: i64,
    import_watching: bool,
    import_planning: bool,
    import_paused: bool,
    import_dropped: bool,
    import_completed: bool,
    skip_already_watched: bool,
    last_sync_deferred_count: i64,
    last_sync_auth_failed: bool,
}

impl ExternalAccountRaw {
    fn into_plaintext(self) -> Result<ExternalAccount, String> {
        let access_bytes = crypto::decrypt(&self.access_token_encrypted)
            .map_err(|e| format!("decrypt access token: {e}"))?;
        // Refresh token is allowed to be empty (AL case). Encrypted
        // empty-plaintext is still an AEAD-tagged blob, so decrypt
        // returns `Ok(Vec::new())`. An all-zero blob — from a pre-
        // #62 row that somehow sneaks through, say — would fail the
        // tag check here and surface as "re-link required."
        let refresh_bytes = if self.refresh_token_encrypted.is_empty() {
            Vec::new()
        } else {
            crypto::decrypt(&self.refresh_token_encrypted)
                .map_err(|e| format!("decrypt refresh token: {e}"))?
        };
        let access_token = String::from_utf8(access_bytes)
            .map_err(|e| format!("access token is not UTF-8: {e}"))?;
        let refresh_token = String::from_utf8(refresh_bytes)
            .map_err(|e| format!("refresh token is not UTF-8: {e}"))?;
        Ok(ExternalAccount {
            id: self.id,
            provider: self.provider,
            provider_user_id: self.provider_user_id,
            username: self.username,
            access_token,
            refresh_token,
            access_token_expires_at: self.access_token_expires_at,
            score_format: self.score_format,
            list_last_synced_at: self.list_last_synced_at,
            list_full_resync_at: self.list_full_resync_at,
            linked_at: self.linked_at,
            import_watching: self.import_watching,
            import_planning: self.import_planning,
            import_paused: self.import_paused,
            import_dropped: self.import_dropped,
            import_completed: self.import_completed,
            skip_already_watched: self.skip_already_watched,
            last_sync_deferred_count: self.last_sync_deferred_count,
            last_sync_auth_failed: self.last_sync_auth_failed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn in_memory_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&pool).await.unwrap();
        pool
    }

    fn sample_anilist_request() -> LinkRequest {
        LinkRequest {
            provider: PROVIDER_ANILIST.to_string(),
            provider_user_id: "12345".to_string(),
            username: "user123".to_string(),
            access_token: "al-access-token-abc".to_string(),
            refresh_token: String::new(),
            access_token_expires_at: None,
            score_format: "POINT_10".to_string(),
        }
    }

    fn sample_mal_request() -> LinkRequest {
        LinkRequest {
            provider: PROVIDER_MAL.to_string(),
            provider_user_id: "mal_user".to_string(),
            username: "mal_user".to_string(),
            access_token: "mal-access-token-xyz".to_string(),
            refresh_token: "mal-refresh-token-uvw".to_string(),
            access_token_expires_at: Some(1_800_000_000),
            score_format: "POINT_10".to_string(),
        }
    }

    #[tokio::test]
    async fn get_current_returns_none_when_no_account_linked() {
        let db = in_memory_pool().await;
        assert!(get_current(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn link_then_get_roundtrips_plaintext_tokens() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_mal_request()).await.unwrap();
        let got = get_current(&db).await.unwrap().expect("linked");
        assert_eq!(got.id, id);
        // The whole point of the crypto layer: plaintext tokens
        // round-trip cleanly, while the DB-at-rest blob is
        // ciphertext (verified by the encryption tests in
        // `services::crypto`).
        assert_eq!(got.access_token, "mal-access-token-xyz");
        assert_eq!(got.refresh_token, "mal-refresh-token-uvw");
        assert_eq!(got.provider, PROVIDER_MAL);
        assert_eq!(got.access_token_expires_at, Some(1_800_000_000));
    }

    #[tokio::test]
    async fn link_second_provider_is_rejected_while_first_exists() {
        // Plan decision #10 — one-at-a-time. A user with AL linked
        // must unlink before linking MAL, no exception. This
        // protects against a settings-UI bug that fires two link
        // calls back-to-back.
        let db = in_memory_pool().await;
        link(&db, sample_anilist_request()).await.unwrap();
        let err = link(&db, sample_mal_request()).await.unwrap_err();
        assert!(
            err.to_lowercase().contains("already linked"),
            "error must call out the one-at-a-time rule: {err}"
        );
    }

    #[tokio::test]
    async fn link_same_provider_and_user_is_idempotent_and_updates_tokens() {
        // Re-link of the same account refreshes tokens on the same
        // row rather than tripping UNIQUE(provider). Matches
        // decision #8's re-link-same-account idempotence.
        let db = in_memory_pool().await;
        let first_id = link(&db, sample_mal_request()).await.unwrap();

        let mut updated = sample_mal_request();
        updated.access_token = "mal-rotated-access".to_string();
        updated.refresh_token = "mal-rotated-refresh".to_string();
        updated.access_token_expires_at = Some(1_900_000_000);
        let second_id = link(&db, updated).await.unwrap();
        assert_eq!(first_id, second_id, "same row re-used on re-link");

        let got = get_current(&db).await.unwrap().expect("still linked");
        assert_eq!(got.access_token, "mal-rotated-access");
        assert_eq!(got.refresh_token, "mal-rotated-refresh");
        assert_eq!(got.access_token_expires_at, Some(1_900_000_000));
    }

    #[tokio::test]
    async fn an_account_whose_tokens_no_longer_decrypt_makes_way_for_a_new_link() {
        // After a key change the old row can't be read, shown or
        // unlinked; a new link used to be refused because of it.
        let db = in_memory_pool().await;
        link(&db, sample_mal_request()).await.unwrap();
        sqlx::query("UPDATE external_accounts SET access_token_encrypted = x'00', refresh_token_encrypted = x'00'")
            .execute(&db)
            .await
            .unwrap();
        assert!(
            get_current(&db).await.is_err(),
            "unreadable, as after a key change"
        );

        let mut other_user = sample_mal_request();
        other_user.provider_user_id = "different_mal_user".to_string();
        link(&db, other_user)
            .await
            .expect("the stale row makes way");
        let current = get_current(&db).await.unwrap().unwrap();
        assert_eq!(current.provider_user_id, "different_mal_user");
    }

    #[tokio::test]
    async fn a_failed_read_during_a_link_leaves_the_linked_account_alone() {
        // `get_current` errs on a query failure as well as on tokens that
        // don't decrypt, and the link read either as "stale" and unlinked
        // a valid account.
        let db = in_memory_pool().await;
        link(&db, sample_mal_request()).await.unwrap();
        // A read failure the narrow `current_row` query doesn't hit.
        sqlx::query("UPDATE external_accounts SET last_sync_deferred_count = 'unreadable'")
            .execute(&db)
            .await
            .unwrap();
        let mut other_user = sample_mal_request();
        other_user.provider_user_id = "different_mal_user".to_string();
        assert!(link(&db, other_user).await.is_err());
        let rows: Vec<String> =
            sqlx::query_scalar("SELECT provider_user_id FROM external_accounts")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(rows, vec![sample_mal_request().provider_user_id]);
    }

    #[tokio::test]
    async fn link_same_provider_different_user_id_is_rejected() {
        // Same-provider-different-user must reach the friendly
        // "unlink first" error, NOT the cryptic UNIQUE(provider)
        // constraint string. The narrow `WHERE provider != ?` guard
        // used to let this case slip past the NOT EXISTS check and
        // hit the schema constraint at INSERT time, surfacing
        // "external_accounts INSERT failed: UNIQUE constraint failed"
        // to the OAuth submit toast — a confusing error for what's
        // actually "you already linked a different account; unlink
        // first." The widened guard now catches both same-provider
        // and cross-provider switches with the same message.
        let db = in_memory_pool().await;
        link(&db, sample_mal_request()).await.unwrap();
        let mut other_user = sample_mal_request();
        other_user.provider_user_id = "different_mal_user".to_string();
        let err = link(&db, other_user).await.unwrap_err();
        assert!(
            err.contains("Another external account is already linked"),
            "expected friendly unlink-first message, got: {err}"
        );
        assert!(
            !err.to_lowercase().contains("unique"),
            "raw SQL constraint error must not leak through to the user: {err}"
        );
    }

    #[tokio::test]
    async fn unlink_removes_row_and_leaves_slot_open() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_anilist_request()).await.unwrap();
        unlink(&db, id).await.unwrap();
        assert!(get_current(&db).await.unwrap().is_none());

        // Slot is open — linking MAL now must succeed (one-at-a-time
        // rule is per "currently linked," not per "ever linked").
        link(&db, sample_mal_request()).await.unwrap();
        assert_eq!(
            get_current(&db).await.unwrap().unwrap().provider,
            PROVIDER_MAL
        );
    }

    #[tokio::test]
    async fn update_tokens_encrypts_new_plaintext_and_roundtrips() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_mal_request()).await.unwrap();
        update_tokens(&db, id, "new-access", "new-refresh", Some(2_000_000_000))
            .await
            .unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert_eq!(got.access_token, "new-access");
        assert_eq!(got.refresh_token, "new-refresh");
        assert_eq!(got.access_token_expires_at, Some(2_000_000_000));
    }

    #[tokio::test]
    async fn update_preferences_persists_all_six_flags() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_anilist_request()).await.unwrap();
        let prefs = ImportPreferences {
            import_watching: true,
            import_planning: false,
            import_paused: true,
            import_dropped: true,
            import_completed: true,
            skip_already_watched: true,
        };
        update_preferences(&db, id, prefs).await.unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert!(got.import_watching);
        assert!(!got.import_planning);
        assert!(got.import_paused);
        assert!(got.import_dropped);
        assert!(got.import_completed);
        assert!(got.skip_already_watched);
    }

    #[tokio::test]
    async fn default_preferences_match_the_plan_doc() {
        // Plan decision baseline: Watching + PTW on by default,
        // Paused/Dropped/Completed off, skip-already-watched off.
        // A silent change to the schema defaults would be a
        // user-visible regression at first-link time — pin them.
        let db = in_memory_pool().await;
        link(&db, sample_anilist_request()).await.unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert!(got.import_watching, "Watching default on");
        assert!(got.import_planning, "PTW default on");
        assert!(!got.import_paused, "Paused default off");
        assert!(!got.import_dropped, "Dropped default off");
        assert!(!got.import_completed, "Completed default off");
        assert!(!got.skip_already_watched, "Skip-watched default off");
    }

    #[tokio::test]
    async fn stamp_list_synced_delta_only_writes_last_synced_at() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_anilist_request()).await.unwrap();
        stamp_list_synced(&db, id, 1_700_000_000, false)
            .await
            .unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert_eq!(got.list_last_synced_at, Some(1_700_000_000));
        assert!(
            got.list_full_resync_at.is_none(),
            "delta tick must not advance the full-resync cursor"
        );
    }

    #[tokio::test]
    async fn stamp_list_synced_full_writes_both_cursors() {
        let db = in_memory_pool().await;
        let id = link(&db, sample_anilist_request()).await.unwrap();
        stamp_list_synced(&db, id, 1_700_000_000, true)
            .await
            .unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert_eq!(got.list_last_synced_at, Some(1_700_000_000));
        assert_eq!(got.list_full_resync_at, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn stamp_list_synced_delta_after_full_keeps_full_cursor() {
        // Full sync at T=100, delta at T=500 → list_last_synced_at
        // bumps to 500, but list_full_resync_at stays at 100 (the
        // weekly-backstop window is measured from the last full).
        let db = in_memory_pool().await;
        let id = link(&db, sample_anilist_request()).await.unwrap();
        stamp_list_synced(&db, id, 100, true).await.unwrap();
        stamp_list_synced(&db, id, 500, false).await.unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert_eq!(got.list_last_synced_at, Some(500));
        assert_eq!(got.list_full_resync_at, Some(100));
    }

    #[tokio::test]
    async fn empty_refresh_token_roundtrips_as_empty_string() {
        // AL has no refresh token (implicit grant). Schema stores
        // empty blob; decrypt path short-circuits to empty string.
        let db = in_memory_pool().await;
        link(&db, sample_anilist_request()).await.unwrap();
        let got = get_current(&db).await.unwrap().unwrap();
        assert!(
            got.refresh_token.is_empty(),
            "AL refresh token must roundtrip as empty"
        );
    }
}
