//! File-name mode: what one file's name holds.
//!
//! Moved verbatim from `services::media` (phase 11a of the parser plan),
//! which re-exports it, so the import, the scanner and every other reader
//! of a file name keep their call sites. The branch order is load-bearing
//! (root `AGENTS.md`, "Parse-ordering in `services/media.rs`"); the next
//! steps replace branches with the release-title rules one at a time,
//! each with zero unexplained differences over every corpus.

use std::sync::LazyLock;

use anitomy::ElementCategory;
use regex_lite::Regex;
use serde::Serialize;

use crate::services::media::anitomy_parse;

// ── Pre-compiled regexes for episode-number parsing ───────────────────────
// Lifted to LazyLock statics — `parse_episode_number` runs once per video
// file during every library scan, RSS sync, auto-search, and upgrade
// sweep. A ~200-series library with ~12 eps each running under a 1-minute
// RSS loop was paying four Regex::new compiles per call and blowing
// through the CPU budget of the hot path for no reason.
//
// Multi-episode tails (issue #246). Each anchored branch below carries
// optional further numbers glued to the first with `-` or `~` and no
// whitespace: `S01E05-E06` / `S01E05E06` / `S01E05-06`, ` - 05-06 `,
// `E05-E06`, `Episode 5-6`, `OVA 01-02`. The SxxExx, dash, and E-prefix
// branches take a run of tails (Sonarr's "extend" and "duplicate"
// styles, `S01E01-02-03` / `S01E01E02E03`) and the last number is the
// end of the span. The tail lives inside the branch regex so it can only
// ever match in the slot that branch already trusts for a single
// episode; there is no free-floating range regex here (the release-side
// `auto_search::release_parse::RE_RANGE` is that, and it only feeds
// candidate matching, never attribution). `accept_span_tail` applies the
// shape rules. A rejected tail leaves the branch where it was before
// tails existed: the follow-set branches rescan from inside the rejected
// number (`scan_tails`), the two without a follow-set fall back to the
// first number.
pub(crate) static RE_SXEX: LazyLock<Regex> = LazyLock::new(|| {
    // The tail must be followed by a non-alphanumeric or the end so
    // `S01E05-1080p` stays a single episode.
    Regex::new(r"s(\d{1,2})e(\d{1,4})(?:(?:(?:-e|-|e)(\d{1,4})(?:v\d)?)+(?:[^a-z0-9]|$))?")
        .expect("RE_SXEX compiles")
});

pub(crate) static RE_DASH_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r" - (\d{1,4})(?:v\d)?(?:[-~](\d{1,4})(?:v\d)?)*(?:\s|\.|\[|\(|$)")
        .expect("RE_DASH_EP compiles")
});

pub(crate) static RE_E_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s._\-])e(?:p\.?)?(\d{1,4})(?:v\d)?(?:[-~]e?(?:p\.?)?(\d{1,4})(?:v\d)?)*(?:\s|\.|\[|\(|$)",
    )
    .expect("RE_E_PREFIX compiles")
});

pub(crate) static RE_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"episode\s*(\d{1,4})(?:[-~](\d{1,4}))?").expect("RE_EPISODE compiles")
});

pub(crate) static RE_BARE_NUM_DASH: LazyLock<Regex> = LazyLock::new(|| {
    // Bare 1-3 digit episode number followed by ` - <subtitle>`. Used
    // by no-group-prefix releases shaped like
    // `<title> NN - <subtitle>.mkv`. The caller takes the FIRST match
    // because the actual episode marker comes BEFORE the subtitle, and
    // the subtitle itself can contain incidental ` <number> - ` runs
    // (e.g. `Show 03 - Title, Part 4 - Coda.mkv` → must resolve 03,
    // not 4). 1-3 digits keeps 4-digit years out.
    Regex::new(r"(?:^|\s)(\d{1,3})(?:v\d)?\s+-\s").expect("RE_BARE_NUM_DASH compiles")
});

pub(crate) static RE_BARE_NUM_BRACKET: LazyLock<Regex> = LazyLock::new(|| {
    // Bare 1-3 digit episode number followed by a quality `(...)` or
    // hash `[...]` bracket. Catches packs whose filenames are
    // space-delimited rather than `S01E01` or `- 01` — i.e. shaped
    // like `<title tokens> NN (<quality>) [<hash>].mkv`. 1-3 digits
    // keeps 4-digit years from being read as episode numbers. The
    // caller picks the rightmost match so a title token like
    // `something 99 (extra) 12 (...)` still resolves to 12, not 99.
    Regex::new(r"(?:^|\s)(\d{1,3})(?:v\d)?\s+[\(\[]").expect("RE_BARE_NUM_BRACKET compiles")
});

pub(crate) static RE_NCOP_NCED: LazyLock<Regex> = LazyLock::new(|| {
    // Creditless OP/ED marker that must NOT be a substring of an
    // unrelated word. Plain `contains("nced")` trips on any filename
    // containing "synced", "convinced", "announced", "experienced",
    // "bounced", etc. — we anchor the left edge to start-of-string or
    // a non-letter so only real `NCED`/`NCOP` tokens match. The right
    // edge is left open so glued-digit forms like `NCED01a` / `NCOP01`
    // (LOGH, some older BDMV rips) still resolve.
    Regex::new(r"(?:^|[^a-z])(?:ncop|nced)").expect("RE_NCOP_NCED compiles")
});

pub(crate) static RE_OVA_EP: LazyLock<Regex> = LazyLock::new(|| {
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

/// `SP01` / `SP 1` / `Special 01` / `OAD 01`: the other marker-then-
/// number special shapes, same rules as `RE_OVA_EP`.
pub(crate) static RE_SPECIAL_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s._\-])(?:sp|specials?|oad)\s*(\d{1,3})(?:v\d)?(?:[-~](\d{1,3}))?(?:\s|\.|\[|\(|$)",
    )
    .expect("RE_SPECIAL_EP compiles")
});

/// Sonarr's shape: the episode number followed by the marker
/// (`- 01 OVA [1080p]`, `05 Special`). The marker has to end the
/// segment (a bracket, a ` - `, the extension, or the end) so an
/// episode title that starts with the word (`- 01 - Special Training`)
/// is not one.
pub(crate) static RE_NUM_THEN_SPECIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s._\-])\d{1,3}(?:v\d)?[\s._\-]+(?:ova|oad|sp|specials?)(?:\s*[\[\(]|\s*-\s|\.[a-z0-9]{2,4}$|$)",
    )
    .expect("RE_NUM_THEN_SPECIAL compiles")
});

/// A bare marker segment with no number (`Show - OVA (BD 1080p)`,
/// `Show - OVA.mkv`, `Show [OVA]`). Only counted past the first ` - `
/// or inside brackets, so a title that starts with the word (`Special
/// A - 01`) keeps its name. The two-letter `sp` counts only as a
/// dash segment: `[SP]` / `(SP)` is a subtitle-track tag on plenty of
/// ordinary episodes.
pub(crate) static RE_BARE_SPECIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:\s-\s(?:ova|oad|sp|specials?)|[\[\(](?:ova|oad|specials?))(?:\s*[\]\)\[\(]|\s*-\s|\.[a-z0-9]{2,4}$|$)",
    )
    .expect("RE_BARE_SPECIAL compiles")
});

pub(crate) static RE_HALF_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
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

pub(crate) static RE_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    // Release version: glued to the episode digits (`05v2`, `S02E10v2`,
    // `023v2`) or a standalone token (`(v2)`, `[v2]`, `.V2.`, ` v2 `).
    // The left edge is a digit or a delimiter so codec / audio tokens
    // like `AV1` never read as a version.
    Regex::new(r"(?:\d|^|[\s._\-\(\[])v(\d{1,2})(?:[\s._\-\)\]]|$)").expect("RE_VERSION compiles")
});

/// Scene-style revision words. Matched against the **original-case**
/// name, never the lowercased one: release titles carry `PROPER` /
/// `REPACK` / `RERIP` in capitals by convention, while a Title Case
/// `Proper` is an episode title word (`A Proper Goodbye`), which
/// Ryokan's own renamed library files contain. Sonarr matches these
/// case-insensitively and lives with the false positive; Ryokan's
/// tag rows keep the release title, so the library file name is never
/// what gets parsed, but the interactive picker and the disk fallback
/// still see file names.
pub(crate) static RE_PROPER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bPROPER\b").expect("RE_PROPER compiles"));

pub(crate) static RE_REPACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:REPACK|RERIP)\b").expect("RE_REPACK compiles"));

/// anitomy `AnimeType` values that mark a file as a non-episodic extra
/// (issue #203): promotional videos, creditless openings / endings,
/// previews. Deliberately NOT `OAD` / `OVA` / `Special` / `SP` /
/// `Movie`: those are frequently wanted content and, with no special
/// slot in the Season-1 model yet, stay with the collision check.
/// anitomy has no `CM` / `Menu` / `Trailer` keywords today; they are
/// listed so the gate picks them up if a later anitomy adds them.
pub(crate) const NON_EPISODIC_ANIME_TYPES: &[&str] = &[
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
pub(crate) fn starts_with_non_episode_marker(rest: &str) -> bool {
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
pub(crate) fn parse_dot_delimited_episode(lower: &str) -> Option<i32> {
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
    /// The name marks the file as a special: `OVA 01` / `SP1` (the
    /// number came from the marker branch), `01 Special`, a bare
    /// `- OVA` segment, or season 0. What that means depends on the
    /// series: for a TV entry the file goes to the Specials folder,
    /// for an entry that *is* the OVA / special the marker is part of
    /// its identity and the file is an ordinary episode. See
    /// [`is_tv_format`].
    pub special: bool,
}

impl EpisodeSpan {
    pub fn single(season: Option<i32>, episode: i32) -> Self {
        Self {
            season,
            first: episode,
            last: episode,
            special: false,
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

/// Longest span a single file name may claim. Real multi-episode files
/// hold two or three episodes, a few OVA collections four; six bounds
/// what a mis-read range can silently mark as present. A longer tail is
/// not a range (the name parses as its first episode, as before #246).
pub const MAX_FILE_SPAN: i32 = 6;

/// Shape rules for a multi-episode tail: the second number is larger,
/// both numbers have at least two digits (`05-06`, `10-12`, `99-100`;
/// never `1-2`, which is how a subtitle like "1-2 Punch" reads, nor
/// `5-10`), a four-digit first number is not a year, and the span fits
/// [`MAX_FILE_SPAN`]. Anything else is not a range.
pub(crate) fn accept_span_tail(first: &str, last: &str) -> Option<(i32, i32)> {
    let a: i32 = first.parse().ok()?;
    let b: i32 = last.parse().ok()?;
    if b <= a {
        return None;
    }
    if first.len() < 2 || last.len() < 2 {
        return None;
    }
    if first.len() == 4 && (1900..=2100).contains(&a) {
        return None;
    }
    if b - a + 1 > MAX_FILE_SPAN {
        return None;
    }
    Some((a, b))
}

pub(crate) enum Tail {
    /// No tail: the ordinary single episode.
    Single(i32),
    /// An accepted multi-episode tail.
    Range(i32, i32),
    /// A tail is present but fails the shape rules.
    Rejected,
}

/// Read a branch's first-number capture (`first_idx`) and optional
/// tail capture (`last_idx`).
pub(crate) fn read_tail(
    caps: &regex_lite::Captures<'_>,
    first_idx: usize,
    last_idx: usize,
) -> Option<Tail> {
    let first = caps.get(first_idx)?.as_str();
    let Some(last) = caps.get(last_idx) else {
        return Some(Tail::Single(first.parse().ok()?));
    };
    Some(match accept_span_tail(first, last.as_str()) {
        Some((a, b)) => Tail::Range(a, b),
        None => Tail::Rejected,
    })
}

/// Run a follow-set branch over `lower`. A match whose tail fails the
/// shape rules is not the episode, but the match may have swallowed the
/// separator of a later marker (` - 2019-2020 - 05`), so the scan
/// restarts one byte into the rejected number instead of after the
/// match; `captures_iter` would resume past the space the next ` - 05`
/// needs. The single form never matched at a rejected position either
/// (its follow-set refused the `-`), so the branch ends up exactly where
/// it was before tails existed.
pub(crate) fn scan_tails(
    re: &Regex,
    lower: &str,
    first_idx: usize,
    last_idx: usize,
) -> Option<EpisodeSpan> {
    let mut from = 0;
    while from < lower.len() {
        let caps = re.captures(&lower[from..])?;
        match read_tail(&caps, first_idx, last_idx)? {
            Tail::Single(e) => return Some(EpisodeSpan::single(None, e)),
            Tail::Range(a, b) => {
                return Some(EpisodeSpan {
                    season: None,
                    first: a,
                    last: b,
                    special: false,
                });
            }
            // The first number is ASCII digits, so one byte in is a
            // char boundary.
            Tail::Rejected => from += caps.get(first_idx)?.start() + 1,
        }
    }
    None
}

/// [`parse_episode_number`] with the multi-episode tail kept. The
/// branch order and every guard are the ones documented on that
/// function; only the return shape differs.
pub fn parse_episode_span(lower: &str) -> Option<EpisodeSpan> {
    let mut parsed = parse_episode_span_inner(lower)?;
    // The anitomy gate runs last and only when a number was found, so
    // the (comparatively) expensive tokenizer pass is skipped for the
    // NC / bare-marker shapes the regex branches already reject.
    if is_non_episodic_extra(lower) {
        return None;
    }
    if !parsed.special && (parsed.season == Some(0) || has_special_marker(lower)) {
        parsed.special = true;
    }
    Some(parsed)
}

/// True when the name carries a special marker in a shape that does
/// not itself supply the episode number: Sonarr's `01 OVA` / `05
/// Special`, or a bare `- OVA` segment. The marker-then-number shapes
/// (`OVA 01`, `SP1`) are read by the episode parser's own branch and
/// flag the span there, so an episode title that merely starts with
/// the word (`- S01E05 - Special 2`) is not one. Underscores are read
/// as spaces the way the episode parser does.
pub fn has_special_marker(lower: &str) -> bool {
    let normalized = if lower.contains('_') {
        Some(lower.replace('_', " "))
    } else {
        None
    };
    let lower = normalized.as_deref().unwrap_or(lower);
    RE_NUM_THEN_SPECIAL.is_match(lower) || RE_BARE_SPECIAL.is_match(lower)
}

/// True for a release title or file name that names a special of a
/// series rather than one of its episodes: the parsed span is flagged,
/// or a bare marker segment is present with no number at all. The
/// automatic paths reject these for a TV series (the special is never
/// the episode they are looking for); the interactive picker keeps
/// them, since a grab from there imports into the Specials folder.
pub fn is_special_release(title: &str) -> bool {
    let lower = title.to_lowercase();
    match parse_episode_span(&lower) {
        Some(span) => span.special,
        None => has_special_marker(&lower),
    }
}

pub(crate) fn parse_episode_span_inner(lower: &str) -> Option<EpisodeSpan> {
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
                special: false,
            },
            Tail::Single(e) => EpisodeSpan::single(Some(s), e),
            Tail::Rejected => EpisodeSpan::single(Some(s), caps.get(2)?.as_str().parse().ok()?),
        });
    }

    // " - 05" pattern (common in anime releases from SubsPlease, Erai, etc).
    if let Some(span) = scan_tails(&RE_DASH_EP, lower, 1, 2) {
        return Some(span);
    }

    // "E05" or "EP05" or "Ep.05" pattern.
    if let Some(span) = scan_tails(&RE_E_PREFIX, lower, 1, 2) {
        return Some(span);
    }

    // "Episode 05" pattern. No follow-set; a rejected tail keeps the
    // first number.
    if let Some(caps) = RE_EPISODE.captures(lower) {
        return Some(match read_tail(&caps, 1, 2)? {
            Tail::Range(a, b) => EpisodeSpan {
                season: None,
                first: a,
                last: b,
                special: false,
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
    if let Some(span) = scan_tails(&RE_OVA_EP, lower, 1, 2) {
        return Some(EpisodeSpan {
            special: true,
            ..span
        });
    }
    // `SP01` / `Special 01` / `OAD 01`, the same shape with the other
    // marker words.
    if let Some(span) = scan_tails(&RE_SPECIAL_EP, lower, 1, 2) {
        return Some(EpisodeSpan {
            special: true,
            ..span
        });
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
pub(crate) fn is_non_episodic_extra(lower: &str) -> bool {
    // anitomy rejects NUL bytes; nothing real carries them.
    if lower.contains('\0') {
        return false;
    }
    let elements = anitomy_parse(lower);
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
pub(crate) fn keyword_from(haystack: &str, keyword: &str, min_at: usize) -> bool {
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

/// A release's revision, Sonarr's `Revision` without the `REAL`
/// counter (a scene TV convention anime never uses). `version` is 1
/// for a plain release, the `vN` number when the name carries one,
/// and 2 for a `PROPER` / `REPACK` / `RERIP` with no explicit number.
/// `repack` records the REPACK / RERIP word on its own so a caller can
/// tell a fixed re-release from a fansub `v2`; both rank the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseRevision {
    pub version: u32,
    pub repack: bool,
}

impl Default for ReleaseRevision {
    fn default() -> Self {
        Self {
            version: 1,
            repack: false,
        }
    }
}

impl ReleaseRevision {
    /// True when `self` is a later revision of the same release than
    /// `other`. Only the version number orders revisions; a REPACK is
    /// a v2 and a `v3` beats it.
    pub fn is_newer_than(self, other: ReleaseRevision) -> bool {
        self.version > other.version
    }
}

/// Parse the revision off a release title or file name. Takes the
/// original-case name: the `vN` token is read case-insensitively
/// through [`parse_release_version`], the scene words only in
/// capitals (see `RE_PROPER`).
pub fn parse_release_revision(name: &str) -> ReleaseRevision {
    let repack = RE_REPACK.is_match(name);
    let proper = repack || RE_PROPER.is_match(name);
    let explicit = parse_release_version(&name.to_lowercase());
    let version = match explicit {
        Some(v) => v.max(1),
        None if proper => 2,
        None => 1,
    };
    ReleaseRevision { version, repack }
}
