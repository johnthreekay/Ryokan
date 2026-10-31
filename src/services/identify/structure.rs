//! What a title claims (season, part, episodes, year, format) and how an
//! entry fits each claim: `Yes`, `No`, or `Unknown` when the entry's data
//! can't say. Only claims the title makes are judged.

use crate::services::release_title::ParsedTitle;

use super::graph::EntryCard;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Yes,
    No,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Claims {
    /// One explicit season (never season 0, the specials season).
    pub season: Option<i32>,
    /// Several seasons (`S01-S04`): a pack across entries.
    pub multi_season: bool,
    pub part: Option<i32>,
    /// First and last episode the title names.
    pub episodes: Option<(i32, i32)>,
    pub special: bool,
    pub movie: bool,
    pub year: Option<i32>,
}

impl Claims {
    /// The claims of a parsed title. The part is `main_part`, read from
    /// the main name: a `Part 2` in an episode's own title (`Tokyo Colony
    /// No 1-Part 2`) is no claim about the show.
    pub fn of(parsed: &ParsedTitle, main_part: Option<i32>) -> Self {
        let positive: Vec<i32> = parsed
            .seasons
            .numbers
            .iter()
            .copied()
            .filter(|s| *s > 0)
            .collect();
        let hint_only = parsed.seasons.hint_only;
        let season = match positive.as_slice() {
            [one] if !hint_only => Some(*one),
            _ => None,
        };
        let part = main_part.filter(|p| *p > 0);
        let season_zero = parsed.seasons.numbers == [0];
        let episodes = if parsed.extra || parsed.half_episode {
            None
        } else {
            parsed.episodes.as_ref().map(|c| {
                let first = c.list.iter().copied().min().unwrap_or(c.first);
                let last = c.list.iter().copied().max().unwrap_or(c.last).max(first);
                (first, last)
            })
        };
        Self {
            season,
            multi_season: positive.len() > 1,
            part,
            episodes,
            special: parsed.special || season_zero,
            movie: parsed.movie,
            year: parsed.year,
        }
    }

    /// Whether the title states this marker as its season or part.
    pub fn states(&self, marker: i32) -> bool {
        self.season == Some(marker) || self.part == Some(marker)
    }
}

/// One entry's fit to each claim; `None` where the title makes no claim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Fits {
    pub season: Option<Fit>,
    pub part: Option<Fit>,
    pub episode: Option<Fit>,
    pub year: Option<Fit>,
    pub format: Option<Fit>,
}

impl Fits {
    fn all(&self) -> [(&'static str, Option<Fit>); 5] {
        [
            ("season", self.season),
            ("part", self.part),
            ("episode", self.episode),
            ("year", self.year),
            ("format", self.format),
        ]
    }

    /// Some claim is `No`.
    pub fn incompatible(&self) -> bool {
        self.first_no().is_some()
    }

    /// Every claim the title makes is `Yes`.
    pub fn confirmed(&self) -> bool {
        self.all()
            .iter()
            .all(|(_, f)| f.is_none_or(|f| f == Fit::Yes))
    }

    /// The first claim the entry fails, for reasons.
    pub fn first_no(&self) -> Option<&'static str> {
        self.all()
            .iter()
            .find(|(_, f)| *f == Some(Fit::No))
            .map(|(name, _)| *name)
    }
}

pub(super) fn fit(entry: &EntryCard, claims: &Claims) -> Fits {
    Fits {
        season: claims.season.map(|s| season_fit(entry, s)),
        part: claims.part.map(|p| part_fit(entry, p)),
        episode: claims.episodes.map(|(first, last)| {
            // A special's number is no episode of a TV or movie entry.
            if claims.special && !is_special_format(entry) {
                Fit::Unknown
            } else {
                episode_fit(entry, first, last)
            }
        }),
        year: claims.year.map(|y| year_fit(entry, y)),
        format: format_fit(entry, claims),
    }
}

fn is_special_format(entry: &EntryCard) -> bool {
    matches!(entry.format.as_str(), "OVA" | "ONA" | "SPECIAL")
}

fn season_fit(entry: &EntryCard, season: i32) -> Fit {
    if entry.seasons.is_empty() {
        Fit::Unknown
    } else if entry.seasons.contains(&season) {
        Fit::Yes
    } else {
        Fit::No
    }
}

fn part_fit(entry: &EntryCard, part: i32) -> Fit {
    if entry.parts.is_empty() {
        Fit::Unknown
    } else if entry.parts.contains(&part) {
        Fit::Yes
    } else {
        Fit::No
    }
}

/// Every numbering the entry's episodes may be written in, as
/// `(first, last)` with `last` unknown when the count is.
pub(super) fn episode_frames(entry: &EntryCard) -> Vec<(i32, Option<i32>)> {
    let count = entry.episodes;
    let span = |before: i32| (before + 1, count.map(|c| before + c));
    let mut frames = vec![span(0)];
    if entry.frames.absolute > 0 {
        frames.push(span(entry.frames.absolute));
    }
    for &(_, offset) in &entry.frames.tvdb {
        match offset {
            // An entry TVDB splits across seasons starts mid-entry there
            // (`62-77` is a season's `1-16`).
            Some(before) if before != 0 => {
                let (lo, hi) = span(before);
                frames.push((lo.max(1), hi));
            }
            Some(_) => {}
            // In a TVDB season at an unknown place.
            None => frames.push((1, None)),
        }
    }
    // `Kizumonogatari - 02` is the second movie.
    if entry.is_single_entry() {
        for n in entry
            .sequence
            .into_iter()
            .chain(entry.parts.iter().copied())
        {
            frames.push((n, Some(n)));
        }
    }
    frames
}

fn episode_fit(entry: &EntryCard, first: i32, last: i32) -> Fit {
    let frames = episode_frames(entry);
    if frames
        .iter()
        .any(|&(lo, hi)| hi.is_some_and(|hi| lo <= first && last <= hi))
    {
        return Fit::Yes;
    }
    if frames.iter().any(|&(lo, hi)| hi.is_none() && lo <= first) {
        return Fit::Unknown;
    }
    Fit::No
}

fn year_fit(entry: &EntryCard, year: i32) -> Fit {
    let named = [
        &entry.names.romaji,
        &entry.names.english,
        &entry.names.synonyms,
    ]
    .iter()
    .flat_map(|forms| forms.iter())
    .any(|f| f.year == Some(year));
    if named || entry.frames.tvdb_start_year == Some(year) {
        return Fit::Yes;
    }
    match entry.season_year {
        None => Fit::Unknown,
        Some(start) => {
            let end = entry.end_year.unwrap_or(start).max(start);
            if (start..=end).contains(&year) {
                Fit::Yes
            } else {
                Fit::No
            }
        }
    }
}

fn format_fit(entry: &EntryCard, claims: &Claims) -> Option<Fit> {
    if claims.movie {
        return Some(match entry.format.as_str() {
            "MOVIE" => Fit::Yes,
            "TV" | "TV_SHORT" => Fit::No,
            _ => Fit::Unknown,
        });
    }
    if claims.special {
        return Some(if is_special_format(entry) {
            Fit::Yes
        } else {
            Fit::Unknown
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::identify::graph::{FranchiseGraph, GraphNode, OffsetRow};
    use crate::services::release_title;

    fn franchise() -> crate::services::identify::Franchise {
        let node = |id, romaji: &str, format: &str, episodes, year| GraphNode {
            id,
            title_romaji: romaji.to_string(),
            format: format.to_string(),
            episodes: Some(episodes),
            season_year: Some(year),
            ..GraphNode::default()
        };
        FranchiseGraph {
            nodes: vec![
                node(1, "Show", "TV", 12, 2019),
                node(2, "Show 2nd Season", "TV", 12, 2021),
                node(3, "Show II: The Movie", "MOVIE", 1, 2022),
            ],
            offsets: vec![OffsetRow {
                id: 2,
                episodes: 12,
            }],
            ..FranchiseGraph::default()
        }
        .build(1, &[])
        .unwrap()
    }

    fn fits(id: i64, title: &str) -> Fits {
        let f = franchise();
        fit(
            f.entry(id).unwrap(),
            &Claims::of(&release_title::parse(title), None),
        )
    }

    #[test]
    fn episodes_fit_any_numbering() {
        assert_eq!(fits(2, "[G] Show - 05").episode, Some(Fit::Yes));
        assert_eq!(fits(2, "[G] Show - 17").episode, Some(Fit::Yes));
        assert_eq!(fits(2, "[G] Show - 25").episode, Some(Fit::No));
        assert_eq!(fits(1, "[G] Show - 17").episode, Some(Fit::No));
        // A movie's part number.
        assert_eq!(fits(3, "[G] Show - 02").episode, Some(Fit::Yes));
        assert_eq!(fits(3, "[G] Show - 03").episode, Some(Fit::No));
    }

    #[test]
    fn seasons_years_and_formats_are_judged_only_when_claimed() {
        let f = fits(1, "[G] Show S02E05");
        assert_eq!((f.season, f.episode), (Some(Fit::No), Some(Fit::Yes)));
        assert!(f.incompatible() && !f.confirmed());
        assert_eq!(fits(2, "[G] Show S02E05").season, Some(Fit::Yes));
        assert_eq!(fits(1, "[G] Show (2021) - 05").year, Some(Fit::No));
        assert_eq!(fits(2, "[G] Show (2021) - 05").year, Some(Fit::Yes));
        assert_eq!(fits(1, "[G] Show Movie (BD)").format, Some(Fit::No));
        assert_eq!(fits(3, "[G] Show Movie (BD)").format, Some(Fit::Yes));
        let none = fits(1, "[G] Show (BD)");
        assert!(none.confirmed() && !none.incompatible());
    }
}
