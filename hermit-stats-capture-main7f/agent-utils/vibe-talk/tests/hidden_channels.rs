//! `#199 removable-config-channels`: every channel on the list can be taken off it, including the
//! ones the configuration file names, end to end over the real router.
//!
//! The owner could not remove the deployment's one configured channel from his list, because the
//! server refused to delete a channel its file names and the page hid the button. The file is still
//! the statement of which channels exist and this server still never writes it; what changed is
//! that removing a configured channel HIDES it, durably, in the store. The claims pinned here:
//!
//!   * **Hidden means gone everywhere**, not only from one listing: the channel list, client-config,
//!     the inbox, the voice agent's tool descriptions and `list_channels`, and every channel-scoped
//!     route, which answer for it exactly as they answer for a channel that was never configured.
//!   * **It outlasts a restart**, and **it comes back** — through Settings' Show again, or through
//!     adding it again from the directory or by id, which is the act the owner will reach for.
//!   * **The agent cannot do either.** Write scope, and no tool — the `#39 channel-alias` posture.
//!   * **Nothing is held in memory only**: without a store the hide is refused by name.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::http::router;
use vibe_talk::model::ChannelId;
use vibe_talk::store::fake::FakeStore;
use vibe_talk::store::StateStore as _;
use vibe_talk::testing::{READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};

/// The configured labels, quoted rather than read from the fixture, so a listing that fell back to
/// something else could not pass by moving together with the expectation.
const READ_LABEL: &str = "build noise";
const WRITE_LABEL: &str = "lead team";

struct Harness {
    router: axum::Router,
    discord: Arc<FakeDiscord>,
    store: Arc<FakeStore>,
}

fn harness() -> Harness {
    let (state, discord, store) = vibe_talk::testing::state_with_store();
    Harness {
        router: router(state),
        discord,
        store,
    }
}

async fn call(
    harness: &Harness,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_router(&harness.router, method, uri, token, body).await
}

async fn call_router(
    router: &axum::Router,
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
    let response = router
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

async fn remove(harness: &Harness, channel: &str) -> (StatusCode, Value) {
    call(
        harness,
        "DELETE",
        &format!("/api/v1/channels/{channel}"),
        Some(WRITE_TOKEN),
        None,
    )
    .await
}

async fn show_again(harness: &Harness, channel: &str) -> (StatusCode, Value) {
    call(
        harness,
        "DELETE",
        &format!("/api/v1/channels/{channel}/hidden"),
        Some(WRITE_TOKEN),
        None,
    )
    .await
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap_or_else(|| panic!("not an array: {list}"))
        .iter()
        .map(|channel| channel["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// One MCP call, as a hosted voice agent makes it.
async fn mcp(harness: &Harness, token: &str, body: Value) -> Value {
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn removing_a_configured_channel_hides_it_from_every_listing_and_every_route() {
    let harness = harness();
    harness
        .discord
        .seed(&ChannelId(WRITE_CHANNEL.to_owned()), "codex-eng", "hello");

    let (status, body) = remove(&harness, WRITE_CHANNEL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channel"]["id"], WRITE_CHANNEL);
    assert_eq!(body["channel"]["label"], WRITE_LABEL);
    assert_eq!(ids(&body["channels"]), [READ_CHANNEL]);
    assert_eq!(ids(&body["hidden_channels"]), [WRITE_CHANNEL]);
    assert_eq!(
        harness.store.hidden_channels().await.expect("read back"),
        [ChannelId(WRITE_CHANNEL.to_owned())],
        "the hide was not made durable"
    );
    assert!(
        harness.discord.unregistration_calls().is_empty(),
        "hiding a configured channel reached upstream"
    );

    // Every listing, at the scope that reads it.
    let (_, listing) = call(&harness, "GET", "/api/v1/channels", Some(READ_TOKEN), None).await;
    assert_eq!(ids(&listing["channels"]), [READ_CHANNEL]);

    let (_, config) = call(
        &harness,
        "GET",
        "/api/v1/client-config",
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    assert_eq!(ids(&config["channels"]), [READ_CHANNEL]);
    assert_eq!(ids(&config["hidden_channels"]), [WRITE_CHANNEL]);
    let (_, read_config) = call(
        &harness,
        "GET",
        "/api/v1/client-config",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(
        read_config.get("hidden_channels"),
        None,
        "a read-scope caller was told the names of channels the owner put away: {read_config}"
    );

    let (status, inbox) = call(&harness, "GET", "/api/v1/inbox", Some(READ_TOKEN), None).await;
    assert_eq!(status, StatusCode::OK, "{inbox}");
    let inboxed: Vec<&str> = inbox["channels"]
        .as_array()
        .expect("inbox rows")
        .iter()
        .map(|row| row["channel"]["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(inboxed, [READ_CHANNEL]);

    let (_, tools) = call(
        &harness,
        "GET",
        "/api/v1/agent-tools",
        Some(READ_TOKEN),
        None,
    )
    .await;
    let rendered = tools.to_string();
    assert!(rendered.contains(READ_LABEL), "{rendered}");
    assert!(
        !rendered.contains(WRITE_LABEL),
        "the voice agent is still told about a hidden channel: {rendered}"
    );
    let listed = mcp(
        &harness,
        READ_TOKEN,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "list_channels", "arguments": {} },
        }),
    )
    .await;
    let text = listed["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("list_channels answered no text: {listed}"));
    assert!(text.contains(READ_LABEL), "{text}");
    assert!(!text.contains(WRITE_LABEL), "{text}");

    // Off the list means unreachable, as a removed added channel is — not merely unlisted.
    for (method, path, token, body) in [
        ("GET", "messages", READ_TOKEN, None),
        ("GET", "page", READ_TOKEN, None),
        ("GET", "todo", READ_TOKEN, None),
        ("GET", "stream", READ_TOKEN, None),
        (
            "POST",
            "reply",
            WRITE_TOKEN,
            Some(json!({ "text": "must not post" })),
        ),
    ] {
        let (status, refused) = call(
            &harness,
            method,
            &format!("/api/v1/channels/{WRITE_CHANNEL}/{path}"),
            Some(token),
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} still reaches a hidden channel: {refused}"
        );
    }
    assert!(
        harness.discord.posted().is_empty(),
        "a hidden channel was posted to"
    );
}

#[tokio::test]
async fn a_hidden_channel_stays_hidden_across_a_restart() {
    let store = Arc::new(FakeStore::new());
    let (first, _discord) = vibe_talk::testing::state_with(store.clone());
    let (status, body) = call_router(
        &router(first),
        "DELETE",
        &format!("/api/v1/channels/{READ_CHANNEL}"),
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A new process over the same store, restored the way `main` restores it.
    let (second, _discord) = vibe_talk::testing::state_with(store);
    assert_eq!(second.restore_hidden_channels().await.expect("restore"), 1);
    let (_, listing) = call_router(
        &router(second),
        "GET",
        "/api/v1/channels",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(
        ids(&listing["channels"]),
        [WRITE_CHANNEL],
        "the configuration file brought a hidden channel back on restart"
    );
}

#[tokio::test]
async fn show_again_puts_it_back_and_admits_nothing_the_file_does_not_name() {
    let harness = harness();
    harness
        .discord
        .seed(&ChannelId(WRITE_CHANNEL.to_owned()), "codex-eng", "hello");
    remove(&harness, WRITE_CHANNEL).await;
    // Hiding twice is a retry, not an error.
    let (status, again) = remove(&harness, WRITE_CHANNEL).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(ids(&again["hidden_channels"]), [WRITE_CHANNEL]);

    let (status, body) = show_again(&harness, WRITE_CHANNEL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channel"]["id"], WRITE_CHANNEL);
    assert_eq!(ids(&body["channels"]), [READ_CHANNEL, WRITE_CHANNEL]);
    assert_eq!(body["hidden_channels"], json!([]));
    assert!(harness
        .store
        .hidden_channels()
        .await
        .expect("read")
        .is_empty());
    let (status, page) = call(
        &harness,
        "GET",
        &format!("/api/v1/channels/{WRITE_CHANNEL}/messages"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "shown again but unreachable: {page}"
    );

    // Showing one that is not hidden answers the list as it stands.
    let (status, body) = show_again(&harness, WRITE_CHANNEL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body["channels"]), [READ_CHANNEL, WRITE_CHANNEL]);

    // The one channel route that addresses something outside the allowlist must not become a way
    // to admit a channel the operator never listed.
    let (status, refused) = show_again(&harness, "9999999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    assert_eq!(refused["error"], "unknown_channel");
    let (_, listing) = call(&harness, "GET", "/api/v1/channels", Some(READ_TOKEN), None).await;
    assert_eq!(ids(&listing["channels"]), [READ_CHANNEL, WRITE_CHANNEL]);
}

#[tokio::test]
async fn hiding_and_showing_need_the_write_scope_and_no_tool_reaches_either() {
    let harness = harness();
    for (path, what) in [
        (format!("/api/v1/channels/{READ_CHANNEL}"), "hid"),
        (format!("/api/v1/channels/{READ_CHANNEL}/hidden"), "showed"),
    ] {
        let (status, body) = call(&harness, "DELETE", &path, Some(READ_TOKEN), None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a read token {what} a channel: {body}"
        );
    }

    // The scope is not the guard against the agent — a hosted agent routinely holds the write
    // token for `post_reply` — so the tool list is checked at both scopes, and an invented name is
    // refused rather than routed.
    for token in [READ_TOKEN, WRITE_TOKEN] {
        let body = mcp(
            &harness,
            token,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .expect("a tool array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        assert!(names.contains(&"list_channels"), "{names:?}");
        for name in &names {
            assert!(
                !["hide", "show", "remove", "delete"]
                    .iter()
                    .any(|verb| name.contains(verb)),
                "{name} is offered to a model and looks like it edits the channel list: {names:?}"
            );
        }
    }
    for tool in [
        vibe_talk::send_approval::Approval::Ask,
        vibe_talk::send_approval::Approval::Direct,
    ]
    .into_iter()
    .flat_map(|approval| vibe_talk::mcp::tool_manifest(&[], approval))
    {
        assert!(
            !(tool.method == "DELETE" && tool.path.starts_with("/api/v1/channels")),
            "{} reaches a channel-list route",
            tool.name
        );
    }
    for invented in [
        "remove_channel",
        "hide_channel",
        "show_channel",
        "unhide_channel",
    ] {
        let body = mcp(
            &harness,
            WRITE_TOKEN,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": invented, "arguments": { "channel_id": READ_CHANNEL } },
            }),
        )
        .await;
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("unknown tool")),
            "{invented} was not refused: {body}"
        );
    }
    assert!(harness
        .store
        .hidden_channels()
        .await
        .expect("read")
        .is_empty());
}

#[tokio::test]
async fn adding_a_hidden_configured_channel_by_id_shows_it_again_on_the_files_terms() {
    let harness = harness();
    remove(&harness, WRITE_CHANNEL).await;

    // The request's own label and write policy are not the file's, and the file owns the channel.
    let (status, body) = call(
        &harness,
        "POST",
        "/api/v1/channels",
        Some(WRITE_TOKEN),
        Some(json!({ "id": WRITE_CHANNEL, "label": "typed again", "writable": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channel"]["label"], WRITE_LABEL);
    assert_eq!(body["channel"]["writable"], true);
    assert_eq!(body["channel"]["added"], false);
    assert_eq!(ids(&body["channels"]), [READ_CHANNEL, WRITE_CHANNEL]);
    assert!(harness
        .store
        .hidden_channels()
        .await
        .expect("read")
        .is_empty());
    assert!(
        harness
            .store
            .added_channels()
            .await
            .expect("read")
            .is_empty(),
        "showing a configured channel again turned it into an added one"
    );

    // While it is listed, adding it is the conflict it always was.
    let (status, refused) = call(
        &harness,
        "POST",
        "/api/v1/channels",
        Some(WRITE_TOKEN),
        Some(json!({ "id": WRITE_CHANNEL, "label": "twice" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
}

#[tokio::test]
async fn the_directory_offers_a_hidden_configured_channel_and_adding_it_shows_it_again() {
    let harness = harness();
    let write = ChannelId(WRITE_CHANNEL.to_owned());
    // The bridge resolves the browsed source to the configured channel's id, as a bridge that
    // already knows the channel does.
    harness
        .discord
        .enable_channel_registration(&write, false, true);
    harness.discord.enable_channel_directory(
        vec![vibe_talk::directory::DirectoryEntry {
            source: "room/lead".to_owned(),
            name: "Lead Team".to_owned(),
            registered_channel_id: Some(write.clone()),
        }],
        false,
    );
    remove(&harness, WRITE_CHANNEL).await;

    let (status, page) = call(
        &harness,
        "GET",
        "/api/v1/channel-directory",
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["entries"][0]["tracked"], false,
        "a hidden channel was shown as already on the list, with no way to add it back: {page}"
    );

    let (status, body) = call(
        &harness,
        "POST",
        "/api/v1/channels",
        Some(WRITE_TOKEN),
        Some(json!({ "source": "room/lead", "label": "Lead Team" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body["channels"]), [READ_CHANNEL, WRITE_CHANNEL]);
    assert!(harness
        .store
        .hidden_channels()
        .await
        .expect("read")
        .is_empty());
    assert!(
        harness.discord.unregistration_calls().is_empty(),
        "a configured channel was deleted upstream"
    );

    // A source that IS the configured id needs no registration at all.
    remove(&harness, WRITE_CHANNEL).await;
    let calls_before = harness.discord.registration_calls().len();
    let (status, body) = call(
        &harness,
        "POST",
        "/api/v1/channels",
        Some(WRITE_TOKEN),
        Some(json!({ "source": WRITE_CHANNEL, "label": "Lead Team" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body["channels"]), [READ_CHANNEL, WRITE_CHANNEL]);
    assert_eq!(
        harness.discord.registration_calls().len(),
        calls_before,
        "showing a configured channel again registered it upstream"
    );
}

#[tokio::test]
async fn without_a_store_a_configured_channel_is_not_hidden_until_the_next_restart() {
    let (state, _discord) =
        vibe_talk::testing::state_with(Arc::new(vibe_talk::store::disabled::DisabledStore));
    let router = router(state);
    let (status, refused) = call_router(
        &router,
        "DELETE",
        &format!("/api/v1/channels/{WRITE_CHANNEL}"),
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{refused}");
    assert_eq!(refused["error"], "storage_not_configured");
    let (_, listing) =
        call_router(&router, "GET", "/api/v1/channels", Some(READ_TOKEN), None).await;
    assert_eq!(
        ids(&listing["channels"]),
        [READ_CHANNEL, WRITE_CHANNEL],
        "a refused hide took effect in memory anyway"
    );
}

#[tokio::test]
async fn with_every_channel_hidden_the_server_still_answers_and_one_can_be_added() {
    let harness = harness();
    remove(&harness, READ_CHANNEL).await;
    let (status, body) = remove(&harness, WRITE_CHANNEL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channels"], json!([]));
    assert_eq!(ids(&body["hidden_channels"]), [READ_CHANNEL, WRITE_CHANNEL]);

    let (status, config) = call(
        &harness,
        "GET",
        "/api/v1/client-config",
        Some(WRITE_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["channels"], json!([]));
    let tools = mcp(
        &harness,
        WRITE_TOKEN,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    assert!(
        tools["result"]["tools"].is_array(),
        "an empty list broke the tool manifest: {tools}"
    );

    let fresh = "7777777777777777777";
    harness
        .discord
        .seed(&ChannelId(fresh.to_owned()), "codex-eng", "hello");
    let (status, added) = call(
        &harness,
        "POST",
        "/api/v1/channels",
        Some(WRITE_TOKEN),
        Some(json!({ "id": fresh, "label": "second team" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(ids(&added["channels"]), [fresh]);
    assert_eq!(
        ids(&added["hidden_channels"]),
        [READ_CHANNEL, WRITE_CHANNEL]
    );
}

/// A message summary, a thread sentence and a thread name, filed for `channel`.
fn summary_rows(channel: &ChannelId) -> [vibe_talk::store::SummaryKey; 3] {
    let key = |message: &str, version: &str| vibe_talk::store::SummaryKey {
        channel: channel.clone(),
        message: vibe_talk::model::MessageId(message.to_owned()),
        content_hash: 7,
        version: version.to_owned(),
    };
    [
        key("1000000000000000100", "v2-current"),
        key("thread-sentence:opaque/thread-A", "v2-current"),
        key(
            "thread-name:opaque/thread-A",
            vibe_talk::store::KEPT_VERSION,
        ),
    ]
}

/// Which of `channel`'s three rows are still filed: summary, sentence, name.
async fn filed(harness: &Harness, channel: &ChannelId) -> [bool; 3] {
    let mut held = [false; 3];
    for (slot, key) in summary_rows(channel).iter().enumerate() {
        held[slot] = harness
            .store
            .cached_summary(key)
            .await
            .expect("readable")
            .is_some();
    }
    held
}

#[tokio::test]
async fn taking_a_channel_off_the_list_deletes_its_summaries_and_keeps_its_thread_names() {
    // `#208 persistent-cache`. A summary is a second at-rest copy of a channel's text, so a
    // channel taken off the list takes its summaries and thread sentences with it — whether it is
    // a configured one being hidden or an added one being removed. Its thread NAMES stay, so it
    // comes back under the names the owner knew. Every other channel is untouched.
    let harness = harness();
    let added = ChannelId("3333333333".to_owned());
    harness.discord.register_channel(&added);
    let (status, body) = call(
        &harness,
        "POST",
        "/api/v1/channels",
        Some(WRITE_TOKEN),
        Some(json!({"id": added.as_str(), "label": "side project"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let configured = ChannelId(READ_CHANNEL.to_owned());
    let kept = ChannelId(WRITE_CHANNEL.to_owned());
    for channel in [&configured, &added, &kept] {
        for key in summary_rows(channel) {
            harness
                .store
                .cache_summary(&key, "somebody else's words, shortened", None)
                .await
                .expect("file");
        }
    }

    let (status, body) = remove(&harness, READ_CHANNEL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        filed(&harness, &configured).await,
        [false, false, true],
        "hiding a configured channel kept its summaries, or lost its thread names"
    );
    assert_eq!(filed(&harness, &added).await, [true, true, true]);
    assert_eq!(filed(&harness, &kept).await, [true, true, true]);

    let (status, body) = remove(&harness, added.as_str()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        filed(&harness, &added).await,
        [false, false, true],
        "removing an added channel kept its summaries, or lost its thread names"
    );
    assert_eq!(
        filed(&harness, &kept).await,
        [true, true, true],
        "a channel still on the list lost its summaries"
    );
}
