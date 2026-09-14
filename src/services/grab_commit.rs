//! Shared grab-commit helper for issue #83's interactive file-picker.
//!
//! Two paths end up with a live, file-priority-applied, resumed torrent
//! that Ryokan wants to attribute to the library:
//!
//!   1. **User-confirmed** — `handlers::grab::grab_confirm` applies the
//!      user's selections and resumes the torrent. Filenames here are
//!      the user-kept subset (decision #7).
//!   2. **Walkaway auto-commit** — `services::grab_sweep::auto_commit_row`
//!      marks every file wanted on a heartbeat-lapsed row (decision #3).
//!      Filenames here are the full file list.
//!
//! Both paths need the same downstream work: write a `grabbed_torrents`
//! row so post-processing picks up the download, and run sibling
//! auto-expand so a batch pack's sequels/prequels/side-stories get
//! their own library rows and per-file routing.
//!
//! The auto-search path in `handlers::library::search` already does this
//! with a pre-computed classification from the scoring pipeline. The
//! interactive path doesn't — the modal doesn't run source
//! classification. We fall back to `ClassificationResult::unknown()`
//! and let post-processing backfill the real `(source, resolution,
//! is_remux)` verdict once files land on disk. `needs_review` flips
//! true on the unknown row so the classifier review page surfaces it
//! if post-processing can't confidently classify it either.

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::grabbed_torrents;
use crate::models::log::LogCategory;
use crate::models::pending_grabs::PendingGrab;
use crate::services::anilist;
use crate::services::auto_expand::{self, AutoExpandGrabContext};
use crate::services::auto_search;
use crate::services::logger;
use crate::services::notifications;
use crate::services::source::ClassificationResult;

/// Write the `grabbed_torrents` row and kick off sibling auto-expand
/// for a pending grab that's now live on the download client. Returns
/// `Some(grab_id)` on success, `None` when library attribution was
/// skipped (no series context, empty title, DB insert deduped against
/// an in-flight row).
///
/// `filenames` is the file list to feed into auto-expand — the
/// user-selected subset on the confirm path, the full list on the
/// auto-commit path. Auto-expand writes `grabbed_torrent_series`
/// routes only for files present in the list, so a subset here
/// correctly limits sibling library attribution to what's actually
/// going to import (decision #7's post-confirm timing shift).
///
/// Auto-expand fires as a `tokio::spawn` — the caller returns to the
/// HTTP response before the metadata-bound relation walk completes.
/// Post-processing has its own auto-expand safety net so a failure
/// here doesn't prevent eventual library attribution.
pub async fn commit_grab_and_expand(
    state: &AppState,
    row: &PendingGrab,
    filenames: Vec<String>,
    release_title: &str,
    is_batch: bool,
) -> Option<i64> {
    // Bare-magnet grab from the global search page with no series
    // context — we can't attribute the download to a library row, so
    // skip the write. The torrent still downloads (the caller already
    // issued the resume); it just won't show up in any series's
    // grabbed list. Post-processing will ignore it for the same
    // reason — no `grabbed_torrents` row to key off.
    let Some(series_id) = row.series_id else {
        logger::debug(
            &state.db,
            LogCategory::Grab,
            "skipping grab-row write — no series_id on pending grab",
            &row.info_hash,
        )
        .await;
        return None;
    };

    if release_title.trim().is_empty() {
        logger::warn(
            &state.db,
            LogCategory::Grab,
            "skipping grab-row write — release title empty",
            &row.info_hash,
        )
        .await;
        return None;
    }

    let ep_nums = episode_numbers_for_commit(release_title, &filenames);

    let grab_id = match grabbed_torrents::record_grab(
        &state.db,
        &row.info_hash,
        release_title,
        series_id,
        &ep_nums,
        is_batch,
    )
    .await
    {
        Ok(Some(id)) => {
            // Stamp the dispatch client id captured at preview time so
            // post-processing's `resolve_grab_client` routes the import /
            // delete back through the same client even if defaults change.
            // Without this, picker-confirm and walkaway-auto-commit grabs
            // land NULL-stamped: SAB rows are still rescued by the
            // `SABnzbd_nzo_` hash heuristic, but a BT grab routed to a
            // non-default client (e.g. indexer pinned to seedbox-qBit
            // while local-qBit is the default) silently falls through to
            // the torrent default at import time.
            if let Err(e) =
                grabbed_torrents::set_download_client(&state.db, id, row.download_client_id).await
            {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    "set_download_client failed after record_grab",
                    &format!("{} ({})", e, row.info_hash),
                )
                .await;
            }
            id
        }
        Ok(None) => {
            // Dedup hit against an in-flight `pending` row — another
            // flow is mid-commit on this hash. Don't stomp it; the
            // other flow will drive auto-expand. This matches the
            // auto_search path's `grab_id.flatten()` behavior.
            logger::debug(
                &state.db,
                LogCategory::Grab,
                "grab dedup hit — skipping auto-expand",
                &row.info_hash,
            )
            .await;
            return None;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::Grab,
                "record_grab failed",
                &format!("{} ({})", e, row.info_hash),
            )
            .await;
            return None;
        }
    };

    // The grab's own episodes read as downloading from here on. The
    // direct grab endpoints write these rows themselves; this path
    // (picker confirm, walkaway auto-commit) used to leave the parent
    // untagged, so the Wanted page kept listing a picked episode as
    // missing until the import landed. Auto-expand only backfills what
    // the grab did not already claim, so nothing is written twice.
    let classification = crate::services::source::classify_release_sync(release_title, None);
    let release_group = release_group_from_metadata(&row.release_metadata_json);
    for ep in &ep_nums {
        if let Err(e) = crate::models::episode_tags::record_grab(
            &state.db,
            series_id,
            *ep,
            &classification,
            release_title,
            &release_group,
            0,
            is_batch,
        )
        .await
        {
            logger::warn(
                &state.db,
                LogCategory::Grab,
                &format!("failed to record grabbed tag for episode {ep}"),
                &format!("{} ({})", e, row.info_hash),
            )
            .await;
        }
    }

    // Issue #118 — fire `NotificationEvent::Grabbed` for this commit.
    // No-op early-return when no providers are configured (the
    // foundation PR ships an always-empty cache); subsequent provider
    // PRs flip this from a tree-fall into a real outbound dispatch
    // without further changes here. Indexer name + score aren't
    // resolvable from the picker/walkaway path's `PendingGrab` row
    // (the modal doesn't carry the auto_search scoring context);
    // both default to None. Episode number is the lowest in the
    // parsed range — single-episode grabs use it directly, batches
    // pick the first.
    notifications::emit_grabbed(
        state,
        series_id,
        ep_nums.first().copied().unwrap_or(0),
        release_title,
        None,
        None,
        Some(row.client_kind.clone()),
    )
    .await;

    // Fire-and-forget the sibling auto-expand. Don't block the HTTP
    // response — the transitive relation walk can take several
    // seconds on cold DETAIL_CACHE, and confirm / auto-commit both
    // want to return as soon as the client-side work is done.
    //
    // A failed fetch or panic inside the spawn drops the auto-expand
    // silently; the import-time call in `services::post_processing`
    // is the safety net that guarantees siblings land eventually.
    if !filenames.is_empty() {
        let db_task = state.db.clone();
        let title_task = release_title.to_string();
        let ep_nums_task = ep_nums.clone();
        let info_hash_task = row.info_hash.clone();
        tokio::spawn(async move {
            run_auto_expand(
                db_task,
                info_hash_task,
                series_id,
                ep_nums_task,
                grab_id,
                title_task,
                filenames,
            )
            .await;
        });
    }

    Some(grab_id)
}

/// The episodes a confirmed grab is for: the selected files' own
/// numbers when any of them parse (a picker grab that kept two files
/// of a twelve-episode batch is a grab for those two, so the Wanted
/// page and the series page show only those as downloading and the
/// import claims only those), else the release title's numbers (a
/// single-file release, or a pack whose files carry no numbers).
/// `parse_release_numbers` handles single (`... - 05 ...`), range
/// (`01-12`), and absolute-numbered (`25-48`) titles; an unparseable
/// title yields an empty list, which post-processing tolerates.
pub(crate) fn episode_numbers_for_commit(release_title: &str, filenames: &[String]) -> Vec<i32> {
    let mut eps: Vec<i32> = filenames
        .iter()
        .filter(|n| auto_search::is_media_filename(n))
        .filter_map(|n| {
            let base = n.rsplit('/').next().unwrap_or(n).to_ascii_lowercase();
            crate::services::media::parse_episode_span(&base)
        })
        .filter(|span| !span.special)
        .flat_map(|span| span.episodes())
        .filter(|e| *e > 0)
        .collect();
    if eps.is_empty() {
        eps = auto_search::parse_release_numbers(release_title)
            .into_iter()
            .collect();
    }
    eps.sort_unstable();
    eps.dedup();
    eps
}

/// The `group` the picker stored in the pending row's release
/// metadata, empty when absent.
fn release_group_from_metadata(release_metadata_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(release_metadata_json)
        .ok()
        .and_then(|v| v.get("group").and_then(|g| g.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// Resolve the parent series's AL detail and invoke `expand_from_files`.
/// Broken out of the spawn so the `?`-style control flow reads cleanly
/// without making every step inside the spawn an `if let Some` ladder.
async fn run_auto_expand(
    db: SqlitePool,
    info_hash: String,
    series_id: i64,
    ep_nums: Vec<i32>,
    grab_id: i64,
    title: String,
    filenames: Vec<String>,
) {
    // Look up anilist_id from series_id. Negative AL IDs (MAL-fallback
    // sentinel per CLAUDE.md) route to Jikan inside
    // `get_anime_detail_with_options`, which correctly returns an
    // AnimeDetail for display purposes but carries the negated id —
    // sibling detection still runs against that shape since
    // expand_from_files already filters `parent_detail.id <= 0`
    // internally.
    let anilist_id =
        match sqlx::query_scalar::<_, i64>("SELECT anilist_id FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_optional(&db)
            .await
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                logger::warn(
                    &db,
                    LogCategory::Grab,
                    "auto-expand: series row vanished between grab-row write and detail fetch",
                    &format!("series_id={} hash={}", series_id, info_hash),
                )
                .await;
                return;
            }
            Err(e) => {
                logger::warn(
                    &db,
                    LogCategory::Grab,
                    "auto-expand: DB error resolving series_id",
                    &format!("{} ({})", e, info_hash),
                )
                .await;
                return;
            }
        };

    let detail = match anilist::get_anime_detail_with_options(anilist_id, None, false).await {
        Ok(d) => d,
        Err(e) => {
            logger::info(
                &db,
                LogCategory::Grab,
                "auto-expand: AL detail fetch failed; post-processing will retry at import time",
                &format!("{} ({})", e, info_hash),
            )
            .await;
            return;
        }
    };

    let ctx = AutoExpandGrabContext {
        classification: ClassificationResult::unknown(),
        release_group: String::new(),
        size_bytes: 0,
    };
    // `expand_from_files` returns `newly_added_siblings: usize` and
    // handles its own per-sibling error logging; we only surface the
    // count here so a zero-sibling detection run is visible in the
    // logs alongside a match-heavy one. Matches the style of the
    // two DB fetches above which log their outcomes explicitly.
    let newly_added = auto_expand::expand_from_files(
        &db, &filenames, &detail, series_id, &ep_nums, grab_id, &title, &ctx,
    )
    .await;
    if newly_added > 0 {
        logger::info(
            &db,
            LogCategory::Grab,
            &format!("grab auto-expand added {newly_added} sibling series from '{title}'"),
            &info_hash,
        )
        .await;
    } else {
        // Emit a debug tombstone for the zero-sibling run too — makes
        // "did auto-expand even fire on this grab?" answerable from
        // logs without re-deriving from the absence of an info line.
        logger::debug(
            &db,
            LogCategory::Grab,
            &format!("grab auto-expand detected no siblings in '{title}'"),
            &info_hash,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

    fn pending_grab_for(series_id: Option<i64>, info_hash: &str) -> PendingGrab {
        PendingGrab {
            preview_id: "pv-1".to_string(),
            info_hash: info_hash.to_string(),
            client_kind: "qbittorrent".to_string(),
            indexer_id: None,
            series_id,
            created_at: 0,
            heartbeat_at: 0,
            file_list_json: String::new(),
            release_metadata_json: String::new(),
            error_message: String::new(),
            we_added_torrent: true,
            download_client_id: None,
        }
    }

    #[tokio::test]
    async fn commit_returns_none_when_pending_grab_has_no_series_id() {
        // Bare-magnet grab from the global search page (no series
        // attribution). The handler still issues the resume — this
        // helper just bails on the library-attribution write.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(None, "deadbeef");
        let result = commit_grab_and_expand(&state, &row, vec![], "[Group] Show 01", false).await;
        assert!(result.is_none());
        // Nothing was written.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn commit_returns_none_when_release_title_is_empty() {
        // Defensive — the handler shouldn't be calling with an empty
        // title, but if it does we don't want to write a grab row
        // with `release_title = ''` (post-processing keys naming
        // off this column).
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "deadbeef");
        let result = commit_grab_and_expand(&state, &row, vec!["a.mkv".into()], "  ", false).await;
        assert!(result.is_none());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn commit_writes_grab_row_with_parsed_episode_numbers() {
        // Happy path: series attribution + a parseable single-episode
        // title → record_grab returns Some(id), the row lands with
        // episode_numbers=[1]. We pass empty `filenames` so the
        // fire-and-forget auto_expand spawn no-ops (the
        // `if !filenames.is_empty()` guard skips it), keeping the
        // test deterministic.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "abcdef0001");
        let id = commit_grab_and_expand(
            &state,
            &row,
            vec![], // empty so auto_expand spawn doesn't fire
            "[GroupX] Show - 01 [1080p].mkv",
            false,
        )
        .await
        .expect("commit should succeed and return a grab id");

        let (got_hash, got_series, got_eps): (String, i64, String) = sqlx::query_as(
            "SELECT hash, series_id, episode_numbers FROM grabbed_torrents WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(got_hash, "abcdef0001");
        assert_eq!(got_series, series_id);
        assert_eq!(got_eps, "[1]", "parsed episode numbers should round-trip");

        // The grab's episode reads as downloading from now on.
        let state_row: Option<String> = sqlx::query_scalar(
            "SELECT state FROM episode_quality_tags WHERE series_id = ? AND episode_number = 1",
        )
        .bind(series_id)
        .fetch_optional(&db)
        .await
        .unwrap();
        assert_eq!(state_row.as_deref(), Some("grabbed"));
    }

    #[test]
    fn commit_episodes_come_from_the_selected_files_when_they_parse() {
        let title = "[Group] Show - 01-12 (BD 1080p) [Batch]";
        let picked = vec![
            "Show/[Group] Show - 02 (BD 1080p).mkv".to_string(),
            "Show/[Group] Show - 03 (BD 1080p).mkv".to_string(),
            "Show/Extras/[Group] Show - NCOP1.mkv".to_string(),
            "Show/readme.txt".to_string(),
        ];
        assert_eq!(episode_numbers_for_commit(title, &picked), vec![2, 3]);
        // No selected file parses: the title's range stands.
        let unnumbered = vec!["Show/movie.mkv".to_string()];
        assert_eq!(
            episode_numbers_for_commit(title, &unnumbered),
            (1..=12).collect::<Vec<i32>>()
        );
        // A single-file release: file and title agree.
        assert_eq!(
            episode_numbers_for_commit(
                "[Group] Show - 05 (1080p).mkv",
                &["[Group] Show - 05 (1080p).mkv".to_string()]
            ),
            vec![5]
        );
    }

    #[tokio::test]
    async fn commit_handles_unparseable_title_with_empty_episode_list() {
        // A title that the parser can't decode (no number tokens at
        // all) writes the grab row with an empty episode_numbers
        // array. Post-processing per-file classification picks up
        // the slack at import time.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 200, "Movie").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "unparseable01");
        let id = commit_grab_and_expand(&state, &row, vec![], "Some Movie [1080p]", false)
            .await
            .expect("commit should succeed even with unparseable title");
        let eps: String =
            sqlx::query_scalar("SELECT episode_numbers FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(eps, "[]");
    }

    #[tokio::test]
    async fn commit_dedups_against_inflight_pending_row_by_hash() {
        // record_grab returns Ok(None) when an in-flight `pending`
        // row already exists for the same hash (PR 110's dedup
        // guard). Pin that path: a second commit with the same
        // hash returns None and doesn't double-insert.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 300, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "dup-hash-1");

        let first = commit_grab_and_expand(&state, &row, vec![], "[G] Show - 01.mkv", false).await;
        assert!(first.is_some());

        let second = commit_grab_and_expand(&state, &row, vec![], "[G] Show - 01.mkv", false).await;
        assert!(
            second.is_none(),
            "dedup must return None on the second call"
        );

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE hash = 'dup-hash-1'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1, "exactly one row should exist after the dedup");
    }

    #[tokio::test]
    async fn commit_stamps_download_client_id_from_pending_grab() {
        // Picker confirm + walkaway auto-commit both flow through this
        // helper. Without the stamp, post-processing's resolve_grab_client
        // can't route the import / delete back through the same client
        // that received the grab — a non-default-client BT pin would
        // silently fall through to the torrent default at import.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 500, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let mut row = pending_grab_for(Some(series_id), "stamp-hash");
        row.download_client_id = Some(7);
        let id = commit_grab_and_expand(&state, &row, vec![], "[G] Show - 01.mkv", false)
            .await
            .expect("commit");
        let stamped: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stamped, Some(7));
    }

    #[tokio::test]
    async fn commit_leaves_download_client_id_null_when_pending_row_has_none() {
        // Bare-magnet / legacy pending rows with no client capture still
        // round-trip cleanly: the stamp call writes NULL, which is the
        // sentinel post-processing's heuristic chain expects.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 501, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "null-stamp-hash");
        let id = commit_grab_and_expand(&state, &row, vec![], "[G] Show - 02.mkv", false)
            .await
            .expect("commit");
        let stamped: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stamped, None);
    }

    #[tokio::test]
    async fn commit_writes_is_batch_flag_into_grab_row() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 400, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "batch-hash");
        let id = commit_grab_and_expand(&state, &row, vec![], "[G] Show 01-12 Batch [1080p]", true)
            .await
            .expect("commit");
        let is_batch: i64 =
            sqlx::query_scalar("SELECT is_batch FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(is_batch, 1);
    }
}
