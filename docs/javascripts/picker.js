// Stack-builder generator. Renders a docker-compose.yml + Ryokan
// settings snippet from the form on `docs/stack-builder.md`. Pure
// vanilla JS, no framework — lives at the document level so the
// site's `extra_javascript` config wires it in without a build step.
//
// Design discipline: every option the form exposes must produce a
// compose snippet that runs as-is. No "edit before use" knobs except
// where explicitly called out (reverse-proxy domain, Gluetun VPN
// credentials). When in doubt, drop the option rather than ship a
// half-baked combination.

(function () {
  'use strict';

  // Per-client config matrix. The generator templates these into
  // service blocks plus the Ryokan settings snippet.
  const CLIENTS = {
    qbittorrent: {
      label: 'qBittorrent',
      image: 'lscr.io/linuxserver/qbittorrent:latest',
      port: 8080,
      extra_ports: ['6881:6881', '6881:6881/udp'],
      category: 'anime',
      download_path: '/downloads',
      default_url: 'http://qbittorrent:8080',
      config_dir: 'qbittorrent',
      env: { WEBUI_PORT: '8080' },
      protocol: 'torrent',
    },
    deluge: {
      label: 'Deluge',
      image: 'lscr.io/linuxserver/deluge:latest',
      port: 8112,
      extra_ports: ['6881:6881', '6881:6881/udp'],
      category: 'anime',
      download_path: '/downloads',
      default_url: 'http://deluge:8112',
      config_dir: 'deluge',
      env: {},
      protocol: 'torrent',
    },
    transmission: {
      label: 'Transmission',
      image: 'lscr.io/linuxserver/transmission:latest',
      port: 9091,
      extra_ports: ['51413:51413', '51413:51413/udp'],
      category: 'anime',
      download_path: '/downloads',
      default_url: 'http://transmission:9091',
      config_dir: 'transmission',
      env: {},
      protocol: 'torrent',
    },
    rtorrent: {
      label: 'rTorrent (ruTorrent)',
      // Compose-side service name. Distinct from the kind key
      // (`rtorrent`) because the rest of the docs (quick-start.md,
      // download-clients.md) refer to the container as `rutorrent`,
      // matching the upstream image's natural labeling. Use this
      // wherever the container name leaks through: the compose
      // service block, the URL Ryokan dials, and the appdata path.
      service_name: 'rutorrent',
      // crazymax/rtorrent-rutorrent is the maintained image;
      // linuxserver's rutorrent was deprecated and their README
      // points users at this one.
      image: 'crazymax/rtorrent-rutorrent:latest',
      // XML-RPC; this is what Ryokan talks to. Reachable via
      // Docker DNS only (`expose_main_port: false`); we don't
      // need to host-map it.
      port: 8000,
      expose_main_port: false,
      // 8082:8080 host:container = ruTorrent web UI.
      // 50000 = inbound BT peer connections.
      // 6881/udp = DHT.
      extra_ports: ['8082:8080', '50000:50000', '6881:6881/udp'],
      category: 'anime',
      download_path: '/downloads',
      // /RPC2 path is required; rTorrent's XML-RPC endpoint.
      default_url: 'http://rutorrent:8000/RPC2',
      config_dir: 'rutorrent',
      // crazy-max image's config volume is /data (linuxserver was /config).
      config_mount_target: '/data',
      env: {},
      protocol: 'torrent',
    },
    sabnzbd: {
      label: 'SABnzbd',
      image: 'lscr.io/linuxserver/sabnzbd:latest',
      // SAB listens on 8080 inside the container; we map it to host
      // 8081 so the conventional 8080 stays free for qBit-on-host.
      // Ryokan reaches SAB through Docker DNS at the *container*
      // port (8080), not the host port (8081).
      port: 8080,
      host_port: 8081,
      extra_ports: [],
      category: 'anime',
      download_path: '/downloads',
      default_url: 'http://sabnzbd:8080',
      config_dir: 'sabnzbd',
      env: {},
      protocol: 'usenet',
    },
  };

  function readForm() {
    const form = document.getElementById('stack-form');
    if (!form) return null;
    const dlclients = Array.from(
      form.querySelectorAll('input[name="dlclient"]:checked')
    ).map((el) => el.value);
    const get = (name) => form.querySelector(`[name="${name}"]`);
    const radio = (name) =>
      form.querySelector(`input[name="${name}"]:checked`).value;
    return {
      dlclients,
      media_server: radio('media_server'),
      requests: radio('requests'),
      vpn: radio('vpn'),
      proxy: radio('proxy'),
      puid: get('puid').value || '1000',
      pgid: get('pgid').value || '1000',
      tz: get('tz').value || 'UTC',
      paths: {
        downloads: get('downloads_path').value || '/srv/media/downloads',
        media: get('media_path').value || '/srv/media/anime',
        appdata: get('appdata_path').value || '/srv/docker',
      },
    };
  }

  // Whether a given download client should sit behind the VPN.
  // Usenet doesn't care about IP-leak the way BT does (the protocol
  // talks to commercial Usenet providers over TLS, not peers), so
  // we leave SAB out of the gluetun network even when VPN is on.
  // Users who *want* SAB through the VPN can move it manually.
  function isBehindVpn(kind, cfg) {
    if (cfg.vpn !== 'gluetun') return false;
    return CLIENTS[kind].protocol === 'torrent';
  }

  // The URL Ryokan should use to reach a given client. Behind gluetun
  // the client shares gluetun's network namespace — its container
  // name doesn't resolve on the user-defined media network, only
  // `gluetun` does — so we rewrite the host part of `default_url` to
  // target `gluetun` while preserving the port and path. Port works
  // out: containers in a shared namespace see each other's listeners
  // on localhost, so qBit's 8080 inside that namespace is reachable
  // as gluetun:8080 from peer containers on `media`. Standalone (no
  // VPN) clients keep their own container-name URL.
  function urlForClient(kind, cfg) {
    const c = CLIENTS[kind];
    if (isBehindVpn(kind, cfg)) {
      // Use service_name (compose-side) rather than kind (form-side)
      // since the URL contains the container name, not the form value.
      // Same difference for everyone except rtorrent (kind=rtorrent,
      // service=rutorrent).
      const serviceName = c.service_name || kind;
      return c.default_url.replace(`://${serviceName}:`, '://gluetun:');
    }
    return c.default_url;
  }

  function renderClient(kind, cfg) {
    const c = CLIENTS[kind];
    const behindVpn = isBehindVpn(kind, cfg);

    // When behind gluetun, the download client shares gluetun's
    // network namespace via `network_mode: "service:gluetun"`. Host
    // ports get exposed on the gluetun container instead; they
    // can't coexist with `network_mode: service:`. The download
    // client's own `networks:` and `ports:` blocks must be omitted.
    //
    // `host_port` overrides the host side of the main port mapping
    // when host and container ports diverge (SAB listens on 8080
    // inside but is mapped to 8081 outside). `expose_main_port:
    // false` skips the main mapping entirely (rTorrent's XML-RPC
    // is reachable via Docker DNS only; no host expose needed).
    const mainMapping =
      c.expose_main_port === false
        ? null
        : `"${c.host_port || c.port}:${c.port}"`;
    const portsList = behindVpn
      ? null
      : [
          ...(mainMapping ? [mainMapping] : []),
          ...c.extra_ports.map((p) => `"${p}"`),
        ];

    const baseEnv = [
      `      PUID: "${cfg.puid}"`,
      `      PGID: "${cfg.pgid}"`,
      `      TZ: "${cfg.tz}"`,
    ];
    const extraEnv = Object.entries(c.env).map(
      ([k, v]) => `      ${k}: "${v}"`
    );
    const envBlock = baseEnv.concat(extraEnv).join('\n');

    // Compose service name and container name. Defaults to the kind
    // key; overridable via `service_name` (currently rtorrent →
    // rutorrent so the URL Ryokan dials matches the rest of the docs).
    const svc = c.service_name || kind;
    const lines = [`  ${svc}:`];
    lines.push(`    image: ${c.image}`);
    lines.push(`    container_name: ${svc}`);
    if (behindVpn) {
      lines.push('    network_mode: "service:gluetun"');
      lines.push('    depends_on:');
      lines.push('      - gluetun');
    } else {
      lines.push('    networks: [media]');
      if (portsList && portsList.length > 0) {
        lines.push('    ports:');
        portsList.forEach((p) => lines.push(`      - ${p}`));
      }
    }
    lines.push('    volumes:');
    const configTarget = c.config_mount_target || '/config';
    lines.push(`      - ${cfg.paths.appdata}/${c.config_dir}:${configTarget}`);
    // SAB splits in-progress (`/incomplete-downloads`) from completed
    // (`/downloads`); without a mount for the incomplete side, SAB
    // falls back to writing it under /config which clutters the
    // config volume and makes pause/resume across container restarts
    // unreliable.
    if (kind === 'sabnzbd') {
      lines.push(`      - ${cfg.paths.appdata}/sabnzbd/incomplete:/incomplete-downloads`);
    }
    // rTorrent's crazy-max image looks for htpasswd files at /passwd
    // (rutorrent.htpasswd for the web UI, rpc.htpasswd for XML-RPC).
    // Mount the folder unconditionally so users can drop files in to
    // enable auth without having to edit the compose. When the folder
    // is empty, the image runs without auth.
    if (kind === 'rtorrent') {
      // Note: this nests under the same parent the rTorrent /data
      // mount uses (${appdata}/rutorrent for /data,
      // ${appdata}/rutorrent/passwd for /passwd). Container targets
      // are independent so the host-side overlap is harmless; the
      // docs/quick-start does the same layering.
      lines.push(`      - ${cfg.paths.appdata}/rutorrent/passwd:/passwd`);
    }
    lines.push(`      - ${cfg.paths.downloads}:${c.download_path}`);
    lines.push('    environment:');
    lines.push(envBlock);
    lines.push('    restart: unless-stopped');
    return lines.join('\n');
  }

  function renderRyokan(cfg) {
    const deps = cfg.dlclients.slice();
    if (cfg.vpn === 'gluetun' && deps.some((k) => isBehindVpn(k, cfg))) {
      // Behind-VPN download clients depend on gluetun; Ryokan
      // depends on them, so transitively gluetun starts first.
      // Keeping the explicit list makes startup ordering visible
      // in `docker compose ps`.
    }
    const dependsList = deps.length
      ? `    depends_on:\n${deps.map((k) => `      - ${k}`).join('\n')}\n`
      : '';
    return `  ryokan:
    image: ghcr.io/johnthreekay/ryokan:latest
    container_name: ryokan
    networks: [media]
    ports:
      - "8978:8978"
    volumes:
      - ${cfg.paths.appdata}/ryokan:/data
      - ${cfg.paths.downloads}:/downloads
      - ${cfg.paths.media}:/media/anime
    environment:
      PUID: "${cfg.puid}"
      PGID: "${cfg.pgid}"
      TZ: "${cfg.tz}"
      RUST_LOG: ryokan=info
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://localhost:8978/login"]
      interval: 30s
      timeout: 5s
      start_period: 30s
      retries: 3
${dependsList}    restart: unless-stopped`;
  }

  function renderJellyfin(cfg) {
    return `  jellyfin:
    image: lscr.io/linuxserver/jellyfin:latest
    container_name: jellyfin
    networks: [media]
    ports:
      - "8096:8096"
    devices:
      # /dev/dri enables Intel/AMD hardware transcode. Remove this
      # line if your host has no iGPU or you don't need transcoding.
      - /dev/dri:/dev/dri
    volumes:
      - ${cfg.paths.appdata}/jellyfin:/config
      - ${cfg.paths.media}:/data/media:ro
    environment:
      PUID: "${cfg.puid}"
      PGID: "${cfg.pgid}"
      TZ: "${cfg.tz}"
    restart: unless-stopped`;
  }

  function renderSeerr(cfg) {
    const deps = ['ryokan'];
    if (cfg.media_server === 'jellyfin') deps.push('jellyfin');
    return `  seerr:
    image: ghcr.io/seerr-team/seerr:latest
    container_name: seerr
    init: true
    networks: [media]
    ports:
      - "5055:5055"
    volumes:
      - ${cfg.paths.appdata}/seerr:/app/config
    environment:
      PUID: "${cfg.puid}"
      PGID: "${cfg.pgid}"
      TZ: "${cfg.tz}"
    depends_on:
${deps.map((d) => `      - ${d}`).join('\n')}
    restart: unless-stopped`;
  }

  function renderGluetun(cfg) {
    // Forward each behind-VPN download client's host port through
    // gluetun's network namespace. Without these, the WebUI is
    // unreachable from the host even though the container is
    // running fine inside the VPN namespace. Mirrors the
    // host_port / expose_main_port logic in renderClient so
    // SAB (host:container divergence) and rTorrent (XML-RPC
    // not host-exposed) work correctly here too. SAB is usenet
    // so it never passes the isBehindVpn filter, but the shape
    // stays consistent.
    const portForwards = cfg.dlclients
      .filter((k) => isBehindVpn(k, cfg))
      .flatMap((k) => {
        const c = CLIENTS[k];
        const mainMapping =
          c.expose_main_port === false
            ? null
            : `      - "${c.host_port || c.port}:${c.port}"`;
        return [
          ...(mainMapping ? [mainMapping] : []),
          ...c.extra_ports.map((p) => `      - "${p}"`),
        ];
      });
    const portsBlock = portForwards.length
      ? `    ports:\n${portForwards.join('\n')}\n`
      : '';
    return `  gluetun:
    image: qmcgaw/gluetun:latest
    container_name: gluetun
    cap_add:
      - NET_ADMIN
    devices:
      - /dev/net/tun:/dev/net/tun
    networks: [media]
${portsBlock}    volumes:
      - ${cfg.paths.appdata}/gluetun:/gluetun
    environment:
      # ---- VPN provider config ----
      # Pick your provider and protocol; gluetun's docs at
      # https://github.com/qdm12/gluetun-wiki list the env vars
      # each provider expects. Common shape:
      VPN_SERVICE_PROVIDER: "protonvpn"        # or pia, privatevpn, airvpn, mullvad, nordvpn, custom, etc.
      VPN_TYPE: "wireguard"                    # or openvpn
      WIREGUARD_PRIVATE_KEY: "PASTE_KEY_HERE"
      WIREGUARD_ADDRESSES: "10.x.x.x/32"
      SERVER_CITIES: "Amsterdam"               # provider-specific filter
      TZ: "${cfg.tz}"
      # ---- Port forwarding ----
      # Without an open inbound port, torrent clients can only make
      # outbound connections; peers can't dial in, which tanks leech
      # speed and ratio-building on private trackers. Flip this on
      # if your provider supports port forwarding.
      #
      # Provider support (as of 2026):
      #   ProtonVPN  yes (auto-renews, 60s lease)
      #   PIA        yes (single port, may rotate on reconnect)
      #   PrivateVPN yes
      #   AirVPN     yes (configure the port in their portal first)
      #   Mullvad    NO. Dropped port forwarding in 2023; switch
      #              providers if you need it.
      #   NordVPN    no
      VPN_PORT_FORWARDING: "on"
      # Gluetun writes the assigned port number to this file inside
      # its container. Read it with:
      #   docker exec gluetun cat /tmp/gluetun/forwarded_port
      # See the "Port forwarding" section in the settings snippet
      # below for the qBittorrent / Deluge plumbing.
      VPN_PORT_FORWARDING_STATUS_FILE: "/tmp/gluetun/forwarded_port"
    restart: unless-stopped
    # All torrent download clients in this stack share gluetun's
    # network namespace via \`network_mode: "service:gluetun"\` and
    # depend on gluetun starting first. SAB is left out of the VPN
    # because Usenet talks TLS to your provider, not to peers.`;
  }

  function renderProxy(cfg) {
    if (cfg.proxy === 'none') return null;
    if (cfg.proxy === 'caddy') {
      return `  caddy:
    image: caddy:2-alpine
    container_name: caddy
    networks: [media]
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ${cfg.paths.appdata}/caddy/Caddyfile:/etc/caddy/Caddyfile
      - ${cfg.paths.appdata}/caddy/data:/data
      - ${cfg.paths.appdata}/caddy/config:/config
    restart: unless-stopped
    # Stub Caddyfile; drop your hostname in. Caddy auto-provisions
    # Let's Encrypt certs once a real domain points at this host.
    # Example:
    #   ryokan.example.com {
    #       reverse_proxy ryokan:8978
    #   }`;
    }
    if (cfg.proxy === 'traefik') {
      return `  traefik:
    image: traefik:v3
    container_name: traefik
    networks: [media]
    ports:
      - "80:80"
      - "443:443"
      - "8080:8080"  # dashboard; remove for prod
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - ${cfg.paths.appdata}/traefik/traefik.yml:/etc/traefik/traefik.yml
      - ${cfg.paths.appdata}/traefik/acme.json:/acme.json
    command:
      - --api.insecure=true
      - --providers.docker=true
      - --providers.docker.exposedbydefault=false
      - --entrypoints.web.address=:80
      - --entrypoints.websecure.address=:443
      - --certificatesresolvers.le.acme.email=you@example.com
      - --certificatesresolvers.le.acme.storage=/acme.json
      - --certificatesresolvers.le.acme.tlschallenge=true
    restart: unless-stopped
    # Add Traefik labels to each service you want exposed, e.g.:
    #   labels:
    #     - traefik.enable=true
    #     - traefik.http.routers.ryokan.rule=Host(\`ryokan.example.com\`)
    #     - traefik.http.routers.ryokan.tls.certresolver=le`;
    }
    if (cfg.proxy === 'nginx') {
      return `  nginx:
    image: nginx:alpine
    container_name: nginx
    networks: [media]
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ${cfg.paths.appdata}/nginx/nginx.conf:/etc/nginx/nginx.conf:ro
      - ${cfg.paths.appdata}/nginx/conf.d:/etc/nginx/conf.d:ro
      - ${cfg.paths.appdata}/nginx/certs:/etc/nginx/certs:ro
    restart: unless-stopped
    # Manual config. Drop a server block at
    # ${cfg.paths.appdata}/nginx/conf.d/ryokan.conf, e.g.:
    #
    #   server {
    #       listen 443 ssl;
    #       server_name ryokan.example.com;
    #       ssl_certificate /etc/nginx/certs/fullchain.pem;
    #       ssl_certificate_key /etc/nginx/certs/privkey.pem;
    #       location / {
    #           proxy_pass http://ryokan:8978;
    #           proxy_set_header Host \$host;
    #           proxy_set_header X-Real-IP \$remote_addr;
    #           proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
    #       }
    #   }
    #
    # nginx doesn't auto-provision certs; pair with certbot or bring
    # your own. If you set RYOKAN_TRUSTED_PROXY=1, make sure nginx
    # strips and rewrites X-Forwarded-* headers on ingress.`;
    }
    if (cfg.proxy === 'cloudflared') {
      return `  cloudflared:
    image: cloudflare/cloudflared:latest
    container_name: cloudflared
    networks: [media]
    command: tunnel --no-autoupdate run
    environment:
      TUNNEL_TOKEN: "PASTE_YOUR_TUNNEL_TOKEN_HERE"
    restart: unless-stopped
    # Cloudflare Tunnel: no host ports needed. Cloudflare's edge
    # punches out to this container over a persistent connection.
    # Configure the tunnel route in the Cloudflare Zero Trust
    # dashboard to forward your hostname (e.g. ryokan.example.com)
    # to http://ryokan:8978. TLS is handled at Cloudflare's edge,
    # so set RYOKAN_TRUSTED_PROXY=1 in Ryokan's env.`;
    }
    return null;
  }

  function renderCompose(cfg) {
    if (cfg.dlclients.length === 0) {
      return '# Pick at least one download client.\n';
    }

    const services = [];
    services.push(renderRyokan(cfg));
    cfg.dlclients.forEach((kind) => services.push(renderClient(kind, cfg)));
    if (cfg.vpn === 'gluetun') services.push(renderGluetun(cfg));
    if (cfg.media_server === 'jellyfin') services.push(renderJellyfin(cfg));
    if (cfg.requests === 'seerr') services.push(renderSeerr(cfg));
    const proxy = renderProxy(cfg);
    if (proxy) services.push(proxy);

    // Enumerate per-service appdata subdirectories so the mkdir
    // pre-creates each one with the right ownership. Without this,
    // Docker creates lazy bind-mount targets as root on first up
    // and a Jellyfin / Seerr / non-linuxserver container can fail
    // to write to its own config volume.
    const serviceDirs = ['ryokan'];
    cfg.dlclients.forEach((k) => serviceDirs.push(CLIENTS[k].config_dir));
    if (cfg.media_server === 'jellyfin') serviceDirs.push('jellyfin');
    if (cfg.requests === 'seerr') serviceDirs.push('seerr');
    if (cfg.vpn === 'gluetun') serviceDirs.push('gluetun');
    if (cfg.proxy === 'caddy') serviceDirs.push('caddy');
    if (cfg.proxy === 'traefik') serviceDirs.push('traefik');
    if (cfg.proxy === 'nginx') serviceDirs.push('nginx');
    const appdataPaths = serviceDirs
      .map((d) => `${cfg.paths.appdata}/${d}`)
      .join(' ');

    const header = `# =============================================================================
# Ryokan stack: generated from the picker
# =============================================================================
#
# Before first \`docker compose up\`:
#   sudo mkdir -p ${cfg.paths.downloads} ${cfg.paths.media} ${appdataPaths}
#   sudo chown -R ${cfg.puid}:${cfg.pgid} ${cfg.paths.downloads} ${cfg.paths.media} ${cfg.paths.appdata}
#
# Path layout: ${cfg.paths.downloads} (downloads) and ${cfg.paths.media}
# (library) should be on the same filesystem so post-processing can
# hardlink instead of copying. Both are mounted into Ryokan AND the
# download client(s) at matching paths inside the container, so no
# per-client \`download_path\` translation is needed in Settings.
#
# =============================================================================

networks:
  media:
    name: media

services:
`;
    return header + services.join('\n\n') + '\n';
  }

  function renderSettings(cfg) {
    const lines = [];
    lines.push('--- Settings → Download Clients ---');
    lines.push('');
    if (cfg.dlclients.length === 0) {
      lines.push('Add a download client first.');
    } else {
      cfg.dlclients.forEach((kind, i) => {
        const c = CLIENTS[kind];
        const url = urlForClient(kind, cfg);
        const vpnNote = isBehindVpn(kind, cfg)
          ? '   # behind gluetun: talk to it via the gluetun container'
          : '';
        lines.push(`${c.label}:`);
        lines.push(`  URL:           ${url}${vpnNote}`);
        if (kind === 'qbittorrent') {
          // qBit 4.6.1+ removed the hardcoded admin/adminadmin default
          // and instead generates a random temporary password on first
          // start, printed only to stdout. Direct users to docker logs.
          lines.push('  Username:      admin');
          lines.push('  Password:      qBit 4.6.1+ generates a random temp password on first start.');
          lines.push('                 Find it with:  docker logs qbittorrent | grep -i "temporary password"');
          lines.push('                 Log in with that, set a permanent password under');
          lines.push('                 Tools → Options → Web UI → Authentication, then paste it here.');
          // qBit 4.5+ enables Host header validation by default. The
          // Host header on requests from Ryokan-in-container is
          // `qbittorrent:8080`, which qBit rejects with 401 even when
          // the credentials are correct. Symptom: Ryokan shows the
          // row stuck on "qBittorrent Unauthorized" while the WebUI
          // works fine from a browser. Standard homelab workaround
          // is to disable the check entirely.
          lines.push('  Heads-up:      Settings → Connections will say "qBittorrent Unauthorized" even with');
          lines.push('                 correct credentials until you turn off Host header validation.');
          lines.push('                 Tools → Options → Web UI → uncheck "Enable Host header validation",');
          lines.push('                 Save, then `docker compose restart qbittorrent`.');
        } else if (kind === 'rtorrent') {
          // crazy-max image looks for /passwd/rutorrent.htpasswd (web UI)
          // and /passwd/rpc.htpasswd (XML-RPC). Without files, both are
          // unauthenticated. The compose mounts /passwd unconditionally
          // so users can drop files in to enable auth without editing
          // the compose; the post-loop block below shows how to generate.
          lines.push('  Username:      admin     (matches /passwd/rpc.htpasswd; see "Generate htpasswd" below)');
          lines.push('  Password:      whatever you put in rpc.htpasswd');
        } else if (kind === 'sabnzbd') {
          lines.push('  API Key:       (paste from SAB → Config → General → API Key)');
        }
        lines.push(`  Category:      ${c.category}`);
        lines.push(`  Download path: ${c.download_path}    # what Ryokan sees inside its container`);
        // First client of each protocol becomes the default for that
        // protocol. Walk the list in order; first qbit/deluge/trans/
        // rtorrent → torrent default; first sabnzbd → usenet default.
        const isFirstOfProtocol =
          cfg.dlclients
            .slice(0, i + 1)
            .filter((k) => CLIENTS[k].protocol === c.protocol).length === 1;
        lines.push(
          `  Default for ${c.protocol}: ${isFirstOfProtocol ? 'YES' : 'no'}`
        );
        lines.push('');
      });

      // Fallback for users whose Docker DNS doesn't resolve cross-
      // service names (separate compose files, services on different
      // hosts, custom network plugins, etc.). Host LAN IP plus the
      // host-mapped port works wherever the service-name URL doesn't.
      lines.push('If "Test connection" fails because the service-name URL above');
      lines.push('does not resolve (separate compose files, different hosts, custom');
      lines.push('Docker network plugins, etc.), use your host\'s LAN IP and the');
      lines.push('host-mapped port instead. For example: http://192.168.1.100:8080');
      lines.push('in place of http://qbittorrent:8080.');
      lines.push('');
    }

    if (cfg.dlclients.includes('rtorrent')) {
      lines.push('--- Generate htpasswd for rTorrent (before first compose up) ---');
      lines.push('');
      lines.push('The crazy-max rtorrent-rutorrent image enforces basic auth on the');
      lines.push('web UI and the XML-RPC endpoint when /passwd/rutorrent.htpasswd');
      lines.push('and /passwd/rpc.htpasswd exist. Without the files, both are open.');
      lines.push('Generate both with the same credentials in one shot:');
      lines.push('');
      lines.push(`    sudo mkdir -p ${cfg.paths.appdata}/rutorrent/passwd`);
      lines.push('    docker run --rm httpd:2.4-alpine htpasswd -Bbn admin "REPLACE-WITH-YOUR-PASSWORD" \\');
      lines.push(`      | sudo tee ${cfg.paths.appdata}/rutorrent/passwd/rutorrent.htpasswd > /dev/null`);
      lines.push(`    sudo cp ${cfg.paths.appdata}/rutorrent/passwd/rutorrent.htpasswd \\`);
      lines.push(`        ${cfg.paths.appdata}/rutorrent/passwd/rpc.htpasswd`);
      lines.push(`    sudo chown -R ${cfg.puid}:${cfg.pgid} ${cfg.paths.appdata}/rutorrent/passwd`);
      lines.push('');
      lines.push('Same credentials cover ruTorrent web UI and Ryokan XML-RPC.');
      lines.push('');
    }

    if (cfg.media_server === 'jellyfin') {
      // The API key is generated in Jellyfin and consumed by Ryokan —
      // spelling out both ends so users don't get stuck looking for a
      // key Ryokan would create itself.
      lines.push('--- In Ryokan: Settings → Connections → Jellyfin ---');
      lines.push('');
      lines.push('  URL:     http://jellyfin:8096');
      lines.push('  API Key: First, in Jellyfin: Dashboard → API Keys → "+" → name it "Ryokan".');
      lines.push('           Copy the generated key, then paste it here in Ryokan and Save.');
      lines.push('');
    }

    lines.push('--- Settings → General ---');
    lines.push('');
    lines.push('Media Root Path:  /media/anime');
    lines.push('File operation:   hardlink   (default; works because downloads and media share a filesystem)');
    lines.push('');

    if (cfg.requests === 'seerr') {
      // Both shims live on the same Ryokan host:port — Sonarr at the
      // root, Radarr at the /radarr URL base. Seerr only allows two
      // Sonarr + two Radarr indexer slots; you need both for series
      // (Sonarr-shim) and films (Radarr-shim) requests to route to
      // Ryokan. Each shim has its own API key in Ryokan's settings.
      lines.push('--- Inside Seerr (after first-run setup at http://localhost:5055) ---');
      lines.push('');
      lines.push('Add Sonarr server (anibridge shim, for series):');
      lines.push('  Hostname:        ryokan');
      lines.push('  Port:            8978');
      lines.push('  API Key:         (Ryokan → Settings → Connections → Sonarr API → API Key)');
      lines.push('  Use SSL:         no');
      lines.push('  Quality Profile: HD-1080p');
      lines.push('  Root Folder:     /media/anime');
      lines.push('');
      lines.push('Add Radarr server (anibridge shim, for anime films; note the /radarr URL base):');
      lines.push('  Hostname:        ryokan');
      lines.push('  Port:            8978');
      lines.push('  URL Base:        /radarr');
      lines.push('  API Key:         (Ryokan → Settings → Connections → Radarr API → API Key)');
      lines.push('  Use SSL:         no');
      lines.push('  Quality Profile: HD-1080p');
      lines.push('  Root Folder:     /media/anime');
      lines.push('');
    }

    if (cfg.vpn === 'gluetun') {
      lines.push('--- Gluetun reminders ---');
      lines.push('');
      lines.push('Edit the gluetun service env in the compose to point at your VPN provider.');
      lines.push('Common providers: mullvad, protonvpn, pia, nordvpn, custom (paste-your-own-config).');
      lines.push('Wireguard is faster than OpenVPN if your provider supports it.');
      lines.push('');
      const torrents = cfg.dlclients.filter(
        (k) => CLIENTS[k].protocol === 'torrent'
      );
      if (torrents.length > 0) {
        lines.push(
          `${torrents.map((k) => CLIENTS[k].label).join(', ')} share gluetun's network namespace.`
        );
        lines.push("Their host ports are exposed on the gluetun container, not on themselves.");
      }
      if (cfg.dlclients.includes('sabnzbd')) {
        lines.push('SAB stays outside the VPN (Usenet talks TLS to your provider, not to peers).');
      }
      lines.push('');

      // Port forwarding only matters when there's a torrent client.
      // SAB-only stacks don't need inbound ports.
      if (torrents.length > 0) {
        lines.push('--- Port forwarding (open the inbound torrent port through gluetun) ---');
        lines.push('');
        lines.push('Why: without this, peers can\'t dial in to your torrent client. You\'ll still');
        lines.push('download (outbound connections work) but uploads stall and private trackers');
        lines.push('mark you "unconnectable", killing ratio.');
        lines.push('');
        lines.push('1. In the gluetun env block above, confirm VPN_PORT_FORWARDING="on" and that');
        lines.push('   your provider supports it (ProtonVPN, PIA, PrivateVPN, AirVPN; NOT Mullvad).');
        lines.push('');
        lines.push('2. Bring the stack up. Once gluetun connects, find the assigned port:');
        lines.push('');
        lines.push('     docker exec gluetun cat /tmp/gluetun/forwarded_port');
        lines.push('');
        lines.push('   Or scan the logs:');
        lines.push('');
        lines.push('     docker logs gluetun 2>&1 | grep -i "port forward"');
        lines.push('');
        lines.push('3. Paste that port number into your torrent client AND bind it to the tun0');
        lines.push('   interface (the VPN tunnel device). The interface bind is a belt-and-');
        lines.push('   suspenders kill switch: even though gluetun\'s built-in firewall blocks');
        lines.push('   non-VPN traffic at the namespace level, binding the client to tun0 means');
        lines.push('   if the tunnel drops, the client refuses to send packets at all rather');
        lines.push('   than potentially leaking through gluetun\'s upstream interface.');
        lines.push('');
        const qbit = torrents.includes('qbittorrent');
        const deluge = torrents.includes('deluge');
        const trans = torrents.includes('transmission');
        const rt = torrents.includes('rtorrent');
        if (qbit) {
          lines.push('   qBittorrent:');
          lines.push('     Tools → Options → Advanced → "Network Interface" = tun0');
          lines.push('     Tools → Options → Connection →');
          lines.push('       "Port used for incoming connections" = <forwarded port>');
          lines.push('       Uncheck "Use UPnP / NAT-PMP port forwarding"');
          lines.push('     The green/red icon next to "Connection status" in the bottom bar');
          lines.push('     turns green once peers can reach you. If it stays red after a');
          lines.push('     restart, the tun0 bind is wrong; double-check the interface name');
          lines.push('     with `docker exec gluetun ip a` (look for the tunnel device).');
        }
        if (deluge) {
          lines.push('   Deluge:');
          lines.push('     Preferences → Network → Interface = tun0');
          lines.push('     Preferences → Network → Incoming Port: pin to <forwarded port>');
          lines.push('     Disable UPnP / NAT-PMP under the same panel.');
        }
        if (trans) {
          lines.push('   Transmission:');
          lines.push('     Edit → Preferences → Network → "Listening port" = <forwarded port>');
          lines.push('     Uncheck "Use UPnP or NAT-PMP port forwarding".');
          lines.push('     Transmission has no GUI option for binding to a specific interface;');
          lines.push('     gluetun\'s firewall already provides the kill switch at the namespace');
          lines.push('     level, so this is fine for most users. If you want a hard bind, edit');
          lines.push('     settings.json: "bind-address-ipv4": "<tun0 ip>" (find with');
          lines.push('     `docker exec gluetun ip -4 -o addr show dev tun0`).');
        }
        if (rt) {
          lines.push('   rTorrent: edit your .rtorrent.rc:');
          lines.push('     network.port_range.set = <forwarded port>-<forwarded port>');
          lines.push('     network.port_random.set = no');
          lines.push('     network.bind_address.set = <tun0 ip>');
          lines.push('     (find tun0 ip with `docker exec gluetun ip -4 -o addr show dev tun0`.)');
        }
        lines.push('');
        lines.push('4. Caveats:');
        lines.push('   - The forwarded port can rotate (especially PIA, AirVPN). For stability,');
        lines.push('     run a sidecar script that polls /tmp/gluetun/forwarded_port and updates');
        lines.push('     the client via its API on change. "gluetun qbittorrent port forward"');
        lines.push('     turns up several ready-made ones.');
        lines.push('   - Empty status file = no port assigned yet. Check `docker logs gluetun`');
        lines.push('     for "port forwarded" entries; handshake errors usually mean wrong');
        lines.push('     credentials or a server that doesn\'t support PF.');
        lines.push('');
      }
    }

    if (cfg.proxy !== 'none') {
      lines.push('--- Reverse-proxy reminders ---');
      lines.push('');
      if (cfg.proxy === 'cloudflared') {
        lines.push('Cloudflare Tunnel: paste your tunnel token into the cloudflared service env above.');
        lines.push('Then in Cloudflare Zero Trust → Networks → Tunnels, configure a public hostname');
        lines.push("pointing at http://ryokan:8978. Set RYOKAN_TRUSTED_PROXY=1 in Ryokan's env so it");
        lines.push("trusts the X-Forwarded-* headers Cloudflare adds.");
      } else {
        lines.push(`Drop your real domain into the ${cfg.proxy} config (see comments in the compose).`);
        lines.push("Set RYOKAN_TRUSTED_PROXY=1 in Ryokan's env once HTTPS is working; the login cookie");
        lines.push("turns Secure on its own from the proxy's X-Forwarded-Proto header.");
      }
    }

    return lines.join('\n');
  }

  // The output `<pre>` blocks use `data-picker="..."` selectors rather
  // than ids on purpose. The `content.code.copy` feature rewrites any
  // `<pre id="x">` to `<pre id="__code_x">` so its own copy-button
  // wiring can find it — which makes `getElementById('compose-output')`
  // return null at runtime. Data attributes survive that rewrite.
  function rerender() {
    // Wrapped in try/catch for the same reason `init()` is — the
    // picker IS the page; a silent throw from inside renderCompose
    // / renderSettings (malformed input value, future renderer
    // edit, etc.) would freeze the output at its last good state
    // with no diagnostic.
    try {
      const cfg = readForm();
      if (!cfg) return;
      const composeEl = document.querySelector('[data-picker="compose"] code');
      const settingsEl = document.querySelector('[data-picker="settings"] code');
      if (composeEl) composeEl.textContent = renderCompose(cfg);
      if (settingsEl) settingsEl.textContent = renderSettings(cfg);
    } catch (err) {
      const composeEl = document.querySelector('[data-picker="compose"] code');
      if (composeEl) {
        composeEl.textContent =
          '# picker.js render error: ' +
          (err && err.message ? err.message : String(err)) +
          '\n# Open DevTools console for the full stack.';
      }
      // eslint-disable-next-line no-console
      console.error('[picker.js rerender]', err);
    }
  }

  function copyCompose() {
    const composeEl = document.querySelector('[data-picker="compose"] code');
    if (!composeEl) return;
    const text = composeEl.textContent;
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard
        .writeText(text)
        .then(() => flashCopyButton('Copied!'))
        .catch(() => flashCopyButton('Copy failed'));
    } else {
      flashCopyButton('Clipboard unavailable');
    }
  }

  function flashCopyButton(label) {
    const btn = document.getElementById('copy-compose');
    if (!btn) return;
    const original = btn.textContent;
    btn.textContent = label;
    setTimeout(() => {
      btn.textContent = original;
    }, 1500);
  }

  // Idempotent init. The site's `document$` observable can fire more
  // than once (instant navigation, theme toggle, etc.) — guarding via
  // a dataset flag so we don't stack duplicate listeners on the form.
  function init() {
    try {
      const form = document.getElementById('stack-form');
      if (!form) return;
      if (form.dataset.pickerInit !== '1') {
        form.dataset.pickerInit = '1';
        form.addEventListener('input', rerender);
        form.addEventListener('change', rerender);
      }
      const copyBtn = document.getElementById('copy-compose');
      if (copyBtn && copyBtn.dataset.pickerInit !== '1') {
        copyBtn.dataset.pickerInit = '1';
        copyBtn.addEventListener('click', copyCompose);
      }
      rerender();
    } catch (err) {
      // Make failures visible without DevTools — the picker is the
      // whole point of the page, a silent "Loading…" is worse than
      // a stack trace in the output box.
      const out = document.querySelector('[data-picker="compose"] code');
      if (out) {
        out.textContent =
          '# picker.js init error: ' +
          (err && err.message ? err.message : String(err)) +
          '\n# Open DevTools console for the full stack.';
      }
      // Still surface to console for debugging.
      // eslint-disable-next-line no-console
      console.error('[picker.js]', err);
    }
  }

  // The site theme exposes a `document$` observable (an RxJS
  // document-state subject) that fires once on initial load and
  // again on instant-navigation transitions. Subscribing to it is
  // the canonical integration pattern; fall back to DOMContentLoaded
  // when the theme's runtime isn't present.
  if (typeof window !== 'undefined' && window.document$ &&
      typeof window.document$.subscribe === 'function') {
    window.document$.subscribe(init);
  } else if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
