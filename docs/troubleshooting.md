# Troubleshooting

Concrete diagnostic steps for the most common things that break. If the symptom isn't covered here, **System → Logs** ([System → Logs reference](system.md#logs)) is usually the next stop; almost every Ryokan operation writes a log line, filterable by subsystem.

## Things to check first

Before drilling into a specific symptom below, three quick checks resolve most issues:

- **System → Logs**, filtered by category to whatever subsystem you suspect (AniList, Jikan, Kitsu, Grab, AutoSearch, Nyaa, DownloadClient, Jellyfin, PostProcess, etc.). Pick the one matching what you were doing when the issue appeared.
- **Test connection** on each download-client row (Settings → Download Clients) and each indexer row (Settings → Indexers). Connection tests catch most config issues at config time rather than at grab time. The [Download clients](download-clients.md) page lists per-client gotchas.
- **The grab-history modal** on each episode (click the episode on its series page, then the **Grab History** section of the episode modal) shows every release ever grabbed for that episode, with state (`grabbed` / `completed` / `failed` / `removed` / `replaced`) and timestamp. Useful for "why is this episode in this state?" questions.

## SAB downloads disappear from Ryokan but still download in SAB

You'll see this in System → Logs as a debug-level line:

```
sab list_scoped: dropped every slot via category filter — configured_category="anime" queue_slots=0 history_slots=1 seen_categories={"default"}
```

What happened: SAB doesn't have the category Ryokan was configured to use. It accepted the NZB but landed it in the default bucket. Ryokan's `list_scoped` filters by category to avoid accidentally treating other tools' jobs as its own, so the job becomes invisible.

**Fix**: click Test connection on the SAB row in Settings → Download Clients. The auto-create path will create the missing category and re-tag the just-added job. After that, future grabs land correctly.

If Test connection shows `(warning: SAB rejected category creation: HTTP 403 ...)`, your SAB API key is the read-only `nzb_api_key`. Use the **full API Key** from SAB → Config → General → Security → API Key.

## Cancel Pending doesn't actually remove the SAB job

Was a real bug as of 1.4.x; fixed in 1.5.x. Update Ryokan and try again. The bug was that the SAB delete code tried `mode=history&name=delete` first, which phantom-succeeds on unknown nzo_ids; for an in-flight grab (still in queue, not history), the delete would claim success without actually touching the queue. Queue-first ordering fixes it.

## AniList keeps returning 429 Too Many Requests

Open the most recent failure in System → Logs. The detail line now carries diagnostic headers:

```
[limit=30 remaining=0 reset=1777813117 retry_after=6 ryokan_60s=27]
```

- **`remaining=0` AND `ryokan_60s` close to 30** → Ryokan over-fired. This shouldn't happen with the rate-limit clamp, but if it does, file an issue.
- **`remaining=0` AND `ryokan_60s` low** → the budget was burned outside Ryokan-this-process. Candidates: another tab on anilist.co (each profile-page render makes many GraphQL calls), a second Ryokan instance pointed at the same AL account, an extension or helper tool.
- **`no rate-limit headers`** → the 429 came from somewhere other than AL's normal rate-limiter (Cloudflare, an upstream proxy, AL's auth layer misusing 429 for token issues).

AL doesn't document a per-token quota, but in practice authenticated calls (`MediaListCollection`, `Viewer`) seem to have one separate from the global per-IP cap; unauthenticated search can succeed while authenticated calls 429.

## AniList per-account cooldown stuck past 60s

If `external_sync` keeps 429ing despite `ryokan_60s=1` and minutes between attempts, AL has likely flagged your account for an extended cooldown. The documented window is 60s rolling but the (undocumented) burst limiter can hold an account for hours.

**What to do**:

1. Stop any other Ryokan instance you might have running (`pgrep -fa ryokan`, `docker ps`, `systemctl status`).
2. Close any anilist.co tabs in your browser.
3. Don't fire manual Sync Now / Search Missing; let the supervised loop's exponential backoff carry you (15 min × 2^errors, capped after 5 errors at ~8h between attempts).
4. Wait. The cooldown clears on AL's end with no action from your side.

Re-linking the account during a cooldown won't help; the OAuth submit handler validates the new token via a `Viewer` probe that hits the same per-account quota. You'll get a "Link failed" with the same 429 in the surfaced error message.

## Episode shows "Importing…" forever

The poller saw the torrent reach 100% but the post-processing tick hasn't moved the file into the library yet. Two real causes:

- **Post-processing is disabled** in Settings → General. Ryokan correctly leaves the file at the download client's path; the row shouldn't be showing "Importing…" in this state. If it is, force-refresh the page (Ctrl+Shift+R); there's a known race where the per-row state can lag the global toggle.
- **Post-processing is on but the import is failing.** Check System → Logs filtered to `PostProcess`. Common causes: `media_root` isn't writable by the runtime user, the media filesystem is full, or Ryokan can't see the download client's complete path (per-client `download_path` mismatch; see [Download clients → Per-client download paths](download-clients.md#per-client-download-paths)).

## Series-page state is stale

Most live-state surfaces (download progress bars, season-size badge, modal-footer buttons) update via a 5s poller. If something looks wrong:

1. **Refresh the page** (F5). The server-rendered page is the ground truth; if refresh fixes it, it's a JS-side staleness bug worth filing.
2. If refresh *doesn't* fix it, the underlying DB state is what you're seeing. Check the grab-history modal for an authoritative view of that episode's grab state.

## Search returns no results

Check System → Logs filtered to `Search` and `AutoSearch`. The most common causes:

- **Profile mismatch**: your active quality profile doesn't accept any of the released qualities. Try widening the profile (Settings → Preferred Quality & Releases) or use the Interactive Search button on the episode for a one-off relaxed search.
- **Custom Format scoring threshold**: if `custom_format_minimum_score` is set (Settings → Custom Formats), releases scoring below it are silently dropped from auto-search candidates. They still show up in interactive search.
- **Indexer down**: an unreachable torznab indexer doesn't fail-fast; it just contributes nothing to the merged result set. Test connection on each indexer to verify.

## An adult title finds nothing even with an indexer configured

Nyaa keeps adult releases on sukebei, which Ryokan does not search, so an adult title (marked 18+ on its series page) depends on an indexer that carries them, such as sukebei through Prowlarr or Jackett. Indexers file those releases under the adult category rather than anime, and since 1.9.2 Ryokan asks for both categories whenever the title is adult. Movies ask for the Movies category too, since trackers disagree on where anime films go. Ryokan also never asks an indexer for a category it does not report, and the indexer's **Categories** field under Settings → Indexers overrides all of this when you know better. If a search still comes back empty, run the same search in Prowlarr or Jackett: if it finds releases there, check that the indexer is enabled under **Settings → Indexers** and that its test passes.

## Migrations failing on first boot

Ryokan's migrations are idempotent by design (each `ALTER TABLE … ADD COLUMN` swallows already-exists errors), so applying twice is a no-op. The thing that wedges them is a corrupt SQLite file from an earlier crash. To check:

1. Stop Ryokan: `docker compose down ryokan` (no `-v`).
2. Back up the DB: `cp /srv/docker/ryokan/ryokan.db /srv/docker/ryokan/ryokan.db.backup` (path is wherever you put `/data` in your compose).
3. Check integrity: `sqlite3 /srv/docker/ryokan/ryokan.db "PRAGMA integrity_check;"`. If it returns `ok`, the DB itself is fine and the migration error is something else (network during migration? unusual). If it returns anything other than `ok`, the DB is corrupt; restore from your backup or accept losing the DB and starting fresh (delete the file, restart Ryokan, the first-run setup runs again).

## Auto search saw releases but grabbed none

Automatic search only takes a release whose name contains one of the series' titles or synonyms as written and names nothing beyond it, so a sequel titled by subtitle ("Dr. Stone New World") is not mistaken for the first season. When the only releases on offer name the show some other way, the search toast reads "looked close but none named the series exactly" and lists examples, and System → Logs has the full list. If one of those is the right show, add that name on the series page under **Advanced search overrides**, **Alternate titles**, and search again. See [How releases are scored](scoring.md#what-automatic-search-will-and-will-not-grab).

## Ryokan removed a download it decided was the wrong series

Ryokan compares the files inside every download with the series it was grabbed for. When the file names clearly belong to a different show, it removes the download, blocklists the release, and searches again. Open System, Misgrabs to see what was caught and why: the row shows the file names Ryokan looked at.

If the release was actually correct, click **Restore**. Ryokan adds it back to the download client and never flags that release again. If a series keeps producing misgrabs, its AniList titles probably do not match how groups name it; check the series page's title and any synonyms, and check which indexers you have configured, since any-word search on some indexers returns unrelated releases.

Ryokan stops searching again automatically after three misgrabs for the same series in a day. Fix the cause, then search from the series page.

If you would rather decide yourself, turn off "Remove and blocklist detected misgrabs" under Settings, General. Downloads are then held in the client and listed on the Misgrabs tab until you restore or dismiss them.

## I deleted an episode or series by accident

If a recycle bin path is configured under Settings → General, nothing is gone yet: open the Library page and click the recycle-bin icon in the toolbar (it appears once the bin has items), or go to `/library/recycle`. Each deleted episode (with its NFO, subtitles, and thumbnail) or series folder is listed by the day it was deleted with a **Restore** button that puts it back exactly where it came from. Restoring a series folder brings the files back but not the library entry. Re-add the series from Search afterwards and Ryokan will pick the files up on disk. Entries purge automatically after the configured number of days (14 by default), so restore before then. If the path was empty at the time of the delete, the files were removed permanently. If the path was set but not writable, the delete was refused and the file is still where it was.

---

*Last updated: 2026-08-29.*
