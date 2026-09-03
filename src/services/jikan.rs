use crate::services::html::sanitize_rich_description;
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::services::anilist::{AnimeDetail, AnimeEntry, RelatedEntry, StreamingEpisode};

/// Default base URL for the MAL metadata fallback. Tenrai's v1 API is a
/// drop-in continuation of Jikan v4 (identical response schema, authless);
/// Jikan's public API at api.jikan.moe is being discontinued October 1,
/// 2026 and its search endpoint is already failing intermittently (#193).
/// Point `JIKAN_API_BASE` at any Jikan-v4-compatible base (e.g. a
/// self-hosted Jikan) to override.
const JIKAN_API: &str = "https://api.tenrai.org/v1";

/// Cache TTL in seconds (7 days).
const CACHE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
const NEGATIVE_CACHE_SENTINEL: &str = "__RYOKAN_EMPTY__";
const DETAIL_CACHE_TTL_SECS: u64 = 15 * 60;
const DETAIL_CACHE_MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone)]
struct DetailCacheEntry {
    detail: AnimeDetail,
    fetched_at: Instant,
}

static DETAIL_CACHE: LazyLock<RwLock<HashMap<i64, DetailCacheEntry>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Shared reqwest client. Six call sites in this module previously
/// rebuilt the client per request — wasteful given how often Jikan
/// gets hit (search, details, episodes, relations, all routed
/// through different helpers). One shared client lets the connection
/// pool reuse TLS sessions across calls.
///
/// Timeouts: 10s connect, 30s overall — same rationale as the AniList
/// client. Callers' cooldown/backoff logic is unaffected; this just
/// stops a hung connection from pinning a pool slot for hours.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the Jikan reqwest client should not fail")
});

/// When Jikan rate-limits us, remember "unavailable until Instant" so
/// `search_anime` returns a clean cooldown error immediately rather than
/// hammering the API and piling up more 429s.
static JIKAN_COOLDOWN_UNTIL: LazyLock<StdMutex<Option<Instant>>> =
    LazyLock::new(|| StdMutex::new(None));

const JIKAN_COOLDOWN_DEFAULT: Duration = Duration::from_secs(60);
const JIKAN_COOLDOWN_MAX: Duration = Duration::from_secs(300);

fn jikan_cooldown_remaining() -> Option<Duration> {
    let guard = JIKAN_COOLDOWN_UNTIL.lock().ok()?;
    let until = (*guard)?;
    let now = Instant::now();
    if now < until { Some(until - now) } else { None }
}

fn set_jikan_cooldown(retry_after_secs: Option<u64>) {
    let dur = retry_after_secs
        .map(Duration::from_secs)
        .unwrap_or(JIKAN_COOLDOWN_DEFAULT)
        .min(JIKAN_COOLDOWN_MAX);
    if let Ok(mut guard) = JIKAN_COOLDOWN_UNTIL.lock() {
        *guard = Some(Instant::now() + dur);
    }
}

/// Parse Jikan's JSON error body (shape: `{"status":"429","type":"...","message":"..."}`)
/// into a human-readable one-liner. Falls back to a short snippet if parsing fails.
fn parse_jikan_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(msg) = v["message"].as_str()
    {
        // Take just the first sentence; the rest is usually a docs link.
        let short = msg.split('.').next().unwrap_or(msg).trim();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return format!("Jikan rate-limited (HTTP 429): {}", short);
        }
        return format!("Jikan HTTP {}: {}", status.as_u16(), short);
    }
    let snippet: String = body.chars().take(120).collect();
    format!("Jikan HTTP {}: {}", status.as_u16(), snippet.trim())
}

#[derive(Debug, Clone)]
pub struct EpisodeInfo {
    pub title: String,
    pub aired: String,
}

fn is_rate_limited(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || body.to_ascii_lowercase().contains("too many requests")
        || body.to_ascii_lowercase().contains("rate limit")
}

async fn get_text_with_retry(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let mut backoff = Duration::from_millis(700);
    let mut last_err = String::new();

    for attempt in 0..4 {
        let resp = client
            .get(url)
            .header("User-Agent", "Ryokan/0.1")
            .send()
            .await
            .map_err(|e| format!("Jikan request failed: {}", e))?;

        let status = resp.status();
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let text = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read Jikan response: {}", e))?;

        if status.is_success() {
            return Ok(text);
        }

        // chars().take() instead of byte-slice — Jikan error bodies
        // often contain non-ASCII characters (curly apostrophes etc.)
        // and a byte-slice at index 200 panics if a multi-byte char
        // straddles the boundary. Mirrors the pattern in
        // parse_jikan_error above.
        let preview: String = text.chars().take(200).collect();
        last_err = format!("HTTP {status}: {preview}");
        if is_rate_limited(status, &text) && attempt < 3 {
            tokio::time::sleep(backoff).await;
            backoff *= 2;
            continue;
        }

        // Falling out of the retry loop on a rate-limited response
        // (final attempt or non-retryable status). Set the global
        // cooldown so subsequent jikan calls — including episode
        // pagination, relations fetches, and other endpoints that go
        // through this helper — skip the round trip entirely instead
        // of burning another ~9s of retry sleep on the same 429 storm.
        // The search caller already does this for its own path; this
        // brings the rest of the helpers into the same backoff regime.
        if is_rate_limited(status, &text) {
            set_jikan_cooldown(retry_after_secs);
        }
        return Err(last_err);
    }

    Err(last_err)
}

async fn get_json_with_retry<T: for<'de> serde::Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let text = get_text_with_retry(client, url).await?;
    serde_json::from_str(&text).map_err(|e| format!("Failed to parse Jikan response: {}", e))
}

#[derive(Deserialize)]
struct JikanResponse {
    data: Vec<JikanEpisode>,
}

#[derive(Deserialize)]
struct JikanEpisode {
    #[allow(dead_code)]
    mal_id: i32,
    #[serde(alias = "episode_id")]
    episode_id: Option<i32>,
    title: Option<String>,
    aired: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    data: Vec<SearchAnime>,
}

#[derive(Debug, Deserialize)]
struct SearchAnime {
    mal_id: i64,
    #[serde(default)]
    title: Option<String>,
    title_english: Option<String>,
    title_japanese: Option<String>,
    images: Option<SearchImages>,
    #[serde(rename = "type")]
    anime_type: Option<String>,
    status: Option<String>,
    episodes: Option<i32>,
    /// MAL's 1-10 community score, e.g. `7.85`. `None` for entries
    /// with no rating yet. Multiplied by 10 on conversion to
    /// `AnimeEntry::average_score` (Option<i32> on the AL 0-100
    /// scale) so both providers feed the same downstream sink.
    score: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SearchImages {
    jpg: Option<ImageSet>,
    webp: Option<ImageSet>,
}

#[derive(Debug, Deserialize)]
struct ImageSet {
    large_image_url: Option<String>,
    image_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FullResponse {
    data: FullAnime,
}

#[derive(Debug, Deserialize)]
struct RelationsResponse {
    data: Vec<RelationGroupResponse>,
}

#[derive(Debug, Deserialize)]
struct RelationGroupResponse {
    relation: String,
    entry: Vec<RelationEntryResponse>,
}

#[derive(Debug, Deserialize)]
struct RelationEntryResponse {
    mal_id: i64,
    #[serde(rename = "type")]
    media_type: String,
    name: String,
    #[allow(dead_code)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FullAnime {
    mal_id: i64,
    #[serde(default)]
    title: Option<String>,
    title_english: Option<String>,
    title_japanese: Option<String>,
    synopsis: Option<String>,
    background: Option<String>,
    /// MAL age-rating label, e.g. `Rx - Hentai`; the adult signal.
    #[serde(default)]
    rating: Option<String>,
    images: Option<SearchImages>,
    trailer: Option<TrailerInfo>,
    #[serde(rename = "type")]
    anime_type: Option<String>,
    status: Option<String>,
    episodes: Option<i32>,
    duration: Option<String>,
    season: Option<String>,
    year: Option<i32>,
    score: Option<f64>,
    genres: Option<Vec<NamedItem>>,
    themes: Option<Vec<NamedItem>>,
    demographics: Option<Vec<NamedItem>>,
    aired: Option<AiredInfo>,
    /// `/anime/{id}/full` already returns the same relation graph that
    /// `/anime/{id}/relations` would — pulling it out of the full payload
    /// avoids a second round-trip per detail fetch. Falls through empty
    /// (no implicit fallback to the dedicated endpoint) if Jikan ever
    /// changes the contract; the relations panel for that fetch will
    /// just be empty until the next refresh sweep. The dedicated
    /// `/relations` helper still exists (used by `fetch_sequel_chain_ids`)
    /// if a future caller needs an explicit fallback.
    #[serde(default)]
    relations: Option<Vec<RelationGroupResponse>>,
}

#[derive(Debug, Deserialize)]
struct TrailerInfo {
    images: Option<TrailerImages>,
    #[allow(dead_code)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TrailerImages {
    maximum_image_url: Option<String>,
    large_image_url: Option<String>,
    medium_image_url: Option<String>,
    image_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedItem {
    name: String,
}

#[derive(Debug, Deserialize)]
struct AiredInfo {
    from: Option<String>,
    to: Option<String>,
}

/// Parse the year out of a Jikan ISO-8601 air date (`YYYY-MM-DDT...`).
/// Used to populate `end_year` from `aired.to` for finished shows so
/// Layer 4 temporal inference can distinguish "finished last year" from
/// "started years ago, still airing."
fn parse_air_year(value: Option<&str>) -> Option<i32> {
    value
        .and_then(|s| s.get(0..4))
        .and_then(|y| y.parse::<i32>().ok())
}

pub async fn search_anime(query: &str) -> Result<Vec<AnimeEntry>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    // Short-circuit if a prior request told us Jikan is rate-limiting us.
    // Prevents the "AL 429 → Jikan 429" cascade that drains Jikan's budget
    // whenever AniList enters its cooldown window.
    if let Some(remaining) = jikan_cooldown_remaining() {
        return Err(format!(
            "Jikan rate-limited (cooldown {}s remaining)",
            remaining.as_secs().max(1)
        ));
    }

    let client = &*HTTP_CLIENT;
    let api_base = std::env::var("JIKAN_API_BASE").unwrap_or_else(|_| JIKAN_API.to_string());
    let url = format!("{}/anime", api_base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .query(&[("q", query), ("limit", "10")])
        .header("User-Agent", "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("Jikan unreachable: {}", e))?;

    let status = resp.status();
    let retry_after_secs = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read Jikan search response: {}", e))?;

    if !status.is_success() {
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            set_jikan_cooldown(retry_after_secs);
        }
        return Err(parse_jikan_error(status, &text));
    }

    let body: SearchResponse = serde_json::from_str(&text)
        .map_err(|e| format!("Failed to parse Jikan search response: {}", e))?;

    Ok(body
        .data
        .into_iter()
        .map(|anime| {
            let (format, _) = normalize_enum_label(anime.anime_type);
            let (status, status_display) = normalize_enum_label(anime.status);
            AnimeEntry {
                id: -anime.mal_id,
                id_mal: Some(anime.mal_id),
                title_romaji: anime.title.clone().unwrap_or_default(),
                title_english: anime.title_english.unwrap_or_default(),
                title_native: anime.title_japanese.unwrap_or_default(),
                cover_url: first_image_url(anime.images.as_ref()),
                format,
                status,
                status_display,
                episodes: anime.episodes.filter(|&n| n > 0),
                season_year: None, // Jikan search results don't include year
                source: "mal".to_string(),
                // MAL ships a 0-10 float; AnimeEntry stores 0-100 to
                // match AL's `averageScore` shape. `(s * 10.0).round()`
                // keeps the typical "7.85" → 79 mapping (AL would
                // call this 79).
                average_score: anime
                    .score
                    .filter(|s| *s > 0.0)
                    .map(|s| (s * 10.0).round() as i32),
            }
        })
        .collect())
}

fn score_class(score: Option<i32>, is_ten_point: bool) -> String {
    let class = if is_ten_point {
        match score {
            Some(s) if s >= 9 => "tag-score-purple",
            Some(s) if s >= 7 => "tag-score-green",
            Some(s) if s > 5 => "tag-score-yellow",
            _ => "tag-score-red",
        }
    } else {
        match score {
            Some(s) if s >= 85 => "tag-score-purple",
            Some(s) if s >= 75 => "tag-score-green",
            Some(s) if s > 65 => "tag-score-yellow",
            _ => "tag-score-red",
        }
    };
    class.to_string()
}

fn prettify_label(raw: &str) -> String {
    raw.replace('_', " ").to_uppercase()
}

fn normalize_enum_label(raw: Option<String>) -> (String, String) {
    let raw = raw.unwrap_or_default();
    let enum_value = raw.to_uppercase().replace(' ', "_");
    let display = prettify_label(&enum_value);
    (enum_value, display)
}

async fn fetch_relation_card_detail(mal_id: i64, fallback_name: &str) -> RelatedEntry {
    let fallback = || RelatedEntry {
        id: -mal_id,
        id_mal: Some(mal_id),
        title_romaji: fallback_name.to_string(),
        title_english: String::new(),
        title_native: String::new(),
        cover_url: String::new(),
        format: String::new(),
        status: String::new(),
        status_display: String::new(),
        episodes: None,
        relation_type: String::new(),
        season_year: None,
        media_type: "ANIME".to_string(),
    };

    let client = &*HTTP_CLIENT;
    let api_base = std::env::var("JIKAN_API_BASE").unwrap_or_else(|_| JIKAN_API.to_string());
    let url = format!("{}/anime/{}/full", api_base.trim_end_matches('/'), mal_id);
    let body: FullResponse = match get_json_with_retry(client, &url).await {
        Ok(body) => body,
        Err(_) => return fallback(),
    };

    let anime = body.data;
    let (format, _) = normalize_enum_label(anime.anime_type);
    let (status, status_display) = normalize_enum_label(anime.status);
    RelatedEntry {
        id: -anime.mal_id,
        id_mal: Some(anime.mal_id),
        title_romaji: non_empty(anime.title.as_deref().unwrap_or(""), fallback_name),
        title_english: anime.title_english.unwrap_or_default(),
        title_native: anime.title_japanese.unwrap_or_default(),
        cover_url: first_image_url(anime.images.as_ref()),
        format,
        status,
        status_display,
        episodes: anime.episodes.filter(|&n| n > 0),
        relation_type: String::new(),
        season_year: anime.year,
        media_type: "ANIME".to_string(),
    }
}

/// Build the per-relation card list given an already-fetched relation
/// group set. Hits `/anime/{id}` once per ANIME entry (up to
/// `MAX_RELATION_FETCHES`) for cover/title detail; the relation-group
/// fetch itself is now done as part of `/anime/{id}/full`, so this no
/// longer pays for a separate `/relations` round-trip.
async fn enrich_relations(groups: Vec<RelationGroupResponse>) -> Vec<RelatedEntry> {
    // Jikan's documented anonymous limit is ~3 req/s AND ~60 req/min.
    // The per-second budget is the easy one; the per-minute budget is
    // tight enough that a 10-entry relations graph (sequels, prequels,
    // OVAs, ONAs, side-stories) at 400 ms per call adds 10 requests in
    // 4 s on top of whatever else the metadata path is doing — and a
    // single 429 here flips set_jikan_cooldown, blocking every concurrent
    // search for the cooldown window.
    //
    // Bump the sleep to 500 ms (2 req/s) and cap the fan-out at 8
    // cards total. The relations panel is a "what else exists in this
    // franchise" affordance, not a comprehensive graph; the first 8
    // entries are plenty for the UI.
    const MAX_RELATION_FETCHES: usize = 8;
    const RELATION_FETCH_INTERVAL_MS: u64 = 500;

    let mut out = Vec::new();
    let mut request_count: usize = 0;
    'outer: for group in groups {
        let rel_type = group.relation.to_uppercase().replace(' ', "_");
        for entry in group.entry {
            if !entry.media_type.eq_ignore_ascii_case("ANIME") {
                continue;
            }
            if request_count >= MAX_RELATION_FETCHES {
                break 'outer;
            }
            if request_count > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(RELATION_FETCH_INTERVAL_MS))
                    .await;
            }
            request_count += 1;

            let mut card = fetch_relation_card_detail(entry.mal_id, &entry.name).await;
            card.relation_type = rel_type.clone();
            out.push(card);
        }
    }
    out
}

fn non_empty(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

/// Test-only: pre-populate the in-memory detail cache so a unit test
/// can exercise the cache-hit code path without needing the live
/// Jikan API. Stamped with `Instant::now()` so it stays valid for the
/// next 15 minutes (the cache TTL) — well beyond any test suite
/// runtime.
#[cfg(any(test, feature = "test-support"))]
pub async fn seed_detail_cache_for_tests(mal_id: i64, detail: AnimeDetail) {
    let mut cache = DETAIL_CACHE.write().await;
    cache.insert(
        mal_id,
        DetailCacheEntry {
            detail,
            fetched_at: Instant::now(),
        },
    );
}

/// Test-only: drop a single entry from the detail cache so a follow-up
/// test exercising the cache-miss branch isn't contaminated by a
/// previous seed. The cache is process-global; tests sharing it have
/// to clean up after themselves.
#[cfg(any(test, feature = "test-support"))]
pub async fn clear_detail_cache_entry_for_tests(mal_id: i64) {
    let mut cache = DETAIL_CACHE.write().await;
    cache.remove(&mal_id);
}

/// Test-only: clear the entire detail cache + the rate-limit cooldown
/// guard. Both are process-wide statics that survive across tests; a
/// stray cooldown left over from one test (e.g. a wiremock that
/// returned 429) would short-circuit every subsequent call with
/// "Jikan rate-limited" before the wiremock even gets a request.
#[cfg(any(test, feature = "test-support"))]
pub async fn reset_state_for_tests() {
    {
        let mut cache = DETAIL_CACHE.write().await;
        cache.clear();
    }
    if let Ok(mut g) = JIKAN_COOLDOWN_UNTIL.lock() {
        *g = None;
    }
}

pub async fn get_anime_detail_cached(mal_id: i64) -> Result<AnimeDetail, String> {
    {
        let cache = DETAIL_CACHE.read().await;
        if let Some(entry) = cache.get(&mal_id)
            && entry.fetched_at.elapsed().as_secs() < DETAIL_CACHE_TTL_SECS
        {
            return Ok(entry.detail.clone());
        }
    }

    let detail = get_anime_detail(mal_id).await?;

    {
        let mut cache = DETAIL_CACHE.write().await;
        cache.insert(
            mal_id,
            DetailCacheEntry {
                detail: detail.clone(),
                fetched_at: Instant::now(),
            },
        );
        // Cap the cache so a long-running process can't accumulate
        // every MAL ID it ever touched. Mirrors anilist::DETAIL_CACHE
        // eviction (drop expired first, then drop oldest if still
        // over).
        if cache.len() > DETAIL_CACHE_MAX_ENTRIES {
            let expired: Vec<i64> = cache
                .iter()
                .filter(|(_, e)| e.fetched_at.elapsed().as_secs() >= DETAIL_CACHE_TTL_SECS)
                .map(|(k, _)| *k)
                .collect();
            for k in &expired {
                cache.remove(k);
            }
            if cache.len() > DETAIL_CACHE_MAX_ENTRIES
                && let Some((&oldest_key, _)) = cache.iter().min_by_key(|(_, e)| e.fetched_at)
            {
                cache.remove(&oldest_key);
            }
        }
    }

    Ok(detail)
}

/// MAL's `rating` label is the only adult signal Jikan carries. `Rx -
/// Hentai` is the tier AniList also flags `isAdult`; `R+ - Mild Nudity`
/// and below are not adult on either site.
fn is_adult_rating(rating: Option<&str>) -> bool {
    rating.is_some_and(|r| r.trim_start().starts_with("Rx"))
}

pub async fn get_anime_detail(mal_id: i64) -> Result<AnimeDetail, String> {
    let client = &*HTTP_CLIENT;
    let api_base = std::env::var("JIKAN_API_BASE").unwrap_or_else(|_| JIKAN_API.to_string());
    let url = format!("{}/anime/{}/full", api_base.trim_end_matches('/'), mal_id);
    let body: FullResponse = get_json_with_retry(client, &url)
        .await
        .map_err(|e| format!("Jikan detail failed: {}", e))?;

    let mut anime = body.data;
    let mut genres = Vec::new();
    if let Some(items) = anime.genres {
        genres.extend(items.into_iter().map(|g| g.name));
    }
    if let Some(items) = anime.themes {
        genres.extend(items.into_iter().map(|g| g.name));
    }
    if let Some(items) = anime.demographics {
        genres.extend(items.into_iter().map(|g| g.name));
    }

    let description = build_description(&anime.synopsis, &anime.background);
    let duration = parse_duration_minutes(anime.duration.as_deref());
    let next_airing = estimate_next_airing(
        &anime.status,
        anime.aired.as_ref().and_then(|a| a.from.as_deref()),
    );
    let end_year = parse_air_year(anime.aired.as_ref().and_then(|a| a.to.as_deref()));
    let (format, _) = normalize_enum_label(anime.anime_type.clone());
    let (status, status_display) = normalize_enum_label(anime.status.clone());
    // Relations come pre-baked in the `/full` payload (Jikan v4 — confirmed
    // 2026-04). Skip the redundant `/anime/{id}/relations` round-trip.
    let relations = enrich_relations(anime.relations.take().unwrap_or_default()).await;

    Ok(AnimeDetail {
        is_adult: is_adult_rating(anime.rating.as_deref()),
        id: -anime.mal_id,
        id_mal: Some(anime.mal_id),
        title_romaji: anime.title.clone().unwrap_or_default(),
        title_english: anime.title_english.unwrap_or_default(),
        title_native: anime.title_japanese.unwrap_or_default(),
        cover_url: first_image_url(anime.images.as_ref()),
        banner_url: anime
            .trailer
            .as_ref()
            .and_then(|t| t.images.as_ref())
            .map(first_trailer_image_url)
            .unwrap_or_default(),
        format,
        status,
        status_display,
        episodes: anime.episodes.filter(|&n| n > 0),
        duration,
        season: anime.season.unwrap_or_default().to_uppercase(),
        season_year: anime.year,
        end_year,
        description,
        genres,
        average_score: anime.score.map(|s| s.round() as i32),
        average_score_display: anime.score.map(format_ten_point_score),
        score_is_ten_point: true,
        score_class: score_class(anime.score.map(|s| s.round() as i32), true),
        next_airing_episode: next_airing.and_then(|(ep, _)| ep),
        next_airing_at: next_airing.and_then(|(_, ts)| ts),
        synonyms: Vec::new(),
        streaming_episodes: anime
            .trailer
            .and_then(|t| t.url)
            .map(|url| {
                vec![StreamingEpisode {
                    title: "Trailer".to_string(),
                    thumbnail: String::new(),
                    url,
                    site: "Trailer".to_string(),
                }]
            })
            .unwrap_or_default(),
        relations,
    })
}

async fn fetch_relation_groups_raw(mal_id: i64) -> Vec<RelationGroupResponse> {
    let client = &*HTTP_CLIENT;
    let api_base = std::env::var("JIKAN_API_BASE").unwrap_or_else(|_| JIKAN_API.to_string());
    let url = format!(
        "{}/anime/{}/relations",
        api_base.trim_end_matches('/'),
        mal_id
    );
    match get_json_with_retry::<RelationsResponse>(client, &url).await {
        Ok(body) => body.data,
        Err(_) => Vec::new(),
    }
}

async fn fetch_sequel_chain_ids(start_mal_id: i64, max_extra: usize) -> Vec<i64> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(start_mal_id);
    let mut current = start_mal_id;

    for _ in 0..max_extra {
        let groups = fetch_relation_groups_raw(current).await;
        let next_id = groups
            .into_iter()
            .find(|g| g.relation.eq_ignore_ascii_case("Sequel"))
            .and_then(|g| {
                g.entry.into_iter().find_map(|entry| {
                    if entry.media_type.eq_ignore_ascii_case("anime")
                        && !seen.contains(&entry.mal_id)
                    {
                        Some(entry.mal_id)
                    } else {
                        None
                    }
                })
            });

        let Some(next_id) = next_id else {
            break;
        };
        seen.insert(next_id);
        out.push(next_id);
        current = next_id;
    }

    out
}

pub async fn fetch_episode_titles(db: &SqlitePool, mal_id: i64) -> HashMap<i32, EpisodeInfo> {
    if let Ok(Some(cached)) = get_cached_episodes(db, mal_id).await {
        return cached;
    }

    let episodes = fetch_from_jikan(mal_id).await;
    let _ = cache_episodes(db, mal_id, &episodes).await;
    episodes
}

pub async fn fetch_episode_titles_for_detail(
    db: &SqlitePool,
    detail: &AnimeDetail,
) -> HashMap<i32, EpisodeInfo> {
    let Some(mal_id) = detail.id_mal else {
        return HashMap::new();
    };

    // Currently-airing series have `episodes: null` on AniList, so
    // `detail.episodes.unwrap_or(0)` is 0 and the cache check below would
    // accept whatever short-count is already stored. Use `nextAiringEpisode
    // - 1` as a lower bound for what a "complete enough" cache looks like;
    // otherwise a broken partial fetch (see the Jikan pagination bug with
    // One Piece) would get pinned in the cache forever.
    let effective_target = detail.episodes.unwrap_or(0).max(
        detail
            .next_airing_episode
            .map(|n| (n - 1).max(0))
            .unwrap_or(0),
    );

    // MAL lags AniList by a few episodes for long-running series (One Piece
    // is the canonical case — AniList's airing schedule is always ahead of
    // what MAL has indexed). Accept a small gap so we don't re-fetch Jikan
    // on every page load just because the last 1-2 episodes haven't made it
    // into MAL's database yet.
    //
    // Keep this tight. With tolerance=10 a 12-episode airing season with a
    // 2-episode partial cache would happily satisfy the check (2 + 10 ≥ 12)
    // and the background sweep would never refetch — the cache stayed pinned
    // for the full 7-day TTL even though new episodes had aired. Tolerance=2
    // still absorbs the normal 1-episode MAL indexing lag for long runners
    // while forcing a refetch as soon as the gap grows beyond that.
    const JIKAN_LAG_TOLERANCE: i32 = 2;

    if let Ok(Some(cached)) = get_cached_episodes(db, mal_id).await {
        let cached_count = cached.len() as i32;
        if effective_target <= 0
            || cached.is_empty()
            || cached_count + JIKAN_LAG_TOLERANCE >= effective_target
        {
            return cached;
        }
    }

    // Cache missing or insufficient — fetch fresh from Jikan directly so we
    // don't reuse the same insufficient cache via `fetch_episode_titles`.
    let fresh = fetch_from_jikan(mal_id).await;
    let _ = cache_episodes(db, mal_id, &fresh).await;
    let target_count = effective_target;
    let mut merged = fresh;
    if target_count <= 0 || (merged.len() as i32) >= target_count {
        return merged;
    }

    let sequel_ids = fetch_sequel_chain_ids(mal_id, 4).await;
    let mut next_number = merged.keys().max().copied().unwrap_or(0) + 1;
    for sequel_id in sequel_ids {
        let sequel_eps = fetch_episode_titles(db, sequel_id).await;
        if sequel_eps.is_empty() {
            continue;
        }
        let mut ordered: Vec<_> = sequel_eps.into_iter().collect();
        ordered.sort_by_key(|(num, _)| *num);
        for (_, info) in ordered {
            merged.insert(next_number, info);
            next_number += 1;
            if target_count > 0 && (merged.len() as i32) >= target_count {
                let _ = cache_episodes(db, mal_id, &merged).await;
                return merged;
            }
        }
    }

    let _ = cache_episodes(db, mal_id, &merged).await;
    merged
}

async fn get_cached_episodes(
    db: &SqlitePool,
    mal_id: i64,
) -> Result<Option<HashMap<i32, EpisodeInfo>>, sqlx::Error> {
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        r#"
        SELECT episode_number, title, aired FROM episode_cache
        WHERE mal_id = ?
        AND cached_at > datetime('now', ? || ' seconds')
        "#,
    )
    .bind(mal_id)
    .bind(-CACHE_TTL_SECS)
    .fetch_all(db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut map = HashMap::new();
    let mut has_negative_sentinel = false;
    for (num, title, aired) in rows {
        if num == 0 && title == NEGATIVE_CACHE_SENTINEL {
            has_negative_sentinel = true;
            continue;
        }
        map.insert(num, EpisodeInfo { title, aired });
    }

    if has_negative_sentinel || !map.is_empty() {
        Ok(Some(map))
    } else {
        Ok(None)
    }
}

async fn cache_episodes(
    db: &SqlitePool,
    mal_id: i64,
    episodes: &HashMap<i32, EpisodeInfo>,
) -> Result<(), sqlx::Error> {
    // Wrap DELETE + N INSERTs in one transaction. SQLite's WAL only
    // fsyncs at commit, so a 1100-episode One Piece refresh becomes
    // one fsync instead of 1101 — orders-of-magnitude difference on
    // any non-tmpfs disk. As a bonus the writer lock is held once and
    // released once, which keeps concurrent readers (the rest of the
    // app) from being chunked into 1100 tiny windows.
    let mut tx = db.begin().await?;

    sqlx::query("DELETE FROM episode_cache WHERE mal_id = ?")
        .bind(mal_id)
        .execute(&mut *tx)
        .await?;

    if episodes.is_empty() {
        sqlx::query(
            "INSERT INTO episode_cache (mal_id, episode_number, title, aired) VALUES (?, 0, ?, '')",
        )
        .bind(mal_id)
        .bind(NEGATIVE_CACHE_SENTINEL)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    for (num, info) in episodes {
        sqlx::query(
            "INSERT INTO episode_cache (mal_id, episode_number, title, aired) VALUES (?, ?, ?, ?)",
        )
        .bind(mal_id)
        .bind(num)
        .bind(&info.title)
        .bind(&info.aired)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

async fn fetch_from_jikan(mal_id: i64) -> HashMap<i32, EpisodeInfo> {
    let mut episodes = HashMap::new();
    let client = &*HTTP_CLIENT;
    let mut page = 1;
    let api_base = std::env::var("JIKAN_API_BASE").unwrap_or_else(|_| JIKAN_API.to_string());

    loop {
        let url = format!(
            "{}/anime/{}/episodes?page={}",
            api_base.trim_end_matches('/'),
            mal_id,
            page
        );

        let body: JikanResponse = match get_json_with_retry(client, &url).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("Jikan episode fetch failed for mal_id {}: {}", mal_id, e);
                break;
            }
        };

        for (idx, ep) in body.data.iter().enumerate() {
            let aired = ep.aired.as_deref().unwrap_or("").to_string();
            let aired_short = if aired.len() >= 10 {
                aired[..10].to_string()
            } else {
                aired
            };

            let number = ep.episode_id.unwrap_or((page - 1) * 100 + idx as i32 + 1);
            let title = ep.title.clone().unwrap_or_default();

            episodes.insert(
                number,
                EpisodeInfo {
                    title,
                    aired: aired_short,
                },
            );
        }

        // Jikan v4's pagination metadata is unreliable for some anime — One
        // Piece (mal_id 21), for example, reports `has_next_page: false` on
        // every page despite having 12 pages of real data. Drive termination
        // off the payload size instead: Jikan serves 100 episodes per page,
        // so anything smaller means we're past the last full page.
        if body.data.len() < 100 {
            break;
        }

        page += 1;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }

    episodes
}

fn build_description(synopsis: &Option<String>, background: &Option<String>) -> String {
    let combined = match (synopsis.as_deref(), background.as_deref()) {
        (Some(s), Some(b)) if !b.trim().is_empty() => format!("{}\n\n{}", s.trim(), b.trim()),
        (Some(s), _) => s.trim().to_string(),
        (_, Some(b)) => b.trim().to_string(),
        _ => String::new(),
    };

    sanitize_rich_description(&combined, false)
}

fn first_image_url(images: Option<&SearchImages>) -> String {
    images
        .and_then(|imgs| {
            imgs.webp
                .as_ref()
                .and_then(|set| set.large_image_url.clone().or(set.image_url.clone()))
        })
        .or_else(|| {
            images.and_then(|imgs| {
                imgs.jpg
                    .as_ref()
                    .and_then(|set| set.large_image_url.clone().or(set.image_url.clone()))
            })
        })
        .unwrap_or_default()
}

fn first_trailer_image_url(images: &TrailerImages) -> String {
    images
        .maximum_image_url
        .clone()
        .or(images.large_image_url.clone())
        .or(images.medium_image_url.clone())
        .or(images.image_url.clone())
        .unwrap_or_default()
}

fn parse_duration_minutes(duration: Option<&str>) -> Option<i32> {
    let raw = duration?.trim();
    let mut total = 0;
    let mut saw = false;

    for part in raw.split(',').map(str::trim) {
        if let Some(num) = part.strip_suffix(" hr") {
            if let Ok(hours) = num.trim().parse::<i32>() {
                total += hours * 60;
                saw = true;
            }
        } else if let Some(num) = part.strip_suffix(" hrs") {
            if let Ok(hours) = num.trim().parse::<i32>() {
                total += hours * 60;
                saw = true;
            }
        } else if let Some(num) = part.strip_suffix(" min")
            && let Ok(minutes) = num.trim().parse::<i32>()
        {
            total += minutes;
            saw = true;
        }
    }

    if saw { Some(total) } else { None }
}

fn estimate_next_airing(
    status: &Option<String>,
    aired_from: Option<&str>,
) -> Option<(Option<i32>, Option<i64>)> {
    let status = status.as_deref()?.to_ascii_lowercase();
    if !status.contains("currently") {
        return None;
    }

    let _ = aired_from;
    Some((None, None))
}

fn format_ten_point_score(score: f64) -> String {
    let rounded = (score * 100.0).round() / 100.0;
    let mut s = format!("{:.2}", rounded);
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    format!("{}/10", s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── parse_jikan_error ────────────────────────────────────────

    #[test]
    fn parse_jikan_error_extracts_first_sentence_of_message_on_429() {
        // Jikan error bodies carry a verbose message + docs link.
        // The impl trims to the first sentence so the log line stays
        // short and actionable.
        let body = r#"{"status":429,"type":"RateLimitException","message":"You are being rate limited. See docs at https://docs.api.jikan.moe"}"#;
        let err = parse_jikan_error(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        assert!(err.starts_with("Jikan rate-limited"));
        assert!(err.contains("You are being rate limited"));
        assert!(!err.contains("https://"));
    }

    #[test]
    fn parse_jikan_error_includes_status_code_on_non_429() {
        let body = r#"{"status":404,"message":"Resource does not exist"}"#;
        let err = parse_jikan_error(reqwest::StatusCode::NOT_FOUND, body);
        assert!(err.contains("404"));
        assert!(err.contains("Resource does not exist"));
    }

    #[test]
    fn parse_jikan_error_falls_back_to_snippet_on_non_json_body() {
        // HTML error pages from a CDN in front of Jikan — no
        // `message` field to extract.
        let body = "<html><body>502 Bad Gateway</body></html>";
        let err = parse_jikan_error(reqwest::StatusCode::BAD_GATEWAY, body);
        assert!(err.contains("502"));
        assert!(err.contains("<html>") || err.contains("Bad Gateway"));
    }

    #[test]
    fn parse_jikan_error_caps_non_json_snippet_length() {
        // 500 chars of noise — must not flood the log line.
        let body = "x".repeat(500);
        let err = parse_jikan_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &body);
        // Impl caps at 120 chars of body + the "Jikan HTTP N:" prefix.
        assert!(
            err.len() < 200,
            "error should be capped; got {} bytes: {err}",
            err.len()
        );
    }

    // ─── is_rate_limited ──────────────────────────────────────────

    #[test]
    fn is_rate_limited_true_on_429_status() {
        assert!(is_rate_limited(reqwest::StatusCode::TOO_MANY_REQUESTS, ""));
    }

    #[test]
    fn is_rate_limited_false_on_other_statuses() {
        assert!(!is_rate_limited(reqwest::StatusCode::OK, ""));
        assert!(!is_rate_limited(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            ""
        ));
        assert!(!is_rate_limited(reqwest::StatusCode::NOT_FOUND, ""));
    }

    // ─── parse_air_year ───────────────────────────────────────────

    #[test]
    fn parse_air_year_extracts_year_from_iso_date() {
        assert_eq!(
            parse_air_year(Some("2024-03-15T00:00:00+00:00")),
            Some(2024)
        );
    }

    #[test]
    fn parse_air_year_returns_none_on_missing_input() {
        assert_eq!(parse_air_year(None), None);
    }

    #[test]
    fn parse_air_year_returns_none_on_malformed_input() {
        assert_eq!(parse_air_year(Some("not-a-date")), None);
        assert_eq!(parse_air_year(Some("")), None);
    }

    // ─── score_class ──────────────────────────────────────────────

    #[test]
    fn score_class_maps_ten_point_scale_to_color_tiers() {
        // Ten-point bands: 9+ purple, 7..9 green, >5..7 yellow, else red.
        assert_eq!(score_class(Some(9), true), "tag-score-purple");
        assert_eq!(score_class(Some(8), true), "tag-score-green");
        assert_eq!(score_class(Some(6), true), "tag-score-yellow");
        assert_eq!(score_class(Some(5), true), "tag-score-red");
    }

    #[test]
    fn score_class_maps_percentage_scale_to_color_tiers() {
        // Hundred-point bands: 85+ purple, 75..85 green, >65..75 yellow, else red.
        assert_eq!(score_class(Some(90), false), "tag-score-purple");
        assert_eq!(score_class(Some(80), false), "tag-score-green");
        assert_eq!(score_class(Some(70), false), "tag-score-yellow");
        assert_eq!(score_class(Some(50), false), "tag-score-red");
    }

    #[test]
    fn score_class_none_is_red_on_both_scales() {
        // "Unrated" is a signal the user should be cautious about —
        // red matches that — not a neutral state.
        assert_eq!(score_class(None, true), "tag-score-red");
        assert_eq!(score_class(None, false), "tag-score-red");
    }

    // ─── prettify_label / normalize_enum_label ────────────────────

    #[test]
    fn prettify_label_uppercases_and_unsnakes() {
        assert_eq!(prettify_label("currently_airing"), "CURRENTLY AIRING");
    }

    #[test]
    fn normalize_enum_label_handles_empty_input() {
        let (enum_value, display) = normalize_enum_label(None);
        assert_eq!(enum_value, "");
        assert_eq!(display, "");
    }

    #[test]
    fn normalize_enum_label_produces_stable_enum_and_human_display() {
        // Jikan returns "Currently Airing" with spaces; the AniList-
        // compatible shape uses "CURRENTLY_AIRING". normalize_enum_label
        // produces both from a single raw input so call sites can
        // pick whichever they need.
        let (enum_value, display) = normalize_enum_label(Some("Currently Airing".into()));
        assert_eq!(enum_value, "CURRENTLY_AIRING");
        assert_eq!(display, "CURRENTLY AIRING");
    }

    // ─── non_empty ────────────────────────────────────────────────

    #[test]
    fn non_empty_returns_primary_when_present() {
        assert_eq!(non_empty("primary", "fallback"), "primary");
    }

    #[test]
    fn non_empty_returns_fallback_when_primary_empty() {
        assert_eq!(non_empty("", "fallback"), "fallback");
    }

    // ─── parse_duration_minutes ───────────────────────────────────

    #[test]
    fn parse_duration_minutes_handles_hr_min_combinations() {
        // Jikan's actual format is comma-separated: "1 hr, 30 min" /
        // "24 min" / "1 hr" / "2 hrs". The parser splits on comma
        // and accumulates each part — a single-string "1 hr 30 min"
        // (no comma) doesn't match either the " hr" or " min" suffix
        // on the whole string and parses as None.
        assert_eq!(parse_duration_minutes(Some("1 hr, 30 min")), Some(90));
        assert_eq!(parse_duration_minutes(Some("24 min")), Some(24));
        assert_eq!(parse_duration_minutes(Some("1 hr")), Some(60));
        assert_eq!(parse_duration_minutes(Some("2 hrs")), Some(120));
    }

    #[test]
    fn parse_duration_minutes_returns_none_on_unrecognized_format() {
        // Jikan also uses "Unknown" for series where no episode
        // duration is set. Must not mis-parse to 0 — callers
        // distinguish None from Some(0).
        assert_eq!(parse_duration_minutes(Some("Unknown")), None);
        assert_eq!(parse_duration_minutes(None), None);
        assert_eq!(parse_duration_minutes(Some("")), None);
    }

    // ─── first_image_url ──────────────────────────────────────────

    #[test]
    fn first_image_url_prefers_webp_large_over_webp_image() {
        let images = SearchImages {
            jpg: None,
            webp: Some(ImageSet {
                large_image_url: Some("https://cdn.example.com/large.webp".into()),
                image_url: Some("https://cdn.example.com/medium.webp".into()),
            }),
        };
        assert_eq!(
            first_image_url(Some(&images)),
            "https://cdn.example.com/large.webp"
        );
    }

    #[test]
    fn first_image_url_falls_back_from_webp_to_jpg_large() {
        let images = SearchImages {
            jpg: Some(ImageSet {
                large_image_url: Some("https://cdn.example.com/art.jpg".into()),
                image_url: None,
            }),
            webp: None,
        };
        assert_eq!(
            first_image_url(Some(&images)),
            "https://cdn.example.com/art.jpg"
        );
    }

    #[test]
    fn first_image_url_returns_empty_string_when_no_urls_present() {
        let images = SearchImages {
            jpg: Some(ImageSet {
                large_image_url: None,
                image_url: None,
            }),
            webp: None,
        };
        assert_eq!(first_image_url(Some(&images)), "");
        assert_eq!(first_image_url(None), "");
    }

    // ─── format_ten_point_score ───────────────────────────────────

    #[test]
    fn format_ten_point_score_trims_trailing_zeros() {
        assert_eq!(format_ten_point_score(8.5), "8.5/10");
        assert_eq!(format_ten_point_score(9.0), "9/10");
        assert_eq!(format_ten_point_score(7.50), "7.5/10");
    }

    #[test]
    fn format_ten_point_score_rounds_to_two_decimal_places() {
        assert_eq!(format_ten_point_score(8.456), "8.46/10");
        assert_eq!(format_ten_point_score(8.444), "8.44/10");
    }

    // ─── build_description ────────────────────────────────────────

    #[test]
    fn build_description_joins_synopsis_and_background_with_blank_line() {
        let s = Some("The story of a girl.".to_string());
        let b = Some("Based on the novel by X.".to_string());
        let result = build_description(&s, &b);
        assert!(result.contains("The story of a girl."));
        assert!(result.contains("Based on the novel by X."));
    }

    #[test]
    fn build_description_omits_background_when_empty_or_whitespace() {
        let s = Some("synopsis only".to_string());
        let b = Some("   ".to_string());
        assert_eq!(build_description(&s, &b), "synopsis only");
    }

    #[test]
    fn build_description_returns_empty_when_both_missing() {
        assert_eq!(build_description(&None, &None), "");
    }

    // ─── estimate_next_airing ─────────────────────────────────────

    #[test]
    fn estimate_next_airing_fires_only_for_currently_airing_status() {
        // Only status strings containing "currently" (case-insensitive)
        // get the next-airing estimate — finished/upcoming shouldn't.
        assert!(estimate_next_airing(&Some("Currently Airing".into()), None).is_some());
        assert!(estimate_next_airing(&Some("Finished Airing".into()), None).is_none());
        assert!(estimate_next_airing(&None, None).is_none());
    }

    #[test]
    fn is_adult_rating_only_flags_rx() {
        assert!(is_adult_rating(Some("Rx - Hentai")));
        assert!(is_adult_rating(Some("  Rx - Hentai")));
        assert!(!is_adult_rating(Some("R+ - Mild Nudity")));
        assert!(!is_adult_rating(Some("R - 17+ (violence & profanity)")));
        assert!(!is_adult_rating(Some("PG-13 - Teens 13 or older")));
        assert!(!is_adult_rating(None));
    }
}
