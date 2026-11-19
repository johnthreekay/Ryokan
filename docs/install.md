# Docker installation

The Ryokan image is `ghcr.io/johnthreekay/ryokan:latest`, for `linux/amd64` and `linux/arm64`.

This page covers the Ryokan container on its own. For Ryokan together with a media server and a download client, follow the [Quick start](quick-start.md).

## Quick install

Save this as `docker-compose.yml`:

```yaml
services:
  ryokan:
    image: ghcr.io/johnthreekay/ryokan:latest
    container_name: ryokan
    ports:
      - "8978:8978"
    volumes:
      - ryokan-data:/data
    environment:
      - PUID=1000
      - PGID=1000
      - TZ=Etc/UTC
    restart: unless-stopped

volumes:
  ryokan-data:
```

Then run:

```sh
docker compose up -d
```

Open <http://localhost:8978> and create your account.

## Recommended install (with a media folder)

The quick install lets you search and grab, but Ryokan cannot place finished downloads in your library until it can see both. Add one mount, for a folder that holds your downloads and your library:

```yaml
services:
  ryokan:
    image: ghcr.io/johnthreekay/ryokan:latest
    container_name: ryokan
    ports:
      - "8978:8978"
    volumes:
      - ryokan-data:/data
      - /srv/media:/srv/media              # downloads and library
    environment:
      - PUID=1000
      - PGID=1000
      - TZ=America/Chicago
    restart: unless-stopped

volumes:
  ryokan-data:
```

Here `/srv/media` holds two folders, `/srv/media/downloads` and `/srv/media/anime`. Use your own folder in place of `/srv/media` on both sides of the mount.

- Enter `/srv/media/anime` as the **Media Root Path**, during first-run setup or later under Settings → General.
- Mount the same folder at the same path in your download client, and have the client save into `/srv/media/downloads`. Ryokan looks for a finished file at exactly the path the client reports.

Keep downloads and the library in one mount like this. A hardlink only works inside a single mount, and with hardlinks an imported episode takes no extra space while the torrent keeps seeding. With two separate mounts Ryokan has to copy each file instead. See [Volume layout](docker.md#volume-layout).

## PUID and PGID

```yaml
environment:
  - PUID=1000
  - PGID=1000
```

Ryokan runs as this user and group inside the container. Set them to the user that owns your media, so the files Ryokan writes have the same owner as the rest of your library. To find the values, run `id -u` and `id -g` as that user. On most systems both are 1000.

Other containers that follow the [linuxserver.io convention](https://docs.linuxserver.io/general/understanding-puid-and-pgid/) use the same two variables. Give them the same values.

!!! warning "Ryokan does not change the owner of your media"
    On start, the container takes ownership of its own data folder (`/data`) and nothing else. Your downloads and library folders must already be writable by the user you set here.

## First-run setup

The first time you open Ryokan it asks you to create an account. After that the setup page is closed.

If you forget the password, see [Reset auth](docker.md#reset-auth).

## Updating

```sh
docker compose pull
docker compose up -d
```

Your data is in the volume and is kept. The database is upgraded automatically on start. See [Docker reference → Updating](docker.md#updating) for one command to avoid.

## Where next

- **[Configuration](configuration.md)**: the Settings tabs.
- **[Download clients](download-clients.md)**: setup notes for each client.
- **[External accounts](external-accounts.md)**: link AniList or MAL.
- **[Docker reference](docker.md)**: environment variables, reverse proxies and other details.
- **[Build from source](from-source.md)**: run Ryokan without Docker.

---

*Last updated: 2026-11-18.*
