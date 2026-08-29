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
| `RYOKAN_COOKIE_SECURE` | unset (off) | Append `Secure` to the session cookie. Off by default so HTTP localhost works; flip on for HTTPS. |
| `RYOKAN_RESET_AUTH` | unset | Set to `1` *and* create a `data/.reset-auth` sentinel file to wipe users and sessions on next boot. Both required so a stuck-on env var can't silently wipe auth on every boot. See [Reset auth](#reset-auth). |
| `RYOKAN_DB_LOG_LEVEL` | `info` | Write-side floor for the DB-backed logs table (separate from `RUST_LOG`). One of `trace`, `debug`, `info`, `warn`, `error`. Read-side filtering on the System → Logs page is independent. |
| `RYOKAN_ENCRYPTION_KEY` | unset (file fallback) | Base64-encoded 32-byte AEAD key for encrypting OAuth tokens. Loading priority: env var, then key file, then auto-generated on first run. **Key rotation isn't supported**; changing it invalidates all stored OAuth tokens and you'll need to re-link external accounts. |
| `RYOKAN_KEY_FILE_PATH` | `/data/.ryokan-key` (Docker) | Where the auto-generated encryption key lives. Set in the image. Don't change unless you have a specific reason. |
| `RYOKAN_ANIBRIDGE_CACHE_DIR` | `/data/cache/anibridge` (Docker) | Where the TMDB-to-AniList mappings cache lives. If unset, the cache fails to persist and every restart re-downloads about 9 MB. |
| `RYOKAN_MEDIA_CACHE_DIR` | `/data/cache/artwork` (Docker) | Artwork blob cache root. Content-addressed, so duplicate cover art doesn't re-store. |

## Volume layout

```yaml
volumes:
  - ryokan-data:/data
  - /srv/downloads:/downloads          # optional but required for post-processing
  - /srv/media/anime:/media/anime      # optional but required for post-processing
```

**`/data` (required)** holds the SQLite database, the artwork blob cache, the encryption key, the anibridge mappings cache, the default `backups/` folder, and any sentinel files. Loss of `/data` means losing your library state, queued grabs, scoring history, and OAuth tokens. The named-volume default (`ryokan-data`) keeps it inside Docker; bind-mount to a host path if you want the database visible from the host filesystem.

**`/downloads` and `/media/...`** are post-processing's source and destination. They're optional in the sense that Ryokan boots without them, but post-processing requires both to be visible inside the container at the same paths your download client uses for "complete" files and the path you set in Settings → General → Media Root Path.

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

## Reset auth

If you forget your admin password and have no other recovery path, you can wipe the users and sessions tables and create a new admin account on next boot. Two steps are required so a stuck-on env var can't silently wipe auth on every restart:

1. Add `RYOKAN_RESET_AUTH=1` to your compose file's environment block.
2. Create the sentinel file: `touch /path/to/your/data-volume/.reset-auth`.

Restart the container. On boot, Ryokan deletes both tables, removes the sentinel, and `/setup` opens for a fresh admin account.

OAuth tokens, library state, scoring history, and Custom Formats are preserved. Only authentication state is wiped.

## Running behind a reverse proxy

If you put Ryokan behind nginx, Caddy, Traefik, or similar, set `RYOKAN_TRUSTED_PROXY=1` so the per-IP login throttle reads `X-Forwarded-For` from the proxy instead of the proxy's own IP. The proxy must overwrite these headers on ingress (don't pass through whatever the client sent), or you've just added a header-spoofing bypass.

Also set `RYOKAN_COOKIE_SECURE=1` if the proxy serves Ryokan over HTTPS, so the session cookie carries the `Secure` flag.

The [Stack builder](stack-builder.md) generates Caddy / Traefik / nginx config with the right header rewrites and env-var combinations.

---

*Last updated: 2026-08-29.*
