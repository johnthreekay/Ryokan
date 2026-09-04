//! Torznab/newznab indexer abstraction (issue #28).
//!
//! ## What's here
//!
//! The [`Indexer`] trait, the [`Release`] / [`SearchQuery`] /
//! [`IndexerCaps`] data model, and pure-function helpers for the
//! auto-search dedup pass and concurrent fan-out. The concrete
//! `TorznabIndexer` impl lives in the `torznab/` submodule alongside
//! the search-pipeline integration that populates
//! `Vec<Arc<dyn Indexer>>` from the [`crate::models::indexers`] table.
//!
//! ## Why Nyaa stays out-of-band
//!
//! Per plan decision #1, the existing direct Nyaa scraper in
//! [`crate::services::nyaa`] is NOT adapted to this trait. The
//! search pipeline dispatches to Nyaa-direct + fans out to
//! `Indexer` impls in parallel, then merges. Conforming Nyaa to
//! the trait would have meant adding [`Release`] fields like
//! `nyaa_description: Option<String>` that only one impl
//! populates — a noisy contract — and the source-classification
//! pipeline already reads Nyaa's description body directly. Pretending
//! the sources are uniform would have hidden that coupling.
//!
//! ## Protocol notes (from research, 2026-04-25)
//!
//! Authoritative shapes that any future impl must respect:
//!
//! - **URL shape is opaque to Ryokan.** Prowlarr emits
//!   `http://host:9696/{N}/api?apikey={KEY}&t=...`; Jackett emits
//!   `http://host:9117/api/v2.0/indexers/{slug}/results/torznab/api?apikey={KEY}&t=...`.
//!   Both end in `/api` and accept torznab params after `?`. The
//!   user pastes the full base URL verbatim from each tool's
//!   "Copy Torznab Url" button; Ryokan must not parse or
//!   reconstruct it.
//! - **Errors come back as HTTP 200 with `<error code="N"
//!   description="..."/>` bodies.** Real impls (Prowlarr, Jackett)
//!   also return non-200 in some paths (Prowlarr 401 on bad apikey
//!   before the torznab layer); both must be handled.
//! - **Anime category is `5070`** in the standard torznab namespace.
//!   AnimeTosho via Prowlarr historically mis-tagged anime as
//!   `5999` (Other) — title-parse fallback is required if the cat
//!   doesn't include 5070.
//! - **Per-indexer rate limits live inside Prowlarr/Jackett,** not
//!   the indexer itself. They surface as `429 Retry-After`. The
//!   torznab client honors them via the per-id [`cooldown`] table
//!   in this module: on 429 it stamps `until = now + Retry-After`
//!   (capped at [`cooldown::COOLDOWN_MAX`], defaulted to
//!   [`cooldown::COOLDOWN_DEFAULT`] when the header is missing) and
//!   subsequent calls for that indexer short-circuit at the top of
//!   `fetch()` until the window lifts. Per-id rather than global
//!   so a 429 on AB doesn't silence a healthy NZBGeek for the same
//!   window — each Prowlarr-fronted indexer has its own budget.
//! - **`tvsearch` with `cat=5070&q=<title>` is the right anime
//!   path.** `season`/`ep` params don't translate cleanly because
//!   anime trackers key on absolute episode numbers in titles.

pub mod cooldown;
pub mod torznab;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Standard torznab category id for anime. See doc-comment above for
/// why title-parse fallback is needed when this is absent from a
/// release's reported categories.
pub const TORZNAB_CAT_ANIME: i32 = 5070;

/// Standard torznab / newznab parent category for movies. Some
/// trackers file anime films here rather than under TV/Anime.
pub const TORZNAB_CAT_MOVIES: i32 = 2000;

/// Standard torznab / newznab parent category for adult content.
/// Prowlarr and Jackett file sukebei and similar trackers here, so
/// an adult title that only asks for TV/Anime finds nothing.
pub const TORZNAB_CAT_XXX: i32 = 6000;

/// Every category id an indexer reports in its caps, parents and
/// children flattened. Empty when the caps have not been probed yet.
pub fn known_category_ids(caps_json: &str) -> Vec<i32> {
    fn walk(cats: &[CategoryCap], out: &mut Vec<i32>) {
        for c in cats {
            out.push(c.id);
            walk(&c.subcategories, out);
        }
    }
    let Ok(caps) = serde_json::from_str::<IndexerCaps>(caps_json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(&caps.categories, &mut out);
    out.sort_unstable();
    out.dedup();
    out
}

/// The indexer's reported categories as `id name` lines for the edit
/// form, parents first, custom ids last.
/// One category an indexer's caps report, for the categories field's
/// chip list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedCategory {
    pub id: i32,
    pub name: String,
}

pub fn reported_categories(caps_json: &str) -> Vec<ReportedCategory> {
    let Ok(caps) = serde_json::from_str::<IndexerCaps>(caps_json) else {
        return Vec::new();
    };
    let mut flat: Vec<(i32, String)> = Vec::new();
    fn walk(cats: &[CategoryCap], out: &mut Vec<(i32, String)>) {
        for c in cats {
            out.push((c.id, c.name.clone()));
            walk(&c.subcategories, out);
        }
    }
    walk(&caps.categories, &mut flat);
    flat.sort_by_key(|(id, _)| *id);
    flat.dedup_by_key(|(id, _)| *id);
    flat.into_iter()
        .map(|(id, name)| ReportedCategory { id, name })
        .collect()
}

/// What to put on the wire for one indexer: the requested categories
/// that the indexer actually reports, and when it reports none of them,
/// its own top-level categories instead. A tracker whose caps list only
/// XXX (sukebei through Prowlarr) or only Movies (YTS) would otherwise
/// answer every anime request with nothing, silently. A requested
/// parent counts as supported when the indexer lists it or any of its
/// standard children (2000 is satisfied by 2040). With no caps cached
/// the request goes out as asked.
pub fn resolve_request_categories(requested: &[i32], known: &[i32]) -> Vec<i32> {
    if known.is_empty() {
        return requested.to_vec();
    }
    // Standard ids are grouped by thousands: 5070 sits under 5000, 2040
    // under 2000. A request for a child is satisfied by its parent (an
    // indexer that lists TV lists anime), and a request for a parent by
    // any of its children.
    let supports = |cat: i32| -> bool {
        let standard = cat < 100_000;
        let parent = cat - cat % 1000;
        known.iter().any(|k| {
            *k == cat
                || (standard && *k == parent)
                || (standard && cat == parent && (parent..parent + 1000).contains(k))
        })
    };
    let supported: Vec<i32> = requested.iter().copied().filter(|c| supports(*c)).collect();
    if !supported.is_empty() {
        return supported;
    }
    let mut parents: Vec<i32> = known
        .iter()
        .copied()
        .filter(|k| *k < 100_000 && k % 1000 == 0)
        .collect();
    if parents.is_empty() {
        parents = known.to_vec();
    }
    parents
}

/// The categories a search for a series asks an indexer for. A parent
/// category includes its children on the wire, so parents are enough.
/// Anime is always requested; movies also ask for Movies because
/// trackers disagree on where anime films go; adult titles also ask
/// for XXX, which is where Prowlarr and Jackett put sukebei.
pub fn search_categories(format: &str, is_adult: bool) -> Vec<i32> {
    let mut cats = vec![TORZNAB_CAT_ANIME];
    if format.eq_ignore_ascii_case("MOVIE") {
        cats.push(TORZNAB_CAT_MOVIES);
    }
    if is_adult {
        cats.push(TORZNAB_CAT_XXX);
    }
    cats
}

/// Default per-indexer search timeout when the row's
/// `request_timeout_secs` is NULL. Decision #7 — tighter than
/// Sonarr's 100s default because Ryokan's interactive search
/// surface needs lower user-perceived latency. Overridable
/// process-wide via `RYOKAN_INDEXER_DEFAULT_TIMEOUT_SECS`.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Indexer caps cache TTL. Decision #6 — matches Sonarr's
/// `NewznabCapabilitiesProvider.cs` 7-day default. The search
/// pipeline re-fetches lazily on next read past the TTL; manual
/// "Refresh caps" button on the indexer edit page covers the
/// out-of-band edit case.
pub const CAPS_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

/// What an [`Indexer::search`] caller asks for. Mirrors torznab's
/// `t=tvsearch` parameter set; the impl translates to the wire
/// format. `q` is the only free-text input — `season`/`ep` are
/// deliberately omitted because anime trackers key on absolute
/// episode numbers in release titles, not season+ep.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub q: String,
    /// Torznab category ids. Defaults to `[5070]` (anime) when
    /// empty. Multiple ids OR together on the wire (`cat=5070,5080`).
    pub categories: Vec<i32>,
    /// Page size. None lets the impl pick (typically the indexer's
    /// caps-reported default). Must be ≤ caps `max_limit`.
    pub limit: Option<u32>,
    /// 0-based offset for paging. None = 0.
    pub offset: Option<u32>,
}

/// One release row from a torznab response. Field set is the union
/// of what real Prowlarr/Jackett deployments emit; impl-specific
/// fields go in [`extra`] so the core type stays portable across
/// indexers.
///
/// Source classification consumes [`title`] + [`size_bytes`] +
/// [`info_hash`] (when present) — same inputs the existing
/// pipeline derives from a Nyaa scrape. The Nyaa-description-body
/// signal is unavailable here; classification degrades to four
/// layers (filename + ffprobe + temporal + group-map).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// FK to [`crate::models::indexers::Indexer::id`] of the
    /// indexer that surfaced this release. The dedup pass
    /// (decision #3) attributes each (infohash, indexer) pair to
    /// the lowest-priority-number indexer.
    pub indexer_id: i64,
    /// Snapshot of the indexer's priority at search time so a
    /// later DB edit can't change attribution retroactively.
    pub indexer_priority: i32,
    /// Snapshot of the indexer's display name at search time.
    /// Same retroactive-edit reasoning as `indexer_priority` —
    /// a later rename of the indexer row in Settings shouldn't
    /// rewrite the name on past Release records that callers
    /// kept around. Surfaces through `into_search_result` to
    /// drive the "Indexer" column on the interactive search UI.
    #[serde(default)]
    pub indexer_name: String,
    pub title: String,
    /// Stable per-release identifier from the torznab `<guid>`
    /// element. Used as a dedup key when [`info_hash`] is empty.
    pub guid: String,
    /// Download URL. For Prowlarr this is a proxy URL with the
    /// apikey appended; the .torrent fetch must go through Prowlarr.
    /// Per research note: stale on Prowlarr restart, so don't cache
    /// across days.
    pub link: String,
    /// Magnet URI when the indexer surfaces one, else empty.
    pub magnet: String,
    /// Unix timestamp of `<pubDate>`. 0 when missing/unparseable.
    pub publish_date: i64,
    pub size_bytes: u64,
    pub seeders: i32,
    pub leechers: i32,
    /// Lowercase hex; empty when the indexer doesn't expose it
    /// (some private trackers omit it). Dedup falls back to
    /// [`guid`] when this is empty.
    pub info_hash: String,
    /// Standard torznab category ids on this release. May contain
    /// indexer-specific subcategory ids beyond the well-known
    /// 5000-series. Empty is legal — the title-parse fallback
    /// catches anime mis-tags like AnimeTosho via Prowlarr's
    /// 5999 issue.
    pub categories: Vec<i32>,
    /// `1.0` = full count, `0.0` = freeleech. Some private
    /// trackers expose this; public trackers don't. None when
    /// the indexer doesn't emit the attr.
    pub download_volume_factor: Option<f32>,
    pub upload_volume_factor: Option<f32>,
    /// Catch-all for impl-specific torznab attrs not promoted to
    /// first-class fields. Inspector-friendly only — scoring path
    /// must not key off these.
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

/// Caps response shape. Cached as JSON in `indexers.caps_json`
/// per the 7-day TTL. The settings UI renders [`categories`] as a
/// multi-select on the per-indexer config form.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexerCaps {
    pub categories: Vec<CategoryCap>,
    pub search_modes: Vec<SearchModeCap>,
    /// Server-reported maximum `limit` per request. None when the
    /// caps response doesn't carry it; defaults to spec value 100.
    pub max_limit: Option<u32>,
    /// Server-reported default `limit`. None when missing; spec
    /// default is 50.
    pub default_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryCap {
    pub id: i32,
    pub name: String,
    /// Subcategories nested under this top-level category id.
    /// Most indexers report a flat list; Prowlarr nests where the
    /// upstream tracker has subcats (e.g., 5070 Anime → 5080
    /// Anime/Movies on some private trackers).
    #[serde(default)]
    pub subcategories: Vec<CategoryCap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchModeCap {
    /// `"search"` / `"tvsearch"` / `"movie"` / `"music"` / `"book"`.
    pub mode: String,
    pub available: bool,
    /// Supported params (`q`, `cat`, `season`, `ep`, `tvdbid`,
    /// `imdbid`, etc.). Per-mode.
    #[serde(default)]
    pub supported_params: Vec<String>,
}

/// The torznab/newznab indexer trait. Every impl talks to a single
/// indexer instance (a row in the `indexers` table). The search
/// pipeline holds these as `Vec<Arc<dyn Indexer>>` so the fan-out
/// can run them concurrently.
///
/// `async_trait` (rather than the stable native syntax) for the
/// same reason [`crate::services::download_client::DownloadClient`]
/// uses it: object-safety + Send-bound futures on Tokio's
/// multi-threaded runtime require the boxed-future macro.
#[async_trait::async_trait]
pub trait Indexer: Send + Sync {
    /// FK to `indexers.id`.
    fn id(&self) -> i64;
    fn name(&self) -> &str;
    /// Sonarr-convention priority (lower = preferred). Drives
    /// auto-search dedup attribution per [`dedup_for_auto_search`]
    /// and the fan-out concurrency order.
    fn priority(&self) -> i32;
    fn is_private_tracker(&self) -> bool;
    /// Multi-client routing — id of the row in
    /// `download_clients` this indexer is pinned to. `None` means
    /// "use the default client." Read by
    /// [`crate::AppState::client_for_indexer`] at grab time so
    /// the cache lookup avoids hitting the DB on the hot path.
    fn download_client_id(&self) -> Option<i64> {
        None
    }
    /// Indexer protocol kind ("torznab" / "newznab"). Default
    /// "torznab" matches the v1 wire — only impl that returns
    /// "newznab" is the future Usenet variant; the kind drives
    /// download-client protocol routing at grab time and the
    /// per-feed `RssSource::Indexer { kind }` attribution.
    fn kind(&self) -> &str {
        "torznab"
    }

    /// Fetch capabilities from `t=caps`. Impls should respect the
    /// 7-day TTL on the row's [`caps_json`] cache; the search-path
    /// caller persists fresh JSON via
    /// [`crate::models::indexers::update_caps`] when this returns
    /// after a network round-trip.
    async fn caps(&self) -> Result<IndexerCaps, String>;

    /// Search this indexer for releases matching `query`. Impls
    /// are responsible for:
    ///
    /// - Parsing torznab `<error code="N"/>` bodies even on HTTP
    ///   200 (per protocol).
    /// - Honoring 429 + `Retry-After` from upstream.
    /// - Filtering results below the indexer's configured
    ///   `min_seeders` *before* return — the search pipeline's
    ///   scoring runs on whatever this returns, so a low-seeder
    ///   release leaking into the candidate set is wasted work.
    /// - Stamping each [`Release`] with `indexer_id` +
    ///   `indexer_priority` so the dedup pass can attribute
    ///   correctly.
    async fn search(&self, query: &SearchQuery) -> Result<Vec<Release>, String>;
}

/// Per-indexer search outcome. Surfaces partial failures so the
/// auto-search inspector can show "AnimeTosho: timeout after 30s"
/// alongside successful results from other indexers; produced by the
/// concurrent fan-out helper.
#[derive(Debug)]
pub struct IndexerSearchOutcome {
    pub indexer_id: i64,
    pub indexer_name: String,
    pub result: Result<Vec<Release>, String>,
}

/// Auto-search dedup pass (decision #3). Collapses the same
/// (infohash, ?) release reported by multiple indexers into a
/// single [`Release`], attributing to the lowest-priority-number
/// indexer (Sonarr convention) and aggregating seeder counts via
/// `max` (most accurate signal across reporting indexers).
///
/// The dedup key is `info_hash` when present; otherwise `guid`.
/// The (lossy) fallback exists because some private trackers omit
/// infohash from torznab responses — without the guid fallback,
/// every release from those indexers would slip past dedup and
/// flood the candidate set.
///
/// **Interactive search policy (decision #3):** when interactive
/// search learns to fan out to indexers, it should NOT dedup across
/// indexers — one row per `(indexer, infohash)` so the user can
/// pick a preferred tracker. That path is intentionally Nyaa-only
/// for now; the per-tracker-row behavior lands alongside the
/// seed-rules wiring it depends on. The `merge_for_interactive_search`
/// helper that previously lived here was removed in PR #107 round-2
/// review since it was dead code; it gets reintroduced when there's
/// a caller.
pub fn dedup_for_auto_search(releases: Vec<Release>) -> Vec<Release> {
    let mut by_key: HashMap<String, Release> = HashMap::new();
    for release in releases {
        let key = if !release.info_hash.is_empty() {
            release.info_hash.to_ascii_lowercase()
        } else if !release.guid.is_empty() {
            release.guid.clone()
        } else {
            // No infohash, no guid — keep the release but key by
            // a uniqueness-safe value so it doesn't collide with
            // other no-key releases. Using the title alone would
            // collide across indexers; including indexer_id keeps
            // them separate without losing them entirely.
            format!("__{}_{}", release.indexer_id, release.title)
        };
        match by_key.get_mut(&key) {
            None => {
                by_key.insert(key, release);
            }
            Some(existing) => {
                // Lower priority number = preferred indexer for
                // attribution. Tiebreak on indexer_id ascending so
                // the result is stable across calls.
                let take_new = release.indexer_priority < existing.indexer_priority
                    || (release.indexer_priority == existing.indexer_priority
                        && release.indexer_id < existing.indexer_id);
                let merged_seeders = existing.seeders.max(release.seeders);
                let merged_leechers = existing.leechers.max(release.leechers);
                if take_new {
                    *existing = release;
                }
                existing.seeders = merged_seeders;
                existing.leechers = merged_leechers;
            }
        }
    }
    let mut out: Vec<Release> = by_key.into_values().collect();
    // Stable sort by priority then id so callers downstream don't
    // see HashMap iteration nondeterminism.
    out.sort_by(|a, b| {
        a.indexer_priority
            .cmp(&b.indexer_priority)
            .then(a.indexer_id.cmp(&b.indexer_id))
    });
    out
}

/// PR #107 review fix #4: build a fresh `Vec<Arc<dyn Indexer>>`
/// from the DB and wrap it in the [`crate::IndexerCache`] swap-on-
/// write Arc. Called once at startup and again from the
/// Settings → Indexers handlers on add/edit/delete so the cache
/// stays current. Failed instantiations log + drop, same partial-
/// fan-out posture as the old in-line `load_indexer_clients`.
pub async fn rebuild_cache(db: &sqlx::SqlitePool) -> crate::IndexerCache {
    let rows = match crate::models::indexers::list_enabled(db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("indexers: failed to load from DB: {e}");
            Vec::new()
        }
    };
    // PR #107 round-3 review fix #1: pair the row + client
    // together through the SAME filter_map so a `from_row_arc`
    // failure drops both halves atomically. The previous
    // `rows.iter().zip(clients.iter())` realigned positionally —
    // skipping row B would silently pair row B with client C and
    // write C's caps_json under B's id when the probe later ran.
    let pairs: Vec<(crate::models::indexers::Indexer, Arc<dyn Indexer>)> = rows
        .into_iter()
        .filter_map(|row| match torznab::TorznabIndexer::from_row_arc(&row) {
            Ok(idx) => Some((row, idx)),
            Err(e) => {
                tracing::warn!("indexers: skipping #{} ({}) — {}", row.id, row.name, e);
                None
            }
        })
        .collect();

    // PR #107 round-2 review fix #6: lazy caps probe for rows that
    // haven't been probed yet. Detached `tokio::spawn`s so a slow
    // indexer can't block startup or the post-edit settings save.
    // No retry on failure — the next rebuild_cache call (next
    // settings edit, next process restart) re-tries any row whose
    // caps_json is still empty.
    for (row, client) in &pairs {
        if !row.caps_json.is_empty() {
            continue;
        }
        let db_clone = db.clone();
        let client_clone = client.clone();
        let row_id = row.id;
        let row_name = row.name.clone();
        tokio::spawn(async move {
            match client_clone.caps().await {
                Ok(caps) => match serde_json::to_string(&caps) {
                    Ok(json) => {
                        if let Err(e) =
                            crate::models::indexers::update_caps(&db_clone, row_id, &json).await
                        {
                            tracing::warn!(
                                "indexers: caps probe persist failed for #{} ({}): {}",
                                row_id,
                                row_name,
                                e
                            );
                        }
                    }
                    Err(e) => {
                        // PR #107 round-3 review fix #5: don't
                        // swallow serialize failures silently.
                        tracing::warn!(
                            "indexers: caps serialize failed for #{} ({}): {}",
                            row_id,
                            row_name,
                            e
                        );
                    }
                },
                Err(e) => {
                    tracing::debug!(
                        "indexers: caps probe failed for #{} ({}): {}",
                        row_id,
                        row_name,
                        e
                    );
                }
            }
        });
    }

    let clients: Vec<Arc<dyn Indexer>> = pairs.into_iter().map(|(_, c)| c).collect();
    Arc::new(tokio::sync::RwLock::new(Arc::new(clients)))
}

/// Swap fresh contents into an existing cache without reallocating
/// the outer Arc. Used by the Settings handlers after an upsert /
/// delete so handler code holding `state.indexers.clone()`
/// continues to see the new list on the next read.
pub async fn refresh_cache_in_place(cache: &crate::IndexerCache, db: &sqlx::SqlitePool) {
    let rebuilt = rebuild_cache(db).await;
    let new_inner = rebuilt.read().await.clone();
    let mut guard = cache.write().await;
    *guard = new_inner;
}

/// Concurrent fan-out across configured indexers. Each indexer
/// runs in its own future with the indexer's own request timeout;
/// a slow indexer holds up only its own slot, not the whole
/// search. Failures are captured as [`IndexerSearchOutcome`]
/// items rather than propagated — the auto-search inspector
/// shows per-indexer success/failure instead of failing the
/// whole search when one indexer dies.
pub async fn fan_out_search(
    indexers: &[Arc<dyn Indexer>],
    query: &SearchQuery,
) -> Vec<IndexerSearchOutcome> {
    use futures_util::future::join_all;
    let futures = indexers.iter().map(|idx| async move {
        IndexerSearchOutcome {
            indexer_id: idx.id(),
            indexer_name: idx.name().to_string(),
            result: idx.search(query).await,
        }
    });
    join_all(futures).await
}

impl Release {
    /// Convert a torznab/newznab [`Release`] into the
    /// [`crate::services::nyaa::SearchResult`] shape so the
    /// downstream auto-search loop can dedup + score indexer
    /// results alongside Nyaa results without a separate code
    /// path.
    ///
    /// Classification fields (`source`, `web_kind`, `is_remux`,
    /// `is_bdmv`, `quality_label`) are left empty — the
    /// downstream `classify_release` call fills them via the
    /// filename + ffprobe + temporal + group-map layers (the
    /// Nyaa-description-body layer is unavailable for indexer
    /// results, but the other four are load-bearing).
    ///
    /// Multi-RSS — convert this indexer-sourced release into
    /// an `RssItem` for the RSS sync fan-out (Option B). Carries
    /// `RssSource::Indexer { id, name, kind }` so the grab path
    /// can route through the indexer's `download_client_id`
    /// pin and the protocol-aware download-client guard
    /// (torrent vs NZB).
    ///
    /// `kind` should be the indexer's `kind` column ("torznab" /
    /// "newznab"). The indexer `name` is supplied separately so
    /// the row's display name flows through to log lines via
    /// `RssSource::label()`.
    pub fn to_rss_item(
        &self,
        indexer_name: &str,
        indexer_kind: &str,
    ) -> crate::services::rss::RssItem {
        let group = extract_group_from_title(&self.title);
        let resolution = extract_resolution_from_title(&self.title);
        let is_batch = detect_batch_from_title(&self.title);
        crate::services::rss::RssItem {
            title: self.title.clone(),
            link: self.link.clone(),
            guid: self.guid.clone(),
            // `link` IS the .torrent URL on torznab indexers (the
            // enclosure URL). The Nyaa-direct path uses
            // `nyaa:downloadurl` for the same role; here we map
            // straight from `link`.
            torrent: self.link.clone(),
            magnet: self.magnet.clone(),
            // PR 112 review #7 — torznab releases carry an
            // info_hash via `<torznab:attr name="infohash">`;
            // newznab releases never do (it's an NZB pointer,
            // not a torrent), so this stays empty for newznab
            // items. `build_item_key` falls through to GUID →
            // link → title for the dedup key when info_hash is
            // absent, so newznab items dedup by GUID just fine —
            // the info_hash dedup path is torznab-only by design.
            info_hash: self.info_hash.clone(),
            group,
            resolution,
            is_batch,
            source: crate::services::rss::RssSource::Indexer {
                id: self.indexer_id,
                name: indexer_name.to_string(),
                kind: indexer_kind.to_string(),
            },
        }
    }

    /// Group + resolution are extracted via the same simple
    /// patterns the Nyaa scraper uses on raw release titles —
    /// good enough for scoring, not as accurate as the full
    /// anitomy pass.
    pub fn into_search_result(self) -> crate::services::nyaa::SearchResult {
        let group = extract_group_from_title(&self.title);
        let resolution = extract_resolution_from_title(&self.title);
        let is_batch = detect_batch_from_title(&self.title);
        let indexer_id = self.indexer_id;
        let indexer_name = self.indexer_name;
        crate::services::nyaa::SearchResult {
            match_provenance: None,
            title: self.title,
            link: self.link.clone(),
            magnet: self.magnet,
            torrent: self.link,
            size: format_bytes_human(self.size_bytes),
            size_bytes: self.size_bytes as i64,
            seeders: self.seeders,
            leechers: self.leechers,
            downloads: 0,
            group,
            resolution,
            quality_label: String::new(),
            source: String::new(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch,
            // Indexer results don't carry Nyaa's "trusted uploader"
            // flag. Default false — scoring won't apply the trusted
            // bonus, which is acceptable for v1 (PT releases
            // typically have other quality signals).
            is_trusted: false,
            score: 0,
            info_hash: self.info_hash,
            score_breakdown: Vec::new(),
            // pubDate from the release as ISO-ish; the rest of
            // Ryokan's UI only renders the string verbatim.
            upload_date: format_publish_date(self.publish_date),
            // PR #107 review fix #7: propagate the indexer_id so
            // grabbed_torrents.indexer_id can be populated at grab
            // time. Without this, the indexer_id FK column would be
            // dormant — nothing surfaces the source indexer to the
            // grab path.
            indexer_id: Some(indexer_id),
            indexer_name,
        }
    }
}

fn extract_group_from_title(title: &str) -> String {
    // First `[Group]` bracket is the convention. Second-bracket
    // (e.g., `[BD 1080p]`) is metadata; ignore. Empty when no
    // bracketed group is present.
    let bytes = title.as_bytes();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'[' {
            start = Some(i + 1);
        } else if b == b']'
            && let Some(s) = start
        {
            let inner = &title[s..i];
            if !inner.is_empty() && !inner.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return inner.to_string();
            }
            start = None;
        }
    }
    String::new()
}

fn extract_resolution_from_title(title: &str) -> String {
    // PR #107 review fix #10: case-insensitive single pass instead
    // of two separate lists. Covers 2160P / 480P uppercase that the
    // earlier list missed.
    let lower = title.to_ascii_lowercase();
    for needle in ["2160p", "1080p", "720p", "480p"] {
        if lower.contains(needle) {
            let digits: String = needle.chars().take_while(|c| c.is_ascii_digit()).collect();
            return digits;
        }
    }
    String::new()
}

fn detect_batch_from_title(title: &str) -> bool {
    // PR #107 review fix #9: tighten "complete" matching with a
    // word-boundary-equivalent check. The earlier substring scan
    // matched "Incomplete Edition" and similar negated forms.
    // Now require the keyword to be flanked by whitespace, brackets,
    // or string boundaries — matches the way real release titles
    // tag a batch ("Show - Complete", "[Group] Show Complete BD").
    let lower = title.to_ascii_lowercase();
    if lower.contains("[bd]") || lower.contains("(bd)") || lower.contains("season pack") {
        return true;
    }
    for needle in ["batch", "complete"] {
        if let Some(idx) = lower.find(needle) {
            let before_ok = idx == 0
                || lower
                    .as_bytes()
                    .get(idx - 1)
                    .is_some_and(|&b| !b.is_ascii_alphabetic());
            let after = idx + needle.len();
            let after_ok = after == lower.len()
                || lower
                    .as_bytes()
                    .get(after)
                    .is_some_and(|&b| !b.is_ascii_alphabetic());
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

fn format_bytes_human(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 1.0 {
        format!("{:.1} GiB", gib)
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn format_publish_date(unix_ts: i64) -> String {
    if unix_ts <= 0 {
        return String::new();
    }
    // Match Nyaa's "YYYY-MM-DD HH:MM" UTC shape. Civil-calendar
    // expansion mirrors the parser's days-since-epoch math.
    let days = unix_ts.div_euclid(86400);
    let secs_of_day = unix_ts.rem_euclid(86400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;

    // Howard Hinnant's civil_from_days, inverse of the parser's
    // days_from_civil.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, hour, minute)
}

/// Multi-RSS — fetch the indexer's "recent items" feed for
/// the RSS sync fan-out (Option B). Issues an empty-`q` torznab
/// search, which Prowlarr / Jackett / native indexers all treat
/// as "return the most recent N items" — the same shape an RSS
/// feed delivers, just over the same XML transport the search
/// pipeline already speaks.
///
/// Per-source attribution is stamped via `Release::to_rss_item`
/// so each item carries `RssSource::Indexer { id, name, kind }`
/// for the grab-time client routing in a later change. The indexer's own
/// `min_seeders` filter runs inside `search()` before we see
/// the releases, so a low-seeder release leaking into the
/// fan-out is wasted work (matches the search-path behavior).
///
/// `limit` defaults to None — the indexer's caps-reported
/// default applies (typically 50 items, well over what a 60s
/// sync tick actually needs but consistent with existing search
/// behavior).
///
/// `categories` is empty — both `torznab/client.rs` and the
/// newznab path fall through to `[TORZNAB_CAT_ANIME]` (5070) on
/// an empty list. The 5070 category id is shared between the two
/// protocols (newznab's anime category is also 5070 in mainline
/// schemas); no protocol-aware branching needed here.
pub async fn fetch_indexer_rss(
    indexer: &dyn Indexer,
) -> Result<Vec<crate::services::rss::RssItem>, String> {
    let releases = indexer
        .search(&SearchQuery {
            q: String::new(),
            categories: Vec::new(),
            limit: None,
            offset: None,
        })
        .await?;
    let name = indexer.name().to_string();
    let kind = indexer.kind().to_string();
    Ok(releases
        .iter()
        .map(|r| r.to_rss_item(&name, &kind))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(
        indexer_id: i64,
        priority: i32,
        info_hash: &str,
        guid: &str,
        title: &str,
        seeders: i32,
    ) -> Release {
        Release {
            indexer_id,
            indexer_priority: priority,
            indexer_name: format!("Indexer{indexer_id}"),
            title: title.to_string(),
            guid: guid.to_string(),
            link: String::new(),
            magnet: String::new(),
            publish_date: 0,
            size_bytes: 1_000_000_000,
            seeders,
            leechers: 0,
            info_hash: info_hash.to_string(),
            categories: vec![TORZNAB_CAT_ANIME],
            download_volume_factor: None,
            upload_volume_factor: None,
            extra: HashMap::new(),
        }
    }

    // ── into_search_result ───────────────────────────────────────────

    /// The interactive-search "Indexer" column in the UI keys off
    /// `SearchResult::indexer_name`. Without this propagation,
    /// torznab/newznab fan-out results would land with empty
    /// `indexer_name` and the column would render "Nyaa" for every
    /// row regardless of where the result actually came from —
    /// exactly the symptom the user reported before this fix.
    #[test]
    fn into_search_result_propagates_indexer_name() {
        let r = release(7, 25, "abc", "g1", "[nekoBT] Show - 01", 10);
        let sr = r.into_search_result();
        assert_eq!(
            sr.indexer_name, "Indexer7",
            "indexer_name must round-trip through into_search_result()"
        );
        assert_eq!(sr.indexer_id, Some(7));
    }

    /// Counterpart: a Release with an empty indexer_name (defensive
    /// — shouldn't happen in production but the field is `String`,
    /// not `NonEmptyString`) produces an empty SearchResult name,
    /// which the UI then renders as "Nyaa" via its `|| 'Nyaa'`
    /// fallback. Both paths must work.
    #[test]
    fn into_search_result_handles_empty_indexer_name() {
        let mut r = release(7, 25, "abc", "g1", "Show - 01", 10);
        r.indexer_name = String::new();
        let sr = r.into_search_result();
        assert!(sr.indexer_name.is_empty());
    }

    // ── dedup_for_auto_search ────────────────────────────────────────

    #[test]
    fn dedup_keeps_single_release_unchanged() {
        let input = vec![release(1, 25, "abc123", "g1", "Show", 10)];
        let out = dedup_for_auto_search(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].info_hash, "abc123");
        assert_eq!(out[0].seeders, 10);
    }

    #[test]
    fn dedup_attributes_to_lower_priority_indexer() {
        // Two indexers report the same infohash. The lower priority
        // number (Sonarr convention) wins attribution.
        let input = vec![
            release(2, 50, "abc123", "g1", "Show (mirror)", 5),
            release(1, 5, "abc123", "g2", "Show (preferred)", 8),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(out.len(), 1, "same infohash must collapse to one row");
        // Attribution: indexer 1 wins (priority 5 < 50).
        assert_eq!(out[0].indexer_id, 1);
        assert_eq!(out[0].title, "Show (preferred)");
    }

    #[test]
    fn dedup_aggregates_seeders_via_max_across_reporters() {
        // Same release, two reports — keep the higher seeder count
        // since indexers can disagree by minutes-old data and max
        // is most likely accurate.
        let input = vec![
            release(1, 5, "abc123", "g1", "Show", 8),
            release(2, 50, "abc123", "g2", "Show (mirror)", 42),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seeders, 42, "max seeders across reporters");
    }

    #[test]
    fn dedup_falls_back_to_guid_when_infohash_empty() {
        // Some private trackers omit infohash. Dedup must still
        // collapse same-guid rows or the same release from one
        // indexer would appear twice.
        let input = vec![
            release(1, 5, "", "private-guid-1", "Show", 5),
            release(1, 5, "", "private-guid-1", "Show", 5),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(
            out.len(),
            1,
            "same guid must collapse even without infohash"
        );
    }

    #[test]
    fn dedup_keeps_no_key_releases_separate_per_indexer() {
        // Pathological: no infohash, no guid. Don't collapse them
        // into one row across indexers (we can't tell if they're
        // the same release) but also don't lose them.
        let input = vec![
            release(1, 5, "", "", "Show A", 5),
            release(2, 50, "", "", "Show A", 5),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(
            out.len(),
            2,
            "no-key releases from different indexers stay distinct"
        );
    }

    #[test]
    fn dedup_output_is_sorted_by_priority_ascending() {
        // Stable order = deterministic UI rendering across calls.
        let input = vec![
            release(2, 50, "h2", "g2", "Low priority", 5),
            release(1, 5, "h1", "g1", "High priority", 5),
            release(3, 25, "h3", "g3", "Mid priority", 5),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].indexer_priority, 5);
        assert_eq!(out[1].indexer_priority, 25);
        assert_eq!(out[2].indexer_priority, 50);
    }

    // ── Helper unit tests (PR #107 review fix #10) ──────────────────

    #[test]
    fn extract_group_pulls_first_bracketed_segment() {
        assert_eq!(
            super::extract_group_from_title("[GroupX] Show - 01 [BD 1080p].mkv"),
            "GroupX"
        );
    }

    #[test]
    fn extract_group_skips_pure_numeric_brackets() {
        // Some indexers prefix with a numeric ID (`[1234]` etc.).
        // Skip those — they're not the release group.
        assert_eq!(
            super::extract_group_from_title("[12345] [GroupX] Show.mkv"),
            "GroupX"
        );
    }

    #[test]
    fn extract_group_returns_empty_when_no_brackets() {
        assert_eq!(super::extract_group_from_title("Show - 01.mkv"), "");
    }

    #[test]
    fn extract_resolution_handles_uppercase_p() {
        // PR #107 review fix #10: 2160P / 480P with uppercase P
        // weren't covered by the original list; the case-insensitive
        // pass catches both.
        assert_eq!(
            super::extract_resolution_from_title("Show 2160P.mkv"),
            "2160"
        );
        assert_eq!(super::extract_resolution_from_title("Show 480P.mkv"), "480");
        assert_eq!(super::extract_resolution_from_title("Show [1080P]"), "1080");
    }

    #[test]
    fn extract_resolution_returns_empty_when_no_marker() {
        assert_eq!(super::extract_resolution_from_title("Show - 01.mkv"), "");
    }

    #[test]
    fn detect_batch_word_boundary_blocks_incomplete_edition() {
        // PR #107 review fix #9: "Incomplete" used to trigger because
        // the substring contains "complete". The word-boundary check
        // rejects it now.
        assert!(!super::detect_batch_from_title(
            "Show Incomplete Edition.mkv"
        ));
        // Real "Complete" still triggers.
        assert!(super::detect_batch_from_title("Show Complete BD.mkv"));
        // Other markers stay positive.
        assert!(super::detect_batch_from_title("[Group] Show Batch.mkv"));
        assert!(super::detect_batch_from_title(
            "[Group] Show Season Pack.mkv"
        ));
        assert!(super::detect_batch_from_title("Show [BD]"));
    }

    #[test]
    fn format_bytes_human_zero_renders_empty() {
        assert_eq!(super::format_bytes_human(0), "");
    }

    #[test]
    fn format_bytes_human_under_gib_uses_mib() {
        assert_eq!(super::format_bytes_human(500 * 1024 * 1024), "500 MiB");
    }

    #[test]
    fn format_bytes_human_at_or_above_gib_uses_gib() {
        assert_eq!(super::format_bytes_human(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(super::format_bytes_human(2 * 1024u64.pow(3)), "2.0 GiB");
    }

    #[test]
    fn format_publish_date_zero_or_negative_renders_empty() {
        assert_eq!(super::format_publish_date(0), "");
        assert_eq!(super::format_publish_date(-1), "");
    }

    #[test]
    fn format_publish_date_round_trips_known_dates() {
        // Epoch + 1s.
        assert_eq!(super::format_publish_date(1), "1970-01-01 00:00");
        // One full day in (non-leap 1970).
        assert_eq!(super::format_publish_date(86_400), "1970-01-02 00:00");
        // 2000-01-01 00:00:00 UTC = 946684800. Y2K boundary; verifies
        // century-handling in the civil-from-days math.
        assert_eq!(super::format_publish_date(946_684_800), "2000-01-01 00:00");
    }

    // ── rebuild_cache row/client pairing (PR #107 round-4 review fix #2) ─

    #[tokio::test]
    async fn rebuild_cache_drops_bad_rows_without_misaligning_clients() {
        // PR #107 round-3 review fix #1 was a real correctness bug:
        // when `from_row_arc` failed for a row mid-list, the
        // positional `rows.iter().zip(clients.iter())` pairing
        // realigned wrongly, causing the caps probe to write the
        // wrong row's caps under the surviving row's id.
        //
        // This test pins the pairing fix: insert [A_good, B_bad,
        // C_good] (B has an empty URL so from_row_arc rejects it),
        // call rebuild_cache, and assert the cache contains exactly
        // A and C in priority order. A future refactor that re-
        // introduces a parallel-collect pattern would silently
        // regress without this regression guard.
        use crate::models::indexers::{IndexerForm, KIND_TORZNAB, insert};

        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        let mk = |name: &str, url: &str, priority: i32| IndexerForm {
            name: Box::leak(name.to_string().into_boxed_str()),
            kind: KIND_TORZNAB,
            url: Box::leak(url.to_string().into_boxed_str()),
            api_key: "k",
            priority,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 0,
            request_timeout_secs: None,
            download_client_id: None,
            rss_enabled: false,
            categories: "",
        };
        let a_id = insert(&db, mk("A", "https://a.example/api", 5))
            .await
            .unwrap();
        let _b_id = insert(&db, mk("B_bad", "", 25)).await.unwrap();
        let c_id = insert(&db, mk("C", "https://c.example/api", 50))
            .await
            .unwrap();

        let cache = rebuild_cache(&db).await;
        let snapshot = cache.read().await.clone();

        assert_eq!(snapshot.len(), 2, "B (bad URL) must be dropped");
        // Ordered by priority asc — A (5) then C (50).
        assert_eq!(snapshot[0].id(), a_id, "first surviving entry must be A");
        assert_eq!(snapshot[1].id(), c_id, "second surviving entry must be C");
        // Specifically: B's id must NOT appear in the cache.
        assert!(
            !snapshot.iter().any(|c| c.id() == _b_id),
            "B's id must not survive — its client was never instantiated"
        );
    }
}

#[cfg(test)]
mod search_categories_tests {
    use super::*;

    #[test]
    fn request_categories_fall_back_to_what_the_indexer_reports() {
        // No caps yet: ask as requested.
        assert_eq!(resolve_request_categories(&[5070], &[]), vec![5070]);
        // A multi-category tracker: keep the supported subset.
        let nyaa = [5000, 2000, 140679, 127720];
        assert_eq!(
            resolve_request_categories(&[5070, 2000], &nyaa),
            vec![5070, 2000]
        );
        assert_eq!(resolve_request_categories(&[5070], &nyaa), vec![5070]);
        // sukebei through Prowlarr reports XXX and custom adult cats only:
        // an anime-only request becomes XXX.
        let sukebei = [6000, 125996, 140679];
        assert_eq!(resolve_request_categories(&[5070], &sukebei), vec![6000]);
        assert_eq!(
            resolve_request_categories(&[5070, 6000], &sukebei),
            vec![6000]
        );
        // YTS reports Movies only: a series request becomes Movies.
        let yts = [2000, 100044, 100045];
        assert_eq!(resolve_request_categories(&[5070], &yts), vec![2000]);
        assert_eq!(resolve_request_categories(&[5070, 2000], &yts), vec![2000]);
        // Only custom categories: send those.
        assert_eq!(
            resolve_request_categories(&[5070], &[100001, 100002]),
            vec![100001, 100002]
        );
    }

    #[test]
    fn known_category_ids_flattens_children_and_tolerates_missing_caps() {
        assert!(known_category_ids("").is_empty());
        let json = r#"{"categories":[{"id":2000,"name":"Movies","subcategories":[{"id":2040,"name":"HD","subcategories":[]}]},{"id":6000,"name":"XXX","subcategories":[]}],"search_modes":[],"max_limit":null,"default_limit":null}"#;
        assert_eq!(known_category_ids(json), vec![2000, 2040, 6000]);
    }

    #[test]
    fn categories_follow_format_and_adult_flag() {
        assert_eq!(search_categories("TV", false), vec![5070]);
        assert_eq!(search_categories("OVA", false), vec![5070]);
        assert_eq!(search_categories("MOVIE", false), vec![5070, 2000]);
        assert_eq!(search_categories("TV", true), vec![5070, 6000]);
        assert_eq!(search_categories("MOVIE", true), vec![5070, 2000, 6000]);
        assert_eq!(search_categories("", true), vec![5070, 6000]);
    }
}
