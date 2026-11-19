# FAQ

## How is this different from Sonarr?

Sonarr is built for TV in general and supports anime as a special case. Ryokan only does anime. In practice:

- **Batches.** Ryokan searches for a season pack with one search. Sonarr's anime mode searches every episode separately.
- **Release names.** Ryokan reads fansub and BluRay-pack naming, absolute episode numbers and revisions (v2) by default.
- **Metadata.** Ryokan uses AniList, so seasons and titles match how anime is listed.
- **[SeaDex](https://releases.moe).** Ryokan can prefer the releases SeaDex recommends.
- **BluRay or WEB.** Ryokan looks at more than the release title to tell them apart, including the release group, the file itself, and whether the show is still airing.

## Is this a fork of Sonarr?

No. Ryokan is written from scratch in Rust. It follows some Sonarr conventions on purpose, so that TRaSH-Guides Custom Formats and Seerr work with it.

## Can it manage manga, light novels, or webtoons?

No, only anime. They may come later.

## Can I run multiple Ryokan instances?

Yes, but they know nothing about each other. Each has its own database and library. If two use the same AniList account, they share its rate limit and will hit it. See [Troubleshooting](troubleshooting.md#anilist-per-account-cooldown-stuck-past-60s).

To use one library from several machines, run one instance and reach it over your network.

## Is Ryokan multi-user?

No, and there are no plans for it. Ryokan has one account. To share your library with other people, use Jellyfin, which has its own users, watch progress and parental controls.

## Can I use the API directly?

Yes. The API documentation is at `/api-docs`, and the OpenAPI file at `/api-docs/openapi.json`. Both need you to be logged in.

There are three ways to authenticate:

- **Your login session**, for the same endpoints the web UI uses.
- **The Sonarr / Radarr API keys** from **Settings → Connections**, for the endpoints Seerr calls. Send the key in the `X-Api-Key` header or as `?apikey=`.
- **Keys from Settings → API Keys**, for specific features. Currently that is the calendar feed at `/api/calendar.ics`. See [Calendar](calendar.md).

## Does Ryokan handle a file that holds two episodes?

Yes. A release like `Title - 05-06` or `S01E05-E06` is imported as one file named with the range, and both episodes count as present. Deleting the file removes both.

Such a file is only replaced by a release that covers both episodes.

## Where do OVAs and specials go?

A special that comes inside a batch for a TV series, such as `Show - OVA 01` or `Show - SP1`, goes into a `Specials` folder in the series folder and is named `S00E01` style. Jellyfin, Plex and Kodi all recognise that. It does not appear in the episode list.

Automatic search and RSS do not grab specials for a TV series. Use interactive search when you want one.

A series that is itself an OVA on AniList is treated like any other series.

## Can a .strm file stand in for an episode?

Yes. A `.strm` file is a small text file containing a link to a stream, which Jellyfin, Emby and Kodi play in place of a video. Ryokan counts a `.strm` in the series folder as that episode, so the episode shows as present and is not searched for. Import Library picks them up too.

Ryokan never opens the link. The series page shows **Stream link** where the file size would be.

An upgrade replaces a `.strm` like any other file. To prevent that, turn off **Upgrades** on the series page.

## How do I back up?

Use [System → Backup](system.md#backup). You can download a backup, save one to a folder, schedule them, and restore from one.

To back up the files yourself, stop Ryokan first and copy the whole data folder: `/srv/docker/ryokan` in the [quick start](quick-start.md), or the Docker volume otherwise. A copy taken while Ryokan is running can miss recent changes.

The data folder contains a hidden `.ryokan-key` file. Keep it with the database. Without it you have to link your AniList and MyAnimeList accounts again.

---

*Last updated: 2026-08-29.*
