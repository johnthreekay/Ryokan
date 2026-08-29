# FAQ

## How is this different from Sonarr?

Sonarr was built for general-purpose TV; its anime support is real but bolted on. Ryokan is anime-only and tunes its release-classification logic, search shape, and metadata for that. The concrete differences:

- **Batch releases are a first-class search target, not a fan-out.** Sonarr's anime mode searches every episode individually, which multiplies search time and indexer hits with every episode and indexer (and, in our experience, times out before the results aggregate). Ryokan sends one search per batch.
- **Anime release-name parsing is the default-handled path, not an edge case.** [Anitomy](https://github.com/erengy/anitomy) (a parser specifically for anime release titles) handles tokenization; Custom Formats can match against anitomy fields directly; AniList is the primary metadata source.
- **[SeaDex](https://releases.moe) is integrated as an authoritative-pick layer.** Sonarr has nothing equivalent.
- **Source classification combines multiple signals** (filename, Nyaa description scrape, release-group reputation table, ffprobe output, directory walk, and a temporal "is this still airing?" heuristic) before deciding whether a release is BD or WEB. Sonarr decides quality from the release title plus Custom Formats; for anime, that misclassifies often enough that the manual-review pile stays large.

## Is this a fork of Sonarr?

No. Different language (Rust vs C#), different architecture, different release ecosystem. Some conventions are deliberately Sonarr / Radarr-shaped (Custom Format JSON format, source taxonomy, the API shape Seerr expects) so TRaSH-Guides presets and Seerr-style request frontends Just Work, but the codebase is independent.

## Can it manage manga, light novels, or webtoons?

Not yet. Ryokan only manages anime in the current 1.x line. Manga, light novels, and similar long-form formats need different metadata sources, different naming conventions, and different file-organization patterns; supporting them properly is the focus of v2.0 rather than something to graft on partially.

## Can I run multiple Ryokan instances?

Technically yes, but they don't coordinate. Each instance has its own database, its own grab history, its own AniList / MAL link state. Two instances pointed at the same AniList account share that account's per-token rate-limit budget (which you'll trip), so this isn't a "scale horizontally" pattern; it's "two completely separate libraries that happen to share metadata sources".

If you need shared library state across machines, stick to one instance and put it behind a reverse proxy or VPN-mesh so both machines can reach it. See [Troubleshooting → AniList per-account cooldown](troubleshooting.md#anilist-per-account-cooldown-stuck-past-60s) for the rate-limit angle.

## Is Ryokan multi-user?

Not supported and not on the roadmap. Single-admin only. The reasoning matches Sonarr's [PR #7186](https://github.com/Sonarr/Sonarr/pull/7186) rejection (Jan 2025): private-tracker account-sharing semantics get messy fast, and the "PVR shared with friends" use case is well-served by Jellyfin sitting on top of a single-admin PVR. Jellyfin already handles per-user libraries, watch progress, parental controls, etc.

## Can I use the API directly?

Yes. Ryokan exposes a Swagger UI at `/api-docs` and the OpenAPI JSON at `/api-docs/openapi.json`. Two auth paths:

- **Cookie auth** for the web-UI-facing endpoints (whatever the browser uses; you log in with username + password).
- **API-key auth** for the Sonarr / Radarr-compatible shim that Seerr and friends call (`X-Api-Key` header or `?apikey=` query string, configured in **Settings → Connections**).
- **Per-tool API keys** for narrower jobs. Right now the calendar subscription feed at `/api/calendar.ics` is the main one. Create a key with the `calendar` permission on **Settings → API Keys** and the [Calendar](calendar.md) page builds the subscription URL for you.

## How do I back up?

Ryokan has a built-in backup on [System → Backup](system.md#backup): download a snapshot, save one to the backup folder, schedule daily or weekly backups under Settings → General, and restore by uploading an archive. That is the supported path; a plain file copy taken while Ryokan is running can miss writes.

If you would rather copy the volume yourself, stop Ryokan first. The whole `/data` volume captures everything: the SQLite database, the artwork cache, the encryption key, the OAuth tokens (encrypted at rest), and config sentinels. Standard SQLite backup tools work, or just stop Ryokan and copy the volume.

The encryption key is the load-bearing bit. Lose it but keep the database, and every encrypted OAuth token in `external_accounts` becomes unrecoverable; you'll need to re-link those accounts. The key lives at `/data/.ryokan-key` by default; back it up alongside the DB.

If you're using the [quick start's](quick-start.md) `/srv/docker/ryokan` bind-mount layout, the whole folder is your backup target. With Docker named volumes it's under `/var/lib/docker/volumes/<volume-name>/`; same idea, less convenient.

---

*Last updated: 2026-08-29.*
