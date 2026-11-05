//! The real Discord client, over Discord's HTTP API.
//!
//! The parts with judgment in them — which URL, which headers, which body, and how a Discord
//! payload maps onto [`Message`] — are pure functions below, and they are unit-tested. The async
//! wrapper around them is deliberately thin, because it is the only part that cannot be tested
//! without a live token.
//!
//! The one piece of judgment that used to live in that thin wrapper — what to do about a **429** —
//! has been moved out into [`super::ratelimit`], which is transport-agnostic and is exercised both
//! by its own tests and by [`super::fake::FakeDiscord`]. What remains here is reading the headers
//! off a reqwest response and handing them over.

#[path = "threads.rs"]
mod threading;

use async_trait::async_trait;
use serde_json::json;

use super::ratelimit::{parse_rate_limit, Attempt, Headers, RateLimiter};
#[cfg(test)]
use crate::chat::ChatError as DiscordError;
use crate::chat::{ChatClient, ChatError, ChatIdentity, RegisteredChannel};
use crate::config::{DiscordConfig, Secret};
use crate::model::{ChannelId, Message, MessageId, Reaction, UserId};

/// Discord's own ceiling on `GET /channels/{id}/messages?limit=`.
pub const DISCORD_MAX_LIMIT: u16 = 100;
/// Discord's ceiling on a single message body, in characters: "Message contents (up to 2000
/// characters)", <https://discord.com/developers/docs/resources/message#create-message>.
pub const DISCORD_MAX_CONTENT_LEN: usize = 2000;
/// Google Chat's ceiling on one message, in bytes: "The maximum message size (including any text
/// or cards) is 32,000 bytes. To send a message that exceeds this size, your Chat app must send
/// multiple messages instead." <https://developers.google.com/workspace/chat/create-messages>,
/// read 2026-10-12. Google Chat is reached through a bridge speaking Discord's HTTP API, so the
/// limit lives beside Discord's. `#263 post-split-per-provider`.
pub const GOOGLE_CHAT_MAX_MESSAGE_BYTES: usize = 32_000;
/// Discord's ceiling on `allowed_mentions.users`.
pub const DISCORD_MAX_MENTIONED_USERS: usize = 100;

/// The user snowflakes a message body mentions, in order of first appearance, without duplicates.
///
/// This reads Discord's own mention syntax — `<@123>`, and `<@!123>` for the legacy nickname form
/// — and nothing else. A bare `@name` is not a mention to Discord and is not treated as one here.
#[must_use]
pub fn mentioned_user_ids(content: &str) -> Vec<UserId> {
    let mut found: Vec<UserId> = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<@") {
        let after = &rest[start + 2..];
        // Skip the legacy nickname marker, and refuse `<@&…>`, which is a ROLE mention: allowing
        // one through here would page a whole team.
        let digits_start = usize::from(after.starts_with('!'));
        let body = &after[digits_start..];
        let end = body
            .find('>')
            .filter(|end| *end > 0 && body[..*end].chars().all(|c| c.is_ascii_digit()));
        match end {
            Some(end) => {
                let id = UserId(body[..end].to_owned());
                if !found.contains(&id) {
                    found.push(id);
                }
                rest = &body[end + 1..];
            }
            None => rest = after,
        }
    }
    found
}

/// A request, described independently of any HTTP client.
#[derive(Debug, PartialEq, Eq)]
pub struct PreparedRequest {
    /// HTTP method.
    pub method: &'static str,
    /// Fully qualified URL.
    pub url: String,
    /// JSON body, when the method carries one.
    pub body: Option<serde_json::Value>,
}

/// Build the `Authorization` header value for a bot token.
///
/// The `Bot ` prefix is mandatory; without it Discord answers 401 for a perfectly good token.
#[must_use]
pub fn authorization_header(token: &Secret) -> String {
    format!("Bot {}", token.expose())
}

/// Build the request that reads one page of messages.
///
/// `before` and `after` are Discord's own cursors and are mutually exclusive; sending both is
/// undefined, so it is refused here rather than resolved by a coin flip.
///
/// # Errors
///
/// Returns [`DiscordError::Refused`] when both cursors are supplied.
pub fn page_request(
    api_base: &str,
    channel: &ChannelId,
    limit: u16,
    before: Option<&MessageId>,
    after: Option<&MessageId>,
) -> Result<PreparedRequest, ChatError> {
    if before.is_some() && after.is_some() {
        return Err(ChatError::Refused(
            "before and after cannot both be given: the chat API does not define what that means"
                .to_owned(),
        ));
    }
    let limit = limit.clamp(1, DISCORD_MAX_LIMIT);
    let mut url = format!(
        "{}/channels/{}/messages?limit={}",
        api_base.trim_end_matches('/'),
        channel.as_str(),
        limit
    );
    if let Some(cursor) = before {
        url.push_str(&format!("&before={}", cursor.as_str()));
    }
    if let Some(cursor) = after {
        url.push_str(&format!("&after={}", cursor.as_str()));
    }
    Ok(PreparedRequest {
        method: "GET",
        url,
        body: None,
    })
}

/// Build the request that reads the most recent messages, with no cursor.
#[must_use]
pub fn fetch_request(api_base: &str, channel: &ChannelId, limit: u16) -> PreparedRequest {
    page_request(api_base, channel, limit, None, None)
        .expect("a request with neither cursor is always buildable")
}

/// Build the request that asks Discord who this token belongs to.
///
/// Documented as `GET /users/@me`. It names no channel and needs no permission beyond a valid
/// token, which is exactly what makes it able to separate a bad credential from an unreachable
/// channel.
#[must_use]
pub fn identity_request(api_base: &str) -> PreparedRequest {
    PreparedRequest {
        method: "GET",
        url: format!("{}/users/@me", api_base.trim_end_matches('/')),
        body: None,
    }
}

/// Build the optional bridge request that registers a provider-neutral channel reference.
#[must_use]
pub fn register_channel_request(api_base: &str, source: &str, label: &str) -> PreparedRequest {
    PreparedRequest {
        method: "POST",
        url: format!("{}/channels", api_base.trim_end_matches('/')),
        body: Some(serde_json::json!({ "source": source, "label": label })),
    }
}

fn parse_registration_identity(value: &serde_json::Value) -> Result<(ChannelId, bool), ChatError> {
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| crate::directory::path_safe_id(id))
        .ok_or_else(|| {
            ChatError::Shape(
                "channel registration answer has no path-safe string \"id\" field".to_owned(),
            )
        })?;
    let created = value
        .get("created")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            ChatError::Shape(
                "channel registration answer has no boolean \"created\" field".to_owned(),
            )
        })?;
    Ok((ChannelId(id.to_owned()), created))
}

/// Read the stable channel id, creation status, and optional write policy returned by a registration
/// bridge. An omitted `writable` field remains false for compatibility with older bridges.
///
/// # Errors
///
/// Returns [`ChatError::Shape`] when a required field or the optional write policy is malformed.
pub fn parse_registered_channel(value: &serde_json::Value) -> Result<RegisteredChannel, ChatError> {
    let (id, created) = parse_registration_identity(value)?;
    let writable = match value.get("writable") {
        None => false,
        Some(value) => value.as_bool().ok_or_else(|| {
            ChatError::Shape(
                "channel registration answer has a non-boolean \"writable\" field".to_owned(),
            )
        })?,
    };
    Ok(RegisteredChannel {
        id,
        created,
        writable,
    })
}

/// Build the bridge request that reads one page of the channel directory. `#19 channel-browser`.
///
/// # Errors
///
/// Returns [`ChatError::Shape`] when the configured API base is not a URL that can take a path.
pub fn channel_directory_request(
    api_base: &str,
    request: &crate::directory::DirectoryRequest,
) -> Result<PreparedRequest, ChatError> {
    let mut url = reqwest::Url::parse(api_base.trim_end_matches('/'))
        .map_err(|_| ChatError::Shape("invalid chat API base".to_owned()))?;
    url.path_segments_mut()
        .map_err(|_| ChatError::Shape("chat API base cannot contain path segments".to_owned()))?
        .pop_if_empty()
        .push("channel-directory");
    {
        let mut query = url.query_pairs_mut();
        if let Some(text) = request.query.as_deref() {
            query.append_pair("query", text);
        }
        query.append_pair("limit", &request.limit.to_string());
        if let Some(cursor) = request.cursor.as_deref() {
            query.append_pair("cursor", cursor);
        }
    }
    Ok(PreparedRequest {
        method: "GET",
        url: url.into(),
        body: None,
    })
}

/// Build the optional bridge request that removes a registered channel.
#[must_use]
pub fn unregister_channel_request(api_base: &str, channel: &ChannelId) -> PreparedRequest {
    PreparedRequest {
        method: "DELETE",
        url: format!(
            "{}/channels/{}",
            api_base.trim_end_matches('/'),
            channel.as_str()
        ),
        body: None,
    }
}

/// Build the optional bridge request that moves its provider read cursor through one message.
#[must_use]
pub fn mark_read_request(
    api_base: &str,
    channel: &ChannelId,
    through: &MessageId,
) -> PreparedRequest {
    PreparedRequest {
        method: "POST",
        url: format!(
            "{}/channels/{}/read",
            api_base.trim_end_matches('/'),
            channel.as_str()
        ),
        body: Some(serde_json::json!({ "message_id": through.as_str() })),
    }
}

/// Build the optional bridge request that clears its provider's unread state through one message,
/// automatically, as vibe-talk ingests it. `#232 upstream-mark-read`.
///
/// A separate route from [`mark_read_request`], which is the owner's own action: this one runs
/// with nobody waiting on it, so the bridge serves it behind the owner's actions without making
/// them wait long, answers a dry run (saying whether a real run would have moved the cursor), and
/// refuses a channel where moving the provider's cursor would clear messages vibe-talk never
/// received.
#[must_use]
pub fn upstream_clear_request(
    api_base: &str,
    channel: &ChannelId,
    through: &MessageId,
    dry_run: bool,
) -> PreparedRequest {
    PreparedRequest {
        method: "POST",
        url: format!(
            "{}/channels/{}/upstream-clear",
            api_base.trim_end_matches('/'),
            channel.as_str()
        ),
        body: Some(serde_json::json!({ "message_id": through.as_str(), "dry_run": dry_run })),
    }
}

/// Read the bridge's answer to [`upstream_clear_request`].
///
/// # Errors
///
/// [`ChatError::Shape`] when `target`, `advanced` or `dry_run` is missing, or `previous` is neither
/// a string nor null. `scope` defaults to `space`. `dry_run` is required because the loop checks it
/// against what it asked: a bridge that does not say cannot be trusted with a dry run.
pub fn parse_upstream_clear(
    value: &serde_json::Value,
) -> Result<crate::chat::UpstreamClearOutcome, ChatError> {
    let shape = |what: &str| ChatError::Shape(format!("upstream clear answer {what}"));
    let target = value
        .get("target")
        .and_then(serde_json::Value::as_str)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| shape("has no target"))?;
    let advanced = value
        .get("advanced")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| shape("does not say whether the cursor advanced"))?;
    let previous = match value.get("previous") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(previous)) => Some(previous.clone()),
        Some(_) => return Err(shape("has a previous cursor that is not a string")),
    };
    Ok(crate::chat::UpstreamClearOutcome {
        scope: value
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("space")
            .to_owned(),
        target: target.to_owned(),
        previous,
        advanced,
        dry_run: value
            .get("dry_run")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| shape("does not say whether it was a dry run"))?,
    })
}

/// Read a [`BotIdentity`] out of Discord's answer to [`identity_request`].
///
/// # Errors
///
/// Returns [`DiscordError::Shape`] when the payload carries no string `id`. The username is
/// allowed to be absent — it is a label, and refusing an otherwise perfectly good identity over a
/// missing display name would turn a cosmetic difference into a failed credential check.
pub fn parse_identity(value: &serde_json::Value) -> Result<ChatIdentity, ChatError> {
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ChatError::Shape("the identity answer has no string \"id\" field".to_owned())
        })?;
    Ok(ChatIdentity {
        id: id.to_owned(),
        username: value
            .get("username")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(no username)")
            .to_owned(),
    })
}

/// Build the request that posts a message.
///
/// # Errors
///
/// Returns [`DiscordError::Refused`] when the content is empty or longer than Discord accepts, so
/// the rejection happens here rather than as an opaque 400 from Discord.
pub fn post_request(
    api_base: &str,
    channel: &ChannelId,
    content: &str,
    reply_to: Option<&MessageId>,
) -> Result<PreparedRequest, ChatError> {
    post_request_with_nonce(
        api_base,
        channel,
        content,
        reply_to,
        None,
        &crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
    )
}

/// [`post_request`], with a nonce and the provider's own post limit: Discord's 2000 characters,
/// or what a bridge was configured to accept. `#263 post-split-per-provider`.
fn post_request_with_nonce(
    api_base: &str,
    channel: &ChannelId,
    content: &str,
    reply_to: Option<&MessageId>,
    nonce: Option<&str>,
    limit: &crate::chat::PostLimit,
) -> Result<PreparedRequest, ChatError> {
    if content.trim().is_empty() {
        return Err(ChatError::Refused("message content is empty".to_owned()));
    }
    if let Some(refusal) = limit.refusal(content) {
        return Err(ChatError::Refused(refusal));
    }
    // Mentions, exactly and only the ones the text itself names.
    //
    // `parse: []` disables Discord's own scan of the body, which is what keeps a model that
    // repeated "@everyone" back from paging the whole server. But it disables user pings too, so
    // an empty `allowed_mentions` would make `<@123>` render as a mention and notify nobody —
    // silently, which is the worst of both. Listing the ids found in the body restores the ping
    // for those users while leaving @everyone, @here, and roles unreachable.
    let mut mentioned = mentioned_user_ids(content);
    // Discord's own ceiling; over it the whole request is rejected, so cut here instead.
    mentioned.truncate(DISCORD_MAX_MENTIONED_USERS);
    let mut body = json!({
        "content": content,
        "allowed_mentions": {
            "parse": [],
            "users": mentioned.iter().map(UserId::as_str).collect::<Vec<_>>(),
        },
    });
    if let Some(target) = reply_to {
        body["message_reference"] = json!({ "message_id": target.as_str() });
    }
    if let Some(nonce) = nonce {
        if nonce.is_empty()
            || nonce.len() > 128
            || !nonce.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
            })
        {
            return Err(ChatError::Refused(
                "message nonce must be 1 to 128 path-safe ASCII characters".to_owned(),
            ));
        }
        body["nonce"] = json!(nonce);
    }
    Ok(PreparedRequest {
        method: "POST",
        url: format!(
            "{}/channels/{}/messages",
            api_base.trim_end_matches('/'),
            channel.as_str()
        ),
        body: Some(body),
    })
}

/// Convert one Discord message object into this server's [`Message`].
///
/// # Errors
///
/// Returns [`DiscordError::Shape`] when a required field is absent or the wrong type.
pub fn parse_message(value: &serde_json::Value) -> Result<Message, ChatError> {
    let field = |name: &str| -> Result<String, ChatError> {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ChatError::Shape(format!("message is missing a string {name:?}")))
    };
    let author = value
        .get("author")
        .ok_or_else(|| ChatError::Shape("message is missing an author".to_owned()))?;
    let name = author
        .get("global_name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| author.get("username").and_then(serde_json::Value::as_str))
        .ok_or_else(|| ChatError::Shape("author has no username".to_owned()))?;
    let author_id = author
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ChatError::Shape("author has no id".to_owned()))?;
    Ok(Message {
        thread: value
            .get("thread")
            .filter(|thread| !thread.is_null())
            .map(|thread| {
                if thread.get("is_root").is_some() {
                    serde_json::from_value(thread.clone()).map_err(|error| {
                        ChatError::Shape(format!("invalid thread membership: {error}"))
                    })
                } else {
                    let id = thread
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| ChatError::Shape("thread object has no id".to_owned()))?;
                    Ok(crate::threads::MessageThread {
                        id: id.to_owned(),
                        root_message_id: Some(MessageId(field("id")?)),
                        is_root: true,
                        reply_count: thread
                            .get("message_count")
                            .and_then(serde_json::Value::as_u64),
                        reply_count_exact: false,
                    })
                }
            })
            .transpose()?,
        id: MessageId(field("id")?),
        channel_id: ChannelId(field("channel_id")?),
        author: name.to_owned(),
        author_id: UserId(author_id.to_owned()),
        author_is_bot: author
            .get("bot")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        timestamp: field("timestamp")?,
        // Left EMPTY on purpose. This layer holds no configuration and therefore does not know the
        // operator's zone; `crate::ops::stamp` is the single place that fills it in. See
        // `crate::model::Message::spoken_time`.
        spoken_time: String::new(),
        // `message_reference.message_id`. Discord uses this object for several kinds of reference
        // — a reply, a forward, a crosspost — and only reports the id we care about on the ones
        // that have one, so a missing or non-string value is an ordinary message rather than a
        // shape error. This is deliberately NOT read from `referenced_message`: that is the whole
        // embedded parent object, it is absent when the parent was deleted, and this server needs
        // only the id.
        reply_to: value
            .get("message_reference")
            .and_then(|reference| reference.get("message_id"))
            .and_then(serde_json::Value::as_str)
            .map(|id| MessageId(id.to_owned())),
        // A message can legitimately have empty content (an embed or an attachment only), so this
        // one is defaulted rather than required.
        content: value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        // Empty for the same reason as `spoken_time` above, and one more: how this server's voice
        // reads a message is an application decision, not something a chat adapter gets a say in.
        // See `crate::model::Message::spoken_content`.
        spoken_content: String::new(),
        content_html: String::new(),
        noise: false,
        reactions: parse_reactions(value.get("reactions")),
        heading: None,
    })
}

/// The most distinct emoji one message keeps. `#219 emoji-reactions`. Channel content sets the
/// number, so it is bounded; past it the rest are dropped, in the order the provider listed them.
pub const MAX_REACTIONS: usize = 50;

/// The longest emoji or custom name kept, in characters. A standard emoji is a handful of code
/// points even as a family or with a skin tone; a custom name is a short word.
pub const MAX_REACTION_CHARS: usize = 64;

/// The tallies of a Discord message's `reactions` array, which the Google Chat bridge speaks too.
/// `#219 emoji-reactions`.
///
/// Absent or null is `None`, and so is anything that is not an array: this copy says nothing
/// about reactions, which is not the same as saying there are none. Lenient inside the array,
/// because no reaction is worth failing a message read for: an entry without a count above zero,
/// or with neither a name nor an id, is skipped. A custom emoji is one with an `id`; it keeps its
/// name, colons trimmed, as the text to show, or `emoji` when its name is gone (Discord sends
/// null for a deleted one).
#[must_use]
pub fn parse_reactions(value: Option<&serde_json::Value>) -> Option<Vec<Reaction>> {
    fn text(value: Option<&serde_json::Value>) -> Option<&str> {
        value
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }
    let items = value?.as_array()?;
    let bounded = |text: &str| text.chars().take(MAX_REACTION_CHARS).collect::<String>();
    let reactions = items
        .iter()
        .filter_map(|item| {
            let count = item.get("count").and_then(serde_json::Value::as_u64)?;
            if count == 0 {
                return None;
            }
            let emoji = item.get("emoji");
            let name = text(emoji.and_then(|emoji| emoji.get("name")));
            let id = text(emoji.and_then(|emoji| emoji.get("id")));
            let (emoji, custom) = match (id, name) {
                (Some(_), name) => (
                    name.map(|name| name.trim_matches(':'))
                        .filter(|name| !name.is_empty())
                        .unwrap_or("emoji"),
                    true,
                ),
                (None, Some(name)) => (name, false),
                (None, None) => return None,
            };
            Some(Reaction {
                emoji: bounded(emoji),
                custom,
                custom_id: id.map(bounded),
                count: u32::try_from(count).unwrap_or(u32::MAX),
            })
        })
        .take(MAX_REACTIONS)
        .collect();
    Some(reactions)
}

/// Convert a Discord message-list payload, normalizing to oldest-first.
///
/// # Errors
///
/// Returns [`DiscordError::Shape`] when the payload is not an array of understandable messages.
pub fn parse_message_list(value: &serde_json::Value) -> Result<Vec<Message>, ChatError> {
    let items = value
        .as_array()
        .ok_or_else(|| ChatError::Shape("expected a JSON array of messages".to_owned()))?;
    let mut messages = items
        .iter()
        .map(parse_message)
        .collect::<Result<Vec<_>, _>>()?;
    crate::model::sort_oldest_first(&mut messages);
    Ok(messages)
}

/// Copy the headers this client reads out of a reqwest response.
///
/// Only the rate-limit family, and only when the value is valid ASCII — nothing else here has any
/// use for a header, and copying the whole map would put an `Authorization` echo or a cookie into
/// a struct that ends up in a log line.
fn rate_limit_headers(headers: &reqwest::header::HeaderMap) -> Headers {
    const READ: [&str; 6] = [
        "retry-after",
        "x-ratelimit-bucket",
        "x-ratelimit-global",
        "x-ratelimit-remaining",
        "x-ratelimit-reset-after",
        "x-ratelimit-scope",
    ];
    let mut out = Headers::new();
    for name in READ {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            out = out.with(name, value);
        }
    }
    out
}

fn fresh_post_nonce() -> String {
    use base64::Engine as _;

    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("the system CSPRNG must be available to mint a post nonce");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn redact_provider_text(text: &str, bot_token: &Secret) -> String {
    let secret = bot_token.expose();
    if secret.is_empty() {
        text.to_owned()
    } else {
        text.replace(secret, "<redacted>")
    }
}

/// A live Discord client.
#[derive(Debug)]
pub struct HttpDiscordClient {
    provider_name: String,
    thread_api: crate::threads::ThreadApi,
    thread_inventory: threading::InventoryCache,
    client: reqwest::Client,
    api_base: String,
    bot_token: Secret,
    request_timeout: std::time::Duration,
    channel_registration: bool,
    upstream_read_marks: bool,
    owner_user_id: Option<String>,
    /// The most one post may carry; see [`DiscordConfig::post_limit`].
    post_limit: crate::chat::PostLimit,
    /// Which bridge endpoint answers a read of the newest messages. `#208 persistent-cache`.
    recent: crate::config::RecentReads,
    /// Discord's rate limits, obeyed. Shared by every request this client makes, which is what
    /// lets one channel's 429 stop the *next* request on that channel before it is sent, and a
    /// global 429 stop all of them. See [`super::ratelimit`].
    limiter: RateLimiter,
}

impl HttpDiscordClient {
    /// Build a client from the Discord section of the configuration.
    ///
    /// # Errors
    ///
    /// Returns [`DiscordError::Transport`] when the underlying HTTP client cannot be built.
    pub fn new(config: &DiscordConfig) -> Result<Self, ChatError> {
        if config.channel_registration
            && crate::config::is_official_discord_api_base(&config.api_base)
        {
            return Err(ChatError::Refused(
                "channel registration cannot target Discord's official API".to_owned(),
            ));
        }
        if config.thread_api == crate::threads::ThreadApi::Bridge
            && crate::config::is_official_discord_api_base(&config.api_base)
        {
            return Err(ChatError::Refused(
                "bridge threading requires the API base of a compatible bridge".to_owned(),
            )
            .with_provider(&config.provider_name));
        }
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "vibe-talk (https://github.com/rrnewton/agent-utils, ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .timeout(std::time::Duration::from_secs(
                config.request_timeout_seconds,
            ))
            .build()
            .map_err(|e| {
                ChatError::Transport(e.to_string()).with_provider(&config.provider_name)
            })?;
        Ok(Self {
            provider_name: config.provider_name.clone(),
            thread_api: config.thread_api,
            thread_inventory: threading::InventoryCache::default(),
            client,
            api_base: config.api_base.clone(),
            bot_token: config.bot_token.clone(),
            request_timeout: std::time::Duration::from_secs(config.request_timeout_seconds),
            channel_registration: config.channel_registration,
            upstream_read_marks: config.upstream_read_marks,
            owner_user_id: config.owner_user_id.clone(),
            post_limit: config.post_limit(),
            recent: config.recent,
            limiter: RateLimiter::new(),
        })
    }

    /// The rate-limit state this client shares across every request.
    #[must_use]
    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    /// Send a prepared request, obeying Discord's rate limits within a bounded budget.
    ///
    /// The retry lives here rather than in the caller because the bucket state has to be shared:
    /// a per-caller retry would have every route rediscovering the same closed window one rejected
    /// request at a time.
    async fn send(&self, request: PreparedRequest) -> Result<serde_json::Value, ChatError> {
        self.send_with_timeout(request, self.request_timeout).await
    }

    async fn send_with_timeout(
        &self,
        request: PreparedRequest,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, ChatError> {
        let method = match request.method {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            "DELETE" => reqwest::Method::DELETE,
            other => return Err(ChatError::Refused(format!("unsupported method {other}"))),
        };
        let url = &request.url;
        let body = request.body.as_ref();
        self.limiter
            .run(request.method, url, || async {
                let mut builder = self
                    .client
                    .request(method.clone(), url)
                    .timeout(timeout)
                    .header(
                        reqwest::header::AUTHORIZATION,
                        authorization_header(&self.bot_token),
                    );
                if let Some(body) = body {
                    builder = builder.json(body);
                }
                let response = builder
                    .send()
                    .await
                    .map_err(|e| ChatError::Transport(e.to_string()))?;
                let status = response.status();
                let headers = rate_limit_headers(response.headers());
                let text = response
                    .text()
                    .await
                    .map_err(|e| ChatError::Transport(e.to_string()))?;
                // Before the generic failure path: a 429 is an instruction to wait, not an error
                // to report. Surfacing it as one is what used to turn a transient burst into an
                // HTTP 502 for whoever asked the question.
                if let Some(limit) = parse_rate_limit(status.as_u16(), &headers, &text) {
                    return Ok(Attempt::Limited(limit));
                }
                if !status.is_success() {
                    // Truncate: a Discord error body is short, but an intermediary's error page is
                    // not, and this string ends up in a log.
                    let body = redact_provider_text(&text, &self.bot_token)
                        .chars()
                        .take(500)
                        .collect();
                    return Err(ChatError::Status {
                        status: status.as_u16(),
                        body,
                    });
                }
                // Write-only provider operations commonly answer with 204 No Content. Represent
                // an empty successful body as JSON null: callers that need a payload will still
                // reject it in their shape parser, while actions such as an upstream read mark can
                // accept the success without inventing a response-body requirement.
                let value = if text.trim().is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_str(&text).map_err(|e| ChatError::Shape(e.to_string()))?
                };
                Ok(Attempt::Done(value, headers))
            })
            .await
    }
}

#[async_trait]
impl ChatClient for HttpDiscordClient {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn claims_source(&self, source: &str) -> crate::chat::SourceClaim {
        // A registration bridge resolves references upstream, so only it can say what it accepts.
        // Direct Discord accepts nothing but its own decimal snowflakes.
        if self.channel_registration {
            crate::chat::SourceClaim::Possible
        } else if !source.is_empty() && source.bytes().all(|byte| byte.is_ascii_digit()) {
            crate::chat::SourceClaim::Certain
        } else {
            crate::chat::SourceClaim::Never
        }
    }

    fn self_author_id(&self) -> Option<String> {
        super::self_user_id_from_token(self.bot_token.expose())
    }

    fn owner_author_id(&self) -> Option<String> {
        self.owner_user_id.clone()
    }

    fn supports_threading(&self) -> bool {
        self.thread_api != crate::threads::ThreadApi::Off
    }

    fn post_limit(&self, _channel: &ChannelId) -> crate::chat::PostLimit {
        self.post_limit
    }

    /// A bridge's own thread for the first part, as its answer reported it. `#263
    /// post-split-per-provider`. Every Google Chat message carries one, so the rest of a long
    /// post goes under the first part rather than after it.
    ///
    /// None from Discord itself, natively threaded or not: a message posted here is not a thread
    /// until one is started from it, which this route does not do, so a long post stays a run of
    /// consecutive messages there, as it always was.
    fn continuation_thread(&self, _channel: &ChannelId, first: &Message) -> Option<String> {
        if self.thread_api != crate::threads::ThreadApi::Bridge {
            return None;
        }
        first.thread.as_ref().map(|thread| thread.id.clone())
    }

    async fn fetch_timeline(
        &self,
        channel: &ChannelId,
        request: &crate::threads::TimelineRequest,
    ) -> Result<crate::threads::TimelinePage, ChatError> {
        self.timeline(channel, request)
            .await
            .map_err(|error| error.with_provider(self.provider_name()))
    }

    async fn post_in_thread(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.post_in_thread_keyed(channel, thread_id, content, reply_to, None)
            .await
    }

    /// `#195 send-resilience`. Both post routes carry the nonce the bridge contract makes its
    /// provider request id — main posts under registration, thread posts on the bridge thread
    /// API — so a key given as that nonce names one post however often it is sent. A native
    /// thread post goes to Discord itself, which honours no such field, so it is excluded.
    fn supports_idempotent_posts(&self) -> bool {
        self.channel_registration && self.thread_api != crate::threads::ThreadApi::Native
    }

    async fn post_in_thread_keyed(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
        key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.thread_post(channel, thread_id, content, reply_to, key)
            .await
            .map_err(|error| error.with_provider(self.provider_name()))
    }

    async fn fetch_thread_message(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        message_id: &MessageId,
    ) -> Result<Message, ChatError> {
        self.thread_message(channel, thread_id, message_id)
            .await
            .map_err(|error| error.with_provider(self.provider_name()))
    }

    fn supports_channel_registration(&self) -> bool {
        self.channel_registration
    }

    async fn register_channel(
        &self,
        source: &str,
        label: &str,
    ) -> Result<RegisteredChannel, ChatError> {
        async {
            if !self.channel_registration {
                return Err(ChatError::Refused(
                    "channel registration is disabled; ask the deployment operator to enable it for this backend"
                        .to_owned(),
                ));
            }
            let value = self
                .send(register_channel_request(&self.api_base, source, label))
                .await?;
            match parse_registered_channel(&value) {
                Ok(registered) => {
                    if registered.id.as_str() == self.bot_token.expose() {
                        return Err(ChatError::Shape(
                            "channel registration answer used the configured credential as its id"
                                .to_owned(),
                        ));
                    }
                    Ok(registered)
                }
                Err(error) => {
                    // A bridge can create the registration and then answer with a malformed optional
                    // policy. Once the stable id and `created` bit are trustworthy, compensate before
                    // returning the shape error so the failed local add does not leak an upstream row.
                    if let Ok((id, true)) = parse_registration_identity(&value) {
                        if id.as_str() != self.bot_token.expose() {
                            if let Err(cleanup) = self
                                .send(unregister_channel_request(&self.api_base, &id))
                                .await
                            {
                                tracing::warn!(channel = %id, %cleanup, "could not roll back malformed channel registration response");
                            }
                        }
                    }
                    Err(error)
                }
            }
        }
        .await
        .map_err(|error| error.with_provider(self.provider_name()))
    }

    fn supports_channel_discovery(&self) -> bool {
        self.channel_registration
    }

    async fn discover_channels(
        &self,
        request: &crate::directory::DirectoryRequest,
    ) -> Result<crate::directory::DirectoryPage, ChatError> {
        async {
            if !self.channel_registration {
                return Err(ChatError::Refused(
                    "channel discovery needs channel registration, which is disabled for this backend"
                        .to_owned(),
                ));
            }
            let value = self
                .send(channel_directory_request(&self.api_base, request)?)
                .await?;
            crate::directory::parse_directory_page(&value, request.limit)
        }
        .await
        .map_err(|error| error.with_provider(self.provider_name()))
    }

    async fn unregister_channel(&self, channel: &ChannelId) -> Result<(), ChatError> {
        async {
            if !self.channel_registration {
                return Err(ChatError::Refused(
                    "channel registration is disabled; ask the deployment operator to enable it for this backend"
                        .to_owned(),
                ));
            }
            let _ = self
                .send(unregister_channel_request(&self.api_base, channel))
                .await?;
            Ok(())
        }
        .await
        .map_err(|error| error.with_provider(self.provider_name()))
    }

    fn supports_upstream_read_mark(&self) -> bool {
        self.upstream_read_marks
    }

    async fn probe_channel(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        if self.thread_api == crate::threads::ThreadApi::Native {
            self.native_probe(channel, limit)
                .await
                .map_err(|error| error.with_provider(self.provider_name()))
        } else {
            self.fetch_recent(channel, limit).await
        }
    }

    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        async {
            let value = self.send(identity_request(&self.api_base)).await?;
            parse_identity(&value)
        }
        .await
        .map_err(|error| error.with_provider(self.provider_name()))
    }

    async fn fetch_page(
        &self,
        channel: &ChannelId,
        limit: u16,
        before: Option<&MessageId>,
        after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        async {
            let request = page_request(&self.api_base, channel, limit, before, after)?;
            let value = self.send(request).await?;
            parse_message_list(&value)
        }
        .await
        .map_err(|error| error.with_provider(self.provider_name()))
    }

    /// `#208 persistent-cache`. With `recent_view = "flat"` the bridge's Flat view answers;
    /// otherwise `/messages`, exactly as before.
    async fn fetch_recent(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        Ok(self.fetch_recent_dated(channel, limit).await?.messages)
    }

    async fn fetch_recent_dated(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<crate::chat::RecentRead, ChatError> {
        match self.recent.view {
            crate::config::RecentView::Messages => Ok(crate::chat::RecentRead {
                messages: self.fetch_page(channel, limit, None, None).await?,
                as_of: None,
                exhausted: false,
            }),
            crate::config::RecentView::Flat => self
                .flat_recent(channel, limit)
                .await
                .map_err(|error| error.with_provider(self.provider_name())),
        }
    }

    async fn post_message(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.post_message_keyed(channel, content, reply_to, None)
            .await
    }

    async fn post_message_keyed(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
        key: Option<&str>,
    ) -> Result<Message, ChatError> {
        async {
            self.validate_main_post(channel)?;
            // The caller's key when it has one, so the page's next attempt at the same post reuses
            // it; otherwise one minted here, kept across this call's own retries as before.
            let nonce = self
                .channel_registration
                .then(|| key.map_or_else(fresh_post_nonce, str::to_owned));
            let request = post_request_with_nonce(
                &self.api_base,
                channel,
                content,
                reply_to,
                nonce.as_deref(),
                &self.post_limit,
            )?;
            let value = self.send(request).await?;
            parse_message(&value)
        }
        .await
        .map_err(|error| error.with_provider(self.provider_name()))
    }

    async fn mark_read_upstream(
        &self,
        channel: &ChannelId,
        through: &MessageId,
    ) -> Result<(), ChatError> {
        async {
        if !self.upstream_read_marks {
            return Err(ChatError::Refused(
                "upstream read marks are disabled; ask the deployment operator to enable them for this backend"
                    .to_owned(),
            ));
        }
        let _ = self
            .send(mark_read_request(&self.api_base, channel, through))
            .await?;
        Ok(())
        }
        .await
        .map_err(|error| error.with_provider(self.provider_name()))
    }

    /// The same bridge opt-in as the manual mark, and never against Discord itself, which has no
    /// such route and no user read state for a bot. `#232 upstream-mark-read`.
    fn supports_upstream_clear(&self) -> bool {
        self.upstream_read_marks && !crate::config::is_official_discord_api_base(&self.api_base)
    }

    async fn clear_upstream_through(
        &self,
        channel: &ChannelId,
        through: &MessageId,
        dry_run: bool,
    ) -> Result<crate::chat::UpstreamClearOutcome, ChatError> {
        async {
            if !self.supports_upstream_clear() {
                return Err(ChatError::Refused(
                    "automatic upstream clearing needs a bridge with upstream read marks enabled"
                        .to_owned(),
                ));
            }
            let value = self
                .send(upstream_clear_request(
                    &self.api_base,
                    channel,
                    through,
                    dry_run,
                ))
                .await?;
            parse_upstream_clear(&value)
        }
        .await
        .map_err(|error| error.with_provider(self.provider_name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://discord.com/api/v10";

    fn channel() -> ChannelId {
        ChannelId("123".to_owned())
    }

    fn client_config(api_base: &str, channel_registration: bool) -> DiscordConfig {
        DiscordConfig {
            thread_api: crate::threads::ThreadApi::Native,
            provider_name: "Google Chat".to_owned(),
            bot_token: Secret::new("abc"),
            owner_user_id: None,
            api_base: api_base.to_owned(),
            request_timeout_seconds: 20,
            channel_registration,
            upstream_read_marks: false,
            markup: crate::render::Markup::GoogleChat,
            max_post_chars: None,
            max_post_bytes: None,
            recent: crate::config::RecentReads::default(),
        }
    }

    #[test]
    fn only_the_rate_limit_headers_are_copied_off_a_response() {
        // Two properties at once. The rate-limit family must survive, or the client cannot obey a
        // limit it was told about; and nothing else may, because this struct is what a WARN line
        // prints and a response can carry a `set-cookie` or an authorization echo.
        let mut raw = reqwest::header::HeaderMap::new();
        raw.insert("retry-after", "1".parse().expect("ascii"));
        raw.insert("X-RateLimit-Bucket", "abcd".parse().expect("ascii"));
        raw.insert("x-ratelimit-remaining", "0".parse().expect("ascii"));
        raw.insert("x-ratelimit-reset-after", "1.25".parse().expect("ascii"));
        raw.insert("x-ratelimit-scope", "user".parse().expect("ascii"));
        raw.insert("set-cookie", "session=secret".parse().expect("ascii"));
        raw.insert("authorization", "Bot abc".parse().expect("ascii"));

        let headers = rate_limit_headers(&raw);
        assert_eq!(headers.get("Retry-After"), Some("1"));
        assert_eq!(headers.get("x-ratelimit-bucket"), Some("abcd"));
        assert_eq!(
            super::super::ratelimit::exhausted_for(&headers),
            Some(std::time::Duration::from_millis(1250)),
            "the bucket accounting has to survive the copy, or nothing can be preempted"
        );
        assert_eq!(headers.get("set-cookie"), None);
        assert_eq!(
            headers.get("authorization"),
            None,
            "a header struct that ends up in a log must not carry a credential"
        );
    }

    #[test]
    fn configured_request_timeout_reaches_the_http_client() {
        let mut config = client_config("https://bridge.example/v1", true);
        config.request_timeout_seconds = 75;

        let client = HttpDiscordClient::new(&config).expect("valid client");

        assert_eq!(client.request_timeout, std::time::Duration::from_secs(75));
    }

    #[test]
    fn registration_requests_use_the_bridge_contract_exactly() {
        let request = register_channel_request(
            "https://bridge.example/v1/",
            "https://chat.example/room/thread",
            "release thread",
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "https://bridge.example/v1/channels");
        assert_eq!(
            request.body,
            Some(json!({
                "source": "https://chat.example/room/thread",
                "label": "release thread",
            }))
        );

        let removed = unregister_channel_request(
            "https://bridge.example/v1/",
            &ChannelId("managed-123".to_owned()),
        );
        assert_eq!(removed.method, "DELETE");
        assert_eq!(
            removed.url,
            "https://bridge.example/v1/channels/managed-123"
        );
        assert_eq!(removed.body, None);

        let bridged = post_request_with_nonce(
            "https://bridge.example/v1",
            &ChannelId("managed-123".to_owned()),
            "hello",
            None,
            Some("nonce_ABC-123~"),
            &crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
        )
        .expect("valid bridge post");
        assert_eq!(bridged.body.expect("body")["nonce"], "nonce_ABC-123~");
    }

    #[test]
    fn directory_requests_encode_every_opaque_value() {
        let request = channel_directory_request(
            "https://bridge.example/v1/",
            &crate::directory::DirectoryRequest {
                query: Some("release & ops/2".to_owned()),
                cursor: Some("a+b/c=".to_owned()),
                limit: 25,
            },
        )
        .expect("valid request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.url,
            "https://bridge.example/v1/channel-directory?query=release+%26+ops%2F2&limit=25&cursor=a%2Bb%2Fc%3D"
        );
        assert_eq!(request.body, None);

        let first = channel_directory_request(
            "https://bridge.example/v1",
            &crate::directory::DirectoryRequest {
                query: None,
                cursor: None,
                limit: 10,
            },
        )
        .expect("valid request");
        assert_eq!(
            first.url,
            "https://bridge.example/v1/channel-directory?limit=10"
        );
    }

    #[test]
    fn registration_answers_require_an_id_and_created_flag_and_default_to_read_only() {
        assert_eq!(
            parse_registered_channel(&json!({"id": "managed-123", "created": true}))
                .expect("legacy answer remains valid"),
            RegisteredChannel {
                id: ChannelId("managed-123".to_owned()),
                created: true,
                writable: false,
            }
        );
        assert_eq!(
            parse_registered_channel(
                &json!({"id": "managed-456", "created": false, "writable": true})
            )
            .expect("bridge may declare the channel writable"),
            RegisteredChannel {
                id: ChannelId("managed-456".to_owned()),
                created: false,
                writable: true,
            }
        );
        for invalid in [
            json!({"created": true}),
            json!({"id": "managed-123"}),
            json!({"id": "", "created": true}),
            json!({"id": "spaces/unsafe", "created": true}),
            json!({"id": "..", "created": true}),
            json!({"id": "managed-123", "created": "yes"}),
            json!({"id": "managed-123", "created": true, "writable": "yes"}),
        ] {
            assert!(matches!(
                parse_registered_channel(&invalid),
                Err(ChatError::Shape(_))
            ));
        }
    }

    #[test]
    fn http_client_refuses_managed_mode_on_official_discord() {
        let error = HttpDiscordClient::new(&client_config(BASE, true))
            .expect_err("the client constructor must preserve the official-host guard");
        assert!(matches!(error, ChatError::Refused(detail) if detail.contains("official API")));
    }

    #[test]
    fn authorization_header_carries_the_bot_prefix() {
        let header = authorization_header(&Secret::new("abc"));
        assert_eq!(header, "Bot abc");
    }

    #[test]
    fn fetch_url_matches_the_documented_endpoint() {
        let request = fetch_request(BASE, &channel(), 25);
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.url,
            "https://discord.com/api/v10/channels/123/messages?limit=25"
        );
        assert!(request.body.is_none());
    }

    #[test]
    fn fetch_limit_is_clamped_to_discord_bounds() {
        assert!(fetch_request(BASE, &channel(), 0).url.ends_with("limit=1"));
        assert!(fetch_request(BASE, &channel(), 9999)
            .url
            .ends_with("limit=100"));
    }

    #[test]
    fn a_cursor_reaches_the_url_as_discords_own_parameter() {
        let before = page_request(
            BASE,
            &channel(),
            10,
            Some(&MessageId("55".to_owned())),
            None,
        )
        .expect("one cursor is fine");
        assert_eq!(
            before.url,
            "https://discord.com/api/v10/channels/123/messages?limit=10&before=55"
        );
        let after = page_request(
            BASE,
            &channel(),
            10,
            None,
            Some(&MessageId("55".to_owned())),
        )
        .expect("one cursor is fine");
        assert_eq!(
            after.url,
            "https://discord.com/api/v10/channels/123/messages?limit=10&after=55"
        );
    }

    #[test]
    fn both_cursors_at_once_are_refused_rather_than_resolved_by_a_coin_flip() {
        // Discord does not define the behaviour, so a client that picked one would be inventing
        // a semantics — and the walk built on it would be wrong in a way only live traffic shows.
        assert!(matches!(
            page_request(
                BASE,
                &channel(),
                10,
                Some(&MessageId("1".to_owned())),
                Some(&MessageId("2".to_owned()))
            ),
            Err(DiscordError::Refused(_))
        ));
    }

    #[test]
    fn fetch_url_tolerates_a_trailing_slash_on_the_base() {
        let request = fetch_request("https://example.test/api/v10/", &channel(), 5);
        assert_eq!(
            request.url,
            "https://example.test/api/v10/channels/123/messages?limit=5"
        );
    }

    #[test]
    fn upstream_read_request_names_an_inclusive_message_boundary() {
        let request = mark_read_request(
            "https://bridge.example/api/v10/",
            &channel(),
            &MessageId("999".to_owned()),
        );
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "https://bridge.example/api/v10/channels/123/read"
        );
        assert_eq!(
            request.body,
            Some(serde_json::json!({ "message_id": "999" }))
        );
    }

    #[tokio::test]
    async fn upstream_read_accepts_an_empty_success_response() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let read = socket.read(&mut request).await.expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /channels/123/read HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("\r\nauthorization: bot abc\r\n"));
            assert!(request.contains(r#"{"message_id":"999"}"#));
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .expect("write response");
        });

        let client = HttpDiscordClient {
            thread_api: crate::threads::ThreadApi::Off,
            thread_inventory: threading::InventoryCache::default(),
            provider_name: "Discord".to_owned(),
            client: reqwest::Client::new(),
            api_base: format!("http://{address}"),
            bot_token: Secret::new("abc"),
            request_timeout: std::time::Duration::from_secs(20),
            channel_registration: false,
            upstream_read_marks: true,
            owner_user_id: None,
            post_limit: crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
            recent: crate::config::RecentReads::default(),
            limiter: RateLimiter::new(),
        };
        client
            .mark_read_upstream(&channel(), &MessageId("999".to_owned()))
            .await
            .expect("an empty 204 is a successful write");
        server.await.expect("mock server");
    }

    fn bridge_client(api_base: String, upstream_read_marks: bool) -> HttpDiscordClient {
        HttpDiscordClient {
            thread_api: crate::threads::ThreadApi::Off,
            thread_inventory: threading::InventoryCache::default(),
            provider_name: "Google Chat".to_owned(),
            client: reqwest::Client::new(),
            api_base,
            bot_token: Secret::new("abc"),
            request_timeout: std::time::Duration::from_secs(20),
            channel_registration: false,
            upstream_read_marks,
            owner_user_id: None,
            post_limit: crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
            recent: crate::config::RecentReads::default(),
            limiter: RateLimiter::new(),
        }
    }

    /// Answer one request on a loopback socket with `response`, handing back what was asked.
    async fn answer_once(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let read = socket.read(&mut request).await.expect("read request");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        (format!("http://{address}"), server)
    }

    #[test]
    fn upstream_clear_request_names_the_message_and_whether_it_is_a_dry_run() {
        // `#232 upstream-mark-read`.
        let request = upstream_clear_request(
            "https://bridge.example/api/v10/",
            &channel(),
            &MessageId("999".to_owned()),
            true,
        );
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "https://bridge.example/api/v10/channels/123/upstream-clear"
        );
        assert_eq!(
            request.body,
            Some(serde_json::json!({ "message_id": "999", "dry_run": true }))
        );
    }

    #[test]
    fn upstream_clear_answer_is_read_exactly_and_refused_when_it_names_no_target() {
        let outcome = parse_upstream_clear(&serde_json::json!({
            "scope": "space",
            "channel_id": "123",
            "message_id": "999",
            "target": "2026-10-11T10:42:00.123000Z",
            "previous": "2026-10-11T09:00:00Z",
            "advanced": true,
            "dry_run": false,
        }))
        .expect("a full answer");
        assert_eq!(
            outcome,
            crate::chat::UpstreamClearOutcome {
                scope: "space".to_owned(),
                target: "2026-10-11T10:42:00.123000Z".to_owned(),
                previous: Some("2026-10-11T09:00:00Z".to_owned()),
                advanced: true,
                dry_run: false,
            }
        );
        let never_read = parse_upstream_clear(&serde_json::json!({
            "target": "2026-10-11T10:42:00Z", "previous": null, "advanced": false, "dry_run": true,
        }))
        .expect("a space with no cursor yet");
        assert_eq!(never_read.previous, None);
        assert!(never_read.dry_run);
        for broken in [
            serde_json::json!({ "advanced": true }),
            serde_json::json!({ "target": "", "advanced": true }),
            serde_json::json!({ "target": "2026-10-11T10:42:00Z" }),
            serde_json::json!({ "target": "2026-10-11T10:42:00Z", "advanced": true, "previous": 7, "dry_run": false }),
            // A bridge that does not say whether it ran dry cannot be trusted with a dry run.
            serde_json::json!({ "target": "2026-10-11T10:42:00Z", "advanced": true }),
            serde_json::json!({ "target": "2026-10-11T10:42:00Z", "advanced": true, "dry_run": "no" }),
            serde_json::Value::Null,
        ] {
            assert!(
                matches!(parse_upstream_clear(&broken), Err(ChatError::Shape(_))),
                "{broken} was accepted"
            );
        }
    }

    #[tokio::test]
    async fn upstream_clear_posts_to_the_bridge_and_reads_its_answer() {
        let (base, server) = answer_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\
             Content-Length: 135\r\n\r\n\
             {\"scope\":\"space\",\"channel_id\":\"123\",\"message_id\":\"999\",\"target\":\"2026-10-11T10:42:00Z\",\"previous\":null,\"advanced\":false,\"dry_run\":true}",
        )
        .await;
        let client = bridge_client(base, true);
        assert!(client.supports_upstream_clear());
        let outcome = client
            .clear_upstream_through(&channel(), &MessageId("999".to_owned()), true)
            .await
            .expect("the bridge answered");
        assert!(outcome.dry_run);
        assert!(!outcome.advanced);
        assert_eq!(outcome.target, "2026-10-11T10:42:00Z");
        let request = server.await.expect("mock server");
        assert!(
            request.starts_with("POST /channels/123/upstream-clear HTTP/1.1"),
            "{request}"
        );
        assert!(request.contains(r#""dry_run":true"#), "{request}");
        assert!(request.contains(r#""message_id":"999""#), "{request}");
    }

    #[tokio::test]
    async fn upstream_clear_carries_a_bridge_refusal_through_as_its_status() {
        let (base, server) = answer_once(
            "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nConnection: close\r\n\
             Content-Length: 26\r\n\r\n{\"code\":\"thread_channel\"}\n",
        )
        .await;
        let error = bridge_client(base, true)
            .clear_upstream_through(&channel(), &MessageId("999".to_owned()), false)
            .await
            .expect_err("409 is a refusal");
        assert!(
            matches!(error.cause(), ChatError::Status { status: 409, body } if body.contains("thread_channel")),
            "{error:?}"
        );
        server.await.expect("mock server");
    }

    #[tokio::test]
    async fn upstream_clear_is_never_offered_against_discord_itself_or_without_the_opt_in() {
        let official = bridge_client(BASE.to_owned(), true);
        assert!(!official.supports_upstream_clear());
        let error = official
            .clear_upstream_through(&channel(), &MessageId("999".to_owned()), false)
            .await
            .expect_err("refused before any request");
        assert!(matches!(error.cause(), ChatError::Refused(_)), "{error:?}");
        let not_opted_in = bridge_client("https://bridge.example/api/v10".to_owned(), false);
        assert!(!not_opted_in.supports_upstream_clear());
    }

    #[tokio::test]
    async fn channel_registration_uses_the_live_bridge_contract() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept registration");
            let mut request = vec![0_u8; 4096];
            let read = socket.read(&mut request).await.expect("read registration");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /channels HTTP/1.1"));
            assert!(request.contains(
                r#"{"label":"release thread","source":"https://chat.example/space/thread"}"#
            ));
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 51\r\nConnection: close\r\n\r\n{\"id\":\"managed-123\",\"created\":true,\"writable\":true}",
                )
                .await
                .expect("write registration response");

            let (mut socket, _) = listener.accept().await.expect("accept removal");
            let mut request = vec![0_u8; 4096];
            let read = socket.read(&mut request).await.expect("read removal");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("DELETE /channels/managed-123 HTTP/1.1"));
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .expect("write removal response");
        });

        let client = HttpDiscordClient {
            thread_api: crate::threads::ThreadApi::Off,
            thread_inventory: threading::InventoryCache::default(),
            provider_name: "Google Chat".to_owned(),
            client: reqwest::Client::new(),
            api_base: format!("http://{address}"),
            bot_token: Secret::new("abc"),
            request_timeout: std::time::Duration::from_secs(20),
            channel_registration: true,
            upstream_read_marks: false,
            owner_user_id: None,
            post_limit: crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
            recent: crate::config::RecentReads::default(),
            limiter: RateLimiter::new(),
        };
        let registered = client
            .register_channel("https://chat.example/space/thread", "release thread")
            .await
            .expect("registration succeeds");
        assert_eq!(registered.id.as_str(), "managed-123");
        assert!(registered.created);
        assert!(registered.writable);
        client
            .unregister_channel(&registered.id)
            .await
            .expect("removal succeeds");
        server.await.expect("mock server");
    }

    #[tokio::test]
    async fn malformed_writable_rolls_back_a_new_registration() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept registration");
            let mut request = vec![0_u8; 4096];
            let _read = socket.read(&mut request).await.expect("read registration");
            let body = r#"{"id":"managed-bad","created":true,"writable":"yes"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write malformed response");

            let (mut socket, _) = listener.accept().await.expect("accept compensation");
            let mut request = vec![0_u8; 4096];
            let read = socket.read(&mut request).await.expect("read compensation");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("DELETE /channels/managed-bad HTTP/1.1"));
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .expect("write compensation response");
        });

        let client = HttpDiscordClient {
            thread_api: crate::threads::ThreadApi::Off,
            thread_inventory: threading::InventoryCache::default(),
            provider_name: "Google Chat".to_owned(),
            client: reqwest::Client::new(),
            api_base: format!("http://{address}"),
            bot_token: Secret::new("abc"),
            request_timeout: std::time::Duration::from_secs(20),
            channel_registration: true,
            upstream_read_marks: false,
            owner_user_id: None,
            post_limit: crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
            recent: crate::config::RecentReads::default(),
            limiter: RateLimiter::new(),
        };
        assert!(matches!(
            client.register_channel("source", "label").await.expect_err("invalid policy").cause(),
            ChatError::Shape(detail) if detail.contains("writable")
        ));
        server.await.expect("mock server");
    }

    #[tokio::test]
    async fn malformed_policy_does_not_delete_a_reused_registration() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept registration");
            let mut request = vec![0_u8; 4096];
            let _read = socket.read(&mut request).await.expect("read registration");
            let body = r#"{"id":"managed-existing","created":false,"writable":"yes"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write malformed response");
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "a reused registration was deleted"
            );
        });

        let client = HttpDiscordClient {
            thread_api: crate::threads::ThreadApi::Off,
            thread_inventory: threading::InventoryCache::default(),
            provider_name: "Google Chat".to_owned(),
            client: reqwest::Client::new(),
            api_base: format!("http://{address}"),
            bot_token: Secret::new("abc"),
            request_timeout: std::time::Duration::from_secs(20),
            channel_registration: true,
            upstream_read_marks: false,
            owner_user_id: None,
            post_limit: crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
            recent: crate::config::RecentReads::default(),
            limiter: RateLimiter::new(),
        };
        assert!(matches!(
            client.register_channel("source", "label").await.expect_err("invalid policy").cause(),
            ChatError::Shape(detail) if detail.contains("writable")
        ));
        server.await.expect("mock server");
    }

    #[tokio::test]
    async fn registration_ids_cannot_equal_the_bot_token() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const TOKEN: &str = "path-safe-bot-token-123456789";
        for body in [
            format!(r#"{{"id":"{TOKEN}","created":false,"writable":true}}"#),
            format!(r#"{{"id":"{TOKEN}","created":true,"writable":"yes"}}"#),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback");
            let address = listener.local_addr().expect("listener address");
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept registration");
                let mut request = vec![0_u8; 4096];
                let _read = socket.read(&mut request).await.expect("read registration");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                        .await
                        .is_err(),
                    "the credential-shaped id was sent back in a DELETE path"
                );
            });

            let client = HttpDiscordClient {
                thread_api: crate::threads::ThreadApi::Off,
                thread_inventory: threading::InventoryCache::default(),
                provider_name: "Google Chat".to_owned(),
                client: reqwest::Client::new(),
                api_base: format!("http://{address}"),
                bot_token: Secret::new(TOKEN),
                request_timeout: std::time::Duration::from_secs(20),
                channel_registration: true,
                upstream_read_marks: false,
                owner_user_id: None,
                post_limit: crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
                recent: crate::config::RecentReads::default(),
                limiter: RateLimiter::new(),
            };
            let error = client
                .register_channel("source", "label")
                .await
                .expect_err("credential-shaped ids must be rejected");
            assert!(!error.to_string().contains(TOKEN));
            server.await.expect("mock server");
        }
    }

    #[tokio::test]
    async fn bridge_error_bodies_cannot_echo_the_bot_token() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const TOKEN: &str = "bridge-secret-token-123456789";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept registration");
            let mut request = vec![0_u8; 4096];
            let _read = socket.read(&mut request).await.expect("read registration");
            let body = format!("bridge rejected Authorization: Bot {TOKEN}; token={TOKEN}");
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write rejection");
        });

        let client = HttpDiscordClient {
            thread_api: crate::threads::ThreadApi::Off,
            thread_inventory: threading::InventoryCache::default(),
            provider_name: "Google Chat".to_owned(),
            client: reqwest::Client::new(),
            api_base: format!("http://{address}"),
            bot_token: Secret::new(TOKEN),
            request_timeout: std::time::Duration::from_secs(20),
            channel_registration: true,
            upstream_read_marks: false,
            owner_user_id: None,
            post_limit: crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
            recent: crate::config::RecentReads::default(),
            limiter: RateLimiter::new(),
        };
        let error = client
            .register_channel("source", "label")
            .await
            .expect_err("bridge rejects registration");
        let rendered = error.to_string();
        assert!(rendered.contains("HTTP 401"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains(TOKEN), "token leaked: {rendered}");
        server.await.expect("mock server");
    }

    #[tokio::test]
    async fn bridge_post_retries_reuse_one_path_safe_nonce() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept post");
                let mut request = vec![0_u8; 4096];
                let read = socket.read(&mut request).await.expect("read post");
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.starts_with("POST /channels/managed-123/messages HTTP/1.1"));
                bodies.push(
                    request
                        .split("\r\n\r\n")
                        .nth(1)
                        .expect("request body")
                        .to_owned(),
                );
                if attempt == 0 {
                    let body = r#"{"retry_after":0.001,"global":false}"#;
                    let response = format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write rate limit");
                } else {
                    let body = r#"{"id":"9001","channel_id":"managed-123","content":"hello","timestamp":"2026-09-19T00:00:00Z","author":{"id":"7","username":"bot","bot":true}}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write post response");
                }
            }
            bodies
        });

        let client = HttpDiscordClient {
            thread_api: crate::threads::ThreadApi::Off,
            thread_inventory: threading::InventoryCache::default(),
            provider_name: "Google Chat".to_owned(),
            client: reqwest::Client::new(),
            api_base: format!("http://{address}"),
            bot_token: Secret::new("abc"),
            request_timeout: std::time::Duration::from_secs(20),
            channel_registration: true,
            upstream_read_marks: false,
            owner_user_id: None,
            post_limit: crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN),
            recent: crate::config::RecentReads::default(),
            limiter: RateLimiter::new(),
        };
        client
            .post_message(&ChannelId("managed-123".to_owned()), "hello", None)
            .await
            .expect("retry succeeds");
        let bodies = server.await.expect("mock server");
        let first: serde_json::Value = serde_json::from_str(&bodies[0]).expect("first JSON");
        let second: serde_json::Value = serde_json::from_str(&bodies[1]).expect("second JSON");
        let nonce = first["nonce"].as_str().expect("bridge nonce");
        assert_eq!(second["nonce"], nonce, "a retry minted a different nonce");
        assert!(
            !nonce.is_empty()
                && nonce.len() <= 128
                && nonce.bytes().all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'.' | b'_' | b'~')),
            "nonce is not path-safe: {nonce:?}"
        );
    }

    #[test]
    fn post_body_suppresses_mentions() {
        let request =
            post_request(BASE, &channel(), "@everyone deploy is green", None).expect("valid post");
        let body = request.body.expect("post has a body");
        assert_eq!(body["content"], "@everyone deploy is green");
        assert_eq!(
            body["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "a posted message must never be able to ping the server"
        );
        assert_eq!(
            body["allowed_mentions"]["users"],
            serde_json::json!([]),
            "a message that mentions nobody must not authorize any ping"
        );
        assert!(body.get("message_reference").is_none());
        assert!(
            body.get("nonce").is_none(),
            "official Discord must not receive the provider bridge nonce"
        );
        assert_eq!(
            request.url,
            "https://discord.com/api/v10/channels/123/messages"
        );
    }

    #[test]
    fn a_user_mention_in_the_body_is_authorized_so_it_actually_notifies() {
        // The end of the feature: an id read off a message, written back as `<@id>`, has to ping.
        let request = post_request(
            BASE,
            &channel(),
            "<@1532416065114607829> the mac runner is back",
            None,
        )
        .expect("valid post");
        let body = request.body.expect("post has a body");
        assert_eq!(
            body["allowed_mentions"]["users"],
            serde_json::json!(["1532416065114607829"]),
            "the mentioned user must be authorized, or the ping is silently dropped"
        );
        assert_eq!(
            body["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "authorizing one user must not re-enable Discord's own scan of the body"
        );
    }

    #[test]
    fn everyone_here_and_role_mentions_stay_unreachable() {
        let request = post_request(
            BASE,
            &channel(),
            "@everyone @here <@&999888777> deploy is green",
            None,
        )
        .expect("valid post");
        let body = request.body.expect("post has a body");
        assert_eq!(
            body["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "the whole-server ping must stay off"
        );
        assert_eq!(
            body["allowed_mentions"]["users"],
            serde_json::json!([]),
            "a ROLE mention must not be smuggled through the user allowlist"
        );
    }

    #[test]
    fn mention_scanning_reads_discord_syntax_and_nothing_else() {
        assert_eq!(mentioned_user_ids("hello @coding_agent"), vec![]);
        assert_eq!(
            mentioned_user_ids("<@1> and <@!2> and <@1> again"),
            vec![UserId("1".to_owned()), UserId("2".to_owned())],
            "the legacy nickname form counts, and duplicates are not repeated"
        );
        assert_eq!(
            mentioned_user_ids("<@notanid> <@> <@12x> <@#5>"),
            vec![],
            "only decimal snowflakes are mentions"
        );
        assert_eq!(
            mentioned_user_ids("<@&7>"),
            vec![],
            "a role mention is not a user mention"
        );
    }

    #[test]
    fn the_mention_list_is_capped_at_discords_own_ceiling() {
        // Over the ceiling Discord rejects the whole message, so the post would fail entirely.
        let body_text = (0..DISCORD_MAX_MENTIONED_USERS + 20)
            .map(|i| format!("<@{}>", 1000 + i))
            .collect::<Vec<_>>()
            .join(" ");
        let request = post_request(BASE, &channel(), &body_text, None).expect("valid post");
        let body = request.body.expect("post has a body");
        assert_eq!(
            body["allowed_mentions"]["users"]
                .as_array()
                .expect("a list")
                .len(),
            DISCORD_MAX_MENTIONED_USERS
        );
    }

    #[test]
    fn post_body_carries_the_reply_target() {
        let request = post_request(BASE, &channel(), "ack", Some(&MessageId("999".to_owned())))
            .expect("valid post");
        let body = request.body.expect("post has a body");
        assert_eq!(body["message_reference"]["message_id"], "999");
    }

    /// `#263 post-split-per-provider`. Only a bridge continues a long post in the thread its
    /// first part is in; Discord itself keeps consecutive messages, threads or not. And a
    /// bridge's post is held to the size it was configured with, not to Discord's.
    #[test]
    fn only_a_bridge_continues_a_long_post_and_each_is_held_to_its_own_size() {
        let first = parse_message(&serde_json::json!({
            "id": "1000000000000000002", "channel_id": "123", "content": "part one",
            "timestamp": "2026-10-12T12:00:00.000000+00:00",
            "author": { "id": "7", "username": "bridge", "bot": true },
            "thread": { "id": "t_1", "root_message_id": "1000000000000000002", "is_root": true,
                        "reply_count": null, "reply_count_exact": false },
        }))
        .expect("message");
        let mut config = client_config("https://bridge.example/v1", true);
        for (thread_api, expected) in [
            (crate::threads::ThreadApi::Bridge, Some("t_1")),
            (crate::threads::ThreadApi::Native, None),
            (crate::threads::ThreadApi::Off, None),
        ] {
            config.thread_api = thread_api;
            let client = HttpDiscordClient::new(&config).expect("client");
            assert_eq!(
                client.continuation_thread(&channel(), &first).as_deref(),
                expected,
                "{thread_api:?}"
            );
        }
        let native = HttpDiscordClient::new(&client_config(BASE, false)).expect("client");
        assert_eq!(native.continuation_thread(&channel(), &first), None);
        assert_eq!(
            native.post_limit(&channel()),
            crate::chat::PostLimit::chars(DISCORD_MAX_CONTENT_LEN)
        );

        config.max_post_bytes = Some(30_000);
        let bridge = HttpDiscordClient::new(&config).expect("client");
        let long = "y".repeat(10_000);
        let request = post_request_with_nonce(
            "https://bridge.example/v1",
            &channel(),
            &long,
            None,
            None,
            &bridge.post_limit(&channel()),
        );
        assert!(request.is_ok(), "a bridge was refused a post it takes");
        let too_long = "y".repeat(30_001);
        assert!(matches!(
            post_request_with_nonce(
                "https://bridge.example/v1",
                &channel(),
                &too_long,
                None,
                None,
                &bridge.post_limit(&channel()),
            ),
            Err(DiscordError::Refused(detail)) if detail.contains("30000")
        ));
    }

    #[test]
    fn empty_and_oversized_posts_are_refused_before_discord_sees_them() {
        assert!(matches!(
            post_request(BASE, &channel(), "   ", None),
            Err(DiscordError::Refused(_))
        ));
        let long = "x".repeat(DISCORD_MAX_CONTENT_LEN + 1);
        assert!(matches!(
            post_request(BASE, &channel(), &long, None),
            Err(DiscordError::Refused(_))
        ));
        let exact = "x".repeat(DISCORD_MAX_CONTENT_LEN);
        assert!(post_request(BASE, &channel(), &exact, None).is_ok());
    }

    fn sample_payload() -> serde_json::Value {
        // Shaped after Discord's documented message object, newest first as Discord returns it.
        serde_json::json!([
          {
            "id": "1000000000000000002",
            "channel_id": "123",
            "content": "second",
            "timestamp": "2026-08-18T12:01:00.000000+00:00",
            "author": { "id": "300000000000000003", "username": "coder-bot", "global_name": null, "bot": true }
          },
          {
            "id": "1000000000000000001",
            "channel_id": "123",
            "content": "first",
            "timestamp": "2026-08-18T12:00:00.000000+00:00",
            "author": { "id": "400000000000000004", "username": "alice", "global_name": "Alice Doe", "bot": false }
          }
        ])
    }

    #[test]
    fn a_reply_carries_the_id_of_what_it_answers_and_an_ordinary_message_carries_none() {
        // Discord records a reply on the ANSWERING message, as `message_reference.message_id`, and
        // there is no field at all on the message being answered. Everything the inbox view does
        // with "this has been replied to" is derived from this one pointer, so it has to survive
        // the mapping.
        let payload = serde_json::json!([
          {
            "id": "1000000000000000002",
            "channel_id": "123",
            "content": "answering you",
            "timestamp": "2026-08-18T12:01:00.000000+00:00",
            "author": { "id": "3", "username": "coder-bot", "global_name": null, "bot": true },
            "message_reference": { "message_id": "1000000000000000001", "channel_id": "123" }
          },
          {
            "id": "1000000000000000001",
            "channel_id": "123",
            "content": "asking you",
            "timestamp": "2026-08-18T12:00:00.000000+00:00",
            "author": { "id": "4", "username": "alice", "global_name": "Alice Doe", "bot": false }
          }
        ]);
        let messages = parse_message_list(&payload).expect("parses");
        assert_eq!(messages[0].content, "asking you");
        assert_eq!(
            messages[0].reply_to, None,
            "the message being ANSWERED must not claim to be a reply; Discord marks only the answer"
        );
        assert_eq!(
            messages[1].reply_to.as_ref().map(MessageId::as_str),
            Some("1000000000000000001"),
            "the reply lost the id of what it answers"
        );
    }

    #[test]
    fn a_reference_without_a_message_id_is_an_ordinary_message_rather_than_a_shape_error() {
        // Discord uses `message_reference` for forwards and crossposts too, and those need not
        // carry the field this server reads. A missing pointer is "not a reply we can resolve",
        // never a refusal to parse the channel.
        let payload = serde_json::json!([{
            "id": "1000000000000000009",
            "channel_id": "123",
            "content": "forwarded",
            "timestamp": "2026-08-18T12:02:00.000000+00:00",
            "author": { "id": "5", "username": "someone", "global_name": null, "bot": false },
            "message_reference": { "channel_id": "456" }
        }]);
        let messages = parse_message_list(&payload).expect("parses");
        assert_eq!(messages[0].reply_to, None);
    }

    #[test]
    fn payload_is_mapped_and_reordered_oldest_first() {
        let messages = parse_message_list(&sample_payload()).expect("parses");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "first");
        assert_eq!(
            messages[0].author, "Alice Doe",
            "global_name wins over username"
        );
        assert!(!messages[0].author_is_bot);
        assert_eq!(messages[1].content, "second");
        assert_eq!(
            messages[1].author, "coder-bot",
            "null global_name falls back to username"
        );
        assert!(messages[1].author_is_bot);
        assert_eq!(messages[1].channel_id.as_str(), "123");
        assert_eq!(
            messages[0].author_id.as_str(),
            "400000000000000004",
            "the author snowflake must survive the mapping; it is what a mention is built from"
        );
        assert_eq!(
            messages[1].author_id.as_str(),
            "300000000000000003",
            "a BOT author carries an id too: addressing another agent is a legitimate reply"
        );
    }

    #[test]
    fn a_message_whose_author_has_no_id_fails_loudly() {
        // Defaulting to an empty id would mint `<@>`, which is not a mention and not an error
        // either — it would just quietly fail to notify anyone.
        let value = serde_json::json!({
            "id": "5", "channel_id": "123", "timestamp": "t",
            "author": { "username": "u" }
        });
        assert!(matches!(parse_message(&value), Err(DiscordError::Shape(_))));
    }

    #[test]
    fn a_message_with_no_content_is_kept_not_dropped() {
        let value = serde_json::json!({
            "id": "5", "channel_id": "123", "timestamp": "t",
            "author": { "id": "500000000000000005", "username": "u" }
        });
        let message = parse_message(&value).expect("attachment-only messages still parse");
        assert_eq!(message.content, "");
        assert!(!message.author_is_bot);
    }

    fn reacted(reactions: serde_json::Value) -> Message {
        parse_message(&serde_json::json!({
            "id": "5", "channel_id": "123", "timestamp": "t", "content": "ack me",
            "author": { "id": "500000000000000005", "username": "u" },
            "reactions": reactions,
        }))
        .expect("a message with reactions parses")
    }

    #[test]
    fn reactions_are_read_as_discord_and_the_bridge_send_them() {
        // `#219 emoji-reactions`. The shape both Discord's message object and the Google Chat
        // bridge carry: a standard emoji is its glyph with a null id, a custom one has an id.
        let message = reacted(serde_json::json!([
            {"emoji": {"id": null, "name": "👀"}, "count": 1, "me": false,
             "count_details": {"burst": 0, "normal": 1}, "burst_colors": []},
            {"emoji": {"name": "✅"}, "count": 1},
            {"emoji": {"id": "112233", "name": "party-parrot", "animated": false}, "count": 3},
            {"emoji": {"id": "customEmojis/abc", "name": ":ack-claim:"}, "count": 1},
            {"emoji": {"id": "998877", "name": null}, "count": 2},
        ]));
        let reactions = message.reactions.expect("an array is an answer");
        let shown: Vec<(&str, bool, Option<&str>, u32)> = reactions
            .iter()
            .map(|r| (r.emoji.as_str(), r.custom, r.custom_id.as_deref(), r.count))
            .collect();
        assert_eq!(
            shown,
            [
                ("👀", false, None, 1),
                ("✅", false, None, 1),
                ("party-parrot", true, Some("112233"), 3),
                // The bridge may pass a name with its colons; the page adds its own.
                ("ack-claim", true, Some("customEmojis/abc"), 1),
                // A deleted custom emoji has no name left, and is still a reaction.
                ("emoji", true, Some("998877"), 2),
            ]
        );
    }

    #[test]
    fn no_reactions_array_is_not_the_same_answer_as_an_empty_one() {
        // Absent or not an array: this copy says nothing about reactions. Empty: there are none.
        let value = serde_json::json!({
            "id": "5", "channel_id": "123", "timestamp": "t",
            "author": { "id": "500000000000000005", "username": "u" }
        });
        assert_eq!(parse_message(&value).expect("parses").reactions, None);
        assert_eq!(reacted(serde_json::Value::Null).reactions, None);
        assert_eq!(reacted(serde_json::json!({"👀": 1})).reactions, None);
        assert_eq!(reacted(serde_json::json!([])).reactions, Some(Vec::new()));
    }

    #[test]
    fn a_malformed_reaction_is_skipped_and_never_fails_the_message() {
        let message = reacted(serde_json::json!([
            {"emoji": {"name": "👀"}, "count": 0},
            {"emoji": {"name": "👀"}, "count": -1},
            {"emoji": {"name": "👀"}, "count": "1"},
            {"emoji": {"name": "  "}, "count": 1},
            {"emoji": {}, "count": 1},
            {"count": 1},
            "👀",
            {"emoji": {"name": "✅"}, "count": 5_000_000_000_u64},
        ]));
        let reactions = message.reactions.expect("an array is an answer");
        assert_eq!(reactions.len(), 1, "{reactions:?}");
        assert_eq!(reactions[0].emoji, "✅");
        assert_eq!(reactions[0].count, u32::MAX, "a count past u32 saturates");
        assert_eq!(message.content, "ack me", "the message itself survived");
    }

    #[test]
    fn reactions_are_bounded_in_number_and_length() {
        let many: Vec<serde_json::Value> = (0..MAX_REACTIONS + 10)
            .map(|i| serde_json::json!({"emoji": {"id": i.to_string(), "name": format!("e{i}")}, "count": 1}))
            .collect();
        let reactions = reacted(serde_json::Value::Array(many))
            .reactions
            .expect("an answer");
        assert_eq!(reactions.len(), MAX_REACTIONS);
        assert_eq!(reactions[0].emoji, "e0", "the provider's order is kept");
        let long = "x".repeat(MAX_REACTION_CHARS * 3);
        let reactions =
            reacted(serde_json::json!([{"emoji": {"id": long, "name": long}, "count": 1}]))
                .reactions
                .expect("an answer");
        assert_eq!(reactions[0].emoji.chars().count(), MAX_REACTION_CHARS);
        assert_eq!(
            reactions[0]
                .custom_id
                .as_deref()
                .map(|id| id.chars().count()),
            Some(MAX_REACTION_CHARS)
        );
    }

    #[test]
    fn a_malformed_payload_fails_loudly() {
        assert!(matches!(
            parse_message_list(&serde_json::json!({"not": "an array"})),
            Err(DiscordError::Shape(_))
        ));
        assert!(matches!(
            parse_message(&serde_json::json!({"id": "5"})),
            Err(DiscordError::Shape(_))
        ));
    }
}
