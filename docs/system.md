# System

The **System** page is Ryokan's operational view: logs, background-task health, recent RSS activity, episodes flagged for review, notification destinations, a link to these docs, and debug toggles. Settings (the things that change behavior) live under **Settings**; System is where you go to see what Ryokan has been doing or to flip a runtime toggle.

The page has a left sidebar with eleven entries (it collapses to a strip on narrow screens). Each gets its own section below.

## Logs

DB-backed log of everything Ryokan does, filterable by category, level, and free-text search. This is where most "why didn't this work?" questions get answered.

- **Category filter**: 19 categories, one per subsystem (Search, Grab, AutoSearch, AniList, DownloadClient, PostProcess, Quality, etc.). Pick the one matching what you were doing when the issue appeared.
- **Level filter**: trace / debug / info / warn / error. The DB-side floor is set by `RYOKAN_DB_LOG_LEVEL` (default `info`). Setting the filter to `trace` or `debug` won't surface entries Ryokan never persisted; bump the env var if you need that detail. See [Docker reference → Environment variables](docker.md#environment-variables).
- **Search box**: substring match against the message and detail columns. Useful for finding a specific release title, hash, or filename.
- **Older →** paginates backwards. Logs older than ~30 days are pruned by the `cleanup` background task.

For specific diagnostic walkthroughs, see [Troubleshooting](troubleshooting.md).

## RSS

Recent RSS poll history, one row per feed per tick. Shows item count pulled, latest item title, and the most recent error if a poll failed.

When to come here:

- A scheduled grab didn't fire and you want to confirm Ryokan saw the release on RSS.
- A feed silently broke (host moved, auth changed) and the configured indexer isn't reporting errors elsewhere.
- You want to see how often a feed actually surfaces new items before committing to it.

The RSS poll cadence is set in **Settings → General → RSS Sync Interval**. Manual sync is the **Sync RSS Now** button on this tab, or **Run now** on the `rss_sync` row under Scheduled Tasks.

## Scheduled Tasks

Status of Ryokan's background tasks: external_sync (watch-list sync), post_processing (move imported files into the library), grab_sweep (reconcile pending grabs against the download client's state), upgrade_search (look for better releases of already-grabbed episodes), library_classify, metadata_refresh, airing_refresh (refreshes the air times that show up on the [Calendar](calendar.md)), and a handful more.

Each row shows the schedule, whether the task is enabled, the last run's status and detail, when it last started and finished, and a **Run now** button.

When to come here:

- A feature feels "stuck" and you want to see if its background task is alive (running the loop) or wedged (crash-looping with restarts).
- After updating Ryokan, to confirm tasks resumed cleanly on the new image.
- A specific recurring action (watch-list sync, upgrade search) hasn't happened recently and you want to confirm timing.

## Import Library

A one-time wizard for anime you already have on disk: it walks a folder, matches each series on AniList, previews what would happen to every file, then imports. It has its own page: [Manual import](manual-import.md).

## Backup

Download a backup, keep scheduled ones in a folder, and restore from one.

A backup is a `.tar.gz` holding a consistent snapshot of the database (`ryokan.db`), the encryption key (`.ryokan-key`) that protects linked AniList and MyAnimeList tokens, a `manifest.json` with the Ryokan version and schema level, and, when you tick the option, the cached artwork. The snapshot is taken with SQLite's `VACUUM INTO`, so it is complete even while Ryokan is busy; copying `ryokan.db` by hand while Ryokan runs is not, because recent writes live in `ryokan.db-wal` until a checkpoint.

**A backup is a password export.** It contains the key, the encrypted account tokens, every download client password, and the activity log. Keep it where you keep secrets. For sharing with support, tick **Sanitize** instead: passwords, API keys, and tokens are blanked, the log is trimmed to its last 1000 lines, and the key and hostname stay out.

- **Download backup** builds the archive and sends it to the browser.
- **Save to backup folder** writes one to the folder from Settings → General, the same as a scheduled run, and prunes older ones past the retention count. The folder's contents are listed below the buttons with per-file Download and Delete.
- **Scheduled backups** (off by default) run daily or weekly from the same folder settings. They show up in [Scheduled Tasks](#scheduled-tasks) as `backup` with a Run now button.

**Restore** is two steps. Upload a backup: Ryokan checks that it is a Ryokan archive from this or an older version, saves a backup of the current state to the folder first (`auto-pre-restore-<time>.tar.gz`, never pruned), and stages the files. Then restart Ryokan. The staged files are swapped in before the database opens, the previous database stays next to the restored one as `ryokan.db.pre-restore-<time>` for a manual rollback, and everyone is signed out. Until the restart, the tab shows the staged backup with a **Cancel restore** button. A backup made by a newer Ryokan is refused. A sanitized backup restores but needs passwords and account links entered again.

The `ryokan.db.pre-restore-<time>` file (and `.ryokan-key.pre-restore-<time>` / `artwork.pre-restore-<time>` when those were replaced) is never cleaned up automatically. Delete it yourself once you are sure the restore is what you wanted. A sanitized download is named `ryokan-backup-<time>-sanitized.tar.gz` so it cannot be mistaken for the key-bearing kind.

Ryokan does not restart itself. In Docker, `docker compose restart ryokan`. Backups land under `/data/backups` by default there, on the same volume as the database, so point the folder at another disk or a mounted share if the goal is surviving that volume.

## Needs Review

Episodes the source classifier flagged as low-confidence, where the heuristics couldn't confidently decide BD vs. WEB or what release group it came from. Each row gives you the chance to manually accept the classifier's verdict, override it, or re-classify.

When to come here:

- After a big batch grab where some files used unusual naming conventions.
- After importing files from outside Ryokan (e.g. legacy library scan).
- Periodically, to keep your library's quality_tag accurate so upgrade-search behaves predictably.

This list is opt-in noise: each "needs review" entry is also written to the `Quality` log category, and notifications can fire on each one (off by default for a new provider, toggled per provider on **System → Notifications**, because reclassify sweeps can produce hundreds of entries at once).

## Misgrabs

Ryokan checks every download against the list of files the download client reports. When those files clearly name a different series than the one the release was grabbed for, the download is a misgrab. By default Ryokan removes it from the download client, adds the release to the blocklist so it is never grabbed again, sends a notification, and searches again for the episode it was supposed to fill.

The Misgrabs tab lists what was caught: the series, the release name, a sample of the file names inside it, when it was detected, and what happened to it. Each row has two actions.

- **Restore** says the release was right after all. Ryokan stops treating it as a misgrab for good, and if the download was removed it is added back to the download client.
- **Dismiss** confirms the misgrab. The release stays on the blocklist and the row leaves this tab. If the download was only flagged and is still in the client, it is removed now.

Downloads whose file names carry no title at all (for example `01.mkv` inside an unnamed folder) are never treated as misgrabs, and neither are files that share a word with the series title, so abbreviated fansub names are safe.

You can turn off automatic removal under Settings, General, "Remove and blocklist detected misgrabs". Ryokan then keeps the download in the client, never imports it, and lists it here as held until you restore or dismiss it.

## Notifications

CRUD UI for outbound notification destinations. Two provider kinds:

- **Webhook**: posts JSON to any HTTPS endpoint you configure (ntfy, Apprise, n8n, custom). Optional HMAC secret signs the body so receivers can verify it came from your Ryokan.
- **Discord**: posts an embed to a Discord webhook URL you provide.

Per-event opt-in matrix per provider: Grabbed, Imported, Import failed, Classifier needs review, Indexer down, Download client unreachable, Re-link required, Health (test).

When to come here:

- First-time setup of a Discord channel or a webhook receiver.
- Tweaking which events fire to which destination (you might want imports going to Discord but classifier-needs-review only going to a quiet ntfy channel).
- Sending a test event to confirm the destination is wired up correctly (the **Send test** button on each provider's modal).

The receiving side of `/api/webhook/autobrr` is *inbound*; that's a separate concept from these *outbound* notifications. The autobrr inbound webhook lives in **Settings → Indexers**.

## Docs

Opens this documentation site in a new tab. The scoring reference that used to live here is now [How releases are scored](scoring.md).

## Credits

Where Ryokan's data and releases come from (AniList, MyAnimeList through Tenrai, Kitsu, Nyaa, your indexers and feeds, SeaDex, anibridge-mappings, TRaSH Guides), the libraries it is built on, and the font it uses, each with a link and its license. The full list of Rust crates with their license texts is the third-party notices file in the repository, linked from this tab.

## Debug

Diagnostic switches and one-shot actions. The grabbing switches that used to live here (non-English releases, searching when a series is added) are under Settings, General, Grabbing.

- **Force MAL/Tenrai fallback for search and tracked fallback entries**: temporarily skip AniList and go straight to the MAL provider (Tenrai; any Jikan-v4-compatible API via `JIKAN_API_BASE`) for metadata fetches. Useful when AniList is rate-limited or returning stale data; flip back off after the issue clears.
- **Force Kitsu fallback**: same idea, for the Kitsu provider further down the metadata chain.

Toast feedback appears on this tab when a debug action succeeds or fails. Backup and Notifications toast too; the read-only tabs don't.

---

*Last updated: 2026-09-04.*
