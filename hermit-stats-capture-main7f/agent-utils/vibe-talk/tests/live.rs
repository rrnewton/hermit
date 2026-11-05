//! End-to-end tests of the live stream: `GET /api/v1/channels/{id}/stream`.
//!
//! Driven through the real router, exactly as `tests/api.rs` drives everything else, so the
//! bearer check, the channel allowlist, the SSE framing and the headers are the ones a browser
//! would meet.
//!
//! **One harness detail is load-bearing and cost an hour the first time.** `BodyExt::collect()`
//! HANGS on an endless body: an SSE response never ends, so "read the whole body and assert on it"
//! is a test that never finishes and reports nothing. Frames are read INCREMENTALLY here, inside a
//! `tokio::time::timeout`, and the timeout is what turns "the page never got the message" into a
//! failure instead of a hung suite.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::http::router;
use vibe_talk::live::LiveHub;
use vibe_talk::model::ChannelId;
use vibe_talk::state::AppState;
use vibe_talk::store::StateStore as _;
use vibe_talk::testing::{READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};

struct Harness {
    router: axum::Router,
    discord: Arc<FakeDiscord>,
    live: Arc<LiveHub>,
    state: AppState,
}

fn harness() -> Harness {
    let (state, discord) = vibe_talk::testing::state();
    let live = Arc::clone(&state.live);
    Harness {
        router: router(state.clone()),
        discord,
        live,
        state,
    }
}

async fn add_channel(harness: &Harness, id: &str, label: &str) -> StatusCode {
    harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/channels")
                .header("authorization", format!("Bearer {WRITE_TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "id": id, "label": label }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("router responds")
        .status()
}

async fn wait_for_fetches(discord: &FakeDiscord, minimum: usize) {
    for _ in 0..100 {
        if discord.fetch_count() >= minimum {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "the poller made {} fetches, fewer than the expected {minimum}",
        discord.fetch_count()
    );
}

/// Ingest whatever the fake currently holds for `channel`, exactly as a poll tick would.
///
/// This is the real [`vibe_talk::live::poll_once`], not a shortcut around it, so the seeding rule
/// applies here too: the first call publishes nothing.
async fn tick(harness: &Harness, channel: &ChannelId, cursor: &mut Option<u64>) -> usize {
    vibe_talk::live::poll_once(
        harness.discord.as_ref(),
        harness.live.as_ref(),
        channel,
        50,
        cursor,
    )
    .await
    .expect("the fake reads")
    .published
}

async fn open(
    harness: &Harness,
    uri: &str,
    token: Option<&str>,
    last_event_id: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(id) = last_event_id {
        builder = builder.header("last-event-id", id);
    }
    harness
        .router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("router responds")
}

/// Read frames until `want` complete SSE events have arrived, or fail by timing out.
///
/// An SSE event ends at a blank line, so counting `\n\n` counts events.
async fn read_events(body: &mut Body, want: usize) -> String {
    let mut text = String::new();
    while text.matches("\n\n").count() < want {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .unwrap_or_else(|_| {
                panic!("the stream produced nothing within five seconds; so far: {text:?}")
            })
            .expect("the stream ended before it delivered anything")
            .expect("the frame is readable");
        if let Some(data) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(data));
        }
    }
    text
}

/// Read frames until the stream ENDS, or fail by timing out.
///
/// The counterpart of [`read_events`], and the only way to assert that a stream stopped: reading
/// for a fixed number of events cannot tell "there are no more" from "the next one is late".
async fn read_to_end(body: &mut Body) -> String {
    let mut text = String::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .unwrap_or_else(|_| {
                panic!("the stream was still open after five seconds; so far: {text:?}")
            });
        let Some(frame) = frame else {
            return text;
        };
        let frame = frame.expect("the frame is readable");
        if let Some(data) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(data));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_channel_added_while_the_poller_is_running_publishes_new_messages() {
    let harness = harness();
    let interval = Duration::from_secs(10);
    let poller = tokio::spawn(vibe_talk::live::poll_forever(
        harness.state.clone(),
        None,
        harness.state.chat.clone(),
        50,
        interval,
    ));
    wait_for_fetches(harness.discord.as_ref(), 2).await;

    let fresh = ChannelId("3333333333333333333".to_owned());
    harness
        .discord
        .seed(&fresh, "codex-eng", "present when the channel was added");
    assert_eq!(
        add_channel(&harness, fresh.as_str(), "second team").await,
        StatusCode::OK
    );
    let after_add_probe = harness.discord.fetch_count();

    tokio::time::advance(interval).await;
    wait_for_fetches(harness.discord.as_ref(), after_add_probe + 3).await;
    let mut subscription = harness.live.subscribe(&fresh, None);
    assert!(
        subscription.replay.is_empty(),
        "the first poll of a newly added channel must seed its cursor, not publish its history"
    );

    harness
        .discord
        .seed(&fresh, "codex-eng", "arrived after the channel was added");
    let before_live_tick = harness.discord.fetch_count();
    tokio::time::advance(interval).await;
    wait_for_fetches(harness.discord.as_ref(), before_live_tick + 3).await;
    let published = subscription.receiver.try_recv().expect(
        "the running poller did not publish a message from the channel added through the API",
    );
    assert_eq!(
        published.message.content,
        "arrived after the channel was added"
    );
    poller.abort();
}

#[tokio::test(start_paused = true)]
async fn a_channel_restored_at_startup_is_polled_and_publishes_new_messages() {
    let (state, discord, store) = vibe_talk::testing::state_with_store();
    let fresh = ChannelId("4444444444444444444".to_owned());
    store
        .add_channel(&fresh, "restored team", false, None, 1)
        .await
        .expect("store added channel");
    assert_eq!(
        state
            .restore_added_channels()
            .await
            .expect("restore channels"),
        1
    );
    discord.seed(&fresh, "codex-eng", "present at startup");

    let interval = Duration::from_secs(10);
    let poller = tokio::spawn(vibe_talk::live::poll_forever(
        state.clone(),
        None,
        state.chat.clone(),
        50,
        interval,
    ));
    wait_for_fetches(discord.as_ref(), 3).await;
    let mut subscription = state.live.subscribe(&fresh, None);
    assert!(
        subscription.replay.is_empty(),
        "the startup poll must seed the restored channel instead of replaying old history"
    );

    discord.seed(&fresh, "codex-eng", "arrived after restart");
    let before_live_tick = discord.fetch_count();
    tokio::time::advance(interval).await;
    wait_for_fetches(discord.as_ref(), before_live_tick + 3).await;
    let published = subscription
        .receiver
        .try_recv()
        .expect("the poller did not publish a message from the restored channel");
    assert_eq!(published.message.content, "arrived after restart");
    poller.abort();
}

/// The `data:` payloads of every complete event in `text`, parsed.
fn payloads(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|json| serde_json::from_str(json.trim()).ok())
        .collect()
}

#[tokio::test]
async fn an_unauthenticated_caller_cannot_open_a_stream() {
    let harness = harness();
    let response = open(
        &harness,
        &format!("/api/v1/channels/{READ_CHANNEL}/stream"),
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_channel_outside_the_allowlist_is_refused_rather_than_streamed_empty() {
    // The failure this exists for: a stream is the one route where "no such channel" and "nothing
    // is happening in that channel" look identical from the client, forever. So it must refuse.
    let harness = harness();
    let response = open(
        &harness,
        "/api/v1/channels/9999999999/stream",
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("an error body is finite")
        .to_bytes();
    let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(payload["error"], "unknown_channel");
}

#[tokio::test]
async fn a_message_published_after_attach_reaches_an_attached_subscriber() {
    let harness = harness();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    harness.discord.seed(&channel, "codex-eng", "already here");
    let mut cursor = None;
    assert_eq!(
        tick(&harness, &channel, &mut cursor).await,
        0,
        "the seeding tick must publish nothing"
    );

    let response = open(
        &harness,
        &format!("/api/v1/channels/{READ_CHANNEL}/stream"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();

    harness
        .discord
        .seed(&channel, "claude-integ", "the runner came back");
    assert_eq!(tick(&harness, &channel, &mut cursor).await, 1);

    let text = read_events(&mut body, 1).await;
    assert!(
        text.contains("event: message"),
        "every event must name itself so a page can branch on the type: {text:?}"
    );
    assert!(
        text.contains("id: 1"),
        "every event must carry its message id, or Last-Event-ID has nothing to send back: \
         {text:?}"
    );
    let payload = payloads(&text).pop().expect("one parsed event");
    assert_eq!(payload["message"]["content"], "the runner came back");
    assert_eq!(payload["self_posted"], false);
    assert!(
        payload["untrusted_content_notice"]
            .as_str()
            .expect("the notice rides along")
            .contains("DATA"),
        "a pushed message is third-party text exactly as a fetched one is: {payload}"
    );
    assert_eq!(
        payload["message"]["content"].as_str(),
        Some("the runner came back")
    );
    assert!(
        !text.contains("already here"),
        "the history that predates the subscriber must not be delivered as news: {text:?}"
    );
}

#[tokio::test]
async fn last_event_id_replays_only_what_came_after_it() {
    let harness = harness();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut cursor = None;
    tick(&harness, &channel, &mut cursor).await;

    harness.discord.seed(&channel, "a", "one");
    let second = harness.discord.seed(&channel, "a", "two");
    harness.discord.seed(&channel, "a", "three");
    assert_eq!(tick(&harness, &channel, &mut cursor).await, 3);

    let response = open(
        &harness,
        &format!("/api/v1/channels/{READ_CHANNEL}/stream"),
        Some(READ_TOKEN),
        Some(second.as_str()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let text = read_events(&mut body, 1).await;
    let seen: Vec<String> = payloads(&text)
        .iter()
        .filter_map(|p| p["message"]["content"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        seen,
        vec!["three".to_owned()],
        "resuming must deliver strictly what came AFTER the id the page last saw: {text:?}"
    );
}

#[tokio::test]
async fn a_reconnect_with_no_last_event_id_gets_the_whole_replay_tail() {
    // The control for the test above: without it, an implementation that replayed nothing at all
    // would satisfy "only what came after".
    let harness = harness();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut cursor = None;
    tick(&harness, &channel, &mut cursor).await;
    harness.discord.seed(&channel, "a", "one");
    harness.discord.seed(&channel, "a", "two");
    tick(&harness, &channel, &mut cursor).await;

    let response = open(
        &harness,
        &format!("/api/v1/channels/{READ_CHANNEL}/stream"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    let mut body = response.into_body();
    let text = read_events(&mut body, 2).await;
    let seen: Vec<String> = payloads(&text)
        .iter()
        .filter_map(|p| p["message"]["content"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(seen, vec!["one".to_owned(), "two".to_owned()]);
}

#[tokio::test]
async fn a_replayed_message_is_labelled_as_replayed_and_a_live_one_is_not() {
    // The burst this exists to stop: a page that attaches with no `Last-Event-ID` — a fresh
    // sign-in, a channel change, the reconnect after an `event: reset` — is handed the whole tail,
    // up to two hundred messages. They belong on screen. They must NOT be announced to a live,
    // billed conversation as "a message was just posted", and the page cannot tell them from a
    // live arrival unless this server says which is which.
    let harness = harness();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut cursor = None;
    tick(&harness, &channel, &mut cursor).await;
    harness
        .discord
        .seed(&channel, "a", "said before you got here");
    assert_eq!(tick(&harness, &channel, &mut cursor).await, 1);

    let response = open(
        &harness,
        &format!("/api/v1/channels/{READ_CHANNEL}/stream"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    let mut body = response.into_body();
    let tail = read_events(&mut body, 1).await;
    let held = payloads(&tail).pop().expect("the tail event");
    assert_eq!(held["message"]["content"], "said before you got here");
    assert_eq!(
        held["replayed"], true,
        "a message out of the replay tail must say so, or the page relays stale channel text into \
         a paid conversation as news: {tail:?}"
    );
    assert_eq!(
        held["from_tail"], true,
        "the page skips a read only for what every read after the attach has seen: {tail:?}"
    );

    harness
        .discord
        .seed(&channel, "a", "and this just happened");
    assert_eq!(tick(&harness, &channel, &mut cursor).await, 1);
    let live = read_events(&mut body, 1).await;
    let arrived = payloads(&live).pop().expect("the live event");
    assert_eq!(arrived["message"]["content"], "and this just happened");
    assert_eq!(
        arrived["replayed"], false,
        "and a message that really did just arrive must NOT be marked replayed, or the feature \
         relays nothing at all: {live:?}"
    );
    assert_eq!(arrived["from_tail"], false, "{live:?}");
}

#[tokio::test]
async fn a_subscriber_that_falls_behind_gets_one_reset_and_the_stream_ends() {
    // The lag path, which had no server-side test at all: the page branches on the event NAME, and
    // the whole argument for `reset` over a short resume is that the stream STOPS so the page has
    // to re-read. Both halves are pinned here — a stream that carried on after the reset would
    // leave a page reading a feed with a hole in it and no way to know.
    let harness = harness();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let mut cursor = None;
    tick(&harness, &channel, &mut cursor).await;

    // Attached and then never read, which is exactly what a stalled browser looks like.
    let response = open(
        &harness,
        &format!("/api/v1/channels/{READ_CHANNEL}/stream"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();

    let overflow = vibe_talk::live::BROADCAST_CAPACITY + 8;
    for n in 0..overflow {
        harness.discord.seed(&channel, "a", &format!("m{n}"));
    }
    // Two ticks, because one tick is bounded by `MAX_PAGES_PER_TICK`; between them everything
    // seeded is published, and the subscriber has read none of it.
    let mut published = tick(&harness, &channel, &mut cursor).await;
    while published < overflow {
        let more = tick(&harness, &channel, &mut cursor).await;
        assert!(more > 0, "the walk stalled with {published} of {overflow}");
        published += more;
    }

    let text = read_to_end(&mut body).await;
    assert!(
        text.contains("event: reset"),
        "a subscriber this far behind must be told to re-read, by the event name the page branches \
         on: {text:?}"
    );
    assert!(
        !text.contains(&format!("m{}", overflow - 1)),
        "and the reset must be the LAST thing on the stream — carrying on afterwards resumes short \
         over a gap the page cannot see: {text:?}"
    );
    // `read_to_end` returning at all is the other half: the body ENDED rather than parking.
    let after_reset = text
        .split("event: reset")
        .nth(1)
        .expect("the reset is in there");
    assert!(
        !after_reset.contains("event: message"),
        "nothing may follow the reset: {after_reset:?}"
    );
}

#[tokio::test]
async fn the_stream_is_not_cached_and_is_not_buffered_by_a_reverse_proxy() {
    let harness = harness();
    let response = open(
        &harness,
        &format!("/api/v1/channels/{READ_CHANNEL}/stream"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        "no-store"
    );
    assert_eq!(
        headers
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        "no",
        "without this an nginx-shaped proxy buffers the whole response and the stream is dead \
         behind the tunnel while working perfectly on localhost"
    );
    assert!(headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .starts_with("text/event-stream"));
}

#[tokio::test]
async fn a_message_this_server_posted_is_marked_so_the_page_does_not_relay_it_back() {
    // The feedback loop, end to end: `ops::reply` posts as the bot, the poller then sees that very
    // message, and relaying it into the live conversation would have the agent answering itself.
    let harness = harness();
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    let mut cursor = None;
    tick(&harness, &channel, &mut cursor).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/channels/{WRITE_CHANNEL}/reply"))
        .header("authorization", format!("Bearer {WRITE_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"text": "landed it"}).to_string(),
        ))
        .expect("request");
    let posted = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    assert_eq!(posted.status(), StatusCode::OK);

    harness.discord.seed(&channel, "someone-else", "nice");
    assert_eq!(tick(&harness, &channel, &mut cursor).await, 2);

    let response = open(
        &harness,
        &format!("/api/v1/channels/{WRITE_CHANNEL}/stream"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    let mut body = response.into_body();
    let text = read_events(&mut body, 2).await;
    let flags: Vec<(String, bool)> = payloads(&text)
        .iter()
        .map(|p| {
            (
                p["message"]["content"].as_str().unwrap_or("").to_owned(),
                p["self_posted"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        flags,
        vec![("landed it".to_owned(), true), ("nice".to_owned(), false),],
        "only the message this server posted may be flagged, and it must still be DELIVERED — the \
         channel view shows it; it is the RELAY to the agent the page suppresses: {text:?}"
    );
}

#[tokio::test]
async fn the_hub_is_reachable_from_the_shared_state_so_a_second_reader_sees_the_same_feed() {
    // Two subscribers, one publish. A hub that handed each caller its own channel would pass every
    // other test here and deliver a message to exactly one of two open phones.
    let (state, discord) = vibe_talk::testing::state();
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let hub: &Arc<LiveHub> = &state.live;
    let mut first = hub.subscribe(&channel, None);
    let mut second = hub.subscribe(&channel, None);
    let mut cursor = None;
    vibe_talk::live::poll_once(discord.as_ref(), hub.as_ref(), &channel, 50, &mut cursor)
        .await
        .expect("seeds");
    discord.seed(&channel, "a", "to both of you");
    vibe_talk::live::poll_once(discord.as_ref(), hub.as_ref(), &channel, 50, &mut cursor)
        .await
        .expect("polls");
    assert_eq!(
        first.receiver.try_recv().expect("first").message.content,
        "to both of you"
    );
    assert_eq!(
        second.receiver.try_recv().expect("second").message.content,
        "to both of you"
    );
}
