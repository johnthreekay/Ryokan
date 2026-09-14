# Wanted

The Wanted page lists what the library is still after, the way Sonarr's Wanted page does. It lives at `/wanted` in the top nav next to Calendar.

## Missing

Every monitored episode that has already aired and is not on disk or downloading, grouped by series. A series whose monitoring is off does not appear, and neither do episodes that have not aired yet. Each row links to the series page and shows the episode numbers it is short of.

## Quality cutoff unmet

Every episode on disk whose quality is below your cutoff (Settings → Quality & Releases), for series that allow upgrades. Each entry shows the quality of the file you have. This is the same list the daily upgrade search works through.

## Searching

Each row has the same pair of buttons the series page offers for an episode. **Auto search** runs the series' automatic search and grabs the best release for every listed episode. **Interactive** opens the release list so you can pick one yourself: choose the episode at the top of the window, or **Batch releases** to look for a season pack, then press **Grab** on the release you want. A batch release opens the file picker first, as it does on the series page. A grabbed episode leaves the list once it is downloading.

Tick several rows and press **Search selected**, or press **Search all** for every row on the current tab. Both run the automatic search. The searches run one series at a time in the background, and the toast in the corner follows along and reports how many releases were grabbed at the end. Only one such run goes at a time; a second request while one is running is refused rather than queued.

There is no scheduled search for missing episodes. RSS picks up new episodes as they are released, and the upgrade search runs daily. This page is the way to go back for older gaps.
