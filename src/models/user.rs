use sqlx::SqlitePool;
use std::sync::LazyLock;

#[allow(dead_code)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
}

/// Pre-computed bcrypt hash of a dummy password, used to equalize the
/// timing of failed logins against nonexistent usernames with failed
/// logins against existing usernames. Without this, the `None` branch
/// of [`verify_user`] would short-circuit to `Ok(None)` and finish ~50ms
/// faster than the `Some` branch that actually calls `bcrypt::verify`,
/// giving an attacker a trivial timing side channel for username
/// enumeration. Computed once at first use so the cost is paid only
/// once for the entire process.
static DUMMY_BCRYPT_HASH: LazyLock<String> = LazyLock::new(|| {
    // The input password here is irrelevant — no real user will ever
    // authenticate against this hash because the comparison always
    // happens against random attacker-supplied input.
    bcrypt::hash("ryokan-timing-equalizer-dummy-password", 10)
        .expect("bcrypt::hash of a fixed input should not fail")
});

/// Force the [`DUMMY_BCRYPT_HASH`] LazyLock to initialise. Call once during
/// startup so the ~50ms `bcrypt::hash` cost is paid before the first login
/// probe hits it — otherwise a cold process has a one-shot timing oracle on
/// its first failed-username attempt (bcrypt::hash + bcrypt::verify ≈ 100ms
/// vs the ~50ms of every subsequent attempt).
pub fn warm_timing_equalizer() {
    let _ = &*DUMMY_BCRYPT_HASH;
}

/// Check if any users exist (for first-run setup detection).
pub async fn has_users(db: &SqlitePool) -> Result<bool, sqlx::Error> {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await?;
    Ok(count.0 > 0)
}

/// bcrypt-hash `password` at cost 10.
async fn hash_password(password: &str) -> Result<String, String> {
    // bcrypt::hash burns ~50ms of CPU on a runtime worker. Move it to a
    // blocking thread so a setup POST under any concurrent async load
    // doesn't stall every other task on the same worker.
    let password_owned = password.to_string();
    tokio::task::spawn_blocking(move || bcrypt::hash(&password_owned, 10))
        .await
        .map_err(|e| format!("Hash spawn_blocking failed: {}", e))?
        .map_err(|e| format!("Hash error: {}", e))
}

/// Create a new user with a bcrypt-hashed password.
pub async fn create_user(db: &SqlitePool, username: &str, password: &str) -> Result<i64, String> {
    let hash = hash_password(password).await?;

    let result = sqlx::query("INSERT INTO users (username, password_hash) VALUES (?, ?)")
        .bind(username)
        .bind(&hash)
        .execute(db)
        .await
        .map_err(|e| format!("DB error: {}", e))?;

    Ok(result.last_insert_rowid())
}

/// Create the first-run admin account, but only while the users table
/// is empty. Returns `Ok(None)` when an account already exists.
///
/// The emptiness check and the insert are one statement, so two
/// concurrent `/setup` submissions can't both create an account. A
/// `has_users()` check before a plain insert can't promise that: both
/// requests pass the check during the ~50ms bcrypt hash, then both
/// insert, and `UNIQUE(username)` only stops them when the usernames
/// match.
pub async fn create_first_user(
    db: &SqlitePool,
    username: &str,
    password: &str,
) -> Result<Option<i64>, String> {
    let hash = hash_password(password).await?;

    let result = sqlx::query(
        "INSERT INTO users (username, password_hash) \
         SELECT ?, ? WHERE NOT EXISTS (SELECT 1 FROM users)",
    )
    .bind(username)
    .bind(&hash)
    .execute(db)
    .await
    .map_err(|e| format!("DB error: {}", e))?;

    if result.rows_affected() == 0 {
        return Ok(None);
    }
    Ok(Some(result.last_insert_rowid()))
}

/// Wipe all user and session rows. Used by the password-recovery boot
/// path (`#22`) — when `RYOKAN_RESET_AUTH=1` is set alongside a
/// `data/.reset-auth` sentinel file, `main()` calls this before the
/// router mounts so `has_users()` reports false and `/setup` re-renders.
///
/// Deliberately does NOT touch the `config` table — Jellyfin / qBit
/// credentials and other settings survive a password reset. The user's
/// recovery recipe is "reset auth, re-create admin, log back in" — not
/// "factory reset the whole install."
pub async fn reset_all(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions").execute(db).await?;
    sqlx::query("DELETE FROM users").execute(db).await?;
    Ok(())
}

/// Verify credentials and return the user if valid.
///
/// Timing note: when the username does not exist, this still runs a
/// `bcrypt::verify` against a dummy hash so the failure path takes the
/// same wall time as a real "wrong password" failure. Without this,
/// an attacker can enumerate valid usernames in a timing side channel
/// (the missing-user branch would short-circuit ~50ms faster than the
/// bcrypt-verify branch).
pub async fn verify_user(
    db: &SqlitePool,
    username: &str,
    password: &str,
) -> Result<Option<User>, String> {
    let row: Option<(i64, String, String)> =
        sqlx::query_as("SELECT id, username, password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(db)
            .await
            .map_err(|e| format!("DB error: {}", e))?;

    // bcrypt::verify is ~50ms of CPU work; running it on a runtime
    // worker stalls every other async task on that worker. Wrap both
    // branches in spawn_blocking so concurrent login attempts don't
    // serialise behind each other on a single worker thread. The
    // timing-equalisation invariant (the no-such-user branch must
    // take the same wall time as the wrong-password branch) is
    // preserved — both branches still go through one bcrypt::verify
    // and one spawn_blocking trip.
    let password_owned = password.to_string();
    match row {
        Some((id, uname, hash)) => {
            let hash_for_verify = hash.clone();
            let valid = tokio::task::spawn_blocking(move || {
                bcrypt::verify(&password_owned, &hash_for_verify).unwrap_or(false)
            })
            .await
            .map_err(|e| format!("verify spawn_blocking failed: {}", e))?;
            if valid {
                Ok(Some(User {
                    id,
                    username: uname,
                    password_hash: hash,
                }))
            } else {
                Ok(None)
            }
        }
        None => {
            // Burn equivalent CPU time against the dummy hash and
            // discard the result. DUMMY_BCRYPT_HASH is a
            // `static LazyLock<String>` so the closure can reference it
            // without capturing. Propagate JoinError with `?` to match
            // the Some branch — without parity, an extremely-rare
            // spawn_blocking panic would distinguish the two branches
            // by both wall time AND result shape, defeating the
            // username-enumeration timing equaliser the function
            // exists to provide.
            let _ = tokio::task::spawn_blocking(move || {
                bcrypt::verify(&password_owned, &DUMMY_BCRYPT_HASH).unwrap_or(false)
            })
            .await
            .map_err(|e| format!("verify spawn_blocking failed: {}", e))?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #22: reset_all must wipe both users and sessions, so `has_users()`
    /// returns false after it runs and the first-run setup page re-renders.
    #[tokio::test]
    async fn reset_all_wipes_users_and_sessions() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        create_user(&db, "admin", "hunter2")
            .await
            .expect("create admin");

        // Seed a session row directly — real sessions are minted by the
        // login handler, but for this test we only need a row present so
        // reset_all has something to delete.
        sqlx::query("INSERT INTO sessions (token, user_id) VALUES ('sid', 1)")
            .execute(&db)
            .await
            .expect("seed session");

        assert!(has_users(&db).await.unwrap(), "user exists before reset");
        let session_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(session_count.0, 1);

        reset_all(&db).await.expect("reset_all");

        assert!(!has_users(&db).await.unwrap(), "users wiped");
        let session_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(session_count.0, 0, "sessions wiped");
    }

    /// `create_first_user` inserts into an empty table and refuses once
    /// any account exists, whatever the username.
    #[tokio::test]
    async fn create_first_user_only_inserts_into_empty_table() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        let first = create_first_user(&db, "admin", "hunter2")
            .await
            .expect("first account");
        assert!(first.is_some(), "empty table accepts the first account");

        let second = create_first_user(&db, "someone-else", "hunter2")
            .await
            .expect("second attempt is not a DB error");
        assert_eq!(second, None, "an existing account blocks the second");

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
    }
}
