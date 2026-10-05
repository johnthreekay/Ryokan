//! SeaDex (releases.moe) lookup client.
//!
//! SeaDex is a community-curated "best anime releases" database keyed by
//! AniList ID. This module handles the API round-trip, the usability
//! filter (Nyaa-hosted, muxed, curation-best), and the dual-audio /
//! file-count tiebreak that picks a single winner per entry.
//!
//! The output of [`pick_best`] is a single torrent record whose
//! `info_hash` can be fed to [`to_magnet_uri`] to synthesize a magnet
//! URI without any further HTTP round-trip — SeaDex already ships the
//! hash in the API response, so we skip scraping the Nyaa view page.
//!
//! Scope limits (per the integration plan, V1):
//! - AnimeBytes (private tracker) entries are filtered out — we have no
//!   credentials.
//! - AnimeTosho / RuTracker entries are filtered out — we don't support
//!   those hosts yet (six entries total, see plan §4.5).
//! - Unmuxed best releases are filtered out — the user would have to
//!   hand-mux sidecars in MPV. We'd rather fall through to Nyaa search
//!   than hand the user a broken download.
//! - Umbrella / franchise resolution (walking AniList relations) is V2.

// Phase 1+2 lands the module in isolation; later phases wire it into
// `auto_search`, the scoring pipeline, and the settings UI. Until those
// phases land, most of the public surface is referenced only by unit
// tests, which trips `-D warnings` dead-code errors in clippy. Scope the
// allow to this file so the warnings come back the moment a caller goes
// missing later.
#![allow(dead_code)]

use std::sync::LazyLock;
use std::time::Duration;

use crate::services::http_body::CappedBody;
use serde::Deserialize;

const SEADEX_API: &str = "https://releases.moe/api/collections/entries/records";

/// Score bump applied at scoring time when a candidate's info hash
/// matches a SeaDex "best" torrent for the series. Large enough to
/// reliably outrank an otherwise-tied release, small enough that a
/// preferred-group or resolution bonus can still move the needle.
/// Suppressed entirely when the user has a `SeaDexBestSpecification`
/// Custom Format installed (see `custom_formats::has_seadex_cf`).
pub const SEADEX_SCORE_BOOST: i32 = 10_000;

/// Process-global reqwest client for SeaDex. Same reasoning as the
/// nyaa.rs client: avoid re-doing the TLS handshake and keep-alive
/// pool on every lookup.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Ryokan/0.1")
        .timeout(Duration::from_secs(15))
        .build()
        .expect("building the SeaDex reqwest client should not fail")
});

// ───────────────────────────────────────────────────────────────────────────
// Public types
// ───────────────────────────────────────────────────────────────────────────

/// A SeaDex entry (one AniList ID → zero or more curated torrents).
#[derive(Debug, Clone)]
pub struct SeaDexEntry {
    pub anilist_id: i64,
    pub pocketbase_id: String,
    pub notes: String,
    pub incomplete: bool,
    pub torrents: Vec<SeaDexTorrent>,
}

/// A single torrent record from SeaDex. Fields are preserved verbatim
/// from the API — interpretation happens in [`is_usable`], [`pick_best`],
/// and friends.
#[derive(Debug, Clone)]
pub struct SeaDexTorrent {
    pub release_group: String,
    /// Tracker enum: `"Nyaa"`, `"AB"`, `"AnimeTosho"`, `"RuTracker"`.
    /// See plan §2. Ryokan V1 only acts on `"Nyaa"`.
    pub tracker: String,
    /// Full URL for public trackers. May be relative (`/torrents.php?...`)
    /// for AnimeBytes, or even a literal non-URL string (`"Chihiro"` at
    /// alID=151126). Defensive parsing in [`is_usable`].
    pub url: String,
    /// Lowercase 40-char hex hash for public trackers;
    /// `"<redacted>"` for AnimeBytes.
    pub info_hash: String,
    pub is_best: bool,
    pub dual_audio: bool,
    pub files: Vec<SeaDexFile>,
}

/// A single file inside a SeaDex torrent. Used by the unmuxed heuristic
/// in [`is_unmuxed`].
#[derive(Debug, Clone)]
pub struct SeaDexFile {
    pub length: i64,
    pub name: String,
}

// ───────────────────────────────────────────────────────────────────────────
// Internal JSON shape (PocketBase)
// ───────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PbListResponse {
    #[serde(default)]
    items: Vec<PbRecord>,
}

#[derive(Deserialize)]
struct PbRecord {
    #[serde(default, rename = "alID")]
    al_id: i64,
    #[serde(default)]
    id: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    incomplete: bool,
    #[serde(default)]
    expand: Option<PbExpand>,
}

#[derive(Deserialize)]
struct PbExpand {
    #[serde(default)]
    trs: Vec<PbTorrent>,
}

#[derive(Deserialize)]
struct PbTorrent {
    #[serde(default, rename = "releaseGroup")]
    release_group: String,
    #[serde(default)]
    tracker: String,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "infoHash")]
    info_hash: String,
    #[serde(default, rename = "isBest")]
    is_best: bool,
    #[serde(default, rename = "dualAudio")]
    dual_audio: bool,
    #[serde(default)]
    files: Vec<PbFile>,
}

#[derive(Deserialize)]
struct PbFile {
    #[serde(default)]
    length: i64,
    #[serde(default)]
    name: String,
}

impl From<PbRecord> for SeaDexEntry {
    fn from(r: PbRecord) -> Self {
        let torrents = r
            .expand
            .map(|e| e.trs.into_iter().map(Into::into).collect())
            .unwrap_or_default();
        SeaDexEntry {
            anilist_id: r.al_id,
            pocketbase_id: r.id,
            notes: r.notes,
            incomplete: r.incomplete,
            torrents,
        }
    }
}

impl From<PbTorrent> for SeaDexTorrent {
    fn from(t: PbTorrent) -> Self {
        SeaDexTorrent {
            release_group: t.release_group,
            tracker: t.tracker,
            url: t.url,
            info_hash: t.info_hash,
            is_best: t.is_best,
            dual_audio: t.dual_audio,
            files: t.files.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<PbFile> for SeaDexFile {
    fn from(f: PbFile) -> Self {
        SeaDexFile {
            length: f.length,
            name: f.name,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Lookup
// ───────────────────────────────────────────────────────────────────────────

/// Look up a SeaDex entry by AniList ID.
///
/// Returns:
/// - `Ok(Some(entry))` if SeaDex has a record for this AniList ID
/// - `Ok(None)` if SeaDex doesn't have a record (the PocketBase
///   `items` array came back empty — not an error)
/// - `Err(_)` on HTTP / parse failures
///
/// Caching lives one layer up in `auto_search` (in-memory + SQLite
/// persistence with in-flight coalescing); this function just owns the
/// network round-trip + parse.
pub async fn lookup(anilist_id: i64) -> Result<Option<SeaDexEntry>, String> {
    let url = format!("{}?filter=alID%3D{}&expand=trs", SEADEX_API, anilist_id,);

    let response = HTTP_CLIENT
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("SeaDex request failed: {e}"))?;

    // PocketBase returns 200 with an empty `items` array when the
    // record doesn't exist, not 404 — so any non-200 is a real error.
    if !response.status().is_success() {
        return Err(format!(
            "SeaDex API returned HTTP {}",
            response.status().as_u16()
        ));
    }

    let body = response
        .text_capped()
        .await
        .map_err(|e| format!("SeaDex read body failed: {e}"))?;

    parse_list_response(&body)
}

/// Maximum AniList ids to OR-batch into a single PocketBase filter
/// query. PocketBase itself accepts `perPage=500` (and 1000 on newer
/// versions), but the bottleneck here is URL length: each id costs
/// roughly `alID=1234567||` (≈14 chars), so 50 ids fits comfortably
/// inside any sane URL-length cap with overhead to spare. Bump cautiously.
///
/// Exposed `pub(crate)` so callers (e.g. the upgrade-sweep prewarm in
/// `auto_search`) can compute accurate chunk counts for logging without
/// inlining their own copy of the literal.
pub(crate) const SEADEX_BATCH_SIZE: usize = 50;

/// Look up multiple AniList ids in one PocketBase request using the
/// `||` (OR) filter operator. Returns a map keyed by every requested id:
/// hits map to `Some(entry)`, misses (no record on releases.moe) map
/// to `None`. Ids are chunked at [`SEADEX_BATCH_SIZE`] and chunks are
/// issued with bounded parallelism (`SEADEX_BATCH_PARALLELISM`-wide).
/// 2-wide is safe: releases.moe is fronted by Cloudflare which only
/// rate-limits at far higher concurrency than this, and we still keep
/// the round-trip count to `ceil(N / SEADEX_BATCH_SIZE)`. A chunk
/// failure aborts the whole call so the caller can decide whether to
/// fall back to per-id lookups.
pub async fn lookup_batch(
    anilist_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Option<SeaDexEntry>>, String> {
    let mut out: std::collections::HashMap<i64, Option<SeaDexEntry>> =
        std::collections::HashMap::with_capacity(anilist_ids.len());
    // Seed every requested id as a miss — chunk responses overwrite
    // the entry on hit. Without this seed, callers would have to
    // distinguish "id wasn't requested" from "id was requested but
    // SeaDex has no record."
    for id in anilist_ids {
        out.entry(*id).or_insert(None);
    }

    // Build all chunk URLs up front so the parallel runner is just
    // "fire each request, parse, fold." Owned ids per chunk keeps
    // them 'static for the spawn closure.
    let chunk_urls: Vec<String> = anilist_ids
        .chunks(SEADEX_BATCH_SIZE)
        .filter(|c| !c.is_empty())
        .map(|chunk| {
            // Build `(alID=A||alID=B||...)` and URL-encode just the operators.
            // `=` and digits are URL-safe; `||` becomes `%7C%7C`.
            let mut filter = String::with_capacity(chunk.len() * 16);
            for (i, id) in chunk.iter().enumerate() {
                if i > 0 {
                    filter.push_str("%7C%7C");
                }
                filter.push_str("alID%3D");
                filter.push_str(&id.to_string());
            }
            // perPage covers the whole chunk in one page so we never
            // paginate a single request.
            format!(
                "{}?filter={}&expand=trs&perPage={}",
                SEADEX_API,
                filter,
                chunk.len()
            )
        })
        .collect();

    // Run chunks with bounded concurrency via a semaphore. The
    // ordering of completion doesn't matter because we fold every
    // entry into the same output map keyed by anilist_id.
    use tokio::sync::Semaphore;
    let permits = std::sync::Arc::new(Semaphore::new(SEADEX_BATCH_PARALLELISM));
    let mut join_set: tokio::task::JoinSet<Result<Vec<SeaDexEntry>, String>> =
        tokio::task::JoinSet::new();
    for url in chunk_urls {
        let permits = permits.clone();
        join_set.spawn(async move {
            let _permit = permits
                .acquire()
                .await
                .map_err(|e| format!("SeaDex batch semaphore closed: {e}"))?;
            let response = HTTP_CLIENT
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("SeaDex batch request failed: {e}"))?;
            if !response.status().is_success() {
                return Err(format!(
                    "SeaDex batch API returned HTTP {}",
                    response.status().as_u16()
                ));
            }
            let body = response
                .text_capped()
                .await
                .map_err(|e| format!("SeaDex batch read body failed: {e}"))?;
            parse_list_response_multi(&body)
        });
    }
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(entries)) => {
                for entry in entries {
                    out.insert(entry.anilist_id, Some(entry));
                }
            }
            Ok(Err(e)) => {
                // Best-effort: cancel any in-flight chunks so we don't
                // keep hammering after the caller has already learned
                // the call failed.
                join_set.shutdown().await;
                return Err(e);
            }
            Err(join_err) => {
                join_set.shutdown().await;
                return Err(format!("SeaDex batch task join failed: {join_err}"));
            }
        }
    }

    Ok(out)
}

/// How many SeaDex batch chunks may be in flight at once. 2-wide
/// roughly halves wall time on libraries that need ≥ 2 chunks
/// (≥ 51 ids in this prewarm pass). Cloudflare's rate-limit floor
/// is far higher than 2 RPS so we don't risk a 429 here.
const SEADEX_BATCH_PARALLELISM: usize = 2;

/// Internal parser — split out so unit tests can feed it a fixture
/// without touching the network.
fn parse_list_response(body: &str) -> Result<Option<SeaDexEntry>, String> {
    let parsed: PbListResponse =
        serde_json::from_str(body).map_err(|e| format!("SeaDex parse failed: {e}"))?;

    Ok(parsed.items.into_iter().next().map(Into::into))
}

/// Multi-item variant of [`parse_list_response`] for the batched-OR
/// query. Returns every entry in the page rather than just the first.
fn parse_list_response_multi(body: &str) -> Result<Vec<SeaDexEntry>, String> {
    let parsed: PbListResponse =
        serde_json::from_str(body).map_err(|e| format!("SeaDex batch parse failed: {e}"))?;
    Ok(parsed.items.into_iter().map(Into::into).collect())
}

// ───────────────────────────────────────────────────────────────────────────
// Filters
// ───────────────────────────────────────────────────────────────────────────

/// Classify a filename by extension into video / audio-sidecar / sub-sidecar
/// buckets. Used by the file-structure heuristic in [`is_unmuxed`].
fn file_kind(name: &str) -> FileKind {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "mkv" | "mp4" | "m2ts" | "ts" | "avi" | "mov" | "webm" => FileKind::Video,
        "mka" | "flac" | "opus" | "aac" | "ac3" | "dts" | "wav" => FileKind::AudioSidecar,
        "ass" | "srt" | "ssa" | "sub" | "vtt" | "idx" | "pgs" | "sup" => FileKind::SubSidecar,
        _ => FileKind::Other,
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum FileKind {
    Video,
    AudioSidecar,
    SubSidecar,
    Other,
}

/// True if the torrent looks "unmuxed" — either the editor notes say so,
/// or the file listing has audio / sub sidecars stacked next to video
/// files (audio count ≥ video count, or sub count ≥ video count with no
/// audio at all).
///
/// Both signals are documented in plan §4.4 and are combined here.
pub fn is_unmuxed(torrent: &SeaDexTorrent, notes: &str) -> bool {
    // Signal 1: editor notes keyword. Case-insensitive substring is
    // sufficient — the known corpus uses lowercase `unmuxed` / `unmux`
    // / `needs mux` in running prose. Open question §10 of the plan
    // allows upgrading to word-boundary regex if we see a false
    // positive, but there isn't one today.
    let notes_lower = notes.to_ascii_lowercase();
    if notes_lower.contains("unmuxed")
        || notes_lower.contains("unmux")
        || notes_lower.contains("needs mux")
    {
        return true;
    }

    // Signal 2: file-structure heuristic. Count videos, audio
    // sidecars, and sub sidecars. A muxed release embeds audio+subs
    // inside the .mkv, so both sidecar counts should be zero.
    let mut video_count = 0i32;
    let mut audio_count = 0i32;
    let mut sub_count = 0i32;
    for f in &torrent.files {
        match file_kind(&f.name) {
            FileKind::Video => video_count += 1,
            FileKind::AudioSidecar => audio_count += 1,
            FileKind::SubSidecar => sub_count += 1,
            FileKind::Other => {}
        }
    }

    if video_count == 0 {
        return false;
    }

    // Audio-sidecar pattern: one .mka per .mkv (or more).
    if audio_count >= video_count {
        return true;
    }
    // Sub-sidecar-only pattern: a .ass per .mp4 with no audio sidecars.
    // (Muxed fansub releases have zero external subs.)
    if sub_count >= video_count && audio_count == 0 {
        return true;
    }

    false
}

/// True if the URL looks like a real nyaa.si view page. Guards against
/// the documented `url="Chihiro"` data-quality case and any future
/// weirdness where the tracker field says Nyaa but the URL doesn't
/// actually point there.
fn looks_like_nyaa_url(url: &str) -> bool {
    // A substring test passed `https://evil.example/?nyaa.si/view/1`, which
    // the Nyaa client then fetched.
    crate::services::nyaa::view_page_url(url).is_some()
}

/// Full usability gate. A torrent is usable by Ryokan iff:
/// - `is_best == true` (SeaDex curation flag — non-best is NOT a fallback)
/// - `tracker == "Nyaa"` (positive match, not `!= "AB"`, per plan §4.5)
/// - URL looks like a real nyaa.si page (defensive against the
///   `url="Chihiro"` case)
/// - `info_hash` is a plausible hash (non-empty, not `"<redacted>"`)
/// - [`is_unmuxed`] is false
pub fn is_usable(torrent: &SeaDexTorrent, notes: &str) -> bool {
    if !torrent.is_best {
        return false;
    }
    if torrent.tracker != "Nyaa" {
        return false;
    }
    if !looks_like_nyaa_url(&torrent.url) {
        return false;
    }
    if torrent.info_hash.is_empty() || torrent.info_hash.eq_ignore_ascii_case("<redacted>") {
        return false;
    }
    if is_unmuxed(torrent, notes) {
        return false;
    }
    true
}

// ───────────────────────────────────────────────────────────────────────────
// Selection
// ───────────────────────────────────────────────────────────────────────────

/// Pick the single best torrent from a SeaDex entry, applying the
/// full V1 selection pipeline (plan §5.1):
///
/// 1. Keep only [`is_usable`] torrents.
/// 2. If any have `dualAudio` diversity, filter by `prefer_subs`
///    (`true` → keep `dualAudio=false`, else keep `dualAudio=true`).
///    If the filter empties the pool, fall back to the pre-filter set
///    so we still return *something* usable.
/// 3. Tiebreak by file count descending (mega-packs win over patches /
///    partial-series batches per plan §4.2).
/// 4. Stable tiebreak by info_hash lexicographic so repeated lookups
///    return the same pick.
///
/// Returns `None` if the entry has no usable torrent — caller should
/// fall through to the regular Nyaa search.
/// Return every usable "best" info hash for the entry as a lowercase
/// set. Used by the auto-search scoring path to detect SeaDex candidates
/// without having to re-run [`pick_best`] — multiple torrents can carry
/// `isBest=true` (different group, different container, etc.), and the
/// boost should apply to all of them.
pub fn best_hashes(entry: &SeaDexEntry) -> std::collections::HashSet<String> {
    entry
        .torrents
        .iter()
        .filter(|t| is_usable(t, &entry.notes))
        .map(|t| t.info_hash.to_ascii_lowercase())
        .collect()
}

pub fn pick_best(entry: &SeaDexEntry, prefer_subs: bool) -> Option<&SeaDexTorrent> {
    let usable: Vec<&SeaDexTorrent> = entry
        .torrents
        .iter()
        .filter(|t| is_usable(t, &entry.notes))
        .collect();
    if usable.is_empty() {
        return None;
    }

    // Dual-audio filter — only kicks in when the pool has diversity.
    // If every usable torrent has the same dualAudio value, prefer_subs
    // is a no-op.
    let has_dub = usable.iter().any(|t| t.dual_audio);
    let has_sub = usable.iter().any(|t| !t.dual_audio);
    let filtered: Vec<&SeaDexTorrent> = if has_dub && has_sub {
        let want_dual = !prefer_subs;
        usable
            .iter()
            .copied()
            .filter(|t| t.dual_audio == want_dual)
            .collect()
    } else {
        usable.clone()
    };

    // Defensive fallback: if the dual-audio filter emptied the pool
    // (can't actually happen given the `has_dub && has_sub` guard, but
    // belt-and-suspenders for future refactors), use the un-filtered set.
    let pool: Vec<&SeaDexTorrent> = if filtered.is_empty() {
        usable
    } else {
        filtered
    };

    // File-count descending, then info_hash ascending for stability.
    pool.into_iter().max_by(|a, b| {
        a.files
            .len()
            .cmp(&b.files.len())
            .then_with(|| b.info_hash.cmp(&a.info_hash))
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Magnet construction
// ───────────────────────────────────────────────────────────────────────────

/// Canonical Nyaa public-tracker set, hardcoded so we don't have to
/// scrape a magnet link out of the view page. DHT bootstrap alone works
/// but is slow (tens of seconds to first peer); these five trackers are
/// stable and shave the cold-start delay.
const NYAA_TRACKER_SET: &[&str] = &[
    "http://nyaa.tracker.wf:7777/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://tracker.torrent.eu.org:451/announce",
];

/// Synthesize a magnet URI from a SeaDex torrent record. All public
/// `isBest=true` Nyaa entries in SeaDex carry a valid 40-char hex
/// `infoHash` (verified in the plan's 2595-entry audit), so this never
/// needs an HTTP round-trip.
///
/// The URI shape is:
/// ```text
/// magnet:?xt=urn:btih:<hash>&dn=<group>&tr=<tracker1>&tr=<tracker2>...
/// ```
///
/// `dn` is a display hint for qBit's UI while metadata is being
/// fetched; `tr[]` are the Nyaa public tracker set. Callers that only
/// care about the hash can pass the URI to qBit unchanged.
pub fn to_magnet_uri(torrent: &SeaDexTorrent) -> String {
    let mut uri = format!("magnet:?xt=urn:btih:{}", torrent.info_hash);
    if !torrent.release_group.is_empty() {
        uri.push_str("&dn=");
        uri.push_str(&urlencoding::encode(&torrent.release_group));
    }
    for tracker in NYAA_TRACKER_SET {
        uri.push_str("&tr=");
        uri.push_str(&urlencoding::encode(tracker));
    }
    uri
}

/// Return the torrent's Nyaa view URL for display / linking. Thin
/// accessor kept in the public API so callers don't reach into the
/// struct field directly — keeps the option open to normalize URLs
/// (strip trailing slashes, canonicalize, etc.) later.
pub fn to_nyaa_view_url(torrent: &SeaDexTorrent) -> &str {
    &torrent.url
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_nyaa_listing_counts_as_a_nyaa_url() {
        assert!(looks_like_nyaa_url("https://nyaa.si/view/1446994"));
        for url in [
            "https://evil.example/?nyaa.si/view/1",
            "http://127.0.0.1:8080/nyaa.si/torrent/1",
            "https://nyaa.si.evil.example/view/1",
            "Chihiro",
            "/torrents.php?id=1",
        ] {
            assert!(!looks_like_nyaa_url(url), "{url}");
        }
    }

    fn torrent(
        group: &str,
        tracker: &str,
        url: &str,
        info_hash: &str,
        is_best: bool,
        dual_audio: bool,
        files: Vec<(&str, i64)>,
    ) -> SeaDexTorrent {
        SeaDexTorrent {
            release_group: group.into(),
            tracker: tracker.into(),
            url: url.into(),
            info_hash: info_hash.into(),
            is_best,
            dual_audio,
            files: files
                .into_iter()
                .map(|(n, l)| SeaDexFile {
                    name: n.into(),
                    length: l,
                })
                .collect(),
        }
    }

    fn nyaa_torrent(group: &str, hash: &str, dual_audio: bool, files: usize) -> SeaDexTorrent {
        let files = (0..files)
            .map(|i| (format!("episode_{i:02}.mkv"), 1_000_000_000_i64))
            .collect::<Vec<_>>();
        SeaDexTorrent {
            release_group: group.into(),
            tracker: "Nyaa".into(),
            url: format!("https://nyaa.si/view/{}", 1_000_000 + files.len() as u32),
            info_hash: hash.into(),
            is_best: true,
            dual_audio,
            files: files
                .into_iter()
                .map(|(n, l)| SeaDexFile { name: n, length: l })
                .collect(),
        }
    }

    // ── parse_list_response ──────────────────────────────────────────

    #[test]
    fn parse_empty_response_returns_none() {
        let body = r#"{"items":[],"page":1,"perPage":30,"totalItems":0}"#;
        let out = parse_list_response(body).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn parse_minimal_response() {
        let body = r#"{
          "items": [{
            "alID": 9260,
            "id": "abc123",
            "notes": "some notes",
            "incomplete": false,
            "expand": {
              "trs": [{
                "releaseGroup": "MTBB",
                "tracker": "Nyaa",
                "url": "https://nyaa.si/view/1446994",
                "infoHash": "abcdef0123456789abcdef0123456789abcdef01",
                "isBest": true,
                "dualAudio": false,
                "files": [{"name": "ep01.mkv", "length": 123}]
              }]
            }
          }]
        }"#;
        let entry = parse_list_response(body).unwrap().expect("some entry");
        assert_eq!(entry.anilist_id, 9260);
        assert_eq!(entry.torrents.len(), 1);
        assert_eq!(entry.torrents[0].release_group, "MTBB");
        assert_eq!(entry.torrents[0].tracker, "Nyaa");
        assert!(entry.torrents[0].is_best);
        assert_eq!(entry.torrents[0].files.len(), 1);
    }

    #[test]
    fn parse_entry_with_no_expand_is_empty_torrents() {
        let body = r#"{
          "items": [{"alID": 1, "id": "x", "notes": "", "incomplete": false}]
        }"#;
        let entry = parse_list_response(body).unwrap().expect("some entry");
        assert!(entry.torrents.is_empty());
    }

    // ── is_unmuxed — signal 1: notes keyword ─────────────────────────

    #[test]
    fn unmuxed_notes_keyword() {
        let t = nyaa_torrent("Headpatter", "a".repeat(40).as_str(), false, 12);
        assert!(is_unmuxed(
            &t,
            "Headpatter is the unmuxed best but needs hand-muxing"
        ));
        assert!(is_unmuxed(&t, "notes mention UNMUXED in caps"));
        assert!(is_unmuxed(&t, "unmux sidecars"));
        assert!(is_unmuxed(&t, "needs mux per E.N.D notes"));
        assert!(!is_unmuxed(&t, "Headpatter's normal BD encode, MTBB subs"));
    }

    // ── is_unmuxed — signal 2: file-structure heuristic ─────────────

    #[test]
    fn unmuxed_by_audio_sidecar_pattern() {
        // JySzE Cowboy Bebop shape: 40 .mkv + 157 .mka.
        let mut files: Vec<(&str, i64)> = Vec::new();
        for i in 0..40 {
            // Leak an owned name via Box::leak for the test helper.
            let name = Box::leak(format!("ep_{i}.mkv").into_boxed_str());
            files.push((name, 1));
        }
        for i in 0..157 {
            let name = Box::leak(format!("ep_{i}.mka").into_boxed_str());
            files.push((name, 1));
        }
        let t = torrent(
            "JySzE",
            "Nyaa",
            "https://nyaa.si/view/1",
            "h",
            true,
            false,
            files,
        );
        assert!(is_unmuxed(&t, ""));
    }

    #[test]
    fn unmuxed_by_sub_sidecar_pattern_with_no_audio() {
        // Figmentos Kemonozume: .mp4 videos with .ass sidecars, no .mka.
        let files = vec![
            ("kemonozume_01.mp4", 1),
            ("kemonozume_01.ass", 1),
            ("kemonozume_02.mp4", 1),
            ("kemonozume_02.ass", 1),
        ];
        let t = torrent(
            "Figmentos",
            "Nyaa",
            "https://nyaa.si/view/2",
            "h",
            true,
            false,
            files,
        );
        assert!(is_unmuxed(&t, ""));
    }

    #[test]
    fn muxed_release_has_no_sidecars() {
        // MTBB Monogatari: pure .mkv, no sidecars. Not unmuxed.
        let files = vec![("mono_01.mkv", 1), ("mono_02.mkv", 1), ("mono_03.mkv", 1)];
        let t = torrent(
            "MTBB",
            "Nyaa",
            "https://nyaa.si/view/3",
            "h",
            true,
            false,
            files,
        );
        assert!(!is_unmuxed(&t, ""));
    }

    #[test]
    fn sub_sidecar_with_audio_present_is_muxed() {
        // If a release has .ass AND .mka, the .ass is probably a font
        // attachment or a bonus sub — not a forced sidecar. Don't flag.
        // (Well, we flag on audio-count alone in this case.)
        let files = vec![
            ("ep01.mkv", 1),
            ("ep01.mka", 1), // triggers audio sidecar rule
            ("ep01.ass", 1),
        ];
        let t = torrent(
            "X",
            "Nyaa",
            "https://nyaa.si/view/4",
            "h",
            true,
            false,
            files,
        );
        assert!(is_unmuxed(&t, ""));
    }

    #[test]
    fn zero_video_files_is_not_unmuxed() {
        let files = vec![("cover.jpg", 1), ("readme.txt", 1)];
        let t = torrent(
            "X",
            "Nyaa",
            "https://nyaa.si/view/5",
            "h",
            true,
            false,
            files,
        );
        assert!(!is_unmuxed(&t, ""));
    }

    // ── is_usable ──────────────────────────────────────────────────

    #[test]
    fn usable_rejects_non_best() {
        let mut t = nyaa_torrent("X", &"a".repeat(40), false, 1);
        t.is_best = false;
        assert!(!is_usable(&t, ""));
    }

    #[test]
    fn usable_rejects_ab_tracker() {
        let mut t = nyaa_torrent("X", &"a".repeat(40), false, 1);
        t.tracker = "AB".into();
        assert!(!is_usable(&t, ""));
    }

    #[test]
    fn usable_rejects_animetosho() {
        let mut t = nyaa_torrent("FREEPALESTINE", &"a".repeat(40), false, 1);
        t.tracker = "AnimeTosho".into();
        t.url = "https://animetosho.org/view/freepalestine.12345".into();
        assert!(!is_usable(&t, ""));
    }

    #[test]
    fn usable_rejects_chihiro_literal_url() {
        // alID=151126 data-quality case: tracker says something but URL
        // is a literal non-URL string. Even if tracker were "Nyaa",
        // looks_like_nyaa_url should kill it.
        let t = torrent(
            "Chihiro",
            "Nyaa",
            "Chihiro", // literally this string
            "abcdef0123456789abcdef0123456789abcdef01",
            true,
            false,
            vec![("ep01.mkv", 1)],
        );
        assert!(!is_usable(&t, ""));
    }

    #[test]
    fn usable_rejects_redacted_infohash() {
        let t = torrent(
            "X",
            "Nyaa",
            "https://nyaa.si/view/9",
            "<redacted>",
            true,
            false,
            vec![("ep01.mkv", 1)],
        );
        assert!(!is_usable(&t, ""));
    }

    #[test]
    fn usable_rejects_unmuxed() {
        let t = nyaa_torrent("E.N.D", &"a".repeat(40), false, 1);
        assert!(!is_usable(&t, "E.N.D is the unmuxed best release"));
    }

    #[test]
    fn usable_accepts_clean_nyaa_muxed_best() {
        let t = nyaa_torrent("MTBB", &"a".repeat(40), false, 1);
        assert!(is_usable(&t, "standard notes"));
    }

    // ── pick_best ──────────────────────────────────────────────────

    #[test]
    fn pick_best_returns_none_on_empty() {
        let entry = SeaDexEntry {
            anilist_id: 1,
            pocketbase_id: "x".into(),
            notes: String::new(),
            incomplete: false,
            torrents: vec![],
        };
        assert!(pick_best(&entry, true).is_none());
    }

    #[test]
    fn pick_best_returns_none_when_all_filtered() {
        // All AB — no Nyaa candidates.
        let mut a = nyaa_torrent("MTBB", &"a".repeat(40), false, 1);
        a.tracker = "AB".into();
        let entry = SeaDexEntry {
            anilist_id: 1,
            pocketbase_id: "x".into(),
            notes: String::new(),
            incomplete: false,
            torrents: vec![a],
        };
        assert!(pick_best(&entry, true).is_none());
    }

    #[test]
    fn pick_best_single_candidate() {
        let a = nyaa_torrent("MTBB", &"a".repeat(40), false, 12);
        let entry = SeaDexEntry {
            anilist_id: 9260,
            pocketbase_id: "x".into(),
            notes: String::new(),
            incomplete: false,
            torrents: vec![a],
        };
        let pick = pick_best(&entry, true).unwrap();
        assert_eq!(pick.release_group, "MTBB");
    }

    #[test]
    fn pick_best_chainsaw_man_dual_audio_split() {
        // alID=127230: Flugel dualAudio=true, Okay-Subs dualAudio=false.
        // Both Nyaa, both muxed, both best. Filtering by prefer_subs
        // picks exactly one.
        let flugel = nyaa_torrent("Flugel", &"b".repeat(40), true, 12);
        let okay = nyaa_torrent("Okay-Subs", &"a".repeat(40), false, 12);
        let entry = SeaDexEntry {
            anilist_id: 127230,
            pocketbase_id: "csm".into(),
            notes: String::new(),
            incomplete: false,
            torrents: vec![flugel, okay],
        };
        let subs = pick_best(&entry, true).unwrap();
        assert_eq!(subs.release_group, "Okay-Subs");
        let dubs = pick_best(&entry, false).unwrap();
        assert_eq!(dubs.release_group, "Flugel");
    }

    #[test]
    fn pick_best_file_count_tiebreak_favors_mega_pack() {
        // Pattern C from plan §4.2: JySzE 554-file mega-pack vs two
        // 1-file patches. Prefer_subs is unused (all same dual_audio).
        let mega = nyaa_torrent("JySzE-mega", &"f".repeat(40), false, 554);
        let patch1 = nyaa_torrent("JySzE-patch1", &"e".repeat(40), false, 1);
        let patch2 = nyaa_torrent("JySzE-patch2", &"d".repeat(40), false, 1);
        let entry = SeaDexEntry {
            anilist_id: 1735,
            pocketbase_id: "lotgh".into(),
            notes: String::new(),
            incomplete: false,
            torrents: vec![patch1, mega, patch2],
        };
        let pick = pick_best(&entry, true).unwrap();
        assert_eq!(pick.release_group, "JySzE-mega");
    }

    #[test]
    fn pick_best_equivalent_remuxes_stable_tiebreak() {
        // Pattern A: BiRJU vs PMR — same file count, no dual-audio split.
        // Stable pick is whichever info_hash sorts smaller (we pick the
        // larger one for determinism; the exact order doesn't matter,
        // just that it's consistent across calls).
        let a = nyaa_torrent("BiRJU", &"a".repeat(40), false, 12);
        let b = nyaa_torrent("PMR", &"b".repeat(40), false, 12);
        let entry = SeaDexEntry {
            anilist_id: 103119,
            pocketbase_id: "x".into(),
            notes: String::new(),
            incomplete: false,
            torrents: vec![a.clone(), b.clone()],
        };
        // Whatever we pick, two back-to-back calls must agree.
        let p1 = pick_best(&entry, true).map(|t| t.release_group.clone());
        let p2 = pick_best(&entry, true).map(|t| t.release_group.clone());
        assert_eq!(p1, p2);
        assert!(p1.is_some());
    }

    #[test]
    fn pick_best_skips_unmuxed_best_even_if_only_candidate() {
        let mut t = nyaa_torrent("Headpatter", &"a".repeat(40), false, 12);
        // Inject an audio sidecar to trigger the heuristic.
        t.files.push(SeaDexFile {
            name: "bonus.mka".into(),
            length: 1,
        });
        // Need audio_count >= video_count, which is already not the
        // case here (12 vs 1). Use notes instead.
        let entry = SeaDexEntry {
            anilist_id: 20910,
            pocketbase_id: "x".into(),
            notes: "Headpatter is the unmuxed best".into(),
            incomplete: false,
            torrents: vec![t],
        };
        assert!(pick_best(&entry, true).is_none());
    }

    #[test]
    fn pick_best_skips_dual_audio_filter_if_pool_is_uniform() {
        // Two subs-only releases — prefer_subs has nothing to filter.
        let a = nyaa_torrent("Group1", &"a".repeat(40), false, 12);
        let b = nyaa_torrent("Group2", &"b".repeat(40), false, 10);
        let entry = SeaDexEntry {
            anilist_id: 1,
            pocketbase_id: "x".into(),
            notes: String::new(),
            incomplete: false,
            torrents: vec![a, b],
        };
        // prefer_subs=false (wants dub) — but nothing has dub, so we
        // fall back to file-count tiebreak on the whole set.
        let pick = pick_best(&entry, false).unwrap();
        assert_eq!(pick.release_group, "Group1"); // more files
    }

    // ── to_magnet_uri ──────────────────────────────────────────────

    #[test]
    fn magnet_contains_hash_and_trackers() {
        let t = torrent(
            "MTBB",
            "Nyaa",
            "https://nyaa.si/view/1",
            "abcdef0123456789abcdef0123456789abcdef01",
            true,
            false,
            vec![("ep01.mkv", 1)],
        );
        let uri = to_magnet_uri(&t);
        assert!(uri.starts_with("magnet:?xt=urn:btih:abcdef0123456789abcdef0123456789abcdef01"));
        assert!(uri.contains("&dn=MTBB"));
        // Every tracker should be URL-encoded and present.
        for tr in NYAA_TRACKER_SET {
            assert!(uri.contains(&urlencoding::encode(tr).into_owned()));
        }
    }

    #[test]
    fn magnet_encodes_release_group_with_special_chars() {
        let t = torrent(
            "Yūrei",
            "Nyaa",
            "https://nyaa.si/view/1",
            "aa",
            true,
            false,
            vec![],
        );
        let uri = to_magnet_uri(&t);
        // `ū` is percent-encoded in the `dn=` param.
        assert!(uri.contains("&dn="));
        assert!(!uri.contains("ū"));
    }

    #[test]
    fn magnet_empty_group_is_omitted() {
        let t = torrent(
            "",
            "Nyaa",
            "https://nyaa.si/view/1",
            "aa",
            true,
            false,
            vec![],
        );
        let uri = to_magnet_uri(&t);
        assert!(!uri.contains("&dn="));
    }

    // ── to_nyaa_view_url ───────────────────────────────────────────

    #[test]
    fn view_url_is_passthrough() {
        let t = torrent(
            "MTBB",
            "Nyaa",
            "https://nyaa.si/view/1446994",
            "aa",
            true,
            false,
            vec![],
        );
        assert_eq!(to_nyaa_view_url(&t), "https://nyaa.si/view/1446994");
    }
}
