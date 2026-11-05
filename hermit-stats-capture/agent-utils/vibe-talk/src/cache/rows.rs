//! The cache's own names for channels, scopes and messages, and how a [`Message`] is stored.
//!
//! # What is stored of a message, and what is not
//!
//! Stored: the provider's [`Message`] as the client normalised it, minus every field that is
//! derived when it is served. Reading a row back gives exactly the message the client produced,
//! with those fields empty: `spoken_time`, `spoken_content`, `content_html`, `heading` and
//! `noise` are never stored, so a row served from the cache goes through stamping, the noise
//! rules and rendering exactly as an upstream one does. Provider JSON as received, headers,
//! cursors and tokens are never stored either.

use serde::{Deserialize, Serialize};

use crate::model::{ChannelId, Message, MessageId, Reaction, UserId};
use crate::threads::MessageThread;

/// A channel as the cache names it: the provider it belongs to, and its id there.
///
/// The provider key is part of the name because one server can hold several providers, and two
/// providers' channel ids are different channels even when the text is the same.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChanKey {
    /// `[[providers]] key`.
    pub provider_key: String,
    /// The channel id in the application namespace, as [`Message::channel_id`] carries it.
    pub channel_id: String,
}

impl ChanKey {
    /// Name a channel.
    #[must_use]
    pub fn new(provider_key: impl Into<String>, channel_id: impl Into<String>) -> Self {
        Self {
            provider_key: provider_key.into(),
            channel_id: channel_id.into(),
        }
    }
}

/// A view of a channel that lists messages: the channel's own messages, everything flat, or one
/// thread.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// Top-level messages (and, on Slack, replies also sent to the channel).
    Main,
    /// Everything in time order, replies included: the bridge's space-level list.
    Flat,
    /// The replies of one thread, by its opaque id.
    Thread(String),
}

impl Scope {
    /// The stored `scope_key`: `main`, `flat` or `thread:<id>`.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Main => "main".to_owned(),
            Self::Flat => "flat".to_owned(),
            Self::Thread(id) => format!("thread:{id}"),
        }
    }

    /// Read a stored `scope_key`.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        match key {
            "main" => Some(Self::Main),
            "flat" => Some(Self::Flat),
            other => other
                .strip_prefix("thread:")
                .filter(|id| !id.is_empty())
                .map(|id| Self::Thread(id.to_owned())),
        }
    }

    /// The stored `home_scope` kind: `main`, `flat` or `thread`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Flat => "flat",
            Self::Thread(_) => "thread",
        }
    }
}

/// One message offered to the cache by a read or a push.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incoming {
    /// The message as the provider client normalised it. Derived fields are ignored.
    pub message: Message,
    /// The scope this message belongs to by nature: where an exhaustive listing that lacks it
    /// means it was deleted.
    pub home: Scope,
    /// The provider's edit time, when it reports one.
    pub edited_ms: Option<i64>,
}

impl Incoming {
    /// Offer `message`, whose home scope is `home`. The fields derived when a message is served
    /// (spoken forms, rendered HTML, the heading, the noise mark) are dropped here: the cache never
    /// stores them, and they would only make the queued copy larger than its byte budget counts.
    #[must_use]
    pub fn new(mut message: Message, home: Scope) -> Self {
        message.spoken_time = String::new();
        message.spoken_content = String::new();
        message.content_html = String::new();
        message.heading = None;
        message.noise = false;
        Self {
            message,
            home,
            edited_ms: None,
        }
    }
}

/// A message read back from the cache, with what the cache knows about that copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedMessage {
    /// The message, with every derived field empty: stamp, mark and render it as an upstream one.
    pub message: Message,
    /// Provider creation time, as the cache orders by it.
    pub created_ms: i64,
    /// Local time of the observation that set this content.
    pub observed_ms: i64,
    /// Whether the content came from a push that may carry only a snippet.
    pub content_partial: bool,
}

/// The JSON `extra` column: what a message carries beyond the indexed columns.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Extra {
    /// The provider's exact instant, as [`Message::timestamp`].
    pub(crate) timestamp: String,
    /// The provider's thread record for this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread: Option<MessageThread>,
    /// Reaction tallies, when the source could see them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reactions: Option<Vec<Reaction>>,
}

/// Provider creation time, derived as `ops::message_ms` derives it: the id's own time first (a
/// snowflake carries it), then the timestamp. `None` when neither says.
#[must_use]
pub fn created_ms(message: &Message) -> Option<i64> {
    message
        .id
        .created_at_ms()
        .or_else(|| crate::clock::instant_ms(&message.timestamp))
}

/// The numeric id, when the id is a number that fits: the tiebreak inside one millisecond.
pub(crate) fn numeric_id(message: &Message) -> Option<i64> {
    message.id.numeric().and_then(|n| i64::try_from(n).ok())
}

/// The content hash as the store's summaries key spells it, so a cached row can be joined to
/// its summary.
pub(crate) fn content_hash(content: &str) -> String {
    format!("{:016x}", crate::summarize::content_hash(content))
}

/// The `thread_id` column for an incoming message: set for a reply, and for a root only once a
/// listing shows it has replies. An unanswered top-level message names itself the root of its
/// own thread on several providers, and must not look like a thread to thread search.
pub(crate) fn thread_id(message: &Message) -> Option<String> {
    let thread = message.thread.as_ref()?;
    if thread.is_root && thread.reply_count.unwrap_or(0) == 0 {
        None
    } else {
        Some(thread.id.clone())
    }
}

/// Rebuild a message from its stored columns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reconstruct(
    channel_id: &str,
    message_id: &str,
    author_id: &str,
    author: &str,
    author_is_bot: bool,
    reply_to: Option<String>,
    content: String,
    extra: &str,
) -> Message {
    let extra: Extra = serde_json::from_str(extra).unwrap_or_default();
    Message {
        thread: extra.thread,
        id: MessageId(message_id.to_owned()),
        channel_id: ChannelId(channel_id.to_owned()),
        author: author.to_owned(),
        author_id: UserId(author_id.to_owned()),
        author_is_bot,
        timestamp: extra.timestamp,
        spoken_time: String::new(),
        reply_to: reply_to.map(MessageId),
        content,
        spoken_content: String::new(),
        content_html: String::new(),
        noise: false,
        reactions: extra.reactions,
        heading: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_keys_round_trip_and_a_bare_thread_prefix_is_not_a_scope() {
        for scope in [Scope::Main, Scope::Flat, Scope::Thread("17.5".to_owned())] {
            assert_eq!(Scope::parse(&scope.key()), Some(scope.clone()));
        }
        assert_eq!(Scope::parse("thread:"), None);
        assert_eq!(Scope::parse("other"), None);
        assert_eq!(Scope::Thread("x".to_owned()).kind(), "thread");
    }

    #[test]
    fn an_unanswered_root_has_no_thread_id_and_a_reply_or_an_answered_root_does() {
        let mut message = crate::cache::fixtures::message("1", "a", "x", 5);
        assert_eq!(thread_id(&message), None);
        message.thread = Some(MessageThread {
            id: "t1".to_owned(),
            root_message_id: Some(MessageId("1".to_owned())),
            is_root: true,
            reply_count: Some(0),
            reply_count_exact: true,
        });
        assert_eq!(thread_id(&message), None);
        if let Some(thread) = message.thread.as_mut() {
            thread.reply_count = Some(2);
        }
        assert_eq!(thread_id(&message).as_deref(), Some("t1"));
        if let Some(thread) = message.thread.as_mut() {
            thread.is_root = false;
            thread.reply_count = None;
        }
        assert_eq!(thread_id(&message).as_deref(), Some("t1"));
    }

    #[test]
    fn an_incoming_message_keeps_no_field_derived_when_it_is_served() {
        let mut message = crate::cache::fixtures::message("1", "a", "hello", 5);
        message.spoken_time = "09:51".to_owned();
        message.spoken_content = "hello there".to_owned();
        message.content_html = "<p>hello</p>".to_owned();
        message.noise = true;
        message.heading = Some(crate::model::Heading::default());
        let incoming = Incoming::new(message, Scope::Main);
        assert_eq!(
            incoming.message,
            crate::cache::fixtures::message("1", "a", "hello", 5)
        );
    }

    #[test]
    fn the_creation_time_comes_from_the_id_first_then_the_timestamp() {
        let message = crate::cache::fixtures::message("not-a-number", "a", "x", 1_700_000_000_000);
        assert_eq!(created_ms(&message), Some(1_700_000_000_000));
        let snowflake = Message {
            id: MessageId(((1_000_u64) << 22).to_string()),
            ..message.clone()
        };
        assert_eq!(created_ms(&snowflake), Some(1_420_070_401_000));
        let neither = Message {
            timestamp: "yesterday".to_owned(),
            ..message
        };
        assert_eq!(created_ms(&neither), None);
    }
}
