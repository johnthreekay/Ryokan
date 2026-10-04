//! Artwork fan-out: one blob, many filesystem destinations.
//!
//! `copy_artwork` is the entry point; it reads a content-addressed blob
//! once under spawn_blocking and then fs::writes (or hardlinks) the
//! bytes to every destination. `copy_series_and_season_poster` and
//! `copy_series_banner_and_backdrop` are thin wrappers for the Jellyfin
//! library layout.

use std::path::Path;

use crate::models::{artwork_cache, log::LogCategory};
use crate::services::{artwork, logger};

pub(super) async fn copy_artwork(
    db: &sqlx::SqlitePool,
    series_id: i64,
    cache_key: &str,
    image_kind: &str,
    source_url: Option<&str>,
    dests: &[&Path],
) -> Vec<bool> {
    if dests.is_empty() {
        return Vec::new();
    }

    // Step 1: cache miss + non-empty source URL → self-heal by
    // re-fetching. Two SELECTs on the unhappy path, one on the happy
    // path (the re-check after the fetch is necessary because the
    // fetch may have partially succeeded — e.g. blob written but ref
    // upsert failed).
    //
    // `metadata_sync::refresh_series_metadata_inner` is the only
    // upstream caller of `cache_image`, and every error there is
    // swallowed via `let _ = ...`. A transient fetch failure (AL CDN
    // hiccup, a cancelled rebuild sweep, a Jikan-fallback with an
    // empty banner field, …) would otherwise leave the blob missing
    // indefinitely — baking the re-fetch in here makes the self-heal
    // an invariant that any future caller supplying a source URL
    // inherits automatically.
    let mut cached = artwork_cache::get(db, cache_key).await.ok().flatten();
    if cached.is_none()
        && let Some(url) = source_url.filter(|u| !u.trim().is_empty())
    {
        tracing::debug!(
            target: "ryokan::post_processing",
            series_id,
            cache_key,
            image_kind,
            "artwork cache miss; fetching on demand",
        );
        if let Err(err) =
            artwork::cache_image(db, cache_key, "series", Some(series_id), image_kind, url).await
        {
            logger::warn(
                db,
                LogCategory::PostProcess,
                &format!(
                    "On-demand artwork fetch failed for {image_kind} on series_id={series_id}"
                ),
                &format!("source_url={url}, error={err}"),
            )
            .await;
        }
        cached = artwork_cache::get(db, cache_key).await.ok().flatten();
    }

    let Some(entry) = cached else {
        tracing::debug!(
            target: "ryokan::post_processing",
            series_id,
            cache_key,
            image_kind,
            "artwork blob missing from cache and no recoverable source; skipping copy",
        );
        return vec![false; dests.len()];
    };

    // Step 2: read the blob bytes once, fan out to each dest. Under
    // one `spawn_blocking` so an NFS-backed media root doesn't
    // serialize file-open round-trips from separate tokio tasks.
    //
    // Outcome is either `SourceReadFailed` (one error, affects every
    // dest) or `PerDest` (dest-specific errors). Keeping these
    // distinct lets the caller log the source-read failure exactly
    // once instead of N times with misleading `dst=` paths.
    enum CopyOutcome {
        SourceReadFailed(std::io::Error),
        PerDest(Vec<Result<(), std::io::Error>>),
    }

    // Only the cache's own copy, by file name (`blob_path_in_cache`):
    // the stored path comes from the database, and a restored one could
    // name any readable file to be copied into the media library.
    let Some(src) = crate::services::artwork::blob_path_in_cache(&entry.local_path) else {
        return vec![false; dests.len()];
    };
    let owned_dests: Vec<std::path::PathBuf> = dests.iter().map(|p| p.to_path_buf()).collect();
    let src_display = src.display().to_string();
    let copy_result = tokio::task::spawn_blocking(move || -> CopyOutcome {
        let bytes = match std::fs::read(&src) {
            Ok(b) => b,
            Err(e) => return CopyOutcome::SourceReadFailed(e),
        };
        // First dest gets the real write; subsequent dests are
        // hardlinked to it when possible so a multi-dest fan-out
        // spends one blob's worth of bytes on disk regardless of
        // `dests.len()`. Motivating case: series-root `banner.jpg`
        // + `backdrop.jpg` (same blob, same directory) would
        // otherwise cost ~500 MB of pure duplication in a
        // 1000-series library, plus the same amount on every
        // rsync/backup/snapshot.
        //
        // Fallback to `fs::write` if the hardlink fails — cross-fs
        // (backdrop dir vs. series-root dir on different mounts),
        // unusual FS (FAT32, some SMB mounts), or the first write
        // errored and the source doesn't exist to link against. The
        // fallback is behaviorally identical to the pre-dedupe code.
        let per_dest: Vec<std::io::Result<()>> = owned_dests
            .iter()
            .enumerate()
            .map(|(i, dst)| -> std::io::Result<()> {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if i == 0 {
                    // Anchor: actual bytes on disk. Subsequent
                    // hardlinks reference this inode.
                    std::fs::write(dst, &bytes)
                } else {
                    // Hardlink wants a nonexistent dst; clean any
                    // stale file first. `remove_file` error is
                    // ignored because "already absent" is the
                    // happy case.
                    let _ = std::fs::remove_file(dst);
                    let anchor = &owned_dests[0];
                    std::fs::hard_link(anchor, dst)
                        .or_else(|_hardlink_err| std::fs::write(dst, &bytes))
                }
            })
            .collect();
        CopyOutcome::PerDest(per_dest)
    })
    .await;

    match copy_result {
        Ok(CopyOutcome::SourceReadFailed(err)) => {
            logger::warn(
                db,
                LogCategory::PostProcess,
                &format!("Failed to read cached {image_kind} blob for series_id={series_id}"),
                &format!("src={}, error={}", src_display, err),
            )
            .await;
            vec![false; dests.len()]
        }
        Ok(CopyOutcome::PerDest(per_dest)) => {
            let mut results = Vec::with_capacity(dests.len());
            for (i, r) in per_dest.into_iter().enumerate() {
                match r {
                    Ok(()) => results.push(true),
                    Err(err) => {
                        logger::warn(
                            db,
                            LogCategory::PostProcess,
                            &format!(
                                "Failed to write series {image_kind} for series_id={series_id}"
                            ),
                            &format!(
                                "src={}, dst={}, error={}",
                                src_display,
                                dests[i].display(),
                                err
                            ),
                        )
                        .await;
                        results.push(false);
                    }
                }
            }
            results
        }
        Err(join_err) => {
            logger::warn(
                db,
                LogCategory::PostProcess,
                &format!("{image_kind} copy task panicked for series_id={series_id}"),
                &format!("src={}, error={}", src_display, join_err),
            )
            .await;
            vec![false; dests.len()]
        }
    }
}

/// Named result of a poster fan-out to the two slots Jellyfin reads:
/// the series-root `poster.jpg` (series-level card) and the season
/// folder's `folder.jpg` (season-card poster). Structural naming so
/// the caller can't accidentally swap the two booleans via a slice
/// reorder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PosterOutcome {
    /// True if the series-root `poster.jpg` landed on disk.
    pub series_root: bool,
    /// True if the `Season NN/folder.jpg` landed on disk.
    pub season_folder: bool,
}

/// Copy the cached series poster blob to both the series-root
/// `poster.jpg` and the season folder's `folder.jpg` — the two files
/// Jellyfin reads for the series card + season card posters. The
/// blob is self-healed from `source_url` if the cache is empty and
/// read into memory once, fanned out to both dests under a single
/// `spawn_blocking` (see [`copy_artwork`]).
pub(super) async fn copy_series_and_season_poster(
    db: &sqlx::SqlitePool,
    series_id: i64,
    source_url: Option<&str>,
    series_poster_dest: &Path,
    season_folder_dest: &Path,
) -> PosterOutcome {
    let cache_key = format!("series-{}-cover", series_id);
    let results = copy_artwork(
        db,
        series_id,
        &cache_key,
        "cover",
        source_url,
        &[series_poster_dest, season_folder_dest],
    )
    .await;
    // `copy_artwork`'s Vec<bool> contract is index-aligned to the
    // `dests` slice — mapping those indices onto the named fields
    // here (rather than at the caller) keeps the positional
    // dependency contained to this 3-line function, where a swap
    // would be immediately obvious to any reader.
    PosterOutcome {
        series_root: results.first().copied().unwrap_or(false),
        season_folder: results.get(1).copied().unwrap_or(false),
    }
}

/// Named result of a banner fan-out to the two Jellyfin image slots
/// we fill from the AniList `bannerImage`. Structural naming so the
/// caller can't accidentally swap the two booleans via slice reorder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct BannerOutcome {
    /// True if `{series_folder}/banner.jpg` landed — Jellyfin
    /// `ImageType::Banner` (3). Only surfaces in "Banner" library
    /// layouts; mostly legacy in the current Jellyfin web UI.
    pub series_banner: bool,
    /// True if `{series_folder}/backdrop.jpg` landed — Jellyfin
    /// `ImageType::Backdrop` (2). This is the slot the series detail
    /// page reads for the hero image behind the header, and what the
    /// home screen uses as the featured background. AniList's
    /// `bannerImage` is semantically a backdrop (wide hero, 1900×400
    /// typical), not a thin Kodi-style banner, so we copy the same
    /// blob into both slots and let Jellyfin pick per UI context.
    pub series_backdrop: bool,
}

/// Copy the cached AniList banner blob to the two series-level image
/// slots Jellyfin cares about: `banner.jpg` (legacy banner slot) and
/// `backdrop.jpg` (modern hero/fanart slot). One `cache_image` fetch
/// on a miss, one `fs::read` of the cached blob, two `fs::write`s
/// under a single `spawn_blocking`.
///
/// Background: Jellyfin 10.x's default UI barely renders `ImageType::
/// Banner` — the prominent wide image on the series detail page is
/// the Backdrop. Copying the AL banner only to `banner.jpg` left
/// Jellyfin showing nothing (or scraping TVDB/TMDB) for the backdrop
/// slot. Writing both files keeps the data path unambiguous: same
/// source blob, two on-disk filenames so auto-discovery finds the
/// right one per slot.
pub(super) async fn copy_series_banner_and_backdrop(
    db: &sqlx::SqlitePool,
    series_id: i64,
    source_url: Option<&str>,
    banner_dest: &Path,
    backdrop_dest: &Path,
) -> BannerOutcome {
    let cache_key = format!("series-{}-banner", series_id);
    let results = copy_artwork(
        db,
        series_id,
        &cache_key,
        "banner",
        source_url,
        &[banner_dest, backdrop_dest],
    )
    .await;
    // Same positional-contract containment pattern as
    // `copy_series_and_season_poster` — keep the slice-index
    // mapping local to this 3-line helper.
    BannerOutcome {
        series_banner: results.first().copied().unwrap_or(false),
        series_backdrop: results.get(1).copied().unwrap_or(false),
    }
}

/// #30 — Decide what offset to subtract from a parsed filename episode
/// number when the file isn't covered by a Phase 2 auto-expand route
/// row. Used by the legacy single-series path so absolute-numbered
/// releases of sequel cours (`[SubsPlease] Jujutsu Kaisen - 56` for
/// JJK S3 E9 with prior-cour total 47) land on the correct relative
/// episode.
///
/// Rule: if the series has a non-zero `cumulative_prior_episodes` AND
/// the parsed number exceeds that cumulative, treat the filename as
/// absolute-numbered and subtract. Otherwise return 0 (relative
/// numbering, file renames to its parsed number as-is).
///
/// Example with JJK S3 (cumulative = 47, own episodes = 12):
///   - raw = 56 (SubsPlease absolute) → 56 > 47 → offset = 47 → ep = 9. ✓
///   - raw = 9 (Erai-raws relative) → 9 ≤ 47 → offset = 0 → ep = 9. ✓
///   - raw = 25 (stray S2 E1 file that got mis-grabbed) → 25 ≤ 47 →
///     offset = 0, file lands as E25 of S3 (which doesn't exist,
#[cfg(test)]
mod tests {
    use super::*;

    // ── copy_artwork fan-out ──────────────────────────────────────────────
    //
    // The multi-dest fan-out in `copy_artwork` is the headline of the
    // PR-51 review's dedupe item — one `std::fs::read` under a single
    // `spawn_blocking`, N `fs::write`s to each dest. A regression that
    // collapses back to per-dest spawns (or reads the blob once per
    // dest) would be invisible to the existing NFO-level tests, so we
    // pin the contract directly here.

    /// Create just the two artwork tables we need, without dragging in
    /// the full `models::migrate` schema. FKs aren't enforced on
    /// in-memory SQLite unless `PRAGMA foreign_keys = ON` is set, so
    /// the missing `series` parent row is harmless.
    async fn setup_artwork_only_db() -> sqlx::SqlitePool {
        let db = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("open in-memory db");
        sqlx::query(
            r#"CREATE TABLE image_blobs (
                blob_hash TEXT PRIMARY KEY,
                local_path TEXT NOT NULL DEFAULT '',
                content_type TEXT NOT NULL DEFAULT '',
                byte_size INTEGER NOT NULL DEFAULT 0,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )"#,
        )
        .execute(&db)
        .await
        .expect("create image_blobs");
        sqlx::query(
            r#"CREATE TABLE image_refs (
                cache_key TEXT PRIMARY KEY,
                parent_kind TEXT NOT NULL DEFAULT '',
                parent_id INTEGER,
                image_kind TEXT NOT NULL DEFAULT '',
                source_url TEXT NOT NULL DEFAULT '',
                blob_hash TEXT NOT NULL,
                last_write INTEGER NOT NULL DEFAULT 0,
                cached_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )"#,
        )
        .execute(&db)
        .await
        .expect("create image_refs");
        // The `logs` table is used by `logger::warn` on the failure
        // paths; create a minimal shape so those calls don't spuriously
        // fail the test when exercising the error branches.
        sqlx::query(
            r#"CREATE TABLE logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                level TEXT NOT NULL DEFAULT '',
                category TEXT NOT NULL DEFAULT '',
                message TEXT NOT NULL DEFAULT '',
                detail TEXT NOT NULL DEFAULT ''
            )"#,
        )
        .execute(&db)
        .await
        .expect("create logs");
        db
    }

    /// A blob path inside the real artwork cache: copies read only from
    /// there (`blob_path_in_cache`). Named after the test's own temp dir
    /// so parallel tests never share one; removed with `remove_test_blob`.
    fn test_blob_path(dir: &std::path::Path) -> std::path::PathBuf {
        let blobs = crate::services::artwork::media_cache_dir().join("blobs");
        std::fs::create_dir_all(&blobs).expect("create blob dir");
        blobs.join(format!(
            "{}.jpg",
            dir.file_name().unwrap().to_string_lossy()
        ))
    }

    fn remove_test_blob(dir: &std::path::Path) {
        let _ = std::fs::remove_file(test_blob_path(dir));
    }

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nonce = format!(
            "ryokan_pp_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            label,
        );
        let dir = std::env::temp_dir().join(nonce);
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    async fn register_blob(
        db: &sqlx::SqlitePool,
        cache_key: &str,
        blob_path: &std::path::Path,
        blob_hash: &str,
        byte_size: i64,
    ) {
        artwork_cache::upsert_blob(
            db,
            blob_hash,
            &blob_path.to_string_lossy(),
            "image/jpeg",
            byte_size,
        )
        .await
        .expect("upsert_blob");
        artwork_cache::upsert_ref(
            db,
            artwork_cache::RefUpsert {
                cache_key,
                parent_kind: "series",
                parent_id: Some(42),
                image_kind: "cover",
                source_url: "",
                blob_hash,
                last_write: 0,
            },
        )
        .await
        .expect("upsert_ref");
    }

    #[tokio::test]
    async fn a_blob_path_outside_the_cache_is_never_read() {
        // A restored database can set `local_path` to any file. Only the
        // cache's own copy (same file name under `blobs/`) is read.
        let db = setup_artwork_only_db().await;
        let dir = unique_test_dir("copy_outside");
        let outside = dir.join("secret.key");
        std::fs::write(&outside, b"\xFF\xD8\xFF not really a key").unwrap();
        register_blob(&db, "series-42-cover", &outside, "deadbeef", 8).await;
        let dst = dir.join("poster.jpg");
        let results = copy_artwork(&db, 42, "series-42-cover", "cover", None, &[&dst]).await;
        assert_eq!(results, vec![false]);
        assert!(
            !dst.exists(),
            "the outside file was not copied into the library"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn copy_artwork_fans_out_single_blob_to_multiple_dests() {
        let db = setup_artwork_only_db().await;
        let dir = unique_test_dir("copy_fanout");
        let blob_path = test_blob_path(&dir);
        let payload = b"\xFF\xD8\xFF\xE0test jpeg body".to_vec();
        std::fs::write(&blob_path, &payload).expect("write blob");
        register_blob(
            &db,
            "series-42-cover",
            &blob_path,
            "deadbeef",
            payload.len() as i64,
        )
        .await;

        let dst_a = dir.join("poster.jpg");
        let dst_b = dir.join("season/folder.jpg");

        let results =
            copy_artwork(&db, 42, "series-42-cover", "cover", None, &[&dst_a, &dst_b]).await;

        assert_eq!(results, vec![true, true], "both dests must report success");
        assert_eq!(
            std::fs::read(&dst_a).expect("read dst_a"),
            payload,
            "dst_a bytes must match source",
        );
        assert_eq!(
            std::fs::read(&dst_b).expect("read dst_b"),
            payload,
            "dst_b bytes must match source (nested dir must be created)",
        );

        // Cleanup — best effort.
        remove_test_blob(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Dedupe invariant: the fan-out must hardlink subsequent dests
    /// to the first rather than writing the blob bytes twice. A
    /// regression that silently restores per-dest writes would burn
    /// on-disk bytes for every import (most visibly on banner +
    /// backdrop, which always share the same blob in the same dir).
    ///
    /// Unix-only because this assertion leans on
    /// `std::os::unix::fs::MetadataExt::{ino, nlink}` — the cheapest
    /// way to prove hardlink-ness without shelling out to `stat`.
    /// **For Windows support:** the production code path
    /// ([`copy_artwork`]'s `std::fs::hard_link` call) already works
    /// cross-platform (Linux/macOS `link(2)`, Windows
    /// `CreateHardLinkW`) — only this test's assertion is gated.
    /// To cover Windows, add a mirror test gated on `#[cfg(windows)]`
    /// using `std::os::windows::fs::MetadataExt::number_of_links()`
    /// (expect ≥ 2 on the anchor), or check file-index identity via
    /// `BY_HANDLE_FILE_INFORMATION` through the `windows` crate for
    /// a direct inode-equivalent comparison. The cross-platform
    /// `copy_artwork_overwrites_preexisting_dest_files` test below
    /// already runs on every target and pins *behavior* (both dests
    /// end up with the new payload) independent of mechanism.
    #[cfg(unix)]
    #[tokio::test]
    async fn copy_artwork_hardlinks_subsequent_dests_to_the_first() {
        use std::os::unix::fs::MetadataExt;

        let db = setup_artwork_only_db().await;
        let dir = unique_test_dir("copy_hardlink");
        let blob_path = test_blob_path(&dir);
        let payload = b"\xFF\xD8\xFF\xE0anchor-inode-test".to_vec();
        std::fs::write(&blob_path, &payload).expect("write blob");
        register_blob(
            &db,
            "series-42-banner",
            &blob_path,
            "deadbeef",
            payload.len() as i64,
        )
        .await;

        // Both dests live in the same directory (same fs guaranteed
        // on tmpfs/ext4) so hardlink must succeed without falling
        // back to `fs::write`.
        let banner_dst = dir.join("banner.jpg");
        let backdrop_dst = dir.join("backdrop.jpg");

        let results = copy_artwork(
            &db,
            42,
            "series-42-banner",
            "banner",
            None,
            &[&banner_dst, &backdrop_dst],
        )
        .await;

        assert_eq!(results, vec![true, true]);

        let banner_meta = std::fs::metadata(&banner_dst).expect("stat banner");
        let backdrop_meta = std::fs::metadata(&backdrop_dst).expect("stat backdrop");
        assert_eq!(
            banner_meta.ino(),
            backdrop_meta.ino(),
            "backdrop.jpg must be hardlinked to banner.jpg, not a separate copy",
        );
        // Hardlinks share nlink count ≥ 2 (the anchor + at least one link).
        assert!(
            banner_meta.nlink() >= 2,
            "banner.jpg nlink must reflect the hardlinked backdrop; got {}",
            banner_meta.nlink(),
        );

        remove_test_blob(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hardlinking is best-effort: if the dst already exists (e.g.
    /// a previous run landed it), the fan-out must clean it up
    /// first. A regression that left the stale dst in place would
    /// silently keep an old blob on one of the Jellyfin image
    /// slots after an artwork refresh.
    #[tokio::test]
    async fn copy_artwork_overwrites_preexisting_dest_files() {
        let db = setup_artwork_only_db().await;
        let dir = unique_test_dir("copy_overwrite");
        let blob_path = test_blob_path(&dir);
        let new_payload = b"new artwork blob".to_vec();
        std::fs::write(&blob_path, &new_payload).expect("write blob");
        register_blob(
            &db,
            "series-42-banner",
            &blob_path,
            "deadbeef",
            new_payload.len() as i64,
        )
        .await;

        let banner_dst = dir.join("banner.jpg");
        let backdrop_dst = dir.join("backdrop.jpg");
        // Pre-seed both dests with stale bytes.
        std::fs::write(&banner_dst, b"stale banner").expect("seed banner");
        std::fs::write(&backdrop_dst, b"stale backdrop").expect("seed backdrop");

        let results = copy_artwork(
            &db,
            42,
            "series-42-banner",
            "banner",
            None,
            &[&banner_dst, &backdrop_dst],
        )
        .await;

        assert_eq!(results, vec![true, true]);
        assert_eq!(std::fs::read(&banner_dst).unwrap(), new_payload);
        assert_eq!(std::fs::read(&backdrop_dst).unwrap(), new_payload);

        remove_test_blob(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn copy_artwork_returns_all_false_when_cache_entry_missing_and_no_source() {
        // Neither a cache row nor a source URL — function must degrade
        // gracefully to `[false; N]` rather than erroring upward.
        let db = setup_artwork_only_db().await;
        let dir = unique_test_dir("copy_nocache");
        let dst_a = dir.join("poster.jpg");
        let dst_b = dir.join("folder.jpg");

        let results =
            copy_artwork(&db, 42, "series-42-cover", "cover", None, &[&dst_a, &dst_b]).await;

        assert_eq!(results, vec![false, false]);
        assert!(!dst_a.exists(), "dst_a must not exist on cache miss");
        assert!(!dst_b.exists(), "dst_b must not exist on cache miss");

        remove_test_blob(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn copy_artwork_source_read_failure_fans_false_to_all_dests() {
        // Cache row exists but the blob file on disk has been removed
        // (e.g. manual cleanup of the blob cache dir). The source-read
        // failure path must return `[false; N]` and log once at the
        // call site rather than N times — exercised here by calling
        // the function and confirming all dests report false and
        // nothing lands on disk.
        let db = setup_artwork_only_db().await;
        let dir = unique_test_dir("copy_srcgone");
        let blob_path = test_blob_path(&dir);
        // Register the ref as if the blob were real, then remove the
        // file so `fs::read` fails.
        std::fs::write(&blob_path, b"stub").expect("write stub");
        register_blob(&db, "series-42-cover", &blob_path, "deadbeef", 4).await;
        std::fs::remove_file(&blob_path).expect("unlink blob");

        let dst_a = dir.join("poster.jpg");
        let dst_b = dir.join("folder.jpg");

        let results =
            copy_artwork(&db, 42, "series-42-cover", "cover", None, &[&dst_a, &dst_b]).await;

        assert_eq!(results, vec![false, false]);
        assert!(!dst_a.exists());
        assert!(!dst_b.exists());

        remove_test_blob(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }
}
