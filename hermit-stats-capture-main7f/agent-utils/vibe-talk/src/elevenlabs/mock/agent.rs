//! The brain behind the mock's WebSocket: one conversation, held for real.
//!
//! # Two different things are called "ping"
//!
//! RFC6455 has **control frames** with opcode `0x9`/`0xA`, which the transport answers by itself.
//! ElevenLabs *also* sends an application-level JSON event `{"type":"ping","ping_event":{…}}`
//! that the page answers with a JSON `pong`. They are unrelated. This module speaks the JSON one;
//! [`tokio_tungstenite`] handles the control frames, and both are recorded in the trace under
//! distinct names so a test cannot accidentally assert on the wrong one.
//!
//! # Why the turn is a real MCP round trip
//!
//! The failure this whole mock exists to reproduce is *an agent that answers without calling any
//! tool*. That cannot be reproduced by a canned reply, because a canned reply is what the bug
//! looks like. So the honest path — [`Scenario::Full`] — really does POST `initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call list_channels` and
//! `tools/call digest_channel` at the vibe-talk bridge it was pointed at, and really does build
//! the spoken answer out of the text those calls returned. [`Scenario::NoToolCall`] is then the
//! same code path with the tools skipped, which is exactly the difference the bug was.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use super::audio;
use super::trace::Direction;
use super::{MockState, Say, Scenario, Speaker};

/// How many audio frames an answer is spoken in, and how long each one is.
///
/// Fixed rather than proportional to the text: the point is that a page receives playable PCM,
/// and a length that moved with the wording would make every trace assertion restate the wording.
const AUDIO_FRAMES: usize = 3;
const SAMPLES_PER_FRAME: usize = 160;

type Sink = SplitSink<WebSocketStream<TcpStream>, Message>;
type Source = SplitStream<WebSocketStream<TcpStream>>;

/// What ended a conversation.
enum Ending {
    /// Say goodbye properly.
    Closed,
    /// Drop the TCP connection with no close frame, which a browser reports as code 1006.
    Dropped,
}

/// Accept one connection, check its single-use nonce, and hold the conversation.
///
/// Everything is recorded in the trace, including the refusals: a browser only ever sees an
/// immediate close, so "you never had a token", "you already used it" and "you took too long"
/// have to be recoverable from somewhere other than the socket.
// The handshake callback's error type is `http::Response<Option<String>>`, fixed by
// tungstenite's `Callback` trait. Boxing it — clippy's suggestion — is not available to us, and
// this is one allocation per refused handshake on loopback.
#[allow(clippy::result_large_err)]
pub async fn serve_connection(state: Arc<MockState>, stream: TcpStream) {
    let mut refusal: Option<&'static str> = None;
    let mut agent_id = String::new();
    let accepted = {
        let state = &state;
        let refusal = &mut refusal;
        let agent_id = &mut agent_id;
        tokio_tungstenite::accept_hdr_async(
            stream,
            move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                  response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let query = request.uri().query().unwrap_or("").to_owned();
                *agent_id = query_param(&query, "agent_id").unwrap_or_default();
                let token = query_param(&query, "token").unwrap_or_default();
                match state.spend_nonce(&token) {
                    Ok(()) => Ok(response),
                    Err(reason) => {
                        *refusal = Some(reason);
                        let body = format!("{{\"error\":\"{reason}\"}}");
                        Err(http_error(reason, body))
                    }
                }
            },
        )
        .await
    };

    let socket = match accepted {
        Ok(socket) => socket,
        Err(error) => {
            let kind = refusal.unwrap_or("handshake_failed");
            state
                .trace()
                .record(Direction::Inbound, kind, &error.to_string(), 0);
            return;
        }
    };
    state
        .trace()
        .record(Direction::Inbound, "socket_open", &agent_id, 0);

    match converse(&state, socket).await {
        Ending::Closed => state
            .trace()
            .record(Direction::Internal, "socket_closed", "", 0),
        Ending::Dropped => state.trace().record(
            Direction::Internal,
            "socket_dropped",
            "the connection was dropped mid-turn with no close frame",
            0,
        ),
    }
}

/// Build the HTTP rejection tungstenite writes when the handshake callback refuses.
fn http_error(
    reason: &'static str,
    body: String,
) -> tokio_tungstenite::tungstenite::handshake::server::ErrorResponse {
    use tokio_tungstenite::tungstenite::http::{HeaderValue, Response, StatusCode};
    let mut response = Response::new(Some(body));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(
        "x-mock-refusal",
        HeaderValue::from_static(match reason {
            "token_reused" => "token_reused",
            "token_expired" => "token_expired",
            _ => "token_unknown",
        }),
    );
    response
}

/// Read one query parameter out of a raw query string, without a URL crate.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| value.to_owned())
    })
}

async fn converse(state: &Arc<MockState>, socket: WebSocketStream<TcpStream>) -> Ending {
    let (mut tx, mut rx) = socket.split();

    // The vendor requires the client to speak first. A page that connects and starts uploading
    // audio without an initiation is a real bug, and it must not look like a working call.
    match wait_for_initiation(state, &mut rx).await {
        Some(true) => {}
        Some(false) => {
            state.trace().record(
                Direction::Internal,
                "initiation_missing",
                "the client sent an event before conversation_initiation_client_data",
                0,
            );
            let _ = tx
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Protocol,
                    reason: "initiation_missing".into(),
                })))
                .await;
            let _ = tx.flush().await;
            return Ending::Closed;
        }
        None => return Ending::Closed,
    }

    let conversation = state.next_conversation_id();
    let format = state.scenario().audio_format();
    send(
        state,
        &mut tx,
        "conversation_initiation_metadata",
        &json!({
            "type": "conversation_initiation_metadata",
            "conversation_initiation_metadata_event": {
                "conversation_id": conversation,
                "agent_output_audio_format": format,
                "user_input_audio_format": super::PCM_FORMAT,
            }
        }),
    )
    .await;

    let mut say_rx = state.subscribe();
    let mut event_id: u64 = 0;
    let mut ping_ticker = state.options().ping_every.map(|every| {
        let mut interval = tokio::time::interval(every.max(Duration::from_millis(1)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval
    });

    loop {
        let tick = async {
            match ping_ticker.as_mut() {
                Some(interval) => {
                    interval.tick().await;
                }
                // No ping schedule: never wake for one.
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            () = tick => {
                event_id += 1;
                send(state, &mut tx, "ping", &json!({
                    "type": "ping",
                    "ping_event": { "event_id": event_id, "ping_ms": 0 }
                })).await;
            }
            said = say_rx.recv() => {
                let Ok(said) = said else { return Ending::Closed };
                if let Some(ending) = perform(state, &mut tx, &mut event_id, said).await {
                    return ending;
                }
            }
            incoming = rx.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(ending) =
                            on_client_event(state, &mut tx, &mut event_id, &text).await
                        {
                            return ending;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let detail = frame
                            .map(|f| format!("{} {}", u16::from(f.code), f.reason))
                            .unwrap_or_default();
                        state.trace().record(
                            Direction::Inbound, "client_close", &detail, 0,
                        );
                        return Ending::Closed;
                    }
                    // Control frames. Recorded under names that cannot be confused with the JSON
                    // events of the same word.
                    Some(Ok(Message::Ping(payload))) => state.trace().record(
                        Direction::Inbound, "ws_control_ping", "", payload.len(),
                    ),
                    Some(Ok(Message::Pong(payload))) => state.trace().record(
                        Direction::Inbound, "ws_control_pong", "", payload.len(),
                    ),
                    Some(Ok(Message::Binary(payload))) => state.trace().record(
                        Direction::Inbound,
                        "binary_frame",
                        "this protocol is JSON text; a binary frame is a client bug",
                        payload.len(),
                    ),
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(error)) => {
                        state.trace().record(
                            Direction::Inbound, "socket_error", &error.to_string(), 0,
                        );
                        return Ending::Closed;
                    }
                    None => return Ending::Closed,
                }
            }
        }
    }
}

/// Read frames until the first application event. `Some(true)` when it was the initiation,
/// `Some(false)` when it was anything else, `None` when the peer left first.
async fn wait_for_initiation(state: &Arc<MockState>, rx: &mut Source) -> Option<bool> {
    loop {
        match rx.next().await? {
            Ok(Message::Text(text)) => {
                let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let kind = parsed.get("type").and_then(Value::as_str).unwrap_or("");
                let is_initiation = kind == "conversation_initiation_client_data";
                state.trace().record(
                    Direction::Inbound,
                    if is_initiation {
                        "conversation_initiation_client_data"
                    } else {
                        "premature_event"
                    },
                    kind,
                    text.len(),
                );
                return Some(is_initiation);
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

async fn on_client_event(
    state: &Arc<MockState>,
    tx: &mut Sink,
    event_id: &mut u64,
    text: &str,
) -> Option<Ending> {
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);

    if let Some(chunk) = parsed.get("user_audio_chunk").and_then(Value::as_str) {
        match audio::decode(chunk) {
            Ok(bytes) => state.trace().record(
                Direction::Inbound,
                "user_audio_chunk",
                &format!("{} samples", audio::sample_count(&bytes)),
                bytes.len(),
            ),
            Err(error) => state.trace().record(
                Direction::Inbound,
                error.kind(),
                &error.to_string(),
                chunk.len(),
            ),
        }
        return None;
    }

    match parsed.get("type").and_then(Value::as_str).unwrap_or("") {
        "pong" => {
            let echoed = parsed.get("event_id").cloned().unwrap_or(Value::Null);
            state
                .trace()
                .record(Direction::Inbound, "pong", &echoed.to_string(), 0);
            None
        }
        "user_message" => {
            let said = parsed
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            state
                .trace()
                .record(Direction::Inbound, "user_message", &said, said.len());
            perform(
                state,
                tx,
                event_id,
                Say {
                    who: Speaker::User,
                    text: said,
                },
            )
            .await
        }
        "contextual_update" => on_contextual_update(state, &parsed, text.len()),
        "conversation_initiation_client_data" => {
            state.trace().record(
                Direction::Inbound,
                "duplicate_initiation",
                "the conversation was already initiated",
                0,
            );
            None
        }
        other => {
            // An event this mock does not model, recorded under a kind of its own so that
            // "accepted and ignored" can never be mistaken for "understood". Every modelled event
            // above records under its own name; a test that asserts on `client_event` is asserting
            // that the mock did NOT understand the frame.
            state
                .trace()
                .record(Direction::Inbound, "client_event", other, text.len());
            None
        }
    }
}

/// Our model of the vendor's `contextual_update`: context in, no turn out.
///
/// # UNVERIFIED AGAINST THE LIVE VENDOR
///
/// Nothing below has ever been observed against ElevenLabs. `contextual_update` is *believed* to
/// be a client event that injects text into the agent's context without consuming a turn, and that
/// belief came from a recon plan rather than from the vendor's protocol reference. One billed
/// `scripts/run.sh --smoke-agent` conversation would settle it, and that run has not been made.
///
/// It is modelled here anyway, and deliberately, because two shipped features already put this
/// frame on the wire — `#46 conversation-replay`'s default transport and `#73 mute-is-invisible`'s
/// announcement that a mute is deliberate — and a mock that let both fall into the catch-all above
/// would answer them exactly as it answers `totally_made_up_event`. A test written against that
/// mock proves only that unknown frames are ignored. This function is where this repository's
/// model of the contract lives, so that a test can state the model and a future observation of the
/// real vendor has one place to correct.
///
/// The model has three parts:
///
/// 1. **The frame carries a non-empty `text`.** One without is malformed rather than an update
///    that says nothing, and is recorded under its own kind. What the vendor does with such a
///    frame is unknown; what is *not* unknown is that the page must never send one.
/// 2. **An accepted update enters the conversation's context** — [`MockState::context`], which is
///    the half that silence cannot demonstrate.
/// 3. **It consumes no turn.** Nothing is spoken back, which is the reason the page prefers it to
///    `user_message` for both features.
fn on_contextual_update(state: &Arc<MockState>, parsed: &Value, size: usize) -> Option<Ending> {
    let text = parsed.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        state.trace().record(
            Direction::Inbound,
            "contextual_update_without_text",
            "a contextual update carried no text, so the agent was told nothing",
            size,
        );
        return None;
    }
    state.note_context(text);
    state
        .trace()
        .record(Direction::Inbound, "contextual_update", text, text.len());
    None
}

/// Act on one instruction, from `/_mock/say` or from a spoken `user_message`.
async fn perform(
    state: &Arc<MockState>,
    tx: &mut Sink,
    event_id: &mut u64,
    said: Say,
) -> Option<Ending> {
    match said.who {
        Speaker::Agent => {
            speak(state, tx, event_id, &said.text).await;
            None
        }
        Speaker::Correction => {
            send(
                state,
                tx,
                "agent_response_correction",
                &json!({
                    "type": "agent_response_correction",
                    "agent_response_correction_event": {
                        "original_agent_response": "",
                        "corrected_agent_response": said.text,
                    }
                }),
            )
            .await;
            None
        }
        Speaker::Interrupt => {
            *event_id += 1;
            send(
                state,
                tx,
                "interruption",
                &json!({
                    "type": "interruption",
                    "interruption_event": { "event_id": *event_id }
                }),
            )
            .await;
            None
        }
        Speaker::User => hold_turn(state, tx, event_id, &said.text).await,
    }
}

/// The whole turn: transcribe, decide, and answer — or fail in the way the scenario asks for.
async fn hold_turn(
    state: &Arc<MockState>,
    tx: &mut Sink,
    event_id: &mut u64,
    asked: &str,
) -> Option<Ending> {
    send(
        state,
        tx,
        "user_transcript",
        &json!({
            "type": "user_transcript",
            "user_transcription_event": { "user_transcript": asked }
        }),
    )
    .await;

    match state.scenario() {
        Scenario::NoReply => {
            // The socket stays open and nothing comes back. This is what a wedged agent looks
            // like from a phone, and it is why the page needs a way out that is not a timeout.
            state.trace().record(
                Direction::Internal,
                "no_reply",
                "the question was transcribed and deliberately not answered",
                0,
            );
            None
        }
        Scenario::SocketDrop => {
            state.trace().record(
                Direction::Internal,
                "socket_drop",
                "dropping the connection mid-turn, with no close frame",
                0,
            );
            Some(Ending::Dropped)
        }
        Scenario::NoToolCall => {
            // THE BUG, on purpose: a confident answer with no tool call behind it. Nothing is
            // posted to /mcp on this path, which is what the trace assertion turns into a test.
            state.trace().record(
                Direction::Internal,
                "no_tool_call",
                "answering without calling any tool",
                0,
            );
            speak(
                state,
                tx,
                event_id,
                "There is nothing new in that channel as far as I can tell.",
            )
            .await;
            None
        }
        Scenario::IgnoredToolResult => {
            let _ = read_the_channel(state).await;
            // The tools ran; the answer ignores them. This is the harder half of the same
            // failure, and the only way to catch it is to check the reply against what came back.
            speak(
                state,
                tx,
                event_id,
                "I had a look and there is nothing worth repeating right now.",
            )
            .await;
            None
        }
        Scenario::Full | Scenario::UnsupportedAudio | Scenario::MintRejected => {
            match read_the_channel(state).await {
                Ok(reply) => speak(state, tx, event_id, &reply).await,
                Err(detail) => {
                    state
                        .trace()
                        .record(Direction::Bridge, "bridge_failed", &detail, 0);
                    speak(
                        state,
                        tx,
                        event_id,
                        &format!("I could not reach the bridge: {detail}"),
                    )
                    .await;
                }
            }
            None
        }
    }
}

/// Emit an `agent_response` and the audio for it.
async fn speak(state: &Arc<MockState>, tx: &mut Sink, event_id: &mut u64, text: &str) {
    send(
        state,
        tx,
        "agent_response",
        &json!({
            "type": "agent_response",
            "agent_response_event": { "agent_response": text }
        }),
    )
    .await;
    for _ in 0..AUDIO_FRAMES {
        *event_id += 1;
        let pcm = audio::tone(SAMPLES_PER_FRAME);
        // The base64 is NOT put in the summary: audio is recorded as a length, the same rule
        // `crate::access` states for channel text.
        let frame = json!({
            "type": "audio",
            "audio_event": { "event_id": *event_id, "audio_base_64": audio::encode(&pcm) }
        });
        state
            .trace()
            .record(Direction::Outbound, "audio", "", pcm.len());
        let _ = tx.send(Message::Text(frame.to_string().into())).await;
    }
    let _ = tx.flush().await;
}

async fn send(state: &Arc<MockState>, tx: &mut Sink, kind: &str, event: &Value) {
    let text = event.to_string();
    let summary = match kind {
        "agent_response" => event["agent_response_event"]["agent_response"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        "user_transcript" => event["user_transcription_event"]["user_transcript"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        "conversation_initiation_metadata" => {
            event["conversation_initiation_metadata_event"].to_string()
        }
        "ping" => event["ping_event"]["event_id"].to_string(),
        _ => String::new(),
    };
    state
        .trace()
        .record(Direction::Outbound, kind, &summary, text.len());
    let _ = tx.send(Message::Text(text.into())).await;
    let _ = tx.flush().await;
}

// --- the MCP half ----------------------------------------------------------------------------

/// Run the real MCP handshake and two real read tools against the bridge, and build the sentence
/// that will be spoken out of what came back.
///
/// # Errors
///
/// Any step that does not answer the way the protocol says it will, described in words. The
/// caller says it out loud rather than pretending the channel was empty, because "I could not
/// reach the bridge" and "the channel is quiet" are the two answers that must never be confused.
async fn read_the_channel(state: &Arc<MockState>) -> Result<String, String> {
    let endpoint = format!("{}/mcp", state.options().bridge_base.trim_end_matches('/'));

    let hello = rpc(
        state,
        &endpoint,
        "initialize",
        json!({
            "protocolVersion": crate::mcp::protocol::PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "vibe-talk-mock-elevenlabs", "version": "1" }
        }),
    )
    .await?;
    let echoed = hello["protocolVersion"].as_str().unwrap_or_default();
    if echoed != crate::mcp::protocol::PROTOCOL_VERSION {
        return Err(format!(
            "the bridge settled on protocol {echoed}, not {}",
            crate::mcp::protocol::PROTOCOL_VERSION
        ));
    }

    notify(state, &endpoint).await?;
    let _tools = rpc(state, &endpoint, "tools/list", json!({})).await?;

    let listed = call_tool(state, &endpoint, "list_channels", json!({})).await?;
    let channel = first_channel_id(&listed)
        .ok_or_else(|| "the bridge listed no channel this agent could read".to_owned())?;
    let digest = call_tool(
        state,
        &endpoint,
        "digest_channel",
        json!({ "channel_id": channel }),
    )
    .await?;

    let newest =
        newest_summary(&digest).unwrap_or_else(|| "nothing has been said in there yet".to_owned());
    Ok(format!(
        "In channel {channel}, the most recent thing said is: {newest}"
    ))
}

async fn rpc(
    state: &Arc<MockState>,
    endpoint: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let id = state.next_rpc_id();
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let response = state
        .bridge()
        .post(endpoint)
        .bearer_auth(state.options().bridge_token.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| state.trace().redacted(&e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| state.trace().redacted(&e.to_string()))?;
    if !status.is_success() {
        return Err(format!("{method} answered HTTP {}", status.as_u16()));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{method} answered non-JSON: {e}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("{method} answered a JSON-RPC error: {error}"));
    }
    state.trace().record(
        Direction::Bridge,
        method,
        &format!("HTTP {}", status.as_u16()),
        text.len(),
    );
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// `notifications/initialized` has no id and no response body; the spec asks for a bare 202.
async fn notify(state: &Arc<MockState>, endpoint: &str) -> Result<(), String> {
    let body = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let response = state
        .bridge()
        .post(endpoint)
        .bearer_auth(state.options().bridge_token.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| state.trace().redacted(&e.to_string()))?;
    let status = response.status();
    if status != reqwest::StatusCode::ACCEPTED {
        return Err(format!(
            "notifications/initialized answered HTTP {}, not 202",
            status.as_u16()
        ));
    }
    state.trace().record(
        Direction::Bridge,
        "notifications/initialized",
        "HTTP 202",
        0,
    );
    Ok(())
}

async fn call_tool(
    state: &Arc<MockState>,
    endpoint: &str,
    tool: &str,
    arguments: Value,
) -> Result<String, String> {
    let id = state.next_rpc_id();
    let body = json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
    let response = state
        .bridge()
        .post(endpoint)
        .bearer_auth(state.options().bridge_token.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| state.trace().redacted(&e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| state.trace().redacted(&e.to_string()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{tool} answered non-JSON: {e}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("{tool} was refused: {error}"));
    }
    let content = value["result"]["content"][0]["text"]
        .as_str()
        .ok_or_else(|| format!("{tool} returned no text content"))?
        .to_owned();
    state.trace().record(
        Direction::Bridge,
        &format!("tools/call:{tool}"),
        &format!(
            "HTTP {} · {} chars",
            status.as_u16(),
            content.chars().count()
        ),
        text.len(),
    );
    Ok(content)
}

/// Pull the first channel id out of `list_channels`' prose, which reads `- label (id 123) — …`.
fn first_channel_id(listing: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let (_, rest) = line.split_once("(id ")?;
        let (id, _) = rest.split_once(')')?;
        Some(id.trim().to_owned())
    })
}

/// The summary of the newest digest entry: digest lines read `[id | time | author <@id>] summary`
/// and are ordered oldest first.
fn newest_summary(digest: &str) -> Option<String> {
    digest
        .lines()
        .rev()
        .filter(|line| line.starts_with('['))
        .find_map(|line| line.split_once("] ").map(|(_, s)| s.trim().to_owned()))
}

impl MockState {
    /// The next conversation id. A counter, because a random one would make two screenshot runs
    /// differ in the connection details the settings screen prints.
    fn next_conversation_id(&self) -> String {
        let n = self.conversations.fetch_add(1, Ordering::SeqCst) + 1;
        format!("conv_mock_{n:04}")
    }

    fn next_rpc_id(&self) -> u64 {
        self.rpc_ids.fetch_add(1, Ordering::SeqCst) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_channel_is_read_out_of_the_listing_prose() {
        let listing = "Channels this server can reach:\n\
                       - build noise (id 1111111111) — read-only\n\
                       - lead team (id 2222222222) — readable and postable\n";
        assert_eq!(first_channel_id(listing).as_deref(), Some("1111111111"));
    }

    #[test]
    fn a_listing_with_no_channel_yields_nothing_rather_than_a_guess() {
        assert_eq!(
            first_channel_id("No channels are configured on this server."),
            None
        );
        assert_eq!(first_channel_id(""), None);
    }

    #[test]
    fn the_newest_digest_line_is_the_last_one_because_digests_are_oldest_first() {
        let digest = "Digest of build noise (id 1): the whole channel, 2 messages, oldest first.\n\
                      <<<BEGIN>>>\n\
                      [10 | 09:00 | ana <@1>] the older thing\n\
                      [11 | 09:01 | bo <@2>] the mac runner finally went green\n\
                      <<<END>>>\n";
        assert_eq!(
            newest_summary(digest).as_deref(),
            Some("the mac runner finally went green")
        );
    }

    #[test]
    fn an_empty_digest_has_no_newest_line() {
        assert_eq!(newest_summary("Digest of x (id 1): no messages.\n"), None);
    }

    #[test]
    fn a_query_string_is_read_without_a_url_crate() {
        let query = "agent_id=agent_1&token=mock-nonce-0001";
        assert_eq!(query_param(query, "agent_id").as_deref(), Some("agent_1"));
        assert_eq!(
            query_param(query, "token").as_deref(),
            Some("mock-nonce-0001")
        );
        assert_eq!(query_param(query, "missing"), None);
        // A prefix must not match: `tokenish=` is not `token=`.
        assert_eq!(query_param("tokenish=no", "token"), None);
    }
}
