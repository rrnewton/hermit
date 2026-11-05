//! Time formatting for warnings: US Eastern by default ("2:31 AM ET"), UTC on request.
//!
//! The US Eastern rules are computed directly (daylight time from the second Sunday of March at
//! 07:00 UTC to the first Sunday of November at 06:00 UTC, the rule in force since 2007) so the
//! binary does not depend on a system time-zone database.

use serde::{Deserialize, Serialize};

/// Which clock the human-readable messages use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DisplayTz {
    /// US Eastern time, written like `2:31 AM ET`.
    #[default]
    #[serde(rename = "US-Eastern")]
    UsEastern,
    /// UTC, written like `06:31 UTC`.
    #[serde(rename = "UTC")]
    Utc,
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
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Day of week for a day count since 1970-01-01, with 0 = Sunday.
fn weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

/// Day-of-month of the `nth` (1-based) Sunday of `month` in `year`.
fn nth_sunday(year: i64, month: u32, nth: u32) -> u32 {
    let first = days_from_civil(year, month, 1);
    let offset = (7 - weekday(first)) % 7;
    1 + u32::try_from(offset).unwrap_or(0) + 7 * (nth - 1)
}

/// UTC offset of US Eastern time at Unix `seconds`, in seconds (-14400 or -18000).
pub fn us_eastern_offset(seconds: i64) -> i64 {
    let (year, _, _, _, _, _) = utc_parts(seconds);
    let dst_start = days_from_civil(year, 3, nth_sunday(year, 3, 2)) * 86_400 + 7 * 3_600;
    let dst_end = days_from_civil(year, 11, nth_sunday(year, 11, 1)) * 86_400 + 6 * 3_600;
    if seconds >= dst_start && seconds < dst_end {
        -4 * 3_600
    } else {
        -5 * 3_600
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Format `at` for a human, prefixing the date when it falls on a different local day than `now`.
pub fn human(at: f64, now: f64, tz: DisplayTz) -> String {
    // Past 9999-12-31 the calendar arithmetic below could overflow; no such time is shown.
    if !at.is_finite() || at.abs() > 253_402_300_799.0 {
        return "a time too far off to show".to_string();
    }
    let at_s = at.round() as i64;
    let now_s = now.round() as i64;
    match tz {
        DisplayTz::UsEastern => {
            let local = at_s + us_eastern_offset(at_s);
            let local_now = now_s + us_eastern_offset(now_s);
            let (_, month, day, hour, minute, _) = utc_parts(local);
            let (hour12, meridiem) = match hour {
                0 => (12, "AM"),
                1..=11 => (hour, "AM"),
                12 => (12, "PM"),
                _ => (hour - 12, "PM"),
            };
            let time = format!("{hour12}:{minute:02} {meridiem} ET");
            if local.div_euclid(86_400) == local_now.div_euclid(86_400) {
                time
            } else {
                format!(
                    "{} {day} {time}",
                    MONTHS[(month as usize).saturating_sub(1) % 12]
                )
            }
        }
        DisplayTz::Utc => {
            let (_, month, day, hour, minute, _) = utc_parts(at_s);
            let time = format!("{hour:02}:{minute:02} UTC");
            if at_s.div_euclid(86_400) == now_s.div_euclid(86_400) {
                time
            } else {
                format!(
                    "{} {day} {time}",
                    MONTHS[(month as usize).saturating_sub(1) % 12]
                )
            }
        }
    }
}

/// RFC 3339 UTC timestamp with millisecond precision, for audit records.
pub fn rfc3339_utc(at: f64) -> String {
    let whole = at.floor() as i64;
    let millis = ((at - at.floor()) * 1000.0).floor() as u32;
    let (year, month, day, hour, minute, second) = utc_parts(whole);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Local US Eastern wall time with seconds, for audit records (`2026-10-05 02:31:07 ET`).
pub fn eastern_stamp(at: f64) -> String {
    let whole = at.floor() as i64;
    let local = whole + us_eastern_offset(whole);
    let (year, month, day, hour, minute, second) = utc_parts(local);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} ET")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> f64 {
        (days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60) as f64
    }

    #[test]
    fn civil_round_trip() {
        for days in [-1_000_000, -1, 0, 1, 20_000, 20_730, 2_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(utc_parts(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn eastern_daylight_boundaries_2026() {
        // 2026: DST starts Sunday March 8 at 07:00 UTC and ends Sunday November 1 at 06:00 UTC.
        assert_eq!(nth_sunday(2026, 3, 2), 8);
        assert_eq!(nth_sunday(2026, 11, 1), 1);
        let before = unix(2026, 3, 8, 6, 59) as i64;
        let after = unix(2026, 3, 8, 7, 0) as i64;
        assert_eq!(us_eastern_offset(before), -18_000);
        assert_eq!(us_eastern_offset(after), -14_400);
        let end_before = unix(2026, 11, 1, 5, 59) as i64;
        let end_after = unix(2026, 11, 1, 6, 0) as i64;
        assert_eq!(us_eastern_offset(end_before), -14_400);
        assert_eq!(us_eastern_offset(end_after), -18_000);
    }

    #[test]
    fn human_eastern_formats() {
        // 2026-10-05 06:31 UTC is 2:31 AM EDT.
        let at = unix(2026, 10, 5, 6, 31);
        assert_eq!(human(at, at, DisplayTz::UsEastern), "2:31 AM ET");
        // 2026-10-04 15:04 UTC is 11:04 AM EDT.
        let incident = unix(2026, 10, 4, 15, 4);
        assert_eq!(
            human(incident, incident, DisplayTz::UsEastern),
            "11:04 AM ET"
        );
        // Noon and midnight.
        assert_eq!(
            human(
                unix(2026, 10, 4, 16, 0),
                unix(2026, 10, 4, 16, 0),
                DisplayTz::UsEastern
            ),
            "12:00 PM ET"
        );
        assert_eq!(
            human(
                unix(2026, 10, 5, 4, 0),
                unix(2026, 10, 5, 4, 0),
                DisplayTz::UsEastern
            ),
            "12:00 AM ET"
        );
        // A different local day is prefixed with the date.
        assert_eq!(
            human(
                unix(2026, 10, 5, 6, 31),
                unix(2026, 10, 4, 15, 0),
                DisplayTz::UsEastern
            ),
            "Oct 5 2:31 AM ET"
        );
        // Winter time is UTC-5.
        assert_eq!(
            human(
                unix(2026, 12, 1, 17, 5),
                unix(2026, 12, 1, 17, 5),
                DisplayTz::UsEastern
            ),
            "12:05 PM ET"
        );
        assert_eq!(human(at, at, DisplayTz::Utc), "06:31 UTC");
    }

    #[test]
    fn audit_stamps() {
        let at = unix(2026, 10, 5, 6, 31) + 7.25;
        assert_eq!(rfc3339_utc(at), "2026-10-05T06:31:07.250Z");
        assert_eq!(eastern_stamp(at), "2026-10-05 02:31:07 ET");
    }
}
