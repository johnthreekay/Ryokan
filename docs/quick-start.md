# Quick start

This guide takes you from a bare Linux server to a full installation of Ryokan and its connectors.

## 1. Install Docker

Skip this step if you already have Docker + Docker Compose.

=== "Debian-based"

    ```sh
    curl -fsSL https://get.docker.com | sudo sh
    ```

=== "Fedora and RHEL-based"

    ```sh
    curl -fsSL https://get.docker.com | sudo sh
    sudo systemctl enable --now docker
    ```

=== "Arch-based"

    ```sh
    sudo pacman -S docker docker-compose
    sudo systemctl enable --now docker
    ```

=== "openSUSE"

    ```sh
    sudo zypper install docker docker-compose
    sudo systemctl enable --now docker
    ```

=== "Unraid, Synology, TrueNAS"

    Docker is already part of the system. On Synology, install **Container Manager** from the Package Center. On Unraid, install the **Compose Manager** plugin from Community Applications.

    Use a folder on your storage pool for the two folders in step 2, for example `/mnt/user/media` on Unraid or `/volume1/media` on Synology.

Then let your own user run Docker without `sudo`:

```sh
sudo usermod -aG docker $USER
```

You'll have to log out and log back in for that to take effect. Check that it works:

```sh
docker run --rm hello-world
```

It should print "Hello from Docker!" along with the steps it took to generate the message.

## 2. Choose your stack

The defaults are a good start if you have no preference. You can always come back and change them later.

<form id="stack-form" class="stack-form" autocomplete="off">
<div class="sf-group">
  <div class="sf-head"><span class="sf-title">Download client</span><span class="sf-sub">Pick one or more.</span></div>
  <div class="sf-grid">
    <label class="sf-opt"><input type="checkbox" name="dlclient" value="qbittorrent" checked><span class="sf-name">qBittorrent</span><span class="sf-desc">Recommended.</span></label>
    <label class="sf-opt"><input type="checkbox" name="dlclient" value="deluge"><span class="sf-name">Deluge</span></label>
    <label class="sf-opt"><input type="checkbox" name="dlclient" value="transmission"><span class="sf-name">Transmission</span></label>
    <label class="sf-opt"><input type="checkbox" name="dlclient" value="rtorrent"><span class="sf-name">rTorrent</span><span class="sf-desc">Comes with the ruTorrent web UI.</span></label>
    <label class="sf-opt"><input type="checkbox" name="dlclient" value="sabnzbd"><span class="sf-name">SABnzbd</span><span class="sf-desc">Usenet. Needs a Usenet provider and indexer.</span></label>
  </div>
</div>
<div class="sf-group">
  <div class="sf-head"><span class="sf-title">Media server</span><span class="sf-sub">Allows you to stream your library to other devices.</span></div>
  <div class="sf-grid">
    <label class="sf-opt"><input type="radio" name="media_server" value="jellyfin" checked><span class="sf-name">Jellyfin</span><span class="sf-desc">Recommended. Free and open source.</span></label>
    <label class="sf-opt"><input type="radio" name="media_server" value="emby"><span class="sf-name">Emby</span><span class="sf-desc">Some features need Emby Premiere.</span></label>
    <label class="sf-opt"><input type="radio" name="media_server" value="plex"><span class="sf-name">Plex</span><span class="sf-desc">Needs a Plex account.</span></label>
    <label class="sf-opt"><input type="radio" name="media_server" value="none"><span class="sf-name">None</span><span class="sf-desc">You already have one, or don't want one.</span></label>
  </div>
</div>
<div class="sf-group">
  <div class="sf-head"><span class="sf-title">Requests</span><span class="sf-sub">A page where you or your friends/family request anime.</span></div>
  <div class="sf-grid">
    <label class="sf-opt"><input type="radio" name="requests" value="none" checked><span class="sf-name">None</span></label>
    <label class="sf-opt"><input type="radio" name="requests" value="seerr"><span class="sf-name">Seerr</span><span class="sf-desc">Sends requests to Ryokan.</span></label>
  </div>
</div>
<div class="sf-group">
  <div class="sf-head"><span class="sf-title">Folders and user</span></div>
  <div class="sf-fields">
    <label class="sf-field sf-wide"><span class="sf-label">Media folder</span><input type="text" name="shared_path" value="/srv/media" spellcheck="false"><span class="sf-help">Holds your downloads and your library. Use a full path.</span></label>
    <label class="sf-field sf-wide"><span class="sf-label">Settings folder</span><input type="text" name="appdata_path" value="/srv/docker" spellcheck="false"><span class="sf-help">Each app keeps its settings in a folder under this one.</span></label>
    <label class="sf-field"><span class="sf-label">User ID</span><input type="number" name="puid" value="1000" min="0"><span class="sf-help"><code>id -u</code> prints yours.</span></label>
    <label class="sf-field"><span class="sf-label">Group ID</span><input type="number" name="pgid" value="1000" min="0"><span class="sf-help"><code>id -g</code> prints yours.</span></label>
    <label class="sf-field"><span class="sf-label">Time zone</span><input type="text" name="tz" value="UTC" placeholder="America/Chicago" spellcheck="false"><span class="sf-help">Such as <code>America/Chicago</code>.</span></label>
  </div>
</div>
<details class="sf-more">
  <summary>More options: file handling, indexers, VPN, HTTPS, host check</summary>
  <div class="sf-group">
    <div class="sf-head"><span class="sf-title">File operation mode</span><span class="sf-sub">How a finished download gets into your library.</span></div>
    <div class="sf-grid">
      <label class="sf-opt"><input type="radio" name="file_mode" value="hardlink" checked><span class="sf-name">Hardlink</span><span class="sf-desc">Recommended. No extra disk space, and torrents keep seeding.</span></label>
      <label class="sf-opt"><input type="radio" name="file_mode" value="copy"><span class="sf-name">Copy</span><span class="sf-desc">Torrents keep seeding. Each episode takes its space twice.</span></label>
      <label class="sf-opt"><input type="radio" name="file_mode" value="move"><span class="sf-name">Move</span><span class="sf-desc">No extra disk space. Torrents stop seeding.</span></label>
    </div>
  </div>
  <div class="sf-group">
    <div class="sf-head"><span class="sf-title">Indexer manager</span><span class="sf-sub">Lets Ryokan search trackers and Usenet indexers besides Nyaa, which is built in.</span></div>
    <div class="sf-grid">
      <label class="sf-opt"><input type="radio" name="indexer" value="none" checked><span class="sf-name">None</span><span class="sf-desc">Nyaa only.</span></label>
      <label class="sf-opt"><input type="radio" name="indexer" value="prowlarr"><span class="sf-name">Prowlarr</span><span class="sf-desc">Torrents and Usenet. The usual choice.</span></label>
      <label class="sf-opt"><input type="radio" name="indexer" value="jackett"><span class="sf-name">Jackett</span><span class="sf-desc">Torrents only.</span></label>
    </div>
  </div>
  <div class="sf-group">
    <div class="sf-head"><span class="sf-title">VPN</span><span class="sf-sub">Sends your torrent clients through a VPN with Gluetun. You need a VPN subscription.</span></div>
    <div class="sf-grid">
      <label class="sf-opt"><input type="radio" name="vpn" value="none" checked><span class="sf-name">None</span><span class="sf-desc">Torrents use your own connection.</span></label>
      <label class="sf-opt"><input type="radio" name="vpn" value="gluetun"><span class="sf-name">Gluetun</span><span class="sf-desc">ProtonVPN, Mullvad, PIA, NordVPN and others.</span></label>
    </div>
  </div>
  <div class="sf-group">
    <div class="sf-head"><span class="sf-title">Reverse proxy</span><span class="sf-sub">Puts Ryokan on your own domain with HTTPS. You need a domain.</span></div>
    <div class="sf-grid">
      <label class="sf-opt"><input type="radio" name="proxy" value="none" checked><span class="sf-name">None</span><span class="sf-desc">Reach Ryokan by address and port.</span></label>
      <label class="sf-opt"><input type="radio" name="proxy" value="caddy"><span class="sf-name">Caddy</span><span class="sf-desc">Gets certificates on its own. The easy one.</span></label>
      <label class="sf-opt"><input type="radio" name="proxy" value="traefik"><span class="sf-name">Traefik</span><span class="sf-desc">Configured with labels.</span></label>
      <label class="sf-opt"><input type="radio" name="proxy" value="nginx"><span class="sf-name">nginx</span><span class="sf-desc">You write the config and bring the certificates.</span></label>
      <label class="sf-opt"><input type="radio" name="proxy" value="cloudflared"><span class="sf-name">Cloudflare Tunnel</span><span class="sf-desc">No open ports on your router.</span></label>
    </div>
  </div>
  <div class="sf-group">
    <div class="sf-head"><span class="sf-title">Host check</span><span class="sf-sub">Ryokan's pages then only answer to an IP address, <code>localhost</code>, or the names you list. See <a href="../docker/#host-check">Host check</a>.</span></div>
    <div class="sf-grid">
      <label class="sf-opt"><input type="checkbox" name="host_check"><span class="sf-name">Turn on the host check</span><span class="sf-desc">Off by default.</span></label>
    </div>
    <div class="sf-fields">
      <label class="sf-field sf-wide"><span class="sf-label">Names you open Ryokan by</span><input type="text" name="allowed_hosts" value="" placeholder="nas ryokan.lan ryokan.example.com" spellcheck="false"><span class="sf-help">Separate them with spaces.</span></label>
    </div>
  </div>
</details>
<div class="sf-summary">
  <span class="sf-summary-label">Your stack</span>
  <span class="sf-chips" data-picker="summary"><span class="sf-chip">Ryokan</span><span class="sf-chip">qBittorrent</span><span class="sf-chip">Jellyfin</span></span>
</div>
<p class="sf-error" data-picker="problem" hidden></p>
</form>

!!! tip "Already running one of these?"
    Pick **None** for the media server if you have one, or untick the download client and remove it from the file in step 3. Your existing media server has to be able to read `/srv/media/anime`, and your existing download client has to save into `/srv/media/downloads`.

## 3. Create the folders and start the stack

Create the folders the stack uses, owned by the user from step 2:

<button type="button" data-copy="prep" class="stack-copy">Copy commands</button>

<pre data-picker="prep" class="stack-output"><code>sudo mkdir -p \
  /srv/media/downloads \
  /srv/media/anime \
  /srv/docker/ryokan \
  /srv/docker/qbittorrent \
  /srv/docker/jellyfin
sudo chown -R 1000:1000 \
  /srv/media/downloads \
  /srv/media/anime \
  /srv/docker/ryokan \
  /srv/docker/qbittorrent \
  /srv/docker/jellyfin</code></pre>

Next, make a folder for the stack itself, for example `/srv/stack`, and save this in it as `docker-compose.yml`:

<button type="button" data-copy="compose" class="stack-copy">Copy file</button>

<pre data-picker="compose" class="stack-output"><code class="language-yaml">Loading…</code></pre>

<div markdown data-when="client:rtorrent">

**rTorrent: set a password before the first start.** Without these two files, the ruTorrent page and the address Ryokan uses are open to anyone on your network. Put your own password in place of `REPLACE-WITH-YOUR-PASSWORD`:

```text
sudo mkdir -p /srv/docker/rutorrent/passwd
docker run --rm httpd:2.4-alpine htpasswd -Bbn admin "REPLACE-WITH-YOUR-PASSWORD" \
  | sudo tee /srv/docker/rutorrent/passwd/rutorrent.htpasswd > /dev/null
sudo cp /srv/docker/rutorrent/passwd/rutorrent.htpasswd /srv/docker/rutorrent/passwd/rpc.htpasswd
sudo chown -R 1000:1000 /srv/docker/rutorrent/passwd
```

The username is `admin`.

</div>

<div markdown data-when="server:plex">

**Plex: add a claim token before the first start.** Plex only lets you set up a new server from the same machine, or with a claim token. Get one at [plex.tv/claim](https://www.plex.tv/claim/), paste it into `PLEX_CLAIM` in the file, and start the stack within four minutes.

</div>

<div markdown data-when="vpn:gluetun">

**VPN: add your provider's details before the first start.** In the `gluetun` part of the file, set `VPN_SERVICE_PROVIDER` and the keys or login your provider gave you. The [Gluetun wiki](https://github.com/qdm12/gluetun-wiki) lists what each provider needs. If they are wrong, your torrent clients have no connection at all.

</div>

<div markdown data-when="proxy:caddy">

**Caddy: put your domain in the file.** Replace `ryokan.example.com` in the `caddy` part of the file with your own name, and point that name at this server.

</div>

<div markdown data-when="proxy:traefik">

**Traefik: put your email and domain in the file.** Replace `you@example.com` in the `traefik` part of the file. Then add the three labels from the comment under it to the `ryokan` service, with your own name in place of `ryokan.example.com`.

</div>

<div markdown data-when="proxy:nginx">

**nginx: write the config before the first start.** The comment in the `nginx` part of the file has a config to start from. Save it as `/srv/docker/nginx/conf.d/ryokan.conf`, and put your certificate files in `/srv/docker/nginx/certs`.

</div>

<div markdown data-when="proxy:cloudflared">

**Cloudflare Tunnel: paste your tunnel token into the file.** Create a tunnel in Cloudflare Zero Trust under **Networks → Tunnels**, and paste its token into `TUNNEL_TOKEN`. In the tunnel's settings, add a public hostname that points at `http://ryokan:8978`.

</div>

Start the stack from the folder that holds the file:

```sh
docker compose up -d
```

The first start downloads every app, which can take a few minutes. `docker compose ps` should then list each one as running.

The addresses below use `localhost`. If the server is another machine, use its name or IP address in place of `localhost`.

## 4. Set up your download client

Each client needs to save into `/srv/media/downloads`. <span data-when="mode:hardlink">Ryokan can only hardlink a finished file when the client saved it there.</span><span data-when="mode:copy mode:move">That is where Ryokan picks finished files up from.</span>

<div markdown data-when="client:qbittorrent">

### qBittorrent

1. qBittorrent prints a temporary password to its log the first time it starts. Find it with:

    ```sh
    docker compose logs qbittorrent | grep -i "temporary password"
    ```

2. Open <http://localhost:8080> and log in as `admin` with your temp password.
3. Go to **Tools → Options → Web UI** and set your own password under Authentication.
4. Go to **Tools → Options → Downloads** and set **Default Save Path** to `/srv/media/downloads`.
5. Click **Save**.

</div>

<div markdown data-when="client:deluge">

### Deluge

1. Open <http://localhost:8112> and log in with the default password `deluge`. Change it when Deluge asks.
2. Go to **Preferences → Downloads** and set **Download to** to `/srv/media/downloads`.
3. Go to **Preferences → Network**. Under Incoming Port, untick **Use Random Port** and set the port to <code data-v="peer-deluge">6881</code>. This is the port the stack opens for Deluge.
4. Click **Apply**.

</div>

<div markdown data-when="client:transmission">

### Transmission

1. Open <http://localhost:9091>. The stack runs Transmission without a login.
2. Open the preferences and go to **Torrents**. Set **Download to** to `/srv/media/downloads`.
3. Turn on **Use temporary folder** and set it to `/srv/media/downloads/incomplete`.

</div>

<div markdown data-when="client:rtorrent">

### rTorrent

rTorrent's image saves to its own folder, so its config file needs two changes. Open `/srv/docker/rutorrent/rtorrent/.rtorrent.rc` in an editor.

1. Add this line:

    ```text
    directory.default.set = /srv/media/downloads/temp
    ```

2. Change the line that starts with `method.insert = d.get_finished_dir` to:

    ```text
    method.insert = d.get_finished_dir, simple, "cat=/srv/media/downloads/complete/,$d.custom1="
    ```

3. Restart it:

    ```sh
    docker compose restart rutorrent
    ```

The ruTorrent page is at <http://localhost:8082>. Log in as `admin` with the password from step 3.

</div>

<div markdown data-when="client:sabnzbd">

### SABnzbd

1. Open <http://localhost:8081> and go through the setup wizard. This is where you enter your Usenet provider.
2. Go to **Config → Folders**. Set **Temporary Download Folder** to `/srv/media/downloads/incomplete` and **Completed Download Folder** to `/srv/media/downloads/complete`.
3. Go to **Config → Special** and add `sabnzbd` to **host_whitelist**. Save, and restart SABnzbd. Until you do, SABnzbd answers Ryokan with "Access denied".
4. Go to **Config → General → Security** and copy the **API Key**. Use the full API key, not the NZB key. You need it in step 7.

</div>

## 5. Set up your media server

<div markdown data-when="server:none">

You picked no media server, so there is nothing to do here. If you run one somewhere else, give it a library for shows that points at `/srv/media/anime`.

</div>

<div markdown data-when="server:jellyfin">

Open <http://localhost:8096> and go through Jellyfin's setup wizard.

1. Create an admin account. It is separate from Ryokan's.
2. Add a library with the content type **Shows**, and give it the folder `/srv/media/anime`.
3. Accept the defaults for everything else.

Then make a key for Ryokan. Go to **Dashboard → API Keys**, click **+**, and name it "Ryokan". Copy the key. You need it in the next step.

</div>

<div markdown data-when="server:emby">

Open <http://localhost:8096> and go through Emby's setup wizard.

1. Create an admin account. It is separate from Ryokan's.
2. Add a library with the content type **TV shows**, and give it the folder `/srv/media/anime`.
3. Accept the defaults for everything else.

Then make a key for Ryokan. Open **Settings** (the gear), go to **Advanced → API Keys**, click **New API Key**, and name it "Ryokan". Copy the key. You need it in the next step.

</div>

<div markdown data-when="server:plex">

Open <http://localhost:32400/web> and sign in with your Plex account.

1. Give the server a name.
2. Add a library of the type **TV Shows**, and give it the folder `/srv/media/anime`.

Then find your token for Ryokan. Once the library has something in it, open any episode, click **⋯ → Get Info → View XML**, and look at the address of the page that opens. It ends in `X-Plex-Token=`. The value after it is the token. Until then, leave the media server fields in the next step empty and add them later.

</div>

<div markdown data-when="server:any">

The library stays empty until Ryokan imports its first episode.

</div>

## 6. First login to Ryokan

Open <http://localhost:8978>. Ryokan asks you to create an account. Choose a username and a password of 8 to 72 characters. Ryokan has a single user, and this is it.

Next it asks where finished episodes should go:

| Field | Value |
|---|---|
| Media Root Path | `/srv/media/anime` |
| File operation mode | <span data-v="file-mode">Hardlink</span> |

<div markdown data-when="server:jellyfin">

The media server fields on the same page are optional, but filling them in means new episodes show up in Jellyfin straight away:

| Field | Value |
|---|---|
| Type | Jellyfin |
| URL | `http://jellyfin:8096` |
| API Key | The key you made in step 5 |

</div>

<div markdown data-when="server:emby">

The media server fields on the same page are optional, but filling them in means new episodes show up in Emby straight away:

| Field | Value |
|---|---|
| Type | Emby |
| URL | `http://emby:8096` |
| API Key | The key you made in step 5 |

</div>

<div markdown data-when="server:plex">

The media server fields on the same page are optional, but filling them in means new episodes show up in Plex straight away:

| Field | Value |
|---|---|
| Type | Plex |
| URL | `http://plex:32400` |
| API Key | Your token from step 5 |

</div>

Click **Save and continue**.

You can change any of this later under **Settings → General** and **Settings → Connections**. If you click **Skip for now** instead, Ryokan does not import finished downloads until you fill it in there.

## 7. Add your download client to Ryokan

In Ryokan, go to **Settings → Download Clients → Add download client**. Pick the client's kind, give it any name, and fill in the rest:

<div markdown data-when="client:qbittorrent">

**qBittorrent**

| Field | Value |
|---|---|
| URL | <code data-v="url-qbittorrent">http://qbittorrent:8080</code> |
| Username | `admin` |
| Password | The password you set in step 4 |
| Category | `anime` |
| Download path | Leave empty |

</div>

<div markdown data-when="client:deluge">

**Deluge**

| Field | Value |
|---|---|
| URL | <code data-v="url-deluge">http://deluge:8112</code> |
| Password | The password you set in step 4 |
| Category | `anime` |
| Download path | Leave empty |

</div>

<div markdown data-when="client:transmission">

**Transmission**

| Field | Value |
|---|---|
| URL | <code data-v="url-transmission">http://transmission:9091</code> |
| Username and password | Leave empty |
| Category | `anime` |
| Download path | Leave empty |

</div>

<div markdown data-when="client:rtorrent">

**rTorrent**

| Field | Value |
|---|---|
| URL | <code data-v="url-rtorrent">http://rutorrent:8000/RPC2</code> |
| Username | `admin` |
| Password | The password you set in step 3 |
| Category | `anime` |
| Download path | Leave empty |

</div>

<div markdown data-when="client:sabnzbd">

**SABnzbd**

| Field | Value |
|---|---|
| URL | <code data-v="url-sabnzbd">http://sabnzbd:8080</code> |
| API Key | The key you copied in step 4 |
| Category | `anime` |
| Download path | Leave empty |

</div>

Click **Test connection**. It should say Connected, with a version number. If it does not, see [Download clients](download-clients.md). Then save.

The first torrent client you add becomes the default for torrents, and the first Usenet client the default for Usenet.

!!! tip "If the client is not part of this stack"
    The addresses above are names that only work between the apps in this stack. For a client running anywhere else, use the address you would type in a browser, for example `http://192.168.1.100:8080`.

## 8. Add a show and watch it land

On the library page, click **+ Add Series**, type a title, and pick the show from the results.

On the series page, each episode has two buttons. **Interactive search** lists the available releases, best first, each with a **Grab** button. **Auto search** grabs the best one for you. **Search Monitored Episodes** at the top does the same for every monitored episode.

When the download finishes:

1. Ryokan places the file in `/srv/media/anime`, under `<show name>/Season 01/`. <span data-when="mode:hardlink">The file is hardlinked, so it takes no extra space and the client keeps seeding.</span><span data-when="mode:copy">The file is copied, so the client keeps seeding its own copy.</span><span data-when="mode:move">The file is moved out of the downloads folder, and Ryokan removes the finished download from the client.</span>
2. The media server shows the episode straight away if you connected it in step 6. Otherwise it appears after the server's next library scan.

That is a working install. The sections below are for the extras you picked, and for things you can add later.

<div markdown data-when="requests:seerr">

## Connect Seerr

Seerr talks to Ryokan the way it would talk to Sonarr and Radarr. Ryokan plays both parts.

**In Ryokan**, go to **Settings → Connections**. Under both Seerr sections, turn on **Enable Sonarr API** and **Enable Radarr API**, click **Generate** beside each API Key, and save. Both are off, with no key, on a new install.

**In Seerr**, open <http://localhost:5055> and go through its setup. <span data-when="server:jellyfin">When it asks for your media server, the hostname is `jellyfin` and the port is `8096`.</span><span data-when="server:emby">When it asks for your media server, the hostname is `emby` and the port is `8096`.</span> When it asks for services, add both of these:

| Field | Sonarr server (series) | Radarr server (films) |
|---|---|---|
| Hostname | `ryokan` | `ryokan` |
| Port | `8978` | `8978` |
| Use SSL | Off | Off |
| API Key | The Sonarr key from Ryokan | The Radarr key from Ryokan |
| URL Base | Leave empty | `/radarr` |
| Quality Profile | Default | Default |
| Root Folder | `/srv/media/anime` | `/srv/media/anime` |

Click **Test** on each before saving. The profile and folder lists fill in once the test passes.

Turn on **Enable Scan** for both if you want Seerr to show what is already in your library and the progress of a download.

</div>

<div markdown data-when="vpn:gluetun">

## VPN port forwarding

Check first that the VPN is up:

```sh
docker compose logs gluetun | tail -n 20
```

Your torrent clients now download through the VPN, but other peers cannot connect to you until a port is forwarded. Downloads still work without one. Uploads are slow, and private trackers mark you as unconnectable.

Port forwarding needs a provider that offers it. ProtonVPN, PIA, PrivateVPN and AirVPN do. Mullvad and NordVPN do not. If yours does not, remove the two `VPN_PORT_FORWARDING` lines from the file and skip this section.

First, find the port Gluetun was given:

```sh
docker exec gluetun cat /tmp/gluetun/forwarded_port
```

An empty answer means no port yet. Look in the Gluetun log for the reason.

Then enter that port in your torrent client:

<div markdown data-when="client:qbittorrent">

- **qBittorrent**: under **Tools → Options → Connection**, set **Port used for incoming connections** to the port and untick **Use UPnP / NAT-PMP port forwarding**. Under **Advanced**, set **Network Interface** to `tun0`.

</div>

<div markdown data-when="client:deluge">

- **Deluge**: under **Preferences → Network**, set **Incoming Port** to the port, set **Interface** to `tun0`, and turn off UPnP and NAT-PMP.

</div>

<div markdown data-when="client:transmission">

- **Transmission**: in the preferences under **Network**, set **Listening port** to the port and untick **Use UPnP or NAT-PMP port forwarding**.

</div>

<div markdown data-when="client:rtorrent">

- **rTorrent**: in `/srv/docker/rutorrent/rtorrent/.rtorrent.rc`, set `network.port_range.set` to the port twice with a dash between, such as `51234-51234`, and set `network.port_random.set = no`. Then restart it.

</div>

Some providers change the port when the VPN reconnects. If uploads stop, read the port again and update the client.

<div markdown data-when="client:sabnzbd">

SABnzbd stays outside the VPN. Usenet is an encrypted connection to your provider, with no peers.

</div>

</div>

<div markdown data-when="proxy:any">

## Reverse proxy

Once Ryokan opens on your domain over HTTPS, add this to the `environment` part of the `ryokan` service in the file and run `docker compose up -d` again:

```yaml
RYOKAN_TRUSTED_PROXY: "1"
```

Ryokan then reads the visitor's real address from the proxy, and marks the login cookie as secure. Only set it behind a proxy. See [Docker reference](docker.md).

<div markdown data-when="hostcheck:on">

The host check is on, so your domain has to be in the list of names in step 2. Without it, every page opened through the proxy answers with a "421" error.

</div>

</div>

<div markdown data-when="indexer:none">

## Add an indexer (optional)

Nyaa is built in, so Ryokan can search for anime without a separate indexer.

If you already run Prowlarr or Jackett, go to **Settings → Indexers → Add indexer** in Ryokan. Paste the indexer's URL, which ends in `/api`, and its API key, give it a name, and click **Test**. To run one as part of this stack, pick it under **More options** in step 2.

</div>

<div markdown data-when="indexer:prowlarr">

## Add indexers with Prowlarr

Nyaa is built in, so Ryokan already searches it. Prowlarr adds other trackers and Usenet indexers.

**In Prowlarr**, open <http://localhost:9696> and create a login when it asks.

1. Go to **Indexers → Add Indexer** and add the ones you use.
2. Go to **Settings → General** and copy the **API Key**.

**In Ryokan**, go to **Settings → Indexers → Add indexer**, once for each indexer in Prowlarr:

| Field | Value |
|---|---|
| Name | Anything, such as the indexer's name |
| Kind | `torznab` for a torrent indexer, `newznab` for a Usenet one |
| URL | `http://prowlarr:9696/1/api` |
| API Key | The key from Prowlarr |

The `1` in the URL is the indexer's number in Prowlarr. To find it, click the indexer in Prowlarr's list. Its Torznab Url ends in a number and `/api`.

Click **Test**, then save.

</div>

<div markdown data-when="indexer:jackett">

## Add indexers with Jackett

Nyaa is built in, so Ryokan already searches it. Jackett adds other trackers.

**In Jackett**, open <http://localhost:9117>.

1. Click **Add indexer** and add the ones you use.
2. Copy the **API Key** from the top right of the page.

**In Ryokan**, go to **Settings → Indexers → Add indexer**, once for each indexer in Jackett:

| Field | Value |
|---|---|
| Name | Anything, such as the indexer's name |
| Kind | `torznab` |
| URL | `http://jackett:9117/api/v2.0/indexers/NAME/results/torznab/api` |
| API Key | The key from Jackett |

Put the indexer's own name in place of `NAME`. To find it, click **Copy Torznab Feed** beside the indexer in Jackett and look at the address it copies.

Click **Test**, then save.

</div>

## Link AniList or MAL (optional)

To have Ryokan add shows when you mark them as watching on AniList or MAL, see **[External accounts](external-accounts.md)**. Shows you added by hand are not affected.

## If something goes wrong

- **The media server cannot read the files.** Ryokan and the media server are then running as different users. Give every app the same user and group ID in step 2, and run the `chown` command from step 3 again. See [PUID and PGID](install.md#puid-and-pgid).
- **An app keeps restarting.** `docker compose logs <name>` shows why. The usual cause is a settings folder that was not created in step 3.
- **[Troubleshooting](troubleshooting.md)** covers the rest.

## What next?

- **[Configuration](configuration.md)**: every Settings tab.
- **[Importing an existing library](manual-import.md)**: bring in anime you already have without downloading it again.
- **[How releases are scored](scoring.md)**: why Ryokan picks the release it picks.

---

*Last updated: 2026-11-18.*
