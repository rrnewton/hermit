//! The owner's own words for the voice agent: where they are kept, what Settings shows of them,
//! and how they reach a call.
//!
//! `#272 voice-agent-instructions`. The owner asked for "a section for additional instructions that
//! the user can fill in" and "a collapse section to actually edit the system prompt", and guessed
//! that a hosted agent would refuse the second while still taking the first. So there are two
//! texts ([`OwnerPrompt`]):
//!
//! * **Additional instructions**, added to every call: "call me Sam", "keep it under three
//!   sentences".
//! * **The base prompt itself**, his own edited copy of [`voice_agent::SYSTEM_PROMPT`].
//!
//! # What neither can change
//!
//! The section that says what the agent can do — read only, send after asking, or send at once —
//! is not his to edit. [`voice_agent::prompt_with`] still chooses it from the credential and the
//! send setting and puts it after his words, and the server enforces both whatever the prompt says:
//! a read token cannot post, and with "Ask me before the voice agent sends" on, `post_reply` only
//! proposes. Editing the prompt cannot grant a power the token lacks.
//!
//! # Where they live
//!
//! One row in the store, written only by a write-scope `PUT /api/v1/voice-prompt` that Settings
//! makes. No MCP tool reaches it — a model that could rewrite its own instructions could undo the
//! rules about untrusted chat text — and [`crate::mcp::protocol::dispatch`] refuses any tool name
//! the manifest does not list. They are readable at read scope, because the assembled prompt is:
//! a bridge holding the read token fetches it.
//!
//! # It fails toward the shipped prompt
//!
//! [`current`] answers the shipped prompt whenever the store cannot say what the owner saved: a
//! broken store must never be what changes what the agent is told.
//!
//! # How they reach a call
//!
//! * **A deployment-managed bridge** fetches `GET /api/v1/voice-agent` as each call opens; the
//!   prompt there is assembled with both texts, so his next call has them.
//! * **A hosted agent** is configured in its vendor's console, and the page opens its socket. So
//!   the page hands his words over as the socket opens, in the way [`for_session`] chooses: the
//!   whole assembled prompt as an override when the agent's security settings allow one, and
//!   otherwise his instructions alone, as a note right after the conversation starts. An edited
//!   base cannot reach such an agent at all, and the session says so rather than pretending.
//!
//! The override is sent only when the agent's own configuration says it is allowed. One it does
//! not allow is believed — from the vendor's documentation, not from a call observed here — to end
//! the conversation rather than be ignored, so "could not tell" is treated as "not allowed", and
//! the note is the fallback, never the override.
//!
//! # An agent that allows it always gets this server's prompt
//!
//! Even with nothing of the owner's saved. The prompt carries the send rules, and those follow the
//! Sending switch ([`Variant`]); a console prompt pasted by hand does not, so after the switch moved
//! such an agent would go on reading back a message the server had already posted, or announce as
//! sent one still waiting for a yes. Handing an agent that allows it the assembled prompt on every
//! call is what makes its rules follow the switch. The cost is that its console prompt is never
//! used, which the QUICKSTART says before an operator allows it. An agent that does not allow it
//! is handed nothing when nothing is saved, and its call opens exactly as it always did; the
//! session still says `not_sent`, so the page can tell the owner that agent keeps its console
//! prompt.
//!
//! # The vendor is asked once in a while, not on every call
//!
//! Whether an agent allows the override is a round trip of its own, beside the mint. The answer is
//! kept for [`AGENT_CHECK_TTL`] per agent ([`OverrideAnswers`]), so most calls do not wait on it at
//! all. A failure or a timeout counts as "not allowed", and is kept too, but only for
//! [`AGENT_CHECK_FAILURE_TTL`]: a deployment where the read always fails — a key that can mint a
//! session but not read the agent, or a vendor that keeps timing out — must not wait up to
//! [`AGENT_CHECK_DEADLINE`] and log a warning at every call start, and a vendor that was briefly
//! down is asked again soon.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::ConversationBackend;
use crate::contract::{PromptDelivery, PromptTransport, SessionPrompt, VoicePromptSettings};
use crate::ops::OpError;
use crate::send_approval;
use crate::state::AppState;
use crate::store::StoreError;
use crate::voice_agent::{self, EditedPrompt, OwnerPrompt, Variant};

/// The owner's words in force, as of this call. Read on every profile and every voice session, so
/// a save in Settings reaches the next call.
///
/// What he saved, or the shipped prompt when he saved nothing or no store is configured. Any other
/// store failure is the shipped prompt too, with a warning.
pub async fn current(state: &AppState) -> OwnerPrompt {
    match state.store.owner_prompt().await {
        Ok(Some(prompt)) => prompt,
        Ok(None) | Err(StoreError::Unavailable(_)) => OwnerPrompt::default(),
        Err(error) => {
            tracing::warn!(
                %error,
                "could not read the owner's instructions for the voice agent; using the shipped prompt"
            );
            OwnerPrompt::default()
        }
    }
}

/// How the owner's words reach this deployment's voice agent: through the profile a bridge fetches,
/// or through the page, for a hosted agent.
#[must_use]
pub fn delivery(state: &AppState) -> PromptDelivery {
    match state.config.conversation.backend {
        ConversationBackend::ElevenLabs => PromptDelivery::Page,
        ConversationBackend::WebSocket => PromptDelivery::Profile,
    }
}

fn settings(state: &AppState, owner: &OwnerPrompt, changeable: bool) -> VoicePromptSettings {
    VoicePromptSettings {
        instructions: owner.instructions.clone(),
        system_prompt: owner.base().to_owned(),
        system_prompt_edited: owner.system_prompt.is_some(),
        edited_from_version: owner
            .system_prompt
            .as_ref()
            .map(|edited| edited.from_version),
        prompt_version: voice_agent::PROMPT_VERSION,
        max_instructions_chars: u32::try_from(voice_agent::MAX_INSTRUCTIONS_CHARS)
            .unwrap_or(u32::MAX),
        max_system_prompt_chars: u32::try_from(voice_agent::MAX_SYSTEM_PROMPT_CHARS)
            .unwrap_or(u32::MAX),
        changeable,
        delivery: delivery(state),
    }
}

/// The owner's words as Settings shows them, and whether this server can keep a change.
///
/// `changeable` is false when no store is configured, and when the configured one cannot be read —
/// a page must not offer an editor whose saved text it cannot know.
pub async fn view(state: &AppState) -> VoicePromptSettings {
    let (owner, changeable) = match state.store.owner_prompt().await {
        Ok(Some(prompt)) => (prompt, true),
        Ok(None) => (OwnerPrompt::default(), true),
        Err(StoreError::Unavailable(_)) => (OwnerPrompt::default(), false),
        Err(error) => {
            tracing::warn!(%error, "could not read the voice agent's instructions for Settings");
            (OwnerPrompt::default(), false)
        }
    };
    settings(state, &owner, changeable)
}

/// One save from Settings. A field left `None` keeps that text as it is, so the two editors save
/// independently.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    /// The new additional instructions; empty removes them.
    pub instructions: Option<String>,
    /// The new base prompt; empty, or the shipped prompt itself, restores the shipped one.
    pub system_prompt: Option<String>,
}

/// Record a change and return the owner's words as they now stand. **Write scope**, and the
/// owner's act alone: there is no tool for it.
///
/// # Errors
///
/// [`OpError::Store`] with [`StoreError::TooLarge`] for a text over its limit, and when no store is
/// configured, the store cannot be read (a change is applied to what is saved, and a save over an
/// unknown row could drop the other text), or the write fails.
pub async fn set(state: &AppState, change: Change) -> Result<VoicePromptSettings, OpError> {
    let instructions = change
        .instructions
        .as_deref()
        .map(voice_agent::clean_instructions)
        .transpose()
        .map_err(StoreError::TooLarge)?;
    let system_prompt = change
        .system_prompt
        .as_deref()
        .map(voice_agent::clean_system_prompt)
        .transpose()
        .map_err(StoreError::TooLarge)?;
    let mut owner = state.store.owner_prompt().await?.unwrap_or_default();
    if let Some(instructions) = instructions {
        owner.instructions = instructions;
    }
    if let Some(text) = system_prompt {
        owner.system_prompt = text.map(|text| {
            // The version shipped when he made THIS text: saving the same words again keeps it,
            // so a re-save does not hide that the shipped prompt has moved on.
            let from_version = match &owner.system_prompt {
                Some(kept) if kept.text == text => kept.from_version,
                _ => voice_agent::PROMPT_VERSION,
            };
            EditedPrompt { text, from_version }
        });
    }
    let stored = state.store.set_owner_prompt(&owner).await?;
    // Sizes and a flag, and none of his words: the access log is content-free.
    tracing::info!(
        target: "vibe_talk::access",
        event = "changed",
        instructions_chars = stored.instructions.chars().count(),
        system_prompt_edited = stored.system_prompt.is_some(),
        "voice_prompt"
    );
    Ok(settings(state, &stored, true))
}

/// The longest a call's start waits to learn whether the hosted agent allows its prompt to be
/// replaced, when no answer is kept. Asked beside the session mint rather than after it, so this is
/// the most it can add; past it the answer counts as "not allowed", which sends the instructions as
/// a note instead.
pub const AGENT_CHECK_DEADLINE: Duration = Duration::from_millis(2_500);

/// How long the vendor's answer about one agent is believed. Short, because it is believed in both
/// directions: an operator who withdraws the permission at the vendor while this still says
/// "allowed" has his next calls handed an override the agent refuses — believed to end each such
/// call — until it runs out. The QUICKSTART says so.
pub const AGENT_CHECK_TTL: Duration = Duration::from_secs(120);

/// How long a failure to read the agent's configuration — an error or a timeout — is kept, as "not
/// allowed". Long enough that a deployment where the read always fails pays the round trip, and logs
/// its warning, once in a while rather than at every call start; short enough that a vendor that
/// was briefly down keeps the override away for seconds, not for the whole of [`AGENT_CHECK_TTL`].
pub const AGENT_CHECK_FAILURE_TTL: Duration = Duration::from_secs(20);

/// What the vendor last said about each agent letting a client replace its prompt, and when.
/// `#272 voice-agent-instructions`.
///
/// An answer is kept for [`AGENT_CHECK_TTL`]. A failure or a timeout is kept as "not allowed" for
/// [`AGENT_CHECK_FAILURE_TTL`] only, so a vendor that was briefly down does not keep the override
/// away for long, and one that always fails is not asked at every call start.
#[derive(Debug, Default)]
pub struct OverrideAnswers {
    answers: Mutex<HashMap<String, KeptAnswer>>,
}

/// One kept answer: what the agent's configuration said, and when it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeptAnswer {
    /// `AgentProfile::prompt_override` as read: `None` when the configuration did not say, or when
    /// it could not be read.
    pub allowed: Option<bool>,
    /// When it was read, or when the read failed.
    pub at: Instant,
    /// The read failed or timed out, so there is no answer: kept for [`AGENT_CHECK_FAILURE_TTL`].
    pub failed: bool,
}

impl KeptAnswer {
    /// How long this is believed: an answer for [`AGENT_CHECK_TTL`], a failure for
    /// [`AGENT_CHECK_FAILURE_TTL`].
    #[must_use]
    pub fn lifetime(&self) -> Duration {
        if self.failed {
            AGENT_CHECK_FAILURE_TTL
        } else {
            AGENT_CHECK_TTL
        }
    }
}

impl OverrideAnswers {
    /// Nothing kept yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What is kept for `agent`, if it is younger than its [`KeptAnswer::lifetime`] at `now`.
    #[must_use]
    pub fn fresh(&self, agent: &str, now: Instant) -> Option<KeptAnswer> {
        let answers = self
            .answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        answers
            .get(agent)
            .copied()
            .filter(|kept| now.saturating_duration_since(kept.at) < kept.lifetime())
    }

    /// Keep what the vendor said about `agent` at `at`, in place of anything kept before.
    pub fn keep(&self, agent: &str, allowed: Option<bool>, at: Instant) {
        self.insert(
            agent,
            KeptAnswer {
                allowed,
                at,
                failed: false,
            },
        );
    }

    /// Keep that asking the vendor about `agent` failed or timed out at `at`, in place of anything
    /// kept before: "not allowed", for [`AGENT_CHECK_FAILURE_TTL`].
    pub fn keep_failure(&self, agent: &str, at: Instant) {
        self.insert(
            agent,
            KeptAnswer {
                allowed: None,
                at,
                failed: true,
            },
        );
    }

    fn insert(&self, agent: &str, kept: KeptAnswer) {
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(agent.to_owned(), kept);
    }
}

/// Where the hosted agent is asked about, and which agent: the key its answer is kept under, so two
/// configurations cannot share an answer.
fn agent_key(config: &crate::config::ElevenLabsConfig) -> String {
    format!(
        "{}\n{}",
        config.api_base,
        config.agent_id.as_deref().unwrap_or_default()
    )
}

/// Whether the configured hosted agent lets a client replace its prompt: what is kept while it is
/// fresh, otherwise the vendor's answer, asked within [`AGENT_CHECK_DEADLINE`]. `None` when it did
/// not say, could not be asked, or did not answer in time — all of which count as "not allowed",
/// and the last two of which are kept for [`AGENT_CHECK_FAILURE_TTL`].
async fn override_allowed(state: &AppState) -> Option<bool> {
    let config = &state.config.elevenlabs;
    let agent = agent_key(config);
    if let Some(kept) = state.agent_override.fresh(&agent, Instant::now()) {
        return kept.allowed;
    }
    match tokio::time::timeout(AGENT_CHECK_DEADLINE, state.elevenlabs.agent_profile(config)).await {
        Ok(Ok(profile)) => {
            state
                .agent_override
                .keep(&agent, profile.prompt_override, Instant::now());
            profile.prompt_override
        }
        Ok(Err(error)) => {
            state.agent_override.keep_failure(&agent, Instant::now());
            tracing::warn!(
                %error,
                "could not read whether the voice agent allows its prompt to be replaced; \
                 treating it as not allowed for {} s",
                AGENT_CHECK_FAILURE_TTL.as_secs()
            );
            None
        }
        Err(_) => {
            state.agent_override.keep_failure(&agent, Instant::now());
            tracing::warn!(
                "the voice agent's configuration did not answer in time; treating a prompt \
                 override as not allowed for {} s",
                AGENT_CHECK_FAILURE_TTL.as_secs()
            );
            None
        }
    }
}

/// What the page should hand a hosted agent as this call opens, or `None` for a provider that
/// reads the profile instead.
///
/// An agent that allows its prompt to be replaced is handed the assembled prompt whether or not the
/// owner has written anything, so its send rules follow the Sending switch (see the module
/// documentation). One that does not, with nothing of his that can go, is handed nothing — but the
/// session still says so (`not_sent`, with what the vendor said), because that is how the page
/// learns the agent keeps its console prompt: without it, the warning beside the Sending switch
/// could only ever say "if".
///
/// The hosted agent is given the prompt for a credential that may post, under the send setting —
/// the voice session that carries this needs the write token, and an agent that can propose is the
/// one the voice page exists to confirm for. This server cannot see which token the operator gave
/// the agent's MCP connection at the vendor. One given only the read token should not be allowed a
/// prompt override: it would be told it may send, and the server would refuse every send. The
/// QUICKSTART and Settings say so.
pub async fn for_session(state: &AppState) -> Option<SessionPrompt> {
    if delivery(state) != PromptDelivery::Page {
        return None;
    }
    let owner = current(state).await;
    let allowed = override_allowed(state).await;
    let variant = Variant::for_caller(true, send_approval::current(state).await);
    Some(session_prompt(variant, &owner, allowed))
}

/// The choice [`for_session`] makes, once it knows what the agent allows.
#[must_use]
pub fn session_prompt(
    variant: Variant,
    owner: &OwnerPrompt,
    allowed: Option<bool>,
) -> SessionPrompt {
    if allowed == Some(true) {
        let assembled = voice_agent::prompt_with(variant, owner);
        return SessionPrompt {
            transport: PromptTransport::PromptOverride,
            text: assembled.text,
            system_prompt_applied: assembled.system_prompt_edited,
            instructions_applied: assembled.additional_instructions,
            override_allowed: allowed,
        };
    }
    match voice_agent::owner_note(owner) {
        Some(note) => SessionPrompt {
            transport: PromptTransport::ContextualUpdate,
            text: note,
            system_prompt_applied: false,
            instructions_applied: true,
            override_allowed: allowed,
        },
        None => SessionPrompt {
            transport: PromptTransport::NotSent,
            text: String::new(),
            system_prompt_applied: false,
            instructions_applied: false,
            override_allowed: allowed,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StateStore as _;
    use crate::testing;
    use std::sync::Arc;

    fn owner(instructions: &str, base: Option<&str>) -> OwnerPrompt {
        OwnerPrompt {
            instructions: instructions.to_owned(),
            system_prompt: base.map(|text| EditedPrompt {
                text: text.to_owned(),
                from_version: voice_agent::PROMPT_VERSION,
            }),
        }
    }

    fn change(instructions: Option<&str>, system_prompt: Option<&str>) -> Change {
        Change {
            instructions: instructions.map(str::to_owned),
            system_prompt: system_prompt.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn with_nothing_saved_the_agent_gets_the_shipped_prompt_and_settings_can_save() {
        let (state, _discord, _store) = testing::state_with_store();
        assert!(current(&state).await.is_shipped());
        let shown = view(&state).await;
        assert!(shown.changeable);
        assert_eq!(shown.instructions, "");
        assert_eq!(shown.system_prompt, voice_agent::SYSTEM_PROMPT);
        assert!(!shown.system_prompt_edited);
        assert_eq!(shown.edited_from_version, None);
        assert_eq!(shown.prompt_version, voice_agent::PROMPT_VERSION);
        assert_eq!(shown.max_instructions_chars, 4_000);
    }

    #[tokio::test]
    async fn the_two_texts_save_independently_and_an_empty_box_restores() {
        let (state, _discord, _store) = testing::state_with_store();
        let saved = set(&state, change(Some("Call me Sam.\r\n"), None))
            .await
            .expect("save");
        assert_eq!(saved.instructions, "Call me Sam.");
        assert!(!saved.system_prompt_edited);

        let saved = set(&state, change(None, Some("You read me the chat.")))
            .await
            .expect("save");
        assert_eq!(
            saved.instructions, "Call me Sam.",
            "the other text was lost"
        );
        assert!(saved.system_prompt_edited);
        assert_eq!(saved.system_prompt, "You read me the chat.");
        assert_eq!(saved.edited_from_version, Some(voice_agent::PROMPT_VERSION));
        assert_eq!(
            current(&state).await,
            owner("Call me Sam.", Some("You read me the chat."))
        );

        // The shipped prompt pasted back is not an edit, and neither is an empty box.
        for restore in [String::new(), voice_agent::SYSTEM_PROMPT.to_owned()] {
            set(&state, change(None, Some("mine"))).await.expect("save");
            let restored = set(&state, change(None, Some(&restore)))
                .await
                .expect("save");
            assert!(!restored.system_prompt_edited);
            assert_eq!(restored.system_prompt, voice_agent::SYSTEM_PROMPT);
            assert_eq!(restored.instructions, "Call me Sam.");
        }
        let cleared = set(&state, change(Some(""), None)).await.expect("save");
        assert_eq!(cleared.instructions, "");
        assert!(current(&state).await.is_shipped());
    }

    #[tokio::test]
    async fn an_edit_keeps_the_version_it_was_made_from_until_its_words_change() {
        let (state, _discord, store) = testing::state_with_store();
        store
            .set_owner_prompt(&OwnerPrompt {
                instructions: String::new(),
                system_prompt: Some(EditedPrompt {
                    text: "mine".to_owned(),
                    from_version: 3,
                }),
            })
            .await
            .expect("seed");
        let same = set(&state, change(Some("Be brief."), Some("mine")))
            .await
            .expect("save");
        assert_eq!(
            same.edited_from_version,
            Some(3),
            "a re-save hid an older edit"
        );
        let new = set(&state, change(None, Some("mine, changed")))
            .await
            .expect("save");
        assert_eq!(new.edited_from_version, Some(voice_agent::PROMPT_VERSION));
    }

    #[tokio::test]
    async fn a_text_over_its_limit_is_refused_and_nothing_is_saved() {
        let (state, _discord, _store) = testing::state_with_store();
        let long = "x".repeat(voice_agent::MAX_INSTRUCTIONS_CHARS + 1);
        let refused = set(&state, change(Some(&long), Some("mine")))
            .await
            .expect_err("too long");
        assert_eq!(refused.code(), "too_large");
        assert!(
            current(&state).await.is_shipped(),
            "half a refused save was kept"
        );
    }

    #[tokio::test]
    async fn no_store_means_the_shipped_prompt_and_an_editor_that_cannot_save() {
        let (state, _discord) =
            testing::state_with(Arc::new(crate::store::disabled::DisabledStore));
        assert!(current(&state).await.is_shipped());
        assert!(!view(&state).await.changeable);
        assert_eq!(
            set(&state, change(Some("Be brief."), None))
                .await
                .expect_err("nowhere to keep it")
                .code(),
            "storage_not_configured"
        );
    }

    #[tokio::test]
    async fn a_store_that_fails_to_answer_gives_the_shipped_prompt_and_refuses_a_save() {
        let (state, _discord, store) = testing::state_with_store();
        store
            .set_owner_prompt(&owner("Be brief.", Some("mine")))
            .await
            .expect("seed");
        store.fail_next("the disk is gone");
        assert!(
            current(&state).await.is_shipped(),
            "a broken store changed the prompt"
        );
        store.fail_next("the disk is gone");
        assert!(!view(&state).await.changeable);
        store.fail_next("the disk is gone");
        assert!(set(&state, change(None, Some("other"))).await.is_err());
        assert_eq!(current(&state).await, owner("Be brief.", Some("mine")));
    }

    #[test]
    fn an_agent_that_allows_it_gets_the_whole_prompt_with_the_section_last() {
        let mine = owner("Call me Sam.", Some("You read me the chat."));
        for variant in [Variant::WriteConfirm, Variant::WriteDirect] {
            let sent = session_prompt(variant, &mine, Some(true));
            assert_eq!(sent.transport, PromptTransport::PromptOverride);
            assert_eq!(sent.text, voice_agent::prompt_with(variant, &mine).text);
            assert!(sent.system_prompt_applied && sent.instructions_applied);
            assert_eq!(sent.override_allowed, Some(true));
        }
    }

    #[test]
    fn an_agent_that_refuses_or_cannot_say_gets_the_instructions_as_a_note_and_never_an_override() {
        let mine = owner("Call me Sam.", Some("You read me the chat."));
        for allowed in [Some(false), None] {
            let sent = session_prompt(Variant::WriteConfirm, &mine, allowed);
            assert_eq!(
                sent.transport,
                PromptTransport::ContextualUpdate,
                "{allowed:?}"
            );
            assert_eq!(
                sent.text,
                format!("{}\nCall me Sam.", voice_agent::OWNER_INSTRUCTIONS_HEADING)
            );
            assert!(sent.instructions_applied);
            assert!(
                !sent.system_prompt_applied,
                "an edit it cannot take was reported applied"
            );
            assert!(
                !sent.text.contains("You read me the chat."),
                "the edited base went out as a note"
            );
            assert_eq!(sent.override_allowed, allowed);

            // Only an edited base, and nowhere to put it: nothing goes, and it says so.
            let none = session_prompt(Variant::WriteConfirm, &owner("", Some("mine")), allowed);
            assert_eq!(none.transport, PromptTransport::NotSent);
            assert!(none.text.is_empty());
            assert!(!none.system_prompt_applied && !none.instructions_applied);
        }
    }

    #[test]
    fn an_agent_that_allows_it_gets_the_shipped_prompt_even_with_nothing_saved() {
        for variant in [Variant::WriteConfirm, Variant::WriteDirect] {
            let sent = session_prompt(variant, &OwnerPrompt::default(), Some(true));
            assert_eq!(sent.transport, PromptTransport::PromptOverride);
            assert_eq!(
                sent.text,
                voice_agent::prompt(variant).text,
                "the send rules for the switch did not go"
            );
            assert!(!sent.system_prompt_applied && !sent.instructions_applied);
        }
    }

    #[test]
    fn a_kept_answer_is_believed_for_its_time_and_for_its_agent_only() {
        let answers = OverrideAnswers::new();
        let read = Instant::now();
        assert_eq!(
            answers.fresh("agent_a", read),
            None,
            "an answer nobody gave"
        );
        answers.keep("agent_a", Some(true), read);
        let kept = answers.fresh("agent_a", read + AGENT_CHECK_TTL - Duration::from_millis(1));
        assert_eq!(kept.map(|kept| kept.allowed), Some(Some(true)));
        assert_eq!(
            answers.fresh("agent_a", read + AGENT_CHECK_TTL),
            None,
            "an answer outlived its time"
        );
        assert_eq!(
            answers.fresh("agent_b", read),
            None,
            "one agent's answer was believed of another"
        );
        // "It did not say" is an answer too, and is kept as one.
        answers.keep("agent_b", None, read);
        assert_eq!(
            answers.fresh("agent_b", read).map(|kept| kept.allowed),
            Some(None)
        );
        // A later answer replaces the earlier one.
        answers.keep("agent_a", Some(false), read + Duration::from_secs(1));
        assert_eq!(
            answers
                .fresh("agent_a", read + Duration::from_secs(1))
                .map(|kept| kept.allowed),
            Some(Some(false))
        );
    }

    #[test]
    fn a_failure_is_kept_as_not_allowed_for_less_time_than_an_answer() {
        let answers = OverrideAnswers::new();
        let failed = Instant::now();
        answers.keep_failure("agent_a", failed);
        let kept = answers
            .fresh(
                "agent_a",
                failed + AGENT_CHECK_FAILURE_TTL - Duration::from_millis(1),
            )
            .expect("a failure that was not kept");
        assert_eq!(kept.allowed, None, "a failure was kept as an answer");
        assert!(kept.failed);
        assert_eq!(
            answers.fresh("agent_a", failed + AGENT_CHECK_FAILURE_TTL),
            None,
            "a failure was believed for as long as an answer"
        );
        assert!(AGENT_CHECK_FAILURE_TTL < AGENT_CHECK_TTL);
        // An answer read after it replaces it, and is kept for an answer's time.
        let read = failed + AGENT_CHECK_FAILURE_TTL;
        answers.keep("agent_a", Some(true), read);
        let kept = answers.fresh("agent_a", read + AGENT_CHECK_TTL - Duration::from_millis(1));
        assert_eq!(
            kept.map(|kept| (kept.allowed, kept.failed)),
            Some((Some(true), false))
        );
    }

    #[tokio::test]
    async fn the_vendor_is_asked_once_for_a_run_of_calls_and_a_failure_is_kept_briefly() {
        let (state, _discord, store, elevenlabs) = testing::state_with_store_and_voice();
        let agent_reads = || {
            elevenlabs
                .requested()
                .iter()
                .filter(|url| url.contains("/convai/agents/"))
                .count()
        };
        elevenlabs.fail_next_agent_profile("the vendor is down");
        elevenlabs.allow_prompt_override(Some(true));
        let failed = for_session(&state)
            .await
            .expect("a hosted agent's session says");
        assert_eq!(
            failed.transport,
            PromptTransport::NotSent,
            "a vendor that did not answer was taken for an agent that allows an override"
        );
        assert_eq!(failed.override_allowed, None);
        assert_eq!(agent_reads(), 1);
        // The failure is kept, briefly: a deployment where the read always fails must not wait on
        // the vendor, and log a warning, at every call start.
        for _ in 0..2 {
            let again = for_session(&state)
                .await
                .expect("a hosted agent's session says");
            assert_eq!(again.transport, PromptTransport::NotSent);
        }
        assert_eq!(
            agent_reads(),
            1,
            "every call start asked a vendor that had just failed"
        );
        // Once the failure has run out, the vendor is asked again.
        let lapsed = Instant::now()
            .checked_sub(AGENT_CHECK_FAILURE_TTL)
            .expect("the clock has run that long");
        state
            .agent_override
            .keep_failure(&agent_key(&state.config.elevenlabs), lapsed);
        let first = for_session(&state).await.expect("an agent that allows it");
        assert_eq!(first.transport, PromptTransport::PromptOverride);
        assert_eq!(agent_reads(), 2, "a failure was kept past its time");
        store
            .set_owner_prompt(&owner("Call me Sam.", None))
            .await
            .expect("seed");
        for _ in 0..3 {
            let next = for_session(&state).await.expect("handed over");
            assert_eq!(next.transport, PromptTransport::PromptOverride);
        }
        assert_eq!(agent_reads(), 2, "each call start asked the vendor again");
    }

    #[tokio::test(start_paused = true)]
    async fn a_vendor_that_does_not_answer_in_time_is_kept_as_a_failure_and_not_asked_again() {
        // A vendor that keeps timing out is the costlier of the two failures: every call start it
        // is asked at waits out the whole deadline. Its timeout is kept as "not allowed", as an
        // error is, so the next call start neither waits on it nor believes an answer it gives.
        let (state, _discord, _store, elevenlabs) = testing::state_with_store_and_voice();
        let agent_reads = || {
            elevenlabs
                .requested()
                .iter()
                .filter(|url| url.contains("/convai/agents/"))
                .count()
        };
        elevenlabs.allow_prompt_override(Some(true));
        let stall = AGENT_CHECK_DEADLINE * 4;
        elevenlabs.stall_next_agent_profile(stall);
        let started = tokio::time::Instant::now();
        let timed_out = for_session(&state)
            .await
            .expect("a hosted agent's session says");
        assert!(
            started.elapsed() < stall,
            "the call start waited for the vendor past its deadline"
        );
        assert_eq!(
            timed_out.transport,
            PromptTransport::NotSent,
            "a vendor that did not answer in time was taken for an agent that allows an override"
        );
        assert_eq!(timed_out.override_allowed, None);
        assert_eq!(agent_reads(), 1);
        let next = for_session(&state)
            .await
            .expect("a hosted agent's session says");
        assert_eq!(
            agent_reads(),
            1,
            "the next call start asked a vendor that had just timed out"
        );
        assert_eq!(
            next.transport,
            PromptTransport::NotSent,
            "a timeout was not kept as not allowed"
        );
    }

    #[tokio::test]
    async fn a_call_carries_nothing_when_nothing_was_written_or_a_bridge_reads_the_profile() {
        let (state, _discord, _store) = testing::state_with_store();
        assert_eq!(delivery(&state), PromptDelivery::Page);
        // The test agent's configuration does not say: nothing goes, and the session says so.
        assert_eq!(
            for_session(&state).await,
            Some(SessionPrompt {
                transport: PromptTransport::NotSent,
                text: String::new(),
                system_prompt_applied: false,
                instructions_applied: false,
                override_allowed: None,
            }),
            "the shipped prompt needs no handing over"
        );

        let toml = format!(
            "{}\n[conversation]\nbackend = \"websocket\"\nwebsocket_url = \"/ws\"\n",
            testing::config_toml()
        );
        let (bridge, _discord, store) = testing::state_with_store_from_toml(&toml);
        store
            .set_owner_prompt(&owner("Call me Sam.", None))
            .await
            .expect("seed");
        assert_eq!(delivery(&bridge), PromptDelivery::Profile);
        assert_eq!(
            for_session(&bridge).await,
            None,
            "a bridge reads the profile; the page must not send the words twice"
        );
    }

    #[tokio::test]
    async fn a_refusing_agent_with_nothing_saved_is_reported_not_sent_rather_than_left_unsaid() {
        // The vendor's default: the override not allowed, and nothing saved. Nothing goes, but the
        // session says not_sent, not nothing at all, so the page can say that agent keeps the
        // prompt in its console rather than "if its provider lets this page replace it" for good.
        let (state, _discord, _store, elevenlabs) = testing::state_with_store_and_voice();
        elevenlabs.allow_prompt_override(Some(false));
        assert_eq!(
            for_session(&state).await,
            Some(SessionPrompt {
                transport: PromptTransport::NotSent,
                text: String::new(),
                system_prompt_applied: false,
                instructions_applied: false,
                override_allowed: Some(false),
            }),
            "the page was not told the agent keeps its console prompt"
        );
    }
}
