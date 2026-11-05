//! The offline gate: signed URL → real WebSocket → MCP `tools/call` → fake Discord → an answer.
//!
//! Every other test double in this repository stops at a boundary. The Rust fake mints a URL that
//! points nowhere; the screenshot harness replaced `window.WebSocket` inside the page; the smoke
//! script stubbed the conversation loop. This file is the one place where the whole chain runs,
//! on loopback, deterministically, for free — and where the failure that mattered in production,
//! *an agent that answers without calling any tool*, is a RED test rather than a story.
//!
//! Everything here is real except the two ends: Discord is [`vibe_talk::discord::fake`] and the
//! vendor is [`vibe_talk::elevenlabs::mock`]. In between, the production router, the production
//! MCP dispatcher, the production ops layer and the production ElevenLabs HTTP client all
//! execute.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use vibe_talk::config::{Config, ElevenLabsConfig, Secret};
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::elevenlabs::http::HttpElevenLabsClient;
use vibe_talk::elevenlabs::mock::client::{ConversationClient, Incoming};
use vibe_talk::elevenlabs::mock::{
    MockElevenLabs, MockHandle, MockOptions, Scenario, MOCK_AGENT_ID, MOCK_API_KEY,
    UNSUPPORTED_FORMAT,
};
use vibe_talk::elevenlabs::{SignedUrlProvider as _, DOCUMENTED_VALIDITY_SECONDS};
use vibe_talk::model::ChannelId;
use vibe_talk::state::AppState;
use vibe_talk::testing::{READ_CHANNEL, WRITE_TOKEN};

/// A line planted in the fake Discord that must survive all the way into what the agent says.
///
/// Short enough that the digest summariser does not truncate it, and distinctive enough that its
/// presence in an `agent_response` cannot be a coincidence.
const SENTINEL: &str = "the mac runner finally went green";

/// Everything a test needs: a real bridge, a real mock, and the seeded Discord behind them.
struct Harness {
    mock: MockHandle,
    discord: Arc<FakeDiscord>,
    bridge_base: String,
}

impl Harness {
    async fn start(scenario: Scenario) -> Self {
        Self::start_with(scenario, |options| options).await
    }

    async fn start_with(scenario: Scenario, tune: impl FnOnce(MockOptions) -> MockOptions) -> Self {
        let (state, discord) = vibe_talk::testing::state();
        discord.seed(&ChannelId(READ_CHANNEL.to_owned()), "ana", "an older note");
        discord.seed(&ChannelId(READ_CHANNEL.to_owned()), "bo", SENTINEL);
        let bridge_base = serve(vibe_talk::http::router(state)).await;

        let options = tune(MockOptions {
            bridge_base: bridge_base.clone(),
            bridge_token: WRITE_TOKEN.to_owned(),
            scenario,
            ..MockOptions::default()
        });
        let mock = MockElevenLabs::spawn(options)
            .await
            .expect("the mock starts");
        Self {
            mock,
            discord,
            bridge_base,
        }
    }

    /// Mint through the REAL ElevenLabs client, pointed at the mock's HTTP half.
    async fn mint(&self) -> Result<String, vibe_talk::elevenlabs::SignedUrlError> {
        let client = HttpElevenLabsClient::new().expect("client");
        let config = ElevenLabsConfig {
            agent_id: Some(MOCK_AGENT_ID.to_owned()),
            api_key: Some(Secret::new(MOCK_API_KEY)),
            api_base: self.mock.api_base(),
            voice_id: None,
        };
        client.signed_url(&config).await.map(|url| url.signed_url)
    }

    /// Mint, connect, and send the initiation the vendor requires.
    async fn open(&self) -> ConversationClient {
        let url = self.mint().await.expect("mints");
        let mut socket = ConversationClient::connect(&url).await.expect("connects");
        socket.initiate().await.expect("initiates");
        socket
    }
}

/// Serve a router on a real loopback socket and return its base URL.
///
/// A real socket, not `tower::oneshot`: the mock's agent brain is a `reqwest` client, so there
/// has to be something to connect to.
async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{address}")
}

/// Wait for one event of a given type, answering pings on the way.
async fn wait_for(socket: &mut ConversationClient, kind: &str) -> Vec<Incoming> {
    socket
        .collect_until(kind, Duration::from_secs(10))
        .await
        .unwrap_or_else(|e| panic!("waiting for {kind}: {e}"))
}

fn agent_said(events: &[Incoming]) -> Option<String> {
    events.iter().find_map(|event| match event {
        Incoming::Event { kind, value } if kind == "agent_response" => Some(
            value["agent_response_event"]["agent_response"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ),
        _ => None,
    })
}

// --- the mint --------------------------------------------------------------------------------

#[tokio::test]
async fn the_real_client_mints_a_url_this_process_can_actually_open() {
    // The fake mints a URL that points nowhere, so nothing until now has proved that what comes
    // out of `parse_signed_url` is dialable.
    let harness = Harness::start(Scenario::Full).await;
    let client = HttpElevenLabsClient::new().expect("client");
    let minted = client
        .signed_url(&ElevenLabsConfig {
            agent_id: Some(MOCK_AGENT_ID.to_owned()),
            api_key: Some(Secret::new(MOCK_API_KEY)),
            api_base: harness.mock.api_base(),
            voice_id: None,
        })
        .await
        .expect("mints");

    assert!(
        minted.signed_url.starts_with("ws://127.0.0.1:"),
        "{}",
        minted.signed_url
    );
    assert_eq!(minted.agent_id, MOCK_AGENT_ID);
    assert_eq!(minted.valid_for_seconds, DOCUMENTED_VALIDITY_SECONDS);

    let mints = harness.mock.trace().mint_requests();
    assert_eq!(mints.len(), 1, "exactly one call reached the vendor");
    assert!(mints[0].api_key_header, "the key travelled in a header");
    assert!(mints[0].api_key_accepted);
    assert!(
        !mints[0].url.contains(MOCK_API_KEY),
        "the account key reached the URL: {}",
        mints[0].url
    );

    ConversationClient::connect(&minted.signed_url)
        .await
        .expect("the minted URL opens a real socket");
}

#[tokio::test]
async fn a_wrong_key_reaches_the_page_as_a_bad_gateway_that_does_not_quote_it() {
    // The same shape as `signed_url.rs::an_elevenlabs_401_is_surfaced_honestly`, but against a
    // vendor that really answers over HTTP, so `SignedUrlError::from_response` runs for real on a
    // body the vendor really wrote — and that body deliberately quotes the key back.
    let harness = Harness::start(Scenario::Full).await;
    let toml = vibe_talk::testing::config_toml()
        .replace(vibe_talk::elevenlabs::fake::KNOWN_AGENT_ID, MOCK_AGENT_ID)
        .replace(
            vibe_talk::elevenlabs::fake::VALID_API_KEY,
            "xi-a-revoked-key-value",
        )
        + &format!("api_base = \"{}\"\n", harness.mock.api_base());
    let config = Config::from_toml_and_env(&toml, &std::collections::BTreeMap::new())
        .expect("configuration parses");
    let (base_state, _discord) = vibe_talk::testing::state();
    let state = AppState {
        config: Arc::new(config),
        elevenlabs: Arc::new(HttpElevenLabsClient::new().expect("client")),
        ..base_state
    };
    let base = serve(vibe_talk::http::router(state)).await;

    let answer = reqwest::Client::new()
        .get(format!("{base}/api/v1/signed-url"))
        .bearer_auth(WRITE_TOKEN)
        .send()
        .await
        .expect("the route answers");
    let status = answer.status();
    let body = answer.text().await.expect("body");

    assert_eq!(status.as_u16(), 502, "{body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["error"], "elevenlabs_error");
    assert!(parsed["detail"]
        .as_str()
        .unwrap_or_default()
        .contains("401"));
    assert!(
        !body.contains("xi-a-revoked-key-value"),
        "the key leaked through a vendor error body: {body}"
    );
    assert!(
        !body.contains("ws://"),
        "no fallback URL was invented: {body}"
    );
}

#[tokio::test]
async fn the_mint_rejected_scenario_refuses_the_correct_key() {
    let harness = Harness::start(Scenario::MintRejected).await;
    let error = harness.mint().await.expect_err("the mint is rejected");
    assert!(error.to_string().contains("401"), "{error}");
    assert!(
        !error.to_string().contains(MOCK_API_KEY),
        "the mock quotes the key back on purpose; the client must redact it: {error}"
    );
}

#[tokio::test]
async fn a_rejection_that_quotes_the_key_back_reaches_the_trace_redacted() {
    // The mock's 401 body deliberately quotes the presented key, the way a real upstream does.
    // The body is recorded, because "the vendor said no" without saying what it said is the least
    // useful line a diagnostic file can hold — and it must be recorded with the key taken OUT.
    let harness = Harness::start(Scenario::MintRejected).await;
    let _ = harness.mint().await.expect_err("the mint is rejected");

    let mints = harness.mock.trace().of_kind("mint");
    assert_eq!(mints.len(), 1);
    assert!(mints[0].summary.contains("401"), "{}", mints[0].summary);
    assert!(
        mints[0].summary.contains("invalid_api_key"),
        "a rejection that does not say why is barely worth recording: {}",
        mints[0].summary
    );
    assert!(
        !mints[0].summary.contains(MOCK_API_KEY),
        "the vendor quoted the key back and the trace kept it: {}",
        mints[0].summary
    );
    assert!(
        mints[0].summary.contains(vibe_talk::elevenlabs::REDACTED),
        "{}",
        mints[0].summary
    );
}

#[tokio::test]
async fn an_unknown_agent_is_a_404_rather_than_a_conversation() {
    let harness = Harness::start(Scenario::Full).await;
    let client = HttpElevenLabsClient::new().expect("client");
    let error = client
        .signed_url(&ElevenLabsConfig {
            agent_id: Some("agent_that_does_not_exist".to_owned()),
            api_key: Some(Secret::new(MOCK_API_KEY)),
            api_base: harness.mock.api_base(),
            voice_id: None,
        })
        .await
        .expect_err("an unknown agent has no conversation");
    assert!(error.to_string().contains("404"), "{error}");
}

// --- the whole chain -------------------------------------------------------------------------

#[tokio::test]
async fn a_spoken_question_becomes_a_real_mcp_tool_call_and_an_answer_made_of_its_result() {
    // THE ONE THAT MATTERS. Nothing between the socket and Discord is stubbed.
    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;

    let metadata = wait_for(&mut socket, "conversation_initiation_metadata").await;
    match metadata.last().expect("an event") {
        Incoming::Event { value, .. } => {
            let meta = &value["conversation_initiation_metadata_event"];
            assert_eq!(meta["agent_output_audio_format"], "pcm_16000");
            assert!(
                meta["conversation_id"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("conv_mock_"),
                "the conversation id must be a counter, not a random: {meta}"
            );
        }
        other => panic!("expected metadata, got {other:?}"),
    }

    socket
        .ask("what is new in build noise?")
        .await
        .expect("asks");
    let events = wait_for(&mut socket, "agent_response").await;

    let said = agent_said(&events).expect("the agent answered");
    assert!(
        said.contains(SENTINEL),
        "the answer must be MADE OF what the tools returned, not merely follow them: {said}"
    );

    assert_eq!(
        harness.mock.trace().mcp_methods(),
        vec![
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call:list_channels",
            "tools/call:digest_channel",
        ],
        "the whole MCP handshake and both read tools must have run"
    );
    assert!(
        harness.discord.fetch_count() > 0,
        "the digest must have reached Discord rather than being answered from nothing"
    );

    // And the audio really came back, in frames a page could play.
    let audio_frames = events.iter().filter(|e| e.kind() == Some("audio")).count();
    assert!(audio_frames > 0 || harness.mock.trace().count("audio") > 0);
}

#[tokio::test]
async fn the_no_tool_call_scenario_reproduces_the_production_failure_offline() {
    // 2026-08-19: the agent answered the owner without invoking anything. Free, offline, RED.
    let harness = Harness::start(Scenario::NoToolCall).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;
    socket
        .ask("what is new in build noise?")
        .await
        .expect("asks");
    let events = wait_for(&mut socket, "agent_response").await;

    let said = agent_said(&events).expect("the agent answered — that is the whole problem");
    assert!(!said.is_empty(), "it was confident, just wrong");
    assert!(
        harness.mock.trace().mcp_methods().is_empty(),
        "nothing may have reached /mcp: {:?}",
        harness.mock.trace().mcp_methods()
    );
    assert_eq!(
        harness.discord.fetch_count(),
        0,
        "and nothing may have reached Discord"
    );
    assert!(
        !said.contains(SENTINEL),
        "an answer with no tool call cannot contain channel text: {said}"
    );
}

#[tokio::test]
async fn the_ignored_tool_result_scenario_calls_the_tools_and_then_ignores_them() {
    let harness = Harness::start(Scenario::IgnoredToolResult).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;
    socket.ask("what is new?").await.expect("asks");
    let events = wait_for(&mut socket, "agent_response").await;

    let said = agent_said(&events).expect("answered");
    assert!(
        harness
            .mock
            .trace()
            .mcp_methods()
            .contains(&"tools/call:digest_channel".to_owned()),
        "the tools really ran"
    );
    assert!(
        !said.contains(SENTINEL),
        "and the answer contains none of what they returned: {said}"
    );
}

#[tokio::test]
async fn the_no_reply_scenario_transcribes_and_then_says_nothing() {
    let harness = Harness::start(Scenario::NoReply).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;
    socket.ask("are you there?").await.expect("asks");

    let transcript = wait_for(&mut socket, "user_transcript").await;
    assert!(transcript
        .iter()
        .any(|e| e.kind() == Some("user_transcript")));

    // Nothing follows. The short deadline is the assertion.
    let error = socket
        .next_within(Duration::from_millis(300))
        .await
        .expect_err("nothing may arrive");
    assert!(error.to_string().contains("said nothing"), "{error}");
    assert_eq!(harness.mock.trace().count("agent_response"), 0);
}

#[tokio::test]
async fn the_socket_drop_scenario_ends_the_call_without_a_close_frame() {
    let harness = Harness::start(Scenario::SocketDrop).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;
    socket.ask("what is new?").await.expect("asks");

    let events = wait_for(&mut socket, "agent_response").await;
    let last = events.last().expect("something ended it");
    assert!(
        matches!(last, Incoming::Dropped)
            || matches!(last, Incoming::Closed { code, .. } if *code != 1000),
        "a dropped connection must not look like a polite goodbye: {last:?}"
    );
    assert_eq!(harness.mock.trace().count("agent_response"), 0);
}

#[tokio::test]
async fn the_unsupported_audio_scenario_negotiates_something_the_page_cannot_play() {
    let harness = Harness::start(Scenario::UnsupportedAudio).await;
    let mut socket = harness.open().await;
    let events = wait_for(&mut socket, "conversation_initiation_metadata").await;
    match events.last().expect("metadata") {
        Incoming::Event { value, .. } => assert_eq!(
            value["conversation_initiation_metadata_event"]["agent_output_audio_format"],
            UNSUPPORTED_FORMAT
        ),
        other => panic!("expected metadata, got {other:?}"),
    }
}

// --- what the client sends -------------------------------------------------------------------

#[tokio::test]
async fn uploaded_pcm_is_decoded_and_counted_and_a_broken_chunk_is_named() {
    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;

    for _ in 0..3 {
        socket
            .send_audio(&vibe_talk::elevenlabs::mock::audio::tone(160))
            .await
            .expect("uploads");
    }
    // Two and a half samples. A page that shipped this is truncating.
    socket
        .send(&json!({ "user_audio_chunk": "AAECAwQ=" }))
        .await
        .expect("uploads");
    // Force a round trip so the four frames above are certainly processed.
    socket.ask("did you get that?").await.expect("asks");
    wait_for(&mut socket, "agent_response").await;

    let trace = harness.mock.trace();
    assert_eq!(trace.pcm_frames(), 3, "three good frames");
    assert_eq!(trace.pcm_bytes(), 3 * 320);
    assert_eq!(
        trace.count("malformed_pcm"),
        1,
        "an odd-length chunk is REPORTED, never silently accepted"
    );
}

#[tokio::test]
async fn an_event_before_the_initiation_closes_the_socket_as_a_protocol_error() {
    let harness = Harness::start(Scenario::Full).await;
    let url = harness.mint().await.expect("mints");
    let mut socket = ConversationClient::connect(&url).await.expect("connects");
    socket.ask("hello?").await.expect("sends");

    match socket.next().await.expect("something comes back") {
        Incoming::Closed { code, reason } => {
            assert_eq!(code, 1002, "1002 is 'protocol error'");
            assert_eq!(reason, "initiation_missing");
        }
        other => panic!("expected a protocol close, got {other:?}"),
    }
    assert!(!harness.mock.trace().initiation_seen());
    assert_eq!(harness.mock.trace().count("initiation_missing"), 1);
}

#[tokio::test]
async fn a_ping_nobody_answers_is_noticed() {
    let harness = Harness::start_with(Scenario::Full, |options| MockOptions {
        ping_every: Some(Duration::from_millis(20)),
        ..options
    })
    .await;
    let mut socket = harness.open().await;

    // `next` deliberately does NOT answer pings, so this is the page that stopped talking.
    let mut seen = 0;
    while seen < 2 {
        if let Incoming::Event { kind, .. } = socket
            .next_within(Duration::from_secs(5))
            .await
            .expect("events keep coming")
        {
            if kind == "ping" {
                seen += 1;
            }
        }
    }
    assert!(
        harness.mock.trace().pings_unanswered() >= 1,
        "an unanswered ping must be visible: {:?}",
        harness.mock.trace().of_kind("ping").len()
    );
}

#[tokio::test]
async fn a_ping_the_page_answers_is_not_counted_against_it() {
    let harness = Harness::start_with(Scenario::Full, |options| MockOptions {
        ping_every: Some(Duration::from_millis(20)),
        ..options
    })
    .await;
    let mut socket = harness.open().await;

    // Answer exactly one ping, the way `web/voice.js` does, and stop.
    loop {
        if let Incoming::Event { kind, value } = socket
            .next_within(Duration::from_secs(5))
            .await
            .expect("events keep coming")
        {
            if kind == "ping" {
                socket.pong(&value).await.expect("pongs");
                break;
            }
        }
    }
    // Read one more event so the pong is certainly on the wire and processed.
    let _ = socket.next_within(Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let trace = harness.mock.trace();
    assert!(
        trace.count("pong") >= 1,
        "the page's pong must be recorded; trace was {:?}",
        trace
            .events()
            .iter()
            .map(|e| e.kind.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        trace.pings_unanswered() < trace.count("ping"),
        "an answered ping must not still be counted as outstanding"
    );
}

// --- the nonce -------------------------------------------------------------------------------

#[tokio::test]
async fn a_signed_url_is_single_use() {
    let harness = Harness::start(Scenario::Full).await;
    let url = harness.mint().await.expect("mints");
    let _first = ConversationClient::connect(&url)
        .await
        .expect("first opens");
    let error = ConversationClient::connect(&url)
        .await
        .expect_err("a minted URL is a credential, and it is spent");
    assert!(error.to_string().contains("403"), "{error}");
    assert_eq!(harness.mock.trace().count("token_reused"), 1);
}

#[tokio::test]
async fn a_stale_signed_url_is_refused_by_a_different_name() {
    // "You already used it" and "you took too long" have different fixes, so they are different
    // words even though a browser sees the same immediate close for both.
    let harness = Harness::start_with(Scenario::Full, |options| MockOptions {
        validity: Duration::from_millis(1),
        ..options
    })
    .await;
    let url = harness.mint().await.expect("mints");
    tokio::time::sleep(Duration::from_millis(30)).await;
    let error = ConversationClient::connect(&url)
        .await
        .expect_err("a stale URL is refused");
    assert!(error.to_string().contains("403"), "{error}");
    assert_eq!(harness.mock.trace().count("token_expired"), 1);
    assert_eq!(harness.mock.trace().count("token_reused"), 0);
}

#[tokio::test]
async fn a_token_that_was_never_minted_is_refused() {
    let harness = Harness::start(Scenario::Full).await;
    let url = format!(
        "ws://{}/v1/convai/conversation?agent_id={MOCK_AGENT_ID}&token=invented",
        harness.mock.ws_addr()
    );
    assert!(ConversationClient::connect(&url).await.is_err());
    assert_eq!(harness.mock.trace().count("token_unknown"), 1);
}

// --- the trace -------------------------------------------------------------------------------

#[tokio::test]
async fn nothing_secret_reaches_the_trace() {
    let harness = Harness::start(Scenario::Full).await;
    let url = harness.mint().await.expect("mints");
    let nonce = url
        .rsplit_once("token=")
        .map(|(_, token)| token.to_owned())
        .expect("the minted URL carries a nonce");
    let mut socket = ConversationClient::connect(&url).await.expect("connects");
    socket.initiate().await.expect("initiates");
    wait_for(&mut socket, "conversation_initiation_metadata").await;
    socket
        .send_audio(&vibe_talk::elevenlabs::mock::audio::tone(160))
        .await
        .expect("uploads");
    socket.ask("what is new?").await.expect("asks");
    wait_for(&mut socket, "agent_response").await;

    // The minted URL IS recorded — a trace that only says "200" cannot tell you the page dialled
    // the wrong port — and recording it is what makes the nonce redaction load-bearing rather
    // than decorative.
    let minted = harness.mock.trace().of_kind("minted");
    assert_eq!(minted.len(), 1, "the minted URL must be recorded");
    assert!(
        minted[0].summary.contains("ws://127.0.0.1:"),
        "{}",
        minted[0].summary
    );
    assert!(
        minted[0]
            .summary
            .contains(&format!("token={}", vibe_talk::elevenlabs::REDACTED)),
        "the nonce is a credential and must be redacted in place: {}",
        minted[0].summary
    );

    let rendered = harness.mock.trace().to_json().to_string();
    for secret in [MOCK_API_KEY, WRITE_TOKEN, nonce.as_str()] {
        assert!(
            !rendered.contains(secret),
            "a credential reached the trace, which gets written to a file and pasted into an \
             issue: {secret}"
        );
    }
    // Audio is a LENGTH, never bytes: the same rule `src/access.rs` states for channel text.
    let audio = harness.mock.trace().of_kind("audio");
    assert!(!audio.is_empty(), "audio was sent");
    for event in audio {
        assert!(event.summary.is_empty(), "{:?}", event.summary);
        assert!(event.size > 0, "the length is what is recorded");
    }
    let uploaded = harness.mock.trace().of_kind("user_audio_chunk");
    for event in uploaded {
        assert!(
            !event.summary.contains('='),
            "an uploaded chunk was recorded as base64: {}",
            event.summary
        );
    }
}

#[tokio::test]
async fn the_control_plane_lives_under_a_prefix_the_real_vendor_does_not_have() {
    // A client that accidentally points at api.elevenlabs.io must fail loudly, not half work.
    let harness = Harness::start(Scenario::Full).await;
    let client = reqwest::Client::new();
    let base = harness.mock.control_base();

    let scenario = client
        .post(format!("{base}/_mock/scenario"))
        .json(&json!({ "scenario": "no_tool_call" }))
        .send()
        .await
        .expect("answers");
    assert_eq!(scenario.status().as_u16(), 200);

    let unknown = client
        .post(format!("{base}/_mock/scenario"))
        .json(&json!({ "scenario": "definitely-not-a-scenario" }))
        .send()
        .await
        .expect("answers");
    assert_eq!(
        unknown.status().as_u16(),
        400,
        "an unknown scenario must not silently run the happy path"
    );

    let missing = client
        .get(format!("{base}/v1/text-to-speech/voice"))
        .send()
        .await
        .expect("answers");
    assert_eq!(missing.status().as_u16(), 404);
    let body = missing.text().await.expect("body");
    assert!(body.contains("vibe-talk"), "{body}");
}

#[tokio::test]
async fn saying_something_into_a_closed_conversation_is_refused_rather_than_photographed() {
    let harness = Harness::start(Scenario::Full).await;
    let answer = reqwest::Client::new()
        .post(format!("{}/_mock/say", harness.mock.control_base()))
        .json(&json!({ "who": "agent", "text": "nobody is listening" }))
        .send()
        .await
        .expect("answers");
    assert_eq!(
        answer.status().as_u16(),
        409,
        "a harness that 'said' something into nothing would photograph an empty screen"
    );
}

#[tokio::test]
async fn the_control_plane_can_drive_a_conversation_the_way_a_screenshot_run_would() {
    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;

    let answer = reqwest::Client::new()
        .post(format!("{}/_mock/say", harness.mock.control_base()))
        .json(&json!({ "who": "agent", "text": "byte identical, every run" }))
        .send()
        .await
        .expect("answers");
    assert_eq!(answer.status().as_u16(), 200);

    let events = wait_for(&mut socket, "agent_response").await;
    assert_eq!(
        agent_said(&events).as_deref(),
        Some("byte identical, every run"),
        "the harness's exact text must be what the page renders"
    );
}

#[tokio::test]
async fn the_trace_endpoint_reports_what_happened() {
    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;

    let trace: Value = reqwest::Client::new()
        .get(format!("{}/_mock/trace", harness.mock.control_base()))
        .send()
        .await
        .expect("answers")
        .json()
        .await
        .expect("json");
    let kinds: Vec<&str> = trace["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"conversation_initiation_client_data"),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&"conversation_initiation_metadata"),
        "{kinds:?}"
    );
    assert_eq!(trace["mints"].as_array().expect("mints").len(), 1);
}

#[tokio::test]
async fn the_bridge_the_mock_was_pointed_at_is_the_one_it_calls() {
    // A mock that silently fell back to some other bridge would make every assertion above a
    // statement about the wrong process.
    let harness = Harness::start(Scenario::Full).await;
    assert!(
        harness.bridge_base.starts_with("http://127.0.0.1:"),
        "{}",
        harness.bridge_base
    );
    assert_ne!(harness.bridge_base, harness.mock.control_base());
}

// --- the mute announcement, offline ------------------------------------------------------------
//
// `#73 mute-is-invisible`. `/voice`'s mute withholds audio and nothing else, which is byte-identical
// to the reader going quiet — and going quiet is what makes an agent start asking whether anyone is
// there. So the page now says it, as a `contextual_update` client event on the conversation socket.
//
// WHAT THIS FILE CAN AND CANNOT SETTLE. It can settle our half: the frame the page ships is
// well-formed, a server that MODELS this event accepts it in the middle of a live conversation, it
// does not consume a turn, the text reaches the agent's context, and the conversation still works
// afterwards. It CANNOT settle the vendor's half — whether ElevenLabs implements
// `contextual_update` at all, and whether a real agent reading it holds instead of prompting. The
// mock is our model of the vendor, not the vendor. One billed `scripts/run.sh --smoke-agent`
// conversation answers that, and it has not been run.
//
// AND THE MODELLING IS THE POINT, because the first version of these tests did not have it. The
// mock had no `contextual_update` arm at all, so the frame fell into the catch-all for events it
// does not understand — recorded, answered with silence — and every assertion here was equally
// true of `totally_made_up_event`. `src/elevenlabs/mock/agent.rs` now models the event, and
// `an_unrecognised_client_event_is_not_mistaken_for_a_contextual_update` below is the control that
// keeps these tests from sliding back into that: it pins that an unknown type is recorded under a
// DIFFERENT kind and reaches no context, so renaming the frame here turns this file red.

/// The page's own source, so the frame under test is the frame that ships.
const VOICE_JS: &str = include_str!("../web/voice.js");

/// One string constant from `web/voice.js`, with its `+`-joined pieces put back together.
///
/// Reading the real sentence is the point. A test that invented its own wording would prove that
/// the mock accepts *some* contextual update, which nobody was in doubt about; what is worth
/// pinning is that the bytes `/voice` actually puts on the socket are accepted. It is deliberately
/// strict: a constant it cannot parse is a failure, never a quietly empty string.
fn page_notice(name: &str) -> String {
    let anchor = format!("const {name} =");
    let start = VOICE_JS.find(&anchor).unwrap_or_else(|| {
        panic!("web/voice.js no longer defines `{name}`, so mute tells the agent nothing")
    });
    let rest = &VOICE_JS[start + anchor.len()..];
    let end = rest
        .find(";\n")
        .unwrap_or_else(|| panic!("`{name}` in web/voice.js is not a terminated declaration"));
    let text: String = rest[..end].split('"').skip(1).step_by(2).collect();
    assert!(
        !text.is_empty(),
        "`{name}` in web/voice.js is not a plain string literal this test can read"
    );
    text
}

#[tokio::test]
async fn the_pages_mute_notice_is_accepted_mid_call_and_answered_with_silence() {
    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;

    let notice = page_notice("MUTE_NOTICE");
    assert!(
        notice.contains("muted"),
        "the page's mute notice does not mention muting: {notice}"
    );
    socket
        .send(&json!({ "type": "contextual_update", "text": notice }))
        .await
        .expect("the page's mute frame goes on the wire");

    // The short deadline IS the assertion, exactly as in the no-reply scenario above: an update
    // that consumed a turn would come back as an agent_response, spoken over a reader who muted
    // precisely because they did not want to be spoken to.
    let quiet = socket.next_within(Duration::from_millis(300)).await;
    assert!(
        quiet.is_err(),
        "a contextual update was answered out loud: {quiet:?}"
    );
    assert_eq!(harness.mock.trace().count("agent_response"), 0);

    // The control, without which "this server never says anything" would pass the assertion above.
    // It also proves the socket survived the frame rather than being closed as a protocol error.
    socket.ask("what is new?").await.expect("asks");
    let events = wait_for(&mut socket, "agent_response").await;
    assert!(
        agent_said(&events).is_some(),
        "the conversation stopped working after a contextual update: {events:?}"
    );

    // RECOGNISED, not merely tolerated. An event the mock does not model is recorded under
    // `client_event` with the type string as its summary; a `contextual_update` is recorded under
    // its own kind with the TEXT as its summary. Asserting the second is what makes this test say
    // something about this frame rather than about unknown frames in general.
    let updates = harness.mock.trace().of_kind("contextual_update");
    assert_eq!(
        updates.len(),
        1,
        "the mute notice was not recognised as a contextual update: {:?}",
        harness
            .mock
            .trace()
            .of_kind("client_event")
            .iter()
            .map(|e| e.summary.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(updates[0].summary, notice);
    assert_eq!(
        harness.mock.trace().count("client_event"),
        0,
        "the frame fell through to the mock's unknown-event catch-all"
    );

    // And the half silence cannot show: the sentence is IN the agent's context. A frame that was
    // dropped on the floor is also answered with silence, so without this the assertions above are
    // satisfied by a server that has never heard of the event.
    assert_eq!(
        harness.mock.context(),
        vec![notice.clone()],
        "the mute notice never reached the agent's context"
    );
}

#[tokio::test]
async fn an_unrecognised_client_event_is_not_mistaken_for_a_contextual_update() {
    // THE CONTROL for everything above, and the reason it exists is a real review finding: with no
    // `contextual_update` arm in the mock, the mute test passed unchanged when its frame type was
    // renamed to `totally_made_up_event`. This pins the difference, so that regression cannot come
    // back quietly — the modelled event and the unmodelled one must land in different places.
    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;

    socket
        .send(&json!({ "type": "totally_made_up_event", "text": page_notice("MUTE_NOTICE") }))
        .await
        .expect("an invented event goes on the wire");
    socket.ask("what is new?").await.expect("asks");
    wait_for(&mut socket, "agent_response").await;

    let unknown = harness.mock.trace().of_kind("client_event");
    assert_eq!(
        unknown.len(),
        1,
        "an invented event was not recorded as an unrecognised one"
    );
    assert_eq!(unknown[0].summary, "totally_made_up_event");
    assert_eq!(
        harness.mock.trace().count("contextual_update"),
        0,
        "an invented event was recorded as though the mock understood it"
    );
    assert!(
        harness.mock.context().is_empty(),
        "an invented event put text into the agent's context: {:?}",
        harness.mock.context()
    );
}

#[tokio::test]
async fn a_contextual_update_with_no_text_tells_the_agent_nothing_and_says_so() {
    // The frame's whole payload is its `text`. One without is malformed rather than an update that
    // says nothing, and the distinction is worth a kind of its own: "the page sent an empty
    // announcement" and "the page sent no announcement" have different fixes, and only the first
    // is invisible from the socket. What the REAL vendor does with such a frame is unknown; what
    // this pins is that the mock will not silently count it as context the agent has been given.
    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;

    socket
        .send(&json!({ "type": "contextual_update" }))
        .await
        .expect("sends");
    socket
        .send(&json!({ "type": "contextual_update", "text": "   " }))
        .await
        .expect("sends");
    socket.ask("still there?").await.expect("asks");
    wait_for(&mut socket, "agent_response").await;

    assert_eq!(
        harness.mock.trace().count("contextual_update_without_text"),
        2
    );
    assert_eq!(harness.mock.trace().count("contextual_update"), 0);
    assert!(
        harness.mock.context().is_empty(),
        "an empty update was counted as something the agent was told: {:?}",
        harness.mock.context()
    );
}

#[tokio::test]
async fn the_replay_preamble_is_the_same_event_and_lands_in_the_agents_context() {
    // `#46 conversation-replay` has been putting a `contextual_update` on this socket since before
    // `#73 mute-is-invisible` existed — `web/voice.js` sends it immediately after the initiation
    // frame on the default transport — and the mock never modelled that one either. Same event,
    // same contract, so it is pinned in the same place: the previous conversation's summary must
    // reach the agent's context, and must not cost the turn the reader is waiting for.
    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;

    let preamble = "Earlier today you and he discussed the mac runner stalling.";
    socket
        .send(&json!({ "type": "contextual_update", "text": preamble }))
        .await
        .expect("the replay preamble goes on the wire");

    let quiet = socket.next_within(Duration::from_millis(300)).await;
    assert!(
        quiet.is_err(),
        "the replay preamble was answered out loud, before the reader said anything: {quiet:?}"
    );
    assert_eq!(harness.mock.context(), vec![preamble.to_owned()]);
    assert_eq!(harness.mock.trace().count("agent_response"), 0);
}

#[tokio::test]
async fn the_unmute_notice_is_the_other_sentence_and_lifts_the_hold() {
    // Both halves ship or neither is worth shipping: an agent told to hold and never told to stop
    // is a worse conversation than one that occasionally asks whether you are there.
    let muted = page_notice("MUTE_NOTICE");
    let back = page_notice("UNMUTE_NOTICE");
    assert_ne!(
        muted, back,
        "mute and unmute say the same thing to the agent"
    );
    assert!(
        back.contains("unmuted"),
        "the page's unmute notice does not say the pause ended: {back}"
    );
    assert!(
        !back.contains("Do not ask"),
        "the unmute notice repeats the hold instruction, so the agent keeps waiting: {back}"
    );

    let harness = Harness::start(Scenario::Full).await;
    let mut socket = harness.open().await;
    wait_for(&mut socket, "conversation_initiation_metadata").await;

    for text in [&muted, &back] {
        socket
            .send(&json!({ "type": "contextual_update", "text": text }))
            .await
            .expect("sends");
    }
    socket.ask("still with me?").await.expect("asks");
    let events = wait_for(&mut socket, "agent_response").await;
    assert!(agent_said(&events).is_some(), "{events:?}");
    assert_eq!(
        harness.mock.trace().count("contextual_update"),
        2,
        "a mute and an unmute are two announcements, not one"
    );
    // In that order, and both of them: an agent that was told to hold and then told nothing else
    // holds for the rest of the call, which is the failure this pair exists to prevent.
    assert_eq!(harness.mock.context(), vec![muted.clone(), back.clone()]);
}
