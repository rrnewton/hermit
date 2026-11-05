//! A voice agent may PROPOSE a post; only the person it speaks for may send it.
//!
//! `#34 voice-chat-write-confirm`. `post_reply` has always been marked as needing approval, and its
//! description has always told the model to read the text back and get a spoken yes. Nothing
//! outside the model enforced either. Over MCP a write-scope caller's `post_reply` posted on the
//! spot, so whether the owner's bot spoke was decided by the model alone — which is why no voice
//! bridge was trusted with the write credential.
//!
//! This module is the enforcement, and it lives in the application so every provider inherits it:
//!
//! 1. **Proposing does not post.** [`PostGate::propose`] records the exact text, the channel and
//!    the reply target, and hands back a short-lived, single-use handle. The model is told plainly
//!    that nothing was sent.
//! 2. **Committing needs something the model cannot produce.** The commit is an application route
//!    (`POST /api/v1/post-proposals/commit`), never a tool, so no model can call it: the voice page
//!    calls it when the owner taps Send, or a bridge calls it after attributing a separate, final
//!    user turn to the speaker. A tool that reached it would undo the whole gate, which is why the
//!    manifest test forbids one.
//! 3. **A handle is bound to what was proposed.** The commit restates the channel, the text and the
//!    reply target, and any difference refuses it. So does expiry, reuse, cancellation, or a newer
//!    proposal, and EVERY attempt spends the handle — a refused commit cannot be retried into a
//!    success by guessing what changed.
//! 4. **Nothing said is logged.** [`crate::access::post_gate`] records the proposal's serial, the
//!    channel, the text's length and the outcome, never the words.
//!
//! # Unless the owner turns it off
//!
//! `#264 voice-send-approval`. All of the above is what `post_reply` does while "Ask me before the
//! voice agent sends" is on, which is the shipped default. The owner may turn it off in Settings,
//! and then `post_reply` posts at once and nothing here is involved; see [`crate::send_approval`].
//! The switch is his alone — a write-scope route Settings calls, never a tool — so the rule above
//! still holds in the form that matters: nothing a model can call decides whether it is asked.
//!
//! # Why one pending proposal, not a queue
//!
//! A voice conversation proposes one message, reads it back, and waits. A second proposal means
//! the draft changed — "no, say it like this" — and the old one is exactly what must not be sent
//! by a tap that arrives late. So a new proposal SUPERSEDES the pending one rather than queueing
//! beside it, and the screen only ever offers the draft the conversation is currently on.
//!
//! # Why the handle is a capability anyway
//!
//! Every route here already needs the write-scope token, so the handle is not what keeps a
//! stranger out. It is what binds a tap to ONE proposal: 128 bits from the system CSPRNG, carried
//! in request bodies rather than paths so the access log's path field never holds it.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// How long a proposal can be confirmed.
///
/// Long enough to hear the read-back and tap Send, or answer it aloud; short enough that a
/// forgotten draft cannot be sent by a tap minutes later, after the conversation has moved on.
pub const PROPOSAL_TTL: Duration = Duration::from_secs(120);

/// How many spent handles are remembered, so a second commit is refused as `proposal_used`
/// rather than as a handle that never existed. Bounded because nothing else bounds it.
const SPENT_MEMORY: usize = 64;

/// How long after it ends a post the voice agent sent at once keeps the same words to the same
/// place from being posted again. `#264 voice-send-approval`.
///
/// A hosted tool call can time out, or be retried by its transport, after the post has landed, and
/// an owner told "not sent" says "try again". A commit cannot be repeated — it spends a single-use
/// handle — but a direct send has no handle, so this is what stands in for one. A minute covers a
/// retry and an owner's "try again"; it is short enough that saying the same thing again on
/// purpose, later, is not refused.
pub const DIRECT_REPEAT_WINDOW: Duration = Duration::from_secs(60);

/// How many direct sends are remembered at once. Bounded because nothing else bounds it.
const DIRECT_MEMORY: usize = 16;

/// What a proposal is bound to. A commit must restate all three exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The channel id, already resolved from any name or alias the model used.
    pub channel_id: String,
    /// The exact text that would be posted.
    pub text: String,
    /// The message this answers, if any.
    pub reply_to: Option<String>,
}

/// One proposal waiting for confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    /// A small public number for correlating log lines and screens. Not a capability.
    pub serial: u64,
    /// The single-use capability a commit must present.
    pub handle: String,
    /// What was proposed.
    pub binding: Binding,
    /// When it was made.
    pub created: Instant,
    /// How long it lives.
    pub ttl: Duration,
}

impl Proposal {
    /// Whole milliseconds left before it expires, as of `now`.
    #[must_use]
    pub fn expires_in_ms(&self, now: Instant) -> u64 {
        let left = self
            .ttl
            .saturating_sub(now.saturating_duration_since(self.created));
        u64::try_from(left.as_millis()).unwrap_or(u64::MAX)
    }

    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created) >= self.ttl
    }
}

/// Why a commit or a cancel was refused. Each is a machine code a client branches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No proposal was ever made with this handle, or it is too old to be remembered.
    Unknown,
    /// The proposal outlived [`PROPOSAL_TTL`].
    Expired,
    /// The handle was already committed, refused, or cancelled.
    Used,
    /// A newer proposal replaced it before it was confirmed.
    Superseded,
    /// The commit restated a different channel, text, or reply target.
    Mismatch,
}

impl Refusal {
    /// The code a response and a log line carry.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "proposal_unknown",
            Self::Expired => "proposal_expired",
            Self::Used => "proposal_used",
            Self::Superseded => "proposal_superseded",
            Self::Mismatch => "proposal_mismatch",
        }
    }

    /// One sentence for the person who tapped.
    #[must_use]
    pub fn sentence(self) -> &'static str {
        match self {
            Self::Unknown => "this server has no such proposal, so nothing was sent",
            Self::Expired => "the proposal expired before it was confirmed, so nothing was sent",
            Self::Used => {
                "that proposal was already confirmed or cancelled; it cannot be sent again"
            }
            Self::Superseded => "a newer draft replaced this one, so nothing was sent",
            Self::Mismatch => {
                "what was confirmed differs from what was proposed, so nothing was sent"
            }
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    pending: Option<Proposal>,
    /// Recently spent handles and why, newest last.
    spent: VecDeque<(String, Refusal)>,
    next_serial: u64,
    /// Direct sends on their way, or ended within [`DIRECT_REPEAT_WINDOW`] having posted something
    /// or not knowing whether they did, oldest first.
    direct: VecDeque<DirectSend>,
}

/// What became of a post the voice agent sent at once, as far as a repeat of it needs to know.
/// `#264 voice-send-approval`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectOutcome {
    /// Still on its way to the chat service.
    InFlight,
    /// Posted, all of it.
    Sent,
    /// Some of its parts were posted, and the rest were not.
    Partly,
    /// Some of its parts were posted, and whether the rest were could not be confirmed: the answer
    /// for the part that failed was lost, so it may be in the channel too.
    PartlyUnconfirmed,
    /// Nobody knows: the chat service's answer was lost, or the request was abandoned while the
    /// post was on its way.
    Unconfirmed,
}

impl DirectOutcome {
    /// The word a log line carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight",
            Self::Sent => "sent",
            Self::Partly => "partly",
            Self::PartlyUnconfirmed => "partly_unconfirmed",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}

/// An earlier direct send of the same words to the same place, which a new one is not posted over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repeat {
    /// The earlier send's number.
    pub serial: u64,
    /// What became of it.
    pub outcome: DirectOutcome,
    /// How long ago it began.
    pub age: Duration,
    /// How much longer it keeps the same words from being posted again: the whole window while it
    /// is still on its way.
    pub left: Duration,
}

#[derive(Debug)]
struct DirectSend {
    serial: u64,
    binding: Binding,
    outcome: DirectOutcome,
    started: Instant,
    /// When it ended; `None` while it is on its way.
    ended: Option<Instant>,
}

impl DirectSend {
    fn lapsed(&self, now: Instant) -> bool {
        self.ended
            .is_some_and(|ended| now.saturating_duration_since(ended) >= DIRECT_REPEAT_WINDOW)
    }
}

/// A direct send on its way, from [`PostGate::begin_direct`]. Finish it with what happened.
///
/// Dropped unfinished — the request abandoned while the post was on the wire, as a client that
/// hangs up mid-call does — it is remembered as [`DirectOutcome::Unconfirmed`], because that is
/// exactly what it is.
#[derive(Debug)]
#[must_use = "a direct send that is never finished is remembered as unconfirmed"]
pub struct DirectTicket<'a> {
    gate: &'a PostGate,
    serial: u64,
    finished: bool,
}

impl DirectTicket<'_> {
    /// The number this send's log lines carry.
    #[must_use]
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// Record what happened: `Some` when something was posted or may have been, `None` when
    /// certainly nothing was — which forgets the send, so the same words may go again at once.
    pub fn finish(mut self, outcome: Option<DirectOutcome>) {
        self.finished = true;
        self.gate.end_direct(self.serial, outcome);
    }
}

impl Drop for DirectTicket<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.gate
                .end_direct(self.serial, Some(DirectOutcome::Unconfirmed));
        }
    }
}

impl Inner {
    fn spend(&mut self, handle: String, why: Refusal) {
        if self.spent.len() >= SPENT_MEMORY {
            self.spent.pop_front();
        }
        self.spent.push_back((handle, why));
    }

    /// Move an expired pending proposal into the spent list, and say whether one was.
    fn expire(&mut self, now: Instant) -> Option<Proposal> {
        if self.pending.as_ref().is_some_and(|p| p.expired(now)) {
            let lapsed = self.pending.take()?;
            self.spend(lapsed.handle.clone(), Refusal::Expired);
            return Some(lapsed);
        }
        None
    }

    fn refusal_for(&self, handle: &str) -> Refusal {
        self.spent
            .iter()
            .rev()
            .find(|(spent, _)| spent == handle)
            .map_or(Refusal::Unknown, |(_, why)| *why)
    }
}

/// The server's one pending post proposal, and the rules for spending it.
#[derive(Debug)]
pub struct PostGate {
    inner: Mutex<Inner>,
    ttl: Duration,
    /// Bumped on every change, so a long poll wakes without polling.
    changed: watch::Sender<u64>,
}

impl Default for PostGate {
    fn default() -> Self {
        Self::new()
    }
}

/// What happened to a proposal that was replaced or lapsed on the way past, for the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Displaced {
    /// The proposal that stopped being confirmable.
    pub proposal: Proposal,
    /// Why.
    pub why: Refusal,
}

impl PostGate {
    /// A gate whose proposals live for [`PROPOSAL_TTL`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl(PROPOSAL_TTL)
    }

    /// A gate with a different lifetime. For tests, which should not wait two minutes.
    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            inner: Mutex::new(Inner::default()),
            ttl,
            changed,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a panic while holding it; the state inside is still a valid
        // Option and list, and refusing every post forever would be worse than carrying on.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Record a proposal, replacing any pending one, and return it with whatever it displaced.
    pub fn propose(&self, binding: Binding) -> (Proposal, Option<Displaced>) {
        let now = Instant::now();
        let mut inner = self.lock();
        let displaced = match inner.expire(now) {
            Some(lapsed) => Some(Displaced {
                proposal: lapsed,
                why: Refusal::Expired,
            }),
            None => inner.pending.take().map(|old| Displaced {
                proposal: old,
                why: Refusal::Superseded,
            }),
        };
        if let Some(Displaced { proposal, why }) = &displaced {
            if *why == Refusal::Superseded {
                inner.spend(proposal.handle.clone(), Refusal::Superseded);
            }
        }
        inner.next_serial += 1;
        let proposal = Proposal {
            serial: inner.next_serial,
            handle: fresh_handle(),
            binding,
            created: now,
            ttl: self.ttl,
        };
        inner.pending = Some(proposal.clone());
        drop(inner);
        self.notify();
        (proposal, displaced)
    }

    /// Number a post the voice agent is about to send at once, and remember it as on its way.
    /// `#264 voice-send-approval`.
    ///
    /// Unless the same words to the same place — the whole [`Binding`] — are already on their way,
    /// or ended within [`DIRECT_REPEAT_WINDOW`] posted, partly posted or unconfirmed. Then nothing
    /// is numbered or remembered, and the earlier send is returned for the caller to answer with
    /// instead of posting. A send that certainly posted nothing is forgotten when it ends, so it
    /// never stands in the way of trying again.
    ///
    /// The number comes from the proposals' counter, so the access log's `proposal=` numbers stay
    /// one sequence; nothing pending is touched and no watcher is woken, because nothing a watcher
    /// shows changed.
    ///
    /// # Errors
    ///
    /// The earlier send, as a [`Repeat`].
    pub fn begin_direct(&self, binding: Binding) -> Result<DirectTicket<'_>, Repeat> {
        let now = Instant::now();
        let mut inner = self.lock();
        inner.direct.retain(|send| !send.lapsed(now));
        if let Some(earlier) = inner.direct.iter().find(|send| send.binding == binding) {
            let left = earlier.ended.map_or(DIRECT_REPEAT_WINDOW, |ended| {
                DIRECT_REPEAT_WINDOW.saturating_sub(now.saturating_duration_since(ended))
            });
            return Err(Repeat {
                serial: earlier.serial,
                outcome: earlier.outcome,
                age: now.saturating_duration_since(earlier.started),
                left,
            });
        }
        if inner.direct.len() >= DIRECT_MEMORY {
            inner.direct.pop_front();
        }
        inner.next_serial += 1;
        let serial = inner.next_serial;
        inner.direct.push_back(DirectSend {
            serial,
            binding,
            outcome: DirectOutcome::InFlight,
            started: now,
            ended: None,
        });
        drop(inner);
        Ok(DirectTicket {
            gate: self,
            serial,
            finished: false,
        })
    }

    fn end_direct(&self, serial: u64, outcome: Option<DirectOutcome>) {
        let now = Instant::now();
        let mut inner = self.lock();
        let Some(at) = inner.direct.iter().position(|send| send.serial == serial) else {
            return;
        };
        match outcome {
            None => {
                inner.direct.remove(at);
            }
            Some(outcome) => {
                if let Some(send) = inner.direct.get_mut(at) {
                    send.outcome = outcome;
                    send.ended = Some(now);
                }
            }
        }
    }

    /// The pending proposal, if one is still confirmable.
    #[must_use]
    pub fn pending(&self) -> Option<Proposal> {
        let mut inner = self.lock();
        if inner.expire(Instant::now()).is_some() {
            drop(inner);
            self.notify();
            return None;
        }
        inner.pending.clone()
    }

    /// Spend `handle` for a commit. Succeeds only when it names the pending, unexpired proposal
    /// AND `restated` matches its binding exactly. Every outcome except [`Refusal::Unknown`] and an
    /// already-spent handle consumes the proposal.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] saying why nothing may be posted; the proposal, when there was one, so the
    /// log line can name its serial and channel. Boxed, because the refusal is the common case
    /// and should not carry a whole proposal's width on the stack.
    pub fn take(
        &self,
        handle: &str,
        restated: &Binding,
    ) -> Result<Proposal, (Refusal, Option<Box<Proposal>>)> {
        let now = Instant::now();
        let mut inner = self.lock();
        let lapsed = inner.expire(now);
        let names_pending = inner.pending.as_ref().is_some_and(|p| p.handle == handle);
        if !names_pending {
            let why = inner.refusal_for(handle);
            drop(inner);
            // Only an expiry noticed on the way changed anything a watcher could see; a stranger's
            // handle must not so much as wake one.
            if lapsed.is_some() {
                self.notify();
            }
            return Err((why, lapsed.filter(|p| p.handle == handle).map(Box::new)));
        }
        let Some(proposal) = inner.pending.take() else {
            return Err((Refusal::Unknown, None));
        };
        if proposal.binding != *restated {
            inner.spend(proposal.handle.clone(), Refusal::Used);
            drop(inner);
            self.notify();
            return Err((Refusal::Mismatch, Some(Box::new(proposal))));
        }
        inner.spend(proposal.handle.clone(), Refusal::Used);
        drop(inner);
        self.notify();
        Ok(proposal)
    }

    /// Withdraw the pending proposal named by `handle`.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] saying why there was nothing to cancel.
    pub fn cancel(&self, handle: &str) -> Result<Proposal, Refusal> {
        let now = Instant::now();
        let mut inner = self.lock();
        let lapsed = inner.expire(now);
        if inner.pending.as_ref().is_none_or(|p| p.handle != handle) {
            let why = inner.refusal_for(handle);
            drop(inner);
            if lapsed.is_some() {
                self.notify();
            }
            return Err(why);
        }
        let proposal = inner.pending.take().ok_or(Refusal::Unknown)?;
        inner.spend(proposal.handle.clone(), Refusal::Used);
        drop(inner);
        self.notify();
        Ok(proposal)
    }

    /// Wait until the pending proposal's serial differs from `seen`, or `wait` passes, and return
    /// whatever is pending then. `seen` of `None` means "nothing on screen yet".
    pub async fn wait_for_change(&self, seen: Option<u64>, wait: Duration) -> Option<Proposal> {
        let mut changes = self.changed.subscribe();
        let deadline = Instant::now() + wait;
        loop {
            let current = self.pending();
            if current.as_ref().map(|p| p.serial) != seen {
                return current;
            }
            // Wake for a change, for the pending proposal's own expiry, or for the deadline —
            // whichever comes first. Expiry changes the answer without anybody calling in.
            let expiry = current
                .as_ref()
                .map_or(deadline, |p| (p.created + p.ttl).min(deadline));
            let woke = tokio::time::timeout_at(expiry, changes.changed()).await;
            if Instant::now() >= deadline {
                return self.pending();
            }
            if matches!(woke, Ok(Err(_))) {
                // The sender lives as long as the gate, so this is unreachable; returning is the
                // safe answer if it ever is not.
                return self.pending();
            }
        }
    }
}

fn fresh_handle() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; 16];
    // Same rule as a speech ticket: without OS randomness the only alternative is a guessable
    // handle, and that is not a condition to paper over.
    getrandom::fill(&mut bytes).expect("the system CSPRNG must be available to mint a handle");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(text: &str) -> Binding {
        Binding {
            channel_id: "111".to_owned(),
            text: text.to_owned(),
            reply_to: None,
        }
    }

    #[tokio::test]
    async fn a_matching_commit_spends_the_proposal_exactly_once() {
        let gate = PostGate::new();
        let (proposal, displaced) = gate.propose(binding("ship it"));
        assert!(displaced.is_none());
        assert_eq!(
            gate.take(&proposal.handle, &binding("ship it")),
            Ok(proposal.clone())
        );
        assert_eq!(
            gate.take(&proposal.handle, &binding("ship it"))
                .map_err(|e| e.0),
            Err(Refusal::Used)
        );
        assert!(gate.pending().is_none());
    }

    #[tokio::test]
    async fn any_difference_in_the_restated_binding_refuses_and_spends_it() {
        for restated in [
            binding("ship it!"),
            Binding {
                channel_id: "222".to_owned(),
                ..binding("ship it")
            },
            Binding {
                reply_to: Some("9".to_owned()),
                ..binding("ship it")
            },
        ] {
            let gate = PostGate::new();
            let (proposal, _) = gate.propose(binding("ship it"));
            assert_eq!(
                gate.take(&proposal.handle, &restated).map_err(|e| e.0),
                Err(Refusal::Mismatch),
                "{restated:?}"
            );
            // Spent: the correct restatement cannot now succeed.
            assert_eq!(
                gate.take(&proposal.handle, &binding("ship it"))
                    .map_err(|e| e.0),
                Err(Refusal::Used)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_proposal_is_refused_as_expired() {
        let gate = PostGate::with_ttl(Duration::from_secs(5));
        let (proposal, _) = gate.propose(binding("late"));
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(gate.pending().is_none());
        assert_eq!(
            gate.take(&proposal.handle, &binding("late"))
                .map_err(|e| e.0),
            Err(Refusal::Expired)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_commit_that_is_first_to_notice_the_expiry_refuses_it_and_wakes_watchers() {
        // Nothing reads `pending` first here, so the lapse is found inside `take` itself.
        let gate = PostGate::with_ttl(Duration::from_secs(5));
        let (proposal, _) = gate.propose(binding("late"));
        let changes = gate.changed.subscribe();
        tokio::time::advance(Duration::from_secs(5)).await;
        let (why, lapsed) = gate
            .take(&proposal.handle, &binding("late"))
            .expect_err("an expired proposal was committed");
        assert_eq!(why, Refusal::Expired);
        assert_eq!(lapsed.map(|p| p.serial), Some(proposal.serial));
        assert!(changes.has_changed().expect("the gate is alive"));
        assert!(gate.pending().is_none());
    }

    #[tokio::test]
    async fn a_newer_proposal_supersedes_the_pending_one() {
        let gate = PostGate::new();
        let (first, _) = gate.propose(binding("draft one"));
        let (second, displaced) = gate.propose(binding("draft two"));
        assert_eq!(
            displaced,
            Some(Displaced {
                proposal: first.clone(),
                why: Refusal::Superseded
            })
        );
        assert!(second.serial > first.serial);
        assert_ne!(first.handle, second.handle);
        assert_eq!(
            gate.take(&first.handle, &binding("draft one"))
                .map_err(|e| e.0),
            Err(Refusal::Superseded)
        );
        assert_eq!(gate.pending(), Some(second));
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_direct_send_is_not_begun_twice_on_its_way_or_within_a_minute() {
        // `#264 voice-send-approval`: a retried tool call, or an owner's "try again" after a lost
        // answer, must not post the same words twice.
        let gate = PostGate::new();
        let (draft, _) = gate.propose(binding("draft"));
        let changes = gate.changed.subscribe();
        let first = gate
            .begin_direct(binding("rebase"))
            .expect("nothing like it yet");
        let serial = first.serial();
        assert!(serial > draft.serial, "direct sends share the counter");
        let on_its_way = gate
            .begin_direct(binding("rebase"))
            .expect_err("begun twice while on its way");
        assert_eq!(on_its_way.serial, serial);
        assert_eq!(on_its_way.outcome, DirectOutcome::InFlight);
        assert_eq!(on_its_way.left, DIRECT_REPEAT_WINDOW);

        // Other words, another channel or another reply target are other posts.
        let other = gate.begin_direct(binding("merge")).expect("other words");
        other.finish(Some(DirectOutcome::Sent));
        let elsewhere = Binding {
            channel_id: "222".to_owned(),
            ..binding("rebase")
        };
        gate.begin_direct(elsewhere)
            .expect("another channel")
            .finish(None);
        let replying = Binding {
            reply_to: Some("42".to_owned()),
            ..binding("rebase")
        };
        gate.begin_direct(replying)
            .expect("another reply target")
            .finish(None);

        first.finish(Some(DirectOutcome::Sent));
        tokio::time::advance(Duration::from_secs(20)).await;
        let sent = gate
            .begin_direct(binding("rebase"))
            .expect_err("posted again 20 s after it went");
        assert_eq!(sent.outcome, DirectOutcome::Sent);
        assert_eq!(sent.age, Duration::from_secs(20));
        assert_eq!(sent.left, Duration::from_secs(40));

        tokio::time::advance(Duration::from_secs(40)).await;
        gate.begin_direct(binding("rebase"))
            .expect("a minute after it ended, the same words may go again")
            .finish(None);
        assert_eq!(
            gate.pending(),
            Some(draft),
            "a direct send touched the pending proposal"
        );
        assert!(
            !changes.has_changed().expect("the gate is alive"),
            "a watcher was woken"
        );
    }

    #[tokio::test]
    async fn a_direct_send_that_certainly_posted_nothing_is_forgotten_and_an_unknown_one_is_not() {
        let gate = PostGate::new();
        gate.begin_direct(binding("refused"))
            .expect("first")
            .finish(None);
        gate.begin_direct(binding("refused"))
            .expect("nothing was posted, so trying again is allowed")
            .finish(None);

        for outcome in [
            DirectOutcome::Partly,
            DirectOutcome::PartlyUnconfirmed,
            DirectOutcome::Unconfirmed,
        ] {
            let text = outcome.as_str();
            gate.begin_direct(binding(text))
                .expect("first")
                .finish(Some(outcome));
            let repeat = gate
                .begin_direct(binding(text))
                .expect_err("posted again over a send that may have landed");
            assert_eq!(repeat.outcome, outcome);
        }
    }

    #[tokio::test]
    async fn a_direct_send_abandoned_on_its_way_is_remembered_as_unconfirmed() {
        // A client that hangs up mid-call drops the request, and the post may already be on the
        // wire: that is a send nobody can confirm, not one that never happened.
        let gate = PostGate::new();
        let ticket = gate.begin_direct(binding("rebase")).expect("first");
        drop(ticket);
        let repeat = gate
            .begin_direct(binding("rebase"))
            .expect_err("an abandoned send was forgotten");
        assert_eq!(repeat.outcome, DirectOutcome::Unconfirmed);
    }

    #[tokio::test]
    async fn a_cancelled_proposal_cannot_be_committed() {
        let gate = PostGate::new();
        let (proposal, _) = gate.propose(binding("never mind"));
        assert_eq!(gate.cancel(&proposal.handle), Ok(proposal.clone()));
        assert_eq!(gate.cancel(&proposal.handle), Err(Refusal::Used));
        assert_eq!(
            gate.take(&proposal.handle, &binding("never mind"))
                .map_err(|e| e.0),
            Err(Refusal::Used)
        );
    }

    #[tokio::test]
    async fn a_handle_that_was_never_issued_is_unknown_and_disturbs_nothing() {
        let gate = PostGate::new();
        let (proposal, _) = gate.propose(binding("real"));
        assert_eq!(
            gate.take("made-up", &binding("real")).map_err(|e| e.0),
            Err(Refusal::Unknown)
        );
        assert_eq!(gate.cancel("made-up"), Err(Refusal::Unknown));
        assert_eq!(gate.pending(), Some(proposal));
    }

    #[tokio::test]
    async fn handles_are_distinct_and_unguessably_long() {
        let gate = PostGate::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..100 {
            let (proposal, _) = gate.propose(binding(&format!("m{i}")));
            // 16 bytes, unpadded base64url.
            assert_eq!(proposal.handle.len(), 22, "{}", proposal.handle);
            assert!(seen.insert(proposal.handle));
        }
    }

    #[tokio::test]
    async fn concurrent_commits_of_one_handle_post_at_most_once() {
        let gate = std::sync::Arc::new(PostGate::new());
        let (proposal, _) = gate.propose(binding("once"));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let gate = gate.clone();
            let handle = proposal.handle.clone();
            tasks.push(tokio::spawn(async move {
                gate.take(&handle, &binding("once")).is_ok()
            }));
        }
        let mut wins = 0;
        for task in tasks {
            if task.await.expect("task") {
                wins += 1;
            }
        }
        assert_eq!(wins, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_poll_wakes_for_a_new_proposal_and_for_an_expiry() {
        let gate = std::sync::Arc::new(PostGate::with_ttl(Duration::from_secs(5)));
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.wait_for_change(None, Duration::from_secs(30)).await })
        };
        tokio::task::yield_now().await;
        let (proposal, _) = gate.propose(binding("hello"));
        assert_eq!(waiter.await.expect("task"), Some(proposal.clone()));

        // Seen: waits, then wakes by itself at expiry with nothing pending.
        let started = Instant::now();
        let after = gate
            .wait_for_change(Some(proposal.serial), Duration::from_secs(30))
            .await;
        assert_eq!(after, None);
        assert_eq!(started.elapsed(), Duration::from_secs(5));

        // Nothing changes: answers at the deadline with the same (empty) state.
        let started = Instant::now();
        assert_eq!(
            gate.wait_for_change(None, Duration::from_secs(7)).await,
            None
        );
        assert_eq!(started.elapsed(), Duration::from_secs(7));
    }
}
