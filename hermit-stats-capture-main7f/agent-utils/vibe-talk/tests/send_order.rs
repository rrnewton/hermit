//! `#277 send-order`: the server posts to each destination one at a time, in the order the posts
//! arrived, and holds a destination from the first part of a long post to its last.
//!
//! The owner: "I write A then B but B lands first." The page keeps a destination's messages in the
//! order they were written, but the server is the last place that order can be lost before the chat
//! service: the page's retry of A, sent after it stopped waiting for an answer the server was still
//! waiting on, and then B, reached a slow provider while that first A was still on its way. The
//! page stopping waiting closes its connection, and the server drops that request's handler; the
//! post it started has to hold its destination all the same, until the provider answers or the
//! provider client's own request timeout gives up on it. That timeout is the limit: past it the
//! server lets the next post go while a slow bridge may still be working on the first.
//!
//! Every test drives the real reply route, concurrently, against a provider that holds each post
//! until the test lets it go, and records the order posts REACH it in, and the order they land in.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tower::ServiceExt as _;
use vibe_talk::chat::{ChatClient, ChatError, ChatIdentity};
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::model::{ChannelId, Message, MessageId};
use vibe_talk::state::AppState;
use vibe_talk::testing::{self, WRITE_CHANNEL, WRITE_TOKEN};

/// A provider that lets each post through only when the test gives the gate a permit, and writes
/// down what reached it — `text`, or `text@thread` — before it waits, and what landed once it has.
struct HeldPosts {
    inner: Arc<FakeDiscord>,
    gate: Arc<Semaphore>,
    reached: Mutex<Vec<String>>,
    landed: Mutex<Vec<String>>,
}

impl HeldPosts {
    async fn post(
        &self,
        label: String,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.reached.lock().expect("reached").push(label.clone());
        self.gate
            .acquire()
            .await
            .expect("the test keeps the gate open")
            .forget();
        let posted = self.inner.post_message(channel, content, reply_to).await;
        if posted.is_ok() {
            self.landed.lock().expect("landed").push(label);
        }
        posted
    }
}

#[async_trait]
impl ChatClient for HeldPosts {
    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn supports_threading(&self) -> bool {
        true
    }

    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        self.inner.identity().await
    }

    async fn fetch_page(
        &self,
        channel: &ChannelId,
        limit: u16,
        before: Option<&MessageId>,
        after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        self.inner.fetch_page(channel, limit, before, after).await
    }

    async fn post_message(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        let label = first_word(content);
        self.post(label, channel, content, reply_to).await
    }

    async fn post_in_thread(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        let label = format!("{}@{thread_id}", first_word(content));
        self.post(label, channel, content, reply_to).await
    }
}

/// A post's name in the record: its first word, so each part of a long post can carry its own.
fn first_word(content: &str) -> String {
    content.split_whitespace().next().unwrap_or("").to_owned()
}

/// The server, with every post held at the provider until `gate` has a permit for it.
fn held_server() -> (AppState, Arc<HeldPosts>) {
    let (mut state, fake) = testing::state();
    let held = Arc::new(HeldPosts {
        inner: fake,
        gate: Arc::new(Semaphore::new(0)),
        reached: Mutex::new(Vec::new()),
        landed: Mutex::new(Vec::new()),
    });
    state.replace_chat(Arc::clone(&held) as Arc<dyn ChatClient>);
    (state, held)
}

/// POST `body` to the channel's reply route, as the page does, in a task of its own.
fn send(state: &AppState, body: Value) -> tokio::task::JoinHandle<(StatusCode, Value)> {
    let app = vibe_talk::http::router(state.clone());
    tokio::spawn(async move {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/channels/{WRITE_CHANNEL}/reply"))
            .header("authorization", format!("Bearer {WRITE_TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let response = app.oneshot(request).await.expect("router responds");
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
    })
}

/// What has reached the provider, once everything that can run has run.
async fn reached(held: &HeldPosts) -> Vec<String> {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    held.reached.lock().expect("reached").clone()
}

#[tokio::test]
async fn a_second_post_to_one_destination_waits_until_the_first_has_landed() {
    let (state, held) = held_server();
    let first = send(
        &state,
        json!({ "text": "A written first", "idempotency_key": "outgoing-a" }),
    );
    assert_eq!(reached(&held).await, ["A"]);

    // The page's retry of A, under A's key, while the first A is still on its way; then B.
    let retry = send(
        &state,
        json!({ "text": "A written first", "idempotency_key": "outgoing-a" }),
    );
    assert_eq!(reached(&held).await, ["A"]);
    let second = send(
        &state,
        json!({ "text": "B written second", "idempotency_key": "outgoing-b" }),
    );
    assert_eq!(
        reached(&held).await,
        ["A"],
        "a later post reached the provider while the first was still on its way"
    );

    held.gate.add_permits(1);
    assert_eq!(first.await.expect("first").0, StatusCode::OK);
    assert_eq!(
        reached(&held).await,
        ["A", "A"],
        "the retry did not go next"
    );
    held.gate.add_permits(1);
    assert_eq!(retry.await.expect("retry").0, StatusCode::OK);
    assert_eq!(reached(&held).await, ["A", "A", "B"]);
    held.gate.add_permits(1);
    assert_eq!(second.await.expect("second").0, StatusCode::OK);
    assert_eq!(
        state.post_lanes.in_use(),
        0,
        "a destination nobody is posting to is still held"
    );
}

#[tokio::test]
async fn a_post_whose_caller_went_away_holds_its_destination_until_the_provider_answers() {
    let (state, held) = held_server();
    let first = send(
        &state,
        json!({ "text": "A written first", "idempotency_key": "outgoing-a" }),
    );
    assert_eq!(reached(&held).await, ["A"]);

    // The page stops waiting for A — its own timeout, a sign-out, a proxy dropping a phone's
    // connection — and the server drops the request's handler with the connection.
    first.abort();
    assert!(
        first
            .await
            .expect_err("the caller went away")
            .is_cancelled(),
        "the request finished before its caller went away"
    );
    let second = send(
        &state,
        json!({ "text": "B written second", "idempotency_key": "outgoing-b" }),
    );
    assert_eq!(
        reached(&held).await,
        ["A"],
        "B reached the provider while it was still working on A, whose caller had gone"
    );
    assert_eq!(
        state.post_lanes.in_use(),
        1,
        "the destination was let go with the request"
    );

    // The provider answers for A, though nobody is waiting for that answer any more.
    held.gate.add_permits(1);
    assert_eq!(reached(&held).await, ["A", "B"], "B did not go once A had");
    held.gate.add_permits(1);
    assert_eq!(second.await.expect("second").0, StatusCode::OK);
    assert_eq!(
        *held.landed.lock().expect("landed"),
        ["A", "B"],
        "A, whose caller had gone, was not posted, or landed after B"
    );
    assert_eq!(state.post_lanes.in_use(), 0);
}

#[tokio::test]
async fn another_thread_does_not_wait_for_a_post_held_on_the_main_channel() {
    let (state, held) = held_server();
    let main = send(&state, json!({ "text": "A on the main channel" }));
    assert_eq!(reached(&held).await, ["A"]);
    let thread = send(
        &state,
        json!({ "text": "T into a thread", "thread_id": "t_one" }),
    );
    let other = send(
        &state,
        json!({ "text": "U into another", "thread_id": "t_two" }),
    );
    let mut seen = reached(&held).await;
    seen.sort();
    assert_eq!(
        seen,
        ["A", "T@t_one", "U@t_two"],
        "a thread waited for the main channel"
    );
    held.gate.add_permits(3);
    for task in [main, thread, other] {
        assert_eq!(task.await.expect("post").0, StatusCode::OK);
    }
}

#[tokio::test]
async fn nothing_lands_between_the_parts_of_a_long_post() {
    let (state, held) = held_server();
    // Two parts at the default limit of 2000 characters, each starting with its own word.
    let long = format!("first {}\n\nsecond {}", "a".repeat(1500), "b".repeat(1500));
    let split = send(&state, json!({ "text": long }));
    assert_eq!(reached(&held).await, ["first"]);
    let between = send(&state, json!({ "text": "B written meanwhile" }));
    assert_eq!(reached(&held).await, ["first"]);
    held.gate.add_permits(1);
    assert_eq!(
        reached(&held).await,
        ["first", "second"],
        "another post landed between the parts of a long one"
    );
    held.gate.add_permits(2);
    assert_eq!(split.await.expect("split").0, StatusCode::OK);
    assert_eq!(between.await.expect("between").0, StatusCode::OK);
    assert_eq!(reached(&held).await, ["first", "second", "B"]);
}
