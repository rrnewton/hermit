//! Slack implementation of the provider-neutral chat interface.
//!
//! [`HttpSlackClient`] reads and posts through a fixed subset of the Slack Web API, so a
//! deployment can point it at Slack itself or at a compatible bridge that implements exactly that
//! subset:
//!
//! | Method | Used for |
//! |---|---|
//! | `auth.test` | [`crate::chat::ChatClient::identity`], and the account this client posts as |
//! | `conversations.info` | verifying a conversation before it is registered |
//! | `conversations.history` | a conversation's main timeline, the thread list, reply targets |
//! | `conversations.replies` | a thread's messages, thread membership checks, exact lookups |
//! | `chat.postMessage` | every post, in a conversation or in a thread |
//! | `users.info` | an author's display name when the message does not carry one |
//! | `users.conversations` | the channel browser |
//! | `conversations.mark` | clearing unread in Slack, only with `upstream_clear` on (`slack/clear.rs`) |
//!
//! Every call is a form-encoded `POST {api_base}/{method}` with `Authorization: Bearer <token>`.
//! With `upstream_clear` on, `conversations.info` is also read for the account's cursor,
//! `last_read`, before every mark.
//!
//! # Identity mapping
//!
//! The rest of the server is snowflake-ordered, so a Slack `ts` is encoded as a snowflake-shaped
//! [`crate::model::MessageId`] that orders, pages and dates exactly as the `ts` does; see
//! [`ids`]. A whole conversation's channel id is its Slack id, and a channel narrowed to one
//! thread is `"{conversation}~{thread_ts}"`. Thread ids in the thread APIs are root `ts` values.
//!
//! # What it does not do
//!
//! There is no Events API or Socket Mode connection: new messages arrive through the server's
//! ordinary bounded live polling, which reads a conversation's main timeline (thread replies are
//! read when a thread or a thread-scoped channel is opened).
//!
//! There is no manual "Mark read in Slack" on a message. The one upstream write besides posting is
//! the automatic clear of [`crate::upstream_clear`], off unless the deployment sets
//! `upstream_clear`: it moves the conversation's read cursor forward, never back, and cannot mark
//! a thread read, because Slack offers no way to with this kind of token. Replies inside threads
//! therefore stay unread in Slack, and a channel narrowed to one thread is refused.

mod clear;
mod client;
mod history;
pub mod ids;
pub mod mrkdwn;
mod timeline;

pub use client::{HttpSlackClient, SLACK_MAX_TEXT_CHARS};
pub use ids::{message_id_from_ts, parse_source, ts_from_message_id};

/// The Slack Web API base used when a deployment does not name a bridge.
pub const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";

/// The most characters one post to Slack itself is cut to, unless configured otherwise.
/// `#263 post-split-per-provider`.
///
/// Slack keeps [`SLACK_MAX_TEXT_CHARS`] and truncates past it. A thousand short of that, because
/// Slack does not say how it counts a character, and an emoji is one character here and may be two
/// there.
pub const SLACK_DEFAULT_POST_CHARS: usize = 39_000;

/// Whether `api_base` is Slack's own Web API rather than a bridge in front of it.
#[must_use]
pub fn is_official_slack_api_base(api_base: &str) -> bool {
    reqwest::Url::parse(api_base.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "slack.com" || host.ends_with(".slack.com"))
}

/// Slack access parameters for one workspace connection.
#[derive(Debug, Clone)]
pub struct SlackConfig {
    /// Display name of the chat service, normally `Slack`.
    pub provider_name: String,
    /// Web API base, such as [`DEFAULT_SLACK_API_BASE`]. A trailing slash is tolerated.
    pub api_base: String,
    /// Sent as `Authorization: Bearer <token>`: an `xoxb-` bot token, an `xoxp-` user token, or
    /// the token of a compatible bridge.
    pub token: crate::config::Secret,
    /// The owner's own Slack user id (`U…` or `W…`), when configured.
    pub owner_user_id: Option<String>,
    /// Seconds one HTTP request may take.
    pub request_timeout_seconds: u64,
    /// Whether the app's "Add channel" accepts Slack links and conversation ids as sources.
    pub channel_registration: bool,
    /// The `writable` policy reported for channels registered through the app.
    pub registered_channels_writable: bool,
    /// Whether the channel browser lists the conversations this account belongs to.
    pub channel_discovery: bool,
    /// Whether the server may clear a conversation's unread state in Slack's own client, moving
    /// the token's read cursor with `conversations.mark` (`slack/clear.rs`). Off by default.
    pub upstream_clear: bool,
    /// The most characters one post may hold, as Slack is sent it, when configured.
    /// `#263 post-split-per-provider`; see [`SlackConfig::post_limit`].
    pub max_post_chars: Option<usize>,
    /// The most UTF-8 bytes one post may hold, as Slack is sent it, when configured.
    pub max_post_bytes: Option<usize>,
}

impl SlackConfig {
    /// The most text one post through this connection may carry, counted as Slack is sent it:
    /// with `&`, `<` and `>` escaped ([`mrkdwn::escape_outgoing`]). `#263
    /// post-split-per-provider`.
    ///
    /// Slack itself takes [`SLACK_DEFAULT_POST_CHARS`] unless configured, and never more than
    /// [`SLACK_MAX_TEXT_CHARS`]. A bridge takes what the operator said it accepts
    /// (`max_post_chars`, `max_post_bytes`), and when the operator said nothing, 2000 characters:
    /// the most any bridge has been sent since posts were first split, so an upgrade never sends a
    /// bridge a longer post than it has accepted before.
    #[must_use]
    pub fn post_limit(&self) -> crate::chat::PostLimit {
        let official = is_official_slack_api_base(&self.api_base);
        let stated = self.max_post_chars.is_some() || self.max_post_bytes.is_some();
        let max_chars = match self.max_post_chars {
            Some(max) if official => Some(max.min(SLACK_MAX_TEXT_CHARS)),
            Some(max) => Some(max),
            None if official => Some(SLACK_DEFAULT_POST_CHARS),
            None if !stated => Some(crate::discord::http::DISCORD_MAX_CONTENT_LEN),
            None => None,
        };
        crate::chat::PostLimit {
            max_chars,
            max_bytes: self.max_post_bytes,
            escaping: crate::chat::PostEscaping::HtmlEntities,
        }
    }
}
