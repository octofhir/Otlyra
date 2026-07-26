//! The date on a cookie, read the way they are actually written.
//!
//! `Expires` is nominally an HTTP date and in practice is not. Servers send
//! `Sun, 06 Nov 1994 08:49:37 GMT`, `Sunday, 06-Nov-94 08:49:37 GMT` and
//! `Sun Nov  6 08:49:37 1994`, and worse — a two-digit year, a missing comma, a
//! zone nobody reads. RFC 6265 §5.1.1 answers this with an algorithm that does
//! not parse a format at all: it splits the string on punctuation and asks each
//! piece in turn whether it looks like a time, a day, a month or a year. A
//! stricter reader would drop cookies a browser keeps, which reads to a person as
//! being signed out.
//!
//! Written here rather than taken from a date crate because this *is* the
//! difference: an HTTP-date crate implements the three formats the specification
//! names, which is the reader this algorithm exists to replace.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The month names the algorithm knows, in order, matched on the first three
/// characters and case-insensitively.
const MONTHS: [&[u8; 3]; 12] = [
    b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov", b"dec",
];

/// The characters that separate one piece of a date from the next.
///
/// `%x09 / %x20-2F / %x3B-40 / %x5B-60 / %x7B-7E`, which is the specification's
/// own set. Note what is *not* in it: `:` is `%x3A`, one below the third range,
/// because it holds a time together.
fn is_delimiter(byte: u8) -> bool {
    byte == 0x09
        || (0x20..=0x2F).contains(&byte)
        || (0x3B..=0x40).contains(&byte)
        || (0x5B..=0x60).contains(&byte)
        || (0x7B..=0x7E).contains(&byte)
}

/// Read between `min` and `max` digits off the front, and say what is left.
///
/// `None` when there are fewer than `min`. Reading no more than `max` is what
/// makes the productions exclusive: a four-digit year cannot be read as a
/// two-digit day, because the two digits would be followed by another digit and
/// every production requires a non-digit after it.
fn digits(input: &[u8], min: usize, max: usize) -> Option<(u32, &[u8])> {
    let taken = input
        .iter()
        .take(max)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if taken < min {
        return None;
    }
    let value = input[..taken]
        .iter()
        .fold(0u32, |value, byte| value * 10 + u32::from(byte - b'0'));
    Some((value, &input[taken..]))
}

/// Whether what follows a production is allowed to follow it: anything that is
/// not a digit, or nothing at all.
fn ends_here(rest: &[u8]) -> bool {
    rest.first().is_none_or(|byte| !byte.is_ascii_digit())
}

/// `1*2DIGIT ":" 1*2DIGIT ":" 1*2DIGIT`, then anything that is not a digit.
fn read_time(token: &[u8]) -> Option<(u32, u32, u32)> {
    let (hour, rest) = digits(token, 1, 2)?;
    let (minute, rest) = digits(rest.strip_prefix(b":")?, 1, 2)?;
    let (second, rest) = digits(rest.strip_prefix(b":")?, 1, 2)?;
    ends_here(rest).then_some((hour, minute, second))
}

/// `1*2DIGIT`, then anything that is not a digit — so `31st` is the thirty-first.
fn read_day(token: &[u8]) -> Option<u32> {
    let (day, rest) = digits(token, 1, 2)?;
    ends_here(rest).then_some(day)
}

/// The first three characters against a month name, and the rest ignored — so
/// `November` and `Nov` are the same month.
fn read_month(token: &[u8]) -> Option<u32> {
    let head: [u8; 3] = token.get(..3)?.try_into().ok()?;
    let head = head.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|month| head == **month)
        .map(|index| index as u32 + 1)
}

/// `2*4DIGIT`, then anything that is not a digit.
fn read_year(token: &[u8]) -> Option<u32> {
    let (year, rest) = digits(token, 2, 4)?;
    ends_here(rest).then_some(year)
}

/// Days from 1970-01-01 to `year-month-day`, proleptic Gregorian.
///
/// Howard Hinnant's `days_from_civil`, which is exact for every year the
/// algorithm admits and needs no table. A day outside its month — `31 Feb` — runs
/// over into the next one rather than failing, which is what the specification
/// asks for: it validates the day against 1–31 and nothing finer.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // March-based years, so a leap day falls at the end and never inside.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Read an `Expires` value.
///
/// `None` when the string does not carry all four of a time, a day, a month and a
/// year, or carries one out of range — which the specification says is a cookie
/// whose `Expires` attribute is ignored, leaving it a session cookie, rather than
/// a cookie that is dropped.
pub fn parse(value: &str) -> Option<SystemTime> {
    let bytes = value.as_bytes();
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);

    for token in bytes.split(|&byte| is_delimiter(byte)) {
        if token.is_empty() {
            continue;
        }
        // In this order, and each token answers at most one question: the
        // specification's own step 2, and the reason `1994` in a string that
        // already has a day is a year rather than a second day.
        if time.is_none()
            && let Some(read) = read_time(token)
        {
            time = Some(read);
        } else if day.is_none()
            && let Some(read) = read_day(token)
        {
            day = Some(read);
        } else if month.is_none()
            && let Some(read) = read_month(token)
        {
            month = Some(read);
        } else if year.is_none()
            && let Some(read) = read_year(token)
        {
            year = Some(read);
        }
    }

    let (hour, minute, second) = time?;
    let day = day?;
    let month = month?;
    let year = match year? {
        // A two-digit year is within a century of now, and the specification
        // fixes the split rather than leaving it to the reader's clock.
        year @ 70..=99 => year + 1900,
        year @ 0..=69 => year + 2000,
        year => year,
    };

    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let days = days_from_civil(i64::from(year), i64::from(month), i64::from(day));
    let seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);

    // A date before 1970 is how a server deletes a cookie, so it has to be
    // representable rather than clamped: clamping it to the epoch would make it
    // an expiry in the past all the same, but only by accident.
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds since the epoch, negative before it, so a case reads as a number
    /// rather than as a construction.
    fn epoch_seconds(time: SystemTime) -> i64 {
        match time.duration_since(UNIX_EPOCH) {
            Ok(since) => since.as_secs() as i64,
            Err(before) => -(before.duration().as_secs() as i64),
        }
    }

    fn seconds(value: &str) -> Option<i64> {
        parse(value).map(epoch_seconds)
    }

    /// The three formats HTTP names, which are the three a server sends, and all
    /// of them the same instant.
    #[test]
    fn the_three_http_dates_are_one_instant() {
        let expected = Some(784_111_777);
        assert_eq!(seconds("Sun, 06 Nov 1994 08:49:37 GMT"), expected);
        assert_eq!(seconds("Sunday, 06-Nov-94 08:49:37 GMT"), expected);
        assert_eq!(seconds("Sun Nov  6 08:49:37 1994"), expected);
    }

    /// And the shapes that are not any of the three, which is why this is an
    /// algorithm and not a format.
    #[test]
    fn the_shapes_no_format_covers_are_read_anyway() {
        let expected = Some(784_111_777);
        // No comma, no zone, the pieces in another order.
        assert_eq!(seconds("06 Nov 1994 08:49:37"), expected);
        assert_eq!(seconds("1994 Nov 6 08:49:37"), expected);
        // A zone that is not GMT is not read — the specification says every date
        // is UTC — and an ordinal suffix on the day is punctuation to skip.
        assert_eq!(seconds("Sun, 06 Nov 1994 08:49:37 EST"), expected);
        assert_eq!(seconds("Sun, 6th Nov 1994 08:49:37 GMT"), expected);
        // A month spelled out, and a lowercase one.
        assert_eq!(seconds("Sun, 06 november 1994 08:49:37 GMT"), expected);
    }

    /// The century a two-digit year falls in is fixed by the specification, not
    /// by the clock.
    #[test]
    fn a_two_digit_year_splits_at_seventy() {
        // 69 is 2069 and 70 is 1970 — the two sides of the split.
        assert_eq!(seconds("Wed, 01 Jan 69 00:00:00 GMT"), Some(3_124_224_000));
        assert_eq!(seconds("Thu, 01 Jan 70 00:00:00 GMT"), Some(0));
    }

    /// How a server deletes a cookie: an expiry in the past, which is usually
    /// before the epoch and has to be representable rather than clamped.
    #[test]
    fn a_date_before_the_epoch_is_kept_as_one() {
        assert_eq!(seconds("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(seconds("Thu, 01-Jan-1970 00:00:01 GMT"), Some(1));
        assert_eq!(
            seconds("Mon, 01 Jan 1601 00:00:00 GMT"),
            Some(-11_644_473_600)
        );
    }

    /// A leap day, and the day after it, because the arithmetic is the one place
    /// this file can be wrong without looking wrong.
    #[test]
    fn a_leap_day_lands_on_the_leap_day() {
        assert_eq!(
            seconds("Sat, 29 Feb 2020 00:00:00 GMT"),
            Some(1_582_934_400)
        );
        assert_eq!(
            seconds("Sun, 01 Mar 2020 00:00:00 GMT"),
            Some(1_583_020_800)
        );
        // 2000 is a leap year and 1900 is not, which is the rule a table gets
        // wrong.
        assert_eq!(seconds("Tue, 29 Feb 2000 00:00:00 GMT"), Some(951_782_400));
        assert_eq!(
            seconds("Wed, 01 Mar 1900 00:00:00 GMT"),
            Some(-2_203_891_200)
        );
    }

    /// A piece missing or out of range is a date that is not read — which leaves
    /// the cookie a session cookie rather than dropping it.
    #[test]
    fn an_incomplete_or_impossible_date_is_no_date() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("Sun, 06 Nov 1994"), None, "no time");
        assert_eq!(parse("06 Nov 08:49:37"), None, "no year");
        assert_eq!(parse("Sun, 06 1994 08:49:37"), None, "no month");
        assert_eq!(parse("Nov 1994 08:49:37"), None, "no day");
        assert_eq!(parse("Sun, 32 Nov 1994 08:49:37 GMT"), None, "no such day");
        assert_eq!(parse("Sun, 06 Nov 1994 24:49:37 GMT"), None, "no such hour");
        assert_eq!(
            parse("Sun, 06 Nov 1994 08:60:37 GMT"),
            None,
            "no such minute"
        );
        assert_eq!(
            parse("Sun, 06 Nov 1994 08:49:60 GMT"),
            None,
            "no such second"
        );
        assert_eq!(parse("Sun, 06 Nov 1600 08:49:37 GMT"), None, "before 1601");
    }

    /// Each token answers one question, so a five-digit run is neither a year nor
    /// a day and a second number is not a second day.
    #[test]
    fn a_token_is_read_once_and_a_digit_run_is_read_whole() {
        // `12345` is five digits: too many for a year, and the two-digit reading
        // is refused because a digit follows it.
        assert_eq!(parse("Sun, 06 Nov 12345 08:49:37 GMT"), None);
        // `06` is the day, so the next number that fits a year is the year, and a
        // third one is ignored rather than replacing it.
        assert_eq!(
            seconds("Sun, 06 Nov 1994 08:49:37 GMT 2020"),
            Some(784_111_777)
        );
    }

    /// A colon is not a delimiter, which is the whole reason a time survives the
    /// split.
    #[test]
    fn the_delimiter_set_holds_a_time_together() {
        assert!(is_delimiter(b' '));
        assert!(is_delimiter(b','));
        assert!(is_delimiter(b'-'));
        assert!(is_delimiter(b'/'));
        assert!(is_delimiter(b'\t'));
        assert!(!is_delimiter(b':'));
        assert!(!is_delimiter(b'0'));
        assert!(!is_delimiter(b'a'));
    }
}
