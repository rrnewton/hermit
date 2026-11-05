//! Talking to the ElevenLabs conversational agent in TEXT, over its own WebSocket.
//!
//! # Why a socket and not an endpoint
//!
//! There is no "send this text to my agent, get text back" REST call. The agent is reachable only
//! through the conversation WebSocket the browser uses, so a server that wants one sentence out of
//! it has to open a conversation, be a client for the length of one turn, and hang up.
//! `simulate_conversation` looks like the missing endpoint and is not: it is deprecated, and what
//! it simulates is the *user*, which is the half we already have.
//!
//! # `text_only` is the whole reason this is affordable
//!
//! The initiation carries `conversation_config_override.text_only = true`, which suppresses speech
//! synthesis for the conversation. Without it every turn would also render audio nobody listens
//! to, and audio is the expensive half.
//!
//! # Shape of this module
//!
//! The same shape as [`super::http`]: the parts with judgment in them — which frames go out, and
//! how an inbound frame is classified — are pure functions, unit-tested below. The async wrapper
//! is deliberately thin, because it is the only part that cannot be exercised without a live
//! account. [`crate::elevenlabs::mock`] closes most of that gap by speaking the same protocol on
//! loopback.
//!
//! # NOTHING HERE HAS MET THE LIVE VENDOR
//!
//! Every frame name, every nesting, and the `text_only` override itself were read out of the
//! vendor's SDK rather than observed on a billed conversation. The mock this repository already
//! ships speaks the same shapes, so the two agree — but they agree with each other, not with
//! ElevenLabs. The first real run is the experiment, which is exactly why
//! [`crate::summarize::agent`] measures and logs its own round-trip time.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::{
    chat_from_signed, credentials, redact_all, ChatError, SignedUrlProvider, TextChat,
    TextChatProvider,
};
use crate::config::{ElevenLabsConfig, Secret};

/// The initiation the vendor requires before anything else, asking for a text-only conversation.
///
/// `text_only` genuinely suppresses audio generation rather than merely muting playback, which is
/// the difference between a summary that costs a few text tokens and one that also pays for speech
/// nobody will hear.
#[must_use]
pub fn initiation_frame() -> Value {
    json!({
        "type": "conversation_initiation_client_data",
        "conversation_config_override": { "text_only": true },
    })
}

/// One thing said to the agent, in the shape the page says it.
#[must_use]
pub fn user_message_frame(text: &str) -> Value {
    json!({ "type": "user_message", "text": text })
}

/// The answer to the vendor's APPLICATION-level `ping`.
///
/// Not the RFC6455 control frame of the same word — [`tokio_tungstenite`] answers that one on its
/// own. This is a JSON event, and a conversation that stops echoing the `event_id` back is closed
/// by the vendor for being idle. See [`crate::elevenlabs::mock::agent`], which models both and
/// deliberately records them under different names so a test cannot confuse them.
#[must_use]
pub fn pong_frame(ping: &Value) -> Value {
    json!({ "type": "pong", "event_id": ping["ping_event"]["event_id"].clone() })
}

/// What one inbound event means to a client that only wants the agent's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    /// The vendor accepted the initiation. The conversation is open.
    Ready,
    /// The agent's complete answer for this turn.
    Reply(String),
    /// One streamed fragment of an answer still being written.
    Delta(String),
    /// An application-level ping that has to be answered. Carries the whole event, because the
    /// `event_id` to echo lives inside it.
    Ping(Value),
    /// Something this client does not need: the transcript of what we just said, an interruption,
    /// an audio frame it did not ask for. Carries the `type`, so a diagnostic can say what was
    /// ignored rather than "an event".
    Ignored(String),
}

/// Classify one inbound event.
///
/// Deliberately total: an event this server has never heard of is [`AgentEvent::Ignored`] and not
/// an error, because a vendor is free to add events and a summariser that failed on an unknown one
/// would break on an upgrade it had no part in.
#[must_use]
pub fn classify(value: &Value) -> AgentEvent {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "conversation_initiation_metadata" => AgentEvent::Ready,
        "ping" => AgentEvent::Ping(value.clone()),
        "agent_response" => AgentEvent::Reply(
            value
                .get("agent_response_event")
                .and_then(|e| e.get("agent_response"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        // A correction replaces what was said, so it is a reply in its own right rather than a
        // fragment appended to one. The page renders it over the previous line for the same reason.
        "agent_response_correction" => AgentEvent::Reply(
            value
                .get("agent_response_correction_event")
                .and_then(|e| e.get("corrected_agent_response"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        "agent_chat_response_part" => AgentEvent::Delta(
            value
                .get("text_response_part")
                .and_then(Value::as_str)
                .or_else(|| {
                    value
                        .get("text_response_part")
                        .and_then(|p| p.get("text"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default()
                .to_owned(),
        ),
        other => AgentEvent::Ignored(other.to_owned()),
    }
}

/// Opens real conversations against the real vendor.
///
/// Holds the mint rather than performing it, so this and the browser path cannot drift into two
/// ideas of how a signed URL is obtained — and so the whole [`super::SignedUrlError`] taxonomy is
/// exercised here too.
pub struct WebSocketTextChatProvider {
    mint: Arc<dyn SignedUrlProvider>,
}

impl std::fmt::Debug for WebSocketTextChatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebSocketTextChatProvider")
    }
}

impl WebSocketTextChatProvider {
    /// Wrap a mint.
    #[must_use]
    pub fn new(mint: Arc<dyn SignedUrlProvider>) -> Self {
        Self { mint }
    }

    /// [`TextChatProvider::open_text_chat`], unboxed.
    ///
    /// The trait method is a thin wrapper around this one so that a test can hold the CONCRETE
    /// conversation and ask it what it will scrub. Behind `Box<dyn TextChat>` that question cannot
    /// be asked at all, and a redaction nobody can interrogate is a redaction nobody can prove:
    /// a test that builds its own secret list and redacts against it certifies the helper, not
    /// the call site that forgot to put the minted URL into one.
    ///
    /// # Errors
    ///
    /// [`ChatError`], distinguishing an absent setting from a refusal from unreachability.
    pub async fn open(
        &self,
        config: &ElevenLabsConfig,
        deadline: Duration,
    ) -> Result<WebSocketTextChat, ChatError> {
        // The same gate the browser path goes through, first and before any call: an unconfigured
        // server has to fail by NAMING the setting, not by dialling nothing.
        let (_agent_id, api_key) = credentials(config).map_err(chat_from_signed)?;
        let api_key = api_key.clone();
        let minted = self
            .mint
            .signed_url(config)
            .await
            .map_err(chat_from_signed)?;
        // The minted URL carries a single-use token, so it is itself a credential and is scrubbed
        // out of diagnostics exactly as the account key is. A connect error's text quotes the URL
        // it failed to reach, which is precisely where this would otherwise leak.
        let secrets = vec![api_key, Secret::new(minted.signed_url.clone())];
        let connect = tokio_tungstenite::connect_async(&minted.signed_url);
        let socket = match tokio::time::timeout(deadline, connect).await {
            Ok(Ok((socket, _response))) => socket,
            Ok(Err(error)) => {
                return Err(ChatError::Transport(redact_all(
                    &error.to_string(),
                    &secrets,
                )))
            }
            Err(_) => {
                return Err(ChatError::Transport(format!(
                    "waited {deadline:?} for the conversation socket to open and it did not"
                )))
            }
        };
        let mut chat = WebSocketTextChat { socket, secrets };
        chat.initiate(deadline).await?;
        Ok(chat)
    }
}

#[async_trait]
impl TextChatProvider for WebSocketTextChatProvider {
    async fn open_text_chat(
        &self,
        config: &ElevenLabsConfig,
        deadline: Duration,
    ) -> Result<Box<dyn TextChat>, ChatError> {
        Ok(Box::new(self.open(config, deadline).await?))
    }
}

/// One live conversation.
#[derive(Debug)]
pub struct WebSocketTextChat {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// Everything that must never appear in an error built from this conversation: the account
    /// key, and the signed URL's single-use token.
    secrets: Vec<Secret>,
}

impl WebSocketTextChat {
    /// Everything this conversation will scrub out of its own diagnostics.
    ///
    /// Exposed so a test can assert on the LIST rather than on a list it built itself. There are
    /// two entries and the second is the one that is easy to omit: the minted URL carries a
    /// single-use token, and a connect failure quotes the URL verbatim.
    #[must_use]
    pub fn scrubbed(&self) -> &[Secret] {
        &self.secrets
    }

    /// Send the initiation and wait for the vendor to acknowledge it.
    ///
    /// Waiting matters: a conversation that is written to before the metadata arrives is a real
    /// client bug, and the vendor's own answer to it is a protocol close several steps later.
    async fn initiate(&mut self, deadline: Duration) -> Result<(), ChatError> {
        self.send(&initiation_frame()).await?;
        let start = tokio::time::Instant::now();
        loop {
            let left = remaining(start, deadline, "the conversation to be accepted")?;
            match self.read(left).await? {
                AgentEvent::Ready => return Ok(()),
                AgentEvent::Ping(ping) => self.send(&pong_frame(&ping)).await?,
                // Anything the vendor volunteers before the metadata is not this client's problem;
                // what matters is that the metadata eventually arrives or the deadline passes.
                AgentEvent::Reply(_) | AgentEvent::Delta(_) | AgentEvent::Ignored(_) => {}
            }
        }
    }

    /// Write one JSON event.
    async fn send(&mut self, event: &Value) -> Result<(), ChatError> {
        let secrets = self.secrets.clone();
        self.socket
            .send(Message::Text(event.to_string().into()))
            .await
            .map_err(|e| ChatError::Transport(redact_all(&e.to_string(), &secrets)))?;
        self.socket
            .flush()
            .await
            .map_err(|e| ChatError::Transport(redact_all(&e.to_string(), &secrets)))
    }

    /// Read one application event, skipping frames that carry none.
    async fn read(&mut self, left: Duration) -> Result<AgentEvent, ChatError> {
        loop {
            let frame = tokio::time::timeout(left, self.socket.next())
                .await
                .map_err(|_| {
                    ChatError::Transport(format!("waited {left:?} and the agent said nothing"))
                })?;
            match frame {
                Some(Ok(Message::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).map_err(|e| {
                        ChatError::Shape(redact_all(
                            &format!("an event was not JSON: {e}"),
                            &self.secrets,
                        ))
                    })?;
                    return Ok(classify(&value));
                }
                // A close IS the vendor refusing, and the code is the only thing that says why.
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = frame
                        .map(|f| (u16::from(f.code), f.reason.to_string()))
                        // 1005 is what a browser reports for a close with no status, so a
                        // diagnostic written here reads the same as one copied off a phone.
                        .unwrap_or((1005, String::new()));
                    return Err(ChatError::from_close(code, &reason, &self.secrets));
                }
                Some(Err(error)) => {
                    return Err(ChatError::Transport(redact_all(
                        &error.to_string(),
                        &self.secrets,
                    )))
                }
                None => {
                    return Err(ChatError::Transport(
                        "the conversation ended with no close frame".to_owned(),
                    ))
                }
                // Control frames and binary audio. This client asked for text only; if audio
                // arrives anyway the override did not take, and dropping it is still correct.
                Some(Ok(_)) => {}
            }
        }
    }
}

#[async_trait]
impl TextChat for WebSocketTextChat {
    async fn ask(&mut self, text: &str, deadline: Duration) -> Result<String, ChatError> {
        self.send(&user_message_frame(text)).await?;
        let start = tokio::time::Instant::now();
        // Deltas are accumulated but NOT preferred: `agent_response` is the vendor's own final
        // word for the turn and is what the page renders. The accumulation exists for the case
        // where the stream produced text and then stopped without a final event — answering with
        // what was actually said beats answering with nothing.
        let mut streamed = String::new();
        loop {
            let left = remaining(start, deadline, "the agent to answer")?;
            match self.read(left).await {
                Ok(AgentEvent::Reply(reply)) => return Ok(reply),
                Ok(AgentEvent::Delta(part)) => streamed.push_str(&part),
                Ok(AgentEvent::Ping(ping)) => self.send(&pong_frame(&ping)).await?,
                Ok(AgentEvent::Ready | AgentEvent::Ignored(_)) => {}
                Err(error) => {
                    if streamed.trim().is_empty() {
                        return Err(error);
                    }
                    tracing::warn!(
                        %error,
                        "the conversation ended mid-answer; using the text that had already \
                         streamed rather than discarding a turn that was already paid for"
                    );
                    return Ok(streamed);
                }
            }
        }
    }

    async fn close(mut self: Box<Self>) {
        // Nothing to recover if the goodbye fails, but it is still sent: ElevenLabs bills
        // connection time, and a socket left to time out is billed for the timeout.
        let _ = self.socket.close(None).await;
    }
}

/// How much of `deadline` is left, or the transport error that says it ran out.
fn remaining(
    start: tokio::time::Instant,
    deadline: Duration,
    waiting_for: &str,
) -> Result<Duration, ChatError> {
    deadline.checked_sub(start.elapsed()).ok_or_else(|| {
        ChatError::Transport(format!("waited {deadline:?} for {waiting_for} and gave up"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initiation_asks_for_text_only() {
        // The one field that decides whether this feature costs text tokens or text tokens plus
        // speech. A mutation dropping it would be invisible everywhere else.
        let frame = initiation_frame();
        assert_eq!(frame["type"], "conversation_initiation_client_data");
        assert_eq!(
            frame["conversation_config_override"]["text_only"],
            Value::Bool(true),
            "without text_only the agent renders audio nobody listens to: {frame}"
        );
    }

    #[test]
    fn a_question_goes_out_as_a_user_message() {
        let frame = user_message_frame("summarise this");
        assert_eq!(frame["type"], "user_message");
        assert_eq!(frame["text"], "summarise this");
    }

    #[test]
    fn a_pong_echoes_the_event_id_it_was_pinged_with() {
        // Echoing the wrong id, or none, is how a conversation gets closed for being idle while
        // it is in fact answering.
        let ping = json!({"type": "ping", "ping_event": {"event_id": 7, "ping_ms": 0}});
        let pong = pong_frame(&ping);
        assert_eq!(pong["type"], "pong");
        assert_eq!(pong["event_id"], 7);
    }

    #[test]
    fn the_agents_answer_is_read_out_of_the_event_the_vendor_nests_it_in() {
        assert_eq!(
            classify(&json!({
                "type": "agent_response",
                "agent_response_event": {"agent_response": "the runner stalled"}
            })),
            AgentEvent::Reply("the runner stalled".to_owned())
        );
    }

    #[test]
    fn streamed_fragments_are_recognised_as_fragments_and_not_as_answers() {
        // If a delta were classified as a reply, the first word of a streamed answer would be
        // returned as the whole summary.
        assert_eq!(
            classify(&json!({
                "type": "agent_chat_response_part",
                "text_response_part": "the runner "
            })),
            AgentEvent::Delta("the runner ".to_owned())
        );
    }

    #[test]
    fn every_event_this_client_has_to_handle_is_told_apart() {
        assert_eq!(
            classify(&json!({"type": "conversation_initiation_metadata"})),
            AgentEvent::Ready
        );
        assert!(matches!(
            classify(&json!({"type": "ping", "ping_event": {"event_id": 1}})),
            AgentEvent::Ping(_)
        ));
        for ignorable in ["user_transcript", "interruption", "audio", "vad_score"] {
            assert_eq!(
                classify(&json!({"type": ignorable})),
                AgentEvent::Ignored(ignorable.to_owned()),
                "{ignorable} must be ignored rather than mistaken for an answer"
            );
        }
    }

    #[test]
    fn an_event_this_server_has_never_heard_of_is_ignored_rather_than_fatal() {
        // A vendor is free to add events. A summariser that failed on one would break on an
        // upgrade it had no part in.
        assert_eq!(
            classify(&json!({"type": "something_invented_next_year"})),
            AgentEvent::Ignored("something_invented_next_year".to_owned())
        );
        assert_eq!(classify(&json!({})), AgentEvent::Ignored(String::new()));
    }

    #[test]
    fn a_correction_replaces_the_answer_rather_than_extending_it() {
        assert_eq!(
            classify(&json!({
                "type": "agent_response_correction",
                "agent_response_correction_event": {
                    "original_agent_response": "wrong",
                    "corrected_agent_response": "right",
                }
            })),
            AgentEvent::Reply("right".to_owned())
        );
    }

    #[test]
    fn redaction_composes_over_more_than_one_secret() {
        // This proves the HELPER and nothing else: it builds its own list, so a call site that
        // forgot to put the minted URL into one would sail past it. The claim that the real
        // conversation actually holds both is
        // `tests/agent_summaries.rs::the_live_socket_client_really_holds_a_conversation_over_a_real_websocket`,
        // which asks a conversation opened against the mock what it will scrub.
        let secrets = vec![
            Secret::new("xi-account-key"),
            Secret::new("wss://api.elevenlabs.io/x?token=nonce-abc"),
        ];
        let scrubbed = redact_all(
            "could not reach wss://api.elevenlabs.io/x?token=nonce-abc with key xi-account-key",
            &secrets,
        );
        assert!(!scrubbed.contains("xi-account-key"), "{scrubbed}");
        assert!(!scrubbed.contains("nonce-abc"), "{scrubbed}");
    }
}
