//! Saying an instant out loud, in the operator's own zone.
//!
//! Discord hands this server ISO-8601 in UTC — `2026-08-19T13:51:25.123000+00:00`. That is a fine
//! value to compute with and a terrible one to read aloud, because the reader has to convert it
//! and then label the result. In a live run the assistant was given 13:51:25 UTC and said
//! "thirteen fifty-one Eastern Time": it kept the digits and attached a zone that did not belong
//! to them. Nine fifty-one was the answer.
//!
//! So the conversion happens HERE, once, and what travels onward is a string that is already
//! correct when spoken verbatim. The exact instant travels alongside it, unrounded, because
//! rounding to the second would lose ordering information and because anything that computes with
//! a time needs the real one. See [`crate::model::Message::spoken_time`] for the naming rule that
//! keeps the two apart.
//!
//! # What the spoken form deliberately does not carry
//!
//! Seconds and a zone label. Both were there, and both were removed on report: they are the two
//! parts of `09:51:25 EDT` that cost a listener time and tell them nothing they wanted. The
//! precision is below anything anybody places a chat message to, and the label names the zone
//! they are already in. [`spoken`] says what that trades away.
//!
//! # The zone database is bundled
//!
//! `jiff` is depended on with `tzdb-bundle-always`, so the IANA database is compiled into the
//! binary. The runtime stage of `Containerfile` is `debian:bookworm-slim` with nothing but
//! `ca-certificates` installed — no `tzdata` — and a server that silently fell back to UTC because
//! the image lacked a database would reproduce the exact bug this module exists to fix.

use jiff::tz::TimeZone;
use jiff::Timestamp;

/// A resolved IANA time zone, together with the name the operator configured it under.
///
/// Constructed only through [`zone`], so a `Zone` in hand is always one the database knows.
#[derive(Clone, Debug)]
pub struct Zone {
    name: String,
    tz: TimeZone,
}

impl Zone {
    /// The IANA name as the operator wrote it, for log lines and error messages.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Put an instant into this zone.
    ///
    /// An accessor rather than a public `tz` field: a caller that could reach the `TimeZone` could
    /// also keep one, and a zone outliving the configuration it was resolved from is how two parts
    /// of a server come to disagree about what time it is.
    #[must_use]
    pub fn at(&self, instant: jiff::Timestamp) -> jiff::Zoned {
        instant.to_zoned(self.tz.clone())
    }
}

/// Resolve an IANA zone name such as `America/New_York` or `UTC`.
///
/// # Errors
///
/// Returns a human-readable reason when the name is blank or the bundled database does not have
/// it. The caller is [`crate::config`], which turns that into a startup refusal naming the field —
/// a wrong zone must stop the server, not quietly become UTC.
pub fn zone(name: &str) -> Result<Zone, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("is blank; write an IANA zone name such as \"America/New_York\"".to_owned());
    }
    let tz = TimeZone::get(trimmed).map_err(|e| {
        format!("{trimmed:?} is not an IANA time zone in the bundled database ({e})")
    })?;
    Ok(Zone {
        name: trimmed.to_owned(),
        tz,
    })
}

/// Render an ISO-8601 instant as a bare local time: `09:51`.
///
/// **No seconds and no zone label**, because this value exists to be SPOKEN and neither of them
/// survives the trip. "Nine fifty-one and twenty-five seconds Eastern Daylight Time" is eight
/// syllables of a sentence the listener asked to be short, and the two things they buy are worth
/// nothing here: nobody places a chat message to the second, and the label names a zone the
/// listener is already standing in. The owner reported both, in those words.
///
/// It used to be `09:51:25 EDT`, and the label was load-bearing at the time — it was what made the
/// string safe to read verbatim after an assistant was handed `13:51:25` in UTC and said "thirteen
/// fifty-one Eastern Time", keeping the digits and relabelling them. What replaces the label is
/// that there is nothing left to convert: a bare `09:51` offers no zone to reinterpret, and the
/// tool descriptions say in as many words that this field is already local. Losing the label costs
/// the ability to detect that mistake from the transcript alone, which is the trade being made.
///
/// An input that does not parse is returned UNCHANGED. That is deliberate: the alternative is
/// inventing a time, and a caller reading a raw ISO string aloud is merely awkward, whereas a
/// caller reading a fabricated one is wrong in a way nobody can detect downstream.
#[must_use]
pub fn spoken(iso: &str, zone: &Zone) -> String {
    let Ok(instant) = iso.parse::<Timestamp>() else {
        return iso.to_owned();
    };
    instant
        .to_zoned(zone.tz.clone())
        .strftime("%H:%M")
        .to_string()
}

/// Parse an ISO-8601 instant into milliseconds since the Unix epoch.
///
/// Returns `None` for anything that is not an instant, so a caller-supplied time range can be
/// refused with a named error rather than silently becoming "the beginning of time".
#[must_use]
pub fn instant_ms(iso: &str) -> Option<i64> {
    iso.trim()
        .parse::<Timestamp>()
        .ok()
        .map(Timestamp::as_millisecond)
}

/// Render milliseconds since the Unix epoch back as an ISO-8601 instant in UTC.
///
/// Used for the continuation cursor of a time-range walk: what a caller hands back must be the
/// same kind of value it passed in, or stepping is not a loop it can write.
#[must_use]
pub fn iso_from_ms(ms: i64) -> String {
    Timestamp::from_millisecond(ms).map_or_else(|_| String::new(), |t| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instant_is_rendered_in_the_configured_zone() {
        let eastern = zone("America/New_York").expect("a real IANA zone");
        assert_eq!(
            spoken("2026-08-19T13:51:25.123000+00:00", &eastern),
            "09:51",
            "this is the live failure the issue reported: 13:51 UTC is 09:51 in Eastern"
        );
    }

    #[test]
    fn nothing_a_listener_cannot_use_survives_into_the_spoken_form() {
        // Both halves of what the owner asked to lose, asserted as absences rather than as an
        // equality that would pass for the wrong reason. Seconds are precision nobody places a
        // chat message to; the zone label names the zone the listener is standing in.
        let eastern = zone("America/New_York").expect("a real IANA zone");
        let said = spoken("2026-08-19T13:51:25.123000+00:00", &eastern);
        assert!(!said.contains("25"), "the seconds field survived: {said}");
        assert!(!said.contains("EDT"), "the zone label survived: {said}");
        assert_eq!(
            said.chars().count(),
            5,
            "anything else is decoration: {said}"
        );
    }

    #[test]
    fn the_same_wall_clock_shifts_by_an_hour_in_january_as_a_real_zone_must() {
        // The control that proves a real zone database rather than a hard-coded -4. It used to
        // read the EST/EDT label, which is gone; the DST rule shows up just as plainly in the
        // HOUR, and arguably more so — a wrong label is a cosmetic bug and a wrong hour is not.
        let eastern = zone("America/New_York").expect("a real IANA zone");
        assert_eq!(spoken("2026-01-19T13:51:25+00:00", &eastern), "08:51");
        assert_eq!(spoken("2026-08-19T13:51:25+00:00", &eastern), "09:51");
    }

    #[test]
    fn utc_agrees_with_the_instant_it_was_given() {
        let utc = zone("UTC").expect("UTC is always available");
        assert_eq!(spoken("2026-08-19T13:51:25.123000+00:00", &utc), "13:51");
    }

    #[test]
    fn an_unparseable_input_comes_back_verbatim_rather_than_as_an_invented_time() {
        let utc = zone("UTC").expect("UTC is always available");
        assert_eq!(spoken("not-a-timestamp", &utc), "not-a-timestamp");
        assert_eq!(spoken("", &utc), "");
    }

    #[test]
    fn an_unknown_zone_is_refused_rather_than_defaulted_to_utc() {
        let error = zone("Mars/Olympus_Mons").expect_err("no such zone");
        assert!(error.contains("Mars/Olympus_Mons"), "{error}");
        let error = zone("   ").expect_err("a blank zone is not a zone");
        assert!(error.contains("blank"), "{error}");
    }

    #[test]
    fn an_instant_converts_to_milliseconds_and_back() {
        let ms = instant_ms("2026-08-19T13:51:25.123Z").expect("an instant");
        assert_eq!(ms, 1_787_147_485_123);
        assert_eq!(instant_ms("2026-08-19T09:51:25.123-04:00"), Some(ms));
        assert_eq!(
            instant_ms("yesterday afternoon"),
            None,
            "a range this server cannot place must be refused, not defaulted"
        );
        assert!(
            iso_from_ms(ms).starts_with("2026-08-19T13:51:25.123"),
            "{}",
            iso_from_ms(ms)
        );
    }

    #[test]
    fn a_zone_remembers_the_name_it_was_configured_under() {
        assert_eq!(
            zone("  America/Chicago ").expect("valid").name(),
            "America/Chicago",
            "surrounding whitespace in a config file must not become part of the name"
        );
    }
}
