//! Layer 2 — Nyaa description parsing.
//!
//! Only runs when Layers 1 (filename) and 3 (group identity) come back with
//! a low-confidence verdict. Fetches the full Nyaa listing at
//! `https://nyaa.si/view/{id}`, extracts the markdown-rendered description
//! body, and scans it for structured source metadata that wasn't in the
//! filename or release group fingerprint.
//!
//! The classifier triggers this layer only for the ambiguous tail —
//! confident L1+L3 hits skip the fetch entirely. That's deliberate: Nyaa
//! doesn't publish rate limits, but sustained fast scraping will get you
//! tarpitted, so we cap this layer at one live request per second process-
//! wide and cache results indefinitely keyed by `info_hash`. A typical
//! classification sweep touches a few dozen releases at most, and the cache
//! means re-scoring the same listing (RSS polling, upgrade detection) never
//! hits the network.
//!
//! Confidence budget (capped at 0.85 so post-download ffprobe can still
//! override a misleading description):
//!
//! | Signal                                           | Confidence |
//! |--------------------------------------------------|------------|
//! | Explicit `Source:` / `Video Source:` line        | 0.85       |
//! | Known product name (Dragon Box, UHD BD, …)       | 0.80       |
//! | Free-text source keyword in description body    | 0.70       |
//!
//! This module does NOT fold evidence into a final decision — that's the
//! job of [`crate::services::source::aggregate`]. It just emits every piece
//! of evidence it finds.

use std::sync::LazyLock;
use std::time::Duration;

use scraper::{Html, Selector};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::models::nyaa_description_cache;
use crate::services::source::{Origin, Source, SourceEvidence};

const ORIGIN: Origin = Origin::Description;

/// Minimum spacing between live Nyaa fetches. Nyaa doesn't publish a rate
/// limit, but an unsolicited-scrape-looking pattern gets IPs tarpitted fast.
/// One request per second is what the plan doc specifies and is
/// comfortably below what a human browsing the site would generate.
const MIN_FETCH_INTERVAL: Duration = Duration::from_millis(1000);

/// Process-global throttle state for live Nyaa description fetches. Holding
/// this mutex across the sleep serializes all fetches: if two classifier
/// calls hit the network at the same moment, one waits while the other
/// completes its interval.
static LAST_FETCH: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

/// Process-global `reqwest::Client` for Nyaa description fetches. `Client`
/// wraps an internal connection pool and is designed to be shared — creating
/// a new one per request throws away any established keepalive connections
/// and forces a fresh TCP (and TLS) handshake every time. Build it once and
/// reuse it for the life of the process. Redirects are followed only
/// within the host they started on, so a redirect can't carry the fetch
/// off Nyaa.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Ryokan/0.1")
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let same_host = attempt
                .previous()
                .first()
                .is_some_and(|first| first.host_str() == attempt.url().host_str());
            if same_host && attempt.previous().len() < 3 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .expect("building the Nyaa description reqwest client should not fail")
});

/// A Nyaa listing page is ~50 KB.
const DESCRIPTION_BODY_CAP: usize = 2 << 20;

/// Classify a torrent by scraping its Nyaa description body.
///
/// `info_hash` is the cache key (stable and content-addressed). `view_url`
/// is the result's own link, consulted only on a cache miss and only when
/// [`nyaa_view_url`] maps it to a Nyaa listing page; any other link is no
/// evidence. A cached empty string is a valid "we fetched it and the
/// description was blank" state and produces no evidence; a cache miss
/// plus a failed fetch also produces no evidence (logged warn).
pub async fn classify_description(
    db: &SqlitePool,
    info_hash: &str,
    view_url: &str,
) -> Vec<SourceEvidence> {
    if info_hash.trim().is_empty() || view_url.trim().is_empty() {
        return Vec::new();
    }

    // Cache lookup first so repeated classifications of the same torrent
    // never cost a round trip, even under the rate limit.
    let cached = nyaa_description_cache::get(db, info_hash).await;
    let description_text = match cached {
        Some(text) => text,
        None => {
            let Some(page) = nyaa_view_url(view_url) else {
                tracing::debug!(
                    target: "ryokan::classify",
                    url = view_url,
                    "not a Nyaa listing link; skipping description fetch"
                );
                return Vec::new();
            };
            let fetched = match fetch_description(&page).await {
                Ok(text) => text,
                Err(err) => {
                    tracing::warn!(
                        target: "ryokan::classify",
                        url = view_url,
                        error = %err,
                        "Nyaa description fetch failed"
                    );
                    return Vec::new();
                }
            };
            nyaa_description_cache::upsert(db, info_hash, &fetched).await;
            fetched
        }
    };

    scan_description_for_signals(&description_text)
}

/// The Nyaa listing page to read for `link`; see
/// [`crate::services::nyaa::view_page_url`].
fn nyaa_view_url(link: &str) -> Option<String> {
    crate::services::nyaa::view_page_url(link)
}

#[cfg(test)]
fn nyaa_view_url_on(base: &str, link: &str) -> Option<String> {
    crate::services::nyaa::view_page_url_on(base, link)
}

/// Fetch `view_url`, parse the response as HTML, and return the plain-text
/// rendering of the `#torrent-description` block. All live fetches pass
/// through the global rate limiter.
async fn fetch_description(view_url: &str) -> Result<String, String> {
    rate_limit().await;

    let resp = HTTP_CLIENT
        .get(view_url)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e))?;
    let body = crate::services::http_body::read_capped(resp, DESCRIPTION_BODY_CAP).await?;

    Ok(extract_description_text(&String::from_utf8_lossy(&body)))
}

/// Sleep until at least `MIN_FETCH_INTERVAL` has elapsed since the previous
/// live fetch. Holding the mutex across the sleep serializes requests, so a
/// burst of classifier calls spaces out cleanly at one-per-second rather
/// than all firing simultaneously.
async fn rate_limit() {
    let mut guard = LAST_FETCH.lock().await;
    if let Some(last) = *guard {
        let elapsed = last.elapsed();
        if elapsed < MIN_FETCH_INTERVAL {
            tokio::time::sleep(MIN_FETCH_INTERVAL - elapsed).await;
        }
    }
    *guard = Some(Instant::now());
}

/// Parse a Nyaa view-page HTML response and return the plain-text content
/// of the description block. Nyaa renders submitted markdown into
/// `<div id="torrent-description">`; everything else is chrome.
fn extract_description_text(html: &str) -> String {
    let document = Html::parse_document(html);
    let Ok(selector) = Selector::parse("#torrent-description") else {
        return String::new();
    };
    let Some(node) = document.select(&selector).next() else {
        return String::new();
    };
    // `text()` yields the rendered text nodes in document order with
    // whitespace normalized to single-character gaps by the parser. We join
    // them with newlines so the line-oriented "Source:" matcher still sees
    // each logical line as a distinct unit.
    let mut out = String::new();
    for chunk in node.text() {
        let trimmed = chunk.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────
// Pure scanner
// ─────────────────────────────────────────────────────────────────────────

/// Scan plain-text description content for source-classification evidence.
///
/// This is the pure, deterministic half of the Layer 2 pipeline — all
/// fetching, caching, and rate-limiting is layered above it. Tests exercise
/// this function directly with realistic description bodies copied from
/// real Nyaa listings.
///
/// Rules, applied in order:
/// 1. Scan line-by-line for `Source:`/`Video Source:` labels and map the
///    captured value to a [`Source`] enum. Emits at 0.85.
/// 2. Scan the full body for known product names (Dragon Box, UHD BD,
///    SteelBook, …) that strongly imply a physical-media source. Emits at
///    0.80.
/// 3. Scan the full body for raw source keywords (BDRip, WEB-DL, …) as a
///    fallback. Emits at 0.70.
///
/// Multiple signals can be emitted — the aggregator handles conflict
/// resolution. Duplicate signals pointing at the same source are suppressed
/// so a description that says both "Source: BDMV" and "BluRay" only
/// contributes one BluRay evidence record.
pub fn scan_description_for_signals(text: &str) -> Vec<SourceEvidence> {
    let mut evidence: Vec<SourceEvidence> = Vec::new();
    if text.trim().is_empty() {
        return evidence;
    }

    let lower = text.to_ascii_lowercase();

    // Rule 1: structured Source: lines.
    for line in lower.lines() {
        let Some((label, value)) = split_label(line) else {
            continue;
        };
        // Accept any label that ends with "source" — covers "source",
        // "video source", "audio source", "main source", "raw source". We
        // treat the audio/video split as a single source signal because
        // hybrid releases are rare and the aggregator already flags
        // conflicts as needs_review.
        if !label.ends_with("source") {
            continue;
        }
        if let Some(src) = map_source_phrase(value) {
            push_unique(
                &mut evidence,
                SourceEvidence::new(src, 0.85, ORIGIN, format!("{}: {}", label, value.trim())),
            );
        }
    }

    // Rule 2: product names / physical releases.
    for (needle, src, detail) in PRODUCT_NAMES {
        if contains_phrase(&lower, needle) {
            push_unique(
                &mut evidence,
                SourceEvidence::new(*src, 0.80, ORIGIN, (*detail).to_string()),
            );
        }
    }

    // Rule 3: free-text keyword fallback — only emits when we didn't
    // already log evidence for that source from a stronger rule. Keeps
    // confidence at 0.70.
    for (needle, src) in FREE_TEXT_KEYWORDS {
        if evidence.iter().any(|e| e.source == *src) {
            continue;
        }
        if contains_phrase(&lower, needle) {
            push_unique(
                &mut evidence,
                SourceEvidence::new(*src, 0.70, ORIGIN, format!("keyword: {}", needle)),
            );
        }
    }

    evidence
}

/// Split a line on its first colon into (label, value). Trims both sides
/// and returns `None` if there's no colon or the label contains non-label
/// characters (avoiding false hits on URLs like `https://...` which contain
/// a colon but no label).
fn split_label(line: &str) -> Option<(String, &str)> {
    let (label, value) = line.split_once(':')?;
    let label_trim = label.trim();
    if label_trim.is_empty() || label_trim.len() > 32 {
        return None;
    }
    // Labels are letters, digits, spaces, or hyphens. Anything else —
    // slashes, dots, query params — almost certainly means this isn't a
    // metadata line.
    if !label_trim
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-')
    {
        return None;
    }
    Some((label_trim.to_string(), value))
}

/// Map a captured source value (the right-hand side of a `Source:` line)
/// to a concrete source enum. Accepts a variety of phrasings — `BDMV`,
/// `Blu-ray Disc`, `BD25`, `WEB-DL`, `Amazon WEB-DL`, `Dragon Box DVD`, …
/// Returns `None` if the value doesn't match any known fingerprint.
fn map_source_phrase(value: &str) -> Option<Source> {
    let clean = value.trim();
    if clean.is_empty() {
        return None;
    }
    // Order matters — check the most specific fingerprints first so
    // "BD-Remux" matches the BluRay rule before the generic "remux" one.
    const PHRASES: &[(&str, Source)] = &[
        ("bdmv", Source::BluRay),
        ("bd25", Source::BluRay),
        ("bd50", Source::BluRay),
        ("bd66", Source::BluRay),
        ("bd100", Source::BluRay),
        ("uhd bd", Source::BluRay),
        ("uhd bluray", Source::BluRay),
        ("uhd blu-ray", Source::BluRay),
        ("blu-ray disc", Source::BluRay),
        ("bluray disc", Source::BluRay),
        ("bdrip", Source::BluRay),
        ("bdremux", Source::BluRay),
        ("bd remux", Source::BluRay),
        ("blu-ray", Source::BluRay),
        ("bluray", Source::BluRay),
        ("remux", Source::BluRay),
        (" bd ", Source::BluRay),
        ("dragon box", Source::Dvd),
        ("r2j dvd", Source::Dvd),
        ("r2 dvd", Source::Dvd),
        ("dvdrip", Source::Dvd),
        ("dvd-rip", Source::Dvd),
        ("ld-rip", Source::Dvd), // LaserDisc -> closest bucket is DVD-era physical
        ("laserdisc", Source::Dvd),
        ("dvd", Source::Dvd),
        ("web-dl", Source::Web),
        ("webdl", Source::Web),
        ("webrip", Source::Web),
        ("amazon", Source::Web),
        ("crunchyroll", Source::Web),
        ("funimation", Source::Web),
        ("netflix", Source::Web),
        ("disney+", Source::Web),
        ("hidive", Source::Web),
        ("web", Source::Web),
        ("hdtv", Source::Hdtv),
        ("pdtv", Source::Tv),
        ("sdtv", Source::Tv),
        ("tvrip", Source::Tv),
    ];
    // Pad the value so leading/trailing " bd " fingerprints can match.
    let padded = format!(" {} ", clean);
    for (needle, src) in PHRASES {
        if padded.contains(needle) {
            return Some(*src);
        }
    }
    None
}

/// Product and physical-media names that imply a specific source without
/// needing a structured metadata line. Matched as whole phrases.
const PRODUCT_NAMES: &[(&str, Source, &str)] = &[
    ("dragon box", Source::Dvd, "product: Dragon Box (DVD set)"),
    ("uhd bd", Source::BluRay, "product: UHD BD"),
    ("uhd blu-ray", Source::BluRay, "product: UHD Blu-ray"),
    ("uhd bluray", Source::BluRay, "product: UHD Blu-ray"),
    ("steelbook", Source::BluRay, "product: SteelBook (BD)"),
    ("bdmv", Source::BluRay, "product: BDMV structure"),
    ("bdbox", Source::BluRay, "product: BD-Box"),
    ("bd-box", Source::BluRay, "product: BD-Box"),
];

/// Free-text fallback keywords. These are only consulted if stronger rules
/// didn't already contribute a signal for the corresponding source.
const FREE_TEXT_KEYWORDS: &[(&str, Source)] = &[
    ("bdrip", Source::BluRay),
    ("bd-rip", Source::BluRay),
    ("bdremux", Source::BluRay),
    ("blu-ray", Source::BluRay),
    ("bluray", Source::BluRay),
    ("web-dl", Source::Web),
    ("webdl", Source::Web),
    ("webrip", Source::Web),
    ("dvdrip", Source::Dvd),
    ("dvd-rip", Source::Dvd),
    ("hdtv", Source::Hdtv),
];

/// Case-insensitive phrase search. `needle` is already lowercase; `haystack`
/// is assumed to have been lowercased by the caller. Used as a cheap
/// substring search — we don't need word boundaries because the constants
/// are long enough to avoid false positives ("bluray" won't land inside
/// another English word).
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    !needle.is_empty() && haystack.contains(needle)
}

/// Append an evidence record only if no prior record already matches on
/// `(source, origin)`. Keeps the evidence list de-duplicated per-layer so
/// a description that mentions "BDRip" three times doesn't triple-count.
fn push_unique(list: &mut Vec<SourceEvidence>, item: SourceEvidence) {
    if list
        .iter()
        .any(|e| e.source == item.source && e.origin == item.origin)
    {
        return;
    }
    list.push(item);
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sources_of(text: &str) -> Vec<Source> {
        scan_description_for_signals(text)
            .into_iter()
            .map(|e| e.source)
            .collect()
    }

    #[test]
    fn empty_description_emits_no_evidence() {
        assert!(scan_description_for_signals("").is_empty());
        assert!(scan_description_for_signals("   \n \t ").is_empty());
    }

    #[test]
    fn structured_source_bdmv_classifies_as_bluray() {
        let text = "\
Source: BDMV
Video: 1080p HEVC 10-bit
Audio: FLAC 2.0
Subtitles: ASS/SSA
";
        let evidence = scan_description_for_signals(text);
        assert!(evidence.iter().any(|e| e.source == Source::BluRay));
        // 0.85 because it was a structured Source: line.
        let bd = evidence
            .iter()
            .find(|e| e.source == Source::BluRay)
            .unwrap();
        assert!((bd.confidence - 0.85).abs() < 1e-4);
    }

    #[test]
    fn structured_video_source_web_dl_classifies_as_web() {
        let text = "\
Video Source: Amazon WEB-DL
Audio Source: Amazon WEB-DL
Video Codec: H.264
";
        let evidence = scan_description_for_signals(text);
        assert!(evidence.iter().any(|e| e.source == Source::Web));
        let web = evidence.iter().find(|e| e.source == Source::Web).unwrap();
        assert!((web.confidence - 0.85).abs() < 1e-4);
    }

    #[test]
    fn structured_source_bluray_disc() {
        let text = "Source: Blu-ray Disc\nEncoder: anon\n";
        assert!(sources_of(text).contains(&Source::BluRay));
    }

    #[test]
    fn structured_source_dragon_box_dvd() {
        let text = "Source: Dragon Box DVD\nVideo: MPEG-2\n";
        let evidence = scan_description_for_signals(text);
        assert!(evidence.iter().any(|e| e.source == Source::Dvd));
    }

    #[test]
    fn product_name_dragon_box_without_label() {
        // Plain mention in a paragraph, not a structured metadata line.
        let text = "Sourced from the Dragon Box release with upscale from 480p.";
        let evidence = scan_description_for_signals(text);
        let dvd = evidence
            .iter()
            .find(|e| e.source == Source::Dvd)
            .expect("expected Dragon Box to be detected");
        // Product-name match gets 0.80.
        assert!((dvd.confidence - 0.80).abs() < 1e-4);
    }

    #[test]
    fn product_name_uhd_bluray() {
        let text = "Encoded from the UHD Blu-ray release.";
        assert!(sources_of(text).contains(&Source::BluRay));
    }

    #[test]
    fn free_text_bdrip_falls_through_to_keyword_rule() {
        // No structured label, no product name — just BDRip mentioned in
        // the body. Should still emit a BluRay signal at 0.70.
        let text = "This is a BDRip with HEVC encoding from the Japanese BD.";
        let evidence = scan_description_for_signals(text);
        let bd = evidence
            .iter()
            .find(|e| e.source == Source::BluRay)
            .unwrap();
        assert!((bd.confidence - 0.70).abs() < 1e-4);
    }

    #[test]
    fn structured_rule_beats_free_text_rule() {
        // If there's both a "Source: BDMV" line AND stray "bdrip" text,
        // only one BluRay signal should be emitted (the structured one,
        // which is considered first).
        let text = "\
Source: BDMV
Notes: not a bdrip
";
        let evidence = scan_description_for_signals(text);
        let bluray: Vec<&SourceEvidence> = evidence
            .iter()
            .filter(|e| e.source == Source::BluRay)
            .collect();
        assert_eq!(bluray.len(), 1, "expected a single BluRay signal");
        // And that signal's confidence should be the structured one.
        assert!((bluray[0].confidence - 0.85).abs() < 1e-4);
    }

    #[test]
    fn structured_audio_and_video_source_dedup_to_single_signal() {
        // Hybrid-source releases list both Video Source: and Audio Source:
        // — we treat them as one signal each, but because they agree
        // (both BluRay), only one evidence record should land.
        let text = "\
Video Source: Blu-ray Disc
Audio Source: Blu-ray Disc
";
        let evidence = scan_description_for_signals(text);
        let bluray: Vec<_> = evidence
            .iter()
            .filter(|e| e.source == Source::BluRay)
            .collect();
        assert_eq!(bluray.len(), 1);
    }

    #[test]
    fn structured_audio_video_source_disagreement_emits_conflict() {
        // When video and audio sources disagree, emit both — the aggregator
        // will detect the strong conflict and flag the release for review.
        let text = "\
Video Source: Blu-ray Disc
Audio Source: Amazon WEB-DL
";
        let evidence = scan_description_for_signals(text);
        assert!(evidence.iter().any(|e| e.source == Source::BluRay));
        assert!(evidence.iter().any(|e| e.source == Source::Web));
    }

    #[test]
    fn dvdrip_keyword() {
        let text = "DVDRip encoded in XviD.";
        assert!(sources_of(text).contains(&Source::Dvd));
    }

    #[test]
    fn hdtv_keyword() {
        let text = "Captured from an HDTV broadcast at 720p.";
        assert!(sources_of(text).contains(&Source::Hdtv));
    }

    #[test]
    fn unrelated_colons_are_not_mistaken_for_metadata() {
        // URLs and timestamps contain colons. The label-validation step
        // should reject them so we don't emit bogus evidence from a
        // "https://..." line.
        let text = "\
See https://example.com/anime/12345 for details.
Runtime: 23:52
Episodes: 12
";
        assert!(scan_description_for_signals(text).is_empty());
    }

    #[test]
    fn duplicate_keyword_hits_dedup() {
        // Multiple mentions of the same keyword → single evidence record.
        let text = "A BDRip. Also a BDRip. Still a BDRip. BDRip BDRip BDRip.";
        let evidence = scan_description_for_signals(text);
        let bd_count = evidence
            .iter()
            .filter(|e| e.source == Source::BluRay)
            .count();
        assert_eq!(bd_count, 1);
    }

    #[test]
    fn description_without_any_source_info_produces_no_signal() {
        let text = "\
A mini-encode of the fan-favorite episode.
Staff credits preserved. Duration: 24 minutes.
";
        assert!(scan_description_for_signals(text).is_empty());
    }

    #[test]
    fn steelbook_detected_as_bluray_product() {
        let text = "Remastered edition from the 2023 SteelBook release.";
        assert!(sources_of(text).contains(&Source::BluRay));
    }

    #[test]
    fn source_netflix_classifies_as_web() {
        let text = "Source: Netflix\nVideo: 2160p H.265\n";
        assert!(sources_of(text).contains(&Source::Web));
    }

    #[test]
    fn structured_source_bd_remux() {
        let text = "Source: BD-Remux\n";
        assert!(sources_of(text).contains(&Source::BluRay));
    }

    #[test]
    fn extract_description_pulls_only_description_div() {
        let html = r#"
<html>
<head><title>Nyaa</title></head>
<body>
<div class="panel-body">chrome stuff that should not leak</div>
<div id="torrent-description">
Source: BDMV<br>
Video: 1080p HEVC
</div>
<footer>footer stuff</footer>
</body>
</html>
"#;
        let text = extract_description_text(html);
        assert!(text.contains("BDMV"));
        assert!(!text.contains("chrome stuff"));
        assert!(!text.contains("footer"));
    }

    #[test]
    fn nyaa_view_url_keeps_listing_pages_and_maps_download_links() {
        let on = |link| nyaa_view_url_on("https://nyaa.si", link);
        assert_eq!(
            on("https://nyaa.si/view/1713886").as_deref(),
            Some("https://nyaa.si/view/1713886")
        );
        assert_eq!(
            on("https://NYAA.si/view/1713886#comments").as_deref(),
            Some("https://nyaa.si/view/1713886")
        );
        assert_eq!(
            on("https://nyaa.si/download/1713886.torrent").as_deref(),
            Some("https://nyaa.si/view/1713886")
        );
        // The test seam: a wiremock base keeps its own origin.
        assert_eq!(
            nyaa_view_url_on("http://127.0.0.1:4545", "http://127.0.0.1:4545/view/9").as_deref(),
            Some("http://127.0.0.1:4545/view/9")
        );
    }

    #[test]
    fn nyaa_view_url_refuses_every_other_link() {
        for link in [
            // A torznab download URL, which carries the indexer's key.
            "http://prowlarr:9696/1/download?apikey=secret&link=abc",
            // Internal and metadata addresses a feed could name.
            "http://127.0.0.1:8080/view/1",
            "http://169.254.169.254/latest/meta-data/",
            // Nyaa look-alikes and the wrong scheme or port.
            "https://nyaa.si.evil.example/view/1",
            "https://evil.example/nyaa.si/view/1",
            "http://nyaa.si/view/1",
            "https://nyaa.si:8443/view/1",
            // Nyaa, but not a listing page.
            "https://nyaa.si/view/1/edit",
            "https://nyaa.si/view/",
            "https://nyaa.si/?q=x",
            "https://nyaa.si/download/1.torrent.exe",
            // SeaDex leaves relative and non-URL strings here.
            "/torrents.php?id=1",
            "Chihiro",
            "",
        ] {
            assert_eq!(nyaa_view_url_on("https://nyaa.si", link), None, "{link}");
        }
    }
}
