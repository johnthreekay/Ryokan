use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorMode {
    All,
    Future,
    Missing,
    Existing,
    None,
}

impl MonitorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MonitorMode::All => "all",
            MonitorMode::Future => "future",
            MonitorMode::Missing => "missing",
            MonitorMode::Existing => "existing",
            MonitorMode::None => "none",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => MonitorMode::All,
            "missing" => MonitorMode::Missing,
            "existing" => MonitorMode::Existing,
            "none" => MonitorMode::None,
            _ => MonitorMode::Future,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MonitorMode::All => "All Episodes",
            MonitorMode::Future => "Future Episodes",
            MonitorMode::Missing => "Missing Episodes",
            MonitorMode::Existing => "Existing Episodes",
            MonitorMode::None => "None",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EpisodeMonitorState {
    pub episode_number: i32,
    pub monitored: bool,
}

pub async fn replace_series_states(
    db: &SqlitePool,
    series_id: i64,
    states: &[EpisodeMonitorState],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM episode_monitor_state WHERE series_id = ?")
        .bind(series_id)
        .execute(&mut *tx)
        .await?;

    for state in states {
        sqlx::query(
            "INSERT INTO episode_monitor_state (series_id, episode_number, monitored) VALUES (?, ?, ?)",
        )
        .bind(series_id)
        .bind(state.episode_number)
        .bind(if state.monitored { 1 } else { 0 })
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

pub async fn get_series_states(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Vec<EpisodeMonitorState>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT episode_number, monitored FROM episode_monitor_state WHERE series_id = ? ORDER BY episode_number ASC",
    )
    .bind(series_id)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| EpisodeMonitorState {
            episode_number: row.get("episode_number"),
            monitored: row.get::<i64, _>("monitored") != 0,
        })
        .collect())
}

pub async fn set_episode_monitored(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    monitored: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO episode_monitor_state (series_id, episode_number, monitored)
           VALUES (?, ?, ?)
           ON CONFLICT(series_id, episode_number) DO UPDATE SET monitored = excluded.monitored"#,
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(if monitored { 1i64 } else { 0i64 })
    .execute(db)
    .await?;
    Ok(())
}

/// Every series' monitored episode numbers in one query, for the
/// Wanted page's library-wide missing list.
pub async fn get_monitored_all_series(
    db: &SqlitePool,
) -> Result<std::collections::HashMap<i64, std::collections::HashSet<i32>>, sqlx::Error> {
    let rows: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT series_id, episode_number FROM episode_monitor_state WHERE monitored = 1",
    )
    .fetch_all(db)
    .await?;
    let mut out: std::collections::HashMap<i64, std::collections::HashSet<i32>> =
        std::collections::HashMap::new();
    for (sid, ep) in rows {
        out.entry(sid).or_default().insert(ep);
    }
    Ok(out)
}

pub async fn get_monitored_episode_numbers(
    db: &SqlitePool,
    series_id: i64,
) -> Result<Vec<i32>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT episode_number FROM episode_monitor_state WHERE series_id = ? AND monitored = 1 ORDER BY episode_number ASC",
    )
    .bind(series_id)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| row.get("episode_number"))
        .collect())
}
