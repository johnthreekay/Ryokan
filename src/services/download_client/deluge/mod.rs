//! Deluge implementation of [`DownloadClient`]. Speaks the Deluge Web
//! UI JSON-RPC API at `POST <base_url>/json`.
//!
//! Deluge-specific quirks worth flagging for future readers (most of
//! these are spelled out in the #63 plan at ~/Documents/ryokan-plan-
//! pluggable-download-clients.md → Phase 2):
//!   - **Two-step connect**: `auth.login(password)` establishes a
//!     session cookie, but a freshly-authenticated session isn't
//!     connected to any daemon. Every `core.*` call fails with
//!     `"Unknown method"` (NOT "not connected" — the methods aren't
//!     even registered on the web process) until `web.connect(host_id)`
//!     runs. Single most common first-time integration failure.
//!   - **Label plugin required for scoping**: Ryokan sets a per-grab
//!     label (default `"ryokan"`) and filters `list_scoped` by it.
//!     The Label plugin is bundled with Deluge but disabled by
//!     default; the connection test enables it via `core.enable_plugin`
//!     when it sees `Label` in `available_plugins` but not
//!     `enabled_plugins`. There's an upstream Deluge bug where an
//!     enabled-but-not-restarted Label plugin leaves RPC methods
//!     unregistered on the web process for one session; we re-call
//!     `web.connect` after enabling to force a method re-registration.
//!   - **File priority scale is 0 / 1 / 4 / 7** (Skip / Low / Normal /
//!     High), NOT qBit's 0 / 1 / 6 / 7. Writing `1` for "wanted"
//!     would set the file to Low priority (bandwidth-de-prioritized
//!     relative to peers), which is wrong. Ryokan writes `0` for
//!     skip and `4` for wanted.
//!   - **Duplicate-add detection is message-matching**: the error
//!     code fluctuates across versions (ticket deluge-dev/#3507) so
//!     we match on the substring `"Torrent already in session"` (and
//!     `"Torrent already being added"` for the racing-add case).
//!   - **No `has_metadata` field** in `core.get_torrent_status`
//!     output — live-probed against Deluge 2.x + Label plugin 0.3.
//!     Proxy for metadata-ready: `files` array non-empty.
//!   - **Missing fields silently omitted**: `get_torrent_status`
//!     drops unknown keys from the response rather than returning an
//!     error. Every deserializer here uses `#[serde(default)]`.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use super::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};

/// Deluge file priority: 0 = Skip, 1 = Low, 4 = Normal, 7 = High.
/// Ryokan only ever writes 0 and 4 — the Low/High levels are a
/// Deluge-UI feature Ryokan doesn't control from scoring.
const DELUGE_PRIO_SKIP: i32 = 0;
const DELUGE_PRIO_NORMAL: i32 = 4;

/// Metadata-wait budget for `add_torrent_with_file_filter`. Matches
/// qBit's 10-second ceiling so selective-narrowing latency is
/// consistent across clients — longer waits block interactive grabs.
const METADATA_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct DelugeClient {
    base_url: String,
    password: String,
    label: String,
    http: Client,
    /// Flipped to `true` after a successful `auth.login` +
    /// `web.connect`. Any RPC error that smells like "session
    /// expired" or "not connected to daemon" clears it so the next
    /// call re-runs the full handshake. Atomic so the
    /// `ensure_connected` path doesn't serialize mutations behind a
    /// tokio mutex.
    connected: Arc<AtomicBool>,
    /// Serializes concurrent `ensure_connected` calls so only one
    /// task runs the auth + connect dance when the flag is false.
    /// Without this, a burst of first-time callers all race to log
    /// in and set up the Label plugin.
    connect_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    message: String,
}

/// Raw torrent status fields Ryokan needs. Every field is
/// `#[serde(default)]` because Deluge silently omits keys that
/// aren't populated yet (e.g. pre-metadata torrents have empty
/// `files` but no `total_size`, and plugin-provided fields like
/// `label` only appear when the Label plugin is loaded).
#[derive(Debug, Deserialize, Default)]
struct DelugeRawTorrent {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    save_path: String,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    download_payload_rate: i64,
    #[serde(default)]
    eta: i64,
    #[serde(default)]
    total_size: i64,
    #[serde(default)]
    is_finished: bool,
    /// Per-torrent ratio stop (issue #228). Deluge copies the global
    /// `stop_seed_at_ratio` / `stop_seed_ratio` into each torrent's
    /// options at add time, so these are the effective values.
    #[serde(default)]
    stop_at_ratio: bool,
    #[serde(default)]
    stop_ratio: f64,
    #[serde(default)]
    ratio: f64,
    #[serde(default)]
    label: String,
    #[serde(default)]
    files: Vec<DelugeRawFile>,
    #[serde(default)]
    file_priorities: Vec<i32>,
    /// Per-file progress as a parallel array aligned to `files`,
    /// each entry 0.0–1.0 (fraction, NOT percentage). qBit's
    /// `TorrentFile.progress` uses the same fraction scale; Deluge's
    /// per-torrent `progress` is 0–100 but `file_progress` is 0–1.
    /// Post-processing's "is this file complete?" check filters on
    /// `f.progress >= 1.0`, so this array needs to be populated
    /// correctly or every Deluge grab stays pending forever.
    #[serde(default)]
    file_progress: Vec<f64>,
}

#[derive(Debug, Deserialize, Default)]
struct DelugeRawFile {
    #[serde(default)]
    path: String,
    #[serde(default)]
    size: i64,
}

#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    method: &'a str,
    params: Value,
    id: u32,
}

impl DelugeClient {
    pub fn new(base_url: &str, password: &str, label: &str) -> Self {
        let http = Client::builder()
            .cookie_store(true)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: normalize_base_url(base_url),
            password: password.to_string(),
            label: if label.is_empty() {
                "ryokan".to_string()
            } else {
                label.to_string()
            },
            http,
            connected: Arc::new(AtomicBool::new(false)),
            connect_lock: Arc::new(Mutex::new(())),
        }
    }

    /// JSON-RPC round-trip. Returns the raw `result` value. The caller
    /// is responsible for deserializing into a concrete type — keeps
    /// the one-HTTP-call helper generic while letting each callsite
    /// own its schema.
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let req = RpcRequest {
            method,
            params,
            id: 1,
        };
        let resp = self
            .http
            .post(format!("{}/json", self.base_url))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("Deluge request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Deluge HTTP {status}: {}", body.trim()));
        }

        let parsed: RpcResponse<Value> = resp
            .json()
            .await
            .map_err(|e| format!("Deluge response parse failed: {e}"))?;

        if let Some(err) = parsed.error {
            return Err(err.message);
        }
        Ok(parsed.result.unwrap_or(Value::Null))
    }

    /// First-time or reconnect-after-expiry handshake:
    /// `auth.login` → `web.get_hosts` → `web.connect`. Ensures the
    /// Label plugin is enabled so `list_scoped` filtering works.
    /// Serialized under `connect_lock` so concurrent callers collapse
    /// to a single handshake rather than racing.
    async fn connect(&self) -> Result<(), String> {
        let _guard = self.connect_lock.lock().await;
        // Double-check in case another task beat us through the lock.
        if self.connected.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Step 1: auth.
        let ok: bool =
            serde_json::from_value(self.rpc("auth.login", json!([self.password])).await?)
                .map_err(|e| format!("Deluge auth.login unexpected response: {e}"))?;
        if !ok {
            return Err("Deluge auth failed: invalid password".into());
        }

        // Step 2: resolve host_id and connect to the daemon. No need
        // to cache the host_id across calls — every `connect()`
        // invocation re-fetches it, which is fine because
        // `connect()` only runs on initial handshake + session
        // expiry re-probes (not per-call).
        let hosts_raw = self.rpc("web.get_hosts", json!([])).await?;
        let hosts: Vec<Value> = serde_json::from_value(hosts_raw)
            .map_err(|e| format!("Deluge web.get_hosts parse failed: {e}"))?;
        let host_id = hosts
            .first()
            .and_then(|h| h.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .ok_or("Deluge web.get_hosts returned no hosts")?
            .to_string();

        // web.connect returns the list of daemon methods now available
        // via RPC. We don't inspect the list — the only signal we need
        // is the absence of an error.
        self.rpc("web.connect", json!([host_id])).await?;

        // Step 3: ensure Label plugin is enabled. list_scoped's
        // server-side `{"label": "ryokan"}` filter requires it, and so
        // does per-torrent label assignment in add_torrent.
        self.ensure_label_plugin(&host_id).await?;

        // Step 4: ensure our label exists. Idempotent — duplicate adds
        // error with "Label already exists" which we swallow.
        let _ = self.ensure_label_exists().await;

        self.connected.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Detects the Label plugin state via `web.get_plugins` and
    /// enables it via `core.enable_plugin` if missing. Re-runs
    /// `web.connect(host_id)` after enabling because Deluge doesn't
    /// register newly-enabled plugin RPC methods on an existing web
    /// session until a re-connect (upstream bug, still present in
    /// Deluge 2.x).
    async fn ensure_label_plugin(&self, host_id: &str) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Plugins {
            enabled_plugins: Vec<String>,
            available_plugins: Vec<String>,
        }
        let plugins: Plugins =
            serde_json::from_value(self.rpc("web.get_plugins", json!([])).await?)
                .map_err(|e| format!("Deluge web.get_plugins parse failed: {e}"))?;

        if plugins.enabled_plugins.iter().any(|p| p == "Label") {
            return Ok(());
        }
        if !plugins.available_plugins.iter().any(|p| p == "Label") {
            return Err(
                "Deluge Label plugin is not installed. Install the Label plugin in \
                 Deluge (bundled; just needs enabling) and retry the connection test."
                    .into(),
            );
        }

        // Enable + force method re-registration via a reconnect.
        let _ = self.rpc("core.enable_plugin", json!(["Label"])).await?;
        self.rpc("web.connect", json!([host_id])).await?;
        Ok(())
    }

    async fn ensure_label_exists(&self) -> Result<(), String> {
        // Duplicate-add surfaces as {"error": {"message": "...Label
        // already exists..."}}. Treat that as success; any other
        // error bubbles up as-is so connection faults don't get
        // swallowed here.
        match self.rpc("label.add", json!([self.label])).await {
            Ok(_) => Ok(()),
            Err(msg) if msg.contains("Label already exists") => Ok(()),
            Err(msg) => Err(msg),
        }
    }

    /// Run a connected RPC call. On first invocation, runs the full
    /// handshake. On subsequent invocations, clears the `connected`
    /// flag and retries once if the error looks like session expiry
    /// or daemon disconnect — both of which surface as "Unknown
    /// method" from Deluge's web proxy rather than a 401/403 status.
    async fn connected_rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        if !self.connected.load(Ordering::SeqCst) {
            self.connect().await?;
        }
        match self.rpc(method, params.clone()).await {
            Ok(v) => Ok(v),
            Err(msg) if is_disconnect_error(&msg) => {
                self.connected.store(false, Ordering::SeqCst);
                self.connect().await?;
                self.rpc(method, params).await
            }
            Err(msg) => Err(msg),
        }
    }

    /// Shared implementation for `add_torrent` / `add_torrent_paused`.
    /// `add_paused` flips Deluge's native `add_paused` option; both
    /// outer entry points share the duplicate-detection + labeling
    /// logic so they behave identically apart from the initial
    /// running-state.
    async fn add_torrent_inner(
        &self,
        url: &str,
        info_hash: &str,
        add_paused: bool,
    ) -> Result<AddOutcome, String> {
        // `core.add_torrent_magnet` for magnet URIs; `core.add_torrent_url`
        // for http:// .torrent URLs. The distinction matters because
        // `add_torrent_magnet` errors on an http URL and vice versa
        // (both methods parse their input and reject the other shape).
        let (method, params) = if url.starts_with("magnet:") {
            (
                "core.add_torrent_magnet",
                json!([url, {"add_paused": add_paused}]),
            )
        } else {
            (
                "core.add_torrent_url",
                json!([url, {"add_paused": add_paused}, Value::Null]),
            )
        };

        // Fallback hash for labeling: the caller's pre-computed
        // info_hash, lowercased. Used in arms where Deluge doesn't
        // hand back a hash itself (`add_torrent_url` null-return) or
        // where the torrent already exists (AlreadyPresent).
        // `None` if the caller didn't supply one — `info_hash` empty
        // means `extract_hash` couldn't parse the URL and a
        // `label.set_torrent("", ...)` call would just produce a
        // spurious error.
        let caller_hash = if info_hash.is_empty() {
            None
        } else {
            Some(info_hash.to_ascii_lowercase())
        };

        let (outcome, hash_for_label) = match self.connected_rpc(method, params).await {
            Ok(Value::String(hash)) if !hash.is_empty() => (AddOutcome::Added, Some(hash)),
            // `add_torrent_url` returns null on success rather than the
            // hash. Fall back to the caller's pre-computed `info_hash`
            // so labeling still runs through the same post-match block
            // as the string-return and AlreadyPresent arms — any hash
            // we know, we label.
            Ok(_) => (AddOutcome::Added, caller_hash.clone()),
            Err(msg) if is_duplicate_add_error(&msg) => {
                // Deluge's "already in session" error carries the
                // infohash in the message ("Torrent already in session
                // (<hash>)"). We don't bother parsing it out — the
                // caller pre-computed `info_hash` and passed it in.
                // Tag the existing torrent with our label too: if the
                // user (or Ryokan from a prior session) manually
                // added this torrent without the label, the re-grab
                // should "adopt" it rather than leave it invisible to
                // `list_scoped`.
                (AddOutcome::AlreadyPresent, caller_hash)
            }
            Err(msg) => return Err(msg),
        };

        // Tag the torrent with our scoping label. Failures here are
        // non-fatal to the grab — the torrent is in the client — but
        // matter for `list_scoped` visibility: an unlabeled torrent
        // won't show up in the label-filtered listing and Ryokan will
        // sit forever waiting for it to "appear." Log so the operator
        // has a trail when that happens.
        if let Some(hash) = hash_for_label
            && let Err(e) = self
                .connected_rpc("label.set_torrent", json!([hash, self.label]))
                .await
        {
            tracing::warn!(
                target: "ryokan::download_client::deluge",
                hash = %hash,
                label = %self.label,
                error = %e,
                "label.set_torrent failed — torrent will be invisible to list_scoped until the Label plugin is working and the label is set"
            );
        }

        Ok(outcome)
    }
}

fn is_disconnect_error(msg: &str) -> bool {
    // `core.*` methods disappear from the RPC surface when the web
    // process loses its daemon connection — the Deluge web proxy
    // returns "Unknown method" rather than a more specific status.
    // The "Not connected to a daemon" variant shows up less often
    // but is worth catching too.
    msg.contains("Unknown method") || msg.contains("Not connected to a daemon")
}

#[async_trait]
impl DownloadClient for DelugeClient {
    async fn test(&self) -> Result<String, String> {
        self.connect().await?;
        // `daemon.get_version` returns the Deluge daemon version
        // string ("2.2.0" etc.). Picked over `daemon.info` — the
        // latter is NOT exposed through the web-proxy's `/json`
        // endpoint (only the raw daemon RPC on port 58846), so
        // calling it here returns "Unknown method" post-connect
        // and the settings page shows "Connection failed" despite
        // the handshake working. Live-probed 2026-04-21.
        let version: String =
            serde_json::from_value(self.connected_rpc("daemon.get_version", json!([])).await?)
                .map_err(|e| format!("Deluge daemon.get_version parse failed: {e}"))?;
        Ok(version)
    }

    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        self.add_torrent_inner(url, info_hash, false).await
    }

    async fn add_torrent_paused(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        // Same leaky abstraction qBit 5.x has: Deluge's `add_paused=True`
        // stops the peer handshake, which in turn prevents the
        // libtorrent metadata-exchange extension from running — so a
        // magnet added paused never fetches its `info` dict and
        // `get_files` returns empty forever. The picker modal's
        // readiness poll then hangs until the TTL sweep fires.
        //
        // Workaround mirrors the qBit impl: add running, wait for
        // metadata up to the shared 10s budget, then mark every file
        // as skipped so no content flows. The trait contract's
        // post-condition ("no file data being downloaded") still
        // holds; the only leak is that the torrent is technically
        // active during metadata fetch, typically 1-3s for magnets
        // with cached tracker responses.
        if info_hash.is_empty() {
            // Without a hash we can't poll per-torrent status, so
            // fall back to the simple paused-add — the caller will
            // hit the same empty `get_files` problem but at least
            // we don't block here on a torrent we can't address.
            return self.add_torrent_inner(url, info_hash, true).await;
        }

        let outcome = self.add_torrent_inner(url, info_hash, false).await?;
        let hash_lc = info_hash.to_ascii_lowercase();

        // Poll until the file list is populated. Deluge reports
        // `files: []` until metadata arrives; we reuse the same
        // signal `add_torrent_with_file_filter` does.
        let start = Instant::now();
        let mut delay = Duration::from_millis(500);
        let file_count = loop {
            let status: DelugeRawTorrent = serde_json::from_value(
                self.connected_rpc("core.get_torrent_status", json!([hash_lc, ["files"]]))
                    .await?,
            )
            .map_err(|e| format!("Deluge metadata-poll parse failed: {e}"))?;
            if !status.files.is_empty() {
                break status.files.len();
            }
            if start.elapsed() >= METADATA_WAIT_TIMEOUT {
                // Same fallback as qBit's path: the torrent is left
                // running with all files at default priority. The
                // picker sees an empty file list, surfaces a timeout
                // error to the user, and the sweep auto-commits
                // everything on TTL. Matches "no path out of the
                // modal causes the intended grab to be deleted."
                return Ok(outcome);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        };

        // Mark every file as skipped. `set_torrent_options` takes a
        // full priority array sized to the file count.
        let skip_all: Vec<i32> = vec![DELUGE_PRIO_SKIP; file_count];
        let _: Value = self
            .connected_rpc(
                "core.set_torrent_options",
                json!([[hash_lc.clone()], {"file_priorities": skip_all}]),
            )
            .await?;

        // Pause after priorities land. At this point metadata is
        // fetched and every file is marked skip, so a pause here
        // can't regress into the "metadata never arrives" hole —
        // the torrent can sit paused until the user confirms.
        let _: Value = self
            .connected_rpc("core.pause_torrent", json!([[hash_lc]]))
            .await?;

        Ok(outcome)
    }

    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        info_hash: &str,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        if info_hash.is_empty() {
            return Err("Deluge selective download requires a known info hash".into());
        }
        let hash_lc = info_hash.to_ascii_lowercase();

        self.add_torrent(url, &hash_lc).await?;

        // Poll for metadata. Deluge has no `has_metadata` field in
        // this version — `files` array non-empty is the signal. Same
        // 10s budget as qBit so selective-narrowing latency is
        // consistent across impls.
        let start = Instant::now();
        let mut delay = Duration::from_millis(500);
        let files: Vec<DelugeRawFile> = loop {
            let status: DelugeRawTorrent = serde_json::from_value(
                self.connected_rpc(
                    "core.get_torrent_status",
                    json!([hash_lc, ["files", "file_priorities", "file_progress"]]),
                )
                .await?,
            )
            .map_err(|e| format!("Deluge metadata-poll parse failed: {e}"))?;
            if !status.files.is_empty() {
                break status.files;
            }
            if start.elapsed() >= METADATA_WAIT_TIMEOUT {
                // Same fallback semantics as the qBit impl: drop the
                // narrow, let the torrent download everything. The
                // grab row stays valid; user just gets a full download
                // instead of a filtered one.
                return Ok(SelectiveOutcome::FullDownload);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        };

        let names: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let keep_indices = match pick(&names) {
            Some(ids) if !ids.is_empty() && ids.len() < files.len() => ids,
            _ => return Ok(SelectiveOutcome::FullDownload),
        };

        // Detect prior narrowing: if any existing priority is 0 (skip),
        // an earlier grab has already touched this torrent. Merge the
        // new keep set in additively — only bump new files from 0 →
        // normal; leave files already at normal/high untouched.
        let current: Vec<i32> = serde_json::from_value(
            self.connected_rpc(
                "core.get_torrent_status",
                json!([hash_lc, ["file_priorities"]]),
            )
            .await?,
        )
        .ok()
        .and_then(|v: Value| {
            v.get("file_priorities")
                .cloned()
                .and_then(|p| serde_json::from_value(p).ok())
        })
        .unwrap_or_else(|| vec![DELUGE_PRIO_NORMAL; files.len()]);

        let already_narrowed = current.contains(&DELUGE_PRIO_SKIP);

        let mut new_priorities: Vec<i32> = current.clone();
        if new_priorities.len() != files.len() {
            new_priorities = vec![DELUGE_PRIO_NORMAL; files.len()];
        }

        if already_narrowed {
            for &i in &keep_indices {
                if let Some(slot) = new_priorities.get_mut(i)
                    && *slot == DELUGE_PRIO_SKIP
                {
                    *slot = DELUGE_PRIO_NORMAL;
                }
            }
        } else {
            for (i, slot) in new_priorities.iter_mut().enumerate() {
                *slot = if keep_indices.contains(&i) {
                    DELUGE_PRIO_NORMAL
                } else {
                    DELUGE_PRIO_SKIP
                };
            }
        }

        self.connected_rpc(
            "core.set_torrent_options",
            json!([[hash_lc], {"file_priorities": new_priorities}]),
        )
        .await?;

        Ok(SelectiveOutcome::Filtered(keep_indices))
    }

    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        // Server-side filter by label; no client-side scanning.
        // Empty keys list returns all fields — cheap enough at
        // Ryokan's scope and avoids future breakage if a new field
        // becomes load-bearing for state mapping.
        let filter = json!({"label": self.label});
        let raw: Value = self
            .connected_rpc("core.get_torrents_status", json!([filter, []]))
            .await?;
        let map: std::collections::HashMap<String, DelugeRawTorrent> = serde_json::from_value(raw)
            .map_err(|e| format!("Deluge list_scoped parse failed: {e}"))?;

        // The dict is keyed by infohash; inject the key into each
        // `DelugeRawTorrent.hash` before conversion. We don't trust
        // the inner `hash` field alone: `get_torrent_status` silently
        // omits unknown keys, and future Deluge builds or forks
        // aren't guaranteed to include `hash` in the status dict.
        // Worst case without this: every `DownloadItem.hash` ends up
        // empty and post-processing's grab→torrent match via
        // `by_hash` silently fails.
        Ok(map
            .into_iter()
            .map(|(key, mut raw)| {
                if raw.hash.is_empty() {
                    raw.hash = key;
                }
                to_download_item(raw)
            })
            .collect())
    }

    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String> {
        let status: DelugeRawTorrent = serde_json::from_value(
            self.connected_rpc(
                "core.get_torrent_status",
                json!([info_hash, ["files", "file_priorities", "file_progress"]]),
            )
            .await?,
        )
        .map_err(|e| format!("Deluge get_files parse failed: {e}"))?;
        Ok(to_download_files(&status))
    }

    async fn pause(&self, info_hash: &str) -> Result<(), String> {
        self.connected_rpc("core.pause_torrent", json!([[info_hash]]))
            .await?;
        Ok(())
    }

    async fn resume(&self, info_hash: &str) -> Result<(), String> {
        self.connected_rpc("core.resume_torrent", json!([[info_hash]]))
            .await?;
        Ok(())
    }

    async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), String> {
        // `core.remove_torrent(hash, remove_data)` — single hash,
        // not a list. Batch removal uses `core.remove_torrents` which
        // we don't need.
        self.connected_rpc("core.remove_torrent", json!([info_hash, delete_files]))
            .await?;
        Ok(())
    }

    async fn set_file_wanted(
        &self,
        info_hash: &str,
        files: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        // Deluge only accepts a full-length priority array — there's
        // no partial update. Read current priorities, patch the
        // requested indices, write back.
        let status: DelugeRawTorrent = serde_json::from_value(
            self.connected_rpc(
                "core.get_torrent_status",
                json!([info_hash, ["file_priorities", "files"]]),
            )
            .await?,
        )
        .map_err(|e| format!("Deluge set_file_wanted read failed: {e}"))?;

        let len = status.files.len().max(status.file_priorities.len());
        let mut new_prio: Vec<i32> = if status.file_priorities.len() == len {
            status.file_priorities
        } else {
            vec![DELUGE_PRIO_NORMAL; len]
        };

        let target = if wanted {
            DELUGE_PRIO_NORMAL
        } else {
            DELUGE_PRIO_SKIP
        };
        for &i in files {
            if let Some(slot) = new_prio.get_mut(i) {
                *slot = target;
            }
        }

        self.connected_rpc(
            "core.set_torrent_options",
            json!([[info_hash], {"file_priorities": new_prio}]),
        )
        .await?;
        Ok(())
    }

    fn sonarr_impl_name(&self) -> &'static str {
        "Deluge"
    }

    /// Issue #28 — apply per-torrent ratio rules via Deluge's
    /// `core.set_torrent_options` API.
    ///
    /// Wire shape (live-probed against Deluge 2.x):
    /// `core.set_torrent_options([torrent_id], {options})` where
    /// the options dict carries `stop_at_ratio: bool` + `stop_ratio:
    /// float`. Setting `stop_at_ratio: true` flips the per-torrent
    /// override so this torrent stops at its own ratio instead of
    /// inheriting the global setting.
    ///
    /// Deluge core has **no idle-time stop concept** — torrents
    /// stop on ratio only. `time_minutes`, when set, surfaces as a
    /// debug log so operators see the no-op rather than thinking
    /// it was applied. The autoremoveplus plugin extends Deluge
    /// with idle-time stops but isn't bundled with vanilla Deluge,
    /// so Ryokan can't assume it.
    async fn set_seed_rules(&self, info_hash: &str, rules: super::SeedRules) -> Result<(), String> {
        let hash_lower = info_hash.to_ascii_lowercase();
        let mut options = serde_json::Map::new();
        if let Some(ratio) = rules.ratio {
            // serde_json::Number::from_f64 returns None for NaN /
            // ±Inf. A Null in the options dict would silently
            // unset the per-torrent stop_ratio on the wire — bail
            // loudly so the operator sees the bad config instead
            // of a torrent that seeds forever.
            let ratio_num = serde_json::Number::from_f64(ratio).ok_or_else(|| {
                format!("deluge set_seed_rules: ratio {ratio} is not a finite number")
            })?;
            options.insert("stop_at_ratio".to_string(), serde_json::Value::Bool(true));
            options.insert(
                "stop_ratio".to_string(),
                serde_json::Value::Number(ratio_num),
            );
        }
        if rules.time_minutes.is_some() {
            tracing::debug!(
                "deluge: time_minutes seed-rule ignored — Deluge core has no idle-time stop"
            );
        }
        if options.is_empty() {
            return Ok(());
        }
        self.rpc("core.set_torrent_options", json!([[hash_lower], options]))
            .await?;
        Ok(())
    }
}

fn is_duplicate_add_error(msg: &str) -> bool {
    // Deluge's error codes for duplicate-add are not stable across
    // versions (see deluge-dev ticket #3507). Match on the two known
    // message prefixes instead.
    msg.contains("Torrent already in session") || msg.contains("Torrent already being added")
}

fn to_download_item(raw: DelugeRawTorrent) -> DownloadItem {
    let state_kind = map_deluge_state(&raw);
    let seeding_done = deluge_seeding_done(&raw);
    // content_path for Deluge is not a native field — compute from
    // save_path + files' common prefix via the shared helper.
    let files_view: Vec<DownloadFile> = to_download_files(&raw);
    let content_path = super::compute_content_path(&raw.save_path, &files_view);
    DownloadItem {
        hash: raw.hash,
        name: raw.name,
        size: raw.total_size,
        // Deluge reports progress as a percentage (0.0–100.0);
        // qBit reports it as a fraction (0.0–1.0). Normalize to the
        // fraction scale so `DownloadItem.progress` means the same
        // thing across impls.
        progress: raw.progress / 100.0,
        dlspeed: raw.download_payload_rate,
        // Preserve the raw native string in `state` so the Downloads
        // UI's label-map keeps working for qBit; Deluge's strings
        // don't match qBit's, so the UI will fall through to the
        // raw value in its switch default. Phase 2+ UI work can
        // generalize once a second client is actually live.
        state: raw.state,
        category: raw.label,
        eta: raw.eta,
        save_path: raw.save_path,
        content_path,
        state_kind,
        seeding_done,
    }
}

/// Deluge pauses a torrent when `stop_at_ratio` is on and the ratio
/// reaches `stop_ratio` (`remove_at_ratio` would make it vanish
/// instead). A finished, paused torrent at or past that ratio is done
/// seeding; a paused one below it was paused by hand.
fn deluge_seeding_done(raw: &DelugeRawTorrent) -> bool {
    raw.state == "Paused" && raw.is_finished && raw.stop_at_ratio && raw.ratio >= raw.stop_ratio
}

fn to_download_files(raw: &DelugeRawTorrent) -> Vec<DownloadFile> {
    raw.files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let wanted = raw
                .file_priorities
                .get(i)
                .copied()
                .map(|p| p != DELUGE_PRIO_SKIP)
                .unwrap_or(true);
            // Deluge's `file_progress` is a parallel array of
            // 0.0–1.0 fractions, same scale as qBit's per-file
            // `progress`. Defaulting to 0.0 on index mismatch is
            // safe: post-processing's completion filter requires
            // `>= 1.0`, so a missing entry just keeps the file
            // flagged as "not ready" for one more tick.
            let progress = raw.file_progress.get(i).copied().unwrap_or(0.0);
            DownloadFile {
                name: f.path.clone(),
                size: f.size,
                progress,
                wanted,
            }
        })
        .collect()
}

/// Map Deluge's native state machine to Ryokan's 10-variant enum.
/// Deluge exposes fewer distinct states than qBit; this map collapses
/// the UI-visible distinctions (stalled / queued) that Deluge doesn't
/// natively carry into their non-distinct counterparts — losing some
/// granularity on the Downloads page for Deluge-backed torrents, but
/// not losing any post-processing correctness (completion detection
/// only cares about `is_complete`, which is derived from
/// `is_finished` + state, not from the stalled/queued subdistinction).
fn map_deluge_state(raw: &DelugeRawTorrent) -> DownloadItemState {
    use DownloadItemState::*;
    match raw.state.as_str() {
        "Error" => Errored,
        "Checking" | "Allocating" if raw.is_finished => CheckingSeed,
        "Checking" | "Allocating" => CheckingDownload,
        "Moving" => {
            // `Moving` means files are mid-storage-move — post-
            // processing MUST NOT import until the move finishes,
            // regardless of `is_finished`. Collapse to `Downloading`
            // (non-complete) so `is_complete()` returns false; the
            // raw `Moving` string is preserved in `DownloadItem.state`
            // for UI display. No `Moving` variant in the normalized
            // enum because qBit doesn't have a distinct moving state
            // either (its `checkingUP` covers the same window).
            Downloading
        }
        "Paused" if raw.is_finished => PausedComplete,
        "Paused" => Paused,
        "Queued" if raw.is_finished => SeedingQueued,
        "Queued" => DownloadingQueued,
        "Seeding" => Seeding,
        "Downloading" => Downloading,
        _ => Downloading, // unknown states default to a safe non-complete
    }
}

fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
    // Deluge's Web UI defaults to port 8112 with plain HTTP. We don't
    // second-guess a user's scheme choice — if they type a bare host
    // we assume HTTP on local/private IPs and HTTPS on public hosts,
    // matching the qBit impl's heuristic.
    let lower = trimmed.to_ascii_lowercase();
    let is_local = lower.starts_with("localhost")
        || lower.starts_with("127.")
        || lower.starts_with("10.")
        || lower.starts_with("192.168.")
        || lower.starts_with("172.16.")
        || lower.starts_with("172.17.")
        || lower.starts_with("172.18.")
        || lower.starts_with("172.19.")
        || lower.starts_with("172.20.")
        || lower.starts_with("172.21.")
        || lower.starts_with("172.22.")
        || lower.starts_with("172.23.")
        || lower.starts_with("172.24.")
        || lower.starts_with("172.25.")
        || lower.starts_with("172.26.")
        || lower.starts_with("172.27.")
        || lower.starts_with("172.28.")
        || lower.starts_with("172.29.")
        || lower.starts_with("172.30.")
        || lower.starts_with("172.31.");
    if is_local {
        format!("http://{}", trimmed)
    } else {
        format!("https://{}", trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load-bearing object-safety check from Phase 1. The stub
    /// version of this test lived here to catch trait regressions
    /// before the real impl landed; now it doubles as a smoke test
    /// that the real Deluge impl is still `dyn`-compatible.
    #[test]
    fn deluge_client_is_object_safe() {
        fn _assert_dyn_compatible(_c: Arc<dyn DownloadClient>) {}
        let client = Arc::new(DelugeClient::new("http://localhost:8112", "", "ryokan"))
            as Arc<dyn DownloadClient>;
        _assert_dyn_compatible(client);
    }

    #[test]
    fn sonarr_impl_name_is_deluge() {
        let c = DelugeClient::new("http://localhost:8112", "", "ryokan");
        assert_eq!(c.sonarr_impl_name(), "Deluge");
    }

    #[test]
    fn duplicate_add_error_matcher() {
        assert!(is_duplicate_add_error(
            "Torrent already in session (abc123)."
        ));
        assert!(is_duplicate_add_error(
            "Failure: Torrent already being added..."
        ));
        assert!(!is_duplicate_add_error("Tracker returned error 429"));
        assert!(!is_duplicate_add_error(""));
    }

    #[test]
    fn disconnect_error_matcher() {
        assert!(is_disconnect_error("Unknown method"));
        assert!(is_disconnect_error("Not connected to a daemon"));
        assert!(!is_disconnect_error("Tracker unreachable"));
    }

    #[test]
    fn list_scoped_hash_injection_fills_empty_inner_hash() {
        // Guards the #2 defensive branch in `list_scoped`: when
        // Deluge's status dict omits the `hash` field (future fork /
        // reduced key set), the outer dict key still carries the
        // infohash and `list_scoped` injects it. Without this fix,
        // every `DownloadItem.hash` would be `""` and post-processing's
        // grab→torrent match via `by_hash` would silently fail.
        let key = "abcdef0123456789abcdef0123456789abcdef01".to_string();
        let mut raw = DelugeRawTorrent {
            hash: String::new(),
            name: "silent-hash-drop test".into(),
            state: "Downloading".into(),
            ..Default::default()
        };
        // Mirror the `list_scoped` branch: inject the key iff the
        // inner field was empty, then convert.
        if raw.hash.is_empty() {
            raw.hash = key.clone();
        }
        let item = to_download_item(raw);
        assert_eq!(item.hash, key);
    }

    #[test]
    fn list_scoped_hash_injection_preserves_non_empty_inner_hash() {
        // The other half of the invariant: if the inner `hash` IS
        // populated (common case), don't overwrite it. Belt check
        // against a future "always trust the key" regression that
        // would silently mismatch if Deluge ever keyed by something
        // other than the infohash.
        let inner_hash = "0123456789abcdef0123456789abcdef01234567";
        let key = "wrongkey".to_string();
        let mut raw = DelugeRawTorrent {
            hash: inner_hash.to_string(),
            ..Default::default()
        };
        if raw.hash.is_empty() {
            raw.hash = key;
        }
        let item = to_download_item(raw);
        assert_eq!(item.hash, inner_hash);
    }

    #[test]
    fn state_mapping_completion_semantics() {
        let seeding = DelugeRawTorrent {
            state: "Seeding".into(),
            ..Default::default()
        };
        assert!(map_deluge_state(&seeding).is_complete());

        let paused_complete = DelugeRawTorrent {
            state: "Paused".into(),
            is_finished: true,
            ..Default::default()
        };
        assert!(map_deluge_state(&paused_complete).is_complete());

        let paused_incomplete = DelugeRawTorrent {
            state: "Paused".into(),
            is_finished: false,
            ..Default::default()
        };
        assert!(!map_deluge_state(&paused_incomplete).is_complete());

        // CRITICAL: `Moving` must NOT be treated as complete even
        // when is_finished is true — libtorrent is mid-move, reading
        // content_path is a race.
        let moving = DelugeRawTorrent {
            state: "Moving".into(),
            is_finished: true,
            ..Default::default()
        };
        assert!(!map_deluge_state(&moving).is_complete());

        let errored = DelugeRawTorrent {
            state: "Error".into(),
            ..Default::default()
        };
        assert!(map_deluge_state(&errored).is_errored());

        let downloading = DelugeRawTorrent {
            state: "Downloading".into(),
            is_finished: false,
            ..Default::default()
        };
        assert!(!map_deluge_state(&downloading).is_complete());
    }

    #[test]
    fn empty_label_defaults_to_ryokan() {
        let c = DelugeClient::new("http://localhost:8112", "", "");
        assert_eq!(c.label, "ryokan");
    }

    #[test]
    fn custom_label_preserved() {
        let c = DelugeClient::new("http://localhost:8112", "", "anime-batch");
        assert_eq!(c.label, "anime-batch");
    }

    /// Live smoke test against a running Deluge Web UI at
    /// `http://localhost:8112` (lscr.io/linuxserver/deluge defaults —
    /// Web UI password `deluge`). Opt in by running:
    ///
    ///     RYOKAN_DELUGE_E2E=1 cargo test deluge::tests::live_smoke \
    ///       -- --ignored --nocapture
    ///
    /// Exercises the full surface Ryokan itself hits: test →
    /// add_torrent → list_scoped (with Label plugin round-trip) →
    /// duplicate-add → pause/resume → get_files → delete. Gated
    /// behind `#[ignore]` + env var so CI never depends on a daemon
    /// being up. Mirrors the pattern established by
    /// `transmission::tests::live_smoke` and
    /// `rtorrent::tests::live_smoke`.
    #[tokio::test]
    #[ignore = "requires live Deluge at localhost:8112"]
    async fn live_smoke() {
        if std::env::var("RYOKAN_DELUGE_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_DELUGE_E2E=1 to run against localhost:8112)");
            return;
        }

        let client = DelugeClient::new("http://localhost:8112", "deluge", "ryokan-e2e");

        let version = client.test().await.expect("test() failed");
        eprintln!("Deluge version: {version}");

        let magnet = "magnet:?xt=urn:btih:7a14d93f4c13e9c1ae255e0aa3b85a9aaf0cf52d&dn=sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce";
        let info_hash = "7a14d93f4c13e9c1ae255e0aa3b85a9aaf0cf52d";

        // Ensure a clean slate in case a prior run left state.
        let _ = client.delete(info_hash, false).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let outcome = client
            .add_torrent(magnet, info_hash)
            .await
            .expect("add_torrent() failed");
        eprintln!("add_torrent outcome: {outcome:?}");
        assert!(matches!(
            outcome,
            AddOutcome::Added | AddOutcome::AlreadyPresent
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let list = client.list_scoped().await.expect("list_scoped() failed");
        eprintln!("scoped torrents: {}", list.len());
        let found = list
            .iter()
            .find(|t| t.hash.eq_ignore_ascii_case(info_hash))
            .expect("added torrent must appear in list_scoped");
        assert_eq!(
            found.category, "ryokan-e2e",
            "Deluge label should round-trip as DownloadItem.category"
        );

        let dup = client
            .add_torrent(magnet, info_hash)
            .await
            .expect("duplicate add_torrent() failed");
        assert_eq!(dup, AddOutcome::AlreadyPresent);

        client.pause(info_hash).await.expect("pause() failed");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        client.resume(info_hash).await.expect("resume() failed");

        let _files = client
            .get_files(info_hash)
            .await
            .expect("get_files() failed");

        client
            .delete(info_hash, false)
            .await
            .expect("delete() failed");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let after = client
            .list_scoped()
            .await
            .expect("list_scoped() post-delete failed");
        assert!(
            !after.iter().any(|t| t.hash.eq_ignore_ascii_case(info_hash)),
            "torrent must not survive delete"
        );
        eprintln!("smoke passed");
    }

    /// Upload a local `.torrent` file to Deluge via JSON-RPC
    /// `core.add_torrent_file` with base64-encoded bytes. Returns
    /// the infohash Deluge assigned. Handles the Deluge quirks:
    /// `auth.login` then `web.connect(host_id)` before `core.*`
    /// calls work (documented in `deluge.rs`'s file header and the
    /// CLAUDE.md download-client quirks). Adds with `add_paused=True`
    /// so the torrent doesn't try to download bogus content, and
    /// applies the caller's label via the Label plugin post-add.
    ///
    /// Test-only; raw `reqwest::Client` with its own cookie jar to
    /// avoid reaching inside `DelugeClient` just to piggyback on its
    /// session state.
    async fn upload_torrent_file_deluge(
        base_url: &str,
        password: &str,
        label: &str,
        torrent_path: &std::path::Path,
    ) -> String {
        use serde_json::{Value, json};
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .expect("reqwest client");

        async fn rpc(
            client: &reqwest::Client,
            base_url: &str,
            method: &str,
            params: Value,
        ) -> Value {
            let resp = client
                .post(format!("{base_url}/json"))
                .json(&json!({
                    "method": method,
                    "params": params,
                    "id": 0,
                }))
                .send()
                .await
                .unwrap_or_else(|e| panic!("Deluge {method} transport: {e}"));
            assert_eq!(
                resp.status(),
                200,
                "Deluge {method} returned HTTP {}",
                resp.status()
            );
            resp.json::<Value>()
                .await
                .unwrap_or_else(|e| panic!("Deluge {method} json parse: {e}"))
        }

        // 1. auth.login → session cookie
        let login = rpc(
            &client,
            base_url,
            "auth.login",
            json!([password.to_string()]),
        )
        .await;
        assert_eq!(
            login.get("result").and_then(|v| v.as_bool()),
            Some(true),
            "Deluge auth.login returned: {login}"
        );

        // 2. web.get_hosts → pick the first (usually only) host
        let hosts = rpc(&client, base_url, "web.get_hosts", json!([])).await;
        let host_id = hosts
            .get("result")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|h| h.as_array())
            .and_then(|h| h.first())
            .and_then(|v| v.as_str())
            .expect("Deluge web.get_hosts returned no usable host id")
            .to_string();

        // 3. web.connect(host_id) — required before core.* calls work
        rpc(&client, base_url, "web.connect", json!([host_id])).await;

        // 4. core.add_torrent_file(filename, base64_filedump, options)
        use base64::{Engine, engine::general_purpose};
        let bytes = std::fs::read(torrent_path).expect("read .torrent");
        let b64 = general_purpose::STANDARD.encode(&bytes);
        let add_resp = rpc(
            &client,
            base_url,
            "core.add_torrent_file",
            json!(["testpack.torrent", b64, {"add_paused": true}]),
        )
        .await;
        let hash = add_resp
            .get("result")
            .and_then(|v| v.as_str())
            .expect("Deluge core.add_torrent_file missing hash result")
            .to_string();

        // 5. Apply the Label plugin label so list_scoped() round-trips.
        //    core.enable_plugin is idempotent if already on.
        rpc(&client, base_url, "core.enable_plugin", json!(["Label"])).await;
        rpc(&client, base_url, "label.add", json!([label.to_string()])).await; // may fail if label already exists; ignore
        rpc(
            &client,
            base_url,
            "label.set_torrent",
            json!([hash.clone(), label.to_string()]),
        )
        .await;

        hash
    }

    /// Live smoke covering `add_torrent_with_file_filter` narrowing
    /// (C1) and the re-narrow preservation contract (C2) against
    /// Deluge. Mirrors `qbittorrent::tests::live_smoke_narrowed` in
    /// intent; differences are at the wire-protocol layer only.
    ///
    ///     RYOKAN_DELUGE_E2E=1 cargo test \
    ///       deluge::tests::live_smoke_narrowed -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live Deluge at localhost:8112 + transmission-create"]
    async fn live_smoke_narrowed() {
        if std::env::var("RYOKAN_DELUGE_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_DELUGE_E2E=1 to run against localhost:8112)");
            return;
        }
        let Some((_tmp_guard, torrent_path)) = super::super::test_helpers::build_testpack_torrent()
        else {
            return;
        };
        let base_url = "http://localhost:8112";
        let password = "deluge";
        let label = "ryokan-e2e-narrow";

        let info_hash = upload_torrent_file_deluge(base_url, password, label, &torrent_path).await;
        eprintln!("uploaded testpack hash={info_hash}");

        let client = DelugeClient::new(base_url, password, label);

        let files = client
            .get_files(&info_hash)
            .await
            .expect("get_files should return metadata immediately");
        assert_eq!(
            files.len(),
            7,
            "synthetic testpack should have 7 files (5 episodes + sample + readme), got {}",
            files.len()
        );
        assert!(
            files.iter().all(|f| f.wanted),
            "all files should start wanted=true before narrow"
        );

        // C1: narrow to episode files only
        let episode_indices: Vec<usize> = files
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.name.contains("episode_").then_some(i))
            .collect();
        assert_eq!(episode_indices.len(), 5, "expected 5 episode files");

        let expected_episode_indices = episode_indices.clone();
        let magnet = format!("magnet:?xt=urn:btih:{info_hash}");
        let outcome = client
            .add_torrent_with_file_filter(&magnet, &info_hash, &mut |_names| {
                Some(expected_episode_indices.clone())
            })
            .await
            .expect("add_torrent_with_file_filter C1 failed");

        match outcome {
            SelectiveOutcome::Filtered(kept) => {
                let mut sorted_kept = kept.clone();
                sorted_kept.sort_unstable();
                let mut sorted_expected = episode_indices.clone();
                sorted_expected.sort_unstable();
                assert_eq!(
                    sorted_kept, sorted_expected,
                    "C1 Filtered(kept) should equal the pick indices"
                );
            }
            SelectiveOutcome::FullDownload => {
                panic!("C1 expected Filtered narrow, got FullDownload");
            }
        }

        let files_after_c1 = client
            .get_files(&info_hash)
            .await
            .expect("get_files after C1 failed");
        for (i, f) in files_after_c1.iter().enumerate() {
            let should_be_wanted = episode_indices.contains(&i);
            assert_eq!(
                f.wanted, should_be_wanted,
                "C1 post-narrow: file [{i}] ({}) wanted={} expected={}",
                f.name, f.wanted, should_be_wanted
            );
        }
        eprintln!("C1 narrowing verified");

        // C2: re-narrow with expanded pick (add sample.mkv)
        let expanded_indices: Vec<usize> = files_after_c1
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                (f.name.contains("episode_") || f.name.contains("sample")).then_some(i)
            })
            .collect();
        assert_eq!(expanded_indices.len(), 6);

        let expected_expanded = expanded_indices.clone();
        let outcome2 = client
            .add_torrent_with_file_filter(&magnet, &info_hash, &mut |_names| {
                Some(expected_expanded.clone())
            })
            .await
            .expect("add_torrent_with_file_filter C2 failed");
        assert!(matches!(outcome2, SelectiveOutcome::Filtered(_)));

        let files_after_c2 = client
            .get_files(&info_hash)
            .await
            .expect("get_files after C2 failed");
        for (i, f) in files_after_c2.iter().enumerate() {
            let should_be_wanted = expanded_indices.contains(&i);
            assert_eq!(
                f.wanted, should_be_wanted,
                "C2 post-renarrow: file [{i}] ({}) wanted={} expected={}",
                f.name, f.wanted, should_be_wanted
            );
        }
        eprintln!("C2 re-narrow verified");

        // A7: delete with delete_files=true removes torrent + files
        client
            .delete(&info_hash, true)
            .await
            .expect("delete(hash, true) failed");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let after = client
            .list_scoped()
            .await
            .expect("list_scoped after delete(true) failed");
        assert!(
            !after
                .iter()
                .any(|t| t.hash.eq_ignore_ascii_case(&info_hash)),
            "A7: torrent must not survive delete(_, true)"
        );
        eprintln!("A7 delete(true) verified");
        eprintln!("narrowed-smoke passed");
    }

    /// Live smoke for B2: Deluge `list_scoped` filters by the Label
    /// plugin label, so a torrent with a *different* label must not
    /// surface. Uploads one Ryokan-labeled and one foreign-labeled
    /// torrent, asserts only the Ryokan one appears in `list_scoped`.
    ///
    ///     RYOKAN_DELUGE_E2E=1 cargo test \
    ///       deluge::tests::live_smoke_scoped_exclusion -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live Deluge at localhost:8112 + transmission-create"]
    async fn live_smoke_scoped_exclusion() {
        if std::env::var("RYOKAN_DELUGE_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_DELUGE_E2E=1 to run against localhost:8112)");
            return;
        }
        let Some((_tmp1, torrent1)) =
            super::super::test_helpers::build_named_torrent("ryokan-scoped-test")
        else {
            return;
        };
        let Some((_tmp2, torrent2)) =
            super::super::test_helpers::build_named_torrent("other-tool-test")
        else {
            return;
        };
        let base_url = "http://localhost:8112";
        let password = "deluge";
        let ryokan_label = "ryokan-e2e-scope";
        let foreign_label = "other-tool-scope";

        let ryokan_hash =
            upload_torrent_file_deluge(base_url, password, ryokan_label, &torrent1).await;
        let foreign_hash =
            upload_torrent_file_deluge(base_url, password, foreign_label, &torrent2).await;
        eprintln!("ryokan={ryokan_hash} foreign={foreign_hash}");
        assert_ne!(ryokan_hash, foreign_hash);

        let client = DelugeClient::new(base_url, password, ryokan_label);
        let list = client
            .list_scoped()
            .await
            .expect("list_scoped should succeed");

        assert!(
            list.iter()
                .any(|t| t.hash.eq_ignore_ascii_case(&ryokan_hash)),
            "B2: Ryokan-labeled torrent must appear in list_scoped"
        );
        assert!(
            !list
                .iter()
                .any(|t| t.hash.eq_ignore_ascii_case(&foreign_hash)),
            "B2: foreign-labeled torrent must NOT appear (found {foreign_hash})"
        );
        eprintln!("B2 scoped exclusion verified");

        client
            .delete(&ryokan_hash, true)
            .await
            .expect("cleanup ryokan");
        let foreign_client = DelugeClient::new(base_url, password, foreign_label);
        foreign_client
            .delete(&foreign_hash, true)
            .await
            .expect("cleanup foreign");
        eprintln!("scoped-exclusion smoke passed");
    }

    /// Error-path live smoke (F1 / F2 / F3) against Deluge.
    #[tokio::test]
    #[ignore = "requires live Deluge at localhost:8112"]
    async fn live_smoke_error_paths() {
        if std::env::var("RYOKAN_DELUGE_E2E").is_err() {
            eprintln!("skipping");
            return;
        }
        let client = DelugeClient::new("http://localhost:8112", "deluge", "ryokan-e2e-errs");
        let fake_hash = "0000000000000000000000000000000000000000";

        let result = client.delete(fake_hash, false).await;
        eprintln!("F1 Deluge delete(non-existent) → {result:?}");

        let result = client.get_files(fake_hash).await;
        eprintln!("F2 Deluge get_files(non-existent) → {result:?}");
        if let Ok(files) = result {
            assert!(files.is_empty(), "F2: Ok result must be empty");
        }

        let result = client
            .add_torrent("this-is-not-a-valid-url-or-magnet", fake_hash)
            .await;
        eprintln!("F3 Deluge add(malformed) → {result:?}");
        assert!(
            result.is_err(),
            "F3: add_torrent with malformed URL must return Err (got {result:?})"
        );

        eprintln!("error-paths smoke passed");
    }

    /// E1+E2 live smoke for Deluge: state transitions through
    /// pause→resume→pause, progress sanity in [0.0, 1.0].
    #[tokio::test]
    #[ignore = "requires live Deluge at localhost:8112 + transmission-create"]
    async fn live_smoke_state_progress() {
        if std::env::var("RYOKAN_DELUGE_E2E").is_err() {
            eprintln!("skipping");
            return;
        }
        let Some((_tmp, torrent_path)) = super::super::test_helpers::build_testpack_torrent()
        else {
            return;
        };
        let base_url = "http://localhost:8112";
        let password = "deluge";
        let label = "ryokan-e2e-state";

        let info_hash = upload_torrent_file_deluge(base_url, password, label, &torrent_path).await;
        let client = DelugeClient::new(base_url, password, label);

        async fn poll_until_state(
            client: &DelugeClient,
            hash: &str,
            acceptable: &[DownloadItemState],
        ) -> DownloadItem {
            for _ in 0..30 {
                let list = client.list_scoped().await.expect("list_scoped");
                if let Some(t) = list
                    .iter()
                    .find(|t| t.hash.eq_ignore_ascii_case(hash))
                    .cloned()
                    && acceptable.contains(&t.state_kind)
                {
                    return t;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let list = client.list_scoped().await.expect("list_scoped");
            list.iter()
                .find(|t| t.hash.eq_ignore_ascii_case(hash))
                .cloned()
                .unwrap_or_else(|| panic!("torrent never appeared"))
        }

        // Uploaded with add_paused=true → expect Paused.
        let t = poll_until_state(
            &client,
            &info_hash,
            &[DownloadItemState::Paused, DownloadItemState::PausedComplete],
        )
        .await;
        eprintln!(
            "E1 Deluge paused: state={:?} ({}) progress={}",
            t.state_kind, t.state, t.progress
        );
        assert!(
            matches!(
                t.state_kind,
                DownloadItemState::Paused | DownloadItemState::PausedComplete
            ),
            "E1: Paused expected, got {:?} ({})",
            t.state_kind,
            t.state
        );
        assert!(
            (0.0..=1.0).contains(&t.progress),
            "E2 progress: {}",
            t.progress
        );

        client.resume(&info_hash).await.expect("resume");
        let t = poll_until_state(
            &client,
            &info_hash,
            &[
                DownloadItemState::Downloading,
                DownloadItemState::DownloadingStalled,
                DownloadItemState::DownloadingQueued,
                DownloadItemState::CheckingDownload,
            ],
        )
        .await;
        eprintln!(
            "E1 Deluge resumed: state={:?} ({}) progress={}",
            t.state_kind, t.state, t.progress
        );
        assert!(
            matches!(
                t.state_kind,
                DownloadItemState::Downloading
                    | DownloadItemState::DownloadingStalled
                    | DownloadItemState::DownloadingQueued
                    | DownloadItemState::CheckingDownload
            ),
            "E1: Downloading* expected after resume, got {:?} ({})",
            t.state_kind,
            t.state
        );

        client.pause(&info_hash).await.expect("pause");
        let t = poll_until_state(
            &client,
            &info_hash,
            &[DownloadItemState::Paused, DownloadItemState::PausedComplete],
        )
        .await;
        eprintln!(
            "E1 Deluge re-paused: state={:?} ({}) progress={}",
            t.state_kind, t.state, t.progress
        );
        assert!(matches!(
            t.state_kind,
            DownloadItemState::Paused | DownloadItemState::PausedComplete
        ));

        client.delete(&info_hash, true).await.expect("cleanup");
        eprintln!("state-progress smoke passed");
    }
}

/// Wire-level JSON-RPC coverage via `wiremock`. Parallel to the
/// qBit wiremock tree — covers the Deluge-specific quirks (two-step
/// connect handshake, Label plugin auto-enable + reconnect, 0/4
/// file priority, single-hash `core.remove_torrent`) against a
/// mock `/json` endpoint.
#[cfg(test)]
mod wiremock_tests;

#[cfg(test)]
mod seeding_done_tests {
    //! Issue #228: Deluge pauses a torrent at `stop_ratio` when
    //! `stop_at_ratio` is on; that paused, finished, at-ratio state is
    //! what `deluge_seeding_done` looks for.
    use super::*;

    fn raw(
        state: &str,
        is_finished: bool,
        stop_at_ratio: bool,
        ratio: f64,
        stop_ratio: f64,
    ) -> DelugeRawTorrent {
        DelugeRawTorrent {
            state: state.into(),
            is_finished,
            stop_at_ratio,
            ratio,
            stop_ratio,
            ..Default::default()
        }
    }

    #[test]
    fn paused_finished_at_ratio_is_done() {
        assert!(deluge_seeding_done(&raw("Paused", true, true, 2.0, 2.0)));
        assert!(deluge_seeding_done(&raw("Paused", true, true, 2.4, 2.0)));
    }

    #[test]
    fn paused_without_a_ratio_stop_was_paused_by_hand() {
        assert!(!deluge_seeding_done(&raw("Paused", true, false, 5.0, 2.0)));
    }

    #[test]
    fn paused_below_the_ratio_is_not_done() {
        assert!(!deluge_seeding_done(&raw("Paused", true, true, 1.9, 2.0)));
    }

    #[test]
    fn seeding_or_unfinished_is_never_done() {
        assert!(!deluge_seeding_done(&raw("Seeding", true, true, 3.0, 2.0)));
        assert!(!deluge_seeding_done(&raw("Paused", false, true, 3.0, 2.0)));
    }

    #[test]
    fn a_status_without_the_keys_reads_as_not_done() {
        let t: DelugeRawTorrent = serde_json::from_value(serde_json::json!({
            "hash": "h", "name": "n", "state": "Paused", "is_finished": true
        }))
        .unwrap();
        assert!(!deluge_seeding_done(&t));
    }
}
