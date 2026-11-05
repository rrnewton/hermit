//! Each channel's most recent window, held in memory, so a catch-up can answer inside a voice
//! agent's tool limit while the chat service is slow. `#210 voice-catchup-speed`.
//!
//! # Why this exists
//!
//! `recent_activity` read every listed channel from the chat service on every call. Behind an
//! adapter that runs one provider command at a time, three channels cost three reads in series —
//! about two seconds each — so a catch-up took five to nine seconds, and the one slow command that
//! stalls for a minute takes every tool down with it. The voice runtime gives a tool fifteen
//! seconds, and a call that runs past that is answered with nothing at all. More parallelism is
//! not available: the adapter's single slot is a resource limit, not an oversight.
//!
//! What the server can do on its own is not wait for the slowest read. It keeps the newest window
//! it has read of each channel, keeps that window current from the live feed, and gives a catch-up
//! a deadline: whatever has not been read again by then is answered from the held copy, labelled
//! with its age, rather than not at all.
//!
//! See also [`crate::read_ahead`], read-aloud's separate 30-second memory of fetched messages.
//!
//! # What is held, and for how long
//!
//! For each channel, the RAW messages of the newest successful read — any
//! [`crate::ops::messages`] read, from a tool or from the page, or a refresh started here — and
//! when that read began. Every create, edit and delete the [`crate::live::LiveHub`] publishes is
//! merged in — including those that arrive while a read of it is running, see below — so a window
//! read a minute ago and fed since is as current as the feed. The
//! spoken forms and the noise marks are NOT held: they are worked out again each time a window is
//! served, exactly as for a fresh read, so a held copy can never carry a stale classification.
//!
//! Memory only. Nothing here is written to disk, it is gone when the process restarts, and it is
//! bounded by the channel allowlist times one read's size and [`JOURNAL`] pushes: every path that
//! records or merges a window is already allowlist-gated. The storage purge empties it: see
//! [`RecentWindows::clear`] and "The purge" below.
//!
//! # One read per channel at a time
//!
//! A refresh is SINGLE-FLIGHT: while one channel is being read, a second caller waits on the same
//! read instead of queueing another behind it, and the read is a detached task, so it finishes —
//! and fills the window — even when the caller that started it has given up. That is what makes
//! the retry after an abandoned call cheap: the read it would have repeated has already landed.
//!
//! # A read does not undo what arrived while it ran
//!
//! A read takes seconds, and the provider's answer is a picture of the channel at some moment
//! inside them. A message created, edited or deleted after that moment reaches this server as a
//! push, and is merged into the window then held — and a read that landed afterwards and simply
//! replaced the window would throw the push away. The window would then be served as current, from
//! a read that had just finished, while missing exactly the newest change: a created message gone,
//! a deleted one back.
//!
//! So every push is also kept, numbered, in a short per-channel journal ([`JOURNAL`] deep), and
//! every read takes a [`ReadMark`] — the journal's number — before it asks the provider. When the
//! read lands, the pushes numbered after its mark are applied again on top of what it brought. That
//! is safe whether or not the provider's answer already had them: a delete stays deleted, an edit
//! sets the text it carried, and a created message the read already brought is left as the read
//! has it, since the read is at least as new as the creation.
//!
//! # The purge
//!
//! `DELETE /api/v1/storage` erases the store, and a held window and its journal are channel text
//! too, so the purge also calls [`RecentWindows::clear`], which drops every window, every journal
//! and every failure, and forgets every refresh running. That empties THIS copy. It is one of
//! several copies of channel text in memory, and the purge does not empty all of them: the live
//! stream's replay tail and read-aloud tickets, among others, outlive it. `purge_storage` in
//! `crate::http::api` lists what it leaves.
//!
//! A read already running when that happens must not put its result back when it lands: it read
//! the channel before the owner asked for this copy to go. A [`ReadMark`] therefore also carries
//! how many purges there had been when it was taken, and [`RecentWindows::record`] keeps nothing
//! from a read marked before the newest one. That is the cutoff `crate::read_ahead` keeps as a
//! floor instant, counted instead of timed, so a read marked in the same clock tick as a purge is
//! never mistaken for one marked after it. The next catch-up finds nothing held and reads every
//! channel again, as on a fresh start; it does not wait on a refresh begun before the purge.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::chat::ChatError;
use crate::live::{LiveKind, LiveMessage};
use crate::model::{ChannelId, Message};
use crate::state::AppState;

/// How long a catch-up waits for the channels it holds a copy of. Measured catch-ups took five to
/// nine seconds against a fifteen-second tool limit; this leaves room for the speech model too.
pub const ACTIVITY_DEADLINE: Duration = Duration::from_millis(2_500);

/// How long a catch-up waits for a channel it holds NO copy of, before saying it could not be read
/// in time. Longer, because the alternative is an answer with nothing in it, and still well inside
/// the fifteen-second tool limit.
pub const COLD_DEADLINE: Duration = Duration::from_secs(8);

/// How old a window may be and still be served without reading the channel again, when a live
/// producer is feeding it: every change since the read has been merged in.
pub const REUSE_LIVE_FED: Duration = Duration::from_secs(60);

/// The same, when nothing feeds the window. Long enough that the read started when a call begins
/// is the one its first catch-up uses — measured at five to six seconds apart — and short enough
/// that a second catch-up in the same conversation reads again.
pub const REUSE_UNFED: Duration = Duration::from_secs(15);

/// How many refreshes read at once, across every channel. Four, as the catch-up's fan-out always
/// was: enough that three channels cost one round trip against a provider that can take them,
/// few enough that a long channel list does not burst a provider's rate limit.
pub const REFRESH_CONCURRENCY: usize = 4;

/// How many pushes each channel keeps, to apply again to a read that was running when they
/// arrived. The depth the live hub keeps for a page that attaches late, and far more than one
/// channel's pushes during one read: a read whose mark has fallen out of it does not replace a
/// held window at all. See the module documentation.
pub const JOURNAL: usize = crate::live::REPLAY_TAIL;

/// The four durations, gathered so a test can shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// See [`ACTIVITY_DEADLINE`].
    pub deadline: Duration,
    /// See [`COLD_DEADLINE`].
    pub cold_deadline: Duration,
    /// See [`REUSE_LIVE_FED`].
    pub reuse_live_fed: Duration,
    /// See [`REUSE_UNFED`].
    pub reuse_unfed: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            deadline: ACTIVITY_DEADLINE,
            cold_deadline: COLD_DEADLINE,
            reuse_live_fed: REUSE_LIVE_FED,
            reuse_unfed: REUSE_UNFED,
        }
    }
}

/// One channel's held window.
#[derive(Clone, Debug)]
pub struct Held {
    /// Raw messages, oldest first: never stamped, never noise-marked.
    pub messages: Vec<Message>,
    /// The size the read asked for.
    pub limit: u16,
    /// Whether `messages` is the whole channel: the read came back short, and nothing since has
    /// pushed the oldest message out.
    pub whole: bool,
    /// When the read that produced this window began. Its age is counted from here, because
    /// the window says nothing about the channel before that moment.
    ///
    /// Earlier than the read when the provider said its answer was complete only up to an earlier
    /// instant: then that instant. `#208 persistent-cache`; see [`RecentWindows::record_as_of`].
    pub read_started: Instant,
    /// When this server's read began, never moved back: what "this server read it N ago" counts
    /// from. The same as `read_started` unless the provider's `as_of` moved that back.
    pub read_began: Instant,
    /// When that read finished.
    pub read_finished: Instant,
    /// The provider's own `as_of`, RFC 3339, when it moved `read_started` back: what to say when
    /// the window is served. `None` when the provider said its answer was current.
    pub as_of: Option<String>,
}

impl Held {
    /// How old this window's picture of the channel is: since the read that produced it began, or
    /// since the provider's `as_of` when that is earlier. The window may know nothing of the
    /// channel after that moment. How long ago THIS SERVER read it counts from
    /// [`Held::read_began`].
    #[must_use]
    pub fn age(&self) -> Duration {
        self.read_started.elapsed()
    }

    /// How long ago that read finished: the age that decides whether to read again. A read that
    /// waited a minute for the provider's slot is not a minute stale when it lands.
    #[must_use]
    pub fn since_finished(&self) -> Duration {
        self.read_finished.elapsed()
    }
}

/// What this server knows about one channel right now.
#[derive(Debug, Default)]
pub struct Snapshot {
    /// The held window, if any read has succeeded.
    pub held: Option<Held>,
    /// Whether a refresh is running.
    pub refreshing: bool,
    /// Why the newest refresh failed, when it did and no read has succeeded since.
    pub failure: Option<ChatError>,
}

/// Where a read of one channel began: when, how many pushes that channel had had, and how many
/// purges there had been. Taken with [`RecentWindows::mark`] BEFORE the read asks the provider,
/// and handed back to [`RecentWindows::record`] with what it read, so the pushes that arrived
/// meanwhile are applied again rather than lost, and a purge that came meanwhile is not undone.
#[derive(Clone, Copy, Debug)]
pub struct ReadMark {
    started: Instant,
    pushes: u64,
    purges: u64,
}

impl ReadMark {
    /// When the read began.
    #[must_use]
    pub fn started(&self) -> Instant {
        self.started
    }
}

#[derive(Default)]
struct Slot {
    held: Option<Held>,
    /// The running refresh, and its number, so only that refresh can clear it.
    refreshing: Option<(u64, watch::Receiver<bool>)>,
    /// The newest refresh failure and when its read began. Cleared by a later success.
    failure: Option<(ChatError, Instant)>,
    /// How many pushes this channel has had: the number of the newest one.
    pushes: u64,
    /// The newest [`JOURNAL`] of them, with their numbers, oldest first.
    journal: VecDeque<(u64, LiveMessage)>,
}

impl Slot {
    /// The pushes after `mark`, oldest first, or `None` when some of them are no longer kept.
    fn pushes_since(&self, mark: ReadMark) -> Option<Vec<LiveMessage>> {
        let kept_from = self
            .journal
            .front()
            .map_or(self.pushes + 1, |(number, _)| *number);
        if kept_from > mark.pushes + 1 {
            return None;
        }
        Some(
            self.journal
                .iter()
                .filter(|(number, _)| *number > mark.pushes)
                .map(|(_, live)| live.clone())
                .collect(),
        )
    }
}

/// Everything behind the one lock.
#[derive(Default)]
struct Windows {
    slots: BTreeMap<ChannelId, Slot>,
    /// How many times [`RecentWindows::clear`] has run. A read marked before the newest keeps
    /// nothing.
    purges: u64,
}

/// Every channel's held window, and the refreshes running for them.
pub struct RecentWindows {
    timing: Timing,
    windows: Mutex<Windows>,
    next_refresh: std::sync::atomic::AtomicU64,
    /// [`REFRESH_CONCURRENCY`] permits; a refresh holds one while it reads.
    reading: tokio::sync::Semaphore,
}

impl Default for RecentWindows {
    fn default() -> Self {
        Self::with_timing(Timing::default())
    }
}

impl RecentWindows {
    /// Nothing held, with the production durations.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Nothing held, with these durations. For tests.
    #[must_use]
    pub fn with_timing(timing: Timing) -> Self {
        Self {
            timing,
            windows: Mutex::new(Windows::default()),
            next_refresh: std::sync::atomic::AtomicU64::new(1),
            reading: tokio::sync::Semaphore::new(REFRESH_CONCURRENCY),
        }
    }

    /// The durations in force.
    #[must_use]
    pub fn timing(&self) -> Timing {
        self.timing
    }

    fn windows(&self) -> std::sync::MutexGuard<'_, Windows> {
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Where a read of `channel` that starts now begins. Take it BEFORE asking the provider.
    #[must_use]
    pub fn mark(&self, channel: &ChannelId) -> ReadMark {
        let windows = self.windows();
        ReadMark {
            started: Instant::now(),
            pushes: windows.slots.get(channel).map_or(0, |slot| slot.pushes),
            purges: windows.purges,
        }
    }

    /// Keep the result of a successful read of `channel`, which began at `mark`, with every push
    /// that arrived since applied again on top of it.
    ///
    /// A read marked before the newest purge is dropped: the owner asked for everything read until
    /// then to go, and that includes what this read brought. A read that began before the one
    /// already held is dropped: two reads overlapping, the one that started later saw at least as
    /// much of the channel, whichever finished first. So is a read whose pushes are no longer all
    /// kept, when a window is held: that window has had every push merged as it came, and the read
    /// cannot be made to agree with them.
    pub fn record(&self, channel: &ChannelId, messages: &[Message], limit: u16, mark: ReadMark) {
        self.record_as_of(channel, messages, limit, mark, None);
    }

    /// [`RecentWindows::record`], for a read whose provider said its answer was complete only up
    /// to `as_of`, an RFC 3339 instant. `#208 persistent-cache`, milestone `bridge-recent-flat`.
    ///
    /// The window then knows nothing of the channel after `as_of`, so that is when it starts: its
    /// age is counted from there, and it loses to a window held from a read whose picture is newer,
    /// whichever read began first. A bridge serving a copy it could not bring up to date is the
    /// case. An `as_of` that does not parse, or is no earlier than the read, changes nothing.
    pub fn record_as_of(
        &self,
        channel: &ChannelId,
        messages: &[Message],
        limit: u16,
        mark: ReadMark,
        as_of: Option<&str>,
    ) {
        let started = backdated(mark.started, as_of);
        let mut windows = self.windows();
        if mark.purges != windows.purges {
            return;
        }
        let slot = windows.slots.entry(channel.clone()).or_default();
        // The read succeeded after the failure, however old its picture.
        if slot
            .failure
            .as_ref()
            .is_some_and(|(_, failed)| *failed <= mark.started)
        {
            slot.failure = None;
        }
        if slot
            .held
            .as_ref()
            .is_some_and(|held| held.read_started > started)
        {
            return;
        }
        let since = slot.pushes_since(mark);
        if since.is_none() && slot.held.is_some() {
            tracing::warn!(
                channel = %channel,
                "a channel read was not kept: more pushes arrived while it ran than are journalled"
            );
            return;
        }
        let mut held = Held {
            messages: messages.to_vec(),
            limit,
            whole: messages.len() < usize::from(limit),
            read_started: started,
            read_began: mark.started,
            read_finished: Instant::now(),
            as_of: as_of.filter(|_| started < mark.started).map(str::to_owned),
        };
        for live in since.unwrap_or_default() {
            apply(&mut held, &live, Applying::AgainOnARead);
        }
        slot.held = Some(held);
    }

    fn note_failure(&self, channel: &ChannelId, error: ChatError, mark: ReadMark) {
        let started = mark.started;
        let mut windows = self.windows();
        if mark.purges != windows.purges {
            return;
        }
        let slot = windows.slots.entry(channel.clone()).or_default();
        if slot
            .held
            .as_ref()
            .is_some_and(|held| held.read_started > started)
        {
            return;
        }
        slot.failure = Some((error, started));
    }

    /// Drop every held window, every journalled push and every failure, and forget every refresh
    /// running. For the storage purge, so that this copy of channel text does not outlive it,
    /// though it is not the only copy held in memory.
    ///
    /// A read marked before this keeps nothing when it lands, so a refresh or a page read already
    /// running cannot put back what it read. A refresh still running is forgotten rather than
    /// joined, so the next catch-up reads afresh instead of waiting on a read whose result will be
    /// dropped. See "The purge" in the module documentation.
    pub fn clear(&self) {
        let mut windows = self.windows();
        windows.slots.clear();
        windows.purges += 1;
    }

    /// Whether no channel text is held at all: no window, and no journalled push. What
    /// [`RecentWindows::clear`] leaves.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows()
            .slots
            .values()
            .all(|slot| slot.held.is_none() && slot.journal.is_empty())
    }

    /// Fold one published change into `channel`'s window, when one is held, and journal it for any
    /// read of the channel still running.
    ///
    /// A push never STARTS a window: one message says nothing about what came before it. It is
    /// journalled all the same, so the read that does start one keeps it.
    pub fn merge(&self, channel: &ChannelId, live: &LiveMessage) {
        let mut windows = self.windows();
        let slot = windows.slots.entry(channel.clone()).or_default();
        slot.pushes += 1;
        slot.journal.push_back((slot.pushes, live.clone()));
        while slot.journal.len() > JOURNAL {
            slot.journal.pop_front();
        }
        if let Some(held) = slot.held.as_mut() {
            apply(held, live, Applying::AsItArrives);
        }
    }

    /// What is known about `channel` right now.
    #[must_use]
    pub fn snapshot(&self, channel: &ChannelId) -> Snapshot {
        let windows = self.windows();
        let Some(slot) = windows.slots.get(channel) else {
            return Snapshot::default();
        };
        Snapshot {
            held: slot.held.clone(),
            refreshing: slot.refreshing.is_some(),
            failure: slot.failure.as_ref().map(|(error, _)| error.clone()),
        }
    }

    /// Whether `channel` holds a window at all.
    #[must_use]
    pub fn holds(&self, channel: &ChannelId) -> bool {
        self.windows()
            .slots
            .get(channel)
            .is_some_and(|slot| slot.held.is_some())
    }

    /// Whether the live feed has reported an edit or a delete in `channel` since `mark` was taken,
    /// or more pushes than are journalled, so that it cannot be told. `#208 persistent-cache`:
    /// what may explain two reads of the channel either side of it disagreeing; see
    /// [`crate::recent_view`].
    ///
    /// Across a purge it cannot be told either: [`RecentWindows::clear`] dropped the journal, and
    /// the count of pushes started again from nothing.
    #[must_use]
    pub fn changed_since(&self, channel: &ChannelId, mark: ReadMark) -> bool {
        let windows = self.windows();
        mark.purges != windows.purges
            || windows.slots.get(channel).is_some_and(|slot| {
                slot.pushes_since(mark)
                    .is_none_or(|pushes| pushes.iter().any(|live| live.kind != LiveKind::Create))
            })
    }

    /// Whether `channel`'s window can answer a read of `scan` messages without asking the chat
    /// service: its read finished within `reuse`, and it reaches as deep as asked or holds the
    /// whole channel.
    #[must_use]
    pub fn reusable(&self, channel: &ChannelId, scan: u16, reuse: Duration) -> bool {
        self.windows()
            .slots
            .get(channel)
            .and_then(|slot| slot.held.as_ref())
            .is_some_and(|held| {
                held.since_finished() <= reuse && (held.limit >= scan || held.whole)
            })
    }

    /// Join the refresh running for `channel`, or claim the right to start one.
    ///
    /// The receiver turns `true` when the refresh is over, however it ended. A claim is returned
    /// only to the caller that must run the read; every other caller is handed the same receiver.
    fn claim(self: &Arc<Self>, channel: &ChannelId) -> (watch::Receiver<bool>, Option<Claim>) {
        let mut windows = self.windows();
        let slot = windows.slots.entry(channel.clone()).or_default();
        if let Some((_, running)) = &slot.refreshing {
            return (running.clone(), None);
        }
        let number = self
            .next_refresh
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (done, running) = watch::channel(false);
        slot.refreshing = Some((number, running.clone()));
        let claim = Claim {
            windows: Arc::clone(self),
            channel: channel.clone(),
            number,
            done: Some(done),
        };
        (running, Some(claim))
    }

    fn finish_refresh(&self, channel: &ChannelId, number: u64) {
        let mut windows = self.windows();
        if let Some(slot) = windows.slots.get_mut(channel) {
            if slot
                .refreshing
                .as_ref()
                .is_some_and(|(running, _)| *running == number)
            {
                slot.refreshing = None;
            }
        }
    }
}

/// The right, and the duty, to run one channel's refresh. Dropping it unfinished — a task torn
/// down with the runtime — still clears the running mark, so a channel cannot be left waiting on
/// a refresh that will never end.
struct Claim {
    windows: Arc<RecentWindows>,
    channel: ChannelId,
    number: u64,
    done: Option<watch::Sender<bool>>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.windows.finish_refresh(&self.channel, self.number);
        if let Some(done) = self.done.take() {
            let _ = done.send(true);
        }
    }
}

/// Whether a push is being applied as it arrives, or again on top of a read that was running when
/// it arrived.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Applying {
    AsItArrives,
    AgainOnARead,
}

/// When a read that began at `started` saw the channel, given the provider's `as_of`: the earlier
/// of the two. The provider's clock is wall time, so the gap from now to `as_of` is taken off now.
fn backdated(started: Instant, as_of: Option<&str>) -> Instant {
    let Some(as_of_ms) = as_of.and_then(crate::clock::instant_ms) else {
        return started;
    };
    let lag_ms = jiff::Timestamp::now().as_millisecond() - as_of_ms;
    let Ok(lag_ms) = u64::try_from(lag_ms) else {
        return started;
    };
    Instant::now()
        .checked_sub(Duration::from_millis(lag_ms))
        .map_or(started, |at| at.min(started))
}

/// Fold one push into a window.
fn apply(held: &mut Held, live: &LiveMessage, applying: Applying) {
    match live.kind {
        LiveKind::Delete => held.messages.retain(|m| m.id != live.message.id),
        LiveKind::Update => {
            if let Some(at) = held.messages.iter().position(|m| m.id == live.message.id) {
                held.messages[at] = merged(&held.messages[at], &live.message);
            }
        }
        // The read is at least as new as the creation, so its copy of a message it brought — the
        // text after a later edit whose push is still on its way, say — is the one to keep.
        LiveKind::Create
            if applying == Applying::AgainOnARead
                && held.messages.iter().any(|m| m.id == live.message.id) => {}
        LiveKind::Create => insert(held, &live.message),
    }
}

/// The message a live edit leaves behind: the edit, keeping what an echo cannot see.
///
/// A live copy carries no reactions and may carry no thread; neither absence means "removed", so
/// the held copy's survive. See [`Message::reactions`].
fn merged(held: &Message, edit: &Message) -> Message {
    let mut next = edit.clone();
    if next.reactions.is_none() {
        next.reactions.clone_from(&held.reactions);
    }
    if next.thread.is_none() {
        next.thread.clone_from(&held.thread);
    }
    next
}

/// When a message was created, for placing it in a window: from its id where that carries a time,
/// otherwise from its timestamp.
fn created_ms(message: &Message) -> Option<i64> {
    message
        .id
        .created_at_ms()
        .or_else(|| crate::clock::instant_ms(&message.timestamp))
}

/// Place one created message in a window, oldest first.
fn insert(held: &mut Held, message: &Message) {
    if let Some(at) = held.messages.iter().position(|m| m.id == message.id) {
        // An echo of a message the read already brought.
        held.messages[at] = merged(&held.messages[at], message);
        return;
    }
    let position = match created_ms(message) {
        Some(at) => held
            .messages
            .iter()
            .rposition(|m| created_ms(m).is_some_and(|ms| ms <= at))
            .map_or(0, |found| found + 1),
        None => held.messages.len(),
    };
    // Older than the oldest message of a window that is not the whole channel: it belongs to a
    // stretch the window never read, and keeping it would make the window claim that stretch.
    if position == 0 && !held.whole && !held.messages.is_empty() {
        return;
    }
    held.messages.insert(position, message.clone());
    if held.messages.len() > usize::from(held.limit) {
        held.messages.remove(0);
        held.whole = false;
    }
}

/// How young `channel`'s window must be to be served without reading it again.
#[must_use]
pub fn reuse_for(state: &AppState, channel: &ChannelId) -> Duration {
    let timing = state.live.recent().timing();
    let polled = state
        .providers
        .key_for(channel)
        .as_deref()
        .and_then(|key| state.providers.entry(key))
        .is_some_and(|entry| entry.live_poll_seconds > 0);
    if polled || state.config.ingest.enabled() {
        timing.reuse_live_fed
    } else {
        timing.reuse_unfed
    }
}

/// Start reading `channel`'s newest `limit` messages, or join the read already running.
///
/// The read is a detached task: it ends, and records what it read, whatever happens to the
/// caller. The receiver turns `true` when it is over.
pub fn refresh(state: &AppState, channel: &ChannelId, limit: u16) -> watch::Receiver<bool> {
    let windows = Arc::clone(state.live.recent());
    let (done, claim) = windows.claim(channel);
    if let Some(claim) = claim {
        let state = state.clone();
        tokio::spawn(async move {
            // Begun when asked for, not when a permit came free: the window is no newer than that,
            // and every push from then on is applied to it again. See the module documentation.
            let mark = claim.windows.mark(&claim.channel);
            let started = mark.started();
            let read = match claim.windows.reading.acquire().await {
                Ok(_permit) => crate::recent_view::read(&state, &claim.channel, limit).await,
                Err(_) => Err(ChatError::Refused("the server is shutting down".to_owned())),
            };
            let millis = started.elapsed().as_millis();
            match read {
                Ok(read) => {
                    tracing::info!(
                        channel = %claim.channel,
                        returned = read.messages.len(),
                        millis,
                        stale = read.as_of.is_some(),
                        "recent window refreshed"
                    );
                    claim.windows.record_as_of(
                        &claim.channel,
                        &read.messages,
                        limit,
                        mark,
                        read.as_of.as_deref(),
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        channel = %claim.channel,
                        millis,
                        %error,
                        "recent window refresh failed"
                    );
                    claim.windows.note_failure(&claim.channel, error, mark);
                }
            }
            drop(claim);
        });
    }
    done
}

/// Start reading every listed channel that a catch-up would read again, and return at once with
/// how many are being read.
///
/// For the moment a voice call is being set up: the greeting takes five to six seconds before
/// the agent can ask anything, and that is time for the reads its first catch-up will want.
/// Single-flight like every refresh, and a window young enough to reuse is left alone, so a burst
/// of call starts does not stack reads.
pub fn warm(state: &AppState) -> usize {
    let scan = crate::ops::activity_scan(state, crate::ops::DEFAULT_ACTIVITY_LINES);
    let recent = state.live.recent();
    let mut reading = 0;
    for channel in state.all_channels() {
        if recent.reusable(&channel.id, scan, reuse_for(state, &channel.id)) {
            continue;
        }
        drop(refresh(state, &channel.id, scan));
        reading += 1;
    }
    reading
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageId, UserId};

    fn message(id: u64, text: &str) -> Message {
        Message {
            thread: None,
            id: MessageId(id.to_string()),
            channel_id: ChannelId("1".to_owned()),
            author: "agent".to_owned(),
            author_id: UserId("9".to_owned()),
            author_is_bot: false,
            timestamp: String::new(),
            spoken_time: String::new(),
            reply_to: None,
            content: text.to_owned(),
            spoken_content: String::new(),
            content_html: String::new(),
            noise: false,
            reactions: None,
            heading: None,
        }
    }

    fn live(kind: LiveKind, message: Message) -> LiveMessage {
        LiveMessage {
            event_id: message.id.as_str().to_owned(),
            kind,
            historical: false,
            message,
            self_posted: false,
        }
    }

    fn ids(windows: &RecentWindows, channel: &ChannelId) -> Vec<String> {
        windows
            .snapshot(channel)
            .held
            .expect("held")
            .messages
            .iter()
            .map(|m| m.id.as_str().to_owned())
            .collect()
    }

    // Snowflake ids a thousand apart in their time bits, so their order is their creation order.
    const BASE: u64 = 1 << 40;
    fn id(n: u64) -> u64 {
        BASE + (n << 22)
    }

    #[tokio::test]
    async fn pushes_keep_a_held_window_current_and_never_start_one() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        windows.merge(&channel, &live(LiveKind::Create, message(id(1), "early")));
        assert!(!windows.holds(&channel), "a push started a window");

        let read = [message(id(1), "one"), message(id(2), "two")];
        windows.record(&channel, &read, 3, windows.mark(&channel));
        assert!(windows.snapshot(&channel).held.expect("held").whole);

        windows.merge(&channel, &live(LiveKind::Create, message(id(3), "three")));
        windows.merge(
            &channel,
            &live(LiveKind::Update, message(id(2), "two, edited")),
        );
        windows.merge(&channel, &live(LiveKind::Delete, message(id(1), "")));
        let held = windows.snapshot(&channel).held.expect("held");
        assert_eq!(
            held.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["two, edited", "three"]
        );

        // Two more push the window past its size: the oldest goes, and it is no longer whole.
        windows.merge(&channel, &live(LiveKind::Create, message(id(4), "four")));
        windows.merge(&channel, &live(LiveKind::Create, message(id(5), "five")));
        assert_eq!(
            ids(&windows, &channel),
            [id(3), id(4), id(5)].map(|n| n.to_string())
        );
        assert!(!windows.snapshot(&channel).held.expect("held").whole);

        // A late message older than a partial window's start is kept out; one inside it is placed.
        windows.merge(&channel, &live(LiveKind::Create, message(id(2), "late")));
        windows.merge(
            &channel,
            &live(LiveKind::Create, message(id(4) + 1, "between")),
        );
        assert_eq!(
            ids(&windows, &channel),
            [id(4), id(4) + 1, id(5)].map(|n| n.to_string())
        );
    }

    #[tokio::test]
    async fn an_echo_keeps_the_reactions_the_read_brought() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let mut read = message(id(1), "one");
        read.reactions = Some(Vec::new());
        windows.record(&channel, &[read], 50, windows.mark(&channel));
        windows.merge(&channel, &live(LiveKind::Create, message(id(1), "one")));
        windows.merge(&channel, &live(LiveKind::Update, message(id(1), "one!")));
        let held = windows.snapshot(&channel).held.expect("held");
        assert_eq!(held.messages.len(), 1);
        assert_eq!(held.messages[0].content, "one!");
        assert_eq!(held.messages[0].reactions, Some(Vec::new()));
    }

    #[tokio::test(start_paused = true)]
    async fn an_older_read_finishing_late_does_not_replace_a_newer_one() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let early = windows.mark(&channel);
        tokio::time::sleep(Duration::from_secs(1)).await;
        windows.record(
            &channel,
            &[message(id(2), "newer")],
            50,
            windows.mark(&channel),
        );
        windows.record(&channel, &[message(id(1), "older")], 50, early);
        assert_eq!(ids(&windows, &channel), [id(2).to_string()]);

        windows.note_failure(&channel, ChatError::Transport("late".to_owned()), early);
        assert!(
            windows.snapshot(&channel).failure.is_none(),
            "a failure of a read older than the held window is not news"
        );
    }

    fn contents(windows: &RecentWindows, channel: &ChannelId) -> Vec<String> {
        windows
            .snapshot(channel)
            .held
            .expect("held")
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect()
    }

    /// The provider answers with the channel as it stood when the read began; the pushes that
    /// arrived while it ran are applied again, rather than undone by the read landing.
    #[tokio::test]
    async fn the_pushes_that_arrive_while_a_read_runs_are_applied_to_what_it_brings() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let before = [message(id(1), "one"), message(id(2), "about to go")];
        windows.record(&channel, &before, 50, windows.mark(&channel));

        let mark = windows.mark(&channel);
        windows.merge(
            &channel,
            &live(LiveKind::Create, message(id(3), "pushed mid read")),
        );
        windows.merge(&channel, &live(LiveKind::Delete, message(id(2), "")));
        windows.merge(
            &channel,
            &live(LiveKind::Update, message(id(1), "one, edited")),
        );
        windows.merge(&channel, &live(LiveKind::Create, message(id(4), "four")));
        // The read's picture predates three of the pushes, and already has the fourth message
        // with an edit whose push has not arrived yet.
        let read = [
            message(id(1), "one"),
            message(id(2), "about to go"),
            message(id(4), "four, edited"),
        ];
        windows.record(&channel, &read, 50, mark);

        assert_eq!(
            contents(&windows, &channel),
            ["one, edited", "pushed mid read", "four, edited"]
        );
        // And a read whose mark comes after every push applies none of them.
        let later = windows.mark(&channel);
        windows.record(&channel, &read, 50, later);
        assert_eq!(
            contents(&windows, &channel),
            ["one", "about to go", "four, edited"]
        );
    }

    /// A push never starts a window, but the read that does start one keeps a push that arrived
    /// while it ran.
    #[tokio::test]
    async fn the_first_read_of_a_channel_keeps_a_push_that_arrived_while_it_ran() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let mark = windows.mark(&channel);
        windows.merge(&channel, &live(LiveKind::Create, message(id(2), "pushed")));
        assert!(!windows.holds(&channel));
        windows.record(&channel, &[message(id(1), "read")], 50, mark);
        assert_eq!(contents(&windows, &channel), ["read", "pushed"]);
    }

    /// More pushes during one read than the journal keeps: the read cannot be made to agree with
    /// them, so a held window — which had every one merged as it came — is kept instead.
    #[tokio::test]
    async fn a_read_whose_pushes_are_no_longer_all_kept_does_not_replace_a_held_window() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        windows.record(
            &channel,
            &[message(id(1), "one")],
            3,
            windows.mark(&channel),
        );
        let mark = windows.mark(&channel);
        let pushed = u64::try_from(JOURNAL).expect("small") + 2;
        for n in 2..=pushed {
            windows.merge(&channel, &live(LiveKind::Create, message(id(n), "pushed")));
        }
        let fed = ids(&windows, &channel);
        windows.record(&channel, &[message(id(1), "stale")], 3, mark);
        assert_eq!(
            ids(&windows, &channel),
            fed,
            "a stale read replaced a fed window"
        );

        // A journal that still reaches back to the mark is applied in full.
        let mark = windows.mark(&channel);
        windows.merge(
            &channel,
            &live(LiveKind::Create, message(id(pushed + 1), "newest")),
        );
        windows.record(&channel, &[message(id(pushed), "read")], 3, mark);
        assert_eq!(contents(&windows, &channel), ["read", "newest"]);
    }

    /// `#208 persistent-cache`. A provider that answered from a copy it could not bring up to
    /// date says when that copy was complete; the window starts then, and loses to a window whose
    /// picture is newer however the reads were ordered.
    #[tokio::test(start_paused = true)]
    async fn a_window_read_from_an_older_copy_starts_when_that_copy_was_complete() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let ten_minutes_ago =
            crate::clock::iso_from_ms(jiff::Timestamp::now().as_millisecond() - 600_000);
        windows.record_as_of(
            &channel,
            &[message(id(1), "copy")],
            50,
            windows.mark(&channel),
            Some(&ten_minutes_ago),
        );
        let held = windows.snapshot(&channel).held.expect("held");
        assert!(held.age() >= Duration::from_secs(590), "{:?}", held.age());
        assert!(held.since_finished() < Duration::from_secs(1));
        // This server read it just now, whatever the copy's age.
        assert!(held.read_began.elapsed() < Duration::from_secs(1));
        assert_eq!(held.as_of.as_deref(), Some(ten_minutes_ago.as_str()));

        // A current read replaces it and says nothing about a copy.
        windows.record(
            &channel,
            &[message(id(2), "current")],
            50,
            windows.mark(&channel),
        );
        let held = windows.snapshot(&channel).held.expect("held");
        assert_eq!(held.as_of, None);
        assert!(held.age() < Duration::from_secs(1));

        // An older copy read afterwards does not replace a window whose picture is newer.
        windows.record_as_of(
            &channel,
            &[message(id(1), "copy")],
            50,
            windows.mark(&channel),
            Some(&ten_minutes_ago),
        );
        assert_eq!(ids(&windows, &channel), [id(2).to_string()]);

        // An as_of no earlier than the read, or one that does not parse, changes nothing.
        let in_a_minute =
            crate::clock::iso_from_ms(jiff::Timestamp::now().as_millisecond() + 60_000);
        for as_of in [in_a_minute.as_str(), "seven"] {
            windows.record_as_of(
                &channel,
                &[message(id(3), "three")],
                50,
                windows.mark(&channel),
                Some(as_of),
            );
            let held = windows.snapshot(&channel).held.expect("held");
            assert_eq!(held.as_of, None, "{as_of}");
            assert!(held.age() < Duration::from_secs(1), "{as_of}");
        }
    }

    /// `#208 persistent-cache`. A read that succeeded after a failure clears it, however old the
    /// copy it brought: the read began after the failure even when its `as_of` is earlier.
    #[tokio::test(start_paused = true)]
    async fn a_read_from_an_older_copy_still_clears_a_failure_before_it() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let failed = windows.mark(&channel);
        windows.note_failure(&channel, ChatError::Transport("down".to_owned()), failed);
        assert!(windows.snapshot(&channel).failure.is_some());
        tokio::time::advance(Duration::from_secs(5)).await;
        let ten_minutes_ago =
            crate::clock::iso_from_ms(jiff::Timestamp::now().as_millisecond() - 600_000);
        windows.record_as_of(
            &channel,
            &[message(id(1), "copy")],
            50,
            windows.mark(&channel),
            Some(&ten_minutes_ago),
        );
        let known = windows.snapshot(&channel);
        let held = known.held.expect("held");
        assert!(
            held.read_started < failed.started(),
            "the copy is older than the failure"
        );
        assert!(known.failure.is_none(), "{:?}", known.failure);
    }

    /// `#208 persistent-cache`. What the recent-view shadow asks of the journal: whether an edit
    /// or a delete came after a mark. A creation does not count; pushes the journal no longer holds
    /// might have been either, so they do.
    #[tokio::test]
    async fn an_edit_or_delete_after_a_mark_is_told_and_a_creation_is_not() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let mark = windows.mark(&channel);
        assert!(!windows.changed_since(&channel, mark));
        windows.merge(&channel, &live(LiveKind::Create, message(id(1), "new")));
        assert!(!windows.changed_since(&channel, mark));
        windows.merge(&channel, &live(LiveKind::Update, message(id(1), "edited")));
        assert!(windows.changed_since(&channel, mark));
        let later = windows.mark(&channel);
        assert!(!windows.changed_since(&channel, later));
        windows.merge(&channel, &live(LiveKind::Delete, message(id(1), "")));
        assert!(windows.changed_since(&channel, later));

        let other = ChannelId("2".to_owned());
        let mark = windows.mark(&other);
        for n in 0..=u64::try_from(JOURNAL).expect("small") {
            windows.merge(&other, &live(LiveKind::Create, message(id(n), "new")));
        }
        assert!(
            windows.changed_since(&other, mark),
            "fell out of the journal"
        );
    }

    /// `#210 voice-catchup-speed`'s purge drops the journal and starts the push count again, so
    /// what came after a mark taken before it cannot be told: it may have been an edit. A mark
    /// taken after it is told as before.
    #[tokio::test]
    async fn what_came_after_a_mark_taken_before_the_purge_cannot_be_told() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        windows.merge(&channel, &live(LiveKind::Create, message(id(1), "new")));
        let before = windows.mark(&channel);
        windows.clear();
        assert!(windows.changed_since(&channel, before), "no slot at all");
        windows.merge(&channel, &live(LiveKind::Create, message(id(2), "new")));
        assert!(
            windows.changed_since(&channel, before),
            "a fresh journal counted from nothing"
        );
        let after = windows.mark(&channel);
        assert!(!windows.changed_since(&channel, after));
        windows.merge(&channel, &live(LiveKind::Update, message(id(2), "edited")));
        assert!(windows.changed_since(&channel, after));
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_is_reused_only_while_young_and_deep_enough() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let full: Vec<Message> = (1..=20).map(|n| message(id(n), "m")).collect();
        windows.record(&channel, &full, 20, windows.mark(&channel));
        let reuse = Duration::from_secs(15);
        assert!(windows.reusable(&channel, 20, reuse));
        assert!(
            !windows.reusable(&channel, 50, reuse),
            "a full window of 20 cannot answer a read of 50"
        );
        tokio::time::sleep(Duration::from_secs(16)).await;
        assert!(!windows.reusable(&channel, 20, reuse));
    }

    /// The storage purge leaves no channel text in this memory: no window, no journalled push, no
    /// failure, on any channel.
    #[tokio::test]
    async fn the_purge_drops_every_window_and_every_journalled_push() {
        let windows = RecentWindows::new();
        let one = ChannelId("1".to_owned());
        let two = ChannelId("2".to_owned());
        let three = ChannelId("3".to_owned());
        windows.record(&one, &[message(id(1), "one")], 50, windows.mark(&one));
        windows.merge(&one, &live(LiveKind::Create, message(id(2), "pushed")));
        // Journalled with no window to merge into.
        windows.merge(&two, &live(LiveKind::Create, message(id(3), "journalled")));
        windows.note_failure(
            &three,
            ChatError::Transport("down".to_owned()),
            windows.mark(&three),
        );
        assert!(!windows.is_empty());

        windows.clear();
        assert!(windows.is_empty(), "channel text outlived the purge");
        for channel in [&one, &two, &three] {
            let known = windows.snapshot(channel);
            assert!(known.held.is_none(), "{channel}");
            assert!(known.failure.is_none(), "{channel}");
            assert!(!known.refreshing, "{channel}");
        }

        // A push journalled before the purge is not applied to the first read after it.
        let mark = windows.mark(&two);
        windows.record(&two, &[message(id(1), "read")], 50, mark);
        assert_eq!(contents(&windows, &two), ["read"]);
    }

    /// A read marked before the purge lands after it with what it read then. It keeps nothing,
    /// and nor does its failure; a read marked after the purge, even in the same clock tick, is
    /// kept.
    #[tokio::test(start_paused = true)]
    async fn a_read_marked_before_the_purge_keeps_nothing_when_it_lands() {
        let windows = RecentWindows::new();
        let channel = ChannelId("1".to_owned());
        let before = windows.mark(&channel);
        windows.merge(&channel, &live(LiveKind::Create, message(id(2), "pushed")));
        windows.clear();
        windows.record(&channel, &[message(id(1), "read")], 50, before);
        assert!(windows.is_empty(), "a read begun before the purge undid it");
        windows.note_failure(&channel, ChatError::Transport("late".to_owned()), before);
        assert!(windows.snapshot(&channel).failure.is_none());

        // The paused clock has not moved: same instant as the purge, and still after it.
        let after = windows.mark(&channel);
        assert_eq!(after.started(), before.started());
        windows.record(&channel, &[message(id(1), "read")], 50, after);
        assert_eq!(contents(&windows, &channel), ["read"]);
    }
}
