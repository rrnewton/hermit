//! End-to-end contract for adapter-to-server live-event ingestion.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::http::router;
use vibe_talk::live::{LiveHub, LiveKind};
use vibe_talk::model::ChannelId;
use vibe_talk::state::AppState;
use vibe_talk::testing::{READ_CHANNEL, READ_TOKEN, WRITE_TOKEN};

const INGEST_TOKEN: &str = "test-ingest-token-0000000000";

fn enabled() -> (axum::Router, std::sync::Arc<LiveHub>) {
    let text = format!(
        "{}\n[ingest]\ntoken = \"{INGEST_TOKEN}\"\n",
        vibe_talk::testing::config_toml()
    );
    let (state, _chat, _voice) = vibe_talk::testing::state_from_toml(&text);
    let live = std::sync::Arc::clone(&state.live);
    (router(state), live)
}

fn message(id: &str, content: &str) -> Value {
    json!({
        "id": id,
        "channel_id": READ_CHANNEL,
        "author": "adapter author",
        "author_id": "42",
        "author_is_bot": false,
        "timestamp": "2026-08-20T10:00:00+00:00",
        "spoken_time": "an adapter must not choose this",
        "reply_to": null,
        "content": content,
        "spoken_content": "nor this",
    })
}

async fn post(app: &axum::Router, token: Option<&str>, payload: Value) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/live/events")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(payload.to_string()))
                .expect("request"),
        )
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("finite response")
        .to_bytes();
    let payload = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, payload)
}

async fn post_hint(
    app: &axum::Router,
    token: Option<&str>,
    channel_id: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/live/hints")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(json!({"channel_id": channel_id}).to_string()))
                .expect("request"),
        )
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("finite response")
        .to_bytes();
    let payload = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, payload)
}

fn hints_enabled(
    poll_seconds: u64,
) -> (
    axum::Router,
    AppState,
    std::sync::Arc<vibe_talk::discord::fake::FakeDiscord>,
) {
    let text = format!(
        "{}\n[ingest]\ntoken = \"{INGEST_TOKEN}\"\n",
        vibe_talk::testing::config_toml().replace(
            "[discord]",
            &format!("[discord]\nlive_poll_seconds = {poll_seconds}"),
        )
    );
    let (state, chat, _voice) = vibe_talk::testing::state_from_toml(&text);
    (router(state.clone()), state, chat)
}

fn create(event_id: &str, id: &str, historical: bool) -> Value {
    json!({
        "event_id": event_id,
        "historical": historical,
        "kind": "create",
        "message": message(id, "arrived through push"),
    })
}

#[tokio::test]
async fn ingestion_is_disabled_by_default_and_has_its_own_credential() {
    let (state, _chat) = vibe_talk::testing::state();
    let disabled = router(state);
    let (status, payload) = post(&disabled, Some(WRITE_TOKEN), create("e1", "10", false)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["error"], "unauthenticated");
    let (status, payload) = post_hint(&disabled, None, READ_CHANNEL).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["error"], "unauthenticated");

    let response = disabled
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/client-config")
                .header("authorization", format!("Bearer {READ_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router responds");
    let payload: Value = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes(),
    )
    .expect("client config json");
    assert_eq!(payload["live_delivery"], "off");

    let (app, _live) = enabled();
    for token in [
        None,
        Some(READ_TOKEN),
        Some(WRITE_TOKEN),
        Some("wrong-token"),
    ] {
        let (status, payload) = post(&app, token, create("e1", "10", false)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"], "unauthenticated");
    }

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/client-config")
                .header("authorization", format!("Bearer {READ_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router responds");
    let payload: Value = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes(),
    )
    .expect("client config json");
    assert_eq!(payload["live_delivery"], "push");
}

#[tokio::test]
async fn a_hint_wakes_the_one_authoritative_cursor_and_duplicate_hints_coalesce() {
    let (app, state, chat) = hints_enabled(3_600);
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let poller = tokio::spawn(vibe_talk::live::poll_forever(
        state.clone(),
        None,
        state.chat.clone(),
        50,
        Duration::from_secs(3_600),
    ));
    for _ in 0..100 {
        if chat.fetch_count() >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        chat.fetch_count(),
        2,
        "the startup tick did not seed both channels"
    );
    let mut subscriber = state.live.subscribe(&channel, None).receiver;
    chat.seed(&channel, "someone", "arrived between reconciliation ticks");

    let first = post_hint(&app, Some(INGEST_TOKEN), READ_CHANNEL).await;
    assert_eq!(first.0, StatusCode::ACCEPTED);
    assert_eq!(first.1, json!({"accepted": true, "duplicate": false}));
    // It is deliberately immaterial whether the poll task has taken the first hint before this
    // request lands: either it is folded into the pending set (200) or into the 100 ms burst
    // window (202). The cursor, asserted below, is what guarantees one message delivery.
    let duplicate = post_hint(&app, Some(INGEST_TOKEN), READ_CHANNEL).await;
    assert!(
        matches!(duplicate.0, StatusCode::OK | StatusCode::ACCEPTED),
        "{duplicate:?}"
    );

    let published = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("the hint did not wake the provider before its one-hour interval")
        .expect("the hub stayed open");
    assert_eq!(
        published.message.content,
        "arrived between reconciliation ticks"
    );
    assert!(
        subscriber.try_recv().is_err(),
        "two hints published one provider message twice"
    );
    poller.abort();
}

#[tokio::test]
async fn hints_need_the_ingest_credential_and_an_existing_poll_cursor() {
    let (polled, _state, _chat) = hints_enabled(30);
    for token in [
        None,
        Some(READ_TOKEN),
        Some(WRITE_TOKEN),
        Some("wrong-token"),
    ] {
        let (status, payload) = post_hint(&polled, token, READ_CHANNEL).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"], "unauthenticated");
    }
    // Without a running poll task the pending set is stable, so its acknowledgement contract is
    // deterministic: the first hint queues and a repeat coalesces.
    let first = post_hint(&polled, Some(INGEST_TOKEN), READ_CHANNEL).await;
    assert_eq!(first.0, StatusCode::ACCEPTED);
    assert_eq!(first.1, json!({"accepted": true, "duplicate": false}));
    let duplicate = post_hint(&polled, Some(INGEST_TOKEN), READ_CHANNEL).await;
    assert_eq!(duplicate.0, StatusCode::OK);
    assert_eq!(duplicate.1, json!({"accepted": false, "duplicate": true}));
    let unknown = post_hint(&polled, Some(INGEST_TOKEN), "9999999999").await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);
    assert_eq!(unknown.1["error"], "unknown_channel");

    let (unpolled, _state, _chat) = hints_enabled(0);
    let absent = post_hint(&unpolled, Some(INGEST_TOKEN), READ_CHANNEL).await;
    assert_eq!(absent.0, StatusCode::CONFLICT);
    assert_eq!(absent.1["error"], "channel_is_not_polled");

    // Full pushed events remain a second producer and are still refused beside polling. Hints are
    // safe precisely because they carry no message and wake the poller's own cursor instead.
    let pushed = post(
        &polled,
        Some(INGEST_TOKEN),
        create("second-source", "10", false),
    )
    .await;
    assert_eq!(pushed.0, StatusCode::CONFLICT);
    assert_eq!(pushed.1["error"], "channel_is_polled");
}

#[tokio::test]
async fn accepted_events_are_typed_stamped_and_duplicate_retries_are_no_ops() {
    let (app, live) = enabled();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut subscription = live.subscribe(&channel, None);

    let (status, payload) = post(&app, Some(INGEST_TOKEN), create("event-1", "10", false)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(payload, json!({"accepted": true, "duplicate": false}));
    let created = subscription.receiver.try_recv().expect("create broadcast");
    assert_eq!(created.event_id, "push:event-1");
    assert_eq!(created.kind, LiveKind::Create);
    assert!(!created.historical);
    assert_eq!(created.message.spoken_time, "10:00");

    let (status, payload) = post(&app, Some(INGEST_TOKEN), create("event-1", "10", false)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({"accepted": false, "duplicate": true}));
    assert!(
        subscription.receiver.try_recv().is_err(),
        "an adapter retry was broadcast twice"
    );

    let update = json!({
        "event_id": "event-2",
        "historical": true,
        "kind": "update",
        "message": message("10", "edited through push"),
    });
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), update).await.0,
        StatusCode::ACCEPTED
    );
    let updated = subscription.receiver.try_recv().expect("update broadcast");
    assert_eq!(updated.kind, LiveKind::Update);
    assert!(updated.historical);

    let delete = json!({
        "event_id": "event-3",
        "historical": false,
        "kind": "delete",
        "channel_id": READ_CHANNEL,
        "message_id": "10",
    });
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), delete).await.0,
        StatusCode::ACCEPTED
    );
    let deleted = subscription.receiver.try_recv().expect("delete broadcast");
    assert_eq!(deleted.kind, LiveKind::Delete);
    assert_eq!(deleted.message.id.as_str(), "10");
}

#[tokio::test]
async fn a_pushed_message_arrives_carrying_the_body_a_voice_should_say() {
    // The live path is the one `read new` relays from, and the browser builds that turn out of
    // what arrives here. So this is where the preparation has to already have happened: there is
    // no later server hop to do it on, and asking the page for one would put a round trip on the
    // most latency-sensitive leg there is.
    let (app, live) = enabled();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut subscription = live.subscribe(&channel, None);

    let event = create("event-said", "10", false);
    let mut event = event;
    event["message"]["content"] =
        json!("**shipped** 1000000000000000009 at 2026-08-20T07:00:00+00:00");
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), event).await.0,
        StatusCode::ACCEPTED
    );

    let said = subscription
        .receiver
        .try_recv()
        .expect("create broadcast")
        .message;
    assert!(
        !said.spoken_content.contains("1000000000000000009"),
        "a nineteen-digit id reached the page's relay: {}",
        said.spoken_content
    );
    assert!(
        said.spoken_content.contains("UID ending in 009"),
        "the id was removed rather than named, so the listener cannot tell it from another: {}",
        said.spoken_content
    );
    assert!(
        !said.spoken_content.contains("**"),
        "markdown survived into the spoken body: {}",
        said.spoken_content
    );
    assert!(
        said.spoken_content.contains("shipped"),
        "the words themselves were lost: {}",
        said.spoken_content
    );
    assert!(
        said.content.contains("**shipped** 1000000000000000009"),
        "the raw body must survive UNTOUCHED beside the prepared one — it is what the screen \
         shows, and it is what makes the UID suffix recoverable"
    );

    // And an ordinary sentence carries NOTHING extra. Most chat has no markdown, no timestamp and
    // no id in it, so its prepared form is the identity — sending a second copy of every such
    // message down a phone's connection would buy the listener nothing at all. The field is
    // present only when something was genuinely rewritten, which is also what makes it readable
    // in a payload dump.
    let mut plain = create("event-plain", "12", false);
    plain["message"]["content"] = json!("the tag is cut");
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), plain).await.0,
        StatusCode::ACCEPTED
    );
    let ordinary = subscription.receiver.try_recv().expect("plain").message;
    assert_eq!(
        ordinary.spoken_content, "",
        "an unchanged body was sent twice"
    );
    assert_eq!(
        ordinary.spoken_body(),
        "the tag is cut",
        "and an empty field still has to READ as the raw body, or the message is never spoken"
    );
    assert_eq!(
        serde_json::to_value(&ordinary)
            .expect("serializes")
            .get("spoken_content"),
        None,
        "an empty spoken body must not even appear on the wire"
    );
}

#[tokio::test]
async fn a_rendered_body_an_adapter_sends_never_reaches_the_hub_or_the_page() {
    // `#217 markdown-blocks`. `content_html` is HTML the page inserts as it stands, so this
    // server's sanitizer is the only writer it may have. The stream renders every message again on
    // its way out, but that is one consumer's habit, not a property of the hub: whatever reads the
    // hub next must not find an adapter's markup waiting in it.
    let (app, live) = enabled();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut subscription = live.subscribe(&channel, None);
    let stream = open_stream(&app, None).await;
    assert_eq!(stream.status(), StatusCode::OK);
    let mut body = stream.into_body();

    let smuggled = "<img src=x onerror=alert(1)><a href=\"javascript:alert(2)\">x</a>";
    let mut event = create("event-html", "10", false);
    event["message"]["content"] = json!("- a\n- b");
    event["message"]["content_html"] = json!(smuggled);
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), event).await.0,
        StatusCode::ACCEPTED
    );
    let held = subscription
        .receiver
        .try_recv()
        .expect("create broadcast")
        .message;
    assert_eq!(held.content_html, "", "an adapter's HTML entered the hub");

    let text = read_events(&mut body, 1).await;
    let payload: Value = text
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map(|json| serde_json::from_str(json.trim()).expect("json"))
        .expect("a data line");
    assert_eq!(
        payload["message"]["content_html"], "<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n",
        "the page was not sent this server's own rendering of the text"
    );
}

#[tokio::test]
async fn uid_endings_are_stable_across_two_pushed_messages() {
    // The same compact UID form is used for independent live arrivals.
    let (app, live) = enabled();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut subscription = live.subscribe(&channel, None);

    let mut first = create("event-a", "10", false);
    first["message"]["content"] = json!("1000000000000000009 opened it");
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), first).await.0,
        StatusCode::ACCEPTED
    );
    let mut second = create("event-b", "11", false);
    second["message"]["content"] = json!("1000000000000000017 replied, 1000000000000000009 merged");
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), second).await.0,
        StatusCode::ACCEPTED
    );

    let one = subscription.receiver.try_recv().expect("first").message;
    let two = subscription.receiver.try_recv().expect("second").message;
    assert_eq!(one.spoken_content, "UID ending in 009 opened it");
    assert_eq!(
        two.spoken_content,
        "UID ending in 017 replied, UID ending in 009 merged"
    );
}

#[tokio::test]
async fn turning_the_speech_preparation_off_relays_exactly_what_was_typed() {
    // The control arm. Preparing a body is a judgement about what a listener wants, so an operator
    // tuning a voice agent's prompt has to be able to hear the same deployment both ways —
    // otherwise every difference between two runs is confounded with this pass.
    //
    // Off means the field is EMPTY, not a second copy of the raw body. That is the same value a
    // message carries before anything has touched it, so every reader is already correct: the page
    // falls back, `spoken_body()` falls back, and nothing on the wire changes shape.
    let text = format!(
        "{}\n[ingest]\ntoken = \"{INGEST_TOKEN}\"\n[speakable]\nenabled = false\n",
        vibe_talk::testing::config_toml()
    );
    let (state, _chat, _voice) = vibe_talk::testing::state_from_toml(&text);
    let live = std::sync::Arc::clone(&state.live);
    let app = router(state);
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut subscription = live.subscribe(&channel, None);

    let mut event = create("event-off", "10", false);
    let raw = "**shipped** 1000000000000000009 at 2026-08-20T07:00:00+00:00";
    event["message"]["content"] = json!(raw);
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), event).await.0,
        StatusCode::ACCEPTED
    );

    let said = subscription.receiver.try_recv().expect("create").message;
    assert_eq!(
        said.spoken_content, "",
        "the switch is off and a prepared body was sent anyway"
    );
    assert_eq!(said.spoken_body(), raw, "the raw body is what gets said");
    assert_eq!(said.content, raw, "and it is still what the screen shows");
    assert_eq!(
        said.spoken_time, "10:00",
        "the switch must NOT take the zone conversion with it: that is not a judgement about \
         what a listener wants, it is the difference between the right hour and the wrong one"
    );
}

#[tokio::test]
async fn invalid_or_unallowlisted_events_never_enter_the_hub() {
    let (app, live) = enabled();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut subscription = live.subscribe(&channel, None);

    let mut unknown = create("unknown", "10", false);
    unknown["message"]["channel_id"] = json!("9999999999");
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), unknown).await.0,
        StatusCode::NOT_FOUND
    );

    let bad_id = create("line\nbreak", "10", false);
    assert_eq!(
        post(&app, Some(INGEST_TOKEN), bad_id).await.0,
        StatusCode::BAD_REQUEST
    );
    assert!(subscription.receiver.try_recv().is_err());
}

#[tokio::test]
async fn an_ingest_body_is_bounded_before_it_can_reach_the_hub() {
    let (app, _live) = enabled();
    let oversized = "x".repeat(vibe_talk::http::api::MAX_INGEST_BODY_BYTES + 1);
    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/live/events")
                .header("content-type", "application/json")
                .body(Body::from(oversized.clone()))
                .expect("request"),
        )
        .await
        .expect("router responds");
    assert_eq!(
        unauthorized.status(),
        StatusCode::UNAUTHORIZED,
        "authentication must happen before an untrusted body is read or parsed"
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/live/events")
                .header("authorization", format!("Bearer {INGEST_TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(oversized))
                .expect("request"),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn historical_push_is_replayed_on_the_wire_and_mutations_are_typed() {
    let (app, _live) = enabled();
    let stream = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/channels/{READ_CHANNEL}/stream"))
                .header("authorization", format!("Bearer {READ_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router responds");
    assert_eq!(stream.status(), StatusCode::OK);
    let mut body = stream.into_body();

    assert_eq!(
        post(
            &app,
            Some(INGEST_TOKEN),
            create("opaque-create", "10", true)
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    let update = json!({
        "event_id": "opaque-update",
        "historical": false,
        "kind": "update",
        "message": message("10", "edited"),
    });
    post(&app, Some(INGEST_TOKEN), update).await;
    let delete = json!({
        "event_id": "opaque-delete",
        "historical": false,
        "kind": "delete",
        "channel_id": READ_CHANNEL,
        "message_id": "10",
    });
    post(&app, Some(INGEST_TOKEN), delete).await;

    let text = read_events(&mut body, 3).await;
    assert!(text.contains("id: push:opaque-create\nevent: message"));
    // History, but newer than the attach: not from the tail, so the page cannot assume that a read
    // it already made has seen it.
    assert!(text.contains("\"replayed\":true,\"from_tail\":false"));
    assert!(text.contains("id: push:opaque-update\nevent: message_update"));
    assert!(text.contains("id: push:opaque-delete\nevent: message_delete"));
    assert!(text.contains("\"message_id\":\"10\""));
}

#[tokio::test]
async fn opaque_reconnect_cursor_replays_exactly_after_it_or_forces_a_reset() {
    let (app, _live) = enabled();
    post(
        &app,
        Some(INGEST_TOKEN),
        create("opaque-create", "10", false),
    )
    .await;
    let update = json!({
        "event_id": "opaque-update",
        "historical": false,
        "kind": "update",
        "message": message("10", "edited"),
    });
    post(&app, Some(INGEST_TOKEN), update).await;
    let delete = json!({
        "event_id": "opaque-delete",
        "historical": false,
        "kind": "delete",
        "channel_id": READ_CHANNEL,
        "message_id": "10",
    });
    post(&app, Some(INGEST_TOKEN), delete).await;

    let resumed = open_stream(&app, Some("push:opaque-create")).await;
    let mut body = resumed.into_body();
    let text = read_events(&mut body, 2).await;
    assert!(!text.contains("id: push:opaque-create"));
    assert!(text.contains("id: push:opaque-update"));
    assert!(text.contains("id: push:opaque-delete"));

    let unknown = open_stream(&app, Some("push:opaque-fell-out-of-tail")).await;
    let mut body = unknown.into_body();
    let text = read_events(&mut body, 1).await;
    assert!(text.contains("event: reset"));
    let ended = tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("reset stream did not end");
    assert!(
        ended.is_none(),
        "a reset stream carried on over an unknown gap"
    );

    let unknown_numeric = open_stream(&app, Some("9")).await;
    let mut body = unknown_numeric.into_body();
    let text = read_events(&mut body, 1).await;
    assert!(
        text.contains("event: reset"),
        "a numeric cursor absent from a push tail cannot safely order edits and deletes: {text}"
    );
}

async fn open_stream(app: &axum::Router, after: Option<&str>) -> axum::response::Response {
    let mut request = Request::builder()
        .uri(format!("/api/v1/channels/{READ_CHANNEL}/stream"))
        .header("authorization", format!("Bearer {READ_TOKEN}"));
    if let Some(after) = after {
        request = request.header("last-event-id", after);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("router responds")
}

async fn read_events(body: &mut Body, want: usize) -> String {
    let mut text = String::new();
    while text.matches("\n\n").count() < want {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("stream event timeout")
            .expect("stream ended")
            .expect("stream frame");
        if let Some(data) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(data));
        }
    }
    text
}
