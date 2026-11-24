//! Dates and times over the `time` crate. Everything is UTC.
//!
//! Ryokan stores Unix seconds or SQLite `CURRENT_TIMESTAMP` text and
//! never reads a local zone. The text shapes live in this one file
//! because other code reads several of them back: the recycle bin's
//! date folders, SQLite timestamps, the iCal stamp. Each shape has a
//! test below pinning the exact string.
//!
//! `time` replaced `chrono` (RUSTSEC-2026-0543, unmaintained). It was
//! already in the build through reqwest's cookie store, so the swap
//! added no crate.
//!
//! Two kinds of function: the ones that format a clock value Ryokan
//! made itself return a `String`, since a date always has the parts
//! its description names. The ones that take stored seconds return an
//! `Option`, since a stored number can be out of range (a restored
//! database, an uploaded backup manifest).

use time::format_description::StaticFormatDescription;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, UtcDateTime};

/// `2026-08-05`. Recycle bin date folders, provider air dates, the
/// logs export file name.
const DATE: StaticFormatDescription = format_description!("[year]-[month]-[day]");

/// Reading side of [`DATE`]. A one-digit month or day is accepted
/// (`2026-8-5`), as chrono's `%m` / `%d` did. The year is four digits
/// (a sign may come first) and the text ends where the date does.
/// chrono also took a shorter year and whitespace before each number,
/// which nothing Ryokan reads has.
const DATE_LENIENT: StaticFormatDescription =
    format_description!("[year]-[month padding:none]-[day padding:none]");

/// `2026-08-05 09:03:07`, SQLite's `CURRENT_TIMESTAMP` form. Read
/// exactly: every field padded, one space in the middle. chrono also
/// took unpadded fields and any run of whitespace, which SQLite never
/// writes.
const SQLITE: StaticFormatDescription =
    format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

/// `2026-08-05 09:03 UTC`. Backup labels.
const MINUTE_UTC: StaticFormatDescription =
    format_description!("[year]-[month]-[day] [hour]:[minute] UTC");

/// `Aug 05, 2026 09:03`. An API key's created and last-used stamps.
const SHORT_STAMP: StaticFormatDescription =
    format_description!("[month repr:short] [day], [year] [hour]:[minute]");

/// `August 2026`. The calendar's month heading.
const MONTH_LABEL: StaticFormatDescription = format_description!("[month repr:long] [year]");

/// `Wednesday, Aug 5`. The calendar list's day heading.
const DAY_LABEL: StaticFormatDescription =
    format_description!("[weekday], [month repr:short] [day padding:none]");

/// `20260805T090307Z`, RFC 5545's UTC date-time.
const ICAL_UTC: StaticFormatDescription =
    format_description!("[year][month][day]T[hour][minute][second]Z");

const FORMATS: &str = "the value has every part its format description names";

/// The current instant.
pub fn now() -> UtcDateTime {
    UtcDateTime::now()
}

/// The current instant as Unix seconds.
pub fn now_unix() -> i64 {
    UtcDateTime::now().unix_timestamp()
}

/// The current UTC calendar date.
pub fn today() -> Date {
    UtcDateTime::now().date()
}

/// Unix seconds as an instant. `None` outside the years `time`
/// represents (-9999 to 9999).
pub fn from_unix(secs: i64) -> Option<UtcDateTime> {
    UtcDateTime::from_unix_timestamp(secs).ok()
}

/// `YYYY-MM-DD` as a date. The whole text has to be the date.
pub fn parse_date(text: &str) -> Option<Date> {
    Date::parse(text, DATE_LENIENT).ok()
}

/// A date as `YYYY-MM-DD`.
pub fn format_date(date: Date) -> String {
    date.format(DATE).expect(FORMATS)
}

/// A SQLite `CURRENT_TIMESTAMP` value (`YYYY-MM-DD HH:MM:SS`, UTC) as
/// an instant.
pub fn parse_sqlite(text: &str) -> Option<UtcDateTime> {
    UtcDateTime::parse(text, SQLITE).ok()
}

/// An instant in SQLite's `CURRENT_TIMESTAMP` form.
pub fn format_sqlite(at: UtcDateTime) -> String {
    at.format(SQLITE).expect(FORMATS)
}

/// Whole seconds from a SQLite `CURRENT_TIMESTAMP` value to now.
/// `None` when the text does not parse.
pub fn sqlite_age_secs(text: &str) -> Option<i64> {
    Some((now() - parse_sqlite(text)?).whole_seconds())
}

/// An instant as RFC 3339 with a `Z` offset: `2026-08-05T09:03:07Z`,
/// with the fraction of a second after the seconds when there is one.
/// Empty for a year outside 0 to 9999, which RFC 3339 cannot write;
/// no clock value reaches that.
pub fn rfc3339(at: UtcDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

/// Stored seconds as `2026-08-05 09:03 UTC`.
pub fn minute_label(secs: i64) -> Option<String> {
    from_unix(secs)?.format(MINUTE_UTC).ok()
}

/// Stored seconds as `Aug 05, 2026 09:03`.
pub fn short_stamp(secs: i64) -> Option<String> {
    from_unix(secs)?.format(SHORT_STAMP).ok()
}

/// A date's month as `August 2026`.
pub fn month_label(date: Date) -> String {
    date.format(MONTH_LABEL).expect(FORMATS)
}

/// Stored seconds as `Wednesday, Aug 5`.
pub fn day_label(secs: i64) -> Option<String> {
    from_unix(secs)?.format(DAY_LABEL).ok()
}

/// Stored seconds as RFC 5545's `20260805T090307Z`.
pub fn ical_utc(secs: i64) -> Option<String> {
    from_unix(secs)?.format(ICAL_UTC).ok()
}

#[cfg(test)]
mod tests {
    //! The expected strings are what chrono's `strftime` patterns
    //! produced for the same instant, so a difference here is a change
    //! in something Ryokan writes to disk, to a feed, or to another
    //! app. The instant has a one-digit day, hour, minute and second
    //! on purpose: that is where padding shows.
    //!
    //! When chrono was replaced, every description here was run beside
    //! its chrono pattern over 1970 to 2100 (about 21,000 instants, each
    //! weekday, hour, minute and second) with no difference. What did
    //! change is RFC 3339: chrono wrote the fraction of a second in
    //! three, six or nine digits where `time` drops trailing zeros
    //! (`.5` for `.500`), and chrono's `to_rfc3339` ended in `+00:00`
    //! where this ends in `Z`.
    use super::*;
    use time::macros::{date, utc_datetime};

    const AT: UtcDateTime = utc_datetime!(2026-08-05 09:03:07);

    #[test]
    fn date_keeps_the_recycle_folder_shape() {
        // chrono `%Y-%m-%d`.
        assert_eq!(format_date(date!(2026 - 08 - 05)), "2026-08-05");
        assert_eq!(format_date(date!(2026 - 12 - 31)), "2026-12-31");
        assert_eq!(parse_date("2026-08-05"), Some(date!(2026 - 08 - 05)));
        assert_eq!(parse_date(&format_date(AT.date())), Some(AT.date()));
    }

    #[test]
    fn date_reads_what_chrono_read() {
        // chrono's `%m` and `%d` took one digit or two.
        assert_eq!(parse_date("2026-8-5"), Some(date!(2026 - 08 - 05)));
        assert_eq!(parse_date("2026-08-5"), Some(date!(2026 - 08 - 05)));
        for bad in [
            "",
            "garbage",
            "not-a-date",
            "2026-13-01",
            "2026-02-30",
            "2026-08",
            "2026-08-05x",
            "2026-08-05T09:03:07",
            " 2026-08-05",
            "2026/08/05",
        ] {
            assert_eq!(parse_date(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn sqlite_timestamp_round_trips() {
        // chrono `%Y-%m-%d %H:%M:%S`, the form `CURRENT_TIMESTAMP` writes.
        assert_eq!(format_sqlite(AT), "2026-08-05 09:03:07");
        assert_eq!(parse_sqlite("2026-08-05 09:03:07"), Some(AT));
        assert_eq!(parse_sqlite(&format_sqlite(AT)), Some(AT));
        for bad in [
            "",
            "2026-08-05",
            "2026-08-05T09:03:07",
            "2026-08-05 09:03",
            "2026-08-05 09:03:07.123",
            "2026-08-05 25:03:07",
        ] {
            assert_eq!(parse_sqlite(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn sqlite_age_counts_whole_seconds_back_from_now() {
        let then = now() - time::SignedDuration::seconds(90);
        let age = sqlite_age_secs(&format_sqlite(then)).unwrap();
        assert!((90..=92).contains(&age), "{age}");
        assert_eq!(sqlite_age_secs("yesterday"), None);
    }

    #[test]
    fn rfc3339_ends_in_z() {
        assert_eq!(rfc3339(AT), "2026-08-05T09:03:07Z");
        // The boot stamp the test `AppState` carries, as Seerr reads it.
        assert_eq!(
            rfc3339(from_unix(1_704_067_200).unwrap()),
            "2024-01-01T00:00:00Z"
        );
        assert_eq!(
            rfc3339(utc_datetime!(2026-08-05 09:03:07.123456789)),
            "2026-08-05T09:03:07.123456789Z"
        );
    }

    #[test]
    fn labels_match_the_old_strftime_output() {
        let secs = AT.unix_timestamp();
        // `%Y-%m-%d %H:%M UTC`
        assert_eq!(minute_label(secs).unwrap(), "2026-08-05 09:03 UTC");
        // `%b %d, %Y %H:%M`: the day keeps its zero.
        assert_eq!(short_stamp(secs).unwrap(), "Aug 05, 2026 09:03");
        // `%B %Y`
        assert_eq!(month_label(AT.date()), "August 2026");
        // `%A, %b %-d`: the day loses its zero.
        assert_eq!(day_label(secs).unwrap(), "Wednesday, Aug 5");
        assert_eq!(
            day_label(utc_datetime!(2026-08-28 00:00).unix_timestamp()).unwrap(),
            "Friday, Aug 28"
        );
        // `%Y%m%dT%H%M%SZ`
        assert_eq!(ical_utc(secs).unwrap(), "20260805T090307Z");
        assert_eq!(ical_utc(0).unwrap(), "19700101T000000Z");
    }

    #[test]
    fn stored_seconds_out_of_range_are_none() {
        assert_eq!(from_unix(0), Some(UtcDateTime::UNIX_EPOCH));
        for secs in [i64::MAX, i64::MIN] {
            assert_eq!(from_unix(secs), None);
            assert_eq!(minute_label(secs), None);
            assert_eq!(short_stamp(secs), None);
            assert_eq!(day_label(secs), None);
            assert_eq!(ical_utc(secs), None);
        }
    }

    #[test]
    fn today_is_the_date_of_now() {
        let before = now().date();
        let today = today();
        let after = now().date();
        assert!(today == before || today == after);
        assert!((now_unix() - now().unix_timestamp()).abs() <= 1);
    }
}
