# System

The **System** page shows what Ryokan has been doing: logs, RSS activity, background tasks, backups, and a few other tools. Settings that change how Ryokan behaves are under **Settings** instead.

## Logs

Everything Ryokan does is logged here. When something did not work, this is the first place to look.

- **Category**: one per part of Ryokan (Search, Grab, AniList, DownloadClient, PostProcess, and so on).
- **Level**: trace, debug, info, warn or error. Ryokan only stores `info` and above unless you lower `RYOKAN_DB_LOG_LEVEL`. See [Environment variables](docker.md#environment-variables).
- **Search**: matches text in the message and detail columns, so you can look for a release title or file name.
- **Live**: shows new entries as they arrive.

Entries are listed newest first, 200 to a page. Filters and the page number are kept in the address bar, so you can reload or share the link. Logs older than about 30 days are deleted.

[Troubleshooting](troubleshooting.md) covers what to look for in specific cases.

## RSS

The history of RSS checks, one row per feed each time it was checked: how many items came back, the newest title, and the error if the check failed.

Use it to see whether Ryokan saw a release you expected it to grab, or whether a feed has stopped working.

**Sync RSS Now** checks all feeds immediately. The schedule is set under **Settings → General → RSS Sync Interval**.

## Scheduled Tasks

Ryokan's background tasks, such as the watch-list sync, post-processing, the upgrade search, and metadata refresh. Each row shows the schedule, the result of the last run and when it ran, and a **Run now** button.

If a feature seems stuck, check here whether its task is running and what the last run reported.

## Import Library

A wizard for anime you already have on disk. See [Importing an existing library](manual-import.md).

## Backup

Create backups, keep them in a folder, and restore from one.

A backup is a `.tar.gz` containing the database, the encryption key that protects your linked AniList and MyAnimeList accounts, and the cached artwork if you tick that option.

!!! warning "A backup contains your passwords"
    It holds the encryption key, your account tokens, and every download client password. Store it as you would any secret.

    To send a backup to someone for support, tick **Sanitize**. Passwords, API keys and tokens are blanked, the log is cut to its last 1000 lines, and the encryption key is left out.

- **Download backup** builds a backup and downloads it in your browser.
- **Save to backup folder** writes one to the folder set under Settings → General and deletes old ones beyond the number you keep. The folder's backups are listed below, each with Download and Delete.
- **Scheduled backups** are off by default. Turn them on under Settings → General.

By default the backup folder is `/data/backups`, on the same volume as the database. Point it at another disk if the backup should survive losing that volume.

### Restoring

1. Upload a backup. Ryokan checks it, saves a backup of the current state to the backup folder (`auto-pre-restore-<time>.tar.gz`), and stages the restore.
2. Restart Ryokan. In Docker: `docker compose restart ryokan`. Ryokan does not restart itself.

Until you restart, the tab shows the staged backup and a **Cancel restore** button. After the restart everyone is signed out.

Things to know:

- A backup made by a newer version of Ryokan is refused.
- A sanitized backup restores, but you have to enter passwords and link accounts again.
- The previous database is kept next to the restored one as `ryokan.db.pre-restore-<time>`. Delete it once you are happy with the restore.

## Needs Review

Episodes where Ryokan was not sure of the source (BluRay or WEB) or the release group. For each one you can accept its guess or set the right value.

Keeping these correct matters for upgrades, since Ryokan compares new releases against the quality it thinks you have. Unusual file names and imported libraries produce most of the entries.

Notifications for these are off by default, because a reclassify run can produce hundreds at once.

## Misgrabs

A misgrab is a download whose files clearly belong to a different series than the one it was grabbed for. By default Ryokan removes it from the download client, blocklists the release, notifies you, and searches again.

This tab lists what was caught: the series, the release, some of its file names, and what happened to it.

- **Restore**: the release was right after all. Ryokan stops treating it as a misgrab and adds the download back if it was removed.
- **Dismiss**: it was a misgrab. The release stays blocklisted and the row is removed. If the download is still in the client, it is removed now.

Downloads whose file names contain no title, like `01.mkv`, are never treated as misgrabs. Neither are files that share a word with the series title, so abbreviated fansub names are safe.

To keep misgrabs in the download client until you decide, turn off **Remove and blocklist detected misgrabs** under Settings → General. They are then listed here and never imported.

## Notifications

Where Ryokan sends notifications.

- **Discord**: posts to a Discord webhook URL.
- **Webhook**: posts JSON to any URL, for ntfy, Apprise, n8n or your own receiver.
- **Custom script**: runs a program on the machine Ryokan runs on, for jobs like re-encoding a file once it is imported. See [Custom scripts](#custom-scripts).

For each one, choose which events it receives: Grabbed, Imported, Import failed, Classifier needs review, Indexer down, Download client unreachable, Re-link required. **Send test** sends a test event.

??? info "Verifying webhook signatures"
    Set a secret and Ryokan signs each request so the receiver can check where it came from.

    - `X-Ryokan-Signature` is `sha256=` followed by the HMAC-SHA256 of the body.
    - `X-Ryokan-Signature-V2` is `t=<timestamp>,sha256=<hex>`, computed over `<timestamp>.<body>`. Check this one and reject old timestamps to guard against replays.

The autobrr webhook is something else: it sends releases *to* Ryokan, and is set up under **Settings → Indexers**.

### Custom scripts

A custom script is a program Ryokan runs when an event happens, like Sonarr's Custom Script connection. Enter the full path to it and a time limit. A new script is set to run on **Imported** only; tick other events if you want them.

The script is run with no arguments, from its own folder. The event arrives as environment variables. After an import these are:

| Variable | Value |
|---|---|
| `ryokan_eventtype` | `Imported` |
| `ryokan_dest_path` | The file in your library |
| `ryokan_source_path` | The file it was imported from, in the download folder |
| `ryokan_series_title` | The series title |
| `ryokan_series_id` | Ryokan's id for the series |
| `ryokan_episode_number` | The episode number |
| `ryokan_quality_tag` | The quality Ryokan recorded for the episode |

Other events set `ryokan_eventtype` to their own name (`Grabbed`, `ImportFailed` and so on) and one variable per detail, named like the fields of the webhook's JSON with `ryokan_` in front. **Send test** runs the script once with `ryokan_eventtype` set to `Test` and nothing else, and shows its exit code.

```sh
#!/bin/sh
# A test run has no file to work on.
[ "$ryokan_eventtype" = "Imported" ] || exit 0
echo "Imported: $ryokan_dest_path" >> /config/imports.log
```

Things to know:

- The file has to be marked executable (`chmod +x`) and, for a script, start with a line such as `#!/bin/sh`.
- In Docker, the path is the one inside the container, and the script has to be on a volume the container can see.
- It can't be in a system folder such as `/usr/bin`, and it can't be a downloaded file: nothing in your media library, the recycle bin, a download client's download path, or a release Ryokan grabbed is run.
- Scripts run in the background, one at a time, so an import never waits for one. A script still running at its time limit is stopped, along with anything it started.
- Disabling or deleting a script also drops its runs that are still waiting, and a new path applies to them.
- Stopping Ryokan stops a script that is running and drops the runs still waiting. They are not picked up again after a restart.
- Each run is recorded in **System → Logs** under Notifications, with its exit code and the end of anything it wrote to stderr.
- A script made for Sonarr reads Sonarr's variable names, so it will not work as it is. To hand the file to a tool such as sickbeard_mp4_automator, call the tool's manual script from your own with `$ryokan_dest_path`.

!!! note "Docker"
    The Ryokan image has no Python, ffmpeg or other tools in it. If your script needs them, build your own image on top of Ryokan's:

    ```dockerfile
    FROM ghcr.io/johnthreekay/ryokan:latest
    RUN apt-get update \
        && apt-get install -y --no-install-recommends python3 ffmpeg \
        && rm -rf /var/lib/apt/lists/*
    ```

A custom script runs with the same access to your files as Ryokan itself, and anyone who can log in to Ryokan can add one. To turn the feature off for the whole install, set the environment variable `RYOKAN_CUSTOM_SCRIPTS=0`. Saved scripts are kept, but none is run and none can be added.

## Docs

Opens this site.

## Credits

The services, data sources, libraries and fonts Ryokan uses, with their licenses.

## Debug

- **Force MAL/Tenrai fallback**: skip AniList and fetch metadata from MyAnimeList. Use it while AniList is rate-limiting you or down, and turn it off afterwards.
- **Force Kitsu fallback**: the same, using Kitsu.

---

*Last updated: 2026-09-04.*
