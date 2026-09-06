//! Ryokan — library-crate root.
//!
//! This crate is consumed in two places:
//!
//!   1. **`src/main.rs`** — the binary entry point. Imports [`AppState`],
//!      the module tree, and boots the axum listener. Keeping `main.rs`
//!      thin lets the router and every handler live in the library so
//!      integration tests (under `tests/`) can exercise them without
//!      spawning a subprocess.
//!   2. **`tests/`** integration tests. Each file is its own crate that
//!      depends on `ryokan` as a library — they call [`handlers`],
//!      [`models`], etc. directly. Test-only helpers live in
//!      [`test_support`], gated behind the `test-support` Cargo feature
//!      so they don't leak into release binaries.
//!
//! The module tree is all `pub mod` so the binary and integration
//! tests see the same surface. Individual items inside each module
//! stay `pub(crate)` unless a caller outside the lib (i.e. a test)
//! needs them — keeping internal helpers private prevents accidental
//! API-stability obligations.

// Several enums (`Source`, `Resolution`, `WebKind`, `LogLevel`, etc.)
// expose `fn from_str(&str) -> Self` as an infallible coercion —
// unknown inputs fall back to a default variant rather than erroring.
// That shape matches Ryokan's `Result<_, String>` error convention
// (the few callers that care about the error already have the raw
// string) and predates the lib/bin split. The standard `FromStr`
// trait requires `Result<Self, Err>`, so implementing it would force
// every call site to handle a `Result` that by design can't fail.
// Silenced crate-wide rather than rewriting seven call sites; if a
// future variant of this method can actually fail, it should be
// named something else (e.g. `parse`) at that site.
#![allow(clippy::should_implement_trait)]

pub mod handlers;
pub mod models;
pub mod services;

/// Test scaffolding — in-memory pool builder, `AppState` assembler,
/// series/grab seeders. Always compiled during the library's own
/// `cargo test` via `cfg(test)`; externally visible only when the
/// `test-support` feature is enabled (integration tests in `tests/`
/// opt in through `[features] test-support = []` in Cargo.toml).
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use services::{
    custom_formats::CompiledCfCache, download_client::DownloadClient, indexers::Indexer,
    interactive_search_cache::InteractiveSearchCache, jellyfin::JellyfinClient,
    manual_import::ImportSessionStore, notifications::NotificationProviders,
    oauth_state::OAuthStateStore, progress::ProgressRegistry, task_registry::TaskRegistry,
};

/// PR #107 review fix #4: cached `Vec<Arc<dyn Indexer>>` swapped on
/// `Settings → Indexers` edits. Mirrors [`CompiledCfCache`] —
/// outer `RwLock` owns swap; inner `Arc<Vec<_>>` is cheap-cloned
/// out on the search hot path so the read lock releases before the
/// per-query fan-out begins. Avoids rebuilding reqwest::Client
/// instances on every per-target search.
pub type IndexerCache = Arc<RwLock<Arc<Vec<Arc<dyn Indexer>>>>>;

/// Multi-client routing — id-keyed map of live trait impls plus
/// the per-protocol default ids. The whole struct swaps atomically
/// when the cache is rebuilt by
/// [`services::download_client::rebuild_clients_cache`] on
/// Settings → Connections → Downloads edits. Lookup at grab time
/// is a `HashMap::get` against the inner `Arc` — read lock releases
/// before the dispatch.
///
/// `default_torrent_id` and `default_usenet_id` are the row ids of
/// the `is_default = 1` rows at build time, scoped per protocol.
/// Each is `None` when no client of that protocol is configured
/// (or when every row of that protocol was disabled). The pin-
/// resolution helpers ([`AppState::client_for_indexer`] etc.) fall
/// back to the matching protocol's default — a torznab indexer with
/// no pin routes to `default_torrent_id`; a newznab indexer routes
/// to `default_usenet_id`.
#[derive(Default)]
pub struct DownloadClientPool {
    pub clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>>,
    pub default_torrent_id: Option<i64>,
    pub default_usenet_id: Option<i64>,
}

impl DownloadClientPool {
    /// The default-client id for the given wire protocol
    /// (`"torrent"` / `"usenet"`). Anything else returns None.
    pub fn default_for_protocol(&self, protocol: &str) -> Option<i64> {
        match protocol {
            "torrent" => self.default_torrent_id,
            "usenet" => self.default_usenet_id,
            _ => None,
        }
    }
}

/// Same swap-on-write shape as `IndexerCache` / `CompiledCfCache`
/// — outer `RwLock` owns the swap; inner `Arc<DownloadClientPool>`
/// is cheap-cloned out on the grab-dispatch path so the read
/// lock releases before any HTTP calls.
pub type DownloadClientsCache = Arc<RwLock<Arc<DownloadClientPool>>>;

/// Shared application state available to all handlers. Lives in the
/// library crate (rather than `main.rs`) so integration tests can
/// build instances of it without depending on the binary.
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    /// Multi-client routing pool. Replaced the single-slot
    /// `download_client` field — see [`DownloadClientPool`].
    /// Rebuilt on Settings → Downloads add/edit/delete via
    /// `services::download_client::rebuild_clients_cache`. Pin
    /// resolution at grab time goes through
    /// [`AppState::client_for_indexer`].
    pub download_clients: DownloadClientsCache,
    pub jellyfin: Arc<RwLock<Option<JellyfinClient>>>,
    /// Compiled Custom Formats, loaded once at startup and rebuilt on
    /// CF create/update/delete via `custom_formats::rebuild_cf_cache`.
    /// Outer `RwLock` owns swap; the inner `Arc<Vec<_>>` is cheap-cloned
    /// out on the scoring hot path so the read lock releases before the
    /// per-candidate evaluation loop begins.
    pub custom_formats: CompiledCfCache,
    /// Cached indexer clients (PR #107 review fix #4). Same swap-
    /// on-write pattern as `custom_formats` — rebuilt by the
    /// Settings → Indexers handlers on add/edit/delete, and read
    /// lock-free via `Arc::clone` on the search path. Avoids
    /// rebuilding reqwest clients per search.
    pub indexers: IndexerCache,
    /// In-memory progress registry for long-running user-triggered jobs
    /// (currently the manual auto-search). The frontend mints an opaque
    /// `progress_id`, the trigger handler binds it via
    /// `register(...).await`, and the polling endpoint at
    /// `/api/progress/{id}` drains buffered events. See
    /// `services::progress` for the full lifecycle.
    pub progress: ProgressRegistry,
    /// Flip-to-true-once cache of `user::has_users`. The auth middleware
    /// runs on every protected request and was firing a `SELECT COUNT(*)
    /// FROM users` query for each one just to decide whether to redirect
    /// to `/setup`. Because Ryokan never deletes the admin account, once
    /// this flag is true it stays true for the life of the process, and
    /// the check becomes a lock-free atomic load. While false, the
    /// middleware still hits the DB on the setup-pending path so a fresh
    /// `/setup` submission is picked up on the very next request.
    pub users_exist: Arc<std::sync::atomic::AtomicBool>,
    /// 5-minute TTL cache for interactive-search results so rapid
    /// reloads of the modal during UI iteration reuse the previous
    /// Nyaa hit. Scoped to interactive-search only; auto-search,
    /// RSS, and manual grabs continue to hit Nyaa directly. See
    /// [`services::interactive_search_cache`] for key shape + TTL.
    pub interactive_search_cache: InteractiveSearchCache,
    /// In-memory store for pending OAuth attempts (issue #62).
    /// Holds the PKCE verifier between MAL's `/start` and `/submit`;
    /// 10-minute TTL sweeps forgotten flows. See
    /// [`services::oauth_state`] for scope + lifecycle.
    pub oauth_state: OAuthStateStore,
    /// Wall-clock timestamp captured at process boot. Used by the
    /// Sonarr/Radarr shims' `system_status` endpoint so Seerr's UI
    /// pill reports the actual time the connected app came online —
    /// the prior hardcoded "2024-01-01T00:00:00Z" effectively claimed
    /// the indexer had been up for over a year regardless of when
    /// Ryokan was last restarted, which made the pill useless as a
    /// liveness signal.
    pub start_time: chrono::DateTime<chrono::Utc>,
    /// Lifecycle metadata for every supervised background task.
    /// Each `supervise()` loop registers itself here and updates
    /// status atomically on every iteration; `/api/system/tasks`
    /// reads the snapshot for the System page. See
    /// [`services::task_registry`] for the registry's threading
    /// model (lock-free hot path, snapshot-on-read).
    pub tasks: TaskRegistry,
    /// Per-client probe-status cache for the Settings → Connections
    /// status pills. Each card on the Download Clients tab fires a
    /// `hx-trigger="load"` GET to `/api/download-clients/{id}/status`;
    /// without this cache, every page load AND every hx-boost-nav
    /// into the tab re-runs the network probe (typically 50-500ms
    /// for healthy clients, up to 5s for unreachable ones), and the
    /// "Probing…" placeholder pills flash to real status pills at
    /// staggered times → user perceives flashing on the cards. With
    /// the cache, fresh entries (within TTL) get rendered server-side
    /// in the list partial directly and bypass the probe entirely.
    /// In-memory only; rebuilds on process restart and on every
    /// `download_clients` row CRUD (the rebuild step also wipes the
    /// cache for the affected id so a fresh edit re-probes
    /// immediately).
    pub dc_status_cache: DcStatusCache,
    /// Issue #118 — outbound notification providers cache. Same
    /// swap-on-write shape as `custom_formats` / `indexers` /
    /// `download_clients`: outer `RwLock` owns the swap; the inner
    /// `Arc<Vec<_>>` is cheap-cloned out on every dispatch so the
    /// read lock releases before the per-provider fan-out begins.
    /// Foundation issue ships an always-empty cache (no provider
    /// impls yet) — `services::notifications::dispatch` early-
    /// returns on empty so every hook point is a no-op until the
    /// per-provider issues (#119 webhook, #120 Discord) land.
    pub notification_providers: NotificationProviders,
    /// #122 — in-memory manual-import preview sessions, keyed by the
    /// opaque id in `/system/import?session=<id>`. Same
    /// `Arc<Mutex<HashMap>>` shape as `interactive_search_cache`:
    /// idle sessions evict on the next access (2h TTL, 8-session cap).
    /// Holds walk results plus the user's match / file decisions
    /// between wizard steps; nothing in it is persisted.
    pub import_sessions: ImportSessionStore,
}

/// `(probed_at, version-or-error)` keyed by `download_clients.id`.
/// `Instant` not `SystemTime` so the TTL check is monotonic across
/// system clock adjustments. The status itself is the same shape
/// the probe handler returns (Some(version) on success, error
/// string on failure).
pub type DcStatusCache =
    Arc<std::sync::Mutex<std::collections::HashMap<i64, (std::time::Instant, DcStatusEntry)>>>;

#[derive(Clone, Debug)]
pub struct DcStatusEntry {
    pub version: Option<String>,
    pub error: String,
}

/// TTL for the DC status cache. 10 minutes covers a normal
/// "open Settings, configure other tabs, come back" loop without
/// re-probing — the 60s the cache shipped with was too short for
/// users who dwell on Indexers / Custom Formats between trips back
/// to Connections, so they still saw a "Probing…" flash on every
/// re-entry. The cache is wiped explicitly on every download_clients
/// CRUD so a credential edit re-probes immediately rather than
/// masking failures for the full TTL.
pub const DC_STATUS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

impl AppState {
    /// Resolve a download client for a grab attributable to
    /// `indexer_id`. Pin chain:
    ///
    /// 1. The indexer row's `download_client_id`, if set.
    /// 2. The pool's default client id.
    /// 3. None — caller surfaces "no download client configured."
    ///
    /// Reads the indexer's pin from the `IndexerCache` snapshot
    /// (no DB roundtrip on the grab path). Falls through to
    /// the default when the pinned client id no longer exists
    /// (e.g. user deleted the client without re-pinning the
    /// indexer somehow — shouldn't happen because `delete()`
    /// NULLs the pin, but the fall-through keeps grabs flowing
    /// rather than 500ing).
    pub async fn client_for_indexer(
        &self,
        indexer_id: Option<i64>,
    ) -> Option<Arc<dyn DownloadClient>> {
        self.client_for_indexer_with_id(indexer_id)
            .await
            .map(|(c, _)| c)
    }

    /// Same resolution as [`Self::client_for_indexer`] but also
    /// returns the resolved `download_clients.id` so callers can
    /// stamp it on `grabbed_torrents.download_client_id`.
    /// Post-processing routes per-grab through that id back to the
    /// owning client.
    ///
    /// Default-fallback is per-protocol: a torznab indexer with no
    /// pin lands on `default_torrent_id`; a newznab indexer lands on
    /// `default_usenet_id`. When the indexer's protocol can't be
    /// derived (unknown kind, indexer not in the cache snapshot) —
    /// or when no pin context is given at all — falls through to
    /// the torrent default since every Ryokan-internal default-only
    /// caller is torrent-shaped (Nyaa search, manual grabs, library
    /// re-grab buttons).
    pub async fn client_for_indexer_with_id(
        &self,
        indexer_id: Option<i64>,
    ) -> Option<(Arc<dyn DownloadClient>, i64)> {
        let pool = self.download_clients.read().await.clone();
        let mut protocol: &str = "torrent";
        if let Some(id) = indexer_id {
            let indexers = self.indexers.read().await.clone();
            if let Some(idx) = indexers.iter().find(|i| i.id() == id) {
                if let Some(pinned) = idx.download_client_id()
                    && let Some(client) = pool.clients.get(&pinned)
                {
                    return Some((client.clone(), pinned));
                }
                if let Some(p) =
                    crate::services::download_client::protocol_for_indexer_kind(idx.kind())
                {
                    protocol = p;
                }
            }
        }
        let default_id = pool.default_for_protocol(protocol)?;
        let client = pool.clients.get(&default_id)?.clone();
        Some((client, default_id))
    }

    /// Resolve a download client for the built-in Nyaa search
    /// (no `indexers` row). Reads
    /// `config.nyaa_download_client_id` and falls back to the
    /// default. Caller must pass the current config so the
    /// helper doesn't fire a DB query per grab.
    pub async fn client_for_nyaa(&self, nyaa_pin: Option<i64>) -> Option<Arc<dyn DownloadClient>> {
        self.client_for_nyaa_with_id(nyaa_pin).await.map(|(c, _)| c)
    }

    /// Same resolution as [`Self::client_for_nyaa`] but also returns
    /// the resolved `download_clients.id` for grab-row stamping.
    /// Always falls back to the torrent default — Nyaa items are
    /// magnets / .torrent URLs, so a usenet-default fallback would
    /// just trip the protocol guard at add-time anyway.
    pub async fn client_for_nyaa_with_id(
        &self,
        nyaa_pin: Option<i64>,
    ) -> Option<(Arc<dyn DownloadClient>, i64)> {
        let pool = self.download_clients.read().await.clone();
        if let Some(pinned) = nyaa_pin
            && let Some(client) = pool.clients.get(&pinned)
        {
            return Some((client.clone(), pinned));
        }
        let default_id = pool.default_torrent_id?;
        let client = pool.clients.get(&default_id)?.clone();
        Some((client, default_id))
    }

    /// Default client — used by paths that don't have an
    /// indexer / Nyaa pin context (post-processing on a grab
    /// whose indexer was deleted, manual grabs, etc.). Same
    /// resolution as the helpers above with a None pin: just
    /// the default.
    pub async fn default_download_client(&self) -> Option<Arc<dyn DownloadClient>> {
        self.default_download_client_with_id().await.map(|(c, _)| c)
    }

    /// Same resolution as [`Self::default_download_client`] but also
    /// returns the resolved id for grab-row stamping. Mirror of the
    /// `_with_id` helpers above. Returns the **torrent** default —
    /// every internal call site that hits this helper is torrent-
    /// flavored (Nyaa search, manual grabs, library episode
    /// re-grab, post-processing torrent lookup, RSS / upgrade
    /// "is anything configured" gates). Usenet routing always goes
    /// through the indexer's pin (or its protocol's per-pin default
    /// via `client_for_indexer_with_id`).
    pub async fn default_download_client_with_id(&self) -> Option<(Arc<dyn DownloadClient>, i64)> {
        let pool = self.download_clients.read().await.clone();
        let default_id = pool.default_torrent_id?;
        let client = pool.clients.get(&default_id)?.clone();
        Some((client, default_id))
    }

    /// Look up a specific client by `download_clients.id`. Used by
    /// post-processing's per-grab routing — `grabbed_torrents.download_client_id`
    /// stamps the row, and this helper resolves it back to the live
    /// client. Returns None when the row referenced was deleted from
    /// the pool (e.g. user removed the client mid-import); caller
    /// should fall back to default.
    pub async fn client_by_id(&self, id: i64) -> Option<Arc<dyn DownloadClient>> {
        let pool = self.download_clients.read().await.clone();
        pool.clients.get(&id).cloned()
    }

    /// Resolve a grab to its handling client given the `download_client_id`
    /// stamp (when present) and the hash. Falls back through three layers:
    ///
    ///   1. The stamped client id, if it still exists in the pool.
    ///   2. **Hash-shape heuristic** — SAB's `nzo_id` format
    ///      (`SABnzbd_nzo_…`) is unmistakable. Old grabs from before
    ///      grab-time stamping was wired (the `ALTER TABLE … ADD COLUMN
    ///      download_client_id` migration runs without a backfill) have
    ///      a NULL stamp, and naively falling through to the torrent
    ///      default sends an nzo_id to qBit's `delete` endpoint, which
    ///      silently 200s on unknown hashes — the user's symptom is
    ///      "delete-from-disk leaves the SAB job alive forever." Route
    ///      SAB-shaped hashes to ANY usenet client in the pool instead.
    ///   3. The torrent default (the legacy fall-through; correct for
    ///      BT v1 infohashes — 40-char hex, no SAB-style prefix).
    pub async fn resolve_grab_client(
        &self,
        download_client_id: Option<i64>,
        hash: &str,
    ) -> Option<Arc<dyn DownloadClient>> {
        self.resolve_grab_client_with_id(download_client_id, hash)
            .await
            .map(|(_, c)| c)
    }

    /// [`Self::resolve_grab_client`] with the resolved pool id alongside,
    /// for callers that key per-client state on it (the #228 sweep).
    pub async fn resolve_grab_client_with_id(
        &self,
        download_client_id: Option<i64>,
        hash: &str,
    ) -> Option<(i64, Arc<dyn DownloadClient>)> {
        let pool = self.download_clients.read().await.clone();
        if let Some(id) = download_client_id
            && let Some(client) = pool.clients.get(&id)
        {
            return Some((id, client.clone()));
        }
        if hash.starts_with("SABnzbd_nzo_") {
            for (id, c) in &pool.clients {
                if c.protocol() == "usenet" {
                    return Some((*id, c.clone()));
                }
            }
        }
        let id = pool.default_torrent_id?;
        let client = pool.clients.get(&id)?.clone();
        Some((id, client))
    }
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> SqlitePool {
        state.db.clone()
    }
}

#[cfg(test)]
mod resolve_grab_client_tests {
    use super::*;
    use crate::services::download_client::{
        AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
    };
    use async_trait::async_trait;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::RwLock;

    /// Minimal `DownloadClient` mock that lets a test pin the
    /// `protocol()` return value. Every other method is a no-op stub.
    struct ProtoClient(&'static str);

    #[async_trait]
    impl DownloadClient for ProtoClient {
        async fn test(&self) -> Result<String, String> {
            Ok("ok".into())
        }
        async fn add_torrent(&self, _url: &str, _hash: &str) -> Result<AddOutcome, String> {
            Ok(AddOutcome::Added)
        }
        async fn add_torrent_with_file_filter(
            &self,
            _url: &str,
            _hash: &str,
            _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
        ) -> Result<SelectiveOutcome, String> {
            Ok(SelectiveOutcome::FullDownload)
        }
        async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
            Ok(vec![])
        }
        async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
            Ok(vec![])
        }
        async fn pause(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
        async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
            Ok(())
        }
        async fn set_file_wanted(
            &self,
            _hash: &str,
            _files: &[usize],
            _wanted: bool,
        ) -> Result<(), String> {
            Ok(())
        }
        fn sonarr_impl_name(&self) -> &'static str {
            self.0
        }
        fn protocol(&self) -> &'static str {
            self.0
        }
    }

    fn build_state(default_torrent_id: Option<i64>) -> AppState {
        let mut clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>> =
            std::collections::HashMap::new();
        clients.insert(1, Arc::new(ProtoClient("torrent")));
        clients.insert(2, Arc::new(ProtoClient("usenet")));
        let pool = DownloadClientPool {
            clients,
            default_torrent_id,
            default_usenet_id: Some(2),
        };
        AppState {
            db: sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy pool"),
            download_clients: Arc::new(RwLock::new(Arc::new(pool))),
            jellyfin: Arc::new(RwLock::new(None)),
            custom_formats: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            indexers: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            progress: crate::services::progress::ProgressRegistry::new(),
            users_exist: Arc::new(AtomicBool::new(true)),
            interactive_search_cache: crate::services::interactive_search_cache::new(),
            oauth_state: crate::services::oauth_state::new(),
            start_time: chrono::Utc::now(),
            tasks: crate::services::task_registry::TaskRegistry::new(),
            dc_status_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            notification_providers: crate::services::notifications::empty_cache(),
            import_sessions: crate::services::manual_import::session::new_store(),
        }
    }

    /// Stamped `download_client_id` always wins, even for a SAB-shaped
    /// hash that the heuristic would otherwise re-route.
    #[tokio::test]
    async fn stamped_id_wins_over_hash_heuristic() {
        let state = build_state(Some(1));
        // Pin to the torrent client (id=1) even though the hash looks
        // SAB-shaped — the stamp is authoritative.
        let client = state
            .resolve_grab_client(Some(1), "SABnzbd_nzo_abcdef12")
            .await
            .expect("resolved");
        assert_eq!(client.protocol(), "torrent");
    }

    /// Legacy NULL stamp + SAB-shaped hash routes to a usenet client
    /// in the pool. Without this the user's existing SAB grabs (made
    /// before grab-time stamping was wired) silently route to qBit's
    /// `delete`, which 200s on unknown hashes and leaves the SAB job
    /// alive forever.
    #[tokio::test]
    async fn null_stamp_with_sab_hash_routes_to_usenet_client() {
        let state = build_state(Some(1));
        let client = state
            .resolve_grab_client(None, "SABnzbd_nzo_4rxsukkq")
            .await
            .expect("resolved");
        assert_eq!(client.protocol(), "usenet");
    }

    /// Legacy NULL stamp + BT-shaped hash falls through to the torrent
    /// default — no false-positive on a 40-char-hex infohash.
    #[tokio::test]
    async fn null_stamp_with_bt_hash_falls_through_to_torrent_default() {
        let state = build_state(Some(1));
        let client = state
            .resolve_grab_client(None, "abc123def456abc123def456abc123def4567890")
            .await
            .expect("resolved");
        assert_eq!(client.protocol(), "torrent");
    }

    /// Stamped id that no longer exists (client was deleted) falls
    /// back through the heuristic. Same SAB rescue as the NULL case.
    #[tokio::test]
    async fn stamped_id_missing_from_pool_still_routes_via_heuristic() {
        let state = build_state(Some(1));
        let client = state
            .resolve_grab_client(Some(999), "SABnzbd_nzo_abcdef12")
            .await
            .expect("resolved");
        assert_eq!(client.protocol(), "usenet");
    }
}
