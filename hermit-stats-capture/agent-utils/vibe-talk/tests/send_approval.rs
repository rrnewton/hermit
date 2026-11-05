//! `#264 voice-send-approval`, end to end through the real router.
//!
//! "Ask me before the voice agent sends" is the owner's switch over what `post_reply` does. On —
//! the shipped default — it proposes and posts nothing (`tests/post_gate.rs` covers that flow in
//! full, and must stay green unchanged). Off, it posts at once through the same send path a
//! confirmed proposal takes, and the model is told SENT, or why not. These drive the HTTP stack a
//! bridge or the voice page does, and the in-memory chat service records what actually went out,
//! so "posted once" is an observation rather than an inference from a status code.
//!
//! What the direct send LOGS is tested in `tests/logging.rs`, beside every other access-log check.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::http::router;
use vibe_talk::store::fake::FakeStore;
use vibe_talk::testing::{READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};

struct Harness {
    router: axum::Router,
    discord: Arc<FakeDiscord>,
    state: vibe_talk::state::AppState,
}

fn harness() -> Harness {
    let (state, discord, _store) = vibe_talk::testing::state_with_store();
    Harness {
        router: router(state.clone()),
        discord,
        state,
    }
}

/// A server whose owner has already turned confirmation off.
async fn sending_at_once() -> Harness {
    let harness = harness();
    let (status, body) =
        settings_put(&harness, WRITE_TOKEN, json!({ "ask_before_send": false })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "ask_before_send": false, "changeable": true })
    );
    harness
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

async fn settings_put(harness: &Harness, token: &str, body: Value) -> (StatusCode, Value) {
    send(
        harness,
        "PUT",
        "/api/v1/voice-settings",
        Some(token),
        Some(body),
    )
    .await
}

async fn rpc(harness: &Harness, token: &str, method: &str, params: Value) -> (StatusCode, Value) {
    send(
        harness,
        "POST",
        "/mcp",
        Some(token),
        Some(json!({ "jsonrpc": "2.0", "id": 3, "method": method, "params": params })),
    )
    .await
}

async fn post_reply(harness: &Harness, token: &str, arguments: Value) -> (StatusCode, Value) {
    rpc(
        harness,
        token,
        "tools/call",
        json!({ "name": "post_reply", "arguments": arguments }),
    )
    .await
}

fn tool_text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn listed_post_tool(harness: &Harness) -> Value {
    let (status, body) = rpc(harness, WRITE_TOKEN, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "post_reply")
        .cloned()
        .expect("post_reply is listed to the write token")
}

async fn profile(harness: &Harness, token: &str) -> Value {
    let (status, body) = send(harness, "GET", "/api/v1/voice-agent", Some(token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn post_capability(profile: &Value) -> Value {
    profile["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "post_reply")
        .cloned()
        .expect("post_reply described")
}

async fn nothing_pending(harness: &Harness) {
    let (status, body) = send(
        harness,
        "GET",
        "/api/v1/post-proposals",
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["proposal"],
        Value::Null,
        "a direct send left a proposal: {body}"
    );
}

#[tokio::test]
async fn with_confirmation_off_post_reply_posts_once_and_says_sent_with_what_and_where() {
    let harness = sending_at_once().await;
    let (status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "rebase onto main", "reply_to": "1000" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["result"]["isError"], false, "{response}");
    let said = tool_text(&response);
    assert!(
        said.starts_with(&format!(
            "SENT. Posted to lead team (id {WRITE_CHANNEL}) as a reply to message 1000"
        )),
        "{said}"
    );
    assert!(
        said.contains("Exact text posted: rebase onto main"),
        "{said}"
    );
    assert!(said.contains("Do not call post_reply again"), "{said}");
    let posted = harness.discord.posted();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].channel.as_str(), WRITE_CHANNEL);
    assert_eq!(posted[0].content, "rebase onto main");
    assert_eq!(
        posted[0].reply_to.as_ref().map(|id| id.as_str()),
        Some("1000")
    );
    nothing_pending(&harness).await;
}

#[tokio::test]
async fn a_long_direct_send_is_split_by_the_shared_send_path_and_says_so() {
    let harness = sending_at_once().await;
    let words: Vec<String> = (0..900).map(|i| format!("word{i}")).collect();
    let text = words.join(" ");
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": text }),
    )
    .await;
    let said = tool_text(&response);
    assert_eq!(response["result"]["isError"], false, "{said}");
    let posted = harness.discord.posted();
    assert!(
        posted.len() > 1,
        "a long message went as one part: {}",
        posted.len()
    );
    assert!(
        said.contains(&format!(
            "It was long, so it went as {} messages.",
            posted.len()
        )),
        "{said}"
    );
    // Whatever the cut, every word went, once, in order.
    let rejoined: Vec<String> = posted
        .iter()
        .flat_map(|part| {
            part.content
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(rejoined, words);
}

/// Nine hundred words: long enough to go as more than one message.
fn long_text() -> String {
    (0..900)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[tokio::test]
async fn a_direct_send_that_fails_says_not_sent_and_one_that_fails_partway_says_partly_sent() {
    // A refusal from the chat service is an answer: certainly not posted.
    let harness = sending_at_once().await;
    harness
        .discord
        .refuse_post_after(0, 403, "Missing Permissions");
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "on it" }),
    )
    .await;
    assert_eq!(response["result"]["isError"], true, "{response}");
    let said = tool_text(&response);
    assert!(said.starts_with("NOT SENT. "), "{said}");
    assert!(harness.discord.posted().is_empty());

    harness
        .discord
        .refuse_post_after(1, 403, "Missing Permissions");
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": long_text() }),
    )
    .await;
    assert_eq!(response["result"]["isError"], true, "{response}");
    let said = tool_text(&response);
    assert!(
        said.starts_with(
            "PARTLY SENT. The rest was NOT sent; do not send the whole message again."
        ),
        "{said}"
    );
    assert_eq!(harness.discord.posted().len(), 1);
}

#[tokio::test]
async fn a_direct_send_whose_answer_was_lost_is_not_confirmed_and_never_called_not_sent() {
    // `#264 voice-send-approval`. A timeout or a dropped connection may come after the chat
    // service took the post. "NOT SENT" would have the agent tell the owner so, and his "try
    // again" would post it twice.
    let harness = sending_at_once().await;
    harness.discord.fail_next("operation timed out");
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "on it" }),
    )
    .await;
    assert_eq!(response["result"]["isError"], true, "{response}");
    let said = tool_text(&response);
    assert!(said.starts_with("NOT CONFIRMED. "), "{said}");
    assert!(
        said.contains("do not call post_reply again for these words"),
        "{said}"
    );
    assert!(!said.contains("NOT SENT"), "{said}");

    // Partway, the part whose answer was lost is not called unsent either.
    harness
        .discord
        .fail_post_after(1, "connection reset by peer");
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": long_text() }),
    )
    .await;
    let said = tool_text(&response);
    assert!(said.starts_with("PARTLY SENT. "), "{said}");
    assert!(!said.contains("The rest was NOT sent"), "{said}");
}

#[tokio::test]
async fn the_same_direct_send_again_is_answered_from_the_first_and_posts_nothing_more() {
    // `#264 voice-send-approval`. A commit cannot be repeated — it spends a single-use handle — so
    // a direct send must not be repeatable by a retried tool call or a "try again" either.
    let harness = sending_at_once().await;
    let words = json!({ "channel_id": WRITE_CHANNEL, "text": "rebase onto main" });
    let (_status, first) = post_reply(&harness, WRITE_TOKEN, words.clone()).await;
    assert!(tool_text(&first).starts_with("SENT. "), "{first}");
    let (status, again) = post_reply(&harness, WRITE_TOKEN, words.clone()).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["result"]["isError"], false, "{again}");
    let said = tool_text(&again);
    assert!(said.starts_with("SENT already. "), "{said}");
    assert!(said.contains("lead team"), "{said}");
    assert!(said.contains("posted nothing more"), "{said}");
    assert!(
        said.contains("Exact text posted: rebase onto main"),
        "{said}"
    );
    assert_eq!(
        harness.discord.posted().len(),
        1,
        "the same words were posted twice"
    );

    // Through the REST route a bridge maps the tool onto: the first send's answer, replayed.
    let (status, body) = send(
        &harness,
        "POST",
        "/api/v1/post-proposals",
        Some(WRITE_TOKEN),
        Some(words.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sent"], true, "{body}");
    assert!(
        body["result"]
            .as_str()
            .expect("text")
            .starts_with("SENT already."),
        "{body}"
    );
    assert_eq!(harness.discord.posted().len(), 1);

    // Other words, or the same words as a reply, are other posts.
    let (_status, other) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "rebase onto main", "reply_to": "1000" }),
    )
    .await;
    assert!(tool_text(&other).starts_with("SENT. "), "{other}");
    assert_eq!(harness.discord.posted().len(), 2);
}

#[tokio::test]
async fn a_lost_answer_is_not_posted_over_and_a_refusal_can_be_tried_again() {
    let harness = sending_at_once().await;
    let words = json!({ "channel_id": WRITE_CHANNEL, "text": "on it" });
    // The chat service took it; the answer never came back.
    harness.discord.lose_answer_after(0, "operation timed out");
    let (_status, first) = post_reply(&harness, WRITE_TOKEN, words.clone()).await;
    assert!(tool_text(&first).starts_with("NOT CONFIRMED. "), "{first}");
    assert_eq!(harness.discord.posted().len(), 1, "the post landed");
    // "Try again": the second call posts nothing, and says why.
    let (_status, again) = post_reply(&harness, WRITE_TOKEN, words.clone()).await;
    let said = tool_text(&again);
    assert!(said.starts_with("NOT CONFIRMED. "), "{said}");
    assert!(said.contains("posted nothing"), "{said}");
    assert!(said.contains("check the channel"), "{said}");
    assert_eq!(
        harness.discord.posted().len(),
        1,
        "the owner's try-again posted it twice"
    );
    // The REST route refuses it rather than calling it sent.
    let (status, body) = send(
        &harness,
        "POST",
        "/api/v1/post-proposals",
        Some(WRITE_TOKEN),
        Some(words),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "post_repeated", "{body}");
    assert_eq!(harness.discord.posted().len(), 1);

    // A refusal posted nothing, so the same words may go again at once.
    let refused = json!({ "channel_id": WRITE_CHANNEL, "text": "ship it" });
    harness
        .discord
        .refuse_post_after(0, 403, "Missing Permissions");
    let (_status, first) = post_reply(&harness, WRITE_TOKEN, refused.clone()).await;
    assert!(tool_text(&first).starts_with("NOT SENT. "), "{first}");
    let (_status, again) = post_reply(&harness, WRITE_TOKEN, refused).await;
    assert!(tool_text(&again).starts_with("SENT. "), "{again}");
    assert_eq!(harness.discord.posted().len(), 2);
}

#[tokio::test]
async fn with_confirmation_off_every_fence_still_holds() {
    let harness = sending_at_once().await;
    let (status, _body) = post_reply(
        &harness,
        READ_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "hi" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a read token posted");
    for (arguments, code) in [
        (
            json!({ "channel_id": READ_CHANNEL, "text": "hi" }),
            "channel_not_writable",
        ),
        (
            json!({ "channel_id": "9999999999", "text": "hi" }),
            "unknown_channel",
        ),
        (
            json!({ "channel_id": WRITE_CHANNEL, "text": "   " }),
            "refused",
        ),
    ] {
        let (_status, response) = post_reply(&harness, WRITE_TOKEN, arguments.clone()).await;
        assert_eq!(
            response["result"]["isError"], true,
            "{arguments}: {response}"
        );
        let said = tool_text(&response);
        assert!(
            said.starts_with(&format!("NOT SENT. {code}")),
            "{arguments}: {said}"
        );
    }
    assert!(
        harness.discord.posted().is_empty(),
        "{:?}",
        harness.discord.posted()
    );
}

#[tokio::test]
async fn with_confirmation_on_post_reply_still_only_proposes() {
    let harness = harness();
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "ship it" }),
    )
    .await;
    let said = tool_text(&response);
    assert!(said.starts_with("NOT SENT."), "{said}");
    assert!(said.contains("Send it?"), "{said}");
    // The agent is handed the finished read-back and told to hold on a yes, not to claim a send.
    assert!(said.contains("say only \"One moment.\""), "{said}");
    assert!(harness.discord.posted().is_empty());
    let (_status, body) = send(
        &harness,
        "GET",
        "/api/v1/post-proposals",
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    assert_eq!(body["proposal"]["text"], "ship it", "{body}");
    // The read-back is built from the same channel name and text the voice page's card shows, so
    // the page can hold what was said to exactly that sentence.
    let card = &body["proposal"];
    let expected = format!(
        "Read-back: To {}: {}. Send it?",
        card["channel_name"].as_str().expect("a channel name"),
        card["text"].as_str().expect("a text")
    );
    assert!(said.contains(&expected), "{expected:?} not in {said}");
}

#[tokio::test]
async fn tools_list_and_the_profile_follow_the_setting_as_soon_as_it_changes() {
    let harness = harness();
    let asking = listed_post_tool(&harness).await;
    assert!(
        asking["description"]
            .as_str()
            .expect("text")
            .contains("does NOT send"),
        "{asking}"
    );
    let write = profile(&harness, WRITE_TOKEN).await;
    assert_eq!(write["prompt"]["variant"], "write-confirm");
    assert_eq!(write["ask_before_send"], true);
    assert_eq!(post_capability(&write)["requires_confirmation"], true);

    let (status, _body) =
        settings_put(&harness, WRITE_TOKEN, json!({ "ask_before_send": false })).await;
    assert_eq!(status, StatusCode::OK);
    let direct = listed_post_tool(&harness).await;
    assert!(
        direct["description"]
            .as_str()
            .expect("text")
            .contains("SENDS IT IMMEDIATELY"),
        "{direct}"
    );
    let write = profile(&harness, WRITE_TOKEN).await;
    assert_eq!(write["prompt"]["variant"], "write-direct");
    assert_eq!(
        write["prompt"]["fingerprint"],
        vibe_talk::voice_agent::PINNED_WRITE_DIRECT_FINGERPRINT
    );
    assert_eq!(write["ask_before_send"], false);
    assert_eq!(post_capability(&write)["requires_confirmation"], false);
    // A read token gets the read prompt whatever the owner chose; it cannot send at all.
    let read = profile(&harness, READ_TOKEN).await;
    assert_eq!(read["prompt"]["variant"], "read");
    assert_eq!(post_capability(&read)["available"], false);
    // The documentation route agrees.
    let (_status, tools) = send(
        &harness,
        "GET",
        "/api/v1/agent-tools",
        Some(READ_TOKEN),
        None,
    )
    .await;
    let documented = tools["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "post_reply")
        .cloned()
        .expect("documented");
    assert_eq!(documented["approval"], "automatic", "{documented}");
    assert_eq!(documented["path"], "/api/v1/post-proposals");
}

#[tokio::test]
async fn the_rest_route_a_bridge_maps_the_tool_onto_sends_at_once_too() {
    let harness = sending_at_once().await;
    let (status, body) = send(
        &harness,
        "POST",
        "/api/v1/post-proposals",
        Some(WRITE_TOKEN),
        Some(json!({ "channel_id": WRITE_CHANNEL, "text": "on it" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sent"], true, "{body}");
    assert!(
        body["result"].as_str().expect("text").starts_with("SENT."),
        "{body}"
    );
    assert_eq!(harness.discord.posted().len(), 1);
    nothing_pending(&harness).await;
}

#[tokio::test]
async fn the_setting_is_read_at_read_scope_and_changed_only_at_write_scope() {
    let harness = harness();
    let (status, body) = send(
        &harness,
        "GET",
        "/api/v1/voice-settings",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "ask_before_send": true, "changeable": true }));

    let (status, _body) =
        settings_put(&harness, READ_TOKEN, json!({ "ask_before_send": false })).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token changed the setting"
    );
    let (status, _body) = send(
        &harness,
        "PUT",
        "/api/v1/voice-settings",
        None,
        Some(json!({ "ask_before_send": false })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _body) = settings_put(
        &harness,
        WRITE_TOKEN,
        json!({ "ask_before_send": false, "also": "this" }),
    )
    .await;
    assert!(
        status.is_client_error(),
        "an unknown field was accepted: {status}"
    );
    let (status, _body) =
        settings_put(&harness, WRITE_TOKEN, json!({ "ask_before_send": "no" })).await;
    assert!(
        status.is_client_error(),
        "a non-boolean was accepted: {status}"
    );
    // None of those changed anything.
    let (_status, body) = send(
        &harness,
        "GET",
        "/api/v1/voice-settings",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(body["ask_before_send"], true);

    let (status, body) =
        settings_put(&harness, WRITE_TOKEN, json!({ "ask_before_send": false })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for token in [READ_TOKEN, WRITE_TOKEN] {
        let (_status, config) =
            send(&harness, "GET", "/api/v1/client-config", Some(token), None).await;
        assert_eq!(
            config["voice_settings"],
            json!({ "ask_before_send": false, "changeable": true }),
            "{config}"
        );
    }
    assert!(!vibe_talk::send_approval::current(&harness.state)
        .await
        .asks());
}

#[tokio::test]
async fn a_server_with_no_store_refuses_the_change_and_keeps_asking() {
    let (state, discord) =
        vibe_talk::testing::state_with(Arc::new(vibe_talk::store::disabled::DisabledStore));
    let harness = Harness {
        router: router(state.clone()),
        discord,
        state,
    };
    let (status, body) =
        settings_put(&harness, WRITE_TOKEN, json!({ "ask_before_send": false })).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "storage_not_configured", "{body}");
    let (_status, config) = send(
        &harness,
        "GET",
        "/api/v1/client-config",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(
        config["voice_settings"],
        json!({ "ask_before_send": true, "changeable": false })
    );
    let (_status, response) = post_reply(
        &harness,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "on it" }),
    )
    .await;
    assert!(tool_text(&response).starts_with("NOT SENT."), "{response}");
    assert!(harness.discord.posted().is_empty());
}

#[tokio::test]
async fn the_owners_choice_survives_a_restart_of_the_server() {
    // The same store under a fresh router and state, which is what a restart is to everything
    // above the store.
    let store = Arc::new(FakeStore::new());
    let (first, _discord) = vibe_talk::testing::state_with(store.clone());
    let first = Harness {
        router: router(first.clone()),
        discord: Arc::new(FakeDiscord::new()),
        state: first,
    };
    let (status, _body) =
        settings_put(&first, WRITE_TOKEN, json!({ "ask_before_send": false })).await;
    assert_eq!(status, StatusCode::OK);
    drop(first);
    let (second, discord) = vibe_talk::testing::state_with(store);
    let second = Harness {
        router: router(second.clone()),
        discord,
        state: second,
    };
    let (_status, body) = send(
        &second,
        "GET",
        "/api/v1/voice-settings",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(body["ask_before_send"], false, "{body}");
    let (_status, response) = post_reply(
        &second,
        WRITE_TOKEN,
        json!({ "channel_id": WRITE_CHANNEL, "text": "still on" }),
    )
    .await;
    assert!(tool_text(&response).starts_with("SENT."), "{response}");
    assert_eq!(second.discord.posted().len(), 1);
}
