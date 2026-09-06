use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{
    config, episode_tags, grabbed_torrents, local_metadata, metadata_cache, series,
};
use crate::services::download_client::DownloadClient;
use crate::services::recycle::{self, RecycleKind};
use crate::services::source::{self, SeriesContext};
use crate::services::{logger, media, naming, nfo};

mod artwork_copy;
pub mod client_cleanup;
mod state;

use artwork_copy::{copy_series_and_season_poster, copy_series_banner_and_backdrop};
pub use client_cleanup::{
    remove_stamped_source_paths, sweep_finished_seeds, sweep_finished_seeds_now,
};
use state::fallback_ep_offset;
pub use state::{grab_is_stale, scan_library_for_unclassified, scan_series_for_unclassified};

pub(crate) static POST_PROC_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Process-wide lock for `scan_library_for_unclassified`. Mirrors the
/// `RSS_SYNC_LOCK` / `EXTERNAL_SYNC_LOCK` shape — the supervised 6h
/// sweep awaits it, and the manual Run-now click `try_lock`s and
/// surfaces a friendly busy message instead of interleaving with the
/// supervised tick. Without this, a Run-now click during the
/// supervised cadence flipped the row's `last_started_at` /
/// `last_status` between the two runs' writes (cosmetic flicker on
/// Scheduled Tasks; not data-corrupting since the scan is read-mostly
/// but visible to the user).
pub static LIBRARY_CLASSIFY_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

// Completion and error detection goes through the trait's normalized
// `DownloadItemState` enum (`torrent.state_kind.is_complete()` etc.)
// rather than matching on the raw `state` string — the string is the
// client-native label (qBit: `"stalledUP"`; Deluge: `"Seeding"`), and
// the Phase 1 enum normalizes those into one representation for
// client-agnostic checks. Pre-refactor this function only knew qBit's
// string set, which silently skipped Deluge's completed torrents
// forever (#63 regression).

/// Decide whether a walked file's parsed episode number is one this
/// grab's import should claim.
///
/// SAB's `storage` field can be the parent complete dir (not the
/// per-job folder), so `walk_video_files` may sweep in stranger
/// episodes from sibling jobs. Without a guard, those strangers get
/// hardlinked AND trigger upgrade-replace on existing grabs for those
/// episodes — which incorrectly flips unrelated grabs to `replaced`.
///
/// Permissive cases (return `true`):
///   - `is_batch = 1` — batches legitimately span many episodes.
///   - File is covered by a route row — Phase-2 sibling-routed
///     imports are allowed to import any episode the route claims.
///   - `episode_numbers` is empty — legacy grabs from before the
///     column was reliably populated. Permissive-on-empty preserves
///     backward compatibility for those rows; tightening to
///     "everything must claim explicitly" would silently break
///     imports for any grab whose episode list never got stamped.
///
/// Strict case: parsed `raw_ep_num` must appear in
/// `grab.episode_numbers`.
pub(crate) fn grab_claims_episode(
    is_batch: bool,
    file_in_routes: bool,
    grab_episode_numbers: &[i32],
    raw_ep_num: i32,
) -> bool {
    is_batch
        || file_in_routes
        || grab_episode_numbers.is_empty()
        || grab_episode_numbers.contains(&raw_ep_num)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedEpisode {
    raw_episode: i32,
    episode: i32,
}

fn resolve_episode(
    parsed_season: Option<i32>,
    raw_episode: i32,
    route_offset: Option<i32>,
    cumulative_prior_episodes: i32,
) -> Result<ResolvedEpisode, String> {
    let episode_offset = route_offset.unwrap_or_else(|| {
        if parsed_season.is_some() {
            0
        } else {
            fallback_ep_offset(raw_episode, cumulative_prior_episodes)
        }
    });
    let episode = raw_episode - episode_offset;
    if episode <= 0 {
        return Err(format!(
            "episode {} minus offset {} is non-positive",
            raw_episode, episode_offset
        ));
    }
    Ok(ResolvedEpisode {
        raw_episode,
        episode,
    })
}

/// Resolved batch plan from [`validate_batch_episode_map`].
#[derive(Debug, Default)]
pub(crate) struct BatchPlan {
    /// File index → destination slot for every file the loop imports.
    pub(crate) slots: HashMap<usize, ResolvedEpisode>,
    /// File index → index of the higher-version sibling that won the
    /// same slot (issue #204). The loop skips these with an info log.
    pub(crate) superseded: HashMap<usize, usize>,
}

/// Validate every wanted video in a batch before the import loop performs
/// upgrades or file operations. Inputs carry the exact route and series
/// cumulative offset used by execution, and the resolved plan returned here is
/// consumed by the loop. Validation and execution therefore cannot drift onto
/// different destination slots.
///
/// Unparseable and non-positive files are excluded from the plan rather than
/// failing it: batches routinely ship NCOP/NCED/PV/CM/menu extras that parse
/// to `None` by design, and an excluded file moves nothing, so skipping is
/// non-destructive. (The import loop re-derives the same verdict for files
/// absent from the plan and logs a per-file warning with series context.)
///
/// Two files resolving to the same destination slot are compared by release
/// version (issue #204): `E05` + `E05v2` keeps the v2 and lists the v1 under
/// `superseded`, since a group re-releasing one episode inside its own pack
/// is routine. Candidates that tie on version (true duplicates, or a
/// mis-parse) still fail the whole batch before any mutation — that
/// ambiguity has no safe per-file answer, and importing either file could
/// destroy the other.
pub(crate) fn validate_batch_episode_map(
    files: &[(usize, i64, Option<i32>, i32, String)],
) -> Result<BatchPlan, String> {
    // (series, episode) → candidates in file order. BTreeMap so the
    // error names a deterministic slot when several collide.
    struct Candidate<'a> {
        file_idx: usize,
        version: u32,
        name: &'a str,
    }
    let mut by_slot: std::collections::BTreeMap<(i64, i32), Vec<Candidate<'_>>> =
        std::collections::BTreeMap::new();
    let mut resolved_by_idx: HashMap<usize, ResolvedEpisode> = HashMap::new();

    for (file_idx, series_id, route_offset, cumulative_prior_episodes, name) in files {
        let filename = Path::new(name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name);
        let lower = filename.to_lowercase();
        let Some((parsed_season, raw_episode)) = media::parse_episode_number(&lower) else {
            continue;
        };
        let Ok(resolved) = resolve_episode(
            parsed_season,
            raw_episode,
            *route_offset,
            *cumulative_prior_episodes,
        ) else {
            continue;
        };
        // No version token reads as v1 so `E05` + `E05v2` compare 1 vs 2.
        let version = media::parse_release_version(&lower).unwrap_or(1);
        by_slot
            .entry((*series_id, resolved.episode))
            .or_default()
            .push(Candidate {
                file_idx: *file_idx,
                version,
                name: filename,
            });
        resolved_by_idx.insert(*file_idx, resolved);
    }

    let mut plan = BatchPlan::default();
    for ((series_id, episode), mut candidates) in by_slot {
        if candidates.len() > 1 {
            // Highest version first; file order breaks ties so the
            // error below names the same pair on every run.
            candidates.sort_by(|a, b| b.version.cmp(&a.version).then(a.file_idx.cmp(&b.file_idx)));
            if candidates[0].version == candidates[1].version {
                return Err(format!(
                    "batch preflight mapped both '{}' and '{}' to series {} episode {}; no files were changed",
                    candidates[0].name, candidates[1].name, series_id, episode
                ));
            }
        }
        let winner_idx = candidates[0].file_idx;
        plan.slots.insert(winner_idx, resolved_by_idx[&winner_idx]);
        for loser in candidates.into_iter().skip(1) {
            plan.superseded.insert(loser.file_idx, winner_idx);
        }
    }

    Ok(plan)
}

pub(crate) fn requires_episode_map_preflight(is_batch: bool, video_file_count: usize) -> bool {
    is_batch || video_file_count > 1
}

/// Return the wanted video indices only when every one is complete. Unwanted
/// files are never imported, and a partially complete wanted batch waits rather
/// than committing a partial library state.
pub(crate) fn ready_wanted_video_indices(
    files: &[crate::services::download_client::DownloadFile],
) -> Result<Vec<usize>, String> {
    let wanted: Vec<(usize, &crate::services::download_client::DownloadFile)> = files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.wanted && is_video_file(&file.name))
        .collect();
    if wanted.is_empty() {
        return Err("no wanted video files are visible yet".to_string());
    }
    let incomplete = wanted
        .iter()
        .filter(|(_, file)| file.progress < 1.0)
        .count();
    if incomplete > 0 {
        return Err(format!(
            "{} of {} wanted video files are incomplete",
            incomplete,
            wanted.len()
        ));
    }
    Ok(wanted.into_iter().map(|(idx, _)| idx).collect())
}

/// Validate an untrusted relative path fragment from a download client's
/// file-list API (`DownloadClient::get_files`) before joining it onto a
/// trusted base path.
///
/// `Path::join` with an absolute path **replaces** the base entirely
/// rather than appending — `Path::new("/data").join("/etc/passwd")`
/// resolves to `/etc/passwd`, not `/data/etc/passwd`. Likewise, parent-dir
/// components walk up out of the base. A torrent's file metadata can put
/// arbitrary strings in its file-list entries, so the post-processing
/// import loop must reject any name that would escape the source base
/// before composing the source path.
///
/// Cross-platform note: `Path::components` parses path syntax for the
/// current target. On Unix, `C:\Windows\foo` is a single `Normal`
/// component because `\` is not a separator — the raw-byte `\` check
/// catches Windows-style paths even on a Unix build, so the validator
/// behaves the same regardless of build target.
pub(crate) fn validate_relative_path_fragment(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("empty path fragment");
    }
    if name.contains('\\') {
        return Err("contains backslash (windows-style path separator)");
    }
    for component in Path::new(name).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => return Err("contains parent-dir component"),
            Component::RootDir => return Err("absolute path (starts with root)"),
            Component::Prefix(_) => return Err("contains windows path prefix"),
        }
    }
    Ok(())
}

pub(crate) fn is_video_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    matches!(
        Path::new(&lower)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
        "mkv" | "mp4" | "avi" | "wmv" | "webm" | "m4v" | "ts"
    )
}

/// Walk `root` recursively and synthesize `DownloadFile` entries for
/// every video file found. Returns the names RELATIVE to `root` so
/// the caller can compose `root + name` to get an absolute path
/// matching the BT-shape `save_path + file.name` convention the
/// import loop already uses. Used as a fallback when a download
/// client's `get_files` returns empty for a completed torrent —
/// notably SAB, whose `mode=get_files` API only works for queue
/// items and returns nothing once a job moves to history.
///
/// All synthesized entries set `progress: 1.0` (anything visible
/// on disk has finished downloading by definition) and `wanted:
/// true`. Sizes come from `metadata.len()`, matching what
/// `is_video_file`'s caller does post-import.
///
/// Recursion guard: hard-stops at `MAX_WALK_DEPTH = 4` so a
/// pathological symlink loop or a deeply-nested archive can't hang
/// the import path. Real-world SAB extractions are 1 directory
/// deep (`<storage>/<filename.mkv>`); BT clients with multi-file
/// torrents go 2-3 deep at most.
pub(crate) fn walk_video_files(root: &Path) -> Vec<crate::services::download_client::DownloadFile> {
    const MAX_WALK_DEPTH: u32 = 4;
    let mut out = Vec::new();
    fn recurse(
        root: &Path,
        cur: &Path,
        depth: u32,
        max_depth: u32,
        out: &mut Vec<crate::services::download_client::DownloadFile>,
    ) {
        if depth > max_depth {
            return;
        }
        let Ok(entries) = std::fs::read_dir(cur) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                recurse(root, &path, depth + 1, max_depth, out);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let Some(name_str) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_video_file(name_str) {
                continue;
            }
            // Build the path relative to `root` so the caller's
            // `Path::new(&source_base).join(&file.name)` resolves
            // back to the absolute path. `strip_prefix` always
            // succeeds here because `path` is descended from `root`.
            let rel = path
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| name_str.to_string());
            out.push(crate::services::download_client::DownloadFile {
                name: rel,
                size: metadata.len() as i64,
                progress: 1.0,
                wanted: true,
            });
        }
    }
    recurse(root, root, 0, MAX_WALK_DEPTH, &mut out);
    out
}

/// True when `a` and `b` resolve to the same inode (same device + same
/// inode number). Used by [`do_file_op`] to detect "src and dst already
/// point at the same bytes" cases — a re-import on top of a previously
/// hardlinked dst, or a misconfiguration that resolves both paths to
/// the same file. Without an early-out, the hardlink mode falls through
/// to `fs::copy` on `EEXIST`, and `fs::copy` on a self-overlapping path
/// truncates the shared inode to zero bytes (the read fd reads from the
/// inode the write fd just truncated). That corrupts both the user's
/// media file *and* the seeding source the torrent client still
/// references.
///
/// On non-Unix targets we fall back to plain path equality, which still
/// catches the misconfiguration case (caller passed identical paths)
/// but misses the "hardlinked elsewhere" case. Windows hardlinks are
/// rarer in this codebase's deployment shape (Docker / Linux is the
/// primary target) and the path-identity check is sufficient for the
/// catastrophic-data-loss scenario.
#[cfg(unix)]
pub(crate) fn files_share_inode(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(am), Ok(bm)) => am.dev() == bm.dev() && am.ino() == bm.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
pub(crate) fn files_share_inode(a: &Path, b: &Path) -> bool {
    a == b
}

/// Hardlink → copy fallback. For "move" mode: rename → copy+delete fallback.
///
/// Runs the whole operation under `spawn_blocking` because a Blu-ray
/// episode cross-device copy can easily be 1–4 GB and blocks for
/// multiple seconds; doing that on a tokio worker starves the RSS sync,
/// HTTP handlers, and other background tasks sharing the same runtime.
pub(crate) async fn do_file_op(mode: &str, src: &Path, dst: &Path) -> std::io::Result<()> {
    let mode = mode.to_string();
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        // No-op when src and dst already point at the same bytes — by
        // a prior hardlink import landing the same file twice, or by a
        // misconfiguration that resolves both to the same path. All
        // three modes need this guard: a self-overlapping `fs::copy`
        // truncates the shared inode (corrupting hardlink + copy modes)
        // and the move-mode cross-fs fallback's `remove_file(src)`
        // after the rename would delete the only surviving copy.
        if files_share_inode(&src, &dst) {
            return Ok(());
        }
        match mode.as_str() {
            "move" => {
                // Same-fs rename is atomic and instant — the happy path.
                if std::fs::rename(&src, &dst).is_ok() {
                    return Ok(());
                }
                // Cross-fs fallback: copy to a sibling tmp first then
                // rename onto dst so a partially-copied file can't be
                // observed at dst by a subsequent pass and mistaken for
                // a finished import. Cleans up the tmp on rename failure.
                let mut tmp = dst.as_os_str().to_os_string();
                tmp.push(".ryokan-tmp");
                let tmp = PathBuf::from(tmp);
                std::fs::copy(&src, &tmp)?;
                if let Err(e) = std::fs::rename(&tmp, &dst) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                // Source-remove failure is rare (qBit still holds the
                // file open, source dir is read-only, etc.) and the
                // file is safely at dst either way — but surface a warn
                // so the operator can spot duplicate state in qBit's
                // downloads directory.
                if let Err(e) = std::fs::remove_file(&src) {
                    tracing::warn!(
                        target: "ryokan::post_processing",
                        src = %src.display(),
                        error = %e,
                        "post-copy remove_file failed; file remains at source AND destination",
                    );
                }
                Ok(())
            }
            "copy" => {
                std::fs::copy(&src, &dst)?;
                Ok(())
            }
            _ => {
                // "hardlink" (default): hardlink preferred, copy on
                // failure (cross-fs). `std::fs::hard_link` does NOT
                // overwrite — it returns `EEXIST` if dst already exists.
                // Clean any pre-existing dst first so a re-import
                // doesn't fall through to the `fs::copy` fallback (which
                // would silently degrade to a real copy, breaking the
                // seed-safe-via-shared-inode property the user picked
                // hardlink mode for). The same-inode short-circuit
                // above already handled the "dst is the same file as
                // src via prior hardlink" case; reaching here means
                // dst is a different file we're free to replace.
                if dst.exists() {
                    let _ = std::fs::remove_file(&dst);
                }
                if std::fs::hard_link(&src, &dst).is_err() {
                    std::fs::copy(&src, &dst)?;
                }
                Ok(())
            }
        }
    })
    .await
    .map_err(|e| std::io::Error::other(format!("join error: {}", e)))?
}

/// Sibling temp name an upgrade lands at before the swap (issue #202):
/// `.<basename>.ryokan-new` in the same directory as `dest`, so the
/// final step is an atomic rename. The leading dot matters: retiring
/// the old file goes through `recycle::recycle`, whose companion sweep
/// takes everything prefixed `<stem>.`, and a `<dest>.ryokan-new` name
/// would be swept away with the file it is about to replace. Not a
/// video extension, so library scans ignore a leftover from a crash
/// mid-swap; the next upgrade of the same slot lists it with the old
/// files and retires it.
pub(crate) fn staging_path(dest: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(dest.file_name().unwrap_or_default());
    name.push(".ryokan-new");
    dest.with_file_name(name)
}

/// Undo a staged placement after the old file could not be retired
/// (issue #202). Hardlink / copy modes still have the source, so the
/// staged file is simply removed. Move mode has already consumed the
/// source; the staged file is moved back so a retry sees the original
/// layout, and if even that fails the staged path is named in the log
/// so the file is recoverable by hand.
async fn unstage_upgrade(db: &sqlx::SqlitePool, mode: &str, landing: &Path, src: &Path) {
    let outcome = if mode == "move" {
        do_file_op("move", landing, src).await
    } else {
        tokio::fs::remove_file(landing).await
    };
    if let Err(e) = outcome {
        logger::error(
            db,
            LogCategory::PostProcess,
            &format!(
                "Could not undo the staged upgrade file {}",
                landing.display()
            ),
            &e.to_string(),
        )
        .await;
    }
}

/// Build the naming context for one episode (#124). Quality and group
/// come from the grab-time `episode_quality_tags` row when present
/// (manual overrides included), otherwise from a filename-only pass
/// over the source name; both are the same pre-download signals the
/// grab itself was scored on.
fn episode_name_context(
    ctx: &SeriesImportCtx,
    ep_num: i32,
    ep_title: &str,
    source_name: &str,
    ext: &str,
) -> naming::NameContext {
    let tag = ctx.existing_tags.get(&ep_num);
    let (resolution, source_label, group) = match tag {
        Some(t) if !t.source.is_empty() || !t.resolution.is_empty() => (
            if t.resolution.eq_ignore_ascii_case("unknown") {
                String::new()
            } else {
                t.resolution.clone()
            },
            naming::quality_source_label(&t.source, t.is_remux, &t.web_kind),
            t.release_group.clone(),
        ),
        _ => {
            let c = source::classify_release_sync(source_name, None);
            let resolution = match c.resolution {
                crate::services::source::Resolution::Unknown => String::new(),
                r => r.as_str().to_string(),
            };
            let group = tag
                .map(|t| t.release_group.clone())
                .filter(|g| !g.is_empty())
                .or_else(|| {
                    crate::services::source_filename::classify_filename(source_name).release_group
                })
                .unwrap_or_default();
            (
                resolution,
                naming::quality_source_label(c.source.as_str(), c.is_remux, c.web_kind.as_str()),
                group,
            )
        }
    };
    let group = if group.is_empty() {
        crate::services::source_filename::classify_filename(source_name)
            .release_group
            .unwrap_or_default()
    } else {
        group
    };
    naming::NameContext {
        series_title: ctx.series_title.clone(),
        series_year: ctx.series.season_year,
        season_number: 1,
        episode_number: ep_num,
        episode_title: ep_title.to_string(),
        quality_resolution: resolution,
        quality_source: source_label,
        release_group: group,
        ext: ext.to_string(),
    }
}

struct SeriesImportCtx {
    series: series::Series,
    folder_name: String,
    series_title: String,
    season_dir: PathBuf,
    ep_meta: HashMap<i32, local_metadata::CachedEpisodeMetadata>,
    /// Cached AniList detail used to enrich episode + series NFOs with
    /// plot, genres, runtime, etc. `None` when the per-series metadata
    /// cache is empty — the NFO writers fall back to the minimal
    /// series-row-only shape.
    cached_detail: Option<crate::services::anilist::AnimeDetail>,
    runtime_minutes: Option<i32>,
    /// Snapshot of `episode_quality_tags` for this series at import
    /// start. Used by the per-file post-download reclassify path to
    /// decide whether to UPDATE in place vs INSERT a new row, and to
    /// log the prior source for diagnostics. Refreshed once per
    /// series-ctx build — safe because within a single import pass
    /// each episode is written at most once, so later files can't
    /// depend on earlier files' writes landing in this map.
    existing_tags: HashMap<i32, episode_tags::EpisodeQualityTag>,
}

/// Resolve the [`SeriesImportCtx`] for `series_id`: loads the series
/// row, materializes its folder name + season directory, and warms up
/// the episode metadata and AniList detail caches. Split out of
/// [`import_torrent`] so a multi-series routed batch can reuse the same
/// context across files without re-running the expensive preamble.
async fn load_series_import_ctx(
    state: &AppState,
    cfg: &config::Config,
    series_id: i64,
) -> Result<SeriesImportCtx, String> {
    let series = series::get_by_id(&state.db, series_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("series {} not found", series_id))?;

    // Auto-generate folder_name from the series-folder template (#124)
    // if it was never set.
    let folder_name = if series.folder_name.is_empty() {
        if nfo::best_title(&series).trim().is_empty() {
            return Err(format!(
                "series '{}' has no usable title for folder name",
                series.title
            ));
        }
        let generated = naming::series_folder(
            &cfg.series_folder_format,
            &cfg.title_language,
            &naming::SeriesNames::from_series(&series),
        );
        // Persist it so future imports skip this path.
        let _ = series::update_folder(&state.db, series.id, &generated).await;
        generated
    } else {
        series.folder_name.clone()
    };

    // `series_title` flows into `<showtitle>` in every episode NFO and
    // into the renamed filename stem, so it respects the user's
    // `title_language` preference. `folder_name` above stays on
    // `best_title` because it's a one-time persisted default — later
    // preference changes should not rename folders.
    let series_title = nfo::title_for_preference(&series, &cfg.title_language);
    let season_dir = Path::new(&cfg.media_root)
        .join(&folder_name)
        .join(naming::season_folder(
            &cfg.season_folder_format,
            &cfg.title_language,
            &naming::SeriesNames::from_series(&series),
            1,
        ));

    {
        let season_dir = season_dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&season_dir))
            .await
            .map_err(|e| format!("create season dir join: {}", e))?
            .map_err(|e| format!("create season dir: {}", e))?;
    }

    let ep_meta = local_metadata::get_episode_map_for_series(&state.db, series.id)
        .await
        .unwrap_or_default();

    // Cached AniList detail. Used to enrich both series and episode NFOs
    // (plot, year, rating, runtime, real genres) so Jellyfin doesn't have
    // to scrape its own metadata. Optional — falls back to the minimal
    // series-row-only NFO when the cache is empty.
    let cached_detail = metadata_cache::get_by_series_id(&state.db, series.id)
        .await
        .ok()
        .flatten()
        .map(|c| c.detail);
    let runtime_minutes = cached_detail.as_ref().and_then(|d| d.duration);

    // Load the full `episode_quality_tags` snapshot once here instead
    // of inside the per-file import loop. Previously this ran N times
    // per batch (one fetch per file) even though each file only writes
    // its own episode row and no inter-file read dependency exists.
    let existing_tags = episode_tags::get_for_series(&state.db, series.id)
        .await
        .unwrap_or_default();

    Ok(SeriesImportCtx {
        series,
        folder_name,
        series_title,
        season_dir,
        ep_meta,
        cached_detail,
        runtime_minutes,
        existing_tags,
    })
}

/// Process a single completed torrent. Returns `true` if at least one file was
/// imported, `false` if there was nothing to do yet.
///
/// Outcome of an [`import_torrent`] call. Replaces the older
/// `Result<bool, String>` shape so the caller can distinguish four
/// states the prior bool flattened together:
///
/// - `NotReady` — torrent complete but no video files visible yet
///   (qBit still finalizing, or the pack is all samples/.nfo). Leave
///   the grab pending so the next tick retries.
/// - `Imported` — every video file landed.
/// - `PartiallyImported { failed_episodes }` — some files imported,
///   some failed (typically transient: disk full mid-copy, source
///   vanished, permission flake on one file). Mark the grab imported
///   so the user sees the partial success in the Downloads page, but
///   surface a single SUMMARY error log naming the missing episodes
///   so the failure is greppable in System → Logs rather than buried
///   among per-file errors.
/// - `AllFailed { failed_episodes }` — files were attempted but every
///   `do_file_op` failed. Mark the grab failed rather than leaving it
///   pending forever — the previous Ok(false) collapse meant a disk-
///   full pack would re-attempt every tick, generating duplicate
///   error logs without ever escalating to a user-visible failure.
pub(crate) enum ImportOutcome {
    NotReady,
    Imported,
    PartiallyImported { failed_episodes: Vec<i32> },
    AllFailed { failed_episodes: Vec<i32> },
}

/// Phase 2: if the grab has routing rows in `grabbed_torrent_series`
/// (written by the auto-expand path when a megapack contained sibling
/// entries), each file is routed to the sibling's own library folder
/// instead of the parent's. Grabs without routes fall through to the
/// legacy single-series behavior where every file targets
/// `grab.series_id`.
async fn import_torrent(
    state: &AppState,
    cfg: &config::Config,
    grab: &grabbed_torrents::GrabbedTorrent,
    torrent_hash: &str,
    torrent_save_path: &str,
    // Multi-client routing — the client this grab landed on, threaded
    // through from `run_once`'s per-grab resolution. Pre-PR-F this was
    // re-fetched here as `default_download_client()`, which silently
    // routed `get_files` to the wrong client for any pinned grab.
    client: &std::sync::Arc<dyn DownloadClient>,
) -> Result<ImportOutcome, String> {
    let mut files = client
        .get_files(torrent_hash)
        .await
        .map_err(|e| format!("get torrent files: {}", e))?;

    // Phase 2: look up per-file routing rows written by the auto-expand
    // path. A non-empty result means this grab was an auto-expanded
    // batch and each file is tagged with the sibling series_id it
    // belongs to; an empty result is the legacy path where every file
    // routes to `grab.series_id` (pre-Phase-2 grabs, or Phase-2 grabs
    // where sibling detection returned nothing).
    let mut routes = grabbed_torrents::get_series_routes(&state.db, grab.id)
        .await
        .unwrap_or_default();

    // Grab-time auto-expand can fail when qBit's metadata wait times
    // out on a slow tracker (see the 180s wait in
    // `handlers::library::search::auto_expand_library_from_pack`). By
    // import time the file list is always available — if the grab was
    // a batch and no routes were written, retry sibling detection now
    // so siblings still land in their own folders instead of every
    // file falling back to the parent. Motivating case (#45): the
    // HorribleSubs JoJo P3 48-ep pack, where the grab-time wait timed
    // out and Egypt-hen never got auto-added.
    if routes.is_empty() && grab.is_batch && grab.series_id > 0 {
        match metadata_cache::get_by_series_id(&state.db, grab.series_id).await {
            Ok(Some(cached)) => {
                let filenames: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
                let parent_eps: Vec<i32> = cached
                    .detail
                    .episodes
                    .filter(|n| *n > 0 && *n <= 1000)
                    .map(|n| (1..=n).collect())
                    .unwrap_or_default();
                // Synthetic grab context. The per-episode tag rows
                // `expand_from_files` writes for new siblings get
                // their classifications overwritten by
                // `classify_post_download` further down, so
                // `ClassificationResult::unknown()` is fine here.
                // Release group and size are recoverable via post-
                // download paths too.
                let ctx = crate::services::auto_expand::AutoExpandGrabContext {
                    classification: crate::services::source::ClassificationResult::unknown(),
                    release_group: String::new(),
                    size_bytes: 0,
                };
                let _ = crate::services::auto_expand::expand_from_files(
                    &state.db,
                    &filenames,
                    &cached.detail,
                    grab.series_id,
                    &parent_eps,
                    grab.id,
                    &grab.torrent_name,
                    &ctx,
                )
                .await;
                // Reload regardless of return value: `expand_from_files`
                // writes routes when it detects siblings even if those
                // siblings were already tracked (added=0 but routes
                // written).
                routes = grabbed_torrents::get_series_routes(&state.db, grab.id)
                    .await
                    .unwrap_or_default();
            }
            Ok(None) => {
                // Rare but possible: a grab landed before the metadata
                // sync populated the cache for this series. Log so
                // operators can trace "batch imported but siblings
                // never added" without reading the code.
                logger::debug(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Auto-expand retry skipped for '{}' — no cached AniList detail for parent series_id={}",
                        grab.torrent_name, grab.series_id,
                    ),
                    "",
                )
                .await;
            }
            Err(e) => {
                logger::debug(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Auto-expand retry skipped for '{}' — metadata_cache lookup failed for parent series_id={}",
                        grab.torrent_name, grab.series_id,
                    ),
                    &e.to_string(),
                )
                .await;
            }
        }
    }

    // file_idx → (target series_id, episode_offset), flattened from
    // the routes table. `episode_offset` is subtracted from each
    // file's parsed episode number before the file is renamed /
    // tagged so absolute-numbered batches (e.g. smol Monogatari
    // S07E14 → Owari S2 E01 with offset 13) land under the correct
    // arc-local episode number. Offset is 0 for siblings with arc-
    // local numbering and for all legacy routes (via COALESCE in the
    // model read).
    let routes_by_file: HashMap<usize, (i64, i32)> = routes
        .iter()
        .flat_map(|r| {
            let series_id = r.series_id;
            let offset = r.episode_offset;
            r.file_indices
                .iter()
                .map(move |i| (*i, (series_id, offset)))
        })
        .collect();

    // Preserve the canonical qBit file index alongside each entry so
    // completed files can be correlated back to their route row. qBit
    // returns files in a deterministic order keyed by file index, so
    // `enumerate()` applied to the untouched `files` vec yields the
    // same indices that `detect_sibling_entries_in_pack` recorded at
    // grab time.
    // Determine the source base path. Prefer the torrent's own
    // `save_path` (already translated by the caller via
    // `translate_client_path` to host-view) over the configured
    // per-client download path. Two reasons:
    //   1. SAB's `save_path` is the per-job extracted directory
    //      (e.g. `/downloads/complete/[Erai-raws].One.Piece-1158]/`).
    //      Using that as source_base + filename lands at the actual
    //      .mkv. Using per_client_download_path (the parent
    //      `complete/` folder) would point at a non-existent file.
    //   2. For BT clients with per-category save paths (Deluge
    //      "Move completed on label", qBit per-category save paths)
    //      each torrent reports its own `save_path` extending a
    //      common base. Using save_path preserves the category
    //      subdir; using per_client_download_path flattens it.
    // Fall back to per_client_download_path only when save_path is
    // empty (rare edge case — some clients may report empty
    // save_path mid-metadata-fetch).
    let per_client_download_path = crate::services::download_client::per_client_download_path(cfg);
    let source_base = if !torrent_save_path.is_empty() {
        torrent_save_path.to_string()
    } else {
        per_client_download_path.to_string()
    };

    // Some clients (notably SAB) don't expose a per-file API for
    // completed jobs — `mode=get_files&value=<nzo_id>` only works
    // while the slot is in the queue, returning an empty list once
    // the job moves to history. Without a fallback, completed SAB
    // imports got stuck in NotReady forever. When `get_files` came
    // back empty AND the source directory exists locally, walk it
    // for video files. Synthesizes `DownloadFile` entries with
    // `progress: 1.0` (everything we see on disk has finished
    // downloading by definition) so the rest of the import loop
    // works unchanged.
    if files.is_empty() {
        let walk_root = Path::new(&source_base).to_path_buf();
        if walk_root.is_dir() {
            // Recursive sync read_dir; cross the >5ms threshold easily on
            // a SAB BD-pack with hundreds of files. Hop to the blocking
            // pool so the supervised post-processing tick doesn't stall
            // the runtime while filesystem I/O waits.
            let walk_root_for_blocking = walk_root.clone();
            files = tokio::task::spawn_blocking(move || walk_video_files(&walk_root_for_blocking))
                .await
                .unwrap_or_default();
        } else {
            // SAB's `canonical_job_path` case-3 candidate (constructed
            // as `<storage>/<title>/` when SAB reports the parent
            // complete dir as `storage`) doesn't always resolve on
            // Ryokan's host view. Surface the miss here so a user
            // reporting "SAB job completed but never imported" has a
            // log line to grep for; without it the grab silently
            // stays pending and `client.get_files()` empty leaves no
            // breadcrumb for what went wrong.
            tracing::debug!(
                source_base = %source_base,
                grab_id = grab.id,
                hash = %grab.hash,
                "import_torrent: source_base is not a directory on Ryokan's view — \
                 SAB-canonical path may not resolve through download_path translation"
            );
        }
    }

    // Reject attacker-controlled path fragments before readiness and batch
    // planning. Unsafe entries are not part of the importable video set: they
    // must neither block a legitimate sibling nor make an otherwise single
    // video look like a batch. Keep the original vector untouched so route
    // indices still match the download client's canonical file indices.
    let mut files_for_readiness = files.clone();
    let mut unsafe_video_indices = HashSet::new();
    for (file_idx, file) in files.iter().enumerate() {
        if !file.wanted || !is_video_file(&file.name) {
            continue;
        }
        if let Err(reason) = validate_relative_path_fragment(&file.name) {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Rejected suspicious file-list entry '{}' from grab #{}: {}",
                    file.name, grab.id, reason
                ),
                &format!("hash={}", grab.hash),
            )
            .await;
            files_for_readiness[file_idx].wanted = false;
            unsafe_video_indices.insert(file_idx);
        }
    }

    let wanted_video_indices = match ready_wanted_video_indices(&files_for_readiness) {
        Ok(indices) => indices,
        Err(reason) => {
            // #27 — log this at debug rather than silently looping. qBit
            // reported the torrent complete but its wanted file list may
            // still be finalizing. Waiting for the whole wanted video set is
            // what prevents a batch from being marked imported after only a
            // completed subset landed.
            logger::debug(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Wanted video files are not ready for '{}' — retrying next tick",
                    grab.torrent_name
                ),
                &reason,
            )
            .await;
            return Ok(ImportOutcome::NotReady);
        }
    };
    let video_files: Vec<(usize, &crate::services::download_client::DownloadFile)> =
        wanted_video_indices
            .iter()
            .map(|file_idx| (*file_idx, &files[*file_idx]))
            .collect();

    // Resolve and validate the complete batch mapping before loading series
    // context, deleting an upgrade target, or moving/copying any source.
    // Duplicate destination slots fail the whole import without mutation;
    // unparseable extras (NCOP/PV/CM/menu files) are merely absent from the
    // plan and get skipped with a per-file warning in the loop below.
    // Trust the files we actually received over the stored classifier. Older
    // grabs can predate (or have missed) batch classification; allowing a
    // multi-video import through the single-file path would reintroduce the
    // duplicate-destination overwrite that this preflight prevents.
    let batch_episode_plan = if requires_episode_map_preflight(grab.is_batch, video_files.len()) {
        let mut cumulative_by_series = HashMap::new();
        let mut batch_files = Vec::with_capacity(video_files.len());
        for (file_idx, file) in &video_files {
            let route = routes_by_file.get(file_idx).copied();
            let target_series_id = route
                .map(|(series_id, _)| series_id)
                .unwrap_or(grab.series_id);
            let cumulative_prior_episodes =
                if let Some(value) = cumulative_by_series.get(&target_series_id) {
                    *value
                } else {
                    let value = series::get_by_id(&state.db, target_series_id)
                        .await
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("series {} not found", target_series_id))?
                        .cumulative_prior_episodes;
                    cumulative_by_series.insert(target_series_id, value);
                    value
                };
            batch_files.push((
                *file_idx,
                target_series_id,
                route.map(|(_, offset)| offset),
                cumulative_prior_episodes,
                file.name.clone(),
            ));
        }
        validate_batch_episode_map(&batch_files)?
    } else {
        BatchPlan::default()
    };

    // Lazily-loaded per-series context cache. The single-series case
    // fills exactly one entry; a multi-series routed batch fills one
    // entry per sibling touched.
    let mut series_ctx_cache: HashMap<i64, SeriesImportCtx> = HashMap::new();
    // Unique series_ids that had at least one file successfully
    // imported — drives the per-series NFO/poster write after the loop.
    let mut touched_series: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    // Per-target-series tuple (episode number, individual file size,
    // post-processed on-disk file name) for every file we successfully
    // landed. Replaces the `grab.episode_numbers`-based mark_completed
    // at the end of the loop: bare batch grabs arrive here with an
    // empty episode list on the parent grab row, but we've already
    // parsed ep_num per file above so we can mark completed with the
    // real list instead.
    //
    // The per-file size feeds `mark_grab_history_completed`'s
    // non-batch-only size-refine path (batch rows retain their whole-
    // torrent total so the episode detail modal can show "from an
    // X GiB batch"). The on-disk file name feeds the same function so
    // each per-episode row carries the Sonarr-style renamed basename
    // (e.g. `Jujutsu Kaisen - S01E06 - Hidden Inventory.mkv`) instead
    // of the batch torrent's release title.
    // BTreeMap for deterministic iteration order downstream — the
    // post-loop `mark_completed` / `mark_grab_history_completed` pass
    // runs in series_id ascending order every run, matching
    // `touched_series`'s BTreeSet so log interleaving is stable and
    // greppable. Functionally equivalent to HashMap; pure log hygiene.
    let mut imported_eps_by_series: std::collections::BTreeMap<i64, Vec<(i32, i64, String)>> =
        std::collections::BTreeMap::new();
    let mut imported_count = 0_usize;
    // Source-side paths we successfully imported FROM. Persisted on
    // the grab row at the end of this function so the delete and
    // series-remove handlers can clean up SAB's complete dir
    // regardless of whether the inode-based fallback applies (only
    // hardlink mode shares inodes; copy mode has separate inodes,
    // move mode has no surviving source). Captured in
    // local-translated form (Ryokan's view of the path).
    let mut imported_source_paths: Vec<String> = Vec::new();
    // Episode numbers (post-offset, the same value the rest of the
    // codebase uses) that reached `do_file_op` but failed. Drives the
    // PartiallyImported / AllFailed branches below so partial failures
    // surface as a single summary log rather than scattered per-file
    // errors. Files that never reached the file op (couldn't parse an
    // episode number, offset produced a non-positive result, series
    // context failed to load) skip via `continue` and aren't counted —
    // those paths already log their own warning and aren't retryable
    // by re-running the file op.
    let mut failed_episodes: Vec<i32> = Vec::new();

    // Old grab ids we've marked as replaced during this import pass,
    // paired with the new `grab.id` that superseded them. Deduped via
    // HashSet so a batch that covers 12 episodes doesn't issue 12
    // identical UPDATEs against the same old grab row. Flushed once
    // after the file loop.
    let mut grabs_to_mark_replaced: std::collections::HashSet<i64> =
        std::collections::HashSet::new();

    for (file_idx, file) in &video_files {
        debug_assert!(!unsafe_video_indices.contains(file_idx));
        // Route this file: prefer the routes table (Phase 2 batch
        // auto-expansion), fall back to `grab.series_id` for legacy
        // grabs and for any completed video file whose index wasn't
        // covered by a route (e.g. extension mismatch between
        // `auto_search::is_media_filename` and [`is_video_file`]).
        //
        // `ep_offset` is computed below, once `ctx` is loaded and the
        // filename is parsed — the legacy fallback needs
        // `series.cumulative_prior_episodes` (#30) to pick the right
        // offset for absolute-numbered releases like
        // `[SubsPlease] Jujutsu Kaisen - 56` (which must land as S3 E9,
        // not S3 E56).
        let target_series_id = routes_by_file
            .get(file_idx)
            .map(|(sid, _)| *sid)
            .unwrap_or(grab.series_id);

        // Can't use the clean `Entry::or_insert_with_async` pattern
        // because the loader is async and `entry()` borrows the map
        // across the await. Branching on the Entry variant keeps the
        // hot path (cache hit) to a single lookup and only calls the
        // loader on a cold miss.
        let ctx = match series_ctx_cache.entry(target_series_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                match load_series_import_ctx(state, cfg, target_series_id).await {
                    Ok(ctx) => entry.insert(ctx),
                    Err(e) => {
                        logger::error(
                            &state.db,
                            LogCategory::PostProcess,
                            &format!("Failed to load series context for id={}", target_series_id),
                            &e,
                        )
                        .await;
                        continue;
                    }
                }
            }
        };

        // Reject malicious file-list entries from the download client
        // before composing the source path. `file.name` originates in
        // torrent metadata and is fully attacker-controlled — without
        // this guard, an entry like `/etc/passwd` (or `../../escape`)
        // would let `Path::join` resolve outside `source_base` and the
        // hardlink/copy/move op would touch a host file the user never
        // intended to import. See issue #117.
        if let Err(reason) = validate_relative_path_fragment(&file.name) {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Rejected suspicious file-list entry '{}' from grab #{}: {}",
                    file.name, grab.id, reason
                ),
                &format!("hash={}", grab.hash),
            )
            .await;
            continue;
        }

        let src: PathBuf = Path::new(&source_base).join(&file.name);

        // Defense-in-depth: after the join, canonicalize both sides and
        // confirm the resolved source still lives under the resolved
        // base. Catches symlink games (a `legit.mkv` entry that resolves
        // to a symlink pointing at `/etc/passwd`) and any string-level
        // oversight the validator above might miss. Permissive on
        // canonicalize errors — the file may not yet exist on this
        // node's view, in which case `do_file_op` surfaces the real I/O
        // error downstream and there's nothing for an attacker to
        // dereference anyway.
        //
        // **TOCTOU residual**: there's still a race window between this
        // canonicalize check and the eventual `fs::rename` /
        // `fs::hard_link` inside `do_file_op`. A co-resident attacker
        // with write access to the source-base directory tree could
        // swap a legitimate `release.mkv` for a symlink to `/etc/passwd`
        // after this check passes but before the file op runs. The
        // string-level `validate_relative_path_fragment` (the loop's
        // first defense) is what handles the attacker-controlled-
        // metadata case from issue #117 fully — that's the primary
        // defense and runs before any FS access. Closing the residual
        // would require `O_NOFOLLOW`-style FD discipline through the
        // file ops, which is out of scope for the path-traversal fix
        // and doesn't apply to Ryokan's threat model anyway (the
        // process user already owns the source path; co-resident
        // attacker with write access there is a deeper compromise).

        if let (Ok(canon_src), Ok(canon_base)) =
            (src.canonicalize(), Path::new(&source_base).canonicalize())
            && !canon_src.starts_with(&canon_base)
        {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Rejected file '{}' from grab #{}: resolves outside source base \
                     ({} -> {})",
                    file.name,
                    grab.id,
                    canon_base.display(),
                    canon_src.display()
                ),
                &format!("hash={}", grab.hash),
            )
            .await;
            continue;
        }

        let filename_only = Path::new(&file.name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&file.name);

        // Batch imports consume the exact plan validated before this loop;
        // a batch file absent from the plan was preflight-skipped
        // (unparseable or non-positive) and re-derives the same verdict
        // here, landing in the warn-and-continue arms below. Singles use
        // the same resolver, retaining the legacy first-episode fallback
        // only when exactly one video exists.
        // Issue #204: a lower-version sibling of the file that won the
        // same slot in the batch preflight. Nothing to import; the
        // higher version lands instead.
        if let Some(winner) = batch_episode_plan.superseded.get(file_idx) {
            let winner_name = video_files
                .iter()
                .find(|(idx, _)| idx == winner)
                .and_then(|(_, f)| Path::new(&f.name).file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("a higher version");
            logger::info(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Skipping '{}': superseded by '{}' in the same batch",
                    filename_only, winner_name
                ),
                &format!("series={}", ctx.series.title),
            )
            .await;
            continue;
        }

        let resolved = if let Some(resolved) = batch_episode_plan.slots.get(file_idx) {
            *resolved
        } else {
            let parsed = media::parse_episode_number(&filename_only.to_lowercase());
            let fallback = if video_files.len() == 1 && routes_by_file.is_empty() {
                grab.episode_numbers.first().copied()
            } else {
                None
            };
            let Some((parsed_season, raw_episode)) =
                parsed.or_else(|| fallback.map(|episode| (None, episode)))
            else {
                logger::warn(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Could not parse episode number from '{}'", filename_only),
                    &format!("series={}", ctx.series.title),
                )
                .await;
                continue;
            };
            match resolve_episode(
                parsed_season,
                raw_episode,
                routes_by_file.get(file_idx).map(|(_, offset)| *offset),
                ctx.series.cumulative_prior_episodes,
            ) {
                Ok(resolved) => resolved,
                Err(reason) => {
                    logger::warn(
                        &state.db,
                        LogCategory::PostProcess,
                        &format!("Skipping '{}' — {}", filename_only, reason),
                        &format!("series={}", ctx.series.title),
                    )
                    .await;
                    continue;
                }
            }
        };
        let raw_ep_num = resolved.raw_episode;
        let ep_num = resolved.episode;

        // Skip stranger files. See `grab_claims_episode` doc for the
        // full rationale and matrix of cases.
        let claims_this_episode = grab_claims_episode(
            grab.is_batch,
            routes_by_file.contains_key(file_idx),
            &grab.episode_numbers,
            raw_ep_num,
        );
        if !claims_this_episode {
            logger::debug(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Skipping stranger file '{}' (parsed ep {}) — grab #{} only claims {:?}",
                    filename_only, raw_ep_num, grab.id, grab.episode_numbers
                ),
                "",
            )
            .await;
            continue;
        }

        let ep_title = ctx
            .ep_meta
            .get(&ep_num)
            .map(|m| {
                if !m.title_english.is_empty() {
                    m.title_english.clone()
                } else if !m.title.is_empty() {
                    m.title.clone()
                } else {
                    m.title_romaji.clone()
                }
            })
            .unwrap_or_default();

        let aired = ctx
            .ep_meta
            .get(&ep_num)
            .map(|m| m.aired.clone())
            .unwrap_or_default();

        let ext = Path::new(filename_only)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mkv");

        let season = 1_i32;

        // Destination name from the episode-file template (#124). The
        // quality and group tokens read the grab-time tag row when there
        // is one (it may carry a manual override), else a filename-only
        // classification of the source; the post-download reclassify
        // below still runs on the landed file, it just doesn't rename.
        let name_ctx = episode_name_context(ctx, ep_num, &ep_title, filename_only, ext);
        let episode_name = naming::episode_file(&cfg.episode_file_format, &name_ctx);
        if episode_name.truncated {
            logger::info(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Shortened the file name for S{:02}E{:02} of '{}' to fit the filesystem limit",
                    season, ep_num, ctx.series.title
                ),
                &episode_name.file_name,
            )
            .await;
        }
        let dest_video = ctx.season_dir.join(&episode_name.file_name);
        let dest_nfo = ctx.season_dir.join(format!("{}.nfo", episode_name.stem));

        // Existing files for this episode slot (any extension). Matched
        // by parsing each name back through `parse_episode_number`
        // rather than by a fixed `SxxExx` substring, so the check works
        // for every template the validator accepts (it requires the
        // sample name to parse back) and still catches files named under
        // an earlier template or an episode title that changed between
        // grabs.
        // Walk the season directory off the runtime — a big season pack on
        // an NFS mount can make the sync read_dir/stat calls block for
        // hundreds of ms. The filter logic is cheap CPU, so we also move
        // it into the spawned task.
        let existing_for_ep: Vec<PathBuf> = {
            let season_dir = ctx.season_dir.clone();
            tokio::task::spawn_blocking(move || -> Vec<PathBuf> {
                std::fs::read_dir(&season_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        let is_nfo = p
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| e.eq_ignore_ascii_case("nfo"));
                        !is_nfo
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .and_then(|n| media::parse_episode_number(&n.to_lowercase()))
                                .is_some_and(|(s, e)| e == ep_num && s.is_none_or(|s| s == season))
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
        };

        // Issue #202: on an upgrade, land the new file beside the
        // destination FIRST and only then retire the old one. The
        // previous order (recycle old, delete old torrent, then place)
        // left the user with no old file and no old torrent data when
        // the placement failed (cross-fs copy interrupted, disk full,
        // permissions), and with no recycle bin configured that loss
        // was permanent. Staging at `.<basename>.ryokan-new` makes the
        // failure mode "nothing changed": hardlink mode pays nothing
        // extra, copy / move pay the same cost they always did, just
        // before the old file is touched. The swap itself is a
        // same-directory rename, and the old torrent leaves the client
        // only after the swap.
        let is_upgrade = !existing_for_ep.is_empty();
        let landing = if is_upgrade {
            staging_path(&dest_video)
        } else {
            dest_video.clone()
        };
        let placed = do_file_op(&cfg.post_processing_mode, &src, &landing).await;

        if placed.is_ok() && is_upgrade {
            // Check if this is an upgrade replacing a previously imported file.
            // If an older imported grab exists for this episode, this is an
            // upgrade — remove the old file and old torrent, then import the new one.
            //
            // Using `target_series_id` (the routed sibling) rather than
            // `grab.series_id` (the parent) is what makes per-sibling
            // upgrade detection work: `find_imported_for_episode`
            // unions across the legacy grabbed_torrents column and the
            // routes table, so a prior sibling-routed import still
            // surfaces here.
            let old_grabs =
                grabbed_torrents::find_imported_for_episode(&state.db, target_series_id, ep_num)
                    .await
                    .unwrap_or_default();

            // No matching prior grab row but disk has a file for this
            // SxxExx slot — treat it as an **orphan upgrade**. The disk
            // state is ground truth: a file exists, the user is grabbing
            // something new, they expect the new file to replace what's
            // there. Covers three historical shapes where the DB row
            // doesn't line up:
            //   1. Legacy batch grabs whose `episode_numbers` was
            //      mis-parsed from the release title before the current
            //      batch_episode_numbers logic existed (e.g. Kaizoku
            //      Season 3 packs stored as [3] instead of [1..12] —
            //      `find_imported_for_episode(series, 1)` misses them).
            //   2. Files manually dropped into the library from outside
            //      Ryokan (pre-existing rips, migration from another
            //      PVR) — no grab row ever existed.
            //   3. The original grab's row is in state='pending'
            //      (torrent stuck, crash mid-import) — not 'imported',
            //      so find_imported skips it.
            // The `mark_replaced` step is skipped when old_grabs is
            // empty — the new row simply replaces on disk without a
            // chain pointer. The replacing grab still shows up as
            // 'imported' in history; there's just no "replaced by"
            // backlink because nothing in the DB was the predecessor.
            if old_grabs.is_empty() {
                logger::info(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Orphan upgrade: '{}' replacing S{:02}E{:02} file on disk (no prior imported grab)",
                        filename_only, season, ep_num
                    ),
                    &format!(
                        "series_id={}, existing_files={}, grab_id={}",
                        target_series_id,
                        existing_for_ep.len(),
                        grab.id
                    ),
                )
                .await;
            }

            // Retire the old file(s) to make way for the upgrade. Recycle
            // bin (#123): each old video moves into the bin with its
            // companions (the old NFO's stem may differ from the new
            // dest_stem when the episode title changed between grabs, so
            // the companion sweep keyed on the old stem is what catches
            // it); with no bin configured this is the permanent unlink
            // the upgrade path always did.
            let mut retire_failed = false;
            for old_file in &existing_for_ep {
                if let Err(e) = recycle::recycle(
                    &state.db,
                    &cfg.recycle_bin_path,
                    RecycleKind::Episode,
                    Some(target_series_id),
                    &ctx.series.title,
                    old_file,
                )
                .await
                {
                    retire_failed = true;
                    logger::error(
                        &state.db,
                        LogCategory::PostProcess,
                        &format!(
                            "Failed to remove old file for upgrade: {}",
                            old_file.display()
                        ),
                        &e,
                    )
                    .await;
                }
            }
            // A refused recycle (bin configured but not writable) must not
            // turn into an overwrite: the swap below would replace the
            // file the bin was supposed to keep. Undo the staging, skip
            // this episode; the grab stays partially imported and the log
            // names the reason.
            if retire_failed {
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Upgrade for S{:02}E{:02} of '{}' skipped: the old file could not be recycled",
                        season, ep_num, ctx.series.title
                    ),
                    &grab.torrent_name,
                )
                .await;
                unstage_upgrade(&state.db, &cfg.post_processing_mode, &landing, &src).await;
                // Issue #118: a stalled upgrade is an import failure the
                // user wants to hear about, same as a failed placement.
                crate::services::notifications::emit_import_failed(
                    state,
                    target_series_id,
                    Some(ep_num),
                    &src.display().to_string(),
                    "the old file could not be recycled, so the upgrade was skipped",
                )
                .await;
                failed_episodes.push(ep_num);
                continue;
            }

            // Swap the staged file into place. Same directory, so this
            // is a rename; the old files are retired, so the
            // destination is free. A failure here is exotic (the
            // directory changed under us) and leaves the new file at
            // the staged path, named in the log.
            if let Err(e) = tokio::fs::rename(&landing, &dest_video).await {
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Upgrade for S{:02}E{:02} of '{}' stalled: the new file is staged at {} but could not be renamed into place",
                        season,
                        ep_num,
                        ctx.series.title,
                        landing.display()
                    ),
                    &e.to_string(),
                )
                .await;
                crate::services::notifications::emit_import_failed(
                    state,
                    target_series_id,
                    Some(ep_num),
                    &src.display().to_string(),
                    &format!(
                        "the new file is staged at {} but could not be renamed into place: {e}",
                        landing.display()
                    ),
                )
                .await;
                failed_episodes.push(ep_num);
                continue;
            }

            logger::info(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Replacing S{:02}E{:02} of '{}' with upgraded release",
                    season, ep_num, ctx.series.title
                ),
                &format!("old_grabs={}", old_grabs.len()),
            )
            .await;

            // Clean up old torrents from the download client and mark old
            // grabs as replaced. Reuse the `client` binding cloned at the
            // top of this function instead of re-taking
            // `state.download_client.read()` each iteration — under a big
            // upgrade with many old grabs the per-iteration lock acquire
            // was serializing against any other task touching
            // `state.download_client`.
            //
            // `mark_replaced` (not `mark_removed`) so the Downloads
            // history keeps the upgrade chain: state='replaced' with
            // `replaced_by_grab_id = grab.id`. Without this distinction
            // users who got their existing SubsPlease episodes silently
            // swapped out by a Kaizoku batch had no way to tell the
            // upgrade actually happened — old rows looked identical to
            // user-cancelled grabs.
            // `client.delete` still runs inside the per-episode loop
            // because it's cheap-ish (one RPC per torrent) and the old
            // hash may repeat across per-episode finds — but qBit's
            // delete is idempotent on an already-removed hash, so the
            // repeat is harmless. The expensive SQL UPDATE for
            // `mark_replaced` is deferred to a post-loop flush so a
            // batch grab that covers 12 episodes doesn't UPDATE the
            // same old grab 12 times.
            for old_grab in &old_grabs {
                if !old_grab.hash.is_empty() {
                    // Issue #28 — preserve PT seed rules
                    // across upgrade-replace. The old torrent has
                    // imported and is seeding to its per-tracker
                    // ratio; deleting it mid-seed could ding the
                    // user's tracker ratio. The grab row still
                    // gets `mark_replaced` below so the upgrade
                    // sweep doesn't re-grab.
                    if grabbed_torrents::respects_seed_rules(&state.db, &old_grab.hash).await {
                        logger::info(
                            &state.db,
                            LogCategory::DownloadClient,
                            &format!(
                                "Skipping client delete for upgraded torrent {} (respect_seed_rules)",
                                old_grab.torrent_name
                            ),
                            &old_grab.hash,
                        )
                        .await;
                    } else {
                        // Route the OLD grab's delete to the OLD
                        // grab's client — not the NEW grab's client
                        // bound above. A cross-protocol upgrade
                        // (SAB→qBit or qBit→SAB) would otherwise
                        // hit a client that doesn't know the old
                        // hash and silently leave the old job behind.
                        // `resolve_grab_client` also rescues legacy
                        // NULL-stamped SAB grabs via the nzo_id-shape
                        // heuristic.
                        let target = state
                            .resolve_grab_client(old_grab.download_client_id, &old_grab.hash)
                            .await;
                        if let Some(target) = target {
                            let _ = target.delete(&old_grab.hash, true).await;
                        }
                    }
                }
                grabs_to_mark_replaced.insert(old_grab.id);
            }

            // Per-episode history counterpart: flip the old grab's
            // episode_grab_history row for this specific ep from
            // 'completed' to 'replaced' so the episode detail modal
            // mirrors what the Downloads tab shows. Without this the
            // old Kaizoku row and the new SubsPlease row both read
            // 'completed' in grab history, hiding the upgrade chain.
            // Stays inside the loop since episode_grab_history is
            // keyed on (series_id, episode_number) — one UPDATE per
            // episode is correct, not redundant.
            let _ =
                episode_tags::mark_grab_history_replaced(&state.db, target_series_id, ep_num).await;
        }

        match placed {
            Ok(()) => {
                let _ = nfo::write_episode_nfo(
                    &dest_nfo,
                    &ctx.series_title,
                    season,
                    ep_num,
                    &ep_title,
                    &aired,
                    ctx.runtime_minutes,
                )
                .await;
                imported_count += 1;
                // The early `claims_this_episode` guard above already
                // filtered out stranger files for this grab — so any
                // file reaching this point is a legitimate import,
                // and its source path is safe to stamp.
                imported_source_paths.push(src.display().to_string());
                touched_series.insert(target_series_id);
                logger::info(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Imported S{:02}E{:02} of '{}'",
                        season, ep_num, ctx.series.title
                    ),
                    &format!(
                        "mode={} dest={}",
                        cfg.post_processing_mode,
                        dest_video.display()
                    ),
                )
                .await;
                // Post-download re-classification (Layers 5 + 6). Runs ffprobe
                // on the landed file and walks the series directory for BD
                // artifacts, then upserts episode_quality_tags.
                // Rows with manual_override = 1 are left alone by the DB
                // helpers so user tags stick.
                let series_root = Path::new(&cfg.media_root).join(&ctx.folder_name);
                // Snapshot loaded once per series in `load_series_import_ctx`;
                // see `SeriesImportCtx::existing_tags` for why refreshing
                // per-file is unnecessary.
                let existing_row = ctx.existing_tags.get(&ep_num);
                let pre_source = existing_row.map(|t| t.source.clone()).unwrap_or_default();
                let row_exists = existing_row.is_some();
                let post = source::classify_post_download(
                    &state.db,
                    &dest_video,
                    Some(&series_root),
                    &grab.torrent_name,
                    Some(SeriesContext {
                        status: &ctx.series.status,
                        season_year: ctx.series.season_year,
                        end_year: ctx.series.end_year,
                    }),
                    grab.is_batch,
                )
                .await;
                // Batch grabs often arrive here with no pre-existing tag
                // row: the "Grab batch" dropdown and interactive batch
                // paths skip `episode_tags::record_grab` because they
                // don't know which episodes are in the pack until
                // post-processing parses the filenames. `update_classification`
                // is UPDATE-only, so in that case it would silently
                // affect 0 rows and the episode stays UNKNOWN in the
                // UI despite the classifier correctly identifying it.
                // Branch on row existence: UPSERT via `record_grab`
                // for the no-row case (same pattern
                // `scan_library_for_unclassified` uses for externally
                // imported files), UPDATE in-place via
                // `update_classification` otherwise.
                // Issue #118 — fire `ClassifierNeedsReview` when the
                // post-download classifier flips this row into needs-
                // review. Default-off in the per-event matrix because
                // a reclassify sweep can produce hundreds of rows in
                // a short window; users who want it opt in. The emit
                // is keyed on `post.needs_review` (the in-memory
                // ClassificationResult), not the DB row, since the
                // helper below would have to re-read.
                if post.needs_review {
                    let verdict = post.label();
                    // The event field is i32 representing the
                    // percent (0..=100). `post.confidence` is f32 in
                    // [0.0, 1.0], so cast directly truncates 0.50
                    // to 0 — Discord then renders "0%" for any
                    // sub-1.0 verdict, which is every needs-review
                    // case. Multiply by 100 first so the value sent
                    // matches the percent the user expects.
                    let confidence_pct = (post.confidence * 100.0).round() as i32;
                    crate::services::notifications::emit_classifier_needs_review(
                        state,
                        target_series_id,
                        ep_num,
                        confidence_pct,
                        &verdict,
                    )
                    .await;
                }

                let persist_result = if row_exists {
                    // `update_classification` stamps
                    // classification_attempted_at internally.
                    episode_tags::update_classification(&state.db, target_series_id, ep_num, &post)
                        .await
                } else {
                    let inserted = episode_tags::record_grab(
                        &state.db,
                        target_series_id,
                        ep_num,
                        &post,
                        &grab.torrent_name,
                        "",
                        file.size,
                        grab.is_batch,
                    )
                    .await
                    .map(|_| ());
                    // Issue #53: post-classify call of `record_grab` —
                    // explicitly stamp the attempt timestamp so the
                    // library scan won't keep retrying this row if
                    // `post` came back UNKNOWN. Grab-time `record_grab`
                    // call sites (search.rs, auto_expand.rs, etc.) do
                    // NOT stamp — they're filename-only and the file
                    // hasn't landed yet.
                    let _ = episode_tags::stamp_classification_attempted(
                        &state.db,
                        target_series_id,
                        ep_num,
                    )
                    .await;
                    inserted
                };
                if let Err(e) = persist_result {
                    logger::warn(
                        &state.db,
                        LogCategory::PostProcess,
                        &format!(
                            "Post-download tag persist failed for S{:02}E{:02}",
                            season, ep_num
                        ),
                        &e.to_string(),
                    )
                    .await;
                } else {
                    logger::debug(
                        &state.db,
                        LogCategory::PostProcess,
                        &format!(
                            "Post-download classify S{:02}E{:02}: {} (conf={:.2})",
                            season,
                            ep_num,
                            post.label(),
                            post.confidence
                        ),
                        &format!(
                            "pre={}, post={}, row_existed={}",
                            pre_source,
                            post.source.as_str(),
                            row_exists
                        ),
                    )
                    .await;
                    // If the post-download classifier flipped into needs_review,
                    // surface at INFO so the user can find it in the review list.
                    if post.needs_review {
                        logger::info(
                            &state.db,
                            LogCategory::PostProcess,
                            &format!(
                                "Needs review: {} S{:02}E{:02}",
                                ctx.series.title, season, ep_num
                            ),
                            &format!(
                                "post-download classification {} flagged for review",
                                post.label()
                            ),
                        )
                        .await;
                    }
                }

                // Issue #118 — fire `Imported` per-file. Deliberately
                // sequenced AFTER `update_classification` / `record_grab`
                // so the DB lookup inside `emit_imported` reads the
                // post-download `quality_tag`. Pre-classify the row
                // either had the grab-time tag (often UNKNOWN) or no
                // row at all, which surfaced as an empty Quality field
                // in the Discord embed and a missing `quality_tag`
                // string in the webhook JSON.
                //
                // Quality tag is best-effort: if `update_classification`
                // / `record_grab` errored above, the helper's lookup
                // falls back to `COALESCE(quality_tag, '') = ""` and
                // the event ships with an empty tag rather than
                // skipping the dispatch. The file did land — users
                // legitimately want the import notification even when
                // the persist sidecar errored, and the empty-tag UX is
                // the same as a true UNKNOWN classification.
                crate::services::notifications::emit_imported(
                    state,
                    target_series_id,
                    ep_num,
                    &src.display().to_string(),
                    &dest_video.display().to_string(),
                )
                .await;

                let dest_basename = dest_video
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(filename_only)
                    .to_string();
                imported_eps_by_series
                    .entry(target_series_id)
                    .or_default()
                    .push((ep_num, file.size, dest_basename));
            }
            Err(e) => {
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("File op failed for '{}'", filename_only),
                    &e.to_string(),
                )
                .await;
                // Issue #118 — fire `ImportFailed` per-file. Captures
                // the file-op error string (cross-fs copy failure,
                // permission denied, disk full, ENOSPC). Episode
                // number is known here because the `claims_this_episode`
                // guard above resolved it.
                crate::services::notifications::emit_import_failed(
                    state,
                    target_series_id,
                    Some(ep_num),
                    &src.display().to_string(),
                    &e.to_string(),
                )
                .await;
                // A copy that died part-way (disk full, the hardlink
                // fallback's `fs::copy`) leaves a partial file at the
                // landing path: the hidden `.ryokan-new` on an upgrade,
                // the destination itself on a first import. Neither is
                // a finished file; drop it so the slot reads as empty on
                // the next pass. Best effort: the same-inode guard means
                // a landing that is the source's own link never gets
                // here with `Err`.
                if landing.is_file() && !files_share_inode(&src, &landing) {
                    let _ = tokio::fs::remove_file(&landing).await;
                }
                failed_episodes.push(ep_num);
            }
        }
    }

    if imported_count == 0 {
        // Distinguish "no video files visible yet" (handled earlier as
        // NotReady) from "video files were attempted but every one
        // failed." The latter shouldn't sit pending forever.
        if failed_episodes.is_empty() {
            return Ok(ImportOutcome::NotReady);
        }
        return Ok(ImportOutcome::AllFailed { failed_episodes });
    }

    // Persist the source-side paths so the delete + series-remove
    // handlers can clean up SAB's complete dir for copy/move modes
    // (no shared inode) or for hardlink mode when SAB's
    // `del_files=1` doesn't reach the file (the user's reported
    // bug — SAB's history `storage` field can be the parent
    // complete dir while the actual extracted .mkv lives in a
    // subfolder created by the rar archive contents).
    let _ =
        grabbed_torrents::stamp_imported_source_paths(&state.db, grab.id, &imported_source_paths)
            .await;

    // Flush the `grabbed_torrents.state = 'replaced'` updates collected
    // during the file loop. One UPDATE per distinct old grab instead
    // of one-per-episode so a batch that covered 12 episodes doesn't
    // run 12 identical write-identical-row UPDATEs.
    //
    // Deliberately placed AFTER the `imported_count == 0` early return
    // above: if zero files actually landed (cross-fs copy failure, disk
    // full, permission denied across the whole set), we don't flip old
    // grabs to 'replaced' — they stay 'imported' and the upgrade chain
    // isn't misrepresented. A pre-earlier version ran the marks inline
    // with each file op, which would flip old grabs even on a total
    // failure. Net effect of this placement: orphaned-replace rows
    // can't appear when the replacement never materialized.
    for old_grab_id in &grabs_to_mark_replaced {
        let _ = grabbed_torrents::mark_replaced(&state.db, *old_grab_id, grab.id).await;
    }

    // Series-level artifacts (tvshow.nfo + poster) run once per unique
    // series actually touched, not once total. A multi-series routed
    // batch now maintains the correct per-sibling artifacts instead of
    // dumping everything into the parent's folder.
    //
    // Always (re)write tvshow.nfo so Jellyfin picks up refreshed
    // metadata (status flips from RELEASING to FINISHED, plot updates,
    // newly indexed genres). The previous "write once if missing"
    // behavior meant any NFO written before metadata enrichment shipped
    // never got upgraded. The file is small and the write is local;
    // rewriting on every import run is cheap.
    for series_id in &touched_series {
        let Some(ctx) = series_ctx_cache.get(series_id) else {
            continue;
        };
        let series_root = Path::new(&cfg.media_root).join(&ctx.folder_name);

        // Artwork copies run before NFO writes so the NFO's `<art>`
        // block can reference only the files that actually landed on
        // disk. A hard-coded `<banner>banner.jpg</banner>` tag in
        // tvshow.nfo is worse than useless when banner.jpg doesn't
        // exist — Jellyfin logs a missing-file error per scan and the
        // external-scrape fallback still fires for the empty slot.
        //
        // Series-level cover also feeds the season-level folder.jpg
        // slot, so we dispatch both dests in one `copy_poster` call;
        // the blob is read into memory once and fanned out to both
        // paths under a single `spawn_blocking` (see `copy_artwork`).
        let poster_dest = series_root.join("poster.jpg");
        let season_poster_dest = ctx.season_dir.join("folder.jpg");
        let banner_dest = series_root.join("banner.jpg");
        let backdrop_dest = series_root.join("backdrop.jpg");

        let cover_source = ctx.cached_detail.as_ref().map(|d| d.cover_url.as_str());
        let banner_source = ctx.cached_detail.as_ref().map(|d| d.banner_url.as_str());

        let poster_outcome = copy_series_and_season_poster(
            &state.db,
            ctx.series.id,
            cover_source,
            &poster_dest,
            &season_poster_dest,
        )
        .await;
        let has_poster = poster_outcome.series_root;
        let has_folder_poster = poster_outcome.season_folder;

        let banner_outcome = copy_series_banner_and_backdrop(
            &state.db,
            ctx.series.id,
            banner_source,
            &banner_dest,
            &backdrop_dest,
        )
        .await;
        let has_banner = banner_outcome.series_banner;
        let has_backdrop = banner_outcome.series_backdrop;

        // Always (re)write tvshow.nfo + season.nfo so refreshed
        // AniList metadata (status flips, plot updates, new genres)
        // propagates. The `<art>` blocks are gated on what landed
        // above so a missing banner doesn't leave a dangling
        // reference in the NFO.
        let series_nfo = series_root.join("tvshow.nfo");
        let _ = nfo::write_series_nfo(
            &series_nfo,
            &ctx.series,
            ctx.cached_detail.as_ref(),
            &cfg.title_language,
            has_poster,
            has_banner,
            has_backdrop,
        )
        .await;

        let season_nfo = ctx.season_dir.join("season.nfo");
        let _ = nfo::write_season_nfo(
            &season_nfo,
            1,
            &ctx.series,
            ctx.cached_detail.as_ref(),
            &cfg.title_language,
            has_folder_poster,
        )
        .await;
    }

    // Flip episode tag rows from "grabbed" to "completed" per target
    // series. Uses the accumulator populated during the per-file loop
    // rather than `grab.episode_numbers` so three cases are handled
    // uniformly:
    //   - legacy single-episode grabs (ep list populated at grab time),
    //   - bare batch grabs where `grab.episode_numbers` is empty and
    //     the real list only exists on the landed filenames,
    //   - Phase 2 routed batches where files are split across sibling
    //     series (each sibling gets its own call keyed by
    //     `target_series_id`).
    //
    // Two flips happen per episode: the quality-tag row (via
    // `mark_completed`) and the newest 'grabbed' history row (via
    // `mark_grab_history_completed`). The history flip stamps in the
    // per-episode post-processed file name (Sonarr-style renamed
    // basename) and — only for non-batch rows — refines size_bytes to
    // the imported file's real size. Batch rows keep the whole-
    // torrent total so the episode detail modal can report "this
    // episode came from an X GiB batch".
    for (series_id, episodes) in &imported_eps_by_series {
        let ep_nums: Vec<i32> = episodes.iter().map(|(n, _, _)| *n).collect();
        let _ = episode_tags::mark_completed(&state.db, *series_id, &ep_nums).await;
        for (ep_num, file_size, file_name) in episodes {
            let _ = episode_tags::mark_grab_history_completed(
                &state.db, *series_id, *ep_num, file_name, *file_size,
            )
            .await;
        }
    }

    if failed_episodes.is_empty() {
        Ok(ImportOutcome::Imported)
    } else {
        Ok(ImportOutcome::PartiallyImported { failed_episodes })
    }
}

/// Series-level sidecars for one series: poster / banner / backdrop
/// copies from the artwork cache, then `tvshow.nfo` and `season.nfo`
/// with `<art>` blocks gated on what actually landed. This is the tail
/// of the per-series import loop above, exposed on its own so the
/// manual-import job (#122) can finish a series it just filled the
/// same way a post-processed grab would. Best-effort throughout: a
/// missing cached detail still writes the minimal series-row NFO.
pub async fn write_series_sidecars(state: &AppState, series_id: i64) -> Result<(), String> {
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| format!("config read failed: {e}"))?
        .ok_or_else(|| "config row missing".to_string())?;
    if cfg.media_root.trim().is_empty() {
        return Err("media root is not set".to_string());
    }
    let series_row = series::get_by_id(&state.db, series_id)
        .await
        .map_err(|e| format!("series lookup failed: {e}"))?
        .ok_or_else(|| format!("series {series_id} not found"))?;
    if series_row.folder_name.is_empty() {
        return Err(format!("series {series_id} has no folder name"));
    }
    let cached_detail = metadata_cache::get_by_series_id(&state.db, series_id)
        .await
        .ok()
        .flatten()
        .map(|c| c.detail);

    let series_root = Path::new(cfg.media_root.trim()).join(&series_row.folder_name);
    let season_dir = series_root.join(format!("Season {:02}", 1_i32));
    {
        let season_dir = season_dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&season_dir))
            .await
            .map_err(|e| format!("create season dir join: {e}"))?
            .map_err(|e| format!("create season dir: {e}"))?;
    }

    let poster_dest = series_root.join("poster.jpg");
    let season_poster_dest = season_dir.join("folder.jpg");
    let banner_dest = series_root.join("banner.jpg");
    let backdrop_dest = series_root.join("backdrop.jpg");
    let cover_source = cached_detail.as_ref().map(|d| d.cover_url.as_str());
    let banner_source = cached_detail.as_ref().map(|d| d.banner_url.as_str());

    let poster_outcome = copy_series_and_season_poster(
        &state.db,
        series_id,
        cover_source,
        &poster_dest,
        &season_poster_dest,
    )
    .await;
    let banner_outcome = copy_series_banner_and_backdrop(
        &state.db,
        series_id,
        banner_source,
        &banner_dest,
        &backdrop_dest,
    )
    .await;

    nfo::write_series_nfo(
        &series_root.join("tvshow.nfo"),
        &series_row,
        cached_detail.as_ref(),
        &cfg.title_language,
        poster_outcome.series_root,
        banner_outcome.series_banner,
        banner_outcome.series_backdrop,
    )
    .await
    .map_err(|e| format!("tvshow.nfo: {e}"))?;
    nfo::write_season_nfo(
        &season_dir.join("season.nfo"),
        1,
        &series_row,
        cached_detail.as_ref(),
        &cfg.title_language,
        poster_outcome.season_folder,
    )
    .await
    .map_err(|e| format!("season.nfo: {e}"))?;
    Ok(())
}

/// Run one post-processing cycle. Called by the background task every minute.
pub async fn run_once(state: &AppState) {
    let _guard = match POST_PROC_LOCK.try_lock() {
        Ok(g) => g,
        Err(_) => return, // already running
    };

    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        _ => return,
    };

    // When post-processing is disabled, we still want the UI checkmark to
    // flip as soon as qBit reports the torrent complete — otherwise the
    // row is stuck showing a progress bar forever even though the download
    // finished. Run a lightweight sweep that advances state on
    // episode_quality_tags and grabbed_torrents without moving any files.
    //
    // media_root being empty implies post-processing is unusable even if
    // the toggle is on, so treat it the same as the disabled case.
    if !cfg.post_processing_enabled || cfg.media_root.is_empty() {
        let _ = advance_state_without_import(state).await;
        return;
    }

    let pending = match grabbed_torrents::get_all_pending(&state.db).await {
        Ok(p) => p,
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::PostProcess,
                "Failed to query pending grabs",
                &e.to_string(),
            )
            .await;
            return;
        }
    };

    if pending.is_empty() {
        return;
    }

    // Multi-client fan-out: pending grabs may have landed on
    // different clients (autobrr → seedbox Deluge, RSS → Nyaa-pinned,
    // manual → default). Each grab carries its `download_client_id`
    // (NULL for legacy / unstamped rows → fall back to default).
    // Pre-#PR-F we called `list_scoped()` on the default only, which
    // missed every pinned grab and marked them stale after 60s.
    //
    // Strategy: collect the distinct client ids referenced, fan out
    // `list_scoped()` once per unique client, and build a per-client
    // (hash → torrent) lookup. Each grab is then matched against the
    // map for *its* client. A failed `list_scoped` against one client
    // doesn't poison the others — its grabs just stay pending until
    // the next pass.
    use std::collections::HashSet;
    // Capture both per-protocol defaults — an un-stamped pending grab
    // could come from either side, and post-processing doesn't know
    // the original indexer's protocol from the grab row alone, so it
    // checks both. The grab will only match against `list_scoped()`
    // on the client that actually has it, so naming both as candidates
    // is harmless when only one applies.
    let default_ids: Vec<i64> = {
        let pool = state.download_clients.read().await.clone();
        let mut v = Vec::new();
        if let Some(id) = pool.default_torrent_id {
            v.push(id);
        }
        if let Some(id) = pool.default_usenet_id
            && !v.contains(&id)
        {
            v.push(id);
        }
        v
    };

    // Pre-pass: NULL the `download_client_id` stamp on any pending
    // grab whose stamped client is no longer in the pool (deleted
    // by the user, disabled, or referenced via a never-existed id
    // due to an earlier crash mid-write). Runs unconditionally
    // before fan-out so the cleanup fires even in the corner case
    // where every pending grab points at a gone client and the
    // fan-out itself would early-return at `clients.is_empty()`.
    //
    // PR 110's in-loop cleanup also handled this but only when at
    // least one valid client made it into the per-pass `clients`
    // map — the all-orphans case fell through the `is_empty()`
    // guard. The pre-pass is sufficient because the pool can't
    // change mid-run; checking once up front catches every orphan
    // and lets the loop body stay focused on matching.
    //
    // The next post-processing tick re-loads `pending` and finds
    // the now-NULLed grab, falls through to default, and either
    // matches against the default's `list_scoped` or hits the 60s
    // stale-mark grace window.
    for grab in &pending {
        if let Some(id) = grab.download_client_id
            && state.client_by_id(id).await.is_none()
        {
            let _ = grabbed_torrents::set_download_client(&state.db, grab.id, None).await;
        }
    }

    let mut needed_ids: HashSet<i64> = HashSet::new();
    // Re-read pending after the cleanup so post-cleanup NULL stamps
    // contribute to needed_ids via the `or(default_id_opt)` branch.
    let pending = match grabbed_torrents::get_all_pending(&state.db).await {
        Ok(p) => p,
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::PostProcess,
                "Failed to re-query pending grabs after orphan cleanup",
                &e.to_string(),
            )
            .await;
            return;
        }
    };
    for grab in &pending {
        if let Some(id) = grab.download_client_id {
            needed_ids.insert(id);
        } else {
            for id in &default_ids {
                needed_ids.insert(*id);
            }
        }
    }
    if needed_ids.is_empty() {
        // No clients in pool at all — neither default nor any
        // pinned grab has somewhere to go. Same posture as the
        // pre-PR-F early return when default_download_client
        // returned None.
        return;
    }

    // Build per-client lookup maps. Resolves each id back to its
    // `Arc<dyn DownloadClient>`. A client deleted between the grab
    // and now (rare race) drops out — affected grabs see no match
    // and re-poll on the next pass. The structure carries the
    // resolved Arc so each grab's per-torrent calls (`get_files`,
    // etc.) reach the right client.
    let mut clients: HashMap<i64, std::sync::Arc<dyn DownloadClient>> = HashMap::new();
    let mut by_hash_per_client: HashMap<
        i64,
        HashMap<String, crate::services::download_client::DownloadItem>,
    > = HashMap::new();
    let mut by_name_per_client: HashMap<
        i64,
        HashMap<String, crate::services::download_client::DownloadItem>,
    > = HashMap::new();
    // Per-pass cache of `download_clients.download_path` keyed by
    // client id. Hoisted out of the per-grab loop so a pass with N
    // pending grabs sharing M clients does M queries instead of N.
    // Empty string means "row missing or path not configured" — the
    // import path falls back to legacy `cfg.<active_client>_download_path`.
    let mut download_path_per_client: HashMap<i64, String> = HashMap::new();
    for id in needed_ids {
        let Some(client) = state.client_by_id(id).await else {
            tracing::debug!("post_processing: skipping client id {id} — no longer in pool");
            continue;
        };
        // Fetch the row's download_path once. Resolves to "" when the
        // row vanished between needed_ids collection and now (rare
        // race) — the per-grab path then falls back to legacy.
        let path = crate::models::download_clients::get_by_id(&state.db, id)
            .await
            .ok()
            .flatten()
            .map(|r| r.download_path)
            .unwrap_or_default();
        download_path_per_client.insert(id, path);
        match client.list_scoped().await {
            Ok(torrents) => {
                let by_hash: HashMap<String, _> = torrents
                    .iter()
                    .map(|t| (t.hash.to_lowercase(), t.clone()))
                    .collect();
                let by_name: HashMap<String, _> = torrents
                    .iter()
                    .map(|t| (t.name.to_lowercase(), t.clone()))
                    .collect();
                by_hash_per_client.insert(id, by_hash);
                by_name_per_client.insert(id, by_name);
                clients.insert(id, client);
            }
            Err(e) => {
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Failed to query download client id={id}"),
                    &e.to_string(),
                )
                .await;
                // Fall through — this client's grabs stay pending; the
                // others still get processed.
            }
        }
    }

    if clients.is_empty() {
        return;
    }

    let mut any_imported = false;

    for grab in &pending {
        // Resolve which client this grab landed on. Stamped grabs
        // land on exactly one id; un-stamped grabs (older history,
        // orphan-cleanup just NULLed it) check each per-protocol
        // default in turn and take the first that has the torrent.
        // First-match-wins is safe because Ryokan never adds the
        // same torrent to two clients — the duplicate-add detection
        // in each impl rejects the second.
        let candidate_ids: Vec<i64> = match grab.download_client_id {
            Some(id) => vec![id],
            None => default_ids.clone(),
        };
        let mut hit: Option<(
            i64,
            std::sync::Arc<dyn DownloadClient>,
            crate::services::download_client::DownloadItem,
        )> = None;
        for cid in &candidate_ids {
            let Some(client) = clients.get(cid).cloned() else {
                continue;
            };
            let Some(by_hash) = by_hash_per_client.get(cid) else {
                continue;
            };
            let Some(by_name) = by_name_per_client.get(cid) else {
                continue;
            };
            let matched = if !grab.hash.is_empty() {
                by_hash.get(&grab.hash.to_lowercase())
            } else {
                by_name.get(&grab.torrent_name.to_lowercase())
            };
            if let Some(t) = matched {
                hit = Some((*cid, client, t.clone()));
                break;
            }
        }
        // Pick the first reachable candidate's id even on miss so the
        // `unmatched-grab` branch below can speak about *some* client
        // when emitting the "torrent not found" log line. Falls
        // through to `continue` if nothing was reachable at all.
        let Some(grab_client_id) = hit.as_ref().map(|(id, _, _)| *id).or_else(|| {
            candidate_ids
                .iter()
                .find(|cid| clients.contains_key(cid))
                .copied()
        }) else {
            continue;
        };
        let Some(client) = clients.get(&grab_client_id).cloned() else {
            // Pool changed mid-loop (shouldn't happen — pool is read
            // once at the top of `run_once`), skip defensively.
            continue;
        };
        let matched: Option<&crate::services::download_client::DownloadItem> =
            hit.as_ref().map(|(_, _, t)| t);

        let Some(torrent) = matched else {
            // Item not found in any configured download client. If the
            // grab is old enough (> 60 seconds), the user likely
            // deleted it from the client — mark as removed. The grace
            // window used to be 5 minutes to cover qBit restarts, but
            // in practice the `list_scoped` call would fail outright
            // during a restart (we'd not even reach this branch with
            // a valid item list), so the long grace window just
            // delayed reconciliation of manual deletes for no safety
            // gain. A minute is enough slack for a slow first-poll
            // after an add-torrent / addurl RPC, short enough that
            // "deleted ep 9 in the client and it still shows pending"
            // becomes "shows cancelled within a minute."
            if grab_is_stale(&grab.grabbed_at, 60) {
                logger::warn(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Item removed from download client: '{}'", grab.torrent_name),
                    "Marking as removed (not found in client)",
                )
                .await;
                let _ = grabbed_torrents::mark_removed(&state.db, grab.id).await;
                let _ = episode_tags::clear_tags_for_removal(
                    &state.db,
                    grab.series_id,
                    &grab.episode_numbers,
                )
                .await;
            }
            continue;
        };

        // Detect failed/error items and mark them. The detail line
        // surfaces both the client's native state string (`Failed`
        // for SAB, `error` for qBit, etc.) and Ryokan's normalized
        // `state_kind` slug so a System → Logs reader can diagnose
        // without having to remember which client uses which
        // vocabulary. Pre-multi-client this read `qbit_state=`,
        // which mis-labelled SAB/Deluge/Transmission/rtorrent
        // failures with a qBit prefix.
        if torrent.state_kind.is_errored() {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                &format!("Item in error state: '{}'", grab.torrent_name),
                &format!("state={} kind={:?}", torrent.state, torrent.state_kind),
            )
            .await;
            let _ = grabbed_torrents::mark_failed(&state.db, grab.id).await;
            continue;
        }

        if !torrent.state_kind.is_complete() {
            continue;
        }

        // Stamp qBit's output path on the grab row before we move/
        // hardlink the file into the library. Done BEFORE import so
        // that even if import errors out mid-way, the UI still has a
        // record of where the client left the file. Apply the
        // download_path from the *actual* `download_clients` row this
        // grab landed on (#PR-F multi-client) so a seedbox-reported
        // `/downloads/…` path is rewritten to Ryokan's local mount
        // (e.g. `/mnt/seedbox/downloads/…`). Empty path = same-host
        // client, no rewrite needed. Pre-PR-F this read from
        // `cfg.<active_client>_download_path` — wrong in multi-client
        // because pinned grabs land on a non-default client.
        let cached_path = download_path_per_client
            .get(&grab_client_id)
            .map(|s| s.as_str())
            .unwrap_or("");
        let local_download_path = if cached_path.is_empty() {
            // Row was missing from the pre-loop fetch (rare race) or
            // legitimately had an empty download_path (same-host
            // client). Fall back to the legacy `active_client`
            // download path so a pre-multi-client install on rollover
            // doesn't lose its configured rewrite.
            crate::services::download_client::per_client_download_path(&cfg)
        } else {
            cached_path
        };
        let client_path = {
            let raw = if !torrent.content_path.is_empty() {
                torrent.content_path.clone()
            } else {
                torrent.save_path.clone()
            };
            crate::services::download_client::translate_client_path(
                &raw,
                &torrent.save_path,
                local_download_path,
            )
        };
        let local_save_path = crate::services::download_client::translate_client_path(
            &torrent.save_path,
            &torrent.save_path,
            local_download_path,
        );
        let _ = grabbed_torrents::stamp_client_content_path(&state.db, grab.id, &client_path).await;

        match import_torrent(state, &cfg, grab, &torrent.hash, &local_save_path, &client).await {
            Ok(ImportOutcome::Imported) => {
                any_imported = true;
                let _ = grabbed_torrents::mark_imported(&state.db, grab.id).await;
                let _ = grabbed_torrents::stamp_import_mode(
                    &state.db,
                    grab.id,
                    &cfg.post_processing_mode,
                )
                .await;
                // #27 — log every successful import so there's a trail
                // from grab → complete in System → Logs. Before this,
                // the only log a successful grab produced was the grab
                // itself and maybe the Jellyfin refresh at the end.
                // Operators who went looking for "did this episode
                // land?" had to check the library row or disk.
                logger::info(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Imported '{}'", grab.torrent_name),
                    &format!(
                        "series_id={} episodes={:?}",
                        grab.series_id, grab.episode_numbers
                    ),
                )
                .await;
                // Issue #228: a usenet job, or a torrent imported in
                // move mode, has nothing left to seed and leaves the
                // client now. Hardlink and copy mode torrents keep
                // seeding until the finished-seed sweep sees the
                // client's own rules met.
                client_cleanup::remove_after_import(state, &cfg, grab, grab_client_id, &client)
                    .await;
                // Episode tag "grabbed → completed" flips happen inside
                // `import_torrent` itself so a Phase 2 routed batch can
                // mark each sibling's tags under the sibling's own
                // series_id + per-route episode numbers. Legacy grabs
                // still get the same flip as before via the
                // `routes.is_empty()` fallback there.
            }
            Ok(ImportOutcome::PartiallyImported { failed_episodes }) => {
                // Some files imported, some failed. Mark the grab
                // imported because the user does have partial data on
                // disk and the Downloads page should reflect that — but
                // also emit a single SUMMARY error log so the failure
                // is greppable in System → Logs rather than scattered
                // across N per-file errors. Without this, a 24-episode
                // pack that lost one file to a transient disk-full
                // would silently report as fully imported and the user
                // would only notice in Jellyfin.
                any_imported = true;
                let _ = grabbed_torrents::mark_imported(&state.db, grab.id).await;
                // Issue #228: a partial import must never be swept out
                // of the client; the episodes that failed are still only
                // in the download folder. "partial" keeps the row out of
                // `list_imported_in_client` for good.
                let _ = grabbed_torrents::stamp_import_mode(&state.db, grab.id, "partial").await;
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Partial import for '{}' — {} episode(s) failed",
                        grab.torrent_name,
                        failed_episodes.len()
                    ),
                    &format!(
                        "series_id={} failed_episodes={:?}",
                        grab.series_id, failed_episodes
                    ),
                )
                .await;
            }
            Ok(ImportOutcome::AllFailed { failed_episodes }) => {
                // Every video file we attempted failed. Mark the grab
                // failed so the user can see and act — leaving it
                // pending would re-run the same broken import every
                // tick and just spam the log without ever escalating.
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Import failed for '{}' — every file errored",
                        grab.torrent_name
                    ),
                    &format!(
                        "series_id={} failed_episodes={:?}",
                        grab.series_id, failed_episodes
                    ),
                )
                .await;
                let _ = grabbed_torrents::mark_failed(&state.db, grab.id).await;
            }
            Ok(ImportOutcome::NotReady) => {
                // Torrent complete but no video files yet — leave as pending.
                // The caller (qBit) might still be finalizing the files,
                // or the torrent could be all samples/.nfo (pathological).
                // We intentionally don't escalate here — next post-proc
                // tick retries. A stuck-forever failsafe would need a
                // "pending too long" timer; covered by the plan's
                // future work, not this commit.
            }
            Err(e) => {
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Import failed for '{}'", grab.torrent_name),
                    &e,
                )
                .await;
                let _ = grabbed_torrents::mark_failed(&state.db, grab.id).await;
            }
        }
    }

    if any_imported && let Some(jellyfin) = state.jellyfin.read().await.as_ref() {
        if let Err(e) = jellyfin.refresh_library().await {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                "Jellyfin refresh failed after import",
                &e,
            )
            .await;
        } else {
            logger::info(
                &state.db,
                LogCategory::PostProcess,
                "Triggered Jellyfin library refresh",
                "",
            )
            .await;
        }
    }
}

/// Lightweight variant of `run_once` used when post-processing is
/// disabled (or media_root is unset). Advances a qBit-complete pending
/// grab's state on `grabbed_torrents` and `episode_quality_tags` so the
/// UI checkmark can flip, without moving any files or writing an NFO.
///
/// This exists because the UI otherwise has no way to know a torrent
/// finished downloading when post-processing is off — the checkmark
/// watches `episode_quality_tags.state = 'completed'`, which only gets
/// set by the full import pass. Operators who run Ryokan alongside a
/// separate move/rename tool (or who just leave files in the qBit
/// completed dir) would see every row stuck at "Importing…" forever.
async fn advance_state_without_import(state: &AppState) -> Result<(), ()> {
    let pending = grabbed_torrents::get_all_pending(&state.db)
        .await
        .map_err(|_| ())?;
    if pending.is_empty() {
        return Ok(());
    }

    // Config load is only for the remote-path mapping — we don't
    // need the full cfg here. A single lookup is cheap; avoiding a
    // parameter means `run_task` stays unaware of this codepath's
    // needs.
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|_| ())?
        .unwrap_or_default();

    // Fan out across every configured client so SAB grabs surface
    // here too. Prior implementation called `default_download_client`
    // and missed every SAB grab, leaving Usenet completions stuck at
    // "Importing…" forever when post-processing was disabled.
    let pool = state.download_clients.read().await.clone();
    if pool.clients.is_empty() {
        return Ok(());
    }
    let mut all_torrents: Vec<crate::services::download_client::DownloadItem> = Vec::new();
    for c in pool.clients.values() {
        if let Ok(items) = c.list_scoped().await {
            all_torrents.extend(items);
        }
    }
    let by_hash: HashMap<String, &crate::services::download_client::DownloadItem> = all_torrents
        .iter()
        .map(|t| (t.hash.to_lowercase(), t))
        .collect();
    let by_name: HashMap<String, &crate::services::download_client::DownloadItem> = all_torrents
        .iter()
        .map(|t| (t.name.to_lowercase(), t))
        .collect();

    for grab in &pending {
        let matched = if !grab.hash.is_empty() {
            by_hash.get(&grab.hash.to_lowercase()).copied()
        } else {
            by_name.get(&grab.torrent_name.to_lowercase()).copied()
        };
        let Some(torrent) = matched else { continue };

        if !torrent.state_kind.is_complete() {
            continue;
        }

        // Stamp the client-side path for the episode detail modal.
        // Prefer content_path (native on qBit ≥ 2.6.1; computed from
        // save_path + files' common prefix on Deluge) and fall back
        // to save_path for pre-2.6.1 qBit. Same per-client
        // download-path rewrite as the main import path above.
        let local_download_path = crate::services::download_client::per_client_download_path(&cfg);
        let client_path = {
            let raw = if !torrent.content_path.is_empty() {
                torrent.content_path.clone()
            } else {
                torrent.save_path.clone()
            };
            crate::services::download_client::translate_client_path(
                &raw,
                &torrent.save_path,
                local_download_path,
            )
        };
        let _ = grabbed_torrents::stamp_client_content_path(&state.db, grab.id, &client_path).await;

        // Post-processing-off mode never imports files but still
        // records source-side paths so the series-remove handler can
        // clean SAB's complete dir later. Walk the local-translated
        // content path for video files and stamp them. Best-effort:
        // empty list is fine (the grab might be a torrent with no
        // .mkv-shaped extension, or the path might not be readable
        // from Ryokan's view).
        let walk_root = std::path::Path::new(&client_path).to_path_buf();
        if walk_root.is_dir() {
            // Same blocking-pool hop as the import-time call site —
            // recursive sync read_dir on a multi-file pack mustn't run
            // on the runtime thread.
            let walk_root_for_blocking = walk_root.clone();
            let videos =
                tokio::task::spawn_blocking(move || walk_video_files(&walk_root_for_blocking))
                    .await
                    .unwrap_or_default();
            let source_paths: Vec<String> = videos
                .into_iter()
                .map(|f| walk_root.join(&f.name).display().to_string())
                .collect();
            if !source_paths.is_empty() {
                let _ = grabbed_torrents::stamp_imported_source_paths(
                    &state.db,
                    grab.id,
                    &source_paths,
                )
                .await;
            }
        }

        // Mark the grab row as finalized so we stop polling it and the
        // UI stops treating it as in-flight. Use `mark_completed_no_import`
        // rather than `mark_imported` — we never moved a file, so
        // `imported_at` stays NULL and future reports keyed on that
        // column don't see a false positive for this grab. Then flip
        // the episode tag(s) to 'completed' so the checkmark appears
        // on the next poll. Phase-2 sibling routes get the per-series
        // treatment too.
        let _ = grabbed_torrents::mark_completed_no_import(&state.db, grab.id).await;

        let routes = grabbed_torrents::get_series_routes(&state.db, grab.id)
            .await
            .unwrap_or_default();
        if routes.is_empty() {
            let _ = episode_tags::mark_completed(&state.db, grab.series_id, &grab.episode_numbers)
                .await;
        } else {
            for route in &routes {
                let _ = episode_tags::mark_completed(
                    &state.db,
                    route.series_id,
                    &route.episode_numbers,
                )
                .await;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;
