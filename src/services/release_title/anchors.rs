//! Series-aware reading of a bare number after the title.
//!
//! A bare number right after a title can be the title's own (`Mob Psycho
//! 100`, `Steins;Gate 0`) or the episode (`[EA]Dark_Gathering_24_`). The
//! title alone cannot say which, so `parse` records it as ambiguous and
//! this module settles it with the series' aliases: the longest alias
//! found word for word in the title is the anchor, and only a number
//! right after it is read. A number inside the alias stays the title's.
//! Two to four digits, as in Sonarr's bare absolute patterns (one digit
//! after a name is a sequel number); a year, or a number followed by a
//! lone `.5` / `_5`, is not an episode.

use super::lexer::{Kind, lex};
use super::normalize::normalize;
use super::vocab::{Class, classify};

/// One word of the title text the anchors are matched against.
#[derive(Debug, Clone)]
pub(super) struct Word {
    pub text: String,
    pub num: Option<(u32, u8)>,
    /// A season marker (`S2`, `II`, `4th`), skipped between the name and
    /// the episode.
    pub season_marker: bool,
}

/// Aliases of one series, normalized once and reused for every title.
#[derive(Debug, Clone, Default)]
pub struct SeriesAnchors {
    seqs: Vec<Vec<String>>,
    /// The series is not a TV entry (OVA, ONA, special, movie): a special
    /// marker is part of its name, so `OVA 01` is its episode 1.
    specials_are_episodes: bool,
}

impl SeriesAnchors {
    pub fn new<S: AsRef<str>>(aliases: &[S]) -> Self {
        let mut seqs: Vec<Vec<String>> = Vec::new();
        for alias in aliases {
            let words: Vec<String> = alias_words(alias.as_ref());
            if words.is_empty() {
                continue;
            }
            // `... 4th Season`: EternalAnimes drops the word Season.
            if words.len() > 2
                && words.last().is_some_and(|w| w == "season")
                && matches!(
                    classify_text(&words[words.len() - 2]),
                    Class::Ordinal(n) if n > 0
                )
            {
                seqs.push(words[..words.len() - 1].to_vec());
            }
            seqs.push(words);
        }
        seqs.sort();
        seqs.dedup();
        Self {
            seqs,
            specials_are_episodes: false,
        }
    }

    /// For a series whose format is not TV (`!media::is_tv_format`): its
    /// specials are its episodes.
    pub fn with_specials_as_episodes(mut self, yes: bool) -> Self {
        self.specials_are_episodes = yes;
        self
    }

    pub(super) fn specials_are_episodes(&self) -> bool {
        self.specials_are_episodes
    }

    pub fn is_empty(&self) -> bool {
        self.seqs.is_empty()
    }

    /// Whether an alias found in `words` ends in a special marker word
    /// (`A Returner's Magic Should Be Special`), so a trailing `Special` in
    /// the title is the name, not a special.
    pub(super) fn name_ends_in_marker(&self, words: &[Word]) -> bool {
        self.seqs.iter().any(|seq| {
            seq.last().is_some_and(|w| {
                matches!(w.as_str(), "special" | "specials" | "ova" | "oad" | "ovas")
            }) && words
                .windows(seq.len())
                .any(|win| win.iter().zip(seq).all(|(a, b)| &a.text == b))
        })
    }

    /// Whether any alias is found word for word in `words`.
    pub(super) fn finds_name(&self, words: &[Word]) -> bool {
        self.seqs.iter().any(|seq| {
            seq.len() <= words.len()
                && words
                    .windows(seq.len())
                    .any(|win| win.iter().zip(seq).all(|(a, b)| &a.text == b))
        })
    }

    /// Whether an alias found in `words` holds every number in `nums`
    /// (`Tatsuki Fujimoto 17-26`), so those numbers are the name's.
    pub(super) fn name_holds_numbers(&self, words: &[Word], nums: &[i32]) -> bool {
        self.seqs.iter().any(|seq| {
            nums.iter().all(|n| seq.contains(&n.to_string()))
                && words
                    .windows(seq.len())
                    .any(|win| win.iter().zip(seq).all(|(a, b)| &a.text == b))
        })
    }

    /// The episode right after the longest alias found in `words`.
    pub(super) fn episode_after_name(&self, words: &[Word]) -> Option<i32> {
        let mut best: Option<(usize, usize)> = None; // (alias length, index after it)
        for seq in &self.seqs {
            let len = seq.len();
            if len > words.len() || best.is_some_and(|(b, _)| b >= len) {
                continue;
            }
            if let Some(start) = words
                .windows(len)
                .position(|w| w.iter().zip(seq).all(|(a, b)| &a.text == b))
            {
                best = Some((len, start + len));
            }
        }
        let (_, mut at) = best?;
        if words.get(at).is_some_and(|w| w.season_marker) {
            at += 1;
        }
        let word = words.get(at)?;
        let (value, digits) = word.num?;
        if !(2..=4).contains(&digits) {
            return None;
        }
        if digits == 4 && (1900..=2100).contains(&value) {
            return None;
        }
        // `07_5` (a half episode) or `12 13` (two bare numbers: ambiguous).
        if words.get(at + 1).is_some_and(|next| next.num.is_some()) {
            return None;
        }
        Some(value as i32)
    }
}

fn classify_text(text: &str) -> Class {
    lex(text).toks.first().map(classify).unwrap_or(Class::Plain)
}

/// Normalized words of an alias, lowercased, with compounds split on
/// `-`. Round-bracket parts (`(2023)`) are dropped, as release titles drop
/// them; square-bracket text is part of the name (`[Oshi no Ko]`).
fn alias_words(alias: &str) -> Vec<String> {
    let norm = normalize(alias);
    let lexed = lex(&norm.text);
    let mut out = Vec::new();
    for tok in &lexed.toks {
        let in_square = lexed
            .groups
            .iter()
            .find(|g| g.id == tok.group)
            .is_some_and(|g| g.kind == '[');
        if tok.depth > 0 && !in_square {
            continue;
        }
        match tok.kind {
            Kind::Word => out.extend(
                tok.lower
                    .split('-')
                    .filter(|p| !p.is_empty())
                    .map(String::from),
            ),
            Kind::Num { value, .. } => out.push(value.to_string()),
            Kind::Decimal { .. } => out.push(tok.lower.clone()),
            // As in `title_words`, so `Tatsuki Fujimoto 17-26` matches.
            Kind::RangeOp => out.push("-".into()),
            _ => {}
        }
    }
    out
}

/// Builds the title words from depth-0 tokens (skipping the leading group).
pub(super) fn title_words(toks: &[super::lexer::Tok], skip: &[bool], class: &[Class]) -> Vec<Word> {
    let mut out = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        if skip[i] || tok.depth > 0 {
            continue;
        }
        match tok.kind {
            Kind::Word => {
                let marker = matches!(
                    class[i],
                    Class::SeasonShort(_) | Class::Roman(_) | Class::Ordinal(1..)
                );
                for part in tok.lower.split('-').filter(|p| !p.is_empty()) {
                    out.push(Word {
                        text: part.to_string(),
                        num: None,
                        season_marker: marker,
                    });
                }
            }
            Kind::Num { value, digits, .. } => out.push(Word {
                text: value.to_string(),
                num: Some((value, digits)),
                season_marker: false,
            }),
            Kind::Decimal { .. } => out.push(Word {
                text: tok.lower.clone(),
                num: None,
                season_marker: false,
            }),
            // Kept so `S01-04` never reads as a marker then episode 4.
            Kind::RangeOp => out.push(Word {
                text: "-".into(),
                num: None,
                season_marker: false,
            }),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::services::release_title::{SeriesAnchors, parse};

    fn read(title: &str, aliases: &[&str]) -> Vec<i32> {
        let mut eps: Vec<i32> = parse(title)
            .for_series(&SeriesAnchors::new(aliases))
            .episodes
            .into_iter()
            .collect();
        eps.sort_unstable();
        eps
    }

    #[test]
    fn a_bare_number_after_the_name_is_the_episode() {
        assert_eq!(
            read(
                "[EA]Dark_Gathering_24_[1920x1080][HEVC][831E9094].mkv",
                &["Dark Gathering"]
            ),
            [24]
        );
        assert_eq!(
            read(
                "[Some-Stuffs] Pocket Monsters (2023) 154 (1080p)",
                &["Pocket Monsters (2023)"]
            ),
            [154]
        );
    }

    #[test]
    fn a_number_inside_the_name_is_not() {
        assert!(read("[Group] Mob Psycho 100 (BD 1080p)", &["Mob Psycho 100"]).is_empty());
        assert!(read("[EA]Mob_Psycho_100_[BD]", &["Mob Psycho", "Mob Psycho 100"]).is_empty());
        assert_eq!(
            read(
                "[EA]Mob_Psycho_100_05_[BD]",
                &["Mob Psycho", "Mob Psycho 100"]
            ),
            [5]
        );
        assert!(read("[EA]Kaiju_No_8_[1080p]", &["Kaiju No. 8"]).is_empty());
    }

    #[test]
    fn a_range_inside_the_name_is_not_episodes() {
        assert!(
            read(
                "[Mira] Tatsuki Fujimoto 17-26 - Season 1 - (BD 1080p Opus AV1)",
                &["Tatsuki Fujimoto 17-26"]
            )
            .is_empty()
        );
        assert_eq!(
            read(
                "[EA]Dark_Gathering_01-25_End_[1920x1080]",
                &["Dark Gathering"]
            )
            .len(),
            25
        );
    }

    #[test]
    fn a_special_is_an_episode_only_when_the_series_is_not_tv() {
        let tv = SeriesAnchors::new(&["Hellsing Ultimate"]);
        let ova = SeriesAnchors::new(&["Hellsing Ultimate"]).with_specials_as_episodes(true);
        let parsed = parse("[Group] Hellsing Ultimate OVA 03 (BD 1080p)");
        assert!(parsed.for_series(&tv).episodes.is_empty());
        assert_eq!(
            parsed
                .for_series(&ova)
                .episodes
                .into_iter()
                .collect::<Vec<_>>(),
            [3]
        );
    }

    #[test]
    fn a_bracketed_name_is_still_the_name() {
        assert_eq!(read("[EA]Oshi_no_Ko_05_[1080p]", &["[Oshi no Ko]"]), [5]);
        assert_eq!(
            read(
                "[Some-Stuffs] Pocket Monsters (2023) 154 (1080p)",
                &["Pocket Monsters (2023)"]
            ),
            [154]
        );
    }

    #[test]
    fn nothing_without_a_matching_name() {
        assert!(read("[Group] Dark Gatherings 05 (1080p)", &["Dark Gathering"]).is_empty());
        assert!(read("[EA]Dark_Gathering_05_", &[]).is_empty());
        assert!(read("[Group]_Dark_Gathering_2023_[1080p]", &["Dark Gathering"]).is_empty());
    }
}
