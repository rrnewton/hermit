//! `#34 voice-chat-write-confirm`, end to end through the real router.
//!
//! A model's `post_reply` proposes; only the application's commit route posts, and only with the
//! single-use handle, the proposal restated exactly, and a confirmation the model cannot produce.
//! Every test here drives the same HTTP stack a bridge or the voice page does, and the in-memory
//! chat service records what actually went out — so "nothing was posted" is an observation, not
//! an inference from a status code.
//!
//! What the gate LOGS is tested in `tests/logging.rs`, beside every other access-log assertion:
//! a log capture is per thread, and it is reliable only in a binary where every test holds one.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::http::router;
use vibe_talk::model::ChannelId;
use vibe_talk::post_gate::PostGate;
use vibe_talk::testing::{READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};

struct Harness {
    router: axum::Router,
    discord: Arc<FakeDiscord>,
}

fn harness() -> Harness {
    harness_with(PostGate::new())
}

fn harness_with(gate: PostGate) -> Harness {
    let (mut state, discord) = vibe_talk::testing::state();
    state.post_gate = Arc::new(gate);
    Harness {
        router: router(state),
        discord,
    }
}

async fn send(
    harness: &Harness,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    match body {
        Some(json) => {
            let json = json.to_string();
            send_raw(
                harness,
                method,
                uri,
                Some(token),
                Some("application/json"),
                &json,
            )
            .await
        }
        None => send_raw(harness, method, uri, Some(token), None, "").await,
    }
}

/// The model's side: `post_reply` over MCP. Returns the tool result's text.
async fn post_reply(harness: &Harness, token: &str, arguments: Value) -> (StatusCode, Value) {
    send(
        harness,
        "POST",
        "/mcp",
        token,
        Some(json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": { "name": "post_reply", "arguments": arguments },
        })),
    )
    .await
}

fn tool_text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The application's side: what is waiting for the speaker, handle included.
async fn pending(harness: &Harness) -> Value {
    let (status, body) = send(harness, "GET", "/api/v1/post-proposals", WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["proposal"].clone()
}

/// The commit body for `proposal` exactly as proposed.
fn restated(proposal: &Value) -> Value {
    json!({
        "handle": proposal["handle"],
        "channel_id": proposal["channel_id"],
        "text": proposal["text"],
        "reply_to": proposal["reply_to"],
        "confirmed_by": "ui",
    })
}

async fn commit(harness: &Harness, body: Value) -> (StatusCode, Value) {
    send(
        harness,
        "POST",
        "/api/v1/post-proposals/commit",
        WRITE_TOKEN,
        Some(body),
    )
    .await
}

/// Propose over MCP with the write token, check nothing went out, and return the pending proposal.
async fn propose(harness: &Harness, text: &str) -> Value {
    let (status, response) = post_reply(
        harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": text }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert!(tool_text(&response).starts_with("NOT SENT."), "{response}");
    assert!(
        harness.discord.posted().is_empty(),
        "a proposal posted: {:?}",
        harness.discord.posted()
    );
    let proposal = pending(harness).await;
    assert_eq!(proposal["text"], text, "{proposal}");
    proposal
}

#[tokio::test]
async fn a_write_scope_post_reply_without_confirmation_posts_nothing() {
    let harness = harness();
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "ship it" }),
    )
    .await;
    let said = tool_text(&response);
    assert!(said.starts_with("NOT SENT."), "{said}");
    assert!(said.contains("Exact text: ship it"), "{said}");
    // Asking again is not confirming: it replaces the proposal, and still nothing goes out.
    let (_status, again) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "ship it" }),
    )
    .await;
    assert!(tool_text(&again).starts_with("NOT SENT."), "{again}");
    assert!(
        harness.discord.posted().is_empty(),
        "{:?}",
        harness.discord.posted()
    );
    // The model is never handed the handle that would send it.
    let proposal = pending(&harness).await;
    let handle = proposal["handle"].as_str().expect("a handle");
    assert_eq!(handle.len(), 22, "{proposal}");
    assert!(!said.contains(handle) && !tool_text(&again).contains(handle));
    assert_eq!(proposal["channel_id"], WRITE_CHANNEL);
    assert_eq!(proposal["channel_name"], "lead team");
    assert!(proposal["expires_in_ms"].as_u64().expect("a number") > 0);
}

#[tokio::test]
async fn a_confirmed_handle_posts_exactly_once() {
    let harness = harness();
    let proposal = propose(&harness, "on my way").await;
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["serial"], proposal["serial"]);
    assert_eq!(body["posted"]["content"], "on my way");
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "proposal_used");
    let posted = harness.discord.posted();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].channel.as_str(), WRITE_CHANNEL);
    assert_eq!(posted[0].content, "on my way");
    assert_eq!(pending(&harness).await, Value::Null);
}

#[tokio::test]
async fn a_confirmed_post_that_fails_to_send_still_spends_the_handle() {
    // The confirmation was for one attempt. A failure is reported and the model can propose again;
    // the old handle does not become a standing licence to retry.
    let harness = harness();
    let proposal = propose(&harness, "on my way").await;
    harness.discord.fail_next("the chat service is down");
    let (status, body) = commit(&harness, restated(&proposal)).await;
    // The send path reports a failed first part as a partial send of nothing.
    assert_eq!(status, StatusCode::MULTI_STATUS, "{body}");
    assert_eq!(body["error"], "partially_posted", "{body}");
    assert_eq!(body["posted"], 0, "{body}");
    assert!(harness.discord.posted().is_empty());
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "proposal_used");
    assert!(harness.discord.posted().is_empty());
    assert_eq!(pending(&harness).await, Value::Null);
}

#[tokio::test]
async fn the_reply_target_is_part_of_what_is_confirmed_and_what_is_posted() {
    let harness = harness();
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    harness
        .discord
        .seed(&channel, "alice", "who has the runner");
    let (_status, window) = send(
        &harness,
        "GET",
        &format!("/api/v1/channels/{WRITE_CHANNEL}/messages"),
        READ_TOKEN,
        None,
    )
    .await;
    let target = window["messages"][0]["id"]
        .as_str()
        .expect("the seeded message has an id")
        .to_owned();
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "me", "reply_to": target }),
    )
    .await;
    assert!(tool_text(&response).contains(&target), "{response}");
    let proposal = pending(&harness).await;
    assert_eq!(proposal["reply_to"], target.as_str());
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let posted = harness.discord.posted();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(
        posted[0].reply_to.as_ref().map(|id| id.as_str()),
        Some(target.as_str())
    );
}

#[tokio::test]
async fn an_edited_restatement_is_refused_and_spends_the_handle() {
    for (field, edited) in [
        ("text", json!("on my way!")),
        ("channel_id", json!(READ_CHANNEL)),
        ("reply_to", json!("300")),
    ] {
        let harness = harness();
        let proposal = propose(&harness, "on my way").await;
        let mut body = restated(&proposal);
        body[field] = edited;
        let (status, refused) = commit(&harness, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{field}: {refused}");
        assert_eq!(refused["error"], "proposal_mismatch", "{field}");
        // Guessing what changed cannot turn a refusal into a send: the handle is gone.
        let (status, retried) = commit(&harness, restated(&proposal)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{field}: {retried}");
        assert_eq!(retried["error"], "proposal_used", "{field}");
        assert!(
            harness.discord.posted().is_empty(),
            "{field}: {:?}",
            harness.discord.posted()
        );
    }
}

#[tokio::test]
async fn an_expired_handle_is_refused() {
    let harness = harness_with(PostGate::with_ttl(Duration::from_millis(150)));
    let proposal = propose(&harness, "on my way").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(pending(&harness).await, Value::Null);
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(body["error"], "proposal_expired");
    assert!(harness.discord.posted().is_empty());
}

#[tokio::test]
async fn a_superseded_handle_is_refused_and_only_the_current_draft_can_be_sent() {
    let harness = harness();
    let first = propose(&harness, "on my way").await;
    let second = propose(&harness, "running late").await;
    assert_ne!(first["handle"], second["handle"]);
    let (status, body) = commit(&harness, restated(&first)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "proposal_superseded");
    assert!(harness.discord.posted().is_empty());
    let (status, body) = commit(&harness, restated(&second)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let posted = harness.discord.posted();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].content, "running late");
}

#[tokio::test]
async fn a_cancelled_handle_is_refused() {
    let harness = harness();
    let proposal = propose(&harness, "on my way").await;
    let (status, body) = send(
        &harness,
        "POST",
        "/api/v1/post-proposals/cancel",
        WRITE_TOKEN,
        Some(json!({ "handle": proposal["handle"] })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(pending(&harness).await, Value::Null);
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "proposal_used");
    assert!(harness.discord.posted().is_empty());
}

#[tokio::test]
async fn an_unknown_handle_is_refused_and_disturbs_nothing() {
    let harness = harness();
    let proposal = propose(&harness, "on my way").await;
    let mut forged = restated(&proposal);
    forged["handle"] = json!("AAAAAAAAAAAAAAAAAAAAAA");
    let (status, body) = commit(&harness, forged).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "proposal_unknown");
    assert!(harness.discord.posted().is_empty());
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(harness.discord.posted().len(), 1);
}

#[tokio::test]
async fn a_commit_must_name_a_confirmation_the_server_knows() {
    let harness = harness();
    let proposal = propose(&harness, "on my way").await;
    for confirmed_by in [json!("model"), json!(null), json!("")] {
        let mut body = restated(&proposal);
        body["confirmed_by"] = confirmed_by.clone();
        let (status, _body) = commit(&harness, body).await;
        assert!(
            status.is_client_error(),
            "confirmed_by {confirmed_by} answered {status}"
        );
    }
    assert!(harness.discord.posted().is_empty());
    let mut body = restated(&proposal);
    body["confirmed_by"] = json!("speaker_turn");
    let (status, body) = commit(&harness, body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(harness.discord.posted().len(), 1);
}

#[tokio::test]
async fn concurrent_commits_of_one_handle_post_once() {
    let harness = Arc::new(harness());
    let proposal = propose(&harness, "on my way").await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let harness = Arc::clone(&harness);
        let body = restated(&proposal);
        tasks.push(tokio::spawn(async move { commit(&harness, body).await.0 }));
    }
    let mut ok = 0;
    for task in tasks {
        if task.await.expect("task") == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(harness.discord.posted().len(), 1);
}

#[tokio::test]
async fn the_rest_propose_route_proposes_and_posts_nothing() {
    // The route `post_reply` names as its backing route in the manifest, for a consumer that maps
    // tools onto REST rather than speaking MCP. It must land on the gate too.
    let harness = harness();
    let (status, body) = send(
        &harness,
        "POST",
        "/api/v1/post-proposals",
        WRITE_TOKEN,
        Some(json!({ "channel_id": WRITE_CHANNEL, "text": "on my way" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sent"], false, "{body}");
    let result = body["result"]
        .as_str()
        .expect("a result sentence for the model");
    assert!(result.starts_with("NOT SENT."), "{result}");
    assert!(result.contains("Exact text: on my way"), "{result}");
    assert!(harness.discord.posted().is_empty());
    // The answer may be relayed to a model, so it must not carry the capability that commits.
    let (_, pending) = send(&harness, "GET", "/api/v1/post-proposals", WRITE_TOKEN, None).await;
    let handle = pending["proposal"]["handle"]
        .as_str()
        .expect("a pending proposal");
    assert_eq!(pending["proposal"]["serial"], body["serial"]);
    assert!(
        !body.to_string().contains(handle),
        "the proposing answer carried the handle: {body}"
    );
    let (status, body) = send(
        &harness,
        "POST",
        "/api/v1/post-proposals",
        WRITE_TOKEN,
        Some(json!({ "channel_id": READ_CHANNEL, "text": "on my way" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "channel_not_writable");
}

#[tokio::test]
async fn the_long_poll_wakes_for_a_proposal_and_for_its_end() {
    let harness = Arc::new(harness());
    let waiter = {
        let harness = Arc::clone(&harness);
        tokio::spawn(async move {
            send(
                &harness,
                "GET",
                "/api/v1/post-proposals?wait=20",
                WRITE_TOKEN,
                None,
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let proposal = propose(&harness, "on my way").await;
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the wait ended when a proposal arrived")
        .expect("task");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["proposal"]["serial"], proposal["serial"]);

    let serial = proposal["serial"].as_u64().expect("a serial");
    let waiter = {
        let harness = Arc::clone(&harness);
        tokio::spawn(async move {
            send(
                &harness,
                "GET",
                &format!("/api/v1/post-proposals?wait=20&seen={serial}"),
                WRITE_TOKEN,
                None,
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (status, _body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the wait ended when the proposal was sent")
        .expect("task");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["proposal"], Value::Null);
}

#[tokio::test]
async fn the_read_token_can_neither_propose_see_nor_confirm() {
    let harness = harness();
    let proposal = propose(&harness, "on my way").await;
    let (status, body) = post_reply(
        &harness,
        READ_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "hi" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    for (method, uri, body) in [
        ("GET", "/api/v1/post-proposals", None),
        (
            "POST",
            "/api/v1/post-proposals",
            Some(json!({ "channel_id": WRITE_CHANNEL, "text": "hi" })),
        ),
        (
            "POST",
            "/api/v1/post-proposals/commit",
            Some(restated(&proposal)),
        ),
        (
            "POST",
            "/api/v1/post-proposals/cancel",
            Some(json!({ "handle": proposal["handle"] })),
        ),
    ] {
        let (status, answer) = send(&harness, method, uri, READ_TOKEN, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {answer}");
        assert!(
            !answer.to_string().contains("on my way"),
            "{method} {uri} disclosed the draft: {answer}"
        );
    }
    assert!(harness.discord.posted().is_empty());
    // The refusals left the proposal where it was.
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A request as a careless or hostile caller might send it: any token or none, any content type or
/// none, and a body that need not be JSON at all.
async fn send_raw(
    harness: &Harness,
    method: &str,
    uri: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("accept", "application/json, text/event-stream");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let request = builder.body(Body::from(body.to_owned())).expect("request");
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
async fn a_caller_without_write_scope_is_refused_before_its_request_is_parsed() {
    // `#41 post-gate-scope-first`. Every route that proposes, confirms, withdraws, reads a
    // proposal, or posts must answer a read token 403 and an anonymous caller 401 — the SAME
    // answer a well-formed request gets — however malformed the request is. Parsing first used to
    // answer 422 or 400 instead, which told a caller with no business here what the route accepts.
    let harness = harness();
    let proposal = propose(&harness, "on my way").await;
    let reply = format!("/api/v1/channels/{WRITE_CHANNEL}/reply");
    let ask = format!("/api/v1/channels/{WRITE_CHANNEL}/ask");
    let well_formed_proposal = json!({ "channel_id": WRITE_CHANNEL, "text": "hi" }).to_string();
    let well_formed_commit = restated(&proposal).to_string();
    let well_formed_cancel = json!({ "handle": proposal["handle"] }).to_string();
    let well_formed_reply = json!({ "text": "hi" }).to_string();
    let well_formed_ask = json!({ "question": "why?" }).to_string();
    // (method, route, a well-formed request, malformed variants of the same route)
    let routes: [(&str, &str, &str, &[&str]); 5] = [
        (
            "POST",
            "/api/v1/post-proposals",
            &well_formed_proposal,
            &["{}", "not json", "{\"text\": 7}"],
        ),
        (
            "POST",
            "/api/v1/post-proposals/commit",
            &well_formed_commit,
            &[
                "{}",
                "not json",
                "{\"handle\": \"x\", \"confirmed_by\": \"the_model\"}",
            ],
        ),
        (
            "POST",
            "/api/v1/post-proposals/cancel",
            &well_formed_cancel,
            &["{}", "[]"],
        ),
        ("POST", &reply, &well_formed_reply, &["{}", "not json"]),
        ("POST", &ask, &well_formed_ask, &["{}"]),
    ];
    for (token, refused) in [
        (Some(READ_TOKEN), StatusCode::FORBIDDEN),
        (None, StatusCode::UNAUTHORIZED),
    ] {
        for (method, uri, well_formed, malformed) in &routes {
            let expected = send_raw(
                &harness,
                method,
                uri,
                token,
                Some("application/json"),
                well_formed,
            )
            .await;
            assert_eq!(expected.0, refused, "{method} {uri}: {}", expected.1);
            let mut variants: Vec<(Option<&str>, &str)> = malformed
                .iter()
                .map(|body| (Some("application/json"), *body))
                .collect();
            // No content type, and the wrong one, are refused by `Json` too when it goes first.
            variants.push((None, well_formed));
            variants.push((Some("text/plain"), well_formed));
            for (content_type, body) in variants {
                let answer = send_raw(&harness, method, uri, token, content_type, body).await;
                assert_eq!(
                    answer, expected,
                    "{method} {uri} with {content_type:?} {body:?} was not refused like a \
                     well-formed request"
                );
            }
        }
        // So is a channel id that does not decode: `Path` refuses `%FF` with 400 when it goes first.
        for (uri, well_formed) in [
            ("/api/v1/channels/%FF/reply", &well_formed_reply),
            ("/api/v1/channels/%FF/ask", &well_formed_ask),
        ] {
            let decodable = uri.replace("%FF", WRITE_CHANNEL);
            let expected = send_raw(
                &harness,
                "POST",
                &decodable,
                token,
                Some("application/json"),
                well_formed,
            )
            .await;
            let answer = send_raw(
                &harness,
                "POST",
                uri,
                token,
                Some("application/json"),
                well_formed,
            )
            .await;
            assert_eq!(
                answer, expected,
                "POST {uri} was not refused like {decodable}"
            );
        }
        // The long poll's query is parsed too, and must not be first either.
        for uri in [
            "/api/v1/post-proposals",
            "/api/v1/post-proposals?wait=soon",
            "/api/v1/post-proposals?wait=5&seen=latest",
        ] {
            let (status, answer) = send_raw(&harness, "GET", uri, token, None, "").await;
            assert_eq!(status, refused, "GET {uri}: {answer}");
            assert!(
                !answer.to_string().contains("on my way"),
                "GET {uri}: {answer}"
            );
        }
    }
    assert!(harness.discord.posted().is_empty());

    // The write token still gets the parser's answer, so the check did not simply swallow
    // malformed requests: they are refused for what they are, and nothing is spent or posted.
    // Exact statuses, so a mistyped route answering 404 or 405 cannot pass for a parser's refusal.
    for uri in [
        "/api/v1/post-proposals",
        "/api/v1/post-proposals/commit",
        "/api/v1/post-proposals/cancel",
        reply.as_str(),
        ask.as_str(),
    ] {
        for (body, parsed) in [
            ("{}", StatusCode::UNPROCESSABLE_ENTITY),
            ("not json", StatusCode::BAD_REQUEST),
        ] {
            let (status, answer) = send_raw(
                &harness,
                "POST",
                uri,
                Some(WRITE_TOKEN),
                Some("application/json"),
                body,
            )
            .await;
            assert_eq!(
                status, parsed,
                "POST {uri} {body:?} with the write token: {answer}"
            );
        }
    }
    let (status, answer) = send_raw(
        &harness,
        "GET",
        "/api/v1/post-proposals?wait=soon",
        Some(WRITE_TOKEN),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(harness.discord.posted().is_empty());
    let (status, body) = commit(&harness, restated(&proposal)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the refusals disturbed the proposal: {body}"
    );
}

#[tokio::test]
async fn the_read_scope_tool_list_is_unchanged() {
    // Pinned by name and order, so the gate cannot have added a tool a read-scope bridge sees, or
    // taken one away. The descriptions are pinned against `post_reply` and the gate's routes.
    let harness = harness();
    let (status, body) = send(
        &harness,
        "POST",
        "/mcp",
        READ_TOKEN,
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tools = body["result"]["tools"].as_array().expect("a tool array");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("named"))
        .collect();
    assert_eq!(
        names,
        [
            "recent_activity",
            "list_channels",
            "digest_channel",
            "read_page",
            "count_messages",
            "find_message",
            "read_message",
        ]
    );
    let listing = body.to_string();
    for absent in ["post_reply", "post-proposals", "commit", "proposal"] {
        assert!(!listing.contains(absent), "{absent}: {listing}");
    }
}
