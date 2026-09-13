//! Merge engine: per-entry library writes for both the AL-detail and
//! the Jikan-fallback paths, plus the small `stamp_*` helpers that
//! keep the per-account cross-cutting state (synced_from FK, user
//! score, custom-list memberships) in sync as a side effect of every
//! merge.
//!
//! This module is deliberately the busy one — the merge step is where
//! the sync engine touches the most schema and where every "what
//! happens when X already exists?" decision is made. Each branch in
//! `merge_one_anilist_entry` / `merge_one_jikan_entry` is a load-
//! bearing invariant; the test suite under `tests/merge.rs` pins
//! roughly one assertion per branch.

use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::models::external_accounts::{self, ImportPreferences};
use crate::models::monitoring::MonitorMode;
use crate::models::series;
use crate::models::{metadata_cache, series_custom_lists, series_genres};
use crate::services::{anilist, jikan, monitoring as monitoring_service};

use super::normalize::{import_status, monitor_mode_for};
use super::types::{MergeAction, MergeOutcome, NewArtworkSpec, SyncEntry};

/// Merge a batch of [`SyncEntry`] into the local `series` table.
///
/// Caller is responsible for fetching `detail_map` (typically via
/// `anilist::get_anime_details_batch`) for every NEW positive AL id
/// in `entries`. Existing series don't need a detail entry — the
/// merge only updates `monitor_mode`, leaving cached metadata alone.
///
/// Decision flow per entry:
///   1. anilist_id <= 0  → deferred_jikan += 1, skip (Jikan path TBD).
///   2. series exists and monitor_mode == target → unchanged.
///   3. series exists and monitor_mode != target → apply_monitor_mode,
///      monitor_mode_updated += 1.
///   4. series doesn't exist + detail_map has it → upsert with full
///      core, then apply target monitor_mode. created += 1.
///   5. series doesn't exist + no detail in map → failed entry.
///
/// `apply_monitor_mode` runs `recompute_series_monitoring` as a side
/// effect, so monitoring rows get rebuilt for both new and changed
/// entries. The metadata-cache hydration + per-series classify scan
/// that the interactive add path triggers are intentionally NOT
/// triggered here — bulk-mode coalescing in a follow-up commit will
/// batch them so a 200-series first sync doesn't fan out 200 spawned
/// background tasks.
pub async fn merge_into_library(
    db: &SqlitePool,
    entries: &[SyncEntry],
    detail_map: &HashMap<i64, anilist::AnimeDetail>,
    prefs: &ImportPreferences,
    account_id: Option<i64>,
) -> MergeOutcome {
    let mut outcome = MergeOutcome::default();
    // Loaded once per sync: a removed series the user wants kept off
    // the list is skipped before any lookup or add (Sonarr's
    // "Rejected due to list exclusion").
    let exclusions = crate::models::sync_exclusions::load_set(db).await;

    for entry in entries {
        if entry.anilist_id <= 0 {
            outcome.deferred_jikan += 1;
            continue;
        }
        // An exclusion keeps a removed series out; one the user added
        // back by hand keeps syncing (add_series clears the row too).
        if exclusions.contains(entry.anilist_id, None)
            && !matches!(
                series::get_by_anilist_id(db, entry.anilist_id).await,
                Ok(Some(_))
            )
        {
            outcome.excluded += 1;
            continue;
        }
        let target_mode = monitor_mode_for(entry.status, prefs.skip_already_watched);
        match merge_one_anilist_entry(db, entry, target_mode, detail_map, prefs, account_id).await {
            Ok(MergeAction::Created(spec)) => {
                outcome.created += 1;
                outcome.new_artwork.push(spec);
            }
            Ok(MergeAction::MonitorUpdated) => outcome.monitor_mode_updated += 1,
            Ok(MergeAction::Unchanged) => outcome.unchanged += 1,
            Ok(MergeAction::SkippedByPreference) => outcome.skipped_by_preference += 1,
            Ok(MergeAction::PinnedManually) => outcome.pinned_manually += 1,
            Err(msg) => outcome.failed.push((entry.anilist_id, msg)),
        }
    }
    outcome
}

/// Stamp `series.synced_from_external_account_id` if the caller
/// passed an `account_id` (live sync) and skip silently when `None`
/// (unit tests and theoretical batch-merge paths that don't have a
/// real account). Best-effort write — a failure is logged but does
/// not fail the merge, since the marker is only used by the removal-
/// detection pass and missing it just means the series stays out of
/// removal candidates (safer than the alternative).
async fn stamp_synced_from_if_set(db: &SqlitePool, series_id: i64, account_id: Option<i64>) {
    if let Some(aid) = account_id
        && let Err(e) = series::stamp_synced_from(db, series_id, aid).await
    {
        tracing::warn!("series::stamp_synced_from failed for series_id={series_id}: {e}");
    }
}

/// #62 — write the user's personal score from the sync entry
/// onto `series.user_score`. Skips silently when `account_id` is
/// `None` (unit-test pathway with no live account). Normalizes AL's
/// `0.0` "unrated" sentinel to `NULL` so the schema unambiguously
/// means "rated" when the column is non-null; the render helper
/// still handles 0.0 defensively for any rows that pre-date the
/// normalization.
///
/// Best-effort write, same rationale as `stamp_synced_from_if_set`:
/// a failure logs but doesn't fail the merge. A missing score just
/// means the "You: X" badge won't render until the next tick.
async fn stamp_user_score_if_set(
    db: &SqlitePool,
    series_id: i64,
    score: f64,
    account_id: Option<i64>,
) {
    if account_id.is_none() {
        return;
    }
    let normalized = if score > 0.0 { Some(score) } else { None };
    if let Err(e) = series::update_user_score(db, series_id, normalized).await {
        tracing::warn!("series::update_user_score failed for series_id={series_id}: {e}");
    }
}

/// #62 — replace the series's AL custom-list memberships from
/// `entry.custom_lists`. Skips when `account_id` is `None` (unit-
/// test pathway) and when `provider` isn't AniList (MAL never emits
/// custom-list memberships, so the call would just clear a never-
/// populated set every tick).
///
/// Called from BOTH the AL-detail and Jikan-fallback merge paths.
/// The Jikan path is dead-by-data today — `entries_from_mal` always
/// returns an empty `custom_lists` and the provider check short-
/// circuits before any DB write — but keeping the call symmetric
/// across both paths means a hypothetical future provider added to
/// the Jikan-fallback path inherits the namespace-skip automatically
/// instead of silently clobbering AL's rows. Two cheap branch-and-
/// returns per Jikan merge is a fine tax for that invariant.
///
/// Replace-on-merge rather than incremental: the GraphQL response
/// carries the full membership map per entry, so clear+insert is
/// the right shape for "user moved this out of Hidden Gems" — an
/// upsert path would leak stale rows.
///
/// Best-effort: a failure logs but doesn't fail the merge. A
/// missing membership row just means the badge / filter doesn't
/// reflect the latest state until the next tick.
async fn stamp_custom_lists_if_set(
    db: &SqlitePool,
    series_id: i64,
    provider: &str,
    custom_lists: &[String],
    account_id: Option<i64>,
) {
    if account_id.is_none() {
        return;
    }
    if provider != external_accounts::PROVIDER_ANILIST {
        return;
    }
    if let Err(e) =
        series_custom_lists::replace_for_series(db, series_id, provider, custom_lists).await
    {
        tracing::warn!(
            "series_custom_lists::replace_for_series failed for series_id={series_id}: {e}"
        );
    }
}

/// Walk negated-id [`SyncEntry`]s (the ones `merge_into_library`
/// counted as `deferred_jikan`) and merge each via Jikan-fetched
/// metadata. Used by the MAL sync path so entries whose anibridge
/// MAL→AL lookup missed still land in the library — they sit under
/// the `series.anilist_id = -mal_id` sentinel that the existing
/// reconcile-fallbacks flow already understands.
///
/// Walks one entry at a time rather than fanning out: Jikan is rate-
/// limited at 3 req/s, and `get_anime_detail_cached` carries its own
/// negative-cache + rate-limit state. A failure for any single entry
/// records into `failed` and does not abort the loop, so one
/// upstream-deleted MAL id doesn't block the others from importing.
pub async fn merge_jikan_fallback_entries(
    db: &SqlitePool,
    entries: &[SyncEntry],
    prefs: &ImportPreferences,
    account_id: Option<i64>,
) -> MergeOutcome {
    let mut outcome = MergeOutcome::default();
    // Loaded once per sync: a removed series the user wants kept off
    // the list is skipped before any lookup or add (Sonarr's
    // "Rejected due to list exclusion").
    let exclusions = crate::models::sync_exclusions::load_set(db).await;
    for entry in entries.iter().filter(|e| e.anilist_id < 0) {
        // Recover the original MAL id by negating the sentinel back.
        // `provider_media_id` carries the same value but going through
        // the sentinel keeps the AL-merge path and Jikan-merge path
        // consistent: each derives the upstream id from `anilist_id`.
        let mal_id = -entry.anilist_id;
        if exclusions.contains(entry.anilist_id, Some(mal_id))
            && !matches!(series::get_by_mal_id(db, mal_id).await, Ok(Some(_)))
        {
            outcome.excluded += 1;
            continue;
        }
        let target_mode = monitor_mode_for(entry.status, prefs.skip_already_watched);
        match merge_one_jikan_entry(db, entry, mal_id, target_mode, prefs, account_id).await {
            Ok(MergeAction::Created(spec)) => {
                outcome.created += 1;
                outcome.new_artwork.push(spec);
            }
            Ok(MergeAction::MonitorUpdated) => outcome.monitor_mode_updated += 1,
            Ok(MergeAction::Unchanged) => outcome.unchanged += 1,
            Ok(MergeAction::SkippedByPreference) => outcome.skipped_by_preference += 1,
            Ok(MergeAction::PinnedManually) => outcome.pinned_manually += 1,
            Err(msg) => outcome.failed.push((entry.anilist_id, msg)),
        }
    }
    outcome
}

async fn merge_one_jikan_entry(
    db: &SqlitePool,
    entry: &SyncEntry,
    mal_id: i64,
    target_mode: MonitorMode,
    prefs: &ImportPreferences,
    account_id: Option<i64>,
) -> Result<MergeAction, String> {
    // Two lookup paths because the row may already exist under either
    // identity. anilist_id (negated sentinel) is canonical for sync-
    // sourced rows; mal_id covers the case where a previous
    // reconcile-fallbacks run promoted the row to a real AL id (and
    // the negated sentinel no longer matches).
    let existing = match series::get_by_anilist_id(db, entry.anilist_id).await {
        Ok(Some(row)) => Some(row),
        Ok(None) => series::get_by_mal_id(db, mal_id)
            .await
            .map_err(|e| format!("series mal lookup failed: {e}"))?,
        Err(e) => return Err(format!("series anilist lookup failed: {e}")),
    };

    if let Some(row) = existing {
        // Existing series → always update monitor_mode regardless
        // of import_status preference. A status transition on AL
        // (Watching → Dropped) must downgrade local monitor_mode
        // even when the new status's import flag is off.
        stamp_synced_from_if_set(db, row.id, account_id).await;
        stamp_user_score_if_set(db, row.id, entry.score, account_id).await;
        stamp_custom_lists_if_set(db, row.id, &entry.provider, &entry.custom_lists, account_id)
            .await;
        // Manual override takes precedence: the user has explicitly
        // pinned this series's monitor_mode through the UI. Sync
        // honors that pin until the user clears it via "Sync from
        // AL/MAL" in the dropdown.
        if row.monitor_mode_manual_override {
            return Ok(MergeAction::PinnedManually);
        }
        if row.monitor_mode == target_mode.as_str() {
            return Ok(MergeAction::Unchanged);
        }
        monitoring_service::apply_monitor_mode(db, row.id, target_mode).await?;
        return Ok(MergeAction::MonitorUpdated);
    }

    // New series → only create if the user wants this status imported.
    if !import_status(entry.status, prefs) {
        return Ok(MergeAction::SkippedByPreference);
    }

    // Fetch metadata from Jikan (cached). The cached helper handles
    // the 15-minute TTL + rate-limit pacing internally; we just call
    // it and trust its output.
    let detail = jikan::get_anime_detail_cached(mal_id)
        .await
        .map_err(|e| format!("Jikan detail fetch failed for mal_id {mal_id}: {e}"))?;

    let primary_title = if !detail.title_english.trim().is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    let (series_id, _created) = series::upsert(
        db,
        series::SeriesCore {
            // Preserve the negated sentinel so the existing > 0 filters
            // throughout the AL call sites continue to skip this row,
            // matching how Jikan-fallback entries already behave when
            // added through the interactive search flow.
            anilist_id: entry.anilist_id,
            mal_id: detail.id_mal.or(Some(mal_id)),
            title: primary_title,
            title_romaji: &detail.title_romaji,
            title_english: &detail.title_english,
            title_native: &detail.title_native,
            cover_url: &detail.cover_url,
            format: &detail.format,
            status: &detail.status,
            episodes: detail.episodes,
            season_year: detail.season_year,
            end_year: detail.end_year,
        },
    )
    .await
    .map_err(|e| format!("series upsert failed: {e}"))?;

    // Same metadata_cache write as the AL path so a Jikan-fallback
    // entry's UI looks the same as an AL one (description, relations
    // — Jikan supplies most of them via /anime/{id}/full).
    if let Err(e) = metadata_cache::upsert(
        db,
        series_id,
        entry.anilist_id,
        detail.id_mal.or(Some(mal_id)),
        &detail,
    )
    .await
    {
        tracing::warn!(
            "metadata_cache::upsert failed for series_id={series_id} during Jikan sync: {e}"
        );
    }

    // #62 — populate genre side table from Jikan-supplied genres.
    if let Err(e) = series_genres::replace_for_series(db, series_id, &detail.genres).await {
        tracing::warn!(
            "series_genres::replace_for_series failed for series_id={series_id} during Jikan sync: {e}"
        );
    }

    stamp_synced_from_if_set(db, series_id, account_id).await;
    stamp_user_score_if_set(db, series_id, entry.score, account_id).await;
    stamp_custom_lists_if_set(
        db,
        series_id,
        &entry.provider,
        &entry.custom_lists,
        account_id,
    )
    .await;
    monitoring_service::apply_monitor_mode(db, series_id, target_mode).await?;
    Ok(MergeAction::Created(NewArtworkSpec {
        series_id,
        cover_url: detail.cover_url.clone(),
        banner_url: detail.banner_url.clone(),
    }))
}

async fn merge_one_anilist_entry(
    db: &SqlitePool,
    entry: &SyncEntry,
    target_mode: MonitorMode,
    detail_map: &HashMap<i64, anilist::AnimeDetail>,
    prefs: &ImportPreferences,
    account_id: Option<i64>,
) -> Result<MergeAction, String> {
    let existing = series::get_by_anilist_id(db, entry.anilist_id)
        .await
        .map_err(|e| format!("series lookup failed: {e}"))?;

    if let Some(row) = existing {
        // Existing series → always update monitor_mode regardless of
        // import_status preference. A status transition on AL
        // (Watching → Dropped) must downgrade local monitor_mode
        // even when `import_dropped = false`, otherwise the series
        // silently keeps grabbing for a show the user dropped.
        stamp_synced_from_if_set(db, row.id, account_id).await;
        stamp_user_score_if_set(db, row.id, entry.score, account_id).await;
        stamp_custom_lists_if_set(db, row.id, &entry.provider, &entry.custom_lists, account_id)
            .await;
        // Manual override takes precedence: the user pinned this
        // series's monitor_mode through the UI. Sync honors the pin
        // until the user clears it via "Sync from AL/MAL".
        if row.monitor_mode_manual_override {
            return Ok(MergeAction::PinnedManually);
        }
        if row.monitor_mode == target_mode.as_str() {
            return Ok(MergeAction::Unchanged);
        }
        monitoring_service::apply_monitor_mode(db, row.id, target_mode).await?;
        return Ok(MergeAction::MonitorUpdated);
    }

    // New series → only create if the user wants this status imported.
    if !import_status(entry.status, prefs) {
        return Ok(MergeAction::SkippedByPreference);
    }

    let detail = detail_map.get(&entry.anilist_id).ok_or_else(|| {
        // Most common cause: AL deleted/merged the entry but the
        // user's list still references it. Surface explicitly so the
        // operator knows it isn't a DB error.
        "no AniList detail returned for this id (deleted upstream?)".to_string()
    })?;

    let primary_title = if !detail.title_english.trim().is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    let (series_id, _created) = series::upsert(
        db,
        series::SeriesCore {
            anilist_id: entry.anilist_id,
            mal_id: detail.id_mal,
            title: primary_title,
            title_romaji: &detail.title_romaji,
            title_english: &detail.title_english,
            title_native: &detail.title_native,
            cover_url: &detail.cover_url,
            format: &detail.format,
            status: &detail.status,
            episodes: detail.episodes,
            season_year: detail.season_year,
            end_year: detail.end_year,
        },
    )
    .await
    .map_err(|e| format!("series upsert failed: {e}"))?;

    // Populate the cached AnimeDetail inline so the UI sees full
    // metadata (description, genres, relations) immediately on next
    // page load — without this the row would render with bare
    // series-table fields until the next 12h metadata_refresh sweep.
    // Best-effort: a failure logs but doesn't fail the merge.
    if let Err(e) =
        metadata_cache::upsert(db, series_id, entry.anilist_id, detail.id_mal, detail).await
    {
        tracing::warn!("metadata_cache::upsert failed for series_id={series_id} during sync: {e}");
    }

    // #62 — populate genre side table from AL-supplied genres.
    if let Err(e) = series_genres::replace_for_series(db, series_id, &detail.genres).await {
        tracing::warn!(
            "series_genres::replace_for_series failed for series_id={series_id} during AL sync: {e}"
        );
    }

    stamp_synced_from_if_set(db, series_id, account_id).await;
    stamp_user_score_if_set(db, series_id, entry.score, account_id).await;
    stamp_custom_lists_if_set(
        db,
        series_id,
        &entry.provider,
        &entry.custom_lists,
        account_id,
    )
    .await;
    monitoring_service::apply_monitor_mode(db, series_id, target_mode).await?;
    Ok(MergeAction::Created(NewArtworkSpec {
        series_id,
        cover_url: detail.cover_url.clone(),
        banner_url: detail.banner_url.clone(),
    }))
}
