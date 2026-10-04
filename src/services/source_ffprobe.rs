//! Layer 5 — ffprobe container / stream analysis.
//!
//! The strongest single post-download signal. Shells out to `ffprobe` with
//! `-show_streams -show_format -show_chapters -of json`, then walks the
//! parsed JSON looking for fingerprints that only appear in one source
//! type. A file with FLAC audio and PGS subtitles is, in practice,
//! always a BluRay rip; a file with AAC as its only audio track is
//! overwhelmingly a Web release; native DVD dimensions (720×480, 704×480,
//! 720×576) are diagnostic of a DVDRip.
//!
//! | Signal                                                  | Confidence |
//! |---------------------------------------------------------|------------|
//! | FLAC / TrueHD / DTS-HD MA / PCM audio                   | 0.90       |
//! | PGS (S_HDMV/PGS) subtitle track                         | 0.90       |
//! | Native DVD dimensions (720×480 / 704×480 / 720×576)     | 0.90       |
//! | Commentary audio track (detected via stream title)      | 0.90       |
//! | HEVC + FLAC + 10-bit fingerprint                        | 0.90       |
//! | AAC as the sole audio codec                             | 0.85       |
//! | H.264 + AAC + 8-bit fingerprint                         | 0.85       |
//! | E-AC-3 / DD+ audio                                      | 0.80       |
//!
//! **Intentionally useless signals** (no evidence produced): Opus, AV1,
//! x265/HEVC alone, AC-3 alone, stream track count. These appear in
//! both BluRay and Web releases and would only add noise.
//!
//! The module exposes a pure `scan_ffprobe_json` for unit testability and
//! an async `classify_ffprobe` wrapper that owns the ffprobe shell-out
//! plus `(path, mtime, size)`-keyed caching. Missing `ffprobe` binary,
//! malformed JSON, or probe failures return an empty evidence vec — the
//! aggregator will simply fall back on the other layers.
//!
//! This module does NOT fold evidence into a final decision. It emits
//! a bag of [`SourceEvidence`] plus an observed [`Resolution`] (if any)
//! for the caller to hand to [`crate::services::source::aggregate`].

use std::path::Path;
use std::process::Stdio;
use std::time::SystemTime;

use serde_json::Value;
use sqlx::SqlitePool;
use tokio::process::Command;

use crate::models::media_probe_cache;
use crate::services::source::{Origin, Resolution, Source, SourceEvidence};

const ORIGIN: Origin = Origin::Ffprobe;

/// Public output of Layer 5.
#[derive(Debug, Clone, Default)]
pub struct FfprobeClassification {
    /// Zero or more pieces of source evidence extracted from the probe JSON.
    pub evidence: Vec<SourceEvidence>,
    /// Observed display resolution if the probe output included a video
    /// stream with usable dimensions. Takes precedence over filename-parsed
    /// resolution at the orchestrator level since it's a direct observation.
    pub resolution: Option<Resolution>,
}

/// Run ffprobe against `path` (or return a cached result), parse the JSON,
/// and emit Layer 5 evidence. Returns an empty classification on any error —
/// missing binary, missing file, cache miss that then fails to spawn, probe
/// timeout, malformed JSON — so the caller can always safely aggregate the
/// result without null-checking.
///
/// Failure branches emit `tracing::warn!` with the offending path and the
/// underlying error, so a silently missing-ffprobe install or a corrupt
/// file is visible in the logs. Successful zero-evidence results (the
/// "intentionally useless" fingerprints documented above) are not warned
/// about — they're the happy path for audio-only or AV1/Opus files.
pub async fn classify_ffprobe(db: &SqlitePool, path: &Path) -> FfprobeClassification {
    let Some(path_str) = path.to_str() else {
        tracing::warn!(
            target: "ryokan::source::ffprobe",
            ?path,
            "ffprobe: path contains non-UTF8 bytes; skipping probe"
        );
        return FfprobeClassification::default();
    };

    // Snapshot mtime + size to key the cache. An rmdir-then-recreate (same
    // path, new file) invalidates on either mtime or size. If stat fails,
    // treat it as "can't probe" rather than blindly going to the network.
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = path_str,
                %err,
                "ffprobe: stat failed; skipping probe"
            );
            return FfprobeClassification::default();
        }
    };
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let cached = media_probe_cache::get(db, path_str, mtime, size).await;
    let cache_hit = cached.is_some();
    let probe_json = match cached {
        Some(j) => j,
        None => match run_ffprobe(path).await {
            Some(j) => {
                media_probe_cache::upsert(db, path_str, mtime, size, &j).await;
                j
            }
            None => return FfprobeClassification::default(),
        },
    };
    tracing::debug!(
        target: "ryokan::source::ffprobe",
        path = path_str,
        cache_hit,
        size,
        mtime,
        "ffprobe probe ready"
    );

    scan_ffprobe_json_logged(path_str, &probe_json)
}

/// Largest ffprobe stdout Ryokan keeps. A real episode's probe (streams,
/// format, chapters) is 25-70 KB; a crafted 13 MB mkv with 50k chapters
/// and an 8 MB tag printed 25 MB, all of which used to be buffered,
/// parsed into a `Value`, and cached. A probe over the cap is dropped.
const FFPROBE_STDOUT_CAP: usize = 4 << 20;
/// stderr only feeds the non-zero-exit warn line.
const FFPROBE_STDERR_CAP: usize = 64 << 10;

/// Spawn `ffprobe` and capture its JSON output. Returns `None` on any
/// failure including a missing binary. We pass `-v error` to suppress
/// ffprobe's banner so cache hits compare byte-for-byte.
///
/// Each failure branch emits a targeted `warn!` so the user can see
/// *why* ffprobe didn't contribute evidence: missing binary vs. probe
/// refusal vs. oversized or non-UTF8 output are distinct failure modes
/// with different remediation.
async fn run_ffprobe(path: &Path) -> Option<String> {
    // Capture stderr so the non-zero-exit branch can surface the reason.
    // kill_on_drop ensures the child is reaped if the surrounding
    // timeout fires (and would otherwise leave an orphan ffprobe
    // chewing CPU forever on a corrupt container).
    let spawned = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-show_streams")
        .arg("-show_format")
        .arg("-show_chapters")
        .arg("-of")
        .arg("json")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = %path.display(),
                %err,
                "ffprobe spawn failed — is the `ffprobe` binary installed and on PATH?"
            );
            return None;
        }
    };
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let probe = async {
        let (out, err) = tokio::join!(
            read_capped(stdout, FFPROBE_STDOUT_CAP),
            read_capped(stderr, FFPROBE_STDERR_CAP),
        );
        (out, err, child.wait().await)
    };

    // Cap ffprobe at 60s. A partially-downloaded mkv with a corrupt
    // container or a Blu-ray .m2ts with an inconsistent index will spin
    // ffprobe forever, and the post-processing pass holds POST_PROC_LOCK
    // for the duration — one bad file otherwise pins the entire import
    // queue. ffprobe finishes in milliseconds for healthy files; 60s is
    // generous enough that any non-pathological input completes well
    // under the cap.
    const FFPROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let (stdout, stderr, status) = match tokio::time::timeout(FFPROBE_TIMEOUT, probe).await {
        Ok((Ok(out), Ok(err), Ok(status))) => (out, err, status),
        Ok((out, err, status)) => {
            let err = [out.err(), err.err(), status.err()]
                .into_iter()
                .flatten()
                .next()
                .map(|e| e.to_string())
                .unwrap_or_default();
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = %path.display(),
                %err,
                "ffprobe output read failed"
            );
            return None;
        }
        Err(_) => {
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = %path.display(),
                timeout_secs = FFPROBE_TIMEOUT.as_secs(),
                "ffprobe timed out — file likely has a corrupt container or partial download"
            );
            return None;
        }
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr.kept);
        tracing::warn!(
            target: "ryokan::source::ffprobe",
            path = %path.display(),
            code = ?status.code(),
            stderr = stderr.trim(),
            "ffprobe exited non-zero"
        );
        return None;
    }
    if stdout.truncated {
        tracing::warn!(
            target: "ryokan::source::ffprobe",
            path = %path.display(),
            cap_mb = FFPROBE_STDOUT_CAP >> 20,
            "ffprobe output exceeded the size cap; skipping probe"
        );
        return None;
    }
    match String::from_utf8(stdout.kept) {
        Ok(s) => Some(s),
        Err(err) => {
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = %path.display(),
                %err,
                "ffprobe stdout was not valid UTF-8"
            );
            None
        }
    }
}

/// What [`read_capped`] kept of a pipe.
struct CappedRead {
    kept: Vec<u8>,
    /// More than `cap` bytes arrived; `kept` holds the first `cap`.
    truncated: bool,
}

/// Read `pipe` to EOF keeping at most `cap` bytes. It drains past the
/// cap rather than stopping there: a full pipe would block ffprobe
/// until the timeout. A missing pipe reads as empty.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    cap: usize,
) -> std::io::Result<CappedRead> {
    use tokio::io::AsyncReadExt;

    let mut out = CappedRead {
        kept: Vec::new(),
        truncated: false,
    };
    let Some(mut pipe) = pipe else {
        return Ok(out);
    };
    let mut chunk = vec![0_u8; 64 << 10];
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            return Ok(out);
        }
        let room = cap.saturating_sub(out.kept.len());
        out.truncated |= n > room;
        out.kept.extend_from_slice(&chunk[..n.min(room)]);
    }
}

/// Thin wrapper around `scan_ffprobe_json` that adds path-aware warn
/// logging on JSON parse failures and on the empty-video-stream branch.
/// The pure scanner is kept free of I/O and logging so unit tests can
/// keep calling it directly with canned fixtures.
fn scan_ffprobe_json_logged(path: &str, json: &str) -> FfprobeClassification {
    if serde_json::from_str::<Value>(json).is_err() {
        tracing::warn!(
            target: "ryokan::source::ffprobe",
            path,
            "ffprobe JSON parse failed; skipping probe"
        );
        return FfprobeClassification::default();
    }
    let (facts, out) = scan_ffprobe_json_with_facts(json);
    log_ffprobe_summary(path, &facts, &out);
    if out.evidence.is_empty() && out.resolution.is_none() {
        // No video stream at all — log at debug since this is normal for
        // audio-only files or probes of weird container formats, but
        // still worth tracing when debugging.
        tracing::debug!(
            target: "ryokan::source::ffprobe",
            path,
            "ffprobe: no video stream present, no evidence emitted"
        );
    }
    out
}

/// Emit a one-line debug summary of every ffprobe pass. Captures the
/// observed dimensions / resolution, video + audio codec fingerprints,
/// subtitle / commentary flags, and the evidence vec the rules
/// produced. Without this the only signal a probe ran was whatever
/// pieces of evidence happened to bubble up into `log_classification`
/// in `services/source/mod.rs` — a probe that emitted **zero** evidence
/// (the "intentionally useless" path for AV1/Opus, or any file that
/// didn't fingerprint a single rule) was completely silent, and the
/// observed resolution was never logged anywhere even when it was the
/// signal that overrode the filename layer.
fn log_ffprobe_summary(path: &str, facts: &ProbeFacts, out: &FfprobeClassification) {
    if !facts.has_video {
        return;
    }
    let resolution_label = out
        .resolution
        .map(|r| r.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let audio_codecs = if facts.audio_codecs.is_empty() {
        "(none)".to_string()
    } else {
        facts.audio_codecs.join(",")
    };
    let evidence_summary = if out.evidence.is_empty() {
        "(none)".to_string()
    } else {
        out.evidence
            .iter()
            .map(|e| format!("{}:{:.2} \"{}\"", e.source.as_str(), e.confidence, e.detail))
            .collect::<Vec<_>>()
            .join(", ")
    };
    tracing::debug!(
        target: "ryokan::source::ffprobe",
        path,
        width = facts.width,
        height = facts.height,
        resolution = %resolution_label,
        video_codec = %facts.video_codec,
        bit_depth = facts.bit_depth,
        audio_codecs = %audio_codecs,
        has_pgs_subs = facts.has_pgs_subs,
        has_commentary = facts.has_commentary,
        evidence = %evidence_summary,
        "ffprobe scan"
    );
}

/// Pure scanner: takes a ffprobe JSON document as a string and emits
/// classification evidence. Kept free of I/O and fs access so the unit
/// tests can feed in canned fixtures with no shell-out.
///
/// Production code now routes through [`scan_ffprobe_json_with_facts`]
/// so the logged wrapper can surface the intermediate facts bag; this
/// thin wrapper stays public for the existing test suite.
#[cfg_attr(not(test), allow(dead_code))]
pub fn scan_ffprobe_json(json: &str) -> FfprobeClassification {
    scan_ffprobe_json_with_facts(json).1
}

/// Internal variant that also returns the intermediate [`ProbeFacts`]
/// bag. Used by `scan_ffprobe_json_logged` to emit a structured debug
/// summary of every probe — the observed dimensions, codecs, and
/// fingerprint flags — alongside the final evidence vec. Tests stick
/// with the thinner [`scan_ffprobe_json`] wrapper so they don't have
/// to spell out facts that aren't relevant to whatever rule they're
/// exercising.
fn scan_ffprobe_json_with_facts(json: &str) -> (ProbeFacts, FfprobeClassification) {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return (ProbeFacts::default(), FfprobeClassification::default());
    };
    let Some(streams) = root.get("streams").and_then(|s| s.as_array()) else {
        return (ProbeFacts::default(), FfprobeClassification::default());
    };

    // First pass: collect the facts we need for the rule evaluation.
    let mut facts = ProbeFacts::default();
    for s in streams {
        let codec_type = s
            .get("codec_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let codec_name = s
            .get("codec_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        match codec_type.as_str() {
            "video" => {
                // Skip attached-picture streams (cover art, album thumbnails,
                // poster frames). ffprobe labels them `codec_type = "video"`
                // but their dimensions are the cover image (often small and
                // portrait — 600x900 posters, 300x400 thumbnails) rather
                // than the actual video track. Without this guard they'd
                // overwrite the real video stream's width/height when they
                // sit later in the streams array, which is how an EMBER
                // Wajutsushi release came back with the 1080p main track
                // silently replaced by a ~150-tall thumbnail — the height
                // dropped below the `from_dimensions` 460-pixel floor so
                // every affected episode rendered with no resolution tag
                // at all. Check `disposition.attached_pic` (ffprobe sets it
                // to 1 for these streams) and treat it as "not a real video
                // stream" — skip it entirely.
                let attached_pic = s
                    .get("disposition")
                    .and_then(|d| d.get("attached_pic"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    != 0;
                if attached_pic {
                    continue;
                }
                facts.has_video = true;
                facts.video_codec = codec_name.clone();
                facts.width = s.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                facts.height = s.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                // Bit depth comes from pix_fmt (yuv420p10le → 10-bit, etc.)
                // or bits_per_raw_sample when present.
                let pix_fmt = s
                    .get("pix_fmt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if pix_fmt.contains("10") {
                    facts.bit_depth = 10;
                } else if pix_fmt.contains("12") {
                    facts.bit_depth = 12;
                } else if !pix_fmt.is_empty() {
                    facts.bit_depth = 8;
                }
                if let Some(bps) = s.get("bits_per_raw_sample").and_then(|v| v.as_str())
                    && let Ok(n) = bps.parse::<u8>()
                    && n > 0
                {
                    facts.bit_depth = n;
                }
            }
            "audio" => {
                facts.audio_codecs.push(codec_name.clone());
                // Detect commentary tracks via stream title metadata. Title
                // typically lives under tags.title.
                let title = s
                    .get("tags")
                    .and_then(|t| t.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !title.is_empty()
                    && (title.contains("commentary")
                        || title.contains("audio commentary")
                        || title.contains("director"))
                {
                    facts.has_commentary = true;
                }
            }
            // PGS (Blu-ray) subtitles come through as "hdmv_pgs_subtitle"
            // under codec_name. Also accept the older S_HDMV/PGS form that
            // shows up in some mkvtoolnix-produced files.
            "subtitle" if codec_name.contains("pgs") || codec_name.contains("hdmv_pgs") => {
                facts.has_pgs_subs = true;
            }
            _ => {}
        }
    }

    let mut out = FfprobeClassification::default();
    if !facts.has_video {
        return (facts, out);
    }

    // Observed resolution — always set if we have a video stream with
    // readable dimensions, even when no source evidence fires.
    if facts.width > 0 && facts.height > 0 {
        let res = Resolution::from_dimensions(facts.width, facts.height);
        if res != Resolution::Unknown {
            out.resolution = Some(res);
        }
    }

    // Rule: DVD-native dimensions. 720×480, 704×480 (NTSC), 720×576 (PAL).
    // This is diagnostic regardless of codec — any file with these exact
    // dimensions is a DVD rip.
    if matches!(
        (facts.width, facts.height),
        (720, 480) | (704, 480) | (720, 576)
    ) {
        out.evidence.push(SourceEvidence::new(
            Source::Dvd,
            0.90,
            ORIGIN,
            format!("native DVD dimensions {}x{}", facts.width, facts.height),
        ));
    }

    // Rule: PGS subtitles → BluRay. PGS is the bitmap subtitle format used
    // on retail Blu-ray discs. Web releases never ship PGS.
    if facts.has_pgs_subs {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "PGS subtitle track present",
        ));
    }

    // Rule: commentary track → BluRay. Only retail BD releases ship
    // director / cast commentary tracks.
    if facts.has_commentary {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "commentary audio track",
        ));
    }

    // Rule: high-fidelity audio codecs → BluRay. FLAC, TrueHD, DTS-HD MA,
    // and PCM are the four lossless / master-audio formats that streaming
    // services don't ship.
    let has_flac = facts.audio_codecs.iter().any(|c| c == "flac");
    let has_truehd = facts.audio_codecs.iter().any(|c| c == "truehd");
    // ffprobe reports DTS-HD MA as `dts_hd_ma` and DTS-HD HRA as
    // `dts_hd_hra`. Match those codec IDs explicitly rather than doing a
    // loose "contains dts && contains hd" substring scan, which could
    // false-positive on e.g. a hypothetical "dts_hdcam" codec and misses
    // the point of being a strict BD-exclusive signal.
    let has_dts_hd = facts
        .audio_codecs
        .iter()
        .any(|c| c == "dts_hd_ma" || c == "dts_hd_hra");
    // PCM on ffprobe comes through as e.g. "pcm_s16le" / "pcm_s24le".
    let has_pcm = facts.audio_codecs.iter().any(|c| c.starts_with("pcm_"));
    if has_flac || has_truehd || has_dts_hd || has_pcm {
        let codec = if has_flac {
            "FLAC"
        } else if has_truehd {
            "TrueHD"
        } else if has_dts_hd {
            "DTS-HD MA"
        } else {
            "PCM"
        };
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            format!("{} audio codec", codec),
        ));
    }

    // Rule: E-AC-3 / DD+ → Web. Streaming services (Amazon, Netflix,
    // Disney+) have standardized on DDP for their HD releases. Not
    // diagnostic on its own — but good enough to lean Web.
    let has_ddp = facts
        .audio_codecs
        .iter()
        .any(|c| c == "eac3" || c == "ec-3" || c == "e-ac-3");
    if has_ddp {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.80,
            ORIGIN,
            "E-AC-3 / DD+ audio codec",
        ));
    }

    // Rule: AAC as the sole audio codec → Web. AAC is uncommon on BD
    // releases — BDs ship lossless. An AAC-only file is almost always a
    // streaming rip. The check is "contains aac and no high-fidelity
    // codec" rather than "only aac" because the streamable-set vs.
    // BluRay distinction is the whole point.
    let has_aac = facts.audio_codecs.iter().any(|c| c == "aac");
    if has_aac && !has_flac && !has_truehd && !has_dts_hd && !has_pcm {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.85,
            ORIGIN,
            "AAC audio without lossless track",
        ));
    }

    // Combo fingerprint: H.264 + AAC + 8-bit → Web encode. Catches the
    // typical streaming profile so files that would otherwise pull only
    // the weaker AAC rule get a dedicated high-confidence signal.
    let is_h264 = facts.video_codec == "h264" || facts.video_codec == "avc1";
    if is_h264 && has_aac && facts.bit_depth == 8 && !has_flac && !has_truehd && !has_dts_hd {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.85,
            ORIGIN,
            "H.264 + AAC + 8-bit streaming fingerprint",
        ));
    }

    // Combo fingerprint: HEVC + FLAC + 10-bit → BluRay encode. This is
    // the signature of community BD re-encodes (VCB, Beatrice-Raws, etc.).
    let is_hevc = facts.video_codec == "hevc" || facts.video_codec == "h265";
    if is_hevc && has_flac && facts.bit_depth == 10 {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "HEVC + FLAC + 10-bit BD encode fingerprint",
        ));
    }

    (facts, out)
}

#[derive(Default)]
struct ProbeFacts {
    has_video: bool,
    video_codec: String,
    width: u32,
    height: u32,
    bit_depth: u8,
    audio_codecs: Vec<String>,
    has_pgs_subs: bool,
    has_commentary: bool,
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal ffprobe-shaped JSON builder for tests. Composes a root with
    /// a single video stream and an arbitrary list of audio/subtitle streams.
    fn probe_json(streams: Vec<Value>) -> String {
        serde_json::to_string(&serde_json::json!({
            "streams": streams,
            "format": {},
        }))
        .unwrap()
    }

    fn video_stream(codec: &str, width: u32, height: u32, pix_fmt: &str) -> Value {
        serde_json::json!({
            "codec_type": "video",
            "codec_name": codec,
            "width": width,
            "height": height,
            "pix_fmt": pix_fmt,
        })
    }

    fn audio_stream(codec: &str) -> Value {
        serde_json::json!({
            "codec_type": "audio",
            "codec_name": codec,
        })
    }

    /// Cover-art / attached-picture stream — what ffprobe emits for the
    /// embedded poster/thumbnail many MKV releases carry. ffprobe tags
    /// these with `codec_type = "video"` but marks them via
    /// `disposition.attached_pic = 1`. Used to pin down the attached-pic
    /// regression where these streams overwrote the real video track.
    fn attached_pic_stream(codec: &str, width: u32, height: u32) -> Value {
        serde_json::json!({
            "codec_type": "video",
            "codec_name": codec,
            "width": width,
            "height": height,
            "disposition": { "attached_pic": 1 },
        })
    }

    fn audio_stream_with_title(codec: &str, title: &str) -> Value {
        serde_json::json!({
            "codec_type": "audio",
            "codec_name": codec,
            "tags": { "title": title },
        })
    }

    fn subtitle_stream(codec: &str) -> Value {
        serde_json::json!({
            "codec_type": "subtitle",
            "codec_name": codec,
        })
    }

    #[test]
    fn malformed_json_is_empty() {
        let out = scan_ffprobe_json("not json");
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }

    #[test]
    fn empty_streams_is_empty() {
        let out = scan_ffprobe_json(&probe_json(vec![]));
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }

    #[test]
    fn video_only_sets_resolution() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "h264", 1920, 1080, "yuv420p",
        )]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    /// Attached-picture streams (MKV cover art) must not clobber the main
    /// video's width/height. Regression guard for an EMBER Wajutsushi
    /// release where a ~150-tall thumbnail sat after the real 1080p
    /// track and dropped the reported height below
    /// `Resolution::from_dimensions`'s 460-pixel floor — classification
    /// for 11 of 12 episodes came back with no resolution tag at all.
    #[test]
    fn attached_pic_after_main_video_does_not_overwrite_resolution() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            attached_pic_stream("mjpeg", 500, 150),
            audio_stream("eac3"),
        ]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    /// Same bug, reversed stream order — attached_pic first (as some
    /// containers emit), then the real video. Without the disposition
    /// check the real video's dims would overwrite the thumbnail's and
    /// the test would coincidentally pass, so we keep both orderings.
    #[test]
    fn attached_pic_before_main_video_does_not_overwrite_resolution() {
        let out = scan_ffprobe_json(&probe_json(vec![
            attached_pic_stream("mjpeg", 500, 150),
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("eac3"),
        ]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    #[test]
    fn dvd_dimensions_fire_dvd_rule() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "mpeg2video",
            720,
            480,
            "yuv420p",
        )]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Dvd));
        assert_eq!(out.resolution, Some(Resolution::R480p));
    }

    #[test]
    fn pal_dvd_dimensions_also_fire() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "mpeg2video",
            720,
            576,
            "yuv420p",
        )]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Dvd));
    }

    #[test]
    fn flac_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("flac"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn hevc_flac_10bit_combo_fires_bd_encode_rule() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("flac"),
        ]));
        // Should have both the FLAC rule AND the combo-fingerprint rule.
        let bd_count = out
            .evidence
            .iter()
            .filter(|e| e.source == Source::BluRay)
            .count();
        assert!(bd_count >= 2);
    }

    #[test]
    fn truehd_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("truehd"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn dts_hd_ma_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("dts"), // fake — dts_hd_ma shows up as "dts" with a profile
        ]));
        // Plain DTS doesn't fire — we need "dts" + "hd" in the name.
        assert!(
            !out.evidence
                .iter()
                .any(|e| { e.source == Source::BluRay && e.detail.contains("DTS") })
        );
        let out2 = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("dts_hd_ma"),
        ]));
        assert!(out2.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn pcm_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("mpeg2video", 1920, 1080, "yuv420p"),
            audio_stream("pcm_s16le"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn aac_only_is_web() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("aac"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Web));
    }

    #[test]
    fn aac_with_flac_is_not_web() {
        // Dual audio BDs sometimes carry both an AAC downmix and a FLAC
        // master. FLAC wins.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("aac"),
            audio_stream("flac"),
        ]));
        assert!(!out.evidence.iter().any(|e| e.source == Source::Web));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn h264_aac_8bit_fires_streaming_fingerprint() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("aac"),
        ]));
        // AAC-only rule + combo fingerprint = at least 2 Web hits.
        let web_count = out
            .evidence
            .iter()
            .filter(|e| e.source == Source::Web)
            .count();
        assert!(web_count >= 2);
    }

    #[test]
    fn eac3_is_web() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("eac3"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Web));
    }

    #[test]
    fn pgs_subtitles_are_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            subtitle_stream("hdmv_pgs_subtitle"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn commentary_track_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream_with_title("ac3", "Director Commentary"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn opus_audio_produces_no_evidence() {
        // Opus is explicitly on the "useless signal" list.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("av1", 1920, 1080, "yuv420p"),
            audio_stream("opus"),
        ]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn bare_ac3_produces_no_evidence() {
        // AC-3 alone is too ambiguous (both BD and Web ship it for
        // backwards-compatible tracks).
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("ac3"),
        ]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn hevc_alone_produces_no_evidence() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "hevc", 1920, 1080, "yuv420p",
        )]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn resolution_set_even_when_no_source_evidence() {
        // Useless-signal file — we should still know it's 1080p.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("av1", 1920, 1080, "yuv420p"),
            audio_stream("opus"),
        ]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    #[test]
    fn missing_video_stream_emits_nothing() {
        let out = scan_ffprobe_json(&probe_json(vec![audio_stream("flac")]));
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }

    #[tokio::test]
    async fn read_capped_keeps_a_pipe_under_the_cap_whole() {
        let out = read_capped(Some(&b"{\"streams\":[]}"[..]), 64)
            .await
            .unwrap();
        assert_eq!(out.kept, b"{\"streams\":[]}");
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn read_capped_at_exactly_the_cap_is_not_truncated() {
        let out = read_capped(Some(&[7_u8; 10][..]), 10).await.unwrap();
        assert_eq!(out.kept.len(), 10);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn read_capped_keeps_the_prefix_and_drains_the_rest() {
        // Larger than one 64 KiB read, so the cap lands mid-stream and
        // later reads still have to be drained.
        let body: Vec<u8> = (0..200_000_u32).map(|i| (i % 251) as u8).collect();
        let mut reader = &body[..];
        let out = read_capped(Some(&mut reader), 100_000).await.unwrap();
        assert_eq!(out.kept, &body[..100_000]);
        assert!(out.truncated);
        assert!(reader.is_empty(), "the pipe was drained to EOF");
    }

    #[tokio::test]
    async fn read_capped_reads_a_missing_pipe_as_empty() {
        let out = read_capped(None::<&[u8]>, 10).await.unwrap();
        assert!(out.kept.is_empty());
        assert!(!out.truncated);
    }
}
