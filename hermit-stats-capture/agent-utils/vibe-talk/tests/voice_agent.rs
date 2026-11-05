//! `#14 voice-agent-prompt` and `#11 voice-connect-latency`, through the real router.
//!
//! The profile is only useful if it agrees with what the MCP endpoint will actually do for the
//! same credential, so these compare the two directly rather than against a list typed out here.
//! The timing endpoint is only safe if it refuses anything but phase numbers, so these send it
//! the things a careless client would.

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::http::router;
use vibe_talk::testing::{READ_TOKEN, WRITE_TOKEN};
use vibe_talk::voice_agent;

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if uri == "/mcp" {
        builder = builder.header("accept", "application/json, text/event-stream");
    }
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = router
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

fn app() -> axum::Router {
    let (state, _discord) = vibe_talk::testing::state();
    router(state)
}

async fn listed_tools(router: &axum::Router, token: &str) -> BTreeSet<String> {
    let (status, body) = send(
        router,
        "POST",
        "/mcp",
        Some(token),
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name").to_owned())
        .collect()
}

fn available(profile: &Value) -> BTreeSet<String> {
    profile["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter(|tool| tool["available"] == json!(true))
        .map(|tool| tool["name"].as_str().expect("name").to_owned())
        .collect()
}

#[tokio::test]
async fn the_profile_agrees_with_tools_list_for_every_scope() {
    let router = app();
    for (token, scope, writes) in [(READ_TOKEN, "read", false), (WRITE_TOKEN, "write", true)] {
        let (status, profile) =
            send(&router, "GET", "/api/v1/voice-agent", Some(token), None).await;
        assert_eq!(status, StatusCode::OK, "{profile}");
        assert_eq!(profile["scope"], scope);
        assert_eq!(profile["write_available"], json!(writes));
        assert_eq!(
            available(&profile),
            listed_tools(&router, token).await,
            "the profile and tools/list disagree for the {scope} token"
        );
        assert_eq!(profile["mcp_path"], "/mcp");
        assert_eq!(profile["mentions"]["source"], "returned_token");
    }
}

#[tokio::test]
async fn a_read_caller_is_told_writing_exists_but_is_not_theirs() {
    let router = app();
    let (_, profile) = send(
        &router,
        "GET",
        "/api/v1/voice-agent",
        Some(READ_TOKEN),
        None,
    )
    .await;
    let writes: Vec<&Value> = profile["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter(|tool| tool["access"] == "write")
        .collect();
    assert!(!writes.is_empty(), "no write tool is described at all");
    for tool in writes {
        assert_eq!(tool["available"], json!(false), "{tool}");
        assert_eq!(tool["requires_confirmation"], json!(true), "{tool}");
    }
}

#[tokio::test]
async fn the_profile_carries_the_versioned_prompt_verbatim() {
    let router = app();
    let (_, profile) = send(
        &router,
        "GET",
        "/api/v1/voice-agent",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(profile["prompt"]["id"], voice_agent::PROMPT_ID);
    assert_eq!(
        profile["prompt"]["version"],
        json!(voice_agent::PROMPT_VERSION)
    );
    assert_eq!(profile["prompt"]["variant"], "read");
    assert_eq!(
        profile["prompt"]["fingerprint"],
        voice_agent::PINNED_READ_FINGERPRINT
    );
    assert_eq!(
        profile["prompt"]["text"],
        voice_agent::prompt(voice_agent::Variant::Read).text
    );
}

#[tokio::test]
async fn a_read_credential_is_not_told_it_may_send_and_a_write_credential_is() {
    let router = app();
    let (_, read) = send(
        &router,
        "GET",
        "/api/v1/voice-agent",
        Some(READ_TOKEN),
        None,
    )
    .await;
    let read_text = read["prompt"]["text"].as_str().expect("prompt text");
    assert_eq!(read["write_available"], json!(false));
    assert!(read_text.contains("cannot post or send"), "{read_text}");
    assert!(
        !read_text.contains(voice_agent::SEND_SECTION.trim()),
        "{read_text}"
    );
    assert!(!read_text.contains("explicitly requests it"), "{read_text}");

    let (_, write) = send(
        &router,
        "GET",
        "/api/v1/voice-agent",
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    let write_text = write["prompt"]["text"].as_str().expect("prompt text");
    assert_eq!(write["write_available"], json!(true));
    // The shipped setting asks before the agent sends. `#264 voice-send-approval`.
    assert_eq!(write["prompt"]["variant"], "write-confirm");
    assert_eq!(write["ask_before_send"], json!(true));
    assert_eq!(
        write["prompt"]["fingerprint"],
        voice_agent::PINNED_WRITE_CONFIRM_FINGERPRINT
    );
    assert!(
        write_text.contains(voice_agent::SEND_SECTION.trim()),
        "{write_text}"
    );
    assert!(!write_text.contains("cannot post or send"), "{write_text}");
}

#[tokio::test]
async fn neither_endpoint_answers_without_a_token() {
    let router = app();
    let (status, _) = send(&router, "GET", "/api/v1/voice-agent", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let timing = json!({ "protocol": "vibe-talk-v1", "provider_ready": 10 });
    let (status, _) = send(&router, "POST", "/api/v1/voice-timing", None, Some(timing)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_startup_record_of_phase_numbers_is_accepted_from_the_write_token() {
    let router = app();
    let timing = json!({
        "protocol": "vibe-talk-v1",
        "chat": false,
        "session_acquired": 40,
        "microphone_ready": 310,
        "socket_open": 520,
        "provider_ready": 2480,
        "greeting_text": 3900,
        "greeting_audio_received": 3950,
        "greeting_audible": 4050,
    });
    let (status, _) = send(
        &router,
        "POST",
        "/api/v1/voice-timing",
        Some(READ_TOKEN),
        Some(timing.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token wrote to the log"
    );
    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/voice-timing",
        Some(WRITE_TOKEN),
        Some(timing),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
async fn anything_but_phase_numbers_is_refused_rather_than_logged() {
    let router = app();
    for (why, timing) in [
        (
            "an undocumented field could carry speech",
            json!({ "protocol": "vibe-talk-v1", "greeting_text_value": "hello" }),
        ),
        (
            "a transcript is not a phase",
            json!({ "protocol": "vibe-talk-v1", "transcript": "summarize the channel" }),
        ),
        (
            "an unknown protocol is not a measurement",
            json!({ "protocol": "made-up", "provider_ready": 10 }),
        ),
        (
            "a phase longer than the bound",
            json!({ "protocol": "vibe-talk-v1", "provider_ready": voice_agent::MAX_PHASE_MS + 1 }),
        ),
        (
            "a phase is milliseconds, not text",
            json!({ "protocol": "vibe-talk-v1", "provider_ready": "soon" }),
        ),
    ] {
        let (status, body) = send(
            &router,
            "POST",
            "/api/v1/voice-timing",
            Some(WRITE_TOKEN),
            Some(timing),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
        assert_eq!(body["error"], "invalid_timing", "{why}");
    }
}

// --- voice-unresponsive-signal -------------------------------------------------------------------

/// A report the page could send after two silent turns, as the failure that motivated it looked.
fn silent_report() -> Value {
    json!({
        "protocol": "vibe-talk-v1",
        "chat": false,
        "cause": "silent_turns",
        "since_open_ms": 21_400,
        "turns": 3,
        "silent_turns": 2,
        "audio_ms": 1_000,
        "peak": 0,
    })
}

#[tokio::test]
async fn a_health_report_is_accepted_from_the_write_token_only() {
    let router = app();
    let (status, _) = send(
        &router,
        "POST",
        "/api/v1/voice-health",
        None,
        Some(silent_report()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        &router,
        "POST",
        "/api/v1/voice-health",
        Some(READ_TOKEN),
        Some(silent_report()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token wrote to the log"
    );
    for cause in [
        "silent_greeting",
        "silent_turns",
        "no_reply",
        "error_frame",
        "recovered",
    ] {
        let mut report = silent_report();
        report["cause"] = cause.into();
        let (status, body) = send(
            &router,
            "POST",
            "/api/v1/voice-health",
            Some(WRITE_TOKEN),
            Some(report),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{cause}: {body}");
    }
}

#[tokio::test]
async fn anything_but_the_closed_health_record_is_refused_rather_than_logged() {
    let router = app();
    let with = |key: &str, value: Value| {
        let mut report = silent_report();
        report[key] = value;
        report
    };
    let without = |key: &str| {
        let mut report = silent_report();
        report.as_object_mut().expect("object").remove(key);
        report
    };
    for (why, report) in [
        (
            "an error message is free text",
            with("message", json!("the upstream failed")),
        ),
        (
            "a transcript is not a verdict",
            with("transcript", json!("summarize the channel")),
        ),
        (
            "an unknown cause is refused, not echoed",
            with("cause", json!("made_up")),
        ),
        (
            "an unknown protocol is refused, not echoed",
            with("protocol", json!("some-vendor")),
        ),
        (
            "a count is a number, not text",
            with("turns", json!("three")),
        ),
        ("a peak past the 16-bit range", with("peak", json!(32_769))),
        (
            "more silent turns than turns",
            with("silent_turns", json!(4)),
        ),
        ("a missing field is not a zero", without("peak")),
    ] {
        let (status, body) = send(
            &router,
            "POST",
            "/api/v1/voice-health",
            Some(WRITE_TOKEN),
            Some(report),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
        assert_eq!(body["error"], "invalid_health", "{why}");
    }
}

#[tokio::test]
async fn an_oversized_health_body_is_not_read() {
    let router = app();
    let padded = json!({ "padding": "x".repeat(vibe_talk::voice_health::MAX_BODY_BYTES) });
    let (status, _) = send(
        &router,
        "POST",
        "/api/v1/voice-health",
        Some(WRITE_TOKEN),
        Some(padded),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

// --- #268 voice-call-drops: how a call ended ------------------------------------------------------

/// The record the page sends when a call's connection is reset twenty-two seconds in.
fn end_report() -> Value {
    json!({
        "protocol": "vibe-talk-v1",
        "chat": false,
        "ended_by": "connection_lost",
        "close_code": 1006,
        "since_open_ms": 22_000,
        "turns": 2,
    })
}

#[tokio::test]
async fn a_call_end_record_is_accepted_from_the_write_token_only_for_every_documented_cause() {
    let router = app();
    let (status, _) = send(
        &router,
        "POST",
        "/api/v1/voice-end",
        None,
        Some(end_report()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        &router,
        "POST",
        "/api/v1/voice-end",
        Some(READ_TOKEN),
        Some(end_report()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token wrote to the log"
    );
    for ended_by in [
        "hang_up",
        "error_frame",
        "server_closed",
        "connection_lost",
        "suspended",
    ] {
        let mut report = end_report();
        report["ended_by"] = ended_by.into();
        let (status, body) = send(
            &router,
            "POST",
            "/api/v1/voice-end",
            Some(WRITE_TOKEN),
            Some(report),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{ended_by}: {body}");
    }
    // A page that closed the socket itself has no close code to give.
    let mut report = end_report();
    report["ended_by"] = "hang_up".into();
    report.as_object_mut().expect("object").remove("close_code");
    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/voice-end",
        Some(WRITE_TOKEN),
        Some(report),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
async fn anything_but_the_closed_call_end_record_is_refused_rather_than_logged() {
    let router = app();
    let with = |key: &str, value: Value| {
        let mut report = end_report();
        report[key] = value;
        report
    };
    let without = |key: &str| {
        let mut report = end_report();
        report.as_object_mut().expect("object").remove(key);
        report
    };
    for (why, report) in [
        (
            "a close reason is free text",
            with(
                "reason",
                json!("Connection reset without closing handshake"),
            ),
        ),
        (
            "an unknown cause is refused, not echoed",
            with("ended_by", json!("timer")),
        ),
        (
            "an unknown protocol is refused, not echoed",
            with("protocol", json!("some-vendor")),
        ),
        (
            "a close code outside the WebSocket range",
            with("close_code", json!(999)),
        ),
        (
            "a call longer than a day is not a measurement",
            with("since_open_ms", json!(86_400_001)),
        ),
        ("a missing cause is not a hang-up", without("ended_by")),
    ] {
        let (status, body) = send(
            &router,
            "POST",
            "/api/v1/voice-end",
            Some(WRITE_TOKEN),
            Some(report),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
        assert_eq!(body["error"], "invalid_end", "{why}");
    }
}

#[tokio::test]
async fn one_caller_cannot_flood_the_log_with_call_end_records() {
    let router = app();
    for n in 0..vibe_talk::voice_health::MAX_LINES_PER_WINDOW {
        let (status, body) = send(
            &router,
            "POST",
            "/api/v1/voice-end",
            Some(WRITE_TOKEN),
            Some(end_report()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "record {n}: {body}");
    }
    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/voice-end",
        Some(WRITE_TOKEN),
        Some(end_report()),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"], "voice_end_throttled");
    // Its own budget: the health record is not silenced by a flood of these.
    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/voice-health",
        Some(WRITE_TOKEN),
        Some(silent_report()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
async fn one_caller_cannot_flood_the_log_with_health_records() {
    let router = app();
    for n in 0..vibe_talk::voice_health::MAX_LINES_PER_WINDOW {
        let (status, body) = send(
            &router,
            "POST",
            "/api/v1/voice-health",
            Some(WRITE_TOKEN),
            Some(silent_report()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "record {n}: {body}");
    }
    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/voice-health",
        Some(WRITE_TOKEN),
        Some(silent_report()),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"], "voice_health_throttled");
}
