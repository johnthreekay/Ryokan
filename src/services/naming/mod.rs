//! Configurable naming templates (issue #124).
//!
//! Three templates drive where an imported file lands:
//!
//! * `series_folder_format`, applied **once, at series-add time**. The
//!   result is persisted on `series.folder_name`; later template edits
//!   never rename existing folders.
//! * `season_folder_format`, applied on every import.
//! * `episode_file_format`, applied on every import. Must end with
//!   `{ext}` and must carry `{episode.number}`.
//!
//! The defaults reproduce the pre-#124 hardcoded layout byte for byte
//! (`<title>/Season 01/<title> - S01E07 - <episode title>.mkv`), so an
//! existing install sees no change until it edits a template.
//!
//! Tokens are `{name}` or `{name:00}` (zero-pad to the spec's width),
//! lowercase with dots, so parsing needs no case table and no
//! whitespace-inside-braces handling. Every token value is sanitized on
//! its own (`media::sanitize_folder_name`: path-illegal characters to
//! `_`, leading / trailing dots and whitespace trimmed), the pieces are
//! joined, and a structural cleanup drops what an empty value leaves
//! behind: empty bracket pairs, doubled ` - ` separators, dangling
//! separators at either end. The cleanup runs on the template's literal
//! text only, never inside a value, so a title containing `...` or `[]`
//! is left alone.
//!
//! [`render`] is pure and infallible for a template that passed
//! [`validate`]; import paths still fall back to the default template
//! if a stored template somehow fails, so a bad row can never stop an
//! import.

use std::sync::LazyLock;

use regex_lite::Regex;

use crate::models::series::Series;
use crate::services::anilist::AnimeEntry;
use crate::services::media::{parse_episode_number, parse_episode_span, sanitize_folder_name};

#[cfg(test)]
mod tests;

pub const DEFAULT_SERIES_FOLDER_FORMAT: &str = "{series.title}";
pub const DEFAULT_SEASON_FOLDER_FORMAT: &str = "Season {season.number:00}";
pub const DEFAULT_EPISODE_FILE_FORMAT: &str =
    "{series.title} - S{season.number:00}E{episode.number:00} - {episode.title}{ext}";

/// Longest single path component ext4 / NTFS accept (NAME_MAX).
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Windows' path ceiling without the long-path opt-in. A preview-time
/// warning only; Linux paths run to 4096 and work fine past 260.
pub const WINDOWS_MAX_PATH: usize = 260;

/// Which of the three templates a string is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateKind {
    SeriesFolder,
    SeasonFolder,
    EpisodeFile,
}

impl TemplateKind {
    pub fn default_template(self) -> &'static str {
        match self {
            TemplateKind::SeriesFolder => DEFAULT_SERIES_FOLDER_FORMAT,
            TemplateKind::SeasonFolder => DEFAULT_SEASON_FOLDER_FORMAT,
            TemplateKind::EpisodeFile => DEFAULT_EPISODE_FILE_FORMAT,
        }
    }

    /// Lowercase noun for error messages ("the series folder template").
    pub fn label(self) -> &'static str {
        match self {
            TemplateKind::SeriesFolder => "series folder",
            TemplateKind::SeasonFolder => "season folder",
            TemplateKind::EpisodeFile => "episode file",
        }
    }

    /// The form-field / JSON name (`series_folder`, ...).
    pub fn key(self) -> &'static str {
        match self {
            TemplateKind::SeriesFolder => "series_folder",
            TemplateKind::SeasonFolder => "season_folder",
            TemplateKind::EpisodeFile => "episode_file",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "series_folder" => Some(TemplateKind::SeriesFolder),
            "season_folder" => Some(TemplateKind::SeasonFolder),
            "episode_file" => Some(TemplateKind::EpisodeFile),
            _ => None,
        }
    }

    fn allows(self, token: Token) -> bool {
        match self {
            TemplateKind::SeriesFolder => {
                matches!(token, Token::SeriesTitle | Token::SeriesYear)
            }
            TemplateKind::SeasonFolder => matches!(
                token,
                Token::SeriesTitle | Token::SeriesYear | Token::SeasonNumber
            ),
            TemplateKind::EpisodeFile => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    SeriesTitle,
    SeriesYear,
    SeasonNumber,
    EpisodeNumber,
    EpisodeTitle,
    QualityFull,
    QualityResolution,
    QualitySource,
    Group,
    Ext,
}

/// `(token name, what it renders)` for the settings page reference.
pub const TOKEN_REFERENCE: &[(&str, &str)] = &[
    (
        "{series.title}",
        "series title in your preferred title language",
    ),
    ("{series.year}", "premiere year, empty when unknown"),
    (
        "{season.number}",
        "season number, always 1 today. {season.number:00} pads to 01",
    ),
    (
        "{episode.number}",
        "episode number. {episode.number:00} pads to 01, {episode.number:000} to 001. A file holding several episodes renders the range, like S01E05-E06 or 05-06",
    ),
    (
        "{episode.title}",
        "episode title from metadata, empty when unknown",
    ),
    (
        "{quality.full}",
        "resolution and source together, like 1080p WEB-DL or 1080p BluRay Remux",
    ),
    ("{quality.resolution}", "1080p, 720p, ..."),
    (
        "{quality.source}",
        "BluRay, BluRay Remux, WEB-DL, WEBRip, DVD, HDTV",
    ),
    ("{group}", "release group, empty when unknown"),
    (
        "{ext}",
        "the file extension with its dot. The episode file template must end with it",
    ),
];

impl Token {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "series.title" => Token::SeriesTitle,
            "series.year" => Token::SeriesYear,
            "season.number" => Token::SeasonNumber,
            "episode.number" => Token::EpisodeNumber,
            "episode.title" => Token::EpisodeTitle,
            "quality.full" => Token::QualityFull,
            "quality.resolution" => Token::QualityResolution,
            "quality.source" => Token::QualitySource,
            "group" => Token::Group,
            "ext" => Token::Ext,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Token::SeriesTitle => "series.title",
            Token::SeriesYear => "series.year",
            Token::SeasonNumber => "season.number",
            Token::EpisodeNumber => "episode.number",
            Token::EpisodeTitle => "episode.title",
            Token::QualityFull => "quality.full",
            Token::QualityResolution => "quality.resolution",
            Token::QualitySource => "quality.source",
            Token::Group => "group",
            Token::Ext => "ext",
        }
    }
}

enum Piece<'a> {
    Literal(&'a str),
    Token { token: Token, pad: usize },
}

/// Split a template into literal runs and tokens. Errors name the
/// problem the way the settings page shows it.
fn parse_template(template: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut pieces = Vec::new();
    let mut rest = template;
    while !rest.is_empty() {
        match rest.find(['{', '}']) {
            None => {
                pieces.push(Piece::Literal(rest));
                break;
            }
            Some(at) => {
                if at > 0 {
                    pieces.push(Piece::Literal(&rest[..at]));
                }
                if rest.as_bytes()[at] == b'}' {
                    return Err("a '}' has no matching '{'".to_string());
                }
                let after_open = &rest[at + 1..];
                let Some(close) = after_open.find('}') else {
                    return Err("a '{' is never closed".to_string());
                };
                let inner = &after_open[..close];
                if inner.contains('{') {
                    return Err("a '{' is never closed".to_string());
                }
                let (name, spec) = match inner.split_once(':') {
                    Some((n, s)) => (n.trim(), Some(s.trim())),
                    None => (inner.trim(), None),
                };
                let Some(token) = Token::parse(name) else {
                    return Err(format!("{{{}}} is not a known token", inner.trim()));
                };
                let pad = match spec {
                    None => 0,
                    Some(s) if !s.is_empty() && s.bytes().all(|b| b == b'0') => s.len(),
                    Some(s) => {
                        return Err(format!(
                            "{{{}:{}}} is not a supported format; use zeros to pad, like {{{}:00}}",
                            name, s, name
                        ));
                    }
                };
                pieces.push(Piece::Token { token, pad });
                rest = &after_open[close + 1..];
            }
        }
    }
    Ok(pieces)
}

/// The four title variants plus the premiere year, the inputs of every
/// `series.*` token. Built from a `Series` row, an AniList search
/// entry, or bare strings for the Sonarr / Radarr shims.
#[derive(Clone, Copy, Debug, Default)]
pub struct SeriesNames<'a> {
    pub title: &'a str,
    pub romaji: &'a str,
    pub english: &'a str,
    pub native: &'a str,
    pub year: Option<i32>,
}

impl<'a> SeriesNames<'a> {
    pub fn from_series(s: &'a Series) -> Self {
        Self {
            title: &s.title,
            romaji: &s.title_romaji,
            english: &s.title_english,
            native: &s.title_native,
            year: s.season_year,
        }
    }

    pub fn from_entry(e: &'a AnimeEntry) -> Self {
        Self {
            title: "",
            romaji: &e.title_romaji,
            english: &e.title_english,
            native: &e.title_native,
            year: e.season_year,
        }
    }

    /// The title `config.title_language` asks for, with the same
    /// fallback chain as `nfo::title_for_preference` so the file name
    /// and the NFO's `<showtitle>` always agree.
    pub fn preferred_title(&self, title_language: &str) -> String {
        let pick = |primary: &str, fallbacks: [&str; 3]| -> String {
            if !primary.is_empty() {
                return primary.to_string();
            }
            fallbacks
                .into_iter()
                .find(|f| !f.is_empty())
                .unwrap_or_default()
                .to_string()
        };
        match title_language {
            "romaji" => pick(self.romaji, [self.english, self.native, self.title]),
            "native" => pick(self.native, [self.english, self.romaji, self.title]),
            _ => pick(self.english, [self.romaji, self.native, self.title]),
        }
    }
}

/// Everything a template can ask for. Strings are the already-resolved
/// values (`series_title` is in the preferred language, `quality_*`
/// are composed labels); empty means "unknown" and renders as nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameContext {
    pub series_title: String,
    pub series_year: Option<i32>,
    pub season_number: i32,
    /// The first (usually only) episode the file holds.
    pub episode_number: i32,
    /// The last episode the file holds (issue #246). Zero or equal to
    /// `episode_number` for the ordinary one-episode file; larger for
    /// a multi-episode file, which renders `{episode.number}` as a
    /// range (`05-E06` after an `E`, `05-06` elsewhere) so the name
    /// parses back to the same span.
    pub episode_last: i32,
    pub episode_title: String,
    /// `1080p`, `720p`, ... or empty.
    pub quality_resolution: String,
    /// `BluRay`, `BluRay Remux`, `WEB-DL`, `WEBRip`, `WEB`, `DVD`,
    /// `HDTV`, `TV`, or empty. See [`quality_source_label`].
    pub quality_source: String,
    pub release_group: String,
    /// Without the dot.
    pub ext: String,
}

impl NameContext {
    /// Series-level context (folder templates): season 1, no episode.
    pub fn for_series(names: &SeriesNames<'_>, title_language: &str) -> Self {
        Self {
            series_title: names.preferred_title(title_language),
            series_year: names.year,
            season_number: 1,
            ..Default::default()
        }
    }

    /// True when the context names a multi-episode file.
    pub fn is_multi_episode(&self) -> bool {
        self.episode_last > self.episode_number
    }

    /// `S01E05` / `S01E05-E06`: the display-only slot label used by log
    /// lines and the last-resort fallback name.
    pub fn slot_label(&self) -> String {
        if self.is_multi_episode() {
            format!(
                "S{:02}E{:02}-E{:02}",
                self.season_number, self.episode_number, self.episode_last
            )
        } else {
            format!("S{:02}E{:02}", self.season_number, self.episode_number)
        }
    }
}

/// Compose the `{quality.source}` label from the structured pieces the
/// classifier and `episode_quality_tags` carry. `source` is the tag
/// row's / `Source::as_str` label (`BluRay`, `Web`, `DVD`, `HDTV`,
/// `TV`, `Unknown`); `web_kind` is `WEB-DL` / `WEBRip` / empty.
pub fn quality_source_label(source: &str, is_remux: bool, web_kind: &str) -> String {
    match source.to_ascii_lowercase().as_str() {
        "bluray" | "blu-ray" | "bd" => {
            if is_remux {
                "BluRay Remux".to_string()
            } else {
                "BluRay".to_string()
            }
        }
        "web" | "webdl" | "web-dl" | "webrip" => {
            if web_kind.is_empty() {
                "WEB".to_string()
            } else {
                web_kind.to_string()
            }
        }
        "dvd" => "DVD".to_string(),
        "hdtv" => "HDTV".to_string(),
        "tv" => "TV".to_string(),
        _ => String::new(),
    }
}

/// `{quality.full}`: resolution and source joined, either alone when
/// the other is unknown.
pub fn quality_full(resolution: &str, source: &str) -> String {
    match (resolution.is_empty(), source.is_empty()) {
        (true, true) => String::new(),
        (false, true) => resolution.to_string(),
        (true, false) => source.to_string(),
        (false, false) => format!("{resolution} {source}"),
    }
}

/// A rendered name plus whether NAME_MAX forced a truncation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub name: String,
    pub truncated: bool,
}

/// A rendered episode file name and the stem the `.nfo` companion
/// shares with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeName {
    pub file_name: String,
    pub stem: String,
    pub truncated: bool,
}

enum Segment {
    Literal(String),
    Value(String),
}

static RE_EMPTY_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\s*\]|\(\s*\)|\{\s*\}").expect("RE_EMPTY_BRACKETS compiles"));
static RE_SEPARATOR_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+-\s+(?:-\s+)+").expect("RE_SEPARATOR_RUN compiles"));
static RE_DOT_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.{2,}").expect("RE_DOT_RUN compiles"));
static RE_UNDERSCORE_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_{2,}").expect("RE_UNDERSCORE_RUN compiles"));
static RE_SPACE_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s{2,}").expect("RE_SPACE_RUN compiles"));

/// Characters that count as a dangling separator at either end.
const EDGE_SEPARATORS: &[char] = &[' ', '-', '.', '_'];

/// Cleanup of one literal run after empty values were dropped around
/// it. Loops because one removal can expose the next (`[ - ]` needs
/// the separator gone before the brackets read as empty).
fn clean_literal(text: &str) -> String {
    let mut s = text.to_string();
    loop {
        let before = s.clone();
        s = RE_EMPTY_BRACKETS.replace_all(&s, "").into_owned();
        s = RE_SEPARATOR_RUN.replace_all(&s, " - ").into_owned();
        s = RE_DOT_RUN.replace_all(&s, ".").into_owned();
        s = RE_UNDERSCORE_RUN.replace_all(&s, "_").into_owned();
        s = RE_SPACE_RUN.replace_all(&s, " ").into_owned();
        if s == before {
            return s;
        }
    }
}

/// `{episode.number}` for a multi-episode file (issue #246):
/// `05-E06` when the template's literal text right before the token
/// ends with `E` (the `S01E05-E06` shape, Sonarr's prefixed range), else
/// `05-06` (` - 05-06`, the dash-slot range). Both are shapes
/// `media::parse_episode_span` reads back; the bare `05-06` after an
/// `E` would parse too, but the prefixed form is what Sonarr, Jellyfin
/// and Plex all document. `pad` applies to both numbers; the letter
/// copies the literal's case.
fn episode_range_value(pad: usize, ctx: &NameContext, preceding_literal: Option<&str>) -> String {
    let number = |n: i32| -> String {
        if pad > 0 {
            format!("{n:0pad$}")
        } else {
            n.to_string()
        }
    };
    let prefix = preceding_literal
        .and_then(|lit| lit.chars().last())
        .filter(|c| c.eq_ignore_ascii_case(&'e'))
        .map(|c| c.to_string())
        .unwrap_or_default();
    format!(
        "{}-{}{}",
        number(ctx.episode_number),
        prefix,
        number(ctx.episode_last)
    )
}

fn token_value(token: Token, pad: usize, ctx: &NameContext) -> String {
    let number = |n: i32| -> String {
        if pad > 0 {
            format!("{n:0pad$}")
        } else {
            n.to_string()
        }
    };
    let text = match token {
        Token::SeriesTitle => ctx.series_title.clone(),
        Token::SeriesYear => ctx.series_year.map(number).unwrap_or_default(),
        Token::SeasonNumber => number(ctx.season_number),
        Token::EpisodeNumber => number(ctx.episode_number),
        Token::EpisodeTitle => ctx.episode_title.clone(),
        Token::QualityFull => quality_full(&ctx.quality_resolution, &ctx.quality_source),
        Token::QualityResolution => ctx.quality_resolution.clone(),
        Token::QualitySource => ctx.quality_source.clone(),
        Token::Group => ctx.release_group.clone(),
        // Handled outside the stem; never substituted inline.
        Token::Ext => String::new(),
    };
    sanitize_folder_name(&text)
}

/// Substitute, drop empties, clean the literal runs, join, sanitize.
fn render_stem(pieces: &[Piece<'_>], ctx: &NameContext) -> String {
    // Merge adjacent literals once empty values fall out, so the
    // cleanup sees `[` + `` + `]` as one `[]`.
    let mut segments: Vec<Segment> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let next = match piece {
            Piece::Literal(text) => Segment::Literal((*text).to_string()),
            Piece::Token {
                token: Token::EpisodeNumber,
                pad,
            } if ctx.is_multi_episode() => {
                let preceding = match segments.last() {
                    Some(Segment::Literal(text)) => Some(text.as_str()),
                    _ => None,
                };
                Segment::Value(sanitize_folder_name(&episode_range_value(
                    *pad, ctx, preceding,
                )))
            }
            Piece::Token { token, pad } => {
                let value = token_value(*token, *pad, ctx);
                if value.is_empty() {
                    continue;
                }
                Segment::Value(value)
            }
        };
        match (segments.last_mut(), next) {
            (Some(Segment::Literal(prev)), Segment::Literal(text)) => prev.push_str(&text),
            (_, seg) => segments.push(seg),
        }
    }

    let last = segments.len().saturating_sub(1);
    let mut out = String::new();
    for (i, seg) in segments.into_iter().enumerate() {
        match seg {
            Segment::Value(v) => out.push_str(&v),
            Segment::Literal(text) => {
                let mut cleaned = clean_literal(&text);
                if i == 0 {
                    cleaned = cleaned.trim_start_matches(EDGE_SEPARATORS).to_string();
                }
                if i == last {
                    cleaned = cleaned.trim_end_matches(EDGE_SEPARATORS).to_string();
                }
                out.push_str(&cleaned);
            }
        }
    }
    sanitize_folder_name(&out)
}

/// Cut `value` down by `excess` bytes (plus room for the ellipsis) at a
/// char boundary. Empty when nothing sensible is left.
fn shrink(value: &str, excess: usize) -> String {
    const ELLIPSIS: &str = "\u{2026}";
    let mut target = value.len().saturating_sub(excess + ELLIPSIS.len());
    while target > 0 && !value.is_char_boundary(target) {
        target -= 1;
    }
    if target == 0 {
        return String::new();
    }
    let mut cut = value[..target].trim_end().to_string();
    cut.push_str(ELLIPSIS);
    cut
}

fn cut_to_bytes(value: &str, max: usize) -> String {
    let mut end = max.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].trim_end().to_string()
}

/// Render `template` for `ctx`. Errors only on template syntax (the
/// cases [`validate`] rejects) or when the result is empty.
pub fn render(kind: TemplateKind, template: &str, ctx: &NameContext) -> Result<Rendered, String> {
    let pieces = parse_template(template.trim())?;
    let ext_suffix = match kind {
        TemplateKind::EpisodeFile if !ctx.ext.is_empty() => format!(".{}", ctx.ext),
        _ => String::new(),
    };
    let budget = MAX_COMPONENT_BYTES.saturating_sub(ext_suffix.len());

    let mut stem = render_stem(&pieces, ctx);
    let mut truncated = false;
    if stem.len() > budget {
        // NAME_MAX: shrink the series title first (the long-string
        // offender in practice), then the episode title, then hard-cut.
        // Episode number and extension carry information nothing else
        // does and are never touched.
        truncated = true;
        let mut shrunk = ctx.clone();
        shrunk.series_title = shrink(&ctx.series_title, stem.len() - budget);
        stem = render_stem(&pieces, &shrunk);
        if stem.len() > budget {
            shrunk.episode_title = shrink(&ctx.episode_title, stem.len() - budget);
            stem = render_stem(&pieces, &shrunk);
        }
        if stem.len() > budget {
            stem = sanitize_folder_name(&cut_to_bytes(&stem, budget));
        }
    }
    if stem.is_empty() {
        return Err(format!(
            "the {} template renders to an empty name",
            kind.label()
        ));
    }
    Ok(Rendered {
        name: format!("{stem}{ext_suffix}"),
        truncated,
    })
}

/// The series folder for a new series: the template, falling back to
/// the default when the stored template is unusable.
pub fn series_folder(template: &str, title_language: &str, names: &SeriesNames<'_>) -> String {
    let ctx = NameContext::for_series(names, title_language);
    render_or_default(TemplateKind::SeriesFolder, template, &ctx).name
}

/// The season folder under a series folder.
pub fn season_folder(
    template: &str,
    title_language: &str,
    names: &SeriesNames<'_>,
    season_number: i32,
) -> String {
    let mut ctx = NameContext::for_series(names, title_language);
    ctx.season_number = season_number;
    render_or_default(TemplateKind::SeasonFolder, template, &ctx).name
}

/// The episode file name plus the stem its `.nfo` shares.
pub fn episode_file(template: &str, ctx: &NameContext) -> EpisodeName {
    let rendered = render_or_default(TemplateKind::EpisodeFile, template, ctx);
    let stem = if ctx.ext.is_empty() {
        rendered.name.clone()
    } else {
        rendered
            .name
            .strip_suffix(&format!(".{}", ctx.ext))
            .unwrap_or(&rendered.name)
            .to_string()
    };
    EpisodeName {
        file_name: rendered.name,
        stem,
        truncated: rendered.truncated,
    }
}

/// `render`, then the default template, then a bare `S01E07` shape:
/// an import never fails on naming.
pub fn render_or_default(kind: TemplateKind, template: &str, ctx: &NameContext) -> Rendered {
    let template = if template.trim().is_empty() {
        kind.default_template()
    } else {
        template
    };
    if let Ok(r) = render(kind, template, ctx) {
        return r;
    }
    if let Ok(r) = render(kind, kind.default_template(), ctx) {
        return r;
    }
    let name = match kind {
        TemplateKind::SeriesFolder => "Unknown Series".to_string(),
        TemplateKind::SeasonFolder => format!("Season {:02}", ctx.season_number),
        TemplateKind::EpisodeFile => {
            let stem = ctx.slot_label();
            if ctx.ext.is_empty() {
                stem
            } else {
                format!("{stem}.{}", ctx.ext)
            }
        }
    };
    Rendered {
        name,
        truncated: false,
    }
}

/// The context the settings preview and [`validate`] render with. A
/// real title with no digits, so the parse-back check below can tell
/// the episode number from the title.
pub fn sample_context() -> NameContext {
    NameContext {
        series_title: "Sousou no Frieren".to_string(),
        series_year: Some(2023),
        season_number: 1,
        episode_number: 7,
        episode_last: 7,
        episode_title: "Like a Fairy Tale".to_string(),
        quality_resolution: "1080p".to_string(),
        quality_source: "WEB-DL".to_string(),
        release_group: "SubsPlease".to_string(),
        ext: "mkv".to_string(),
    }
}

/// The sample with every optional value missing: the shape a batch
/// import of a show AniList has no episode titles for produces.
pub fn sparse_context() -> NameContext {
    NameContext {
        series_title: "Sousou no Frieren".to_string(),
        series_year: None,
        season_number: 1,
        episode_number: 7,
        ext: "mkv".to_string(),
        ..Default::default()
    }
}

/// The sample as a two-episode file (issue #246): what the settings
/// page shows under the multi-episode note, and what [`validate`]
/// parses back to make sure the range survives the template.
pub fn multi_episode_sample_context() -> NameContext {
    NameContext {
        episode_last: 8,
        episode_title: "Like a Fairy Tale + The Land Where the Soul Rests".to_string(),
        ..sample_context()
    }
}

/// Server-side validation, the load-bearing half (the page's live
/// preview calls the same function). Every rejection is a sentence the
/// settings page shows next to the field.
pub fn validate(kind: TemplateKind, template: &str) -> Result<(), String> {
    let label = kind.label();
    if template.trim().is_empty() {
        return Err(format!("The {label} template is empty."));
    }
    if template.contains('/') || template.contains('\\') {
        return Err(format!(
            "The {label} template cannot contain / or \\. The layout is always series folder, season folder, file."
        ));
    }
    let template = template.trim();
    let pieces = parse_template(template).map_err(|e| format!("The {label} template: {e}."))?;

    let mut has_episode_number = false;
    let mut ext_positions = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        if let Piece::Token { token, .. } = piece {
            if *token == Token::Ext && kind != TemplateKind::EpisodeFile {
                return Err(format!(
                    "{{ext}} only belongs in the episode file template, not the {label} template."
                ));
            }
            if !kind.allows(*token) {
                return Err(format!(
                    "{{{}}} is not available in the {label} template.",
                    token.name()
                ));
            }
            match token {
                Token::EpisodeNumber => has_episode_number = true,
                Token::Ext => ext_positions.push(i),
                _ => {}
            }
        }
    }
    if kind == TemplateKind::EpisodeFile {
        // Last piece, exactly once. The piece check already covers a
        // padded `{ ext }`, which parses to the same token.
        let ends_with_ext = ext_positions.len() == 1 && ext_positions[0] == pieces.len() - 1;
        if !ends_with_ext {
            return Err(
                "The episode file template must end with {ext}, and use it only once.".to_string(),
            );
        }
        if !has_episode_number {
            return Err("The episode file template must include {episode.number}.".to_string());
        }
    }

    let sample = render(kind, template, &sample_context())
        .map_err(|_| format!("The {label} template renders to an empty name."))?;
    let sparse = render(kind, template, &sparse_context()).map_err(|_| {
        format!(
            "The {label} template renders to an empty name when the optional details (year, episode title, quality, group) are missing."
        )
    })?;

    if kind == TemplateKind::EpisodeFile {
        for name in [&sample.name, &sparse.name] {
            let parsed = parse_episode_number(&name.to_lowercase());
            let ok = matches!(parsed, Some((season, 7)) if season.is_none_or(|s| s == 1));
            if !ok {
                return Err(format!(
                    "Ryokan cannot read the episode number back from '{}'. Keep S{{season.number:00}}E{{episode.number:00}} or ' - {{episode.number:00}}' in the template so library scans and upgrades still find the file.",
                    name
                ));
            }
        }
        // A file holding two episodes must read back as both (issue
        // #246), or the library would count the second one missing and
        // grab it again.
        let multi = render(kind, template, &multi_episode_sample_context())
            .map_err(|_| format!("The {label} template renders to an empty name."))?;
        let span = parse_episode_span(&multi.name.to_lowercase());
        let ok =
            span.is_some_and(|s| s.first == 7 && s.last == 8 && s.season.is_none_or(|n| n == 1));
        if !ok {
            return Err(format!(
                "Ryokan cannot read the episode range back from '{}'. A file that holds several episodes is named with a range like S01E07-E08. Keep S{{season.number:00}}E{{episode.number:00}} or ' - {{episode.number:00}}' in the template so both episodes count as present.",
                multi.name
            ));
        }
    }
    Ok(())
}

/// Validate, then render the sample: what the settings page shows under
/// each field.
pub fn preview(kind: TemplateKind, template: &str) -> Result<Rendered, String> {
    validate(kind, template)?;
    render(kind, template, &sample_context())
}
