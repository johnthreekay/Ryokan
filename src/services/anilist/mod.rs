use crate::services::html::sanitize_rich_description;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::services::jikan;

pub mod airing_schedules;
mod rate_limit;
#[cfg(any(test, feature = "test-support"))]
pub use rate_limit::reset_state_for_tests;
use rate_limit::{
    ANILIST_COOLDOWN_DEFAULT, AniListFailureKind, classify_anilist_failure, cooldown_from_headers,
    excerpt, extract_graphql_error, record_rate_limit_headers, set_anilist_cooldown,
    set_cooldown_until_now_plus, throttle_before_anilist_request,
};
pub use rate_limit::{
    anilist_cooldown_active, is_rate_limit_error, note_external_anilist_response,
    recent_al_request_count_60s,
};

const ANILIST_API_DEFAULT: &str = "https://graphql.anilist.co";

/// AL GraphQL endpoint, with a `RYOKAN_ANILIST_API_BASE` override
/// the same shape as `JIKAN_API_BASE`. Re-read on every call rather
/// than cached so tests can flip it per-fixture without process
/// restart; the env-var lookup is sub-microsecond and dwarfed by the
/// network round-trip that follows it.
fn anilist_api_base() -> String {
    std::env::var("RYOKAN_ANILIST_API_BASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ANILIST_API_DEFAULT.to_string())
}

/// TTL for the search result cache. Short enough to stay fresh, long enough to
/// absorb bursts of repeat queries (which is what actually hammers AniList/Jikan
/// during testing or when a user re-searches the same title).
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(60);

/// In-memory cache TTL for anime detail responses (15 minutes).
const DETAIL_CACHE_TTL_SECS: u64 = 15 * 60;

/// Maximum number of entries in the in-memory detail cache. When exceeded,
/// expired entries are evicted first; if still over limit the oldest entry
/// is removed.
const DETAIL_CACHE_MAX_ENTRIES: usize = 500;

/// In-memory cache for AniList detail responses to avoid rate limiting.
struct CacheEntry {
    detail: AnimeDetail,
    fetched_at: Instant,
}

static DETAIL_CACHE: LazyLock<RwLock<HashMap<i64, CacheEntry>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Shared reqwest client. Each `reqwest::Client::new()` call previously
/// rebuilt the TLS context and connection pool from scratch — wasteful
/// across the search / detail / fallback paths that all hit
/// graphql.anilist.co. Using a single Lazy client lets the pool reuse
/// connections across calls.
///
/// Timeouts: 10s to establish a TCP+TLS handshake, 30s overall per
/// request. Without an overall timeout a hung connection (e.g. half-
/// open after a network partition) pins a pool slot until kernel TCP
/// keepalive resolves, which on default Linux is roughly 2 hours —
/// long enough that interactive searches feel permanently broken even
/// after AL is healthy again. The 30s ceiling is generous relative to
/// AL's typical sub-second response time but still bounded; cooldown /
/// retry semantics live in the callers and are unaffected by this
/// per-attempt cap.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the AniList reqwest client should not fail")
});

type SearchCacheEntry = (Instant, Vec<AnimeEntry>);

/// Search result cache, keyed on (provider-mode, normalized query).
/// Provider-mode is "al" for the normal AniList-first path and "mal" for the
/// force_mal_fallback path; we keep them separate because they return different
/// `source` fields per entry and the frontend displays the distinction.
static SEARCH_CACHE: LazyLock<StdMutex<HashMap<String, SearchCacheEntry>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn normalize_search_key(force_fallback: bool, query: &str) -> String {
    let folded: String = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("{}::{}", if force_fallback { "mal" } else { "al" }, folded)
}

fn search_cache_get(key: &str) -> Option<Vec<AnimeEntry>> {
    let now = Instant::now();
    let mut cache = SEARCH_CACHE.lock().ok()?;
    if let Some((fetched_at, results)) = cache.get(key)
        && now.duration_since(*fetched_at) <= SEARCH_CACHE_TTL
    {
        return Some(results.clone());
    }
    cache.remove(key);
    None
}

fn search_cache_put(key: String, results: Vec<AnimeEntry>) {
    // Skip caching empty results. The four call sites that feed this all
    // produce empty Vecs under user-visible failure modes — AL in cooldown
    // with Jikan also empty, AL network error, AL 403/429/5xx, and the
    // "AL HTTP 200 with `data.Page.media: []`" branch which is the real
    // foot gun: during an AniList search-index outage every query returns
    // an empty success body. Caching it pinned the empty result for the
    // full TTL even after AL recovered, so the next legitimate retry hit
    // a stale 0 silently. A non-cached miss makes the user retype-loop
    // self-healing as soon as upstream comes back.
    if results.is_empty() {
        return;
    }
    if let Ok(mut cache) = SEARCH_CACHE.lock() {
        // Bound the cache. Simple heuristic — if we're >200 entries, drop expired
        // ones; if still too big, just clear. Search queries are long-tail anyway.
        if cache.len() > 200 {
            let now = Instant::now();
            cache.retain(|_, (t, _)| now.duration_since(*t) <= SEARCH_CACHE_TTL);
            if cache.len() > 200 {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), results));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AnimeEntry {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    pub source: String,
    /// Average viewer score on the provider's native scale: AL is
    /// 0-100, Jikan ingest multiplies by 10 to match. `None` for
    /// entries with no community score yet (unaired / recently
    /// added). Used by the Sonarr/Radarr shims to populate the
    /// `ratings` field — Sonarr expects a 0-10 float, so the
    /// downstream conversion divides by 10.
    #[serde(default)]
    pub average_score: Option<i32>,
}

/// One entry from a user's AniList watch list, projected to the
/// fields the watch-list sync (issue #62) cares about.
///
/// Every authenticated AL list query returns `MediaListCollection`
/// grouped by status (CURRENT / PLANNING / COMPLETED / DROPPED /
/// PAUSED / REPEATING) plus one bucket per custom list. We dedup
/// by `media_id` and read the per-entry `customLists` Json field
/// for membership rather than walking the custom-list buckets, so
/// the consumer sees a flat list with custom-list names attached.
#[derive(Debug, Clone)]
pub struct AniListMediaListEntry {
    /// AniList media id (positive). The Ryokan-internal series row's
    /// `anilist_id` is keyed off this for non-Jikan-fallback series.
    pub media_id: i64,
    /// Status string: `CURRENT`, `PLANNING`, `COMPLETED`, `DROPPED`,
    /// `PAUSED`, `REPEATING`. AL's enum on the wire; the sync engine
    /// maps it to Ryokan's monitor-mode defaults.
    pub status: String,
    /// Episodes the user has marked watched (per AL).
    pub progress: i64,
    /// User's score on AL's `scoreFormat`-relative scale. `0.0` means
    /// unrated; never render as "You: 0".
    pub score: f64,
    /// Unix epoch (seconds). The sync engine uses this for delta
    /// filtering against `external_accounts.list_last_synced_at`.
    pub updated_at: i64,
    pub notes: String,
    /// Names of custom lists this entry belongs to (empty if none).
    /// Pulled from AL's per-entry `customLists` Json field, which
    /// returns `{"List Name": true, ...}` — we keep names where the
    /// value is `true`.
    pub custom_lists: Vec<String>,
}

/// Fetch the full `MediaListCollection` for an authenticated user.
/// Returns one entry per (mediaId, status) — duplicates from the
/// custom-list buckets are deduped on the way out, with their custom-
/// list memberships merged onto the primary status entry.
///
/// AL's `MediaListCollection` query returns the entire list in a
/// single GraphQL response (no pagination at this layer), so this is
/// one HTTP request per call regardless of list size. Per-user-token
/// rate limits apply; the same `throttle_before_anilist_request` /
/// `record_rate_limit_headers` machinery the rest of `services::anilist`
/// uses keeps this in the existing rate-limit budget.
///
/// Errors classify the same way other AL calls do (rate-limited /
/// unavailable / not-found prefixes). The watch-list sync task
/// surfaces the message verbatim under `LogCategory::ExternalSync`.
/// Bundle returned by [`fetch_media_list_collection`] — the watch-
/// list entries plus the user's currently-configured `scoreFormat`.
/// The format is fetched in the same GraphQL call (rather than only
/// at link time) so the sync engine refreshes
/// `external_accounts.score_format` on every tick — a user changing
/// their POINT_X preference on AL after linking takes effect on the
/// next sync without forcing them to unlink + re-link.
#[derive(Debug, Clone)]
pub struct MediaListCollectionFetch {
    pub entries: Vec<AniListMediaListEntry>,
    pub score_format: String,
}

pub async fn fetch_media_list_collection(
    token: &str,
    user_id: i64,
) -> Result<MediaListCollectionFetch, String> {
    // Just the fields the sync engine reads. Each entry's `customLists`
    // is AL's per-entry membership map; the outer `lists[].isCustomList`
    // is the bucket flag we use to skip the custom-list duplicates.
    // `User.mediaListOptions.scoreFormat` lets the sync refresh the
    // local `score_format` column on every tick — that way switching
    // POINT_X on AL post-link takes effect on the next sync.
    const QUERY: &str = r#"
        query ($userId: Int!) {
            MediaListCollection(userId: $userId, type: ANIME) {
                lists {
                    name
                    isCustomList
                    entries {
                        mediaId
                        status
                        progress
                        score
                        updatedAt
                        notes
                        customLists
                    }
                }
                user {
                    mediaListOptions {
                        scoreFormat
                    }
                }
            }
        }
    "#;

    let body = serde_json::json!({
        "query": QUERY,
        "variables": { "userId": user_id },
    });

    throttle_before_anilist_request().await;

    let resp = HTTP_CLIENT
        .post(anilist_api_base())
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("User-Agent", "Ryokan/0.1")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("AniList MediaListCollection HTTP error: {e}"))?;

    let status = resp.status();
    record_rate_limit_headers(resp.headers());

    if !status.is_success() {
        // Same retry-after capture as the search/detail paths so
        // subsequent ticks don't pile on while AL is asking us to
        // back off.
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        // Capture the rate-limit headers verbatim so a 429 surfaces
        // AL's actual remaining/limit/reset alongside the body. The
        // 2026-05-03 user report had `MediaListCollection` 429-ing
        // while the unauthenticated search endpoint worked fine
        // simultaneously — most likely a per-account limit that's
        // stricter than the global 30/min, but without these
        // headers in the log we can't distinguish that from a
        // bad-token-misclassified-as-rate-limit case. A `Remaining: 0`
        // confirms a real rate limit; a `Remaining: 80` confirms
        // something else is going on (auth / per-endpoint quota /
        // Cloudflare). Helper at the bottom of this file.
        let rate_limit_summary = format_rate_limit_headers_for_log(resp.headers());
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            set_anilist_cooldown(retry_after_secs, ANILIST_COOLDOWN_DEFAULT);
        }
        let body = resp.text().await.unwrap_or_default();
        // OAuth2-shaped 400 responses with an `invalid_token` /
        // `invalid_grant` error code mean the token is dead — same
        // remediation as a 401/403 (user must re-link). AL's
        // GraphQL endpoint normally returns 200+errors[] for token
        // issues, but the upstream OAuth identity provider can
        // surface a 400 directly when the access token is malformed
        // or revoked at the identity layer. Without this branch the
        // failure routes through the generic "AniList unavailable"
        // path which is_auth_rejection treats as transient — the
        // Settings UI's "Re-link required" banner never fires and
        // the user keeps seeing failed-sync rows on every tick.
        let body_lower = body.to_ascii_lowercase();
        let is_oauth_token_400 = status == reqwest::StatusCode::BAD_REQUEST
            && (body_lower.contains("invalid_token") || body_lower.contains("invalid_grant"));
        return Err(match status.as_u16() {
            429 => format!(
                "AniList rate-limited (status 429): {} [{}]",
                excerpt(&body),
                rate_limit_summary
            ),
            401 | 403 => format!(
                "AniList rejected the watch-list token (status {}); user may need to re-link [{}]",
                status, rate_limit_summary
            ),
            400 if is_oauth_token_400 => format!(
                "AniList rejected the watch-list token (status 400, {}); user may need to re-link [{}]",
                if body_lower.contains("invalid_token") {
                    "invalid_token"
                } else {
                    "invalid_grant"
                },
                rate_limit_summary
            ),
            code => format!(
                "AniList unavailable (status {code}): {} [{}]",
                excerpt(&body),
                rate_limit_summary
            ),
        });
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("AniList MediaListCollection parse failed: {e}"))?;

    if let Some(err_msg) = extract_graphql_error(&json) {
        // GraphQL `errors[]` for an authenticated MediaListCollection
        // call typically means an expired or revoked token (AL replies
        // 200 with `errors[]`, not 401, when the bearer token is
        // invalid). Match the message-prefix taxonomy the rest of
        // services::anilist uses so downstream callers (sync task)
        // can react appropriately:
        //   * `AniList rate-limited` → next tick defers, stays linked
        //   * `AniList not found` / generic → surface as `unavailable`
        //   * Authorization-shaped messages stay as `unavailable` for
        //     now; the submit/refresh path will key off the literal
        //     message and surface a "re-link required" banner.
        let lower = err_msg.to_ascii_lowercase();
        if lower.contains("too many requests") || lower.contains("rate limit") {
            set_cooldown_until_now_plus(ANILIST_COOLDOWN_DEFAULT);
            return Err(format!("AniList rate-limited: {err_msg}"));
        }
        return Err(format!("AniList unavailable: {err_msg}"));
    }

    let lists = extract_media_list_buckets(&json)?;
    let out = parse_media_list_collection_lists(&lists);

    // Sanity log: zero entries kept across all non-custom-list buckets
    // when the response actually had buckets means either the user has
    // an empty list (legitimate) OR a future AL schema change made
    // every bucket `isCustomList: true` (which would silently produce
    // empty syncs forever). Surface the latter via a tracing warn so
    // it shows up in the operator's logs even when the sync looks
    // "successful" with zero results. Doesn't fail the call — an
    // empty list IS a valid state for new accounts.
    if out.is_empty() && all_buckets_are_custom_lists(&lists) {
        tracing::warn!(
            "AniList MediaListCollection returned {} buckets, all isCustomList=true; sync will see zero entries until the schema is investigated",
            lists.len()
        );
    }

    // Pull the user's current scoreFormat from the same response.
    // Empty string fallback if AL omits the field — the sync caller
    // treats empty as "leave the existing value alone" so we don't
    // wipe a known-good score_format on a partial response.
    let score_format = json
        .pointer("/data/MediaListCollection/user/mediaListOptions/scoreFormat")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(MediaListCollectionFetch {
        entries: out,
        score_format,
    })
}

/// Pull the `lists` array out of an AL `MediaListCollection` response.
///
/// AL serves three legitimate response shapes here, and only the
/// third is a real error:
///   1. `{ data: { MediaListCollection: { lists: [...], user: {...} } } }`
///      — normal populated response.
///   2. `{ data: { MediaListCollection: null } }` — accounts with zero
///      anime entries (brand-new users, or someone who cleared their
///      list). AL omits the wrapping object entirely rather than
///      returning an empty `lists` array.
///   3. `data` missing entirely — malformed / partial response, the
///      only case worth surfacing as an error.
///
/// Without the (2) branch, brand-new accounts (and anyone whose
/// imported categories happen to all be empty) hit "missing
/// data.MediaListCollection.lists" and the manual Sync now button
/// fails instead of succeeding gracefully with 0 entries.
fn extract_media_list_buckets(json: &serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
    if json.pointer("/data").is_none() {
        return Err("AniList MediaListCollection missing data".to_string());
    }
    Ok(json
        .pointer("/data/MediaListCollection/lists")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Walk only the non-custom-list buckets to avoid double-counting
/// entries that appear under both their primary status and one or
/// more custom-list buckets. Per-entry `customLists` Json gives us
/// membership directly, so we don't need the bucket walk for that.
///
/// Pure helper extracted from `fetch_media_list_collection` so the
/// parse logic + the all-isCustomList sanity branch are unit-testable
/// without standing up an HTTP mock against `graphql.anilist.co`.
fn parse_media_list_collection_lists(lists: &[serde_json::Value]) -> Vec<AniListMediaListEntry> {
    let mut out: Vec<AniListMediaListEntry> = Vec::new();
    for list in lists {
        if list
            .get("isCustomList")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let entries = match list.get("entries").and_then(|v| v.as_array()) {
            Some(e) => e,
            None => continue,
        };
        for entry in entries {
            let media_id = match entry.get("mediaId").and_then(|v| v.as_i64()) {
                Some(id) => id,
                None => continue,
            };
            let status = entry
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let progress = entry.get("progress").and_then(|v| v.as_i64()).unwrap_or(0);
            let score = entry.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let updated_at = entry.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            let notes = entry
                .get("notes")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let custom_lists = entry
                .get("customLists")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(name, on)| {
                            if on.as_bool().unwrap_or(false) {
                                Some(name.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            out.push(AniListMediaListEntry {
                media_id,
                status,
                progress,
                score,
                updated_at,
                notes,
                custom_lists,
            });
        }
    }
    out
}

/// True when every entry in `lists` is flagged `isCustomList: true`.
/// Empty `lists` returns false — that's the "no buckets at all" case
/// (a brand-new AL account with no list activity), not the "schema
/// drift" signal the sanity warning fires for.
fn all_buckets_are_custom_lists(lists: &[serde_json::Value]) -> bool {
    if lists.is_empty() {
        return false;
    }
    lists.iter().all(|l| {
        l.get("isCustomList")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    })
}

/// Search AniList for anime by title, falling back to MAL/Jikan if AniList 403s.
pub async fn search_anime(query: &str) -> Result<Vec<AnimeEntry>, String> {
    search_anime_with_options(query, false).await
}

pub async fn search_anime_with_options(
    query: &str,
    force_mal_fallback: bool,
) -> Result<Vec<AnimeEntry>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Cache lookup — skip all upstream work for repeat queries within the TTL.
    let cache_key = normalize_search_key(force_mal_fallback, query);
    if let Some(cached) = search_cache_get(&cache_key) {
        tracing::debug!(
            "anilist search cache hit for {:?} ({} results)",
            query,
            cached.len()
        );
        return Ok(cached);
    }

    if force_mal_fallback {
        let results = fallback_jikan(query, None).await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    // 2. If AniList is known to be rate-limited, don't bother hitting it — go
    //    straight to Jikan. This is the key fix for the "both APIs rate-limited
    //    at once" symptom: previously every search during the 60s AL cooldown
    //    still pinged AL, got another 429, then called Jikan, burning Jikan's
    //    (stricter) rate-limit budget alongside.
    if anilist_cooldown_active() {
        tracing::debug!(
            "anilist search skipping AniList for {:?} (still in cooldown)",
            query
        );
        let results = fallback_jikan(
            query,
            Some("AniList rate-limited (skipped during cooldown)".to_string()),
        )
        .await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    let gql = serde_json::json!({
        "query": r#"
            query ($search: String) {
                Page(page: 1, perPage: 10) {
                    media(search: $search, type: ANIME, sort: SEARCH_MATCH) {
                        id
                        idMal
                        title {
                            romaji
                            english
                            native
                        }
                        coverImage {
                            large
                        }
                        format
                        status
                        episodes
                        isAdult
                        seasonYear
                        averageScore
                    }
                }
            }
        "#,
        "variables": { "search": query }
    });

    // Pace via the same shared rate-limit state that fetch_anime_detail
    // uses — search-path 429s would otherwise leave the detail-path
    // throttle decisions working off a stale `remaining`.
    throttle_before_anilist_request().await;

    let client = &*HTTP_CLIENT;
    let resp = match client
        .post(anilist_api_base())
        .header("User-Agent", "Ryokan/0.1")
        .json(&gql)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "AniList request failed for query {:?}: {}; falling back to Jikan/MAL",
                query,
                e
            );
            let results =
                fallback_jikan(query, Some(format!("AniList unreachable: {}", e))).await?;
            search_cache_put(cache_key, results.clone());
            return Ok(results);
        }
    };

    let status = resp.status();
    record_rate_limit_headers(resp.headers());

    // Silently fall back to Jikan/MAL on transient AniList outages:
    //   403 — Cloudflare challenge / geo-block
    //   429 — rate limit (30 req/min anon)
    //   5xx — upstream outage
    // These are the cases where the user's search should just Work via a
    // fallback provider rather than surfacing a cryptic HTTP error.
    if status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        tracing::warn!(
            "AniList search HTTP {} for query {:?} (retry-after={:?}); falling back to Jikan/MAL",
            status,
            query,
            retry_after_secs
        );
        // Start a cooldown so subsequent searches in this window skip
        // AL entirely. 403 (Cloudflare challenge) is the most common
        // AniList outage mode; without this branch, every request kept
        // round-tripping through AL just to bounce on the 403 again
        // before falling back to Jikan. Cloudflare doesn't include
        // Retry-After, so pick a longer default — 60s rarely outlasts
        // a real challenge — and let ANILIST_COOLDOWN_MAX cap it.
        let default_cooldown = if status == reqwest::StatusCode::FORBIDDEN {
            Duration::from_secs(300)
        } else {
            ANILIST_COOLDOWN_DEFAULT
        };
        set_anilist_cooldown(retry_after_secs, default_cooldown);
        let reason = match status.as_u16() {
            429 => format!(
                "AniList rate-limited{}",
                retry_after_secs
                    .map(|r| format!(" (retry in {}s)", r))
                    .unwrap_or_default()
            ),
            403 => "AniList blocked our request (Cloudflare challenge)".to_string(),
            code => format!("AniList upstream error (HTTP {})", code),
        };
        let results = fallback_jikan(query, Some(reason)).await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    // Read the body as text first so a non-JSON error body (common on 4xx/5xx)
    // produces a useful error instead of "Failed to parse AniList response".
    let body_text = resp
        .text()
        .await
        .map_err(|e| format!("AniList response read failed (HTTP {}): {}", status, e))?;

    let body: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(parse_err) => {
            if !status.is_success() {
                let snippet: String = body_text.chars().take(200).collect();
                return Err(format!(
                    "AniList search failed (HTTP {}): {}",
                    status,
                    snippet.trim()
                ));
            }
            return Err(format!("Failed to parse AniList response: {}", parse_err));
        }
    };

    if !status.is_success() {
        let msg = extract_graphql_error(&body).unwrap_or_else(|| body.to_string());
        return Err(format!("AniList search failed (HTTP {}): {}", status, msg));
    }

    if let Some(msg) = extract_graphql_error(&body) {
        return Err(format!("AniList search failed: {}", msg));
    }

    let media = match body["data"]["Page"]["media"].as_array() {
        Some(arr) => arr,
        None => {
            // Schema mismatch — `data.Page.media` is missing or not an
            // array. Don't cache the empty result here: a legitimate
            // 0-hit search hits the Some branch with an empty arr and
            // *does* get cached at line 317 below. Caching the
            // schema-mismatch case would lock us out of fresh requests
            // for SEARCH_CACHE_TTL even after AniList recovers.
            tracing::warn!(
                target: "ryokan::anilist",
                query = %query,
                "AniList response missing data.Page.media; not caching empty result"
            );
            return Ok(Vec::new());
        }
    };

    let entries: Vec<AnimeEntry> = media
        .iter()
        .filter_map(|m| {
            // Drop entries with a missing/non-numeric id rather than
            // collapsing them to 0. A 0-id record would slip past the
            // `id > 0` filters elsewhere only by virtue of being equal
            // to 0 (which is filtered out), but it can still leak into
            // SEARCH_CACHE for SEARCH_CACHE_TTL and confuse the UI.
            let id = m["id"].as_i64().filter(|&n| n > 0)?;
            Some(AnimeEntry {
                id,
                id_mal: m["idMal"].as_i64(),
                title_romaji: m["title"]["romaji"].as_str().unwrap_or("").to_string(),
                title_english: m["title"]["english"].as_str().unwrap_or("").to_string(),
                title_native: m["title"]["native"].as_str().unwrap_or("").to_string(),
                cover_url: m["coverImage"]["large"].as_str().unwrap_or("").to_string(),
                format: m["format"].as_str().unwrap_or("").to_string(),
                status: m["status"].as_str().unwrap_or("").to_string(),
                status_display: prettify_status(m["status"].as_str().unwrap_or("")),
                episodes: m["episodes"].as_i64().filter(|&n| n > 0).map(|e| e as i32),
                season_year: m["seasonYear"].as_i64().map(|y| y as i32),
                source: "anilist".to_string(),
                average_score: m["averageScore"]
                    .as_i64()
                    .filter(|&n| n > 0)
                    .map(|s| s as i32),
            })
        })
        .collect();

    search_cache_put(cache_key, entries.clone());
    Ok(entries)
}

/// Shared fallback helper used when AniList is unavailable or force_mal_fallback is set.
/// Tries Jikan (MAL-backed). If Jikan also fails, `compose_search_error` builds a
/// user-friendly combined message for the frontend.
async fn fallback_jikan(
    query: &str,
    anilist_reason: Option<String>,
) -> Result<Vec<AnimeEntry>, String> {
    match jikan::search_anime(query).await {
        Ok(results) => Ok(results),
        Err(jikan_err) => Err(compose_search_error(anilist_reason.as_deref(), &jikan_err)),
    }
}

/// Produce a clean, human-readable error message from an AniList failure reason
/// (optional — e.g. "AniList rate-limited (retry in 28s)") and a Jikan failure
/// reason. Callers see something like:
///   "Both AniList and Jikan/MAL are rate-limited right now. Try again in ~30s."
/// instead of a raw JSON dump concatenation.
fn compose_search_error(anilist_reason: Option<&str>, jikan_err: &str) -> String {
    let al_rate_limited = anilist_reason
        .map(|r| r.contains("rate-limited") || r.contains("429"))
        .unwrap_or(false);
    let jikan_rate_limited = jikan_err.contains("rate-limited") || jikan_err.contains("429");

    if al_rate_limited && jikan_rate_limited {
        // Try to surface the AL retry hint if we parsed one earlier.
        let hint = anilist_reason
            .and_then(|r| {
                let start = r.find("retry in ")?;
                let tail = &r[start + "retry in ".len()..];
                let end = tail.find(')').unwrap_or(tail.len());
                Some(tail[..end].to_string())
            })
            .map(|s| format!(" Try again in ~{}.", s))
            .unwrap_or_else(|| " Try again in a minute.".to_string());
        return format!(
            "Both AniList and Jikan/MAL are rate-limited right now.{}",
            hint
        );
    }

    match anilist_reason {
        Some(al) => format!("{}. MAL/Jikan fallback also failed: {}", al, jikan_err),
        None => format!("MAL/Jikan search failed: {}", jikan_err),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RelatedEntry {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub relation_type: String,
    pub season_year: Option<i32>,
    pub media_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StreamingEpisode {
    pub title: String,
    pub thumbnail: String,
    pub url: String,
    pub site: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AnimeDetail {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub banner_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub duration: Option<i32>,
    pub season: String,
    pub season_year: Option<i32>,
    /// AniList `endDate.year` when the show has finished. `#[serde(default)]`
    /// so cached JSON blobs from before this field existed deserialize
    /// cleanly to `None`. Consumed by Layer 4 temporal inference.
    #[serde(default)]
    pub end_year: Option<i32>,
    /// AniList `isAdult` (Jikan: a `rating` starting with `Rx`; Kitsu:
    /// `nsfw`). `#[serde(default)]` so cached blobs from before the
    /// field existed deserialize to `false`. Nyaa lists adult releases
    /// on sukebei, which Ryokan does not search, so this mostly explains
    /// why auto-search finds nothing for a title (issue #219).
    #[serde(default)]
    pub is_adult: bool,
    pub description: String,
    pub genres: Vec<String>,
    pub average_score: Option<i32>,
    pub average_score_display: Option<String>,
    pub score_is_ten_point: bool,
    pub score_class: String,
    pub next_airing_episode: Option<i32>,
    pub next_airing_at: Option<i64>,
    pub synonyms: Vec<String>,
    pub streaming_episodes: Vec<StreamingEpisode>,
    pub relations: Vec<RelatedEntry>,
}

impl AnimeDetail {
    /// Effective episode count for rendering, episode cache building, and
    /// monitoring. AniList reports `episodes: null` for currently-airing
    /// series because the final count isn't known yet, so we fall back to
    /// `nextAiringEpisode - 1` (the number of episodes that have already
    /// aired). Without this every airing show looks like it has zero
    /// episodes, which breaks the episode list and the monitoring UI.
    pub fn effective_episode_count(&self) -> i32 {
        match self.episodes.unwrap_or(0) {
            0 => self
                .next_airing_episode
                .map(|n| (n - 1).max(0))
                .unwrap_or(0),
            n => n,
        }
    }

    /// True when the series has finished airing (or was cancelled) per any
    /// of the three metadata providers' vocabularies. AniList uses
    /// `FINISHED` / `CANCELLED`, Jikan normalizes "Finished Airing" →
    /// `FINISHED_AIRING`, and Kitsu uses `FINISHED`. Without this helper
    /// the callsites that just compared against the literal `"FINISHED"`
    /// string silently misclassified every Jikan-fed series as "still
    /// airing", breaking the finished-mode BD probe and the 2-year
    /// sequel-rejection filter whenever the AniList fallback kicked in.
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status.as_str(),
            "FINISHED" | "FINISHED_AIRING" | "CANCELLED"
        )
    }
}

fn prettify_status(status: &str) -> String {
    status.replace('_', " ")
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

pub async fn get_anime_detail(id: i64) -> Result<AnimeDetail, String> {
    get_anime_detail_with_options(id, None, false).await
}

/// Read-only probe of the in-process detail cache. Returns the entry
/// only if it's present AND still fresh (within `DETAIL_CACHE_TTL_SECS`).
/// Used by callers that want to recover partial results from a batch
/// fetch's error path (e.g. transitive relation walks where chunk 1
/// succeeded and seeded the cache before chunk 2 hit a 429).
pub async fn cached_anime_detail(id: i64) -> Option<AnimeDetail> {
    let cache = DETAIL_CACHE.read().await;
    cache.get(&id).and_then(|entry| {
        if entry.fetched_at.elapsed().as_secs() < DETAIL_CACHE_TTL_SECS {
            Some(entry.detail.clone())
        } else {
            None
        }
    })
}

pub async fn get_anime_detail_with_options(
    id: i64,
    mal_id_hint: Option<i64>,
    force_mal_fallback: bool,
) -> Result<AnimeDetail, String> {
    if id < 0 {
        return jikan::get_anime_detail_cached(-id).await;
    }
    if force_mal_fallback && let Some(mid) = mal_id_hint {
        return jikan::get_anime_detail_cached(mid).await;
    }

    {
        let cache = DETAIL_CACHE.read().await;
        if let Some(entry) = cache.get(&id)
            && entry.fetched_at.elapsed().as_secs() < DETAIL_CACHE_TTL_SECS
        {
            return Ok(entry.detail.clone());
        }
    }

    let detail = fetch_anime_detail(id).await?;

    {
        let mut cache = DETAIL_CACHE.write().await;
        cache.insert(
            id,
            CacheEntry {
                detail: detail.clone(),
                fetched_at: Instant::now(),
            },
        );
        // Evict stale/oldest entries when the cache grows too large.
        if cache.len() > DETAIL_CACHE_MAX_ENTRIES {
            let expired: Vec<i64> = cache
                .iter()
                .filter(|(_, e)| e.fetched_at.elapsed().as_secs() >= DETAIL_CACHE_TTL_SECS)
                .map(|(k, _)| *k)
                .collect();
            for k in &expired {
                cache.remove(k);
            }
            // If still over limit, drop the oldest entry.
            if cache.len() > DETAIL_CACHE_MAX_ENTRIES
                && let Some((&oldest_key, _)) = cache.iter().min_by_key(|(_, e)| e.fetched_at)
            {
                cache.remove(&oldest_key);
            }
        }
    }

    Ok(detail)
}

/// What to filter the `Media` query by. AniList's `Media` resolver
/// accepts `id` and `idMal` as independent filters, so a single query
/// shape covers both lookup styles by passing the unused argument as
/// `null` in the variables. Lets `find_anime_detail_by_mal_id` reuse
/// the full field selection without duplicating the query body.
#[derive(Debug, Clone, Copy)]
enum MediaSelector {
    Id(i64),
    IdMal(i64),
}

async fn fetch_anime_detail(id: i64) -> Result<AnimeDetail, String> {
    fetch_media_detail(MediaSelector::Id(id))
        .await?
        .ok_or_else(|| "Anime not found".to_string())
}

/// Build the GraphQL `variables` map for the shared `Media(id:, idMal:)`
/// query. AniList's resolver treats an explicit `id: null` (or
/// `idMal: null`) as "filter where the field equals null" and returns
/// 404, so the unused arm of [`MediaSelector`] must be **omitted** from
/// the variables map (sent as undefined), not sent as JSON null.
/// Verified live 2026-04-19: `{id: 1, idMal: null}` → "Not Found";
/// `{id: 1}` → Cowboy Bebop. Tested in `media_selector_omits_unused_var`.
fn build_media_selector_variables(
    selector: MediaSelector,
) -> serde_json::Map<String, serde_json::Value> {
    let mut variables = serde_json::Map::new();
    match selector {
        MediaSelector::Id(v) => {
            variables.insert("id".to_string(), serde_json::json!(v));
        }
        MediaSelector::IdMal(v) => {
            variables.insert("idMal".to_string(), serde_json::json!(v));
        }
    }
    variables
}

async fn fetch_media_detail(selector: MediaSelector) -> Result<Option<AnimeDetail>, String> {
    // Skip the round trip entirely when a recent 429/403/5xx has tripped
    // the global cooldown. Without this, a metadata-refresh sweep that
    // hits AniList's per-minute cap on the first burst keeps firing
    // request after request — each one immediately bouncing on 429 —
    // for the full duration of the sweep, even though we already know
    // AniList is rate-limiting us. The error string flows up through
    // metadata_sync's fallback chain and the warn log added in PR #31
    // surfaces the cooldown state to the operator.
    if anilist_cooldown_active() {
        // Wording note: "skipping AniList request" — only the AniList
        // round trip is skipped here. The caller's fallback chain
        // (jikan/MAL → kitsu) still runs and may produce the detail
        // from a different provider.
        return Err("AniList rate-limit cooldown active; skipping AniList request".to_string());
    }
    let variables = build_media_selector_variables(selector);
    let gql = serde_json::json!({
        "query": r#"
            query ($id: Int, $idMal: Int) {
                Media(id: $id, idMal: $idMal, type: ANIME) {
                    id
                    idMal
                    title { romaji english native }
                    synonyms
                    coverImage { large extraLarge }
                    bannerImage
                    format
                    status
                    episodes
                    isAdult
                    duration
                    season
                    seasonYear
                    endDate { year }
                    description(asHtml: true)
                    genres
                    averageScore
                    nextAiringEpisode {
                        episode
                        airingAt
                    }
                    streamingEpisodes {
                        title
                        thumbnail
                        url
                        site
                    }
                    relations {
                        edges {
                            relationType(version: 2)
                            node {
                                id
                                idMal
                                title { romaji english native }
                                format
                                status
                                episodes
                                coverImage { large }
                                type
                                seasonYear
                            }
                        }
                    }
                }
            }
        "#,
        "variables": variables
    });

    // Pace the request based on the latest X-RateLimit-Remaining /
    // X-RateLimit-Reset we've seen. This is the primary defense against
    // 429s — by the time AL hands back a 429 we've already wasted the
    // round trip; throttling proactively keeps the sweep inside AL's
    // window and burst limits.
    throttle_before_anilist_request().await;

    let client = &*HTTP_CLIENT;
    let resp = client
        .post(anilist_api_base())
        .header("User-Agent", "Ryokan/0.1")
        .json(&gql)
        .send()
        .await
        .map_err(|e| format!("AniList request failed: {}", e))?;

    let status = resp.status();
    // Headers carry both the rate-limit snapshot (used by future
    // throttles) and Retry-After / X-RateLimit-Reset for cooldown
    // computation. Clone so we can use them after the body has been
    // consumed.
    let headers = resp.headers().clone();
    record_rate_limit_headers(&headers);

    // Read as text first (not .json()) so a Cloudflare HTML challenge
    // doesn't blow up at the parse step — we need the body to classify
    // the failure correctly.
    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            // Body-read failure: the status header was already received,
            // so preserve the rate-limit signal when the status itself
            // told us we were throttled. Without this branch a connection
            // reset partway through a 429 body would erase the throttle
            // signal — `is_rate_limit_error` returns false, the caller
            // happily MAL-falls-back, and the whole "no MAL on rate-limit"
            // invariant collapses on a flaky network.
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let dur = cooldown_from_headers(&headers, ANILIST_COOLDOWN_DEFAULT);
                set_cooldown_until_now_plus(dur);
                return Err(format!(
                    "AniList rate-limited (HTTP 429): body read failed: {}",
                    e
                ));
            }
            return Err(format!(
                "AniList unavailable: failed to read response: {}",
                e
            ));
        }
    };

    if !status.is_success() {
        let (kind, msg) = classify_anilist_failure(status, &body_text);
        // Cooldown only on real throttling. 5xx and AL-side 403s are
        // "AL is down" — letting them set the cooldown would convert
        // subsequent calls into deferred-rate-limit errors and prevent
        // the MAL fallback the caller actually wants.
        if kind == AniListFailureKind::RateLimited {
            // 403 (Cloudflare) doesn't include Retry-After, so pick a
            // longer default — 60s rarely outlasts a real challenge —
            // and let ANILIST_COOLDOWN_MAX cap it. Only Cloudflare 403s
            // reach this branch (non-CF 403s classify as Unavailable).
            let default_cooldown = if status == reqwest::StatusCode::FORBIDDEN {
                Duration::from_secs(300)
            } else {
                ANILIST_COOLDOWN_DEFAULT
            };
            let dur = cooldown_from_headers(&headers, default_cooldown);
            set_cooldown_until_now_plus(dur);
        }
        return Err(msg);
    }

    let body: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
        format!(
            "AniList unavailable: parse error: {} (body: {})",
            e,
            excerpt(&body_text)
        )
    })?;

    if extract_graphql_error(&body).is_some() {
        // Run the classifier even on 2xx responses: AL has been observed
        // to return throttle messages in the GraphQL `errors[]` array
        // with a 200 status (no 429 at the transport layer). Without
        // this branch we'd surface a generic "AniList detail failed"
        // that doesn't match `is_rate_limit_error`, the caller would
        // MAL-fall-back, and the cooldown wouldn't trigger to short-
        // circuit the rest of the sweep.
        let (kind, msg) = classify_anilist_failure(status, &body_text);
        if kind == AniListFailureKind::RateLimited {
            let dur = cooldown_from_headers(&headers, ANILIST_COOLDOWN_DEFAULT);
            set_cooldown_until_now_plus(dur);
        }
        return Err(msg);
    }

    let m = &body["data"]["Media"];
    if m.is_null() {
        return Ok(None);
    }

    Ok(parse_media_node(m))
}

/// Convert a single Media node from the AniList GraphQL response into
/// `AnimeDetail`. Used by both the single-id `fetch_media_detail` path
/// and the batched `get_anime_details_batch` path so the field plucking
/// logic only lives in one place. Returns `None` when the `id` field is
/// missing or non-numeric — every downstream consumer requires `id > 0`,
/// so a 0-id placeholder would just leak into caches and confuse later
/// `id > 0` filters into thinking the entry is a Jikan-fallback row.
fn parse_media_node(m: &serde_json::Value) -> Option<AnimeDetail> {
    let streaming_episodes = m["streamingEpisodes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|ep| StreamingEpisode {
                    title: ep["title"].as_str().unwrap_or("").to_string(),
                    thumbnail: ep["thumbnail"].as_str().unwrap_or("").to_string(),
                    url: ep["url"].as_str().unwrap_or("").to_string(),
                    site: ep["site"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let relations = m["relations"]["edges"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|edge| {
                    let node = &edge["node"];
                    Some(RelatedEntry {
                        id: node["id"].as_i64()?,
                        id_mal: node["idMal"].as_i64(),
                        title_romaji: node["title"]["romaji"].as_str().unwrap_or("").to_string(),
                        title_english: node["title"]["english"].as_str().unwrap_or("").to_string(),
                        title_native: node["title"]["native"].as_str().unwrap_or("").to_string(),
                        cover_url: node["coverImage"]["large"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                        format: node["format"].as_str().unwrap_or("").to_string(),
                        status: node["status"].as_str().unwrap_or("").to_string(),
                        status_display: prettify_status(node["status"].as_str().unwrap_or("")),
                        episodes: node["episodes"]
                            .as_i64()
                            .filter(|&n| n > 0)
                            .map(|e| e as i32),
                        relation_type: edge["relationType"].as_str().unwrap_or("").to_string(),
                        season_year: node["seasonYear"].as_i64().map(|y| y as i32),
                        media_type: node["type"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let id = m["id"].as_i64().filter(|&n| n > 0)?;
    Some(AnimeDetail {
        is_adult: m["isAdult"].as_bool().unwrap_or(false),
        id,
        id_mal: m["idMal"].as_i64(),
        title_romaji: m["title"]["romaji"].as_str().unwrap_or("").to_string(),
        title_english: m["title"]["english"].as_str().unwrap_or("").to_string(),
        title_native: m["title"]["native"].as_str().unwrap_or("").to_string(),
        cover_url: m["coverImage"]["extraLarge"]
            .as_str()
            .or_else(|| m["coverImage"]["large"].as_str())
            .unwrap_or("")
            .to_string(),
        banner_url: m["bannerImage"].as_str().unwrap_or("").to_string(),
        format: m["format"].as_str().unwrap_or("").to_string(),
        status: m["status"].as_str().unwrap_or("").to_string(),
        episodes: m["episodes"].as_i64().filter(|&n| n > 0).map(|e| e as i32),
        duration: m["duration"].as_i64().map(|d| d as i32),
        season: m["season"].as_str().unwrap_or("").to_string(),
        season_year: m["seasonYear"].as_i64().map(|y| y as i32),
        end_year: m["endDate"]["year"].as_i64().map(|y| y as i32),
        description: sanitize_rich_description(m["description"].as_str().unwrap_or(""), true),
        genres: m["genres"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|g| g.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        average_score: m["averageScore"].as_i64().map(|s| s as i32),
        average_score_display: m["averageScore"].as_i64().map(|s| format!("{}%", s)),
        score_is_ten_point: false,
        score_class: score_class(m["averageScore"].as_i64().map(|s| s as i32), false),
        status_display: prettify_status(m["status"].as_str().unwrap_or("")),
        next_airing_episode: m["nextAiringEpisode"]["episode"].as_i64().map(|e| e as i32),
        next_airing_at: m["nextAiringEpisode"]["airingAt"].as_i64(),
        synonyms: m["synonyms"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(|v| v.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        streaming_episodes,
        relations,
    })
}

/// Look up an anime by MAL id and return the full `AnimeDetail` payload
/// in one round-trip — replacing the previous `find_anime_by_mal_id` +
/// `get_anime_detail` two-step. The caller (reconciliation path in
/// library.rs) already needed the full payload anyway; AniList accepts
/// `idMal` on the same `Media` query that returns full detail, so the
/// extra "find then fetch" round-trip was wasted.
///
/// Populates `DETAIL_CACHE` keyed by the resolved AniList id on success
/// so that downstream `get_anime_detail(detail.id)` calls in the same
/// TTL window (Sonarr/Radarr fan-outs, metadata_sync BFS, relation
/// walks) see a cache hit. The pre-PR two-step had this side-effect
/// for free because the second leg went through `get_anime_detail`;
/// the new one-shot helper has to do it explicitly.
pub async fn find_anime_detail_by_mal_id(mal_id: i64) -> Result<Option<AnimeDetail>, String> {
    let result = fetch_media_detail(MediaSelector::IdMal(mal_id)).await?;
    if let Some(detail) = &result
        && detail.id > 0
    {
        let mut cache = DETAIL_CACHE.write().await;
        cache.insert(
            detail.id,
            CacheEntry {
                detail: detail.clone(),
                fetched_at: Instant::now(),
            },
        );
    }
    Ok(result)
}

/// Maximum AniList ids to ask for in a single `Page(media(id_in:[]))`
/// batched detail request. AniList paginates `Page` at perPage=50, but
/// the binding constraint is GraphQL complexity: each `Media` carries a
/// `relations { edges { node {...} } }` block, and 50 × ~10 relations ×
/// edge complexity easily exceeds the documented complexity cap.
/// 25 keeps us comfortably under the cap with full relations included,
/// which matters for the BFS hydrator that needs the next layer of
/// relations on every node it processes.
const ANILIST_BATCH_SIZE: usize = 25;

/// Fetch full `AnimeDetail` payloads for many AniList ids in one
/// `Page(media(id_in:[...]))` request — replacing the historical
/// "loop and call `get_anime_detail` per id" pattern in the metadata
/// BFS, the relation transitive walk, and the Sonarr/Radarr
/// compatibility shims.
///
/// Behavior:
/// - Ids are deduplicated and chunked at [`ANILIST_BATCH_SIZE`]; the
///   helper returns `ceil(N / ANILIST_BATCH_SIZE)` requests' worth of
///   data instead of N.
/// - Each chunk passes through the same cooldown gate, throttle, and
///   rate-limit-header capture as `fetch_media_detail`.
/// - Successful responses populate `DETAIL_CACHE` so subsequent
///   single-id `get_anime_detail` calls for the same ids are cache
///   hits.
/// - On a chunk-level error, processing aborts and `Err(msg)` is
///   returned. The accumulated map is dropped, but `DETAIL_CACHE`
///   already received the per-id writes from any chunks that
///   completed before the failure — callers that want to use those
///   partial results on `Err` must probe `DETAIL_CACHE` per-id
///   themselves (the `Result` shape can only carry one variant).
///   The global cooldown will already be set (via
///   `record_rate_limit_headers` / 429 handling) so retrying the
///   remaining chunks would just bounce immediately anyway.
/// - Negative-result ids (AL had no Media for them) simply don't
///   appear in the output map — callers must check `map.get(id)`.
pub async fn get_anime_details_batch(ids: &[i64]) -> Result<HashMap<i64, AnimeDetail>, String> {
    // Dedup + drop non-positive ids (negative ids are MAL-fallback
    // synthetic markers and should hit the Jikan path, not AniList).
    let unique_ids: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();
    if unique_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut out: HashMap<i64, AnimeDetail> = HashMap::with_capacity(unique_ids.len());
    let client = &*HTTP_CLIENT;

    for chunk in unique_ids.chunks(ANILIST_BATCH_SIZE) {
        if anilist_cooldown_active() {
            return Err("AniList rate-limit cooldown active; skipping AniList request".to_string());
        }

        let gql = serde_json::json!({
            // Inject ANILIST_BATCH_SIZE into the query so the const and the
            // GraphQL `perPage` literal can't drift apart silently — bumping
            // the const used to leave the query truncating to the old value
            // and the extra ids would just disappear from the response.
            "query": format!(r#"
                query ($ids: [Int]) {{
                    Page(perPage: {batch_size}) {{
                        media(id_in: $ids, type: ANIME) {{
                            id
                            idMal
                            title {{ romaji english native }}
                            synonyms
                            coverImage {{ large extraLarge }}
                            bannerImage
                            format
                            status
                            episodes
                            isAdult
                            duration
                            season
                            seasonYear
                            endDate {{ year }}
                            description(asHtml: true)
                            genres
                            averageScore
                            nextAiringEpisode {{ episode airingAt }}
                            streamingEpisodes {{ title thumbnail url site }}
                            relations {{
                                edges {{
                                    relationType(version: 2)
                                    node {{
                                        id
                                        idMal
                                        title {{ romaji english native }}
                                        format
                                        status
                                        episodes
                                        coverImage {{ large }}
                                        type
                                        seasonYear
                                    }}
                                }}
                            }}
                        }}
                    }}
                }}
            "#, batch_size = ANILIST_BATCH_SIZE),
            "variables": { "ids": chunk }
        });

        throttle_before_anilist_request().await;

        let resp = client
            .post(anilist_api_base())
            .header("User-Agent", "Ryokan/0.1")
            .json(&gql)
            .send()
            .await
            .map_err(|e| format!("AniList batch request failed: {}", e))?;

        let status = resp.status();
        let headers = resp.headers().clone();
        record_rate_limit_headers(&headers);

        let body_text = resp
            .text()
            .await
            .map_err(|e| format!("AniList batch unavailable: failed to read response: {}", e))?;

        if !status.is_success() {
            let (kind, msg) = classify_anilist_failure(status, &body_text);
            if kind == AniListFailureKind::RateLimited {
                let default_cooldown = if status == reqwest::StatusCode::FORBIDDEN {
                    Duration::from_secs(300)
                } else {
                    ANILIST_COOLDOWN_DEFAULT
                };
                let dur = cooldown_from_headers(&headers, default_cooldown);
                set_cooldown_until_now_plus(dur);
            }
            return Err(msg);
        }

        let body: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
            format!(
                "AniList batch parse error: {} (body: {})",
                e,
                excerpt(&body_text)
            )
        })?;

        if extract_graphql_error(&body).is_some() {
            let (kind, msg) = classify_anilist_failure(status, &body_text);
            if kind == AniListFailureKind::RateLimited {
                let dur = cooldown_from_headers(&headers, ANILIST_COOLDOWN_DEFAULT);
                set_cooldown_until_now_plus(dur);
            }
            return Err(msg);
        }

        let media_arr = body["data"]["Page"]["media"].as_array();
        if let Some(media) = media_arr {
            // Eagerly populate DETAIL_CACHE so subsequent single-id
            // `get_anime_detail` calls for these ids hit the cache.
            let mut cache = DETAIL_CACHE.write().await;
            for node in media {
                let Some(detail) = parse_media_node(node) else {
                    continue;
                };
                cache.insert(
                    detail.id,
                    CacheEntry {
                        detail: detail.clone(),
                        fetched_at: Instant::now(),
                    },
                );
                out.insert(detail.id, detail);
            }
            // Light eviction — same shape as the single-id path so a
            // big batch can't unbounded-grow the cache. Drop expired
            // entries first; if still over cap (typical when many
            // batches in a row insert fresh ids), drop the oldest
            // entry. Without this oldest-drop fallback, the cache
            // grows monotonically until the TTL window finally ticks.
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
    }

    Ok(out)
}

/// Render AL's rate-limit response headers as a one-line summary
/// suitable for inlining in error messages and System → Logs detail
/// fields. Surfaces only the headers actually present — when AL
/// strips a header the corresponding field is omitted rather than
/// shown as "unknown" so a quick log read makes it obvious which
/// headers AL chose to send.
///
/// Used to disambiguate "real rate limit" (Remaining: 0) from
/// "something else returned 429" (Remaining: 80, no rate-limit
/// headers at all, only Retry-After present, etc.). Without this,
/// every 429 looked identical in the logs and a per-account or
/// per-endpoint quota was indistinguishable from the global cap.
fn format_rate_limit_headers_for_log(headers: &reqwest::header::HeaderMap) -> String {
    let mut parts: Vec<String> = Vec::new();
    let read = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    if let Some(v) = read("x-ratelimit-limit") {
        parts.push(format!("limit={v}"));
    }
    if let Some(v) = read("x-ratelimit-remaining") {
        parts.push(format!("remaining={v}"));
    }
    if let Some(v) = read("x-ratelimit-reset") {
        parts.push(format!("reset={v}"));
    }
    if let Some(v) = read("retry-after") {
        parts.push(format!("retry_after={v}"));
    }
    // `ryokan_60s` is the count of AL requests Ryokan ITSELF made
    // in the last 60 seconds. Surfaced on every 429 so the user
    // can attribute the budget exhaustion: when AL's `remaining=0`
    // but `ryokan_60s` is well under 30, the missing budget was
    // burned by something outside Ryokan on the same IP — another
    // tab on anilist.co (each page render makes many GraphQL
    // calls), a second Ryokan instance, an extension or helper
    // tool. When `ryokan_60s` is >= 30, we have an internal
    // over-firing bug to fix.
    //
    // Tracked separately from `parts` so the "no rate-limit
    // headers" fallback below stays meaningful — a response with
    // no AL rate-limit headers shouldn't get a fake `parts.len() > 0`
    // just because the local counter has a value.
    let ryokan_60s = format!("ryokan_60s={}", rate_limit::recent_al_request_count_60s());
    if parts.is_empty() {
        // No rate-limit headers at all — strong signal the response
        // came from somewhere other than AL's normal rate-limiter
        // (Cloudflare, an upstream proxy, an auth layer that
        // misuses 429). Still include the local count so the user
        // can spot a runaway internal loop in this case too.
        format!("no rate-limit headers; {ryokan_60s}")
    } else {
        parts.push(ryokan_60s);
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_rate_limit_headers_renders_present_fields_only() {
        use reqwest::header::HeaderMap;
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-limit", "30".parse().unwrap());
        h.insert("x-ratelimit-remaining", "0".parse().unwrap());
        h.insert("retry-after", "5".parse().unwrap());
        let out = format_rate_limit_headers_for_log(&h);
        assert!(out.contains("limit=30"), "got: {out}");
        assert!(out.contains("remaining=0"), "got: {out}");
        assert!(out.contains("retry_after=5"), "got: {out}");
        // `reset` was not present and must not appear as "unknown".
        assert!(
            !out.contains("reset="),
            "absent header fields must be omitted; got: {out}"
        );
        // `ryokan_60s` is always appended for budget attribution.
        assert!(
            out.contains("ryokan_60s="),
            "local request counter must always surface so the user can tell whether Ryokan or external traffic burned the AL budget; got: {out}"
        );
    }

    #[test]
    fn format_rate_limit_headers_falls_back_when_no_headers_present() {
        // When AL (or an intermediary) sends none of the rate-limit
        // headers on a 429, the summary must lead with "no rate-limit
        // headers" explicitly — that's a diagnostic hint that the
        // 429 may have come from somewhere other than AL's normal
        // rate-limiter (Cloudflare, an upstream proxy, an auth
        // layer misusing 429). The local `ryokan_60s` counter still
        // appends so a runaway internal loop hitting an upstream
        // proxy that strips AL's headers is also visible.
        let h = reqwest::header::HeaderMap::new();
        let out = format_rate_limit_headers_for_log(&h);
        assert!(out.starts_with("no rate-limit headers"), "got: {out}");
        assert!(
            out.contains("ryokan_60s="),
            "local counter must surface even on the no-headers path; got: {out}"
        );
    }

    #[test]
    fn media_selector_omits_unused_var() {
        // Regression: AniList rejects `Media(id: null, idMal: 1)` as
        // "Not Found" because the resolver treats explicit JSON null as
        // "filter where id equals null." The variables map must OMIT
        // the unused arm, not send it as null. Verified live 2026-04-19.
        let by_id = build_media_selector_variables(MediaSelector::Id(42));
        assert_eq!(by_id.get("id").and_then(|v| v.as_i64()), Some(42));
        assert!(
            !by_id.contains_key("idMal"),
            "Id selector must NOT include idMal var (even as null) — sending null trips an AniList 404"
        );

        let by_mal = build_media_selector_variables(MediaSelector::IdMal(1));
        assert_eq!(by_mal.get("idMal").and_then(|v| v.as_i64()), Some(1));
        assert!(
            !by_mal.contains_key("id"),
            "IdMal selector must NOT include id var (even as null) — sending null trips an AniList 404"
        );
    }

    #[test]
    fn normalize_search_key_folds_whitespace_and_case() {
        assert_eq!(
            normalize_search_key(false, "  Jojo  Part  3 "),
            "al::jojo part 3"
        );
        assert_eq!(normalize_search_key(true, "\tFrieren\n"), "mal::frieren");
    }

    #[test]
    fn normalize_search_key_separates_al_from_mal_modes() {
        assert_ne!(
            normalize_search_key(false, "Bleach"),
            normalize_search_key(true, "Bleach")
        );
    }

    #[test]
    fn compose_error_when_both_rate_limited_suggests_retry() {
        let msg = compose_search_error(
            Some("AniList rate-limited (retry in 28s)"),
            "Jikan rate-limited (HTTP 429): You are being rate-limited",
        );
        assert!(
            msg.contains("Both AniList and Jikan/MAL are rate-limited"),
            "msg was: {}",
            msg
        );
        assert!(msg.contains("28s"), "retry hint lost: {}", msg);
    }

    #[test]
    fn compose_error_falls_back_when_only_one_rate_limited() {
        let msg = compose_search_error(
            Some("AniList rate-limited (retry in 28s)"),
            "Jikan unreachable: connection refused",
        );
        assert!(msg.starts_with("AniList rate-limited"), "msg was: {}", msg);
        assert!(
            msg.contains("connection refused"),
            "jikan detail lost: {}",
            msg
        );
        assert!(
            !msg.contains("Both AniList and Jikan/MAL"),
            "wrong branch: {}",
            msg
        );
    }

    #[test]
    fn compose_error_without_anilist_reason_uses_mal_prefix() {
        let msg = compose_search_error(None, "Jikan HTTP 500: upstream down");
        assert!(
            msg.starts_with("MAL/Jikan search failed"),
            "msg was: {}",
            msg
        );
        assert!(msg.contains("upstream down"));
    }

    #[test]
    fn search_cache_roundtrips_and_expires_on_ttl_mismatch() {
        // We can't sleep for 60s in tests, but we can validate that distinct
        // keys don't collide and that a put/get returns the same Vec.
        let key = normalize_search_key(false, "test query unique 1");
        let entries = vec![AnimeEntry {
            id: 42,
            id_mal: None,
            title_romaji: "Test".into(),
            title_english: "".into(),
            title_native: "".into(),
            cover_url: "".into(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: "Finished".into(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
            average_score: Some(85),
        }];
        search_cache_put(key.clone(), entries.clone());
        let got = search_cache_get(&key).expect("cached value should be present");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 42);

        // A different normalized key should miss.
        let other = normalize_search_key(false, "completely different");
        assert!(search_cache_get(&other).is_none());
    }

    // ── parse_media_list_collection_lists / all_buckets_are_custom_lists ─

    #[test]
    fn parse_media_list_collection_drops_custom_buckets_keeps_status_buckets() {
        // Per-entry customLists membership is read from the entry, so
        // status buckets pass through; isCustomList: true buckets are
        // skipped entirely to avoid double-counting an entry that
        // appears in both its status bucket and a custom one.
        let lists = serde_json::json!([
            {
                "isCustomList": false,
                "entries": [
                    {
                        "mediaId": 100,
                        "status": "CURRENT",
                        "progress": 4,
                        "score": 8.5,
                        "updatedAt": 1_700_000_000_i64,
                        "notes": "",
                        "customLists": {"My picks": true, "On hold": false},
                    },
                    {
                        "mediaId": 200,
                        "status": "PLANNING",
                        "progress": 0,
                        "score": 0,
                        "updatedAt": 1_700_000_001_i64,
                        "notes": "",
                        "customLists": {},
                    }
                ]
            },
            {
                "isCustomList": true,
                "entries": [
                    {"mediaId": 100, "status": "CURRENT", "progress": 4}
                ]
            }
        ]);
        let arr = lists.as_array().unwrap();
        let out = parse_media_list_collection_lists(arr);
        assert_eq!(out.len(), 2, "custom-list bucket must not double-count");
        assert_eq!(out[0].media_id, 100);
        assert_eq!(out[0].status, "CURRENT");
        assert_eq!(out[0].progress, 4);
        assert!((out[0].score - 8.5).abs() < f64::EPSILON);
        assert_eq!(out[0].custom_lists, vec!["My picks".to_string()]);
        assert_eq!(out[1].media_id, 200);
        assert_eq!(out[1].status, "PLANNING");
        assert!(out[1].custom_lists.is_empty());
    }

    #[test]
    fn parse_media_list_collection_handles_missing_optional_fields() {
        // A real AL response omits `notes` for some entries and the
        // `customLists` object can be missing entirely. Defaults must
        // not panic or skip the entry.
        let lists = serde_json::json!([{
            "isCustomList": false,
            "entries": [{"mediaId": 7}]
        }]);
        let out = parse_media_list_collection_lists(lists.as_array().unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].media_id, 7);
        assert_eq!(out[0].progress, 0);
        assert!(out[0].notes.is_empty());
        assert!(out[0].custom_lists.is_empty());
    }

    #[test]
    fn parse_media_list_collection_drops_entries_without_media_id() {
        // A defensive write-side bug at AL would emit an entry with no
        // mediaId. Our merge step keys on it; better to drop than to
        // produce a 0-id row that later collides with everything.
        let lists = serde_json::json!([{
            "isCustomList": false,
            "entries": [
                {"status": "CURRENT"},
                {"mediaId": 99, "status": "CURRENT"}
            ]
        }]);
        let out = parse_media_list_collection_lists(lists.as_array().unwrap());
        assert_eq!(out.len(), 1, "entry without mediaId must be dropped");
        assert_eq!(out[0].media_id, 99);
    }

    #[test]
    fn all_buckets_are_custom_lists_returns_false_on_empty() {
        // Empty `lists` is the "no buckets at all" case, NOT the
        // schema-drift signal. Returning true here would fire the
        // sanity warn for legitimate brand-new accounts.
        assert!(!all_buckets_are_custom_lists(&[]));
    }

    #[test]
    fn all_buckets_are_custom_lists_true_when_every_bucket_is_custom() {
        // The signal the sanity warn fires for: AL ships a schema
        // change where every bucket is `isCustomList: true` and our
        // walker would silently produce empty syncs forever.
        let lists = serde_json::json!([
            {"isCustomList": true, "entries": []},
            {"isCustomList": true, "entries": []}
        ]);
        let arr = lists.as_array().unwrap();
        assert!(all_buckets_are_custom_lists(arr));
    }

    #[test]
    fn extract_media_list_buckets_succeeds_for_populated_response() {
        // The normal shape: lists array present and non-empty.
        let json = serde_json::json!({
            "data": {
                "MediaListCollection": {
                    "lists": [
                        {"isCustomList": false, "entries": []}
                    ],
                    "user": {"mediaListOptions": {"scoreFormat": "POINT_10"}}
                }
            }
        });
        let lists = extract_media_list_buckets(&json).unwrap();
        assert_eq!(lists.len(), 1);
    }

    #[test]
    fn extract_media_list_buckets_returns_empty_when_collection_is_null() {
        // The empty-account shape: AL replies with MediaListCollection
        // = null for accounts with zero anime entries. Should succeed
        // with an empty lists array, not error.
        let json = serde_json::json!({
            "data": {
                "MediaListCollection": null
            }
        });
        let lists = extract_media_list_buckets(&json).unwrap();
        assert!(
            lists.is_empty(),
            "null MediaListCollection should yield empty lists"
        );
    }

    #[test]
    fn extract_media_list_buckets_returns_empty_when_lists_field_is_missing() {
        // Defense-in-depth: a response with the wrapper present but
        // `lists` missing should also degrade to empty rather than
        // erroring. The sync's downstream code handles empty inputs
        // safely already, so being lenient here is the right move.
        let json = serde_json::json!({
            "data": {
                "MediaListCollection": {
                    "user": {"mediaListOptions": {"scoreFormat": "POINT_10"}}
                }
            }
        });
        let lists = extract_media_list_buckets(&json).unwrap();
        assert!(lists.is_empty());
    }

    #[test]
    fn extract_media_list_buckets_errors_when_data_field_missing() {
        // The malformed-response shape: `data` itself missing means
        // something is genuinely wrong upstream (rare — GraphQL errors
        // are caught earlier via the `errors[]` branch). Surface it.
        let json = serde_json::json!({});
        let err = extract_media_list_buckets(&json).unwrap_err();
        assert!(err.contains("missing data"), "got: {err}");
    }

    #[test]
    fn all_buckets_are_custom_lists_false_when_any_bucket_is_status() {
        // Mixed shape (today's reality) → no warn.
        let lists = serde_json::json!([
            {"isCustomList": false, "entries": []},
            {"isCustomList": true, "entries": []}
        ]);
        let arr = lists.as_array().unwrap();
        assert!(!all_buckets_are_custom_lists(arr));
    }

    #[test]
    fn parse_media_node_reads_is_adult_and_defaults_false() {
        // Issue #219 — `isAdult` rides along with the detail fetch so
        // the series row and the auto-search log can say why Nyaa
        // returns nothing for a sukebei-only title.
        let adult = serde_json::json!({
            "id": 21521,
            "title": { "romaji": "Kowaremono: Risa THE ANIMATION" },
            "format": "OVA",
            "status": "FINISHED",
            "episodes": 1,
            "isAdult": true
        });
        assert!(parse_media_node(&adult).expect("parses").is_adult);

        let plain = serde_json::json!({
            "id": 1,
            "title": { "romaji": "Cowboy Bebop" },
            "format": "TV",
            "status": "FINISHED",
            "episodes": 26
        });
        assert!(!parse_media_node(&plain).expect("parses").is_adult);

        // Cached blobs written before the field existed still load.
        let mut detail = parse_media_node(&plain).expect("parses");
        detail.is_adult = true;
        let mut blob: serde_json::Value = serde_json::to_value(&detail).unwrap();
        blob.as_object_mut().unwrap().remove("is_adult");
        let back: AnimeDetail = serde_json::from_value(blob).unwrap();
        assert!(!back.is_adult);
    }
}
