//! Time parsing and formatting: ISO-8601 timestamps in, Unix seconds inside, local or UTC text out.
//!
//! Local time comes from the C library (`localtime_r`), so it honours `TZ` and the system zone
//! database without a Rust time-zone dependency.

/// Unix seconds for an ISO-8601 / RFC 3339 timestamp such as `2026-10-11T19:00:00.123+00:00` or
/// `2026-10-11T19:00:00Z`. Fractional seconds are truncated. A missing offset is read as UTC.
pub fn parse_iso8601(text: &str) -> Option<i64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    if bytes[10] != b'T' && bytes[10] != b't' && bytes[10] != b' ' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let year = num(0..4)?;
    let month = u32::try_from(num(5..7)?).ok()?;
    let day = u32::try_from(num(8..10)?).ok()?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(stripped) = rest.strip_prefix('.') {
        let digits = stripped.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &stripped[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let body = &rest[1..];
            let (h, m) = match body.len() {
                5 if body.as_bytes()[2] == b':' => (&body[0..2], &body[3..5]),
                4 => (&body[0..2], &body[2..4]),
                2 => (&body[0..2], "00"),
                _ => return None,
            };
            let h: i64 = h.parse().ok()?;
            let m: i64 = m.parse().ok()?;
            sign * (h * 3_600 + m * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds.
pub fn rfc3339_utc(seconds: i64) -> String {
    let (y, mo, d, h, mi, s) = utc_parts(seconds);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Civil date and time `(year, month, day, hour, minute, second)` for Unix seconds in UTC.
pub fn utc_parts(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        u32::try_from(day_seconds / 3_600).unwrap_or(0),
        u32::try_from((day_seconds % 3_600) / 60).unwrap_or(0),
        u32::try_from(day_seconds % 60).unwrap_or(0),
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Local wall-clock parts `(month, day, hour, minute, zone abbreviation)` from the C library.
fn local_parts(seconds: i64) -> Option<(u32, u32, u32, u32, String)> {
    #[allow(clippy::useless_conversion)] // time_t is narrower than i64 on 32-bit targets
    let t: libc::time_t = seconds.try_into().ok()?;
    // SAFETY: `tm` is plain old data; zeroed is a valid initial value and localtime_r fills it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    let result = unsafe { libc::localtime_r(&t, &mut tm) };
    if result.is_null() {
        return None;
    }
    let zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        // SAFETY: tm_zone points at a NUL-terminated static or tzset-owned string.
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
            .to_string_lossy()
            .into_owned()
    };
    Some((
        u32::try_from(tm.tm_mon + 1).ok()?,
        u32::try_from(tm.tm_mday).ok()?,
        u32::try_from(tm.tm_hour).ok()?,
        u32::try_from(tm.tm_min).ok()?,
        zone,
    ))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A reset time as people read it: `8:10am PDT` when it is within a day of `now`, otherwise
/// `Oct 11 12:00pm PDT`. Falls back to UTC when the local zone cannot be read.
pub fn human_local(at: i64, now: i64) -> String {
    let Some((month, day, hour, minute, zone)) = local_parts(at) else {
        return rfc3339_utc(at);
    };
    let (h12, ampm) = match hour {
        0 => (12, "am"),
        1..=11 => (hour, "am"),
        12 => (12, "pm"),
        _ => (hour - 12, "pm"),
    };
    let clock = format!("{h12}:{minute:02}{ampm}");
    let zone = if zone.is_empty() {
        String::new()
    } else {
        format!(" {zone}")
    };
    if (at - now).abs() < 20 * 3_600 {
        format!("{clock}{zone}")
    } else {
        let name = MONTHS.get(month as usize - 1).copied().unwrap_or("?");
        format!("{name} {day} {clock}{zone}")
    }
}

/// A duration as `45s`, `12m`, `3h05m` or `2d04h`. Negative durations are written as `0s`.
pub fn human_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60)
    } else {
        format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3_600)
    }
}

/// Parse a duration such as `900`, `90s`, `15m`, `3h` or `1d` into seconds.
pub fn parse_duration(text: &str) -> Option<i64> {
    let text = text.trim();
    let (digits, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let value: i64 = digits.parse().ok()?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    value.checked_mul(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_iso_forms() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2026-10-11T19:00:00Z"), Some(1_791_745_200));
        assert_eq!(
            parse_iso8601("2026-10-11T19:00:00.123456+00:00"),
            Some(1_791_745_200)
        );
        assert_eq!(
            parse_iso8601("2026-10-11T12:00:00-07:00"),
            Some(1_791_745_200)
        );
        assert_eq!(
            parse_iso8601("2026-10-11T21:00:00+0200"),
            Some(1_791_745_200)
        );
        assert_eq!(parse_iso8601("2026-10-11"), None);
        assert_eq!(parse_iso8601("not a time at all, no"), None);
        assert_eq!(parse_iso8601("2026-13-11T19:00:00Z"), None);
    }

    #[test]
    fn round_trips_utc() {
        for t in [0, 1_791_745_200, 951_782_400, 4_102_444_800] {
            assert_eq!(parse_iso8601(&rfc3339_utc(t)), Some(t));
        }
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(-5), "0s");
        assert_eq!(human_duration(59), "59s");
        assert_eq!(human_duration(900), "15m");
        assert_eq!(human_duration(3_600 + 300), "1h05m");
        assert_eq!(human_duration(2 * 86_400 + 4 * 3_600), "2d04h");
        assert_eq!(parse_duration("900"), Some(900));
        assert_eq!(parse_duration("15m"), Some(900));
        assert_eq!(parse_duration("3h"), Some(10_800));
        assert_eq!(parse_duration("1d"), Some(86_400));
        assert_eq!(parse_duration("1w"), None);
        assert_eq!(parse_duration(""), None);
    }
}
