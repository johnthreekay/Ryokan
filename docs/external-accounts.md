# External accounts

Link your AniList or MyAnimeList account and Ryokan adds the anime on your lists to your library, and keeps up with changes you make there.

Accounts are linked under **Settings → Connections**.

## Linking AniList

1. In Ryokan, go to **Settings → Connections** and click **Link AniList**.
2. AniList opens in a new tab. Sign in if you need to, then click **Approve**.
3. Return to Ryokan. The account is linked and you can close the AniList tab.

If the new tab was blocked, allow pop-ups for Ryokan and click **Link AniList** again. If the tab opened but Ryokan still shows the linking box, copy the token and state from the page AniList sent you to and paste them into that box.

An AniList link lasts about a year. After that Ryokan asks you to link again.

## Linking MyAnimeList

The steps are the same: click **Link MyAnimeList**, approve, and return to Ryokan.

A MyAnimeList link renews itself. You only have to link again if you remove Ryokan from your MyAnimeList apps or change your MyAnimeList password.

## Watch-list sync

Ryokan checks the linked account every 30 minutes. Change that with **Sync interval (minutes)**. The minimum is 15.

Tick the lists you want synced: Watching, Planning, Paused, Dropped, Completed. On each check, Ryokan adds series that are new on those lists and updates monitoring for series whose status changed. About once a week it reads the whole list again, which is when it notices anime you removed.

**Sync now** runs a check immediately.

## Keeping a removed series off the sync

Removing a series from Ryokan does not remove it from your AniList or MyAnimeList list, so the next sync adds it back.

To prevent that, tick **Keep it off my … sync** when you remove the series. It goes on the **Sync exclusions** list under Settings → Connections, and syncs skip it until you press **Allow again** there.

## One account at a time

Only one account can be linked at a time. While it is linked, your score for each series is shown on its library card.

**Unlink** removes the link and the scores that came from it. The series it added stay in your library.

## Common issues

- **Re-link required**: the link has expired or been revoked. Click **Link** again.
- **Syncs seem stalled**: AniList limits how many requests Ryokan may make each minute. See [Troubleshooting](troubleshooting.md#anilist-keeps-returning-429-too-many-requests).
- **Linking fails right after you paste the token**: linking counts against the same AniList limit. Wait a minute and try again. See [Troubleshooting](troubleshooting.md#anilist-per-account-cooldown-stuck-past-60s).
- **One MyAnimeList entry covers several series**: AniList sometimes splits one MyAnimeList entry into parts, such as the stages of Steel Ball Run. A status change on MyAnimeList applies to every part in your library. The sync does not add parts you do not have.

---

*Last updated: 2026-10-26.*
