//! Small UTC formatting helpers without a runtime locale or external clock dependency.

use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn unix_seconds(now: SystemTime) -> i64 {
    match now.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

pub(crate) fn utc_parts(seconds: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
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
        i32::try_from(year).unwrap_or(if year < 0 { i32::MIN } else { i32::MAX }),
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
        u32::try_from(day_seconds / 3_600).unwrap_or(0),
        u32::try_from((day_seconds % 3_600) / 60).unwrap_or(0),
        u32::try_from(day_seconds % 60).unwrap_or(0),
    )
}

pub(crate) fn rfc3339(now: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = utc_parts(unix_seconds(now));
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

pub(crate) fn compact_utc(now: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = utc_parts(unix_seconds(now));
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_leap_day_are_formatted_in_utc() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(compact_utc(UNIX_EPOCH), "19700101T000000");
        assert_eq!(
            rfc3339(UNIX_EPOCH + std::time::Duration::from_secs(1_582_934_400)),
            "2020-02-29T00:00:00Z"
        );
    }
}
