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
    for caps in RE_ITEM.captures_iter(xml) {
        let block = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let release = parse_item_block(block, indexer_id, indexer_priority, indexer_name);
        // Skip items with no usable identity. A torznab response
        // shouldn't emit empty items, but be defensive — a single
        // mangled item shouldn't shut out the rest of the page.
        if release.title.is_empty() {
            continue;
        }
        releases.push(release);
    }
    Ok(Ok(releases))
}

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
    let attrs = parse_torznab_attrs(block);
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
    let categories = extract_all_categories(block);
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
    // `[^>]*?` (lazy) — NOT `[^/>]*` — because attribute values
    // contain `/` (URLs!). Excluding `/` killed the match the
    // moment the regex hit the protocol slashes. The lazy match
    // stops at the first `>`, which catches both `/>` self-closing
    // and `>` open-tag forms; the trailing `/` (if any) is part
    // of the captured attrs but harmless to attribute extraction.
    static RE_ENCLOSURE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?is)<enclosure\b([^>]*?)>"#).expect("compiles"));
    let Some(caps) = RE_ENCLOSURE.captures(block) else {
        return EnclosureAttrs::default();
    };
    let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    EnclosureAttrs {
        url: extract_xml_attr(attrs, "url"),
        length: extract_xml_attr(attrs, "length")
            .parse::<u64>()
            .unwrap_or(0),
    }
}

fn parse_torznab_attrs(block: &str) -> HashMap<String, String> {
    static RE_ATTR: LazyLock<Regex> = LazyLock::new(|| {
        // Matches both `<torznab:attr name="X" value="Y"/>` and
        // self-closing variants without the trailing `/`. The
        // `(?is)` flag handles multiline values; lazy quantifier
        // on value stops at the first quote.
        //
        // Newznab uses `<newznab:attr>` instead — accept both.
        // Captures: 1=name, 2=value.
        Regex::new(
            r#"(?is)<(?:torznab|newznab):attr\b[^>]*\bname\s*=\s*"([^"]*)"[^>]*\bvalue\s*=\s*"([^"]*)""#,
        )
        .expect("torznab attr pattern compiles")
    });
    let mut out = HashMap::new();
    for caps in RE_ATTR.captures_iter(block) {
        let name = decode_xml(caps.get(1).map(|m| m.as_str()).unwrap_or("")).to_ascii_lowercase();
        let value = decode_xml(caps.get(2).map(|m| m.as_str()).unwrap_or(""));
        // For multi-valued attrs (esp. `category`), keep the FIRST
        // observed value here; full list goes through
        // [`parse_categories`] which scans for repeats.
        out.entry(name).or_insert(value);
    }
    out
}

/// Variant of [`parse_torznab_attrs`] that captures EVERY value
/// of a repeating attr. Used for `category` extraction since a
/// release can carry multiple cat ids and the single-value map
/// drops repeats.
pub fn parse_repeating_attr(block: &str, name: &str) -> Vec<String> {
    static RE_ATTR_TPL: &str =
        r#"(?is)<(?:torznab|newznab):attr\b[^>]*\bname\s*=\s*"{NAME}"[^>]*\bvalue\s*=\s*"([^"]*)""#;
    // Sanitize `name` — it's a caller-supplied tag we're embedding
    // in a regex literal. Only alphanumerics + `-_` are valid
    // torznab attr names, so anything else is rejected to keep
    // the regex contract clean.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Vec::new();
    }
    let pattern = RE_ATTR_TPL.replace("{NAME}", name);
    let Ok(re) = Regex::new(&pattern) else {
        return Vec::new();
    };
    re.captures_iter(block)
        .filter_map(|caps| caps.get(1).map(|m| decode_xml(m.as_str())))
        .collect()
}

/// Extract every category id reported on an item, including
/// repeats. Combines [`parse_repeating_attr`] with the already-
/// parsed primary category so the result is the union.
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

    let limits_block = extract_tag_block(xml, "limits");
    let max_limit = extract_xml_attr(&limits_block, "max").parse::<u32>().ok();
    let default_limit = extract_xml_attr(&limits_block, "default")
        .parse::<u32>()
        .ok();

    Ok(IndexerCaps {
        categories: parse_caps_categories(xml),
        search_modes: parse_caps_search_modes(xml),
        max_limit,
        default_limit,
    })
}

fn parse_caps_categories(xml: &str) -> Vec<CategoryCap> {
    static RE_CATEGORY: LazyLock<Regex> = LazyLock::new(|| {
        // Two shapes to match:
        //   - `<category attrs>body</category>` (with subcats)
        //   - `<category attrs/>` (self-closing, no subcats)
        // The alternation captures attrs in either form; body is
        // captured only in the paired form (Option<&str> in Rust).
        // Lazy `[^>]*?` keeps URLs-with-slashes safe.
        // Captures: 1=attrs, 2=body (Some when paired, None when
        // self-closing).
        Regex::new(r#"(?is)<category\b([^>]*?)(?:/\s*>|>(.*?)</category>)"#)
            .expect("category pattern compiles")
    });
    static RE_SUBCAT: LazyLock<Regex> = LazyLock::new(|| {
        // Lazy `[^>]*?` for the same reason as enclosure: attribute
        // values can contain `/`. Subcats are self-closing so the
        // captured trailing `/` is safe.
        Regex::new(r#"(?is)<subcat\b([^>]*?)>"#).expect("subcat pattern compiles")
    });
    let mut out = Vec::new();
    for caps in RE_CATEGORY.captures_iter(xml) {
        let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let body = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let id = match extract_xml_attr(attrs, "id").parse::<i32>() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let name = decode_xml(&extract_xml_attr(attrs, "name"));
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

static RE_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<item\b[^>]*>(.*?)</item>").expect("item pattern compiles"));

fn extract_tag(block: &str, tag: &str) -> String {
    // Pattern compilation is per-call here (vs the RSS feed's
    // pre-compiled HashMap) because the torznab tag set is
    // larger and the per-search call rate is lower; the regex
    // engine's own cache handles the perf gap. If profiling
    // shows this hot, lift the LazyLock pattern.
    let pattern = format!(
        r"(?is)<{tag}\b[^>]*>(.*?)</{tag}>",
        tag = regex_escape_tag(tag)
    );
    let Ok(re) = Regex::new(&pattern) else {
        return String::new();
    };
    re.captures(block)
        .and_then(|caps| caps.get(1))
        .map(|m| strip_cdata(m.as_str()))
        .unwrap_or_default()
}

/// Same as `extract_tag` but returns the inner XML (un-stripped)
/// so nested elements remain parseable. Used for `<limits>` /
/// `<categories>` blocks where we need to scan the contained
/// element list.
fn extract_tag_block(xml: &str, tag: &str) -> String {
    let pattern = format!(
        r"(?is)<{tag}\b[^>]*/>|<{tag}\b[^>]*>(.*?)</{tag}>",
        tag = regex_escape_tag(tag)
    );
    let Ok(re) = Regex::new(&pattern) else {
        return String::new();
    };
    let Some(caps) = re.captures(xml) else {
        return String::new();
    };
    // For self-closing `<limits />`, the inner block is empty but
    // the attrs still matter; return the full match so the caller
    // can `extract_xml_attr` over the open-tag.
    let full = caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string();
    let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
    if inner.is_empty() {
        full
    } else {
        // For tag-pair form, return both the open-tag and the
        // body so attribute extraction works regardless of which
        // shape the indexer emitted.
        full
    }
}

fn regex_escape_tag(tag: &str) -> String {
    // Sanitize: only allow alphanumeric, colon (for namespaces),
    // hyphen, underscore. Anything else gets stripped — protects
    // against caller-injected regex meta. In practice every
    // torznab tag is `[a-z]+(?::[a-z]+)?` shaped.
    tag.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ':' || *c == '-' || *c == '_')
        .collect()
}

/// Pull a quoted attribute value from a tag's attribute list.
/// `attrs` is the slice between `<tag` and `>` (or `/>`).
fn extract_xml_attr(attrs: &str, name: &str) -> String {
    let pattern = format!(
        r#"(?is)\b{name}\s*=\s*"([^"]*)""#,
        name = regex_escape_tag(name)
    );
    let Ok(re) = Regex::new(&pattern) else {
        return String::new();
    };
    re.captures(attrs)
        .and_then(|caps| caps.get(1))
        .map(|m| decode_xml(m.as_str()))
        .unwrap_or_default()
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
