use anitomy::{Anitomy, ElementCategory};
use regex_lite::Regex;
use serde::Serialize;
use std::path::Path;
use std::sync::LazyLock;

// ── Pre-compiled regexes for episode-number parsing ───────────────────────
// Lifted to LazyLock statics — `parse_episode_number` runs once per video
// file during every library scan, RSS sync, auto-search, and upgrade
// sweep. A ~200-series library with ~12 eps each running under a 1-minute
// RSS loop was paying four Regex::new compiles per call and blowing
// through the CPU budget of the hot path for no reason.

// Multi-episode tails (issue #246). Each anchored branch below carries an
// optional second number glued to the first with `-` or `~` and no
// whitespace: `S01E05-E06` / `S01E05E06` / `S01E05-06`, ` - 05-06 `,
// `E05-E06`, `Episode 5-6`, `OVA 01-02`. The tail lives inside the branch
// regex so it can only ever match in the slot that branch already trusts
// for a single episode; there is no free-floating range regex here (the
// release-side `auto_search::release_parse::RE_RANGE` is that, and it
// only feeds candidate matching, never attribution). `accept_span_tail`
// applies the shape rules. A rejected tail leaves the branch exactly
// where it was before tails existed: the follow-set branches skip the
// position, the two without a follow-set fall back to the first number.
static RE_SXEX: LazyLock<Regex> = LazyLock::new(|| {
    // The tail must be followed by a non-alphanumeric or the end so
    // `S01E05-1080p` stays a single episode.
    Regex::new(r"s(\d{1,2})e(\d{1,4})(?:(?:-e|-|e)(\d{1,4})(?:v\d)?(?:[^a-z0-9]|$))?")
        .expect("RE_SXEX compiles")
});
static RE_DASH_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r" - (\d{1,4})(?:v\d)?(?:[-~](\d{1,4})(?:v\d)?)?(?:\s|\.|\[|\(|$)")
        .expect("RE_DASH_EP compiles")
});
static RE_E_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s._\-])e(?:p\.?)?(\d{1,4})(?:v\d)?(?:[-~]e?(?:p\.?)?(\d{1,4})(?:v\d)?)?(?:\s|\.|\[|\(|$)",
    )
    .expect("RE_E_PREFIX compiles")
});
static RE_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"episode\s*(\d{1,4})(?:[-~](\d{1,4}))?").expect("RE_EPISODE compiles")
});
static RE_BARE_NUM_DASH: LazyLock<Regex> = LazyLock::new(|| {
    // Bare 1-3 digit episode number followed by ` - <subtitle>`. Used
    // by no-group-prefix releases shaped like
    // `<title> NN - <subtitle>.mkv`. The caller takes the FIRST match
    // because the actual episode marker comes BEFORE the subtitle, and
    // the subtitle itself can contain incidental ` <number> - ` runs
    // (e.g. `Show 03 - Title, Part 4 - Coda.mkv` → must resolve 03,
    // not 4). 1-3 digits keeps 4-digit years out.
    Regex::new(r"(?:^|\s)(\d{1,3})(?:v\d)?\s+-\s").expect("RE_BARE_NUM_DASH compiles")
});
static RE_BARE_NUM_BRACKET: LazyLock<Regex> = LazyLock::new(|| {
    // Bare 1-3 digit episode number followed by a quality `(...)` or
    // hash `[...]` bracket. Catches packs whose filenames are
    // space-delimited rather than `S01E01` or `- 01` — i.e. shaped
    // like `<title tokens> NN (<quality>) [<hash>].mkv`. 1-3 digits
    // keeps 4-digit years from being read as episode numbers. The
    // caller picks the rightmost match so a title token like
    // `something 99 (extra) 12 (...)` still resolves to 12, not 99.
    Regex::new(r"(?:^|\s)(\d{1,3})(?:v\d)?\s+[\(\[]").expect("RE_BARE_NUM_BRACKET compiles")
});
static RE_NCOP_NCED: LazyLock<Regex> = LazyLock::new(|| {
    // Creditless OP/ED marker that must NOT be a substring of an
    // unrelated word. Plain `contains("nced")` trips on any filename
    // containing "synced", "convinced", "announced", "experienced",
    // "bounced", etc. — we anchor the left edge to start-of-string or
    // a non-letter so only real `NCED`/`NCOP` tokens match. The right
    // edge is left open so glued-digit forms like `NCED01a` / `NCOP01`
    // (LOGH, some older BDMV rips) still resolve.
    Regex::new(r"(?:^|[^a-z])(?:ncop|nced)").expect("RE_NCOP_NCED compiles")
});
static RE_OVA_EP: LazyLock<Regex> = LazyLock::new(|| {
    // Explicit `OVA NN` / `OVA01` episode marker — used by
    // multi-episode OVA releases (e.g. the JoJo 1993 / 2000 6-episode
    // OVA sets shaped `<title> - OVA 01.mkv`). Must fire BEFORE the
    // bare-number branches so `- OVA 01.` resolves to 1 instead of
    // being shadowed by an earlier title-side digit. Must NOT match
    // bare `- OVA.` with no trailing digit — single-OVA AL entries
    // (e.g. Nichijou no 0) legitimately have no episode number and
    // should fall through to `None` rather than be invented.
    Regex::new(r"(?:^|[\s._\-])ova\s*(\d{1,3})(?:v\d)?(?:[-~](\d{1,3}))?(?:\s|\.|\[|\(|$)")
        .expect("RE_OVA_EP compiles")
});
static RE_HALF_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    // Half-episode / recap marker glued to the episode token in the
    // dash position (`- 07_5`, `- 07.5`, Erai-raws' recap convention)
    // or to an SxxExx tag (`S01E07.5`). Issue #203: these are
    // non-episodic under the Season-1 model and must not land in the
    // E07 slot. Matched on the raw (pre-underscore-normalization)
    // string because the `_5` form is exactly what normalization
    // would turn into a plain ` - 07 5 ` that RE_DASH_EP accepts.
    // Anchored on the dash / SxxExx position so a title like
    // `2.5 Jigen no Ririsa - 03` is untouched.
    Regex::new(r"(?:[\s_]-[\s_]|s\d{1,2}e)(\d{1,4})[._]5(?:[\s._\[\(]|$)")
        .expect("RE_HALF_EPISODE compiles")
});
static RE_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    // Release version: glued to the episode digits (`05v2`, `S02E10v2`,
    // `023v2`) or a standalone token (`(v2)`, `[v2]`, `.V2.`, ` v2 `).
    // The left edge is a digit or a delimiter so codec / audio tokens
    // like `AV1` never read as a version.
    Regex::new(r"(?:\d|^|[\s._\-\(\[])v(\d{1,2})(?:[\s._\-\)\]]|$)").expect("RE_VERSION compiles")
});

/// anitomy `AnimeType` values that mark a file as a non-episodic extra
/// (issue #203): promotional videos, creditless openings / endings,
/// previews. Deliberately NOT `OAD` / `OVA` / `Special` / `SP` /
/// `Movie`: those are frequently wanted content and, with no special
/// slot in the Season-1 model yet, stay with the collision check.
/// anitomy has no `CM` / `Menu` / `Trailer` keywords today; they are
/// listed so the gate picks them up if a later anitomy adds them.
const NON_EPISODIC_ANIME_TYPES: &[&str] = &[
    "pv", "cm", "ncop", "nced", "op", "ed", "opening", "ending", "preview", "menu", "trailer",
];

/// Non-episodic subtitle markers that can sit where a real subtitle
/// would in a `<title> NN - <subtitle>` filename. When the captured
/// `NN` is followed by one of these markers, it's almost certainly a
/// season/title number (e.g. `Chihayafuru 2 - OVA - Waga Miyo…`),
/// not an episode, and the parser must bail rather than import the
/// file under a mis-numbered slot.
///
/// `sp` / `oad` (issue #203): `86 Eighty-Six Part 2 - SP` otherwise
/// falls back to the `Part 2` number and lands the special in the E02
/// slot. The word-boundary check keeps `sp` from matching `special`'s
/// prefix (that one has its own entry) or a glued `sp1`.
fn starts_with_non_episode_marker(rest: &str) -> bool {
    let trimmed = rest.trim_start();
    const MARKERS: &[&str] = &["ova", "oad", "sp", "special", "specials"];
    for tok in MARKERS {
        if let Some(after) = trimmed.strip_prefix(tok) {
            // Word boundary: next char must not be another letter or
            // digit (so `special` matches but `spectral` doesn't).
            let bounded = match after.as_bytes().first() {
                None => true,
                Some(c) => !c.is_ascii_alphanumeric(),
            };
            if bounded {
                return true;
            }
        }
    }
    false
}

/// Parse the rightmost bare two- or three-digit dot-delimited token.
///
/// Rightmost is deliberate: numeric title tokens such as `86.Eighty.Six`
/// precede the episode token (`86.Eighty.Six.023...`). Four-digit years and
/// resolutions, one-digit audio channel tokens, and alphanumeric codec tokens
/// remain ineligible. A glued revision suffix (`023v2`) is accepted.
///
/// Dotted codec spellings (`H.264`, `x.265`, `MPEG.2`) put a bare digit
/// token immediately right of a codec marker; without the marker guard the
/// rightmost scan would return 264/265 as the episode number. The marker
/// check looks at the last word of the preceding token so mixed-delimiter
/// names (`Title 023 1080p H.264.mkv`, `[BD 1080p x.264]`) are covered too.
fn parse_dot_delimited_episode(lower: &str) -> Option<i32> {
    let tokens: Vec<&str> = lower.split('.').collect();
    tokens.iter().enumerate().rev().find_map(|(i, token)| {
        let digits = token
            .split_once('v')
            .filter(|(_, version)| {
                !version.is_empty() && version.chars().all(|c| c.is_ascii_digit())
            })
            .map(|(digits, _)| digits)
            .unwrap_or(token);
        if !((2..=3).contains(&digits.len()) && digits.chars().all(|c| c.is_ascii_digit())) {
            return None;
        }
        let follows_codec_marker = i > 0
            && tokens[i - 1]
                .rsplit([' ', '-', '_', '(', '['])
                .next()
                .is_some_and(|word| matches!(word, "h" | "x" | "mpeg"));
        if follows_codec_marker {
            return None;
        }
        digits.parse().ok()
    })
}

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
        .trim_matches('.')
        .trim()
        .to_string()
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
}

impl EpisodeFile {
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
    let series_path = Path::new(media_root).join(folder_name);
    if !series_path.is_dir() {
        return Vec::new();
    }

    let mut files = Vec::new();
    scan_dir_recursive(&series_path, &series_path, &mut files);

    // Sort by season then episode number.
    files.sort_by(|a, b| {
        a.season_number
            .unwrap_or(0)
            .cmp(&b.season_number.unwrap_or(0))
            .then(a.episode_number.cmp(&b.episode_number))
    });

    files
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

fn scan_dir_recursive(dir: &Path, series_root: &Path, files: &mut Vec<EpisodeFile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, series_root, files);
        } else if is_video_file(&path)
            && let Some(ep) = parse_episode_file(&path, series_root)
        {
            files.push(ep);
        }
    }
}

fn is_video_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext.to_lowercase().as_str(),
            "mkv" | "mp4" | "avi" | "wmv" | "flv" | "webm" | "m4v" | "ts"
        ),
        None => false,
    }
}

fn parse_episode_file(path: &Path, series_root: &Path) -> Option<EpisodeFile> {
    let basename = path.file_name()?.to_str()?.to_string();
    let lower = basename.to_lowercase();

    let span = parse_episode_span(&lower)?;

    let quality = parse_quality(&lower);

    let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let size_display = format_size(size_bytes);

    // Store relative path from series root so delete handler can reconstruct full path.
    let filename = path
        .strip_prefix(series_root)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or(&basename)
        .to_string();

    Some(EpisodeFile {
        filename,
        episode_number: span.first,
        episode_last: span.last,
        season_number: span.season,
        quality,
        size_bytes,
        size_display,
    })
}

/// Parse episode (and optionally season) number from a filename.
/// Handles patterns like:
/// - `S01E05`, `S1E5`, `s01e05`
/// - `- 05 (v2)`, `- 05v2`, `[group] Title - 05 [1080p]`
/// - `E05`, `EP05`, `Ep.05`
/// - `Episode 05`
/// - `Title - OVA 01` (multi-episode OVA releases with explicit OVA marker)
/// - `Title 05 - Subtitle` (no group bracket, bare number before subtitle)
/// - `Title 05 (1080p) [hash]` (no group bracket, bare number before quality bracket)
/// - Underscore-delimited filenames (`Title_-_05_-_Subtitle`) are normalized
///   to space-delimited before matching.
///
/// Returns `None` for files explicitly tagged as creditless openings or
/// endings (`NCOP`/`NCED`), for files where a captured bare-number is
/// followed by a non-episodic marker like `OVA`/`Special`, for
/// single-OVA entries with no trailing digit, for half-episode / recap
/// files (`- 07_5`), and for files anitomy types as a PV / creditless
/// OP-ED / preview (issue #203; see [`is_non_episodic_extra`]).
///
/// A multi-episode file name (`S01E05-E06`) yields its **first** episode
/// here; [`parse_episode_span`] is the full answer.
pub fn parse_episode_number(lower: &str) -> Option<(Option<i32>, i32)> {
    parse_episode_span(lower).map(|span| (span.season, span.first))
}

/// The episode slot(s) a file name claims (issue #246). `first == last`
/// for the ordinary one-episode file; a multi-episode name spans
/// `first..=last`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EpisodeSpan {
    pub season: Option<i32>,
    pub first: i32,
    pub last: i32,
}

impl EpisodeSpan {
    pub fn single(season: Option<i32>, episode: i32) -> Self {
        Self {
            season,
            first: episode,
            last: episode,
        }
    }

    /// True for a multi-episode span.
    pub fn is_multi(&self) -> bool {
        self.last > self.first
    }

    /// Number of episodes in the span (1 for a single).
    pub fn count(&self) -> i32 {
        self.last - self.first + 1
    }

    /// Every episode number in the span, first to last.
    pub fn episodes(&self) -> std::ops::RangeInclusive<i32> {
        self.first..=self.last
    }

    /// `E05` or `E05-E06`, for log lines and labels.
    pub fn label(&self) -> String {
        if self.is_multi() {
            format!("E{:02}-E{:02}", self.first, self.last)
        } else {
            format!("E{:02}", self.first)
        }
    }
}

/// Longest span a single file name may claim. One cour bounds what a
/// mis-read range can silently mark as present; a longer tail is not a
/// range (the name parses as its first episode, as before #246).
pub const MAX_FILE_SPAN: i32 = 12;

/// Shape rules for a multi-episode tail: the second number is larger,
/// the pair is zero-padded to the same width or both are at least two
/// digits (`05-06`, `10-12`, `99-100`; not `5-10`), and the span fits
/// [`MAX_FILE_SPAN`]. Anything else is not a range.
fn accept_span_tail(first: &str, last: &str) -> Option<(i32, i32)> {
    let a: i32 = first.parse().ok()?;
    let b: i32 = last.parse().ok()?;
    if b <= a {
        return None;
    }
    let same_width = first.len() == last.len();
    let both_padded = first.len() >= 2 && last.len() >= 2;
    if !(same_width || both_padded) {
        return None;
    }
    if b - a + 1 > MAX_FILE_SPAN {
        return None;
    }
    Some((a, b))
}

enum Tail {
    /// No tail: the ordinary single episode.
    Single(i32),
    /// An accepted multi-episode tail.
    Range(i32, i32),
    /// A tail is present but fails the shape rules.
    Rejected,
}

/// Read a branch's first-number capture (`first_idx`) and optional
/// tail capture (`last_idx`).
fn read_tail(caps: &regex_lite::Captures<'_>, first_idx: usize, last_idx: usize) -> Option<Tail> {
    let first = caps.get(first_idx)?.as_str();
    let Some(last) = caps.get(last_idx) else {
        return Some(Tail::Single(first.parse().ok()?));
    };
    Some(match accept_span_tail(first, last.as_str()) {
        Some((a, b)) => Tail::Range(a, b),
        None => Tail::Rejected,
    })
}

/// [`parse_episode_number`] with the multi-episode tail kept. The
/// branch order and every guard are the ones documented on that
/// function; only the return shape differs.
pub fn parse_episode_span(lower: &str) -> Option<EpisodeSpan> {
    let parsed = parse_episode_span_inner(lower)?;
    // The anitomy gate runs last and only when a number was found, so
    // the (comparatively) expensive tokenizer pass is skipped for the
    // NC / bare-marker shapes the regex branches already reject.
    if is_non_episodic_extra(lower) {
        return None;
    }
    Some(parsed)
}

fn parse_episode_span_inner(lower: &str) -> Option<EpisodeSpan> {
    // Non-episodic content guard. Creditless openings/endings carry a
    // small integer suffix (`NCOP 1`, `NCED 1`, or even glued
    // `NCED01a` from LOGH-style packs) that the bare-number
    // fallback below would otherwise mis-parse as episode 1 and
    // clobber the real ep1. Bail before any pattern fires. Matched
    // with a token-boundary regex so incidental substring hits like
    // `synced`, `convinced`, `announced` don't false-positive.
    if RE_NCOP_NCED.is_match(lower) {
        return None;
    }

    // Half-episode / recap guard (issue #203). Checked on the raw
    // string, before underscore normalization would turn `07_5` into
    // a plain `07 5` that RE_DASH_EP accepts as episode 7.
    if RE_HALF_EPISODE.is_match(lower) {
        return None;
    }

    // Underscore-delimited releases (HGS-Renc, some older groups)
    // use `_` where most releases use ` `. Normalize once so a
    // single set of patterns handles both shapes without having to
    // dual-parse. Only allocates when an underscore is actually
    // present — most inputs skip the replace.
    let normalized = if lower.contains('_') {
        Some(lower.replace('_', " "))
    } else {
        None
    };
    let lower = normalized.as_deref().unwrap_or(lower);

    // SxxExx pattern — most reliable. No follow-set, so a rejected tail
    // keeps the first number the way the pre-#246 regex did.
    if let Some(caps) = RE_SXEX.captures(lower) {
        let s: i32 = caps.get(1)?.as_str().parse().ok()?;
        return Some(match read_tail(&caps, 2, 3)? {
            Tail::Range(a, b) => EpisodeSpan {
                season: Some(s),
                first: a,
                last: b,
            },
            Tail::Single(e) => EpisodeSpan::single(Some(s), e),
            Tail::Rejected => EpisodeSpan::single(Some(s), caps.get(2)?.as_str().parse().ok()?),
        });
    }

    // " - 05" pattern (common in anime releases from SubsPlease, Erai, etc).
    // A rejected tail skips the position: the follow-set never let the
    // single form match there.
    for caps in RE_DASH_EP.captures_iter(lower) {
        match read_tail(&caps, 1, 2)? {
            Tail::Single(e) => return Some(EpisodeSpan::single(None, e)),
            Tail::Range(a, b) => {
                return Some(EpisodeSpan {
                    season: None,
                    first: a,
                    last: b,
                });
            }
            Tail::Rejected => continue,
        }
    }

    // "E05" or "EP05" or "Ep.05" pattern.
    for caps in RE_E_PREFIX.captures_iter(lower) {
        match read_tail(&caps, 1, 2)? {
            Tail::Single(e) => return Some(EpisodeSpan::single(None, e)),
            Tail::Range(a, b) => {
                return Some(EpisodeSpan {
                    season: None,
                    first: a,
                    last: b,
                });
            }
            Tail::Rejected => continue,
        }
    }

    // "Episode 05" pattern. No follow-set; a rejected tail keeps the
    // first number.
    if let Some(caps) = RE_EPISODE.captures(lower) {
        return Some(match read_tail(&caps, 1, 2)? {
            Tail::Range(a, b) => EpisodeSpan {
                season: None,
                first: a,
                last: b,
            },
            Tail::Single(e) => EpisodeSpan::single(None, e),
            Tail::Rejected => EpisodeSpan::single(None, caps.get(1)?.as_str().parse().ok()?),
        });
    }

    // Explicit `OVA NN` marker — for multi-episode OVA releases
    // whose filenames have no SxxExx and no leading `- NN -`. Must
    // fire before the bare-number branches so that `Title - OVA 01.`
    // resolves to 1 even when an earlier title token (e.g. a year)
    // would otherwise shadow it. Bare `- OVA.` with no trailing
    // digit correctly falls through here because RE_OVA_EP requires
    // a captured 1-3 digit group.
    for caps in RE_OVA_EP.captures_iter(lower) {
        match read_tail(&caps, 1, 2)? {
            Tail::Single(e) => return Some(EpisodeSpan::single(None, e)),
            Tail::Range(a, b) => {
                return Some(EpisodeSpan {
                    season: None,
                    first: a,
                    last: b,
                });
            }
            Tail::Rejected => continue,
        }
    }

    // Dot-tokenized bare episode number. This covers older complete-series
    // packs whose files are shaped `Title.001...` rather than `Title - 001`.
    if let Some(e) = parse_dot_delimited_episode(lower) {
        return Some(EpisodeSpan::single(None, e));
    }

    // Bare-number-before-subtitle fallback. Shape:
    // `<title> NN - <subtitle>.mkv` — first match wins (subtitle may
    // contain its own ` <n> - ` run that should NOT shadow the real
    // episode marker).
    if let Some(caps) = RE_BARE_NUM_DASH.captures(lower)
        && let Some(m) = caps.get(1)
    {
        if let Some(full) = caps.get(0) {
            // If what follows the ` - ` is a non-episodic marker
            // (e.g. `Chihayafuru 2 - OVA - Waga Miyo…`), the
            // captured `2` is a season/title number, not an
            // episode. Bail with `None` rather than fall through
            // to further patterns — the subsequent bare-number
            // branch would re-capture the same false digit.
            let rest = &lower[full.end()..];
            if starts_with_non_episode_marker(rest) {
                return None;
            }
        }
        if let Ok(e) = m.as_str().parse::<i32>() {
            return Some(EpisodeSpan::single(None, e));
        }
    }

    // Bare-number-before-bracket fallback for space-delimited packs.
    // Use the rightmost match so an earlier title token like
    // `Show 99 (extra) 01` doesn't shadow the actual episode number.
    if let Some(caps) = RE_BARE_NUM_BRACKET.captures_iter(lower).last()
        && let Some(m) = caps.get(1)
        && let Ok(e) = m.as_str().parse::<i32>()
    {
        return Some(EpisodeSpan::single(None, e));
    }

    None
}

/// anitomy `AnimeType` gate (issue #203). True when anitomy types the
/// name as one of [`NON_EPISODIC_ANIME_TYPES`] and the keyword sits in
/// the episode position: after the first ` - ` separator, or anywhere
/// past the title's first word when the name has no separator.
///
/// anitomy contributes only the type here; its `EpisodeNumber` is never
/// consulted because its codec handling is weaker than the homegrown
/// branches (`H.265` parses as episode 265 there; see PR #198).
///
/// The position check is the one guard anitomy needs. It keeps the
/// keyword inside `AnimeTitle` in every case (`ed edd n eddy`,
/// `runway de waratte - character pv`), so a keyword that *leads* the
/// title (`Ed Edd n Eddy - 01`, `OP Rewrite - 03`) is typed as ED / OP
/// exactly like a real extra. What separates them is where the keyword
/// sits: extras carry it after the dash (`- Character PV 1`,
/// `- Creditless Ending 2`) or in a dash-less name (`Creditless Opening
/// 1`), title words sit before it. Subtitle words are safe either way:
/// anitomy reads `- 05 - Opening Night` as an episode title, not a type.
fn is_non_episodic_extra(lower: &str) -> bool {
    // anitomy rejects NUL bytes; nothing real carries them.
    if lower.contains('\0') {
        return false;
    }
    let mut ani = Anitomy::new();
    let elements = match ani.parse(lower) {
        Ok(e) => e,
        Err(e) => e,
    };
    let title_start = elements
        .get(ElementCategory::AnimeTitle)
        .and_then(|t| lower.find(&t.to_ascii_lowercase()))
        .unwrap_or(0);
    // Earliest byte offset a keyword may start at to count: right after
    // the first ` - ` / `_-_` separator, else past the title's first
    // character (so a title-leading keyword never qualifies).
    let min_at = lower
        .find(" - ")
        .or_else(|| lower.find("_-_"))
        .map(|i| i + 3)
        .unwrap_or(title_start + 1);
    elements
        .get_all(ElementCategory::AnimeType)
        .into_iter()
        .map(|t| t.to_ascii_lowercase())
        .any(|t| NON_EPISODIC_ANIME_TYPES.contains(&t.as_str()) && keyword_from(lower, &t, min_at))
}

/// True when `keyword` occurs as a whole word in `haystack` starting at
/// byte offset `min_at` or later.
fn keyword_from(haystack: &str, keyword: &str, min_at: usize) -> bool {
    let bytes = haystack.as_bytes();
    haystack.match_indices(keyword).any(|(at, _)| {
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let end = at + keyword.len();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        before_ok && after_ok && at >= min_at
    })
}

/// Release version carried by a filename (`05v2` → 2, `[v2]` → 2,
/// `S02E10v2` → 2, `.V2.` → 2). `None` when no version token is
/// present; callers treat that as v1. Issue #204 uses it to prefer the
/// highest version when a batch ships `E05` and `E05v2`.
///
/// Leftmost match, not episode-adjacent: a `vN` token in a group tag
/// or title (`[Group v2] Show - 05v3`) wins over the real one. Both
/// files in a colliding pair share that prefix in practice, so they
/// read the same wrong version, tie, and the preflight fails closed
/// rather than picking wrongly.
pub fn parse_release_version(lower: &str) -> Option<u32> {
    RE_VERSION
        .captures(lower)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
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
        assert_eq!(span("Title Episode 5-6.mkv"), Some((None, 5, 6)));
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
        // Unequal width without padding narrows to the first number
        // (SxxExx has no follow-set, so the branch still fires).
        assert_eq!(span("Title - S01E5-10.mkv"), Some((Some(1), 5, 5)));
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
        // Longer than one cour is not a file span.
        assert_eq!(span("[Group] Title - 01-24 [BD 1080p].mkv"), None);
        assert_eq!(span("Title - S01E01-E24.mkv"), Some((Some(1), 1, 1)));
        // The half-episode guard still wins.
        assert_eq!(span("Title - 07.5 [1080p].mkv"), None);
        // Years beside the episode.
        assert_eq!(
            span("Title - 05 (2019-2020) [1080p].mkv"),
            Some((None, 5, 5))
        );
    }

    #[test]
    fn span_helpers() {
        let s = super::EpisodeSpan {
            season: Some(1),
            first: 5,
            last: 6,
        };
        assert!(s.is_multi());
        assert_eq!(s.count(), 2);
        assert_eq!(s.episodes().collect::<Vec<_>>(), vec![5, 6]);
        assert_eq!(s.label(), "E05-E06");
        assert_eq!(super::EpisodeSpan::single(None, 7).label(), "E07");
    }
}
