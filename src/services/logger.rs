use sqlx::SqlitePool;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::models::log::{self, LogCategory, LogLevel};

/// Minimum severity that gets written to the `logs` table. `tracing`
/// (console) is separately filtered by `RUST_LOG` and is not affected
/// by this gate.
///
/// Held as a process-wide `AtomicU8` holding a `LogLevel::severity()`
/// value so the logger hot path can check it without a DB round trip
/// or mutex lock. Default is Info (severity 2) so fresh installs get
/// the same behavior they had before #3b shipped.
static MIN_DB_LOG_LEVEL: AtomicU8 = AtomicU8::new(2);

/// Update the write-side floor. Called from `main.rs` at startup
/// after reading config, and from the Settings save handler whenever
/// the user changes the dropdown.
pub fn set_min_db_log_level(level: LogLevel) {
    MIN_DB_LOG_LEVEL.store(level.severity(), Ordering::Relaxed);
}

/// Application logger that writes to both SQLite and tracing.
///
/// Usage:
///   logger::info(&db, LogCategory::Nyaa, "Search completed", "Found 42 results for 'Dandadan'").await;
///   logger::error(&db, LogCategory::QBit, "Connection failed", &err.to_string()).await;
pub async fn log(
    db: &SqlitePool,
    level: LogLevel,
    category: LogCategory,
    message: &str,
    detail: &str,
) {
    // Write to tracing (console/container log).
    match level {
        LogLevel::Trace => {
            tracing::trace!(category = category.as_str(), detail = detail, "{}", message)
        }
        LogLevel::Debug => {
            tracing::debug!(category = category.as_str(), detail = detail, "{}", message)
        }
        LogLevel::Info => {
            tracing::info!(category = category.as_str(), detail = detail, "{}", message)
        }
        LogLevel::Warn => {
            tracing::warn!(category = category.as_str(), detail = detail, "{}", message)
        }
        LogLevel::Error => {
            tracing::error!(category = category.as_str(), detail = detail, "{}", message)
        }
    }

    // Write to SQLite. Skip rows below the configured floor so the
    // System → Logs table stays concise when the user raises the
    // min level. Don't propagate errors — logging should never crash
    // the app.
    if level.severity() < MIN_DB_LOG_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    if let Err(e) = log::insert(db, level, category, message, detail).await {
        tracing::error!("Failed to write log to database: {}", e);
    }
}

#[allow(dead_code)]
pub async fn trace(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Trace, category, message, detail).await;
}

pub async fn debug(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Debug, category, message, detail).await;
}

pub async fn info(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Info, category, message, detail).await;
}

pub async fn warn(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Warn, category, message, detail).await;
}

pub async fn error(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Error, category, message, detail).await;
}
