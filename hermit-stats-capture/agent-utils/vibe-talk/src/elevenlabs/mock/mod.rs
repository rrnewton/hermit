//! A loopback ElevenLabs substitute: a real mint endpoint, a real WebSocket, a real MCP client.
//!
//! # What problem this solves
//!
//! Before this module the test doubles were three unrelated things. The Rust fake
//! ([`crate::elevenlabs::fake`]) minted a URL that pointed nowhere. The screenshot harness
//! intercepted the mint request in the browser and replaced `window.WebSocket` with a class that
//! answered instantly. The smoke script stubbed the conversation loop. Nothing anywhere drove the
//! actual chain — browser to signed URL to a network socket to an MCP `tools/call` to Discord and
//! back — so the failure that mattered most in production, *an agent that answers without calling
//! any tool*, could not be reproduced offline at all.
//!
//! This is that chain, on 127.0.0.1, deterministic, and free.
//!
//! # Why it ships in the library rather than under `tests/`
//!
//! Three reasons, in order of force:
//!
//! 1. The mint side must be driven by the REAL [`crate::elevenlabs::http::HttpElevenLabsClient`].
//!    Pointing `elevenlabs.api_base` at this mock makes `signed_url_request`, `API_KEY_HEADER`,
//!    `parse_signed_url` and the whole [`crate::elevenlabs::SignedUrlError`] taxonomy run for
//!    real against something that then accepts a socket.
//! 2. `cargo test --locked` is the offline gate CI already runs, and an in-process spawn needs no
//!    subprocess and no fixed port.
//! 3. `src/bin/mock_elevenlabs.rs` needs it, and a binary cannot use `[dev-dependencies]`.
//!
//! # It must never be reachable off this machine
//!
//! It hands out conversations for an agent it does not own, on a key it invented.
//! [`MockElevenLabs::spawn`] therefore refuses any bind address that is not loopback, by name,
//! and every control-plane route lives under a `/_mock/` prefix that the real vendor does not
//! have — so a client that accidentally points at production fails loudly instead of half
//! working.
//!
//! # Determinism is the product
//!
//! Ids are counters, audio is a fixed waveform, and event order is fixed. Anything random here
//! would destroy both the screenshot comparability and the trace assertions, so the mock
//! deliberately does NOT grow toward being a usable local voice agent.
//!
//! # What of the client's half it models, and what it merely tolerates
//!
//! [`agent::on_client_event`](agent) understands `conversation_initiation_client_data`, `pong`,
//! `user_audio_chunk`, `user_message` and `contextual_update`, and each records in the trace under
//! its own name. Anything else is recorded as `client_event` — accepted, traced and answered with
//! silence, but not understood.
//!
//! That line matters more than it looks, and it is written here because it was learned the hard
//! way. `contextual_update` had no arm at all, so `#46 conversation-replay`'s preamble and `#73
//! mute-is-invisible`'s announcement both fell into the catch-all, and a test asserting "the
//! vendor accepts this frame" passed unchanged when the frame was renamed to something invented.
//! **A test that asserts on `client_event` is asserting that the mock did NOT model the frame**;
//! anything stronger has to name the kind, and the kinds are only there for events that are
//! modelled.
//!
//! # RFC6455 is a dependency, not a hand-roll
//!
//! The socket is [`tokio_tungstenite`]. An earlier plan for this module hand-rolled SHA-1, base64
//! and the frame codec, on the belief that crates.io was unreachable from the development host;
//! it is not. Five hundred lines of handshake and framing is the single riskiest thing this
//! module could contain and it buys nothing, so it is a crate — and the interoperability is
//! checked against a real Python `websockets` client, not only against our own reader.

pub mod agent;
pub mod audio;
pub mod client;
pub mod trace;

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use self::trace::{Direction, MintRequest, Trace};
use crate::elevenlabs::http::API_KEY_HEADER;

/// The default agent id the mock account holds. Shaped like a real one; owned by nobody.
pub const MOCK_AGENT_ID: &str = "agent_mock00000000000000000000000";
/// The default API key the mock account accepts. Anything else is a 401.
pub const MOCK_API_KEY: &str = "xi-mock-api-key-not-a-real-one";
/// The audio format the mock negotiates unless a scenario says otherwise.
pub const PCM_FORMAT: &str = "pcm_16000";
/// The audio format the `unsupported_audio` scenario negotiates.
pub const UNSUPPORTED_FORMAT: &str = "ulaw_8000";

/// Why the mock would not start.
#[derive(Debug, thiserror::Error)]
pub enum MockError {
    /// The requested bind address is not loopback.
    #[error(
        "the ElevenLabs mock refuses to bind {0}: it mints conversations for an agent nobody \
         owns, so it must never be reachable off this machine. Bind 127.0.0.1 or ::1."
    )]
    NotLoopback(String),
    /// A listener could not be opened.
    #[error("could not bind {address}: {detail}")]
    Bind {
        /// What was attempted.
        address: String,
        /// The operating system's complaint.
        detail: String,
    },
}

/// What the mock does when it is asked to hold a turn.
///
/// Every variant is one of the conditions the issue named, and each exists because it is
/// something that has actually gone wrong or is about to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scenario {
    /// Transcribe, call the real MCP tools, and answer from what they returned.
    #[default]
    Full,
    /// Answer without calling anything. This is the 2026-08-19 production failure, offline.
    NoToolCall,
    /// Call the tools, then answer with prose containing none of what they returned.
    IgnoredToolResult,
    /// The HTTP mint answers 401, so no socket is ever opened.
    MintRejected,
    /// Send the metadata, then drop the TCP connection mid-turn with no close frame.
    SocketDrop,
    /// Negotiate an audio format the page cannot decode.
    UnsupportedAudio,
    /// Transcribe the question and then say nothing at all.
    NoReply,
}

impl Scenario {
    /// The name used on the wire and on the command line.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::NoToolCall => "no_tool_call",
            Self::IgnoredToolResult => "ignored_tool_result",
            Self::MintRejected => "mint_rejected",
            Self::SocketDrop => "socket_drop",
            Self::UnsupportedAudio => "unsupported_audio",
            Self::NoReply => "no_reply",
        }
    }

    /// Every scenario, in the order the issue lists them.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Full,
            Self::NoToolCall,
            Self::IgnoredToolResult,
            Self::MintRejected,
            Self::SocketDrop,
            Self::UnsupportedAudio,
            Self::NoReply,
        ]
    }

    /// Parse a name, or `None` for anything else. Unknown names are refused rather than defaulted
    /// to `full`: silently running the happy path when a harness asked for a failure is exactly
    /// how a green suite comes to mean nothing.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::all().iter().copied().find(|s| s.name() == name)
    }

    /// The audio format the metadata announces under this scenario.
    #[must_use]
    pub fn audio_format(self) -> &'static str {
        if self == Self::UnsupportedAudio {
            UNSUPPORTED_FORMAT
        } else {
            PCM_FORMAT
        }
    }
}

/// Who a `/_mock/say` command speaks as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    /// Emit `user_transcript` and then run a whole turn, exactly as speech would.
    User,
    /// Emit `agent_response` verbatim, with no tool calls. The harness path: byte-identical text
    /// is what keeps two screenshot runs comparable.
    Agent,
    /// Emit `agent_response_correction`, which the page renders over the last agent line.
    Correction,
    /// Emit `interruption`, which stops playback.
    Interrupt,
}

impl Speaker {
    /// The name used on the wire.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Correction => "correction",
            Self::Interrupt => "interrupt",
        }
    }

    /// Parse a name, or `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::User, Self::Agent, Self::Correction, Self::Interrupt]
            .into_iter()
            .find(|s| s.name() == name)
    }
}

/// One thing for the open conversation to say.
#[derive(Clone, Debug)]
pub struct Say {
    /// Which voice.
    pub who: Speaker,
    /// What to say, used verbatim.
    pub text: String,
}

/// How to start the mock.
#[derive(Clone, Debug)]
pub struct MockOptions {
    /// Loopback address to bind. Anything else is [`MockError::NotLoopback`].
    pub bind: IpAddr,
    /// Port for the HTTP half; 0 asks the kernel.
    pub http_port: u16,
    /// Port for the WebSocket half; 0 asks the kernel.
    pub ws_port: u16,
    /// The agent id this mock account holds.
    pub agent_id: String,
    /// The API key this mock account accepts.
    pub api_key: String,
    /// Base URL of the vibe-talk bridge the agent brain calls, e.g. `http://127.0.0.1:18091`.
    pub bridge_base: String,
    /// Write-scope bearer token for that bridge.
    pub bridge_token: String,
    /// Starting scenario.
    pub scenario: Scenario,
    /// How long a minted nonce stays usable.
    pub validity: Duration,
    /// How often to emit the JSON `ping` event, if at all. One is always sent immediately after
    /// the metadata when this is set, so a test does not have to wait on a timer to observe one.
    pub ping_every: Option<Duration>,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            bind: IpAddr::from([127, 0, 0, 1]),
            http_port: 0,
            ws_port: 0,
            agent_id: MOCK_AGENT_ID.to_owned(),
            api_key: MOCK_API_KEY.to_owned(),
            bridge_base: String::new(),
            bridge_token: String::new(),
            scenario: Scenario::Full,
            validity: Duration::from_secs(u64::from(
                crate::elevenlabs::DOCUMENTED_VALIDITY_SECONDS,
            )),
            ping_every: None,
        }
    }
}

#[derive(Debug)]
struct Nonce {
    issued: Instant,
    used: bool,
}

/// Everything the two listeners share.
#[derive(Debug)]
pub struct MockState {
    options: MockOptions,
    ws_addr: SocketAddr,
    trace: Trace,
    scenario: Mutex<Scenario>,
    nonces: Mutex<BTreeMap<String, Nonce>>,
    context: Mutex<Vec<String>>,
    minted: AtomicU64,
    conversations: AtomicU64,
    rpc_ids: AtomicU64,
    say: broadcast::Sender<Say>,
    bridge: reqwest::Client,
}

impl MockState {
    /// The options this mock was started with.
    #[must_use]
    pub fn options(&self) -> &MockOptions {
        &self.options
    }

    /// The trace.
    #[must_use]
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// The scenario in force right now.
    #[must_use]
    pub fn scenario(&self) -> Scenario {
        *self
            .scenario
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Change the scenario. Takes effect on the next mint or the next turn.
    pub fn set_scenario(&self, scenario: Scenario) {
        *self
            .scenario
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = scenario;
        self.trace
            .record(Direction::Internal, "scenario", scenario.name(), 0);
    }

    /// The HTTP client the agent brain calls the bridge with.
    #[must_use]
    pub fn bridge(&self) -> &reqwest::Client {
        &self.bridge
    }

    /// Everything a `contextual_update` has put into this conversation's context, in order.
    ///
    /// This exists because the trace cannot carry the claim on its own. The vendor's contract for
    /// `contextual_update`, as this repository models it, has two halves — the text enters the
    /// agent's context, and no turn is spent answering it — and only the second is visible as an
    /// absence of frames. An event that was dropped on the floor is *also* answered with silence,
    /// so a test that asserted only silence would pass against a mock that had never heard of the
    /// event. This is the first half, made assertable.
    ///
    /// See [`super::mock::agent`]'s `on_contextual_update` for what is and is not known about the
    /// real vendor here: none of it has been observed against ElevenLabs.
    #[must_use]
    pub fn context(&self) -> Vec<String> {
        self.context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Remember one accepted contextual update.
    ///
    /// Redacted on the way in, for the reason [`Trace::record`] is: this is read by tests and
    /// written into diagnostics, and a secret that reached it would be just as leaked.
    pub(super) fn note_context(&self, text: &str) {
        let text = self.trace.redacted(text);
        self.context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(text);
    }

    /// Forget the accepted contextual updates, as part of `/_mock/reset`.
    fn forget_context(&self) {
        self.context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Mint a single-use nonce and record it as a secret so it cannot reach the trace.
    fn mint_nonce(&self) -> String {
        let n = self.minted.fetch_add(1, Ordering::SeqCst) + 1;
        let nonce = format!("mock-nonce-{n:04}");
        self.trace.keep_secret(&nonce);
        self.nonces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                nonce.clone(),
                Nonce {
                    issued: Instant::now(),
                    used: false,
                },
            );
        nonce
    }

    /// Spend a nonce, or say precisely why it cannot be spent.
    ///
    /// The three refusals are separate strings on purpose: "you never had one", "you already used
    /// it" and "you took too long" have three different fixes, and a browser only ever sees an
    /// immediate close, so the reason has to be recoverable from the trace.
    fn spend_nonce(&self, token: &str) -> Result<(), &'static str> {
        let mut guard = self
            .nonces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = guard.get_mut(token) else {
            return Err("token_unknown");
        };
        if entry.used {
            return Err("token_reused");
        }
        if entry.issued.elapsed() > self.options.validity {
            return Err("token_expired");
        }
        entry.used = true;
        Ok(())
    }

    /// Broadcast something for the open conversation to say. Returns how many sockets heard it.
    pub fn say(&self, say: Say) -> usize {
        self.say.send(say).unwrap_or(0)
    }

    fn subscribe(&self) -> broadcast::Receiver<Say> {
        self.say.subscribe()
    }
}

/// A running mock: two loopback listeners and everything they share.
#[derive(Debug)]
pub struct MockHandle {
    state: Arc<MockState>,
    http_addr: SocketAddr,
    ws_addr: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl MockHandle {
    /// The value to put in `elevenlabs.api_base`, e.g. `http://127.0.0.1:41234/v1`.
    #[must_use]
    pub fn api_base(&self) -> String {
        format!("http://{}/v1", self.http_addr)
    }

    /// The origin, without the `/v1`, for the `/_mock/` control plane.
    #[must_use]
    pub fn control_base(&self) -> String {
        format!("http://{}", self.http_addr)
    }

    /// Where the WebSocket half is listening.
    #[must_use]
    pub fn ws_addr(&self) -> SocketAddr {
        self.ws_addr
    }

    /// The shared state, for a caller that wants to drive the mock in-process.
    #[must_use]
    pub fn state(&self) -> &Arc<MockState> {
        &self.state
    }

    /// The trace.
    #[must_use]
    pub fn trace(&self) -> &Trace {
        self.state.trace()
    }

    /// What `contextual_update` frames have told the agent, in order.
    #[must_use]
    pub fn context(&self) -> Vec<String> {
        self.state.context()
    }

    /// Set the scenario in force.
    pub fn set_scenario(&self, scenario: Scenario) {
        self.state.set_scenario(scenario);
    }

    /// Make the open conversation say something.
    pub fn say(&self, who: Speaker, text: &str) -> usize {
        self.state.say(Say {
            who,
            text: text.to_owned(),
        })
    }

    /// Stop both listeners.
    pub fn shutdown(self) {
        drop(self);
    }
}

impl Drop for MockHandle {
    fn drop(&mut self) {
        // Abort rather than await: a test that finished should not block on a socket that is
        // waiting for a peer which will never arrive.
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// The mock itself.
#[derive(Debug)]
pub struct MockElevenLabs;

impl MockElevenLabs {
    /// Bind both listeners and start serving.
    ///
    /// # Errors
    ///
    /// [`MockError::NotLoopback`] when `options.bind` is anything but a loopback address, and
    /// [`MockError::Bind`] when a listener cannot be opened.
    pub async fn spawn(options: MockOptions) -> Result<MockHandle, MockError> {
        if !options.bind.is_loopback() {
            return Err(MockError::NotLoopback(options.bind.to_string()));
        }

        let ws_listener = tokio::net::TcpListener::bind((options.bind, options.ws_port))
            .await
            .map_err(|e| MockError::Bind {
                address: format!("{}:{}", options.bind, options.ws_port),
                detail: e.to_string(),
            })?;
        let ws_addr = ws_listener.local_addr().map_err(|e| MockError::Bind {
            address: "the websocket listener".to_owned(),
            detail: e.to_string(),
        })?;
        let http_listener = tokio::net::TcpListener::bind((options.bind, options.http_port))
            .await
            .map_err(|e| MockError::Bind {
                address: format!("{}:{}", options.bind, options.http_port),
                detail: e.to_string(),
            })?;
        let http_addr = http_listener.local_addr().map_err(|e| MockError::Bind {
            address: "the http listener".to_owned(),
            detail: e.to_string(),
        })?;

        let trace = Trace::new();
        trace.keep_secret(&options.api_key);
        trace.keep_secret(&options.bridge_token);
        let (say, _) = broadcast::channel(64);
        let state = Arc::new(MockState {
            scenario: Mutex::new(options.scenario),
            ws_addr,
            trace,
            nonces: Mutex::new(BTreeMap::new()),
            context: Mutex::new(Vec::new()),
            minted: AtomicU64::new(0),
            conversations: AtomicU64::new(0),
            rpc_ids: AtomicU64::new(0),
            say,
            bridge: reqwest::Client::new(),
            options,
        });

        let router = axum::Router::new()
            .route(
                "/v1/convai/conversation/get-signed-url",
                get(get_signed_url),
            )
            .route("/_mock/scenario", post(set_scenario))
            .route("/_mock/say", post(say_something))
            .route("/_mock/reset", post(reset))
            .route("/_mock/trace", get(read_trace))
            .fallback(not_found)
            .with_state(Arc::clone(&state));

        let http_task = tokio::spawn(async move {
            let _ = axum::serve(http_listener, router).await;
        });
        let ws_state = Arc::clone(&state);
        let ws_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = ws_listener.accept().await else {
                    return;
                };
                let per_connection = Arc::clone(&ws_state);
                tokio::spawn(async move {
                    agent::serve_connection(per_connection, stream).await;
                });
            }
        });

        Ok(MockHandle {
            state,
            http_addr,
            ws_addr,
            tasks: vec![http_task, ws_task],
        })
    }
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// `GET /v1/convai/conversation/get-signed-url`, the endpoint the real client calls.
async fn get_signed_url(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    let presented = headers
        .get(API_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let agent_id = params.get("agent_id").cloned().unwrap_or_default();
    let url = format!("/v1/convai/conversation/get-signed-url?agent_id={agent_id}");
    let accepted = presented == state.options.api_key;

    let finish = |status: StatusCode, body: Value| {
        state.trace.record_mint(MintRequest {
            url: url.clone(),
            api_key_header: !presented.is_empty(),
            api_key_accepted: accepted,
            status: status.as_u16(),
        });
        // A rejection carries its body into the trace, because "the vendor said no" without
        // saying what it said is the least useful line a diagnostic file can hold. That body
        // deliberately quotes the presented key back — real upstreams do — so this is also the
        // line that makes `Trace::keep_secret(api_key)` load-bearing rather than decorative.
        let detail = if status.is_success() {
            format!("agent_id={agent_id} -> {}", status.as_u16())
        } else {
            format!("agent_id={agent_id} -> {} {body}", status.as_u16())
        };
        state.trace.record(Direction::Inbound, "mint", &detail, 0);
        json_response(status, body)
    };

    if state.scenario() == Scenario::MintRejected {
        // The vendor's own shape for a rejection, and deliberately one that QUOTES the key back:
        // upstreams do that, and the redaction in `SignedUrlError::from_response` is what stops it
        // reaching a log. Driving the real client through this is the only way to prove it.
        return finish(
            StatusCode::UNAUTHORIZED,
            json!({"detail": {
                "status": "invalid_api_key",
                "message": format!("the key {presented} is not valid"),
            }}),
        );
    }
    if !accepted {
        return finish(
            StatusCode::UNAUTHORIZED,
            json!({"detail": {
                "status": "invalid_api_key",
                "message": format!("the key {presented} is not valid"),
            }}),
        );
    }
    if agent_id != state.options.agent_id {
        return finish(
            StatusCode::NOT_FOUND,
            json!({"detail": {"status": "agent_not_found", "message": agent_id}}),
        );
    }

    let nonce = state.mint_nonce();
    let signed = format!(
        "ws://{}/v1/convai/conversation?agent_id={agent_id}&token={nonce}",
        state.ws_addr
    );
    // Record WHAT was minted, not merely that something was. Diagnosing "the page connected to
    // the wrong port" from a trace that only says `200` is guesswork. The nonce inside it is a
    // credential, so this line is also the thing that makes the nonce redaction load-bearing
    // rather than decorative — see `Trace::keep_secret` in `mint_nonce`.
    state
        .trace
        .record(Direction::Outbound, "minted", &signed, signed.len());
    finish(StatusCode::OK, json!({ "signed_url": signed }))
}

async fn set_scenario(State(state): State<Arc<MockState>>, Json(body): Json<Value>) -> Response {
    let name = body
        .get("scenario")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(scenario) = Scenario::from_name(name) else {
        let names: Vec<&str> = Scenario::all().iter().map(|s| s.name()).collect();
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": "unknown_scenario", "asked_for": name, "known": names}),
        );
    };
    state.set_scenario(scenario);
    json_response(StatusCode::OK, json!({"scenario": scenario.name()}))
}

async fn say_something(State(state): State<Arc<MockState>>, Json(body): Json<Value>) -> Response {
    let who = body.get("who").and_then(Value::as_str).unwrap_or("agent");
    let Some(speaker) = Speaker::from_name(who) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": "unknown_speaker", "asked_for": who,
                   "known": ["user", "agent", "correction", "interrupt"]}),
        );
    };
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let heard = state.say(Say { who: speaker, text });
    if heard == 0 {
        // Answering 200 here would let a harness "say" things into a closed socket and photograph
        // an empty screen, which is the failure that is hardest to read back from an image.
        return json_response(
            StatusCode::CONFLICT,
            json!({"error": "no_open_conversation",
                   "detail": "nothing is connected to the mock's websocket to say that"}),
        );
    }
    json_response(StatusCode::OK, json!({"delivered_to": heard}))
}

async fn reset(State(state): State<Arc<MockState>>) -> Response {
    state.trace.reset();
    state.forget_context();
    state.set_scenario(Scenario::Full);
    json_response(StatusCode::OK, json!({"reset": true}))
}

async fn read_trace(State(state): State<Arc<MockState>>) -> Response {
    // The accepted contextual updates ride along with the trace rather than living behind their
    // own route: they are part of the same "what did the mock see" question, and a caller reading
    // the trace over HTTP has no other way to see them.
    let mut payload = state.trace.to_json();
    if let Some(fields) = payload.as_object_mut() {
        fields.insert("context".to_owned(), json!(state.context()));
    }
    json_response(StatusCode::OK, payload)
}

async fn not_found(headers: HeaderMap, uri: axum::http::Uri) -> Response {
    let _ = headers;
    json_response(
        StatusCode::NOT_FOUND,
        json!({"error": "not_found",
               "detail": "this is the vibe-talk ElevenLabs mock, which implements one vendor \
                          endpoint and the /_mock/ control plane",
               "path": uri.path()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_the_issue_named_round_trips_through_its_name() {
        for scenario in Scenario::all() {
            assert_eq!(Scenario::from_name(scenario.name()), Some(*scenario));
        }
        assert_eq!(Scenario::all().len(), 7);
    }

    #[test]
    fn an_unknown_scenario_is_refused_rather_than_defaulted() {
        assert_eq!(Scenario::from_name("Full"), None);
        assert_eq!(Scenario::from_name(""), None);
        assert_eq!(Scenario::from_name("no-tool-call"), None);
    }

    #[test]
    fn only_the_unsupported_scenario_negotiates_something_the_page_cannot_play() {
        for scenario in Scenario::all() {
            let expected = if *scenario == Scenario::UnsupportedAudio {
                UNSUPPORTED_FORMAT
            } else {
                PCM_FORMAT
            };
            assert_eq!(scenario.audio_format(), expected, "{}", scenario.name());
        }
    }

    #[test]
    fn every_speaker_round_trips_through_its_name() {
        for who in [
            Speaker::User,
            Speaker::Agent,
            Speaker::Correction,
            Speaker::Interrupt,
        ] {
            assert_eq!(Speaker::from_name(who.name()), Some(who));
        }
        assert_eq!(Speaker::from_name("nobody"), None);
    }

    #[tokio::test]
    async fn binding_anywhere_but_loopback_is_refused_by_name() {
        // It mints conversations for an agent nobody owns. On 0.0.0.0 it would hand them to the
        // network.
        let error = MockElevenLabs::spawn(MockOptions {
            bind: IpAddr::from([0, 0, 0, 0]),
            ..MockOptions::default()
        })
        .await
        .expect_err("must refuse");
        assert!(
            matches!(&error, MockError::NotLoopback(a) if a == "0.0.0.0"),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains("127.0.0.1"), "{error}");

        for public in ["10.0.0.4", "::"] {
            assert!(
                MockElevenLabs::spawn(MockOptions {
                    bind: public.parse().expect("address"),
                    ..MockOptions::default()
                })
                .await
                .is_err(),
                "{public} was accepted"
            );
        }
    }

    #[tokio::test]
    async fn ipv6_loopback_is_accepted() {
        let mock = MockElevenLabs::spawn(MockOptions {
            bind: "::1".parse().expect("address"),
            ..MockOptions::default()
        })
        .await
        .expect("::1 is loopback");
        assert!(mock.api_base().contains("::1"), "{}", mock.api_base());
    }
}
