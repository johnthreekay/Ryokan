//! File-name mode: what one file's name holds, for the import, the
//! scanner and every other reader of a file name (`services::media`
//! re-exports this module, so their call sites read `media::`).
//!
//! `parse_episode_span` is `ParsedTitle::file_span` (the release-title
//! rules with the file-name shape rules: a loose range keeps its first
//! number or names nothing, a run longer than [`MAX_FILE_SPAN`] is no
//! file, only a tag sets a season, a title-word `Special` is the name),
//! then two residual shapes the title rules leave open, then the
//! special-marker regexes; extras are the title rules' own `extra` (the
//! anitomy `AnimeType` gate went in phase 12b). It replaced the old branch
//! regexes (phase 11b of the parser plan); every difference over the
//! corpora was reviewed (the filename corpus notes each change).

use std::sync::LazyLock;

use regex_lite::Regex;
use serde::Serialize;

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

/// Parse episode (and optionally season) number from a filename: the
/// first episode of [`parse_episode_span`] and the season a tag names.
/// `S01E05`, `- 05v2`, `E05`, `Episode 05`, `OVA 01`, `[05]`, a bare
/// number after the title (`Title 05 (1080p)`, `[EA]Title_05_`), a dash
/// episode before episode-title words, and a dot-delimited scene name
/// all read; creditless and preview extras, half episodes (`- 07.5`) and
/// a bare special marker with no number read as `None`.
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

/// [`parse_episode_number`] with the multi-episode tail kept. The
/// branch order and every guard are the ones documented on that
/// function; only the return shape differs.
pub fn parse_episode_span(lower: &str) -> Option<EpisodeSpan> {
    let mut parsed = file_reading(lower)?;
    if !parsed.special && (parsed.season == Some(0) || has_special_marker(lower)) {
        parsed.special = true;
    }
    Some(parsed)
}

/// [`parse_episode_span`] for a file whose series is known: a number
/// its own name holds is never the episode (`Mob Psycho 100 (BD).mkv`),
/// and where the name is found, a bare reading is the number right after
/// it. The extras rule and the special markers decide first, as without
/// the series.
pub fn parse_episode_span_for(lower: &str, anchors: &super::SeriesAnchors) -> Option<EpisodeSpan> {
    let span = parse_episode_span(lower)?;
    match super::parse(lower).name_reading(anchors, span.first) {
        None => Some(span),
        Some(None) => None,
        Some(Some(n)) => Some(EpisodeSpan {
            first: n,
            last: n,
            ..span
        }),
    }
}

/// Folder names that hold a pack's extras: a file under one is never an
/// episode, whatever it is called (`Extras/Previews/Preview 01.mkv`,
/// `Extra/Preview/Ep. 1.mkv`, `特典映像/[...][Tokuten][07].mkv`). Only a
/// folder named exactly this counts, so `[Group] Show (BD) + Extras/` is
/// still a show's folder. `Specials/` is the import's own rule
/// (`media::path_names_specials`).
const EXTRAS_FOLDERS: &[&str] = &[
    "extra",
    "extras",
    "bonus",
    "bonuses",
    "menu",
    "menus",
    "nc",
    "ncop",
    "nced",
    "ncoped",
    "creditless",
    "nc op ed",
    "ncop ed",
    "pv",
    "pvs",
    "cm",
    "cms",
    "preview",
    "previews",
    "trailer",
    "trailers",
    "teaser",
    "teasers",
    "promo",
    "promos",
    "yokoku",
    "yokoku anime",
    "tokuten",
    "特典",
    "特典映像",
    "映像特典",
    "scans",
    "fonts",
    "ost",
    "featurettes",
    "interviews",
    "behind the scenes",
    "deleted scenes",
    "making",
    "commentary",
];

/// Words a disc's extras folder may lead with (`Bluray Extras`, `BD
/// Menu`, `DVD Bonus`, `Disc 2 Extras`).
const MEDIUM_WORDS: &[&str] = &["bd", "bluray", "blu", "ray", "dvd", "disc", "disk", "bdmv"];

/// Whether a file sits under an extras folder ([`EXTRAS_FOLDERS`], after
/// any [`MEDIUM_WORDS`] and disc numbers). `path` uses `/` (a torrent's
/// file list) or the platform separator.
pub fn path_names_extras(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split(['/', '\\']).collect();
    parts.pop();
    parts.iter().any(|dir| {
        let words: String = dir
            .to_lowercase()
            .chars()
            // `【特典映像】`, `（特典）`: CJK brackets and spaces separate too.
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect();
        let words: Vec<&str> = words.split_whitespace().collect();
        let lead = words
            .iter()
            .take_while(|w| MEDIUM_WORDS.contains(w) || w.bytes().all(|b| b.is_ascii_digit()))
            .count();
        EXTRAS_FOLDERS.contains(&words[lead..].join(" ").as_str())
    })
}

/// [`parse_episode_span`] for a file in a download or a library folder,
/// given its path: only the file's own name is read (a folder's `01-12`
/// is no file's), and a file under an extras folder holds no episode
/// unless its name says it is a special (`Extras/Show - OVA 01.mkv` is
/// still the special a TV series files under `Specials/`).
pub fn parse_file_path(path: &str) -> Option<EpisodeSpan> {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    parse_episode_span(&base.to_lowercase()).filter(|s| s.special || !path_names_extras(path))
}

/// [`parse_episode_span_for`] given a path, as [`parse_file_path`].
pub fn parse_file_path_for(path: &str, anchors: &super::SeriesAnchors) -> Option<EpisodeSpan> {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    parse_episode_span_for(&base.to_lowercase(), anchors)
        .filter(|s| s.special || !path_names_extras(path))
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

/// A dash episode with no range after it (`- 39 Fanart Corner`), for
/// [`file_reading`]'s residual pass.
static RE_DASH_SINGLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r" - (\d{1,4})(?:v\d)?(?:\s|\.|\[|\(|$)").expect("RE_DASH_SINGLE compiles")
});

/// The release-title reading of a file name (`ParsedTitle::file_span`),
/// then, only when that reading neither finds an episode nor says the
/// name holds none (a claim it refused, an extra, a half episode, a
/// special, a season or volume pack), the two old shapes the title rules
/// leave open: a dash
/// episode followed by episode-title words (`- 39 Fanart Corner`,
/// `- 05 Episode Title`) and a dot-delimited scene name
/// (`Series.103.hdtv-lol`).
fn file_reading(lower: &str) -> Option<EpisodeSpan> {
    let title = super::parse(lower);
    if let Some(span) = title.file_span() {
        return Some(span);
    }
    // A season marker or a title-word special (`Show S2 - 05 The
    // Beginning`, `Attack on Titan OAD - 01 The Title`) says nothing
    // about the episode.
    if title.episodes.is_some()
        || title.extra
        || title.half_episode
        || title.file_special()
        || title.volume_range
    {
        return None;
    }
    let normalized = lower.replace('_', " ");
    let dash = RE_DASH_SINGLE
        .captures(&normalized)
        .and_then(|caps| caps.get(1)?.as_str().parse::<i32>().ok());
    // A dotted name with a season marker is a season's folder
    // (`Series.Season.04.HDTV`), not one episode.
    dash.or_else(|| {
        title
            .seasons
            .numbers
            .is_empty()
            .then(|| parse_dot_delimited_episode(lower))
            .flatten()
    })
    .map(|n| EpisodeSpan {
        season: None,
        first: n,
        last: n,
        special: false,
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

#[cfg(test)]
mod tests {
    use super::{parse_episode_span, parse_episode_span_for};
    use crate::services::release_title::SeriesAnchors;

    fn first(name: &str, aliases: &[&str]) -> Option<i32> {
        parse_episode_span_for(&name.to_lowercase(), &SeriesAnchors::new(aliases)).map(|s| s.first)
    }

    #[test]
    fn a_number_the_series_name_holds_is_never_the_file_episode() {
        let mob = ["Mob Psycho 100"];
        assert_eq!(
            parse_episode_span("[g] mob psycho 100 (bd 1080p).mkv").map(|s| s.first),
            Some(100)
        );
        assert_eq!(first("[G] Mob Psycho 100 (BD 1080p).mkv", &mob), None);
        assert_eq!(
            first("[G] Mob Psycho 100 - 05 (BD 1080p).mkv", &mob),
            Some(5)
        );
        // A dash episode equal to the title's number is still the episode.
        assert_eq!(
            first(
                "[SubsPlease] Kaiju No. 8 - 08 (1080p).mkv",
                &["Kaiju No. 8"]
            ),
            Some(8)
        );
    }

    #[test]
    fn an_episode_title_after_a_bare_number_leaves_it_the_episode() {
        // From the 29k-title corpus' file lists, which neither the old
        // parser nor the new one read.
        let first = |name: &str| parse_episode_span(&name.to_lowercase()).map(|s| s.first);
        assert_eq!(first("Ep-04 Blood Line Fever.mkv"), Some(4));
        assert_eq!(
            first(
                "[FMA1394] Fullmetal Alchemist 23 'Heart of Steel' [Dual Audio] [US BD] [892C8762].mkv"
            ),
            Some(23)
        );
        assert_eq!(
            first("[TardS] Holo no Graffiti 095 Going Wild in a Shopping Mall  (WEB 1080p).mp4"),
            Some(95)
        );
        assert_eq!(
            first("[Samir755] Hunter X hunter 1999 -85- Jan × Ken × Rock.mkv"),
            Some(85)
        );
        // A name that is only an extras label and its number is an extra;
        // `[SP][01]` is a special.
        assert_eq!(first("Previews 01.mkv"), None);
        assert_eq!(first("[G] Opening 01 [1080p].mkv"), None);
        let sp = parse_episode_span(
            &"[DBD-Raws][Mobile Suit Gundam The 08th MS Team Miller`s Report][SP][01][1080P][BDRip][HEVC-10bit][FLAC].mkv".to_lowercase(),
        );
        assert!(sp.is_some_and(|s| s.special), "{sp:?}");
        // A title's own number stays the title's: four digits, unpadded,
        // or opening the name.
        assert_eq!(
            first("Mobile Suit Gundam 0083 Stardust Memory [BD].mkv"),
            None
        );
        assert_eq!(first("[CW] 009 Re-Cyborg (2012) [BD 1080p].mkv"), None);
        assert_eq!(first("Cyborg 009 Call of Justice [BD].mkv"), None);
        // An episode written with a mark leaves the title's `0` the title's.
        assert_eq!(
            first("[JacobSwaggedUp] Steins;Gate 0 - 23β (BD 1280x720).mp4"),
            None
        );
        assert_eq!(
            first("Mobile Suit Gundam 00 the Movie A wakening of the Trailblazer (2010).mkv"),
            None
        );
        // A movie's number is no episode before the movie's title; the
        // lone video takes the grab's episode instead.
        assert_eq!(
            first("Detective Conan Movie 02 The Fourteenth Target [BD 1080p].mkv"),
            None
        );
        assert_eq!(first("Show 99 Luftballons [1080p].mkv"), None);
    }

    #[test]
    fn a_decimal_after_a_claimed_episode_is_not_a_half_episode() {
        let read = |name: &str| {
            parse_episode_span(&name.to_lowercase()).map(|s| (s.season, s.first, s.special))
        };
        // Ryokan's own name for a show whose episode title has a decimal.
        assert_eq!(
            read("2.5 Dimensional Seduction - S01E24 - 2.5 Dimensional Ririsa.mkv"),
            Some((Some(1), 24, false))
        );
        assert_eq!(
            read(
                "Stigmata (2010) - S00E17 - Portrait of the Empress Episode 10.5 [Bluray-1080p].mkv"
            ),
            Some((Some(0), 17, true))
        );
        // A quality score in the metadata.
        assert_eq!(
            read(
                "[Delta] That Time I Got Reincarnated as a Slime S04E23 [1080p CR-WEB-MINI AAC-192 HEVC VMAF-93.5].mkv"
            ),
            Some((Some(4), 23, false))
        );
        assert_eq!(
            read("[Delta] Show - 23 [1080p CR-WEB-MINI AAC-192 HEVC VMAF-93.5].mkv"),
            Some((None, 23, false))
        );
        // A half episode is still one, after a dash number that is the
        // name's (`- 100 Years Quest`) too.
        assert_eq!(read("[Group] Show - 07.5 [1080p].mkv"), None);
        assert_eq!(read("[Group] Show Episode 7.5 [1080p].mkv"), None);
        assert_eq!(
            read("[SubsPlease] Fairy Tail - 100 Years Quest - 13.5 (1080p) [CDB92274].mkv"),
            None
        );
        // An episode titled with an extras word alone, after a tag.
        assert_eq!(
            read("Show - S01E05 - Preview.mkv"),
            Some((Some(1), 5, false))
        );
        assert_eq!(read("[Group] Show - 05 - Preview [1080p].mkv"), None);
        assert_eq!(read("Show - S01E05 - NCOP.mkv"), None);
    }

    #[test]
    fn a_number_before_final_is_the_episode() {
        let plain = |name: &str| parse_episode_span(&name.to_lowercase()).map(|s| s.first);
        // Issue #296's own pack ends on `_25_Final_`; the import dropped it.
        assert_eq!(
            plain("[EA]Dark_Gathering_25_Final_[1920x1080][BDRIP][HEVC][5C730C8C].mkv"),
            Some(25)
        );
        assert_eq!(plain("Kuromukuro 26 END [720p].mkv"), Some(26));
        // `Final` opening an episode title leaves the number the episode.
        assert_eq!(plain("Show 05 Final Battle [1080p].mkv"), Some(5));
        assert_eq!(plain("Show 07 Final Exam.mkv"), Some(7));
        assert_eq!(
            plain("[Group] Show - 05 - The Final Battle [1080p].mkv"),
            Some(5)
        );
        // After a season already named, the number is the episode.
        assert_eq!(
            plain("[Group] Shingeki no Kyojin The Final Season 05 [1080p].mkv"),
            Some(5)
        );
        // A title's own number before `Final` is vetoed by the series' name,
        // as `Mob Psycho 100 (BD)` is.
        assert_eq!(
            first("Mob Psycho 100 Final [BD].mkv", &["Mob Psycho 100"]),
            None
        );
    }

    #[test]
    fn a_file_under_an_extras_folder_holds_no_episode() {
        use super::{parse_file_path, path_names_extras};
        // From the 29k-title corpus' file lists: the folder is the only
        // sign these are extras.
        for path in [
            "Extras/Previews/Preview 01.mkv",
            "Extra/Preview/Ep. 1.mkv",
            "(Extras)/[MQS] Dragon Quest - Episodes 1 - 4 Recap [DVD Special].mkv",
            "特典映像/[DBD-Raws][Submarine Super 99][Tokuten][07][480P][BDRip][AVC][AC3].mkv",
            "Show\\Extras\\Preview 01.mkv",
            "Bluray Extras/Previews 01.mkv",
            "Disc 2 Extras/Clip 01.mkv",
        ] {
            assert!(path_names_extras(path), "{path}");
            assert_eq!(parse_file_path(path), None, "{path}");
        }
        assert!(path_names_extras("【特典映像】/Clip 01.mkv"));
        assert!(path_names_extras("（特典）/Clip 01.mkv"));
        // A special stays a special there.
        assert!(parse_file_path("Extras/[G] Show - OVA 01.mkv").is_some_and(|s| s.special));
        // A show folder that mentions extras is still the show's.
        let main = "[Group] Show (BD) + Extras/[Group] Show - 05 [1080p].mkv";
        assert!(!path_names_extras(main));
        assert_eq!(parse_file_path(main).map(|s| s.first), Some(5));
        // Only the file's own name is read, never a folder's range.
        assert_eq!(
            parse_file_path(
                "[EA]Dark_Gathering_01-25_End_[BDRIP]/[EA]Dark_Gathering_05_[1920x1080][HEVC].mkv"
            )
            .map(|s| s.first),
            Some(5)
        );
    }

    #[test]
    fn a_number_written_again_past_the_name_is_the_episode() {
        assert_eq!(
            first(
                "[EA]Vinland_Saga_Season_2_02_[1080p].mkv",
                &["Vinland Saga Season 2"]
            ),
            Some(2)
        );
        assert_eq!(
            first(
                "[EA]Kaiju_No_8_Season_2_08_[1080p].mkv",
                &["Kaiju No. 8 Season 2"]
            ),
            Some(8)
        );
    }

    #[test]
    fn a_residual_reading_the_name_holds_is_no_episode() {
        assert_eq!(
            first("Mob.Psycho.100.Reigen.1080p.WEB.mkv", &["Mob Psycho 100"]),
            None
        );
        assert_eq!(first("Cyborg.009.Movie.BD.mkv", &["Cyborg 009"]), None);
    }

    #[test]
    fn a_found_name_with_nothing_readable_after_it_keeps_the_plain_reading() {
        assert_eq!(
            first(
                "[EA]Fruits_Basket_2019_05_[1080p].mkv",
                &["Fruits Basket (2019)"]
            ),
            Some(5)
        );
        assert_eq!(
            first(
                "Hunter.x.Hunter.2011.148.1080p.BluRay.x264.mkv",
                &["Hunter x Hunter (2011)"]
            ),
            Some(148)
        );
        assert_eq!(
            first("[Group] K Project 05 (BD 1080p).mkv", &["K"]),
            Some(5)
        );
        assert_eq!(
            first("[EA]Mushoku_Tensei_II_05_[1080p].mkv", &["Mushoku Tensei"]),
            Some(5)
        );
        assert_eq!(
            first(
                "[Group] Haibane Renmei 1 [BD 1080p].mkv",
                &["Haibane Renmei"]
            ),
            Some(1)
        );
        assert_eq!(first("[Group] Show - 5 Pilot.mkv", &["Show"]), Some(5));
    }

    #[test]
    fn where_the_name_is_found_the_episode_follows_it() {
        assert_eq!(
            first("[EA]Kaiju_No_8_08_[1080p].mkv", &["Kaiju No. 8"]),
            Some(8)
        );
        assert_eq!(first("[EA]Kaiju_No_8_[1080p].mkv", &["Kaiju No. 8"]), None);
        assert_eq!(
            first("[EA]Dark_Gathering_24_[1080p].mkv", &["Dark Gathering"]),
            Some(24)
        );
        // A name the series does not carry leaves the file's own reading.
        assert_eq!(
            first("[G] Other Show 05 (1080p).mkv", &["Dark Gathering"]),
            Some(5)
        );
    }
}
