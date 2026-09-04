//! Title-normalization, alias collection, and target-matching.
//!
//! The query builder turns an `AnimeDetail` into a set of alias strings,
//! then `matches_target` filters Nyaa releases down to those that plausibly
//! describe the same series. Sibling-rejection keeps sequels / prequels /
//! arcs from false-positive matching via `SiblingRejectPrecompute` +
//! `sibling_match_rejects`.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex_lite::Regex;

use crate::services::anilist::AnimeDetail;

use super::pack_detection::is_pack_candidate_relation;
use super::provenance::{AliasMatch, MatchKind};
use super::{SearchTarget, episode_match, parse_release_numbers, season_mismatch};

pub fn normalize_title(input: &str) -> String {
    let lower = input.to_lowercase();
    let mut cleaned = String::with_capacity(lower.len());
    let mut in_brackets = 0i32;

    for ch in lower.chars() {
        match ch {
            '[' | '(' | '{' => in_brackets += 1,
            ']' | ')' | '}' => in_brackets = (in_brackets - 1).max(0),
            _ if in_brackets > 0 => continue,
            _ if ch.is_alphanumeric() || ch.is_whitespace() => cleaned.push(ch),
            _ => cleaned.push(' '),
        }
    }

    cleaned
        .split_whitespace()
        .filter(|token| {
            // Universal release-side noise tokens — same shape as
            // resolution/codec markers. Container extensions (mkv /
            // mp4 / mka) survive the bracket strip because a `.` is
            // converted to whitespace, leaving the extension as a
            // bare token. They're release metadata, not content;
            // dropping them frees the 1-token-alias surplus budget
            // (issue #103) for legitimate variants like `Episode`
            // markers and `v2` revisions.
            !matches!(
                *token,
                "1080p"
                    | "720p"
                    | "2160p"
                    | "webrip"
                    | "web"
                    | "bluray"
                    | "aac"
                    | "hevc"
                    | "x265"
                    | "x264"
                    | "dual"
                    | "audio"
                    | "multisub"
                    | "mkv"
                    | "mp4"
                    | "mka"
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn token_set(value: &str) -> HashSet<String> {
    value
        .split_whitespace()
        // Keep single-character tokens only when they're numeric —
        // single digits like "0" in "Jujutsu Kaisen 0" are the only
        // thing distinguishing the prequel movie's sibling alias from
        // the base franchise's own alias "Jujutsu Kaisen", so dropping
        // them lets sibling_match_rejects tie on tokens and fail to
        // reject the movie release for an S1 episode target. Single
        // alphabetic characters (stray "a", "I", "N") remain filtered
        // out because they carry no disambiguation value.
        .filter(|token| token.len() > 1 || token.chars().all(|c| c.is_ascii_digit()))
        .map(|token| token.to_string())
        .collect()
}

pub fn token_overlap_ratio(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let common = a.intersection(b).count() as f32;
    common / b.len() as f32
}

/// Tokens that appear in so many anime titles that they say nothing
/// about *which* title a release belongs to: English function words,
/// romaji particles, and format nouns. Issue #219 — the colon-split
/// alias `Risa THE ANIMATION` used to clear the 0.6 overlap gate on
/// `the` + `animation` alone, so every "The Animation" release an
/// indexer returned matched an unrelated adult OVA. The fuzzy overlap
/// path now counts only the distinctive tokens; the verbatim-substring
/// path is unchanged.
const GENERIC_TITLE_TOKENS: &[&str] = &[
    // English function words.
    "the",
    "a",
    "an",
    "of",
    "and",
    "in",
    "on",
    "at",
    "to",
    "for",
    "with",
    "from",
    "by",
    "vs",
    "or",
    // Romaji particles.
    "no",
    "wa",
    "ga",
    "ni",
    "wo",
    "de",
    "mo",
    "ka",
    "ne",
    "yo",
    // Format / packaging nouns.
    "animation",
    "anime",
    "movie",
    "film",
    "series",
    "season",
    "seasons",
    "part",
    "ova",
    "oad",
    "ona",
    "tv",
    "special",
    "specials",
    "episode",
    "episodes",
];

pub fn is_generic_title_token(token: &str) -> bool {
    GENERIC_TITLE_TOKENS.contains(&token)
}

/// Share of the alias's *distinctive* tokens (see
/// `GENERIC_TITLE_TOKENS`) that appear in the title. An alias made of
/// nothing but generic tokens (`The Movie`) has no distinctive signal
/// and scores `0.0`, so only the verbatim-substring path can match it.
pub fn distinctive_overlap_ratio(
    title_tokens: &HashSet<String>,
    alias_tokens: &HashSet<String>,
) -> f32 {
    let distinctive: HashSet<String> = alias_tokens
        .iter()
        .filter(|t| !is_generic_title_token(t))
        .cloned()
        .collect();
    if distinctive.is_empty() {
        return 0.0;
    }
    token_overlap_ratio(title_tokens, &distinctive)
}

/// Fold a series row's alternate titles into the detail's synonyms so
/// every alias builder (search gate, RSS, misgrab verdict) sees them.
pub fn with_alternate_titles(mut detail: AnimeDetail, raw: &str) -> AnimeDetail {
    for title in crate::models::series::parse_alternate_titles(raw) {
        if !detail
            .synonyms
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&title))
        {
            detail.synonyms.push(title);
        }
    }
    detail
}

pub fn collect_aliases(detail: &AnimeDetail) -> Vec<String> {
    dedupe_strings(vec![
        detail.title_romaji.clone(),
        detail.title_english.clone(),
        detail.title_native.clone(),
    ])
}

// ── Sequel / part variant aliases (issue #84) ─────────────────────────────
//
// Motivating bug: auto-searching a sequel cour or a numbered movie entry
// often produces zero results on Nyaa even when a canonical release exists,
// because Nyaa's search is AND-tokenized across the query. An AL alias like
// `Sono Bisque Doll wa Koi wo Suru 2nd Season` forces every returned title
// to carry the tokens {2nd, season}, which shuts out MiniMTBB/YURASUKA/
// Okay-Subs releases that name the same cour `S2` or `S02`. Same shape for
// movie trilogies: AL says `Kizumonogatari II: Nekketsu-hen`; MTBB names
// their files `Kizumonogatari - 02`. Nyaa never unions these.
//
// The fix: detect a sequel/part marker in the alias, extract a franchise
// base, and emit three synthetic aliases covering the release-group
// conventions actually observed in the wild (`S{N}`, `S{NN}`, `- {NN}`).
// These variants feed into BOTH the query-generation side (so Nyaa sees
// queries that actually match the groups' titling) and the alias list
// passed to `matches_target` (so a release matching via the variant isn't
// rejected by the token-overlap filter).
//
// The variant set is kept to three forms specifically because every
// variant is a net new HTTP round-trip per sweep — see mod.rs's
// `build_queries_mixed` for the canonical-full / variant-collapsed
// query shape that keeps the per-sweep query count bounded.
//
// Sibling rejection remains the safety net against cross-cour false
// positives — if base = "Kizumonogatari" and we find `Kizumonogatari -
// 01`, that release would match our Part 2 variant alias but the sibling
// list includes Parts 1 and 3, and the episode-number parser still has to
// see `02` on the filename for Part 2 to win.

// `\b.*$` (word-boundary + arbitrary trailing) instead of `\s*$`
// (strict end-of-string) so romaji titles that pack the cour's arc
// name AFTER the season marker still produce sequel variants. AL ships
// `Youkoso Jitsuryoku Shijou Shugi no Kyoushitsu e 4th Season
// 2-nensei-hen Ichi Gakki` for COTE S4 — release groups drop the
// arc descriptor and just publish `[SubsPlease] Youkoso ... S4 - 06`.
// The pre-fix anchor required the marker to be the LAST tokens of
// the alias, so COTE-shape titles produced no S4/-04 variants and
// the search short-circuited with no matches. `\b` is load-bearing
// against `Seasonal` accidentally matching as `Season`+`al`.
static RE_ORDINAL_SEASON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(.+?)\s+(\d+)(?:st|nd|rd|th)\s+Season\b.*$").unwrap());
static RE_WORD_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(.+?)\s+(First|Second|Third|Fourth|Fifth|Sixth)\s+Season\b.*$").unwrap()
});
static RE_SEASON_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(.+?)\s+Season\s+(\d+)\b.*$").unwrap());
static RE_S_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(.+?)\s+S(\d{1,2})\b.*$").unwrap());
static RE_PART_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(.+?)\s+Part\s+(\d+)(?::\s+.+)?\s*$").unwrap());
static RE_PART_ROMAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(.+?)\s+Part\s+(II|III|IV|V|VI|VII|VIII|IX|X)(?::\s+.+)?\s*$").unwrap()
});
// Roman numeral at the tail, optionally followed by `: subtitle`. Excludes
// plain "I" — a single trailing letter is too often an initial (e.g.
// `Magical Girl Lyrical Nanoha A's`; less extreme, `Slam Dunk I`-style
// false matches on a short initial). The II..X range is what trilogy /
// tetralogy naming actually uses.
static RE_ROMAN_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(.+?)\s+(II|III|IV|V|VI|VII|VIII|IX|X)(?::\s+.+)?\s*$").unwrap()
});

fn ordinal_word_to_n(word: &str) -> Option<u32> {
    match word.to_ascii_lowercase().as_str() {
        "first" => Some(1),
        "second" => Some(2),
        "third" => Some(3),
        "fourth" => Some(4),
        "fifth" => Some(5),
        "sixth" => Some(6),
        _ => None,
    }
}

fn roman_to_n(roman: &str) -> Option<u32> {
    match roman.to_ascii_uppercase().as_str() {
        "I" => Some(1),
        "II" => Some(2),
        "III" => Some(3),
        "IV" => Some(4),
        "V" => Some(5),
        "VI" => Some(6),
        "VII" => Some(7),
        "VIII" => Some(8),
        "IX" => Some(9),
        "X" => Some(10),
        _ => None,
    }
}

fn n_to_roman(n: u32) -> Option<&'static str> {
    match n {
        1 => Some("I"),
        2 => Some("II"),
        3 => Some("III"),
        4 => Some("IV"),
        5 => Some("V"),
        6 => Some("VI"),
        7 => Some("VII"),
        8 => Some("VIII"),
        9 => Some("IX"),
        10 => Some("X"),
        _ => None,
    }
}

/// Parse the alias's tail for a sequel/part marker and return the
/// franchise base and position number. Only the marker at the very end
/// of the alias is recognized (optionally followed by `: subtitle` for
/// the part/roman conventions that carry a sub-title), so mid-string
/// false positives like `Persona 5 the Animation` don't trip `RE_S_N`.
fn extract_sequel_position(alias: &str) -> Option<(String, u32)> {
    // Ordinal-season and word-season come first because they strictly
    // embed the position number — matching them is unambiguous.
    if let Some(cap) = RE_ORDINAL_SEASON.captures(alias) {
        let base = cap.get(1)?.as_str().trim();
        let n: u32 = cap.get(2)?.as_str().parse().ok()?;
        if !base.is_empty() && (1..=20).contains(&n) {
            return Some((base.to_string(), n));
        }
    }
    if let Some(cap) = RE_WORD_SEASON.captures(alias) {
        let base = cap.get(1)?.as_str().trim();
        let n = ordinal_word_to_n(cap.get(2)?.as_str())?;
        if !base.is_empty() {
            return Some((base.to_string(), n));
        }
    }
    if let Some(cap) = RE_SEASON_N.captures(alias) {
        let base = cap.get(1)?.as_str().trim();
        let n: u32 = cap.get(2)?.as_str().parse().ok()?;
        if !base.is_empty() && (1..=20).contains(&n) {
            return Some((base.to_string(), n));
        }
    }
    if let Some(cap) = RE_S_N.captures(alias) {
        let base = cap.get(1)?.as_str().trim();
        let n: u32 = cap.get(2)?.as_str().parse().ok()?;
        if !base.is_empty() && (1..=20).contains(&n) {
            return Some((base.to_string(), n));
        }
    }
    if let Some(cap) = RE_PART_N.captures(alias) {
        let base = cap.get(1)?.as_str().trim();
        let n: u32 = cap.get(2)?.as_str().parse().ok()?;
        if !base.is_empty() && (1..=20).contains(&n) {
            return Some((base.to_string(), n));
        }
    }
    if let Some(cap) = RE_PART_ROMAN.captures(alias) {
        let base = cap.get(1)?.as_str().trim();
        let n = roman_to_n(cap.get(2)?.as_str())?;
        if !base.is_empty() {
            return Some((base.to_string(), n));
        }
    }
    if let Some(cap) = RE_ROMAN_TAIL.captures(alias) {
        let base = cap.get(1)?.as_str().trim();
        let n = roman_to_n(cap.get(2)?.as_str())?;
        // RE_ROMAN_TAIL only captures II..X, so n ≥ 2 here.
        if !base.is_empty() {
            return Some((base.to_string(), n));
        }
    }
    None
}

/// Distinctiveness guardrail for sequel-variant generation. A base that's
/// one short word (`Gundam`) would produce variants that substring-match
/// every unrelated Gundam entry, so those bases are rejected — the
/// sibling-rejection precompute still catches most cross-hit cases, but
/// keeping generic variants out of the query list is cheaper than
/// chasing every false-positive that flows through sibling rejection.
fn is_distinctive_base(base: &str) -> bool {
    let normalized = normalize_title(base);
    let token_count = normalized.split_whitespace().count();
    if token_count >= 2 {
        return true;
    }
    // Single-token base: require at least 7 characters. Picks up
    // `Overlord` (8), `Danmachi` (8), `Kizumonogatari` (14),
    // `Monogatari` (10); still rejects `Gundam` (6), `Naruto` (6),
    // `Bleach` (6) — the 6-char generic franchises where a bare
    // `{name} S2` would substring-match several unrelated entries.
    normalized.chars().count() >= 7
}

/// Generate synthetic alias variants for sequel / part markers. Given an
/// alias carrying a season or part marker, emits the four release-group
/// shorthand forms that Nyaa's AND-tokenized search uses: `S{N}`, `S{NN}`,
/// `- {NN}`, and `{base} {roman}`. Feeds into both query generation (so
/// Nyaa returns the groups' shorthand titles) and `matches_target`'s
/// alias list (so the shorthand titles survive the token-overlap filter).
///
/// Each variant is a **net new HTTP round-trip** per sweep because queries
/// run sequentially through `nyaa::search`, so the variant set is kept
/// deliberately small. Covered conventions:
/// * `S{N}` / `S{NN}` — MTBB/MiniMTBB/Okay-Subs/YURASUKA.
/// * `- {NN}` — movie trilogies (Kizumonogatari) and episode-style batches.
/// * `{base} {roman}` — Overlord IV / Date A Live IV / Danmachi IV and
///   every other franchise whose releases carry the Roman numeral in
///   the filename. Only fires when the AL canonical alias does NOT
///   already contain that Roman form; if AL already gives us `Overlord
///   IV`, the canonical query covers the Roman-numbered release and the
///   variant would just duplicate it.
///
/// Returns an empty Vec when no input alias carries a recognizable
/// marker, or when every candidate base fails the distinctiveness gate.
pub fn sequel_variant_aliases(aliases: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for alias in aliases {
        let Some((base, pos)) = extract_sequel_position(alias) else {
            continue;
        };
        if !is_distinctive_base(&base) {
            continue;
        }
        out.push(format!("{} S{}", base, pos));
        out.push(format!("{} S{:02}", base, pos));
        out.push(format!("{} - {:02}", base, pos));
        if let Some(roman) = n_to_roman(pos) {
            // Skip when the canonical alias this variant came from
            // already ends in `{base} {roman}` — common for Overlord-
            // style AL titles where the Roman numeral IS the marker.
            // No duplicate query emitted, but variants from aliases
            // with a different marker convention (e.g. `Overlord
            // Season 4` → `Overlord IV`) still fire.
            let duplicate = alias
                .to_ascii_lowercase()
                .trim_end()
                .ends_with(&format!(" {}", roman.to_ascii_lowercase()));
            if !duplicate {
                out.push(format!("{} {}", base, roman));
            }
        }
    }
    dedupe_strings(out)
}

/// Distinctive titles of this series' siblings (sequels, prequels, side
/// stories, alternative versions, spin-offs, summaries) — used to reject
/// releases that look MORE like a sibling than the target.
///
/// The motivating bug: auto-searching for Jujutsu Kaisen S1 E6 grabbed a
/// release titled `[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen -
/// 06`, which is actually an S2/S3 arc. The existing season_mismatch()
/// heuristic only catches explicit `S02` / `Season 2` markers; an arc
/// title like "Shimetsu Kaiyuu" slips through. But AniList knows that
/// "Jujutsu Kaisen: Shimetsu Kaiyuu" is a SEQUEL of JJK S1 — so we can
/// use the relation graph to derive the distinctive tokens that, when
/// present in a release filename, mean "this is the sibling, not me".
///
/// Returns sibling titles only where the sibling's normalized title is
/// NOT a substring of any of this target's own aliases (otherwise the
/// sibling title would match the target too — e.g. a prequel sharing
/// the base franchise name is not a useful discriminator). The returned
/// titles are still raw (un-normalized) so the matching logic can
/// re-normalize them the same way it does the release title.
pub fn collect_sibling_aliases(detail: &AnimeDetail, own_aliases: &[String]) -> Vec<String> {
    if detail.id <= 0 || detail.relations.is_empty() {
        return Vec::new();
    }

    // Normalized own-alias set — used to filter out sibling titles that
    // are themselves substrings of one of our own aliases (those would
    // substring-match us too, so they're not distinctive).
    let normalized_own: Vec<String> = own_aliases
        .iter()
        .map(|a| normalize_title(a))
        .filter(|s| !s.is_empty())
        .collect();

    let mut out: Vec<String> = Vec::new();
    for rel in &detail.relations {
        if !rel.media_type.eq_ignore_ascii_case("ANIME") {
            continue;
        }
        if !is_pack_candidate_relation(&rel.relation_type) {
            continue;
        }
        // Consider all three title fields so romaji-only or native-only
        // titles still contribute. The de-dup below squashes repeats.
        for raw in [
            rel.title_english.as_str(),
            rel.title_romaji.as_str(),
            rel.title_native.as_str(),
        ] {
            if raw.is_empty() {
                continue;
            }
            let normalized = normalize_title(raw);
            // Need ≥ 2 tokens for the sibling title to be a meaningful
            // discriminator — a single token is too generic and will
            // false-positive on unrelated releases that happen to share
            // a common word.
            if normalized.split_whitespace().count() < 2 {
                continue;
            }
            // Skip sibling titles whose normalized form is a substring
            // of one of our own aliases — those can't tell us apart
            // from the target.
            if normalized_own.iter().any(|own| own.contains(&normalized)) {
                continue;
            }
            out.push(raw.to_string());
        }
    }
    dedupe_strings(out)
}

/// Precomputed normalized token sets for the own-alias and sibling-alias
/// lists used by [`sibling_match_rejects`]. Built once per target sweep
/// (per call to `find_all_for_target` / `collect_scored_for_target` /
/// `collect_scored_batches_for_target`) and reused across every release
/// candidate the sweep checks against the target, instead of re-running
/// `normalize_title` + `token_set` on the same alias strings ~50×
/// (candidates) per target. Pure perf hoist — the rejection semantics
/// are identical to the prior per-call implementation.
#[derive(Debug, Clone, Default)]
pub struct SiblingRejectPrecompute {
    /// Token sets for own aliases. Used to find the best target-alias
    /// overlap with any release — a sibling only wins if it beats this
    /// number strictly.
    own_token_sets: Vec<HashSet<String>>,
    /// Sibling entries as `(normalized_title, token_set)` pairs. The
    /// normalized title is kept alongside its token set so the
    /// contiguous-substring fallback has a stable, deterministic string
    /// to match against (the old implementation rebuilt this from
    /// `HashSet::iter()` per call, which is nondeterministic order and
    /// would silently misbehave on contiguous-substring checks).
    siblings: Vec<(String, HashSet<String>)>,
}

impl SiblingRejectPrecompute {
    pub fn build(own_aliases: &[String], sibling_aliases: &[String]) -> Self {
        let own_token_sets = own_aliases
            .iter()
            .map(|a| token_set(&normalize_title(a)))
            .collect();
        let siblings = sibling_aliases
            .iter()
            .filter_map(|s| {
                let normalized = normalize_title(s);
                let tokens = token_set(&normalized);
                if tokens.is_empty() {
                    None
                } else {
                    Some((normalized, tokens))
                }
            })
            .collect();
        Self {
            own_token_sets,
            siblings,
        }
    }
}

/// Reject a release when it looks MORE like one of our siblings than
/// it does like us. The check compares token overlap: if any sibling
/// alias shares strictly more tokens with the release than the best
/// target alias does, the release is for the sibling.
///
/// Returns `true` to reject, `false` to keep.
///
/// Called from `matches_target` and the interactive-search path. Both
/// are guarded by an upstream basic alias-match, so by the time we get
/// here the release already passes the "could plausibly be us" gate —
/// the sibling check is the last defense against "plausibly us" also
/// being "more plausibly a sibling".
pub(super) fn sibling_match_rejects(
    normalized_release: &str,
    normalized_release_tokens: &HashSet<String>,
    precompute: &SiblingRejectPrecompute,
) -> bool {
    if precompute.siblings.is_empty() {
        return false;
    }

    // Best token overlap COUNT between release and any of our own aliases.
    // Using absolute overlap count (not ratio) so a sibling with 4 matching
    // tokens beats a target alias with 2 matching tokens even if the target
    // alias has fewer tokens overall.
    let best_own_overlap: usize = precompute
        .own_token_sets
        .iter()
        .map(|tokens| normalized_release_tokens.intersection(tokens).count())
        .max()
        .unwrap_or(0);

    for (normalized_sibling, sibling_tokens) in &precompute.siblings {
        let sibling_overlap = normalized_release_tokens
            .intersection(sibling_tokens)
            .count();
        // Strictly greater: a tie means both the target and the sibling
        // match equally well, which is the normal case for a release
        // like "Jujutsu Kaisen - 06" where sibling "Jujutsu Kaisen 2nd
        // Season" also overlaps on {jujutsu, kaisen}. Only reject when
        // the sibling picks up EXTRA tokens that the target doesn't.
        if sibling_overlap > best_own_overlap {
            // Also require that the sibling's entire normalized title
            // is either a contiguous substring of the release or that
            // ALL of its tokens appear in the release. This prevents
            // freak two-token overlaps ("side story" + some other
            // common fragment) from tripping the rejection.
            let all_tokens_present = sibling_tokens
                .iter()
                .all(|t| normalized_release_tokens.contains(t));
            if all_tokens_present || normalized_release.contains(normalized_sibling.as_str()) {
                return true;
            }
        }
    }
    false
}

/// Extended aliases: synonyms + decomposed sub-phrases from compound titles.
/// Only used as a fallback when primary aliases don't find results.
pub fn collect_extended_aliases(detail: &AnimeDetail) -> Vec<String> {
    let primary = collect_aliases(detail);
    let mut extra = Vec::new();

    // Add AniList synonyms.
    extra.extend(detail.synonyms.iter().cloned());

    // Decompose all titles (primary + synonyms) into sub-phrases.
    // Nyaa releases often use just the subtitle portion
    // (e.g. "Steel Ball Run" from "JoJo's Bizarre Adventure: Part 7–Steel Ball Run").
    let all_titles: Vec<String> = primary.iter().chain(extra.iter()).cloned().collect();
    for title in &all_titles {
        for segment in split_title_segments(title) {
            extra.push(segment);
        }
    }

    // Return only the NEW aliases (not already in primary).
    let primary_lower: HashSet<String> = primary.iter().map(|s| s.to_lowercase()).collect();
    dedupe_strings(extra)
        .into_iter()
        .filter(|s| !primary_lower.contains(&s.to_lowercase()))
        .collect()
}

/// Split a compound title on common delimiters and return meaningful segments.
/// Filters out segments that are too short or too generic to be useful search
/// terms.
///
/// Segments are used both as Nyaa search queries AND as matching aliases
/// inside `matches_target`, which means an over-generic segment can
/// substring-match unrelated shows on Nyaa and cause a completely wrong
/// grab. A single-word subtitle (especially a common English word or
/// hyphenated phrase) is almost always ambiguous — it will substring-match
/// any release that happens to contain the word, regardless of whether
/// that release is for this show or an unrelated one with the same word
/// in its name.
///
/// The 2-token minimum is the cheap defense: segments with only one
/// whitespace-separated token are rejected, regardless of length, because
/// they can't be trusted to uniquely identify a show. Segments with 2+
/// tokens remain — those are specific enough that substring-matching them
/// against an unrelated release is vanishingly unlikely.
fn split_title_segments(title: &str) -> Vec<String> {
    // Normalize various dash types to a common delimiter for splitting.
    let normalized = title
        .replace(['–', '—'], "|") // en dash and em dash
        .replace(": ", "|") // colon+space (keep "Re:Zero" intact)
        .replace(" - ", "|");

    let mut segments = Vec::new();
    for part in normalized.split('|') {
        let trimmed = part.trim();
        // Skip segments that are too short or just "Part N" / "Season N".
        if trimmed.len() < 5 {
            continue;
        }
        if trimmed.eq_ignore_ascii_case(title.trim()) {
            continue;
        }
        // Require at least 2 whitespace-separated tokens. Single-word
        // segments are too generic to use as matching aliases: they can
        // substring-match any release title that happens to contain the
        // word (see doc comment above for the Kizumonogatari / Gundam
        // Iron-Blooded Orphans incident).
        if trimmed.split_whitespace().count() < 2 {
            continue;
        }
        // Skip pure numbering like "Part 7", "Season 2", "2nd Season".
        let lower = trimmed.to_lowercase();
        if lower.starts_with("part ") && lower.len() < 10 {
            continue;
        }
        if lower.starts_with("season ") && lower.len() < 12 {
            continue;
        }
        if lower.ends_with(" season") && lower.len() < 14 {
            continue;
        }
        segments.push(trimmed.to_string());
    }
    segments
}

pub fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// Issue #103: protect short-named series ("Nichijou", "Bocchi") from
/// substring false positives. For an alias that boils down to a single
/// content token, the substring-contains shortcut and the alias-anchored
/// `token_overlap_ratio` both grant 1.0 to ANY release whose title
/// contains that token — including unrelated shows whose names happen
/// to share a Japanese word in common ("nichijou" = "everyday", a word
/// that appears in many slice-of-life titles). Existing
/// `sibling_match_rejects` only catches relatives in the AL graph; an
/// unrelated show isn't reachable that way.
///
/// This helper requires that the release's content tokens (title minus
/// pure-numeric metadata like episode numbers and years) don't
/// substantially exceed the alias's content tokens. Tolerance scales
/// with alias length so multi-token aliases stay permissive (a season
/// subtitle adds expected surplus) while 1-2 token aliases get a tight
/// budget. Returns `true` to keep the alias as a viable match;
/// `false` to discard this alias-vs-release pairing.
/// Which alias tokens size the surplus tolerance in
/// `passes_content_surplus_check`.
#[derive(Clone, Copy)]
enum SurplusBudget {
    /// Every alias content token counts. Used for verbatim-substring
    /// matches, where the whole alias is known to be present.
    FullAlias,
    /// Only distinctive tokens count (#219). Used for fuzzy overlap
    /// matches so generic words can't widen the budget.
    Distinctive,
}

fn passes_content_surplus_check(
    title_tokens: &HashSet<String>,
    alias_tokens: &HashSet<String>,
    budget: SurplusBudget,
) -> bool {
    let is_metadata_number = |t: &str| -> bool {
        // Pure-digit tokens in the episode/year range. Mixed
        // alphanumeric tokens (codecs like "h264", resolutions like
        // "1280x720") can leak in if the bracket strip missed them
        // and the filter list doesn't catch them — those count as
        // surplus and the tolerance absorbs a couple. Keeping this
        // narrow avoids over-stripping legitimate alias tokens like
        // "0" in "Jujutsu Kaisen 0".
        match t.parse::<i32>() {
            Ok(n) => (1..=9999).contains(&n),
            Err(_) => false,
        }
    };
    let title_content: HashSet<&String> = title_tokens
        .iter()
        .filter(|t| !is_metadata_number(t))
        .collect();
    let alias_content: HashSet<&String> = alias_tokens
        .iter()
        .filter(|t| !is_metadata_number(t))
        .collect();

    if alias_content.is_empty() {
        // Pure-numeric alias is degenerate; defer to caller's other
        // matchers rather than reject here.
        return true;
    }

    let surplus = title_content.difference(&alias_content).count();

    // Tolerance scales with alias length:
    //   1 alias token  → at most 1 surplus content token. "Nichijou"
    //                    + episode + group = OK; "Otonari no
    //                    Nichijou" = 2 surplus tokens, REJECT.
    //   2 alias tokens → at most 3 surplus content tokens. "Sword
    //                    Art Online: Alicization" adds "alicization"
    //                    + minor metadata, still under 3.
    //   3+ alias tokens → unlimited. Existing 0.6-ratio gate carries
    //                     the load; multi-token aliases are
    //                     specific enough to not hit the bug.
    let budget_tokens = match budget {
        SurplusBudget::FullAlias => alias_content.len(),
        SurplusBudget::Distinctive => {
            let distinctive = alias_content
                .iter()
                .filter(|t| !is_generic_title_token(t))
                .count();
            if distinctive == 0 {
                alias_content.len()
            } else {
                distinctive
            }
        }
    };
    let tolerance = match budget_tokens {
        1 => 1,
        2 => 3,
        _ => usize::MAX,
    };
    surplus <= tolerance
}

/// How strict the alias scan is. The auto path uses the strict policy
/// (0.6 distinctive overlap plus the #103 surplus budget); the
/// interactive picker uses the relaxed one so users see a broader set
/// of candidates to choose from.
#[derive(Clone, Copy)]
pub(super) struct AliasPolicy {
    pub fuzzy_threshold: f32,
    pub surplus_check: bool,
}

pub(super) const STRICT_ALIAS_POLICY: AliasPolicy = AliasPolicy {
    fuzzy_threshold: 0.6,
    surplus_check: true,
};

pub(super) const RELAXED_ALIAS_POLICY: AliasPolicy = AliasPolicy {
    fuzzy_threshold: 0.5,
    surplus_check: false,
};

/// Scan every alias and report the best match. A verbatim hit wins
/// immediately (it is the maximum); otherwise the fuzzy match with the
/// highest ratio is kept, and ties keep the earlier alias because the
/// canonical AniList titles precede the synthetic sequel variants.
///
/// Issue #103 — short aliases substring-match unrelated shows sharing
/// a token, so a verbatim hit must also pass the content-surplus
/// budget; one that fails contributes nothing and does not fall
/// through to the fuzzy path. Issue #219 — the fuzzy path scores
/// distinctive tokens only, and its surplus budget is keyed on them
/// too: `Risa THE ANIMATION` is a one-word alias for this purpose, so
/// a release that merely says "The Animation" no longer matches, and
/// one that says "Risa" can't smuggle in a whole other title around it.
pub(super) fn best_alias_match(
    normalized_title: &str,
    title_tokens: &HashSet<String>,
    aliases: &[String],
    policy: AliasPolicy,
) -> Option<AliasMatch> {
    let mut best: Option<AliasMatch> = None;
    for alias in aliases {
        let normalized_alias = normalize_title(alias);
        let alias_tokens = token_set(&normalized_alias);
        if normalized_title.contains(&normalized_alias) {
            if !policy.surplus_check
                || passes_content_surplus_check(
                    title_tokens,
                    &alias_tokens,
                    SurplusBudget::FullAlias,
                )
            {
                return Some(AliasMatch {
                    kind: MatchKind::Verbatim,
                    alias: alias.clone(),
                    ratio: 1.0,
                });
            }
            continue;
        }
        let ratio = distinctive_overlap_ratio(title_tokens, &alias_tokens);
        if ratio < policy.fuzzy_threshold {
            continue;
        }
        if policy.surplus_check
            && !passes_content_surplus_check(
                title_tokens,
                &alias_tokens,
                SurplusBudget::Distinctive,
            )
        {
            continue;
        }
        if best.as_ref().is_none_or(|b| ratio > b.ratio) {
            best = Some(AliasMatch {
                kind: MatchKind::Fuzzy,
                alias: alias.clone(),
                ratio,
            });
        }
    }
    best
}

/// The auto-search title gate, reporting how the release matched.
/// `None` means the release is not this series (or not this episode).
/// Whether the release names something beyond the series: anitomy's
/// title for it, minus every word of the series' own titles and
/// synonyms and the usual structural and noise tokens, still has a
/// word left. "Dr. Stone New World - 02" contains "Dr. STONE" verbatim
/// and is season three; "Mob Psycho 100 II" contains "Mob Psycho 100"
/// and is season two. A verbatim match that names more is not the
/// series, and the automatic paths skip it (and report it) rather than
/// grab it. Release-name decorations never count: bracket groups are
/// dropped by normalization and anitomy's title excludes the rest.
///
/// One shape is exempt: `Title - Episode Title - 05`. anitomy folds the
/// episode title into its series title there, so the claim reads
/// "Kimetsu no Yaiba - The Hand Demon". When the first dash segment
/// names the series and nothing else, a later segment is taken as an
/// episode title unless one of its leftover words reads like a season
/// or part marker ("Yuukaku-hen", "Part 2", "II", "The Final Season"),
/// which is what a sequel's subtitle looks like in the same position.
pub fn names_more_than_the_series(title: &str, aliases: &[String]) -> bool {
    let Some(claimed) = crate::services::library_link::extract_anime_title(title) else {
        return false;
    };
    let mut known: HashSet<String> = HashSet::new();
    for alias in aliases {
        known.extend(token_set(&normalize_title(alias)));
    }
    let leftover = |text: &str| -> Vec<String> {
        crate::services::misgrab::verdict::content_tokens(&normalize_title(text))
            .into_iter()
            .filter(|t| !known.contains(t))
            .collect()
    };
    let segments: Vec<&str> = claimed
        .split(" - ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.len() >= 2
        && leftover(segments[0]).is_empty()
        && !crate::services::misgrab::verdict::content_tokens(&normalize_title(segments[0]))
            .is_empty()
    {
        // Raw tokens here, not content tokens: the marker words are
        // exactly the generic ones `content_tokens` strips.
        return segments[1..].iter().any(|seg| {
            token_set(&normalize_title(seg))
                .iter()
                .any(|t| !known.contains(t) && is_season_marker_token(t))
        });
    }
    !leftover(&claimed).is_empty()
}

/// Words that mark a season, part, or arc when they follow a series
/// title: the difference between an episode title and a sequel's
/// subtitle in the `Title - X - 05` position.
fn is_season_marker_token(token: &str) -> bool {
    if matches!(
        token,
        "season"
            | "seasons"
            | "part"
            | "cour"
            | "arc"
            | "hen"
            | "chapter"
            | "saga"
            | "final"
            | "movie"
            | "film"
            | "ova"
            | "oad"
            | "ona"
            | "special"
            | "specials"
            | "ii"
            | "iii"
            | "iv"
            | "v"
            | "vi"
            | "vii"
            | "viii"
            | "ix"
            | "x"
    ) {
        return true;
    }
    // 2nd, 3rd, 10th
    let digits = token.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && matches!(&token[digits.len()..], "st" | "nd" | "rd" | "th")
}

pub fn classify_match(
    title: &str,
    aliases: &[String],
    sibling_precompute: &SiblingRejectPrecompute,
    target: &SearchTarget,
    expected_season: i32,
    allow_batch_episode: bool,
    absolute_offset: i32,
) -> Option<AliasMatch> {
    let normalized_title = normalize_title(title);
    let title_tokens = token_set(&normalized_title);

    let alias_match = best_alias_match(
        &normalized_title,
        &title_tokens,
        aliases,
        STRICT_ALIAS_POLICY,
    )?;

    // Sibling rejection: if the release looks more like a sequel /
    // prequel / side story than it looks like us, reject. See the
    // JJK S1→S3 case in the `collect_sibling_aliases` docstring.
    if sibling_match_rejects(&normalized_title, &title_tokens, sibling_precompute) {
        return None;
    }

    match target {
        SearchTarget::Single => Some(alias_match),
        SearchTarget::Episode(target_ep) => {
            // Season check: reject if release has an explicit season that doesn't match
            if season_mismatch(title, expected_season) {
                return None;
            }

            let parsed = parse_release_numbers(title);
            if parsed.is_empty() {
                return None;
            }
            // Reject releases with 3+ episode numbers (batch/multi-episode)
            // unless the caller explicitly allows batch-to-episode matching
            // (used for quality upgrade searches where BD season packs are the
            // only source for higher-quality individual episodes).
            if !allow_batch_episode && parsed.len() > 2 {
                return None;
            }
            // #30 — Accept either the relative (AL-own) or the absolute
            // (SubsPlease-style) episode number. See `episode_match`
            // for the details.
            episode_match(&parsed, *target_ep, absolute_offset).then_some(alias_match)
        }
    }
}

/// Boolean form of [`classify_match`], kept for the many call sites and
/// tests that only need the verdict.
pub fn matches_target(
    title: &str,
    aliases: &[String],
    sibling_precompute: &SiblingRejectPrecompute,
    target: &SearchTarget,
    expected_season: i32,
    allow_batch_episode: bool,
    absolute_offset: i32,
) -> bool {
    classify_match(
        title,
        aliases,
        sibling_precompute,
        target,
        expected_season,
        allow_batch_episode,
        absolute_offset,
    )
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_more_than_the_series_catches_sequel_subtitles_and_roman_seasons() {
        let dr_stone = vec!["Dr. STONE".to_string()];
        assert!(names_more_than_the_series(
            "[New-raws]Dr. Stone New World - 02 [1080p] [CR].mkv",
            &dr_stone
        ));
        assert!(names_more_than_the_series(
            "[Judas] Dr. Stone: Stone Wars - 02 [1080p]",
            &dr_stone
        ));
        assert!(!names_more_than_the_series(
            "[SubsPlease] Dr. Stone - 02 (1080p) [ABCD1234].mkv",
            &dr_stone
        ));
        assert!(!names_more_than_the_series(
            "Dr.Stone.S01E02.1080p.WEB.x264-GROUP",
            &dr_stone
        ));
        let mob = vec!["Mob Psycho 100".to_string()];
        assert!(names_more_than_the_series(
            "[Erai-raws] Mob Psycho 100 II - 01 [1080p]",
            &mob
        ));
        let mob_ii = vec!["Mob Psycho 100 II".to_string()];
        assert!(!names_more_than_the_series(
            "[Erai-raws] Mob Psycho 100 II - 01 [1080p]",
            &mob_ii
        ));
        // A second alias covers the extra words.
        let kny = vec![
            "Kimetsu no Yaiba".to_string(),
            "Demon Slayer: Kimetsu no Yaiba".to_string(),
        ];
        assert!(!names_more_than_the_series(
            "[Group] Kimetsu no Yaiba (Demon Slayer) - 02 [1080p]",
            &kny
        ));
        // An alternate title the user added counts the same way.
        let with_alt = vec!["Dr. STONE".to_string(), "Dr. Stone New World".to_string()];
        assert!(!names_more_than_the_series(
            "[New-raws]Dr. Stone New World - 02 [1080p] [CR].mkv",
            &with_alt
        ));
    }

    #[test]
    fn names_more_than_the_series_reads_a_folded_episode_title_as_the_series() {
        // anitomy folds the episode title into its series title for the
        // `Title - Episode Title - 05` shape (346-name Nyaa corpus). The
        // first dash segment names the series and nothing else, so the
        // tail is an episode title, not a sequel's subtitle.
        let kny = vec!["Kimetsu no Yaiba".to_string()];
        assert!(!names_more_than_the_series(
            "[Group] Kimetsu no Yaiba - The Hand Demon - 05 [1080p].mkv",
            &kny
        ));
        // The usual order was never affected.
        assert!(!names_more_than_the_series(
            "[Group] Kimetsu no Yaiba - 05 - The Hand Demon [1080p].mkv",
            &kny
        ));
        // A subtitle in that position that reads like a season, part,
        // or arc still names more.
        assert!(names_more_than_the_series(
            "[Group] Kimetsu no Yaiba - Yuukaku-hen - 05 [1080p].mkv",
            &kny
        ));
        assert!(names_more_than_the_series(
            "[Group] Kimetsu no Yaiba - Part 2 - 05 [1080p].mkv",
            &kny
        ));
        assert!(names_more_than_the_series(
            "[Group] Kimetsu no Yaiba - The Final Season - 05 [1080p].mkv",
            &kny
        ));
        let mob = vec!["Mob Psycho 100".to_string()];
        assert!(names_more_than_the_series(
            "[Group] Mob Psycho 100 - II - 01 [1080p].mkv",
            &mob
        ));
        // A colon subtitle is one segment and still names more.
        assert!(names_more_than_the_series(
            "[Group] Kimetsu no Yaiba: Yuukaku-hen - 05 [1080p].mkv",
            &kny
        ));
    }

    // split_title_segments uses a 2-token minimum to reject segments that
    // are too generic to safely become matching aliases. These tests cover
    // the rule in isolation with abstract inputs so the behavior is
    // described, not tied to any particular show.

    #[test]
    fn split_segments_keeps_three_token_subtitle() {
        let segments = split_title_segments("Main Title: Sub One Two Three");
        assert!(
            segments.iter().any(|s| s == "Sub One Two Three"),
            "multi-word subtitle should be kept as a segment, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_keeps_two_token_subtitle() {
        // Two whitespace-separated tokens is the minimum.
        let segments = split_title_segments("Main Title: Alpha Beta");
        assert!(
            segments.iter().any(|s| s == "Alpha Beta"),
            "two-token subtitle should be kept, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_rejects_single_word_subtitle() {
        let segments = split_title_segments("Main Title: Singleword");
        assert!(
            !segments.iter().any(|s| s == "Singleword"),
            "single-word subtitle should be rejected, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_rejects_hyphenated_single_word() {
        // Hyphens are not whitespace, so "Hyphen-Word" is still one token
        // under the rule — important because hyphenated English phrases
        // like "Iron-Blooded" are common enough to substring-match many
        // unrelated titles.
        let segments = split_title_segments("Main Title: Hyphen-Word");
        assert!(
            !segments.iter().any(|s| s == "Hyphen-Word"),
            "hyphenated single-word segment should be rejected, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_keeps_multi_word_main_portion() {
        // Even when the subtitle is rejected, the leading multi-word
        // portion of a compound title remains usable.
        let segments = split_title_segments("Main Title Two: Singleword");
        assert!(
            segments.iter().any(|s| s == "Main Title Two"),
            "multi-word leading portion should be kept, got {:?}",
            segments
        );
    }

    #[test]
    fn matches_target_rejects_unrelated_show_sharing_short_alias_token() {
        // Issue #103 — the short-named series bug. A user tracking a
        // 1-token alias (the canonical case is "Nichijou", a Japanese
        // word meaning "everyday" that shows up in many slice-of-life
        // title fragments) used to match every release whose title
        // contained that token, including totally unrelated shows.
        //
        // Synthetic shapes here so the test isn't tied to any real
        // title — `passes_content_surplus_check` rejects when the
        // release has more than 1 surplus content token beyond a
        // single-token alias.
        let aliases = vec!["Shortname".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);

        // The legit release: alias is the only content token aside
        // from the group + episode. Surplus = 1 (group), at tolerance.
        let legit = "[Group] Shortname - 12 [BD 1080p].mkv";
        assert!(
            matches_target(
                legit,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(12),
                0,
                false,
                0
            ),
            "real release should still match"
        );

        // The false-positive: an unrelated show whose title shares
        // the short alias token but adds 2+ content words. Surplus
        // exceeds the 1-tolerance for a 1-token alias.
        let false_positive = "[Group] Some Other Show no Shortname - 12 [BD 1080p].mkv";
        assert!(
            !matches_target(
                false_positive,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(12),
                0,
                false,
                0
            ),
            "unrelated show sharing the short alias token must be rejected"
        );
    }

    #[test]
    fn matches_target_short_alias_accepts_release_with_episode_marker() {
        // PR #104 review: the 1-token alias tolerance is 1 surplus.
        // `mkv` was eating the budget, so a release shape with one
        // extra word like `Episode` would get rejected even though
        // it's clearly the target's release. Add `mkv`/`mp4`/`mka`
        // to normalize_title's filter list to free the slot. Pin
        // the legit case here.
        let aliases = vec!["Shortname".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let with_episode_marker = "[Group] Shortname Episode 12 [BD 1080p].mkv";
        assert!(
            matches_target(
                with_episode_marker,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(12),
                0,
                false,
                0
            ),
            "legit release with extra `Episode` marker must still match"
        );
        // The v2 / revision form is a similar shape.
        let with_revision = "Shortname - 12 v2 [BD].mkv";
        assert!(
            matches_target(
                with_revision,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(12),
                0,
                false,
                0
            ),
            "release with v2 revision must still match"
        );
    }

    #[test]
    fn matches_target_two_token_alias_keeps_legit_subtitle() {
        // Symmetry guard: a 2-token alias must still accept a release
        // whose title carries an extra subtitle word. Tolerance for
        // 2-token aliases is 3 surplus, so a release with one
        // additional subtitle word is well within budget.
        let aliases = vec!["Sword Art".to_string()]; // synthetic 2-tok
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let legit = "[Group] Sword Art Online - Alicization - 12 [BD 1080p].mkv";
        assert!(
            matches_target(
                legit,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(12),
                0,
                false,
                0
            ),
            "2-token alias should accept a related release with extra subtitle words"
        );
    }

    #[test]
    fn matches_target_rejects_release_whose_only_overlap_is_a_rejected_segment() {
        // End-to-end regression: a release whose token overlap with the
        // primary alias is below the 0.6 threshold must not slip through
        // just because some single-word substring of a synonym happens to
        // appear in the release filename. With the 2-token rule in place,
        // that single-word substring is never produced as an alias, so
        // substring-match can't succeed.
        let aliases = vec![
            "Main Title: Subtitle One".to_string(),
            "Main Title: Subtitle Two".to_string(),
        ];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let unrelated_release =
            "[Group] Totally Different Show - Subtitle One-Word Thing - 01 [1080p].mkv";
        // The release shares only the word "Subtitle" with the primary
        // alias tokens {main, title, subtitle, one} / {main, title,
        // subtitle, two}. Overlap ratio for either alias is 1/4 = 0.25,
        // well below 0.6. No segment derived from the primary aliases
        // survives the 2-token rule to substring-match "Subtitle" in
        // isolation, so the match must fail.
        assert!(
            !matches_target(
                unrelated_release,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(1),
                0,
                false,
                0
            ),
            "unrelated release should not match via token overlap alone"
        );
    }

    #[test]
    fn matches_target_accepts_release_with_full_primary_alias_substring() {
        let aliases = vec!["Main Title Subtitle One".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let good_release = "[Group] Main Title Subtitle One [BD 1080p].mkv";
        assert!(matches_target(
            good_release,
            &aliases,
            &no_siblings,
            &SearchTarget::Single,
            0,
            false,
            0
        ));
    }

    #[test]
    fn matches_target_rejects_sibling_arc_release() {
        // Regression: auto-searching JJK S1 E6 used to grab
        // `[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06`
        // because the sibling arc title has no explicit "S02"/"Season 2"
        // marker for `season_mismatch` to catch, but "Jujutsu Kaisen" is
        // a substring of the release. The sibling check resolves this:
        // the sibling alias "Jujutsu Kaisen: Shimetsu Kaiyuu" has 4
        // overlapping tokens with the release vs the target's 2, so the
        // sibling wins and the release is rejected.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let siblings = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 [1080p CR WEBRip HEVC AAC].mkv";
        assert!(
            !matches_target(
                release,
                &own,
                &precompute,
                &SearchTarget::Episode(6),
                1,
                false,
                0
            ),
            "sibling arc release must not match the base-franchise target"
        );
    }

    #[test]
    fn matches_target_keeps_base_franchise_release_despite_siblings() {
        // Symmetric: with the same sibling list, a plain JJK S1 release
        // should still match the target. The sibling overlaps on only
        // 2 tokens ({jujutsu, kaisen}) — the same as the target's own
        // overlap — so the sibling check is a no-op.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let siblings = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen - 06 [1080p].mkv";
        assert!(matches_target(
            release,
            &own,
            &precompute,
            &SearchTarget::Episode(6),
            1,
            false,
            0
        ));
    }

    #[test]
    fn matches_target_keeps_target_arc_release_against_unrelated_sibling() {
        // A JJK S2 Shibuya Incident target should still accept its own
        // arc release even when the sibling list includes another arc.
        let own = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let siblings = vec![
            "Jujutsu Kaisen".to_string(),
            "Jujutsu Kaisen: Kaigyoku Gyokusetsu".to_string(),
        ];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 [1080p].mkv";
        assert!(matches_target(
            release,
            &own,
            &precompute,
            &SearchTarget::Episode(6),
            0,
            false,
            0
        ));
    }

    // ── #30 — absolute-vs-relative Nyaa episode numbering ──────────────
    //
    // SubsPlease (and others) number sequel cours either as the AL-own
    // relative number ("Otonari S2 - 03") or as the absolute number
    // continuing from S1 ("Jujutsu Kaisen - 56" for JJK S3 E9,
    // "Re Zero - 68" for a post-S2 episode). Before #30 the filter was
    // strict-relative, so the absolute releases were dropped from both
    // interactive and auto search.
    //
    // `episode_match` is the shared check used by both paths; these
    // tests pin both numbering conventions against realistic parsed
    // sets, and then verify the public `matches_target` applies the
    // same rule end-to-end.

    fn parsed(nums: &[i32]) -> std::collections::HashSet<i32> {
        nums.iter().copied().collect()
    }

    #[test]
    fn episode_match_accepts_relative_number_without_offset() {
        // First-season / offset=0: only the relative number counts,
        // which matches the legacy strict-relative behavior.
        assert!(episode_match(&parsed(&[3]), 3, 0));
        assert!(!episode_match(&parsed(&[25]), 3, 0));
    }

    #[test]
    fn episode_match_accepts_relative_number_even_when_offset_set() {
        // SubsPlease "Otonari no Tenshi-sama S2 - 03" (relative
        // numbering) against a target with an S1 prequel of 12
        // episodes must still pass — relative numbering is the more
        // common convention and we can't know which one any given
        // release picked.
        assert!(episode_match(&parsed(&[3]), 3, 12));
    }

    #[test]
    fn episode_match_accepts_absolute_number_against_relative_target() {
        // JJK S3 E9 ships as "Jujutsu Kaisen - 56" — absolute numbering
        // continuing from S1 (24) + S2 (23) = 47 prior cour episodes.
        assert!(episode_match(&parsed(&[56]), 9, 47));
        // Re:Zero - 68 is another realistic example.
        assert!(episode_match(&parsed(&[68]), 18, 50));
    }

    #[test]
    fn episode_match_rejects_unrelated_numbers_with_offset() {
        // An absolute number from a different episode is still wrong.
        // Target is S3 E9 (= absolute 56); release is absolute 60
        // (= S3 E13) — rejected.
        assert!(!episode_match(&parsed(&[60]), 9, 47));
        // Target is S3 E1 (= absolute 48); release is relative 5 — rejected.
        assert!(!episode_match(&parsed(&[5]), 1, 47));
    }

    #[test]
    fn matches_target_accepts_subsplease_absolute_numbered_sequel_cour() {
        // Full-path regression: a SubsPlease absolute-numbered release
        // for JJK S3 E9 must pass through `matches_target` when the
        // cumulative S1+S2 offset (47) is supplied.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&own, &[]);
        let release = "[SubsPlease] Jujutsu Kaisen - 56 (1080p) [0F106B43].mkv";
        assert!(matches_target(
            release,
            &own,
            &no_siblings,
            &SearchTarget::Episode(9),
            // `expected_season` is the season_mismatch target, not the
            // numbering target — this is 3 because we're asking for S3.
            3,
            false,
            47,
        ));
    }

    // ── #84 — sequel-variant alias generation ─────────────────────────
    //
    // These pin the shorthand-marker generator against the two motivating
    // cases (Sono Bisque Doll S2 and the Kizumonogatari trilogy) plus the
    // distinctiveness guardrail (generic franchise names like `Gundam`
    // don't produce variants that would substring-match unrelated series).

    #[test]
    fn sequel_variants_generate_s2_and_s02_from_ordinal_season() {
        let input = vec!["Sono Bisque Doll wa Koi wo Suru 2nd Season".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(
            variants
                .iter()
                .any(|v| v == "Sono Bisque Doll wa Koi wo Suru S2")
        );
        assert!(
            variants
                .iter()
                .any(|v| v == "Sono Bisque Doll wa Koi wo Suru S02")
        );
        assert!(
            variants
                .iter()
                .any(|v| v == "Sono Bisque Doll wa Koi wo Suru - 02")
        );
        // Roman-numeral variant bridges AL-uses-ordinal to release-uses-
        // Roman (e.g. a group naming the same cour `Sono Bisque Doll ... II`).
        assert!(
            variants
                .iter()
                .any(|v| v == "Sono Bisque Doll wa Koi wo Suru II")
        );
    }

    /// Romaji-title regression: AniList ships some titles with the
    /// cour's arc name appended AFTER the `Nth Season` marker. COTE S4
    /// is the canonical case: the AL romaji is `Youkoso Jitsuryoku
    /// Shijou Shugi no Kyoushitsu e 4th Season 2-nensei-hen Ichi
    /// Gakki`, but every release group ships the same cour as
    /// `[SubsPlease] Youkoso ... S4 - 06`. Pre-fix the
    /// `RE_ORDINAL_SEASON` end-of-string anchor required the marker to
    /// be the last tokens of the alias, so the variant generator
    /// produced no `S4` / `S04` / `- 04` aliases for these titles —
    /// auto-search and interactive search both short-circuited with
    /// zero matches even though the franchise had releases on every
    /// configured indexer.
    #[test]
    fn sequel_variants_strip_trailing_arc_descriptor_after_season_marker() {
        let input = vec![
            "Youkoso Jitsuryoku Shijou Shugi no Kyoushitsu e 4th Season 2-nensei-hen Ichi Gakki"
                .to_string(),
        ];
        let variants = sequel_variant_aliases(&input);
        let base = "Youkoso Jitsuryoku Shijou Shugi no Kyoushitsu e";
        assert!(
            variants.iter().any(|v| v == &format!("{} S4", base)),
            "expected base+S4 variant for COTE-shape title; got {:?}",
            variants
        );
        assert!(
            variants.iter().any(|v| v == &format!("{} S04", base)),
            "expected base+S04 variant; got {:?}",
            variants
        );
        assert!(
            variants.iter().any(|v| v == &format!("{} - 04", base)),
            "expected base+`- 04` variant; got {:?}",
            variants
        );
    }

    /// Word-form season marker with trailing arc — covers the
    /// `Some Title Second Season Foo Arc` shape (AniList sometimes
    /// uses ordinal words instead of digits for the season marker).
    #[test]
    fn sequel_variants_handle_word_season_with_trailing_descriptor() {
        let input = vec!["Some Title Second Season Sub Arc".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(
            variants.iter().any(|v| v == "Some Title S2"),
            "word-form season + trailing arc must still produce S2 variant; got {:?}",
            variants
        );
    }

    /// Sanity check: pure-marker aliases (no trailing arc) still work
    /// after the regex relaxation. Without this, the relaxation could
    /// silently change behavior for the simple case the original
    /// regex was designed for.
    #[test]
    fn sequel_variants_still_handle_bare_season_markers() {
        let input = vec!["Sono Bisque Doll wa Koi wo Suru 2nd Season".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(
            variants
                .iter()
                .any(|v| v == "Sono Bisque Doll wa Koi wo Suru S2")
        );
    }

    #[test]
    fn sequel_variants_skip_roman_when_alias_already_ends_in_roman() {
        // AL canonical already carries the Roman numeral (`Overlord IV`
        // is how AL lists the 4th season), so emitting `Overlord IV` as
        // a variant would just duplicate the canonical query. The S{N}
        // / S{NN} / `- {NN}` variants still fire for groups using season-
        // N conventions.
        let input = vec!["Overlord IV".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(variants.iter().any(|v| v == "Overlord S4"));
        assert!(variants.iter().any(|v| v == "Overlord S04"));
        assert!(variants.iter().any(|v| v == "Overlord - 04"));
        assert!(
            !variants.iter().any(|v| v == "Overlord IV"),
            "duplicate Roman variant must be suppressed, got {:?}",
            variants
        );
    }

    #[test]
    fn sequel_variants_generate_movie_trilogy_hyphen_number() {
        // AL canonical: `Kizumonogatari II: Nekketsu-hen`. The target
        // variant MTBB actually ships is `Kizumonogatari - 02`.
        let input = vec!["Kizumonogatari II: Nekketsu-hen".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(variants.iter().any(|v| v == "Kizumonogatari - 02"));
        assert!(variants.iter().any(|v| v == "Kizumonogatari S2"));
        assert!(variants.iter().any(|v| v == "Kizumonogatari S02"));
    }

    #[test]
    fn sequel_variants_generate_short_s_forms_for_part_numbered() {
        let input = vec!["Some Long Title Part 3".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(variants.iter().any(|v| v == "Some Long Title - 03"));
        assert!(variants.iter().any(|v| v == "Some Long Title S3"));
        assert!(variants.iter().any(|v| v == "Some Long Title S03"));
    }

    #[test]
    fn sequel_variants_cap_at_four_per_qualifying_alias() {
        // Each HTTP round-trip per sweep is a real cost. Pin the upper
        // bound at 4 variants per qualifying input alias (S{N}, S{NN},
        // - {NN}, and the Roman form when it's not already the alias'
        // own tail) so a future expansion of the variant list is a
        // conscious decision, not an accidental query-count multiplier.
        let ordinal_input = vec!["Some Long Title Part 3".to_string()];
        let ordinal_variants = sequel_variant_aliases(&ordinal_input);
        assert_eq!(
            ordinal_variants.len(),
            4,
            "ordinal-marker alias should produce 4 variants, got {:?}",
            ordinal_variants
        );
        // When the Roman variant is suppressed (canonical already ends
        // in the Roman form), the count drops to 3.
        let roman_input = vec!["Overlord IV".to_string()];
        let roman_variants = sequel_variant_aliases(&roman_input);
        assert_eq!(
            roman_variants.len(),
            3,
            "Roman-canonical alias should produce 3 variants, got {:?}",
            roman_variants
        );
    }

    #[test]
    fn sequel_variants_handle_word_season_second() {
        // `Monogatari Second Season` is the real AL alias for S2 of the
        // Monogatari franchise. The ordinal-word pattern handles it.
        let input = vec!["Monogatari Second Season".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(variants.iter().any(|v| v == "Monogatari S2"));
        assert!(variants.iter().any(|v| v == "Monogatari - 02"));
    }

    #[test]
    fn sequel_variants_reject_short_generic_base() {
        // `Gundam` as a base is too generic — the variants would
        // substring-match unrelated Gundam entries. The guardrail must
        // keep those out of the query list.
        let input = vec!["Gundam Season 2".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(
            variants.is_empty(),
            "short generic base should be rejected, got {:?}",
            variants
        );
    }

    #[test]
    fn sequel_variants_skip_alias_without_marker() {
        let input = vec!["Some Title Without Any Marker".to_string()];
        let variants = sequel_variant_aliases(&input);
        assert!(variants.is_empty(), "got {:?}", variants);
    }

    #[test]
    fn sequel_variants_dedupe_across_multi_alias_input() {
        // Same base, two marker conventions in different aliases — the
        // per-convention output should be deduped by the final pass.
        let input = vec![
            "Some Long Title 2nd Season".to_string(),
            "Some Long Title Season 2".to_string(),
        ];
        let variants = sequel_variant_aliases(&input);
        let s2_count = variants
            .iter()
            .filter(|v| *v == "Some Long Title S2")
            .count();
        assert_eq!(s2_count, 1, "duplicate S2 variant, got {:?}", variants);
    }

    #[test]
    fn matches_target_accepts_movie_trilogy_hyphen_number_via_variant() {
        // End-to-end: the Part 2 target's alias list is augmented with
        // `sequel_variant_aliases`, and an MTBB-shaped release for
        // `Kizumonogatari - 02` must now pass the filter.
        let primary = vec!["Kizumonogatari II: Nekketsu-hen".to_string()];
        let variants = sequel_variant_aliases(&primary);
        let all_aliases: Vec<String> = primary.iter().chain(variants.iter()).cloned().collect();
        let precompute = SiblingRejectPrecompute::build(&all_aliases, &[]);
        let release = "[MTBB] Kizumonogatari - 02 [BD 1080p FLAC].mkv";
        assert!(
            matches_target(
                release,
                &all_aliases,
                &precompute,
                &SearchTarget::Single,
                0,
                false,
                0,
            ),
            "MTBB Kizumonogatari Part 2 release must match via sequel variant"
        );
    }

    #[test]
    fn matches_target_rejects_wrong_trilogy_entry_via_alias_overlap() {
        // The variant alias list DOES make the Part 2 target receptive to
        // `Kizumonogatari - 02`, but it must NOT also accept `- 01` /
        // `- 03` — that would be wrong-entry routing within the trilogy.
        // Token overlap with the variant `Kizumonogatari - 02` is only
        // 1/2 = 0.5 (< 0.6 threshold) for a `- 01` release — the canonical
        // AL alias has too many extra tokens to reach the threshold on
        // its own either. Pins the correct rejection.
        let primary = vec!["Kizumonogatari II: Nekketsu-hen".to_string()];
        let variants = sequel_variant_aliases(&primary);
        let all_aliases: Vec<String> = primary.iter().chain(variants.iter()).cloned().collect();
        let no_siblings = SiblingRejectPrecompute::build(&all_aliases, &[]);
        let wrong_entry = "[MTBB] Kizumonogatari - 01 [BD 1080p FLAC].mkv";
        assert!(
            !matches_target(
                wrong_entry,
                &all_aliases,
                &no_siblings,
                &SearchTarget::Single,
                0,
                false,
                0,
            ),
            "wrong trilogy entry must not match Part 2 target via variants"
        );
    }

    #[test]
    fn matches_target_rejects_absolute_numbered_sibling_cour_against_wrong_target() {
        // Mirror of the above: a release carrying an absolute number
        // that doesn't line up with our target (even once the offset
        // is added) must still be rejected. target = S3 E1
        // (absolute 48); release is absolute 60 = S3 E13 — wrong
        // episode.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&own, &[]);
        let release = "[SubsPlease] Jujutsu Kaisen - 60 (1080p).mkv";
        assert!(!matches_target(
            release,
            &own,
            &no_siblings,
            &SearchTarget::Episode(1),
            3,
            false,
            47,
        ));
    }

    #[test]
    fn issue_219_generic_the_animation_alias_must_not_match_unrelated_show() {
        // Issue #219 — aliases exactly as Phase 1.5 builds them for
        // AL 21521 (romaji + native canonical, plus the colon-split
        // segment). `the` + `animation` used to be 2 of the segment's
        // 3 tokens, clearing the 0.6 gate for every "The Animation"
        // release an indexer returned.
        let segments = split_title_segments("Kowaremono: Risa THE ANIMATION");
        assert_eq!(segments, vec!["Risa THE ANIMATION".to_string()]);
        let aliases = vec![
            "Kowaremono: Risa THE ANIMATION".to_string(),
            "コワレモノ:璃沙 THE ANIMATION".to_string(),
            "Risa THE ANIMATION".to_string(),
        ];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let target = SearchTarget::Single;
        for legit in [
            "[H-Enc] コワレモノ：璃沙 THE ANIMATION / Kowaremono Risa The Animation 01-02 (BDRip 1080p HEVC AAC)",
            "[Group] Risa The Animation - 01 [1080p]",
            "[Diogo4D] Kowaremono Risa The Animation [DVD][576p][Uncensored] [7FED77BC].mkv",
        ] {
            assert!(
                matches_target(legit, &aliases, &no_siblings, &target, 0, false, 0),
                "the real release must still match: {legit}"
            );
        }
        for wrong in [
            "[APRZ] Grisaia: Phantom Trigger - The Animation - 01 [SORD] [1080p]",
            "[APRZ] Grisaia: Phantom Trigger - The Animation - 02 [SOUL SPEED]",
            "Grisaia Phantom Trigger - The Animation Episode 1-2",
            "[Xonline].Grisaia.Phantom.Trigger.The.Animation-02.BD.1920p.x.264-10Bit.Flac.[02964F5A]",
            "[SubsPlease] Sword Art Online the Animation - 01 (1080p)",
            // Shares the distinctive token but is a different show:
            // the fuzzy path's surplus budget is keyed on the one
            // distinctive token, so the extra title words reject it.
            "[Kira-Fansub] Amagami SS Risa and Miya Arc (BD 1920x1080 h264 AAC)",
        ] {
            assert!(
                !matches_target(wrong, &aliases, &no_siblings, &target, 0, false, 0),
                "unrelated release must be rejected: {wrong}"
            );
        }
    }

    #[test]
    fn distinctive_overlap_ratio_ignores_generic_tokens() {
        let title = token_set("grisaia phantom trigger the animation 01");
        let risa = token_set("risa the animation");
        assert_eq!(distinctive_overlap_ratio(&title, &risa), 0.0);
        assert!(
            token_overlap_ratio(&title, &risa) > 0.6,
            "the old ratio is what let #219 through"
        );

        // Generic tokens are dropped from the denominator, so the
        // distinctive words carry the whole decision.
        let hero = token_set("boku no hero academia");
        let release = token_set("boku hero academia 12");
        assert_eq!(distinctive_overlap_ratio(&release, &hero), 1.0);

        // An all-generic alias has no signal on the fuzzy path.
        let movie = token_set("the movie");
        assert_eq!(
            distinctive_overlap_ratio(&token_set("some show the movie"), &movie),
            0.0
        );
    }

    #[test]
    fn classify_match_prefers_verbatim_over_earlier_fuzzy_alias() {
        // The first alias only matches fuzzily (its tokens are out of
        // order, so it is not a substring, and "journey" is not
        // "journeys": 2 of 3 distinctive tokens); the second is a
        // verbatim substring. The old `.any()` scan would have stopped
        // at the first, hiding the stronger match.
        let aliases = vec![
            "Frieren Journey Beyond".to_string(),
            "Frieren Beyond Journeys End".to_string(),
        ];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let m = classify_match(
            "[G] Frieren Beyond Journeys End - 01 [1080p].mkv",
            &aliases,
            &no_siblings,
            &SearchTarget::Episode(1),
            0,
            false,
            0,
        )
        .expect("matches");
        assert_eq!(m.kind, MatchKind::Verbatim);
        assert_eq!(m.alias, "Frieren Beyond Journeys End");
        assert_eq!(m.ratio, 1.0);
    }

    #[test]
    fn classify_match_reports_highest_fuzzy_ratio_when_no_verbatim() {
        let aliases = vec![
            "Boku no Hero Academia Final Season".to_string(),
            "Boku no Hero Academia".to_string(),
        ];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        // "boku hero academia 12": alias 1 has distinctive tokens
        // {boku, hero, academia, final} -> 0.75; alias 2 has
        // {boku, hero, academia} -> 1.0 but not a substring because
        // "no" is missing from the title.
        let m = classify_match(
            "[G] Boku Hero Academia - 12 [1080p].mkv",
            &aliases,
            &no_siblings,
            &SearchTarget::Episode(12),
            0,
            false,
            0,
        )
        .expect("matches");
        assert_eq!(m.kind, MatchKind::Fuzzy);
        assert_eq!(m.alias, "Boku no Hero Academia");
        assert!((m.ratio - 1.0).abs() < f32::EPSILON, "ratio {}", m.ratio);
    }

    #[test]
    fn classify_match_returns_none_when_episode_check_fails() {
        let aliases = vec!["Sousou no Frieren".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let title = "[G] Sousou no Frieren - 07 [1080p].mkv";
        assert!(
            classify_match(
                title,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(7),
                0,
                false,
                0
            )
            .is_some()
        );
        assert!(
            classify_match(
                title,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(8),
                0,
                false,
                0
            )
            .is_none(),
            "a verbatim alias hit must still fail the episode check"
        );
    }

    #[test]
    fn best_alias_match_relaxed_accepts_0_5_that_strict_rejects() {
        let aliases = vec!["Alpha Beta Gamma Delta".to_string()];
        let title = normalize_title("[G] Alpha Beta Something - 01");
        let tokens = token_set(&title);
        assert!(best_alias_match(&title, &tokens, &aliases, STRICT_ALIAS_POLICY).is_none());
        let relaxed = best_alias_match(&title, &tokens, &aliases, RELAXED_ALIAS_POLICY)
            .expect("relaxed policy accepts 2 of 4 distinctive tokens");
        assert_eq!(relaxed.kind, MatchKind::Fuzzy);
        assert!((relaxed.ratio - 0.5).abs() < f32::EPSILON);
    }
}
