//! One parser for anime release titles (and, in time, file names).
//!
//! `parse` reads a title into a [`ParsedTitle`]: release group, CRC, year,
//! resolution, seasons, the episode claim, specials / extras / half
//! episodes, revision, and the keywords that make a release a pack. It
//! needs no series knowledge. Callers then ask for a reading:
//!
//! - [`ParsedTitle::is_batch`]: is this a multi-episode pack.
//! - [`ParsedTitle::release_episodes`]: the episodes the title itself
//!   names. A bare number right after the title is never one of them.
//! - [`ParsedTitle::for_series`]: the episodes for a known series. A bare
//!   number right after the series' own name counts here, and a number
//!   inside that name (`Mob Psycho 100`) never does.
//!
//! The rule order is load-bearing; see `AGENTS.md` in this directory.

mod anchors;
#[cfg(test)]
mod bench;
#[cfg(test)]
mod differential;
pub(crate) mod file_mode;
#[cfg(test)]
mod legacy;
mod lexer;
mod normalize;
mod rules;
mod vocab;

use std::collections::HashSet;

pub use anchors::SeriesAnchors;
pub use file_mode::ReleaseRevision;
pub use vocab::roman_value;

/// Ranges wider than this name no slots (`1-1000` mega-batches); the
/// release is still a batch.
pub const MAX_EXPANDED_RANGE: i32 = 200;

/// Which rule found the episode claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimRule {
    /// `S01E05`, `S01E01-E02`, `1x05`.
    Tag,
    /// The number after a ` - ` (or `_-_`).
    Dash,
    /// `E05`, `EP05`, `Ep 05`.
    EpisodePrefix,
    /// `Episode 05`, `#05`, `第05话`, `04 of 12`.
    EpisodeWord,
    /// `OVA 01`, `SP01`, `Specials 01-02`.
    SpecialNumber,
    /// `[03]`, `[08v2]`, `(01-12)`, `[01-12 Fin]`.
    Bracket,
    /// A bare range in the title text: `01-25`, `1100-1104`.
    BareRange,
    /// A season marker then a bare number: `S2 05`, `2nd Season 05`, `II 05`.
    MarkerNumber,
}

/// What the title says about episodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeClaim {
    /// Set when a season marker is bound to these numbers (`S01E05`,
    /// `S2 - 04`). Without one, the numbers may be absolute.
    pub season: Option<i32>,
    pub first: i32,
    pub last: i32,
    /// A non-contiguous list (`Episode 27,69,76`); empty otherwise.
    pub list: Vec<i32>,
    pub rule: ClaimRule,
    /// A range written the way a file name writes one: glued to its
    /// operator, two digits or more on each side (`05-06`, not `05 - 06`
    /// or `5-6`). Always true for a single episode.
    pub tight: bool,
    /// The range's operator is glued to both numbers (`5-6`, `05-06`),
    /// not spaced (`05 - 06`). Always true for a single episode.
    pub glued: bool,
    /// The range opened on a zero-padded title number before its dash
    /// (`Show 01 - 119`): a file name's own episode is the dash's.
    pub opens_before_dash: bool,
}

impl EpisodeClaim {
    pub fn spans_several(&self) -> bool {
        self.last > self.first || self.list.len() > 1
    }

    /// Every episode the claim names, up to [`MAX_EXPANDED_RANGE`].
    pub fn episodes(&self) -> HashSet<i32> {
        if !self.list.is_empty() {
            return self.list.iter().copied().collect();
        }
        if self.last - self.first > MAX_EXPANDED_RANGE {
            return HashSet::new();
        }
        (self.first..=self.last).collect()
    }
}

/// Season facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seasons {
    /// Explicit season numbers. One for a single season; several for a
    /// multi-season pack (`S01-S04`, `S1+S2+S3`). From an `SxxEyy` tag
    /// when there is one.
    pub numbers: Vec<i32>,
    /// `Part N` / `P2` / `Vol N` of a season.
    pub part: Option<i32>,
    /// `numbers` came only from a `Part` / `Cour` hint (`Part 2 - 03`),
    /// with no explicit season marker.
    pub hint_only: bool,
    /// A top-level part marker right before a bracket (`Series Cour 2
    /// (WEB 1080p)`), which names a pack even without a season.
    pub part_pack: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedTitle {
    pub group: Option<String>,
    /// CRC32 from a bracket, uppercased.
    pub hash: Option<String>,
    /// Best-effort primary title text.
    pub title: Option<String>,
    pub year: Option<i32>,
    /// Vertical resolution (1080 for `1080p` or `1920x1080`).
    pub resolution: Option<u32>,
    pub seasons: Seasons,
    pub episodes: Option<EpisodeClaim>,
    /// A bare number right after the title (`[EA]Dark_Gathering_24_`,
    /// `Mob Psycho 100 (BD)`). Only series context can say whether it is
    /// an episode or part of the name, so no reading without one uses it.
    pub ambiguous_number: Option<i32>,
    /// The release is a special (`OVA 01`, `- OVA`, `S00E05`, `... OVA`).
    /// What that means is decided by the series' format.
    pub special: bool,
    /// A pack that includes specials (`01-13+OVA`, `S01 + OVAs`).
    pub includes_specials: bool,
    /// Creditless OP/ED, PV, CM, preview, trailer, menu.
    pub extra: bool,
    /// A half episode or recap (`- 07.5`, `07_5`, `S01E07.5`).
    pub half_episode: bool,
    pub revision: ReleaseRevision,
    /// `Batch`, `Complete`, `全集`, a movie collection.
    pub batch_keyword: bool,
    /// `END`, `Fin`, `完` somewhere in the title.
    pub end_marker: bool,
    /// A bracketed end marker (`[Fin]`, `[01-12 Fin]`).
    pub bracketed_end: bool,
    /// The release is a movie (not a pack that includes one).
    pub movie: bool,
    /// A volume range (`Vol 01-02`): a pack whose numbers are not episodes.
    pub volume_range: bool,
    /// An airing span (`[1983-1984]`): with no episode claim, a whole-run pack.
    pub year_range: bool,
    /// The title names one video file.
    pub single_file: bool,
    /// `special` rests only on a marker ending the title text, which a
    /// series named that way vetoes (see [`ParsedTitle::is_special_for`]).
    special_from_title_word: bool,
    anchor_words: Vec<anchors::Word>,
}

/// The episodes a release holds for one series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesReading {
    pub episodes: HashSet<i32>,
    pub via: ReadingSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingSource {
    /// The title's own episode claim.
    Structural,
    /// A bare number right after the series' name.
    AfterName,
    /// Nothing the series could place.
    None,
}

pub fn parse(title: &str) -> ParsedTitle {
    rules::parse(title)
}

/// What a release source stores beside a title, from one parse: the
/// group (empty when none), the resolution as bare digits (`"1080"`,
/// empty when none) and the batch flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseFields {
    pub group: String,
    pub resolution: String,
    pub is_batch: bool,
}

pub fn release_fields(title: &str) -> ReleaseFields {
    let parsed = parse(title);
    ReleaseFields {
        is_batch: parsed.is_batch(),
        resolution: parsed.resolution.map(|r| r.to_string()).unwrap_or_default(),
        group: parsed.group.unwrap_or_default(),
    }
}

/// The season a series name advertises (`2nd Season`, `Season 3`,
/// `Made in Abyss III`, `Part 2`), read the way a release title is. For
/// AniList titles and aliases: the auto-search's expected season and the
/// RSS matcher's series season both come from here.
pub fn season_marker(name: &str) -> Option<i32> {
    parse(name).season_hint().filter(|s| *s > 0)
}

impl ParsedTitle {
    /// A multi-episode pack: a batch keyword, a claim spanning several
    /// episodes (unless the title names one file), a season or volume
    /// pack with no episode claim, an airing span (`[1983-1984]`) with no
    /// claim, or a bracketed end marker with no claim.
    /// A movie, an extras release, or a single file is never a season pack.
    pub fn is_batch(&self) -> bool {
        if self.extra {
            return false;
        }
        if self.batch_keyword {
            return true;
        }
        if let Some(claim) = &self.episodes {
            return claim.spans_several() && !self.single_file;
        }
        if self.single_file || self.movie || self.half_episode {
            return false;
        }
        if self.volume_range || self.year_range {
            return true;
        }
        // An OVA of season 2 is one special, not a season pack; season 0
        // is the specials season, so `S00 Specials` is a pack.
        if self.special && self.seasons.numbers != [0] {
            return false;
        }
        if !self.seasons.numbers.is_empty() && (!self.seasons.hint_only || self.seasons.part_pack) {
            return true;
        }
        self.bracketed_end
    }

    /// Whether the release names no episode slot of its own: a special,
    /// an extra, or a half episode.
    pub fn names_no_slot(&self) -> bool {
        self.special || self.extra || self.half_episode
    }

    /// The episodes the title names, with no series knowledge.
    pub fn release_episodes(&self) -> HashSet<i32> {
        if self.names_no_slot() {
            return HashSet::new();
        }
        self.episodes
            .as_ref()
            .map(EpisodeClaim::episodes)
            .unwrap_or_default()
    }

    /// Whether the title names a season other than `expected`: never when
    /// it names none; a multi-season pack conflicts only when `expected`
    /// is outside it. An expected season of 0 is read as 1.
    pub fn conflicts_with_season(&self, expected: i32) -> bool {
        let expected = expected.max(1);
        !self.seasons.numbers.is_empty() && !self.seasons.numbers.contains(&expected)
    }

    /// What one file's name holds, the import's question: the episode
    /// claim (a loose range keeps its first number, a run longer than
    /// [`file_mode::MAX_FILE_SPAN`] is no file), else a bare number right
    /// after the title (a file name has no series context to doubt it),
    /// never an extra, a half episode or a bare special.
    pub fn file_span(&self) -> Option<file_mode::EpisodeSpan> {
        if self.extra || self.half_episode {
            return None;
        }
        if let Some(claim) = &self.episodes {
            if claim.opens_before_dash {
                return Some(file_mode::EpisodeSpan {
                    season: None,
                    first: claim.last,
                    last: claim.last,
                    special: self.file_special(),
                });
            }
            let first = claim.list.first().copied().unwrap_or(claim.first);
            // A loose range reads its own first number where the old file
            // branches kept it: a tag or `Episode 5-6` (no follow-set), or
            // a spaced pair (`- 05 - 06`, the dash slot's own number). A
            // glued malformed run in any other slot (`- 5-10`) names no
            // file. A run longer than one file holds is a pack.
            let keeps_first =
                matches!(claim.rule, ClaimRule::Tag | ClaimRule::EpisodeWord) || !claim.glued;
            let last = if !claim.list.is_empty() || claim.last < first {
                first
            } else if !claim.tight {
                if !keeps_first {
                    return None;
                }
                first
            } else if claim.last - first + 1 > file_mode::MAX_FILE_SPAN {
                // A tag keeps its own episode (`S01E01-02-1080`); any other
                // over-long run names no file.
                if claim.rule != ClaimRule::Tag {
                    return None;
                }
                first
            } else {
                claim.last
            };
            // Only a tag names a file's season: the import and the scanner
            // read `S3 - 01` as a relative episode with no season.
            let season = claim.season.filter(|_| claim.rule == ClaimRule::Tag);
            return Some(file_mode::EpisodeSpan {
                season,
                first,
                last,
                special: self.file_special(),
            });
        }
        // `Chihayafuru 2 - OVA - ...`: a bare special names no episode,
        // and the title's own number is not one.
        if self.special {
            return None;
        }
        self.ambiguous_number.map(|n| file_mode::EpisodeSpan {
            season: None,
            first: n,
            last: n,
            special: self.file_special(),
        })
    }

    /// What a series' names say about a file name's bare reading:
    /// `None` when they say nothing (no name found, the episode came from
    /// a marked claim such as a dash or a tag, or nothing readable follows
    /// the name), `Some(Some(n))` for the number right after the name
    /// (`[EA]Kaiju_No_8_08_` is 8), `Some(None)` when the number is the
    /// name's own (`Mob Psycho 100 (BD)`, a bare range or marker number
    /// written only inside the name).
    pub fn name_reading(&self, anchors: &SeriesAnchors) -> Option<Option<i32>> {
        if anchors.is_empty() {
            return None;
        }
        match &self.episodes {
            Some(c)
                if matches!(c.rule, ClaimRule::BareRange | ClaimRule::MarkerNumber)
                    && anchors.name_holds_numbers(&self.anchor_words, &[c.first, c.last]) =>
            {
                Some(None)
            }
            Some(_) => None,
            None => {
                if let Some(n) = anchors.episode_after_name(&self.anchor_words) {
                    return Some(Some(n));
                }
                // `[EA]Fruits_Basket_2019_05_`, `K Project 05`: the name is
                // found but the number is not the name's; the plain reading
                // stands.
                self.ambiguous_number
                    .filter(|&n| anchors.name_holds_numbers(&self.anchor_words, &[n]))
                    .map(|_| None)
            }
        }
    }

    /// A file name's special flag: a marker that is part of the title
    /// before the first dash (`Kekkon Special - 01`) is the name.
    fn file_special(&self) -> bool {
        self.special && !self.special_from_title_word
    }

    /// The single season the title names, if exactly one.
    pub fn season_hint(&self) -> Option<i32> {
        match self.seasons.numbers.as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }

    /// Whether the release is a special for a known series: a trailing
    /// `Special` / `OVA` that belongs to the series' own name is not one.
    pub fn is_special_for(&self, anchors: &SeriesAnchors) -> bool {
        self.special
            && !(self.special_from_title_word && anchors.name_ends_in_marker(&self.anchor_words))
    }

    /// The episodes for a known series: the title's own claim (unless a
    /// bare range or marker number belongs to the name), else a bare
    /// number right after the longest alias found word for word. A
    /// special names no episode of a TV series; for any other format
    /// its number is the episode.
    pub fn for_series(&self, anchors: &SeriesAnchors) -> SeriesReading {
        let special = self.is_special_for(anchors) && !anchors.specials_are_episodes();
        if self.extra || self.half_episode || special {
            return SeriesReading {
                episodes: HashSet::new(),
                via: ReadingSource::None,
            };
        }
        // A bare range or marker number in the title text can be the
        // series' own (`Tatsuki Fujimoto 17-26`); the name vetoes it.
        if let Some(claim) = &self.episodes
            && !(matches!(claim.rule, ClaimRule::BareRange | ClaimRule::MarkerNumber)
                && anchors.name_holds_numbers(&self.anchor_words, &[claim.first, claim.last]))
        {
            return SeriesReading {
                episodes: claim.episodes(),
                via: ReadingSource::Structural,
            };
        }
        match anchors.episode_after_name(&self.anchor_words) {
            Some(ep) => SeriesReading {
                episodes: HashSet::from([ep]),
                via: ReadingSource::AfterName,
            },
            None => SeriesReading {
                episodes: HashSet::new(),
                via: ReadingSource::None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{roman_value, season_marker};

    #[test]
    fn a_series_name_advertises_its_season() {
        assert_eq!(season_marker("Kusuriya no Hitorigoto 2nd Season"), Some(2));
        assert_eq!(season_marker("Shingeki no Kyojin Season 3 Part 2"), Some(3));
        assert_eq!(season_marker("Made in Abyss III"), Some(3));
        assert_eq!(season_marker("Mob Psycho 100 II"), Some(2));
        assert_eq!(season_marker("[Oshi no Ko] 2nd Season"), Some(2));
        assert_eq!(season_marker("86: Eighty Six Part 2"), Some(2));
        assert_eq!(season_marker("青之箱 第二季"), Some(2));
        // A title's own number, a lone Part 1 and no marker name no season.
        assert_eq!(season_marker("Chihayafuru 2"), None);
        assert_eq!(season_marker("Steins;Gate 0"), None);
        assert_eq!(season_marker("Kimetsu no Yaiba: Yuukaku-hen"), None);
        assert_eq!(season_marker("Lupin III: Part 6"), Some(6));
    }

    #[test]
    fn release_fields_read_group_and_resolution() {
        let f = |t: &str| super::release_fields(t);
        assert_eq!(f("[SubsPlease] Show - 01").group, "SubsPlease");
        assert_eq!(f("Show - 01").group, "");
        assert_eq!(f("[unclosed group rest").group, "");
        assert_eq!(f("[漢字組] Show").group, "漢字組");
        assert_eq!(f("[] Show").group, "");
        assert_eq!(
            f("Show - 01 1080p H.264-VARYG (Dual-Audio, Multi-Subs)").group,
            "VARYG"
        );
        assert_eq!(
            f("[Group] Show 1080p HEVC + 720p AVC dual-track").resolution,
            "1080"
        );
        assert_eq!(f("[Group] Show 1080P").resolution, "1080");
        assert_eq!(f("[Group] Show - 01").resolution, "");
        assert_eq!(f("[Group] Show 10-bit 1080 BluRay").resolution, "1080");
        assert_eq!(f("[Group] Show - 01 (1920x1080p)").resolution, "1080");
        assert!(f("[Group] Show - 01-12 (1080p)").is_batch);
    }

    #[test]
    fn a_huge_season_range_is_no_range() {
        // An unbounded range end used to expand into a vector of its size:
        // nine digits is gigabytes, an abort on every RSS sync.
        let start = std::time::Instant::now();
        for title in [
            "Season 1-999999999",
            "S1-999999999",
            "Season 1 - Season 999999999",
        ] {
            assert!(super::parse(title).seasons.numbers.len() <= 2, "{title}");
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(
            super::parse("[Group] Show Seasons 1-3 (BD)")
                .seasons
                .numbers,
            [1, 2, 3]
        );
    }

    #[test]
    fn a_padded_season_or_title_number_opens_no_range() {
        let claim = |t: &str| super::parse(t).episodes.map(|c| (c.first, c.last));
        assert_eq!(claim("[Group] Show Season 02 - 05 [1080p]"), Some((5, 5)));
        assert_eq!(claim("[Group] Cyborg 009 - 12 [DVD]"), Some((12, 12)));
        assert_eq!(
            claim("[HorribleSubs] Some Anime Show 01 - 119 [1080p] [Batch]"),
            Some((1, 119))
        );
    }

    #[test]
    fn a_part_after_a_roman_numeral_is_the_season() {
        assert_eq!(
            super::parse("[SubsPlease] Lupin III - Part 6 - 05 (1080p)").season_hint(),
            Some(6)
        );
    }

    #[test]
    fn extras_listed_after_a_plus_keep_the_pack() {
        for title in [
            "[Group] Show S01 [BD 1080p] + NCOP/NCED",
            "[Group] Show S01 [BD 1080p] + NCOP, NCED",
        ] {
            let p = super::parse(title);
            assert!(!p.extra && p.is_batch(), "{title}");
        }
    }

    #[test]
    fn every_bracket_v_tail_is_a_revision() {
        assert_eq!(
            super::parse("[Group]Vinland Saga - 05 [1080p]v2")
                .revision
                .version,
            2
        );
    }

    #[test]
    fn a_marker_ending_an_episode_title_is_no_special() {
        assert!(!super::parse("[Group] Show - 05 - Something Special [1080p]").special);
        assert!(super::parse("[Group] Show - Christmas Special [1080p]").special);
        assert!(super::parse("[Group] Kekkon Special - 01 [1080p]").special);
    }

    #[test]
    fn roman_numerals_decode_in_any_case() {
        assert_eq!(roman_value("iv"), Some(4));
        assert_eq!(roman_value("X"), Some(10));
        assert_eq!(roman_value("I"), Some(1));
        assert_eq!(roman_value("XI"), None);
        assert_eq!(roman_value("vix"), None);
    }
}
