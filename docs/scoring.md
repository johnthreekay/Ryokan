# How releases are scored

Every release Ryokan finds for an episode gets a score, and the highest score is the one it grabs. The score is a sum of parts, and you can see every part for any release: open a series page, click **Interactive search** on an episode, and expand the score next to a release. The names below are the ones that breakdown uses.

## Release signals

These apply to every search, automatic or interactive.

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

Automatic search adds these. They compare the release with the series and episode Ryokan is looking for, so a release for the wrong show or the wrong episode cannot win on popularity alone.

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
| Preferred Group (auto) | +180 for the first group on your preferred list and 30 less per place, -40 for a group not on the list, -15 for no group, all only when you have a list |
| Source / Resolution Fit | Resolution first: +25 at or above the resolution you prefer, +15 more for an exact match, -10 and another -10 per step below it, +10 at or above the cutoff, -10 when it cannot be read. Then source: +15 at or above the source you prefer, +10 more for an exact match, -5 and another -5 per step below, +5 at or above the cutoff, -5 when it cannot be read. When you prefer BluRay, a BDMV adds 7 and a remux adds 5. A release with neither readable: -5 |
| Finished Series BD Bonus | +35 for a BluRay release of a finished series when **Finished series** is set to prefer BD |

The resolution and source you prefer, the cutoffs, and the finished-series rule are all set under **Settings → Quality & Releases** ([Configuration](configuration.md#quality-releases)).

## Custom Formats and SeaDex

Custom Format scores are added on top ([Configuration](configuration.md#custom-formats)). Every format that matches a release contributes its score, so the total is what separates releases that fit the episode equally well. **Minimum Score** on the same tab drops automatic candidates below a floor; interactive search still shows them.

A release that [SeaDex](https://releases.moe) lists as best gets +10,000, through either the SeaDex toggle on Quality & Releases or the SeaDex Best Custom Format, never both. That is large enough to beat every other part combined, which is the point: when the community has a settled answer, Ryokan takes it.

Releases on the blocklist (Downloads → Blocklist) are skipped before any of this runs.

## Search tips

- The **Search** page searches Nyaa directly. The uploader field limits results to one Nyaa account, which is the easiest way to find a specific release group. The category menu narrows to English-translated releases or opens up to all anime, and the filter menu can limit results to trusted uploaders.
- Add "batch" to a query to find season packs, for example "Jujutsu Kaisen batch 1080p".
- **Interactive search** on a series page searches every source at once (Nyaa, your indexers, and your feeds) and ranks the results with the scoring above. **Default custom query tokens** and **Default restrict to uploader** under Quality & Releases pre-fill it, and a series can override both on its own page under **Advanced search overrides**.

## Grabbing a release

Click **Grab** next to a release to send it to your download client. A multi-file release opens the file picker first so you can leave out samples and extras; **Interactive file picker** under **Settings → General** turns that off. The release goes to the client Ryokan routes it to (see [Download clients](download-clients.md)) with your category or label, and post-processing takes it from there once the download finishes.
