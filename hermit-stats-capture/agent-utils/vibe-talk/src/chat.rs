//! Provider-neutral access to the chat service behind the application.
//!
//! Provider implementations translate their wire format into [`crate::model`] types. The rest of
//! the server depends on this module, not on a particular provider. The paging cursor is still a
//! [`MessageId`] for compatibility with the existing Discord API; making cursors fully opaque is a
//! separate change because it also changes ordering and durable-state semantics.

use std::time::Duration;

use async_trait::async_trait;

use crate::model::{ChannelId, Message, MessageId};

/// Why a provider request exhausted its bounded rate-limit wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitExhausted {
    /// Stable lowercase provider name used in the diagnostic.
    pub provider: &'static str,
    /// The method and path whose bucket was exhausted.
    pub route: String,
    /// How many times the request was actually sent.
    pub attempts: u32,
    /// How long this call spent waiting before giving up.
    pub waited: Duration,
    /// The total-wait budget it was measured against.
    pub budget: Duration,
    /// What the provider last asked for, and which was not affordable.
    pub retry_after: Duration,
    /// Whether the limit was global rather than per-route.
    pub global: bool,
}

impl std::fmt::Display for RateLimitExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.format_for(self.provider))
    }
}

impl RateLimitExhausted {
    fn format_for(&self, provider: &str) -> String {
        let scope = if self.global {
            "GLOBAL, the whole bot token"
        } else {
            "this route's bucket"
        };
        let gave_up = if self.attempts == 0 {
            "gave up without sending it, because that bucket is known to be empty".to_owned()
        } else {
            format!("gave up after {} attempt(s)", self.attempts)
        };
        format!(
            "{provider} RATE LIMIT ({scope}) on {route}: {gave_up}; waited {waited:.1}s of a \
             {budget:.0}s budget and {provider} still wants {retry_after:.1}s more",
            route = self.route,
            waited = self.waited.as_secs_f64(),
            budget = self.budget.as_secs_f64(),
            retry_after = self.retry_after.as_secs_f64(),
        )
    }
}

/// A chat-provider read or post failure.
///
/// `Clone` so one failed read can be reported to every caller waiting on it: a channel's refresh
/// is shared, and so is its failure. `#210 voice-catchup-speed`.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ChatError {
    /// The request never completed.
    #[error("chat request failed: {0}")]
    Transport(String),
    /// The provider answered with a non-success status.
    ///
    /// **Not 429.** A rate limit is [`ChatError::RateLimited`] and only ever appears after the
    /// client has waited it out and failed; a bare 429 never reaches a caller.
    #[error("chat returned HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body, truncated by the caller.
        body: String,
    },
    /// The provider rate-limited the request and the wait did not clear inside the client's budget.
    ///
    #[error("{0}")]
    RateLimited(RateLimitExhausted),
    /// The response did not have the shape this server expects.
    #[error("chat response could not be understood: {0}")]
    Shape(String),
    /// The server refused the operation before contacting the provider.
    #[error("refused: {0}")]
    Refused(String),
    /// A forward cursor the backend can no longer answer for. `#203 incremental-refresh`.
    ///
    /// Issued by an earlier process, older than the changes the backend still remembers, failing
    /// its verification, or more changed than one catch-up reads. Never transient: the same cursor
    /// fails the same way again. The caller reads the newest page instead, which issues a fresh one.
    #[error("cannot continue from that position: {0}")]
    CursorExpired(String),
    /// A failure labeled by the backend that actually handled the request.
    #[error("{}", source.format_for(provider))]
    Provider {
        /// Human-readable name supplied by [`ChatClient::provider_name`].
        provider: String,
        /// Original typed failure, retained for classification and retry handling.
        #[source]
        source: Box<ChatError>,
    },
}

impl ChatError {
    /// Attach the backend's display name without losing the typed failure.
    #[must_use]
    pub fn with_provider(self, provider: &str) -> Self {
        Self::Provider {
            provider: provider.to_owned(),
            source: Box::new(self),
        }
    }

    /// The underlying failure, independent of its provider label.
    #[must_use]
    pub fn cause(&self) -> &Self {
        match self {
            Self::Provider { source, .. } => source.cause(),
            other => other,
        }
    }

    fn format_for(&self, provider: &str) -> String {
        match self.cause() {
            Self::Transport(detail) => format!("{provider} request failed: {detail}"),
            Self::Status { status, body } => format!("{provider} returned HTTP {status}: {body}"),
            Self::Shape(detail) => format!("{provider} response could not be understood: {detail}"),
            Self::Refused(detail) => format!("{provider} refused: {detail}"),
            Self::CursorExpired(detail) => {
                format!("{provider} can no longer continue from that position: {detail}")
            }
            Self::RateLimited(detail) => detail.format_for(provider),
            Self::Provider { .. } => unreachable!("cause removes provider context"),
        }
    }

    /// How long the provider still wants us to wait, when this failure is a rate limit.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self.cause() {
            Self::RateLimited(detail) => Some(detail.retry_after),
            _ => None,
        }
    }

    /// Whether the same request, sent again later, could succeed.
    ///
    /// `#195 send-resilience`. A lost connection, a rate limit, a timeout, or a provider's 5xx is a
    /// moment; a refusal — ours, or the provider's 4xx — is an answer, and sending it again asks
    /// the same question. An answer this server could not read is not transient either: it came
    /// from a provider that is up, and repeating the request repeats the answer.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self.cause() {
            Self::Transport(_) | Self::RateLimited(_) => true,
            Self::Status { status, .. } => *status >= 500 || *status == 408 || *status == 429,
            Self::Shape(_) | Self::Refused(_) | Self::CursorExpired(_) | Self::Provider { .. } => {
                false
            }
        }
    }

    /// Whether a post that failed this way may have been posted anyway.
    ///
    /// `#264 voice-send-approval`. Not the question [`ChatError::is_transient`] answers. A lost
    /// connection or a timeout may have cut off the answer after the provider took the post; a 5xx
    /// may come from a gateway in front of a provider that took it; and an answer this server
    /// could not read was a success whose body did not parse. A rate limit, a 4xx and this
    /// server's own refusal each say that the post was not taken.
    #[must_use]
    pub fn may_have_delivered(&self) -> bool {
        match self.cause() {
            Self::Transport(_) | Self::Shape(_) => true,
            Self::Status { status, .. } => *status >= 500,
            Self::RateLimited(_)
            | Self::Refused(_)
            | Self::CursorExpired(_)
            | Self::Provider { .. } => false,
        }
    }
}

/// The account a chat backend posts as, as reported by the provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatIdentity {
    /// Provider-stable account identifier.
    pub id: String,
    /// Human-readable account name.
    pub username: String,
}

/// A channel created or resolved by a provider-side registration bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredChannel {
    /// Stable channel id used by all subsequent vibe-talk routes.
    pub id: ChannelId,
    /// Whether this call created the upstream registration rather than finding an existing one.
    pub created: bool,
    /// Whether the provider bridge permits posting into this channel.
    ///
    /// Registration responses that omit this field remain read-only for backward compatibility.
    pub writable: bool,
}

/// How strongly a provider recognises an operator-supplied channel reference.
///
/// A deployment with several providers has to decide which one a pasted link belongs to before
/// any of them is asked to register it. Only the provider can recognise its own link shapes and id
/// syntax, so the decision is asked of each provider rather than guessed from the text centrally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceClaim {
    /// The reference is unmistakably this provider's: a link to its host, or its own id syntax.
    Certain,
    /// The provider cannot tell. A bridge that resolves references upstream answers this.
    Possible,
    /// The reference cannot be this provider's.
    Never,
}

/// What one automatic upstream clear did, or in a dry run would have done. `#232 upstream-mark-read`.
///
/// The provider's own answer, carried through unchanged: the server logs it and shows it in
/// Settings, and never infers a cursor the provider did not report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamClearOutcome {
    /// What the cursor covers at the provider, such as `space`.
    pub scope: String,
    /// The instant the cursor was moved to (or would have been, in a dry run), as the provider
    /// wrote it.
    pub target: String,
    /// The provider's cursor before this call, when it could be read. `None` means it had none.
    pub previous: Option<String>,
    /// Whether the cursor moved, or in a dry run whether it WOULD have: false when it was already
    /// at or past `target`. A dry run never writes, whatever this says.
    pub advanced: bool,
    /// Whether this was a dry run, so nothing was written at the provider.
    pub dry_run: bool,
}

/// How much text one post may carry at a provider, counted the way that provider counts it.
/// `#263 post-split-per-provider`.
///
/// A post longer than this is cut into parts that each fit ([`crate::discord::split`]); one that
/// fits is sent whole. Until this existed every post was cut at Discord's 2000 characters whatever
/// the provider, so a long message to a service that takes sixteen times that arrived as several.
///
/// Both measures are of the text **as the provider receives it**, after [`PostEscaping`]: a
/// provider that turns `<` into `&lt;` counts four characters where the writer typed one, and a
/// part cut to fit the writer's count could still be refused or truncated upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostLimit {
    /// Most characters (Unicode scalar values) in one post; `None` counts no characters.
    pub max_chars: Option<usize>,
    /// Most UTF-8 bytes in one post; `None` counts no bytes.
    pub max_bytes: Option<usize>,
    /// What the provider's client does to the text before sending it.
    pub escaping: PostEscaping,
}

/// What a provider's client does to a post's text on its way out, as far as its length goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PostEscaping {
    /// Sent exactly as written.
    #[default]
    Verbatim,
    /// `&`, `<` and `>` are sent as `&amp;`, `&lt;` and `&gt;`. Slack's client does this to every
    /// one of them but a user mention; counting a mention as escaped too only ever cuts a part
    /// shorter than it had to be, never longer.
    HtmlEntities,
}

impl PostLimit {
    /// At most `max` characters, sent verbatim.
    #[must_use]
    pub const fn chars(max: usize) -> Self {
        Self {
            max_chars: Some(max),
            max_bytes: None,
            escaping: PostEscaping::Verbatim,
        }
    }

    /// At most `max` UTF-8 bytes, sent verbatim.
    #[must_use]
    pub const fn bytes(max: usize) -> Self {
        Self {
            max_chars: None,
            max_bytes: Some(max),
            escaping: PostEscaping::Verbatim,
        }
    }

    /// The same limit, counted after `escaping`.
    #[must_use]
    pub const fn escaped(self, escaping: PostEscaping) -> Self {
        Self { escaping, ..self }
    }

    /// What one written character costs once escaped: `(characters, bytes)`.
    #[must_use]
    pub fn cost(&self, c: char) -> (usize, usize) {
        match (self.escaping, c) {
            (PostEscaping::HtmlEntities, '&') => (5, 5),
            (PostEscaping::HtmlEntities, '<' | '>') => (4, 4),
            _ => (1, c.len_utf8()),
        }
    }

    /// What `text` costs once escaped: `(characters, bytes)`.
    #[must_use]
    pub fn measure(&self, text: &str) -> (usize, usize) {
        text.chars().fold((0, 0), |(chars, bytes), c| {
            let (more_chars, more_bytes) = self.cost(c);
            (chars + more_chars, bytes + more_bytes)
        })
    }

    /// Whether a post of `chars` characters and `bytes` bytes, as sent, is within this limit.
    #[must_use]
    pub fn within(&self, chars: usize, bytes: usize) -> bool {
        self.max_chars.is_none_or(|max| chars <= max)
            && self.max_bytes.is_none_or(|max| bytes <= max)
    }

    /// Whether `text` may go as one post.
    #[must_use]
    pub fn admits(&self, text: &str) -> bool {
        let (chars, bytes) = self.measure(text);
        self.within(chars, bytes)
    }

    /// Why `text` may not go as one post, for a refusal; `None` when it may.
    #[must_use]
    pub fn refusal(&self, text: &str) -> Option<String> {
        let (chars, bytes) = self.measure(text);
        let escaped = if self.escaping == PostEscaping::Verbatim {
            ""
        } else {
            " once escaped"
        };
        if let Some(max) = self.max_chars.filter(|max| chars > *max) {
            return Some(format!(
                "message content is {chars} characters{escaped}; the chat API accepts at most {max}"
            ));
        }
        self.max_bytes.filter(|max| bytes > *max).map(|max| {
            format!(
                "message content is {bytes} UTF-8 bytes{escaped}; the chat API accepts at most {max}"
            )
        })
    }
}

/// A read of a channel's most recent messages, and how current the provider says it is.
/// `#208 persistent-cache`, milestone `bridge-recent-flat`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecentRead {
    /// Up to the requested number of most recent messages, oldest first.
    pub messages: Vec<Message>,
    /// The instant, RFC 3339, up to which the provider knows `messages` is complete, when it
    /// served a copy it could not bring up to date just now. `None` means now. The same field, and
    /// the same meaning, as [`crate::threads::TimelinePage::as_of`].
    pub as_of: Option<String>,
    /// Whether the provider PROVED that `messages` reach back to the channel's first message.
    ///
    /// Never inferred from a short read: a provider can drop rows it cannot represent, so fewer
    /// messages than asked for is not a proof. False by default, and for every provider today.
    pub exhausted: bool,
}

/// Read and post access to configured chat channels.
#[async_trait]
pub trait ChatClient: Send + Sync {
    /// Human-readable name of the source chat service, including when accessed through a bridge.
    fn provider_name(&self) -> &str {
        "Chat"
    }

    /// Whether `source`, an operator-supplied channel link or reference, belongs to this provider.
    ///
    /// Consulted only when several providers are configured. `Possible` by default, so a bridge
    /// that resolves arbitrary references keeps working as the fallback for anything another
    /// provider does not claim with certainty.
    fn claims_source(&self, _source: &str) -> SourceClaim {
        SourceClaim::Possible
    }

    /// The account this provider posts as, when it is known without asking the provider again.
    ///
    /// The page uses it to draw this server's own posts as the owner's. `None` means not known
    /// yet, which degrades to learning it from the first post rather than guessing.
    fn self_author_id(&self) -> Option<String> {
        None
    }

    /// The owner's own account at this provider, when the deployment has said what it is.
    fn owner_author_id(&self) -> Option<String> {
        None
    }

    /// Whether this backend supports channel and thread timeline views.
    fn supports_threading(&self) -> bool {
        false
    }

    /// Whether a reply to a message on a channel's own timeline is posted in that message's
    /// thread, starting one when it has none, because the provider has no reply pointer of its
    /// own: Slack. Such a reply and a post into the message's thread are then the same act, so
    /// the page does not offer starting a thread from Reply as a separate choice.
    /// `#252 thread-button-state`. False by default.
    fn replies_in_thread(&self) -> bool {
        false
    }

    /// Read one backward page of a channel view, with backend-owned opaque cursors.
    ///
    /// The backend must verify thread membership before any thread-specific read. Discovery that
    /// cannot be completed must fail rather than returning an apparently complete flat history.
    async fn fetch_timeline(
        &self,
        _channel: &ChannelId,
        _request: &crate::threads::TimelineRequest,
    ) -> Result<crate::threads::TimelinePage, ChatError> {
        Err(ChatError::Refused(
            "thread timelines are not supported by this backend".to_owned(),
        ))
    }

    /// What changed in a view since a generic position, for a backend whose
    /// [`ChatClient::fetch_timeline`] does not answer `after` itself. `#203 incremental-refresh`.
    ///
    /// `request` carries no cursor. The default reads newest pages and keeps what is newer than
    /// the position: correct for every backend, with `complete: false`, and no cheaper than a
    /// newest read. The router forwards it to the channel's provider, so a provider may override
    /// it with something cheaper. Answers the delta page and the position that continues it.
    ///
    /// # Errors
    ///
    /// [`ChatError::CursorExpired`] when more changed than one catch-up reads; otherwise the
    /// errors of [`ChatClient::fetch_timeline`].
    async fn fetch_timeline_since(
        &self,
        channel: &ChannelId,
        request: &crate::threads::TimelineRequest,
        since: &crate::timeline_forward::GenericPosition,
    ) -> Result<crate::timeline_forward::GenericDelta, ChatError> {
        crate::timeline_forward::generic_since(self, channel, request, since).await
    }

    /// Post inside a thread after verifying that it belongs to the configured parent channel.
    ///
    /// The caller also enforces the parent channel's write allowlist. A reply-to message does not
    /// implicitly select a thread: the destination is always explicit.
    async fn post_in_thread(
        &self,
        _channel: &ChannelId,
        _thread_id: &str,
        _content: &str,
        _reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        Err(ChatError::Refused(
            "thread posting is not supported by this backend".to_owned(),
        ))
    }

    /// Fetch an exact message after verifying its thread belongs to the configured channel.
    async fn fetch_thread_message(
        &self,
        _channel: &ChannelId,
        _thread_id: &str,
        _message_id: &MessageId,
    ) -> Result<Message, ChatError> {
        Err(ChatError::Refused(
            "thread message lookup is not supported by this backend".to_owned(),
        ))
    }

    /// Whether this provider accepts operator-supplied channel references and manages them upstream.
    ///
    /// False by default so direct providers and existing test doubles keep the established
    /// `{id, label, writable}` registration flow.
    fn supports_channel_registration(&self) -> bool {
        false
    }

    /// Register an operator-supplied channel reference and return its stable local id.
    ///
    /// # Errors
    ///
    /// Returns [`ChatError`] when registration is disabled, rejected, or fails.
    async fn register_channel(
        &self,
        _source: &str,
        _label: &str,
    ) -> Result<RegisteredChannel, ChatError> {
        Err(ChatError::Refused(
            "the configured chat provider does not support channel registration".to_owned(),
        ))
    }

    /// Remove a channel previously registered through the provider bridge.
    ///
    /// # Errors
    ///
    /// Returns [`ChatError`] when registration is disabled, rejected, or fails.
    async fn unregister_channel(&self, _channel: &ChannelId) -> Result<(), ChatError> {
        Err(ChatError::Refused(
            "the configured chat provider does not support channel registration".to_owned(),
        ))
    }

    /// Whether this provider can list the channels its account can see. `#19 channel-browser`.
    ///
    /// False by default: only a registration-capable bridge that answers the directory contract
    /// offers it, because a listed channel is only useful if it can then be registered.
    fn supports_channel_discovery(&self) -> bool {
        false
    }

    /// Read one page of the channels this provider's account can see.
    ///
    /// # Errors
    ///
    /// Returns [`ChatError`] when discovery is unsupported, denied, or fails.
    async fn discover_channels(
        &self,
        _request: &crate::directory::DirectoryRequest,
    ) -> Result<crate::directory::DirectoryPage, ChatError> {
        Err(ChatError::Refused(
            "the configured chat provider cannot list its channels".to_owned(),
        ))
    }

    /// Whether this provider can move its own read cursor forward.
    ///
    /// False by default so existing providers and test doubles do not acquire a write capability
    /// merely by upgrading vibe-talk.
    fn supports_upstream_read_mark(&self) -> bool {
        false
    }

    /// Ask the provider which account the configured credential belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`ChatError`] when the provider cannot authenticate or describe the account.
    async fn identity(&self) -> Result<ChatIdentity, ChatError>;

    /// Fetch one page of a channel, returned oldest first.
    ///
    /// `before` walks backward and `after` walks forward. They are mutually exclusive. These are
    /// message-id cursors for compatibility with Discord; the future opaque-cursor layer belongs
    /// here rather than in callers.
    ///
    /// # Errors
    ///
    /// Returns [`ChatError`] when the request fails, is rejected, or cannot be parsed.
    async fn fetch_page(
        &self,
        channel: &ChannelId,
        limit: u16,
        before: Option<&MessageId>,
        after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError>;

    /// Read a minimal sample for channel reachability checks.
    ///
    /// Backends with thread-only containers can probe their root-message view instead of calling
    /// a main-message endpoint that the provider does not support for those containers.
    async fn probe_channel(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        self.fetch_recent(channel, limit).await
    }

    /// Fetch up to `limit` most recent messages, returned oldest first.
    ///
    /// # Errors
    ///
    /// As [`ChatClient::fetch_page`].
    async fn fetch_recent(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        self.fetch_page(channel, limit, None, None).await
    }

    /// [`ChatClient::fetch_recent`], with what the provider says about how current it is.
    /// `#208 persistent-cache`.
    ///
    /// The default wraps `fetch_recent` and claims nothing: no `as_of`, and not `exhausted`. A
    /// provider that serves the newest messages from a copy it keeps overrides it to say how old
    /// that copy is. The router forwards it to the channel's own provider.
    ///
    /// # Errors
    ///
    /// As [`ChatClient::fetch_recent`].
    async fn fetch_recent_dated(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<RecentRead, ChatError> {
        Ok(RecentRead {
            messages: self.fetch_recent(channel, limit).await?,
            as_of: None,
            exhausted: false,
        })
    }

    /// Post `content` to `channel`, optionally as a reply to an existing message.
    ///
    /// # Errors
    ///
    /// Returns [`ChatError`] when the post fails or is rejected.
    async fn post_message(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError>;

    /// Whether a post carrying an idempotency key is posted at most once, however often it is sent.
    ///
    /// `#195 send-resilience`. A post whose answer is lost — a timeout, a dropped connection, a
    /// gateway's 502 — may or may not have landed, and sending it again is safe only where
    /// something upstream recognises the second request as the first. True only where the key
    /// provably reaches such a thing; the page retries an ambiguous failure on its own only then.
    ///
    /// False by default, and for every provider whose post API has no such key: the keyed methods
    /// below then ignore the key, which is honest exactly because this says so.
    fn supports_idempotent_posts(&self) -> bool {
        false
    }

    /// [`ChatClient::post_message`], carrying the idempotency key for this one part.
    ///
    /// The same key, for the same text, must name the same post on every attempt; see
    /// [`ChatClient::supports_idempotent_posts`]. The default ignores it.
    ///
    /// # Errors
    ///
    /// As [`ChatClient::post_message`].
    async fn post_message_keyed(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
        _key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.post_message(channel, content, reply_to).await
    }

    /// [`ChatClient::post_in_thread`], carrying the idempotency key for this one part.
    ///
    /// # Errors
    ///
    /// As [`ChatClient::post_in_thread`].
    async fn post_in_thread_keyed(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
        _key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.post_in_thread(channel, thread_id, content, reply_to)
            .await
    }

    /// The most text one post to `channel` may carry. `#263 post-split-per-provider`.
    ///
    /// [`crate::ops::reply_scoped`] cuts a longer post into parts that each fit, and sends one
    /// that fits whole. The default is Discord's 2000 characters
    /// ([`crate::discord::http::DISCORD_MAX_CONTENT_LEN`]): the limit every post was cut at before
    /// a provider could say otherwise, so one that does not say keeps that behaviour.
    fn post_limit(&self, _channel: &ChannelId) -> PostLimit {
        PostLimit::chars(crate::discord::http::DISCORD_MAX_CONTENT_LEN)
    }

    /// The thread the rest of a post that had to be cut continues in, once its first part,
    /// `first`, was posted to the conversation itself. `#263 post-split-per-provider`.
    ///
    /// `None`, the default, posts every part into the conversation one after another, and only the
    /// first answers the reply target. That is Discord's behaviour: this route cannot start a
    /// thread from a message it has just posted. A provider where any message can carry a thread
    /// answers the thread `first` is in or now roots, and the rest is posted there, so a long post
    /// reads as one message in the conversation with its continuation under it.
    fn continuation_thread(&self, _channel: &ChannelId, _first: &Message) -> Option<String> {
        None
    }

    /// Move the provider's read cursor through one message.
    ///
    /// Providers define how threads participate in that cursor. This is deliberately separate
    /// from vibe-talk's local read mark and its reversible Done/archive state.
    async fn mark_read_upstream(
        &self,
        _channel: &ChannelId,
        _through: &MessageId,
    ) -> Result<(), ChatError> {
        Err(ChatError::Refused(
            "the configured chat provider does not support upstream read marks".to_owned(),
        ))
    }

    /// Whether this provider can clear its own unread state automatically, as vibe-talk ingests
    /// messages. `#232 upstream-mark-read`.
    ///
    /// Separate from [`ChatClient::supports_upstream_read_mark`], the owner's own manual mark:
    /// the automatic path runs with nobody watching, so it needs a provider endpoint that yields
    /// to the owner's actions, supports a dry run, and refuses a channel where a provider-wide
    /// cursor would clear messages this server never received. False by default.
    fn supports_upstream_clear(&self) -> bool {
        false
    }

    /// Move the provider's read cursor through `through`, or in a dry run report where it would
    /// move. `#232 upstream-mark-read`.
    ///
    /// Never moves the cursor backwards: the provider compares with its current cursor first.
    /// vibe-talk's own read marks and Done state are not touched.
    ///
    /// # Errors
    ///
    /// [`ChatError::Refused`] when unsupported; otherwise the provider's failure. A provider
    /// answers HTTP 409 for a channel the cursor cannot be moved for, and HTTP 404 when it no
    /// longer holds the message.
    async fn clear_upstream_through(
        &self,
        _channel: &ChannelId,
        _through: &MessageId,
        _dry_run: bool,
    ) -> Result<UpstreamClearOutcome, ChatError> {
        Err(ChatError::Refused(
            "the configured chat provider cannot clear its unread state automatically".to_owned(),
        ))
    }
}
