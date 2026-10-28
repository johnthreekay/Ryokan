use anitomy::Anitomy;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// The file-name parser (`parse_episode_span` and friends) lives in
// `release_title::file_mode`; these re-exports keep its call sites.
pub use crate::services::release_title::file_mode::{
    EpisodeSpan, MAX_FILE_SPAN, ReleaseRevision, has_special_marker, is_special_release,
    parse_episode_number, parse_episode_span, parse_episode_span_for, parse_release_revision,
    parse_release_version,
};

/// Sanitize a string for use as a folder name on disk.
/// Replaces filesystem-unsafe characters and trims leading/trailing dots and whitespace.
pub fn sanitize_folder_name(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        // Dots and whitespace in one pass: trimming dots first and
        // whitespace second turned " .. " into "..", a provider title
        // that named the media root's parent as a series folder.
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_string()
}

/// Longest input handed to anitomy. Its number parser runs a C++
/// `std::regex` that recurses once per character of a token; with GCC 14
/// (the Docker builder) a token of about 8,000 characters overflows the
/// thread stack and aborts the process, and a release title from an
/// indexer, an RSS feed or autobrr can be that long. Real titles stay
/// under 300 bytes, so nothing real is cut.
pub(crate) const ANITOMY_MAX_INPUT: usize = 1024;

/// Longest release title Ryokan accepts from an indexer, a feed or an
/// autobrr push; longer ones are dropped (or refused) where they enter.
/// Real titles stay under 300 bytes, and a megabyte "title" only exists
/// to stress the parsers behind it.
pub(crate) const MAX_RELEASE_TITLE_BYTES: usize = 1024;

/// Every anitomy parse goes through here: NUL bytes (anitomy rejects
/// them) removed and the input cut to [`ANITOMY_MAX_INPUT`] bytes on a
/// char boundary. anitomy reports `Err` when it finds no title but
/// fills the elements either way, so both are returned.
pub(crate) fn anitomy_parse(input: &str) -> anitomy::Elements {
    match Anitomy::new().parse(anitomy_input(input)) {
        Ok(elements) | Err(elements) => elements,
    }
}

/// What [`anitomy_parse`] hands anitomy: at most [`ANITOMY_MAX_INPUT`]
/// bytes, cut on a char boundary, with NUL bytes removed.
fn anitomy_input(input: &str) -> std::borrow::Cow<'_, str> {
    let mut end = input.len().min(ANITOMY_MAX_INPUT);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let bounded = &input[..end];
    if bounded.contains('\0') {
        std::borrow::Cow::Owned(bounded.replace('\0', ""))
    } else {
        std::borrow::Cow::Borrowed(bounded)
    }
}

/// Whether a stored `series.folder_name` can be joined onto the media
/// root: one non-empty path component that isn't `.` or `..`. A folder
/// name of `.` is the media root itself: its scan reads every series'
/// files as its own, and removing the series with files would recycle
/// or delete the whole library. New names can't come out that way
/// ([`sanitize_folder_name`]), but a row written before that fix can.
pub fn usable_folder_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && !name.chars().all(|c| c == '.') && !name.contains(['/', '\\', '\0'])
}

/// A file found on disk that represents an episode, or several (issue
/// #246): `episode_number` is the first episode the file holds and
/// `episode_last` the last, equal for the ordinary one-episode file.
/// Consumers that build a "have" set walk [`EpisodeFile::episodes`] so a
/// `S01E05-E06` file counts for both slots.
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeFile {
    pub filename: String,
    pub episode_number: i32,
    pub episode_last: i32,
    pub season_number: Option<i32>,
    pub quality: String,
    pub size_bytes: u64,
    pub size_display: String,
    /// File modification time as Unix seconds, when the scan could
    /// stat it. The upgrade policy's RSS revision window reads it as
    /// the file's age (a hardlinked import keeps the download's
    /// mtime, a copy gets the import's; both are "when it arrived").
    pub modified_secs: Option<i64>,
    /// The file sits in the Specials folder or is numbered `S00Exx`:
    /// kept out of the have-sets, since its number is not an episode of
    /// the series. Decided by place and season only, never by a name
    /// marker, so an OVA-format series whose files say `OVA 01` keeps
    /// every one of them. `scan_series_folder` drops these;
    /// `scan_series_folder_split` lists them.
    pub is_special: bool,
}

impl EpisodeFile {
    /// Whether the file is in Ryokan's own numbering: no season in its
    /// name, or season 1 (every import names files `S01Exx`). A merged
    /// Sonarr-style folder can also hold `Season 02/Show - S02E05.mkv`,
    /// which reads as episode 5 too; a delete or replace of episode 5
    /// picks the own-season file when there is one.
    pub fn is_own_season(&self) -> bool {
        self.season_number.is_none_or(|n| n == 1)
    }

    /// Every episode number the file holds, first to last.
    pub fn episodes(&self) -> std::ops::RangeInclusive<i32> {
        self.episode_number..=self.episode_last
    }

    /// True for a multi-episode file (`S01E05-E06`).
    pub fn is_multi_episode(&self) -> bool {
        self.episode_last > self.episode_number
    }

    /// True when `episode` is one of the episodes this file holds.
    pub fn holds(&self, episode: i32) -> bool {
        self.episodes().contains(&episode)
    }

    /// The file's span as parsed from its name.
    pub fn span(&self) -> EpisodeSpan {
        EpisodeSpan {
            season: self.season_number,
            first: self.episode_number,
            last: self.episode_last,
            special: self.is_special,
        }
    }
}

/// Scan a series folder for video files and parse episode info from
/// filenames. The recursive `std::fs::read_dir` walk runs inside
/// `spawn_blocking` so a deep multi-season tree on a slow / network-
/// mounted media root can't stall a Tokio worker — Sonarr/Radarr poll
/// the compat handlers aggressively and the RSS/upgrade/post-processing
/// background tasks share the same runtime.
pub async fn scan_series_folder(media_root: &str, folder_name: &str) -> Vec<EpisodeFile> {
    if media_root.is_empty() || folder_name.is_empty() {
        return Vec::new();
    }

    let media_root = media_root.to_string();
    let folder_name = folder_name.to_string();
    tokio::task::spawn_blocking(move || scan_series_folder_blocking(&media_root, &folder_name))
        .await
        .unwrap_or_default()
}

/// Batch variant of `scan_series_folder` for the library index's
/// per-card completeness bars: one `spawn_blocking` hop for the whole
/// library instead of one per series. Each entry is (series_id,
/// folder_name); the result maps series_id to that folder's files.
/// A readdir per series is cheap (single-digit ms for a whole
/// library on local disk), but N spawn_blocking hops wouldn't be.
pub async fn scan_series_folders_batch(
    media_root: &str,
    folders: Vec<(i64, String)>,
) -> std::collections::HashMap<i64, Vec<EpisodeFile>> {
    if media_root.is_empty() || folders.is_empty() {
        return std::collections::HashMap::new();
    }
    let media_root = media_root.to_string();
    tokio::task::spawn_blocking(move || {
        folders
            .into_iter()
            .map(|(id, folder)| (id, scan_series_folder_blocking(&media_root, &folder)))
            .collect()
    })
    .await
    .unwrap_or_default()
}

fn scan_series_folder_blocking(media_root: &str, folder_name: &str) -> Vec<EpisodeFile> {
    scan_series_folder_split(media_root, folder_name).0
}

/// Every video in a series folder: `(episodes, specials)`. Specials
/// (`S00Exx`, anything under `Specials/`) are listed apart so no
/// have-set counts an OVA's `01` as episode 1.
pub fn scan_series_folder_split(
    media_root: &str,
    folder_name: &str,
) -> (Vec<EpisodeFile>, Vec<EpisodeFile>) {
    if !usable_folder_name(folder_name) {
        return (Vec::new(), Vec::new());
    }
    let series_path = Path::new(media_root).join(folder_name);
    if !series_path.is_dir() {
        return (Vec::new(), Vec::new());
    }

    let mut files = Vec::new();
    scan_dir_recursive(
        &series_path,
        &series_path,
        &mut files,
        0,
        &mut HashSet::new(),
    );
    let (specials, mut files): (Vec<EpisodeFile>, Vec<EpisodeFile>) =
        files.into_iter().partition(|f| f.is_special);

    // Sort by season then episode number.
    files.sort_by(|a, b| {
        a.season_number
            .unwrap_or(0)
            .cmp(&b.season_number.unwrap_or(0))
            .then(a.episode_number.cmp(&b.episode_number))
    });

    (files, specials)
}

/// List top-level directories in the media root.
pub fn list_media_folders(media_root: &str) -> Vec<String> {
    if media_root.is_empty() {
        return Vec::new();
    }

    let root = Path::new(media_root);
    if !root.is_dir() {
        return Vec::new();
    }

    let mut folders = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                folders.push(name.to_string());
            }
        }
    }
    folders.sort_by_key(|a| a.to_lowercase());
    folders
}

/// How far below a series folder the scan reads. Real layouts are one or
/// two levels deep (`Season 01/`, `Specials/`, a release folder inside).
const MAX_SCAN_DEPTH: usize = 6;

/// Symlinked folders are followed (a season linked in from another
/// drive is a real layout), but each real directory is read once and
/// never past [`MAX_SCAN_DEPTH`]: a link back to a parent folder used to
/// recurse until the stack overflowed and the process aborted.
fn scan_dir_recursive(
    dir: &Path,
    series_root: &Path,
    files: &mut Vec<EpisodeFile>,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(real) = dir.canonicalize() else {
        return;
    };
    if !visited.insert(real) {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, series_root, files, depth + 1, visited);
        } else if is_library_file(&path)
            && let Some(ep) = parse_episode_file(&path, series_root)
        {
            files.push(ep);
        }
    }
}

/// A file the scan counts as an episode: a video, or a `.strm` stub
/// standing in for one.
fn is_library_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext.to_lowercase().as_str(),
            "mkv" | "mp4" | "avi" | "wmv" | "flv" | "webm" | "m4v" | "ts" | "strm"
        ),
        None => false,
    }
}

/// True for a `.strm` stub (issue #287): a small text file holding a
/// stream URL, which Jellyfin, Emby and Kodi play in place of the video.
/// Setups that move media off the server leave one behind under the
/// episode's own name. The library counts it as the episode's file;
/// Ryokan never reads what is inside, never probes it, and never imports
/// one from a download, where the URL would be a release's to choose.
pub fn is_stream_stub(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("strm"))
}

fn parse_episode_file(path: &Path, series_root: &Path) -> Option<EpisodeFile> {
    let basename = path.file_name()?.to_str()?.to_string();
    let lower = basename.to_lowercase();

    let span = parse_episode_span(&lower)?;

    let quality = parse_quality(&lower);

    let metadata = std::fs::metadata(path).ok();
    let size_bytes = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
    // A `.strm` stub is a few bytes of text; its size says nothing
    // about the episode, and "0 MiB" read like a broken file.
    let size_display = if is_stream_stub(&basename) {
        "Stream link".to_string()
    } else {
        format_size(size_bytes)
    };
    let modified_secs = metadata
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

    // Store relative path from series root so delete handler can reconstruct full path.
    let filename = path
        .strip_prefix(series_root)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or(&basename)
        .to_string();

    let is_special = span.season == Some(0) || path_names_specials(Path::new(&filename));
    Some(EpisodeFile {
        filename,
        episode_number: span.first,
        episode_last: span.last,
        season_number: span.season,
        quality,
        size_bytes,
        size_display,
        modified_secs,
        is_special,
    })
}

/// True when a path inside a download or series folder has a
/// `Specials` directory segment (case-insensitive): a BD pack's own
/// `Specials/` subfolder, or Ryokan's own [`SPECIALS_FOLDER`]. The
/// folder is authoritative whatever the file is named.
///
/// [`SPECIALS_FOLDER`]: crate::services::post_processing::SPECIALS_FOLDER
pub fn path_names_specials(path: &Path) -> bool {
    path.parent().is_some_and(|parent| {
        parent.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("specials"))
        })
    })
}

/// Whether a special marker in a file name means "special" for this
/// series: only for a TV entry. For an entry that is itself an OVA /
/// ONA / special / movie the marker is part of the series' identity
/// and its files are ordinary episodes. AniList formats: `TV`,
/// `TV_SHORT`, `MOVIE`, `SPECIAL`, `OVA`, `ONA`, `MUSIC`.
pub fn is_tv_format(format: &str) -> bool {
    matches!(
        format.trim().to_ascii_uppercase().as_str(),
        "TV" | "TV_SHORT"
    )
}

/// Extract quality/resolution from filename.
fn parse_quality(lower: &str) -> String {
    // Source type. Names match `ClassificationResult::label()` in
    // `source.rs` so disk-scanned fallback strings render the same as
    // classifier-derived ones ("BD-1080p", "WEB-1080p", etc.). This
    // path can't distinguish BD Remux / BD-RAW from filename tokens
    // alone; the classifier handles those through the structured
    // columns so the quality shown for a properly-classified episode
    // comes from `tag.quality_tag`, not from here.
    let source = if lower.contains("bluray")
        || lower.contains("blu-ray")
        || lower.contains("bdrip")
        || lower.contains("[bd")
        || lower.contains("(bd")
    {
        "BD"
    } else if lower.contains("webrip") || lower.contains("web-rip") {
        "WEBRip"
    } else if lower.contains("webdl")
        || lower.contains("web-dl")
        || lower.contains("web dl")
        || lower.contains("web")
    {
        // Unified WEB label — matches `ClassificationResult::label()`
        // which collapses WebDl and bare Web into the same output
        // (issue #48). WebRip stays distinct above because it's the
        // lower-quality sub-tier power users want to spot.
        "WEB"
    } else if lower.contains("hdtv") {
        "HDTV"
    } else if lower.contains("dvdrip") || lower.contains("dvd") {
        "DVD"
    } else {
        ""
    };

    // Resolution.
    let res = if lower.contains("2160p") || lower.contains("4k") || lower.contains("uhd") {
        "2160p"
    } else if lower.contains("1080p") || lower.contains("1080") {
        "1080p"
    } else if lower.contains("720p") || lower.contains("720") {
        "720p"
    } else if lower.contains("480p") || lower.contains("480") {
        "480p"
    } else {
        ""
    };

    match (source, res) {
        ("", "") => String::new(),
        ("", r) => r.to_string(),
        (s, "") => s.to_string(),
        (s, r) => format!("{}-{}", s, r),
    }
}

fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{:.1} GiB", gb)
    } else {
        let mb = bytes as f64 / (1024.0 * 1024.0);
        format!("{:.0} MiB", mb)
    }
}

#[cfg(test)]
mod folder_name_tests {
    use super::{
        ANITOMY_MAX_INPUT, anitomy_input, anitomy_parse, sanitize_folder_name, usable_folder_name,
    };

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_read_once_and_never_overflows() {
        // `Show/Season 01/loop -> ..` used to recurse until the stack
        // overflowed. A season linked in from elsewhere still counts.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("media");
        let show = root.join("Show");
        let s1 = show.join("Season 01");
        std::fs::create_dir_all(&s1).unwrap();
        std::fs::write(s1.join("Show - S01E01.mkv"), b"x").unwrap();
        std::os::unix::fs::symlink(&show, s1.join("loop")).unwrap();
        let elsewhere = tmp.path().join("disk2").join("Season 02");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("Show - S02E01.mkv"), b"x").unwrap();
        std::os::unix::fs::symlink(&elsewhere, show.join("Season 02")).unwrap();

        let (files, _) = super::scan_series_folder_split(root.to_str().unwrap(), "Show");
        let mut names: Vec<_> = files.iter().map(|f| f.filename.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            ["Season 01/Show - S01E01.mkv", "Season 02/Show - S02E01.mkv"]
        );
    }

    #[test]
    fn a_strm_stub_counts_as_the_episodes_file() {
        // Issue #287: an episode moved off the server leaves a `.strm`
        // under its own name, and the scan counts it like the video it
        // stands in for.
        let tmp = tempfile::tempdir().unwrap();
        let s1 = tmp.path().join("Show").join("Season 01");
        std::fs::create_dir_all(&s1).unwrap();
        std::fs::write(s1.join("Show - S01E05.mkv"), b"x").unwrap();
        std::fs::write(s1.join("Show - S01E06.STRM"), b"https://example.com/6").unwrap();
        std::fs::write(s1.join("Show - S01E07.txt"), b"x").unwrap();

        let (files, _) = super::scan_series_folder_split(tmp.path().to_str().unwrap(), "Show");
        let found: Vec<_> = files
            .iter()
            .map(|f| (f.filename.as_str(), f.episode_number))
            .collect();
        assert_eq!(
            found,
            [
                ("Season 01/Show - S01E05.mkv", 5),
                ("Season 01/Show - S01E06.STRM", 6)
            ]
        );
        assert_eq!(files[1].size_display, "Stream link");
    }

    #[test]
    fn is_stream_stub_reads_the_extension_only() {
        assert!(super::is_stream_stub("Show - 06.strm"));
        assert!(super::is_stream_stub("Season 01/Show - 06.Strm"));
        assert!(!super::is_stream_stub("Show - 06.mkv"));
        assert!(!super::is_stream_stub("Show.strm - 06.mkv"));
        assert!(!super::is_stream_stub("strm"));
    }

    #[test]
    fn anitomy_never_sees_more_than_its_bound() {
        // A ~8,000-character token overflowed anitomy's recursive
        // std::regex (GCC 14) and aborted the process.
        let digits = "1".repeat(20_000) + "x";
        assert_eq!(anitomy_input(&digits).len(), ANITOMY_MAX_INPUT);
        let _ = anitomy_parse(&digits);
        // Cut on a char boundary, NULs removed.
        let wide = "\u{3042}".repeat(1_000);
        let cut = anitomy_input(&wide);
        assert!(cut.len() <= ANITOMY_MAX_INPUT && cut.chars().all(|c| c == '\u{3042}'));
        assert_eq!(anitomy_input("[G] Show\0 - 01.mkv"), "[G] Show - 01.mkv");
        // Real titles pass through untouched.
        let real = "[Group] Show Title - 28 (1080p) [ABCD1234].mkv";
        assert_eq!(anitomy_input(real), real);
    }

    #[test]
    fn dots_and_spaces_never_survive_at_the_edges() {
        // Trimming dots and then spaces turned " .. " into "..".
        for title in [
            " .. ",
            " . .. . ",
            " . . . ",
            "\u{3000}.\u{3000}",
            ". ",
            " .",
            "...",
        ] {
            assert_eq!(sanitize_folder_name(title), "", "{title:?}");
        }
        assert_eq!(sanitize_folder_name(".hack//Sign"), "hack__Sign");
        assert_eq!(sanitize_folder_name(" Show. "), "Show");
        assert_eq!(sanitize_folder_name("Dr. Stone"), "Dr. Stone");
    }

    #[test]
    fn only_a_real_component_is_a_usable_folder_name() {
        for bad in ["", "  ", ".", "..", "...", "a/b", "a\\b", "../x", "a\0b"] {
            assert!(!usable_folder_name(bad), "{bad:?}");
        }
        for good in ["Show", "Dr. Stone", ".hack", "Show (2011)"] {
            assert!(usable_folder_name(good), "{good:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_episode_number;

    // Fixtures here are real-world filenames captured verbatim from
    // an actual library, lower-cased to match the parser's input. They
    // are kept verbatim so we have genuine ground truth for what
    // shapes the parser must handle — any divergence between these
    // fixtures and what we produce in synthetic tests would be the
    // silent regression hazard the no-fabricated-release-naming
    // feedback exists to prevent.

    fn parse(name: &str) -> Option<(Option<i32>, i32)> {
        parse_episode_number(&name.to_lowercase())
    }

    // ── SxxExx (RE_SXEX) ─────────────────────────────────────────────

    #[test]
    fn sxxexx_dot_separated_full_title() {
        // smol releases use dot-separated tokens.
        assert_eq!(
            parse("Whisper.Me.a.Love.Song.S01E06.REPACK.1080p.WEBRip.DDP2.0.x265-smol.mkv"),
            Some((Some(1), 6))
        );
        assert_eq!(
            parse("Nisekoi.False.Love.S02E11.1080p.BluRay.Opus2.0.x265-smol.mkv"),
            Some((Some(2), 11))
        );
    }

    #[test]
    fn sxxexx_with_v2_suffix_after_episode() {
        // S02E10v2 — the v2 must not corrupt the episode capture.
        assert_eq!(parse("[Judas] Frieren - S02E10v2.mkv"), Some((Some(2), 10)));
        assert_eq!(
            parse("[Judas] Fire Force - S03E18v2.mkv"),
            Some((Some(3), 18))
        );
        assert_eq!(
            parse("JUJUTSU.KAISEN.S03E03.v2.1080p.CR.WEBRip.DUAL.AUDIO.AV1-Sokudo.mkv"),
            Some((Some(3), 3))
        );
    }

    #[test]
    fn sxxexx_minimal_bracketed_group() {
        assert_eq!(parse("[Judas] Kaya-chan - S01E01.mkv"), Some((Some(1), 1)));
    }

    #[test]
    fn sxxexx_dot_separated_with_long_subtitle() {
        // VARYG-style: long descriptive sentence between SxxExx and quality.
        assert_eq!(
            parse(
                "You.and.I.Are.Polar.Opposites.S01E05.Someone.Who.Thinks.and.Someone.Who.Doesnt.1080p.CR.WEB-DL.DUAL.AAC2.0.H.264-VARYG.mkv"
            ),
            Some((Some(1), 5))
        );
    }

    #[test]
    fn sxxexx_space_separated_with_quality_bracket() {
        assert_eq!(
            parse("Jujutsu Kaisen - S02E10 (BD 1080p HEVC) [Vodes].mkv"),
            Some((Some(2), 10))
        );
        assert_eq!(
            parse("[LostYears] Vinland Saga - S02E12 (WEB 1080p HEVC E-AC-3 AAC) [B413DFCD].mkv"),
            Some((Some(2), 12))
        );
        assert_eq!(
            parse("[LostYears] Vinland Saga - S02E13 (WEB 1080p HEVC AAC) [A6F924F6].mkv"),
            Some((Some(2), 13))
        );
    }

    // ── ` - NN ` dash-delimited (RE_DASH_EP) ─────────────────────────

    #[test]
    fn dash_ep_with_quality_and_hash_brackets() {
        assert_eq!(
            parse("[MTBB] Mob Psycho 100 S2 - 10 (BD 1080p) [BED02FC5].mkv"),
            Some((None, 10))
        );
        assert_eq!(
            parse("[Salieri] Jujutsu Kaisen S2 - 06 (1080p) (HDR) [Dual Audio].mkv"),
            Some((None, 6))
        );
        assert_eq!(
            parse("[Kaizoku] Jujutsu Kaisen - 35 (BD 1080p) [3311E835].mkv"),
            Some((None, 35))
        );
        assert_eq!(
            parse("[Kaizoku] Jujutsu Kaisen - 47 (BD 1080p) [3D93127E].mkv"),
            Some((None, 47))
        );
    }

    #[test]
    fn dash_ep_with_square_quality_bracket() {
        // sam / notFoxtrot use `[BD 1080p FLAC]` instead of `(...)`.
        assert_eq!(
            parse("[sam] Vinland Saga - 04 [BD 1080p FLAC] [22FE1988].mkv"),
            Some((None, 4))
        );
        assert_eq!(
            parse("[notFoxtrot] Vinland Saga - 05 [BD 1080p FLAC] [7F2FF84F].mkv"),
            Some((None, 5))
        );
    }

    #[test]
    fn dash_ep_with_end_marker_after_number() {
        // NanakoRaws appends ` END` to the final episode of a season.
        assert_eq!(
            parse("[NanakoRaws] Jujutsu Kaisen S2 - 23 END (1080p).mkv"),
            Some((None, 23))
        );
    }

    #[test]
    fn dash_ep_with_v2_suffix_glued_to_number() {
        // ` - 16v2 ` — version suffix is consumed by the trailing alt.
        assert_eq!(
            parse("[NanakoRaws] Jujutsu Kaisen S2 - 16v2 (1080p).mkv"),
            Some((None, 16))
        );
    }

    // ── Bare number before ` - subtitle` (RE_BARE_NUM_DASH) ──────────

    #[test]
    fn bare_number_before_dash_subtitle_no_group_prefix() {
        // No group bracket, no leading ` - `, just `<title> NN - <subtitle>`.
        // Returned None prior to RE_BARE_NUM_DASH.
        assert_eq!(
            parse("Koyomimonogatari 01 - Koyomi Stone.mkv"),
            Some((None, 1))
        );
        assert_eq!(
            parse("Owarimonogatari 2nd Season 03 - Hitagi Rendezvous, Part 1.mkv"),
            Some((None, 3))
        );
        assert_eq!(
            parse("Nisemonogatari 06 - Karen Bee, Part 6.mkv"),
            Some((None, 6))
        );
    }

    #[test]
    fn sxxexx_with_subtitle_and_quality_brackets() {
        // `[Group] Title - SxxExx - Episode Subtitle (quality) [hash].mkv`
        // Two ` - ` delimiters, parenthesized quality, bracketed hash.
        // Regression guard: RE_SXEX must win before the bare-number
        // branches fire on the " - <subtitle>" portion.
        assert_eq!(
            parse(
                "[smol] Monogatari - S07E05 - Owarimonogatari (BD 1080p HEVC Opus) [A89E97DA].mkv"
            ),
            Some((Some(7), 5))
        );
        assert_eq!(
            parse(
                "[smol] Monogatari - S07E14 - Owarimonogatari Second Season (Ge) (BD 1080p HEVC Opus) [EBAB5DDD].mkv"
            ),
            Some((Some(7), 14))
        );
    }

    // ── "Episode N" (RE_EPISODE) ─────────────────────────────────────

    #[test]
    fn episode_word_prefix_with_three_digit_number() {
        // Long-runner with no other tokens in the filename.
        assert_eq!(parse("Episode 039.mkv"), Some((None, 39)));
    }

    // ── NCOP/NCED non-episodic guard ─────────────────────────────────

    #[test]
    fn nced_non_episode_returns_none() {
        // Creditless ending. Without the NCOP/NCED guard, the
        // bare-number-before-bracket fallback would otherwise parse
        // ` 1 [` as episode 1 and clobber the real ep1 on import.
        assert_eq!(
            parse("[notFoxtrot] Vinland Saga - NCED 1 [BD 1080p FLAC] [CC708A2C].mkv"),
            None
        );
    }

    #[test]
    fn ncop_non_episode_returns_none() {
        assert_eq!(
            parse("[notFoxtrot] Vinland Saga - NCOP 1 [BD 1080p FLAC] [300FAD69].mkv"),
            None
        );
    }

    // ── Non-episode files that have no number anywhere ───────────────

    #[test]
    fn ova_with_no_episode_number_returns_none() {
        // A single OVA file with no episode marker. Today this returns
        // None because no pattern matches, and that's the correct
        // outcome — there's nothing to parse.
        assert_eq!(
            parse(
                "[WBDP] Yahari Ore no Seishun Love Comedy wa Machigatteiru Zoku OVA [BD][1080p-AAC] [93937CE2].mkv"
            ),
            None
        );
    }

    #[test]
    fn pv_promo_video_returns_none() {
        // Bare `PV.mkv` — no digits at all, nothing to parse.
        assert_eq!(parse("PV.mkv"), None);
    }

    #[test]
    fn bd_stream_file_with_only_zero_padded_index_returns_none() {
        // Raw BD stream file from an untitled disc rip. The leading
        // `00000` is a disc-side playlist index, not an episode
        // number — both RE_BARE_NUM_DASH and RE_BARE_NUM_BRACKET
        // require a delimiter after the digits, so this correctly
        // falls through to None.
        assert_eq!(parse("00000.m2ts"), None);
    }

    #[test]
    fn folder_icon_with_arc_number_in_title_returns_none() {
        // Hypothetical: if the folder-icon PNG had been served as
        // media (it isn't — is_media_filename gates .png out), the
        // `Arc 5 Sasuke` title token must NOT be mis-parsed as
        // episode 5. Defensive regression guard for the bare-number
        // branches.
        assert_eq!(
            parse("Naruto Arc 5 Sasuke Recovery Mission Arc Folder Icon.mkv"),
            None
        );
    }

    // ── OVA episode marker (RE_OVA_EP) ───────────────────────────────

    #[test]
    fn ova_numbered_after_dash_resolves_to_episode() {
        // Multi-episode OVA releases with explicit `OVA NN` marker.
        // The 1993 and 2000 JoJo OVAs are 6 and 13 episodes
        // respectively — collapsing them all onto episode 1 (the
        // pre-RE_OVA_EP behavior via grab.episode_numbers fallback)
        // would silently corrupt the library.
        assert_eq!(parse("[Judas] JoJo 1993 - OVA 01.mkv"), Some((None, 1)));
        assert_eq!(parse("[Judas] JoJo 2000 - OVA 01.mkv"), Some((None, 1)));
    }

    #[test]
    fn bare_ova_with_no_trailing_digit_returns_none() {
        // Single-OVA AL entries (e.g. Nichijou no 0, HSotD OVA)
        // have bare `OVA` with no digit. RE_OVA_EP must NOT fire
        // (it requires a captured digit), and no other branch
        // should either. Nichijou specifically is a real AL entry
        // (https://anilist.co/anime/8857) where mis-parsing `OVA`
        // as an episode index could have disastrous outcomes.
        assert_eq!(parse("[Judas] Nichijou - OVA.mkv"), None);
        assert_eq!(
            parse("[Polarwindz] High School of the Dead - OVA (BD 1080p HEVC Dual Audio).mkv"),
            None
        );
    }

    #[test]
    fn s00_ova_subtitle_does_not_shadow_sxxexx() {
        // `<title> - S00E09 OVA <subtitle>` — the `OVA` sits in the
        // subtitle portion, not as an episode marker. RE_SXEX must
        // win first and return the real season 0 / episode 9 so the
        // OVA token is harmless context, not a re-parse hazard.
        assert_eq!(
            parse(
                "[Sokudo] Boku no Hero Academia - S00E09 OVA Make It Do-or-Die Survival Training Part 2 [1080p AV1][dual audio].mkv"
            ),
            Some((Some(0), 9))
        );
    }

    // ── Non-episodic marker guard on RE_BARE_NUM_DASH ────────────────

    #[test]
    fn season_number_before_ova_marker_is_not_episode() {
        // `<title> N - OVA - <subtitle>.mkv` — the `N` is a season
        // label (Chihayafuru 2), not an episode number. Without the
        // marker guard, bare-num-dash would naively capture `2` and
        // import the OVA file as episode 2 of the main series.
        assert_eq!(
            parse("[Judas] CHIHAYAFURU 2 - OVA - Waga Miyo ni Furu Nagame Shima ni.mkv"),
            None
        );
    }

    #[test]
    fn title_side_season_digit_does_not_shadow_dash_ep() {
        // `<Title> <season-digit> - NN.mkv` — when a title-side season
        // number (e.g. "Show 2") sits before the real ` - NN` episode
        // marker, RE_DASH_EP must win because it runs before
        // RE_BARE_NUM_DASH. If someone ever reorders the dispatch so
        // that RE_BARE_NUM_DASH fires first, this case would quietly
        // resolve to the season number (2) instead of the episode
        // (1) and import files under the wrong slot. Regression guard
        // to pin the ordering.
        assert_eq!(parse("show 2 - 01.mkv"), Some((None, 1)));
        assert_eq!(parse("[group] show 2 - 01 [1080p].mkv"), Some((None, 1)));
        assert_eq!(
            parse("[group] show 2 - 05 - subtitle [1080p].mkv"),
            Some((None, 5))
        );
    }

    // ── NCED/NCOP with glued digits (token-boundary guard) ───────────

    #[test]
    fn nced_glued_to_number_returns_none() {
        // LOGH-style `NCED01a` / `NCOP01` — number is glued directly
        // to the creditless marker. The top-level RE_NCOP_NCED guard
        // anchors the left edge to start-of-string or a non-letter so
        // glued-digit variants still trip while incidental substring
        // hits in unrelated words are ignored.
        assert_eq!(parse("show-nced01a.mkv"), None);
        assert_eq!(parse("show-ncop01.mkv"), None);
    }

    #[test]
    fn synced_substring_does_not_false_positive_as_nced() {
        // Regression: the old `contains("nced")` guard tripped on any
        // filename containing the letters `n-c-e-d` — including common
        // English words like `synced`, `announced`, `convinced`. The
        // token-boundary regex requires start-of-string or a non-letter
        // before the marker so these pass through to the real parser.
        assert_eq!(
            parse("[group] synced title - 05 [1080p].mkv"),
            Some((None, 5))
        );
        assert_eq!(
            parse("[group] announced show - 12 [1080p].mkv"),
            Some((None, 12))
        );
    }

    // ── Special / SP marker (no SxxExx, out-of-scope) ────────────────

    #[test]
    fn s03sp_special_marker_returns_none() {
        // `S03SP01` — SP is Special. RE_SXEX requires a contiguous
        // `SxxExx` shape so `S03SP01` falls through every pattern
        // and returns `None`. Specials have ambiguous episode
        // ordering and should NOT be silently mapped to a numbered
        // slot; None is the safe outcome.
        assert_eq!(parse("[Judas] CHIHAYAFURU - S03SP01.mkv"), None);
    }

    // ── Underscore-delimited filenames (normalization) ───────────────

    #[test]
    fn underscore_delimited_dash_ep_normalizes_to_space() {
        // HGS-Renc uses `_` where newer groups use ` `. The
        // underscore normalization converts `_-_02_-_` to
        // ` - 02 - ` so RE_DASH_EP matches via the existing path.
        assert_eq!(
            parse("[HGS-Renc]_Crusher_Joe_-_02_-_The_Ice_Prison_[BD1080][HEVC].mkv"),
            Some((None, 2))
        );
    }

    #[test]
    fn dot_delimited_bare_episode_number_from_complete_series_pack() {
        // Regression: the 153-file [SoM] Dragon Ball pack uses a bare
        // dot-delimited episode token. Returning None made post-processing
        // fall back to the grab's first episode for every file, repeatedly
        // replacing S01E01 and destroying the batch during move mode.
        assert_eq!(
            parse("[SoM] Dragon.Ball.001.V2.DVD.480p.AC3.x264.mkv"),
            Some((None, 1))
        );
        assert_eq!(
            parse("[SoM] Dragon.Ball.023.V2.DVD.480p.AC3.x264.mkv"),
            Some((None, 23))
        );
        assert_eq!(
            parse("[SoM] Dragon.Ball.153.DVD.480p.AC3.x264.mkv"),
            Some((None, 153))
        );
    }

    #[test]
    fn dot_delimited_parser_ignores_audio_year_and_resolution_tokens() {
        assert_eq!(parse("Show.1996.1080p.DDP5.1.x265.mkv"), None);
        assert_eq!(
            parse("Show.S01E07.1080p.DDP5.1.x265.mkv"),
            Some((Some(1), 7))
        );
    }

    #[test]
    fn dot_delimited_parser_uses_episode_after_numeric_title_token() {
        assert_eq!(
            parse("86.Eighty.Six.023.1080p.WEB-DL.DDP2.0.x264.mkv"),
            Some((None, 23))
        );
        assert_eq!(
            parse("86.Eighty.Six.023v2.1080p.WEB-DL.mkv"),
            Some((None, 23))
        );
    }

    #[test]
    fn dot_delimited_parser_skips_dotted_codec_tokens() {
        // `H.264` / `x.265` / `MPEG.2` split into a bare digit token that
        // the rightmost scan would otherwise return as the episode.
        assert_eq!(
            parse("Dragon.Ball.023.DVD.480p.H.264.mkv"),
            Some((None, 23))
        );
        assert_eq!(parse("Show.023.1080p.H.265.mkv"), Some((None, 23)));
        assert_eq!(parse("Show.023.1080p.x.264.mkv"), Some((None, 23)));
        // Mixed space/dot delimiters keep the guard: the codec marker is
        // the last word of the preceding token, not the whole token. With
        // `264` guarded, the bare-number-before-bracket fallback recovers
        // the real episode.
        assert_eq!(
            parse("[Group] Title 023 (1080p) H.264.mkv"),
            Some((None, 23))
        );
        // A codec-digit token with no other candidate yields no episode
        // rather than 264.
        assert_eq!(parse("Movie.Name.1080p.H.264.mkv"), None);
    }

    #[test]
    fn sxxeyy_outranks_dot_delimited_codec_names() {
        // Real WEB-DL scene names carrying dotted `AAC2.0.H.264` tokens;
        // the explicit season marker must keep winning over any bare
        // dot-delimited number.
        assert_eq!(
            parse(
                "Bleach.S17E45.Thousand-Year.Blood.War.DEFEND.YOU.1080p.DSNP.WEB-DL.AAC2.0.H.264-NTb.mkv"
            ),
            Some((Some(17), 45))
        );
        assert_eq!(
            parse(
                "BLEACH.Thousand-Year.Blood.War.S01E45.DEFEND.YOU.1080p.CR.WEB-DL.JPN.AAC2.0.H.264.MSubs-ToonsHub.mkv"
            ),
            Some((Some(1), 45))
        );
    }

    // ── High School DxD S00E18 (RE_SXEX existing path) ───────────────

    #[test]
    fn s00e18_with_double_subtitle_dashes() {
        // `<title> - S00E18 - <subtitle> (<quality>) [<tag>] [<tag>]`
        // — multiple ` - ` delimiters around SxxExx. Regression
        // guard that RE_SXEX wins before any dash-delimited branch
        // fires on the subtitle segment.
        assert_eq!(
            parse(
                "High School DxD - S00E18 - Holiness Behind the Gym (BD 1080p H.264 FLAC) [Dual Audio] [IK].mkv"
            ),
            Some((Some(0), 18))
        );
    }

    // ── parse_quality branch ordering (issue #48) ────────────────────
    //
    // The unified-WEB branch in `parse_quality` uses `.contains("web")`
    // which would swallow a WEBRip filename if evaluated first. These
    // tests lock in the WebRip-before-unified-Web ordering — any
    // reshuffle that lets a WebRip filename resolve as plain "WEB"
    // would silently downgrade the label and potentially mislabel the
    // release for scoring/upgrade decisions.

    #[test]
    fn parse_quality_webrip_wins_over_bare_web_branch() {
        use super::parse_quality;
        assert_eq!(
            parse_quality("show.s01e01.web-rip.1080p.group.mkv"),
            "WEBRip-1080p"
        );
        assert_eq!(
            parse_quality("show.s01e01.webrip.1080p.group.mkv"),
            "WEBRip-1080p"
        );
    }

    #[test]
    fn parse_quality_webdl_and_bare_web_both_render_as_web() {
        use super::parse_quality;
        // Issue #48 unified WebDl and bare-WEB to the same "WEB" label.
        assert_eq!(
            parse_quality("show.s01e01.web-dl.1080p.group.mkv"),
            "WEB-1080p"
        );
        assert_eq!(
            parse_quality("show.s01e01.webdl.1080p.group.mkv"),
            "WEB-1080p"
        );
        // No WEB token in the filename → source stays blank.
        assert_eq!(
            parse_quality("[subsplease] show - 01 (1080p) [abcd].mkv"),
            "1080p"
        );
        // A title that has `WEB` as its only source token resolves to
        // the unified WEB label.
        assert_eq!(
            parse_quality("show.s01e01.web.1080p.group.mkv"),
            "WEB-1080p"
        );
    }

    // ── Non-episodic extras (issue #203) ─────────────────────────────

    #[test]
    fn pv_numbered_after_dash_is_not_an_episode() {
        // Corpus: Moozzi2's Runway de Waratte pack ships `Character PV
        // 1-4` and `PV 1-2` next to E01-E04; they used to land in the
        // episode slots and fail the whole batch closed.
        assert_eq!(
            parse("[npz-Moozzi2] Runway De Waratte - Character PV 1 (US BD, 1080p) [E392C1D3].mkv"),
            None
        );
        assert_eq!(
            parse("[npz-Moozzi2] Runway De Waratte - PV 2 (US BD, 1080p) [ED14DDFE].mkv"),
            None
        );
        // The real episodes of the same pack keep parsing.
        assert_eq!(
            parse("[npz-Moozzi2] Runway De Waratte - 01 (US BD, sub-only, 1080p) [C30B6E41].mkv"),
            Some((None, 1))
        );
    }

    #[test]
    fn creditless_opening_ending_and_preview_are_not_episodes() {
        assert_eq!(parse("[Anime Time] Creditless Opening 1.mkv"), None);
        assert_eq!(
            parse("[Group] Show - Creditless Ending 2 [1080p].mkv"),
            None
        );
        assert_eq!(parse("[Group] Show - Web Preview 01 [1080p].mkv"), None);
    }

    #[test]
    fn title_leading_type_keyword_does_not_gate_a_real_episode() {
        // anitomy types the leading `Ed` / `OP` as ED / OP. The keyword
        // sits before the title, so the gate must stay out of it.
        assert_eq!(parse("Ed Edd n Eddy - 01.mkv"), Some((None, 1)));
        assert_eq!(
            parse("[Group] OP Rewrite - 03 [1080p].mkv"),
            Some((None, 3))
        );
        assert_eq!(
            parse("[HGS-Renc]_Ed_Edd_n_Eddy_-_01_[BD1080].mkv"),
            Some((None, 1))
        );
        // A dash-less PV still gates: the keyword is past the title's
        // first word.
        assert_eq!(parse("[Group] Show PV 1 [1080p].mkv"), None);
    }

    #[test]
    fn subtitle_words_that_look_like_type_keywords_do_not_gate() {
        assert_eq!(
            parse("[Group] Show - 05 - Opening Night [1080p].mkv"),
            Some((None, 5))
        );
        assert_eq!(
            parse("[Group] Show - 12 - The ED of the World [1080p].mkv"),
            Some((None, 12))
        );
        assert_eq!(
            parse("[Group] Show - 05 - Preview of Coming Attractions [1080p].mkv"),
            Some((None, 5))
        );
        assert_eq!(
            parse("[Group] Kaguya-sama - 03 - Opening Ceremony [1080p].mkv"),
            Some((None, 3))
        );
    }

    #[test]
    fn wanted_special_types_are_left_to_the_collision_check() {
        // OAD / Special carry wanted content; the gate must not eat them.
        assert_eq!(
            parse("[Anime Time] Attack on Titan OAD - 01.mkv"),
            Some((None, 1))
        );
        assert_eq!(
            parse("[Group] Show - 12 (Special) [1080p].mkv"),
            Some((None, 12))
        );
    }

    #[test]
    fn bare_sp_marker_in_episode_position_suppresses_part_number() {
        // Corpus: Erai-raws' `Part 2 - SP` used to fall back to the
        // `Part 2` number and collide with E02.
        assert_eq!(
            parse("[Erai-raws] 86 Eighty-Six Part 2 - SP [480p][Multiple Subtitle][FDBE49E5].mkv"),
            None
        );
        assert_eq!(parse("[Group] Show Season 2 - OAD [1080p].mkv"), None);
        // A numbered sibling of the same pack still resolves.
        assert_eq!(
            parse("[Erai-raws] 86 Eighty-Six Part 2 - 02 [480p][Multiple Subtitle][2E7DEB0E].mkv"),
            Some((None, 2))
        );
    }

    #[test]
    fn half_episode_recap_suffix_is_not_an_episode() {
        assert_eq!(
            parse(
                "[Erai-raws] 86 Eighty-Six Part 2 - 07_5 [480p][Multiple Subtitle][3CC7C577].mkv"
            ),
            None
        );
        assert_eq!(parse("[Group] Show - 10.5 [1080p].mkv"), None);
        assert_eq!(parse("Show.S01E07.5.1080p.mkv"), None);
        assert_eq!(parse("[HGS-Renc]_Show_-_07_5_[BD1080].mkv"), None);
    }

    #[test]
    fn half_episode_guard_leaves_decimal_titles_and_audio_tokens_alone() {
        assert_eq!(
            parse("[SubsPlease] 2.5 Jigen no Ririsa - 03 (1080p) [ABCD1234].mkv"),
            Some((None, 3))
        );
        assert_eq!(
            parse("[SubsPlease] 2.5-jigen no Ririsa - 03 (1080p) [ABCD1234].mkv"),
            Some((None, 3))
        );
        // `5.1` audio after an SxxExx tag is not a half-episode.
        assert_eq!(parse("Show.S01E07.DDP5.1.1080p.mkv"), Some((Some(1), 7)));
    }

    // ── Release version (issue #204) ─────────────────────────────────

    #[test]
    fn release_version_shapes() {
        use super::parse_release_version as v;
        assert_eq!(v("show - 05v2 [1080p].mkv"), Some(2));
        assert_eq!(v("show - 05 (v2) [1080p].mkv"), Some(2));
        assert_eq!(v("show - 05 [v3].mkv"), Some(3));
        assert_eq!(v("[som] dragon.ball.001.v2.dvd.480p.mkv"), Some(2));
        assert_eq!(v("show.s02e10v2.1080p.mkv"), Some(2));
        assert_eq!(
            v("[group] hunter x hunter (2011) - 148v2 [1080p].mkv"),
            Some(2)
        );
        assert_eq!(v("show - 05 [1080p].mkv"), None);
        // Codec / audio tokens never read as a version.
        assert_eq!(
            v("[anime land] kimetsu no yaiba movie (bdrip 1080p av1 eac3).mkv"),
            None
        );
        assert_eq!(v("show - 05 [1080p][hevc].mkv"), None);
        assert_eq!(v("show - 05 [1080p][ddp5.1].mkv"), None);
    }

    #[test]
    fn release_revision_reads_versions_and_scene_words() {
        use super::{ReleaseRevision, parse_release_revision as r};
        let plain = ReleaseRevision {
            version: 1,
            repack: false,
        };
        assert_eq!(r("[SubsPlease] Show - 05 (1080p) [ABCD1234].mkv"), plain);
        assert_eq!(
            r("[SubsPlease] Show - 05v2 (1080p) [ABCD1234].mkv"),
            ReleaseRevision {
                version: 2,
                repack: false
            }
        );
        assert_eq!(r("Show.S01E05.PROPER.1080p.WEB.H264-GRP").version, 2);
        assert_eq!(
            r("Show.S01E05.REPACK.1080p.WEB.H264-GRP"),
            ReleaseRevision {
                version: 2,
                repack: true
            }
        );
        assert!(r("Show.S01E05.RERIP.1080p.WEB.H264-GRP").repack);
        // An explicit number wins over the scene word.
        assert_eq!(r("Show.S01E05.REPACK.v3.1080p.WEB-GRP").version, 3);
        // Title Case words are episode titles, not revisions: the
        // library file `Show - S01E05 - A Proper Goodbye.mkv` is v1.
        assert_eq!(r("Show - S01E05 - A Proper Goodbye [WEB 1080p].mkv"), plain);
        assert_eq!(r("Show - S01E05 - The Repack Job.mkv"), plain);
        // Ordering is by version only.
        assert!(
            r("Show - 05v3.mkv").is_newer_than(r("Show.S01E05.REPACK.mkv")),
            "v3 beats a REPACK (v2)"
        );
        assert!(!r("Show - 05.mkv").is_newer_than(plain));
    }

    // ── Specials (Sonarr's Season 0) ──────────────────────────────────

    fn special_of(name: &str) -> Option<bool> {
        super::parse_episode_span(&name.to_lowercase()).map(|s| s.special)
    }

    #[test]
    fn special_marker_shapes() {
        // Marker then number.
        assert_eq!(
            special_of("[Group] Show - OVA 01 (BD 1080p).mkv"),
            Some(true)
        );
        assert_eq!(special_of("[Group] Show - SP01 (1080p).mkv"), Some(true));
        assert_eq!(
            special_of("[Group] Show - Special 2 [1080p].mkv"),
            Some(true)
        );
        assert_eq!(special_of("[Group] Show - OAD 01.mkv"), Some(true));
        assert_eq!(
            special_of("[Group] Show - OVA 01-02 (BD 1080p).mkv"),
            Some(true)
        );
        // Number then marker (Sonarr's shape).
        assert_eq!(special_of("[Group] Show - 01 OVA [1080p].mkv"), Some(true));
        assert_eq!(
            special_of("[Group] Show - 05 Special (BD 1080p).mkv"),
            Some(true)
        );
        // Our own Specials-folder naming.
        assert_eq!(special_of("Show - S00E01 - [BD 1080p].mkv"), Some(true));
        // Ordinary episodes stay ordinary.
        assert_eq!(special_of("[Group] Show - 01 (BD 1080p).mkv"), Some(false));
        assert_eq!(special_of("Show - S01E05 - Title.mkv"), Some(false));
        // An episode title that starts with the word is not a marker,
        // even with a number after it.
        assert_eq!(
            special_of("Show - S01E05 - Special Training Arc [WEB 1080p].mkv"),
            Some(false)
        );
        assert_eq!(
            special_of("[Group] Show - 05 - Special Delivery.mkv"),
            Some(false)
        );
        assert_eq!(
            special_of("Show - S01E05 - Special 2 [1080p].mkv"),
            Some(false)
        );
        // A bracketed `SP` is a subtitle-track tag, not a special.
        assert_eq!(
            special_of("[Erai-raws] Show - 01 [1080p][SP].mkv"),
            Some(false)
        );
        assert_eq!(
            special_of("[Group] Show - 07 (1080p) (SP).mkv"),
            Some(false)
        );
        // A series whose title starts with the word keeps its name.
        assert_eq!(
            special_of("[Group] Special A - 01 (1080p).mkv"),
            Some(false)
        );
        assert_eq!(special_of("[Group] Special A - S01E01.mkv"), Some(false));
        // Underscores read as spaces.
        assert_eq!(special_of("[Group]_Show_-_OVA_01_[1080p].mkv"), Some(true));
    }

    #[test]
    fn special_release_titles_without_a_number_are_specials_too() {
        use super::is_special_release;
        assert!(is_special_release("[Group] Show - OVA (BD 1080p)"));
        assert!(is_special_release("[Group] Show [OVA] [BD 1080p]"));
        assert!(is_special_release("[Group] Show - SP (1080p)"));
        assert!(is_special_release("[Group] Show - OVA 01 (1080p)"));
        assert!(!is_special_release("[Group] Show - 01 (1080p)"));
        assert!(!is_special_release("[Group] Show - 01 [1080p][SP]"));
        assert!(!is_special_release("[Group] Special A - 01 (1080p)"));
        assert!(!is_special_release("[Group] Show Season 2 Batch (1080p)"));
    }

    #[test]
    fn tv_formats() {
        use super::is_tv_format;
        assert!(is_tv_format("TV"));
        assert!(is_tv_format("tv_short"));
        for f in ["OVA", "ONA", "SPECIAL", "MOVIE", "MUSIC", ""] {
            assert!(!is_tv_format(f), "{f}");
        }
    }

    #[test]
    fn scanner_keeps_specials_apart_by_place_not_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let series = dir.path().join("Show");
        std::fs::create_dir_all(series.join("Season 01")).unwrap();
        std::fs::create_dir_all(series.join("Specials")).unwrap();
        for rel in [
            "Season 01/Show - S01E01 - A.mkv",
            "Season 01/Show - S01E02 - B.mkv",
            // Season 0 and the Specials folder are specials ...
            "Specials/Show - S00E01 - [BD 1080p].mkv",
            "Specials/Show - 03.mkv",
            // ... a name marker on its own is not: an OVA-format series
            // is named this way and its files are its episodes.
            "Show - OVA 04.mkv",
        ] {
            std::fs::write(series.join(rel), b"x").unwrap();
        }
        let root = dir.path().to_str().unwrap();
        let (episodes, specials) = super::scan_series_folder_split(root, "Show");
        let mut eps: Vec<i32> = episodes.iter().map(|f| f.episode_number).collect();
        eps.sort_unstable();
        assert_eq!(eps, vec![1, 2, 4]);
        assert!(episodes.iter().all(|f| !f.is_special));
        assert_eq!(specials.len(), 2);
        assert!(specials.iter().all(|f| f.is_special));
        assert!(super::path_names_specials(std::path::Path::new(
            "pack/Specials/Show - 01.mkv"
        )));
        assert!(super::path_names_specials(std::path::Path::new(
            "SPECIALS/x.mkv"
        )));
        assert!(!super::path_names_specials(std::path::Path::new(
            "Specials.mkv"
        )));
        assert!(!super::path_names_specials(std::path::Path::new(
            "Season 01/Show - S01E01.mkv"
        )));
    }

    // ── Multi-episode spans (issue #246) ─────────────────────────────

    fn span(name: &str) -> Option<(Option<i32>, i32, i32)> {
        super::parse_episode_span(&name.to_lowercase()).map(|s| (s.season, s.first, s.last))
    }

    #[test]
    fn span_sxxexx_tails() {
        assert_eq!(span("Title - S01E05-E06.mkv"), Some((Some(1), 5, 6)));
        assert_eq!(span("Title - S01E05E06.mkv"), Some((Some(1), 5, 6)));
        assert_eq!(span("Title - S01E05-06.mkv"), Some((Some(1), 5, 6)));
        assert_eq!(
            span("Title.S01E10-E12.1080p.BluRay.x265-smol.mkv"),
            Some((Some(1), 10, 12))
        );
        assert_eq!(
            span("Title - S01E05-E06v2 [1080p].mkv"),
            Some((Some(1), 5, 6))
        );
        // Sonarr's extend / duplicate styles: the last number ends the span.
        assert_eq!(span("Title - S01E01-02-03.mkv"), Some((Some(1), 1, 3)));
        assert_eq!(span("Title - S01E01E02E03.mkv"), Some((Some(1), 1, 3)));
        assert_eq!(
            span("Title - S01E01-E02-E03 - Title.mkv"),
            Some((Some(1), 1, 3))
        );
        assert_eq!(
            span("[Group] Title - 01-02-03 [1080p].mkv"),
            Some((None, 1, 3))
        );
        assert_eq!(span("Title E01-E02-E03.mkv"), Some((None, 1, 3)));
        // A run that breaks the shape rules is still no range.
        assert_eq!(span("Title - S01E01-02-1080.mkv"), Some((Some(1), 1, 1)));
        assert_eq!(span("[Group] Title - 01-02-2019 [1080p].mkv"), None);
    }

    #[test]
    fn an_extras_word_in_an_episode_title_keeps_the_episode() {
        // Ryokan's own default template puts the episode title last.
        assert_eq!(
            span("Show - S01E05 - Happy Ending.mkv"),
            Some((Some(1), 5, 5))
        );
        assert_eq!(
            span("Show - S01E05 - The Secret Menu.mkv"),
            Some((Some(1), 5, 5))
        );
        assert_eq!(
            span("Show - S01E03 - The Ending.mkv"),
            Some((Some(1), 3, 3))
        );
        assert_eq!(
            span("Today's Menu for the Emiya Family - S01E05 - Title.mkv"),
            Some((Some(1), 5, 5))
        );
        // A label is still an extra.
        assert_eq!(span("[Group] Show - NCOP [1080p].mkv"), None);
        assert_eq!(span("[Group] Show - 12 - Preview [1080p].mkv"), None);
        // A BD menu (the anitomy gate this replaced read it as episode 1).
        assert_eq!(span("[Group] Show Menu 01 [BD].mkv"), None);
    }

    #[test]
    fn a_decimal_in_an_episode_title_is_no_half_episode() {
        assert_eq!(
            span("Show - S01E05 - Chapter 7.5.mkv"),
            Some((Some(1), 5, 5))
        );
        assert_eq!(span("[Group] Show - 07.5 [1080p].mkv"), None);
    }

    #[test]
    fn a_one_digit_nxn_running_into_the_name_is_the_name() {
        assert_eq!(span("[Group] 3x3 Eyes - 05 [DVD].mkv"), Some((None, 5, 5)));
        assert_eq!(span("3x3 Eyes - S01E05 - Title.mkv"), Some((Some(1), 5, 5)));
        assert_eq!(
            span("Series - 2x4 - New Car Smell.mkv"),
            Some((Some(2), 4, 4))
        );
    }

    #[test]
    fn a_padded_title_number_before_the_dash_leaves_the_dash_episode() {
        assert_eq!(span("[Group] Show 02 - 05 [1080p].mkv"), Some((None, 5, 5)));
        assert_eq!(span("Cyborg 009 - 012 - Title.mkv"), Some((None, 12, 12)));
        assert_eq!(span("Cyborg 009 - 12 - Title.mkv"), Some((None, 12, 12)));
    }

    #[test]
    fn a_season_marker_or_title_special_leaves_the_residual_dash() {
        assert_eq!(
            span("[Group] Show S2 - 05 The Beginning.mkv"),
            Some((None, 5, 5))
        );
        assert_eq!(span("Show Part 2 - 05 Title.mkv"), Some((None, 5, 5)));
        assert_eq!(
            span("[Anime Time] Attack on Titan OAD - 01 The Title.mkv"),
            Some((None, 1, 1))
        );
        assert_eq!(span("Show S2 - 05 PROPER.mkv"), Some((None, 5, 5)));
        // A dotted season folder still names no episode.
        assert_eq!(span("30.Series.Season.04.HDTV.XviD-DIMENSION.mkv"), None);
    }

    #[test]
    fn the_last_bare_number_before_metadata_is_the_episode() {
        assert_eq!(span("Show 99 (extra) 12 (1080p).mkv"), Some((None, 12, 12)));
        assert_eq!(
            span("[Group] Cyborg 009 (2001) 05 [DVD].mkv"),
            Some((None, 5, 5))
        );
        assert_eq!(
            span("[Group] Show 2 (2023) - 05 The Title.mkv"),
            Some((None, 5, 5))
        );
        assert_eq!(
            span("[HnY] Beyblade X 136 - The 88th Prince v2.mkv"),
            Some((None, 136, 136))
        );
    }

    #[test]
    fn glued_and_titled_dash_shapes() {
        assert_eq!(
            span("[Group] Show Episode05 [1080p].mkv"),
            Some((None, 5, 5))
        );
        assert_eq!(span("Show - 05-06 The Title.mkv"), Some((None, 5, 6)));
        assert_eq!(span("Show - 1-5 - 07 Title.mkv"), Some((None, 7, 7)));
        let ova = super::parse_episode_span("show ova-01.mkv").expect("a special");
        assert!(ova.special && ova.first == 1);
    }

    #[test]
    fn span_dash_slot_tails() {
        // The report's release (issue #246), with and without the
        // Japanese parenthetical whose `# 5 - 6` used to supply E05.
        assert_eq!(
            span("[SakuraCircle] Saimin Seishidou - 05-06 (OVA催眠性指導 # 5 - 6).mkv"),
            Some((None, 5, 6))
        );
        assert_eq!(
            span("[SakuraCircle] Saimin Seishidou - 05-06 (DVD 720x480 h264 AAC) [ABCD1234].mkv"),
            Some((None, 5, 6))
        );
        assert_eq!(
            span("[Group] Title - 01-02 [1080p].mkv"),
            Some((None, 1, 2))
        );
        assert_eq!(
            span("[Group] Title - 10-12 [1080p].mkv"),
            Some((None, 10, 12))
        );
        assert_eq!(
            span("[Group] Title - 05-06v2 [1080p].mkv"),
            Some((None, 5, 6))
        );
        assert_eq!(
            span("[Group] Title - 05~06 [1080p].mkv"),
            Some((None, 5, 6))
        );
        assert_eq!(
            span("[Group] Title_-_05-06_[1080p].mkv"),
            Some((None, 5, 6))
        );
    }

    #[test]
    fn span_e_prefix_episode_word_and_ova_tails() {
        assert_eq!(span("Title E05-E06 [1080p].mkv"), Some((None, 5, 6)));
        assert_eq!(span("Title Ep05-06 [1080p].mkv"), Some((None, 5, 6)));
        assert_eq!(span("Title Episode 05-06.mkv"), Some((None, 5, 6)));
        // Unpadded single digits are not a range; the Episode branch has
        // no follow-set, so the first number stands.
        assert_eq!(span("Title Episode 5-6.mkv"), Some((None, 5, 5)));
        assert_eq!(span("Title - OVA 01-02.mkv"), Some((None, 1, 2)));
    }

    #[test]
    fn span_single_files_are_unchanged() {
        assert_eq!(
            span("[SubsPlease] Title - 05 (1080p) [ABCD1234].mkv"),
            Some((None, 5, 5))
        );
        assert_eq!(span("Title.S01E06.1080p.mkv"), Some((Some(1), 6, 6)));
        // The scalar parser reports the first episode of a span.
        assert_eq!(parse("Title - S01E05-E06.mkv"), Some((Some(1), 5)));
        assert_eq!(parse("[Group] Title - 05-06 [1080p].mkv"), Some((None, 5)));
    }

    #[test]
    fn span_rejects_spaced_and_unpadded_pairs() {
        // Whitespace around the dash is never a range: the dash slot
        // reads its own number, as it did before spans existed.
        assert_eq!(span("Title 5 - 6.mkv"), Some((None, 6, 6)));
        assert_eq!(
            span("[Group] Title - 05 - 06 [1080p].mkv"),
            Some((None, 5, 5))
        );
        // A single-digit number on either side is not a range; SxxExx
        // has no follow-set, so the branch still fires with the first.
        assert_eq!(span("Title - S01E5-10.mkv"), Some((Some(1), 5, 5)));
        assert_eq!(span("Title - S01E5-6.mkv"), Some((Some(1), 5, 5)));
        // In the dash slot the same pair leaves the branch as it was:
        // nothing else in the name parses.
        assert_eq!(span("[Group] Title - 5-10 [1080p].mkv"), None);
        // Reversed pairs are not ranges.
        assert_eq!(span("[Group] Title - 06-05 [1080p].mkv"), None);
        assert_eq!(span("Title - S01E06-E05.mkv"), Some((Some(1), 6, 6)));
    }

    #[test]
    fn span_rejects_non_episode_digit_pairs() {
        // Resolution glued to the tail.
        assert_eq!(span("Title - S01E05-1080p.mkv"), Some((Some(1), 5, 5)));
        // A pack range in parentheses beside a real episode marker.
        assert_eq!(
            span("[Group] Title (01-12) [BD] - 05.mkv"),
            Some((None, 5, 5))
        );
        // A digit pair inside the episode title.
        assert_eq!(
            span("[Group] Title - 03 - 1-2 Punch [1080p].mkv"),
            Some((None, 3, 3))
        );
        // Longer than `MAX_FILE_SPAN` is not a file span.
        assert_eq!(span("[Group] Title - 01-24 [BD 1080p].mkv"), None);
        assert_eq!(span("[Group] Title - 01-12 [BD 1080p].mkv"), None);
        assert_eq!(
            span("[Group] Title - 01-06 [BD 1080p].mkv"),
            Some((None, 1, 6))
        );
        assert_eq!(span("Title - S01E01-E24.mkv"), Some((Some(1), 1, 1)));
        assert_eq!(span("Title - S01E01-E12.mkv"), Some((Some(1), 1, 1)));
        // The half-episode guard still wins.
        assert_eq!(span("Title - 07.5 [1080p].mkv"), None);
        // Years beside the episode.
        assert_eq!(
            span("Title - 05 (2019-2020) [1080p].mkv"),
            Some((None, 5, 5))
        );
    }

    #[test]
    fn span_rescans_after_a_rejected_tail() {
        // A digit pair in the first dash slot that fails the shape rules
        // must not hide the real episode marker after it: the scan
        // restarts inside the rejected number, not after the match.
        assert_eq!(
            span("[Group] Show - 1-2 Punch - 05 [1080p].mkv"),
            Some((None, 5, 5))
        );
        assert_eq!(
            span("Show - 2019-2020 - 05 [1080p].mkv"),
            Some((None, 5, 5))
        );
        assert_eq!(
            span("[Group] Show - 1-5 - 07 [1080p].mkv"),
            Some((None, 7, 7))
        );
        assert_eq!(span("Show - 2024-01-15 - 05.mkv"), Some((None, 5, 5)));
        assert_eq!(span("Show - 5-10 - 07 - Title.mkv"), Some((None, 7, 7)));
        assert_eq!(span("Show E1-2 - 07.mkv"), Some((None, 7, 7)));
        // A year pair beside a real range leaves the range alone.
        assert_eq!(
            span("Title - 05-06 (2019-2020) [1080p].mkv"),
            Some((None, 5, 6))
        );
    }

    #[test]
    fn span_helpers() {
        let s = super::EpisodeSpan {
            season: Some(1),
            first: 5,
            last: 6,
            special: false,
        };
        assert!(s.is_multi());
        assert_eq!(s.count(), 2);
        assert_eq!(s.episodes().collect::<Vec<_>>(), vec![5, 6]);
        assert_eq!(s.label(), "E05-E06");
        assert_eq!(super::EpisodeSpan::single(None, 7).label(), "E07");
    }
}
