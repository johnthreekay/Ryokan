# services/download_client/AGENTS.md

`DownloadClient` is the trait abstraction over **five supported clients**: four BT clients (qBittorrent, Deluge, Transmission, rTorrent) and one Usenet client (SABnzbd). Multiple clients can be configured simultaneously and routed per-grab. Concrete impls live in `qbittorrent/`, `deluge/`, `transmission/`, `rtorrent/`, `sabnzbd/`, each with a `wiremock_tests/` sibling directory of HTTP-mock tests (distinct from the env-gated `live_smoke` tests in the parent `mod.rs`).

## Trait identity contract

- BT clients address torrents by **v1 infohash, lowercase hex at the trait boundary** — each impl case-munges internally for its wire format.
- SABnzbd has no infohash. SAB hands back an opaque **`nzo_id`** (e.g. `"SABnzbd_nzo_abc123def"`) when an NZB is added, and every subsequent op keys off that. There's no formula to derive `nzo_id` from the NZB URL.
- The trait method **`add_torrent_returning_id`** captures whatever opaque id the wire format uses and returns it — BT impls return the precomputed infohash unchanged; SAB returns the captured `nzo_id`. Callers persist the returned id on `grabbed_torrents.hash`. Subsequent ops receive that string as the trait's `info_hash` parameter and use it verbatim. Pattern lifted from Sonarr's `Download(...) -> string`.
- The 40-char-hex contract only applies inside the four BT impls' add paths.
- The `pick` callback in `add_torrent_with_file_filter` is `&mut dyn FnMut` (not generic) to keep the trait object-safe for `Arc<dyn DownloadClient>` storage on `AppState`.

## Multi-client routing pool

`AppState.download_clients` is a `DownloadClientsCache = Arc<RwLock<Arc<DownloadClientPool>>>`. The pool holds `clients: HashMap<i64, Arc<dyn DownloadClient>>` keyed by `download_clients.id`, plus `default_torrent_id` and `default_usenet_id` (the `is_default = 1` rows scoped per protocol — both can coexist).

Resolution helpers on `AppState`:

- `client_for_indexer(indexer_id)` — indexer's `download_client_id` pin → per-protocol default → None.
- `client_for_nyaa(nyaa_pin)` — `config.nyaa_download_client_id` → torrent default. **Always falls back to torrent** (Nyaa items are magnets / .torrent URLs).
- `default_download_client()` — returns the **torrent** default. Every internal default-only call site is torrent-flavored; usenet routing always goes through an indexer pin or its protocol default.
- `client_by_id(id)` — direct lookup; returns None if the row was deleted from the pool mid-request.
- `resolve_grab_client(download_client_id, hash)` — used by post-processing's per-grab routing. Three-layer fallback: stamped id → SAB hash-shape heuristic (`hash.starts_with("SABnzbd_nzo_")` routes to ANY usenet client in the pool) → torrent default. **The hash-shape heuristic is load-bearing** for grabs predating the `download_client_id` stamp migration: without it, a NULL-stamped SAB nzo_id falls through to qBit's `delete` endpoint, qBit silently 200s on unknown hashes, and the symptom is "delete-from-disk leaves the SAB job alive forever."

`grabbed_torrents.download_client_id` is stamped at grab time so post-processing routes back through the same client even after defaults change. Don't introduce a "current active client" abstraction — the single-slot pre-pool shape is gone deliberately.

## `services::download_client::rebuild_clients_cache`

Call this from any handler that mutates the `download_clients` table (Settings → Connections add/edit/delete) so the pool sees the change. Reads all rows, builds an `Arc<dyn DownloadClient>` for each enabled row via the per-kind dispatcher, captures per-protocol defaults, and atomic-swaps the `Arc<DownloadClientPool>`.

## Per-client scoping

Every impl has a distinct "things Ryokan added" filter so `list_scoped` never returns items from other tooling:

- **qBit**: `?category=<config.qbit_category>` (default `anime`)
- **Deluge**: Label plugin (auto-enabled on first connect; see Deluge quirks)
- **Transmission**: native labels on 4.x, save-path prefix fallback on older
- **rtorrent**: `custom1` field (the ruTorrent label convention)
- **SAB**: `cat=<label>` — same field doubles as the post-processing target directory selector; mirrors the qBit category convention

Set at add-time, read at list-time. The label / category / custom1 string comes from the `download_clients` row, NOT a global setting — multi-client setups can give each client its own label.

## Per-client `download_path` + `translate_client_path`

Ryokan and the client don't always see the same filesystem (Docker volumes on different host paths, seedbox over SSHFS/NFS/rclone). Each client gets its own `{qbit,deluge,transmission,rtorrent}_download_path` config field; `per_client_download_path(&config)` resolves the right one via `active_client`.

`translate_client_path(path, client_save_path, local_download_path)` rewrites a client-reported path by replacing the client's `save_path` prefix with Ryokan's local mount. Trailing slashes normalized; empty `local_download_path` = no rewrite; a path that doesn't start with `client_save_path` is **returned unchanged** rather than silently rewritten — silent rewrite would mask misconfiguration as a later "file not found."

Do not reintroduce a single shared global remote-path mapping.

## Selective downloads — two flows

There are two ways to add a torrent and pick a subset of files. Don't conflate them.

**1. Automated (callback): `add_torrent_with_file_filter`.** Pauses the torrent, waits for metadata, runs the caller's `pick` closure over the file names, sets non-picked files to skip, resumes. Used from auto-search's batch-with-selective branch and `library/search/grab.rs`. **10s** metadata ceiling (the user is waiting). Each impl handles its own wait-and-narrow loop and **must be idempotent on retry**: read each file's `wanted` flag back before changing it so a re-narrow doesn't clobber user edits. The `pick` callback is `&mut dyn FnMut` to keep the trait object-safe.

**2. Interactive (preview→confirm): `add_torrent_paused` + `get_files` + `set_file_wanted` + `resume`.** Used by the grab picker (`handlers/grab.rs:329`). Preview adds the torrent paused, `get_files` lists what's inside, the user picks via the modal UI, confirm calls `set_file_wanted` then `resume`. Reads through `models::pending_grabs` (the preview persists rows there until confirmed/cancelled). The grab-sweep task GC's previews after `HEARTBEAT_TTL_SECS + SWEEP_INTERVAL ≈ 2 min` of inactivity. SAB has a private `add_torrent_paused_returning_id` so the impl can capture the `nzo_id` from the paused-add path.

Distinct from `services::auto_expand` (sibling-series detection inside a batch pack — different problem, different code path).

## Seed rules and `seeding_done` (#28, #228)

`set_seed_rules(hash, SeedRules { ratio, time_minutes })` is called by `apply_indexer_seed_rules` right after an add from an indexer row that has a Seed Ratio / Seed Time; `grabbed_torrents.respect_seed_rules` is set whenever rules were *attempted*, wire success or not, and every Ryokan-initiated client delete (episode delete, series remove, upgrade replace) skips a torrent carrying it. The #228 removal paths are the deliberate exception: `seeding_done` means the client's own rule is satisfied, and a move-mode import has nothing left to seed. What each impl can honor:

| Client | ratio | time_minutes | `seeding_done` |
|---|---|---|---|
| qBit | `setShareLimits ratioLimit` | `seedingTimeLimit` (minutes) | `pausedUP` / `stoppedUP` **and** `ratio >= max_ratio`, `seeding_time >= max_seeding_time*60`, or `now - last_activity >= max_inactive_seeding_time*60`; the `max_*` fields are the *effective* limits (-1 = none) |
| Transmission | `seedRatioMode=1` + `seedRatioLimit` | `seedIdleMode=1` + `seedIdleLimit`, which is **inactivity** minutes, not total seed time (never stops earlier than N minutes, can stop later) | `isFinished` (requested in the `torrent-get` field list), **or** stopped + complete + `uploadRatio` at the effective ratio limit (`seedRatioMode` 1 → `seedRatioLimit`, 0 → the daemon's `seedRatioLimit` from a best-effort `session-get`, 2 → never). 4.x sets `finished` for idle **or** ratio stops; 3.x only for idle, hence the ratio arithmetic (`tx_seeding_done`) |
| Deluge | `stop_at_ratio=true` + `stop_ratio` | not supported (debug log) | `Paused` + `is_finished` + `stop_at_ratio` + `ratio >= stop_ratio`; Deluge copies the global `stop_seed_*` config into each torrent's options at add time so these are effective values |
| rTorrent | **not supported**: the only per-item ratio command is the read-only `d.ratio`; ratio handling is per group in `.rtorrent.rc` (`group.seeding.ratio.enable`, `group2.seeding.ratio.max.set`, action in `group.seeding.ratio.command`). `set_seed_rules` returns `Err` without an RPC call (the earlier `d.ratio.max.set` did not exist and faulted on every grab) | same | `d.complete && !d.is_open && !d.is_active && d.ignore_commands && d.message == ""`: the default group action is `d.try_close= ; d.ignore_commands.set=1`, and the ignore flag is what tells a ratio close from a ruTorrent Stop (open) or a restart (closed, no flag); an item with a message is the errored shape. `d.ignore_commands=` is the optional 14th multicall column |
| SAB | n/a | n/a | always `false`; usenet leaves at import |

`DownloadItem::seeding_done` is what post-processing's finished-seed sweep acts on; it must only be true when the client itself ended seeding, never for a plain user pause or stop, because the sweep deletes the item with files. The switch is per client: `download_clients.remove_completed` (default on), set through `set_remove_completed` rather than the upsert form.

## qBit quirks (`qbittorrent/mod.rs`)

- `content_path` is exposed natively (≥2.6.1) — no common-prefix computation.
- File-priority scale is **0/1/6/7**; Ryokan only writes 0 (skip) or 1 (normal).
- qBit 5.x renamed pause/resume → stop/start. Impl tries the new names first and falls back to old without a version probe so 4.x and 5.x both work.
- **qBit 5.x duplicate-add returns `200 "Fails."`** indistinguishable from the body it uses for a malformed magnet. `add_torrent` disambiguates by probing `/torrents/info?hashes=<hash>` after a `Fails.` and reports `AddOutcome::AlreadyPresent` when the hash is in the session. Without this, every re-grab of an already-present torrent (RSS re-emissions, upgrade-sweep collisions, post-crash regrabs) hard-fails.
- `list_scoped` uses a 2s coalescing cache with single-flight election via `AtomicBool` + `Notify` + RAII `FetchFlightGuard`. The guard clears the in-flight flag on drop including the panic path so a panic inside the fetcher can't wedge the flag forever.
- Re-auth on 403 via session cookie.
- **`setShareLimits` sentinels are `-2` = use the global limit, `-1` = no limit** (per the WebUI API docs). Before #228 the impl sent `-1` for every unset dimension, which switched off the user's global seeding-time and inactivity limits on each grab from a ratio-only indexer. `torrents/info` also carries `ratio`, `max_ratio`, `seeding_time` (seconds), `max_seeding_time` and `max_inactive_seeding_time` (minutes), `last_activity` (unix); all `#[serde(default)]` to "no limit" for old builds.
- **When grabs vanish silently**: qBit's `POST /torrents/add` returns `Ok.` and fetches the `.torrent` async server-side. A silent fetch failure (tracker timeout, 404, etc.) masquerades as a Ryokan bug — check qBit's own logs first.

## Deluge quirks (`deluge/mod.rs`)

- **Two-step connect.** `auth.login(password)` establishes a session cookie but the freshly-authenticated session isn't connected to any daemon; every `core.*` call fails with `"Unknown method"` (NOT "not connected" — methods aren't even registered on the web process) until `web.connect(host_id)` runs. The single most common first-time integration failure.
- **Label plugin required for scoping.** Bundled but disabled by default; the connection test enables it via `core.enable_plugin` when it sees `Label` in `available_plugins` but not `enabled_plugins`. Upstream Deluge bug: an enabled-but-not-restarted Label plugin leaves RPC methods unregistered on the web process for one session — re-call `web.connect` after enabling to force method re-registration.
- File-priority scale is **0/1/4/7** (Skip/Low/Normal/High), **NOT** qBit's 0/1/6/7. Writing `1` for "wanted" would set the file to Low priority. Ryokan writes 0 for skip and 4 for wanted.
- Duplicate-add detection is substring-matching on `"Torrent already in session"` / `"Torrent already being added"` (deluge-dev/#3507 — error code fluctuates across versions).
- **No `has_metadata` field** in `core.get_torrent_status` (live-probed against 2.x + Label plugin 0.3); proxy: `files` array non-empty.
- Every deserializer uses `#[serde(default)]` because `get_torrent_status` silently drops unknown keys rather than returning an error.
- `list_scoped` asks `core.get_torrents_status` for every key (empty key list), which is how `stop_at_ratio` / `stop_ratio` / `ratio` arrive for `seeding_done` (#228).

## Transmission quirks (`transmission/mod.rs`)

- **CSRF session handshake.** Every first request returns 409 + `X-Transmission-Session-Id` header that must echo on every subsequent request. Session ID rotates on daemon restart; mid-stream 409 means re-capture and retry once. The `send` helper handles both transparently.
- Auth is **HTTP Basic**, not RPC-level — wrong creds surface as 401, not an RPC envelope error.
- Native labels in 4.x; Ryokan filters `labels.contains(self.label)` client-side (RPC has no server-side label filter).
- File-selection is **0/1 (unwanted/wanted)** via parallel `files-wanted: [idx]` / `files-unwanted: [idx]` arrays. Priority high/normal/low is a *separate* axis Ryokan deliberately doesn't touch.
- Duplicate-add surfaces as `torrent-duplicate` key inside `result: "success"` envelope (not as an error). No message parsing.
- **Completion is `percentDone >= 1.0`**, NOT `isFinished`. `isFinished` means "hit seed ratio/time target" (user-defined stop condition), not "download complete." It feeds `DownloadItem::seeding_done` together with the ratio fields (#228); `list_scoped` also makes a best-effort `session-get` for the daemon's global ratio limit.
- Status codes 0..=6: 0=Stopped, 1=Queued-to-verify, 2=Verifying, 3=Queued-to-download, 4=Downloading, 5=Queued-to-seed, 6=Seeding.

## rtorrent quirks (`rtorrent/mod.rs`)

- Speaks **XML-RPC** over HTTP to `/RPC2`.
- **Hashes are UPPERCASE on the wire.** Every `d.<method>` / `f.<method>` call keyed by hash takes uppercase-hex; conversion happens inside every helper, not at call sites — trait contract says callers pass lowercase hex.
- Every method takes a target, even `d.multicall2` (empty string as target).
- **Duplicate-add is silent** — `load.start_verbose` returns `0` on both fresh and duplicate adds. Ryokan pre-checks by listing hashes and returns `AddOutcome::AlreadyPresent` when known.
- File priority is binary 0/1 (NOT Deluge's 0/4), BUT after setting priorities **you MUST call `d.update_priorities(<hash>)`** or the new priorities don't take effect. The single most common "my script sets priorities and nothing happens" bug in rtorrent automation.
- **`d.erase` does NOT touch disk** — per cmd-ref verbatim: "the data stored for the item is not touched in any way." Read `content_path` first, call `d.erase`, then recursively remove the FS path. Guard with `content_path != d.directory` so a multi-file torrent dumped at the save root doesn't nuke the entire download dir. Recursive remove runs in `tokio::task::spawn_blocking`.
- `d.base_path` is empty on closed/stopped torrents and after rtorrent restart; fall back to `d.directory + "/" + d.name` when empty.
- During metadata fetch, `base_path` ends in `.meta` (also the signal metadata hasn't arrived); post-metadata it rewrites to actual content name. Poll `!base_path.ends_with(".meta")` at 500ms cadence, **60s budget** (longer than other clients — cold DHT legitimately takes longer).
- Wire tags: rtorrent returns `<i8>` for sizes / rates / most counters; the decoder accepts both `<i4>` and `<i8>`.
- **No per-torrent seed limits.** See the seed-rules table above; do not reintroduce a `d.ratio.*.set` call, none exists in `command_download.cc`.

## SAB quirks (`sabnzbd/mod.rs`)

- **Endpoint shape**: `GET <base>/api?apikey=…&mode=…&output=json` for every call. The user's configured base IS the base — impl appends `/api`. `/sabnzbd` URL_BASE prefix is per-install (not default on linuxserver/sabnzbd, Ubuntu .deb, or most bare installs); users on the legacy prefix configure the base as `http://host:8080/sabnzbd`. Live-probed against linuxserver 2026-04-27.
- **No session/cookie auth** — apikey on every request; no equivalent to qBit's re-auth path.
- **Add response**: `mode=addurl` returns `{"status":true,"nzo_ids":["SABnzbd_nzo_..."]}`. **Empty `nzo_ids` array is ambiguous** — could be SAB's pre-queue dup detection (so we report `AddOutcome::AlreadyPresent` when a `mode=queue` scan finds a slot whose `url` matches) or could be a real failure (malformed URL, indexer auth issue) which we surface as an error rather than silent success.
- **No per-file selection** — NZBs are opaque blobs until SAB's post-processing extraction runs (outside Ryokan's reach). Impl no-ops `set_file_wanted` and returns `SelectiveOutcome::FullDownload` from `add_torrent_with_file_filter` — better than crashing the picker UI.
- **v1 picker-path limitation** — paths going through `add_torrent_with_file_filter` (interactive picker, batch-with-selective from auto_search, selective batches from `library/search/grab.rs`) get the *pre-add BT-style* `info_hash` persisted rather than the real `nzo_id`. Post-processing won't match → row marked stale-removed after 60s. Dominant SAB grab paths (RSS, autobrr, manual `/api/grab`, upgrade sweep) all use `add_torrent_returning_id` and persist the real id, so v1 ships with this gap. A user who hits it sees the file land via post-processing's directory scan eventually but library attribution may be missing.
- **Add paused**: `mode=addurl&priority=-1` adds at SAB's "Paused" priority (queue still processes but doesn't actively download). Suitable for the picker's metadata-wait + selection flow because the file list arrives instantly — NZB describes the file set up-front, no metadata handshake.
- **Storage path**: SAB returns `storage` on completed history slots — absolute path to the unpacked output dir. Queue slots have no `storage` until they move to history; `content_path` reads as empty until then. Post-processing's stale-mark grace window already handles this.
- **Auto-creates the configured category in SAB** if it isn't already there. Two entry points — both call the same `ensure_category` (`mode=get_cats`; if missing, `mode=set_config&section=categories&keyword=<cat>&name=<cat>&dir=<cat>`):
  - **`test()`** (Settings → Test connection button) — explicit, surfaces "(created category 'X' in SAB)" or "(warning: …)" in the version-string toast. Always probes; never short-circuits.
  - **First add per process** (`add_torrent_returning_id` / `add_torrent_paused_returning_id`) — defensive safety net for users who saved their SAB row without clicking Test. Cached via the `category_ensured` `AtomicBool` on `SabClient` so only the first grab pays the `get_cats` round-trip; subsequent grabs early-return. Failure (network blip, read-only `nzb_key`) is logged at `warn` and swallowed — the add succeeds and a process restart re-attempts.
- **Defensive `change_cat` after auto-create-on-add.** SAB's `set_config` writes the category to its config file with no documented guarantee that the write is visible to the very next `addurl` call in the same process (config reload propagation). When `ensure_category_cached_once` reports it just created the category, the add path follows up with `mode=change_cat&value=<nzo_id>&value2=<cat>` to re-tag the just-added queue slot. Without this, a fresh-install first grab can land in SAB's default bucket despite passing `cat=…` on `addurl`, repeating the very symptom auto-create is meant to fix. `change_cat` is queue-only per SAB 5.0 docs; for the add-path use case the just-added job is in queue. `change_cat` failure is logged but never fails the add. The `AlreadyPresent` path skips `change_cat` deliberately — the slot pre-existed our addurl call, so it's either tagged correctly already or was added by another tool we shouldn't overstep.
- **`set_config` parameter shape**: pass both `keyword=` (4.x) and `name=` (5.x) for the category identifier. SAB ignores unknown params, so passing both is safe across versions and removes the version-detection footgun. Live-probed against 5.0.1 (2026-05-02). Full SAB API key required (read-only `nzb_api_key` returns 401/403 on `set_config`); error surfaces with a hint to swap keys.
- **Post-import removal (#228)** is `delete(nzo_id, true)` (queue first, then `mode=history&name=delete&del_files=1`) plus unlinking the stamped `imported_source_paths`, because `del_files=1` no-ops when the history `storage` is the parent complete dir. Same belt and braces as the episode delete path.
- **`list_scoped` diagnostic dumps `seen_categories=…`** when SAB returned slots but the filter dropped all of them — the actionable bit users compare against `configured_category=…` to spot the mismatch from logs alone. Cheap (one `String` allocation per dropped slot, only on the unhappy path).

## Live-smoke tests

Each impl ships a `#[ignore]`d `live_smoke` test that exercises the full trait surface against a real client on localhost. Run with `--ignored` *and* the corresponding env var set:

| Var | Default password / config |
|---|---|
| `RYOKAN_QBIT_E2E=1` | `QBIT_PASS=adminadmin` (qBit-on-first-start default) |
| `RYOKAN_DELUGE_E2E=1` | password from settings |
| `RYOKAN_TRANSMISSION_E2E=1` | settings creds |
| `RYOKAN_RTORRENT_E2E=1` | (no auth) |
| `RYOKAN_SAB_E2E=1` | `RYOKAN_SAB_URL=http://localhost:8080`, `RYOKAN_SAB_API_KEY=<key>`, `RYOKAN_SAB_CAT=ryokan-test` (no usable default for the API key — SAB requires one) |

CI never runs these — they're for hand-validation when touching a client impl.
