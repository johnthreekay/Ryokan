# Quick start

End-to-end: deploy Ryokan + Jellyfin + a download client, configure them, add a show, watch it land in your library.

If you want a complete stack with extras (Seerr, reverse proxy, VPN), use the **[Stack builder](stack-builder.md)** instead. It generates the whole `docker-compose.yml` for you.

## What you'll need

- **Docker** and **Docker Compose** installed (`docker --version` and `docker compose version` should both work at the terminal). New to Docker? Read [Docker's overview](https://docs.docker.com/get-started/docker-overview/) first; it covers what containers, images, and volumes are, which the rest of this page assumes you know.

You don't need a download client running in advance; we'll deploy one alongside Ryokan and Jellyfin in the next step. You also don't need a Prowlarr or AniList account; the built-in Nyaa search works without either.

## 1. Run Ryokan, Jellyfin, and your download client

Set up the host paths. The compose file lives in `/srv/stack/`; per-service config and state go under `/srv/docker/<service>/`; downloads and the media library go under `/srv/media/`.

```sh
sudo mkdir -p /srv/stack /srv/media/{downloads,anime} /srv/docker
sudo chown 1000:1000 /srv/media/downloads /srv/media/anime
cd /srv/stack
```

Adjust `1000:1000` if your host user has different IDs (`id -u` and `id -g` to check). Pick the download client you want to use, create its per-service folder, and save the matching `docker-compose.yml` in `/srv/stack/`. All five composes deploy Ryokan + Jellyfin alongside the chosen client; the three services share `/srv/media/anime` so files Ryokan imports show up in Jellyfin automatically.

=== "qBittorrent"

    ```sh
    sudo mkdir -p /srv/docker/{ryokan,jellyfin,qbittorrent}
    sudo chown -R 1000:1000 /srv/docker
    ```

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - /srv/docker/ryokan:/data
          - /srv/media/downloads:/downloads
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - /srv/docker/jellyfin/config:/config
          - /srv/docker/jellyfin/cache:/cache
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      qbittorrent:
        image: lscr.io/linuxserver/qbittorrent:latest
        container_name: qbittorrent
        ports:
          - "8080:8080"        # web UI
          - "6881:6881"        # BT
          - "6881:6881/udp"
        volumes:
          - /srv/docker/qbittorrent:/config
          - /srv/media/downloads:/downloads
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
          - WEBUI_PORT=8080
        restart: unless-stopped
    ```

=== "Deluge"

    ```sh
    sudo mkdir -p /srv/docker/{ryokan,jellyfin,deluge}
    sudo chown -R 1000:1000 /srv/docker
    ```

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - /srv/docker/ryokan:/data
          - /srv/media/downloads:/downloads
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - /srv/docker/jellyfin/config:/config
          - /srv/docker/jellyfin/cache:/cache
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      deluge:
        image: lscr.io/linuxserver/deluge:latest
        container_name: deluge
        ports:
          - "8112:8112"        # web UI
          - "6881:6881"        # BT
          - "6881:6881/udp"
        volumes:
          - /srv/docker/deluge:/config
          - /srv/media/downloads:/downloads
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped
    ```

=== "Transmission"

    ```sh
    sudo mkdir -p /srv/docker/{ryokan,jellyfin,transmission}
    sudo chown -R 1000:1000 /srv/docker
    ```

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - /srv/docker/ryokan:/data
          - /srv/media/downloads:/downloads
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - /srv/docker/jellyfin/config:/config
          - /srv/docker/jellyfin/cache:/cache
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      transmission:
        image: lscr.io/linuxserver/transmission:latest
        container_name: transmission
        ports:
          - "9091:9091"        # web UI
          - "51413:51413"      # BT
          - "51413:51413/udp"
        volumes:
          - /srv/docker/transmission:/config
          - /srv/media/downloads:/downloads
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
          - USER=admin
          - PASS=changeme      # change before first start
        restart: unless-stopped
    ```

=== "rTorrent"

    ```sh
    sudo mkdir -p /srv/docker/{ryokan,jellyfin,rutorrent/passwd}
    sudo chown -R 1000:1000 /srv/docker
    ```

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - /srv/docker/ryokan:/data
          - /srv/media/downloads:/downloads
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - /srv/docker/jellyfin/config:/config
          - /srv/docker/jellyfin/cache:/cache
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      rutorrent:
        image: crazymax/rtorrent-rutorrent:latest
        container_name: rutorrent
        ports:
          - "8082:8080"        # ruTorrent web UI (host:container; 8082 picked to leave 8080 free for other clients)
          - "50000:50000"      # BT incoming peer connections
          - "6881:6881/udp"    # DHT
        # XML-RPC (port 8000 inside the container) isn't host-exposed.
        # Ryokan talks to it via Docker's per-compose network at
        # `http://rutorrent:8000/RPC2`. Add `- "8000:8000"` here only if
        # you want to hit it from the host with curl for debugging.
        volumes:
          - /srv/docker/rutorrent:/data
          - /srv/media/downloads:/downloads      # contains /downloads/temp and /downloads/complete after first start
          - /srv/docker/rutorrent/passwd:/passwd  # htpasswd files for web UI + RPC auth
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped
    ```

    **Set up basic auth before bringing the stack up.** The crazy-max image looks for htpasswd files at `/passwd/rutorrent.htpasswd` (web UI) and `/passwd/rpc.htpasswd` (XML-RPC, the endpoint Ryokan uses). When the files exist, nginx enforces auth in front of both; when they don't, both are unauthenticated. Generate both with the same credentials in one shot:

    ```sh
    docker run --rm httpd:2.4-alpine htpasswd -Bbn admin "REPLACE-WITH-YOUR-PASSWORD" \
      | sudo tee /srv/docker/rutorrent/passwd/rutorrent.htpasswd > /dev/null
    sudo cp /srv/docker/rutorrent/passwd/rutorrent.htpasswd /srv/docker/rutorrent/passwd/rpc.htpasswd
    sudo chown -R 1000:1000 /srv/docker/rutorrent/passwd
    ```

    Replace `REPLACE-WITH-YOUR-PASSWORD` with whatever you want; same credentials work for ruTorrent's web UI and the connection Ryokan makes. Step 4 walks through plugging them into Ryokan.

=== "SABnzbd"

    ```sh
    sudo mkdir -p /srv/docker/{ryokan,jellyfin,sabnzbd/{config,incomplete}}
    sudo chown -R 1000:1000 /srv/docker
    ```

    ```yaml
    services:
      ryokan:
        image: ghcr.io/johnthreekay/ryokan:latest
        container_name: ryokan
        ports:
          - "8978:8978"
        volumes:
          - /srv/docker/ryokan:/data
          - /srv/media/downloads:/downloads
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      jellyfin:
        image: jellyfin/jellyfin:latest
        container_name: jellyfin
        ports:
          - "8096:8096"
        volumes:
          - /srv/docker/jellyfin/config:/config
          - /srv/docker/jellyfin/cache:/cache
          - /srv/media/anime:/media/anime
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped

      sabnzbd:
        image: lscr.io/linuxserver/sabnzbd:latest
        container_name: sabnzbd
        ports:
          - "8081:8080"          # host:container; SAB lives on 8080 internally
        volumes:
          - /srv/docker/sabnzbd/config:/config
          - /srv/docker/sabnzbd/incomplete:/incomplete-downloads   # in-progress
          - /srv/media/downloads:/downloads                        # completed (shared with Ryokan + Jellyfin)
        environment:
          - PUID=1000
          - PGID=1000
          - TZ=Etc/UTC
        restart: unless-stopped
    ```

    !!! note "Why two folders?"
        SAB splits in-progress (`/incomplete-downloads`) from completed (`/downloads`). The compose puts the in-progress side under SAB's own config tree so it's tucked out of the way; only completed downloads land in `/srv/media/downloads` where Ryokan reads them.

Bring it up:

```sh
docker compose up -d
```

Ryokan is now on port 8978, Jellyfin on 8096, your download client on its default port. Per-service config and state live at `/srv/docker/<service>/`; downloads land at `/srv/media/downloads`; the library lives at `/srv/media/anime`. Everything the stack writes is inspectable from the host (no Docker named volumes to dig through).

??? info "How each client uses `/srv/media/downloads`"
    The five clients structure that folder differently. Ryokan only reads completed files, so all five layouts work transparently.

    - **qBittorrent / Deluge**: write files directly to `/downloads/<category>/<file>` inside the container (or `/downloads/<file>` if no category is set). Single shared folder.
    - **Transmission**: writes everything to `/downloads/`, with a `.part` suffix on the filename while the torrent is in flight; renames on completion.
    - **rTorrent**: splits `/downloads/temp/` (in-progress) and `/downloads/complete/` (completed); files move from temp to complete when the torrent finishes.
    - **SABnzbd**: uses a separate folder for in-progress (`/incomplete-downloads/`, bind-mounted to `/srv/docker/sabnzbd/incomplete` on the host) and `/downloads/` for completed.

!!! tip "Already running Jellyfin or your download client elsewhere?"
    Drop the relevant service block from the compose and skip the corresponding setup step below. Make sure the existing instance can read `/srv/media/anime` (Jellyfin) or write to `/srv/media/downloads` (download client), or adjust paths accordingly.

## 2. First login to Ryokan

Open <http://localhost:8978> in a browser. You'll be redirected to a setup page; pick a username and password and submit. That account is your admin account; Ryokan is single-user, so this is the only one you'll create.

Once you're logged in you'll see an empty library page. That's expected; we haven't told Ryokan about any shows yet.

## 3. Set up Jellyfin

Open <http://localhost:8096> in another tab. Walk through Jellyfin's first-run wizard:

1. Pick your display language.
2. Create a Jellyfin admin account (separate from Ryokan's).
3. **Add a media library**:
    - **Content type**: Shows
    - **Display name**: Anime (or whatever you like)
    - **Folder**: click the `+` and add `/media/anime`. This is the path Jellyfin sees inside its container; it maps to `/srv/media/anime` on your host, the same folder Ryokan writes to.
4. Accept the metadata defaults; you can tweak per-library later.
5. Finish the wizard.

Jellyfin's library will be empty for now. That's fine; once Ryokan grabs and imports its first episode, Jellyfin's scheduled scan will pick it up. You can also click **Scan All Libraries** in Dashboard → Libraries to force one immediately after a grab.

## 4. Add a download client to Ryokan

In Ryokan, go to **Settings → Download Clients → Add download client**. Fill in the values for your chosen client below.

=== "qBittorrent"

    First, fetch qBit's randomly-generated initial password from its logs:

    ```sh
    docker compose logs qbittorrent | grep -i "temporary password"
    ```

    Open <http://localhost:8080> and log in (`admin` / that temporary password). qBit will prompt you to set a real password; do that, then come back to Ryokan.

    In Ryokan's add-client form:

    - **Kind**: qBittorrent
    - **URL**: `http://qbittorrent:8080`
    - **Username**: `admin`
    - **Password**: the password you just set in qBit
    - **Category**: `ryokan-anime`
    - **Default client**: on

=== "Deluge"

    Open <http://localhost:8112>. The default Deluge web UI password is `deluge`; set a real one when prompted.

    In Ryokan's add-client form:

    - **Kind**: Deluge
    - **URL**: `http://deluge:8112`
    - **Password**: the password you set in Deluge's web UI
    - **Label**: `ryokan-anime`
    - **Default client**: on

=== "Transmission"

    Open <http://localhost:9091> and confirm Transmission is up. Auth is the `USER` and `PASS` you set in the compose file.

    In Ryokan's add-client form:

    - **Kind**: Transmission
    - **URL**: `http://transmission:9091`
    - **Username**: `admin` (matches `USER` in the compose)
    - **Password**: whatever you set `PASS` to in the compose
    - **Label**: `ryokan-anime`
    - **Default client**: on

=== "rTorrent"

    Open <http://localhost:8082> to confirm ruTorrent's web UI loads. If you set up htpasswd in step 1, you'll be prompted for the credentials you generated; log in with `admin` and your password.

    In Ryokan's add-client form:

    - **Kind**: rTorrent
    - **URL**: `http://rutorrent:8000/RPC2` (port 8000 is the dedicated XML-RPC port inside the container; Ryokan reaches it via Docker's per-compose DNS without it needing a host-side mapping)
    - **Username**: `admin` (matches the htpasswd you generated in step 1)
    - **Password**: the password you put in the htpasswd file
    - **Label**: `ryokan-anime`
    - **Default client**: on

    If you skipped htpasswd in step 1 (you'll have to manually delete the `/srv/docker/rutorrent/passwd:/passwd` mount from the compose), leave Username and Password blank.

=== "SABnzbd"

    Open <http://localhost:8081> and walk through SAB's first-run wizard. When you reach the final step, SAB shows you an API key; save it. (You can also pull it later from **Config → General → Security → API Key**; make sure it's the **full** API key, not the read-only `nzb_api_key`.)

    In Ryokan's add-client form:

    - **Kind**: SABnzbd
    - **URL**: `http://sabnzbd:8080` (the **container** port, not the 8081 host mapping. Ryokan reaches SAB through Docker's per-compose network where SAB is still on its native 8080; the 8081 in the compose is only for your browser to hit from the host.)
    - **API Key**: paste the full API key
    - **Category**: `ryokan-anime` (Ryokan auto-creates this in SAB if it doesn't exist)
    - **Default client**: on

Click **Test connection** in Ryokan. You should see "Connected" with a version number. If not, the [Download clients page](download-clients.md) has per-client troubleshooting.

!!! tip "If container DNS doesn't resolve"
    The URLs above (`http://qbittorrent:8080`, `http://sabnzbd:8080`, etc.) rely on Docker's per-compose DNS so containers can reach each other by service name. If you've split Ryokan and your download client into separate compose files, run them on different hosts, or have a network plugin that interferes with Docker DNS, swap the service name for your **host's LAN IP and the host-mapped port**. For example: instead of `http://qbittorrent:8080`, use `http://192.168.1.100:8080` (or whatever your host's IP is). It works because every container in the bundled compose publishes its port to the host, so anything that can reach the host can also reach those ports.

Save the row.

## 5. Set the media root

In Ryokan, go to **Settings → General → Media Root Path** and set it to `/media/anime`. That's the path inside Ryokan's container; it maps to `/srv/media/anime` on your host (the same folder Jellyfin reads from).

!!! warning "PUID and PGID matter for shared folders"
    The `1000:1000` defaults work for most homelabs but not all. If files Ryokan writes show up with the wrong owner and Jellyfin can't read them, run `id -u` and `id -g` on your media-owning user and update both services' `PUID` / `PGID`. [Installation → PUID and PGID](install.md#puid-and-pgid) explains why.

## 6. (Optional) Add an indexer

Skip this for now if you want; Nyaa is built in and works out of the box. But if you have a Prowlarr or Jackett set up with private trackers, this is the moment to wire those in.

**Settings → Indexers → Add indexer**. Paste the URL Prowlarr or Jackett gave you (it ends in `/api`), the API key, and pick a name. The defaults handle the rest.

Click **Test connection** to confirm Ryokan can reach it.

## 7. Add a show and watch it land

Go back to the library page, click **+ Add Series**, type the name of an anime you want, and pick the right one from the dropdown. Ryokan fetches metadata from AniList by default.

When the series page opens, each episode row has two icon buttons: **Interactive search** lists the releases ranked by Ryokan's scoring so you can click **Grab** on one, and **Auto search** lets Ryokan grab the highest-scored release for you. **Search Monitored Episodes** at the top runs the automatic version for every monitored episode.

The grab fires off to your download client. When it finishes:

1. Post-processing hardlinks the file into `/srv/media/anime/<show name>/Season 01/<episode>.mkv` on your host.
2. Jellyfin picks it up on its next library scan (or immediately if you click **Scan All Libraries**).
3. The episode is now playable from any Jellyfin client (web, mobile, TV).

## 8. (Optional) Link AniList or MAL

If you want Ryokan to add new shows automatically when you mark them watching on AniList or MAL, the **[External accounts](external-accounts.md)** page walks through linking. You can do this any time; your existing manually-added series stay put.

## What next?

- **[Configuration](configuration.md)** explains every Settings tab so you can tune scoring, choose between hardlink and copy, set up a quality profile, and so on.
- **[Importing an existing library](manual-import.md)** is the answer to "I already have 2 TB of anime". Point Ryokan at the folder, check the matches, and it hardlinks everything into place, no re-downloading.
- **[Stack builder](stack-builder.md)** generates the rest of the homelab stack (Seerr for requests, Caddy / Traefik for HTTPS, Gluetun for VPN-routed grabs) in the same shape if you want to grow beyond the basics.
- **[Troubleshooting](troubleshooting.md)** has the most common stumbles and their fixes.

---

*Last updated: 2026-08-29.*
