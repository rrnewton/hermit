//! `#232 upstream-mark-read`: clearing a channel's unread state at its chat provider, through the
//! real router, store and ingest hub, against the in-memory provider.
//!
//! The loop's own timing (one push per burst, never backwards, retry waits) is tested beside it in
//! `src/upstream_clear.rs`; this file is about the setting: who may see and change it, that it is
//! kept by the server, that a channel whose provider cannot do it is refused, and that a message
//! ingested for an enabled channel reaches the provider.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::model::ChannelId;
use vibe_talk::state::AppState;
use vibe_talk::store::{StateStore as _, UpstreamClearMode};
use vibe_talk::testing::{READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};
use vibe_talk::upstream_clear::Clearer;

async fn call(
    state: &AppState,
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
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
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

fn path(channel: &str) -> String {
    format!("/api/v1/channels/{channel}/upstream-clear")
}

#[tokio::test]
async fn it_is_off_by_default_and_not_offered_by_a_provider_that_cannot() {
    let (state, _discord, _store) = vibe_talk::testing::state_with_store();
    let (status, config) = call(&state, "GET", "/api/v1/client-config", READ_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        config.get("upstream_clear_supported").is_none(),
        "advertised by a provider that cannot: {config}"
    );
    let (status, view) = call(&state, "GET", &path(WRITE_CHANNEL), READ_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["mode"], "off");
    assert_eq!(view["supported"], false);
    let (status, refused) = call(
        &state,
        "PUT",
        &path(WRITE_CHANNEL),
        WRITE_TOKEN,
        Some(json!({ "mode": "on" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "refused");
    assert!(
        state
            .store
            .upstream_clear_settings()
            .await
            .expect("read")
            .is_empty(),
        "a refused setting was stored"
    );
}

#[tokio::test]
async fn the_owner_turns_it_on_the_server_keeps_it_and_a_read_token_cannot_change_it() {
    let (state, discord, store) = vibe_talk::testing::state_with_store();
    discord.enable_upstream_clear();
    let (_, config) = call(&state, "GET", "/api/v1/client-config", READ_TOKEN, None).await;
    assert_eq!(config["upstream_clear_supported"], true);

    let (status, _) = call(
        &state,
        "PUT",
        &path(WRITE_CHANNEL),
        READ_TOKEN,
        Some(json!({ "mode": "on" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, view) = call(
        &state,
        "PUT",
        &path(WRITE_CHANNEL),
        WRITE_TOKEN,
        Some(json!({ "mode": "dry_run" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["mode"], "dry_run");
    assert_eq!(view["supported"], true);
    assert_eq!(view["provider_name"], "Discord");
    assert!(view["enabled_at_ms"].as_i64().is_some(), "{view}");
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    assert_eq!(
        state.live.upstream_clear().mode(&channel),
        UpstreamClearMode::DryRun,
        "the running loop was not told"
    );
    let stored = store.upstream_clear_settings().await.expect("read");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].mode, UpstreamClearMode::DryRun);

    // Seen at read scope, as the server keeps it.
    let (status, view) = call(&state, "GET", &path(WRITE_CHANNEL), READ_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["mode"], "dry_run");
    // The other channel is untouched.
    let (_, other) = call(&state, "GET", &path(READ_CHANNEL), READ_TOKEN, None).await;
    assert_eq!(other["mode"], "off");

    let (status, view) = call(
        &state,
        "PUT",
        &path(WRITE_CHANNEL),
        WRITE_TOKEN,
        Some(json!({ "mode": "off" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["mode"], "off");
    assert!(view.get("enabled_at_ms").is_none(), "{view}");
    assert_eq!(
        state.live.upstream_clear().mode(&channel),
        UpstreamClearMode::Off
    );
}

#[tokio::test]
async fn a_bad_request_an_unknown_channel_and_a_server_without_a_store_are_refused() {
    let (state, discord, _store) = vibe_talk::testing::state_with_store();
    discord.enable_upstream_clear();
    for body in [
        json!({ "mode": "sometimes" }),
        json!({}),
        json!({ "mode": "on", "everything": true }),
        // `#253 upstream-clear-polish`: pushing now is its own route, never part of a save.
        json!({ "mode": "on", "clear_now": true }),
    ] {
        let (status, _) = call(
            &state,
            "PUT",
            &path(WRITE_CHANNEL),
            WRITE_TOKEN,
            Some(body.clone()),
        )
        .await;
        assert!(status.is_client_error(), "{body} was accepted: {status}");
    }
    let (status, _) = call(
        &state,
        "PUT",
        &path("9999999999"),
        WRITE_TOKEN,
        Some(json!({ "mode": "on" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (state, discord) =
        vibe_talk::testing::state_with(Arc::new(vibe_talk::store::disabled::DisabledStore));
    discord.enable_upstream_clear();
    let (status, body) = call(
        &state,
        "PUT",
        &path(WRITE_CHANNEL),
        WRITE_TOKEN,
        Some(json!({ "mode": "on" })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "storage_not_configured");
    let (status, _) = call(&state, "GET", &path(WRITE_CHANNEL), READ_TOKEN, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

fn now_path(channel: &str) -> String {
    format!("/api/v1/channels/{channel}/upstream-clear/now")
}

/// A server whose provider can clear, with the loop running, and `channel` set to `mode`.
async fn running(
    mode: &str,
) -> (
    AppState,
    Arc<vibe_talk::discord::fake::FakeDiscord>,
    Arc<vibe_talk::store::fake::FakeStore>,
) {
    let (state, discord, store) = vibe_talk::testing::state_with_store();
    discord.enable_upstream_clear();
    tokio::spawn(Clearer::for_state(&state).run());
    let (status, view) = call(
        &state,
        "PUT",
        &path(WRITE_CHANNEL),
        WRITE_TOKEN,
        Some(json!({ "mode": mode })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    (state, discord, store)
}

#[tokio::test(start_paused = true)]
async fn mark_read_now_pushes_the_newest_message_at_once_and_answers_what_it_came_to() {
    // `#253 upstream-clear-polish`. One press, one push, of the channel's newest message, without
    // waiting for the next message or for the quiet period; and the answer carries the result.
    let (state, discord, store) = running("on").await;
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    discord.seed(&channel, "agent", "older");
    let newest = discord.seed(&channel, "agent", "newest");
    assert!(
        discord.upstream_clears().is_empty(),
        "turning it on pushed by itself"
    );
    let (status, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["outcome"], "pushed", "{answer}");
    assert_eq!(
        discord.upstream_clears(),
        vec![(channel.clone(), newest.clone(), false)]
    );
    // The answer names the pressed message and carries the attempt that answered it.
    assert_eq!(answer["message_id"], newest.as_str(), "{answer}");
    assert!(answer["message_at_ms"].as_i64().is_some(), "{answer}");
    assert_eq!(answer["attempt"]["message_id"], newest.as_str(), "{answer}");
    assert_eq!(answer["attempt"]["result"], "advanced", "{answer}");
    assert_eq!(
        answer["attempt"]["message_at_ms"], answer["message_at_ms"],
        "{answer}"
    );
    let view = &answer["state"];
    assert_eq!(
        view["mode"], "on",
        "pressing it changed the setting: {view}"
    );
    assert_eq!(view["pending"], false, "{view}");
    assert_eq!(view["last"]["message_id"], newest.as_str());
    assert_eq!(view["last"]["result"], "advanced");
    assert_eq!(view["pushed"]["message_id"], newest.as_str());
    assert_eq!(
        store.upstream_clear_settings().await.expect("read")[0]
            .pushed
            .as_ref()
            .map(|pushed| pushed.message.clone()),
        Some(newest.clone())
    );

    // Pressed again with nothing newer: nothing is sent, and the answer says why.
    let (status, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["outcome"], "covered", "{answer}");
    assert!(answer.get("attempt").is_none(), "{answer}");
    assert_eq!(
        discord.upstream_clears().len(),
        1,
        "pushed the same message twice"
    );

    // A newer message is pushed by the next press, still one push per press.
    let newer = discord.seed(&channel, "agent", "newer");
    let (_, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(answer["outcome"], "pushed", "{answer}");
    assert_eq!(
        discord.upstream_clears(),
        vec![(channel.clone(), newest, false), (channel, newer, false)]
    );
}

#[tokio::test(start_paused = true)]
async fn try_it_now_in_a_dry_run_asks_for_a_dry_run_and_writes_nothing() {
    let (state, discord, _store) = running("dry_run").await;
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    let newest = discord.seed(&channel, "agent", "newest");
    let (status, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["outcome"], "pushed", "{answer}");
    assert_eq!(answer["attempt"]["result"], "dry_run", "{answer}");
    assert_eq!(answer["state"]["last"]["result"], "dry_run", "{answer}");
    assert_eq!(answer["state"]["mode"], "dry_run");
    assert_eq!(
        discord.upstream_clears(),
        vec![(channel.clone(), newest, true)]
    );
    assert_eq!(
        discord.upstream_cursor(&channel),
        None,
        "a dry run moved the marker"
    );
}

#[tokio::test(start_paused = true)]
async fn mark_read_now_is_refused_while_off_and_at_read_scope_and_never_turns_it_on() {
    // A page still showing On after another device turned it off must not turn it back on.
    let (state, discord, store) = running("off").await;
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    discord.seed(&channel, "agent", "newest");
    let (status, refused) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "refused");
    assert_eq!(
        state.live.upstream_clear().mode(&channel),
        UpstreamClearMode::Off
    );
    assert!(store
        .upstream_clear_settings()
        .await
        .expect("read")
        .iter()
        .all(|setting| setting.mode == UpstreamClearMode::Off));
    let (status, _) = call(&state, "POST", &now_path(WRITE_CHANNEL), READ_TOKEN, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&state, "POST", &now_path("9999999999"), WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(discord.upstream_clears().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_push_is_answered_failed_and_the_next_press_keeps_the_retry_wait() {
    // `#253 upstream-clear-polish`. An attempt was recorded and it failed: the press says it
    // failed, never "pushed". A spent quota answers 429 with how long to wait, and pressing again
    // must not spend another call before then: the press says it is waiting for the retry.
    let (state, discord, _store) = running("on").await;
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    let newest = discord.seed(&channel, "agent", "newest");
    discord.fail_upstream_clear_next(vibe_talk::chat::ChatError::Status {
        status: 502,
        body: "mark-read failed".to_owned(),
    });
    let (status, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["outcome"], "failed", "{answer}");
    assert_eq!(answer["attempt"]["result"], "failed", "{answer}");
    assert_eq!(answer["attempt"]["message_id"], newest.as_str(), "{answer}");
    assert!(
        answer["attempt"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("mark-read failed")),
        "{answer}"
    );
    assert_eq!(answer["state"]["last"]["result"], "failed", "{answer}");
    assert!(
        answer["state"]["retry_at_ms"].as_i64().is_some(),
        "{answer}"
    );
    assert_eq!(
        answer["state"]["pending"], true,
        "the failed message was dropped: {answer}"
    );

    let (_, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(answer["outcome"], "retry_waiting", "{answer}");
    assert_eq!(
        discord.upstream_clears().len(),
        1,
        "a press cut the retry wait short"
    );

    // The retry still happens, by itself, for that message.
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    assert_eq!(
        discord.upstream_clears(),
        vec![
            (channel.clone(), newest.clone(), false),
            (channel, newest, false)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_push_that_does_not_finish_in_time_answers_waiting_within_45_seconds_and_still_lands() {
    // `#253 upstream-clear-polish`. Held at most 45 s, under a common 60 s proxy read timeout.
    let (state, discord, store) = running("on").await;
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    let newest = discord.seed(&channel, "agent", "newest");
    let gate = discord.hold_upstream_clears();
    let started = tokio::time::Instant::now();
    let (status, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    let held = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["outcome"], "waiting", "{answer}");
    assert!(
        held >= std::time::Duration::from_secs(44) && held <= std::time::Duration::from_secs(45),
        "held the request {held:?}"
    );
    assert_eq!(
        vibe_talk::http::api::UPSTREAM_CLEAR_NOW_WAIT,
        std::time::Duration::from_secs(45)
    );
    assert_eq!(answer["message_id"], newest.as_str(), "{answer}");
    assert!(answer.get("attempt").is_none(), "{answer}");
    assert_eq!(
        discord.upstream_clears(),
        vec![(channel.clone(), newest.clone(), false)]
    );
    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(
        store.upstream_clear_settings().await.expect("read")[0]
            .pushed
            .as_ref()
            .map(|pushed| pushed.message.clone()),
        Some(newest),
        "the push the page stopped waiting for did not land"
    );
}

#[tokio::test(start_paused = true)]
async fn a_message_ingested_for_an_enabled_channel_reaches_the_provider_and_others_do_not() {
    let (state, discord, _store) = vibe_talk::testing::state_with_store();
    discord.enable_upstream_clear();
    let (status, _) = call(
        &state,
        "PUT",
        &path(WRITE_CHANNEL),
        WRITE_TOKEN,
        Some(json!({ "mode": "on" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let enabled = ChannelId(WRITE_CHANNEL.to_owned());
    let other = ChannelId(READ_CHANNEL.to_owned());
    let in_enabled = discord.seed(&enabled, "agent", "build green");
    let in_other = discord.seed(&other, "person", "lunch?");
    // The hub is where both producers land; publishing is what the poller and the adapter do.
    state
        .live
        .publish(&enabled, discord.message(&in_enabled).expect("seeded"));
    state
        .live
        .publish(&other, discord.message(&in_other).expect("seeded"));
    assert!(state.live.upstream_clear().status(&enabled).pending);
    assert!(!state.live.upstream_clear().status(&other).pending);
    let clearer = Clearer::for_state(&state);
    assert!(
        !clearer.push_due().await,
        "pushed before the channel went quiet"
    );
    tokio::time::advance(vibe_talk::upstream_clear::DEFAULT_QUIET).await;
    assert!(
        clearer.push_due().await,
        "not pushed once the channel went quiet"
    );
    assert!(!clearer.push_due().await, "pushed twice");
    assert_eq!(
        discord.upstream_clears(),
        vec![(enabled.clone(), in_enabled, false)]
    );
    assert_eq!(discord.upstream_cursor(&other), None);
}

#[tokio::test(start_paused = true)]
async fn erasing_the_store_turns_every_channel_off_in_the_running_loop_too() {
    // `DELETE /api/v1/storage` erases the setting with everything else. Settings then reads Off
    // from the store, so the running loop must stop as well: a server that went on marking
    // messages read in the owner's own app while Settings said Off is the one thing this setting
    // must never do.
    let (state, discord, store) = vibe_talk::testing::state_with_store();
    discord.enable_upstream_clear();
    for (channel, mode) in [(WRITE_CHANNEL, "on"), (READ_CHANNEL, "dry_run")] {
        let (status, view) = call(
            &state,
            "PUT",
            &path(channel),
            WRITE_TOKEN,
            Some(json!({ "mode": mode })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{view}");
    }
    let enabled = ChannelId(WRITE_CHANNEL.to_owned());
    let dry = ChannelId(READ_CHANNEL.to_owned());
    // One message is waiting to be pushed when the store is erased.
    let waiting = discord.seed(&enabled, "agent", "build started");
    state
        .live
        .publish(&enabled, discord.message(&waiting).expect("seeded"));
    assert!(state.live.upstream_clear().status(&enabled).pending);

    let (status, payload) = call(&state, "DELETE", "/api/v1/storage", WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert!(
        payload["purged"]
            .as_array()
            .is_some_and(|purged| purged.contains(&json!("upstream_clear"))),
        "the answer does not say the setting was erased: {payload}"
    );
    assert!(store
        .upstream_clear_settings()
        .await
        .expect("read")
        .is_empty());
    for channel in [&enabled, &dry] {
        assert_eq!(
            state.live.upstream_clear().mode(channel),
            UpstreamClearMode::Off,
            "the running loop still clears {channel} after the erase"
        );
        let (_, view) = call(&state, "GET", &path(channel.as_str()), READ_TOKEN, None).await;
        assert_eq!(view["mode"], "off", "{view}");
        assert_eq!(view["pending"], false, "{view}");
    }

    // Neither the waiting message nor a later one reaches the provider.
    let later = discord.seed(&enabled, "agent", "build green");
    state
        .live
        .publish(&enabled, discord.message(&later).expect("seeded"));
    let clearer = Clearer::for_state(&state);
    tokio::time::advance(std::time::Duration::from_secs(120)).await;
    assert!(
        !clearer.push_due().await,
        "a push was still due after the erase"
    );
    assert!(
        discord.upstream_clears().is_empty(),
        "marked read upstream after the setting was erased: {:?}",
        discord.upstream_clears()
    );
    assert_eq!(discord.upstream_cursor(&enabled), None);

    // Turning it on again afterwards works as on a fresh store.
    let (status, view) = call(
        &state,
        "PUT",
        &path(WRITE_CHANNEL),
        WRITE_TOKEN,
        Some(json!({ "mode": "on" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        state.live.upstream_clear().mode(&enabled),
        UpstreamClearMode::On
    );
}

#[tokio::test(start_paused = true)]
async fn a_setting_changed_while_the_press_waits_is_answered_changed() {
    // `#253 upstream-clear-polish`. Another device turns it off, or to a dry run, while the push is
    // on its way: the press says the setting changed, never that it was done.
    for change in ["off", "dry_run"] {
        let (state, discord, _store) = running("on").await;
        let channel = ChannelId(WRITE_CHANNEL.to_owned());
        let newest = discord.seed(&channel, "agent", "newest");
        let gate = discord.hold_upstream_clears();
        let pressing = {
            let state = state.clone();
            tokio::spawn(async move {
                call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await
            })
        };
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert_eq!(
            discord.upstream_clears(),
            vec![(channel.clone(), newest.clone(), false)],
            "the press's push is not on its way"
        );
        let (status, _) = call(
            &state,
            "PUT",
            &path(WRITE_CHANNEL),
            WRITE_TOKEN,
            Some(json!({ "mode": change })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The push in flight finishes, and is recorded, but it does not answer the press.
        gate.add_permits(1);
        let (status, answer) = pressing.await.expect("the press answered");
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["outcome"], "changed", "set to {change}: {answer}");
        assert!(answer.get("attempt").is_none(), "{answer}");
        assert_eq!(answer["state"]["mode"], change, "{answer}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_push_the_provider_refuses_is_answered_unavailable_with_its_attempt() {
    let (state, discord, _store) = running("on").await;
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    discord.seed(&channel, "agent", "newest");
    discord.fail_upstream_clear_next(vibe_talk::chat::ChatError::Status {
        status: 409,
        body: r#"{"message":"thread_channel"}"#.to_owned(),
    });
    let (status, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["outcome"], "unavailable", "{answer}");
    assert_eq!(answer["attempt"]["result"], "unavailable", "{answer}");
    assert!(
        answer["state"]["unavailable"].as_str().is_some(),
        "{answer}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_push_the_bridge_no_longer_holds_is_answered_skipped() {
    let (state, discord, _store) = running("on").await;
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    discord.seed(&channel, "agent", "newest");
    discord.fail_upstream_clear_next(vibe_talk::chat::ChatError::Status {
        status: 404,
        body: r#"{"message":"unknown message"}"#.to_owned(),
    });
    let (status, answer) = call(&state, "POST", &now_path(WRITE_CHANNEL), WRITE_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["outcome"], "skipped", "{answer}");
    assert_eq!(answer["attempt"]["result"], "skipped", "{answer}");
}
