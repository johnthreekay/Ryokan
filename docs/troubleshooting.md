# Troubleshooting

## Things to check first

Three places answer most questions:

- **System → Logs.** Filter by the category for whatever went wrong: Grab, AutoSearch, Nyaa, DownloadClient, PostProcess, AniList, and so on. See [Logs](system.md#logs).
- **Test connection** on each download client and indexer under Settings.
- **Grab History.** Click an episode on its series page. The history lists every release grabbed for it, with its state and time.

## Downloads

### SAB downloads disappear from Ryokan but still download in SAB

SABnzbd does not have the category Ryokan is set to use, so it put the download in its default category, where Ryokan does not look.

Click **Test connection** on the SABnzbd client under Settings → Download Clients. Ryokan creates the category and moves the download into it.

If the test reports `SAB rejected category creation: HTTP 403`, you gave Ryokan the NZB key. Use the full API key from SABnzbd's **Config → General → Security**.

### Episode shows "Importing…" forever

The download finished but the file has not been placed in your library.

- **Post-processing is off.** Turn it on under Settings → General. If it is off on purpose and the row still says "Importing…", reload the page.
- **The import is failing.** Check System → Logs, category `PostProcess`. The usual causes:
    - Ryokan cannot write to the media root
    - the disk is full
    - Ryokan cannot see the download folder at the path the client reports. See [Per-client download paths](download-clients.md#per-client-download-paths).

After 24 hours with nothing to import, Ryokan marks the download failed and puts the release on the blocklist. The limit is **Give up on stuck imports** under Settings → General. To try again, fix the cause, remove the release from Downloads → Blocklist, and search again. The download itself stays in the client, so delete it there if you no longer want it.

### Finished downloads stay in the client

Ryokan removes a download once it has been imported and has finished seeding. If one is still there:

- **Remove completed downloads is off** for that client. Edit the client under Settings → Download Clients.
- **It is still seeding.** A torrent is removed only after the client stops it at a ratio or time limit. Set a limit in the client, with pause or stop as the action, not delete. rTorrent needs a ratio group in `.rtorrent.rc`.
- **You paused it yourself.** Those are left alone.
- **The import failed or was partial.** Those stay so you can look at them.
- **It only just finished.** Ryokan checks every five minutes.

### Ryokan removed a download it decided was the wrong series

Ryokan compares the file names in each download with the series it was grabbed for. When they clearly belong to another show, it removes the download, blocklists the release and searches again. **System → Misgrabs** shows what was caught and the file names it looked at.

If the release was right, click **Restore**. Ryokan adds it back and does not flag it again.

If one series keeps producing misgrabs, groups probably name it differently from AniList. Add the name they use under **Advanced search overrides → Alternate titles** on the series page. Ryokan stops searching again automatically after three misgrabs for a series in a day.

To decide each case yourself, turn off **Remove and blocklist detected misgrabs** under Settings → General.

### Files ending in `.ryokan-tmp` or `.ryokan-new` in the library

These are left behind when an import is interrupted, for example by a power cut while a large file was being copied. Ryokan copies a file under the `.ryokan-tmp` name and renames it when it is whole, so an interrupted copy never leaves a half file under an episode's own name. Ryokan cleans them up hourly once they are more than two hours old. A newer one may be a copy in progress, so leave it.

A `.ryokan-new` file is a complete episode that was about to replace an older one. It goes to the recycle bin if you have one. To keep it, rename it in the season folder: remove the leading dot and the `.ryokan-new` ending.

## Searching

### Search returns no results

Check System → Logs, categories `Search` and `AutoSearch`. Common causes:

- **Your quality settings exclude everything on offer.** Loosen them under Settings → Quality & Releases, or use **Interactive search** on the episode to see everything.
- **Minimum Score** under Settings → Custom Formats is filtering releases out of automatic searches. Interactive search still shows them.
- **An indexer is down.** It simply adds nothing to the results. Run **Test** on each indexer.

### Auto search saw releases but grabbed none

Automatic search only takes a release whose name contains one of the series' titles exactly. When it skips releases that came close, the toast says "looked close but none named the series exactly" and lists some. System → Logs has them all.

If one is right, add that name on the series page under **Advanced search overrides → Alternate titles** and search again. See [What automatic search will and will not grab](scoring.md#what-automatic-search-will-and-will-not-grab).

### RSS skipped a release it used to grab

Since 2.3.0, RSS is stricter about releases that use a shortened title, such as "Ano Hana", or that tag a first season `S01` when the series' titles name no season. System → RSS lists such a release as "No tracked series match".

Add the name the release uses under **Advanced search overrides → Alternate titles** on the series page. The next sync takes it.

### An adult title finds nothing

Nyaa keeps adult releases on sukebei, which Ryokan's built-in search does not cover. You need an indexer that carries them, such as sukebei through Prowlarr or Jackett.

If a search is still empty, run it in Prowlarr or Jackett directly. If it finds releases there, check that the indexer is enabled in Ryokan and passes its test. You can also set the indexer's **Categories** field yourself under Settings → Indexers.

## AniList

### AniList keeps returning 429 Too Many Requests

AniList limits how many requests it accepts each minute. Open the latest failure in System → Logs. Its detail line looks like this:

```
[limit=30 remaining=0 reset=1777813117 retry_after=6 ryokan_60s=27]
```

`ryokan_60s` is how many requests Ryokan itself made in the last minute.

- **`ryokan_60s` is low**: something else used up the limit. This could be an anilist.co tab open in your browser, a second Ryokan using the same account, or another tool.
- **`ryokan_60s` is close to 30**: Ryokan sent too many. That is a bug, so please report it.
- **`no rate-limit headers`**: the 429 did not come from AniList's rate limit. It may be Cloudflare or a proxy in between.

### AniList per-account cooldown stuck past 60s

If syncs keep failing with 429 even though Ryokan is making very few requests, AniList has put your account in a longer cooldown. This can last hours.

1. Stop any other Ryokan using the same account.
2. Close any anilist.co tabs.
3. Do not press **Sync now**. Ryokan retries less and less often by itself.
4. Wait. The cooldown ends without you doing anything.

Linking the account again does not help, because linking counts against the same limit.

## Library

### Series page shows something out of date

The page updates download progress every 5 seconds. If something looks wrong, reload the page. If it is still wrong after a reload, open the episode's Grab History, which shows what Ryokan has recorded for it.

### I deleted an episode or series by accident

If you have a recycle bin set under Settings → General, open the Library page and click the recycle bin icon in the toolbar. Each deleted episode or series folder has a **Restore** button.

Restoring a series folder brings back the files but not the library entry. Add the series again and Ryokan finds the files.

Items are deleted for good after 14 days by default. If no recycle bin was set when you deleted, the files are gone.

## Startup

### Ryokan fails to start with a database error

The database file may have been damaged by an earlier crash. To check:

1. Stop Ryokan: `docker compose down ryokan`. Do not add `-v`.
2. Copy the database somewhere safe: `cp /srv/docker/ryokan/ryokan.db /srv/docker/ryokan/ryokan.db.backup`. Use your own data folder's path.
3. Run `sqlite3 /srv/docker/ryokan/ryokan.db "PRAGMA integrity_check;"`.

If it prints anything other than `ok`, the database is corrupt. Restore a backup, or delete the file and start again from first-run setup.

---

*Last updated: 2026-08-29.*
