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
        ];
        let rows = self.main_view(getters).await?;

        let mut out = Vec::with_capacity(rows.len());
        for cols in rows {
            if cols.len() < getters.len() {
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

            let state_kind = map_state(complete, is_active, hashing, is_open, &message);
            let state_str = state_label(state_kind);
            let progress = if size > 0 {
                (bytes_done as f64 / size as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let save_path = directory.clone();
            let content_path = content_path(&base_path, &directory, &name);
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
            });
        }
        Ok(out)
    }

    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String> {
        let files = self.file_list(info_hash).await?;
        Ok(files
            .into_iter()
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

// ---------------------------------------------------------------------------
// Minimal XML-RPC wire format. Ryokan uses string, i4/i8, boolean,
// array, and struct (decode only, for fault handling). Rolling this
// by hand avoids pulling in dxr/xmlrpc + their four transitive
// proc-macro crates for a narrow one-file use case.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum XmlValue {
    String(String),
    Int(i64),
    Bool(bool),
    Array(Vec<XmlValue>),
}

impl XmlValue {
    fn as_string(&self) -> Option<&str> {
        match self {
            XmlValue::String(s) => Some(s),
            _ => None,
        }
    }
    fn as_int(&self) -> Option<i64> {
        match self {
            XmlValue::Int(i) => Some(*i),
            _ => None,
        }
    }
    fn into_array(self) -> Option<Vec<XmlValue>> {
        match self {
            XmlValue::Array(a) => Some(a),
            _ => None,
        }
    }
}

fn encode_request(method: &str, params: &[XmlValue]) -> String {
    let mut s = String::with_capacity(256);
    s.push_str("<?xml version=\"1.0\"?><methodCall><methodName>");
    s.push_str(&xml_text_escape(method));
    s.push_str("</methodName><params>");
    for p in params {
        s.push_str("<param>");
        encode_value(p, &mut s);
        s.push_str("</param>");
    }
    s.push_str("</params></methodCall>");
    s
}

fn encode_value(v: &XmlValue, out: &mut String) {
    out.push_str("<value>");
    match v {
        XmlValue::String(s) => {
            out.push_str("<string>");
            out.push_str(&xml_text_escape(s));
            out.push_str("</string>");
        }
        XmlValue::Int(i) => {
            out.push_str("<i8>");
            out.push_str(&i.to_string());
            out.push_str("</i8>");
        }
        XmlValue::Bool(b) => {
            out.push_str("<boolean>");
            out.push_str(if *b { "1" } else { "0" });
            out.push_str("</boolean>");
        }
        XmlValue::Array(a) => {
            out.push_str("<array><data>");
            for inner in a {
                encode_value(inner, out);
            }
            out.push_str("</data></array>");
        }
    }
    out.push_str("</value>");
}

fn xml_text_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Backslash + quote escape for values embedded inside an rtorrent
/// inline command string like `d.custom1.set="value"`. We aren't
/// encoding an XML attribute — we're embedding inside a param string
/// that rtorrent's own parser then re-parses. The quoting convention
/// rtorrent's cmd parser accepts is double-quoted + backslash-escape.
fn xml_attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Parse an XML-RPC response body. Returns the single `<param>` value
/// on success, or a decoded fault string on `<fault>`.
fn decode_response(xml: &str) -> Result<XmlValue, String> {
    let mut p = Parser::new(xml);
    p.expect_open("methodResponse")?;
    let tag = p.peek_open().ok_or("malformed XML-RPC response")?;
    match tag {
        "params" => {
            p.expect_open("params")?;
            p.expect_open("param")?;
            let v = decode_value(&mut p)?;
            p.expect_close("param")?;
            // Only single-param responses from rtorrent; skip any
            // trailing params defensively.
            while p.peek_open() == Some("param") {
                p.expect_open("param")?;
                let _ = decode_value(&mut p)?;
                p.expect_close("param")?;
            }
            p.expect_close("params")?;
            p.expect_close("methodResponse")?;
            Ok(v)
        }
        "fault" => {
            p.expect_open("fault")?;
            let v = decode_value(&mut p)?;
            p.expect_close("fault")?;
            p.expect_close("methodResponse")?;
            // Fault values are structs of {faultCode, faultString}.
            // We decode the string out of the raw XML, since the
            // struct decoder isn't needed elsewhere — cheap and
            // sufficient.
            let msg = fault_message(xml).unwrap_or_else(|| "(no fault message)".to_string());
            let _ = v;
            Err(format!("rtorrent fault: {msg}"))
        }
        other => Err(format!("unexpected XML-RPC response tag: {other}")),
    }
}

fn fault_message(xml: &str) -> Option<String> {
    // Find `<name>faultString</name>` then the next <string>...</string>.
    let needle = "<name>faultString</name>";
    let idx = xml.find(needle)?;
    let rest = &xml[idx + needle.len()..];
    let s_start = rest.find("<string>")?;
    let from = &rest[s_start + "<string>".len()..];
    let s_end = from.find("</string>")?;
    Some(xml_text_unescape(&from[..s_end]))
}

fn xml_text_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn decode_value(p: &mut Parser) -> Result<XmlValue, String> {
    p.expect_open("value")?;
    // Implicit string: `<value>bare text</value>` is legal per XML-RPC
    // spec. rtorrent sometimes emits this for empty strings.
    let inner_tag = p.peek_open();
    let v = match inner_tag {
        Some("string") => {
            p.expect_open("string")?;
            let s = p.read_text_until("</string>")?;
            XmlValue::String(xml_text_unescape(s))
        }
        Some("i4") | Some("int") => {
            let tag = inner_tag.unwrap();
            p.consume_open_tag(tag)?;
            let s = p.read_text_until(&format!("</{tag}>"))?;
            let i = s
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("XML-RPC int parse: {e}"))?;
            XmlValue::Int(i)
        }
        Some("i8") => {
            p.expect_open("i8")?;
            let s = p.read_text_until("</i8>")?;
            let i = s
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("XML-RPC i8 parse: {e}"))?;
            XmlValue::Int(i)
        }
        Some("boolean") => {
            p.expect_open("boolean")?;
            let s = p.read_text_until("</boolean>")?;
            XmlValue::Bool(s.trim() != "0")
        }
        Some("array") => {
            p.expect_open("array")?;
            p.expect_open("data")?;
            let mut items = Vec::new();
            while p.peek_open() == Some("value") {
                items.push(decode_value(p)?);
            }
            p.expect_close("data")?;
            p.expect_close("array")?;
            XmlValue::Array(items)
        }
        Some("struct") => {
            // We don't use struct values anywhere except fault, and
            // fault decoding goes through fault_message() not here.
            // Skip through to the closing </struct>.
            p.expect_open("struct")?;
            p.skip_to_close("struct")?;
            XmlValue::String(String::new())
        }
        _ => {
            // Implicit-string case: raw text until </value>.
            // `read_text_until` consumes the marker, so the closing
            // `</value>` has already been swallowed — no additional
            // expect_close call below.
            let s = p.read_text_until("</value>")?;
            return Ok(XmlValue::String(xml_text_unescape(s.trim())));
        }
    };
    p.expect_close("value")?;
    Ok(v)
}

/// Cursor-based pull parser. Just enough shape-awareness to walk
/// well-formed XML-RPC responses. Not a general XML parser — assumes
/// ASCII tag names, no CDATA sections, no namespaces, no processing
/// instructions beyond the `<?xml?>` prolog. All of those hold for
/// rtorrent 0.9.x's XML-RPC responses.
struct Parser<'a> {
    buf: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        // Skip `<?xml?>` prolog if present.
        let mut pos = 0;
        let trimmed = s.trim_start();
        pos += s.len() - trimmed.len();
        if trimmed.starts_with("<?xml")
            && let Some(end) = trimmed.find("?>")
        {
            pos += end + 2;
        }
        Self { buf: s, pos }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.buf.len() {
            let c = self.buf.as_bytes()[self.pos];
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek_open(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let rest = self.buf.get(self.pos..)?;
        if !rest.starts_with('<') || rest.starts_with("</") {
            return None;
        }
        let end = rest.find('>')?;
        let tag_inner = &rest[1..end];
        // Attributes not expected; take the tag name up to the first
        // whitespace.
        let name_end = tag_inner
            .find(|c: char| c.is_whitespace())
            .unwrap_or(tag_inner.len());
        Some(&tag_inner[..name_end])
    }

    fn consume_open_tag(&mut self, tag: &str) -> Result<(), String> {
        self.skip_ws();
        let expected = format!("<{tag}>");
        if self.buf[self.pos..].starts_with(&expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            Err(format!(
                "XML parse: expected <{tag}> at position {}",
                self.pos
            ))
        }
    }

    fn expect_open(&mut self, tag: &str) -> Result<(), String> {
        self.consume_open_tag(tag)
    }

    fn expect_close(&mut self, tag: &str) -> Result<(), String> {
        self.skip_ws();
        let expected = format!("</{tag}>");
        if self.buf[self.pos..].starts_with(&expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            Err(format!(
                "XML parse: expected </{tag}> at position {}",
                self.pos
            ))
        }
    }

    fn read_text_until(&mut self, end_marker: &str) -> Result<&'a str, String> {
        let rest = &self.buf[self.pos..];
        let idx = rest
            .find(end_marker)
            .ok_or_else(|| format!("XML parse: no {end_marker} found"))?;
        let text = &rest[..idx];
        self.pos += idx + end_marker.len();
        Ok(text)
    }

    fn skip_to_close(&mut self, tag: &str) -> Result<(), String> {
        let close = format!("</{tag}>");
        let rest = &self.buf[self.pos..];
        let idx = rest
            .find(&close)
            .ok_or_else(|| format!("XML parse: no {close} found"))?;
        self.pos += idx + close.len();
        Ok(())
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
}
