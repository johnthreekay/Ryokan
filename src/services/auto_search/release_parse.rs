//! Release-filename token parsing: episode numbers, season markers,
//! Roman numerals, part identifiers, subtitle extraction, media-file
//! detection, season-mismatch rejection.

use std::sync::LazyLock;

use regex_lite::Regex;

use crate::services::anilist::AnimeDetail;
use crate::services::release_title;

use super::collect_aliases;

/// Returns 0 if no season indicator is found (treated as season 1 during matching).
pub fn infer_season_from_detail(detail: &AnimeDetail) -> i32 {
    let aliases = collect_aliases(detail);
    for alias in &aliases {
        let s = infer_season_from_title(alias);
        if s > 0 {
            return s;
        }
    }
    0
}

pub(super) fn infer_season_from_title(title: &str) -> i32 {
    release_title::season_marker(title).unwrap_or(0)
}

// ── Pre-compiled regexes for extract_part_number ───────────────────────────
//
// `extract_part_number` recovers the "which entry in a multi-part release"
// number from an AniList title so the selective-download path can match
// it against per-file episode numbers inside a megapack. This is distinct
// from `infer_season_from_detail`, which is about season/cour indexing
// for the *query* sweep. A movie trilogy like Kizumonogatari I/II/III
// has no season at all, just parts.
static RE_PART_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:part|chapter|movie|film)\s*(\d{1,2})\b").unwrap());

/// Roman numeral at a word boundary, II–IX only. Matches at end of
/// string or before common separators (`:` for subtitle, space, `-`)
/// so titles like "Kizumonogatari II: Nekketsu-hen" and "Rebuild of
/// Evangelion III" both resolve cleanly.
///
/// **Single-letter Romans (`I`, `V`, `X`) are deliberately excluded.**
/// Matching bare `I` would fire on any anime title containing the
/// English pronoun — "I Want to Eat Your Pancreas", "I, Robot" — and
/// resolve them as part 1, causing `pick_by_part_number` to narrow a
/// megapack to the wrong file. Bare `V` and `X` carry the same risk
/// ("V for Vendetta", "X/1999"). The tradeoff is that trilogy first
/// entries titled "Kizumonogatari I" no longer narrow to E01 inside a
/// megapack — they fall through to the full pack instead, which is
/// fine because users rarely grab a trilogy's opening chapter in
/// isolation. Explicit markers like "Part 1" / "Chapter 1" still
/// work via [`RE_PART_N`].
static RE_ROMAN_PART: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(ii{1,2}|iv|vi{1,3}|ix)\b(?:\s*[:\-]|$|\s)").unwrap());

/// Extract the "part number" from an AniList detail's titles. Returns
/// `None` when the title carries no such marker — the common case for
/// single-film works and standalone TV seasons.
///
/// This is used by the selective-file download path: when the user
/// grabs a megapack (e.g. the smol Monogatari pack containing Kizumo
/// I, II, III as separate files), we need to know "the target is part
/// 2" so `pick_wanted_file_indices` can match it against the E02 file.
///
/// Matching order (first hit wins):
///   1. Explicit `Part N` / `Chapter N` / `Movie N` / `Film N`
///   2. Roman numeral I–X at a word boundary
///
/// We check all three alias titles (romaji, english, native). Native
/// is unlikely to fire the English regexes but it's cheap to include.
pub fn extract_part_number(detail: &AnimeDetail) -> Option<i32> {
    let titles = [
        detail.title_english.as_str(),
        detail.title_romaji.as_str(),
        detail.title_native.as_str(),
    ];
    for title in titles.iter().filter(|t| !t.is_empty()) {
        if let Some(caps) = RE_PART_N.captures(title)
            && let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok())
            && (1..=20).contains(&n)
        {
            return Some(n);
        }
        if let Some(caps) = RE_ROMAN_PART.captures(title)
            && let Some(m) = caps.get(1)
            && let Some(n) = release_title::roman_value(m.as_str())
        {
            return Some(n as i32);
        }
    }
    None
}

/// Given the file list of a multi-entry pack torrent and the AniList
/// detail of the user's target, return the indices of the files that
/// correspond to the target. Returns `None` when we can't narrow the
/// selection — the safe default is "keep everything" rather than
/// guessing wrong and skipping the one file the user wanted.
///
/// Two narrowing strategies are tried in order:
///
/// 1. **Part number** via [`extract_part_number`]. Handles trilogies
///    and multi-part OVAs where AniList titles end in "II", "III",
///    "Part 2", etc. File selection reads the episodes each file name
///    holds (`release_title`) and keeps files whose episodes contain the
///    target part.
///
/// 2. **Positive subtitle match** via [`extract_season_subtitle`].
///    Handles franchise megapacks where the target itself carries a
///    distinguishing suffix after `:` or ` - ` — e.g. "JoJo's Bizarre
///    Adventure: Stardust Crusaders" inside a JoJo S1–S5 pack. The
///    filename must contain the normalized subtitle.
///
/// Non-media files (NFO, TXT, subtitles) are ignored so they don't
/// dilute the selection. In both strategies, the result must fall
/// within the target's expected episode count (×1.5 + 2 for slack)
/// or we return `None` — guards against positive matches that
/// accidentally sweep in bonus features or siblings that share a
/// subtitle substring.
///
/// Note: **franchise roots without their own subtitle** (e.g. JoJo S1
/// = "JoJo's Bizarre Adventure") are intentionally NOT narrowed here.
/// They're handled by the higher-level multi-series pack detection
/// path which downloads the full pack and auto-adds detected sibling
/// entries to the library instead — a cleaner answer than
/// filename-based negative matching, which is prone to partial
/// coverage when AniList relations don't include every sibling.
pub fn pick_wanted_file_indices(filenames: &[String], detail: &AnimeDetail) -> Option<Vec<usize>> {
    if let Some(part) = extract_part_number(detail)
        && let Some(ids) = pick_by_part_number(filenames, part, detail)
    {
        return Some(ids);
    }
    if let Some(subtitle) = extract_season_subtitle(detail)
        && let Some(ids) = pick_by_subtitle_include(filenames, &subtitle, detail)
    {
        return Some(ids);
    }
    None
}

/// Narrow a megapack to the files that correspond to the target's
/// part number.
///
/// **Assumes 1 part = 1 episode number in the filename.** Works for
/// the canonical smol Monogatari pack (Kizumonogatari I/II/III land
/// as S09E01/S09E02/S09E03) and similar per-episode layouts. Breaks
/// for releases where a single "part" spans multiple files — e.g.
/// multi-file BDMV rips of a single film, or "Part 2 E13-E24" —
/// because a file's episodes containing `part` then matches the wrong
/// files entirely. Rebuild of Evangelion 1.0/2.0/3.0 happens to fall
/// through to `None` for a different reason: a `2.22`-style decimal is
/// no episode, so the match set is empty and the caller keeps the whole
/// pack, which is the safe outcome.
///
/// Returns `None` when:
/// - no files match (keep-everything is safer than picking wrong),
/// - every file matches (nothing was actually narrowed),
/// - the match set exceeds the target's expected episode count (guards
///   against a `part=1` query sweeping in every episode in a 24-ep TV
///   season when the target is actually a 2-ep OVA).
fn pick_by_part_number(
    filenames: &[String],
    part: i32,
    detail: &AnimeDetail,
) -> Option<Vec<usize>> {
    // Part-number narrowing is for trilogies / multi-part OVAs where
    // each "part" is a single file inside a grouped pack (Kizumonogatari
    // I/II/III, Rebuild of Evangelion 1/2/3, etc.). Those AniList
    // entries all have a single-digit `episodes` count because each
    // part is itself one film.
    //
    // A season whose title happens to end in a Roman numeral ("Mob
    // Psycho 100 III", "Overlord IV") ALSO triggers
    // `extract_part_number` — but its SeaDex batch is 12 per-episode
    // files named "...III - 01" … "...III - 12". Part-number narrowing
    // there picks the one file whose parsed ep number equals the
    // Roman numeral (file 3 for "III"), which is catastrophically
    // wrong — the user wants the whole season, not one episode that
    // happens to share the season index. Gate the strategy on a small
    // episode count so seasons fall through to the (correct) full-pack
    // grab + auto_expand path.
    const MAX_EPISODES_FOR_PART_NARROWING: i32 = 3;
    if detail.effective_episode_count() > MAX_EPISODES_FOR_PART_NARROWING {
        return None;
    }

    let mut matches: Vec<usize> = Vec::new();
    let mut media_count = 0usize;
    for (idx, name) in filenames.iter().enumerate() {
        if !is_media_filename(name) {
            continue;
        }
        media_count += 1;
        let base = name.rsplit('/').next().unwrap_or(name);
        if release_title::parse(base)
            .release_episodes()
            .contains(&part)
        {
            matches.push(idx);
        }
    }
    if matches.is_empty() || matches.len() >= media_count {
        return None;
    }
    if !within_expected_episode_count(matches.len(), detail) {
        return None;
    }
    Some(matches)
}

fn pick_by_subtitle_include(
    filenames: &[String],
    subtitle: &str,
    detail: &AnimeDetail,
) -> Option<Vec<usize>> {
    let needle = normalize_subtitle(subtitle);
    if needle.is_empty() {
        return None;
    }
    let mut matches: Vec<usize> = Vec::new();
    let mut media_count = 0usize;
    for (idx, name) in filenames.iter().enumerate() {
        if !is_media_filename(name) {
            continue;
        }
        media_count += 1;
        if normalize_subtitle(name).contains(&needle) {
            matches.push(idx);
        }
    }
    if matches.is_empty() || matches.len() >= media_count {
        return None;
    }
    if !within_expected_episode_count(matches.len(), detail) {
        return None;
    }
    Some(matches)
}

/// Sanity-cap the narrowed selection against the target's expected
/// episode count. Rejects selections that are implausibly larger than
/// the target's own season (×1.5 slack plus 2 for rounding / bonus
/// features / BD extras). Without this guard, a positive subtitle
/// match could accidentally sweep in files for a longer sibling that
/// shares a subtitle substring, and the selective log line would
/// mask the overshoot as a successful narrowing.
///
/// Shows that are still airing report `episodes: None` from AniList;
/// in that case `effective_episode_count()` falls back to
/// `nextAiringEpisode - 1`, which is 0 during the week-0 pre-airing
/// window. Returning `true` unconditionally for 0-count targets keeps
/// the cap disabled for airing shows and lets the strategy's own
/// `matches.len() < media_count` guard carry the safety load.
fn within_expected_episode_count(matches_len: usize, detail: &AnimeDetail) -> bool {
    within_episode_slack(matches_len, detail.effective_episode_count())
}

/// Raw-count version of [`within_expected_episode_count`]. Shared with
/// [`detect_sibling_entries_in_pack`], where the "expected" value comes
/// from a `RelatedEntry` card that doesn't carry `next_airing_episode`
/// and therefore can't use `effective_episode_count`.
pub(super) fn within_episode_slack(matches_len: usize, expected: i32) -> bool {
    if expected <= 0 {
        return true;
    }
    let slack = (expected as f32 * 1.5).ceil() as usize + 2;
    matches_len <= slack
}

/// Extract a season "subtitle" — the trailing portion of the target's
/// title after a delimiter like `: ` or ` - `. For example:
/// * "JoJo no Kimyou na Bouken: Stardust Crusaders" → `Some("Stardust Crusaders")`
/// * "Fate/stay night: Unlimited Blade Works" → `Some("Unlimited Blade Works")`
/// * "Fullmetal Alchemist: Brotherhood" → `None` (single-token subtitle)
/// * "JoJo's Bizarre Adventure" → `None` (no delimiter)
/// * "Monogatari Series: Second Season" → `None` (generic ordinal)
///
/// Prefers the English title, falls back to romaji. Rejects single-token
/// subtitles because they substring-match too aggressively, and rejects
/// pure ordinal "Nth Season" phrasings because release filenames almost
/// always carry "S02" / "2nd" rather than the English rendering, so
/// matching on them yields zero hits and forces the full-pack fallback
/// anyway.
pub fn extract_season_subtitle(detail: &AnimeDetail) -> Option<String> {
    let titles = [detail.title_english.as_str(), detail.title_romaji.as_str()];
    for title in titles.iter().filter(|t| !t.is_empty()) {
        if let Some(sub) = trailing_subtitle_of(title) {
            return Some(sub);
        }
    }
    None
}

pub(super) fn trailing_subtitle_of(title: &str) -> Option<String> {
    // Normalize CJK/en/em dashes and colon-space to a common delimiter.
    // Preserving "Re:" / "Fate/" (no space after) means "Re:Zero kara..."
    // and "Fate/stay night" stay intact and only the trailing "`: Sub`"
    // portion gets split off.
    let normalized = title
        .replace(['–', '—'], "|")
        .replace(": ", "|")
        .replace('：', "|")
        .replace(" - ", "|");
    // Take the LAST segment so "A: B: C" resolves to the innermost "C".
    let last = normalized.rsplit('|').next()?.trim();
    if last.is_empty() || last.eq_ignore_ascii_case(title.trim()) {
        return None;
    }
    // Require ≥ 2 whitespace tokens. Single-word subtitles like
    // "Brotherhood" are too generic to reliably narrow a filename list
    // without false positives on unrelated entries in the same pack.
    if last.split_whitespace().count() < 2 {
        return None;
    }
    let lower = last.to_ascii_lowercase();
    if is_generic_season_subtitle(&lower) {
        return None;
    }
    Some(last.to_string())
}

/// Returns true for subtitle phrases that are pure ordinal/numeric
/// season markers (e.g. "Second Season", "2nd Season", "Part 3"). These
/// are rejected by [`extract_season_subtitle`] because release filenames
/// overwhelmingly carry "S02" / "2nd" rather than the English rendering,
/// so substring-matching them produces zero hits and falls back to a
/// full-pack download anyway. Better to skip the selective path.
fn is_generic_season_subtitle(lower: &str) -> bool {
    matches!(
        lower,
        "first season"
            | "second season"
            | "third season"
            | "fourth season"
            | "fifth season"
            | "sixth season"
            | "seventh season"
            | "eighth season"
            | "ninth season"
            | "tenth season"
            | "1st season"
            | "2nd season"
            | "3rd season"
            | "4th season"
            | "5th season"
            | "6th season"
            | "7th season"
            | "8th season"
            | "9th season"
            | "10th season"
    ) || lower.starts_with("part ")
        || lower.starts_with("chapter ")
}

/// True when the target has a discriminator that [`pick_wanted_file_indices`]
/// can use to narrow a megapack — part number or own subtitle. Gate at
/// the call sites so the expensive metadata-wait path is only entered
/// when it has a chance of actually narrowing the file list.
///
/// Franchise roots without their own subtitle (JoJo S1 = "JoJo's
/// Bizarre Adventure") deliberately return `false` here — they're
/// handled by the higher-level multi-series pack auto-expansion path,
/// not by filename-based negative matching, which produces silent
/// wrong-selections when AniList relations only list direct siblings.
pub fn has_selective_discriminator(detail: &AnimeDetail) -> bool {
    extract_part_number(detail).is_some() || extract_season_subtitle(detail).is_some()
}

/// A sibling anime entry detected in the filename list of a megapack
/// torrent — i.e. a related series (sequel, prequel, side story, …)
/// of the parent target whose own files are also present in the pack.
///
/// Produced by [`detect_sibling_entries_in_pack`] and consumed by the
/// library auto-expand path in `handlers::library`, which upserts each
/// sibling into the tracked series table and records per-file routing
/// so post-processing can move each file into the correct media
pub(super) fn normalize_subtitle(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim().to_string()
}

/// Is this filename likely a media file that `release_title`
/// should even be run against? Used by [`pick_wanted_file_indices`] to
/// stop non-media files (NFOs, subtitles, samples) from inflating the
/// media count or being accidentally kept/skipped.
pub(crate) fn is_media_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.rsplit('.').next(),
        Some("mkv")
            | Some("mp4")
            | Some("avi")
            | Some("m2ts")
            | Some("ts")
            | Some("mov")
            | Some("wmv")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::anilist::AnimeDetail;

    fn detail_with_titles(english: &str, romaji: &str) -> AnimeDetail {
        AnimeDetail {
            is_adult: false,
            id: 1,
            id_mal: None,
            title_romaji: romaji.to_string(),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "MOVIE".to_string(),
            status: String::new(),
            status_display: String::new(),
            episodes: Some(1),
            duration: None,
            season: String::new(),
            season_year: None,
            end_year: None,
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    #[test]
    fn extract_part_number_parses_roman_ii() {
        let d = detail_with_titles(
            "Kizumonogatari II: Nekketsu-hen",
            "Kizumonogatari II: Nekketsu-hen",
        );
        assert_eq!(extract_part_number(&d), Some(2));
    }

    #[test]
    fn extract_part_number_parses_roman_iii() {
        let d = detail_with_titles(
            "Kizumonogatari III: Reiketsu-hen",
            "Kizumonogatari III: Reiketsu-hen",
        );
        assert_eq!(extract_part_number(&d), Some(3));
    }

    #[test]
    fn extract_part_number_parses_explicit_part_n() {
        let d = detail_with_titles("Some Show Part 2", "Some Show Part 2");
        assert_eq!(extract_part_number(&d), Some(2));
    }

    #[test]
    fn extract_part_number_returns_none_for_single_entry() {
        // A standalone film with no part marker at all.
        let d = detail_with_titles("A Silent Voice", "Koe no Katachi");
        assert_eq!(extract_part_number(&d), None);
    }

    #[test]
    fn extract_part_number_drops_bare_roman_i_on_kizu_first_entry() {
        // Kizumonogatari I no longer resolves to Some(1) — see the
        // RE_ROMAN_PART docstring. Dropping bare single-letter Romans
        // trades selective narrowing for entry 1 of a trilogy against
        // not false-positiving any title containing an English pronoun
        // "I". Users who want Kizu I specifically still get the whole
        // smol pack (no selective narrowing), which is the acceptable
        // fallback for this edge case.
        let d = detail_with_titles(
            "Kizumonogatari I: Tekketsu-hen",
            "Kizumonogatari I: Tekketsu-hen",
        );
        assert_eq!(extract_part_number(&d), None);
    }

    #[test]
    fn extract_part_number_rejects_bare_roman_on_english_pronoun() {
        // "I Want to Eat Your Pancreas" must NOT resolve to Some(1).
        // Otherwise, if the user ever grabbed a Monogatari-style
        // megapack with this detail as the target, pick_by_part_number
        // would narrow to "files containing episode 1" for an
        // unrelated film. The same concern motivates dropping bare V
        // ("V for Vendetta") and bare X ("X/1999").
        let d = detail_with_titles("I Want to Eat Your Pancreas", "Kimi no Suizou wo Tabetai");
        assert_eq!(extract_part_number(&d), None);
    }

    #[test]
    fn pick_wanted_file_indices_narrows_smol_monogatari_pack() {
        // Simulates the smol Monogatari megapack. The filenames carry
        // standard S09EXX numbering that `release_title` picks
        // up, and the target's part number (from the AniList title's
        // Roman numeral) selects the right file.
        let files = vec![
            "[smol] Monogatari - S09E01 - Kizumonogatari Tekketsu-hen.mkv".to_string(),
            "[smol] Monogatari - S09E02 - Kizumonogatari Nekketsu-hen.mkv".to_string(),
            "[smol] Monogatari - S09E03 - Kizumonogatari Reiketsu-hen.mkv".to_string(),
        ];
        let d = detail_with_titles("Kizumonogatari II: Nekketsu-hen", "Kizumonogatari II");
        let picked = pick_wanted_file_indices(&files, &d).expect("should narrow");
        assert_eq!(picked, vec![1]);
    }

    #[test]
    fn pick_wanted_file_indices_returns_none_when_target_has_no_part() {
        // Standalone single-file release — nothing to narrow against.
        let files = vec!["[Group] Some Film (BD 1080p).mkv".to_string()];
        let d = detail_with_titles("A Silent Voice", "Koe no Katachi");
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn pick_wanted_file_indices_returns_none_when_no_match() {
        // Target is part 2 but the pack doesn't contain an E02 file.
        // Safer to keep everything than to skip every file.
        let files = vec![
            "[Group] Show - 01.mkv".to_string(),
            "[Group] Show - 03.mkv".to_string(),
        ];
        let d = detail_with_titles("Show II", "Show II");
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn pick_wanted_file_indices_ignores_non_media_files() {
        let files = vec![
            "[Group] Pack - 01.mkv".to_string(),
            "[Group] Pack - 02.mkv".to_string(),
            "[Group] Pack - 02.nfo".to_string(),
            "[Group] Pack - 02.txt".to_string(),
        ];
        let d = detail_with_titles("Show II", "Show II");
        let picked = pick_wanted_file_indices(&files, &d).expect("should narrow");
        // Only the E02 .mkv survives — the .nfo and .txt with "02"
        // in their names are discarded before matching.
        assert_eq!(picked, vec![1]);
    }

    #[test]
    fn pick_wanted_file_indices_returns_none_when_every_media_file_matches() {
        // Not actually a megapack — every media file carries the target
        // number (e.g. a single-movie torrent with sample + main file).
        // Don't mess with priorities.
        let files = vec![
            "[Group] Show II (BD 1080p).mkv".to_string(),
            "[Group] Show II (BD 1080p) - sample.mkv".to_string(),
        ];
        let d = detail_with_titles("Show II", "Show II");
        // Neither file carries an episode number, so `release_title`
        // returns an empty set — no matches, None returned.
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    // ── extract_season_subtitle / positive subtitle match ────────────────
    //
    // Covers the second narrowing strategy in `pick_wanted_file_indices` —
    // positive subtitle match for titles with a distinguishing suffix.
    // Franchise roots without their own subtitle (JoJo S1) are NOT
    // narrowed here; they flow through to the multi-series pack
    // auto-expansion path instead.

    #[test]
    fn extract_season_subtitle_pulls_named_season() {
        // Positive case: the English title ends in `: Stardust Crusaders`,
        // which is a distinctive multi-token phrase.
        let d = detail_with_titles(
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "JoJo no Kimyou na Bouken: Stardust Crusaders",
        );
        assert_eq!(
            extract_season_subtitle(&d).as_deref(),
            Some("Stardust Crusaders")
        );
    }

    #[test]
    fn extract_season_subtitle_pulls_from_dash_delimited_title() {
        // En-dash / hyphen-space delimiter also produces a subtitle.
        let d = detail_with_titles("Fate/stay night - Unlimited Blade Works", "");
        assert_eq!(
            extract_season_subtitle(&d).as_deref(),
            Some("Unlimited Blade Works")
        );
    }

    #[test]
    fn extract_season_subtitle_rejects_single_token_suffix() {
        // "Brotherhood" alone is too generic — it could substring-match
        // an unrelated filename fragment. The 2-token minimum blocks it.
        let d = detail_with_titles("Fullmetal Alchemist: Brotherhood", "");
        assert_eq!(extract_season_subtitle(&d), None);
    }

    #[test]
    fn extract_season_subtitle_rejects_generic_ordinal_season() {
        // "Second Season" is a pure ordinal marker — release filenames
        // carry "S02" / "2nd" rather than the English spelling, so
        // matching on this would yield zero hits and fall back to the
        // full pack anyway. Skip it upfront.
        let d = detail_with_titles("Monogatari Series: Second Season", "");
        assert_eq!(extract_season_subtitle(&d), None);
    }

    #[test]
    fn extract_season_subtitle_returns_none_without_delimiter() {
        // Franchise root with no subtitle — handled by the
        // multi-series pack auto-expansion path, not here.
        let d = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        assert_eq!(extract_season_subtitle(&d), None);
    }

    #[test]
    fn extract_season_subtitle_preserves_re_zero_style_colon() {
        // "Re:Zero kara Hajimeru Isekai Seikatsu" — the `:` has no
        // following space, so it should NOT be split. The trailing
        // segment rule returns the whole title, which equals the
        // original → None.
        let d = detail_with_titles("Re:Zero kara Hajimeru Isekai Seikatsu", "");
        assert_eq!(extract_season_subtitle(&d), None);
    }

    #[test]
    fn pick_wanted_file_indices_narrows_by_subtitle_positive_match() {
        // Simulates a JoJo franchise megapack. The target carries its
        // own distinguishing subtitle ("Stardust Crusaders") that
        // appears in the S2 filenames but not in the S1 / S3 / S4 ones.
        let files = vec![
            "[Group] JoJo's Bizarre Adventure - 01.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - 26.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Stardust Crusaders - 01.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Stardust Crusaders - 48.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Diamond is Unbreakable - 01.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Golden Wind - 01.mkv".to_string(),
        ];
        let mut d = detail_with_titles(
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "JoJo no Kimyou na Bouken: Stardust Crusaders",
        );
        // JoJo S2 is 48 episodes — sanity cap (×1.5 + 2 = 74) passes.
        d.episodes = Some(48);
        let picked = pick_wanted_file_indices(&files, &d).expect("should narrow");
        assert_eq!(picked, vec![2, 3]);
    }

    #[test]
    fn pick_wanted_file_indices_returns_none_for_subtitleless_franchise_root() {
        // JoJo S1 — no subtitle, no part number. The selective path is
        // intentionally not used here. The grab handler's multi-series
        // auto-expansion (Phase 2) is what handles this case.
        let files = vec![
            "[Group] JoJo's Bizarre Adventure - 01.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Stardust Crusaders - 01.mkv".to_string(),
        ];
        let d = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn pick_wanted_file_indices_skips_part_narrowing_on_multi_episode_season() {
        // Regression: Mob Psycho 100 III is a 12-episode season whose
        // AniList title ends in a Roman numeral. Without the episode-
        // count gate, `extract_part_number` returns 3 and
        // `pick_by_part_number` latches onto the "...III - 03" file —
        // narrowing a whole-season SeaDex batch to a single-ep pick.
        // The auto-search grab then records episode_numbers=[1] (the
        // target) while qBit only downloads ep 3's file. Gate fix:
        // detail.episodes > 3 → skip part-number narrowing entirely
        // so the full season downloads as intended.
        let files: Vec<String> = (1..=12)
            .map(|i| format!("[SeaDex] Mob Psycho 100 III - {:02}.mkv", i))
            .collect();
        let mut d = detail_with_titles("Mob Psycho 100 III", "Mob Psycho 100 III");
        d.episodes = Some(12);
        assert_eq!(
            pick_wanted_file_indices(&files, &d),
            None,
            "multi-episode season with Roman-numeral title must not be part-narrowed",
        );
    }

    #[test]
    fn pick_wanted_file_indices_still_narrows_trilogy_part() {
        // Counterpart to the regression test above — the Kizumonogatari
        // trilogy (episodes=1 per AniList part) must still narrow.
        let files = vec![
            "[smol] Monogatari - S09E01 - Kizumonogatari Tekketsu-hen.mkv".to_string(),
            "[smol] Monogatari - S09E02 - Kizumonogatari Nekketsu-hen.mkv".to_string(),
            "[smol] Monogatari - S09E03 - Kizumonogatari Reiketsu-hen.mkv".to_string(),
        ];
        let mut d = detail_with_titles("Kizumonogatari II: Nekketsu-hen", "Kizumonogatari II");
        d.episodes = Some(1);
        assert_eq!(pick_wanted_file_indices(&files, &d), Some(vec![1]));
    }

    #[test]
    fn pick_wanted_file_indices_rejects_overshoot_via_episode_cap() {
        // Contrived pathological case: the target's subtitle is a
        // prefix of every file in a much larger pack. Episode count
        // says 12 but the match set is 50 — the cap fires and we
        // return None rather than producing a wildly-wrong narrowing.
        let files: Vec<String> = (1..=50)
            .map(|i| format!("[Group] Show - Alpha Beta Ep{:02}.mkv", i))
            .collect();
        let mut d = detail_with_titles("Show: Alpha Beta", "");
        d.episodes = Some(12);
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn has_selective_discriminator_true_for_part_number_title() {
        let d = detail_with_titles("Kizumonogatari II: Nekketsu-hen", "");
        assert!(has_selective_discriminator(&d));
    }

    #[test]
    fn has_selective_discriminator_true_for_subtitle_title() {
        let d = detail_with_titles("JoJo's Bizarre Adventure: Stardust Crusaders", "");
        assert!(has_selective_discriminator(&d));
    }

    #[test]
    fn has_selective_discriminator_false_for_standalone_single() {
        // No part number, no subtitle — selective path skipped.
        let d = detail_with_titles("A Silent Voice", "Koe no Katachi");
        assert!(!has_selective_discriminator(&d));
    }

    #[test]
    fn has_selective_discriminator_false_for_franchise_root() {
        // Franchise root without its own subtitle — selective path
        // skipped on purpose. Multi-series auto-expansion handles it.
        let d = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        assert!(!has_selective_discriminator(&d));
    }

    // ── #27 — cour-aware reject parser coverage ────────────────────────────
    //
    // The season check's reject is hard, but its parsers were
    // missing `Part N`, `Cour N`, and Roman numerals on the release side,
    // and Roman numerals on the AL side. These tests pin the JJK / Bleach
    // TYBW / Demon Slayer / Made in Abyss corpus so a future change that
    // weakens the parsers shows up as a red test, not as a wrong-cour
    // grab reported by a user.
}
