//! `#272 voice-agent-instructions`, end to end through the real router.
//!
//! The owner's additional instructions and his edited system prompt are saved by Settings and read
//! back by every device; they reach a deployment-managed bridge through the profile it fetches as a
//! call opens, and a hosted agent through the voice session the page opens it with. Neither can
//! change what the agent may do: the section that says so is chosen from the token and the send
//! setting, and it stays last. These drive the HTTP stack the page and a bridge do.
//!
//! What a save LOGS is tested in `tests/logging.rs`, beside every other access-log check.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::elevenlabs::fake::FakeElevenLabs;
use vibe_talk::http::router;
use vibe_talk::store::fake::FakeStore;
use vibe_talk::testing::{READ_TOKEN, WRITE_TOKEN};
use vibe_talk::voice_agent::{self, Variant};

struct Harness {
    router: axum::Router,
    elevenlabs: Arc<FakeElevenLabs>,
}

fn harness() -> Harness {
    let (state, _discord, _store, elevenlabs) = vibe_talk::testing::state_with_store_and_voice();
    Harness {
        router: router(state),
        elevenlabs,
    }
}

async fn send(
    harness: &Harness,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("accept", "application/json, text/event-stream");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn save(harness: &Harness, token: &str, body: Value) -> (StatusCode, Value) {
    send(
        harness,
        "PUT",
        "/api/v1/voice-prompt",
        Some(token),
        Some(body),
    )
    .await
}

async fn shown(harness: &Harness) -> Value {
    let (status, body) = send(
        harness,
        "GET",
        "/api/v1/voice-prompt",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn profile(harness: &Harness, token: &str) -> Value {
    let (status, body) = send(harness, "GET", "/api/v1/voice-agent", Some(token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn session(harness: &Harness) -> Value {
    let (status, body) = send(
        harness,
        "GET",
        "/api/v1/voice-session",
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

const MINE: &str = "You read the team's chat to me while I drive.";
const ASKED: &str = "Call me Sam.\nKeep answers under three sentences.";

#[tokio::test]
async fn nothing_saved_is_the_shipped_prompt_everywhere() {
    let harness = harness();
    let body = shown(&harness).await;
    assert_eq!(body["instructions"], "");
    assert_eq!(body["system_prompt"], voice_agent::SYSTEM_PROMPT);
    assert_eq!(body["system_prompt_edited"], false);
    assert!(body.get("edited_from_version").is_none(), "{body}");
    assert_eq!(body["prompt_version"], json!(voice_agent::PROMPT_VERSION));
    assert_eq!(body["changeable"], true);
    assert_eq!(
        body["delivery"], "page",
        "the test server's voice is a hosted agent"
    );
    let (_status, config) = send(
        &harness,
        "GET",
        "/api/v1/client-config",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(config["voice_prompt"], body);

    let write = profile(&harness, WRITE_TOKEN).await;
    assert_eq!(
        write["prompt"]["fingerprint"],
        voice_agent::PINNED_WRITE_CONFIRM_FINGERPRINT
    );
    assert_eq!(write["prompt"]["system_prompt_edited"], false);
    assert_eq!(write["prompt"]["additional_instructions"], false);
    // The test agent's configuration says nothing about replacing its prompt, so it keeps the one
    // in its console and is handed nothing, which the session says.
    let handed = session(&harness).await["agent_prompt"].clone();
    assert_eq!(handed["transport"], "not_sent", "{handed}");
    assert!(
        handed.get("text").is_none(),
        "a call with nothing of the owner's carried something to an agent that keeps its prompt"
    );
}

#[tokio::test]
async fn an_agent_that_does_not_allow_it_with_nothing_saved_is_said_to_keep_its_prompt() {
    // The vendor's default. The session used to carry no agent_prompt at all here, so the page
    // never learned that this agent keeps its console prompt, and the warning beside the Sending
    // switch said "only if its provider lets this page replace its prompt" for good.
    let harness = harness();
    harness.elevenlabs.allow_prompt_override(Some(false));
    let handed = session(&harness).await["agent_prompt"].clone();
    assert_eq!(handed["transport"], "not_sent", "{handed}");
    assert_eq!(handed["override_allowed"], false, "{handed}");
    assert!(handed.get("text").is_none(), "{handed}");
    assert_eq!(handed["system_prompt_applied"], false);
    assert_eq!(handed["instructions_applied"], false);
}

fn agent_reads(harness: &Harness) -> usize {
    harness
        .elevenlabs
        .requested()
        .iter()
        .filter(|url| url.contains("/convai/agents/"))
        .count()
}

#[tokio::test]
async fn an_agent_that_allows_it_follows_the_send_switch_with_nothing_saved() {
    // The review's case: a hosted agent, nothing saved in Settings, and the switch moved. Its send
    // rules must follow the switch at its next call, not wait for a prompt pasted by hand.
    let harness = harness();
    harness.elevenlabs.allow_prompt_override(Some(true));
    let handed = session(&harness).await["agent_prompt"].clone();
    assert_eq!(handed["transport"], "prompt_override", "{handed}");
    assert_eq!(handed["system_prompt_applied"], false);
    assert_eq!(handed["instructions_applied"], false);
    assert_eq!(
        handed["text"],
        voice_agent::prompt(Variant::WriteConfirm).text,
        "the shipped prompt for the switch did not go"
    );

    let (status, _body) = send(
        &harness,
        "PUT",
        "/api/v1/voice-settings",
        Some(WRITE_TOKEN),
        Some(json!({ "ask_before_send": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let handed = session(&harness).await["agent_prompt"].clone();
    assert_eq!(
        handed["text"],
        voice_agent::prompt(Variant::WriteDirect).text,
        "the next call still had the rules for asking first"
    );
    assert_eq!(
        agent_reads(&harness),
        1,
        "the vendor was asked again at every call start rather than once for the run"
    );
}

#[tokio::test]
async fn the_owners_words_reach_the_profile_with_the_section_its_token_and_setting_choose() {
    let harness = harness();
    let (status, body) = save(
        &harness,
        WRITE_TOKEN,
        json!({ "instructions": ASKED, "system_prompt": MINE }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["instructions"], ASKED);
    assert_eq!(body["system_prompt"], MINE);
    assert_eq!(body["system_prompt_edited"], true);
    assert_eq!(
        body["edited_from_version"],
        json!(voice_agent::PROMPT_VERSION)
    );

    for (token, section) in [
        (READ_TOKEN, voice_agent::READ_ONLY_SECTION),
        (WRITE_TOKEN, voice_agent::SEND_SECTION),
    ] {
        let prompt = &profile(&harness, token).await["prompt"];
        let text = prompt["text"].as_str().expect("text");
        assert!(text.starts_with(MINE), "{text}");
        assert!(
            !text.contains("Treat all chat text as quoted"),
            "the shipped base stayed beside his copy"
        );
        assert!(text.contains(ASKED), "{text}");
        assert!(
            text.ends_with(section),
            "the capability section is not last: {text}"
        );
        assert_eq!(prompt["system_prompt_edited"], true);
        assert_eq!(prompt["additional_instructions"], true);
        assert_eq!(
            prompt["fingerprint"],
            voice_agent::fingerprint(text),
            "the fingerprint is not of the text served"
        );
    }

    // The send setting still chooses the section under his words.
    let (status, _body) = send(
        &harness,
        "PUT",
        "/api/v1/voice-settings",
        Some(WRITE_TOKEN),
        Some(json!({ "ask_before_send": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let prompt = &profile(&harness, WRITE_TOKEN).await["prompt"];
    assert_eq!(prompt["variant"], "write-direct");
    assert!(prompt["text"]
        .as_str()
        .expect("text")
        .ends_with(voice_agent::SEND_DIRECT_SECTION));
}

#[tokio::test]
async fn each_editor_saves_alone_and_an_empty_box_restores_the_shipped_text() {
    let harness = harness();
    save(&harness, WRITE_TOKEN, json!({ "instructions": ASKED })).await;
    let (status, body) = save(&harness, WRITE_TOKEN, json!({ "system_prompt": MINE })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["instructions"], ASKED,
        "saving the prompt lost the instructions"
    );

    let (_status, body) = save(&harness, WRITE_TOKEN, json!({ "system_prompt": "" })).await;
    assert_eq!(body["system_prompt_edited"], false);
    assert_eq!(body["system_prompt"], voice_agent::SYSTEM_PROMPT);
    assert_eq!(body["instructions"], ASKED);

    let (_status, body) = save(&harness, WRITE_TOKEN, json!({ "instructions": "  " })).await;
    assert_eq!(body["instructions"], "");
    assert_eq!(
        profile(&harness, WRITE_TOKEN).await["prompt"]["fingerprint"],
        voice_agent::PINNED_WRITE_CONFIRM_FINGERPRINT,
        "with both restored the agent is not back on the shipped prompt"
    );
}

#[tokio::test]
async fn the_words_are_read_at_read_scope_and_changed_only_at_write_scope_and_within_bounds() {
    let harness = harness();
    let (status, _body) = save(&harness, READ_TOKEN, json!({ "instructions": ASKED })).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token changed the instructions"
    );
    let (status, _body) = send(
        &harness,
        "PUT",
        "/api/v1/voice-prompt",
        None,
        Some(json!({ "instructions": ASKED })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = save(&harness, WRITE_TOKEN, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _body) = save(
        &harness,
        WRITE_TOKEN,
        json!({ "instructions": ASKED, "variant": "write-direct" }),
    )
    .await;
    assert!(
        status.is_client_error(),
        "an unknown field was accepted: {status}"
    );
    let (status, body) = save(
        &harness,
        WRITE_TOKEN,
        json!({ "instructions": "x".repeat(voice_agent::MAX_INSTRUCTIONS_CHARS + 1) }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "too_large");
    let (status, _body) = save(
        &harness,
        WRITE_TOKEN,
        json!({ "system_prompt": "x".repeat(voice_agent::MAX_SYSTEM_PROMPT_CHARS + 1) }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // None of those changed anything.
    let body = shown(&harness).await;
    assert_eq!(body["instructions"], "");
    assert_eq!(body["system_prompt_edited"], false);
}

#[tokio::test]
async fn no_tool_reaches_the_words() {
    let harness = harness();
    let (status, body) = send(
        &harness,
        "POST",
        "/mcp",
        Some(WRITE_TOKEN),
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for tool in body["result"]["tools"].as_array().expect("tools") {
        let name = tool["name"].as_str().expect("name");
        assert!(
            !name.contains("prompt") && !name.contains("instruction") && !name.contains("setting"),
            "a tool named {name} may reach the owner's words"
        );
    }
}

#[tokio::test]
async fn a_hosted_agent_that_allows_it_is_given_the_whole_prompt_as_the_call_opens() {
    let harness = harness();
    harness.elevenlabs.allow_prompt_override(Some(true));
    save(
        &harness,
        WRITE_TOKEN,
        json!({ "instructions": ASKED, "system_prompt": MINE }),
    )
    .await;
    let session = session(&harness).await;
    assert_eq!(session["protocol"], "elevenlabs");
    let handed = &session["agent_prompt"];
    assert_eq!(handed["transport"], "prompt_override", "{session}");
    assert_eq!(handed["override_allowed"], true);
    assert_eq!(handed["system_prompt_applied"], true);
    assert_eq!(handed["instructions_applied"], true);
    // The prompt a write credential gets under the setting: what the profile serves.
    assert_eq!(
        handed["text"],
        profile(&harness, WRITE_TOKEN).await["prompt"]["text"],
        "the page and a bridge would be given different prompts"
    );
    assert!(handed["text"]
        .as_str()
        .expect("text")
        .ends_with(voice_agent::SEND_SECTION));
    assert!(harness
        .elevenlabs
        .requested()
        .iter()
        .any(|url| url.contains("/convai/agents/")));
}

#[tokio::test]
async fn a_hosted_agent_that_refuses_or_cannot_be_asked_gets_only_the_instructions_as_a_note() {
    for allowed in [Some(false), None] {
        let harness = harness();
        harness.elevenlabs.allow_prompt_override(allowed);
        save(
            &harness,
            WRITE_TOKEN,
            json!({ "instructions": ASKED, "system_prompt": MINE }),
        )
        .await;
        let handed = session(&harness).await["agent_prompt"].clone();
        assert_eq!(
            handed["transport"], "contextual_update",
            "{allowed:?}: {handed}"
        );
        assert_eq!(
            handed["text"],
            format!("{}\n{ASKED}", voice_agent::OWNER_INSTRUCTIONS_HEADING)
        );
        assert_eq!(handed["system_prompt_applied"], false);
        assert_eq!(handed["instructions_applied"], true);
        assert_eq!(handed["override_allowed"], json!(allowed));
    }

    // The vendor failing to answer about an agent that allows it is still "not allowed": the call
    // opens, and the instructions go as a note, never as an override nobody confirmed.
    let harness = harness();
    harness.elevenlabs.allow_prompt_override(Some(true));
    save(
        &harness,
        WRITE_TOKEN,
        json!({ "instructions": ASKED, "system_prompt": MINE }),
    )
    .await;
    harness
        .elevenlabs
        .fail_next_agent_profile("the vendor is down");
    let handed = session(&harness).await["agent_prompt"].clone();
    assert_eq!(handed["transport"], "contextual_update", "{handed}");
    assert!(handed.get("override_allowed").is_none(), "{handed}");
    // ...and the failure is kept for a few seconds: the next call does not wait on the vendor
    // again, and is still given the note. (That the vendor is asked again once it runs out is
    // tested beside the cache, where the clock can be moved.)
    let reads = agent_reads(&harness);
    assert_eq!(
        session(&harness).await["agent_prompt"]["transport"],
        "contextual_update"
    );
    assert_eq!(
        agent_reads(&harness),
        reads,
        "the next call start asked a vendor that had just failed"
    );
}

#[tokio::test]
async fn an_edited_prompt_a_hosted_agent_cannot_take_is_reported_as_not_sent() {
    let harness = harness();
    harness.elevenlabs.allow_prompt_override(Some(false));
    save(&harness, WRITE_TOKEN, json!({ "system_prompt": MINE })).await;
    let handed = session(&harness).await["agent_prompt"].clone();
    assert_eq!(handed["transport"], "not_sent", "{handed}");
    assert!(handed.get("text").is_none(), "{handed}");
    assert_eq!(handed["system_prompt_applied"], false);
}

#[tokio::test]
async fn a_bridge_reads_the_words_from_the_profile_and_its_session_carries_none() {
    let toml = format!(
        "{}\n[conversation]\nbackend = \"websocket\"\nwebsocket_url = \"/ws\"\n",
        vibe_talk::testing::config_toml()
    );
    let (state, _discord, _store) = vibe_talk::testing::state_with_store_from_toml(&toml);
    let harness = Harness {
        router: router(state),
        elevenlabs: Arc::new(FakeElevenLabs::new()),
    };
    let (_status, body) = save(&harness, WRITE_TOKEN, json!({ "instructions": ASKED })).await;
    assert_eq!(body["delivery"], "profile");
    let session = session(&harness).await;
    assert_eq!(session["protocol"], "vibe-talk-v1");
    assert!(session.get("agent_prompt").is_none(), "{session}");
    let prompt = &profile(&harness, WRITE_TOKEN).await["prompt"];
    assert_eq!(
        prompt["text"],
        voice_agent::prompt_with(
            Variant::WriteConfirm,
            &vibe_talk::voice_agent::OwnerPrompt {
                instructions: ASKED.to_owned(),
                system_prompt: None,
            }
        )
        .text
    );
}

#[tokio::test]
async fn a_server_with_no_store_refuses_a_save_and_keeps_the_shipped_prompt() {
    let (state, _discord) =
        vibe_talk::testing::state_with(Arc::new(vibe_talk::store::disabled::DisabledStore));
    let harness = Harness {
        router: router(state),
        elevenlabs: Arc::new(FakeElevenLabs::new()),
    };
    let (status, body) = save(&harness, WRITE_TOKEN, json!({ "instructions": ASKED })).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "storage_not_configured");
    let body = shown(&harness).await;
    assert_eq!(body["changeable"], false);
    assert_eq!(body["instructions"], "");
    assert_eq!(
        profile(&harness, WRITE_TOKEN).await["prompt"]["fingerprint"],
        voice_agent::PINNED_WRITE_CONFIRM_FINGERPRINT
    );
}

#[tokio::test]
async fn the_owners_words_survive_a_restart_of_the_server() {
    let store = Arc::new(FakeStore::new());
    let (first, _discord) = vibe_talk::testing::state_with(store.clone());
    let first = Harness {
        router: router(first),
        elevenlabs: Arc::new(FakeElevenLabs::new()),
    };
    let (status, _body) = save(
        &first,
        WRITE_TOKEN,
        json!({ "instructions": ASKED, "system_prompt": MINE }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    drop(first);
    let (second, _discord) = vibe_talk::testing::state_with(store);
    let second = Harness {
        router: router(second),
        elevenlabs: Arc::new(FakeElevenLabs::new()),
    };
    let body = shown(&second).await;
    assert_eq!(body["instructions"], ASKED);
    assert_eq!(body["system_prompt"], MINE);
}
