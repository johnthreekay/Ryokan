//! Nyaa RSS feed fetch + XML parse.
//!
//! Owns the HTTP client pool, the nyaa.si base URL, and the regex-driven
//! `<item>`/tag/entity decoding pipeline. The `RssItem` type it produces is
//! defined in the parent `services::rss::mod` — kept there because it's the
//! canonical data model the rest of the sync pipeline consumes.
//!
//! Public (to `super`) surface:
//! - `fetch_feeds` / `fetch_feed` — network I/O
//! - `build_item_key` — the dedup key sync uses to detect already-seen items
//! - `extract_group` / `extract_resolution` / `detect_batch` — also called by
//!   `parse_release` in the parent to re-derive these fields from an arbitrary
//!   title string rather than a feed item.

use std::{collections::HashMap, sync::LazyLock, time::Duration};

use regex_lite::Regex;

use super::RssItem;

/// Process-global `reqwest::Client` for RSS fetches. See the same pattern
/// in `source_description.rs`/`nyaa.rs`: a fresh client per call throws
/// away connection keepalive and re-handshakes TLS every tick. 30-second
/// per-request timeout caps the damage from a hung connection so the
/// 5-minute outer `sync_once` timeout isn't the only backstop — a single
/// slow DNS lookup used to be able to eat the whole sync budget.
static RSS_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Ryokan/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the RSS reqwest client should not fail")
});

const NYAA_RSS_BASE: &str = "https://nyaa.si/?page=rss&f=0";

static RE_ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<item>(.*?)</item>").unwrap());
static RE_BATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:e?\d{1,4}|s\d{1,2}e\d{1,4})\s*[-~]\s*(?:e?\d{1,4}|\d{1,4})\b").unwrap()
});

/// Season-marker-immediately-followed-by-bracket pattern. Catches the
/// Kaizoku-style convention where a pack is named `[Group] Series
/// Season N (Descriptor)` with no episode number between the season
/// token and the metadata parens — the opening bracket is the anchor
/// for a metadata block, not an episode number. Single-episode
/// releases have a different anchor (episode token or dash) between
/// the season marker and the bracket, so this regex doesn't fire.
///
/// The season-token vocabulary comes from `super::SEASON_TOKEN_
/// FRAGMENTS` so a new phrasing (e.g. "Chapter N") only needs to be
/// added in one place and both this regex and the masking pass in
/// `parse_release` pick it up. The `\s*[(\[]` tail ensures no `E\d`
/// or other digit sits between the season marker and the bracket,
/// which regex-lite's lack of lookaround would otherwise need to
/// express.
static RE_BATCH_SEASON_BRACKET: LazyLock<Regex> = LazyLock::new(|| {
    let alternation = super::SEASON_TOKEN_FRAGMENTS.join("|");
    Regex::new(&format!(r"(?i)\b(?:{})\s*[(\[]", alternation)).unwrap()
});

pub(super) fn build_item_key(item: &RssItem) -> String {
    if !item.info_hash.is_empty() {
        return format!("hash:{}", item.info_hash.to_lowercase());
    }
    if !item.guid.is_empty() {
        return format!("guid:{}", item.guid);
    }
    if !item.link.is_empty() {
        return format!("link:{}", item.link);
    }
    format!("title:{}", item.title.to_lowercase())
}

async fn fetch_feed(category: &str) -> Result<Vec<RssItem>, String> {
    let url = format!("{}&c={}", NYAA_RSS_BASE, category);
    let xml = RSS_HTTP_CLIENT
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("RSS request failed: {}", e))?
        .text()
        .await
        .map_err(|e| format!("Failed to read RSS response: {}", e))?;

    Ok(parse_feed(&xml))
}

/// Fetch RSS items from all relevant Nyaa categories.
/// Uses English-translated (1_2) by default; adds music categories (1_1, 2_0)
/// if any tracked series has MUSIC format; uses All (1_0) when allow_non_english.
pub(super) async fn fetch_feeds(
    allow_non_english: bool,
    has_music_series: bool,
) -> Result<Vec<RssItem>, String> {
    let mut categories = if allow_non_english {
        vec!["1_0"]
    } else {
        vec!["1_2"]
    };
    if has_music_series {
        if !categories.contains(&"1_1") {
            categories.push("1_1");
        }
        if !categories.contains(&"2_0") {
            categories.push("2_0");
        }
    }

    let mut all_items = Vec::new();
    let mut seen_keys = std::collections::HashSet::new();
    for cat in categories {
        let items = fetch_feed(cat).await?;
        for item in items {
            let key = if !item.info_hash.is_empty() {
                item.info_hash.to_lowercase()
            } else {
                item.title.to_lowercase()
            };
            if seen_keys.insert(key) {
                all_items.push(item);
            }
        }
    }
    Ok(all_items)
}

fn parse_feed(xml: &str) -> Vec<RssItem> {
    let mut items = Vec::new();

    for caps in RE_ITEM.captures_iter(xml) {
        let block = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = decode_xml(&extract_tag(block, "title")).trim().to_string();
        if title.is_empty() {
            continue;
        }

        let link = decode_xml(&extract_tag(block, "link")).trim().to_string();
        let guid = decode_xml(&extract_tag(block, "guid")).trim().to_string();
        let torrent = decode_xml(&extract_tag(block, "nyaa:downloadurl"))
            .trim()
            .to_string();
        let magnet = decode_xml(&extract_tag(block, "nyaa:magneturi"))
            .trim()
            .to_string();
        let info_hash = decode_xml(&extract_tag(block, "nyaa:infohash"))
            .trim()
            .to_lowercase();
        let group = extract_group(&title);
        let resolution = extract_resolution(&title);
        let is_batch = detect_batch(&title);

        items.push(RssItem {
            title,
            link,
            guid,
            torrent,
            magnet,
            info_hash,
            group,
            resolution,
            is_batch,
        });
    }

    items
}

/// Pre-compiled regexes keyed by RSS tag name. Populated lazily on first
/// feed parse so we don't re-compile six regexes per item across thousands
/// of items per sync. Only the six tags `parse_feed` actually reads are
/// included; `extract_tag` returns an empty string for any other tag,
/// matching the old behavior of `Regex::new(...).unwrap()` silently
/// returning no captures for an unmatched pattern.
static RE_EXTRACT_TAGS: LazyLock<HashMap<&'static str, Regex>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for tag in [
        "title",
        "link",
        "guid",
        "nyaa:downloadurl",
        "nyaa:magneturi",
        "nyaa:infohash",
    ] {
        let pattern = format!(r"(?is)<{tag}[^>]*>(.*?)</{tag}>", tag = tag);
        m.insert(
            tag,
            Regex::new(&pattern).expect("extract_tag pattern compiles"),
        );
    }
    m
});

fn extract_tag(block: &str, tag: &str) -> String {
    let Some(re) = RE_EXTRACT_TAGS.get(tag) else {
        return String::new();
    };
    re.captures(block)
        .and_then(|caps| caps.get(1))
        .map(|m| strip_cdata(m.as_str()))
        .unwrap_or_default()
}

fn strip_cdata(value: &str) -> String {
    value
        .trim()
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(value)
        .to_string()
}

/// Decode XML character references in a single pass.
///
/// Handles the five predefined XML entities (`&amp;`, `&lt;`, `&gt;`,
/// `&quot;`, `&apos;`) plus decimal (`&#NNN;`) and hexadecimal
/// (`&#xHH;`) numeric character references. Unknown entities are left
/// untouched.
///
/// The previous implementation used chained `str::replace` calls, which
/// had two problems: (1) it missed `&apos;` and any numeric reference
/// other than the specific literal `&#39;`, so feeds emitting e.g.
/// `&#039;` or `&#x27;` for apostrophes came through mangled; and (2)
/// the `&amp;` → `&` pass ran first, which could cause double-decoding
/// on pathological input like `&amp;lt;`. Scanning once from left to
/// right avoids both issues.
fn decode_xml(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            let ch = value[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let end = bytes[i + 1..]
            .iter()
            .take(16)
            .position(|&b| b == b';')
            .map(|p| i + 1 + p);
        let Some(end) = end else {
            out.push('&');
            i += 1;
            continue;
        };
        let entity = &value[i + 1..end];
        let decoded: Option<char> = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                if let Some(num) = entity.strip_prefix('#') {
                    let code = if let Some(hex) =
                        num.strip_prefix('x').or_else(|| num.strip_prefix('X'))
                    {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num.parse::<u32>().ok()
                    };
                    code.and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                i = end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

pub(super) fn extract_group(title: &str) -> String {
    if let Some(start) = title.find('[')
        && let Some(end) = title[start..].find(']')
    {
        return title[start + 1..start + end].to_string();
    }
    String::new()
}

pub(super) fn extract_resolution(title: &str) -> String {
    let lower = title.to_lowercase();
    for res in ["2160", "1080", "720", "576", "480"] {
        if lower.contains(&format!("{}p", res)) || lower.contains(&format!(" {} ", res)) {
            return res.to_string();
        }
    }
    String::new()
}

pub(super) fn detect_batch(title: &str) -> bool {
    let lower = title.to_lowercase();
    // Mask season markers before running RE_BATCH's `\d+-\d+` range
    // regex — otherwise titles like "Season 3 - 05" match as a spurious
    // range "3 - 05". Same trick `parse_release` uses before RE_ABSOLUTE.
    // Keep the original `lower` for the other predicates: RE_BATCH_
    // SEASON_BRACKET needs the season markers intact (it's literally
    // matching them), and the " batch" / " complete" / etc. substrings
    // don't overlap with season markers anyway.
    let mut masked = lower.clone();
    for re in super::RE_SEASON_MARKER_MASK.iter() {
        masked = re.replace_all(&masked, " ").to_string();
    }
    RE_BATCH.is_match(&masked)
        || RE_BATCH_SEASON_BRACKET.is_match(&lower)
        || lower.contains(" batch")
        || lower.contains(" complete")
        || lower.contains(" mini batch")
        || lower.contains(" full season")
        || lower.contains("全集")
}

#[cfg(test)]
mod detect_batch_tests {
    use super::detect_batch;

    #[test]
    fn kaizoku_season_parens_detected_as_batch() {
        assert!(detect_batch(
            "[Kaizoku] Jujutsu Kaisen Season 3 (WEB 1080p HEVC EAC-3) | The Culling Game Part 1"
        ));
    }

    #[test]
    fn subsplease_weekly_not_batch() {
        assert!(!detect_batch(
            "[SubsPlease] Frieren - 01 (1080p) [ABCD1234].mkv"
        ));
    }

    #[test]
    fn single_episode_not_batch() {
        // Standard weekly release has no season-bracket anchor and no
        // range token. `RE_BATCH_SEASON_BRACKET` doesn't fire because
        // there's no season marker.
        assert!(!detect_batch("[Group] Cool Anime - 05 (1080p)"));
    }

    #[test]
    fn season_dash_episode_not_mistaken_for_range() {
        // "Season 3 - 05" is a single episode of season 3, not a
        // "3 - 05" batch range. RE_BATCH's \d-\d pattern would
        // otherwise catch the season digit + episode as a range; the
        // season-marker mask applied before RE_BATCH runs prevents it.
        assert!(!detect_batch("[Group] Cool Anime Season 3 - 05 (1080p)"));
    }

    #[test]
    fn part_dash_episode_not_mistaken_for_range() {
        // Same false-positive risk for "Part N - NN" — mask covers it.
        assert!(!detect_batch("[Group] Cool Anime Part 2 - 05 (1080p)"));
    }

    #[test]
    fn real_range_still_batch_after_mask() {
        // Sanity: an actual episode range "01-12" survives the mask
        // because it contains no season marker to strip.
        assert!(detect_batch("[Group] Cool Anime - 01-12 (1080p)"));
    }

    #[test]
    fn s3e05_not_batch() {
        // "S3E05" — the S\d immediately follows with E\d, so the
        // `\s*[(\[]` tail on RE_BATCH_SEASON_BRACKET can't match
        // (the next char after "s3" is "e", not whitespace/bracket).
        assert!(!detect_batch("[Group] Cool Anime S3E05 (1080p)"));
    }

    #[test]
    fn explicit_range_still_batch() {
        assert!(detect_batch("[Group] Cool Anime 01-12 (1080p)"));
    }

    #[test]
    fn explicit_batch_token_still_batch() {
        assert!(detect_batch("[Group] Cool Anime Complete Batch"));
    }

    #[test]
    fn nrd_season_parens_detected_as_batch() {
        assert!(detect_batch("[Group] Series 3rd Season (1080p BD)"));
    }

    #[test]
    fn cour_parens_detected_as_batch() {
        assert!(detect_batch("[Group] Series Cour 2 (1080p)"));
    }

    #[test]
    fn part_parens_detected_as_batch() {
        assert!(detect_batch("[Group] Series Part 2 (1080p)"));
    }
}
