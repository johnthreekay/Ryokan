//! SABnzbd implementation of [`DownloadClient`] (#28).
//!
//! Speaks the SAB API
//! (<https://sabnzbd.org/wiki/configuration/4.0/api>). Authenticates by
//! `?apikey=…` on every request — SAB has no session/cookie shape, so
//! there's no equivalent to qBit's `/auth/login` re-auth path.
//!
//! ### Why SAB needs the trait's returning-id variant
//!
//! BT clients (qBit, Deluge, Transmission, rTorrent) all key torrents
//! by v1 infohash — Ryokan computes that from the magnet URL up-front
//! and the trait's `info_hash` parameter is canonical at every wire
//! call. Usenet has no such precomputed id: SAB hands back an
//! `nzo_id` (e.g. `"SABnzbd_nzo_abc123def"`) when the NZB is added,
//! and every subsequent op (queue/history list, pause, resume, delete,
//! file list) keys off that opaque string. There's no formula to
//! derive `nzo_id` from the NZB URL.
//!
//! This impl therefore overrides [`DownloadClient::add_torrent_returning_id`]
//! to capture the `nzo_id` from the `mode=addurl` response and return
//! it to the caller. Callers (RSS sync, autobrr, manual grab, picker)
//! persist the returned id on `grabbed_torrents.hash`. Subsequent ops
//! receive the `nzo_id` as the trait's `info_hash` parameter and use
//! it verbatim.
//!
//! Sonarr's pattern, lifted directly: `Download(...) -> string`
//! returns whichever opaque id the wire format uses; the trait's
//! 40-char-hex contract was specific to BT and only applies inside
//! the four BT impls' add paths.
//!
//! ### Per-client wire quirks (live-probed against SAB 4.x)
//!
//! - **Endpoint shape:** every call is `GET <base>/api?apikey=…&mode=…&output=json`.
//!   The user's configured base IS the base — the impl just appends
//!   `/api`. The `/sabnzbd` URL_BASE prefix isn't default; it's
//!   per-install (set via SAB's `URL_BASE` config). The
//!   linuxserver/sabnzbd Docker image, the Ubuntu .deb, and most
//!   bare installs serve the API at `/api` directly. Installs that
//!   kept SAB's `URL_BASE = /sabnzbd` should configure the base
//!   URL as `http://host:8080/sabnzbd`, and the impl appends `/api`
//!   to land at `/sabnzbd/api`. Live-probed 2026-04-27 against the
//!   linuxserver/sabnzbd image — pre-fix v1 of this impl assumed
//!   the prefix was default and 404'd.
//! - **Add response surface:** `mode=addurl` returns
//!   `{"status":true,"nzo_ids":["SABnzbd_nzo_..."]}` on success.
//!   Empty `nzo_ids` array can mean "duplicate" (SAB's pre-queue dup
//!   detection caught it) — distinguishable only by scanning queue +
//!   history for the URL/filename. v1 of this impl reports
//!   [`AddOutcome::AlreadyPresent`] when `nzo_ids` is empty AND a
//!   `mode=queue` scan finds a slot whose `url` matches; otherwise it
//!   surfaces the empty array as an error so a real failure
//!   (malformed URL, indexer auth issue) doesn't slip past as a
//!   silent success.
//! - **No per-file selection:** SAB downloads NZBs as opaque blobs;
//!   per-file selection (Ryokan's `set_file_wanted`) only becomes
//!   meaningful post-extraction, which SAB handles in its post-
//!   processing pipeline outside Ryokan's reach. The impl therefore
//!   no-ops `set_file_wanted` and returns `SelectiveOutcome::FullDownload`
//!   from `add_torrent_with_file_filter`. The interactive-picker UI
//!   code paths still open against SAB grabs but the user's selection
//!   is silently discarded — better than crashing the modal. A
//!   follow-up could intercept SAB's post-processing extraction step
//!   and apply a file filter there.
//! - **v1 picker-path limitation:** the picker (preview→confirm)
//!   plus `auto_search`'s batch-with-selective branch and
//!   `library/search/grab.rs` selective batches all internally call
//!   `add_torrent_with_file_filter`, which doesn't surface the
//!   captured `nzo_id`. SAB grabs that flow through these paths get
//!   their pre-add (BT-style) `info_hash` persisted on
//!   `grabbed_torrents.hash`, which won't match the real `nzo_id` at
//!   post-processing time — the grab will be marked stale-removed
//!   after 60s. v1 expects the dominant SAB grab paths to be RSS,
//!   autobrr push, manual `/api/grab`, and the upgrade sweep — all
//!   four of which use `add_torrent_returning_id` and persist the
//!   real `nzo_id`. Selective batch grabs through newznab are
//!   uncommon (NZBs are typically single-episode) but a user who
//!   tries one will see the imported file appear via post-
//!   processing's directory scan (eventually) rather than the
//!   `grabbed_torrents` row's hash-match — so library attribution
//!   may end up missing. Tracked as a follow-up.
//! - **Add paused:** SAB's `mode=addurl&priority=-1` adds the NZB at
//!   priority "Paused" (not the same as "Stopped" — the queue still
//!   processes it but doesn't actively download). Closest analog to
//!   the BT add-paused contract; suitable for the picker's
//!   metadata-wait + selection flow because file lists arrive
//!   instantly (NZB describes the file set up-front, no metadata
//!   handshake needed).
//! - **Categories vs labels:** SAB uses `cat=<name>` as both the
//!   scoping mechanism and the post-processing target directory
//!   selector. The impl threads `config.label` (from the
//!   `download_clients` row) as `cat=…` on every add and filters
//!   `list_scoped` by `category` on each returned slot. Mirrors the
//!   qBit category convention.
//! - **Storage path:** SAB returns `storage` on completed history
//!   slots — the absolute path to the unpacked output directory.
//!   Queue slots have no storage value yet (download still running),
//!   so `content_path` reads as empty until the slot moves to
//!   history. Post-processing's stale-mark grace window already
//!   handles "no content_path yet" by waiting on the next pass.

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

#[cfg(test)]
mod wiremock_tests;

use super::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};

/// Default HTTP timeout for SAB calls. SAB's local-LAN responses are
/// sub-100ms in practice; the timeout exists to keep a hung daemon
/// from stalling Ryokan's grab path indefinitely. Matches the budget
/// the BT impls give their wire clients.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Same shape as the BT impls' helpers — prepend `http://` for
/// scheme-less local addresses, `https://` for everything else.
/// Without this, a user typing `localhost:8085` (matching the qBit /
/// Deluge / Transmission UX) would land in reqwest as scheme=
/// `localhost`, path=`8085` and the HTTP request builder would
/// reject the eventual `/api` URL with a cryptic builder error.
fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
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

pub struct SabClient {
    /// Base URL — `http://host:port` or `http://host:port/sabnzbd`.
    /// The URL builder normalizes both shapes; trailing slash
    /// tolerated.
    base_url: String,
    api_key: String,
    /// Category name used for scoping (`list_scoped` filters; `addurl`
    /// stamps). Mirrors `config.label` on the `download_clients` row.
    category: String,
    http: Client,
    /// One-shot guard for the add-path category check. Set after the
    /// first `ensure_category_cached_once` attempt of this client's
    /// lifetime, regardless of outcome — on permanent failure
    /// (read-only `nzb_key`, etc.) retrying every grab would just
    /// spam the log, and on transient failure a process restart
    /// re-attempts. Test-connection (`test()`) bypasses this and
    /// always probes; the user clicked Test specifically.
    category_ensured: std::sync::atomic::AtomicBool,
}

impl SabClient {
    /// Construct from the raw `download_clients` row fields.
    /// `username` is unused (SAB has no concept of per-user accounts at
    /// the API layer; auth is API-key only) but the constructor takes
    /// it for shape-parity with the BT impls' `(url, user, pass, label)`
    /// signature.
    pub fn new(base_url: &str, _username: &str, api_key: &str, category: &str) -> Self {
        let http = Client::builder()
            .user_agent("Ryokan/0.1")
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url: normalize_base_url(base_url),
            api_key: api_key.to_string(),
            category: category.to_string(),
            http,
            category_ensured: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Build the `/api` URL. The user provides the full base
    /// (`http://host:8080` for installs with no URL_BASE prefix —
    /// linuxserver/sabnzbd Docker, the Ubuntu .deb, most bare
    /// installs; or `http://host:8080/sabnzbd` for installs that
    /// kept SAB's default URL_BASE) and the impl just appends
    /// `/api`.
    ///
    /// Live-probed 2026-04-27: the `/sabnzbd` prefix is NOT default
    /// on the linuxserver image; it's a per-install knob the user
    /// sets via SAB's `URL_BASE` config option. Pre-fix v1 of this
    /// impl defaulted the *other* direction (appending `/sabnzbd/api`
    /// when the base didn't have the prefix), which 404'd against
    /// every install that didn't change `URL_BASE`. The user's
    /// configured base IS the base — we don't second-guess.
    fn endpoint(&self) -> String {
        format!("{}/api", self.base_url)
    }

    /// Single-pair query helper. SAB takes mode + per-mode args via
    /// query string. We always append `output=json` and `apikey=…`.
    fn make_query<'a>(&'a self, params: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut q: Vec<(&str, &str)> = Vec::with_capacity(params.len() + 2);
        q.push(("apikey", self.api_key.as_str()));
        q.push(("output", "json"));
        q.extend_from_slice(params);
        q
    }

    /// Map SAB's status string to Ryokan's normalized `DownloadItemState`.
    /// Queue states: `Queued`, `Grabbing`, `Downloading`, `Paused`,
    /// `Verifying`, `Repairing`, `Extracting`, `Moving`, `Running`,
    /// `Completed`, `Failed`. History adds `Completed` and `Failed`.
    fn map_state(status: &str, is_history: bool) -> DownloadItemState {
        match status {
            "Downloading" | "Grabbing" | "Running" => DownloadItemState::Downloading,
            "Queued" => DownloadItemState::DownloadingQueued,
            "Paused" => DownloadItemState::Paused,
            // SAB's verify/repair/extract/move steps are part of its
            // post-processing pipeline. Treat them as "checking" so
            // Ryokan's import path doesn't trip on a half-extracted
            // file. They all transition to Completed on success.
            "Verifying" | "Repairing" | "Extracting" | "Moving" => {
                DownloadItemState::CheckingDownload
            }
            "Completed" => DownloadItemState::PausedComplete,
            "Failed" => DownloadItemState::Errored,
            _ => {
                // History rows in unknown post-proc states usually
                // mean "completed-with-some-issue" — surface as
                // Errored so post-processing skips the import. Queue
                // rows in unknown states default to Downloading
                // (best-effort assumption that motion is happening).
                if is_history {
                    DownloadItemState::Errored
                } else {
                    DownloadItemState::Downloading
                }
            }
        }
    }
}

#[async_trait]
impl DownloadClient for SabClient {
    async fn test(&self) -> Result<String, String> {
        // Two-step probe: `mode=version` is a PUBLIC SAB endpoint and
        // returns 200 even with a missing/wrong API key, so it can't
        // surface auth issues. `mode=queue` requires the API key, so
        // following the version probe with a queue probe catches a
        // bad/missing key at Test-connection time instead of at
        // first-grab time. Without this, users would see a green
        // "Connected: 4.5.5" pill, then their first NZB grab would
        // fail with `SAB add returned HTTP 403 Forbidden` — the
        // exact symptom the SAB-on-NZBGeek-paired-with-Prowlarr
        // setup hits when the password / api_key field on the
        // download_clients row is empty or wrong.
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[("mode", "version")]))
            .send()
            .await
            .map_err(|e| format!("SAB request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SAB returned HTTP {}", resp.status()));
        }
        let body: VersionResponse = resp
            .json()
            .await
            .map_err(|e| format!("SAB version parse failed: {e}"))?;
        let version = body.version;

        // Auth probe — fail with a clear message when the API key is
        // missing/invalid. Only the status code matters; queue body
        // shape isn't parsed.
        let auth_resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[("mode", "queue"), ("start", "0"), ("limit", "1")]))
            .send()
            .await
            .map_err(|e| format!("SAB queue probe failed: {e}"))?;
        if auth_resp.status().as_u16() == 401 || auth_resp.status().as_u16() == 403 {
            // SAB returns plain text on 403 ("API Key Required" /
            // "API Key Incorrect"); surface it so the user knows
            // exactly which field to fix on the download-clients row.
            let detail = auth_resp.text().await.unwrap_or_default();
            let trimmed = detail.trim();
            return Err(if trimmed.is_empty() {
                "SAB API key missing or invalid. Set the API Key field on the SABnzbd download-client row.".to_string()
            } else {
                format!(
                    "SAB API key missing or invalid: {}. Set the API Key field on the SABnzbd download-client row.",
                    trimmed
                )
            });
        }
        if !auth_resp.status().is_success() {
            return Err(format!(
                "SAB queue probe returned HTTP {}",
                auth_resp.status()
            ));
        }

        // Auto-create the configured category if it's missing in
        // SAB. Non-fatal: a get_cats parse failure or a set_config
        // permission error surfaces as a parenthesized warning on
        // the Test-connection toast rather than failing the test
        // (the connection itself works; categories are a secondary
        // concern). The created flag is true only on a successful
        // create — a no-op when the category already exists is
        // silent. Empty configured category short-circuits to a
        // no-op silently too, since the user opted out of filtering.
        let cat_msg = match self.ensure_category().await {
            Ok(true) => format!(" (created category '{}' in SAB)", self.category),
            Ok(false) => String::new(),
            Err(e) => format!(" (warning: {e})"),
        };

        // Return the bare version string (plus optional category
        // note). The status pill on the Settings → Download Clients
        // tab prepends the client kind label itself, so a "SABnzbd "
        // prefix here would render as "SABnzbd SABnzbd 4.5.5"; the
        // toast on the Test-connection button concatenates
        // "Connected: <version>" and reads more naturally without
        // the kind doubling either.
        Ok(format!("{version}{cat_msg}"))
    }

    async fn add_torrent(&self, url: &str, _info_hash: &str) -> Result<AddOutcome, String> {
        // BT-shape callers that don't read the returned id still need
        // a working add path. Drop the captured id; the caller's
        // `info_hash` was a synthetic one or empty — neither is
        // useful as the canonical id. This shape exists only for
        // legacy call sites; new code should use
        // `add_torrent_returning_id`.
        let (outcome, _id) = self.add_torrent_returning_id(url, _info_hash).await?;
        Ok(outcome)
    }

    async fn add_torrent_returning_id(
        &self,
        url: &str,
        _info_hash: &str,
    ) -> Result<(AddOutcome, String), String> {
        // First-grab safety net: ensure the configured category
        // exists in SAB before issuing addurl. Without this, a user
        // who saved their SAB row in Settings without clicking Test
        // and then fired their first grab would have the addurl's
        // `cat=anime` parameter silently fall back to SAB's default
        // bucket — Ryokan's `list_scoped` would then filter the job
        // out and "my SAB downloads aren't getting picked up" reports
        // would persist in spite of the Test-time auto-create.
        // Cached one-shot per process so the cost is paid by the
        // first add only.
        let just_created_category = self.ensure_category_cached_once().await;

        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[
                ("mode", "addurl"),
                ("name", url),
                ("cat", self.category.as_str()),
            ]))
            .send()
            .await
            .map_err(|e| format!("SAB request failed: {e}"))?;
        if !resp.status().is_success() {
            // 401/403 specifically — capture SAB's plain-text body so
            // the user sees `API Key Required` / `API Key Incorrect`
            // instead of the bare HTTP status. Other status codes
            // surface the status alone since SAB's body for those
            // (5xx, 502 Bad Gateway from a fronting proxy, etc.)
            // isn't usefully diagnostic. The Test-connection probe
            // now catches API-key issues at config time, but this
            // path stays robust for users who upgraded a working
            // setup and somehow lost their key.
            let status = resp.status();
            let detail = if matches!(status.as_u16(), 401 | 403) {
                resp.text().await.unwrap_or_default()
            } else {
                String::new()
            };
            let trimmed = detail.trim();
            if trimmed.is_empty() {
                return Err(format!("SAB add returned HTTP {status}"));
            }
            return Err(format!("SAB add returned HTTP {status}: {trimmed}"));
        }
        // SAB sometimes returns 200 with an HTML page (not JSON) when
        // the API key is missing — varies by version + URL_BASE
        // config, and the user-pasted-NZB-Key-instead-of-API-Key
        // case is the most common footgun. Read the body as bytes
        // once so we can fall back to a substring check on the
        // well-known "API Key" warning if JSON parsing fails.
        // Without this the user got an opaque parse error instead of
        // an actionable hint.
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("SAB addurl read body failed: {e}"))?;
        let body: AddUrlResponse = match serde_json::from_slice(&body_bytes) {
            Ok(b) => b,
            Err(e) => {
                let body_text = std::str::from_utf8(&body_bytes).unwrap_or_default();
                if body_text.to_ascii_lowercase().contains("api key") {
                    return Err(format!(
                        "SAB rejected addurl: API key missing. Set the API Key field on the SABnzbd download-client row (Settings → Connections → Downloads → click the SAB row). Use SAB's full API Key (not the NZB Key — Ryokan needs queue access too). Find it in SABnzbd → Config → General → Security → API Key. SAB body: {}",
                        body_text.trim()
                    ));
                }
                return Err(format!("SAB addurl parse failed: {e}"));
            }
        };
        if !body.status {
            let raw_error = body.error.unwrap_or_else(|| "no error provided".into());
            if raw_error.to_ascii_lowercase().contains("api key") {
                return Err(format!(
                    "SAB rejected addurl: API key missing or invalid. Use the full API Key (not the NZB Key) from SABnzbd → Config → General → Security. SAB error: {raw_error}"
                ));
            }
            return Err(format!("SAB rejected addurl: {raw_error}"));
        }
        if let Some(id) = body.nzo_ids.into_iter().next() {
            // Defensive re-tag when we just created the category in
            // this same call. SAB's `set_config` writes the category
            // to its config; whether that change is visible to the
            // very next addurl call is not guaranteed by SAB's docs.
            // If our addurl fired before SAB's config reload
            // propagated, the slot landed in the default bucket
            // despite cat=…, and Ryokan's `list_scoped` filter would
            // drop it on the very next poll. `change_cat` fixes the
            // already-queued slot up so the category is correct.
            // Failure is logged but doesn't fail the add — the
            // worst case is the user sees the same vanish-from-view
            // symptom for this one grab and next-process-restart
            // self-heals because `ensure_category_cached_once`
            // already flagged us as ensured.
            self.maybe_change_cat_after_add(just_created_category, &id)
                .await;
            return Ok((AddOutcome::Added, id));
        }
        // Empty nzo_ids on a status:true response is SAB's pre-queue
        // dedup signal. Scan queue+history to confirm the URL is
        // already in the system; surface that as AlreadyPresent so
        // the upstream grab path's idempotency check works the same
        // as it does for BT clients. If we can't find a match, treat
        // it as a real failure rather than papering over.
        //
        // No defensive change_cat on the AlreadyPresent path: the
        // slot pre-existed this addurl call, so it's either already
        // tagged correctly (added by Ryokan in a prior session) or
        // was added by another tool (in which case re-tagging would
        // overstep and clobber that tool's metadata).
        let already = self.find_id_for_url(url).await;
        match already {
            Some(id) => Ok((AddOutcome::AlreadyPresent, id)),
            None => Err("SAB addurl returned no nzo_id and no matching queue/history slot".into()),
        }
    }

    async fn add_torrent_paused(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        let (outcome, _id) = self.add_torrent_paused_returning_id(url, info_hash).await?;
        Ok(outcome)
    }

    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        info_hash: &str,
        _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        // SAB has no per-file API for in-flight downloads — file
        // selection is done at extraction time post-download via
        // SAB's own scripting hooks, outside Ryokan's reach. Dispatch
        // a normal add and report `FullDownload` so the picker
        // surface treats it as a non-narrow grab. The interactive-
        // picker modal still works (it shows the file list from the
        // returned add response if available, otherwise polls), but
        // the user's selection is silently discarded.
        let _ = self.add_torrent(url, info_hash).await?;
        Ok(SelectiveOutcome::FullDownload)
    }

    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        let queue = self.fetch_queue().await?;
        let history = self.fetch_history().await?;
        let queue_total = queue.slots.len();
        let history_total = history.slots.len();
        let mut out: Vec<DownloadItem> = Vec::with_capacity(queue_total + history_total);

        // Category-match policy:
        //   - Empty `self.category` → no filter; pass everything
        //     through. The user didn't pin a category in Ryokan's
        //     SAB row, so the trade-off "see all SAB activity vs
        //     never see anything" picks visibility.
        //   - Non-empty `self.category` → match SAB's slot category
        //     case-insensitively, AND also accept slots whose
        //     reported category is empty (SAB sometimes drops the
        //     category on jobs added with a cat= parameter that
        //     doesn't correspond to a configured category in SAB's
        //     UI; the addurl call still succeeds but the resulting
        //     slot reports `cat=""`). Without this, jobs Ryokan
        //     just queued via `add_torrent` would silently fail
        //     the filter and the reconcile loop would mark them
        //     removed at the 30s grace window. The trade-off is
        //     accepting cross-tool noise (jobs added by another
        //     SAB caller with no category) — acceptable for a
        //     single-user homelab; revisit if Ryokan grows multi-
        //     user.
        let configured = self.category.trim();
        let want_lower = configured.to_ascii_lowercase();
        let category_matches = |slot_cat: &str| -> bool {
            if configured.is_empty() {
                return true;
            }
            let s = slot_cat.trim();
            s.is_empty() || s.eq_ignore_ascii_case(&want_lower)
        };

        // Track categories of slots we drop so the diagnostic trace
        // below can show the user the actual mismatch. Allocations
        // here are cheap (one String per dropped slot, only when
        // there's a mismatch) and zero on the happy path.
        let mut dropped_cats: Vec<String> = Vec::new();

        for slot in queue.slots {
            if category_matches(&slot.cat) {
                out.push(DownloadItem {
                    hash: slot.nzo_id,
                    name: slot.filename,
                    // SAB reports size as a free-form string ("1.2 GB"). The
                    // bytes field isn't in the public schema for queue
                    // slots, so we leave it 0 here — the post-processing
                    // path doesn't read size off DownloadItem; the
                    // imported file's stat is the source of truth.
                    size: 0,
                    progress: parse_percentage(&slot.percentage),
                    dlspeed: parse_speed_bytes(&slot.kbpersec),
                    state: slot.status.clone(),
                    category: slot.cat,
                    eta: parse_eta_seconds(&slot.timeleft),
                    save_path: String::new(),
                    content_path: String::new(),
                    state_kind: Self::map_state(&slot.status, false),
                    seeding_done: false,
                });
            } else {
                dropped_cats.push(slot.cat);
            }
        }
        for slot in history.slots {
            if !category_matches(&slot.category) {
                dropped_cats.push(slot.category);
                continue;
            }
            // Title-matching ancestor walk, ported from Sonarr's
            // SAB client (`Sabnzbd.cs::GetHistory`). SAB's `storage`
            // field can be:
            //   1. The per-job folder path (`/complete/<title>/`) —
            //      the typical case.
            //   2. A file path inside the job folder
            //      (`/complete/<title>/file.mkv`) — single-file
            //      extracts on some SAB versions.
            //   3. The parent complete dir alone (`/complete/`) —
            //      pathological-but-real edge case observed in the
            //      wild; happens when SAB couldn't determine where
            //      the job extracted to (no rar / weird archive
            //      shape) and fell back to recording the parent.
            //
            // Walk the parent chain of `storage` looking for an
            // ancestor whose filename equals `name` (the job title).
            // That ancestor IS the per-job folder. If we find one,
            // use it as the canonical content_path so import-time
            // walks and delete-time cleanups operate on a precise
            // per-job scope. Without this narrowing, a job's
            // `import_torrent` walks the WHOLE complete dir and
            // sweeps in files belonging to OTHER grabs — which then
            // get stamped onto this grab's `imported_source_paths`
            // and incorrectly removed when this grab is deleted.
            //
            // Fallback when no ancestor matches: try
            // `<storage>/<title>/` as a candidate (covers case 3
            // above where Storage is the parent). If neither
            // resolution works the original `storage` is kept so
            // downstream code at least has something to attempt.
            let canonical_job_path = canonical_job_path(&slot.storage, &slot.name);

            out.push(DownloadItem {
                hash: slot.nzo_id,
                name: slot.name,
                size: slot.bytes,
                progress: 1.0,
                dlspeed: 0,
                state: slot.status.clone(),
                category: slot.category,
                eta: 0,
                save_path: canonical_job_path.clone(),
                content_path: canonical_job_path,
                state_kind: Self::map_state(&slot.status, true),
                seeding_done: false,
            });
        }

        // Diagnostic trace, fires only when SAB returned slots but
        // every one of them got dropped by the category filter — i.e.
        // a user-reported "Ryokan can't see my SAB jobs" case. Avoids
        // spamming logs every 5s on the happy path (the queue-tab
        // poll calls list_scoped on every refresh). The seen-cats
        // dump is the actionable bit: a user comparing
        // `configured_category="anime"` against
        // `seen_categories={"default", "tv"}` can fix the mismatch
        // without having to ssh into the box and inspect SAB's API
        // by hand.
        let total = queue_total + history_total;
        if total > 0 && out.is_empty() {
            let unique: std::collections::BTreeSet<&str> =
                dropped_cats.iter().map(String::as_str).collect();
            tracing::debug!(
                "sab list_scoped: dropped every slot via category filter — configured_category={:?} queue_slots={} history_slots={} seen_categories={:?}",
                configured,
                queue_total,
                history_total,
                unique,
            );
        }

        Ok(out)
    }

    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String> {
        // SAB exposes per-NZB file lists via `mode=get_files&value=<nzo_id>`.
        // The response shape is `{"files":[{"filename":"…","mb":n,"mbleft":n,…}]}`.
        // Returns empty until SAB has parsed the NZB header; matches
        // the trait contract for "metadata not yet ready."
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[("mode", "get_files"), ("value", info_hash)]))
            .send()
            .await
            .map_err(|e| format!("SAB request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SAB get_files HTTP {}", resp.status()));
        }
        let body: GetFilesResponse = resp.json().await.unwrap_or_default();
        Ok(body
            .files
            .into_iter()
            .map(|f| {
                let total_mb = f.mb;
                let left_mb = f.mbleft;
                let progress = if total_mb > 0.0 {
                    ((total_mb - left_mb) / total_mb).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                DownloadFile {
                    name: f.filename,
                    size: (total_mb * 1024.0 * 1024.0) as i64,
                    progress,
                    // SAB doesn't surface a per-file wanted flag (no
                    // selection API). Always-true matches the
                    // FullDownload outcome `add_torrent_with_file_filter`
                    // returns.
                    wanted: true,
                }
            })
            .collect())
    }

    async fn pause(&self, info_hash: &str) -> Result<(), String> {
        self.queue_action("pause", info_hash).await
    }

    async fn resume(&self, info_hash: &str) -> Result<(), String> {
        self.queue_action("resume", info_hash).await
    }

    async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), String> {
        // SAB delete is split between queue and history. The two
        // endpoints disagree about how they signal "nzo_id not
        // present" — and the disagreement runs the opposite way to
        // what an earlier version of this impl assumed (live-probed
        // against SAB 5.0.1 source 2026-05-03):
        //
        //   * `mode=queue&name=delete` → `status: bool(removed)`
        //     where `removed` is the list of nzo_ids actually pulled
        //     from the queue. Unknown nzo_id → empty list →
        //     `status: false`. **Honest signal.**
        //   * `mode=history&name=delete` → `report()` regardless of
        //     whether the nzo_id was found in the history DB. Even
        //     a totally bogus nzo_id returns `status: true`.
        //     **Phantom success.**
        //
        // The earlier impl tried history FIRST under the inverted
        // assumption that queue was the phantom-success endpoint.
        // For in-flight cancels (nzo_id in queue, not history) that
        // history-first path always phantom-succeeded, returned Ok
        // before queue was touched, and the SAB job kept
        // downloading while Ryokan happily marked the grab as
        // removed. Reported 2026-05-03 as "Cancel Pending isn't
        // removing in-flight SAB jobs."
        //
        // Correct order: try queue first (honest signal) → if it
        // says false, fall through to history with `del_files=1`
        // to clean up the imported output dir. `del_files=1` is
        // also passed on the queue call so a partial download's
        // bytes are removed alongside the queue entry.
        let one = "1";
        let zero = "0";
        let del_value = if delete_files { one } else { zero };

        let mut q: Vec<(&str, &str)> = vec![
            ("mode", "queue"),
            ("name", "delete"),
            ("value", info_hash),
            ("del_files", del_value),
        ];
        let body = self.send_delete_probe(&q).await?;
        if body.status {
            return Ok(());
        }

        // Queue didn't have it — try history. Phantom-success here
        // is fine for our purposes: the user asked for the item to
        // be gone, history's `del_files=1` cleans up the unpacked
        // output dir, and a phantom-true on a truly bogus nzo_id is
        // a no-op (nothing to delete).
        q = vec![
            ("mode", "history"),
            ("name", "delete"),
            ("value", info_hash),
            ("del_files", del_value),
        ];
        let body = self.send_delete_probe(&q).await?;
        if body.status {
            Ok(())
        } else {
            Err(format!(
                "SAB delete failed: {}",
                body.error.unwrap_or_else(|| "no error provided".into())
            ))
        }
    }

    async fn set_file_wanted(
        &self,
        _info_hash: &str,
        _files: &[usize],
        _wanted: bool,
    ) -> Result<(), String> {
        // No-op for SAB. Per-file selection isn't part of the public
        // API. See module-level docs.
        Ok(())
    }

    fn sonarr_impl_name(&self) -> &'static str {
        // PR 112 review #1 — Sonarr's canonical name for the SABnzbd
        // download client is `"Sabnzbd"` (PascalCase), matching the
        // BT impls' `"QBittorrent"` / `"Deluge"` / `"Transmission"` /
        // `"RTorrent"`. Lowercase here leaks into the Sonarr/Radarr
        // shim's `/api/v3/downloadclient` `implementation` +
        // `config_contract` payloads, the Settings → API health
        // badge ("sabnzbd 4.5.5"), and `grabbed_torrents.client_kind`
        // — all of which Sonarr would emit in PascalCase.
        "Sabnzbd"
    }

    fn protocol(&self) -> &'static str {
        // PR 112 review #2 — SAB is the only usenet impl; BT
        // impls inherit the `"torrent"` default from the trait.
        // Drives the Sonarr/Radarr shim's `/api/v3/downloadclient`
        // protocol field so a SAB-as-default install reports
        // `"usenet"` correctly.
        "usenet"
    }
}

impl SabClient {
    /// Variant of `add_torrent_paused` that returns the captured
    /// nzo_id. Internal — not part of the trait. Used by the picker's
    /// preview path indirectly via `add_torrent_returning_id`. SAB's
    /// "paused at add" maps to `priority=-1`.
    async fn add_torrent_paused_returning_id(
        &self,
        url: &str,
        _info_hash: &str,
    ) -> Result<(AddOutcome, String), String> {
        // Same first-grab category safety net as the unpaused add
        // path. The picker flow goes through here too; without this
        // the picker's first interactive grab against a fresh SAB
        // would land in the default bucket and disappear from
        // Ryokan's queue tab.
        let just_created_category = self.ensure_category_cached_once().await;

        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[
                ("mode", "addurl"),
                ("name", url),
                ("cat", self.category.as_str()),
                ("priority", "-1"),
            ]))
            .send()
            .await
            .map_err(|e| format!("SAB request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SAB add returned HTTP {}", resp.status()));
        }
        // SAB sometimes returns 200 with an HTML page (not JSON) when
        // the API key is missing — varies by version + URL_BASE
        // config, and the user-pasted-NZB-Key-instead-of-API-Key
        // case is the most common footgun. Read the body as bytes
        // once so we can fall back to a substring check on the
        // well-known "API Key" warning if JSON parsing fails.
        // Without this the user got an opaque parse error instead of
        // an actionable hint.
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("SAB addurl read body failed: {e}"))?;
        let body: AddUrlResponse = match serde_json::from_slice(&body_bytes) {
            Ok(b) => b,
            Err(e) => {
                let body_text = std::str::from_utf8(&body_bytes).unwrap_or_default();
                if body_text.to_ascii_lowercase().contains("api key") {
                    return Err(format!(
                        "SAB rejected addurl: API key missing. Set the API Key field on the SABnzbd download-client row (Settings → Connections → Downloads → click the SAB row). Use SAB's full API Key (not the NZB Key — Ryokan needs queue access too). Find it in SABnzbd → Config → General → Security → API Key. SAB body: {}",
                        body_text.trim()
                    ));
                }
                return Err(format!("SAB addurl parse failed: {e}"));
            }
        };
        if !body.status {
            let raw_error = body.error.unwrap_or_else(|| "no error provided".into());
            if raw_error.to_ascii_lowercase().contains("api key") {
                return Err(format!(
                    "SAB rejected addurl: API key missing or invalid. Use the full API Key (not the NZB Key) from SABnzbd → Config → General → Security. SAB error: {raw_error}"
                ));
            }
            return Err(format!("SAB rejected addurl: {raw_error}"));
        }
        if let Some(id) = body.nzo_ids.into_iter().next() {
            // Same defensive change_cat as the non-paused path.
            self.maybe_change_cat_after_add(just_created_category, &id)
                .await;
            Ok((AddOutcome::Added, id))
        } else {
            let id = self
                .find_id_for_url(url)
                .await
                .ok_or_else(|| "SAB addurl returned no nzo_id".to_string())?;
            Ok((AddOutcome::AlreadyPresent, id))
        }
    }

    async fn fetch_queue(&self) -> Result<QueueBlock, String> {
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[("mode", "queue")]))
            .send()
            .await
            .map_err(|e| format!("SAB request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SAB queue HTTP {}", resp.status()));
        }
        let body: QueueResponse = resp
            .json()
            .await
            .map_err(|e| format!("SAB queue parse failed: {e}"))?;
        Ok(body.queue)
    }

    async fn fetch_history(&self) -> Result<HistoryBlock, String> {
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[("mode", "history")]))
            .send()
            .await
            .map_err(|e| format!("SAB request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SAB history HTTP {}", resp.status()));
        }
        let body: HistoryResponse = resp
            .json()
            .await
            .map_err(|e| format!("SAB history parse failed: {e}"))?;
        Ok(body.history)
    }

    /// SAB's pre-queue dup detection returns an empty `nzo_ids` array
    /// on `mode=addurl` for an already-known URL. Find which slot it
    /// matches by scanning queue + history. URL match is best-effort —
    /// SAB doesn't always echo the original NZB URL on queue slots
    /// (it sometimes replaces with a normalized form), so the match
    /// degrades gracefully: empty result → caller surfaces an error.
    async fn find_id_for_url(&self, url: &str) -> Option<String> {
        let url_norm = url.trim().to_ascii_lowercase();
        if let Ok(queue) = self.fetch_queue().await {
            for slot in queue.slots {
                if slot.url.to_ascii_lowercase() == url_norm {
                    return Some(slot.nzo_id);
                }
            }
        }
        if let Ok(history) = self.fetch_history().await {
            for slot in history.slots {
                if slot.url.to_ascii_lowercase() == url_norm {
                    return Some(slot.nzo_id);
                }
            }
        }
        None
    }

    /// One leg of the two-leg delete (history first, then queue
    /// fallback). Mirrors the auth-aware error path that `add_torrent`
    /// uses: a 401/403 response captures SAB's plain-text body so the
    /// user sees `API Key Incorrect` instead of the unhelpful
    /// `SAB delete failed: no error provided` that comes out when
    /// `resp.json().unwrap_or_default()` parses a non-JSON error page
    /// into an empty `StatusResponse`.
    async fn send_delete_probe(&self, params: &[(&str, &str)]) -> Result<StatusResponse, String> {
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(params))
            .send()
            .await
            .map_err(|e| format!("SAB request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = if matches!(status.as_u16(), 401 | 403) {
                resp.text().await.unwrap_or_default()
            } else {
                String::new()
            };
            let trimmed = detail.trim();
            if trimmed.is_empty() {
                return Err(format!("SAB delete returned HTTP {status}"));
            }
            return Err(format!("SAB delete returned HTTP {status}: {trimmed}"));
        }
        Ok(resp.json().await.unwrap_or_default())
    }

    async fn queue_action(&self, name: &str, nzo_id: &str) -> Result<(), String> {
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[("mode", "queue"), ("name", name), ("value", nzo_id)]))
            .send()
            .await
            .map_err(|e| format!("SAB request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SAB queue {name} HTTP {}", resp.status()));
        }
        let body: StatusResponse = resp.json().await.unwrap_or_default();
        if body.status {
            Ok(())
        } else {
            Err(format!(
                "SAB queue {name} failed: {}",
                body.error.unwrap_or_else(|| "no error provided".into())
            ))
        }
    }

    /// Probe SAB's configured category list. SAB returns
    /// `{"categories":["*","books","movies",...]}` — the leading `"*"`
    /// is the catch-all default bucket and is included in the list.
    /// Empty list means SAB returned a malformed body (or auth was
    /// stripped at a proxy); surfaced as an error rather than silently
    /// reporting "no categories" so callers can distinguish it from a
    /// genuine "user has no custom categories" reply (which still
    /// includes `"*"`).
    async fn get_cats(&self) -> Result<Vec<String>, String> {
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[("mode", "get_cats")]))
            .send()
            .await
            .map_err(|e| format!("SAB get_cats failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SAB get_cats HTTP {}", resp.status()));
        }
        let body: GetCatsResponse = resp
            .json()
            .await
            .map_err(|e| format!("SAB get_cats parse failed: {e}"))?;
        if body.categories.is_empty() {
            return Err(
                "SAB get_cats returned an empty list — likely a proxy stripping the apikey or a malformed response"
                    .into(),
            );
        }
        Ok(body.categories)
    }

    /// Ensure the configured category exists in SAB, creating it via
    /// `mode=set_config` when missing. Returns `Ok(true)` when a new
    /// category was created, `Ok(false)` when it already existed (or
    /// when `self.category` is empty — the user opted out of
    /// per-Ryokan filtering at the row level), `Err(...)` on any API
    /// error.
    ///
    /// Why this exists: without it, a user who configures Ryokan with
    /// category `"anime"` but whose SAB has no such category gets
    /// jobs landing in SAB's default bucket with whatever category
    /// SAB-side rules dictate (often `"default"` or the indexer's
    /// pre-bound name). Ryokan's `list_scoped` then filters them out
    /// and the user sees "my SAB grabs disappear" — exactly the
    /// 2026-05-02 report. Auto-create makes the two ends agree by
    /// default; the explicit Test-connection path surfaces what
    /// happened in the toast so it isn't a silent mutation.
    ///
    /// Creates with `dir=<category>` so the new category gets its own
    /// subfolder under SAB's complete dir; everything else (priority,
    /// post-processing pipeline, scripts) inherits SAB's defaults.
    /// Users who want a non-default folder can edit the category in
    /// SAB's UI after creation — Ryokan won't overwrite it on
    /// subsequent Test clicks because `get_cats` will then find it.
    async fn ensure_category(&self) -> Result<bool, String> {
        let configured = self.category.trim();
        if configured.is_empty() {
            return Ok(false);
        }
        let cats = self.get_cats().await?;
        if cats.iter().any(|c| c.eq_ignore_ascii_case(configured)) {
            return Ok(false);
        }

        // Category missing — create it. SAB's `set_config` requires
        // the **full** API key (not the read-only `nzb_api_key`).
        // 401/403 here surfaces with a hint so the user knows to
        // swap keys instead of chasing a phantom permission bug.
        //
        // Pass both `keyword` and `name` for cross-version safety.
        // SAB 5.x's API doc (live-probed 2026-05-02 against 5.0.1)
        // uses `name=…` for the category identifier; older 4.x
        // installs used `keyword=…`. SAB ignores unknown params,
        // so passing both works on any current version. `dir`
        // gets the same value so the new category gets its own
        // subfolder under SAB's complete dir; everything else
        // (priority, post-processing pipeline, scripts) inherits
        // SAB's defaults.
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[
                ("mode", "set_config"),
                ("section", "categories"),
                ("keyword", configured),
                ("name", configured),
                ("dir", configured),
            ]))
            .send()
            .await
            .map_err(|e| format!("SAB set_config failed: {e}"))?;
        let status = resp.status();
        if matches!(status.as_u16(), 401 | 403) {
            let detail = resp.text().await.unwrap_or_default();
            let trimmed = detail.trim();
            return Err(format!(
                "SAB rejected category creation (HTTP {}{}); the configured API key may be the read-only nzb_key. Use the full API key on the SABnzbd download-client row.",
                status,
                if trimmed.is_empty() {
                    String::new()
                } else {
                    format!(": {trimmed}")
                },
            ));
        }
        if !status.is_success() {
            return Err(format!(
                "SAB set_config HTTP {} when creating category '{}'",
                status, configured
            ));
        }
        // Some SAB versions return `{"status":true}`, others echo back
        // the new category config object directly. The HTTP success
        // status was already verified above; don't gate on body shape.
        Ok(true)
    }

    /// One-shot wrapper around [`ensure_category`] for the add path.
    /// First call per `SabClient` lifetime probes SAB; subsequent
    /// calls early-return. Returns `true` ONLY when this call
    /// actually created the category — callers use that signal to
    /// fire a defensive `change_cat` on the just-added job (see the
    /// add-path comments for the SAB config-reload race motivation).
    ///
    /// Failure here is silent (logged, not propagated): the
    /// connection works for adding, categories are a secondary
    /// concern, and a permanent failure (read-only `nzb_key`)
    /// shouldn't break every grab. The Test-connection toast is
    /// the explicit user-facing surface for these errors.
    async fn ensure_category_cached_once(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.category_ensured.load(Ordering::Relaxed) {
            return false;
        }
        let result = self.ensure_category().await;
        // Set the flag regardless of outcome — on permanent failure
        // (read-only key) retrying every grab would just spam the
        // log; on transient failure (network blip mid-grab) a
        // restart re-attempts. The user-facing fallback is to click
        // Test in Settings, which bypasses this cache.
        self.category_ensured.store(true, Ordering::Relaxed);
        match result {
            Ok(true) => {
                tracing::info!(
                    target: "ryokan::sabnzbd",
                    category = %self.category,
                    "SAB add path created missing category"
                );
                true
            }
            Ok(false) => false,
            Err(e) => {
                tracing::warn!(
                    target: "ryokan::sabnzbd",
                    category = %self.category,
                    error = %e,
                    "SAB ensure_category failed (will retry next process restart)"
                );
                false
            }
        }
    }

    /// Helper for the add path: fire `change_cat` against the
    /// just-added job iff `ensure_category_cached_once` reported
    /// that we created the category in this call. No-op otherwise
    /// (already-present category, empty configured category, or
    /// ensure_category errored — log already emitted at the source).
    /// Failures here are also logged but never propagate; the add
    /// itself succeeded and a vanish-from-view symptom for one grab
    /// is preferable to crashing the grab path.
    async fn maybe_change_cat_after_add(&self, just_created: bool, nzo_id: &str) {
        if !just_created || nzo_id.is_empty() {
            return;
        }
        if let Err(e) = self.change_cat(nzo_id, &self.category).await {
            tracing::warn!(
                target: "ryokan::sabnzbd",
                category = %self.category,
                nzo_id = %nzo_id,
                error = %e,
                "SAB change_cat after auto-create failed; the just-added job may be in SAB's default bucket. Manual fix: re-tag in SAB's UI, or restart Ryokan to retry."
            );
        }
    }

    /// Re-tag an existing queue job with the configured category.
    /// Used by the add path right after `addurl` when we just
    /// created the category in this same call: SAB's set_config
    /// writes to its config file, and there's no documented
    /// guarantee the change is visible to the very next addurl
    /// call. Defensive `change_cat` ensures the just-added slot
    /// actually carries the category — without it, a fresh
    /// install's first grab might land in SAB's default bucket
    /// despite passing `cat=anime` on the addurl, repeating the
    /// exact failure mode we set out to fix.
    ///
    /// `mode=change_cat&value=<nzo_id>&value2=<cat>` is the
    /// queue-only endpoint per SAB's 5.0 API docs (live-probed
    /// 2026-05-02). History items can't be re-tagged through the
    /// API; for the add-path use case the just-added job is in
    /// queue, so this is the right tool.
    async fn change_cat(&self, nzo_id: &str, cat: &str) -> Result<(), String> {
        let resp = self
            .http
            .get(self.endpoint())
            .query(&self.make_query(&[("mode", "change_cat"), ("value", nzo_id), ("value2", cat)]))
            .send()
            .await
            .map_err(|e| format!("SAB change_cat failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SAB change_cat HTTP {}", resp.status()));
        }
        let body: StatusResponse = resp.json().await.unwrap_or_default();
        if body.status {
            Ok(())
        } else {
            Err(format!(
                "SAB change_cat: {}",
                body.error.unwrap_or_else(|| "no error provided".into())
            ))
        }
    }
}

// ── Wire-format response shapes ─────────────────────────────────────

#[derive(Deserialize)]
struct VersionResponse {
    version: String,
}

#[derive(Deserialize, Default)]
struct GetCatsResponse {
    #[serde(default)]
    categories: Vec<String>,
}

#[derive(Deserialize, Default)]
struct AddUrlResponse {
    #[serde(default)]
    status: bool,
    #[serde(default)]
    nzo_ids: Vec<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct StatusResponse {
    #[serde(default)]
    status: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct QueueResponse {
    #[serde(default)]
    queue: QueueBlock,
}

#[derive(Deserialize, Default)]
struct QueueBlock {
    #[serde(default)]
    slots: Vec<QueueSlot>,
}

#[derive(Deserialize)]
struct QueueSlot {
    nzo_id: String,
    filename: String,
    #[serde(default)]
    cat: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    percentage: String,
    #[serde(default)]
    kbpersec: String,
    #[serde(default)]
    timeleft: String,
    #[serde(default)]
    url: String,
}

#[derive(Deserialize, Default)]
struct HistoryResponse {
    #[serde(default)]
    history: HistoryBlock,
}

#[derive(Deserialize, Default)]
struct HistoryBlock {
    #[serde(default)]
    slots: Vec<HistorySlot>,
}

#[derive(Deserialize)]
struct HistorySlot {
    nzo_id: String,
    name: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    storage: String,
    #[serde(default)]
    bytes: i64,
    #[serde(default)]
    url: String,
}

#[derive(Deserialize, Default)]
struct GetFilesResponse {
    #[serde(default)]
    files: Vec<GetFilesEntry>,
}

#[derive(Deserialize, Default)]
struct GetFilesEntry {
    #[serde(default)]
    filename: String,
    #[serde(default)]
    mb: f64,
    #[serde(default)]
    mbleft: f64,
}

// ── Wire-format helpers ─────────────────────────────────────────────

/// Resolve SAB's `storage` field to the canonical per-job folder.
/// SAB's reported storage takes one of three shapes:
///
///   1. **The per-job folder itself** (`/complete/<title>/`). This
///      is the typical case. Detected by the basename equalling
///      the job title; we return it as-is.
///   2. **A file path inside the job folder**
///      (`/complete/<title>/file.mkv`). Single-file extracts on some
///      SAB versions. We use [`title_matching_ancestor`] to walk up
///      and find the title-named parent dir.
///   3. **The parent complete dir alone** (`/complete/`). Edge case
///      observed in the wild — happens when SAB couldn't determine
///      where extraction landed. We construct `<storage>/<title>/`
///      as a candidate. This won't be filesystem-checked here (we
///      operate on SAB-internal paths in Docker setups, where
///      `is_dir()` against Ryokan's filesystem would lie); the
///      downstream import walk handles the "path doesn't exist"
///      case naturally.
///
/// The narrowing is critical: without it, `import_torrent`'s
/// `walk_video_files` would walk SAB's WHOLE complete dir and sweep
/// in files belonging to OTHER grabs, then stamp them onto this
/// grab's `imported_source_paths`. Deleting one episode would then
/// remove sibling episodes' source files too — see the user-reported
/// 1154/1156 bug.
fn canonical_job_path(storage: &str, title: &str) -> String {
    if storage.is_empty() {
        return String::new();
    }
    if title.is_empty() {
        return storage.to_string();
    }
    // Case 1: storage already IS the per-job dir.
    if let Some(name) = std::path::Path::new(storage)
        .file_name()
        .and_then(|n| n.to_str())
        && name == title
    {
        return storage.to_string();
    }
    // Case 2: storage points inside the per-job dir somewhere.
    if let Some(found) = title_matching_ancestor(storage, title) {
        return found;
    }
    // Case 3: storage is the parent root (no per-job context).
    // Construct the candidate; if SAB really created a per-job
    // subdir named after the title, this resolves to it.
    std::path::PathBuf::from(storage)
        .join(title)
        .display()
        .to_string()
}

/// Walk parents of `path` looking for an ancestor whose filename
/// equals `title`. Returns the **outermost** matching ancestor as a
/// string, or `None` if no ancestor matches.
///
/// Ported from Sonarr's `Sabnzbd.cs::GetHistory` (the inner `while`
/// loop ascending parents and matching against `sabHistoryItem.Title`).
/// Used by [`canonical_job_path`] to handle the case where SAB's
/// `storage` field points at a file inside the job folder rather
/// than the folder itself.
///
/// The "outermost match wins" semantic mirrors Sonarr's behavior:
/// the loop overwrites OutputPath each match and keeps walking up,
/// so for a path like `/complete/<title>/<title>/file.mkv` the
/// higher-up `<title>/` directory wins. Real-world archives rarely
/// nest this way, but the chosen behavior is defensive.
fn title_matching_ancestor(path: &str, title: &str) -> Option<String> {
    if title.is_empty() || path.is_empty() {
        return None;
    }
    let mut found: Option<std::path::PathBuf> = None;
    let mut current = std::path::Path::new(path).parent();
    while let Some(dir) = current {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str())
            && name == title
        {
            found = Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    found.map(|p| p.display().to_string())
}

/// SAB returns percentages as strings ("47" → 0.47). Bare-int
/// formatting on most installs; clamp to 0..=1.0.
fn parse_percentage(s: &str) -> f64 {
    s.trim()
        .parse::<f64>()
        .map(|n| (n / 100.0).clamp(0.0, 1.0))
        .unwrap_or(0.0)
}

/// SAB's `kbpersec` on queue slots is a string like `"1234.5"` (KiB/s).
/// Map to bytes/sec for the trait's `dlspeed: i64` field.
fn parse_speed_bytes(s: &str) -> i64 {
    s.trim()
        .parse::<f64>()
        .map(|kb| (kb * 1024.0) as i64)
        .unwrap_or(0)
}

/// SAB's `timeleft` is `"H:MM:SS"` or `"0:00:00"`. Total seconds for
/// the trait's `eta: i64`. Unknown shapes (e.g. SAB's "unknown" string
/// when no estimate is available) collapse to 0.
fn parse_eta_seconds(s: &str) -> i64 {
    let parts: Vec<&str> = s.trim().split(':').collect();
    match parts.len() {
        3 => {
            let h: i64 = parts[0].parse().unwrap_or(0);
            let m: i64 = parts[1].parse().unwrap_or(0);
            let sec: i64 = parts[2].parse().unwrap_or(0);
            h * 3600 + m * 60 + sec
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_appends_api_to_bare_base() {
        // No-prefix install (linuxserver/sabnzbd, Ubuntu .deb, most
        // bare installs). Live-probed 2026-04-27.
        let c = SabClient::new("http://host:8080", "", "k", "anime");
        assert_eq!(c.endpoint(), "http://host:8080/api");
    }

    #[test]
    fn endpoint_appends_api_when_base_has_url_base_prefix() {
        // Install with SAB's `URL_BASE = /sabnzbd` config. User
        // provides the full prefix; impl just appends `/api`.
        let c = SabClient::new("http://host:8080/sabnzbd", "", "k", "anime");
        assert_eq!(c.endpoint(), "http://host:8080/sabnzbd/api");
    }

    #[test]
    fn endpoint_strips_trailing_slash_from_base() {
        let c = SabClient::new("http://host:8080/", "", "k", "anime");
        assert_eq!(c.endpoint(), "http://host:8080/api");
    }

    /// SAB's history `storage` field for a single-file extract often
    /// points at the file inside the job folder. The title-matching
    /// ancestor walk recovers the per-job folder so the import walk
    /// scopes correctly. Without this, when SAB reports the parent
    /// `complete/` dir or a file path, downstream code walked too
    /// broadly and stamped/imported files from OTHER grabs in the
    /// same complete dir.
    #[test]
    fn title_matching_ancestor_finds_job_dir_when_storage_is_a_file_path() {
        let storage = "/downloads/complete/MyJob.S01E01/file.mkv";
        let title = "MyJob.S01E01";
        let got = title_matching_ancestor(storage, title);
        assert_eq!(got, Some("/downloads/complete/MyJob.S01E01".to_string()));
    }

    /// When SAB's `storage` IS the per-job folder already, the walk
    /// finds that folder one level up from the supplied path's
    /// children — but since we walk *parents* of the input, the input
    /// itself isn't checked. Returns None; caller falls back to using
    /// `storage` directly.
    #[test]
    fn title_matching_ancestor_returns_none_when_storage_is_already_job_dir() {
        let storage = "/downloads/complete/MyJob";
        let title = "MyJob";
        // The walk inspects PARENTS of storage; the path itself is the
        // job dir but isn't tested. Caller's fallback chain handles
        // this case via direct use of `storage`.
        assert_eq!(title_matching_ancestor(storage, title), None);
    }

    /// Pathological case: SAB reports `storage` as the parent
    /// complete dir with no per-job subdir. Walk finds no match
    /// (no ancestor named after Title). Caller falls through to the
    /// `<storage>/<title>/` candidate or to `storage` as-is — both
    /// safer than walking the parent dir.
    #[test]
    fn title_matching_ancestor_returns_none_for_parent_only_storage() {
        let storage = "/downloads/complete";
        let title = "MyJob.S01E01";
        assert_eq!(title_matching_ancestor(storage, title), None);
    }

    /// Multi-match: the OUTERMOST title-named ancestor wins (matches
    /// Sonarr's behavior — the loop assignment overwrites and keeps
    /// ascending). Defensive against unusual archive shapes.
    #[test]
    fn title_matching_ancestor_picks_outermost_match() {
        let storage = "/downloads/MyJob/inner/MyJob/file.mkv";
        let title = "MyJob";
        assert_eq!(
            title_matching_ancestor(storage, title),
            Some("/downloads/MyJob".to_string())
        );
    }

    /// Empty title or empty path: no walk, returns None.
    #[test]
    fn title_matching_ancestor_handles_empty_inputs() {
        assert_eq!(title_matching_ancestor("", "MyJob"), None);
        assert_eq!(title_matching_ancestor("/a/b", ""), None);
    }

    /// Per-job dir already as storage — return as-is. The basename
    /// equals the title so we know it IS the job folder.
    #[test]
    fn canonical_job_path_uses_storage_when_basename_matches_title() {
        assert_eq!(
            canonical_job_path("/downloads/complete/MyJob.S01E01", "MyJob.S01E01"),
            "/downloads/complete/MyJob.S01E01"
        );
    }

    /// File path inside the job folder — walk finds the job dir.
    #[test]
    fn canonical_job_path_recovers_job_dir_from_file_path() {
        assert_eq!(
            canonical_job_path("/downloads/complete/MyJob.S01E01/file.mkv", "MyJob.S01E01"),
            "/downloads/complete/MyJob.S01E01"
        );
    }

    /// Parent-only storage — construct a candidate by joining title.
    /// Pinned by the user-reported 1154/1156 bug: when SAB returns
    /// the bare complete dir, downstream walks must scope to a
    /// per-job candidate or sibling grabs' files get swept up too.
    #[test]
    fn canonical_job_path_constructs_candidate_for_parent_only_storage() {
        assert_eq!(
            canonical_job_path("/downloads/complete", "MyJob.S01E01"),
            "/downloads/complete/MyJob.S01E01"
        );
    }

    /// Empty inputs degrade gracefully — no panic, sensible defaults.
    #[test]
    fn canonical_job_path_handles_empty_inputs() {
        assert_eq!(canonical_job_path("", "MyJob"), "");
        assert_eq!(
            canonical_job_path("/downloads/complete", ""),
            "/downloads/complete"
        );
    }

    #[test]
    fn parse_percentage_handles_int_string() {
        assert_eq!(parse_percentage("0"), 0.0);
        assert_eq!(parse_percentage("47"), 0.47);
        assert_eq!(parse_percentage("100"), 1.0);
        assert_eq!(parse_percentage(""), 0.0);
    }

    #[test]
    fn parse_percentage_clamps_above_100() {
        // SAB occasionally reports >100 mid-extract; clamp so the
        // progress bar doesn't overflow.
        assert_eq!(parse_percentage("150"), 1.0);
    }

    #[test]
    fn parse_speed_bytes_converts_kib_to_bytes() {
        assert_eq!(parse_speed_bytes("1024"), 1024 * 1024);
        assert_eq!(parse_speed_bytes("0"), 0);
        assert_eq!(parse_speed_bytes(""), 0);
    }

    #[test]
    fn parse_eta_seconds_handles_h_mm_ss() {
        assert_eq!(parse_eta_seconds("0:00:00"), 0);
        assert_eq!(parse_eta_seconds("0:01:30"), 90);
        assert_eq!(parse_eta_seconds("2:30:00"), 9000);
        // Unknown / non-clock shapes collapse to 0.
        assert_eq!(parse_eta_seconds("unknown"), 0);
        assert_eq!(parse_eta_seconds(""), 0);
    }

    #[test]
    fn map_state_routes_known_strings_through_the_normalized_enum() {
        assert_eq!(
            SabClient::map_state("Downloading", false),
            DownloadItemState::Downloading
        );
        assert_eq!(
            SabClient::map_state("Queued", false),
            DownloadItemState::DownloadingQueued
        );
        assert_eq!(
            SabClient::map_state("Paused", false),
            DownloadItemState::Paused
        );
        assert_eq!(
            SabClient::map_state("Verifying", false),
            DownloadItemState::CheckingDownload
        );
        assert_eq!(
            SabClient::map_state("Completed", true),
            DownloadItemState::PausedComplete
        );
        assert_eq!(
            SabClient::map_state("Failed", true),
            DownloadItemState::Errored
        );
    }

    #[test]
    fn sonarr_impl_name_is_sabnzbd() {
        // PR 112 review #1 — Sonarr's canonical name for the
        // SABnzbd download client is `"Sabnzbd"` (PascalCase),
        // matching the BT impls' PascalCase pattern. Pin so a
        // regression doesn't revert to lowercase and silently
        // change the Settings badge / shim payload / grab
        // `client_kind` rendering.
        let c = SabClient::new("http://localhost:8080", "", "key", "");
        assert_eq!(c.sonarr_impl_name(), "Sabnzbd");
    }

    #[test]
    fn map_state_unknown_history_status_surfaces_as_errored() {
        // History rows in odd post-proc states ("Repair Failed",
        // "Move Failed") would import broken data if treated as
        // complete. Errored makes post-processing skip them.
        assert_eq!(
            SabClient::map_state("Repair Failed", true),
            DownloadItemState::Errored
        );
    }

    /// Live smoke test against a real SAB. Skipped unless
    /// `RYOKAN_SAB_E2E=1` and the corresponding env vars are set.
    /// Mirrors the same gate the BT impls use for their e2e tests
    /// — CI never runs this; it's for hand-validation when touching
    /// the SAB impl.
    #[tokio::test]
    #[ignore]
    async fn live_smoke() {
        if std::env::var("RYOKAN_SAB_E2E").as_deref() != Ok("1") {
            return;
        }
        let url =
            std::env::var("RYOKAN_SAB_URL").unwrap_or_else(|_| "http://127.0.0.1:8085".to_string());
        let key = std::env::var("RYOKAN_SAB_API_KEY")
            .expect("RYOKAN_SAB_API_KEY must be set for live smoke");
        let cat = std::env::var("RYOKAN_SAB_CAT").unwrap_or_else(|_| "anime".to_string());
        let client = SabClient::new(&url, "", &key, &cat);
        let version = client.test().await.expect("test() must succeed");
        eprintln!("connected: {version}");
        let items = client
            .list_scoped()
            .await
            .expect("list_scoped must succeed");
        eprintln!("scoped items: {}", items.len());
    }
}
