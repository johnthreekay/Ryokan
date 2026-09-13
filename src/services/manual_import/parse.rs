//! Per-file parsing for the manual-import wizard (#122): which series
//! a file belongs to, and which episode it is.
//!
//! Title extraction reuses `library_link::extract_anime_title` (the
//! anitomy `AnimeTitle` token the grab-time resolver already trusts)
//! and falls back to the parent folder when the filename carries no
//! title at all (`01.mkv`, `S01E05.mkv`, `Episode 07.mkv`), skipping
//! season-style folders (`Season 01`, `Specials`) on the way up so
//! `Anime/Naruto/Season 01/01.mkv` resolves to "Naruto". Episode and
//! season numbers come from `media::parse_episode_number`, the same
//! parser the library scan uses, so the preview's `S01E07` is the one
//! the import would write.

use std::path::Path;
use std::sync::LazyLock;

use anitomy::{Anitomy, ElementCategory};
use regex_lite::Regex;

use crate::services::{library_link, media};

/// Where the series-title hint for a file came from. Shown in the
/// preview so a wrong match is explainable ("it read the folder name").
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum TitleSource {
    Filename,
    ParentFolder,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedFile {
    pub title: Option<String>,
    pub title_source: TitleSource,
    pub season: Option<i32>,
    /// The first (usually only) episode the file holds.
    pub episode: Option<i32>,
    /// How many episodes the file holds (issue #246): 1 for the usual
    /// file, 2 for `S01E05-E06`.
    pub episode_count: i32,
    /// Year hint from the filename or, failing that, the folder the
    /// title came from. Feeds the match ranking; never persisted.
    pub year: Option<i32>,
    pub group: Option<String>,
}

/// Folder names that group episodes rather than name a series. The
/// parent-folder fallback climbs past these.
static RE_SEASON_FOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:season|series|s)\s*\d{1,3}|specials?|extras?|ovas?|movies?|bonus|nc(?:op|ed)s?|disc\s*\d+|bd\s*\d+|vol(?:ume)?\.?\s*\d+|subs?|raws?|batch)$",
    )
    .expect("RE_SEASON_FOLDER compiles")
});

/// Trailing season marker on a title: `Title S3`, `Title S03`,
/// `Title Season 3`, `Title 3rd Season`, `Title II`. SubsPlease names
/// sequels `Title S3 - 18`, and anitomy leaves the `S3` inside the
/// title, which AniList's search then refuses to match at all.
static RE_TITLE_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)[\s._-]+(?:s(\d{1,2})|season\s*(\d{1,2})|(\d{1,2})(?:st|nd|rd|th)\s+season)\s*$",
    )
    .expect("RE_TITLE_SEASON compiles")
});

/// Roman-numeral sequels (`Overlord IV`, `Mob Psycho 100 II`), II to IV
/// only and uppercase only: `x` in `Hunter x Hunter` and the odd title
/// ending in a lone `V` or `X` must stay put.
static RE_TITLE_ROMAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+(II|III|IV)$").expect("RE_TITLE_ROMAN compiles"));

/// Number carried by a season-style folder (`Season 3`, `S03`,
/// `Series 2`). `Specials` and friends carry none.
static RE_FOLDER_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:season|series|s)\s*(\d{1,3})$").expect("RE_FOLDER_SEASON compiles")
});

/// A four-digit year in the 1900s/2000s with a non-digit on both sides
/// (so `1080p` and `S2024E01`-style runs don't match).
static RE_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^0-9])((?:19|20)\d{2})(?:[^0-9]|$)").expect("RE_YEAR compiles")
});

/// anitomy "titles" that are really episode markers. `Episode 07.mkv`
/// parses with `AnimeTitle = "Episode"`; that must fall through to the
/// folder.
const NOT_A_TITLE: &[&str] = &[
    "episode", "episodes", "ep", "e", "ova", "ona", "oad", "special", "specials", "movie",
    "season", "part", "volume", "vol", "disc", "bonus", "extra", "extras",
];

fn looks_like_title(s: &str) -> bool {
    let t = s.trim();
    if !t.chars().any(|c| c.is_alphabetic()) {
        return false;
    }
    let lower = t.to_lowercase();
    !NOT_A_TITLE.contains(&lower.as_str())
}

pub fn is_season_folder(name: &str) -> bool {
    RE_SEASON_FOLDER.is_match(name.trim().to_lowercase().as_str())
}

/// Season number a `Season N` / `SN` folder names, if it names one.
pub fn folder_season(name: &str) -> Option<i32> {
    RE_FOLDER_SEASON
        .captures(name.trim().to_lowercase().as_str())
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Split a trailing season marker off a title: `("Enen no Shouboutai S3")`
/// becomes `("Enen no Shouboutai", Some(3))`. Titles without a marker
/// come back unchanged with `None`.
pub fn split_season_marker(title: &str) -> (String, Option<i32>) {
    let t = title.trim();
    if let Some(caps) = RE_TITLE_SEASON.captures(t) {
        let season = (1..=3)
            .filter_map(|i| caps.get(i))
            .find_map(|m| m.as_str().parse::<i32>().ok());
        let head = t[..caps.get(0).unwrap().start()].trim_end_matches([' ', '.', '_', '-']);
        if season.is_some() && looks_like_title(head) {
            return (head.to_string(), season);
        }
    }
    if let Some(caps) = RE_TITLE_ROMAN.captures(t) {
        let season = match caps.get(1).map(|m| m.as_str()) {
            Some("II") => 2,
            Some("III") => 3,
            Some("IV") => 4,
            _ => 0,
        };
        let head = t[..caps.get(0).unwrap().start()].trim_end();
        if season > 0 && looks_like_title(head) {
            return (head.to_string(), Some(season));
        }
    }
    (t.to_string(), None)
}

/// Season marker on the series folder, when the folder names the same
/// show as the file: `Overlord IV/Overlord - 03.mkv` gives 4. Season-
/// style folders (`Season 3`, `Specials`) are climbed past first. A
/// folder naming a different show is ignored so a mixed folder can't
/// stamp its season onto a stranger's files.
fn season_from_series_folder(rel_path: &Path, file_title: &str) -> Option<i32> {
    let mut cur = rel_path.parent();
    while let Some(dir) = cur {
        let name = dir.file_name().and_then(|n| n.to_str())?;
        if !is_season_folder(name) {
            let folder_title = title_from_folder(name)?;
            let (clean, marker) = split_season_marker(&folder_title);
            let a = crate::services::manual_import::matching::normalize_title(&clean);
            let b = crate::services::manual_import::matching::normalize_title(file_title);
            let related =
                !a.is_empty() && !b.is_empty() && (a == b || a.contains(&b) || b.contains(&a));
            return if related { marker } else { None };
        }
        cur = dir.parent();
    }
    None
}

/// Nearest season-style ancestor folder that names a number
/// (`Show/Season 3/01.mkv` gives 3). `None` when no ancestor does.
fn season_from_folders(rel_path: &Path) -> Option<i32> {
    let mut cur = rel_path.parent();
    while let Some(dir) = cur {
        let name = dir.file_name().and_then(|n| n.to_str())?;
        if let Some(n) = folder_season(name) {
            return Some(n);
        }
        cur = dir.parent();
    }
    None
}

/// First plausible year in `s`.
pub fn year_hint(s: &str) -> Option<i32> {
    RE_YEAR
        .captures(s)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Strip bracket / paren groups and collapse whitespace: the raw-folder
/// fallback when anitomy can't find a title in a folder name like
/// `[Judas] Naruto (2002) [BD 1080p]` (anitomy usually can; this is
/// the last resort).
fn clean_folder_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0i32;
    for c in name.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Release-shaped tokens that mean a folder name carries more than a
/// title (`Naruto Shippuden 1080p BD`) and needs anitomy to trim it.
static RE_RELEASE_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[^a-z0-9])(?:\d{3,4}p|bd|bluray|blu-ray|bdrip|bdmv|web|webrip|web-dl|webdl|dvd|dvdrip|hdtv|x264|x265|h264|h265|h\.264|h\.265|hevc|avc|av1|flac|aac|opus|dual|remux|10bit|8bit|hi10p|batch|complete)(?:[^a-z0-9]|$)",
    )
    .expect("RE_RELEASE_TOKEN compiles")
});

/// Title hint from a folder name. Bracket groups (`[Group]`, `(2019)`,
/// `[BD 1080p]`) are stripped first; when nothing release-shaped is
/// left the remainder IS the title. anitomy only runs when release
/// tokens survive the strip, because on a bare folder name it reads a
/// trailing number as an episode (`Mob Psycho 100` becomes `Mob
/// Psycho`), and folders don't carry episode numbers.
fn title_from_folder(name: &str) -> Option<String> {
    let cleaned = clean_folder_name(name);
    if cleaned.is_empty() {
        return None;
    }
    if !RE_RELEASE_TOKEN.is_match(&cleaned) {
        return looks_like_title(&cleaned).then_some(cleaned);
    }
    if let Some(t) = library_link::extract_anime_title(&cleaned)
        && looks_like_title(&t)
    {
        return Some(t);
    }
    looks_like_title(&cleaned).then_some(cleaned)
}

/// Walk `rel_path`'s ancestors nearest-first, returning the first
/// folder that names a series (and that folder's raw name for the
/// year hint).
fn parent_title(rel_path: &Path) -> Option<(String, String)> {
    let mut cur = rel_path.parent();
    while let Some(dir) = cur {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            if !is_season_folder(name)
                && let Some(t) = title_from_folder(name)
            {
                return Some((t, name.to_string()));
            }
        } else {
            break;
        }
        cur = dir.parent();
    }
    None
}

/// Parse one candidate file. `rel_path` is relative to the walk root
/// so the folder fallback never climbs above what the user pointed at.
pub fn parse_file(rel_path: &Path) -> ParsedFile {
    let file_name = rel_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let stem = Path::new(&file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&file_name)
        .to_string();

    // One anitomy pass for the title/year/group trio. The project's
    // tuned episode parser (`media::parse_episode_number`) owns the
    // episode/season decision, not anitomy's EpisodeNumber.
    let clean = if file_name.contains('\0') {
        file_name.replace('\0', "")
    } else {
        file_name.clone()
    };
    let mut ani = Anitomy::new();
    let elements = match ani.parse(&clean) {
        Ok(e) => e,
        Err(e) => e,
    };
    let mut title = elements
        .get(ElementCategory::AnimeTitle)
        .map(|s| s.trim().to_string())
        .filter(|s| looks_like_title(s));
    let mut year = elements
        .get(ElementCategory::AnimeYear)
        .and_then(|y| y.trim().parse::<i32>().ok())
        .or_else(|| year_hint(&stem));
    let group = elements
        .get(ElementCategory::ReleaseGroup)
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty());

    let (season, episode, episode_count) =
        match media::parse_episode_span(&file_name.to_lowercase()) {
            Some(span) => (span.season, Some(span.first), span.count()),
            None => (None, None, 1),
        };

    let mut title_source = if title.is_some() {
        TitleSource::Filename
    } else {
        TitleSource::None
    };
    if title.is_none()
        && let Some((t, folder_raw)) = parent_title(rel_path)
    {
        title = Some(t);
        title_source = TitleSource::ParentFolder;
        if year.is_none() {
            year = year_hint(&folder_raw);
        }
    }

    // A season marker inside the title (`Title S3`, `Title II`) names
    // the season and must leave the title, or the AniList search
    // gets a string it can't match. An explicit `S03E18` in the
    // filename wins when both are present; a `Season N` folder is the
    // last resort.
    let mut season = season;
    if let Some(t) = title.take() {
        let (clean, marker) = split_season_marker(&t);
        title = Some(clean);
        if season.is_none() {
            season = marker;
        }
    }
    if season.is_none()
        && title_source == TitleSource::Filename
        && let Some(t) = title.as_deref()
    {
        season = season_from_series_folder(rel_path, t);
    }
    if season.is_none() {
        season = season_from_folders(rel_path);
    }

    ParsedFile {
        title,
        title_source,
        season,
        episode,
        episode_count,
        year,
        group,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ParsedFile {
        parse_file(Path::new(s))
    }

    #[test]
    fn release_style_filename_parses_title_episode_group_from_name() {
        let f = p("Downloads/[SubsPlease] Frieren - Sousou no Frieren - 05 (1080p) [ABCD1234].mkv");
        assert_eq!(f.title.as_deref(), Some("Frieren - Sousou no Frieren"));
        assert_eq!(f.title_source, TitleSource::Filename);
        assert_eq!(f.episode, Some(5));
        assert_eq!(f.group.as_deref(), Some("SubsPlease"));
    }

    #[test]
    fn bare_number_filename_falls_back_to_parent_folder_past_season_dir() {
        let f = p("Anime/Naruto/Season 01/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Naruto"));
        assert_eq!(f.title_source, TitleSource::ParentFolder);
        assert_eq!(f.episode, Some(1));
    }

    #[test]
    fn sxxexx_filename_uses_folder_title_and_keeps_season() {
        let f = p("Mob Psycho 100/S02E03.mkv");
        assert_eq!(f.title.as_deref(), Some("Mob Psycho 100"));
        assert_eq!(f.title_source, TitleSource::ParentFolder);
        assert_eq!(f.season, Some(2));
        assert_eq!(f.episode, Some(3));
    }

    #[test]
    fn episode_word_is_not_a_title() {
        let f = p("Cowboy Bebop/Episode 07.mkv");
        assert_eq!(f.title.as_deref(), Some("Cowboy Bebop"));
        assert_eq!(f.title_source, TitleSource::ParentFolder);
        assert_eq!(f.episode, Some(7));
    }

    #[test]
    fn year_hint_comes_from_filename_then_folder() {
        let f = p("Hunter x Hunter (2011)/Specials/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Hunter x Hunter"));
        assert_eq!(f.year, Some(2011));

        let f = p("Show/[Group] Show (2019) - 01 [1080p].mkv");
        assert_eq!(f.year, Some(2019));
        assert_eq!(f.episode, Some(1));
    }

    #[test]
    fn resolution_is_not_a_year() {
        assert_eq!(year_hint("Show - 01 [1080p x264]"), None);
        assert_eq!(year_hint("Show 2160p"), None);
        assert_eq!(year_hint("Show (2016) 1080p"), Some(2016));
    }

    #[test]
    fn no_title_anywhere_yields_none() {
        let f = p("Season 01/01.mkv");
        assert_eq!(f.title, None);
        assert_eq!(f.title_source, TitleSource::None);
        assert_eq!(f.episode, Some(1));
    }

    #[test]
    fn season_folder_detection() {
        for s in [
            "Season 01",
            "season 2",
            "S1",
            "Specials",
            "Extras",
            "OVA",
            "Disc 1",
            "Vol. 3",
        ] {
            assert!(is_season_folder(s), "{s} should be a season-style folder");
        }
        for s in ["Naruto", "Season of Love", "86 Eighty Six", "S.A.O"] {
            assert!(
                !is_season_folder(s),
                "{s} should not be a season-style folder"
            );
        }
    }

    #[test]
    fn bracketed_folder_names_clean_up() {
        let f = p("[Judas] Vinland Saga (2019) [BD 1080p]/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Vinland Saga"));
        assert_eq!(f.year, Some(2019));
    }

    #[test]
    fn folder_names_with_release_tokens_but_no_brackets_get_trimmed() {
        let f = p("Naruto Shippuden 1080p BD Dual Audio/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Naruto Shippuden"));
    }

    #[test]
    fn folder_trailing_number_is_part_of_the_title() {
        let f = p("86 Eighty Six/01.mkv");
        assert_eq!(f.title.as_deref(), Some("86 Eighty Six"));
        let f = p("Steins;Gate 0/S01E01.mkv");
        assert_eq!(f.title.as_deref(), Some("Steins;Gate 0"));
    }

    #[test]
    fn subsplease_sequel_naming_reads_the_season_out_of_the_title() {
        // The four shapes from a real library scan that AniList
        // returned nothing for while `S3` sat inside the query.
        let cases = [
            (
                "Fire Force/Season 3/[SubsPlease] Enen no Shouboutai S3 - 18 (1080p) [1E9E354E].mkv",
                "Enen no Shouboutai",
                3,
                18,
            ),
            (
                "Oshi no Ko/Season 3/[SubsPlease] Oshi no Ko S3 - 05 (1080p) [5D301BC1].mkv",
                "Oshi no Ko",
                3,
                5,
            ),
            (
                "Frieren/[SubsPlease] Sousou no Frieren S2 - 05 (1080p) [6AAEC79A].mkv",
                "Sousou no Frieren",
                2,
                5,
            ),
            (
                "Tanya/[SubsPlease] Youjo Senki S2 - 02 (1080p) [9A361F78].mkv",
                "Youjo Senki",
                2,
                2,
            ),
        ];
        for (path, title, season, ep) in cases {
            let f = p(path);
            assert_eq!(f.title.as_deref(), Some(title), "{path}");
            assert_eq!(f.season, Some(season), "{path}");
            assert_eq!(f.episode, Some(ep), "{path}");
        }
    }

    #[test]
    fn season_markers_in_folder_titles_and_roman_numerals() {
        let f = p("Mob Psycho 100 II/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Mob Psycho 100"));
        assert_eq!(f.season, Some(2));
        let f = p("Overlord IV/Overlord - 03.mkv");
        assert_eq!(f.title.as_deref(), Some("Overlord"));
        assert_eq!(
            f.season,
            Some(4),
            "series-folder marker applies to a matching file title"
        );
        let f = p("Overlord IV/Bleach - 03.mkv");
        assert_eq!(
            f.season, None,
            "a different show's folder does not stamp its season"
        );
        let f = p("Yuru Camp 2nd Season/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Yuru Camp"));
        assert_eq!(f.season, Some(2));
        let f = p("Mob Psycho 100 Season 3/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Mob Psycho 100"));
        assert_eq!(f.season, Some(3));
    }

    #[test]
    fn season_comes_from_a_season_folder_when_the_name_has_none() {
        let f = p("Show/Season 3/Show - 18.mkv");
        assert_eq!(f.title.as_deref(), Some("Show"));
        assert_eq!(f.season, Some(3));
        // Ryokan's own layout is always Season 01: reads as season 1.
        let f = p("Show/Season 01/Show - 01.mkv");
        assert_eq!(f.season, Some(1));
        // An explicit SxxExx beats a disagreeing folder.
        let f = p("Show/Season 3/Show - S02E04.mkv");
        assert_eq!(f.season, Some(2));
    }

    #[test]
    fn non_markers_stay_in_the_title() {
        for (path, title) in [
            ("Hunter x Hunter/01.mkv", "Hunter x Hunter"),
            ("Steins;Gate 0/01.mkv", "Steins;Gate 0"),
            ("Gundam X/01.mkv", "Gundam X"),
            (
                "Girls und Panzer das Finale/01.mkv",
                "Girls und Panzer das Finale",
            ),
        ] {
            let f = p(path);
            assert_eq!(f.title.as_deref(), Some(title), "{path}");
            assert_eq!(f.season, None, "{path}");
        }
        assert_eq!(
            split_season_marker("S2"),
            ("S2".to_string(), None),
            "no title left"
        );
        assert_eq!(folder_season("Specials"), None);
        assert_eq!(folder_season("S02"), Some(2));
    }

    #[test]
    fn ncop_files_have_no_episode_but_keep_title() {
        let f = p("Show/[Group] Show - NCOP1 [1080p].mkv");
        assert_eq!(f.episode, None);
        assert!(f.title.is_some());
    }
}
