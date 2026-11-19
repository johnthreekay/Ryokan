# Configuration

This page describes each tab under **Settings**. Changes take effect when you save. Nothing needs a restart.

## Connections

Other services Ryokan talks to.

- **AniList / MyAnimeList**: link an account and shows on your watch list are added to the library. See [External accounts](external-accounts.md).
- **Sync interval (minutes)**: how often the watch list is checked. Default 30, minimum 15, maximum 10080 (7 days).
- **Media Server**: your Jellyfin, Emby or Plex server. Pick the **Type**, then enter its URL and API key. Ryokan asks the server to refresh its library after each import. For Jellyfin the URL is `http://jellyfin:8096` when both run in the same compose file. Otherwise use the address you open the server with in a browser. Emby keys are under **API Keys** in its server settings. For Plex, paste your X-Plex-Token in the API key field. Ryokan connects to one media server at a time.
- **Sonarr / Radarr API**: lets Seerr and similar tools request anime from Ryokan as if it were Sonarr or Radarr. The Sonarr API is at `/api/v3/`, the Radarr API at `/radarr/api/v3/`, and each has its own key.

### Saved passwords and keys

Ryokan never shows a saved password or key again. The field reads **[set; leave blank to keep]**.

- Leave it blank to keep the saved value.
- Type a new value to replace it.
- Press **Clear** to remove it.

If you change a service's address (host, port, or http to https), type the password or key again. Ryokan will not send a saved secret to a new address.

For a webhook, a saved header shows its name with `********` in place of the value, and a saved URL shows only its host.

## Download Clients

Add one or more of qBittorrent, Deluge, Transmission, rTorrent and SABnzbd. Setup notes for each are on the [Download clients](download-clients.md) page.

There is one **Default client** for torrents and one for Usenet. Grabs from Nyaa and torznab indexers go to the torrent default, and grabs from newznab indexers go to the Usenet default.

## Indexers

An indexer is a place Ryokan searches. Nyaa is built in and is the first card. Add torznab and newznab indexers (usually through Prowlarr or Jackett) and direct RSS feeds, such as SubsPlease, here.

Each indexer has two optional fields:

- **Categories**: leave blank and Ryokan asks for Anime (5070), plus Movies (2000) for a film and XXX (6000) for an adult title. If the indexer offers none of those, Ryokan asks for the categories it does offer. The RSS poll asks for every category your library needs. If you fill the field in, those category ids are sent on every search and RSS poll, comma separated. The edit form lists the categories the indexer offers. Click one to add or remove it.
- **Download client**: sends this indexer's grabs to a specific client, overriding the default. Use it when one tracker needs a client with its own seeding rules.

### Nyaa

The Nyaa card cannot be removed, but you can click it to:

- turn it off. Automatic and interactive searches then skip Nyaa and its RSS feed stops. The Search page keeps working.
- turn its RSS feed on or off
- choose between English-translated releases and all anime for automatic grabs
- send its grabs to a specific torrent client
- restrict searches to one uploader. A series can override this on its own page.

### autobrr webhook

[autobrr](https://autobrr.com) watches tracker announce channels and can push new releases to Ryokan at `/api/webhook/autobrr`. The webhook has its own API key, shown on this tab.

Name the indexer in Ryokan the same as autobrr's identifier for it, such as `animebytes`. A push from that indexer then uses its download client and seed rules. A push from `nyaa` uses the Nyaa card's client.

A torrent push from an indexer Ryokan does not know is still grabbed, through the default torrent client and without seed rules. A push that is not a torrent, such as a Usenet release, is skipped.

## Quality & Releases

These settings decide which release wins when several match an episode. [How releases are scored](scoring.md) explains how they combine.

- **Preferred / blocked groups**: releases from preferred groups score higher. Releases from blocked groups are never grabbed.
- **Preferred resolution / source**: releases below the resolution, or from the wrong source, are filtered out before scoring.
- **Cutoff source / resolution**: once an episode has a file at or above the cutoff, Ryokan stops looking for a better one.
- **Finished-series quality**: a second cutoff that applies once AniList marks the series as finished. A common setup is WEB while a show airs and BluRay afterwards.
- **Propers and Repacks**: what to do with a v2, PROPER or REPACK of a release you already have. See [Replacing a file you already have](scoring.md#replacing-a-file-you-already-have).
    - *Prefer and upgrade* (default): scores it higher, and replaces your file when it comes from the same group.
    - *Prefer, do not upgrade*: scores it higher only.
    - *Do not prefer*: ignores revisions.
- **Upgrade Until Custom Format Score**: a file that already meets the quality cutoff is still replaced by a release of the same quality with a higher Custom Format score, until the file's score reaches this number. The default, 0, turns this off for any file scoring 0 or more.
- **Minimum Custom Format Score Increment**: how much higher the new release has to score for that kind of upgrade. Default 1.
- **Audio preference**: subtitled, dubbed, or no preference. It changes scores but does not filter anything out.
- **SeaDex enabled**: gives a large score bonus to the releases [SeaDex](https://releases.moe) recommends. If you add a Custom Format that uses `SeaDexBest`, the Custom Format is used and this switch is ignored.
- **Default custom query tokens**: words added to every search, such as `bd 1080p`. A series can override them on its own page.

## Custom Formats

Custom Formats are [Sonarr-style](https://wiki.servarr.com/sonarr/settings#custom-formats) scoring rules. Each release is checked against every Custom Format and the matching scores are added up.

- **Custom Formats list**: add, edit and delete formats. Paste a release title into the test box to see which formats match and the total score. **Install Defaults** loads a set made for anime.
- **Minimum Score**: automatic searches ignore releases that score below this. They still appear in interactive search.
- **Import / Export**: Ryokan reads and writes Sonarr v4 Custom Format JSON, so you can paste formats from [TRaSH-Guides](https://trash-guides.info) or from Sonarr.

## Release Groups

A table of what each release group usually releases, for example that VCB-Studio releases are BluRay encodes. Ryokan uses it when a file name does not say whether the release is BluRay or WEB.

The table comes filled in. The tab also lists **Suggested Mappings** you can accept or edit. Rows you add win over the built-in ones.

## API Keys

Keys for other tools that connect to Ryokan. Each key has a name and one or more scopes, which decide what it may do. The key is shown once, when you create it.

- **calendar**: read the calendar subscription feed. The [Calendar](calendar.md) page builds the subscription URL for you.
- **admin**: everything.
- **search**, **library:read**, **library:write**: not used yet.

## General

### Library and imports

- **Media Root Path**: where imported episodes go. This is the path inside Ryokan's container. In the quick start, `/media/anime` is `/srv/media/anime` on the host.
- **Enable post-processing**: rename finished downloads, place them in the library, and write NFO files for Jellyfin. Needs a media root and a download client.
- **File operation mode**:
    - `hardlink` (default): the library file and the download are the same data on disk, so the torrent keeps seeding and no extra space is used. Falls back to a copy when the two folders are on different filesystems.
    - `copy`
    - `move`
- **Import subtitle files**: off by default. When on, subtitle files that come with a download are placed next to the episode and named after it, with the language from the subtitle's own name, for example `Show - S01E05 - Title.en.ass`. Deleting or replacing the episode removes them too. **Subtitle file extensions** sets which files count (`srt` and `ass` by default).

    A subtitle belongs to a video when one of these is true:

    - its name starts with the video's name
    - it has the same episode number
    - its folder is named after the video, as in a pack's `Subs/` layout

    If none of those match and the download has only one video, every subtitle goes to that episode. When a download has several subtitles of the same kind, they are numbered.

- **Give up on stuck imports after (hours)**: if a download has finished but Ryokan cannot find a video file to import after this long, the download is marked failed, you are notified, and the release goes on the blocklist under Downloads → Blocklist. Remove it from there to allow it to be grabbed again. Default 24. `0` never gives up. The usual cause is that Ryokan cannot see the download folder. Nothing is given up on in the first 15 minutes after a restart, so a download folder that mounts late is not mistaken for a stuck import.
- **Preferred Title Language**: `romaji`, `english` or `native`. Sets the title shown in the UI and used by the `{series.title}` naming token. Searches match all three whichever you pick.

### File naming

Three templates set where an import goes:

| Template | Default | Applies |
|---|---|---|
| Series folder | `{series.title}` | once, when the series is added |
| Season folder | `Season {season.number:00}` | on every import |
| Episode file | `{series.title} - S{season.number:00}E{episode.number:00} - {episode.title}{ext}` | on every import |

Each field shows a sample as you type, and **Reset** restores the default. Changing a template never renames folders or files already in the library.

The episode template must include `{episode.number}` and end with `{ext}`. Ryokan refuses a template if it cannot read the episode number back out of the sample name, because scans and upgrades rely on that.

| Token | Renders |
|---|---|
| `{series.title}` | Series title in your preferred title language |
| `{series.year}` | Premiere year |
| `{season.number}` | Season number, currently always 1. `{season.number:00}` pads to `01` |
| `{episode.number}` | Episode number. `:00` pads to `01`, `:000` to `001` |
| `{episode.absolute}` | Episode number counted across all seasons, as fansub groups number sequels: `019` for episode 7 of a second season that follows 12 episodes. `:000` pads |
| `{episode.title}` | Episode title |
| `{quality.full}` | Resolution and source, like `1080p WEB-DL` |
| `{quality.resolution}` | `1080p`, `720p`, ... |
| `{quality.source}` | `BluRay`, `BluRay Remux`, `WEB-DL`, `WEBRip`, `DVD`, `HDTV` |
| `{group}` | Release group |
| `{ext}` | File extension, including the dot |

Things to know:

- A token with no value is left out along with its brackets or separator, so `[{group}]` never leaves an empty `[]`.
- `{episode.absolute}` is empty for specials, and for a sequel until Ryokan has refreshed its metadata once. Put it after the episode number, as in `S01E07 - 019`, so scans read the right number.
- A file holding several episodes is named with the range: `S01E05-E06`, or `05-06` after a dash.
- Characters that file names cannot contain (`/ \ : * ? " < > |`) become `_`.
- A name that is too long for the filesystem is shortened at the series title. The episode number and extension are always kept.
- On Windows, a warning appears when the sample path is longer than 260 characters.
- Quality and group come from the release that was grabbed. A manual quality override on an episode is used instead.

### Backups

- **Scheduled backups**: off by default. Daily or weekly, Ryokan saves its database and encryption key, and cached artwork if you tick it, to the backup folder. Manual backups and restores are under [System → Backup](system.md#backup).
- **Backup folder**: a path inside Ryokan's container. Empty means a `backups` folder next to the database (`/data/backups` in Docker). A folder on another disk is safer.
- **Backups to keep**: older scheduled backups are deleted after each new one. Default 7. Backups taken automatically before a restore are never deleted.

!!! note
    Copying `ryokan.db` yourself while Ryokan is running can miss recent changes. The built-in backup does not have that problem.

### Recycle bin

- **Recycle bin path**: empty by default, which makes deletes permanent. Set a folder (a path inside the container) and deleted episodes, removed series, and files replaced by an upgrade are moved there instead. Restore or delete them for good from the Recycle Bin on the Library page.
- **Purge after (days)**: recycled items older than this are deleted. Default 14. `0` keeps them until you empty the bin.

Things to know:

- Put the bin on the same filesystem as the media root. The move is then instant and keeps hardlinks to seeding torrents.
- If Ryokan cannot write to the path, deletes are refused until you fix or clear it.
- Restoring puts the files back. It does not re-add a torrent that was removed from the download client.

### Searching and grabbing

- **Enable automatic RSS sync**: check RSS feeds on a schedule and grab matching releases. Each indexer also has its own RSS switch on the Indexers tab.
- **RSS Sync Interval (minutes)**: default 15, minimum 1, maximum 60.
- **Search for monitored episodes when a series is added**: on by default.
- **Search when monitoring is widened**: search for episodes that become monitored when you change a series' monitoring, for example from none to all. Off by default.
- **Add the series to the library when grabbing from Search**: on by default. When off, a grab from the Search page goes to the download client and Ryokan does not track the series.
- **Interactive file picker**: **Batches only** (default) opens the file picker when you grab a multi-file torrent. **Never** grabs everything with one click. Usenet releases always download whole.

### Failed downloads

- **Remove and blocklist detected misgrabs**: on by default. When the files in a download clearly belong to a different series, Ryokan removes the download, blocklists the release, notifies you and searches again. When off, the download stays in the client, is not imported, and is listed under System → Misgrabs.
- **Search again after a failed download**: on by default. When the download client reports a download as failed, Ryokan blocklists the release, removes the download, notifies you and searches for another. It stops after three failures for one series in a day.

A download the client only flags with a warning, such as qBittorrent's "missing files", is left alone and stays on the Downloads page.

Nothing is marked failed in the first 15 minutes after a restart.

A failed *import* is not retried either. The download itself is fine, so downloading it again would not help. Check the logs for what went wrong.

Each download client has a **Remove failed downloads** box (on by default) that decides whether a failed download is also removed from that client.

## Reset or wipe

- **Remove one series**: use **Remove from Library** on its page. You can delete its files at the same time.
- **Wipe everything**: stop Ryokan, delete its data folder (`/srv/docker/ryokan` in the [quick start](quick-start.md)), and start it again. First-run setup runs again.
- **Forgot your password**: see [Reset auth](docker.md#reset-auth). Your library and linked accounts are kept.

The **Force MAL / Kitsu fallback** switches are on the System page, under [Debug](system.md#debug).

---

*Last updated: 2026-10-04.*
