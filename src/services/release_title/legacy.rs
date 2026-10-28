//! Frozen copies of the release-title parsers `release_title` replaces,
//! taken verbatim from `dev` at 966695a (RSS `parse_release` reads a
//! `&str` and keeps stand-ins for its alias views; `media` shares the
//! live `EpisodeSpan` type so the two readings compare directly). Test-only: the
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

/// `media::parse_episode_span` (the file-name parser) as it was before
/// `release_title::file_mode` replaced its branches.
pub(crate) mod media {
    use std::sync::LazyLock;

    use anitomy::ElementCategory;
    use regex_lite::Regex;

    use crate::services::media::anitomy_parse;
    use crate::services::release_title::file_mode::{EpisodeSpan, MAX_FILE_SPAN};

    static RE_SXEX: LazyLock<Regex> = LazyLock::new(|| {
        // The tail must be followed by a non-alphanumeric or the end so
        // `S01E05-1080p` stays a single episode.
        Regex::new(r"s(\d{1,2})e(\d{1,4})(?:(?:(?:-e|-|e)(\d{1,4})(?:v\d)?)+(?:[^a-z0-9]|$))?")
            .expect("RE_SXEX compiles")
    });

    static RE_DASH_EP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r" - (\d{1,4})(?:v\d)?(?:[-~](\d{1,4})(?:v\d)?)*(?:\s|\.|\[|\(|$)")
            .expect("RE_DASH_EP compiles")
    });

    static RE_E_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?:^|[\s._\-])e(?:p\.?)?(\d{1,4})(?:v\d)?(?:[-~]e?(?:p\.?)?(\d{1,4})(?:v\d)?)*(?:\s|\.|\[|\(|$)",
        )
        .expect("RE_E_PREFIX compiles")
    });

    static RE_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"episode\s*(\d{1,4})(?:[-~](\d{1,4}))?").expect("RE_EPISODE compiles")
    });

    static RE_BARE_NUM_DASH: LazyLock<Regex> = LazyLock::new(|| {
        // Bare 1-3 digit episode number followed by ` - <subtitle>`. Used
        // by no-group-prefix releases shaped like
        // `<title> NN - <subtitle>.mkv`. The caller takes the FIRST match
        // because the actual episode marker comes BEFORE the subtitle, and
        // the subtitle itself can contain incidental ` <number> - ` runs
        // (e.g. `Show 03 - Title, Part 4 - Coda.mkv` → must resolve 03,
        // not 4). 1-3 digits keeps 4-digit years out.
        Regex::new(r"(?:^|\s)(\d{1,3})(?:v\d)?\s+-\s").expect("RE_BARE_NUM_DASH compiles")
    });

    static RE_BARE_NUM_BRACKET: LazyLock<Regex> = LazyLock::new(|| {
        // Bare 1-3 digit episode number followed by a quality `(...)` or
        // hash `[...]` bracket. Catches packs whose filenames are
        // space-delimited rather than `S01E01` or `- 01` — i.e. shaped
        // like `<title tokens> NN (<quality>) [<hash>].mkv`. 1-3 digits
        // keeps 4-digit years from being read as episode numbers. The
        // caller picks the rightmost match so a title token like
        // `something 99 (extra) 12 (...)` still resolves to 12, not 99.
        Regex::new(r"(?:^|\s)(\d{1,3})(?:v\d)?\s+[\(\[]").expect("RE_BARE_NUM_BRACKET compiles")
    });

    static RE_NCOP_NCED: LazyLock<Regex> = LazyLock::new(|| {
        // Creditless OP/ED marker that must NOT be a substring of an
        // unrelated word. Plain `contains("nced")` trips on any filename
        // containing "synced", "convinced", "announced", "experienced",
        // "bounced", etc. — we anchor the left edge to start-of-string or
        // a non-letter so only real `NCED`/`NCOP` tokens match. The right
        // edge is left open so glued-digit forms like `NCED01a` / `NCOP01`
        // (LOGH, some older BDMV rips) still resolve.
        Regex::new(r"(?:^|[^a-z])(?:ncop|nced)").expect("RE_NCOP_NCED compiles")
    });

    static RE_OVA_EP: LazyLock<Regex> = LazyLock::new(|| {
        // Explicit `OVA NN` / `OVA01` episode marker — used by
        // multi-episode OVA releases (e.g. the JoJo 1993 / 2000 6-episode
        // OVA sets shaped `<title> - OVA 01.mkv`). Must fire BEFORE the
        // bare-number branches so `- OVA 01.` resolves to 1 instead of
        // being shadowed by an earlier title-side digit. Must NOT match
        // bare `- OVA.` with no trailing digit — single-OVA AL entries
        // (e.g. Nichijou no 0) legitimately have no episode number and
        // should fall through to `None` rather than be invented.
        Regex::new(r"(?:^|[\s._\-])ova\s*(\d{1,3})(?:v\d)?(?:[-~](\d{1,3}))?(?:\s|\.|\[|\(|$)")
            .expect("RE_OVA_EP compiles")
    });

    static RE_SPECIAL_EP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?:^|[\s._\-])(?:sp|specials?|oad)\s*(\d{1,3})(?:v\d)?(?:[-~](\d{1,3}))?(?:\s|\.|\[|\(|$)",
        )
        .expect("RE_SPECIAL_EP compiles")
    });

    static RE_NUM_THEN_SPECIAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?:^|[\s._\-])\d{1,3}(?:v\d)?[\s._\-]+(?:ova|oad|sp|specials?)(?:\s*[\[\(]|\s*-\s|\.[a-z0-9]{2,4}$|$)",
        )
        .expect("RE_NUM_THEN_SPECIAL compiles")
    });

    static RE_BARE_SPECIAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?:\s-\s(?:ova|oad|sp|specials?)|[\[\(](?:ova|oad|specials?))(?:\s*[\]\)\[\(]|\s*-\s|\.[a-z0-9]{2,4}$|$)",
        )
        .expect("RE_BARE_SPECIAL compiles")
    });

    static RE_HALF_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
        // Half-episode / recap marker glued to the episode token in the
        // dash position (`- 07_5`, `- 07.5`, Erai-raws' recap convention)
        // or to an SxxExx tag (`S01E07.5`). Issue #203: these are
        // non-episodic under the Season-1 model and must not land in the
        // E07 slot. Matched on the raw (pre-underscore-normalization)
        // string because the `_5` form is exactly what normalization
        // would turn into a plain ` - 07 5 ` that RE_DASH_EP accepts.
        // Anchored on the dash / SxxExx position so a title like
        // `2.5 Jigen no Ririsa - 03` is untouched.
        Regex::new(r"(?:[\s_]-[\s_]|s\d{1,2}e)(\d{1,4})[._]5(?:[\s._\[\(]|$)")
            .expect("RE_HALF_EPISODE compiles")
    });

    const NON_EPISODIC_ANIME_TYPES: &[&str] = &[
        "pv", "cm", "ncop", "nced", "op", "ed", "opening", "ending", "preview", "menu", "trailer",
    ];

    fn starts_with_non_episode_marker(rest: &str) -> bool {
        let trimmed = rest.trim_start();
        const MARKERS: &[&str] = &["ova", "oad", "sp", "special", "specials"];
        for tok in MARKERS {
            if let Some(after) = trimmed.strip_prefix(tok) {
                // Word boundary: next char must not be another letter or
                // digit (so `special` matches but `spectral` doesn't).
                let bounded = match after.as_bytes().first() {
                    None => true,
                    Some(c) => !c.is_ascii_alphanumeric(),
                };
                if bounded {
                    return true;
                }
            }
        }
        false
    }

    fn parse_dot_delimited_episode(lower: &str) -> Option<i32> {
        let tokens: Vec<&str> = lower.split('.').collect();
        tokens.iter().enumerate().rev().find_map(|(i, token)| {
            let digits = token
                .split_once('v')
                .filter(|(_, version)| {
                    !version.is_empty() && version.chars().all(|c| c.is_ascii_digit())
                })
                .map(|(digits, _)| digits)
                .unwrap_or(token);
            if !((2..=3).contains(&digits.len()) && digits.chars().all(|c| c.is_ascii_digit())) {
                return None;
            }
            let follows_codec_marker = i > 0
                && tokens[i - 1]
                    .rsplit([' ', '-', '_', '(', '['])
                    .next()
                    .is_some_and(|word| matches!(word, "h" | "x" | "mpeg"));
            if follows_codec_marker {
                return None;
            }
            digits.parse().ok()
        })
    }

    fn accept_span_tail(first: &str, last: &str) -> Option<(i32, i32)> {
        let a: i32 = first.parse().ok()?;
        let b: i32 = last.parse().ok()?;
        if b <= a {
            return None;
        }
        if first.len() < 2 || last.len() < 2 {
            return None;
        }
        if first.len() == 4 && (1900..=2100).contains(&a) {
            return None;
        }
        if b - a + 1 > MAX_FILE_SPAN {
            return None;
        }
        Some((a, b))
    }

    enum Tail {
        Single(i32),
        Range(i32, i32),
        Rejected,
    }

    fn read_tail(
        caps: &regex_lite::Captures<'_>,
        first_idx: usize,
        last_idx: usize,
    ) -> Option<Tail> {
        let first = caps.get(first_idx)?.as_str();
        let Some(last) = caps.get(last_idx) else {
            return Some(Tail::Single(first.parse().ok()?));
        };
        Some(match accept_span_tail(first, last.as_str()) {
            Some((a, b)) => Tail::Range(a, b),
            None => Tail::Rejected,
        })
    }

    fn scan_tails(
        re: &Regex,
        lower: &str,
        first_idx: usize,
        last_idx: usize,
    ) -> Option<EpisodeSpan> {
        let mut from = 0;
        while from < lower.len() {
            let caps = re.captures(&lower[from..])?;
            match read_tail(&caps, first_idx, last_idx)? {
                Tail::Single(e) => return Some(EpisodeSpan::single(None, e)),
                Tail::Range(a, b) => {
                    return Some(EpisodeSpan {
                        season: None,
                        first: a,
                        last: b,
                        special: false,
                    });
                }
                // The first number is ASCII digits, so one byte in is a
                // char boundary.
                Tail::Rejected => from += caps.get(first_idx)?.start() + 1,
            }
        }
        None
    }

    pub(crate) fn parse_episode_span(lower: &str) -> Option<EpisodeSpan> {
        let mut parsed = parse_episode_span_inner(lower)?;
        // The anitomy gate runs last and only when a number was found, so
        // the (comparatively) expensive tokenizer pass is skipped for the
        // NC / bare-marker shapes the regex branches already reject.
        if is_non_episodic_extra(lower) {
            return None;
        }
        if !parsed.special && (parsed.season == Some(0) || has_special_marker(lower)) {
            parsed.special = true;
        }
        Some(parsed)
    }

    fn has_special_marker(lower: &str) -> bool {
        let normalized = if lower.contains('_') {
            Some(lower.replace('_', " "))
        } else {
            None
        };
        let lower = normalized.as_deref().unwrap_or(lower);
        RE_NUM_THEN_SPECIAL.is_match(lower) || RE_BARE_SPECIAL.is_match(lower)
    }

    fn parse_episode_span_inner(lower: &str) -> Option<EpisodeSpan> {
        // Non-episodic content guard. Creditless openings/endings carry a
        // small integer suffix (`NCOP 1`, `NCED 1`, or even glued
        // `NCED01a` from LOGH-style packs) that the bare-number
        // fallback below would otherwise mis-parse as episode 1 and
        // clobber the real ep1. Bail before any pattern fires. Matched
        // with a token-boundary regex so incidental substring hits like
        // `synced`, `convinced`, `announced` don't false-positive.
        if RE_NCOP_NCED.is_match(lower) {
            return None;
        }

        // Half-episode / recap guard (issue #203). Checked on the raw
        // string, before underscore normalization would turn `07_5` into
        // a plain `07 5` that RE_DASH_EP accepts as episode 7.
        if RE_HALF_EPISODE.is_match(lower) {
            return None;
        }

        // Underscore-delimited releases (HGS-Renc, some older groups)
        // use `_` where most releases use ` `. Normalize once so a
        // single set of patterns handles both shapes without having to
        // dual-parse. Only allocates when an underscore is actually
        // present — most inputs skip the replace.
        let normalized = if lower.contains('_') {
            Some(lower.replace('_', " "))
        } else {
            None
        };
        let lower = normalized.as_deref().unwrap_or(lower);

        // SxxExx pattern — most reliable. No follow-set, so a rejected tail
        // keeps the first number the way the pre-#246 regex did.
        if let Some(caps) = RE_SXEX.captures(lower) {
            let s: i32 = caps.get(1)?.as_str().parse().ok()?;
            return Some(match read_tail(&caps, 2, 3)? {
                Tail::Range(a, b) => EpisodeSpan {
                    season: Some(s),
                    first: a,
                    last: b,
                    special: false,
                },
                Tail::Single(e) => EpisodeSpan::single(Some(s), e),
                Tail::Rejected => EpisodeSpan::single(Some(s), caps.get(2)?.as_str().parse().ok()?),
            });
        }

        // " - 05" pattern (common in anime releases from SubsPlease, Erai, etc).
        if let Some(span) = scan_tails(&RE_DASH_EP, lower, 1, 2) {
            return Some(span);
        }

        // "E05" or "EP05" or "Ep.05" pattern.
        if let Some(span) = scan_tails(&RE_E_PREFIX, lower, 1, 2) {
            return Some(span);
        }

        // "Episode 05" pattern. No follow-set; a rejected tail keeps the
        // first number.
        if let Some(caps) = RE_EPISODE.captures(lower) {
            return Some(match read_tail(&caps, 1, 2)? {
                Tail::Range(a, b) => EpisodeSpan {
                    season: None,
                    first: a,
                    last: b,
                    special: false,
                },
                Tail::Single(e) => EpisodeSpan::single(None, e),
                Tail::Rejected => EpisodeSpan::single(None, caps.get(1)?.as_str().parse().ok()?),
            });
        }

        // Explicit `OVA NN` marker — for multi-episode OVA releases
        // whose filenames have no SxxExx and no leading `- NN -`. Must
        // fire before the bare-number branches so that `Title - OVA 01.`
        // resolves to 1 even when an earlier title token (e.g. a year)
        // would otherwise shadow it. Bare `- OVA.` with no trailing
        // digit correctly falls through here because RE_OVA_EP requires
        // a captured 1-3 digit group.
        if let Some(span) = scan_tails(&RE_OVA_EP, lower, 1, 2) {
            return Some(EpisodeSpan {
                special: true,
                ..span
            });
        }
        // `SP01` / `Special 01` / `OAD 01`, the same shape with the other
        // marker words.
        if let Some(span) = scan_tails(&RE_SPECIAL_EP, lower, 1, 2) {
            return Some(EpisodeSpan {
                special: true,
                ..span
            });
        }

        // Dot-tokenized bare episode number. This covers older complete-series
        // packs whose files are shaped `Title.001...` rather than `Title - 001`.
        if let Some(e) = parse_dot_delimited_episode(lower) {
            return Some(EpisodeSpan::single(None, e));
        }

        // Bare-number-before-subtitle fallback. Shape:
        // `<title> NN - <subtitle>.mkv` — first match wins (subtitle may
        // contain its own ` <n> - ` run that should NOT shadow the real
        // episode marker).
        if let Some(caps) = RE_BARE_NUM_DASH.captures(lower)
            && let Some(m) = caps.get(1)
        {
            if let Some(full) = caps.get(0) {
                // If what follows the ` - ` is a non-episodic marker
                // (e.g. `Chihayafuru 2 - OVA - Waga Miyo…`), the
                // captured `2` is a season/title number, not an
                // episode. Bail with `None` rather than fall through
                // to further patterns — the subsequent bare-number
                // branch would re-capture the same false digit.
                let rest = &lower[full.end()..];
                if starts_with_non_episode_marker(rest) {
                    return None;
                }
            }
            if let Ok(e) = m.as_str().parse::<i32>() {
                return Some(EpisodeSpan::single(None, e));
            }
        }

        // Bare-number-before-bracket fallback for space-delimited packs.
        // Use the rightmost match so an earlier title token like
        // `Show 99 (extra) 01` doesn't shadow the actual episode number.
        if let Some(caps) = RE_BARE_NUM_BRACKET.captures_iter(lower).last()
            && let Some(m) = caps.get(1)
            && let Ok(e) = m.as_str().parse::<i32>()
        {
            return Some(EpisodeSpan::single(None, e));
        }

        None
    }

    pub(crate) fn is_non_episodic_extra(lower: &str) -> bool {
        // anitomy rejects NUL bytes; nothing real carries them.
        if lower.contains('\0') {
            return false;
        }
        let elements = anitomy_parse(lower);
        let title_start = elements
            .get(ElementCategory::AnimeTitle)
            .and_then(|t| lower.find(&t.to_ascii_lowercase()))
            .unwrap_or(0);
        // Earliest byte offset a keyword may start at to count: right after
        // the first ` - ` / `_-_` separator, else past the title's first
        // character (so a title-leading keyword never qualifies).
        let min_at = lower
            .find(" - ")
            .or_else(|| lower.find("_-_"))
            .map(|i| i + 3)
            .unwrap_or(title_start + 1);
        elements
            .get_all(ElementCategory::AnimeType)
            .into_iter()
            .map(|t| t.to_ascii_lowercase())
            .any(|t| {
                NON_EPISODIC_ANIME_TYPES.contains(&t.as_str()) && keyword_from(lower, &t, min_at)
            })
    }

    fn keyword_from(haystack: &str, keyword: &str, min_at: usize) -> bool {
        let bytes = haystack.as_bytes();
        haystack.match_indices(keyword).any(|(at, _)| {
            let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
            let end = at + keyword.len();
            let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
            before_ok && after_ok && at >= min_at
        })
    }
}
