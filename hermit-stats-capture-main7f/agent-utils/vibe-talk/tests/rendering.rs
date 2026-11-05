//! `#217 markdown-blocks`: which answers carry a message's rendered body, and how they travel.
//!
//! The renderer and the sanitizer have their own tests in `src/render.rs`. These drive the real
//! router and pin down the wire: the routes only the page reads carry `content_html` and nothing
//! toward a model does; a plain sentence carries none; the provider's markup is the one its
//! configuration names; and the page, its script and its JSON travel compressed while the live
//! stream does not.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::chat::{ChatClient, ChatError, ChatIdentity};
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::http::router;
use vibe_talk::model::{ChannelId, Message, MessageId};
use vibe_talk::testing::{READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};
use vibe_talk::threads::{ThreadSummary, TimelinePage, TimelineRequest};

/// The owner's message from `#217 markdown-blocks`, in the shape that showed the defect.
const LIST_MESSAGE: &str = "Here is where things stand after the run:\n\n\
                            - the build is green on both runners\n\
                            - the flaky test is quarantined\n  and has an issue filed against it\n\
                            - docs are updated\n\
                            - the release notes are drafted\n\
                            - the tag is not pushed yet\n  because it waits on your review\n\
                            - nothing else is open\n\n\n\
                            I will push the tag when you say so.\n\nThanks!";
const PLAIN_MESSAGE: &str = "sounds good, I'll take a look";

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, headers, bytes.to_vec())
}

async fn get_json(router: &axum::Router, uri: &str) -> Value {
    let (status, _, bytes) = send(router, "GET", uri, Some(READ_TOKEN), &[], None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{uri}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).expect("json")
}

/// The in-memory Discord, answering timeline reads too: every view is its newest messages, and
/// the first of them is the root of the one thread a thread list shows, so a root is rendered.
struct Threaded(Arc<FakeDiscord>);

#[async_trait::async_trait]
impl ChatClient for Threaded {
    fn supports_threading(&self) -> bool {
        true
    }

    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        self.0.identity().await
    }

    async fn fetch_page(
        &self,
        channel: &ChannelId,
        limit: u16,
        before: Option<&MessageId>,
        after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        self.0.fetch_page(channel, limit, before, after).await
    }

    async fn post_message(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.0.post_message(channel, content, reply_to).await
    }

    async fn fetch_timeline(
        &self,
        channel: &ChannelId,
        request: &TimelineRequest,
    ) -> Result<TimelinePage, ChatError> {
        let messages = self
            .0
            .fetch_page(channel, request.limit, None, None)
            .await?;
        let root = messages.first().cloned();
        Ok(TimelinePage {
            threads: vec![ThreadSummary {
                id: "t1".to_owned(),
                root,
                title: "a thread".to_owned(),
                reply_count: Some(1),
                reply_count_exact: true,
                updated_at: "2026-10-08T12:00:00Z".to_owned(),
                display_name: None,
                summary: None,
            }],
            messages,
            has_threads: true,
            ..TimelinePage::default()
        })
    }
}

/// A server reading `discord` through [`Threaded`], from configuration `toml`.
fn threaded(toml: &str) -> (axum::Router, Arc<FakeDiscord>) {
    let (mut state, discord, _) = vibe_talk::testing::state_from_toml(toml);
    state.replace_chat(Arc::new(Threaded(discord.clone())));
    (router(state), discord)
}

/// A server whose lead channel holds the list message and a plain one, and its router.
fn seeded() -> (axum::Router, vibe_talk::state::AppState) {
    let (mut state, discord, _store) = vibe_talk::testing::state_with_store();
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    discord.seed(&channel, "claude-integ", LIST_MESSAGE);
    discord.seed(&channel, "codex-eng", PLAIN_MESSAGE);
    state.replace_chat(Arc::new(Threaded(discord)));
    (router(state.clone()), state)
}

/// The message with this text, out of an answer's `messages`.
fn with_text<'v>(body: &'v Value, text: &str) -> &'v Value {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["content"] == text)
        .unwrap_or_else(|| panic!("no message says {text:?} in {body}"))
}

#[tokio::test]
async fn a_timeline_read_carries_the_rendered_body_and_a_plain_sentence_carries_none() {
    let (router, _) = seeded();
    let threads = get_json(
        &router,
        &format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view=threads"),
    )
    .await;
    assert!(
        threads["threads"][0]["root"]["content_html"]
            .as_str()
            .is_some_and(|html| html.contains("<ul>")),
        "a thread's first message was not rendered: {threads}"
    );
    for view in ["flat", "main"] {
        let body = get_json(
            &router,
            &format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view={view}"),
        )
        .await;
        let list = with_text(&body, LIST_MESSAGE);
        let html = list["content_html"]
            .as_str()
            .expect("the list message is rendered");
        assert!(
            html.starts_with("<p>Here is where things stand after the run:</p>\n<ul>\n"),
            "{html}"
        );
        assert_eq!(html.matches("<li>").count(), 6, "{html}");
        assert!(
            html.ends_with("<p>I will push the tag when you say so.</p>\n<p>Thanks!</p>\n"),
            "{html}"
        );
        // The source rides along: Copy text keeps it, and the speech path falls back to it.
        assert_eq!(list["content"], LIST_MESSAGE);
        let plain = with_text(&body, PLAIN_MESSAGE);
        assert!(
            plain.get("content_html").is_none(),
            "a sentence rendering changes nothing still carried HTML: {plain}"
        );
    }
}

#[tokio::test]
async fn the_page_route_renders_only_when_asked_because_the_voice_agent_reads_it_too() {
    let (router, _) = seeded();
    let page = format!("/api/v1/channels/{WRITE_CHANNEL}/page");
    let unasked = get_json(&router, &page).await;
    assert!(
        with_text(&unasked, LIST_MESSAGE)
            .get("content_html")
            .is_none(),
        "read_page's answer carried HTML into a model's context"
    );
    let asked = get_json(&router, &format!("{page}?render=html")).await;
    assert!(with_text(&asked, LIST_MESSAGE)["content_html"]
        .as_str()
        .is_some_and(|html| html.contains("<ul>")));
    let (status, _, _) = send(
        &router,
        "GET",
        &format!("{page}?render=markdown"),
        Some(READ_TOKEN),
        &[],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unknown rendering was not refused"
    );
}

#[tokio::test]
async fn nothing_toward_a_model_carries_html() {
    // The scrollback route, one message by id, the digest, and the MCP tool that reads a page: each
    // is read by the voice agent or the plain page, and each says the message as text.
    let (router, _) = seeded();
    let messages = get_json(
        &router,
        &format!("/api/v1/channels/{WRITE_CHANNEL}/messages"),
    )
    .await;
    let list = with_text(&messages, LIST_MESSAGE);
    assert!(list.get("content_html").is_none(), "{list}");
    let id = list["id"].as_str().expect("id");
    let one = get_json(
        &router,
        &format!("/api/v1/channels/{WRITE_CHANNEL}/messages/{id}"),
    )
    .await;
    assert!(!one.to_string().contains("content_html"), "{one}");
    let digest = get_json(&router, &format!("/api/v1/channels/{WRITE_CHANNEL}/digest")).await;
    assert!(!digest.to_string().contains("<li>"), "{digest}");
    let (status, _, bytes) = send(
        &router,
        "POST",
        "/mcp",
        Some(READ_TOKEN),
        &[("accept", "application/json, text/event-stream")],
        Some(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "read_page", "arguments": { "channel_id": WRITE_CHANNEL } }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("the flaky test is quarantined"), "{text}");
    assert!(
        !text.contains("content_html") && !text.contains("<li>"),
        "{text}"
    );
}

#[tokio::test]
async fn the_to_do_list_and_a_posts_answer_carry_rendered_bodies() {
    let (router, _) = seeded();
    let todo = get_json(&router, &format!("/api/v1/channels/{WRITE_CHANNEL}/todo")).await;
    assert!(
        with_text(&todo, LIST_MESSAGE)["content_html"].is_string(),
        "{todo}"
    );
    let (status, _, bytes) = send(
        &router,
        "POST",
        &format!("/api/v1/channels/{WRITE_CHANNEL}/reply"),
        Some(WRITE_TOKEN),
        &[],
        Some(json!({ "text": "**done**, see <@123>" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let answer: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        answer["posted"]["content_html"],
        "<p><strong>done</strong>, see <span class=\"mention\">@123</span></p>\n"
    );
    assert_eq!(
        answer["parts"][0]["content_html"],
        answer["posted"]["content_html"]
    );
}

#[tokio::test]
async fn a_pin_carries_its_snapshot_rendered_and_a_cut_one_with_its_ellipsis() {
    let (router, _) = seeded();
    let pin = |id: &str, content: String| {
        let router = router.clone();
        let uri = format!("/api/v1/channels/{WRITE_CHANNEL}/pins/{id}");
        async move {
            let (status, _, bytes) = send(
                &router,
                "PUT",
                &uri,
                Some(WRITE_TOKEN),
                &[],
                Some(json!({
                    "author": "claude-integ", "author_id": "7", "author_is_bot": true,
                    "content": content, "timestamp": "2026-10-08T12:00:00Z",
                    "thread_id": null, "thread_root": false,
                })),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            serde_json::from_slice::<Value>(&bytes).expect("json")
        }
    };
    let changed = pin("1000000000000000101", "- one\n- two".to_owned()).await;
    assert_eq!(
        changed["pin"]["content_html"],
        "<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n"
    );
    let long = format!("- {}", "w".repeat(vibe_talk::store::MAX_PIN_TEXT_CHARS));
    let cut = pin("1000000000000000102", long).await;
    assert_eq!(cut["pin"]["truncated"], true);
    let html = cut["pin"]["content_html"].as_str().expect("rendered");
    assert!(
        html.ends_with("w\u{2026}</li>\n</ul>\n"),
        "the cut snapshot lost its ellipsis: {html}"
    );
    let list = get_json(&router, &format!("/api/v1/channels/{WRITE_CHANNEL}/pins")).await;
    assert_eq!(
        list["pins"][0]["content_html"],
        "<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n"
    );
}

#[tokio::test]
async fn a_live_event_carries_the_rendered_body_and_the_stream_is_never_compressed() {
    let (state, discord) = vibe_talk::testing::state();
    let router = router(state.clone());
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    let mut cursor = None;
    // The real poll tick, as `tests/live.rs` drives it: the first publishes nothing.
    discord.seed(&channel, "codex-eng", "already here");
    vibe_talk::live::poll_once(
        discord.as_ref(),
        state.live.as_ref(),
        &channel,
        50,
        &mut cursor,
    )
    .await
    .expect("the fake reads");
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/channels/{WRITE_CHANNEL}/stream"))
                .header("authorization", format!("Bearer {READ_TOKEN}"))
                .header("accept-encoding", "br, gzip")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("content-encoding").is_none(),
        "the live stream was compressed, so events would wait for a compressor's buffer"
    );
    let mut body = response.into_body();
    discord.seed(&channel, "claude-integ", "- a\n- b");
    let tick = vibe_talk::live::poll_once(
        discord.as_ref(),
        state.live.as_ref(),
        &channel,
        50,
        &mut cursor,
    )
    .await
    .expect("the fake reads");
    assert_eq!(tick.published, 1);
    let mut text = String::new();
    while !text.contains("\n\n") {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("an event within five seconds")
            .expect("the stream is open")
            .expect("a readable frame");
        if let Some(data) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(data));
        }
    }
    let payload: Value = text
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map(|json| serde_json::from_str(json.trim()).expect("json"))
        .expect("a data line");
    assert_eq!(payload["message"]["content"], "- a\n- b");
    assert_eq!(
        payload["message"]["content_html"],
        "<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n"
    );
}

#[tokio::test]
async fn a_provider_configured_as_google_chat_reads_one_asterisk_as_bold() {
    let toml = vibe_talk::testing::config_toml().replace(
        "bot_token = \"test-bot-token\"",
        "bot_token = \"test-bot-token\"\nmarkup = \"google-chat\"",
    );
    let (router, discord) = threaded(&toml);
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    discord.seed(
        &channel,
        "a person",
        "*bold* and _it_ and <https://example.com/d|the doc>",
    );
    let body = get_json(
        &router,
        &format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view=flat"),
    )
    .await;
    let html = body["messages"][0]["content_html"]
        .as_str()
        .expect("rendered");
    assert!(
        html.starts_with(
            "<p><strong>bold</strong> and <em>it</em> and <a href=\"https://example.com/d\""
        ),
        "{html}"
    );
    assert!(html.contains(">the doc</a>"), "{html}");

    // A misspelt markup is refused at startup, and so is one on a Slack provider, whose text is
    // Slack's by definition.
    let misspelt = vibe_talk::testing::config_toml().replace(
        "bot_token = \"test-bot-token\"",
        "bot_token = \"test-bot-token\"\nmarkup = \"slack-ish\"",
    );
    let no_env = std::collections::BTreeMap::new();
    assert!(vibe_talk::config::Config::from_toml_and_env(&misspelt, &no_env).is_err());
    let slack = format!(
        "{}\n[[providers]]\nkey = \"slack\"\nkind = \"slack\"\ntoken = \"xoxb-test\"\nmarkup = \"slack\"\n",
        vibe_talk::testing::config_toml()
    );
    let refused = vibe_talk::config::Config::from_toml_and_env(&slack, &no_env)
        .expect_err("markup on a Slack provider was accepted");
    assert!(
        refused.to_string().contains("providers.slack.markup"),
        "{refused}"
    );
}

/// The pull requests in the status line from `#227 github-link-abbrev`, with neutral names: each
/// address and the label the page should show for it.
const LANDED: [(&str, &str); 4] = [
    ("https://github.com/octo/gizmo/pull/3871", "gizmo#3871"),
    ("https://github.com/octo/gizmo/pull/3908", "gizmo#3908"),
    ("https://github.com/octo/sprocket/pull/969", "sprocket#969"),
    ("https://github.com/octo/sprocket/pull/974", "sprocket#974"),
];

/// That status line, as an agent writes it, with each address put in the shape `form` gives it.
fn status_line(form: fn(&str) -> String) -> String {
    let [a, b, c, d] = LANDED.map(|(address, _)| form(address));
    format!(
        "Landed (4): {a} (record of accept/accept4), {b} (fix the poll loop), {c} and {d} \
         (bump the toolchain)"
    )
}

#[tokio::test]
async fn a_status_line_of_pull_request_addresses_reaches_the_page_as_short_links_in_every_markup() {
    // The owner: "These PR URLs are annoying. … Just say REPO#123 as a hyperlink instead of the
    // bare url." (`REPO` stands for the repository he named.) Every markup a provider may be
    // configured with, and every form its text sends an address in: bare, in angle brackets, and
    // the chat services' address labelled with itself.
    let bare: fn(&str) -> String = str::to_owned;
    let angled: fn(&str) -> String = |address| format!("<{address}>");
    let labelled: fn(&str) -> String = |address| format!("<{address}|{address}>");
    for (markup, forms) in [
        (None, vec![bare, angled]),
        (Some("google-chat"), vec![bare, angled, labelled]),
        (Some("slack"), vec![bare, angled, labelled]),
    ] {
        let toml = match markup {
            None => vibe_talk::testing::config_toml(),
            Some(markup) => vibe_talk::testing::config_toml().replace(
                "bot_token = \"test-bot-token\"",
                &format!("bot_token = \"test-bot-token\"\nmarkup = \"{markup}\""),
            ),
        };
        let (router, discord) = threaded(&toml);
        let channel = ChannelId(WRITE_CHANNEL.to_owned());
        let written: Vec<String> = forms.iter().map(|form| status_line(*form)).collect();
        for text in &written {
            discord.seed(&channel, "an agent", text);
        }
        let body = get_json(
            &router,
            &format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view=flat"),
        )
        .await;
        for text in &written {
            let message = with_text(&body, text);
            let html = message["content_html"].as_str().expect("rendered");
            for (address, label) in LANDED {
                // The address exactly as written, twice — where it goes and what a hover says —
                // and the short label as the only thing on the screen.
                let link = format!(
                    "<a href=\"{address}\" title=\"{address}\" target=\"_blank\" \
                     rel=\"noopener noreferrer nofollow\">{label}</a>"
                );
                assert!(html.contains(&link), "{markup:?}: no {label} in {html}");
            }
            assert!(
                !html.contains(">https://"),
                "{markup:?}: an address is still on the screen: {html}"
            );
            assert!(
                html.contains("</a> (record of accept/accept4), <a "),
                "{markup:?}: the words between the links moved: {html}"
            );
            // The text the page copies and the voice falls back to is the message as written.
            assert_eq!(message["content"], text.as_str());
        }
    }
}

/// A response's body length, and its `content-encoding`.
async fn fetched(
    router: &axum::Router,
    uri: &str,
    token: Option<&str>,
    encoding: Option<&str>,
) -> (usize, Option<String>, HeaderMap) {
    let headers: Vec<(&str, &str)> = encoding
        .map(|e| ("accept-encoding", e))
        .into_iter()
        .collect();
    let (status, headers, bytes) = send(router, "GET", uri, token, &headers, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    let used = headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    (bytes.len(), used, headers)
}

#[tokio::test]
async fn the_page_its_script_and_its_json_travel_compressed_when_the_browser_offers() {
    let (router, _) = seeded();
    for uri in ["/voice", "/voice.js", "/voice.css", "/contract.js"] {
        let (plain, none, _) = fetched(&router, uri, None, None).await;
        assert_eq!(
            none, None,
            "{uri} was compressed for a request that offered nothing"
        );
        let (gzip, used, headers) = fetched(&router, uri, None, Some("gzip")).await;
        assert_eq!(used.as_deref(), Some("gzip"), "{uri}");
        assert!(gzip * 2 < plain, "{uri}: gzip {gzip} of {plain}");
        assert_eq!(
            headers.get("vary").and_then(|v| v.to_str().ok()),
            Some("accept-encoding")
        );
        let (br, used, _) = fetched(&router, uri, None, Some("br;q=1.0, gzip;q=0.8")).await;
        assert_eq!(used.as_deref(), Some("br"), "{uri}");
        assert!(br < plain, "{uri}: br {br} of {plain}");
    }
    let timeline = format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view=flat");
    let (_, used, headers) = fetched(&router, &timeline, Some(READ_TOKEN), Some("gzip, br")).await;
    assert_eq!(
        used.as_deref(),
        Some("br"),
        "the timeline's JSON was not compressed"
    );
    assert!(headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|kind| kind.starts_with("application/json")));
}

#[test]
fn only_text_the_page_reads_is_compressed() {
    use vibe_talk::http::compressible_type;
    for kind in [
        "text/html; charset=utf-8",
        "text/javascript; charset=utf-8",
        "text/css; charset=utf-8",
        "application/json",
        "application/manifest+json",
    ] {
        assert!(compressible_type(kind), "{kind}");
    }
    for kind in [
        "text/event-stream",
        "audio/mpeg",
        "audio/wav",
        "image/png",
        "application/octet-stream",
        "",
    ] {
        assert!(!compressible_type(kind), "{kind}");
    }
}

/// The sizes `#217 markdown-blocks` reports, printed rather than asserted: what the page's script
/// and a timeline cost on the wire, uncompressed and compressed. `--nocapture` to read them.
#[tokio::test]
async fn report_the_sizes_on_the_wire() {
    let (router, discord) = threaded(&vibe_talk::testing::config_toml());
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    // A day of an agent channel: mostly prose, some of it Markdown with a list or a link, no two
    // messages alike — repeated text would flatter the compressor.
    let topics = [
        "retry budget",
        "mac runner",
        "cache key",
        "release token",
        "nightly sweep",
        "ordering bug",
        "arm64 job",
        "docs build",
        "changelog script",
        "flaky test",
    ];
    for (round, topic) in topics.iter().enumerate() {
        let sha = format!("{:07x}", 0x4f2_1ab0 + round * 7919);
        discord.seed(
            &channel,
            "codex-eng",
            &format!(
            "pushed the {topic} change at {sha}; focused tests pass locally ({} of {}), and the \
             branch is ready for review once CI reports.", 10 + round, 10 + round),
        );
        discord.seed(
            &channel,
            "claude-integ",
            &format!(
            "Where the {topic} work stands:\n\n- CI is green on run {}\n- one review comment is \
             open\n  and waits on a reply\n- the docs mention it\n\nI will land it after the \
             review.", 7000 + round * 13),
        );
        discord.seed(
            &channel,
            "codex-review",
            &format!(
            "one finding on the {topic} change: see [the diff](https://example.com/diff/{sha}) \
             and `src/{}.rs`, line {}.", topic.replace(' ', "_"), 40 + round * 3),
        );
        discord.seed(
            &channel,
            "owner",
            &format!("thanks, the {topic} one can wait until tomorrow"),
        );
    }
    let timeline = format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view=flat&limit=40");
    let (identity, _, _) = fetched(&router, &timeline, Some(READ_TOKEN), None).await;
    let (gzip, _, _) = fetched(&router, &timeline, Some(READ_TOKEN), Some("gzip")).await;
    let (br, _, _) = fetched(&router, &timeline, Some(READ_TOKEN), Some("br")).await;
    let mut body = get_json(&router, &timeline).await;
    for message in body["messages"].as_array_mut().expect("messages") {
        message
            .as_object_mut()
            .expect("object")
            .remove("content_html");
    }
    let without = serde_json::to_vec(&body).expect("json").len();
    let (js, _, _) = fetched(&router, "/voice.js", None, None).await;
    let (js_gzip, _, _) = fetched(&router, "/voice.js", None, Some("gzip")).await;
    let (js_br, _, _) = fetched(&router, "/voice.js", None, Some("br")).await;
    println!(
        "sizes: timeline(40) without content_html {without} B; with it identity {identity} B, \
         gzip {gzip} B, br {br} B; voice.js identity {js} B, gzip {js_gzip} B, br {js_br} B"
    );
}
