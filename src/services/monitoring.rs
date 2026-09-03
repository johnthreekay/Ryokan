use std::collections::{HashMap, HashSet};

use chrono::{NaiveDate, Utc};
use sqlx::SqlitePool;

use crate::{
    models::{
        config, local_metadata, metadata_cache,
        monitoring::{self, EpisodeMonitorState, MonitorMode},
        series,
    },
    services::{jikan, media},
};

#[derive(Debug, Clone)]
pub struct MonitoringSummary {
    pub mode: MonitorMode,
    pub monitored_count: usize,
    pub total_count: usize,
}

pub async fn apply_monitor_mode(
    db: &SqlitePool,
    series_id: i64,
    mode: MonitorMode,
) -> Result<MonitoringSummary, String> {
    series::update_monitor_mode(db, series_id, mode.as_str())
        .await
        .map_err(|e| e.to_string())?;
    recompute_series_monitoring(db, series_id).await
}

pub async fn recompute_series_monitoring(
    db: &SqlitePool,
    series_id: i64,
) -> Result<MonitoringSummary, String> {
    let Some(row) = series::get_by_id(db, series_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Err("Series not found".to_string());
    };

    let mode = row.monitor_mode_enum();
    let total = effective_episode_count(db, &row).await;
    let episode_numbers: Vec<i32> = (1..=total).collect();

    if episode_numbers.is_empty() {
        monitoring::replace_series_states(db, row.id, &[])
            .await
            .map_err(|e| e.to_string())?;
        return Ok(MonitoringSummary {
            mode,
            monitored_count: 0,
            total_count: 0,
        });
    }

    let cfg = config::get_config(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    let disk_files = media::scan_series_folder(&cfg.media_root, &row.folder_name).await;
    let existing_eps: HashSet<i32> = disk_files.iter().map(|f| f.episode_number).collect();
    let episode_info = load_episode_info(db, &row).await;
    let monitored_eps =
        resolve_monitored_episodes(&row, &episode_numbers, &existing_eps, &episode_info, mode);

    let states: Vec<EpisodeMonitorState> = episode_numbers
        .iter()
        .map(|ep| EpisodeMonitorState {
            episode_number: *ep,
            monitored: monitored_eps.contains(ep),
        })
        .collect();

    monitoring::replace_series_states(db, row.id, &states)
        .await
        .map_err(|e| e.to_string())?;

    Ok(MonitoringSummary {
        mode,
        monitored_count: monitored_eps.len(),
        total_count: episode_numbers.len(),
    })
}

pub async fn ensure_series_monitoring_rows(
    db: &SqlitePool,
    tracked: &series::Series,
) -> Result<MonitoringSummary, String> {
    let total = effective_episode_count(db, tracked).await;
    let episode_numbers: Vec<i32> = (1..=total).collect();
    let existing = monitoring::get_series_states(db, tracked.id)
        .await
        .map_err(|e| e.to_string())?;

    // Recompute from scratch whenever the row count diverges from the
    // effective episode count. `recompute_series_monitoring` uses a single
    // transaction to DELETE + INSERT, which avoids the 1157-round-trip cost
    // that a naive insert loop would pay for something like One Piece.
    if existing.len() != episode_numbers.len() {
        return recompute_series_monitoring(db, tracked.id).await;
    }

    let monitored_count = existing.iter().filter(|s| s.monitored).count();
    Ok(MonitoringSummary {
        mode: tracked.monitor_mode_enum(),
        monitored_count,
        total_count: episode_numbers.len(),
    })
}

/// Returns the effective episode count for a tracked series, preferring the
/// AniList-reported `episodes` field and falling back through cached
/// metadata (`next_airing_episode - 1`) and the cached episode map. This
/// matters for currently-airing long-runners like One Piece where AniList
/// reports `episodes: null` — without the fallback, monitoring recomputes
/// against zero episodes and the per-episode Monitor buttons have nothing
/// to toggle.
async fn effective_episode_count(db: &SqlitePool, row: &series::Series) -> i32 {
    if let Some(n) = row.episodes
        && n > 0
    {
        return n;
    }
    if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, row.id).await {
        let n = cached.detail.effective_episode_count();
        if n > 0 {
            return n;
        }
    }
    if let Ok(map) = local_metadata::get_episode_map_for_series(db, row.id).await
        && let Some(max) = map.keys().copied().max()
    {
        return max;
    }
    0
}

/// Aired-date map for monitoring decisions, read from the cached
/// `local_metadata` episode rows only. `resolve_monitored_episodes`
/// uses the `aired` field to split Missing (already aired, not on
/// disk) from Future (not yet aired); the `title` field rides along
/// but isn't used here.
///
/// **Deliberately cache-only — no live Jikan fetch.** `recompute_series_monitoring`
/// runs on the request path for monitor-mode changes, the add-series
/// flow, the bulk-monitor handler, and the series-detail page render.
/// The pre-2026-05-12 version fell through to `jikan::fetch_episode_titles`
/// when the cache was empty, which on a freshly-added series (cache not
/// yet populated by the background `refresh_series_metadata` spawn)
/// did a paginated Jikan walk with a 400ms-per-page sleep inside the
/// handler — multi-second stall before `set_monitoring` could return
/// its `HX-Refresh`, surfacing as "I have to refresh the page to see
/// the new entry." The episode map is populated asynchronously by
/// `refresh_series_metadata` regardless; for the add-series case,
/// `add_series` re-runs `recompute_series_monitoring` once that spawn
/// finishes so the aired-date-aware modes catch up off the request
/// path. When the cache is empty here, `resolve_monitored_episodes`
/// degrades to its disk-files + `is_finished` heuristics, which are
/// already correct for finished series and self-heal for airing ones
/// on the next recompute.
async fn load_episode_info(
    db: &SqlitePool,
    row: &series::Series,
) -> HashMap<i32, jikan::EpisodeInfo> {
    match local_metadata::get_episode_map_for_series(db, row.id).await {
        Ok(cached) => cached
            .into_iter()
            .map(|(num, ep)| {
                (
                    num,
                    jikan::EpisodeInfo {
                        title: ep.title,
                        aired: ep.aired,
                    },
                )
            })
            .collect(),
        Err(_) => HashMap::new(),
    }
}

fn resolve_monitored_episodes(
    row: &series::Series,
    episode_numbers: &[i32],
    existing_eps: &HashSet<i32>,
    episode_info: &HashMap<i32, jikan::EpisodeInfo>,
    mode: MonitorMode,
) -> HashSet<i32> {
    let today = Utc::now().date_naive();
    let mut latest_aired_known = 0;

    for ep in episode_numbers {
        if let Some(info) = episode_info.get(ep)
            && let Some(aired) = parse_aired_date(&info.aired)
            && aired <= today
        {
            latest_aired_known = latest_aired_known.max(*ep);
        }
    }

    let max_existing = existing_eps.iter().copied().max().unwrap_or(0);
    let is_finished = matches!(
        row.status.trim().to_ascii_uppercase().as_str(),
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED"
    );

    episode_numbers
        .iter()
        .copied()
        .filter(|ep| match mode {
            MonitorMode::All => true,
            MonitorMode::None => false,
            MonitorMode::Existing => existing_eps.contains(ep),
            MonitorMode::Missing => {
                if existing_eps.contains(ep) {
                    return false;
                }
                if is_finished {
                    return true;
                }
                if let Some(info) = episode_info.get(ep)
                    && let Some(aired) = parse_aired_date(&info.aired)
                {
                    return aired <= today;
                }
                *ep <= latest_aired_known && latest_aired_known > 0
            }
            MonitorMode::Future => {
                if existing_eps.contains(ep) {
                    return false;
                }
                // A finished series has, by definition, nothing in the
                // future — every episode has already aired. Without
                // this short-circuit, a finished series with no aired-
                // date metadata cached (a freshly-added show before
                // its first metadata refresh) hits the
                // `*ep > max_existing` fallback below: with no on-disk
                // files, that's `ep > 0` for every episode →
                // monitor everything, exactly the wrong answer. The
                // aired-date branch above catches the case where
                // metadata IS present (every aired date is in the
                // past, so `aired > today` is false everywhere); the
                // explicit `is_finished` gate covers the no-metadata
                // case symmetrically.
                if is_finished {
                    return false;
                }
                if let Some(info) = episode_info.get(ep)
                    && let Some(aired) = parse_aired_date(&info.aired)
                {
                    return aired > today;
                }
                if latest_aired_known > 0 {
                    *ep > latest_aired_known
                } else {
                    *ep > max_existing
                }
            }
        })
        .collect()
}

fn parse_aired_date(value: &str) -> Option<NaiveDate> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let date_part = trimmed.split('T').next().unwrap_or(trimmed);
    NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    //! `resolve_monitored_episodes` drives every "what should I grab?"
    //! decision in Ryokan — five modes (`All`/`None`/`Existing`/
    //! `Missing`/`Future`) crossed with finished/airing status and
    //! whether per-episode aired-date metadata is present. The
    //! function is pure (no async, no DB, no I/O), which makes
    //! comprehensive coverage cheap and makes regressions in the
    //! date-based fallbacks immediately visible.
    //!
    //! The "no episode info" tests are the load-bearing ones — most
    //! tracked series eventually have aired-date metadata cached, but
    //! a freshly-added series before its first metadata refresh has
    //! none, and the fallback math in `Future` mode used to monitor
    //! every episode of a finished series in that state. The fix
    //! lives in the `is_finished` short-circuit at the top of the
    //! Future arm.
    use super::*;
    use chrono::TimeZone;
    use rstest::rstest;

    /// Build a minimal `Series` with the fields `resolve_monitored_episodes`
    /// reads (just `status`). Everything else gets cheap defaults so the
    /// test bodies stay focused on the inputs that matter.
    fn series_with_status(status: &str) -> series::Series {
        series::Series {
            is_adult: false,
            id: 1,
            anilist_id: 1,
            mal_id: None,
            title: String::new(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            format: String::new(),
            status: status.to_string(),
            episodes: Some(12),
            season_year: None,
            end_year: None,
            folder_name: String::new(),
            monitor_mode: "all".to_string(),
            allow_upgrades: true,
            allow_pt_upgrades: false,
            custom_query_tokens: String::new(),
            restrict_to_uploader: String::new(),
            cumulative_prior_episodes: 0,
            monitor_mode_manual_override: false,
            user_score: None,
            added_at: String::new(),
        }
    }

    /// Build episode-info entries with aired-dates relative to "today."
    /// The function under test compares against `Utc::now().date_naive()`,
    /// so we have to use real wall-clock time rather than fixturing —
    /// `chrono::Utc.with_ymd_and_hms` doesn't cleanly stub out the
    /// inner clock. We build offsets that are far enough from today
    /// that timezone drift around a date boundary can't flip them.
    fn aired(days_offset: i64) -> jikan::EpisodeInfo {
        let today = chrono::Utc::now().date_naive();
        let date = today
            .checked_add_signed(chrono::Duration::days(days_offset))
            .unwrap_or_else(|| {
                chrono::Utc
                    .with_ymd_and_hms(2000, 1, 1, 0, 0, 0)
                    .unwrap()
                    .date_naive()
            });
        jikan::EpisodeInfo {
            title: String::new(),
            aired: date.format("%Y-%m-%d").to_string(),
        }
    }

    fn ep_set(eps: &[i32]) -> HashSet<i32> {
        eps.iter().copied().collect()
    }

    // ── parse_aired_date ──────────────────────────────────────────────

    #[rstest]
    #[case("2024-04-25", true)]
    #[case("2024-04-25T13:00:00+00:00", true)] // strips T-suffix
    #[case("", false)]
    #[case("not a date", false)]
    #[case("2024-13-99", false)] // month 13 out of range
    #[case("2024-04-25 13:00", false)] // space-separated datetime — only 'T' is stripped
    fn parse_aired_date_handles_realistic_inputs(#[case] input: &str, #[case] should_parse: bool) {
        let got = parse_aired_date(input);
        assert_eq!(
            got.is_some(),
            should_parse,
            "parse_aired_date({input:?}) → {got:?}"
        );
    }

    // ── MonitorMode::All / None ───────────────────────────────────────

    #[test]
    fn all_mode_monitors_every_episode() {
        let row = series_with_status("RELEASING");
        let eps: Vec<i32> = (1..=5).collect();
        let got = resolve_monitored_episodes(
            &row,
            &eps,
            &HashSet::new(),
            &HashMap::new(),
            MonitorMode::All,
        );
        assert_eq!(got, ep_set(&[1, 2, 3, 4, 5]));
    }

    #[test]
    fn none_mode_monitors_nothing() {
        let row = series_with_status("RELEASING");
        let eps: Vec<i32> = (1..=5).collect();
        // Even with on-disk files, None means none.
        let got = resolve_monitored_episodes(
            &row,
            &eps,
            &ep_set(&[1, 2]),
            &HashMap::new(),
            MonitorMode::None,
        );
        assert!(got.is_empty());
    }

    // ── MonitorMode::Existing ─────────────────────────────────────────

    #[test]
    fn existing_mode_monitors_only_on_disk() {
        let row = series_with_status("FINISHED");
        let eps: Vec<i32> = (1..=10).collect();
        let got = resolve_monitored_episodes(
            &row,
            &eps,
            &ep_set(&[2, 5, 9]),
            &HashMap::new(),
            MonitorMode::Existing,
        );
        assert_eq!(got, ep_set(&[2, 5, 9]));
    }

    #[test]
    fn existing_mode_with_empty_disk_set_returns_empty() {
        let row = series_with_status("RELEASING");
        let eps: Vec<i32> = (1..=5).collect();
        let got = resolve_monitored_episodes(
            &row,
            &eps,
            &HashSet::new(),
            &HashMap::new(),
            MonitorMode::Existing,
        );
        assert!(got.is_empty());
    }

    // ── MonitorMode::Missing ──────────────────────────────────────────

    #[test]
    fn missing_mode_finished_monitors_everything_not_on_disk() {
        let row = series_with_status("FINISHED");
        let eps: Vec<i32> = (1..=10).collect();
        let got = resolve_monitored_episodes(
            &row,
            &eps,
            &ep_set(&[1, 2, 3]),
            &HashMap::new(),
            MonitorMode::Missing,
        );
        assert_eq!(got, ep_set(&[4, 5, 6, 7, 8, 9, 10]));
    }

    #[test]
    fn missing_mode_finished_aliases_count() {
        // FINISHED / FINISHED_AIRING / CANCELLED all hit the same branch.
        for status in ["FINISHED", "finished_airing", "Cancelled"] {
            let row = series_with_status(status);
            let eps = vec![1, 2, 3];
            let got = resolve_monitored_episodes(
                &row,
                &eps,
                &ep_set(&[]),
                &HashMap::new(),
                MonitorMode::Missing,
            );
            assert_eq!(
                got,
                ep_set(&[1, 2, 3]),
                "status {status:?} should be treated as finished",
            );
        }
    }

    #[test]
    fn missing_mode_airing_uses_aired_dates_when_present() {
        // E1-E3 already aired, E4 airs tomorrow, E5 in a week.
        // None on disk. Expect: monitor only the past-aired ones.
        let row = series_with_status("RELEASING");
        let mut info = HashMap::new();
        info.insert(1, aired(-21));
        info.insert(2, aired(-14));
        info.insert(3, aired(-7));
        info.insert(4, aired(1));
        info.insert(5, aired(7));
        let eps: Vec<i32> = (1..=5).collect();
        let got =
            resolve_monitored_episodes(&row, &eps, &HashSet::new(), &info, MonitorMode::Missing);
        assert_eq!(got, ep_set(&[1, 2, 3]));
    }

    #[test]
    fn missing_mode_airing_falls_back_to_latest_aired_known() {
        // E1-E3 have aired-date metadata (in the past), E4-E5 don't.
        // The fallback should monitor up to `latest_aired_known = 3`.
        let row = series_with_status("RELEASING");
        let mut info = HashMap::new();
        info.insert(1, aired(-30));
        info.insert(2, aired(-23));
        info.insert(3, aired(-16));
        // E4, E5 missing from info — date unknown.
        let eps: Vec<i32> = (1..=5).collect();
        let got =
            resolve_monitored_episodes(&row, &eps, &HashSet::new(), &info, MonitorMode::Missing);
        // E1-3 monitored via aired check; E4-5 NOT monitored because
        // `*ep <= latest_aired_known(3)` is false for both.
        assert_eq!(got, ep_set(&[1, 2, 3]));
    }

    #[test]
    fn missing_mode_airing_with_no_info_at_all_returns_empty() {
        // No episode_info, latest_aired_known = 0, no fallback signal
        // for "aired or not." Conservative behavior: monitor nothing
        // until the metadata sync populates aired dates.
        let row = series_with_status("RELEASING");
        let eps: Vec<i32> = (1..=5).collect();
        let got = resolve_monitored_episodes(
            &row,
            &eps,
            &HashSet::new(),
            &HashMap::new(),
            MonitorMode::Missing,
        );
        assert!(got.is_empty());
    }

    // ── MonitorMode::Future ───────────────────────────────────────────

    #[test]
    fn future_mode_airing_monitors_unaired_episodes() {
        // E1-E2 already aired, E3-E5 in the future. None on disk.
        let row = series_with_status("RELEASING");
        let mut info = HashMap::new();
        info.insert(1, aired(-14));
        info.insert(2, aired(-7));
        info.insert(3, aired(1));
        info.insert(4, aired(8));
        info.insert(5, aired(15));
        let eps: Vec<i32> = (1..=5).collect();
        let got =
            resolve_monitored_episodes(&row, &eps, &HashSet::new(), &info, MonitorMode::Future);
        assert_eq!(got, ep_set(&[3, 4, 5]));
    }

    #[test]
    fn future_mode_airing_falls_back_to_latest_aired_known() {
        // E1-3 aired-known (past), E4-E5 unknown. Fallback says
        // "future = anything past the latest known aired episode."
        let row = series_with_status("RELEASING");
        let mut info = HashMap::new();
        info.insert(1, aired(-30));
        info.insert(2, aired(-23));
        info.insert(3, aired(-16));
        let eps: Vec<i32> = (1..=5).collect();
        let got =
            resolve_monitored_episodes(&row, &eps, &HashSet::new(), &info, MonitorMode::Future);
        assert_eq!(got, ep_set(&[4, 5]));
    }

    #[test]
    fn future_mode_finished_with_episode_info_monitors_nothing() {
        // Every episode of a finished series has, by definition,
        // already aired. With aired-date metadata this collapses
        // correctly via the `aired > today` check.
        let row = series_with_status("FINISHED");
        let mut info = HashMap::new();
        for ep in 1..=5 {
            info.insert(ep, aired(-365));
        }
        let eps: Vec<i32> = (1..=5).collect();
        let got =
            resolve_monitored_episodes(&row, &eps, &HashSet::new(), &info, MonitorMode::Future);
        assert!(got.is_empty());
    }

    #[test]
    fn future_mode_finished_without_episode_info_monitors_nothing() {
        // The bug-pinning case. A finished series with no aired-date
        // metadata used to fall through to `*ep > max_existing` (0)
        // — which is `ep > 0` for every episode → monitor everything,
        // exactly the wrong answer for Future mode on a show where
        // nothing is in the future. The `is_finished` short-circuit
        // at the top of the Future arm catches this.
        let row = series_with_status("FINISHED");
        let eps: Vec<i32> = (1..=5).collect();
        let got = resolve_monitored_episodes(
            &row,
            &eps,
            &HashSet::new(),
            &HashMap::new(),
            MonitorMode::Future,
        );
        assert!(
            got.is_empty(),
            "finished series + no episode info + no on-disk files \
             must NOT monitor every episode in Future mode; got {got:?}"
        );
    }

    #[test]
    fn future_mode_existing_episodes_never_monitored() {
        // Even a future-aired episode that's somehow on disk (manual
        // import? grab landed early?) should NOT be monitored — it's
        // already there, no point in re-grabbing.
        let row = series_with_status("RELEASING");
        let mut info = HashMap::new();
        info.insert(1, aired(7)); // future-aired
        let eps = vec![1];
        let got = resolve_monitored_episodes(&row, &eps, &ep_set(&[1]), &info, MonitorMode::Future);
        assert!(got.is_empty());
    }

    // ── Edge cases ────────────────────────────────────────────────────

    #[test]
    fn empty_episode_numbers_returns_empty_for_every_mode() {
        let row = series_with_status("RELEASING");
        for mode in [
            MonitorMode::All,
            MonitorMode::None,
            MonitorMode::Existing,
            MonitorMode::Missing,
            MonitorMode::Future,
        ] {
            let got = resolve_monitored_episodes(&row, &[], &HashSet::new(), &HashMap::new(), mode);
            assert!(got.is_empty(), "mode {mode:?} should return empty set");
        }
    }

    #[test]
    fn malformed_aired_date_treated_as_unknown() {
        // A junk aired string (Jikan occasionally returns "Not yet
        // aired" or similar non-date values) must NOT crash and must
        // NOT count toward latest_aired_known.
        let row = series_with_status("RELEASING");
        let mut info = HashMap::new();
        info.insert(
            1,
            jikan::EpisodeInfo {
                title: String::new(),
                aired: "Not yet aired".into(),
            },
        );
        info.insert(2, aired(-7));
        let eps = vec![1, 2, 3];
        // Missing mode should treat E1 as unknown-aired, E2 as
        // past-aired, E3 as unknown. latest_aired_known = 2, so
        // E3 (unknown) doesn't pass `*ep <= latest_aired_known` since
        // it would (3 ≤ 2 is false).
        let got =
            resolve_monitored_episodes(&row, &eps, &HashSet::new(), &info, MonitorMode::Missing);
        // E1: no parseable date, falls through to fallback. 1 ≤ 2 → monitored.
        // E2: aired ≤ today → monitored.
        // E3: no info, fallback `3 ≤ 2` → not monitored.
        assert_eq!(got, ep_set(&[1, 2]));
    }
}
