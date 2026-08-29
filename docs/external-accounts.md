# External accounts

Link your AniList or MyAnimeList account and Ryokan will pull anime you mark as watching (or planning, completed, etc.) into your library automatically. New entries on the linked side become new series in Ryokan on the next sync tick; status changes propagate.

You don't have to link anything. Manually-added series work fine without it. But if you already track anime on AL or MAL, linking saves typing.

Configure under **Settings → Connections** (the External Accounts card near the bottom of the tab).

## Linking AniList

1. In Ryokan: **Settings → Connections → External Accounts → Link AniList**.
2. Ryokan opens AniList's authorization page in a new browser tab via the `/start` endpoint.
3. Sign in to AniList if you aren't already, click **Approve**.
4. AniList redirects you to a broker page hosted alongside Ryokan's docs at `johnthreekay.github.io/Ryokan/auth/anilist/`. The broker exists because AniList's OAuth requires a static redirect URL, but every Ryokan instance is at a different address; the broker reflects the token back to your clipboard so you can paste it into your own Ryokan.
5. Copy the access token + state from the broker page and paste them into Ryokan's paste modal.

The token lasts about a year. No refresh flow; you'll re-link annually. Ryokan reads your AniList score format (POINT_10, POINT_100, etc.) on every sync, so flipping the format on AniList takes effect on the next tick without unlinking.

## Linking MyAnimeList

Same shape as AniList: Ryokan opens MAL's authorization page, you approve, the broker page reflects the token back, you paste it into Ryokan.

The difference under the hood is that MAL uses a more involved OAuth flow with a "refresh token" kept alongside the access token. Practical effect: when MAL's access token expires (every ~30 days), Ryokan refreshes it automatically using the refresh token without making you re-link. You only need to re-link if MAL invalidates the refresh token itself, which usually means you revoked the app or changed your MAL password.

## Watch-list sync

The watch-list sync runs as a background task every `external_sync_interval_minutes` (default 30, minimum 15). Each tick:

1. Asks AniList or MAL for entries that changed since the last successful sync.
2. Filters by your import preferences (Watching, Planning, Paused, Dropped, Completed).
3. Pre-fetches AniList metadata for new ids in one batch so the import is fast.
4. Adds new series to your library, updates monitor mode (Watching → All / Future / Cutoff) on existing ones based on the linked-side status.
5. Once the last full re-fetch is more than 7 days old, does a full re-fetch instead of an incremental delta to catch removals; AniList and MAL don't expose a "this entry was deleted" signal, so the only way to detect a removal is to re-list the whole thing and notice the missing id.

The "Sync now" button on the External Accounts card forces an immediate tick instead of waiting for the next scheduled one.

## Failure modes

- **Token expired or revoked**: AniList responds with a GraphQL error mentioning `"token"`, or with HTTP 401. MAL returns 401 too. Ryokan logs the failure and surfaces "user may need to re-link" in System → Logs (filtered to ExternalSync category). The sync's exponential backoff defers retries so the failed sync doesn't hammer the server.
- **Rate limited**: AniList caps at 30 requests per minute in degraded mode (the current state). The sync stops short of the cap rather than firing into a 429. See [Troubleshooting → AniList rate limits](troubleshooting.md#anilist-keeps-returning-429-too-many-requests).
- **Token rejected on link** (the most common 400 on link/submit): the validation probe Ryokan uses to confirm the freshly-pasted token works shares the same per-account quota as syncing. If your account is in a rate-limit cooldown when you try to link, the link fails with the same 429. See [Troubleshooting → Per-account AniList cooldown](troubleshooting.md#anilist-per-account-cooldown-stuck-past-60s).

## Provider order is fixed for the Sonarr / Radarr shim

Ryokan's Sonarr/Radarr-compatible API (anibridge) does AniList lookups first, MAL lookups second when AniList is down. There's no user-facing toggle for this and no plan to add one. Seerr expects stable provider behavior; falling back inconsistently confuses its caching.

---

*Last updated: 2026-08-29.*
