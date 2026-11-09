//! How a series' release numbers map onto its own episodes (#303).
//!
//! AniList numbers every entry from 1. Release groups often don't: a
//! sequel cour is `Jujutsu Kaisen - 56`, and an entry AniList split out of
//! one MAL entry keeps the MAL entry's numbering (`Steel Ball Run - 07` is
//! 2nd - 3rd STAGE's episode 6, because 1st STAGE was episode 1). The
//! series row carries the offset (`cumulative_prior_episodes`, written
//! only through `anime_relations::cumulative_prior_episodes`), where it
//! came from (`cumulative_offset_source`) and the user's choice
//! (`episode_numbering`). [`Numbering`] is the three resolved into the one
//! rule every reader of a release title or download file name applies:
//! the search gate, scoring, the episode-set pick, the grab commit, the
//! picker, the import and RSS.
//!
//! Library files are never read through it: Ryokan names them in the
//! series' own numbering (`naming::validate` parses every template back),
//! so the scanner and the import's on-disk lookups stay relative.

use std::collections::HashSet;

/// Where `series.cumulative_prior_episodes` came from, as
/// `series.cumulative_offset_source` stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OffsetSource {
    /// Written before the source was recorded, or never written.
    #[default]
    Unknown,
    /// A curated anime-relations rule.
    Rule,
    /// Entries AniList split out of the same MAL entry before this one
    /// (`local_metadata::split_prior_episodes`). Releases follow the MAL
    /// entry, so these read absolute only.
    Split,
    /// The TV PREQUEL walk (`local_metadata::compute_cumulative_prior_episodes`).
    Walk,
}

impl OffsetSource {
    pub fn as_str(self) -> &'static str {
        match self {
            OffsetSource::Unknown => "",
            OffsetSource::Rule => "rule",
            OffsetSource::Split => "split",
            OffsetSource::Walk => "walk",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value.trim() {
            "rule" => OffsetSource::Rule,
            "split" => OffsetSource::Split,
            "walk" => OffsetSource::Walk,
            _ => OffsetSource::Unknown,
        }
    }
}

/// The user's choice on the series page, as `series.episode_numbering`
/// stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumberingSetting {
    /// Decided from the offset's source (see [`Numbering::resolve`]).
    #[default]
    Automatic,
    /// Releases count from 1 the way AniList numbers the entry.
    AniList,
    /// Releases count on from the entries before this one.
    Absolute,
}

impl NumberingSetting {
    pub fn as_str(self) -> &'static str {
        match self {
            NumberingSetting::Automatic => "",
            NumberingSetting::AniList => "anilist",
            NumberingSetting::Absolute => "absolute",
        }
    }

    /// Unknown values read as automatic, the way the form coerces them
    /// on save.
    pub fn from_str(value: &str) -> Self {
        match value.trim() {
            "anilist" => NumberingSetting::AniList,
            "absolute" => NumberingSetting::Absolute,
            _ => NumberingSetting::Automatic,
        }
    }
}

/// Which numbers of a release stand for an episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The episode's own number or that number plus the offset (#30):
    /// a TV sequel's releases come both ways.
    Either,
    /// The episode's own number only.
    Relative,
    /// The number plus the offset only: a number at or below the offset
    /// is an earlier entry's.
    Absolute,
}

/// A series' offset and reading, resolved once from its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Numbering {
    pub offset: i32,
    pub reading: Reading,
    /// The TVDB season a split offset was confirmed in
    /// (`series.cumulative_offset_season`): under [`Reading::Absolute`] a
    /// tag naming it (`S06E07` for Steel Ball Run 2nd - 3rd STAGE, TVDB
    /// season 6 with 1st STAGE as `S06E01`) counts on from the earlier
    /// parts the way a bare number does. Every other tag counts within
    /// its own season.
    pub tag_season: Option<i32>,
}

impl Default for Numbering {
    fn default() -> Self {
        Numbering::relative()
    }
}

/// A bare offset reads the way Ryokan read one before #303: either
/// numbering. Callers with a series row use [`crate::models::series::Series::numbering`].
impl From<i32> for Numbering {
    fn from(offset: i32) -> Self {
        Numbering::either(offset)
    }
}

/// The series page's line under the Episode numbering select: what
/// "Automatic" means for this series, and what a choice does when there
/// is nothing to count on from.
pub fn setting_hint(offset: i32, source: OffsetSource, setting: NumberingSetting) -> String {
    let offset = offset.max(0);
    let first_absolute = offset.saturating_add(1);
    if offset == 0 {
        return "No earlier episodes are known for this series, so releases are read from episode 1 whichever numbering is picked."
            .to_string();
    }
    match setting {
        NumberingSetting::Absolute => format!(
            "Releases count on from the earlier entries: episode 1 is numbered {first_absolute}, and a number up to {offset} belongs to an earlier entry."
        ),
        NumberingSetting::AniList => {
            "Releases number this series from 1, the way AniList does.".to_string()
        }
        NumberingSetting::Automatic if source == OffsetSource::Split => format!(
            "Automatic: this series continues an entry it shares with earlier parts on MyAnimeList and TVDB, so episode 1 is numbered {first_absolute}. Pick AniList if its release groups number it from 1."
        ),
        NumberingSetting::Automatic => format!(
            "Automatic: a release may number episode 1 as 1 or as {first_absolute}, counting the earlier seasons. Pick one when the release groups for this series agree on it."
        ),
    }
}

/// The season of the title's episode claim when it is a real season tag
/// (`S01E07`, `1x07`), the claim [`Numbering::for_tag`] reads within its
/// season. A season marker before a bare number (`S2 - 05`) is not one:
/// the import has always read those as bare numbers.
pub fn season_tag(parsed: &crate::services::release_title::ParsedTitle) -> Option<i32> {
    parsed
        .episodes
        .as_ref()
        .filter(|claim| claim.rule == crate::services::release_title::ClaimRule::Tag)
        .and_then(|claim| claim.season)
}

impl Numbering {
    /// No offset: every number is the series' own.
    pub const fn relative() -> Self {
        Numbering {
            offset: 0,
            reading: Reading::Relative,
            tag_season: None,
        }
    }

    /// Today's reading for an offset with no source to go by: either
    /// numbering when there is an offset.
    pub fn either(offset: i32) -> Self {
        Numbering::resolve(offset, OffsetSource::Unknown, NumberingSetting::Automatic)
    }

    /// The rule for a series with offset `offset` from `source` and the
    /// user's `setting`. An offset of 0 (or below) is always relative:
    /// there is nothing to count on from.
    pub fn resolve(offset: i32, source: OffsetSource, setting: NumberingSetting) -> Self {
        let offset = offset.max(0);
        if offset == 0 {
            return Numbering::relative();
        }
        let reading = match setting {
            NumberingSetting::AniList => Reading::Relative,
            NumberingSetting::Absolute => Reading::Absolute,
            NumberingSetting::Automatic if source == OffsetSource::Split => Reading::Absolute,
            NumberingSetting::Automatic => Reading::Either,
        };
        Numbering {
            offset,
            reading,
            tag_season: None,
        }
    }

    /// The same rule with tags of TVDB season `season` read in the
    /// numbering the offset counts in (see [`Numbering::tag_season`]).
    pub fn with_tag_season(self, season: Option<i32>) -> Self {
        Numbering {
            tag_season: season.filter(|s| *s > 0),
            ..self
        }
    }

    /// The offset absolute numbers carry, 0 when none are read (no
    /// offset, or AniList numbering chosen).
    pub fn absolute_offset(&self) -> i32 {
        match self.reading {
            Reading::Relative => 0,
            Reading::Either | Reading::Absolute => self.offset,
        }
    }

    /// The reading for the numbers of a release whose claim is a real
    /// season tag of season `tag_season` (`S01E07`), `None` for every
    /// other claim. A tag counts within its season, so under
    /// [`Reading::Absolute`] it reads relative, as the import and the grab
    /// commit have always read tags; the one exception is the TVDB season
    /// the split was confirmed in ([`Numbering::tag_season`]).
    pub fn for_tag(self, tag_season: Option<i32>) -> Self {
        match tag_season {
            Some(season)
                if self.reading == Reading::Absolute && self.tag_season != Some(season) =>
            {
                Numbering {
                    reading: Reading::Relative,
                    ..self
                }
            }
            _ => self,
        }
    }

    /// True when the release numbers `parsed` hold episode `episode`.
    /// Under [`Reading::Absolute`] every number has to be past the
    /// offset: a run reaching back into an earlier part (`- 01-02` for
    /// Steel Ball Run 2nd - 3rd STAGE) is no release of this one, which is
    /// how the import and RSS read it too.
    pub fn accepts(&self, parsed: &HashSet<i32>, episode: i32) -> bool {
        let relative = || parsed.contains(&episode);
        let absolute = || self.offset > 0 && parsed.contains(&episode.saturating_add(self.offset));
        match self.reading {
            Reading::Relative => relative(),
            Reading::Absolute => absolute() && parsed.iter().all(|n| *n > self.offset),
            Reading::Either => relative() || absolute(),
        }
    }

    /// [`Numbering::accepts`] for a pack. The import reads a pack's
    /// files one at a time and skips an earlier part's (`01` in Steel
    /// Ball Run's `01-12`), so a run reaching back past the offset still
    /// holds this part's episodes; only a single file is refused for it.
    pub fn pack_accepts(&self, parsed: &HashSet<i32>, episode: i32) -> bool {
        match self.reading {
            Reading::Absolute => {
                self.offset > 0 && parsed.contains(&episode.saturating_add(self.offset))
            }
            Reading::Either | Reading::Relative => self.accepts(parsed, episode),
        }
    }

    /// True when a release whose seasons `parsed` reads may be this
    /// series under the season gate: its own `expected` season, or the
    /// TVDB season its split counts in (`S06E07` for Steel Ball Run 2nd -
    /// 3rd STAGE, whose own name names no season).
    pub fn season_allowed(
        &self,
        parsed: &crate::services::release_title::ParsedTitle,
        expected: i32,
    ) -> bool {
        !parsed.conflicts_with_season(expected)
            || (self.reading == Reading::Absolute
                && self
                    .tag_season
                    .is_some_and(|season| !parsed.conflicts_with_season(season)))
    }

    /// The offset a file name's span (`season` set only for a real tag)
    /// starting at `first` is read with: a tag counts within its season
    /// (offset 0) unless it names [`Numbering::tag_season`] under
    /// [`Reading::Absolute`]; a bare number per [`Numbering::run_offset`].
    pub fn file_offset(&self, season: Option<i32>, first: i32) -> i32 {
        match season {
            Some(s) if self.reading == Reading::Absolute && self.tag_season == Some(s) => {
                self.offset
            }
            Some(_) => 0,
            None => self.run_offset(first),
        }
    }

    /// The number a search query names for `episode`: the absolute one
    /// when only that reading counts, else the episode's own (the
    /// franchise pass queries the absolute number under
    /// [`Reading::Either`]).
    pub fn query_episode(&self, episode: i32) -> i32 {
        match self.reading {
            Reading::Absolute => episode.saturating_add(self.offset),
            Reading::Either | Reading::Relative => episode,
        }
    }

    /// The offset a bare run starting at `first` is read with when
    /// nothing else says which numbering it uses: under
    /// [`Reading::Either`] a run past the offset is absolute (#30's
    /// `fallback_ep_offset`), under the other two the reading decides.
    /// The caller drops a number that comes out at or below 0.
    pub fn run_offset(&self, first: i32) -> i32 {
        match self.reading {
            Reading::Relative => 0,
            Reading::Absolute => self.offset,
            Reading::Either if self.offset > 0 && first > self.offset => self.offset,
            Reading::Either => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn set(nums: &[i32]) -> HashSet<i32> {
        nums.iter().copied().collect()
    }

    #[rstest]
    #[case::no_offset_is_relative(
        0,
        OffsetSource::Split,
        NumberingSetting::Automatic,
        Reading::Relative
    )]
    #[case::split_reads_absolute(
        1,
        OffsetSource::Split,
        NumberingSetting::Automatic,
        Reading::Absolute
    )]
    #[case::walk_reads_either(47, OffsetSource::Walk, NumberingSetting::Automatic, Reading::Either)]
    #[case::rule_reads_either(12, OffsetSource::Rule, NumberingSetting::Automatic, Reading::Either)]
    #[case::unknown_reads_either(
        47,
        OffsetSource::Unknown,
        NumberingSetting::Automatic,
        Reading::Either
    )]
    #[case::anilist_wins(1, OffsetSource::Split, NumberingSetting::AniList, Reading::Relative)]
    #[case::absolute_wins(47, OffsetSource::Walk, NumberingSetting::Absolute, Reading::Absolute)]
    #[case::absolute_without_offset(
        0,
        OffsetSource::Walk,
        NumberingSetting::Absolute,
        Reading::Relative
    )]
    fn resolve_picks_the_reading(
        #[case] offset: i32,
        #[case] source: OffsetSource,
        #[case] setting: NumberingSetting,
        #[case] reading: Reading,
    ) {
        assert_eq!(Numbering::resolve(offset, source, setting).reading, reading);
    }

    #[test]
    fn negative_offsets_read_as_none() {
        assert_eq!(
            Numbering::resolve(-3, OffsetSource::Walk, NumberingSetting::Absolute),
            Numbering::relative()
        );
    }

    #[test]
    fn steel_ball_run_reads_one_number_per_episode() {
        // 2nd - 3rd STAGE: offset 1 from the split, absolute only.
        let n = Numbering::resolve(1, OffsetSource::Split, NumberingSetting::Automatic);
        assert!(n.accepts(&set(&[7]), 6), "- 07 is episode 6");
        assert!(!n.accepts(&set(&[6]), 6), "- 06 is episode 5");
        assert!(!n.accepts(&set(&[1]), 1), "- 01 is 1st STAGE's");
        assert!(n.accepts(&set(&[2]), 1));
        assert_eq!(n.query_episode(6), 7);
        assert_eq!(n.run_offset(1), 1, "- 01 comes out as episode 0, no slot");
        assert_eq!(n.run_offset(7), 1);
    }

    #[test]
    fn either_keeps_the_thirty_rule() {
        let n = Numbering::either(47);
        assert!(n.accepts(&set(&[5]), 5));
        assert!(n.accepts(&set(&[52]), 5));
        assert!(!n.accepts(&set(&[6]), 5));
        assert_eq!(n.query_episode(5), 5);
        assert_eq!(n.run_offset(47), 0, "at the offset stays relative");
        assert_eq!(n.run_offset(48), 47);
    }

    #[test]
    fn relative_ignores_the_offset() {
        let n = Numbering::resolve(47, OffsetSource::Walk, NumberingSetting::AniList);
        assert!(n.accepts(&set(&[5]), 5));
        assert!(!n.accepts(&set(&[52]), 5));
        assert_eq!(n.absolute_offset(), 0);
        assert_eq!(n.run_offset(52), 0);
    }

    #[test]
    fn a_tag_stays_relative_under_absolute_unless_it_names_the_split_season() {
        let n = Numbering::resolve(1, OffsetSource::Split, NumberingSetting::Automatic)
            .with_tag_season(Some(6));
        assert_eq!(n.for_tag(Some(1)).reading, Reading::Relative);
        assert_eq!(n.for_tag(Some(6)), n, "S06E07 counts on from 1st STAGE");
        assert_eq!(n.for_tag(None), n);
        assert!(n.for_tag(Some(6)).accepts(&set(&[7]), 6));
        assert_eq!(n.file_offset(Some(6), 7), 1);
        assert_eq!(n.file_offset(Some(1), 7), 0);
        assert_eq!(n.file_offset(None, 7), 1);
        let either = Numbering::either(47).with_tag_season(Some(3));
        assert_eq!(
            either.for_tag(Some(3)),
            either,
            "today's tag reading is unchanged"
        );
        assert_eq!(either.file_offset(Some(3), 52), 0);
        assert_eq!(either.file_offset(None, 52), 47);
    }

    #[test]
    fn an_absolute_run_never_reaches_into_the_earlier_part() {
        let n = Numbering::resolve(1, OffsetSource::Split, NumberingSetting::Automatic);
        assert!(
            !n.accepts(&set(&[1, 2]), 1),
            "- 01-02 holds 1st STAGE's episode"
        );
        assert!(n.accepts(&set(&[2, 3]), 1));
        // Either never had the rule.
        assert!(Numbering::either(1).accepts(&set(&[1, 2]), 1));
        // A pack's files are imported one at a time, 1st STAGE's skipped.
        let pack: Vec<i32> = (1..=12).collect();
        assert!(n.pack_accepts(&set(&pack), 1));
        assert!(n.pack_accepts(&set(&pack), 11));
        assert!(!n.pack_accepts(&set(&pack), 12));
        assert!(Numbering::either(1).pack_accepts(&set(&pack), 12));
    }

    #[test]
    fn the_hint_never_offers_a_choice_that_changes_nothing() {
        let zero = setting_hint(0, OffsetSource::Walk, NumberingSetting::Absolute);
        assert!(zero.contains("whichever numbering is picked"), "{zero}");
        assert!(!zero.contains("Pick"), "{zero}");
        let split = setting_hint(1, OffsetSource::Split, NumberingSetting::Automatic);
        assert!(split.contains("episode 1 is numbered 2"), "{split}");
        for hint in [
            zero,
            split,
            setting_hint(47, OffsetSource::Walk, NumberingSetting::Automatic),
            setting_hint(47, OffsetSource::Walk, NumberingSetting::AniList),
            setting_hint(47, OffsetSource::Walk, NumberingSetting::Absolute),
        ] {
            assert!(!hint.contains('\u{2014}'), "no em dash: {hint}");
        }
    }

    #[test]
    fn stored_strings_round_trip() {
        for s in [
            OffsetSource::Unknown,
            OffsetSource::Rule,
            OffsetSource::Split,
            OffsetSource::Walk,
        ] {
            assert_eq!(OffsetSource::from_str(s.as_str()), s);
        }
        for s in [
            NumberingSetting::Automatic,
            NumberingSetting::AniList,
            NumberingSetting::Absolute,
        ] {
            assert_eq!(NumberingSetting::from_str(s.as_str()), s);
        }
        assert_eq!(
            NumberingSetting::from_str("tvdb"),
            NumberingSetting::Automatic
        );
    }
}
