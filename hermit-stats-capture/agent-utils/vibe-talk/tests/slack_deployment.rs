//! A Slack-only deployment, assembled exactly as `main` assembles one: configuration text with a
//! `[[providers]]` entry of kind `slack`, the provider router built from it, and the HTTP routes
//! answering through the real Slack client against the loopback Slack fake.

mod slack_fake;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use slack_fake::{profiled, ts, Fake, FakeState, TOKEN};
use tower::ServiceExt as _;
use vibe_talk::providers::ChatRouter;
use vibe_talk::state::AppState;
use vibe_talk::testing::WRITE_TOKEN;

const TEAM: &str = "C0000MAIN1";
const OTHER: &str = "C0000OTHER";
const META_TAIL_UNIT: &str = include_str!("../systemd/vibe-talk-live-hint.service.example");

fn active_unit_value<'a>(unit: &'a str, key: &str) -> Vec<&'a str> {
    unit.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix(key))
        .filter_map(|value| value.strip_prefix('='))
        .collect()
}

#[test]
fn the_meta_tail_unit_pins_its_proven_runtime_constraints() {
    let exec_start = active_unit_value(META_TAIL_UNIT, "ExecStart");
    assert_eq!(
        exec_start.len(),
        1,
        "the unit needs one literal event source"
    );
    assert!(
        exec_start[0].contains(" -- /REPLACE_WITH_META_BIN/meta slack.conversation tail "),
        "the exceptional runtime envelope belongs to the concrete Meta tail command"
    );
    assert_eq!(active_unit_value(META_TAIL_UNIT, "MemoryHigh"), ["1G"]);
    assert_eq!(active_unit_value(META_TAIL_UNIT, "MemoryMax"), ["2G"]);
    assert_eq!(active_unit_value(META_TAIL_UNIT, "TasksMax"), ["2048"]);
    assert!(
        active_unit_value(META_TAIL_UNIT, "NoNewPrivileges").is_empty(),
        "Meta's managed runtime aborts under NoNewPrivileges"
    );
    assert!(
        active_unit_value(META_TAIL_UNIT, "PrivateTmp").is_empty(),
        "Meta's managed runtime aborts under PrivateTmp"
    );
    assert!(
        META_TAIL_UNIT.contains("These are Meta CLI managed-runtime budgets"),
        "the large Meta-specific budget must not look like a generic adapter requirement"
    );
}

fn config_text(api_base: &str, provider_extra: &str) -> String {
    format!(
        r#"
[auth]
read_token = "{read}"
write_token = "{WRITE_TOKEN}"

[[providers]]
key = "slack"
kind = "slack"
api_base = "{api_base}"
token = "{TOKEN}"
owner_user_id = "UOWNER001"
registered_channels_writable = true
{provider_extra}
[[channels]]
id = "{TEAM}"
label = "team"
writable = true
"#,
        read = vibe_talk::testing::READ_TOKEN,
    )
}

async fn deployment(fake: &Fake) -> AppState {
    deployment_with(fake, "").await
}

/// A deployment whose Slack provider entry carries `provider_extra` lines as well.
async fn deployment_with(fake: &Fake, provider_extra: &str) -> AppState {
    let text = config_text(&fake.base, provider_extra);
    let (mut state, _discord, _store) = vibe_talk::testing::state_with_store_from_toml(&text);
    let router = ChatRouter::from_config(&state.config).expect("providers build");
    state.replace_providers(Arc::new(router));
    state
}

async fn call(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {WRITE_TOKEN}"));
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
        None => builder.body(Body::empty()),
    }
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

fn fixture() -> FakeState {
    let mut state = FakeState::default();
    state.history.insert(
        TEAM.to_owned(),
        vec![
            profiled(
                &ts(20, 0),
                "UADA00001",
                "Ada",
                "second <https://example.com|link>",
            ),
            profiled(&ts(10, 0), "UGRACE001", "Grace", "first"),
        ],
    );
    state.history.insert(
        OTHER.to_owned(),
        vec![profiled(&ts(30, 0), "UADA00001", "Ada", "elsewhere")],
    );
    state
}

#[tokio::test]
async fn a_slack_provider_from_configuration_reads_and_describes_its_channels() {
    let fake = Fake::start(fixture(), 20).await;
    let state = deployment(&fake).await;

    let (status, config) = call(&state, "GET", "/api/v1/client-config", None).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["chat_provider_name"], "Slack");
    assert_eq!(config["channels"][0]["provider"], "slack");
    // One provider: the deployment-wide fields are its own, and no per-provider list is sent.
    assert!(config.get("providers").is_none(), "{config}");
    assert_eq!(config["threading_supported"], true);
    assert_eq!(config["owner_author_id"], "UOWNER001");

    let (status, page) = call(
        &state,
        "GET",
        &format!("/api/v1/channels/{TEAM}/page"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let contents: Vec<&str> = page["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(contents, ["first", "second [link](https://example.com)"]);
    assert_eq!(page["messages"][0]["author"], "Grace");
}

#[tokio::test]
async fn a_pasted_slack_link_adds_a_channel_through_the_slack_provider() {
    let fake = Fake::start(fixture(), 20).await;
    let state = deployment(&fake).await;
    let (status, body) = call(
        &state,
        "POST",
        "/api/v1/channels",
        Some(json!({
            "source": format!("https://acme.slack.com/archives/{OTHER}"),
            "label": "other",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channel"]["id"], OTHER);
    assert_eq!(body["channel"]["provider"], "slack");
    assert_eq!(body["channel"]["writable"], true);
    let (status, page) = call(
        &state,
        "GET",
        &format!("/api/v1/channels/{OTHER}/page"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.to_string().contains("elsewhere"));
}

#[tokio::test]
async fn clearing_unread_in_slack_is_offered_only_when_the_deployment_turns_it_on() {
    // `#258 slack-clear-unread`.
    let path = format!("/api/v1/channels/{TEAM}/upstream-clear");
    let fake = Fake::start(fixture(), 20).await;
    let state = deployment(&fake).await;
    let (_, config) = call(&state, "GET", "/api/v1/client-config", None).await;
    assert!(config.get("upstream_clear_supported").is_none(), "{config}");
    let (status, refused) = call(&state, "PUT", &path, Some(json!({"mode": "on"}))).await;
    assert!(status.is_client_error(), "{status} {refused}");

    let fake = Fake::start(fixture(), 20).await;
    let state = deployment_with(&fake, "upstream_clear = true\n").await;
    let (_, config) = call(&state, "GET", "/api/v1/client-config", None).await;
    assert_eq!(config["upstream_clear_supported"], true, "{config}");
    let (status, view) = call(&state, "PUT", &path, Some(json!({"mode": "dry_run"}))).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    // Settings words the control with this: "Clear unread in Slack".
    assert_eq!(view["provider_name"], "Slack");
    assert_eq!(view["supported"], true);
    assert_eq!(view["mode"], "dry_run");
    assert_eq!(
        fake.calls("conversations.mark"),
        0,
        "a dry run marks nothing"
    );
}

/// `#252 thread-button-state`, through the routes the page uses. A Slack message nobody has
/// answered heads its own thread, named by its `ts` with no replies; the thread opens with the
/// message alone in it; and the page's first send from that thread's box posts into it with
/// `thread_ts` = the message's `ts`, which is what starts the thread at Slack. The page is also told
/// that a Slack reply is a thread reply, so its reply screen offers no separate "new thread" box.
#[tokio::test]
async fn a_slack_message_nobody_has_answered_can_have_its_thread_started_from_the_page() {
    let fake = Fake::start(fixture(), 20).await;
    let state = deployment(&fake).await;

    let (status, config) = call(&state, "GET", "/api/v1/client-config", None).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["replies_in_thread"], true, "{config}");

    let (status, main) = call(
        &state,
        "GET",
        &format!("/api/v1/channels/{TEAM}/timeline?view=main"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{main}");
    let first = ts(10, 0);
    let message = main["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["content"] == "first")
        .expect("the message nobody has answered")
        .clone();
    assert_eq!(
        message["thread"],
        json!({"id": first, "root_message_id": message["id"], "is_root": true,
            "reply_count": 0, "reply_count_exact": true}),
        "{message}"
    );

    let (status, thread) = call(
        &state,
        "GET",
        &format!("/api/v1/channels/{TEAM}/timeline?view=thread&thread_id={first}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{thread}");
    let ids: Vec<&Value> = thread["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| &message["id"])
        .collect();
    assert_eq!(
        ids,
        [&message["id"]],
        "the empty thread is not the message alone"
    );
    assert_eq!(thread["thread"]["id"], first);
    assert_eq!(thread["thread"]["reply_count"], 0);

    let (status, sent) = call(
        &state,
        "POST",
        &format!("/api/v1/channels/{TEAM}/reply"),
        Some(json!({"text": "the first reply", "thread_id": first})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let posts = fake.requests("chat.postMessage");
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0]["thread_ts"], first,
        "the first reply did not start the message's thread"
    );
    assert_eq!(posts[0]["channel"], TEAM);
    assert_eq!(sent["posted"]["thread"]["id"], first);
    assert_eq!(sent["posted"]["thread"]["is_root"], false);
    assert_eq!(sent["posted"]["thread"]["root_message_id"], message["id"]);
}
