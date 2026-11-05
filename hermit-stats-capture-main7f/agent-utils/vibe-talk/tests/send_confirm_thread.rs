//! `send-confirm-thread`: a reply delivered into a thread is confirmed as delivered.
//!
//! The owner replied to a message in a thread from Main, on a channel served by a Google Chat
//! bridge. The reply arrived and its recipient answered it, but the page said "Delivery
//! unconfirmed ... timeline response included a different thread". The bridge had answered the
//! post with its stored copy of the new message — its live event stream stored the message before
//! the provider's send command returned — and that copy carried no `thread`, which the wire format
//! leaves optional. vibe-talk read the missing membership as another thread.
//!
//! Every test drives the real reply route through the real bridge client, against a loopback
//! bridge that answers a thread post as that one did, and posts a given nonce at most once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::discord::http::HttpDiscordClient;
use vibe_talk::state::AppState;
use vibe_talk::testing::{self, WRITE_CHANNEL, WRITE_TOKEN};

/// One request to the server, as the page makes it, with the answer's status and JSON body.
async fn call(state: &AppState, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/channels/{WRITE_CHANNEL}/reply"))
        .header("authorization", format!("Bearer {WRITE_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
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

/// How the bridge places the message in its answer to a thread post.
#[derive(Clone, Copy)]
enum Answer {
    /// Not at all: the copy its live event stream stored first, with no `thread`.
    Stored,
    /// In a thread other than the one posted to: a bridge that put the post somewhere else.
    Elsewhere,
}

/// Every request the bridge received: the path, and the nonce it carried.
type Requests = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// vibe-talk in front of a Google Chat bridge that answers thread posts as `answer` says.
///
/// Like the provider behind it, the bridge posts a nonce once: a second request under a nonce it
/// has seen is answered with the message the first one posted.
async fn bridge(answer: Answer) -> (AppState, Requests, tokio::task::JoinHandle<()>) {
    let requests: Requests = Arc::default();
    let posted: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let seen = Arc::clone(&requests);
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
        let seen = Arc::clone(&seen);
        let posted = Arc::clone(&posted);
        async move {
            let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let nonce = value["nonce"].as_str().map(str::to_owned);
            let path = uri.path().to_owned();
            seen.lock()
                .expect("requests")
                .push((path.clone(), nonce.clone()));
            let id = {
                let mut posted = posted.lock().expect("posted");
                let next = format!("91950{:03}", posted.len() + 1);
                nonce.map_or(next.clone(), |nonce| {
                    posted.entry(nonce).or_insert(next).clone()
                })
            };
            let mut message = json!({
                "id": id, "channel_id": WRITE_CHANNEL,
                "content": value["content"].as_str().unwrap_or_default(),
                "timestamp": "2026-10-13T02:49:10.561Z",
                "author": {"id": "7", "username": "owner", "bot": false},
            });
            if let Answer::Elsewhere = answer {
                message["thread"] = json!({
                    "id": "t_another", "root_message_id": null, "is_root": false,
                    "reply_count": null, "reply_count_exact": false,
                });
            }
            (StatusCode::OK, message.to_string())
        }
    });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    let mut config = testing::config();
    let discord = config.discord_mut().expect("discord provider");
    discord.api_base = base;
    discord.provider_name = "Google Chat".to_owned();
    discord.channel_registration = true;
    discord.thread_api = vibe_talk::threads::ThreadApi::Bridge;
    let (mut state, _) = testing::state();
    state.replace_chat(Arc::new(HttpDiscordClient::new(discord).expect("client")));
    (state, requests, task)
}

/// What the page sends for Reply on a thread's message in Main: the message it answers, the
/// thread that message is in, and the outbox entry's id as the idempotency key.
fn reply_in_thread() -> Value {
    json!({
        "text": "Yes, ship it after the canary.",
        "reply_to": "9194000001",
        "thread_id": "t_root",
        "idempotency_key": "outbox-entry-1",
    })
}

#[tokio::test]
async fn a_reply_the_bridge_confirms_without_membership_is_sent_in_its_thread() {
    let (state, requests, task) = bridge(Answer::Stored).await;
    let (status, answer) = call(&state, reply_in_thread()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let part = &answer["parts"][0];
    assert_eq!(part["id"], "91950001", "{answer}");
    assert_eq!(part["thread"]["id"], "t_root", "{answer}");
    assert_eq!(part["thread"]["is_root"], false, "{answer}");
    assert_eq!(answer["posted"]["thread"]["id"], "t_root", "{answer}");

    // Retry, as the page's Retry does: the same request under the same key. It is confirmed too,
    // as the same message, and it asked the bridge for the same post rather than another.
    let (status, again) = call(&state, reply_in_thread()).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["parts"][0]["id"], part["id"], "{again}");
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests
            .iter()
            .all(|(path, _)| path == &format!("/channels/{WRITE_CHANNEL}/threads/t_root/messages")),
        "{requests:?}"
    );
    assert!(requests[0].1.is_some(), "the post carried no nonce");
    assert_eq!(
        requests[0].1, requests[1].1,
        "a retry must carry the same nonce"
    );
    task.abort();
}

#[tokio::test]
async fn a_reply_the_bridge_places_in_another_thread_is_still_unconfirmed() {
    let (state, requests, task) = bridge(Answer::Elsewhere).await;
    let (status, answer) = call(&state, reply_in_thread()).await;
    // Not sent, and not "nothing happened" either: the bridge answered, but not for this thread.
    assert_eq!(status, StatusCode::MULTI_STATUS, "{answer}");
    assert_eq!(answer["error"], "partially_posted", "{answer}");
    assert_eq!(answer["posted"], 0, "{answer}");
    assert_eq!(answer["retryable"], false, "{answer}");
    assert!(
        answer["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("different thread")),
        "{answer}"
    );
    assert_eq!(requests.lock().expect("requests").len(), 1);
    task.abort();
}
