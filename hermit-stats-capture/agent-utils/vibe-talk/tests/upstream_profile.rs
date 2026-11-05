//! `#208 persistent-cache`, milestone `upstream-profile`: every upstream operation is timed, every
//! Slack Web API call is counted as the far end counts it, and none of it carries channel text.
//!
//! The first test is an INVENTORY, checked against `src/chat.rs` in the style of
//! `tests/scope_first.rs`. [`ProfiledClient`] wraps every provider's client, and
//! [`vibe_talk::providers::ChatRouter`] fronts them all. A `ChatClient` method either of them
//! leaves to the trait's default body runs that default on the wrapper, which calls the wrapper's
//! other methods and silently skips whatever the provider overrides. So a method added to the
//! trait fails here until both forward it.

mod slack_fake;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use slack_fake::{profiled, ts, Fake, FakeState, METHODS, TOKEN};
use tower::ServiceExt as _;
use vibe_talk::chat::{
    ChatClient, ChatError, ChatIdentity, PostLimit, RecentRead, RegisteredChannel, SourceClaim,
    UpstreamClearOutcome,
};
use vibe_talk::directory::{DirectoryPage, DirectoryRequest};
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::model::{ChannelId, Message, MessageId};
use vibe_talk::providers::ChatRouter;
use vibe_talk::threads::{TimelinePage, TimelineRequest};
use vibe_talk::timeline_forward::{GenericDelta, GenericPosition};
use vibe_talk::upstream_profile::{Outcome, ProfiledClient, UpstreamProfile};

const CHAT_SOURCE: &str = include_str!("../src/chat.rs");
const ROUTER_SOURCE: &str = include_str!("../src/providers.rs");
const PROFILE_SOURCE: &str = include_str!("../src/upstream_profile.rs");

/// The code directly inside the block `header` opens: comments, string and character literals,
/// and every nested block left out.
fn top_level_of(source: &str, header: &str) -> String {
    assert!(header.ends_with('{'), "a header opens a block: {header}");
    let start = source
        .find(header)
        .unwrap_or_else(|| panic!("no `{header}` in the source"))
        + header.len();
    let chars: Vec<char> = source[start..].chars().collect();
    let identifier = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::new();
    let mut depth = 0_usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '/' if next == Some('/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push('\n');
                continue;
            }
            '/' if next == Some('*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
                out.push(' ');
                continue;
            }
            'r' if matches!(next, Some('#' | '"')) && (i == 0 || !identifier(chars[i - 1])) => {
                let mut j = i + 1;
                let mut hashes = 0;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if chars.get(j) == Some(&'"') {
                    j += 1;
                    loop {
                        assert!(j < chars.len(), "an unterminated raw string");
                        if chars[j] == '"' && chars[j + 1..].iter().take(hashes).all(|c| *c == '#')
                        {
                            break;
                        }
                        j += 1;
                    }
                    i = j + 1 + hashes;
                    out.push(' ');
                    continue;
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                out.push(' ');
                continue;
            }
            '\'' if next == Some('\\') => {
                i += 2;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
                out.push(' ');
                continue;
            }
            '\'' if chars.get(i + 2) == Some(&'\'') => {
                i += 3;
                out.push(' ');
                continue;
            }
            '{' => {
                depth += 1;
                i += 1;
                continue;
            }
            '}' => {
                if depth == 0 {
                    return out;
                }
                depth -= 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        if depth == 0 {
            out.push(c);
        }
        i += 1;
    }
    panic!("the block `{header}` opens is never closed");
}

/// Every `fn` declared in `code`, and the `async` ones among them.
fn fn_names(code: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let identifier = |c: char| c.is_alphanumeric() || c == '_';
    let mut all = BTreeSet::new();
    let mut asynchronous = BTreeSet::new();
    for (at, _) in code.match_indices("fn ") {
        if code[..at].chars().next_back().is_some_and(identifier) {
            continue;
        }
        let name: String = code[at + 3..]
            .trim_start()
            .chars()
            .take_while(|c| identifier(*c))
            .collect();
        assert!(!name.is_empty(), "a `fn` with no name in {code}");
        if code[..at].trim_end().ends_with("async") {
            asynchronous.insert(name.clone());
        }
        all.insert(name);
    }
    (all, asynchronous)
}

/// Every method of `trait ChatClient`, and the `async` ones: those that may reach a provider.
fn trait_methods() -> (BTreeSet<String>, BTreeSet<String>) {
    fn_names(&top_level_of(
        CHAT_SOURCE,
        "pub trait ChatClient: Send + Sync {",
    ))
}

#[test]
fn the_inventory_reader_sees_declarations_and_nothing_else() {
    // The reader is the test's own weak point: one that found nothing would pass everything.
    let sample = r##"
        pub trait T {
            /// `{a, b}` in a comment is not a block, and `fn hidden(` here is not a method.
            fn plain(&self) -> bool { let s = "fn fake() {"; let c = '{'; s.is_empty() && c == '}' }
            async fn read(&self, f: fn(&str) -> u8) -> Result<(), E>;
            fn nested(&self) { fn inner() {} let r = r#"} fn raw() {"#; }
            // fn commented_out(&self);
        }
        fn after_the_block() {}
    "##;
    let (all, asynchronous) = fn_names(&top_level_of(sample, "pub trait T {"));
    assert_eq!(
        all.into_iter().collect::<Vec<_>>(),
        ["nested", "plain", "read"]
    );
    assert_eq!(asynchronous.into_iter().collect::<Vec<_>>(), ["read"]);

    let (all, asynchronous) = trait_methods();
    for known in [
        "provider_name",
        "identity",
        "fetch_page",
        "fetch_timeline_since",
        "clear_upstream_through",
    ] {
        assert!(all.contains(known), "{known} not found in {all:?}");
    }
    assert!(asynchronous.contains("fetch_page") && !asynchronous.contains("provider_name"));
    assert!(
        all.len() >= 27,
        "the trait had 27 methods when this was written: {all:?}"
    );
}

#[test]
fn the_profiled_client_and_the_router_forward_every_chat_client_method() {
    let (methods, _) = trait_methods();
    for (file, source, header) in [
        (
            "src/upstream_profile.rs",
            PROFILE_SOURCE,
            "impl ChatClient for ProfiledClient {",
        ),
        (
            "src/providers.rs",
            ROUTER_SOURCE,
            "impl ChatClient for ChatRouter {",
        ),
    ] {
        let (defined, _) = fn_names(&top_level_of(source, header));
        let missing: Vec<&String> = methods.difference(&defined).collect();
        assert!(
            missing.is_empty(),
            "`{header}` in {file} leaves {missing:?} to the trait's default body. A default runs on \
             the wrapper and skips the provider's own override, so forward each one explicitly to \
             the wrapped client."
        );
    }
}

/// A client that overrides every `ChatClient` method, records which one ran, and answers with
/// something no default would.
struct Recorder {
    calls: Mutex<Vec<&'static str>>,
    message: Message,
}

impl Recorder {
    async fn new() -> Self {
        let fake = FakeDiscord::new();
        let channel = ChannelId("C1".to_owned());
        fake.seed(&channel, "ada", "words of some length");
        let message = fake.fetch_recent(&channel, 1).await.expect("seeded")[0].clone();
        Self {
            calls: Mutex::new(Vec::new()),
            message,
        }
    }

    fn note(&self, name: &'static str) {
        self.calls.lock().expect("calls").push(name);
    }

    fn take(&self) -> Vec<&'static str> {
        std::mem::take(&mut *self.calls.lock().expect("calls"))
    }

    fn page(&self) -> TimelinePage {
        TimelinePage {
            messages: vec![self.message.clone()],
            ..TimelinePage::default()
        }
    }
}

#[async_trait]
impl ChatClient for Recorder {
    fn provider_name(&self) -> &str {
        self.note("provider_name");
        "Recorder"
    }
    fn claims_source(&self, _source: &str) -> SourceClaim {
        self.note("claims_source");
        SourceClaim::Certain
    }
    fn self_author_id(&self) -> Option<String> {
        self.note("self_author_id");
        Some("self".to_owned())
    }
    fn owner_author_id(&self) -> Option<String> {
        self.note("owner_author_id");
        Some("owner".to_owned())
    }
    fn supports_threading(&self) -> bool {
        self.note("supports_threading");
        true
    }
    fn replies_in_thread(&self) -> bool {
        self.note("replies_in_thread");
        true
    }
    async fn fetch_timeline(
        &self,
        _channel: &ChannelId,
        _request: &TimelineRequest,
    ) -> Result<TimelinePage, ChatError> {
        self.note("fetch_timeline");
        Ok(self.page())
    }
    async fn fetch_timeline_since(
        &self,
        _channel: &ChannelId,
        _request: &TimelineRequest,
        since: &GenericPosition,
    ) -> Result<GenericDelta, ChatError> {
        self.note("fetch_timeline_since");
        Ok(GenericDelta {
            page: self.page(),
            position: since.clone(),
        })
    }
    async fn post_in_thread(
        &self,
        _channel: &ChannelId,
        _thread_id: &str,
        _content: &str,
        _reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.note("post_in_thread");
        Ok(self.message.clone())
    }
    async fn fetch_thread_message(
        &self,
        _channel: &ChannelId,
        _thread_id: &str,
        _message_id: &MessageId,
    ) -> Result<Message, ChatError> {
        self.note("fetch_thread_message");
        Ok(self.message.clone())
    }
    fn supports_channel_registration(&self) -> bool {
        self.note("supports_channel_registration");
        true
    }
    async fn register_channel(
        &self,
        _source: &str,
        _label: &str,
    ) -> Result<RegisteredChannel, ChatError> {
        self.note("register_channel");
        Ok(RegisteredChannel {
            id: ChannelId("C2".to_owned()),
            created: true,
            writable: true,
        })
    }
    async fn unregister_channel(&self, _channel: &ChannelId) -> Result<(), ChatError> {
        self.note("unregister_channel");
        Ok(())
    }
    fn supports_channel_discovery(&self) -> bool {
        self.note("supports_channel_discovery");
        true
    }
    async fn discover_channels(
        &self,
        _request: &DirectoryRequest,
    ) -> Result<DirectoryPage, ChatError> {
        self.note("discover_channels");
        Ok(DirectoryPage {
            entries: Vec::new(),
            next_cursor: Some("next".to_owned()),
            truncated: true,
        })
    }
    fn supports_upstream_read_mark(&self) -> bool {
        self.note("supports_upstream_read_mark");
        true
    }
    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        self.note("identity");
        Ok(ChatIdentity {
            id: "U1".to_owned(),
            username: "recorder".to_owned(),
        })
    }
    async fn fetch_page(
        &self,
        _channel: &ChannelId,
        _limit: u16,
        _before: Option<&MessageId>,
        _after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        self.note("fetch_page");
        Ok(vec![self.message.clone()])
    }
    async fn probe_channel(
        &self,
        _channel: &ChannelId,
        _limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        self.note("probe_channel");
        Ok(vec![self.message.clone()])
    }
    async fn fetch_recent(
        &self,
        _channel: &ChannelId,
        _limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        self.note("fetch_recent");
        Ok(vec![self.message.clone()])
    }
    async fn fetch_recent_dated(
        &self,
        _channel: &ChannelId,
        _limit: u16,
    ) -> Result<RecentRead, ChatError> {
        self.note("fetch_recent_dated");
        Ok(RecentRead {
            messages: vec![self.message.clone()],
            as_of: Some("2026-10-12T09:00:00Z".to_owned()),
            exhausted: false,
        })
    }
    async fn post_message(
        &self,
        _channel: &ChannelId,
        _content: &str,
        _reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.note("post_message");
        Ok(self.message.clone())
    }
    fn supports_idempotent_posts(&self) -> bool {
        self.note("supports_idempotent_posts");
        true
    }
    async fn post_message_keyed(
        &self,
        _channel: &ChannelId,
        _content: &str,
        _reply_to: Option<&MessageId>,
        _key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.note("post_message_keyed");
        Ok(self.message.clone())
    }
    async fn post_in_thread_keyed(
        &self,
        _channel: &ChannelId,
        _thread_id: &str,
        _content: &str,
        _reply_to: Option<&MessageId>,
        _key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.note("post_in_thread_keyed");
        Ok(self.message.clone())
    }
    fn post_limit(&self, _channel: &ChannelId) -> PostLimit {
        self.note("post_limit");
        PostLimit::bytes(4321)
    }
    fn continuation_thread(&self, _channel: &ChannelId, _first: &Message) -> Option<String> {
        self.note("continuation_thread");
        Some("t-continued".to_owned())
    }
    async fn mark_read_upstream(
        &self,
        _channel: &ChannelId,
        _through: &MessageId,
    ) -> Result<(), ChatError> {
        self.note("mark_read_upstream");
        Ok(())
    }
    fn supports_upstream_clear(&self) -> bool {
        self.note("supports_upstream_clear");
        true
    }
    async fn clear_upstream_through(
        &self,
        _channel: &ChannelId,
        through: &MessageId,
        dry_run: bool,
    ) -> Result<UpstreamClearOutcome, ChatError> {
        self.note("clear_upstream_through");
        Ok(UpstreamClearOutcome {
            scope: "conversation".to_owned(),
            target: through.0.clone(),
            previous: None,
            advanced: true,
            dry_run,
        })
    }
}

#[tokio::test]
async fn every_method_reaches_the_wrapped_clients_own_override_and_each_upstream_one_is_timed() {
    let recorder = Arc::new(Recorder::new().await);
    let profile = UpstreamProfile::new();
    let client = ProfiledClient::new(
        Arc::clone(&recorder) as Arc<dyn ChatClient>,
        profile.provider("rec", "Recorder"),
    );
    let channel = ChannelId("C1".to_owned());
    let id = recorder.message.id.clone();
    let request = TimelineRequest::default();
    let mut exercised = BTreeSet::new();
    // Exactly the one override ran: a method left to the trait's default would run the default
    // on the wrapper, which records nothing here or records the method the default calls.
    let mut ran = |name: &'static str| {
        assert_eq!(
            recorder.take(),
            [name],
            "{name} did not reach the wrapped client's own {name}"
        );
        exercised.insert(name.to_owned());
    };

    assert_eq!(client.provider_name(), "Recorder");
    ran("provider_name");
    assert_eq!(client.claims_source("anything"), SourceClaim::Certain);
    ran("claims_source");
    assert_eq!(client.self_author_id().as_deref(), Some("self"));
    ran("self_author_id");
    assert_eq!(client.owner_author_id().as_deref(), Some("owner"));
    ran("owner_author_id");
    assert!(client.supports_threading());
    ran("supports_threading");
    assert!(client.replies_in_thread());
    ran("replies_in_thread");
    assert_eq!(
        client
            .fetch_timeline(&channel, &request)
            .await
            .expect("timeline")
            .messages
            .len(),
        1
    );
    ran("fetch_timeline");
    client
        .fetch_timeline_since(&channel, &request, &GenericPosition::origin())
        .await
        .expect("since");
    ran("fetch_timeline_since");
    client
        .post_in_thread(&channel, "t", "hi", None)
        .await
        .expect("post");
    ran("post_in_thread");
    client
        .fetch_thread_message(&channel, "t", &id)
        .await
        .expect("lookup");
    ran("fetch_thread_message");
    assert!(client.supports_channel_registration());
    ran("supports_channel_registration");
    assert!(
        client
            .register_channel("source", "label")
            .await
            .expect("register")
            .created
    );
    ran("register_channel");
    client.unregister_channel(&channel).await.expect("remove");
    ran("unregister_channel");
    assert!(client.supports_channel_discovery());
    ran("supports_channel_discovery");
    let directory = client
        .discover_channels(&DirectoryRequest {
            query: None,
            cursor: None,
            limit: 5,
        })
        .await
        .expect("directory");
    assert!(directory.truncated);
    ran("discover_channels");
    assert!(client.supports_upstream_read_mark());
    ran("supports_upstream_read_mark");
    assert_eq!(client.identity().await.expect("identity").id, "U1");
    ran("identity");
    client
        .fetch_page(&channel, 5, None, None)
        .await
        .expect("page");
    ran("fetch_page");
    client.probe_channel(&channel, 5).await.expect("probe");
    ran("probe_channel");
    client.fetch_recent(&channel, 5).await.expect("recent");
    ran("fetch_recent");
    let dated = client
        .fetch_recent_dated(&channel, 5)
        .await
        .expect("dated recent");
    assert_eq!(dated.as_of.as_deref(), Some("2026-10-12T09:00:00Z"));
    assert_eq!(dated.messages.len(), 1);
    ran("fetch_recent_dated");
    client
        .post_message(&channel, "hi", None)
        .await
        .expect("post");
    ran("post_message");
    assert!(client.supports_idempotent_posts());
    ran("supports_idempotent_posts");
    client
        .post_message_keyed(&channel, "hi", None, Some("k"))
        .await
        .expect("keyed post");
    ran("post_message_keyed");
    client
        .post_in_thread_keyed(&channel, "t", "hi", None, Some("k"))
        .await
        .expect("keyed thread post");
    ran("post_in_thread_keyed");
    assert_eq!(client.post_limit(&channel), PostLimit::bytes(4321));
    ran("post_limit");
    assert_eq!(
        client
            .continuation_thread(&channel, &recorder.message)
            .as_deref(),
        Some("t-continued")
    );
    ran("continuation_thread");
    client
        .mark_read_upstream(&channel, &id)
        .await
        .expect("mark");
    ran("mark_read_upstream");
    assert!(client.supports_upstream_clear());
    ran("supports_upstream_clear");
    assert!(
        client
            .clear_upstream_through(&channel, &id, true)
            .await
            .expect("clear")
            .dry_run
    );
    ran("clear_upstream_through");

    let (methods, upstream) = trait_methods();
    assert_eq!(
        exercised, methods,
        "every ChatClient method must be called through the wrapper here"
    );

    let report = profile.report();
    let rec = &report.providers[0];
    let timed: BTreeSet<String> = rec
        .operations
        .iter()
        .map(|row| row.name.to_owned())
        .collect();
    assert_eq!(
        timed, upstream,
        "every async method is timed, and only those: a capability question is not an upstream call"
    );
    assert!(rec
        .operations
        .iter()
        .all(|row| row.outcome == Outcome::Ok && row.count == 1 && row.p50_ms >= 0.0));
    let rows = |name: &str| {
        rec.operations
            .iter()
            .find(|row| row.name == name)
            .and_then(|row| row.rows)
    };
    assert_eq!(rows("fetch_recent"), Some(1));
    assert_eq!(rows("fetch_recent_dated"), Some(1));
    assert_eq!(
        rows("discover_channels"),
        Some(0),
        "an empty directory page"
    );
    // Seven reads each return the one message; posts return ours and are not counted.
    let bytes = recorder.message.content.len() as u64;
    assert_eq!(rec.content_bytes.messages, 7, "{:?}", rec.content_bytes);
    assert_eq!(rec.content_bytes.total, 7 * bytes);
    assert_eq!(rec.content_bytes.max, bytes);
    assert!(
        rec.shim_calls.is_empty(),
        "this client times no calls of its own"
    );
}

#[tokio::test]
async fn a_failure_is_timed_under_its_outcome_and_returned_unchanged() {
    let fake = Arc::new(FakeDiscord::new());
    fake.revoke_token();
    let router = ChatRouter::single(fake, "ns", 0);
    let error = router.identity().await.expect_err("revoked");
    assert!(matches!(
        error.cause(),
        ChatError::Status { status: 401, .. }
    ));
    let report = router.profile().report();
    let row = report.providers[0]
        .operations
        .iter()
        .find(|row| row.name == "identity")
        .expect("identity timed");
    assert_eq!(row.outcome, Outcome::Unauthorized);
    assert_eq!(row.count, 1);
    assert_eq!(row.rows, Some(0));
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(
                    "authorization",
                    format!("Bearer {}", vibe_talk::testing::READ_TOKEN),
                )
                .body(Body::empty())
                .expect("request"),
        )
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

#[tokio::test]
async fn diagnostics_show_each_providers_timings_and_sizes_and_no_channel_text() {
    let (state, discord) = vibe_talk::testing::state();
    let channel = ChannelId(vibe_talk::testing::READ_CHANNEL.to_owned());
    let words = "a-sentence-only-the-channel-should-ever-show";
    discord.seed(&channel, "Grace Hopper", words);
    discord.seed(&channel, "Ada Lovelace", "short");
    let router = vibe_talk::http::router(state);
    let (status, _) = get(&router, &format!("/api/v1/channels/{}/messages", channel.0)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, report) = get(&router, "/api/v1/diagnostics").await;
    assert_eq!(status, StatusCode::OK);
    let upstream = &report["upstream"];
    assert!(upstream["since"]
        .as_str()
        .is_some_and(|since| since.ends_with('Z')));
    let providers = upstream["providers"].as_array().expect("providers");
    assert_eq!(providers.len(), 1, "{upstream:#}");
    let discord_profile = &providers[0];
    assert_eq!(discord_profile["provider"], "discord", "{upstream:#}");
    assert_eq!(discord_profile["name"], "Discord");
    let operations = discord_profile["operations"]
        .as_array()
        .expect("operations");
    let ok_reads: u64 = operations
        .iter()
        .filter(|row| row["outcome"] == "ok" && row["rows"].as_u64().unwrap_or(0) > 0)
        .map(|row| row["count"].as_u64().unwrap_or(0))
        .sum();
    assert!(ok_reads >= 1, "the read was not timed: {upstream:#}");
    assert!(
        operations.iter().any(|row| row["name"] == "identity"),
        "the report's own token check is counted too: {upstream:#}"
    );
    for row in operations {
        for field in ["count", "mean_ms", "p50_ms", "p90_ms", "p99_ms", "max_ms"] {
            assert!(row[field].is_number(), "{field} in {row}");
        }
        assert!(row["octaves_ms"].is_array(), "{row}");
    }
    assert_eq!(discord_profile["shim_calls"], Value::Array(Vec::new()));
    let sizes = &discord_profile["content_bytes"];
    assert!(
        sizes["messages"].as_u64().is_some_and(|n| n >= 2),
        "{upstream:#}"
    );
    assert!(
        sizes["max"]
            .as_u64()
            .is_some_and(|max| max >= words.len() as u64),
        "{upstream:#}"
    );

    let text = upstream.to_string();
    for private in [
        words,
        "short",
        "Grace Hopper",
        "Ada Lovelace",
        channel.0.as_str(),
        vibe_talk::testing::WRITE_CHANNEL,
    ] {
        assert!(
            !text.contains(private),
            "the upstream section carries {private:?}: {text}"
        );
    }
}

const TEAM: &str = "C0000MAIN1";

/// A Slack deployment built from configuration exactly as `main` builds one.
async fn slack_deployment(fake: &Fake) -> vibe_talk::state::AppState {
    let text = format!(
        r#"
[auth]
read_token = "{read}"
write_token = "{write}"

[[providers]]
key = "slack"
kind = "slack"
api_base = "{base}"
token = "{TOKEN}"

[[channels]]
id = "{TEAM}"
label = "team"
writable = true
"#,
        read = vibe_talk::testing::READ_TOKEN,
        write = vibe_talk::testing::WRITE_TOKEN,
        base = fake.base,
    );
    let (mut state, _discord, _store) = vibe_talk::testing::state_with_store_from_toml(&text);
    let router = ChatRouter::from_config(&state.config).expect("providers build");
    state.replace_providers(Arc::new(router));
    state
}

#[tokio::test]
async fn slack_web_api_calls_are_counted_as_the_far_end_counts_them() {
    let mut fixture = FakeState::default();
    fixture.history.insert(
        TEAM.to_owned(),
        vec![
            profiled(&ts(20, 0), "UADA00001", "Ada", "second message"),
            profiled(&ts(10, 0), "UGRACE001", "Grace", "first"),
        ],
    );
    let fake = Fake::start(fixture, 20).await;
    let state = slack_deployment(&fake).await;
    let router = vibe_talk::http::router(state.clone());
    let (status, body) = get(&router, &format!("/api/v1/channels/{TEAM}/messages")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let count = |report: &vibe_talk::upstream_profile::UpstreamReport,
                 shim: bool,
                 name: &str,
                 outcome: Outcome| {
        let provider = &report.providers[0];
        let rows = if shim {
            &provider.shim_calls
        } else {
            &provider.operations
        };
        rows.iter()
            .find(|row| row.name == name && row.outcome == outcome)
            .map_or(0, |row| row.count)
    };
    let before = state.providers.profile().report();
    // The page's read of a channel's newest messages asks for them dated, whether or not the
    // provider dates them.
    assert!(
        count(&before, false, "fetch_recent_dated", Outcome::Ok) >= 1,
        "{before:#?}"
    );

    // One 429, waited out: two requests at the far end, two calls here, one operation.
    fake.with(|state| state.rate_limit_once = Some(("conversations.history".to_owned(), 0)));
    let read = state
        .chat
        .fetch_recent(&ChannelId(TEAM.to_owned()), 10)
        .await
        .expect("read after a rate limit");
    assert_eq!(read.len(), 2);
    let _ = state.chat.identity().await.expect("auth.test");

    let report = state.providers.profile().report();
    assert_eq!(report.providers.len(), 1);
    let slack = &report.providers[0];
    assert_eq!(
        (slack.provider.as_str(), slack.name.as_str()),
        ("slack", "Slack")
    );
    for method in METHODS {
        let counted: u64 = slack
            .shim_calls
            .iter()
            .filter(|row| row.name == method)
            .map(|row| row.count)
            .sum();
        assert_eq!(
            counted,
            fake.calls(method) as u64,
            "{method}: the far end saw {} requests and the profile counted {counted}: {:#?}",
            fake.calls(method),
            slack.shim_calls
        );
    }
    let history = |outcome: Outcome| {
        slack
            .shim_calls
            .iter()
            .find(|row| row.name == "conversations.history" && row.outcome == outcome)
            .map_or(0, |row| row.count)
    };
    assert_eq!(history(Outcome::RateLimited), 1, "{:#?}", slack.shim_calls);
    assert!(history(Outcome::Ok) >= 2, "{:#?}", slack.shim_calls);
    assert!(
        slack.shim_calls.iter().all(|row| row.rows.is_none()),
        "a call has no rows"
    );
    assert_eq!(
        count(&report, false, "fetch_recent", Outcome::Ok),
        count(&before, false, "fetch_recent", Outcome::Ok) + 1,
        "one operation, however many calls it took"
    );
    assert_eq!(
        count(&report, true, "conversations.history", Outcome::Ok)
            + count(&report, true, "conversations.history", Outcome::RateLimited),
        count(&before, true, "conversations.history", Outcome::Ok) + 2,
        "{:#?}",
        slack.shim_calls
    );
    assert!(
        slack.content_bytes.messages >= 4,
        "{:?}",
        slack.content_bytes
    );

    // A refusal from Slack is a call with an outcome, and the operation fails with the same one.
    fake.with(|state| {
        state.fail.insert(
            "conversations.history".to_owned(),
            "channel_not_found".to_owned(),
        )
    });
    let _ = state
        .chat
        .fetch_recent(&ChannelId(TEAM.to_owned()), 10)
        .await
        .expect_err("refused");
    let report = state.providers.profile().report();
    let slack = &report.providers[0];
    assert!(slack
        .shim_calls
        .iter()
        .any(|row| row.name == "conversations.history" && row.outcome == Outcome::NotFound));
    assert!(slack
        .operations
        .iter()
        .any(|row| row.name == "fetch_recent" && row.outcome == Outcome::NotFound));
}
