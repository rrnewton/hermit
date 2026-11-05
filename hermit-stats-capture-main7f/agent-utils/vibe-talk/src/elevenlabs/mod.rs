//! Minting a short-lived signed conversation URL from ElevenLabs.
//!
//! Turning on "Enable Authentication" on an ElevenLabs agent closes the public `talk-to` link: a
//! conversation can only be started from a **signed URL**, which is minted from ElevenLabs' API
//! with an account API key and is good for a documented fifteen minutes. That key is an account
//! credential — it can spend money and read the account's agents — so it lives here, on the
//! server, and the only thing that ever crosses the wire to a caller is the minted URL.
//!
//! Three rules hold in this module, and the tests exist to hold them:
//!
//! * **The key never appears in an output.** It travels in a request *header*, never in a URL or
//!   a body, and every error built from a vendor response goes through [`redact`] first — because
//!   an upstream is free to echo a credential back in its own error text, and that text ends up
//!   in a log and in an API response.
//! * **A missing key is a loud, specific failure.** There is no unsigned fallback: an agent with
//!   authentication enabled would simply refuse the connection later, in the browser, where the
//!   operator cannot see why.
//! * **The failure modes are distinguishable.** "You never configured a key", "ElevenLabs
//!   rejected the key", and "ElevenLabs is unreachable" are three different errors with three
//!   different HTTP statuses, because they have three different fixes.

pub mod fake;
pub mod http;
pub mod mock;
pub mod socket;
pub mod speech;

// Keep the audio client API usable while shared handlers use `crate::speech::SpeechProvider`.
pub use crate::speech::{clamp_speed, Speech, MAX_SPEECH_SPEED, MIN_SPEECH_SPEED};

/// Audio stream carrying an ElevenLabs client error until the provider adapter handles it.
pub type SpeechStream = crate::speech::SpeechStream<SpeechError>;

use std::time::Duration;

use async_trait::async_trait;

use crate::config::{ElevenLabsConfig, Secret};

/// Why a signed URL could not be minted.
#[derive(Debug, thiserror::Error)]
pub enum SignedUrlError {
    /// A required setting is absent, so no call was attempted.
    #[error("{0} is not configured; this server cannot mint a signed conversation URL")]
    NotConfigured(&'static str),
    /// The request never completed.
    #[error("elevenlabs request failed: {0}")]
    Transport(String),
    /// ElevenLabs answered with a non-success status.
    #[error("elevenlabs returned HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body, truncated and redacted.
        body: String,
    },
    /// The response did not have the shape this server expects.
    #[error("elevenlabs response could not be understood: {0}")]
    Shape(String),
}

impl SignedUrlError {
    /// Build a [`SignedUrlError::Status`] from a vendor response body.
    ///
    /// Truncates — an intermediary's error page is not short — and redacts, because the body is
    /// third-party text that may quote the credential we just sent it. Every construction of the
    /// `Status` variant goes through here so there is one place to check.
    #[must_use]
    pub fn from_response(status: u16, body: &str, api_key: &Secret) -> Self {
        let mut body = redact(body, api_key);
        body.truncate(500);
        Self::Status { status, body }
    }

    /// Stable machine-readable code for the API layer.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured(_) => "elevenlabs_not_configured",
            Self::Transport(_) | Self::Status { .. } | Self::Shape(_) => "elevenlabs_error",
        }
    }
}

/// Replace every occurrence of a secret with a marker.
///
/// A blank secret is not searched for: `str::replace` with an empty needle inserts the marker
/// between every character, which would be a spectacular way to mangle an error message.
#[must_use]
pub fn redact(text: &str, secret: &Secret) -> String {
    let needle = secret.expose();
    if needle.is_empty() {
        return text.to_owned();
    }
    text.replace(needle, REDACTED)
}

/// [`redact`] against every secret in turn.
///
/// A conversation holds more than one: the account key, and the minted URL's single-use token,
/// which is a credential in its own right and is quoted verbatim by any connect failure. One call
/// per secret is easy to forget at one of several call sites, so there is a function for it.
#[must_use]
pub fn redact_all(text: &str, secrets: &[Secret]) -> String {
    secrets
        .iter()
        .fold(text.to_owned(), |acc, secret| redact(&acc, secret))
}

/// What [`redact`] leaves behind.
pub const REDACTED: &str = "<redacted>";

/// Fifteen minutes, per ElevenLabs' own documentation of the signed URL's lifetime. Recorded so
/// the web page can say something true about how long the operator has to press "start".
pub const DOCUMENTED_VALIDITY_SECONDS: u32 = 15 * 60;

/// The credentials a mint needs, checked together so the caller learns which one is absent.
///
/// # Errors
///
/// Returns [`SignedUrlError::NotConfigured`] naming the field the operator has to set. The key is
/// checked first because it is the one that is easy to forget: an agent id is visible in the
/// dashboard URL, whereas the key is shown once at creation.
pub fn credentials(config: &ElevenLabsConfig) -> Result<(&str, &Secret), SignedUrlError> {
    let api_key = config
        .api_key
        .as_ref()
        .filter(|key| !key.expose().trim().is_empty())
        .ok_or(SignedUrlError::NotConfigured("elevenlabs.api_key"))?;
    let agent_id = config
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or(SignedUrlError::NotConfigured("elevenlabs.agent_id"))?;
    Ok((agent_id, api_key))
}

/// The account key, which read-aloud needs whichever voice it ends up using.
///
/// # Errors
///
/// [`SpeechError::NotConfigured`] naming the absent setting, before any call is attempted.
pub fn speech_key(config: &ElevenLabsConfig) -> Result<&Secret, SpeechError> {
    config
        .api_key
        .as_ref()
        .filter(|key| !key.expose().trim().is_empty())
        .ok_or(SpeechError::NotConfigured("elevenlabs.api_key"))
}

/// The voice the operator named explicitly, if they named one.
///
/// `None` is not an error here: an unset `voice_id` means "use the voice the AGENT already
/// speaks in", which is resolved against ElevenLabs. See [`SpeechProvider::speak`].
#[must_use]
pub fn configured_voice(config: &ElevenLabsConfig) -> Option<&str> {
    config
        .voice_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// The agent whose own voice read-aloud borrows when no `voice_id` is configured.
///
/// # Errors
///
/// [`SpeechError::NotConfigured`] when neither setting is present, naming BOTH -- either one fixes
/// it, and an error naming only one would send the operator to configure the wrong thing.
pub fn voice_source_agent(config: &ElevenLabsConfig) -> Result<&str, SpeechError> {
    config
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or(SpeechError::NotConfigured(
            "elevenlabs.voice_id (or elevenlabs.agent_id, whose own voice would be used)",
        ))
}

/// Why a message could not be read aloud.
///
/// The same four distinguishable failures as [`SignedUrlError`], for the same reason: "you never
/// configured a voice", "ElevenLabs rejected the key", "ElevenLabs is unreachable" and "the answer
/// was not audio" have four different fixes and must not arrive as one generic error.
#[derive(Debug, thiserror::Error)]
pub enum SpeechError {
    /// A required setting is absent, so no call was attempted.
    #[error("{0} is not configured; this server cannot read a message aloud")]
    NotConfigured(&'static str),
    /// The request never completed.
    #[error("elevenlabs request failed: {0}")]
    Transport(String),
    /// ElevenLabs answered with a non-success status.
    #[error("elevenlabs returned HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body, truncated and redacted.
        body: String,
    },
    /// There was nothing worth sending, so nothing was sent.
    ///
    /// A separate variant rather than an empty success: spending a vendor call on a message with
    /// no speakable text bills the account for silence, and answering with zero bytes of audio
    /// would reach the reader as a player that does nothing and says nothing about why.
    #[error("this message has no text to read aloud")]
    Empty,
}

impl SpeechError {
    /// Build a [`SpeechError::Status`] from a vendor response body, truncated and redacted.
    ///
    /// Goes through the same [`redact`] the signed-URL path uses: an upstream is free to echo the
    /// credential back in its own error text, and that text ends up in a log and in an API answer.
    #[must_use]
    pub fn from_response(status: u16, body: &str, api_key: &Secret) -> Self {
        let mut body = redact(body, api_key);
        body.truncate(500);
        Self::Status { status, body }
    }
}

impl SpeechError {
    /// Stable machine-readable code for the API layer.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured(_) => "elevenlabs_not_configured",
            Self::Empty => "nothing_to_read",
            Self::Transport(_) | Self::Status { .. } => "elevenlabs_error",
        }
    }
}

/// Carry a signed-URL-shaped failure across to the speech side.
///
/// Read-aloud reuses the agent-configuration GET, so its failures arrive as [`SignedUrlError`].
/// They are the same three failures with the same three fixes; only the sentence differs.
#[must_use]
pub fn speech_from_signed(error: SignedUrlError) -> SpeechError {
    match error {
        SignedUrlError::NotConfigured(what) => SpeechError::NotConfigured(what),
        SignedUrlError::Transport(detail) => SpeechError::Transport(detail),
        SignedUrlError::Status { status, body } => SpeechError::Status { status, body },
        SignedUrlError::Shape(detail) => SpeechError::Transport(detail),
    }
}

/// How a voice should sound, as the configured agent already sounds.
///
/// Read-aloud borrows the agent's whole delivery, not merely its voice id. The owner set his agent
/// to speak faster than default and then heard read-aloud speak at default -- correctly, because
/// nothing was carrying the speed across. A voice at the wrong pace is a different voice.
///
/// Every field is optional and an absent one is simply not sent, so the vendor's own default
/// applies. This never invents a value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoiceStyle {
    /// The synthesis model the agent uses.
    pub model_id: Option<String>,
    /// 1.0 is the vendor's default; below is slower, above is faster.
    pub speed: Option<f64>,
    /// How consistent the delivery is between generations.
    pub stability: Option<f64>,
    /// How closely the output tracks the original voice.
    pub similarity_boost: Option<f64>,
}

/// ElevenLabs audio transport. Application code uses [`crate::speech::SpeechProvider`].
#[async_trait]
pub trait SpeechProvider: Send + Sync {
    /// Read `text` aloud in the configured voice.
    ///
    /// # Errors
    ///
    /// Returns [`SpeechError`] when a setting is absent, the text is empty, the request fails, or
    /// ElevenLabs refuses.
    ///
    /// `speed` overrides whatever the agent is configured with, for a reader who wants this
    /// particular pass faster or slower. `None` means "however the agent speaks".
    async fn speak(
        &self,
        config: &ElevenLabsConfig,
        text: &str,
        speed: Option<f64>,
    ) -> Result<Speech, SpeechError>;

    /// The same thing, but handed back as it arrives.
    ///
    /// # Errors
    ///
    /// As [`SpeechProvider::speak`]. A failure that happens PART WAY THROUGH arrives as an error
    /// item in the stream rather than here, because by then a status and some audio have already
    /// been sent and there is nothing left to turn into an HTTP error.
    ///
    /// The default implementation generates the whole thing and hands it over in one chunk. That is
    /// the honest behaviour for a provider that cannot stream — a fake, or a future backend without
    /// the endpoint — and it keeps every such provider correct rather than making them all
    /// implement a second method to say "no".
    async fn speak_stream(
        &self,
        config: &ElevenLabsConfig,
        text: &str,
        speed: Option<f64>,
    ) -> Result<SpeechStream, SpeechError> {
        let spoken = self.speak(config, text, speed).await?;
        Ok(SpeechStream {
            content_type: spoken.content_type,
            preamble: None,
            session: None,
            chunks: Box::pin(futures_util::stream::once(async move {
                Ok(bytes::Bytes::from(spoken.audio))
            })),
        })
    }
}

/// A minted, short-lived conversation URL.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SignedUrl {
    /// The `wss://` URL the browser connects to. Short-lived, and itself a credential.
    pub signed_url: String,
    /// The agent it was minted for. Public: it identifies a widget, not an account.
    pub agent_id: String,
    /// How long ElevenLabs documents the URL as being valid for.
    pub valid_for_seconds: u32,
}

/// Who the account API key belongs to, as ElevenLabs reports it.
///
/// Every field is optional and none of them is a secret: this is the answer to "which account did
/// my key actually reach", which is the question behind the most confusing ElevenLabs failure
/// there is — a key that works perfectly and an agent id that is real, in two different
/// workspaces, producing a 404 that reads like a typo.
///
/// Optional rather than required because the vendor is free to change what it volunteers, and a
/// diagnostic that refuses to report a *reachable* account over a renamed field would be worse
/// than one that reports less than it hoped for. See [`Account::describe`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Account {
    /// The account's own id.
    pub user_id: Option<String>,
    /// The workspace the key resolves into, when the vendor names one.
    pub workspace: Option<String>,
    /// The subscription tier, which is what decides several of the quotas.
    pub tier: Option<String>,
}

impl Account {
    /// One line naming whatever the vendor was willing to say.
    ///
    /// Never empty: "the key was accepted, and ElevenLabs named neither an account nor a
    /// workspace" is itself the useful answer when that is what happened, and an empty string
    /// would read as a check that did not run.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(workspace) = &self.workspace {
            parts.push(format!("workspace {workspace}"));
        }
        if let Some(user) = &self.user_id {
            parts.push(format!("account {user}"));
        }
        if let Some(tier) = &self.tier {
            parts.push(format!("subscription {tier}"));
        }
        if parts.is_empty() {
            return "the key was accepted; ElevenLabs named no account or workspace in its answer"
                .to_owned();
        }
        format!("the key was accepted by {}", parts.join(", "))
    }
}

/// What the configured agent is, as ElevenLabs reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentProfile {
    /// The agent id that was asked about.
    pub agent_id: String,
    /// The agent's name in the dashboard, when it has one.
    pub name: Option<String>,
    /// The voice this agent speaks in, which read-aloud borrows when `elevenlabs.voice_id` is
    /// unset. `None` when the agent genuinely has none — see [`crate::probe::Diagnosis::NoVoice`].
    pub voice_id: Option<String>,
    /// Whether the agent's security settings let a client replace its system prompt when a
    /// conversation starts. `None` when its configuration does not say. `#272
    /// voice-agent-instructions`: the owner's edited prompt is sent as such an override only when
    /// this is `Some(true)`, because an override the agent does not allow is believed to end the
    /// conversation rather than be ignored.
    pub prompt_override: Option<bool>,
}

/// Something that can mint a signed conversation URL.
///
/// The configuration is passed in rather than captured at construction so that an implementation
/// cannot quietly answer from a credential the running server is not actually configured with.
#[async_trait]
pub trait SignedUrlProvider: Send + Sync {
    /// Mint a signed conversation URL for the configured agent.
    ///
    /// # Errors
    ///
    /// Returns [`SignedUrlError`] when a setting is absent, the request fails, ElevenLabs refuses,
    /// or the answer cannot be understood.
    async fn signed_url(&self, config: &ElevenLabsConfig) -> Result<SignedUrl, SignedUrlError>;

    /// Ask ElevenLabs which account the configured API key belongs to.
    ///
    /// Needs the key and NOTHING ELSE — no agent, no voice — which is precisely what makes it able
    /// to say "your key is fine, it is the agent id that is wrong". Minting a signed URL cannot:
    /// it fails identically for a bad key and for an agent on another account until you read the
    /// status code, and by then the operator has already changed the wrong setting.
    ///
    /// # Errors
    ///
    /// [`SignedUrlError::NotConfigured`] when there is no key, and the usual transport, status and
    /// shape failures otherwise.
    async fn account(&self, config: &ElevenLabsConfig) -> Result<Account, SignedUrlError>;

    /// Read the configured agent's own configuration back.
    ///
    /// # Errors
    ///
    /// [`SignedUrlError::NotConfigured`] when the key or the agent id is absent, a 404 status when
    /// the account has no such agent, and the usual transport and shape failures.
    async fn agent_profile(
        &self,
        config: &ElevenLabsConfig,
    ) -> Result<AgentProfile, SignedUrlError>;
}

/// Why a text turn with the conversational agent could not be completed.
///
/// The same four distinguishable failures as [`SignedUrlError`], and for the same reason: "you
/// never configured an agent", "ElevenLabs refused", "ElevenLabs is unreachable" and "the answer
/// was not in a shape this server understands" have four different fixes. They must not arrive as
/// one generic error, because the operator's next action differs in every case.
///
/// [`ChatError::Refused`] carries a sentence rather than a status code because a ConvAI refusal
/// arrives by two different routes: the mint is HTTP and refuses with a status, while the socket
/// refuses by *closing* with an RFC6455 code. Inventing an HTTP status for a close frame would
/// make a diagnostic lie about which half said no.
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    /// A required setting is absent, so no call was attempted.
    #[error("{0} is not configured; this server cannot talk to a conversational agent")]
    NotConfigured(&'static str),
    /// ElevenLabs was reached and said no — a rejected key, an agent that is not on the account,
    /// or a conversation the vendor closed on us.
    #[error("elevenlabs refused the conversation: {0}")]
    Refused(String),
    /// The conversation never completed: DNS, TCP, TLS, a dropped socket, or a deadline.
    #[error("the conversational agent could not be reached: {0}")]
    Transport(String),
    /// The vendor answered, but not with anything this server can read as a reply.
    #[error("the conversational agent answered with something this server cannot use: {0}")]
    Shape(String),
}

impl ChatError {
    /// Build a [`ChatError::Refused`] from a vendor response body.
    ///
    /// Truncates and redacts for the same reason [`SignedUrlError::from_response`] does: the body
    /// is third-party text that may quote the credential we just sent, and it ends up in a log.
    /// Takes every secret the conversation holds, not only the key — see [`redact_all`].
    #[must_use]
    pub fn from_response(status: u16, body: &str, secrets: &[Secret]) -> Self {
        let mut body = redact_all(body, secrets);
        body.truncate(500);
        Self::Refused(format!("HTTP {status}: {body}"))
    }

    /// Build a [`ChatError::Refused`] from a WebSocket close frame.
    ///
    /// The reason is vendor text and is redacted on the same terms. So is the CODE's meaning: a
    /// close with no reason at all is still a refusal, and saying so beats an empty message.
    #[must_use]
    pub fn from_close(code: u16, reason: &str, secrets: &[Secret]) -> Self {
        let mut reason = redact_all(reason, secrets);
        reason.truncate(500);
        if reason.trim().is_empty() {
            reason = "(the vendor gave no reason)".to_owned();
        }
        Self::Refused(format!(
            "the agent closed the conversation with code {code}: {reason}"
        ))
    }

    /// Stable machine-readable code for the API layer.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured(_) => "elevenlabs_not_configured",
            Self::Refused(_) | Self::Transport(_) | Self::Shape(_) => "elevenlabs_error",
        }
    }
}

/// Carry a signed-URL-shaped failure across to the chat side.
///
/// A text conversation begins with the same mint the browser uses, so its failures arrive as
/// [`SignedUrlError`]. They are the same failures with the same fixes; only the sentence differs.
#[must_use]
pub fn chat_from_signed(error: SignedUrlError) -> ChatError {
    match error {
        SignedUrlError::NotConfigured(what) => ChatError::NotConfigured(what),
        SignedUrlError::Transport(detail) => ChatError::Transport(detail),
        // Already redacted and truncated at construction; re-wrapping the sentence is all that is
        // left to do.
        SignedUrlError::Status { status, body } => {
            ChatError::Refused(format!("HTTP {status}: {body}"))
        }
        SignedUrlError::Shape(detail) => ChatError::Shape(detail),
    }
}

/// One open, text-only conversation with the configured agent.
///
/// **A ConvAI socket is a conversation, not a request channel.** Everything asked down one of
/// these accumulates in the agent's context, including its own previous answers, so a caller that
/// wants N independent answers cannot simply call [`TextChat::ask`] N times and expect N
/// independent answers. Deciding how many turns one of these is allowed to serve is the caller's
/// job; see [`crate::summarize::agent`], which is the only caller in this tree and says why it
/// picked the number it did.
#[async_trait]
pub trait TextChat: Send + std::fmt::Debug {
    /// Say one thing and read until the agent has said its piece.
    ///
    /// Implementations must answer the vendor's application-level `ping` events while waiting,
    /// because a conversation that stops answering them is closed by the vendor.
    ///
    /// # Errors
    ///
    /// [`ChatError::Transport`] when the socket fails or `deadline` passes with no reply,
    /// [`ChatError::Refused`] when the vendor closes the conversation, and [`ChatError::Shape`]
    /// when what came back is not a reply this server can read.
    async fn ask(&mut self, text: &str, deadline: Duration) -> Result<String, ChatError>;

    /// Close the conversation politely.
    ///
    /// Takes `self` by box because a closed conversation must not be reachable afterwards, and
    /// returns nothing because there is no useful recovery from a failed goodbye — the socket is
    /// going away either way. ElevenLabs bills connection time, so this is not optional politeness.
    async fn close(self: Box<Self>);
}

/// Opening a text-only conversation with the configured agent.
///
/// Separate from [`TextChat`] so that "the vendor would not let us start" and "the conversation
/// failed once it was running" are different call sites with different errors — and so that a
/// pool can hold the opener for the whole life of the process while opening and discarding the
/// conversations themselves.
#[async_trait]
pub trait TextChatProvider: Send + Sync {
    /// Open one text-only conversation.
    ///
    /// `deadline` bounds the whole opening handshake — mint, socket, and the vendor's initiation
    /// metadata — because every one of those steps can hang and none of them should hang forever.
    ///
    /// # Errors
    ///
    /// [`ChatError`], distinguishing an absent setting from a refusal from unreachability.
    async fn open_text_chat(
        &self,
        config: &ElevenLabsConfig,
        deadline: Duration,
    ) -> Result<Box<dyn TextChat>, ChatError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(agent: Option<&str>, key: Option<&str>) -> ElevenLabsConfig {
        ElevenLabsConfig {
            agent_id: agent.map(str::to_owned),
            api_key: key.map(Secret::new),
            api_base: crate::config::DEFAULT_ELEVENLABS_API_BASE.to_owned(),
            voice_id: None,
        }
    }

    #[test]
    fn a_missing_api_key_names_the_api_key() {
        let error = credentials(&config(Some("agent_1"), None)).expect_err("must refuse");
        assert!(
            matches!(&error, SignedUrlError::NotConfigured(field) if *field == "elevenlabs.api_key"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("elevenlabs.api_key"),
            "the operator must be told WHICH setting is missing: {error}"
        );
    }

    #[test]
    fn a_missing_agent_id_names_the_agent_id() {
        let error = credentials(&config(None, Some("xi-key"))).expect_err("must refuse");
        assert!(
            matches!(&error, SignedUrlError::NotConfigured(field) if *field == "elevenlabs.agent_id"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_blank_setting_counts_as_absent() {
        // A container that renders an unset variable as "" must not look configured.
        assert!(credentials(&config(Some("agent_1"), Some("   "))).is_err());
        assert!(credentials(&config(Some(""), Some("xi-key"))).is_err());
        let trimmable = config(Some(" agent_1 "), Some("xi-key"));
        let (agent, key) = credentials(&trimmable).expect("valid");
        assert_eq!(
            agent, "agent_1",
            "the agent id is trimmed before it is used"
        );
        assert_eq!(key.expose(), "xi-key");
    }

    #[test]
    fn a_vendor_body_that_echoes_the_key_is_redacted() {
        // Not hypothetical enough to skip: an upstream error message quoting the credential it
        // was given is a normal thing for an API to do, and this body reaches a log and a caller.
        let key = Secret::new("xi-secret-key-value");
        let error = SignedUrlError::from_response(
            401,
            r#"{"detail":{"status":"invalid_api_key","message":"xi-secret-key-value is invalid"}}"#,
            &key,
        );
        let rendered = error.to_string();
        assert!(
            !rendered.contains("xi-secret-key-value"),
            "leak: {rendered}"
        );
        assert!(rendered.contains(REDACTED), "{rendered}");
        assert!(
            rendered.contains("401"),
            "the status must survive: {rendered}"
        );
    }

    #[test]
    fn an_oversized_vendor_body_is_truncated() {
        let key = Secret::new("xi-secret-key-value");
        let error = SignedUrlError::from_response(502, &"x".repeat(10_000), &key);
        match error {
            SignedUrlError::Status { body, .. } => assert_eq!(body.len(), 500),
            other => panic!("expected a status error, got {other:?}"),
        }
    }

    #[test]
    fn redacting_against_a_blank_secret_does_not_shred_the_text() {
        assert_eq!(redact("hello", &Secret::new("")), "hello");
    }

    #[test]
    fn a_chat_refusal_that_echoes_the_key_is_redacted_whichever_half_refused() {
        // Both halves of a ConvAI conversation can quote the credential back: the mint is an HTTP
        // API, and a close frame's reason is free text the vendor writes. Neither may reach a log.
        let key = [Secret::new("xi-secret-key-value")];
        let from_http = ChatError::from_response(
            401,
            r#"{"detail":{"message":"xi-secret-key-value is invalid"}}"#,
            &key,
        )
        .to_string();
        assert!(!from_http.contains("xi-secret-key-value"), "{from_http}");
        assert!(from_http.contains(REDACTED), "{from_http}");
        assert!(from_http.contains("401"), "the status must survive");

        let from_close =
            ChatError::from_close(1008, "key xi-secret-key-value is not allowed here", &key)
                .to_string();
        assert!(!from_close.contains("xi-secret-key-value"), "{from_close}");
        assert!(
            from_close.contains("1008"),
            "the close code must survive: {from_close}"
        );
    }

    #[test]
    fn a_close_with_no_reason_still_says_that_the_vendor_refused() {
        // A close frame with an empty reason is normal, and "elevenlabs refused the conversation:"
        // followed by nothing is the least useful line a log can hold.
        let rendered = ChatError::from_close(1011, "   ", &[Secret::new("k")]).to_string();
        assert!(rendered.contains("1011"), "{rendered}");
        assert!(rendered.contains("no reason"), "{rendered}");
    }

    #[test]
    fn the_four_chat_failures_stay_apart_when_a_signed_url_failure_is_carried_across() {
        // The mint's taxonomy has to survive the trip, or "you never configured a key" arrives at
        // the caller looking like "the vendor is down" and sends the operator to the wrong place.
        assert!(matches!(
            chat_from_signed(SignedUrlError::NotConfigured("elevenlabs.agent_id")),
            ChatError::NotConfigured("elevenlabs.agent_id")
        ));
        assert!(matches!(
            chat_from_signed(SignedUrlError::Transport("dns".to_owned())),
            ChatError::Transport(_)
        ));
        assert!(matches!(
            chat_from_signed(SignedUrlError::from_response(
                401,
                "no",
                &Secret::new("xi-k")
            )),
            ChatError::Refused(_)
        ));
        assert!(matches!(
            chat_from_signed(SignedUrlError::Shape("no signed_url".to_owned())),
            ChatError::Shape(_)
        ));
    }

    #[test]
    fn error_codes_separate_misconfiguration_from_vendor_failure() {
        assert_eq!(
            ChatError::NotConfigured("elevenlabs.agent_id").code(),
            "elevenlabs_not_configured"
        );
        assert_eq!(
            ChatError::Refused("no".to_owned()).code(),
            "elevenlabs_error"
        );
        assert_eq!(
            SignedUrlError::NotConfigured("elevenlabs.api_key").code(),
            "elevenlabs_not_configured"
        );
        assert_eq!(
            SignedUrlError::from_response(401, "no", &Secret::new("k")).code(),
            "elevenlabs_error"
        );
        assert_eq!(
            SignedUrlError::Transport("dns".to_owned()).code(),
            "elevenlabs_error"
        );
    }
}
