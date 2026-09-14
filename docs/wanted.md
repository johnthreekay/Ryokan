# Wanted

The Wanted page lists what the library is still after, the way Sonarr's Wanted page does. It lives at `/wanted` in the top nav next to Calendar.

## Missing

Every monitored episode that has already aired and is not on disk or downloading, grouped by series. A series whose monitoring is off does not appear, and neither do episodes that have not aired yet. Each row links to the series page and shows the episode numbers it is short of.

## Quality cutoff unmet

Every episode on disk whose quality is below your cutoff (Settings → Quality & Releases), for series that allow upgrades. Each entry shows the quality of the file you have. This is the same list the daily upgrade search works through.

## Searching

Each row has the same pair of buttons the series page offers for an episode. **Auto search** runs the series' automatic search and grabs the best release for every listed episode. **Interactive** opens the release list so you can pick one yourself.

The **Search for** menu at the top of that window decides what is listed:

- **Batch releases** looks for season packs and complete releases. Grabbing one opens the file picker with only the files for the episodes you are missing ticked, so a twelve-episode pack for two missing episodes downloads two files. Tick more if you want the rest. Each file shows the episode its name parses to. Picking a single episode from the menu and grabbing a batch from its results ticks that episode's file only. With the file picker turned off in Settings, a batch from this page is grabbed whole, as it is from the series page. Closing the picker without confirming still grabs the files that were ticked, a couple of minutes later.
- **Episodes 3-11** (the whole wanted range, the default when more than one episode is wanted) runs one search and lists every single-episode release for those episodes, with an Episode column and the list ordered by episode. Batches are left out of this list on purpose, so a season pack only shows up when you ask for one. The first twelve wanted episodes each get their own query; past that only the series-wide search runs, so a series missing hundreds of episodes is a batch case. The per-episode entries stay the thorough option for one gap.
- **Episode N** searches that one episode, as on the series page.

Press **Grab** on the release you want. A grabbed episode leaves the list once it is downloading.

Tick several rows and press **Search selected**, or press **Search all** for every row on the current tab. Both run the automatic search. The searches run one series at a time in the background, and the toast in the corner follows along and reports how many releases were grabbed at the end. Only one such run goes at a time; a second request while one is running is refused rather than queued.

There is no scheduled search for missing episodes. RSS picks up new episodes as they are released, and the upgrade search runs daily. This page is the way to go back for older gaps.
