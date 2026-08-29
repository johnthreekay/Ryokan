# External accounts

Link your AniList or MyAnimeList account and Ryokan adds the anime you mark as watching (or planning, completed, and so on) to your library for you. New entries on the linked side show up in Ryokan on the next sync, and status changes carry over.

Both accounts are set up under **Settings → Connections**.

## Linking AniList

1. In Ryokan, go to **Settings → Connections** and click **Link AniList**.
2. AniList opens in a new tab. Sign in if you need to, then click **Approve**.
3. AniList sends you to a page that shows your token. Copy it.
4. Back in Ryokan, paste the token into the box that opened and confirm.

The link lasts about a year; after that Ryokan asks you to link again. Your AniList score format (10-point, 100-point, and so on) is picked up automatically, so changing it on AniList needs no re-link.

## Linking MyAnimeList

Same steps: click **Link MyAnimeList**, approve on MyAnimeList, copy the token from the page it sends you to, and paste it into Ryokan.

MyAnimeList links renew themselves in the background. You only need to link again if you remove Ryokan from your MyAnimeList apps or change your MyAnimeList password.

## Watch-list sync

Once an account is linked, Ryokan checks it every 30 minutes. **Sync interval (minutes)** in the same settings section changes that, down to every 15 minutes. Each check:

- picks up anime you added or moved between lists, limited to the lists you ticked (Watching, Planning, Paused, Dropped, Completed),
- adds new series to your library and adjusts monitoring on existing ones to match their status on the linked side,
- about once a week, re-reads the whole list so anime you removed are noticed too.

**Sync now** next to the linked account runs a check right away.

## Common issues

- **Re-link required**: the account's access has expired or been revoked. Click **Link** again. Until you do, Ryokan retries at a slower pace and logs the problem under **System → Logs** (category External Sync).
- **AniList rate limit**: AniList allows a limited number of requests per minute, and Ryokan stops short of that limit on its own. If syncs look stalled, see [Troubleshooting](troubleshooting.md#anilist-keeps-returning-429-too-many-requests).
- **Linking fails right after pasting the token**: linking uses the same AniList allowance as syncing. If your account is in a cooldown, wait a minute and try again. See [Troubleshooting](troubleshooting.md#anilist-per-account-cooldown-stuck-past-60s).

---

*Last updated: 2026-08-29.*
