//! BYD's clock from the HTTP `Date` header (`pybyd/_transport.py`,
//! `SecureTransport._sync_clock_from_header`).
//!
//! pyBYD reads the `Date` header of every reply with Python's
//! `email.utils.parsedate_to_datetime` and, when BYD's clock is more than
//! [`SERVER_TIME_DRIFT_THRESHOLD_MS`] away from ours, stamps later requests
//! with BYD's time. [`sync_clock_from_header`] is that step; the parser is a
//! port of CPython 3.12's `email._parseaddr._parsedate_tz`, lenient forms
//! and all.
//!
//! One difference is deliberate: a date without a zone (or with `-0000`)
//! is a naive `datetime`, which Python reads in the host's local zone. This
//! crate reads no environment, so it takes such a date as UTC, as pyBYD
//! does on a host whose zone is UTC. BYD sends `GMT`.

use crate::aes::is_python_space;
use crate::coerce::py_is_digit;
use crate::constants::SERVER_TIME_DRIFT_THRESHOLD_MS;
use crate::pyjson::python_number_text;

/// Why a `Date` header could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockHeaderError {
    /// Python raised `OverflowError`: a number in the header does not fit
    /// what `datetime` and `timedelta` hold. pyBYD does not catch it, so
    /// the request fails.
    Overflow,
}

/// The server clock offset after a reply with `date_header`: pyBYD's
/// `_sync_clock_from_header`, given our wall clock `now_ms` and the offset
/// so far.
///
/// A missing, empty or unreadable header keeps `offset_ms`. A drift beyond
/// the threshold becomes the offset; a smaller one resets it to 0.
pub fn sync_clock_from_header(
    date_header: Option<&str>,
    now_ms: i64,
    offset_ms: i64,
) -> Result<i64, ClockHeaderError> {
    let Some(header) = date_header.filter(|header| !header.is_empty()) else {
        return Ok(offset_ms);
    };
    let Some(server_ms) = parse_http_date_ms(header)? else {
        return Ok(offset_ms);
    };
    let drift = i128::from(server_ms) - i128::from(now_ms);
    if drift.abs() > i128::from(SERVER_TIME_DRIFT_THRESHOLD_MS) {
        // Dates are years 1 to 9999, so the drift fits easily.
        Ok(drift as i64)
    } else {
        Ok(0)
    }
}

/// `int(parsedate_to_datetime(header).timestamp() * 1000)`, or `None` where
/// Python raises `ValueError` (which pyBYD ignores).
pub fn parse_http_date_ms(header: &str) -> Result<Option<i64>, ClockHeaderError> {
    let Some(parsed) = parsedate_tz(header) else {
        return Ok(None);
    };
    // `timezone(timedelta(seconds=tz))` is built before `datetime(...)` runs.
    let tz = match parsed.tz {
        None => 0,
        Some(Err(())) => return Err(ClockHeaderError::Overflow),
        Some(Ok(tz)) => {
            if !(-999_999_999..=999_999_999).contains(&tz.div_euclid(86_400)) {
                return Err(ClockHeaderError::Overflow);
            }
            if tz <= -86_400 || tz >= 86_400 {
                return Ok(None);
            }
            tz as i64
        }
    };
    // `datetime` reads each field as a C int first.
    let mut fields = [0i64; 6];
    let values = [
        parsed.year,
        Ok(parsed.month),
        parsed.day,
        parsed.hour,
        parsed.minute,
        parsed.second,
    ];
    for (slot, value) in fields.iter_mut().zip(values) {
        match value {
            Ok(value) if i32::try_from(value).is_ok() => *slot = value as i64,
            _ => return Err(ClockHeaderError::Overflow),
        }
    }
    let [year, month, day, hour, minute, second] = fields;
    if !(1..=9999).contains(&year)
        || !(1..=days_in_month(year, month)).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return Ok(None);
    }
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - tz;
    Ok(Some(seconds * 1000))
}

/// An integer as Python's `int(text)` reads it: `None` when it is not one,
/// `Err` when it is one beyond `i128`.
type PyInt = Result<i128, ()>;

fn py_int(text: &str) -> Option<PyInt> {
    let text = python_number_text(text)?;
    let digits = text.strip_prefix(['+', '-']).unwrap_or(&text);
    if digits.is_empty()
        || !digits.starts_with(|ch: char| ch.is_ascii_digit())
        || !digits.ends_with(|ch: char| ch.is_ascii_digit())
        || !digits.chars().all(|ch| ch.is_ascii_digit() || ch == '_')
        || digits.contains("__")
    {
        return None;
    }
    Some(text.replace('_', "").parse::<i128>().map_err(|_| ()))
}

/// What `_parsedate_tz` returns: the date's fields and the zone's offset in
/// seconds (`None` for no zone or `-0000`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedDate {
    year: PyInt,
    month: i128,
    day: PyInt,
    hour: PyInt,
    minute: PyInt,
    second: PyInt,
    tz: Option<PyInt>,
}

const MONTH_NAMES: [&str; 24] = [
    "jan",
    "feb",
    "mar",
    "apr",
    "may",
    "jun",
    "jul",
    "aug",
    "sep",
    "oct",
    "nov",
    "dec",
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

const TIMEZONES: [(&str, i128); 14] = [
    ("UT", 0),
    ("UTC", 0),
    ("GMT", 0),
    ("Z", 0),
    ("AST", -400),
    ("ADT", -300),
    ("EST", -500),
    ("EDT", -400),
    ("CST", -600),
    ("CDT", -500),
    ("MST", -700),
    ("MDT", -600),
    ("PST", -800),
    ("PDT", -700),
];

/// CPython's `email._parseaddr._parsedate_tz`.
pub(crate) fn parsedate_tz(data: &str) -> Option<ParsedDate> {
    let mut data: Vec<String> = data
        .split(is_python_space)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if data.is_empty() {
        return None;
    }
    if data[0].ends_with(',') || DAY_NAMES.contains(&data[0].to_lowercase().as_str()) {
        data.remove(0);
    } else if let Some(index) = data[0].rfind(',') {
        data[0] = data[0][index + 1..].to_owned();
    }
    if data.len() == 3 {
        let stuff: Vec<String> = data[0].split('-').map(str::to_owned).collect();
        if stuff.len() == 3 {
            data = stuff.into_iter().chain(data[1..].iter().cloned()).collect();
        }
    }
    if data.len() == 4 {
        let s = data[3].clone();
        let index = s.find('+').or_else(|| s.find('-'));
        match index {
            Some(index) if index > 0 => {
                data.truncate(3);
                data.push(s[..index].to_owned());
                data.push(s[index..].to_owned());
            }
            _ => data.push(String::new()),
        }
    }
    if data.len() < 5 {
        return None;
    }
    data.truncate(5);
    let [mut dd, mut mm, mut yy, mut tm, mut tz]: [String; 5] = data.try_into().ok()?;
    if dd.is_empty() || mm.is_empty() || yy.is_empty() {
        return None;
    }
    mm = mm.to_lowercase();
    if !MONTH_NAMES.contains(&mm.as_str()) {
        let lowered = dd.to_lowercase();
        dd = mm;
        mm = lowered;
        if !MONTH_NAMES.contains(&mm.as_str()) {
            return None;
        }
    }
    let mut month = MONTH_NAMES.iter().position(|name| *name == mm)? as i128 + 1;
    if month > 12 {
        month -= 12;
    }
    if dd.ends_with(',') {
        dd.pop();
    }
    if yy.find(':').is_some_and(|index| index > 0) {
        std::mem::swap(&mut yy, &mut tm);
    }
    if yy.ends_with(',') {
        yy.pop();
        if yy.is_empty() {
            return None;
        }
    }
    if !yy.chars().next().is_some_and(py_is_digit) {
        std::mem::swap(&mut yy, &mut tz);
    }
    if tm.ends_with(',') {
        tm.pop();
    }
    let parts: Vec<&str> = tm.split(':').collect();
    let (thh, tmm, tss) = match parts.as_slice() {
        [hh, mm] => (*hh, *mm, "0"),
        [hh, mm, ss] => (*hh, *mm, *ss),
        [single] if single.contains('.') => {
            let dotted: Vec<&str> = single.split('.').collect();
            match dotted.as_slice() {
                [hh, mm] => (*hh, *mm, "0"),
                [hh, mm, ss] => (*hh, *mm, *ss),
                _ => return None,
            }
        }
        _ => return None,
    };
    let mut year = py_int(&yy)?;
    let day = py_int(&dd)?;
    let hour = py_int(thh)?;
    let minute = py_int(tmm)?;
    let second = py_int(tss)?;
    if let Ok(value) = year
        && value < 100
    {
        year = Ok(if value > 68 {
            value + 1900
        } else {
            value + 2000
        });
    }
    let tz = tz.to_uppercase();
    let mut offset = match TIMEZONES.iter().find(|(name, _)| *name == tz) {
        Some(&(_, value)) => Some(Ok(value)),
        None => py_int(&tz),
    };
    // Only a numeric zone can start with '-': `-0000` means "no zone".
    if offset == Some(Ok(0)) && tz.starts_with('-') {
        offset = None;
    }
    let offset = offset.map(|offset| {
        offset.and_then(|value| {
            if value == 0 {
                return Ok(0);
            }
            let (sign, magnitude) = if value < 0 {
                (-1, value.checked_neg().ok_or(())?)
            } else {
                (1, value)
            };
            (magnitude / 100)
                .checked_mul(3600)
                .and_then(|hours| hours.checked_add((magnitude % 100) * 60))
                .map(|seconds| sign * seconds)
                .ok_or(())
        })
    });
    Some(ParsedDate {
        year,
        month,
        day,
        hour,
        minute,
        second,
        tz: offset,
    })
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_795_000_000_000;

    fn ms(header: &str) -> Option<i64> {
        parse_http_date_ms(header).unwrap()
    }

    #[test]
    fn reads_the_forms_python_reads() {
        let gmt = ms("Sat, 22 Nov 2026 10:00:00 GMT").unwrap();
        assert_eq!(gmt, 1_795_341_600_000);
        assert_eq!(ms("22 Nov 2026 10:00:00 +0000"), Some(gmt));
        assert_eq!(ms("22 Nov 2026 12:00:00 +0200"), Some(gmt));
        assert_eq!(ms("22 Nov 2026 05:00:00 EST"), Some(gmt));
        assert_eq!(ms("22 Nov 2026 10:00:00 -0000"), Some(gmt));
        assert_eq!(ms("22 Nov 2026 10:00:00"), Some(gmt));
        assert_eq!(ms("Saturday, 22-Nov-26 10:00:00 GMT"), Some(gmt));
        assert_eq!(ms("Nov 22 2026 10:00 GMT"), Some(gmt));
        assert_eq!(ms("22 Nov 2026 10.00.00 GMT"), Some(gmt));
        assert_eq!(ms("22 november 2026 10:00:00+0000"), Some(gmt));
        assert_eq!(ms("22 Nov 70 00:00:00 GMT"), Some(325 * 86_400_000));
        assert_eq!(ms("1 Jan 1970 00:00:00 GMT"), Some(0));
        // Two-digit (and shorter) years are 1969-1999 or 2000-2068.
        assert_eq!(ms("1 Jan 1 00:00:00 GMT"), Some(978_307_200_000));
        assert_eq!(ms("1 Jan 100 00:00:00 GMT"), Some(-59_011_459_200_000));
    }

    #[test]
    fn refuses_what_python_refuses() {
        for header in [
            "",
            "   ",
            "garbage",
            "22 Foo 2026 10:00:00 GMT",
            "31 Feb 2026 10:00:00 GMT",
            "22 Nov 2026 24:00:00 GMT",
            "22 Nov 10000 10:00:00 GMT",
            "22 Nov 2026 10:00:00 +2400",
            "22 Nov 2026 10 GMT",
            "22 Nov 2026 a:b:c GMT",
        ] {
            assert_eq!(ms(header), None, "{header}");
        }
        for header in [
            "22 Nov 99999999999999999999 10:00:00 GMT",
            "22 Nov 2026 10:00:00 +99999999999999",
            "22 Nov 2026 99999999999:00:00 GMT",
            "22 Nov 2026 10:00:00 +999999999999999999999999999999999999999999",
        ] {
            assert_eq!(
                parse_http_date_ms(header),
                Err(ClockHeaderError::Overflow),
                "{header}"
            );
        }
    }

    #[test]
    fn drift_beyond_the_threshold_becomes_the_offset() {
        let header = "Sat, 22 Nov 2026 10:00:00 GMT";
        let server = ms(header).unwrap();
        assert_eq!(
            sync_clock_from_header(Some(header), server - 60_000, 0),
            Ok(60_000)
        );
        assert_eq!(
            sync_clock_from_header(Some(header), server + 31_000, 0),
            Ok(-31_000)
        );
        assert_eq!(
            sync_clock_from_header(Some(header), server - 30_000, 5),
            Ok(0)
        );
        assert_eq!(sync_clock_from_header(None, NOW, 7), Ok(7));
        assert_eq!(sync_clock_from_header(Some(""), NOW, 7), Ok(7));
        assert_eq!(sync_clock_from_header(Some("nonsense"), NOW, 7), Ok(7));
    }
}
