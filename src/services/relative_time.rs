//! Short-form humanized relative-time formatting for `[data-ts]` cells.
//!
//! The `[data-ts]` UI idiom (used on Downloads → History, Series → header
//! "metadata refreshed N ago", etc.) ships a server-rendered string in
//! `textContent` and a JS tick (`static/js/base.js`'s `refresh()` hook,
//! 30s `setInterval` + every htmx.onLoad) that re-renders to keep it
//! current.
//!
//! Pre-this-helper, the server emitted the raw SQLite timestamp
//! ("2026-05-04 12:34:56") and a CSS rule `[data-ts]:not([data-ts-rendered])
//! { visibility: hidden }` hid the cell until JS replaced the text with
//! "5h ago" and stamped the rendered marker. Two compounding flashes:
//! (1) the brief blank-column paint between body-swap and JS-fill, and
//! (2) the column-width snap when "2026-05-04 12:34:56" (19 chars) was
//! replaced by "5h ago" (~6 chars) under `table-layout: auto`.
//!
//! Server-rendering the same short form here lets templates emit the
//! final string + `data-ts-rendered="1"` directly, so the visibility-
//! hidden CSS never applies on first paint and the JS tick produces an
//! identical string (idempotent textContent assignment, no paint).
//!
//! The output format matches `static/js/base.js`'s `humanize(deltaSec)`
//! exactly: `"just now"` / `"42s ago"` / `"5m ago"` / `"3h ago"` / `"2d ago"`.
//! Anything older than 30 days returns a `YYYY-MM-DD` slice (matching
//! the JS branch at `rel === null`).
//!
//! Distinct from `handlers::settings::humanize_relative_time` which
//! produces the long form ("4 minutes ago" / "Never") used on settings
//! status panels — keeping both lets data-ts cells stay narrow without
//! breaking the long-form callers.

/// Parse a SQLite `CURRENT_TIMESTAMP` string ("YYYY-MM-DD HH:MM:SS",
/// always UTC) into Unix epoch seconds. Returns `None` on any parse
/// failure (template falls back to displaying the raw string in that
/// case rather than a confusing "0s ago").
pub fn parse_sqlite_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' || (bytes[10] != b' ' && bytes[10] != b'T') {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: i64 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: i64 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let minute: i64 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let second: i64 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;

    // Days from civil date — algorithm from Howard Hinnant's date.h
    // (public-domain), written when no date library was worth pulling
    // in for this one conversion. Operates in proleptic Gregorian.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era * 146097 + doe - 719468;

    Some(days_since_epoch * 86400 + hour * 3600 + minute * 60 + second)
}

/// Short-form humanized relative time matching the JS renderer in
/// `static/js/base.js`. `now_ts` is the reference Unix-epoch seconds
/// (test-friendly: tests inject a fixed `now`; production passes
/// `current_unix_ts()`).
///
/// Returns `None` for timestamps older than 30 days — caller renders
/// a `YYYY-MM-DD` date slice instead (mirrors the JS branch).
pub fn humanize_short(unix_ts: i64, now_ts: i64) -> Option<String> {
    let delta = now_ts - unix_ts;
    let abs = delta.unsigned_abs() as i64;
    let future = delta < 0;
    if abs < 5 {
        return Some("just now".to_string());
    }
    let (value, unit) = if abs < 60 {
        (abs, "s")
    } else if abs < 3600 {
        ((abs + 30) / 60, "m") // round-half-up
    } else if abs < 86400 {
        ((abs + 1800) / 3600, "h")
    } else if abs < 30 * 86400 {
        ((abs + 43200) / 86400, "d")
    } else {
        return None;
    };
    Some(if future {
        format!("in {}{}", value, unit)
    } else {
        format!("{}{} ago", value, unit)
    })
}

/// One-shot helper for templates: parse a SQLite-shape timestamp and
/// return the short humanized form, or fall through to the first 10
/// chars (the date portion) for stale entries / parse failures. Never
/// panics; never returns empty for a non-empty input.
pub fn humanize_sqlite_short(raw: &str, now_ts: i64) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let Some(unix_ts) = parse_sqlite_utc(raw)
        && let Some(rel) = humanize_short(unix_ts, now_ts)
    {
        return rel;
    }
    // Fall back to date-only slice (10 chars covers "YYYY-MM-DD" exactly).
    if raw.len() >= 10 && raw.is_char_boundary(10) {
        raw[..10].to_string()
    } else {
        raw.to_string()
    }
}

/// Wrapper that reads `SystemTime::now()` and forwards. Templates call
/// this so a single template tag computes the relative time. Tests
/// should call [`humanize_short`] directly with an injected `now_ts`.
pub fn humanize_sqlite_short_now(raw: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    humanize_sqlite_short(raw, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sqlite_utc_handles_canonical_shape() {
        let ts = parse_sqlite_utc("2026-05-04 14:35:22").unwrap();
        // 2026-05-04 14:35:22 UTC = 1777987 ish
        // Quick sanity: should be near current era.
        assert!(ts > 1_700_000_000 && ts < 2_000_000_000);
    }

    #[test]
    fn parse_sqlite_utc_handles_iso_t_separator() {
        let ts1 = parse_sqlite_utc("2026-05-04 14:35:22").unwrap();
        let ts2 = parse_sqlite_utc("2026-05-04T14:35:22").unwrap();
        assert_eq!(ts1, ts2);
    }

    #[test]
    fn parse_sqlite_utc_rejects_short_or_malformed() {
        assert_eq!(parse_sqlite_utc(""), None);
        assert_eq!(parse_sqlite_utc("not a date"), None);
        assert_eq!(parse_sqlite_utc("2026-05-04"), None); // no time
        assert_eq!(parse_sqlite_utc("2026/05/04 14:35:22"), None); // wrong sep
    }

    #[test]
    fn humanize_short_under_5s_says_just_now() {
        let now: i64 = 1_700_000_000;
        assert_eq!(humanize_short(now, now).as_deref(), Some("just now"));
        assert_eq!(humanize_short(now - 4, now).as_deref(), Some("just now"));
    }

    #[test]
    fn humanize_short_seconds_to_minutes_to_hours_to_days() {
        let now: i64 = 1_700_000_000;
        assert_eq!(humanize_short(now - 30, now).as_deref(), Some("30s ago"));
        assert_eq!(humanize_short(now - 60, now).as_deref(), Some("1m ago"));
        assert_eq!(humanize_short(now - 3600, now).as_deref(), Some("1h ago"));
        assert_eq!(humanize_short(now - 86400, now).as_deref(), Some("1d ago"));
        assert_eq!(
            humanize_short(now - 5 * 86400, now).as_deref(),
            Some("5d ago")
        );
    }

    #[test]
    fn humanize_short_over_30d_returns_none() {
        let now: i64 = 1_700_000_000;
        assert!(humanize_short(now - 31 * 86400, now).is_none());
        assert!(humanize_short(now - 365 * 86400, now).is_none());
    }

    #[test]
    fn humanize_short_future_timestamps_use_in_prefix() {
        let now: i64 = 1_700_000_000;
        assert_eq!(humanize_short(now + 30, now).as_deref(), Some("in 30s"));
        assert_eq!(humanize_short(now + 60, now).as_deref(), Some("in 1m"));
    }

    #[test]
    fn humanize_sqlite_short_falls_back_to_date_for_stale_entries() {
        // Injected `now` 5 years past the timestamp -> falls into the
        // "older than 30 days" branch -> returns date slice.
        let now: i64 = 2_000_000_000; // 2033-ish
        assert_eq!(
            humanize_sqlite_short("2026-05-04 14:35:22", now),
            "2026-05-04"
        );
    }

    #[test]
    fn humanize_sqlite_short_falls_back_to_raw_for_unparseable_input() {
        let now: i64 = 1_700_000_000;
        assert_eq!(humanize_sqlite_short("garbage", now), "garbage");
        assert_eq!(humanize_sqlite_short("", now), "");
    }
}
