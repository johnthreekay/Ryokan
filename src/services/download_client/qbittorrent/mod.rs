//! qBittorrent implementation of [`DownloadClient`]. Speaks the
//! qBit v2 Web API (https://github.com/qbittorrent/qBittorrent/wiki/
//! WebUI-API-(qBittorrent-4.1)). Authenticates via `/auth/login`
//! with a session cookie and falls back to a re-login on 403.
//!
//! qBit-specific quirks worth flagging for future readers (most of
//! these are explained in the #63 plan at ~/Documents/ryokan-plan-
//! pluggable-download-clients.md):
//!   - `content_path` is exposed natively (≥ 2.6.1); no common-prefix
//!     computation needed like Deluge/Transmission/rtorrent.
//!   - `?category=X` is the scoping mechanism — all Ryokan-owned
//!     torrents are tagged with `config.qbit_category`.
//!   - File priority scale is 0/1/6/7. Ryokan only writes 0 (skip)
//!     and 1 (normal); the higher levels are a qBit-only feature.
//!   - qBit 5.x renamed pause/resume to stop/start. This impl tries
//!     the new names first and falls back to the old ones so 4.x
//!     and 5.x both work without a version probe.
//!   - Short-TTL coalescing cache on `list_scoped` so a burst of UI
//!     polls collapses to one upstream fetch — the cache mutex is
//!     never held across the HTTP round trip so mutation calls
//!     don't serialize behind a hung seedbox's in-flight GET.
//!   - qBit 5.x changed `POST /torrents/add` duplicate-response body
//!     from the silent `200 "Ok."` older versions returned to
//!     `200 "Fails."` — indistinguishable from the body used for a
//!     genuinely-malformed magnet. `add_torrent` disambiguates by
//!     probing `/torrents/info?hashes=<hash>` after a `Fails.` and
//!     reporting `AddOutcome::AlreadyPresent` when the hash is
//!     present in the session. See the comment on `add_torrent`.

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};

use super::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};

/// How long a successful `get_torrents` result is served from the
/// client-side cache before a fresh HTTP round trip is made. The
/// downloads page and every open series tab poll this endpoint every
/// 5s; with a remote qBit (seedbox) each one pays a full network RTT.
/// Coalescing at 2s means a burst of N concurrent polls collapses to
/// a single upstream fetch while the UI still refreshes on its own
/// 5s cadence — the user-visible staleness ceiling is 2s, not 5s.
const TORRENTS_CACHE_TTL: Duration = Duration::from_secs(2);

/// Stamped cache slot for the `/torrents/info` result. Held under a
/// `Mutex` on `QbitClient` and read/written only in short critical
/// sections — never across an await that does I/O. Aliased here so
/// the struct field and the get/invalidate call sites stay readable
/// and clippy stops flagging the nested generics.
type TorrentsCacheSlot = Option<(Instant, Vec<DownloadItem>)>;

/// qBittorrent Web API client with automatic re-authentication.
pub struct QbitClient {
    base_url: String,
    user: String,
    pass: String,
    category: String,
    http: Client,
    logged_in: Arc<Mutex<bool>>,
    torrents_cache: Arc<Mutex<TorrentsCacheSlot>>,
    /// Single-flight election flag for the `/torrents/info` fetch.
    /// An `AtomicBool` rather than `Mutex<bool>` specifically so the
    /// RAII `FetchFlightGuard` below can clear it from `Drop` — which
    /// is sync and can't `.await` a `tokio::sync::Mutex`. The guard
    /// ensures a panic inside `list_scoped_uncached` can't wedge the
    /// flag at `true` forever, silently turning every subsequent
    /// caller into a waiter stuck on `notify_waiters()` that will
    /// never fire.
    torrents_fetch_in_flight: Arc<AtomicBool>,
    torrents_fetch_done: Arc<Notify>,
}

/// RAII guard that clears the in-flight flag and wakes waiters on
/// drop — on both the happy and panic paths. Constructed only on the
/// fetcher branch of `list_scoped`, immediately after the
/// compare-and-swap that claimed leadership.
struct FetchFlightGuard<'a> {
    flag: &'a AtomicBool,
    notify: &'a Notify,
}

impl Drop for FetchFlightGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// Raw torrent shape qBit returns from `/torrents/info`. Deserialized
/// into `Vec<QbitRawTorrent>`, then each is converted to
/// [`DownloadItem`] (mapping state strings to the normalized enum).
#[derive(Debug, Deserialize)]
struct QbitRawTorrent {
    hash: String,
    name: String,
    size: i64,
    progress: f64,
    dlspeed: i64,
    state: String,
    category: String,
    eta: i64,
    #[serde(default)]
    save_path: String,
    #[serde(default)]
    content_path: String,
    /// Share-limit progress (issue #228). `max_ratio`,
    /// `max_seeding_time` (minutes) and `max_inactive_seeding_time`
    /// (minutes) are the *effective* limits: the per-torrent override
    /// when one is set, else the global one, and `-1` when there is no
    /// limit. `seeding_time` is seconds; `last_activity` a unix time.
    /// Builds that predate a field (inactive limits arrived in 4.6)
    /// read as "no limit".
    #[serde(default)]
    ratio: f64,
    #[serde(default = "no_limit_f64")]
    max_ratio: f64,
    #[serde(default)]
    seeding_time: i64,
    #[serde(default = "no_limit_i64")]
    max_seeding_time: i64,
    #[serde(default = "no_limit_i64")]
    max_inactive_seeding_time: i64,
    #[serde(default)]
    last_activity: i64,
}

fn no_limit_f64() -> f64 {
    -1.0
}

fn no_limit_i64() -> i64 {
    -1
}

/// Raw per-file shape qBit returns from `/torrents/files`. Converted
/// to [`DownloadFile`] on read (qBit `priority == 0` → `wanted=false`).
#[derive(Debug, Deserialize)]
struct QbitRawFile {
    name: String,
    size: i64,
    progress: f64,
    /// qBit file priority: 0 = skip, 1 = normal, 6 = high, 7 = max.
    /// Defaulted to `1` (normal) on the off chance qBit omits the
    /// field — safer than defaulting to 0 "skip", which our
    /// additive-merge logic interprets as "this torrent has been
    /// narrowed before".
    #[serde(default = "default_file_priority")]
    priority: i32,
}

fn default_file_priority() -> i32 {
    1
}

impl QbitClient {
    pub fn new(base_url: &str, user: &str, pass: &str, category: &str) -> Self {
        let http = Client::builder()
            .cookie_store(true)
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: normalize_base_url(base_url),
            user: user.to_string(),
            pass: pass.to_string(),
            category: category.to_string(),
            http,
            logged_in: Arc::new(Mutex::new(false)),
            torrents_cache: Arc::new(Mutex::new(None)),
            torrents_fetch_in_flight: Arc::new(AtomicBool::new(false)),
            torrents_fetch_done: Arc::new(Notify::new()),
        }
    }

    async fn invalidate_torrents_cache(&self) {
        *self.torrents_cache.lock().await = None;
    }

    async fn login(&self) -> Result<(), String> {
        let resp = self
            .http
            .post(format!("{}/api/v2/auth/login", self.base_url))
            .form(&[("username", &self.user), ("password", &self.pass)])
            .send()
            .await
            .map_err(|e| format!("qbit login failed: {}", e))?;

        let body = resp.text().await.unwrap_or_default();
        if body == "Fails." {
            return Err("qbit auth failed: invalid credentials".into());
        }

        *self.logged_in.lock().await = true;
        Ok(())
    }

    async fn ensure_login(&self) -> Result<(), String> {
        let logged_in = *self.logged_in.lock().await;
        if !logged_in {
            self.login().await?;
        }
        Ok(())
    }

    async fn do_get(&self, endpoint: &str) -> Result<reqwest::Response, String> {
        self.ensure_login().await?;

        let url = format!("{}{}", self.base_url, endpoint);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if resp.status() == StatusCode::FORBIDDEN {
            *self.logged_in.lock().await = false;
            self.login().await?;
            self.http
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("retry failed: {}", e))
        } else {
            Ok(resp)
        }
    }

    async fn do_post_form(
        &self,
        endpoint: &str,
        form: &[(&str, &str)],
    ) -> Result<reqwest::Response, String> {
        self.ensure_login().await?;

        let url = format!("{}{}", self.base_url, endpoint);
        let resp = self
            .http
            .post(&url)
            .form(form)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if resp.status() == StatusCode::FORBIDDEN {
            *self.logged_in.lock().await = false;
            self.login().await?;
            self.http
                .post(&url)
                .form(form)
                .send()
                .await
                .map_err(|e| format!("retry failed: {}", e))
        } else {
            Ok(resp)
        }
    }

    /// Set per-file priority (qBit's native 0/1/6/7 scale). Ryokan
    /// only uses 0 and 1 internally; higher levels are untouched.
    async fn set_file_priority(
        &self,
        hash: &str,
        file_ids: &[usize],
        priority: i32,
    ) -> Result<(), String> {
        if file_ids.is_empty() {
            return Ok(());
        }
        let id_str = file_ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("|");
        let prio_str = priority.to_string();
        let form = [
            ("hash", hash),
            ("id", id_str.as_str()),
            ("priority", prio_str.as_str()),
        ];
        let resp = self
            .do_post_form("/api/v2/torrents/filePrio", &form)
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("qbit filePrio failed: {} {}", status, body.trim()));
        }
        Ok(())
    }

    /// Wait for qBit to finish pulling metadata for a magnet. Returns
    /// the file list once available. Used internally by
    /// `add_torrent_with_file_filter` for the narrow-after-metadata
    /// flow; external callers use the free [`super::wait_for_files`].
    async fn wait_for_metadata(
        &self,
        hash: &str,
        timeout: Duration,
    ) -> Result<Vec<QbitRawFile>, String> {
        let start = Instant::now();
        let mut delay = Duration::from_millis(500);
        loop {
            match self.get_torrent_files_raw(hash).await {
                Ok(files) if !files.is_empty() => return Ok(files),
                Ok(_) => {}
                Err(e) => {
                    if start.elapsed() >= timeout {
                        return Err(format!(
                            "qbit metadata fetch error after {:?}: {}",
                            timeout, e
                        ));
                    }
                }
            }
            if start.elapsed() >= timeout {
                return Err(format!("qbit metadata fetch timed out after {:?}", timeout));
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    /// Raw `/torrents/files?hash=X` fetch. qBit 5.x returns **404
    /// with `"Not Found"` body** for this endpoint while the torrent
    /// is still fetching metadata — not an empty JSON array. We
    /// translate 404 → `Ok(vec![])` so the wait loop's "empty →
    /// retry" arm drives the poll correctly.
    async fn get_torrent_files_raw(&self, hash: &str) -> Result<Vec<QbitRawFile>, String> {
        let endpoint = format!("/api/v2/torrents/files?hash={}", hash);
        let resp = self.do_get(&endpoint).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "qbit torrent files fetch failed: {} {}",
                status,
                body.trim()
            ));
        }
        let files: Vec<QbitRawFile> = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse torrent files: {}", e))?;
        Ok(files)
    }

    async fn list_scoped_uncached(&self) -> Result<Vec<DownloadItem>, String> {
        let endpoint = if self.category.is_empty() {
            "/api/v2/torrents/info".to_string()
        } else {
            format!("/api/v2/torrents/info?category={}", self.category)
        };

        let resp = self.do_get(&endpoint).await?;
        let raw: Vec<QbitRawTorrent> = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse torrents: {}", e))?;

        Ok(raw.into_iter().map(to_download_item).collect())
    }
}

#[async_trait]
impl DownloadClient for QbitClient {
    async fn test(&self) -> Result<String, String> {
        let resp = self.do_get("/api/v2/app/version").await?;
        let version = resp.text().await.unwrap_or_default();
        Ok(version)
    }

    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        let form = [("urls", url), ("category", &self.category)];
        let resp = self.do_post_form("/api/v2/torrents/add", &form).await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(format!("qbit add failed (HTTP {status}): {body}"));
        }
        // qBit returns 200 OK with body "Fails." in two different
        // scenarios that are indistinguishable from the response
        // alone: a genuinely-malformed magnet (bad hash length, bad
        // URN scheme) and — the case live-verified against v5.1.4
        // during the Phase 1 download-client smoke work — a *duplicate*
        // magnet whose info-hash is already in the session. Older
        // builds are silent about duplicates (the "always 200 Ok."
        // behavior the module-header comment describes); v5.x changed
        // that, and the auto-search path lit up with
        // `qBit returned 'Fails.'` errors on every re-grab of a
        // torrent already in the client (e.g. RSS re-emitting an item
        // whose grab row was lost on a crash, or a manual grab +
        // upgrade-sweep hitting the same release).
        //
        // We can disambiguate after the fact: look the info-hash up
        // via `/torrents/info?hashes=`. Present → duplicate, report
        // `AlreadyPresent` and let the caller record the grab
        // normally. Missing → the magnet really was rejected; surface
        // the error so auto-search backs off.
        if body.trim() == "Fails." {
            if !info_hash.is_empty() {
                let hash_lc = info_hash.to_ascii_lowercase();
                let lookup = format!("/api/v2/torrents/info?hashes={hash_lc}");
                if let Ok(resp) = self.do_get(&lookup).await
                    && resp.status().is_success()
                    && let Ok(raw) = resp.json::<Vec<QbitRawTorrent>>().await
                    && raw.iter().any(|t| t.hash.eq_ignore_ascii_case(&hash_lc))
                {
                    return Ok(AddOutcome::AlreadyPresent);
                }
            }
            return Err(format!(
                "qbit add rejected url={url}: qBit returned 'Fails.'"
            ));
        }
        self.invalidate_torrents_cache().await;
        // On the 200 "Ok." path qBit returns the same body whether the
        // hash was new or (on older builds) a silent duplicate. We
        // report `Added` either way; the `AlreadyPresent` path only
        // fires for the v5.x "Fails." duplicate disambiguation above.
        Ok(AddOutcome::Added)
    }

    /// Add a torrent in a state where no file data downloads, for the
    /// interactive file picker (#83).
    ///
    /// The trait contract is "paused," but qBit 5.x can't deliver that
    /// directly: a stopped torrent doesn't publish its file list
    /// through `/torrents/files`, so the picker would never have
    /// files to render. Instead we race — add running, wait for
    /// metadata (same 10s budget as `add_torrent_with_file_filter`),
    /// then set **every** file to priority 0 before returning. From
    /// the caller's perspective the post-condition holds: files are
    /// visible and nothing is downloading peer data.
    ///
    /// Confirm-time flow: caller flips the user's wanted files back
    /// to priority 1 via `set_file_wanted(indices, wanted=true)`. The
    /// torrent is already running so no explicit `resume` is needed —
    /// calling `resume` is still harmless, and matches the uniform
    /// cross-client contract the handler uses (see Deluge /
    /// Transmission / rTorrent impls which DO need the resume).
    ///
    /// Metadata-fetch failure: if the 10s budget elapses, the
    /// torrent is left running with every file at default priority
    /// (all-wanted). The caller can surface this as "metadata
    /// timeout, grab with defaults" — matching the plan doc's
    /// decision #1 two-button dialog.
    async fn add_torrent_paused(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        if info_hash.is_empty() {
            return Err("qBit paused add requires a pre-computed info hash".into());
        }
        let hash_lc = info_hash.to_ascii_lowercase();

        let outcome = self.add_torrent(url, &hash_lc).await?;

        // Don't touch a pre-existing torrent's priorities. The user
        // may have partial-downloaded this release from an earlier
        // grab with careful file-selection; mark-all-skip would wipe
        // that and force them to re-pick. Handler is responsible for
        // surfacing the existing state to the modal instead (same-
        // hash dedup flow, plan decision #6).
        if outcome == AddOutcome::AlreadyPresent {
            return Ok(outcome);
        }

        // Explicit resume so a fresh add that landed in stopped state
        // starts flowing metadata. Matches the pattern in
        // `add_torrent_with_file_filter`.
        let _ = self.resume(&hash_lc).await;

        match self
            .wait_for_metadata(&hash_lc, Duration::from_secs(10))
            .await
        {
            Ok(files) => {
                // Mark every file skipped so the torrent idles until
                // the caller flips selections back via set_file_wanted.
                let all_ids: Vec<usize> = (0..files.len()).collect();
                if !all_ids.is_empty() {
                    self.set_file_priority(&hash_lc, &all_ids, 0).await?;
                }
            }
            Err(_) => {
                // Metadata timeout — leave the torrent running at
                // default priorities. Handler will see an empty /
                // not-yet-populated file list on the GET preview
                // endpoint and surface the timeout dialog to the
                // user. No data loss, just a UX prompt.
            }
        }

        Ok(outcome)
    }

    /// Add a torrent, wait for metadata, invoke `pick`, and mark the
    /// rest as skip.
    ///
    /// Notably: we do **not** add paused. qBit 5.x renamed pause/resume
    /// to stop/start and — more importantly — a torrent added in
    /// stopped state doesn't publish its file list through
    /// `/torrents/files`, so the old "add paused → wait metadata →
    /// set priorities → resume" flow hangs forever on 5.x. Instead
    /// we add unpaused and race `filePrio` against qBit's
    /// peer-discovery startup; for a `.torrent` URL qBit parses the
    /// file list within a couple seconds — well before real data
    /// transfer — so the window where unwanted pieces might be
    /// requested is small and bounded.
    ///
    /// An explicit `resume` call after the add handles the dedup case
    /// where qBit already has the same info hash sitting in stopped
    /// state from an earlier failed grab.
    ///
    /// **Additive merge**: when a second selective grab lands on a
    /// torrent that's already been narrowed, we only bump the *new*
    /// wanted files from skip → normal. Files already at normal/high
    /// stay untouched so previous grabs on this megapack keep
    /// downloading. Detection via per-file priority readback: any
    /// file at priority 0 means the torrent was narrowed before.
    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        info_hash: &str,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        if info_hash.is_empty() {
            return Err("selective download requires a known info hash".into());
        }
        let hash_lc = info_hash.to_ascii_lowercase();

        self.add_torrent(url, &hash_lc).await?;

        // If qBit already had this hash sitting in stopped state,
        // the add above is a dedup no-op and the torrent is still
        // stopped. Explicitly start it so metadata flows.
        self.resume(&hash_lc).await?;

        let files = match self
            .wait_for_metadata(&hash_lc, Duration::from_secs(10))
            .await
        {
            Ok(files) => files,
            Err(_) => return Ok(SelectiveOutcome::FullDownload),
        };

        let names: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
        let keep = pick(&names);

        let new_keep_ids = match keep {
            Some(ids) if !ids.is_empty() && ids.len() < files.len() => ids,
            _ => return Ok(SelectiveOutcome::FullDownload),
        };

        let already_narrowed = files.iter().any(|f| f.priority == 0);

        if already_narrowed {
            let to_bump: Vec<usize> = new_keep_ids
                .iter()
                .copied()
                .filter(|&i| files.get(i).map(|f| f.priority == 0).unwrap_or(false))
                .collect();
            if !to_bump.is_empty() {
                self.set_file_priority(&hash_lc, &to_bump, 1).await?;
            }
        } else {
            let skip_ids: Vec<usize> = (0..files.len())
                .filter(|i| !new_keep_ids.contains(i))
                .collect();
            self.set_file_priority(&hash_lc, &skip_ids, 0).await?;
            self.set_file_priority(&hash_lc, &new_keep_ids, 1).await?;
        }
        Ok(SelectiveOutcome::Filtered(new_keep_ids))
    }

    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        {
            let guard = self.torrents_cache.lock().await;
            if let Some((stamped, torrents)) = guard.as_ref()
                && stamped.elapsed() < TORRENTS_CACHE_TTL
            {
                return Ok(torrents.clone());
            }
        }

        // Register for the wake-up BEFORE we try to become the fetcher
        // so a fast fetcher can't complete and wake between our CAS
        // attempt and our await — that would make us miss the wake-up
        // and hang until the next mutation.
        let notified = self.torrents_fetch_done.notified();
        tokio::pin!(notified);

        // Atomic compare-and-swap elects exactly one fetcher per burst.
        let is_fetcher = self
            .torrents_fetch_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();

        if !is_fetcher {
            notified.as_mut().await;
            {
                let guard = self.torrents_cache.lock().await;
                if let Some((stamped, torrents)) = guard.as_ref()
                    && stamped.elapsed() < TORRENTS_CACHE_TTL
                {
                    return Ok(torrents.clone());
                }
            }
            // Leader's fetch errored (cache still empty). Fall through
            // to an uncoordinated direct fetch so this waiter doesn't
            // return stale or empty data.
            return self.list_scoped_uncached().await;
        }

        // Fetcher path. `_flight_guard` clears the in-flight flag and
        // wakes waiters in its `Drop`, so a panic inside
        // `list_scoped_uncached` can't wedge the flag at `true` and
        // leave every subsequent caller stuck awaiting a notify that
        // never fires. Without this, a single panic inside
        // JSON-parsing would silently brick the downloads queue for
        // the life of the process.
        let _flight_guard = FetchFlightGuard {
            flag: &self.torrents_fetch_in_flight,
            notify: &self.torrents_fetch_done,
        };

        let result = self.list_scoped_uncached().await;
        if let Ok(ref torrents) = result {
            let mut guard = self.torrents_cache.lock().await;
            *guard = Some((Instant::now(), torrents.clone()));
        }
        result
        // _flight_guard drops here: clears flag + notifies waiters.
    }

    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String> {
        let raw = self.get_torrent_files_raw(info_hash).await?;
        Ok(raw
            .into_iter()
            .map(|f| DownloadFile {
                name: f.name,
                size: f.size,
                progress: f.progress,
                wanted: f.priority != 0,
            })
            .collect())
    }

    /// qBit 5.x renamed `/torrents/pause` to `/torrents/stop`. Try
    /// the new name first and fall back to the old one so both
    /// generations work without a version probe.
    async fn pause(&self, info_hash: &str) -> Result<(), String> {
        let form = [("hashes", info_hash)];
        let resp = self.do_post_form("/api/v2/torrents/stop", &form).await?;
        if resp.status().is_success() {
            self.invalidate_torrents_cache().await;
            return Ok(());
        }
        let resp = self.do_post_form("/api/v2/torrents/pause", &form).await?;
        if resp.status().is_success() {
            self.invalidate_torrents_cache().await;
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("qbit pause failed: {} {}", status, body.trim()))
    }

    /// qBit 5.x renamed `/torrents/resume` to `/torrents/start`.
    /// Same dual-path strategy as pause.
    async fn resume(&self, info_hash: &str) -> Result<(), String> {
        let form = [("hashes", info_hash)];
        let resp = self.do_post_form("/api/v2/torrents/start", &form).await?;
        if resp.status().is_success() {
            self.invalidate_torrents_cache().await;
            return Ok(());
        }
        let resp = self.do_post_form("/api/v2/torrents/resume", &form).await?;
        if resp.status().is_success() {
            self.invalidate_torrents_cache().await;
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("qbit resume failed: {} {}", status, body.trim()))
    }

    async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), String> {
        let delete_str = if delete_files { "true" } else { "false" };
        let form = [("hashes", info_hash), ("deleteFiles", delete_str)];
        let resp = self.do_post_form("/api/v2/torrents/delete", &form).await?;
        if !resp.status().is_success() {
            return Err("Failed to delete torrent".into());
        }
        self.invalidate_torrents_cache().await;
        Ok(())
    }

    async fn set_file_wanted(
        &self,
        info_hash: &str,
        files: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        // qBit priority 0 = skip, 1 = normal. The higher levels
        // (6=high, 7=max) are intentionally unused by Ryokan.
        let priority = if wanted { 1 } else { 0 };
        self.set_file_priority(info_hash, files, priority).await
    }

    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }

    /// Issue #28 — qBit's per-torrent share-limit endpoint.
    ///
    /// Wire shape: `POST /api/v2/torrents/setShareLimits` with
    /// form fields `hashes`, `ratioLimit`, `seedingTimeLimit`, and
    /// `inactiveSeedingTimeLimit`. Per the qBit WebUI API docs:
    /// - `-2` = use the global limit.
    /// - `-1` = no limit for this torrent.
    /// - any other value = the per-torrent override.
    ///
    /// Ryokan's [`SeedRules`] uses `Option<f64>` / `Option<u64>`; a
    /// `None` field translates to `-2` so the torrent keeps the
    /// user's global rule for that dimension (the `respect_seed_rules`
    /// flag tracks whether ANY rule is in effect, so a None field
    /// doesn't mean "no rule" at the model layer). Before #228 this
    /// sent `-1`, which switched every unset dimension to "unlimited"
    /// and disabled the global seeding-time and inactivity limits on
    /// each grab from a ratio-only indexer.
    /// `inactiveSeedingTimeLimit` isn't in the trait; `-2` keeps the
    /// global inactivity limit.
    async fn set_seed_rules(&self, info_hash: &str, rules: super::SeedRules) -> Result<(), String> {
        let ratio = rules
            .ratio
            .map(|r| r.to_string())
            .unwrap_or_else(|| "-2".to_string());
        let seeding_time = rules
            .time_minutes
            .map(|m| m.to_string())
            .unwrap_or_else(|| "-2".to_string());
        let form = [
            ("hashes", info_hash),
            ("ratioLimit", ratio.as_str()),
            ("seedingTimeLimit", seeding_time.as_str()),
            ("inactiveSeedingTimeLimit", "-2"),
        ];
        let resp = self
            .do_post_form("/api/v2/torrents/setShareLimits", &form)
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "qbit setShareLimits failed: {} {}",
                status,
                body.trim()
            ));
        }
        Ok(())
    }
}

fn to_download_item(raw: QbitRawTorrent) -> DownloadItem {
    let state_kind = map_qbit_state(&raw.state);
    let seeding_done = qbit_seeding_done(&raw, chrono::Utc::now().timestamp());
    DownloadItem {
        hash: raw.hash,
        name: raw.name,
        size: raw.size,
        progress: raw.progress,
        dlspeed: raw.dlspeed,
        state: raw.state,
        category: raw.category,
        eta: raw.eta,
        save_path: raw.save_path,
        content_path: raw.content_path,
        state_kind,
        seeding_done,
    }
}

/// qBit stops a torrent itself when a share limit is reached (its
/// "pause" / "stop" action; the "remove" actions make it vanish). A
/// stopped-complete torrent whose ratio, seeding time, or inactivity
/// has reached the effective limit is therefore done seeding; a
/// stopped-complete torrent below every limit was paused by hand and
/// is left alone. Mirrors Sonarr's `HasReachedSeedLimit`.
fn qbit_seeding_done(raw: &QbitRawTorrent, now_unix: i64) -> bool {
    if !matches!(raw.state.as_str(), "pausedUP" | "stoppedUP") {
        return false;
    }
    let ratio_met = raw.max_ratio >= 0.0 && raw.ratio >= raw.max_ratio;
    let time_met = raw.max_seeding_time >= 0 && raw.seeding_time >= raw.max_seeding_time * 60;
    // The inactivity branch is the one place user intent and client
    // intent blur: a torrent paused by hand that then sits idle past a
    // global inactivity limit reads as done. qBit would normally have
    // stopped it itself first, so the window is small, and the library
    // keeps its own copy either way.
    let inactive_met = raw.max_inactive_seeding_time >= 0
        && raw.last_activity > 0
        && now_unix - raw.last_activity >= raw.max_inactive_seeding_time * 60;
    ratio_met || time_met || inactive_met
}

/// Map qBit's native state strings to the normalized
/// [`DownloadItemState`] enum. qBit's state machine covers 10+
/// distinct values; any unknown string falls back to `Downloading`
/// (generic in-progress) rather than `Errored` so a future qBit
/// version introducing a new state label doesn't flip active
/// torrents red in the UI until we update this table.
fn map_qbit_state(state: &str) -> DownloadItemState {
    use DownloadItemState::*;
    match state {
        "downloading" | "forcedDL" => Downloading,
        "stalledDL" => DownloadingStalled,
        "queuedDL" => DownloadingQueued,
        "checkingDL" => CheckingDownload,
        "uploading" | "forcedUP" => Seeding,
        "stalledUP" => SeedingStalled,
        "queuedUP" => SeedingQueued,
        "checkingUP" => CheckingSeed,
        "pausedDL" | "stoppedDL" => Paused,
        "pausedUP" | "stoppedUP" => PausedComplete,
        "error" | "missingFiles" => Errored,
        // `metaDL`, `allocating`, `moving`, `unknown`, and any
        // future state label all fall back to `Downloading` (generic
        // in-progress) rather than `Errored` so a new qBit version
        // with an unseen state doesn't flip active torrents red.
        _ => Downloading,
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

    let lower = trimmed.to_ascii_lowercase();
    // RFC 1918 + loopback + hostname "localhost". The 172.16.0.0/12
    // block covers 172.16–172.31; each second octet is listed
    // explicitly (not as a `172.2` prefix) because `172.2`.starts_with
    // also matches `172.2.x.x` (public) and `172.200-172.255.x.x`.
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

    #[test]
    fn normalize_base_url_preserves_scheme() {
        assert_eq!(normalize_base_url("http://foo:8080"), "http://foo:8080");
        assert_eq!(normalize_base_url("https://foo"), "https://foo");
    }

    #[test]
    fn normalize_base_url_adds_http_for_local() {
        assert_eq!(
            normalize_base_url("localhost:8080"),
            "http://localhost:8080"
        );
        assert_eq!(
            normalize_base_url("127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_base_url("192.168.1.2:8080"),
            "http://192.168.1.2:8080"
        );
    }

    #[test]
    fn normalize_base_url_adds_https_for_remote() {
        assert_eq!(
            normalize_base_url("seedbox.example.com"),
            "https://seedbox.example.com"
        );
    }

    #[test]
    fn normalize_base_url_rfc1918_full_172_range() {
        // Every octet in 172.16–172.31 should be treated as local. The
        // old `starts_with("172.2")` check caught 172.20–172.29 but
        // also (incorrectly) 172.2.x.x — a public IP.
        for octet in 16..=31 {
            let host = format!("172.{octet}.0.1:8080");
            assert_eq!(
                normalize_base_url(&host),
                format!("http://{host}"),
                "172.{octet}.x.x should be treated as local"
            );
        }
    }

    #[test]
    fn normalize_base_url_public_172_2_is_not_local() {
        // 172.2.x.x is public IP space; must get https, not http.
        assert_eq!(
            normalize_base_url("172.2.3.4:8080"),
            "https://172.2.3.4:8080"
        );
    }

    #[test]
    fn normalize_base_url_trims_trailing_slash() {
        assert_eq!(normalize_base_url("http://foo:8080/"), "http://foo:8080");
    }

    #[test]
    fn qbit_state_maps_all_seeding_variants_to_complete() {
        assert!(map_qbit_state("uploading").is_complete());
        assert!(map_qbit_state("stalledUP").is_complete());
        assert!(map_qbit_state("queuedUP").is_complete());
        assert!(map_qbit_state("forcedUP").is_complete());
        assert!(map_qbit_state("checkingUP").is_complete());
        assert!(map_qbit_state("pausedUP").is_complete());
        assert!(map_qbit_state("stoppedUP").is_complete());
    }

    #[test]
    fn qbit_state_maps_download_variants_to_incomplete() {
        assert!(!map_qbit_state("downloading").is_complete());
        assert!(!map_qbit_state("stalledDL").is_complete());
        assert!(!map_qbit_state("queuedDL").is_complete());
        assert!(!map_qbit_state("checkingDL").is_complete());
        assert!(!map_qbit_state("pausedDL").is_complete());
    }

    #[test]
    fn qbit_state_maps_error_states() {
        assert!(map_qbit_state("error").is_errored());
        assert!(map_qbit_state("missingFiles").is_errored());
    }

    #[test]
    fn qbit_state_unknown_falls_back_to_downloading() {
        assert_eq!(
            map_qbit_state("someFutureState"),
            DownloadItemState::Downloading
        );
    }

    /// Live smoke test against a running qBittorrent at
    /// `http://localhost:8080` (lscr.io/linuxserver/qbittorrent
    /// defaults — user `admin`, password the one printed on first
    /// startup to the container log). Opt in:
    ///
    ///     RYOKAN_QBIT_E2E=1 QBIT_PASS=<pw> cargo test \
    ///       qbittorrent::tests::live_smoke -- --ignored --nocapture
    ///
    /// Exercises the full surface Ryokan itself hits: test →
    /// add_torrent → list_scoped (category round-trip) →
    /// duplicate-add → pause/resume → get_files → delete. Gated
    /// behind `#[ignore]` + env var so CI never depends on a daemon
    /// being up. Mirrors the pattern in the other three client impls.
    ///
    /// Unlike the Deluge/Transmission/rtorrent smokes, qBit's
    /// password is generated at container first-boot (linuxserver
    /// image pattern), so we take it via a second env var rather
    /// than hardcoding a default.
    #[tokio::test]
    #[ignore = "requires live qBittorrent at localhost:8080"]
    async fn live_smoke() {
        if std::env::var("RYOKAN_QBIT_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_QBIT_E2E=1 to run against localhost:8080)");
            return;
        }
        let pass = std::env::var("QBIT_PASS").unwrap_or_else(|_| "adminadmin".to_string());

        let client = QbitClient::new("http://localhost:8080", "admin", &pass, "ryokan-e2e");

        let version = client.test().await.expect("test() failed");
        eprintln!("qBittorrent version: {version}");

        let magnet = "magnet:?xt=urn:btih:7a14d93f4c13e9c1ae255e0aa3b85a9aaf0cf52d&dn=sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce";
        let info_hash = "7a14d93f4c13e9c1ae255e0aa3b85a9aaf0cf52d";

        // Clean slate.
        let _ = client.delete(info_hash, false).await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        let outcome = client
            .add_torrent(magnet, info_hash)
            .await
            .expect("add_torrent() failed");
        eprintln!("add_torrent outcome: {outcome:?}");
        // qBit's add API doesn't surface duplicate-add to the caller;
        // a re-add returns Added silently. Accept either outcome here
        // so the smoke survives a prior-run leftover the cleanup
        // didn't catch.
        assert!(matches!(
            outcome,
            AddOutcome::Added | AddOutcome::AlreadyPresent
        ));

        tokio::time::sleep(Duration::from_millis(1500)).await;
        let list = client.list_scoped().await.expect("list_scoped() failed");
        eprintln!("scoped torrents: {}", list.len());
        let found = list
            .iter()
            .find(|t| t.hash.eq_ignore_ascii_case(info_hash))
            .expect("added torrent must appear in list_scoped");
        assert_eq!(
            found.category, "ryokan-e2e",
            "qBit category should round-trip as DownloadItem.category"
        );

        // Duplicate-add contract: qBit 5.x responds with 200 "Fails."
        // when asked to add a hash already in the session, which
        // `add_torrent` now disambiguates via `/torrents/info?hashes=`
        // and surfaces as `AddOutcome::AlreadyPresent`. Older builds
        // silently returned 200 "Ok." and `AddOutcome::Added`. Both
        // must succeed — a bare `Err` here would mean the v5.x
        // disambiguation regressed.
        let dup_outcome = client
            .add_torrent(magnet, info_hash)
            .await
            .expect("duplicate add_torrent() should succeed");
        assert!(matches!(
            dup_outcome,
            AddOutcome::Added | AddOutcome::AlreadyPresent
        ));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let still_there = client
            .list_scoped()
            .await
            .expect("list_scoped() after re-add failed")
            .iter()
            .any(|t| t.hash.eq_ignore_ascii_case(info_hash));
        assert!(
            still_there,
            "torrent must still be present after duplicate-add attempt"
        );

        client.pause(info_hash).await.expect("pause() failed");
        tokio::time::sleep(Duration::from_millis(500)).await;
        client.resume(info_hash).await.expect("resume() failed");

        let _files = client
            .get_files(info_hash)
            .await
            .expect("get_files() failed");

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

    /// Live smoke covering `add_torrent_with_file_filter` narrowing
    /// (C1) and the re-narrow preservation contract (C2). Uses a
    /// synthetic multi-file `.torrent` (built via
    /// `transmission-create`) uploaded directly via qBit's multipart
    /// endpoint so metadata is present immediately — no DHT
    /// dependency, no flakiness window.
    ///
    /// Contract verified:
    /// * C1 — narrowing a full file list down to a proper subset
    ///   returns `SelectiveOutcome::Filtered(kept_indices)` and
    ///   writes `wanted=false` to every non-selected file.
    /// * C2 — re-narrowing with an expanded pick bumps previously-
    ///   skipped files that are now wanted, but does *not* re-skip
    ///   files the expanded pick dropped. Matches the `already_narrowed`
    ///   branch in `add_torrent_with_file_filter` whose contract says
    ///   "re-narrow must not clobber user edits."
    ///
    /// Gated the same way as `live_smoke`:
    ///
    ///     RYOKAN_QBIT_E2E=1 QBIT_PASS=<pw> cargo test \
    ///       qbittorrent::tests::live_smoke_narrowed -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live qBittorrent at localhost:8080 + transmission-create"]
    async fn live_smoke_narrowed() {
        if std::env::var("RYOKAN_QBIT_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_QBIT_E2E=1 to run against localhost:8080)");
            return;
        }
        let Some((_tmp_guard, torrent_path)) = super::super::test_helpers::build_testpack_torrent()
        else {
            return;
        };
        let pass = std::env::var("QBIT_PASS").unwrap_or_else(|_| "adminadmin".to_string());
        let base_url = "http://localhost:8080";
        let category = "ryokan-e2e-narrow";

        let info_hash = super::super::test_helpers::upload_torrent_file_qbit(
            base_url,
            "admin",
            &pass,
            category,
            &torrent_path,
        )
        .await;
        eprintln!("uploaded testpack hash={info_hash}");

        let client = QbitClient::new(base_url, "admin", &pass, category);

        // Cleanup-on-panic safety: if any assertion fails below, we want
        // the torrent gone so the next test run starts clean. Accomplish
        // via a drop-guard closure pattern — but since we can't early-
        // return from a test with cleanup, just ensure the final delete
        // runs and previous state is cleared at entry too.
        let files = client
            .get_files(&info_hash)
            .await
            .expect("get_files should return metadata immediately after file upload");
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

        // --- C1: narrow to episode files only ---
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

        // Verify priorities actually applied to the client state.
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
        eprintln!("C1 narrowing verified (5 episodes wanted, sample+readme skipped)");

        // --- C2: re-narrow with expanded pick (add sample.mkv) ---
        let expanded_indices: Vec<usize> = files_after_c1
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                (f.name.contains("episode_") || f.name.contains("sample")).then_some(i)
            })
            .collect();
        assert_eq!(
            expanded_indices.len(),
            6,
            "expected 6 files in expanded pick (5 episodes + sample)"
        );

        let expected_expanded = expanded_indices.clone();
        let outcome2 = client
            .add_torrent_with_file_filter(&magnet, &info_hash, &mut |_names| {
                Some(expected_expanded.clone())
            })
            .await
            .expect("add_torrent_with_file_filter C2 failed");

        match outcome2 {
            SelectiveOutcome::Filtered(_) => {}
            SelectiveOutcome::FullDownload => {
                panic!("C2 expected Filtered re-narrow, got FullDownload");
            }
        }

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
        eprintln!("C2 re-narrow verified (sample now wanted, readme still skipped)");

        // --- A7: delete with delete_files=true removes torrent + files ---
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
        eprintln!("A7 delete(true) verified — torrent gone from list_scoped");
        eprintln!("narrowed-smoke passed");
    }

    /// Live smoke covering B2 (`list_scoped` isolation): uploads a
    /// Ryokan-scoped torrent alongside a non-Ryokan torrent and
    /// asserts `list_scoped` returns exactly one (the Ryokan one).
    /// Validates the scoping mechanism (qBit's `?category=` filter)
    /// does the right thing when other tooling's torrents are also
    /// present in the client.
    ///
    ///     RYOKAN_QBIT_E2E=1 QBIT_PASS=<pw> cargo test \
    ///       qbittorrent::tests::live_smoke_scoped_exclusion -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live qBittorrent at localhost:8080 + transmission-create"]
    async fn live_smoke_scoped_exclusion() {
        if std::env::var("RYOKAN_QBIT_E2E").is_err() {
            eprintln!("skipping (set RYOKAN_QBIT_E2E=1 to run against localhost:8080)");
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
        let pass = std::env::var("QBIT_PASS").unwrap_or_else(|_| "adminadmin".to_string());
        let base_url = "http://localhost:8080";
        let ryokan_category = "ryokan-e2e-scope";
        let foreign_category = "other-tool-scope";

        let ryokan_hash = super::super::test_helpers::upload_torrent_file_qbit(
            base_url,
            "admin",
            &pass,
            ryokan_category,
            &torrent1,
        )
        .await;
        let foreign_hash = super::super::test_helpers::upload_torrent_file_qbit(
            base_url,
            "admin",
            &pass,
            foreign_category,
            &torrent2,
        )
        .await;
        eprintln!("ryokan={ryokan_hash} foreign={foreign_hash}");
        assert_ne!(
            ryokan_hash, foreign_hash,
            "test distinct torrents must have distinct hashes"
        );

        let client = QbitClient::new(base_url, "admin", &pass, ryokan_category);
        let list = client
            .list_scoped()
            .await
            .expect("list_scoped should succeed");

        assert!(
            list.iter()
                .any(|t| t.hash.eq_ignore_ascii_case(&ryokan_hash)),
            "B2: Ryokan-scoped torrent must appear in list_scoped"
        );
        assert!(
            !list
                .iter()
                .any(|t| t.hash.eq_ignore_ascii_case(&foreign_hash)),
            "B2: foreign-categorized torrent must NOT appear in list_scoped (found foreign {foreign_hash} in scoped list)"
        );
        eprintln!("B2 scoped exclusion verified (Ryokan present, foreign absent)");

        // Cleanup both — foreign one via a separate client instance scoped to it.
        client
            .delete(&ryokan_hash, true)
            .await
            .expect("cleanup ryokan");
        let foreign_client = QbitClient::new(base_url, "admin", &pass, foreign_category);
        foreign_client
            .delete(&foreign_hash, true)
            .await
            .expect("cleanup foreign");
        eprintln!("scoped-exclusion smoke passed");
    }

    /// Live smoke covering error paths (F1, F2, F3): `delete` and
    /// `get_files` on a non-existent hash must not panic and must
    /// surface a sensible result; `add_torrent` with a malformed URL
    /// must return `Err`. Validates defensive behavior Ryokan's
    /// handlers depend on (e.g. post-processing calling `get_files`
    /// on a hash that might have been deleted by another path, or
    /// the user pasting garbage into the interactive-search URL
    /// field).
    ///
    ///     RYOKAN_QBIT_E2E=1 QBIT_PASS=<pw> cargo test \
    ///       qbittorrent::tests::live_smoke_error_paths -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live qBittorrent at localhost:8080"]
    async fn live_smoke_error_paths() {
        if std::env::var("RYOKAN_QBIT_E2E").is_err() {
            eprintln!("skipping");
            return;
        }
        let pass = std::env::var("QBIT_PASS").unwrap_or_else(|_| "adminadmin".to_string());
        let client = QbitClient::new("http://localhost:8080", "admin", &pass, "ryokan-e2e-errs");
        let fake_hash = "0000000000000000000000000000000000000000";

        // F1: delete non-existent hash — should return Ok (qBit's
        // DELETE is idempotent; deleting a hash not in the session
        // is a no-op). Must not panic regardless.
        let result = client.delete(fake_hash, false).await;
        eprintln!("F1 qBit delete(non-existent) → {result:?}");
        // qBit's actual behavior: silently succeeds. Accept both so
        // a future qBit version that starts erroring doesn't regress
        // the test — the essential property is "no panic".

        // F2: get_files non-existent — per trait contract returns
        // empty `Vec` or Err (qBit returns 404, which the impl maps
        // to Err). Must not panic.
        let result = client.get_files(fake_hash).await;
        eprintln!("F2 qBit get_files(non-existent) → {result:?}");
        if let Ok(files) = result {
            assert!(
                files.is_empty(),
                "F2: Ok result must be empty Vec per trait contract"
            );
        }
        // Err is also acceptable per trait contract; only panic is a fail.

        // F3: add with malformed URL. Must return Err, not panic.
        let result = client
            .add_torrent("this-is-not-a-valid-url-or-magnet", fake_hash)
            .await;
        eprintln!("F3 qBit add(malformed) → {result:?}");
        assert!(
            result.is_err(),
            "F3: add_torrent with malformed URL must return Err (got {result:?})"
        );

        eprintln!("error-paths smoke passed");
    }

    /// Live smoke for E1+E2: verifies `DownloadItemState` transitions
    /// correctly through pause→resume→pause and that `progress` stays
    /// in its contract range [0.0, 1.0] throughout. Uses the synthetic
    /// testpack so there's no real content to download — the torrent
    /// sits at ~0 progress, letting us observe state changes without
    /// racing a real download.
    ///
    ///     RYOKAN_QBIT_E2E=1 QBIT_PASS=<pw> cargo test \
    ///       qbittorrent::tests::live_smoke_state_progress -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live qBittorrent at localhost:8080 + transmission-create"]
    async fn live_smoke_state_progress() {
        if std::env::var("RYOKAN_QBIT_E2E").is_err() {
            eprintln!("skipping");
            return;
        }
        let Some((_tmp, torrent_path)) = super::super::test_helpers::build_testpack_torrent()
        else {
            return;
        };
        let pass = std::env::var("QBIT_PASS").unwrap_or_else(|_| "adminadmin".to_string());
        let base_url = "http://localhost:8080";
        let category = "ryokan-e2e-state";

        let info_hash = super::super::test_helpers::upload_torrent_file_qbit(
            base_url,
            "admin",
            &pass,
            category,
            &torrent_path,
        )
        .await;
        let client = QbitClient::new(base_url, "admin", &pass, category);

        // qBit 5.x ignores the `paused` / `stopped` multipart flag on
        // add — empirically verified against v5.1.4. Issue an explicit
        // pause so the torrent settles into the Paused state we
        // expect to observe. This isn't cheating the test: the
        // `pause()` RPC round-trip is what the test would exercise on
        // the Resume→Pause transition anyway.
        tokio::time::sleep(Duration::from_millis(500)).await;
        client.pause(&info_hash).await.expect("initial pause");

        /// Poll list_scoped until the target hash appears AND its
        /// state_kind matches one of `acceptable`. qBit goes through
        /// transient states on add (e.g. `checkingResumeData` which
        /// the state mapping surfaces as Downloading) before settling
        /// to the requested pause/resume state — we need the stable
        /// state, not the first observation.
        async fn poll_until_state(
            client: &QbitClient,
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
            // Fall through: return the last observed state so the
            // assertion below produces a useful error message.
            let list = client.list_scoped().await.expect("list_scoped");
            list.iter()
                .find(|t| t.hash.eq_ignore_ascii_case(hash))
                .cloned()
                .unwrap_or_else(|| panic!("torrent never appeared in list_scoped"))
        }

        // Uploaded paused → state should settle to Paused (after qBit
        // completes resume-data checking, typically <1s).
        let t = poll_until_state(
            &client,
            &info_hash,
            &[DownloadItemState::Paused, DownloadItemState::PausedComplete],
        )
        .await;
        eprintln!(
            "E1 after paused-upload: state={:?} ({}) progress={}",
            t.state_kind, t.state, t.progress
        );
        assert!(
            matches!(
                t.state_kind,
                DownloadItemState::Paused | DownloadItemState::PausedComplete
            ),
            "E1: expected Paused after paused-upload, got {:?} ({})",
            t.state_kind,
            t.state
        );
        assert!(
            (0.0..=1.0).contains(&t.progress),
            "E2: progress must be in [0.0, 1.0], got {}",
            t.progress
        );

        // Resume → state transitions to a Downloading* variant.
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
            "E1 after resume: state={:?} ({}) progress={}",
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
            "E1: expected Downloading* after resume, got {:?} ({})",
            t.state_kind,
            t.state
        );
        assert!(
            (0.0..=1.0).contains(&t.progress),
            "E2 progress: {}",
            t.progress
        );

        // Pause again → back to Paused.
        client.pause(&info_hash).await.expect("pause");
        let t = poll_until_state(
            &client,
            &info_hash,
            &[DownloadItemState::Paused, DownloadItemState::PausedComplete],
        )
        .await;
        eprintln!(
            "E1 after re-pause: state={:?} ({}) progress={}",
            t.state_kind, t.state, t.progress
        );
        assert!(
            matches!(
                t.state_kind,
                DownloadItemState::Paused | DownloadItemState::PausedComplete
            ),
            "E1: expected Paused after re-pause, got {:?} ({})",
            t.state_kind,
            t.state
        );

        client.delete(&info_hash, true).await.expect("cleanup");
        eprintln!("state-progress smoke passed");
    }
}

/// Wire-level tests backed by `wiremock` — covers the trait methods
/// (add_torrent, list_scoped, get_files, pause/resume/delete,
/// set_file_wanted) against a mock HTTP server, filling the
/// request-construction gap that Sonarr's proxy-layer mocking
/// deliberately skips. Separate top-level submodule rather than
/// living inside the existing `tests` module so wiremock's
/// `tokio::test`-based server spin-up doesn't entangle with the
/// pure-function tests above.
#[cfg(test)]
mod wiremock_tests;

#[cfg(test)]
mod seeding_done_tests {
    //! Issue #228: `qbit_seeding_done` reads qBit's effective share
    //! limits off `torrents/info`. Units: `seeding_time` seconds,
    //! `max_seeding_time` / `max_inactive_seeding_time` minutes,
    //! `last_activity` unix seconds, `-1` = no limit.
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn raw(state: &str) -> QbitRawTorrent {
        QbitRawTorrent {
            hash: "h".into(),
            name: "n".into(),
            size: 1,
            progress: 1.0,
            dlspeed: 0,
            state: state.into(),
            category: String::new(),
            eta: 0,
            save_path: String::new(),
            content_path: String::new(),
            ratio: 0.0,
            max_ratio: -1.0,
            seeding_time: 0,
            max_seeding_time: -1,
            max_inactive_seeding_time: -1,
            last_activity: 0,
        }
    }

    #[test]
    fn paused_at_ratio_limit_is_done() {
        let mut t = raw("pausedUP");
        t.max_ratio = 2.0;
        t.ratio = 2.01;
        assert!(qbit_seeding_done(&t, NOW));
        t.state = "stoppedUP".into();
        assert!(
            qbit_seeding_done(&t, NOW),
            "5.x spells the stop state stoppedUP"
        );
    }

    #[test]
    fn stopped_at_seeding_time_limit_is_done() {
        let mut t = raw("stoppedUP");
        t.max_seeding_time = 60;
        t.seeding_time = 60 * 60;
        assert!(qbit_seeding_done(&t, NOW));
        t.seeding_time = 59 * 60;
        assert!(
            !qbit_seeding_done(&t, NOW),
            "minutes on the limit, seconds on the clock"
        );
    }

    #[test]
    fn stopped_at_inactivity_limit_is_done() {
        let mut t = raw("pausedUP");
        t.max_inactive_seeding_time = 30;
        t.last_activity = NOW - 31 * 60;
        assert!(qbit_seeding_done(&t, NOW));
        t.last_activity = NOW - 10 * 60;
        assert!(!qbit_seeding_done(&t, NOW));
        t.last_activity = 0;
        assert!(
            !qbit_seeding_done(&t, NOW),
            "no activity timestamp, no inactivity verdict"
        );
    }

    #[test]
    fn paused_below_every_limit_was_paused_by_hand() {
        let mut t = raw("pausedUP");
        t.max_ratio = 2.0;
        t.ratio = 1.5;
        t.max_seeding_time = 600;
        t.seeding_time = 60;
        assert!(!qbit_seeding_done(&t, NOW));
    }

    #[test]
    fn still_seeding_is_never_done_even_past_the_limit() {
        let mut t = raw("uploading");
        t.max_ratio = 1.0;
        t.ratio = 3.0;
        assert!(!qbit_seeding_done(&t, NOW));
        t.state = "stalledUP".into();
        assert!(!qbit_seeding_done(&t, NOW));
    }

    #[test]
    fn no_limits_means_never_done() {
        let mut t = raw("pausedUP");
        t.ratio = 99.0;
        t.seeding_time = 10_000_000;
        assert!(
            !qbit_seeding_done(&t, NOW),
            "-1 on every limit is qBit's 'unlimited'"
        );
    }

    #[test]
    fn old_builds_without_the_fields_read_as_no_limit() {
        let t: QbitRawTorrent = serde_json::from_value(serde_json::json!({
            "hash": "h", "name": "n", "size": 1, "progress": 1.0, "dlspeed": 0,
            "state": "pausedUP", "category": "", "eta": 0
        }))
        .unwrap();
        assert_eq!(t.max_ratio, -1.0);
        assert_eq!(t.max_seeding_time, -1);
        assert_eq!(t.max_inactive_seeding_time, -1);
        assert!(!qbit_seeding_done(&t, NOW));
    }
}
