//! Torznab/newznab response XML parser (regex-based, like
//! [`crate::services::rss::feed`]).
//!
//! Parses three response shapes:
//! - Search response (RSS 2.0 + `<torznab:attr>` extensions).
//! - Caps response (`<caps>` with `<categories>`/`<searching>`/`<limits>`).
//! - Error response (`<error code="N" description="..."/>`).
//!
//! Each `parse_*` returns a typed result; the caller decides what
//! to do with `Err`. The parser does not hit the network — it's
//! pure on `&str` input and entirely unit-testable.

use regex_lite::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

use super::super::{CategoryCap, IndexerCaps, Release, SearchModeCap};

/// Decoded `<error code="N" description="..."/>` body. Returned
/// even on HTTP 200 per protocol, so the caller compares against
/// well-known codes (100/101 = bad creds, 200 = missing param,
/// 900 = unknown failure, etc.) before treating the response as
/// success.
#[derive(Debug, Clone, PartialEq)]
pub struct TorznabError {
    pub code: i32,
    pub description: String,
}

/// Detect a torznab `<error/>` body. Returns `None` when the body
/// looks like a normal response. Keep this cheap — it runs on
/// every response before the search/caps parsers do their work.
pub fn parse_error(xml: &str) -> Option<TorznabError> {
    static RE_ERROR: LazyLock<Regex> = LazyLock::new(|| {
        // Self-closing `<error code="..." description="..."/>`.
        // Some impls also emit it with attributes in reversed
        // order or as paired tags; the `(?is)` flag lets the dot
        // span newlines and the lazy quantifiers handle either
        // shape. Captures: 1=code, 2=description.
        Regex::new(r#"(?is)<error\b[^>]*\bcode\s*=\s*"(\d+)"[^>]*\bdescription\s*=\s*"([^"]*)""#)
            .expect("torznab error pattern compiles")
    });
    let caps = RE_ERROR.captures(xml)?;
    let code = caps.get(1)?.as_str().parse::<i32>().ok()?;
    let description = decode_xml(caps.get(2)?.as_str());
    Some(TorznabError { code, description })
}

/// Parse a torznab search response into a list of [`Release`]
/// records. The `indexer_id` and `indexer_priority` fields are
/// stamped from the caller's snapshot so dedup attribution is
/// deterministic across calls.
///
/// Returns `Ok(Err(TorznabError))` when the body is an error
/// response (HTTP 200 + `<error/>`), `Ok(Ok(releases))` on
/// success, and the outer `Err` only on unrecoverable parse
/// failures (truly malformed XML — empty result list is *not* a
/// failure).
pub fn parse_search_response(
    xml: &str,
    indexer_id: i64,
    indexer_priority: i32,
    indexer_name: &str,
) -> Result<Result<Vec<Release>, TorznabError>, String> {
    if let Some(err) = parse_error(xml) {
        return Ok(Err(err));
    }

    let mut releases = Vec::new();
    for block in item_blocks(xml).take(MAX_ITEMS) {
        let release = parse_item_block(block, indexer_id, indexer_priority, indexer_name);
        // Skip items with no usable identity. A torznab response
        // shouldn't emit empty items, but be defensive — a single
        // mangled item shouldn't shut out the rest of the page. A
        // title past the release-title cap is no real release either.
        if release.title.is_empty()
            || release.title.len() > crate::services::media::MAX_RELEASE_TITLE_BYTES
        {
            continue;
        }
        releases.push(release);
    }
    Ok(Ok(releases))
}

/// Most items read from one search response. Prowlarr pages at 100
/// and Jackett's aggregate rarely passes a few hundred; past this the
/// rest of the page is ignored rather than scored.
pub(crate) const MAX_ITEMS: usize = 1000;

fn parse_item_block(
    block: &str,
    indexer_id: i64,
    indexer_priority: i32,
    indexer_name: &str,
) -> Release {
    let title = decode_xml(&extract_tag(block, "title"));
    let guid = decode_xml(&extract_tag(block, "guid"));
    let link = decode_xml(&extract_tag(block, "link"));
    let pub_date_raw = decode_xml(&extract_tag(block, "pubDate"));
    let publish_date = parse_rfc2822_to_unix(&pub_date_raw);

    // Enclosure attrs supply size + the canonical download URL.
    // Spec says the enclosure URL is authoritative for downloads;
    // `<link>` is sometimes a comments-page URL.
    let enclosure = parse_enclosure(block);
    let size_from_enclosure = enclosure.length;

    // Build the torznab:attr map. Keyed by lowercase name so the
    // caller doesn't have to remember the exact casing each indexer
    // uses (Prowlarr/Jackett are consistent, but private trackers
    // sometimes diverge).
    let all_attrs = torznab_attr_pairs(block);
    let attrs = first_values(&all_attrs);
    let attr = |key: &str| attrs.get(&key.to_ascii_lowercase()).cloned();

    let size_bytes = attr("size")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(size_from_enclosure);
    let seeders = attr("seeders")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    let leechers = attr("leechers")
        .or_else(|| {
            attr("peers").and_then(|p| {
                // Some indexers omit `leechers` and only emit `peers`
                // (= seeders + leechers). Derive when possible.
                let peers = p.parse::<i32>().ok()?;
                Some((peers - seeders).max(0).to_string())
            })
        })
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    // Only a real hash is kept; see `download_client::normalize_info_hash`
    // for what an indexer-supplied `all` used to do.
    let info_hash = attr("infohash")
        .and_then(|s| crate::services::download_client::normalize_info_hash(&s))
        .unwrap_or_default();
    let magnet = attr("magneturl").unwrap_or_default();

    // PR #107 review fix #2: use the multi-value extractor so a
    // release marked with BOTH `5070` and `5999` (the AnimeTosho-
    // via-Prowlarr mis-tag from Prowlarr#1253) surfaces both ids.
    // The single-value `parse_categories` only returns the first
    // observed value, which would silently break the title-parse
    // fallback the doc comment promises.
    let categories = all_attrs
        .iter()
        .filter(|(name, _)| name == "category")
        .filter_map(|(_, value)| value.parse::<i32>().ok())
        .collect();
    let download_volume_factor = attr("downloadvolumefactor").and_then(|s| s.parse::<f32>().ok());
    let upload_volume_factor = attr("uploadvolumefactor").and_then(|s| s.parse::<f32>().ok());

    // Stash unrecognized attrs in `extra` for the inspector to
    // surface. Skip the well-known ones we already promoted to
    // first-class fields so the map isn't redundant.
    let extra = attrs
        .iter()
        .filter(|(k, _)| !WELL_KNOWN_ATTRS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let download_url = if !enclosure.url.is_empty() {
        enclosure.url
    } else {
        link.clone()
    };

    Release {
        indexer_id,
        indexer_priority,
        indexer_name: indexer_name.to_string(),
        title,
        guid,
        link: download_url,
        magnet,
        publish_date,
        size_bytes,
        seeders,
        leechers,
        info_hash,
        categories,
        download_volume_factor,
        upload_volume_factor,
        extra,
    }
}

const WELL_KNOWN_ATTRS: &[&str] = &[
    "size",
    "seeders",
    "leechers",
    "peers",
    "infohash",
    "magneturl",
    "category",
    "downloadvolumefactor",
    "uploadvolumefactor",
];

#[derive(Default)]
struct EnclosureAttrs {
    url: String,
    length: u64,
}

fn parse_enclosure(block: &str) -> EnclosureAttrs {
    let Some(tag) = next_open_tag(block, &["enclosure"], 0) else {
        return EnclosureAttrs::default();
    };
    EnclosureAttrs {
        url: extract_xml_attr(tag.attrs, "url"),
        length: extract_xml_attr(tag.attrs, "length")
            .parse::<u64>()
            .unwrap_or(0),
    }
}

/// Every `<torznab:attr name="X" value="Y"/>` (or `newznab:attr`) in
/// an item, in document order, names lowercased and both sides
/// decoded.
fn torznab_attr_pairs(block: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(tag) = next_open_tag(block, &["torznab:attr", "newznab:attr"], from) {
        from = tag.end;
        let name = extract_xml_attr(tag.attrs, "name").to_ascii_lowercase();
        if !name.is_empty() {
            out.push((name, extract_xml_attr(tag.attrs, "value")));
        }
    }
    out
}

/// The first value of each attribute. A repeating attribute
/// (`category`) is read from the full list instead.
fn first_values(pairs: &[(String, String)]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, value) in pairs {
        out.entry(name.clone()).or_insert_with(|| value.clone());
    }
    out
}

/// Every value of a repeating attr in `block` (`category` is the one
/// that repeats). Names outside `[A-Za-z0-9_-]` match nothing.
pub fn parse_repeating_attr(block: &str, name: &str) -> Vec<String> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Vec::new();
    }
    torznab_attr_pairs(block)
        .into_iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
        .collect()
}

/// Extract every category id reported on an item, including
/// repeats.
pub fn extract_all_categories(block: &str) -> Vec<i32> {
    parse_repeating_attr(block, "category")
        .into_iter()
        .filter_map(|s| s.parse::<i32>().ok())
        .collect()
}

/// Parse a torznab caps response into [`IndexerCaps`]. Best-effort:
/// missing fields render as None / empty Vec rather than failing.
/// Indexers vary in how they format caps (Prowlarr is generous,
/// some private trackers are sparse).
pub fn parse_caps_response(xml: &str) -> Result<IndexerCaps, String> {
    if let Some(err) = parse_error(xml) {
        return Err(format!(
            "Indexer caps returned error code {}: {}",
            err.code, err.description
        ));
    }

    static RE_LIMITS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<limits\b([^>]*)>").expect("compiles"));
    let limits_block = RE_LIMITS
        .captures(xml)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
        .unwrap_or("");
    let max_limit = extract_xml_attr(limits_block, "max").parse::<u32>().ok();
    let default_limit = extract_xml_attr(limits_block, "default")
        .parse::<u32>()
        .ok();

    Ok(IndexerCaps {
        categories: parse_caps_categories(xml),
        search_modes: parse_caps_search_modes(xml),
        max_limit,
        default_limit,
    })
}

/// Each `<category>` with its `<subcat>`s. Open tags are found first;
/// a paired tag's body is the text up to its `</category>`, looked for
/// only before the next open tag, so the scan is linear. The single
/// `<category ...>(.*?)</category>` pattern this replaces kept an
/// unclosed tag's search alive to the end of the input on every match
/// after it: a 218 KB caps body took 15 seconds on a runtime worker.
fn parse_caps_categories(xml: &str) -> Vec<CategoryCap> {
    static RE_CATEGORY_OPEN: LazyLock<Regex> = LazyLock::new(|| {
        // Lazy `[^>]*?` keeps URLs-with-slashes safe. Captures:
        // 1=attrs, 2=`/` for a self-closing tag.
        Regex::new(r#"(?is)<category\b([^>]*?)(/?)\s*>"#).expect("category pattern compiles")
    });
    static RE_SUBCAT: LazyLock<Regex> = LazyLock::new(|| {
        // Lazy `[^>]*?` for the same reason as enclosure: attribute
        // values can contain `/`. Subcats are self-closing so the
        // captured trailing `/` is safe.
        Regex::new(r#"(?is)<subcat\b([^>]*?)>"#).expect("subcat pattern compiles")
    });
    struct Open<'a> {
        start: usize,
        end: usize,
        attrs: &'a str,
        self_closing: bool,
    }
    let opens: Vec<Open> = RE_CATEGORY_OPEN
        .captures_iter(xml)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            Some(Open {
                start: whole.start(),
                end: whole.end(),
                attrs: caps.get(1).map_or("", |m| m.as_str()),
                self_closing: caps.get(2).is_some_and(|m| !m.as_str().is_empty()),
            })
        })
        .collect();
    let mut out = Vec::new();
    for (i, open) in opens.iter().enumerate() {
        let Ok(id) = extract_xml_attr(open.attrs, "id").parse::<i32>() else {
            continue;
        };
        let body = if open.self_closing {
            ""
        } else {
            let window_end = opens.get(i + 1).map_or(xml.len(), |next| next.start);
            let window = &xml[open.end..window_end];
            next_close_tag(window, "category", 0).map_or(window, |at| &window[..at])
        };
        let name = decode_xml(&extract_xml_attr(open.attrs, "name"));
        let subcategories = RE_SUBCAT
            .captures_iter(body)
            .filter_map(|sc| {
                let sub_attrs = sc.get(1).map(|m| m.as_str()).unwrap_or("");
                let sub_id = extract_xml_attr(sub_attrs, "id").parse::<i32>().ok()?;
                let sub_name = decode_xml(&extract_xml_attr(sub_attrs, "name"));
                Some(CategoryCap {
                    id: sub_id,
                    name: sub_name,
                    subcategories: Vec::new(),
                })
            })
            .collect();
        out.push(CategoryCap {
            id,
            name,
            subcategories,
        });
    }
    out
}

fn parse_caps_search_modes(xml: &str) -> Vec<SearchModeCap> {
    static RE_MODE: LazyLock<Regex> = LazyLock::new(|| {
        // Match every search-mode element: `<search>`, `<tv-search>`,
        // `<movie-search>`, etc. Lazy `[^>]*?` for the same URL-with-
        // slashes reason as enclosure/subcat. Captures: 1=tag, 2=attrs.
        Regex::new(r#"(?is)<((?:search|tv-search|movie-search|music-search|book-search|audio-search))\b([^>]*?)>"#)
            .expect("search mode pattern compiles")
    });
    let mut out = Vec::new();
    for caps in RE_MODE.captures_iter(xml) {
        let tag = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let attrs = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let available = extract_xml_attr(attrs, "available").eq_ignore_ascii_case("yes");
        let supported_params = extract_xml_attr(attrs, "supportedParams");
        let supported_params: Vec<String> = if supported_params.is_empty() {
            Vec::new()
        } else {
            supported_params
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        out.push(SearchModeCap {
            mode: tag.to_string(),
            available,
            supported_params,
        });
    }
    out
}

// ── Shared XML helpers (parallel to services::rss::feed) ─────────

// ── Tag scanner ──────────────────────────────────────────────────
//
// The item path finds tags by hand: jump from `<` to `<` (a memchr
// scan) and compare the name without ASCII case. regex-lite has no
// literal prefilter, so `<item\b[^>]*>(.*?)</item>` plus a compiled
// pattern per tag, enclosure and attribute ran the full matcher over
// every byte several times: a 1,000-item page took ~190 ms in the
// test profile, on a runtime worker.

/// An open tag: the attribute text between the name and `>`, and the
/// offset just past `>`.
struct OpenTag<'a> {
    attrs: &'a str,
    end: usize,
}

/// The next `<name ...>` at or after `from` for any of `names`
/// (ASCII, compared without case; the name must be followed by `>`,
/// `/` or whitespace, so `<item>` never matches `<items>`).
fn next_open_tag<'a>(hay: &'a str, names: &[&str], from: usize) -> Option<OpenTag<'a>> {
    let bytes = hay.as_bytes();
    let mut at = from;
    while let Some(offset) = hay.get(at..)?.find('<') {
        let start = at + offset;
        for name in names {
            let name_end = start + 1 + name.len();
            if bytes
                .get(start + 1..name_end)
                .is_some_and(|n| n.eq_ignore_ascii_case(name.as_bytes()))
                && bytes
                    .get(name_end)
                    .is_some_and(|&b| b == b'>' || b == b'/' || b.is_ascii_whitespace())
            {
                let close = name_end + hay[name_end..].find('>')?;
                return Some(OpenTag {
                    attrs: &hay[name_end..close],
                    end: close + 1,
                });
            }
        }
        at = start + 1;
    }
    None
}

/// Offset of the next `</name` at or after `from` (followed by `>` or
/// whitespace), without ASCII case.
fn next_close_tag(hay: &str, name: &str, from: usize) -> Option<usize> {
    let bytes = hay.as_bytes();
    let mut at = from;
    while let Some(offset) = hay.get(at..)?.find("</") {
        let start = at + offset;
        let name_end = start + 2 + name.len();
        if bytes
            .get(start + 2..name_end)
            .is_some_and(|n| n.eq_ignore_ascii_case(name.as_bytes()))
            && bytes
                .get(name_end)
                .is_some_and(|&b| b == b'>' || b.is_ascii_whitespace())
        {
            return Some(start);
        }
        at = start + 2;
    }
    None
}

/// The body of every `<item>...</item>`, in order. An item with no
/// closing tag ends the list.
fn item_blocks(xml: &str) -> impl Iterator<Item = &str> {
    let mut from = 0;
    std::iter::from_fn(move || {
        let open = next_open_tag(xml, &["item"], from)?;
        let close = next_close_tag(xml, "item", open.end)?;
        from = close;
        Some(&xml[open.end..close])
    })
}

/// The text of the first `<tag>...</tag>` in `block`, CDATA stripped.
/// A self-closing or unclosed tag reads as empty.
fn extract_tag(block: &str, tag: &str) -> String {
    let Some(open) = next_open_tag(block, &[tag], 0) else {
        return String::new();
    };
    if open.attrs.ends_with('/') {
        return String::new();
    }
    next_close_tag(block, tag, open.end)
        .map(|close| strip_cdata(&block[open.end..close]))
        .unwrap_or_default()
}

/// Pull a double-quoted attribute value from a tag's attribute list
/// (`attrs` is the text between the tag name and `>`). Names compare
/// without case, and each `name="value"` pair is read in turn, so a
/// name can't match inside another (`foo-name="..."`) or inside a
/// value.
fn extract_xml_attr(attrs: &str, name: &str) -> String {
    let mut rest = attrs;
    loop {
        let Some(eq) = rest.find('=') else {
            return String::new();
        };
        let key = rest[..eq]
            .trim_end()
            .rsplit(|c: char| c.is_ascii_whitespace())
            .next()
            .unwrap_or("");
        let after = rest[eq + 1..].trim_start();
        let Some(quoted) = after.strip_prefix('"') else {
            rest = after;
            continue;
        };
        let Some(close) = quoted.find('"') else {
            return String::new();
        };
        if key.eq_ignore_ascii_case(name) {
            return decode_xml(&quoted[..close]);
        }
        rest = &quoted[close + 1..];
    }
}

fn strip_cdata(value: &str) -> String {
    value
        .trim()
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(value)
        .trim()
        .to_string()
}

/// Decode the five XML predefined entities + numeric character
/// references. Mirrors [`crate::services::rss::feed::decode_xml`]
/// — kept as a sibling rather than re-exported because the RSS
/// version is `pub(super)` to its module.
fn decode_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        // Look ahead for `;`. If we don't see one within ~10 chars,
        // emit the `&` literal and continue.
        let mut entity = String::new();
        let mut found = false;
        for _ in 0..10 {
            match chars.next() {
                Some(';') => {
                    found = true;
                    break;
                }
                Some(ch) => entity.push(ch),
                None => break,
            }
        }
        if !found {
            out.push('&');
            out.push_str(&entity);
            continue;
        }
        match entity.as_str() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            num if num.starts_with('#') => {
                let body = &num[1..];
                let parsed = if let Some(hex) = body.strip_prefix(['x', 'X']) {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    body.parse::<u32>().ok()
                };
                if let Some(code) = parsed
                    && let Some(ch) = char::from_u32(code)
                {
                    out.push(ch);
                } else {
                    // Unparseable numeric entity — emit the literal
                    // so the caller still has SOMETHING to inspect.
                    out.push('&');
                    out.push_str(&entity);
                    out.push(';');
                }
            }
            _ => {
                // Unknown named entity — emit the literal.
                out.push('&');
                out.push_str(&entity);
                out.push(';');
            }
        }
    }
    out
}

/// Parse an RFC 2822 datetime ("Fri, 24 Apr 2026 18:32:01 +0000")
/// to a Unix timestamp. Defensive — any parse failure returns 0
/// (publish_date Default) rather than poisoning the whole item.
fn parse_rfc2822_to_unix(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    // Manual parse to avoid pulling in `chrono` for one helper.
    // RFC 2822 shape: "Day, DD Mon YYYY HH:MM:SS ±ZZZZ".
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 {
        return 0;
    }
    // After splitting: parts[0]="Fri,", [1]="24", [2]="Apr",
    // [3]="2026", [4]="18:32:01", [5?]="+0000".
    let day: u32 = match parts[1].parse() {
        Ok(n) => n,
        Err(_) => return 0,
    };
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return 0,
    };
    let year: i32 = match parts[3].parse() {
        Ok(n) => n,
        Err(_) => return 0,
    };
    let time_parts: Vec<&str> = parts[4].split(':').collect();
    if time_parts.len() != 3 {
        return 0;
    }
    let hour: u32 = time_parts[0].parse().unwrap_or(0);
    let minute: u32 = time_parts[1].parse().unwrap_or(0);
    let second: u32 = time_parts[2].parse().unwrap_or(0);

    // Convert to Unix timestamp via days-since-epoch math. Civil-
    // calendar algorithm from Howard Hinnant's `date` library —
    // valid for any proleptic Gregorian date.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era as i64 * 146097 + doe as i64 - 719468;
    let unix = days_since_epoch * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;

    // Apply the timezone offset if present. RFC 2822 emits ±HHMM
    // or 'GMT'/'UTC' — handle the numeric form; named zones are
    // assumed UTC (real impls send ±0000 for UTC anyway).
    if parts.len() >= 6 {
        let tz = parts[5];
        if let Some(sign) = tz.chars().next()
            && (sign == '+' || sign == '-')
            && tz.len() >= 5
        {
            // `get`, not `[1..3]`: a multi-byte character in an indexer's
            // timezone (`+0é00`) put a byte offset inside it and panicked.
            let hh: i64 = tz.get(1..3).and_then(|v| v.parse().ok()).unwrap_or(0);
            let mm: i64 = tz.get(3..5).and_then(|v| v.parse().ok()).unwrap_or(0);
            let offset = (hh * 3600 + mm * 60) * if sign == '+' { -1 } else { 1 };
            return unix + offset;
        }
    }
    unix
}

#[cfg(test)]
mod info_hash_tests {
    use super::*;

    fn item_with_hash(hash: &str) -> String {
        format!(
            r#"<item><title>Show - 01</title><link>https://idx.example/dl/1</link>
            <torznab:attr name="infohash" value="{hash}"/></item>"#
        )
    }

    #[test]
    fn an_infohash_that_is_not_a_hash_is_dropped() {
        for bad in ["all", "ABC", "aabbccddeeff00112233445566778899aabbccdd|x"] {
            let release = parse_item_block(&item_with_hash(bad), 1, 0, "idx");
            assert_eq!(release.info_hash, "", "{bad}");
        }
        let release = parse_item_block(
            &item_with_hash("AABBCCDDEEFF00112233445566778899AABBCCDD"),
            1,
            0,
            "idx",
        );
        assert_eq!(
            release.info_hash,
            "aabbccddeeff00112233445566778899aabbccdd"
        );
    }

    #[test]
    fn a_non_ascii_timezone_does_not_panic() {
        // Byte offsets inside `é` used to panic the RSS tick on every
        // restart while the item stayed in the feed.
        let base = parse_rfc2822_to_unix("Fri, 24 Apr 2026 18:32:01 +0000");
        assert_eq!(
            parse_rfc2822_to_unix("Fri, 24 Apr 2026 18:32:01 +0é00"),
            base
        );
    }
}
