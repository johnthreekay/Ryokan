# Download clients

Ryokan works with **qBittorrent**, **Deluge**, **Transmission**, **rTorrent** and **SABnzbd**. You can add more than one.

The [quick start](quick-start.md) sets up a client together with Ryokan. This page is for connecting a client you already run, and for working out why a connection fails.

## Setting up each client

Clients are added under **Settings → Download Clients → Add download client**.

The URLs below use the service name from a compose file, such as `http://qbittorrent:8080`. That works when Ryokan and the client are in the same compose file. Otherwise use the address you open the client with in a browser.

Give Ryokan its own category or label, such as `ryokan-anime`. Ryokan only looks at downloads with that category, so it leaves everything else in the client alone.

=== "qBittorrent"

    Works with qBittorrent 4.x and 5.x.

    - **Kind**: qBittorrent
    - **URL**: `http://qbittorrent:8080`
    - **Username** / **Password**: your web UI login
    - **Category**: `ryokan-anime`

    !!! warning "If a grab never appears in qBittorrent"
        qBittorrent fetches the `.torrent` file itself and reports success to Ryokan before it has done so. If the fetch fails, the grab looks fine in Ryokan and is missing from qBittorrent. Check qBittorrent's log: `docker compose logs qbittorrent | tail -50`.

    If Test connection says "Unauthorized" with the right password, look at **Tools → Options → Web UI → Server domains** in qBittorrent. It has to be `*` or include the name in Ryokan's URL.

=== "Deluge"

    Set a web UI password first. The default is `deluge`.

    - **Kind**: Deluge
    - **URL**: `http://deluge:8112`
    - **Password**: the web UI password
    - **Label**: `ryokan-anime`

    Ryokan turns on Deluge's Label plugin the first time it connects. If Test connection fails with "Unknown method", click it again.

    !!! info "Two-step connect"
        Logging in to Deluge's web UI does not connect it to the Deluge daemon. That is a second step, and until it happens every request fails with "Unknown method". Ryokan does both steps when you click Test connection. If the errors come back later, the daemon has probably restarted. Click Test connection again.

=== "Transmission"

    Set a username and password. The linuxserver image takes them from the `USER` and `PASS` environment variables. Without them, anyone who can reach Transmission can control it.

    - **Kind**: Transmission
    - **URL**: `http://transmission:9091`
    - **Username** / **Password**: your `USER` and `PASS`
    - **Label**: `ryokan-anime`

    Labels need Transmission 4.x. On 3.x and earlier, Ryokan tells its torrents apart by their save folder instead.

    !!! info "Session IDs"
        Transmission requires a session ID with every request and issues a new one when it restarts. Ryokan fetches it and retries by itself, so a restart of Transmission needs nothing from you.

=== "rTorrent"

    Use the `crazymax/rtorrent-rutorrent` image. Set a password first: the [Quick start](quick-start.md) shows the command when you pick rTorrent.

    - **Kind**: rTorrent
    - **URL**: `http://rutorrent:8000/RPC2`
    - **Username** / **Password**: the login from `rpc.htpasswd`. Leave both blank if you did not set one.
    - **Label**: `ryokan-anime`

    !!! warning "Give each torrent its own folder"
        When Ryokan removes a torrent from rTorrent it also deletes the torrent's files, because rTorrent does not. If rTorrent saves every torrent straight into one shared folder, check that setup before you let Ryokan remove downloads.

    A magnet link without trackers can take up to a minute to start in rTorrent.

=== "SABnzbd"

    1. Copy the API key from **Config → General → Security**. Use the full API key, not the NZB key.
    2. Add `sabnzbd`, or whatever name is in Ryokan's URL, to **Config → Special → host_whitelist**. Save and restart SABnzbd. Without this, Test connection fails with a 403.

    - **Kind**: SABnzbd
    - **URL**: `http://sabnzbd:8080`. This is SABnzbd's own port, not the host port you may have mapped it to.
    - **API Key**: the full API key
    - **Category**: `ryokan-anime`. Ryokan creates it in SABnzbd when you click Test connection.

    SABnzbd always downloads a whole release. Ryokan then imports the files it wants and ignores the rest.

    If downloads disappear from Ryokan but are still running in SABnzbd, see [Troubleshooting](troubleshooting.md#sab-downloads-disappear-from-ryokan-but-still-download-in-sab).

## If "Test connection" fails

- **Cannot connect**: a URL with a service name only works inside the same compose file. Try the client's IP address and published port.
- **Wrong port**: inside a compose file, use the client's own port (SABnzbd's 8080), not the host port you mapped it to (8081 in the quick start).
- **qBittorrent says Unauthorized**: see the qBittorrent tab above.
- **SABnzbd says Unauthorized**: you pasted the NZB key. Use the full API key.
- **SABnzbd returns 403**: the name in Ryokan's URL is not in `host_whitelist`.

## Seeding rules and removal after import

### Seeding rules

An indexer can have a **Seed Ratio** and a **Seed Time** (minutes). Ryokan passes them to the download client with each grab from that indexer. What happens next depends on the client:

| Client | Seed Ratio | Seed Time |
|---|---|---|
| qBittorrent | Per-torrent ratio limit | Per-torrent seeding time limit |
| Transmission | Per-torrent ratio limit | Idle limit: the torrent stops after that many minutes without uploading |
| Deluge | Per-torrent stop ratio | Ignored |
| rTorrent | Ignored | Ignored |
| SABnzbd | Not applicable | Not applicable |

For qBittorrent, a rule you leave empty uses qBittorrent's global limit. Before Ryokan 1.9.3, an empty rule turned that limit off for the torrent. For rTorrent, set up a ratio group in `.rtorrent.rc` instead.

A torrent with seeding rules is left in the client when you delete or upgrade its episode in Ryokan, so it can finish seeding.

### Removing downloads

Each client has two boxes, both on by default:

- **Remove completed downloads**: once a download has been imported and the client reports it has finished seeding, Ryokan removes it from the client along with its files. Your library keeps its own copy. Usenet downloads, and torrents imported in Move mode, are removed straight after import.
- **Remove failed downloads**: when the client reports a download as failed, Ryokan removes it before searching for a replacement. With the box off, the failed download stays in the client. The release is blocklisted either way.

Downloads that were only partly imported, and torrents you paused yourself, are never removed.

!!! warning "Let Ryokan do the removing"
    The client has to keep a finished download until Ryokan has imported it. Set torrent clients to pause or stop at their limit, not to delete, and set SABnzbd to keep completed jobs in its history. If the client removes a download first, Ryokan never sees it finish.

## Per-client download paths

Fill in a client's **Download path** when Ryokan and the client see the download folder at different paths. For example:

- the client saves to `/data/torrents`, but Ryokan has the same folder mounted at `/downloads`
- the client runs on a seedbox that you have mounted locally

Ryokan replaces the start of each path the client reports with this value.

Leave it empty when both use the same path, as in the [quick start](quick-start.md).

## Routing: which client gets each grab

With more than one client, Ryokan chooses in this order:

1. The client set on the indexer, if any.
2. For Nyaa, the client set on the Nyaa card.
3. The default client for the type of download: the torrent default for torznab indexers and Nyaa, the Usenet default for newznab indexers.

The download keeps the client it was sent to, so changing a default later does not affect downloads already running.

---

*Last updated: 2026-09-06.*
