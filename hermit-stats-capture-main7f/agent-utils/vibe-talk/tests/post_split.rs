//! `#263 post-split-per-provider`: a long post is cut at its own provider's limit, not Discord's,
//! and where a provider continues a long message in a thread the rest goes under the first part.
//!
//! The owner sent a long message to Google Chat and it arrived as three: every post was cut at
//! Discord's 2000 characters whatever the provider, and the parts were posted one after another.
//! What he asked for instead, if it had to be cut at all: one message in the conversation, and the
//! rest as replies in its thread.
//!
//! Every test here drives the real HTTP route through the real provider client, against a loopback
//! fake of the provider: a Google Chat bridge speaking Discord's HTTP API, and the Slack Web API.
//! Discord itself is the in-memory fake every other API test uses.

mod slack_fake;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use slack_fake::{profiled, ts, Fake, FakeState, TOKEN};
use tower::ServiceExt as _;
use vibe_talk::discord::http::HttpDiscordClient;
use vibe_talk::model::ChannelId;
use vibe_talk::providers::ChatRouter;
use vibe_talk::state::AppState;
use vibe_talk::testing::{self, WRITE_CHANNEL, WRITE_TOKEN};

/// One request to the server, as the page makes it, with the answer's status and JSON body.
async fn call(state: &AppState, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {WRITE_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = vibe_talk::http::router(state.clone())
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

/// Numbered paragraphs of ordinary prose, `bytes` long or a little over, so there are real places
/// to cut and no two parts are the same words (a repeated part is never offered as resumable).
fn prose(bytes: usize) -> String {
    let mut text = String::new();
    let mut run = 0;
    while text.len() < bytes {
        run += 1;
        text.push_str(&format!(
            "Run {run}: the nightly build went red on the arm64 runner, and the retry went green. \
             Nobody changed the toolchain, so this looks like the flaky disk again.\n\n"
        ));
    }
    text
}

/// A 207's body without its `unsent` text, which is long, for an assertion to print.
fn brief(answer: &Value) -> Value {
    let mut answer = answer.clone();
    if let Some(fields) = answer.as_object_mut() {
        fields.remove("unsent");
    }
    answer
}

// --- Google Chat, through a bridge speaking Discord's HTTP API ------------------------------------

/// Everything the bridge was asked to post: the path, and the JSON body.
type Posts = Arc<Mutex<Vec<(String, Value)>>>;

const MAIN: &str = "/channels/2222222222/messages";

fn in_thread(thread: &str) -> String {
    format!("/channels/2222222222/threads/{thread}/messages")
}

/// vibe-talk in front of a Google Chat bridge that takes `max_post_bytes` in one message.
///
/// As Google Chat does, the bridge puts every message posted to the conversation in a thread of
/// its own — named here `t_` and the message's id — and every message posted to a thread in that
/// thread. It records every post, and answers the ones numbered in `fail` (from 1, across the
/// test) with the 502 of a send that timed out with nobody knowing whether it went out.
async fn google_chat(
    max_post_bytes: usize,
    fail: &'static [usize],
) -> (AppState, Posts, tokio::task::JoinHandle<()>) {
    let posts: Posts = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let seen = Arc::clone(&posts);
    let bridge =
        axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
            let seen = Arc::clone(&seen);
            async move {
                let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let content = value["content"].as_str().unwrap_or_default().to_owned();
                let path = uri.path().to_owned();
                let count = {
                    let mut seen = seen.lock().expect("posts");
                    seen.push((path.clone(), value));
                    seen.len()
                };
                if fail.contains(&count) {
                    return (
                        StatusCode::BAD_GATEWAY,
                        r#"{"message":"Google Chat message send failed"}"#.to_owned(),
                    );
                }
                let id = format!("9194{count:04}");
                let thread = match path.split("/threads/").nth(1) {
                    Some(rest) => json!({
                        "id": rest.split('/').next().unwrap_or_default(), "root_message_id": null,
                        "is_root": false, "reply_count": null, "reply_count_exact": false,
                    }),
                    None => json!({
                        "id": format!("t_{id}"), "root_message_id": id, "is_root": true,
                        "reply_count": null, "reply_count_exact": false,
                    }),
                };
                let message = json!({
                    "id": id, "channel_id": WRITE_CHANNEL, "content": content,
                    "timestamp": "2026-10-12T10:20:00Z", "thread": thread,
                    "author": {"id": "7", "username": "bridge", "bot": true},
                });
                (StatusCode::OK, message.to_string())
            }
        });
    let task = tokio::spawn(async move { axum::serve(listener, bridge).await.expect("serve") });
    let mut config = testing::config();
    let discord = config.discord_mut().expect("discord provider");
    discord.api_base = base;
    discord.provider_name = "Google Chat".to_owned();
    discord.markup = vibe_talk::render::Markup::GoogleChat;
    discord.channel_registration = true;
    discord.thread_api = vibe_talk::threads::ThreadApi::Bridge;
    discord.max_post_bytes = Some(max_post_bytes);
    let (mut state, _) = testing::state();
    state.replace_chat(Arc::new(HttpDiscordClient::new(discord).expect("client")));
    (state, posts, task)
}

fn reply_uri() -> String {
    format!("/api/v1/channels/{WRITE_CHANNEL}/reply")
}

#[tokio::test]
async fn a_message_google_chat_takes_whole_is_posted_as_one_message() {
    // The owner's message: three posts when every provider was cut at Discord's 2000 characters.
    let (state, posts, task) = google_chat(30_000, &[]).await;
    let text = prose(5_500);
    assert!(
        text.chars().count() > 4_000,
        "the fixture would not split at 2000"
    );
    let (status, answer) = call(&state, &reply_uri(), json!({ "text": text })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["parts"].as_array().map(Vec::len),
        Some(1),
        "{answer}"
    );
    let posts = posts.lock().expect("posts");
    assert_eq!(posts.len(), 1, "a message that fits was posted as several");
    assert_eq!(posts[0].0, MAIN);
    assert_eq!(
        posts[0].1["content"],
        text.as_str(),
        "the one post is not the text as typed"
    );
    task.abort();
}

#[tokio::test]
async fn a_message_over_google_chats_limit_is_one_message_with_the_rest_in_its_thread() {
    let (state, posts, task) = google_chat(600, &[]).await;
    let text = prose(2_400);
    let (status, answer) = call(
        &state,
        &reply_uri(),
        json!({ "text": text, "reply_to": "9100000000000000001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let parts = answer["parts"].as_array().expect("parts");
    assert!(
        parts.len() > 3,
        "the fixture did not split: {}",
        parts.len()
    );

    let posts = posts.lock().expect("posts");
    assert_eq!(posts.len(), parts.len());
    assert_eq!(
        posts[0].0, MAIN,
        "the first part did not go to the conversation"
    );
    for (path, _) in &posts[1..] {
        assert_eq!(
            path,
            &in_thread("t_91940001"),
            "a later part was not posted in the first part's thread"
        );
    }
    // Only the first part answers the message; the rest continue it.
    assert_eq!(
        posts[0].1["message_reference"]["message_id"],
        "9100000000000000001"
    );
    assert!(posts[1..]
        .iter()
        .all(|(_, body)| body.get("message_reference").is_none()));
    // Nothing typed was lost, and every part is within what Google Chat was said to take.
    let rejoined: String = posts
        .iter()
        .map(|(_, body)| body["content"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(rejoined, text);
    assert!(posts
        .iter()
        .all(|(_, body)| body["content"].as_str().unwrap_or_default().len() <= 600));
    // What the page draws at once: the first part as the thread's root, the rest inside it.
    assert_eq!(parts[0]["thread"]["is_root"], true, "{}", parts[0]);
    assert!(parts[1..]
        .iter()
        .all(|part| part["thread"]["id"] == "t_91940001" && part["thread"]["is_root"] == false));
    task.abort();
}

#[tokio::test]
async fn a_long_post_into_a_thread_keeps_every_part_in_that_thread() {
    let (state, posts, task) = google_chat(600, &[]).await;
    let text = prose(2_000);
    let (status, answer) = call(
        &state,
        &reply_uri(),
        json!({ "text": text, "thread_id": "t_77" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let posts = posts.lock().expect("posts");
    assert!(posts.len() > 2, "the fixture did not split");
    assert!(
        posts.iter().all(|(path, _)| path == &in_thread("t_77")),
        "a part left the thread it was sent to: {:?}",
        posts.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    task.abort();
}

/// `#195 send-resilience`, continued in a thread. Part 1 posts, part 2's answer is lost. The 207
/// names the thread the rest belongs in, and the remainder sent there — without the reply
/// target, as the part that failed was — asks for that part again as the same request, under
/// the same nonce, and for nothing already posted.
#[tokio::test]
async fn the_rest_of_a_continued_post_is_resumed_in_the_first_parts_thread() {
    for reply_to in [None, Some("9100000000000000001")] {
        let (state, posts, task) = google_chat(600, &[2]).await;
        let text = prose(2_400);
        let mut body = json!({ "text": text, "idempotency_key": "outgoing-key_263" });
        if let Some(target) = reply_to {
            body["reply_to"] = json!(target);
        }
        let (status, partial) = call(&state, &reply_uri(), body).await;
        let shown = brief(&partial);
        assert_eq!(status, StatusCode::MULTI_STATUS, "{shown}");
        assert_eq!(partial["posted"], 1, "{shown}");
        assert_eq!(partial["retryable"], true, "{shown}");
        assert_eq!(
            partial["thread_id"], "t_91940001",
            "the 207 did not say where the rest goes: {shown}"
        );
        assert_eq!(
            partial["resumable"], true,
            "a remainder that goes where the failed part went was left to the reader \
             ({reply_to:?}): {shown}"
        );

        // What the page sends next: the unsent text, to that thread, under the same key.
        let unsent = partial["unsent"].as_str().expect("unsent").to_owned();
        let (status, done) = call(
            &state,
            &reply_uri(),
            json!({
                "text": unsent, "thread_id": "t_91940001", "idempotency_key": "outgoing-key_263",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{done}");
        let posts = posts.lock().expect("posts");
        let (failed, again) = (&posts[1], &posts[2]);
        assert_eq!(failed.0, in_thread("t_91940001"));
        assert_eq!(again.0, failed.0, "the retry went somewhere else");
        assert_eq!(
            again.1["content"], failed.1["content"],
            "the retry cut the failed part differently"
        );
        assert_eq!(
            again.1["nonce"], failed.1["nonce"],
            "the retry asked under another request id"
        );
        assert!(
            posts[1..]
                .iter()
                .all(|(path, _)| path == &in_thread("t_91940001")),
            "the retry posted outside the thread"
        );
        assert_eq!(
            posts.iter().filter(|(path, _)| path == MAIN).count(),
            1,
            "the part already posted was posted again"
        );
        let rejoined: String = std::iter::once(&posts[0])
            .chain(&posts[2..])
            .map(|(_, body)| body["content"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            rejoined, text,
            "{reply_to:?}: the thread does not hold the rest of the text"
        );
        task.abort();
    }
}

// --- Slack -----------------------------------------------------------------------------------------

const TEAM: &str = "C0000MAIN1";

/// A Slack deployment assembled from configuration as `main` assembles one, against the fake,
/// taking `max_post_chars` in one message: a bridge in front of Slack, as the fake's address is.
async fn slack(fake: &Fake, max_post_chars: usize) -> AppState {
    let text = format!(
        r#"
[auth]
read_token = "{read}"
write_token = "{WRITE_TOKEN}"

[[providers]]
key = "slack"
kind = "slack"
api_base = "{base}"
token = "{TOKEN}"
max_post_chars = {max_post_chars}

[[channels]]
id = "{TEAM}"
label = "team"
writable = true
"#,
        read = testing::READ_TOKEN,
        base = fake.base,
    );
    let (mut state, _discord, _store) = testing::state_with_store_from_toml(&text);
    let router = ChatRouter::from_config(&state.config).expect("providers build");
    state.replace_providers(Arc::new(router));
    state
}

fn slack_fixture() -> FakeState {
    let mut state = FakeState::default();
    state.history.insert(
        TEAM.to_owned(),
        vec![profiled(
            &ts(10, 0),
            "UADA00001",
            "Ada",
            "is the build green?",
        )],
    );
    state
}

fn team_reply() -> String {
    format!("/api/v1/channels/{TEAM}/reply")
}

#[tokio::test]
async fn a_slack_post_over_its_limit_is_one_message_with_the_rest_in_its_thread() {
    let fake = Fake::start(slack_fixture(), 20).await;
    let state = slack(&fake, 600).await;

    // Under the limit as Slack is sent it: one message.
    let (status, answer) = call(&state, &team_reply(), json!({ "text": "short" })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(fake.requests("chat.postMessage").len(), 1);

    // Over it, counted after the escapes Slack is sent: `<` is four characters there.
    let text = format!(
        "{}\n\nVec<Option<T>> -> Result<(), E>\n\n{}",
        prose(900),
        prose(900)
    );
    let (status, answer) = call(&state, &team_reply(), json!({ "text": text })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let sent = fake.requests("chat.postMessage")[1..].to_vec();
    assert!(sent.len() > 2, "the fixture did not split");
    assert!(
        !sent[0].contains_key("thread_ts"),
        "the first part did not go to the conversation"
    );
    let first_ts = vibe_talk::slack::ts_from_message_id(&vibe_talk::model::MessageId(
        answer["parts"][0]["id"]
            .as_str()
            .expect("first id")
            .to_owned(),
    ))
    .expect("a Slack id");
    for later in &sent[1..] {
        assert_eq!(
            later.get("thread_ts"),
            Some(&first_ts),
            "a later part was not posted in the first part's thread"
        );
    }
    assert!(sent
        .iter()
        .all(|params| params["text"].chars().count() <= 600));
    let parts = answer["parts"].as_array().expect("parts");
    assert_eq!(parts[0]["thread"]["is_root"], true, "{}", parts[0]);
    assert_eq!(parts[0]["thread"]["id"], first_ts.as_str());
    let rejoined: String = parts
        .iter()
        .map(|part| part["content"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(rejoined, text);
}

#[tokio::test]
async fn a_long_slack_reply_stays_in_the_thread_it_answered() {
    // A Slack reply IS a thread reply. Before, only the first part went into the answered
    // message's thread, and the rest landed in the conversation on their own.
    let fake = Fake::start(slack_fixture(), 20).await;
    let state = slack(&fake, 600).await;
    let target = vibe_talk::slack::message_id_from_ts(&ts(10, 0)).expect("id");
    let text = prose(1_800);
    let (status, answer) = call(
        &state,
        &team_reply(),
        json!({ "text": text, "reply_to": target.as_str() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let sent = fake.requests("chat.postMessage");
    assert!(sent.len() > 2, "the fixture did not split");
    assert!(
        sent.iter()
            .all(|params| params.get("thread_ts") == Some(&ts(10, 0))),
        "a part left the thread of the message it answers: {sent:?}"
    );
}

#[tokio::test]
async fn a_slack_post_that_failed_part_way_is_resumed_in_the_first_parts_thread() {
    let mut fixture = slack_fixture();
    fixture.fail_posts = vec![2];
    let fake = Fake::start(fixture, 20).await;
    let state = slack(&fake, 600).await;
    let text = prose(1_800);
    let (status, partial) = call(&state, &team_reply(), json!({ "text": text })).await;
    let shown = brief(&partial);
    assert_eq!(status, StatusCode::MULTI_STATUS, "{shown}");
    assert_eq!(partial["posted"], 1, "{shown}");
    let sent = fake.requests("chat.postMessage");
    let thread = sent[1]["thread_ts"].clone();
    assert!(!sent[0].contains_key("thread_ts"));
    assert_eq!(partial["thread_id"], thread.as_str(), "{shown}");

    let unsent = partial["unsent"].as_str().expect("unsent").to_owned();
    let (status, done) = call(
        &state,
        &team_reply(),
        json!({ "text": unsent, "thread_id": thread }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    let sent = fake.requests("chat.postMessage");
    assert_eq!(
        sent[2]["text"], sent[1]["text"],
        "the retry did not start at the part that failed"
    );
    assert!(
        sent[2..]
            .iter()
            .all(|params| params.get("thread_ts") == Some(&thread)),
        "the retry posted outside the first part's thread"
    );
    assert_eq!(
        sent.iter()
            .filter(|params| !params.contains_key("thread_ts"))
            .count(),
        1,
        "the part already posted was posted again"
    );
}

// --- Discord -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_long_discord_post_is_still_consecutive_messages_in_the_channel() {
    // Discord cannot start a thread from a message this route just posted, so it keeps what it
    // always did: messages one after another, and only the first answers the message.
    let (state, discord) = testing::state();
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    let target = discord.seed(&channel, "codex-eng", "what happened?");
    let text = prose(4_500);
    let (status, answer) = call(
        &state,
        &reply_uri(),
        json!({ "text": text, "reply_to": target.as_str() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let posted = discord.posted();
    assert!(
        posted.len() > 2,
        "the fixture did not split at 2000 characters"
    );
    assert!(posted.iter().all(|post| post.channel == channel));
    assert!(posted
        .iter()
        .all(|post| post.content.chars().count() <= 2000));
    assert_eq!(posted[0].reply_to.as_ref(), Some(&target));
    assert!(posted[1..].iter().all(|post| post.reply_to.is_none()));
    let parts = answer["parts"].as_array().expect("parts");
    assert!(
        parts
            .iter()
            .all(|part| part.get("thread").is_none_or(Value::is_null)),
        "a Discord part was placed in a thread"
    );
}

// --- a direct voice send ---------------------------------------------------------------------------

/// `post_reply` through MCP, as a voice agent calls it, and the sentence its model is handed.
async fn voice_post_reply(state: &AppState, text: &str) -> String {
    voice_post_reply_with(state, json!({ "channel_id": WRITE_CHANNEL, "text": text })).await
}

/// [`voice_post_reply`], with the tool's `arguments` as given.
async fn voice_post_reply_with(state: &AppState, arguments: Value) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", format!("Bearer {WRITE_TOKEN}"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "post_reply", "arguments": arguments },
            })
            .to_string(),
        ))
        .expect("request");
    let response = vibe_talk::http::router(state.clone())
        .oneshot(request)
        .await
        .expect("router responds");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("a JSON-RPC answer");
    body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// `#264 voice-send-approval` with "Ask me before the voice agent sends" off. The direct send is
/// the same send as every other, so a long one to Google Chat is one message in the conversation
/// with the rest in its thread, and the model is told where the rest is: a speaker told only that
/// it went as several messages looks in the channel and finds one. One that fails part-way answers
/// the REST route with the reply route's 207, naming that thread, and tells the model it was partly
/// sent.
#[tokio::test]
async fn a_direct_voice_send_continues_in_the_first_parts_thread_and_says_where() {
    let (state, posts, task) = google_chat(600, &[]).await;
    state
        .store
        .set_ask_before_send(false)
        .await
        .expect("confirmation off");
    let said = voice_post_reply(&state, &prose(2_400)).await;
    {
        let posts = posts.lock().expect("posts");
        assert!(posts.len() > 2, "the fixture did not split");
        assert_eq!(
            posts[0].0, MAIN,
            "the first part did not go to the conversation"
        );
        assert!(
            posts[1..]
                .iter()
                .all(|(path, _)| path == &in_thread("t_91940001")),
            "a later part was not posted in the first part's thread"
        );
        assert!(said.starts_with("SENT. "), "{said}");
        assert!(
            said.contains(&format!(
                "It was long, so it went as {} messages: the first in the channel, and the rest \
                 in the thread under it.",
                posts.len()
            )),
            "{said}"
        );
    }
    task.abort();

    let (state, posts, task) = google_chat(600, &[2]).await;
    state
        .store
        .set_ask_before_send(false)
        .await
        .expect("confirmation off");
    let (status, partial) = call(
        &state,
        "/api/v1/post-proposals",
        json!({ "channel_id": WRITE_CHANNEL, "text": prose(2_400) }),
    )
    .await;
    let shown = brief(&partial);
    assert_eq!(status, StatusCode::MULTI_STATUS, "{shown}");
    assert_eq!(partial["posted"], 1, "{shown}");
    assert_eq!(
        partial["thread_id"], "t_91940001",
        "the 207 did not say where the rest goes: {shown}"
    );
    assert_eq!(posts.lock().expect("posts").len(), 2);
    task.abort();

    // The tool, the same failure: the part whose answer was lost may be in the thread, so the
    // model is told it was partly sent and not that the rest was not.
    let (state, _posts, task) = google_chat(600, &[2]).await;
    state
        .store
        .set_ask_before_send(false)
        .await
        .expect("confirmation off");
    let said = voice_post_reply(&state, &prose(2_400)).await;
    assert!(said.starts_with("PARTLY SENT. "), "{said}");
    assert!(!said.contains("The rest was NOT sent"), "{said}");
    task.abort();
}

/// `#264 voice-send-approval`, a direct send that answers a message on Slack. A Slack reply is a
/// thread reply, so every part, the first too, goes into the answered message's thread, and the
/// model is told so: not that the first is in the channel, where the speaker would not find it.
#[tokio::test]
async fn a_long_direct_slack_reply_is_said_to_be_all_in_the_thread_it_answered() {
    let fake = Fake::start(slack_fixture(), 20).await;
    let state = slack(&fake, 600).await;
    state
        .store
        .set_ask_before_send(false)
        .await
        .expect("confirmation off");
    let target = vibe_talk::slack::message_id_from_ts(&ts(10, 0)).expect("id");
    let said = voice_post_reply_with(
        &state,
        json!({ "channel_id": TEAM, "text": prose(1_800), "reply_to": target.as_str() }),
    )
    .await;
    let sent = fake.requests("chat.postMessage");
    assert!(sent.len() > 2, "the fixture did not split");
    assert!(
        sent.iter()
            .all(|params| params.get("thread_ts") == Some(&ts(10, 0))),
        "a part left the thread of the message it answers: {sent:?}"
    );
    assert!(said.starts_with("SENT. "), "{said}");
    assert!(
        said.contains(&format!(
            "It was long, so it went as {} messages, all of them in the thread of the message it \
             answered.",
            sent.len()
        )),
        "{said}"
    );
    assert!(!said.contains("in the channel"), "{said}");
}

/// `#264 voice-send-approval`: the same words asked for again after a direct send whose second
/// part's answer was lost post nothing, and the model is told what the first send's own failure
/// told it: part was posted, and whether the rest was could not be confirmed. Not that the rest
/// did not go, which a part that may be in the thread would make untrue.
#[tokio::test]
async fn a_repeat_of_a_direct_send_whose_rest_is_unconfirmed_does_not_say_the_rest_did_not_go() {
    let (state, posts, task) = google_chat(600, &[2]).await;
    state
        .store
        .set_ask_before_send(false)
        .await
        .expect("confirmation off");
    let text = prose(2_400);
    let first = voice_post_reply(&state, &text).await;
    assert!(
        first.starts_with("PARTLY SENT. Whether the rest was posted could not be confirmed"),
        "{first}"
    );
    let attempts = posts.lock().expect("posts").len();

    let again = voice_post_reply(&state, &text).await;
    assert!(again.starts_with("PARTLY SENT already. "), "{again}");
    assert!(
        again.contains("whether the rest was posted could not be confirmed"),
        "{again}"
    );
    assert!(!again.contains("did not"), "{again}");
    assert_eq!(
        posts.lock().expect("posts").len(),
        attempts,
        "the repeat posted"
    );
    task.abort();
}
