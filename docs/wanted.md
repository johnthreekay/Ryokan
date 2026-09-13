# Wanted

The Wanted page lists what the library is still after, the way Sonarr's Wanted page does. It lives at `/wanted` in the top nav next to Calendar.

## Missing

Every monitored episode that has already aired and is not on disk or downloading, grouped by series. A series whose monitoring is off does not appear, and neither do episodes that have not aired yet. Each row links to the series page and shows the episode numbers it is short of.

## Cutoff unmet

Every episode on disk whose quality is below your cutoff (Settings → Quality & Releases), for series that allow upgrades. Each entry shows the quality of the file you have. This is the same list the daily upgrade search works through.

## Searching

**Search** on a row runs that series' automatic search, the same one the series page offers. Tick several rows and press **Search selected**, or press **Search all** for every row on the current tab. The searches run one series at a time in the background, and the toast in the corner follows along and reports how many releases were grabbed at the end. Only one such run goes at a time; a second request while one is running is refused rather than queued.

There is no scheduled search for missing episodes. RSS picks up new episodes as they are released, and the upgrade search runs daily. This page is the way to go back for older gaps.
