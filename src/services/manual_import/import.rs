//! The mutation step of the manual-import wizard (#122, part B).
//!
//! Runs over a `Ready` [`ImportSession`] and, per non-skipped matched
//! series: creates or reuses the `series` row (by AniList / MAL id,
//! through `series::upsert` like the Add Series path), lands every
//! selected file under `<media_root>/<folder>/Season 01/` through
//! `post_processing::do_file_op` in the session's mode, retires the
//! old file through the recycle bin when the preview said "Replace",
//! then hands the folder to `post_processing::scan_series_for_unclassified`
//! for the same classify + tag + history writes an external drop-in
//! gets, and finishes with `write_series_sidecars` (artwork, NFOs).
//!
//! Progress streams through the session's import progress id; the
//! cancel flag on the session is checked between files (never inside
//! a copy, which would leave a partial file at the destination). One
//! import runs at a time ([`IMPORT_LOCK`]): two concurrent jobs could
//! race on folder names and on the post-processing lock.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::Ordering;

use futures_util::FutureExt;

use super::preview::{self, FileStatus, GroupKind, ProjectionContext};
use super::{ImportSession, SeriesGroup, SessionStatus, session};
use crate::AppState;
use crate::models::{
    config, episode_tags, local_metadata, log::LogCategory, metadata_cache, series,
};
use crate::services::monitoring as monitoring_service;
use crate::services::naming;
use crate::services::recycle::{self, RecycleKind};
use crate::services::{library_link, logger, media, metadata_sync, nfo, post_processing, progress};

/// One import at a time. `try_lock`, same shape as the RSS / upgrade
/// locks: a second confirm while one runs gets "already running".
pub static IMPORT_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// The scan job already used the session id as its progress id, and
/// its terminal event may still be buffered when the user confirms.
/// A distinct id keeps the import's toast from consuming it.
pub fn import_progress_id(session_id: &str) -> String {
    format!("{session_id}-import")
}

/// Knobs the tests flip. `hydrate_metadata` gates the AniList detail
/// fetch + airing stamp for newly created series; off, the series
/// row is built from the search result alone and NFOs get the
/// minimal series-row shape.
#[derive(Clone, Copy, Debug)]
pub struct ImportOptions {
    pub hydrate_metadata: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            hydrate_metadata: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupReport {
    pub parsed_title: String,
    pub series_title: String,
    pub anilist_id: i64,
    pub series_id: Option<i64>,
    pub folder_name: String,
    pub created: bool,
    pub written: usize,
    pub replaced: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub series_created: usize,
    pub series_merged: usize,
    /// Groups the job did not touch: skipped, unmatched, or nothing
    /// selected to write.
    pub series_skipped: usize,
    pub files_written: usize,
    pub files_replaced: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub bytes_written: u64,
    pub cancelled: bool,
    pub groups: Vec<GroupReport>,
}

impl ImportReport {
    pub fn files_landed(&self) -> usize {
        self.files_written + self.files_replaced
    }
}

/// Flip the session to `Importing`, take the import lock, and run the
/// job in the background under its own progress id. The caller
/// redirects to the wizard page, which watches that id.
pub async fn start_import(
    state: &AppState,
    session_id: &str,
    opts: ImportOptions,
) -> Result<(), String> {
    let guard = IMPORT_LOCK
        .try_lock()
        .map_err(|_| "An import is already running. Wait for it to finish.".to_string())?;
    let flipped = session::update(&state.import_sessions, session_id, |s| {
        if s.status == SessionStatus::Ready {
            s.status = SessionStatus::Importing;
            s.cancel.store(false, Ordering::Relaxed);
            true
        } else {
            false
        }
    });
    match flipped {
        Some(true) => {}
        Some(false) => return Err("This preview is not ready to import.".to_string()),
        None => return Err("Preview session not found".to_string()),
    }
    let handle = state
        .progress
        .register(import_progress_id(session_id))
        .await;
    let state = state.clone();
    let id = session_id.to_string();
    tokio::spawn(async move {
        // Held for the job's lifetime; released when the task ends.
        let _guard = guard;
        let fut: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<ImportReport, String>> + Send>,
        > = Box::pin(run_import(state.clone(), id.clone(), opts));
        let result = progress::scope(
            handle.clone(),
            std::panic::AssertUnwindSafe(fut).catch_unwind(),
        )
        .await;
        let outcome: Result<ImportReport, String> = match result {
            Ok(r) => r,
            Err(_) => Err("the import crashed; see the server log".to_string()),
        };
        if let Err(msg) = outcome {
            session::update(&state.import_sessions, &id, |s| {
                s.status = SessionStatus::Failed(msg.clone());
            });
            logger::error(
                &state.db,
                LogCategory::Library,
                "Manual import failed",
                &msg,
            )
            .await;
            handle
                .emit("error", "error", "Import failed", Some(msg), true)
                .await;
        }
    });
    Ok(())
}

/// Files already under the series folder whose episodes overlap
/// `episode..episode + count` (a multi-episode file, issue #246, is
/// found by any of its episodes), each with the span it holds. Uses the
/// library scanner rather than a `SxxExx` stem match so an externally
/// named file (`Show - 07.mkv`) is found too.
async fn existing_files_for_episode(
    media_root: &str,
    folder: &str,
    episode: i32,
    count: i32,
) -> Vec<(PathBuf, media::EpisodeSpan)> {
    let last = episode + count.max(1) - 1;
    media::scan_series_folder(media_root, folder)
        .await
        .into_iter()
        .filter(|f| f.episode_number <= last && f.episode_last >= episode)
        .map(|f| {
            let span = f.span();
            (Path::new(media_root).join(folder).join(f.filename), span)
        })
        .collect()
}

/// Series row for a group: the existing one for a merge, or an upsert
/// from the AniList search entry (same fields the Add Series modal
/// posts) for a new series. Returns `(row, created)`.
async fn resolve_series_row(
    db: &sqlx::SqlitePool,
    group: &SeriesGroup,
    title_pref: &str,
) -> Result<(series::Series, bool), String> {
    if let Some(existing) = &group.existing {
        return match series::get_by_id(db, existing.id).await {
            Ok(Some(row)) => Ok((row, false)),
            Ok(None) => Err(format!(
                "{} was removed from the library after the preview",
                existing.title
            )),
            Err(e) => Err(format!("series lookup failed: {e}")),
        };
    }
    let entry = group
        .picked()
        .ok_or_else(|| "no AniList match".to_string())?;
    let title = library_link::pick_title(
        title_pref,
        &entry.title_english,
        &entry.title_romaji,
        &entry.title_native,
    );
    let (id, created) = series::upsert(
        db,
        series::SeriesCore {
            anilist_id: entry.id,
            mal_id: entry.id_mal,
            title,
            title_romaji: &entry.title_romaji,
            title_english: &entry.title_english,
            title_native: &entry.title_native,
            cover_url: &entry.cover_url,
            format: &entry.format,
            status: &entry.status,
            episodes: entry.episodes,
            season_year: entry.season_year,
            end_year: None,
        },
    )
    .await
    .map_err(|e| format!("series upsert failed: {e}"))?;
    let row = series::get_by_id(db, id)
        .await
        .map_err(|e| format!("series lookup failed: {e}"))?
        .ok_or_else(|| "series vanished after upsert".to_string())?;
    Ok((row, created))
}

/// A series phase 1 wrote into, carried to phase 2.
struct Touched {
    row: series::Series,
    created: bool,
    /// `(first episode, episode count, destination)` for every file
    /// that landed.
    landed: Vec<(i32, i32, PathBuf)>,
}

/// The job, in two phases. Phase 1 lands files: per group, resolve
/// the series row (upsert like Add Series), settle the folder, and
/// hardlink / copy / move every selected file. It touches no network,
/// so files arrive fast and a cancel between files loses nothing.
/// Phase 2 finishes each touched series: metadata hydration for new
/// series (the slow part: AniList detail, Jikan episodes, artwork),
/// episode NFOs, the classify + tag scan, and the series sidecars.
/// The scan always runs for a touched series, even after a cancel,
/// so what landed is tagged; hydration and sidecars are skipped past
/// the cancel point and the periodic tasks catch up.
///
/// Sets the session to `Done(report)` itself so a caller awaiting it
/// (tests) sees the same state the page does.
pub async fn run_import(
    state: AppState,
    session_id: String,
    opts: ImportOptions,
) -> Result<ImportReport, String> {
    let state = &state;
    let Some(sess) = session::get(&state.import_sessions, &session_id) else {
        return Err("preview session vanished".to_string());
    };
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| format!("config read failed: {e}"))?
        .unwrap_or_default();
    let media_root = cfg.media_root.trim().to_string();
    if media_root.is_empty() {
        return Err("media root is not set".to_string());
    }
    let mode = sess.mode.as_str();
    let cancel = sess.cancel.clone();

    // Folders owned by a series row: a same-named folder on disk is a
    // merge target, not a collision. Grows as this run creates series.
    let mut owned_folders: HashSet<String> = series::get_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.folder_name)
        .filter(|f| !f.is_empty())
        .collect();

    let total_writes: usize = {
        let disk: HashSet<String> = media::list_media_folders(&media_root).into_iter().collect();
        let ctx = ProjectionContext {
            media_root: &media_root,
            owned_folders: &owned_folders,
            disk_folders: &disk,
            title_pref: &cfg.title_language,
            series_folder_format: &cfg.series_folder_format,
            season_folder_format: &cfg.season_folder_format,
        };
        sess.groups
            .iter()
            .map(|g| preview::project_group(g, &ctx).counts.writes())
            .sum()
    };

    let mut report = ImportReport::default();
    let mut done_files = 0usize;
    let mut touched: Vec<Touched> = Vec::new();
    // One listing of the media root, maintained as this run creates
    // folders, rather than a readdir per group on a runtime worker
    // (a network mount makes that the >5ms blocking case).
    let mut disk: HashSet<String> = media::list_media_folders(&media_root).into_iter().collect();

    // ── Phase 1: land files ─────────────────────────────────────────
    for stored in &sess.groups {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        // Fresh library row + tags per group: the preview's snapshot
        // may be stale (a series added since, or an earlier run of
        // this same session), and projecting from it would re-import
        // what is already there. Fresh folder listing too: earlier
        // groups may have created folders this one collides with.
        let mut group = stored.clone();
        super::resolve_existing(&state.db, &mut group).await;
        let group = &group;
        let ctx = ProjectionContext {
            media_root: &media_root,
            owned_folders: &owned_folders,
            disk_folders: &disk,
            title_pref: &cfg.title_language,
            series_folder_format: &cfg.series_folder_format,
            season_folder_format: &cfg.season_folder_format,
        };
        let view = preview::project_group(group, &ctx);
        let mut gr = GroupReport {
            parsed_title: group.parsed_title.clone(),
            anilist_id: group.picked().map(|e| e.id).unwrap_or(0),
            ..Default::default()
        };
        if !matches!(view.kind, GroupKind::New | GroupKind::Merge) || view.counts.writes() == 0 {
            report.series_skipped += 1;
            gr.skipped = group.files.len();
            gr.series_title = group
                .picked()
                .map(|e| preview::entry_title(e, &cfg.title_language).to_string())
                .unwrap_or_default();
            report.groups.push(gr);
            continue;
        }

        progress::emit(
            "import",
            "info",
            format!("Preparing {}", group.parsed_title),
            Some(format!("{done_files} of {total_writes} files")),
            false,
        )
        .await;

        let (mut row, created) =
            match resolve_series_row(&state.db, group, &cfg.title_language).await {
                Ok(v) => v,
                Err(e) => {
                    gr.errors.push(e);
                    gr.skipped = group.files.len();
                    report.series_skipped += 1;
                    report.groups.push(gr);
                    continue;
                }
            };
        gr.series_id = Some(row.id);
        // Every user-facing string about this series (report row,
        // toast, recycle manifest, log lines) follows the configured
        // title language, not the frozen `series.title` column.
        let series_title = nfo::title_for_preference(&row, &cfg.title_language);
        gr.series_title = series_title.clone();
        gr.created = created;

        // Folder: a created series keeps the upsert's generated name
        // unless an unowned folder of that name already exists (the
        // preview showed the suffixed name); a tracked series with no
        // folder yet gets one the same way.
        if created || row.folder_name.is_empty() {
            let base = if row.folder_name.is_empty() {
                naming::series_folder(
                    &cfg.series_folder_format,
                    &cfg.title_language,
                    &naming::SeriesNames::from_series(&row),
                )
            } else {
                row.folder_name.clone()
            };
            let (folder, _) = preview::unique_folder_name(&base, &ctx);
            if folder != row.folder_name {
                if let Err(e) = series::update_folder(&state.db, row.id, &folder).await {
                    gr.errors.push(format!("could not set folder name: {e}"));
                }
                row.folder_name = folder;
            }
        }
        owned_folders.insert(row.folder_name.clone());
        disk.insert(row.folder_name.clone());
        gr.folder_name = row.folder_name.clone();

        if created {
            // Imports don't grab: the user brought what they have.
            // "future" keeps airing shows moving without a backfill
            // storm across a whole imported library.
            let _ = series::update_monitor_mode(&state.db, row.id, "future").await;
            report.series_created += 1;
        } else {
            report.series_merged += 1;
        }

        let season_dir = Path::new(&media_root)
            .join(&row.folder_name)
            .join(naming::season_folder(
                &cfg.season_folder_format,
                &cfg.title_language,
                &naming::SeriesNames::from_series(&row),
                1,
            ));
        let mut landed: Vec<(i32, i32, PathBuf)> = Vec::new();

        for (file, fv) in group.files.iter().zip(view.files.iter()) {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            let replacing = match fv.status {
                FileStatus::Import => false,
                FileStatus::WouldReplace => true,
                _ => {
                    gr.skipped += 1;
                    report.files_skipped += 1;
                    continue;
                }
            };
            let Some(ep) = file.episode else {
                gr.skipped += 1;
                report.files_skipped += 1;
                continue;
            };
            progress::emit(
                "import",
                "info",
                format!("Importing {} E{:02}", series_title, ep),
                Some(format!("{} of {total_writes} files", done_files + 1)),
                false,
            )
            .await;

            if replacing {
                // Retire the old file(s) first, through the recycle bin
                // when one is configured. A refused recycle must not
                // become an overwrite: skip the file, keep the reason.
                let old = existing_files_for_episode(
                    &media_root,
                    &row.folder_name,
                    ep,
                    file.episode_count,
                )
                .await;
                // The same rule post-processing applies (issue #246): a
                // file that covers fewer episodes than one it would
                // replace is left alone, or the other episodes would go
                // with the old file.
                let last = ep + file.episode_count.max(1) - 1;
                if let Some((_, wider)) = old.iter().find(|(_, s)| s.first < ep || s.last > last) {
                    gr.errors.push(format!(
                        "{}: the file on disk holds {}, more than this file; left alone",
                        file.rel_path,
                        wider.label()
                    ));
                    report.files_failed += 1;
                    done_files += 1;
                    continue;
                }
                let mut retire_failed = None;
                for (old_path, _) in &old {
                    if let Err(e) = recycle::recycle(
                        &state.db,
                        &cfg.recycle_bin_path,
                        RecycleKind::Episode,
                        Some(row.id),
                        &series_title,
                        old_path,
                    )
                    .await
                    {
                        retire_failed = Some(format!("{}: {e}", old_path.display()));
                        break;
                    }
                }
                if let Some(e) = retire_failed {
                    gr.errors
                        .push(format!("{}: could not replace, {e}", file.rel_path));
                    report.files_failed += 1;
                    done_files += 1;
                    continue;
                }
                for held in ep..ep + file.episode_count.max(1) {
                    let _ = episode_tags::mark_grab_history_replaced(&state.db, row.id, held).await;
                    // A pinned episode keeps its override (`clear_episode_tag`
                    // blanks a pinned row's state, which the preview never
                    // offered to do for a second episode).
                    let pinned = group
                        .existing
                        .as_ref()
                        .and_then(|e| e.tags.get(&held))
                        .is_some_and(|t| t.manual_override);
                    if !pinned {
                        let _ = episode_tags::clear_episode_tag(&state.db, row.id, held).await;
                    }
                }
            }

            let dest = season_dir.join(&file.file_name);
            // Backstop for the preview's "Already on disk": a stranger
            // at the destination is never overwritten (do_file_op
            // would unlink it outside the recycle bin). The same-inode
            // case is a re-import of the same file and passes through
            // to do_file_op's no-op.
            if !replacing && dest.exists() && !post_processing::files_share_inode(&file.path, &dest)
            {
                gr.errors.push(format!(
                    "{}: a different file is already at the destination, left alone",
                    file.rel_path
                ));
                report.files_failed += 1;
                done_files += 1;
                continue;
            }
            match post_processing::do_file_op(mode, &file.path, &dest).await {
                Ok(()) => {
                    if replacing {
                        gr.replaced += 1;
                        report.files_replaced += 1;
                    } else {
                        gr.written += 1;
                        report.files_written += 1;
                    }
                    report.bytes_written += file.size_bytes;
                    landed.push((ep, file.episode_count.max(1), dest));
                }
                Err(e) => {
                    gr.errors.push(format!("{}: {e}", file.rel_path));
                    report.files_failed += 1;
                    logger::warn(
                        &state.db,
                        LogCategory::Library,
                        &format!("Manual import: failed to {} {}", mode, file.rel_path),
                        &e.to_string(),
                    )
                    .await;
                }
            }
            done_files += 1;
        }

        if !landed.is_empty() {
            touched.push(Touched {
                row,
                created,
                landed,
            });
        }
        report.groups.push(gr);
        if report.cancelled {
            break;
        }
    }

    // ── Phase 2: finish each touched series ─────────────────────────
    let total_series = touched.len();
    for (i, t) in touched.iter().enumerate() {
        let series_title = nfo::title_for_preference(&t.row, &cfg.title_language);
        let cancelled = cancel.load(Ordering::Relaxed);
        if cancelled {
            report.cancelled = true;
        }
        if t.created && opts.hydrate_metadata && !cancelled {
            progress::emit(
                "finish",
                "info",
                format!("Fetching metadata for {}", series_title),
                Some(format!("{} of {total_series} series", i + 1)),
                false,
            )
            .await;
            if let Err(e) =
                metadata_sync::refresh_series_metadata(&state.db, &t.row, cfg.force_mal_fallback)
                    .await
            {
                logger::warn(
                    &state.db,
                    LogCategory::Library,
                    &format!("Manual import: metadata fetch failed for {}", series_title),
                    &e,
                )
                .await;
            }
        }
        if t.created {
            let _ = monitoring_service::recompute_series_monitoring(&state.db, t.row.id).await;
        }

        let ep_meta = local_metadata::get_episode_map_for_series(&state.db, t.row.id)
            .await
            .unwrap_or_default();
        let runtime_minutes = metadata_cache::get_by_series_id(&state.db, t.row.id)
            .await
            .ok()
            .flatten()
            .and_then(|c| c.detail.duration);
        for (ep, count, dest) in &t.landed {
            // One `<episodedetails>` per episode the file holds (issue
            // #246); the usual file has one.
            let entries: Vec<nfo::EpisodeNfoEntry> = (*ep..*ep + *count)
                .map(|n| {
                    let (title, aired) = ep_meta
                        .get(&n)
                        .map(|m| {
                            let title = if !m.title_english.is_empty() {
                                m.title_english.clone()
                            } else if !m.title.is_empty() {
                                m.title.clone()
                            } else {
                                m.title_romaji.clone()
                            };
                            (title, m.aired.clone())
                        })
                        .unwrap_or_default();
                    nfo::EpisodeNfoEntry {
                        episode: n,
                        title,
                        aired,
                    }
                })
                .collect();
            let _ = nfo::write_multi_episode_nfo(
                &dest.with_extension("nfo"),
                &series_title,
                1,
                &entries,
                runtime_minutes,
            )
            .await;
        }

        progress::emit(
            "finish",
            "info",
            format!("Classifying {}", series_title),
            Some(format!("{} of {total_series} series", i + 1)),
            false,
        )
        .await;
        // Same classify + tag + history writes an external drop-in
        // gets from the library sweep, scoped to this series. Runs
        // even after a cancel so what landed is never left untagged.
        let _ = post_processing::scan_series_for_unclassified(state, t.row.id).await;
        if cancelled {
            continue;
        }
        if let Err(e) = post_processing::write_series_sidecars(state, t.row.id).await {
            logger::warn(
                &state.db,
                LogCategory::Library,
                &format!("Manual import: sidecar write failed for {}", series_title),
                &e,
            )
            .await;
        }
        if t.created && opts.hydrate_metadata {
            let db = state.db.clone();
            let sid = t.row.id;
            tokio::spawn(async move {
                let _ = crate::services::airing_refresh::refresh_for_series(&db, sid).await;
            });
        }
    }

    let summary = format!(
        "root={}, mode={}, created={}, merged={}, written={}, replaced={}, skipped={}, failed={}, bytes={}, cancelled={}",
        sess.root.display(),
        mode,
        report.series_created,
        report.series_merged,
        report.files_written,
        report.files_replaced,
        report.files_skipped,
        report.files_failed,
        report.bytes_written,
        report.cancelled
    );
    logger::info(
        &state.db,
        LogCategory::Library,
        if report.cancelled {
            "Manual import cancelled"
        } else {
            "Manual import complete"
        },
        &summary,
    )
    .await;

    let (kind, title) = if report.cancelled {
        ("warn", "Import cancelled")
    } else if report.files_failed > 0 {
        ("warn", "Import finished with errors")
    } else {
        ("success", "Import complete")
    };
    let body = format!(
        "{} imported, {} replaced, {} skipped, {} failed",
        report.files_written, report.files_replaced, report.files_skipped, report.files_failed
    );
    session::update(&state.import_sessions, &session_id, |s| {
        s.status = SessionStatus::Done(Box::new(report.clone()));
    })
    .ok_or_else(|| "preview session vanished".to_string())?;
    progress::emit("done", kind, title, Some(body), true).await;
    Ok(report)
}

/// Recompute a Ready session's outcome counts for the confirm step.
pub fn writes_for(session: &ImportSession, ctx: &ProjectionContext<'_>) -> usize {
    session
        .groups
        .iter()
        .map(|g| preview::project_group(g, ctx).counts.writes())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::config::{Config, save_config};
    use crate::services::anilist::AnimeEntry;
    use crate::services::manual_import::{CandidateFile, ImportMode, parse::TitleSource};
    use crate::services::source;
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
    use std::fs;

    fn entry(id: i64, english: &str) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: format!("{english} Romaji"),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
            average_score: None,
        }
    }

    fn touch(p: &Path) -> u64 {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let bytes = p.file_name().unwrap().to_string_lossy().into_owned();
        fs::write(p, bytes.as_bytes()).unwrap();
        bytes.len() as u64
    }

    fn candidate(root: &Path, rel: &str, episode: Option<i32>) -> CandidateFile {
        let path = root.join(rel);
        let size = touch(&path);
        let file_name = Path::new(rel)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        CandidateFile {
            path,
            rel_path: rel.to_string(),
            file_name: file_name.clone(),
            size_bytes: size,
            parsed_title: Some("Show".into()),
            title_source: TitleSource::Filename,
            season: None,
            episode,
            year: None,
            group: None,
            quality_label: source::classify_release_sync(&file_name, None).label(),
            selected: true,
            episode_count: 1,
            source_episode: None,
        }
    }

    fn group(files: Vec<CandidateFile>, entry: AnimeEntry) -> SeriesGroup {
        SeriesGroup {
            key: "show".into(),
            parsed_title: "Show".into(),
            season: None,
            tmdb_season: None,
            year: None,
            query: "Show".into(),
            files,
            candidates: vec![entry],
            pick: Some(0),
            low_confidence: false,
            search_error: None,
            skipped: false,
            existing: None,
            resolved_by_id: false,
            mapping_note: None,
            search_results: Vec::new(),
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        media: PathBuf,
        src: PathBuf,
        bin: PathBuf,
        state: AppState,
    }

    async fn fixture(mode: &str, with_bin: bool) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        let src = tmp.path().join("src");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&src).unwrap();
        let db = in_memory_pool().await;
        let cfg = Config {
            media_root: media.to_string_lossy().into_owned(),
            recycle_bin_path: if with_bin {
                bin.to_string_lossy().into_owned()
            } else {
                String::new()
            },
            post_processing_mode: mode.to_string(),
            ..Config::default()
        };
        save_config(&db, &cfg).await.unwrap();
        Fixture {
            _tmp: tmp,
            media,
            src,
            bin,
            state: build_test_app_state(db, None),
        }
    }

    fn ready_session(
        state: &AppState,
        root: &Path,
        mode: ImportMode,
        groups: Vec<SeriesGroup>,
    ) -> String {
        let mut s = ImportSession::new(session::mint_id(), root.to_path_buf(), mode, false, false);
        s.status = SessionStatus::Ready;
        s.stats.files = groups.iter().map(|g| g.files.len()).sum();
        s.groups = groups;
        let id = s.id.clone();
        session::insert(&state.import_sessions, s);
        id
    }

    const OPTS: ImportOptions = ImportOptions {
        hydrate_metadata: false,
    };

    #[cfg(unix)]
    fn same_inode(a: &Path, b: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let (ma, mb) = (fs::metadata(a).unwrap(), fs::metadata(b).unwrap());
        ma.dev() == mb.dev() && ma.ino() == mb.ino()
    }

    #[tokio::test]
    async fn imports_new_series_by_hardlink_and_tags_episodes() {
        let f = fixture("hardlink", false).await;
        let files = vec![
            candidate(&f.src, "Show/[G] Show - 01 [WEB 1080p].mkv", Some(1)),
            candidate(&f.src, "Show/[G] Show - 02 [WEB 1080p].mkv", Some(2)),
            candidate(&f.src, "Show/[G] Show - NCOP.mkv", None),
        ];
        let id = ready_session(
            &f.state,
            &f.src,
            ImportMode::Hardlink,
            vec![group(files, entry(100, "Show: Title"))],
        );

        let report = run_import(f.state.clone(), id.clone(), OPTS).await.unwrap();
        assert_eq!(report.series_created, 1);
        assert_eq!(report.series_merged, 0);
        assert_eq!(report.files_written, 2);
        assert_eq!(report.files_skipped, 1, "NCOP has no episode number");
        assert_eq!(report.files_failed, 0);
        assert!(!report.cancelled);
        assert_eq!(report.groups.len(), 1);
        assert!(report.groups[0].created);
        assert_eq!(report.groups[0].folder_name, "Show_ Title");

        let row = series::get_by_anilist_id(&f.state.db, 100)
            .await
            .unwrap()
            .expect("series created");
        assert_eq!(row.folder_name, "Show_ Title");
        assert_eq!(row.monitor_mode, "future", "imports don't backfill");
        assert_eq!(row.title, "Show: Title");

        let season = f.media.join("Show_ Title").join("Season 01");
        let dest1 = season.join("[G] Show - 01 [WEB 1080p].mkv");
        assert!(dest1.exists());
        assert!(season.join("[G] Show - 02 [WEB 1080p].mkv").exists());
        assert!(!season.join("[G] Show - NCOP.mkv").exists());
        #[cfg(unix)]
        assert!(
            same_inode(&f.src.join("Show/[G] Show - 01 [WEB 1080p].mkv"), &dest1),
            "hardlinked, not copied"
        );
        assert!(
            season.join("[G] Show - 01 [WEB 1080p].nfo").exists(),
            "episode NFO"
        );
        assert!(
            f.media.join("Show_ Title").join("tvshow.nfo").exists(),
            "series NFO"
        );
        assert!(season.join("season.nfo").exists(), "season NFO");

        let tags = episode_tags::get_for_series(&f.state.db, row.id)
            .await
            .unwrap();
        assert_eq!(tags.len(), 2, "one tag per landed episode: {tags:?}");
        assert_eq!(tags[&1].state, "completed");
        assert_eq!(tags[&1].quality_tag, "WEB-1080p");

        match session::get(&f.state.import_sessions, &id).unwrap().status {
            SessionStatus::Done(r) => assert_eq!(*r, report),
            other => panic!("expected Done, got {other:?}"),
        }

        // Re-projecting the same group against the now-tracked series
        // shows nothing left to write: the import is idempotent.
        let mut g = session::get(&f.state.import_sessions, &id)
            .unwrap()
            .groups
            .remove(0);
        crate::services::manual_import::resolve_existing(&f.state.db, &mut g).await;
        assert!(g.existing.is_some());
        let owned: HashSet<String> = [row.folder_name.clone()].into_iter().collect();
        let disk: HashSet<String> = media::list_media_folders(&f.media.to_string_lossy())
            .into_iter()
            .collect();
        let ctx = ProjectionContext {
            media_root: &f.media.to_string_lossy(),
            owned_folders: &owned,
            disk_folders: &disk,
            title_pref: "english",
            series_folder_format: naming::DEFAULT_SERIES_FOLDER_FORMAT,
            season_folder_format: naming::DEFAULT_SEASON_FOLDER_FORMAT,
        };
        let view = preview::project_group(&g, &ctx);
        assert_eq!(view.kind, GroupKind::Merge);
        assert_eq!(view.counts.writes(), 0);
        assert_eq!(view.counts.present, 2);
        let again = run_import(f.state.clone(), id, OPTS).await.unwrap();
        assert_eq!(again.files_written, 0);
        assert_eq!(again.series_skipped, 1);
    }

    #[tokio::test]
    async fn merge_replaces_lower_quality_through_recycle_bin() {
        let f = fixture("hardlink", true).await;
        let sid = seed_series(&f.state.db, 100, "Show").await;
        // What the library holds: a WEB 720p ep 1, tagged and completed.
        let old = f.media.join("Show/Season 01/Show - 01 [WEB 720p].mkv");
        touch(&old);
        let old_class = source::classify_release_sync("Show - 01 [WEB 720p].mkv", None);
        episode_tags::record_grab(
            &f.state.db,
            sid,
            1,
            &old_class,
            "Show - 01 [WEB 720p].mkv",
            "",
            1,
            false,
        )
        .await
        .unwrap();
        episode_tags::mark_completed(&f.state.db, sid, &[1])
            .await
            .unwrap();
        episode_tags::mark_grab_history_completed(
            &f.state.db,
            sid,
            1,
            "Show - 01 [WEB 720p].mkv",
            1,
        )
        .await
        .unwrap();

        let files = vec![
            candidate(&f.src, "Show/[G] Show - 01 [BD 1080p].mkv", Some(1)),
            candidate(&f.src, "Show/[G] Show - 02 [BD 1080p].mkv", Some(2)),
        ];
        let mut g = group(files, entry(100, "Show"));
        crate::services::manual_import::resolve_existing(&f.state.db, &mut g).await;
        assert!(g.existing.is_some(), "resolved by AL id");
        let id = ready_session(&f.state, &f.src, ImportMode::Hardlink, vec![g]);

        let report = run_import(f.state.clone(), id, OPTS).await.unwrap();
        assert_eq!(report.series_merged, 1);
        assert_eq!(report.series_created, 0);
        assert_eq!(report.files_replaced, 1);
        assert_eq!(report.files_written, 1);
        assert_eq!(report.files_failed, 0, "{:?}", report.groups[0].errors);

        assert!(!old.exists(), "old file retired");
        let in_bin = walkdir::WalkDir::new(&f.bin)
            .into_iter()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy() == "Show - 01 [WEB 720p].mkv");
        assert!(in_bin, "old file landed in the recycle bin");
        assert!(
            f.media
                .join("Show/Season 01/[G] Show - 01 [BD 1080p].mkv")
                .exists()
        );
        assert!(
            f.media
                .join("Show/Season 01/[G] Show - 02 [BD 1080p].mkv")
                .exists()
        );

        let tags = episode_tags::get_for_series(&f.state.db, sid)
            .await
            .unwrap();
        assert_eq!(
            tags[&1].quality_tag, "BD-1080p",
            "re-tagged from the new file"
        );
        assert_eq!(tags[&2].quality_tag, "BD-1080p");
        let states: Vec<String> = sqlx::query_scalar(
            "SELECT state FROM episode_grab_history WHERE series_id = ? AND episode_number = 1 ORDER BY id",
        )
        .bind(sid)
        .fetch_all(&f.state.db)
        .await
        .unwrap();
        assert_eq!(
            states,
            vec!["replaced".to_string(), "completed".to_string()]
        );
    }

    #[tokio::test]
    async fn titles_follow_the_configured_language_everywhere() {
        let f = fixture("hardlink", false).await;
        let mut cfg = config::get_config(&f.state.db).await.unwrap().unwrap();
        cfg.title_language = "romaji".into();
        config::save_config(&f.state.db, &cfg).await.unwrap();
        // A tracked series whose frozen `title` is the English one.
        let sid = seed_series(&f.state.db, 100, "Show").await;
        sqlx::query("UPDATE series SET title = 'Show English', title_english = 'Show English', title_romaji = 'Show Romaji' WHERE id = ?")
            .bind(sid)
            .execute(&f.state.db)
            .await
            .unwrap();
        let files = vec![candidate(&f.src, "Show/Show - 01.mkv", Some(1))];
        let mut e = entry(100, "Show English");
        e.title_romaji = "Show Romaji".into();
        let mut g = group(files, e);
        crate::services::manual_import::resolve_existing(&f.state.db, &mut g).await;
        assert_eq!(
            g.existing.as_ref().map(|x| x.title.as_str()),
            Some("Show Romaji"),
            "the merge badge follows the preference, not series.title"
        );
        let id = ready_session(&f.state, &f.src, ImportMode::Hardlink, vec![g]);
        let report = run_import(f.state.clone(), id, OPTS).await.unwrap();
        assert_eq!(report.groups[0].series_title, "Show Romaji");

        // A created series is titled in the preference too.
        let files = vec![candidate(&f.src, "Other/Other - 01.mkv", Some(1))];
        let mut e = entry(200, "Other English");
        e.title_romaji = "Other Romaji".into();
        let id = ready_session(
            &f.state,
            &f.src,
            ImportMode::Hardlink,
            vec![group(files, e)],
        );
        let report = run_import(f.state.clone(), id, OPTS).await.unwrap();
        assert_eq!(report.groups[0].series_title, "Other Romaji");
        let row = series::get_by_anilist_id(&f.state.db, 200)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.title, "Other Romaji");
    }

    #[tokio::test]
    async fn a_stranger_at_the_destination_is_never_overwritten() {
        let f = fixture("copy", false).await;
        seed_series(&f.state.db, 100, "Show").await;
        // Untagged file already sitting where the import would write.
        let dest = f.media.join("Show/Season 01/Show - 01.mkv");
        touch(&dest);
        std::fs::write(&dest, b"the original").unwrap();
        let files = vec![candidate(&f.src, "Show/Show - 01.mkv", Some(1))];
        let mut g = group(files, entry(100, "Show"));
        crate::services::manual_import::resolve_existing(&f.state.db, &mut g).await;
        let id = ready_session(&f.state, &f.src, ImportMode::Copy, vec![g]);
        let report = run_import(f.state.clone(), id, OPTS).await.unwrap();
        assert_eq!(report.files_written, 0, "{:?}", report.groups[0].errors);
        assert_eq!(std::fs::read(&dest).unwrap(), b"the original", "left alone");
        // The preview projects it as "Already on disk" (skipped); the
        // phase-1 backstop covers the case where it did not.
        assert!(
            report.groups[0].skipped == 1
                || report.groups[0]
                    .errors
                    .iter()
                    .any(|e| e.contains("already at the destination")),
            "{:?}",
            report.groups[0]
        );
    }

    #[tokio::test]
    async fn cancelled_before_start_writes_nothing() {
        let f = fixture("hardlink", false).await;
        let files = vec![candidate(&f.src, "Show/Show - 01.mkv", Some(1))];
        let id = ready_session(
            &f.state,
            &f.src,
            ImportMode::Hardlink,
            vec![group(files, entry(100, "Show"))],
        );
        session::get(&f.state.import_sessions, &id)
            .unwrap()
            .cancel
            .store(true, Ordering::Relaxed);
        let report = run_import(f.state.clone(), id, OPTS).await.unwrap();
        assert!(report.cancelled);
        assert_eq!(report.files_written, 0);
        assert!(
            series::get_by_anilist_id(&f.state.db, 100)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!f.media.join("Show").exists());
    }

    #[tokio::test]
    async fn unowned_folder_collision_suffixes_at_import_time() {
        let f = fixture("copy", false).await;
        fs::create_dir_all(f.media.join("Show")).unwrap();
        let files = vec![candidate(&f.src, "Show/Show - 01.mkv", Some(1))];
        let id = ready_session(
            &f.state,
            &f.src,
            ImportMode::Copy,
            vec![group(files, entry(100, "Show"))],
        );
        let report = run_import(f.state.clone(), id, OPTS).await.unwrap();
        assert_eq!(report.groups[0].folder_name, "Show (2)");
        let row = series::get_by_anilist_id(&f.state.db, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.folder_name, "Show (2)");
        assert!(f.media.join("Show (2)/Season 01/Show - 01.mkv").exists());
        assert!(
            !f.media.join("Show/Season 01").exists(),
            "the stranger's folder is untouched"
        );
    }

    #[tokio::test]
    async fn move_mode_removes_the_source() {
        let f = fixture("move", false).await;
        let files = vec![candidate(&f.src, "Show/Show - 01.mkv", Some(1))];
        let id = ready_session(
            &f.state,
            &f.src,
            ImportMode::Move,
            vec![group(files, entry(100, "Show"))],
        );
        let report = run_import(f.state.clone(), id, OPTS).await.unwrap();
        assert_eq!(report.files_written, 1);
        assert!(!f.src.join("Show/Show - 01.mkv").exists(), "moved away");
        assert!(f.media.join("Show/Season 01/Show - 01.mkv").exists());
    }

    #[tokio::test]
    async fn start_import_requires_ready_and_holds_the_lock() {
        let f = fixture("hardlink", false).await;
        let files = vec![candidate(&f.src, "Show/Show - 01.mkv", Some(1))];
        let id = ready_session(
            &f.state,
            &f.src,
            ImportMode::Hardlink,
            vec![group(files, entry(100, "Show"))],
        );
        // Not Ready: refused.
        session::update(&f.state.import_sessions, &id, |s| {
            s.status = SessionStatus::Scanning
        });
        let err = start_import(&f.state, &id, OPTS).await.unwrap_err();
        assert!(err.contains("not ready"), "{err}");
        session::update(&f.state.import_sessions, &id, |s| {
            s.status = SessionStatus::Ready
        });
        // Lock held elsewhere: refused with "already running".
        let guard = IMPORT_LOCK.lock().await;
        let err = start_import(&f.state, &id, OPTS).await.unwrap_err();
        assert!(err.contains("already running"), "{err}");
        drop(guard);
        start_import(&f.state, &id, OPTS).await.unwrap();
        for _ in 0..200 {
            if matches!(
                session::get(&f.state.import_sessions, &id).unwrap().status,
                SessionStatus::Done(_)
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        match session::get(&f.state.import_sessions, &id).unwrap().status {
            SessionStatus::Done(r) => assert_eq!(r.files_written, 1),
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(import_progress_id("abc"), "abc-import");
    }
}
