# Calendar

Ryokan's calendar shows what's airing for the shows in your library. It lives at `/calendar` from the top nav, between Library and Search. There's also a subscription feed if you'd rather see new episodes inside Apple Calendar, Google Calendar, or Thunderbird alongside the rest of your week.

## Views

Three quick toggles at the top of the page:

- **This week**: list view. Each day is a sticky header with the episodes airing that day stacked beneath. Today gets a colored stripe so your eye lands there.
- **Next week**: same shape, but the next seven days.
- **This month**: calendar grid view. Sun-to-Sat rows for the current month; each cell holds the episodes for that day.

## What you'll see on each episode

- The series cover thumbnail and title.
- The episode number (E01, E02, ...). Each anime "season" in Ryokan is its own series with its own E1, E2 numbering, so an episode of *Re:Zero 4th Season* shows as E01 of that series, not S04E01 of the franchise.
- A **Premiere** badge (in the list) or a small **★** (in the grid) on E01 of any series, so you can spot where a new cour starts.
- Whether the series is **Monitored**. This is the same flag the rest of Ryokan uses to decide whether to grab episodes for it.

Click an episode to jump to its series page.

## Filtering

- **Search**: type into the box to filter the list down. The search matches across all the title variants Ryokan knows about (romaji, English, native), so "attack on titan" finds *Shingeki no Kyojin* even if your title language is set to romaji.
- **Monitored only**: hide series Ryokan isn't tracking. Useful when your library has both actively-monitored shows and stuff you've added but turned monitoring off for.

## Subscribing from your calendar app

Click the **iCal Link** button in the page header. The modal:

1. Asks you to pick an API key with the `calendar` scope (create one on **Settings → API Keys** if you don't have one yet).
2. Builds the full subscription URL. The key is part of the URL, so treat it like a password.
3. Has a Copy button to put the URL on your clipboard.

Then in your calendar app:

- **Apple Calendar**: File → New Calendar Subscription, paste the URL.
- **Google Calendar**: Other calendars → + → From URL, paste the URL.
- **Thunderbird**: New Calendar → On the Network → iCalendar (ICS), paste the URL.

The feed defaults to the next 30 days. If you want a wider or narrower window, append `?days=N` to the URL (capped at 90). Append `?monitored=true` to only see monitored series.

## How fresh is the data?

Air times come from AniList. Ryokan refreshes them every 12 hours and stores its own copy, so the calendar page doesn't have to call AniList every time you load it. Two things to know:

- A newly-added series shows up within seconds. Ryokan grabs its air times right after you add it, not on the next 12-hour refresh.
- If AniList updates an episode's air time, you'll see the change within 12 hours. If you want it sooner, go to **System → Scheduled Tasks**, find **Episode air-date refresh**, and click **Run now**.

## Series Ryokan adds via MAL

A few series in your library may have come from MAL instead of AniList. These are usually older or obscure titles that AniList doesn't have a record for. Those won't show up on the calendar, because the calendar's air-time data only comes from AniList. Most shows are on AniList and won't be affected.

---

*Last updated: 2026-08-29.*
