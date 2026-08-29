# System

The **System** page is Ryokan's operational view: logs, background-task health, recent RSS activity, episodes flagged for review, notification destinations, scoring reference, and debug toggles. Settings (the things that change behavior) live under **Settings**; System is where you go to see what Ryokan has been doing or to flip a runtime toggle.

The page has a left sidebar with ten entries (it collapses to a strip on narrow screens). Each gets its own section below.

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

## Scoring

Read-only reference. Shows the scoring weights Ryokan uses (seeders, preferred-group order, resolution match, batch bonus, dual-audio penalty/bonus, etc.) so you can predict why one release outranked another.

This page doesn't change behavior; the actual scoring inputs (preferred groups, resolution / source profile, custom formats) are configured in **Settings → Preferred Quality & Releases** and **Settings → Custom Formats** ([Configuration](configuration.md) explains those tabs).

## Credits

Project credits and third-party license attributions. Useful when you want to know which crates Ryokan ships with or want to see the upstream URL for a specific dependency.

The full third-party license texts are bundled into the binary at compile time and surfaced from this tab.

## Debug

Runtime toggles and diagnostic actions that don't fit cleanly under Settings.

- **Allow non-English releases**: when off, Nyaa search restricts to category `1_2` (English-translated). When on, Ryokan also pulls from category `1_0` (Anime All; includes untranslated and multi-sub releases). Music releases always search categories `1_1` + `2_0` regardless.
- **Force MAL/Tenrai fallback for search and tracked fallback entries**: temporarily skip AniList and go straight to the MAL provider (Tenrai; any Jikan-v4-compatible API via `JIKAN_API_BASE`) for metadata fetches. Useful when AniList is rate-limited or returning stale data; flip back off after the issue clears.
- **Force Kitsu fallback**: same idea, for the Kitsu provider further down the metadata chain.
- **Auto-grab monitored episodes when adding a new series**: when on, Ryokan searches for and grabs every monitored episode right after you add a series. When off, you trigger searches yourself.

Toast feedback appears on this tab when a debug action succeeds or fails. Backup and Notifications toast too; the read-only tabs don't.

---

*Last updated: 2026-08-29.*
