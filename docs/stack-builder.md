---
title: Stack builder
hide:
  - toc
---

# Stack builder

Pick the pieces you want and copy the generated `docker-compose.yml` straight into your homelab. Everything is pre-wired: paths line up so post-processing hardlinks work without fiddling, PUID/PGID is consistent, and the per-client Ryokan settings are printed alongside so you know what to paste into Settings → Download Clients.

This is opinionated. Sane defaults beat a config matrix. If you need something the picker doesn't generate, copy the output and edit by hand; the comments call out the load-bearing bits.

<form id="stack-form" class="stack-form">

<fieldset>
  <legend>Download client(s)</legend>
  <p class="hint">Multi-select. Pick more than one to set up multi-client routing (Ryokan picks one default per protocol). Indexers can be pinned per-client in Ryokan's Settings.</p>
  <label><input type="checkbox" name="dlclient" value="qbittorrent" checked> qBittorrent</label>
  <label><input type="checkbox" name="dlclient" value="deluge"> Deluge</label>
  <label><input type="checkbox" name="dlclient" value="transmission"> Transmission</label>
  <label><input type="checkbox" name="dlclient" value="rtorrent"> rTorrent (ruTorrent)</label>
  <label><input type="checkbox" name="dlclient" value="sabnzbd"> SABnzbd</label>
</fieldset>

<fieldset>
  <legend>Media server</legend>
  <p class="hint">Ryokan integrates with Jellyfin (library refresh, on-disk validation). Plex and Emby aren't supported as integrations.</p>
  <label><input type="radio" name="media_server" value="jellyfin" checked> Jellyfin</label>
  <label><input type="radio" name="media_server" value="none"> None</label>
</fieldset>

<fieldset>
  <legend>Request frontend</legend>
  <p class="hint">Seerr requests anime through Ryokan via the Sonarr/Radarr API shim.</p>
  <label><input type="radio" name="requests" value="seerr" checked> Seerr</label>
  <label><input type="radio" name="requests" value="none"> None</label>
</fieldset>

<fieldset>
  <legend>VPN</legend>
  <p class="hint">Routes torrent download clients through Gluetun's network namespace. SAB stays outside the VPN (Usenet talks TLS to your provider, not to peers). You'll need to fill in your provider credentials in the generated compose; gluetun's wiki at <a href="https://github.com/qdm12/gluetun-wiki" target="_blank" rel="noopener">qdm12/gluetun-wiki</a> lists the env vars per provider.</p>
  <label><input type="radio" name="vpn" value="none" checked> None</label>
  <label><input type="radio" name="vpn" value="gluetun"> Gluetun (Mullvad, ProtonVPN, PIA, NordVPN, custom)</label>
</fieldset>

<fieldset>
  <legend>Reverse proxy</legend>
  <p class="hint">Adds the proxy container and stub config; you'll still need to point a real domain at it. Cloudflare Tunnel skips the proxy container entirely (Cloudflare's edge does TLS).</p>
  <label><input type="radio" name="proxy" value="none" checked> None</label>
  <label><input type="radio" name="proxy" value="caddy"> Caddy</label>
  <label><input type="radio" name="proxy" value="traefik"> Traefik</label>
  <label><input type="radio" name="proxy" value="nginx"> nginx (manual config)</label>
  <label><input type="radio" name="proxy" value="cloudflared"> Cloudflare Tunnel</label>
</fieldset>

<fieldset>
  <legend>Hardening</legend>
  <p class="hint">The host check makes Ryokan's web UI answer only to the names you open it by, so a page on another site can't point its own name at Ryokan and use it (DNS rebinding). Off by default. IP addresses and <code>localhost</code> always work. List every name you open Ryokan by, such as your server's name (<code>nas</code>), <code>ryokan.lan</code> or your reverse proxy's domain. Inside Docker, Ryokan doesn't know your server's name unless you list it. Seerr, autobrr and calendar apps aren't affected. See <a href="docker.md#host-check">Host check</a>.</p>
  <label><input type="checkbox" name="host_check"> Host check</label>
  <label>Names you open Ryokan by <input type="text" name="allowed_hosts" value="" placeholder="e.g. nas ryokan.lan ryokan.example.com"></label>
</fieldset>

<fieldset>
  <legend>User / group</legend>
  <p class="hint">Run <code>id -u</code> / <code>id -g</code> on the host to find these. Match the user that owns your media library so post-processed files land with the right ownership.</p>
  <label>PUID <input type="number" name="puid" value="1000" min="0"></label>
  <label>PGID <input type="number" name="pgid" value="1000" min="0"></label>
  <label>TZ <input type="text" name="tz" value="UTC" placeholder="e.g. America/Chicago"></label>
</fieldset>

<fieldset>
  <legend>Host paths</legend>
  <p class="hint">Where on your host the data lives. The shared folder holds <code>downloads/</code> and the library, <code>anime/</code>, and every container mounts it at the same path it has on the host, so imports hardlink and the paths you type in Ryokan are the ones you see on the host. It has to be a full path, such as <code>/srv/media</code> or <code>/data</code>. If a folder can't work, for example because an app keeps its own settings there, the output below says why instead of showing a compose file. Per-service config goes under <code>/srv/docker/&lt;service&gt;/</code>.</p>
  <label>Shared media folder <input type="text" name="shared_path" value="/srv/media"></label>
  <label>Per-service config root <input type="text" name="appdata_path" value="/srv/docker"></label>
</fieldset>

</form>

## Generated `docker-compose.yml`

<button type="button" id="copy-compose" class="md-button md-button--primary">Copy</button>

<pre data-picker="compose" class="stack-output"><code class="language-yaml">Loading…</code></pre>

## Ryokan settings to paste in

After the stack is up, set each download client's folders as listed, then open Ryokan at `http://localhost:8978`. Create your account, enter the library values when first-run setup asks for them, and paste the rest into the matching Settings panels.

<pre data-picker="settings" class="stack-output"><code>Loading…</code></pre>

## Notes

- **Hardlinks**: post-processing defaults to hardlink mode, and a hardlink needs the download and the library inside one mount. Every container mounts the shared folder at its host path, so the client reports `/srv/media/downloads/foo.mkv` and Ryokan opens that same path and links it into `/srv/media/anime`. That only works once each client saves into the shared folder, which is why the settings above list a download folder for every client. If a hardlink fails anyway (the folder spans two filesystems), Ryokan copies instead.
- **First-run**: every container needs its `/srv/docker/<service>/` subdirectory pre-owned by the PUID/PGID you set above. The generated compose's header comment includes the exact `mkdir + chown` for the services you picked.
- **Reverse proxy**: the generated config is a stub. You'll need to point a real domain (or Cloudflare Tunnel route) at the proxy and edit the proxy's config file with your actual hostname.
- **VPN**: Gluetun expects WireGuard or OpenVPN credentials in its env. Wrong/missing credentials show up as connection failures on the download client (which can't reach trackers). The gluetun container's logs are the right place to debug.
- **SABnzbd "Access denied"**: SABnzbd checks the hostname it is called by, and Ryokan calls it `sabnzbd`. Until that name is in **Config → Special → host_whitelist**, SAB answers Ryokan with "Access denied - Hostname verification failed" (a 403 in Settings → Download Clients). Add it, save, and restart SAB. qBittorrent's own host check accepts any name by default, so it needs nothing.
- **Torrent ports**: each torrent client that listens for peers gets its own host port, starting at 6881, so picking several never publishes one port twice. Deluge starts on a random port until you set its Incoming Port to the one in its settings above.

---

*Last updated: 2026-10-04.*
