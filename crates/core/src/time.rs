//! UTC times as Unix seconds, without a date library: parsing what rclone and models send,
//! and printing what tools show.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn digits(text: &str, at: usize, len: usize) -> Option<u32> {
    let part = text.get(at..at + len)?;
    if part.bytes().all(|b| b.is_ascii_digit()) {
        part.parse().ok()
    } else {
        None
    }
}

/// `2026-10-29T12:00:00Z`, with optional fractional seconds and a `±HH:MM` offset instead of
/// `Z` (rclone's local backend reports local time with an offset).
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let text = text.trim();
    let date = parse_ymd(text)?;
    let sep = text.as_bytes().get(10)?;
    if !matches!(sep, b'T' | b't' | b' ') {
        return None;
    }
    let (hour, minute) = (digits(text, 11, 2)?, digits(text, 14, 2)?);
    if text.as_bytes().get(13) != Some(&b':') {
        return None;
    }
    let mut rest = &text[16..];
    let mut second = 0;
    if let Some(after) = rest.strip_prefix(':') {
        second = digits(after, 0, 2)?;
        rest = &after[2..];
        if let Some(fraction) = rest.strip_prefix('.') {
            let end = fraction
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(fraction.len());
            rest = &fraction[end..];
        }
    }
    let offset = match rest {
        "Z" | "z" | "" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (h, m) = (digits(rest, 1, 2)?, digits(rest, 4, 2)?);
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            sign * i64::from(h * 3600 + m * 60)
        }
    };
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(date + i64::from(hour * 3600 + minute * 60 + second) - offset)
}

/// The start (UTC) of a `YYYY-MM-DD` date at the beginning of `text`.
fn parse_ymd(text: &str) -> Option<i64> {
    let (year, month, day) = (
        digits(text, 0, 4)?,
        digits(text, 5, 2)?,
        digits(text, 8, 2)?,
    );
    let bytes = text.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
    {
        return None;
    }
    Some(days_from_civil(i64::from(year), month, day) * 86_400)
}

/// A date a model might send: RFC 3339, `YYYY-MM-DD` (midnight UTC) or `YYYY-MM-DD HH:MM`.
pub fn parse_date(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.len() == 10 {
        return parse_ymd(text);
    }
    parse_rfc3339(text)
}

/// A length of time such as `30m`, `24h`, `7d` or `2w`; a bare number is seconds.
pub fn parse_age(text: &str) -> Option<i64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: i64 = number.parse().ok()?;
    let scale = match unit.trim() {
        "" | "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return None,
    };
    number.checked_mul(scale)
}

/// `2026-10-29T12:00:00Z`.
pub fn rfc3339(ts: i64) -> String {
    let (y, mo, d) = civil_from_days(ts.div_euclid(86_400));
    let s = ts.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// `14:05Z` when `ts` is on the same UTC day as `now`, else `2026-10-30 14:05Z`.
pub fn short(ts: i64, now: i64) -> String {
    let s = ts.rem_euclid(86_400);
    let clock = format!("{:02}:{:02}Z", s / 3600, s / 60 % 60);
    if ts.div_euclid(86_400) == now.div_euclid(86_400) {
        clock
    } else {
        let (y, mo, d) = civil_from_days(ts.div_euclid(86_400));
        format!("{y:04}-{mo:02}-{d:02} {clock}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for ts in [0, 86_399, 951_782_400, 1_793_000_000, -86_400] {
            assert_eq!(parse_rfc3339(&rfc3339(ts)), Some(ts), "{ts}");
        }
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn parses_what_rclone_and_models_send() {
        let base = parse_rfc3339("2026-10-29T12:00:00Z").unwrap();
        assert_eq!(parse_rfc3339("2026-10-29T12:00:00.123456789Z"), Some(base));
        assert_eq!(parse_rfc3339("2026-10-29T13:00:00+01:00"), Some(base));
        assert_eq!(parse_rfc3339("2026-10-29T07:30:00.5-04:30"), Some(base));
        assert_eq!(parse_date("2026-10-29 12:00"), Some(base));
        assert_eq!(parse_date("2026-10-29"), Some(base - 12 * 3600));
        for bad in [
            "",
            "2026-13-01",
            "2026-10-29T25:00:00Z",
            "yesterday",
            "2026-10-29T12:00:00+0100",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn ages_and_short_times() {
        assert_eq!(parse_age("7d"), Some(7 * 86_400));
        assert_eq!(parse_age("24h"), Some(86_400));
        assert_eq!(parse_age("90"), Some(90));
        assert_eq!(parse_age("1y"), None);
        let t = parse_rfc3339("2026-10-31T14:05:59Z").unwrap();
        assert_eq!(short(t, t + 60), "14:05Z");
        assert_eq!(short(t, t + 86_400), "2026-10-31 14:05Z");
    }
}
