//! Everything a read-aloud needs, resolved BEFORE the reader taps.
//!
//! # Why this exists
//!
//! Reading a message aloud used to do all of its work on the tap: fetch a window of fifty messages
//! from Discord to find one by id, normalise its text for speech, ask ElevenLabs which voice the
//! agent speaks in, and only then start generating audio. Four waits in series, every time, and the
//! reader is sitting there through all of them having already been shown the very text they are
//! waiting to hear.
//!
//! The page already HAS that text — it drew it. So none of that work needs to happen on the tap. It
//! happens once, when read-aloud mode is turned on, and what the tap gets is a TICKET: an opaque
//! string naming one message whose text is already resolved and waiting. Playing it is then a
//! single hop that does a map lookup and starts forwarding bytes.
//!
//! # Why a ticket rather than the text
//!
//! The obvious shortcut is to let the page send the text it already has. That is refused here for
//! the same reason `/speak` always refused it: this server holds the operator's ElevenLabs
//! credential, and a route that reads back whatever it is handed is a route that spends their
//! balance on anything at all. The text in a ticket was resolved by this server FROM A REAL MESSAGE
//! and cannot be edited afterwards, so the guarantee is if anything stronger than before — it is
//! fixed at mint time rather than re-derived under a caller-supplied id.
//!
//! # Why the ticket is a capability
//!
//! It travels as a URL, because that is the whole point: an `<audio src>` cannot carry an
//! `Authorization` header, and without it the browser cannot stream. So the ticket has to
//! authenticate on its own. It is 256 bits from the system CSPRNG, it names exactly one message, it
//! expires in minutes, and the store it lives in is capped. What it grants — hearing one message
//! the holder could already read, plus appending bounded content-free playback timings for that
//! same read — is deliberately the smallest thing that makes the URL work. The timing callback
//! takes no bearer header for exactly the same reason as the audio element: the random ticket is
//! intentionally its capability too. It answers one 404 for a ticket that was never minted, one
//! that has expired, and one whose audio has not been played yet, so it reveals nothing the audio
//! route does not.
//!
//! It WILL appear in the page's DOM and in this server's access log, which is the cost of the
//! approach and the reason for the short life and the narrow scope.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a minted ticket stays playable.
///
/// Long enough to turn the mode on, read a screenful, and tap something near the bottom; short
/// enough that a URL copied out of a log is worthless by the time anyone reads it. A ticket that
/// has expired is not an error the reader should ever see — the page re-mints as it scrolls.
pub const TICKET_TTL: Duration = Duration::from_secs(600);

/// The most tickets held at once, across every reader.
///
/// A ceiling rather than a target. Turning the mode on mints one per visible row, and scrolling
/// mints more; without a cap a page left open all day is an unbounded map. Evicting the oldest is
/// safe because the page re-mints anything it still needs.
pub const MAX_TICKETS: usize = 512;

/// One message, resolved and ready to be spoken.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    /// The channel it came from, kept for the log line rather than for the lookup.
    pub channel: String,
    /// The Discord message this speaks. One ticket is one message.
    pub message: String,
    /// The text as it will be SENT: already through `speakable::for_speech`, so the tap does not
    /// pay for normalisation either.
    pub said: String,
    /// The pace the reader had chosen when this was minted, if they had chosen one.
    pub speed: Option<f64>,
    /// Opaque correlation key for content-free server and browser timing records.
    pub observation: String,
    /// Message lookup and normalization, ending before provider connection or readiness begins.
    pub preparation_ms: u64,
}

/// How the audio request itself spent its time, measured on the server.
///
/// The four phases add up to `request_to_first_audio_ms`, so a report can say where the wait went
/// without subtracting numbers taken from different clocks or different requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerTiming {
    /// From the audio request arriving until its first real audio chunk (not a container header).
    pub request_to_first_audio_ms: u64,
    /// Transport establishment this request waited for: zero when it reused a live session, and
    /// includes joining a connection another request (normally preparation) had already begun.
    pub request_connection_ms: u64,
    /// Waiting for a new session to announce itself ready; zero when a live session was reused.
    pub request_session_start_ms: u64,
    /// Waiting for an earlier read on the same session to finish or acknowledge its interrupt.
    pub yield_ms: u64,
    /// The remainder: from the prompt (or, for a provider without sessions, the request) until the
    /// first audio chunk, including any first attempt that failed and was repeated.
    pub first_audio_ms: u64,
    /// Content-free facts about the provider session; `None` for a provider that keeps none.
    pub session: Option<crate::speech::SessionUse>,
}

#[derive(Clone, Debug)]
struct Entry {
    prepared: Prepared,
    minted: Instant,
    server_timing: Option<ServerTiming>,
}

/// A short-lived table of prepared messages, keyed by an unguessable ticket.
#[derive(Debug)]
pub struct SpeechTickets {
    held: Mutex<HashMap<String, Entry>>,
}

impl Default for SpeechTickets {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeechTickets {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
        }
    }

    /// Mint a ticket for one prepared message, and return it.
    ///
    /// Expired entries are dropped on the way past, so a store nobody reads from still shrinks.
    pub fn mint(&self, prepared: Prepared) -> String {
        let ticket = fresh_ticket();
        if let Ok(mut held) = self.held.lock() {
            let now = Instant::now();
            held.retain(|_, entry| now.duration_since(entry.minted) < TICKET_TTL);
            // Only after expiry has been applied, so a store full of dead tickets evicts those
            // rather than a live one somebody is about to tap.
            while held.len() >= MAX_TICKETS {
                let Some(oldest) = held
                    .iter()
                    .min_by_key(|(_, entry)| entry.minted)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                held.remove(&oldest);
            }
            held.insert(
                ticket.clone(),
                Entry {
                    prepared,
                    minted: now,
                    server_timing: None,
                },
            );
        }
        ticket
    }

    /// What this ticket names, or `None` when it is unknown or has expired.
    ///
    /// The two are deliberately the same answer. Telling a caller which of "never existed" and
    /// "existed and lapsed" applies would let them probe for the difference.
    #[must_use]
    pub fn claim(&self, ticket: &str) -> Option<Prepared> {
        let mut held = self.held.lock().ok()?;
        let entry = held.get(ticket)?;
        if Instant::now().duration_since(entry.minted) >= TICKET_TTL {
            held.remove(ticket);
            return None;
        }
        // NOT removed on a successful claim. An `<audio>` element may open the URL more than once
        // — a retry, a re-read, a media session restoring itself — and a single-use ticket turns
        // the second of those into a failure the reader cannot explain or act on.
        Some(entry.prepared.clone())
    }

    /// Whether this ticket is live: known and unexpired. The same test as [`Self::claim`], without
    /// the copy and without dropping an expired entry, for checking the credential before the rest
    /// of a request is read.
    #[must_use]
    pub fn knows(&self, ticket: &str) -> bool {
        self.held.lock().is_ok_and(|held| {
            held.get(ticket)
                .is_some_and(|entry| Instant::now().duration_since(entry.minted) < TICKET_TTL)
        })
    }

    /// Attach the provider phases measured by the streaming handler to this observation.
    pub fn record_server_timing(&self, ticket: &str, timing: ServerTiming) {
        if let Ok(mut held) = self.held.lock() {
            if let Some(entry) = held.get_mut(ticket) {
                entry.server_timing = Some(timing);
            }
        }
    }

    /// Resolve a browser report to its content-free observation and server phases.
    #[must_use]
    pub fn observed_timing(&self, ticket: &str) -> Option<(Prepared, ServerTiming)> {
        let held = self.held.lock().ok()?;
        let entry = held.get(ticket)?;
        if Instant::now().duration_since(entry.minted) >= TICKET_TTL {
            return None;
        }
        Some((entry.prepared.clone(), entry.server_timing?))
    }

    /// How many tickets are held. For tests and the diagnostics line; not a stable API.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.lock().map(|held| held.len()).unwrap_or(0)
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 256 bits from the system CSPRNG, URL-safe and unpadded.
///
/// `getrandom` rather than a hash of a counter and a clock: this string is the only thing standing
/// between a stranger and one of the owner's messages, and the two properties that matter are that
/// it cannot be guessed and cannot be predicted from another one.
fn fresh_ticket() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    // A failure here means the OS could not produce randomness, which is not a condition this
    // server can paper over with something weaker — so it panics rather than minting a guessable
    // ticket. In practice `getrandom` on Linux does not fail once the pool is initialised.
    getrandom::fill(&mut bytes).expect("the system CSPRNG must be available to mint a ticket");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A correlation id which carries no message, credential, provider, or deployment identity.
#[must_use]
pub fn fresh_observation() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("the system CSPRNG must be available for timing ids");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(message: &str) -> Prepared {
        Prepared {
            channel: "2222222222".to_owned(),
            message: message.to_owned(),
            said: format!("the text of {message}"),
            speed: None,
            observation: fresh_observation(),
            preparation_ms: 1,
        }
    }

    #[test]
    fn a_ticket_names_exactly_what_was_minted_under_it() {
        let tickets = SpeechTickets::new();
        let one_prepared = prepared("1000000000000000001");
        let two_prepared = prepared("1000000000000000002");
        let one = tickets.mint(one_prepared.clone());
        let two = tickets.mint(two_prepared.clone());
        assert_ne!(one, two, "two mints produced the same ticket");
        assert_eq!(tickets.claim(&one), Some(one_prepared));
        assert_eq!(tickets.claim(&two), Some(two_prepared));
    }

    #[test]
    fn an_unknown_ticket_is_simply_unknown() {
        let tickets = SpeechTickets::new();
        assert_eq!(tickets.claim("not-a-ticket"), None);
    }

    #[test]
    fn a_ticket_survives_being_claimed_twice() {
        // An `<audio>` element may open the URL again — a retry, a re-read, a media session
        // restoring itself. Consuming the ticket on first claim turns that into a failure the
        // reader cannot explain, for a message they have already been allowed to hear.
        let tickets = SpeechTickets::new();
        let ticket = tickets.mint(prepared("1000000000000000003"));
        assert!(tickets.claim(&ticket).is_some());
        assert!(tickets.claim(&ticket).is_some(), "the second play failed");
    }

    #[test]
    fn tickets_are_unguessable_and_url_safe() {
        // Both properties matter and they are separate: the string has to survive being a path
        // segment, and it has to be worth nothing to someone who has seen a different one.
        let tickets = SpeechTickets::new();
        let minted: Vec<String> = (0..64)
            .map(|n| tickets.mint(prepared(&n.to_string())))
            .collect();
        let unique: std::collections::HashSet<&String> = minted.iter().collect();
        assert_eq!(unique.len(), minted.len(), "a ticket repeated");
        for ticket in &minted {
            assert_eq!(ticket.len(), 43, "not 256 bits of ticket: {ticket}");
            assert!(
                ticket
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "a ticket needs escaping to sit in a URL: {ticket}"
            );
        }
    }

    #[test]
    fn the_store_is_capped_and_evicts_the_oldest_rather_than_growing() {
        let tickets = SpeechTickets::new();
        let first = tickets.mint(prepared("oldest"));
        for n in 0..MAX_TICKETS {
            let _ = tickets.mint(prepared(&n.to_string()));
        }
        assert!(
            tickets.len() <= MAX_TICKETS,
            "the store grew past its cap: {}",
            tickets.len()
        );
        assert_eq!(
            tickets.claim(&first),
            None,
            "the oldest ticket survived while newer ones were minted over it"
        );
    }
}
