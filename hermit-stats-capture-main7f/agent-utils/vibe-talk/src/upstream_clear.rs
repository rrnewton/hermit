//! Clearing a channel's unread state at its chat provider, as this server ingests it.
//! `#232 upstream-mark-read`.
//!
//! # Why
//!
//! vibe-talk reads only some of the owner's channels, and the ones it reads are mostly coding
//! agents posting updates. In the provider's own client those updates bury the messages from
//! people. With this turned on for a channel, every message this server receives there is also
//! marked read in the provider's client, so that client's unread list keeps the people and
//! vibe-talk keeps the agents. vibe-talk's own read marks and Done state are not touched.
//!
//! # How
//!
//! * **The setting is the server's, per channel**: off, a dry run, or on, in the durable store
//!   ([`crate::store::UpstreamClearSetting`]), because the server has to act with no page open.
//! * **Ingest never waits for it.** [`crate::live::LiveHub`] is where both message producers land
//!   (adapter pushes and polls). On every created message it calls [`Candidates::offer`], which
//!   is one map update under a mutex and a wake-up, with no I/O.
//! * **One push per burst.** A channel's newest message is pushed after [`Timing::quiet`] with
//!   nothing newer, or after [`Timing::max_wait`] during steady traffic, and never sooner than
//!   [`Timing::max_wait`] after that channel's previous push.
//! * **Never backwards.** A message at or before the newest one already covered is never pushed;
//!   that floor is in the store and survives a restart. The provider bridge also checks its
//!   cursor and refuses to move it back; on this path it may weigh a push against the cursor it
//!   last read or wrote, a few minutes old at most, which the README's bridge section describes.
//! * **Failures wait and retry**, doubling from [`Timing::retry_first`] to [`Timing::retry_cap`]
//!   with a little per-channel jitter. The newest message always wins, and a success resets it.
//! * **Every attempt is logged on one line** starting `upstream-clear:`, dry runs included, so
//!   an operator can see what the server would do before the owner relies on it.
//! * **"Mark read now"** in Settings ([`Candidates::push_now`], `#253 upstream-clear-polish`)
//!   pushes the channel's newest message at once instead of after the quiet wait, through the same
//!   loop, and the request waits for the loop to record that push ([`Candidates::wait_for`]) so
//!   the page can say what it came to, a failure as a failure. It never shortens a retry wait.
//!
//! Pushes are serial across channels: one task, one provider call at a time.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::chat::{ChatClient, ChatError, UpstreamClearOutcome};
use crate::model::{ChannelId, Message, MessageId};
use crate::store::{
    StateStore, StoreError, UpstreamClearAttempt, UpstreamClearMode, UpstreamClearResult,
    UpstreamClearSetting,
};

/// How long a channel must be quiet before its newest message is pushed.
pub const DEFAULT_QUIET: Duration = Duration::from_secs(5);
/// The longest a message waits during steady traffic, and the least time between two pushes for
/// one channel.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(30);
/// The wait after a first failure.
pub const DEFAULT_RETRY_FIRST: Duration = Duration::from_secs(30);
/// The longest wait between retries.
pub const DEFAULT_RETRY_CAP: Duration = Duration::from_secs(15 * 60);

/// What a refusal for a thread-bound channel means, in words for Settings.
pub const THREAD_CHANNEL_REASON: &str = "this channel is one thread of a larger space, and the \
     provider's read marker covers the whole space, so it would also clear messages vibe-talk \
     never received";

/// The waits this module keeps. Tests shorten them; the server uses [`Timing::default`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// See [`DEFAULT_QUIET`].
    pub quiet: Duration,
    /// See [`DEFAULT_MAX_WAIT`].
    pub max_wait: Duration,
    /// See [`DEFAULT_RETRY_FIRST`].
    pub retry_first: Duration,
    /// See [`DEFAULT_RETRY_CAP`].
    pub retry_cap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            quiet: DEFAULT_QUIET,
            max_wait: DEFAULT_MAX_WAIT,
            retry_first: DEFAULT_RETRY_FIRST,
            retry_cap: DEFAULT_RETRY_CAP,
        }
    }
}

/// How long to wait after `failures` failures in a row: doubling from `first`, never more than
/// `cap`, plus up to a tenth more that depends only on the channel, so channels that failed
/// together do not all retry in the same instant.
#[must_use]
pub fn retry_delay(timing: &Timing, channel: &ChannelId, failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    let base = timing
        .retry_first
        .saturating_mul(1_u32 << doublings)
        .min(timing.retry_cap);
    // FNV-1a over the id: stable, cheap, and spread enough for a handful of channels.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in channel.as_str().bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    base + base.mul_f64(f64::from(hash % 1000) / 10_000.0)
}

/// Where a message sits in its channel: its instant in milliseconds, then its numeric id for two
/// messages in the same millisecond. `None` when its instant cannot be read, and such a message
/// is never pushed: a cursor cannot be placed by a time nobody can read.
#[must_use]
pub fn position(message: &Message) -> Option<(i64, u64)> {
    position_of(&message.id, &message.timestamp)
}

fn position_of(id: &MessageId, timestamp: &str) -> Option<(i64, u64)> {
    let instant = timestamp.parse::<jiff::Timestamp>().ok()?;
    Some((instant.as_millisecond(), id.numeric().unwrap_or(0)))
}

/// A message waiting to be pushed.
#[derive(Clone, Debug)]
struct Pending {
    message: MessageId,
    timestamp: String,
    position: (i64, u64),
    first_seen: Instant,
    last_seen: Instant,
    immediate: bool,
}

/// One enabled channel's state between pushes.
#[derive(Debug)]
struct Runtime {
    mode: UpstreamClearMode,
    generation: u64,
    pending: Option<Pending>,
    floor: Option<(i64, u64)>,
    last_push: Option<Instant>,
    failures: u32,
    retry_at: Option<Instant>,
    retry_at_ms: Option<i64>,
    unavailable: Option<String>,
    /// The latest push to finish AND be recorded in the store in this generation, whatever it came
    /// to; see [`Candidates::wait_for`].
    last_report: Option<Report>,
    /// How many pushes have finished and been recorded in this generation, so a press can tell a
    /// push after it from one before it.
    reports: u64,
}

/// One finished, recorded push, as a press waiting on it needs it. `#253 upstream-clear-polish`.
#[derive(Clone, Debug)]
struct Report {
    position: (i64, u64),
    /// What was recorded; `None` when nothing was sent because the channel is no longer one this
    /// server can clear.
    attempt: Option<UpstreamClearAttempt>,
}

/// One push the loop has taken and not yet finished.
#[derive(Clone, Debug)]
pub struct Job {
    /// The channel.
    pub channel: ChannelId,
    /// The newest message, at the moment the push was taken.
    pub message: MessageId,
    /// That message's instant as the provider wrote it.
    pub timestamp: String,
    /// Off is never taken; this is a dry run or on.
    pub mode: UpstreamClearMode,
    position: (i64, u64),
    generation: u64,
    pending: Pending,
}

/// What the page is told about a channel's push state beyond what the store holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveStatus {
    /// Whether a message is waiting to be pushed.
    pub pending: bool,
    /// When the next retry is due, in milliseconds since the Unix epoch, after a failure.
    pub retry_at_ms: Option<i64>,
    /// Why this channel cannot be cleared at all, once the provider has said so.
    pub unavailable: Option<String>,
}

/// What a press of "Mark read now" came to before anything is sent. `#253 upstream-clear-polish`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushNow {
    /// The channel is off in the running loop.
    Off,
    /// Its provider refused the channel; nothing more is pushed until the setting is saved again.
    Unavailable,
    /// The message's instant cannot be read, and a cursor cannot be placed by a time nobody can
    /// read.
    Unplaceable,
    /// An earlier push, or an earlier dry run, already covers it. Nothing is sent.
    Covered,
    /// The channel is waiting out a failure. The message is pushed when the retry is due, not
    /// sooner: a press never shortens the wait a spent quota asked for.
    RetryWaiting,
    /// Due at once. Wait for it with [`Candidates::wait_for`].
    Queued(Ticket),
}

/// What waiting on a press came to: [`Candidates::wait_for`]. `#253 upstream-clear-polish`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settled {
    /// The push that answers the press finished and was recorded: the pressed message's own, or a
    /// newer message's, which covers it. This is the attempt as recorded, failures included.
    Recorded(UpstreamClearAttempt),
    /// An older push, already on its way when the press came, failed after it, and the channel is
    /// waiting out that failure. The pressed message goes at the retry; nothing was sent for it.
    RetryWaiting,
    /// The provider refused the channel while the press waited.
    Unavailable,
    /// The channel's setting changed while the press waited (turned off, set to the other mode,
    /// or every setting erased), or it is no longer a channel this server can clear, so the push
    /// will not be made as asked.
    Changed,
    /// Nothing answered the press before the deadline. The push is still queued or on its way,
    /// and the loop carries on without whoever was waiting.
    Waiting,
}

/// A press that was queued, to wait on with [`Candidates::wait_for`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    generation: u64,
    position: (i64, u64),
    reports: u64,
}

/// What noting a message came to, when the channel takes it.
#[derive(Clone, Copy, Debug)]
struct Noted {
    ticket: Ticket,
    /// Whether it is now the channel's newest waiting message.
    newest: bool,
    /// Whether the channel is waiting out a failure.
    retry_waiting: bool,
}

/// The newest message per enabled channel, waiting to be pushed, and the wake-up for the loop.
///
/// Owned by [`crate::live::LiveHub`], which feeds it. Holds nothing for a channel that is off, so
/// a deployment with the feature unused pays one map lookup per message.
#[derive(Debug, Default)]
pub struct Candidates {
    channels: Mutex<BTreeMap<ChannelId, Runtime>>,
    next_generation: Mutex<u64>,
    wake: Notify,
    /// Woken each time a finished push has been recorded; see [`Candidates::wait_for`].
    recorded: Notify,
    /// Held across a change to the store's settings and the matching change here; see
    /// [`Candidates::changing`].
    changes: tokio::sync::Mutex<()>,
}

fn lock<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

impl Candidates {
    /// An empty set: every channel off.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note a newly created message. Never blocks on I/O; returns whether it is now the
    /// channel's newest waiting message.
    pub fn offer(&self, channel: &ChannelId, message: &Message) -> bool {
        self.note(channel, &message.id, &message.timestamp, false)
            .is_ok_and(|noted| noted.newest)
    }

    /// [`Candidates::offer`] from a message's id and instant alone.
    pub fn offer_parts(&self, channel: &ChannelId, id: &MessageId, timestamp: &str) -> bool {
        self.note(channel, id, timestamp, false)
            .is_ok_and(|noted| noted.newest)
    }

    /// Note `message`, the channel's newest, and push it as soon as the loop can, without waiting
    /// for quiet or for the spacing between pushes: the owner pressed "Mark read now" (or "Try it
    /// now" in a dry run). `#253 upstream-clear-polish`.
    ///
    /// A message already covered is not pushed again, and a channel waiting out a failure keeps
    /// its wait: the message goes at the retry.
    pub fn push_now(&self, channel: &ChannelId, message: &Message) -> PushNow {
        match self.note(channel, &message.id, &message.timestamp, true) {
            Err(refused) => refused,
            Ok(noted) if noted.retry_waiting => PushNow::RetryWaiting,
            Ok(noted) => PushNow::Queued(noted.ticket),
        }
    }

    fn note(
        &self,
        channel: &ChannelId,
        id: &MessageId,
        timestamp: &str,
        immediate: bool,
    ) -> Result<Noted, PushNow> {
        let Some(position) = position_of(id, timestamp) else {
            return Err(PushNow::Unplaceable);
        };
        let now = Instant::now();
        let mut channels = lock(&self.channels);
        let Some(runtime) = channels.get_mut(channel) else {
            return Err(PushNow::Off);
        };
        if runtime.unavailable.is_some() {
            return Err(PushNow::Unavailable);
        }
        if runtime.floor.is_some_and(|floor| position <= floor) {
            return Err(PushNow::Covered);
        }
        let mut noted = Noted {
            ticket: Ticket {
                generation: runtime.generation,
                position,
                reports: runtime.reports,
            },
            newest: false,
            retry_waiting: runtime.retry_at.is_some_and(|retry| retry > now),
        };
        let pending = match runtime.pending.take() {
            Some(mut pending) if position <= pending.position => {
                pending.last_seen = now;
                pending.immediate |= immediate;
                runtime.pending = Some(pending);
                drop(channels);
                if immediate {
                    self.wake.notify_one();
                }
                return Ok(noted);
            }
            Some(pending) => Pending {
                message: id.clone(),
                timestamp: timestamp.to_owned(),
                position,
                first_seen: pending.first_seen,
                last_seen: now,
                immediate: immediate || pending.immediate,
            },
            None => Pending {
                message: id.clone(),
                timestamp: timestamp.to_owned(),
                position,
                first_seen: now,
                last_seen: now,
                immediate,
            },
        };
        runtime.pending = Some(pending);
        drop(channels);
        self.wake.notify_one();
        noted.newest = true;
        Ok(noted)
    }

    /// Wait, until `deadline` at the latest, for what answers the press `ticket` queued, and say
    /// what that was. `#253 upstream-clear-polish`.
    ///
    /// Only an answer to THIS press settles it: a recorded push at or past the pressed message
    /// (whatever it came to, so a failure is never reported as done), a failure of an older push
    /// after the press (the pressed message then waits for the retry), the provider refusing the
    /// channel, or the channel's setting changing. A push of an older message that succeeds after
    /// the press does not; the pressed message is still to go.
    pub async fn wait_for(
        &self,
        channel: &ChannelId,
        ticket: Ticket,
        deadline: Instant,
    ) -> Settled {
        loop {
            // Armed before the check, so a record landing between the two still wakes it.
            let recorded = self.recorded.notified();
            tokio::pin!(recorded);
            recorded.as_mut().enable();
            if let Some(settled) = self.settled(channel, ticket) {
                return settled;
            }
            tokio::select! {
                () = &mut recorded => {}
                () = tokio::time::sleep_until(deadline) => {
                    return self.settled(channel, ticket).unwrap_or(Settled::Waiting);
                }
            }
        }
    }

    fn settled(&self, channel: &ChannelId, ticket: Ticket) -> Option<Settled> {
        let channels = lock(&self.channels);
        let Some(runtime) = channels.get(channel) else {
            // Turned off, or every channel forgotten: nothing more will be pushed.
            return Some(Settled::Changed);
        };
        if runtime.generation != ticket.generation {
            return Some(Settled::Changed);
        }
        let since = runtime.reports > ticket.reports;
        if let Some(report) = runtime.last_report.as_ref().filter(|_| since) {
            if report.position >= ticket.position {
                return Some(
                    report
                        .attempt
                        .clone()
                        .map_or(Settled::Changed, Settled::Recorded),
                );
            }
        }
        if runtime.unavailable.is_some() {
            return Some(Settled::Unavailable);
        }
        if since && runtime.retry_at.is_some() {
            return Some(Settled::RetryWaiting);
        }
        None
    }

    /// Say that `job` has finished and `attempt` is recorded (`None` when nothing was sent), and
    /// wake whoever waits on it.
    fn recorded(&self, job: &Job, attempt: Option<&UpstreamClearAttempt>) {
        let mut channels = lock(&self.channels);
        if let Some(runtime) = channels.get_mut(&job.channel) {
            if runtime.generation == job.generation {
                runtime.last_report = Some(Report {
                    position: job.position,
                    attempt: attempt.cloned(),
                });
                runtime.reports += 1;
            }
        }
        drop(channels);
        self.recorded.notify_waiters();
    }

    /// Hold this while changing settings in the store and then here, and let it go before any
    /// provider call.
    ///
    /// The store is what Settings shows; this set is what the loop acts on. Two changes that
    /// interleave (a save and an erase of the store, or two saves from two devices) could
    /// otherwise leave this set acting on a mode the store no longer holds, which is the loop
    /// marking messages read while Settings says Off.
    pub async fn changing(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.changes.lock().await
    }

    /// Forget every channel, as when the store's settings were erased: nothing more is pushed
    /// until a channel is set again. A push already in flight finishes and is logged, and changes
    /// nothing here. Returns how many channels were forgotten.
    pub fn forget_all(&self) -> usize {
        let mut channels = lock(&self.channels);
        let forgotten = channels.len();
        channels.clear();
        drop(channels);
        self.wake.notify_one();
        // A press waiting on a push that will now never happen stops waiting.
        self.recorded.notify_waiters();
        forgotten
    }

    /// Put a channel in `mode`, with `floor` the position its provider cursor is already known to
    /// cover. Off forgets the channel. Any other change starts it afresh, keeping a message
    /// already waiting.
    pub fn set_mode(
        &self,
        channel: &ChannelId,
        mode: UpstreamClearMode,
        floor: Option<(i64, u64)>,
    ) {
        let generation = {
            let mut next = lock(&self.next_generation);
            *next += 1;
            *next
        };
        let mut channels = lock(&self.channels);
        if mode == UpstreamClearMode::Off {
            channels.remove(channel);
        } else {
            match channels.remove(channel) {
                // The same mode again, still possible: nothing to start afresh.
                Some(previous) if previous.mode == mode && previous.unavailable.is_none() => {
                    channels.insert(channel.clone(), previous);
                }
                previous => {
                    let pending = previous
                        .and_then(|previous| previous.pending)
                        .filter(|pending| floor.is_none_or(|floor| pending.position > floor));
                    channels.insert(
                        channel.clone(),
                        Runtime {
                            mode,
                            generation,
                            pending,
                            floor,
                            last_push: None,
                            failures: 0,
                            retry_at: None,
                            retry_at_ms: None,
                            unavailable: None,
                            last_report: None,
                            reports: 0,
                        },
                    );
                }
            }
        }
        drop(channels);
        self.wake.notify_one();
        // A press waiting on this channel stops waiting when its setting changes.
        self.recorded.notify_waiters();
    }

    /// The mode the loop is acting on for `channel`: off when it holds nothing for it.
    #[must_use]
    pub fn mode(&self, channel: &ChannelId) -> UpstreamClearMode {
        lock(&self.channels)
            .get(channel)
            .map_or(UpstreamClearMode::Off, |runtime| runtime.mode)
    }

    /// What the page is told beyond the store, for one channel.
    #[must_use]
    pub fn status(&self, channel: &ChannelId) -> LiveStatus {
        lock(&self.channels)
            .get(channel)
            .map(|runtime| LiveStatus {
                pending: runtime.pending.is_some(),
                retry_at_ms: runtime.retry_at_ms,
                unavailable: runtime.unavailable.clone(),
            })
            .unwrap_or_default()
    }

    fn due_at(runtime: &Runtime, timing: &Timing) -> Option<Instant> {
        let pending = runtime.pending.as_ref()?;
        let mut due = if pending.immediate {
            pending.first_seen
        } else {
            let burst =
                (pending.last_seen + timing.quiet).min(pending.first_seen + timing.max_wait);
            runtime
                .last_push
                .map_or(burst, |pushed| burst.max(pushed + timing.max_wait))
        };
        if let Some(retry) = runtime.retry_at {
            due = due.max(retry);
        }
        Some(due)
    }

    /// When the next push is due, across every channel.
    fn next_due(&self, timing: &Timing) -> Option<Instant> {
        lock(&self.channels)
            .values()
            .filter_map(|runtime| Self::due_at(runtime, timing))
            .min()
    }

    /// Take the push that is due soonest, if one is due by `now`.
    fn take_due(&self, now: Instant, timing: &Timing) -> Option<Job> {
        let mut channels = lock(&self.channels);
        let (channel, _) = channels
            .iter()
            .filter_map(|(channel, runtime)| {
                Self::due_at(runtime, timing).map(|due| (channel.clone(), due))
            })
            .filter(|(_, due)| *due <= now)
            .min_by_key(|(_, due)| *due)?;
        let runtime = channels.get_mut(&channel)?;
        let pending = runtime.pending.take()?;
        Some(Job {
            channel,
            message: pending.message.clone(),
            timestamp: pending.timestamp.clone(),
            mode: runtime.mode,
            position: pending.position,
            generation: runtime.generation,
            pending,
        })
    }

    /// Fold a finished push back in.
    fn finish(
        &self,
        job: &Job,
        result: UpstreamClearResult,
        error: Option<&ChatError>,
        timing: &Timing,
    ) {
        let now = Instant::now();
        let mut channels = lock(&self.channels);
        let Some(runtime) = channels.get_mut(&job.channel) else {
            return;
        };
        if runtime.generation != job.generation {
            // The owner changed the mode while this was in flight; what it found is logged and
            // recorded, and the fresh state is left alone.
            return;
        }
        runtime.last_push = Some(now);
        match result {
            UpstreamClearResult::Advanced
            | UpstreamClearResult::AlreadyRead
            | UpstreamClearResult::DryRun => {
                runtime.floor = Some(
                    runtime
                        .floor
                        .map_or(job.position, |floor| floor.max(job.position)),
                );
                runtime.failures = 0;
                runtime.retry_at = None;
                runtime.retry_at_ms = None;
                if runtime
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.position <= job.position)
                {
                    runtime.pending = None;
                }
            }
            UpstreamClearResult::Skipped => {
                // The bridge no longer holds it. Wait for something newer rather than retrying.
                runtime.retry_at = None;
                runtime.retry_at_ms = None;
            }
            UpstreamClearResult::Unavailable => {
                runtime.pending = None;
                runtime.unavailable = Some(
                    error
                        .map(unavailable_reason)
                        .unwrap_or_else(|| THREAD_CHANNEL_REASON.to_owned()),
                );
            }
            UpstreamClearResult::Failed => {
                runtime.failures = runtime.failures.saturating_add(1);
                let mut wait = retry_delay(timing, &job.channel, runtime.failures);
                if let Some(asked) = error.and_then(ChatError::retry_after) {
                    wait = wait.max(asked);
                }
                runtime.retry_at = Some(now + wait);
                runtime.retry_at_ms = Some(
                    now_ms().saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)),
                );
                // The newest message wins: put this one back unless something newer arrived while
                // it was in flight. An OLDER one that arrived meanwhile (a late poll) is replaced,
                // because this one covers it.
                if runtime
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.position < job.position)
                {
                    runtime.pending = Some(Pending {
                        immediate: false,
                        ..job.pending.clone()
                    });
                }
            }
        }
        drop(channels);
        self.wake.notify_one();
    }
}

/// The sentence Settings shows for a channel the provider refused outright.
fn unavailable_reason(error: &ChatError) -> String {
    match error.cause() {
        ChatError::Status { body, .. } if body.contains("thread_channel") => {
            THREAD_CHANNEL_REASON.to_owned()
        }
        ChatError::Refused(reason) => reason.clone(),
        _ => error.to_string(),
    }
}

/// A provider that answered a dry run as a real push, or the other way round, does not keep the
/// contract, and the loop stops for that channel rather than trust it again: one that ignores a
/// dry run may already have marked messages read, and every retry would mark more.
#[must_use]
pub fn dry_run_ignored(asked_dry_run: bool, outcome: &UpstreamClearOutcome) -> ChatError {
    let previous = outcome.previous.as_deref().unwrap_or("none");
    ChatError::Refused(if asked_dry_run {
        format!(
            "the bridge answered a dry run as a real push, so it may have marked messages read \
             (it reports its read marker at {previous}, target {}); nothing more is pushed until \
             the setting is saved again, and the bridge needs fixing",
            outcome.target
        )
    } else {
        "the bridge answered a push as a dry run, so nothing was marked read; nothing more is \
         pushed until the setting is saved again, and the bridge needs fixing"
            .to_owned()
    })
}

/// A failure in words for the log and for Settings.
///
/// A bridge older than this feature answers the route with 405, as it does one with mark-read
/// turned off; both are said as what to do about it rather than as a bare status.
#[must_use]
pub fn failure_words(error: &ChatError) -> String {
    match error.cause() {
        ChatError::Status { status: 405, .. } => format!(
            "{error}; the bridge does not offer automatic clearing, so it needs updating or its \
             mark-read is turned off"
        ),
        _ => error.to_string(),
    }
}

/// What one provider answer comes to.
#[must_use]
pub fn classify(answer: &Result<UpstreamClearOutcome, ChatError>) -> UpstreamClearResult {
    match answer {
        Ok(outcome) if outcome.dry_run => UpstreamClearResult::DryRun,
        Ok(outcome) if outcome.advanced => UpstreamClearResult::Advanced,
        Ok(_) => UpstreamClearResult::AlreadyRead,
        Err(error) => match error.cause() {
            // Refused by this server or the provider client, not by a passing fault: trying the
            // same thing again cannot help.
            ChatError::Status { status: 409, .. } | ChatError::Refused(_) => {
                UpstreamClearResult::Unavailable
            }
            ChatError::Status { status: 404, .. } => UpstreamClearResult::Skipped,
            _ => UpstreamClearResult::Failed,
        },
    }
}

/// A channel's display name for a log line, when it has one.
pub type LabelOf = Arc<dyn Fn(&ChannelId) -> Option<String> + Send + Sync>;

/// Whether a channel is one the server answers for and whose provider can be cleared.
pub type Eligible = Arc<dyn Fn(&ChannelId) -> bool + Send + Sync>;

/// Everything the loop needs, without the whole server state, so a test can build one.
#[derive(Clone)]
pub struct Clearer {
    /// The provider router; each push goes to the channel's own provider.
    pub chat: Arc<dyn ChatClient>,
    /// Where the setting and every attempt are kept.
    pub store: Arc<dyn StateStore>,
    /// The hub's waiting messages.
    pub candidates: Arc<Candidates>,
    /// The waits.
    pub timing: Timing,
    /// The channel's display name for the log line, when it has one.
    pub label: LabelOf,
    /// Whether `channel` is one the server answers for and whose provider can be cleared.
    pub eligible: Eligible,
}

impl std::fmt::Debug for Clearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clearer")
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

/// The floor a stored setting gives the loop: the newest message already covered.
#[must_use]
pub fn stored_floor(setting: &UpstreamClearSetting) -> Option<(i64, u64)> {
    setting.pushed.as_ref().map(UpstreamClearAttempt::position)
}

impl Clearer {
    /// The loop for a running server.
    #[must_use]
    pub fn for_state(state: &crate::state::AppState) -> Self {
        let labels = state.clone();
        let eligible = state.clone();
        Self {
            chat: Arc::clone(&state.chat),
            store: Arc::clone(&state.store),
            candidates: Arc::clone(state.live.upstream_clear()),
            timing: Timing::default(),
            label: Arc::new(move |channel| {
                labels
                    .channel(channel.as_str())
                    .map(|info| info.alias.unwrap_or(info.label))
            }),
            eligible: Arc::new(move |channel| {
                eligible.channel(channel.as_str()).is_some()
                    && eligible
                        .providers
                        .client_for(channel)
                        .is_some_and(|client| client.supports_upstream_clear())
            }),
        }
    }

    /// Load every enabled channel from the store and seed each with its newest message, so a
    /// message that arrived while the server was down is still cleared.
    ///
    /// # Errors
    ///
    /// The store's, when it cannot be read. [`StoreError::Unavailable`] means no store is
    /// configured, and nothing can be enabled.
    pub async fn seed(&self) -> Result<usize, StoreError> {
        // The settings are read and taken in as one change, so a save landing meanwhile is
        // neither lost nor overwritten by an older reading; the provider is asked afterwards.
        let enabled: Vec<UpstreamClearSetting> = {
            let _changing = self.candidates.changing().await;
            let settings = self.store.upstream_clear_settings().await?;
            settings
                .into_iter()
                .filter(|setting| {
                    setting.mode != UpstreamClearMode::Off && (self.eligible)(&setting.channel)
                })
                .inspect(|setting| {
                    self.candidates
                        .set_mode(&setting.channel, setting.mode, stored_floor(setting));
                })
                .collect()
        };
        for setting in &enabled {
            tracing::info!(
                channel = %setting.channel,
                mode = setting.mode.as_str(),
                "upstream-clear: {} channel {} ({}) is enabled; its newest message is checked now",
                setting.mode.as_str(),
                setting.channel,
                self.label_of(&setting.channel),
            );
            match self.chat.fetch_recent(&setting.channel, 1).await {
                Ok(messages) => {
                    if let Some(newest) = messages.last() {
                        self.candidates.offer(&setting.channel, newest);
                    }
                }
                Err(error) => tracing::warn!(
                    channel = %setting.channel,
                    %error,
                    "upstream-clear: could not read the newest message to seed this channel; \
                     the next message that arrives will be pushed instead"
                ),
            }
        }
        Ok(enabled.len())
    }

    fn label_of(&self, channel: &ChannelId) -> String {
        (self.label)(channel).unwrap_or_else(|| "unlisted".to_owned())
    }

    /// Push the soonest due message, if one is due now. Returns whether one was pushed.
    pub async fn push_due(&self) -> bool {
        let Some(job) = self.candidates.take_due(Instant::now(), &self.timing) else {
            return false;
        };
        if !(self.eligible)(&job.channel) {
            tracing::info!(
                channel = %job.channel,
                "upstream-clear: channel {} is no longer listed or its provider cannot be \
                 cleared; nothing was pushed",
                job.channel
            );
            self.candidates.recorded(&job, None);
            return true;
        }
        let dry_run = job.mode == UpstreamClearMode::DryRun;
        let answer = match self
            .chat
            .clear_upstream_through(&job.channel, &job.message, dry_run)
            .await
        {
            Ok(outcome) if outcome.dry_run != dry_run => Err(dry_run_ignored(dry_run, &outcome)),
            answer => answer,
        };
        let would_advance = answer.as_ref().is_ok_and(|outcome| outcome.advanced);
        let result = classify(&answer);
        self.candidates
            .finish(&job, result, answer.as_ref().err(), &self.timing);
        let attempt = UpstreamClearAttempt {
            at_ms: now_ms(),
            message: job.message.clone(),
            message_ms: job.position.0,
            result,
            target: answer.as_ref().ok().map(|outcome| outcome.target.clone()),
            previous: answer
                .as_ref()
                .ok()
                .and_then(|outcome| outcome.previous.clone()),
            error: answer.as_ref().err().map(failure_words),
        };
        self.log(&job, &attempt, would_advance);
        if let Err(error) = self
            .store
            .record_upstream_clear(&job.channel, &attempt)
            .await
        {
            tracing::warn!(
                channel = %job.channel,
                %error,
                "upstream-clear: could not record this attempt; Settings will show an older one"
            );
        }
        // Only now, so a press waiting on this push reads the attempt back from the store.
        self.candidates.recorded(&job, Some(&attempt));
        true
    }

    /// `advanced` is the provider's own word: in a dry run, whether a real push would have moved
    /// its marker.
    fn log(&self, job: &Job, attempt: &UpstreamClearAttempt, advanced: bool) {
        let head = format!(
            "upstream-clear: {} channel {} ({}) through {} at {}",
            job.mode.as_str(),
            job.channel,
            self.label_of(&job.channel),
            job.message,
            job.timestamp,
        );
        let previous = attempt.previous.as_deref().unwrap_or("none");
        let target = attempt.target.as_deref().unwrap_or("?");
        match attempt.result {
            UpstreamClearResult::Advanced => tracing::info!(
                channel = %job.channel,
                "{head}: advanced (the provider's read marker moved from {previous} to {target})"
            ),
            UpstreamClearResult::AlreadyRead => tracing::info!(
                channel = %job.channel,
                "{head}: already-read (the provider's read marker was already at {previous})"
            ),
            UpstreamClearResult::DryRun if advanced => tracing::info!(
                channel = %job.channel,
                "{head}: dry-run (would move the provider's read marker from {previous} to \
                 {target}; nothing was written)"
            ),
            UpstreamClearResult::DryRun => tracing::info!(
                channel = %job.channel,
                "{head}: dry-run (would leave the provider's read marker at {previous}, already \
                 at or past {target}; nothing was written)"
            ),
            UpstreamClearResult::Skipped => tracing::info!(
                channel = %job.channel,
                "{head}: skipped (the bridge no longer holds that message; waiting for a newer one)"
            ),
            UpstreamClearResult::Unavailable => tracing::warn!(
                channel = %job.channel,
                "{head}: failed: not available for this channel ({}); no more attempts until the \
                 setting is changed",
                attempt.error.as_deref().unwrap_or("refused"),
            ),
            UpstreamClearResult::Failed => tracing::warn!(
                channel = %job.channel,
                "{head}: failed: {}; retrying later",
                attempt.error.as_deref().unwrap_or("unknown failure"),
            ),
        }
    }

    /// Push due messages until the process ends. Spawned once by `main`.
    pub async fn run(self) {
        match self.seed().await {
            Ok(enabled) => tracing::info!(
                enabled,
                "upstream-clear is ready: a channel set to clear unread in its provider has every \
                 message vibe-talk receives marked read there too, one push per burst. Off for \
                 every channel unless set in Settings"
            ),
            Err(StoreError::Unavailable(setting)) => {
                tracing::info!(
                    "upstream-clear is unavailable: it keeps its setting in the store, and \
                     {setting} is not configured"
                );
                return;
            }
            Err(error) => tracing::warn!(
                %error,
                "upstream-clear could not read its settings; channels set in Settings from now on \
                 still work"
            ),
        }
        loop {
            while self.push_due().await {}
            let notified = self.candidates.wake.notified();
            match self.candidates.next_due(&self.timing) {
                Some(due) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(due) => {}
                        () = notified => {}
                    }
                }
                None => notified.await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::fake::FakeDiscord;
    use crate::store::fake::FakeStore;

    const QUICK: Timing = Timing {
        quiet: Duration::from_secs(5),
        max_wait: Duration::from_secs(30),
        retry_first: Duration::from_secs(30),
        retry_cap: Duration::from_secs(15 * 60),
    };

    #[test]
    fn the_retry_wait_doubles_to_its_cap_with_at_most_a_tenth_more() {
        let channel = ChannelId("1111111111".to_owned());
        let mut previous = Duration::ZERO;
        for failures in 1..=12 {
            let wait = retry_delay(&QUICK, &channel, failures);
            let base = (QUICK.retry_first * 2_u32.pow((failures - 1).min(16))).min(QUICK.retry_cap);
            assert!(
                wait >= base && wait <= base.mul_f64(1.1),
                "{failures}: {wait:?}"
            );
            assert!(wait >= previous, "the wait shrank at {failures}");
            previous = wait;
        }
        assert!(retry_delay(&QUICK, &channel, 40) <= QUICK.retry_cap.mul_f64(1.1));
    }

    #[test]
    fn answers_are_classified_by_what_the_provider_said() {
        let outcome = |advanced, dry_run| UpstreamClearOutcome {
            scope: "space".to_owned(),
            target: "t".to_owned(),
            previous: None,
            advanced,
            dry_run,
        };
        assert_eq!(
            classify(&Ok(outcome(true, false))),
            UpstreamClearResult::Advanced
        );
        assert_eq!(
            classify(&Ok(outcome(false, false))),
            UpstreamClearResult::AlreadyRead
        );
        assert_eq!(
            classify(&Ok(outcome(false, true))),
            UpstreamClearResult::DryRun
        );
        let status = |status: u16| {
            Err(ChatError::Status {
                status,
                body: String::new(),
            }
            .with_provider("Google Chat"))
        };
        assert_eq!(classify(&status(409)), UpstreamClearResult::Unavailable);
        assert_eq!(classify(&status(404)), UpstreamClearResult::Skipped);
        assert_eq!(classify(&status(502)), UpstreamClearResult::Failed);
        assert_eq!(classify(&status(405)), UpstreamClearResult::Failed);
        assert_eq!(
            classify(&Err(ChatError::Transport("reset".to_owned()))),
            UpstreamClearResult::Failed
        );
        // Refused before or instead of a provider write: trying again cannot help.
        assert_eq!(
            classify(&Err(
                ChatError::Refused("cannot".to_owned()).with_provider("Google Chat")
            )),
            UpstreamClearResult::Unavailable
        );
    }

    #[test]
    fn a_bridge_without_the_route_is_said_as_one_that_needs_updating() {
        let old = ChatError::Status {
            status: 405,
            body: r#"{"message":"method not allowed"}"#.to_owned(),
        }
        .with_provider("Google Chat");
        let words = failure_words(&old);
        assert!(
            words.starts_with("Google Chat returned HTTP 405"),
            "{words}"
        );
        assert!(words.contains("needs updating"), "{words}");
        let other = ChatError::Transport("reset".to_owned());
        assert_eq!(failure_words(&other), other.to_string());
    }

    #[test]
    fn a_message_whose_instant_cannot_be_read_is_never_a_candidate() {
        let candidates = Candidates::new();
        let channel = ChannelId("1111111111".to_owned());
        candidates.set_mode(&channel, UpstreamClearMode::On, None);
        let fake = FakeDiscord::new();
        let id = fake.seed(&channel, "agent", "hello");
        let mut message = fake.message(&id).expect("seeded");
        message.timestamp = "yesterday".to_owned();
        assert!(!candidates.offer(&channel, &message));
        assert!(!candidates.status(&channel).pending);
    }

    /// A clearer over the fake provider and fake store, every channel eligible.
    fn clearer(fake: &Arc<FakeDiscord>, store: &Arc<FakeStore>) -> Clearer {
        Clearer {
            chat: fake.clone(),
            store: store.clone(),
            candidates: Arc::new(Candidates::new()),
            timing: QUICK,
            label: Arc::new(|_| Some("agents".to_owned())),
            eligible: Arc::new(|_| true),
        }
    }

    const AGENTS: &str = "1111111111";
    const PEOPLE: &str = "2222222222";

    fn agents() -> ChannelId {
        ChannelId(AGENTS.to_owned())
    }

    /// Seed one message `seconds` after a fixed instant and hand it back as the hub would see it.
    fn arrive(fake: &FakeDiscord, channel: &ChannelId, seconds: i64) -> Message {
        let id = fake.seed_at(
            channel,
            "agent",
            "working",
            1_790_000_000_000 + seconds * 1000,
        );
        fake.message(&id).expect("seeded")
    }

    /// Let the loop do whatever is due within `seconds`, with time standing still otherwise.
    async fn pass(seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }

    async fn on(clearer: &Clearer, channel: &ChannelId, mode: UpstreamClearMode) {
        let setting = clearer
            .store
            .set_upstream_clear_mode(channel, mode, 1)
            .await
            .expect("store");
        clearer
            .candidates
            .set_mode(channel, mode, stored_floor(&setting));
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_is_pushed_once_with_its_newest_message_after_the_channel_goes_quiet() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        pass(1).await;
        let mut newest = None;
        for second in 0..3 {
            let message = arrive(&fake, &agents(), second);
            assert!(clearer.candidates.offer(&agents(), &message));
            newest = Some(message);
            pass(1).await;
        }
        assert!(
            fake.upstream_clears().is_empty(),
            "pushed before the burst was over"
        );
        pass(5).await;
        let newest = newest.expect("three arrived");
        assert_eq!(
            fake.upstream_clears(),
            vec![(agents(), newest.id.clone(), false)],
            "one push, for the newest message"
        );
        assert_eq!(
            fake.upstream_cursor(&agents()),
            Some(newest.timestamp.clone())
        );
        pass(120).await;
        assert_eq!(
            fake.upstream_clears().len(),
            1,
            "a quiet channel was pushed again"
        );
        let setting = &store.upstream_clear_settings().await.expect("read")[0];
        let pushed = setting.pushed.as_ref().expect("recorded");
        assert_eq!(pushed.message, newest.id);
        assert_eq!(pushed.result, UpstreamClearResult::Advanced);
    }

    #[tokio::test(start_paused = true)]
    async fn steady_traffic_is_pushed_at_most_once_per_spacing() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        // A message every two seconds for three minutes never goes quiet for five.
        for second in (0..180).step_by(2) {
            clearer
                .candidates
                .offer(&agents(), &arrive(&fake, &agents(), second));
            pass(2).await;
        }
        let pushes = fake.upstream_clears().len();
        assert!(
            (5..=7).contains(&pushes),
            "three minutes of steady traffic took {pushes} pushes, not about one per 30 s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_marker_never_moves_backwards() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        let older = arrive(&fake, &agents(), 10);
        let newer = arrive(&fake, &agents(), 20);
        clearer.candidates.offer(&agents(), &newer);
        pass(10).await;
        assert_eq!(fake.upstream_clears().len(), 1);
        // An older message arriving late, a retry of the same one, and a seed of it: none is
        // pushed, because the newest pushed message is already past them.
        assert!(!clearer.candidates.offer(&agents(), &older));
        assert!(!clearer.candidates.offer(&agents(), &newer));
        pass(120).await;
        assert_eq!(
            fake.upstream_clears().len(),
            1,
            "{:?}",
            fake.upstream_clears()
        );
        // The owner read further natively: a newer push finds the cursor already there and the
        // provider does not move it back.
        let newest = arrive(&fake, &agents(), 30);
        fake.set_upstream_cursor(&agents(), "2026-10-01T00:00:00Z");
        clearer.candidates.offer(&agents(), &newest);
        pass(60).await;
        assert_eq!(fake.upstream_clears().len(), 2);
        assert_eq!(
            fake.upstream_cursor(&agents()).as_deref(),
            Some("2026-10-01T00:00:00Z"),
            "the provider's cursor moved backwards"
        );
        let setting = &store.upstream_clear_settings().await.expect("read")[0];
        assert_eq!(
            setting.last.as_ref().map(|last| last.result),
            Some(UpstreamClearResult::AlreadyRead)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn only_channels_set_to_clear_are_pushed() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        let people = ChannelId(PEOPLE.to_owned());
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        on(&clearer, &people, UpstreamClearMode::On).await;
        on(&clearer, &people, UpstreamClearMode::Off).await;
        tokio::spawn(clearer.clone().run());
        assert!(!clearer
            .candidates
            .offer(&people, &arrive(&fake, &people, 1)));
        clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 2));
        pass(60).await;
        let pushed: Vec<ChannelId> = fake
            .upstream_clears()
            .into_iter()
            .map(|(channel, _, _)| channel)
            .collect();
        assert_eq!(pushed, vec![agents()]);
        assert_eq!(fake.upstream_cursor(&people), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dry_run_writes_nothing_and_says_what_it_would_have_done() {
        let logs = crate::testing::LogCapture::info_only();
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::DryRun).await;
        tokio::spawn(clearer.clone().run());
        let message = arrive(&fake, &agents(), 5);
        clearer.candidates.offer(&agents(), &message);
        pass(10).await;
        assert_eq!(
            fake.upstream_clears(),
            vec![(agents(), message.id.clone(), true)]
        );
        assert_eq!(
            fake.upstream_cursor(&agents()),
            None,
            "a dry run moved the cursor"
        );
        let text = logs.text();
        let line = text
            .lines()
            .find(|line| {
                line.contains("upstream-clear: dry_run channel") && line.contains(" through ")
            })
            .unwrap_or_else(|| panic!("no dry-run line in {text}"));
        assert!(line.contains(AGENTS), "{line}");
        assert!(line.contains("(agents)"), "{line}");
        assert!(line.contains(message.id.as_str()), "{line}");
        assert!(
            line.contains(&message.timestamp),
            "the line names no target time: {line}"
        );
        assert!(line.contains("dry-run"), "{line}");
        assert!(
            line.contains("would move the provider's read marker from none to"),
            "{line}"
        );
        let setting = &store.upstream_clear_settings().await.expect("read")[0];
        assert_eq!(setting.pushed, None, "a dry run is not a push");
        assert_eq!(
            setting.last.as_ref().map(|last| last.result),
            Some(UpstreamClearResult::DryRun)
        );
        // The owner has read further natively: the line says a real push would move nothing,
        // rather than claiming it would move the marker from where it is to somewhere earlier.
        fake.set_upstream_cursor(&agents(), "2026-10-01T00:00:00Z");
        let later = arrive(&fake, &agents(), 60);
        clearer.candidates.offer(&agents(), &later);
        pass(60).await;
        assert_eq!(fake.upstream_clears().len(), 2);
        let text = logs.text();
        let line = text
            .lines()
            .find(|line| line.contains(later.id.as_str()) && line.contains("dry-run"))
            .unwrap_or_else(|| panic!("no second dry-run line in {text}"));
        assert!(
            line.contains(
                "would leave the provider's read marker at 2026-10-01T00:00:00Z, already at or past"
            ),
            "{line}"
        );
        assert!(!line.contains("would move"), "{line}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_bridge_that_mistakes_a_dry_run_for_a_push_is_stopped_rather_than_trusted_again() {
        let logs = crate::testing::LogCapture::info_only();
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        fake.flip_upstream_dry_run_answers();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        let people = ChannelId(PEOPLE.to_owned());
        on(&clearer, &agents(), UpstreamClearMode::DryRun).await;
        on(&clearer, &people, UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 1));
        clearer
            .candidates
            .offer(&people, &arrive(&fake, &people, 2));
        pass(10).await;
        assert_eq!(fake.upstream_clears().len(), 2);
        let ignored = clearer.candidates.status(&agents()).unavailable;
        assert!(
            ignored
                .as_deref()
                .is_some_and(|why| why.contains("answered a dry run as a real push")),
            "{ignored:?}"
        );
        let flipped = clearer.candidates.status(&people).unavailable;
        assert!(
            flipped
                .as_deref()
                .is_some_and(|why| why.contains("answered a push as a dry run")),
            "{flipped:?}"
        );
        // Neither is asked again, however much arrives.
        assert!(!clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 60)));
        assert!(!clearer
            .candidates
            .offer(&people, &arrive(&fake, &people, 61)));
        pass(600).await;
        assert_eq!(
            fake.upstream_clears().len(),
            2,
            "a bridge that broke the dry-run contract was asked again"
        );
        for setting in store.upstream_clear_settings().await.expect("read") {
            assert_eq!(
                setting.pushed, None,
                "a broken answer was recorded as a push"
            );
            assert_eq!(
                setting.last.as_ref().map(|last| last.result),
                Some(UpstreamClearResult::Unavailable)
            );
        }
        assert!(
            logs.text().contains("answered a dry run as a real push"),
            "{}",
            logs.text()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_is_logged_recorded_and_retried_after_a_growing_wait() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        fake.fail_upstream_clear_next(ChatError::Transport("bridge restarting".to_owned()));
        fake.fail_upstream_clear_next(ChatError::Status {
            status: 502,
            body: "mark-read failed".to_owned(),
        });
        let message = arrive(&fake, &agents(), 1);
        clearer.candidates.offer(&agents(), &message);
        pass(6).await;
        assert_eq!(fake.upstream_clears().len(), 1);
        let setting = &store.upstream_clear_settings().await.expect("read")[0];
        assert_eq!(setting.failures, 1);
        assert!(clearer.candidates.status(&agents()).retry_at_ms.is_some());
        // The failure is checked where it is recorded, not in a captured log. A per-thread log
        // capture loses events when another test thread registers the same callsite first
        // (`tracing` caches each callsite's interest process-wide), so under the full suite's
        // parallel load the log line was missing about half the time though the push had failed.
        // The recorded attempt is what Settings shows and what the log line is written from.
        let last = setting
            .last
            .as_ref()
            .expect("the failed attempt is recorded");
        assert!(
            last.error
                .as_deref()
                .is_some_and(|error| error.contains("bridge restarting")),
            "{last:?}"
        );
        // The first retry waits at least 30 s, not the 5 s quiet period.
        pass(25).await;
        assert_eq!(fake.upstream_clears().len(), 1, "retried before the wait");
        pass(9).await;
        assert_eq!(
            fake.upstream_clears().len(),
            2,
            "not retried after the wait"
        );
        assert_eq!(
            store.upstream_clear_settings().await.expect("read")[0].failures,
            2
        );
        // The second waits about twice as long.
        pass(50).await;
        assert_eq!(fake.upstream_clears().len(), 2, "the wait did not grow");
        pass(20).await;
        assert_eq!(fake.upstream_clears().len(), 3);
        let setting = &store.upstream_clear_settings().await.expect("read")[0];
        assert_eq!(setting.failures, 0, "a success resets the count");
        assert_eq!(
            setting.pushed.as_ref().map(|p| &p.message),
            Some(&message.id)
        );
        assert_eq!(clearer.candidates.status(&agents()), LiveStatus::default());
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_message_replaces_one_waiting_to_be_retried() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        fake.fail_upstream_clear_next(ChatError::Transport("down".to_owned()));
        clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 1));
        pass(6).await;
        let newer = arrive(&fake, &agents(), 7);
        clearer.candidates.offer(&agents(), &newer);
        pass(60).await;
        let calls = fake.upstream_clears();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[1].1, newer.id, "the retry pushed the older message");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_push_is_retried_for_its_own_message_when_an_older_one_arrived_meanwhile() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        let older = arrive(&fake, &agents(), 10);
        let newer = arrive(&fake, &agents(), 20);
        let gate = fake.hold_upstream_clears();
        fake.fail_upstream_clear_next(ChatError::Transport("bridge restarting".to_owned()));
        tokio::spawn(clearer.clone().run());
        clearer.candidates.offer(&agents(), &newer);
        pass(10).await;
        assert_eq!(
            fake.upstream_clears().len(),
            1,
            "the push should be in flight"
        );
        // A late poll delivers an older message while the push is in flight, and the push fails.
        assert!(clearer.candidates.offer(&agents(), &older));
        gate.add_permits(10);
        pass(120).await;
        let calls = fake.upstream_clears();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(
            calls[1].1, newer.id,
            "the retry pushed the older message, leaving the newer one unread"
        );
        assert_eq!(
            fake.upstream_cursor(&agents()),
            Some(newer.timestamp.clone())
        );
        assert_eq!(clearer.candidates.status(&agents()), LiveStatus::default());
    }

    #[tokio::test(start_paused = true)]
    async fn forgetting_every_channel_stops_a_waiting_push_and_every_later_one() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        pass(1).await;
        clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 1));
        assert!(clearer.candidates.status(&agents()).pending);
        // What the erase route does: the store's settings go, then the loop's.
        store.purge_everything().await.expect("purge");
        assert_eq!(clearer.candidates.forget_all(), 1);
        assert_eq!(clearer.candidates.mode(&agents()), UpstreamClearMode::Off);
        assert!(!clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 2)));
        pass(120).await;
        assert!(fake.upstream_clears().is_empty());
        assert_eq!(clearer.candidates.forget_all(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_channel_stops_until_its_setting_changes_and_a_missing_message_waits() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        // 404: the bridge no longer holds it. Not retried; the next message is pushed.
        fake.fail_upstream_clear_next(ChatError::Status {
            status: 404,
            body: "unknown message cursor".to_owned(),
        });
        clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 1));
        pass(120).await;
        assert_eq!(
            fake.upstream_clears().len(),
            1,
            "a missing message was retried"
        );
        clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 130));
        pass(60).await;
        assert_eq!(fake.upstream_clears().len(), 2);
        // 409: one thread of a space. Nothing more until the owner changes the setting.
        fake.fail_upstream_clear_next(ChatError::Status {
            status: 409,
            body: r#"{"code":"thread_channel"}"#.to_owned(),
        });
        clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 200));
        pass(60).await;
        assert_eq!(fake.upstream_clears().len(), 3);
        assert_eq!(
            clearer.candidates.status(&agents()).unavailable.as_deref(),
            Some(THREAD_CHANNEL_REASON)
        );
        assert!(!clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 300)));
        pass(600).await;
        assert_eq!(
            fake.upstream_clears().len(),
            3,
            "a refused channel was pushed again"
        );
        // Saving the setting again is the owner asking to try again.
        clearer
            .candidates
            .set_mode(&agents(), UpstreamClearMode::On, None);
        assert!(clearer
            .candidates
            .offer(&agents(), &arrive(&fake, &agents(), 900)));
        pass(60).await;
        assert_eq!(fake.upstream_clears().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_push_in_flight_never_holds_up_ingest() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let hub = Arc::new(crate::live::LiveHub::new());
        let clearer = Clearer {
            candidates: Arc::clone(hub.upstream_clear()),
            ..clearer(&fake, &store)
        };
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        let gate = fake.hold_upstream_clears();
        tokio::spawn(clearer.clone().run());
        let mut subscription = hub.subscribe(&agents(), None);
        hub.publish(&agents(), arrive(&fake, &agents(), 1));
        pass(10).await;
        assert_eq!(
            fake.upstream_clears().len(),
            1,
            "the push should be in flight"
        );
        // The provider has not answered. Publishing still lands at once, every time.
        for second in 2..50 {
            hub.publish(&agents(), arrive(&fake, &agents(), second));
        }
        let mut delivered = 0;
        while subscription.receiver.try_recv().is_ok() {
            delivered += 1;
        }
        assert_eq!(delivered, 49, "ingest waited for the provider");
        assert!(
            clearer.candidates.status(&agents()).pending,
            "the newest is waiting its turn"
        );
        gate.add_permits(10);
        pass(60).await;
        let calls = fake.upstream_clears();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[1].1, MessageId::at_time_ms(1_790_000_049_000));
    }

    #[tokio::test(start_paused = true)]
    async fn the_setting_and_the_marker_outlast_a_restart() {
        let dir = crate::testing::TempDir::new("upstream-clear-restart");
        let file = dir.path().join("vibe-talk.sqlite3");
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let pushed;
        {
            let store: Arc<dyn StateStore> = Arc::new(
                crate::store::sqlite::SqliteStore::open(&file, crate::store::Retention::default())
                    .expect("open"),
            );
            let clearer = Clearer {
                store,
                ..clearer(&fake, &Arc::new(FakeStore::new()))
            };
            on(&clearer, &agents(), UpstreamClearMode::On).await;
            pushed = arrive(&fake, &agents(), 1);
            clearer.candidates.offer(&agents(), &pushed);
            pass(10).await;
            assert!(clearer.push_due().await);
            assert_eq!(fake.upstream_clears().len(), 1);
        }
        // A restart with nothing new: the seed finds the pushed message and pushes nothing.
        let restarted = |fake: &Arc<FakeDiscord>| Clearer {
            store: Arc::new(
                crate::store::sqlite::SqliteStore::open(&file, crate::store::Retention::default())
                    .expect("reopen"),
            ),
            ..clearer(fake, &Arc::new(FakeStore::new()))
        };
        let again = restarted(&fake);
        assert_eq!(again.seed().await.expect("seed"), 1);
        assert_eq!(again.candidates.mode(&agents()), UpstreamClearMode::On);
        pass(60).await;
        assert!(
            !again.push_due().await,
            "the pushed message was pushed again"
        );
        // A message that arrived while the server was down is pushed after the next restart.
        let missed = arrive(&fake, &agents(), 100);
        let third = restarted(&fake);
        assert_eq!(third.seed().await.expect("seed"), 1);
        pass(10).await;
        assert!(third.push_due().await);
        assert_eq!(
            fake.upstream_clears().last().map(|call| &call.1),
            Some(&missed.id)
        );
        assert_ne!(pushed.id, missed.id);
    }

    #[tokio::test(start_paused = true)]
    async fn a_channel_that_is_off_offers_nothing() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        let channel = ChannelId("1111111111".to_owned());
        let id = fake.seed(&channel, "agent", "one");
        let message = fake.message(&id).expect("seeded");
        assert!(!clearer.candidates.offer(&channel, &message));
        tokio::time::advance(Duration::from_secs(120)).await;
        assert!(!clearer.push_due().await);
        assert!(fake.upstream_clears().is_empty());
        assert_eq!(
            clearer.candidates.push_now(&channel, &message),
            PushNow::Off,
            "a press pushed for a channel that is off"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_pushes_at_once_inside_the_spacing_and_the_wait_ends_when_it_is_recorded() {
        // `#253 upstream-clear-polish`. "Mark read now" skips the quiet period and the spacing
        // between pushes, and whoever waits on it wakes once the attempt is in the store.
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        let first = arrive(&fake, &agents(), 0);
        clearer.candidates.offer(&agents(), &first);
        pass(6).await;
        assert_eq!(fake.upstream_clears().len(), 1);
        // Two seconds after a push, well inside the 30 s spacing.
        let second = arrive(&fake, &agents(), 8);
        let PushNow::Queued(ticket) = clearer.candidates.push_now(&agents(), &second) else {
            panic!("the press was not queued");
        };
        let started = Instant::now();
        let settled = clearer
            .candidates
            .wait_for(&agents(), ticket, started + Duration::from_secs(45))
            .await;
        let Settled::Recorded(attempt) = settled else {
            panic!("the press settled as {settled:?}");
        };
        assert_eq!(attempt.message, second.id, "settled by another push");
        assert_eq!(attempt.result, UpstreamClearResult::Advanced);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the press waited {:?} for the spacing",
            started.elapsed()
        );
        assert_eq!(
            fake.upstream_clears(),
            vec![
                (agents(), first.id.clone(), false),
                (agents(), second.id.clone(), false)
            ]
        );
        let setting = &store.upstream_clear_settings().await.expect("read")[0];
        assert_eq!(
            setting.last.as_ref().map(|last| &last.message),
            Some(&second.id),
            "the wait ended before the attempt was recorded"
        );
        // Nothing newer: covered, and nothing is sent.
        assert_eq!(
            clearer.candidates.push_now(&agents(), &second),
            PushNow::Covered
        );
        let mut unreadable = second.clone();
        unreadable.timestamp = "yesterday".to_owned();
        assert_eq!(
            clearer.candidates.push_now(&agents(), &unreadable),
            PushNow::Unplaceable
        );
        pass(120).await;
        assert_eq!(fake.upstream_clears().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_ends_when_the_setting_changes_and_otherwise_at_its_deadline() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        // No loop running: nothing will ever record the push.
        let message = arrive(&fake, &agents(), 0);
        let PushNow::Queued(ticket) = clearer.candidates.push_now(&agents(), &message) else {
            panic!("the press was not queued");
        };
        let started = Instant::now();
        assert_eq!(
            clearer
                .candidates
                .wait_for(&agents(), ticket, started + Duration::from_secs(45))
                .await,
            Settled::Waiting,
            "settled with nothing recorded"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(45));
        for change in [UpstreamClearMode::DryRun, UpstreamClearMode::Off] {
            let candidates = Arc::clone(&clearer.candidates);
            let PushNow::Queued(ticket) = candidates.push_now(&agents(), &message) else {
                panic!("the press was not queued");
            };
            let waiting = tokio::spawn(async move {
                let started = Instant::now();
                let settled = candidates
                    .wait_for(&agents(), ticket, started + Duration::from_secs(45))
                    .await;
                (settled, started.elapsed())
            });
            pass(1).await;
            clearer.candidates.set_mode(&agents(), change, None);
            let (settled, waited) = waiting.await.expect("the wait finished");
            assert_eq!(
                settled,
                Settled::Changed,
                "set to {change:?}, the press said {settled:?}"
            );
            assert!(
                waited < Duration::from_secs(2),
                "the wait ran on for {waited:?} after it was set to {change:?}"
            );
            if change == UpstreamClearMode::DryRun {
                clearer
                    .candidates
                    .set_mode(&agents(), UpstreamClearMode::On, None);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_whose_push_fails_settles_as_that_failure_never_as_done() {
        // `#253 upstream-clear-polish`. The reviewer's case: an attempt was recorded, and it failed.
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        let message = arrive(&fake, &agents(), 0);
        fake.fail_upstream_clear_next(ChatError::Status {
            status: 502,
            body: "mark-read failed".to_owned(),
        });
        let PushNow::Queued(ticket) = clearer.candidates.push_now(&agents(), &message) else {
            panic!("the press was not queued");
        };
        let settled = clearer
            .candidates
            .wait_for(&agents(), ticket, Instant::now() + Duration::from_secs(45))
            .await;
        let Settled::Recorded(attempt) = settled else {
            panic!("the press settled as {settled:?}");
        };
        assert_eq!(attempt.result, UpstreamClearResult::Failed);
        assert_eq!(attempt.message, message.id);
        assert!(attempt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("502")));
        assert!(clearer.candidates.status(&agents()).retry_at_ms.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_behind_an_older_push_waits_past_its_success_and_says_so_when_it_fails() {
        // `#253 upstream-clear-polish`. A push of an OLDER message already on its way when the
        // press came does not answer the press: its success is not the press's, and its failure
        // leaves the pressed message waiting for the retry.
        for fails in [false, true] {
            let fake = Arc::new(FakeDiscord::new());
            fake.enable_upstream_clear();
            let store = Arc::new(FakeStore::new());
            let clearer = clearer(&fake, &store);
            on(&clearer, &agents(), UpstreamClearMode::On).await;
            let gate = fake.hold_upstream_clears();
            tokio::spawn(clearer.clone().run());
            let older = arrive(&fake, &agents(), 0);
            clearer.candidates.offer(&agents(), &older);
            pass(6).await;
            assert_eq!(
                fake.upstream_clears().len(),
                1,
                "the older push is not on its way"
            );
            let newest = arrive(&fake, &agents(), 7);
            let PushNow::Queued(ticket) = clearer.candidates.push_now(&agents(), &newest) else {
                panic!("the press was not queued");
            };
            let candidates = Arc::clone(&clearer.candidates);
            let waiting = tokio::spawn(async move {
                candidates
                    .wait_for(&agents(), ticket, Instant::now() + Duration::from_secs(45))
                    .await
            });
            if fails {
                fake.fail_upstream_clear_next(ChatError::Status {
                    status: 502,
                    body: "mark-read failed".to_owned(),
                });
            }
            gate.add_permits(1);
            pass(1).await;
            if fails {
                assert_eq!(waiting.await.expect("waited"), Settled::RetryWaiting);
                assert_eq!(
                    fake.upstream_clears().len(),
                    1,
                    "the pressed message went before the retry"
                );
                assert!(clearer.candidates.status(&agents()).pending);
                continue;
            }
            assert!(
                !waiting.is_finished(),
                "the older push's success settled the press"
            );
            gate.add_permits(1);
            let settled = waiting.await.expect("waited");
            let Settled::Recorded(attempt) = settled else {
                panic!("the press settled as {settled:?}");
            };
            assert_eq!(attempt.message, newest.id, "settled by the older push");
            assert_eq!(attempt.result, UpstreamClearResult::Advanced);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_the_provider_refuses_settles_as_unavailable() {
        let fake = Arc::new(FakeDiscord::new());
        fake.enable_upstream_clear();
        let store = Arc::new(FakeStore::new());
        let clearer = clearer(&fake, &store);
        on(&clearer, &agents(), UpstreamClearMode::On).await;
        tokio::spawn(clearer.clone().run());
        let message = arrive(&fake, &agents(), 0);
        fake.fail_upstream_clear_next(ChatError::Status {
            status: 409,
            body: r#"{"message":"thread_channel"}"#.to_owned(),
        });
        let PushNow::Queued(ticket) = clearer.candidates.push_now(&agents(), &message) else {
            panic!("the press was not queued");
        };
        let settled = clearer
            .candidates
            .wait_for(&agents(), ticket, Instant::now() + Duration::from_secs(45))
            .await;
        let Settled::Recorded(attempt) = settled else {
            panic!("the press settled as {settled:?}");
        };
        assert_eq!(attempt.result, UpstreamClearResult::Unavailable);
        assert!(clearer.candidates.status(&agents()).unavailable.is_some());
    }
}
