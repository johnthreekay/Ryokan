// Stack picker. Renders a docker-compose.yml from the form in
// `docs/quick-start.md` and fits the page's steps to the choices:
// `data-when` blocks, `data-v` values and the paths in its text. Pure
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
  // service blocks. The URL Ryokan
  // dials is built from the service name, `port` and `url_path`.
  const CLIENTS = {
    qbittorrent: {
      label: 'qBittorrent',
      image: 'lscr.io/linuxserver/qbittorrent:latest',
      port: 8080,
      extra_ports: [],
      // Peer port from the shared 6881+ range (see peerPorts); the
      // linuxserver image passes TORRENTING_PORT to qbittorrent-nox.
      peer_port: { udp_only: false, env: 'TORRENTING_PORT' },
      category: 'anime',
      config_dir: 'qbittorrent',
      env: { WEBUI_PORT: '8080' },
      protocol: 'torrent',
    },
    deluge: {
      label: 'Deluge',
      image: 'lscr.io/linuxserver/deluge:latest',
      port: 8112,
      extra_ports: [],
      // No env for it: Deluge starts on a random port until the
      // Quick start's Incoming Port step pins this one.
      peer_port: { udp_only: false, env: null },
      category: 'anime',
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
      extra_ports: ['8082:8080', '50000:50000'],
      // Behind gluetun the client shares gluetun's network namespace,
      // where gluetun's control server already listens on 8000 and
      // qBittorrent's web UI on 8080, and the image also takes each
      // port plus one for its health checks. The image's XMLRPC_PORT
      // and RUTORRENT_PORT move both clear of them there.
      vpn: {
        port: 8010,
        extra_ports: ['8082:8090', '50000:50000'],
        env: { XMLRPC_PORT: '8010', RUTORRENT_PORT: '8090' },
      },
      // DHT (udp) from the shared 6881+ range; the image reads
      // RT_DHT_PORT.
      peer_port: { udp_only: true, env: 'RT_DHT_PORT' },
      category: 'anime',
      // /RPC2 path is required; rTorrent's XML-RPC endpoint.
      url_path: '/RPC2',
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
      code: stackCode(form),
      link: stackLink(form),
      dlclients,
      media_server: radio('media_server'),
      requests: radio('requests'),
      vpn: radio('vpn'),
      proxy: radio('proxy'),
      indexer: radio('indexer'),
      // Not part of the file: Ryokan's own setting, shown in the steps.
      file_mode: radio('file_mode'),
      host_check: get('host_check').checked,
      // Host-name characters only, space-separated: the value lands in
      // a double-quoted YAML string.
      allowed_hosts: get('allowed_hosts')
        .value.replace(/[^A-Za-z0-9.*,\- ]/g, ' ')
        .split(/[\s,]+/)
        .filter(Boolean)
        .join(' '),
      puid: get('puid').value || '1000',
      pgid: get('pgid').value || '1000',
      tz: get('tz').value || 'UTC',
      paths: sharedPaths(
        get('shared_path').value,
        get('appdata_path').value || '/srv/docker'
      ),
    };
  }

  // One host folder holds the downloads and the library, and every
  // container mounts it at the same path it has on the host. A
  // hardlink needs both ends inside one mount, and with identical
  // paths the clients report locations Ryokan can open as they are
  // (no per-client path rewrite), and the Media Root the user types
  // is the host path they already know.
  function sharedPaths(raw, appdata) {
    const typed = (raw || '').trim();
    // An absolute path is cleaned (`//`, `.`, `..`, a trailing `/`) so
    // sharedPathProblem judges the folder Docker will mount; anything
    // else stays as typed for the error to quote.
    let shared = '/srv/media';
    if (typed) shared = typed.startsWith('/') ? cleanPath(typed) : typed;
    return {
      shared,
      downloads: `${shared}/downloads`,
      media: `${shared}/anime`,
      appdata,
    };
  }

  function cleanPath(path) {
    const parts = [];
    path.split('/').forEach((part) => {
      if (part === '..') parts.pop();
      else if (part && part !== '.') parts.push(part);
    });
    return `/${parts.join('/')}`;
  }

  function isSameOrInside(path, dir) {
    return path === dir || path.startsWith(dir === '/' ? '/' : `${dir}/`);
  }

  // Container folders a mount at the same path would hide.
  const SYSTEM_DIRS = ['/', '/bin', '/boot', '/dev', '/etc', '/lib', '/lib64',
    '/proc', '/root', '/run', '/sbin', '/sys', '/tmp', '/usr', '/var'];

  // Container folders the shared one may not be or sit inside, in the
  // services that mount it: their own config mounts, and /app, where
  // the Ryokan image keeps its program.
  function reservedTargets(cfg) {
    const out = [
      { path: '/config', why: 'Ryokan and the other apps keep their settings at /config inside their containers' },
      { path: '/app', why: 'the Ryokan image keeps its program in /app' },
    ];
    cfg.dlclients.forEach((kind) => {
      const c = CLIENTS[kind];
      const target = c.config_mount_target;
      if (target) out.push({ path: target, why: `${c.label} keeps its settings at ${target} inside its container` });
    });
    if (cfg.dlclients.includes('rtorrent')) {
      out.push({ path: '/passwd', why: 'rTorrent reads its passwords from /passwd inside its container' });
    }
    return out;
  }

  // Why the shared folder can't work as typed, or null. Every container
  // that mounts it does so at its host path, so it has to be a valid
  // container path as well (absolute, no `:`); a second mount at a
  // service's own config path (`/data` beside rTorrent's /data) is
  // refused by `docker run` as a duplicate mount point and silently
  // replaces the config mount under Compose (the later line wins); and
  // a library inside a config folder is re-owned along with it
  // (Ryokan's entrypoint takes ownership of everything in its data dir).
  function sharedPathProblem(cfg) {
    const shared = cfg.paths.shared;
    const pick = 'Pick a folder such as /srv/media.';
    if (shared.includes(':')) {
      return "The shared media folder can't contain a colon. Docker reads a colon in a volume line as a separator.";
    }
    if (!shared.startsWith('/')) {
      return 'The shared media folder has to be a full path that starts with /, such as /srv/media. Every container mounts it at that same path.';
    }
    if (SYSTEM_DIRS.includes(shared)) {
      return `The shared media folder can't be ${shared}. Every container mounts it at that same path, where it would hide the container's own ${shared}. ${pick}`;
    }
    const reserved = reservedTargets(cfg).find((r) => isSameOrInside(shared, r.path));
    if (reserved) {
      return `The shared media folder can't be ${reserved.path} or a folder inside it, because ${reserved.why}. ${pick}`;
    }
    const appdata = (cfg.paths.appdata || '').trim();
    if (appdata.startsWith('/')) {
      const root = cleanPath(appdata);
      const dir = serviceDirs(cfg)
        .map((d) => (root === '/' ? `/${d}` : `${root}/${d}`))
        .find((d) => isSameOrInside(shared, d));
      if (dir) {
        return `The shared media folder can't be ${dir} or a folder inside it. That folder holds an app's settings, and apps such as Ryokan take ownership of everything in their settings folder when they start. ${pick}`;
      }
    }
    return null;
  }

  // Host ports from 6881 up for the torrent clients that take one
  // (peer_port), in the order they were picked, so two clients never
  // publish the same port: that made `docker compose up` fail with
  // "port is already allocated".
  function peerPorts(cfg) {
    const ports = {};
    let next = 6881;
    cfg.dlclients.forEach((kind) => {
      if (CLIENTS[kind].peer_port) ports[kind] = next++;
    });
    return ports;
  }

  // A client's entry with its behind-the-VPN overrides (`vpn`)
  // applied when it sits behind gluetun.
  function clientSpec(kind, cfg) {
    const c = CLIENTS[kind];
    if (!c.vpn || !isBehindVpn(kind, cfg)) return c;
    return Object.assign({}, c, c.vpn, {
      env: Object.assign({}, c.env, c.vpn.env),
    });
  }

  // Every host port mapping a client publishes (on itself, or on
  // gluetun when it sits behind the VPN).
  function portMappings(kind, cfg) {
    const c = clientSpec(kind, cfg);
    const out = [];
    if (c.expose_main_port !== false) out.push(`${c.host_port || c.port}:${c.port}`);
    out.push(...c.extra_ports);
    const peer = peerPorts(cfg)[kind];
    if (peer) {
      if (!c.peer_port.udp_only) out.push(`${peer}:${peer}`);
      out.push(`${peer}:${peer}/udp`);
    }
    return out;
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
  // `gluetun` does — so the host is `gluetun`. Port works out:
  // containers in a shared namespace see each other's listeners on
  // localhost, so qBit's 8080 inside that namespace is reachable as
  // gluetun:8080 from peer containers on `media`. Standalone (no VPN)
  // clients keep their own container name: service_name (compose-side)
  // rather than kind (form-side), which differ only for rtorrent
  // (kind=rtorrent, service=rutorrent).
  function urlForClient(kind, cfg) {
    const c = clientSpec(kind, cfg);
    const host = isBehindVpn(kind, cfg) ? 'gluetun' : c.service_name || kind;
    return `http://${host}:${c.port}${c.url_path || ''}`;
  }

  function renderClient(kind, cfg) {
    const c = clientSpec(kind, cfg);
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
    const portsList = behindVpn
      ? null
      : portMappings(kind, cfg).map((p) => `"${p}"`);

    const baseEnv = [
      `      PUID: "${cfg.puid}"`,
      `      PGID: "${cfg.pgid}"`,
      `      TZ: "${cfg.tz}"`,
    ];
    const envVars = Object.assign({}, c.env);
    const peer = peerPorts(cfg)[kind];
    if (peer && c.peer_port.env) envVars[c.peer_port.env] = String(peer);
    const extraEnv = Object.entries(envVars).map(
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
    lines.push(`      - ${cfg.paths.shared}:${cfg.paths.shared}`);
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
    // Service names, not form values: rTorrent's service is rutorrent,
    // and naming `rtorrent` here made the whole compose invalid.
    const dependsList = deps.length
      ? `    depends_on:\n${deps
          .map((k) => `      - ${CLIENTS[k].service_name || k}`)
          .join('\n')}\n`
      : '';
    // Opt-in DNS-rebinding defense (docs/docker.md#host-check). A proxy
    // passes the public domain through as the Host, so it has to be
    // listed.
    let hardening = '';
    if (cfg.host_check) {
      hardening += '\n      RYOKAN_HOST_CHECK: "1"';
      if (cfg.allowed_hosts) {
        hardening += `\n      RYOKAN_ALLOWED_HOSTS: "${cfg.allowed_hosts}"`;
      } else if (cfg.proxy !== 'none') {
        hardening += '\n      # Add the domain your proxy serves Ryokan on:\n      # RYOKAN_ALLOWED_HOSTS: "ryokan.example.com"';
      } else {
        // The container's hostname is a random ID, not the server's, so
        // http://nas:8978 is refused until "nas" is listed.
        hardening += "\n      # Add your server's name if you open Ryokan by it (IP addresses\n      # and localhost always work):\n      # RYOKAN_ALLOWED_HOSTS: \"nas\"";
      }
    }
    return `  ryokan:
    image: ghcr.io/johnthreekay/ryokan:latest
    container_name: ryokan
    networks: [media]
    ports:
      - "8978:8978"
    volumes:
      - ${cfg.paths.appdata}/ryokan:/config
      - ${cfg.paths.shared}:${cfg.paths.shared}
    environment:
      PUID: "${cfg.puid}"
      PGID: "${cfg.pgid}"
      TZ: "${cfg.tz}"
      # Ryokan's own files live at /config, which leaves /data free
      # for a shared media folder there.
      RYOKAN_DATA_DIR: /config
      RUST_LOG: ryokan=info${hardening}
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
      - ${cfg.paths.media}:${cfg.paths.media}:ro
    environment:
      PUID: "${cfg.puid}"
      PGID: "${cfg.pgid}"
      TZ: "${cfg.tz}"
    restart: unless-stopped`;
  }

  function renderEmby(cfg) {
    return `  emby:
    image: lscr.io/linuxserver/emby:latest
    container_name: emby
    networks: [media]
    ports:
      - "8096:8096"
    devices:
      # /dev/dri enables Intel/AMD hardware transcode. Remove this
      # line if your host has no iGPU or you don't need transcoding.
      - /dev/dri:/dev/dri
    volumes:
      - ${cfg.paths.appdata}/emby:/config
      - ${cfg.paths.media}:${cfg.paths.media}:ro
    environment:
      PUID: "${cfg.puid}"
      PGID: "${cfg.pgid}"
      TZ: "${cfg.tz}"
    restart: unless-stopped`;
  }

  function renderPlex(cfg) {
    return `  plex:
    image: lscr.io/linuxserver/plex:latest
    container_name: plex
    networks: [media]
    ports:
      - "32400:32400"
    devices:
      # /dev/dri enables Intel/AMD hardware transcode (Plex Pass).
      # Remove this line if your host has no iGPU or you don't need it.
      - /dev/dri:/dev/dri
    volumes:
      - ${cfg.paths.appdata}/plex:/config
      - ${cfg.paths.media}:${cfg.paths.media}:ro
    environment:
      PUID: "${cfg.puid}"
      PGID: "${cfg.pgid}"
      TZ: "${cfg.tz}"
      VERSION: docker
      # Links the new server to your Plex account on first start. Get a
      # token at https://www.plex.tv/claim (valid for four minutes) and
      # paste it here. You can remove the line once the server is set up.
      PLEX_CLAIM: ""
    restart: unless-stopped`;
  }

  // Prowlarr or Jackett: where the user adds trackers and Usenet
  // indexers, which Ryokan then searches over torznab / newznab.
  const INDEXERS = {
    prowlarr: { label: 'Prowlarr', image: 'lscr.io/linuxserver/prowlarr:latest', port: 9696 },
    jackett: { label: 'Jackett', image: 'lscr.io/linuxserver/jackett:latest', port: 9117 },
  };

  function renderIndexer(cfg) {
    const ix = INDEXERS[cfg.indexer];
    if (!ix) return null;
    return `  ${cfg.indexer}:
    image: ${ix.image}
    container_name: ${cfg.indexer}
    networks: [media]
    ports:
      - "${ix.port}:${ix.port}"
    volumes:
      - ${cfg.paths.appdata}/${cfg.indexer}:/config
    environment:
      PUID: "${cfg.puid}"
      PGID: "${cfg.pgid}"
      TZ: "${cfg.tz}"
    restart: unless-stopped`;
  }

  // The media server picked, as a compose service name, or null.
  function mediaServerService(cfg) {
    return ['jellyfin', 'emby', 'plex'].includes(cfg.media_server)
      ? cfg.media_server
      : null;
  }

  function renderSeerr(cfg) {
    const deps = ['ryokan'];
    if (mediaServerService(cfg)) deps.push(mediaServerService(cfg));
    return `  seerr:
    image: ghcr.io/seerr-team/seerr:latest
    container_name: seerr
    init: true
    networks: [media]
    ports:
      - "5055:5055"
    volumes:
      - ${cfg.paths.appdata}/seerr:/app/config
    # Seerr has no PUID / PGID setting. This runs it as the same user.
    user: "${cfg.puid}:${cfg.pgid}"
    environment:
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
      .flatMap((k) => portMappings(k, cfg).map((p) => `      - "${p}"`));
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
      # The Quick start's "VPN port forwarding" section has the steps
      # for each torrent client.
      VPN_PORT_FORWARDING_STATUS_FILE: "/tmp/gluetun/forwarded_port"
    restart: unless-stopped
    # All torrent download clients in this stack share gluetun's
    # network namespace via network_mode: "service:gluetun" and
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
      - ${cfg.paths.appdata}/caddy/data:/data
      - ${cfg.paths.appdata}/caddy/config:/config
    # Put your own hostname in place of ryokan.example.com. Caddy gets
    # a Let's Encrypt certificate once that name points at this host.
    command: caddy reverse-proxy --from ryokan.example.com --to ryokan:8978
    restart: unless-stopped`;
    }
    if (cfg.proxy === 'traefik') {
      return `  traefik:
    image: traefik:v3
    container_name: traefik
    networks: [media]
    ports:
      - "80:80"
      - "443:443"
      # Dashboard on host port 8090: qBittorrent's web UI has 8080.
      # Remove for prod.
      - "8090:8080"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      # Traefik keeps its certificates here.
      - ${cfg.paths.appdata}/traefik:/letsencrypt
    command:
      - --api.insecure=true
      - --providers.docker=true
      - --providers.docker.exposedbydefault=false
      - --entrypoints.web.address=:80
      - --entrypoints.websecure.address=:443
      - --certificatesresolvers.le.acme.email=you@example.com
      - --certificatesresolvers.le.acme.storage=/letsencrypt/acme.json
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

  // Per-service appdata subdirectories, one per service the stack runs.
  function serviceDirs(cfg) {
    const dirs = ['ryokan'];
    cfg.dlclients.forEach((k) => dirs.push(CLIENTS[k].config_dir));
    if (mediaServerService(cfg)) dirs.push(mediaServerService(cfg));
    if (INDEXERS[cfg.indexer]) dirs.push(cfg.indexer);
    if (cfg.requests === 'seerr') dirs.push('seerr');
    if (cfg.vpn === 'gluetun') dirs.push('gluetun');
    if (cfg.proxy === 'caddy') dirs.push('caddy/data', 'caddy/config');
    if (cfg.proxy === 'traefik') dirs.push('traefik');
    if (cfg.proxy === 'nginx') dirs.push('nginx/conf.d', 'nginx/certs');
    return dirs;
  }

  function renderCompose(cfg) {
    if (cfg.dlclients.length === 0) {
      return '# Pick at least one download client.\n';
    }
    const problem = sharedPathProblem(cfg);
    if (problem) return `# ${problem.replace(/\. /g, '.\n# ')}\n`;

    const services = [];
    services.push(renderRyokan(cfg));
    cfg.dlclients.forEach((kind) => services.push(renderClient(kind, cfg)));
    if (cfg.vpn === 'gluetun') services.push(renderGluetun(cfg));
    if (cfg.media_server === 'jellyfin') services.push(renderJellyfin(cfg));
    if (cfg.media_server === 'emby') services.push(renderEmby(cfg));
    if (cfg.media_server === 'plex') services.push(renderPlex(cfg));
    const indexer = renderIndexer(cfg);
    if (indexer) services.push(indexer);
    if (cfg.requests === 'seerr') services.push(renderSeerr(cfg));
    const proxy = renderProxy(cfg);
    if (proxy) services.push(proxy);

    // Enumerate per-service appdata subdirectories so the mkdir
    // pre-creates each one with the right ownership. Without this,
    // Docker creates lazy bind-mount targets as root on first up
    // and a Jellyfin / Seerr / non-linuxserver container can fail
    // to write to its own config volume. The chown takes these and
    // nothing else: the config root itself often holds other stacks'
    // folders, and a recursive chown of it re-owned all of them.
    const appdataPaths = serviceDirs(cfg)
      .map((d) => `${cfg.paths.appdata}/${d}`)
      .join(' ');

    // The code holds every choice in the form, so a file someone pastes
    // when asking for help can be opened in the picker as they built it.
    const origin = cfg.code
      ? `# Ryokan stack\n# Open this exact stack in the picker:\n#   ${cfg.link}`
      : '# Ryokan stack';
    const header = `# =============================================================================
${origin}
# =============================================================================
#
# Before the first start, create the folders:
#   sudo mkdir -p ${cfg.paths.downloads} ${cfg.paths.media} ${appdataPaths}
#   sudo chown -R ${cfg.puid}:${cfg.pgid} ${cfg.paths.downloads} ${cfg.paths.media} ${appdataPaths}
#
# Then start the stack from the folder that holds this file:
#   docker compose up -d
#
# Path layout: ${cfg.paths.shared} holds downloads/ and anime/ (the
# library), and every container mounts it at that same path. A hardlink
# needs both ends inside one mount, so this is what lets post-processing
# hardlink instead of copy, and the clients report paths Ryokan can open
# as they are. Point each client's download folder at
# ${cfg.paths.downloads} (the Quick start says where).
#
# =============================================================================

networks:
  media:
    name: media

services:
`;
    return header + services.join('\n\n') + '\n';
  }

  // The two commands to run before the first start, as their own block
  // on the page (the compose file's header comment repeats them).
  function prepCommands(cfg) {
    if (cfg.dlclients.length === 0) return '# Pick at least one download client.';
    if (sharedPathProblem(cfg)) return '# Fix the media folder above first.';
    // One folder per line, so nothing runs off the side of the block.
    const dirs = [cfg.paths.downloads, cfg.paths.media]
      .concat(serviceDirs(cfg).map((d) => `${cfg.paths.appdata}/${d}`))
      .map((d) => ` \\\n  ${d}`)
      .join('');
    return `sudo mkdir -p${dirs}\nsudo chown -R ${cfg.puid}:${cfg.pgid}${dirs}`;
  }

  const SERVER_LABELS = { jellyfin: 'Jellyfin', emby: 'Emby', plex: 'Plex' };
  const PROXY_LABELS = {
    caddy: 'Caddy',
    traefik: 'Traefik',
    nginx: 'nginx',
    cloudflared: 'Cloudflare Tunnel',
  };

  // What the stack runs, in the order the steps set it up.
  function summaryLabels(cfg) {
    const out = ['Ryokan'];
    cfg.dlclients.forEach((k) => out.push(CLIENTS[k].label));
    if (cfg.vpn === 'gluetun') out.push('Gluetun');
    if (mediaServerService(cfg)) out.push(SERVER_LABELS[cfg.media_server]);
    if (INDEXERS[cfg.indexer]) out.push(INDEXERS[cfg.indexer].label);
    if (cfg.requests === 'seerr') out.push('Seerr');
    if (cfg.proxy !== 'none') out.push(PROXY_LABELS[cfg.proxy]);
    return out;
  }

  // The facts a `data-when` attribute is matched against. A block is
  // shown when any of its space-separated tokens holds; `!token` holds
  // when the fact is absent.
  function stackFacts(cfg) {
    const f = new Set();
    cfg.dlclients.forEach((k) => f.add(`client:${k}`));
    if (cfg.dlclients.some((k) => CLIENTS[k].protocol === 'torrent')) f.add('client:torrent');
    f.add(`server:${cfg.media_server}`);
    if (mediaServerService(cfg)) f.add('server:any');
    f.add(`requests:${cfg.requests}`);
    f.add(`indexer:${cfg.indexer}`);
    f.add(`mode:${cfg.file_mode}`);
    f.add(`vpn:${cfg.vpn}`);
    f.add(`proxy:${cfg.proxy}`);
    if (cfg.proxy !== 'none') f.add('proxy:any');
    if (cfg.host_check) f.add('hostcheck:on');
    return f;
  }

  function whenHolds(expr, facts) {
    return expr
      .split(/\s+/)
      .filter(Boolean)
      .some((t) => (t[0] === '!' ? !facts.has(t.slice(1)) : facts.has(t)));
  }

  // Single values a `data-v` element shows: the address Ryokan reaches
  // each client at, and each client's peer port.
  function stackValues(cfg) {
    const v = {
      'file-mode': { hardlink: 'Hardlink', copy: 'Copy', move: 'Move' }[cfg.file_mode],
    };
    Object.keys(CLIENTS).forEach((k) => {
      v[`url-${k}`] = urlForClient(k, cfg);
    });
    const peers = peerPorts(cfg);
    Object.keys(peers).forEach((k) => {
      v[`peer-${k}`] = String(peers[k]);
    });
    return v;
  }

  // The page's prose is written with the default folders and IDs. In the
  // article that holds the form those are swapped for what the form holds, so
  // every command and path on the page is the reader's own.
  const LIVE_DEFAULTS = sharedPaths('/srv/media', '/srv/docker');

  function livePairs(cfg) {
    return [
      [LIVE_DEFAULTS.downloads, cfg.paths.downloads],
      [LIVE_DEFAULTS.media, cfg.paths.media],
      [LIVE_DEFAULTS.shared, cfg.paths.shared],
      [LIVE_DEFAULTS.appdata, cfg.paths.appdata],
      ['1000:1000', `${cfg.puid}:${cfg.pgid}`],
    ];
  }

  // Replaces every `from` string with its `to` in one pass, longest
  // first, so `/srv/media/anime` is never read as `/srv/media` + rest
  // and a replacement is never replaced again.
  function swapAll(text, pairs, from, to) {
    const sorted = pairs
      .filter((p) => p[from])
      .sort((a, b) => b[from].length - a[from].length);
    const map = new Map();
    sorted.forEach((p) => {
      if (!map.has(p[from])) map.set(p[from], p[to]);
    });
    const re = new RegExp(
      sorted.map((p) => p[from].replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|'),
      'g'
    );
    return text.replace(re, (m) => map.get(m));
  }

  const liveShown = new WeakMap();
  let lastPairs = null;

  function applyLive(cfg) {
    const pairs = livePairs(cfg);
    const form = document.getElementById('stack-form');
    const article = form && form.closest('article');
    (article ? [article] : []).forEach((root) => {
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      const nodes = [];
      while (walker.nextNode()) nodes.push(walker.currentNode);
      nodes.forEach((node) => {
        const parent = node.parentElement;
        if (!parent || parent.closest('#stack-form, [data-picker], [data-v], script, style')) return;
        // Text the script has not written (the page as served, or
        // something typed since) is read back to the defaults first.
        const known = liveShown.get(node);
        let tpl = known ? known.tpl : node.nodeValue;
        if (known && known.shown !== node.nodeValue) {
          tpl = lastPairs ? swapAll(node.nodeValue, lastPairs, 1, 0) : node.nodeValue;
        }
        const shown = swapAll(tpl, pairs, 0, 1);
        if (shown !== node.nodeValue) node.nodeValue = shown;
        liveShown.set(node, { tpl, shown });
      });
    });
    lastPairs = pairs;
  }

  // Hides the "On this page" entry of a heading whose section is hidden.
  function syncToc() {
    document.querySelectorAll('article [id]').forEach((h) => {
      if (!/^H[2-6]$/.test(h.tagName)) return;
      const hidden = !!h.closest('[hidden]');
      document
        .querySelectorAll(`.md-nav--secondary a[href$="#${CSS.escape(h.id)}"]`)
        .forEach((a) => {
          const li = a.closest('li');
          if (li) li.hidden = hidden;
        });
    });
  }

  function setText(selector, text) {
    document.querySelectorAll(selector).forEach((el) => {
      if (el.textContent !== text) el.textContent = text;
    });
  }

  // The output blocks use `data-picker="..."` selectors rather than ids
  // on purpose: the `content.code.copy` feature rewrites a `<pre id>`.
  function rerender() {
    try {
      const cfg = readForm();
      if (!cfg) return;
      const problem =
        cfg.dlclients.length === 0
          ? 'Pick at least one download client.'
          : sharedPathProblem(cfg);

      setText('[data-picker="compose"] code', renderCompose(cfg));
      setText('[data-picker="prep"] code', prepCommands(cfg));

      const summary = document.querySelector('[data-picker="summary"]');
      if (summary) {
        summary.textContent = '';
        summaryLabels(cfg).forEach((label) => {
          const chip = document.createElement('span');
          chip.className = 'sf-chip';
          chip.textContent = label;
          summary.appendChild(chip);
        });
      }
      const problemEl = document.querySelector('[data-picker="problem"]');
      if (problemEl) {
        problemEl.textContent = problem || '';
        problemEl.hidden = !problem;
      }

      const facts = stackFacts(cfg);
      document.querySelectorAll('[data-when]').forEach((el) => {
        el.hidden = !whenHolds(el.dataset.when, facts);
      });
      const values = stackValues(cfg);
      document.querySelectorAll('[data-v]').forEach((el) => {
        const value = values[el.dataset.v];
        if (value !== undefined && el.textContent !== value) el.textContent = value;
      });
      // A folder that can't work stays out of the page's commands.
      if (!sharedPathProblem(cfg)) applyLive(cfg);
      syncToc();
      saveForm();
    } catch (err) {
      setText(
        '[data-picker="compose"] code',
        '# picker.js render error: ' +
          (err && err.message ? err.message : String(err)) +
          '\n# Open DevTools console for the full stack.'
      );
      // eslint-disable-next-line no-console
      console.error('[picker.js rerender]', err);
    }
  }

  // The stack code: the form's state, minus whatever is still at its
  // default, as base64url JSON behind a format version. `#stack=<code>`
  // on the page's address loads it back into the form.
  const CODE_VERSION = '1';

  function formState(form, changedOnly) {
    const state = {};
    const changed = new Set();
    form.querySelectorAll('input[name]').forEach((el) => {
      if (el.type === 'checkbox') {
        if (!state[el.name]) state[el.name] = [];
        if (el.checked) state[el.name].push(el.value);
        if (el.checked !== el.defaultChecked) changed.add(el.name);
      } else if (el.type === 'radio') {
        if (el.checked) state[el.name] = el.value;
        if (el.checked !== el.defaultChecked) changed.add(el.name);
      } else {
        state[el.name] = el.value;
        if (el.value !== el.defaultValue) changed.add(el.name);
      }
    });
    if (changedOnly) {
      Object.keys(state).forEach((name) => {
        if (!changed.has(name)) delete state[name];
      });
    }
    return state;
  }

  function stackCode(form) {
    const json = JSON.stringify(formState(form, true));
    const b64 = btoa(unescape(encodeURIComponent(json)))
      .replace(/\+/g, '-')
      .replace(/\//g, '_')
      .replace(/=+$/, '');
    return `${CODE_VERSION}-${b64}`;
  }

  function stackLink(form) {
    const canonical = document.querySelector('link[rel="canonical"]');
    const base = (canonical && canonical.href) || window.location.href.split('#')[0];
    return `${base}#stack=${stackCode(form)}`;
  }

  // The state a `#stack=` link carries, or null. Anything that does not
  // decode to an object is ignored, and applyState only ever ticks
  // inputs and sets field values, so a made-up code can do no more than
  // fill in the form.
  function stateFromHash() {
    const m = /^#stack=(\d+)-([A-Za-z0-9_-]*)$/.exec(window.location.hash || '');
    if (!m || m[1] !== CODE_VERSION) return null;
    try {
      const json = decodeURIComponent(
        escape(atob(m[2].replace(/-/g, '+').replace(/_/g, '/')))
      );
      const state = JSON.parse(json);
      return state && typeof state === 'object' && !Array.isArray(state) ? state : null;
    } catch (err) {
      return null;
    }
  }

  // The choices are remembered in this browser, so a reload keeps them.
  const STORE_KEY = 'ryokan-stack-v1';

  function saveForm() {
    const form = document.getElementById('stack-form');
    if (!form) return;
    const state = formState(form, false);
    try {
      localStorage.setItem(STORE_KEY, JSON.stringify(state));
    } catch (err) {
      // Private windows and blocked storage: the form still works.
    }
  }

  function restoreForm(form) {
    // A stack link wins over what this browser remembers, and starts
    // from the defaults, since the code leaves those out.
    const linked = stateFromHash();
    if (linked) {
      form.reset();
      applyState(form, linked);
      return;
    }
    let state = null;
    try {
      state = JSON.parse(localStorage.getItem(STORE_KEY) || 'null');
    } catch (err) {
      state = null;
    }
    if (!state || typeof state !== 'object') return;
    applyState(form, state);
  }

  function applyState(form, state) {
    form.querySelectorAll('input[name]').forEach((el) => {
      const saved = state[el.name];
      if (saved === undefined) return;
      if (el.type === 'checkbox') {
        el.checked = Array.isArray(saved) && saved.includes(el.value);
      } else if (el.type === 'radio') {
        el.checked = saved === el.value;
      } else if (typeof saved === 'string') {
        el.value = saved;
      }
    });
    // A saved value that no longer exists must not leave a radio group
    // with nothing picked.
    new Set(
      Array.from(form.querySelectorAll('input[type="radio"]')).map((el) => el.name)
    ).forEach((name) => {
      const group = form.querySelectorAll(`input[type="radio"][name="${name}"]`);
      if (!Array.from(group).some((el) => el.checked)) {
        (form.querySelector(`input[type="radio"][name="${name}"][checked]`) || group[0]).checked = true;
      }
    });
  }

  function copyBlock(btn) {
    const src = document.querySelector(`[data-picker="${btn.dataset.copy}"] code`);
    if (!src) return;
    const flash = (label) => {
      const original = btn.dataset.label || btn.textContent;
      btn.dataset.label = original;
      btn.textContent = label;
      setTimeout(() => {
        btn.textContent = original;
      }, 1500);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard
        .writeText(src.textContent)
        .then(() => flash('Copied!'))
        .catch(() => flash('Copy failed'));
    } else {
      flash('Clipboard unavailable');
    }
  }

  // Idempotent init. The site's `document$` observable can fire more
  // than once; the dataset flags keep listeners from stacking.
  function init() {
    try {
      const form = document.getElementById('stack-form');
      if (!form) return;
      if (form.dataset.pickerInit !== '1') {
        form.dataset.pickerInit = '1';
        restoreForm(form);
        form.addEventListener('input', rerender);
        form.addEventListener('change', rerender);
        // The form has nothing to submit; Enter in a field must not
        // reload the page.
        form.addEventListener('submit', (e) => e.preventDefault());
        // A stack link opened while the page is already showing.
        window.addEventListener('hashchange', () => {
          if (stateFromHash()) {
            restoreForm(form);
            rerender();
          }
        });
      }
      document.querySelectorAll('button[data-copy]').forEach((btn) => {
        if (btn.dataset.pickerInit === '1') return;
        btn.dataset.pickerInit = '1';
        btn.addEventListener('click', () => copyBlock(btn));
      });
      rerender();
    } catch (err) {
      setText(
        '[data-picker="compose"] code',
        '# picker.js init error: ' +
          (err && err.message ? err.message : String(err)) +
          '\n# Open DevTools console for the full stack.'
      );
      // eslint-disable-next-line no-console
      console.error('[picker.js]', err);
    }
  }

  if (typeof window !== 'undefined' && window.document$ &&
      typeof window.document$.subscribe === 'function') {
    window.document$.subscribe(init);
  } else if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
