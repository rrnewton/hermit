//! `#206 pin-message`: the owner's own pinned messages, over HTTP.
//!
//! The store's half — idempotence, the bound, durability, the migration — is pinned in
//! `src/store/sqlite.rs` against a real file. These pin what the routes promise on top of it:
//!
//!   * **The routes follow the house rules.** Listing is a read, pinning and unpinning are writes,
//!     an unconfigured channel is the same 404 everywhere, and a snapshot the store cannot keep is
//!     a 400 that names the field rather than a quiet trim.
//!   * **Both acts are idempotent**, and say whether they changed anything, so two devices a
//!     refresh apart cannot fight over a pin.
//!   * **A refresh that already happens is what keeps every device current.** Every channel read
//!     carries the list's revision, and only a change moves it.
//!   * **The owner, and only the owner, decides.** No MCP tool lists, pins or unpins.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::chat::{ChatClient, ChatError, ChatIdentity};
use vibe_talk::http::router;
use vibe_talk::model::{ChannelId, Message, MessageId, UserId};
use vibe_talk::store::{MAX_PINS_PER_CHANNEL, MAX_PIN_TEXT_CHARS};
use vibe_talk::testing::{self, READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};
use vibe_talk::threads::{TimelinePage, TimelineRequest};

/// A backend that answers every timeline read with one message, so the route's own fields are
/// what is under test.
struct OneMessage;

fn message(id: &str) -> Message {
    Message {
        thread: None,
        id: MessageId(id.to_owned()),
        channel_id: ChannelId(READ_CHANNEL.to_owned()),
        author: "build-bot".to_owned(),
        author_id: UserId("7".to_owned()),
        author_is_bot: true,
        timestamp: "2026-10-04T12:00:00Z".to_owned(),
        spoken_time: String::new(),
        reply_to: None,
        content: "the nightly build is green".to_owned(),
        spoken_content: String::new(),
        content_html: String::new(),
        noise: false,
        reactions: None,
        heading: None,
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
    fn new() -> Self {
        let (mut state, _discord, _store) = testing::state_with_store();
        state.replace_chat(Arc::new(OneMessage));
        Self {
            router: router(state),
        }
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

    async fn pin(&self, channel: &str, id: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            "PUT",
            &format!("/api/v1/channels/{channel}/pins/{id}"),
            Some(WRITE_TOKEN),
            Some(body),
        )
        .await
    }

    async fn list(&self, channel: &str) -> Value {
        let (status, body) = self
            .call(
                "GET",
                &format!("/api/v1/channels/{channel}/pins"),
                Some(READ_TOKEN),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn revision_on_read(&self, route: &str) -> Value {
        let (status, body) = self
            .call(
                "GET",
                &format!("/api/v1/channels/{READ_CHANNEL}/{route}"),
                Some(READ_TOKEN),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{route}: {body}");
        body["pins_revision"].clone()
    }
}

/// The row a page sends when the owner pins it.
fn snapshot(content: &str) -> Value {
    json!({
        "author": "build-bot",
        "author_id": "7",
        "author_is_bot": true,
        "content": content,
        "timestamp": "2026-10-04T12:00:00Z",
        "thread_id": "spaces/A/threads/B",
        "thread_root": false,
    })
}

fn ids(list: &Value) -> Vec<&str> {
    list["pins"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|pin| pin["message_id"].as_str().expect("an id"))
        .collect()
}

#[tokio::test]
async fn a_pin_is_listed_with_its_snapshot_and_both_acts_are_idempotent() {
    let harness = Harness::new();
    let empty = harness.list(READ_CHANNEL).await;
    assert_eq!(empty["pins"], json!([]));
    assert_eq!(empty["revision"], json!(0));
    assert_eq!(empty["limit"], json!(MAX_PINS_PER_CHANNEL));
    assert!(empty["pins_notice"]
        .as_str()
        .is_some_and(|notice| notice.contains("pins nothing there")));

    let (status, first) = harness
        .pin(READ_CHANNEL, "1000000000000000100", snapshot("green again"))
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["pinned"], json!(true));
    assert_eq!(first["changed"], json!(true));
    assert_eq!(first["unpinned"], json!([]));
    assert_eq!(first["pin"]["content"], json!("green again"));
    assert_eq!(first["pin"]["thread_id"], json!("spaces/A/threads/B"));
    let revision = first["revision"].as_i64().expect("a revision");
    assert!(revision > 0);

    // The second tap of a control whose answer the reader may not have seen yet.
    let (status, again) = harness
        .pin(READ_CHANNEL, "1000000000000000100", snapshot("green again"))
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        again["changed"],
        json!(false),
        "a repeat said it changed something"
    );
    assert_eq!(
        again["revision"],
        json!(revision),
        "a repeat moved the revision"
    );

    let listed = harness.list(READ_CHANNEL).await;
    assert_eq!(ids(&listed), vec!["1000000000000000100"]);
    assert_eq!(listed["revision"], json!(revision));
    let pin = &listed["pins"][0];
    assert_eq!(pin["author"], json!("build-bot"));
    assert_eq!(pin["author_id"], json!("7"));
    assert_eq!(pin["author_is_bot"], json!(true));
    assert_eq!(pin["timestamp"], json!("2026-10-04T12:00:00Z"));
    assert_eq!(pin["truncated"], json!(false));
    assert!(pin["pinned_at_ms"].as_i64().is_some_and(|at| at > 0));

    let uri = format!("/api/v1/channels/{READ_CHANNEL}/pins/1000000000000000100");
    let (status, off) = harness.call("DELETE", &uri, Some(WRITE_TOKEN), None).await;
    assert_eq!(status, StatusCode::OK, "{off}");
    assert_eq!(off["pinned"], json!(false));
    assert_eq!(off["changed"], json!(true));
    assert!(
        off.get("pin").is_none(),
        "an unpin answered with a pin: {off}"
    );
    let (status, off_again) = harness.call("DELETE", &uri, Some(WRITE_TOKEN), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unpinning twice is not an error: {off_again}"
    );
    assert_eq!(off_again["changed"], json!(false));
    assert_eq!(off_again["revision"], off["revision"]);
    assert_eq!(harness.list(READ_CHANNEL).await["pins"], json!([]));
}

#[tokio::test]
async fn every_channel_read_carries_the_pin_revision_and_only_a_change_moves_it() {
    // What lets a page keep pins current without re-reading the list on every refresh.
    let harness = Harness::new();
    for route in ["timeline?view=main", "page", "todo"] {
        assert_eq!(
            harness.revision_on_read(route).await,
            json!(0),
            "{route} did not say the channel has no pins yet"
        );
    }
    let (_, pinned) = harness
        .pin(READ_CHANNEL, "1000000000000000100", snapshot("pinned"))
        .await;
    for route in ["timeline?view=main", "page", "todo"] {
        assert_eq!(
            harness.revision_on_read(route).await,
            pinned["revision"],
            "{route} did not carry the revision the pin made"
        );
    }
    // Another channel's pin is not this channel's change.
    harness
        .pin(WRITE_CHANNEL, "1000000000000000200", snapshot("elsewhere"))
        .await;
    assert_eq!(
        harness.revision_on_read("timeline?view=main").await,
        pinned["revision"]
    );
}

#[tokio::test]
async fn a_revision_the_store_cannot_read_is_left_out_rather_than_failing_the_read() {
    // SOFT, on purpose: a timeline read is how the owner reads his channel, and a counter for a
    // marker must not take the channel away.
    let (state, _discord, store) = testing::state_with_store();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    assert_eq!(
        vibe_talk::ops::pins_revision(&state, &channel).await,
        Some(0)
    );
    store.fail_next("the disk went away");
    assert_eq!(vibe_talk::ops::pins_revision(&state, &channel).await, None);
    assert_eq!(
        vibe_talk::ops::pins_revision(&state, &channel).await,
        Some(0),
        "one failure is not a standing answer"
    );
}

#[tokio::test]
async fn a_server_with_no_store_refuses_pins_by_naming_the_setting_rather_than_listing_none() {
    // "Nothing is pinned" would be a lie a page would believe, and then offer a Pin that fails.
    let (mut state, _discord) =
        testing::state_with(Arc::new(vibe_talk::store::disabled::DisabledStore));
    state.replace_chat(Arc::new(OneMessage));
    let harness = Harness {
        router: router(state),
    };
    let (status, body) = harness
        .call(
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/pins"),
            Some(READ_TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], json!("storage_not_configured"));
    let (status, body) = harness
        .pin(READ_CHANNEL, "1000000000000000100", snapshot("x"))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn listing_is_a_read_and_pinning_and_unpinning_are_writes() {
    let harness = Harness::new();
    let pins = format!("/api/v1/channels/{READ_CHANNEL}/pins");
    let one = format!("{pins}/1000000000000000100");
    assert_eq!(
        harness.call("GET", &pins, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = harness
        .call("PUT", &one, Some(READ_TOKEN), Some(snapshot("x")))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token pinned a message"
    );
    let (status, _) = harness.call("DELETE", &one, Some(READ_TOKEN), None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a read token unpinned a message"
    );
    assert_eq!(
        harness.list(READ_CHANNEL).await["pins"],
        json!([]),
        "a refused write was kept"
    );

    // An unconfigured channel is the allowlist's one answer, for every method.
    let elsewhere = "/api/v1/channels/9999999999/pins";
    for (method, uri, body) in [
        ("GET", elsewhere.to_owned(), None),
        ("PUT", format!("{elsewhere}/1"), Some(snapshot("x"))),
        ("DELETE", format!("{elsewhere}/1"), None),
    ] {
        let (status, answer) = harness.call(method, &uri, Some(WRITE_TOKEN), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}: {answer}");
        assert_eq!(answer["error"], json!("unknown_channel"));
    }
}

#[tokio::test]
async fn a_snapshot_is_held_to_its_bounds_the_text_cut_and_every_id_refused() {
    let harness = Harness::new();
    // The text is cut, not refused, and the cut is recorded.
    let long = "x".repeat(MAX_PIN_TEXT_CHARS + 50);
    let (status, cut) = harness
        .pin(READ_CHANNEL, "1000000000000000100", snapshot(&long))
        .await;
    assert_eq!(status, StatusCode::OK, "{cut}");
    assert_eq!(cut["pin"]["truncated"], json!(true));
    assert_eq!(
        cut["pin"]["content"]
            .as_str()
            .map(|text| text.chars().count()),
        Some(MAX_PIN_TEXT_CHARS)
    );

    // Everything else is refused, naming what was wrong, and nothing is kept.
    let oversized = "9".repeat(vibe_talk::store::MAX_PIN_ID_BYTES + 1);
    let mut no_author = snapshot("x");
    no_author["author_id"] = json!("");
    let mut long_time = snapshot("x");
    long_time["timestamp"] = json!("2".repeat(65));
    let mut long_thread = snapshot("x");
    long_thread["thread_id"] = json!(oversized.clone());
    for (what, id, body) in [
        ("an empty author id", "1000000000000000101", no_author),
        ("an oversized timestamp", "1000000000000000102", long_time),
        ("an oversized thread id", "1000000000000000103", long_thread),
        ("an oversized message id", oversized.as_str(), snapshot("x")),
    ] {
        let (status, answer) = harness.pin(READ_CHANNEL, id, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {answer}");
        assert_eq!(answer["error"], json!("bad_id"), "{what}");
    }
    // A body missing what a row needs is the JSON extractor's refusal, before the store.
    let (status, _) = harness
        .pin(
            READ_CHANNEL,
            "1000000000000000104",
            json!({ "content": "x" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, answer) = harness
        .call(
            "DELETE",
            &format!("/api/v1/channels/{READ_CHANNEL}/pins/{oversized}"),
            Some(WRITE_TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(
        ids(&harness.list(READ_CHANNEL).await),
        vec!["1000000000000000100"]
    );
}

#[tokio::test]
async fn a_message_id_with_slashes_travels_escaped_and_comes_back_whole() {
    // A provider-neutral id is opaque to this server and may carry a path separator; the page
    // escapes it as one path segment, as it does for every other per-message route.
    let harness = Harness::new();
    let (status, body) = harness
        .pin(READ_CHANNEL, "spaces%2FA%2Fmessages%2FB", snapshot("x"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message_id"], json!("spaces/A/messages/B"));
    assert_eq!(
        ids(&harness.list(READ_CHANNEL).await),
        vec!["spaces/A/messages/B"]
    );
    let (status, _) = harness
        .call(
            "DELETE",
            &format!("/api/v1/channels/{READ_CHANNEL}/pins/spaces%2FA%2Fmessages%2FB"),
            Some(WRITE_TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.list(READ_CHANNEL).await["pins"], json!([]));
}

#[tokio::test]
async fn at_the_bound_a_pin_names_the_oldest_pin_it_unpinned() {
    let harness = Harness::new();
    let id = |n: usize| format!("{}", 1_000_000_000_000_001_000_u64 + n as u64);
    for n in 0..MAX_PINS_PER_CHANNEL {
        let (status, body) = harness.pin(READ_CHANNEL, &id(n), snapshot("x")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["unpinned"],
            json!([]),
            "pin {n} unpinned something under the bound"
        );
    }
    let (status, over) = harness
        .pin(READ_CHANNEL, &id(MAX_PINS_PER_CHANNEL), snapshot("x"))
        .await;
    assert_eq!(status, StatusCode::OK, "{over}");
    assert_eq!(over["unpinned"], json!([id(0)]));
    let listed = harness.list(READ_CHANNEL).await;
    assert_eq!(ids(&listed).len(), MAX_PINS_PER_CHANNEL);
    assert!(!ids(&listed).contains(&id(0).as_str()));
}

#[tokio::test]
async fn no_mcp_tool_can_list_pin_or_unpin() {
    // The scope is not what keeps a voice agent out — it is routinely given the write token.
    // There being no tool is.
    let harness = Harness::new();
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
        names.iter().all(|name| !name.contains("pin")),
        "a model was handed a pin tool: {names:?}"
    );
}
