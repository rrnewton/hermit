//! The real ElevenLabs client.
//!
//! Same shape as [`crate::discord::http`]: the parts with judgment in them — which URL, which
//! header, and how the answer maps onto a [`SignedUrl`] — are pure functions, unit-tested here.
//! The async wrapper around them is deliberately thin, because it is the only part that cannot be
//! exercised without a live account key.

use async_trait::async_trait;

use super::{
    clamp_speed, configured_voice, credentials, speech_from_signed, speech_key, voice_source_agent,
    Account, AgentProfile, SignedUrl, SignedUrlError, SignedUrlProvider, Speech, SpeechError,
    SpeechProvider, VoiceStyle,
};
use crate::config::{ElevenLabsConfig, Secret};

/// A request, described independently of any HTTP client.
#[derive(Debug, PartialEq, Eq)]
pub struct PreparedRequest {
    /// HTTP method.
    pub method: &'static str,
    /// Fully qualified URL. Carries the agent id, which is public; never the key.
    pub url: String,
}

/// The header ElevenLabs authenticates with. Not `Authorization`, and not a bearer token.
pub const API_KEY_HEADER: &str = "xi-api-key";

/// Build the request that mints a signed conversation URL.
///
/// Documented as `GET /v1/convai/conversation/get-signed-url?agent_id=<id>` with the account key
/// in the `xi-api-key` header. The key deliberately does NOT go in the query string: a URL is the
/// single most-logged, most-forwarded, most-referrer-leaked part of an HTTP request.
#[must_use]
pub fn signed_url_request(api_base: &str, agent_id: &str) -> PreparedRequest {
    PreparedRequest {
        method: "GET",
        url: format!(
            "{}/convai/conversation/get-signed-url?agent_id={}",
            api_base.trim_end_matches('/'),
            urlencode(agent_id)
        ),
    }
}

/// The default ElevenLabs model for reading a message aloud.
///
/// Named here rather than left to the vendor's account default, so two deployments of this server
/// read the same text the same way and a change of model is a change to this line.
pub const TTS_MODEL: &str = "eleven_turbo_v2_5";

/// The audio this server asks for, and therefore what it tells the browser it is sending.
pub const TTS_CONTENT_TYPE: &str = "audio/mpeg";

/// What [`SPEECH_FORMAT`] actually is, told to the browser rather than guessed by it.
///
/// The vendor labels the streaming response `audio/mpeg` regardless of the format requested, and a
/// browser handed Opus-in-Ogg under an mp3 label refuses to play it. This server asked for the
/// format, so this server is the thing that knows what it is.
pub const SPEECH_CONTENT_TYPE: &str = "audio/ogg";

/// Build the request that reads text aloud.
///
/// Documented as `POST /v1/text-to-speech/{voice_id}` with the account key in the `xi-api-key`
/// header. A POST rather than a GET even though it only reads: it SPENDS MONEY at a vendor, and a
/// GET is fair game for a browser, a proxy or a crawler to fetch speculatively.
#[must_use]
pub fn speech_request(api_base: &str, voice_id: &str) -> PreparedRequest {
    PreparedRequest {
        method: "POST",
        url: format!(
            "{}/text-to-speech/{}",
            api_base.trim_end_matches('/'),
            urlencode(voice_id)
        ),
    }
}

/// The audio format asked of the vendor, and the reason it is not the default.
///
/// Sending nothing takes the vendor's default, which is mp3 at 44.1kHz and 128kbit/s — around
/// sixteen kilobytes for every second of speech. A long message is then more than a megabyte
/// crossing a phone connection before the reader hears anything. This is one voice reading prose,
/// not music: 32kbit/s Opus is a quarter of the size and, at that bitrate, audibly BETTER than the
/// mp3 it replaces, because mp3 is poor down there and Opus was designed for it.
///
/// `opus_48000_32` is in the vendor's published format list, and Safari has decoded Opus for years.
pub const SPEECH_FORMAT: &str = "opus_48000_32";

/// How hard the vendor is asked to trade quality for a faster first byte.
///
/// Their scale is 0 (none) to 4. 4 also turns their text normaliser off, which this server
/// explicitly must not use: `speakable::for_speech` has already rewritten the numbers, dates and
/// identifiers that a normaliser exists to catch, and 4 would mispronounce whatever it left. 3 is
/// the most that keeps the normaliser, and the quality cost on one voice reading prose aloud is not
/// something this use can hear.
pub const SPEECH_LATENCY_MODE: u8 = 3;

/// `POST /v1/text-to-speech/{voice_id}/stream` — the same generation, delivered as it happens.
///
/// A separate function from [`speech_request`] rather than a flag on it, because the two differ in
/// what the caller must then DO with the response: this one must not be buffered, and a shared
/// constructor would make forgetting that a silent performance regression rather than a type error.
#[must_use]
pub fn speech_stream_request(api_base: &str, voice_id: &str) -> PreparedRequest {
    PreparedRequest {
        method: "POST",
        url: format!(
            "{}/text-to-speech/{}/stream?output_format={SPEECH_FORMAT}\
             &optimize_streaming_latency={SPEECH_LATENCY_MODE}",
            api_base.trim_end_matches('/'),
            urlencode(voice_id)
        ),
    }
}

/// Build the request that reads an agent's own configuration back.
///
/// Documented as `GET /v1/convai/agents/{agent_id}`. Read-aloud uses it for ONE field:
/// `conversation_config.tts.voice_id`, the voice that agent already speaks in.
#[must_use]
pub fn agent_request(api_base: &str, agent_id: &str) -> PreparedRequest {
    PreparedRequest {
        method: "GET",
        url: format!(
            "{}/convai/agents/{}",
            api_base.trim_end_matches('/'),
            urlencode(agent_id)
        ),
    }
}

/// Build the request that asks which account an API key belongs to.
///
/// Documented as `GET /v1/user`. The smallest authenticated call on this API: it names no agent
/// and no voice, so it answers "is this key any good" on its own.
#[must_use]
pub fn account_request(api_base: &str) -> PreparedRequest {
    PreparedRequest {
        method: "GET",
        url: format!("{}/user", api_base.trim_end_matches('/')),
    }
}

/// Read an [`Account`] out of the answer to [`account_request`].
///
/// Every field is looked for in more than one place and none of them is required. That is not
/// laziness: what this function is FOR is telling an operator which account their key reached,
/// and a parser that refused the whole answer because the vendor renamed `subscription.tier`
/// would convert a working key into a failed check.
#[must_use]
pub fn parse_account(value: &serde_json::Value) -> Account {
    let text = |path: &[&str]| -> Option<String> {
        let mut cursor = value;
        for key in path {
            cursor = cursor.get(key)?;
        }
        cursor
            .as_str()
            .map(str::trim)
            .filter(|found| !found.is_empty())
            .map(str::to_owned)
    };
    Account {
        user_id: text(&["user_id"]).or_else(|| text(&["subscription", "user_id"])),
        workspace: text(&["workspace_id"])
            .or_else(|| text(&["workspace", "id"]))
            .or_else(|| text(&["workspace", "name"])),
        tier: text(&["subscription", "tier"]).or_else(|| text(&["subscription_tier"])),
    }
}

/// Read an [`AgentProfile`] out of the answer to [`agent_request`].
///
/// A missing voice is [`None`] rather than an error, because "this agent exists and has no voice"
/// is a real, actionable state with its own remedy, and collapsing it into "the request failed"
/// would send the operator to look at their API key.
#[must_use]
pub fn parse_agent_profile(value: &serde_json::Value, agent_id: &str) -> AgentProfile {
    AgentProfile {
        agent_id: agent_id.to_owned(),
        name: value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        voice_id: parse_agent_voice(value).ok(),
        prompt_override: parse_prompt_override(value),
    }
}

/// Whether the agent lets a client override its system prompt, read from
/// `platform_settings.overrides.conversation_config_override.agent.prompt.prompt` on
/// `GET /v1/convai/agents/{agent_id}`. `None` when the answer does not carry a boolean there.
/// `#272 voice-agent-instructions`.
#[must_use]
pub fn parse_prompt_override(value: &serde_json::Value) -> Option<bool> {
    value
        .pointer("/platform_settings/overrides/conversation_config_override/agent/prompt/prompt")
        .and_then(serde_json::Value::as_bool)
}

/// Pull the agent's configured voice out of its configuration.
///
/// # Errors
///
/// [`SpeechError::Status`] shape is not used here; an answer without the field is reported as a
/// [`SpeechError::NotConfigured`] naming `voice_id`, because that is the operator's fix: the agent
/// really does not have a voice to borrow, and one has to be named.
pub fn parse_agent_voice(value: &serde_json::Value) -> Result<String, SpeechError> {
    agent_tts(value)
        .and_then(|tts| tts.get("voice_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or(SpeechError::NotConfigured(
            "elevenlabs.voice_id (the configured agent reports no voice of its own)",
        ))
}

fn agent_tts(value: &serde_json::Value) -> Option<&serde_json::Value> {
    value.get("conversation_config").and_then(|c| c.get("tts"))
}

/// Pull the agent's DELIVERY out of its configuration: how fast, how stable, which model.
///
/// Separate from [`parse_agent_voice`] because they fail differently. A missing voice id is a
/// refusal -- there is nothing to speak with. A missing speed is not: the vendor's default applies
/// and the reader hears something. So this returns a style with holes in it rather than an error,
/// and every hole means "do not send that field at all".
#[must_use]
pub fn parse_agent_style(value: &serde_json::Value) -> VoiceStyle {
    let Some(tts) = agent_tts(value) else {
        return VoiceStyle::default();
    };
    let number = |key: &str| tts.get(key).and_then(serde_json::Value::as_f64);
    VoiceStyle {
        model_id: tts
            .get("model_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        speed: number("speed"),
        stability: number("stability"),
        similarity_boost: number("similarity_boost"),
    }
}

/// The body that goes with [`speech_request`].
///
/// `voice_settings` is only present when there is something to say about the voice, and only the
/// fields actually known are inside it. Sending a default this server made up would OVERRIDE the
/// account's own setting with a guess.
#[must_use]
pub fn speech_body(text: &str, style: &VoiceStyle, speed: Option<f64>) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert(
        "text".to_owned(),
        serde_json::Value::String(text.to_owned()),
    );
    body.insert(
        "model_id".to_owned(),
        serde_json::Value::String(
            style
                .model_id
                .clone()
                .unwrap_or_else(|| TTS_MODEL.to_owned()),
        ),
    );
    let mut settings = serde_json::Map::new();
    let mut put = |key: &str, value: Option<f64>| {
        if let Some(number) = value.and_then(serde_json::Number::from_f64) {
            settings.insert(key.to_owned(), serde_json::Value::Number(number));
        }
    };
    // The READER's speed wins over the agent's, because they asked for it on this pass.
    put("speed", clamp_speed(speed).or(style.speed));
    put("stability", style.stability);
    put("similarity_boost", style.similarity_boost);
    if !settings.is_empty() {
        body.insert(
            "voice_settings".to_owned(),
            serde_json::Value::Object(settings),
        );
    }
    serde_json::Value::Object(body)
}

/// Percent-encode the few characters that could change the meaning of a query string.
///
/// An agent id is `agent_` plus hex in practice, so this is a guard rather than a general encoder;
/// it exists so a pasted-with-junk id fails ElevenLabs' validation rather than smuggling a second
/// query parameter into the URL.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Convert ElevenLabs' answer into a [`SignedUrl`].
///
/// # Errors
///
/// Returns [`SignedUrlError::Shape`] when `signed_url` is absent, not a string, or blank. A blank
/// one is rejected rather than passed through, because it would surface as an inscrutable browser
/// failure several steps later.
pub fn parse_signed_url(
    value: &serde_json::Value,
    agent_id: &str,
) -> Result<SignedUrl, SignedUrlError> {
    let signed = value
        .get("signed_url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            SignedUrlError::Shape("answer has no string \"signed_url\" field".to_owned())
        })?;
    if signed.trim().is_empty() {
        return Err(SignedUrlError::Shape(
            "\"signed_url\" is blank; there is nothing to connect to".to_owned(),
        ));
    }
    Ok(SignedUrl {
        signed_url: signed.to_owned(),
        agent_id: agent_id.to_owned(),
        valid_for_seconds: super::DOCUMENTED_VALIDITY_SECONDS,
    })
}

/// A live ElevenLabs client.
#[derive(Debug)]
pub struct HttpElevenLabsClient {
    client: reqwest::Client,
    /// The agent's own voice AND delivery, once looked up.
    ///
    /// Cached because it is a property of the deployment's configuration, not of the message being
    /// read: without this, every tap on a message would spend TWO vendor calls instead of one, and
    /// the extra one would return the same answer every time.
    borrowed_voice: std::sync::Mutex<Option<(String, VoiceStyle)>>,
}

impl HttpElevenLabsClient {
    /// Build a client.
    ///
    /// # Errors
    ///
    /// Returns [`SignedUrlError::Transport`] when the underlying HTTP client cannot be built.
    pub fn new() -> Result<Self, SignedUrlError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "vibe-talk (https://github.com/rrnewton/agent-utils, ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| SignedUrlError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            borrowed_voice: std::sync::Mutex::new(None),
        })
    }

    /// Which voice reads a message: the one the operator named, or the AGENT'S OWN.
    ///
    /// `agent_id` and `voice_id` are different things in ElevenLabs' model -- an agent is a prompt,
    /// an LLM, tools AND a voice; the text-to-speech API takes a voice and knows nothing about
    /// agents. So they cannot be one setting. But the agent HAS a voice, and borrowing it is what
    /// the operator means by "read it in the voice I already configured": one setting, and the
    /// message is read by the same voice that talks to them.
    ///
    /// `voice_id` remains available for the case those should differ.
    async fn voice_for(
        &self,
        config: &ElevenLabsConfig,
        api_key: &Secret,
    ) -> Result<(String, VoiceStyle), SpeechError> {
        if let Some(held) = self.borrowed_voice.lock().ok().and_then(|v| v.clone()) {
            return Ok(held);
        }
        // Even with a voice named explicitly, the AGENT is still asked -- for its delivery. An
        // operator who set their agent to speak quickly and then named a voice separately still
        // means "at the pace I chose"; hearing that voice at the vendor's default pace is the bug
        // this exists to avoid. With no agent to ask, an explicit voice speaks with the vendor's
        // defaults, which is all there is to go on.
        let named = configured_voice(config).map(str::to_owned);
        let agent_id = match voice_source_agent(config) {
            Ok(id) => id,
            Err(absent) => return named.map(|v| (v, VoiceStyle::default())).ok_or(absent),
        };
        let request = agent_request(&config.api_base, agent_id);
        let value = self
            .get_json(&request, api_key)
            .await
            .map_err(speech_from_signed)?;
        let style = parse_agent_style(&value);
        let voice = match named {
            Some(explicit) => explicit,
            None => parse_agent_voice(&value)?,
        };
        let resolved = (voice, style);
        if let Ok(mut held) = self.borrowed_voice.lock() {
            *held = Some(resolved.clone());
        }
        Ok(resolved)
    }

    /// The same GET the signed-URL path uses, named so read-aloud can share it rather than grow a
    /// second HTTP shape against the same vendor.
    async fn get_json(
        &self,
        request: &PreparedRequest,
        api_key: &Secret,
    ) -> Result<serde_json::Value, SignedUrlError> {
        let response = self
            .client
            .get(&request.url)
            .header(API_KEY_HEADER, api_key.expose())
            .send()
            .await
            // A transport error's text can contain the URL, which holds the agent id but never the
            // key. Redact anyway: this is the cheap end of "never leak", and reqwest is free to
            // include whatever it likes here.
            .map_err(|e| SignedUrlError::Transport(super::redact(&e.to_string(), api_key)))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| SignedUrlError::Transport(super::redact(&e.to_string(), api_key)))?;
        if !status.is_success() {
            return Err(SignedUrlError::from_response(
                status.as_u16(),
                &text,
                api_key,
            ));
        }
        serde_json::from_str(&text)
            .map_err(|e| SignedUrlError::Shape(super::redact(&e.to_string(), api_key)))
    }
}

#[async_trait]
impl SignedUrlProvider for HttpElevenLabsClient {
    async fn signed_url(&self, config: &ElevenLabsConfig) -> Result<SignedUrl, SignedUrlError> {
        let (agent_id, api_key) = credentials(config)?;
        let request = signed_url_request(&config.api_base, agent_id);
        let value = self.get_json(&request, api_key).await?;
        parse_signed_url(&value, agent_id)
    }

    async fn account(&self, config: &ElevenLabsConfig) -> Result<Account, SignedUrlError> {
        // Only the KEY is required here, deliberately: an operator with no agent configured still
        // needs to be able to find out whether their key works.
        let api_key = config
            .api_key
            .as_ref()
            .filter(|key| !key.expose().trim().is_empty())
            .ok_or(SignedUrlError::NotConfigured("elevenlabs.api_key"))?;
        let value = self
            .get_json(&account_request(&config.api_base), api_key)
            .await?;
        Ok(parse_account(&value))
    }

    async fn agent_profile(
        &self,
        config: &ElevenLabsConfig,
    ) -> Result<AgentProfile, SignedUrlError> {
        let (agent_id, api_key) = credentials(config)?;
        let value = self
            .get_json(&agent_request(&config.api_base, agent_id), api_key)
            .await?;
        Ok(parse_agent_profile(&value, agent_id))
    }
}

#[async_trait]
impl SpeechProvider for HttpElevenLabsClient {
    async fn speak(
        &self,
        config: &ElevenLabsConfig,
        text: &str,
        speed: Option<f64>,
    ) -> Result<Speech, SpeechError> {
        let api_key = speech_key(config)?;
        let (voice, style) = self.voice_for(config, api_key).await?;
        let voice_id = voice.as_str();
        // Checked BEFORE the call, not after: a blank message would otherwise be billed as a
        // request that returns silence.
        if text.trim().is_empty() {
            return Err(SpeechError::Empty);
        }
        let request = speech_request(&config.api_base, voice_id);
        let response = self
            .client
            .post(&request.url)
            .header(API_KEY_HEADER, api_key.expose())
            .json(&speech_body(text, &style, speed))
            .send()
            .await
            .map_err(|e| SpeechError::Transport(super::redact(&e.to_string(), api_key)))?;
        let status = response.status();
        if !status.is_success() {
            // The FAILURE body is text; the success body is audio. Only this branch decodes as a
            // string, because calling `.text()` on megabytes of mp3 would be nonsense.
            let body = response.text().await.unwrap_or_default();
            return Err(SpeechError::from_response(status.as_u16(), &body, api_key));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or(TTS_CONTENT_TYPE)
            .to_owned();
        let audio = response
            .bytes()
            .await
            .map_err(|e| SpeechError::Transport(super::redact(&e.to_string(), api_key)))?;
        Ok(Speech {
            audio: audio.to_vec(),
            content_type,
        })
    }

    async fn speak_stream(
        &self,
        config: &ElevenLabsConfig,
        text: &str,
        speed: Option<f64>,
    ) -> Result<super::SpeechStream, SpeechError> {
        use futures_util::StreamExt as _;

        let api_key = speech_key(config)?;
        // Cached after the first call, and read-aloud mode warms it before the reader taps
        // anything, so this is a map lookup on the path that matters.
        let (voice, style) = self.voice_for(config, api_key).await?;
        if text.trim().is_empty() {
            return Err(SpeechError::Empty);
        }
        let request = speech_stream_request(&config.api_base, voice.as_str());
        let response = self
            .client
            .post(&request.url)
            .header(API_KEY_HEADER, api_key.expose())
            .json(&speech_body(text, &style, speed))
            .send()
            .await
            .map_err(|e| SpeechError::Transport(super::redact(&e.to_string(), api_key)))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(SpeechError::from_response(status.as_u16(), &body, api_key));
        }
        // NOT read from the response. The vendor labels this `audio/mpeg` whatever was actually
        // asked for, and a browser handed Opus-in-Ogg under an mp3 label will refuse to play it.
        // What went out in `output_format` is what is coming back, so that is what is declared.
        let content_type = SPEECH_CONTENT_TYPE.to_owned();
        let redaction = api_key.clone();
        let chunks = response
            .bytes_stream()
            .map(move |chunk| {
                chunk.map_err(|e| SpeechError::Transport(super::redact(&e.to_string(), &redaction)))
            })
            .boxed();
        Ok(super::SpeechStream {
            content_type,
            preamble: None,
            session: None,
            chunks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_ELEVENLABS_API_BASE;

    #[test]
    fn the_url_matches_the_documented_endpoint() {
        let request = signed_url_request(DEFAULT_ELEVENLABS_API_BASE, "agent_0123");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.url,
            "https://api.elevenlabs.io/v1/convai/conversation/get-signed-url?agent_id=agent_0123"
        );
    }

    #[test]
    fn the_url_tolerates_a_trailing_slash_on_the_base() {
        let request = signed_url_request("https://example.test/v1/", "agent_1");
        assert_eq!(
            request.url,
            "https://example.test/v1/convai/conversation/get-signed-url?agent_id=agent_1"
        );
    }

    #[test]
    fn the_url_never_carries_the_key() {
        // The whole reason the key goes in a header. If this ever regresses, the credential is in
        // every proxy log between here and ElevenLabs.
        let request = signed_url_request(DEFAULT_ELEVENLABS_API_BASE, "agent_1");
        assert!(!request.url.contains("api_key"), "{}", request.url);
        assert!(!request.url.contains("xi-"), "{}", request.url);
    }

    #[test]
    fn a_junk_agent_id_cannot_smuggle_a_second_parameter() {
        let request = signed_url_request(DEFAULT_ELEVENLABS_API_BASE, "agent_1&admin=true");
        assert!(
            request.url.ends_with("agent_id=agent_1%26admin%3Dtrue"),
            "{}",
            request.url
        );
    }

    /// A one-shot HTTP server on loopback that captures the request head and answers with
    /// `status` and `body`. It exists so the live client's *transport* choices — which header
    /// carries the key, and what happens to a rejection body — are checked by execution rather
    /// than by reading the code. Without it, moving the key into the query string is a change no
    /// test notices.
    async fn one_shot_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            let mut buffer = vec![0_u8; 8192];
            let read = stream.read(&mut buffer).await.expect("read request");
            let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            stream.flush().await.expect("flush");
            head
        });
        (format!("http://{addr}/v1"), handle)
    }

    #[test]
    fn an_agents_own_voice_is_read_out_of_its_configuration() {
        // The one field read-aloud borrows, at the path ElevenLabs actually puts it:
        // `conversation_config.tts.voice_id` on `GET /v1/convai/agents/{id}`.
        let value = serde_json::json!({
            "agent_id": "agent_1",
            "name": "lead",
            "conversation_config": { "tts": { "voice_id": "voice_abc", "model_id": "x" } }
        });
        assert_eq!(parse_agent_voice(&value).expect("a voice"), "voice_abc");
    }

    #[test]
    fn whether_the_agent_lets_a_client_replace_its_prompt_is_read_and_never_guessed() {
        // `#272 voice-agent-instructions`.
        let allowing = |prompt: serde_json::Value| {
            serde_json::json!({
                "agent_id": "agent_1",
                "platform_settings": { "overrides": { "conversation_config_override": {
                    "agent": { "prompt": { "prompt": prompt }, "first_message": true },
                    "conversation": { "text_only": true }
                } } }
            })
        };
        assert_eq!(
            parse_prompt_override(&allowing(serde_json::json!(true))),
            Some(true)
        );
        assert_eq!(
            parse_prompt_override(&allowing(serde_json::json!(false))),
            Some(false)
        );
        let profile = parse_agent_profile(&allowing(serde_json::json!(true)), "agent_1");
        assert_eq!(profile.prompt_override, Some(true));
        // Absent, or not a boolean: unknown, which the caller treats as not allowed.
        for unknown in [
            serde_json::json!({ "agent_id": "agent_1" }),
            serde_json::json!({ "platform_settings": { "overrides": {} } }),
            allowing(serde_json::json!("yes")),
        ] {
            assert_eq!(parse_prompt_override(&unknown), None, "{unknown}");
        }
    }

    #[test]
    fn an_agent_with_no_voice_of_its_own_says_so_rather_than_reading_in_silence() {
        for value in [
            serde_json::json!({ "conversation_config": { "tts": {} } }),
            serde_json::json!({ "conversation_config": {} }),
            serde_json::json!({}),
            // Present but blank is the same as absent: it would produce a request to an empty
            // voice id, which fails later and further away.
            serde_json::json!({ "conversation_config": { "tts": { "voice_id": "  " } } }),
        ] {
            let got = parse_agent_voice(&value);
            assert!(got.is_err(), "{value} produced {got:?}");
        }
    }

    fn live_config(api_base: String) -> ElevenLabsConfig {
        ElevenLabsConfig {
            agent_id: Some("agent_live".to_owned()),
            api_key: Some(Secret::new("xi-live-key-value")),
            api_base,
            voice_id: Some("voice_live".to_owned()),
        }
    }

    #[tokio::test]
    async fn the_live_client_sends_the_key_as_a_header_and_never_in_the_url() {
        let (api_base, server) =
            one_shot_server("200 OK", r#"{"signed_url":"wss://example.test/live"}"#).await;
        let client = HttpElevenLabsClient::new().expect("client");
        let minted = client
            .signed_url(&live_config(api_base))
            .await
            .expect("mints");
        assert_eq!(minted.signed_url, "wss://example.test/live");
        assert_eq!(minted.agent_id, "agent_live");

        let head = server.await.expect("server");
        let request_line = head.lines().next().expect("request line");
        assert!(
            request_line.contains("/v1/convai/conversation/get-signed-url?agent_id=agent_live"),
            "unexpected request line: {request_line}"
        );
        assert!(
            !request_line.contains("xi-live-key-value"),
            "the account key reached the URL, where every proxy logs it: {request_line}"
        );
        assert!(
            head.to_lowercase()
                .contains("xi-api-key: xi-live-key-value"),
            "the key must travel in the xi-api-key header: {head}"
        );
    }

    #[tokio::test]
    async fn the_live_client_redacts_a_rejection_that_quotes_the_key_back() {
        let (api_base, server) = one_shot_server(
            "401 Unauthorized",
            r#"{"detail":{"message":"key xi-live-key-value is invalid"}}"#,
        )
        .await;
        let client = HttpElevenLabsClient::new().expect("client");
        let error = client
            .signed_url(&live_config(api_base))
            .await
            .expect_err("a 401 is not a success");
        let _ = server.await;
        let rendered = error.to_string();
        assert!(rendered.contains("401"), "{rendered}");
        assert!(
            !rendered.contains("xi-live-key-value"),
            "the key leaked out of a vendor error body: {rendered}"
        );
        assert!(rendered.contains(super::super::REDACTED), "{rendered}");
    }

    #[test]
    fn a_signed_url_is_passed_through_unchanged() {
        let value = serde_json::json!({
            "signed_url": "wss://api.elevenlabs.io/v1/convai/conversation?agent_id=agent_1&token=abc"
        });
        let minted = parse_signed_url(&value, "agent_1").expect("parses");
        assert_eq!(
            minted.signed_url,
            "wss://api.elevenlabs.io/v1/convai/conversation?agent_id=agent_1&token=abc"
        );
        assert_eq!(minted.agent_id, "agent_1");
        assert_eq!(
            minted.valid_for_seconds,
            super::super::DOCUMENTED_VALIDITY_SECONDS
        );
    }

    #[test]
    fn an_answer_without_a_signed_url_fails_loudly() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({ "signed_url": null }),
            serde_json::json!({ "signed_url": 7 }),
            serde_json::json!({ "signed_url": "   " }),
        ] {
            assert!(
                matches!(
                    parse_signed_url(&value, "agent_1"),
                    Err(SignedUrlError::Shape(_))
                ),
                "an unusable answer must not be handed to a browser: {value}"
            );
        }
    }
}
