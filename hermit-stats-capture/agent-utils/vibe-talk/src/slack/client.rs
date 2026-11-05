//! The Slack Web API client: transport, error mapping, author names, and message mapping.
//!
//! Reads, timelines and posts are built on top of this in [`super::history`] and
//! [`super::timeline`]; this file owns the one function that talks to the network, so the token,
//! the rate-limit budget and the translation of Slack's `{ok:false}` answers live in one place.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt as _;
use serde_json::Value;

use super::ids::{self, ChannelRef};
use super::SlackConfig;
use crate::chat::{
    ChatClient, ChatError, ChatIdentity, RateLimitExhausted, RegisteredChannel, SourceClaim,
};
use crate::config::Secret;
use crate::model::{ChannelId, Message, MessageId, UserId};
use crate::threads::MessageThread;
use crate::upstream_profile::{self, ProviderProfile, Series};

/// Slack's own ceiling on `chat.postMessage` text: "Slack will truncate messages containing more
/// than 40,000 characters", <https://docs.slack.dev/reference/methods/chat.postMessage>, read
/// 2026-10-12. Longer text would be cut short upstream, so it is split or refused here instead.
pub const SLACK_MAX_TEXT_CHARS: usize = 40_000;
/// The longest one request may spend waiting out rate limits, across all its attempts.
pub const RATE_LIMIT_BUDGET: Duration = Duration::from_secs(30);
/// How many times one logical request may be sent before a rate limit is declared unclearable.
const MAX_RATE_LIMIT_ATTEMPTS: u32 = 5;
/// The wait assumed when a rate-limited answer carries no readable `Retry-After`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);
/// The floor on one retry wait, so "retry now" is never spent in the same millisecond.
const MIN_RETRY_WAIT: Duration = Duration::from_millis(50);
/// The largest `Retry-After` believed, in seconds; anything larger blows the budget anyway.
const MAX_PARSED_RETRY_AFTER_SECONDS: f64 = 3600.0;
/// Most authors whose `users.info` answer is remembered.
const USER_CACHE_MAX: usize = 2000;
/// How long a resolved author name is trusted.
const USER_CACHE_TTL: Duration = Duration::from_secs(3600);
/// How long a failed lookup is remembered, so a missing scope does not cost a call per message.
const USER_FAILURE_TTL: Duration = Duration::from_secs(300);
/// Most conversations remembered as having threads.
const THREADED_MAX: usize = 1000;

/// Message subtypes that are somebody's words. Every other subtype is a system event (a join, a
/// topic change, a rename, …) and is omitted from reads.
const KEPT_SUBTYPES: [&str; 4] = [
    "bot_message",
    "thread_broadcast",
    "file_share",
    "me_message",
];

/// Slack `error` strings that mean the credential itself was refused.
const UNAUTHORIZED: [&str; 5] = [
    "not_authed",
    "invalid_auth",
    "account_inactive",
    "token_revoked",
    "token_expired",
];
/// Slack `error` strings that mean the credential is good but may not do this.
const FORBIDDEN: [&str; 8] = [
    "missing_scope",
    "not_in_channel",
    "access_denied",
    "restricted_action",
    "is_archived",
    "no_permission",
    "not_allowed_token_type",
    "team_access_not_granted",
];
/// Slack `error` strings that mean the named thing does not exist for this credential.
const NOT_FOUND: [&str; 4] = [
    "channel_not_found",
    "thread_not_found",
    "message_not_found",
    "user_not_found",
];

/// Whether a request may wait out a rate limit or should give up at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Patience {
    /// Wait within [`RATE_LIMIT_BUDGET`]: the caller needs this answer.
    Budgeted,
    /// Give up at the first rate limit: the answer is cosmetic, such as an author's name.
    BestEffort,
}

/// One answer from the transport.
enum Outcome {
    Done(Value),
    Limited(Duration),
}

/// What is remembered about one author.
#[derive(Clone, Debug)]
struct CachedUser {
    name: Option<String>,
    is_bot: bool,
    at: Instant,
}

/// A live Slack client.
#[derive(Debug)]
pub struct HttpSlackClient {
    pub(super) provider_name: String,
    api_base: String,
    token: Secret,
    owner_user_id: Option<String>,
    client: reqwest::Client,
    pub(super) channel_registration: bool,
    pub(super) registered_writable: bool,
    pub(super) channel_discovery: bool,
    /// Whether the server may move this account's Slack read cursor; see `super::clear`.
    pub(super) upstream_clear: bool,
    /// The account this client posts as, learned from `auth.test` or from its first post.
    self_user_id: Mutex<Option<String>>,
    users: Mutex<HashMap<String, CachedUser>>,
    /// Conversations in which a thread root has been seen, so `has_threads` does not flicker
    /// between pages that happen to contain none.
    threaded: Mutex<HashSet<String>>,
    /// Stretches after a cursor proven to hold no real messages, so a walk that ran out of
    /// requests while crossing system events resumes rather than restarts. See `super::history`.
    pub(super) proven: Mutex<HashMap<(String, i64), (i64, Instant)>>,
    /// The replies each thread's last Flat read brought, by conversation and root `ts`, and what
    /// the root said then, so the next Flat read reads only the threads that moved; and when a
    /// read of each thread, Flat or the thread view, last finished. `#282 slack-flat-skip`; see
    /// `super::timeline`.
    pub(super) flat_memory: Mutex<super::timeline::FlatMemory>,
    /// How long those replies are trusted before a Flat read reads their thread in full again.
    pub(super) flat_reconcile: Duration,
    /// The most one post may carry, as Slack is sent it; see [`SlackConfig::post_limit`].
    pub(super) post_limit: crate::chat::PostLimit,
    /// Where each Web API request is timed, by method and outcome, when the client was built with
    /// one. `#208 persistent-cache`; see [`crate::upstream_profile`].
    shim_profile: Option<Arc<ProviderProfile>>,
}

impl HttpSlackClient {
    /// Build a client from one Slack provider's configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ChatError::Transport`] when the HTTP client cannot be built, and
    /// [`ChatError::Refused`] when the API base is not an `http(s)` URL.
    pub fn new(config: &SlackConfig) -> Result<Self, ChatError> {
        let api_base = config.api_base.trim().trim_end_matches('/').to_owned();
        let parsed = reqwest::Url::parse(&api_base).map_err(|_| {
            ChatError::Refused("the Slack API base is not a URL".to_owned())
                .with_provider(&config.provider_name)
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(
                ChatError::Refused("the Slack API base must be an http(s) URL".to_owned())
                    .with_provider(&config.provider_name),
            );
        }
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "vibe-talk (https://github.com/rrnewton/agent-utils, ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .timeout(Duration::from_secs(config.request_timeout_seconds.max(1)))
            .build()
            .map_err(|e| {
                ChatError::Transport(e.to_string()).with_provider(&config.provider_name)
            })?;
        Ok(Self {
            provider_name: config.provider_name.clone(),
            api_base,
            token: config.token.clone(),
            owner_user_id: config
                .owner_user_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            client,
            channel_registration: config.channel_registration,
            registered_writable: config.registered_channels_writable,
            channel_discovery: config.channel_discovery,
            upstream_clear: config.upstream_clear,
            self_user_id: Mutex::new(None),
            users: Mutex::new(HashMap::new()),
            threaded: Mutex::new(HashSet::new()),
            proven: Mutex::new(HashMap::new()),
            flat_memory: Mutex::default(),
            flat_reconcile: super::timeline::FLAT_RECONCILE,
            post_limit: config.post_limit(),
            shim_profile: None,
        })
    }

    /// Time every Web API request this client sends into `profile`, by method and outcome.
    /// `#208 persistent-cache`.
    ///
    /// Each request counts once, including one answered with a rate limit and sent again, which
    /// is how the far end's own log counts them. A request the caller stopped waiting for counts
    /// as abandoned.
    #[must_use]
    pub fn with_shim_profile(mut self, profile: Arc<ProviderProfile>) -> Self {
        self.shim_profile = Some(profile);
        self
    }

    /// How long the replies a Flat read remembers for a thread are trusted before a Flat read
    /// reads that thread in full again: fifteen minutes unless set, the page's own reconcile
    /// period. `#282 slack-flat-skip`.
    ///
    /// Until then a thread whose root reports the same reply count and latest reply costs no call,
    /// and one with new replies costs one. An edit to a reply changes nothing on its root, so it
    /// reaches Flat when this period runs out, or on the next Flat read after the thread view has
    /// shown it (the thread view shows it at once). That holds for every reader of Flat, not only
    /// the page: a summary, or a recent window where `recent_view` is `"flat"`, may also see a
    /// reply's text as it was up to this long ago. `Duration::ZERO` reads every thread in full on
    /// every Flat read.
    #[must_use]
    pub fn with_flat_reconcile(mut self, period: Duration) -> Self {
        self.flat_reconcile = period;
        self
    }

    /// Call one Web API method, waiting out rate limits within [`RATE_LIMIT_BUDGET`].
    pub(super) async fn call(
        &self,
        method: &'static str,
        params: &[(&str, String)],
    ) -> Result<Value, ChatError> {
        self.call_with(method, params, Patience::Budgeted).await
    }

    pub(super) async fn call_with(
        &self,
        method: &'static str,
        params: &[(&str, String)],
        patience: Patience,
    ) -> Result<Value, ChatError> {
        let url = format!("{}/{method}", self.api_base);
        let mut waited = Duration::ZERO;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let timer = self
                .shim_profile
                .as_deref()
                .map(|profile| profile.start(Series::ShimCall, method));
            let answer = self.attempt(&url, method, params).await;
            if let Some(timer) = timer {
                timer.finish(
                    match &answer {
                        Ok(Outcome::Done(_)) => upstream_profile::Outcome::Ok,
                        Ok(Outcome::Limited(_)) => upstream_profile::Outcome::RateLimited,
                        Err(error) => upstream_profile::Outcome::of(error),
                    },
                    0,
                );
            }
            match answer? {
                Outcome::Done(value) => return Ok(value),
                Outcome::Limited(retry_after) => {
                    let wait = retry_after.max(MIN_RETRY_WAIT);
                    // A wait longer than what is left is refused BEFORE it is taken: parking a
                    // caller and then failing anyway is the worst of both.
                    if patience == Patience::BestEffort
                        || attempts >= MAX_RATE_LIMIT_ATTEMPTS
                        || waited + wait > RATE_LIMIT_BUDGET
                    {
                        return Err(ChatError::RateLimited(RateLimitExhausted {
                            provider: "slack",
                            route: method.to_owned(),
                            attempts,
                            waited,
                            budget: RATE_LIMIT_BUDGET,
                            retry_after,
                            global: false,
                        }));
                    }
                    tokio::time::sleep(wait).await;
                    waited += wait;
                }
            }
        }
    }

    async fn attempt(
        &self,
        url: &str,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<Outcome, ChatError> {
        let response = self
            .client
            .post(url)
            .bearer_auth(self.token.expose())
            .form(params)
            .send()
            .await
            .map_err(|e| ChatError::Transport(self.redact(&e.to_string())))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(|seconds| Duration::from_secs_f64(seconds.min(MAX_PARSED_RETRY_AFTER_SECONDS)));
        let text = response
            .text()
            .await
            .map_err(|e| ChatError::Transport(self.redact(&e.to_string())))?;
        if status.as_u16() == 429 {
            return Ok(Outcome::Limited(retry_after.unwrap_or(DEFAULT_RETRY_AFTER)));
        }
        if !status.is_success() {
            // Truncated: an intermediary's error page is long, and this string ends up in a log.
            return Err(ChatError::Status {
                status: status.as_u16(),
                body: self.redact(&text).chars().take(500).collect(),
            });
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            ChatError::Shape(format!("{method} answered with something that is not JSON"))
        })?;
        match value.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(Outcome::Done(value)),
            Some(false) => {
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_error");
                if error == "ratelimited" {
                    return Ok(Outcome::Limited(retry_after.unwrap_or(DEFAULT_RETRY_AFTER)));
                }
                Err(self.slack_error(method, error, &value))
            }
            None => Err(ChatError::Shape(format!(
                "{method} answer has no boolean \"ok\" field"
            ))),
        }
    }

    /// Translate a Slack `{ok:false, error}` into the status the probe and the API classify.
    fn slack_error(&self, method: &str, error: &str, value: &Value) -> ChatError {
        let status = if UNAUTHORIZED.contains(&error) {
            401
        } else if FORBIDDEN.contains(&error) {
            403
        } else if NOT_FOUND.contains(&error) {
            404
        } else {
            400
        };
        // Not JSON on purpose: the probe reads a numeric `code` out of a JSON body as a Discord
        // error code, and Slack's answer must be classified by its status alone.
        let mut body = format!("{method} answered {error}");
        if let Some(needed) = value.get("needed").and_then(Value::as_str) {
            body.push_str(&format!(" (needed scope: {needed})"));
        }
        ChatError::Status {
            status,
            body: self.redact(&body).chars().take(500).collect(),
        }
    }

    /// The Slack `error` code inside a failure `slack_error` made for `method`,
    /// and the scope Slack said was needed, when it said one.
    ///
    /// `None` for any other failure: a transport fault, a rate limit, or another method's answer.
    pub(super) fn slack_error_code<'e>(
        error: &'e ChatError,
        method: &str,
    ) -> Option<(&'e str, Option<&'e str>)> {
        let ChatError::Status { body, .. } = error.cause() else {
            return None;
        };
        let rest = body.strip_prefix(method)?.strip_prefix(" answered ")?;
        let end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(rest.len());
        let (code, tail) = rest.split_at(end);
        if code.is_empty() {
            return None;
        }
        let needed = tail
            .strip_prefix(" (needed scope: ")
            .and_then(|scope| scope.strip_suffix(')'))
            .filter(|scope| !scope.is_empty());
        Some((code, needed))
    }

    fn redact(&self, text: &str) -> String {
        let secret = self.token.expose();
        if secret.is_empty() {
            text.to_owned()
        } else {
            text.replace(secret, "<redacted>")
        }
    }

    /// Remember the account this client posts as.
    pub(super) fn learn_self(&self, id: &str) {
        let mut slot = self
            .self_user_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() && !id.is_empty() {
            *slot = Some(id.to_owned());
        }
    }

    /// Note that `conversation` has threads.
    pub(super) fn mark_threaded(&self, conversation: &str) {
        let mut threaded = self
            .threaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if threaded.len() >= THREADED_MAX {
            threaded.clear();
        }
        threaded.insert(conversation.to_owned());
    }

    /// Whether a thread root has been seen in `conversation`.
    pub(super) fn is_threaded(&self, conversation: &str) -> bool {
        self.threaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(conversation)
    }

    /// An author's display name and bot flag from `users.info`, cached; `None` when unknown.
    ///
    /// Never fails a read: a lookup that fails, including on a rate limit, degrades to the raw id
    /// and is remembered briefly so a missing `users:read` scope does not cost a call per message.
    pub(super) async fn user(&self, id: &str) -> Option<(String, bool)> {
        {
            let users = self
                .users
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = users.get(id) {
                let ttl = if cached.name.is_some() {
                    USER_CACHE_TTL
                } else {
                    USER_FAILURE_TTL
                };
                if cached.at.elapsed() < ttl {
                    return cached.name.clone().map(|name| (name, cached.is_bot));
                }
            }
        }
        let answer = self
            .call_with(
                "users.info",
                &[("user", id.to_owned())],
                Patience::BestEffort,
            )
            .await;
        let (name, is_bot) = match &answer {
            Ok(value) => {
                let user = value.get("user");
                let profile = user.and_then(|user| user.get("profile"));
                let text = |object: Option<&Value>, key: &str| {
                    object
                        .and_then(|object| object.get(key))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                };
                let name = text(profile, "display_name")
                    .or_else(|| text(profile, "real_name"))
                    .or_else(|| text(user, "real_name"))
                    .or_else(|| text(user, "name"));
                let is_bot = user
                    .and_then(|user| user.get("is_bot"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                (name, is_bot)
            }
            Err(error) => {
                tracing::debug!(user = id, %error, "Slack author lookup failed; showing the raw id");
                (None, false)
            }
        };
        let mut users = self
            .users
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if users.len() >= USER_CACHE_MAX {
            users.retain(|_, cached| cached.at.elapsed() < USER_CACHE_TTL);
            while users.len() >= USER_CACHE_MAX {
                let Some(oldest) = users
                    .iter()
                    .min_by_key(|(_, cached)| cached.at)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                users.remove(&oldest);
            }
        }
        users.insert(
            id.to_owned(),
            CachedUser {
                name: name.clone(),
                is_bot,
                at: Instant::now(),
            },
        );
        name.map(|name| (name, is_bot))
    }

    /// Convert raw Slack messages into [`Message`]s for `channel`, resolving author names.
    ///
    /// Order is preserved. A message with no author of any kind is omitted, with a warning.
    pub(super) async fn messages(&self, channel: &ChannelId, raws: &[Raw]) -> Vec<Message> {
        let mut wanted: Vec<&str> = raws.iter().filter_map(Raw::lookup_needed).collect();
        wanted.sort_unstable();
        wanted.dedup();
        let lookups: Vec<_> = wanted.iter().map(|id| self.user(id)).collect();
        let found: Vec<Option<(String, bool)>> = futures_util::stream::iter(lookups)
            .buffered(4)
            .collect()
            .await;
        let resolved: HashMap<String, (String, bool)> = wanted
            .iter()
            .zip(found)
            .filter_map(|(id, found)| found.map(|found| ((*id).to_owned(), found)))
            .collect();
        raws.iter()
            .filter_map(|raw| {
                let message = raw.to_message(channel, &resolved);
                if message.is_none() {
                    tracing::warn!(ts = %raw.ts, "omitting a Slack message with no author");
                }
                message
            })
            .collect()
    }

    /// Resolve a vibe-talk channel id into its Slack conversation and optional thread.
    pub(super) fn channel_ref(channel: &ChannelId) -> Result<ChannelRef, ChatError> {
        ChannelRef::parse(channel.as_str()).ok_or_else(|| {
            ChatError::Refused(format!(
                "{channel} is not a Slack channel id: expected a conversation id such as C0123ABCD, \
                 or one narrowed to a thread such as C0123ABCD~1700000000.000100"
            ))
        })
    }
}

/// One Slack message object with its parsed `ts`.
#[derive(Clone, Debug)]
pub(super) struct Raw {
    /// Microseconds since the Unix epoch.
    pub micros: i64,
    /// Canonical `ts`.
    pub ts: String,
    /// The message object as Slack sent it.
    pub value: Value,
}

impl Raw {
    /// Wrap one message object, refusing one without a usable `ts`.
    pub fn parse(value: Value) -> Result<Self, ChatError> {
        let micros = value
            .get("ts")
            .and_then(Value::as_str)
            .and_then(ids::ts_micros)
            .ok_or_else(|| ChatError::Shape("a message has no valid \"ts\"".to_owned()))?;
        Ok(Self {
            micros,
            ts: ids::micros_ts(micros),
            value,
        })
    }

    fn str_field(&self, key: &str) -> Option<&str> {
        self.value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    }

    /// Whether this is somebody's words rather than a system event, and can be given an id.
    pub fn is_real(&self) -> bool {
        self.str_field("subtype")
            .is_none_or(|subtype| KEPT_SUBTYPES.contains(&subtype))
            && ids::micros_message_id(self.micros).is_some()
    }

    /// The canonical root `ts` of the thread this message belongs to, when it belongs to one.
    pub fn thread_ts(&self) -> Option<String> {
        self.str_field("thread_ts").and_then(ids::canonical_ts)
    }

    /// Whether this message is the root of a thread with at least one reply.
    pub fn is_thread_root(&self) -> bool {
        self.thread_ts().as_deref() == Some(self.ts.as_str()) && self.reply_count() > 0
    }

    /// Replies Slack reports for this root; zero when absent.
    pub fn reply_count(&self) -> u64 {
        self.value
            .get("reply_count")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// The latest reply's instant, for a root; the message's own otherwise.
    pub fn activity_micros(&self) -> i64 {
        self.str_field("latest_reply")
            .and_then(ids::ts_micros)
            .unwrap_or(self.micros)
    }

    fn author_id(&self) -> Option<&str> {
        self.str_field("user")
            .or_else(|| self.str_field("bot_id"))
            .or_else(|| self.str_field("app_id"))
    }

    fn own_name(&self) -> Option<String> {
        let nested = |object: &str, key: &str| {
            self.value
                .get(object)
                .and_then(|object| object.get(key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        nested("user_profile", "display_name")
            .or_else(|| nested("user_profile", "real_name"))
            .or_else(|| nested("bot_profile", "name"))
            .or_else(|| {
                self.str_field("username")
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
            })
    }

    /// The user id whose name must be looked up, when the message does not carry one.
    fn lookup_needed(&self) -> Option<&str> {
        if self.own_name().is_some() {
            return None;
        }
        self.str_field("user")
    }

    /// Map onto the server's [`Message`] for `channel`.
    pub fn to_message(
        &self,
        channel: &ChannelId,
        resolved: &HashMap<String, (String, bool)>,
    ) -> Option<Message> {
        let id = ids::micros_message_id(self.micros)?;
        let author_id = self.author_id()?.to_owned();
        let looked_up = resolved.get(&author_id);
        let author = self
            .own_name()
            .or_else(|| looked_up.map(|(name, _)| name.clone()))
            .unwrap_or_else(|| author_id.clone());
        let author_is_bot = self.str_field("bot_id").is_some()
            || self.str_field("subtype") == Some("bot_message")
            || looked_up.is_some_and(|(_, is_bot)| *is_bot);
        // A message with no `thread_ts` is on the conversation's own timeline, and Slack lets any
        // such message head a thread: posting with `thread_ts` = its `ts` starts one. So it is
        // named as the root of its own thread with no replies, as a message nobody has answered on
        // Google Chat is, and the page can offer to start that thread (`#252 thread-button-state`).
        // Slack only writes `thread_ts` and `reply_count` once there is a reply, which makes the
        // count of nothing exact.
        let thread_ts = self.thread_ts().unwrap_or_else(|| self.ts.clone());
        let is_root = thread_ts == self.ts;
        let reply_count = is_root.then(|| self.reply_count());
        let thread = Some(MessageThread {
            root_message_id: ids::message_id_from_ts(&thread_ts),
            id: thread_ts,
            is_root,
            reply_count,
            reply_count_exact: reply_count.is_some(),
        });
        Some(Message {
            thread,
            id,
            channel_id: channel.clone(),
            author,
            author_id: UserId(author_id),
            author_is_bot,
            timestamp: rfc3339(self.micros),
            // Left EMPTY: this layer holds no configuration, and `crate::ops` is the one filler.
            spoken_time: String::new(),
            // Slack has no reply pointer; a reply is thread membership, carried in `thread`.
            reply_to: None,
            content: super::mrkdwn::to_markdown(self.str_field("text").unwrap_or_default()),
            spoken_content: String::new(),
            content_html: String::new(),
            noise: false,
            reactions: None,
            heading: None,
        })
    }
}

/// RFC 3339 UTC with microseconds: `2023-11-14T22:13:20.000100Z`.
pub(super) fn rfc3339(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    match jiff::Timestamp::from_second(secs) {
        Ok(at) => format!("{}.{frac:06}Z", at.strftime("%Y-%m-%dT%H:%M:%S")),
        Err(_) => ids::micros_ts(micros),
    }
}

/// Map a refusal of a channel id or cursor before any request is sent.
pub(super) fn refused(detail: &str) -> ChatError {
    ChatError::Refused(detail.to_owned())
}

/// Map a malformed answer.
pub(super) fn shape(detail: &str) -> ChatError {
    ChatError::Shape(detail.to_owned())
}

#[async_trait]
impl ChatClient for HttpSlackClient {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn claims_source(&self, source: &str) -> SourceClaim {
        // Any Slack link is ours, even one this client cannot parse: registration then explains
        // which link shapes it accepts, rather than another provider guessing at it.
        if ids::is_slack_link(source) || ids::parse_source(source).is_some() {
            SourceClaim::Certain
        } else {
            SourceClaim::Never
        }
    }

    fn self_author_id(&self) -> Option<String> {
        self.self_user_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn owner_author_id(&self) -> Option<String> {
        self.owner_user_id.clone()
    }

    fn supports_threading(&self) -> bool {
        true
    }

    /// Slack has no reply pointer: a reply is posted in the target's thread (`timeline::post`).
    fn replies_in_thread(&self) -> bool {
        true
    }

    fn post_limit(&self, _channel: &ChannelId) -> crate::chat::PostLimit {
        self.post_limit
    }

    /// The thread the first part went into, or the one it now roots. `#263
    /// post-split-per-provider`.
    ///
    /// A Slack reply is a thread reply, and any message can root a thread: its own `ts` is the
    /// thread id. So the rest of a long post goes under the first part rather than after it, and a
    /// first part that answered a message — which Slack placed in that message's thread — is
    /// followed into the same thread rather than left there alone.
    fn continuation_thread(&self, _channel: &ChannelId, first: &Message) -> Option<String> {
        first
            .thread
            .as_ref()
            .map(|thread| thread.id.clone())
            .or_else(|| ids::ts_from_message_id(&first.id))
    }

    async fn fetch_timeline(
        &self,
        channel: &ChannelId,
        request: &crate::threads::TimelineRequest,
    ) -> Result<crate::threads::TimelinePage, ChatError> {
        self.timeline(channel, request)
            .await
            .map_err(|error| error.with_provider(&self.provider_name))
    }

    async fn post_in_thread(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.thread_post(channel, thread_id, content, reply_to)
            .await
            .map_err(|error| error.with_provider(&self.provider_name))
    }

    async fn fetch_thread_message(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        message_id: &MessageId,
    ) -> Result<Message, ChatError> {
        self.thread_message(channel, thread_id, message_id)
            .await
            .map_err(|error| error.with_provider(&self.provider_name))
    }

    fn supports_channel_registration(&self) -> bool {
        self.channel_registration
    }

    async fn register_channel(
        &self,
        source: &str,
        label: &str,
    ) -> Result<RegisteredChannel, ChatError> {
        self.register(source, label)
            .await
            .map_err(|error| error.with_provider(&self.provider_name))
    }

    async fn unregister_channel(&self, _channel: &ChannelId) -> Result<(), ChatError> {
        // Registration created nothing upstream, so there is nothing to remove there.
        if self.channel_registration {
            Ok(())
        } else {
            Err(refused(
                "channel registration is disabled; ask the deployment operator to enable it for this backend",
            )
            .with_provider(&self.provider_name))
        }
    }

    fn supports_channel_discovery(&self) -> bool {
        self.channel_discovery
    }

    async fn discover_channels(
        &self,
        request: &crate::directory::DirectoryRequest,
    ) -> Result<crate::directory::DirectoryPage, ChatError> {
        self.discover(request)
            .await
            .map_err(|error| error.with_provider(&self.provider_name))
    }

    /// The owner's manual "Mark read in" a message is not offered: only the automatic clear is.
    fn supports_upstream_read_mark(&self) -> bool {
        false
    }

    /// Only when the deployment turned on `upstream_clear` for this provider. `#258
    /// slack-clear-unread`.
    fn supports_upstream_clear(&self) -> bool {
        self.upstream_clear
    }

    async fn clear_upstream_through(
        &self,
        channel: &ChannelId,
        through: &MessageId,
        dry_run: bool,
    ) -> Result<crate::chat::UpstreamClearOutcome, ChatError> {
        self.clear_through(channel, through, dry_run)
            .await
            .map_err(|error| error.with_provider(&self.provider_name))
    }

    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        async {
            let value = self.call("auth.test", &[]).await?;
            let id = value
                .get("user_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| shape("auth.test answer has no string \"user_id\" field"))?;
            self.learn_self(id);
            Ok::<_, ChatError>(ChatIdentity {
                id: id.to_owned(),
                username: value
                    .get("user")
                    .and_then(Value::as_str)
                    .unwrap_or("(no username)")
                    .to_owned(),
            })
        }
        .await
        .map_err(|error| error.with_provider(&self.provider_name))
    }

    async fn fetch_page(
        &self,
        channel: &ChannelId,
        limit: u16,
        before: Option<&MessageId>,
        after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        self.read_page(channel, limit, before, after)
            .await
            .map_err(|error| error.with_provider(&self.provider_name))
    }

    async fn post_message(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.post(channel, content, reply_to)
            .await
            .map_err(|error| error.with_provider(&self.provider_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> HttpSlackClient {
        HttpSlackClient::new(&SlackConfig {
            provider_name: "Slack".to_owned(),
            api_base: "https://slack.example/api".to_owned(),
            token: Secret::new("xoxp-not-a-real-token"),
            owner_user_id: None,
            request_timeout_seconds: 5,
            channel_registration: false,
            registered_channels_writable: false,
            channel_discovery: false,
            upstream_clear: false,
            max_post_chars: None,
            max_post_bytes: None,
        })
        .expect("client")
    }

    #[test]
    fn a_slack_error_code_is_read_back_out_of_the_failure_it_became() {
        let client = client();
        let scoped = client.slack_error(
            "conversations.mark",
            "missing_scope",
            &serde_json::json!({"ok": false, "error": "missing_scope", "needed": "groups:write"}),
        );
        assert_eq!(
            HttpSlackClient::slack_error_code(&scoped, "conversations.mark"),
            Some(("missing_scope", Some("groups:write")))
        );
        let wrapped = client
            .slack_error("conversations.info", "unknown_method", &Value::Null)
            .with_provider("Slack");
        assert_eq!(
            HttpSlackClient::slack_error_code(&wrapped, "conversations.info"),
            Some(("unknown_method", None))
        );
        assert_eq!(
            HttpSlackClient::slack_error_code(&wrapped, "conversations.mark"),
            None,
            "another method's answer is not this one's"
        );
        assert_eq!(
            HttpSlackClient::slack_error_code(
                &ChatError::Transport("reset".to_owned()),
                "conversations.mark"
            ),
            None
        );
    }
}
