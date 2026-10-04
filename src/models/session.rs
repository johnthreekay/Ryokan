use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// What the `sessions.token` column holds: the SHA-256 of the cookie
/// value, never the value itself. A copy of the database (a leftover
/// `ryokan.db.pre-restore-*`, a readable file on a shared host) used to
/// hand over working admin cookies; a hash can't be replayed. The same
/// reasoning as `login_device::hash`.
fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Create a new session token for a user.
pub async fn create_session(db: &SqlitePool, user_id: i64) -> Result<String, sqlx::Error> {
    let token = generate_token();

    sqlx::query("INSERT INTO sessions (token, user_id) VALUES (?, ?)")
        .bind(token_hash(&token))
        .bind(user_id)
        .execute(db)
        .await?;

    Ok(token)
}

/// Validate a session token and return the user_id if valid. Sessions older
/// than 7 days are treated as invalid to match the 604800-second Max-Age
/// sent on the cookie itself — without this the server-side row was valid
/// forever and a stolen token never expired. Expired rows are swept by
/// [`cleanup`] from the hourly background task.
pub async fn validate_session(db: &SqlitePool, token: &str) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT user_id FROM sessions WHERE token = ? AND created_at > datetime('now', '-7 days')",
    )
    .bind(token_hash(token))
    .fetch_optional(db)
    .await?;

    Ok(row.map(|(id,)| id))
}

/// Drop session rows whose `created_at` is older than `max_age_days`.
/// Called from the hourly cleanup task so the `sessions` table doesn't
/// accumulate expired rows indefinitely — without this sweep, every login
/// leaves a permanent row that [`validate_session`] simply ignores once
/// stale. Use 7 days to match the cookie Max-Age and the TTL check above.
pub async fn cleanup(db: &SqlitePool, max_age_days: i32) -> Result<u64, sqlx::Error> {
    let cutoff = format!("-{} days", max_age_days);
    let res = sqlx::query("DELETE FROM sessions WHERE created_at < datetime('now', ?)")
        .bind(cutoff)
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

/// Delete a session (logout).
pub async fn delete_session(db: &SqlitePool, token: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(token_hash(token))
        .execute(db)
        .await?;
    Ok(())
}

/// Generate a cryptographically random session token. 32 bytes of CSPRNG
/// output rendered as a 64-char hex string. `rand::random::<[u8; 32]>()`
/// pulls from `ThreadRng` (ChaCha-based, reseeded automatically) — same
/// security properties as the older `thread_rng().gen()` loop, just
/// without the manual collection.
fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_database_holds_a_hash_not_the_cookie() {
        // A copy of the database used to hand over working cookies.
        let db = crate::test_support::in_memory_pool().await;
        sqlx::query("INSERT INTO users (username, password_hash) VALUES ('admin', 'x')")
            .execute(&db)
            .await
            .unwrap();
        let token = create_session(&db, 1).await.unwrap();
        let stored: String = sqlx::query_scalar("SELECT token FROM sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_ne!(stored, token, "the raw token never reaches the database");
        assert_eq!(stored, token_hash(&token));
        assert_eq!(validate_session(&db, &token).await.unwrap(), Some(1));
        assert_eq!(
            validate_session(&db, &stored).await.unwrap(),
            None,
            "the stored value is not a cookie"
        );
        delete_session(&db, &token).await.unwrap();
        assert_eq!(validate_session(&db, &token).await.unwrap(), None);
    }

    #[tokio::test]
    async fn sessions_from_before_hashing_are_cleared_once() {
        let db = crate::test_support::in_memory_pool().await;
        sqlx::query("INSERT INTO users (username, password_hash) VALUES ('admin', 'x')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM schema_migrations WHERE id = 'sessions_hashed_v1'")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (token, user_id) VALUES ('raw-old-token', 1)")
            .execute(&db)
            .await
            .unwrap();
        crate::models::migrate(&db).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0);
        // Once only: a session made after the migration survives the next boot.
        let token = create_session(&db, 1).await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        assert_eq!(validate_session(&db, &token).await.unwrap(), Some(1));
    }
}
