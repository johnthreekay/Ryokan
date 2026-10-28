//! Frozen copies of the release-title parsers `release_title` replaces,
//! taken verbatim from `dev` at 966695a (RSS `parse_release` reads a
//! `&str` and keeps stand-ins for its alias views). The old file-name
//! branches went with the anitomy crate (phase 12e); the filename corpus
//! and its per-file notes pin the new reading. Test-only: the
//! differential compares them with the new parser while call sites
//! migrate, and the copies go when the last old parser is deleted. Fixes
//! belong in `release_title`, never here.

#![allow(dead_code, clippy::all)]

pub(crate) mod scraper {
    use std::sync::LazyLock;

    static BATCH_RANGE_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"(?i)\b\d{1,3}\s*[-~]\s*\d{2,3}\b").expect("BATCH_RANGE_RE parses")
    });

    static SEASON_MARKER_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"(?i)\b(s\d{1,2}|season\s*\d+)\b").expect("SEASON_MARKER_RE parses")
    });

    static ROMAN_SEASON_MARKER_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"\b(II|III|IV|VI|VII|VIII|IX)\b")
            .expect("ROMAN_SEASON_MARKER_RE parses")
    });

    static SINGLE_EP_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(
            // The dash-number form takes a half-episode `.5` and a
            // revision `v2` tail: `Show Season 2 - 05v2` is a weekly, not
            // a season pack (`\b` alone does not sit between `5` and `v`).
            r"(?i)(s\d{1,2}e\d{1,3}|\s-\s*\d{1,3}(?:\.\d+)?(?:v\d+)?\b|\bep\.?\s*\d{1,3}\b|\bepisode\s*\d{1,3}\b|#\d{1,3})",
        )
        .expect("SINGLE_EP_RE parses")
    });

    pub(crate) fn detect_batch(title: &str) -> bool {
        let lower = title.to_lowercase();

        // Explicit batch keywords.
        if lower.contains("batch") || lower.contains("complete") {
            return true;
        }

        // Numeric episode ranges like "01-12", "01~24", "01 - 24". A range
        // ascends, and a spaced one pairs numbers of one width: in
        // `Chihayafuru 2 - 05`, `Mob Psycho 100 - 05`, and `Season 2 - 05`
        // (season markers are masked first) the left number is the
        // title's own, the shape every weekly of such a series has. The
        // dash between a title and its episode is spaced; a pack's range
        // is `01-12` or `01 - 12`.
        if has_plausible_range(&SEASON_MARKER_RE.replace_all(&lower, " ")) {
            return true;
        }

        // Season marker with no single-episode indicator — the dominant
        // batch form for BD season packs: `Show S1 (BD 1080p)`, `Show
        // [Season 1] [BD 1080p]`, `Show.S01.1080p.BluRay...`,
        // `Mob Psycho 100 III (BD 1080p)`. The single-ep guard keeps
        // `Show S01E12` / `Show S1 - 12` off the batch path.
        //
        // The Roman-numeral check runs against the raw title (not `lower`)
        // because the regex is case-sensitive — see ROMAN_SEASON_MARKER_RE.
        let has_season_marker =
            SEASON_MARKER_RE.is_match(&lower) || ROMAN_SEASON_MARKER_RE.is_match(title);
        if has_season_marker && !SINGLE_EP_RE.is_match(&lower) {
            return true;
        }

        false
    }

    pub(crate) fn has_plausible_range(text: &str) -> bool {
        BATCH_RANGE_RE.find_iter(text).any(|m| {
            let found = m.as_str();
            let spaced = found.contains(char::is_whitespace);
            let mut parts = found.split(['-', '~']);
            let (Some(left), Some(right)) = (parts.next(), parts.next()) else {
                return false;
            };
            let (left, right) = (left.trim(), right.trim());
            let (Ok(l), Ok(r)) = (left.parse::<u32>(), right.parse::<u32>()) else {
                return false;
            };
            l < r && (!spaced || left.len() == right.len())
        })
    }
}

pub(crate) mod feed {
    use regex_lite::Regex;
    use std::sync::LazyLock;

    static RE_BATCH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(?:e?\d{1,4}|s\d{1,2}e\d{1,4})\s*[-~]\s*(?:e?\d{1,4}|\d{1,4})\b").unwrap()
    });

    static RE_BATCH_LEFT_DIGITS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\d{1,4}").unwrap());

    static RE_BATCH_RIGHT_AFTER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:v\d+)?\s*[-~]\s*(?:e)?(\d{1,4})(?:v\d+)?\b").unwrap());

    pub(crate) fn has_valid_batch_range(text: &str) -> bool {
        let bytes = text.as_bytes();
        for m in RE_BATCH_LEFT_DIGITS.find_iter(text) {
            // Word-boundary check at the left digit's start — mirrors the
            // `\b` RE_BATCH uses on the left side. A digit embedded in a
            // word (`bd1080p`) is not a range candidate.
            if m.start() > 0 {
                let prev = bytes[m.start() - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' {
                    continue;
                }
            }
            let Ok(left) = m.as_str().parse::<u32>() else {
                continue;
            };
            if left == 0 {
                continue;
            }
            let tail = &text[m.end()..];
            let Some(cap) = RE_BATCH_RIGHT_AFTER.captures(tail) else {
                continue;
            };
            let Some(right_digits) = cap.get(1) else {
                continue;
            };
            let Ok(right) = right_digits.as_str().parse::<u32>() else {
                continue;
            };
            // A spaced range pairs numbers of one width (`01 - 12`): with
            // `Chihayafuru 2 - 05` or `Mob Psycho 100 - 05` the left number
            // is the title's own beside the episode, the shape every
            // weekly of such a series has. Same rule as the Nyaa scraper.
            let spaced = cap
                .get(0)
                .is_some_and(|whole| whole.as_str().contains(char::is_whitespace));
            if left < right && (!spaced || m.as_str().len() == right_digits.as_str().len()) {
                return true;
            }
        }
        false
    }

    static RE_BATCH_SEASON_BRACKET: LazyLock<Regex> = LazyLock::new(|| {
        let alternation = super::rss::SEASON_TOKEN_FRAGMENTS.join("|");
        Regex::new(&format!(r"(?i)\b(?:{})\s*[(\[]", alternation)).unwrap()
    });

    pub(crate) fn detect_batch(title: &str) -> bool {
        let lower = title.to_lowercase();
        // Mask season markers before running RE_BATCH's `\d+-\d+` range
        // regex — otherwise titles like "Season 3 - 05" match as a spurious
        // range "3 - 05". Same trick `parse_release` uses before RE_ABSOLUTE.
        // Keep the original `lower` for the other predicates: RE_BATCH_
        // SEASON_BRACKET needs the season markers intact (it's literally
        // matching them), and the " batch" / " complete" / etc. substrings
        // don't overlap with season markers anyway.
        let mut masked = lower.clone();
        for re in super::rss::RE_SEASON_MARKER_MASK.iter() {
            masked = re.replace_all(&masked, " ").to_string();
        }
        (RE_BATCH.is_match(&masked) && has_valid_batch_range(&masked))
            || RE_BATCH_SEASON_BRACKET.is_match(&lower)
            || lower.contains(" batch")
            || lower.contains(" complete")
            || lower.contains(" mini batch")
            || lower.contains(" full season")
            || lower.contains("全集")
    }

    pub(crate) fn extract_group(title: &str) -> String {
        if let Some(start) = title.find('[')
            && let Some(end) = title[start..].find(']')
        {
            return title[start + 1..start + end].to_string();
        }
        String::new()
    }

    pub(crate) fn extract_resolution(title: &str) -> String {
        let lower = title.to_lowercase();
        for res in ["2160", "1080", "720", "576", "480"] {
            if lower.contains(&format!("{}p", res)) || lower.contains(&format!(" {} ", res)) {
                return res.to_string();
            }
        }
        String::new()
    }
}

pub(crate) mod indexers {
    pub(crate) fn extract_group_from_title(title: &str) -> String {
        // First `[Group]` bracket is the convention. Second-bracket
        // (e.g., `[BD 1080p]`) is metadata; ignore. Empty when no
        // bracketed group is present.
        let bytes = title.as_bytes();
        let mut start = None;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'[' {
                start = Some(i + 1);
            } else if b == b']'
                && let Some(s) = start
            {
                let inner = &title[s..i];
                if !inner.is_empty() && !inner.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    return inner.to_string();
                }
                start = None;
            }
        }
        String::new()
    }

    pub(crate) fn extract_resolution_from_title(title: &str) -> String {
        // PR #107 review fix #10: case-insensitive single pass instead
        // of two separate lists. Covers 2160P / 480P uppercase that the
        // earlier list missed.
        let lower = title.to_ascii_lowercase();
        for needle in ["2160p", "1080p", "720p", "480p"] {
            if lower.contains(needle) {
                let digits: String = needle.chars().take_while(|c| c.is_ascii_digit()).collect();
                return digits;
            }
        }
        String::new()
    }

    pub(crate) fn detect_batch_from_title(title: &str) -> bool {
        // PR #107 review fix #9: tighten "complete" matching with a
        // word-boundary-equivalent check. The earlier substring scan
        // matched "Incomplete Edition" and similar negated forms.
        // Now require the keyword to be flanked by whitespace, brackets,
        // or string boundaries — matches the way real release titles
        // tag a batch ("Show - Complete", "[Group] Show Complete BD").
        let lower = title.to_ascii_lowercase();
        if lower.contains("[bd]") || lower.contains("(bd)") || lower.contains("season pack") {
            return true;
        }
        for needle in ["batch", "complete"] {
            if let Some(idx) = lower.find(needle) {
                let before_ok = idx == 0
                    || lower
                        .as_bytes()
                        .get(idx - 1)
                        .is_some_and(|&b| !b.is_ascii_alphabetic());
                let after = idx + needle.len();
                let after_ok = after == lower.len()
                    || lower
                        .as_bytes()
                        .get(after)
                        .is_some_and(|&b| !b.is_ascii_alphabetic());
                if before_ok && after_ok {
                    return true;
                }
            }
        }
        false
    }
}

pub(crate) mod release_parse {
    use regex_lite::Regex;
    use std::collections::HashSet;
    use std::sync::LazyLock;

    static RE_EPISODE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        vec![
            // S01E05 style
            Regex::new(r"s\d{1,2}e(\d{1,4})").unwrap(),
            // E05 / Ep05 / Ep.05 style
            Regex::new(r"(?:^|[\s._\-])e(?:p\.?)?(\d{1,4})(?:v\d)?(?:\s|\.|\[|\(|$)").unwrap(),
            // " - 05" style (common for fansubs)
            Regex::new(r"(?:^|\s)-\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
            // "Episode 05"
            Regex::new(r"episode\s*(\d{1,4})").unwrap(),
        ]
    });

    static RE_RANGE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:^|[\s._\-])(\d{1,3})\s*[-~]\s*(\d{1,3})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap()
    });

    static RE_SXXEXX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"s(\d{1,2})e\d{1,4}").unwrap());

    static RE_STANDALONE_S: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|[\s.\[\(])s(\d{1,2})(?:[\s.\]\)\-]|$)").unwrap());

    static RE_RELEASE_SEASON_N: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"season\s*(\d+)").unwrap());

    static RE_RELEASE_NTH_SEASON: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d+)(?:st|nd|rd|th)\s+season").unwrap());

    static RE_RELEASE_PART_COUR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b(?:part|cour)\s+(\d+)").unwrap());

    static RE_RELEASE_ROMAN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(ii{1,2}|iv|vi{1,3}|ix)\b(?:\s*[:\-\s\.\]\)\,]|$)").unwrap()
    });

    pub(crate) fn is_noise_number(n: i32) -> bool {
        // Year range only. Resolution values (480/576/720/1080/2160) and
        // codec markers (264/265) used to live here as belt-and-suspenders
        // protection, but `parse_release_numbers` already strips bracketed
        // and parenthesized content (where every well-formed release puts
        // its resolution / codec tags) before running these regexes — so
        // an unbracketed `1080` *can* only be a real episode number, e.g.
        // One Piece 1080. Filtering it out as "noise" was silently making
        // those specific episodes ungrabbable for long-running shows. The
        // 1900..=2100 year range stays because release titles legitimately
        // carry an unbracketed year token (`Show 2024 BD`) that the dash-
        // number regex would otherwise capture as an episode.
        (1900..=2100).contains(&n)
    }

    pub fn parse_release_numbers(title: &str) -> HashSet<i32> {
        let lower = title.to_lowercase();
        let mut numbers = HashSet::new();

        // Strip bracketed content first to avoid matching metadata like [1080p] or (2024)
        let stripped = {
            let mut out = String::with_capacity(lower.len());
            let mut depth = 0i32;
            for ch in lower.chars() {
                match ch {
                    '[' | '(' | '{' => depth += 1,
                    ']' | ')' | '}' => depth = (depth - 1).max(0),
                    _ if depth > 0 => continue,
                    _ => out.push(ch),
                }
            }
            out
        };

        for re in RE_EPISODE_PATTERNS.iter() {
            for caps in re.captures_iter(&stripped) {
                if let Some(value) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok())
                    && !is_noise_number(value)
                {
                    numbers.insert(value);
                }
            }
        }

        // Range pattern for batch detection (e.g. "01-12", "01~24")
        // Only add range numbers, not used as the sole episode match.
        if let Some(caps) = RE_RANGE.captures(&stripped) {
            let start = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .unwrap_or(0);
            let end = caps
                .get(2)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .unwrap_or(0);
            if start > 0
                && end >= start
                && end - start <= 200
                && !is_noise_number(start)
                && !is_noise_number(end)
            {
                for value in start..=end {
                    numbers.insert(value);
                }
            }
        }

        numbers
    }

    pub fn parse_release_season(title: &str) -> i32 {
        let lower = title.to_lowercase();

        // S01E05, S02E03, etc.
        if let Some(caps) = RE_SXXEXX.captures(&lower)
            && let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok())
        {
            return n;
        }

        // Standalone "S2", "S3" (not part of resolution like "S01E01")
        if let Some(caps) = RE_STANDALONE_S.captures(&lower)
            && let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok())
            && n > 0
            && n <= 30
        {
            return n;
        }

        // "Season 2", "Season 3"
        if let Some(caps) = RE_RELEASE_SEASON_N.captures(&lower)
            && let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok())
        {
            return n;
        }

        // "2nd Season", "3rd Season"
        if let Some(caps) = RE_RELEASE_NTH_SEASON.captures(&lower)
            && let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok())
        {
            return n;
        }

        // "Part 2", "Cour 2"
        if let Some(caps) = RE_RELEASE_PART_COUR.captures(&lower)
            && let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok())
            && n >= 2
        {
            return n;
        }

        // Roman numeral II–IX.
        if let Some(caps) = RE_RELEASE_ROMAN.captures(&lower)
            && let Some(m) = caps.get(1)
        {
            let n = roman_to_i32(m.as_str());
            if n >= 2 {
                return n;
            }
        }

        0
    }

    pub(crate) fn roman_to_i32(s: &str) -> i32 {
        match s.to_ascii_lowercase().as_str() {
            "ii" => 2,
            "iii" => 3,
            "iv" => 4,
            "vi" => 6,
            "vii" => 7,
            "viii" => 8,
            "ix" => 9,
            _ => 0,
        }
    }
}

pub(crate) mod rss {
    use regex_lite::Regex;
    use std::collections::HashSet;
    use std::sync::LazyLock;

    /// The fields `parse_release` fills; the alias views are stand-ins here.
    #[derive(Debug, Clone)]
    pub(crate) struct ParsedRelease {
        pub normalized_title: String,
        pub core_title: String,
        pub collapsed_title: String,
        pub collapsed_core_title: String,
        pub season_hint: Option<i32>,
        pub season_relative_eps: HashSet<i32>,
        pub absolute_eps: HashSet<i32>,
        pub parse_mode: &'static str,
    }

    struct RssItem {
        title: String,
    }

    fn normalize_core_title(s: &str) -> String {
        s.to_string()
    }

    fn collapse_alias(s: &str) -> String {
        s.to_string()
    }

    static RE_SEASON_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        vec![
            Regex::new(r"(?i)\bseason\s*(\d{1,2})\b").unwrap(),
            Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)\s+season\b").unwrap(),
            Regex::new(r"(?i)\bs(\d{1,2})\b").unwrap(),
            Regex::new(r"(?i)\bpart\s*(\d{1,2})\b").unwrap(),
            Regex::new(r"(?i)\bcour\s*(\d{1,2})\b").unwrap(),
        ]
    });

    static RE_SEASON_EP_RANGE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        vec![
        Regex::new(r"(?i)\bs(\d{1,2})\s*e(\d{1,4})\s*[-~]\s*e?(\d{1,4})(?:v\d+)?\b").unwrap(),
        Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)\s+season\b\s*[-:]\s*(\d{1,4})\s*[-~]\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
        Regex::new(r"(?i)\bseason\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})\s*[-~]\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
        Regex::new(r"(?i)\bpart\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})\s*[-~]\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
        Regex::new(r"(?i)\bcour\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})\s*[-~]\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
    ]
    });

    static RE_SEASON_EP_SINGLE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        vec![
            Regex::new(r"(?i)\bs(\d{1,2})\s*e(\d{1,4})(?:v\d+)?\b").unwrap(),
            Regex::new(r"(?i)\bs(\d{1,2})[ ._-]*ep?(\d{1,4})(?:v\d+)?\b").unwrap(),
        ]
    });

    static RE_SEASON_DASH: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        vec![
        Regex::new(r"(?i)\bs(\d{1,2})\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
        Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)\s+season\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
        Regex::new(r"(?i)\bseason\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
        Regex::new(r"(?i)\bpart\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
        Regex::new(r"(?i)\bcour\s*(\d{1,2})\b\s*[-:]\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
    ]
    });

    static RE_RANGE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\b(\d{1,3})\s*[-~]\s*(\d{1,3})(?:v\d+)?\b").unwrap());

    static RE_ABSOLUTE: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
        vec![
            (
                "absolute_dash",
                Regex::new(r"(?i)(?:^|\s)-\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
            ),
            (
                "absolute",
                Regex::new(r"(?i)\bepisode\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
            ),
            (
                "absolute",
                Regex::new(r"(?i)\be(?:p\.?|pisode)?\s*(\d{1,4})(?:v\d+)?\b").unwrap(),
            ),
            (
                "absolute",
                Regex::new(r"(?i)\b(\d{1,4})(?:v\d+)?\s*(?:\(|\[)").unwrap(),
            ),
            (
                "absolute_dash",
                Regex::new(
                    r"(?i)\b-\s*(\d{1,4})(?:v\d+)?(?:\s+final|\s+end)?(?:\.[a-z0-9]{2,4}|$)",
                )
                .unwrap(),
            ),
            (
                "absolute",
                Regex::new(r"(?i)\b(\d{1,4})(?:v\d+)?\s*(?:final|end)\b").unwrap(),
            ),
        ]
    });

    pub(crate) const SEASON_TOKEN_FRAGMENTS: &[&str] = &[
        r"season\s*\d{1,2}",
        r"\d{1,2}(?:st|nd|rd|th)\s+season",
        r"part\s*\d{1,2}",
        r"cour\s*\d{1,2}",
        r"s\d{1,2}",
    ];

    pub(crate) static RE_SEASON_MARKER_MASK: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        SEASON_TOKEN_FRAGMENTS
            .iter()
            .map(|frag| Regex::new(&format!(r"(?i)\b{}\b", frag)).unwrap())
            .collect()
    });

    pub(crate) fn parse_season_number(value: &str) -> Option<i32> {
        for re in RE_SEASON_PATTERNS.iter() {
            if let Some(value) = re
                .captures(value)
                .and_then(|caps| caps.get(1))
                .and_then(|m| m.as_str().parse::<i32>().ok())
            {
                return Some(value);
            }
        }
        None
    }

    pub(crate) fn parse_release(title: &str) -> ParsedRelease {
        let item = RssItem {
            title: title.to_string(),
        };
        let normalized_title = crate::services::auto_search::normalize_title(&item.title);
        let core_title = normalize_core_title(&normalized_title);
        let collapsed_title = collapse_alias(&normalized_title);
        let collapsed_core_title = collapse_alias(&core_title);
        let lower = item.title.to_lowercase();

        let mut season_hint = parse_season_number(&normalized_title);
        let mut season_relative_eps = HashSet::new();
        let mut absolute_eps = HashSet::new();
        let mut parse_mode = "unknown";

        // Season+episode range patterns
        for re in RE_SEASON_EP_RANGE.iter() {
            if let Some(caps) = re.captures(&lower) {
                season_hint = caps
                    .get(1)
                    .and_then(|m| m.as_str().parse::<i32>().ok())
                    .or(season_hint);
                let start = caps
                    .get(2)
                    .and_then(|m| m.as_str().parse::<i32>().ok())
                    .unwrap_or(0);
                let end = caps
                    .get(3)
                    .and_then(|m| m.as_str().parse::<i32>().ok())
                    .unwrap_or(0);
                if start > 0 && end >= start && end - start <= 200 {
                    for ep in start..=end {
                        season_relative_eps.insert(ep);
                    }
                    parse_mode = "season_episode_range";
                    return ParsedRelease {
                        normalized_title,
                        core_title,
                        collapsed_title,
                        collapsed_core_title,
                        season_hint,
                        season_relative_eps,
                        absolute_eps,
                        parse_mode,
                    };
                }
            }
        }

        // Season+episode single patterns
        for re in RE_SEASON_EP_SINGLE.iter() {
            if let Some(caps) = re.captures(&lower) {
                season_hint = caps
                    .get(1)
                    .and_then(|m| m.as_str().parse::<i32>().ok())
                    .or(season_hint);
                if let Some(ep) = caps.get(2).and_then(|m| m.as_str().parse::<i32>().ok()) {
                    season_relative_eps.insert(ep);
                    parse_mode = "season_episode";
                    return ParsedRelease {
                        normalized_title,
                        core_title,
                        collapsed_title,
                        collapsed_core_title,
                        season_hint,
                        season_relative_eps,
                        absolute_eps,
                        parse_mode,
                    };
                }
            }
        }

        // Season+dash+episode patterns
        for re in RE_SEASON_DASH.iter() {
            if let Some(caps) = re.captures(&lower) {
                season_hint = caps
                    .get(1)
                    .and_then(|m| m.as_str().parse::<i32>().ok())
                    .or(season_hint);
                if let Some(ep) = caps.get(2).and_then(|m| m.as_str().parse::<i32>().ok()) {
                    season_relative_eps.insert(ep);
                    parse_mode = "season_dash_episode";
                    return ParsedRelease {
                        normalized_title,
                        core_title,
                        collapsed_title,
                        collapsed_core_title,
                        season_hint,
                        season_relative_eps,
                        absolute_eps,
                        parse_mode,
                    };
                }
            }
        }

        // Mask season markers out of the search window for the absolute-
        // episode and plain-range passes. See RE_SEASON_MARKER_MASK.
        let mut masked = lower.clone();
        for re in RE_SEASON_MARKER_MASK.iter() {
            masked = re.replace_all(&masked, " ").to_string();
        }

        // Plain range (no season prefix)
        if let Some(caps) = RE_RANGE.captures(&masked) {
            let start = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .unwrap_or(0);
            let end = caps
                .get(2)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .unwrap_or(0);
            if start > 0 && end >= start && end - start <= 200 {
                for ep in start..=end {
                    absolute_eps.insert(ep);
                }
                parse_mode = "range";
            }
        }

        // Absolute episode patterns (run against the season-masked title
        // so e.g. the "3" in "Season 3 (web ..." doesn't get picked up as
        // absolute episode 3 via the digit-before-paren pattern).
        if absolute_eps.is_empty() {
            for (mode, re) in RE_ABSOLUTE.iter() {
                for caps in re.captures_iter(&masked) {
                    if let Some(value) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
                        absolute_eps.insert(value);
                    }
                }
                if !absolute_eps.is_empty() {
                    parse_mode = mode;
                    break;
                }
            }
        }

        ParsedRelease {
            normalized_title,
            core_title,
            collapsed_title,
            collapsed_core_title,
            season_hint,
            season_relative_eps,
            absolute_eps,
            parse_mode,
        }
    }
}
