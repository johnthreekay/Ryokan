# Welcome

**Ryokan** is a self-hosted **anime PVR**. Think of it as a personal recorder for anime releases. You add the shows you want to your library, Ryokan watches for new episodes, picks the best release of each one, and sends it to your download client. Once the file lands, it's renamed and dropped into your media library so Jellyfin (or whatever you use) sees it as a normal episode.

If you've used Sonarr for TV, the shape is the same. Ryokan is just the anime-tuned version of that idea: release-group reputation, batch packs, fansub conventions, [SeaDex](https://releases.moe) authoritative picks, AniList as the metadata source.

!!! info "Project status"
    Ryokan is a one-person project; expect rough edges. v1.X handles anime only; manga and light novels are on the roadmap for v2.X.

## What you can do with it

- **Track anime from AniList or MAL.** Link your account and Ryokan auto-adds shows you mark as watching. Or add them manually from Ryokan's search.
- **Search multiple sources at once.** Built-in [Nyaa](https://nyaa.si) search, torznab and newznab indexers via Prowlarr or Jackett, direct RSS feeds, and autobrr webhooks all merge into one ranked result list.
- **Pick the best release automatically.** A quality profile, [TRaSH-Guides](https://trash-guides.info)-compatible Custom Formats (the same scoring rules Sonarr uses), and optional [SeaDex](https://releases.moe) picks (a community-curated list of best anime releases) decide what gets grabbed.
- **Re-grab when something better lands.** Set a quality cutoff once; Ryokan watches for upgrades and replaces older grabs as higher-scoring releases show up.
- **See what's airing this week.** A built-in [Calendar](calendar.md) shows upcoming episodes for the shows in your library, as a list or a month grid. You can also subscribe to it from your phone or laptop's calendar app.
- **Use the download client you already have.** qBittorrent, Deluge, Transmission, rTorrent, or SABnzbd. Run more than one at once and Ryokan routes per-grab.
- **Land files in your library automatically.** Hardlink (default; keeps the torrent seeding), copy, or move.
- **Import the anime you already have.** Point the [import wizard](manual-import.md) at a folder; it matches each series on AniList and lands the files in place.
- **Undo a deletion.** With a recycle bin configured, deleted episodes and series folders wait there until you restore or purge them.
- **Back up and restore from the UI.** Snapshot the database and encryption key from System → Backup, on a schedule if you like, and restore by uploading the archive.
- **Plug into Seerr.** Ryokan exposes a Sonarr/Radarr-compatible API so Seerr requests anime the same way it asks Sonarr for TV.

## Get started

Ryokan runs as a single Docker container; if anything else in your homelab does, this will too. New to Docker? [Docker's overview](https://docs.docker.com/get-started/docker-overview/) is a 10-minute read that covers containers, images, and volumes, which the rest of these docs assume you've got a feel for.

!!! tip "Most users should start here"
    The **[Stack builder](stack-builder.md)** generates a complete `docker-compose.yml` for Ryokan **plus** your download client, Jellyfin, Seerr, and a reverse proxy if you want one. Click through a checkbox form, copy the result, run `docker compose up -d`. Paths line up so post-processing works without fiddling, and the matching Ryokan settings are printed alongside so you know what to paste in once Ryokan is up.

If you'd rather build the stack yourself, work through these in order:

1. **[Quick start](quick-start.md)**: get Ryokan running and grabbing in about 10 minutes. Hands-held end to end.
2. **[Docker installation](install.md)**: the Docker-only details. Skip if the Stack builder generated your compose for you.
3. **[Configuration](configuration.md)**: the tabs in Settings, what each does, what to leave alone.
4. **[Download clients](download-clients.md)**: per-client setup notes (qBit, Deluge, Transmission, rTorrent, SABnzbd).
5. **[External accounts](external-accounts.md)**: link AniList or MAL so your watch list pulls into Ryokan automatically.

## When something's wrong

- **[Troubleshooting](troubleshooting.md)**: concrete diagnostic steps for the most common "why didn't this work" cases.
- **[FAQ](faq.md)**: how Ryokan compares to Sonarr, multi-user, manga support, API access, backup.

---

*Last updated: 2026-08-29.*
