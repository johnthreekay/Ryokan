//! 5-minute in-memory cache for interactive-search results.
//!
//! The interactive search UI on the series page hits Nyaa on every
//! open, which turns into rapid-fire requests during UI iteration
//! (tweak CSS, reload, click search, tweak CSS, reload, click
//! search...). This cache lets repeat queries within a 5-minute
//! window reuse the previous hit instead of reissuing the Nyaa
//! request, so a developer working on picker/layout changes doesn't
//! make Nyaa look like a user under attack.
//!
//! Scope deliberately narrow:
//!   * Only the three interactive-search endpoints use it. Auto-search,
//!     RSS, manual batch/episode grabs hit Nyaa directly.
//!   * Keyed by the series request id and what was searched for
//!     ([`Key`]: one episode, the batch list, an episode set).
//!     Config changes (preferred_groups, quality profile, etc.) don't
//!     invalidate the cache — a 5-minute staleness on scoring input is
//!     fine for the "I'm iterating on the UI" use case.
//!   * No size cap. In practice the map is bounded by
//!     `series × (batch + per-episode queries)` the user actually
//!     clicks on, which is small. If that assumption ever fails, add
//!     an LRU or a size cap.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::services::nyaa::SearchResult;

pub const INTERACTIVE_SEARCH_TTL: Duration = Duration::from_secs(5 * 60);

/// What an interactive search was for: one episode, the batch list, or
/// the Wanted page's episode set (the sorted wanted episodes, since the
/// probe queries derive from them).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Episode(i64, i32),
    Batch(i64),
    EpisodeSet(i64, Vec<i32>),
}

pub type InteractiveSearchCache = Arc<Mutex<HashMap<Key, (Instant, Arc<Vec<SearchResult>>)>>>;

pub fn new() -> InteractiveSearchCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Return cached results when a fresh entry (inserted within
/// [`INTERACTIVE_SEARCH_TTL`]) exists. Stale entries are evicted
/// on the same lookup so the map can't grow unboundedly from
/// old queries against since-deleted series.
pub fn get(cache: &InteractiveSearchCache, key: Key) -> Option<Arc<Vec<SearchResult>>> {
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((inserted_at, results)) = guard.get(&key) {
        if inserted_at.elapsed() < INTERACTIVE_SEARCH_TTL {
            return Some(results.clone());
        }
        // Stale — evict so the map can't grow unboundedly from
        // queries against since-deleted series. A plain miss
        // doesn't touch the map.
        guard.remove(&key);
    }
    None
}

pub fn insert(cache: &InteractiveSearchCache, key: Key, results: Vec<SearchResult>) {
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    guard.insert(key, (Instant::now(), Arc::new(results)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<SearchResult> {
        Vec::new()
    }

    #[test]
    fn miss_returns_none() {
        let cache = new();
        assert!(get(&cache, Key::Episode(1, 5)).is_none());
    }

    #[test]
    fn insert_then_hit_returns_same_results() {
        let cache = new();
        insert(&cache, Key::Episode(1, 5), sample());
        assert!(get(&cache, Key::Episode(1, 5)).is_some());
    }

    #[test]
    fn distinct_keys_dont_collide() {
        // Episode searches and batch searches for the same series
        // must have separate cache slots. Previously if they
        // shared a key the first interactive batch search would
        // pollute the per-episode cache.
        let cache = new();
        insert(&cache, Key::Episode(1, 5), sample());
        assert!(
            get(&cache, Key::Batch(1)).is_none(),
            "batch key must not hit the per-episode entry"
        );
        assert!(
            get(&cache, Key::Episode(2, 5)).is_none(),
            "different series id must not hit another series's entry"
        );
    }

    #[test]
    fn an_episode_set_key_is_its_own_slot() {
        let cache = new();
        insert(&cache, Key::EpisodeSet(1, vec![3, 4, 5]), sample());
        assert!(get(&cache, Key::EpisodeSet(1, vec![3, 4, 5])).is_some());
        assert!(get(&cache, Key::EpisodeSet(1, vec![3, 4])).is_none());
        assert!(get(&cache, Key::Episode(1, 3)).is_none());
    }

    #[test]
    fn stale_entries_are_evicted_on_lookup() {
        // Manually insert a stale entry by reaching past the
        // public API, then verify get() both misses and removes
        // the stale slot so the map doesn't grow without bound.
        let cache = new();
        let stale_time = Instant::now() - (INTERACTIVE_SEARCH_TTL + Duration::from_secs(1));
        cache
            .lock()
            .unwrap()
            .insert(Key::Episode(1, 5), (stale_time, Arc::new(sample())));
        assert!(get(&cache, Key::Episode(1, 5)).is_none());
        assert!(
            cache.lock().unwrap().get(&Key::Episode(1, 5)).is_none(),
            "stale entry must be evicted, not just returned as a miss"
        );
    }
}
