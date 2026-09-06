//! Pluggable download-client abstraction.
//!
//! Ryokan addresses torrents by **v1 infohash** throughout, extracted
//! client-side from the magnet URL before any download client is
//! called. All BT clients key torrents by the same v1 infohash, so the
//! trait uses it as the canonical item ID. Each impl is responsible
//! for case-normalization (qBit/Deluge/Transmission want lowercase hex;
//! rtorrent wants uppercase).
//!
//! Trait contracts:
//!   - `info_hash: &str` parameters are **always lowercase hex** (40
//!     chars). Each impl case-converts internally for its wire format.
//!     Callers never case-munge.
//!   - `Result<_, String>` error type matches the existing project
//!     convention. Precludes caller-side retry policy based on error
//!     class; accepted until smart retry becomes a real feature.
//!   - All mutating operations are idempotent against repeated calls
//!     with the same `info_hash`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[cfg(test)]
pub(crate) mod test_helpers;

pub mod qbittorrent;

/// Stub Deluge implementation. Exists during Phase 1 to compile-check
/// that the `DownloadClient` trait is object-safe and doesn't bake
/// qBit-specific assumptions into the method shapes. All methods
/// except `test()` and `sonarr_impl_name()` are `unimplemented!()` —
/// not wired into `AppState` or Settings UI. Phase 2 replaces with
/// the real impl.
pub mod deluge;

/// rtorrent via XML-RPC. See module docs for wire-format quirks
/// (uppercase hash, `d.update_priorities` flush, silent duplicate-add,
/// `d.erase` not touching disk, `.meta` sentinel in base_path).
pub mod rtorrent;

pub mod transmission;

pub mod sabnzbd;

#[async_trait]
pub trait DownloadClient: Send + Sync {
    /// Test connection and return the client's version string.
    async fn test(&self) -> Result<String, String>;

    /// Add a torrent by magnet / HTTP `.torrent` URL. `info_hash` is
    /// the v1 infohash Ryokan pre-computed from the magnet; impls may
    /// use it for idempotency checks and addressing.
    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String>;

    /// Adopt-style add that returns the canonical client-side id
    /// alongside the outcome. Mirrors Sonarr's `Download(...) -> string`:
    /// the caller doesn't have to know whether the id is a v1 infohash
    /// or a SAB `nzo_id` — the client tells it. Returned id is what
    /// Ryokan persists on `grabbed_torrents.hash` and what every
    /// subsequent op (`list_scoped`, `get_files`, `delete`, etc.)
    /// receives.
    ///
    /// Default impl: forwards to [`add_torrent`] and returns the
    /// caller's pre-computed `info_hash`. BT impls (qBit, Deluge,
    /// Transmission, rtorrent) all use this default — the v1 infohash
    /// IS the canonical id at the wire level. Only impls whose wire
    /// id can't be derived from the URL alone need to override —
    /// SAB returns the `nzo_id` from the queue add response;
    /// hypothetical NZBGet would return its `NzbId`.
    async fn add_torrent_returning_id(
        &self,
        url: &str,
        info_hash: &str,
    ) -> Result<(AddOutcome, String), String> {
        let outcome = self.add_torrent(url, info_hash).await?;
        Ok((outcome, info_hash.to_string()))
    }

    /// Add a torrent in a state where **file data does not actively
    /// download until the caller resumes**. Entry point for the
    /// interactive file picker (#83) — the handler shows the user the
    /// file list, the user picks, then the handler calls
    /// [`set_file_wanted`](Self::set_file_wanted) to mark unwanted
    /// files as skipped and [`resume`](Self::resume) to start
    /// downloading the wanted subset.
    ///
    /// Post-condition: the torrent is in the client's "not consuming
    /// peer bandwidth for file data" state. For Deluge, Transmission,
    /// and rTorrent this maps cleanly to the native `paused` flag;
    /// metadata continues to arrive over DHT/peers while the torrent
    /// is paused.
    ///
    /// **qBit 5.x leaky abstraction:** qBit `v5.x` stopped torrents
    /// don't publish their file list through `/torrents/files` (the
    /// "paused → no metadata" quirk documented in
    /// `qbittorrent.rs`'s `add_torrent_with_file_filter` header). The
    /// qBit impl works around this by adding running, waiting for
    /// metadata up to a fixed budget, then calling
    /// `set_file_wanted(all_indices, wanted=false)` to skip every
    /// file before returning. From the caller's perspective the
    /// post-condition holds — no file data is being downloaded — but
    /// the call blocks for the metadata-fetch duration (typically
    /// 1-3s for `.torrent` URLs, up to `~10s` for DHT-dependent
    /// magnets). Callers that want to avoid stalling the request
    /// thread should run this inside a `tokio::spawn` and surface
    /// progress separately.
    ///
    /// Default implementation provided for compatibility so impls can
    /// adopt the picker incrementally; the default is best-effort and
    /// may leave a torrent running with all files downloading on
    /// impls that haven't overridden it.
    async fn add_torrent_paused(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        self.add_torrent(url, info_hash).await
    }

    /// Add a torrent and narrow it to a subset of its files. The `pick`
    /// callback receives the file names and returns the indices to
    /// keep (or `None` for a full grab). Each impl handles its own
    /// metadata-ready wait internally and must be idempotent: if a
    /// prior grab left this hash with priorities already set, re-narrow
    /// must not clobber user edits (use per-file `wanted` readback).
    ///
    /// The `pick` callback is `&mut dyn FnMut` rather than a generic
    /// `<F: FnMut>` to keep the trait object-safe — generic trait
    /// methods break `dyn DownloadClient`. Callers typically bind a
    /// closure and pass `&mut closure`.
    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        info_hash: &str,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String>;

    /// List torrents scoped to Ryokan's owned set only. Each impl
    /// defines "owned" — qBit uses `?category=`; Deluge the Label
    /// plugin; Transmission either native labels (4.x) or a
    /// save-path prefix; rtorrent the `custom1` field convention.
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String>;

    /// Files inside a torrent. Returns an empty `Vec` while metadata
    /// is still being fetched (each impl signals "not ready"
    /// differently — qBit returns 404; Transmission reports empty
    /// `files` with `metadataPercentComplete < 1.0`; Deluge has
    /// `has_metadata == false`). Trait contract normalizes all of
    /// these to "empty = not ready." See [`wait_for_files`] for the
    /// corresponding polling helper.
    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String>;

    async fn pause(&self, info_hash: &str) -> Result<(), String>;
    async fn resume(&self, info_hash: &str) -> Result<(), String>;
    async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), String>;

    /// Set per-file wanted/unwanted. Binary is sufficient — Ryokan
    /// only uses priority 0 (skip) and "normal" (include); qBit's
    /// higher priority levels are never written from Ryokan. Each
    /// impl maps `wanted: bool` to its native representation
    /// (qBit: `0` vs `1`; Deluge: `0` vs `4`; Transmission:
    /// `files-unwanted` vs `files-wanted`; rtorrent: `f.priority.set`
    /// 0 vs 1 *and* a mandatory `d.update_priorities` call).
    async fn set_file_wanted(
        &self,
        info_hash: &str,
        files: &[usize],
        wanted: bool,
    ) -> Result<(), String>;

    /// Sonarr-canonical implementation name for the
    /// `/api/v3/downloadclient` shim response. Values:
    /// `"QBittorrent" | "Deluge" | "Transmission" | "RTorrent"
    /// | "Sabnzbd"`. Distinct from the `active_client`
    /// discriminator (lowercase-snake: `"qbittorrent"` etc.).
    fn sonarr_impl_name(&self) -> &'static str;

    /// PR 112 review #2 — protocol of the client. `"torrent"` for
    /// BT impls (default), `"usenet"` for SAB. The Sonarr/Radarr
    /// `/api/v3/downloadclient` shim emits this verbatim so a
    /// SAB-as-default install reports `"usenet"` correctly
    /// instead of the previously hardcoded `"torrent"`. Mirror
    /// of the `protocol_for_client_kind` helper at the row-kind
    /// layer.
    fn protocol(&self) -> &'static str {
        "torrent"
    }

    /// Issue #28 — apply per-torrent seed-rule overrides
    /// after [`add_torrent`]. Caller invokes this immediately after
    /// the grab when the source indexer has seed rules configured;
    /// the client enforces the rule on its own (Ryokan doesn't
    /// poll seed state to decide when to stop).
    ///
    /// Per-impl wire mapping (issue #228 corrected two of these):
    /// - **qBit**: `POST /torrents/setShareLimits` with
    ///   `ratioLimit` (`-2` = use the global limit, `-1` = no limit,
    ///   float = set) and `seedingTimeLimit` (`-2`/`-1`/minutes).
    /// - **Deluge**: `core.set_torrent_options` with
    ///   `stop_at_ratio: true` + `stop_ratio` for ratio; idle-time
    ///   stop is not supported by Deluge core, time_minutes is a
    ///   no-op there with a debug log.
    /// - **Transmission**: `torrent-set` with `seedRatioLimit` +
    ///   `seedRatioMode: 1` (override global) for ratio,
    ///   `seedIdleLimit` + `seedIdleMode: 1` for `time_minutes`,
    ///   which Transmission reads as minutes of inactivity.
    /// - **rTorrent**: not supported. Its only per-item ratio
    ///   command is the read-only `d.ratio`; ratios are configured
    ///   per group in `.rtorrent.rc`. The impl returns `Err` without
    ///   a wire call so the gap is logged per grab.
    ///
    /// `Option`-wrapped fields mean "no rule configured for this
    /// dimension." Per-impl handling diverges:
    /// - **Deluge / Transmission** leave the per-torrent setting
    ///   untouched on `None` — they only write the field they were
    ///   given.
    /// - **qBit's `setShareLimits` always writes BOTH `ratioLimit`
    ///   and `seedingTimeLimit`** in one call (the API takes them
    ///   as a pair); `None` is translated to `-2` (use the global
    ///   limit), which means a previously-set per-torrent ratio
    ///   would be reset to global if `set_seed_rules` is later
    ///   called with `ratio: None`.
    ///
    /// In practice this is fine because the call site is
    /// [`apply_indexer_seed_rules`] — invoked exactly once per
    /// grab, immediately after `add_torrent`, on a torrent that
    /// has no prior per-torrent overrides. If a future caller
    /// updates rules on a long-lived torrent, the qBit divergence
    /// becomes load-bearing and the impl will need a read-then-
    /// write.
    ///
    /// Default impl is a no-op so impls can adopt the trait method
    /// incrementally and the build doesn't break the moment a new
    /// client lands. Callers must not assume seed rules took effect
    /// for clients that haven't overridden this — the
    /// `respect_seed_rules` grab-row flag exists separately.
    async fn set_seed_rules(&self, _info_hash: &str, _rules: SeedRules) -> Result<(), String> {
        Ok(())
    }
}

/// Per-torrent seed-rule overrides. Both fields are optional —
/// `None` means "don't change this rule on the client side."
/// Issue #28 maps these to the client's native API per the
/// trait method's per-impl mapping table.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeedRules {
    /// Stop seeding when the ratio reaches this value. `None`
    /// leaves the client's global ratio policy in place.
    pub ratio: Option<f64>,
    /// Stop seeding after this many minutes of seed time. `None`
    /// leaves the client's global time policy in place. Not all
    /// clients support this — see the trait method header.
    pub time_minutes: Option<u64>,
}

impl SeedRules {
    /// Construct from a [`crate::models::indexers::Indexer`] row.
    /// Maps the row's `seed_ratio` and `seed_time_minutes` columns
    /// directly. Returns a no-op `SeedRules` (both fields None)
    /// when the row has no rules configured — callers can call
    /// [`is_empty`] to skip the wire call entirely.
    pub fn from_indexer_row(row: &crate::models::indexers::Indexer) -> Self {
        Self {
            ratio: row.seed_ratio,
            time_minutes: row.seed_time_minutes.map(|n| n.max(0) as u64),
        }
    }

    /// True when no rules are configured; the caller can skip the
    /// wire call entirely.
    pub fn is_empty(&self) -> bool {
        self.ratio.is_none() && self.time_minutes.is_none()
    }
}

/// Issue #28 — apply per-indexer seed rules after a successful
/// `add_torrent`. Looks up the indexer row by id, builds a
/// [`SeedRules`], and calls the trait method.
///
/// Returns `true` when rules were attempted (regardless of wire
/// success) so the caller can flip `grabbed_torrents.respect_seed_rules
/// = 1`. The flag tracks "this grab carries indexer-specific seed
/// rules" — even if the wire call failed, the user-configured intent
/// stands and the delete-path skip should still respect it.
///
/// Returns `false` for Nyaa grabs (`indexer_id == None`), grabs from
/// indexers without seed rules, or DB read failures. In all three
/// cases the grab behaves the same as a v1.4 Nyaa grab.
///
/// Wire-call failures log at `warn` and DON'T propagate — a
/// `setShareLimits` glitch shouldn't fail a successful grab. The
/// upgrade sweep can re-apply rules on the next pass if the user
/// observes the gap.
pub async fn apply_indexer_seed_rules(
    db: &sqlx::SqlitePool,
    client: &dyn DownloadClient,
    info_hash: &str,
    indexer_id: Option<i64>,
) -> bool {
    let Some(id) = indexer_id else {
        return false;
    };
    let row = match crate::models::indexers::get_by_id(db, id).await {
        Ok(Some(r)) => r,
        _ => return false,
    };
    let rules = SeedRules::from_indexer_row(&row);
    if rules.is_empty() {
        return false;
    }
    if let Err(e) = client.set_seed_rules(info_hash, rules).await {
        crate::services::logger::warn(
            db,
            crate::models::log::LogCategory::Grab,
            &format!("indexer #{} ({}): set_seed_rules failed", row.id, row.name),
            &format!("{info_hash}: {e}"),
        )
        .await;
    }
    true
}

/// Outcome of an [`DownloadClient::add_torrent`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// Client accepted the torrent fresh.
    Added,
    /// Client already had this infohash. qBit is silent about this;
    /// Transmission/Deluge/rtorrent surface a duplicate error that
    /// each impl catches and converts to this variant. Callers treat
    /// it as success — the torrent is in the client, which is the
    /// post-condition [`add_torrent`] promises.
    AlreadyPresent,
}

/// Outcome of an [`DownloadClient::add_torrent_with_file_filter`] call.
#[derive(Debug)]
pub enum SelectiveOutcome {
    /// Filter narrowed the torrent to specific files. Contains the
    /// kept file indices (always a strict subset of the file list).
    Filtered(Vec<usize>),
    /// No filter applied — the torrent is downloading all files.
    /// Used when the caller's `pick` returned `None`, when the pick
    /// matched every file (not a megapack after all), or when
    /// metadata fetch timed out and the impl resumed the already-
    /// added torrent unchanged instead of leaving it stuck paused.
    FullDownload,
}

/// A torrent as seen through the `DownloadClient` trait.
///
/// **JSON shape**: `state` is the client-native string (qBit:
/// `"stalledUP"`, Deluge: `"Downloading"`, Transmission: `"4"`,
/// rtorrent: computed), kept around for debug / tooltip use.
/// `state_kind` is the normalized cross-client enum — this is what
/// UI code should drive off so the Downloads page behaves the same
/// regardless of which client is active.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct DownloadItem {
    pub hash: String,
    pub name: String,
    pub size: i64,
    pub progress: f64,
    pub dlspeed: i64,
    /// Client-native state string. Exposed for diagnostics; UI code
    /// should prefer `state_kind` for cross-client consistency.
    pub state: String,
    pub category: String,
    pub eta: i64,
    #[serde(default)]
    pub save_path: String,
    /// Top-level path of the torrent's content (qBit ≥ 2.6.1 native;
    /// other impls compute from save_path + files' common prefix).
    /// Empty when metadata isn't ready yet.
    #[serde(default)]
    pub content_path: String,
    /// Normalized state across every download client. Serialized as a
    /// kebab-case slug (`"downloading"`, `"seeding-stalled"`, etc.) so
    /// the JS state-label + badge-class maps can live on the
    /// normalized vocabulary rather than any client's native strings.
    #[serde(default)]
    pub state_kind: DownloadItemState,
    /// The client has stopped this item because its own seeding rules
    /// are met (issue #228): qBit paused or stopped it at its effective
    /// ratio, seeding-time, or inactivity limit; Transmission reports
    /// `isFinished`; Deluge paused it at `stop_ratio`; rTorrent closed
    /// a complete item. `false` while seeding, for a user pause the
    /// client can tell apart, and always for usenet. Post-processing's
    /// finished-seed sweep removes an imported item once this is true.
    #[serde(default)]
    pub seeding_done: bool,
}

/// One file inside a torrent.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct DownloadFile {
    /// Relative path of the file within the torrent (from save_path).
    pub name: String,
    pub size: i64,
    pub progress: f64,
    /// `true` when this file is actively being downloaded;
    /// `false` when the client was told to skip it. Used by the
    /// "already narrowed" idempotency check in
    /// `add_torrent_with_file_filter` so re-narrow doesn't clobber
    /// user edits from a prior grab.
    pub wanted: bool,
}

/// Normalized torrent state — 11 variants that every download
/// client impl maps its native state vocabulary into. Serialized as
/// a kebab-case slug on the wire (`"downloading-stalled"`,
/// `"paused-complete"`, etc.) so the Downloads-page UI can key label
/// and badge-class maps off a single cross-client vocabulary rather
/// than mapping each client's distinct state strings separately.
/// Each impl maps its native state strings into this enum inside the
/// client's `list_scoped` implementation; `DownloadItem.state` keeps
/// the native string around for UI display so Phase 1 is a drop-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadItemState {
    #[default]
    Downloading,
    DownloadingStalled,
    DownloadingQueued,
    CheckingDownload,
    Seeding,
    SeedingStalled,
    SeedingQueued,
    CheckingSeed,
    /// Paused while still incomplete.
    Paused,
    /// Paused/stopped after completion — treat as complete for
    /// post-processing purposes.
    PausedComplete,
    Errored,
}

impl DownloadItemState {
    /// Downloaded, verified, and safe to import. Post-processing's
    /// completion check bottoms out here.
    pub fn is_complete(self) -> bool {
        matches!(
            self,
            Self::Seeding
                | Self::SeedingStalled
                | Self::SeedingQueued
                | Self::CheckingSeed
                | Self::PausedComplete
        )
    }

    pub fn is_errored(self) -> bool {
        matches!(self, Self::Errored)
    }
}

/// Return the per-client `<client>_download_path` for whichever
/// download client is currently active, based on `config.active_client`.
/// Empty string when the active client has no override configured —
/// translate_client_path treats that as "no rewrite."
pub fn per_client_download_path(config: &crate::models::config::Config) -> &str {
    match config.active_client.as_str() {
        "deluge" => &config.deluge_download_path,
        "transmission" => &config.transmission_download_path,
        "rtorrent" => &config.rtorrent_download_path,
        // qBittorrent is the default / unknown fallback.
        _ => &config.qbit_download_path,
    }
}

/// Translate a client-reported path into one Ryokan-on-host can
/// actually read. Replaces the client's `save_path` prefix with the
/// user-configured per-client `download_path`.
///
/// Examples:
///   - Deluge in a Docker container sees `/downloads/Show.mkv`; the
///     host volume is mounted at `/home/user/downloads-deluge`.
///     User sets `deluge_download_path = /home/user/downloads-deluge`.
///     Deluge reports `torrent.save_path = /downloads`. Calling this
///     with `path = /downloads/Show.mkv`, `client_save_path = /downloads`,
///     `local_download_path = /home/user/downloads-deluge` produces
///     `/home/user/downloads-deluge/Show.mkv`.
///   - qBit on the same host as Ryokan with no config override:
///     `local_download_path` is empty → returns `path` unchanged.
///
/// Trailing slashes on either prefix are normalized so
/// `/downloads` and `/downloads/` behave identically. If
/// `local_download_path` is empty, the input is returned unchanged
/// (no-override case). If the path doesn't start with
/// `client_save_path`, it's also returned unchanged — silently
/// rewriting an unexpected path is worse than surfacing the
/// mismatch as a downstream "file not found" error.
pub fn translate_client_path(
    path: &str,
    client_save_path: &str,
    local_download_path: &str,
) -> String {
    let remote = client_save_path.trim_end_matches('/');
    let local = local_download_path.trim_end_matches('/');
    if local.is_empty() {
        return path.to_string();
    }
    if remote.is_empty() {
        // The client didn't tell us a save_path prefix — all we can
        // do is return the original. Can happen for edge states
        // (metadata not yet arrived) but not for completed torrents.
        return path.to_string();
    }
    if let Some(rest) = path.strip_prefix(remote) {
        format!("{local}{rest}")
    } else {
        path.to_string()
    }
}

/// Construct the concrete download-client impl dictated by the
/// config's `active_client` discriminator. Returns `None` if the
/// active client's credentials are empty (user hasn't configured it
/// yet) — the caller leaves `AppState.download_client` at `None` and
/// the grab path surfaces "Download client not configured" errors.
///
/// Single construction point: both startup init (`main.rs`) and
/// settings save (`handlers::settings`) go through this so the
/// "which impl do we pick" logic lives in one place and the arm for
/// each client ships alongside its `mod deluge` / `mod qbittorrent`
/// etc. as Phase 3+ clients land.
/// Wire-protocol family a download-client kind handles. Lets the
/// indexer-pin save path enforce "torznab → torrent client; newznab
/// → usenet client" without scattering the kind→protocol mapping
/// across Settings handlers + UI templates. Mirrors the `Protocol`
/// discriminator Sonarr exposes on `IDownloadClient`.
///
/// Returns `None` for an unknown kind — callers treat that as
/// "permissive" since `rebuild_clients_cache` already rejects
/// unknown kinds at instantiation time, so an unrecognized kind
/// here just means the indexer-pin save path won't second-guess.
pub fn protocol_for_client_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "qbittorrent" | "deluge" | "transmission" | "rtorrent" => Some("torrent"),
        "sabnzbd" => Some("usenet"),
        _ => None,
    }
}

/// Mirror for `indexers.kind`. Torznab indexers surface torrent
/// magnets / `.torrent` URLs; newznab indexers surface NZB URLs.
/// Invalid combos (newznab → BT client, torznab → SAB) are rejected
/// at the indexer upsert / Nyaa-pin save path with a clear toast.
pub fn protocol_for_indexer_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "torznab" => Some("torrent"),
        "newznab" => Some("usenet"),
        _ => None,
    }
}

/// Multi-client routing — materialize every enabled row in
/// `download_clients` into a live `Arc<dyn DownloadClient>`,
/// keyed by row id. Used at startup and on Settings → Downloads
/// add/edit/delete to rebuild the cache.
///
/// Failed instantiations (bad URL, reqwest::Client build failure)
/// log + drop. The default client id is captured separately so
/// [`AppState::client_for_indexer`] can fall through to it
/// without re-querying the DB.
pub async fn rebuild_clients_cache(cache: &crate::DownloadClientsCache, db: &sqlx::SqlitePool) {
    use crate::DownloadClientPool;
    use crate::models::download_clients;
    use std::collections::HashMap;
    use std::sync::Arc;
    let rows = match download_clients::list_enabled(db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("download_clients: failed to load from DB: {e}");
            Vec::new()
        }
    };
    let mut clients: HashMap<i64, Arc<dyn DownloadClient>> = HashMap::new();
    // Per-protocol defaults — at most one of each. Captured during
    // the row iteration below, then patched up post-loop with a
    // lowest-id fallback per protocol so a manual DB edit that left
    // a protocol without an explicit default still routes somewhere
    // (matching the prior single-default behavior, just per-bucket).
    let mut default_torrent_id: Option<i64> = None;
    let mut default_usenet_id: Option<i64> = None;
    let mut all_torrent_ids: Vec<i64> = Vec::new();
    let mut all_usenet_ids: Vec<i64> = Vec::new();
    for row in rows {
        let client: Option<Arc<dyn DownloadClient>> = match row.kind.as_str() {
            "deluge" if !row.url.is_empty() => Some(Arc::new(deluge::DelugeClient::new(
                &row.url,
                &row.password,
                &row.label,
            ))),
            "transmission" if !row.url.is_empty() => {
                Some(Arc::new(transmission::TransmissionClient::new(
                    &row.url,
                    &row.username,
                    &row.password,
                    &row.label,
                )))
            }
            "rtorrent" if !row.url.is_empty() => Some(Arc::new(rtorrent::RtorrentClient::new(
                &row.url,
                &row.username,
                &row.password,
                &row.label,
            ))),
            "qbittorrent" if !row.url.is_empty() => Some(Arc::new(qbittorrent::QbitClient::new(
                &row.url,
                &row.username,
                &row.password,
                &row.label,
            ))),
            // SAB takes (url, username, api_key, category).
            // The `download_clients.password` column carries the SAB
            // API key for usenet rows (SAB has no per-user auth at
            // the API layer; the API key is the only credential).
            // Naming is awkward but reusing the column avoids a
            // schema change and the form already labels it "API key"
            // for the Usenet kind.
            "sabnzbd" if !row.url.is_empty() => Some(Arc::new(sabnzbd::SabClient::new(
                &row.url,
                &row.username,
                &row.password,
                &row.label,
            ))),
            other => {
                tracing::warn!(
                    "download_clients: skipping #{} ({}) — unknown / unsupported kind {:?}",
                    row.id,
                    row.name,
                    other,
                );
                None
            }
        };
        if let Some(c) = client {
            // Bucket by protocol so the per-protocol fallback below
            // can pick a lowest-id replacement when no row of that
            // protocol carries `is_default = 1`.
            match protocol_for_client_kind(&row.kind) {
                Some("torrent") => {
                    all_torrent_ids.push(row.id);
                    if row.is_default && default_torrent_id.is_none() {
                        default_torrent_id = Some(row.id);
                    }
                }
                Some("usenet") => {
                    all_usenet_ids.push(row.id);
                    if row.is_default && default_usenet_id.is_none() {
                        default_usenet_id = Some(row.id);
                    }
                }
                _ => {}
            }
            clients.insert(row.id, c);
        }
    }
    // Per-protocol fall-through: if rows of a protocol exist but none
    // carries `is_default = 1` (e.g. the user marked a kind+URL combo
    // as default but it failed to instantiate, or a manual DB edit
    // cleared every flag), pick the lowest surviving row id of that
    // protocol so the grab path isn't surprised by a present-but-
    // empty pool. Surface this in logs so a "I marked X as default
    // but grabs are landing on Y" report has an obvious culprit.
    if default_torrent_id.is_none()
        && let Some(min) = all_torrent_ids.iter().min().copied()
    {
        default_torrent_id = Some(min);
        tracing::warn!(
            "download_clients: no torrent row marked is_default=1; \
             picking client id {min} as fallback torrent default"
        );
    }
    if default_usenet_id.is_none()
        && let Some(min) = all_usenet_ids.iter().min().copied()
    {
        default_usenet_id = Some(min);
        tracing::warn!(
            "download_clients: no usenet row marked is_default=1; \
             picking client id {min} as fallback usenet default"
        );
    }
    let pool = Arc::new(DownloadClientPool {
        clients,
        default_torrent_id,
        default_usenet_id,
    });
    *cache.write().await = pool;
}

/// Poll `get_files` until non-empty or `timeout` elapses. 500ms
/// initial interval, doubling up to a 2s cap. Used by callers that
/// need the file list before proceeding (e.g. the 180s background
/// auto-expand wait in `handlers::library::search`). Impls that need
/// a wait internally (inside `add_torrent_with_file_filter`) may use
/// this or write their own — it's just a convenience over the trait
/// method.
pub async fn wait_for_files(
    client: &dyn DownloadClient,
    info_hash: &str,
    timeout: Duration,
) -> Result<Vec<DownloadFile>, String> {
    let start = Instant::now();
    let mut delay = Duration::from_millis(500);
    loop {
        match client.get_files(info_hash).await {
            Ok(files) if !files.is_empty() => return Ok(files),
            Ok(_) => {}
            Err(e) => {
                if start.elapsed() >= timeout {
                    return Err(format!("metadata fetch error after {:?}: {}", timeout, e));
                }
            }
        }
        if start.elapsed() >= timeout {
            return Err(format!("metadata fetch timed out after {:?}", timeout));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

/// Compute a torrent's `content_path` from its `save_path` and file
/// list, for clients (Deluge/Transmission/rtorrent) that don't expose
/// it natively the way qBit ≥ 2.6.1 does. Handles the three cases:
///
///   1. **Single-file torrent** (`files.len() == 1`, no `/` in the
///      name): `save_path + "/" + files[0].name`. Points at the
///      file itself.
///   2. **Multi-file with wrapping directory** (all `files[i].name`
///      share a non-empty prefix ending in `/`):
///      `save_path + "/" + common_prefix_dir`. Points at the folder.
///   3. **Multi-file dumped at save root** (no common prefix):
///      `content_path == save_path`.
///
/// Returns an empty string if `files` is empty (metadata not yet
/// known) — caller should check and retry.
///
/// qBit uses its native `content_path` field; the Deluge and
/// Transmission impls call this to derive the equivalent
/// client-agnostic path from `save_path` + the file list.
pub fn compute_content_path(save_path: &str, files: &[DownloadFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let save = save_path.trim_end_matches('/');
    if files.len() == 1 && !files[0].name.contains('/') {
        return format!("{save}/{}", files[0].name);
    }
    let first = &files[0].name;
    let Some(slash_idx) = first.find('/') else {
        return save.to_string();
    };
    let candidate = &first[..=slash_idx];
    if files.iter().all(|f| f.name.starts_with(candidate)) {
        return format!("{save}/{}", candidate.trim_end_matches('/'));
    }
    save.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str) -> DownloadFile {
        DownloadFile {
            name: name.to_string(),
            size: 0,
            progress: 0.0,
            wanted: true,
        }
    }

    #[test]
    fn content_path_single_file_points_at_file() {
        let files = vec![f("release.mkv")];
        assert_eq!(
            compute_content_path("/downloads", &files),
            "/downloads/release.mkv"
        );
    }

    #[test]
    fn content_path_multi_file_with_wrapper_points_at_folder() {
        let files = vec![
            f("[Group] Show/01.mkv"),
            f("[Group] Show/02.mkv"),
            f("[Group] Show/03.mkv"),
        ];
        assert_eq!(
            compute_content_path("/downloads", &files),
            "/downloads/[Group] Show"
        );
    }

    #[test]
    fn content_path_multi_file_dumped_at_root_is_save_path() {
        let files = vec![f("01.mkv"), f("02.mkv"), f("03.mkv")];
        assert_eq!(compute_content_path("/downloads", &files), "/downloads");
    }

    #[test]
    fn content_path_mixed_folders_no_common_prefix() {
        let files = vec![f("folder_a/01.mkv"), f("folder_b/02.mkv"), f("bare.mkv")];
        assert_eq!(compute_content_path("/downloads", &files), "/downloads");
    }

    #[test]
    fn content_path_normalizes_trailing_slash_on_save_path() {
        let files = vec![f("release.mkv")];
        assert_eq!(
            compute_content_path("/downloads/", &files),
            "/downloads/release.mkv"
        );
    }

    #[test]
    fn content_path_empty_files_returns_empty() {
        assert_eq!(compute_content_path("/downloads", &[]), "");
    }

    #[test]
    fn translate_client_path_rewrites_matching_prefix() {
        assert_eq!(
            translate_client_path("/downloads/anime/file.mkv", "/downloads", "/mnt/seedbox"),
            "/mnt/seedbox/anime/file.mkv"
        );
    }

    #[test]
    fn translate_client_path_trims_trailing_slashes() {
        // Trailing-slash normalization on both sides should produce
        // identical output — prevents the /downloads vs /downloads/
        // foot-gun that bites every Sonarr remote-path setup.
        assert_eq!(
            translate_client_path("/downloads/x.mkv", "/downloads/", "/mnt/seedbox/"),
            "/mnt/seedbox/x.mkv"
        );
        assert_eq!(
            translate_client_path("/downloads/x.mkv", "/downloads", "/mnt/seedbox"),
            "/mnt/seedbox/x.mkv"
        );
    }

    #[test]
    fn translate_client_path_empty_local_passes_through() {
        // No override configured = no rewrite. The "local client,
        // no Docker, Ryokan reads client's save_path directly" case.
        assert_eq!(
            translate_client_path("/downloads/x.mkv", "/downloads", ""),
            "/downloads/x.mkv"
        );
    }

    #[test]
    fn translate_client_path_non_matching_prefix_unchanged() {
        // If the path isn't under the client's save_path, don't
        // silently rewrite — could indicate user mis-config.
        assert_eq!(
            translate_client_path("/other/path.mkv", "/downloads", "/mnt/seedbox"),
            "/other/path.mkv"
        );
    }

    #[test]
    fn translate_client_path_empty_remote_passes_through() {
        // Client hasn't reported a save_path yet (e.g. metadata not
        // arrived). Return the input verbatim rather than turning
        // `path` into `local/path` which would usually be wrong.
        assert_eq!(
            translate_client_path("/downloads/x.mkv", "", "/mnt/seedbox"),
            "/downloads/x.mkv"
        );
    }

    #[test]
    fn translate_client_path_exact_match_collapses_to_local() {
        // `path == client_save_path` is the common case when
        // post-processing asks for the translated save_path itself
        // (not a content_path under it). `strip_prefix` returns
        // `Some("")`, so the concatenation becomes `local + ""` —
        // we just return `local`.
        assert_eq!(
            translate_client_path("/downloads", "/downloads", "/mnt/seedbox"),
            "/mnt/seedbox"
        );
        // Trailing slash on input is also handled — trimmed to the
        // same canonical form.
        assert_eq!(
            translate_client_path("/downloads/", "/downloads", "/mnt/seedbox"),
            "/mnt/seedbox/"
        );
    }

    #[test]
    fn state_is_complete_catches_all_seed_variants() {
        use DownloadItemState::*;
        assert!(Seeding.is_complete());
        assert!(SeedingStalled.is_complete());
        assert!(SeedingQueued.is_complete());
        assert!(CheckingSeed.is_complete());
        assert!(PausedComplete.is_complete());
    }

    #[test]
    fn state_is_complete_rejects_download_variants() {
        use DownloadItemState::*;
        assert!(!Downloading.is_complete());
        assert!(!DownloadingStalled.is_complete());
        assert!(!DownloadingQueued.is_complete());
        assert!(!CheckingDownload.is_complete());
        assert!(!Paused.is_complete());
        assert!(!Errored.is_complete());
    }

    #[test]
    fn state_is_errored_only_errored_variant() {
        use DownloadItemState::*;
        assert!(Errored.is_errored());
        for v in [
            Downloading,
            DownloadingStalled,
            DownloadingQueued,
            CheckingDownload,
            Seeding,
            SeedingStalled,
            SeedingQueued,
            CheckingSeed,
            Paused,
            PausedComplete,
        ] {
            assert!(!v.is_errored(), "{v:?} should not be errored");
        }
    }

    // The exhaustive match inside this helper is the one and only
    // place new `DownloadItemState` variants have to be registered
    // for cross-layer tests — the array returned is what every
    // contract test below iterates over. Adding a new variant without
    // also adding a match arm here fails to compile.
    fn all_variants_with_slugs() -> Vec<(DownloadItemState, &'static str)> {
        fn _slug(v: DownloadItemState) -> &'static str {
            match v {
                DownloadItemState::Downloading => "downloading",
                DownloadItemState::DownloadingStalled => "downloading-stalled",
                DownloadItemState::DownloadingQueued => "downloading-queued",
                DownloadItemState::CheckingDownload => "checking-download",
                DownloadItemState::Seeding => "seeding",
                DownloadItemState::SeedingStalled => "seeding-stalled",
                DownloadItemState::SeedingQueued => "seeding-queued",
                DownloadItemState::CheckingSeed => "checking-seed",
                DownloadItemState::Paused => "paused",
                DownloadItemState::PausedComplete => "paused-complete",
                DownloadItemState::Errored => "errored",
            }
        }
        use DownloadItemState::*;
        let list = [
            Downloading,
            DownloadingStalled,
            DownloadingQueued,
            CheckingDownload,
            Seeding,
            SeedingStalled,
            SeedingQueued,
            CheckingSeed,
            Paused,
            PausedComplete,
            Errored,
        ];
        list.into_iter().map(|v| (v, _slug(v))).collect()
    }

    #[test]
    fn state_kebab_slugs_are_stable_and_distinct() {
        let variants = all_variants_with_slugs();
        let mut seen = std::collections::HashSet::new();
        for (v, expected_slug) in &variants {
            let actual = serde_json::to_value(v).unwrap();
            let actual_slug = actual
                .as_str()
                .unwrap_or_else(|| panic!("{v:?} didn't serialize to a string"));
            assert_eq!(
                actual_slug, *expected_slug,
                "{v:?} slug drifted — JS state-label map keys off the kebab vocabulary"
            );
            assert!(
                seen.insert(actual_slug.to_string()),
                "duplicate slug {actual_slug:?}"
            );
        }
    }

    #[test]
    fn state_kebab_slugs_roundtrip_through_serde() {
        // DownloadItem carries state_kind; the queue endpoint
        // serializes it over the wire and the JS sort / label / badge
        // maps key off the slug. Verify the wire format roundtrips
        // exactly — a silent rename (say, via removing `#[serde(...)]`)
        // would break the JS without any compile-time signal.
        for (v, slug) in all_variants_with_slugs() {
            let json = serde_json::json!(slug);
            let parsed: DownloadItemState = serde_json::from_value(json).unwrap_or_else(|e| {
                panic!("slug {slug:?} failed to deserialize back into {v:?}: {e}")
            });
            assert_eq!(parsed, v);
        }
    }

    #[test]
    fn download_item_wire_shape_carries_state_kind() {
        // Regression for the download-state badge refactor: state_kind
        // used to be `#[serde(skip)]` and the JS state-label map keyed off
        // the client-native `state` string. After switching to the
        // kebab enum on both sides, state_kind MUST appear on the
        // wire or the Downloads queue page renders every row with
        // an empty badge class and "Downloading" label.
        let item = DownloadItem {
            hash: "a".repeat(40),
            name: "Some.Release.mkv".to_string(),
            size: 1024,
            progress: 0.5,
            dlspeed: 0,
            state: "stalledUP".to_string(),
            category: "anime".to_string(),
            eta: 0,
            save_path: "/downloads".to_string(),
            content_path: "/downloads/Some.Release.mkv".to_string(),
            state_kind: DownloadItemState::SeedingStalled,
            seeding_done: false,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["state_kind"], "seeding-stalled");
        assert_eq!(v["state"], "stalledUP");
    }
}
