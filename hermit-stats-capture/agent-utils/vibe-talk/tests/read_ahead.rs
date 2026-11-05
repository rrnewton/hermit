//! `#191 read-aloud-first-lag`: preparing speech reads what the page has just fetched, fetches only
//! what is missing (one shared window fetch, or thread messages a few at a time), and never waits
//! for the provider warm-up.
//!
//! And what it reads from memory is never older than what the server has been told: an edit or a
//! deletion an adapter pushes, a deletion a delta reports, and the storage purge all reach it, even
//! when they arrive while a fetch that read the old text is still in flight.
//!
//! The content-free timing line these requests log is checked in `tests/logging.rs`, where every
//! test captures logs.
//!
//! Every test runs on tokio's paused clock against a chat backend that takes a fixed time per
//! fetch. So "the tap waited for one fetch, not two" is a measured virtual duration, not a race
//! against the machine's real speed.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::chat::{ChatClient, ChatError, ChatIdentity};
use vibe_talk::model::{ChannelId, Message, MessageId, UserId};
use vibe_talk::read_ahead::{THREAD_FETCHES_IN_FLIGHT, THREAD_FETCH_CONCURRENCY, WARM_UP_BOUND};
use vibe_talk::speech::{Description, Playback, Speech, SpeechError, SpeechProvider};
use vibe_talk::state::AppState;
use vibe_talk::testing::{self, READ_CHANNEL, READ_TOKEN, WRITE_TOKEN};
use vibe_talk::threads::{MessageThread, TimelineDelta, TimelinePage, TimelineRequest};

const THREAD: &str = "opaque/thread-T";
/// Every thread this backend has starts with this; any other is outside the channel.
const THREADS: &str = "opaque/thread-";
const INGEST_TOKEN: &str = "test-ingest-token-0000000000";
/// What one chat fetch costs, like the deployed adapter's ~2 s window read, scaled down.
const FETCH: Duration = Duration::from_millis(1_000);
/// What one thread message lookup costs.
const LOOKUP: Duration = Duration::from_millis(100);

fn message(id: &str, thread: Option<&str>) -> Message {
    Message {
        id: MessageId(id.to_owned()),
        channel_id: ChannelId(READ_CHANNEL.to_owned()),
        author: "Reader".to_owned(),
        author_id: UserId("7".to_owned()),
        author_is_bot: false,
        timestamp: "2026-10-12T01:00:00Z".to_owned(),
        spoken_time: String::new(),
        reply_to: None,
        content: format!("words of message {id}"),
        spoken_content: String::new(),
        content_html: String::new(),
        noise: false,
        thread: thread.map(|thread| MessageThread {
            id: thread.to_owned(),
            root_message_id: None,
            is_root: false,
            reply_count: None,
            reply_count_exact: false,
        }),
        reactions: None,
        heading: None,
    }
}

/// A chat backend that takes [`FETCH`] per window and [`LOOKUP`] per thread message, and counts.
///
/// A window is read when the fetch is SENT, like a backend that answers from what it held then:
/// an edit made while the fetch is out is not in its answer.
#[derive(Default)]
struct SlowChat {
    windows: AtomicUsize,
    timelines: AtomicUsize,
    lookups: AtomicUsize,
    lookups_in_flight: AtomicUsize,
    most_lookups_in_flight: AtomicUsize,
    looked_up: Mutex<Vec<String>>,
    /// The text a message has now, after an edit.
    edited: Mutex<HashMap<String, String>>,
    /// Messages the backend no longer has.
    deleted: Mutex<HashSet<String>>,
}

impl SlowChat {
    /// The channel's messages as they stand now, oldest first.
    fn window(&self) -> Vec<Message> {
        let edited = self.edited.lock().unwrap();
        let deleted = self.deleted.lock().unwrap();
        ["101", "102", "103"]
            .iter()
            .filter(|id| !deleted.contains(**id))
            .map(|id| {
                let mut message = message(id, None);
                if let Some(text) = edited.get(*id) {
                    message.content.clone_from(text);
                }
                message
            })
            .collect()
    }

    fn edit(&self, id: &str, text: &str) {
        self.edited
            .lock()
            .unwrap()
            .insert(id.to_owned(), text.to_owned());
    }

    fn delete(&self, id: &str) {
        self.deleted.lock().unwrap().insert(id.to_owned());
    }

    fn thread() -> Vec<Message> {
        (201..=210)
            .map(|n| message(&n.to_string(), Some(THREAD)))
            .collect()
    }
}

#[async_trait::async_trait]
impl ChatClient for SlowChat {
    fn supports_threading(&self) -> bool {
        true
    }

    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        Ok(ChatIdentity {
            id: "7".to_owned(),
            username: "Reader".to_owned(),
        })
    }

    async fn fetch_page(
        &self,
        _channel: &ChannelId,
        limit: u16,
        _before: Option<&MessageId>,
        _after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        self.windows.fetch_add(1, Ordering::SeqCst);
        // The newest `limit`, read now and answered a fetch later.
        let mut window = self.window();
        let keep = usize::from(limit).min(window.len());
        window.drain(..window.len() - keep);
        tokio::time::sleep(FETCH).await;
        Ok(window)
    }

    async fn fetch_timeline(
        &self,
        _channel: &ChannelId,
        request: &TimelineRequest,
    ) -> Result<TimelinePage, ChatError> {
        self.timelines.fetch_add(1, Ordering::SeqCst);
        // An answer to `after`: only what changed, which here is only what was deleted.
        if request.after.is_some() {
            let mut deleted: Vec<MessageId> = self
                .deleted
                .lock()
                .unwrap()
                .iter()
                .map(|id| MessageId(id.clone()))
                .collect();
            deleted.sort_by(|a, b| a.0.cmp(&b.0));
            tokio::time::sleep(FETCH).await;
            return Ok(TimelinePage {
                delta: Some(TimelineDelta {
                    more: false,
                    complete: true,
                    deleted,
                    removed_threads: Vec::new(),
                }),
                next_after: Some("native-cursor-2".to_owned()),
                ..TimelinePage::default()
            });
        }
        let messages = match request.thread_id.as_deref() {
            Some(THREAD) => Self::thread(),
            Some(_) => {
                return Err(ChatError::Refused(
                    "thread is outside this channel".to_owned(),
                ))
            }
            None => self.window(),
        };
        tokio::time::sleep(FETCH).await;
        Ok(TimelinePage {
            messages,
            next_after: Some("native-cursor-1".to_owned()),
            ..TimelinePage::default()
        })
    }

    async fn fetch_thread_message(
        &self,
        _channel: &ChannelId,
        thread: &str,
        id: &MessageId,
    ) -> Result<Message, ChatError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        self.looked_up.lock().unwrap().push(id.0.clone());
        if !thread.starts_with(THREADS) {
            return Err(ChatError::Refused(
                "thread is outside this channel".to_owned(),
            ));
        }
        let now = self.lookups_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_lookups_in_flight.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(LOOKUP).await;
        self.lookups_in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(message(&id.0, Some(thread)))
    }

    async fn post_message(
        &self,
        _channel: &ChannelId,
        _content: &str,
        _reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        Err(ChatError::Refused("this test posts nothing".to_owned()))
    }
}

/// A server voice whose warm-up takes `warm_up` and counts how often it was started and finished.
struct TimedSpeech {
    warm_up: Duration,
    started: AtomicUsize,
    finished: AtomicUsize,
}

impl TimedSpeech {
    fn new(warm_up: Duration) -> Self {
        Self {
            warm_up,
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl SpeechProvider for TimedSpeech {
    fn describe(&self) -> Description {
        Description {
            backend: "timed-test",
            label: "Timed test voice".to_owned(),
            playback: Playback::Audio,
            local_only: false,
        }
    }

    async fn warm_up(&self) -> Result<(), SpeechError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.warm_up).await;
        self.finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn speak(&self, text: &str, _speed: Option<f64>) -> Result<Speech, SpeechError> {
        Ok(Speech {
            audio: text.as_bytes().to_vec(),
            content_type: "audio/test".to_owned(),
        })
    }
}

fn harness(speech: Arc<TimedSpeech>) -> (axum::Router, AppState, Arc<SlowChat>) {
    // An adapter may push edits and deletions for these channels.
    let (mut state, _chat, _elevenlabs) = testing::state_from_toml(&format!(
        "{}\n[ingest]\ntoken = \"{INGEST_TOKEN}\"\n",
        testing::config_toml_without_elevenlabs()
    ));
    let chat = Arc::new(SlowChat::default());
    state.replace_chat(chat.clone());
    state.speech = speech;
    (vibe_talk::http::router(state.clone()), state, chat)
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_as(app, READ_TOKEN, method, path, body).await
}

async fn call_as(
    app: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("router");
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

fn prepare_path() -> String {
    format!("/api/v1/channels/{READ_CHANNEL}/speech/prepare")
}

fn page_path() -> String {
    format!("/api/v1/channels/{READ_CHANNEL}/page")
}

/// What an adapter pushes for one message, as it now reads.
fn pushed(id: &str, text: &str) -> Value {
    let mut message = message(id, None);
    message.content = text.to_owned();
    serde_json::to_value(&message).expect("a message serializes")
}

/// The words a prepared ticket plays.
async fn play(app: &axum::Router, prepared: &Value) -> String {
    let url = prepared["prepared"][0]["url"].as_str().expect("ticket url");
    let request = Request::builder()
        .uri(url)
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn prepared_ids(body: &Value) -> Vec<String> {
    body["prepared"]
        .as_array()
        .expect("prepared list")
        .iter()
        .map(|entry| entry["message_id"].as_str().expect("id").to_owned())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_window_the_page_just_fetched_is_prepared_from_memory_without_a_fetch() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let (status, _) = call(
        &app,
        "GET",
        &format!("/api/v1/channels/{READ_CHANNEL}/page?limit=20"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(chat.windows.load(Ordering::SeqCst), 1);

    let began = tokio::time::Instant::now();
    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ["101", "103"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(prepared_ids(&body), ["101", "103"]);
    assert_eq!(
        chat.windows.load(Ordering::SeqCst),
        1,
        "preparation fetched the window the page had just fetched"
    );
    assert!(
        began.elapsed() < FETCH,
        "the prepare took {:?}, as long as a fetch",
        began.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn a_thread_view_the_page_fetched_answers_only_for_that_thread() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let encoded = THREAD.replace('/', "%2F");
    let (status, _) = call(
        &app,
        "GET",
        &format!("/api/v1/channels/{READ_CHANNEL}/timeline?view=thread&thread_id={encoded}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ["201", "202"], "thread_id": THREAD})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(prepared_ids(&body), ["201", "202"]);
    assert_eq!(
        chat.lookups.load(Ordering::SeqCst),
        0,
        "the thread view was fetched again"
    );

    // The same ids under another thread are not answered from memory. They go to the backend,
    // which checks thread membership and refuses.
    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ["201"], "thread_id": "another-thread"})),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    assert_eq!(chat.lookups.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn preparations_that_miss_together_share_one_window_fetch() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let began = tokio::time::Instant::now();
    // The batch for the whole screen, and the tapped row on its own a moment later.
    let path = prepare_path();
    let batch = call(
        &app,
        "POST",
        &path,
        Some(json!({"ids": ["101", "102", "103"]})),
    );
    let tap = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        call(&app, "POST", &path, Some(json!({"ids": ["102"]}))).await
    };
    let ((batch_status, batch_body), (tap_status, tap_body)) = tokio::join!(batch, tap);
    assert_eq!(batch_status, StatusCode::OK, "{batch_body}");
    assert_eq!(tap_status, StatusCode::OK, "{tap_body}");
    assert_eq!(prepared_ids(&batch_body), ["101", "102", "103"]);
    assert_eq!(prepared_ids(&tap_body), ["102"]);
    assert_eq!(
        chat.windows.load(Ordering::SeqCst),
        1,
        "the second preparation queued a second identical window fetch"
    );
    assert!(
        began.elapsed() < FETCH * 2,
        "both were answered after {:?}: the fetches ran one after the other",
        began.elapsed()
    );

    // An id that is not in the window is simply absent, as it always was.
    let (status, body) = call(&app, "POST", &path, Some(json!({"ids": ["101", "999"]}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(prepared_ids(&body), ["101"]);
}

#[tokio::test(start_paused = true)]
async fn missing_thread_messages_are_fetched_a_few_at_a_time_and_then_remembered() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let ids: Vec<String> = (201..=210).map(|n| n.to_string()).collect();
    let began = tokio::time::Instant::now();
    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ids, "thread_id": THREAD})),
    )
    .await;
    let took = began.elapsed();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(prepared_ids(&body), ids, "the order on screen was not kept");
    assert_eq!(chat.lookups.load(Ordering::SeqCst), ids.len());
    assert_eq!(
        chat.most_lookups_in_flight.load(Ordering::SeqCst),
        THREAD_FETCH_CONCURRENCY,
        "the lookups were not concurrent, or not bounded"
    );
    let rounds = u32::try_from(ids.len().div_ceil(THREAD_FETCH_CONCURRENCY)).expect("small");
    assert_eq!(took, LOOKUP * rounds, "ten lookups took {took:?}");

    // A second preparation of the same messages, moments later, fetches none of them.
    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ["203", "209"], "thread_id": THREAD})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(prepared_ids(&body), ["203", "209"]);
    assert_eq!(chat.lookups.load(Ordering::SeqCst), ids.len());
}

#[tokio::test(start_paused = true)]
async fn preparation_starts_the_warm_up_and_does_not_wait_for_it() {
    let warm_up = Duration::from_secs(10);
    let speech = Arc::new(TimedSpeech::new(warm_up));
    let (app, state, _chat) = harness(speech.clone());
    // Fetched first, so the prepare itself has nothing to wait for but the warm-up.
    call(
        &app,
        "GET",
        &format!("/api/v1/channels/{READ_CHANNEL}/page"),
        None,
    )
    .await;

    let began = tokio::time::Instant::now();
    let (status, body) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["101"]}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        began.elapsed() < warm_up,
        "the prepare answered after {:?}, so it waited for the warm-up",
        began.elapsed()
    );
    tokio::task::yield_now().await;
    assert_eq!(
        speech.started.load(Ordering::SeqCst),
        1,
        "no warm-up was started"
    );
    assert_eq!(speech.finished.load(Ordering::SeqCst), 0);
    assert!(state.read_ahead.warming());

    // A second prepare while it runs does not start another.
    let (status, _) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["102"]}))).await;
    assert_eq!(status, StatusCode::OK);
    tokio::task::yield_now().await;
    assert_eq!(
        speech.started.load(Ordering::SeqCst),
        1,
        "two warm-ups ran at once"
    );

    // Once it has finished, the next prepare warms up again: the session may have aged out.
    tokio::time::sleep(warm_up + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(speech.finished.load(Ordering::SeqCst), 1);
    assert!(!state.read_ahead.warming());
    let (status, _) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["103"]}))).await;
    assert_eq!(status, StatusCode::OK);
    tokio::task::yield_now().await;
    assert_eq!(speech.started.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_prepared_ticket_from_memory_still_plays_the_message_text() {
    let (app, _state, _chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    call(
        &app,
        "GET",
        &format!("/api/v1/channels/{READ_CHANNEL}/page"),
        None,
    )
    .await;
    let (status, body) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["102"]}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(play(&app, &body).await, "words of message 102");
}

#[tokio::test(start_paused = true)]
async fn a_preparation_joins_only_a_window_at_least_as_wide_as_its_own() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let path = prepare_path();
    // The batch asks for the newest two; the tap, a moment later, for a row only three reach.
    let batch = call(
        &app,
        "POST",
        &path,
        Some(json!({"ids": ["102", "103"], "limit": 2})),
    );
    let tap = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        call(
            &app,
            "POST",
            &path,
            Some(json!({"ids": ["101"], "limit": 3})),
        )
        .await
    };
    let ((batch_status, batch_body), (tap_status, tap_body)) = tokio::join!(batch, tap);
    assert_eq!(batch_status, StatusCode::OK, "{batch_body}");
    assert_eq!(tap_status, StatusCode::OK, "{tap_body}");
    assert_eq!(prepared_ids(&batch_body), ["102", "103"]);
    assert_eq!(
        prepared_ids(&tap_body),
        ["101"],
        "the tap took the narrower window's answer and lost its row"
    );
    assert_eq!(chat.windows.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn thread_lookups_are_bounded_across_every_preparation_at_once() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let path = prepare_path();
    // One preparation per thread on screen, all sent together, as the page does.
    let threads = ["T", "U", "V"];
    let requests = threads.iter().enumerate().map(|(n, name)| {
        let ids: Vec<String> = (1..=10).map(|k| format!("{}{k:02}", n + 2)).collect();
        let body = json!({"ids": ids, "thread_id": format!("{THREADS}{name}")});
        let app = app.clone();
        let path = path.clone();
        async move { (ids, call(&app, "POST", &path, Some(body)).await) }
    });
    for (ids, (status, body)) in futures_util::future::join_all(requests).await {
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(prepared_ids(&body), ids);
    }
    assert_eq!(chat.lookups.load(Ordering::SeqCst), 30);
    assert!(
        threads.len() * THREAD_FETCH_CONCURRENCY > THREAD_FETCHES_IN_FLIGHT,
        "this test needs more per-request lookups than the shared bound allows"
    );
    assert_eq!(
        chat.most_lookups_in_flight.load(Ordering::SeqCst),
        THREAD_FETCHES_IN_FLIGHT,
        "the preparations together had more lookups in flight than the shared bound"
    );
}

#[tokio::test(start_paused = true)]
async fn a_warm_up_that_never_finishes_is_given_up_and_the_next_preparation_starts_another() {
    let speech = Arc::new(TimedSpeech::new(WARM_UP_BOUND * 100));
    let (app, state, _chat) = harness(speech.clone());
    call(&app, "GET", &page_path(), None).await;
    let (status, _) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["101"]}))).await;
    assert_eq!(status, StatusCode::OK);
    tokio::task::yield_now().await;
    assert_eq!(speech.started.load(Ordering::SeqCst), 1);

    // Still within the bound: it is still the one warm-up.
    tokio::time::sleep(WARM_UP_BOUND - Duration::from_secs(1)).await;
    assert!(state.read_ahead.warming());
    call(&app, "GET", &page_path(), None).await;
    call(&app, "POST", &prepare_path(), Some(json!({"ids": ["102"]}))).await;
    tokio::task::yield_now().await;
    assert_eq!(speech.started.load(Ordering::SeqCst), 1);

    // Past it, it has been given up on, and the next preparation warms up again.
    tokio::time::sleep(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(
        !state.read_ahead.warming(),
        "a hung warm-up was never given up"
    );
    assert_eq!(speech.finished.load(Ordering::SeqCst), 0);
    call(&app, "POST", &prepare_path(), Some(json!({"ids": ["103"]}))).await;
    tokio::task::yield_now().await;
    assert_eq!(speech.started.load(Ordering::SeqCst), 2);
}

// What follows is the promise the memory must keep above all: a message that has been edited or
// deleted is never read aloud from it, and a purge leaves none of it behind. Each case fails if its
// wiring (`ingest_event`, `ops::timeline`, `purge_storage`) stops telling the memory.

#[tokio::test(start_paused = true)]
async fn an_edit_an_adapter_pushes_is_never_read_aloud_from_memory() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let (status, _) = call(&app, "GET", &page_path(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(chat.windows.load(Ordering::SeqCst), 1);

    chat.edit("102", "the words as edited");
    let (status, body) = call_as(
        &app,
        INGEST_TOKEN,
        "POST",
        "/api/v1/live/events",
        Some(json!({
            "event_id": "edit-102",
            "historical": false,
            "kind": "update",
            "message": pushed("102", "the words as edited"),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let (status, body) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["102"]}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(play(&app, &body).await, "the words as edited");
    assert_eq!(
        chat.windows.load(Ordering::SeqCst),
        2,
        "the edited message was prepared from memory"
    );
}

#[tokio::test(start_paused = true)]
async fn a_deletion_an_adapter_pushes_is_never_read_aloud_from_memory() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    call(&app, "GET", &page_path(), None).await;

    chat.delete("102");
    let (status, body) = call_as(
        &app,
        INGEST_TOKEN,
        "POST",
        "/api/v1/live/events",
        Some(json!({
            "event_id": "delete-102",
            "historical": false,
            "kind": "delete",
            "channel_id": READ_CHANNEL,
            "message_id": "102",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ["101", "102"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        prepared_ids(&body),
        ["101"],
        "the deleted message was prepared from memory"
    );
    assert_eq!(chat.windows.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_deletion_a_delta_reports_is_never_read_aloud_from_memory() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let timeline = format!("/api/v1/channels/{READ_CHANNEL}/timeline?view=main");
    let (status, body) = call(&app, "GET", &timeline, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cursor = body["next_after"]
        .as_str()
        .expect("forward cursor")
        .to_owned();

    // The page's next refresh learns of the deletion from the delta.
    chat.delete("102");
    let (status, body) = call(&app, "GET", &format!("{timeline}&after={cursor}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delta"]["deleted"], json!(["102"]), "{body}");

    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ["101", "102"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        prepared_ids(&body),
        ["101"],
        "the deleted message was prepared from memory"
    );
    assert_eq!(
        chat.windows.load(Ordering::SeqCst),
        1,
        "the preparation did not go back to the backend for the missing message"
    );
}

#[tokio::test(start_paused = true)]
async fn the_storage_purge_leaves_no_message_text_in_memory() {
    let (app, state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    call(&app, "GET", &page_path(), None).await;
    assert!(!state.read_ahead.is_empty());

    let (status, body) = call_as(&app, WRITE_TOKEN, "DELETE", "/api/v1/storage", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        state.read_ahead.is_empty(),
        "message text outlived the purge"
    );

    let (status, body) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["101"]}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(prepared_ids(&body), ["101"]);
    assert_eq!(chat.windows.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn an_edit_pushed_while_the_page_is_fetching_is_not_undone_when_the_fetch_lands() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    // The page's fetch reads the old text, and lands only after the edit has been pushed.
    let path = page_path();
    let page = call(&app, "GET", &path, None);
    let edit = async {
        tokio::time::sleep(FETCH / 5).await;
        chat.edit("102", "the words as edited");
        call_as(
            &app,
            INGEST_TOKEN,
            "POST",
            "/api/v1/live/events",
            Some(json!({
                "event_id": "edit-102",
                "historical": false,
                "kind": "update",
                "message": pushed("102", "the words as edited"),
            })),
        )
        .await
    };
    let ((page_status, _), (edit_status, edit_body)) = tokio::join!(page, edit);
    assert_eq!(page_status, StatusCode::OK);
    assert_eq!(edit_status, StatusCode::ACCEPTED, "{edit_body}");

    let (status, body) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["102"]}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        play(&app, &body).await,
        "the words as edited",
        "the fetch that was in flight put the old text back"
    );
    // The rest of what that fetch read is still answered from memory.
    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ["101", "103"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(prepared_ids(&body), ["101", "103"]);
    assert_eq!(chat.windows.load(Ordering::SeqCst), 2);
}

/// The same through the channel's ordinary read (`ops::messages`), which remembers what it read
/// for read-aloud as the page's walk does.
#[tokio::test(start_paused = true)]
async fn an_edit_pushed_while_a_channel_read_is_fetching_is_not_undone_when_the_fetch_lands() {
    let (app, _state, chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let path = format!("/api/v1/channels/{READ_CHANNEL}/messages");
    let read = call(&app, "GET", &path, None);
    let edit = async {
        tokio::time::sleep(FETCH / 5).await;
        chat.edit("102", "the words as edited");
        call_as(
            &app,
            INGEST_TOKEN,
            "POST",
            "/api/v1/live/events",
            Some(json!({
                "event_id": "edit-102",
                "historical": false,
                "kind": "update",
                "message": pushed("102", "the words as edited"),
            })),
        )
        .await
    };
    let ((read_status, read_body), (edit_status, edit_body)) = tokio::join!(read, edit);
    assert_eq!(read_status, StatusCode::OK, "{read_body}");
    assert_eq!(edit_status, StatusCode::ACCEPTED, "{edit_body}");
    assert_eq!(
        read_body["messages"][1]["content"], "words of message 102",
        "the read was meant to land with the text from before the edit"
    );

    let (status, body) = call(&app, "POST", &prepare_path(), Some(json!({"ids": ["102"]}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        play(&app, &body).await,
        "the words as edited",
        "the channel read that was in flight put the old text back"
    );
    let (status, body) = call(
        &app,
        "POST",
        &prepare_path(),
        Some(json!({"ids": ["101", "103"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(prepared_ids(&body), ["101", "103"]);
    assert_eq!(chat.windows.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_purge_while_the_page_is_fetching_leaves_nothing_behind_when_the_fetch_lands() {
    let (app, state, _chat) = harness(Arc::new(TimedSpeech::new(Duration::ZERO)));
    let path = page_path();
    let page = call(&app, "GET", &path, None);
    let purge = async {
        tokio::time::sleep(FETCH / 5).await;
        call_as(&app, WRITE_TOKEN, "DELETE", "/api/v1/storage", None).await
    };
    let ((page_status, _), (purge_status, purge_body)) = tokio::join!(page, purge);
    assert_eq!(page_status, StatusCode::OK);
    assert_eq!(purge_status, StatusCode::OK, "{purge_body}");
    assert!(
        state.read_ahead.is_empty(),
        "a fetch that began before the purge put its text back"
    );
}
