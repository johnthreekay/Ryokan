//! iCal calendar feed (issue #115).
//!
//! `GET /api/calendar.ics` — returns an iCalendar 2.0 document
//! covering the next N days of upcoming episodes (default 30) for
//! the user's library. Subscribed by Google Calendar / Apple
//! Calendar / Thunderbird via the per-key URL surfaced in the
//! Settings → Calendar panel.
//!
//! Auth: `calendar`-scoped API key (`require_calendar_scope`
//! middleware in `handlers::scoped_auth`). Calendar subscribers
//! can't carry cookies, which is why the scoped-key system in
//! #114 had to land first.
//!
//! ## Output shape
//!
//! Hand-rolled iCalendar 2.0 text (no `ics`-crate dependency —
//! the format is small and the round-trip we care about is just
//! "RFC-5545 compatible enough for Google + Apple + Thunderbird").
//!
//! Per VEVENT:
//! - `SUMMARY`: `<series_title> · E<episode>`. Each anime "season"
//!   in Ryokan is its own series with its own E1..EN numbering, so
//!   a hardcoded `S01` would read as wrong when the title already
//!   names the season ("Re:Zero ... 4th Season S01E07"). The
//!   on-disk naming in `services/post_processing` keeps S01E for
//!   Sonarr/Plex compat; the user-facing calendar entry doesn't
//!   need that constraint.
//! - `DTSTART` / `DTEND`: from `airing_at` + `duration_minutes`.
//! - `UID`: `ryokan-<series_id>-<episode>@ryokan.local` — stable
//!   across feed fetches so calendar clients dedupe.
//! - `DESCRIPTION`: monitoring state + grabbed status.
//! - `URL`: deep link back to `/series/{id}` on the request's host
//!   (best-effort; falls back to a relative URL if the host can't
//!   be resolved).
//! - `STATUS`: `TENTATIVE` for episodes >7 days out (AL airing
//!   schedules can shift), `CONFIRMED` for the next 7 days.
//!
//! ## Caching
//!
//! Server-side: the calendar reader joins against the local
//! `episode_airings` table, kept fresh by the 12h `airing_refresh`
//! supervised task — no per-request AL fetch, no in-process cache.
//! HTTP-side: `Cache-Control: public, max-age=600` + an `ETag`
//! hashed over each event's `(series_id, episode, airing_at)`
//! tuple, so calendar clients honor conditional GETs for free.

use askama::Template;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderName, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use axum_htmx::{HxBoosted, HxRequest};
use chrono::{Datelike, TimeZone};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::config;
use crate::services::calendar::{self, DEFAULT_FORWARD_DAYS, UpcomingEpisode};

const NOW_PLUS_7_DAYS_THRESHOLD: i64 = 7 * 86400;

/// "Next week" needs an offset start (skip the first 7 days). All
/// other week-shaped ranges start from now. Returns
/// `(from_offset_days, length_days)`.
///
/// `month` is *not* served by this function — it's a calendar-month
/// grid view, not a sliding-window list, so the handler computes
/// its own from/to from the calendar month containing "now."
fn range_to_window(range: &str) -> (i64, i64) {
    match range {
        "next_week" | "next-week" => (7, 7),
        _ => (0, 7),
    }
}

/// Compute the from/to window for the month-grid view: the calendar
/// weeks (Sun-start) covering the month containing `now_utc`. The
/// grid extends to the Sunday on or before the 1st and the Saturday
/// on or after the last day of the month, so a 4-week or 6-week
/// grid is always a clean rectangle.
fn month_grid_window(now_utc: chrono::DateTime<chrono::Utc>) -> (i64, i64) {
    let first_of_month = chrono::Utc
        .with_ymd_and_hms(now_utc.year(), now_utc.month(), 1, 0, 0, 0)
        .single()
        .expect("first-of-month is always valid");
    let next_month_first = if now_utc.month() == 12 {
        chrono::Utc
            .with_ymd_and_hms(now_utc.year() + 1, 1, 1, 0, 0, 0)
            .single()
            .expect("next-january is always valid")
    } else {
        chrono::Utc
            .with_ymd_and_hms(now_utc.year(), now_utc.month() + 1, 1, 0, 0, 0)
            .single()
            .expect("next-month-first is always valid")
    };
    let last_of_month = next_month_first - chrono::Duration::days(1);
    let grid_start = sunday_on_or_before(first_of_month);
    // Grid end is exclusive — Saturday on/after last day, then +1.
    let grid_end_exclusive = saturday_on_or_after(last_of_month) + chrono::Duration::days(1);
    (grid_start.timestamp(), grid_end_exclusive.timestamp())
}

fn sunday_on_or_before(d: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    let offset = d.weekday().num_days_from_sunday() as i64;
    d - chrono::Duration::days(offset)
}

fn saturday_on_or_after(d: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    let offset = 6_i64 - d.weekday().num_days_from_sunday() as i64;
    d + chrono::Duration::days(offset)
}

#[derive(Debug, Deserialize)]
pub struct CalendarPageQuery {
    /// `this_week` (default), `next_week`, or `month`.
    #[serde(default)]
    pub range: Option<String>,
    /// `?monitored=true` filters to only monitored series. Default
    /// off — surfaces every airing series.
    #[serde(default)]
    pub monitored: Option<bool>,
}

#[derive(Template)]
#[template(path = "calendar.html")]
struct CalendarPageTemplate {
    page: String,
    title_language: String,
    /// Active range token — drives the toggle's selected state.
    range: String,
    monitored_only: bool,
    /// `"list"` for week-shaped ranges, `"grid"` for month. Drives
    /// the body partial selection in the template.
    view_mode: String,
    /// Pre-grouped day buckets for the list view. Empty when
    /// `view_mode == "grid"`.
    day_buckets: Vec<DayBucket>,
    /// Pre-built calendar-month grid for the grid view. Empty
    /// (`weeks: vec![]`) when `view_mode == "list"`.
    month_grid: MonthGrid,
    /// Calendar-scoped API keys for the Subscribe section. Filtered
    /// to enabled keys with the `calendar` or `admin` scope so
    /// users only see ones that'd actually authorize the feed.
    calendar_keys: Vec<CalendarKeyOption>,
    /// True when the user has at least one positive-AL-id series in
    /// their library; drives the empty-state copy ("add a series"
    /// vs. "no episodes airing in this range").
    library_is_empty: bool,
}

/// Per-episode template input. Pre-formats per-day-grouping
/// fields so the client doesn't have to re-derive them.
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeView {
    pub series_id: i64,
    pub series_title: String,
    pub cover_url: String,
    pub episode: i32,
    /// Unix epoch seconds (UTC). The client renders this in the
    /// user's local timezone via `new Date(unixTs * 1000)`.
    pub airing_at: i64,
    pub monitored: bool,
    /// Lowercase concatenation of every title variant (romaji +
    /// english + native + db-stored) so the page's series-name
    /// search input matches against any of them, not just the
    /// resolved `title_language` form. Server-precomputed once.
    pub search_haystack: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayBucket {
    /// Render-ready day label, e.g. `"Monday, May 12"`. Server-
    /// formatted in UTC for the initial render; the client may
    /// re-group by browser-local date if it cares (most won't —
    /// the day boundary at UTC matches the airingAt value the
    /// browser would compute back).
    pub label: String,
    /// UTC midnight Unix timestamp for this bucket's day. Used
    /// client-side to highlight the today-section and let the
    /// initial-load auto-scroll find it.
    pub day_key: i64,
    pub episodes: Vec<EpisodeView>,
}

#[derive(Debug, Clone, Serialize)]
struct CalendarKeyOption {
    id: i64,
    name: String,
}

/// One cell in the month-grid view. Cells outside the current
/// calendar month are still rendered (faded) so each row is a
/// complete Sun→Sat week.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MonthCell {
    /// UTC midnight Unix timestamp for this cell's day. Lets the
    /// JS today-highlighter match against the same key list-view
    /// uses.
    pub day_key: i64,
    /// Day-of-month number (1-31). Just the day, e.g. `12`.
    pub day_number: u32,
    /// `true` when this cell is "now"'s UTC date — drives the
    /// today accent in CSS so first paint doesn't flash.
    pub is_today: bool,
    /// `true` when this cell falls inside the visible calendar
    /// month. Cells outside (the leading/trailing week padding)
    /// render faded.
    pub is_in_current_month: bool,
    /// Episodes airing on this day, in airing-time order.
    pub episodes: Vec<EpisodeView>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MonthGrid {
    /// Render-ready label, e.g. `"May 2026"`.
    pub month_label: String,
    /// 4–6 weeks, each a Sun→Sat row of 7 cells.
    pub weeks: Vec<Vec<MonthCell>>,
}

/// `GET /calendar` — the in-app calendar page. Cookie-auth gated
/// (sits inside the `protected_routes` group).
///
/// Render branching:
/// - `HX-Request: true` *and not* `HX-Boosted: true` → renders just
///   the `partials/calendar/list.html` partial. This matches the
///   range-tab swap path (explicit `hx-get` against `#calendar-list`)
///   so changing the range only replaces the list region.
/// - Boosted nav (clicking the topbar Calendar link sends
///   `HX-Request` *and* `HX-Boosted`) or plain GET → full page.
///   Used on direct URL hits, browser back/forward, and the no-JS
///   fallback for the range tabs.
///
/// The boost-vs-swap discriminator is load-bearing: returning the
/// partial on a boosted nav strips the page chrome (header, range
/// tabs, modal) and leaves the user staring at a bare list.
pub async fn page(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    HxBoosted(is_boosted): HxBoosted,
    Query(params): Query<CalendarPageQuery>,
) -> Html<String> {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let range = params.range.unwrap_or_else(|| "this_week".to_string());
    let monitored_only = params.monitored.unwrap_or(false);

    let now_utc = chrono::Utc::now();
    let now = now_utc.timestamp();
    let view_mode = if range == "month" { "grid" } else { "list" };

    // Window selection: list ranges slide N days from now;
    // grid uses the calendar weeks containing the current
    // month so the rendered grid is always a clean rectangle.
    let (from, to) = if view_mode == "grid" {
        month_grid_window(now_utc)
    } else {
        let (offset_days, length_days) = range_to_window(&range);
        let f = now + offset_days * 86400;
        (f, f + length_days * 86400)
    };

    let episodes = calendar::fetch_upcoming(&state.db, &cfg, from, to, monitored_only)
        .await
        .unwrap_or_default();

    let library_is_empty = library_has_no_positive_al_series(&state.db).await;

    let episode_views: Vec<EpisodeView> = episodes
        .into_iter()
        .map(|e| EpisodeView {
            series_id: e.series_id,
            series_title: e.series_title,
            cover_url: e.cover_url,
            episode: e.episode,
            airing_at: e.airing_at,
            monitored: e.monitored,
            search_haystack: e.search_haystack,
        })
        .collect();

    let (day_buckets, month_grid) = if view_mode == "grid" {
        (
            Vec::new(),
            build_month_grid(&episode_views, now_utc, from, to),
        )
    } else {
        (group_by_day(&episode_views), MonthGrid::default())
    };

    // HTMX swap (range tabs, monitored toggle) → just the body
    // partial for the active view. A boosted full-page nav also
    // carries `HX-Request` but expects a full body, so it falls
    // through to the page render below.
    if is_htmx && !is_boosted {
        return if view_mode == "grid" {
            let partial = CalendarGridPartial {
                month_grid,
                library_is_empty,
            };
            Html(partial.render().unwrap_or_default())
        } else {
            let partial = CalendarListPartial {
                day_buckets,
                library_is_empty,
            };
            Html(partial.render().unwrap_or_default())
        };
    }

    // Full page render — includes chrome (range tabs, filters,
    // iCal modal) plus the body partial.
    let calendar_keys = collect_calendar_keys(&state.db).await;
    let tmpl = CalendarPageTemplate {
        page: "calendar".to_string(),
        title_language: cfg.title_language.clone(),
        range,
        monitored_only,
        view_mode: view_mode.to_string(),
        day_buckets,
        month_grid,
        calendar_keys,
        library_is_empty,
    };
    Html(tmpl.render().unwrap_or_default())
}

#[derive(Template)]
#[template(path = "partials/calendar/list.html")]
struct CalendarListPartial {
    day_buckets: Vec<DayBucket>,
    library_is_empty: bool,
}

#[derive(Template)]
#[template(path = "partials/calendar/grid.html")]
struct CalendarGridPartial {
    month_grid: MonthGrid,
    library_is_empty: bool,
}

/// Build the Sun→Sat calendar grid for the month containing
/// `now_utc`. The `from`/`to` window is the same one passed to
/// [`calendar::fetch_upcoming`], so cells are guaranteed to fall
/// inside the fetched episode range.
fn build_month_grid(
    episodes: &[EpisodeView],
    now_utc: chrono::DateTime<chrono::Utc>,
    from: i64,
    to_exclusive: i64,
) -> MonthGrid {
    use std::collections::HashMap;
    let mut by_day: HashMap<i64, Vec<EpisodeView>> = HashMap::new();
    for ep in episodes {
        let day_key = ep.airing_at - ep.airing_at.rem_euclid(86400);
        by_day.entry(day_key).or_default().push(ep.clone());
    }
    for v in by_day.values_mut() {
        v.sort_by_key(|e| e.airing_at);
    }

    let today_key = now_utc.timestamp() - now_utc.timestamp().rem_euclid(86400);
    let current_month = now_utc.month();
    let month_label = now_utc.format("%B %Y").to_string();

    let mut weeks: Vec<Vec<MonthCell>> = Vec::new();
    let mut cur = from;
    while cur < to_exclusive {
        let mut week: Vec<MonthCell> = Vec::with_capacity(7);
        for _ in 0..7 {
            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(cur, 0)
                .unwrap_or_else(|| chrono::Utc.timestamp_opt(0, 0).single().unwrap());
            let cell = MonthCell {
                day_key: cur,
                day_number: dt.day(),
                is_today: cur == today_key,
                is_in_current_month: dt.month() == current_month,
                episodes: by_day.remove(&cur).unwrap_or_default(),
            };
            week.push(cell);
            cur += 86400;
        }
        weeks.push(week);
    }

    MonthGrid { month_label, weeks }
}

async fn library_has_no_positive_al_series(db: &sqlx::SqlitePool) -> bool {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM series WHERE anilist_id > 0")
        .fetch_one(db)
        .await
        .unwrap_or(0);
    count == 0
}

async fn collect_calendar_keys(db: &sqlx::SqlitePool) -> Vec<CalendarKeyOption> {
    let keys = crate::models::api_key::list(db).await.unwrap_or_default();
    keys.into_iter()
        .filter(|k| k.enabled && k.scopes.iter().any(|s| s == "calendar" || s == "admin"))
        .map(|k| CalendarKeyOption {
            id: k.id,
            name: k.name,
        })
        .collect()
}

/// Group a flat episode list into day buckets keyed by the date
/// portion of `airing_at` (UTC). Server-side grouping so the
/// initial render is one pass; the JS-driven re-render uses the
/// same logic against the JSON wire shape.
fn group_by_day(episodes: &[EpisodeView]) -> Vec<DayBucket> {
    use std::collections::BTreeMap;
    let mut by_date: BTreeMap<i64, Vec<EpisodeView>> = BTreeMap::new();
    for ep in episodes {
        // Collapse to UTC midnight so two episodes on the same UTC
        // date sort into the same bucket.
        let day_key = ep.airing_at - (ep.airing_at.rem_euclid(86400));
        by_date.entry(day_key).or_default().push(ep.clone());
    }
    by_date
        .into_iter()
        .map(|(day_key, eps)| DayBucket {
            label: chrono::DateTime::<chrono::Utc>::from_timestamp(day_key, 0)
                .map(|dt| dt.format("%A, %b %-d").to_string())
                .unwrap_or_default(),
            day_key,
            episodes: eps,
        })
        .collect()
}

/// Query string for the iCal endpoint. Both fields opt-in; the
/// default behavior is "next 30 days, every airing series."
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct IcalQuery {
    /// Forward window in days. Capped at 90 server-side so a
    /// `?days=10000` request can't blow up the AL fetch budget.
    #[serde(default)]
    pub days: Option<i64>,
    /// `?monitored=true` filters to only monitored series. Default
    /// off — the unconditional default surfaces every airing
    /// series so users can browse "what's coming up" beyond their
    /// own list.
    #[serde(default)]
    pub monitored: Option<bool>,
}

const MAX_DAYS: i64 = 90;

/// `GET /api/calendar.ics`. Wired in `main.rs` behind the
/// `require_calendar_scope` middleware so only `calendar`-scoped
/// API keys reach it.
#[utoipa::path(
    get,
    path = "/api/calendar.ics",
    tag = "Calendar",
    summary = "iCal feed of upcoming episodes",
    description = "Airing schedule for library series as an iCalendar file. Requires an API key with the calendar scope, via X-Api-Key header or ?apikey= query, since calendar apps cannot carry cookies. Days is clamped to 90.",
    params(IcalQuery),
    responses(
        (status = 200, description = "text/calendar feed"),
        (status = 401, description = "Missing or invalid API key"),
        (status = 503, description = "Config not yet available"),
    ),
)]
pub async fn ical_feed(
    State(state): State<AppState>,
    Query(params): Query<IcalQuery>,
    headers: HeaderMap,
) -> Response {
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        Ok(None) | Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "5")],
                "Ryokan config not yet available",
            )
                .into_response();
        }
    };

    let days = params
        .days
        .unwrap_or(DEFAULT_FORWARD_DAYS)
        .clamp(1, MAX_DAYS);
    let monitored_only = params.monitored.unwrap_or(false);
    let now = chrono::Utc::now().timestamp();
    let from = now;
    let to = now + days * 86400;

    let episodes = match calendar::fetch_upcoming(&state.db, &cfg, from, to, monitored_only).await {
        Ok(v) => v,
        Err(e) => {
            // Surface the AL failure-prefix taxonomy as the right
            // HTTP shape: 503 + Retry-After for transient issues so
            // calendar clients back off (they rage-poll on 401 less
            // than on 5xx). Pure 4xx for misconfig isn't reachable
            // here — the auth middleware already gated; any error
            // bubbling out is upstream.
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "60")],
                format!("Calendar fetch failed: {e}"),
            )
                .into_response();
        }
    };

    let host_and_scheme = extract_host_and_scheme(&headers);
    let body = render_ical(
        &episodes,
        &cfg.title_language,
        host_and_scheme.as_ref(),
        now,
    );
    let etag = etag_for(&episodes);

    // Conditional GET — if the client sent the same etag they're
    // already showing, return 304 with empty body.
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH)
        && let Ok(s) = if_none_match.to_str()
        && s == etag.as_str()
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag.as_str())]).into_response();
    }

    let mut response_headers = vec![
        (
            header::CONTENT_TYPE,
            "text/calendar; charset=utf-8".to_string(),
        ),
        (header::CACHE_CONTROL, "public, max-age=600".to_string()),
        (header::ETAG, etag),
    ];
    // `Content-Disposition` so a direct browser hit downloads as
    // ryokan.ics rather than rendering inline as text/plain. Some
    // calendar apps (Apple Calendar, Outlook) want the file path
    // to end in `.ics` for their auto-import handlers.
    response_headers.push((
        HeaderName::from_static("content-disposition"),
        "inline; filename=\"ryokan.ics\"".to_string(),
    ));

    let header_pairs: Vec<(HeaderName, String)> = response_headers;
    let mut builder = Response::builder().status(StatusCode::OK);
    for (name, value) in header_pairs {
        builder = builder.header(name, value);
    }
    builder.body(body.into()).unwrap_or_else(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to build response",
        )
            .into_response()
    })
}

/// Best-effort extraction of the request's external host so the
/// iCal `URL` field can deep-link back to the series page. Returns
/// `(host, scheme)` when resolvable, or `None` when nothing is
/// trustworthy enough to emit (in which case the URL field falls
/// back to a relative path).
///
/// `X-Forwarded-Host` and `X-Forwarded-Proto` are honored only
/// when `RYOKAN_TRUSTED_PROXY` is set, matching the auth path's
/// header-trust contract: trusting them by default would let any
/// HTTP client write whatever they want into iCal events. With
/// the flag off we use the request's `Host` header (browsers /
/// HTTP clients always set this, and an attacker can't usefully
/// spoof it for someone else's calendar feed) and default the
/// scheme to `http`.
fn extract_host_and_scheme(headers: &HeaderMap) -> Option<(String, &'static str)> {
    let trust = *crate::handlers::auth::TRUST_PROXY_HEADERS;
    let host = if trust {
        headers
            .get("x-forwarded-host")
            .or_else(|| headers.get(header::HOST))
    } else {
        headers.get(header::HOST)
    }
    .and_then(|h| h.to_str().ok())?
    .to_string();
    let scheme = if trust {
        match headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or(s).trim().to_ascii_lowercase())
        {
            Some(ref s) if s == "https" => "https",
            _ => "http",
        }
    } else {
        "http"
    };
    Some((host, scheme))
}

/// Hand-rolled iCalendar 2.0 text. RFC 5545 compatible enough for
/// Google Calendar / Apple Calendar / Thunderbird auto-subscribe;
/// not a complete implementation (no recurring events, no VTIMEZONE,
/// no per-event TIMEZONE — every DTSTART is plain UTC `Z`-suffixed).
fn render_ical(
    episodes: &[UpcomingEpisode],
    _title_language: &str,
    host_and_scheme: Option<&(String, &'static str)>,
    now_unix: i64,
) -> String {
    // CRLF line endings per RFC 5545 §3.1. Some clients are lax
    // about LF, but Apple Calendar specifically rejects mixed.
    let mut out = String::with_capacity(256 + episodes.len() * 256);
    out.push_str("BEGIN:VCALENDAR\r\n");
    out.push_str("VERSION:2.0\r\n");
    out.push_str("PRODID:-//Ryokan//Calendar 1.0//EN\r\n");
    out.push_str("CALSCALE:GREGORIAN\r\n");
    out.push_str("METHOD:PUBLISH\r\n");
    out.push_str("X-WR-CALNAME:Ryokan\r\n");

    for ep in episodes {
        let start_utc = format_ical_utc(ep.airing_at);
        let duration_secs = (ep.duration_minutes.max(1) as i64) * 60;
        let end_utc = format_ical_utc(ep.airing_at + duration_secs);
        let summary = format!("{} \u{00B7} E{:02}", ep.series_title, ep.episode);
        let uid = format!("ryokan-{}-{}@ryokan.local", ep.series_id, ep.episode);
        let status = if ep.airing_at - now_unix > NOW_PLUS_7_DAYS_THRESHOLD {
            "TENTATIVE"
        } else {
            "CONFIRMED"
        };
        let mon_label = if ep.monitored {
            "Monitored"
        } else {
            "Not monitored"
        };
        // Real `\n` here, not the two-char escape `\\n`. RFC 5545
        // §3.3.11 says newlines inside TEXT values must be encoded
        // as the two-char sequence `\n`, and `escape_ical_text`'s
        // `'\n' => "\\n"` arm handles the encoding. Writing
        // `"\\n"` here would emit literal backslash-n on the wire.
        let description = format!("AniList ID: {}\n{}", ep.anilist_id, mon_label);
        let url = match host_and_scheme {
            Some((h, scheme)) => format!("{}://{}/series/{}", scheme, h, ep.series_id),
            None => format!("/series/{}", ep.series_id),
        };

        out.push_str("BEGIN:VEVENT\r\n");
        // DTSTAMP is required per RFC 5545; use now as the
        // server-side stamp. Some validators reject events
        // without it. Every content line below routes through
        // `push_folded_line` so a long title (the SUMMARY/UID
        // most likely to overflow) gets the §3.1 75-octet fold.
        push_folded_line(&mut out, &format!("DTSTAMP:{}", format_ical_utc(now_unix)));
        push_folded_line(&mut out, &format!("UID:{}", escape_ical_text(&uid)));
        push_folded_line(&mut out, &format!("DTSTART:{}", start_utc));
        push_folded_line(&mut out, &format!("DTEND:{}", end_utc));
        push_folded_line(&mut out, &format!("SUMMARY:{}", escape_ical_text(&summary)));
        push_folded_line(
            &mut out,
            &format!("DESCRIPTION:{}", escape_ical_text(&description)),
        );
        push_folded_line(&mut out, &format!("URL:{}", escape_ical_text(&url)));
        push_folded_line(&mut out, &format!("STATUS:{}", status));
        out.push_str("END:VEVENT\r\n");
    }

    out.push_str("END:VCALENDAR\r\n");
    out
}

/// Format a Unix epoch seconds value as an RFC 5545 UTC timestamp:
/// `YYYYMMDDTHHMMSSZ`. The `Z` suffix marks UTC; without it the
/// time gets interpreted as floating local time and shows up
/// shifted in subscribers' calendars.
fn format_ical_utc(unix_secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix_secs, 0)
        .map(|dt| dt.format("%Y%m%dT%H%M%SZ").to_string())
        .unwrap_or_else(|| "19700101T000000Z".to_string())
}

/// Escape per RFC 5545 §3.3.11. Backslash, comma, semicolon get
/// escaped; literal newlines become `\n`. Carriage returns are
/// dropped (they'd be re-introduced by the line wrapper).
fn escape_ical_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Build an ETag by hashing every event's identity tuple
/// `(series_id, episode, airing_at)`. Conditional-GET-friendly
/// because the same set of events hashes the same way, and any
/// shift (an episode shifted, a series added or removed) changes
/// the digest. The previous shape used `(count, max(airing_at))`
/// which collided when two distinct sets shared a count and a max
/// — narrow in practice, but a hash over the full identity space
/// is correctness-preserving and just as cheap.
fn etag_for(episodes: &[UpcomingEpisode]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    episodes.len().hash(&mut hasher);
    for e in episodes {
        e.series_id.hash(&mut hasher);
        e.episode.hash(&mut hasher);
        e.airing_at.hash(&mut hasher);
    }
    format!("\"{:x}\"", hasher.finish())
}

/// Fold a content line per RFC 5545 §3.1: lines longer than 75
/// octets are split with CRLF followed by a single space, which
/// the parser folds back together. Splits respect UTF-8 char
/// boundaries — the 75-octet limit is byte-based, but cutting
/// mid-codepoint would corrupt non-ASCII titles.
///
/// Emits a trailing `\r\n` so callers can write a complete content
/// line in one push.
fn push_folded_line(out: &mut String, line: &str) {
    const MAX: usize = 75;
    let bytes = line.as_bytes();
    if bytes.len() <= MAX {
        out.push_str(line);
        out.push_str("\r\n");
        return;
    }
    let mut start = 0;
    let mut first = true;
    while start < bytes.len() {
        let budget = if first { MAX } else { MAX - 1 }; // continuation lines start with a space
        let mut end = (start + budget).min(bytes.len());
        // Walk back to the nearest UTF-8 char boundary.
        while end < bytes.len() && !line.is_char_boundary(end) {
            end -= 1;
        }
        if !first {
            out.push(' ');
        }
        out.push_str(&line[start..end]);
        out.push_str("\r\n");
        start = end;
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(
        series_id: i64,
        anilist_id: i32,
        episode: i32,
        airing_at: i64,
        monitored: bool,
    ) -> UpcomingEpisode {
        UpcomingEpisode {
            series_id,
            anilist_id,
            series_title: "Test Series".to_string(),
            episode,
            airing_at,
            duration_minutes: 24,
            monitored,
            cover_url: String::new(),
            search_haystack: "test series".to_string(),
        }
    }

    #[test]
    fn empty_calendar_renders_valid_skeleton() {
        let body = render_ical(&[], "romaji", None, 0);
        assert!(body.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(body.contains("VERSION:2.0\r\n"));
        assert!(body.contains("PRODID:-//Ryokan//Calendar 1.0//EN\r\n"));
        assert!(body.ends_with("END:VCALENDAR\r\n"));
        assert!(!body.contains("BEGIN:VEVENT"));
    }

    #[test]
    fn vevent_carries_uid_dtstart_dtend_status() {
        let now = 1_700_000_000_i64; // somewhere in 2023
        let host = ("ryokan.example:8978".to_string(), "http");
        let body = render_ical(
            &[ep(42, 100, 7, now + 3 * 86400, true)],
            "romaji",
            Some(&host),
            now,
        );
        assert!(body.contains("UID:ryokan-42-7@ryokan.local\r\n"));
        // 3 days out → CONFIRMED, not TENTATIVE.
        assert!(body.contains("STATUS:CONFIRMED\r\n"));
        // DTSTART is the UTC airing time.
        assert!(body.contains("DTSTART:"));
        assert!(body.contains("DTEND:"));
        // SUMMARY is `<title> · E<NN>` zero-padded; no S01 prefix
        // because each anime "season" is its own Ryokan series.
        assert!(body.contains("SUMMARY:Test Series \u{00B7} E07\r\n"));
        // URL uses the host header.
        assert!(body.contains("URL:http://ryokan.example:8978/series/42\r\n"));
        // DESCRIPTION line break is RFC-5545 `\n`, not literal `\\n`.
        // After escape_ical_text the wire-form is single-backslash-n
        // sandwiched between the two parts.
        assert!(body.contains("DESCRIPTION:AniList ID: 100\\nMonitored\r\n"));
    }

    #[test]
    fn vevent_url_picks_https_when_scheme_is_passed() {
        let now = 1_700_000_000_i64;
        let host = ("ryokan.example".to_string(), "https");
        let body = render_ical(&[ep(7, 1, 1, now + 3600, true)], "romaji", Some(&host), now);
        assert!(body.contains("URL:https://ryokan.example/series/7\r\n"));
    }

    #[test]
    fn far_out_episodes_get_tentative_status() {
        let now = 1_700_000_000_i64;
        // 14 days out → past the 7-day threshold → TENTATIVE.
        let body = render_ical(&[ep(1, 1, 1, now + 14 * 86400, true)], "romaji", None, now);
        assert!(body.contains("STATUS:TENTATIVE\r\n"));
    }

    #[test]
    fn etag_is_stable_for_same_events() {
        let now = 1_700_000_000_i64;
        let evs = vec![
            ep(1, 1, 1, now + 86400, true),
            ep(2, 2, 1, now + 2 * 86400, false),
        ];
        let a = etag_for(&evs);
        let b = etag_for(&evs);
        assert_eq!(a, b);
    }

    #[test]
    fn etag_changes_when_max_airing_changes() {
        let now = 1_700_000_000_i64;
        let a = etag_for(&[ep(1, 1, 1, now + 86400, true)]);
        let b = etag_for(&[ep(1, 1, 1, now + 2 * 86400, true)]);
        assert_ne!(a, b);
    }

    #[test]
    fn description_text_escape_handles_special_chars() {
        let escaped = escape_ical_text("a, b; c\\d\nE");
        assert_eq!(escaped, "a\\, b\\; c\\\\d\\nE");
    }

    #[test]
    fn url_falls_back_to_relative_when_no_host() {
        let now = 1_700_000_000_i64;
        let body = render_ical(&[ep(99, 1, 1, now + 3600, true)], "romaji", None, now);
        assert!(body.contains("URL:/series/99\r\n"));
    }

    /// `2026-05-01` is a Friday → Sun-on-or-before is 2026-04-26
    /// (Sunday). `2026-05-31` is a Sunday → Sat-on-or-after is
    /// 2026-06-06 (Saturday). Grid spans 6 full weeks = 42 days.
    #[test]
    fn month_grid_window_spans_full_weeks_around_may_2026() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 12, 0, 0)
            .single()
            .unwrap();
        let (from, to) = month_grid_window(now);
        let from_dt = chrono::DateTime::<chrono::Utc>::from_timestamp(from, 0).unwrap();
        let to_dt = chrono::DateTime::<chrono::Utc>::from_timestamp(to, 0).unwrap();
        assert_eq!(from_dt.format("%Y-%m-%d").to_string(), "2026-04-26");
        assert_eq!(to_dt.format("%Y-%m-%d").to_string(), "2026-06-07");
        // 6 weeks × 7 days × 86400 sec.
        assert_eq!(to - from, 6 * 7 * 86400);
    }

    /// `2026-02-01` is a Sunday → grid starts on the 1st itself.
    /// `2026-02-28` is a Saturday → grid ends on the 28th. 4 weeks.
    #[test]
    fn month_grid_window_compacts_to_4_weeks_when_aligned() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 2, 14, 0, 0, 0)
            .single()
            .unwrap();
        let (from, to) = month_grid_window(now);
        assert_eq!(to - from, 4 * 7 * 86400);
    }

    #[test]
    fn month_grid_handles_december_year_rollover() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 12, 15, 0, 0, 0)
            .single()
            .unwrap();
        let (from, to) = month_grid_window(now);
        // Window must be a positive multiple of 7 days.
        let span_days = (to - from) / 86400;
        assert!((4 * 7..=6 * 7).contains(&span_days));
        assert_eq!(span_days % 7, 0);
    }

    fn ev(series_id: i64, episode: i32, airing_at: i64, monitored: bool) -> EpisodeView {
        EpisodeView {
            series_id,
            series_title: "Test".to_string(),
            cover_url: String::new(),
            episode,
            airing_at,
            monitored,
            search_haystack: "test".to_string(),
        }
    }

    #[test]
    fn build_month_grid_buckets_episodes_to_correct_cells() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 12, 0, 0)
            .single()
            .unwrap();
        let (from, to) = month_grid_window(now);
        // Two episodes on May 12 (Tue) at 09:00 and 21:00 UTC.
        let may12_09 = chrono::Utc
            .with_ymd_and_hms(2026, 5, 12, 9, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        let may12_21 = chrono::Utc
            .with_ymd_and_hms(2026, 5, 12, 21, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        let eps = vec![ev(1, 5, may12_21, true), ev(2, 1, may12_09, false)];
        let grid = build_month_grid(&eps, now, from, to);
        // Find the May 12 cell.
        let mut found: Option<&MonthCell> = None;
        for week in &grid.weeks {
            for cell in week {
                if cell.day_number == 12 && cell.is_in_current_month {
                    found = Some(cell);
                }
            }
        }
        let cell = found.expect("May 12 should be in grid");
        assert_eq!(cell.episodes.len(), 2);
        // Sorted by airing_at ascending.
        assert_eq!(cell.episodes[0].airing_at, may12_09);
        assert_eq!(cell.episodes[1].airing_at, may12_21);
    }

    #[test]
    fn build_month_grid_marks_today_cell() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 18, 0, 0)
            .single()
            .unwrap();
        let (from, to) = month_grid_window(now);
        let grid = build_month_grid(&[], now, from, to);
        let today_cells: Vec<&MonthCell> =
            grid.weeks.iter().flatten().filter(|c| c.is_today).collect();
        assert_eq!(today_cells.len(), 1);
        assert_eq!(today_cells[0].day_number, 9);
        assert!(today_cells[0].is_in_current_month);
    }
}
