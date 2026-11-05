//! Thread views and writes keep the configured parent channel's authorization boundary.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use vibe_talk::chat::{ChatClient, ChatError, ChatIdentity};
use vibe_talk::model::{ChannelId, Message, MessageId, Reaction, UserId};
use vibe_talk::testing::{self, READ_CHANNEL, READ_TOKEN, WRITE_CHANNEL, WRITE_TOKEN};
use vibe_talk::threads::{
    MessageThread, ThreadSummary, TimelineDelta, TimelinePage, TimelineRequest, TimelineView,
};

const THREAD: &str = "opaque/thread-A";

fn message(id: &str, content: &str, thread: bool) -> Message {
    Message {
        id: MessageId(id.to_owned()),
        channel_id: ChannelId(WRITE_CHANNEL.to_owned()),
        author: "Reader".to_owned(),
        author_id: UserId("7".to_owned()),
        author_is_bot: false,
        timestamp: "2026-09-20T01:00:00Z".to_owned(),
        spoken_time: String::new(),
        reply_to: None,
        content: content.to_owned(),
        spoken_content: String::new(),
        content_html: String::new(),
        noise: false,
        thread: thread.then(|| MessageThread {
            id: THREAD.to_owned(),
            root_message_id: Some(MessageId("10".to_owned())),
            is_root: id == "10",
            reply_count: Some(2),
            reply_count_exact: true,
        }),
        reactions: None,
        heading: None,
    }
}

type RecordedPost = (String, Option<String>, String, Option<String>);

#[derive(Default)]
struct ThreadBackend {
    reads: Mutex<Vec<(String, TimelineRequest)>>,
    posts: Mutex<Vec<RecordedPost>>,
    lookups: Mutex<Vec<(String, String, String)>>,
    /// The thread root's reactions, as the backend reports them. `#219 emoji-reactions`.
    root_reactions: Mutex<Option<Vec<Reaction>>>,
}

impl ThreadBackend {
    fn check_thread(thread: &str) -> Result<(), ChatError> {
        if thread != THREAD {
            return Err(
                ChatError::Refused("thread is outside this channel".to_owned())
                    .with_provider("Google Chat"),
            );
        }
        Ok(())
    }

    fn post(
        &self,
        channel: &ChannelId,
        thread: Option<&str>,
        content: &str,
        reply: Option<&MessageId>,
    ) -> Message {
        self.posts.lock().unwrap().push((
            channel.0.clone(),
            thread.map(str::to_owned),
            content.to_owned(),
            reply.map(|id| id.0.clone()),
        ));
        let mut posted = message("30", content, thread.is_some());
        posted.reply_to = reply.cloned();
        posted
    }
}

#[async_trait::async_trait]
impl ChatClient for ThreadBackend {
    fn provider_name(&self) -> &str {
        "Google Chat"
    }
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
        _limit: u16,
        _before: Option<&MessageId>,
        _after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        Ok(vec![message("20", "main message", false)])
    }

    async fn fetch_timeline(
        &self,
        channel: &ChannelId,
        request: &TimelineRequest,
    ) -> Result<TimelinePage, ChatError> {
        self.reads
            .lock()
            .unwrap()
            .push((channel.0.clone(), request.clone()));
        if let Some(thread) = &request.thread_id {
            Self::check_thread(thread)?;
        }
        let mut root = message("10", "root", true);
        root.reactions = self.root_reactions.lock().unwrap().clone();
        let summary = ThreadSummary {
            id: THREAD.to_owned(),
            root: Some(root.clone()),
            title: "root".to_owned(),
            reply_count: Some(2),
            reply_count_exact: true,
            updated_at: "2026-09-20T01:00:00Z".to_owned(),
            display_name: None,
            summary: None,
        };
        Ok(TimelinePage {
            messages: if request.view == TimelineView::Threads {
                vec![]
            } else {
                vec![root, message("11", "older thread text", true)]
            },
            threads: if request.view == TimelineView::Threads {
                vec![summary.clone()]
            } else {
                vec![]
            },
            thread: (request.view == TimelineView::Thread).then_some(summary),
            has_threads: true,
            has_more: true,
            next_before: Some("opaque cursor/+=".to_owned()),
            notice: None,
            ..TimelinePage::default()
        })
    }

    async fn fetch_thread_message(
        &self,
        channel: &ChannelId,
        thread: &str,
        id: &MessageId,
    ) -> Result<Message, ChatError> {
        Self::check_thread(thread)?;
        self.lookups
            .lock()
            .unwrap()
            .push((channel.0.clone(), thread.to_owned(), id.0.clone()));
        Ok(message(&id.0, "older thread text", true))
    }

    async fn post_message(
        &self,
        channel: &ChannelId,
        content: &str,
        reply: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        Ok(self.post(channel, None, content, reply))
    }

    async fn post_in_thread(
        &self,
        channel: &ChannelId,
        thread: &str,
        content: &str,
        reply: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        Self::check_thread(thread)?;
        Ok(self.post(channel, Some(thread), content, reply))
    }
}

fn harness() -> (axum::Router, Arc<ThreadBackend>) {
    let (mut state, _) = testing::state();
    let backend = Arc::new(ThreadBackend::default());
    state.replace_chat(backend.clone());
    (vibe_talk::http::router(state), backend)
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = request
        .header("content-type", "application/json")
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn timeline_authorizes_parent_before_asking_for_any_thread() {
    let (app, backend) = harness();
    for view in [
        "main",
        "threads",
        "flat",
        "thread&thread_id=opaque%2Fthread-A",
    ] {
        let path = format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view={view}");
        assert_eq!(
            call(&app, "GET", &path, None, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        let unknown = path.replace(WRITE_CHANNEL, "unknown");
        assert_eq!(
            call(&app, "GET", &unknown, Some(READ_TOKEN), None).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert!(backend.reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn timeline_passes_opaque_context_and_stamps_message_and_root_times() {
    let (app, backend) = harness();
    let path=format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view=thread&thread_id=opaque%2Fthread-A&before=opaque%20cursor%2F%2B%3D&limit=999");
    let (status, body) = call(&app, "GET", &path, Some(READ_TOKEN), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["next_before"], "opaque cursor/+=");
    assert_eq!(body["thread"]["id"], THREAD);
    assert_eq!(body["thread"]["reply_count"], 2);
    assert!(!body["messages"][0]["spoken_time"]
        .as_str()
        .unwrap()
        .is_empty());
    assert!(!body["thread"]["root"]["spoken_time"]
        .as_str()
        .unwrap()
        .is_empty());
    let reads = backend.reads.lock().unwrap();
    assert_eq!(reads[0].1.thread_id.as_deref(), Some(THREAD));
    assert_eq!(reads[0].1.before.as_deref(), Some("opaque cursor/+="));
    assert_eq!(reads[0].1.limit, 50);
}

#[tokio::test]
async fn invalid_context_is_rejected_instead_of_falling_back_to_main() {
    let (app, backend) = harness();
    for suffix in [
        "view=thread",
        "view=main&thread_id=a",
        "view=thread&thread_id=",
        "view=flat&before=",
    ] {
        let path = format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?{suffix}");
        assert_eq!(
            call(&app, "GET", &path, Some(READ_TOKEN), None).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(backend.reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn standalone_and_thread_composers_route_without_inventing_reply_targets() {
    let (app, backend) = harness();
    let path = format!("/api/v1/channels/{WRITE_CHANNEL}/reply");
    for body in [
        json!({"text":"new main message"}),
        json!({"text":"new thread message","thread_id":THREAD}),
    ] {
        assert_eq!(
            call(&app, "POST", &path, Some(WRITE_TOKEN), Some(body))
                .await
                .0,
            StatusCode::OK
        );
    }
    let posts = backend.posts.lock().unwrap();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].1, None);
    assert_eq!(posts[1].1.as_deref(), Some(THREAD));
    assert!(posts.iter().all(|p| p.3.is_none()));
}

#[tokio::test]
async fn thread_writes_keep_write_scope_parent_policy_and_membership() {
    let (app, backend) = harness();
    let body = json!({"text":"must not post","thread_id":THREAD});
    for (channel, token, status) in [
        (WRITE_CHANNEL, Some(READ_TOKEN), StatusCode::FORBIDDEN),
        (WRITE_CHANNEL, None, StatusCode::UNAUTHORIZED),
        (READ_CHANNEL, Some(WRITE_TOKEN), StatusCode::FORBIDDEN),
        ("unknown", Some(WRITE_TOKEN), StatusCode::NOT_FOUND),
    ] {
        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/api/v1/channels/{channel}/reply"),
                token,
                Some(body.clone())
            )
            .await
            .0,
            status
        );
    }
    let (_, failure) = call(
        &app,
        "POST",
        &format!("/api/v1/channels/{WRITE_CHANNEL}/reply"),
        Some(WRITE_TOKEN),
        Some(json!({"text":"must not post","thread_id":"foreign-thread"})),
    )
    .await;
    assert!(failure["detail"].as_str().unwrap().contains("Google Chat"));
    assert_eq!(failure["posted"], 0);
    assert!(backend.posts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn every_part_of_a_long_thread_message_stays_in_its_thread() {
    let (app, backend) = harness();
    let text = "message ".repeat(500);
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/v1/channels/{WRITE_CHANNEL}/reply"),
        Some(WRITE_TOKEN),
        Some(json!({"text":text,"thread_id":THREAD,"reply_to":"10"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let posts = backend.posts.lock().unwrap();
    assert!(posts.len() > 1);
    assert!(posts.iter().all(|p| p.1.as_deref() == Some(THREAD)));
    assert_eq!(posts[0].3.as_deref(), Some("10"));
    assert!(posts[1..].iter().all(|p| p.3.is_none()));
    assert_eq!(posts.iter().map(|p| p.2.as_str()).collect::<String>(), text);
}

#[tokio::test]
async fn older_thread_messages_remain_addressable_for_summaries_and_speech() {
    let (app, backend) = harness();
    let prefix = format!("/api/v1/channels/{WRITE_CHANNEL}");
    for suffix in [
        "/messages/11?thread_id=opaque%2Fthread-A",
        "/messages/11/summary?thread_id=opaque%2Fthread-A",
    ] {
        let (status, body) = call(
            &app,
            "GET",
            &(prefix.clone() + suffix),
            Some(READ_TOKEN),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = call(
        &app,
        "POST",
        &(prefix + "/speech/prepare"),
        Some(READ_TOKEN),
        Some(json!({"ids":["11"],"thread_id":THREAD})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["prepared"][0]["message_id"], "11");
    // Two lookups, not three: the summary route remembers the thread message it looked up, as
    // speech preparation does, so the preparation right after it recalls the message rather than
    // asking again. `#208 persistent-cache`.
    assert_eq!(backend.lookups.lock().unwrap().len(), 2);
    let (_, config) = call(&app, "GET", "/api/v1/client-config", Some(READ_TOKEN), None).await;
    assert_eq!(config["threading_supported"], true);
    assert_eq!(config["chat_provider_name"], "Google Chat");
}

#[tokio::test]
async fn a_timeline_marks_noise_on_messages_and_on_thread_roots() {
    // `#196 auto-read-noise`. One filter for the whole page, thread roots included, and evaluated
    // on every read: the rules below are changed between two reads of the same thread.
    let (app, _backend) = harness();
    let path = format!(
        "/api/v1/channels/{WRITE_CHANNEL}/timeline?view=thread&thread_id=opaque%2Fthread-A"
    );
    let (status, body) = call(&app, "GET", &path, Some(READ_TOKEN), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("noise").is_none()),
        "nothing here matches the default rule: {body}"
    );

    let (status, saved) = call(
        &app,
        "PUT",
        "/api/v1/noise-rules",
        Some(WRITE_TOKEN),
        Some(json!({ "rules": ["older thread text", "ROOT"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, body) = call(&app, "GET", &path, Some(READ_TOKEN), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["messages"][1]["content"], "older thread text");
    assert_eq!(body["messages"][1]["noise"], true, "{body}");
    assert_eq!(
        body["thread"]["root"]["noise"], true,
        "a thread root escaped the rules: {body}"
    );
}

// --- `#203 incremental-refresh`: forward reads through the route ------------------------------

/// A backend with a change record, the way a bridge with a journal has one: every message seeded,
/// edited or deleted gets a revision, a newest page names the newest as `fake:{rev}`, and `after`
/// answers exactly what changed since, deletions included. Built WITHOUT a change record
/// (`native: false`) it issues no cursor of its own, which is every backend the generic fallback
/// is for.
struct Journal {
    native: bool,
    state: Mutex<JournalState>,
    reads: Mutex<Vec<TimelineRequest>>,
    /// The one conversation the channel is registered as, when it is: its Main is then that whole
    /// conversation, and it has no child threads.
    scope: Mutex<Option<ThreadSummary>>,
}

#[derive(Default)]
struct JournalState {
    revision: u64,
    messages: Vec<(u64, Message)>,
    deleted: Vec<(u64, MessageId)>,
}

impl Journal {
    fn new(native: bool) -> Self {
        Self {
            native,
            state: Mutex::new(JournalState::default()),
            reads: Mutex::new(Vec::new()),
            scope: Mutex::new(None),
        }
    }

    fn seed(&self, id: &str, minute: u32) {
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        let mut posted = message(id, &format!("message {id}"), false);
        posted.timestamp = format!("2026-10-04T07:{minute:02}:00Z");
        let revision = state.revision;
        state.messages.push((revision, posted));
    }

    fn edit(&self, id: &str, content: &str) {
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        let revision = state.revision;
        let held = state
            .messages
            .iter_mut()
            .find(|(_, m)| m.id.0 == id)
            .unwrap();
        held.0 = revision;
        held.1.content = content.to_owned();
    }

    /// Set a message's reactions, as a backend that sees them records a change. `#219
    /// emoji-reactions`.
    fn react(&self, id: &str, reactions: Option<Vec<Reaction>>) {
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        let revision = state.revision;
        let held = state
            .messages
            .iter_mut()
            .find(|(_, m)| m.id.0 == id)
            .unwrap();
        held.0 = revision;
        held.1.reactions = reactions;
    }

    fn delete(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        let revision = state.revision;
        state.messages.retain(|(_, m)| m.id.0 != id);
        state.deleted.push((revision, MessageId(id.to_owned())));
    }

    fn forwarded(&self) -> Vec<Option<String>> {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.after.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl ChatClient for Journal {
    fn provider_name(&self) -> &str {
        "Example Bridge"
    }
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
        _limit: u16,
        _before: Option<&MessageId>,
        _after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        Ok(Vec::new())
    }
    async fn post_message(
        &self,
        _channel: &ChannelId,
        content: &str,
        _reply: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        Ok(message("99", content, false))
    }
    async fn fetch_timeline(
        &self,
        _channel: &ChannelId,
        request: &TimelineRequest,
    ) -> Result<TimelinePage, ChatError> {
        self.reads.lock().unwrap().push(request.clone());
        let state = self.state.lock().unwrap();
        let mut current: Vec<(u64, Message)> = state.messages.clone();
        current.sort_by(|a, b| a.1.timestamp.cmp(&b.1.timestamp));
        if let Some(after) = &request.after {
            assert!(
                self.native,
                "a backend without a cursor of its own was handed one"
            );
            let since: u64 = match after.strip_prefix("fake:").and_then(|r| r.parse().ok()) {
                Some(since) => since,
                None => {
                    // Labelled, as every real backend labels its failures.
                    return Err(ChatError::CursorExpired(
                        "issued by an earlier process".to_owned(),
                    )
                    .with_provider("Example Bridge"));
                }
            };
            return Ok(TimelinePage {
                messages: current
                    .into_iter()
                    .filter(|(revision, _)| *revision > since)
                    .map(|(_, m)| m)
                    .collect(),
                has_threads: true,
                next_after: Some(format!("fake:{}", state.revision)),
                delta: Some(TimelineDelta {
                    more: false,
                    complete: true,
                    deleted: state
                        .deleted
                        .iter()
                        .filter(|(revision, _)| *revision > since)
                        .map(|(_, id)| id.clone())
                        .collect(),
                    removed_threads: Vec::new(),
                }),
                ..TimelinePage::default()
            });
        }
        let all: Vec<Message> = current.into_iter().map(|(_, m)| m).collect();
        let limit = usize::from(request.limit);
        let (messages, has_more) = if request.before.as_deref() == Some("older") {
            (all[..all.len().saturating_sub(limit)].to_vec(), false)
        } else {
            (
                all[all.len().saturating_sub(limit)..].to_vec(),
                all.len() > limit,
            )
        };
        let scope = self.scope.lock().unwrap().clone();
        Ok(TimelinePage {
            messages,
            has_threads: scope.is_none(),
            thread: scope,
            has_more,
            next_before: has_more.then(|| "older".to_owned()),
            next_after: self.native.then(|| format!("fake:{}", state.revision)),
            ..TimelinePage::default()
        })
    }
}

fn journal(native: bool) -> (axum::Router, Arc<Journal>) {
    let (mut state, _) = testing::state();
    let backend = Arc::new(Journal::new(native));
    state.replace_chat(backend.clone());
    (vibe_talk::http::router(state), backend)
}

fn timeline_path(query: &str) -> String {
    format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?{query}")
}

fn ids(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

fn next_after(body: &Value) -> String {
    let cursor = body["next_after"]
        .as_str()
        .expect("a forward cursor")
        .to_owned();
    assert!(
        !cursor.contains("fake:"),
        "the page saw the backend's own cursor: {cursor}"
    );
    cursor
}

#[tokio::test]
async fn a_backend_with_a_change_record_answers_edits_and_deletions_since_the_last_read() {
    let (app, backend) = journal(true);
    for (id, minute) in [("1", 0), ("2", 1), ("3", 2)] {
        backend.seed(id, minute);
    }
    let (status, newest) = call(
        &app,
        "GET",
        &timeline_path("view=main"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{newest}");
    assert_eq!(ids(&newest), ["1", "2", "3"]);
    assert!(newest["delta"].is_null(), "a newest page is not a delta");
    let cursor = next_after(&newest);

    backend.seed("4", 3);
    backend.edit("2", "corrected");
    backend.delete("3");
    let (status, delta) = call(
        &app,
        "GET",
        &timeline_path(&format!("view=main&after={cursor}")),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{delta}");
    assert_eq!(ids(&delta), ["2", "4"], "only what changed, oldest first");
    assert_eq!(delta["messages"][0]["content"], "corrected");
    assert!(!delta["messages"][0]["spoken_time"]
        .as_str()
        .unwrap()
        .is_empty());
    assert_eq!(
        delta["delta"],
        json!({"more": false, "complete": true, "deleted": ["3"], "removed_threads": []})
    );
    assert_eq!(delta["has_more"], false);
    assert!(delta["next_before"].is_null());
    assert_eq!(delta["returned"], 2);
    // The backend was handed its OWN cursor, unwrapped, and never the envelope.
    assert_eq!(backend.forwarded(), [None, Some("fake:3".to_owned())]);

    let (_, quiet) = call(
        &app,
        "GET",
        &timeline_path(&format!("view=main&after={}", next_after(&delta))),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert!(
        ids(&quiet).is_empty(),
        "nothing changed, nothing returned: {quiet}"
    );
    assert_eq!(quiet["delta"]["complete"], true);
    assert_eq!(
        backend.forwarded().last().unwrap().as_deref(),
        Some("fake:6")
    );
}

#[tokio::test]
async fn a_backend_without_a_cursor_of_its_own_is_caught_up_generically() {
    let (app, backend) = journal(false);
    backend.seed("1", 0);
    backend.seed("2", 1);
    let (_, newest) = call(
        &app,
        "GET",
        &timeline_path("view=main"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    let cursor = next_after(&newest);
    backend.seed("3", 2);
    backend.edit("1", "an edit a creation-time read cannot see");
    let (status, delta) = call(
        &app,
        "GET",
        &timeline_path(&format!("view=main&after={cursor}")),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{delta}");
    assert_eq!(ids(&delta), ["3"]);
    assert_eq!(
        delta["delta"],
        json!({"more": false, "complete": false, "deleted": [], "removed_threads": []}),
        "a generic catch-up promises additions only"
    );
    let (_, quiet) = call(
        &app,
        "GET",
        &timeline_path(&format!("view=main&after={}", next_after(&delta))),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert!(ids(&quiet).is_empty(), "{quiet}");
    assert!(
        backend.forwarded().iter().all(Option::is_none),
        "a generic cursor reached the backend"
    );
}

#[tokio::test]
async fn a_forward_cursor_is_refused_anywhere_but_the_read_it_came_from() {
    let (app, backend) = journal(true);
    backend.seed("1", 0);
    let (_, newest) = call(
        &app,
        "GET",
        &timeline_path("view=main"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    let cursor = next_after(&newest);
    let reads = backend.forwarded().len();
    for (path, code) in [
        (
            timeline_path(&format!("view=flat&after={cursor}")),
            "cursor_mismatch",
        ),
        (
            timeline_path(&format!("view=thread&thread_id=t&after={cursor}")),
            "cursor_mismatch",
        ),
        (
            format!("/api/v1/channels/{READ_CHANNEL}/timeline?view=main&after={cursor}"),
            "cursor_mismatch",
        ),
        (
            timeline_path("view=main&after=not-a-cursor"),
            "cursor_mismatch",
        ),
        (
            timeline_path(&format!("view=main&before=older&after={cursor}")),
            "bad_request",
        ),
        (timeline_path("view=main&after="), "bad_request"),
    ] {
        let (status, body) = call(&app, "GET", &path, Some(READ_TOKEN), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["error"], code, "{path}: {body}");
    }
    assert_eq!(
        backend.forwarded().len(),
        reads,
        "a refused cursor reached the backend"
    );
}

#[tokio::test]
async fn a_cursor_the_backend_cannot_continue_answers_410_cursor_expired() {
    use vibe_talk::timeline_forward::{encode, Forward};
    let (app, _backend) = journal(true);
    let expired = encode(
        &ChannelId(WRITE_CHANNEL.to_owned()),
        TimelineView::Main,
        None,
        &Forward::Native("issued-before-a-restart".to_owned()),
    )
    .unwrap();
    let (status, body) = call(
        &app,
        "GET",
        &timeline_path(&format!("view=main&after={expired}")),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(body["error"], "cursor_expired");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("Example Bridge can no longer continue"),
        "{body}"
    );
}

#[tokio::test]
async fn a_step_back_carries_no_forward_cursor_and_a_delta_reports_only_its_own_dismissals() {
    let (app, backend) = journal(true);
    for (id, minute) in [("1", 0), ("2", 1), ("3", 2)] {
        backend.seed(id, minute);
    }
    let (_, newest) = call(
        &app,
        "GET",
        &timeline_path("view=main&limit=2"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(newest["next_before"], "older");
    let cursor = next_after(&newest);
    let (_, older) = call(
        &app,
        "GET",
        &timeline_path("view=main&limit=2&before=older"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(ids(&older), ["1"]);
    assert!(
        older.get("next_after").is_none(),
        "a step back says nothing about the newest end: {older}"
    );

    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/v1/channels/{WRITE_CHANNEL}/dismiss"),
        Some(WRITE_TOKEN),
        Some(json!({"messages": ["1", "4"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    backend.seed("4", 3);
    let (_, delta) = call(
        &app,
        "GET",
        &timeline_path(&format!("view=main&limit=2&after={cursor}")),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(ids(&delta), ["4"]);
    assert_eq!(
        delta["dismissed"],
        json!(["4"]),
        "dismissals outside the delta leaked in"
    );
}

// --- `#222 unread-replies`: the replies' read state through Main --------------------------------

async fn mark(app: &axum::Router, act: &str, ids: &[&str]) {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/v1/channels/{WRITE_CHANNEL}/{act}"),
        Some(WRITE_TOKEN),
        Some(json!({ "messages": ids })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{act}: {body}");
}

#[tokio::test]
async fn main_says_which_messages_are_done_from_its_oldest_row_up_and_nothing_else_does() {
    // Main shows a thread's root and none of its replies, so its rows' own `dismissed` cannot tell
    // a page that a reply it holds from All was marked Done, or put back, on another device.
    let (app, backend) = journal(true);
    for (id, minute) in [("2", 0), ("3", 1), ("4", 2)] {
        backend.seed(id, minute);
    }
    // One mark older than the page, one on it, and two on replies Main never shows.
    mark(&app, "dismiss", &["1", "3", "5", "6"]).await;
    let (status, main) = call(
        &app,
        "GET",
        &timeline_path("view=main"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{main}");
    assert_eq!(
        main["dismissed"],
        json!(["3"]),
        "the rows' own marks changed"
    );
    assert_eq!(
        main["reply_dismissals"],
        json!({"from": "2", "messages": ["6", "5", "3"]}),
        "not every mark from the oldest row up, newest first"
    );
    // Put back on another device: complete from `from` up, so its absence says so.
    mark(&app, "restore", &["5"]).await;
    let (_, again) = call(
        &app,
        "GET",
        &timeline_path("view=main"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(again["reply_dismissals"]["messages"], json!(["6", "3"]));
    // All carries every reply as a row of its own, and a thread view every message of its thread.
    for view in ["flat", "thread&thread_id=opaque%2Fthread-A"] {
        let (_, other) = call(
            &app,
            "GET",
            &timeline_path(&format!("view={view}")),
            Some(READ_TOKEN),
            None,
        )
        .await;
        assert!(
            other.get("reply_dismissals").is_none(),
            "view={view} carried Main's list: {other}"
        );
    }
    // A step back's roots are older than anything waiting; a delta's rows say it from their oldest.
    let (_, newest) = call(
        &app,
        "GET",
        &timeline_path("view=main&limit=2"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(newest["reply_dismissals"]["from"], "3");
    let cursor = next_after(&newest);
    let (_, older) = call(
        &app,
        "GET",
        &timeline_path("view=main&limit=2&before=older"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert!(
        older.get("reply_dismissals").is_none(),
        "a step back carried it: {older}"
    );
    backend.seed("7", 3);
    mark(&app, "dismiss", &["8"]).await;
    let (_, delta) = call(
        &app,
        "GET",
        &timeline_path(&format!("view=main&limit=2&after={cursor}")),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(ids(&delta), ["7"]);
    assert_eq!(
        delta["reply_dismissals"],
        json!({"from": "7", "messages": ["8"]})
    );
}

#[tokio::test]
async fn a_channel_scoped_to_one_conversation_has_no_replies_outside_main_to_speak_for() {
    // Its Main is the whole conversation, replies and all, so `dismissed` already covers them.
    let (app, backend) = journal(false);
    backend.seed("2", 0);
    *backend.scope.lock().unwrap() = Some(ThreadSummary {
        id: THREAD.to_owned(),
        root: None,
        title: "the conversation".to_owned(),
        reply_count: None,
        reply_count_exact: false,
        updated_at: "2026-10-04T07:00:00Z".to_owned(),
        display_name: None,
        summary: None,
    });
    mark(&app, "dismiss", &["2", "5"]).await;
    let (status, main) = call(
        &app,
        "GET",
        &timeline_path("view=main"),
        Some(READ_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{main}");
    assert_eq!(main["dismissed"], json!(["2"]));
    assert!(
        main.get("reply_dismissals").is_none(),
        "a scoped channel carried it: {main}"
    );
}

// --- `#219 emoji-reactions`: reactions through the route -----------------------------------------

fn tally(emoji: &str, count: u32) -> Reaction {
    Reaction {
        emoji: emoji.to_owned(),
        custom: false,
        custom_id: None,
        count,
    }
}

fn custom_tally(name: &str, id: &str, count: u32) -> Reaction {
    Reaction {
        emoji: name.to_owned(),
        custom: true,
        custom_id: Some(id.to_owned()),
        count,
    }
}

#[tokio::test]
async fn reactions_reach_the_page_and_a_reaction_alone_is_a_change_in_the_next_delta() {
    // Both kinds of backend: one with a change record, which reports the reaction itself, and one
    // without, whose generic catch-up must see a message with new tallies as changed.
    for native in [true, false] {
        let (app, backend) = journal(native);
        backend.seed("1", 0);
        backend.seed("2", 1);
        backend.react("2", Some(Vec::new()));
        let (status, newest) = call(
            &app,
            "GET",
            &timeline_path("view=main"),
            Some(READ_TOKEN),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{newest}");
        assert!(
            newest["messages"][0].get("reactions").is_none(),
            "a copy that cannot see reactions claimed an answer (native {native}): {newest}"
        );
        assert_eq!(
            newest["messages"][1]["reactions"],
            json!([]),
            "known-none was not carried as an empty list (native {native})"
        );
        let cursor = next_after(&newest);

        backend.react(
            "2",
            Some(vec![
                tally("👀", 1),
                custom_tally("party-parrot", "112233", 2),
            ]),
        );
        let (status, delta) = call(
            &app,
            "GET",
            &timeline_path(&format!("view=main&after={cursor}")),
            Some(READ_TOKEN),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{delta}");
        assert_eq!(
            ids(&delta),
            ["2"],
            "a reaction-only change was not a change (native {native})"
        );
        assert_eq!(
            delta["messages"][0]["reactions"],
            json!([
                {"emoji": "👀", "count": 1},
                {"emoji": "party-parrot", "custom": true, "custom_id": "112233", "count": 2},
            ])
        );
        let (_, quiet) = call(
            &app,
            "GET",
            &timeline_path(&format!("view=main&after={}", next_after(&delta))),
            Some(READ_TOKEN),
            None,
        )
        .await;
        assert!(
            ids(&quiet).is_empty(),
            "unchanged reactions came back (native {native}): {quiet}"
        );
    }
}

#[tokio::test]
async fn a_thread_root_carries_its_reactions_in_every_view_that_shows_it() {
    let (app, backend) = harness();
    *backend.root_reactions.lock().unwrap() = Some(vec![tally("✅", 1)]);
    for view in ["main", "threads", "thread&thread_id=opaque%2Fthread-A"] {
        let path = format!("/api/v1/channels/{WRITE_CHANNEL}/timeline?view={view}");
        let (status, body) = call(&app, "GET", &path, Some(READ_TOKEN), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let root = if view == "threads" {
            &body["threads"][0]["root"]
        } else {
            &body["messages"][0]
        };
        assert_eq!(root["id"], "10", "{body}");
        assert_eq!(
            root["reactions"],
            json!([{"emoji": "✅", "count": 1}]),
            "{view}: {body}"
        );
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .all(|m| m.get("reactions").is_none()),
            "{view}: a reply was given its root's reactions: {body}"
        );
    }
}
