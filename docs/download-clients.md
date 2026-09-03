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

*Last updated: 2026-08-29.*
