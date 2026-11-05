//! The time stamp that opens every chat request prompt.
//!
//! A bridge that was down, or a coordinator that was busy, can hand an agent a message long after
//! it was written, and several such prompts can arrive at once. Nothing in the prompt said when
//! the message was sent, so a stale request read as a current one. Each prompt now opens with the
//! message's own create time, in the service's local time zone, and with how much later it was
//! typed when that is at least [`LATE_PROMPT_AFTER`].
//!
//! The delay is measured when the prompt is typed, not when it is rendered: a prompt waits in the
//! coordinator's queue while the agent is busy. The bridge therefore queues a request prompt with
//! a [`deferred`] marker naming the create time; the queue stores the text with an opening for
//! the time it was queued and records the create time beside it ([`QueuedOpening`]), and the
//! drain replaces that opening with one for the moment it types the prompt ([`retime`]). The
//! stored text stays printable, so a queue reader that knows nothing of this types it as it was
//! queued: the send time right, the delay as of queueing.
//!
//! The zone is the one the C library resolves for the process: the `TZ` environment variable when
//! it is set, otherwise the host's `/etc/localtime`. Its abbreviation is printed, so a stamp never
//! reads as a different zone's time.

use std::time::Duration;

/// How much later than its create time a message must reach the agent before its prompt also says
/// how long it waited. Ordinary delivery takes seconds; a prompt queued behind a busy agent, or
/// held while the bridge was down, takes longer.
pub(crate) const LATE_PROMPT_AFTER: Duration = Duration::from_secs(120);

/// The opening of a request prompt's first line, as `Sent 2026.10.07:08:45 EDT. ` or, for a late
/// one, `Sent 2026.10.07:08:45 EDT, delivered 1 h 12 min later. `; empty when `created_at` cannot
/// be read, which a provider-accepted message never is.
pub(crate) fn opening(created_at: &str, now_millis: u64) -> String {
    let Some(created) = rfc3339_instant(created_at) else {
        return String::new();
    };
    let stamp = stamp(created.seconds, zone_at(created.seconds));
    // Exact, fraction included, so a message 119.5 seconds old is not called late; a fraction
    // finer than a nanosecond counts as one more, so a message is never made older than it is.
    let waited_nanos = i128::from(now_millis) * 1_000_000
        - (i128::from(created.seconds) * 1_000_000_000
            + i128::from(created.nanos)
            + i128::from(created.finer));
    if waited_nanos >= i128::from(LATE_PROMPT_AFTER.as_secs()) * 1_000_000_000 {
        let waited = u64::try_from(waited_nanos / 1_000_000_000).unwrap_or(u64::MAX);
        format!("Sent {stamp}, delivered {} later. ", duration_words(waited))
    } else {
        format!("Sent {stamp}. ")
    }
}

/// Starts a [`deferred`] prompt. No rendered prompt holds it: prompts never carry control
/// characters into the pane.
const DEFERRED: char = '\u{0}';

/// A request prompt whose opening is decided when it is queued and again when it is typed:
/// `rest` is the prompt after the opening, and `created_at` the message's create time.
pub(crate) fn deferred(created_at: &str, rest: &str) -> String {
    format!("{DEFERRED}{created_at}{DEFERRED}{rest}")
}

/// What the queue stores for a [`deferred`] prompt: the text it keeps, opening included, and the
/// create time and the exact opening it records beside it.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct QueuedOpening {
    pub(crate) text: String,
    pub(crate) sent_at: String,
    pub(crate) opening: String,
}

/// The queued form of `text` when it is a [`deferred`] prompt with a readable create time, opened
/// for `now_millis`; `None` for any other text, which is queued unchanged.
pub(crate) fn queue_deferred(text: &str, now_millis: u64) -> Option<QueuedOpening> {
    let (sent_at, rest) = text.strip_prefix(DEFERRED)?.split_once(DEFERRED)?;
    rfc3339_instant(sent_at)?;
    let opening = opening(sent_at, now_millis);
    Some(QueuedOpening {
        text: format!("{opening}{rest}"),
        sent_at: sent_at.to_owned(),
        opening,
    })
}

/// The text to type for a queued prompt that recorded `sent_at` and `opening`: that opening
/// replaced by one for `now_millis`. `None` when the text does not begin with exactly that
/// opening, or `sent_at` cannot be read; the text is then typed as stored, never cut.
pub(crate) fn retime(text: &str, sent_at: &str, opening: &str, now_millis: u64) -> Option<String> {
    rfc3339_instant(sent_at)?;
    if !opening.starts_with("Sent ") {
        return None;
    }
    let rest = text.strip_prefix(opening)?;
    Some(format!("{}{rest}", self::opening(sent_at, now_millis)))
}

/// The wall clock a prompt is rendered against.
pub(crate) fn now_millis() -> u64 {
    #[cfg(test)]
    if let Some(now) = test_clock::NOW.with(std::cell::Cell::get) {
        return now;
    }
    crate::chat_runtime::unix_millis()
}

/// A UTC offset and the name printed for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Zone {
    pub(crate) offset_seconds: i64,
    pub(crate) name: String,
}

impl Zone {
    fn utc() -> Self {
        Self {
            offset_seconds: 0,
            name: "UTC".to_owned(),
        }
    }
}

fn zone_at(unix_seconds: i64) -> Zone {
    #[cfg(test)]
    if let Some(zone) = test_clock::ZONE.with(|zone| zone.borrow().clone()) {
        return zone;
    }
    process_zone(unix_seconds).unwrap_or_else(Zone::utc)
}

/// The process's local zone at `unix_seconds`, from `localtime_r`. An abbreviation that is not a
/// short printable ASCII word is replaced by the numeric offset, as `UTC-04:00`.
pub(crate) fn process_zone(unix_seconds: i64) -> Option<Zone> {
    let time = libc::time_t::try_from(unix_seconds).ok()?;
    // SAFETY: `tm` is plain old data, and `localtime_r` writes it in full before it returns
    // non-null; both pointers are valid for the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return None;
    }
    // `c_long` is 32 bits on some targets, where this conversion widens.
    #[allow(clippy::useless_conversion)]
    let offset_seconds = i64::from(tm.tm_gmtoff);
    let abbreviation = if tm.tm_zone.is_null() {
        None
    } else {
        // SAFETY: a non-null `tm_zone` from `localtime_r` points at a NUL-terminated string the
        // C library keeps for the life of the process.
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
            .to_str()
            .ok()
            .filter(|name| {
                (1..=8).contains(&name.len())
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'-')
            })
            .map(str::to_owned)
    };
    Some(Zone {
        offset_seconds,
        name: abbreviation.unwrap_or_else(|| numeric_offset(offset_seconds)),
    })
}

fn numeric_offset(offset_seconds: i64) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let minutes = offset_seconds.unsigned_abs() / 60;
    format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// `YYYY.MM.DD:HH:MM NAME` for `unix_seconds` shifted into `zone`.
fn stamp(unix_seconds: i64, zone: Zone) -> String {
    let local = unix_seconds.saturating_add(zone.offset_seconds);
    let (year, month, day) = civil_from_days(local.div_euclid(86_400));
    let minute_of_day = local.rem_euclid(86_400) / 60;
    format!(
        "{year:04}.{month:02}.{day:02}:{:02}:{:02} {}",
        minute_of_day / 60,
        minute_of_day % 60,
        zone.name
    )
}

/// `N min`, `H h M min`, or from a day on `D d H h`, rounded down.
fn duration_words(seconds: u64) -> String {
    let minutes = seconds / 60;
    let (days, hours, minutes) = (minutes / 1_440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes} min"),
        (0, _) => format!("{hours} h {minutes} min"),
        _ => format!("{days} d {hours} h"),
    }
}

/// An instant: whole seconds since the Unix epoch, nanoseconds past them, and whether the
/// timestamp had a nonzero fraction finer than a nanosecond.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Instant {
    pub(crate) seconds: i64,
    pub(crate) nanos: u32,
    pub(crate) finer: bool,
}

/// The instant an RFC 3339 timestamp names, `2026-10-07T12:45:00Z` or with a fraction or a
/// `+hh:mm`/`-hh:mm` offset; `None` for anything else. Digits finer than a nanosecond are not
/// rounded into the others, which would change the second and so the minute shown; `finer` says
/// they were there. A leap second reads as the last nanosecond of the second before it.
pub(crate) fn rfc3339_instant(value: &str) -> Option<Instant> {
    let bytes = value.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = bytes.get(range)?;
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    };
    if bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut zone = 19;
    let mut nanos: u32 = 0;
    let mut finer = false;
    if bytes.get(zone) == Some(&b'.') {
        zone += 1;
        let start = zone;
        while bytes.get(zone).is_some_and(u8::is_ascii_digit) {
            zone += 1;
        }
        if zone == start {
            return None;
        }
        let mut scale = 100_000_000;
        for &digit in &bytes[start..zone] {
            if scale == 0 {
                finer |= digit != b'0';
            } else {
                nanos += u32::from(digit - b'0') * scale;
                scale /= 10;
            }
        }
    }
    let offset = match bytes.get(zone) {
        Some(b'Z' | b'z') if zone + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-'))
            if zone + 6 == bytes.len() && bytes.get(zone + 3) == Some(&b':') =>
        {
            let (hours, minutes) = (number(zone + 1..zone + 3)?, number(zone + 4..zone + 6)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = hours * 3_600 + minutes * 60;
            if *sign == b'-' {
                -offset
            } else {
                offset
            }
        }
        _ => return None,
    };
    let (second, nanos, finer) = if second == 60 {
        (59, 999_999_999, false)
    } else {
        (second, nanos, finer)
    };
    let local = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(Instant {
        seconds: local - offset,
        nanos,
        finer,
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date `days` after 1970-01-01, the inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Per-thread overrides of the clock and zone a prompt is rendered against, so a test can state
/// the exact first line without depending on when or where it runs.
#[cfg(test)]
pub(crate) mod test_clock {
    use super::Zone;
    use std::cell::{Cell, RefCell};

    thread_local! {
        pub(super) static NOW: Cell<Option<u64>> = const { Cell::new(None) };
        pub(super) static ZONE: RefCell<Option<Zone>> = const { RefCell::new(None) };
    }

    /// Render this thread's prompts at `now_millis` in a fixed zone `offset_seconds` from UTC.
    pub(crate) fn set(now_millis: u64, offset_seconds: i64, name: &str) {
        NOW.with(|now| now.set(Some(now_millis)));
        ZONE.with(|zone| {
            *zone.borrow_mut() = Some(Zone {
                offset_seconds,
                name: name.to_owned(),
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDT: i64 = -4 * 3_600;

    /// 2026-10-07T12:45:00Z, 8:45 AM EDT.
    const MORNING: i64 = 1_791_377_100;

    fn at(seconds: i64) -> u64 {
        u64::try_from(seconds).unwrap() * 1_000
    }

    fn seconds(value: &str) -> Option<i64> {
        rfc3339_instant(value).map(|instant| instant.seconds)
    }

    #[test]
    fn rfc3339_reads_utc_fractions_and_offsets() {
        assert_eq!(seconds("2026-10-07T12:45:00Z"), Some(MORNING));
        assert_eq!(seconds("2026-10-07T08:45:00-04:00"), Some(MORNING));
        assert_eq!(seconds("2026-10-07T18:15:00+05:30"), Some(MORNING));
        assert_eq!(seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(seconds("1969-12-31T23:59:59Z"), Some(-1));
        assert_eq!(seconds("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(
            rfc3339_instant("2026-10-07t12:45:00.123456z"),
            Some(Instant {
                seconds: MORNING,
                nanos: 123_456_000,
                finer: false
            })
        );
        // Digits finer than a nanosecond are noted, never carried into the second.
        assert_eq!(
            rfc3339_instant("2026-10-07T12:45:00.0000000001Z"),
            Some(Instant {
                seconds: MORNING,
                nanos: 0,
                finer: true
            })
        );
        assert_eq!(
            rfc3339_instant("2026-10-07T12:44:59.9999999999Z"),
            Some(Instant {
                seconds: MORNING - 1,
                nanos: 999_999_999,
                finer: true
            })
        );
        assert_eq!(
            rfc3339_instant("2016-12-31T23:59:60Z"),
            Some(Instant {
                seconds: 1_483_228_799,
                nanos: 999_999_999,
                finer: false
            })
        );
    }

    #[test]
    fn rfc3339_rejects_what_the_subscription_crate_rejects() {
        for value in [
            "",
            "2026-10-07",
            "2026-10-07 12:45:00Z",
            "2026-10-07T12:45:00",
            "2026-10-07T12:45:00.Z",
            "2026-13-07T12:45:00Z",
            "2025-02-29T12:45:00Z",
            "2026-10-07T24:00:00Z",
            "2026-10-07T12:45:00+0400",
            "2026-10-07T12:45:00+24:00",
            "2026-10-07T12:45:00Zjunk",
            "+026-10-07T12:45:00Z",
        ] {
            assert_eq!(rfc3339_instant(value), None, "{value}");
        }
    }

    #[test]
    fn civil_dates_round_trip() {
        for days in [
            -800_000, -719_468, -1, 0, 1, 10_957, 11_016, 20_733, 2_000_000,
        ] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(
                days_from_civil(year, month, day),
                days,
                "{year}-{month}-{day}"
            );
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn the_stamp_is_the_owners_format_in_the_given_zone() {
        let eastern = Zone {
            offset_seconds: EDT,
            name: "EDT".to_owned(),
        };
        assert_eq!(stamp(MORNING, eastern.clone()), "2026.10.07:08:45 EDT");
        // Shifting into the zone can cross midnight and the year.
        assert_eq!(stamp(1_798_765_200, eastern), "2026.12.31:21:00 EDT");
        assert_eq!(stamp(MORNING, Zone::utc()), "2026.10.07:12:45 UTC");
    }

    #[test]
    fn a_prompt_typed_promptly_gives_only_the_send_time() {
        test_clock::set(at(MORNING + 119), EDT, "EDT");
        assert_eq!(
            opening("2026-10-07T12:45:00Z", now_millis()),
            "Sent 2026.10.07:08:45 EDT. "
        );
        // A create time after the bridge's clock is not reported as a negative wait.
        assert_eq!(
            opening("2026-10-07T12:50:00Z", now_millis()),
            "Sent 2026.10.07:08:50 EDT. "
        );
    }

    #[test]
    fn a_sub_nanosecond_fraction_never_changes_the_minute_shown() {
        test_clock::set(at(MORNING), EDT, "EDT");
        assert_eq!(
            opening("2026-10-07T12:44:59.9999999999Z", at(MORNING)),
            "Sent 2026.10.07:08:44 EDT. "
        );
        // ...and keeps a message just under the threshold from counting as late.
        assert_eq!(
            opening("2026-10-07T12:45:00.0000000001Z", at(MORNING + 120)),
            "Sent 2026.10.07:08:45 EDT. "
        );
    }

    #[test]
    fn lateness_counts_the_fraction_of_the_create_time() {
        test_clock::set(at(MORNING), EDT, "EDT");
        // 119.5 seconds is not late; 120 seconds is.
        assert_eq!(
            opening("2026-10-07T12:45:00.5Z", at(MORNING + 120)),
            "Sent 2026.10.07:08:45 EDT. "
        );
        assert_eq!(
            opening("2026-10-07T12:45:00.5Z", at(MORNING + 120) + 500),
            "Sent 2026.10.07:08:45 EDT, delivered 2 min later. "
        );
        // 3,599.5 seconds is still under an hour.
        assert_eq!(
            opening("2026-10-07T12:45:00.5Z", at(MORNING + 3_600)),
            "Sent 2026.10.07:08:45 EDT, delivered 59 min later. "
        );
        assert_eq!(
            opening("2026-10-07T12:45:00.5Z", at(MORNING + 3_600) + 500),
            "Sent 2026.10.07:08:45 EDT, delivered 1 h 0 min later. "
        );
    }

    #[test]
    fn a_late_prompt_says_how_much_later_it_was_typed() {
        for (waited, words) in [
            (120, "2 min"),
            (59 * 60 + 59, "59 min"),
            (3_600, "1 h 0 min"),
            (3_600 + 12 * 60 + 30, "1 h 12 min"),
            (34 * 3_600 + 5 * 60, "1 d 10 h"),
        ] {
            test_clock::set(at(MORNING + waited), EDT, "EDT");
            assert_eq!(
                opening("2026-10-07T12:45:00Z", now_millis()),
                format!("Sent 2026.10.07:08:45 EDT, delivered {words} later. "),
                "{waited}"
            );
        }
    }

    #[test]
    fn an_unreadable_create_time_opens_with_nothing() {
        assert_eq!(opening("yesterday", at(MORNING)), "");
    }

    #[test]
    fn a_deferred_prompt_is_queued_printable_and_retimed_when_typed() {
        test_clock::set(at(MORNING + 30), EDT, "EDT");
        let marked = deferred(
            "2026-10-07T12:45:00Z",
            "The user wants the lock released before starting a task.",
        );
        let queued = queue_deferred(&marked, now_millis()).expect("deferred prompt");
        assert_eq!(
            queued,
            QueuedOpening {
                text: "Sent 2026.10.07:08:45 EDT. The user wants the lock released before starting a task.".to_owned(),
                sent_at: "2026-10-07T12:45:00Z".to_owned(),
                opening: "Sent 2026.10.07:08:45 EDT. ".to_owned(),
            }
        );
        assert!(!queued.text.chars().any(char::is_control));
        // Typed an hour and twelve minutes after it was written, behind a busy agent.
        assert_eq!(
            retime(
                &queued.text,
                &queued.sent_at,
                &queued.opening,
                at(MORNING + 72 * 60)
            ),
            Some(
                "Sent 2026.10.07:08:45 EDT, delivered 1 h 12 min later. The user wants the lock released before starting a task."
                    .to_owned()
            )
        );
        // Ordinary text, and a marker without a readable create time, are queued unchanged.
        assert_eq!(queue_deferred("plain prompt", now_millis()), None);
        assert_eq!(
            queue_deferred(&deferred("soon", "text"), now_millis()),
            None
        );
        // A record that does not fit its text leaves the text as stored, never cut: an opening
        // the text does not begin with (this stale one is longer than the real opening and ends
        // inside the request, where cutting by length alone would lose its start), one that is
        // not an opening, an unreadable create time.
        let stale = "Sent 2026.10.07:08:45 EDT, delivered 1 h 12 min later. ";
        assert_eq!(
            retime(&queued.text, &queued.sent_at, stale, at(MORNING)),
            None
        );
        assert_eq!(retime(&queued.text, &queued.sent_at, "", at(MORNING)), None);
        assert_eq!(
            retime(&queued.text, &queued.sent_at, "Sent", at(MORNING)),
            None
        );
        assert_eq!(
            retime(&queued.text, "soon", &queued.opening, at(MORNING)),
            None
        );
    }

    /// Set in the child process [`the_process_zone_follows_tz_in_both_seasons`] starts.
    const ZONE_CHILD: &str = "AGENTCTL_PROMPT_ZONE_CHILD";

    #[test]
    fn the_process_zone_follows_tz_in_both_seasons() {
        // 2026-01-15T12:00:00Z and 2026-07-15T12:00:00Z.
        let instants = [1_768_478_400, 1_784_116_800];
        if std::env::var_os(ZONE_CHILD).is_some() {
            for instant in instants {
                let zone = process_zone(instant).expect("localtime_r");
                println!("<zone {} {}>", zone.offset_seconds, zone.name);
            }
            return;
        }
        // The zone is the process's, so each case runs this test again in a child with its TZ.
        for (tz, expected) in [
            ("EST5EDT,M3.2.0,M11.1.0", ["-18000 EST", "-14400 EDT"]),
            ("UTC0", ["0 UTC", "0 UTC"]),
            // The extreme POSIX offset, with its implicit hour of daylight time.
            ("STD-24:59:59DST,M3.2.0,M11.1.0", ["89999 STD", "93599 DST"]),
        ] {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "prompt_time::tests::the_process_zone_follows_tz_in_both_seasons",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(ZONE_CHILD, "1")
                .env("TZ", tz)
                .output()
                .expect("run the child");
            assert!(output.status.success(), "{tz}: {output:?}");
            // libtest writes its own words on the line before the first output, so the zones are
            // found between their markers wherever they fall.
            let stdout = String::from_utf8_lossy(&output.stdout);
            let zones: Vec<&str> = stdout
                .split("<zone ")
                .skip(1)
                .filter_map(|part| part.split_once('>').map(|(zone, _)| zone))
                .collect();
            assert_eq!(zones, expected, "{tz}");
        }
        assert_eq!(numeric_offset(EDT), "UTC-04:00");
        assert_eq!(numeric_offset(19_800), "UTC+05:30");
    }
}
