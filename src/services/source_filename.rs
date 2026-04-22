//! Layer 1 — filename token parsing via anitomy.
//!
//! Given a torrent title or filename, extract source/resolution/remux info
//! and emit weighted evidence records. This is the cheapest and most commonly
//! used classification layer — every pre-download classification call runs
//! through it first, and it's also the entry point for on-disk filename-based
//! classification.
//!
//! Confidence budget per signal type (capped at 0.95 so a perfect filename
//! match leaves room for post-download layers to correct a misnamed release):
//!
//! | Signal                                        | Confidence |
//! |-----------------------------------------------|------------|
//! | Explicit source keyword (BDRip/WEB-DL/HDTV…)  | 0.95       |
//! | DVD resolution override of BD keyword         | 0.85       |
//! | Streaming platform tag (CR/AMZN/NF/…)         | 0.90       |
//! | BD-exclusive audio codec (FLAC, TrueHD, …)    | 0.85       |
//! | Web-typical audio codec (AAC, DDP, E-AC-3)    | 0.75       |
//!
//! This module does NOT fold evidence into a final source decision — that's
//! the job of [`crate::services::source::aggregate`]. It just emits every
//! piece of evidence it finds so the aggregator has full context.

use anitomy::{Anitomy, ElementCategory};

use crate::services::source::{Origin, Resolution, Source, SourceEvidence, WebKind, contains_word};

const ORIGIN: Origin = Origin::Filename;

/// Output of Layer 1 classification.
///
/// `evidence` is NOT yet aggregated — it's handed off to
/// `source::aggregate` together with evidence from later layers.
#[derive(Debug, Clone)]
pub struct FilenameClassification {
    pub evidence: Vec<SourceEvidence>,
    pub resolution: Resolution,
    pub is_remux: bool,
    /// True when the filename indicates a raw BDMV / BD-Raw release —
    /// the actual disc structure rather than an MKV-wrapped remux. Set
    /// when tokens like `BDMV`, `BD-RAW`, `BDRAW`, or `ISO` appear.
    /// Distinct from `is_remux` and treated as a separate, higher tier.
    pub is_bdmv: bool,
    /// Distinguishes WEB-DL vs WEBRip when the filename is specific
    /// enough to tell them apart. Bare "WEB" tokens leave this as
    /// `WebKind::Unknown` so the aggregator doesn't claim certainty
    /// the filename never indicated.
    pub web_kind: WebKind,
    /// Release group extracted from the title, if any. Consumed by Layer 3
    /// (group identity table) downstream.
    pub release_group: Option<String>,
}

impl FilenameClassification {
    pub fn empty() -> Self {
        Self {
            evidence: Vec::new(),
            resolution: Resolution::Unknown,
            is_remux: false,
            is_bdmv: false,
            web_kind: WebKind::Unknown,
            release_group: None,
        }
    }
}

/// Parse a torrent title and emit classification evidence.
///
/// Uses anitomy to tokenize the title into structured fields (`Source`,
/// `AudioTerm`, `VideoResolution`, `ReleaseGroup`) and applies weighted
/// scoring rules on top. Anitomy construction is cheap — roughly equivalent
/// to one `Vec` allocation — so we build a fresh parser per call rather than
/// hold shared state.
pub fn classify_filename(title: &str) -> FilenameClassification {
    let mut result = FilenameClassification::empty();
    if title.trim().is_empty() {
        return result;
    }

    // Anitomy rejects NUL bytes; strip them defensively. Real torrent titles
    // shouldn't contain them, but a corrupted feed byte shouldn't panic us.
    let clean = if title.contains('\0') {
        title.replace('\0', "")
    } else {
        title.to_string()
    };

    let mut ani = Anitomy::new();
    // anitomy reports Err when it couldn't find an AnimeTitle, but in both
    // Ok/Err cases it still fills the `Elements` with whatever it parsed.
    let elements = match ani.parse(&clean) {
        Ok(e) => e,
        Err(e) => e,
    };

    // ── Resolution ────────────────────────────────────────────────────────
    if let Some(res_str) = elements.get(ElementCategory::VideoResolution) {
        result.resolution = Resolution::from_str(res_str);
        if result.resolution == Resolution::Unknown {
            // anitomy sometimes captures the dimension form (e.g. "1920x1080")
            // which Resolution::from_str doesn't understand. Fall through to
            // the dimension parser.
            result.resolution = parse_dimensions(res_str);
        }
    }
    if result.resolution == Resolution::Unknown {
        result.resolution = parse_dimensions(&clean);
    }

    // ── Release group ─────────────────────────────────────────────────────
    if let Some(group) = elements.get(ElementCategory::ReleaseGroup) {
        let trimmed = group.trim();
        if !trimmed.is_empty() {
            result.release_group = Some(trimmed.to_string());
        }
    }

    let title_lower = clean.to_ascii_lowercase();

    // ── Remux detection ───────────────────────────────────────────────────
    // anitomy tags some releases with Source="Remux" and some with it baked
    // into the VideoTerm; a direct substring check catches both.
    result.is_remux = title_lower.contains("remux");

    // ── BDMV / BD-Raw detection ───────────────────────────────────────────
    // BDMV releases ship the disc structure intact (folder layout or full
    // ISO) instead of an MKV-wrapped remux. They're a distinct tier in the
    // anime scene because of the audio-track and chapter fidelity, and
    // because file sizes are dramatically larger. Match BDMV/BD-RAW/BDRAW
    // and "Disc" / "Disk" only when accompanied by another BD signal — bare
    // "Disc" is too noisy on its own.
    let bdmv_keyword = contains_word(&title_lower, "bdmv")
        || contains_word(&title_lower, "bd-raw")
        || contains_word(&title_lower, "bdraw")
        || contains_word(&title_lower, "bdiso");
    if bdmv_keyword {
        result.is_bdmv = true;
        // BDMV implies a Remux-class container truth — the encode is the
        // disc itself, not a re-encode. We still leave is_remux alone (the
        // two flags are mutually exclusive at the label level), but we
        // emit a strong BluRay evidence record so the aggregator commits
        // to the right source even if no other BD token appears.
    }

    // ── WEB-DL vs WEBRip distinction ──────────────────────────────────────
    // Sonarr's quality definitions split "WEB" into two tiers. Detect both
    // up front so it's available regardless of which path emits the Web
    // evidence below. Whole-word matching (with punctuation tolerance)
    // ensures "WEBRip" inside a longer token doesn't false-match.
    if contains_word(&title_lower, "web-dl")
        || contains_word(&title_lower, "webdl")
        || contains_word(&title_lower, "web.dl")
    {
        result.web_kind = WebKind::WebDl;
    } else if contains_word(&title_lower, "webrip")
        || contains_word(&title_lower, "web-rip")
        || contains_word(&title_lower, "web.rip")
    {
        result.web_kind = WebKind::WebRip;
    }

    // ── Explicit source keyword ───────────────────────────────────────────
    // Try anitomy's structured Source field first, then fall back to scanning
    // the raw title for source tokens. Anitomy's keyword list is good but
    // doesn't catch every variant (e.g. it sometimes misses "WEB-DL" when the
    // title uses dot-separators, and `PDTV` isn't consistently tagged). The
    // fallback scanner also lets us pick up a SECOND source signal in the
    // rare multi-source torrent title, which feeds the review-detection rule.
    if let Some(src_str) = elements.get(ElementCategory::Source)
        && let Some((src, conf, detail)) = source_from_keyword(src_str, result.resolution)
    {
        result
            .evidence
            .push(SourceEvidence::new(src, conf, ORIGIN, detail));
    }
    for (token, mapped_src) in SOURCE_FALLBACK_TOKENS {
        if !contains_word(&title_lower, token) {
            continue;
        }
        // Skip if we've already emitted evidence for this source — avoids
        // double-counting when anitomy and the fallback scanner both catch
        // the same token.
        if result.evidence.iter().any(|e| e.source == *mapped_src) {
            continue;
        }
        // Apply the DVD-resolution override for BD-class tokens.
        let (final_src, final_conf, detail) = if *mapped_src == Source::BluRay
            && matches!(result.resolution, Resolution::R480p | Resolution::R576p)
        {
            (
                Source::Dvd,
                0.85,
                format!("{} + {} → DVD override", token, result.resolution.as_str()),
            )
        } else {
            // Bare "web" is a weaker signal than "web-dl"/"webrip": the
            // latter two pin WebKind explicitly, bare "web" leaves
            // WebKind::Unknown. Drop its confidence to 0.85 so the
            // aggregator leaves room for post-download layers (ffprobe,
            // group-map) to overrule when they disagree. Every other
            // explicit source keyword stays at 0.95.
            let conf = if *token == "web" { 0.85 } else { 0.95 };
            (*mapped_src, conf, format!("keyword: {}", token))
        };
        result
            .evidence
            .push(SourceEvidence::new(final_src, final_conf, ORIGIN, detail));
    }

    // Remux tends to live in VideoTerm rather than Source for some releases.
    // If we detected remux but no BD evidence has been emitted yet, add one.
    if result.is_remux && !result.evidence.iter().any(|e| e.source == Source::BluRay) {
        result.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.95,
            ORIGIN,
            "Remux keyword".to_string(),
        ));
    }

    // BDMV / BD-Raw is an even stronger BluRay signal than Remux — the
    // release is literally the disc. Emit BluRay evidence if no source
    // token has fired yet so the aggregator doesn't fall back to Unknown
    // on a barebones "[group] Series Vol1 BDMV" title.
    if result.is_bdmv && !result.evidence.iter().any(|e| e.source == Source::BluRay) {
        result.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.95,
            ORIGIN,
            "BDMV keyword".to_string(),
        ));
    }

    // ── Streaming platform tag ────────────────────────────────────────────
    if let Some(platform) = detect_platform_tag(&title_lower) {
        result.evidence.push(SourceEvidence::new(
            Source::Web,
            0.90,
            ORIGIN,
            format!("platform tag: {}", platform),
        ));
    }

    // ── Audio codec inference ─────────────────────────────────────────────
    // Gather every audio term anitomy identified, plus what we can scan from
    // the raw title. Scanning the raw title catches releases that pack codec
    // tokens into brackets where anitomy doesn't tag them as AudioTerm.
    let audio_terms: Vec<String> = elements
        .get_all(ElementCategory::AudioTerm)
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    if let Some((src, conf, detail)) = audio_signal(&audio_terms, &title_lower) {
        result
            .evidence
            .push(SourceEvidence::new(src, conf, ORIGIN, detail));
    }

    result
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

/// Map a source keyword (as emitted by anitomy's `Source` field) to our
/// `Source` enum and a confidence score.
///
/// Applies the DVD-resolution override: a BD-class keyword combined with a
/// 480p or 576p resolution is treated as a DVD source. NTSC/PAL DVDs have
/// exactly those pixel heights, and "BDRip 480p" tags in the wild usually
/// mean the package was shipped on a BD but the video was encoded from the
/// DVD master.
fn source_from_keyword(keyword: &str, res: Resolution) -> Option<(Source, f32, String)> {
    let normalized: String = keyword
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '.')
        .collect::<String>()
        .to_ascii_lowercase();
    let (src, detail) = match normalized.as_str() {
        "bdrip" | "bluray" | "bd" | "bdremux" | "bdmv" | "bdraw" | "bdiso" => {
            (Source::BluRay, "BD keyword")
        }
        "remux" => (Source::BluRay, "Remux keyword"),
        "dvd" | "dvdrip" | "r2j" | "r2" => (Source::Dvd, "DVD keyword"),
        "webdl" | "web" | "webrip" => (Source::Web, "Web keyword"),
        "hdtv" => (Source::Hdtv, "HDTV keyword"),
        "tv" | "tvrip" | "pdtv" | "sdtv" => (Source::Tv, "TV keyword"),
        _ => return None,
    };

    if src == Source::BluRay && matches!(res, Resolution::R480p | Resolution::R576p) {
        return Some((
            Source::Dvd,
            0.85,
            format!("{} + {} → DVD override", detail, res.as_str()),
        ));
    }

    Some((src, 0.95, detail.to_string()))
}

/// Hierarchy of audio-based classification.
///
/// Lossless audio is BD-exclusive in practice (no streaming platform ships
/// FLAC/TrueHD/DTS-HD MA), so a single lossless term is a confident BD
/// signal. Lossy Dolby variants (DDP/E-AC-3) are streaming-native. Plain AAC
/// is suggestive but weaker because older BD rips also used it.
///
/// We merge both anitomy-tagged `AudioTerm`s and a raw-title scan so that
/// releases with codecs packed into brackets (common for mini-encode groups)
/// still contribute evidence.
fn audio_signal(audio_terms: &[String], title_lower: &str) -> Option<(Source, f32, String)> {
    let mut lossless_hits: Vec<&'static str> = Vec::new();
    let mut web_hits: Vec<&'static str> = Vec::new();

    for raw in audio_terms {
        classify_audio_token(raw, &mut lossless_hits, &mut web_hits);
    }

    // Raw-title fallback for codec tokens anitomy didn't catch.
    for &token in LOSSLESS_TOKENS {
        if contains_word(title_lower, token) && !lossless_hits.contains(&token) {
            lossless_hits.push(token);
        }
    }
    for &token in WEB_AUDIO_TOKENS {
        if contains_word(title_lower, token) && !web_hits.contains(&token) {
            web_hits.push(token);
        }
    }

    if !lossless_hits.is_empty() {
        Some((
            Source::BluRay,
            0.85,
            format!("lossless audio ({})", lossless_hits.join(", ")),
        ))
    } else if !web_hits.is_empty() {
        Some((
            Source::Web,
            0.75,
            format!("web audio ({})", web_hits.join(", ")),
        ))
    } else {
        None
    }
}

fn classify_audio_token(raw: &str, lossless: &mut Vec<&'static str>, web: &mut Vec<&'static str>) {
    let normalized: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '.' && *c != '_')
        .collect::<String>()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "flac" => push_unique(lossless, "flac"),
        "truehd" => push_unique(lossless, "truehd"),
        "dtshd" | "dtshdma" | "dtshdhra" => push_unique(lossless, "dts-hd"),
        "pcm" | "lpcm" => push_unique(lossless, "pcm"),
        "ddp" | "ddp51" | "ddp71" | "eac3" | "dd+" => push_unique(web, "ddp"),
        "aac" => push_unique(web, "aac"),
        _ => {}
    }
}

fn push_unique(list: &mut Vec<&'static str>, item: &'static str) {
    if !list.contains(&item) {
        list.push(item);
    }
}

const LOSSLESS_TOKENS: &[&str] = &["flac", "truehd", "dts-hd", "dtshd", "pcm", "lpcm"];
const WEB_AUDIO_TOKENS: &[&str] = &["ddp", "eac3", "e-ac-3"];

/// Raw-title fallback for source keywords. Anitomy's structured Source field
/// is accurate when it fires, but misses a few variants in practice —
/// particularly when release names use dot-separators (e.g.
/// `Show.S01E01.WEB-DL.DDP`) or rare TV broadcast tags (`PDTV`, `SDTV`).
/// The scanner runs after anitomy and only contributes evidence for a
/// source that hasn't already been emitted, so anitomy's stricter parse
/// takes precedence when both agree.
const SOURCE_FALLBACK_TOKENS: &[(&str, Source)] = &[
    // BluRay variants — BDMV/BD-RAW/BDRAW are caught by the dedicated
    // is_bdmv detector above and emit their own evidence record, so they
    // don't need to appear here.
    ("bdrip", Source::BluRay),
    ("bdremux", Source::BluRay),
    ("bluray", Source::BluRay),
    ("blu-ray", Source::BluRay),
    // Web variants — the bare "web" entry catches space-separated
    // forms like "(WEB 1080p AV1 EAC-3)" that anitomy sometimes
    // misses as an ElementCategory::Source and that the hyphenated
    // variants don't cover. The dedup check inside `classify_filename`'s
    // fallback loop (`.any(|e| e.source == *mapped_src)`) prevents
    // double-counting when both the specific (web-dl) and the bare
    // (web) token are present in the same title. Bare "web" also gets
    // a lower confidence (0.85 vs 0.95 for the hyphenated variants) —
    // see the comment in that loop for the rationale.
    ("web-dl", Source::Web),
    ("webrip", Source::Web),
    ("webdl", Source::Web),
    ("web", Source::Web),
    // DVD variants
    ("dvdrip", Source::Dvd),
    // HDTV
    ("hdtv", Source::Hdtv),
    // OTA TV broadcast variants
    ("pdtv", Source::Tv),
    ("sdtv", Source::Tv),
    ("tvrip", Source::Tv),
];

/// Detect streaming-platform tags in a lowercased title.
///
/// Each entry is checked as a whole word (bracketed/dotted/underscored
/// boundaries), so "nf" matches "[NF]" and "NF." but not "wolf". Order
/// matters only for logging: first match wins.
fn detect_platform_tag(title_lower: &str) -> Option<&'static str> {
    PLATFORM_TAGS
        .iter()
        .find(|tag| contains_word(title_lower, tag))
        .copied()
}

const PLATFORM_TAGS: &[&str] = &[
    "crunchyroll",
    "cr",
    "amazon",
    "amzn",
    "disney+",
    "disney",
    "dsnp",
    "hidive",
    "hidi",
    "netflix",
    "nf",
    "funimation",
    "funi",
    "hmax",
    "appletv",
    "atvp",
    "adn",
];

/// Parse a dimension fragment like "1920x1080" or "1280x720" into a
/// `Resolution`. Returns `Resolution::Unknown` if no dimension pair is found
/// or the pair doesn't round-trip to a known tier.
fn parse_dimensions(text: &str) -> Resolution {
    // Walk the bytes looking for a digit run, an x/X separator, another
    // digit run. Cheaper than instantiating a regex and easier to reason
    // about than keeping a LazyLock alive.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip non-digits.
        while i < bytes.len() && !bytes[i].is_ascii_digit() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let w_len = i - start;
        if !(3..=4).contains(&w_len) {
            continue;
        }
        if i < bytes.len() && (bytes[i] == b'x' || bytes[i] == b'X') {
            let sep = i;
            let h_start = sep + 1;
            let mut j = h_start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let h_len = j - h_start;
            if (3..=4).contains(&h_len) {
                let w: u32 = std::str::from_utf8(&bytes[start..sep])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let h: u32 = std::str::from_utf8(&bytes[h_start..j])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let r = Resolution::from_dimensions(w, h);
                if r != Resolution::Unknown {
                    return r;
                }
                i = j;
                continue;
            }
        }
    }
    Resolution::Unknown
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::source::aggregate;

    /// Run a title through Layer 1, then fold its evidence through the
    /// aggregator and return the final source decision. This is the path the
    /// production caller will take.
    fn classify(title: &str) -> (Source, Resolution, bool) {
        let res = classify_filename(title);
        let mut result = aggregate(&res.evidence);
        result.resolution = res.resolution;
        result.is_remux = res.is_remux;
        (result.source, result.resolution, result.is_remux)
    }

    // ── Realistic release titles ─────────────────────────────────────────

    #[test]
    fn subsplease_has_no_layer1_evidence() {
        // SubsPlease releases contain no platform tag or source keyword —
        // Layer 1 alone should be unable to classify them. They're picked up
        // by Layer 3 (group table).
        let fc = classify_filename("[SubsPlease] Sousou no Frieren - 01 (1080p) [ABCD1234].mkv");
        assert_eq!(fc.resolution, Resolution::R1080p);
        assert_eq!(fc.release_group.as_deref(), Some("SubsPlease"));
        assert!(
            fc.evidence.is_empty(),
            "expected no evidence for SubsPlease, got {:?}",
            fc.evidence
        );
    }

    #[test]
    fn erai_raws_cr_tag_classifies_as_web() {
        let (src, res, _) = classify(
            "[Erai-raws] Sousou no Frieren - 01 [1080p][Multiple Subtitle][CR][A1B2C3D4].mkv",
        );
        assert_eq!(src, Source::Web);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn explicit_bdrip_classifies_as_bluray() {
        let (src, res, _) =
            classify("[Beatrice-Raws] Sousou no Frieren 01 [BDRip 1920x1080 HEVC FLAC].mkv");
        assert_eq!(src, Source::BluRay);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn explicit_web_dl_classifies_as_web() {
        let (src, res, _) = classify("Sousou.no.Frieren.S01E01.1080p.WEB-DL.DDP5.1.H.264-NTb.mkv");
        assert_eq!(src, Source::Web);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn bare_web_in_parens_classifies_as_web() {
        // Regression: "[miniKaizoku] Jujutsu Kaisen Season 3 (WEB 1080p
        // AV1 EAC-3) | The Culling Game Part 1" used to classify as
        // Unknown source because SOURCE_FALLBACK_TOKENS didn't include
        // bare "web" (only the hyphenated web-dl / webrip forms). The
        // title's space-separated WEB token inside parens now matches
        // the fallback and fires Source::Web evidence.
        let (src, res, _) = classify(
            "[miniKaizoku] Jujutsu Kaisen Season 3 (WEB 1080p AV1 EAC-3) | The Culling Game Part 1",
        );
        assert_eq!(src, Source::Web);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn hdtv_keyword() {
        let (src, res, _) = classify("Dragon.Ball.Z.Kai.S01E01.720p.HDTV.x264-anon.mkv");
        assert_eq!(src, Source::Hdtv);
        assert_eq!(res, Resolution::R720p);
    }

    #[test]
    fn vcb_studio_flac_audio_bluray() {
        // VCB-Studio doesn't write "BDRip" in their tags — classification
        // comes from the FLAC audio codec.
        let (src, res, _) =
            classify("[VCB-Studio] Made in Abyss [01][Hi10p_1080p][x264_2flac].mkv");
        assert_eq!(src, Source::BluRay);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn bluray_keyword_with_dimensions() {
        let (src, res, _) = classify("[Coalgirls] Made in Abyss 01 (1920x1080 Blu-ray FLAC).mkv");
        assert_eq!(src, Source::BluRay);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn remux_flag_detected() {
        let fc =
            classify_filename("Sousou.no.Frieren.S01.1080p.BluRay.REMUX.AVC.DTS-HD.MA.5.1-FGT.mkv");
        assert!(fc.is_remux);
        let mut result = aggregate(&fc.evidence);
        result.resolution = fc.resolution;
        assert_eq!(result.source, Source::BluRay);
        assert_eq!(result.resolution, Resolution::R1080p);
    }

    #[test]
    fn netflix_2160p_web() {
        let (src, res, _) =
            classify("Sousou.no.Frieren.S01E01.2160p.NF.WEB-DL.DDP5.1.Atmos.HEVC-FLUX.mkv");
        assert_eq!(src, Source::Web);
        assert_eq!(res, Resolution::R2160p);
    }

    #[test]
    fn amzn_tag_web() {
        let (src, _, _) = classify("Show.Name.S01E01.1080p.AMZN.WEB-DL.DDP2.0.H.264-XYZ.mkv");
        assert_eq!(src, Source::Web);
    }

    #[test]
    fn disney_plus_tag_web() {
        let (src, _, _) = classify("Show.Name.S01E01.1080p.DSNP.WEB-DL.DDP5.1.H.264-ABC.mkv");
        assert_eq!(src, Source::Web);
    }

    #[test]
    fn hidive_tag_web() {
        let (src, _, _) = classify("Show.Name.S01E01.1080p.HIDI.WEB-DL.AAC2.0.H.264-DEF.mkv");
        assert_eq!(src, Source::Web);
    }

    #[test]
    fn dvdrip_480p() {
        let (src, res, _) = classify("Old.Anime.S01E01.DVDRip.480p.XviD-ABC.mkv");
        assert_eq!(src, Source::Dvd);
        assert_eq!(res, Resolution::R480p);
    }

    #[test]
    fn bdrip_480p_downgrades_to_dvd() {
        // BD keyword + 480p → DVD override. Old "BDRip 480p" tags generally
        // mean the video master was sourced from DVD.
        let (src, res, _) = classify("[SomeGroup] Old Anime 01 [BDRip 480p AAC].mkv");
        assert_eq!(src, Source::Dvd);
        assert_eq!(res, Resolution::R480p);
    }

    #[test]
    fn pdtv_tv_source() {
        let (src, _, _) = classify("Sazae-san.Ep01.PDTV.x264-anon.mkv");
        assert_eq!(src, Source::Tv);
    }

    #[test]
    fn tvrip_classifies_as_tv() {
        let (src, _, _) = classify("Cowboy.Bebop.S01E01.TVRip.XviD-ABC.mkv");
        assert_eq!(src, Source::Tv);
    }

    #[test]
    fn dvdrip_ac3() {
        let (src, res, _) = classify("Neon.Genesis.Evangelion.S01E01.DVDRip.x264.AC3-ABC.mkv");
        assert_eq!(src, Source::Dvd);
        assert!(matches!(
            res,
            Resolution::R480p | Resolution::R576p | Resolution::Unknown
        ));
    }

    #[test]
    fn judas_bd_mini_encode() {
        let (src, res, _) = classify("[Judas] Sousou no Frieren - S01E01 (BD 1080p HEVC Opus).mkv");
        assert_eq!(src, Source::BluRay);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn kawaiika_bdrip() {
        let (src, res, _) =
            classify("[Kawaiika-Raws] Sousou no Frieren 01 [BD 1080p HEVC E-AC3].mkv");
        assert_eq!(src, Source::BluRay);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn sam_bdrip_flac() {
        let (src, res, _) = classify("[sam] Made in Abyss S01E01 [BDRip 1080p HEVC FLAC].mkv");
        assert_eq!(src, Source::BluRay);
        assert_eq!(res, Resolution::R1080p);
    }

    #[test]
    fn horriblesubs_is_unclassifiable_by_layer1() {
        // HorribleSubs releases are WEB but the title carries no source
        // token. Layer 1 returns no evidence — Layer 3 picks this up.
        let fc = classify_filename("[HorribleSubs] Show - 01 [720p].mkv");
        assert_eq!(fc.resolution, Resolution::R720p);
        assert!(fc.evidence.is_empty());
    }

    #[test]
    fn empty_title_produces_empty_classification() {
        let fc = classify_filename("");
        assert!(fc.evidence.is_empty());
        assert_eq!(fc.resolution, Resolution::Unknown);
        assert!(fc.release_group.is_none());
    }

    #[test]
    fn resolution_from_raw_dimensions() {
        let fc = classify_filename("[Beatrice-Raws] Show 01 [BDRip 1920x1080 HEVC FLAC].mkv");
        assert_eq!(fc.resolution, Resolution::R1080p);
    }

    #[test]
    fn dvd_dimensions_classify_as_480p() {
        // 720x480 is NTSC DVD.
        let fc = classify_filename("[Group] Old Show 01 [DVD 720x480 AC3].mkv");
        assert_eq!(fc.resolution, Resolution::R480p);
    }

    #[test]
    fn aac_alone_gives_web_signal() {
        // AAC alone contributes a single Web signal at 0.75 confidence —
        // above MIN_TOTAL so the aggregator accepts it, but below
        // STRONG_THRESHOLD so a stronger post-download signal (e.g. FLAC
        // from ffprobe) can still override it. Aggregator result should be
        // Web with a clean lead (no runner-up).
        let fc = classify_filename("[Group] Show 01 [720p AAC].mkv");
        assert_eq!(fc.resolution, Resolution::R720p);
        let result = aggregate(&fc.evidence);
        assert_eq!(result.source, Source::Web);
        assert!(!result.needs_review);
    }

    #[test]
    fn release_group_extracted() {
        let fc = classify_filename("[Beatrice-Raws] Made in Abyss 01 [BDRip 1080p HEVC FLAC].mkv");
        assert_eq!(fc.release_group.as_deref(), Some("Beatrice-Raws"));
    }

    #[test]
    fn platform_tag_boundary_rejects_false_positives() {
        // "wolf" contains "nf" but must not match the NF platform tag.
        assert!(!contains_word("wolf children", "nf"));
        assert!(contains_word("[nf].web-dl", "nf"));
        // "CR" should not match the middle of "MKVCRUSH" etc.
        assert!(!contains_word("mkvcrush", "cr"));
        assert!(contains_word("[cr]", "cr"));
    }

    #[test]
    fn contains_word_case_insensitive() {
        assert!(contains_word("[AMZN].WEB-DL", "amzn"));
        assert!(contains_word("[amzn].web-dl", "amzn"));
    }

    // ── Aggregator integration with Layer 1 ──────────────────────────────

    #[test]
    fn conflict_between_bdrip_and_aac_resolves_to_bluray() {
        // BDRip keyword (0.95) should dominate the weaker AAC signal (0.75).
        // 0.95 ≥ STRONG_THRESHOLD, rule 1 short-circuits to BluRay.
        let (src, _, _) = classify("[Group] Show 01 [BDRip 1080p HEVC AAC].mkv");
        assert_eq!(src, Source::BluRay);
    }

    #[test]
    fn two_conflicting_strong_signals_flag_for_review() {
        // Contrived: both WEB-DL and BD keywords in the same title. In
        // practice this would be a misnamed release. Aggregator should still
        // pick one but the ClassificationResult's needs_review flag is set.
        let fc = classify_filename("[Group] Show - 01 [WEB-DL 1080p] [BDRip Re-encode] [FLAC].mkv");
        // Both signals are present in the evidence trail.
        let has_web = fc.evidence.iter().any(|e| e.source == Source::Web);
        let has_bd = fc.evidence.iter().any(|e| e.source == Source::BluRay);
        assert!(
            has_web && has_bd,
            "expected both Web and BluRay evidence, got {:?}",
            fc.evidence
        );
    }

    // ── Sonarr-parity sub-classification ─────────────────────────────────

    #[test]
    fn web_dl_filename_sets_webdl_kind() {
        let fc = classify_filename("Sousou.no.Frieren.S01E01.1080p.WEB-DL.DDP5.1.H.264-NTb.mkv");
        assert_eq!(fc.web_kind, WebKind::WebDl);
        assert!(!fc.is_bdmv);
    }

    #[test]
    fn webrip_filename_sets_webrip_kind() {
        let fc = classify_filename("Show.Name.S01E01.1080p.WEBRip.x264-XYZ.mkv");
        assert_eq!(fc.web_kind, WebKind::WebRip);
        assert!(!fc.is_bdmv);
    }

    #[test]
    fn bare_web_keyword_leaves_kind_unknown() {
        // A title that just says "WEB" without a -DL/-Rip qualifier should
        // not commit to either variant. The Source::Web evidence still
        // fires, but web_kind stays Unknown.
        let fc = classify_filename("Show.Name.S01E01.1080p.WEB.x264-XYZ.mkv");
        assert_eq!(fc.web_kind, WebKind::Unknown);
    }

    #[test]
    fn bdmv_keyword_sets_is_bdmv_and_emits_bluray() {
        let fc = classify_filename("[Group] Sousou no Frieren Vol.1 BDMV [Bluray ISO 1080p]");
        assert!(fc.is_bdmv, "BDMV token should set is_bdmv");
        // The dedicated BDMV emitter should fire so the aggregator commits
        // to BluRay even on a barebones disc dump.
        let result = aggregate(&fc.evidence);
        assert_eq!(result.source, Source::BluRay);
    }

    #[test]
    fn bdraw_token_sets_is_bdmv() {
        let fc = classify_filename("[Group] Sousou no Frieren Vol.1 [BD-RAW 1080p]");
        assert!(fc.is_bdmv);
    }

    #[test]
    fn remux_does_not_set_is_bdmv() {
        // Remux and BDMV are distinct release classes — a Remux is an
        // MKV-wrapped extract, not the disc structure itself. Make sure
        // the Remux detector doesn't accidentally trip the BDMV flag.
        let fc =
            classify_filename("Sousou.no.Frieren.S01.1080p.BluRay.REMUX.AVC.DTS-HD.MA.5.1-FGT.mkv");
        assert!(fc.is_remux);
        assert!(!fc.is_bdmv);
    }

    #[test]
    fn bdmv_does_not_set_is_remux() {
        let fc = classify_filename("[Group] Sousou no Frieren Vol.1 BDMV [1080p]");
        assert!(fc.is_bdmv);
        assert!(!fc.is_remux);
    }
}
