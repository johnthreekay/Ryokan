//! Step two: split the normalized title into tokens.
//!
//! Every token remembers what separated it from the one before (space,
//! underscore, dot, or nothing) and how deep in brackets it sits. That is
//! what lets the rules tell `07.5` from `07 5`, `_-_` from `8-gou`, and a
//! bracketed `[03]` from title text, without the `\b` anchors that break
//! beside `_` (issue #296).
//!
//! - `_` and whitespace separate. `.` separates too, except inside a
//!   decimal: digits, a dot, and exactly one digit (`07.5`, `2.0`, `5.1`,
//!   `26.5.1`, and glued forms like `AAC2.0`). `100.2016` is two numbers.
//! - `-` between two digit runs is a range operator (`01-12`). Between
//!   other word characters it joins a compound word (`8-gou`, `WEB-DL`,
//!   `x265-KAF`, `S01-S04`). With a separator on both sides it is a
//!   segment dash (` - `, `_-_`).
//! - `~` between numbers is a range operator, spaced or not (`01 ~ 12`).
//! - Digits glued to CJK text split off (`ユーフォニアム3`, `第01`), and a
//!   number glued to `END`, `FIN` or `TV` splits too (`12END`, `001-293TV`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sep {
    Start,
    Space,
    Underscore,
    Dot,
    /// Glued to the previous token, or after punctuation such as `]`.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    /// Letters, or letters mixed with digits: `Gathering`, `x264`, `1080p`,
    /// `S01E02`, `4th`, `WEB-DL`, `8-gou`.
    Word,
    /// A digit run, with a glued revision (`05v2`).
    Num {
        value: u32,
        digits: u8,
        version: Option<u32>,
    },
    /// `07.5`, `2.0`, `26.5.1`: the integer part and whether the first
    /// fraction digit is a 5 (a half episode in an episode slot).
    Decimal {
        int: u32,
        digits: u8,
        half: bool,
    },
    Open(char),
    Close(char),
    /// A dash with a separator on both sides.
    SegDash,
    /// `-` or `~` joining two numbers.
    RangeOp,
    /// A dash glued to the word after it but not the one before
    /// (` -Báleygr`, `]-RTX`).
    LeadDash,
    /// `/`, `|`, `·`: separates alternate titles.
    Alt,
    Plus,
    Comma,
    Amp,
    Colon,
    Hash,
    Other,
}

#[derive(Debug, Clone)]
pub(super) struct Tok {
    pub kind: Kind,
    pub text: String,
    pub lower: String,
    pub sep: Sep,
    /// Brackets enclosing the token. An `Open` / `Close` token carries
    /// the depth outside its own bracket.
    pub depth: u16,
    /// Innermost enclosing bracket id (0 = top level). An `Open` /
    /// `Close` token carries its own bracket's id.
    pub group: u32,
    /// Char offsets into the normalized text.
    pub start: usize,
    pub end: usize,
}

impl Tok {
    pub fn num(&self) -> Option<(u32, u8, Option<u32>)> {
        match self.kind {
            Kind::Num {
                value,
                digits,
                version,
            } => Some((value, digits, version)),
            _ => None,
        }
    }

    pub fn is_word(&self) -> bool {
        self.kind == Kind::Word
    }
}

/// A bracketed group: `[...]`, `(...)` or `{...}`.
#[derive(Debug, Clone)]
pub(super) struct Group {
    pub id: u32,
    pub open: usize,
    /// Index of the closing token; the token count when never closed.
    pub close: usize,
    pub kind: char,
}

pub(super) struct Lexed {
    pub toks: Vec<Tok>,
    pub groups: Vec<Group>,
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xAC00..=0xD7AF | 0x31F0..=0x31FF)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

pub(super) fn lex(text: &str) -> Lexed {
    let chars: Vec<char> = text.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut stack: Vec<usize> = Vec::new(); // indexes into `groups`
    let mut sep = Sep::Start;
    let mut i = 0;

    #[allow(clippy::too_many_arguments)]
    let push = |toks: &mut Vec<Tok>,
                kind: Kind,
                s: String,
                sep: Sep,
                stack: &[usize],
                groups: &[Group],
                start: usize,
                end: usize| {
        let depth = stack.len() as u16;
        let group = stack.last().map(|&g| groups[g].id).unwrap_or(0);
        let lower = s.to_lowercase();
        toks.push(Tok {
            kind,
            text: s,
            lower,
            sep,
            depth,
            group,
            start,
            end,
        });
    };

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            sep = Sep::Space;
            i += 1;
            continue;
        }
        if c == '_' {
            if sep != Sep::Space {
                sep = Sep::Underscore;
            }
            i += 1;
            continue;
        }
        if is_word_char(c) {
            let start = i;
            let mut j = i;
            while j < chars.len() {
                let cj = chars[j];
                if is_word_char(cj) {
                    // CJK text and ASCII digits split apart.
                    if j > start {
                        let prev = chars[j - 1];
                        if (is_cjk(prev) && cj.is_ascii_digit())
                            || (prev.is_ascii_digit() && is_cjk(cj))
                        {
                            break;
                        }
                    }
                    j += 1;
                    continue;
                }
                let prev = chars[j - 1];
                let next = chars.get(j + 1).copied();
                if cj == '.' && prev.is_ascii_digit() && next.is_some_and(|n| n.is_ascii_digit()) {
                    // A decimal takes exactly one digit after the dot, and
                    // the word ends there or continues as another decimal
                    // (`26.5.1`); `2x04.2x05` is two words.
                    let after = chars.get(j + 2).copied();
                    if !after.is_some_and(|a| is_word_char(a) && a != '.') {
                        j += 2;
                        continue;
                    }
                }
                if cj == '-'
                    && next.is_some_and(is_word_char)
                    && !(prev.is_ascii_digit() && next.is_some_and(|n| n.is_ascii_digit()))
                    && !is_cjk(prev)
                    && !next.is_some_and(is_cjk)
                    && !(prev.is_alphabetic() && short_number_ends_word(&chars, j + 1))
                {
                    j += 1;
                    continue;
                }
                break;
            }
            let word: String = chars[start..j].iter().collect();
            let mut cursor = start;
            for (kind, s) in split_word(&word) {
                let len = s.chars().count();
                push(
                    &mut toks,
                    kind,
                    s,
                    sep,
                    &stack,
                    &groups,
                    cursor,
                    cursor + len,
                );
                cursor += len;
                sep = Sep::None;
            }
            i = j;
            sep = Sep::None;
            continue;
        }
        match c {
            '.' => {
                if sep == Sep::None || sep == Sep::Start {
                    sep = Sep::Dot;
                }
                i += 1;
                continue;
            }
            '[' | '(' | '{' => {
                let id = groups.len() as u32 + 1;
                groups.push(Group {
                    id,
                    open: toks.len(),
                    close: usize::MAX,
                    kind: c,
                });
                let depth = stack.len() as u16;
                toks.push(Tok {
                    kind: Kind::Open(c),
                    text: c.to_string(),
                    lower: c.to_string(),
                    sep,
                    depth,
                    group: id,
                    start: i,
                    end: i + 1,
                });
                stack.push(groups.len() - 1);
            }
            ']' | ')' | '}' => {
                if let Some(g) = stack.pop() {
                    groups[g].close = toks.len();
                    let id = groups[g].id;
                    toks.push(Tok {
                        kind: Kind::Close(c),
                        text: c.to_string(),
                        lower: c.to_string(),
                        sep,
                        depth: stack.len() as u16,
                        group: id,
                        start: i,
                        end: i + 1,
                    });
                } else {
                    push(
                        &mut toks,
                        Kind::Other,
                        c.to_string(),
                        sep,
                        &stack,
                        &groups,
                        i,
                        i + 1,
                    );
                }
            }
            '-' | '~' => {
                let prev_tok_num = toks
                    .last()
                    .is_some_and(|t| matches!(t.kind, Kind::Num { .. }));
                let next_digit =
                    next_significant(&chars, i + 1).is_some_and(|n| n.is_ascii_digit());
                let glued_before = i > 0 && is_word_char(chars[i - 1]);
                let glued_after = chars.get(i + 1).is_some_and(|n| is_word_char(*n));
                let space_after = chars
                    .get(i + 1)
                    .is_none_or(|n| n.is_whitespace() || *n == '_' || matches!(n, '[' | '('));
                let kind = if c == '-'
                    && glued_before
                    && chars[i - 1].is_alphabetic()
                    && short_number_ends_word(&chars, i + 1)
                {
                    // `Series_Title-01`: a title glued to its episode.
                    Kind::SegDash
                } else if c == '~' {
                    if prev_tok_num && next_digit {
                        Kind::RangeOp
                    } else {
                        Kind::Other
                    }
                } else if glued_before
                    && glued_after
                    && chars[i - 1].is_ascii_digit()
                    && chars[i + 1].is_ascii_digit()
                {
                    // `01-12`, and `S01E01-02` / `S01-04` / `Ep01-12`,
                    // where the left side ends a word.
                    Kind::RangeOp
                } else if !glued_before && space_after {
                    Kind::SegDash
                } else if !glued_before && glued_after {
                    Kind::LeadDash
                } else if prev_tok_num && next_digit {
                    // `01 -12`, `1 - 152` written oddly: still between numbers.
                    Kind::RangeOp
                } else {
                    Kind::Other
                };
                push(
                    &mut toks,
                    kind,
                    c.to_string(),
                    sep,
                    &stack,
                    &groups,
                    i,
                    i + 1,
                );
            }
            '/' | '|' | '·' => push(
                &mut toks,
                Kind::Alt,
                c.to_string(),
                sep,
                &stack,
                &groups,
                i,
                i + 1,
            ),
            '+' => push(
                &mut toks,
                Kind::Plus,
                c.to_string(),
                sep,
                &stack,
                &groups,
                i,
                i + 1,
            ),
            ',' => push(
                &mut toks,
                Kind::Comma,
                c.to_string(),
                sep,
                &stack,
                &groups,
                i,
                i + 1,
            ),
            '&' => push(
                &mut toks,
                Kind::Amp,
                c.to_string(),
                sep,
                &stack,
                &groups,
                i,
                i + 1,
            ),
            ':' | '：' => push(
                &mut toks,
                Kind::Colon,
                c.to_string(),
                sep,
                &stack,
                &groups,
                i,
                i + 1,
            ),
            '#' => push(
                &mut toks,
                Kind::Hash,
                c.to_string(),
                sep,
                &stack,
                &groups,
                i,
                i + 1,
            ),
            _ => push(
                &mut toks,
                Kind::Other,
                c.to_string(),
                sep,
                &stack,
                &groups,
                i,
                i + 1,
            ),
        }
        sep = Sep::None;
        i += 1;
    }
    let n = toks.len();
    for g in &mut groups {
        if g.close == usize::MAX {
            g.close = n;
        }
    }
    Lexed { toks, groups }
}

/// Two or three digits at `from` that end the word (`Title-01`, not
/// `TV-1` or `DVD-480p`).
fn short_number_ends_word(chars: &[char], from: usize) -> bool {
    let run = chars[from.min(chars.len())..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count();
    (2..=3).contains(&run) && !chars.get(from + run).is_some_and(|c| is_word_char(*c))
}

fn next_significant(chars: &[char], from: usize) -> Option<char> {
    chars[from.min(chars.len())..]
        .iter()
        .copied()
        .find(|c| !c.is_whitespace() && *c != '_')
}

/// Splits one word run into tokens: a digit run becomes a number (with a
/// glued `v2`), a decimal becomes a decimal, and a number glued to `END`,
/// `FIN` or `TV` splits off. Anything else stays one word.
fn split_word(word: &str) -> Vec<(Kind, String)> {
    let bytes = word.as_bytes();
    let digit_end = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digit_end == 0 || digit_end > 9 {
        return vec![(Kind::Word, word.to_string())];
    }
    let value: u32 = word[..digit_end].parse().unwrap_or(0);
    let digits = digit_end as u8;
    let rest = &word[digit_end..];
    if rest.is_empty() {
        return vec![(
            Kind::Num {
                value,
                digits,
                version: None,
            },
            word.to_string(),
        )];
    }
    if let Some(frac) = rest.strip_prefix('.') {
        let first = frac.chars().next();
        if first.is_some_and(|c| c.is_ascii_digit())
            && frac.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            return vec![(
                Kind::Decimal {
                    int: value,
                    digits,
                    half: first == Some('5'),
                },
                word.to_string(),
            )];
        }
    }
    let lower = rest.to_ascii_lowercase();
    if let Some(v) = lower.strip_prefix('v')
        && !v.is_empty()
        && v.len() <= 2
        && v.bytes().all(|b| b.is_ascii_digit())
    {
        return vec![(
            Kind::Num {
                value,
                digits,
                version: v.parse().ok(),
            },
            word.to_string(),
        )];
    }
    for marker in ["end", "fin", "tv"] {
        if let Some(after) = lower.strip_prefix(marker)
            && !after.starts_with(|c: char| c.is_ascii_alphanumeric())
        {
            return vec![
                (
                    Kind::Num {
                        value,
                        digits,
                        version: None,
                    },
                    word[..digit_end].to_string(),
                ),
                (Kind::Word, rest.to_string()),
            ];
        }
    }
    vec![(Kind::Word, word.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<String> {
        lex(text)
            .toks
            .iter()
            .map(|t| match &t.kind {
                Kind::Word => format!("W:{}", t.text),
                Kind::Num { value, version, .. } => match version {
                    Some(v) => format!("N:{value}v{v}"),
                    None => format!("N:{value}"),
                },
                Kind::Decimal { int, half, .. } => {
                    format!("D:{int}{}", if *half { ".5" } else { "" })
                }
                Kind::SegDash => "SD".into(),
                Kind::RangeOp => "R".into(),
                Kind::LeadDash => "LD".into(),
                Kind::Open(c) | Kind::Close(c) => c.to_string(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn underscores_separate_and_ranges_glue() {
        assert_eq!(
            kinds("[EA]Dark_Gathering_01-25_End_[BDRIP]"),
            [
                "[",
                "W:EA",
                "]",
                "W:Dark",
                "W:Gathering",
                "N:1",
                "R",
                "N:25",
                "W:End",
                "[",
                "W:BDRIP",
                "]"
            ]
        );
    }

    #[test]
    fn segment_dash_versus_compound_word() {
        assert_eq!(
            kinds("Kaiju No. 8 - Kaijuu 8-gou"),
            ["W:Kaiju", "W:No", "N:8", "SD", "W:Kaijuu", "W:8-gou"]
        );
        assert_eq!(kinds("Show_-_05"), ["W:Show", "SD", "N:5"]);
    }

    #[test]
    fn decimals_versions_and_glued_markers() {
        assert_eq!(kinds("- 07.5 x"), ["SD", "D:7.5", "W:x"]);
        assert_eq!(kinds("AAC2.0 100.2016"), ["W:AAC2.0", "N:100", "N:2016"]);
        assert_eq!(kinds("12v2 [12END]"), ["N:12v2", "[", "N:12", "W:END", "]"]);
        assert_eq!(
            kinds("[001-293TV全集]"),
            ["[", "N:1", "R", "N:293", "W:TV全集", "]"]
        );
    }

    #[test]
    fn cjk_and_digits_split() {
        assert_eq!(kinds("第01-12话"), ["W:第", "N:1", "R", "N:12", "W:话"]);
        assert_eq!(kinds("ユーフォニアム3"), ["W:ユーフォニアム", "N:3"]);
    }

    #[test]
    fn tilde_ranges_and_lead_dashes() {
        assert_eq!(kinds("01 ~ 12"), ["N:1", "R", "N:12"]);
        assert_eq!(kinds("AAC -Báleygr"), ["W:AAC", "LD", "W:Báleygr"]);
        assert_eq!(kinds("[1080p]-RTX"), ["[", "W:1080p", "]", "LD", "W:RTX"]);
    }
}
