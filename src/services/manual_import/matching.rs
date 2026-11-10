//! Series matching for the manual-import wizard (#122): turn a parsed
//! title into a ranked list of AniList candidates.
//!
//! The AL call itself is `anilist::search_anime`, which already brings
//! the in-memory search cache, the rate-limit throttle, and the Jikan
//! fallback. This module owns what happens around it: the query string
//! (title plus a "season N" suffix when the files say so), the ranking
//! that reorders AL's `SEARCH_MATCH` results with what the files know
//! (year, file count, season), and the low-confidence flag that makes
//! the preview say "check this one".

use std::sync::LazyLock;

use regex_lite::Regex;

use crate::services::anilist::AnimeEntry;
use crate::services::library_link::share_substantive_token;
use crate::services::release_title;

/// What the files told us about a group, as ranking input.
#[derive(Clone, Debug, Default)]
pub struct RankInput<'a> {
    pub title: &'a str,
    pub season: Option<i32>,
    pub year: Option<i32>,
    pub file_count: usize,
}

/// Lowercase alphanumeric words joined by single spaces. Both sides of
/// every comparison go through this so punctuation and case never
/// count against a match.
pub fn normalize_title(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Grouping key for "these files are the same series": the normalized
/// title, with the season folded in when the files carry one, so
/// `Show S01` and `Show S02` become two groups (AniList lists seasons
/// as separate entries).
pub fn group_key(title: &str, season: Option<i32>) -> String {
    match season {
        Some(n) if n > 1 => format!("{}|s{}", normalize_title(title), n),
        _ => normalize_title(title),
    }
}

/// The AL search string for a group. Season 2+ appends "season N";
/// AL's search handles that phrasing well for the common
/// "2nd Season" / "Season 2" / "II" naming.
pub fn build_query(title: &str, season: Option<i32>) -> String {
    let t = title.trim();
    match season {
        Some(n) if n > 1 => format!("{t} season {n}"),
        _ => t.to_string(),
    }
}

/// Queries to try, in order, when `query` came back empty: the bare
/// title (drops the `season N` suffix), then the title cut at its
/// first ` - ` / `: ` subtitle separator or parenthetical. Each entry
/// is distinct from `query` and from the ones before it.
pub fn fallback_queries(title: &str, query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |q: String| {
        let q = q.trim().to_string();
        if q.len() >= 2
            && normalize_title(&q) != normalize_title(query)
            && !out
                .iter()
                .any(|o| normalize_title(o) == normalize_title(&q))
        {
            out.push(q);
        }
    };
    push(title.to_string());
    let head = title
        .split_once(" - ")
        .or_else(|| title.split_once(": "))
        .map(|(h, _)| h)
        .unwrap_or(title);
    let no_parens: String = {
        let mut depth = 0i32;
        head.chars()
            .filter(|c| {
                match c {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth = (depth - 1).max(0),
                    _ if depth == 0 => return true,
                    _ => {}
                }
                false
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    if normalize_title(&no_parens).chars().count() >= 3 {
        push(no_parens);
    }
    out
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// 0.0..=1.0 similarity of two titles after normalization. Exact
/// containment (one normalized title inside the other) floors at 0.85
/// so `Frieren` vs `Sousou no Frieren` reads as a near-match rather
/// than a 40% edit distance.
pub fn similarity(a: &str, b: &str) -> f32 {
    let na = normalize_title(a);
    let nb = normalize_title(b);
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    let ca: Vec<char> = na.chars().collect();
    let cb: Vec<char> = nb.chars().collect();
    let dist = levenshtein(&ca, &cb) as f32;
    let max_len = ca.len().max(cb.len()) as f32;
    let lev = 1.0 - dist / max_len;
    let contained = na.contains(&nb) || nb.contains(&na);
    if contained { lev.max(0.85) } else { lev }
}

fn best_slot_similarity(title: &str, entry: &AnimeEntry) -> f32 {
    [
        &entry.title_romaji,
        &entry.title_english,
        &entry.title_native,
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .map(|s| similarity(title, s))
    .fold(0.0, f32::max)
}

/// `season 2` / `2nd season` / `II` / `part 2` / trailing ` 2` in any
/// title slot.
static RE_SEASON_MARK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:season\s*(\d{1,2})|(\d{1,2})(?:st|nd|rd|th)\s+season|part\s*(\d{1,2})|\s(\d{1,2})$|\s(ii|iii|iv|v|vi|vii|viii|ix|x)$)")
        .expect("RE_SEASON_MARK compiles")
});

/// Season number an AL title slot advertises, if any.
pub fn season_marker(slot: &str) -> Option<i32> {
    let caps = RE_SEASON_MARK.captures(slot)?;
    for i in 1..=4 {
        if let Some(m) = caps.get(i) {
            return m.as_str().parse().ok();
        }
    }
    caps.get(5)
        .and_then(|m| release_title::roman_value(m.as_str()))
        .filter(|n| *n >= 2)
        .map(|n| n as i32)
}

fn entry_season_marker(entry: &AnimeEntry) -> Option<i32> {
    [&entry.title_romaji, &entry.title_english]
        .into_iter()
        .filter(|s| !s.is_empty())
        .find_map(|s| season_marker(s))
}

/// Combined score for one candidate. Title similarity dominates; the
/// file-derived signals nudge. `position` is AL's own result order and
/// only breaks ties.
pub fn score_entry(input: &RankInput<'_>, entry: &AnimeEntry, position: usize) -> f32 {
    let mut score = best_slot_similarity(input.title, entry);

    let is_movie = entry.format.eq_ignore_ascii_case("MOVIE");
    let is_series = matches!(
        entry.format.to_ascii_uppercase().as_str(),
        "TV" | "TV_SHORT" | "ONA" | "OVA" | "SPECIAL"
    );
    if input.file_count == 1 && is_movie {
        score += 0.10;
    } else if input.file_count >= 3 && is_movie {
        score -= 0.20;
    } else if input.file_count >= 3 && is_series {
        score += 0.05;
    }

    if let (Some(y), Some(sy)) = (input.year, entry.season_year) {
        match (y - sy).abs() {
            0 => score += 0.15,
            1 => score += 0.05,
            2 => {}
            _ => score -= 0.10,
        }
    }

    match (input.season, entry_season_marker(entry)) {
        (Some(want), Some(have)) if want > 1 && want == have => score += 0.15,
        (Some(want), Some(have)) if want > 1 && want != have => score -= 0.10,
        (Some(want), None) if want > 1 => score -= 0.05,
        // Files say season 1 (or nothing); an entry advertising a
        // later season is probably the wrong one.
        (Some(1) | None, Some(_)) => score -= 0.10,
        _ => {}
    }

    score - 0.02 * position as f32
}

/// Reorder AL's results by [`score_entry`], best first. Stable, so
/// equal scores keep AL's order.
pub fn rank_entries(input: &RankInput<'_>, entries: Vec<AnimeEntry>) -> Vec<AnimeEntry> {
    let mut scored: Vec<(f32, usize, AnimeEntry)> = entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| (score_entry(input, &e, i), i, e))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(_, _, e)| e).collect()
}

/// True when the top pick deserves a "check this" flag: no substantive
/// token in common with the parsed title (the grab resolver's own
/// false-positive guard) or a weak best-slot similarity.
pub fn is_low_confidence(title: &str, entry: &AnimeEntry) -> bool {
    let shares = [
        &entry.title_romaji,
        &entry.title_english,
        &entry.title_native,
    ]
    .iter()
    .any(|slot| !slot.is_empty() && share_substantive_token(title, slot));
    !shares || best_slot_similarity(title, entry) < 0.45
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, romaji: &str, english: &str, format: &str, year: Option<i32>) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: romaji.to_string(),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: format.to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes: Some(12),
            season_year: year,
            source: "anilist".to_string(),
            average_score: None,
        }
    }

    #[test]
    fn normalize_and_group_key() {
        assert_eq!(normalize_title("Sousou no Frieren!"), "sousou no frieren");
        assert_eq!(group_key("Mob Psycho 100", Some(1)), "mob psycho 100");
        assert_eq!(group_key("Mob Psycho 100", Some(2)), "mob psycho 100|s2");
        assert_eq!(
            build_query("Mob Psycho 100", Some(2)),
            "Mob Psycho 100 season 2"
        );
        assert_eq!(build_query(" Naruto ", None), "Naruto");
    }

    #[test]
    fn fallback_queries_drop_the_season_suffix_then_the_subtitle() {
        assert_eq!(
            fallback_queries("Enen no Shouboutai", "Enen no Shouboutai season 3"),
            vec!["Enen no Shouboutai"]
        );
        assert_eq!(
            fallback_queries("Frieren - Sousou no Frieren", "Frieren - Sousou no Frieren"),
            vec!["Frieren"]
        );
        assert_eq!(
            fallback_queries("Show (2019)", "Show (2019) season 2"),
            vec!["Show (2019)", "Show"]
        );
        // Nothing left to try when the bare title IS the query.
        assert!(fallback_queries("Naruto", "Naruto").is_empty());
    }

    #[test]
    fn similarity_basics() {
        assert_eq!(similarity("Naruto", "NARUTO"), 1.0);
        assert!(similarity("Frieren", "Sousou no Frieren") >= 0.85);
        assert!(similarity("Naruto", "Bleach") < 0.4);
        assert_eq!(similarity("", "Bleach"), 0.0);
    }

    #[test]
    fn year_and_format_reorder_al_results() {
        // AL returns the 1999 series first; files say 2011 and there
        // are many of them, so the 2011 TV entry should win.
        let input = RankInput {
            title: "Hunter x Hunter",
            season: None,
            year: Some(2011),
            file_count: 24,
        };
        let entries = vec![
            entry(1, "Hunter x Hunter", "Hunter x Hunter", "TV", Some(1999)),
            entry(
                2,
                "Hunter x Hunter (2011)",
                "Hunter x Hunter",
                "TV",
                Some(2011),
            ),
            entry(
                3,
                "Hunter x Hunter Movie",
                "Hunter x Hunter: Phantom Rouge",
                "MOVIE",
                Some(2013),
            ),
        ];
        let ranked = rank_entries(&input, entries);
        assert_eq!(ranked[0].id, 2);
        assert_eq!(
            ranked.last().unwrap().id,
            3,
            "movie sinks under a 24-file group"
        );
    }

    #[test]
    fn single_file_prefers_movie() {
        let input = RankInput {
            title: "Your Name",
            season: None,
            year: None,
            file_count: 1,
        };
        let entries = vec![
            entry(
                1,
                "Kimi no Na wa. Another Side",
                "Your Name Another Side",
                "TV",
                Some(2017),
            ),
            entry(2, "Kimi no Na wa.", "Your Name.", "MOVIE", Some(2016)),
        ];
        let ranked = rank_entries(&input, entries);
        assert_eq!(ranked[0].id, 2);
    }

    #[test]
    fn season_marker_matching() {
        assert_eq!(season_marker("Mob Psycho 100 II"), Some(2));
        assert_eq!(season_marker("Mob Psycho 100 Season 3"), Some(3));
        assert_eq!(season_marker("Yuru Camp 2nd Season"), Some(2));
        assert_eq!(season_marker("Mob Psycho 100"), None);

        let input = RankInput {
            title: "Mob Psycho 100",
            season: Some(2),
            year: None,
            file_count: 13,
        };
        let entries = vec![
            entry(1, "Mob Psycho 100", "Mob Psycho 100", "TV", Some(2016)),
            entry(
                2,
                "Mob Psycho 100 II",
                "Mob Psycho 100 II",
                "TV",
                Some(2019),
            ),
            entry(
                3,
                "Mob Psycho 100 III",
                "Mob Psycho 100 III",
                "TV",
                Some(2022),
            ),
        ];
        let ranked = rank_entries(&input, entries);
        assert_eq!(ranked[0].id, 2);

        // Season 1 files should not pick a later season.
        let input = RankInput {
            title: "Mob Psycho 100",
            season: Some(1),
            year: None,
            file_count: 12,
        };
        let entries = vec![
            entry(
                2,
                "Mob Psycho 100 II",
                "Mob Psycho 100 II",
                "TV",
                Some(2019),
            ),
            entry(1, "Mob Psycho 100", "Mob Psycho 100", "TV", Some(2016)),
        ];
        let ranked = rank_entries(&input, entries);
        assert_eq!(ranked[0].id, 1);
    }

    #[test]
    fn low_confidence_flags_unrelated_top_hit() {
        let e = entry(1, "Bleach", "Bleach", "TV", None);
        assert!(is_low_confidence("Naruto", &e));
        let e = entry(2, "Naruto", "Naruto", "TV", None);
        assert!(!is_low_confidence("Naruto", &e));
        // Shares a token but barely resembles: still flagged.
        let e = entry(3, "Boruto: Naruto Next Generations", "", "TV", None);
        assert!(is_low_confidence("Naruto Shippuden", &e));
        // Containment counts as plausible: a franchise entry that
        // embeds the parsed title is a real candidate, and the
        // ranking (file count vs movie) sorts it below the series.
        let e = entry(
            4,
            "Naruto: Shippuuden Movie 4 - The Lost Tower",
            "",
            "MOVIE",
            None,
        );
        assert!(!is_low_confidence("Naruto", &e));
    }
}
