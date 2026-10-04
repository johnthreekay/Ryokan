//! Browsers that have logged in before, for the login throttle
//! (OWASP's "device cookies"). Failed logins are throttled per username
//! and per IP, so anyone could keep failing as `admin` and lock the real
//! admin out, from every IP at once behind a reverse proxy that isn't
//! trusted for `X-Forwarded-For`. A login that carries a known device
//! cookie is throttled on that device's own bucket instead, so someone
//! else's failures never block a browser that has logged in before.
//!
//! The cookie holds a random token; the table holds its SHA-256, so a
//! database dump can't be replayed as a device.

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// Name of the device cookie.
pub const COOKIE: &str = "ryokan_device";

/// How long a device stays known without logging in again: 400 days,
/// the longest browsers keep a cookie.
pub const MAX_AGE_DAYS: i64 = 400;

/// The stored form of `token`, also its throttle bucket key.
pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Mint a device token for `user_id`. Returns the token for the cookie.
pub async fn create(db: &SqlitePool, user_id: i64) -> Result<String, sqlx::Error> {
    let bytes: [u8; 32] = rand::random();
    let token = hex::encode(bytes);
    sqlx::query("INSERT INTO login_devices (token_hash, user_id) VALUES (?, ?)")
        .bind(hash(&token))
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(token)
}

/// True when `token` belongs to a device that logged in within
/// [`MAX_AGE_DAYS`]; marks it used. A lookup error reads as unknown, so
/// the login falls back to the ordinary per-username and per-IP buckets.
pub async fn is_known(db: &SqlitePool, token: &str) -> bool {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    sqlx::query(
        "UPDATE login_devices SET last_used_at = strftime('%s', 'now') \
          WHERE token_hash = ? AND last_used_at > strftime('%s', 'now') - ?",
    )
    .bind(hash(token))
    .bind(MAX_AGE_DAYS * 86_400)
    .execute(db)
    .await
    .is_ok_and(|r| r.rows_affected() > 0)
}

/// Drop devices unused for [`MAX_AGE_DAYS`].
pub async fn cleanup(db: &SqlitePool) -> Result<u64, sqlx::Error> {
    let res =
        sqlx::query("DELETE FROM login_devices WHERE last_used_at <= strftime('%s', 'now') - ?")
            .bind(MAX_AGE_DAYS * 86_400)
            .execute(db)
            .await?;
    Ok(res.rows_affected())
}
