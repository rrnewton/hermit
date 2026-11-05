//! "The voice service is not responding", as one content-free log line.
//!
//! `voice-unresponsive-signal`. A live call once stayed green for its whole length while every
//! turn produced no speech and no transcript: the socket was open, turns completed, and each one
//! carried PCM that nobody could hear. Nothing on the page and nothing in this log said so. The
//! page now judges each turn from the frames `vibe-talk-v1` already carries and reports what it
//! concluded here, so a log reader can see the episode without anyone having been on the call.
//!
//! **Content-free by construction.** [`HealthReport`] is a closed set of fields: two closed enums,
//! a flag, and integers. An unknown field, an unknown [`Cause`], or an unknown protocol is refused
//! rather than echoed, so the one line it writes has no free-text field at all — not the words
//! said, not an error message, not an id, not a provider.
//!
//! **Bounded.** The page reports each cause at most once per call, and a recovery only after an
//! unresponsive report it attempted, so a well-behaved call writes at most seven lines: four
//! causes and three recoveries. The server does not trust that: [`LogBudget`] caps the lines this
//! route writes per minute across every caller, and [`MAX_BODY_BYTES`] caps what it will read.
//! The first refusal in a window writes one line saying records are being dropped, so throttling
//! is visible without becoming a flood of its own.
//!
//! **How a call ended** (`#268 voice-call-drops`) is the second record here, [`EndReport`]: one
//! line per call that opened, naming who or what ended it, on the same terms — closed, bounded by
//! its own [`LogBudget`], and free of anything said.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The largest request body this route reads. A valid report is under 200 bytes.
pub const MAX_BODY_BYTES: usize = 512;

/// The most lines this route writes per [`BUDGET_WINDOW`], across every caller. Well above what
/// honest pages produce — seven lines per call at most — and far below a flood.
pub const MAX_LINES_PER_WINDOW: u32 = 30;

/// The window [`MAX_LINES_PER_WINDOW`] is counted over.
pub const BUDGET_WINDOW: Duration = Duration::from_secs(60);

/// The longest call offset accepted: a day. Past that the number is not a measurement.
pub const MAX_SINCE_OPEN_MS: u32 = 86_400_000;

/// The most audio one turn is believed to carry: an hour.
pub const MAX_AUDIO_MS: u32 = 3_600_000;

/// The largest magnitude a 16-bit sample can have (`|-32768|`).
pub const MAX_PEAK: u32 = 32_768;

/// Wire protocols this signal is defined for. Only the provider-neutral one: a hosted vendor's
/// protocol has error frames of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Protocol {
    /// The deployment-managed WebSocket protocol.
    #[serde(rename = "vibe-talk-v1")]
    VibeTalkV1,
}

impl Protocol {
    const fn as_str(self) -> &'static str {
        match self {
            Self::VibeTalkV1 => "vibe-talk-v1",
        }
    }
}

/// What the page concluded. See the README, "When the voice service stops answering".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The greeting turn completed with no audible audio and no assistant text.
    SilentGreeting,
    /// Consecutive turns completed with no audible audio and no assistant text.
    SilentTurns,
    /// A turn the page started got nothing at all within its bound.
    NoReply,
    /// The service sent an `error` frame.
    ErrorFrame,
    /// Audible audio or assistant text arrived after an unresponsive report.
    Recovered,
}

impl Cause {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SilentGreeting => "silent_greeting",
            Self::SilentTurns => "silent_turns",
            Self::NoReply => "no_reply",
            Self::ErrorFrame => "error_frame",
            Self::Recovered => "recovered",
        }
    }
}

/// One page's report. The field set IS the allowlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthReport {
    /// Wire protocol of the call.
    pub protocol: Protocol,
    /// Whether the call was typed, in which case only text counts as a reply.
    pub chat: bool,
    /// What the page concluded.
    pub cause: Cause,
    /// Milliseconds since the socket opened.
    pub since_open_ms: u32,
    /// Completed turns in the call so far: an ordinal, not an id.
    pub turns: u32,
    /// Consecutive silent turns ending at the most recent one.
    pub silent_turns: u32,
    /// Milliseconds of PCM the last silent turn carried; 0 when it carried none.
    pub audio_ms: u32,
    /// The largest sample magnitude in the last silent turn's PCM.
    pub peak: u32,
}

impl HealthReport {
    /// Refuse anything that is not a plausible report.
    ///
    /// # Errors
    ///
    /// Returns a sentence naming the offending field.
    pub fn validate(&self) -> Result<(), String> {
        if self.since_open_ms > MAX_SINCE_OPEN_MS {
            return Err(format!(
                "since_open_ms is longer than {MAX_SINCE_OPEN_MS} ms"
            ));
        }
        if self.audio_ms > MAX_AUDIO_MS {
            return Err(format!("audio_ms is longer than {MAX_AUDIO_MS} ms"));
        }
        if self.peak > MAX_PEAK {
            return Err(format!("peak is larger than {MAX_PEAK}"));
        }
        if self.silent_turns > self.turns {
            return Err("silent_turns is more than turns".to_owned());
        }
        Ok(())
    }

    /// Whether this report ends an episode rather than starting one.
    #[must_use]
    pub fn is_recovery(&self) -> bool {
        self.cause == Cause::Recovered
    }

    /// The single log line: enum names and integers, and nothing anybody said.
    #[must_use]
    pub fn log_fields(&self) -> String {
        format!(
            "protocol={} chat={} cause={} since_open_ms={} turns={} silent_turns={} audio_ms={} peak={}",
            self.protocol.as_str(),
            self.chat,
            self.cause.as_str(),
            self.since_open_ms,
            self.turns,
            self.silent_turns,
            self.audio_ms,
            self.peak,
        )
    }
}

/// Who or what ended a call that had opened, as the page saw it. `#268 voice-call-drops`. See the
/// README, "How a call ended".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedBy {
    /// The reader pressed Hang up.
    HangUp,
    /// The voice service sent an `error` frame, which ends its session, and the page then said
    /// goodbye (`audio_end`, `quit`) and closed. The only other way the page ends a call.
    ErrorFrame,
    /// The voice service's side closed the socket with a closing handshake while the reader was in
    /// the call and the page was in sight.
    ServerClosed,
    /// The connection was lost without a closing handshake (the browser's 1006, or an error on the
    /// socket before its close): the network, the phone, or the far side cut it.
    ConnectionLost,
    /// The socket closed while the page was in the background, or just after it came back.
    Suspended,
}

impl EndedBy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::HangUp => "hang_up",
            Self::ErrorFrame => "error_frame",
            Self::ServerClosed => "server_closed",
            Self::ConnectionLost => "connection_lost",
            Self::Suspended => "suspended",
        }
    }
}

/// One page's record of how a call that opened ended: `POST /api/v1/voice-end`. The field set IS
/// the allowlist, as for [`HealthReport`]: closed enums, a flag and integers, and nothing anybody
/// said.
///
/// `#268 voice-call-drops`. Calls ended on their own, and the only record of why was the voice
/// bridge's log, which saw the page say goodbye and could not see what made it. One line per call
/// here tells a hang-up from a drop from the service ending the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EndReport {
    /// Wire protocol of the call.
    pub protocol: Protocol,
    /// Whether the call was typed.
    pub chat: bool,
    /// Who or what ended it.
    pub ended_by: EndedBy,
    /// The close code the browser reported, `1000`–`4999`; absent when the page closed the socket
    /// itself.
    #[serde(default)]
    pub close_code: Option<u16>,
    /// Milliseconds from the socket opening to the end.
    pub since_open_ms: u32,
    /// Completed turns in the call: an ordinal, not an id.
    pub turns: u32,
}

impl EndReport {
    /// Refuse anything that is not a plausible record.
    ///
    /// # Errors
    ///
    /// Returns a sentence naming the offending field.
    pub fn validate(&self) -> Result<(), String> {
        if self.since_open_ms > MAX_SINCE_OPEN_MS {
            return Err(format!(
                "since_open_ms is longer than {MAX_SINCE_OPEN_MS} ms"
            ));
        }
        if self
            .close_code
            .is_some_and(|code| !(1000..=4999).contains(&code))
        {
            return Err("close_code is not a WebSocket close code (1000-4999)".to_owned());
        }
        Ok(())
    }

    /// Whether the reader ended the call, rather than anything ending it for them.
    #[must_use]
    pub fn by_the_reader(&self) -> bool {
        self.ended_by == EndedBy::HangUp
    }

    /// The single log line: enum names and integers, and nothing anybody said.
    #[must_use]
    pub fn log_fields(&self) -> String {
        format!(
            "protocol={} chat={} ended_by={} close_code={} since_open_ms={} turns={}",
            self.protocol.as_str(),
            self.chat,
            self.ended_by.as_str(),
            self.close_code
                .map_or_else(|| "-".to_owned(), |code| code.to_string()),
            self.since_open_ms,
            self.turns,
        )
    }
}

/// What [`LogBudget::spend`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spend {
    /// Write the record.
    Allowed,
    /// Refuse it, and say once that this window is dropping records.
    FirstRefusal,
    /// Refuse it quietly: this window has already said so.
    Refused,
}

impl Spend {
    /// Whether the record may be logged.
    #[must_use]
    pub const fn allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// A fixed-window count of the lines this route has written.
#[derive(Debug)]
pub struct LogBudget {
    window: Mutex<(Instant, u32)>,
}

impl Default for LogBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBudget {
    /// A budget whose first window starts now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            window: Mutex::new((Instant::now(), 0)),
        }
    }

    /// Spend one line if the current window has one left.
    pub fn spend(&self) -> Spend {
        self.spend_at(Instant::now())
    }

    fn spend_at(&self, now: Instant) -> Spend {
        let mut window = self
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.duration_since(window.0) >= BUDGET_WINDOW {
            *window = (now, 0);
        }
        // Counting on past the cap (saturating) is how the first refusal is told from the rest.
        window.1 = window.1.saturating_add(1);
        if window.1 <= MAX_LINES_PER_WINDOW {
            Spend::Allowed
        } else if window.1 == MAX_LINES_PER_WINDOW + 1 {
            Spend::FirstRefusal
        } else {
            Spend::Refused
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(cause: Cause) -> HealthReport {
        HealthReport {
            protocol: Protocol::VibeTalkV1,
            chat: false,
            cause,
            since_open_ms: 21_400,
            turns: 3,
            silent_turns: 2,
            audio_ms: 1_000,
            peak: 0,
        }
    }

    #[test]
    fn the_log_line_is_enum_names_and_integers_only() {
        let line = report(Cause::SilentTurns).log_fields();
        assert_eq!(
            line,
            "protocol=vibe-talk-v1 chat=false cause=silent_turns since_open_ms=21400 turns=3 \
             silent_turns=2 audio_ms=1000 peak=0"
        );
        assert!(
            !line.contains('"'),
            "a quoted value is a free-text field: {line}"
        );
    }

    #[test]
    fn every_cause_has_the_wire_name_it_deserializes_from() {
        for cause in [
            Cause::SilentGreeting,
            Cause::SilentTurns,
            Cause::NoReply,
            Cause::ErrorFrame,
            Cause::Recovered,
        ] {
            let wire = serde_json::to_value(cause).expect("serializes");
            assert_eq!(wire, cause.as_str());
        }
    }

    #[test]
    fn a_report_refuses_unknown_fields_values_and_absurd_numbers() {
        let mut smuggled = serde_json::to_value(report(Cause::NoReply)).expect("serializes");
        smuggled["message"] = "the upstream failed".into();
        assert!(serde_json::from_value::<HealthReport>(smuggled).is_err());
        let mut unknown = serde_json::to_value(report(Cause::NoReply)).expect("serializes");
        unknown["cause"] = "made_up".into();
        assert!(serde_json::from_value::<HealthReport>(unknown).is_err());
        for absurd in [
            HealthReport {
                since_open_ms: MAX_SINCE_OPEN_MS + 1,
                ..report(Cause::NoReply)
            },
            HealthReport {
                audio_ms: MAX_AUDIO_MS + 1,
                ..report(Cause::NoReply)
            },
            HealthReport {
                peak: MAX_PEAK + 1,
                ..report(Cause::NoReply)
            },
            HealthReport {
                silent_turns: 4,
                ..report(Cause::NoReply)
            },
        ] {
            assert!(absurd.validate().is_err(), "{absurd:?}");
        }
        report(Cause::NoReply).validate().expect("valid");
    }

    #[test]
    fn the_budget_refuses_past_its_cap_and_refills_with_the_next_window() {
        let budget = LogBudget::new();
        let start = Instant::now();
        for _ in 0..MAX_LINES_PER_WINDOW {
            assert_eq!(budget.spend_at(start), Spend::Allowed);
        }
        assert_eq!(budget.spend_at(start), Spend::FirstRefusal);
        assert_eq!(budget.spend_at(start + BUDGET_WINDOW / 2), Spend::Refused);
        assert_eq!(budget.spend_at(start + BUDGET_WINDOW), Spend::Allowed);
    }
}
