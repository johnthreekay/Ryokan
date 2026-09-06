# Download clients

Ryokan supports five download clients: **qBittorrent**, **Deluge**, **Transmission**, **rTorrent**, and **SABnzbd**. You can run more than one at once and Ryokan routes per-grab; the bundled [quick start](quick-start.md) walks through deploying one alongside Ryokan and Jellyfin in a single compose.

This page is the deeper reference: per-client setup walkthroughs, the fields Ryokan needs, common gotchas, plus how Ryokan picks which client to send each grab to. Use it when you're connecting Ryokan to a client you already have running, or when you're trying to figure out why a connection isn't working.

## Per-client setup

Pick the tab for your client. Each one covers preparation steps inside the client itself, the fields to fill in under **Settings → Download Clients → Add download client** in Ryokan, and the gotchas worth knowing about.

=== "qBittorrent"

    **Tested against**: qBittorrent 4.x and 5.x. Ryokan auto-detects 5.x's renamed `stop`/`start` endpoints (was `pause`/`resume` in 4.x) by trying the new names first and falling back, so you don't have to tell Ryokan which version you're on.

    **Prepare qBit:**

    1. Open qBit's web UI (default port 8080) and create your admin password if it's still on the random first-run password.
    2. **Tools → Options → Web UI**: uncheck **Enable Host header validation**, then Save and restart the qBit container. qBit 4.5+ enables this by default; with it on, qBit returns 401 even for correct credentials when Ryokan-in-container POSTs with a `Host: qbittorrent:8080` header. Symptom in Ryokan is "qBittorrent Unauthorized" stuck on Settings → Download Clients while the web UI works fine from your browser.

    **Add it to Ryokan** (Settings → Download Clients):

    - **Kind**: qBittorrent
    - **URL**: `http://qbittorrent:8080` if Ryokan and qBit share a Docker compose, else your host's LAN IP and the host-mapped port.
    - **Username** / **Password**: what you log in to the web UI with. qBit re-auths on 403 via session cookie, so Ryokan won't pester you about expired tokens.
    - **Category**: a distinctive label like `ryokan-anime`. Ryokan only sees torrents in this category, which keeps it from accidentally touching torrents added by other tools.

    !!! warning "Silent grab failures"
        qBit's `POST /torrents/add` returns `Ok.` and fetches the `.torrent` URL **server-side**. A silent fetch failure (tracker timeout, indexer 404) looks identical to a successful add from Ryokan's perspective. If a grab "vanishes" without showing up in qBit, check qBit's own logs first; that's where the failure lives. `docker compose logs qbittorrent | tail -50` is usually enough.

=== "Deluge"

    **Prepare Deluge:**

    1. Open Deluge's web UI (default port 8112) and set a real password; the default is `deluge`.
    2. The **Label plugin** is bundled but disabled by default. Ryokan's Test connection enables it automatically on first connect. There's a known upstream Deluge bug where an enabled-but-not-restarted Label plugin leaves RPC methods unregistered for one session; if it doesn't take, click Test connection again.

    **Add it to Ryokan** (Settings → Download Clients):

    - **Kind**: Deluge
    - **URL**: `http://deluge:8112` for in-compose, else LAN IP plus port.
    - **Password**: the web UI password you set above. Deluge has no per-user auth at the API layer; the password is the only credential.
    - **Label**: `ryokan-anime` (or your chosen tag). Ryokan adds and filters by this label.

    !!! info "Two-step connect"
        `auth.login(password)` establishes a session, but the freshly-authenticated session isn't connected to any daemon. Every `core.*` call fails with `Unknown method` until `web.connect(host_id)` runs. Ryokan handles this in the connection-test path. If you see "Unknown method" errors after configuring Deluge, it usually means the daemon side restarted and the web process needs a re-connect; clicking Test connection again refreshes it.

=== "Transmission"

    **Prepare Transmission:**

    1. Open Transmission's web UI (default port 9091) and confirm it loads.
    2. Set credentials. The linuxserver/transmission image takes them from the `USER` and `PASS` environment variables; without them, Transmission accepts unauthenticated connections, which is fine on a localhost-only setup but a problem the moment you expose the web UI.

    **Add it to Ryokan** (Settings → Download Clients):

    - **Kind**: Transmission
    - **URL**: `http://transmission:9091` for in-compose, else LAN IP plus port.
    - **Username** / **Password**: matches the `USER` and `PASS` env vars (HTTP Basic auth, not RPC-level). Wrong credentials surface as 401, not as an RPC envelope error.
    - **Label**: `ryokan-anime`. Native labels work in Transmission 4.x; on 3.x and earlier Ryokan falls back to a save-path prefix to scope its torrents.

    !!! info "Session-ID handshake"
        Transmission requires a CSRF session ID on every RPC call. Ryokan handles this transparently. Daemon restart rotates the session ID; mid-stream 409s are retried once automatically.

=== "rTorrent"

    The recommended Docker image is `crazymax/rtorrent-rutorrent` (linuxserver's rutorrent image was deprecated by its maintainers).

    **Prepare rTorrent:**

    1. Set up htpasswd files for the web UI and XML-RPC; the image enforces basic auth when the files exist and runs without auth when they don't. The [quick start's rTorrent tab](quick-start.md#1-run-ryokan-jellyfin-and-your-download-client) shows the one-liner that generates both with the same credentials.
    2. Open ruTorrent's web UI (typically port 8082 host-side in the bundled compose) and confirm it loads with your htpasswd credentials.

    **Add it to Ryokan** (Settings → Download Clients):

    - **Kind**: rTorrent
    - **URL**: `http://rutorrent:8000/RPC2` for in-compose. Port 8000 is the dedicated XML-RPC port inside the container; `/RPC2` is the path. Ryokan reaches it via Docker DNS so the XML-RPC port doesn't need a host-side mapping.
    - **Username** / **Password**: matches `/passwd/rpc.htpasswd`. Leave both blank if you skipped htpasswd setup.
    - **Label**: `ryokan-anime`. Stored in the `custom1` field per the ruTorrent label convention.

    !!! warning "`d.erase` doesn't touch disk"
        rTorrent's docs are explicit: removing a torrent leaves the data in place. Ryokan reads `content_path` first, calls `d.erase`, then recursively removes the filesystem path. There's a guard preventing a multi-file torrent dumped at the save root from nuking the entire download directory, but if you've configured rTorrent in an unusual way (no save-path subfolder, all torrents share `/downloads`) this is the bit to double-check.

    !!! info "Cold-DHT metadata fetches are slow"
        rTorrent's metadata-fetch budget is 60s here vs. 10s for the BT clients with trackers; the longer budget is real, not a Ryokan-side throttle. If a magnet-only release without trackers takes a minute to start downloading, that's expected.

=== "SABnzbd"

    **Prepare SAB:**

    1. Open SAB's web UI (default port 8080 inside the container; in the bundled compose that's host port 8081). Walk through the first-run wizard.
    2. **Get the API key**: SAB shows it on the wizard's final step, or from **Config → General → Security → API Key** later. Make sure it's the **full** API key, not the read-only `nzb_api_key`. The Test-connection probe in Ryokan catches a wrong/missing key at config time instead of at first grab.

    **Add it to Ryokan** (Settings → Download Clients):

    - **Kind**: SABnzbd
    - **URL**: `http://sabnzbd:8080` for in-compose. Note this is the **container** port, not whatever host port you mapped SAB to. Ryokan reaches SAB through Docker's per-compose network where SAB still listens on its native 8080; the host mapping only matters for your browser.
    - **API Key**: paste the full key.
    - **Category**: `ryokan-anime`. SAB auto-creates this category on first connect if it doesn't exist (Ryokan's Test connection handles the create via `set_config`).

    !!! info "No per-file selection on the wire"
        NZBs are opaque blobs until SAB extracts them. Ryokan's interactive picker still works (it shows the file list for selection if SAB has parsed the headers), but the actual file selection is a no-op at the wire level. SAB downloads the whole NZB, then post-processing imports the files Ryokan wanted and skips the rest.

    !!! tip "Disappearing downloads"
        If grabs "vanish" from Ryokan but you can see them downloading in SAB, the category Ryokan was configured to use probably doesn't exist in SAB. Click Test connection to auto-create it; the [troubleshooting page](troubleshooting.md#sab-downloads-disappear-from-ryokan-but-still-download-in-sab) has the full diagnosis.

## Seeding rules and removal after import

An indexer row can carry a **Seed Ratio** and a **Seed Time** (minutes). Ryokan passes them to the download client when it adds a grab from that indexer, and its own delete actions (deleting an episode, replacing it with an upgrade) leave such a torrent in the client so the client's rules decide when seeding ends. The one exception is a torrent imported in Move mode, which cannot seed once its file has moved. What the client does with the rules, and what "finished seeding" means to it, differs per client:

- **qBittorrent**: both rules become per-torrent share limits. A rule you leave empty keeps qBittorrent's global limit for that dimension (before 1.9.3 it switched the limit off for the torrent). Finished means qBittorrent stopped the torrent at its ratio, seeding-time, or inactivity limit. If qBittorrent is set to remove torrents itself when a limit is reached, that works too.
- **Transmission**: the ratio becomes a per-torrent ratio limit. Transmission has no total seeding-time limit, so Seed Time becomes its idle limit: the torrent stops after that many minutes without upload activity, which is never earlier than the same number of minutes of seeding and can be later while peers are still pulling. Finished means Transmission stopped the torrent at its ratio or idle limit.
- **Deluge**: the ratio becomes a per-torrent stop ratio. Deluge has no seeding-time limit, so Seed Time is ignored. Finished means Deluge paused the torrent at its stop ratio.
- **rTorrent**: has no per-torrent limits at all, so neither rule is applied and Ryokan logs a warning for each such grab. Configure a ratio group in `.rtorrent.rc` instead; the group's rules apply to every item in it. Finished means the ratio group closed the item. A torrent you stopped in ruTorrent stays open and is left alone.
- **SABnzbd**: nothing seeds. A job leaves SAB's history as soon as Ryokan has imported it.

Each client row has a **Remove completed downloads** box, on by default. With it on, a torrent is removed from that client, files included, once it has been imported and the client reports it finished; usenet jobs and Move-mode torrents go right after import. Ryokan checks every five minutes. The library keeps its own copy in every mode. Partial and failed imports, and torrents you paused by hand, stay.

For this to work the client has to keep finished downloads long enough for Ryokan to import them, the same requirement Sonarr and Radarr have: SABnzbd should keep completed jobs in its history (its History Retention setting), and a torrent client should pause or stop a torrent when its limit is reached rather than delete it. Ryokan does the deleting once the import is done. If the client removes a download first, Ryokan never sees it finish.

## If "Test connection" fails

The most common causes:

- **Wrong URL host**: container-name URLs like `http://qbittorrent:8080` rely on Docker's per-compose DNS. If Ryokan and the client are in separate compose files, run on different hosts, or you have a network plugin that interferes with Docker DNS, swap the container name for your host's LAN IP and the host-mapped port.
- **Wrong port**: container port vs. host port matters. The URLs above use container ports (because Ryokan reaches the client over Docker's internal network); the host-mapped port is only for your browser.
- **Stuck-on credentials**: qBit's "Unauthorized" with right credentials usually means Host header validation needs disabling (see the qBittorrent tab above). SAB's "Unauthorized" usually means the read-only `nzb_api_key` was pasted instead of the full key.

## Per-client download paths

Each client gets its own `download_path` config field for cases where Ryokan and the client see the filesystem differently. Common reasons:

- Docker volumes mounted at different host paths.
- Seedboxes accessed over SSHFS where the local mount point doesn't match the seedbox's internal path.
- Existing setups where the client was using `/data/torrents` and Ryokan's compose mounts at `/downloads`.

The translation works as a prefix substitution: when the client reports a path, Ryokan replaces the client's `save_path` prefix with the configured `download_path` to get a path Ryokan itself can read. If the client-reported path doesn't start with the expected prefix, Ryokan returns the path unchanged rather than silently rewriting; silent rewrite would mask misconfiguration as a "file not found" error later.

In the bundled compose ([quick start](quick-start.md)), Ryokan and the download client both bind-mount the same host folder at `/downloads`, so the path is identical on both sides and `download_path` stays empty.

## Routing: which client gets each grab

When you have multiple download clients configured, Ryokan picks one per-grab based on this chain:

1. **Per-indexer pin**. A torznab or newznab indexer row has an optional `download_client_id`. Grabs from that indexer always go to the pinned client, overriding everything below.
2. **Per-protocol default**. Without a pin, torznab grabs (torrent metadata) go to the torrent default; newznab grabs (NZB metadata) go to the usenet default. Both defaults can coexist (one row per protocol marked `is_default`).
3. **Nyaa**. Has its own `nyaa_download_client_id` config field. Always falls back to the torrent default if unset; Nyaa items are magnets or `.torrent` URLs, so routing them to a usenet client would just trip the protocol guard at add-time.

At grab time the chosen client ID is stamped on the `grabbed_torrents` row, so post-processing routes back through the same client even if you change defaults later.

---

*Last updated: 2026-09-06.*
