# Docker reference

Environment variables, volumes, updating, and a few less common setups. For a step-by-step install, see [Docker installation](install.md).

## Environment variables

Most installs only set `PUID`, `PGID` and `TZ`.

| Variable | Default | Purpose |
|---|---|---|
| `PUID` / `PGID` | `1000` / `1000` | The user and group Ryokan runs as. See [PUID and PGID](install.md#puid-and-pgid). |
| `TZ` | UTC | Timezone for times in the UI and logs, such as `America/Chicago` or `Europe/London`. |
| `LISTEN_ADDR` | `0.0.0.0:8978` | Address and port Ryokan listens on. |
| `RUST_LOG` | `ryokan=info` | What is printed to the container log. `ryokan=debug` prints more. |
| `RYOKAN_DB_LOG_LEVEL` | `info` | The log level for System → Logs: `trace`, `debug`, `info`, `warn` or `error`. Trace is the most verbose while error is the least verbose. |
| `RYOKAN_DATA_DIR` | `/data` | The folder for Ryokan's own files. See [Moving Ryokan's data folder](#moving-ryokans-data-folder). |
| `RYOKAN_TRUSTED_PROXY` | off | Set to `1` behind a reverse proxy. See [Running behind a reverse proxy](#running-behind-a-reverse-proxy). |
| `RYOKAN_COOKIE_SECURE` | off | Forces the `Secure` flag on the login cookie. Only needed for an HTTPS proxy that does not send `X-Forwarded-Proto`. With this set you cannot stay logged in over plain HTTP. |
| `RYOKAN_FRAME_ANCESTORS` | any site | Which pages may show Ryokan inside a frame: `self`, `none`, or one or more origins like `https://dash.example.com`, separated by spaces or commas. Leave it unset, or list your dashboard, if you embed Ryokan in Organizr or Homarr. |
| `RYOKAN_HOST_CHECK` | off | Set to `1` to turn on the [host check](#host-check). Ryokan's pages will then only answer to an IP address, `localhost`, or a name listed in `RYOKAN_ALLOWED_HOSTS`. |
| `RYOKAN_ALLOWED_HOSTS` | unset | The names you open Ryokan by, for the host check, separated by spaces or commas: `nas ryokan.lan *.example.com`. Leave out `http://` and the port. Setting this turns the host check on, even without `RYOKAN_HOST_CHECK`. |
| `RYOKAN_CUSTOM_SCRIPTS` | on | Set to `0` to turn off [custom scripts](system.md#custom-scripts). Saved ones are kept but not run, and none can be added. |
| `RYOKAN_RESET_AUTH` | unset | Used to reset a forgotten password. See [Reset auth](#reset-auth). |
| `RYOKAN_ENCRYPTION_KEY` | unset | A base64-encoded 32-byte key used to encrypt linked account tokens. When unset, Ryokan generates one and stores it in the data folder. Changing the key will force you to link your accounts again. |

These override where single files and folders are kept. Most installs should leave them unset and use `RYOKAN_DATA_DIR`.

| Variable | Default |
|---|---|
| `DATABASE_URL` | `<data folder>/ryokan.db` |
| `RYOKAN_KEY_FILE_PATH` | `<data folder>/.ryokan-key` |
| `RYOKAN_ANIBRIDGE_CACHE_DIR` | `<data folder>/cache/anibridge` |
| `RYOKAN_MEDIA_CACHE_DIR` | `<data folder>/cache/artwork` |

## Volume layout

```yaml
volumes:
  - /srv/docker/ryokan:/config         # Ryokan's own files
  - /srv/media:/srv/media              # downloads and library
environment:
  RYOKAN_DATA_DIR: /config
```

This is the layout the [Quick start](quick-start.md) generates.

**`/config`** holds the database, the encryption key, caches and backups. If you lose it you lose your library, history and linked accounts. `RYOKAN_DATA_DIR` tells Ryokan to keep its files there. Without it, Ryokan uses `/data`.

**`/srv/media`** is one folder that holds both your downloads and your library, for example `/srv/media/downloads` and `/srv/media/anime`. Ryokan starts without it but cannot import anything. Mount the same folder at the same path in your download client, so both see a finished file at the same place.

One folder for both matters because a hardlink only works inside a single mount. With hardlinks, an imported episode takes no extra space and the torrent keeps seeding.

### Separate downloads and library folders

```yaml
volumes:
  - ryokan-data:/data
  - /srv/downloads:/downloads
  - /srv/media/anime:/media/anime
```

This works, but Ryokan cannot hardlink between two mounts, even when both folders are on the same disk. It copies each file instead, so an episode takes its space twice for as long as the download stays in the client. If that's what you want, set the file operation mode to Copy or Move under Settings → General.

### Moving Ryokan's data folder

An install that keeps Ryokan's files in `/data` can move them, to free `/data` for a shared media folder or to match the layout above. The container takes ownership of the folder `RYOKAN_DATA_DIR` names and leaves every other mount alone.

To move an existing install:

1. Stop Ryokan.
2. Copy everything from the old data volume to the new one, including the hidden `.ryokan-key` file. Without it you have to link your AniList and MyAnimeList accounts again.
3. If your compose file sets `DATABASE_URL`, `RYOKAN_KEY_FILE_PATH`, `RYOKAN_MEDIA_CACHE_DIR` or `RYOKAN_ANIBRIDGE_CACHE_DIR`, remove them. They would keep pointing at the old location.
4. Set `RYOKAN_DATA_DIR`, change the volume line, and start Ryokan.
5. If Settings → General → Backup folder points into the old data folder, such as `/data/backups`, clear it.

## Healthcheck

The image checks itself by requesting `/login` every 30 seconds, and `docker ps` shows the result as healthy or unhealthy.

## Updating

```sh
docker compose pull
docker compose up -d
```

!!! danger "Do not run `docker compose down -v`"
    `-v` deletes named volumes, which means your database, encryption key and library. It cannot be undone. `docker compose down` without `-v` is safe.

## Branch and pull request images

Every branch pushed to the repository is published as an image tagged with the branch name, with `/` replaced by `-`. For the example branch `feat/hello-bro`:

```bash
docker pull ghcr.io/johnthreekay/ryokan:feat-hello-bro
```

Pull requests from forks cannot publish images. Their build is attached to the pull request's **Publish Docker image** run under Actions, as `ryokan-pr-<number>-linux-amd64` or `-linux-arm64`, and is kept for 14 days. Download it and load it:

```bash
docker load -i ryokan-pr-123-linux-amd64.tar
docker run -p 8978:8978 -v ./data:/data ghcr.io/johnthreekay/ryokan:pr-123
```

## Reset auth

If you forget your password, you can delete the account and create a new one. Your library, settings and linked accounts are kept.

1. Add `RYOKAN_RESET_AUTH=1` to the environment section of your compose file.
2. Create an empty file named `.reset-auth` next to `ryokan.db`:

    ```sh
    docker exec ryokan touch /data/.reset-auth
    ```

    Use your `RYOKAN_DATA_DIR` in place of `/data` if you changed it.

3. Start Ryokan again from the folder that holds your compose file:

    ```sh
    docker compose up -d
    ```

    This recreates the container with the new variable. You can optionally use `--force-recreate` to be absolutely certain the container is recreated.

4. Open Ryokan. It asks you to create a new account.
5. Remove `RYOKAN_RESET_AUTH` from the compose file and run `docker compose up -d` once more.

Both the variable and the file are required, so that a variable left in the compose file cannot delete your account on every restart.

## Host check

The host check makes the web UI answer only when it is opened by an IP address, by `localhost`, or by a name you have listed. It is off by default.

It protects against DNS rebinding, where a web page on another site points its own name at your Ryokan's address and then acts as if it were Ryokan's own page.

### Turning it on

1. Work out every name you open Ryokan by. Look at the address bar on each device you use: `http://nas:8978` means `nas`, and `https://ryokan.example.com` means `ryokan.example.com`. IP addresses and `localhost` always work and do not need listing.
2. Add the names to the environment section of your compose file:

    ```yaml
    environment:
      - RYOKAN_HOST_CHECK=1
      - RYOKAN_ALLOWED_HOSTS=nas ryokan.lan ryokan.example.com
    ```

    Separate the names with spaces or commas. Leave out `http://` and the port. `*.example.com` covers every name under `example.com`.

3. Apply the change from the folder that holds your compose file:

    ```sh
    docker compose up -d
    ```

4. Open Ryokan by each of the names. A name you forgot gets a "421" error page instead of Ryokan. If that happens, open Ryokan by its IP address, which always works, add the name to the list, and run `docker compose up -d` again.

If you only ever open Ryokan by IP address or `localhost`, set `RYOKAN_HOST_CHECK=1` on its own and leave `RYOKAN_ALLOWED_HOSTS` out.

To turn the check off, remove both lines and run `docker compose up -d`.

### Things to know

- Ryokan also accepts the name of the machine it runs on. Inside Docker that is the container's name, which is a random ID unless your compose file sets `hostname:`. So your server's name still has to be listed.
- Behind a reverse proxy with `RYOKAN_TRUSTED_PROXY=1`, the name the proxy uses to reach Ryokan has to be listed as well, unless the proxy passes the original host through (nginx: `proxy_set_header Host $host;`).
- Seerr, autobrr and calendar apps are not affected. They use an API key and can keep calling Ryokan by its service name.

## Running behind a reverse proxy

Set `RYOKAN_TRUSTED_PROXY=1`. Ryokan then reads the visitor's address from `X-Forwarded-For`, which the login rate limit needs, and marks the login cookie `Secure` when the proxy reports HTTPS.

Only set it behind a proxy that overwrites `X-Forwarded-For` itself. If the proxy passes along whatever the visitor sent, anyone can fake their address and get around the login rate limit.

The [Quick start](quick-start.md#2-choose-your-stack) can add a proxy to your stack. Caddy works once you put your domain in. Traefik needs three labels added to Ryokan's service, and nginx needs a config you write yourself. The generated file has a starting point for each.

---

*Last updated: 2026-11-18.*
