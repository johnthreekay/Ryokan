# Docker reference

Reference for environment variables, healthcheck behavior, volume layout, and update semantics. For step-by-step install, see [Installation](install.md).

## Environment variables

Most users only need `PUID`, `PGID`, and `TZ`. The rest are for fine-tuning.

| Variable | Default | Purpose |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:8978` | TCP bind. Change the port if 8978 conflicts with something else. |
| `PUID` / `PGID` | `1000` / `1000` | Runtime UID/GID. Match your host's media-owning user. See [Installation → PUID and PGID](install.md#puid-and-pgid). |
| `TZ` | unset (UTC) | Container timezone. Determines what timestamps look like in the UI and logs. Standard tzdata names like `America/Chicago` or `Europe/London`. |
| `RUST_LOG` | `ryokan=info` (image) | Console log filter. Set to `ryokan=debug` for verbose output while debugging. |
| `RYOKAN_TRUSTED_PROXY` | unset (off) | Trust `X-Forwarded-For` and `X-Real-IP` for client IP. Off by default. Flip on only behind a reverse proxy that overwrites these headers on ingress; otherwise an attacker can spoof a fresh IP per attempt and bypass the per-IP login throttle. |
| `RYOKAN_COOKIE_SECURE` | unset (off) | Force the `Secure` flag onto the login cookie. Usually unnecessary: with `RYOKAN_TRUSTED_PROXY=1`, Ryokan sets the flag on its own whenever the proxy reports HTTPS. Set this only for an HTTPS proxy that doesn't send `X-Forwarded-Proto`. Leave it off for plain HTTP or you won't be able to stay logged in. |
| `RYOKAN_FRAME_ANCESTORS` | unset (any site may embed Ryokan) | Limit which pages may show Ryokan inside a frame. `self` allows only Ryokan itself, `none` allows nothing, and an origin like `https://dash.example.com` allows that page; separate several with spaces or commas. If you embed Ryokan in a dashboard such as Organizr or Homarr, leave this unset or list the dashboard's origin. Entries Ryokan can't read are skipped with a warning in the log, and if none are left only Ryokan itself may frame it. |
| `RYOKAN_HOST_CHECK` | unset (off) | Turn on the host check: the web UI answers only when you open it by an IP address, `localhost`, the machine's hostname, or a name in `RYOKAN_ALLOWED_HOSTS`. Other names get an error page. See [Host check](#host-check). |
| `RYOKAN_ALLOWED_HOSTS` | unset | Extra names the host check accepts, separated by spaces or commas: `ryokan.lan`, `*.example.com` (any name ending in `.example.com`). Setting it turns the host check on by itself. |
| `RYOKAN_DATA_DIR` | `/data` (image) | The folder for Ryokan's own files: the database, the encryption key, the artwork and anibridge caches, and the default `backups/` folder. Change it when `/data` is already your shared media or downloads mount. See [Moving Ryokan's data folder](#moving-ryokans-data-folder). |
| `RYOKAN_RESET_AUTH` | unset | Set to `1` *and* create a `.reset-auth` file next to `ryokan.db` to wipe users and sessions on next boot. Both required so a stuck-on env var can't silently wipe auth on every boot. See [Reset auth](#reset-auth). |
| `RYOKAN_DB_LOG_LEVEL` | `info` | Write-side floor for the DB-backed logs table (separate from `RUST_LOG`). One of `trace`, `debug`, `info`, `warn`, `error`. Read-side filtering on the System → Logs page is independent. |
| `RYOKAN_ENCRYPTION_KEY` | unset (file fallback) | Base64-encoded 32-byte AEAD key for encrypting OAuth tokens. Loading priority: env var, then key file, then auto-generated on first run. **Key rotation isn't supported**; changing it invalidates all stored OAuth tokens and you'll need to re-link external accounts. |
| `DATABASE_URL` | unset (`<data folder>/ryokan.db`) | SQLite connection string. Overrides the database location only. Most installs leave it unset. |
| `RYOKAN_KEY_FILE_PATH` | unset (`<data folder>/.ryokan-key`) | Where the auto-generated encryption key lives. Overrides this one path. Don't set it unless you have a specific reason. |
| `RYOKAN_ANIBRIDGE_CACHE_DIR` | unset (`<data folder>/cache/anibridge`) | Where the TMDB-to-AniList mappings cache lives. Overrides this one path. |
| `RYOKAN_MEDIA_CACHE_DIR` | unset (`<data folder>/cache/artwork`) | Artwork blob cache root. Overrides this one path. Content-addressed, so duplicate cover art doesn't re-store. |

## Volume layout

```yaml
volumes:
  - ryokan-data:/data
  - /srv/downloads:/downloads          # optional but required for post-processing
  - /srv/media/anime:/media/anime      # optional but required for post-processing
```

**`/data` (required)** holds the SQLite database, the artwork blob cache, the encryption key, the anibridge mappings cache, the default `backups/` folder, and any sentinel files. Loss of `/data` means losing your library state, queued grabs, scoring history, and OAuth tokens. The named-volume default (`ryokan-data`) keeps it inside Docker. Bind-mount it to a host path if you want the database visible from the host filesystem.

**`/downloads` and `/media/...`** are post-processing's source and destination. They're optional in the sense that Ryokan boots without them, but post-processing requires both to be visible inside the container at the same paths your download client uses for "complete" files and the path you set in Settings → General → Media Root Path.

### Moving Ryokan's data folder

Many *arr setups mount one shared filesystem at `/data` so the download client and every app see the same paths and can hardlink. Ryokan's own files can live somewhere else so that `/data` stays free for that mount. Set `RYOKAN_DATA_DIR` and mount a volume at the same path:

```yaml
volumes:
  - ryokan-config:/config
  - /srv/data:/data                    # your shared downloads and media
environment:
  - RYOKAN_DATA_DIR=/config
```

On start, the container takes ownership of the folder `RYOKAN_DATA_DIR` names and nothing else. Your shared `/data` mount keeps its owner. Setting `RYOKAN_DATA_DIR` is what moves the state. Pointing only the single-path variables (`DATABASE_URL` and the rest) at another folder still leaves `/data` as the folder the container takes ownership of, and the container log warns about it on every start.

To move an existing install:

1. Stop Ryokan.
2. Copy everything from the old data volume into the new one, including the hidden `.ryokan-key` file. Without that file, linked AniList and MyAnimeList accounts have to be linked again.
3. Remove `DATABASE_URL`, `RYOKAN_KEY_FILE_PATH`, `RYOKAN_MEDIA_CACHE_DIR`, and `RYOKAN_ANIBRIDGE_CACHE_DIR` from your compose file if you set them. Each one pins its own path and would keep it on the old volume.
4. Set `RYOKAN_DATA_DIR`, update the volume line, and start Ryokan.
5. If Settings → General → Backup folder is set to a path under the old data folder, such as `/data/backups`, clear it. Backups then go to `backups` in the new data folder.

Cover art keeps working from the moved cache. A backup made in System → Backup restores into the new layout too.

## Healthcheck

The image ships with:

```dockerfile
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS http://localhost:8978/login || exit 1
```

The probe targets `/login` because that's the canonical "is Ryokan up" endpoint. There is no `/healthz`. `/login` returns 200 once the auth UI is live, or 303 redirecting to `/setup` on a fresh container with no users yet. Both are valid "up" signals.

`start-period=30s` covers cold-boot work: idempotent migrations, password-hash warmup, the multi-client cache rebuild, and optional Jellyfin client init. ARM64 first-runs occasionally bumped against an earlier 10-second budget; 30 seconds matches the CI smoke-test poll window.

## Updating

```sh
docker compose pull
docker compose up -d
```

The named volume preserves your data. The image's binary is replaced. Migrations run automatically on next boot and are idempotent: applying twice is a no-op.

!!! danger "Don't `docker compose down -v`"
    The `-v` flag removes named volumes. With the documented setup, that means deleting your DB, encryption key, OAuth tokens, and library state. There's no undo. `down` without `-v` is safe.

## Branch and pull request images

Every branch pushed to the repository publishes a multi-arch image tagged with the branch name, with `/` replaced by `-`. A branch called `feat/misgrab-guardrails` is pullable as:

```bash
docker pull ghcr.io/johnthreekay/ryokan:feat-misgrab-guardrails
```

`main` and `dev` keep their own tags, and releases keep `latest` and the version tags.

A pull request from a fork cannot publish to the registry, so its build lands as a workflow artifact instead. Open the pull request's **Publish Docker image** run under Actions, download `ryokan-pr-<number>-linux-amd64` (or `-linux-arm64`), and load it:

```bash
docker load -i ryokan-pr-123-linux-amd64.tar
docker run -p 8978:8978 -v ./data:/data ghcr.io/johnthreekay/ryokan:pr-123
```

The artifact is kept for 14 days after the run.

## Reset auth

If you forget your admin password and have no other recovery path, you can wipe the users and sessions tables and create a new admin account on next boot. Two steps are required so a stuck-on env var can't silently wipe auth on every restart:

1. Add `RYOKAN_RESET_AUTH=1` to your compose file's environment block.
2. Create the sentinel file next to `ryokan.db`, which is in the data folder unless you set `DATABASE_URL`: `touch /path/to/your/data-volume/.reset-auth` on the host, or `docker exec ryokan touch /data/.reset-auth` (use your `RYOKAN_DATA_DIR` in place of `/data` if you changed it).

Restart the container. On boot, Ryokan deletes both tables, removes the sentinel, and `/setup` opens for a fresh admin account. Remove `RYOKAN_RESET_AUTH` from your compose file afterwards.

OAuth tokens, library state, scoring history, and Custom Formats are preserved. Only authentication state is wiped.

## Host check

A web page on another site can point its own name at your Ryokan's address and then use Ryokan as if it were that site. Before the first account exists, that is enough to create the admin account. The host check (off by default, like Transmission's host whitelist) stops it: with `RYOKAN_HOST_CHECK=1`, the web UI answers only when you open it by an IP address (`http://192.168.1.20:8978`), `localhost`, the machine's hostname, or a name you list in `RYOKAN_ALLOWED_HOSTS`. Anything else gets a "421" page naming the setting.

- If you open Ryokan by a name, such as `ryokan.lan` or a domain on your reverse proxy, add that name to `RYOKAN_ALLOWED_HOSTS`.
- Behind a reverse proxy with `RYOKAN_TRUSTED_PROXY=1`, the name in `X-Forwarded-Host` has to pass too, and so does the name the proxy uses to reach Ryokan, unless the proxy passes the original one through (nginx: `proxy_set_header Host $host;`).
- Seerr, autobrr and calendar apps are not affected: the Sonarr / Radarr API, the autobrr webhook and the iCal feed check their API key instead, so other containers can keep calling Ryokan by its service name.

## Running behind a reverse proxy

If you put Ryokan behind nginx, Caddy, Traefik, or similar, set `RYOKAN_TRUSTED_PROXY=1` so the per-IP login throttle reads `X-Forwarded-For` from the proxy instead of the proxy's own IP. The proxy must overwrite these headers on ingress (don't pass through whatever the client sent), or you've just added a header-spoofing bypass.

With that set, the login cookie is marked `Secure` automatically whenever the proxy reports HTTPS (`X-Forwarded-Proto: https`), the same way Sonarr does it. `RYOKAN_COOKIE_SECURE=1` forces the flag for a proxy that doesn't send that header.

The [Stack builder](stack-builder.md) generates Caddy / Traefik / nginx config with the right header rewrites and env-var combinations.

---

*Last updated: 2026-10-04.*
