# Wanted

The Wanted page lists the episodes Ryokan still needs. It has two tabs.

## Missing

Monitored episodes that have aired and are neither in your library nor downloading, grouped by series. Episodes that have not aired yet are not listed, and neither are series you do not monitor.

## Quality cutoff unmet

Episodes you have whose quality is below your cutoff (Settings → Quality & Releases), with the quality of the current file. Only series that allow upgrades are listed. The daily upgrade search works through this same list.

## Searching

Each row has two buttons:

- **Auto search** grabs the best release for every episode listed in the row.
- **Interactive** opens a list of releases for you to choose from.

To search several series at once, tick their rows and press **Search selected**, or press **Search all**. Both run automatic searches, one series at a time. A toast shows the progress and how many releases were grabbed. A second run cannot start while one is going.

### Interactive search

The **Search for** menu at the top of the window sets what is listed:

- **Episodes 3-11** (the default when a series is missing more than one episode): single-episode releases for all the wanted episodes, sorted by episode. Batches are not shown here.
- **Episode N**: releases for that one episode.
- **Batch releases**: season packs and complete releases.

Press **Grab** on the release you want.

Grabbing a batch opens the file picker with only the files for your missing episodes ticked. A twelve-episode pack for two missing episodes downloads two files, unless you tick more.

!!! warning "Closing the picker does not cancel the grab"
    If you close the picker without confirming, the ticked files are grabbed a couple of minutes later. To cancel, untick everything and then close it.

When a series is missing a large number of episodes, search for a batch. The **Episodes** search runs a separate query for each of the first twelve wanted episodes only.

## Older gaps

Ryokan does not search for missing episodes on a schedule. RSS picks up new episodes as they are released, and the upgrade search runs daily. For anything older, search from this page.
