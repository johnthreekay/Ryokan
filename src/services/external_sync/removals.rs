//! Removal detection: walk the local sync-marked series and downgrade
//! any whose AL/MAL id isn't in the latest fetch to `monitor_mode = None`.
//! Only runs on full-resync passes — delta runs by definition only see
//! changed entries, so a series that didn't change wouldn't appear in
//! `fetch_ids` and would be wrongly flagged as removed.

use sqlx::SqlitePool;

use crate::models::monitoring::MonitorMode;
use crate::models::series;
use crate::services::monitoring as monitoring_service;

use super::types::RemovalReport;

/// Find sync-marked series that aren't in the current fetch and
/// downgrade their `monitor_mode` to `None`. Run AFTER the merge
/// passes (so the merge's own monitor_mode writes don't fight us)
/// and ONLY on full-resync runs (delta runs by definition only see
/// changed entries, so a series that didn't change wouldn't appear
/// in `fetch_ids` and would be wrongly flagged as removed).
///
/// `fetch_ids` is the set of `anilist_id` values from the current
/// sync's entries — positive AL ids for AL syncs, mix of positive
/// (anibridge-resolved) and negated (Jikan-fallback sentinel) for
/// MAL syncs. The same value the merge wrote to `series.anilist_id`,
/// so the comparison is straightforward.
///
/// `fetch_mal_ids` is the MAL ids of a MAL list's entries (empty for
/// an AniList sync). A row of one of them stays: the merge applies a
/// MAL entry to every library row of it, and AniList can split a MAL
/// entry into several rows (#292) while the entry resolves to only one
/// AniList id.
///
/// Series whose `monitor_mode` is already `None` are left alone —
/// no point burning a write to set the same value, and the user
/// might have manually downgraded for their own reasons.
pub async fn detect_removals(
    db: &SqlitePool,
    account_id: i64,
    fetch_ids: &std::collections::HashSet<i64>,
    fetch_mal_ids: &std::collections::HashSet<i64>,
) -> Result<RemovalReport, String> {
    let synced = series::list_synced_from(db, account_id)
        .await
        .map_err(|e| format!("list_synced_from: {e}"))?;
    let mut report = RemovalReport::default();
    let already_none = MonitorMode::None.as_str();
    for s in synced {
        if fetch_ids.contains(&s.anilist_id) || s.mal_id.is_some_and(|m| fetch_mal_ids.contains(&m))
        {
            continue;
        }
        if s.monitor_mode == already_none {
            continue;
        }
        // Manual override pins the user's chosen monitor_mode against
        // both merge updates AND removal detection. The user
        // explicitly set this mode and may want to keep grabbing the
        // series (e.g. they took it off AL because their list was
        // public and contained spoilers, but still want Ryokan to
        // pick up new episodes).
        if s.monitor_mode_manual_override {
            continue;
        }
        monitoring_service::apply_monitor_mode(db, s.id, MonitorMode::None).await?;
        report.removed.push(s.id);
    }
    Ok(report)
}
