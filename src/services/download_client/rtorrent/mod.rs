//! rtorrent implementation of [`DownloadClient`]. Speaks XML-RPC over
//! HTTP to the `/RPC2` endpoint (the one every realistic deployment
//! exposes behind ruTorrent's nginx/lighttpd or a standalone
//! mod_scgi_mgr bridge — raw SCGI isn't worth supporting).
//!
//! rtorrent-specific quirks worth flagging (verified against rtorrent
//! 0.9.8 via ruTorrent LSIO container, live-probed 2026-04-21):
//!   - **Hashes are UPPERCASE on the wire.** Every `d.<method>` /
//!     `f.<method>` call keyed by hash takes an uppercase-hex string.
//!     The trait contract says callers pass lowercase hex; the
//!     [`UPPER_HASH`](call_on_torrent) conversion happens inside every
//!     helper, not at call sites.
//!   - **Every method takes a target**, even if it's a no-op. `d.*`
//!     methods' first param is the torrent hash; `f.*` methods' first
//!     param is `"<HASH>:f<index>"`; `d.multicall2` takes an empty
//!     string as its target (rtorrent's "all commands have a target,
//!     empty is legal" convention).
//!   - **Duplicate-add is silent.** `load.start_verbose` returns `0`
//!     on both a fresh add and a duplicate — no fault, no warning.
//!     Ryokan pre-checks by listing hashes and returns
//!     `AddOutcome::AlreadyPresent` when the hash is already known.
//!   - **File priority is binary 0/1, NOT Deluge's 0/4**, BUT after
//!     setting priorities you MUST call `d.update_priorities(<hash>)`
//!     or the new priorities don't take effect. The single most
//!     common "my script sets priorities and nothing happens" bug in
//!     rtorrent automation.
//!   - **`d.erase` does NOT touch disk.** Per rtorrent's cmd-ref
//!     verbatim: "the data stored for the item is not touched in any
//!     way." Ryokan reads `content_path` first, calls `d.erase`, then
//!     recursively removes the filesystem path — guarded by
//!     `content_path != d.directory` so a multi-file torrent dumped
//!     at the save root doesn't nuke the user's entire download dir.
//!   - **`d.base_path` is empty on closed/stopped torrents** (and
//!     after rtorrent restart). Fall back to `d.directory + "/" +
//!     d.name` when empty. During metadata fetch base_path ends in
//!     `.meta` — also a signal metadata hasn't arrived yet.
//!   - **Metadata-ready signal**: pre-metadata magnets show
//!     `base_path = ".../{HASH}.meta"` with `size_bytes=1`,
//!     `size_files=1`. Post-metadata: base_path rewrites to the
//!     actual content name. Poll for `!base_path.ends_with(".meta")`
//!     at 500ms cadence, **60s budget** — longer than the other
//!     clients because cold DHT legitimately takes longer.
//!   - **Duplicate-add detection via pre-check.** The plan doc
//!     suggested fault-string matching on `"info-hash already used"`
//!     but rtorrent 0.9.8 silently accepts dup-adds, so we pre-check
//!     with a `d.multicall2` and short-circuit before `load.start_verbose`.
//!   - **i8 vs i4 wire tags.** rtorrent returns `<i8>` for sizes,
//!     rates, and most counters; the decoder accepts both.

use async_trait::async_trait;
use reqwest::Client;
use std::time::{Duration, Instant};

use super::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};

mod xmlrpc;

use xmlrpc::{XmlValue, decode_response, encode_request, xml_attr_escape};

/// 60s budget vs the 10s used by qBit/Deluge/Transmission — cold DHT
/// fetches on a fresh rtorrent legitimately take longer, and the
/// alternative (fall through to FullDownload after 10s) is worse
/// than waiting a minute once.
const METADATA_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Normalize a user-entered base URL into a full XML-RPC endpoint.
/// Trims trailing slashes and appends `/RPC2` when the path doesn't
/// already end in it (case-insensitive so `/rpc2` pasted from docs is
/// treated as already-canonical). Empty in → empty out so an
/// unconfigured client builds without a dangling `/RPC2`.
///
/// The append happens here rather than at config-save time so the
/// stored URL matches what the user typed — same convention Deluge
/// and Transmission use for their own path suffixes (`/json` and
/// `/transmission/rpc` respectively).
fn canonicalize_endpoint(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.to_ascii_lowercase().ends_with("/rpc2") {
        return trimmed.to_string();
    }
    format!("{}/RPC2", trimmed)
}

pub struct RtorrentClient {
    /// Full URL of the XML-RPC endpoint (e.g. `http://host:8081/RPC2`).
    /// Constructed in [`RtorrentClient::new`] from the user-entered
    /// base URL by appending `/RPC2` when it isn't already there — same
    /// invisible-to-the-user convention Deluge (`/json`) and
    /// Transmission (`/transmission/rpc`) use so the stored config
    /// value stays as the user typed it.
    endpoint: String,
    user: String,
    password: String,
    label: String,
    http: Client,
}

impl RtorrentClient {
    pub fn new(base_url: &str, user: &str, password: &str, label: &str) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            endpoint: canonicalize_endpoint(base_url),
            user: user.to_string(),
            password: password.to_string(),
            label: if label.is_empty() {
                "ryokan".to_string()
            } else {
                label.to_string()
            },
            http,
        }
    }

    /// Wire-level XML-RPC round-trip. Returns the single `<param>`
    /// response value, or a decoded fault string.
    async fn call(&self, method: &str, params: &[XmlValue]) -> Result<XmlValue, String> {
        let body = encode_request(method, params);
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("Content-Type", "text/xml")
            .body(body);
        if !self.user.is_empty() || !self.password.is_empty() {
            req = req.basic_auth(&self.user, Some(&self.password));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("rtorrent request failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("rtorrent auth failed: check username/password".into());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("rtorrent HTTP {status}: {}", body.trim()));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| format!("rtorrent response read failed: {e}"))?;
        decode_response(&text)
    }

    /// Helper: call a method whose first arg is the torrent hash.
    /// Uppercases the hash before sending per the rtorrent contract.
    async fn call_on_torrent(
        &self,
        method: &str,
        info_hash: &str,
        extra: &[XmlValue],
    ) -> Result<XmlValue, String> {
        let hash_uc = info_hash.to_ascii_uppercase();
        let mut params = vec![XmlValue::String(hash_uc)];
        params.extend_from_slice(extra);
        self.call(method, &params).await
    }

    /// d.multicall2 against the `"main"` view with the given getter
    /// accessors. Returns rows as a vec of value vecs in getter order.
    async fn main_view(&self, getters: &[&str]) -> Result<Vec<Vec<XmlValue>>, String> {
        let mut params = vec![
            XmlValue::String(String::new()),
            XmlValue::String("main".into()),
        ];
        for g in getters {
            params.push(XmlValue::String((*g).into()));
        }
        let resp = self.call("d.multicall2", &params).await?;
        let rows = resp
            .into_array()
            .ok_or("d.multicall2 did not return an array")?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let cols = row
                .into_array()
                .ok_or("d.multicall2 row was not an array")?;
            out.push(cols);
        }
        Ok(out)
    }

    async fn file_list(&self, info_hash: &str) -> Result<Vec<FileRow>, String> {
        let hash_uc = info_hash.to_ascii_uppercase();
        let params = vec![
            XmlValue::String(hash_uc),
            XmlValue::String(String::new()),
            XmlValue::String("f.path=".into()),
            XmlValue::String("f.size_bytes=".into()),
            XmlValue::String("f.priority=".into()),
            XmlValue::String("f.completed_chunks=".into()),
            XmlValue::String("f.size_chunks=".into()),
        ];
        let resp = self.call("f.multicall", &params).await?;
        let rows = resp
            .into_array()
            .ok_or("f.multicall did not return an array")?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let cols = row.into_array().ok_or("f.multicall row was not an array")?;
            if cols.len() < 5 {
                return Err("f.multicall row had < 5 columns".into());
            }
            out.push(FileRow {
                path: cols[0].as_string().unwrap_or_default().to_string(),
                size: cols[1].as_int().unwrap_or(0),
                priority: cols[2].as_int().unwrap_or(1) as i32,
                completed_chunks: cols[3].as_int().unwrap_or(0),
                size_chunks: cols[4].as_int().unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// Does a hash currently exist in the client? Used to detect
    /// duplicate adds (rtorrent is silent about them — see module doc).
    async fn hash_exists(&self, info_hash: &str) -> Result<bool, String> {
        let rows = self.main_view(&["d.hash="]).await?;
        let hash_uc = info_hash.to_ascii_uppercase();
        Ok(rows
            .into_iter()
            .filter_map(|row| row.into_iter().next())
            .any(|v| {
                v.as_string()
                    .map(|s| s.eq_ignore_ascii_case(&hash_uc))
                    .unwrap_or(false)
            }))
    }
}

struct FileRow {
    path: String,
    size: i64,
    priority: i32,
    completed_chunks: i64,
    size_chunks: i64,
}

#[async_trait]
impl DownloadClient for RtorrentClient {
    async fn test(&self) -> Result<String, String> {
        let v = self.call("system.client_version", &[]).await?;
        v.as_string()
            .ok_or_else(|| "rtorrent system.client_version returned non-string".into())
            .map(|s| s.to_string())
    }

    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        // Up-front URL-shape validation. rtorrent's `load.start_verbose`
        // silently accepts garbage-string inputs and returns 0 (success)
        // without actually creating a torrent — caller can't tell a
        // typo'd URL from a real add. Surfaced 2026-04-23 as an #85
        // parity gap (qBit / Deluge / Transmission all reject
        // malformed URLs with an RPC-level error; only rtorrent
        // swallowed them). Reject anything that isn't a magnet URI
        // or an http(s) URL before burning an XML-RPC round trip.
        // Lowercase the scheme first so `MAGNET:` / `HTTP://` also
        // match — RFC 3986 schemes are case-insensitive. Internal
        // Ryokan call sites always emit lowercase, but third-party
        // integrations (a future torznab-pushed release, a hand-
        // edited feed URL) might not.
        let lowered = url.trim().to_ascii_lowercase();
        let looks_valid = lowered.starts_with("magnet:")
            || lowered.starts_with("http://")
            || lowered.starts_with("https://");
        if !looks_valid {
            return Err(format!(
                "rtorrent add rejected url={url}: expected magnet: / http:// / https:// scheme"
            ));
        }

        // Pre-check — rtorrent silently accepts duplicate adds so we
        // need to detect them ourselves. The cost is one extra
        // multicall, which is cheap relative to the magnet load that
        // follows.
        // NOTE: hash_exists is O(all-daemon-torrents). Typical Ryokan
        // deployments stay under the ≤100 assumption in list_scoped's
        // comment; a shared seedbox with thousands of torrents would
        // pay this cost per add_torrent. Server-side filter-view is
        // heavier code; defer until a real user hits it.
        let already = if info_hash.is_empty() {
            false
        } else {
            match self.hash_exists(info_hash).await {
                Ok(b) => b,
                Err(e) => {
                    // Swallow and fall through to load.start_verbose —
                    // rtorrent's silent dup-add means the torrent ends
                    // up added once either way. If the underlying issue
                    // (network / auth) persists, the load.start_verbose
                    // that follows will surface a clearer error.
                    tracing::debug!(
                        target: "ryokan::download_client::rtorrent",
                        error = %e,
                        "hash_exists pre-check failed; falling through to load.start_verbose"
                    );
                    false
                }
            }
        };
        if already {
            // Re-apply the scoping label in case the user (or a prior
            // session) added this torrent without it — matches the
            // Deluge/Transmission impls' "adopt existing torrent"
            // semantics so it becomes visible to list_scoped.
            if !self.label.is_empty() {
                let _ = self
                    .call_on_torrent(
                        "d.custom1.set",
                        info_hash,
                        &[XmlValue::String(self.label.clone())],
                    )
                    .await;
            }
            return Ok(AddOutcome::AlreadyPresent);
        }

        // load.start_verbose takes (target, uri, commands...). The
        // trailing commands are executed on the new torrent *before*
        // it's started, which is the idiomatic place to stamp the
        // custom1 label. Without this the torrent appears in `main`
        // unlabeled for one tick and briefly leaks out of
        // list_scoped's filter.
        let label_cmd = format!("d.custom1.set=\"{}\"", xml_attr_escape(&self.label));
        self.call(
            "load.start_verbose",
            &[
                XmlValue::String(String::new()),
                XmlValue::String(url.to_string()),
                XmlValue::String(label_cmd),
            ],
        )
        .await?;
        Ok(AddOutcome::Added)
    }

    /// Add a torrent for the interactive file picker. rtorrent's
    /// `d.pause` is a soft flag — it lowers upload rates but doesn't
    /// fully stop peer communication, so a torrent that's paused
    /// immediately after load can still be fetching chunks by the
    /// time the user opens the picker modal. The contract the picker
    /// relies on ("no file data is being downloaded until confirm")
    /// breaks.
    ///
    /// Workaround mirrors qBit / Deluge / Transmission: add running,
    /// wait for metadata via the `base_path` `.meta`-sentinel signal,
    /// set every file's priority to 0 (skip) + mandatory
    /// `d.update_priorities` flush, then pause. Every file at priority
    /// 0 means no chunks flow regardless of what pause means; pausing
    /// afterward just stops the peer churn while the user deliberates.
    async fn add_torrent_paused(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        let outcome = self.add_torrent(url, info_hash).await?;

        if info_hash.is_empty() {
            // No pre-computed hash → can't address the post-add calls.
            // Caller gets back the Added/AlreadyPresent outcome but
            // the torrent may be running. Upstream validation normally
            // ensures info_hash is set for paused adds, but fall
            // through gracefully rather than hard-failing.
            return Ok(outcome);
        }

        // Don't touch a pre-existing torrent. If the user already has
        // this release running from a prior grab, blanket-skipping
        // all its files out from under them is destructive — the
        // handler is responsible for surfacing the existing state to
        // the modal instead (same-hash dedup flow, plan decision #6).
        if outcome == AddOutcome::AlreadyPresent {
            return Ok(outcome);
        }

        // Small settle loop: wait until the hash appears in
        // `d.multicall2`. `load.start_verbose` returns before rtorrent
        // finishes registering the torrent; subsequent commands
        // return "No such download" on some versions. Budget 2s.
        let hash_uc = info_hash.to_ascii_uppercase();
        let start = Instant::now();
        let mut delay = Duration::from_millis(100);
        loop {
            match self.hash_exists(&hash_uc).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!(
                        target: "ryokan::download_client::rtorrent",
                        error = %e,
                        "hash_exists poll failed during paused-add settle"
                    );
                }
            }
            if start.elapsed() >= Duration::from_secs(2) {
                return Ok(outcome);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        }

        // Poll for metadata. `file_list` is the raw `f.multicall`
        // helper and returns the `<HASH>.meta` placeholder (size 1)
        // during metadata fetch — `files.is_empty()` is NOT a
        // sufficient "not ready" signal because the placeholder
        // satisfies it. The `.meta` filter on the trait-level
        // `get_files` is a separate layer and doesn't fire here.
        // Require at least one non-`.meta` entry; rtorrent's
        // metadata-pending and metadata-arrived states are mutually
        // exclusive, so one real file means the whole list is real.
        let metadata_start = Instant::now();
        let mut metadata_delay = Duration::from_millis(500);
        let file_count = loop {
            match self.file_list(info_hash).await {
                Ok(files) if files.iter().any(|f| !f.path.ends_with(".meta")) => {
                    break files.len();
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(
                        target: "ryokan::download_client::rtorrent",
                        error = %e,
                        "file_list poll failed during paused-add metadata wait"
                    );
                }
            }
            if metadata_start.elapsed() >= METADATA_WAIT_TIMEOUT {
                // Timeout fallback matches the other impls: try to
                // pause the still-metadata-less torrent and return
                // Added. Picker modal will surface an error on its
                // polling loop; the sweep auto-commits on TTL.
                let _ = self.pause(info_hash).await;
                return Ok(outcome);
            }
            tokio::time::sleep(metadata_delay).await;
            metadata_delay = (metadata_delay * 2).min(Duration::from_secs(2));
        };

        // Skip every file. Mandatory `d.update_priorities` flush or
        // the writes don't take effect — the single biggest rtorrent
        // gotcha this impl exists to paper over.
        for i in 0..file_count {
            self.call(
                "f.priority.set",
                &[
                    XmlValue::String(format!("{hash_uc}:f{i}")),
                    XmlValue::Int(0),
                ],
            )
            .await?;
        }
        self.call("d.update_priorities", &[XmlValue::String(hash_uc)])
            .await?;

        self.pause(info_hash).await?;
        Ok(outcome)
    }

    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        info_hash: &str,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        if info_hash.is_empty() {
            return Err("rtorrent selective download requires a known info hash".into());
        }

        self.add_torrent(url, info_hash).await?;

        // Poll for metadata readiness. Signal is base_path no longer
        // ending in `.meta` — size_files/size_bytes are always at
        // least 1 even pre-metadata (the sentinel file), so the
        // plan-doc-proposed `size_bytes > 0` heuristic doesn't work
        // in practice.
        let hash_uc = info_hash.to_ascii_uppercase();
        // Hoisted out of the poll loop — params are invariant across
        // iterations and there's no reason to rebuild on every tick.
        let poll_params = vec![
            XmlValue::String(String::new()),
            XmlValue::String("main".into()),
            XmlValue::String("d.hash=".into()),
            XmlValue::String("d.base_path=".into()),
        ];
        let start = Instant::now();
        let mut delay = Duration::from_millis(500);
        let files: Vec<FileRow> = loop {
            let rows_resp = self.call("d.multicall2", &poll_params).await?;
            let rows = rows_resp
                .into_array()
                .ok_or("d.multicall2 did not return an array")?;
            let ready = rows.into_iter().any(|row| {
                let cols = row.into_array().unwrap_or_default();
                if cols.len() < 2 {
                    return false;
                }
                let hash_match = cols[0]
                    .as_string()
                    .map(|s| s.eq_ignore_ascii_case(&hash_uc))
                    .unwrap_or(false);
                let base_ready = cols[1]
                    .as_string()
                    .map(|s| !s.is_empty() && !s.ends_with(".meta"))
                    .unwrap_or(false);
                hash_match && base_ready
            });
            if ready {
                break self.file_list(info_hash).await?;
            }
            if start.elapsed() >= METADATA_WAIT_TIMEOUT {
                // Same fallback as qBit/Deluge/Transmission: torrent
                // stays added, full download. The label is already
                // stamped so list_scoped will still see it once
                // metadata eventually arrives.
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

        // Re-narrow idempotency: if any existing file is priority 0
        // (skip), merge additively — only flip new keeps to 1 and
        // leave existing unwanted files alone.
        let already_narrowed = files.iter().any(|f| f.priority == 0);

        for (i, f) in files.iter().enumerate() {
            let target_prio = if already_narrowed {
                if keep_indices.contains(&i) && f.priority == 0 {
                    Some(1)
                } else {
                    None
                }
            } else if keep_indices.contains(&i) {
                Some(1)
            } else {
                Some(0)
            };
            if let Some(p) = target_prio {
                self.call(
                    "f.priority.set",
                    &[
                        XmlValue::String(format!("{hash_uc}:f{i}")),
                        XmlValue::Int(p as i64),
                    ],
                )
                .await?;
            }
        }

        // Mandatory: without this, the priority writes above don't
        // actually take effect. This is the single biggest rtorrent
        // gotcha and the reason this trait impl has a long comment.
        self.call("d.update_priorities", &[XmlValue::String(hash_uc.clone())])
            .await?;

        Ok(SelectiveOutcome::Filtered(keep_indices))
    }

    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        // One multicall fetches all the columns list_scoped needs.
        // rtorrent doesn't have a server-side "custom1 equals" filter
        // on d.multicall2, so we pull all and filter client-side —
        // the typical scoping size is tiny (≤100 torrents) so this is
        // fine. If that changes we can switch to the filter-view
        // idiom (`d.views.new_filtered` / `d.custom1=`) once.
        let getters = &[
            "d.hash=",
            "d.name=",
            "d.size_bytes=",
            "d.bytes_done=",
            "d.down.rate=",
            "d.custom1=",
            "d.complete=",
            "d.is_active=",
            "d.hashing=",
            "d.is_open=",
            "d.message=",
            "d.base_path=",
            "d.directory=",
            // Issue #228: set by the default ratio-group action
            // (`d.try_close= ; d.ignore_commands.set=1`), which is the
            // one signal that separates "closed at ratio" from a stop
            // by hand or after a restart. The reader tolerates a
            // 13-column row (older fixtures) as "not set"; an rTorrent
            // without the command would fault the whole multicall, but
            // `d.ignore_commands` has been in the 0.9 series throughout.
            "d.ignore_commands=",
        ];
        let rows = self.main_view(getters).await?;

        let mut out = Vec::with_capacity(rows.len());
        for cols in rows {
            if cols.len() < 13 {
                continue;
            }
            let hash = cols[0].as_string().unwrap_or_default().to_ascii_lowercase();
            let name = cols[1].as_string().unwrap_or_default().to_string();
            let size = cols[2].as_int().unwrap_or(0);
            let bytes_done = cols[3].as_int().unwrap_or(0);
            let dlspeed = cols[4].as_int().unwrap_or(0);
            let custom1 = cols[5].as_string().unwrap_or_default().to_string();
            if custom1 != self.label {
                continue;
            }
            let complete = cols[6].as_int().unwrap_or(0) != 0;
            let is_active = cols[7].as_int().unwrap_or(0) != 0;
            let hashing = cols[8].as_int().unwrap_or(0) != 0;
            let is_open = cols[9].as_int().unwrap_or(0) != 0;
            let message = cols[10].as_string().unwrap_or_default().to_string();
            let base_path = cols[11].as_string().unwrap_or_default().to_string();
            let directory = cols[12].as_string().unwrap_or_default().to_string();
            let ignore_commands = cols.get(13).and_then(|v| v.as_int()).unwrap_or(0) != 0;

            let state_kind = map_state(complete, is_active, hashing, is_open, &message);
            let state_str = state_label(state_kind);
            let progress = if size > 0 {
                (bytes_done as f64 / size as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let save_path = directory.clone();
            let content_path = content_path(&base_path, &directory, &name);
            // rTorrent's default ratio-group action closes a finished
            // item and sets its ignore flag; only that combination is
            // done seeding (issue #228). A stop by hand leaves the item
            // open; a restart reloads stopped items closed but without
            // the flag; a tracker or disk error carries a message. A
            // custom group action that does not set the flag never
            // reads as done, which is the safe way to be wrong.
            let seeding_done =
                complete && !is_open && !is_active && ignore_commands && message.is_empty();
            out.push(DownloadItem {
                hash,
                name,
                size,
                progress,
                dlspeed,
                state: state_str.to_string(),
                category: custom1,
                eta: 0,
                save_path,
                content_path,
                state_kind,
                seeding_done,
            });
        }
        Ok(out)
    }

    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String> {
        let files = self.file_list(info_hash).await?;
        Ok(files
            .into_iter()
            // During metadata fetch rtorrent exposes a single
            // `<UPPERCASE-HASH>.meta` placeholder (size 1) before the
            // real file list arrives. The `DownloadClient` trait
            // contract says an empty `get_files` = metadata not ready,
            // so filter the placeholder out or the preview endpoint
            // flips to `status: ready` with one fake file and the
            // picker modal renders a checkbox next to a .meta stub.
            .filter(|f| !f.path.ends_with(".meta"))
            .map(|f| {
                let progress = if f.size_chunks > 0 {
                    (f.completed_chunks as f64 / f.size_chunks as f64).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                DownloadFile {
                    name: f.path,
                    size: f.size,
                    progress,
                    wanted: f.priority != 0,
                }
            })
            .collect())
    }

    async fn pause(&self, info_hash: &str) -> Result<(), String> {
        self.call_on_torrent("d.pause", info_hash, &[]).await?;
        Ok(())
    }

    async fn resume(&self, info_hash: &str) -> Result<(), String> {
        self.call_on_torrent("d.resume", info_hash, &[]).await?;
        Ok(())
    }

    async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), String> {
        // rtorrent's d.erase does NOT touch disk. For delete_files=true
        // we read base_path / directory first, erase from the client,
        // then rm the filesystem path — guarded by
        // `base_path != directory` so a torrent dumped at the save
        // root doesn't take the entire download dir down with it.
        let paths = if delete_files {
            let rows = self
                .main_view(&["d.hash=", "d.base_path=", "d.directory=", "d.name="])
                .await?;
            let hash_uc = info_hash.to_ascii_uppercase();
            rows.into_iter().find_map(|cols| {
                if cols.len() < 4 {
                    return None;
                }
                let h = cols[0].as_string().unwrap_or_default();
                if !h.eq_ignore_ascii_case(&hash_uc) {
                    return None;
                }
                let base = cols[1].as_string().unwrap_or_default().to_string();
                let dir = cols[2].as_string().unwrap_or_default().to_string();
                let name = cols[3].as_string().unwrap_or_default().to_string();
                Some((base, dir, name))
            })
        } else {
            None
        };

        self.call_on_torrent("d.erase", info_hash, &[]).await?;

        if let Some((base_path, directory, name)) = paths
            && let Some(effective) = safe_delete_target(&base_path, &directory, &name)
        {
            tokio::task::spawn_blocking(move || {
                let p = std::path::Path::new(&effective);
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(p);
                } else if p.exists() {
                    let _ = std::fs::remove_file(p);
                }
            })
            .await
            .map_err(|e| format!("rtorrent delete: spawn_blocking failed: {e}"))?;
        }
        Ok(())
    }

    async fn set_file_wanted(
        &self,
        info_hash: &str,
        files: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        let hash_uc = info_hash.to_ascii_uppercase();
        let target = if wanted { 1 } else { 0 };
        for &i in files {
            self.call(
                "f.priority.set",
                &[
                    XmlValue::String(format!("{hash_uc}:f{i}")),
                    XmlValue::Int(target),
                ],
            )
            .await?;
        }
        self.call("d.update_priorities", &[XmlValue::String(hash_uc)])
            .await?;
        Ok(())
    }

    fn sonarr_impl_name(&self) -> &'static str {
        "RTorrent"
    }

    /// Issue #28 asked for per-torrent seed rules; rTorrent has none
    /// (issue #228). Its only per-item ratio command is the read-only
    /// `d.ratio`; ratio handling is configured per *group* in
    /// `.rtorrent.rc` (`group.seeding.ratio.enable`,
    /// `group2.seeding.ratio.min/max/upload.set`, the action in
    /// `group.seeding.ratio.command`) and applies to every item in the
    /// group. Until #228 this called a `d.ratio.max.set` that does not
    /// exist, so every seed-ruled grab faulted and nothing was applied.
    /// Returning `Err` makes `apply_indexer_seed_rules` log the gap per
    /// grab; the `respect_seed_rules` flag is still set, so Ryokan
    /// keeps its hands off the item and rTorrent's own ratio group
    /// decides when seeding ends.
    async fn set_seed_rules(&self, info_hash: &str, rules: super::SeedRules) -> Result<(), String> {
        let ratio = rules
            .ratio
            .map(|r| r.to_string())
            .unwrap_or_else(|| "none".to_string());
        let time = rules
            .time_minutes
            .map(|m| format!("{m}m"))
            .unwrap_or_else(|| "none".to_string());
        Err(format!(
            "rTorrent has no per-torrent seed limits (ratio={ratio} time={time} not applied to {info_hash}); configure a ratio group in .rtorrent.rc"
        ))
    }
}

/// Resolve a filesystem path to remove for `delete(delete_files=true)`,
/// applying three safety rails:
///
///   1. **Empty ends → no-op.** Nothing reliable to delete.
///   2. **`.meta` sentinel → no-op.** Pre-metadata torrent; rtorrent
///      handles its own cleanup and there's no real content to remove.
///   3. **Never delete the save root or any ancestor of it.**
///      Normalization of trailing slashes prevents a
///      `base_path = "/downloads/"` vs `directory = "/downloads"`
///      mismatch from bypassing the guard. The ancestor check is
///      belt-and-braces against a misconfigured rtorrent that reports
///      a `base_path` *above* the `directory` — a bug we shouldn't
///      paper over by wiping the user's entire download tree.
///      Note: `effective == "/"` is caught by the empty-after-normalize
///      branch (`trim_end_matches('/')` on `"/"` yields `""`), not by
///      an explicit `== "/"` check.
///
/// **Out of scope: symlink traversal.** If rtorrent reports a
/// `base_path` that's a symlink to somewhere else on disk,
/// `remove_dir_all` follows through the link. The threat model here
/// assumes a trustworthy daemon; a compromised rtorrent is outside
/// what this function defends against. Flagged so anyone porting
/// this logic into a less-trusted context knows the gap.
///
/// Returns `None` if no safe removal target exists. Callers treat
/// `None` as "client-side erase already happened; disk unchanged."
fn safe_delete_target(base_path: &str, directory: &str, name: &str) -> Option<String> {
    let effective = if !base_path.is_empty() && !base_path.ends_with(".meta") {
        base_path.to_string()
    } else if !directory.is_empty() && !name.is_empty() {
        format!("{}/{}", directory.trim_end_matches('/'), name)
    } else {
        return None;
    };

    let norm = |p: &str| p.trim_end_matches('/').to_string();
    let e = norm(&effective);
    let d = norm(directory);
    // Empty after normalization catches the `effective = "/"` case
    // (no explicit root check needed — `trim_end_matches('/')` strips
    // the single slash leaving "") plus the degenerate empty-input
    // case.
    if e.is_empty() || d.is_empty() {
        return None;
    }
    // Equal-after-normalization: dumped at save root. Refuse.
    if e == d {
        return None;
    }
    // Effective is an ancestor of directory. `d.starts_with("e/")`
    // catches cases like effective="/downloads", directory="/downloads/anime".
    let e_prefix = format!("{e}/");
    if d.starts_with(&e_prefix) {
        return None;
    }
    Some(effective)
}

fn content_path(base_path: &str, directory: &str, name: &str) -> String {
    // Base path is rtorrent's authoritative content location when
    // populated and not the .meta sentinel. When empty (closed /
    // stopped / post-restart) or mid-metadata-fetch, reconstruct from
    // `directory + "/" + name` per the kannibalox cmd-ref fallback.
    if !base_path.is_empty() && !base_path.ends_with(".meta") {
        base_path.to_string()
    } else if !directory.is_empty() && !name.is_empty() {
        format!("{}/{}", directory.trim_end_matches('/'), name)
    } else {
        String::new()
    }
}

/// 10-variant state mapping from rtorrent's (complete, is_active,
/// hashing, is_open, message) signals. rtorrent has fewer distinct
/// UI states than qBit; the Paused/PausedComplete and Checking
/// distinctions are preserved but stalled-vs-active isn't exposed
/// natively so we collapse it into Downloading/Seeding.
///
/// **Errored is intentionally conservative.** We only surface Errored
/// when the torrent is entirely stopped (`!is_active && !hashing &&
/// !is_open`) with a non-empty `d.message`. An active torrent that's
/// seeing a persistent tracker 410 won't flag as Errored here — it'll
/// show Downloading/Seeding with the raw message still available in
/// `DownloadItem.state` for the UI. This matches the rest of the
/// trait's "prefer false negatives over false positives" error
/// semantics: transient tracker hiccups on running torrents are
/// normal and Ryokan shouldn't panic every time a tracker burps.
fn map_state(
    complete: bool,
    is_active: bool,
    hashing: bool,
    is_open: bool,
    message: &str,
) -> DownloadItemState {
    use DownloadItemState::*;
    // rtorrent uses `d.message` to surface tracker errors and other
    // fault conditions. Non-empty + "[No peers]" etc. are normal; we
    // only flag errors on messages rtorrent itself classifies as such.
    // For now, treat any non-empty non-tracker-info message as Errored
    // only if hashing is false AND is_active is false — conservative
    // to avoid flagging transient tracker hiccups.
    if !message.is_empty() && !is_active && !hashing && !is_open {
        return Errored;
    }
    if hashing {
        return if complete {
            CheckingSeed
        } else {
            CheckingDownload
        };
    }
    if !is_open || !is_active {
        return if complete { PausedComplete } else { Paused };
    }
    if complete { Seeding } else { Downloading }
}

fn state_label(s: DownloadItemState) -> &'static str {
    use DownloadItemState::*;
    match s {
        Downloading => "Downloading",
        DownloadingStalled => "Stalled (DL)",
        DownloadingQueued => "Queued (DL)",
        CheckingDownload => "Checking",
        Seeding => "Seeding",
        SeedingStalled => "Stalled (UL)",
        SeedingQueued => "Queued (UL)",
        CheckingSeed => "Checking (seed)",
        Paused => "Paused",
        PausedComplete => "Paused (complete)",
        Errored => "Errored",
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn rtorrent_client_is_object_safe() {
        fn _assert_dyn_compatible(_c: Arc<dyn DownloadClient>) {}
        let c = Arc::new(RtorrentClient::new(
            "http://localhost:8081/RPC2",
            "",
            "",
            "ryokan",
        )) as Arc<dyn DownloadClient>;
        _assert_dyn_compatible(c);
    }

    #[test]
    fn sonarr_impl_name_is_rtorrent() {
        let c = RtorrentClient::new("http://localhost:8081/RPC2", "", "", "ryokan");
        assert_eq!(c.sonarr_impl_name(), "RTorrent");
    }

    #[test]
    fn empty_label_defaults_to_ryokan() {
        let c = RtorrentClient::new("http://localhost:8081/RPC2", "", "", "");
        assert_eq!(c.label, "ryokan");
    }

    #[test]
    fn endpoint_appends_rpc2_when_missing() {
        let c = RtorrentClient::new("http://host:8081", "", "", "ryokan");
        assert_eq!(c.endpoint, "http://host:8081/RPC2");
    }

    #[test]
    fn endpoint_strips_trailing_slash_then_appends() {
        let c = RtorrentClient::new("http://host:8081/", "", "", "ryokan");
        assert_eq!(c.endpoint, "http://host:8081/RPC2");
    }

    #[test]
    fn endpoint_preserves_existing_rpc2_suffix() {
        let c = RtorrentClient::new("http://host:8081/RPC2", "", "", "ryokan");
        assert_eq!(c.endpoint, "http://host:8081/RPC2");
    }

    #[test]
    fn endpoint_tolerates_lowercase_rpc2() {
        let c = RtorrentClient::new("http://host:8081/rpc2", "", "", "ryokan");
        assert_eq!(c.endpoint, "http://host:8081/rpc2");
    }

    #[test]
    fn endpoint_empty_in_empty_out() {
        let c = RtorrentClient::new("", "", "", "ryokan");
        assert_eq!(c.endpoint, "");
        let c = RtorrentClient::new("   ", "", "", "ryokan");
        assert_eq!(c.endpoint, "");
    }

    #[test]
    fn endpoint_handles_seedbox_path_prefix() {
        let c = RtorrentClient::new("https://seedbox.example.com/rutorrent", "", "", "ryokan");
        assert_eq!(c.endpoint, "https://seedbox.example.com/rutorrent/RPC2");
    }

    #[test]
    fn content_path_prefers_base_path_when_populated() {
        // Normal running/complete torrent — base_path points at the
        // actual content location.
        assert_eq!(
            content_path("/downloads/sintel", "/downloads", "sintel"),
            "/downloads/sintel"
        );
    }

    #[test]
    fn content_path_falls_back_when_base_path_empty() {
        // Closed/stopped torrents: base_path comes back empty.
        // Reconstruct from directory + name.
        assert_eq!(content_path("", "/downloads", "Show"), "/downloads/Show");
        // Trailing-slash on directory is normalized.
        assert_eq!(content_path("", "/downloads/", "Show"), "/downloads/Show");
    }

    #[test]
    fn content_path_falls_back_when_base_path_is_meta_sentinel() {
        // Pre-metadata: rtorrent populates base_path with a `.meta`
        // sentinel, which isn't a real content location. Fall through
        // to directory+name (also not real yet, but more useful once
        // metadata arrives without another poll).
        assert_eq!(
            content_path("/downloads/incoming/ABC.meta", "/downloads", "Show"),
            "/downloads/Show"
        );
    }

    #[test]
    fn content_path_empty_when_no_inputs() {
        assert_eq!(content_path("", "", ""), "");
    }

    #[test]
    fn state_mapping_completion_semantics() {
        // Seeding: complete + active + open, not hashing.
        let s = map_state(true, true, false, true, "");
        assert_eq!(s, DownloadItemState::Seeding);
        assert!(s.is_complete());

        // Downloading.
        let s = map_state(false, true, false, true, "");
        assert_eq!(s, DownloadItemState::Downloading);
        assert!(!s.is_complete());

        // Paused complete.
        let s = map_state(true, false, false, false, "");
        assert_eq!(s, DownloadItemState::PausedComplete);
        assert!(s.is_complete());

        // Paused incomplete.
        let s = map_state(false, false, false, false, "");
        assert_eq!(s, DownloadItemState::Paused);
        assert!(!s.is_complete());

        // Hashing (verifying): Checking, complete or not.
        let s_checking_dl = map_state(false, false, true, false, "");
        assert_eq!(s_checking_dl, DownloadItemState::CheckingDownload);
        let s_checking_seed = map_state(true, false, true, false, "");
        assert_eq!(s_checking_seed, DownloadItemState::CheckingSeed);

        // Errored: non-empty message AND not active AND not hashing AND not open.
        let s = map_state(false, false, false, false, "Tracker returned 410");
        assert_eq!(s, DownloadItemState::Errored);
        assert!(s.is_errored());
    }

    #[test]
    fn encode_request_method_only() {
        let s = encode_request("system.client_version", &[]);
        assert!(s.contains("<methodName>system.client_version</methodName>"));
        assert!(s.contains("<params></params>"));
    }

    #[test]
    fn encode_request_with_string_and_int() {
        let s = encode_request(
            "d.multicall2",
            &[
                XmlValue::String("".into()),
                XmlValue::String("main".into()),
                XmlValue::Int(42),
            ],
        );
        assert!(s.contains("<value><string></string></value>"));
        assert!(s.contains("<value><string>main</string></value>"));
        assert!(s.contains("<value><i8>42</i8></value>"));
    }

    #[test]
    fn encode_escapes_xml_specials_in_strings() {
        let s = encode_request("m", &[XmlValue::String("a & b < c > d".into())]);
        assert!(s.contains("a &amp; b &lt; c &gt; d"));
    }

    #[test]
    fn xml_attr_escape_handles_backslash_and_quote() {
        // rtorrent's own command parser uses backslash escapes; XML
        // attribute escaping isn't what we need here.
        assert_eq!(xml_attr_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn decode_string_response() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<methodResponse>
<params><param><value><string>0.9.8</string></value></param></params>
</methodResponse>"#;
        let v = decode_response(xml).unwrap();
        assert_eq!(v.as_string(), Some("0.9.8"));
    }

    #[test]
    fn decode_i4_and_i8_responses() {
        let xml4 = r#"<?xml version="1.0"?>
<methodResponse>
<params><param><value><i4>0</i4></value></param></params>
</methodResponse>"#;
        let v = decode_response(xml4).unwrap();
        assert_eq!(v.as_int(), Some(0));

        let xml8 = r#"<?xml version="1.0"?>
<methodResponse>
<params><param><value><i8>123456789012</i8></value></param></params>
</methodResponse>"#;
        let v = decode_response(xml8).unwrap();
        assert_eq!(v.as_int(), Some(123456789012));
    }

    #[test]
    fn decode_nested_array_response() {
        // Shape of d.multicall2 response: array of arrays.
        let xml = r#"<?xml version="1.0"?>
<methodResponse>
<params><param><value><array><data>
<value><array><data>
<value><string>ABCD</string></value>
<value><i8>100</i8></value>
</data></array></value>
<value><array><data>
<value><string>EFGH</string></value>
<value><i8>200</i8></value>
</data></array></value>
</data></array></value></param></params>
</methodResponse>"#;
        let v = decode_response(xml).unwrap();
        let outer = v.into_array().unwrap();
        assert_eq!(outer.len(), 2);
        let row0 = outer[0].clone().into_array().unwrap();
        assert_eq!(row0[0].as_string(), Some("ABCD"));
        assert_eq!(row0[1].as_int(), Some(100));
    }

    #[test]
    fn decode_fault_surfaces_message() {
        let xml = r#"<?xml version="1.0"?>
<methodResponse>
<fault>
<value><struct>
<member><name>faultCode</name><value><i4>-503</i4></value></member>
<member><name>faultString</name><value><string>Command "foo" does not exist.</string></value></member>
</struct></value>
</fault>
</methodResponse>"#;
        let err = decode_response(xml).unwrap_err();
        assert!(err.contains("Command \"foo\" does not exist."));
    }

    #[test]
    fn decode_empty_string_implicit_and_explicit() {
        // Explicit form.
        let xml1 = r#"<?xml version="1.0"?>
<methodResponse>
<params><param><value><string></string></value></param></params>
</methodResponse>"#;
        let v = decode_response(xml1).unwrap();
        assert_eq!(v.as_string(), Some(""));
    }

    #[test]
    fn decode_implicit_string_value_without_inner_tag() {
        // XML-RPC spec allows bare `<value>text</value>` as equivalent
        // to `<value><string>text</string></value>`. rtorrent rarely
        // emits this but it's legal; guard against a future regression.
        let xml = r#"<?xml version="1.0"?>
<methodResponse>
<params><param><value>plain</value></param></params>
</methodResponse>"#;
        let v = decode_response(xml).unwrap();
        assert_eq!(v.as_string(), Some("plain"));
    }

    #[test]
    fn decode_fault_with_escaped_chars() {
        // Fault strings may contain XML-escaped characters (`&lt;`,
        // `&amp;`, `&quot;`). fault_message() unescapes them so the
        // error surfaced to the user reads naturally.
        let xml = r#"<?xml version="1.0"?>
<methodResponse>
<fault>
<value><struct>
<member><name>faultCode</name><value><i4>-503</i4></value></member>
<member><name>faultString</name><value><string>Method &quot;foo&lt;bar&gt;&amp;baz&quot; missing.</string></value></member>
</struct></value>
</fault>
</methodResponse>"#;
        let err = decode_response(xml).unwrap_err();
        assert!(
            err.contains(r#"Method "foo<bar>&baz" missing."#),
            "got: {err}"
        );
    }

    #[test]
    fn safe_delete_target_populated_base_path() {
        // Normal case: base_path points at the real content location,
        // different from the save root.
        assert_eq!(
            safe_delete_target("/downloads/sintel", "/downloads", "sintel"),
            Some("/downloads/sintel".into())
        );
    }

    #[test]
    fn safe_delete_target_refuses_save_root() {
        // base_path == directory: save-root collision. Must return None.
        assert_eq!(
            safe_delete_target("/downloads", "/downloads", "sintel"),
            None
        );
    }

    #[test]
    fn safe_delete_target_refuses_trailing_slash_divergence() {
        // base_path="/downloads/", directory="/downloads" — normalize
        // both before comparing, still collides, still refuse.
        assert_eq!(
            safe_delete_target("/downloads/", "/downloads", "sintel"),
            None
        );
        assert_eq!(
            safe_delete_target("/downloads", "/downloads/", "sintel"),
            None
        );
    }

    #[test]
    fn safe_delete_target_refuses_ancestor() {
        // base_path is an ancestor of directory — even more dangerous
        // than equality. Refuse.
        assert_eq!(
            safe_delete_target("/downloads", "/downloads/anime/airing", "sintel"),
            None
        );
    }

    #[test]
    fn safe_delete_target_refuses_root() {
        // `effective = "/"` would spawn `remove_dir_all("/")` — no.
        assert_eq!(safe_delete_target("/", "/downloads", "sintel"), None);
    }

    #[test]
    fn safe_delete_target_refuses_meta_sentinel() {
        // Pre-metadata .meta sentinel: nothing real to delete yet.
        assert_eq!(
            safe_delete_target("/downloads/incoming/ABC.meta", "/downloads", "sintel"),
            Some("/downloads/sintel".into())
        );
    }

    #[test]
    fn safe_delete_target_falls_back_to_directory_plus_name() {
        // Closed/stopped torrent: base_path is empty. Reconstruct.
        assert_eq!(
            safe_delete_target("", "/downloads", "sintel"),
            Some("/downloads/sintel".into())
        );
    }

    #[test]
    fn safe_delete_target_none_when_no_inputs() {
        assert_eq!(safe_delete_target("", "", ""), None);
        assert_eq!(safe_delete_target("", "/downloads", ""), None);
        assert_eq!(safe_delete_target("", "", "sintel"), None);
    }

    /// Live smoke test against a running rtorrent at
    /// `http://localhost:8081/RPC2` (ruTorrent LSIO default). Opt in:
    ///
    ///     RYOKAN_RTORRENT_E2E=1 cargo test rtorrent::tests::live_smoke \
    ///       -- --ignored --nocapture
    ///
    /// Exercises the trait surface Ryokan itself hits. `#[ignore]` so
    /// CI never depends on a daemon being up.
    #[tokio::test]
    #[ignore = "requires live rtorrent at localhost:8081/RPC2"]
    async fn live_smoke() {
        if std::env::var("RYOKAN_RTORRENT_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_RTORRENT_E2E=1 to run against localhost:8081)");
            return;
        }

        let client = RtorrentClient::new("http://localhost:8081/RPC2", "", "", "ryokan-e2e");

        let version = client.test().await.expect("test() failed");
        eprintln!("rtorrent version: {version}");

        let magnet = "magnet:?xt=urn:btih:7a14d93f4c13e9c1ae255e0aa3b85a9aaf0cf52d&dn=sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce";
        let info_hash = "7a14d93f4c13e9c1ae255e0aa3b85a9aaf0cf52d";

        // Ensure a clean slate in case a prior run left state.
        let _ = client.delete(info_hash, false).await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        let outcome = client
            .add_torrent(magnet, info_hash)
            .await
            .expect("add_torrent() failed");
        eprintln!("add_torrent outcome: {outcome:?}");
        assert_eq!(outcome, AddOutcome::Added);

        tokio::time::sleep(Duration::from_millis(1500)).await;
        let list = client.list_scoped().await.expect("list_scoped() failed");
        eprintln!("scoped torrents: {}", list.len());
        let found = list
            .iter()
            .find(|t| t.hash.eq_ignore_ascii_case(info_hash))
            .expect("added torrent must appear in list_scoped");
        assert_eq!(
            found.category, "ryokan-e2e",
            "custom1 label should round-trip as DownloadItem.category"
        );

        let dup = client
            .add_torrent(magnet, info_hash)
            .await
            .expect("duplicate add_torrent() failed");
        assert_eq!(dup, AddOutcome::AlreadyPresent);

        client.pause(info_hash).await.expect("pause() failed");
        tokio::time::sleep(Duration::from_millis(500)).await;
        client.resume(info_hash).await.expect("resume() failed");

        let _files = client
            .get_files(info_hash)
            .await
            .expect("get_files() failed");

        // delete(_, false) — remove from client only, leave any files.
        client
            .delete(info_hash, false)
            .await
            .expect("delete() failed");
        tokio::time::sleep(Duration::from_millis(500)).await;
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

    /// Upload a local `.torrent` to rtorrent via the XML-RPC
    /// `load.raw_start_verbose` method, which accepts the entire
    /// `.torrent` payload as a `<base64>` blob — the canonical
    /// rtorrent pattern for loading a local torrent without a
    /// filesystem mount or HTTP round-trip. Stops (pauses) the
    /// torrent immediately after load so it doesn't try to download
    /// fake content from the bogus tracker. Applies the Ryokan
    /// label via `d.custom1.set` after load.
    ///
    /// Returns the infohash rtorrent assigned (extracted from the
    /// torrent's bencode — the only way since `load.raw_start_verbose`
    /// returns 0 on success, not the hash).
    async fn upload_torrent_file_rtorrent(
        rpc_url: &str,
        label: &str,
        torrent_path: &std::path::Path,
    ) -> String {
        let bytes = std::fs::read(torrent_path).expect("read .torrent");
        let client = RtorrentClient::new(rpc_url, "", "", label);

        // Compute the infohash client-side by SHA1'ing the bencoded
        // `info` dict (shared helper in test_helpers).
        let info_hash = super::super::test_helpers::bencode_info_hash(&bytes)
            .expect("extract infohash from .torrent");
        let hash_uc = info_hash.to_ascii_uppercase();

        // `load.raw_start_verbose` auto-starts the torrent, which is
        // necessary for rtorrent to populate `d.base_path` — required
        // by `add_torrent_with_file_filter`'s metadata-readiness
        // poll (sibling smoke). Then we explicitly pause via the
        // trait's pause method so the torrent ends up in the soft-
        // paused state (`is_active=false`, `is_open=true`) that
        // `client.resume()` → `d.resume` can cleanly undo.
        //
        // Post-load commands passed to `load.raw_start_verbose` run
        // BEFORE the session-level auto-start scheduler fires, so a
        // post-command `d.pause` races and sometimes doesn't stick.
        // Pausing from the outside after a short settle is more
        // reliable.
        client
            .call(
                "load.raw_start_verbose",
                &[
                    XmlValue::String(String::new()),
                    XmlValue::Base64(bytes),
                    XmlValue::String(format!("d.custom1.set={label}")),
                ],
            )
            .await
            .expect("load.raw_start_verbose failed");

        // Wait for the torrent to register in the main view, then
        // pause it explicitly. Poll because rtorrent's post-load
        // bookkeeping is async: the hash needs to appear via
        // `d.multicall2` before `d.pause` addresses anything.
        let hash_uc_clone = hash_uc.clone();
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if client
                .call("d.pause", &[XmlValue::String(hash_uc_clone.clone())])
                .await
                .is_ok()
            {
                break;
            }
        }

        info_hash.to_ascii_lowercase()
    }

    /// Live smoke covering `add_torrent_with_file_filter` narrowing
    /// (C1) and the re-narrow preservation contract (C2) against
    /// rtorrent. Mirrors the qBit/Deluge/Transmission equivalents;
    /// differences are XML-RPC encoding and rtorrent's mandatory
    /// `d.update_priorities` call after file-priority writes.
    ///
    ///     RYOKAN_RTORRENT_E2E=1 cargo test \
    ///       rtorrent::tests::live_smoke_narrowed -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live rtorrent via ruTorrent at localhost:8081 + transmission-create"]
    async fn live_smoke_narrowed() {
        if std::env::var("RYOKAN_RTORRENT_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_RTORRENT_E2E=1 to run against localhost:8081)");
            return;
        }
        let Some((_tmp_guard, torrent_path)) = super::super::test_helpers::build_testpack_torrent()
        else {
            return;
        };
        let rpc_url = "http://localhost:8081/RPC2";
        let label = "ryokan-e2e-narrow";

        let info_hash = upload_torrent_file_rtorrent(rpc_url, label, &torrent_path).await;
        eprintln!("uploaded testpack hash={info_hash}");

        let client = RtorrentClient::new(rpc_url, "", "", label);

        // rtorrent needs a moment to register files after load — poll
        // briefly until metadata is visible via get_files.
        let mut files = Vec::new();
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            match client.get_files(&info_hash).await {
                Ok(f) if !f.is_empty() => {
                    files = f;
                    break;
                }
                _ => continue,
            }
        }
        assert_eq!(
            files.len(),
            7,
            "synthetic testpack should have 7 files, got {}",
            files.len()
        );
        assert!(
            files.iter().all(|f| f.wanted),
            "all files should start wanted=true before narrow"
        );

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
                assert_eq!(sorted_kept, sorted_expected);
            }
            SelectiveOutcome::FullDownload => panic!("C1 expected Filtered, got FullDownload"),
        }

        let files_after_c1 = client.get_files(&info_hash).await.expect("get_files C1");
        for (i, f) in files_after_c1.iter().enumerate() {
            let should_be_wanted = episode_indices.contains(&i);
            assert_eq!(
                f.wanted, should_be_wanted,
                "C1 post-narrow: [{i}] ({}) wanted={} expected={}",
                f.name, f.wanted, should_be_wanted
            );
        }
        eprintln!("C1 narrowing verified");

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

        let files_after_c2 = client.get_files(&info_hash).await.expect("get_files C2");
        for (i, f) in files_after_c2.iter().enumerate() {
            let should_be_wanted = expanded_indices.contains(&i);
            assert_eq!(
                f.wanted, should_be_wanted,
                "C2 post-renarrow: [{i}] ({}) wanted={} expected={}",
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

    /// Live smoke for B2: rtorrent `list_scoped` filters by the
    /// `custom1` field (the ruTorrent "Label" convention).
    /// A torrent with a different `custom1` value must not surface.
    ///
    ///     RYOKAN_RTORRENT_E2E=1 cargo test \
    ///       rtorrent::tests::live_smoke_scoped_exclusion -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live rtorrent via ruTorrent at localhost:8081 + transmission-create"]
    async fn live_smoke_scoped_exclusion() {
        if std::env::var("RYOKAN_RTORRENT_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_RTORRENT_E2E=1 to run against localhost:8081)");
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
        let rpc_url = "http://localhost:8081/RPC2";
        let ryokan_label = "ryokan-e2e-scope";
        let foreign_label = "other-tool-scope";

        let ryokan_hash = upload_torrent_file_rtorrent(rpc_url, ryokan_label, &torrent1).await;
        let foreign_hash = upload_torrent_file_rtorrent(rpc_url, foreign_label, &torrent2).await;
        eprintln!("ryokan={ryokan_hash} foreign={foreign_hash}");
        assert_ne!(ryokan_hash, foreign_hash);

        // rtorrent needs a brief moment to register custom1 labels
        // after load.raw_start_verbose runs the post-load commands.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let client = RtorrentClient::new(rpc_url, "", "", ryokan_label);
        let list = client.list_scoped().await.expect("list_scoped");

        assert!(
            list.iter()
                .any(|t| t.hash.eq_ignore_ascii_case(&ryokan_hash)),
            "B2: Ryokan-labeled torrent must appear"
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
        let foreign_client = RtorrentClient::new(rpc_url, "", "", foreign_label);
        foreign_client
            .delete(&foreign_hash, true)
            .await
            .expect("cleanup foreign");
        eprintln!("scoped-exclusion smoke passed");
    }

    /// Error-path live smoke (F1 / F2 / F3) against rtorrent.
    #[tokio::test]
    #[ignore = "requires live rtorrent via ruTorrent at localhost:8081"]
    async fn live_smoke_error_paths() {
        if std::env::var("RYOKAN_RTORRENT_E2E").is_err() {
            eprintln!("skipping");
            return;
        }
        let client = RtorrentClient::new("http://localhost:8081/RPC2", "", "", "ryokan-e2e-errs");
        let fake_hash = "0000000000000000000000000000000000000000";

        let result = client.delete(fake_hash, false).await;
        eprintln!("F1 rtorrent delete(non-existent) → {result:?}");

        let result = client.get_files(fake_hash).await;
        eprintln!("F2 rtorrent get_files(non-existent) → {result:?}");
        if let Ok(files) = result {
            assert!(files.is_empty(), "F2: Ok result must be empty");
        }

        let result = client
            .add_torrent("this-is-not-a-valid-url-or-magnet", fake_hash)
            .await;
        eprintln!("F3 rtorrent add(malformed) → {result:?}");
        // Fixed 2026-04-23: rtorrent's `add_torrent` now rejects
        // non-magnet / non-http(s) URLs up front. Before the fix
        // `load.start_verbose` silently returned 0 (success) on
        // garbage strings, which would have let a typo'd URL appear
        // to succeed from Ryokan's side while creating no torrent.
        // This smoke fails regression-detection if that validation
        // is removed.
        assert!(
            result.is_err(),
            "F3: add_torrent with malformed URL must return Err (got {result:?})"
        );

        eprintln!("error-paths smoke passed");
    }

    /// E1+E2 live smoke for rtorrent.
    #[tokio::test]
    #[ignore = "requires live rtorrent via ruTorrent at localhost:8081 + transmission-create"]
    async fn live_smoke_state_progress() {
        if std::env::var("RYOKAN_RTORRENT_E2E").is_err() {
            eprintln!("skipping");
            return;
        }
        let Some((_tmp, torrent_path)) = super::super::test_helpers::build_testpack_torrent()
        else {
            return;
        };
        let rpc_url = "http://localhost:8081/RPC2";
        let label = "ryokan-e2e-state";

        let info_hash = upload_torrent_file_rtorrent(rpc_url, label, &torrent_path).await;
        let client = RtorrentClient::new(rpc_url, "", "", label);

        async fn poll_until_state(
            client: &RtorrentClient,
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

        // Uploaded with d.stop post-command → expect Paused.
        let t = poll_until_state(
            &client,
            &info_hash,
            &[DownloadItemState::Paused, DownloadItemState::PausedComplete],
        )
        .await;
        eprintln!(
            "E1 rtorrent paused: state={:?} ({}) progress={}",
            t.state_kind, t.state, t.progress
        );
        assert!(matches!(
            t.state_kind,
            DownloadItemState::Paused | DownloadItemState::PausedComplete
        ));
        assert!((0.0..=1.0).contains(&t.progress));

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
            "E1 rtorrent resumed: state={:?} ({}) progress={}",
            t.state_kind, t.state, t.progress
        );
        assert!(matches!(
            t.state_kind,
            DownloadItemState::Downloading
                | DownloadItemState::DownloadingStalled
                | DownloadItemState::DownloadingQueued
                | DownloadItemState::CheckingDownload
        ));

        client.pause(&info_hash).await.expect("pause");
        let t = poll_until_state(
            &client,
            &info_hash,
            &[DownloadItemState::Paused, DownloadItemState::PausedComplete],
        )
        .await;
        eprintln!(
            "E1 rtorrent re-paused: state={:?} ({}) progress={}",
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

/// Wire-level XML-RPC trait coverage via `wiremock`. The existing
/// inline tests above cover the XML-RPC codec (encode/decode,
/// i4/i8 variants, fault extraction) thoroughly. This submodule
/// fills the gap on the trait-method side — the rtorrent quirks
/// that matter for correctness: uppercase-hash wire contract,
/// silent-0-return-on-dup pre-check via `hash_exists`, mandatory
/// `d.update_priorities` flush after `f.priority.set`, and the
/// base_path-empty fallback for closed/restarted torrents.
#[cfg(test)]
mod wiremock_tests;
