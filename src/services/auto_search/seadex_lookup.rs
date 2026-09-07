//! SeaDex lookup + cache + DB persistence. Split out of `mod.rs`
//! during the v1.5 refactor — about 550 lines of self-contained
//! cache/state/HTTP code that the rest of `auto_search` only touches
//! through `fetch_seadex_payload` + `seadex_gates` + `is_seadex_match`
//! (the three `pub(super)` entry points exposed below) and through
//! `seadex_warm_cache_from_db` / `prewarm_seadex_negative` (called
//! from main.rs's startup hook).
//!
//! The in-memory `SEADEX_CACHE` / `SEADEX_INFLIGHT` statics live here
//! too. The cache key is `anilist_id` so synthetic Jikan-fallback rows
//! (negative ids) never enter — the auto-search sweep in `mod.rs`
//! filters those at the call site.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use tokio::sync::Notify;

use crate::models::config::Config;
use crate::models::log::LogCategory;
use crate::services::custom_formats::{self, CompiledCustomFormat};
use crate::services::nyaa::{self, SearchResult};
use crate::services::{logger, seadex};

/// Look up the SeaDex entry for `anilist_id` and return the set of
/// usable "best" info hashes. Disabled (or lookup-failed) returns an
/// empty set, which causes the scoring-time SeaDex bonus and any
/// `SeaDexBest` Custom Format spec to harmlessly contribute zero.
///
/// Emits both `tracing::debug!` lines (for `RUST_LOG=ryokan=debug`
/// console readers) and `LogCategory::AutoSearch` rows (for the
/// in-app Log Viewer) for every call — skip, hit, miss, and error.
/// The previous version silently swallowed errors into
/// `HashSet::new()`, which made a dead releases.moe indistinguishable
/// from "SeaDex not configured" or "this title isn't on SeaDex."
/// Everything the auto-search pipeline needs from one SeaDex lookup:
/// the set of "best" info hashes (for the filter bypass and score
/// overlay) and fully-populated `SearchResult` candidates built
/// directly from each curated torrent's Nyaa view page.
///
/// The pre-fetched candidates are the key to surfacing SeaDex releases
/// whose Nyaa titles don't overlap with the target's AniList aliases
/// (smol's `Monogatari (Season 9)` megapack for Kizumonogatari Part 2
/// is the canonical example). The text-query sweep can't find them;
/// we go direct to `/view/<id>` and inject the result ourselves.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct SeaDexPayload {
    pub(super) hashes: HashSet<String>,
    /// Synthetic candidates fetched directly from each SeaDex-curated
    /// torrent's Nyaa view URL. Empty when the lookup is skipped or
    /// fails, or when every fetch fails. Merged into the candidate
    /// pool by the caller before the text-query sweep runs.
    pub(super) candidates: Vec<SearchResult>,
}

/// 24-hour in-memory cache for SeaDex lookups, keyed by AniList ID.
///
/// A single auto-search sweep across a multi-target batch (`find_all_for_target`,
/// `collect_scored_batches_for_target`, `collect_scored_for_target`)
/// can round-trip releases.moe several times per target, and each hit
/// also fetches every SeaDex-best torrent's Nyaa view page. For a
/// JoJo S1–S5 sweep that's up to ~5 × (1 + N) HTTP requests — enough
/// to throttle both releases.moe and Nyaa on a cold start.
///
/// SeaDex is a curated dataset that updates on the order of days, not
/// minutes — once the community picks a "best" release for a title it
/// rarely churns — so a 24h TTL amortizes the cost down to ~1 lookup
/// per target per day while still catching the occasional revision.
/// Anything shorter burns network round-trips for no observable
/// correctness benefit. Config changes (preferred groups, resolution)
/// affect how candidates get *scored* downstream, not what SeaDex
/// returns, so keying by anilist_id alone is correct.
///
/// The cache lives for the lifetime of the process, so a restart is
/// the operator's escape hatch if they ever need to force-refresh.
const SEADEX_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
// Errors are cached for a much shorter window. The point is to absorb a
// transient releases.moe outage (or a brief 5xx burst) without every
// concurrent search hammering the upstream — but not so long that a
// recovered service stays masked across the next RSS sweep.
const SEADEX_ERROR_TTL: Duration = Duration::from_secs(5 * 60);
// Cap the cache so a long-running process can't accumulate every
// AniList ID it ever touched. Mirrors anilist::DETAIL_CACHE_MAX_ENTRIES.
const SEADEX_CACHE_MAX_ENTRIES: usize = 500;

/// Cache value carries an `expires_at` (rather than `fetched_at`) so the
/// success and error TTLs can coexist without `cache_get` having to know
/// which kind it is reading.
static SEADEX_CACHE: LazyLock<StdMutex<HashMap<i64, (Instant, SeaDexPayload)>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// In-flight registry — one `Notify` per anilist_id currently being
/// fetched. Concurrent callers find the existing entry, await the
/// notify, then re-check the cache. Without this, the cold-cache
/// window is a thundering-herd target: an RSS sweep, a manual button,
/// and an anibridge request can all fire on the same series in the
/// same second and each one hits releases.moe.
static SEADEX_INFLIGHT: LazyLock<StdMutex<HashMap<i64, Arc<Notify>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn seadex_cache_get(anilist_id: i64) -> Option<SeaDexPayload> {
    let cache = SEADEX_CACHE.lock().ok()?;
    let (expires_at, payload) = cache.get(&anilist_id)?;
    if Instant::now() < *expires_at {
        Some(payload.clone())
    } else {
        None
    }
}

fn seadex_cache_put_with_ttl(anilist_id: i64, payload: SeaDexPayload, ttl: Duration) {
    if let Ok(mut cache) = SEADEX_CACHE.lock() {
        let expires_at = Instant::now() + ttl;
        cache.insert(anilist_id, (expires_at, payload));
        if cache.len() > SEADEX_CACHE_MAX_ENTRIES {
            // Drop expired first; if still over cap, drop the entry
            // that expires soonest (effectively LRU under uniform TTL).
            let now = Instant::now();
            let expired: Vec<i64> = cache
                .iter()
                .filter(|(_, (expires_at, _))| *expires_at <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in &expired {
                cache.remove(k);
            }
            // Exclude the entry we just inserted from the soonest-expires
            // candidate set. Without this, an error entry (5-min TTL)
            // inserted into a cache full of fresh success entries (24h
            // TTL) immediately self-evicts because it's the row with the
            // earliest `expires_at` — defeating the negative-cache
            // coalescing the short TTL was added to provide.
            if cache.len() > SEADEX_CACHE_MAX_ENTRIES
                && let Some((&oldest, _)) = cache
                    .iter()
                    .filter(|(k, _)| **k != anilist_id)
                    .min_by_key(|(_, (expires_at, _))| *expires_at)
            {
                cache.remove(&oldest);
            }
        }
    }
}

fn seadex_cache_put(anilist_id: i64, payload: SeaDexPayload) {
    seadex_cache_put_with_ttl(anilist_id, payload, SEADEX_CACHE_TTL);
}

fn seadex_cache_put_error(anilist_id: i64) {
    seadex_cache_put_with_ttl(anilist_id, SeaDexPayload::default(), SEADEX_ERROR_TTL);
}

/// Persist a successful (or "no entry") lookup to SQLite so it survives
/// process restart. Called from the leader path on the success / no-entry
/// branches; the error branch deliberately doesn't persist (5-min TTL is
/// too short to be worth the I/O, and a restart should re-probe upstream
/// health rather than inherit a "this is broken" verdict).
async fn seadex_persist_to_db(db: &SqlitePool, anilist_id: i64, payload: &SeaDexPayload) {
    let json = match serde_json::to_string(payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("seadex: failed to serialize payload for persistence: {e}");
            return;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let res = sqlx::query(
        "INSERT INTO seadex_lookup_cache (anilist_id, payload_json, cached_at) \
         VALUES (?, ?, ?) \
         ON CONFLICT(anilist_id) DO UPDATE SET payload_json = excluded.payload_json, cached_at = excluded.cached_at",
    )
    .bind(anilist_id)
    .bind(&json)
    .bind(now)
    .execute(db)
    .await;
    if let Err(e) = res {
        tracing::warn!("seadex: failed to persist cache row for anilist_id={anilist_id}: {e}");
    }
}

/// Warm the in-memory SeaDex cache from SQLite at startup. Drops rows
/// older than `SEADEX_CACHE_TTL` opportunistically (cheap to run during
/// boot; avoids unbounded growth of the persisted table over time).
/// Called once from `main()` after migrations.
pub async fn seadex_warm_cache_from_db(db: &SqlitePool) {
    let ttl_secs = SEADEX_CACHE_TTL.as_secs() as i64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Drop expired persisted rows first so the SELECT below doesn't have
    // to filter them in Rust and the table stays bounded.
    if let Err(e) = sqlx::query("DELETE FROM seadex_lookup_cache WHERE cached_at + ? < ?")
        .bind(ttl_secs)
        .bind(now)
        .execute(db)
        .await
    {
        tracing::warn!("seadex: failed to evict expired persisted rows: {e}");
    }

    let rows = match sqlx::query_as::<_, (i64, String, i64)>(
        "SELECT anilist_id, payload_json, cached_at FROM seadex_lookup_cache",
    )
    .fetch_all(db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("seadex: failed to read persisted cache for warming: {e}");
            return;
        }
    };

    let mut warmed = 0usize;
    for (anilist_id, json, cached_at) in rows {
        let remaining_secs = (cached_at + ttl_secs).saturating_sub(now);
        if remaining_secs <= 0 {
            continue;
        }
        let payload: SeaDexPayload = match serde_json::from_str(&json) {
            Ok(p) => p,
            Err(e) => {
                // Schema drift in SearchResult or SeaDexPayload would
                // land here. Skip the row rather than aborting startup;
                // it'll be re-fetched on next lookup.
                tracing::warn!(
                    "seadex: skipping unparseable persisted row anilist_id={anilist_id}: {e}"
                );
                continue;
            }
        };
        seadex_cache_put_with_ttl(
            anilist_id,
            payload,
            Duration::from_secs(remaining_secs as u64),
        );
        warmed += 1;
    }
    tracing::info!("seadex: warmed {warmed} persisted cache entries from SQLite");
}

/// Pre-fetch SeaDex hits for many AniList ids in one OR-batched
/// PocketBase request and cache the *negative* responses (ids that
/// SeaDex doesn't know about) so the per-series loop downstream skips
/// the SeaDex round-trip for those ids entirely.
///
/// Designed for the upgrade-search sweep: most series in a typical
/// library have no SeaDex entry, so a single batched call (50 ids per
/// chunk) replaces N sequential round-trips for those ids. Hits are
/// deliberately NOT cached here — the per-series fetch path also pulls
/// Nyaa view-page candidates for each usable torrent in the entry,
/// which doesn't fit the "single batch query" shape; letting hits
/// flow through the lazy path keeps that work amortized across the
/// loop iterations rather than concentrated in a startup burst.
///
/// Already-cached ids (positive or negative) are skipped, so calling
/// this repeatedly within a TTL window is cheap. Failures are logged
/// and swallowed: the worst case is the per-series loop pays the
/// previously-existing per-id cost.
pub async fn prewarm_seadex_negative(db: &SqlitePool, anilist_ids: &[i64]) {
    let to_query: Vec<i64> = anilist_ids
        .iter()
        .copied()
        .filter(|id| {
            *id > 0
                && seadex_cache_get(*id).is_none()
                // Don't prewarm an id that's already being fetched by another
                // concurrent path (RSS sweep, manual button, anibridge request).
                // The leader's `seadex_cache_put` will populate the cache for us;
                // doubling the request would defeat the in-flight coalescing.
                && !seadex_inflight_contains(*id)
        })
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();
    if to_query.is_empty() {
        return;
    }
    tracing::debug!(
        "seadex: prewarming negative cache for {} anilist_id(s)",
        to_query.len()
    );
    let results = match seadex::lookup_batch(&to_query).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("seadex: batch prewarm failed: {e}");
            return;
        }
    };
    let mut cached = 0usize;
    for (anilist_id, entry) in results {
        if entry.is_none() {
            let payload = SeaDexPayload::default();
            seadex_cache_put(anilist_id, payload.clone());
            seadex_persist_to_db(db, anilist_id, &payload).await;
            cached += 1;
        }
    }
    tracing::info!(
        "seadex: prewarm cached {cached} negative entries from {} batched lookup(s)",
        to_query.len().div_ceil(seadex::SEADEX_BATCH_SIZE)
    );
}

/// Cheap non-blocking check for "is this anilist_id currently being
/// fetched by some other coalesced path?" Returns false on lock
/// poisoning so prewarm errs on the side of doing the work.
fn seadex_inflight_contains(anilist_id: i64) -> bool {
    SEADEX_INFLIGHT
        .lock()
        .map(|m| m.contains_key(&anilist_id))
        .unwrap_or(false)
}

/// Drop guard — removes the in-flight registry entry and wakes any
/// waiters even if the leader's fetch panics or returns early. Without
/// this, a stuck entry would block every future lookup for that
/// anilist_id until process restart.
struct SeaDexInFlightGuard {
    anilist_id: i64,
    notify: Arc<Notify>,
}
impl Drop for SeaDexInFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = SEADEX_INFLIGHT.lock() {
            map.remove(&self.anilist_id);
        }
        self.notify.notify_waiters();
    }
}

pub(super) async fn fetch_seadex_payload(
    db: &SqlitePool,
    seadex_enabled: bool,
    anilist_id: i64,
    series_title: &str,
    preferred_groups: &[String],
    preferred_resolution: &str,
    prefer_subs: bool,
) -> SeaDexPayload {
    if !seadex_enabled {
        tracing::debug!(
            "seadex: skipping lookup — gate off (seadex_enabled=false and no SeaDex CF installed)"
        );
        // Intentionally no DB log row for the "gate off" path — this
        // would spam the Log Viewer with one line per search for every
        // user who hasn't turned SeaDex on.
        return SeaDexPayload::default();
    }
    if anilist_id <= 0 {
        tracing::debug!(
            "seadex: skipping lookup — no AniList ID on target (anilist_id={anilist_id})"
        );
        logger::debug(
            db,
            LogCategory::AutoSearch,
            &format!("SeaDex lookup skipped for {series_title}"),
            &format!("no AniList ID (anilist_id={anilist_id})"),
        )
        .await;
        return SeaDexPayload::default();
    }
    if let Some(cached) = seadex_cache_get(anilist_id) {
        tracing::debug!(
            "seadex: cache hit for anilist_id={anilist_id} ({} hash(es), {} candidate(s))",
            cached.hashes.len(),
            cached.candidates.len()
        );
        return cached;
    }

    // Leadership election. If another task is already fetching this
    // anilist_id, wait for it to finish and then re-read the cache.
    // Loop because a leader could finish without populating the cache
    // (defensive against future bail-outs); in that case we re-attempt
    // leadership ourselves rather than spinning on a notify that's
    // already been sent.
    //
    // The `Role` enum exists to keep the `StdMutex` guard out of the
    // async scope — `MutexGuard` is `!Send`, so we have to drop it
    // before any `.await` (the compiler can't see that `drop(inflight)`
    // happens before the await on its own).
    enum Role {
        Lead(SeaDexInFlightGuard),
        Wait(Arc<Notify>),
    }
    let _guard: Option<SeaDexInFlightGuard> = loop {
        let role = match SEADEX_INFLIGHT.lock() {
            Err(_) => {
                // Poisoned — skip coalescing entirely and just fetch.
                // Redundant network work beats wedging the path.
                break None;
            }
            Ok(mut inflight) => {
                if let Some(existing) = inflight.get(&anilist_id) {
                    Role::Wait(existing.clone())
                } else {
                    let notify = Arc::new(Notify::new());
                    inflight.insert(anilist_id, notify.clone());
                    Role::Lead(SeaDexInFlightGuard { anilist_id, notify })
                }
            }
        };
        // MutexGuard dropped at end of `match` expression above.
        match role {
            Role::Lead(g) => break Some(g),
            Role::Wait(notify) => {
                // Subscribe BEFORE re-checking the cache. `Notify::notify_waiters`
                // doesn't leave a permit for future `.notified()` calls — if the
                // leader fires the notify between our unlock above and our
                // first poll, we'd hang forever waiting for a notification
                // that already happened. The recipe from tokio's docs is
                // pin → enable → re-check → await: enabling registers our
                // waiter atomically against the next `notify_waiters`, so
                // any notification fired after `enable()` (including ones
                // that race with our cache re-check) wakes us correctly.
                let waiter = notify.notified();
                tokio::pin!(waiter);
                waiter.as_mut().enable();
                if let Some(cached) = seadex_cache_get(anilist_id) {
                    tracing::debug!("seadex: coalesced wait hit for anilist_id={anilist_id}");
                    return cached;
                }
                waiter.await;
                if let Some(cached) = seadex_cache_get(anilist_id) {
                    tracing::debug!("seadex: coalesced wait hit for anilist_id={anilist_id}");
                    return cached;
                }
                // Leader didn't populate; loop and try to lead ourselves.
                continue;
            }
        }
    };

    tracing::debug!("seadex: fetching releases.moe entry for anilist_id={anilist_id}");
    match seadex::lookup(anilist_id).await {
        Ok(Some(entry)) => {
            let hashes = seadex::best_hashes(&entry);
            tracing::debug!(
                "seadex: releases.moe returned {} usable hash(es) for anilist_id={}",
                hashes.len(),
                anilist_id
            );
            logger::debug(
                db,
                LogCategory::AutoSearch,
                &format!(
                    "SeaDex lookup: {} usable hash(es) for {series_title}",
                    hashes.len()
                ),
                &format!("anilist_id={anilist_id}"),
            )
            .await;

            // Fetch each usable torrent's view page in parallel via
            // JoinSet — a typical SeaDex entry has 1–4 usable torrents
            // and the previous serial loop turned that into 1–4 ×
            // ~500ms of wall time on cache miss. Concurrency is
            // self-bounded by `usable.len()`, so no semaphore needed.
            let opts_for_score = nyaa::SearchOptions {
                query: series_title.to_string(),
                category: "1_0".to_string(),
                filter: "0".to_string(),
                user: String::new(),
                preferred_groups: preferred_groups.to_vec(),
                preferred_resolution: preferred_resolution.to_string(),
                prefer_subs,
            };
            let mut join_set: tokio::task::JoinSet<(String, Result<SearchResult, String>)> =
                tokio::task::JoinSet::new();
            for torrent in entry.torrents.iter() {
                if !seadex::is_usable(torrent, &entry.notes) {
                    continue;
                }
                let view_url = seadex::to_nyaa_view_url(torrent).to_string();
                let opts = opts_for_score.clone();
                join_set.spawn(async move {
                    let result = nyaa::fetch_view_result(&view_url, &opts).await;
                    (view_url, result)
                });
            }
            let mut candidates = Vec::new();
            while let Some(joined) = join_set.join_next().await {
                match joined {
                    Ok((view_url, Ok(result))) => {
                        tracing::debug!(
                            "seadex: injected curated candidate from view url={} title={:?} hash={}",
                            view_url,
                            result.title,
                            result.info_hash
                        );
                        candidates.push(result);
                    }
                    Ok((view_url, Err(e))) => {
                        tracing::warn!("seadex: failed to fetch view page for {}: {}", view_url, e);
                        logger::warn(
                            db,
                            LogCategory::AutoSearch,
                            &format!("SeaDex view-page fetch failed for {series_title}"),
                            &format!("url={view_url}, error={e}"),
                        )
                        .await;
                    }
                    Err(join_err) => {
                        tracing::warn!("seadex: view-page task failed to join: {join_err}");
                    }
                }
            }
            let payload = SeaDexPayload { hashes, candidates };
            seadex_cache_put(anilist_id, payload.clone());
            seadex_persist_to_db(db, anilist_id, &payload).await;
            payload
        }
        Ok(None) => {
            tracing::debug!("seadex: releases.moe has no entry for anilist_id={anilist_id}");
            logger::debug(
                db,
                LogCategory::AutoSearch,
                &format!("SeaDex has no entry for {series_title}"),
                &format!("anilist_id={anilist_id}"),
            )
            .await;
            // Cache the "no entry" result so we don't re-hit releases.moe
            // for the same anilist_id within the TTL window.
            let payload = SeaDexPayload::default();
            seadex_cache_put(anilist_id, payload.clone());
            seadex_persist_to_db(db, anilist_id, &payload).await;
            payload
        }
        Err(e) => {
            tracing::warn!("seadex: releases.moe lookup failed for anilist_id={anilist_id}: {e}");
            logger::warn(
                db,
                LogCategory::AutoSearch,
                &format!("SeaDex lookup failed for {series_title}"),
                &format!("anilist_id={anilist_id}, error={e}"),
            )
            .await;
            // Negative-cache the failure (short TTL) so concurrent
            // searches and immediate retries don't hammer a broken
            // upstream until the window expires.
            seadex_cache_put_error(anilist_id);
            SeaDexPayload::default()
        }
    }
}

/// Decide whether the current search call needs to make a SeaDex
/// network round-trip. Hashes are required if *either* the config has
/// SeaDex enabled (hardcoded boost) or the compiled CF set contains a
/// `SeaDexBestSpecification` (Custom-Format-driven boost) — so one call
/// serves both paths. Returns the gate flag plus the "hardcoded boost
/// active" flag (suppressed whenever the user has a SeaDex CF, to
/// avoid double-counting).
pub(super) fn seadex_gates(
    config: &Config,
    cfs: &[CompiledCustomFormat],
) -> (bool /* needs_lookup */, bool /* boost_enabled */) {
    let has_cf = custom_formats::has_seadex_cf(cfs);
    // SeaDex picks are Nyaa torrents; with the built-in search off
    // they must not seed the pool through the side door.
    let needs_lookup = config.nyaa_enabled && (config.seadex_enabled || has_cf);
    let boost_enabled = config.seadex_enabled && !has_cf;
    (needs_lookup, boost_enabled)
}

/// True if `info_hash` (non-empty) is in the SeaDex best-hashes set.
///
/// **Both inputs must already be lowercase.** `seadex::best_hashes`
/// populates the set with lowercase strings, and `extract_hash` in
/// `services::nyaa` lowercases every scraped magnet hash at parse
/// time. Enforced by `debug_assert!` so a future caller that forgets
/// fails loudly in tests. The previous version called
/// `info_hash.to_ascii_lowercase()` on every invocation — one
/// allocation per candidate × per CF, which adds up on a batch sweep.
pub(super) fn is_seadex_match(info_hash: &str, seadex_hashes: &HashSet<String>) -> bool {
    if info_hash.is_empty() || seadex_hashes.is_empty() {
        return false;
    }
    debug_assert!(
        !info_hash.chars().any(|c| c.is_ascii_uppercase()),
        "is_seadex_match: info_hash must be lowercase, got {info_hash:?}"
    );
    seadex_hashes.contains(info_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SeaDex persistence ───────────────────────────────────────────

    // ── SeaDex persistence ───────────────────────────────────────────
    //
    // Use anilist_ids in the 990_000_000+ range so they can't collide
    // with the in-memory `SEADEX_CACHE` global between tests run on the
    // same process. (Tests get their own in-memory SQLite pool, but the
    // process-global LazyLock cache is shared.)

    #[tokio::test]
    async fn seadex_persist_round_trips_through_warm() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        let anilist_id = 990_000_001;
        let mut hashes = HashSet::new();
        hashes.insert("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string());
        let payload = SeaDexPayload {
            hashes,
            candidates: vec![],
        };

        seadex_persist_to_db(&db, anilist_id, &payload).await;
        seadex_warm_cache_from_db(&db).await;

        let cached = seadex_cache_get(anilist_id).expect("warmed entry should be present");
        assert_eq!(cached.hashes.len(), 1);
        assert!(
            cached
                .hashes
                .contains("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
    }

    #[tokio::test]
    async fn seadex_warm_drops_expired_persisted_rows() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        let anilist_id = 990_000_002;
        // Insert a row whose `cached_at` is older than the TTL.
        let stale_cached_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - (SEADEX_CACHE_TTL.as_secs() as i64 + 60);
        sqlx::query(
        "INSERT INTO seadex_lookup_cache (anilist_id, payload_json, cached_at) VALUES (?, ?, ?)",
    )
    .bind(anilist_id)
    .bind(serde_json::to_string(&SeaDexPayload::default()).unwrap())
    .bind(stale_cached_at)
    .execute(&db)
    .await
    .unwrap();

        seadex_warm_cache_from_db(&db).await;

        // The expired row should be evicted from the persistent table
        // and never make it into the in-memory cache.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM seadex_lookup_cache WHERE anilist_id = ?")
                .bind(anilist_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count.0, 0);
        assert!(seadex_cache_get(anilist_id).is_none());
    }

    #[tokio::test]
    async fn seadex_error_cache_is_in_memory_only() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        let anilist_id = 990_000_003;
        seadex_cache_put_error(anilist_id);

        // The negative-error path must not write to SQLite — restart
        // should re-probe upstream rather than inherit a "broken" verdict.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM seadex_lookup_cache WHERE anilist_id = ?")
                .bind(anilist_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count.0, 0);
        // But the in-memory cache should hold the (default) negative entry.
        assert!(seadex_cache_get(anilist_id).is_some());
    }

    #[tokio::test]
    async fn seadex_inflight_contains_tracks_registry() {
        // Locks the contract that `prewarm_seadex_negative` relies on
        // when filtering ids: an id with a registered Notify reads as
        // "inflight" and an unregistered one doesn't. Without this
        // gate the prewarm could redundantly issue a request that's
        // already mid-flight on another path.
        let anilist_id = 990_000_004;
        // Sanity: not present at start.
        assert!(!seadex_inflight_contains(anilist_id));
        // Register a fake in-flight entry, then verify the helper sees it.
        {
            let mut map = SEADEX_INFLIGHT.lock().unwrap();
            map.insert(anilist_id, Arc::new(Notify::new()));
        }
        assert!(seadex_inflight_contains(anilist_id));
        // Clean up so other tests aren't affected.
        SEADEX_INFLIGHT.lock().unwrap().remove(&anilist_id);
        assert!(!seadex_inflight_contains(anilist_id));
    }
}

#[cfg(test)]
mod nyaa_gate_tests {
    use super::*;

    #[test]
    fn seadex_seed_is_off_when_the_built_in_nyaa_search_is_off() {
        let mut config = Config {
            seadex_enabled: true,
            ..Default::default()
        };
        assert_eq!(seadex_gates(&config, &[]), (true, true));
        config.nyaa_enabled = false;
        assert_eq!(
            seadex_gates(&config, &[]),
            (false, true),
            "no lookup: SeaDex picks are Nyaa torrents"
        );
    }
}
