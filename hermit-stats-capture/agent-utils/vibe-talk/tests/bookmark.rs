//! `#240 reading-bookmark`: the owner's place in a channel's backlog, over HTTP.
//!
//! The store's half — the shared write rule, durability across a restart, the purge, the migration,
//! and a property test against a one-slot model — is pinned in `src/store/` against a real file.
//! These pin what the routes promise on top of it:
//!
//!   * **The routes follow the house rules.** Reading is a read, moving and clearing are writes, an
//!     unconfigured channel is the same 404 everywhere, and an anchor the store cannot keep is a 400
//!     that names the field.
//!   * **A conditional write never undoes a move made elsewhere.** A stale `if_revision` is 409, and
//!     the refusal carries where the bookmark is now, so the page needs no second read.
//!   * **A refresh that already happens keeps every device current.** Every channel read carries the
//!     bookmark, and a read the store cannot answer leaves the field out rather than failing.
//!   * **The owner, and only the owner, decides.** No MCP tool reads or moves it.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::chat::{ChatClient, ChatError, ChatIdentity};
use vibe_talk::http::router;
use vibe_talk::model::{ChannelId, Message, MessageId, UserId};
use vibe_talk::testing::{self, READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};
use vibe_talk::threads::{TimelinePage, TimelineRequest};

/// A backend that answers every read with one message, so the routes' own fields are under test.
struct OneMessage;

fn message(id: &str) -> Message {
    Message {
        thread: None,
        id: MessageId(id.to_owned()),
        channel_id: ChannelId(READ_CHANNEL.to_owned()),
        author: "build-bot".to_owned(),
        author_id: UserId("7".to_owned()),
        author_is_bot: true,
        timestamp: "2026-10-12T12:00:00Z".to_owned(),
        spoken_time: String::new(),
        reply_to: None,
        content: "the nightly build is green".to_owned(),
        spoken_content: String::new(),
        content_html: String::new(),
        heading: None,
        noise: false,
        reactions: None,
    }
}

#[async_trait::async_trait]
impl ChatClient for OneMessage {
    fn provider_name(&self) -> &str {
        "Example chat"
    }

    fn supports_threading(&self) -> bool {
        true
    }

    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        Ok(ChatIdentity {
            id: "7".to_owned(),
            username: "reader".to_owned(),
        })
    }

    async fn fetch_page(
        &self,
        _channel: &ChannelId,
        _limit: u16,
        _before: Option<&MessageId>,
        _after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        Ok(vec![message("1000000000000000100")])
    }

    async fn fetch_timeline(
        &self,
        _channel: &ChannelId,
        _request: &TimelineRequest,
    ) -> Result<TimelinePage, ChatError> {
        Ok(TimelinePage {
            messages: vec![message("1000000000000000100")],
            ..TimelinePage::default()
        })
    }

    async fn post_message(
        &self,
        _channel: &ChannelId,
        _content: &str,
        _reply: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        Err(ChatError::Refused("this backend does not post".to_owned()))
    }
}

struct Harness {
    router: axum::Router,
}

impl Harness {
    fn new() -> (Self, Arc<vibe_talk::store::fake::FakeStore>) {
        let (mut state, _discord, store) = testing::state_with_store();
        state.replace_chat(Arc::new(OneMessage));
        (
            Self {
                router: router(state),
            },
            store,
        )
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
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
        let response = self
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

    async fn put(&self, channel: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            "PUT",
            &format!("/api/v1/channels/{channel}/bookmark"),
            Some(WRITE_TOKEN),
            Some(body),
        )
        .await
    }

    async fn clear(&self, channel: &str, query: &str) -> (StatusCode, Value) {
        self.call(
            "DELETE",
            &format!("/api/v1/channels/{channel}/bookmark{query}"),
            Some(WRITE_TOKEN),
            None,
        )
        .await
    }

    async fn get(&self, channel: &str) -> Value {
        let (status, body) = self
            .call(
                "GET",
                &format!("/api/v1/channels/{channel}/bookmark"),
                Some(READ_TOKEN),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn on_read(&self, route: &str) -> Option<Value> {
        let (status, body) = self
            .call(
                "GET",
                &format!("/api/v1/channels/{READ_CHANNEL}/{route}"),
                Some(READ_TOKEN),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{route}: {body}");
        body.get("bookmark").cloned()
    }
}

/// The anchor a page sends when the owner presses Shift+B on a message.
fn above(id: &str) -> Value {
    json!({
        "message_id": id,
        "timestamp": "2026-10-12T12:00:00Z",
        "thread_id": "spaces/A/threads/B",
    })
}

const READS: [&str; 4] = ["timeline?view=main", "timeline?view=flat", "page", "todo"];

#[tokio::test]
async fn a_bookmark_is_set_read_back_repeated_and_cleared() {
    let (harness, _store) = Harness::new();
    let empty = harness.get(READ_CHANNEL).await;
    assert_eq!(empty["bookmark"], json!({ "revision": 0, "anchor": null }));
    assert_eq!(empty["changed"], json!(false));
    assert!(empty["bookmark_notice"]
        .as_str()
        .is_some_and(|notice| notice.contains("marks nothing read")));

    let (status, set) = harness
        .put(READ_CHANNEL, above("1000000000000000100"))
        .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["changed"], json!(true));
    assert_eq!(
        set["bookmark"]["anchor"],
        json!({
            "message_id": "1000000000000000100",
            "place": "above",
            "timestamp": "2026-10-12T12:00:00Z",
            "thread_id": "spaces/A/threads/B",
        }),
        "the place defaults to above, and the thread rides along"
    );
    let revision = set["bookmark"]["revision"].as_i64().expect("a revision");
    assert!(revision > 0);
    assert_eq!(harness.get(READ_CHANNEL).await["bookmark"], set["bookmark"]);

    // The second press of a key whose answer the reader may not have seen yet.
    let (status, again) = harness
        .put(READ_CHANNEL, above("1000000000000000100"))
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        again["changed"],
        json!(false),
        "a repeat said it changed something"
    );
    assert_eq!(
        again["bookmark"]["revision"],
        json!(revision),
        "a repeat moved the revision"
    );

    let mut below = above("1000000000000000100");
    below["place"] = json!("below");
    below["thread_id"] = Value::Null;
    let (status, moved) = harness.put(READ_CHANNEL, below).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["bookmark"]["anchor"]["place"], json!("below"));
    assert!(
        moved["bookmark"]["anchor"].get("thread_id").is_none(),
        "a message in no thread was given one: {moved}"
    );

    let (status, off) = harness.clear(READ_CHANNEL, "").await;
    assert_eq!(status, StatusCode::OK, "{off}");
    assert_eq!(off["changed"], json!(true));
    assert_eq!(off["bookmark"]["anchor"], Value::Null);
    let off_revision = off["bookmark"]["revision"].as_i64().expect("a revision");
    assert!(
        off_revision > moved["bookmark"]["revision"].as_i64().expect("a revision"),
        "clearing did not move the revision, so a page holding the old one could still write"
    );
    let (status, off_again) = harness.clear(READ_CHANNEL, "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "clearing twice is not an error: {off_again}"
    );
    assert_eq!(off_again["changed"], json!(false));
    assert_eq!(off_again["bookmark"]["revision"], json!(off_revision));
}

#[tokio::test]
async fn a_stale_conditional_write_is_refused_with_where_the_bookmark_is_now() {
    // What keeps an automatic move — the page sliding the line down after a Done — from undoing
    // a move the owner made on his other device a moment before.
    let (harness, _store) = Harness::new();
    let (_, phone) = harness
        .put(READ_CHANNEL, above("1000000000000000100"))
        .await;
    let seen = phone["bookmark"]["revision"].as_i64().expect("a revision");
    let (_, desk) = harness
        .put(READ_CHANNEL, above("1000000000000000300"))
        .await;

    let mut bump = above("1000000000000000200");
    bump["if_revision"] = json!(seen);
    let (status, refused) = harness.put(READ_CHANNEL, bump.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], json!("bookmark_moved"));
    assert!(refused["detail"].as_str().is_some_and(|d| !d.is_empty()));
    assert_eq!(
        refused["bookmark"], desk["bookmark"],
        "the refusal did not say where the bookmark is now"
    );
    assert_eq!(refused["changed"], json!(false));
    assert_eq!(
        harness.get(READ_CHANNEL).await["bookmark"],
        desk["bookmark"]
    );

    // With the revision that is stored, the same write goes through.
    bump["if_revision"] = desk["bookmark"]["revision"].clone();
    let (status, bumped) = harness.put(READ_CHANNEL, bump).await;
    assert_eq!(status, StatusCode::OK, "{bumped}");
    assert_eq!(
        bumped["bookmark"]["anchor"]["message_id"],
        json!("1000000000000000200")
    );

    // A clear is conditional the same way, in its query string.
    let (status, refused) = harness
        .clear(READ_CHANNEL, &format!("?if_revision={seen}"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["bookmark"], bumped["bookmark"]);
    let (status, cleared) = harness
        .clear(
            READ_CHANNEL,
            &format!("?if_revision={}", bumped["bookmark"]["revision"]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["bookmark"]["anchor"], Value::Null);
}

#[tokio::test]
async fn every_channel_read_carries_the_bookmark_and_another_channels_is_its_own() {
    let (harness, _store) = Harness::new();
    for route in READS {
        assert_eq!(
            harness.on_read(route).await,
            Some(json!({ "revision": 0, "anchor": null })),
            "{route} did not say the channel has no bookmark"
        );
    }
    let (_, set) = harness
        .put(READ_CHANNEL, above("1000000000000000100"))
        .await;
    for route in READS {
        assert_eq!(
            harness.on_read(route).await,
            Some(set["bookmark"].clone()),
            "{route} did not carry the bookmark just set"
        );
    }
    harness
        .put(WRITE_CHANNEL, above("1000000000000000900"))
        .await;
    assert_eq!(
        harness.on_read("timeline?view=main").await,
        Some(set["bookmark"].clone()),
        "another channel's bookmark showed up in this one"
    );
}

#[tokio::test]
async fn a_bookmark_the_store_cannot_read_is_left_out_of_a_read_rather_than_failing_it() {
    // SOFT, as the pins' revision is: a marker must not take the channel away. Left OUT, not null:
    // null would tell the page there is no bookmark, and it would draw none.
    let (harness, store) = Harness::new();
    harness
        .put(READ_CHANNEL, above("1000000000000000100"))
        .await;
    store.fail_next("the disk went away");
    let (status, body) = harness
        .call(
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/timeline?view=main"),
            Some(READ_TOKEN),
            None,
        )
        .await;
    // The one failure lands on whichever store read the route makes first; whichever it is, the
    // read either fails on its Done marks as before or answers without a bookmark — never with
    // `null` for a bookmark that is set.
    if status == StatusCode::OK {
        assert!(
            body.get("bookmark")
                .is_none_or(|held| !held["anchor"].is_null()),
            "a failing store was reported as no bookmark: {body}"
        );
    }
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let (state, _discord, store) = testing::state_with_store();
    assert_eq!(
        vibe_talk::ops::bookmark_state(&state, &channel)
            .await
            .map(|held| held.revision),
        Some(0)
    );
    store.fail_next("the disk went away");
    assert_eq!(vibe_talk::ops::bookmark_state(&state, &channel).await, None);
    assert!(
        vibe_talk::ops::bookmark_state(&state, &channel)
            .await
            .is_some(),
        "one failure is not a standing answer"
    );
}

#[tokio::test]
async fn a_server_with_no_store_refuses_the_bookmark_and_its_reads_leave_it_out() {
    let (mut state, _discord) =
        testing::state_with(Arc::new(vibe_talk::store::disabled::DisabledStore));
    state.replace_chat(Arc::new(OneMessage));
    let harness = Harness {
        router: router(state),
    };
    let (status, body) = harness
        .call(
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/bookmark"),
            Some(READ_TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], json!("storage_not_configured"));
    let (status, _) = harness
        .put(READ_CHANNEL, above("1000000000000000100"))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, body) = harness
        .call(
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/timeline?view=main"),
            Some(READ_TOKEN),
            None,
        )
        .await;
    if status == StatusCode::OK {
        assert!(body.get("bookmark").is_none(), "{body}");
    }
}

#[tokio::test]
async fn reading_is_a_read_and_moving_and_clearing_are_writes() {
    let (harness, _store) = Harness::new();
    let uri = format!("/api/v1/channels/{READ_CHANNEL}/bookmark");
    assert_eq!(
        harness.call("GET", &uri, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = harness
        .call(
            "PUT",
            &uri,
            Some(READ_TOKEN),
            Some(above("1000000000000000100")),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token moved the bookmark"
    );
    let (status, _) = harness.call("DELETE", &uri, Some(READ_TOKEN), None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token cleared the bookmark"
    );
    assert_eq!(
        harness.get(READ_CHANNEL).await["bookmark"]["anchor"],
        Value::Null,
        "a refused write was kept"
    );
    let elsewhere = "/api/v1/channels/9999999999/bookmark";
    for (method, body) in [
        ("GET", None),
        ("PUT", Some(above("1000000000000000100"))),
        ("DELETE", None),
    ] {
        let (status, answer) = harness
            .call(method, elsewhere, Some(WRITE_TOKEN), body)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}: {answer}");
        assert_eq!(answer["error"], json!("unknown_channel"));
    }
}

#[tokio::test]
async fn an_anchor_the_store_cannot_keep_is_refused_naming_the_field() {
    let (harness, _store) = Harness::new();
    let oversized = "9".repeat(vibe_talk::store::MAX_PIN_ID_BYTES + 1);
    let mut long_time = above("1000000000000000100");
    long_time["timestamp"] = json!("2".repeat(65));
    let mut long_thread = above("1000000000000000100");
    long_thread["thread_id"] = json!(oversized.clone());
    let mut no_time = above("1000000000000000100");
    no_time["timestamp"] = json!("yesterday");
    for (what, body) in [
        ("an empty message id", above("")),
        ("an oversized message id", above(&oversized)),
        ("an oversized timestamp", long_time),
        ("a timestamp that is no instant", no_time),
        ("an oversized thread id", long_thread),
    ] {
        let (status, answer) = harness.put(READ_CHANNEL, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {answer}");
        assert_eq!(answer["error"], json!("bad_id"), "{what}");
        assert!(
            answer["detail"]
                .as_str()
                .is_some_and(|d| d.contains("bookmark")),
            "{what}: {answer}"
        );
    }
    let mut sideways = above("1000000000000000100");
    sideways["place"] = json!("beside");
    for body in [json!({ "message_id": "1" }), sideways] {
        let (status, _) = harness.put(READ_CHANNEL, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert_eq!(
        harness.get(READ_CHANNEL).await["bookmark"]["anchor"],
        Value::Null
    );
}

#[tokio::test]
async fn no_mcp_tool_can_read_or_move_the_bookmark() {
    // The scope is not what keeps a voice agent out — it is routinely given the write token.
    let (harness, _store) = Harness::new();
    let (status, body) = harness
        .call(
            "POST",
            "/mcp",
            Some(WRITE_TOKEN),
            Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        !names.is_empty(),
        "the tool list is empty, so this proves nothing"
    );
    assert!(
        names.iter().all(|name| !name.contains("bookmark")),
        "a model was handed a bookmark tool: {names:?}"
    );
}
