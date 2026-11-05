//! The operations behind BOTH the JSON API and the MCP tools.
//!
//! There are now two front doors onto the same capability — `/api/v1/...` for the phone web app,
//! and `/mcp` for a hosted voice agent — and the dangerous failure mode is that they drift: one
//! of them forgets the channel allowlist, or the read/write split, or the untrusted-content
//! boundary. So the policy lives here, once, and both transports call it.
//!
//! What is deliberately NOT here: authentication. Deciding whether a caller may act at all is the
//! transport's job, because the two transports present credentials differently (a bearer header
//! on a REST route, a bearer header on a JSON-RPC POST) even though they check the same tokens.
//! Every function below assumes the caller has already been authorized for the scope it needs;
//! the scope each one needs is stated in its documentation and enforced by its only callers.

use crate::chat::ChatError;
use crate::model::{ChannelId, ChannelInfo, Message, MessageId};
use crate::retrieval::{self, Resolution};
use crate::state::AppState;
use crate::summary::{self, DigestEntry, DEFAULT_SUMMARY_CHARS};

/// Why an operation could not be carried out.
///
/// These are the refusals a *correctly authenticated* caller can hit. Authentication failures do
/// not appear here at all — see [`crate::auth`].
#[derive(Debug, thiserror::Error)]
pub enum OpError {
    /// The channel is not in the configured allowlist. This is the same answer a caller gets for
    /// a channel that does not exist, on purpose: the allowlist is not a directory to probe.
    #[error("that channel is not configured on this server")]
    UnknownChannel,
    /// The message is not in the window this server fetched.
    #[error("that message is not in the recent window for this channel")]
    UnknownMessage,
    /// A resolve call arrived with nothing to resolve.
    #[error("query must not be empty")]
    EmptyQuery,
    /// The channel is configured, but read-only.
    #[error("this channel is configured read-only")]
    ChannelNotWritable,
    /// A cursor argument was not a Discord message id.
    #[error("that cursor is not a message id; use the one the previous page handed back")]
    InvalidCursor,
    /// A forward cursor that does not decode, or was issued for another channel, view or thread.
    /// `#203 incremental-refresh`. The caller reads the newest page again, which issues one that
    /// fits.
    #[error(
        "that forward cursor was not issued for this channel and view; read the newest page again"
    )]
    CursorMismatch,
    /// The requested span could not be turned into a single walk.
    #[error(
        "that is not a span this server can walk: give either a cursor to step back from, or a \
         start time and an optional end time, but not both"
    )]
    InvalidRange,
    /// A relative window (`hours`) that is not a whole number of hours inside the allowed range.
    /// `#190 voice-agent-tools`.
    #[error("hours must be a whole number from 1 to {MAX_WINDOW_HOURS}")]
    InvalidHours,
    /// A relative window given together with an absolute one. `hours` already says where the span
    /// starts, so combining it with `since`, `until` or `before` would leave the caller unable to
    /// know which of the two it got.
    #[error(
        "give hours on its own: it already says where the span starts, so it cannot be combined \
         with since, until or before"
    )]
    HoursWithSpan,
    /// Some parts of a split message reached the chat provider and the rest did not.
    ///
    /// A distinct variant rather than a plain [`ChatError`], because the two call for opposite things
    /// from the caller. A plain failure means "nothing happened, send it again". This means "half
    /// of it is in the channel and cannot be unsent" — sending the whole thing again would post
    /// the first half twice. `unsent` is the remainder, and once the caller has cleared their
    /// draft it is the only copy of that text in existence, so it travels with the error rather
    /// than being reconstructable from it.
    #[error("posted {posted} part(s) and then failed: {cause}")]
    PartiallyPosted {
        /// How many parts the provider accepted before the failure.
        posted: usize,
        /// The text that did not reach the provider, ready to be sent on its own.
        unsent: String,
        /// Why the next part failed.
        cause: String,
        /// Whether that failure was a moment rather than an answer; see [`ChatError::is_transient`].
        retryable: bool,
        /// Whether sending `unsent` again, with the same key, to `thread_id`, and with the reply
        /// target only when `thread_id` is the request's own, asks the provider for the part that
        /// failed under the very words and key it had, and for nothing already posted. See
        /// [`unsent_remainder`].
        resumable: bool,
        /// The thread `unsent` belongs in: the request's own, or — `#263
        /// post-split-per-provider` — the thread the first part started, where the rest of a
        /// post to the conversation was going. `None` for the conversation itself.
        thread_id: Option<String>,
        /// Whether the part that failed may have been posted anyway, so `posted` counts only the
        /// parts known to have landed; see [`ChatError::may_have_delivered`]. `#264
        /// voice-send-approval`: what lets a voice agent say "not confirmed" rather than "not
        /// sent" about a post whose answer was lost.
        uncertain: bool,
    },

    /// A catch-up's deadline passed before this channel was read, and nothing earlier is held.
    /// `#210 voice-catchup-speed`. The read carries on without the caller, so asking again in a
    /// moment is answered from what it brings.
    #[error(
        "the chat service did not answer in time and this server holds no earlier copy; it is \
         still being read, so ask again in a moment"
    )]
    NotReadInTime,

    /// The configured chat provider failed, or refused the request before it was sent.
    #[error(transparent)]
    Chat(#[from] ChatError),
    /// The durable state store failed, or is not configured at all.
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    /// The summariser could not produce a summary.
    #[error(transparent)]
    Summarizer(#[from] crate::summarize::SummaryError),
}

impl OpError {
    /// Stable machine-readable code, shared by both transports so a client can branch on it.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownChannel => "unknown_channel",
            Self::UnknownMessage => "unknown_message",
            Self::EmptyQuery => "empty_query",
            Self::ChannelNotWritable => "channel_not_writable",
            Self::InvalidCursor => "invalid_cursor",
            Self::InvalidRange | Self::HoursWithSpan => "invalid_range",
            Self::InvalidHours => "invalid_hours",
            Self::CursorMismatch => "cursor_mismatch",
            Self::PartiallyPosted { .. } => "partially_posted",
            Self::NotReadInTime => "not_read_in_time",
            Self::Chat(error) if matches!(error.cause(), ChatError::Refused(_)) => "refused",
            Self::Chat(error) if matches!(error.cause(), ChatError::CursorExpired(_)) => {
                "cursor_expired"
            }
            Self::Chat(_) => "chat_error",
            Self::Store(inner) => inner.code(),
            Self::Summarizer(inner) => inner.code(),
        }
    }
}

/// Resolve a caller-supplied channel id against the allowlist.
///
/// This is the single gate every channel-scoped operation goes through. A snowflake that is not
/// configured does not exist here, however many channels the bot is actually in.
async fn allowed(state: &AppState, channel_id: &str) -> Result<ChannelInfo, OpError> {
    let mut channel = configured(state, channel_id)?;
    channel.alias = aliases(state).await.remove(channel.id.as_str());
    Ok(channel)
}

/// The allowlist gate WITHOUT the alias overlay.
///
/// Two callers, and both of them have a reason not to pay for a store read: [`watch`], which is
/// deliberately synchronous and throws the channel away, and the two alias operations themselves,
/// which are about to learn the alias from the write they are making. Everything that RENDERS a
/// channel name goes through [`allowed`] instead.
fn configured(state: &AppState, channel_id: &str) -> Result<ChannelInfo, OpError> {
    state.channel(channel_id).ok_or(OpError::UnknownChannel)
}

/// The channels this server is configured for, wearing the operator's own names. Read scope.
pub async fn channels(state: &AppState) -> Vec<ChannelInfo> {
    let aliases = aliases(state).await;
    state
        .all_channels()
        .into_iter()
        .map(|mut channel| {
            channel.alias = aliases.get(channel.id.as_str()).cloned();
            channel
        })
        .collect()
}

/// The configured channels the owner took off his list, wearing his own names, so Settings can
/// offer each one back under the name he knew it by. `#199 removable-config-channels`.
pub async fn hidden_channels(state: &AppState) -> Vec<ChannelInfo> {
    let hidden = state.hidden_configured_channels();
    if hidden.is_empty() {
        return hidden;
    }
    let aliases = aliases(state).await;
    hidden
        .into_iter()
        .map(|mut channel| {
            channel.alias = aliases.get(channel.id.as_str()).cloned();
            channel
        })
        .collect()
}

/// The configured channel a caller named, by id or by the name it was given.
///
/// A model asked about "lead team" passes "lead team", however clearly the tool description
/// said to pass an id — so an exact id wins, and failing that, a configured label or operator
/// alias matching case-insensitively (surrounding whitespace ignored) stands in for its id. This
/// only picks a channel from the allowlist; it never admits one. A name two channels share
/// names neither, so an ambiguous name resolves to nothing rather than to a guess.
pub async fn channel_named(state: &AppState, name: &str) -> Option<ChannelId> {
    let name = name.trim();
    let channels = channels(state).await;
    if let Some(exact) = channels.iter().find(|c| c.id.as_str() == name) {
        return Some(exact.id.clone());
    }
    if name.is_empty() {
        return None;
    }
    let same = |candidate: &str| candidate.trim().to_lowercase() == name.to_lowercase();
    let mut named = channels
        .iter()
        .filter(|c| same(&c.label) || c.alias.as_deref().is_some_and(same));
    let found = named.next()?;
    if named.next().is_some() {
        return None;
    }
    Some(found.id.clone())
}

/// The operator's local channel names, by channel id. `#39 channel-alias`.
///
/// **A failure here is not a failure of the operation asking.** An alias is a decoration on a
/// channel, so a store that is not configured — the ordinary state of a deployment that has never
/// set `storage.path` — must not make reading a channel impossible; it degrades to the configured
/// label, which is exactly what the reader saw before there were aliases at all. A real backend
/// failure degrades the same way and says so in the log, because the alternative is that a
/// corrupt row takes the whole bridge down.
async fn aliases(state: &AppState) -> std::collections::BTreeMap<String, String> {
    match state.store.channel_aliases().await {
        Ok(found) => found
            .into_iter()
            .map(|a| (a.channel.as_str().to_owned(), a.alias))
            .collect(),
        // Expected whenever no store is configured. Not worth a line per request.
        Err(crate::store::StoreError::Unavailable(_)) => std::collections::BTreeMap::new(),
        Err(error) => {
            tracing::warn!(
                %error,
                "could not read channel aliases; falling back to the configured labels"
            );
            std::collections::BTreeMap::new()
        }
    }
}

/// Give one channel a local name. Write scope, and the OPERATOR's act alone.
///
/// Nothing about this reaches Discord: the channel keeps whatever name it has there, and no
/// caller outside this deployment can see the one set here. It is deliberately absent from the
/// MCP tool manifest — a model that could rename the channels it is also reporting on could
/// quietly make its own reports unrecognisable.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist, so a snowflake nobody configured cannot be
/// used to grow the store one row at a time; [`OpError::Store`] when the text is unusable, when
/// no store is configured, or when the write fails.
pub async fn set_channel_alias(
    state: &AppState,
    channel_id: &str,
    alias: &str,
) -> Result<ChannelInfo, OpError> {
    let mut channel = configured(state, channel_id)?;
    let stored = state.store.set_channel_alias(&channel.id, alias).await?;
    channel.alias = Some(stored.alias);
    Ok(channel)
}

/// Drop one channel's local name, putting the configured label back. Write scope.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Store`] when the channel had no
/// alias, when no store is configured, or when the write fails.
pub async fn clear_channel_alias(
    state: &AppState,
    channel_id: &str,
) -> Result<ChannelInfo, OpError> {
    let channel = configured(state, channel_id)?;
    state.store.clear_channel_alias(&channel.id).await?;
    Ok(channel)
}

/// Fill in the two spoken forms — [`Message::spoken_time`] and [`Message::spoken_content`] — for a
/// freshly fetched batch.
///
/// THE ONLY PLACE the zone conversion happens. It sits here rather than at the render sites
/// because there are four of those — [`crate::untrusted::render_for_model`] and three handlers in
/// [`crate::mcp::protocol`] — and both front doors already funnel through this module. Formatting
/// per render site would mean writing the conversion four times and having it drift three ways.
///
/// Preparing the BODY for speech joined it here for exactly that reason, one gap later: the browser
/// was building its own spoken text out of `content`, so the path that reads new messages aloud was
/// the one path with no preparation at all. Doing it here means every reader of a message — the
/// page, the model, the audio route — is offered the same prepared body, and no new one has to
/// remember to ask.
///
/// One clock reading for the whole batch, so two messages a millisecond apart are not described
/// relative to two different "now"s.
///
/// Everything downstream PRINTS these fields and must never compute them.
fn stamp(state: &AppState, messages: &mut [Message]) {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    for message in messages {
        message.spoken_time = crate::clock::spoken(&message.timestamp, &state.config.timezone);
        message.spoken_content = prepared_body(state, &message.content, now_ms);
    }
}

/// The spoken form of one body, or EMPTY when saying it and reading it are the same thing.
///
/// Most chat is a sentence with no markdown, no timestamp and no id in it, and for those the
/// rewrite is the identity. Sending the result anyway would put a second copy of every message in
/// every window, down the connection of a phone, to say nothing new. See
/// [`Message::spoken_content`] for why one empty value can mean both "unprepared" and "unchanged".
///
/// Visible to the crate because the live ingest route has to do the same thing to a message that
/// never passes through [`stamp`] — it arrives already formed, from an adapter, and goes straight
/// to the page. Two callers, one definition, for exactly the reason `stamp` gives.
///
/// This is also where [`crate::config::SpeakableConfig`] is honoured, and the only place it needs
/// to be. With the switch off the field stays empty, which every reader already handles — it is
/// the same value a message carries before anything has prepared it, and
/// [`Message::spoken_body`] answers both with the raw body. So one branch here turns the pass off
/// for the page, the digest, the model and the audio route at once, and no reader learns a second
/// way to be given nothing.
pub(crate) fn prepared_body(state: &AppState, content: &str, now_ms: i64) -> String {
    if !state.config.speakable.enabled {
        return String::new();
    }
    let said = state
        .spoken_names
        .for_speech(content, now_ms, &state.config.timezone);
    if said == content {
        String::new()
    } else {
        said
    }
}

/// One fetched window of a channel, and what it took to fetch it.
///
/// The `limit` is carried alongside the messages for one reason: without it, `messages.len()` is
/// an unlabelled number that a caller will render as a channel total. It is not one. Discord's
/// REST API offers no message count for a guild text channel — `GET /channels/{id}/messages`
/// accepts `limit` 1..=100 and returns no total, and `message_count` / `total_message_sent` exist
/// only on THREAD channels — so the only thing this server can honestly say about a full window
/// is "there are at least this many".
#[derive(Clone, Debug)]
pub struct Window {
    /// Channel that was read.
    pub channel: ChannelInfo,
    /// Messages, oldest first.
    pub messages: Vec<Message>,
    /// The size actually asked of Discord, already clamped to what Discord will honour.
    pub limit: u16,
    /// The instant, RFC 3339, up to which the provider knows `messages` is complete, when it
    /// served a copy it could not bring up to date just now. `None` means now. `#208
    /// persistent-cache`: a bridge reading its Flat view says so; see [`crate::recent_view`].
    pub as_of: Option<String>,
}

impl Window {
    /// Whether `messages` is the WHOLE channel rather than a window onto it.
    ///
    /// A fetch that comes back with fewer messages than it asked for is Discord saying there is
    /// nothing older, so the count is the channel's. A fetch that comes back full says nothing
    /// about how much more there is. This is the only case in which a precise number may be shown.
    #[must_use]
    pub fn is_whole_channel(&self) -> bool {
        self.messages.len() < usize::from(self.limit)
    }
}

/// Recent messages of one channel, oldest first. Read scope.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] for a channel outside the allowlist; [`OpError::Chat`] when the
/// fetch fails.
pub async fn messages(
    state: &AppState,
    channel_id: &str,
    limit: Option<u16>,
) -> Result<Window, OpError> {
    let channel = allowed(state, channel_id).await?;
    // Clamped once: the size fetched, the size `crate::recent` records, and the size speech
    // preparation compares a remembered window against (`crate::read_ahead`) are one number.
    let limit = window_limit(state, limit);
    // Before the fetch, not after, like the mark below: see `ReadAhead::remember`.
    let fetch_began = std::time::Instant::now();
    // `#210 voice-catchup-speed`. Every read of a channel's newest messages is the newest copy of
    // it, whoever asked: kept raw, before anything below prepares it, with the pushes that arrived
    // while it ran applied again on top. See `crate::recent`.
    let mark = state.live.recent().mark(&channel.id);
    let read = crate::recent_view::read(state, &channel.id, limit).await?;
    state.live.recent().record_as_of(
        &channel.id,
        &read.messages,
        limit,
        mark,
        read.as_of.as_deref(),
    );
    let mut messages = read.messages;
    stamp(state, &mut messages);
    // `#196 auto-read-noise`. Decided HERE, on every read, from the text as it now stands — never
    // recorded — so every caller of this function (the digest, the to-do list, a lookup by id,
    // bankruptcy) sees the same answer the page does. See `crate::noise`.
    crate::noise::filter_for(state, &channel.id)
        .await
        .mark(&mut messages);
    // `#191 read-aloud-first-lag`: a speech preparation in the next few seconds reads these
    // rather than fetching the same window again.
    state
        .read_ahead
        .remember(&channel.id, None, &messages, fetch_began);
    Ok(Window {
        channel,
        messages,
        limit,
        as_of: read.as_of,
    })
}

/// How many messages [`messages`] asks the chat backend for, given what the caller asked for.
///
/// Clamped to Discord's own ceiling, not merely to the configured one. Nothing in
/// `Config::from_toml_and_env` stops an operator writing `max_fetch_limit = 200`, but both the
/// real client and the fake clamp the request to 100 — so comparing against the unclamped
/// number would report a FULL 100-message window as the whole channel.
///
/// Public so speech preparation can tell whether another preparation's window was at least as
/// wide as the one it would have fetched itself (`crate::read_ahead::messages_for_speech`).
#[must_use]
pub fn window_limit(state: &AppState, limit: Option<u16>) -> u16 {
    state
        .effective_limit(limit)
        .min(crate::discord::http::DISCORD_MAX_LIMIT)
}

/// One message in full, by id, within the recent window. Read scope.
///
/// # Errors
///
/// [`OpError::UnknownChannel`], [`OpError::UnknownMessage`] when the id is not in the fetched
/// window, or [`OpError::Chat`].
pub async fn message_by_id(
    state: &AppState,
    channel_id: &str,
    message_id: &str,
    limit: Option<u16>,
) -> Result<(ChannelInfo, Message), OpError> {
    let window = messages(state, channel_id, limit).await?;
    let wanted = MessageId(message_id.to_owned());
    let message = window
        .messages
        .into_iter()
        .find(|m| m.id == wanted)
        .ok_or(OpError::UnknownMessage)?;
    Ok((window.channel, message))
}

/// Look up a message within an explicitly selected thread, or the ordinary channel window.
///
/// The parent channel allowlist is checked before the backend verifies thread membership.
pub async fn message_by_id_scoped(
    state: &AppState,
    channel_id: &str,
    message_id: &str,
    limit: Option<u16>,
    thread_id: Option<&str>,
) -> Result<(ChannelInfo, Message), OpError> {
    let Some(thread_id) = thread_id else {
        return message_by_id(state, channel_id, message_id, limit).await;
    };
    let channel = allowed(state, channel_id).await?;
    validate_thread_id(thread_id)?;
    let mut message = state
        .chat
        .fetch_thread_message(&channel.id, thread_id, &MessageId(message_id.to_owned()))
        .await?;
    stamp(state, std::slice::from_mut(&mut message));
    crate::noise::filter_for(state, &channel.id)
        .await
        .mark(std::slice::from_mut(&mut message));
    Ok((channel, message))
}

/// Browse a channel or its threads through the provider's own pagination contract.
///
/// `request.after` is the PAGE-VISIBLE forward cursor, vibe-talk's envelope around either the
/// backend's own cursor or a generic position; see [`crate::timeline_forward`]. It is unwrapped
/// here, so a backend only ever sees a cursor it issued, and every newest page and delta leaves
/// with a fresh envelope in `next_after`. A `before` page leaves without one: it says nothing about
/// the newest end. `#203 incremental-refresh`.
///
/// # Errors
///
/// [`OpError::UnknownChannel`]; [`OpError::CursorMismatch`] for a forward cursor issued for any
/// other channel, view or thread, checked before anything is read; [`OpError::Chat`], including
/// [`ChatError::CursorExpired`] for one the backend can no longer answer for.
pub async fn timeline(
    state: &AppState,
    channel_id: &str,
    request: &crate::threads::TimelineRequest,
) -> Result<(ChannelInfo, crate::threads::TimelinePage), OpError> {
    use crate::timeline_forward::{self as forward, Forward};
    let channel = allowed(state, channel_id).await?;
    request.one_direction()?;
    let thread = request.thread_id.as_deref();
    let resumed = request
        .after
        .as_deref()
        .map(|raw| forward::decode(raw, &channel.id, request.view, thread))
        .transpose()
        .map_err(|_| OpError::CursorMismatch)?;
    let mut backend = request.clone();
    // Before the fetch, not after: see `ReadAhead::remember`.
    let fetch_began = std::time::Instant::now();
    let (mut page, position) = match resumed {
        Some(Forward::Native(own)) => {
            backend.after = Some(own);
            (
                state.chat.fetch_timeline(&channel.id, &backend).await?,
                None,
            )
        }
        Some(Forward::Generic(since)) => {
            backend.after = None;
            let read = state
                .chat
                .fetch_timeline_since(&channel.id, &backend, &since)
                .await?;
            (read.page, Some(read.position))
        }
        None => (state.chat.fetch_timeline(&channel.id, request).await?, None),
    };
    page.next_after = if request.before.is_some() {
        None
    } else {
        let next = match (page.next_after.take(), position) {
            (Some(own), _) => Forward::Native(own),
            (None, Some(position)) => Forward::Generic(position),
            (None, None) => Forward::Generic(forward::position_of(&page, request.view)),
        };
        Some(forward::encode(&channel.id, request.view, thread, &next)?)
    };
    // One filter for the whole page, thread roots included: a root that is itself a placeholder
    // is drawn as one, in the thread list as much as in the conversation.
    let noise = crate::noise::filter_for(state, &channel.id).await;
    stamp(state, &mut page.messages);
    noise.mark(&mut page.messages);
    for summary in page.threads.iter_mut().chain(page.thread.iter_mut()) {
        if let Some(root) = &mut summary.root {
            stamp(state, std::slice::from_mut(root));
            noise.mark(std::slice::from_mut(root));
            state.read_ahead.remember(
                &channel.id,
                Some(&summary.id),
                std::slice::from_ref(root),
                fetch_began,
            );
        }
    }
    // `#191 read-aloud-first-lag`: what the page is about to draw is what it will ask to hear.
    // A delta carries only what changed, which is exactly what has to replace an older copy.
    state
        .read_ahead
        .remember(&channel.id, thread, &page.messages, fetch_began);
    for gone in page.delta.iter().flat_map(|delta| &delta.deleted) {
        state.read_ahead.forget(&channel.id, gone);
    }
    Ok((channel, page))
}

/// One speakable line per recent message, and what was left out of it.
#[derive(Clone, Debug)]
pub struct Digest {
    /// Channel that was read.
    pub channel: ChannelInfo,
    /// One line per message, oldest first — noise excluded.
    pub entries: Vec<DigestEntry>,
    /// [`Window::is_whole_channel`]: true when the WINDOW was the entire channel, false when it
    /// was a window onto something larger. A caller that renders a count without consulting it is
    /// reporting the fetch size as a channel total.
    pub complete: bool,
    /// How many messages in the window the owner's noise rules call read, and so have no line.
    /// `#196 auto-read-noise`. Reported rather than silently dropped, so a caller can say "and
    /// three placeholders" instead of making the channel sound quieter than it is.
    pub noise: usize,
    /// The oldest message the window reached, placeholder or not: where a step back begins.
    ///
    /// Not the first entry. When every message in the window is a placeholder there IS no entry,
    /// and a digest with no cursor leaves a caller no way past them to the older messages behind;
    /// when only the oldest is one, stepping back from the first entry hands it over again.
    pub oldest: Option<MessageId>,
    /// [`Window::as_of`]: when set, the provider's copy was complete only up to that instant.
    pub as_of: Option<String>,
}

/// One speakable line per recent message. Read scope.
///
/// A message the owner's noise rules call read gets no line: a digest is what the agent reads to
/// find out what is there, and nine `Working…` lines out of fifty are exactly the noise the owner
/// asked to stop hearing. [`Digest::noise`] says how many were left out.
///
/// # Errors
///
/// As [`messages`].
pub async fn digest(
    state: &AppState,
    channel_id: &str,
    limit: Option<u16>,
    width: Option<u16>,
) -> Result<Digest, OpError> {
    let window = messages(state, channel_id, limit).await?;
    let complete = window.is_whole_channel();
    let width = usize::from(width.unwrap_or(0));
    let width = if width == 0 {
        DEFAULT_SUMMARY_CHARS
    } else {
        width
    };
    let oldest = window.messages.first().map(|m| m.id.clone());
    let (noise, kept): (Vec<Message>, Vec<Message>) =
        window.messages.into_iter().partition(|m| m.noise);
    Ok(Digest {
        channel: window.channel,
        entries: summary::digest(&kept, width),
        complete,
        noise: noise.len(),
        oldest,
        as_of: window.as_of,
    })
}

/// How far back [`activity`] reads when the caller does not say: one day.
pub const DEFAULT_ACTIVITY_HOURS: u64 = 24;

/// How many of each channel's most recent lines [`activity`] carries when the caller does not say.
pub const DEFAULT_ACTIVITY_LINES: u16 = 8;

/// The most lines per channel [`activity`] will carry, whatever the caller asks for.
pub const MAX_ACTIVITY_LINES: u16 = 30;

/// What [`activity`] should cover. Every field but the clock is optional.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActivityRequest {
    /// Hours back from `now_ms`. Absent means [`DEFAULT_ACTIVITY_HOURS`]; outside
    /// `1..=MAX_WINDOW_HOURS` it is CLAMPED, and [`Activity::asked_hours`] says so.
    pub hours: Option<u64>,
    /// Lines per channel. Absent means [`DEFAULT_ACTIVITY_LINES`]; clamped to
    /// `1..=MAX_ACTIVITY_LINES`.
    pub per_channel_limit: Option<u16>,
    /// The clock the window is resolved against: [`now_ms`] in production, a fixed instant in a
    /// test.
    pub now_ms: i64,
}

/// Every listed channel's recent activity, read in one call. `#190 voice-agent-tools`.
#[derive(Debug)]
pub struct Activity {
    /// The window actually read, in hours.
    pub hours: u64,
    /// What the caller asked for, when that was outside `1..=MAX_WINDOW_HOURS` and so was clamped
    /// to [`Activity::hours`]. Said rather than silently corrected.
    pub asked_hours: Option<u64>,
    /// The clock reading the window was resolved against.
    pub now_ms: i64,
    /// Where the window starts: `now_ms` minus `hours`. Inclusive.
    pub since_ms: i64,
    /// Lines per channel, after clamping.
    pub per_channel_limit: u16,
    /// One entry per listed channel, in list order, each read or not on its own.
    pub channels: Vec<ChannelActivity>,
}

/// One channel's part of an [`Activity`].
#[derive(Debug)]
pub struct ChannelActivity {
    /// The channel, wearing the operator's own name.
    pub channel: ChannelInfo,
    /// What was read, or why it could not be. One channel failing does not fail the others.
    pub read: Result<WindowActivity, OpError>,
    /// How current `read` is. Always [`Currency::Current`] beside an error.
    pub currency: Currency,
    /// The provider's `as_of`, RFC 3339, for the read that produced `read`, when the provider said
    /// that read was complete only up to then: true however recently the read itself ran. `#208
    /// persistent-cache`; see [`crate::recent::Held::as_of`].
    pub as_of: Option<String>,
}

/// How current one channel's part of an [`Activity`] is. `#210 voice-catchup-speed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Currency {
    /// Read from the chat service during this call, or recently enough to stand for now.
    Current,
    /// This server's earlier copy, because the channel was not read again in time.
    Held {
        /// How long ago this server's read of the copy began. A copy the provider dated with
        /// `as_of` is older than that: [`ChannelActivity::as_of`] says how much.
        age: std::time::Duration,
        /// Why it was not read again: the code of the newest read's failure, or `None` when that
        /// read is still running.
        failed: Option<&'static str>,
    },
}

/// What one channel holds inside an activity window.
#[derive(Clone, Debug)]
pub struct WindowActivity {
    /// The most recent lines inside the window, at most the request's per-channel limit, OLDEST
    /// FIRST like every digest. Placeholders excluded.
    pub entries: Vec<DigestEntry>,
    /// Messages inside the window that are not placeholders — the number to say aloud.
    pub counted: usize,
    /// Whether `counted` is the window's whole count. False means the read stopped before it
    /// reached the start of the window, so `counted` is a LOWER BOUND, exactly as a capped
    /// `count_messages` is.
    pub complete: bool,
    /// Placeholders inside the window that the owner's noise rules call read. `#196
    /// auto-read-noise`.
    pub noise: usize,
    /// The newest message this read saw that is not a placeholder, inside the window or not, so
    /// "what was the last thing said, and where?" has an answer even after a quiet day.
    pub newest: Option<Newest>,
}

/// A channel's newest message and the instant it was created.
#[derive(Clone, Debug)]
pub struct Newest {
    /// When it was created, in milliseconds since the Unix epoch, from its id where possible.
    pub at_ms: i64,
    /// The message, as a digest line.
    pub entry: DigestEntry,
}

/// How many channels an [`Activity`] answered current, from a held copy, or not at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivitySources {
    /// Read during the call, or recently enough to stand for now.
    pub current: usize,
    /// Answered from this server's earlier copy.
    pub held: usize,
    /// Not answered: nothing held, and the read failed or did not finish in time.
    pub unread: usize,
}

impl Activity {
    /// The newest message any channel holds, as far as these reads saw, and its channel.
    ///
    /// A tie goes to the channel listed first, so the answer does not depend on read order.
    #[must_use]
    pub fn newest(&self) -> Option<(&ChannelInfo, &Newest)> {
        self.channels
            .iter()
            .filter_map(|c| {
                let read = c.read.as_ref().ok()?;
                read.newest.as_ref().map(|newest| (&c.channel, newest))
            })
            .reduce(|best, next| {
                if next.1.at_ms > best.1.at_ms {
                    next
                } else {
                    best
                }
            })
    }

    /// Whether an instant falls inside this window.
    #[must_use]
    pub fn covers(&self, at_ms: i64) -> bool {
        at_ms >= self.since_ms
    }

    /// How the channels were answered, for the log line. `#210 voice-catchup-speed`.
    #[must_use]
    pub fn sources(&self) -> ActivitySources {
        let mut sources = ActivitySources::default();
        for part in &self.channels {
            match (&part.read, part.currency) {
                (Err(_), _) => sources.unread += 1,
                (Ok(_), Currency::Current) => sources.current += 1,
                (Ok(_), Currency::Held { .. }) => sources.held += 1,
            }
        }
        sources
    }
}

/// How many messages [`activity`] reads of each channel to show `lines` of them: as deep as a
/// default read goes, and always deep enough to say whether there is more than the lines shown,
/// clamped by [`window_limit`] as [`messages`] clamps it, so a held window's recorded size and
/// this scan compare like for like.
#[must_use]
pub fn activity_scan(state: &AppState, lines: u16) -> u16 {
    window_limit(
        state,
        Some(state.config.chat.default_fetch_limit.max(lines + 1)),
    )
}

/// Catch up on every listed channel in one call. Read scope. `#190 voice-agent-tools`.
///
/// **Why a batching operation exists at all.** The tools were thin on purpose, so a model composes
/// rather than consumes pre-chewed answers. But a voice agent pays for composition in ROUNDS — each
/// tool call is a turn of the speech model, and a runtime that speaks a filler line while a tool
/// runs gives up after a round or two with nothing but filler in it. "What happened in the last
/// day?" across three channels was list, clock, then one read per channel: up to five rounds, any
/// of which could be the one that fails. This answers it in one, with no required argument.
///
/// **It answers by a deadline, not when the slowest read ends.** `#210 voice-catchup-speed`. Each
/// channel is one read of its most recent messages — the same read, noise marks and spoken times
/// every other read gets — filtered to the window, started for every channel at once and shared
/// with any read of that channel already running (see [`crate::recent`]). A channel read again
/// within [`crate::recent::Timing::deadline`] is current; one that was not is answered from this
/// server's held copy and says how old it is ([`Currency::Held`]); one with nothing held waits up
/// to [`crate::recent::Timing::cold_deadline`] and is then [`OpError::NotReadInTime`]. A window
/// young enough to reuse ([`crate::recent::reuse_for`]) is served without a read at all.
///
/// # Errors
///
/// The first channel's error when EVERY channel failed outright, since an answer made only of
/// failures is a failure. A channel still being read does not count as failed, and an empty list is
/// not an error: there is simply nothing to report.
pub async fn activity(state: &AppState, request: ActivityRequest) -> Result<Activity, OpError> {
    let asked = request.hours.unwrap_or(DEFAULT_ACTIVITY_HOURS);
    let hours = asked.clamp(1, MAX_WINDOW_HOURS);
    let since_ms = hours_before(hours, request.now_ms)?;
    let lines = request
        .per_channel_limit
        .unwrap_or(DEFAULT_ACTIVITY_LINES)
        .clamp(1, MAX_ACTIVITY_LINES);
    let scan = activity_scan(state, lines);
    let channels = channels(state).await;
    let recent = state.live.recent();
    let timing = recent.timing();
    let began = tokio::time::Instant::now();

    // Every read starts before any is waited for, so their waits overlap rather than add up.
    let waits: Vec<_> = channels
        .iter()
        .map(|channel| {
            let reuse = crate::recent::reuse_for(state, &channel.id);
            (!recent.reusable(&channel.id, scan, reuse))
                .then(|| crate::recent::refresh(state, &channel.id, scan))
        })
        .collect();
    for (channel, wait) in channels.iter().zip(waits) {
        let Some(mut done) = wait else { continue };
        let budget = if recent.holds(&channel.id) {
            timing.deadline
        } else {
            timing.cold_deadline
        };
        // A deadline passing is not a failure here: what was not read in time is answered from
        // what is held, below. Nor is a finished read's sender being gone.
        let _ = tokio::time::timeout_at(began + budget, done.wait_for(|over| *over)).await;
    }

    let mut reads: Vec<ChannelActivity> = Vec::with_capacity(channels.len());
    for channel in channels {
        let reuse = crate::recent::reuse_for(state, &channel.id);
        reads.push(channel_activity(state, channel, began, reuse, lines, since_ms).await);
    }
    if !reads.is_empty()
        && reads
            .iter()
            .all(|c| matches!(c.read, Err(ref error) if !matches!(error, OpError::NotReadInTime)))
    {
        // Every read failed, so this returns the first channel's reason.
        reads.swap_remove(0).read?;
    }
    Ok(Activity {
        hours,
        asked_hours: (asked != hours).then_some(asked),
        now_ms: request.now_ms,
        since_ms,
        per_channel_limit: lines,
        channels: reads,
    })
}

/// One channel's part of [`activity`], from what this server holds once the waiting is over.
async fn channel_activity(
    state: &AppState,
    channel: ChannelInfo,
    began: tokio::time::Instant,
    reuse: std::time::Duration,
    lines: u16,
    since_ms: i64,
) -> ChannelActivity {
    let known = state.live.recent().snapshot(&channel.id);
    let Some(held) = known.held else {
        let error = match known.failure {
            Some(error) if !known.refreshing => OpError::Chat(error),
            _ => OpError::NotReadInTime,
        };
        return ChannelActivity {
            channel,
            read: Err(error),
            currency: Currency::Current,
            as_of: None,
        };
    };
    let currency = if held.read_finished >= began || held.since_finished() <= reuse {
        Currency::Current
    } else {
        Currency::Held {
            age: held.read_began.elapsed(),
            // Said even while another read is running: "the last read failed" is the more useful
            // of the two things that are true.
            failed: known.failure.map(|error| OpError::Chat(error).code()),
        }
    };
    let mut messages = held.messages;
    stamp(state, &mut messages);
    crate::noise::filter_for(state, &channel.id)
        .await
        .mark(&mut messages);
    ChannelActivity {
        channel,
        read: Ok(window_activity(messages, held.whole, lines, since_ms)),
        currency,
        as_of: held.as_of,
    }
}

/// What one channel's window holds inside an activity span. `whole` says the messages are the
/// entire channel.
fn window_activity(
    messages: Vec<Message>,
    whole: bool,
    lines: u16,
    since_ms: i64,
) -> WindowActivity {
    // The count is exact only when the read reached past the start of the window: either the
    // channel ran out, or its oldest fetched message is already older than the window. Otherwise
    // there may be more inside the window than this one fetch reached.
    let complete = whole
        || messages
            .first()
            .and_then(message_ms)
            .is_some_and(|ms| ms < since_ms);
    let newest = messages.iter().rev().find(|m| !m.noise).and_then(|m| {
        message_ms(m).map(|at_ms| Newest {
            at_ms,
            entry: summary::digest_entry(m, DEFAULT_SUMMARY_CHARS),
        })
    });
    // A message whose time cannot be read is kept, as a time-range page keeps it: dropping it
    // would make the channel sound quieter than it is.
    let (noise, kept): (Vec<Message>, Vec<Message>) = messages
        .into_iter()
        .filter(|m| message_ms(m).is_none_or(|ms| ms >= since_ms))
        .partition(|m| m.noise);
    let skip = kept.len().saturating_sub(usize::from(lines));
    WindowActivity {
        entries: summary::digest(&kept[skip..], DEFAULT_SUMMARY_CHARS),
        counted: kept.len(),
        complete,
        noise: noise.len(),
        newest,
    }
}

/// The largest page this server will hand back in one step.
///
/// One less than Discord's own ceiling, and that is the whole point: the page is fetched with
/// `limit + 1` and the extra message is dropped, which is the only way to answer "is there more?"
/// exactly rather than by guessing from a full window. Allowing 100 would make the probe
/// impossible on the largest page and force `has_more` back into a guess.
pub const MAX_PAGE: u16 = crate::discord::http::DISCORD_MAX_LIMIT - 1;

/// What a caller wants one step of a walk to cover.
///
/// Two modes, and mixing them is an error rather than a precedence rule: `before` steps BACKWARDS
/// from a cursor (or from the newest message when absent), and `since`/`until` jump to a
/// half-open time span. A caller that supplied both would have no way to know which it got.
#[derive(Clone, Copy, Debug, Default)]
pub struct PageRequest<'a> {
    /// How many messages to return. Clamped by the configured ceiling and by [`MAX_PAGE`].
    pub limit: Option<u16>,
    /// Step back from this message id, exclusive. Hand back [`Page::next_before`].
    pub before: Option<&'a str>,
    /// Start of a time span, inclusive, as an ISO-8601 instant.
    pub since: Option<&'a str>,
    /// End of a time span, EXCLUSIVE, as an ISO-8601 instant. Requires `since`.
    pub until: Option<&'a str>,
    /// A span that starts this many hours before now, on this server's clock, and runs to now.
    /// Stands alone: refused with any of the three above. `#190 voice-agent-tools`.
    pub hours: Option<u64>,
}

/// The longest relative window any tool reads, in hours: one week.
pub const MAX_WINDOW_HOURS: u64 = 168;

/// Milliseconds per hour, for turning a relative window into an instant.
const HOUR_MS: i64 = 3_600_000;

/// This server's clock, in milliseconds since the Unix epoch.
///
/// The one reading a relative window is resolved against. `#190 voice-agent-tools`: a caller that
/// wants "the last 24 hours" should not have to learn the time first and do date arithmetic — at
/// the speed of a voice turn, every extra round is another chance to fail.
#[must_use]
pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// The instant `hours` before `now_ms`, for a span that runs to now.
///
/// # Errors
///
/// [`OpError::InvalidHours`] outside `1..=MAX_WINDOW_HOURS`.
pub fn hours_before(hours: u64, now_ms: i64) -> Result<i64, OpError> {
    if !(1..=MAX_WINDOW_HOURS).contains(&hours) {
        return Err(OpError::InvalidHours);
    }
    let span = i64::try_from(hours).map_err(|_| OpError::InvalidHours)? * HOUR_MS;
    Ok(now_ms - span)
}

/// One step of a walk through a channel, and everything needed to take the next one.
///
/// Every field here exists to stop a caller mistaking a page for the whole. The issue this comes
/// from was reported because a model asked "how many messages are in this channel?" and answered
/// **100** — the size of the page it had been handed, by a response that never said it was a page.
#[derive(Clone, Debug)]
pub struct Page {
    /// Channel that was read.
    pub channel: ChannelInfo,
    /// The messages, oldest first.
    pub messages: Vec<Message>,
    /// The page size actually used, after both ceilings.
    pub limit: u16,
    /// Whether messages exist beyond this page in the direction of travel.
    ///
    /// Decided by over-fetching one and dropping it, so it is a fact rather than an inference from
    /// a full window.
    pub has_more: bool,
    /// The `before` cursor for the next step back. Set only when [`Page::has_more`].
    pub next_before: Option<MessageId>,
    /// The `since` instant for the next step of a range walk. Set only when [`Page::has_more`].
    pub next_since: Option<String>,
}

impl Page {
    /// How many messages this page carries.
    #[must_use]
    pub fn returned(&self) -> usize {
        self.messages.len()
    }
}

/// Parse a caller-supplied cursor into a message id.
fn cursor(value: Option<&str>) -> Result<Option<MessageId>, OpError> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(raw) => {
            let id = MessageId(raw.to_owned());
            if id.numeric().is_none() {
                return Err(OpError::InvalidCursor);
            }
            Ok(Some(id))
        }
    }
}

/// Whether an optional argument is absent or only whitespace, which every parser here treats alike.
fn blank(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim().is_empty())
}

fn instant(value: Option<&str>) -> Result<Option<i64>, OpError> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(raw) => crate::clock::instant_ms(raw)
            .map(Some)
            .ok_or(OpError::InvalidRange),
    }
}

/// One step of a walk: a page that knows it is a page. Read scope.
///
/// # Errors
///
/// [`OpError::UnknownChannel`], [`OpError::InvalidCursor`] for a cursor that is not a snowflake,
/// [`OpError::InvalidRange`] for an unparseable or contradictory span, [`OpError::InvalidHours`]
/// or [`OpError::HoursWithSpan`] for an unusable relative window, or [`OpError::Chat`].
pub async fn page(
    state: &AppState,
    channel_id: &str,
    request: PageRequest<'_>,
) -> Result<Page, OpError> {
    let channel = allowed(state, channel_id).await?;
    let before = cursor(request.before)?;
    let until = instant(request.until)?;
    let since = match request.hours {
        None => instant(request.since)?,
        // A blank `since` is absent, as everywhere else here: a model that sends `"since": ""`
        // beside `hours` asked for one window, not two.
        Some(_) if !blank(request.since) || until.is_some() || before.is_some() => {
            return Err(OpError::HoursWithSpan);
        }
        // From then to now is exactly a `since` walk with no end, so it is one.
        Some(hours) => Some(hours_before(hours, now_ms())?),
    };
    if (since.is_some() || until.is_some()) && before.is_some() {
        return Err(OpError::InvalidRange);
    }
    if until.is_some() && since.is_none() {
        // An open-ended "everything before this instant" is a `before` walk wearing a time
        // costume, and answering it would need a cursor this server has no way to place a page
        // relative to. Say so instead of returning a span that is not the one asked for.
        return Err(OpError::InvalidRange);
    }
    if let (Some(from), Some(to)) = (since, until) {
        if to <= from {
            return Err(OpError::InvalidRange);
        }
    }

    let limit = state
        .effective_limit(request.limit)
        .min(MAX_PAGE)
        .min(state.config.chat.max_fetch_limit);
    // The over-fetch. One extra message answers "is there more" outright; the alternative is a
    // second round trip or the guess that a full window means more exists.
    let probe = limit + 1;

    // Before the fetch, not after: see `ReadAhead::remember`.
    let fetch_began = std::time::Instant::now();
    let (mut messages, forward) = match since {
        Some(from) => {
            // Discord's `after` is EXCLUSIVE, and the `since` edge is inclusive, so the cursor
            // goes one millisecond earlier. Off by one here silently drops a message that sits
            // exactly on the boundary — the edge the issue asks be tested.
            let after = MessageId::at_time_ms(from - 1);
            let mut fetched = state
                .chat
                .fetch_page(&channel.id, probe, None, Some(&after))
                .await?;
            // Both edges applied to the message's own creation instant, because the boundary
            // cursor is only millisecond-accurate: the extra millisecond it lets through at the
            // bottom is trimmed here, and a message exactly at `until` is deterministically OUT.
            fetched.retain(|m| {
                message_ms(m).is_none_or(|ms| ms >= from && until.is_none_or(|to| ms < to))
            });
            (fetched, true)
        }
        None => {
            let fetched = state
                .chat
                .fetch_page(&channel.id, probe, before.as_ref(), None)
                .await?;
            (fetched, false)
        }
    };

    let has_more = messages.len() > usize::from(limit);
    if has_more {
        if forward {
            // Walking forward, the extra one is the NEWEST; drop it from that end.
            messages.truncate(usize::from(limit));
        } else {
            let excess = messages.len() - usize::from(limit);
            messages.drain(..excess);
        }
    }
    stamp(state, &mut messages);
    // Marked, not dropped: this route is also what the page draws the channel from, and the page
    // shows a placeholder dimmed rather than pretending it is not there. The agent's `read_page`
    // tool is what leaves them out; see `mcp::protocol::run_page`.
    crate::noise::filter_for(state, &channel.id)
        .await
        .mark(&mut messages);
    // `#191 read-aloud-first-lag`: the page draws the channel from this, and then asks to hear it.
    state
        .read_ahead
        .remember(&channel.id, None, &messages, fetch_began);

    let (next_before, next_since) = if !has_more {
        (None, None)
    } else if forward {
        // Resume one millisecond after the newest message returned, so the next step covers the
        // rest of the span without repeating this one's last message.
        let next = messages
            .last()
            .and_then(message_ms)
            .map(|ms| crate::clock::iso_from_ms(ms + 1));
        (None, next)
    } else {
        (messages.first().map(|m| m.id.clone()), None)
    };

    Ok(Page {
        channel,
        messages,
        limit,
        has_more,
        next_before,
        next_since,
    })
}

/// When a message was created, from its snowflake, falling back to its reported timestamp.
///
/// The snowflake is authoritative because it is what Discord's own `before`/`after` compare, so a
/// range filtered by anything else could disagree with the page it was fetched from.
fn message_ms(message: &Message) -> Option<i64> {
    message
        .id
        .created_at_ms()
        .or_else(|| crate::clock::instant_ms(&message.timestamp))
}

/// A bounded, honest answer to "how many messages are in there?".
///
/// Discord publishes no message count for a guild text channel, so a promised total would be
/// either slow or invented. This walks backwards until the channel runs out or the cap stops it,
/// and says which happened.
#[derive(Clone, Debug)]
pub struct MessageCount {
    /// Channel that was counted.
    pub channel: ChannelInfo,
    /// How many messages were actually seen.
    pub counted: usize,
    /// Whether the cap stopped the walk, making `counted` a LOWER BOUND rather than a total.
    pub at_least: bool,
    /// The ceiling that applied.
    pub cap: u32,
    /// Oldest message reached, so a caller can carry on from there if it wants to.
    pub oldest_seen: Option<MessageId>,
    /// Newest message the walk started from.
    pub newest_seen: Option<MessageId>,
    /// Messages the walk passed that the owner's noise rules call read, and so did NOT count.
    /// `#196 auto-read-noise`.
    pub noise: usize,
}

/// Count the messages in a channel, up to a cap, optionally only since an instant. Read scope.
///
/// A message the owner's noise rules call read is not counted — "how many messages since this
/// morning" asked of a channel where a third are `Working…` placeholders should not answer with
/// the placeholders — and [`MessageCount::noise`] says how many were passed over. The cap bounds
/// what the walk READS, placeholders included, because it is a ceiling on requests: a channel of
/// nothing but placeholders must not be walked to its first message.
///
/// # Errors
///
/// [`OpError::UnknownChannel`], [`OpError::InvalidRange`] for an unparseable `since`, or
/// [`OpError::Chat`].
pub async fn count(
    state: &AppState,
    channel_id: &str,
    since: Option<&str>,
    cap: Option<u32>,
) -> Result<MessageCount, OpError> {
    count_within(state, channel_id, since, None, cap).await
}

/// [`count`], optionally over the last `hours` on this server's clock instead of since an instant.
/// Read scope. `#190 voice-agent-tools`.
///
/// # Errors
///
/// As [`count`], plus [`OpError::InvalidHours`] for an unusable window and
/// [`OpError::HoursWithSpan`] when both `hours` and `since` are given.
pub async fn count_within(
    state: &AppState,
    channel_id: &str,
    since: Option<&str>,
    hours: Option<u64>,
    cap: Option<u32>,
) -> Result<MessageCount, OpError> {
    let channel = allowed(state, channel_id).await?;
    let since = match hours {
        None => instant(since)?,
        Some(_) if !blank(since) => return Err(OpError::HoursWithSpan),
        Some(hours) => Some(hours_before(hours, now_ms())?),
    };
    let cap = state.effective_count_cap(cap);

    let stride = crate::discord::http::DISCORD_MAX_LIMIT;
    let mut counted: usize = 0;
    let mut at_least = false;
    let mut oldest_seen: Option<MessageId> = None;
    let mut newest_seen: Option<MessageId> = None;
    let mut before: Option<MessageId> = None;
    let mut noise: usize = 0;
    let filter = crate::noise::filter_for(state, &channel.id).await;

    loop {
        let batch = state
            .chat
            .fetch_page(&channel.id, stride, before.as_ref(), None)
            .await?;
        if batch.is_empty() {
            break;
        }
        if newest_seen.is_none() {
            newest_seen = batch.last().map(|m| m.id.clone());
        }
        // Oldest first, so the front of the batch is the far end of the walk.
        let oldest = batch.first().expect("non-empty").clone();
        let exhausted = batch.len() < usize::from(stride);

        let in_range: Vec<&Message> = match since {
            None => batch.iter().collect(),
            Some(from) => batch
                .iter()
                .filter(|m| message_ms(m).is_none_or(|ms| ms >= from))
                .collect(),
        };
        let kept = in_range.len();
        let placeholders = in_range.iter().filter(|m| filter.is_noise(m)).count();
        counted += kept - placeholders;
        noise += placeholders;
        oldest_seen = Some(oldest.id.clone());

        // Stop as soon as the walk has passed the `since` boundary: everything older is outside
        // the question, and continuing would spend requests to count nothing.
        let passed_since = since.is_some() && kept < batch.len();
        if passed_since || exhausted {
            break;
        }
        if counted + noise >= usize::try_from(cap).unwrap_or(usize::MAX) {
            at_least = true;
            break;
        }
        before = Some(oldest.id);
    }

    Ok(MessageCount {
        channel,
        counted,
        at_least,
        cap,
        oldest_seen,
        newest_seen,
        noise,
    })
}

/// Semantic random access: describe a message, get that message. Read scope.
///
/// Returns the resolution and how many messages were searched, so a caller can honestly say "I
/// only looked at the last twenty".
///
/// # Errors
///
/// [`OpError::EmptyQuery`] for a blank query, plus the errors of [`messages`].
pub async fn resolve(
    state: &AppState,
    channel_id: &str,
    query: &str,
    limit: Option<u16>,
    max_alternatives: Option<u16>,
) -> Result<(ChannelInfo, Resolution, usize), OpError> {
    let info = allowed(state, channel_id).await?;
    if query.trim().is_empty() {
        return Err(OpError::EmptyQuery);
    }
    let limit = state.effective_limit(limit);
    // This path does NOT go through `messages` above — it fetches directly — so it needs its own
    // stamping call. `run_find` renders a timestamp for the best match and every alternative, and
    // without this those lines would be the only ones still speaking raw UTC.
    let mut messages = state.chat.fetch_recent(&info.id, limit).await?;
    stamp(state, &mut messages);
    // `#196 auto-read-noise`. A placeholder is never the message somebody is describing, and one
    // that happened to share a word with the description would be a confident wrong answer.
    let noise = crate::noise::filter_for(state, &info.id).await;
    messages.retain(|message| !noise.is_noise(message));
    let max_alternatives = usize::from(max_alternatives.unwrap_or(3)).min(10);
    let resolution = retrieval::resolve(state.ranker.as_ref(), &messages, query, max_alternatives);
    let searched = messages.len();
    Ok((info, resolution, searched))
}

/// Propose a post for confirmation, and post nothing. **Write scope.** `#34
/// voice-chat-write-confirm`: what MCP's `post_reply` does.
///
/// The same fences [`reply`] applies — allowlisted, writable, not empty — are applied NOW, so a
/// proposal the owner is asked to confirm is one that could actually be sent. Whatever proposal
/// was pending is superseded, and says so in the log.
///
/// # Errors
///
/// [`OpError::UnknownChannel`], [`OpError::ChannelNotWritable`], or [`OpError::Chat`] for an empty
/// message. Nothing is recorded when any of them applies.
pub async fn propose_reply(
    state: &AppState,
    channel_id: &str,
    text: &str,
    reply_to: Option<&str>,
) -> Result<(ChannelInfo, crate::post_gate::Proposal), OpError> {
    let info = allowed(state, channel_id).await?;
    if !info.writable {
        return Err(OpError::ChannelNotWritable);
    }
    if text.trim().is_empty() {
        return Err(OpError::Chat(ChatError::Refused(
            "message content is empty".to_owned(),
        )));
    }
    let (proposal, displaced) = state.post_gate.propose(crate::post_gate::Binding {
        channel_id: info.id.as_str().to_owned(),
        text: text.to_owned(),
        reply_to: reply_to.map(str::to_owned),
    });
    if let Some(crate::post_gate::Displaced { proposal: old, why }) = displaced {
        log_gate_step(
            if why == crate::post_gate::Refusal::Expired {
                "expired"
            } else {
                "superseded"
            },
            &old,
            None,
            None,
        );
    }
    log_gate_step("proposed", &proposal, None, None);
    Ok((info, proposal))
}

/// What a proposing caller is told: MCP's `post_reply` answer, and the `result` of
/// `POST /api/v1/post-proposals`, so a consumer that maps the tool onto the REST route hands its
/// model the same words.
///
/// It says NOT SENT first, gives the exact text to read back, and says the confirmation is not the
/// caller's to give. The handle is deliberately absent: a model has no route that could spend it,
/// and there is no reason to put a capability where it can be repeated.
#[must_use]
pub fn proposal_notice(info: &ChannelInfo, proposal: &crate::post_gate::Proposal) -> String {
    let seconds = proposal.expires_in_ms(tokio::time::Instant::now()) / 1000;
    let target = match &proposal.binding.reply_to {
        Some(message) => format!(" as a reply to message {message}"),
        None => String::new(),
    };
    format!(
        "NOT SENT. Proposal {serial} is waiting for the speaker's confirmation.\n\
         Channel: {channel} (id {id}){target}\n\
         Exact text: {text}\n\
         Read-back: {read_back}\n\
         Say that read-back to the speaker word for word, adding nothing before or after it, then \
         wait for the speaker. If they say yes, say only \"One moment.\" The voice page judges \
         their answer, sends it on a yes or a tap on Send, and then reports the result in a \
         message that starts with \"Voice page:\"; do not say it was sent before that. You cannot \
         send it yourself, and nothing you say or call confirms it. Do not call post_reply again \
         to confirm; calling it again replaces this proposal with a new one. If the speaker wants \
         different words, call post_reply with the new text. The proposal lapses in {seconds} \
         seconds.",
        serial = proposal.serial,
        channel = info.display_name(),
        id = info.id,
        text = proposal.binding.text,
        read_back = read_back(info.display_name(), &proposal.binding.text),
    )
}

/// The one sentence a voice agent reads a proposal back with: `To {channel}: {text}. Send it?`.
///
/// `#264 voice-send-approval`. The voice page takes a spoken yes only after exactly this, word for
/// word (its `isExactReadBack`), so the owner has heard what would be posted and where and nothing
/// else. Handing the model the finished sentence is what makes that easy to get right.
#[must_use]
pub fn read_back(channel: &str, text: &str) -> String {
    let text = text.trim();
    let stop = if text.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    };
    format!("To {channel}: {text}{stop} Send it?")
}

/// Who confirmed a proposal, as a commit names it. Recorded, not trusted: both are write-scope
/// application callers, and this says which path the owner's confirmation took.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmedBy {
    /// The owner tapped Send on the voice page.
    Ui,
    /// A separate, final user turn was taken as the speaker's yes — by the voice page's own
    /// judge (`#264 voice-send-approval`), or by a bridge keeping the same rules. Only meaningful
    /// after a read-back that finished normally: a yes following a barge-in, or reaching a model
    /// session that never saw the proposal, is not agreement to text the speaker heard. That rule
    /// is the confirming client's to keep; this server cannot observe it.
    SpeakerTurn,
}

impl ConfirmedBy {
    /// The word a log line carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::SpeakerTurn => "speaker_turn",
        }
    }
}

/// Why a commit posted nothing, or posted only part.
#[derive(Debug)]
pub enum CommitError {
    /// The gate refused the handle; nothing was sent.
    Refused(crate::post_gate::Refusal),
    /// The gate accepted it and the post itself then failed. The handle is spent either way: a
    /// failure after acceptance may have delivered part, and a retry must be a new proposal.
    Post(OpError),
}

/// Spend a proposal and post it, once. **Write scope**, and reachable only from the application
/// route — never from a tool.
///
/// # Errors
///
/// [`CommitError::Refused`] when the handle is unknown, expired, spent, superseded, or restated
/// differently; [`CommitError::Post`] when the post that followed failed.
pub async fn commit_proposal(
    state: &AppState,
    handle: &str,
    restated: &crate::post_gate::Binding,
    confirmed_by: ConfirmedBy,
) -> Result<(u64, Message, Vec<Message>), CommitError> {
    let proposal = match state.post_gate.take(handle, restated) {
        Ok(proposal) => proposal,
        Err((why, proposal)) => {
            match proposal {
                Some(proposal) => {
                    log_gate_step("refused", &proposal, Some(why.code()), Some(confirmed_by));
                }
                None => crate::access::post_gate(
                    "refused",
                    None,
                    Some(restated.channel_id.as_str()),
                    Some(restated.text.chars().count()),
                    Some(why.code()),
                    Some(confirmed_by.as_str()),
                ),
            }
            return Err(CommitError::Refused(why));
        }
    };
    let binding = &proposal.binding;
    match reply(
        state,
        &binding.channel_id,
        &binding.text,
        binding.reply_to.as_deref(),
    )
    .await
    {
        Ok((_channel, posted, parts)) => {
            log_gate_step("committed", &proposal, None, Some(confirmed_by));
            Ok((proposal.serial, posted, parts))
        }
        Err(error) => {
            log_gate_step("failed", &proposal, Some(error.code()), Some(confirmed_by));
            Err(CommitError::Post(error))
        }
    }
}

/// Withdraw a proposal without posting it. **Write scope.**
///
/// # Errors
///
/// The [`crate::post_gate::Refusal`] saying why there was nothing to cancel.
pub fn cancel_proposal(state: &AppState, handle: &str) -> Result<(), crate::post_gate::Refusal> {
    match state.post_gate.cancel(handle) {
        Ok(proposal) => {
            log_gate_step("cancelled", &proposal, None, None);
            Ok(())
        }
        Err(why) => {
            crate::access::post_gate("refused", None, None, None, Some(why.code()), None);
            Err(why)
        }
    }
}

fn log_gate_step(
    event: &str,
    proposal: &crate::post_gate::Proposal,
    reason: Option<&str>,
    confirmed_by: Option<ConfirmedBy>,
) {
    crate::access::post_gate(
        event,
        Some(proposal.serial),
        Some(proposal.binding.channel_id.as_str()),
        Some(proposal.binding.text.chars().count()),
        reason,
        confirmed_by.map(ConfirmedBy::as_str),
    );
}

/// Post a message as the bot. **Write scope**, and the only operation that speaks in the owner's
/// name.
///
/// Two fences, in this order: the channel must be in the allowlist at all, and it must
/// additionally be marked writable. A channel that is merely configured is not postable.
///
/// # Errors
///
/// [`OpError::UnknownChannel`], [`OpError::ChannelNotWritable`], or [`OpError::Chat`] — which
/// also covers the emptiness refusal made before any request is sent.
pub async fn reply(
    state: &AppState,
    channel_id: &str,
    text: &str,
    reply_to: Option<&str>,
) -> Result<(ChannelInfo, Message, Vec<Message>), OpError> {
    reply_scoped(state, channel_id, text, reply_to, None, None).await
}

/// Post to the channel or to one of its threads, preserving write policy and partial progress.
///
/// A post longer than the channel's provider accepts in one message
/// ([`ChatClient::post_limit`]) is cut into parts that each fit, and one that fits goes whole.
/// `#263 post-split-per-provider`: the parts of a post to the conversation itself go where the
/// provider continues a long message ([`ChatClient::continuation_thread`]) — the first in the
/// conversation and the rest in the thread under it, for Google Chat and Slack — and one after
/// another in the conversation for Discord. A post to a thread stays in that thread, every part.
///
/// `idempotency_key` names this post across attempts — `#195 send-resilience`. A caller that
/// lost an answer sends the same text again under the same key, and every part goes upstream
/// under the same per-part key it had the first time ([`part_key`]); a provider for which
/// [`ChatClient::supports_idempotent_posts`] holds then posts it at most once. After a 207 the
/// same holds for the unsent text, sent to the thread the 207 names, only where the 207 says
/// `resumable`; see [`unsent_remainder`].
///
/// [`ChatClient::supports_idempotent_posts`]: crate::chat::ChatClient::supports_idempotent_posts
/// [`ChatClient::post_limit`]: crate::chat::ChatClient::post_limit
/// [`ChatClient::continuation_thread`]: crate::chat::ChatClient::continuation_thread
pub async fn reply_scoped(
    state: &AppState,
    channel_id: &str,
    text: &str,
    reply_to: Option<&str>,
    thread_id: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<(ChannelInfo, Message, Vec<Message>), OpError> {
    let info = allowed(state, channel_id).await?;
    if !info.writable {
        return Err(OpError::ChannelNotWritable);
    }
    if let Some(thread) = thread_id {
        validate_thread_id(thread)?;
    }
    if let Some(key) = idempotency_key {
        validate_idempotency_key(key)?;
    }
    let reply_to = reply_to.map(|id| MessageId(id.to_owned()));

    // LONGER THAN THE PROVIDER ACCEPTS IS NOT A REFUSAL. A reply somebody typed on a phone came
    // back as an error and stayed stuck in the box; coding agents posting into the channel already
    // split their own long messages. See `crate::discord::split`. And it is the PROVIDER's limit,
    // not Discord's: cut at Discord's 2000 characters, a long message to Google Chat, which takes
    // sixteen times that, arrived as three. `#263 post-split-per-provider`.
    let limit = state.chat.post_limit(&info.id);
    let parts = crate::discord::split::split_within(text, &limit);
    if parts.is_empty() {
        return Err(OpError::Chat(ChatError::Refused(
            "message content is empty".to_owned(),
        )));
    }

    let keys = idempotency_key.map(|key| part_keys(key, &parts));
    // `#277 send-order`. This destination's turn, after every post to it that arrived first, held
    // from the first part to the last, so nothing lands between the parts of a long post. Waiting
    // for it is this request's own: a caller that goes away while it waits gives up its place, and
    // nothing of its post has been sent. See `crate::post_lanes`.
    let turn = state.post_lanes.turn(info.id.as_str(), thread_id).await;
    // Posting is not this request's own. axum drops a handler the moment its caller closes the
    // connection — the page stops waiting after `OUTGOING_TIMEOUT_MS`, signs out, or loses its
    // connection behind a proxy — and with it whatever it holds. Were the turn and the provider
    // call held here, the next post to this destination (the page's retry of this very message,
    // and then the one written after it) would go at once, while the provider was still working
    // on this one, and could land first. So a task of its own owns the turn and the provider
    // calls, and holds the destination until the provider has answered for the last part it tried
    // or the provider client's own request timeout has given up on it; a caller that has gone is
    // simply not told.
    let posting = Posting {
        info,
        text: text.to_owned(),
        parts,
        keys,
        reply_to,
        thread_id: thread_id.map(str::to_owned),
        limit,
    };
    match tokio::spawn(post_parts(state.clone(), posting, turn)).await {
        Ok(result) => result,
        Err(stopped) if stopped.is_panic() => std::panic::resume_unwind(stopped.into_panic()),
        // Only a runtime shutting down cancels it; whatever it had sent may have landed.
        Err(stopped) => Err(OpError::Chat(ChatError::Transport(format!(
            "the post was stopped before the provider answered: {stopped}"
        )))),
    }
}

/// One post, cut in parts and ready to go: everything [`post_parts`] needs, owned, so the task that
/// posts it can outlive the request that asked for it.
struct Posting {
    info: ChannelInfo,
    text: String,
    parts: Vec<String>,
    keys: Option<Vec<String>>,
    reply_to: Option<MessageId>,
    thread_id: Option<String>,
    limit: crate::chat::PostLimit,
}

/// Post every part of `posting` in order, holding the destination's `_turn` until the provider has
/// answered for the last part tried. The body of [`reply_scoped`], run as a task of its own.
async fn post_parts(
    state: AppState,
    posting: Posting,
    _turn: crate::post_lanes::Turn,
) -> Result<(ChannelInfo, Message, Vec<Message>), OpError> {
    let Posting {
        info,
        text,
        parts,
        keys,
        reply_to,
        thread_id,
        limit,
    } = posting;
    let thread_id = thread_id.as_deref();
    let mut posted: Vec<Message> = Vec::new();
    // Where the parts after the first go, for a post to the conversation itself: the thread under
    // the first part, where the provider continues a long message there. Learned from the first
    // part's own answer, so it is only ever a thread this post started or joined.
    let mut continuation: Option<String> = None;
    for (index, part) in parts.iter().enumerate() {
        // Only the FIRST part answers the message. Discord threads a reply from one message, and
        // pointing every part at the same parent would render as several separate answers to the
        // same thing rather than as one answer that ran long.
        let parent = if index == 0 { reply_to.as_ref() } else { None };
        let key = keys.as_ref().map(|keys| keys[index].as_str());
        let into = thread_id.or(continuation.as_deref());
        let sent = match into {
            Some(thread) => {
                state
                    .chat
                    .post_in_thread_keyed(&info.id, thread, part, parent, key)
                    .await
            }
            None => {
                state
                    .chat
                    .post_message_keyed(&info.id, part, parent, key)
                    .await
            }
        };
        match sent {
            Ok(mut one) => {
                stamp(&state, std::slice::from_mut(&mut one));
                // Remember that WE posted this, before anything can observe it. `#44 live-push`:
                // the poller will see it in a few seconds, and relaying it into the live
                // conversation would make the agent hear its own reply as new information.
                state.live.note_self_posted(&one.id);
                if index == 0 && thread_id.is_none() && parts.len() > 1 {
                    continuation = state.chat.continuation_thread(&info.id, &one);
                    if let (Some(thread), None) = (&continuation, &one.thread) {
                        // It roots that thread now, whether or not the provider's answer for a
                        // message that had no replies yet said so; the page draws it as one.
                        one.thread = Some(crate::threads::MessageThread {
                            id: thread.clone(),
                            root_message_id: Some(one.id.clone()),
                            is_root: true,
                            reply_count: None,
                            reply_count_exact: false,
                        });
                    }
                }
                posted.push(one);
            }
            Err(cause) => {
                // PART OF IT IS ALREADY IN THE CHANNEL and cannot be unsent. Reporting a plain
                // failure here would be a lie the reader acts on: they would send the whole thing
                // again and post the first half twice. So the caller is told exactly how much
                // landed AND handed back the text that did not, which is the only copy of it left
                // once their draft is cleared — and where it belongs, which after the first part
                // of a continued post is that part's thread rather than the conversation.
                let replying = reply_to.is_some() && continuation.is_none();
                let (unsent, resumable) = unsent_remainder(&text, &parts, index, replying, &limit);
                return Err(OpError::PartiallyPosted {
                    posted: posted.len(),
                    unsent,
                    retryable: cause.is_transient(),
                    resumable,
                    thread_id: into.map(str::to_owned),
                    uncertain: cause.may_have_delivered(),
                    cause: cause.to_string(),
                });
            }
        }
    }
    let first = posted.first().cloned().expect("at least one part posted");
    Ok((info, first, posted))
}

/// The longest idempotency key a caller may send. Short enough that a part's key, which adds a
/// fingerprint and a count, stays inside the 128 characters a bridge nonce may have.
pub const IDEMPOTENCY_KEY_MAX_CHARS: usize = 64;

fn validate_idempotency_key(key: &str) -> Result<(), OpError> {
    if key.is_empty()
        || key.len() > IDEMPOTENCY_KEY_MAX_CHARS
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ChatError::Refused(format!(
            "idempotency_key must be 1 to {IDEMPOTENCY_KEY_MAX_CHARS} letters, digits, '-' or '_'"
        ))
        .into());
    }
    Ok(())
}

/// The key ONE part of a split post travels upstream with: the caller's key, bound to that part's
/// own text.
///
/// Bound to the TEXT rather than to the part's position, because the position is not stable
/// across attempts. A 207 hands the caller back the unsent remainder, and its retry sends only
/// that — so the part that failed ambiguously, the one whose arrival nobody knows, is part 0 of
/// the retry where it was part 2 of the first attempt. Where splitting the remainder cuts that
/// part where the first attempt did its text is the same, so its key is too, and the provider
/// that may already hold it is asked for the same post rather than a new one. That "where" is
/// checked, not assumed: [`unsent_remainder`]. The same binding means a key reused for different words can
/// never be answered with a post of other words.
///
/// `earlier_twins` counts identical parts before this one in the same request, so a post that
/// repeats a whole part posts it twice, as written, rather than collapsing into one.
///
/// FNV-1a rather than a cryptographic hash: this distinguishes a caller's own parts from one
/// another and needs no secrecy — the key it extends is already the caller's to choose.
#[must_use]
pub fn part_key(key: &str, part: &str, earlier_twins: usize) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let fingerprint = part.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    });
    format!("{key}.{fingerprint:016x}.{earlier_twins}")
}

/// The upstream key of every part of one post, in order: [`part_key`] with each part's count of
/// identical parts before it.
#[must_use]
pub fn part_keys(key: &str, parts: &[String]) -> Vec<String> {
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let earlier_twins = parts[..index].iter().filter(|other| *other == part).count();
            part_key(key, part, earlier_twins)
        })
        .collect()
}

/// What a 207 hands back once part `index` of `parts` (split from `text`) failed: the text the
/// caller should send if it sends anything again, and whether sending it — with the same key,
/// thread and reply target — provably asks again for the part that failed, as the same request,
/// and for nothing already posted.
///
/// `#195 send-resilience`. The page retries a 207 on its own only when this says so, and tells
/// the reader a retry "cannot post it twice"; both rest on the part that failed — the one that
/// may have landed with its answer lost — going upstream again as the same words under the same
/// key. That is checked here rather than assumed, because the splitter does not promise it:
///
/// - **Nothing posted.** The remainder is the caller's own `text`, byte for byte, not the parts
///   joined back up. Sending it again is the same request, so it splits and is keyed exactly as
///   it was. (Joined parts carry any fence the splitter closed and reopened, and splitting THOSE
///   cuts somewhere else.)
/// - **Some posted.** The joined remainder has to split so that its FIRST part is exactly the
///   part that failed. It need not when a fence straddled a cut: the closing and reopening
///   markers this server added become ordinary text the second time, and the cut moves. The
///   parts after it were never attempted, so where they are cut posts nothing twice.
/// - **No part of the retry may repeat a part already posted.** Twins are counted afresh in
///   each request, so such a part would be keyed as its posted twin and silently collapse into
///   it — and the part that failed, if it is one, would not be keyed as it was.
/// - **A reply.** Only the first part of the first attempt answered the message; the remainder
///   sent again with the reply target makes its first part a different request. Not so for a
///   remainder continuing in the first part's thread (`#263 post-split-per-provider`): that goes
///   to the thread the 207 names, without the reply target, exactly as the part that failed did,
///   so the caller passes `replying` false for it.
///
/// `limit` is the one the post was cut to, so the remainder is cut the same way.
///
/// False here leaves the 207 to the reader, which is what every 207 was before keys existed.
fn unsent_remainder(
    text: &str,
    parts: &[String],
    index: usize,
    replying: bool,
    limit: &crate::chat::PostLimit,
) -> (String, bool) {
    if index == 0 {
        return (text.to_owned(), true);
    }
    let unsent = parts[index..].join("");
    let again = crate::discord::split::split_within(&unsent, limit);
    let resumable = !replying
        && again.first() == Some(&parts[index])
        && !again.iter().any(|part| parts[..index].contains(part));
    (unsent, resumable)
}

fn validate_thread_id(thread_id: &str) -> Result<(), OpError> {
    if thread_id.trim().is_empty()
        || thread_id.len() > 1024
        || thread_id.chars().any(char::is_control)
    {
        return Err(ChatError::Refused(
            "thread_id must be a non-empty thread identifier".to_owned(),
        )
        .into());
    }
    Ok(())
}

/// Attach to a channel's live feed. Read scope.
///
/// The reason this is here rather than in the handler is the allowlist. [`allowed`] is private to
/// this module on purpose — the module doc says both front doors go through it — and a stream is
/// the easiest thing on this server to leave accidentally open, so it goes through the same gate
/// as a fetch and gets the same refusal for a channel outside the configuration. Never a 200 with
/// an empty stream: that reads as "this channel is quiet" rather than "there is no such channel".
///
/// `after` is a resume point, taken from the caller's `Last-Event-ID`.
///
/// Not `async`, unlike everything else here: subscribing touches no network and no store, and
/// making it await nothing would be a promise about future cost that this does not have.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] for a channel outside the allowlist.
pub fn watch(
    state: &AppState,
    channel_id: &str,
    after: Option<&MessageId>,
) -> Result<(ChannelInfo, crate::live::Subscription), OpError> {
    let channel = configured(state, channel_id)?;
    let subscription = state.live.subscribe(&channel.id, after);
    Ok((channel, subscription))
}

/// One channel's inbox state, as far as THIS SERVER is concerned.
///
/// See [`crate::store::INBOX_NOTICE`]: nothing here comes from Discord and nothing here goes back
/// to it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct InboxEntry {
    /// The channel, as configured here.
    pub channel: ChannelInfo,
    /// The newest message the owner has been shown, when this server has been told.
    pub last_read: Option<MessageId>,
    /// When the mark was set, in milliseconds since the Unix epoch.
    pub marked_at_ms: Option<i64>,
}

/// Inbox state for every configured channel, in configuration order. Read scope.
///
/// Every configured channel appears, including the ones with no mark: "never marked" is a state
/// the interface has to be able to show, and an absent row would read as "no such channel". One
/// the owner took off his list does not, because to him it is no longer a channel he reads.
///
/// # Errors
///
/// [`OpError::Store`] when no store is configured, or the read fails.
pub async fn inbox(state: &AppState) -> Result<Vec<InboxEntry>, OpError> {
    let marks = state.store.read_marks().await?;
    Ok(state
        .listed_configured_channels()
        .into_iter()
        .map(|channel| {
            let mark = marks.iter().find(|m| m.channel == channel.id);
            InboxEntry {
                channel,
                last_read: mark.map(|m| m.last_read.clone()),
                marked_at_ms: mark.map(|m| m.marked_at_ms),
            }
        })
        .collect())
}

/// Move this server's read mark for one channel forward. Write scope.
///
/// **Write scope, and the reason is not that it touches Discord** — it does not, and cannot. It
/// mutates durable server state that another device will read back, which is a strictly larger
/// capability than any of the reads on this server, and the `/voice` page already holds the write
/// token.
///
/// Goes through [`allowed`] like every other channel operation, so a snowflake outside the
/// configuration cannot be used to grow the store one row at a time.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Store`] when no store is
/// configured, the id cannot be ordered, or the write fails.
pub async fn mark_read(
    state: &AppState,
    channel_id: &str,
    upto: &MessageId,
) -> Result<(ChannelInfo, crate::store::ReadMark), OpError> {
    let channel = allowed(state, channel_id).await?;
    let mark = state.store.mark_read(&channel.id, upto).await?;
    Ok((channel, mark))
}

/// Drop this server's read mark for one channel, making its whole window unread again. Write
/// scope.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Store`] when there was no mark,
/// when no store is configured, or when the write fails.
pub async fn forget_read_mark(state: &AppState, channel_id: &str) -> Result<ChannelInfo, OpError> {
    let channel = allowed(state, channel_id).await?;
    state.store.forget_read_mark(&channel.id).await?;
    Ok(channel)
}

// --- the to-do view: which messages have NOT been dealt with -------------------------------------
//
// `#50 todo-view`. A long backlog of assistant messages is a to-do list in practice, and Discord
// has nowhere to write down which of them you have finished with. So this server writes it down —
// see `#61 unread-status` and the module documentation on `crate::store`: the overlay is OURS,
// there is no sync-in and no sync-back, and every answer here says so out loud rather than letting
// the owner find it from a divergence.
//
// WHAT IS NOT HERE, stated so a reader of this file does not go looking for it. `#50` also asks
// for a message to leave the list when it is REPLIED to, detected from the reply reference Discord
// records. That needs a field on `crate::model::Message` and a conversion in `crate::discord`, and
// it is deliberately a separate change: it is a wire-format change touching every struct literal
// that builds a Message, and landing it inside this one would make an overlay nobody could review
// on its own. Until it lands, "dealt with" is DECLARED and never derived — which is also why the
// question of which wins when the two disagree does not arise yet.

/// One channel's to-do list: what has not been dealt with, and how much has.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TodoView {
    /// The channel, as configured here.
    pub channel: ChannelInfo,
    /// The messages still wanting attention, oldest first.
    pub messages: Vec<Message>,
    /// How many messages the window held before the dealt-with ones were dropped.
    ///
    /// Reported so a caller can say "9 of 30 left" rather than implying the channel holds nine
    /// messages. It is the WINDOW's size, not the channel's: see [`Window::is_whole_channel`].
    pub window: usize,
    /// Whether that window is the whole channel, so a caller can avoid a confident wrong total.
    pub complete: bool,
    /// When the window's oldest message was sent, as the provider stamped it, dealt with or not;
    /// absent for an empty window. `#240 reading-bookmark`: a page drawing the bookmark on this
    /// list needs to know whether a message the list leaves out is inside the window (so dealt
    /// with, like everything between it and the list) or older than it (so nothing is known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_from: Option<String>,
    /// How many messages in the window the owner's noise rules call read. `#196 auto-read-noise`.
    ///
    /// They are not in `messages` — a placeholder is not something to deal with — and they are
    /// still in `window`, because they are still in the channel.
    pub noise: usize,
}

/// The messages in one channel that have not been dealt with here. Read scope to ask.
///
/// The list is the recent window MINUS the dismissals, MINUS what the owner's noise rules call
/// read: there is no server-side notion of "important" and no inference from dwell time, both of
/// which `#50` excludes deliberately. A message leaves this list because somebody said so — by
/// tapping Done on it, or by writing a rule it matches (`#196 auto-read-noise`).
///
/// A store that is not configured is an ERROR here, unlike in [`summarize_message`], and the
/// difference is not an inconsistency. There, the store is a cache and degrading to "generate
/// every time" produces the same answer more slowly. Here it is the ANSWER: with no overlay every
/// message is undealt-with, so a to-do list served from a missing store would silently be the
/// plain channel wearing a name that promises filtering.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Chat`] when the fetch fails;
/// [`OpError::Store`] when no store is configured or the read fails.
pub async fn todo(
    state: &AppState,
    channel_id: &str,
    limit: Option<u16>,
) -> Result<TodoView, OpError> {
    let window = messages(state, channel_id, limit).await?;
    let complete = window.is_whole_channel();
    let dismissed = state.store.dismissals(&window.channel.id).await?;
    let dismissed: std::collections::HashSet<&str> =
        dismissed.iter().map(MessageId::as_str).collect();
    let total = window.messages.len();
    let window_from = window
        .messages
        .first()
        .map(|message| message.timestamp.clone());
    let noise = window
        .messages
        .iter()
        .filter(|message| message.noise)
        .count();
    let left = window
        .messages
        .into_iter()
        .filter(|message| !message.noise && !dismissed.contains(message.id.as_str()))
        .collect();
    Ok(TodoView {
        channel: window.channel,
        messages: left,
        window: total,
        complete,
        window_from,
        noise,
    })
}

/// Which of `messages` the reader has archived.
///
/// Intersected with the window rather than returned whole: the store holds every dismissal the
/// channel has ever had, and a client only ever needs to grey the rows it is showing. Sending the
/// lot would grow without bound and say nothing the page could use.
///
/// `#50 todo-view`. The To do filter REMOVES these; the ordinary channel view dims them, and until
/// this existed it could not, because nothing in the messages payload said which ones they were.
///
/// # Errors
///
/// [`OpError`] when the store cannot be read.
pub async fn dismissed_within(
    state: &AppState,
    channel: &ChannelId,
    messages: &[Message],
) -> Result<Vec<MessageId>, OpError> {
    let dismissed = state.store.dismissals(channel).await?;
    Ok(dismissed_among(dismissed, messages))
}

/// The marks among `dismissed` that name one of `messages`, in `dismissed`'s order.
fn dismissed_among(dismissed: Vec<MessageId>, messages: &[Message]) -> Vec<MessageId> {
    let present: std::collections::HashSet<&str> =
        messages.iter().map(|message| message.id.as_str()).collect();
    dismissed
        .into_iter()
        .filter(|id| present.contains(id.as_str()))
        .collect()
}

/// The most Done marks one [`crate::contract::ReplyDismissals`] carries. `#222 unread-replies`.
///
/// A thousand ids is about twenty kilobytes before compression, on a newest read of Main and not on
/// a step back. Main's newest page is fifty rows; the marks from its oldest row up are what the
/// reader dealt with since then, which is a day or two of reading in a busy channel and well under
/// this. Past it the newest are kept and the list says where it starts, so what it does say is
/// still complete.
pub const MAX_REPLY_DISMISSALS: usize = 1_000;

/// The read state a timeline page carries: `dismissed` for its rows, and, when `replies` is asked
/// for, every mark from the page's oldest row up. ONE read of the store for both: it is the same
/// list, cut two ways. `#222 unread-replies`.
///
/// # Errors
///
/// [`OpError`] when the store cannot be read.
pub async fn timeline_read_state(
    state: &AppState,
    channel: &ChannelId,
    messages: &[Message],
    replies: bool,
) -> Result<(Vec<MessageId>, Option<crate::contract::ReplyDismissals>), OpError> {
    let dismissed = state.store.dismissals(channel).await?;
    let from_up = if replies {
        reply_dismissals(&dismissed, messages)
    } else {
        None
    };
    Ok((dismissed_among(dismissed, messages), from_up))
}

/// Every mark at or after the oldest of `messages`, newest first, capped at
/// [`MAX_REPLY_DISMISSALS`]. `#222 unread-replies`.
///
/// `None` when no message has a position — an empty page, or ids that are not numbers, which the
/// store could never have marked anyway. Each mark is ordered by its own position, not by the
/// order the store answered in: the list is complete from `from` up only if nothing above `from`
/// was cut, and that is a statement about positions.
#[must_use]
pub fn reply_dismissals(
    dismissed: &[MessageId],
    messages: &[Message],
) -> Option<crate::contract::ReplyDismissals> {
    let oldest = messages
        .iter()
        .filter_map(|message| message.id.numeric().map(|at| (at, &message.id)))
        .min_by_key(|(at, _)| *at)?;
    let mut above: Vec<(u64, &MessageId)> = dismissed
        .iter()
        .filter_map(|id| id.numeric().map(|at| (at, id)))
        .filter(|(at, _)| *at >= oldest.0)
        .collect();
    above.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let from = if above.len() > MAX_REPLY_DISMISSALS {
        above.truncate(MAX_REPLY_DISMISSALS);
        // The oldest mark kept: everything above it is kept, so the list is complete from there.
        above
            .last()
            .map_or_else(|| oldest.1.clone(), |(_, id)| (*id).clone())
    } else {
        oldest.1.clone()
    };
    Some(crate::contract::ReplyDismissals {
        from,
        messages: above.into_iter().map(|(_, id)| id.clone()).collect(),
    })
}

/// What one dismissal or one restoration did.
#[derive(Clone, Debug, serde::Serialize)]
pub struct InboxChange {
    /// The channel, as configured here.
    pub channel: ChannelInfo,
    /// Exactly which messages changed state, oldest first.
    ///
    /// The list, not merely a count, and that is what makes the undo exact: a bulk clear reports
    /// the set it cleared, and putting that set back restores precisely what went — not "the last
    /// N", which is a different set the moment anything else has happened.
    pub messages: Vec<MessageId>,
}

/// The most ids one dismissal or one restoration may name. `#240 reading-bookmark`.
///
/// "Done through here" on a long backlog names every unread message above the row, and at one page
/// (99) a request that was 11 requests and 11 passes of the retention prune for a thousand
/// messages. At a thousand it is one transaction and one prune. Storage is bounded by the
/// retention cap ([`crate::store::Retention::max_dismissals`]) on every write whatever the batch,
/// and a thousand ids is about thirty kilobytes of JSON, far under the body limit.
pub const MAX_DISMISS_BATCH: usize = 1_000;

/// Mark messages as dealt with here. Write scope.
///
/// The configured-channel allowlist is checked without fetching the provider. The ids came from
/// an authenticated page's loaded snapshot and are bounded to [`MAX_DISMISS_BATCH`] per request.
/// This keeps a local archive action local even when the source provider is slow or unavailable.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist, [`OpError::InvalidRange`] for an oversized
/// batch, or [`OpError::Store`].
pub async fn dismiss(
    state: &AppState,
    channel_id: &str,
    wanted: &[MessageId],
) -> Result<InboxChange, OpError> {
    // This is local inbox state. Re-reading the chat provider merely to prove that ids which the
    // authenticated browser just received still exist made a Done tap cost a full provider round
    // trip. Several taps then occupied every adapter worker and turned a local archive into a
    // burst of 20-second 502s. The channel allowlist remains the authority boundary; the batch
    // ceiling bounds storage growth from a malformed authenticated client.
    let channel = allowed(state, channel_id).await?;
    if wanted.len() > MAX_DISMISS_BATCH {
        return Err(OpError::InvalidRange);
    }
    state.store.dismiss(&channel.id, wanted).await?;
    Ok(InboxChange {
        channel,
        messages: wanted.to_vec(),
    })
}

/// Declare bankruptcy: mark everything in the window at or before `through` as dealt with.
///
/// **The boundary is INCLUDED.** "Everything before this one" is what a person means when they
/// press on a row and give up on the backlog, and the row they pressed is part of what they are
/// giving up on. That is a decision, not an accident, and it is tested at the boundary.
///
/// **`limit` is the window the caller READ with, and it bounds what this can clear.** The same
/// number that was passed to [`todo`], because "everything before this one" means everything the
/// reader was looking at — and a caller that read a three-message page must not be able to clear a
/// fifty-message backlog by naming the oldest row of the three. Passing `None` means the default
/// window, which is what a caller that read `/todo` without a limit was shown. A `through` outside
/// the narrowed window is [`OpError::UnknownMessage`], which is the right refusal: the caller is
/// naming a boundary it did not display.
///
/// Contrast [`dismiss`], which takes no window: there the window is only a check that the named
/// ids exist, so narrowing it could only refuse an id the caller really did see.
///
/// Answers the exact set it cleared, so the undo is exact — which is what makes a bulk,
/// destructive action safe enough to offer at all. Messages already dealt with are not in it: they
/// did not change, and restoring them would resurrect something the reader cleared earlier and
/// never asked to see again.
///
/// **Nor is a message the owner's noise rules call read** (`#196 auto-read-noise`). The to-do list
/// this clears never showed one, so sweeping it up would clear a message the reader did not see —
/// and, worse, RECORD it: a dismissal outlives the rule, so removing the rule would no longer bring
/// the placeholder back, and an agent that later edits it into the real answer would find the
/// answer already dealt with. A placeholder stays read by being judged on every read, never by
/// being written down. A `through` that names a placeholder is still a valid boundary; only the
/// placeholder itself is left alone.
///
/// # Errors
///
/// As [`dismiss`]. `through` must itself be in the window `limit` describes.
pub async fn declare_bankruptcy(
    state: &AppState,
    channel_id: &str,
    through: &MessageId,
    limit: Option<u16>,
) -> Result<InboxChange, OpError> {
    let window = messages(state, channel_id, limit).await?;
    let boundary = window
        .messages
        .iter()
        .position(|m| &m.id == through)
        .ok_or(OpError::UnknownMessage)?;
    let already = state.store.dismissals(&window.channel.id).await?;
    let already: std::collections::HashSet<&str> = already.iter().map(MessageId::as_str).collect();
    // By POSITION in the window, which is already sorted oldest-first, rather than by comparing
    // snowflakes here: the window is the ordering this server has, and a second ordering rule
    // beside it is a second thing to get wrong.
    let clearing: Vec<MessageId> = window.messages[..=boundary]
        .iter()
        .filter(|m| !m.noise)
        .map(|m| m.id.clone())
        .filter(|id| !already.contains(id.as_str()))
        .collect();
    state.store.dismiss(&window.channel.id, &clearing).await?;
    Ok(InboxChange {
        channel: window.channel,
        messages: clearing,
    })
}

/// Put messages back into the to-do list. Write scope.
///
/// The undo. Not checked against the window, on purpose and unlike [`dismiss`]: this only ever
/// REMOVES rows, so an id that is not there costs nothing, and refusing a set because one of its
/// messages has since scrolled out of the window would make an undo fail exactly when the reader
/// most needs it.
///
/// Bounded to [`MAX_DISMISS_BATCH`] like a dismissal, so the undo of the largest batch one
/// dismissal may carry is one request too.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist, [`OpError::InvalidRange`] for an oversized
/// batch, or [`OpError::Store`].
pub async fn restore(
    state: &AppState,
    channel_id: &str,
    wanted: &[MessageId],
) -> Result<InboxChange, OpError> {
    let channel = allowed(state, channel_id).await?;
    if wanted.len() > MAX_DISMISS_BATCH {
        return Err(OpError::InvalidRange);
    }
    state.store.restore(&channel.id, wanted).await?;
    Ok(InboxChange {
        channel,
        messages: wanted.to_vec(),
    })
}

// --- messages read automatically -----------------------------------------------------------------
//
// `#196 auto-read-noise`. The rules and the one override. Neither is reachable from an MCP tool —
// the same reasoning as `#39 channel-alias`, and stronger here: a model that could add a rule could
// hide messages from the person it reports to. See `crate::noise` for the matcher and for why a
// rule is evaluated on every read rather than written down as dismissals.

/// The noise rules in force. Read scope: they are shown in Settings and they decide what every
/// read calls noise, so nothing about them is secret from a caller who can read the channels.
///
/// # Errors
///
/// [`OpError::Store`] when a configured store cannot be read. No store at all is the defaults.
pub async fn noise_rules(state: &AppState) -> Result<Vec<String>, OpError> {
    Ok(crate::noise::current_rules(state).await?)
}

/// Replace the noise rules. Write scope, and the OPERATOR's act alone — there is no MCP tool.
///
/// # Errors
///
/// [`OpError::Store`] for a list the validator refuses, when no store is configured, or when the
/// write fails.
pub async fn set_noise_rules(state: &AppState, rules: &[String]) -> Result<Vec<String>, OpError> {
    Ok(state.store.set_noise_rules(rules).await?)
}

/// Say that these messages are not noise, whatever the rules say. Write scope.
///
/// The rescue for a false positive. Checked against the allowlist and bounded to one page per
/// request, exactly as [`dismiss`] is, and for the same reason not checked against a fresh fetch:
/// the ids came from the page the owner is looking at.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist, [`OpError::InvalidRange`] for an oversized
/// batch, or [`OpError::Store`].
pub async fn exempt_from_noise(
    state: &AppState,
    channel_id: &str,
    wanted: &[MessageId],
) -> Result<InboxChange, OpError> {
    let channel = allowed(state, channel_id).await?;
    if wanted.len() > usize::from(MAX_PAGE) {
        return Err(OpError::InvalidRange);
    }
    state.store.exempt_from_noise(&channel.id, wanted).await?;
    Ok(InboxChange {
        channel,
        messages: wanted.to_vec(),
    })
}

// --- pinned messages -------------------------------------------------------------------------------
//
// `#206 pin-message`. The owner's own pins: a message he wants to find again, kept by this server so
// it outlives a restart and shows on every device, filtered to from beside the search glass. OURS in
// the sense `#61 unread-status` gave the read marks — nothing is read from the chat service's own
// pinned messages and nothing is written back to them — and, like `#39 channel-alias`, the owner's
// act alone: there is no MCP tool for any of this, so a model can neither pin nor read the list.

/// Every pin in one channel, with the revision the list is at. Read scope, like `/todo`: the pins
/// are copies of messages in a channel the caller may already read.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Store`] when no store is
/// configured or the read fails.
pub async fn pins(
    state: &AppState,
    channel_id: &str,
) -> Result<(ChannelInfo, crate::store::Pins), OpError> {
    let channel = allowed(state, channel_id).await?;
    let pins = state.store.pins(&channel.id).await?;
    Ok((channel, pins))
}

/// Pin one message, keeping `snapshot` of it. Write scope.
///
/// Checked against the allowlist and NOT against a fresh fetch, for the reason [`dismiss`] is
/// not: the snapshot came from the page the owner is looking at, and a pin that cost a provider
/// round trip would be slow exactly when the provider is. The allowlist is what keeps the table
/// from growing one channel at a time, and [`crate::store::MAX_PINS_PER_CHANNEL`] what bounds it
/// inside one.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Store`] for a snapshot the store
/// refuses, when no store is configured, or when the write fails.
pub async fn pin(
    state: &AppState,
    channel_id: &str,
    snapshot: &crate::store::PinSnapshot,
) -> Result<(ChannelInfo, crate::store::PinChange), OpError> {
    let channel = allowed(state, channel_id).await?;
    let change = state.store.pin(&channel.id, snapshot).await?;
    Ok((channel, change))
}

/// Unpin one message. Write scope. Unpinning a message that is not pinned is not an error.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Store`] when no store is
/// configured or the write fails.
pub async fn unpin(
    state: &AppState,
    channel_id: &str,
    message: &MessageId,
) -> Result<(ChannelInfo, crate::store::PinChange), OpError> {
    let channel = allowed(state, channel_id).await?;
    let change = state.store.unpin(&channel.id, message).await?;
    Ok((channel, change))
}

/// The revision a channel's pins are at, for a read to carry, or `None` when it cannot be said.
///
/// SOFT, unlike every other store read on the read path: a timeline read is how the owner reads
/// his channel, and failing it because the pins' counter could not be read would take the channel
/// away to protect a marker. `None` tells the page to keep the pins it has — a store with nothing
/// configured has none to show, and a failing one is logged here and reported by the next write.
pub async fn pins_revision(state: &AppState, channel: &ChannelId) -> Option<i64> {
    match state.store.pins_revision(channel).await {
        Ok(revision) => Some(revision),
        Err(crate::store::StoreError::Unavailable(_)) => None,
        Err(error) => {
            tracing::warn!(%error, "could not read the pin revision for a channel read");
            None
        }
    }
}

// --- the reading bookmark --------------------------------------------------------------------
//
// `#240 reading-bookmark`. One place per channel the owner keeps while working down a backlog: a
// line drawn above one message. Kept here so it follows him between his phone and his desk, and,
// like `#206 pin-message`, the owner's act alone: no MCP tool reads or moves it. Setting it marks
// nothing read; the page moves it on its own only after a Done the owner made on the message under
// it, and then conditionally (`if_revision`), so a move made on the other device is never undone.

/// One channel's bookmark. Read scope.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Store`] when no store is
/// configured or the read fails.
pub async fn bookmark(
    state: &AppState,
    channel_id: &str,
) -> Result<(ChannelInfo, crate::store::Bookmark), OpError> {
    let channel = allowed(state, channel_id).await?;
    let bookmark = state.store.bookmark(&channel.id).await?;
    Ok((channel, bookmark))
}

/// Put one channel's bookmark at `anchor`, or clear it with `None`; with `if_revision`, only when
/// that is the revision stored. Write scope.
///
/// Checked against the allowlist and NOT against a fresh fetch, for the reason [`pin`] is not: the
/// anchor came from the page the owner is looking at. A refused conditional write is not an error
/// here; [`crate::store::BookmarkChange::applied`] says so and the caller answers 409.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::Store`] for an anchor the store
/// refuses, when no store is configured, or when the write fails.
pub async fn set_bookmark(
    state: &AppState,
    channel_id: &str,
    anchor: Option<&crate::store::BookmarkAnchor>,
    if_revision: Option<i64>,
) -> Result<(ChannelInfo, crate::store::BookmarkChange), OpError> {
    let channel = allowed(state, channel_id).await?;
    let change = state
        .store
        .set_bookmark(&channel.id, anchor, if_revision)
        .await?;
    Ok((channel, change))
}

/// A channel's bookmark for a read to carry, or `None` when it cannot be said.
///
/// SOFT, for [`pins_revision`]'s reason: a channel read must not fail because a marker could not be
/// read. `None` tells the page to keep the bookmark it has, which is why the payload's field is
/// absent rather than `null` then: `null` would say there is none.
pub async fn bookmark_state(
    state: &AppState,
    channel: &ChannelId,
) -> Option<crate::store::Bookmark> {
    match state.store.bookmark(channel).await {
        Ok(bookmark) => Some(bookmark),
        Err(crate::store::StoreError::Unavailable(_)) => None,
        Err(error) => {
            tracing::warn!(%error, "could not read the bookmark for a channel read");
            None
        }
    }
}

/// What happened when a summary was asked for.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "summary")]
pub enum SummaryOutcome {
    /// The message was short enough to read as it is; nothing was generated and nothing was
    /// cached. Distinguished from a summary on purpose: a page must be able to show the original
    /// rather than a shortened copy of something that was already short.
    BelowThreshold,
    /// Served from the store, under the current policy.
    Cached(String),
    /// Generated now, and filed.
    Generated(String),
}

impl SummaryOutcome {
    /// The summary text, when there is one.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::BelowThreshold => None,
            Self::Cached(text) | Self::Generated(text) => Some(text),
        }
    }
}

/// One summary, and what it cost to obtain.
///
/// A struct rather than a tuple because the third member is the whole point of it existing: the
/// open question about a hosted conversational agent is how long a round trip to one takes
/// compared with a full-size model, and a deployment that cannot answer that from its own logs and
/// its own API has to run a separate experiment to find out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summarised {
    /// The channel it came from.
    pub channel: ChannelInfo,
    /// Whether it was below the threshold, served from the cache, or generated now.
    pub outcome: SummaryOutcome,
    /// Wall-clock milliseconds spent inside the summariser.
    ///
    /// `None` when none was called — a message below the threshold, or a cache hit. That is the
    /// distinction worth keeping: a zero here would say "the vendor answered instantly", which is
    /// a very different claim from "no vendor was asked".
    pub generated_in_ms: Option<u64>,
}

/// One message, summarised — from the cache when the policy and the text are unchanged. Read
/// scope to ASK; write scope to leave anything behind.
///
/// The ORDER here is load-bearing. The allowlist first, then what the page itself read in the
/// last thirty seconds, then the threshold, then the store, and only then the chat provider and
/// the summariser:
///
/// * **The threshold is checked BEFORE the cache and before the summariser**, so a short message
///   costs neither a lookup nor a call.
/// * **A hit costs no chat call when the row was just read.** `#208 persistent-cache`, milestone
///   `summary-keep`. The page asks for a summary of a row it has just drawn, so the text is in
///   [`crate::read_ahead`] already; a stored summary of exactly that text is answered from there,
///   with no upstream read. That memory is the thirty-second one speech preparation uses, cleared
///   by pushed edits and deletes, delta deletions and the purge, so a summary is never made from
///   text older than the page was shown and a pushed delete is honoured at once. On a provider
///   whose feed carries no deletes, a message deleted upstream in those thirty seconds can still
///   have its stored summary served, which is the window speech preparation already accepts.
/// * **Requests that miss together share one fetch.** Otherwise the channel window is read through
///   the same per-channel turn speech preparation takes
///   ([`crate::read_ahead::ReadAhead::window_turn`]), so twenty rows asked for at once cost one
///   window read, not twenty.
///
/// `caller` is the scope the credential actually carries, and it decides what is WRITTEN. A
/// read-scope token may SPEND the cache and may never FILL it: it is never filed, never keeps an
/// entry alive, and is served a stored answer cut shorter than the cap now in force rather than
/// asking for a longer one it could not keep. The read token is the one pasted into a hosted voice
/// agent, and a durable write reachable from the least-trusted credential is precisely what the
/// two-token split exists to prevent — see [`crate::http`] for the rule stated once. A write-scope
/// hit more than a day old is kept alive ([`crate::store::StateStore::touch_summary`]), so the age
/// limit counts from the last use.
///
/// What is stored is the answer uncut up to a generous ceiling; what is handed out is cut at the
/// cap in force ([`crate::summarize::cut_for_serving`]).
///
/// The summariser is a model being fed channel text, so the request is built through
/// [`crate::summarize::SummaryRequest::new`], which frames it with [`crate::untrusted`] — the
/// fence is part of constructing the request rather than something this function could forget. A
/// summary is not exempt from the boundary for being short.
///
/// # Errors
///
/// [`OpError::UnknownChannel`] outside the allowlist; [`OpError::UnknownMessage`] when the
/// message is not in the fetched window; [`OpError::Chat`] when the fetch fails;
/// [`OpError::Summarizer`] when the summariser refuses. A store that is unavailable is NOT an
/// error here — see below.
pub async fn summarize_message(
    state: &AppState,
    caller: crate::auth::Scope,
    channel_id: &str,
    message_id: &str,
    also: &[String],
    limit: Option<u16>,
) -> Result<Summarised, OpError> {
    summarize_message_scoped(state, caller, channel_id, message_id, also, limit, None).await
}

/// Summarize messages from a selected thread without mixing in other threads' context.
///
/// See [`summarize_message`] for the order of work and what each scope may write.
pub async fn summarize_message_scoped(
    state: &AppState,
    caller: crate::auth::Scope,
    channel_id: &str,
    message_id: &str,
    also: &[String],
    limit: Option<u16>,
    thread_id: Option<&str>,
) -> Result<Summarised, OpError> {
    // Before anything is recalled or fetched: a shared window fetch that finished after this
    // instant is fresh enough to answer this request.
    let began = std::time::Instant::now();
    state.summary_counters.request();
    if thread_id.is_some() && also.len() >= usize::from(MAX_PAGE) {
        return Err(ChatError::Refused("too many messages in one summary".to_owned()).into());
    }
    let channel = allowed(state, channel_id).await?;
    if let Some(thread_id) = thread_id {
        validate_thread_id(thread_id)?;
    }

    // FROM MEMORY. Every message the row is made of, as the page was just handed it.
    let mut ids = Vec::with_capacity(1 + also.len());
    ids.push(message_id.to_owned());
    ids.extend(also.iter().cloned());
    let mut recalled = state.read_ahead.recall(&channel.id, thread_id, &ids);
    if let Some(target) = combined_row(message_id, also, |id| recalled.remove(id)) {
        if below_threshold(state, &target) {
            return Ok(below_threshold_answer(channel));
        }
        let key = summary_key(state, &channel, &target);
        match stored_summary(state, caller, &key).await {
            Stored::Serve { text, below_cap } => {
                state.summary_counters.hit(true, below_cap);
                return Ok(cached_answer(channel, text));
            }
            Stored::Miss { below_cap } => {
                // A thread summary takes no context, and neither does a deployment configured for
                // none: the text in hand is everything the summariser would be shown.
                if thread_id.is_some() || state.config.summaries.context_messages == 0 {
                    return generate_summary(state, caller, channel, &target, &[], key, below_cap)
                        .await;
                }
            }
        }
    }

    // UPSTREAM: the existing path, with the channel window shared between requests.
    let window = if let Some(thread_id) = thread_id {
        let fetching = std::time::Instant::now();
        let mut messages = Vec::with_capacity(ids.len());
        for id in &ids {
            let (_, message) =
                message_by_id_scoped(state, channel_id, id, limit, Some(thread_id)).await?;
            messages.push(message);
        }
        state.summary_counters.fetched(messages.len());
        // As speech preparation does: the next request for these rows recalls them.
        state
            .read_ahead
            .remember(&channel.id, Some(thread_id), &messages, fetching);
        Window {
            channel,
            messages,
            limit: state.effective_limit(limit),
            as_of: None,
        }
    } else {
        shared_window(state, channel, limit, began).await?
    };
    let position = window
        .messages
        .iter()
        .position(|m| m.id.as_str() == message_id)
        .ok_or(OpError::UnknownMessage)?;

    // WHAT THE READER IS LOOKING AT, WHICH MAY BE SEVERAL MESSAGES.
    //
    // A post over Discord's length limit arrives as a message and a short trailing remainder, and
    // the page draws those as one row. Summarising only the first would describe the part that
    // stops mid-sentence and ignore the end of it — and the tail on its own is the least
    // summarisable text in the channel, being the back half of a sentence whose front half is
    // somewhere else. So the group is joined FIRST and summarised as the one piece of writing it
    // was before Discord cut it up.
    //
    // The identity stays the first message's throughout: that is what the row is keyed by, what a
    // reply threads from, and what the cache is filed under. Only the CONTENT is combined — and
    // because the cache key hashes that content, a row that later groups differently is a
    // different key rather than a stale hit.
    let target = combined_row(message_id, also, |id| {
        window
            .messages
            .iter()
            .find(|m| m.id.as_str() == id)
            .cloned()
    })
    .ok_or(OpError::UnknownMessage)?;

    if below_threshold(state, &target) {
        return Ok(below_threshold_answer(window.channel));
    }

    let key = summary_key(state, &window.channel, &target);
    // A store that is absent or broken degrades to "generate every time", loudly in the log and
    // silently to the caller. The alternative — refusing to summarise because the CACHE is not
    // configured — would make an optional durability feature a hard dependency of a read.
    let below_cap = match stored_summary(state, caller, &key).await {
        Stored::Serve { text, below_cap } => {
            state.summary_counters.hit(false, below_cap);
            return Ok(cached_answer(window.channel, text));
        }
        Stored::Miss { below_cap } => below_cap,
    };

    let start = position.saturating_sub(state.config.summaries.context_messages);
    let context = &window.messages[start..position];
    generate_summary(
        state,
        caller,
        window.channel.clone(),
        &target,
        context,
        key,
        below_cap,
    )
    .await
}

/// The row a summary is about: the message `message_id`, with the `also` messages' text joined on
/// after it, one per line, in the order given. `None` when any of them cannot be found.
fn combined_row(
    message_id: &str,
    also: &[String],
    mut find: impl FnMut(&str) -> Option<Message>,
) -> Option<Message> {
    let mut combined = find(message_id)?;
    if !also.is_empty() {
        let mut parts = vec![combined.content.clone()];
        for id in also {
            parts.push(find(id)?.content);
        }
        combined.content = parts.join("\n");
    }
    Some(combined)
}

fn below_threshold(state: &AppState, target: &Message) -> bool {
    target.content.chars().count() < state.config.summaries.threshold_chars
}

fn below_threshold_answer(channel: ChannelInfo) -> Summarised {
    Summarised {
        channel,
        outcome: SummaryOutcome::BelowThreshold,
        generated_in_ms: None,
    }
}

fn cached_answer(channel: ChannelInfo, text: String) -> Summarised {
    Summarised {
        channel,
        outcome: SummaryOutcome::Cached(text),
        generated_in_ms: None,
    }
}

fn summary_key(
    state: &AppState,
    channel: &ChannelInfo,
    target: &Message,
) -> crate::store::SummaryKey {
    crate::store::SummaryKey {
        channel: channel.id.clone(),
        message: target.id.clone(),
        content_hash: crate::summarize::content_hash(&target.content),
        version: state.summary_version.to_string(),
    }
}

/// What the store has for one key, from this caller's point of view.
enum Stored {
    /// Serve this, already cut for serving. `below_cap` when the stored answer was cut shorter
    /// than the cap now in force (only ever served to a read-scope caller).
    Serve { text: String, below_cap: bool },
    /// Ask the summariser. `below_cap` when there IS an entry, cut shorter than the cap now in
    /// force, and this write-scope caller is to replace it.
    Miss { below_cap: bool },
}

/// Look `key` up for `caller`. Keeps a write-scope hit alive; writes nothing for a read-scope one.
async fn stored_summary(
    state: &AppState,
    caller: crate::auth::Scope,
    key: &crate::store::SummaryKey,
) -> Stored {
    let requested = state.config.summaries.target_chars;
    let may_write = caller >= crate::auth::Scope::Write;
    match state.store.cached_summary(key).await {
        Ok(Some(hit)) => {
            let below_cap = crate::summarize::cut_below_cap(hit.cut_at, requested);
            if below_cap && may_write {
                return Stored::Miss { below_cap: true };
            }
            if may_write
                && hit.made_at_ms < crate::store::now_ms() - crate::store::SUMMARY_TOUCH_AFTER_MS
            {
                if let Err(error) = state.store.touch_summary(key).await {
                    tracing::warn!(%error, "a cached summary could not be kept alive");
                }
            }
            Stored::Serve {
                text: crate::summarize::cut_for_serving(&hit.text, requested),
                below_cap,
            }
        }
        Ok(None) | Err(crate::store::StoreError::Unavailable(_)) => {
            Stored::Miss { below_cap: false }
        }
        Err(error) => {
            tracing::warn!(%error, "summary cache unreadable; generating uncached");
            Stored::Miss { below_cap: false }
        }
    }
}

/// The channel window for a summary, read once for every request of the channel that misses at
/// the same time: through the same turn speech preparation takes. `#208 persistent-cache`.
///
/// A request that waited behind a fetch which finished after it began, and was at least as wide as
/// its own, rebuilds that window from memory instead of reading it again. If any message of it has
/// been forgotten since — an edit or a delete was pushed — it reads the window itself.
async fn shared_window(
    state: &AppState,
    channel: ChannelInfo,
    limit: Option<u16>,
    began: std::time::Instant,
) -> Result<Window, OpError> {
    let width = window_limit(state, limit);
    let mut turn = state.read_ahead.window_turn(&channel.id).await;
    if let Some(read) = turn
        .as_ref()
        .filter(|read| read.finished >= began && read.limit >= width)
    {
        if let Some(messages) = state.read_ahead.recall_window(&channel.id, read) {
            state.summary_counters.joined();
            return Ok(Window {
                channel,
                messages,
                limit: read.limit,
                as_of: read.as_of.clone(),
            });
        }
    }
    // `messages` remembers the window for everyone else as well.
    let window = messages(state, channel.id.as_str(), limit).await?;
    state.summary_counters.fetched(1);
    *turn = Some(crate::read_ahead::WindowRead::of(&window));
    Ok(window)
}

/// Ask the summariser, file the answer for a write-scope caller, and hand it out cut for serving.
async fn generate_summary(
    state: &AppState,
    caller: crate::auth::Scope,
    channel: ChannelInfo,
    target: &Message,
    context: &[Message],
    key: crate::store::SummaryKey,
    below_cap: bool,
) -> Result<Summarised, OpError> {
    state.summary_counters.miss();
    let requested = state.config.summaries.target_chars;
    let request = crate::summarize::SummaryRequest::new(target, context, requested);
    // Timed HERE rather than inside the summariser, so the number is the WHOLE cost of answering
    // — the vendor round trip plus everything this server does around it — rather than the part
    // the vendor happens to report. That is the number a reader waiting for a row is living
    // through, and it is what the page quotes back as a median.
    let began = std::time::Instant::now();
    let generated = state.summarizer.summarize(&request).await;
    let generated_in_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    // Logged on the failure path too. A summariser that takes thirty seconds to time out is a
    // performance fact about the deployment, and it is the one a person is most likely to want.
    tracing::info!(
        backend = state.summarizer.backend(),
        elapsed_ms = generated_in_ms,
        ok = generated.is_ok(),
        "asked the summariser for one message"
    );
    let generated = generated?;
    state.summary_counters.generated(below_cap);
    if caller >= crate::auth::Scope::Write {
        if let Err(error) = state
            .store
            .cache_summary(&key, &generated.text, generated.cut_at)
            .await
        {
            tracing::warn!(%error, "summary could not be cached; it will be generated again");
        }
    } else {
        tracing::debug!(
            "a read-scope caller asked for a summary; it was generated and NOT filed, because a \
             read token may spend the cache and never fill it"
        );
    }
    Ok(Summarised {
        channel,
        outcome: SummaryOutcome::Generated(crate::summarize::cut_for_serving(
            &generated.text,
            requested,
        )),
        generated_in_ms: Some(generated_in_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StateStore as _;
    use crate::testing::{self, READ_CHANNEL, WRITE_CHANNEL};

    #[test]
    fn the_read_back_is_one_sentence_ending_with_the_question() {
        // `#264 voice-send-approval`: the voice page holds a spoken read-back to exactly this.
        assert_eq!(
            read_back("lead team", "on it, rebasing now"),
            "To lead team: on it, rebasing now. Send it?"
        );
        // A text that already ends a sentence is not given a second full stop.
        assert_eq!(
            read_back("lead team", " landed it! "),
            "To lead team: landed it! Send it?"
        );
        assert_eq!(read_back("ops", "done?"), "To ops: done? Send it?");
    }

    #[tokio::test]
    async fn a_channel_outside_the_allowlist_is_refused_for_reads() {
        let (state, _fake) = testing::state();
        let error = messages(&state, "9999999999", None)
            .await
            .expect_err("an unconfigured channel must not be readable");
        assert!(matches!(error, OpError::UnknownChannel), "{error:?}");
    }

    #[tokio::test]
    async fn a_channel_outside_the_allowlist_is_refused_for_writes_and_never_reaches_discord() {
        let (state, fake) = testing::state();
        let error = reply(&state, "9999999999", "hello", None)
            .await
            .expect_err("an unconfigured channel must not be postable");
        assert!(matches!(error, OpError::UnknownChannel), "{error:?}");
        assert!(
            fake.posted().is_empty(),
            "a refused post must not reach Discord at all: {:?}",
            fake.posted()
        );
    }

    #[tokio::test]
    async fn a_read_only_channel_is_refused_for_writes_and_never_reaches_discord() {
        let (state, fake) = testing::state();
        let error = reply(&state, READ_CHANNEL, "hello", None)
            .await
            .expect_err("a read-only channel must not be postable");
        assert!(matches!(error, OpError::ChannelNotWritable), "{error:?}");
        assert!(fake.posted().is_empty(), "{:?}", fake.posted());
    }

    #[tokio::test]
    async fn a_writable_channel_actually_posts_what_was_asked_for() {
        let (state, fake) = testing::state();
        let (info, posted, _parts) = reply(&state, WRITE_CHANNEL, "shipped it", None)
            .await
            .expect("the writable channel accepts a post");
        assert_eq!(info.id.as_str(), WRITE_CHANNEL);
        assert_eq!(posted.content, "shipped it");
        let recorded = fake.posted();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert_eq!(recorded[0].channel.as_str(), WRITE_CHANNEL);
        assert_eq!(recorded[0].content, "shipped it");
    }

    #[tokio::test]
    async fn an_empty_query_is_refused_before_anything_is_fetched() {
        let (state, _fake) = testing::state();
        let error = resolve(&state, READ_CHANNEL, "   ", None, None)
            .await
            .expect_err("a blank query has nothing to resolve");
        assert!(matches!(error, OpError::EmptyQuery), "{error:?}");
    }

    #[tokio::test]
    async fn an_unknown_message_id_is_a_miss_not_a_substitute() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        fake.seed(&channel, "agent", "first");
        fake.seed(&channel, "agent", "second");
        let error = message_by_id(&state, READ_CHANNEL, "404040404040", None)
            .await
            .expect_err("an id outside the window must miss");
        assert!(matches!(error, OpError::UnknownMessage), "{error:?}");
    }

    #[tokio::test]
    async fn a_short_fetch_is_the_whole_channel_and_a_full_one_is_not() {
        // The fixture's default_fetch_limit is 20 (see `testing::config_toml`).
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        for i in 0..5 {
            fake.seed(&channel, "agent", &format!("line {i}"));
        }
        let window = messages(&state, READ_CHANNEL, None).await.expect("reads");
        assert!(
            window.is_whole_channel(),
            "a fetch that came back short is Discord saying there is nothing older"
        );

        for i in 0..40 {
            fake.seed(&channel, "agent", &format!("more {i}"));
        }
        let window = messages(&state, READ_CHANNEL, None).await.expect("reads");
        assert_eq!(window.messages.len(), 20);
        assert!(
            !window.is_whole_channel(),
            "a window that filled says nothing about how much more there is"
        );
    }

    #[tokio::test]
    async fn a_configured_limit_above_discords_ceiling_does_not_fake_a_whole_channel() {
        // The clamp trap. Nothing in configuration validation stops `max_fetch_limit = 200`, but
        // Discord tops out at 100 — so a 100-message answer to a "200" request is a FULL window,
        // not a small channel, and comparing against 200 would report the wrong thing.
        let toml = testing::config_toml()
            .replace("default_fetch_limit = 20", "default_fetch_limit = 200")
            .replace("max_fetch_limit = 50", "max_fetch_limit = 200");
        let (state, fake, _elevenlabs) = testing::state_from_toml(&toml);
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        for i in 0..120 {
            fake.seed(&channel, "agent", &format!("line {i}"));
        }
        let window = messages(&state, READ_CHANNEL, None).await.expect("reads");
        assert_eq!(
            window.limit,
            crate::discord::http::DISCORD_MAX_LIMIT,
            "the window must record the size Discord was actually asked for"
        );
        assert_eq!(window.messages.len(), 100);
        assert!(
            !window.is_whole_channel(),
            "a full 100-message window must never be reported as the whole channel"
        );
    }

    #[tokio::test]
    async fn a_page_knows_whether_more_remain_without_a_second_round_trip() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        for i in 0..25 {
            fake.seed(&channel, "agent", &format!("m{i}"));
        }
        let before = fake.fetch_count();
        let step = page(
            &state,
            READ_CHANNEL,
            PageRequest {
                limit: Some(10),
                ..PageRequest::default()
            },
        )
        .await
        .expect("reads");
        assert_eq!(
            step.returned(),
            10,
            "the extra probe message must be dropped"
        );
        assert!(step.has_more);
        assert_eq!(
            step.next_before.as_ref().map(MessageId::as_str),
            Some(step.messages[0].id.as_str()),
            "the cursor to step back from is the OLDEST message this page returned"
        );
        assert_eq!(
            fake.fetch_count() - before,
            1,
            "over-fetching by one is the point: answering \"is there more\" must not cost a \
             second request"
        );
        assert_eq!(
            step.messages.last().expect("non-empty").content,
            "m24",
            "an uncursored page starts at the newest message"
        );
    }

    #[tokio::test]
    async fn the_last_page_says_there_is_nothing_beyond_it() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        for i in 0..4 {
            fake.seed(&channel, "agent", &format!("m{i}"));
        }
        let step = page(
            &state,
            READ_CHANNEL,
            PageRequest {
                limit: Some(10),
                ..PageRequest::default()
            },
        )
        .await
        .expect("reads");
        assert_eq!(step.returned(), 4);
        assert!(
            !step.has_more,
            "a control: an implementation that always claimed more would pass the other test"
        );
        assert!(step.next_before.is_none());
    }

    #[tokio::test]
    async fn stepping_twice_covers_a_disjoint_span_with_nothing_skipped_at_the_boundary() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        for i in 0..25 {
            fake.seed(&channel, "agent", &format!("m{i}"));
        }
        let first = page(
            &state,
            READ_CHANNEL,
            PageRequest {
                limit: Some(10),
                ..PageRequest::default()
            },
        )
        .await
        .expect("reads");
        let cursor = first.next_before.clone().expect("more remain");
        let second = page(
            &state,
            READ_CHANNEL,
            PageRequest {
                limit: Some(10),
                before: Some(cursor.as_str()),
                ..PageRequest::default()
            },
        )
        .await
        .expect("reads");

        let ids = |p: &Page| p.messages.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        let (a, b) = (ids(&first), ids(&second));
        assert!(
            a.iter().all(|id| !b.contains(id)),
            "the two steps overlap: {a:?} / {b:?}"
        );
        let mut union: Vec<_> = b.iter().chain(a.iter()).cloned().collect();
        assert_eq!(union.len(), 20);
        let expected: Vec<_> = (5..25).map(|i| format!("m{i}")).collect();
        let got: Vec<_> = second
            .messages
            .iter()
            .chain(first.messages.iter())
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(
            got, expected,
            "the two steps together must be exactly the newest twenty, in order, with nothing \
             skipped or repeated at the boundary"
        );
        union.dedup();
        assert_eq!(union.len(), 20);
    }

    #[tokio::test]
    async fn a_time_range_is_exact_at_both_edges() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        let base = 1_787_000_000_000_i64;
        for (offset, label) in [
            (-1_000, "before the window"),
            (0, "exactly at the start"),
            (5_000, "inside"),
            (10_000, "exactly at the end"),
            (15_000, "after the window"),
        ] {
            fake.seed_at(&channel, "agent", label, base + offset);
        }
        let since = crate::clock::iso_from_ms(base);
        let until = crate::clock::iso_from_ms(base + 10_000);
        let step = page(
            &state,
            READ_CHANNEL,
            PageRequest {
                limit: Some(10),
                since: Some(&since),
                until: Some(&until),
                ..PageRequest::default()
            },
        )
        .await
        .expect("reads");
        assert_eq!(
            step.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            vec!["exactly at the start", "inside"],
            "the start edge is INCLUSIVE and the end edge is EXCLUSIVE, both exactly"
        );
        assert!(!step.has_more);
    }

    #[tokio::test]
    async fn a_range_walk_hands_back_where_to_resume() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        let base = 1_787_000_000_000_i64;
        for i in 0..6 {
            fake.seed_at(&channel, "agent", &format!("m{i}"), base + i * 1_000);
        }
        let since = crate::clock::iso_from_ms(base);
        let first = page(
            &state,
            READ_CHANNEL,
            PageRequest {
                limit: Some(2),
                since: Some(&since),
                ..PageRequest::default()
            },
        )
        .await
        .expect("reads");
        assert_eq!(
            first
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            vec!["m0", "m1"],
            "a range walks FORWARD from its start, oldest first"
        );
        assert!(first.has_more);
        let resume = first.next_since.clone().expect("a place to resume");
        let second = page(
            &state,
            READ_CHANNEL,
            PageRequest {
                limit: Some(2),
                since: Some(&resume),
                ..PageRequest::default()
            },
        )
        .await
        .expect("reads");
        assert_eq!(
            second
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            vec!["m2", "m3"],
            "resuming must continue rather than repeat the last message of the previous step"
        );
    }

    #[tokio::test]
    async fn a_page_refuses_arguments_it_cannot_honour_instead_of_picking_one() {
        let (state, _fake) = testing::state();
        let cases: [(PageRequest<'_>, &str); 4] = [
            (
                PageRequest {
                    before: Some("not-a-snowflake"),
                    ..PageRequest::default()
                },
                "invalid_cursor",
            ),
            (
                PageRequest {
                    since: Some("yesterday afternoon"),
                    ..PageRequest::default()
                },
                "invalid_range",
            ),
            (
                PageRequest {
                    before: Some("1000000000000000001"),
                    since: Some("2026-08-19T00:00:00Z"),
                    ..PageRequest::default()
                },
                "invalid_range",
            ),
            (
                PageRequest {
                    since: Some("2026-08-19T10:00:00Z"),
                    until: Some("2026-08-19T09:00:00Z"),
                    ..PageRequest::default()
                },
                "invalid_range",
            ),
        ];
        for (request, code) in cases {
            let error = page(&state, READ_CHANNEL, request)
                .await
                .expect_err("must refuse");
            assert_eq!(error.code(), code, "{request:?} -> {error}");
        }
    }

    #[tokio::test]
    async fn the_count_is_not_the_page_size() {
        // The reported bug, reproduced. The fixture's max_fetch_limit is 50 and Discord's own
        // page ceiling is 100; a channel of 150 must answer 150, never either of those.
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        for i in 0..150 {
            fake.seed(&channel, "agent", &format!("m{i}"));
        }
        let tally = count(&state, READ_CHANNEL, None, None)
            .await
            .expect("counts");
        assert_eq!(tally.counted, 150);
        assert!(
            !tally.at_least,
            "the walk reached the end, so this is a total and must not be hedged"
        );
        assert_ne!(tally.counted, 50);
        assert_ne!(tally.counted, 100);
    }

    #[tokio::test]
    async fn a_count_stopped_by_the_ceiling_says_it_is_a_lower_bound() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        for i in 0..350 {
            fake.seed(&channel, "agent", &format!("m{i}"));
        }
        let tally = count(&state, READ_CHANNEL, None, Some(120))
            .await
            .expect("counts");
        assert!(
            tally.at_least,
            "a walk the cap stopped must not be reported as a total"
        );
        assert!(tally.counted >= 120, "{}", tally.counted);
        assert_eq!(tally.cap, 120);
        assert!(tally.oldest_seen.is_some() && tally.newest_seen.is_some());
    }

    #[tokio::test]
    async fn a_count_since_an_instant_counts_only_what_falls_after_it() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        let base = 1_787_000_000_000_i64;
        for i in 0..10 {
            fake.seed_at(&channel, "agent", &format!("m{i}"), base + i * 1_000);
        }
        let since = crate::clock::iso_from_ms(base + 6_000);
        let tally = count(&state, READ_CHANNEL, Some(&since), None)
            .await
            .expect("counts");
        assert_eq!(
            tally.counted, 4,
            "m6 through m9, with the boundary itself counted as inside"
        );
        assert!(!tally.at_least);
    }

    // --- `#39 channel-alias` -------------------------------------------------------------------

    #[tokio::test]
    async fn the_operators_name_reaches_every_channel_this_server_hands_back() {
        // The overlay, on both of the two paths a channel leaves this module by: the LISTING, and
        // the per-channel gate every read goes through. A fix applied to only one of the two is
        // how the picker and the digest header end up disagreeing about what the channel is
        // called.
        let (state, fake, store) = testing::state_with_store();
        store
            .set_channel_alias(
                &crate::model::ChannelId(READ_CHANNEL.to_owned()),
                "the build channel",
            )
            .await
            .expect("the operator names it");

        let listed = channels(&state).await;
        let renamed = listed
            .iter()
            .find(|c| c.id.as_str() == READ_CHANNEL)
            .expect("the read channel is configured");
        assert_eq!(renamed.alias.as_deref(), Some("the build channel"));
        assert_eq!(
            renamed.label, "build noise",
            "the configured label survives"
        );
        assert_eq!(renamed.display_name(), "the build channel");

        let untouched = listed
            .iter()
            .find(|c| c.id.as_str() == WRITE_CHANNEL)
            .expect("the write channel is configured");
        assert_eq!(untouched.alias, None, "one alias must not rename the rest");
        assert_eq!(untouched.display_name(), "lead team");

        fake.seed(
            &crate::model::ChannelId(READ_CHANNEL.to_owned()),
            "agent",
            "the arm64 job failed",
        );
        let window = messages(&state, READ_CHANNEL, None).await.expect("reads");
        assert_eq!(
            window.channel.display_name(),
            "the build channel",
            "a read must come back wearing the name the operator gave it"
        );
    }

    #[tokio::test]
    async fn a_channel_is_found_by_id_label_or_alias_but_never_outside_the_allowlist() {
        let (state, _fake, _store) = testing::state_with_store();
        let read = Some(crate::model::ChannelId(READ_CHANNEL.to_owned()));
        assert_eq!(channel_named(&state, READ_CHANNEL).await, read);
        assert_eq!(channel_named(&state, "build noise").await, read);
        assert_eq!(channel_named(&state, "  Build NOISE ").await, read);
        assert_eq!(channel_named(&state, "the build channel").await, None);
        set_channel_alias(&state, READ_CHANNEL, "The Build Channel")
            .await
            .expect("set");
        assert_eq!(channel_named(&state, "the build channel").await, read);
        assert_eq!(
            channel_named(&state, "build noise").await,
            read,
            "the configured label still names the channel once it has an alias"
        );
        for stranger in ["9999999999", "", "   ", "build", "noise"] {
            assert_eq!(channel_named(&state, stranger).await, None, "{stranger:?}");
        }
    }

    #[tokio::test]
    async fn a_name_two_channels_answer_to_names_neither() {
        let (state, _fake, _store) = testing::state_with_store();
        set_channel_alias(&state, WRITE_CHANNEL, "Build Noise")
            .await
            .expect("set");
        assert_eq!(channel_named(&state, "build noise").await, None);
        assert_eq!(
            channel_named(&state, WRITE_CHANNEL).await,
            Some(crate::model::ChannelId(WRITE_CHANNEL.to_owned())),
            "an exact id is never ambiguous"
        );
    }

    #[tokio::test]
    async fn clearing_the_alias_puts_the_configured_label_back_everywhere() {
        let (state, _fake, store) = testing::state_with_store();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        set_channel_alias(&state, READ_CHANNEL, "the build channel")
            .await
            .expect("set");
        let cleared = clear_channel_alias(&state, READ_CHANNEL)
            .await
            .expect("clear");
        assert_eq!(cleared.alias, None);
        assert_eq!(cleared.display_name(), "build noise");
        assert!(
            store.channel_aliases().await.expect("read").is_empty(),
            "clearing must remove the row, not merely stop showing it"
        );
        assert_eq!(
            channels(&state)
                .await
                .iter()
                .find(|c| c.id == channel)
                .expect("configured")
                .display_name(),
            "build noise"
        );
    }

    #[tokio::test]
    async fn setting_an_alias_is_confined_to_the_allowlist_and_writes_nothing_when_refused() {
        // Otherwise an unconfigured snowflake grows this table one row at a time, and each row
        // names a channel this server will never show.
        let (state, _fake, store) = testing::state_with_store();
        let error = set_channel_alias(&state, "9999999999", "somewhere else")
            .await
            .expect_err("an unconfigured channel must not be nameable");
        assert!(matches!(error, OpError::UnknownChannel), "{error:?}");
        assert!(
            store.channel_aliases().await.expect("read").is_empty(),
            "a refused rename must not have written anything"
        );
        let error = clear_channel_alias(&state, "9999999999")
            .await
            .expect_err("nor clearable");
        assert!(matches!(error, OpError::UnknownChannel), "{error:?}");
    }

    #[tokio::test]
    async fn a_blank_alias_is_refused_rather_than_silently_clearing_the_name() {
        let (state, _fake, store) = testing::state_with_store();
        set_channel_alias(&state, READ_CHANNEL, "the build channel")
            .await
            .expect("set");
        let error = set_channel_alias(&state, READ_CHANNEL, "   ")
            .await
            .expect_err("a blank name must be refused");
        assert_eq!(error.code(), "bad_id", "{error}");
        assert_eq!(
            store
                .channel_aliases()
                .await
                .expect("read")
                .first()
                .map(|a| a.alias.clone()),
            Some("the build channel".to_owned()),
            "a refused rename must leave the name that was already there"
        );
    }

    #[tokio::test]
    async fn a_channel_with_no_alias_cannot_be_cleared_and_is_told_so() {
        let (state, _fake, _store) = testing::state_with_store();
        let error = clear_channel_alias(&state, READ_CHANNEL)
            .await
            .expect_err("there was nothing to clear");
        assert_eq!(error.code(), "not_found", "{error}");
    }

    #[tokio::test]
    async fn with_no_store_configured_a_channel_still_reads_under_its_configured_label() {
        // The ordinary deployment that never set `storage.path`. An alias is a DECORATION: a
        // store that refuses every call must cost the operator his aliases and nothing else —
        // certainly not the ability to read a channel at all.
        let (state, fake) =
            testing::state_with(std::sync::Arc::new(crate::store::disabled::DisabledStore));
        fake.seed(
            &crate::model::ChannelId(READ_CHANNEL.to_owned()),
            "agent",
            "the arm64 job failed",
        );
        let listed = channels(&state).await;
        assert_eq!(listed.len(), 2, "the configured channels are still listed");
        assert!(listed.iter().all(|c| c.alias.is_none()));
        let window = messages(&state, READ_CHANNEL, None)
            .await
            .expect("a channel must stay readable with no store at all");
        assert_eq!(window.channel.display_name(), "build noise");
    }

    #[tokio::test]
    async fn a_store_that_fails_degrades_to_the_configured_label_instead_of_failing_the_read() {
        let (state, _fake, store) = testing::state_with_store();
        set_channel_alias(&state, READ_CHANNEL, "the build channel")
            .await
            .expect("set");
        let named = |list: &[ChannelInfo]| {
            list.iter()
                .find(|c| c.id.as_str() == READ_CHANNEL)
                .expect("configured")
                .display_name()
                .to_owned()
        };
        // The control, so the degraded answer below is not the only answer this can give.
        assert_eq!(named(&channels(&state).await), "the build channel");

        store.fail_next("the disk is full");
        assert_eq!(
            named(&channels(&state).await),
            "build noise",
            "a backend failure must degrade to the configured label, not take the listing down"
        );
    }

    #[test]
    fn every_refusal_has_a_stable_code() {
        assert_eq!(OpError::UnknownChannel.code(), "unknown_channel");
        assert_eq!(OpError::UnknownMessage.code(), "unknown_message");
        assert_eq!(OpError::EmptyQuery.code(), "empty_query");
        assert_eq!(OpError::ChannelNotWritable.code(), "channel_not_writable");
        assert_eq!(OpError::InvalidCursor.code(), "invalid_cursor");
        assert_eq!(OpError::InvalidRange.code(), "invalid_range");
        assert_eq!(OpError::CursorMismatch.code(), "cursor_mismatch");
        assert_eq!(
            OpError::Chat(ChatError::CursorExpired("gone".to_owned()).with_provider("Bridge"))
                .code(),
            "cursor_expired"
        );
        assert_eq!(
            OpError::Chat(ChatError::Refused("too long".to_owned())).code(),
            "refused"
        );
        assert_eq!(
            OpError::Chat(ChatError::Transport("down".to_owned())).code(),
            "chat_error"
        );
        assert_eq!(
            OpError::Chat(
                ChatError::Refused("unsupported".to_owned()).with_provider("Google Chat")
            )
            .code(),
            "refused"
        );
        assert_eq!(
            OpError::Chat(ChatError::Transport("down".to_owned()).with_provider("Google Chat"))
                .code(),
            "chat_error"
        );
    }

    /// `#195 send-resilience`. What a 207's `resumable` stands for, over messages of every shape
    /// and every part one could fail at: sending the unsent text again asks for the part that
    /// failed under its own words and key, and keys nothing as a part already posted, which a
    /// provider honouring keys would silently drop.
    #[test]
    fn a_resumable_remainder_asks_for_the_failed_part_as_the_same_request() {
        use crate::chat::{PostEscaping, PostLimit};
        use crate::discord::split::split_within;

        let prose = |n: usize| -> String {
            (0..n)
                .map(|i| format!("sentence {i} of the answer."))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let code = |lines: usize| format!("```rust\n{}```", "let answer = 42;\n".repeat(lines));
        let block = "a line of the same log output\n".repeat(66);
        let texts = [
            ("prose", prose(700)),
            (
                "fence then prose",
                format!("```sh\nls -l\n```\n\n{}", prose(500)),
            ),
            (
                "prose then fence",
                format!("{}\n\n{}", prose(300), code(200)),
            ),
            ("one long fence", code(600)),
            (
                "fence between prose",
                format!("{}\n\n{}\n\n{}", prose(150), code(150), prose(150)),
            ),
            ("repeated parts", format!("{block}\n{block}\n{block}")),
        ];
        // Discord's limit, and — `#263 post-split-per-provider` — the other shapes a provider's
        // limit takes: bytes, and characters counted after escaping.
        let discord = PostLimit::chars(crate::discord::http::DISCORD_MAX_CONTENT_LEN);
        let limits = [
            discord,
            PostLimit::bytes(3_000),
            PostLimit::chars(2_500).escaped(PostEscaping::HtmlEntities),
        ];
        let mut refused = Vec::new();
        for (limit, (name, text)) in limits
            .iter()
            .flat_map(|limit| texts.iter().map(move |text| (limit, text)))
        {
            let parts = split_within(text, limit);
            assert!(parts.len() > 2, "{name}: the fixture did not split");
            let keys = part_keys("outgoing-key", &parts);
            for index in 0..parts.len() {
                let (unsent, resumable) = unsent_remainder(text, &parts, index, false, limit);
                if index == 0 {
                    assert_eq!(
                        &unsent, text,
                        "{name}: nothing posted, yet the text changed"
                    );
                    assert!(resumable, "{name}: the same request was called unsafe");
                }
                if !resumable {
                    if *limit == discord {
                        refused.push((*name, index));
                    }
                    continue;
                }
                let retry = split_within(&unsent, limit);
                let again = part_keys("outgoing-key", &retry);
                assert_eq!(
                    retry[0], parts[index],
                    "{name}: retrying from part {index} cuts the failed part differently"
                );
                assert_eq!(
                    again[0], keys[index],
                    "{name}: retrying from part {index} asks for the failed part under another key"
                );
                assert!(
                    !again.iter().any(|key| keys[..index].contains(key)),
                    "{name}: retrying from part {index} would collapse into a posted part"
                );
            }
        }
        // The cases the reviewer of `#194` found: prose behind a fence that already posted is
        // resumed; a part opened and closed inside a split fence, and a part repeated among those
        // posted, are not.
        assert!(
            !refused
                .iter()
                .any(|(name, _)| *name == "prose" || *name == "fence then prose"),
            "a remainder that splits the same way was left to the reader: {refused:?}"
        );
        assert!(refused.contains(&("one long fence", 1)), "{refused:?}");
        assert!(refused.contains(&("repeated parts", 1)), "{refused:?}");
        // A reply's remainder would go again with the reply target on its first part.
        let text = prose(700);
        let parts = split_within(&text, &discord);
        assert!(!unsent_remainder(&text, &parts, 1, true, &discord).1);
        assert!(unsent_remainder(&text, &parts, 0, true, &discord).1);
    }

    // --- `#190 voice-agent-tools` ----------------------------------------------------------------

    /// A fixed clock for the activity tests: 2026-10-07T07:52:00Z, the morning of the report.
    const NOW: i64 = 1_791_359_520_000;
    const MINUTE: i64 = 60_000;

    fn at(fake: &crate::discord::fake::FakeDiscord, channel: &str, text: &str, ago_ms: i64) {
        fake.seed_at(
            &crate::model::ChannelId(channel.to_owned()),
            "agent",
            text,
            NOW - ago_ms,
        );
    }

    fn request(hours: Option<u64>) -> ActivityRequest {
        ActivityRequest {
            hours,
            per_channel_limit: None,
            now_ms: NOW,
        }
    }

    fn part<'a>(activity: &'a Activity, channel: &str) -> &'a WindowActivity {
        activity
            .channels
            .iter()
            .find(|c| c.channel.id.as_str() == channel)
            .unwrap_or_else(|| panic!("{channel} was not read"))
            .read
            .as_ref()
            .unwrap_or_else(|e| panic!("{channel} failed: {e}"))
    }

    fn summaries(read: &WindowActivity) -> Vec<&str> {
        read.entries.iter().map(|e| e.summary.as_str()).collect()
    }

    #[tokio::test]
    async fn activity_reads_every_channel_once_and_names_the_newest_message() {
        let (state, fake) = testing::state();
        at(&fake, READ_CHANNEL, "yesterday's news", 30 * HOUR_MS);
        at(&fake, READ_CHANNEL, "three hours ago", 3 * HOUR_MS);
        at(&fake, WRITE_CHANNEL, "an hour ago", HOUR_MS);
        at(&fake, WRITE_CHANNEL, "twenty minutes ago", 20 * MINUTE);
        let before = fake.fetch_count();

        let activity = activity(&state, request(None)).await.expect("reads");

        assert_eq!(
            fake.fetch_count() - before,
            2,
            "one fetch per channel, and no list or clock round before them"
        );
        assert_eq!(activity.hours, DEFAULT_ACTIVITY_HOURS);
        assert_eq!(activity.since_ms, NOW - 24 * HOUR_MS);
        let read = part(&activity, READ_CHANNEL);
        assert_eq!(summaries(read), ["three hours ago"]);
        assert_eq!(read.counted, 1);
        assert!(read.complete, "the read reached past the window's start");
        let write = part(&activity, WRITE_CHANNEL);
        assert_eq!(
            summaries(write),
            ["an hour ago", "twenty minutes ago"],
            "oldest first, newest last, like every digest"
        );
        let (channel, newest) = activity.newest().expect("there is a newest message");
        assert_eq!(channel.id.as_str(), WRITE_CHANNEL);
        assert_eq!(newest.entry.summary, "twenty minutes ago");
        assert_eq!(newest.at_ms, NOW - 20 * MINUTE);
        assert!(activity.covers(newest.at_ms));
    }

    #[tokio::test]
    async fn the_window_leaves_older_messages_out_and_a_quiet_day_still_names_the_newest() {
        let (state, fake) = testing::state();
        at(&fake, READ_CHANNEL, "two days ago", 50 * HOUR_MS);

        let day = activity(&state, request(None)).await.expect("reads");
        let read = part(&day, READ_CHANNEL);
        assert_eq!(read.counted, 0);
        assert!(read.entries.is_empty(), "{:?}", summaries(read));
        assert!(read.complete);
        let empty = part(&day, WRITE_CHANNEL);
        assert_eq!((empty.counted, empty.noise), (0, 0));
        assert!(empty.newest.is_none());
        let (channel, newest) = day.newest().expect("the old message is still the newest");
        assert_eq!(channel.id.as_str(), READ_CHANNEL);
        assert!(
            !day.covers(newest.at_ms),
            "it is outside the window, and must be reported as older"
        );

        let three_days = activity(&state, request(Some(72))).await.expect("reads");
        assert_eq!(summaries(part(&three_days, READ_CHANNEL)), ["two days ago"]);
    }

    #[tokio::test]
    async fn activity_leaves_placeholders_out_and_says_how_many() {
        let (state, fake) = testing::state();
        at(&fake, READ_CHANNEL, "the real answer", 2 * HOUR_MS);
        at(&fake, READ_CHANNEL, "Working…", HOUR_MS);

        let activity = activity(&state, request(None)).await.expect("reads");
        let read = part(&activity, READ_CHANNEL);
        assert_eq!(summaries(read), ["the real answer"]);
        assert_eq!((read.counted, read.noise), (1, 1));
        let (_, newest) = activity.newest().expect("newest");
        assert_eq!(
            newest.entry.summary, "the real answer",
            "a placeholder is never the newest message worth naming"
        );
    }

    #[tokio::test]
    async fn activity_says_at_least_when_its_read_stops_inside_the_window() {
        let (state, fake) = testing::state();
        // Thirty messages inside the window, against a default fetch of twenty.
        for i in 0..30 {
            at(&fake, READ_CHANNEL, &format!("m{i}"), (60 - i) * MINUTE);
        }
        let activity = activity(&state, request(None)).await.expect("reads");
        let read = part(&activity, READ_CHANNEL);
        assert!(
            !read.complete,
            "the fetch never reached the start of the window, so its count is a lower bound"
        );
        assert_eq!(read.counted, 20);
        assert_eq!(
            summaries(read),
            ["m22", "m23", "m24", "m25", "m26", "m27", "m28", "m29"],
            "the newest eight, oldest first"
        );
    }

    #[tokio::test]
    async fn activity_clamps_its_window_and_line_count_and_records_what_was_asked() {
        let (state, fake) = testing::state();
        for i in 0..40 {
            at(&fake, READ_CHANNEL, &format!("m{i}"), (i + 1) * MINUTE);
        }
        for (asked, hours, recorded) in [
            (Some(0), 1, Some(0)),
            (Some(1_000), MAX_WINDOW_HOURS, Some(1_000)),
            (Some(24), 24, None),
            (None, DEFAULT_ACTIVITY_HOURS, None),
        ] {
            let read = activity(&state, request(asked)).await.expect("reads");
            assert_eq!(
                (read.hours, read.asked_hours),
                (hours, recorded),
                "{asked:?}"
            );
        }
        for (asked, lines) in [(Some(0), 1), (Some(500), MAX_ACTIVITY_LINES), (None, 8)] {
            let read = activity(
                &state,
                ActivityRequest {
                    per_channel_limit: asked,
                    ..request(None)
                },
            )
            .await
            .expect("reads");
            assert_eq!(read.per_channel_limit, lines, "{asked:?}");
            assert_eq!(
                part(&read, READ_CHANNEL).entries.len(),
                usize::from(lines).min(40),
                "{asked:?}"
            );
        }
    }

    #[tokio::test]
    async fn one_channel_failing_leaves_the_others_answered_and_all_failing_is_an_error() {
        let toml = testing::config_toml().replace(
            "[elevenlabs]",
            "[[channels]]\nid = \"3333333333\"\nlabel = \"gone\"\nwritable = false\n\n[elevenlabs]",
        );
        let (state, fake, _elevenlabs) = testing::state_from_toml(&toml);
        at(&fake, READ_CHANNEL, "still here", HOUR_MS);
        let activity = activity(&state, request(None))
            .await
            .expect("two of three read");
        assert_eq!(activity.channels.len(), 3);
        let gone = activity
            .channels
            .iter()
            .find(|c| c.channel.id.as_str() == "3333333333")
            .expect("listed");
        assert!(gone.read.is_err(), "the unreadable channel says so");
        assert_eq!(summaries(part(&activity, READ_CHANNEL)), ["still here"]);

        // A server that holds nothing to fall back on. One that does answers from it, and says
        // so: see `tests/voice_catchup.rs`. `#210 voice-catchup-speed`.
        let (state, fake, _elevenlabs) = testing::state_from_toml(&toml);
        fake.revoke_token();
        let error = super::activity(&state, request(None))
            .await
            .expect_err("an answer made only of failures is a failure");
        assert_eq!(error.code(), "chat_error", "{error}");
    }

    #[tokio::test]
    async fn activity_over_an_empty_list_is_an_answer_not_an_error() {
        let (state, _fake) = testing::state();
        if let Ok(mut hidden) = state.hidden_channels.write() {
            hidden.insert(crate::model::ChannelId(READ_CHANNEL.to_owned()));
            hidden.insert(crate::model::ChannelId(WRITE_CHANNEL.to_owned()));
        }
        let activity = activity(&state, request(None))
            .await
            .expect("nothing to read");
        assert!(activity.channels.is_empty());
        assert!(activity.newest().is_none());
    }

    #[tokio::test]
    async fn a_relative_window_reads_from_that_many_hours_ago_to_now() {
        let (state, fake) = testing::state();
        let channel = crate::model::ChannelId(READ_CHANNEL.to_owned());
        let now = now_ms();
        fake.seed_at(&channel, "agent", "yesterday morning", now - 30 * HOUR_MS);
        fake.seed_at(&channel, "agent", "two hours ago", now - 2 * HOUR_MS);
        let step = page(
            &state,
            READ_CHANNEL,
            PageRequest {
                hours: Some(24),
                ..PageRequest::default()
            },
        )
        .await
        .expect("reads");
        assert_eq!(
            step.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["two hours ago"]
        );
        let tally = count_within(&state, READ_CHANNEL, None, Some(24), None)
            .await
            .expect("counts");
        assert_eq!((tally.counted, tally.at_least), (1, false));
        let tally = count_within(&state, READ_CHANNEL, Some("  "), Some(48), None)
            .await
            .expect("a blank since is absent, not a second window");
        assert_eq!(tally.counted, 2);
    }

    #[tokio::test]
    async fn a_relative_window_stands_alone_and_inside_its_range() {
        let (state, _fake) = testing::state();
        let cases: [(PageRequest<'_>, &str); 6] = [
            (
                PageRequest {
                    hours: Some(24),
                    since: Some("2026-08-19T00:00:00Z"),
                    ..PageRequest::default()
                },
                "invalid_range",
            ),
            (
                PageRequest {
                    hours: Some(24),
                    before: Some("1000000000000000001"),
                    ..PageRequest::default()
                },
                "invalid_range",
            ),
            (
                PageRequest {
                    hours: Some(24),
                    until: Some("2026-08-19T00:00:00Z"),
                    ..PageRequest::default()
                },
                "invalid_range",
            ),
            (
                PageRequest {
                    hours: Some(0),
                    ..PageRequest::default()
                },
                "invalid_hours",
            ),
            (
                PageRequest {
                    hours: Some(MAX_WINDOW_HOURS + 1),
                    ..PageRequest::default()
                },
                "invalid_hours",
            ),
            (
                PageRequest {
                    hours: Some(u64::MAX),
                    ..PageRequest::default()
                },
                "invalid_hours",
            ),
        ];
        for (request, code) in cases {
            let error = page(&state, READ_CHANNEL, request)
                .await
                .expect_err("must refuse");
            assert_eq!(error.code(), code, "{request:?} -> {error}");
        }
        let error = count_within(
            &state,
            READ_CHANNEL,
            Some("2026-08-19T00:00:00Z"),
            Some(24),
            None,
        )
        .await
        .expect_err("two windows at once");
        assert_eq!(error.code(), "invalid_range");
        assert!(
            error.to_string().contains("give hours on its own"),
            "{error}"
        );
        let error = count_within(&state, READ_CHANNEL, None, Some(169), None)
            .await
            .expect_err("past a week");
        assert_eq!(
            error.to_string(),
            "hours must be a whole number from 1 to 168"
        );
    }

    /// A message with only an id, which is all [`reply_dismissals`] reads of one.
    fn positioned(id: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": id, "channel_id": WRITE_CHANNEL, "author": "agent", "author_id": "1",
            "author_is_bot": true, "timestamp": "2026-10-08T07:00:00Z", "content": "",
        }))
        .expect("a message")
    }

    fn ids(raw: &[&str]) -> Vec<MessageId> {
        raw.iter().map(|id| MessageId((*id).to_owned())).collect()
    }

    #[test]
    fn reply_dismissals_are_every_mark_from_the_pages_oldest_row_up_by_position_newest_first() {
        // `#222 unread-replies`. "9" is below "10" as a number and above it as a string: the cut is
        // by number, as the store orders marks.
        let page = [positioned("100"), positioned("10"), positioned("300")];
        let marks = ids(&["9", "1000", "10", "250", "5"]);
        let got = reply_dismissals(&marks, &page).expect("a page with positions");
        assert_eq!(
            got.from,
            MessageId("10".to_owned()),
            "complete from the oldest row"
        );
        assert_eq!(got.messages, ids(&["1000", "250", "10"]));
    }

    #[test]
    fn reply_dismissals_past_the_cap_keep_the_newest_and_say_where_they_start() {
        let page = [positioned("1")];
        let marks: Vec<MessageId> = (1..=MAX_REPLY_DISMISSALS as u64 + 5)
            .map(|n| MessageId(n.to_string()))
            .collect();
        let got = reply_dismissals(&marks, &page).expect("a page with positions");
        assert_eq!(got.messages.len(), MAX_REPLY_DISMISSALS);
        assert_eq!(
            got.messages[0],
            MessageId((MAX_REPLY_DISMISSALS as u64 + 5).to_string())
        );
        // From 6 up every mark is kept; 1 to 5 were cut, so the list no longer speaks for them.
        assert_eq!(got.from, MessageId("6".to_owned()));
        assert_eq!(got.messages.last(), Some(&got.from));
    }

    #[test]
    fn reply_dismissals_say_nothing_for_a_page_without_positions() {
        assert_eq!(reply_dismissals(&ids(&["5"]), &[]), None, "an empty page");
        assert_eq!(
            reply_dismissals(&ids(&["5"]), &[positioned("spaces/A/messages/x")]),
            None,
            "ids that are not numbers"
        );
        let none = reply_dismissals(&[], &[positioned("7")]).expect("a page with positions");
        assert!(
            none.messages.is_empty(),
            "nothing marked is an empty list, not an absent one"
        );
    }
}
