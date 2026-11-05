//! Server configuration: a TOML file, with environment overrides on top.
//!
//! Nothing here is host-specific and nothing here has a default that would let the server start
//! with a guessable credential. Every secret is required, is read from file or environment, and is
//! wrapped in [`Secret`] so it cannot be printed by accident.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{ChannelId, ChannelInfo};
use crate::store::Retention;

/// Environment variable naming the configuration file.
pub const ENV_CONFIG_PATH: &str = "VIBE_TALK_CONFIG";
/// Environment variable overriding the bind address.
pub const ENV_BIND: &str = "VIBE_TALK_BIND";
/// Environment variable carrying the Discord bot token.
pub const ENV_DISCORD_BOT_TOKEN: &str = "VIBE_TALK_DISCORD_BOT_TOKEN";
/// Environment variable overriding how often each channel is polled for inbound messages.
///
/// `0` turns live ingestion off, which is the default. See [`crate::live`].
pub const ENV_LIVE_POLL_SECONDS: &str = "VIBE_TALK_LIVE_POLL_SECONDS";
/// Environment variable carrying the read-scope API token.
pub const ENV_READ_TOKEN: &str = "VIBE_TALK_READ_TOKEN";
/// Environment variable carrying the write-scope API token.
pub const ENV_WRITE_TOKEN: &str = "VIBE_TALK_WRITE_TOKEN";
/// Environment variable carrying the dedicated live-adapter token.
pub const ENV_INGEST_TOKEN: &str = "VIBE_TALK_INGEST_TOKEN";
/// Environment variable selecting read-aloud: `elevenlabs` (default), `browser`, or `conversation`.
pub const ENV_READ_ALOUD_BACKEND: &str = "VIBE_TALK_READ_ALOUD_BACKEND";
/// Environment variable carrying the ElevenLabs API key.
pub const ENV_ELEVENLABS_API_KEY: &str = "VIBE_TALK_ELEVENLABS_API_KEY";
/// Environment variable carrying the ElevenLabs agent id.
pub const ENV_ELEVENLABS_AGENT_ID: &str = "VIBE_TALK_ELEVENLABS_AGENT_ID";
/// Which ElevenLabs voice reads a message aloud. Public: it identifies a catalogue voice.
pub const ENV_ELEVENLABS_VOICE_ID: &str = "VIBE_TALK_ELEVENLABS_VOICE_ID";
/// The reader's OWN Discord account, so their own messages are drawn as theirs.
pub const ENV_DISCORD_OWNER_USER_ID: &str = "VIBE_TALK_DISCORD_OWNER_USER_ID";
/// Environment variable overriding the ElevenLabs API base, so a test or a proxy can point
/// elsewhere.
pub const ENV_ELEVENLABS_API_BASE: &str = "VIBE_TALK_ELEVENLABS_API_BASE";
/// Environment variable overriding the channel list, as `id:label:rw` entries separated by commas.
pub const ENV_CHANNELS: &str = "VIBE_TALK_CHANNELS";
/// Environment variable overriding the externally reachable base URL.
pub const ENV_PUBLIC_BASE_URL: &str = "VIBE_TALK_PUBLIC_BASE_URL";
/// Environment variable overriding the operator's time zone, as an IANA name.
pub const ENV_TIMEZONE: &str = "VIBE_TALK_TIMEZONE";
/// Environment variable naming the SQLite file that holds durable state.
///
/// Unset means this server keeps nothing between restarts, and says so rather than pretending.
/// See [`crate::store`].
pub const ENV_STORAGE_PATH: &str = "VIBE_TALK_STORAGE_PATH";

/// Environment variable turning conversation replay on or off (`1`/`true`/`yes`/`on`).
pub const ENV_REPLAY_ENABLED: &str = "VIBE_TALK_REPLAY_ENABLED";
/// Environment variable capping the replayed transcript, in characters.
pub const ENV_REPLAY_MAX_CHARS: &str = "VIBE_TALK_REPLAY_MAX_CHARS";
/// Environment variable capping the replayed transcript, in turns.
pub const ENV_REPLAY_MAX_TURNS: &str = "VIBE_TALK_REPLAY_MAX_TURNS";
/// Environment variable choosing how the replay reaches the vendor.
pub const ENV_REPLAY_TRANSPORT: &str = "VIBE_TALK_REPLAY_TRANSPORT";

/// Environment variable turning the rewrite-for-speech pass on or off (`1`/`true`/`yes`/`on`,
/// or `0`/`false`/`no`/`off`).
///
/// An env override exists here and not for every other boolean because this is the switch an
/// operator reaches for while tuning a voice agent's prompt: it is the difference between two runs
/// of the same deployment, not a property of the deployment. See [`SpeakableConfig`].
pub const ENV_SPEAKABLE: &str = "VIBE_TALK_SPEAKABLE";

/// Environment variable overriding the model a summariser is asked for.
pub const ENV_SUMMARY_MODEL: &str = "VIBE_TALK_SUMMARY_MODEL";

/// Environment form of the DEPRECATED `summaries.backend` key.
///
/// Kept solely as a refusal seam. There is one summariser — see [`crate::summarize`] — so this
/// selects nothing; it exists so that a deployment still setting it is told, by name, rather than
/// having it silently ignored or reported as an unknown key.
pub const ENV_SUMMARY_BACKEND: &str = "VIBE_TALK_SUMMARY_BACKEND";

/// Shortest API token this server will accept.
///
/// These tokens are the ONLY thing between the public internet and a bot that can post to the
/// owner's channels, so a short one is refused at startup rather than warned about.
pub const MIN_TOKEN_LEN: usize = 24;

/// A string that must never reach a log line.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the secret. Call sites should be few and obvious.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time-ish comparison against a presented credential.
    ///
    /// This compares every byte rather than returning early, so the reply latency does not leak
    /// how much of a guessed token was correct.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let got = presented.as_bytes();
        let mut diff = u8::from(expected.len() != got.len());
        for i in 0..expected.len().max(got.len()) {
            let a = expected.get(i).copied().unwrap_or(0);
            let b = got.get(i).copied().unwrap_or(0);
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Everything the server needs to run.
#[derive(Debug)]
pub struct Config {
    /// Address to bind the HTTP listener to.
    pub bind: SocketAddr,
    /// Externally reachable base URL, when the deployment knows it.
    pub public_base_url: Option<String>,
    /// The zone every message timestamp is rendered into before anything speaks it.
    ///
    /// Configured, never hard-coded, and validated at load: a name the bundled database does not
    /// know stops the server rather than quietly becoming UTC. See [`crate::clock`].
    pub timezone: crate::clock::Zone,
    /// Read limits shared by every chat provider.
    pub chat: ChatLimits,
    /// Every configured chat provider, in configuration order.
    ///
    /// The legacy `[discord]` section, when present, is the first entry under the key
    /// [`LEGACY_DISCORD_PROVIDER_KEY`]; each `[[providers]]` entry follows it. A deployment always
    /// has at least one, and every configured channel names exactly one of them.
    pub providers: Vec<ProviderConfig>,
    /// Whether and how an earlier transcript is replayed into a new call.
    pub replay: ReplayConfig,
    /// API credentials for callers (the voice agent, and the web app).
    pub auth: AuthConfig,
    /// Optional authenticated adapter-to-server live-event and change-hint input.
    pub ingest: IngestConfig,
    /// Channels this server will read.
    pub channels: Vec<ChannelInfo>,
    /// Read-aloud backend, independent of conversational voice mode.
    pub read_aloud: ReadAloudConfig,
    /// Whether message bodies are rewritten for speech before anything says them.
    pub speakable: SpeakableConfig,
    /// ElevenLabs wiring, when configured.
    pub elevenlabs: ElevenLabsConfig,
    /// Provider used for live conversational voice sessions.
    pub conversation: ConversationConfig,
    /// Durable state: where it lives, and how much of it is kept.
    pub storage: StorageConfig,
    /// Summarisation policy. Every field that changes what a summary says is part of the cache
    /// key; see [`crate::summarize::policy_version`].
    pub summaries: SummariesConfig,
    /// The `[message_cache]` table, read and checked. `#208 persistent-cache`.
    ///
    /// `None` when the table is ABSENT, which is not the same as `enabled = false`: absent means
    /// "this configuration says nothing about a cache", and a later build treats a cache file it
    /// finds in that state differently from one it was told to turn off. This build reads the
    /// table and refuses a bad one at startup, but opens no cache: the cache library is not
    /// called from the request path yet.
    pub message_cache: Option<MessageCacheConfig>,
}

/// Default `[message_cache] max_days`: how long a cached message is kept, from when it was
/// written upstream. A host overlay that turns the cache on sets its own, smaller value until the
/// upstream retention is known.
pub const DEFAULT_MESSAGE_CACHE_MAX_DAYS: u16 = 90;
/// Default `[message_cache] max_megabytes`: the ceiling on the cache file and its two working
/// files together.
pub const DEFAULT_MESSAGE_CACHE_MAX_MEGABYTES: u32 = 256;
/// Default `[message_cache] timeline_fallback_ms`: how long a timeline read waits on the provider
/// before a cached page may stand in for it.
pub const DEFAULT_MESSAGE_CACHE_TIMELINE_FALLBACK_MS: u64 = 10_000;

/// `[message_cache]`: the disposable on-disk copy of channel messages. `#208 persistent-cache`.
///
/// A SEPARATE SQLite file from `[storage]`, never tables inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageCacheConfig {
    /// Whether the cache may be opened at all. `false` is the shipped default, and on a build
    /// that opens the cache it means "erase any cache file this server made".
    pub enabled: bool,
    /// Where the cache file lives. `None` means beside `[storage] path`, named after the store
    /// and this host.
    pub path: Option<PathBuf>,
    /// Oldest message kept, in days from when it was written upstream: 1 to 365.
    pub max_days: u16,
    /// Ceiling on the cache file plus its `-wal` and `-shm` files, in megabytes: 16 to 4096.
    pub max_megabytes: u32,
    /// Whether older history may be fetched in the background to fill the cache.
    pub backfill: CacheBackfill,
    /// How long a timeline read waits on the provider before a cached page may answer instead,
    /// in milliseconds: 1,000 to 60,000.
    pub timeline_fallback_ms: u64,
    /// Per-channel age limits, for conversations kept upstream for less time than `max_days`.
    pub channels: Vec<CacheChannelLimit>,
}

impl Default for MessageCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            max_days: DEFAULT_MESSAGE_CACHE_MAX_DAYS,
            max_megabytes: DEFAULT_MESSAGE_CACHE_MAX_MEGABYTES,
            backfill: CacheBackfill::Adaptive,
            timeline_fallback_ms: DEFAULT_MESSAGE_CACHE_TIMELINE_FALLBACK_MS,
            channels: Vec::new(),
        }
    }
}

impl MessageCacheConfig {
    /// The age limit for one channel, in days: its own `[[message_cache.channel]]` entry when it
    /// has one, otherwise `max_days`.
    #[must_use]
    pub fn max_days_for(&self, channel_id: &str) -> u16 {
        self.channels
            .iter()
            .find(|limit| limit.id.as_str() == channel_id)
            .map_or(self.max_days, |limit| limit.max_days)
    }
}

/// `[message_cache] backfill`: whether older history may be fetched to fill the cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheBackfill {
    /// Fetch older history while nothing else is using the provider, one call at a time.
    #[default]
    Adaptive,
    /// Cache only what reading brings in.
    Off,
}

/// One `[[message_cache.channel]]` entry: a channel kept for less (or more) time than the default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheChannelLimit {
    /// The channel, as `[[channels]] id` names it.
    pub id: ChannelId,
    /// Oldest message kept for this channel, in days: 1 to 365.
    pub max_days: u16,
}

/// How message read-aloud is synthesized; separate from conversational voice mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadAloudBackend {
    /// Server audio from ElevenLabs, preserving the default for existing deployments.
    #[default]
    ElevenLabs,
    /// Browser Web Speech using a voice advertised as local by the browser.
    Browser,
    /// Ask the configured conversational WebSocket agent to speak the message.
    Conversation,
}

/// Backend selection for tapping messages to read them aloud.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadAloudConfig {
    /// Speech backend; defaults to ElevenLabs when omitted.
    #[serde(default)]
    pub backend: ReadAloudBackend,
    /// Optional server-side route to the same conversational agent. Useful when the browser uses
    /// a public `wss://` ingress while the application can reach the bridge on loopback.
    pub websocket_url: Option<String>,
}

/// Whether a message body is rewritten before it is spoken.
///
/// **On by default**, because the unprepared form is what produced the complaint: markdown read as
/// "asterisk asterisk deploy asterisk asterisk", ISO timestamps read character by character, and
/// nineteen-digit account ids read digit by digit — each of them noise the listener cannot use and
/// cannot skip. See [`crate::speakable`] for what the pass does.
///
/// It is a switch rather than a fact because the pass is a JUDGEMENT about what a listener wants,
/// and an operator tuning a voice agent's prompt needs to be able to hear the same deployment
/// both ways. Turning it off is not a downgrade path for a bug — it is the control arm.
///
/// # What it does not switch off
///
/// [`Config::timezone`] conversion. A message still carries a local `spoken_time` with the switch
/// off, because that conversion is not a judgement: handing an assistant a UTC instant and letting
/// it do the arithmetic produced a real wrong answer, and there is no reading of "turn the
/// rewriting off" under which the operator was asking for that back.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpeakableConfig {
    /// Rewrite bodies for speech. Defaults to `true` when the section or the key is absent.
    #[serde(default = "yes")]
    pub enabled: bool,
}

impl Default for SpeakableConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// `serde` needs a function, not a literal, for a field that defaults to on.
fn yes() -> bool {
    true
}

/// What a summary is, in numbers.
///
/// Every field that changes what a summary SAYS is folded into
/// [`crate::summarize::policy_version`] and therefore into the cache key; `threshold_chars` and
/// `reply_timeout_seconds` change no word of one and are not (`#208 persistent-cache`). A field
/// added later must be decided one way or the other there, and the unit test that enumerates every
/// field fails until it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummariesConfig {
    /// Below this many characters a message is not summarised at all — it is already short
    /// enough to read, and summarising it would spend a model call to produce something longer
    /// than the original.
    pub threshold_chars: usize,
    /// How long a summary should be.
    pub target_chars: usize,
    /// How many preceding messages ride along as context.
    pub context_messages: usize,
    /// A model name, folded into the cache key and read by no backend.
    ///
    /// **Vestigial.** The shipped summariser asks a configured ElevenLabs agent, and WHICH model
    /// that agent runs is a property of the agent, set in the vendor's dashboard, not something
    /// this server can choose per request. Setting this therefore changes nothing about a summary
    /// except that every previously cached one becomes unreachable. It is folded into
    /// [`crate::summarize::policy_version`] anyway, as a model name would change the wording, and
    /// it should probably be deleted rather than documented as a selector.
    pub model: Option<String>,
    /// How many summaries share one conversation with the agent before it is recycled.
    ///
    /// It lives here rather than beside the summariser that reads it because it
    /// changes what a summary SAYS: a conversation carries every earlier summary in its context,
    /// so the answer to the eighth question is written in front of seven other people's messages.
    /// See [`crate::summarize::agent`] for the arithmetic behind the default.
    pub max_per_socket: usize,
    /// How long a conversation with the agent may sit unused before it is closed.
    pub socket_idle_seconds: u64,
    /// How long one turn with the agent, or one opening handshake, may take.
    pub reply_timeout_seconds: u64,
    /// A direct Messages API summariser, when `[summaries.llm]` is present. `#233 llm-summarizer`.
    ///
    /// When present it is THE summariser, and it is also the one that names threads; see
    /// [`crate::summarize::summarizer_for`]. Only the parts of it that change what a summary says
    /// (dialect, model, output budget) reach the cache key; see [`LlmConfig`].
    pub llm: Option<LlmConfig>,
}

impl Default for SummariesConfig {
    fn default() -> Self {
        Self {
            threshold_chars: DEFAULT_SUMMARY_THRESHOLD_CHARS,
            target_chars: crate::summary::DEFAULT_SUMMARY_CHARS,
            context_messages: DEFAULT_SUMMARY_CONTEXT_MESSAGES,
            model: None,
            max_per_socket: crate::summarize::agent::DEFAULT_MAX_PER_SOCKET,
            socket_idle_seconds: crate::summarize::agent::DEFAULT_SOCKET_IDLE_SECONDS,
            reply_timeout_seconds: crate::summarize::agent::DEFAULT_REPLY_TIMEOUT_SECONDS,
            llm: None,
        }
    }
}

/// Which wire dialect of the Messages API a [`LlmConfig`] speaks. `#233 llm-summarizer`.
///
/// One request body, two ways of addressing it: see [`crate::summarize::messages_api`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmDialect {
    /// `POST {base_url}/v1/messages`, authenticated with an `x-api-key` read from a file.
    Anthropic,
    /// `POST {base_url}/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:rawPredict`,
    /// authenticated with an optional bearer token and/or an mTLS client identity.
    Vertex,
}

impl LlmDialect {
    /// The spelling used in the configuration file, the startup log and the cache key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Vertex => "vertex",
        }
    }
}

/// `[summaries.llm]`: summaries from one plain Messages API call, with no agent harness and no
/// tools. `#233 llm-summarizer`.
///
/// **No secret is ever written in this section.** The API key and the bearer token are read from
/// FILES (or, for the bearer token, from a command's output) at call time, so a key rotated on
/// disk is picked up without a restart and nothing secret sits in a file that is shared, copied
/// into a ticket or printed by `--help`. A key written inline is refused by name at load.
///
/// Only [`LlmConfig::dialect`], [`LlmConfig::model`] and [`LlmConfig::max_tokens`] change what a
/// summary SAYS, and only they are folded into the cache key (through the summariser's
/// [`crate::summarize::Summarizer::policy_input`]). Everything else here is transport: endpoints,
/// credentials, timeouts and the rate limit. Folding those in would throw every cached summary
/// away on a key rotation, and would write a deployment's file layout into a cache key on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmConfig {
    /// Which wire dialect to speak.
    pub dialect: LlmDialect,
    /// Where the API lives, without a trailing slash. Defaults per dialect: the Anthropic API, or
    /// the public Google Vertex endpoint for [`LlmConfig::region`].
    pub base_url: String,
    /// The model to ask. Default [`DEFAULT_LLM_MODEL`], which is a valid name in both dialects.
    pub model: String,
    /// Google Cloud project. Vertex only, and required there.
    pub project: Option<String>,
    /// Google Cloud region, or `global`. Vertex only, and required there.
    pub region: Option<String>,
    /// File holding the API key sent as `x-api-key`. Required for the Anthropic dialect; on the
    /// Vertex dialect it is sent only when it is configured here explicitly.
    pub api_key_file: Option<PathBuf>,
    /// File holding a bearer token, re-read on every call. Vertex only.
    pub bearer_token_file: Option<PathBuf>,
    /// A command (argv, no shell) whose standard output is a bearer token, cached briefly.
    /// Vertex only, and not together with `bearer_token_file`.
    pub bearer_token_command: Option<Vec<String>>,
    /// PEM file holding a client certificate chain and its private key, for a gateway that
    /// authenticates by client certificate. Re-read whenever its modification time changes,
    /// because such identities are short-lived and rotate on disk.
    pub client_identity_file: Option<PathBuf>,
    /// PEM bundle of extra root certificates to trust, for a gateway that chains to a private
    /// root. Added to the bundled public roots, never replacing them.
    pub ca_bundle_file: Option<PathBuf>,
    /// Extra request headers, sent on every call. NOT for secrets: they are written inline.
    pub extra_headers: BTreeMap<String, String>,
    /// The output budget of one call, in tokens.
    pub max_tokens: u32,
    /// How long one call may take, in seconds.
    pub timeout_secs: u64,
    /// Client-side ceiling on calls per minute, across every summary and thread name.
    pub requests_per_minute: u32,
    /// How many calls may go out back to back before the per-minute pace applies.
    pub burst: u32,
}

/// The model `[summaries.llm]` asks when none is named: a small, fast model whose alias is
/// valid on both the Anthropic API and Google Vertex.
pub const DEFAULT_LLM_MODEL: &str = "claude-haiku-4-5";
/// Where the Anthropic dialect sends requests by default.
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// Default output budget: a slug and a sentence, or one sentence, with room to spare.
pub const DEFAULT_LLM_MAX_TOKENS: u32 = 120;
/// Default ceiling on one call, in seconds.
pub const DEFAULT_LLM_TIMEOUT_SECS: u64 = 30;
/// Default client-side rate limit: conservative, because a thread list can ask for dozens of
/// names at once and the model provider's own limit is the operator's to spend, not ours.
pub const DEFAULT_LLM_REQUESTS_PER_MINUTE: u32 = 15;
/// Default burst before the per-minute pace applies.
pub const DEFAULT_LLM_BURST: u32 = 3;

/// The public Google Vertex endpoint for `region`.
#[must_use]
pub fn vertex_base_url(region: &str) -> String {
    if region == "global" {
        "https://aiplatform.googleapis.com/v1".to_owned()
    } else {
        format!("https://{region}-aiplatform.googleapis.com/v1")
    }
}

/// Below this, a message is left alone.
pub const DEFAULT_SUMMARY_THRESHOLD_CHARS: usize = 400;
/// How many preceding messages ride along as context by default.
///
/// Three, and the honest note is that there is no offline oracle for this number: it was chosen,
/// not measured. It is configuration precisely so a deployment can disagree.
pub const DEFAULT_SUMMARY_CONTEXT_MESSAGES: usize = 3;

/// Where durable state lives, and how much of it is kept.
///
/// See [`crate::store`] for what is stored and why the absent case is a refusal rather than an
/// in-memory fallback.
#[derive(Debug, Default)]
pub struct StorageConfig {
    /// Absolute path to the SQLite file. `None` means this server keeps nothing.
    pub path: Option<PathBuf>,
    /// Bounds on what is kept.
    pub retention: Retention,
}

/// Default ceiling on a replayed transcript, in characters.
///
/// A guess, and labelled as one. It is billed per call and the model's window is finite; nothing
/// in this crate can measure whether it is right. `scripts/smoke-agent.py --replay-check` can.
pub const DEFAULT_REPLAY_MAX_CHARS: usize = 6000;
/// Default ceiling on a replayed transcript, in turns.
pub const DEFAULT_REPLAY_MAX_TURNS: usize = 40;

/// Reconstructing continuity across a hang-up. See [`crate::replay`].
#[derive(Debug)]
pub struct ReplayConfig {
    /// Whether the page may ask for a replay at all.
    ///
    /// **Off by default, and the reason is privacy rather than caution.** Every new call re-sends
    /// earlier conversation content — including Discord text written by other people — to the
    /// voice vendor. That is a decision an operator makes deliberately.
    pub enabled: bool,
    /// The budget.
    pub policy: crate::replay::ReplayPolicy,
    /// How the payload reaches the vendor.
    pub transport: crate::replay::Transport,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            policy: crate::replay::ReplayPolicy {
                max_chars: DEFAULT_REPLAY_MAX_CHARS,
                max_turns: DEFAULT_REPLAY_MAX_TURNS,
            },
            transport: crate::replay::Transport::ContextualUpdate,
        }
    }
}

/// Discord access parameters.
#[derive(Debug)]
pub struct DiscordConfig {
    /// Source service's display name. Bridges set this to the service they represent.
    pub provider_name: String,
    /// Thread protocol: `native` (default), normalized `bridge`, or `off`.
    pub thread_api: crate::threads::ThreadApi,
    /// Bot token. Sent as `Authorization: Bot <token>`.
    pub bot_token: Secret,
    /// The reader's own Discord user id, when they have said what it is.
    ///
    /// THIS CANNOT BE DERIVED, and that is why it is a setting. The bot token identifies the BOT
    /// -- `self_user_id_from_token` reads that straight out of it -- and a bot's account has no
    /// relationship to the human reading the channel. So the bridge's own posts are recognised for
    /// free, and messages the owner typed into Discord HIMSELF are just somebody else's until he
    /// says otherwise. The web app can also be told per-account in Settings, which needs no
    /// restart; this is the answer for a deployment that would rather state it once.
    ///
    /// Public: a user id is visible on every message that account has ever sent.
    pub owner_user_id: Option<String>,
    /// API base, so a test or a proxy can point elsewhere.
    pub api_base: String,
    /// Deadline for one ordinary provider HTTP request, in seconds.
    pub request_timeout_seconds: u64,
    /// Whether the configured HTTP bridge manages channel registration.
    ///
    /// Off by default because Discord itself does not provide these endpoints. An operator opts in
    /// only when `api_base` names a compatible bridge.
    pub channel_registration: bool,
    /// Whether the configured HTTP bridge implements the optional upstream read endpoint.
    ///
    /// Off by default because Discord itself does not provide this operation. An operator must
    /// opt in only when `api_base` names a compatible bridge.
    pub upstream_read_marks: bool,
    /// How this provider's messages are written, for the page's renderer. `#217 markdown-blocks`.
    ///
    /// Discord's own Markdown unless the operator says otherwise. A bridge for another service
    /// passes that service's text through, and the one difference worth a setting is whether a
    /// single asterisk means bold: it does in Google Chat. See [`crate::render::Markup`].
    pub markup: crate::render::Markup,
    /// The most characters one post may hold, when the operator has said. `#263
    /// post-split-per-provider`; see [`DiscordConfig::post_limit`].
    pub max_post_chars: Option<usize>,
    /// The most UTF-8 bytes one post may hold, when the operator has said.
    pub max_post_bytes: Option<usize>,
    /// How a bridge answers a read of a channel's most recent messages, and whether that read is
    /// checked against the other way. `#208 persistent-cache`. Only `thread_api = "bridge"`
    /// accepts a setting here; everywhere else it is the default.
    pub recent: RecentReads,
}

impl DiscordConfig {
    /// The most text one post through this provider may carry. `#263 post-split-per-provider`.
    ///
    /// Discord itself takes its own 2000 characters, or fewer when configured. A bridge takes
    /// what the operator said it accepts (`max_post_chars`, `max_post_bytes`), and when the
    /// operator said nothing, 2000 characters: the most any bridge has been sent since posts were
    /// first split, so an upgrade never sends a bridge a longer post than it has accepted before.
    /// A bridge serving Google Chat is held to Google Chat's own
    /// [`crate::discord::http::GOOGLE_CHAT_MAX_MESSAGE_BYTES`] as well, whatever it was told.
    #[must_use]
    pub fn post_limit(&self) -> crate::chat::PostLimit {
        use crate::discord::http::{DISCORD_MAX_CONTENT_LEN, GOOGLE_CHAT_MAX_MESSAGE_BYTES};
        let official = is_official_discord_api_base(&self.api_base);
        let stated = self.max_post_chars.is_some() || self.max_post_bytes.is_some();
        let max_chars = match self.max_post_chars {
            Some(max) if official => Some(max.min(DISCORD_MAX_CONTENT_LEN)),
            Some(max) => Some(max),
            None if official || !stated => Some(DISCORD_MAX_CONTENT_LEN),
            None => None,
        };
        let max_bytes = if !official && self.markup == crate::render::Markup::GoogleChat {
            Some(
                self.max_post_bytes
                    .map_or(GOOGLE_CHAT_MAX_MESSAGE_BYTES, |max| {
                        max.min(GOOGLE_CHAT_MAX_MESSAGE_BYTES)
                    }),
            )
        } else {
            self.max_post_bytes
        };
        crate::chat::PostLimit {
            max_chars,
            max_bytes,
            escaping: crate::chat::PostEscaping::Verbatim,
        }
    }
}

/// Which bridge endpoint answers a read of a channel's most recent messages. `#208
/// persistent-cache`, milestone `bridge-recent-flat`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecentView {
    /// `GET /channels/{id}/messages`, as every Discord-protocol provider has always been read.
    /// The default.
    #[default]
    Messages,
    /// The newest page of `GET /channels/{id}/timeline?view=flat`: every message in the channel,
    /// replies included, in one chronological history. A bridge that keeps a copy of the channel
    /// answers it from that copy, and says with `as_of` when the copy is not current.
    Flat,
}

/// A bridge's recent-read settings. `#208 persistent-cache`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecentReads {
    /// Which endpoint answers. `recent_view`, default `messages`.
    pub view: RecentView,
    /// With `messages`: check such reads against a later read of the Flat view and count where
    /// the two disagree, for diagnostics. The checks run one at a time, after recent reads have
    /// stopped, because a bridge may serve a Flat read ahead of `/messages`. `recent_view_shadow`,
    /// default off. See [`crate::recent_view`].
    pub shadow: bool,
}

/// Read limits shared by every chat provider.
///
/// They bound what one request to THIS server may cost, whichever provider then answers it, so
/// they are one setting rather than one per provider. They are read from `[chat]`, or from the
/// legacy `[discord]` section where older configurations put them.
#[derive(Clone, Copy, Debug)]
pub struct ChatLimits {
    /// Default number of messages a fetch returns.
    pub default_fetch_limit: u16,
    /// Hard ceiling on a caller-requested fetch size.
    pub max_fetch_limit: u16,
    /// How many messages a count may walk before it gives up and answers "at least this many".
    ///
    /// A cost ceiling, not a preference. Discord has no message-count endpoint, so counting means
    /// paginating at a hundred messages per request against a shared rate limit; without a cap, a
    /// single spoken "how many messages are in there?" could fire dozens of requests at a busy
    /// channel and stall every other read behind it.
    pub max_count_scan: u32,
}

/// The first key of a multi-provider deployment, and the only one of a legacy configuration.
pub const LEGACY_DISCORD_PROVIDER_KEY: &str = "discord";

/// Slack access parameters for one `[[providers]]` entry of kind `slack`.
///
/// Mirrors [`crate::slack::SlackConfig`]; the provider router converts one into the other.
#[derive(Debug)]
pub struct SlackSettings {
    /// Slack Web API base, or a compatible bridge implementing the same method subset.
    pub api_base: String,
    /// Bearer token: a bot (`xoxb-`) or user (`xoxp-`) token, or a compatible bridge's credential.
    pub token: Secret,
    /// The owner's own Slack user id, when the deployment has said what it is.
    pub owner_user_id: Option<String>,
    /// Deadline for one ordinary provider HTTP request, in seconds.
    pub request_timeout_seconds: u64,
    /// Whether the app may add a Slack conversation or thread by pasting its link.
    pub channel_registration: bool,
    /// The write policy given to a conversation added from the app.
    pub registered_channels_writable: bool,
    /// Whether the app may list the conversations the token's account belongs to.
    pub channel_discovery: bool,
    /// Whether Settings may offer "Clear unread in Slack" for this provider's channels, moving the
    /// token's own read cursor with `conversations.mark`. Off by default. `#258 slack-clear-unread`.
    pub upstream_clear: bool,
    /// The most characters one post may hold, as Slack is sent it, when the operator has said.
    /// `#263 post-split-per-provider`; see [`crate::slack::SlackConfig::post_limit`].
    pub max_post_chars: Option<usize>,
    /// The most UTF-8 bytes one post may hold, as Slack is sent it, when the operator has said.
    pub max_post_bytes: Option<usize>,
}

/// How one provider is reached.
#[derive(Debug)]
pub enum ProviderKind {
    /// Discord itself, or a bridge speaking Discord's HTTP API (for example, for Google Chat).
    Discord(DiscordConfig),
    /// Slack's Web API, or a bridge speaking the same method subset.
    Slack(SlackSettings),
}

/// One configured chat provider.
#[derive(Debug)]
pub struct ProviderConfig {
    /// Short, path-safe key that channels use to name this provider.
    pub key: String,
    /// Display name of the source service, used in errors, diagnostics, and the app.
    pub name: String,
    /// How often each configured channel is polled for messages nobody asked for. `0` is OFF.
    ///
    /// **Off by default.** Every tick is one Discord request per channel against a shared rate
    /// limit. The client now obeys `Retry-After` within a bounded budget
    /// ([`crate::discord::ratelimit`]), so a burst is waited out rather than surfaced as a gateway
    /// error — but waiting is not extra quota, and a poll that runs whether or not anybody is
    /// listening spends the same quota the questions do. Turning it on is a decision an operator
    /// makes with that in front of them, not a thing that happens because they upgraded. Refused
    /// below [`crate::live::MIN_POLL_SECONDS`].
    pub live_poll_seconds: u64,
    /// How this provider is reached.
    pub kind: ProviderKind,
}

impl ProviderConfig {
    /// The namespace recorded beside a channel this provider registered.
    ///
    /// For a Discord-protocol provider it is the API base, exactly as single-provider deployments
    /// have always recorded it, so their stored registrations keep resolving after an upgrade.
    #[must_use]
    pub fn namespace(&self) -> String {
        match &self.kind {
            ProviderKind::Discord(discord) => discord.api_base.trim_end_matches('/').to_owned(),
            ProviderKind::Slack(slack) => {
                format!("slack:{}", slack.api_base.trim_end_matches('/'))
            }
        }
    }

    /// Whether this provider manages channel registration for links pasted into the app.
    #[must_use]
    pub fn channel_registration(&self) -> bool {
        match &self.kind {
            ProviderKind::Discord(discord) => discord.channel_registration,
            ProviderKind::Slack(slack) => slack.channel_registration,
        }
    }

    /// The configured owner account at this provider.
    #[must_use]
    pub fn owner_user_id(&self) -> Option<&str> {
        match &self.kind {
            ProviderKind::Discord(discord) => discord.owner_user_id.as_deref(),
            ProviderKind::Slack(slack) => slack.owner_user_id.as_deref(),
        }
    }

    /// How this provider's messages are written, for the page's renderer: the configured
    /// `markup` of a Discord-protocol provider, and always Slack's for a Slack provider.
    #[must_use]
    pub fn markup(&self) -> crate::render::Markup {
        match &self.kind {
            ProviderKind::Discord(discord) => discord.markup,
            ProviderKind::Slack(_) => crate::render::Markup::Slack,
        }
    }

    /// The provider's credential, for redaction.
    #[must_use]
    pub fn token(&self) -> &Secret {
        match &self.kind {
            ProviderKind::Discord(discord) => &discord.bot_token,
            ProviderKind::Slack(slack) => &slack.token,
        }
    }
}

/// API credentials this server requires of its callers.
#[derive(Debug)]
pub struct AuthConfig {
    /// Token permitting reads.
    pub read_token: Secret,
    /// Token permitting reads AND posting.
    pub write_token: Secret,
}

/// Authentication for provider-neutral live-event and change-hint ingestion.
#[derive(Debug, Default)]
pub struct IngestConfig {
    /// Dedicated bearer token. `None` means both live-adapter routes are disabled.
    pub token: Option<Secret>,
}

impl IngestConfig {
    /// Whether an adapter may deliver events or change hints to this server.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.token.is_some()
    }
}

/// ElevenLabs wiring.
///
/// Both credentials are optional: the Discord half of this server is useful on its own, and a
/// deployment that has not wired a voice agent yet should still start. What is NOT optional is
/// what happens when something asks for a signed conversation URL without them — that is a loud,
/// specific refusal naming the absent setting, never a silent fallback. See
/// [`crate::elevenlabs::credentials`].
// Clone because the agent summariser has to HOLD this: `Summarizer::summarize` takes no
// configuration, unlike the signed-URL client, so a backend that talks to ElevenLabs
// cannot be handed the settings at call time and must be built with them.
#[derive(Clone, Debug)]
pub struct ElevenLabsConfig {
    /// Agent id, recorded so the deployment is self-describing. Public: it identifies a widget.
    pub agent_id: Option<String>,
    /// Account API key, used to mint short-lived signed conversation URLs. A secret.
    pub api_key: Option<Secret>,
    /// API base, so a test or a proxy can point elsewhere.
    pub api_base: String,
    /// Which catalogue voice reads a message aloud.
    ///
    /// Optional and separate from `agent_id`: a deployment can have a conversational agent and no
    /// reading voice, or a reading voice and no agent, and the two failures have different fixes.
    /// Absent means read-aloud is unavailable, which is a loud refusal naming this setting rather
    /// than a silent fallback to whichever voice the account happens to list first -- an account's
    /// first voice is not a decision anybody made.
    pub voice_id: Option<String>,
}

impl Default for ElevenLabsConfig {
    fn default() -> Self {
        Self {
            agent_id: None,
            api_key: None,
            api_base: DEFAULT_ELEVENLABS_API_BASE.to_owned(),
            voice_id: None,
        }
    }
}

/// Conversational voice backend selected by this deployment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationBackend {
    /// Mint a short-lived URL using the existing ElevenLabs integration.
    #[default]
    ElevenLabs,
    /// Connect the browser to a deployment-managed `vibe-talk-v1` WebSocket.
    WebSocket,
}

/// Provider-neutral live conversation configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationConfig {
    /// Which provider opens live voice sessions.
    #[serde(default)]
    pub backend: ConversationBackend,
    /// Stable WebSocket endpoint used by [`ConversationBackend::WebSocket`].
    pub websocket_url: Option<String>,
    /// Human-readable label shown in connection details.
    #[serde(default = "default_conversation_label")]
    pub label: String,
    /// The SHIPPED default of "Ask me before the voice agent sends". `#264 voice-send-approval`.
    ///
    /// On unless the operator turns it off here. The owner's own choice, made in Settings, is kept
    /// in the store and wins over this; this value is what applies until he makes one, and again
    /// after a purge. See [`crate::send_approval`].
    #[serde(default = "default_ask_before_send")]
    pub ask_before_send: bool,
}

fn default_conversation_label() -> String {
    "WebSocket voice provider".to_owned()
}

const fn default_ask_before_send() -> bool {
    true
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            backend: ConversationBackend::ElevenLabs,
            websocket_url: None,
            label: default_conversation_label(),
            ask_before_send: default_ask_before_send(),
        }
    }
}

/// Configuration could not be assembled.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The TOML file did not parse.
    #[error("configuration file did not parse: {0}")]
    Parse(#[from] toml::de::Error),
    /// The configuration file could not be read.
    #[error("configuration file {path} could not be read: {source}")]
    Read {
        /// Path that was attempted.
        path: String,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// A required value was absent from both the file and the environment.
    #[error("{0} is required: set it in the configuration file or in the environment")]
    Missing(String),
    /// A value was present but unusable.
    #[error("{field} is invalid: {detail}")]
    Invalid {
        /// Field name as the operator would write it.
        field: String,
        /// Why it was rejected.
        detail: String,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    server: FileServer,
    #[serde(default)]
    discord: FileDiscord,
    #[serde(default)]
    chat: FileChat,
    #[serde(default)]
    providers: Vec<FileProvider>,
    #[serde(default)]
    auth: FileAuth,
    #[serde(default)]
    ingest: FileIngest,
    #[serde(default)]
    channels: Vec<FileChannel>,
    #[serde(default)]
    elevenlabs: FileElevenLabs,
    #[serde(default)]
    conversation: ConversationConfig,
    #[serde(default)]
    read_aloud: ReadAloudConfig,
    #[serde(default)]
    speakable: SpeakableConfig,
    #[serde(default)]
    storage: FileStorage,
    #[serde(default)]
    summaries: FileSummaries,
    #[serde(default)]
    replay: FileReplay,
    /// `#208 persistent-cache`. `None` when the table is absent, which is not `enabled = false`.
    message_cache: Option<FileMessageCache>,
}

/// `[message_cache]` as written. Integers are read wide so that an out-of-range value is refused
/// with its range named, rather than as a type error that does not say what would be accepted.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMessageCache {
    enabled: Option<bool>,
    path: Option<String>,
    max_days: Option<i64>,
    max_megabytes: Option<i64>,
    backfill: Option<CacheBackfill>,
    timeline_fallback_ms: Option<i64>,
    #[serde(default)]
    channel: Vec<FileCacheChannel>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileCacheChannel {
    id: String,
    max_days: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileReplay {
    enabled: Option<bool>,
    max_chars: Option<usize>,
    max_turns: Option<usize>,
    transport: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileServer {
    bind: Option<String>,
    public_base_url: Option<String>,
    timezone: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDiscord {
    provider_name: Option<String>,
    thread_api: Option<crate::threads::ThreadApi>,
    bot_token: Option<Secret>,
    owner_user_id: Option<String>,
    api_base: Option<String>,
    request_timeout_seconds: Option<u64>,
    default_fetch_limit: Option<u16>,
    max_fetch_limit: Option<u16>,
    max_count_scan: Option<u32>,
    live_poll_seconds: Option<u64>,
    channel_registration: Option<bool>,
    upstream_read_marks: Option<bool>,
    markup: Option<crate::render::Markup>,
    max_post_chars: Option<usize>,
    max_post_bytes: Option<usize>,
    recent_view: Option<RecentView>,
    recent_view_shadow: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileChat {
    default_fetch_limit: Option<u16>,
    max_fetch_limit: Option<u16>,
    max_count_scan: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileProvider {
    key: String,
    kind: String,
    name: Option<String>,
    api_base: Option<String>,
    token: Option<Secret>,
    owner_user_id: Option<String>,
    request_timeout_seconds: Option<u64>,
    live_poll_seconds: Option<u64>,
    channel_registration: Option<bool>,
    max_post_chars: Option<usize>,
    max_post_bytes: Option<usize>,
    // Discord-protocol providers only.
    thread_api: Option<crate::threads::ThreadApi>,
    upstream_read_marks: Option<bool>,
    markup: Option<crate::render::Markup>,
    // Discord-protocol providers with `thread_api = "bridge"` only.
    recent_view: Option<RecentView>,
    recent_view_shadow: Option<bool>,
    // Slack providers only.
    registered_channels_writable: Option<bool>,
    channel_discovery: Option<bool>,
    upstream_clear: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAuth {
    read_token: Option<Secret>,
    write_token: Option<Secret>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIngest {
    token: Option<Secret>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileChannel {
    id: String,
    label: String,
    #[serde(default)]
    writable: bool,
    provider: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileElevenLabs {
    agent_id: Option<String>,
    api_key: Option<Secret>,
    api_base: Option<String>,
    voice_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSummaries {
    threshold_chars: Option<usize>,
    target_chars: Option<usize>,
    context_messages: Option<usize>,
    model: Option<String>,
    /// DEPRECATED and selects nothing. Still declared so that a configuration naming it gets the
    /// refusal in [`summaries_config`] rather than `deny_unknown_fields`' "unknown field
    /// `backend`", which reads as "this binary is too old" and sends the operator to a rollback.
    backend: Option<String>,
    max_per_socket: Option<usize>,
    socket_idle_seconds: Option<u64>,
    reply_timeout_seconds: Option<u64>,
    llm: Option<FileLlm>,
}

/// `[summaries.llm]` as written. See [`LlmConfig`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLlm {
    dialect: Option<LlmDialect>,
    base_url: Option<String>,
    model: Option<String>,
    project: Option<String>,
    region: Option<String>,
    api_key_file: Option<String>,
    bearer_token_file: Option<String>,
    bearer_token_command: Option<Vec<String>>,
    client_identity_file: Option<String>,
    ca_bundle_file: Option<String>,
    extra_headers: Option<BTreeMap<String, String>>,
    max_tokens: Option<u32>,
    timeout_secs: Option<u64>,
    requests_per_minute: Option<u32>,
    burst: Option<u32>,
    /// Declared only to be REFUSED by name: a secret does not belong inline. Typed as a raw value
    /// so the refusal never has to parse, hold or print it.
    api_key: Option<toml::Value>,
    /// As `api_key`.
    bearer_token: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileStorage {
    path: Option<String>,
    max_conversations: Option<u16>,
    max_turns_per_conversation: Option<u16>,
    max_summaries: Option<u32>,
    max_dismissals: Option<u32>,
    retain_days: Option<u16>,
    summary_retain_days: Option<u16>,
}

/// Default bind address: every interface inside a container, port 8080.
const DEFAULT_BIND: &str = "0.0.0.0:8080";
/// Default Discord API base.
pub const DEFAULT_DISCORD_API_BASE: &str = "https://discord.com/api/v10";
pub(crate) fn is_official_discord_api_base(api_base: &str) -> bool {
    reqwest::Url::parse(api_base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "discord.com" | "discordapp.com"))
}

/// Default Slack Web API base.
pub const DEFAULT_SLACK_API_BASE: &str = crate::slack::DEFAULT_SLACK_API_BASE;

/// Default ElevenLabs API base.
pub const DEFAULT_ELEVENLABS_API_BASE: &str = "https://api.elevenlabs.io/v1";
/// Default operator time zone.
///
/// UTC because it is the one zone that is right for nobody in particular and therefore cannot be
/// mistaken for a considered choice — an operator who leaves it alone hears the same instant
/// Discord reported, correctly labelled, rather than a plausible-looking local time from whichever
/// datacentre the container happens to run in.
pub const DEFAULT_TIMEZONE: &str = "UTC";
const DEFAULT_FETCH_LIMIT: u16 = 50;
const DEFAULT_MAX_FETCH_LIMIT: u16 = 100;
const DEFAULT_PROVIDER_REQUEST_TIMEOUT_SECONDS: u64 = 20;
/// Default cost ceiling for a count: five Discord requests' worth.
const DEFAULT_MAX_COUNT_SCAN: u32 = 500;

impl Config {
    /// Read the configuration file at `path`, then apply process-environment overrides.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be read or parsed, or when the assembled
    /// configuration is incomplete or invalid.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let env: BTreeMap<String, String> = std::env::vars().collect();
        Self::from_toml_and_env(&text, &env)
    }

    /// Assemble a configuration from TOML text plus an environment map.
    ///
    /// Environment values take precedence over file values, so a container can be given its
    /// secrets without ever writing them to disk.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the text does not parse, a required value is absent, or a
    /// present value is unusable.
    pub fn from_toml_and_env(
        text: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let file: FileConfig = toml::from_str(text)?;
        let get = |key: &str| env.get(key).map(String::as_str).filter(|v| !v.is_empty());

        let bind_text = get(ENV_BIND)
            .map(str::to_owned)
            .or(file.server.bind)
            .unwrap_or_else(|| DEFAULT_BIND.to_owned());
        let bind = bind_text
            .parse::<SocketAddr>()
            .map_err(|e| ConfigError::Invalid {
                field: "server.bind".to_owned(),
                detail: format!("{bind_text:?} is not an address:port ({e})"),
            })?;

        let timezone_name = get(ENV_TIMEZONE)
            .map(str::to_owned)
            .or(file.server.timezone)
            .unwrap_or_else(|| DEFAULT_TIMEZONE.to_owned());
        let timezone =
            crate::clock::zone(&timezone_name).map_err(|detail| ConfigError::Invalid {
                field: "server.timezone".to_owned(),
                detail,
            })?;

        let FileConfig {
            discord: file_discord,
            chat: file_chat,
            providers: file_providers,
            ..
        } = file;
        let chat = chat_limits(&file_chat, &file_discord)?;
        let mut providers = Vec::new();
        if let Some(legacy) =
            legacy_discord_provider(file_discord, &get, !file_providers.is_empty())?
        {
            providers.push(legacy);
        }
        for entry in file_providers {
            let provider = provider_entry(entry, &get)?;
            if providers
                .iter()
                .any(|p: &ProviderConfig| p.key == provider.key)
            {
                return Err(ConfigError::Invalid {
                    field: "providers.key".to_owned(),
                    detail: format!("{:?} names two providers", provider.key),
                });
            }
            providers.push(provider);
        }

        let read_token = get(ENV_READ_TOKEN)
            .map(Secret::new)
            .or(file.auth.read_token)
            .ok_or_else(|| ConfigError::Missing("auth.read_token".to_owned()))?;
        let write_token = get(ENV_WRITE_TOKEN)
            .map(Secret::new)
            .or(file.auth.write_token)
            .ok_or_else(|| ConfigError::Missing("auth.write_token".to_owned()))?;
        check_token_strength("auth.read_token", &read_token)?;
        check_token_strength("auth.write_token", &write_token)?;
        if read_token == write_token {
            return Err(ConfigError::Invalid {
                field: "auth.write_token".to_owned(),
                detail: "must differ from auth.read_token, or the read scope grants posting"
                    .to_owned(),
            });
        }
        let ingest_token = get(ENV_INGEST_TOKEN).map(Secret::new).or(file.ingest.token);
        if let Some(token) = ingest_token.as_ref() {
            check_token_strength("ingest.token", token)?;
            if token == &read_token || token == &write_token {
                return Err(ConfigError::Invalid {
                    field: "ingest.token".to_owned(),
                    detail: "must differ from both API tokens, because an adapter credential has \
                             no browser read or write scope"
                        .to_owned(),
                });
            }
        }
        // Full pushed EVENTS and polling remain mutually exclusive PER CHANNEL: the event route
        // refuses a polled provider's channels below. The token may nevertheless coexist with a
        // polling provider because `/live/hints` carries no message — it wakes that provider's
        // one authoritative cursor, so there is still exactly one producer.

        let channels = match get(ENV_CHANNELS) {
            Some(spec) => parse_channel_spec(spec)?,
            None => file
                .channels
                .into_iter()
                .map(|c| ChannelInfo {
                    id: ChannelId(c.id),
                    label: c.label,
                    writable: c.writable,
                    // Configuration carries no alias: the operator's local name lives in the
                    // store, and `crate::ops` overlays it. `#39 channel-alias`.
                    alias: None,
                    // From the file, so not removable from the app.
                    added: false,
                    provider: c.provider,
                })
                .collect(),
        };
        let channels = resolve_channel_providers(channels, &providers)?;
        validate_channels(&channels)?;
        let speakable = SpeakableConfig {
            enabled: match get(ENV_SPEAKABLE) {
                None => file.speakable.enabled,
                Some(raw) => switch(raw, "speakable.enabled")?,
            },
        };

        let storage = storage_config(get(ENV_STORAGE_PATH), file.storage)?;
        let message_cache = file
            .message_cache
            .map(|table| message_cache_config(table, storage.path.as_deref()))
            .transpose()?;
        let summaries = summaries_config(
            get(ENV_SUMMARY_MODEL),
            get(ENV_SUMMARY_BACKEND),
            file.summaries,
        )?;
        let replay = replay_config(
            [
                get(ENV_REPLAY_ENABLED),
                get(ENV_REPLAY_MAX_CHARS),
                get(ENV_REPLAY_MAX_TURNS),
                get(ENV_REPLAY_TRANSPORT),
            ],
            file.replay,
        )?;

        Ok(Self {
            bind,
            public_base_url: get(ENV_PUBLIC_BASE_URL)
                .map(str::to_owned)
                .or(file.server.public_base_url),
            timezone,
            chat,
            providers,
            auth: AuthConfig {
                read_token,
                write_token,
            },
            ingest: IngestConfig {
                token: ingest_token,
            },
            channels,
            read_aloud: ReadAloudConfig {
                backend: match get(ENV_READ_ALOUD_BACKEND) {
                    None => file.read_aloud.backend,
                    Some("browser") => ReadAloudBackend::Browser,
                    Some("elevenlabs") => ReadAloudBackend::ElevenLabs,
                    Some("conversation") => ReadAloudBackend::Conversation,
                    Some(other) => {
                        return Err(ConfigError::Invalid {
                            field: "read_aloud.backend".to_owned(),
                            detail: format!(
                                "unknown backend {other:?}; choose browser, elevenlabs, or conversation"
                            ),
                        })
                    }
                },
                websocket_url: file.read_aloud.websocket_url,
            },
            speakable,
            elevenlabs: ElevenLabsConfig {
                agent_id: get(ENV_ELEVENLABS_AGENT_ID)
                    .map(str::to_owned)
                    .or(file.elevenlabs.agent_id),
                api_key: get(ENV_ELEVENLABS_API_KEY)
                    .map(Secret::new)
                    .or(file.elevenlabs.api_key),
                api_base: get(ENV_ELEVENLABS_API_BASE)
                    .map(str::to_owned)
                    .or(file.elevenlabs.api_base)
                    .unwrap_or_else(|| DEFAULT_ELEVENLABS_API_BASE.to_owned()),
                voice_id: get(ENV_ELEVENLABS_VOICE_ID)
                    .map(str::to_owned)
                    .or(file.elevenlabs.voice_id),
            },
            conversation: file.conversation,
            storage,
            summaries,
            replay,
            message_cache,
        })
    }
}

/// Resolve and check `[message_cache]`. `#208 persistent-cache`.
///
/// Every bound is refused rather than clamped, and `0` is refused rather than read as
/// "unlimited": a cache with no age or size limit is not something a typo should produce.
fn message_cache_config(
    file: FileMessageCache,
    storage_path: Option<&Path>,
) -> Result<MessageCacheConfig, ConfigError> {
    fn ranged(
        field: &str,
        value: i64,
        low: i64,
        high: i64,
        unit: &str,
    ) -> Result<i64, ConfigError> {
        if (low..=high).contains(&value) {
            Ok(value)
        } else {
            Err(ConfigError::Invalid {
                field: field.to_owned(),
                detail: format!(
                    "{value} is out of range; give {low} to {high} {unit} (0 is refused, not \
                     read as unlimited)"
                ),
            })
        }
    }
    let defaults = MessageCacheConfig::default();
    let enabled = file.enabled.unwrap_or(defaults.enabled);
    let path = file
        .path
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    if let Some(path) = &path {
        if !path.is_absolute() {
            return Err(ConfigError::Invalid {
                field: "message_cache.path".to_owned(),
                detail: format!("{} is relative; give an absolute path", path.display()),
            });
        }
        // Compared once `.` and `..` are resolved, so `/data/./s.sqlite3` is caught too. A
        // symbolic link is resolved when the cache opens, which checks again.
        let normalized = crate::cache::file::normalize_lexically;
        if storage_path.is_some_and(|store| normalized(store) == normalized(path)) {
            return Err(ConfigError::Invalid {
                field: "message_cache.path".to_owned(),
                detail: "is the same file as storage.path; the cache is a separate, disposable \
                         file, and erasing it must never touch the store"
                    .to_owned(),
            });
        }
    }
    if enabled && storage_path.is_none() {
        // The switch that turns the cache off, and the list of every cache file this server has
        // opened, are kept beside the store so that purging the store cannot turn the cache back
        // on. With no store there is nowhere to keep them.
        return Err(ConfigError::Invalid {
            field: "message_cache.enabled".to_owned(),
            detail: "needs storage.path: the cache's off switch is kept beside the store"
                .to_owned(),
        });
    }
    let narrow = |value: i64| u16::try_from(value).unwrap_or(u16::MAX);
    let max_days = narrow(ranged(
        "message_cache.max_days",
        file.max_days.unwrap_or(i64::from(defaults.max_days)),
        1,
        365,
        "days",
    )?);
    let max_megabytes = u32::try_from(ranged(
        "message_cache.max_megabytes",
        file.max_megabytes
            .unwrap_or(i64::from(defaults.max_megabytes)),
        16,
        4096,
        "megabytes",
    )?)
    .unwrap_or(u32::MAX);
    let timeline_fallback_ms = u64::try_from(ranged(
        "message_cache.timeline_fallback_ms",
        file.timeline_fallback_ms
            .unwrap_or(i64::try_from(defaults.timeline_fallback_ms).unwrap_or(i64::MAX)),
        1_000,
        60_000,
        "milliseconds",
    )?)
    .unwrap_or(u64::MAX);
    let mut channels: Vec<CacheChannelLimit> = Vec::new();
    for entry in file.channel {
        let id = entry.id.trim().to_owned();
        if id.is_empty() {
            return Err(ConfigError::Invalid {
                field: "message_cache.channel.id".to_owned(),
                detail: "is empty".to_owned(),
            });
        }
        if channels.iter().any(|limit| limit.id.as_str() == id) {
            return Err(ConfigError::Invalid {
                field: "message_cache.channel.id".to_owned(),
                detail: format!("{id:?} is listed twice"),
            });
        }
        let max_days = narrow(ranged(
            "message_cache.channel.max_days",
            entry.max_days,
            1,
            365,
            "days",
        )?);
        channels.push(CacheChannelLimit {
            id: ChannelId(id),
            max_days,
        });
    }
    Ok(MessageCacheConfig {
        enabled,
        path,
        max_days,
        max_megabytes,
        backfill: file.backfill.unwrap_or(defaults.backfill),
        timeline_fallback_ms,
        channels,
    })
}

/// Resolve the `[replay]` section.
/// Read an environment value as a boolean, REFUSING anything that is neither.
///
/// Deliberately stricter than the `matches!(raw, "1" | "true" | …)` form used for `replay.enabled`,
/// and for a reason that only applies to a setting whose default is ON: under that form every
/// unrecognised spelling — `TRUE`, `enabled`, a stray quote — reads as false, so a typo silently
/// turns the feature off while the operator believes they turned it on. An operator reaching for
/// this switch is A/B-testing a voice agent, and a run that was quietly the wrong arm is worse
/// than a server that refused to start and said which value it could not read.
fn switch(raw: &str, field: &'static str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(ConfigError::Invalid {
            field: field.to_owned(),
            detail: format!("{other:?} is neither on nor off; write true or false"),
        }),
    }
}

fn replay_config(
    from_env: [Option<&str>; 4],
    file: FileReplay,
) -> Result<ReplayConfig, ConfigError> {
    let [enabled_env, chars_env, turns_env, transport_env] = from_env;
    let defaults = ReplayConfig::default();
    let number =
        |raw: Option<&str>, field: &'static str, from_file: Option<usize>, fallback| match raw {
            Some(text) => text
                .trim()
                .parse::<usize>()
                .map_err(|e| ConfigError::Invalid {
                    field: field.to_owned(),
                    detail: format!("{text:?} is not a whole number ({e})"),
                }),
            None => Ok(from_file.unwrap_or(fallback)),
        };
    let enabled = match enabled_env {
        Some(raw) => matches!(raw.trim(), "1" | "true" | "yes" | "on"),
        None => file.enabled.unwrap_or(defaults.enabled),
    };
    let max_chars = number(
        chars_env,
        "replay.max_chars",
        file.max_chars,
        defaults.policy.max_chars,
    )?;
    let max_turns = number(
        turns_env,
        "replay.max_turns",
        file.max_turns,
        defaults.policy.max_turns,
    )?;
    if max_chars == 0 || max_turns == 0 {
        // Zero is not "unlimited" and must not be readable as it. A budget of nothing produces an
        // empty replay on every call, which the page then correctly reports as "the agent starts
        // fresh" — a feature that is silently off while the setting says it is on.
        return Err(ConfigError::Invalid {
            field: if max_chars == 0 {
                "replay.max_chars".to_owned()
            } else {
                "replay.max_turns".to_owned()
            },
            detail: "must be at least 1. Zero is not 'no limit'; it is a replay that is always \
                     empty while the interface says resuming is on. Use replay.enabled = false."
                .to_owned(),
        });
    }
    let transport_text = transport_env.map(str::to_owned).or(file.transport);
    let transport = match transport_text {
        Some(text) => {
            crate::replay::Transport::parse(&text).ok_or_else(|| ConfigError::Invalid {
                field: "replay.transport".to_owned(),
                detail: format!(
                    "{text:?} is not a transport. Use \"contextual_update\" (the default) or \
                     \"client_data\"."
                ),
            })?
        }
        None => defaults.transport,
    };
    Ok(ReplayConfig {
        enabled,
        policy: crate::replay::ReplayPolicy {
            max_chars,
            max_turns,
        },
        transport,
    })
}

/// What `summaries.backend` does now that there is only one summariser.
///
/// Three outcomes, and the middle one is the reason this is not a blanket refusal:
///
/// * `"extractive"` — **refused.** That summariser was removed. Quietly promoting it to the agent
///   would start paying a vendor, and start sending other people's channel text to one, on a
///   deployment whose own configuration file says the opposite.
/// * `"elevenlabs_agent"` — **accepted, loudly.** One release ago this was the documented way to
///   ask for real summaries, so it is sitting in live configuration files right now, and refusing
///   it would stop those servers booting for no behavioural gain: it names exactly what they get.
///   A warning is not silent acceptance — the operator is told the key is dead and to delete it.
/// * anything else — refused as an unknown value, saying the key is deprecated so the reply to
///   "then what IS the right value?" is in the same sentence.
fn deprecated_backend(raw: &str) -> Result<(), ConfigError> {
    let refuse = |detail: String| ConfigError::Invalid {
        field: "summaries.backend".to_owned(),
        detail,
    };
    match raw.trim() {
        "extractive" => Err(refuse(
            "the extractive summariser was REMOVED. It truncated rather than comprehended, and \
             the page already shows a clamped prefix without it. Summaries now always come from \
             the ElevenLabs agent named in [elevenlabs]; a deployment with no credentials there \
             shows every long message as failed rather than as truncated. Delete this line."
                .to_owned(),
        )),
        "elevenlabs_agent" => {
            tracing::warn!(
                setting = "summaries.backend",
                env = ENV_SUMMARY_BACKEND,
                "summaries.backend IS DEPRECATED AND SELECTS NOTHING. It is being IGNORED. There \
                 is one summariser — the ElevenLabs agent named in [elevenlabs] — and it is what \
                 you get whether or not this key is present. Delete the line (and \
                 VIBE_TALK_SUMMARY_BACKEND, if it is set in the environment)."
            );
            Ok(())
        }
        other => Err(refuse(format!(
            "{other:?} is not a summariser, and summaries.backend is DEPRECATED: it selects \
             nothing. There is one summariser — the ElevenLabs agent named in [elevenlabs] — and \
             it is what you get whether or not this key is present. Delete this line."
        ))),
    }
}

/// Resolve the `[summaries]` section.
fn summaries_config(
    model_from_env: Option<&str>,
    backend_from_env: Option<&str>,
    file: FileSummaries,
) -> Result<SummariesConfig, ConfigError> {
    let defaults = SummariesConfig::default();
    // BOTH SOURCES, INDEPENDENTLY, AND THE FILE FIRST.
    //
    // Not `env.or(file)`. That is the right shape for a setting where one source OVERRIDES the
    // other, and this is not one: neither form selects anything any more, so each is a separate
    // statement that has to be answered on its own terms. Written as an override, a deployment
    // with `VIBE_TALK_SUMMARY_BACKEND=elevenlabs_agent` in the environment and the removed
    // `backend = "extractive"` still in its file booted on the strength of the environment and was
    // never told the file was wrong — which is verbatim the case this refusal exists to prevent.
    //
    // The file is judged first because it is the one a person edits and keeps.
    if let Some(raw) = file.backend.as_deref() {
        deprecated_backend(raw)?;
    }
    if let Some(raw) = backend_from_env {
        deprecated_backend(raw)?;
    }
    // `summaries.model` is NOT a fallback for `summaries.llm.model`. It was documented for a long
    // time as read by nothing, so a deployment may still hold a value from an older summariser
    // that no Messages API provider knows; inheriting it would fail every call. The section names
    // its own model or takes the default.
    let llm = file.llm.map(llm_config).transpose()?;
    let summaries = SummariesConfig {
        threshold_chars: file.threshold_chars.unwrap_or(defaults.threshold_chars),
        target_chars: file.target_chars.unwrap_or(defaults.target_chars),
        context_messages: file.context_messages.unwrap_or(defaults.context_messages),
        model: model_from_env
            .map(str::to_owned)
            .or(file.model)
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty()),
        max_per_socket: file.max_per_socket.unwrap_or(defaults.max_per_socket),
        socket_idle_seconds: file
            .socket_idle_seconds
            .unwrap_or(defaults.socket_idle_seconds),
        reply_timeout_seconds: file
            .reply_timeout_seconds
            .unwrap_or(defaults.reply_timeout_seconds),
        llm,
    };
    if summaries.max_per_socket == 0 {
        // Zero is not "unlimited" and must not read as it. A conversation that may serve no
        // summaries is one that is opened and immediately recycled, for ever, at a mint and a
        // handshake apiece.
        return Err(ConfigError::Invalid {
            field: "summaries.max_per_socket".to_owned(),
            detail: "must be at least 1. Zero is not 'no limit'; it is a conversation opened and \
                     thrown away without asking it anything."
                .to_owned(),
        });
    }
    if summaries.reply_timeout_seconds == 0 {
        return Err(ConfigError::Invalid {
            field: "summaries.reply_timeout_seconds".to_owned(),
            detail: "must be at least 1, or every turn is abandoned before the agent can answer"
                .to_owned(),
        });
    }
    if summaries.target_chars == 0 {
        return Err(ConfigError::Invalid {
            field: "summaries.target_chars".to_owned(),
            detail: "must be at least 1, or every summary is the empty string".to_owned(),
        });
    }
    if summaries.threshold_chars < summaries.target_chars {
        // Otherwise the summary of a message just over the threshold is LONGER than the message,
        // which is a cost with a negative benefit and reads on screen as the page padding things
        // out.
        return Err(ConfigError::Invalid {
            field: "summaries.threshold_chars".to_owned(),
            detail: format!(
                "{} is below summaries.target_chars {}, so a summarised message would come back \
                 longer than it started",
                summaries.threshold_chars, summaries.target_chars
            ),
        });
    }
    Ok(summaries)
}

/// Request headers `[summaries.llm].extra_headers` may not set, because this server sets them
/// itself and a second copy would either be ignored or override a credential.
const RESERVED_LLM_HEADERS: &[&str] = &[
    "anthropic-version",
    "authorization",
    "content-length",
    "content-type",
    "host",
    "x-api-key",
];

/// Fragments that mark a request header as one that carries a credential. An `extra_headers` value
/// is written INLINE in the configuration file — a file that is shared, copied and pasted — so a
/// header whose name contains one of these is refused rather than trusted to hold no secret.
/// Credentials have their own settings, which read files.
const SECRET_HEADER_FRAGMENTS: &[&str] = &[
    "auth",
    "cookie",
    "credential",
    "key",
    "passwd",
    "password",
    "secret",
    "session",
    "signature",
    "token",
];

/// Resolve `[summaries.llm]`. `#233 llm-summarizer`.
fn llm_config(file: FileLlm) -> Result<LlmConfig, ConfigError> {
    let invalid = |field: &str, detail: String| ConfigError::Invalid {
        field: format!("summaries.llm.{field}"),
        detail,
    };
    for (written, field, instead) in [
        (file.api_key.is_some(), "api_key", "api_key_file"),
        (
            file.bearer_token.is_some(),
            "bearer_token",
            "bearer_token_file or bearer_token_command",
        ),
    ] {
        if written {
            return Err(invalid(
                field,
                format!(
                    "a secret must not be written in the configuration file; put it in a file \
                     readable only by this server and name that file with {instead}"
                ),
            ));
        }
    }
    let dialect = file.dialect.unwrap_or(LlmDialect::Anthropic);
    let text = |value: Option<String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let model = text(file.model).unwrap_or_else(|| DEFAULT_LLM_MODEL.to_owned());
    // Each of these is spliced into a URL path on the Vertex dialect, so each is held to the
    // characters its real values use. Anything else is a typo at best and a path injection at worst.
    let identifier = |field: &str, value: &str, extra: &str| -> Result<(), ConfigError> {
        if value.len() > 128
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || extra.contains(c))
        {
            return Err(invalid(
                field,
                format!(
                    "{value:?} is not a {field} name: use letters, digits, '-' and {extra:?}, at \
                     most 128 characters"
                ),
            ));
        }
        Ok(())
    };
    identifier("model", &model, "._@")?;
    let project = text(file.project);
    let region = text(file.region);
    if let Some(project) = &project {
        identifier("project", project, "._:")?;
    }
    if let Some(region) = &region {
        identifier("region", region, "")?;
    }
    let path = |field: &str, value: Option<String>| -> Result<Option<PathBuf>, ConfigError> {
        let Some(value) = text(value) else {
            return Ok(None);
        };
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            // The `storage.path` rule, for the same reason: a relative path is resolved against
            // whatever directory the process happened to start in.
            return Err(invalid(
                field,
                format!("{} is relative; give an absolute path", path.display()),
            ));
        }
        Ok(Some(path))
    };
    let api_key_file = path("api_key_file", file.api_key_file)?;
    let bearer_token_file = path("bearer_token_file", file.bearer_token_file)?;
    let client_identity_file = path("client_identity_file", file.client_identity_file)?;
    let ca_bundle_file = path("ca_bundle_file", file.ca_bundle_file)?;
    let bearer_token_command = match file.bearer_token_command {
        Some(argv) if argv.first().is_none_or(|program| program.trim().is_empty()) => {
            return Err(invalid(
                "bearer_token_command",
                "must name a program, as an argument list such as [\"gcloud\", \"auth\", \
                 \"print-access-token\"]; it is run without a shell"
                    .to_owned(),
            ))
        }
        other => other,
    };

    match dialect {
        LlmDialect::Anthropic => {
            for (field, set) in [
                ("project", project.is_some()),
                ("region", region.is_some()),
                ("bearer_token_file", bearer_token_file.is_some()),
                ("bearer_token_command", bearer_token_command.is_some()),
            ] {
                if set {
                    return Err(invalid(
                        field,
                        "is a Vertex setting and this section says dialect = \"anthropic\""
                            .to_owned(),
                    ));
                }
            }
            if api_key_file.is_none() {
                return Err(ConfigError::Missing(
                    "summaries.llm.api_key_file (the anthropic dialect authenticates with an API \
                     key read from a file)"
                        .to_owned(),
                ));
            }
        }
        LlmDialect::Vertex => {
            if project.is_none() {
                return Err(ConfigError::Missing(
                    "summaries.llm.project (the vertex dialect addresses a Google Cloud project)"
                        .to_owned(),
                ));
            }
            if region.is_none() {
                return Err(ConfigError::Missing(
                    "summaries.llm.region (the vertex dialect addresses a region, or \"global\")"
                        .to_owned(),
                ));
            }
            if bearer_token_file.is_some() && bearer_token_command.is_some() {
                return Err(invalid(
                    "bearer_token_command",
                    "set bearer_token_file OR bearer_token_command, not both: two sources for one \
                     credential is a question about which one won"
                        .to_owned(),
                ));
            }
            if bearer_token_file.is_none()
                && bearer_token_command.is_none()
                && client_identity_file.is_none()
                && api_key_file.is_none()
            {
                return Err(ConfigError::Missing(
                    "a credential for summaries.llm: bearer_token_file, bearer_token_command, \
                     client_identity_file or api_key_file"
                        .to_owned(),
                ));
            }
        }
    }

    let base_url = match text(file.base_url) {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => match dialect {
            LlmDialect::Anthropic => DEFAULT_ANTHROPIC_BASE_URL.to_owned(),
            LlmDialect::Vertex => vertex_base_url(region.as_deref().unwrap_or("global")),
        },
    };
    let parsed = reqwest::Url::parse(&base_url)
        .map_err(|error| invalid("base_url", format!("{base_url:?} is not a URL ({error})")))?;
    let loopback = matches!(
        parsed.host(),
        Some(url::Host::Ipv4(ip)) if ip.is_loopback()
    ) || matches!(parsed.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        || parsed.host_str() == Some("localhost");
    match parsed.scheme() {
        "https" => {}
        // Plain HTTP only to this machine, which is what a local mock or a sidecar is. Anywhere
        // else it would carry the credential and other people's channel text in the clear.
        "http" if loopback => {}
        other => {
            return Err(invalid(
                "base_url",
                format!(
                    "{base_url:?} uses {other:?}; use https (plain http is accepted only for a \
                     loopback address)"
                ),
            ))
        }
    }
    if parsed.query().is_some() || parsed.fragment().is_some() || !parsed.username().is_empty() {
        return Err(invalid(
            "base_url",
            "must be a plain base URL, with no credentials, query or fragment".to_owned(),
        ));
    }

    let mut extra_headers = BTreeMap::new();
    for (name, value) in file.extra_headers.unwrap_or_default() {
        let lower = name.trim().to_ascii_lowercase();
        if RESERVED_LLM_HEADERS.contains(&lower.as_str()) {
            return Err(invalid(
                "extra_headers",
                format!("{name:?} is set by this server and cannot be overridden here"),
            ));
        }
        if let Some(fragment) = SECRET_HEADER_FRAGMENTS
            .iter()
            .find(|fragment| lower.contains(*fragment))
        {
            // The value is not echoed: by the header's own name, it may be a credential.
            return Err(invalid(
                "extra_headers",
                format!(
                    "{name:?} looks like a header that carries a credential (its name contains \
                     {fragment:?}), and a value here is written in the configuration file itself. \
                     Put credentials in a file and name it with api_key_file, bearer_token_file or \
                     bearer_token_command"
                ),
            ));
        }
        if reqwest::header::HeaderName::from_bytes(lower.as_bytes()).is_err()
            || reqwest::header::HeaderValue::from_str(&value).is_err()
        {
            // The value is not echoed: it is not meant to be a secret, but a header is exactly
            // where somebody would paste one.
            return Err(invalid(
                "extra_headers",
                format!("{name:?} is not a valid header name, or its value is not a valid value"),
            ));
        }
        extra_headers.insert(lower, value);
    }

    let max_tokens = file.max_tokens.unwrap_or(DEFAULT_LLM_MAX_TOKENS);
    if !(1..=4096).contains(&max_tokens) {
        return Err(invalid(
            "max_tokens",
            format!("{max_tokens} is outside 1 to 4096; a summary is one sentence"),
        ));
    }
    let timeout_secs = file.timeout_secs.unwrap_or(DEFAULT_LLM_TIMEOUT_SECS);
    if !(1..=600).contains(&timeout_secs) {
        return Err(invalid(
            "timeout_secs",
            format!("{timeout_secs} is outside 1 to 600 seconds"),
        ));
    }
    let requests_per_minute = file
        .requests_per_minute
        .unwrap_or(DEFAULT_LLM_REQUESTS_PER_MINUTE);
    let burst = file.burst.unwrap_or(DEFAULT_LLM_BURST);
    if requests_per_minute == 0 || burst == 0 {
        return Err(invalid(
            if requests_per_minute == 0 {
                "requests_per_minute"
            } else {
                "burst"
            },
            "must be at least 1. Zero is not 'unlimited'; it is a summariser that never calls"
                .to_owned(),
        ));
    }
    Ok(LlmConfig {
        dialect,
        base_url,
        model,
        project,
        region,
        api_key_file,
        bearer_token_file,
        bearer_token_command,
        client_identity_file,
        ca_bundle_file,
        extra_headers,
        max_tokens,
        timeout_secs,
        requests_per_minute,
        burst,
    })
}

#[cfg(test)]
mod llm_tests;

/// Environment variable naming configuration OVERLAY files, separated by `:`, applied after any
/// `--config-overlay` flags. See [`merge_overlays`].
pub const ENV_CONFIG_OVERLAY: &str = "VIBE_TALK_CONFIG_OVERLAY";

/// Tables an overlay REPLACES whole instead of merging into, as paths from the document root.
///
/// `[summaries.llm]` is one because its keys only make sense together: a host that moves from the
/// shared file's `dialect = "anthropic"` to its own Vertex gateway must not inherit the shared
/// file's `api_key_file`, or that key would be sent as `x-api-key` to a gateway it was never
/// meant for. A merge has no way to say "and drop that key", so the section is taken from the
/// last layer that names it, exactly as written there.
pub const OVERLAY_REPLACES_WHOLE: &[&[&str]] = &[&["summaries", "llm"]];

/// Lay overlay TOML documents over a base document, table by table, and return the merged text.
///
/// The problem it solves: one configuration file shared by two hosts, where one host needs its own
/// `[summaries.llm]` (a different credential path, a different gateway) without editing the shared
/// file. Each overlay is a TOML document whose tables MERGE into the base: a key it sets replaces
/// the base's value, a table it names is merged key by key, and everything it does not mention is
/// left as the base has it. An ARRAY is a value like any other and is replaced whole, so an
/// overlay that writes `[[channels]]` replaces the channel list rather than appending to it. A
/// table in [`OVERLAY_REPLACES_WHOLE`] — `[summaries.llm]` — is replaced whole too: an overlay that
/// names it carries the whole section.
///
/// Overlays apply in order, each over the result of the last. Every document is parsed AND checked
/// against the configuration's shape on its own first, so a syntax error, a mistyped value or an
/// unknown key is reported against the file that has it, at its own line and column, rather than
/// against the merged text, which nobody has seen.
///
/// # Errors
///
/// [`ConfigError::Parse`] when the base does not parse or does not fit the configuration's shape;
/// [`ConfigError::Invalid`] naming the overlay when one does not, or when the merged document
/// cannot be written back out.
pub fn merge_overlays(base: &str, overlays: &[(String, String)]) -> Result<String, ConfigError> {
    toml::from_str::<FileConfig>(base)?;
    let mut merged: toml::Table = toml::from_str(base)?;
    for (label, text) in overlays {
        let refuse = |error: toml::de::Error| ConfigError::Invalid {
            field: format!("configuration overlay {label}"),
            detail: format!("did not parse: {error}"),
        };
        toml::from_str::<FileConfig>(text).map_err(refuse)?;
        let overlay: toml::Table = toml::from_str(text).map_err(refuse)?;
        merge_table(&mut merged, overlay, &mut Vec::new());
    }
    toml::to_string(&merged).map_err(|error| ConfigError::Invalid {
        field: "configuration overlay".to_owned(),
        detail: format!("the merged configuration could not be written back out: {error}"),
    })
}

fn merge_table(base: &mut toml::Table, overlay: toml::Table, path: &mut Vec<String>) {
    for (key, value) in overlay {
        path.push(key.clone());
        let whole = OVERLAY_REPLACES_WHOLE
            .iter()
            .any(|table| table.iter().copied().eq(path.iter().map(String::as_str)));
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(into)), toml::Value::Table(from)) if !whole => {
                merge_table(into, from, path);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
        path.pop();
    }
}

impl Config {
    /// [`Config::load`], with overlay files laid over the base before anything is resolved.
    ///
    /// `base` is `None` for a deployment configured from the environment alone. See
    /// [`merge_overlays`] for the merge rule.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] when a file cannot be read, and anything [`Config::from_toml_and_env`]
    /// or [`merge_overlays`] refuses.
    pub fn load_layered(base: Option<&Path>, overlays: &[PathBuf]) -> Result<Self, ConfigError> {
        let read = |path: &Path| {
            std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.display().to_string(),
                source,
            })
        };
        let text = base.map(read).transpose()?.unwrap_or_default();
        let mut layers = Vec::new();
        for overlay in overlays {
            layers.push((overlay.display().to_string(), read(overlay)?));
        }
        let env: BTreeMap<String, String> = std::env::vars().collect();
        if layers.is_empty() {
            return Self::from_toml_and_env(&text, &env);
        }
        Self::from_toml_and_env(&merge_overlays(&text, &layers)?, &env)
    }
}

/// Resolve the `[storage]` section.
///
/// The one rule worth spelling out is that the path must be **absolute**. A relative path is
/// resolved against the process's working directory, which inside a container is a directory in
/// the image — so a store configured that way works perfectly until the image is rebuilt and then
/// silently loses everything. That is the exact failure this setting exists to prevent, so it is
/// refused at load rather than warned about at runtime.
fn storage_config(from_env: Option<&str>, file: FileStorage) -> Result<StorageConfig, ConfigError> {
    let path = from_env
        .map(str::to_owned)
        .or(file.path)
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    if let Some(path) = &path {
        if !path.is_absolute() {
            return Err(ConfigError::Invalid {
                field: "storage.path".to_owned(),
                detail: format!(
                    "{} is relative; give an absolute path on a mounted volume, or a rebuild of \
                     the image silently discards everything stored so far",
                    path.display()
                ),
            });
        }
    }

    let retain_days = file
        .retain_days
        .unwrap_or(crate::store::DEFAULT_RETAIN_DAYS);
    let retention = Retention {
        max_conversations: file
            .max_conversations
            .unwrap_or(crate::store::DEFAULT_MAX_CONVERSATIONS),
        max_turns_per_conversation: file
            .max_turns_per_conversation
            .unwrap_or(crate::store::DEFAULT_MAX_TURNS_PER_CONVERSATION),
        max_summaries: file
            .max_summaries
            .unwrap_or(crate::store::DEFAULT_MAX_SUMMARIES),
        max_dismissals: file
            .max_dismissals
            .unwrap_or(crate::store::DEFAULT_MAX_DISMISSALS),
        retain_days,
        // `#208 persistent-cache`. Until it was its own setting, a cached summary lived for
        // `retain_days` from when it was made; unset, it still lives that long, now counted from
        // its last use, so a host that lowered `retain_days` for privacy keeps summaries no longer.
        summary_retain_days: file.summary_retain_days.unwrap_or(retain_days),
    };
    if retention.max_conversations == 0 {
        return Err(ConfigError::Invalid {
            field: "storage.max_conversations".to_owned(),
            detail: "must be at least 1; zero would delete every conversation as it was written"
                .to_owned(),
        });
    }
    if retention.max_turns_per_conversation == 0 {
        return Err(ConfigError::Invalid {
            field: "storage.max_turns_per_conversation".to_owned(),
            detail: "must be at least 1, or no turn can ever be recorded".to_owned(),
        });
    }
    if retention.max_summaries == 0 {
        return Err(ConfigError::Invalid {
            field: "storage.max_summaries".to_owned(),
            detail: "must be at least 1; zero would delete every cached summary as it was written"
                .to_owned(),
        });
    }
    if retention.max_dismissals == 0 {
        return Err(ConfigError::Invalid {
            field: "storage.max_dismissals".to_owned(),
            detail: "must be at least 1; zero would put every message the owner dealt with back \
                     in front of him the moment he cleared it"
                .to_owned(),
        });
    }
    Ok(StorageConfig { path, retention })
}

fn check_token_strength(field: &str, token: &Secret) -> Result<(), ConfigError> {
    if token.expose().len() < MIN_TOKEN_LEN {
        return Err(ConfigError::Invalid {
            field: field.to_owned(),
            detail: format!(
                "must be at least {MIN_TOKEN_LEN} characters; this token guards a bot that can \
                 post to your channels"
            ),
        });
    }
    Ok(())
}

impl Config {
    /// The first Discord-protocol provider, when there is one.
    #[must_use]
    pub fn discord(&self) -> Option<&DiscordConfig> {
        self.providers
            .iter()
            .find_map(|provider| match &provider.kind {
                ProviderKind::Discord(discord) => Some(discord),
                ProviderKind::Slack(_) => None,
            })
    }

    /// Mutable access to the first Discord-protocol provider, for tests and tools.
    pub fn discord_mut(&mut self) -> Option<&mut DiscordConfig> {
        self.providers
            .iter_mut()
            .find_map(|provider| match &mut provider.kind {
                ProviderKind::Discord(discord) => Some(discord),
                ProviderKind::Slack(_) => None,
            })
    }

    /// The provider configured under `key`.
    #[must_use]
    pub fn provider(&self, key: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|provider| provider.key == key)
    }

    /// The provider a channel belongs to when nothing says otherwise.
    #[must_use]
    pub fn default_provider_key(&self) -> Option<&str> {
        default_provider_key(&self.providers)
    }

    /// The shortest live poll interval of any provider, or `0` when none polls.
    #[must_use]
    pub fn live_poll_seconds(&self) -> u64 {
        self.providers
            .iter()
            .map(|provider| provider.live_poll_seconds)
            .filter(|seconds| *seconds > 0)
            .min()
            .unwrap_or(0)
    }
}

/// The provider a channel belongs to when it does not say: the only one, or the legacy section.
#[must_use]
pub fn default_provider_key(providers: &[ProviderConfig]) -> Option<&str> {
    match providers {
        [only] => Some(only.key.as_str()),
        _ => providers
            .iter()
            .find(|provider| provider.key == LEGACY_DISCORD_PROVIDER_KEY)
            .map(|provider| provider.key.as_str()),
    }
}

fn validate_poll_seconds(field: &str, seconds: u64) -> Result<(), ConfigError> {
    if seconds != 0 && seconds < crate::live::MIN_POLL_SECONDS {
        return Err(ConfigError::Invalid {
            field: field.to_owned(),
            detail: format!(
                "{seconds} is below the {} second floor. Every tick is one request to the chat provider \
                 per channel against a shared rate limit, and obeying Retry-After \
                 buys time, not quota; use 0 to turn live ingestion off.",
                crate::live::MIN_POLL_SECONDS
            ),
        });
    }
    Ok(())
}

fn validate_request_timeout(field: &str, seconds: u64) -> Result<(), ConfigError> {
    if !(1..=120).contains(&seconds) {
        return Err(ConfigError::Invalid {
            field: field.to_owned(),
            detail: "must be between 1 and 120 seconds".to_owned(),
        });
    }
    Ok(())
}

fn validate_display_name(field: &str, name: &str) -> Result<(), ConfigError> {
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(ConfigError::Invalid {
            field: field.to_owned(),
            detail: "use a non-empty display name without control characters".to_owned(),
        });
    }
    Ok(())
}

/// Read the shared limits from `[chat]`, or from `[discord]` where older files keep them.
fn chat_limits(chat: &FileChat, discord: &FileDiscord) -> Result<ChatLimits, ConfigError> {
    fn pick<T: Copy>(
        name: &str,
        chat: Option<T>,
        discord: Option<T>,
    ) -> Result<Option<T>, ConfigError> {
        match (chat, discord) {
            (Some(_), Some(_)) => Err(ConfigError::Invalid {
                field: format!("chat.{name}"),
                detail: format!("is also set as discord.{name}; keep exactly one"),
            }),
            (chat, discord) => Ok(chat.or(discord)),
        }
    }
    let default_fetch_limit = pick(
        "default_fetch_limit",
        chat.default_fetch_limit,
        discord.default_fetch_limit,
    )?
    .unwrap_or(DEFAULT_FETCH_LIMIT);
    let max_fetch_limit = pick(
        "max_fetch_limit",
        chat.max_fetch_limit,
        discord.max_fetch_limit,
    )?
    .unwrap_or(DEFAULT_MAX_FETCH_LIMIT);
    let max_count_scan = pick(
        "max_count_scan",
        chat.max_count_scan,
        discord.max_count_scan,
    )?
    .unwrap_or(DEFAULT_MAX_COUNT_SCAN);
    if default_fetch_limit == 0 || max_fetch_limit == 0 {
        return Err(ConfigError::Invalid {
            field: "discord.max_fetch_limit".to_owned(),
            detail: "fetch limits must be at least 1".to_owned(),
        });
    }
    if max_count_scan == 0 {
        return Err(ConfigError::Invalid {
            field: "discord.max_count_scan".to_owned(),
            detail: "must be at least 1, or counting can never answer anything".to_owned(),
        });
    }
    if default_fetch_limit > max_fetch_limit {
        return Err(ConfigError::Invalid {
            field: "discord.default_fetch_limit".to_owned(),
            detail: format!("{default_fetch_limit} exceeds max_fetch_limit {max_fetch_limit}"),
        });
    }
    Ok(ChatLimits {
        default_fetch_limit,
        max_fetch_limit,
        max_count_scan,
    })
}

/// Build the provider the legacy `[discord]` section describes, when it describes one.
///
/// Required when there is no `[[providers]]` entry, exactly as before multi-provider support.
/// Alongside other providers it is optional, but a section that configures a provider without a
/// credential is refused rather than silently ignored.
fn legacy_discord_provider<'e>(
    file: FileDiscord,
    get: &impl Fn(&str) -> Option<&'e str>,
    has_other_providers: bool,
) -> Result<Option<ProviderConfig>, ConfigError> {
    let Some(bot_token) = get(ENV_DISCORD_BOT_TOKEN)
        .map(Secret::new)
        .or(file.bot_token)
    else {
        if !has_other_providers {
            return Err(ConfigError::Missing("discord.bot_token".to_owned()));
        }
        let configures_a_provider = file.provider_name.is_some()
            || file.thread_api.is_some()
            || file.owner_user_id.is_some()
            || file.api_base.is_some()
            || file.request_timeout_seconds.is_some()
            || file.live_poll_seconds.is_some()
            || file.channel_registration.is_some()
            || file.upstream_read_marks.is_some()
            || file.markup.is_some()
            || file.max_post_chars.is_some()
            || file.max_post_bytes.is_some()
            || file.recent_view.is_some()
            || file.recent_view_shadow.is_some()
            || get(ENV_DISCORD_OWNER_USER_ID).is_some()
            || get(ENV_LIVE_POLL_SECONDS).is_some();
        if configures_a_provider {
            return Err(ConfigError::Invalid {
                field: "discord.bot_token".to_owned(),
                detail: "the [discord] section configures a provider but has no bot token; set \
                         one, or move those settings to a [[providers]] entry"
                    .to_owned(),
            });
        }
        return Ok(None);
    };
    if bot_token.expose().trim().is_empty() {
        return Err(ConfigError::Invalid {
            field: "discord.bot_token".to_owned(),
            detail: "is blank".to_owned(),
        });
    }

    let provider_name = file
        .provider_name
        .unwrap_or_else(|| "Discord".to_owned())
        .trim()
        .to_owned();
    validate_display_name("discord.provider_name", &provider_name)?;
    let request_timeout_seconds = file
        .request_timeout_seconds
        .unwrap_or(DEFAULT_PROVIDER_REQUEST_TIMEOUT_SECONDS);
    validate_request_timeout("discord.request_timeout_seconds", request_timeout_seconds)?;
    let live_poll_seconds = match get(ENV_LIVE_POLL_SECONDS) {
        Some(raw) => raw
            .trim()
            .parse::<u64>()
            .map_err(|e| ConfigError::Invalid {
                field: "discord.live_poll_seconds".to_owned(),
                detail: format!("{raw:?} is not a whole number of seconds ({e})"),
            })?,
        None => file.live_poll_seconds.unwrap_or(0),
    };
    validate_poll_seconds("discord.live_poll_seconds", live_poll_seconds)?;

    let discord = discord_protocol(
        "discord",
        provider_name.clone(),
        bot_token,
        get(ENV_DISCORD_OWNER_USER_ID)
            .map(str::to_owned)
            .or(file.owner_user_id),
        file.api_base,
        request_timeout_seconds,
        file.channel_registration,
        file.thread_api,
        file.upstream_read_marks,
        file.markup,
        PostSize {
            chars: file.max_post_chars,
            bytes: file.max_post_bytes,
        },
        (file.recent_view, file.recent_view_shadow),
    )?;
    Ok(Some(ProviderConfig {
        key: LEGACY_DISCORD_PROVIDER_KEY.to_owned(),
        name: provider_name,
        live_poll_seconds,
        kind: ProviderKind::Discord(discord),
    }))
}

/// Validate and assemble one Discord-protocol provider; `prefix` names its section in errors.
#[allow(clippy::too_many_arguments)]
fn discord_protocol(
    prefix: &str,
    provider_name: String,
    bot_token: Secret,
    owner_user_id: Option<String>,
    api_base: Option<String>,
    request_timeout_seconds: u64,
    channel_registration: Option<bool>,
    thread_api: Option<crate::threads::ThreadApi>,
    upstream_read_marks: Option<bool>,
    markup: Option<crate::render::Markup>,
    post_size: PostSize,
    (recent_view, recent_view_shadow): (Option<RecentView>, Option<bool>),
) -> Result<DiscordConfig, ConfigError> {
    let channel_registration = channel_registration.unwrap_or(false);
    let api_base = api_base.unwrap_or_else(|| DEFAULT_DISCORD_API_BASE.to_owned());
    let official_discord_host = is_official_discord_api_base(&api_base);
    if channel_registration && official_discord_host {
        return Err(ConfigError::Invalid {
            field: format!("{prefix}.channel_registration"),
            detail: "cannot be enabled with Discord's official API base: Discord does not implement the managed POST /channels and DELETE /channels/{id} contract; set the api_base to a compatible bridge"
                .to_owned(),
        });
    }
    if thread_api == Some(crate::threads::ThreadApi::Bridge) && official_discord_host {
        return Err(ConfigError::Invalid {
            field: format!("{prefix}.thread_api"),
            detail: "bridge threading requires the API base of a compatible bridge".to_owned(),
        });
    }
    let markup = markup.unwrap_or_default();
    let ceiling = if official_discord_host {
        Some((
            crate::discord::http::DISCORD_MAX_CONTENT_LEN,
            "characters",
            "Discord accepts",
        ))
    } else if markup == crate::render::Markup::GoogleChat {
        Some((
            crate::discord::http::GOOGLE_CHAT_MAX_MESSAGE_BYTES,
            "bytes",
            "Google Chat accepts",
        ))
    } else {
        None
    };
    validate_post_size(prefix, post_size, ceiling)?;
    let recent = recent_reads(
        prefix,
        thread_api.unwrap_or_default(),
        recent_view,
        recent_view_shadow,
    )?;
    Ok(DiscordConfig {
        provider_name,
        thread_api: thread_api.unwrap_or_default(),
        bot_token,
        owner_user_id: owner_user_id
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty()),
        api_base,
        request_timeout_seconds,
        channel_registration,
        upstream_read_marks: upstream_read_marks.unwrap_or(false),
        markup,
        max_post_chars: post_size.chars,
        max_post_bytes: post_size.bytes,
        recent,
    })
}

/// The smallest post size an operator may configure. Below it a reopened code fence and its
/// closing marker take most of a part, and a long message becomes dozens of posts.
pub const MIN_POST_SIZE: usize = 500;

/// A provider's configured post size: `max_post_chars` and `max_post_bytes`.
#[derive(Clone, Copy, Debug, Default)]
struct PostSize {
    chars: Option<usize>,
    bytes: Option<usize>,
}

/// Refuse a post size below [`MIN_POST_SIZE`], or above what the service behind the provider
/// accepts: `ceiling` is that service's maximum, the unit it counts, and how to say whose it is.
///
/// A ceiling in bytes bounds a character limit too: a character is at least one byte, so more
/// characters than the byte ceiling can never fit.
fn validate_post_size(
    prefix: &str,
    size: PostSize,
    ceiling: Option<(usize, &str, &str)>,
) -> Result<(), ConfigError> {
    for (name, value, unit) in [
        ("max_post_chars", size.chars, "characters"),
        ("max_post_bytes", size.bytes, "bytes"),
    ] {
        let Some(value) = value else { continue };
        let field = format!("{prefix}.{name}");
        if value < MIN_POST_SIZE {
            return Err(ConfigError::Invalid {
                field,
                detail: format!("must be at least {MIN_POST_SIZE}"),
            });
        }
        if let Some((max, ceiling_unit, whose)) = ceiling {
            if value > max && (unit == ceiling_unit || ceiling_unit == "bytes") {
                return Err(ConfigError::Invalid {
                    field,
                    detail: format!("{whose} at most {max} {ceiling_unit} in one message"),
                });
            }
        }
    }
    Ok(())
}

/// Validate a Discord-protocol provider's recent-read settings. `#208 persistent-cache`.
///
/// Both choose between two endpoints only a compatible bridge serves, so any other thread
/// protocol refuses them by name rather than ignoring them. A shadow compares the `/messages`
/// read with the Flat view, so it means nothing once Flat answers the read itself.
fn recent_reads(
    prefix: &str,
    thread_api: crate::threads::ThreadApi,
    view: Option<RecentView>,
    shadow: Option<bool>,
) -> Result<RecentReads, ConfigError> {
    if thread_api != crate::threads::ThreadApi::Bridge {
        for (name, set) in [
            ("recent_view", view.is_some()),
            ("recent_view_shadow", shadow.is_some()),
        ] {
            if set {
                return Err(ConfigError::Invalid {
                    field: format!("{prefix}.{name}"),
                    detail: "applies only with thread_api = \"bridge\": it chooses between two \
                             endpoints of a compatible bridge"
                        .to_owned(),
                });
            }
        }
    }
    let recent = RecentReads {
        view: view.unwrap_or_default(),
        shadow: shadow.unwrap_or(false),
    };
    if recent.shadow && recent.view == RecentView::Flat {
        return Err(ConfigError::Invalid {
            field: format!("{prefix}.recent_view_shadow"),
            detail: "compares the /messages read with the Flat view, so it does not apply with \
                     recent_view = \"flat\"; remove it"
                .to_owned(),
        });
    }
    Ok(recent)
}

/// Whether `key` may name a provider: short, lowercase, and safe in a setting or env var name.
fn valid_provider_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// The environment variable that supplies one `[[providers]]` entry's token.
#[must_use]
pub fn provider_token_env(key: &str) -> String {
    format!(
        "VIBE_TALK_PROVIDER_{}_TOKEN",
        key.to_ascii_uppercase().replace('-', "_")
    )
}

/// Validate and assemble one `[[providers]]` entry.
fn provider_entry<'e>(
    entry: FileProvider,
    get: &impl Fn(&str) -> Option<&'e str>,
) -> Result<ProviderConfig, ConfigError> {
    let key = entry.key.trim().to_owned();
    if !valid_provider_key(&key) {
        return Err(ConfigError::Invalid {
            field: "providers.key".to_owned(),
            detail: format!(
                "{key:?} must be 1 to 32 lowercase letters, digits, or hyphens, starting with a \
                 letter or digit"
            ),
        });
    }
    let field = |name: &str| format!("providers.{key}.{name}");
    let token = get(&provider_token_env(&key))
        .map(Secret::new)
        .or(entry.token)
        .ok_or_else(|| ConfigError::Missing(field("token")))?;
    if token.expose().trim().is_empty() {
        return Err(ConfigError::Invalid {
            field: field("token"),
            detail: "is blank".to_owned(),
        });
    }
    let request_timeout_seconds = entry
        .request_timeout_seconds
        .unwrap_or(DEFAULT_PROVIDER_REQUEST_TIMEOUT_SECONDS);
    validate_request_timeout(&field("request_timeout_seconds"), request_timeout_seconds)?;
    let live_poll_seconds = entry.live_poll_seconds.unwrap_or(0);
    validate_poll_seconds(&field("live_poll_seconds"), live_poll_seconds)?;
    let refuse = |name: &str, kind: &str| ConfigError::Invalid {
        field: field(name),
        detail: format!("does not apply to a provider of kind {kind:?}"),
    };
    let kind_name = entry.kind.trim().to_owned();
    let name = entry
        .name
        .unwrap_or_else(|| {
            if kind_name == "slack" {
                "Slack".to_owned()
            } else {
                "Discord".to_owned()
            }
        })
        .trim()
        .to_owned();
    validate_display_name(&field("name"), &name)?;
    let kind = match kind_name.as_str() {
        "discord" => {
            if entry.registered_channels_writable.is_some() {
                return Err(refuse("registered_channels_writable", "discord"));
            }
            if entry.channel_discovery.is_some() {
                return Err(refuse("channel_discovery", "discord"));
            }
            // A Discord-protocol bridge opts in to clearing with `upstream_read_marks`.
            if entry.upstream_clear.is_some() {
                return Err(refuse("upstream_clear", "discord"));
            }
            ProviderKind::Discord(discord_protocol(
                &format!("providers.{key}"),
                name.clone(),
                token,
                entry.owner_user_id,
                entry.api_base,
                request_timeout_seconds,
                entry.channel_registration,
                entry.thread_api,
                entry.upstream_read_marks,
                entry.markup,
                PostSize {
                    chars: entry.max_post_chars,
                    bytes: entry.max_post_bytes,
                },
                (entry.recent_view, entry.recent_view_shadow),
            )?)
        }
        "slack" => {
            if entry.thread_api.is_some() {
                return Err(refuse("thread_api", "slack"));
            }
            if entry.recent_view.is_some() {
                return Err(refuse("recent_view", "slack"));
            }
            if entry.recent_view_shadow.is_some() {
                return Err(refuse("recent_view_shadow", "slack"));
            }
            if entry.upstream_read_marks.is_some() {
                return Err(refuse("upstream_read_marks", "slack"));
            }
            // A Slack provider's text is Slack's by definition; there is nothing to choose.
            if entry.markup.is_some() {
                return Err(refuse("markup", "slack"));
            }
            let api_base = entry
                .api_base
                .unwrap_or_else(|| DEFAULT_SLACK_API_BASE.to_owned());
            if !reqwest::Url::parse(&api_base)
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
            {
                return Err(ConfigError::Invalid {
                    field: field("api_base"),
                    detail: format!("{api_base:?} is not an http or https URL"),
                });
            }
            let post_size = PostSize {
                chars: entry.max_post_chars,
                bytes: entry.max_post_bytes,
            };
            let ceiling = crate::slack::is_official_slack_api_base(&api_base).then_some((
                crate::slack::SLACK_MAX_TEXT_CHARS,
                "characters",
                "Slack keeps",
            ));
            validate_post_size(&format!("providers.{key}"), post_size, ceiling)?;
            ProviderKind::Slack(SlackSettings {
                api_base,
                token,
                owner_user_id: entry
                    .owner_user_id
                    .map(|id| id.trim().to_owned())
                    .filter(|id| !id.is_empty()),
                request_timeout_seconds,
                channel_registration: entry.channel_registration.unwrap_or(true),
                registered_channels_writable: entry.registered_channels_writable.unwrap_or(false),
                channel_discovery: entry.channel_discovery.unwrap_or(true),
                upstream_clear: entry.upstream_clear.unwrap_or(false),
                max_post_chars: post_size.chars,
                max_post_bytes: post_size.bytes,
            })
        }
        other => {
            return Err(ConfigError::Invalid {
                field: field("kind"),
                detail: format!("unknown kind {other:?}; choose discord or slack"),
            })
        }
    };
    Ok(ProviderConfig {
        key,
        name,
        live_poll_seconds,
        kind,
    })
}

/// Give every channel the key of the provider it belongs to, refusing any that cannot be placed.
fn resolve_channel_providers(
    channels: Vec<ChannelInfo>,
    providers: &[ProviderConfig],
) -> Result<Vec<ChannelInfo>, ConfigError> {
    let default = default_provider_key(providers);
    channels
        .into_iter()
        .map(|mut channel| {
            let key = match channel.provider.take() {
                Some(named) => {
                    let named = named.trim().to_owned();
                    if !providers.iter().any(|provider| provider.key == named) {
                        return Err(ConfigError::Invalid {
                            field: "channels.provider".to_owned(),
                            detail: format!(
                                "channel {} names provider {named:?}, which is not configured",
                                channel.id
                            ),
                        });
                    }
                    named
                }
                None => default
                    .ok_or_else(|| ConfigError::Invalid {
                        field: "channels.provider".to_owned(),
                        detail: format!(
                            "channel {} must name its provider: this deployment has several and \
                             no legacy [discord] section to default to",
                            channel.id
                        ),
                    })?
                    .to_owned(),
            };
            channel.provider = Some(key);
            Ok(channel)
        })
        .collect()
}

fn validate_channels(channels: &[ChannelInfo]) -> Result<(), ConfigError> {
    if channels.is_empty() {
        return Err(ConfigError::Missing("channels".to_owned()));
    }
    let mut seen = std::collections::BTreeSet::new();
    for channel in channels {
        if channel.id.as_str().is_empty() {
            return Err(ConfigError::Invalid {
                field: "channels.id".to_owned(),
                detail: "is blank".to_owned(),
            });
        }
        if !seen.insert(channel.id.clone()) {
            return Err(ConfigError::Invalid {
                field: "channels.id".to_owned(),
                detail: format!("{} is listed twice", channel.id),
            });
        }
    }
    Ok(())
}

/// Parse the `VIBE_TALK_CHANNELS` form: `id:label[:rw|:ro[:provider]]`, comma-separated.
///
/// A missing mode means read-only; a missing provider means the deployment's default provider.
fn parse_channel_spec(spec: &str) -> Result<Vec<ChannelInfo>, ConfigError> {
    let mut out = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let mut parts = entry.split(':');
        let id = parts.next().unwrap_or_default().trim();
        let label = parts.next().unwrap_or_default().trim();
        let mode = parts.next().unwrap_or("ro").trim();
        let provider = parts
            .next()
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .map(str::to_owned);
        if parts.next().is_some() {
            return Err(ConfigError::Invalid {
                field: ENV_CHANNELS.to_owned(),
                detail: format!("{entry:?} has more than four colon-separated fields"),
            });
        }
        if id.is_empty() || label.is_empty() {
            return Err(ConfigError::Invalid {
                field: ENV_CHANNELS.to_owned(),
                detail: format!("{entry:?} must be id:label[:rw|:ro[:provider]]"),
            });
        }
        let writable = match mode {
            "rw" => true,
            "ro" => false,
            other => {
                return Err(ConfigError::Invalid {
                    field: ENV_CHANNELS.to_owned(),
                    detail: format!("{other:?} is not rw or ro"),
                })
            }
        };
        out.push(ChannelInfo {
            id: ChannelId(id.to_owned()),
            label: label.to_owned(),
            writable,
            alias: None,
            added: false,
            provider,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const FULL: &str = r#"
[server]
bind = "127.0.0.1:9000"

[discord]
bot_token = "file-discord-token"

[auth]
read_token = "read-token-that-is-long-enough"
write_token = "write-token-that-is-long-enough"

[[channels]]
id = "111"
label = "lead"
writable = true
"#;

    /// Slack alongside a Google Chat bridge, with no legacy `[discord]` section.
    const MULTI: &str = r#"
[auth]
read_token = "read-token-that-is-long-enough"
write_token = "write-token-that-is-long-enough"

[chat]
default_fetch_limit = 30

[[providers]]
key = "gchat"
kind = "discord"
name = "Google Chat"
api_base = "http://127.0.0.1:18765/api/v10"
token = "bridge-token"
channel_registration = true
thread_api = "off"

[[providers]]
key = "slack"
kind = "slack"
token = "xoxb-slack-token"
owner_user_id = " U0OWNER001 "
live_poll_seconds = 15

[[channels]]
id = "900000000000000201"
label = "homebase"
provider = "gchat"

[[channels]]
id = "C0123ABCDE"
label = "team"
writable = true
provider = "slack"
"#;

    #[test]
    fn several_providers_load_without_a_legacy_discord_section() {
        let cfg = Config::from_toml_and_env(MULTI, &env(&[])).expect("valid multi config");
        let keys: Vec<&str> = cfg.providers.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["gchat", "slack"]);
        assert_eq!(cfg.default_provider_key(), None);
        assert_eq!(cfg.chat.default_fetch_limit, 30);
        assert_eq!(cfg.live_poll_seconds(), 15);

        let gchat = cfg.provider("gchat").expect("gchat");
        assert_eq!(gchat.name, "Google Chat");
        assert_eq!(gchat.namespace(), "http://127.0.0.1:18765/api/v10");
        let ProviderKind::Discord(bridge) = &gchat.kind else {
            panic!("gchat is a Discord-protocol bridge");
        };
        assert!(bridge.channel_registration);
        assert_eq!(bridge.thread_api, crate::threads::ThreadApi::Off);
        assert_eq!(bridge.bot_token.expose(), "bridge-token");

        let slack = cfg.provider("slack").expect("slack");
        assert_eq!(slack.name, "Slack");
        assert_eq!(slack.namespace(), "slack:https://slack.com/api");
        assert_eq!(slack.owner_user_id(), Some("U0OWNER001"));
        let ProviderKind::Slack(settings) = &slack.kind else {
            panic!("slack is a Slack provider");
        };
        assert_eq!(settings.api_base, DEFAULT_SLACK_API_BASE);
        assert!(
            settings.channel_registration,
            "Slack links are accepted by default"
        );
        assert!(settings.channel_discovery);
        assert!(!settings.registered_channels_writable);
        assert!(
            !settings.upstream_clear,
            "clearing unread in Slack is off unless the deployment turns it on"
        );

        let providers: Vec<Option<&str>> = cfg
            .channels
            .iter()
            .map(|channel| channel.provider.as_deref())
            .collect();
        assert_eq!(providers, [Some("gchat"), Some("slack")]);
    }

    /// The limit a Slack provider's client is built with, from its settings.
    fn slack_post_limit(cfg: &Config, key: &str) -> crate::chat::PostLimit {
        let ProviderKind::Slack(settings) = &cfg.provider(key).expect("slack").kind else {
            panic!("{key} is a Slack provider");
        };
        crate::slack::SlackConfig {
            provider_name: "Slack".to_owned(),
            api_base: settings.api_base.clone(),
            token: settings.token.clone(),
            owner_user_id: None,
            request_timeout_seconds: settings.request_timeout_seconds,
            channel_registration: settings.channel_registration,
            registered_channels_writable: settings.registered_channels_writable,
            channel_discovery: settings.channel_discovery,
            upstream_clear: settings.upstream_clear,
            max_post_chars: settings.max_post_chars,
            max_post_bytes: settings.max_post_bytes,
        }
        .post_limit()
    }

    /// The limit a Discord-protocol provider's client is built with.
    fn bridge_post_limit(cfg: &Config, key: &str) -> crate::chat::PostLimit {
        let ProviderKind::Discord(discord) = &cfg.provider(key).expect("provider").kind else {
            panic!("{key} is a Discord-protocol provider");
        };
        discord.post_limit()
    }

    /// `#263 post-split-per-provider`. Every post used to be cut at Discord's 2000 characters;
    /// each provider now says how much one post may carry, counted as that provider counts it.
    #[test]
    fn each_provider_says_how_much_one_post_may_carry() {
        use crate::chat::{PostEscaping, PostLimit};

        // Discord itself takes its own 2000 characters, and a byte limit only adds to that.
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(bridge_post_limit(&cfg, "discord"), PostLimit::chars(2000));
        let text = FULL.replace("[discord]", "[discord]\nmax_post_bytes = 1500");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("a byte limit for Discord");
        assert_eq!(
            bridge_post_limit(&cfg, "discord"),
            PostLimit {
                max_chars: Some(2000),
                max_bytes: Some(1500),
                escaping: PostEscaping::Verbatim,
            }
        );

        // A bridge that says nothing is sent what bridges have always been sent: nothing longer
        // than it has accepted before. Slack itself, its own ceiling less a margin, escaped.
        let cfg = Config::from_toml_and_env(MULTI, &env(&[])).expect("valid multi config");
        assert_eq!(bridge_post_limit(&cfg, "gchat"), PostLimit::chars(2000));
        assert_eq!(
            slack_post_limit(&cfg, "slack"),
            PostLimit::chars(crate::slack::SLACK_DEFAULT_POST_CHARS)
                .escaped(PostEscaping::HtmlEntities)
        );

        // A bridge that says what it accepts is held to exactly that, in the unit it gave.
        let text = MULTI.replace(
            "thread_api = \"off\"",
            "thread_api = \"off\"\nmax_post_bytes = 30000",
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("a bridge byte limit");
        assert_eq!(bridge_post_limit(&cfg, "gchat"), PostLimit::bytes(30_000));

        // A bridge serving Google Chat is held to Google Chat's own ceiling as well, so a limit
        // in characters cannot carry more bytes than Google Chat takes.
        let text = MULTI.replace(
            "thread_api = \"off\"",
            "thread_api = \"off\"\nmarkup = \"google-chat\"\nmax_post_chars = 20000",
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("a Google Chat bridge");
        assert_eq!(
            bridge_post_limit(&cfg, "gchat"),
            PostLimit {
                max_chars: Some(20_000),
                max_bytes: Some(crate::discord::http::GOOGLE_CHAT_MAX_MESSAGE_BYTES),
                escaping: PostEscaping::Verbatim,
            }
        );

        // A Slack bridge that says nothing, and one that says it takes four thousand bytes.
        let bridged = MULTI.replace(
            "kind = \"slack\"\n",
            "kind = \"slack\"\napi_base = \"http://127.0.0.1:18766/api\"\n",
        );
        let cfg = Config::from_toml_and_env(&bridged, &env(&[])).expect("a Slack bridge");
        assert_eq!(
            slack_post_limit(&cfg, "slack"),
            PostLimit::chars(2000).escaped(PostEscaping::HtmlEntities)
        );
        let text = bridged.replace(
            "live_poll_seconds = 15",
            "live_poll_seconds = 15\nmax_post_bytes = 4000",
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("a Slack bridge limit");
        assert_eq!(
            slack_post_limit(&cfg, "slack"),
            PostLimit::bytes(4000).escaped(PostEscaping::HtmlEntities)
        );
    }

    #[test]
    fn a_post_size_the_service_would_not_accept_is_refused_by_name() {
        let refused = |text: &str, expected: &str| {
            let error = Config::from_toml_and_env(text, &env(&[]))
                .expect_err("a post size out of range must not start the server");
            assert!(
                matches!(&error, ConfigError::Invalid { field, .. } if field == expected),
                "{expected}: {error}"
            );
        };
        refused(
            &FULL.replace("[discord]", "[discord]\nmax_post_chars = 2001"),
            "discord.max_post_chars",
        );
        refused(
            &FULL.replace("[discord]", "[discord]\nmax_post_bytes = 499"),
            "discord.max_post_bytes",
        );
        refused(
            &MULTI.replace(
                "thread_api = \"off\"",
                "thread_api = \"off\"\nmarkup = \"google-chat\"\nmax_post_bytes = 32001",
            ),
            "providers.gchat.max_post_bytes",
        );
        refused(
            &MULTI.replace(
                "live_poll_seconds = 15",
                "live_poll_seconds = 15\nmax_post_chars = 40001",
            ),
            "providers.slack.max_post_chars",
        );
        // Within range is accepted, and a bridge's own ceiling is the operator's to know.
        for text in [
            MULTI.replace(
                "thread_api = \"off\"",
                "thread_api = \"off\"\nmax_post_chars = 100000",
            ),
            MULTI.replace(
                "live_poll_seconds = 15",
                "live_poll_seconds = 15\nmax_post_chars = 40000\nmax_post_bytes = 90000",
            ),
        ] {
            Config::from_toml_and_env(&text, &env(&[])).expect("within range");
        }
    }

    #[test]
    fn a_provider_token_can_come_from_the_environment() {
        let text = MULTI.replace("token = \"xoxb-slack-token\"\n", "");
        assert!(matches!(
            Config::from_toml_and_env(&text, &env(&[])),
            Err(ConfigError::Missing(field)) if field == "providers.slack.token"
        ));
        let cfg = Config::from_toml_and_env(
            &text,
            &env(&[("VIBE_TALK_PROVIDER_SLACK_TOKEN", "xoxp-from-env")]),
        )
        .expect("token from the environment");
        assert_eq!(
            cfg.provider("slack").expect("slack").token().expose(),
            "xoxp-from-env"
        );
        assert_eq!(
            provider_token_env("google-chat"),
            "VIBE_TALK_PROVIDER_GOOGLE_CHAT_TOKEN"
        );
    }

    #[test]
    fn the_legacy_section_is_the_default_provider_beside_new_ones() {
        let text = format!(
            "{FULL}\n[[providers]]\nkey = \"slack\"\nkind = \"slack\"\ntoken = \"xoxb-t\"\n\n\
             [[channels]]\nid = \"C0123ABCDE\"\nlabel = \"team\"\nprovider = \"slack\"\n"
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("legacy plus slack");
        assert_eq!(cfg.providers[0].key, LEGACY_DISCORD_PROVIDER_KEY);
        assert_eq!(
            cfg.default_provider_key(),
            Some(LEGACY_DISCORD_PROVIDER_KEY)
        );
        assert_eq!(cfg.channels[0].provider.as_deref(), Some("discord"));
        assert_eq!(cfg.channels[1].provider.as_deref(), Some("slack"));
        assert!(cfg.discord().is_some());
    }

    #[test]
    fn a_slack_provider_may_opt_in_to_clearing_unread_in_slack() {
        // `#258 slack-clear-unread`.
        let text = MULTI.replace(
            "live_poll_seconds = 15",
            "live_poll_seconds = 15\nupstream_clear = true",
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid");
        let ProviderKind::Slack(settings) = &cfg.provider("slack").expect("slack").kind else {
            panic!("slack is a Slack provider");
        };
        assert!(settings.upstream_clear);
        let router = crate::providers::ChatRouter::from_config(&cfg).expect("clients");
        assert!(crate::chat::ChatClient::supports_upstream_clear(&router));
        let wrong = MULTI.replace(
            "live_poll_seconds = 15",
            "live_poll_seconds = 15\nupstream_clear = \"yes\"",
        );
        assert!(matches!(
            Config::from_toml_and_env(&wrong, &env(&[])),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn provider_mistakes_are_refused_by_name() {
        let invalid = |text: String, wanted: &str| match Config::from_toml_and_env(&text, &env(&[]))
        {
            Err(ConfigError::Invalid { field, .. }) => assert_eq!(field, wanted, "{text}"),
            other => panic!("expected {wanted} to be refused, got {other:?}"),
        };
        // A channel with no provider, in a deployment with several and no legacy default.
        invalid(
            MULTI.replace("provider = \"gchat\"\n", ""),
            "channels.provider",
        );
        // A channel naming a provider that does not exist.
        invalid(
            MULTI.replace("provider = \"slack\"\n", "provider = \"teams\"\n"),
            "channels.provider",
        );
        invalid(
            MULTI.replace("key = \"slack\"", "key = \"Slack\""),
            "providers.key",
        );
        invalid(
            MULTI.replace("key = \"slack\"", "key = \"gchat\""),
            "providers.key",
        );
        invalid(
            MULTI.replace("kind = \"slack\"", "kind = \"teams\""),
            "providers.slack.kind",
        );
        invalid(
            MULTI.replace("live_poll_seconds = 15", "live_poll_seconds = 2"),
            "providers.slack.live_poll_seconds",
        );
        invalid(
            MULTI.replace(
                "live_poll_seconds = 15",
                "live_poll_seconds = 15\nthread_api = \"off\"",
            ),
            "providers.slack.thread_api",
        );
        invalid(
            MULTI.replace(
                "thread_api = \"off\"",
                "thread_api = \"off\"\nchannel_discovery = true",
            ),
            "providers.gchat.channel_discovery",
        );
        // A Discord-protocol bridge opts in to clearing with `upstream_read_marks` instead.
        invalid(
            MULTI.replace(
                "thread_api = \"off\"",
                "thread_api = \"off\"\nupstream_clear = true",
            ),
            "providers.gchat.upstream_clear",
        );
        invalid(
            MULTI.replace(
                "live_poll_seconds = 15",
                "live_poll_seconds = 15\napi_base = \"ftp://x\"",
            ),
            "providers.slack.api_base",
        );
        // A [discord] section that configures a provider but forgot its credential.
        invalid(
            format!("[discord]\nprovider_name = \"Discord\"\n{MULTI}"),
            "discord.bot_token",
        );
        // The shared limits may live in one place, not both.
        invalid(
            format!("[discord]\ndefault_fetch_limit = 20\n{MULTI}"),
            "chat.default_fetch_limit",
        );
    }

    #[test]
    fn a_legacy_discord_section_may_carry_only_the_shared_limits() {
        let text = format!(
            "[discord]\nmax_count_scan = 900\n{}",
            MULTI.replace("[chat]\ndefault_fetch_limit = 30\n", "")
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("limits only");
        assert_eq!(cfg.chat.max_count_scan, 900);
        assert!(cfg
            .discord()
            .is_some_and(|discord| discord.provider_name == "Google Chat"));
    }

    #[test]
    fn an_ingest_token_may_wake_a_polled_provider_without_becoming_a_second_source() {
        let with_ingest = MULTI.replace(
            "[chat]",
            "[ingest]\ntoken = \"ingest-token-that-is-long-enough\"\n\n[chat]",
        );
        Config::from_toml_and_env(&with_ingest, &env(&[]))
            .expect("the bridge pushes while Slack is polled");

        let single = FULL.replace("[discord]", "[discord]\nlive_poll_seconds = 30")
            + "\n[ingest]\ntoken = \"ingest-token-that-is-long-enough\"\n";
        let config = Config::from_toml_and_env(&single, &env(&[]))
            .expect("the token carries content-free hints beside the one poll cursor");
        assert!(config.ingest.enabled());
        assert_eq!(config.providers[0].live_poll_seconds, 30);
    }

    #[test]
    fn the_channel_environment_form_may_name_a_provider() {
        let cfg = Config::from_toml_and_env(
            MULTI,
            &env(&[(
                ENV_CHANNELS,
                "900000000000000201:homebase:ro:gchat,C0123ABCDE:team:rw:slack",
            )]),
        )
        .expect("environment channels");
        assert_eq!(cfg.channels[1].provider.as_deref(), Some("slack"));
        assert!(cfg.channels[1].writable);
        assert!(matches!(
            Config::from_toml_and_env(MULTI, &env(&[(ENV_CHANNELS, "1:a:rw:slack:extra")])),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn file_only_configuration_loads() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(cfg.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(
            cfg.discord().expect("discord provider").bot_token.expose(),
            "file-discord-token"
        );
        assert_eq!(cfg.channels.len(), 1);
        assert!(cfg.channels[0].writable);
        assert_eq!(
            cfg.discord().expect("discord provider").api_base,
            DEFAULT_DISCORD_API_BASE
        );
    }

    #[test]
    fn environment_overrides_the_file() {
        let cfg = Config::from_toml_and_env(
            FULL,
            &env(&[
                (ENV_BIND, "0.0.0.0:1234"),
                (ENV_DISCORD_BOT_TOKEN, "env-discord-token"),
            ]),
        )
        .expect("valid config");
        assert_eq!(cfg.bind.to_string(), "0.0.0.0:1234");
        assert_eq!(
            cfg.discord().expect("discord provider").bot_token.expose(),
            "env-discord-token"
        );
    }

    #[test]
    fn empty_environment_value_does_not_override() {
        // An unset variable rendered as "" by a container runtime must not blank out the file.
        let cfg = Config::from_toml_and_env(FULL, &env(&[(ENV_DISCORD_BOT_TOKEN, "")]))
            .expect("valid config");
        assert_eq!(
            cfg.discord().expect("discord provider").bot_token.expose(),
            "file-discord-token"
        );
    }

    #[test]
    fn missing_bot_token_is_refused() {
        let text = FULL.replace("bot_token = \"file-discord-token\"", "");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Missing(field) if field == "discord.bot_token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn short_api_token_is_refused() {
        let text = FULL.replace("read-token-that-is-long-enough", "short");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "auth.read_token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn identical_read_and_write_tokens_are_refused() {
        let text = FULL.replace(
            "write-token-that-is-long-enough",
            "read-token-that-is-long-enough",
        );
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "auth.write_token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn no_channels_is_refused() {
        let text = FULL
            .split("[[channels]]")
            .next()
            .expect("prefix")
            .to_owned();
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Missing(field) if field == "channels"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn duplicate_channel_ids_are_refused() {
        let err = Config::from_toml_and_env(
            FULL,
            &env(&[(ENV_CHANNELS, "111:lead:rw,111:lead-again:ro")]),
        )
        .expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "channels.id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn channel_spec_defaults_to_read_only() {
        let cfg =
            Config::from_toml_and_env(FULL, &env(&[(ENV_CHANNELS, "222:ops , 333:build:rw")]))
                .expect("valid config");
        assert_eq!(cfg.channels.len(), 2);
        assert_eq!(cfg.channels[0].label, "ops");
        assert!(
            !cfg.channels[0].writable,
            "a channel with no mode must default to read-only"
        );
        assert!(cfg.channels[1].writable);
    }

    #[test]
    fn channel_spec_rejects_an_unknown_mode() {
        let err = Config::from_toml_and_env(FULL, &env(&[(ENV_CHANNELS, "222:ops:writable")]))
            .expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == ENV_CHANNELS),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn fetch_limits_must_be_ordered() {
        let text = format!("{FULL}\ndefault_fetch_limit_placeholder = 0\n")
            .replace(
                "[discord]",
                "[discord]\ndefault_fetch_limit = 90\nmax_fetch_limit = 20",
            )
            .replace("default_fetch_limit_placeholder = 0\n", "");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "discord.default_fetch_limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn provider_request_timeout_is_bounded_and_configurable() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(
            cfg.discord()
                .expect("discord provider")
                .request_timeout_seconds,
            DEFAULT_PROVIDER_REQUEST_TIMEOUT_SECONDS
        );

        let text = FULL.replace("[discord]", "[discord]\nrequest_timeout_seconds = 75");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(
            cfg.discord()
                .expect("discord provider")
                .request_timeout_seconds,
            75
        );

        for seconds in [0, 121] {
            let text = FULL.replace(
                "[discord]",
                &format!("[discord]\nrequest_timeout_seconds = {seconds}"),
            );
            let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
            assert!(
                matches!(&err, ConfigError::Invalid { field, .. }
                    if field == "discord.request_timeout_seconds"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn the_timezone_defaults_to_utc_and_can_be_set_from_file_or_environment() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(cfg.timezone.name(), DEFAULT_TIMEZONE);

        let text = FULL.replace("[server]", "[server]\ntimezone = \"America/New_York\"");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(cfg.timezone.name(), "America/New_York");

        let cfg = Config::from_toml_and_env(&text, &env(&[(ENV_TIMEZONE, "Europe/Berlin")]))
            .expect("valid config");
        assert_eq!(
            cfg.timezone.name(),
            "Europe/Berlin",
            "the environment must win, so a container can be told its operator's zone"
        );
    }

    #[test]
    fn an_unknown_timezone_stops_the_server_instead_of_becoming_utc() {
        // Silently defaulting is the failure this whole feature exists to prevent: the operator
        // would hear confidently-labelled times in the wrong zone and have no way to notice.
        let text = FULL.replace("[server]", "[server]\ntimezone = \"America/Nowhere\"");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, detail }
                if field == "server.timezone" && detail.contains("America/Nowhere")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_count_scan_ceiling_defaults_and_must_be_usable() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(cfg.chat.max_count_scan, DEFAULT_MAX_COUNT_SCAN);

        let text = FULL.replace("[discord]", "[discord]\nmax_count_scan = 1200");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(cfg.chat.max_count_scan, 1200);

        let text = FULL.replace("[discord]", "[discord]\nmax_count_scan = 0");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "discord.max_count_scan"),
            "a zero ceiling means every count answers \"at least zero\": {err}"
        );
    }

    #[test]
    fn live_ingestion_is_off_unless_asked_for_and_refuses_a_reckless_interval() {
        // OFF by default is the whole safety property here: upgrading this server must not start
        // a new, permanent stream of Discord requests on an operator's account without them
        // having typed anything.
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(cfg.providers[0].live_poll_seconds, 0);

        let text = FULL.replace("[discord]", "[discord]\nlive_poll_seconds = 30");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(cfg.providers[0].live_poll_seconds, 30);

        // The environment wins over the file, like every other setting here.
        let cfg = Config::from_toml_and_env(&text, &env(&[(ENV_LIVE_POLL_SECONDS, "60")]))
            .expect("valid config");
        assert_eq!(cfg.providers[0].live_poll_seconds, 60);

        // ...including turning it back OFF, which is the one an operator reaches for in a hurry.
        let cfg = Config::from_toml_and_env(&text, &env(&[(ENV_LIVE_POLL_SECONDS, "0")]))
            .expect("valid config");
        assert_eq!(cfg.providers[0].live_poll_seconds, 0);

        // Refused, not clamped. A silently-corrected 1 would look like it was honoured.
        let text = FULL.replace("[discord]", "[discord]\nlive_poll_seconds = 1");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, detail }
                if field == "discord.live_poll_seconds" && detail.contains("rate limit")),
            "unexpected error: {err}"
        );

        let err = Config::from_toml_and_env(FULL, &env(&[(ENV_LIVE_POLL_SECONDS, "soon")]))
            .expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. }
                if field == "discord.live_poll_seconds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn adapter_ingestion_is_opt_in_with_a_dedicated_strong_token() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert!(!cfg.ingest.enabled());

        let text = format!("{FULL}\n[ingest]\ntoken = \"adapter-token-that-is-long-enough\"\n");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert!(cfg.ingest.enabled());
        assert_eq!(
            cfg.ingest.token.as_ref().map(Secret::expose),
            Some("adapter-token-that-is-long-enough")
        );

        let cfg = Config::from_toml_and_env(
            FULL,
            &env(&[(ENV_INGEST_TOKEN, "environment-adapter-token-long-enough")]),
        )
        .expect("valid config");
        assert_eq!(
            cfg.ingest.token.as_ref().map(Secret::expose),
            Some("environment-adapter-token-long-enough")
        );
    }

    #[test]
    fn adapter_token_cannot_reuse_a_browser_token() {
        let weak = format!("{FULL}\n[ingest]\ntoken = \"too-short\"\n");
        let err = Config::from_toml_and_env(&weak, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "ingest.token"),
            "unexpected error: {err}"
        );

        let reused = format!("{FULL}\n[ingest]\ntoken = \"read-token-that-is-long-enough\"\n");
        let err = Config::from_toml_and_env(&reused, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "ingest.token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn preparing_a_body_for_speech_is_on_until_somebody_turns_it_off() {
        // The default is the load-bearing half. A deployment that never heard of this section is
        // the common case, and it is the one the complaint came from.
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert!(cfg.speakable.enabled, "a silent config must still prepare");

        let text = format!("{FULL}\n[speakable]\nenabled = false\n");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert!(!cfg.speakable.enabled);

        // Present but empty: the section exists and says nothing, which is still "on".
        let text = format!("{FULL}\n[speakable]\n");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert!(cfg.speakable.enabled);
    }

    #[test]
    fn the_environment_can_flip_the_speech_switch_both_ways() {
        for (raw, expected) in [
            ("0", false),
            ("false", false),
            ("off", false),
            ("No", false),
            ("1", true),
            ("TRUE", true),
            ("on", true),
        ] {
            // Set the FILE to the opposite of what the environment says, so a pass could not come
            // from the override being ignored.
            let text = format!("{FULL}\n[speakable]\nenabled = {}\n", !expected);
            let cfg = Config::from_toml_and_env(&text, &env(&[(ENV_SPEAKABLE, raw)]))
                .expect("valid config");
            assert_eq!(cfg.speakable.enabled, expected, "{raw:?} read wrong");
        }
    }

    #[test]
    fn a_speech_switch_that_is_neither_on_nor_off_stops_the_server() {
        // The reason this one refuses where `replay.enabled` shrugs: its default is ON, so the
        // shrugging form turns a typo into a silently disabled feature, and the operator setting
        // it is comparing two runs and would attribute the difference to their prompt.
        let error = Config::from_toml_and_env(FULL, &env(&[(ENV_SPEAKABLE, "enabled")]))
            .expect_err("a value that is neither must not be read as off");
        assert!(
            matches!(&error, ConfigError::Invalid { field, detail }
                if field == "speakable.enabled" && detail.contains("neither on nor off")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn channel_registration_is_an_explicit_bridge_opt_in() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert!(
            !cfg.discord()
                .expect("discord provider")
                .channel_registration
        );

        let text = FULL.replace(
            "[discord]",
            "[discord]\nchannel_registration = true\napi_base = \"https://bridge.example/v1\"",
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid bridge config");
        assert!(
            cfg.discord()
                .expect("discord provider")
                .channel_registration
        );
    }

    #[test]
    fn channel_registration_refuses_the_official_discord_api_base() {
        for api_base in [
            None,
            Some("https://discord.com/api/v10/"),
            Some("https://discord.com/api/v9"),
            Some("https://discordapp.com/api/v10"),
        ] {
            let replacement = match api_base {
                Some(api_base) => {
                    format!("[discord]\nchannel_registration = true\napi_base = \"{api_base}\"")
                }
                None => "[discord]\nchannel_registration = true".to_owned(),
            };
            let text = FULL.replace("[discord]", &replacement);
            let error = Config::from_toml_and_env(&text, &env(&[]))
                .expect_err("the official API must never receive managed lifecycle requests");
            assert!(
                matches!(&error, ConfigError::Invalid { field, detail }
                    if field == "discord.channel_registration"
                        && detail.contains("official API base")
                        && detail.contains("compatible bridge")),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn upstream_read_marks_are_an_explicit_bridge_opt_in() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert!(!cfg.discord().expect("discord provider").upstream_read_marks);

        let text = FULL.replace("[discord]", "[discord]\nupstream_read_marks = true");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert!(cfg.discord().expect("discord provider").upstream_read_marks);
    }

    #[test]
    fn resuming_is_off_by_default_and_its_budget_cannot_be_configured_into_a_lie() {
        // OFF by default is the safety property, and it is about privacy: every new call re-sends
        // prior conversation content to a vendor, so an upgrade must not start doing that.
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert!(!cfg.replay.enabled);
        assert_eq!(cfg.replay.policy.max_chars, DEFAULT_REPLAY_MAX_CHARS);
        assert_eq!(cfg.replay.policy.max_turns, DEFAULT_REPLAY_MAX_TURNS);
        assert_eq!(
            cfg.replay.transport,
            crate::replay::Transport::ContextualUpdate,
            "the transport that does not depend on a dashboard setting is the default"
        );

        let text = format!("{FULL}\n[replay]\nenabled = true\nmax_turns = 5\n");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert!(cfg.replay.enabled);
        assert_eq!(cfg.replay.policy.max_turns, 5);

        let cfg = Config::from_toml_and_env(&text, &env(&[(ENV_REPLAY_ENABLED, "0")]))
            .expect("valid config");
        assert!(
            !cfg.replay.enabled,
            "the environment has to be able to turn it back OFF, which is the direction an \
             operator reaches for in a hurry"
        );

        // Zero is not "no limit". It is a replay that is always empty while the interface says
        // resuming is on, which is the silent-nothing this feature must not be able to become.
        let text = format!("{FULL}\n[replay]\nenabled = true\nmax_chars = 0\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, detail }
                if field == "replay.max_chars" && detail.contains("not 'no limit'")),
            "unexpected error: {err}"
        );

        // An unknown transport is refused rather than silently defaulted: a typo'd
        // "prompt_override" that quietly became contextual_update would be a deployment behaving
        // differently from its own configuration file.
        let text = format!("{FULL}\n[replay]\ntransport = \"prompt_override\"\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "replay.transport"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_shipped_example_configuration_is_one_this_server_would_actually_accept() {
        // Both example files are the first thing an operator copies, and `deny_unknown_fields`
        // means a section documented but never wired up is a server that refuses to start. Nothing
        // checked either of them until `#46 conversation-replay` added a section to both.
        let example = include_str!("../vibe-talk.example.toml")
            .replace(
                "REPLACE-ME-at-least-24-characters",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .replace(
                "REPLACE-ME-different-and-at-least-24-characters",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            );
        Config::from_toml_and_env(&example, &env(&[])).expect("the shipped example must parse");
    }

    #[test]
    fn unknown_configuration_keys_are_refused() {
        let text = format!("{FULL}\n[nonsense]\nkey = 1\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(err, ConfigError::Parse(_)),
            "a typo'd section must fail loudly, not be ignored"
        );
    }

    #[test]
    fn the_elevenlabs_credentials_come_from_file_or_environment() {
        let text = format!(
            "{FULL}\n[elevenlabs]\nagent_id = \"agent_from_file\"\napi_key = \"key-from-file\"\n"
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(cfg.elevenlabs.agent_id.as_deref(), Some("agent_from_file"));
        assert_eq!(
            cfg.elevenlabs.api_key.as_ref().map(Secret::expose),
            Some("key-from-file")
        );
        assert_eq!(cfg.elevenlabs.api_base, DEFAULT_ELEVENLABS_API_BASE);

        let cfg = Config::from_toml_and_env(
            &text,
            &env(&[
                (ENV_ELEVENLABS_AGENT_ID, "agent_from_env"),
                (ENV_ELEVENLABS_API_KEY, "key-from-env"),
                (ENV_ELEVENLABS_API_BASE, "https://proxy.test/v1"),
            ]),
        )
        .expect("valid config");
        assert_eq!(cfg.elevenlabs.agent_id.as_deref(), Some("agent_from_env"));
        assert_eq!(
            cfg.elevenlabs.api_key.as_ref().map(Secret::expose),
            Some("key-from-env")
        );
        assert_eq!(cfg.elevenlabs.api_base, "https://proxy.test/v1");
    }

    #[test]
    fn a_conversation_provider_can_be_selected_without_naming_it_in_source() {
        let text = format!(
            "{FULL}\n[conversation]\nbackend = \"websocket\"\nwebsocket_url = \
             \"wss://voice-provider.example/ws\"\nlabel = \"Private preview\"\n"
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(cfg.conversation.backend, ConversationBackend::WebSocket);
        assert_eq!(
            cfg.conversation.websocket_url.as_deref(),
            Some("wss://voice-provider.example/ws")
        );
        assert_eq!(cfg.conversation.label, "Private preview");
        assert!(
            cfg.conversation.ask_before_send,
            "confirmation must be on unless the operator turns it off"
        );
    }

    #[test]
    fn asking_before_a_voice_send_is_on_by_default_and_can_be_turned_off() {
        // `#264 voice-send-approval`. The shipped default; the owner's saved choice is separate.
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert!(cfg.conversation.ask_before_send);
        let text = format!("{FULL}\n[conversation]\nask_before_send = false\n");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert!(!cfg.conversation.ask_before_send);
    }

    #[test]
    fn an_unconfigured_elevenlabs_section_still_loads() {
        // The Discord half of this server is useful on its own; a deployment with no voice agent
        // must still start, and fail only when something actually asks for a signed URL.
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert!(cfg.elevenlabs.agent_id.is_none());
        assert!(cfg.elevenlabs.api_key.is_none());
    }

    #[test]
    fn storage_is_off_unless_a_path_is_given() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert!(
            cfg.storage.path.is_none(),
            "a server with no storage configured must not invent a path for the owner's speech"
        );
        assert_eq!(
            cfg.storage.retention,
            crate::store::Retention::default(),
            "the bounds still have to be defined, so the absent case is one decision not two"
        );
    }

    #[test]
    fn the_storage_path_comes_from_file_or_environment() {
        let text = format!("{FULL}\n[storage]\npath = \"/var/lib/vibe-talk/from-file.sqlite3\"\n");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(
            cfg.storage.path.as_deref(),
            Some(Path::new("/var/lib/vibe-talk/from-file.sqlite3"))
        );

        let cfg =
            Config::from_toml_and_env(&text, &env(&[(ENV_STORAGE_PATH, "/data/from-env.sqlite3")]))
                .expect("valid config");
        assert_eq!(
            cfg.storage.path.as_deref(),
            Some(Path::new("/data/from-env.sqlite3")),
            "the environment must win, so a container can be told where its volume is"
        );
    }

    #[test]
    fn a_relative_storage_path_is_refused() {
        // A relative path inside a container points into the IMAGE, so the store works until the
        // next rebuild and then is gone. Refusing is the whole point of the setting.
        let text = format!("{FULL}\n[storage]\npath = \"state/vibe-talk.sqlite3\"\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "storage.path"),
            "unexpected error: {err}"
        );

        let err = Config::from_toml_and_env(FULL, &env(&[(ENV_STORAGE_PATH, "./state.sqlite3")]))
            .expect_err("must refuse the environment form too");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "storage.path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_summary_age_limit_is_its_own_setting_and_defaults_to_the_record_age_limit() {
        // `#208 persistent-cache`. Unset, a summary lives as long as it always did: `retain_days`,
        // so a host that lowered that for privacy keeps summaries no longer than before.
        let unset = format!("{FULL}\n[storage]\nretain_days = 9\n");
        let cfg = Config::from_toml_and_env(&unset, &env(&[])).expect("valid config");
        assert_eq!(cfg.storage.retention.summary_retain_days, 9);
        let set = format!("{FULL}\n[storage]\nretain_days = 9\nsummary_retain_days = 4\n");
        let cfg = Config::from_toml_and_env(&set, &env(&[])).expect("valid config");
        assert_eq!(cfg.storage.retention.summary_retain_days, 4);
        assert_eq!(cfg.storage.retention.retain_days, 9, "the two are separate");
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(
            cfg.storage.retention.summary_retain_days,
            crate::store::DEFAULT_RETAIN_DAYS
        );
    }

    #[test]
    fn a_message_cache_table_is_read_with_its_defaults_and_absent_is_not_off() {
        // `#208 persistent-cache`, milestone `cache-store`. Absent and `enabled = false` are two
        // different answers: the first says nothing about a cache, the second turns it off.
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(cfg.message_cache, None);
        let text = format!("{FULL}\n[message_cache]\n");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("an empty table is valid");
        assert_eq!(cfg.message_cache, Some(MessageCacheConfig::default()));
        let defaults = MessageCacheConfig::default();
        assert!(!defaults.enabled, "shipped off");
        assert_eq!(defaults.max_days, 90);
        assert_eq!(defaults.max_megabytes, 256);
        assert_eq!(defaults.backfill, CacheBackfill::Adaptive);
        assert_eq!(defaults.timeline_fallback_ms, 10_000);
        assert_eq!(defaults.path, None);

        let text = format!(
            "{FULL}\n[storage]\npath = \"/data/state.sqlite3\"\n\
             [message_cache]\nenabled = true\nmax_days = 14\nmax_megabytes = 64\n\
             backfill = \"off\"\ntimeline_fallback_ms = 2500\n\
             path = \"/data/cache/c.sqlite3\"\n\
             [[message_cache.channel]]\nid = \"111\"\nmax_days = 1\n"
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid");
        let cache = cfg.message_cache.expect("present");
        assert!(cache.enabled);
        assert_eq!(cache.max_days, 14);
        assert_eq!(cache.max_megabytes, 64);
        assert_eq!(cache.backfill, CacheBackfill::Off);
        assert_eq!(cache.timeline_fallback_ms, 2500);
        assert_eq!(
            cache.path.as_deref(),
            Some(Path::new("/data/cache/c.sqlite3"))
        );
        assert_eq!(cache.max_days_for("111"), 1);
        assert_eq!(cache.max_days_for("222"), 14);
        // ...and the rest of the configuration is read exactly as without it.
        assert_eq!(
            cfg.storage.retention,
            Config::from_toml_and_env(FULL, &env(&[]))
                .expect("valid config")
                .storage
                .retention
        );
    }

    #[test]
    fn a_message_cache_table_refuses_bad_bounds_unknown_keys_and_the_store_path() {
        // `#208 persistent-cache`, milestone `cache-store`.
        let refused = |table: &str| -> String {
            let text = format!("{FULL}\n[storage]\npath = \"/data/s.sqlite3\"\n{table}");
            match Config::from_toml_and_env(&text, &env(&[])) {
                Err(ConfigError::Invalid { field, .. }) => field,
                Err(ConfigError::Parse(_)) => "parse".to_owned(),
                other => panic!("{table:?} was not refused: {other:?}"),
            }
        };
        for (table, field) in [
            ("[message_cache]\nmax_days = 0\n", "message_cache.max_days"),
            (
                "[message_cache]\nmax_days = 366\n",
                "message_cache.max_days",
            ),
            ("[message_cache]\nmax_days = -1\n", "message_cache.max_days"),
            (
                "[message_cache]\nmax_megabytes = 15\n",
                "message_cache.max_megabytes",
            ),
            (
                "[message_cache]\nmax_megabytes = 4097\n",
                "message_cache.max_megabytes",
            ),
            (
                "[message_cache]\ntimeline_fallback_ms = 0\n",
                "message_cache.timeline_fallback_ms",
            ),
            (
                "[message_cache]\npath = \"relative.sqlite3\"\n",
                "message_cache.path",
            ),
            (
                "[message_cache]\npath = \"/data/s.sqlite3\"\n",
                "message_cache.path",
            ),
            (
                "[message_cache]\npath = \"/data/./x/../s.sqlite3\"\n",
                "message_cache.path",
            ),
            (
                "[[message_cache.channel]]\nid = \"111\"\nmax_days = 0\n",
                "message_cache.channel.max_days",
            ),
            (
                "[[message_cache.channel]]\nid = \" \"\nmax_days = 3\n",
                "message_cache.channel.id",
            ),
            (
                "[[message_cache.channel]]\nid = \"111\"\nmax_days = 3\n\
                 [[message_cache.channel]]\nid = \"111\"\nmax_days = 4\n",
                "message_cache.channel.id",
            ),
            // A typo must fail loudly rather than leave a bound at its default.
            ("[message_cache]\nmax_megabyte = 64\n", "parse"),
            ("[message_cache]\nbackfill = \"eager\"\n", "parse"),
        ] {
            assert_eq!(refused(table), field, "{table:?}");
        }
        // Turning it on needs somewhere to keep the off switch.
        let text = format!("{FULL}\n[message_cache]\nenabled = true\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("needs a store");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "message_cache.enabled"),
            "{err}"
        );
        // ...but a table that leaves it off is fine without one.
        let text = format!("{FULL}\n[message_cache]\nenabled = false\nmax_days = 7\n");
        Config::from_toml_and_env(&text, &env(&[])).expect("off needs no store");
    }

    #[test]
    fn retention_bounds_are_configurable_and_must_be_usable() {
        let text = format!(
            "{FULL}\n[storage]\npath = \"/data/s.sqlite3\"\nmax_conversations = 3\n\
             max_turns_per_conversation = 7\nmax_summaries = 11\nretain_days = 0\n"
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(cfg.storage.retention.max_conversations, 3);
        assert_eq!(cfg.storage.retention.max_turns_per_conversation, 7);
        assert_eq!(
            cfg.storage.retention.max_summaries, 11,
            "the summary cache is bounded too, and the bound has to be settable"
        );
        assert_eq!(
            cfg.storage.retention.retain_days, 0,
            "zero days must mean no age limit, not immediate deletion"
        );

        let text = format!("{FULL}\n[storage]\nmax_conversations = 0\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "storage.max_conversations"),
            "unexpected error: {err}"
        );

        let text = format!("{FULL}\n[storage]\nmax_turns_per_conversation = 0\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "storage.max_turns_per_conversation"),
            "unexpected error: {err}"
        );

        let text = format!("{FULL}\n[storage]\nmax_summaries = 0\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "storage.max_summaries"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_typo_in_the_storage_section_is_refused_rather_than_ignored() {
        let text = format!("{FULL}\n[storage]\npth = \"/data/s.sqlite3\"\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(err, ConfigError::Parse(_)),
            "a misspelled key must not silently leave storage off: {err}"
        );
    }

    #[test]
    fn the_summary_policy_defaults_and_is_configurable() {
        let cfg = Config::from_toml_and_env(FULL, &env(&[])).expect("valid config");
        assert_eq!(cfg.summaries, SummariesConfig::default());

        let text = format!(
            "{FULL}\n[summaries]\nthreshold_chars = 500\ntarget_chars = 80\n\
             context_messages = 1\nmodel = \"some-model\"\n"
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(cfg.summaries.threshold_chars, 500);
        assert_eq!(cfg.summaries.target_chars, 80);
        assert_eq!(cfg.summaries.context_messages, 1);
        assert_eq!(cfg.summaries.model.as_deref(), Some("some-model"));

        let cfg = Config::from_toml_and_env(&text, &env(&[(ENV_SUMMARY_MODEL, "from-env")]))
            .expect("valid config");
        assert_eq!(cfg.summaries.model.as_deref(), Some("from-env"));
    }

    #[test]
    fn a_configuration_that_still_names_the_removed_summariser_is_refused_by_name() {
        // `deny_unknown_fields` would answer "unknown field `backend`", which reads as "this
        // binary is too old" and sends the operator to a rollback. The field is kept declared
        // precisely so this sentence can be said instead — and from BOTH sources, or
        // VIBE_TALK_SUMMARY_BACKEND=extractive is a silent no-op.
        let named = |err: ConfigError| match err {
            ConfigError::Invalid { field, detail } => {
                assert_eq!(field, "summaries.backend");
                assert!(
                    detail.contains("REMOVED"),
                    "the refusal has to say the summariser is gone, not merely that the value is \
                     wrong: {detail}"
                );
                assert!(
                    detail.contains("ElevenLabs"),
                    "the refusal has to say what runs instead: {detail}"
                );
            }
            other => panic!("unexpected error: {other}"),
        };

        let text = format!("{FULL}\n[summaries]\nbackend = \"extractive\"\n");
        named(Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse the file form"));
        named(
            Config::from_toml_and_env(FULL, &env(&[(ENV_SUMMARY_BACKEND, "extractive")]))
                .expect_err("must refuse the environment form"),
        );

        // AND NEITHER SOURCE CAN COVER FOR THE OTHER. Written as `env.or(file)` — the shape that
        // is correct for a setting where one source overrides the other — the blessed environment
        // spelling consumed the slot and the removed value in the FILE was never examined at all.
        // That deployment booted with a generic deprecation warning and was never told the line it
        // had actually written was refused, which is the exact outcome the refusal exists to
        // prevent. Both orders, because either can be the one that short-circuits.
        named(
            Config::from_toml_and_env(&text, &env(&[(ENV_SUMMARY_BACKEND, "elevenlabs_agent")]))
                .expect_err("a blessed environment value must not hide a removed file value"),
        );
        let blessed_file = format!("{FULL}\n[summaries]\nbackend = \"elevenlabs_agent\"\n");
        named(
            Config::from_toml_and_env(&blessed_file, &env(&[(ENV_SUMMARY_BACKEND, "extractive")]))
                .expect_err("a blessed file value must not hide a removed environment value"),
        );
    }

    #[test]
    fn the_one_surviving_spelling_still_boots_and_is_told_it_selects_nothing() {
        // `backend = "elevenlabs_agent"` was, one release ago, the documented way to ask for real
        // summaries, so it is in live configuration files right now. Refusing it would stop those
        // servers booting for no behavioural gain — they already get exactly what it names. What
        // it must NOT do is be accepted in silence, which is how a dead setting survives for
        // years.
        let text = format!("{FULL}\n[summaries]\nbackend = \"elevenlabs_agent\"\n");
        for source in [None, Some((ENV_SUMMARY_BACKEND, "elevenlabs_agent"))] {
            let logs = crate::testing::LogCapture::info_only();
            let cfg = match source {
                None => Config::from_toml_and_env(&text, &env(&[])),
                Some(pair) => Config::from_toml_and_env(FULL, &env(&[pair])),
            }
            .expect("a deployment that spelled it right must still boot");
            assert_eq!(
                cfg.summaries,
                SummariesConfig::default(),
                "the dead key must not perturb anything else in [summaries]"
            );
            let said = logs.text();
            assert!(
                said.contains("summaries.backend") && said.contains("DEPRECATED"),
                "an ignored setting that says nothing is how a dead key survives for years: \
                 {said:?}"
            );
            assert!(
                said.contains("IGNORED"),
                "the warning has to say the key had no effect, not merely that it is old: {said:?}"
            );
        }
    }

    #[test]
    fn an_unknown_summariser_is_refused_and_told_the_key_is_deprecated() {
        // A typo is still refused rather than ignored — but the useful half of the answer is that
        // there is nothing to spell correctly any more.
        let text = format!("{FULL}\n[summaries]\nbackend = \"elevenlabs-agent\"\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        match &err {
            ConfigError::Invalid { field, detail } => {
                assert_eq!(field, "summaries.backend");
                assert!(detail.contains("DEPRECATED"), "{detail}");
                assert!(
                    detail.contains("elevenlabs-agent"),
                    "the refusal has to quote what was actually written: {detail}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_conversation_budget_of_zero_is_refused_rather_than_read_as_unlimited() {
        let text = format!("{FULL}\n[summaries]\nmax_per_socket = 0\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "summaries.max_per_socket"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("not 'no limit'"),
            "the refusal has to say WHY zero is not unlimited: {err}"
        );

        let text = format!("{FULL}\n[summaries]\nreply_timeout_seconds = 0\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "summaries.reply_timeout_seconds"),
            "unexpected error: {err}"
        );

        let text = format!(
            "{FULL}\n[summaries]\nmax_per_socket = 3\nsocket_idle_seconds = 5\n\
             reply_timeout_seconds = 45\n"
        );
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        assert_eq!(cfg.summaries.max_per_socket, 3);
        assert_eq!(cfg.summaries.socket_idle_seconds, 5);
        assert_eq!(cfg.summaries.reply_timeout_seconds, 45);
    }

    #[test]
    fn a_summary_longer_than_the_message_it_replaces_is_refused() {
        // Otherwise a message just over the threshold comes back LONGER than it started, which is
        // a cost with a negative benefit and reads on screen as the page padding things out.
        let text = format!("{FULL}\n[summaries]\nthreshold_chars = 50\ntarget_chars = 160\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "summaries.threshold_chars"),
            "unexpected error: {err}"
        );

        let text = format!("{FULL}\n[summaries]\ntarget_chars = 0\n");
        let err = Config::from_toml_and_env(&text, &env(&[])).expect_err("must refuse");
        assert!(
            matches!(&err, ConfigError::Invalid { field, .. } if field == "summaries.target_chars"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        let secret = Secret::new("hunter2-hunter2-hunter2");
        let rendered = format!("{secret:?}");
        assert!(
            !rendered.contains("hunter2"),
            "Secret leaked in Debug: {rendered}"
        );
        assert_eq!(rendered, "Secret(<redacted>)");
    }

    #[test]
    fn config_debug_output_does_not_leak_tokens() {
        let text = format!("{FULL}\n[elevenlabs]\napi_key = \"xi-key-in-the-file\"\n");
        let cfg = Config::from_toml_and_env(&text, &env(&[])).expect("valid config");
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("file-discord-token"), "leak: {rendered}");
        assert!(
            !rendered.contains("write-token-that-is-long-enough"),
            "leak: {rendered}"
        );
        assert!(
            !rendered.contains("xi-key-in-the-file"),
            "the ElevenLabs key leaked through Debug: {rendered}"
        );
    }

    #[test]
    fn secret_matches_only_the_exact_value() {
        let secret = Secret::new("abcdefgh");
        assert!(secret.matches("abcdefgh"));
        assert!(!secret.matches("abcdefg"));
        assert!(!secret.matches("abcdefghi"));
        assert!(!secret.matches("abcdefgH"));
        assert!(!secret.matches(""));
    }

    #[test]
    fn thread_protocol_is_explicit_and_bridge_routes_cannot_target_the_native_api() {
        use crate::threads::ThreadApi;
        assert_eq!(
            Config::from_toml_and_env(FULL, &env(&[]))
                .expect("default")
                .discord()
                .expect("discord provider")
                .thread_api,
            ThreadApi::Native
        );
        for (mode, expected) in [
            ("native", ThreadApi::Native),
            ("bridge", ThreadApi::Bridge),
            ("off", ThreadApi::Off),
        ] {
            let text = FULL.replace(
                "[discord]",
                &format!(
                    "[discord]\nthread_api = \"{mode}\"\napi_base = \"http://127.0.0.1:9876\""
                ),
            );
            assert_eq!(
                Config::from_toml_and_env(&text, &env(&[]))
                    .expect("explicit mode")
                    .discord()
                    .expect("discord provider")
                    .thread_api,
                expected
            );
        }
        let invalid = FULL.replace("[discord]", "[discord]\nthread_api = \"bridge\"");
        assert!(
            matches!(Config::from_toml_and_env(&invalid,&env(&[])),Err(ConfigError::Invalid{field,..}) if field=="discord.thread_api")
        );
        let invalid = FULL.replace("[discord]", "[discord]\nthread_api = \"guess\"");
        assert!(Config::from_toml_and_env(&invalid, &env(&[])).is_err());
    }

    /// `#208 persistent-cache`, milestone `bridge-recent-flat`: a bridge's recent read is
    /// `/messages`, unshadowed, unless the operator says otherwise, and only a bridge may say so.
    #[test]
    fn a_bridges_recent_read_is_messages_by_default_and_flat_only_on_request() {
        let bridge = |extra: &str| {
            FULL.replace(
                "[discord]",
                &format!(
                    "[discord]\nthread_api = \"bridge\"\napi_base = \"http://127.0.0.1:9876\"\n{extra}"
                ),
            )
        };
        let recent = |text: &str| {
            Config::from_toml_and_env(text, &env(&[]))
                .expect("valid")
                .discord()
                .expect("discord provider")
                .recent
        };
        assert_eq!(recent(FULL), RecentReads::default());
        assert_eq!(recent(&bridge("")), RecentReads::default());
        assert_eq!(
            RecentReads::default(),
            RecentReads {
                view: RecentView::Messages,
                shadow: false,
            }
        );
        assert_eq!(
            recent(&bridge("recent_view = \"flat\"")),
            RecentReads {
                view: RecentView::Flat,
                shadow: false,
            }
        );
        assert_eq!(
            recent(&bridge(
                "recent_view = \"messages\"\nrecent_view_shadow = true"
            )),
            RecentReads {
                view: RecentView::Messages,
                shadow: true,
            }
        );

        let refused = |text: &str, wanted: &str| match Config::from_toml_and_env(text, &env(&[])) {
            Err(ConfigError::Invalid { field, .. }) => assert_eq!(field, wanted, "{text}"),
            other => panic!("expected {wanted} to be refused, got {other:?}"),
        };
        refused(
            &FULL.replace("[discord]", "[discord]\nrecent_view = \"flat\""),
            "discord.recent_view",
        );
        refused(
            &FULL.replace(
                "[discord]",
                "[discord]\nthread_api = \"off\"\nrecent_view_shadow = false",
            ),
            "discord.recent_view_shadow",
        );
        refused(
            &bridge("recent_view = \"flat\"\nrecent_view_shadow = true"),
            "discord.recent_view_shadow",
        );
        refused(
            &MULTI.replace(
                "live_poll_seconds = 15",
                "live_poll_seconds = 15\nrecent_view = \"messages\"",
            ),
            "providers.slack.recent_view",
        );
        refused(
            &MULTI.replace(
                "live_poll_seconds = 15",
                "live_poll_seconds = 15\nrecent_view_shadow = true",
            ),
            "providers.slack.recent_view_shadow",
        );
        refused(
            &MULTI.replace(
                "thread_api = \"off\"",
                "thread_api = \"off\"\nrecent_view = \"flat\"",
            ),
            "providers.gchat.recent_view",
        );
        let gchat = MULTI.replace(
            "thread_api = \"off\"",
            "thread_api = \"bridge\"\nrecent_view = \"flat\"",
        );
        let cfg = Config::from_toml_and_env(&gchat, &env(&[])).expect("a bridge may read flat");
        assert!(
            Config::from_toml_and_env(&bridge("recent_view = \"sideways\""), &env(&[])).is_err()
        );
        let ProviderKind::Discord(gchat) = &cfg.provider("gchat").expect("gchat").kind else {
            panic!("gchat is a Discord-protocol bridge");
        };
        assert_eq!(gchat.recent.view, RecentView::Flat);
    }
}
