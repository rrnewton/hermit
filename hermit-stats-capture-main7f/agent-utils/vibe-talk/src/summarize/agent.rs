//! A summariser backed by the ElevenLabs conversational agent, over a POOLED text-only socket.
//!
//! # The thing that makes this different from an HTTP client
//!
//! A ConvAI socket is a **conversation**, not a request channel. Everything sent down one stays in
//! the agent's context, including the agent's own previous answers. Summarising twenty messages
//! down one socket therefore makes each summary an input to the next: the context grows without
//! bound, the token cost grows with it, and by the tenth summary the agent is answering about
//! message ten while looking at nine summaries of other people's messages. There is no event that
//! resets conversation history, so the only way to get a clean context is a new socket.
//!
//! The opposite extreme is a socket per summary, which is correct and wasteful: every summary then
//! pays a mint, a TLS handshake, a WebSocket upgrade and an initiation round trip before the
//! question is even asked, and ElevenLabs bills connection time as well as tokens.
//!
//! So: **one pooled conversation, recycled every [`DEFAULT_MAX_PER_SOCKET`] summaries and closed
//! after [`DEFAULT_SOCKET_IDLE_SECONDS`] of quiet.** Both numbers are configuration, and both are
//! folded into the cache key, because both change what a summary says.
//!
//! # Why eight
//!
//! Counting in prompt-equivalents — one prompt being the instruction plus one message plus its
//! context window — the k-th summary on a shared socket carries roughly `k` of them, so a socket
//! that serves `N` summaries costs about `N(N+1)/2` where `N` separate sockets would cost `N`.
//! At `N = 8` that is 36 prompt-equivalents for 8 summaries, and the handshake is paid once
//! instead of eight times. At `N = 32` it is 528 for 32, and the quadratic term has comfortably
//! overtaken the handshake it was meant to amortise. Eight is also about as many messages as one
//! person reads through in a sitting before the pause exceeds the idle timeout and the socket is
//! closed anyway, so in practice the recycle limit is the *backstop* and the idle timeout is what
//! usually retires a conversation.
//!
//! None of this is measured against ElevenLabs, because nothing here has met ElevenLabs. It is an
//! argument from the shape of the protocol, and it is configuration precisely so a deployment that
//! measures something different can disagree.
//!
//! # Plain text, and why the instruction is not the whole answer
//!
//! The summary is rendered with `textContent` and read aloud, so a markdown bullet is not
//! formatting there — it is an asterisk on the screen and a word in the ear. [`PLAIN_TEXT_RULE`]
//! asks the model for plain text, and [`plain`] flattens what comes back, because an instruction
//! is a request and a model is free to decline it.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use super::{Summarizer, Summary, SummaryError, SummaryRequest, PROMPT};
use crate::config::{ElevenLabsConfig, SummariesConfig};
use crate::elevenlabs::{ChatError, TextChat, TextChatProvider};

/// The name this backend contributes to [`super::policy_version`].
pub const BACKEND: &str = "elevenlabs-agent";

/// How many summaries share one conversation before it is recycled. See the module doc for the
/// arithmetic behind the number.
pub const DEFAULT_MAX_PER_SOCKET: usize = 8;

/// How long a conversation may sit unused before it is closed.
///
/// Thirty seconds, chosen against the reading pattern rather than against a benchmark: a person
/// working down a channel taps the next message within seconds, and a person who has stopped has
/// usually stopped for minutes. Holding the socket across the first gap saves a handshake; holding
/// it across the second is paying a vendor to keep a conversation nobody is having.
pub const DEFAULT_SOCKET_IDLE_SECONDS: u64 = 30;

/// How long one turn — or one whole opening handshake — may take before it is abandoned.
///
/// One setting for both because they fail the same way and are fixed the same way, and because a
/// second number would have to be justified separately without any evidence to justify it with.
pub const DEFAULT_REPLY_TIMEOUT_SECONDS: u64 = 30;

/// What this backend adds to [`PROMPT`], and the whole of the owner's plain-text requirement.
///
/// Part of [`Summarizer::policy_input`], so editing this sentence makes every summary produced
/// under the old one unreachable — the same rule [`PROMPT`] is under, for the same reason.
pub const PLAIN_TEXT_RULE: &str =
    "Answer with plain text only. No markdown, no asterisks, no underscores, no bullet points, no \
     headings, no numbered lists, no code fences, no emoji: the answer is inserted into a page as \
     literal text and is also read aloud, so a formatting character is shown verbatim and spoken. \
     Answer with the summary itself and nothing else — no preamble, no sign-off, and no question \
     back.";

/// How a pooled conversation is retired, and how long anything is waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolPolicy {
    /// Summaries one conversation may serve before it is recycled.
    pub max_per_socket: usize,
    /// Quiet after which a held conversation is closed.
    pub idle_after: Duration,
    /// Ceiling on one turn, and on one opening handshake.
    pub deadline: Duration,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            max_per_socket: DEFAULT_MAX_PER_SOCKET,
            idle_after: Duration::from_secs(DEFAULT_SOCKET_IDLE_SECONDS),
            deadline: Duration::from_secs(DEFAULT_REPLY_TIMEOUT_SECONDS),
        }
    }
}

impl PoolPolicy {
    /// Read the policy out of the summary settings.
    #[must_use]
    pub const fn from_config(config: &SummariesConfig) -> Self {
        Self {
            max_per_socket: config.max_per_socket,
            idle_after: Duration::from_secs(config.socket_idle_seconds),
            deadline: Duration::from_secs(config.reply_timeout_seconds),
        }
    }
}

/// What the pool has actually done, so a test can assert on it and an operator can read it.
///
/// Counts rather than gauges: "how many conversations were opened for these twenty summaries" is
/// the question the whole design is an answer to, and a gauge cannot answer it after the fact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentStats {
    /// Conversations opened against the vendor.
    pub conversations_opened: usize,
    /// Summaries answered.
    pub summaries: usize,
    /// Conversations closed because their turn budget was spent.
    pub recycled: usize,
    /// Conversations closed because nothing came for [`PoolPolicy::idle_after`].
    pub idle_closed: usize,
    /// Times a pooled conversation failed and a fresh one was opened to ask again.
    pub retried: usize,
}

/// Summaries from the configured ElevenLabs conversational agent.
pub struct AgentSummarizer {
    inner: Arc<Inner>,
    reaper: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for AgentSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSummarizer")
            .field("policy", &self.inner.policy)
            .field("stats", &self.stats())
            .finish()
    }
}

impl Drop for AgentSummarizer {
    fn drop(&mut self) {
        // The reaper holds a Weak and would stop on its own at the next tick; aborting means a
        // process that is shutting down does not wait out an idle timeout to do it.
        if let Some(reaper) = self.reaper.take() {
            reaper.abort();
        }
    }
}

struct Inner {
    chats: Arc<dyn TextChatProvider>,
    elevenlabs: ElevenLabsConfig,
    policy: PoolPolicy,
    policy_input: String,
    /// The pool: at most one conversation, because a conversation cannot serve two turns at once
    /// without interleaving them. An async mutex rather than a std one because it is held across
    /// the vendor round trip.
    pool: tokio::sync::Mutex<Option<Pooled>>,
    stats: Mutex<AgentStats>,
}

struct Pooled {
    chat: Box<dyn TextChat>,
    /// Summaries this conversation has already answered, which is also how much of the agent's
    /// context is other people's messages rather than this one.
    served: usize,
    last_used: Instant,
}

/// One completed round trip, with its cost broken out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Timing {
    /// Time spent opening a conversation, or zero when a held one was reused. This is the number
    /// the pool exists to avoid paying.
    connected_in: Duration,
    /// Time from asking to the agent's final word.
    answered_in: Duration,
    /// Which turn of the conversation this was, counting from one.
    socket_turn: usize,
    /// Whether a held conversation was reused.
    reused: bool,
}

impl AgentSummarizer {
    /// Build a summariser over `chats`, for the agent `elevenlabs` names.
    ///
    /// Starts a background task that closes the pooled conversation once it has been quiet for
    /// [`PoolPolicy::idle_after`]. Outside a Tokio runtime there is no task to start, and the
    /// conversation is then retired on the next summary instead — said out loud rather than
    /// silently, because "closed when idle" would otherwise be a claim with nothing behind it.
    #[must_use]
    pub fn new(
        chats: Arc<dyn TextChatProvider>,
        elevenlabs: ElevenLabsConfig,
        policy: PoolPolicy,
    ) -> Self {
        let agent = elevenlabs
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or("(unset)")
            .to_owned();
        let inner = Arc::new(Inner {
            chats,
            elevenlabs,
            policy,
            // WHICH agent answers decides what a summary says at least as much as the prompt does
            // — it carries its own system prompt, its own model and its own voice of writing — and
            // it lives outside `SummariesConfig`, so this is the only place it can enter the cache
            // key. Pointing the deployment at a different agent must not serve summaries the old
            // one wrote.
            policy_input: format!("{PROMPT}\n{PLAIN_TEXT_RULE}\nagent={agent}"),
            pool: tokio::sync::Mutex::new(None),
            stats: Mutex::new(AgentStats::default()),
        });
        let reaper = spawn_reaper(&inner);
        Self { inner, reaper }
    }

    /// What the pool has done so far.
    #[must_use]
    pub fn stats(&self) -> AgentStats {
        *self
            .inner
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Close the pooled conversation if it has been quiet long enough. Returns whether it did.
    ///
    /// Public because it is what the background task calls, and a guarantee whose only exercise is
    /// a timer inside a spawned task is a guarantee no test can state.
    pub async fn close_if_idle(&self) -> bool {
        self.inner.close_if_idle().await
    }
}

/// Start the idle reaper, if there is a runtime to start it on.
fn spawn_reaper(inner: &Arc<Inner>) -> Option<tokio::task::JoinHandle<()>> {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            "no Tokio runtime at construction, so the summariser's idle conversation reaper is \
             NOT running; a pooled conversation will be retired on the next summary instead of on \
             a timer, and until then the vendor is billing for it"
        );
        return None;
    };
    // A Weak, so the task cannot be the reason a summariser stays alive: a leaked task holding an
    // Arc would hold a vendor socket open for the life of the process.
    let weak = Arc::downgrade(inner);
    // Never zero, or this becomes a busy loop against a mutex.
    let every = inner.policy.idle_after.max(Duration::from_millis(100));
    Some(handle.spawn(reap(weak, every)))
}

async fn reap(weak: Weak<Inner>, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        let Some(inner) = weak.upgrade() else { return };
        if inner.close_if_idle().await {
            tracing::debug!("closed an idle agent conversation; the vendor bills connection time");
        }
    }
}

impl Inner {
    fn note(&self, change: impl FnOnce(&mut AgentStats)) {
        change(
            &mut self
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    async fn close_if_idle(&self) -> bool {
        let mut slot = self.pool.lock().await;
        let idle = slot
            .as_ref()
            .is_some_and(|held| held.last_used.elapsed() >= self.policy.idle_after);
        if !idle {
            return false;
        }
        if let Some(held) = slot.take() {
            held.chat.close().await;
            self.note(|s| s.idle_closed += 1);
        }
        true
    }

    /// Retire a held conversation that has spent its budget or gone quiet, BEFORE it is used.
    ///
    /// The idle check is repeated here rather than left to the reaper because the reaper is
    /// optional: without it, this is the only thing standing between a summary and an agent whose
    /// context is an hour old.
    async fn retire_stale(&self, slot: &mut Option<Pooled>) {
        let Some(held) = slot.as_ref() else { return };
        let spent = held.served >= self.policy.max_per_socket;
        let idle = held.last_used.elapsed() >= self.policy.idle_after;
        if !spent && !idle {
            return;
        }
        if let Some(held) = slot.take() {
            held.chat.close().await;
            self.note(|s| {
                if spent {
                    s.recycled += 1;
                } else {
                    s.idle_closed += 1;
                }
            });
        }
    }

    /// One question, on a pooled conversation, opening one if there is none to reuse.
    async fn turn(&self, asked: &str) -> Result<(String, Timing), ChatError> {
        let mut slot = self.pool.lock().await;
        self.retire_stale(&mut slot).await;

        if slot.is_some() {
            let started = Instant::now();
            let attempt = {
                let held = slot.as_mut().expect("checked immediately above");
                held.chat.ask(asked, self.policy.deadline).await
            };
            match attempt {
                Ok(reply) => {
                    let timing = self
                        .finish(&mut slot, started.elapsed(), Duration::ZERO, true)
                        .await;
                    return Ok((reply, timing));
                }
                Err(error) => {
                    // A conversation that failed mid-turn is not reusable, whatever went wrong.
                    if let Some(dead) = slot.take() {
                        dead.chat.close().await;
                    }
                    // ONE fresh attempt, and only for a socket that was already open. A vendor is
                    // free to close an idle conversation from its end, and the first write is
                    // where we find out; failing the summary for that would make the pool a
                    // source of errors that the socket-per-summary design does not have. A fresh
                    // socket that fails is a real failure and is not retried, so this cannot
                    // become a loop.
                    self.note(|s| s.retried += 1);
                    tracing::warn!(
                        %error,
                        "the pooled agent conversation failed; opening a fresh one and asking \
                         once more"
                    );
                }
            }
        }

        let connect_started = Instant::now();
        let chat = self
            .chats
            .open_text_chat(&self.elevenlabs, self.policy.deadline)
            .await?;
        let connected_in = connect_started.elapsed();
        self.note(|s| s.conversations_opened += 1);
        *slot = Some(Pooled {
            chat,
            served: 0,
            last_used: Instant::now(),
        });

        let started = Instant::now();
        let attempt = {
            let held = slot.as_mut().expect("just opened");
            held.chat.ask(asked, self.policy.deadline).await
        };
        match attempt {
            Ok(reply) => {
                let timing = self
                    .finish(&mut slot, started.elapsed(), connected_in, false)
                    .await;
                Ok((reply, timing))
            }
            Err(error) => {
                if let Some(dead) = slot.take() {
                    dead.chat.close().await;
                }
                Err(error)
            }
        }
    }

    /// Book a successful turn, and recycle the conversation if that was its last.
    ///
    /// Recycling EAGERLY rather than on the next summary is deliberate: the conversation has
    /// nothing left to give and the vendor is billing for it, so the moment its budget is spent is
    /// the moment to hang up. Waiting for the next summary would hold a useless socket open for
    /// however long the reader takes to tap the next message — which, by the argument for the idle
    /// timeout, is most of the time.
    async fn finish(
        &self,
        slot: &mut Option<Pooled>,
        answered_in: Duration,
        connected_in: Duration,
        reused: bool,
    ) -> Timing {
        let held = slot.as_mut().expect("a turn just succeeded on it");
        held.served += 1;
        held.last_used = Instant::now();
        let socket_turn = held.served;
        let spent = held.served >= self.policy.max_per_socket;
        self.note(|s| s.summaries += 1);
        if spent {
            if let Some(done) = slot.take() {
                done.chat.close().await;
                self.note(|s| s.recycled += 1);
            }
        }
        Timing {
            connected_in,
            answered_in,
            socket_turn,
            reused,
        }
    }
}

/// What the agent is actually sent: the plain-text rule, the width, then the framed request.
///
/// The rule sits OUTSIDE the fence [`SummaryRequest::prompt`] builds, alongside the instruction it
/// extends. Everything written by another party stays inside it.
#[must_use]
pub fn instruction(request: &SummaryRequest<'_>) -> String {
    format!(
        "{PLAIN_TEXT_RULE}\nKeep the answer under {} characters.\n\n{}",
        request.target_chars,
        request.prompt()
    )
}

/// Flatten a model's answer into something a page can set as `textContent`.
///
/// The instruction is the primary mechanism and this is the backstop, so it is deliberately
/// conservative — it removes the markdown a model reaches for by reflex and nothing else:
///
/// * leading heading and list markers, per line, because a model asked for one sentence sometimes
///   answers with one bullet;
/// * paired `**` and `__`, which are unambiguously emphasis;
/// * everything [`crate::summary::condense`] already flattens — fenced code, links, and all
///   whitespace including the newlines that would otherwise arrive as a wall.
///
/// It does NOT strip single `*` or `_`, because those are ordinary characters in ordinary prose
/// and mangling "the * operator" to fix a bullet that was already stripped is a worse outcome than
/// the bullet.
///
/// Reusing [`crate::summary::condense`] rather than reimplementing the code-and-link flattening it
/// already does is deliberate: the digest line, the collapsed row preview and this summary all end
/// up being read aloud, and two ideas of what "speakable" means is one too many.
///
/// # Length
///
/// `requested_chars` is the length the model was ASKED for, and the answer is NOT cut there. It is
/// cut only past [`super::hard_cap_chars`] of it — 640 for the default 160 — at a word boundary,
/// ending "…". `#257 summary-truncation`: cutting at the requested length turned every answer that
/// ran one word long into a sentence that stopped mid-thought.
///
/// What is STORED is [`plain_uncut`]'s answer, and this is what serving it gives
/// ([`super::cut_for_serving`]); the two agree by construction. `#208 persistent-cache`.
#[must_use]
pub fn plain(reply: &str, requested_chars: usize) -> String {
    super::cut_for_serving(&plain_uncut(reply, requested_chars).0, requested_chars)
}

/// [`plain`]'s flattening, cut only at the stored ceiling ([`super::stored_ceiling_chars`], four
/// times the hard cap), and the length it was cut at when it ran past that.
/// `#208 persistent-cache`.
///
/// This is what is filed. The cap in force is applied when it is served, so a later cap change
/// needs no new answer unless this one was cut shorter than the new cap. Every backend's answer,
/// and a thread sentence too ([`super::threads::parse_thread_answer`]), passes through here, so
/// they all agree on where that is.
#[must_use]
pub fn plain_uncut(reply: &str, requested_chars: usize) -> (String, Option<u32>) {
    let unmarked: Vec<String> = reply.lines().map(strip_line_markers).collect();
    let joined = unmarked.join(" ").replace("**", "").replace("__", "");
    let ceiling = super::stored_ceiling_chars(requested_chars);
    let (text, cut) = crate::summary::condense_noting_cut(&joined, ceiling);
    (
        text,
        cut.then(|| u32::try_from(ceiling).unwrap_or(u32::MAX)),
    )
}

/// Drop a leading heading or list marker from one line.
fn strip_line_markers(line: &str) -> String {
    let trimmed = line.trim_start();
    let after_heading = trimmed.trim_start_matches('#');
    let trimmed = if after_heading.len() < trimmed.len() && after_heading.starts_with(' ') {
        after_heading.trim_start()
    } else {
        trimmed
    };
    for bullet in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            return rest.trim_start().to_owned();
        }
    }
    // `1. ` / `1) `, up to two digits: enough for a list, short of eating "2024 was the year".
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && digits.len() <= 2 {
        let rest = &trimmed[digits.len()..];
        for marker in [". ", ") "] {
            if let Some(rest) = rest.strip_prefix(marker) {
                return rest.trim_start().to_owned();
            }
        }
    }
    trimmed.to_owned()
}

/// Carry a conversation failure across into the summariser's own taxonomy.
///
/// All four modes survive the trip. They have four different fixes, and a summariser that
/// collapsed them would send the operator to the wrong one.
#[must_use]
pub fn summary_from_chat(error: ChatError) -> SummaryError {
    match error {
        ChatError::NotConfigured(what) => SummaryError::NotConfigured(what),
        ChatError::Refused(detail) => SummaryError::Refused(detail),
        ChatError::Transport(detail) => SummaryError::Transport(detail),
        ChatError::Shape(detail) => SummaryError::Shape(detail),
    }
}

#[async_trait]
impl Summarizer for AgentSummarizer {
    fn describe(&self) -> &'static str {
        "the ElevenLabs conversational agent, over a pooled text-only WebSocket"
    }

    fn unavailable_reason(&self) -> Option<String> {
        crate::elevenlabs::credentials(&self.inner.elevenlabs)
            .err()
            .map(|error| error.to_string())
    }

    fn backend(&self) -> &'static str {
        BACKEND
    }

    fn policy_input(&self) -> &str {
        &self.inner.policy_input
    }

    async fn summarize(&self, request: &SummaryRequest<'_>) -> Result<Summary, SummaryError> {
        let asked = instruction(request);
        let (reply, timing) = self.inner.turn(&asked).await.map_err(summary_from_chat)?;
        // Stored uncut, cut when served: see `plain_uncut`. `#208 persistent-cache`.
        let (summary, cut_at) = plain_uncut(&reply, request.target_chars);
        // Not an empty success. A blank line on the page is indistinguishable from a summary that
        // has not loaded, and `condense`'s "(no text)" placeholder is worse — it is right for a
        // digest of a message with no words in it, and here it would be this server asserting that
        // a paid-for answer said nothing when what happened is that it failed.
        if summary.trim().is_empty() || summary == crate::summary::NO_TEXT {
            return Err(SummaryError::Shape(
                "the agent answered with no text at all".to_owned(),
            ));
        }
        // THE MEASUREMENT. The owner's open question is how a round trip to a hosted
        // conversational agent compares with a full-size model, and the answer is a number nobody
        // has. So the first real run measures itself: `connect_ms` is what the pool saves, and
        // `answer_ms` is the comparison. INFO, not DEBUG, because a number that has to be turned
        // on is a number nobody will have when they want it. No message text is logged.
        tracing::info!(
            backend = BACKEND,
            connect_ms = timing.connected_in.as_millis(),
            answer_ms = timing.answered_in.as_millis(),
            reused_socket = timing.reused,
            socket_turn = timing.socket_turn,
            summary_chars = summary.chars().count(),
            "summarised one message with the ElevenLabs agent"
        );
        Ok(Summary {
            text: summary,
            cut_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;
    use crate::elevenlabs::fake::{FakeElevenLabs, KNOWN_AGENT_ID, VALID_API_KEY};
    use crate::model::{Message, MessageId, UserId};

    fn elevenlabs(agent: Option<&str>, key: Option<&str>) -> ElevenLabsConfig {
        ElevenLabsConfig {
            agent_id: agent.map(str::to_owned),
            api_key: key.map(Secret::new),
            api_base: crate::config::DEFAULT_ELEVENLABS_API_BASE.to_owned(),
            voice_id: None,
        }
    }

    fn wired() -> ElevenLabsConfig {
        elevenlabs(Some(KNOWN_AGENT_ID), Some(VALID_API_KEY))
    }

    fn message(content: &str) -> Message {
        Message {
            thread: None,
            id: MessageId("1".to_owned()),
            channel_id: crate::model::ChannelId("100".to_owned()),
            author: "codex-eng".to_owned(),
            author_id: UserId("2".to_owned()),
            author_is_bot: false,
            content: content.to_owned(),
            spoken_content: String::new(),
            content_html: String::new(),
            noise: false,
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            spoken_time: "midnight".to_owned(),
            reply_to: None,
            reactions: None,
            heading: None,
        }
    }

    fn summarizer(vendor: &Arc<FakeElevenLabs>, policy: PoolPolicy) -> AgentSummarizer {
        AgentSummarizer::new(
            Arc::clone(vendor) as Arc<dyn TextChatProvider>,
            wired(),
            policy,
        )
    }

    async fn summarise(under: &AgentSummarizer, text: &str) -> Result<String, SummaryError> {
        let target = message(text);
        let request = SummaryRequest::new(&target, &[], 160);
        under.summarize(&request).await.map(|summary| summary.text)
    }

    #[tokio::test]
    async fn what_the_agent_is_sent_carries_the_fence_and_the_plain_text_rule() {
        // Both halves in one assertion because both are structural: the rule is the owner's
        // explicit requirement, and the fence is the untrusted-input boundary that
        // `SummaryRequest` exists to make unforgettable.
        let vendor = Arc::new(FakeElevenLabs::new());
        let under = summarizer(&vendor, PoolPolicy::default());
        summarise(&under, "a wall of text").await.expect("answers");

        let asked = &vendor.chats()[0].asked[0];
        assert!(
            asked.contains(PLAIN_TEXT_RULE),
            "the plain-text rule never reached the agent: {asked}"
        );
        assert!(asked.contains(PROMPT), "{asked}");
        assert!(
            asked.contains("a wall of text"),
            "the message itself was not sent: {asked}"
        );
        assert!(
            asked.contains(crate::untrusted::FENCE),
            "channel text reached a model outside the fence: {asked}"
        );
    }

    #[tokio::test]
    async fn eight_summaries_share_one_conversation_and_the_ninth_starts_a_new_one() {
        // The whole design, as a count. A backend with no pool would open nine; one that never
        // recycled would open one and let the agent answer the ninth while looking at eight
        // summaries of other people's messages.
        let vendor = Arc::new(FakeElevenLabs::new());
        let under = summarizer(&vendor, PoolPolicy::default());
        for n in 0..DEFAULT_MAX_PER_SOCKET {
            summarise(&under, &format!("message {n}"))
                .await
                .expect("answers");
        }
        assert_eq!(
            vendor.chats().len(),
            1,
            "the budget is {DEFAULT_MAX_PER_SOCKET} summaries per conversation and it opened {} \
             for {DEFAULT_MAX_PER_SOCKET}",
            vendor.chats().len()
        );
        assert_eq!(under.stats().conversations_opened, 1);
        assert_eq!(under.stats().recycled, 1, "the spent socket must be let go");

        summarise(&under, "one more").await.expect("answers");
        assert_eq!(
            vendor.chats().len(),
            2,
            "the ninth summary must get a clean context, not a ninth turn"
        );
        assert_eq!(vendor.chats()[0].asked.len(), DEFAULT_MAX_PER_SOCKET);
        assert_eq!(vendor.chats()[1].asked.len(), 1);
    }

    #[tokio::test]
    async fn a_shallower_budget_really_is_shallower() {
        // The control for the test above: without this, "it opened one conversation" is also what
        // a backend that ignores the setting entirely would do.
        let vendor = Arc::new(FakeElevenLabs::new());
        let under = summarizer(
            &vendor,
            PoolPolicy {
                max_per_socket: 2,
                ..PoolPolicy::default()
            },
        );
        for n in 0..6 {
            summarise(&under, &format!("message {n}"))
                .await
                .expect("answers");
        }
        assert_eq!(vendor.chats().len(), 3);
        for chat in vendor.chats() {
            assert_eq!(chat.asked.len(), 2);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_conversation_nobody_is_having_is_closed_rather_than_billed() {
        let vendor = Arc::new(FakeElevenLabs::new());
        let under = summarizer(&vendor, PoolPolicy::default());
        summarise(&under, "the first one").await.expect("answers");
        assert!(!vendor.chats()[0].closed, "still in use");

        assert!(
            !under.close_if_idle().await,
            "a conversation used a moment ago is not idle"
        );
        tokio::time::advance(Duration::from_secs(DEFAULT_SOCKET_IDLE_SECONDS + 1)).await;
        assert!(under.close_if_idle().await, "this one is");
        assert!(
            vendor.chats()[0].closed,
            "an idle conversation must be CLOSED, not merely forgotten: the vendor bills \
             connection time either way"
        );
        assert_eq!(under.stats().idle_closed, 1);

        // ...and the next summary gets a fresh one rather than a dangling handle.
        summarise(&under, "much later").await.expect("answers");
        assert_eq!(vendor.chats().len(), 2);
    }

    #[test]
    fn missing_credentials_are_reported_before_anything_is_asked() {
        let vendor = Arc::new(FakeElevenLabs::new());
        let wired_up = summarizer(&vendor, PoolPolicy::default());
        assert_eq!(wired_up.unavailable_reason(), None);
        for (agent, key) in [
            (None, Some(VALID_API_KEY)),
            (Some(KNOWN_AGENT_ID), None),
            (None, None),
        ] {
            let unwired = AgentSummarizer::new(
                Arc::clone(&vendor) as Arc<dyn TextChatProvider>,
                elevenlabs(agent, key),
                PoolPolicy::default(),
            );
            let reason = unwired
                .unavailable_reason()
                .expect("an unconfigured agent is unavailable");
            assert!(
                reason.contains("elevenlabs"),
                "the reason does not name the setting: {reason}"
            );
        }
        assert!(
            vendor.chats().is_empty(),
            "checking availability talked to the vendor"
        );
    }

    #[tokio::test]
    async fn a_vendor_that_says_no_is_reported_as_a_refusal_and_not_as_a_dead_network() {
        let vendor = Arc::new(FakeElevenLabs::new());
        let under = AgentSummarizer::new(
            Arc::clone(&vendor) as Arc<dyn TextChatProvider>,
            elevenlabs(
                Some(KNOWN_AGENT_ID),
                Some("xi-a-key-this-account-does-not-have"),
            ),
            PoolPolicy::default(),
        );
        let error = summarise(&under, "a wall of text")
            .await
            .expect_err("a rejected key must not produce a summary");
        assert!(
            matches!(error, SummaryError::Refused(_)),
            "a rejection is not unreachability: {error:?}"
        );
        assert_eq!(error.code(), "summarizer_refused");
        assert!(
            !error
                .to_string()
                .contains("xi-a-key-this-account-does-not-have"),
            "the key leaked out of a vendor refusal: {error}"
        );
    }

    #[tokio::test]
    async fn an_unconfigured_server_names_the_setting_before_it_dials_anything() {
        let vendor = Arc::new(FakeElevenLabs::new());
        let under = AgentSummarizer::new(
            Arc::clone(&vendor) as Arc<dyn TextChatProvider>,
            elevenlabs(Some(KNOWN_AGENT_ID), None),
            PoolPolicy::default(),
        );
        let error = summarise(&under, "a wall of text")
            .await
            .expect_err("must refuse");
        assert!(
            matches!(&error, SummaryError::NotConfigured(f) if *f == "elevenlabs.api_key"),
            "unexpected error: {error:?}"
        );
        assert!(
            vendor.chats().is_empty(),
            "nothing should be opened when there is no key to open it with"
        );
    }

    #[tokio::test]
    async fn a_pooled_conversation_the_vendor_dropped_costs_a_retry_and_not_a_summary() {
        // The failure a pool has that a socket-per-summary design does not: the vendor closed an
        // idle conversation from its end and we only find out on the next write.
        let vendor = Arc::new(FakeElevenLabs::new());
        let under = summarizer(&vendor, PoolPolicy::default());
        summarise(&under, "the first one").await.expect("answers");

        vendor.fail_next_turn("the conversation was closed at the far end");
        let summary = summarise(&under, "the second one")
            .await
            .expect("a dropped pooled socket must not fail a summary");
        assert!(!summary.is_empty());
        assert_eq!(under.stats().retried, 1);
        assert_eq!(
            vendor.chats().len(),
            2,
            "the retry must be on a FRESH conversation"
        );
    }

    #[tokio::test]
    async fn a_fresh_conversation_that_fails_is_not_retried_forever() {
        // The other half of the retry rule. Without it a vendor that refuses every turn would be
        // asked in a loop, on this server's money.
        let vendor = Arc::new(FakeElevenLabs::new());
        let under = summarizer(&vendor, PoolPolicy::default());
        vendor.fail_next_turn("no");
        let error = summarise(&under, "the first one")
            .await
            .expect_err("must fail");
        assert!(matches!(error, SummaryError::Transport(_)), "{error:?}");
        assert_eq!(under.stats().retried, 0);
        assert_eq!(vendor.chats().len(), 1, "exactly one attempt");
    }

    #[tokio::test]
    async fn an_answer_with_no_words_in_it_is_a_failure_rather_than_a_blank_line() {
        let vendor = Arc::new(FakeElevenLabs::new());
        vendor.answer_with("   ");
        let under = summarizer(&vendor, PoolPolicy::default());
        let error = summarise(&under, "a wall of text")
            .await
            .expect_err("must refuse");
        assert!(matches!(error, SummaryError::Shape(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_markdown_answer_is_flattened_into_something_a_page_can_set_as_textcontent() {
        // The instruction asks for plain text; a model is free to decline. This is the backstop,
        // and the owner's requirement is about what reaches the page, not about what was asked.
        let vendor = Arc::new(FakeElevenLabs::new());
        vendor.answer_with("## Summary\n\n- **The runner** stalled\n- see ```code``` for why");
        let under = summarizer(&vendor, PoolPolicy::default());
        let summary = summarise(&under, "a wall of text").await.expect("answers");
        for forbidden in ["**", "##", "\n", "- ", "```"] {
            assert!(
                !summary.contains(forbidden),
                "{forbidden:?} survived into a summary rendered with textContent: {summary:?}"
            );
        }
        assert!(
            summary.contains("The runner stalled"),
            "the words themselves must survive: {summary:?}"
        );
    }

    #[test]
    fn flattening_leaves_ordinary_prose_alone() {
        // The other side of the backstop: over-stripping mangles text nobody asked it to touch.
        assert_eq!(
            plain("the * operator binds tighter than +", 160),
            "the * operator binds tighter than +"
        );
        assert_eq!(
            plain("2026 was the year it shipped", 160),
            "2026 was the year it shipped"
        );
        assert_eq!(plain("a plain sentence", 160), "a plain sentence");
    }

    /// A sentence of `chars` characters, of ordinary words, ending in a full stop.
    fn sentence_of(chars: usize) -> String {
        let mut out = String::from("The team");
        let words = [
            "reviewed", "storage", "use", "in", "the", "build", "cache", "and", "found", "old",
            "records",
        ];
        for word in words.iter().cycle() {
            if out.chars().count() + 1 + word.chars().count() + 1 > chars {
                break;
            }
            out.push(' ');
            out.push_str(word);
        }
        while out.chars().count() + 1 < chars {
            out.push('s');
        }
        out.push('.');
        out
    }

    #[test]
    fn an_answer_half_as_long_again_as_asked_for_is_kept_whole() {
        // `#257 summary-truncation`. Asked for 160, a model answered 240: an ordinary overshoot,
        // and it used to reach the page cut at 160 with "…".
        let answer = sentence_of(240);
        assert_eq!(answer.chars().count(), 240);
        assert_eq!(plain(&answer, 160), answer);
        let twice = sentence_of(320);
        assert_eq!(
            plain(&twice, 160),
            twice,
            "even twice the length is kept whole"
        );
        let at_the_cap = sentence_of(super::super::hard_cap_chars(160));
        assert_eq!(
            plain(&at_the_cap, 160),
            at_the_cap,
            "the cap itself is kept"
        );
    }

    #[test]
    fn an_answer_past_the_hard_cap_is_cut_at_a_word_boundary_with_an_ellipsis() {
        let cap = super::super::hard_cap_chars(160);
        let wall = "alpha bravo charlie delta echo foxtrot ".repeat(40);
        assert!(wall.chars().count() > cap);
        let cut = plain(&wall, 160);
        assert!(
            cut.chars().count() <= cap,
            "over the cap: {}",
            cut.chars().count()
        );
        assert!(
            cut.chars().count() > cap - 12,
            "cut far short of the cap: {}",
            cut.chars().count()
        );
        let kept = cut
            .strip_suffix('…')
            .expect("a cut answer ends with an ellipsis");
        assert!(
            wall.starts_with(&format!("{kept} ")),
            "the cut split a word: {kept:?}"
        );
    }

    #[test]
    fn what_is_stored_is_cut_only_past_the_ceiling_and_says_where() {
        // `#208 persistent-cache`. The stored answer runs to four times the hard cap, so a later
        // cap change can be served from it; `cut_at` is set only when it really was cut.
        let cap = super::super::hard_cap_chars(160);
        let ceiling = super::super::stored_ceiling_chars(160);
        assert_eq!(ceiling, 4 * cap);
        let past_the_cap = sentence_of(cap + 200);
        assert_eq!(
            plain_uncut(&past_the_cap, 160),
            (past_the_cap.clone(), None),
            "an answer past the cap but within the ceiling is stored whole"
        );
        let at_the_ceiling = sentence_of(ceiling);
        assert_eq!(plain_uncut(&at_the_ceiling, 160), (at_the_ceiling, None));
        let wall = "alpha bravo charlie delta echo foxtrot ".repeat(120);
        let (stored, cut_at) = plain_uncut(&wall, 160);
        assert_eq!(cut_at, Some(u32::try_from(ceiling).expect("small")));
        assert!(
            stored.chars().count() <= ceiling,
            "{}",
            stored.chars().count()
        );
        assert!(stored.ends_with('…'), "{stored:?}");
        // ...and serving what was stored is what the old single cut gave.
        assert_eq!(
            super::super::cut_for_serving(&stored, 160),
            plain(&wall, 160)
        );
        assert_eq!(
            super::super::cut_for_serving(&past_the_cap, 160),
            plain(&past_the_cap, 160)
        );
    }

    #[test]
    fn an_answer_with_no_words_is_still_the_placeholder_and_never_counts_as_cut() {
        assert_eq!(
            plain_uncut("``\n  ``", 160),
            (crate::summary::NO_TEXT.to_owned(), None)
        );
        assert_eq!(plain("   ", 160), crate::summary::NO_TEXT);
    }

    #[tokio::test]
    async fn the_agent_backend_keeps_an_overlong_answer_whole_too() {
        // The two backends agree: the ElevenLabs agent's answer goes through `plain` as the
        // Messages API's does, so a reply one and a half times the requested length survives.
        let vendor = Arc::new(FakeElevenLabs::new());
        let answer = sentence_of(240);
        vendor.answer_with(&answer);
        let under = summarizer(&vendor, PoolPolicy::default());
        let summary = summarise(&under, "a wall of text").await.expect("answers");
        assert_eq!(summary, answer);
    }

    #[test]
    fn the_agents_identity_is_part_of_the_cache_key() {
        // Pointing the deployment at a different agent changes the model, the system prompt and
        // the voice of every summary. Serving the old agent's answers afterwards is the
        // silent-stale-summary failure with a different cause.
        let vendor = Arc::new(FakeElevenLabs::new());
        let one = AgentSummarizer::new(
            Arc::clone(&vendor) as Arc<dyn TextChatProvider>,
            elevenlabs(Some("agent_one"), Some(VALID_API_KEY)),
            PoolPolicy::default(),
        );
        let two = AgentSummarizer::new(
            Arc::clone(&vendor) as Arc<dyn TextChatProvider>,
            elevenlabs(Some("agent_two"), Some(VALID_API_KEY)),
            PoolPolicy::default(),
        );
        let config = SummariesConfig::default();
        assert_ne!(
            super::super::policy_version_for(&config, &one),
            super::super::policy_version_for(&config, &two)
        );
        // ...and the identity is in `policy_input`, not merely in the slug. `policy_version` folds
        // the bare `PROMPT`, so a summariser whose extra policy input were dropped would land on
        // exactly this string — which is the same summary being served for a different agent.
        assert_ne!(
            super::super::policy_version_for(&config, &one),
            super::super::policy_version_of(PROMPT, &config, BACKEND),
            "the plain-text rule and the agent id are not reaching the cache key at all"
        );
    }
}
