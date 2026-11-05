//! The public wire contract: the JSON this server, its browser page, and `vibe-talk-v1` peers
//! exchange.
//!
//! Rust is the authority. Every type here derives `serde` for the bytes that really cross the wire
//! and [`JsonSchema`] for a description of those same bytes, and [`schema_document`] collects the
//! contract's roots into one JSON Schema document. That document is checked in at
//! `contract/vibe-talk.schema.json`; the browser's TypeScript declarations and its runtime
//! validators are generated from it (`make -C vibe-talk contract`), and `tests/contract.rs` fails
//! when the checked-in copy no longer matches these types. So a renamed field, a field made
//! optional, or a new union variant is a reviewable diff in three generated files rather than a
//! property the page silently stops finding.
//!
//! The schema describes what a SENDER emits (`schemars`' serialize contract): a field serde always
//! writes is required, and one it skips when empty is optional. Unknown properties are permitted
//! throughout, so adding a field stays backward compatible; removing one, making one optional,
//! or changing a tag is a protocol change. So is a new value of an enum such as [`TokenScope`],
//! [`LiveDelivery`], or [`TranscriptRole`]: a page loaded before it refuses every answer or frame
//! that carries it. That matters most for [`TranscriptRole`], which peers outside this repository
//! send. A new `vibe-talk-v1` frame TYPE is the exception, because a page ignores a type it does
//! not know.
//!
//! Some contract types live beside the code that builds them — [`crate::model::Message`],
//! [`crate::threads::ThreadSummary`], [`crate::conversation::VoiceSession`] — and are part of the
//! contract because a root here reaches them. The HTTP bodies the page decodes, the live stream's
//! event payloads, and the `vibe-talk-v1` frames are defined here, where the whole public surface
//! can be read in one place.

use schemars::generate::SchemaSettings;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{ChannelId, ChannelInfo, Message, MessageId};

/// A JSON error body.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ApiErrorBody {
    /// Stable machine-readable code.
    pub error: &'static str,
    /// Human-readable detail. Never contains a secret.
    pub detail: String,
}

/// What the web app needs to know at startup.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ClientConfigResponse {
    /// Source chat service's human-readable name, supplied by the selected backend.
    pub chat_provider_name: String,
    /// Configured channels.
    pub channels: Vec<ChannelInfo>,
    /// Channels from the configuration file the owner took off his list, in configuration order.
    /// `#199 removable-config-channels`.
    ///
    /// Settings lists them so each can be shown again; `channels` above already leaves them out.
    /// Sent only to a write-scope caller, because showing one again takes that scope and a
    /// read-scope credential has no use for the names of channels the owner put away. Absent when
    /// none are hidden, and from an older server, which could not hide one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_channels: Vec<ChannelInfo>,
    /// ElevenLabs agent id, when the deployment has one. Not a secret: it identifies a public
    /// widget. The API key never leaves the server.
    pub elevenlabs_agent_id: Option<String>,
    /// Selected conversational voice provider, named by its implementation.
    pub conversational_voice: crate::conversation::VoiceDescription,
    /// Selected read-aloud backend and the playback interface the browser should use.
    pub read_aloud: crate::speech::Description,
    /// Server version, so a stale cached page is visible.
    pub version: &'static str,
    /// Whether a new call will be handed the earlier transcript: the operator allowed it AND the
    /// voice backend can carry it (a `vibe-talk-v1` socket cannot; see `replay::offered`).
    ///
    /// The page needs it BEFORE it has anything to replay: the Settings screen has to describe
    /// what resuming will do, and the control's own note has to say what the next call will be —
    /// both of which are questions asked while there is no conversation to fetch.
    pub replay_enabled: bool,
    /// The Discord account this bridge posts as, when it can be known.
    ///
    /// NOT a secret: a bot's user id is visible on every message it has ever sent. It is here so
    /// the channel view can tell the owner's own words -- posted on his behalf by this bridge --
    /// from everybody else's, on the FIRST render rather than only after he has replied from the
    /// app. `None` when the token is not in the shape ids can be read from; the page then falls
    /// back to learning it the old way.
    pub self_author_id: Option<String>,
    /// The READER's own Discord account, when the operator has said what it is.
    ///
    /// Distinct from `self_author_id`, which is the BRIDGE's account and is read out of the bot
    /// token. This one cannot be derived from anything the server holds — a bot's account has no
    /// relationship to the human reading the channel — so it is absent unless configured. The page
    /// draws both as the owner's own words, because they are: one typed into Discord, one dictated
    /// through this bridge.
    pub owner_author_id: Option<String>,
    /// Seconds between live ingestion ticks, or `0` when live ingestion is OFF.
    ///
    /// The page needs this to tell the truth about its own channel view. With ingestion off the
    /// SSE stream attaches and delivers nothing, which is indistinguishable from a quiet channel;
    /// a page that showed a "live" indicator on that basis would be claiming a freshness it does
    /// not have. Not a secret: it is a property of this deployment, and the caller already holds
    /// a token.
    pub live_poll_seconds: u64,
    /// How changes reach the SSE hub.
    pub live_delivery: LiveDelivery,
    /// Whether the selected provider accepts channel links or references and manages registration.
    pub channel_registration_supported: bool,
    /// Whether the selected provider can list its channels for browsing. `#19 channel-browser`.
    pub channel_discovery_supported: bool,
    /// Whether the selected provider exposes a write-through read cursor.
    pub upstream_read_mark_supported: bool,
    /// Whether the selected provider can clear its own unread state as this server ingests
    /// messages, for a channel the owner turns that on for. `#232 upstream-mark-read`.
    ///
    /// Settings offers the per-channel control only when this is true. Sent only when true:
    /// absent, from this server or an older one, means false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub upstream_clear_supported: bool,
    /// Whether the backend supports channel, thread-list, and flattened timelines.
    pub threading_supported: bool,
    /// Whether a reply to a main-channel message is posted in that message's thread, starting one
    /// when it has none, because the provider has no reply pointer of its own (Slack).
    ///
    /// `#252 thread-button-state`. Such a reply already starts the message's thread, so the reply
    /// screen offers no separate "start a new thread" choice when this is true. Sent only when
    /// true: absent, from this server or an older one, means false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replies_in_thread: bool,
    /// Whether a post sent again under the same `idempotency_key` is posted at most once.
    ///
    /// `#195 send-resilience`. The page retries a send whose answer it lost — a timeout, a 502 —
    /// on its own only when this is true, because only then can the retry not post the message
    /// twice. Sent only when true: absent, from this server or an older one, means false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idempotent_posts_supported: bool,
    /// Whether this server rewrites message bodies for speech before anything says them.
    ///
    /// Here for the same reason as `replay_enabled`: Settings has to be able to describe what this
    /// deployment does, and the page cannot infer this one from any message it holds. An empty
    /// `spoken_content` means "the rewrite changed nothing" just as often as it means "the pass is
    /// off" — deliberately, see [`crate::model::Message::spoken_content`] — so a page guessing from
    /// the data would tell the operator the feature was off every time the channel was plain prose.
    /// They are flipping this switch to compare two runs; guessing is exactly what they cannot do.
    pub speech_prep_enabled: bool,
    /// Each configured chat provider and what it can do, in configuration order.
    ///
    /// Every channel names its provider by key ([`ChannelInfo::provider`]). The deployment-wide
    /// flags above say whether ANY provider can do something; a page showing one channel uses
    /// that channel's provider entry here instead, so a channel read through a provider without
    /// threads is never offered a thread view because some other provider has one.
    ///
    /// Sent only when there are several providers. With one, and from an older server, it is
    /// absent and the deployment-wide flags are that provider's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderDescription>,
    /// Why message summaries cannot work on this deployment at all, when the server knows.
    ///
    /// Present means the page disables the summaries control and says this instead of letting
    /// every long row fail one request at a time. Absent means summaries may work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summaries_unavailable: Option<String>,
    /// The scope of the token that asked.
    ///
    /// The caller's own scope, so it discloses nothing it did not already hold. The page needs it
    /// to avoid asking for what this token cannot have: stored conversations are write-only (see
    /// the conversation routes), and a read-scope page that probed them anyway got a 403 and a
    /// console error on every load. `#38 read-token-conversation-probe`.
    pub token_scope: TokenScope,
    /// The rules that decide which messages are read automatically. `#196 auto-read-noise`.
    ///
    /// Here rather than behind a route of its own because the page needs them at startup, for the
    /// Settings editor, and every device and the voice agent share one list. Absent from an older
    /// server — the page then hides the editor — and also when this one cannot read its store, so
    /// a broken store makes the editor disappear rather than show a list that is not in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_rules: Option<NoiseRules>,
    /// "Ask me before the voice agent sends", as Settings shows it. `#264 voice-send-approval`.
    ///
    /// Here so the checkbox is drawn at sign-in, as the noise rules are. Readable at read scope —
    /// it says how the voice agent behaves, which a reader of the channels may know — and changed
    /// only by a write-scope `PUT /api/v1/voice-settings`. Absent from an older server, which
    /// always asked; the page then hides the checkbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_settings: Option<VoiceSettings>,
    /// The owner's own words for the voice agent, as Settings shows them. `#272
    /// voice-agent-instructions`.
    ///
    /// Here so the Instructions editor is drawn at sign-in, as the send setting is. Readable at
    /// read scope — the assembled prompt is served there too, to a bridge holding the read token —
    /// and changed only by a write-scope `PUT /api/v1/voice-prompt`. Absent from an older server;
    /// the page then hides the editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_prompt: Option<VoicePromptSettings>,
}

/// The owner's own words for the voice agent, as Settings shows them. `#272
/// voice-agent-instructions`.
///
/// The body of `GET` and `PUT /api/v1/voice-prompt`, and `voice_prompt` in client-config. Both
/// texts join only the scope-neutral part of the prompt: what the agent can do — read, send, or
/// ask first — is added by the server from the caller's token and the send setting, after them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct VoicePromptSettings {
    /// Additional instructions, added to every call. Empty for none.
    pub instructions: String,
    /// The base prompt in force: the owner's edited copy when `system_prompt_edited`, otherwise
    /// the shipped one, so the editor always opens on what the agent is actually given.
    pub system_prompt: String,
    /// Whether `system_prompt` is the owner's edited copy.
    pub system_prompt_edited: bool,
    /// The prompt version shipped when he last saved his copy; present exactly when he has one.
    /// Behind `prompt_version` means the shipped prompt has changed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_from_version: Option<u32>,
    /// The version of the prompt this server ships.
    pub prompt_version: u32,
    /// The longest `instructions` a save may carry, in characters.
    pub max_instructions_chars: u32,
    /// The longest `system_prompt` a save may carry, in characters.
    pub max_system_prompt_chars: u32,
    /// Whether this server can keep a change. False with no store configured, and when the store
    /// cannot be read — the agent then gets the shipped prompt.
    pub changeable: bool,
    /// How these words reach this deployment's voice agent.
    pub delivery: PromptDelivery,
}

/// How the owner's words reach the voice agent. `#272 voice-agent-instructions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptDelivery {
    /// A deployment-managed bridge fetches the assembled prompt from `GET /api/v1/voice-agent` as
    /// each call opens, so both texts reach its next call.
    Profile,
    /// The page hands them to a hosted agent as each call opens: the whole assembled prompt when
    /// the agent allows its prompt to be replaced, otherwise the instructions alone, as a note at
    /// the start. The voice session says which ([`SessionPrompt`]).
    Page,
}

/// What the page hands a hosted agent as a call opens, so the call carries the owner's words.
/// `#272 voice-agent-instructions`.
///
/// On every voice session whose provider is one the page configures, whatever the owner has saved.
/// An agent that allows its prompt to be replaced is handed the assembled prompt on every call, so
/// its send rules follow the Sending switch; one that does not is told `not_sent` when nothing of
/// the owner's can reach it, so the page learns that the agent keeps the prompt in its provider's
/// console. Absent for a provider that reads the profile instead (a deployment-managed bridge), and
/// from an older server; the page then sends exactly what it always sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SessionPrompt {
    /// How to hand `text` over.
    pub transport: PromptTransport,
    /// What to send: the whole assembled prompt for `prompt_override` — the shipped one when the
    /// owner saved nothing — the heading and the instructions for `contextual_update`; absent for
    /// `not_sent`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Whether the owner's edited base prompt is in what is sent.
    pub system_prompt_applied: bool,
    /// Whether the owner's additional instructions are in what is sent.
    pub instructions_applied: bool,
    /// What the agent's configuration said about replacing its prompt; absent when it could not
    /// be read or did not say, which is treated as not allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_allowed: Option<bool>,
}

/// How the page hands the owner's words to a hosted agent. `#272 voice-agent-instructions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptTransport {
    /// As the system prompt itself, in the initiation frame's
    /// `conversation_config_override.agent.prompt.prompt`. Only when the agent allows it: an
    /// override it does not allow is believed to end the conversation.
    PromptOverride,
    /// As a `contextual_update` sent right after the initiation frame. The agent keeps its own
    /// prompt; only the instructions reach it.
    ContextualUpdate,
    /// Nothing: the agent does not allow its prompt to be replaced (or its configuration did not
    /// say, or could not be read), and the owner has no instructions saved — he saved nothing, or
    /// edited only the base prompt. The initiation frame is exactly what it always was, and the
    /// agent keeps the prompt in its provider's console.
    NotSent,
}

/// Whether the voice agent must ask before it sends. `#264 voice-send-approval`.
///
/// The body of `GET` and `PUT /api/v1/voice-settings`, and `voice_settings` in client-config.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct VoiceSettings {
    /// The value in force: true means `post_reply` proposes and the owner confirms, by a tap or by
    /// a plain yes the voice page hears; false means `post_reply` posts at once. The owner's saved
    /// choice, or the configured default until he makes one.
    pub ask_before_send: bool,
    /// Whether this server can keep a change. False with no store configured, and when the store
    /// cannot be read — the server then asks, whatever the configured default says.
    pub changeable: bool,
}

/// The owner's noise rules, as the Settings editor shows them. `#196 auto-read-noise`.
///
/// Also the body `PUT /api/v1/noise-rules` answers with, so the editor redraws from what the
/// server STORED — trimmed and de-duplicated — rather than from what was typed.
#[derive(Debug, Serialize, JsonSchema)]
pub struct NoiseRules {
    /// The rules in force, in the owner's order and his own spelling. Until he saves a list this
    /// is the shipped default.
    pub rules: Vec<String>,
    /// One sentence saying how a rule matches, written beside the matcher and quoted by the page
    /// where the rules are edited, so the description cannot drift from what the server does.
    pub matching: &'static str,
}

/// `GET /api/v1/channels/{id}/messages`: a channel's most recent messages, the window the classic
/// page draws.
///
/// `#208 persistent-cache` made it a contract root, appended below, when it gained `as_of`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct MessagesResponse {
    /// Channel that was read.
    pub channel: ChannelInfo,
    /// Messages, oldest first.
    pub messages: Vec<Message>,
    /// Whether the fetch came back short, so `messages.len()` counts the CHANNEL rather than the
    /// window this server asked for.
    ///
    /// A client may show a message count only when this is true. Discord gives no message count
    /// for a guild text channel, so a full window means "at least this many" and nothing more.
    pub complete: bool,
    /// Which of `messages` the reader has ARCHIVED, so the view can dim them.
    ///
    /// The ordinary channel view shows archived rows greyed rather than hidden -- hiding is what
    /// the To do filter is for -- and it can only do that if the payload says which they are.
    pub dismissed: Vec<MessageId>,
    /// The instant, RFC 3339, up to which the provider knows `messages` is complete, when it
    /// served a copy it could not bring up to date just now: a bridge reading its Flat view says
    /// so. Absent means now, and from an older server. The same field as
    /// [`crate::threads::TimelinePage::as_of`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// Standing reminder that the content is third-party text.
    pub untrusted_content_notice: &'static str,
}

/// A provider-neutral timeline plus this application's channel and read-state metadata.
#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineResponse {
    /// Configured parent channel, including its local alias and write policy.
    pub channel: ChannelInfo,
    /// View represented by this page.
    pub view: crate::threads::TimelineView,
    /// Page size actually requested.
    pub limit: u16,
    /// Entries on this page, never a channel-wide total.
    pub returned: usize,
    /// Thread and message data, ordered by the backend.
    #[serde(flatten)]
    pub page: crate::threads::TimelinePage,
    /// Messages on this page that the reader has archived locally.
    pub dismissed: Vec<MessageId>,
    /// `#222 unread-replies`. On the newest page of Main, or a delta of it, in a channel with
    /// threads: every message marked Done from a position on, the replies in Main's threads
    /// included. Absent everywhere else.
    ///
    /// `dismissed` names only the rows on the page, and Main shows a thread's root but none of its
    /// replies, so without this a page in Main could not learn that a reply it holds from All was
    /// marked Done, or put back, on another device. See [`ReplyDismissals`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_dismissals: Option<ReplyDismissals>,
    /// The revision this channel's pins are at. `#206 pin-message`.
    ///
    /// On every read, newest page and delta alike, so a refresh that already happens is what tells
    /// a page another device pinned or unpinned something; the page reads `/pins` again only when
    /// this differs from the revision of the list it holds. Absent when the store cannot say — no
    /// storage configured, or a failing one — and from an older server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pins_revision: Option<i64>,
    /// This channel's bookmark and its revision. `#240 reading-bookmark`.
    ///
    /// On every read, newest page and delta alike, as `pins_revision` is, and for its reason: a
    /// refresh that already happens is how a page learns the owner moved the bookmark on his other
    /// device. ONE small row, so it rides inline rather than as a revision and a second read.
    /// Absent when the store cannot say, and the page keeps what it has; `anchor: null` inside it
    /// is the different answer that no bookmark is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bookmark: Option<crate::store::Bookmark>,
    /// Channel content remains untrusted data in every view.
    pub untrusted_content_notice: &'static str,
}

/// Every message marked Done at or after one position in a channel. `#222 unread-replies`.
///
/// COMPLETE FROM `from` UP, which is the whole of its use: a message at or after `from` that is
/// not in `messages` is not Done, so a reply put back on another device leaves the list as surely
/// as one marked Done joins it. Below `from` it says nothing. A position is a message id read as a
/// number, which every provider here orders by time, so a reply is never before its thread's root
/// and every reply to a root at or after `from` is covered.
///
/// `from` is the page's oldest row, raised to the oldest mark kept when more than
/// [`crate::ops::MAX_REPLY_DISMISSALS`] marks lie above it: the newest are kept, because unread
/// replies are the recent ones. Ids and no text, as `dismissed` is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ReplyDismissals {
    /// The position the list is complete from: a message id, compared as a number.
    pub from: MessageId,
    /// Every message at or after `from` that is marked Done, newest first.
    pub messages: Vec<MessageId>,
}

/// Whether this server clears one channel's unread state at its chat provider, and what it last
/// did: `GET` and `PUT /api/v1/channels/{id}/upstream-clear`. `#232 upstream-mark-read`.
///
/// Everything here is the server's own record. The provider's cursor is never read back except
/// as the answer to a push, so `pushed` and `last` say what this server did, not what the owner
/// has since read in the provider's own client.
#[derive(Debug, Serialize, JsonSchema)]
pub struct UpstreamClearState {
    /// The channel it is about.
    pub channel: ChannelInfo,
    /// The name of the channel's chat provider, for the words on the control.
    pub provider_name: String,
    /// Whether the channel's provider can do this at all. When false the mode is always off.
    pub supported: bool,
    /// What the owner chose. Off unless set.
    pub mode: crate::store::UpstreamClearMode,
    /// When it was turned on or to a dry run, in milliseconds since the Unix epoch, while it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_at_ms: Option<i64>,
    /// The newest push the provider's cursor is known to cover because of this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<UpstreamClearAttemptView>,
    /// The provider's cursor before this server first moved it, for a manual undo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_cursor: Option<String>,
    /// The most recent attempt of any kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<UpstreamClearAttemptView>,
    /// Failed attempts in a row.
    pub failures: u32,
    /// Whether a message is waiting to be pushed.
    pub pending: bool,
    /// When the next retry is due, in milliseconds since the Unix epoch, after a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at_ms: Option<i64>,
    /// Why the provider cannot do this for this channel, once it has said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

/// What Settings' "Mark read now" came to: `POST /api/v1/channels/{id}/upstream-clear/now`.
/// `#253 upstream-clear-polish`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct UpstreamClearNowResponse {
    /// What happened to the press.
    pub outcome: UpstreamClearNowOutcome,
    /// The channel's newest message, the one the press asked to push. Absent with `no_message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// That message's own instant, in milliseconds since the Unix epoch, when it can be read. An
    /// attempt recorded later whose `message_at_ms` is at or after it answers the press, which is
    /// how a page told `waiting` recognises the answer when it reads the state again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_at_ms: Option<i64>,
    /// The recorded push that answered the press, with `pushed`, `failed` and `skipped`, and with
    /// `unavailable` when the push itself was refused: the pressed message's own, or a newer
    /// message's, which covers it. What the press came to is said from this, never from
    /// `state.last`, which a later push may already have replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<UpstreamClearAttemptView>,
    /// The channel's state after it, as `GET .../upstream-clear` would answer now.
    pub state: UpstreamClearState,
}

/// What happened to a press of "Mark read now". `#253 upstream-clear-polish`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamClearNowOutcome {
    /// The press's push was made and the provider took it: `attempt` says whether its marker
    /// moved, was already there, or, in a dry run, what a real push would have done.
    Pushed,
    /// The press's push was made and failed; `attempt.error` says why. It is retried by itself, at
    /// `state.retry_at_ms`.
    Failed,
    /// The bridge no longer holds the message the push named, so nothing was marked; the next
    /// message that arrives is pushed.
    Skipped,
    /// An earlier push, or an earlier dry run, already covers the newest message. Nothing was sent.
    Covered,
    /// The channel has no message that can be marked.
    NoMessage,
    /// The channel is waiting out a failure, from before the press or of an older push that was
    /// already on its way, and the newest message goes when the retry is due, at
    /// `state.retry_at_ms`. Nothing was sent for the press.
    RetryWaiting,
    /// The push is queued or on its way, and had not been recorded when the server stopped
    /// waiting, at most 45 seconds after the press. The server keeps going without the page.
    Waiting,
    /// The channel's setting changed while the press waited (turned off, set to the other mode,
    /// or every setting erased), or the channel stopped being one this server can clear, so the
    /// push was not made as asked. `state` says what holds now.
    Changed,
    /// The provider refused this channel; `state.unavailable` says why.
    Unavailable,
}

impl UpstreamClearNowOutcome {
    /// The wire name, for log lines.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pushed => "pushed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Covered => "covered",
            Self::NoMessage => "no_message",
            Self::RetryWaiting => "retry_waiting",
            Self::Waiting => "waiting",
            Self::Changed => "changed",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One push, as Settings shows it. `#232 upstream-mark-read`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct UpstreamClearAttemptView {
    /// When it was made, in milliseconds since the Unix epoch.
    pub at_ms: i64,
    /// The newest ingested message it was for.
    pub message_id: MessageId,
    /// That message's instant, in milliseconds since the Unix epoch.
    pub message_at_ms: i64,
    /// What it came to.
    pub result: crate::store::UpstreamClearResult,
    /// Where the provider's cursor was moved to, or would have been, as the provider wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The provider's cursor before it, as the provider wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Why it failed, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<&crate::store::UpstreamClearAttempt> for UpstreamClearAttemptView {
    fn from(attempt: &crate::store::UpstreamClearAttempt) -> Self {
        Self {
            at_ms: attempt.at_ms,
            message_id: attempt.message.clone(),
            message_at_ms: attempt.message_ms,
            result: attempt.result,
            target: attempt.target.clone(),
            previous: attempt.previous.clone(),
            error: attempt.error.clone(),
        }
    }
}

/// One channel's pins: `GET /api/v1/channels/{id}/pins`. `#206 pin-message`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PinsResponse {
    /// The channel, as configured here.
    pub channel: ChannelInfo,
    /// Every pin, in the order their messages were sent, oldest first, each with the snapshot that
    /// lets the page draw it when its message is not loaded.
    pub pins: Vec<crate::store::Pin>,
    /// The revision this list is at; see [`TimelineResponse::pins_revision`].
    pub revision: i64,
    /// How many pins a channel keeps before the oldest is unpinned to make room.
    pub limit: usize,
    /// The standing statement that these pins are this server's own.
    pub pins_notice: &'static str,
    /// A snapshot is channel content, and untrusted, like every other.
    pub untrusted_content_notice: &'static str,
}

/// What one pin or unpin did: `PUT` and `DELETE /api/v1/channels/{id}/pins/{message_id}`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PinChangeResponse {
    /// The channel, as configured here.
    pub channel: ChannelInfo,
    /// The message the call named.
    pub message_id: MessageId,
    /// Whether it is pinned now.
    pub pinned: bool,
    /// Whether the call changed anything; false when it repeated what was already so.
    pub changed: bool,
    /// The pin exactly as stored — its text cut to the bound — or absent after an unpin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<crate::store::Pin>,
    /// Pins the bound dropped to make room for this one, oldest first. Empty except at the bound,
    /// and the page says so when it is not.
    pub unpinned: Vec<MessageId>,
    /// The channel's pin revision after the call, so the page that made it need not read the list
    /// again to stay current.
    pub revision: i64,
    /// The standing statement that these pins are this server's own.
    pub pins_notice: &'static str,
}

/// One channel's bookmark, read or written: `GET`, `PUT` and `DELETE /api/v1/channels/{id}/bookmark`.
/// `#240 reading-bookmark`.
///
/// A conditional write that lost to a move made elsewhere answers 409 with this same body and an
/// [`ApiErrorBody`]'s two fields beside it, so the page takes where the bookmark is now from the
/// refusal itself.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BookmarkResponse {
    /// The channel, as configured here.
    pub channel: ChannelInfo,
    /// The bookmark as stored after the call.
    pub bookmark: crate::store::Bookmark,
    /// Whether the call changed what is stored: false for a read, a repeat, and a refusal.
    pub changed: bool,
    /// The standing statement that the bookmark is this server's own and marks nothing read.
    pub bookmark_notice: &'static str,
}

/// A post waiting for the owner's confirmation. `#34 voice-chat-write-confirm`.
///
/// Everything the confirmation has to show — the exact text, where it goes, what it answers — and
/// the handle that commits exactly this. The handle is a capability: it is handed only to a
/// write-scope caller of the proposal routes, never to a model, and a commit must restate the
/// channel, text, and reply target it was issued for.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PendingPost {
    /// Correlates this proposal's steps in the access log. Increases with every proposal.
    pub serial: u64,
    /// The single-use confirmation handle.
    pub handle: String,
    /// The channel it would be posted to.
    pub channel_id: ChannelId,
    /// That channel's name as the owner knows it, alias included.
    pub channel_name: String,
    /// The exact text that would be posted.
    pub text: String,
    /// The message it would reply to, when any.
    pub reply_to: Option<MessageId>,
    /// Milliseconds until the handle expires.
    pub expires_in_ms: u64,
}

/// The post waiting for confirmation, or `null` when none is.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PendingPostResponse {
    /// The pending proposal. At most one exists: a new proposal supersedes the old.
    pub proposal: Option<PendingPost>,
}

/// A confirmed proposal, posted.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CommittedPostResponse {
    /// Which proposal this was.
    pub serial: u64,
    /// The FIRST message as the chat service accepted it.
    pub posted: Message,
    /// Every part, in order. One element for a message that fitted.
    pub parts: Vec<Message>,
}

/// The scope of the credential that made a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    /// May read channel history and summaries.
    Read,
    /// May also post, and may use the write-only conversation routes.
    Write,
}

impl From<crate::auth::Scope> for TokenScope {
    fn from(scope: crate::auth::Scope) -> Self {
        match scope {
            crate::auth::Scope::Read => Self::Read,
            crate::auth::Scope::Write => Self::Write,
        }
    }
}

/// One configured chat provider, as the page needs to know it.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ProviderDescription {
    /// The key channels name this provider by.
    pub key: String,
    /// Human-readable name of the source chat service.
    pub name: String,
    /// Whether this provider accepts pasted links and references when adding a channel.
    pub channel_registration_supported: bool,
    /// Whether this provider can list its channels for browsing.
    pub channel_discovery_supported: bool,
    /// Whether this provider exposes a write-through read cursor.
    pub upstream_read_mark_supported: bool,
    /// Whether this provider can clear its own unread state as this server ingests messages.
    /// `#232 upstream-mark-read`. Sent only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub upstream_clear_supported: bool,
    /// Whether this provider supports channel, thread-list, and flattened timelines.
    pub threading_supported: bool,
    /// Whether a reply to a main-channel message is posted in that message's thread, starting one
    /// when it has none, because this provider has no reply pointer of its own (Slack).
    /// `#252 thread-button-state`. Sent only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replies_in_thread: bool,
    /// Whether this provider posts a message sent again under the same key at most once. Sent
    /// only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idempotent_posts_supported: bool,
    /// How changes in this provider's channels reach the live stream.
    pub live_delivery: LiveDelivery,
    /// Seconds between this provider's live polls, or `0` when it is not polled.
    pub live_poll_seconds: u64,
    /// The account this server posts as at this provider, when known.
    pub self_author_id: Option<String>,
    /// The owner's own account at this provider, when configured.
    pub owner_author_id: Option<String>,
}

/// How channel changes reach the live stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LiveDelivery {
    /// Live ingestion is off; the stream attaches and delivers nothing.
    Off,
    /// The server polls the chat provider on an interval.
    Poll,
    /// A provider-neutral adapter pushes changes to the ingest route.
    Push,
}

/// The data of a live stream `message` or `message_update` event.
#[derive(Debug, Serialize, JsonSchema)]
pub struct LiveMessageEvent<'a> {
    /// The message as it now stands. UNTRUSTED: written by whoever is in the channel.
    pub message: &'a Message,
    /// Whether it was classified as history — a replay or a catch-up — rather than news. The page
    /// may render such a message but must not announce it to a live conversation.
    pub replayed: bool,
    /// Whether it was published before this stream attached and is re-sent from the replay tail.
    /// Any channel read begun after the stream's response arrived has already seen it. An
    /// adapter's catch-up arriving after the attach is `replayed` but not from the tail: it may be
    /// newer than every read the page has made.
    pub from_tail: bool,
    /// Whether THIS SERVER posted it; the page must not relay these into a live conversation.
    pub self_posted: bool,
    /// Standing reminder that the content is third-party text.
    pub untrusted_content_notice: &'static str,
}

/// The data of a live stream `message_delete` event: only the ids, never the removed text.
#[derive(Debug, Serialize, JsonSchema)]
pub struct LiveDeleteEvent {
    /// Channel the message was removed from.
    pub channel_id: ChannelId,
    /// The message that was removed.
    pub message_id: MessageId,
    /// Whether it was classified as history rather than news.
    pub replayed: bool,
    /// Whether it was published before this stream attached; see [`LiveMessageEvent::from_tail`].
    pub from_tail: bool,
}

/// The data of a live stream `reset` event: the subscriber fell behind the replay tail.
#[derive(Debug, Serialize, JsonSchema)]
pub struct LiveResetEvent {
    /// Events this subscriber missed, or `0` when the count is unknown.
    pub missed: u64,
    /// What the subscriber should do instead of resuming.
    pub detail: &'static str,
}

/// Who spoke a `vibe-talk-v1` transcript line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRole {
    /// The person on the call.
    User,
    /// The voice agent.
    Assistant,
}

/// A JSON control frame a `vibe-talk-v1` server sends. Audio travels separately, as binary frames
/// of 24 kHz, 16-bit little-endian mono PCM.
///
/// A client ignores a `type` it does not know, so a new frame type is backward compatible; a
/// changed field of a known type is not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VibeTalkV1ServerFrame {
    /// The session is ready. Always the first frame.
    SessionStarted {
        /// Whether the agent speaks first. When true, a voice client withholds microphone frames
        /// until the first `turn_complete`. Absent means false.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        greeting: Option<bool>,
        /// Provider session identifier, for connection details only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// Recognised or generated speech text.
    Transcript {
        /// Who said it.
        role: TranscriptRole,
        /// The words, or the current hypothesis of them.
        text: String,
        /// The turn this text belongs to. A frame without one cannot be matched and is treated as
        /// its own turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        /// False for a streaming hypothesis the provider may still correct. Absent means true, so
        /// a provider that only sends finished text needs no change.
        #[serde(default, rename = "final", skip_serializing_if = "Option::is_none")]
        is_final: Option<bool>,
    },
    /// Every accepted prompt, and every closed audio segment, ends with exactly one of these.
    TurnComplete {
        /// The turn that ended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        /// True when an `interrupt` cut the turn short.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupted: Option<bool>,
    },
    /// The provider failed. No `turn_complete` follows, so a client treats this as the end of the
    /// session.
    Error {
        /// What went wrong, in words.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// Additional detail, when the provider has any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// A JSON control frame a `vibe-talk-v1` client sends. Microphone audio travels separately, as
/// binary PCM at the session's input rate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VibeTalkV1ClientFrame {
    /// Microphone frames follow.
    AudioStart,
    /// The open microphone segment is over; the server answers with `turn_complete`.
    AudioEnd,
    /// One typed turn.
    Prompt {
        /// What the person typed.
        text: String,
    },
    /// Stop the running turn. The server answers with exactly one interrupted `turn_complete`, or
    /// ignores this when nothing is running.
    Interrupt,
    /// End the session.
    Quit,
}

/// Every root of the contract: each value that crosses a boundary on its own, by the name the
/// generated code gives it.
///
/// The order is the document's order, so keep it stable; a type reachable only through another
/// root still appears under `$defs` without being listed here.
fn roots(generator: &mut schemars::SchemaGenerator) -> Vec<schemars::Schema> {
    vec![
        generator.subschema_for::<ApiErrorBody>(),
        generator.subschema_for::<ClientConfigResponse>(),
        generator.subschema_for::<crate::conversation::VoiceSession>(),
        generator.subschema_for::<TimelineResponse>(),
        generator.subschema_for::<PendingPostResponse>(),
        generator.subschema_for::<CommittedPostResponse>(),
        generator.subschema_for::<LiveMessageEvent<'static>>(),
        generator.subschema_for::<LiveDeleteEvent>(),
        generator.subschema_for::<LiveResetEvent>(),
        generator.subschema_for::<VibeTalkV1ServerFrame>(),
        generator.subschema_for::<VibeTalkV1ClientFrame>(),
        // Not bodies on their own, but rows the page keeps in device storage and reads back
        // across a reload, so they need their own validators.
        generator.subschema_for::<Message>(),
        generator.subschema_for::<crate::threads::ThreadSummary>(),
        // `#206 pin-message`. Appended, so the roots before them keep their places.
        generator.subschema_for::<PinsResponse>(),
        generator.subschema_for::<PinChangeResponse>(),
        // `#232 upstream-mark-read`. Appended, for the same reason.
        generator.subschema_for::<UpstreamClearState>(),
        // `#240 reading-bookmark`. Appended, for the same reason.
        generator.subschema_for::<BookmarkResponse>(),
        // `#253 upstream-clear-polish`. Appended, for the same reason.
        generator.subschema_for::<UpstreamClearNowResponse>(),
        // `#264 voice-send-approval`. Appended, for the same reason.
        generator.subschema_for::<VoiceSettings>(),
        // `#208 persistent-cache`. Appended, for the same reason.
        generator.subschema_for::<MessagesResponse>(),
        // `#272 voice-agent-instructions`. Appended, for the same reason.
        generator.subschema_for::<VoicePromptSettings>(),
    ]
}

/// The whole contract as one JSON Schema (draft 2020-12) document.
///
/// Its `anyOf` lists the roots — the values that cross a boundary on their own — and `$defs`
/// holds every named type any of them reaches. Deterministic: `serde_json` keeps object keys
/// sorted, and nothing here depends on iteration order.
#[must_use]
pub fn schema_document() -> serde_json::Value {
    let mut generator = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator();
    let any_of = roots(&mut generator);
    let definitions = generator.take_definitions(true);
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://github.com/rrnewton/agent-utils/vibe-talk/contract/vibe-talk.schema.json",
        "title": "vibe-talk wire contract",
        "description": "Generated from vibe-talk/src/contract.rs by `make -C vibe-talk contract`. Do not edit.",
        "anyOf": any_of,
        "$defs": definitions,
    })
}

/// [`schema_document`] exactly as it is checked in: pretty-printed, with a trailing newline.
#[must_use]
pub fn schema_text() -> String {
    let mut text =
        serde_json::to_string_pretty(&schema_document()).expect("a JSON value always serializes");
    text.push('\n');
    text
}
