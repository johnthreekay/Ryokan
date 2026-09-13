//! Watch-list sync background task (issue #62).
//!
//! Pulls the user's AniList or MyAnimeList watch list into the
//! Ryokan library on a configurable cadence (default 30 minutes,
//! range 15 minutes .. 7 days per plan decision #5). One linked
//! account at a time; the supervised task no-ops when nothing is
//! linked or the linked account's tokens fail to decrypt.
//!
//! ## Sync strategy (decision #4)
//!
//! - **Delta on every tick**: query the provider for entries with
//!   `updatedAt > list_last_synced_at`. Cheap, catches the 99%
//!   common case (status changes, score updates, additions).
//! - **Full resync once a week**: backstop against provider-side
//!   drift (missed `updatedAt` fires, backdated bulk imports,
//!   schema additions that retroactively populate fields).
//! - **First sync** is always full: `list_last_synced_at` is NULL,
//!   so there's no delta cursor to start from. Uses a staging-table-
//!   then-merge transaction so the library never flickers through
//!   a half-imported state.
//!
//! ## Status (this commit)
//!
//! End-to-end import + merge + delta cursor + bulk-mode coalescing
//! for both AniList and MyAnimeList watch lists, with bidirectional
//! status tracking: monitor_mode follows the user's AL/MAL status
//! transitions (Watching ↔ Dropped, etc.) regardless of import
//! preferences for existing series, and full-resync runs detect
//! series that have been removed from the user's list and downgrade
//! their monitor_mode to None. Manually-added series are never touched
//! by removal detection (synced_from_external_account_id IS NULL).
//!
//! AL entries land under their real AL id; MAL entries that anibridge
//! can resolve land under the resolved AL id; MAL entries that
//! anibridge misses fall back to the Jikan-fetched-detail path and
//! land under the `-mal_id` sentinel that the existing
//! reconcile-fallbacks flow knows how to promote later. Newly-imported
//! series get their AnimeDetail cached + their artwork fetched + (if
//! configured) a single Jellyfin refresh, all in one coalesced
//! post-merge background task per tick.
//!
//! ## File layout (post v1.5 split)
//!
//! Production code is split across this `mod.rs` (the supervised-tick
//! orchestrator + AL/MAL fetch wrappers) and topical siblings:
//!
//! - `types` — `NormalizedStatus`, `SyncEntry`, `MergeOutcome`, `NewArtworkSpec`,
//!   `MergeAction`, `RemovalReport`.
//! - `normalize` — `entries_from_*`, `monitor_mode_for`, `import_status`,
//!   `resolve_mal_anilist_ids`.
//! - `merge` — `merge_into_library`, the per-entry AL + Jikan merge bodies,
//!   the `stamp_*` cross-cutting helpers, `merge_jikan_fallback_entries`.
//! - `cursor` — `should_full_resync`, `drop_entries_before_cursor`,
//!   `FULL_RESYNC_INTERVAL_SECS`.
//! - `removals` — `detect_removals`.
//!
//! The public surface is unchanged — `tick_once`, `tick_once_or_busy`,
//! `has_linked_account`, `minutes_since_last_run`, plus the type
//! re-exports below cover everything `main.rs`, `handlers::oauth`, and
//! the e2e test reach for. Topic submodules under `tests/` cover the
//! same cuts.

use std::collections::HashMap;
use std::sync::LazyLock;

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::external_accounts::{self, ImportPreferences};
use crate::models::log::LogCategory;
use crate::services::{anibridge, anilist, artwork, logger, mal};

pub mod cursor;
pub mod merge;
pub mod normalize;
pub mod removals;
pub mod types;

#[cfg(test)]
mod tests;

pub use cursor::{FULL_RESYNC_INTERVAL_SECS, drop_entries_before_cursor, should_full_resync};
pub use merge::{merge_into_library, merge_jikan_fallback_entries};
pub use normalize::{
    entries_from_anilist, entries_from_mal, import_status, monitor_mode_for,
    resolve_mal_anilist_ids,
};
pub use removals::detect_removals;
pub use types::{MergeOutcome, NewArtworkSpec, NormalizedStatus, RemovalReport, SyncEntry};

/// Process-wide lock guarding the watch-list sync. Two callers can
/// race: the supervised cadence loop in `main.rs` and the manual
/// "Sync now" handler. Without serialization they'd produce two
/// concurrent fetches, two merge passes against the same `series`
/// rows (idempotent on data but counters double-count), and two
/// `spawn_post_merge_bulk_pass` artwork loops.
///
/// Mirrors `services::rss::RSS_SYNC_LOCK` but with a split policy:
/// the supervised path **awaits** the lock (a pending manual sync
/// shouldn't push the next supervised tick into the exponential-
/// backoff path), while the manual path **try-locks** and surfaces
/// a "sync already in progress" error so the user gets immediate
/// feedback instead of a silent hang.
pub(crate) static EXTERNAL_SYNC_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// True when a row exists in `external_accounts`. Used by the
/// supervised loop to short-circuit before stamping
/// `scheduled_task_runs` — a 30-minute cadence with no linked
/// account would otherwise churn the table with "no external
/// account linked" rows forever.
pub async fn has_linked_account(db: &SqlitePool) -> bool {
    matches!(external_accounts::get_current(db).await, Ok(Some(_)))
}

/// Run one sync iteration against the linked account. Used by the
/// supervised loop in `main.rs::external_sync`. Awaits
/// [`EXTERNAL_SYNC_LOCK`] — a manual "Sync now" in flight blocks
/// this call until the manual sync completes, instead of letting
/// the supervised tick fail and trigger exponential backoff.
///
/// Returns a one-line summary used by `scheduled_task_runs.detail`.
/// Errors bubble up so the supervised loop's `mark_finished("error",
/// …)` path captures the failure.
pub async fn tick_once(state: &AppState) -> Result<String, String> {
    let _guard = EXTERNAL_SYNC_LOCK.lock().await;
    tick_once_inner(state, false).await
}

/// Manual-trigger variant. Returns a "sync already running" error
/// rather than waiting if the supervised loop or another manual
/// trigger is already mid-tick. The user-facing toast surfaces this
/// error directly so a double-click doesn't silently queue.
///
/// Forces a full resync regardless of `list_full_resync_at`: a user
/// clicking "Sync now" almost always means "I just changed my list,
/// reflect it" — including removals. Without the force flag, removal
/// detection would be skipped until the next 7-day boundary, leaving
/// a removed-from-AL series grabbing for up to a week. The cursor
/// stamps still advance, so the next supervised tick reads as
/// already-synced.
pub async fn tick_once_or_busy(state: &AppState) -> Result<String, String> {
    let _guard = EXTERNAL_SYNC_LOCK
        .try_lock()
        .map_err(|_| "Watch-list sync is already running.".to_string())?;
    tick_once_inner(state, true).await
}

/// True when the sync error string indicates the user's auth token
/// is dead and re-linking is the fix (vs. transient rate-limits,
/// network errors, or upstream 5xx). Matches the stable prefixes
/// the sync engine emits — adding new wordings means updating this
/// list, which is the project's existing string-tag convention for
/// the AL failure taxonomy.
pub(crate) fn is_auth_rejection(err: &str) -> bool {
    const AUTH_PREFIXES: &[&str] = &[
        "AniList rejected the watch-list token",
        "MAL access token expired and no refresh token stored",
        "MAL refresh failed (re-link required)",
        "MAL rejected the token immediately after refresh",
        "re-link required",
    ];
    AUTH_PREFIXES.iter().any(|p| err.contains(p))
}

async fn tick_once_inner(state: &AppState, force_full_sync: bool) -> Result<String, String> {
    let account = external_accounts::get_current(&state.db)
        .await
        .map_err(|e| format!("read external_accounts: {e}"))?;

    let Some(account) = account else {
        return Ok("no external account linked".to_string());
    };

    // Capture the tick's wall-clock at entry, before any network
    // fetch. The cursor we stamp on success is "the moment we started
    // looking" — using the post-fetch time would risk dropping
    // entries the user updated while we were syncing.
    let tick_started_at = current_unix_ts();
    let is_full_sync = force_full_sync
        || should_full_resync(
            account.list_last_synced_at,
            account.list_full_resync_at,
            tick_started_at,
        );
    let delta_cursor = if is_full_sync {
        None
    } else {
        account.list_last_synced_at
    };

    let raw = match account.provider.as_str() {
        external_accounts::PROVIDER_ANILIST => {
            sync_anilist(state, &account, delta_cursor, is_full_sync).await
        }
        external_accounts::PROVIDER_MAL => {
            sync_mal(state, account.clone(), delta_cursor, is_full_sync).await
        }
        other => {
            // Unknown provider string — schema CHECK constraint should
            // prevent this, but surface explicitly rather than panic.
            return Err(format!("unknown external_accounts.provider: {other}"));
        }
    };

    let summary = match raw {
        Ok(s) => s,
        Err(e) => {
            // #62 — auth-rejection detection. The sync engine
            // returns stable error-prefix strings for token-dead
            // failures; a match flips the sticky flag so the
            // Settings UI can render the "Re-link required" banner.
            // Other failure modes (rate-limit, network timeout)
            // leave the flag alone — they're transient.
            if is_auth_rejection(&e) {
                // Capture the prior flag state BEFORE the update so the
                // notification fires only on the false→true transition.
                // Without this gate every subsequent failed tick (every
                // 15 min initially, backing off to ~8h) would re-fire the
                // re-link ping during a long token-dead window — a single
                // unattended weekend could produce 5-10 duplicate Discord
                // pings for the same dead token.
                let was_already_failed = account.last_sync_auth_failed;
                let write_ok = match external_accounts::update_last_sync_auth_failed(
                    &state.db, account.id, true,
                )
                .await
                {
                    Ok(()) => true,
                    Err(write_err) => {
                        tracing::warn!(
                            "failed to set last_sync_auth_failed for account_id={}: {write_err}",
                            account.id
                        );
                        false
                    }
                };
                // Issue #118 — fire the re-link-required notification at
                // the moment the sticky flag flips on for the first time.
                // Default-on event policy (this is something the user
                // genuinely needs to know); Settings UI's "Re-link
                // required" banner already fires every tick, but a
                // Discord ping is what gets a user back into the app to
                // actually click it. One ping per fail→fail-resolved
                // cycle is the right cadence.
                //
                // **Gated on `write_ok`** so a transient DB failure
                // doesn't leak the duplicate-ping problem this guard
                // exists to prevent: if the write failed, the next
                // tick re-reads `last_sync_auth_failed = false` from
                // the DB → `was_already_failed = false` again → re-
                // emits. Skipping the emit on write failure means the
                // user might miss the first ping but won't get spammed.
                if !was_already_failed && write_ok {
                    crate::services::notifications::emit_external_sync_relink_required(
                        state,
                        &account.provider,
                    );
                }
            }
            return Err(e);
        }
    };

    // Only stamp on success — a failed tick must not advance the
    // cursor or the entries it skipped fetching would be lost forever.
    external_accounts::stamp_list_synced(&state.db, account.id, tick_started_at, is_full_sync)
        .await?;
    // Clear any stale auth-failure flag — the sync just succeeded,
    // whatever caused the prior failure resolved (e.g. user
    // re-linked).
    if account.last_sync_auth_failed
        && let Err(e) =
            external_accounts::update_last_sync_auth_failed(&state.db, account.id, false).await
    {
        tracing::warn!(
            "failed to clear last_sync_auth_failed for account_id={}: {e}",
            account.id
        );
    }

    Ok(if is_full_sync {
        format!("{summary} [full-resync]")
    } else {
        format!("{summary} [delta]")
    })
}

/// Fetch the AL watch list and merge entries into the library. AL
/// is the simpler path: every entry's `media_id` is already the AL
/// ID we'd write to `series.anilist_id`, so no anibridge resolution
/// step is needed. Bulk-mode coalescing for the metadata-cache
/// hydration + classify-scan side effects lands in a follow-up
/// commit; for now the merge step does the upsert + monitor_mode
/// write only.
async fn sync_anilist(
    state: &AppState,
    account: &external_accounts::ExternalAccount,
    delta_cursor: Option<i64>,
    is_full_sync: bool,
) -> Result<String, String> {
    let user_id: i64 = account.provider_user_id.parse().map_err(|e| {
        format!(
            "AL provider_user_id is not a valid integer: {} ({e})",
            account.provider_user_id
        )
    })?;

    let fetch = anilist::fetch_media_list_collection(&account.access_token, user_id).await?;
    let raw = fetch.entries;
    let raw_total = raw.len();

    // Refresh the user's score_format on the linked-account row so
    // the "You: X" badge picks up POINT_X changes the user made on
    // AL after their original link. Empty-string responses no-op
    // (defensive — AL's user.mediaListOptions field has been stable
    // for years but a partial response shouldn't blank a known-good
    // value).
    if let Err(e) =
        external_accounts::update_score_format(&state.db, account.id, &fetch.score_format).await
    {
        tracing::warn!(
            "update_score_format failed for account_id={}: {e}",
            account.id
        );
    }

    let prefs = ImportPreferences {
        import_watching: account.import_watching,
        import_planning: account.import_planning,
        import_paused: account.import_paused,
        import_dropped: account.import_dropped,
        import_completed: account.import_completed,
        skip_already_watched: account.skip_already_watched,
    };
    // Convert raw → SyncEntry without filtering: existing-series
    // monitor_mode updates need to flow regardless of whether the
    // user wants this status imported. The merge step gates only the
    // create branch.
    let entries = entries_from_anilist(raw);
    let after_convert = entries.len();

    // Delta filter: drop entries that haven't changed since the last
    // successful tick. On a full-resync run delta_cursor = None, so
    // every entry passes through.
    let entries = drop_entries_before_cursor(entries, delta_cursor);
    let kept = entries.len();
    let stale_dropped = after_convert - kept;

    // Pre-fetch AnimeDetail for the AL ids that don't yet have a
    // series row AND would be created (status passes import prefs).
    // Existing rows skip the fetch — the merge step only touches
    // monitor_mode for those — and not-existing-but-not-importable
    // entries skip too, since the merge will mark them
    // SkippedByPreference without needing the detail.
    let new_ids = ids_needing_detail_fetch(&state.db, &entries, &prefs).await;
    let detail_map = if new_ids.is_empty() {
        HashMap::new()
    } else {
        anilist::get_anime_details_batch(&new_ids)
            .await
            .map_err(|e| format!("AniList detail batch fetch failed: {e}"))?
    };

    let outcome =
        merge_into_library(&state.db, &entries, &detail_map, &prefs, Some(account.id)).await;

    // Removal detection (full-resync only). Delta runs by definition
    // only fetch CHANGED entries, so a series whose updated_at is
    // older than the cursor wouldn't be in `entries` even though
    // it's still on the user's AL list. Running removal on a delta
    // would wrongly downgrade every still-on-list series whose entry
    // didn't change since the last tick. Full-resync includes every
    // entry on the list, so the missing-from-fetch check is sound.
    let removal_report = if is_full_sync {
        let fetch_ids: std::collections::HashSet<i64> =
            entries.iter().map(|e| e.anilist_id).collect();
        detect_removals(&state.db, account.id, &fetch_ids).await?
    } else {
        RemovalReport::default()
    };
    let removed_count = removal_report.removed.len();

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "AniList watch-list synced: {kept} kept ({stale_dropped} pre-cursor), {} created, {} monitor-mode updated, {} unchanged, {} skipped (import prefs off), {} excluded, {} pinned-manually, {} removed-from-list, {} failed",
            outcome.created,
            outcome.monitor_mode_updated,
            outcome.unchanged,
            outcome.skipped_by_preference,
            outcome.excluded,
            outcome.pinned_manually,
            removed_count,
            outcome.failed.len(),
        ),
        &format!("username={} fetched_total={raw_total}", account.username),
    )
    .await;
    log_failed_entries(&state.db, &outcome).await;
    // #62 — clear any stale MAL deferred count from a prior
    // provider on this same account row. AL syncs never produce
    // deferred entries (no anibridge step), so always writing 0
    // keeps the Settings UI accurate after a provider switch.
    if let Err(e) =
        external_accounts::update_last_sync_deferred_count(&state.db, account.id, 0).await
    {
        tracing::warn!(
            "update_last_sync_deferred_count failed for account_id={}: {e}",
            account.id
        );
    }
    spawn_post_merge_bulk_pass(state, outcome.new_artwork.clone()).await;

    Ok(format!(
        "AniList: fetched {raw_total}, kept {kept}, created {}, updated {}, unchanged {}, skipped {}, excluded {}, pinned-manually {}, removed-from-list {}, failed {}",
        outcome.created,
        outcome.monitor_mode_updated,
        outcome.unchanged,
        outcome.skipped_by_preference,
        outcome.excluded,
        outcome.pinned_manually,
        removed_count,
        outcome.failed.len(),
    ))
}

/// Return the AL ids in `entries` that need an AnimeDetail fetch
/// before merge: ids whose `series` row doesn't exist locally AND
/// whose status passes the user's import preferences. Existing rows
/// skip because the merge updates only their monitor_mode; not-
/// existing + not-importable entries skip because the merge will
/// SkippedByPreference them without ever needing the detail.
async fn ids_needing_detail_fetch(
    db: &SqlitePool,
    entries: &[SyncEntry],
    prefs: &ImportPreferences,
) -> Vec<i64> {
    let mut out = Vec::new();
    for entry in entries {
        if entry.anilist_id <= 0 {
            continue;
        }
        if !import_status(entry.status, prefs) {
            continue;
        }
        if matches!(
            crate::models::series::get_by_anilist_id(db, entry.anilist_id).await,
            Ok(None)
        ) {
            out.push(entry.anilist_id);
        }
    }
    out
}

/// Pump per-entry merge failures into the dedicated AniList log
/// category so the operator can see specifically which ids failed.
/// Capped to the first 10 to keep one bad list from spamming the
/// `logs` table — the count in the summary line still covers the
/// total.
async fn log_failed_entries(db: &SqlitePool, outcome: &MergeOutcome) {
    for (id, msg) in outcome.failed.iter().take(10) {
        logger::warn(
            db,
            LogCategory::ExternalSync,
            &format!("Watch-list merge failed for AL id {id}"),
            msg,
        )
        .await;
    }
}

/// Fetch the MAL watch list, resolve anibridge MAL→AL where possible,
/// and merge the resolved entries. Entries whose anibridge lookup
/// missed are counted in `outcome.deferred_jikan` and skipped — the
/// Jikan-fallback path that writes them under the negated-MAL-id
/// sentinel lands in a follow-up commit.
///
/// Token-refresh happens at this layer rather than inside
/// `services::mal::fetch_animelist` because refresh requires writing
/// the new tokens back to `external_accounts`, which the model
/// layer owns. On 401: refresh, persist, retry the fetch once. A
/// second 401 returns an error rather than looping forever — that
/// shape signals "user must re-link" and the next tick won't fix
/// it.
async fn sync_mal(
    state: &AppState,
    account: external_accounts::ExternalAccount,
    delta_cursor: Option<i64>,
    is_full_sync: bool,
) -> Result<String, String> {
    let mut access_token = account.access_token.clone();

    let entries = match mal::fetch_animelist(&access_token).await {
        Ok(entries) => entries,
        Err(mal::MalFetchError::Unauthorized) => {
            // Refresh the access token. If THIS fails (refresh token
            // dead or revoked), surface a clear "re-link required"
            // message; the eventual UI banner will read it.
            if account.refresh_token.is_empty() {
                return Err(
                    "MAL access token expired and no refresh token stored — re-link required"
                        .into(),
                );
            }
            let new_tokens = mal::refresh_access_token(&account.refresh_token)
                .await
                .map_err(|e| format!("MAL refresh failed (re-link required): {e}"))?;

            let expires_at = current_unix_ts() + new_tokens.expires_in;
            external_accounts::update_tokens(
                &state.db,
                account.id,
                &new_tokens.access_token,
                &new_tokens.refresh_token,
                Some(expires_at),
            )
            .await
            .map_err(|e| format!("persist refreshed MAL tokens: {e}"))?;

            logger::info(
                &state.db,
                LogCategory::ExternalSync,
                "MAL access token refreshed",
                &format!("account_id={} expires_at={}", account.id, expires_at),
            )
            .await;
            access_token = new_tokens.access_token;

            // Retry the fetch once with the new token. A second 401
            // here is a hard "re-link required" — the refresh
            // succeeded but the new token isn't accepted, which is
            // the failure mode you'd see if MAL revoked the OAuth
            // app or the user revoked their grant.
            mal::fetch_animelist(&access_token)
                .await
                .map_err(|e| match e {
                    mal::MalFetchError::Unauthorized => {
                        "MAL rejected the token immediately after refresh — re-link required".into()
                    }
                    mal::MalFetchError::Other(msg) => format!("MAL fetch failed: {msg}"),
                })?
        }
        Err(mal::MalFetchError::Other(msg)) => return Err(format!("MAL fetch failed: {msg}")),
    };

    let raw_total = entries.len();
    let prefs = ImportPreferences {
        import_watching: account.import_watching,
        import_planning: account.import_planning,
        import_paused: account.import_paused,
        import_dropped: account.import_dropped,
        import_completed: account.import_completed,
        skip_already_watched: account.skip_already_watched,
    };
    // Convert without filter — same rationale as sync_anilist.
    let normalized = entries_from_mal(entries);
    let after_convert = normalized.len();

    // Delta filter happens BEFORE anibridge resolution / detail fetch
    // so a delta tick doesn't incur a single network call when the
    // user's list hasn't changed since last tick.
    let normalized = drop_entries_before_cursor(normalized, delta_cursor);
    let kept = normalized.len();
    let stale_dropped = after_convert - kept;

    // Resolve MAL → AL via anibridge. Misses fall back to the
    // negated-MAL-id sentinel, handled by merge_jikan_fallback_entries
    // in the second pass below.
    let _ = anibridge::ensure_loaded().await;
    let resolved = resolve_mal_anilist_ids(normalized).await;

    let new_ids = ids_needing_detail_fetch(&state.db, &resolved, &prefs).await;
    let detail_map = if new_ids.is_empty() {
        HashMap::new()
    } else {
        anilist::get_anime_details_batch(&new_ids)
            .await
            .map_err(|e| format!("AniList detail batch fetch failed: {e}"))?
    };

    let al_outcome =
        merge_into_library(&state.db, &resolved, &detail_map, &prefs, Some(account.id)).await;

    // Second pass: walk the negated-id (anibridge-miss) entries and
    // merge each via Jikan metadata. The combined outcome's
    // deferred_jikan counter falls toward zero as Jikan acts on
    // entries; anything still deferred at the end means Jikan also
    // failed (rate-limited, deleted upstream, etc.).
    let jikan_outcome =
        merge_jikan_fallback_entries(&state.db, &resolved, &prefs, Some(account.id)).await;
    let outcome = al_outcome.merge_pass(jikan_outcome);

    // Removal detection (full-resync only) — same rationale as the
    // AL path. fetch_ids covers BOTH positive (anibridge-resolved)
    // and negated (Jikan-fallback sentinel) ids since both shapes
    // land in series.anilist_id.
    let removal_report = if is_full_sync {
        let fetch_ids: std::collections::HashSet<i64> =
            resolved.iter().map(|e| e.anilist_id).collect();
        detect_removals(&state.db, account.id, &fetch_ids).await?
    } else {
        RemovalReport::default()
    };
    let removed_count = removal_report.removed.len();

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "MyAnimeList watch-list synced: {kept} kept ({stale_dropped} pre-cursor), {} created, {} monitor-mode updated, {} unchanged, {} skipped (import prefs off), {} excluded, {} pinned-manually, {} deferred, {} removed-from-list, {} failed",
            outcome.created,
            outcome.monitor_mode_updated,
            outcome.unchanged,
            outcome.skipped_by_preference,
            outcome.excluded,
            outcome.pinned_manually,
            outcome.deferred_jikan,
            removed_count,
            outcome.failed.len(),
        ),
        &format!("username={} fetched_total={raw_total}", account.username),
    )
    .await;
    log_failed_entries(&state.db, &outcome).await;
    // #62 — persist the MAL→AL mapping-failure count so the
    // Settings UI can render a "N series couldn't be mapped" banner
    // without scraping the supervised-loop summary string.
    if let Err(e) = external_accounts::update_last_sync_deferred_count(
        &state.db,
        account.id,
        outcome.deferred_jikan as i64,
    )
    .await
    {
        tracing::warn!(
            "update_last_sync_deferred_count failed for account_id={}: {e}",
            account.id
        );
    }
    spawn_post_merge_bulk_pass(state, outcome.new_artwork.clone()).await;

    Ok(format!(
        "MyAnimeList: fetched {raw_total}, kept {kept}, created {}, updated {}, unchanged {}, skipped {}, excluded {}, pinned-manually {}, deferred {}, removed-from-list {}, failed {}",
        outcome.created,
        outcome.monitor_mode_updated,
        outcome.unchanged,
        outcome.skipped_by_preference,
        outcome.excluded,
        outcome.pinned_manually,
        outcome.deferred_jikan,
        removed_count,
        outcome.failed.len(),
    ))
}

/// Coalesced post-merge work for sync-imported series. Runs once per
/// tick (vs. once per series for the interactive add path) so a
/// 200-series first sync doesn't spawn 200 background tasks. Caches
/// cover + banner artwork sequentially through `artwork::cache_image`,
/// then fires a single Jellyfin library refresh if any series was
/// imported and the user has Jellyfin configured.
///
/// All work runs in a spawned task — the sync tick returns immediately
/// after kicking off this future. A failure in any step logs but
/// doesn't propagate; the artwork host being down or Jellyfin being
/// offline shouldn't make the next tick consider the prior tick a
/// failure (which would block the cursor advance).
async fn spawn_post_merge_bulk_pass(state: &AppState, specs: Vec<NewArtworkSpec>) {
    if specs.is_empty() {
        return;
    }
    let db = state.db.clone();
    let jellyfin = state.jellyfin.read().await.clone();
    tokio::spawn(async move {
        // Sequential rather than parallel: hammering an artwork CDN
        // with 400 concurrent requests is the kind of thing that gets
        // an IP rate-limited. The serial walk takes a minute or two
        // for a fresh import; the user's library still renders during
        // that window because cached_or_source_url falls back to the
        // upstream URL when the local key isn't present yet.
        for spec in &specs {
            if !spec.cover_url.is_empty() {
                let _ = artwork::cache_image(
                    &db,
                    &format!("series-{}-cover", spec.series_id),
                    "series",
                    Some(spec.series_id),
                    "cover",
                    &spec.cover_url,
                )
                .await;
            }
            if !spec.banner_url.is_empty() {
                let _ = artwork::cache_image(
                    &db,
                    &format!("series-{}-banner", spec.series_id),
                    "series",
                    Some(spec.series_id),
                    "banner",
                    &spec.banner_url,
                )
                .await;
            }
        }
        // One Jellyfin refresh at the end — covers all newly-imported
        // series in a single call. The same coalesce avoids the
        // pattern where 200 individual interactive adds would fire 200
        // /Library/Refresh requests against Jellyfin and overwhelm the
        // scan queue.
        if let Some(client) = jellyfin
            && let Err(e) = client.refresh_library().await
        {
            logger::warn(
                &db,
                LogCategory::Jellyfin,
                "Sync-driven Jellyfin refresh failed",
                &e,
            )
            .await;
        }
        logger::info(
            &db,
            LogCategory::ExternalSync,
            &format!(
                "Bulk-mode post-merge artwork cache complete ({} series)",
                specs.len()
            ),
            "",
        )
        .await;
    });
}

pub(crate) fn current_unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the most recent successful tick from `scheduled_task_runs`.
/// The supervised loop seeds its `minutes_since_last` counter from
/// this so a process restart doesn't force an immediate re-run when
/// we last synced under the configured cadence.
pub async fn minutes_since_last_run(db: &SqlitePool) -> i64 {
    crate::models::scheduled_tasks::minutes_since_last_finished(db, "external_sync")
        .await
        .unwrap_or(10_000)
}
