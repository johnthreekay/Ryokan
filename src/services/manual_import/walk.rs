//! Recursive media walk for the manual-import wizard (#122).
//!
//! Walks an arbitrary user directory (not a Ryokan-managed series
//! folder, so `media::scan_series_folder` doesn't apply) and returns
//! every video file with its size and root-relative path. Everything
//! about the walk is a preview-time decision the user can see and
//! change: hidden entries are skipped unless asked for, symlinks are
//! not followed unless asked for (arr-stack libraries commonly have
//! media-side symlinks pointing at downloads-side originals; following
//! them would import the same bytes twice), and Ryokan's own media root
//! and recycle bin are always excluded so a walk over `/mnt/anime` that
//! happens to contain `/mnt/anime/ryokan` doesn't try to re-import
//! files Ryokan already tracks.
//!
//! Permission and IO errors are per-entry: the entry is counted and
//! skipped, the walk continues. The one fatal case is a root that
//! doesn't exist or isn't a directory.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// `Anime/<Show>/<Season>/<file>` is depth 3 from a library root;
/// 8 covers a couple of extra grouping levels (`Anime/By Year/2024/...`)
/// without letting a mounted `/` walk the whole disk.
pub const DEFAULT_MAX_DEPTH: usize = 8;

/// Hard cap on candidate files per preview. A 2 TB library is a few
/// thousand episodes; 50k is well past any real collection and keeps
/// the in-memory session bounded if someone points the wizard at a
/// root with a huge non-anime tree beneath it.
pub const MAX_FILES: usize = 50_000;

/// Directory names that are never media, regardless of the hidden
/// toggle (some aren't dot-prefixed). NAS sidecar dirs, OS trash, and
/// Ryokan's own cross-fs copy staging name.
const JUNK_DIRS: &[&str] = &[
    ".thumbnails",
    ".AppleDouble",
    "@eaDir",
    "lost+found",
    "#recycle",
    "$RECYCLE.BIN",
    "System Volume Information",
    ".ryokan-tmp",
];

#[derive(Clone, Debug)]
pub struct WalkOptions {
    pub root: PathBuf,
    /// Follow symlinked files and directories. Off by default; see the
    /// module docs for why.
    pub follow_symlinks: bool,
    pub max_depth: usize,
    /// Descend into dot-prefixed directories and include dot-prefixed
    /// files. Off by default.
    pub include_hidden: bool,
    /// Subtrees to skip entirely. The job passes Ryokan's `media_root`
    /// and `recycle_bin_path`; each is canonicalized here when it
    /// exists so a symlinked root still matches.
    pub excludes: Vec<PathBuf>,
}

impl WalkOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            follow_symlinks: false,
            max_depth: DEFAULT_MAX_DEPTH,
            include_hidden: false,
            excludes: Vec::new(),
        }
    }
}

/// What the walk saw, for the preview's summary line and for the
/// "why isn't my file listed" question.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Video files returned.
    pub files: usize,
    /// Directories entered (the root counts).
    pub dirs: usize,
    pub skipped_hidden: usize,
    pub skipped_junk: usize,
    /// Entries under an excluded subtree (Ryokan's media root or bin).
    pub skipped_excluded: usize,
    /// Symlinks skipped because `follow_symlinks` is off.
    pub skipped_symlinks: usize,
    /// Regular files with a non-video extension.
    pub skipped_non_video: usize,
    /// Per-entry IO / permission errors.
    pub errors: usize,
    /// The walk stopped at [`MAX_FILES`].
    pub truncated: bool,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFile {
    /// Absolute path (root-canonical when the root canonicalized).
    pub path: PathBuf,
    /// Path relative to the walk root; what the preview shows and
    /// what the parser reads parent-folder hints from.
    pub rel_path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct WalkOutput {
    /// The root as walked (canonicalized when possible).
    pub root: PathBuf,
    pub files: Vec<RawFile>,
    pub stats: WalkStats,
}

/// True for the files Ryokan imports: the container extensions the
/// post-processing import uses, so a file this walk lists is one the
/// import step can handle, plus `.strm` stubs (issue #287), which the
/// library scan counts as the episode's file.
pub fn is_library_file(name: &str) -> bool {
    crate::services::post_processing::is_video_file(name)
        || crate::services::media::is_stream_stub(name)
}

fn is_hidden_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| s.starts_with('.'))
}

fn is_junk_dir(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| JUNK_DIRS.iter().any(|j| s.eq_ignore_ascii_case(j)))
}

/// Skip counters shared between `filter_entry` (which decides whether
/// to descend) and the main loop. `Cell` because both closures need
/// mutation through a shared borrow.
#[derive(Default)]
struct Skips {
    hidden: Cell<usize>,
    junk: Cell<usize>,
    excluded: Cell<usize>,
    symlinks: Cell<usize>,
}

/// Synchronous walk. Callers on the async runtime go through [`walk`].
pub fn walk_blocking(opts: &WalkOptions) -> Result<WalkOutput, String> {
    let root = &opts.root;
    let meta =
        std::fs::metadata(root).map_err(|e| format!("cannot read {}: {}", root.display(), e))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    // Canonicalize so exclude prefixes compare on the same footing and
    // a root given through a symlink still walks its real subtree.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
    let excludes: Vec<PathBuf> = opts
        .excludes
        .iter()
        .filter(|p| !p.as_os_str().is_empty())
        .flat_map(|p| {
            let mut v = vec![p.clone()];
            if let Ok(c) = std::fs::canonicalize(p)
                && c != *p
            {
                v.push(c);
            }
            v
        })
        .collect();
    // A root that IS an excluded subtree walks nothing; the handler
    // rejects this earlier with a clearer message, this is the backstop.
    if excludes.iter().any(|ex| root.starts_with(ex)) {
        return Err(format!(
            "{} is inside Ryokan's own media root or recycle bin",
            root.display()
        ));
    }

    let skips = Skips::default();
    let include_hidden = opts.include_hidden;
    let follow = opts.follow_symlinks;
    let walker = WalkDir::new(&root)
        .follow_links(follow)
        .max_depth(opts.max_depth)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name();
            if !include_hidden && is_hidden_name(name) {
                skips.hidden.set(skips.hidden.get() + 1);
                return false;
            }
            if e.file_type().is_dir() && is_junk_dir(name) {
                skips.junk.set(skips.junk.get() + 1);
                return false;
            }
            if !follow && e.path_is_symlink() {
                skips.symlinks.set(skips.symlinks.get() + 1);
                return false;
            }
            if excludes.iter().any(|ex| e.path().starts_with(ex)) {
                skips.excluded.set(skips.excluded.get() + 1);
                return false;
            }
            true
        });

    let mut stats = WalkStats::default();
    let mut files = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        if entry.file_type().is_dir() {
            stats.dirs += 1;
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !is_library_file(&name) {
            stats.skipped_non_video += 1;
            continue;
        }
        if files.len() >= MAX_FILES {
            stats.truncated = true;
            break;
        }
        let size_bytes = match entry.metadata() {
            Ok(m) => m.len(),
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let path = entry.path().to_path_buf();
        let rel_path = path
            .strip_prefix(&root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| PathBuf::from(entry.file_name()));
        stats.total_bytes += size_bytes;
        files.push(RawFile {
            path,
            rel_path,
            size_bytes,
        });
    }
    stats.files = files.len();
    stats.skipped_hidden = skips.hidden.get();
    stats.skipped_junk = skips.junk.get();
    stats.skipped_excluded = skips.excluded.get();
    stats.skipped_symlinks = skips.symlinks.get();

    Ok(WalkOutput { root, files, stats })
}

/// `spawn_blocking` wrapper: a deep tree on a network mount can stall
/// for seconds, which must not land on a Tokio worker.
pub async fn walk(opts: WalkOptions) -> Result<WalkOutput, String> {
    tokio::task::spawn_blocking(move || walk_blocking(&opts))
        .await
        .map_err(|e| format!("walk task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, b"x").unwrap();
    }

    fn rel_names(out: &WalkOutput) -> Vec<String> {
        let mut v: Vec<String> = out
            .files
            .iter()
            .map(|f| f.rel_path.to_string_lossy().replace('\\', "/"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn lists_strm_stubs_beside_videos() {
        // Issue #287: a `.strm` left where an episode's video was is an
        // episode file to the library scan, so the import lists it too.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("Frieren/Sousou no Frieren - 05.mkv"));
        touch(&root.join("Frieren/Sousou no Frieren - 06.strm"));

        let out = walk_blocking(&WalkOptions::new(root)).unwrap();
        assert_eq!(
            rel_names(&out),
            vec![
                "Frieren/Sousou no Frieren - 05.mkv",
                "Frieren/Sousou no Frieren - 06.strm",
            ]
        );
        assert_eq!(out.stats.skipped_non_video, 0);
    }

    #[test]
    fn lists_video_files_relative_to_root_and_skips_other_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("Naruto/Season 01/Naruto - 01.mkv"));
        touch(&root.join("Naruto/Season 01/Naruto - 01.nfo"));
        touch(&root.join("Naruto/Season 01/Naruto - 02.mp4"));
        touch(&root.join("poster.jpg"));

        let out = walk_blocking(&WalkOptions::new(root)).unwrap();
        assert_eq!(
            rel_names(&out),
            vec![
                "Naruto/Season 01/Naruto - 01.mkv",
                "Naruto/Season 01/Naruto - 02.mp4",
            ]
        );
        assert_eq!(out.stats.files, 2);
        assert_eq!(out.stats.skipped_non_video, 2);
        assert_eq!(out.stats.total_bytes, 2);
        assert!(out.stats.dirs >= 3);
    }

    #[test]
    fn hidden_entries_skipped_unless_opted_in() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join(".hidden/Show - 01.mkv"));
        touch(&root.join("Show/.Show - 02.mkv"));
        touch(&root.join("Show/Show - 03.mkv"));

        let out = walk_blocking(&WalkOptions::new(root)).unwrap();
        assert_eq!(rel_names(&out), vec!["Show/Show - 03.mkv"]);
        assert_eq!(out.stats.skipped_hidden, 2);

        let mut opts = WalkOptions::new(root);
        opts.include_hidden = true;
        let out = walk_blocking(&opts).unwrap();
        assert_eq!(out.stats.files, 3);
        assert_eq!(out.stats.skipped_hidden, 0);
    }

    #[test]
    fn junk_dirs_are_always_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("@eaDir/Show - 01.mkv"));
        touch(&root.join("lost+found/Show - 01.mkv"));
        touch(&root.join("Show/Show - 01.mkv"));

        let mut opts = WalkOptions::new(root);
        opts.include_hidden = true;
        let out = walk_blocking(&opts).unwrap();
        assert_eq!(rel_names(&out), vec!["Show/Show - 01.mkv"]);
        assert_eq!(out.stats.skipped_junk, 2);
    }

    #[test]
    fn excluded_subtree_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("ryokan/Tracked/Season 01/Tracked - 01.mkv"));
        touch(&root.join("Other/Other - 01.mkv"));

        let mut opts = WalkOptions::new(root);
        opts.excludes = vec![root.join("ryokan")];
        let out = walk_blocking(&opts).unwrap();
        assert_eq!(rel_names(&out), vec!["Other/Other - 01.mkv"]);
        assert_eq!(out.stats.skipped_excluded, 1);
    }

    #[test]
    fn root_inside_exclude_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("media");
        touch(&root.join("Show/Show - 01.mkv"));
        let mut opts = WalkOptions::new(root.join("Show"));
        opts.excludes = vec![root.clone()];
        let err = walk_blocking(&opts).unwrap_err();
        assert!(err.contains("inside Ryokan's own media root"), "{err}");
    }

    #[test]
    fn missing_root_and_file_root_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert!(walk_blocking(&WalkOptions::new(&missing)).is_err());
        let file = tmp.path().join("file.mkv");
        touch(&file);
        let err = walk_blocking(&WalkOptions::new(&file)).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn max_depth_is_honored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a/b/c/deep - 01.mkv"));
        touch(&root.join("shallow - 01.mkv"));
        let mut opts = WalkOptions::new(root);
        opts.max_depth = 2;
        let out = walk_blocking(&opts).unwrap();
        assert_eq!(rel_names(&out), vec!["shallow - 01.mkv"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_follow_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().join("root");
        touch(&root.join("real/Show - 01.mkv"));
        // A linked directory and a linked file, both pointing at a
        // sibling of the walk root so they are only reachable through
        // the links.
        let outside = tmp.path().join("outside");
        touch(&outside.join("Linked - 01.mkv"));
        std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("Linked - 01.mkv"),
            root.join("real/linkfile - 02.mkv"),
        )
        .unwrap();

        let out = walk_blocking(&WalkOptions::new(root)).unwrap();
        assert_eq!(rel_names(&out), vec!["real/Show - 01.mkv"]);
        assert_eq!(out.stats.skipped_symlinks, 2);

        let mut opts = WalkOptions::new(root);
        opts.follow_symlinks = true;
        let out = walk_blocking(&opts).unwrap();
        assert_eq!(
            rel_names(&out),
            vec![
                "linkdir/Linked - 01.mkv",
                "real/Show - 01.mkv",
                "real/linkfile - 02.mkv",
            ]
        );
        assert_eq!(out.stats.skipped_symlinks, 0);
    }
}
