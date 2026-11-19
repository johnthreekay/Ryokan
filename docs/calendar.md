# Calendar

The Calendar page shows when episodes air for the shows in your library. You can also subscribe to it from Apple Calendar, Google Calendar or Thunderbird.

## Views

- **This week**: a list, grouped by day, with today highlighted.
- **Next week**: the same for the seven days after.
- **This month**: a month grid.

## What each episode shows

- The series cover and title.
- The episode number. Each season is its own series in Ryokan, so the first episode of *Re:Zero 4th Season* is E01, not S04E01.
- A **Premiere** badge in the list, or a **★** in the grid, on the first episode of a series.
- Whether the series is **Monitored**.

Click an episode to open its series page.

## Filtering

- **Search** matches romaji, English and native titles, so "attack on titan" finds *Shingeki no Kyojin*.
- **Monitored only** hides series that are not monitored.

## Subscribing from your calendar app

1. Create an API key with the `calendar` scope under **Settings → API Keys**, if you do not have one.
2. On the Calendar page, click **iCal Link**, choose the key, and copy the URL.
3. Add the URL to your calendar app:
    - **Apple Calendar**: File → New Calendar Subscription
    - **Google Calendar**: Other calendars → + → From URL
    - **Thunderbird**: New Calendar → On the Network → iCalendar (ICS)

The URL contains the key, so treat it like a password.

The feed covers the next 30 days. To change that, add options to the end of the URL:

- `&days=N` sets the number of days, up to 90.
- `&monitored=true` leaves out series that are not monitored.

## How current the times are

Air times come from AniList. Ryokan fetches them when you add a series and refreshes them every 12 hours.

To refresh now, go to **System → Scheduled Tasks** and click **Run now** on **Episode air-date refresh**.

## Series added from MyAnimeList

A series that AniList has no entry for, which Ryokan added from MyAnimeList instead, does not appear on the calendar. This mostly affects old or obscure titles.

---

*Last updated: 2026-10-04.*
