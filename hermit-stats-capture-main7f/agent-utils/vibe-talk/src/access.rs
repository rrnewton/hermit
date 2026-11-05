//! The access log: one line per request, so that **silence is legible**.
//!
//! # Why this exists
//!
//! A voice agent reported reading a Discord channel and posting a reply. It had done neither. It
//! described a digest it had invented, and it named tools this server does not have
//! (`file_system_read`, `code_execution_sandbox`, `web_scraper`). The only way to disprove it was
//! to go and look at the channel, because at `RUST_LOG=info` this server logged **nothing per
//! request** — so "the agent never called us" and "the agent called us and we answered" produced
//! byte-identical output: none.
//!
//! That is the failure this module removes. An absence of evidence was doing the work of evidence
//! of absence, and it could not. Every request now leaves exactly one INFO line, so an empty log
//! is a *finding* — nobody called — rather than an ambiguity. The startup banner says so out
//! loud, because a reader has to know the log would have spoken.
//!
//! # What is deliberately NOT in a line
//!
//! * **No credential, ever.** Not the token, not a prefix of it, not a hash of it. A hash prefix
//!   still lets an attacker who has the log confirm a guess, and there is no operational question
//!   here that a token value answers. What matters is *which class* of credential arrived —
//!   read or write — and [`Credential`] carries exactly that much.
//! * **No channel text at INFO.** Message content is third-party data written by other people; it
//!   does not belong in an operator's log or in whatever ships that log onward. Ids, counts, and
//!   lengths answer the operational questions ("did the post go through, and how long was it?")
//!   without copying the content anywhere.

use crate::auth::{self, Scope};
use crate::config::{AuthConfig, Secret};

/// The tracing target every access line carries, so an operator can filter for exactly these:
/// `RUST_LOG=vibe_talk::access=info`.
///
/// The `tracing` macros need a literal, so this constant is the documentation and the tests'
/// anchor rather than the thing the macros interpolate. [`Self`]-consistency is asserted in the
/// tests below.
pub const TARGET: &str = "vibe_talk::access";

/// Which class of credential a request arrived with. Never the credential itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Credential {
    /// No `Authorization` header, or one that is not a bearer token.
    Absent,
    /// A bearer token that matches neither configured token.
    Unrecognized,
    /// The read-scope token.
    Read,
    /// The write-scope token.
    Write,
    /// The adapter-only live-event ingestion token.
    Ingest,
}

impl Credential {
    /// Classify the `Authorization` header of a request.
    ///
    /// "Absent" and "unrecognized" are kept apart because they are different incidents: the first
    /// is usually a misconfigured client, the second is usually a stale token or a stranger.
    #[must_use]
    pub fn classify(header: Option<&str>, config: &AuthConfig) -> Self {
        match auth::scope_of(header, config) {
            Some(Scope::Write) => Self::Write,
            Some(Scope::Read) => Self::Read,
            None if auth::bearer_token(header).is_none() => Self::Absent,
            None => Self::Unrecognized,
        }
    }

    /// Classify an ordinary API token or the optional adapter-only credential.
    #[must_use]
    pub fn classify_request(
        header: Option<&str>,
        config: &AuthConfig,
        ingest: Option<&Secret>,
    ) -> Self {
        let classified = Self::classify(header, config);
        if classified != Self::Unrecognized {
            return classified;
        }
        match (auth::bearer_token(header), ingest) {
            (Some(presented), Some(expected)) if expected.matches(presented) => Self::Ingest,
            _ => Self::Unrecognized,
        }
    }

    /// The word that appears in a log line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Unrecognized => "unrecognized",
            Self::Read => "read",
            Self::Write => "write",
            Self::Ingest => "ingest",
        }
    }
}

impl std::fmt::Display for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<Scope> for Credential {
    /// A request that reached the MCP dispatcher has already authenticated, so only the two
    /// recognized classes are reachable here.
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::Read => Self::Read,
            Scope::Write => Self::Write,
        }
    }
}

/// How a tool call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    /// The tool ran and produced a result.
    Ok,
    /// The call was refused, either by the scope fence or by an operational rule.
    Refused,
}

impl ToolOutcome {
    /// The word that appears in a log line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Refused => "refused",
        }
    }
}

impl std::fmt::Display for ToolOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One line for one HTTP request, emitted after the response status is known.
pub fn request(method: &str, path: &str, credential: Credential, status: u16, millis: u128) {
    tracing::info!(
        target: "vibe_talk::access",
        method,
        path,
        credential = credential.as_str(),
        status,
        millis,
        "request"
    );
}

/// One line for one JSON-RPC message arriving at `/mcp`.
///
/// Logged separately from [`request`] because the HTTP line cannot see inside the body: every MCP
/// call is a `POST /mcp`, so without this the log could not distinguish `tools/list` from an
/// actual `tools/call`.
pub fn rpc(rpc_method: &str, credential: Credential, is_notification: bool) {
    tracing::info!(
        target: "vibe_talk::access",
        rpc_method,
        credential = credential.as_str(),
        is_notification,
        "mcp"
    );
}

/// One line for one tool call: which tool, which channel, whether it was allowed, and how long it
/// took.
///
/// `reason` is the machine-readable refusal code — `forbidden_scope`, `unknown_channel`,
/// `channel_not_writable`, `unknown_tool` — never free text from a channel. `millis` is measured
/// from the moment the call was parsed. `#210 voice-catchup-speed`: without it a tool's latency
/// could only be recovered by pairing this line with its `request` line, and that line is missing
/// in exactly the case that matters — a call the caller gave up on.
pub fn tool_call(
    tool: &str,
    channel: Option<&str>,
    credential: Credential,
    outcome: ToolOutcome,
    reason: Option<&str>,
    text_len: Option<usize>,
    millis: u128,
) {
    tracing::info!(
        target: "vibe_talk::access",
        tool,
        channel = channel.unwrap_or("-"),
        credential = credential.as_str(),
        outcome = outcome.as_str(),
        reason = reason.unwrap_or("-"),
        // A LENGTH, not the text. Enough to answer "did the whole message go through?" without
        // putting a word of it in the log.
        text_len = text_len.map_or(-1_i64, |n| i64::try_from(n).unwrap_or(i64::MAX)),
        millis,
        "tool"
    );
}

/// One line for a tool call the caller stopped waiting for. `#210 voice-catchup-speed`.
///
/// A voice runtime gives a tool a fixed time and then drops the request. The server's handler is
/// dropped with it, mid-call, so neither [`tool_call`] nor [`request`] is ever written, and the
/// one call that mattered most left no trace here at all. This is written from that drop.
pub fn tool_abandoned(tool: &str, channel: Option<&str>, credential: Credential, millis: u128) {
    tracing::info!(
        target: "vibe_talk::access",
        tool,
        channel = channel.unwrap_or("-"),
        credential = credential.as_str(),
        outcome = "abandoned",
        millis,
        "tool"
    );
}

/// One line for one step of a post proposal's life: proposed, committed, refused, cancelled,
/// superseded, or expired. `#34 voice-chat-write-confirm`.
///
/// The serial correlates the steps of one proposal. The handle is deliberately absent — it is a
/// capability — and so is every word of the text: a length answers "was it the message that was
/// read back?" well enough without copying it. `confirmed_by` is the confirming party the commit
/// named (`ui` or `speaker_turn`), `-` for any step that is not a commit.
pub fn post_gate(
    event: &str,
    serial: Option<u64>,
    channel: Option<&str>,
    text_len: Option<usize>,
    reason: Option<&str>,
    confirmed_by: Option<&str>,
) {
    tracing::info!(
        target: "vibe_talk::access",
        event,
        proposal = serial.map_or(-1_i64, |n| i64::try_from(n).unwrap_or(i64::MAX)),
        channel = channel.unwrap_or("-"),
        text_len = text_len.map_or(-1_i64, |n| i64::try_from(n).unwrap_or(i64::MAX)),
        reason = reason.unwrap_or("-"),
        confirmed_by = confirmed_by.unwrap_or("-"),
        "post_gate"
    );
}

/// The arguments of a tool call, at DEBUG.
///
/// This is the one line that can carry channel text — `post_reply` puts the message the owner is
/// about to send in its arguments — so it is DEBUG and stays DEBUG. At the INFO level an operator
/// runs in production, the access log records that a post happened, to which channel, and how
/// long it was; it does not copy what was said into a file that gets shipped somewhere else.
pub fn tool_arguments(tool: &str, arguments: &serde_json::Value) {
    tracing::debug!(
        target: "vibe_talk::access",
        tool,
        arguments = %arguments,
        "tool arguments"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;

    fn config() -> AuthConfig {
        AuthConfig {
            read_token: Secret::new("read-token-that-is-long-enough"),
            write_token: Secret::new("write-token-that-is-long-enough"),
        }
    }

    #[test]
    fn the_documented_target_is_the_one_the_macros_use() {
        // The macros need a literal, so the constant could drift from them silently. This is the
        // check that an operator following `RUST_LOG=vibe_talk::access=info` from the README gets
        // the lines this module emits.
        let capture = crate::testing::LogCapture::start();
        request("GET", "/healthz", Credential::Absent, 200, 0);
        assert!(
            capture.text().contains(TARGET),
            "the emitted target does not match TARGET: {}",
            capture.text()
        );
    }

    #[test]
    fn a_credential_is_classified_without_being_reproduced() {
        assert_eq!(Credential::classify(None, &config()), Credential::Absent);
        assert_eq!(
            Credential::classify(Some("not-a-bearer-header"), &config()),
            Credential::Absent
        );
        assert_eq!(
            Credential::classify(Some("Bearer something-else-entirely"), &config()),
            Credential::Unrecognized
        );
        assert_eq!(
            Credential::classify(Some("Bearer read-token-that-is-long-enough"), &config()),
            Credential::Read
        );
        assert_eq!(
            Credential::classify(Some("Bearer write-token-that-is-long-enough"), &config()),
            Credential::Write
        );
        assert_eq!(
            Credential::classify_request(
                Some("Bearer ingest-token-that-is-long-enough"),
                &config(),
                Some(&Secret::new("ingest-token-that-is-long-enough")),
            ),
            Credential::Ingest
        );
        for class in [
            Credential::Absent,
            Credential::Unrecognized,
            Credential::Read,
            Credential::Write,
            Credential::Ingest,
        ] {
            let rendered = format!("{class}");
            assert!(
                !rendered.contains("token-that-is-long-enough"),
                "a credential class rendered part of a token: {rendered}"
            );
        }
    }
}
