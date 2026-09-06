//! Lifecycle + classification-gap scanning for the post-processing loop.
//!
//! `grab_is_stale` drives the age-based failure sweep, `fallback_ep_offset`
//! picks between absolute and relative episode numbering for SubsPlease-style
//! releases (issue #30), and the `scan_*_for_unclassified` family surfaces
//! episodes whose quality-tag row is missing so the UI can prompt a
//! classifier re-run.

use std::collections::HashMap;
use std::path::Path;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, grabbed_torrents, series};
use crate::services::source::{self, SeriesContext};
use crate::services::{logger, media};

use super::POST_PROC_LOCK;

/// Seconds elapsed since a SQLite `CURRENT_TIMESTAMP` value
/// (`"YYYY-MM-DD HH:MM:SS"`, UTC). `None` when the text does not parse.
pub fn sqlite_age_secs(timestamp: &str) -> Option<i64> {
    let then = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S").ok()?;
    let now = chrono::Utc::now().naive_utc();
    Some(now.signed_duration_since(then).num_seconds())
}

pub fn grab_is_stale(grabbed_at: &str, max_age_secs: i64) -> bool {
    sqlite_age_secs(grabbed_at).is_some_and(|elapsed| elapsed > max_age_secs)
}

pub(super) fn fallback_ep_offset(raw_ep_num: i32, cumulative_prior_episodes: i32) -> i32 {
    if cumulative_prior_episodes > 0 && raw_ep_num > cumulative_prior_episodes {
        cumulative_prior_episodes
    } else {
        0
    }
}

/// Per-series derived state used during an import. Built once per target
/// series_id on demand inside [`import_torrent`] so a multi-series batch
/// grab doesn't re-fetch the same rows or re-create the same directories
/// for every file. The single-series case fills exactly one entry; a

#[derive(Default)]
pub struct LibraryClassifyReport {
    pub series_scanned: usize,
    pub files_scanned: usize,
    pub files_classified: usize,
    pub files_needing_review: usize,
}

/// One file queued up by the lock-held enumeration phase, carried over
/// into the unlocked classification phase of `scan_library_for_unclassified`.
struct PendingClassification {
    series_id: i64,
    series_status: String,
    series_season_year: Option<i32>,
    series_end_year: Option<i32>,
    series_root: std::path::PathBuf,
    file_path: std::path::PathBuf,
    episode_number: i32,
    /// Sanitized on-disk filename. Used as a fallback title for L1
    /// classification when `original_torrent_name` is empty (externally
    /// imported files that Ryokan never grabbed).
    title: String,
    /// Original torrent name from `grabbed_torrents`, if a Ryokan grab
    /// exists for this episode. Passed to `classify_post_download` as
    /// the L1 input when non-empty: the post-processing import step
    /// sanitizes release tags out of filenames (e.g. "[SubsPlease] Foo
    /// (1080p) [Batch]" becomes "Foo - S01E05 - Title.mkv"), so
    /// classifying against the on-disk name yields `rule=empty` and
    /// the episode shows as UNKNOWN forever. Looking up the original
    /// grab lets us classify against the unsanitized release name.
    original_torrent_name: String,
    /// True when an `episode_quality_tags` row already exists for this
    /// (series, episode) pair. Determines whether the persist step goes
    /// through `update_classification` (UPDATE) or `record_grab` +
    /// `mark_completed` (INSERT upsert + state flip).
    row_exists: bool,
}

/// Walk every tracked series and (re-)classify on-disk video files that
/// don't yet have a confident classification row. This is the Phase 2
/// "library scan path" — it catches files that were imported outside of
/// Ryokan's own grab pipeline (pre-existing rips, manual drops, migrations
/// from another PVR), AND it self-heals rows that the classifier first
/// saw as `Unknown` (low-confidence filename-only result) now that the
/// file is actually on disk and the full ffprobe/dir-walk pipeline can
/// run against it.
///
/// Skips files whose tag row is:
///  - already classified with a confident non-empty, non-"unknown" source,
///  - flagged `needs_review = 1` (user should resolve via the review
///    queue — we don't want to race against them),
///  - flagged `manual_override = 1` (user pinned the classification).
///
/// **Locking:** the enumeration phase (config read, `scan_series_folder`,
/// existing-tag lookup, disk-existence filter) holds `POST_PROC_LOCK` so
/// the work list is a consistent snapshot that can't be invalidated by a
/// parallel `run_once`. Once the list is built, the lock is released
/// before we shell out to ffprobe via `classify_post_download` — those
/// calls can take hundreds of ms per file and would otherwise block the
/// 1-minute `process_completed_downloads` background loop for the full
/// duration of a large scan. The DB writes in the second phase rely on
/// SQLite's normal write serialization; the worst-case race (a real
/// import landing between our classify and persist steps on the same
/// episode) leaves a single row briefly stale and self-heals on the
/// next scan.
pub async fn scan_library_for_unclassified(state: &AppState) -> LibraryClassifyReport {
    scan_for_unclassified(state, None).await
}

/// Issue #53: same enumeration + classify pipeline as
/// [`scan_library_for_unclassified`] but scoped to a single series.
/// Called from the import flow (`handlers/library/crud::add_series`) as a
/// one-shot tokio::spawn so a freshly-imported series with pre-existing
/// files on disk gets a classification pass within seconds instead of
/// waiting up to six hours for the next periodic sweep.
pub async fn scan_series_for_unclassified(
    state: &AppState,
    series_id: i64,
) -> LibraryClassifyReport {
    scan_for_unclassified(state, Some(series_id)).await
}

async fn scan_for_unclassified(
    state: &AppState,
    only_series_id: Option<i64>,
) -> LibraryClassifyReport {
    let mut report = LibraryClassifyReport::default();

    let pending: Vec<PendingClassification> = {
        // Lock scope: enumerate only. Dropped before the classify loop.
        let _guard = POST_PROC_LOCK.lock().await;

        let cfg = match config::get_config(&state.db).await.ok().flatten() {
            Some(c) => c,
            None => return report,
        };
        if cfg.media_root.is_empty() {
            return report;
        }

        let tracked = match only_series_id {
            // Single-series fast path used by the import hook — skip the
            // full library enumeration when we know which series to look
            // at. Bail silently when the id doesn't resolve (deleted
            // between spawn and run).
            Some(id) => match series::get_by_id(&state.db, id).await.ok().flatten() {
                Some(row) => vec![row],
                None => return report,
            },
            None => series::get_all(&state.db).await.unwrap_or_default(),
        };
        let mut pending = Vec::new();

        for row in &tracked {
            if row.folder_name.is_empty() {
                continue;
            }
            let disk_files = media::scan_series_folder(&cfg.media_root, &row.folder_name).await;
            if disk_files.is_empty() {
                continue;
            }
            report.series_scanned += 1;

            let existing = episode_tags::get_for_series(&state.db, row.id)
                .await
                .unwrap_or_default();

            // One bulk fetch of imported grabs for this series, used
            // by every file in the inner loop below to look up the
            // unsanitized torrent name. Replaces a pair of per-file
            // queries (find_imported_for_episode + a fallback
            // most_recent_imported_torrent_name_for_series) that ran
            // inside the held POST_PROC_LOCK — that was ~4800
            // round-trips per pass on a 100-series, 24-ep library.
            let imported_grabs = grabbed_torrents::imported_grabs_for_series(&state.db, row.id)
                .await
                .unwrap_or_default();
            // Per-episode lookup; first occurrence (DESC by grabbed_at)
            // wins, so each episode maps to its most recent grab. This
            // matches the prior find_imported_for_episode .next()
            // semantics on a DESC-sorted result set.
            let mut grab_name_for_episode: HashMap<i32, String> = HashMap::new();
            for (name, eps) in &imported_grabs {
                for ep in eps {
                    grab_name_for_episode
                        .entry(*ep)
                        .or_insert_with(|| name.clone());
                }
            }
            let most_recent_grab_name: Option<&str> =
                imported_grabs.first().map(|(n, _)| n.as_str());

            let series_root = Path::new(&cfg.media_root).join(&row.folder_name);

            for file in &disk_files {
                report.files_scanned += 1;

                // Decide whether to (re-)classify this file.
                //
                // Skip when:
                //  - `manual_override = 1`: the user explicitly pinned
                //    this classification — never touch.
                //  - a tag exists with a non-empty source that isn't
                //    "unknown". Already confidently classified. If such
                //    a row is *also* flagged `needs_review`, we respect
                //    that pending user decision and leave it alone.
                //
                // Pick up when:
                //  - no tag exists yet (externally-imported file).
                //  - the source column is empty (pre-Phase-1b rows).
                //  - the source is literally "unknown" (case-insensitive)
                //    — the classifier couldn't decide at grab time but
                //    the file exists now, so retry with the full
                //    ffprobe/dir-walk pipeline. This is the background
                //    self-healing path for rows that started as Unknown.
                //
                // Note: unknown rows are retried **even when
                // `needs_review = 1`**. That used to trap stale bad
                // classifications — a row that was classified
                // against the sanitized post-import filename before
                // the original-torrent-name lookup landed produces
                // `rule=empty` and gets flagged for review, and the
                // old guard then refused to retry it even after the
                // bug was fixed. An unknown needs-review row carries
                // no useful user decision to stomp on (the user
                // resolves via `set_manual_override`, which sets
                // `manual_override = 1` and clears `needs_review`),
                // so retrying is safe.
                let tag = existing.get(&file.episode_number);
                if let Some(t) = tag {
                    if t.manual_override {
                        continue;
                    }
                    let src = t.source.trim();
                    let is_unknown = src.is_empty() || src.eq_ignore_ascii_case("unknown");
                    if !is_unknown {
                        continue;
                    }
                    // Issue #53: an UNKNOWN row that was already attempted
                    // by the full-pipeline classifier (ffprobe + dir +
                    // group + temporal + filename) won't change verdict
                    // on the same bytes. Skip it so we don't re-ffprobe
                    // every six hours forever — the user can still force
                    // a fresh attempt by clearing/re-applying a manual
                    // override or running the sweep manually after
                    // updating the source-pipeline rules.
                    if t.classification_attempted_at.is_some() {
                        continue;
                    }
                }

                // Reconstruct the absolute path so ffprobe can read the file.
                let file_path = series_root.join(&file.filename);
                if !file_path.exists() {
                    continue;
                }

                // Use the sanitized on-disk filename as the fallback
                // title. For Ryokan-grabbed files we'll override this
                // with the original torrent name in the unlocked
                // classify phase below so L1 sees the full release tag
                // set instead of the tags-stripped post-import name.
                let title = Path::new(&file.filename)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&file.filename)
                    .to_string();

                // Look up the grab that covers this episode so we can
                // classify against the unsanitized torrent name. The
                // pre-built `grab_name_for_episode` map and
                // `most_recent_grab_name` fallback come from the
                // single bulk fetch above — both lookups are now
                // pure-Rust and don't touch the DB inside the held
                // POST_PROC_LOCK. The fallback exists for old batch
                // grabs recorded with `episode_numbers = []` before
                // the grab-time episode-range fix; for a single-series
                // batch the most-recent grab is by definition the one
                // these files came from.
                let original_torrent_name = grab_name_for_episode
                    .get(&file.episode_number)
                    .cloned()
                    .or_else(|| most_recent_grab_name.map(|s| s.to_string()))
                    .unwrap_or_default();

                pending.push(PendingClassification {
                    series_id: row.id,
                    series_status: row.status.clone(),
                    series_season_year: row.season_year,
                    series_end_year: row.end_year,
                    series_root: series_root.clone(),
                    file_path,
                    episode_number: file.episode_number,
                    title,
                    original_torrent_name,
                    row_exists: tag.is_some(),
                });
            }
        }

        pending
    };

    // Unlocked classification + persist phase. ffprobe shell-outs are the
    // slow part; doing them here instead of under the lock lets
    // `process_completed_downloads` keep running in parallel.
    for item in pending {
        // Prefer the original torrent name from `grabbed_torrents`
        // (carries full release tags like "[SubsPlease] Foo (01-12)
        // [BD 1080p]"), fall back to the sanitized on-disk filename
        // for externally-imported files that Ryokan never grabbed.
        // This is the difference between L1 producing useful
        // evidence and producing `rule=empty` on a release that
        // landed via post-processing's rename step.
        let classify_title: &str = if !item.original_torrent_name.is_empty() {
            &item.original_torrent_name
        } else {
            &item.title
        };
        let is_batch = crate::models::grabbed_torrents::get_is_batch_by_name(
            &state.db,
            item.series_id,
            classify_title,
        )
        .await
        .unwrap_or(false);
        let result = source::classify_post_download(
            &state.db,
            &item.file_path,
            Some(&item.series_root),
            classify_title,
            Some(SeriesContext {
                status: &item.series_status,
                season_year: item.series_season_year,
                end_year: item.series_end_year,
            }),
            is_batch,
        )
        .await;

        // The row may not exist yet for externally-imported files, so
        // we can't rely on `update_classification` alone (it's an
        // UPDATE, not an UPSERT). Use `record_grab` with synthetic
        // release metadata to insert-or-upsert, then flip state to
        // 'completed' since the file is already on disk.
        if !item.row_exists {
            // Best-effort single stat — failure just leaves size at 0.
            let file_size = tokio::fs::metadata(&item.file_path)
                .await
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            // Externally-imported file — we're creating both the quality
            // tag and the grab history row from thin air. `is_batch` is
            // looked up above from the optional matching grabbed_torrents
            // row; falls back to false for truly external files.
            let _ = episode_tags::record_grab(
                &state.db,
                item.series_id,
                item.episode_number,
                &result,
                classify_title,
                "",
                file_size,
                is_batch,
            )
            .await;
            let _ = episode_tags::mark_completed(&state.db, item.series_id, &[item.episode_number])
                .await;
            // Issue #53: stamp classification_attempted_at so the next
            // sweep skips this row if `result` came back UNKNOWN. The
            // grab-time path of `record_grab` deliberately leaves the
            // column NULL, so we set it explicitly here for the
            // post-classify path.
            let _ = episode_tags::stamp_classification_attempted(
                &state.db,
                item.series_id,
                item.episode_number,
            )
            .await;
            // Flip the fresh grab history row to 'completed' and stamp
            // in the on-disk file basename for the episode detail modal.
            let imported_basename = item
                .file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(classify_title)
                .to_string();
            let _ = episode_tags::mark_grab_history_completed(
                &state.db,
                item.series_id,
                item.episode_number,
                &imported_basename,
                file_size,
            )
            .await;
        } else {
            // `update_classification` already sets
            // classification_attempted_at internally — no extra stamp
            // needed on this branch.
            let _ = episode_tags::update_classification(
                &state.db,
                item.series_id,
                item.episode_number,
                &result,
            )
            .await;
        }

        report.files_classified += 1;
        if result.needs_review {
            report.files_needing_review += 1;
            // Issue #118 — fire `ClassifierNeedsReview` for the
            // reclassify-sweep path (Settings → Reclassify all). Same
            // event the post-download classifier fires; default-off
            // in the matrix because this sweep can flip dozens of
            // rows in one shot. Users opt in by enabling the event
            // for a provider.
            let verdict = result.label();
            crate::services::notifications::emit_classifier_needs_review(
                state,
                item.series_id,
                item.episode_number,
                result.confidence as i32,
                &verdict,
            )
            .await;
        }
    }

    logger::info(
        &state.db,
        LogCategory::PostProcess,
        "Library classify scan finished",
        &format!(
            "series={}, files_scanned={}, classified={}, needs_review={}",
            report.series_scanned,
            report.files_scanned,
            report.files_classified,
            report.files_needing_review
        ),
    )
    .await;

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── #30 fallback offset selection ─────────────────────────────────

    #[test]
    fn fallback_offset_absolute_jjk_s3_subsplease() {
        // Motivating case: [SubsPlease] Jujutsu Kaisen - 56 → S3 E9.
        // S1 (24) + S2 (23) = 47 prior-cour episodes. Parsed 56 > 47,
        // so the filename is absolute-numbered and offset = 47.
        assert_eq!(fallback_ep_offset(56, 47), 47);
    }

    #[test]
    fn fallback_offset_relative_release_within_cour() {
        // Erai-raws / cour-specific releases number from 1: raw 9 is
        // relative, 9 ≤ 47, offset = 0. Subtracting zero leaves E9.
        assert_eq!(fallback_ep_offset(9, 47), 0);
    }

    #[test]
    fn fallback_offset_zero_for_first_season_entry() {
        // First-season entry has no prior cours (cumulative = 0), so
        // the legacy behavior is preserved regardless of parsed number.
        assert_eq!(fallback_ep_offset(56, 0), 0);
        assert_eq!(fallback_ep_offset(1, 0), 0);
    }

    #[test]
    fn fallback_offset_mis_grabbed_prior_cour_number() {
        // Pathological: parsed 25 (S2 E1) somehow downloaded into the
        // S3 series folder. 25 ≤ 47 → offset = 0, file lands as E25 of
        // S3. S3 doesn't have E25 so the subsequent rename step fails
        // — the right loud failure rather than silently translating to
        // a plausible-looking E-22.
        assert_eq!(fallback_ep_offset(25, 47), 0);
    }

    #[test]
    fn fallback_offset_equal_to_cumulative() {
        // Boundary: raw exactly equals cumulative. That value would be
        // ambiguous (last episode of the prior cour, or legitimate
        // relative E47 of a show with 47+ episodes). Without more
        // signal we keep it as relative (offset 0) — the alternative
        // (offset = cumulative) would silently map legitimate E47
        // releases of a 48-episode show to E0.
        assert_eq!(fallback_ep_offset(47, 47), 0);
    }

    // ── grab_is_stale ────────────────────────────────────────────────

    fn ts_secs_ago(secs: i64) -> String {
        let t = chrono::Utc::now().naive_utc() - chrono::Duration::seconds(secs);
        t.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    #[test]
    fn grab_is_stale_true_for_age_beyond_threshold() {
        // 5 minutes old, threshold 1 minute → stale.
        let ts = ts_secs_ago(300);
        assert!(grab_is_stale(&ts, 60));
    }

    #[test]
    fn grab_is_stale_false_for_age_within_threshold() {
        // 30 seconds old, threshold 5 minutes → fresh.
        let ts = ts_secs_ago(30);
        assert!(!grab_is_stale(&ts, 300));
    }

    #[test]
    fn grab_is_stale_boundary_exactly_at_threshold_is_not_stale() {
        // Strictly `>` in the impl, so `elapsed == max_age_secs` does
        // NOT count as stale. Pin this so a future tweak of the operator
        // can't silently change the boundary semantics.
        let ts = ts_secs_ago(60);
        // Allow ±1s slack for the inevitable wallclock advance between
        // `ts_secs_ago(60)` and `Utc::now()` inside `grab_is_stale`.
        assert!(grab_is_stale(&ts, 58));
        assert!(!grab_is_stale(&ts, 62));
    }

    #[test]
    fn grab_is_stale_returns_false_on_unparseable_timestamp() {
        // SQLite always emits "YYYY-MM-DD HH:MM:SS"; an unparseable
        // value (corruption, manual edit, ISO-8601 with a `T`) returns
        // false rather than panicking — staleness is an optimization,
        // not a correctness gate.
        assert!(!grab_is_stale("not a date", 60));
        assert!(!grab_is_stale("2026-04-25T12:00:00Z", 60));
        assert!(!grab_is_stale("", 60));
    }

    // ── scan_for_unclassified — early returns and empty cases ────────

    #[tokio::test]
    async fn scan_for_unclassified_returns_zero_when_no_config() {
        // Fresh DB has no `config` row → early-return with all counters at zero.
        let state = crate::test_support::build_test_app_state(
            crate::test_support::in_memory_pool().await,
            None,
        );
        let report = scan_for_unclassified(&state, None).await;
        assert_eq!(report.series_scanned, 0);
        assert_eq!(report.files_scanned, 0);
        assert_eq!(report.files_classified, 0);
        assert_eq!(report.files_needing_review, 0);
    }

    #[tokio::test]
    async fn scan_for_unclassified_returns_zero_when_media_root_empty() {
        // Config row exists but `media_root` is "" → second early return.
        let db = crate::test_support::in_memory_pool().await;
        sqlx::query(
            "INSERT INTO config (id, media_root, post_processing_mode) VALUES (1, '', 'hardlink')",
        )
        .execute(&db)
        .await
        .unwrap();
        let state = crate::test_support::build_test_app_state(db, None);
        let report = scan_for_unclassified(&state, None).await;
        assert_eq!(report.series_scanned, 0);
        assert_eq!(report.files_scanned, 0);
    }

    #[tokio::test]
    async fn scan_series_for_unclassified_bails_on_unknown_series_id() {
        // Single-series fast path with an id that doesn't resolve →
        // early return without touching the filesystem.
        let db = crate::test_support::in_memory_pool().await;
        sqlx::query("INSERT INTO config (id, media_root, post_processing_mode) VALUES (1, '/nonexistent', 'hardlink')")
            .execute(&db).await.unwrap();
        let state = crate::test_support::build_test_app_state(db, None);
        let report = scan_series_for_unclassified(&state, 9999).await;
        assert_eq!(report.series_scanned, 0);
    }

    #[tokio::test]
    async fn scan_for_unclassified_skips_series_with_empty_folder_name() {
        // Tracked series with `folder_name = ''` is skipped without
        // bumping any counter — the loop's first guard.
        let db = crate::test_support::in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().to_string_lossy().into_owned();
        sqlx::query(
            "INSERT INTO config (id, media_root, post_processing_mode) VALUES (1, ?, 'hardlink')",
        )
        .bind(&media_root)
        .execute(&db)
        .await
        .unwrap();
        crate::test_support::seed_series(&db, 1, "Show").await;
        // The seed helper sets a non-empty folder by default; force it
        // to empty so we hit the guard branch deterministically.
        sqlx::query("UPDATE series SET folder_name = ''")
            .execute(&db)
            .await
            .unwrap();

        let state = crate::test_support::build_test_app_state(db, None);
        let report = scan_for_unclassified(&state, None).await;
        assert_eq!(report.series_scanned, 0);
        assert_eq!(report.files_scanned, 0);
    }
}
