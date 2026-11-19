# How releases are scored

Ryokan gives every release it finds a score and grabs the one with the highest. The score is a sum of parts.

To see the parts for a release, open a series page, click **Interactive search** on an episode, and expand the score next to the release. The names in the tables below are the ones shown there.

## Release signals

These apply to every search.

| Part | Points |
|---|---|
| Seeders | +30 over 100, +25 over 50, +20 over 10, +10 for any, -10 for none |
| Preferred Group | +140 for the first group on your preferred list, 20 less for each place further down |
| Non-Preferred Group | -15 for a group that is not on your list |
| No Group Tag | -10 for a release that names no group, when you have a preferred list |
| Preferred Resolution | +20 when the release is the resolution you prefer |
| Batch Release | +15 |
| Compact Batch | +10 more for a batch under 25 GiB |
| Trusted Uploader | +10 for a Nyaa release from a trusted uploader |
| Encoding / Source Quality | +5 for 10-bit, x265, HEVC, or BluRay in the title |
| Dub / Dual Audio | -15 for a dub or dual-audio release when you prefer subs, +15 when you prefer dubs |
| Downloads | +15 over 10,000, +10 over 5,000, +5 over 1,000 |

## Fit for the episode

Automatic search adds these. They measure how well the release matches the series and episode being searched for, so that a popular release for the wrong show or episode does not win.

| Part | Points |
|---|---|
| Title Alias Match | up to +40, by how much of the series title, or one of its other names, the release title contains |
| Title Match Confidence | 0 for an exact title, -25 for a partial one, and another -10 when the match came through an alternate title or a related series |
| Season Mismatch | -100 when the release names a different season |
| Episode Number Match | +40 when the release covers the episode |
| Wrong Episode Number | -1000 for a release found through a related series that names a different episode |
| Unparseable Episode Number | -500 for such a release with no episode number Ryokan can read |
| Single-Episode Target | +10 for a single-episode release when one episode is wanted |
| Batch Penalty | -20 for a batch when one episode is wanted, -5 for a batch when the whole title is one episode (a movie or an OVA) |
| Movie / Special / OVA | +8 when the title is one episode and the release says movie, special, or OVA |
| Preferred Group (auto) | +180 for the first group on your preferred list and 30 less per place, -40 for a group not on the list, -15 for no group. Only when you have a list |
| Source / Resolution Fit | See below |
| Finished Series BD Bonus | +35 for a BluRay release of a finished series when **Finished series** is set to prefer BD |

**Source / Resolution Fit** is made up of:

| | Resolution | Source |
|---|---|---|
| At or above what you prefer | +25 | +15 |
| Exactly what you prefer | +15 more | +10 more |
| Below what you prefer | -10, and -10 more per step | -5, and -5 more per step |
| At or above the cutoff | +10 | +5 |
| Cannot be read | -10 | -5 |

When you prefer BluRay, a BDMV adds 7 and a remux adds 5. A release where neither resolution nor source can be read gets -5.

The preferred resolution and source, the cutoffs, and the finished-series rule are set under **Settings → Quality & Releases** ([Configuration](configuration.md#quality-releases)).

## Custom Formats and SeaDex

Custom Format scores are added on top. Every format that matches a release contributes its score. See [Custom Formats](configuration.md#custom-formats).

**Minimum Score** on the Custom Formats tab removes releases below that score from automatic searches. Interactive search still shows them.

A release that [SeaDex](https://releases.moe) lists as best gets +10,000, which beats everything else combined. Turn it on with the SeaDex switch under Quality & Releases or with the SeaDex Best Custom Format. If both are on, the bonus is still only counted once.

Releases on the blocklist (Downloads → Blocklist) are never scored.

## Replacing a file you already have

Ryokan replaces an episode you already have when a release is better in one of three ways. It checks them in this order.

**Quality.** A higher source or resolution, while the file is below your cutoff. Once the file meets the cutoff, quality alone does not replace it.

**Revision.** A v2, PROPER or REPACK of the same release from the same group. This replaces the file even if it already meets the cutoff, because a v2 is a fix. Through RSS, this only happens while the file is less than a week old. A v2 from a different group is ignored.

**Custom Format score.** A release of the same quality with a higher Custom Format score, until the file's score reaches **Upgrade Until Custom Format Score**. The new release has to score higher by at least the **Minimum Custom Format Score Increment**. The default cutoff of 0 turns this off for any file scoring 0 or more. This also happens only through RSS, and only for files Ryokan grabbed itself.

The daily upgrade search only looks at files below the quality cutoff. Revision and score upgrades for files that already meet it come through RSS.

**Propers and Repacks** under Quality & Releases controls how revisions are treated: **Prefer and upgrade** (the default), **Prefer, do not upgrade**, or **Do not prefer**.

To stop a series' files from being replaced, turn off **Upgrades** on its series page. Ryokan then only grabs episodes you do not have. Searching a single episode yourself, or picking a release in interactive search, still replaces the file.

When a release is not taken, the reason is in the RSS log and the automatic search log.

## What automatic search will and will not grab

Automatic search and RSS only take a release whose name contains one of the series' titles exactly and names nothing more. A release called "Dr. Stone New World" is not taken for Dr. STONE.

When a search skips releases for this reason, the toast and the log list them. If one of them is right:

1. Open the series page and expand **Advanced search overrides**.
2. Under **Alternate titles**, add the name that group uses, one per line.

That name is then searched for and treated as an exact match everywhere: automatic search, RSS, the upgrade search and the misgrab check.

Interactive search shows near matches too, since you choose the release yourself.

### Sequels numbered as one long series

Fansub groups often keep counting across seasons, so "Jujutsu Kaisen - 56" is season 3, episode 9. Ryokan works out how many episodes came before from AniList's list of sequels, and searches under the earlier seasons' names as well. Where AniList is missing a link, it uses [anime-relations](https://github.com/erengy/anime-relations), a community-maintained list that ships with Ryokan.

## Search tips

- The **Search** page searches Nyaa directly. Use the uploader field to find a specific group's releases. The category menu switches between English-translated releases and all anime, and the filter menu can limit results to trusted uploaders.
- Add "batch" to a query to find season packs, for example "Jujutsu Kaisen batch 1080p".
- **Interactive search** on a series page searches Nyaa, your indexers and your feeds together and ranks the results. **Default custom query tokens** under Quality & Releases are added to it, and a series can set its own under **Advanced search overrides**.

## Grabbing a release

Click **Grab** next to a release to send it to your download client. When the download finishes, Ryokan imports it.

A torrent with several files opens the file picker first, so you can leave out samples and extras. Turn the picker off with **Interactive file picker** under **Settings → General**.

!!! warning "Closing the picker does not cancel the grab"
    If you close the picker, the tab or the page without confirming, the release is grabbed a couple of minutes later with the files that were ticked. To cancel, untick everything and then close it.
