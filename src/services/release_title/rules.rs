//! Step four: the ordered rules that turn tokens into a [`ParsedTitle`].
//!
//! Episode rules run in this order and the first that matches wins (the
//! order is load-bearing, see `AGENTS.md`):
//!
//! 1. `SxxEyy` tags, any depth (`(S01E03)` counts).
//! 2. The number after a ` - ` / `_-_`, when what follows it is a
//!    boundary (bracket, metadata, another dash, the end), never title
//!    text: `Series - 100 Years Quest - 05` is episode 5. A season marker
//!    right before the dash binds the season (`S2 - 04`, `IV - 13`).
//! 3. `E05` / `EP05` / `Ep 05`.
//! 4. `Episode 05`, `#05`, `第05话`, `04 of 12`.
//! 5. `OVA 01` / `SP01` (a special).
//! 6. A bracketed number or range (`[03]`, `[08v2]`, `(01-12)`).
//! 7. A bare range in the title text (`01-25`, `1100-1104`).
//! 8. A season marker then a bare number (`S2 05`, `2nd Season 05`, `II 05`).
//!
//! Then two gates drop the claim: a half episode (`- 07.5`) and an extra
//! (NCOP, preview). A bare number after the title that none of these
//! place is recorded as ambiguous for series-aware readers.

use super::anchors::title_words;
use super::lexer::{Group, Kind, Sep, Tok, lex};
use super::normalize::normalize;
use super::vocab::{Class, classify, is_crc, word_ordinal};
use super::{ClaimRule, EpisodeClaim, ParsedTitle, Seasons};

struct Ctx<'a> {
    text: &'a [char],
    toks: &'a [Tok],
    groups: &'a [Group],
    class: Vec<Class>,
    /// Tokens no rule reads: the leading group and `{...}` notes.
    skip: Vec<bool>,
}

fn english_ordinal(lower: &str) -> bool {
    lower.ends_with("st") || lower.ends_with("nd") || lower.ends_with("rd") || lower.ends_with("th")
}

/// Largest season a range reads (`S01-S04`, `Seasons 1-3`): a range end
/// past it is not a season, and an unbounded one would expand into a
/// vector the size of the number.
const MAX_SEASON: u32 = 99;

fn is_year(value: u32, digits: u8) -> bool {
    digits == 4 && (1900..=2100).contains(&value)
}

impl Ctx<'_> {
    fn next(&self, i: usize) -> Option<usize> {
        (i + 1..self.toks.len()).find(|&j| !self.skip[j])
    }

    fn prev(&self, i: usize) -> Option<usize> {
        (0..i).rev().find(|&j| !self.skip[j])
    }

    fn num(&self, i: usize) -> Option<(u32, u8, Option<u32>)> {
        self.toks.get(i).and_then(Tok::num)
    }

    /// A number usable as an episode: up to four digits, not a year.
    fn episode_num(&self, i: usize) -> Option<u32> {
        let (value, digits, _) = self.num(i)?;
        (digits <= 4 && !is_year(value, digits)).then_some(value)
    }

    fn group(&self, id: u32) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    fn inner_text(&self, g: &Group) -> String {
        let start = self.toks[g.open].end;
        let end = self
            .toks
            .get(g.close)
            .map(|t| t.start)
            .unwrap_or(self.text.len());
        self.text[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// Tokens directly inside a bracket (any depth), skipped ones excluded.
    fn content(&self, g: &Group) -> Vec<usize> {
        (g.open + 1..g.close.min(self.toks.len()))
            .filter(|&i| !self.skip[i])
            .collect()
    }

    /// Whether what follows token `i` ends an episode number: the end,
    /// a bracket, a dash, a separator, or a non-title word (metadata, end
    /// / special / extra / batch markers). A plain word is title text.
    /// Whether token `i` marks an episode: a tag, `E05`, `OVA 01`, a dash
    /// followed by a number, or a number after `Episode`.
    fn marks_episode(&self, i: usize) -> bool {
        matches!(
            self.class[i],
            Class::Tag { .. } | Class::EpPrefix(..) | Class::SpecialNum(_)
        ) || self.toks[i].kind == Kind::SegDash
            && self.next(i).is_some_and(|n| self.episode_num(n).is_some())
            || self.episode_num(i).is_some()
                && self
                    .prev(i)
                    .is_some_and(|p| self.class[p] == Class::EpisodeWord)
    }

    fn boundary_after(&self, i: usize) -> bool {
        let Some(j) = self.next(i) else {
            return true;
        };
        match self.toks[j].kind {
            Kind::Num { .. } | Kind::Decimal { .. } | Kind::RangeOp => false,
            Kind::Word => !matches!(
                self.class[j],
                Class::Plain
                    | Class::SeasonWord
                    | Class::PartWord
                    | Class::VolWord
                    | Class::Ordinal(_)
                    | Class::WordNumber(_)
                    | Class::Of
                    | Class::Collection
                    | Class::Roman(_)
                    | Class::CjkEpisode
                    | Class::CjkSeasonCounter
            ),
            _ => true,
        }
    }

    /// A range starting at the number `k`: `-` / `~` (or, in an episode
    /// slot, a spaced dash) then a larger number. Outside an episode slot
    /// a spaced range needs equal widths or a zero-padded left side, so a
    /// title's own number (`Chihayafuru 2 - 05`) is never a range start.
    fn range_after(&self, k: usize, slot: bool) -> Option<(u32, usize)> {
        let (a, a_digits, _) = self.num(k)?;
        let j = self.next(k)?;
        let op = &self.toks[j];
        let spaced = match op.kind {
            Kind::RangeOp => {
                op.sep == Sep::Space || self.toks.get(j + 1).is_some_and(|t| t.sep == Sep::Space)
            }
            Kind::SegDash if slot => true,
            _ => return None,
        };
        let m = self.next(j)?;
        let (b, b_digits, _) = self.num(m)?;
        if b <= a || is_year(b, b_digits) || is_year(a, a_digits) || b_digits > 4 {
            return None;
        }
        if spaced
            && !slot
            && a_digits != b_digits
            && !(a_digits >= 2 && self.toks[k].text.starts_with('0'))
        {
            return None;
        }
        Some(self.extend_glued(b, m))
    }

    /// Follows a glued run of further numbers (`01-02-03`, `S01E05-06`).
    fn extend_glued(&self, mut last: u32, mut at: usize) -> (u32, usize) {
        while let Some(op) = self.next(at)
            && self.toks[op].kind == Kind::RangeOp
            && self.toks[op].sep == Sep::None
            && let Some(n) = self.next(op)
            && let Some((v, d, _)) = self.num(n)
            && v > last
            && !is_year(v, d)
        {
            last = v;
            at = n;
        }
        (last, at)
    }

    /// The season a marker right before token `i` names: `S2`, `IV`,
    /// `Season 2`, `2nd Season`, `2.ª Temporada`, `第2季`, `TV-1`.
    fn season_marker_before(&self, i: usize) -> Option<u32> {
        let p = self.prev(i)?;
        match self.class[p] {
            Class::SeasonShort(n) | Class::Roman(n) | Class::TvSeason(n) | Class::CjkSeason(n) => {
                return Some(n);
            }
            Class::SeasonWord if !self.season_word_takes_number(p) => {
                return self.ordinal_before_season_word(p);
            }
            Class::CjkSeasonCounter => {
                let n = self.prev(p)?;
                let o = self.prev(n)?;
                if self.class[o] == Class::CjkOrdinal {
                    return self.num(n).map(|(v, _, _)| v);
                }
            }
            _ => {}
        }
        if let Some((n, _, _)) = self.num(p)
            && let Some(w) = self.prev(p)
            && self.class[w] == Class::SeasonWord
        {
            return Some(n);
        }
        None
    }

    /// A season word that takes the number after it (`Season 2`), as
    /// opposed to one closing an ordinal (`2nd Season 12`, where 12 is
    /// the episode).
    fn season_word_takes_number(&self, w: usize) -> bool {
        self.class[w] == Class::SeasonWord && self.ordinal_before_season_word(w).is_none()
    }

    /// `2nd Season`, `2a temporada`, `2.ª Temporada`, `Second Season`.
    fn ordinal_before_season_word(&self, w: usize) -> Option<u32> {
        let o = self.prev(w)?;
        match self.class[o] {
            Class::Ordinal(n) if n > 0 => Some(n),
            Class::Ordinal(0) => self.prev(o).and_then(|n| self.num(n)).map(|(v, _, _)| v),
            Class::Plain => word_ordinal(&self.toks[o].lower),
            _ => None,
        }
    }

    fn claim(&self, first: u32, last: u32, season: Option<u32>, rule: ClaimRule) -> EpisodeClaim {
        EpisodeClaim {
            season: season.map(|s| s as i32),
            first: first as i32,
            last: last as i32,
            list: Vec::new(),
            rule,
            tight: true,
            glued: true,
            opens_before_dash: false,
        }
    }

    /// Whether the range starting at number `k` is written tight: the
    /// operator glued on both sides and two digits or more each side.
    fn tight_from(&self, k: usize) -> bool {
        let Some(op) = self.next(k) else { return false };
        let Some(r) = self.next(op) else { return false };
        let digits = |t: usize| self.num(t).map(|(_, d, _)| d).unwrap_or(0);
        self.glued_from(k) && digits(k) >= 2 && digits(r) >= 2
    }

    /// Whether the operator after number `k` is glued to both numbers.
    fn glued_from(&self, k: usize) -> bool {
        let Some(op) = self.next(k) else { return false };
        let Some(r) = self.next(op) else { return false };
        self.toks[op].kind == Kind::RangeOp
            && self.toks[op].sep == Sep::None
            && self.toks[r].sep == Sep::None
    }

    /// `claim` for a range read from number `k`.
    fn range_claim(
        &self,
        k: usize,
        first: u32,
        last: u32,
        season: Option<u32>,
        rule: ClaimRule,
    ) -> EpisodeClaim {
        let mut claim = self.claim(first, last, season, rule);
        claim.tight = first == last || self.tight_from(k);
        claim.glued = first == last || self.glued_from(k);
        claim
    }
}

pub(super) fn parse(title: &str) -> ParsedTitle {
    let norm = normalize(title);
    let text: Vec<char> = norm.text.chars().collect();
    let lexed = lex(&norm.text);
    let mut class: Vec<Class> = lexed.toks.iter().map(classify).collect();
    // `3x3 Eyes`: a one-digit `NxN` that runs on into title words is the
    // name, not a tag (`Series Title 1x1`, `- 2x4 -` stay tags).
    for (i, tok) in lexed.toks.iter().enumerate() {
        if matches!(class[i], Class::Tag { .. })
            && tok.lower.starts_with(|c: char| c.is_ascii_digit())
            && tok.lower.rsplit('x').next().is_some_and(|ep| ep.len() < 2)
            && lexed
                .toks
                .get(i + 1)
                .is_some_and(|n| n.depth == tok.depth && n.is_word() && classify(n) == Class::Plain)
        {
            class[i] = Class::Plain;
        }
    }
    let mut ctx = Ctx {
        text: &text,
        toks: &lexed.toks,
        groups: &lexed.groups,
        class,
        skip: vec![false; lexed.toks.len()],
    };
    for g in ctx.groups {
        if g.kind == '{' {
            for i in g.open..=g.close.min(ctx.toks.len() - 1) {
                ctx.skip[i] = true;
            }
        }
    }

    let mut out = ParsedTitle {
        single_file: norm.single_file,
        ..ParsedTitle::default()
    };
    out.group = leading_group(&mut ctx);
    out.revision = revision(title);
    out.resolution =
        (0..ctx.toks.len())
            .filter(|&i| !ctx.skip[i])
            .find_map(|i| match ctx.class[i] {
                Class::Resolution(n) => Some(n),
                _ => None,
            });
    out.year = (0..ctx.toks.len()).filter(|&i| !ctx.skip[i]).find_map(|i| {
        let (v, d, _) = ctx.num(i)?;
        is_year(v, d).then_some(v as i32)
    });
    out.year_range = year_range(&ctx);
    out.hash = ctx
        .groups
        .iter()
        .filter(|g| g.kind == '[' || g.kind == '(')
        .filter_map(|g| {
            let content = ctx.content(g);
            match content.as_slice() {
                [only] if is_crc(&ctx.toks[*only].text) => {
                    Some(ctx.toks[*only].text.to_uppercase())
                }
                _ => None,
            }
        })
        .next_back();

    out.half_episode = half_episode(&ctx);
    out.episodes = if out.half_episode {
        None
    } else {
        episode_claim(&ctx)
    };
    out.extra = extra(&ctx);
    if out.extra {
        out.episodes = None;
    }
    out.seasons = seasons(&ctx);
    out.includes_specials = includes_specials(&ctx);
    (out.special, out.special_from_title_word) = special(&ctx, out.episodes.as_ref(), &out.seasons);
    keywords(&ctx, &mut out);
    if out.episodes.is_none() && !out.half_episode {
        out.ambiguous_number = ambiguous_number(&ctx);
    }
    if out.group.is_none() {
        out.group = trailing_group(&ctx);
    }
    if out.resolution.is_none() {
        out.resolution = bare_resolution(&ctx, out.episodes.as_ref());
    }
    out.title = primary_title(&ctx);
    out.anchor_words = title_words(ctx.toks, &ctx.skip, &ctx.class);
    out
}

/// An airing span (`[1983-1984]`, `(2019-2021)`): two ascending years
/// joined by a range dash.
fn year_range(ctx: &Ctx) -> bool {
    (0..ctx.toks.len()).any(|i| {
        if ctx.skip[i] || ctx.toks[i].kind != Kind::RangeOp {
            return false;
        }
        let year = |at: Option<usize>| at.and_then(|k| ctx.num(k)).filter(|&(v, d, _)| is_year(v, d));
        matches!((year(ctx.prev(i)), year(ctx.next(i))), (Some((a, _, _)), Some((b, _, _))) if a < b)
    })
}

/// The shared revision grammar (`media::parse_release_revision`), plus a
/// `v2` glued to a closing bracket (`[Eng-Subs]v2`), which it misses.
fn revision(title: &str) -> super::ReleaseRevision {
    let mut rev = crate::services::media::parse_release_revision(title);
    if rev.version == 1 {
        let lower = title.to_lowercase();
        // Every `]v` / `)v`, not just the first: `[Group]Vinland ... [1080p]v2`.
        let found = lower
            .match_indices("]v")
            .chain(lower.match_indices(")v"))
            .find_map(|(at, _)| {
                let rest = &lower[at + 2..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                let after = rest[digits.len()..].chars().next();
                ((1..=2).contains(&digits.len())
                    && after.is_none_or(|c| c.is_whitespace() || "._-)]".contains(c)))
                .then(|| digits.parse::<u32>().ok())
                .flatten()
            });
        if let Some(v) = found {
            rev.version = v;
        }
    }
    rev
}

// ── Group ──────────────────────────────────────────────────────────────

/// The first leading bracket that is not metadata, a number, a date or a
/// CRC: `[SubsPlease]`, `(Wonder-Subs)`, `[Naruto-Kun.Hu - UraharaShop]`.
/// Metadata brackets before it (`[BD][1080p]`, `【BDMV】`) are skipped.
fn leading_group(ctx: &mut Ctx) -> Option<String> {
    let mut i = 0;
    while i < ctx.toks.len() {
        let tok = &ctx.toks[i];
        let Kind::Open(kind) = tok.kind else {
            return None;
        };
        if kind == '{' {
            return None;
        }
        let g = ctx.group(tok.group)?.clone();
        if g.close >= ctx.toks.len() {
            // `[unclosed group rest`: not a group.
            return None;
        }
        let content = ctx.content(&g);
        let inner = ctx.inner_text(&g).to_lowercase();
        if is_website(&inner) {
            for j in g.open..=g.close.min(ctx.toks.len() - 1) {
                ctx.skip[j] = true;
            }
            i = g.close + 1;
            continue;
        }
        let has_word = content.iter().any(|&c| ctx.toks[c].is_word());
        let is_group = match content.first() {
            None => !ctx.inner_text(&g).is_empty(),
            Some(_) if !has_word => {
                !content
                    .iter()
                    .any(|&c| matches!(ctx.toks[c].kind, Kind::Num { .. }))
                    && !ctx.inner_text(&g).is_empty()
            }
            Some(&first) => {
                let tok = &ctx.toks[first];
                let numeric_first = matches!(tok.kind, Kind::Num { .. } | Kind::Decimal { .. });
                let all_meta = content.iter().all(|&c| {
                    !ctx.toks[c].is_word()
                        || matches!(
                            ctx.class[c],
                            Class::Resolution(_)
                                | Class::Meta
                                | Class::Movie
                                | Class::Special
                                | Class::Extra
                                | Class::Batch
                                | Class::End
                                | Class::Version(_)
                        )
                });
                let crc = content.len() == 1 && is_crc(&tok.text);
                !numeric_first && !all_meta && !crc
            }
        };
        if is_group {
            for j in g.open..=g.close.min(ctx.toks.len() - 1) {
                ctx.skip[j] = true;
            }
            return Some(ctx.inner_text(&g));
        }
        i = g.close + 1;
    }
    None
}

/// `www.Torrenting.com`, `高清剧集网发布 www.DDHDTV.com`, `TorrentS.pl`.
fn is_website(inner: &str) -> bool {
    // A group name may carry a domain (`Lilith-Raws x WitEx.io`); only a
    // `www.` or a bracket holding nothing but a domain is a website.
    inner.contains("www.")
        || (!inner.contains(char::is_whitespace)
            && inner.rsplit_once('.').is_some_and(|(name, tld)| {
                !name.is_empty()
                    && matches!(
                        tld,
                        "com"
                            | "net"
                            | "org"
                            | "pl"
                            | "tv"
                            | "io"
                            | "cc"
                            | "me"
                            | "to"
                            | "xyz"
                            | "info"
                            | "ru"
                    )
            }))
}

/// A scene-style group at the end: `x265-YURASUKA`, `H.264-VARYG`,
/// `AC3-KuroNeko`, ` -Báleygr`, `[1080p]-RTX`.
fn trailing_group(ctx: &Ctx) -> Option<String> {
    let mut found = None;
    for i in 0..ctx.toks.len() {
        let tok = &ctx.toks[i];
        if ctx.skip[i] || tok.depth > 0 {
            continue;
        }
        // `H.264-VARYG`, `WEB-DL-GROUP`, `x264-Tsundere-Raws`: the group
        // follows the longest metadata head.
        if tok.is_word() && ctx.class[i] == Class::Plain && tok.text.contains('-') {
            let split = tok
                .text
                .match_indices('-')
                .map(|(at, _)| (&tok.text[..at], &tok.text[at + 1..]))
                .rfind(|(head, _)| {
                    let head_lower = head.to_lowercase();
                    matches!(head_lower.as_str(), "264" | "265")
                        || matches!(
                            classify_word(&head_lower),
                            Class::Meta | Class::Resolution(_)
                        )
                });
            if let Some((_, tail)) = split
                && !tail.is_empty()
                && classify_word(&tail.to_lowercase()) == Class::Plain
            {
                found = Some(tail.to_string());
            }
        }
        if tok.kind == Kind::LeadDash
            && let Some(n) = ctx.next(i)
            && ctx.toks[n].is_word()
            && ctx.class[n] == Class::Plain
            && ctx.next(n).is_none()
        {
            found = Some(ctx.toks[n].text.clone());
        }
    }
    if found.is_none() {
        // `Title - 031 - Episode Title [Lunar]`: a one-word group in the
        // last bracket; two words (`(Dual Audio)[MICO XD]`) only right
        // after another bracket.
        let last = ctx.toks.len().checked_sub(1)?;
        let plain = |t: usize| {
            ctx.toks[t].is_word()
                && ctx.class[t] == Class::Plain
                && !is_crc(&ctx.toks[t].text)
                && ctx.toks[t].text.chars().all(|c| c.is_ascii_alphanumeric())
        };
        if let Kind::Close(']') = ctx.toks[last].kind
            && let Some(g) = ctx.group(ctx.toks[last].group)
        {
            let content = ctx.content(g);
            let after_bracket = ctx
                .prev(g.open)
                .is_some_and(|p| matches!(ctx.toks[p].kind, Kind::Close(_)));
            match content.as_slice() {
                [only] if plain(*only) => found = Some(ctx.toks[*only].text.clone()),
                [a, b]
                    if after_bracket
                        && plain(*a)
                        && plain(*b)
                        && ctx.toks[*b].sep == Sep::Space =>
                {
                    found = Some(format!("{} {}", ctx.toks[*a].text, ctx.toks[*b].text));
                }
                _ => {}
            }
        }
    }
    if found.is_none() {
        found = group_before_alternate_title(ctx);
    }
    found
}

/// `... (1080p TVer Web-DL) [Lord Karasuma] / Detective Conan ...`: one or
/// two plain words in a square bracket that follows a metadata bracket
/// and comes right before an alternate title (` / `, ` | `).
fn group_before_alternate_title(ctx: &Ctx) -> Option<String> {
    ctx.groups.iter().filter(|g| g.kind == '[').find_map(|g| {
        let content = ctx.content(g);
        let words_ok = (1..=2).contains(&content.len())
            && content.iter().all(|&t| {
                ctx.toks[t].is_word()
                    && ctx.class[t] == Class::Plain
                    && ctx.toks[t].text.chars().all(|c| c.is_ascii_alphanumeric())
            });
        let after_meta = ctx.prev(g.open).is_some_and(|p| {
            matches!(ctx.toks[p].kind, Kind::Close(_))
                && ctx
                    .group(ctx.toks[p].group)
                    .is_some_and(|m| ctx.content(m).iter().any(|&t| ctx.class[t].is_meta()))
        });
        let before_alt = g.close < ctx.toks.len()
            && ctx
                .next(g.close)
                .is_some_and(|n| ctx.toks[n].kind == Kind::Alt);
        (words_ok && after_meta && before_alt).then(|| {
            content
                .iter()
                .map(|&t| ctx.toks[t].text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
    })
}

fn classify_word(lower: &str) -> Class {
    lex(lower)
        .toks
        .first()
        .map(classify)
        .unwrap_or(Class::Plain)
}

// ── Episode claim ──────────────────────────────────────────────────────

fn episode_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    tag_claim(ctx)
        .or_else(|| dash_claim(ctx))
        .or_else(|| prefix_claim(ctx))
        .or_else(|| episode_word_claim(ctx))
        .or_else(|| special_number_claim(ctx))
        .or_else(|| bracket_claim(ctx))
        .or_else(|| bare_range_claim(ctx))
        .or_else(|| marker_number_claim(ctx))
}

/// Whether a tag's episode run (`S01E05-06`, `S01E01E02E03`, a glued
/// `S01E05` `-06`) has two digits or more in every number.
fn tag_run_is_tight(lower: &str, ctx: &Ctx, i: usize) -> bool {
    let run = lower
        .split_once('e')
        .map(|(_, rest)| rest.trim_start_matches('p'))
        .unwrap_or_default();
    // The run ends at the first part that is not a number (`-1080p`).
    let inner = run
        .split(['-', 'e'])
        .filter(|p| !p.is_empty())
        .map(|p| p.split('v').next().unwrap_or(p))
        .take_while(|p| p.bytes().all(|b| b.is_ascii_digit()))
        .all(|p| p.len() >= 2);
    let glued = match ctx.next(i) {
        Some(op) if ctx.toks[op].kind == Kind::RangeOp && ctx.toks[op].sep == Sep::None => ctx
            .next(op)
            .and_then(|r| ctx.num(r))
            .is_none_or(|(_, d, _)| d >= 2),
        _ => true,
    };
    inner && glued
}

fn tag_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        match ctx.class[i] {
            Class::Tag {
                season,
                first,
                last,
                ..
            } => {
                let (last, _) = ctx.extend_glued(last, i);
                let mut claim = ctx.claim(first, last, Some(season), ClaimRule::Tag);
                claim.tight = first == last || tag_run_is_tight(&ctx.toks[i].lower, ctx, i);
                return Some(claim);
            }
            // `S01 E05`, `S01 E27-E51`, `S6.E1-E2`, `S01.E.01`.
            Class::SeasonShort(season) => {
                if let Some(n) = ctx.next(i)
                    && matches!(ctx.toks[n].sep, Sep::Space | Sep::Dot)
                {
                    if let Class::EpPrefix(first, last) = ctx.class[n] {
                        let (last, _) = ctx.extend_glued(last, n);
                        return Some(ctx.claim(first, last, Some(season), ClaimRule::Tag));
                    }
                    if matches!(ctx.toks[n].lower.as_str(), "e" | "ep")
                        && let Some(k) = ctx.next(n)
                        && let Some(a) = ctx.episode_num(k)
                    {
                        return Some(ctx.claim(a, a, Some(season), ClaimRule::Tag));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn dash_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    for d in 0..ctx.toks.len() {
        if ctx.skip[d] || ctx.toks[d].kind != Kind::SegDash || ctx.toks[d].depth > 0 {
            continue;
        }
        // `Epi 1 - 4`: the right end of a marked range, not a dash episode.
        if let Some(p) = ctx.prev(d)
            && ctx.num(p).is_some()
            && let Some(w) = ctx.prev(p)
            && ctx.class[w] == Class::EpisodeWord
        {
            continue;
        }
        let Some(k) = ctx.next(d) else { continue };
        let Some(a) = ctx.episode_num(k) else {
            continue;
        };
        let season = ctx.season_marker_before(d);
        if let Some((b, m)) = ctx.range_after(k, true)
            && (ctx.boundary_after(m)
                || ctx.tight_from(k)
                    && ctx
                        .next(m)
                        .is_some_and(|w| ctx.toks[w].is_word() && ctx.class[w] == Class::Plain))
        {
            // `Show - 1-5 - 07 Title`: a digit pair in the episode title;
            // the next dash holds the episode.
            let later_episode = ctx
                .next(m)
                .filter(|&d2| ctx.toks[d2].kind == Kind::SegDash)
                .and_then(|d2| ctx.next(d2))
                .and_then(|e| ctx.episode_num(e));
            if let Some(e) = later_episode {
                return Some(ctx.claim(e, e, season, ClaimRule::Dash));
            }
            // `- 05-06 The Title`: a range written tight is one even
            // before episode-title words.
            return Some(ctx.range_claim(k, a, b, season, ClaimRule::Dash));
        }
        if ctx.boundary_after(k) {
            // `Show 01 - 119 [Batch]`: a zero-padded number before the
            // dash opens a range (a title's own `2` or `100` never does).
            if let Some(p) = ctx.prev(d)
                && let Some((left, digits, _)) = ctx.num(p)
                && digits >= 2
                && ctx.toks[p].text.starts_with('0')
                && left >= 1
                && left < a
                // `Cyborg 009 - 12`: a padded title number is wider than
                // the episode after it.
                && ctx.num(k).is_some_and(|(_, a_digits, _)| digits <= a_digits)
                // `Season 02 - 05`: the padded number is the season's.
                && ctx.prev(p).is_some_and(|w| {
                    ctx.toks[w].is_word()
                        && !matches!(
                            ctx.class[w],
                            Class::SeasonWord | Class::PartWord | Class::VolWord | Class::EpisodeWord
                        )
                })
            {
                let mut claim = ctx.claim(left, a, season, ClaimRule::Dash);
                claim.tight = false;
                claim.glued = false;
                claim.opens_before_dash = true;
                return Some(claim);
            }
            return Some(ctx.claim(a, a, season, ClaimRule::Dash));
        }
    }
    None
}

fn prefix_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] || bonus_episode(ctx, i) {
            continue;
        }
        if let Class::EpPrefix(first, last) = ctx.class[i] {
            let (last, _) = ctx.extend_glued(last, i);
            return Some(ctx.claim(
                first,
                last,
                ctx.season_marker_before(i),
                ClaimRule::EpisodePrefix,
            ));
        }
        if ctx.class[i] == Class::EpisodeWord
            && matches!(ctx.toks[i].lower.as_str(), "ep" | "epi" | "eps")
        {
            let mut k = ctx.next(i)?;
            if ctx.toks[k].kind == Kind::Colon {
                k = ctx.next(k)?;
            }
            if let Some(a) = ctx.episode_num(k) {
                let last = ctx.range_after(k, true).map(|(b, _)| b).unwrap_or(a);
                return Some(ctx.range_claim(
                    k,
                    a,
                    last,
                    ctx.season_marker_before(i),
                    ClaimRule::EpisodePrefix,
                ));
            }
        }
    }
    None
}

fn episode_word_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        let tok = &ctx.toks[i];
        if ctx.class[i] == Class::EpisodeWord && !matches!(tok.lower.as_str(), "ep" | "epi" | "eps")
        {
            let Some(k) = ctx.next(i) else { continue };
            let Some(a) = ctx.episode_num(k) else {
                continue;
            };
            // `Episode 27,69,76`.
            let mut list = vec![a as i32];
            let mut at = k;
            while let Some(c) = ctx.next(at)
                && ctx.toks[c].kind == Kind::Comma
                && let Some(n) = ctx.next(c)
                && let Some(v) = ctx.episode_num(n)
            {
                list.push(v as i32);
                at = n;
            }
            if list.len() > 1 {
                let mut claim = ctx.claim(a, a, None, ClaimRule::EpisodeWord);
                claim.last = *list.iter().max().unwrap_or(&claim.first);
                claim.list = list;
                return Some(claim);
            }
            let last = ctx.range_after(k, true).map(|(b, _)| b).unwrap_or(a);
            return Some(ctx.range_claim(
                k,
                a,
                last,
                ctx.season_marker_before(i),
                ClaimRule::EpisodeWord,
            ));
        }
        // `#957`.
        if tok.kind == Kind::Hash
            && let Some(k) = ctx.next(i)
            && ctx.toks[k].sep == Sep::None
            && let Some(a) = ctx.episode_num(k)
        {
            return Some(ctx.claim(a, a, None, ClaimRule::EpisodeWord));
        }
        // `第05话`, `第01-12话`.
        if ctx.class[i] == Class::CjkOrdinal
            && let Some(k) = ctx.next(i)
            && let Some(a) = ctx.episode_num(k)
        {
            let (last, end) = ctx.range_after(k, true).unwrap_or((a, k));
            if ctx
                .next(end)
                .is_some_and(|c| ctx.class[c] == Class::CjkEpisode)
            {
                return Some(ctx.range_claim(k, a, last, None, ClaimRule::EpisodeWord));
            }
        }
        // `04 of 12`.
        if let Some(a) = ctx.episode_num(i)
            && let Some(o) = ctx.next(i)
            && ctx.class[o] == Class::Of
            && ctx.next(o).is_some_and(|n| ctx.num(n).is_some())
        {
            let season = ctx
                .group(tok.group)
                .and_then(|g| ctx.season_marker_before(g.open));
            return Some(ctx.claim(a, a, season, ClaimRule::EpisodeWord));
        }
    }
    None
}

fn special_number_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        if let Class::SpecialNum(n) = ctx.class[i] {
            return Some(ctx.claim(n, n, None, ClaimRule::SpecialNumber));
        }
        if matches!(ctx.class[i], Class::Special | Class::Sp)
            && !ctx.prev(i).is_some_and(|p| ctx.toks[p].kind == Kind::Plus)
            && let Some(k) = ctx.next(i)
            && let Some(a) = ctx.episode_num(k)
            && ctx.toks[k].depth == ctx.toks[i].depth
        {
            let (last, end) = ctx.range_after(k, true).unwrap_or((a, k));
            if ctx.boundary_after(end) {
                return Some(ctx.range_claim(k, a, last, None, ClaimRule::SpecialNumber));
            }
        }
    }
    None
}

/// `[03]`, `[08v2]`, `[12END]`, `[01-12 Fin]`, `(01-12)`, `[第01-12话]`,
/// `[04 of 12]`, `[001-293TV全集]`. A single number needs square
/// brackets; `(86)` beside a title is an alternate title.
fn bracket_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    for g in ctx.groups {
        // `[224] Title`: a numeric tag opening the title is not an episode.
        if g.kind == '{' || g.open == 0 || ctx.skip.get(g.open).copied().unwrap_or(true) {
            continue;
        }
        let content = ctx.content(g);
        let mut iter = content.iter().copied();
        let Some(mut first) = iter.next() else {
            continue;
        };
        if ctx.class[first] == Class::CjkOrdinal {
            match iter.next() {
                Some(n) => first = n,
                None => continue,
            }
        }
        let Some(a) = ctx.episode_num(first) else {
            continue;
        };
        // A date (`[2019.12.03]`) starts with a year and is skipped above;
        // `[2026]` too.
        let season = ctx.season_marker_before(g.open);
        if let Some((b, _)) = ctx.range_after(first, true) {
            return Some(ctx.range_claim(first, a, b, season, ClaimRule::Bracket));
        }
        let rest_ok = content
            .iter()
            .skip_while(|&&c| c != first)
            .skip(1)
            .all(|&c| {
                let tok = &ctx.toks[c];
                matches!(
                    ctx.class[c],
                    Class::End | Class::Version(_) | Class::CjkEpisode | Class::Batch
                ) || (tok.is_word()
                    && tok.text.chars().all(|ch| !ch.is_ascii())
                    && tok.sep != Sep::None)
            });
        if ctx.next(first).is_some_and(|o| ctx.class[o] == Class::Of) {
            return Some(ctx.claim(a, a, season, ClaimRule::Bracket));
        }
        if g.kind == '[' && rest_ok {
            return Some(ctx.claim(a, a, season, ClaimRule::Bracket));
        }
    }
    None
}

fn bare_range_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    for k in 0..ctx.toks.len() {
        if ctx.skip[k] || ctx.toks[k].depth > 0 {
            continue;
        }
        let Some(a) = ctx.episode_num(k) else {
            continue;
        };
        if ctx.prev(k).is_some_and(|p| {
            ctx.season_word_takes_number(p)
                || matches!(
                    ctx.class[p],
                    Class::PartWord | Class::VolWord | Class::EpisodeWord
                )
        }) {
            continue;
        }
        if let Some((b, m)) = ctx.range_after(k, false)
            && ctx.boundary_after(m)
        {
            return Some(ctx.range_claim(k, a, b, None, ClaimRule::BareRange));
        }
    }
    None
}

/// `S2 05`, `Season 2 05v2`, `2nd Season 05`, `4th 04`, `II 05`.
fn marker_number_claim(ctx: &Ctx) -> Option<EpisodeClaim> {
    for k in 0..ctx.toks.len() {
        if ctx.skip[k] {
            continue;
        }
        let Some((value, digits, _)) = ctx.num(k) else {
            continue;
        };
        if !(2..=3).contains(&digits) {
            continue;
        }
        let season = match ctx.prev(k) {
            Some(p)
                if matches!(ctx.class[p], Class::Ordinal(n) if n > 0)
                    && english_ordinal(&ctx.toks[p].lower) =>
            {
                match ctx.class[p] {
                    Class::Ordinal(n) => Some(n),
                    _ => None,
                }
            }
            _ => ctx.season_marker_before(k),
        };
        let Some(season) = season else { continue };
        // `Season 2 05`: the marker's own number is `prev(k)`; make sure
        // `k` is not that number.
        if ctx.prev(k).is_some_and(|p| ctx.season_word_takes_number(p)) {
            continue;
        }
        let mut end = k;
        if let Some(v) = ctx.next(k)
            && matches!(ctx.class[v], Class::Version(_))
        {
            end = v;
        }
        if ctx.boundary_after(end) {
            return Some(ctx.claim(value, value, Some(season), ClaimRule::MarkerNumber));
        }
    }
    None
}

// ── Gates ──────────────────────────────────────────────────────────────

/// `- 07.5`, `_-_12.5_`, `ep 13.5`, `S01E07.5`, `(12.5)`, `Title 12.5 [`.
fn half_episode(ctx: &Ctx) -> bool {
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        if matches!(ctx.class[i], Class::HalfTag(_)) {
            return true;
        }
        let Kind::Decimal { half, digits, .. } = ctx.toks[i].kind else {
            // `07_5`: a number, an underscore, and a lone 5.
            if let Some((_, d, _)) = ctx.num(i)
                && d >= 2
                && let Some(n) = ctx.next(i)
                && ctx.toks[n].sep == Sep::Underscore
                && ctx.num(n).is_some_and(|(v, d, _)| v == 5 && d == 1)
                && ctx
                    .prev(i)
                    .is_some_and(|p| ctx.toks[p].kind == Kind::SegDash)
            {
                return true;
            }
            continue;
        };
        if !half || digits > 4 {
            continue;
        }
        let prev = ctx.prev(i);
        let slot = prev.is_some_and(|p| {
            ctx.toks[p].kind == Kind::SegDash
                || ctx.class[p] == Class::EpisodeWord
                || matches!(ctx.toks[p].kind, Kind::Open(_))
        });
        let before_meta = ctx.next(i).is_none_or(|n| {
            matches!(
                ctx.toks[n].kind,
                Kind::Open(_) | Kind::Close(_) | Kind::SegDash
            ) || matches!(ctx.class[n], Class::Special | Class::End)
        });
        // `- S01E05 - Chapter 7.5`: past a marked episode a decimal is
        // the episode title's.
        let marked_before = (0..i).any(|j| !ctx.skip[j] && ctx.marks_episode(j));
        if slot || (ctx.toks[i].depth == 0 && before_meta && i > 0 && !marked_before) {
            return true;
        }
    }
    false
}

/// NCOP / NCED / creditless anywhere; OP / ED / PV / CM / preview /
/// trailer / menu after the first title word when nothing structural came
/// before it, or followed by a boundary when an episode did (`- 05 -
/// Opening Night` is an episode title). A keyword after `+` means the
/// pack includes extras, not that it is one.
fn extra(ctx: &Ctx) -> bool {
    let mut first_word_seen = false;
    let mut structure_before = false;
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        let tok = &ctx.toks[i];
        let class = ctx.class[i];
        if ctx.marks_episode(i) {
            structure_before = true;
        }
        if class == Class::Extra {
            if after_plus(ctx, i) {
                continue;
            }
            let lower = tok.lower.as_str();
            if matches!(
                lower,
                "ncop" | "nced" | "ncoped" | "creditless" | "textless"
            ) {
                return true;
            }
            // Inside a phrase of title words the word is the name or the
            // episode title, not an extras label: before any episode mark
            // when more words follow (`Today's Menu for the Emiya Family -
            // S01E05`), after one when the phrase opens a segment of its
            // own (`- S01E05 - Happy Ending`). `- 12 Minute Preview` is a
            // label.
            let title_word = |j: usize| {
                ctx.toks[j].depth == tok.depth
                    && ctx.toks[j].is_word()
                    && matches!(ctx.class[j], Class::Plain | Class::Of)
            };
            let mut start = i;
            while let Some(p) = ctx.prev(start).filter(|&p| title_word(p)) {
                start = p;
            }
            let in_phrase = start < i;
            let opens_segment = ctx
                .prev(start)
                .is_some_and(|p| ctx.toks[p].kind == Kind::SegDash);
            if in_phrase
                && if structure_before {
                    opens_segment
                } else {
                    ctx.next(i).is_some_and(title_word)
                }
            {
                continue;
            }
            let bracketed = tok.depth > 0;
            if (bracketed || first_word_seen) && (!structure_before || ctx.boundary_after(i)) {
                return true;
            }
        }
        if tok.is_word() && tok.depth == 0 {
            first_word_seen = true;
        }
    }
    false
}

/// Whether token `i` is joined to the previous one by `+` (directly, or
/// through `&` / `and` in a run like `+ NCOP & NCED`).
/// `S1 + Ep. 0`: an episode added to a season pack, not the release's
/// own episode.
fn bonus_episode(ctx: &Ctx, i: usize) -> bool {
    matches!(ctx.class[i], Class::EpPrefix(..) | Class::EpisodeWord)
        && after_plus(ctx, i)
        && (0..i).any(|k| {
            !ctx.skip[k] && matches!(ctx.class[k], Class::SeasonShort(_) | Class::SeasonWord)
        })
}

fn after_plus(ctx: &Ctx, i: usize) -> bool {
    let mut at = i;
    while let Some(p) = ctx.prev(at) {
        match ctx.toks[p].kind {
            Kind::Plus => return true,
            // `+ NCOP & NCED`, `+ NCOP/NCED`, `+ NCOP, NCED`.
            Kind::Amp | Kind::Alt | Kind::Comma => at = p,
            Kind::Word
                if matches!(ctx.class[p], Class::Extra | Class::Special | Class::Sp)
                    || ctx.toks[p].lower == "and" =>
            {
                at = p
            }
            _ => return false,
        }
    }
    false
}

// ── Seasons ────────────────────────────────────────────────────────────

fn seasons(ctx: &Ctx) -> Seasons {
    let mut tag: Option<u32> = None;
    let mut explicit: Vec<u32> = Vec::new();
    let mut part: Option<u32> = None;
    let mut marker_seen = false;
    let mut hint_only = false;
    let mut part_pack = false;
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        match ctx.class[i] {
            Class::Tag { season, .. } | Class::HalfTag(season) => {
                tag.get_or_insert(season);
            }
            Class::SeasonRange(a, b) => explicit.extend(a..=b),
            Class::SeasonShort(n) => {
                explicit.push(n);
                if let Some(b) = season_range_end(ctx, i, n) {
                    explicit.extend(n + 1..=b);
                }
            }
            Class::TvSeason(n) | Class::CjkSeason(n) => explicit.push(n),
            Class::SeasonWord => match ctx.ordinal_before_season_word(i) {
                // `2nd Season 12`: the number after is the episode.
                Some(n) => explicit.push(n),
                None => season_word_numbers(ctx, i, &mut explicit),
            },
            Class::CjkOrdinal => {
                if let Some(k) = ctx.next(i)
                    && let Some((n, _, _)) = ctx.num(k)
                    && ctx
                        .next(k)
                        .is_some_and(|c| ctx.class[c] == Class::CjkSeasonCounter)
                {
                    explicit.push(n);
                }
            }
            // A bare English ordinal before an episode number or range
            // (`4th 04`, `2nd 01-12`); `1a` alone is not one.
            Class::Ordinal(n) if n > 0 && english_ordinal(&ctx.toks[i].lower) => {
                if let Some(k) = ctx.next(i)
                    && ctx.num(k).is_some()
                {
                    explicit.push(n);
                }
            }
            Class::Roman(n) => {
                if roman_is_marker(ctx, i) {
                    explicit.push(n);
                }
            }
            Class::PartWord => {
                if let Some(k) = ctx.next(i) {
                    let n = match (ctx.num(k), ctx.class[k]) {
                        (Some((n, _, _)), _) => Some(n),
                        (None, Class::Roman(n)) => Some(n),
                        _ => None,
                    };
                    if part.is_none()
                        && let Some(n) = n
                    {
                        part = Some(n);
                        // Kaizoku's `Series Cour 2 (WEB 1080p)`: a top-level
                        // part marker right before a bracket is a pack.
                        part_pack = ctx.toks[i].depth == 0
                            && ctx
                                .next(k)
                                .is_some_and(|b| matches!(ctx.toks[b].kind, Kind::Open(_)));
                    }
                }
            }
            Class::PartShort(n) => {
                if ctx.season_marker_before(i).is_some() {
                    part = part.or(Some(n));
                }
            }
            Class::VolWord => {
                if marker_seen
                    && let Some(k) = ctx.next(i)
                    && let Some((n, _, _)) = ctx.num(k)
                {
                    part = part.or(Some(n));
                }
            }
            _ => {}
        }
        marker_seen |= !explicit.is_empty();
    }
    let numbers: Vec<i32> = if let Some(t) = tag {
        vec![t as i32]
    } else if !explicit.is_empty() {
        let mut uniq: Vec<u32> = explicit.clone();
        uniq.sort_unstable();
        uniq.dedup();
        if uniq.len() > 1 {
            uniq.iter().map(|&n| n as i32).collect()
        } else {
            vec![explicit[0] as i32]
        }
    } else if let Some(p) = part.filter(|p| *p >= 2) {
        hint_only = true;
        vec![p as i32]
    } else {
        Vec::new()
    };
    Seasons {
        numbers,
        part: part.map(|p| p as i32),
        hint_only,
        part_pack,
    }
}

/// The end of a season range starting at `S01` (token `i`): `S01-04`,
/// `S01 - S07`, `S01 S04` (two adjacent markers, as Sonarr reads them).
fn season_range_end(ctx: &Ctx, i: usize, start: u32) -> Option<u32> {
    let mut j = ctx.next(i)?;
    let op = ctx.toks[j].kind.clone();
    if matches!(op, Kind::RangeOp | Kind::SegDash) {
        j = ctx.next(j)?;
        // `S01-04` takes a bare number; `S3 - 05` is an episode, so a
        // spaced dash needs a second marker (`S01 - S07`).
        if op == Kind::RangeOp
            && let Some((b, _, _)) = ctx.num(j)
        {
            return (b > start && b <= MAX_SEASON).then_some(b);
        }
    }
    // Exactly two markers: a third (`S01 S03 S04`) makes it a list.
    let lone =
        |at: Option<usize>| !at.is_some_and(|a| matches!(ctx.class[a], Class::SeasonShort(_)));
    match ctx.class[j] {
        Class::SeasonShort(b)
            if b > start
                && ctx.toks[j].depth == ctx.toks[i].depth
                && lone(ctx.prev(i))
                && lone(ctx.next(j)) =>
        {
            Some(b)
        }
        _ => None,
    }
}

/// Numbers after a season word: `Season 2`, `Seasons 1-3`, `Season 1,2,3`,
/// `Saisons 1/2/3`, `Saison 1 & 2`, `Season 01+Season 02`, `Season One`,
/// `Season S2` (the `S2` is read on its own).
fn season_word_numbers(ctx: &Ctx, i: usize, out: &mut Vec<u32>) {
    let Some(mut k) = ctx.next(i) else { return };
    if let Class::WordNumber(n) = ctx.class[k] {
        out.push(n);
        return;
    }
    let Some((mut n, _, _)) = ctx.num(k) else {
        return;
    };
    out.push(n);
    loop {
        let Some(op) = ctx.next(k) else { return };
        let Some(m) = ctx.next(op) else { return };
        // `Season 01 - Season 07`.
        if ctx.toks[op].kind == Kind::SegDash && ctx.class[m] == Class::SeasonWord {
            if let Some(e) = ctx.next(m)
                && let Some((v, _, _)) = ctx.num(e)
                && v > n
                && v <= MAX_SEASON
            {
                out.extend(n + 1..=v);
            }
            return;
        }
        let Some((v, _, _)) = ctx.num(m) else { return };
        match ctx.toks[op].kind {
            Kind::RangeOp if v > n && v <= MAX_SEASON => out.extend(n + 1..=v),
            Kind::Comma | Kind::Alt | Kind::Amp | Kind::Plus => out.push(v),
            _ => return,
        }
        n = v;
        k = m;
    }
}

/// An uppercase `II`..`IX` is a season marker when it ends the title (a
/// bracket, dash, `|`, the end, or metadata follows) or is followed by an
/// episode number, never mid-title (`Lupin III vs`, `Lupin III: The
/// Castle`, `Date A Live IV x DanMachi`).
/// A Roman numeral ending a series name before a subtitle colon
/// (`DanMachi IV: Fuka Shou`), the season of an AniList name. Not read in
/// release titles, where `Lupin III: The Castle of Cagliostro` is a name;
/// `Lupin III: Part 6` is never one either way.
pub(super) fn roman_before_colon(name: &str) -> Option<u32> {
    let norm = normalize(name);
    let lexed = lex(&norm.text);
    let toks = &lexed.toks;
    (1..toks.len().saturating_sub(1)).find_map(|i| {
        let Class::Roman(n) = classify(&toks[i]) else {
            return None;
        };
        (toks[i].depth == 0
            && toks[i - 1].is_word()
            && toks[i + 1].kind == Kind::Colon
            && toks
                .get(i + 2)
                .is_none_or(|t| classify(t) != Class::PartWord))
        .then_some(n)
    })
}

fn roman_is_marker(ctx: &Ctx, i: usize) -> bool {
    // `Lupin III - Part 6 - 05`, `Lupin III: Part 6`: the part after the
    // dash or colon is the season and the numeral is the name.
    if let Some(d) = ctx.next(i)
        && matches!(ctx.toks[d].kind, Kind::SegDash | Kind::Colon)
        && ctx.next(d).is_some_and(|w| ctx.class[w] == Class::PartWord)
    {
        return false;
    }
    // Inside a note (`(..., The Legend of Hei II)`) it is an alternate
    // title; only top level or a bracket of its own (`[II]`) counts.
    if ctx.toks[i].depth > 0
        && !ctx
            .group(ctx.toks[i].group)
            .is_some_and(|g| ctx.content(g).len() == 1)
    {
        return false;
    }
    let Some(p) = ctx.prev(i) else { return false };
    if !(ctx.toks[p].is_word()
        || matches!(
            ctx.toks[p].kind,
            Kind::Comma | Kind::Open(_) | Kind::Num { .. } | Kind::Other
        ))
    {
        return false;
    }
    let Some(n) = ctx.next(i) else { return true };
    match ctx.toks[n].kind {
        Kind::Open(_)
        | Kind::Close(_)
        | Kind::SegDash
        | Kind::Alt
        | Kind::Comma
        | Kind::LeadDash => true,
        Kind::Num { .. } => true,
        Kind::Word => ctx.class[n].is_meta() || matches!(ctx.class[n], Class::Batch | Class::End),
        _ => false,
    }
}

// ── Specials ───────────────────────────────────────────────────────────

/// A special marker inside a pack: after `+` (`+ OVA`, `+OVAs`, `+ Sp`),
/// in a bracket that also holds a `+`, or after a season in a list
/// (`S1, S2, S2 OVA, S3`).
fn includes_specials(ctx: &Ctx) -> bool {
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        if bonus_episode(ctx, i) {
            return true;
        }
        if !matches!(ctx.class[i], Class::Special | Class::Sp) {
            continue;
        }
        if after_plus(ctx, i) {
            return true;
        }
        if ctx.toks[i].depth > 0
            && let Some(g) = ctx.group(ctx.toks[i].group)
            && ctx
                .content(g)
                .iter()
                .any(|&c| ctx.toks[c].kind == Kind::Plus)
        {
            return true;
        }
        if let Some(p) = ctx.prev(i)
            && matches!(ctx.class[p], Class::SeasonShort(_))
            && ctx.prev(p).is_some_and(|q| ctx.toks[q].kind == Kind::Comma)
        {
            return true;
        }
    }
    false
}

/// `(special, from a title word)`. The second is set when the only
/// evidence is a marker ending the title text (`... Kekkon Special - 01`),
/// which a series named that way (`A Returner's Magic Should Be Special`)
/// can veto.
fn special(ctx: &Ctx, claim: Option<&EpisodeClaim>, seasons: &Seasons) -> (bool, bool) {
    if claim.is_some_and(|c| c.rule == ClaimRule::SpecialNumber || c.season == Some(0))
        || seasons.numbers == [0]
    {
        return (true, false);
    }
    // A tag naming a real season (`S02E03`) is a regular episode.
    let real_season = claim.is_some_and(|c| c.season.is_some_and(|s| s > 0));
    let mut metadata_seen = false;
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        let tok = &ctx.toks[i];
        if ctx.class[i].is_meta() {
            metadata_seen = true;
        }
        if ctx.class[i] != Class::Special || after_plus(ctx, i) {
            continue;
        }
        let prev = ctx.prev(i);
        let next = ctx.next(i);
        let next_kind = next.map(|n| &ctx.toks[n].kind);
        // `(OVA)`, `[Special]`: a bracket holding only the marker.
        if tok.depth > 0
            && let Some(g) = ctx.group(tok.group)
            && ctx.content(g).len() == 1
        {
            return (true, false);
        }
        // ` - OVA`, `- 01 - Special`, `13.5 OVA`, `05 Special`.
        let ends = next.is_none_or(|_| {
            matches!(
                next_kind,
                Some(Kind::Open(_) | Kind::Close(_) | Kind::SegDash | Kind::Alt)
            )
        });
        if prev.is_some_and(|p| {
            ctx.toks[p].kind == Kind::SegDash
                || matches!(ctx.toks[p].kind, Kind::Num { .. } | Kind::Decimal { .. })
        }) && ends
        {
            return (true, false);
        }
        // `Show OVA-01`: a marker glued to its number is a special's.
        if let Some(d) = next
            && ctx.toks[d].kind == Kind::SegDash
            && ctx.toks[d].sep == Sep::None
            && ctx
                .next(d)
                .is_some_and(|n| ctx.toks[n].sep == Sep::None && ctx.num(n).is_some())
        {
            return (true, false);
        }
        // `- 05 - Something Special`: past an episode's dash the marker
        // ends the episode title.
        let past_episode_dash = claim.is_some()
            && (0..i).any(|j| {
                !ctx.skip[j] && ctx.toks[j].depth == 0 && ctx.toks[j].kind == Kind::SegDash
            });
        // `R.O.D (Read or Die) OVA (BD`, `Mob Psycho 100 Specials (BD`,
        // `... Kekkon Special - 01 ~ 03`: a marker ending the title text.
        if tok.depth == 0
            && !metadata_seen
            && ends
            && prev.is_some()
            && !real_season
            && !past_episode_dash
        {
            return (true, true);
        }
    }
    (false, false)
}

// ── Keywords ───────────────────────────────────────────────────────────

fn keywords(ctx: &Ctx, out: &mut ParsedTitle) {
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        let tok = &ctx.toks[i];
        let plus = ctx.prev(i).is_some_and(|p| ctx.toks[p].kind == Kind::Plus);
        match ctx.class[i] {
            Class::Batch => out.batch_keyword = true,
            Class::Collection => {
                let compound = ctx.prev(i).is_some_and(|p| {
                    matches!(
                        ctx.toks[p].lower.as_str(),
                        "complete"
                            | "ultimate"
                            | "perfect"
                            | "movie"
                            | "movies"
                            | "series"
                            | "bd"
                            | "blu-ray"
                            | "box"
                    )
                });
                let alone = tok.depth > 0
                    && ctx
                        .group(tok.group)
                        .is_some_and(|g| ctx.content(g).len() == 1);
                if compound || alone {
                    out.batch_keyword = true;
                }
            }
            Class::Movies if !plus => out.batch_keyword = true,
            Class::Movie if !plus => out.movie = true,
            Class::End => {
                out.end_marker = true;
                if tok.depth > 0 {
                    out.bracketed_end = true;
                }
            }
            Class::VolWord => {
                if let Some(k) = ctx.next(i)
                    && ctx.range_after(k, true).is_some()
                {
                    out.volume_range = true;
                }
            }
            _ => {}
        }
        // `Season Pack`, `Full Season`, `Mini Batch`.
        if let Some(n) = ctx.next(i) {
            let pair = (tok.lower.as_str(), ctx.toks[n].lower.as_str());
            if matches!(
                pair,
                ("season", "pack") | ("full", "season") | ("complete", "series")
            ) {
                out.batch_keyword = true;
            }
        }
    }
}

// ── Ambiguous trailing number ──────────────────────────────────────────

/// A bare number after the title that no rule placed: before a subtitle
/// dash (`86 - Eighty Six`, `Cyborg 009 - Legend`), before a bracket
/// (`Mob Psycho 100 (BD)`, `Dark_Gathering_24_[`), or at the very end
/// (`MAX 13`).
fn ambiguous_number(ctx: &Ctx) -> Option<i32> {
    // The last bare number before the metadata, as Sonarr's bare absolute
    // pattern reads it: `Show 99 (extra) 12 (1080p)` is 12.
    let mut found = None;
    for k in 0..ctx.toks.len() {
        if ctx.skip[k] || ctx.toks[k].depth > 0 {
            continue;
        }
        // Past the title: metadata at top level ends the search
        // (`1080p NF WEB-DL AAC2.0 H.264 (Japanese Sub)` has no title number).
        if ctx.class[k].is_meta() {
            return found;
        }
        // `Show 2 (2023) - 05 The Title`: a number after a later dash is
        // the episode, so one before it is the title's.
        if ctx.toks[k].kind == Kind::SegDash && ctx.next(k).is_some_and(|n| ctx.num(n).is_some()) {
            found = None;
            continue;
        }
        let Some((value, digits, _)) = ctx.num(k) else {
            continue;
        };
        if digits > 4 || is_year(value, digits) {
            continue;
        }
        // `H.264` split by mixed delimiters: the codec's number (`Beyblade
        // X 136` is the title's).
        if ctx.toks[k].sep == Sep::Dot
            && ctx
                .prev(k)
                .is_some_and(|p| matches!(ctx.toks[p].lower.as_str(), "h" | "x"))
        {
            continue;
        }
        // A marked number (`+ Ep. 0`, `#5`, `Part 2`, `Season 2`, `Vol 3`)
        // or the right side of a pair (`06-05`) is not the title's.
        if ctx.prev(k).is_some_and(|p| {
            matches!(
                ctx.class[p],
                Class::EpisodeWord | Class::PartWord | Class::SeasonWord | Class::VolWord
            ) || matches!(ctx.toks[p].kind, Kind::Hash | Kind::Plus | Kind::RangeOp)
        }) {
            continue;
        }
        let mut n = ctx.next(k);
        if let Some(v) = n
            && matches!(ctx.class[v], Class::Version(_))
        {
            n = ctx.next(v);
        }
        let hit = match n {
            // A number ending the name only when no bracketed one came
            // before it.
            None => digits >= 2 && found.is_none(),
            Some(j) => match ctx.toks[j].kind {
                Kind::Open(_) => true,
                Kind::SegDash => ctx.next(j).is_some_and(|w| ctx.toks[w].is_word()),
                // Scene names put metadata right after it:
                // `Dragon.Ball.043.V2.480p.DBox`.
                _ => digits >= 2 && ctx.class[j].is_meta(),
            },
        };
        if hit {
            found = Some(value as i32);
        }
    }
    found
}

/// A standard height written without `p` (`10-bit 1080 BluRay`), read
/// as a resolution only beside metadata and only when it is not the
/// episode (One Piece's `- 1080` is claimed first).
fn bare_resolution(ctx: &Ctx, claim: Option<&EpisodeClaim>) -> Option<u32> {
    for k in 0..ctx.toks.len() {
        if ctx.skip[k] {
            continue;
        }
        let Some((value, _, _)) = ctx.num(k) else {
            continue;
        };
        if !matches!(value, 480 | 576 | 720 | 1080 | 2160)
            || claim.is_some_and(|c| c.first == value as i32 || c.last == value as i32)
        {
            continue;
        }
        let beside_meta = [ctx.prev(k), ctx.next(k)]
            .into_iter()
            .flatten()
            .any(|n| ctx.class[n].is_meta());
        if beside_meta {
            return Some(value);
        }
    }
    None
}

// ── Title ──────────────────────────────────────────────────────────────

/// Depth-0 text up to the first structural or metadata token.
fn primary_title(ctx: &Ctx) -> Option<String> {
    let mut words: Vec<&str> = Vec::new();
    for i in 0..ctx.toks.len() {
        if ctx.skip[i] {
            continue;
        }
        let tok = &ctx.toks[i];
        if tok.depth > 0 || matches!(tok.kind, Kind::Open(_)) {
            if words.is_empty() {
                continue;
            }
            break;
        }
        let stop = !matches!(
            ctx.class[i],
            Class::Plain | Class::Roman(_) | Class::Collection | Class::Of | Class::WordNumber(_)
        ) || tok.kind == Kind::SegDash
            && ctx.next(i).is_some_and(|n| ctx.num(n).is_some())
            || tok.kind == Kind::Alt;
        if stop && !words.is_empty() {
            break;
        }
        if tok.is_word() || tok.num().is_some() {
            words.push(&tok.text);
        }
    }
    (!words.is_empty()).then(|| words.join(" "))
}
