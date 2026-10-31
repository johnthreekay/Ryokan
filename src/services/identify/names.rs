//! Names as comparable token sets.
//!
//! Both sides go through [`NameForm::of`]: an entry's titles and a
//! release's name zones. Apostrophes are deleted (`JoJo's` is `jojos`),
//! diacritics folded, a name that is wholly one bracket group keeps its
//! text (`[Oshi No Ko]`), a four-digit year becomes a year marker, and
//! season / part markers, Roman sequence numbers, generic words and lone
//! letters are removed. What is left are the name's distinctive tokens:
//! Latin words and numbers, and CJK runs, which are matched by
//! containment because CJK text has no word breaks.

use crate::services::auto_search::{is_generic_title_token, sole_bracket_group};
use crate::services::release_title;

/// Words that say what kind of thing a name is, not which one; on top of
/// the auto-search's generic list.
const IDENTIFY_GENERIC: &[&str] = &["hen", "arc", "x", "gekijouban"];

/// Words that name a release format (`HUNTER×HUNTER OVA`, `... Movie`):
/// a name carrying one says something a title may not.
const FORMAT_WORDS: &[&str] = &[
    "ova",
    "oad",
    "ona",
    "special",
    "specials",
    "movie",
    "film",
    "gekijouban",
];

/// Ordinal words that open a season or part marker (`Second Season`,
/// `The Final Season`).
const ORDINAL_WORDS: &[&str] = &[
    "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth", "final",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct NameForm {
    /// Distinctive Latin tokens (words and numbers), in order.
    pub latin: Vec<String>,
    /// CJK runs, matched by containment.
    pub cjk: Vec<String>,
    /// A four-digit year the name carries (`Fruits Basket (2019)`).
    pub year: Option<i32>,
    /// The season the name advertises (`2nd Season`, `Season 3`, `II`).
    pub season: Option<i32>,
    /// The part of a season the name advertises (`Part 2`, `Cour 2`).
    pub part: Option<i32>,
    /// An uppercase Roman number after the first word (`Kizumonogatari
    /// II: Nekketsu-hen`, `Lupin III`): a movie series' part number.
    pub sequence: Option<i32>,
    /// The name carries a format word (`HUNTER×HUNTER OVA`).
    pub format_word: bool,
}

impl NameForm {
    pub fn of(raw: &str) -> Self {
        let raw = raw.trim();
        let raw = sole_bracket_group(raw).unwrap_or(raw);
        if raw.is_empty() {
            return Self::default();
        }
        let parsed = release_title::parse(raw);
        let (season, part) = if parsed.seasons.hint_only {
            (None, parsed.seasons.part.or(parsed.season_hint()))
        } else {
            (
                release_title::season_marker(raw),
                parsed.seasons.part.filter(|p| *p > 0),
            )
        };
        let (text, sequence) = strip_roman_sequence(raw);
        let text = strip_cjk_markers(&fold(&text));
        let mut form = Self {
            season: season.filter(|s| *s > 0),
            part,
            sequence,
            ..Self::default()
        };
        let mut latin: Vec<String> = Vec::new();
        for token in tokens(&text) {
            match token {
                Token::Latin(t) => latin.push(t),
                Token::Cjk(c) => {
                    if c.chars().count() > 1 {
                        form.cjk.push(c);
                    }
                }
            }
        }
        let latin = strip_markers(latin);
        form.format_word = latin.iter().any(|t| FORMAT_WORDS.contains(&t.as_str()));
        for t in latin {
            if let Some(year) = as_year(&t) {
                form.year.get_or_insert(year);
                continue;
            }
            let lone_letter = t.chars().count() == 1 && t.chars().all(|c| c.is_alphabetic());
            if lone_letter || is_generic_title_token(&t) || IDENTIFY_GENERIC.contains(&t.as_str()) {
                continue;
            }
            if !form.latin.contains(&t) {
                form.latin.push(t);
            }
        }
        form
    }

    /// No distinctive token at all (`The Movie`, an empty field).
    pub fn is_empty(&self) -> bool {
        self.latin.is_empty() && self.cjk.is_empty()
    }

    /// A season or part marker of 2 or more.
    pub fn sequel_marker(&self) -> Option<i32> {
        self.season
            .filter(|s| *s >= 2)
            .or(self.part.filter(|p| *p >= 2))
    }
}

fn as_year(token: &str) -> Option<i32> {
    if token.len() != 4 || !token.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let year: i32 = token.parse().ok()?;
    (1900..=2100).contains(&year).then_some(year)
}

pub(super) fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}' // kana, including the prolonged sound mark
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{31F0}'..='\u{31FF}')
}

/// Lowercase, apostrophes deleted, diacritics folded.
pub(super) fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\'' | '\u{2018}' | '\u{2019}' | '`' | '\u{02BC}' | '\u{00B4}' => {}
            _ => {
                for lower in c.to_lowercase() {
                    out.push(fold_char(lower));
                }
            }
        }
    }
    out
}

fn fold_char(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' => 'i',
        'ñ' | 'ń' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => 'u',
        'ý' | 'ÿ' => 'y',
        'ś' | 'š' => 's',
        'ź' | 'ż' | 'ž' => 'z',
        'ł' => 'l',
        'ř' => 'r',
        'ť' => 't',
        'ď' => 'd',
        // Full-width forms (`ＳＰＹ`) fold onto ASCII.
        '\u{FF10}'..='\u{FF19}' | '\u{FF41}'..='\u{FF5A}' => {
            char::from_u32(c as u32 - 0xFEE0).unwrap_or(c)
        }
        _ => c,
    }
}

enum Token {
    Latin(String),
    Cjk(String),
}

/// Latin alphanumeric runs and CJK runs; everything else separates.
fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut latin = String::new();
    let mut cjk = String::new();
    let flush = |latin: &mut String, cjk: &mut String, out: &mut Vec<Token>| {
        if !latin.is_empty() {
            out.push(Token::Latin(std::mem::take(latin)));
        }
        if !cjk.is_empty() {
            out.push(Token::Cjk(std::mem::take(cjk)));
        }
    };
    for c in text.chars() {
        if is_cjk(c) {
            if !latin.is_empty() {
                out.push(Token::Latin(std::mem::take(&mut latin)));
            }
            cjk.push(c);
        } else if c.is_alphanumeric() {
            if !cjk.is_empty() {
                out.push(Token::Cjk(std::mem::take(&mut cjk)));
            }
            latin.push(c);
        } else {
            flush(&mut latin, &mut cjk, &mut out);
        }
    }
    flush(&mut latin, &mut cjk, &mut out);
    out
}

fn is_number(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())
}

fn is_ordinal_number(t: &str) -> bool {
    let digits = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &t[digits.len()..];
    is_number(digits) && matches!(suffix, "st" | "nd" | "rd" | "th")
}

/// `s2`, `s02`, `season2`, `cour2`, `part2`.
fn is_glued_marker(t: &str) -> bool {
    ["season", "part", "cour", "s"].iter().any(|prefix| {
        t.strip_prefix(prefix)
            .is_some_and(|rest| is_number(rest) && rest.len() <= 2)
    })
}

/// Season and part markers out of a token list: `season 2`, `2nd season`,
/// `second season`, `the final season`, `part 2`, `cour 2`, `s2`.
fn strip_markers(tokens: Vec<String>) -> Vec<String> {
    let marker_word = |t: &str| matches!(t, "season" | "seasons" | "part" | "cour");
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        let next = tokens.get(i + 1).map(String::as_str);
        if marker_word(t) {
            // `Season 2`: the number is the marker's.
            i += if next.is_some_and(is_number) { 2 } else { 1 };
            continue;
        }
        if (is_ordinal_number(t) || ORDINAL_WORDS.contains(&t)) && next.is_some_and(marker_word) {
            i += 2;
            continue;
        }
        if is_glued_marker(t) {
            i += 1;
            continue;
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

/// An uppercase Roman number (`II`..`IX`, and `I` right before a colon;
/// never `X`, which is `Hunter X Hunter`'s)
/// after the first word is a sequence number, not a word of the name.
fn strip_roman_sequence(raw: &str) -> (String, Option<i32>) {
    let mut out = String::with_capacity(raw.len());
    let mut sequence = None;
    let mut word_count = 0usize;
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            word_count += 1;
            let before_colon = chars[i..]
                .iter()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| *c == ':');
            let roman = word
                .chars()
                .all(|c| matches!(c, 'I' | 'V' | 'X'))
                .then(|| release_title::roman_value(&word))
                .flatten()
                .filter(|n| (2..=9).contains(n) || *n == 1 && before_colon);
            if word_count > 1
                && let Some(n) = roman
            {
                sequence.get_or_insert(n as i32);
                out.push(' ');
                continue;
            }
            out.push_str(&word);
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    (out, sequence)
}

/// `第2季`, `第二期`, `第3部`, `第2クール`: CJK season markers.
fn strip_cjk_markers(text: &str) -> String {
    const NUMERALS: &str = "0123456789一二三四五六七八九十";
    const COUNTERS: &[&str] = &["季", "期", "部", "章", "クール"];
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '第' {
            let mut j = i + 1;
            while j < chars.len() && NUMERALS.contains(chars[j]) {
                j += 1;
            }
            if j > i + 1 {
                let rest: String = chars[j..].iter().collect();
                if let Some(counter) = COUNTERS.iter().find(|c| rest.starts_with(**c)) {
                    out.push(' ');
                    i = j + counter.chars().count();
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::NameForm;

    fn latin(name: &str) -> Vec<String> {
        NameForm::of(name).latin
    }

    #[test]
    fn a_name_keeps_its_distinctive_tokens() {
        assert_eq!(
            latin("JoJo no Kimyou na Bouken: Steel Ball Run"),
            ["jojo", "kimyou", "na", "bouken", "steel", "ball", "run"]
        );
        assert_eq!(
            latin("JoJo's Bizarre Adventure"),
            ["jojos", "bizarre", "adventure"]
        );
        assert_eq!(latin("[Oshi No Ko]"), ["oshi", "ko"]);
        assert_eq!(latin("HUNTER×HUNTER"), ["hunter"]);
        assert_eq!(latin("Hunter x Hunter"), ["hunter"]);
        assert_eq!(latin("Kaiju No. 8"), ["kaiju", "8"]);
        assert_eq!(latin("Pokémon"), ["pokemon"]);
        let ova = NameForm::of("HUNTER×HUNTER OVA");
        assert_eq!(
            (ova.latin.as_slice(), ova.format_word),
            (["hunter".to_string()].as_slice(), true)
        );
        assert!(!NameForm::of("HUNTER×HUNTER").format_word);
    }

    #[test]
    fn markers_and_years_come_out_of_the_tokens() {
        let f = NameForm::of("Fruits Basket (2019)");
        assert_eq!(f.latin, ["fruits", "basket"]);
        assert_eq!(f.year, Some(2019));
        let f = NameForm::of("Jujutsu Kaisen 2nd Season");
        assert_eq!(f.latin, ["jujutsu", "kaisen"]);
        assert_eq!(f.season, Some(2));
        let f = NameForm::of("SPY×FAMILY Part 2");
        assert_eq!(f.latin, ["spy", "family"]);
        assert_eq!(f.part, Some(2));
        assert_eq!(f.sequel_marker(), Some(2));
        let f = NameForm::of("Shingeki no Kyojin: The Final Season");
        assert_eq!(f.latin, ["shingeki", "kyojin"]);
        let f = NameForm::of("Mob Psycho 100 II");
        assert_eq!(f.latin, ["mob", "psycho", "100"]);
        assert_eq!(f.season, Some(2));
        let f = NameForm::of("Kizumonogatari II: Nekketsu-hen");
        assert_eq!(f.latin, ["kizumonogatari", "nekketsu"]);
        assert_eq!((f.sequence, f.season), (Some(2), None));
        let f = NameForm::of("Kizumonogatari I: Tetsuketsu-hen");
        assert_eq!(f.sequence, Some(1));
        // A first word is never a sequence number, nor is `X`.
        assert_eq!(NameForm::of("I Want to Eat Your Pancreas").sequence, None);
        assert_eq!(NameForm::of("Hunter X Hunter").sequence, None);
    }

    #[test]
    fn cjk_runs_are_kept_whole_without_their_markers() {
        let f = NameForm::of("進撃の巨人 Season2");
        assert_eq!(f.cjk, ["進撃の巨人"]);
        assert!(f.latin.is_empty());
        let f = NameForm::of("呪術廻戦 第2期");
        assert_eq!(f.cjk, ["呪術廻戦"]);
        assert!(f.latin.is_empty(), "{:?}", f.latin);
    }

    #[test]
    fn a_name_of_generic_words_is_empty() {
        assert!(NameForm::of("The Movie").is_empty());
        assert!(NameForm::of("").is_empty());
        assert!(NameForm::of("!!!").is_empty());
    }
}
