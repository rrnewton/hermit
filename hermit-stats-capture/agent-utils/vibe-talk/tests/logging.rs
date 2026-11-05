//! What the access log must contain, and what it must never contain.
//!
//! These tests exist because of a specific incident: a voice agent claimed to have read a Discord
//! channel and posted a reply, and had done neither — it invented a digest and named tools this
//! server does not have. The claim could only be disproved by going and looking at the channel,
//! because the server logged nothing per request, so "the agent never called" and "the agent
//! called and we answered" were the same output. Every assertion below is aimed at making that
//! question answerable from one line of log.
//!
//! Because the whole point is that an absence of evidence was mistaken for evidence, each test
//! that asserts something is ABSENT from the log also asserts that the log was not simply empty.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::http::router;
use vibe_talk::model::ChannelId;
use vibe_talk::testing::{LogCapture, READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};

struct Harness {
    router: axum::Router,
    discord: std::sync::Arc<vibe_talk::discord::fake::FakeDiscord>,
}

fn harness() -> Harness {
    let (state, discord) = vibe_talk::testing::state();
    Harness {
        router: router(state),
        discord,
    }
}

async fn call(
    harness: &Harness,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
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
    let _ = response.into_body().collect().await.expect("body");
    status
}

async fn rpc(harness: &Harness, token: Option<&str>, body: Value) -> StatusCode {
    call(harness, "POST", "/mcp", token, Some(body)).await
}

fn tools_call(name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

#[tokio::test]
async fn every_request_leaves_exactly_one_line() {
    let capture = LogCapture::info_only();
    let harness = harness();
    assert_eq!(
        call(&harness, "GET", "/api/v1/channels", Some(READ_TOKEN), None).await,
        StatusCode::OK
    );
    let lines: Vec<String> = capture
        .access_lines()
        .into_iter()
        .filter(|l| l.contains("request"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one request line, got: {lines:?}"
    );
    let line = &lines[0];
    for field in [
        "method=\"GET\"",
        "path=\"/api/v1/channels\"",
        "credential=\"read\"",
        "status=200",
    ] {
        assert!(line.contains(field), "line is missing {field}: {line}");
    }
    assert!(line.contains("millis="), "no duration recorded: {line}");
}

#[tokio::test]
async fn a_refused_request_is_logged_as_loudly_as_an_accepted_one() {
    // The case under investigation is a client that may never have gotten in. A log that only
    // records successful calls cannot tell that story.
    let capture = LogCapture::info_only();
    let harness = harness();
    call(&harness, "GET", "/api/v1/channels", None, None).await;
    call(
        &harness,
        "GET",
        "/api/v1/channels",
        Some("a-stale-token-000000000000"),
        None,
    )
    .await;
    call(&harness, "GET", "/no/such/route", Some(READ_TOKEN), None).await;

    let text = capture.text();
    assert!(
        text.contains("credential=\"absent\"") && text.contains("status=401"),
        "an unauthenticated request left no legible line: {text}"
    );
    assert!(
        text.contains("credential=\"unrecognized\""),
        "a wrong token must be distinguishable from no token at all: {text}"
    );
    assert!(
        text.contains("path=\"/no/such/route\"") && text.contains("status=404"),
        "a request to a path that does not exist -- the most likely shape of a misconfigured \
         client -- was not logged: {text}"
    );
}

#[tokio::test]
async fn an_mcp_tool_call_is_identifiable_by_tool_and_channel() {
    let capture = LogCapture::info_only();
    let harness = harness();
    harness
        .discord
        .seed(&ChannelId(READ_CHANNEL.to_owned()), "codex-eng", "hello");
    assert_eq!(
        rpc(
            &harness,
            Some(READ_TOKEN),
            tools_call("digest_channel", json!({ "channel_id": READ_CHANNEL })),
        )
        .await,
        StatusCode::OK
    );
    let text = capture.text();
    assert!(
        text.contains("rpc_method=\"tools/call\""),
        "the JSON-RPC method is not in the log; every MCP call is a POST /mcp, so without it the \
         log cannot tell tools/list from a real tool call: {text}"
    );
    assert!(
        text.contains("tool=\"digest_channel\""),
        "the tool name is missing: {text}"
    );
    assert!(
        text.contains(&format!("channel=\"{READ_CHANNEL}\"")),
        "the channel is missing: {text}"
    );
    assert!(
        text.contains("outcome=\"ok\""),
        "the outcome is missing: {text}"
    );
}

#[tokio::test]
async fn a_tool_the_server_does_not_have_is_logged_as_refused() {
    // Precisely the confabulation case: the agent named `web_scraper`, which does not exist here.
    // Either it never called, or it called and was refused, and the log has to say which.
    let capture = LogCapture::info_only();
    let harness = harness();
    rpc(
        &harness,
        Some(READ_TOKEN),
        tools_call("web_scraper", json!({ "url": "https://example.test" })),
    )
    .await;
    let text = capture.text();
    assert!(text.contains("tool=\"web_scraper\""), "{text}");
    assert!(text.contains("outcome=\"refused\""), "{text}");
    assert!(text.contains("reason=\"unknown_tool\""), "{text}");
}

#[tokio::test]
async fn a_refusal_records_why_it_was_refused() {
    let capture = LogCapture::info_only();
    let harness = harness();
    // The scope fence.
    rpc(
        &harness,
        Some(READ_TOKEN),
        tools_call(
            "post_reply",
            json!({ "channel_id": WRITE_CHANNEL, "text": "nope" }),
        ),
    )
    .await;
    // An operational rule: a configured but read-only channel.
    rpc(
        &harness,
        Some(WRITE_TOKEN),
        tools_call(
            "post_reply",
            json!({ "channel_id": READ_CHANNEL, "text": "nope" }),
        ),
    )
    .await;
    let text = capture.text();
    assert!(
        text.contains("reason=\"forbidden_scope\""),
        "a scope refusal must name the scope fence: {text}"
    );
    assert!(
        text.contains("reason=\"channel_not_writable\""),
        "an operational refusal must carry its code: {text}"
    );
}

#[tokio::test]
async fn a_post_records_its_length_but_not_its_words_at_info() {
    let capture = LogCapture::info_only();
    let harness = harness();
    let secret_sounding_text = "the-message-body-nobody-should-find-in-a-log";
    assert_eq!(
        rpc(
            &harness,
            Some(WRITE_TOKEN),
            tools_call(
                "post_reply",
                json!({ "channel_id": WRITE_CHANNEL, "text": secret_sounding_text }),
            ),
        )
        .await,
        StatusCode::OK
    );
    let text = capture.text();
    assert!(
        text.contains("tool=\"post_reply\"") && text.contains("outcome=\"ok\""),
        "the log did not record the post at all, so the absence below proves nothing: {text}"
    );
    assert!(
        text.contains(&format!(
            "text_len={}",
            secret_sounding_text.chars().count()
        )),
        "the length is what answers 'did the whole message go through?': {text}"
    );
    assert!(
        !text.contains(secret_sounding_text),
        "channel content reached the INFO log: {text}"
    );
}

#[tokio::test]
async fn message_content_read_out_of_a_channel_never_reaches_the_info_log() {
    let capture = LogCapture::info_only();
    let harness = harness();
    let content = "third-party-text-that-must-not-be-copied-into-an-operator-log";
    harness
        .discord
        .seed(&ChannelId(READ_CHANNEL.to_owned()), "someone", content);
    assert_eq!(
        call(
            &harness,
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/messages"),
            Some(READ_TOKEN),
            None,
        )
        .await,
        StatusCode::OK
    );
    let text = capture.text();
    assert!(
        text.contains(&format!(
            "path=\"/api/v1/channels/{READ_CHANNEL}/messages\""
        )),
        "the read was not logged, so the absence below proves nothing: {text}"
    );
    assert!(!text.contains(content), "channel text leaked into the log");
}

#[tokio::test]
async fn no_credential_ever_appears_in_the_log() {
    // Every path a token can arrive on, at the level a deployment runs at. Not truncated, not
    // hashed, not prefixed: absent.
    let capture = LogCapture::info_only();
    let harness = harness();
    call(&harness, "GET", "/api/v1/channels", Some(READ_TOKEN), None).await;
    call(&harness, "GET", "/api/v1/channels", Some(WRITE_TOKEN), None).await;
    call(
        &harness,
        "GET",
        "/api/v1/channels",
        Some("a-stale-token-000000000000"),
        None,
    )
    .await;
    rpc(
        &harness,
        Some(WRITE_TOKEN),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    call(
        &harness,
        "GET",
        "/api/v1/signed-url",
        Some(WRITE_TOKEN),
        None,
    )
    .await;

    let text = capture.text();
    assert!(
        text.contains("credential=\"read\"") && text.contains("credential=\"write\""),
        "the log recorded nothing, so this test would pass on a server that logs nothing: {text}"
    );
    for secret in [
        READ_TOKEN,
        WRITE_TOKEN,
        "a-stale-token-000000000000",
        "test-bot-token",
        vibe_talk::elevenlabs::fake::VALID_API_KEY,
    ] {
        assert!(
            !text.contains(secret),
            "a credential reached the log: {text}"
        );
        // A prefix long enough to confirm a guess is a leak too.
        let prefix = &secret[..12.min(secret.len())];
        assert!(
            !text.contains(prefix),
            "a credential PREFIX ({prefix}) reached the log: {text}"
        );
    }
}

#[tokio::test]
async fn tool_arguments_are_available_at_debug_and_only_at_debug() {
    // The content rule is a LEVEL rule, not a "we happen not to log it" rule: the detail exists
    // for debugging, and turning INFO on must not turn it on.
    let content = "content-visible-only-when-debugging";
    let debug = LogCapture::start();
    {
        let harness = harness();
        rpc(
            &harness,
            Some(WRITE_TOKEN),
            tools_call(
                "post_reply",
                json!({ "channel_id": WRITE_CHANNEL, "text": content }),
            ),
        )
        .await;
    }
    assert!(
        debug.text().contains(content),
        "the DEBUG detail line is missing, so the INFO assertion below is vacuous: {}",
        debug.text()
    );
    drop(debug);

    let info = LogCapture::info_only();
    {
        let harness = harness();
        rpc(
            &harness,
            Some(WRITE_TOKEN),
            tools_call(
                "post_reply",
                json!({ "channel_id": WRITE_CHANNEL, "text": content }),
            ),
        )
        .await;
    }
    assert!(
        !info.text().contains(content),
        "content that is supposed to be DEBUG-only appeared at INFO: {}",
        info.text()
    );
}

#[tokio::test]
async fn a_server_nobody_called_logs_nothing_which_is_the_whole_point() {
    // The control for every test above. If a line appeared here, "the log is empty" would stop
    // meaning "nobody called", and the log would be back to answering nothing.
    let capture = LogCapture::info_only();
    let _harness = harness();
    assert!(
        capture.access_lines().is_empty(),
        "building a server emitted access lines, so an empty log would no longer prove that \
         nothing called: {:?}",
        capture.access_lines()
    );
}

#[tokio::test]
async fn a_stream_leaves_one_line_at_attach_and_it_reads_as_instant() {
    // `#44 live-push`. The access-log middleware returns as soon as the status is known, and for a
    // streaming body that is BEFORE a single event has gone out — so a stream held open for an
    // hour logs `millis=0`. That is not a defect and it is not going to be fixed here; it is a
    // thing a reader of the log will otherwise misread as an instant request, so it is pinned.
    //
    // What is a defect and what this also rules out: a stream logging nothing at all, or logging
    // again per event. The `#41`-era question "did the client call us?" has to be answerable for
    // the route that stays connected, not only for the ones that answer and go.
    let capture = LogCapture::info_only();
    let harness = harness();
    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/channels/{READ_CHANNEL}/stream"))
        .header("authorization", format!("Bearer {READ_TOKEN}"))
        .body(Body::empty())
        .expect("request");
    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    // The body is deliberately NOT read: it never ends, and the point is that the line is already
    // there before anything is read from it.
    let lines: Vec<String> = capture
        .access_lines()
        .into_iter()
        .filter(|l| l.contains("request"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "a stream must leave exactly one line, at attach: {lines:?}"
    );
    assert!(
        lines[0].contains("path=\"/api/v1/channels/1111111111/stream\""),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("status=200"), "{}", lines[0]);
    assert!(
        lines[0].contains("millis=0"),
        "the attach line is written before the first event, so it must read as zero — if this \
         ever becomes non-zero the log has started waiting for the stream to END, which would \
         delay every line by however long the reader stayed connected: {}",
        lines[0]
    );
    drop(response);
}

#[tokio::test]
async fn a_tool_line_names_an_integer_channel_and_the_shape_of_an_unreadable_one() {
    // A channel id that arrives in a shape the server cannot read was logged as `-`, exactly like
    // no channel at all, so a live refusal could not say which of the two it had been sent.
    let capture = LogCapture::info_only();
    let harness = harness();
    let number: u64 = READ_CHANNEL.parse().expect("digits");
    let near: f64 = READ_CHANNEL.parse().expect("digits");
    for (arguments, expected) in [
        (
            json!({ "channel_id": number }),
            format!("channel=\"{READ_CHANNEL}\""),
        ),
        (
            json!({ "channel_id": near }),
            "channel=\"<unreadable number>\"".to_owned(),
        ),
        (
            json!({ "channel_id": null }),
            "channel=\"<null>\"".to_owned(),
        ),
        (json!({}), "channel=\"-\"".to_owned()),
        (
            json!(format!("{{\"channel_id\": \"{READ_CHANNEL}\"}}")),
            "channel=\"<string arguments>\"".to_owned(),
        ),
    ] {
        let before = capture.access_lines().len();
        rpc(
            &harness,
            Some(READ_TOKEN),
            tools_call("digest_channel", arguments.clone()),
        )
        .await;
        let lines = capture.access_lines();
        let tool_line = lines[before..]
            .iter()
            .find(|line| line.contains("tool=\"digest_channel\""))
            .unwrap_or_else(|| panic!("no tool line for {arguments}: {lines:?}"));
        assert!(tool_line.contains(&expected), "{arguments}: {tool_line}");
    }
}

#[tokio::test]
async fn a_voice_health_record_is_one_line_of_names_and_numbers_and_a_refused_one_is_none() {
    let capture = LogCapture::info_only();
    let harness = harness();
    let report = json!({
        "protocol": "vibe-talk-v1",
        "chat": false,
        "cause": "silent_turns",
        "since_open_ms": 21_400,
        "turns": 3,
        "silent_turns": 2,
        "audio_ms": 1_000,
        "peak": 0,
    });
    let mut smuggled = report.clone();
    smuggled["message"] = json!("the upstream failed");
    assert_eq!(
        call(
            &harness,
            "POST",
            "/api/v1/voice-health",
            Some(WRITE_TOKEN),
            Some(smuggled)
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &harness,
            "POST",
            "/api/v1/voice-health",
            Some(WRITE_TOKEN),
            Some(report.clone())
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let mut recovered = report;
    recovered["cause"] = json!("recovered");
    assert_eq!(
        call(
            &harness,
            "POST",
            "/api/v1/voice-health",
            Some(WRITE_TOKEN),
            Some(recovered)
        )
        .await,
        StatusCode::NO_CONTENT
    );

    let text = capture.text();
    assert!(!text.is_empty(), "nothing was captured at all");
    assert!(
        !text.contains("upstream failed"),
        "refused free text reached the log: {text}"
    );
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("voice_unresponsive"))
        .collect();
    assert_eq!(lines.len(), 1, "expected one unresponsive line: {lines:?}");
    assert!(lines[0].contains("WARN"), "{}", lines[0]);
    let fields = lines[0]
        .split("voice_unresponsive ")
        .nth(1)
        .expect("fields follow the name");
    assert_eq!(
        fields,
        "protocol=vibe-talk-v1 chat=false cause=silent_turns since_open_ms=21400 turns=3 \
         silent_turns=2 audio_ms=1000 peak=0"
    );
    let recoveries: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("voice_recovered"))
        .collect();
    assert_eq!(recoveries.len(), 1, "{recoveries:?}");
    assert!(recoveries[0].contains("INFO") && recoveries[0].contains("cause=recovered"));
}

/// `#268 voice-call-drops`. How a call ended is one line of names and numbers: a hang-up at info,
/// anything that ended the call for the reader at warn, and a refused record is no line at all.
#[tokio::test]
async fn a_call_end_record_is_one_line_and_says_who_ended_the_call() {
    let capture = LogCapture::info_only();
    let harness = harness();
    let dropped = json!({
        "protocol": "vibe-talk-v1",
        "chat": false,
        "ended_by": "connection_lost",
        "close_code": 1006,
        "since_open_ms": 22_000,
        "turns": 2,
    });
    let mut smuggled = dropped.clone();
    smuggled["reason"] = json!("Connection reset without closing handshake");
    assert_eq!(
        call(
            &harness,
            "POST",
            "/api/v1/voice-end",
            Some(WRITE_TOKEN),
            Some(smuggled)
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &harness,
            "POST",
            "/api/v1/voice-end",
            Some(WRITE_TOKEN),
            Some(dropped)
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let hang_up = json!({
        "protocol": "vibe-talk-v1",
        "chat": false,
        "ended_by": "hang_up",
        "since_open_ms": 95_000,
        "turns": 7,
    });
    assert_eq!(
        call(
            &harness,
            "POST",
            "/api/v1/voice-end",
            Some(WRITE_TOKEN),
            Some(hang_up)
        )
        .await,
        StatusCode::NO_CONTENT
    );

    let text = capture.text();
    assert!(
        !text.contains("closing handshake"),
        "refused free text reached the log: {text}"
    );
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("voice_call_ended "))
        .collect();
    assert_eq!(lines.len(), 2, "expected one line per call: {lines:?}");
    assert!(lines[0].contains("WARN"), "{}", lines[0]);
    assert_eq!(
        lines[0].split("voice_call_ended ").nth(1),
        Some(
            "protocol=vibe-talk-v1 chat=false ended_by=connection_lost close_code=1006 \
             since_open_ms=22000 turns=2"
        )
    );
    assert!(lines[1].contains("INFO"), "{}", lines[1]);
    assert_eq!(
        lines[1].split("voice_call_ended ").nth(1),
        Some(
            "protocol=vibe-talk-v1 chat=false ended_by=hang_up close_code=- since_open_ms=95000 \
             turns=7"
        )
    );
}

/// A throttled route says so once per window, not once per refused record.
#[tokio::test]
async fn throttled_health_records_are_announced_once_per_window() {
    let capture = LogCapture::info_only();
    let harness = harness();
    let report = json!({
        "protocol": "vibe-talk-v1",
        "chat": false,
        "cause": "no_reply",
        "since_open_ms": 15_000,
        "turns": 0,
        "silent_turns": 0,
        "audio_ms": 0,
        "peak": 0,
    });
    let cap = vibe_talk::voice_health::MAX_LINES_PER_WINDOW;
    for n in 0..cap + 3 {
        let expected = if n < cap {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(
            call(
                &harness,
                "POST",
                "/api/v1/voice-health",
                Some(WRITE_TOKEN),
                Some(report.clone())
            )
            .await,
            expected,
            "record {n}"
        );
    }
    let text = capture.text();
    let throttled: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("voice_health_throttled max_lines="))
        .collect();
    assert_eq!(throttled.len(), 1, "{throttled:?}");
    assert!(throttled[0].contains("WARN"), "{}", throttled[0]);
    assert!(
        throttled[0].contains("max_lines=30 window_s=60"),
        "{}",
        throttled[0]
    );
    let written = text
        .lines()
        .filter(|l| l.contains("voice_unresponsive"))
        .count();
    assert_eq!(written, 30);
}

/// One JSON request, answering with its status and body.
async fn json_call(
    harness: &Harness,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {WRITE_TOKEN}"))
        .header("accept", "application/json, text/event-stream");
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

#[tokio::test]
async fn the_post_gate_logs_every_step_without_a_word_of_the_message_or_its_handle() {
    // `#34 voice-chat-write-confirm`, rule 4: refusals and commits are logged, and nothing said
    // is. The handle is a capability, so it is kept out of the log as firmly as the text.
    const SECRET: &str = "the vault code is 4417 and nobody may read it";
    let capture = LogCapture::info_only();
    let harness = harness();
    let mut handles = Vec::new();
    let propose = || async {
        let status = rpc(
            &harness,
            Some(WRITE_TOKEN),
            tools_call(
                "post_reply",
                json!({ "channel_id": WRITE_CHANNEL, "text": SECRET }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_status, body) = json_call(&harness, "GET", "/api/v1/post-proposals", None).await;
        body["proposal"].clone()
    };
    let restated = |proposal: &Value, text: &str, confirmed_by: &str| {
        json!({
            "handle": proposal["handle"],
            "channel_id": proposal["channel_id"],
            "text": text,
            "reply_to": proposal["reply_to"],
            "confirmed_by": confirmed_by,
        })
    };
    let first = propose().await;
    let edited = format!("{SECRET}!");
    let (status, _) = json_call(
        &harness,
        "POST",
        "/api/v1/post-proposals/commit",
        Some(restated(&first, &edited, "ui")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let second = propose().await;
    let third = propose().await;
    let (status, _) = json_call(
        &harness,
        "POST",
        "/api/v1/post-proposals/cancel",
        Some(json!({ "handle": third["handle"] })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let fourth = propose().await;
    let (status, _) = json_call(
        &harness,
        "POST",
        "/api/v1/post-proposals/commit",
        Some(restated(&fourth, SECRET, "speaker_turn")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.discord.posted().len(), 1);
    // A handle the server never issued is refused, logged, and kept out of the log too: a guess
    // at a capability is not something to write down either.
    let guessed = "gUeSsEd-handle-0123456";
    let (status, _) = json_call(
        &harness,
        "POST",
        "/api/v1/post-proposals/commit",
        Some(json!({
            "handle": guessed,
            "channel_id": WRITE_CHANNEL,
            "text": SECRET,
            "reply_to": null,
            "confirmed_by": "ui",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(harness.discord.posted().len(), 1);
    handles.push(guessed.to_owned());
    for proposal in [&first, &second, &third, &fourth] {
        handles.push(proposal["handle"].as_str().expect("a handle").to_owned());
    }

    let gate_lines = capture
        .access_lines()
        .into_iter()
        .filter(|line| line.contains("post_gate"))
        .collect::<Vec<_>>()
        .join("\n");
    for step in [
        "event=\"proposed\"",
        "event=\"refused\"",
        "event=\"superseded\"",
        "event=\"cancelled\"",
        "event=\"committed\"",
        "reason=\"proposal_mismatch\"",
        "reason=\"proposal_unknown\"",
        "confirmed_by=\"speaker_turn\"",
    ] {
        assert!(gate_lines.contains(step), "no {step} in:\n{gate_lines}");
    }
    assert!(
        gate_lines.contains(&format!("text_len={}", SECRET.chars().count())),
        "{gate_lines}"
    );
    let everything = capture.text();
    assert!(
        !everything.contains("vault code"),
        "message text reached the INFO log:\n{everything}"
    );
    for handle in &handles {
        assert!(
            !everything.contains(handle.as_str()),
            "a handle reached the log:\n{everything}"
        );
    }
}

/// `#191 read-aloud-first-lag`: a read aloud leaves its phases in the log, and none of its text.
///
/// One line per preparation (lookup, fetch, normalization, whether the warm-up was started), one
/// when the warm-up it started finishes, and the browser's line with the time to speech as well as
/// to the first frame. Numbers and labels only.
#[tokio::test]
async fn read_aloud_logs_each_phase_and_never_the_message() {
    const SPOKEN: &str = "the words a reader asked to hear";
    let capture = LogCapture::info_only();
    let harness = harness();
    let id = harness
        .discord
        .seed(&ChannelId(READ_CHANNEL.to_owned()), "Reader", SPOKEN);
    // The page draws the channel, and then asks for what it drew to be prepared.
    assert_eq!(
        call(
            &harness,
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/page"),
            Some(READ_TOKEN),
            None,
        )
        .await,
        StatusCode::OK
    );
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/channels/{READ_CHANNEL}/speech/prepare"))
        .header("authorization", format!("Bearer {READ_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "ids": [id.0] }).to_string()))
        .expect("request");
    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let prepared: Value = serde_json::from_slice(&bytes).expect("prepared");
    let url = prepared["prepared"][0]["url"]
        .as_str()
        .expect("ticket")
        .to_owned();
    // The warm-up runs beside the request; let it finish on this thread.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        call(&harness, "GET", &url, None, None).await,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &harness,
            "POST",
            &format!("{url}/timing"),
            None,
            Some(json!({
                "tap_to_play_ms": 3,
                "request_to_loaded_ms": 40,
                "loaded_to_playing_ms": 500,
                "tap_to_audible_ms": 543,
                "tap_to_speech_ms": 1210
            })),
        )
        .await,
        StatusCode::NO_CONTENT
    );

    let text = capture.text();
    let line_with = |marker: &str| -> String {
        text.lines()
            .find(|line| line.contains(marker))
            .unwrap_or_else(|| panic!("no {marker:?} line in:\n{text}"))
            .to_owned()
    };
    let preparation = line_with("read-aloud preparation");
    for field in [
        "ids=1",
        "threaded=false",
        "recent_hits=1",
        "fetches=0",
        "resolved=1",
        "lookup_ms=",
        "fetch_ms=0",
        "normalize_ms=",
        "preparation_ms=",
        "warm_up=\"started\"",
    ] {
        assert!(
            preparation.contains(field),
            "{field} missing from {preparation}"
        );
    }
    let warm_up = line_with("read-aloud warm-up finished");
    assert!(warm_up.contains("warm_up_ms="), "{warm_up}");
    assert!(warm_up.contains("ok=true"), "{warm_up}");
    let audible = line_with("read-aloud reached audible playback");
    assert!(audible.contains("tap_to_speech_ms=1210"), "{audible}");
    assert!(audible.contains("tap_to_audible_ms=543"), "{audible}");
    assert!(
        !text.contains(SPOKEN),
        "message text reached the INFO log:\n{text}"
    );
}

/// A server voice whose warm-up always fails, with a detail no log may carry.
struct FailingWarmUp;

/// In the failing warm-up's detail, and so in no log line.
const WARM_UP_DETAIL: &str = "the provider said something private about the account";

#[async_trait::async_trait]
impl vibe_talk::speech::SpeechProvider for FailingWarmUp {
    fn describe(&self) -> vibe_talk::speech::Description {
        vibe_talk::speech::Description {
            backend: "failing-warm-up",
            label: "Failing warm-up".to_owned(),
            playback: vibe_talk::speech::Playback::Audio,
            local_only: false,
        }
    }

    async fn warm_up(&self) -> Result<(), vibe_talk::speech::SpeechError> {
        Err(vibe_talk::speech::SpeechError::Backend {
            code: "conversation_speech_error",
            detail: WARM_UP_DETAIL.to_owned(),
        })
    }

    async fn speak(
        &self,
        text: &str,
        _speed: Option<f64>,
    ) -> Result<vibe_talk::speech::Speech, vibe_talk::speech::SpeechError> {
        Ok(vibe_talk::speech::Speech {
            audio: text.as_bytes().to_vec(),
            content_type: "audio/test".to_owned(),
        })
    }
}

/// `#191 read-aloud-first-lag`: a warm-up that fails is a WARN, so it shows without reading every
/// INFO line, and it names the provider's error code, never the provider's words.
#[tokio::test]
async fn a_failed_read_aloud_warm_up_is_a_warning_with_its_code_and_nothing_else() {
    let capture = LogCapture::info_only();
    let (mut state, discord) = vibe_talk::testing::state();
    state.speech = std::sync::Arc::new(FailingWarmUp);
    let harness = Harness {
        router: router(state),
        discord,
    };
    let id = harness.discord.seed(
        &ChannelId(READ_CHANNEL.to_owned()),
        "Reader",
        "a line to hear",
    );
    assert_eq!(
        call(
            &harness,
            "POST",
            &format!("/api/v1/channels/{READ_CHANNEL}/speech/prepare"),
            Some(READ_TOKEN),
            Some(json!({ "ids": [id.0] })),
        )
        .await,
        StatusCode::OK,
        "a failing warm-up must not fail the preparation"
    );
    // The warm-up runs beside the request; let it finish on this thread.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let text = capture.text();
    let line = text
        .lines()
        .find(|line| line.contains("read-aloud warm-up finished"))
        .unwrap_or_else(|| panic!("no warm-up line in:\n{text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("ok=false"), "{line}");
    assert!(
        line.contains("code=\"conversation_speech_error\""),
        "{line}"
    );
    assert!(
        !text.contains(WARM_UP_DETAIL),
        "the provider's detail reached the log:\n{text}"
    );
}

/// `#210 voice-catchup-speed`. Every tool line says how long the call took, a catch-up says how
/// its channels were answered, and a call the caller gave up on leaves a line of its own. Before,
/// an abandoned call left neither a tool line nor a request line: the one call that mattered most
/// was the one the log could not see. Here rather than beside the other catch-up tests because
/// a capture only sees a line whose call site was first reached under some capture.
#[tokio::test(start_paused = true)]
async fn a_tool_call_is_timed_and_an_abandoned_one_still_leaves_a_line() {
    use std::sync::Arc;
    use std::time::Duration;
    use vibe_talk::mcp::protocol::{dispatch, Outcome};

    let capture = LogCapture::info_only();
    let (mut state, discord) = vibe_talk::testing::state();
    discord.seed(
        &ChannelId(READ_CHANNEL.to_owned()),
        "codex-eng",
        "secret plan",
    );
    let slow = Arc::new(vibe_talk::testing::SlowChat::new(
        Arc::clone(&discord),
        Duration::from_secs(2),
    ));
    let client: Arc<dyn vibe_talk::chat::ChatClient> = slow;
    state.replace_chat(client);
    let call = tools_call("recent_activity", json!({}));

    let abandoned = tokio::time::timeout(
        Duration::from_millis(500),
        dispatch(&state, vibe_talk::auth::Scope::Read, &call),
    )
    .await;
    assert!(abandoned.is_err(), "the caller gave up first");
    let lines = capture.access_lines();
    let gave_up = lines
        .iter()
        .find(|line| line.contains("outcome=\"abandoned\""))
        .unwrap_or_else(|| panic!("an abandoned call left no line: {lines:#?}"));
    assert!(gave_up.contains("tool=\"recent_activity\""), "{gave_up}");
    assert!(gave_up.contains("millis="), "{gave_up}");

    // The reads the abandoned call started land; the next call is answered from them.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let outcome = dispatch(&state, vibe_talk::auth::Scope::Read, &call).await;
    assert!(matches!(outcome, Outcome::Reply(_)), "{outcome:?}");
    let lines = capture.access_lines();
    let answered = lines
        .iter()
        .find(|line| line.contains("outcome=\"ok\""))
        .unwrap_or_else(|| panic!("no answered line: {lines:#?}"));
    assert!(answered.contains("millis="), "{answered}");
    let sources = lines
        .iter()
        .find(|line| line.contains("recent_activity current="))
        .unwrap_or_else(|| panic!("no catch-up line: {lines:#?}"));
    assert!(
        sources.contains("current=2") && sources.contains("held=0") && sources.contains("unread=0"),
        "{sources}"
    );
    assert!(
        !capture.text().contains("secret plan"),
        "channel text reached the log: {}",
        capture.text()
    );
}

#[tokio::test]
async fn a_direct_send_and_the_setting_that_allowed_it_are_logged_without_a_word_said() {
    // `#264 voice-send-approval`. With confirmation off the post is logged as one `sent_direct`
    // line — a number, the channel and a length — and turning confirmation off is logged as the
    // boolean alone.
    const SECRET: &str = "the vault code is 4417 and nobody may read it";
    let capture = LogCapture::info_only();
    let harness = harness();
    let (status, body) = json_call(
        &harness,
        "PUT",
        "/api/v1/voice-settings",
        Some(json!({ "ask_before_send": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let status = rpc(
        &harness,
        Some(WRITE_TOKEN),
        tools_call(
            "post_reply",
            json!({ "channel_id": WRITE_CHANNEL, "text": SECRET }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.discord.posted().len(), 1);
    // The same words again are answered from the first send, posting nothing, and logged as such.
    let status = rpc(
        &harness,
        Some(WRITE_TOKEN),
        tools_call(
            "post_reply",
            json!({ "channel_id": WRITE_CHANNEL, "text": SECRET }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.discord.posted().len(), 1);
    harness.discord.fail_next("the chat service is down");
    let status = rpc(
        &harness,
        Some(WRITE_TOKEN),
        tools_call(
            "post_reply",
            json!({ "channel_id": WRITE_CHANNEL, "text": format!("{SECRET}, again") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.discord.posted().len(), 1);

    let lines = capture.access_lines();
    let sent = lines
        .iter()
        .find(|line| line.contains("event=\"sent_direct\""))
        .unwrap_or_else(|| panic!("no sent_direct line in:\n{}", lines.join("\n")));
    assert!(sent.contains("post_gate"), "{sent}");
    assert!(
        sent.contains(&format!("channel=\"{WRITE_CHANNEL}\""))
            || sent.contains(&format!("channel={WRITE_CHANNEL}")),
        "{sent}"
    );
    assert!(
        sent.contains(&format!("text_len={}", SECRET.chars().count())),
        "{sent}"
    );
    assert!(
        !sent.contains("proposal=-1"),
        "the direct send was not numbered: {sent}"
    );
    let failed = lines
        .iter()
        .find(|line| line.contains("event=\"direct_failed\""))
        .unwrap_or_else(|| panic!("no direct_failed line in:\n{}", lines.join("\n")));
    // A first part that fails is a partial send of nothing, and is coded as one.
    assert!(failed.contains("reason=\"partially_posted\""), "{failed}");
    let repeat = lines
        .iter()
        .find(|line| line.contains("event=\"direct_repeat\""))
        .unwrap_or_else(|| panic!("no direct_repeat line in:\n{}", lines.join("\n")));
    // The earlier send's number and what became of it, and still not a word said.
    assert!(repeat.contains("reason=\"sent\""), "{repeat}");
    assert!(!repeat.contains("proposal=-1"), "{repeat}");
    let changed = lines
        .iter()
        .find(|line| line.contains("voice_settings"))
        .unwrap_or_else(|| panic!("no voice_settings line in:\n{}", lines.join("\n")));
    assert!(changed.contains("ask_before_send=false"), "{changed}");
    let everything = capture.text();
    assert!(
        !everything.contains("vault code"),
        "message text reached the INFO log:\n{everything}"
    );
}

#[tokio::test]
async fn saving_the_owners_words_for_the_voice_agent_logs_sizes_and_never_the_words() {
    // `#272 voice-agent-instructions`. The instructions and the edited prompt are the owner's own
    // text; the access log records that he changed them, how long the instructions are, and
    // whether the prompt is edited, and nothing he wrote.
    const SECRET: &str = "my sister's name is Priya and the vault code is 4417";
    let capture = LogCapture::info_only();
    let harness = harness();
    let (status, body) = json_call(
        &harness,
        "PUT",
        "/api/v1/voice-prompt",
        Some(json!({ "instructions": SECRET, "system_prompt": format!("{SECRET}, edited") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let lines = capture.access_lines();
    let changed = lines
        .iter()
        .find(|line| line.contains("voice_prompt"))
        .unwrap_or_else(|| panic!("no voice_prompt line in:\n{}", lines.join("\n")));
    assert!(changed.contains("event=\"changed\""), "{changed}");
    assert!(
        changed.contains(&format!("instructions_chars={}", SECRET.chars().count())),
        "{changed}"
    );
    assert!(changed.contains("system_prompt_edited=true"), "{changed}");
    let everything = capture.text();
    assert!(
        !everything.contains("vault code") && !everything.contains("Priya"),
        "the owner's words reached the INFO log:\n{everything}"
    );
}

#[tokio::test]
async fn the_summary_route_and_taking_a_channel_off_the_list_log_numbers_and_never_a_key() {
    // `#208 persistent-cache`. A hit from memory, a shared fetch, a generated summary, a stored one
    // kept alive, and a channel whose summaries are deleted with it: none of them may put a message
    // id, a content hash or a word of a summary into the INFO log.
    use vibe_talk::store::StateStore as _;
    let capture = LogCapture::info_only();
    let (state, discord, store, summarizer) =
        vibe_talk::testing::state_with_summarizer(&vibe_talk::testing::config_toml());
    let answer = "SUMMARY-WORDS-THAT-MUST-NOT-REACH-THE-LOG ".repeat(4);
    summarizer.answer_with(&answer);
    let content = "MESSAGE-WORDS-THAT-MUST-NOT-REACH-THE-LOG ".repeat(20);
    let id = discord
        .seed(&ChannelId(READ_CHANNEL.to_owned()), "someone", &content)
        .0;
    let harness = Harness {
        router: router(state.clone()),
        discord,
    };
    let summary = format!("/api/v1/channels/{READ_CHANNEL}/messages/{id}/summary");
    for token in [WRITE_TOKEN, WRITE_TOKEN, READ_TOKEN] {
        assert_eq!(
            call(&harness, "GET", &summary, Some(token), None).await,
            StatusCode::OK
        );
    }
    assert_eq!(summarizer.calls(), 1, "the later asks were hits");
    assert_eq!(
        call(
            &harness,
            "DELETE",
            &format!("/api/v1/channels/{READ_CHANNEL}"),
            Some(WRITE_TOKEN),
            None,
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        store
            .cached_summary(&vibe_talk::store::SummaryKey {
                channel: ChannelId(READ_CHANNEL.to_owned()),
                message: vibe_talk::model::MessageId(id.clone()),
                content_hash: vibe_talk::summarize::content_hash(&content),
                version: state.summary_version.to_string(),
            })
            .await
            .expect("readable"),
        None,
        "the hidden channel's summary was not deleted"
    );
    let text = capture.text();
    assert!(
        text.contains("asked the summariser for one message")
            && text.contains("summaries_deleted=1"),
        "the summary and the removal were not logged, so the absences below prove nothing: {text}"
    );
    let hash = format!("{:016x}", vibe_talk::summarize::content_hash(&content));
    for secret in [
        "SUMMARY-WORDS-THAT-MUST-NOT-REACH-THE-LOG",
        "MESSAGE-WORDS-THAT-MUST-NOT-REACH-THE-LOG",
        hash.as_str(),
        state.summary_version.as_ref(),
    ] {
        assert!(
            !text.contains(secret),
            "{secret} reached the INFO log: {text}"
        );
    }
    // The message id is in the request path, which the access log records by design; it must not
    // appear anywhere ELSE, in a line of the summary route's own.
    for line in text.lines().filter(|line| !line.contains("path=")) {
        assert!(
            !line.contains(&id),
            "the message id reached the INFO log: {line}"
        );
    }
}

#[tokio::test]
async fn the_hourly_upstream_line_is_names_and_numbers_and_never_the_channel() {
    // `#208 persistent-cache`, milestone `upstream-profile`. One line per provider per hour says
    // how long its operations took and how big the messages read were. The provider key, the
    // operation names and outcome words are this server's own; everything else is a number.
    let capture = LogCapture::info_only();
    let (state, discord) = vibe_talk::testing::state();
    let profile = std::sync::Arc::clone(state.providers.profile());
    let harness = Harness {
        router: router(state),
        discord,
    };
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let content = "third-party-words-that-must-not-reach-the-hourly-line";
    let id = harness
        .discord
        .seed(&channel, "Somebody Particular", content);
    assert_eq!(
        call(
            &harness,
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/messages"),
            Some(READ_TOKEN),
            None,
        )
        .await,
        StatusCode::OK
    );

    profile.log_hour();
    let hourly = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| line.contains("upstream timings for the last hour"))
            .map(str::to_owned)
            .collect()
    };
    let lines = hourly(&capture.text());
    assert_eq!(lines.len(), 1, "one line per provider: {lines:?}");
    let line = &lines[0];
    for field in [
        "provider=\"discord\"",
        "operations=",
        "operations_failed=0",
        "operation_timings=\"",
        " ok n=",
        "p90_ms=",
        "shim_calls=0",
        "content_messages=",
        "content_bytes_p90=",
    ] {
        assert!(
            line.contains(field),
            "the hourly line has no {field}: {line}"
        );
    }
    assert!(
        !line.contains("content_messages=0"),
        "the read's message was not counted, so the absence below proves nothing: {line}"
    );
    for private in [content, "Somebody Particular", READ_CHANNEL, id.0.as_str()] {
        assert!(
            !line.contains(private),
            "the hourly line carries {private:?}: {line}"
        );
    }

    // The next hour starts from nothing: the line is about that hour, not the process.
    profile.log_hour();
    let lines = hourly(&capture.text());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[1].contains("operations=0") && lines[1].contains("content_messages=0"),
        "{}",
        lines[1]
    );
}
