//! Every configuration check this server can make, re-run on demand and returned structured.
//!
//! # Why this exists
//!
//! [`crate::probe`] already told the causes of an unreachable channel apart — five of them, each
//! with a remedy naming the click that fixes it — and then printed the answer to a console, once,
//! at startup, and threw it away. Everything downstream of that was blind. The phone app could
//! not say why a digest was empty; a reader could not tell "ElevenLabs rejected my key" from
//! "the agent id is from another workspace" from "the server never had a key at all", because all
//! three arrive at a page as the same failed request. The knowledge existed and could not be got
//! at, which is the specific defect this module closes.
//!
//! # What it is not
//!
//! It is not a health check. `/healthz` answers "this process is alive" and deliberately says
//! nothing about the configuration, because it is unauthenticated. This is the opposite: it is
//! authenticated, it says a great deal about the configuration, and it is worth nothing as a
//! liveness signal because it is *supposed* to answer 200 while reporting that six things are
//! broken.
//!
//! # Three rules
//!
//! * **No credential, ever.** Not the bot token, not the API key, not the read or write token,
//!   and not a fragment of one. Every string in the finished report goes through
//!   [`crate::elevenlabs::redact_all`] against every secret the deployment holds — see
//!   [`redacted`]. That is belt and braces on top of the vendor error types, which already redact
//!   at construction, and it is deliberate: a vendor is free to echo a credential back inside an
//!   error body, and this route's whole job is to show error bodies to somebody.
//! * **Bounded.** These are LIVE vendor calls made in response to somebody pressing a button. Each
//!   one gets [`CHECK_BUDGET`] and the report as a whole gets [`TOTAL_BUDGET`]; a vendor that does
//!   not answer becomes [`Diagnosis::TimedOut`] — a failed check with a reason — and never a
//!   request that hangs.
//! * **Read-only.** Nothing here writes. The storage check proves writability by taking the write
//!   lock and rolling back, precisely so that a READ-scope caller does not cause a durable write;
//!   see [`crate::store::StateStore::check_writable`].
//!
//! Beside the checks, one section that is not a check: `summaries`, a copy of the summary route's
//! counters, which are held in memory and are numbers only (`#208 persistent-cache`).
//!
//! # What has actually been exercised
//!
//! Every path below has been driven against this repository's in-memory Discord and in-memory
//! ElevenLabs, including a fake that says no in each distinguishable way. **None of it has met
//! live Discord or live ElevenLabs.** The endpoint shapes and the status codes come from the
//! vendors' documentation, exactly as the rest of this crate's vendor knowledge does.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::Secret;
use crate::elevenlabs::{
    configured_voice, redact_all, speech_key, AgentProfile, SignedUrlError, SpeechError,
};
use crate::probe::{self, Diagnosis};
use crate::state::AppState;
use crate::store::StoreError;

/// How long any ONE check may take before it is reported as a timeout.
///
/// Five seconds is chosen against what a person will sit through, not against what a vendor
/// might need: this route exists to be pressed by somebody wondering why nothing works, and an
/// answer that takes half a minute to say "the network is down" has told them the same thing the
/// spinner already did.
pub const CHECK_BUDGET: Duration = Duration::from_secs(5);

/// How long the WHOLE report may take.
///
/// Necessary as well as [`CHECK_BUDGET`], not instead of it: the number of checks grows with the
/// number of configured channels, so a per-check bound alone would let twenty dead channels add
/// up to a hundred seconds. When this is spent the remaining checks are reported as timeouts
/// immediately, which is honest — they did not run, and saying so beats omitting them.
pub const TOTAL_BUDGET: Duration = Duration::from_secs(20);

/// How a check came out, in the three words an interface can colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The check established what it set out to establish.
    Pass,
    /// It passed, and something about the answer is worth saying out loud anyway.
    Warn,
    /// It did not pass. There is always a remedy.
    Fail,
}

impl Status {
    /// Read the status off a diagnosis.
    #[must_use]
    pub fn of(diagnosis: &Diagnosis) -> Self {
        if diagnosis.is_failure() {
            Self::Fail
        } else if diagnosis.is_warning() {
            Self::Warn
        } else {
            Self::Pass
        }
    }
}

/// One finished check, as it goes over the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    /// Stable machine-readable identifier, e.g. `discord.channel`. A client branches on this;
    /// it never branches on the prose.
    pub id: &'static str,
    /// What was being checked, in words.
    pub title: String,
    /// Which thing of that kind, when there is more than one — a channel snowflake, say.
    pub subject: Option<String>,
    /// Pass, warn, or fail.
    pub status: Status,
    /// The headline, from the shared vocabulary in [`crate::probe`].
    pub summary: String,
    /// The evidence: an account name, a vendor's own error body, a store's reason.
    pub detail: Option<String>,
    /// What to do about it. Absent only when there is nothing to do.
    ///
    /// **Present on every failure.** That is asserted by a test, because a diagnostic that
    /// reports a red row and then says nothing about it is the thing this route was built to
    /// replace.
    pub remedy: Option<String>,
    /// True when nothing was called because a setting is absent.
    ///
    /// Kept apart from an ordinary failure because "the vendor said no" and "you never gave us a
    /// key" look identical in a list of red rows and are fixed completely differently.
    pub unconfigured: bool,
}

/// Every check, and the arithmetic over them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiagnosticsReport {
    /// True when nothing failed. Warnings do not clear it and do not set it.
    pub ok: bool,
    /// How many passed, warnings included.
    pub passed: usize,
    /// How many are warnings.
    pub warned: usize,
    /// How many failed.
    pub failed: usize,
    /// The checks, in the order they were run, which is the order they are worth reading.
    pub checks: Vec<CheckReport>,
    /// The whole-report deadline, so a reader can tell a slow vendor from a broken one.
    pub budget_seconds: u64,
    /// How long the run actually took.
    pub took_ms: u64,
    /// How the summary route has spent its requests since this server started: numbers only, no
    /// key, id or text. `#208 persistent-cache`; see [`crate::summarize::counters`].
    pub summaries: crate::summarize::counters::SummaryCounts,
    /// Each channel whose provider has `recent_view_shadow` on, with what the shadow has counted
    /// since this process started: Flat reads, and where they disagreed with `/messages`. `#208
    /// persistent-cache`; see [`crate::recent_view`]. Counts only, no message text or id. Absent
    /// when no provider has the shadow on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_view_shadow: Vec<crate::recent_view::ChannelTally>,
    /// How long each provider's operations and Web API calls have taken since the process
    /// started, and how big the messages read were. Numbers only: no channel, message or text.
    /// Not a check, and never a failure. `#208 persistent-cache`; see [`crate::upstream_profile`].
    pub upstream: crate::upstream_profile::UpstreamReport,
}

/// Redact every secret this deployment holds out of a string.
///
/// The last line of defence rather than the only one: the vendor error types redact at
/// construction, and this catches anything that arrived by a path nobody thought about — a store
/// backend error quoting a connection string, a transport error quoting a URL somebody put a
/// token in.
///
/// Built from [`secrets_of`] rather than from its own list, so that a test asserting "none of
/// these appears" and the code doing the removing cannot disagree about what "these" are. A
/// second, hand-maintained list here is exactly how a credential added later comes to be the one
/// that is not redacted.
#[must_use]
pub fn redacted(text: &str, state: &AppState) -> String {
    redact_all(text, &secrets_of(state))
}

/// A whole-report deadline, and the per-call budget derived from it.
struct Deadline {
    started: Instant,
    total: Duration,
}

impl Deadline {
    fn new(total: Duration) -> Self {
        Self {
            started: Instant::now(),
            total,
        }
    }

    /// What the next call gets: the per-check budget, or whatever is left of the total, whichever
    /// is smaller. Zero once the total is spent, which turns every remaining check into a
    /// timeout rather than an omission.
    fn budget(&self) -> Duration {
        self.total
            .saturating_sub(self.started.elapsed())
            .min(CHECK_BUDGET)
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Run `work` inside the deadline, turning an elapsed budget into a diagnosis rather than a hang.
async fn within<T, F>(deadline: &Deadline, work: F) -> Result<T, Diagnosis>
where
    F: std::future::Future<Output = T>,
{
    let budget = deadline.budget();
    tokio::time::timeout(budget, work)
        .await
        .map_err(|_elapsed| Diagnosis::TimedOut(budget.as_secs()))
}

/// Turn an ElevenLabs failure into the shared vocabulary.
///
/// `not_found` is passed in because a 404 means different things on the two endpoints this is
/// used for: on the agent lookup it is "this account has no such agent", and on the account
/// lookup it is a vendor that moved an endpoint. Guessing one for both would send an operator to
/// change an agent id that was correct.
fn classify_elevenlabs(error: &SignedUrlError, not_found: Diagnosis) -> Diagnosis {
    match error {
        SignedUrlError::NotConfigured(setting) => Diagnosis::NotConfigured(setting),
        SignedUrlError::Transport(detail) => Diagnosis::Unreachable(detail.clone()),
        SignedUrlError::Shape(detail) => Diagnosis::Unintelligible(detail.clone()),
        SignedUrlError::Status { status, body } => match status {
            401 | 403 => Diagnosis::KeyRejected,
            404 => not_found,
            429 => Diagnosis::RateLimited,
            other => Diagnosis::UnexpectedStatus {
                status: *other,
                body: body.clone(),
            },
        },
    }
}

/// Turn a store failure into the shared vocabulary.
fn classify_store(error: &StoreError) -> Diagnosis {
    match error {
        StoreError::Unavailable(setting) => Diagnosis::NotConfigured(setting),
        other => Diagnosis::StorageUnusable(other.to_string()),
    }
}

/// One check before it is redacted and serialized.
struct Check {
    id: &'static str,
    title: String,
    subject: Option<String>,
    /// The chat provider a `discord.` check is about, when the deployment has several.
    provider: Option<String>,
    diagnosis: Diagnosis,
}

impl Check {
    fn new(id: &'static str, title: impl Into<String>, diagnosis: Diagnosis) -> Self {
        Self {
            id,
            title: title.into(),
            subject: None,
            provider: None,
            diagnosis,
        }
    }

    fn about(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    fn for_provider(mut self, provider: &str) -> Self {
        self.provider = Some(provider.to_owned());
        self
    }

    fn finish(self, state: &AppState) -> CheckReport {
        // The stable check ids are retained for existing API consumers. Display names and
        // remedies describe the actual backend, including compatible provider adapters.
        let provider = if self.id.starts_with("discord.") {
            self.provider
                .as_deref()
                .unwrap_or_else(|| state.chat.provider_name())
        } else if self.id.starts_with("elevenlabs.") {
            "ElevenLabs"
        } else {
            "the provider"
        };
        let remedy = self.diagnosis.remedy_for(provider);
        CheckReport {
            id: self.id,
            title: self.title,
            subject: self.subject,
            status: Status::of(&self.diagnosis),
            summary: self.diagnosis.headline_for(provider),
            detail: self
                .diagnosis
                .detail()
                .map(|detail| redacted(&detail, state)),
            remedy: (!remedy.is_empty()).then(|| redacted(&remedy, state)),
            unconfigured: self.diagnosis.is_unconfigured(),
        }
    }
}

/// Run every check and assemble the report.
///
/// Sequential rather than concurrent, deliberately. These calls go to two vendors that both rate
/// limit, the Discord half shares one token's budget with everything else this server does, and
/// the whole run is bounded anyway — so firing them all at once would buy a couple of seconds at
/// the cost of a route that can rate-limit the deployment it was asked to diagnose.
pub async fn run(state: &AppState) -> DiagnosticsReport {
    let deadline = Deadline::new(TOTAL_BUDGET);
    let mut checks = Vec::new();

    checks.extend(discord_token(state, &deadline).await);
    checks.extend(discord_channels(state, &deadline).await);

    checks.push(read_aloud_selection(state));

    checks.push(elevenlabs_key(state, &deadline).await);
    // One vendor call answers both of these, and the voice check reads the agent's verdict as
    // well as its profile — so the voice row is BUILT first and PUSHED second, which is the
    // cheapest way to keep reading order without cloning a diagnosis to satisfy the borrow.
    let (agent, profile) = elevenlabs_agent(state, &deadline).await;
    let voice = elevenlabs_voice(state, profile.as_ref(), &agent);
    checks.push(
        Check::new("elevenlabs.agent", "ElevenLabs agent", agent)
            .about(state.config.elevenlabs.agent_id.clone().unwrap_or_default()),
    );
    checks.push(voice);

    checks.push(summaries(state));
    checks.push(storage(state, &deadline).await);

    let checks: Vec<CheckReport> = checks
        .into_iter()
        .map(|check| check.finish(state))
        .collect();
    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    let warned = checks.iter().filter(|c| c.status == Status::Warn).count();
    DiagnosticsReport {
        ok: failed == 0,
        passed: checks.len() - failed,
        warned,
        failed,
        checks,
        budget_seconds: TOTAL_BUDGET.as_secs(),
        took_ms: deadline.elapsed_ms(),
        summaries: state.summary_counters.snapshot(),
        recent_view_shadow: crate::recent_view::report(state),
        // Read after the checks, so the calls this report just made are counted too.
        upstream: state.providers.profile().report(),
    }
}

/// Identify the selected playback path without claiming that audio played on the device.
fn read_aloud_selection(state: &AppState) -> Check {
    let provider = state.speech.describe();
    let playback = match provider.playback {
        crate::speech::Playback::Browser => {
            "the browser uses the device's speech engine and selects a voice it reports as local; \
             no ElevenLabs credentials are needed for this read-aloud mode. The server cannot \
             verify browser voice availability or audible playback: open the message view, \
             enable Read, and tap a message on the device"
        }
        crate::speech::Playback::Audio => {
            "the server generates audio through this provider; this check confirms the selected \
             backend only and does not synthesize or play audio"
        }
    };
    Check::new(
        "read_aloud",
        "Read-aloud backend selection",
        Diagnosis::Confirmed(format!("{} selected: {playback}", provider.label)),
    )
    .about(provider.backend)
}

/// Did each chat provider accept its token at all, and whose token is it?
///
/// Asked WITHOUT naming a channel, which is the entire point: a bad token and an uninvited bot
/// are the two most common ways this deployment is wrong, and a channel read reports both as a
/// channel that cannot be read. One check per configured provider.
async fn discord_token(state: &AppState, deadline: &Deadline) -> Vec<Check> {
    let mut checks = Vec::new();
    let multi = state.providers.is_multi();
    for entry in state.providers.entries() {
        let provider = entry.client.provider_name();
        let diagnosis = match within(deadline, entry.client.identity()).await {
            Err(timed_out) => timed_out,
            Ok(Ok(identity)) => Diagnosis::Confirmed(format!(
                "{provider} accepted the token; it belongs to {} ({})",
                identity.username, identity.id
            )),
            Ok(Err(error)) => probe::classify(&error),
        };
        let check = Check::new("discord.token", format!("{provider} token"), diagnosis)
            .for_provider(provider);
        checks.push(if multi {
            check.about(entry.key.as_str())
        } else {
            check
        });
    }
    checks
}

/// Can the bot read each configured channel? The five causes, told apart.
async fn discord_channels(state: &AppState, deadline: &Deadline) -> Vec<Check> {
    let channels = state.all_channels();
    let report =
        probe::probe_channels_within(state.chat.as_ref(), &channels, Some(deadline.budget())).await;
    report
        .outcomes
        .into_iter()
        .map(|outcome| {
            let provider = outcome
                .channel
                .provider
                .as_deref()
                .and_then(|key| state.providers.entry(key))
                .map_or_else(
                    || state.chat.provider_name().to_owned(),
                    |entry| entry.client.provider_name().to_owned(),
                );
            Check::new(
                "discord.channel",
                format!("{provider} channel: {}", outcome.channel.label),
                outcome.diagnosis,
            )
            .for_provider(&provider)
            .about(outcome.channel.id.as_str())
        })
        .collect()
}

/// Was the ElevenLabs API key accepted, and which account did it reach?
async fn elevenlabs_key(state: &AppState, deadline: &Deadline) -> Check {
    let diagnosis = match speech_key(&state.config.elevenlabs) {
        Err(SpeechError::NotConfigured(setting)) => Diagnosis::NotConfigured(setting),
        // `speech_key` returns exactly one variant on failure; anything else here would be a
        // change in that function, and reporting it as unconfigured would be a lie.
        Err(other) => Diagnosis::Unintelligible(other.to_string()),
        Ok(_key) => {
            match within(deadline, state.elevenlabs.account(&state.config.elevenlabs)).await {
                Err(timed_out) => timed_out,
                Ok(Ok(account)) => Diagnosis::Confirmed(account.describe()),
                // A 404 here is not an unknown agent — no agent was named — so it is reported as the
                // unexpected status it is rather than sending the operator to fix an agent id.
                Ok(Err(error)) => classify_elevenlabs(
                    &error,
                    Diagnosis::UnexpectedStatus {
                        status: 404,
                        body: "ElevenLabs has no account endpoint at the configured api_base"
                            .to_owned(),
                    },
                ),
            }
        }
    };
    Check::new("elevenlabs.api_key", "ElevenLabs API key", diagnosis)
}

/// Does the configured agent exist, and does it report a voice?
///
/// Hands the profile back as well as the check, so the voice check below can reuse the one call
/// rather than spending a second vendor request to learn the same thing.
async fn elevenlabs_agent(
    state: &AppState,
    deadline: &Deadline,
) -> (Diagnosis, Option<AgentProfile>) {
    let config = &state.config.elevenlabs;
    match within(deadline, state.elevenlabs.agent_profile(config)).await {
        Err(timed_out) => (timed_out, None),
        Ok(Ok(profile)) => match &profile.voice_id {
            Some(voice) => (
                Diagnosis::Confirmed(format!(
                    "agent {}{} exists and speaks in voice {voice}",
                    profile.agent_id,
                    profile
                        .name
                        .as_ref()
                        .map_or_else(String::new, |name| format!(" (\"{name}\")")),
                )),
                Some(profile),
            ),
            // The agent is REAL and has no voice. Reported against the agent as well as against
            // the voice, because the fix is on the agent.
            None => (Diagnosis::NoVoice, Some(profile)),
        },
        Ok(Err(error)) => (classify_elevenlabs(&error, Diagnosis::UnknownAgent), None),
    }
}

/// Does a voice resolve — either configured outright, or borrowed from the agent?
///
/// No vendor call of its own: it is a question about how two settings combine, and the answer is
/// already in hand. `elevenlabs.voice_id` is OPTIONAL, and this check is the thing that makes
/// that claim checkable rather than a sentence in a README.
///
/// `agent` is the diagnosis the agent check reached, and it is taken for one specific reason. If
/// the agent lookup TIMED OUT, or the key was rejected, then this check knows nothing about
/// whether a voice would have resolved — and answering [`Diagnosis::NoVoice`] there would print
/// "give the agent a voice" over a network failure, which is a wrong instruction, not merely an
/// unhelpful one. So the real cause is carried down instead, and the row says what actually went
/// wrong.
fn elevenlabs_voice(state: &AppState, profile: Option<&AgentProfile>, agent: &Diagnosis) -> Check {
    let config = &state.config.elevenlabs;
    let diagnosis = match (
        configured_voice(config),
        profile.and_then(|p| p.voice_id.as_deref()),
    ) {
        // An explicitly named voice needs no agent at all, so it resolves even when the agent
        // check went red. That is not optimism: read-aloud really does work in this state.
        (Some(named), _) => Diagnosis::Confirmed(format!(
            "elevenlabs.voice_id names {named} explicitly, so ElevenLabs synthesis uses that voice"
        )),
        (None, Some(borrowed)) => Diagnosis::Confirmed(format!(
            "elevenlabs.voice_id is unset, so ElevenLabs synthesis borrows the agent's own voice, \
             {borrowed} — which is the intended default and needs no setting"
        )),
        // Nothing configured, and nothing to borrow. Either the agent genuinely has no voice —
        // in which case `agent` is already `NoVoice` and repeating it is right — or the agent
        // could not be read at all, in which case THAT is the fault and this row must say so.
        (None, None) => agent.clone(),
    };
    Check::new("elevenlabs.voice", "ElevenLabs voice", diagnosis)
}

/// Can this deployment summarise anything at all?
///
/// With `[summaries.llm]` configured, the answer is about that section's files; otherwise it is
/// about the ElevenLabs agent, as below.
///
/// No vendor call of its own — the key and the agent already have rows above, and asking a third
/// time would spend a request to learn the same thing. What this row adds is the CONSEQUENCE,
/// which neither of those rows states: there is exactly one summariser and it is the ElevenLabs
/// agent, so a deployment without credentials does not get shorter summaries, it gets none, and
/// every long row on `/voice` shows as failed. Before the extractive backend was removed the
/// honest answer here was "summaries still work, without a model", and a reader who remembers
/// that needs to be told it changed.
///
/// [`crate::elevenlabs::credentials`] rather than `speech_key`: a summary needs the agent id as
/// well as the account key, and read-aloud does not.
fn summaries(state: &AppState) -> Check {
    // `#233 llm-summarizer`. With `[summaries.llm]` the ElevenLabs credentials are irrelevant to
    // summaries, and judging them here would paint a working deployment red. What can be known
    // without a model call is whether the files it reads are there.
    if let Some(llm) = &state.config.summaries.llm {
        let diagnosis = match crate::summarize::messages_api::unusable_file(llm) {
            Some((setting, _why)) => Diagnosis::NotConfigured(setting),
            None => Diagnosis::Confirmed(format!(
                "summaries and thread names are produced by {} ({}, model {}), under policy \
                 version {}",
                state.summarizer.describe(),
                llm.dialect.as_str(),
                llm.model,
                state.summary_version
            )),
        };
        return Check::new("summaries", "Message summaries", diagnosis);
    }
    let diagnosis = match crate::elevenlabs::credentials(&state.config.elevenlabs) {
        Err(SignedUrlError::NotConfigured(setting)) => Diagnosis::NotConfigured(setting),
        // `credentials` returns exactly one variant on failure. Anything else is a change in that
        // function, and calling it unconfigured would be a guess.
        Err(other) => Diagnosis::Unintelligible(other.to_string()),
        Ok(_) => Diagnosis::Confirmed(format!(
            "summaries are produced by {}, under policy version {}",
            state.summarizer.describe(),
            state.summary_version
        )),
    };
    Check::new("summaries", "Message summaries", diagnosis)
}

/// Can the durable store be written to? Without writing to it.
async fn storage(state: &AppState, deadline: &Deadline) -> Check {
    let diagnosis = match within(deadline, state.store.check_writable()).await {
        Err(timed_out) => timed_out,
        Ok(Ok(evidence)) => Diagnosis::Confirmed(evidence),
        Ok(Err(error)) => classify_store(&error),
    };
    Check::new("storage", "Durable storage", diagnosis)
}

/// Every secret a deployment holds, for the tests that assert none of them can appear.
///
/// Exposed so a test does not have to rebuild the list and quietly miss the one that leaked.
#[must_use]
pub fn secrets_of(state: &AppState) -> Vec<Secret> {
    let mut secrets: Vec<Secret> = state
        .config
        .providers
        .iter()
        .map(|provider| provider.token().clone())
        .collect();
    secrets.push(state.config.auth.read_token.clone());
    secrets.push(state.config.auth.write_token.clone());
    if let Some(token) = &state.config.ingest.token {
        secrets.push(token.clone());
    }
    if let Some(key) = &state.config.elevenlabs.api_key {
        secrets.push(key.clone());
    }
    secrets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    struct NamedChat(std::sync::Arc<crate::discord::fake::FakeDiscord>);

    #[async_trait::async_trait]
    impl crate::chat::ChatClient for NamedChat {
        fn provider_name(&self) -> &str {
            "Google Chat"
        }

        async fn identity(&self) -> Result<crate::chat::ChatIdentity, crate::chat::ChatError> {
            self.0.identity().await
        }

        async fn fetch_page(
            &self,
            channel: &crate::model::ChannelId,
            limit: u16,
            before: Option<&crate::model::MessageId>,
            after: Option<&crate::model::MessageId>,
        ) -> Result<Vec<crate::model::Message>, crate::chat::ChatError> {
            self.0.fetch_page(channel, limit, before, after).await
        }

        async fn post_message(
            &self,
            _channel: &crate::model::ChannelId,
            _content: &str,
            _reply_to: Option<&crate::model::MessageId>,
        ) -> Result<crate::model::Message, crate::chat::ChatError> {
            panic!("diagnostics must never post")
        }
    }

    #[tokio::test]
    async fn chat_checks_take_their_platform_name_from_the_backend() {
        let (mut state, fake) = testing::state();
        state.replace_chat(std::sync::Arc::new(NamedChat(std::sync::Arc::clone(&fake))));
        for rejected in [false, true] {
            if rejected {
                fake.revoke_token();
            }
            let report = run(&state).await;
            for check in report
                .checks
                .iter()
                .filter(|check| check.id.starts_with("discord."))
            {
                assert!(check.title.starts_with("Google Chat"), "{check:?}");
                let text = format!(
                    "{} {} {:?} {:?}",
                    check.title, check.summary, check.detail, check.remedy
                );
                assert!(!text.contains("Discord"), "{text}");
                assert!(!text.contains("Reset Token"), "{text}");
            }
        }
        let rate_limited =
            Check::new("elevenlabs.key", "ElevenLabs key", Diagnosis::RateLimited).finish(&state);
        assert!(rate_limited.summary.starts_with("ElevenLabs"));
    }

    #[tokio::test]
    async fn a_correctly_wired_deployment_reports_every_check_green() {
        let (state, _discord) = testing::state();
        let report = run(&state).await;
        assert!(report.ok, "{:#?}", report.checks);
        assert_eq!(report.failed, 0);
        // One token check, two channels, read-aloud selection, key, agent, voice, summaries, storage.
        assert_eq!(report.checks.len(), 9, "{:#?}", report.checks);
    }

    #[tokio::test]
    async fn a_deployment_with_no_credentials_is_told_it_cannot_summarise_at_all() {
        // The row exists because the consequence changed and nothing else in this report says so:
        // there is one summariser, it is the ElevenLabs agent, and without credentials the answer
        // is not "shorter summaries" but "no summaries, and every long row shows as failed".
        let (state, _discord, _vendor) =
            testing::state_from_toml(&testing::config_toml_without_elevenlabs());
        let report = run(&state).await;
        let row = report
            .checks
            .iter()
            .find(|c| c.id == "summaries")
            .unwrap_or_else(|| panic!("no summaries row at all: {:#?}", report.checks));
        assert_eq!(row.status, Status::Fail, "{row:#?}");
        assert!(
            row.unconfigured,
            "an absent setting must not be reported as a vendor that said no: {row:#?}"
        );
        assert!(
            row.remedy
                .as_deref()
                .is_some_and(|r| r.contains("elevenlabs.api_key")),
            "the remedy has to name the setting to add: {row:#?}"
        );

        // A key with NO AGENT. This is not the same absence: read-aloud works in this state, so a
        // check written against `speech_key` — which is what the key row above uses — would call
        // this green while every summary still failed. A summary needs an agent to ask.
        let keyed = testing::config_toml_without_elevenlabs()
            + "\n[elevenlabs]\napi_key = \"xi-a-key-with-nothing-to-ask\"\n";
        let (state, _discord, _vendor) = testing::state_from_toml(&keyed);
        let report = run(&state).await;
        let row = report
            .checks
            .iter()
            .find(|c| c.id == "summaries")
            .expect("a summaries row");
        assert_eq!(row.status, Status::Fail, "{row:#?}");
        assert!(
            row.remedy
                .as_deref()
                .is_some_and(|r| r.contains("elevenlabs.agent_id")),
            "a key with no agent to ask cannot summarise, and the row has to name the agent id \
             rather than the key that is already there: {row:#?}"
        );

        // THE CONTROL. The same row, on a wired deployment, passes and says which summariser —
        // without it, "the row is red" is satisfied by a row that is always red.
        let (wired, _discord) = testing::state();
        let report = run(&wired).await;
        let row = report
            .checks
            .iter()
            .find(|c| c.id == "summaries")
            .expect("a summaries row");
        assert_eq!(row.status, Status::Pass, "{row:#?}");
        assert!(
            row.detail
                .as_deref()
                .is_some_and(|d| d.contains(wired.summarizer.describe())),
            "a passing row still has to say WHICH summariser answers, in that summariser's own \
             words: {row:#?}"
        );
    }

    #[tokio::test]
    async fn every_failing_check_says_what_to_do_about_it() {
        // The property the whole route exists for. A red row with no remedy is the "unavailable"
        // this replaces.
        let (state, discord) = testing::state();
        discord.revoke_token();
        let report = run(&state).await;
        assert!(!report.ok);
        for check in report.checks.iter().filter(|c| c.status == Status::Fail) {
            let remedy = check.remedy.as_deref().unwrap_or("");
            assert!(
                remedy.len() > 20,
                "{} has no usable remedy: {check:#?}",
                check.id
            );
        }
    }

    #[test]
    fn a_status_is_read_off_the_shared_vocabulary_rather_than_invented() {
        assert_eq!(
            Status::of(&Diagnosis::Confirmed("fine".to_owned())),
            Status::Pass
        );
        assert_eq!(Status::of(&Diagnosis::NoVoice), Status::Fail);
        assert_eq!(
            Status::of(&Diagnosis::Readable {
                messages: 1,
                suspect_message_content_intent: true
            }),
            Status::Warn
        );
    }

    #[test]
    fn a_404_is_classified_differently_on_the_two_endpoints_it_can_arrive_from() {
        // An agent lookup's 404 means "wrong agent id"; the account lookup's does not, and telling
        // an operator to fix an agent id that was never sent is worse than saying nothing.
        let not_found = SignedUrlError::Status {
            status: 404,
            body: "{}".to_owned(),
        };
        assert_eq!(
            classify_elevenlabs(&not_found, Diagnosis::UnknownAgent),
            Diagnosis::UnknownAgent
        );
        assert!(matches!(
            classify_elevenlabs(
                &not_found,
                Diagnosis::UnexpectedStatus {
                    status: 404,
                    body: String::new()
                }
            ),
            Diagnosis::UnexpectedStatus { .. }
        ));
    }

    #[test]
    fn a_missing_setting_is_kept_apart_from_a_vendor_refusal() {
        assert!(Diagnosis::NotConfigured("elevenlabs.api_key").is_unconfigured());
        assert!(!Diagnosis::KeyRejected.is_unconfigured());
        assert!(Diagnosis::NotConfigured("elevenlabs.api_key").is_failure());
    }
}
