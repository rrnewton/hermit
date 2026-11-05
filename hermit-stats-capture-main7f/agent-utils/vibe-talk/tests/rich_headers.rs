//! `#242 rich-headers`: where a message's heading travels, and the fixture the browser check draws.
//!
//! The rules that find a heading have their own tests in `src/heading/tests.rs`, and the marking of
//! the words it restates in `src/render.rs`. These drive the real router: the heading rides with
//! `content_html` on the routes only the page reads and on nothing toward a model; a paren tag is
//! confirmed by a second message from the same author within one read; and the messages
//! `tests/rich_headers_browser.py` draws are exactly what this server sends for their text.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::http::router;
use vibe_talk::model::ChannelId;
use vibe_talk::testing::{WRITE_CHANNEL, WRITE_TOKEN};

const HEADED: &str = "[build-bot coordinator] Backup check, 10:52 PM ET. All 4 shards are synced.";

async fn call(router: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {WRITE_TOKEN}"))
        .header("accept", "application/json, text/event-stream");
    let body = match body {
        Some(json) => {
            request = request.header("content-type", "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{uri}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)))
}

async fn get(router: &axum::Router, uri: &str) -> Value {
    call(router, "GET", uri, None).await
}

/// The message saying `text`, out of an answer's `messages`.
fn said<'v>(body: &'v Value, text: &str) -> &'v Value {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["content"] == text)
        .unwrap_or_else(|| panic!("no message says {text:?} in {body}"))
}

#[tokio::test]
async fn the_heading_travels_where_the_rendered_body_does_and_nowhere_toward_a_model() {
    let (state, discord) = vibe_talk::testing::state();
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    discord.seed(&channel, "build-bot", HEADED);
    let router = router(state);

    let page = get(
        &router,
        &format!("/api/v1/channels/{WRITE_CHANNEL}/page?render=html"),
    )
    .await;
    let message = said(&page, HEADED);
    assert_eq!(
        message["heading"],
        json!({"tag": "build-bot coordinator", "time": "10:52 PM ET", "title": "Backup check",
               "cut": 51}),
        "{message}"
    );
    // A plain body still carries no HTML: the page marks the restated words itself.
    assert!(message.get("content_html").is_none(), "{message}");
    assert_eq!(message["content"], HEADED, "the text is the whole text");

    let todo = get(&router, &format!("/api/v1/channels/{WRITE_CHANNEL}/todo")).await;
    assert!(said(&todo, HEADED)["heading"].is_object(), "{todo}");

    // Toward a model: the plain page read, the scrollback, one message, the digest, and the MCP
    // tool. None of them says anything about a heading.
    for uri in [
        format!("/api/v1/channels/{WRITE_CHANNEL}/page"),
        format!("/api/v1/channels/{WRITE_CHANNEL}/messages"),
        format!("/api/v1/channels/{WRITE_CHANNEL}/digest"),
    ] {
        let answer = get(&router, &uri).await;
        assert!(
            !answer.to_string().contains("\"heading\""),
            "{uri}: {answer}"
        );
    }
    let mcp = call(
        &router,
        "POST",
        "/mcp",
        Some(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "read_page", "arguments": { "channel_id": WRITE_CHANNEL } }
        })),
    )
    .await;
    let text = mcp.to_string();
    assert!(text.contains("Backup check"), "{text}");
    assert!(
        !text.contains("\\\"heading\\\"") && !text.contains("\"heading\""),
        "{text}"
    );
}

#[tokio::test]
async fn a_pin_carries_the_heading_of_its_snapshot() {
    let (state, _discord) = vibe_talk::testing::state();
    let router = router(state);
    let changed = call(
        &router,
        "PUT",
        &format!("/api/v1/channels/{WRITE_CHANNEL}/pins/1000000000000000101"),
        Some(json!({
            "author": "build-bot", "author_id": "7", "author_is_bot": true,
            "content": "[build-bot] Status, 9:05 AM ET:\n- one\n- two",
            "timestamp": "2026-10-08T12:00:00Z", "thread_id": null, "thread_root": false,
        })),
    )
    .await;
    assert_eq!(
        changed["pin"]["heading"],
        json!({"tag": "build-bot", "time": "9:05 AM ET", "title": "Status", "cut": 31})
    );
    assert_eq!(
        changed["pin"]["content_html"],
        "<p class=\"rh-src\">[build-bot] Status, 9:05 AM ET:</p>\n\
         <ul>\n<li>one</li>\n<li>two</li>\n</ul>\n"
    );
    let list = get(&router, &format!("/api/v1/channels/{WRITE_CHANNEL}/pins")).await;
    assert_eq!(list["pins"][0]["heading"], changed["pin"]["heading"]);
}

#[tokio::test]
async fn one_read_confirms_a_paren_tag_its_author_opened_two_messages_with() {
    let (state, discord) = vibe_talk::testing::state();
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    let first = "(ops2) Midday summary, 15:05Z.";
    discord.seed(&channel, "build-bot", first);
    // Another author's message with the same tag does not count for this one.
    discord.seed(&channel, "alice", "(ops2) my own aside");
    let router = router(state);
    let uri = format!("/api/v1/channels/{WRITE_CHANNEL}/page?render=html");
    let once = get(&router, &uri).await;
    assert!(said(&once, first).get("heading").is_none(), "{once}");

    let second = "(ops2) Evening summary, 23:05Z.";
    discord.seed(&channel, "build-bot", second);
    let twice = get(&router, &uri).await;
    assert_eq!(said(&twice, first)["heading"]["tag"], "ops2", "{twice}");
    assert_eq!(said(&twice, second)["heading"]["title"], "Evening summary");
    assert!(
        said(&twice, "(ops2) my own aside").get("heading").is_none(),
        "{twice}"
    );
}

#[tokio::test]
async fn pins_alone_confirm_a_paren_tag_with_no_read_of_the_channel() {
    // A fresh process, and nothing read: the pins route records the pins it serves, so two pins of
    // one author's `(tag)` messages confirm the tag for both.
    let (state, _discord) = vibe_talk::testing::state();
    let router = router(state);
    let pin = |id: &str, content: &str| {
        let uri = format!("/api/v1/channels/{WRITE_CHANNEL}/pins/{id}");
        let body = json!({
            "author": "build-bot", "author_id": "7", "author_is_bot": true, "content": content,
            "timestamp": "2026-10-08T12:00:00Z", "thread_id": null, "thread_root": false,
        });
        (uri, body)
    };
    let (uri, body) = pin("1000000000000000101", "(ops2) Midday summary, 15:05Z.");
    let first = call(&router, "PUT", &uri, Some(body)).await;
    assert!(
        first["pin"].get("heading").is_none(),
        "one message: {first}"
    );
    let (uri, body) = pin("1000000000000000102", "(ops2) Evening summary, 23:05Z.");
    let second = call(&router, "PUT", &uri, Some(body)).await;
    assert_eq!(
        second["pin"]["heading"]["title"], "Evening summary",
        "{second}"
    );
    let list = get(&router, &format!("/api/v1/channels/{WRITE_CHANNEL}/pins")).await;
    let pins = list["pins"].as_array().expect("pins");
    assert_eq!(pins.len(), 2, "{list}");
    for pin in pins {
        assert_eq!(pin["heading"]["tag"], "ops2", "{list}");
    }
}

#[tokio::test]
async fn a_trailing_tag_heads_a_bots_message_and_never_a_persons() {
    let (state, discord) = vibe_talk::testing::state();
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    // Twice each, so the tag would be confirmed for either author.
    for (author, text) in [
        ("build-bot", "Release notes (ops2): three fixes."),
        ("build-bot", "Release notes (ops2): one more."),
        ("alice", "Great work today (finally): it shipped."),
        ("alice", "Great work today (team): it shipped."),
        ("alice", "Coffee break (team): back soon."),
    ] {
        discord.seed(&channel, author, text);
    }
    let router = router(state);
    let page = get(
        &router,
        &format!("/api/v1/channels/{WRITE_CHANNEL}/page?render=html"),
    )
    .await;
    assert_eq!(
        said(&page, "Release notes (ops2): three fixes.")["heading"]["tag"],
        "ops2",
        "{page}"
    );
    for text in [
        "Great work today (finally): it shipped.",
        "Great work today (team): it shipped.",
        "Coffee break (team): back soon.",
    ] {
        assert!(said(&page, text).get("heading").is_none(), "{page}");
    }
}

// ---- the browser check's messages ------------------------------------------------------------

/// The messages `tests/rich_headers_browser.py` draws, as (author, text). Every shape the owner
/// named, in neutral words: bracket and paren tags, times in each place, a title-less time, a tag
/// alone, a rendered body, a plain one, a combined pair, a title that is one long unbroken token,
/// and someone without a tag at all.
const BROWSER_MESSAGES: &[(&str, &str)] = &[
    ("alice", "Is the release branch green yet?"),
    (
        "build-bot",
        "[build-bot coordinator] Backup check, 10:52 PM ET. All 4 shards are synced, the queue is \
         empty, and nothing needed fixing.",
    ),
    (
        "build-bot",
        "[build-bot coordinator] Report (11:58 AM ET):\nThe nightly run finished. Two jobs were \
         retried and both came back green.",
    ),
    (
        "build-bot",
        "[build-bot coordinator] Cache warm-up and cold starts, first figures (3:40 PM ET).\n\n\
         - shard one: 412 ms\n- shard two: 388 ms\n- shard three: 405 ms",
    ),
    (
        "build-bot",
        "(ops2) Midday summary, 15:05Z.\nNothing is stuck; the backlog cleared by 06:10Z.",
    ),
    (
        "build-bot",
        "(ops2) Weekly tally, 11:45Z (plan filed 02:57Z; expect 3-5 merges).\n\
         **Three** merged, and one is still in review.",
    ),
    (
        "build-bot",
        "[build-bot coordinator] 3:20 PM ET:\nAll shards idle.",
    ),
    (
        "build-bot",
        "[build-bot coordinator] Yes. The run is green and the tag is pushed.",
    ),
    (
        "build-bot",
        "[build-bot coordinator] Release notes, 4:05 PM ET: three fixes",
    ),
    (
        "build-bot",
        "[build-bot coordinator] Follow-up: and one new flag.",
    ),
    // A title that is one unbroken token, as a commit hash is: it has to break inside its row on a
    // phone rather than push the header off the screen.
    (
        "build-bot",
        "[build-bot coordinator] Merged \
         2c94a99229cf0cb59720f5094fd658b6d3cbfb34432720925fe692db30e41e4d.\nNext merge is queued.",
    ),
    ("owner", "[human] Thanks, ship it."),
];

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("rich_headers_messages.json")
}

/// What the server sends for each of [`BROWSER_MESSAGES`]: its text, rendered body and heading.
async fn browser_messages() -> Value {
    let (state, discord) = vibe_talk::testing::state();
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    for (author, text) in BROWSER_MESSAGES {
        discord.seed(&channel, author, text);
    }
    let router = router(state);
    let page = get(
        &router,
        &format!("/api/v1/channels/{WRITE_CHANNEL}/page?render=html&limit=50"),
    )
    .await;
    let rows: Vec<Value> = BROWSER_MESSAGES
        .iter()
        .map(|(author, text)| {
            let message = said(&page, text);
            let mut row = json!({ "author": author, "content": text });
            for field in ["content_html", "heading"] {
                if let Some(value) = message.get(field) {
                    row[field] = value.clone();
                }
            }
            row
        })
        .collect();
    json!(rows)
}

#[tokio::test]
async fn the_browser_check_draws_what_this_server_sends() {
    let expected = browser_messages().await;
    let path = fixture_path();
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&expected).expect("serializes")
    );
    if std::env::var_os("VIBE_TALK_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().expect("a directory")).expect("mkdir");
        std::fs::write(&path, &text).expect("write the fixture");
        return;
    }
    let held = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        held == text,
        "{} is not what the server sends for its messages; rewrite it with \
         VIBE_TALK_UPDATE_FIXTURES=1 cargo test --test rich_headers",
        path.display()
    );
    // Every message the browser draws with a heading has one, and the ones without do not.
    let headed = expected
        .as_array()
        .expect("rows")
        .iter()
        .filter(|row| row.get("heading").is_some())
        .count();
    assert_eq!(headed, BROWSER_MESSAGES.len() - 1, "{expected:#}");
}

#[test]
fn the_fixture_is_named_for_the_browser_check() {
    // The Python check reads this path; a rename on either side should fail here, not there.
    let script = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("rich_headers_browser.py"),
    )
    .expect("the browser check exists");
    assert!(script.contains("rich_headers_messages.json"));
}
