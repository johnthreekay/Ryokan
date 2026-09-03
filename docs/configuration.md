# Configuration

This page is the Settings reference. Once Ryokan is up and you've worked through the [quick start](quick-start.md), come here to look up what each control does. The Settings UI lives at `/settings` once you're logged in.

Changes apply on save; no restart needed.

## Connections

Third-party services Ryokan talks to.

- **AniList / MyAnimeList accounts**: OAuth-linked for watch-list sync. When linked, anime you mark "watching" (or "planning", "completed", etc.) on AniList or MAL get auto-added to your Ryokan library on the next sync tick. Setup walkthrough: [External accounts](external-accounts.md).
- **Sync interval (minutes)**: how often the watch-list sync runs. Default 30 minutes; minimum 15, maximum 10080 (7 days). The form won't let you type anything below 15. If a value somehow ends up outside that range, it falls back to 30.
- **Jellyfin**: server URL and API key. Lets Ryokan trigger a Jellyfin library refresh after each import and validate that imported files actually landed on disk. URL is `http://jellyfin:8096` when Ryokan and Jellyfin share a Docker compose; if they're on different hosts or in separate composes, use your host's LAN IP and the host-mapped port.
- **Sonarr / Radarr API shim (anibridge)**: exposes a Sonarr-compatible and Radarr-compatible API so request frontends like Seerr can ask Ryokan for anime the same way they'd ask Sonarr for TV. The Sonarr side lives at `/api/v3/...`, the Radarr side at `/radarr/api/v3/...`. Each has its own API key.

## Download Clients

Pick one or more of qBittorrent, Deluge, Transmission, rTorrent, SABnzbd. Ryokan supports running multiple at once and routes per-grab. Per-client setup notes (URLs, credentials, common gotchas) live on the [Download clients](download-clients.md) page.

The **Default client** checkbox is per-protocol, not global. You can have one default torrent client and one default usenet client coexisting; Nyaa and torznab indexer grabs route to the torrent default, and newznab indexer grabs route to the usenet default.

## Indexers

Add torznab indexers (typically fronted by Prowlarr) and newznab indexers (typically Jackett or direct from NZBGeek-style services) here. Direct RSS feeds from sources like SubsPlease also live in this tab.

**Indexer** here means a search source. Ryokan ships with built-in Nyaa search; everything else lands in this tab.

Each indexer row has an optional **download client pin** that overrides the per-protocol default for grabs from that indexer. Useful when you want one private tracker's grabs going to a specific qBit instance with stricter seed rules.

Two more things live on this tab:

- **autobrr webhook**: accepts inbound webhooks at `/api/webhook/autobrr`. [autobrr](https://autobrr.com) is a separate self-hosted tool that watches IRC announce channels for new releases and pushes matches as HTTP webhooks; this is the receiving side. The webhook has its own API key with a dedicated regenerate button, so an accidental tab POST can't silently rotate or wipe it.
- **Nyaa search** pin: Ryokan's built-in Nyaa search is not an indexer row, so this fieldset is where you pin it to a specific torrent client. **(use default)** routes Nyaa grabs to the torrent default.

## Preferred Quality & Releases

The scoring inputs that decide which release wins when several match the same episode.

- **Preferred / blocked groups**: whitelists and blacklists of release-group names (e.g. `[VCB-Studio]`). Preferred groups boost scores; blocked groups exclude their releases entirely.
- **Preferred resolution / source**: a coarse first-pass filter. Releases below the resolution floor or wrong source (e.g. WEB-DL when you want BluRay) get filtered out before scoring runs.
- **Cutoff source / resolution**: once an episode has a release at or above the cutoff, the upgrade-search task stops looking for better. Set this to "I'll take 1080p WEB-DL but stop churning through grabs once I've got that."
- **Finished-series quality**: a separate cutoff that applies once AniList marks the series as `FINISHED`. Pattern: WEB while a series is airing, BluRay once the season's done.
- **Audio preference**: subtitled, dubbed, or no preference. Affects scoring, not filtering.
- **SeaDex enabled**: when on, Ryokan consults [SeaDex](https://releases.moe) (a community-curated list of "best release" picks per AniList ID) and gives matching releases a large score bonus. Adding a Custom Format that uses the `SeaDexBest` spec automatically suppresses this toggle so you don't double-count.
- **Default custom query tokens / restrict-to-uploader**: defaults pre-filled into the manual search modal so common filters don't have to be retyped.

## Custom Formats

[Sonarr-style](https://wiki.servarr.com/sonarr/settings#custom-formats) scoring rules. Each release gets scored against every Custom Format; the cumulative score is a tiebreaker on top of the resolution/source profile.

- **Custom Formats list**: add, edit, delete CFs. Includes a release-title test box (paste a release title; see which CFs match and what the cumulative score is) and an Install Defaults button that loads a bundled anime-tuned set.
- **Minimum Score**: a floor for auto-search candidates. Releases scoring below this are silently dropped from auto-search but still show up in interactive search where you can override.
- **Import / Export**: Ryokan round-trips Sonarr v4 CF JSON, so you can paste [TRaSH-Guides](https://trash-guides.info) JSON (a community-maintained set of CF presets) or copy an existing Sonarr setup. The Ryokan-native export keeps Ryokan-only specs (`Ryokan.SeaDexBestSpecification`) verbatim.

## Release Groups

A per-group reputation map. Tells the classifier things like "VCB-Studio always means BluRay encode, regardless of what the filename claims." Used as one of the layers the source classifier consults when the filename alone is ambiguous about BD vs. WEB.

The mapping ships seeded from a bundled table (rows tagged `seed`). The tab also lists **Suggested Mappings** you can accept or edit, and your own rows (tagged `user`) take precedence over the seed.

## API Keys

Issue API keys for outside tools that need to talk to Ryokan. Each key gets a name, a list of permissions ("scopes") that decide what the key can do, and shows you the key text once when you create it. Save it then; if you lose it, regenerate.

- **calendar**: lets the key read the iCal subscription feed. Calendar apps (Apple Calendar, Google Calendar, Thunderbird) can't log in like a browser, so the subscription URL carries the key in the URL itself. The [Calendar](calendar.md) page has a button that builds the full URL for you.
- **search**, **library:read**, **library:write**: reserved for API surfaces that don't exist yet. A key can carry them, but nothing checks them today.
- **admin**: covers everything. Use it sparingly; prefer narrower scopes when one fits.

## General

Day-to-day knobs.

- **Media Root Path**: where Ryokan imports completed downloads. The value is the path *inside* Ryokan's container. With the default compose, `/media/anime` maps to `/srv/media/anime` on the host.
- **Enable automatic RSS sync**: polls the official Nyaa anime RSS feed on the interval below and auto-grabs matching releases.
- **RSS Sync Interval (minutes)**: how often the background RSS poller runs. Default 15 minutes; minimum 1, maximum 60.
- **Skip the Nyaa RSS feed**: the background poller skips Nyaa entirely; indexer RSS feeds (torznab / newznab) and direct RSS feeds still run on the same interval. Use it when you only want releases from your configured indexers.
- **Enable post-processing**: rename and move completed downloads into the media library and write NFO sidecars for Jellyfin. Needs a media root and a download client.
- **File operation mode**: `hardlink` (default; keeps the torrent seeding by sharing the same inode between the download folder and the library), `copy`, or `move`. Hardlink automatically falls back to copy when the source and destination are on different filesystems (where hardlinks aren't possible).
- **Preferred Title Language**: `romaji` / `english` / `native`. Display-only for scoring and search, which match across all three regardless. It also picks the title the `{series.title}` naming token renders.
- **File naming**: three templates decide where an import lands. **Series folder** (default `{series.title}`) applies once, when a series is added. Series already in your library keep their folder, so changing it never renames anything. **Season folder** (default `Season {season.number:00}`) and **Episode file** (default `{series.title} - S{season.number:00}E{episode.number:00} - {episode.title}{ext}`) apply to every import. Files already in the library keep their names. Each field shows a sample as you type, the combined path shows underneath, and **Reset** puts the default back. A token with no value (a show AniList has no episode titles for, a release with no group) drops out together with its brackets or separator, so `[{quality.full}]` never leaves an empty `[]` behind. The episode template must end with `{ext}` and include `{episode.number}`, and Ryokan checks that it can read the episode number back out of the sample name, because library scans and upgrades depend on that. Characters that are not allowed in file names (`/ \ : * ? " < > |`) become `_`. A name that would exceed the filesystem limit is shortened at the series title so the episode number and extension always survive. On Windows a warning appears when the sample path passes 260 characters.

    | Token | Renders |
    |---|---|
    | `{series.title}` | Series title in your preferred title language |
    | `{series.year}` | Premiere year, empty when unknown |
    | `{season.number}` | Season number (always 1 today). `{season.number:00}` pads to `01` |
    | `{episode.number}` | Episode number. `{episode.number:00}` pads to `01`, `{episode.number:000}` to `001` |
    | `{episode.title}` | Episode title from metadata, empty when unknown |
    | `{quality.full}` | Resolution and source together, like `1080p WEB-DL` or `1080p BluRay Remux` |
    | `{quality.resolution}` | `1080p`, `720p`, ... |
    | `{quality.source}` | `BluRay`, `BluRay Remux`, `WEB-DL`, `WEBRip`, `DVD`, `HDTV` |
    | `{group}` | Release group, empty when unknown |
    | `{ext}` | The file extension with its dot. The episode file template must end with it |

    Quality and group come from the release Ryokan grabbed (the same signals it scored), so a manual quality override on an episode is honored. Renaming files already in the library to a new template is not part of this release.
- **Scheduled backups**: off by default. Daily or weekly, Ryokan writes a backup of the database and encryption key (plus cached artwork if ticked) to the backup folder and keeps the newest N. Manual backups, the folder's contents, and restores live on [System → Backup](system.md#backup). Leave it off if you already back up the whole data folder some other way, but note that a plain file copy of `ryokan.db` taken while Ryokan runs can miss recent writes; the built-in backup does not.
- **Backup folder**: empty keeps backups in a `backups` folder next to the database (`/data/backups` in Docker). The value is the path inside Ryokan's container. A folder on another disk or a mounted share is the point.
- **Backups to keep**: older scheduled backups are deleted after each new one. Default 7. Backups taken automatically before a restore are never pruned.
- **Recycle bin path**: empty by default, which means deletes are permanent. Set it to a directory (inside the container, like the media root) and deleting an episode, removing a series with its files, or replacing a file during an upgrade moves the files there instead. Each entry keeps the video plus its `.nfo`, subtitles, and thumbnail, and the Library page's Recycle Bin view can restore or permanently delete it. Keep it on the same filesystem as the media root so the move is an instant rename that preserves seeding hardlinks. On a different filesystem Ryokan copies, verifies the size, then deletes. If the path is set but Ryokan cannot write to it, deletes are refused until you fix it or clear the path. Restore puts the files back and re-tags the episode, but a torrent that was removed from the download client at delete time is not re-added, and files that crossed filesystems come back with a new modification time. Clearing the path leaves anything already recycled where it is.
- **Purge after (days)**: how long recycled items survive before the hourly cleanup task deletes them for good. Default 14. `0` keeps everything until you empty the bin manually.
- **Search when monitoring is widened**: when a series switches to monitoring more episodes (for example none → all), run a search for the newly monitored ones. Off by default.
- **Add the series to the library when grabbing from Search**: on by default. Off sends the release to the download client without tracking the series.
- **Interactive file picker**: whether the file picker opens for multi-file releases. **Batches only** (default) opens it for batches and one-clicks single-file releases; **Never** is one-click everywhere.
- **Search for monitored episodes when a series is added**: on by default. Off adds the series without starting a download.
- **Allow non-English releases in automatic grabs**: when off, auto-search and RSS use Nyaa's English-translated category. When on, they search every anime category, including untranslated and multi-sub releases. Interactive search always shows every category.
- **Remove and blocklist detected misgrabs**: when the files inside a download clearly name a different series, Ryokan removes the download from the client, blocklists the release, notifies you, and searches again. On by default. Off keeps the download in the client, never imports it, and lists it under System, Misgrabs for you to restore or dismiss.

## On the System page (not Settings)

The **Force MAL / Kitsu fallback** switches live on the **System** page rather than under Settings. See [System → Debug](system.md#debug).

## Reset / wipe state

- **Wipe library + grab history**: there's no global UI button (**System → Debug → Clear grab history** only clears the RSS poller's grab history). The closest is per-series "Remove from library", which cleans up that series' rows and optionally deletes the on-disk files.
- **Wipe everything**: stop Ryokan, delete its data folder (`/srv/docker/ryokan` if you followed the [quick start](quick-start.md), or the named Docker volume otherwise), restart. First-run setup runs again.
- **Reset auth only**: when you've forgotten your admin password but want to keep your library and OAuth tokens intact. See [Docker reference → Reset auth](docker.md#reset-auth) for the two-step gate.

---

*Last updated: 2026-08-29.*
