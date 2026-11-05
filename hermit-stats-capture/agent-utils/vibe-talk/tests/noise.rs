//! `#196 auto-read-noise`: placeholder messages are read automatically, end to end.
//!
//! The owner's evidence was nine `_Working…_` placeholders in the latest fifty messages of his
//! main space, each a thread reply from an agent whose real answer arrived later. These tests pin
//! the claims the feature makes about them:
//!
//!   * **Everything that reads "to do" agrees.** The to-do list, the count, the digest and the
//!     agent's tools leave a placeholder out and SAY how many they left out; the channel reads mark
//!     it rather than drop it, because the page shows it dimmed.
//!   * **It is evaluated, never recorded.** An edit that turns the placeholder into the answer makes
//!     it unread again, and removing the rule un-hides everything it caught — without one store row
//!     changing. Nothing here writes a dismissal.
//!   * **The owner, and only the owner, decides.** The rules are edited with the write token and by
//!     no MCP tool at all; a false positive is rescued per message.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::http::router;
use vibe_talk::model::{ChannelId, MessageId};
use vibe_talk::ops;
use vibe_talk::state::AppState;
use vibe_talk::store::fake::FakeStore;
use vibe_talk::store::StateStore as _;
use vibe_talk::testing::{READ_CHANNEL, READ_TOKEN, WRITE_TOKEN};

/// The placeholder exactly as the evidence shows it: italics and a real ellipsis.
const PLACEHOLDER: &str = "_Working…_";
const AGENT: &str = "build-agent-bot";

struct Harness {
    state: AppState,
    router: axum::Router,
    discord: Arc<FakeDiscord>,
    store: Arc<FakeStore>,
    channel: ChannelId,
}

impl Harness {
    fn new() -> Self {
        let (state, discord, store) = vibe_talk::testing::state_with_store();
        Self {
            router: router(state.clone()),
            state,
            discord,
            store,
            channel: ChannelId(READ_CHANNEL.to_owned()),
        }
    }

    fn seed(&self, author: &str, content: &str) -> MessageId {
        self.discord.seed(&self.channel, author, content)
    }

    /// A small channel in the owner's shape: a question, two placeholders, and the answers.
    fn conversation(&self) -> Conversation {
        Conversation {
            question: self.seed("alice", "Can you find out why the nightly build is red?"),
            first: self.seed(AGENT, PLACEHOLDER),
            answer: self.seed(
                AGENT,
                "Working… done. The nightly build is red because a fixture expired.",
            ),
            second: self.seed(AGENT, "Working..."),
        }
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
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

    async fn set_rules(&self, rules: &[&str]) -> (StatusCode, Value) {
        self.call(
            "PUT",
            "/api/v1/noise-rules",
            WRITE_TOKEN,
            Some(json!({ "rules": rules })),
        )
        .await
    }

    async fn todo_ids(&self) -> Vec<MessageId> {
        ops::todo(&self.state, READ_CHANNEL, None)
            .await
            .expect("reads")
            .messages
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    async fn mcp(&self, token: &str, body: Value) -> Value {
        let (status, payload) = self.call("POST", "/mcp", token, Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{payload}");
        payload
    }

    async fn tool_text(&self, name: &str, arguments: Value) -> String {
        let payload = self
            .mcp(
                READ_TOKEN,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": arguments },
                }),
            )
            .await;
        payload["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text in {payload}"))
            .to_owned()
    }
}

struct Conversation {
    question: MessageId,
    first: MessageId,
    answer: MessageId,
    second: MessageId,
}

#[tokio::test]
async fn the_to_do_list_leaves_placeholders_out_and_says_how_many() {
    let harness = Harness::new();
    let c = harness.conversation();
    let view = ops::todo(&harness.state, READ_CHANNEL, None)
        .await
        .expect("reads");
    let ids: Vec<MessageId> = view.messages.iter().map(|m| m.id.clone()).collect();
    assert_eq!(
        ids,
        vec![c.question.clone(), c.answer.clone()],
        "a placeholder is not something to deal with"
    );
    assert_eq!(view.noise, 2, "the list must say what it left out");
    assert_eq!(view.window, 4, "the placeholders are still in the channel");
    // EVALUATED, NOT RECORDED. Nothing was dismissed to produce that list.
    assert!(
        harness
            .store
            .dismissals(&harness.channel)
            .await
            .expect("reads")
            .is_empty(),
        "matching a rule wrote a dismissal"
    );
}

#[tokio::test]
async fn the_channel_reads_mark_a_placeholder_rather_than_drop_it() {
    // The page draws the channel from these and shows a placeholder dimmed. Dropping it here would
    // leave the owner no way to see what was hidden or to rescue a false positive.
    let harness = Harness::new();
    let c = harness.conversation();
    let (status, page) = harness
        .call(
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/page"),
            READ_TOKEN,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let flags: Vec<(String, bool)> = page["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| {
            (
                m["id"].as_str().expect("id").to_owned(),
                m["noise"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        flags,
        vec![
            (c.question.0.clone(), false),
            (c.first.0.clone(), true),
            (c.answer.0.clone(), false),
            (c.second.0.clone(), true),
        ]
    );
    // An ordinary message carries no flag at all: absent is false, and the wire stays as it was.
    let plain = &page["messages"][0];
    assert!(
        plain.get("noise").is_none(),
        "an ordinary message grew a field: {plain}"
    );
    // The threaded timeline is pinned in tests/threads.rs, against a backend that has threads.
}

#[tokio::test]
async fn an_edited_placeholder_is_unread_again_without_any_record_changing() {
    // Some agents EDIT the placeholder into the answer. A dismissal written when it said
    // "Working…" would keep the finished answer out of the list forever.
    let harness = Harness::new();
    let c = harness.conversation();
    assert!(!harness.todo_ids().await.contains(&c.first));
    harness.discord.edit(
        &c.first,
        "Found it: the fixture certificate expired on the first of the month.",
    );
    assert!(
        harness.todo_ids().await.contains(&c.first),
        "an edited placeholder stayed hidden"
    );
}

#[tokio::test]
async fn removing_the_rule_un_hides_everything_it_caught() {
    let harness = Harness::new();
    let c = harness.conversation();
    assert_eq!(harness.todo_ids().await.len(), 2);
    let (status, body) = harness.set_rules(&[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["rules"],
        json!([]),
        "an empty list is how it is turned off"
    );
    assert_eq!(
        harness.todo_ids().await,
        vec![c.question, c.first, c.answer, c.second],
        "removing the rule must bring back every message it caught"
    );
    // ...and an empty list STAYS empty: the default does not creep back.
    assert_eq!(
        harness.store.noise_rules().await.expect("reads"),
        Some(Vec::new())
    );
}

#[tokio::test]
async fn clearing_the_backlog_dismisses_only_what_the_list_showed() {
    // The to-do list never shows a placeholder, so "clear everything up to this row" must not
    // write a dismissal for one. A dismissal is DURABLE: the placeholder would stay hidden after
    // the rule was removed, and an agent that later edited it into the real answer would find the
    // answer hidden as well — and the page, which counted the rows it showed, would promise to
    // clear two and report three.
    let harness = Harness::new();
    let question = harness.seed("alice", "Is the nightly build green?");
    let placeholder = harness.seed(AGENT, PLACEHOLDER);
    let answer = harness.seed(AGENT, "Green again since the fixture was renewed.");
    assert_eq!(
        harness.todo_ids().await,
        vec![question.clone(), answer.clone()]
    );
    let (status, cleared) = harness
        .call(
            "POST",
            &format!("/api/v1/channels/{READ_CHANNEL}/dismiss"),
            WRITE_TOKEN,
            Some(json!({ "through": answer.as_str() })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(
        cleared["messages"],
        json!([question.as_str(), answer.as_str()]),
        "the clear swept up a message the list never showed"
    );
    assert_eq!(cleared["count"], json!(2), "{cleared}");
    let stored = harness
        .store
        .dismissals(&harness.channel)
        .await
        .expect("reads");
    assert!(
        !stored.contains(&placeholder),
        "a placeholder was recorded as dealt with: {stored:?}"
    );
    // The consequence that matters: the placeholder edited into the answer is unread again.
    harness.discord.edit(
        &placeholder,
        "Found it: the fixture certificate expired on the first of the month.",
    );
    assert_eq!(
        harness.todo_ids().await,
        vec![placeholder],
        "an edited placeholder swept up by a clear stayed hidden"
    );
}

#[tokio::test]
async fn a_prefix_rule_catches_what_starts_that_way_and_nothing_else() {
    let harness = Harness::new();
    let thinking = harness.seed(AGENT, "Thinking about the cache layout…");
    let real = harness.seed(AGENT, "I was thinking we could shard by region.");
    let (status, _) = harness.set_rules(&["Working…", "Thinking*"]).await;
    assert_eq!(status, StatusCode::OK);
    let left = harness.todo_ids().await;
    assert!(
        !left.contains(&thinking),
        "a prefix rule missed its message"
    );
    assert!(
        left.contains(&real),
        "a prefix rule matched the middle of a message"
    );
}

#[tokio::test]
async fn the_count_and_the_digest_skip_placeholders_and_say_so() {
    let harness = Harness::new();
    let _c = harness.conversation();
    let (status, count) = harness
        .call(
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/count"),
            READ_TOKEN,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{count}");
    assert_eq!(count["counted"], json!(2), "{count}");
    assert_eq!(count["noise"], json!(2), "{count}");

    let (status, digest) = harness
        .call(
            "GET",
            &format!("/api/v1/channels/{READ_CHANNEL}/digest"),
            READ_TOKEN,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{digest}");
    let entries = digest["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "{digest}");
    assert!(
        entries.iter().all(|e| !e["summary"]
            .as_str()
            .unwrap_or("")
            .eq_ignore_ascii_case("working...")),
        "a placeholder got a digest line: {digest}"
    );
    assert_eq!(digest["noise"], json!(2), "{digest}");
}

#[tokio::test]
async fn the_agents_tools_leave_placeholders_out_and_say_how_many() {
    let harness = Harness::new();
    let c = harness.conversation();
    let digest = harness
        .tool_text("digest_channel", json!({ "channel_id": READ_CHANNEL }))
        .await;
    assert!(digest.contains(c.answer.as_str()), "{digest}");
    assert!(
        !digest.contains(c.first.as_str()),
        "the placeholder reached the agent: {digest}"
    );
    assert!(
        digest.contains("2 placeholder messages"),
        "the agent was not told what was left out: {digest}"
    );

    let page = harness
        .tool_text("read_page", json!({ "channel_id": READ_CHANNEL }))
        .await;
    assert!(page.contains("2 messages returned"), "{page}");
    assert!(!page.contains(c.second.as_str()), "{page}");
    assert!(page.contains("2 placeholder messages"), "{page}");

    let count = harness
        .tool_text("count_messages", json!({ "channel_id": READ_CHANNEL }))
        .await;
    assert!(count.contains("exactly 2 messages"), "{count}");
    assert!(
        count.contains("not counting 2 placeholder messages"),
        "{count}"
    );

    // A channel of NOTHING but placeholders is not "no messages in this channel".
    let quiet = Harness::new();
    quiet.seed(AGENT, PLACEHOLDER);
    let digest = quiet
        .tool_text("digest_channel", json!({ "channel_id": READ_CHANNEL }))
        .await;
    assert!(digest.contains("nothing to report"), "{digest}");
    assert!(!digest.contains("no messages in this channel"), "{digest}");
}

#[tokio::test]
async fn a_page_of_nothing_but_placeholders_still_says_how_to_step_back() {
    // A walk back with a small limit can land on a run of placeholders, and in the owner's space
    // they come in runs. "Nothing to report" WITHOUT the cursor ends the walk right there: the
    // real messages behind the run are then out of the agent's reach, which is hiding messages —
    // the one thing this feature must never do.
    let harness = Harness::new();
    let behind = harness.seed("alice", "The release notes are in the shared folder.");
    let oldest_placeholder = harness.seed(AGENT, PLACEHOLDER);
    harness.seed(AGENT, PLACEHOLDER);
    harness.seed(AGENT, "Working...");
    let page = harness
        .tool_text(
            "read_page",
            json!({ "channel_id": READ_CHANNEL, "limit": 3 }),
        )
        .await;
    assert!(page.contains("nothing to report"), "{page}");
    let cursor = format!("before={}", oldest_placeholder.as_str());
    assert!(
        page.contains(&cursor),
        "the walk was given no way past the placeholders: {page}"
    );
    // ...and the cursor it was given really does reach what is behind them.
    let next = harness
        .tool_text(
            "read_page",
            json!({
                "channel_id": READ_CHANNEL,
                "limit": 3,
                "before": oldest_placeholder.as_str(),
            }),
        )
        .await;
    assert!(next.contains(behind.as_str()), "{next}");
}

#[tokio::test]
async fn a_digest_steps_back_from_the_oldest_message_it_reached_placeholder_or_not() {
    // The same gap in the digest: a window of nothing but placeholders has no entry to take a
    // cursor from, and a short digest that says "there are older messages" without saying how to
    // reach them leaves the agent guessing a bigger limit.
    let harness = Harness::new();
    let behind = harness.seed("alice", "The release notes are in the shared folder.");
    let oldest_placeholder = harness.seed(AGENT, PLACEHOLDER);
    harness.seed(AGENT, PLACEHOLDER);
    harness.seed(AGENT, PLACEHOLDER);
    let digest = harness
        .tool_text(
            "digest_channel",
            json!({ "channel_id": READ_CHANNEL, "limit": 3 }),
        )
        .await;
    assert!(digest.contains("nothing to report"), "{digest}");
    assert!(!digest.contains(behind.as_str()), "{digest}");
    let step_back = format!("read_page with before={}", oldest_placeholder.as_str());
    assert!(
        digest.contains(&step_back),
        "the digest gave no way past the placeholders: {digest}"
    );

    // With a real message in the window too, the cursor is still the oldest message REACHED, not
    // the oldest one kept: stepping back from the kept one would hand the placeholder below it
    // over again.
    let harness = Harness::new();
    harness.seed("alice", "The release notes are in the shared folder.");
    let oldest_placeholder = harness.seed(AGENT, PLACEHOLDER);
    let answer = harness.seed(AGENT, "The notes moved to the team wiki last week.");
    let digest = harness
        .tool_text(
            "digest_channel",
            json!({ "channel_id": READ_CHANNEL, "limit": 2 }),
        )
        .await;
    assert!(digest.contains(answer.as_str()), "{digest}");
    let step_back = format!("read_page with before={}", oldest_placeholder.as_str());
    assert!(digest.contains(&step_back), "{digest}");
}

#[tokio::test]
async fn not_noise_rescues_one_message_and_leaves_the_rule_alone() {
    let harness = Harness::new();
    let c = harness.conversation();
    let (status, body) = harness
        .call(
            "POST",
            &format!("/api/v1/channels/{READ_CHANNEL}/not-noise"),
            WRITE_TOKEN,
            Some(json!({ "messages": [c.first.as_str()] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["messages"], json!([c.first.as_str()]));
    let left = harness.todo_ids().await;
    assert!(
        left.contains(&c.first),
        "the rescued message is still hidden"
    );
    assert!(
        !left.contains(&c.second),
        "rescuing one message weakened the rule for the others"
    );
    // An empty rescue is a refusal, not a success for having done nothing.
    let (status, _) = harness
        .call(
            "POST",
            &format!("/api/v1/channels/{READ_CHANNEL}/not-noise"),
            WRITE_TOKEN,
            Some(json!({ "messages": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A channel outside the allowlist cannot grow the store one row at a time.
    let (status, _) = harness
        .call(
            "POST",
            "/api/v1/channels/999999/not-noise",
            WRITE_TOKEN,
            Some(json!({ "messages": ["1"] })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_rules_are_read_at_read_scope_and_written_only_at_write_scope() {
    let harness = Harness::new();
    let (status, config) = harness
        .call("GET", "/api/v1/client-config", READ_TOKEN, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(
        config["noise_rules"]["rules"],
        json!(["Working…"]),
        "a fresh deployment starts with the placeholder rule"
    );
    assert!(
        config["noise_rules"]["matching"]
            .as_str()
            .is_some_and(|s| s.contains("whole text")),
        "the editor has no sentence saying how a rule matches: {config}"
    );
    // Reading the defaults wrote nothing: a read-scope credential writes nothing durable.
    assert_eq!(harness.store.noise_rules().await.expect("reads"), None);

    let (status, refused) = harness
        .call(
            "PUT",
            "/api/v1/noise-rules",
            READ_TOKEN,
            Some(json!({ "rules": ["anything"] })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    let (status, refused) = harness
        .call(
            "POST",
            &format!("/api/v1/channels/{READ_CHANNEL}/not-noise"),
            READ_TOKEN,
            Some(json!({ "messages": ["1"] })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(harness.store.noise_rules().await.expect("reads"), None);

    let (status, saved) = harness
        .set_rules(&["  Working…  ", "working...", "Thinking*"])
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        saved["rules"],
        json!(["Working…", "Thinking*"]),
        "the answer must be what was STORED, trimmed and de-duplicated"
    );
    let (_, config) = harness
        .call("GET", "/api/v1/client-config", READ_TOKEN, None)
        .await;
    assert_eq!(
        config["noise_rules"]["rules"],
        json!(["Working…", "Thinking*"])
    );

    let (status, refused) = harness.set_rules(&["*"]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "bad_id");
}

#[tokio::test]
async fn no_tool_can_edit_the_rules_whatever_the_credential() {
    // The scope is not the fence — a hosted agent is routinely handed the write token for
    // `post_reply`. The fence is that no tool exists.
    let harness = Harness::new();
    for token in [READ_TOKEN, WRITE_TOKEN] {
        let listed = harness
            .mcp(
                token,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
            )
            .await;
        let names: Vec<String> = listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| t["name"].as_str().expect("name").to_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.contains("noise") && !n.contains("rule")),
            "a tool can reach the noise rules: {names:?}"
        );
        for invented in ["set_noise_rules", "not_noise", "mark_noise"] {
            let refused = harness
                .mcp(
                    token,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/call",
                        "params": { "name": invented, "arguments": { "rules": ["anything"] } },
                    }),
                )
                .await;
            assert!(
                refused.get("error").is_some(),
                "{invented} was not refused: {refused}"
            );
        }
    }
    assert_eq!(harness.store.noise_rules().await.expect("reads"), None);
}

#[tokio::test]
async fn without_a_store_the_default_still_applies_and_an_edit_says_why_it_cannot_stick() {
    let (state, discord) =
        vibe_talk::testing::state_with(Arc::new(vibe_talk::store::disabled::DisabledStore));
    let channel = ChannelId(READ_CHANNEL.to_owned());
    discord.seed(&channel, AGENT, PLACEHOLDER);
    let window = ops::messages(&state, READ_CHANNEL, None)
        .await
        .expect("reads");
    assert!(window.messages[0].noise, "the default rule needs no store");
    let error = ops::set_noise_rules(&state, &["Thinking…".to_owned()])
        .await
        .expect_err("nothing to keep it in");
    assert_eq!(error.code(), "storage_not_configured", "{error}");
}

#[tokio::test]
async fn a_store_that_fails_hides_nothing() {
    // The direction to fail in: show the placeholders as they were before this existed, rather
    // than hide messages on the strength of a store that is not answering.
    let harness = Harness::new();
    harness.seed(AGENT, PLACEHOLDER);
    let window = ops::messages(&harness.state, READ_CHANNEL, None)
        .await
        .expect("reads");
    let placeholder = &window.messages[0];
    assert!(
        placeholder.noise,
        "the control: with the store answering, the rule applies"
    );
    // Armed immediately before the filter is built, so the failure lands on the rules read and
    // not on the alias read every channel operation makes first.
    harness.store.fail_next("the disk is full");
    let filter = vibe_talk::noise::filter_for(&harness.state, &harness.channel).await;
    assert!(
        !filter.is_noise(placeholder),
        "a failing store hid a message"
    );
    // ...and the next reading, with the store back, applies the rule again.
    let filter = vibe_talk::noise::filter_for(&harness.state, &harness.channel).await;
    assert!(filter.is_noise(placeholder));
}

/// Read SSE frames until `want` events have arrived, or fail by timing out.
async fn read_events(body: &mut Body, want: usize) -> String {
    let mut text = String::new();
    while text.matches("\n\n").count() < want {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .unwrap_or_else(|_| panic!("the stream produced nothing in five seconds: {text:?}"))
            .expect("the stream ended early")
            .expect("the frame is readable");
        if let Some(data) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(data));
        }
    }
    text
}

async fn open_stream(harness: &Harness) -> Body {
    let response = harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/channels/{READ_CHANNEL}/stream"))
                .header("authorization", format!("Bearer {READ_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body()
}

#[tokio::test]
async fn the_live_stream_judges_a_message_on_its_way_out_not_when_it_was_published() {
    let harness = Harness::new();
    let id = harness.seed(AGENT, PLACEHOLDER);
    let window = ops::messages(&harness.state, READ_CHANNEL, None)
        .await
        .expect("reads");
    let mut published = window.messages[0].clone();
    // Published carrying the WRONG answer on purpose: what an adapter sent is not trusted.
    published.noise = false;
    harness.state.live.publish(&harness.channel, published);

    let mut body = open_stream(&harness).await;
    let frame = read_events(&mut body, 1).await;
    assert!(frame.contains(id.as_str()), "{frame}");
    assert!(
        frame.contains("\"noise\":true"),
        "the stream did not judge it: {frame}"
    );

    // The rule removed AFTER publication: a page attaching now must be told the new answer.
    let (status, _) = harness.set_rules(&[]).await;
    assert_eq!(status, StatusCode::OK);
    let mut body = open_stream(&harness).await;
    let frame = read_events(&mut body, 1).await;
    assert!(frame.contains(id.as_str()), "{frame}");
    assert!(
        !frame.contains("\"noise\""),
        "the replay carried a verdict the rules no longer give: {frame}"
    );
}
