//! An append-only, credential-redacted record of everything the mock did.
//!
//! # Why a trace and not just assertions on the socket
//!
//! Half of what this mock exists to prove is a NEGATIVE: that the agent never called `/mcp`, that
//! no PCM was uploaded, that the account key never reached a URL. A negative cannot be asserted
//! from the socket, because the absence of a frame is indistinguishable from a frame that has not
//! arrived yet. The trace turns each of those into a positive statement about a finite list.
//!
//! # The redaction rule
//!
//! Every string that enters the trace goes through [`crate::elevenlabs::redact`] against every
//! registered secret — the account API key, the bridge bearer tokens, and each minted WebSocket
//! nonce (registered as it is minted, so the trace cannot be written before the secret is known).
//! Audio is recorded as a **length only**, never as base64: the same rule
//! [`crate::access`] already states for channel text, for the same reason — a trace is written to
//! a file and pasted into an issue.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::Secret;

/// Which way a recorded event was travelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// From the browser (or the test client) into the mock.
    Inbound,
    /// From the mock out to the browser.
    Outbound,
    /// Between the mock's agent brain and the vibe-talk bridge.
    Bridge,
    /// The mock's own bookkeeping, with no wire event.
    Internal,
}

/// One recorded event.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TraceEvent {
    /// Position in the trace, from zero.
    pub seq: u64,
    /// Milliseconds since the mock was spawned. Diagnostic only — nothing asserts on it, because
    /// wall-clock timing is the one part of this mock that cannot be deterministic.
    pub at_ms: u64,
    /// Which way it was going.
    pub direction: Direction,
    /// A short, stable label: the JSON event `type`, an MCP method, or a named mock condition.
    pub kind: String,
    /// Redacted human-readable detail.
    pub summary: String,
    /// Bytes involved, where that is meaningful. Audio records ONLY this.
    pub size: usize,
}

/// One recorded mint attempt against the HTTP half.
#[derive(Clone, Debug, serde::Serialize)]
pub struct MintRequest {
    /// The request URL, redacted. The account key must never appear here.
    pub url: String,
    /// Whether an `xi-api-key` header was present at all.
    pub api_key_header: bool,
    /// Whether the presented key was the one this mock account holds.
    pub api_key_accepted: bool,
    /// The status the mock answered with.
    pub status: u16,
}

#[derive(Debug, Default)]
struct Inner {
    events: Vec<TraceEvent>,
    mints: Vec<MintRequest>,
    secrets: Vec<Secret>,
}

/// The shared, cloneable trace handle.
#[derive(Clone, Debug)]
pub struct Trace {
    inner: Arc<Mutex<Inner>>,
    started: Instant,
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl Trace {
    /// An empty trace with no registered secrets.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            started: Instant::now(),
        }
    }

    /// Register a value that must never appear in this trace.
    ///
    /// Blank values are ignored: [`crate::elevenlabs::redact`] documents why an empty needle is
    /// not searched for, and registering one would be a way to silently do nothing.
    pub fn keep_secret(&self, value: &str) {
        if value.trim().is_empty() {
            return;
        }
        self.lock().secrets.push(Secret::new(value));
    }

    /// Redact a string against every registered secret.
    #[must_use]
    pub fn redacted(&self, text: &str) -> String {
        let guard = self.lock();
        let mut out = text.to_owned();
        for secret in &guard.secrets {
            out = crate::elevenlabs::redact(&out, secret);
        }
        out
    }

    /// Record an event. `summary` is redacted here, so no call site has to remember to.
    pub fn record(&self, direction: Direction, kind: &str, summary: &str, size: usize) {
        let summary = self.redacted(summary);
        let at_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut guard = self.lock();
        let seq = guard.events.len() as u64;
        guard.events.push(TraceEvent {
            seq,
            at_ms,
            direction,
            kind: kind.to_owned(),
            summary,
            size,
        });
    }

    /// Record a mint attempt against the HTTP half.
    pub fn record_mint(&self, mint: MintRequest) {
        let redacted = MintRequest {
            url: self.redacted(&mint.url),
            ..mint
        };
        self.lock().mints.push(redacted);
    }

    /// Every event, in order.
    #[must_use]
    pub fn events(&self) -> Vec<TraceEvent> {
        self.lock().events.clone()
    }

    /// Every mint attempt, in order.
    #[must_use]
    pub fn mint_requests(&self) -> Vec<MintRequest> {
        self.lock().mints.clone()
    }

    /// Whether a `conversation_initiation_client_data` was ever received.
    #[must_use]
    pub fn initiation_seen(&self) -> bool {
        self.count("conversation_initiation_client_data") > 0
    }

    /// JSON `ping` events sent that no matching `pong` came back for.
    ///
    /// This is the application-level ping ElevenLabs documents, not the RFC6455 control frame.
    #[must_use]
    pub fn pings_unanswered(&self) -> usize {
        self.count("ping").saturating_sub(self.count("pong"))
    }

    /// How many `user_audio_chunk` frames were accepted and decoded.
    #[must_use]
    pub fn pcm_frames(&self) -> usize {
        self.count("user_audio_chunk")
    }

    /// Total decoded PCM bytes received.
    #[must_use]
    pub fn pcm_bytes(&self) -> usize {
        self.lock()
            .events
            .iter()
            .filter(|e| e.kind == "user_audio_chunk")
            .map(|e| e.size)
            .sum()
    }

    /// The MCP methods the agent brain actually called, in order, as `initialize`,
    /// `notifications/initialized`, `tools/list`, `tools/call:<tool>`.
    #[must_use]
    pub fn mcp_methods(&self) -> Vec<String> {
        self.lock()
            .events
            .iter()
            .filter(|e| e.direction == Direction::Bridge)
            .map(|e| e.kind.clone())
            .collect()
    }

    /// Every event of one kind, in order.
    #[must_use]
    pub fn of_kind(&self, kind: &str) -> Vec<TraceEvent> {
        self.lock()
            .events
            .iter()
            .filter(|e| e.kind == kind)
            .cloned()
            .collect()
    }

    /// How many events of one kind were recorded.
    #[must_use]
    pub fn count(&self, kind: &str) -> usize {
        self.lock().events.iter().filter(|e| e.kind == kind).count()
    }

    /// The whole trace as JSON, for `GET /_mock/trace` and for the assertions that scan the bytes
    /// for a leaked credential.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let guard = self.lock();
        serde_json::json!({
            "events": guard.events,
            "mints": guard.mints,
        })
    }

    /// Forget everything except the registered secrets.
    ///
    /// The secrets survive a reset on purpose: they are properties of the running mock, and a
    /// reset that dropped them would leave the next events unredacted.
    pub fn reset(&self) {
        let mut guard = self.lock();
        guard.events.clear();
        guard.mints.clear();
    }

    /// Write the trace to `path` as JSON Lines, one event per line.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error; the caller is a binary writing a diagnostic file, and a
    /// failure there should be said out loud rather than swallowed.
    pub fn write_jsonl(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write as _;
        let guard = self.lock();
        let mut file = std::fs::File::create(path)?;
        for event in &guard.events {
            writeln!(
                file,
                "{}",
                serde_json::to_string(event).unwrap_or_else(|e| format!(r#"{{"error":"{e}"}}"#))
            )?;
        }
        for mint in &guard.mints {
            writeln!(
                file,
                "{}",
                serde_json::to_string(mint).unwrap_or_else(|e| format!(r#"{{"error":"{e}"}}"#))
            )?;
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registered_secret_never_reaches_a_summary() {
        let trace = Trace::new();
        trace.keep_secret("xi-account-key");
        trace.record(
            Direction::Inbound,
            "mint",
            "the key xi-account-key was rejected",
            0,
        );
        let rendered = trace.to_json().to_string();
        assert!(!rendered.contains("xi-account-key"), "{rendered}");
        assert!(rendered.contains(crate::elevenlabs::REDACTED), "{rendered}");
    }

    #[test]
    fn a_blank_secret_is_ignored_rather_than_shredding_the_text() {
        let trace = Trace::new();
        trace.keep_secret("");
        trace.keep_secret("   ");
        trace.record(Direction::Internal, "note", "hello", 0);
        assert_eq!(trace.events()[0].summary, "hello");
    }

    #[test]
    fn a_secret_registered_late_still_covers_events_written_after_it() {
        // Nonces are minted mid-run; the ordering matters, so it is stated by a test.
        let trace = Trace::new();
        trace.record(Direction::Internal, "before", "nonce-1", 0);
        trace.keep_secret("nonce-1");
        trace.record(Direction::Internal, "after", "nonce-1", 0);
        assert_eq!(trace.of_kind("before")[0].summary, "nonce-1");
        assert_eq!(
            trace.of_kind("after")[0].summary,
            crate::elevenlabs::REDACTED
        );
    }

    #[test]
    fn the_typed_accessors_read_the_event_list() {
        let trace = Trace::new();
        trace.record(
            Direction::Inbound,
            "conversation_initiation_client_data",
            "",
            0,
        );
        trace.record(Direction::Outbound, "ping", "event_id 1", 0);
        trace.record(Direction::Outbound, "ping", "event_id 2", 0);
        trace.record(Direction::Inbound, "pong", "event_id 1", 0);
        trace.record(Direction::Inbound, "user_audio_chunk", "320 bytes", 320);
        trace.record(Direction::Bridge, "initialize", "ok", 0);
        trace.record(Direction::Bridge, "tools/call:digest_channel", "ok", 0);

        assert!(trace.initiation_seen());
        assert_eq!(trace.pings_unanswered(), 1);
        assert_eq!(trace.pcm_frames(), 1);
        assert_eq!(trace.pcm_bytes(), 320);
        assert_eq!(
            trace.mcp_methods(),
            vec!["initialize", "tools/call:digest_channel"]
        );
    }

    #[test]
    fn more_pongs_than_pings_does_not_underflow_the_count() {
        let trace = Trace::new();
        trace.record(Direction::Inbound, "pong", "", 0);
        assert_eq!(trace.pings_unanswered(), 0);
    }

    #[test]
    fn a_reset_keeps_the_secrets_it_was_told() {
        let trace = Trace::new();
        trace.keep_secret("nonce-9");
        trace.record(Direction::Internal, "a", "nonce-9", 0);
        trace.reset();
        assert!(trace.events().is_empty());
        trace.record(Direction::Internal, "b", "nonce-9", 0);
        assert_eq!(trace.events()[0].summary, crate::elevenlabs::REDACTED);
    }

    #[test]
    fn a_mint_url_is_redacted_on_the_way_in() {
        let trace = Trace::new();
        trace.keep_secret("xi-key");
        trace.record_mint(MintRequest {
            url: "http://127.0.0.1:1/v1/get-signed-url?agent_id=a&leak=xi-key".to_owned(),
            api_key_header: true,
            api_key_accepted: false,
            status: 401,
        });
        assert!(!trace.mint_requests()[0].url.contains("xi-key"));
    }
}
