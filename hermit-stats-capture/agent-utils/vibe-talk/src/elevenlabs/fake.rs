//! An in-memory ElevenLabs stand-in for tests and for local development.
//!
//! # It must be able to say no
//!
//! The same design note as [`crate::discord::fake`], for the same reason. A fake that answers
//! every mint with a plausible `wss://` URL cannot fail, so a test written against it would be
//! self-certifying: green here and worth nothing about the real client. This one therefore holds
//! the two things a real ElevenLabs account holds — **the API key it will accept**, and **the
//! agent ids it actually has** — and refuses anything else the way ElevenLabs refuses it: HTTP
//! 401 for a bad key, HTTP 404 for an agent that is not on the account.
//!
//! It also shares the real client's code for the two things that could otherwise drift: it builds
//! its request through [`super::http::signed_url_request`], and it goes through
//! [`super::credentials`] first, so a server with no key configured fails here exactly as it fails
//! in production — before any call is attempted, naming the setting.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::http::{
    account_request, agent_request, signed_url_request, speech_request, TTS_CONTENT_TYPE,
};
use super::{
    chat_from_signed, clamp_speed, configured_voice, credentials, speech_key, voice_source_agent,
    Account, AgentProfile, ChatError, SignedUrl, SignedUrlError, SignedUrlProvider, Speech,
    SpeechError, SpeechProvider, TextChat, TextChatProvider,
};
use crate::config::ElevenLabsConfig;

/// The API key [`FakeElevenLabs::new`] accepts. Anything else is a 401.
pub const VALID_API_KEY: &str = "xi-fake-api-key-not-a-real-one";
/// The agent id [`FakeElevenLabs::new`] knows about. Anything else is a 404.
pub const KNOWN_AGENT_ID: &str = "agent_fake0000000000000000000000";
/// The voice id [`FakeElevenLabs::new`] knows about. Anything else is a 404.
pub const KNOWN_VOICE_ID: &str = "voice_fake0000000000000000000000";
/// The workspace [`FakeElevenLabs::new`] says [`VALID_API_KEY`] belongs to.
pub const FAKE_WORKSPACE: &str = "workspace_fake000000000000000000";
/// The account id [`FakeElevenLabs::new`] says [`VALID_API_KEY`] belongs to.
pub const FAKE_USER_ID: &str = "user_fake00000000000000000000000";
/// The subscription tier [`FakeElevenLabs::new`] reports.
pub const FAKE_TIER: &str = "creator";
/// The audio [`FakeElevenLabs::new`] returns. Not real mp3 -- nothing here decodes it -- but it is
/// BYTES, and distinct per text, so a test can prove which message was actually read.
#[must_use]
pub fn fake_audio(text: &str) -> Vec<u8> {
    let mut audio = b"FAKE-MP3:".to_vec();
    audio.extend_from_slice(text.as_bytes());
    audio
}

/// The URL [`FakeElevenLabs::new`] mints.
pub const MINTED_URL: &str =
    "wss://api.elevenlabs.io/v1/convai/conversation?agent_id=agent_fake0000000000000000000000\
     &token=fake-conversation-token";

/// Everything one text conversation with the fake agent saw.
///
/// The record, not a count, and for the same reason [`FakeElevenLabs::spoken`] is a list: the
/// claim worth making about a pooled socket is "these eight summaries went down ONE conversation
/// and the ninth opened another", and a count of conversations cannot say which question landed
/// on which.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FakeChat {
    /// Every prompt sent down this conversation, in order.
    pub asked: Vec<String>,
    /// Whether it was closed politely. ElevenLabs bills connection time, so "forgotten" and
    /// "closed" are different outcomes and a test has to be able to tell them apart.
    pub closed: bool,
}

/// In-memory ElevenLabs.
#[derive(Debug)]
pub struct FakeElevenLabs {
    valid_api_key: String,
    /// Shared rather than owned, because an open conversation records into it after the call that
    /// created it has returned.
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    known_agents: BTreeSet<String>,
    known_voices: BTreeSet<String>,
    /// Every text conversation opened, in order.
    chats: Vec<FakeChat>,
    /// What the agent answers, when a test has chosen. `None` is an answer derived from which
    /// conversation and which turn it is, so a test can prove WHICH socket served a summary.
    answer: Option<String>,
    /// Make the next turn fail. One-shot, so recovery is testable too.
    fail_turn: Option<String>,
    /// Every text this fake was asked to read, in order, so a test can prove WHAT was spoken --
    /// and that a refusal spoke nothing.
    spoken: Vec<String>,
    /// The speed asked for on each of those, after clamping.
    speeds: Vec<Option<f64>>,
    /// Every URL this fake was asked to fetch, in order. Lets a test prove a mint did NOT happen.
    requested: Vec<String>,
    minted_url: String,
    fail_with: Option<String>,
    /// Agents that exist on this account but have no voice of their own.
    ///
    /// Its own set rather than a flag, because "the agent is not on this account" and "the agent
    /// is here and has no voice" are two DIFFERENT failures with two different remedies, and a
    /// fake that could only produce one of them would leave the other's remedy untested.
    voiceless_agents: BTreeSet<String>,
    /// What every agent's configuration says about a client replacing its prompt: `None` says
    /// nothing, as an agent configured before the setting existed would. `#272
    /// voice-agent-instructions`.
    prompt_override: Option<bool>,
    /// Make the next read of an agent's configuration fail, and nothing else: a mint beside it
    /// still succeeds. One-shot.
    fail_agent_profile: Option<String>,
    /// Make the next read of an agent's configuration wait this long before it answers: a vendor
    /// that does not answer in time. One-shot.
    stall_agent_profile: Option<Duration>,
}

impl FakeElevenLabs {
    /// Every text this fake was asked to read, in order.
    ///
    /// The point of the list rather than a count: "the reader tapped the second message and the
    /// SECOND message is what was spoken" is the claim worth making, and a count cannot make it.
    #[must_use]
    pub fn spoken(&self) -> Vec<String> {
        self.lock().spoken.clone()
    }

    /// The speed each of those was asked for, after clamping. `None` means the agent's own.
    #[must_use]
    pub fn speeds(&self) -> Vec<Option<f64>> {
        self.lock().speeds.clone()
    }
}

impl Default for FakeElevenLabs {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeElevenLabs {
    /// A fake account holding [`VALID_API_KEY`] and one agent, [`KNOWN_AGENT_ID`].
    #[must_use]
    pub fn new() -> Self {
        let mut known_agents = BTreeSet::new();
        known_agents.insert(KNOWN_AGENT_ID.to_owned());
        let mut known_voices = BTreeSet::new();
        known_voices.insert(KNOWN_VOICE_ID.to_owned());
        Self {
            valid_api_key: VALID_API_KEY.to_owned(),
            state: Arc::new(Mutex::new(State {
                known_agents,
                known_voices,
                chats: Vec::new(),
                answer: None,
                fail_turn: None,
                spoken: Vec::new(),
                speeds: Vec::new(),
                requested: Vec::new(),
                minted_url: MINTED_URL.to_owned(),
                fail_with: None,
                voiceless_agents: BTreeSet::new(),
                prompt_override: None,
                fail_agent_profile: None,
                stall_agent_profile: None,
            })),
        }
    }

    /// Say what the agents' configuration reports about replacing their prompt. `#272
    /// voice-agent-instructions`.
    pub fn allow_prompt_override(&self, allowed: Option<bool>) {
        self.lock().prompt_override = allowed;
    }

    /// Make the next read of an agent's configuration fail while mints keep working: the vendor
    /// refusing one call of two that a voice session makes side by side.
    pub fn fail_next_agent_profile(&self, detail: &str) {
        self.lock().fail_agent_profile = Some(detail.to_owned());
    }

    /// Make the next read of an agent's configuration wait `delay` before it answers, while mints
    /// keep working: a vendor slower than a caller's deadline. The read is recorded as it starts,
    /// so a caller that gives up on it has still asked once. Under a paused tokio clock the wait
    /// costs no real time.
    pub fn stall_next_agent_profile(&self, delay: Duration) {
        self.lock().stall_agent_profile = Some(delay);
    }

    /// Declare that `agent_id` exists on this account but speaks in no voice of its own.
    ///
    /// The equivalent of an agent created in the dashboard and never given a voice. Registers the
    /// agent as well, so a caller cannot accidentally set this on an agent the account does not
    /// have and then be testing the 404 path instead.
    pub fn register_voiceless_agent(&self, agent_id: &str) {
        let mut state = self.lock();
        state.known_agents.insert(agent_id.to_owned());
        state.voiceless_agents.insert(agent_id.to_owned());
    }

    /// Every text conversation this fake has held, in order.
    #[must_use]
    pub fn chats(&self) -> Vec<FakeChat> {
        self.lock().chats.clone()
    }

    /// Fix what the agent answers, so a test can drive the formatting path.
    pub fn answer_with(&self, text: &str) {
        self.lock().answer = Some(text.to_owned());
    }

    /// Make the next TURN fail — the conversation is open and the question is refused.
    ///
    /// Distinct from [`FakeElevenLabs::fail_next`], which fails the mint and so means the
    /// conversation was never opened at all. A pool has to survive the first and report the
    /// second, and a fake with one control could not tell them apart.
    pub fn fail_next_turn(&self, detail: &str) {
        self.lock().fail_turn = Some(detail.to_owned());
    }

    /// Declare that this account has `agent_id`.
    pub fn register_agent(&self, agent_id: &str) {
        self.lock().known_agents.insert(agent_id.to_owned());
    }

    /// Change what a successful mint returns, so pass-through can be asserted on a distinctive
    /// value rather than on a constant this module also produces by default.
    pub fn mint(&self, url: &str) {
        self.lock().minted_url = url.to_owned();
    }

    /// Make the next mint fail at the transport, so unreachability can be tested.
    pub fn fail_next(&self, detail: &str) {
        self.lock().fail_with = Some(detail.to_owned());
    }

    /// Every URL this fake was asked for, in order.
    #[must_use]
    pub fn requested(&self) -> Vec<String> {
        self.lock().requested.clone()
    }

    /// How many mints were attempted. "The route never called out" is otherwise
    /// indistinguishable from "it called out and the answer was discarded".
    #[must_use]
    pub fn attempts(&self) -> usize {
        self.lock().requested.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// ElevenLabs' own shape for a rejected key, deliberately QUOTING the key back.
    ///
    /// An upstream is free to echo a credential into its own error text, and
    /// [`SignedUrlError::from_response`]'s redaction is what stops it reaching a log or a caller.
    /// One function rather than a copy per call path, so a path added later cannot be the one
    /// that forgets to redact.
    fn reject_wrong_key(&self, api_key: &crate::config::Secret) -> Result<(), SignedUrlError> {
        if api_key.expose() == self.valid_api_key {
            return Ok(());
        }
        Err(SignedUrlError::from_response(
            401,
            &format!(
                r#"{{"detail":{{"status":"invalid_api_key","message":"the key {} is not valid"}}}}"#,
                api_key.expose()
            ),
            api_key,
        ))
    }
}

#[async_trait]
impl SpeechProvider for FakeElevenLabs {
    async fn speak(
        &self,
        config: &ElevenLabsConfig,
        text: &str,
        speed: Option<f64>,
    ) -> Result<Speech, SpeechError> {
        // Same gate the real client goes through, first, so an unconfigured server cannot be made
        // to look configured by swapping in this fake.
        let api_key = speech_key(config)?;
        // Same resolution the real client performs: an explicit voice wins, otherwise the
        // configured AGENT's own voice is borrowed. A fake that only honoured an explicit voice
        // would leave the path production actually takes untested.
        let voice_id = match configured_voice(config) {
            Some(named) => named.to_owned(),
            None => {
                let agent_id = voice_source_agent(config)?;
                if !self.lock().known_agents.contains(agent_id) {
                    return Err(SpeechError::from_response(
                        404,
                        &format!(
                            r#"{{"detail":{{"status":"agent_not_found","message":"{agent_id}"}}}}"#
                        ),
                        api_key,
                    ));
                }
                KNOWN_VOICE_ID.to_owned()
            }
        };
        let voice_id = voice_id.as_str();
        // ...and the same refusal, BEFORE anything is recorded as spoken: a blank message must not
        // appear in `spoken`, because a test proving "the refusal read nothing" depends on it.
        if text.trim().is_empty() {
            return Err(SpeechError::Empty);
        }
        let request = speech_request(&config.api_base, voice_id);
        self.lock().requested.push(request.url);

        if let Some(detail) = self.lock().fail_with.take() {
            return Err(SpeechError::Transport(detail));
        }
        if api_key.expose() != self.valid_api_key {
            // Quotes the key back deliberately, exactly as the signed-URL path does: an upstream
            // is free to, and the redaction is what stops it reaching a log or a caller.
            return Err(SpeechError::from_response(
                401,
                &format!(
                    r#"{{"detail":{{"status":"invalid_api_key","message":"the key {} is not valid"}}}}"#,
                    api_key.expose()
                ),
                api_key,
            ));
        }
        if !self.lock().known_voices.contains(voice_id) {
            return Err(SpeechError::from_response(
                404,
                &format!(r#"{{"detail":{{"status":"voice_not_found","message":"{voice_id}"}}}}"#),
                api_key,
            ));
        }
        self.lock().spoken.push(text.to_owned());
        self.lock().speeds.push(clamp_speed(speed));
        Ok(Speech {
            audio: fake_audio(text),
            content_type: TTS_CONTENT_TYPE.to_owned(),
        })
    }
}

#[async_trait]
impl SignedUrlProvider for FakeElevenLabs {
    async fn signed_url(&self, config: &ElevenLabsConfig) -> Result<SignedUrl, SignedUrlError> {
        // Same gate the real client goes through, first, so an unconfigured server cannot be made
        // to look configured by swapping in this fake.
        let (agent_id, api_key) = credentials(config)?;
        let request = signed_url_request(&config.api_base, agent_id);
        self.lock().requested.push(request.url);

        if let Some(detail) = self.lock().fail_with.take() {
            return Err(SignedUrlError::Transport(detail));
        }
        if api_key.expose() != self.valid_api_key {
            // ElevenLabs' own shape for a rejected key, and deliberately one that QUOTES the key
            // back: an upstream is free to do that, and the redaction in `from_response` is what
            // stops it reaching a log or a caller.
            return Err(SignedUrlError::from_response(
                401,
                &format!(
                    r#"{{"detail":{{"status":"invalid_api_key","message":"the key {} is not valid"}}}}"#,
                    api_key.expose()
                ),
                api_key,
            ));
        }
        if !self.lock().known_agents.contains(agent_id) {
            return Err(SignedUrlError::from_response(
                404,
                &format!(r#"{{"detail":{{"status":"agent_not_found","message":"{agent_id}"}}}}"#),
                api_key,
            ));
        }
        Ok(SignedUrl {
            signed_url: self.lock().minted_url.clone(),
            agent_id: agent_id.to_owned(),
            valid_for_seconds: super::DOCUMENTED_VALIDITY_SECONDS,
        })
    }

    async fn account(&self, config: &ElevenLabsConfig) -> Result<Account, SignedUrlError> {
        // The KEY alone, exactly as the live client requires: an operator with no agent
        // configured must still be able to find out whether their key works.
        let api_key = config
            .api_key
            .as_ref()
            .filter(|key| !key.expose().trim().is_empty())
            .ok_or(SignedUrlError::NotConfigured("elevenlabs.api_key"))?;
        self.lock()
            .requested
            .push(account_request(&config.api_base).url);
        if let Some(detail) = self.lock().fail_with.take() {
            return Err(SignedUrlError::Transport(detail));
        }
        self.reject_wrong_key(api_key)?;
        Ok(Account {
            user_id: Some(FAKE_USER_ID.to_owned()),
            workspace: Some(FAKE_WORKSPACE.to_owned()),
            tier: Some(FAKE_TIER.to_owned()),
        })
    }

    async fn agent_profile(
        &self,
        config: &ElevenLabsConfig,
    ) -> Result<AgentProfile, SignedUrlError> {
        let (agent_id, api_key) = credentials(config)?;
        self.lock()
            .requested
            .push(agent_request(&config.api_base, agent_id).url);
        let stall = self.lock().stall_agent_profile.take();
        if let Some(delay) = stall {
            tokio::time::sleep(delay).await;
        }
        if let Some(detail) = self.lock().fail_agent_profile.take() {
            return Err(SignedUrlError::Transport(detail));
        }
        if let Some(detail) = self.lock().fail_with.take() {
            return Err(SignedUrlError::Transport(detail));
        }
        self.reject_wrong_key(api_key)?;
        if !self.lock().known_agents.contains(agent_id) {
            return Err(SignedUrlError::from_response(
                404,
                &format!(r#"{{"detail":{{"status":"agent_not_found","message":"{agent_id}"}}}}"#),
                api_key,
            ));
        }
        let voiceless = self.lock().voiceless_agents.contains(agent_id);
        Ok(AgentProfile {
            agent_id: agent_id.to_owned(),
            name: Some("the fake agent".to_owned()),
            voice_id: (!voiceless).then(|| KNOWN_VOICE_ID.to_owned()),
            prompt_override: self.lock().prompt_override,
        })
    }
}

#[async_trait]
impl TextChatProvider for FakeElevenLabs {
    async fn open_text_chat(
        &self,
        config: &ElevenLabsConfig,
        _deadline: Duration,
    ) -> Result<Box<dyn TextChat>, ChatError> {
        // A real conversation begins with a real mint, so this one does too — through this fake's
        // OWN signed-URL path, not around it. That is what makes "this account does not have that
        // key" and "this account does not have that agent" refusals of a CONVERSATION rather than
        // refusals a separate code path would have to reinvent and could get wrong differently.
        let _minted = self.signed_url(config).await.map_err(chat_from_signed)?;
        let mut state = self.lock();
        state.chats.push(FakeChat::default());
        let index = state.chats.len() - 1;
        drop(state);
        Ok(Box::new(FakeTextChat {
            state: Arc::clone(&self.state),
            index,
        }))
    }
}

/// One conversation with the fake agent.
#[derive(Debug)]
pub struct FakeTextChat {
    state: Arc<Mutex<State>>,
    index: usize,
}

impl FakeTextChat {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl TextChat for FakeTextChat {
    async fn ask(&mut self, text: &str, _deadline: Duration) -> Result<String, ChatError> {
        let mut state = self.lock();
        if let Some(detail) = state.fail_turn.take() {
            return Err(ChatError::Transport(detail));
        }
        let chat = &mut state.chats[self.index];
        chat.asked.push(text.to_owned());
        let turn = chat.asked.len();
        // Deliberately NOT the prompt back: a fake that echoed its input would let a summariser
        // that returned the message verbatim pass as one that summarised it. It DOES name the
        // conversation and the turn, because "which socket served this summary" is the one thing
        // a pool has to be able to prove.
        Ok(state.answer.clone().unwrap_or_else(|| {
            format!(
                "the fake agent answered on conversation {} turn {turn}",
                self.index
            )
        }))
    }

    async fn close(self: Box<Self>) {
        self.lock().chats[self.index].closed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Secret, DEFAULT_ELEVENLABS_API_BASE};

    fn config(agent: Option<&str>, key: Option<&str>) -> ElevenLabsConfig {
        ElevenLabsConfig {
            agent_id: agent.map(str::to_owned),
            api_key: key.map(Secret::new),
            api_base: DEFAULT_ELEVENLABS_API_BASE.to_owned(),
            voice_id: None,
        }
    }

    fn good() -> ElevenLabsConfig {
        config(Some(KNOWN_AGENT_ID), Some(VALID_API_KEY))
    }

    #[tokio::test]
    async fn a_correctly_configured_server_gets_a_url() {
        let fake = FakeElevenLabs::new();
        let minted = fake.signed_url(&good()).await.expect("mints");
        assert_eq!(minted.signed_url, MINTED_URL);
        assert_eq!(minted.agent_id, KNOWN_AGENT_ID);
        assert_eq!(fake.attempts(), 1);
        assert!(
            fake.requested()[0].contains(KNOWN_AGENT_ID),
            "the fake must exercise the real URL builder: {:?}",
            fake.requested()
        );
    }

    #[tokio::test]
    async fn a_wrong_key_is_refused_the_way_elevenlabs_refuses_it() {
        // The load-bearing property. If this fake minted a URL for any key, every test below it
        // would be certifying nothing.
        let fake = FakeElevenLabs::new();
        let error = fake
            .signed_url(&config(Some(KNOWN_AGENT_ID), Some("xi-wrong-key")))
            .await
            .expect_err("a key this account does not have must not mint a conversation");
        match error {
            SignedUrlError::Status { status, body } => {
                assert_eq!(status, 401);
                assert!(body.contains("invalid_api_key"), "{body}");
                assert!(!body.contains("xi-wrong-key"), "key echoed back: {body}");
            }
            other => panic!("expected a 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_key_fails_before_any_call_is_attempted() {
        let fake = FakeElevenLabs::new();
        let error = fake
            .signed_url(&config(Some(KNOWN_AGENT_ID), None))
            .await
            .expect_err("an unconfigured server must not mint");
        assert!(
            matches!(&error, SignedUrlError::NotConfigured(f) if *f == "elevenlabs.api_key"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fake.attempts(),
            0,
            "nothing should be requested when there is no key to request it with"
        );
    }

    #[tokio::test]
    async fn an_agent_this_account_does_not_have_is_a_404() {
        let fake = FakeElevenLabs::new();
        let error = fake
            .signed_url(&config(Some("agent_someone_elses"), Some(VALID_API_KEY)))
            .await
            .expect_err("an unknown agent must not mint");
        assert!(
            matches!(&error, SignedUrlError::Status { status, .. } if *status == 404),
            "unexpected error: {error}"
        );
        // Registering it is the counterpart of creating the agent on the account.
        fake.register_agent("agent_someone_elses");
        assert!(fake
            .signed_url(&config(Some("agent_someone_elses"), Some(VALID_API_KEY)))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_text_conversation_is_refused_on_exactly_the_terms_a_mint_is() {
        // The load-bearing property for the summariser tests above this one. A fake that opened a
        // conversation for any key would make every one of them self-certifying.
        let fake = FakeElevenLabs::new();
        let deadline = Duration::from_secs(1);
        assert!(fake.open_text_chat(&good(), deadline).await.is_ok());

        let wrong_key = config(Some(KNOWN_AGENT_ID), Some("xi-wrong-key"));
        assert!(
            matches!(
                fake.open_text_chat(&wrong_key, deadline).await,
                Err(ChatError::Refused(_))
            ),
            "a key this account does not have must not open a conversation"
        );
        let no_key = config(Some(KNOWN_AGENT_ID), None);
        assert!(matches!(
            fake.open_text_chat(&no_key, deadline).await,
            Err(ChatError::NotConfigured("elevenlabs.api_key"))
        ));
        let unknown_agent = config(Some("agent_someone_elses"), Some(VALID_API_KEY));
        assert!(matches!(
            fake.open_text_chat(&unknown_agent, deadline).await,
            Err(ChatError::Refused(_))
        ));
        assert_eq!(
            fake.chats().len(),
            1,
            "a refused conversation must not be recorded as one that happened"
        );
    }

    #[tokio::test]
    async fn a_conversation_records_what_it_was_asked_and_whether_it_was_hung_up() {
        let fake = FakeElevenLabs::new();
        let mut chat = fake
            .open_text_chat(&good(), Duration::from_secs(1))
            .await
            .expect("opens");
        let first = chat
            .ask("summarise this", Duration::from_secs(1))
            .await
            .expect("answers");
        assert!(
            !first.contains("summarise this"),
            "a fake that echoed its input would let a summariser that returned the message \
             verbatim pass: {first}"
        );
        chat.ask("and this", Duration::from_secs(1))
            .await
            .expect("answers");
        assert_eq!(fake.chats()[0].asked, vec!["summarise this", "and this"]);
        assert!(!fake.chats()[0].closed);
        chat.close().await;
        assert!(fake.chats()[0].closed);
    }

    #[tokio::test]
    async fn a_refused_turn_is_one_shot_so_recovery_is_testable() {
        let fake = FakeElevenLabs::new();
        let mut chat = fake
            .open_text_chat(&good(), Duration::from_secs(1))
            .await
            .expect("opens");
        fake.fail_next_turn("the conversation was closed at the far end");
        assert!(chat.ask("one", Duration::from_secs(1)).await.is_err());
        assert!(chat.ask("two", Duration::from_secs(1)).await.is_ok());
    }

    #[tokio::test]
    async fn injected_transport_failures_surface_and_are_one_shot() {
        let fake = FakeElevenLabs::new();
        fake.fail_next("dns lookup failed");
        assert!(matches!(
            fake.signed_url(&good()).await,
            Err(SignedUrlError::Transport(_))
        ));
        assert!(fake.signed_url(&good()).await.is_ok());
    }
}
