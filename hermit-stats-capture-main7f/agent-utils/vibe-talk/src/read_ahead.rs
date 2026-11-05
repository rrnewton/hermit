//! What speech preparation keeps between requests, so a tap does not wait for a fetch the page
//! has only just made. `#191 read-aloud-first-lag`.
//!
//! The measured first read of a session took five seconds from tap to sound, and 1.78 s of that
//! was the tap waiting for `POST /api/v1/channels/{id}/speech/prepare`. Nearly all of the prepare
//! was one chat window fetch, which re-read the window the page itself had fetched a few seconds
//! earlier. In a threaded view it was worse: the prepare fetched each message on its own, one
//! after the other. And the provider warm-up waited for all of it before it started.
//!
//! This module holds three things that remove those waits:
//!
//! - [`ReadAhead::remember`] and [`ReadAhead::recall`]: every message this server fetches for a
//!   page (`ops::messages`, `ops::page`, `ops::timeline`) is kept for [`RECENT_TTL`], and
//!   preparation looks there first. Bounded by [`MAX_RECENT`].
//! - [`ReadAhead::window_turn`]: when several preparations for one channel miss together, one of
//!   them fetches the window and the others wait for it, rather than queueing a second identical
//!   fetch behind the first at the chat adapter. The summary route shares the same turn
//!   (`#208 persistent-cache`), so a page asking for twenty summaries at once costs one fetch, and
//!   a summary asked for right after the page's own read is answered from here with none.
//! - [`ReadAhead::start_warm_up`]: the provider warm-up starts at the top of a preparation and is
//!   never awaited by it. At most one runs at a time, for at most [`WARM_UP_BOUND`].
//!
//! [`messages_for_speech`] puts the first two together for the prepare route.
//!
//! See also [`crate::recent`], the voice catch-up's separate memory of each channel's window.
//!
//! # What is kept, and for how long
//!
//! Message text from allowlisted channels, in memory only, for thirty seconds from the moment the
//! fetch that read it began. That is the same text the page was handed in the same thirty seconds.
//! Nothing here reaches the store, and a restart forgets it. A read still checks the channel
//! allowlist first (the prepare route does that before it calls [`messages_for_speech`]), so a
//! channel taken off the list cannot be read from here either.
//!
//! # Edits, deletions and the purge
//!
//! An update or delete pushed by an adapter, and a delta that reports a deletion, drop the message
//! at once ([`ReadAhead::forget`]); the storage purge drops everything ([`ReadAhead::clear`]).
//!
//! A fetch already in flight when that happens must not put the old copy back: it read the
//! message before the change, and stores it a second or two after. So `forget` leaves a tombstone
//! for [`RECENT_TTL`], `clear` raises a floor, and [`ReadAhead::remember`] takes the instant its
//! fetch BEGAN and keeps nothing a tombstone or the floor is newer than. A tombstone older than
//! [`RECENT_TTL`] is not needed: a fetch that began before it is too old to keep anything anyway.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{StreamExt as _, TryStreamExt as _};

use crate::model::{ChannelId, ChannelInfo, Message, MessageId};
use crate::ops::{self, OpError};
use crate::speech::SpeechProvider;
use crate::state::AppState;

/// How long a fetched message stays readable from here, counted from when its fetch began.
///
/// Long enough to cover "the page drew the channel, the reader turned Read on and tapped a line".
/// The measured case was 3 to 8 seconds. Short enough that an edit nobody pushed is stale for
/// half a minute at most. A ticket already minted keeps its text for longer than this anyway.
pub const RECENT_TTL: Duration = Duration::from_secs(30);

/// The most messages kept at once, across every channel, and the most tombstones.
///
/// Ten full windows of a hundred. Beyond it the oldest are dropped first. A tombstone dropped
/// early raises the floor in its place (see [`ReadAhead::forget`]), so the bound never lets an
/// in-flight fetch restore a forgotten message.
pub const MAX_RECENT: usize = 1_000;

/// How many thread messages ONE preparation fetches at once when they missed.
///
/// Measured against the deployed chat adapter's own log (completion times only, so the overlap is
/// inferred rather than observed). A lookup the adapter answers from its own cache took about
/// 42 ms when the previous prepare route asked for them one after another. Lookups that were in
/// flight together completed 0 to 9 ms apart. So the per-lookup cost is mostly round trip, and a
/// few in flight together removes most of it. A lookup the adapter must send upstream still waits
/// for its single upstream slot, and four in flight only queue there. They add no upstream load.
pub const THREAD_FETCH_CONCURRENCY: usize = 4;

/// How many thread message lookups ALL preparations together have in flight at once.
///
/// The page sends one preparation per thread on screen, all at once, so a per-request bound
/// alone multiplies by the number of threads. This caps the total. It is twice the per-request
/// bound on purpose: one large batch can hold at most half of it, so a tapped row's own
/// preparation, sent while that batch is still fetching, still finds a free slot.
pub const THREAD_FETCHES_IN_FLIGHT: usize = 2 * THREAD_FETCH_CONCURRENCY;

/// The longest a warm-up started by [`ReadAhead::start_warm_up`] is waited for.
///
/// The providers bound their own steps (a conversation session waits at most 25 s for its lock,
/// 10 s to connect and 10 s to start), and this sits above all of them together, so it never cuts
/// a warm-up short that would have finished. It exists so that one that hangs anyway cannot keep
/// every later preparation from starting one: only one runs at a time.
pub const WARM_UP_BOUND: Duration = Duration::from_secs(60);

/// The content-free code logged when a warm-up outlasts [`WARM_UP_BOUND`].
pub const WARM_UP_TIMED_OUT: &str = "warm_up_timed_out";

type Key = (ChannelId, MessageId);

#[derive(Debug)]
struct Recent {
    message: Message,
    /// The thread the backend fetched this under or labelled it with. `None` is the channel's
    /// ordinary window.
    thread: Option<String>,
    /// When the fetch that read it began.
    fetched: Instant,
}

/// Everything behind the one lock. See the module documentation.
#[derive(Debug, Default)]
struct Held {
    recent: HashMap<Key, Recent>,
    /// Messages forgotten in the last [`RECENT_TTL`], and when.
    forgotten: HashMap<Key, Instant>,
    /// A fetch that began at or before this keeps nothing.
    floor: Option<Instant>,
}

impl Held {
    fn prune(&mut self, now: Instant) {
        self.recent
            .retain(|_, entry| now.saturating_duration_since(entry.fetched) < RECENT_TTL);
        self.forgotten
            .retain(|_, at| now.saturating_duration_since(*at) < RECENT_TTL);
    }
}

/// What the last window fetch made through [`ReadAhead::window_turn`] read.
#[derive(Clone, Debug)]
pub struct WindowRead {
    /// When it finished.
    pub finished: Instant,
    /// How many messages it asked for ([`ops::window_limit`]).
    pub limit: u16,
    /// The ids it answered with, oldest first. Ids only, never text: the text is in the memory
    /// above, under its own expiry and tombstones, and [`ReadAhead::recall_window`] puts the two
    /// back together for a caller that needs the window in order (a summary's context).
    pub ids: Arc<[MessageId]>,
    /// The provider's `as_of` for that read ([`ops::Window::as_of`]), so a window rebuilt from it
    /// says what the read said: `None` when the provider's answer was current.
    pub as_of: Option<String>,
}

impl WindowRead {
    /// What `window` read, finishing now.
    #[must_use]
    pub fn of(window: &ops::Window) -> Self {
        Self {
            finished: Instant::now(),
            limit: window.limit,
            ids: window.messages.iter().map(|m| m.id.clone()).collect(),
            as_of: window.as_of.clone(),
        }
    }
}

/// Messages fetched in the last [`RECENT_TTL`], one window fetch per channel at a time, a bound on
/// thread lookups, and the provider warm-up. See the module documentation.
#[derive(Debug)]
pub struct ReadAhead {
    held: Mutex<Held>,
    /// Per channel: the last window fetch made through [`ReadAhead::window_turn`].
    windows: Mutex<HashMap<ChannelId, Arc<tokio::sync::Mutex<Option<WindowRead>>>>>,
    /// [`THREAD_FETCHES_IN_FLIGHT`] permits, one per thread message lookup.
    lookups: tokio::sync::Semaphore,
    warming: Arc<AtomicBool>,
}

impl Default for ReadAhead {
    fn default() -> Self {
        Self {
            held: Mutex::default(),
            windows: Mutex::default(),
            lookups: tokio::sync::Semaphore::new(THREAD_FETCHES_IN_FLIGHT),
            warming: Arc::default(),
        }
    }
}

impl ReadAhead {
    /// Nothing remembered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep `messages`, fetched from `channel` by a fetch that began at `fetch_began`.
    ///
    /// `thread` is the thread the fetch was scoped to. Without one, each message is filed under
    /// the thread the backend labelled it with, if any. A thread lookup must match that label
    /// before it is answered from here.
    ///
    /// `fetch_began` must be taken BEFORE the fetch is sent. A message forgotten after that
    /// instant, or everything if the memory was cleared after it, is not kept: the fetch may have
    /// read it before the change that forgot it. And the message expires [`RECENT_TTL`] after it,
    /// not after this call, because that is the oldest the text can be.
    pub fn remember(
        &self,
        channel: &ChannelId,
        thread: Option<&str>,
        messages: &[Message],
        fetch_began: Instant,
    ) {
        self.remember_at(channel, thread, messages, fetch_began, Instant::now());
    }

    fn remember_at(
        &self,
        channel: &ChannelId,
        thread: Option<&str>,
        messages: &[Message],
        fetch_began: Instant,
        now: Instant,
    ) {
        if messages.is_empty() {
            return;
        }
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        held.prune(now);
        if now.saturating_duration_since(fetch_began) >= RECENT_TTL
            || held.floor.is_some_and(|floor| fetch_began <= floor)
        {
            return;
        }
        for message in messages {
            let key = (channel.clone(), message.id.clone());
            if held
                .forgotten
                .get(&key)
                .is_some_and(|forgotten| *forgotten >= fetch_began)
            {
                continue;
            }
            let filed_under = thread
                .map(str::to_owned)
                .or_else(|| message.thread.as_ref().map(|t| t.id.clone()));
            held.recent.insert(
                key,
                Recent {
                    message: message.clone(),
                    thread: filed_under,
                    fetched: fetch_began,
                },
            );
        }
        if held.recent.len() > MAX_RECENT {
            let mut ages: Vec<_> = held
                .recent
                .iter()
                .map(|(key, entry)| (entry.fetched, key.clone()))
                .collect();
            ages.sort_by_key(|(fetched, _)| *fetched);
            let excess = held.recent.len() - MAX_RECENT;
            for (_, key) in ages.into_iter().take(excess) {
                held.recent.remove(&key);
            }
        }
    }

    /// The messages among `ids` that were fetched from `channel` in the last [`RECENT_TTL`].
    ///
    /// With `thread`, only a message fetched in that thread or labelled with it. Without one, any
    /// message of the channel: the channel allowlist is the boundary, and the caller has already
    /// checked it.
    #[must_use]
    pub fn recall(
        &self,
        channel: &ChannelId,
        thread: Option<&str>,
        ids: &[String],
    ) -> HashMap<String, Message> {
        self.recall_at(channel, thread, ids, Instant::now())
    }

    fn recall_at(
        &self,
        channel: &ChannelId,
        thread: Option<&str>,
        ids: &[String],
        now: Instant,
    ) -> HashMap<String, Message> {
        let Ok(held) = self.held.lock() else {
            return HashMap::new();
        };
        let mut found = HashMap::new();
        for id in ids {
            let Some(entry) = held.recent.get(&(channel.clone(), MessageId(id.clone()))) else {
                continue;
            };
            if now.saturating_duration_since(entry.fetched) >= RECENT_TTL {
                continue;
            }
            if thread.is_some_and(|thread| entry.thread.as_deref() != Some(thread)) {
                continue;
            }
            found.insert(id.clone(), entry.message.clone());
        }
        found
    }

    /// The window `read` fetched, oldest first, rebuilt from memory: `None` unless EVERY message it
    /// answered with is still held for `channel`. `#208 persistent-cache`.
    ///
    /// All or nothing, because the caller wants the window in order — a summary takes the messages
    /// just before its target as context — and a window with a hole where an edited or deleted
    /// message was forgotten is not the window. Such a caller reads the window itself instead.
    #[must_use]
    pub fn recall_window(&self, channel: &ChannelId, read: &WindowRead) -> Option<Vec<Message>> {
        let ids: Vec<String> = read.ids.iter().map(|id| id.0.clone()).collect();
        let mut found = self.recall(channel, None, &ids);
        ids.iter().map(|id| found.remove(id)).collect()
    }

    /// Drop one message: it was edited or deleted, and the copy here no longer says what it says.
    ///
    /// Leaves a tombstone, so a fetch that began before now cannot put the old copy back.
    pub fn forget(&self, channel: &ChannelId, id: &MessageId) {
        self.forget_at(channel, id, Instant::now());
    }

    fn forget_at(&self, channel: &ChannelId, id: &MessageId, now: Instant) {
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        held.prune(now);
        let key = (channel.clone(), id.clone());
        held.recent.remove(&key);
        held.forgotten.insert(key, now);
        if held.forgotten.len() > MAX_RECENT {
            // Over the bound, the oldest tombstones go, and the floor rises to the newest of them:
            // every fetch they were guarding against began before it, so it now keeps nothing.
            let mut ages: Vec<_> = held
                .forgotten
                .iter()
                .map(|(key, at)| (*at, key.clone()))
                .collect();
            ages.sort_by_key(|(at, _)| *at);
            let excess = held.forgotten.len() - MAX_RECENT;
            for (at, key) in ages.into_iter().take(excess) {
                held.forgotten.remove(&key);
                held.floor = Some(held.floor.map_or(at, |floor| floor.max(at)));
            }
        }
    }

    /// Drop everything. For the storage purge, so that this copy of channel text does not outlive
    /// it, though it is not the only copy held in memory: `purge_storage` in `crate::http::api`
    /// lists what the purge leaves.
    ///
    /// Raises the floor to now, so a fetch already in flight keeps nothing when it lands.
    pub fn clear(&self) {
        self.clear_at(Instant::now());
    }

    fn clear_at(&self, now: Instant) {
        if let Ok(mut held) = self.held.lock() {
            held.recent.clear();
            held.forgotten.clear();
            held.floor = Some(now);
        }
    }

    /// How many messages are held, expired ones included until the next change.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.lock().map_or(0, |held| held.recent.len())
    }

    /// Whether no message is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Wait for this channel's window fetch slot.
    ///
    /// The guard holds what the last window fetch through this slot read, and when it finished. A
    /// caller that finds one that finished after its own start, and was at least as wide as its
    /// own would be, waited behind a fetch that has just read the window, and should use that
    /// rather than read it again.
    pub async fn window_turn(
        &self,
        channel: &ChannelId,
    ) -> tokio::sync::OwnedMutexGuard<Option<WindowRead>> {
        let slot = {
            let mut windows = self
                .windows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(windows.entry(channel.clone()).or_default())
        };
        slot.lock_owned().await
    }

    /// Start `speech`'s warm-up in the background, unless one is already running.
    ///
    /// Answers whether this call started one. The caller does not wait for it. A tap that comes
    /// while the warm-up is still opening a session joins that opening (see
    /// `ConversationSpeech::session_for_read`), so there is nothing to gain by holding the prepare
    /// response for it.
    ///
    /// Given up after [`WARM_UP_BOUND`], so one that hangs holds off later warm-ups for that long
    /// at most. Success is logged at INFO; a failure or a timeout at WARN, with the provider's
    /// content-free error code (or [`WARM_UP_TIMED_OUT`]) and never its detail. The read itself
    /// reports a failure if it persists.
    pub fn start_warm_up(&self, speech: Arc<dyn SpeechProvider>) -> bool {
        if self.warming.swap(true, Ordering::AcqRel) {
            return false;
        }
        let running = WarmingFlag(Arc::clone(&self.warming));
        tokio::spawn(async move {
            // Held across the await, so the flag clears even if the provider panics.
            let _running = running;
            let began = Instant::now();
            let outcome = tokio::time::timeout(WARM_UP_BOUND, speech.warm_up()).await;
            let warm_up_ms = millis(began.elapsed());
            match outcome {
                Ok(Ok(())) => {
                    tracing::info!(warm_up_ms, ok = true, "read-aloud warm-up finished");
                }
                Ok(Err(error)) => tracing::warn!(
                    warm_up_ms,
                    ok = false,
                    code = error.code(),
                    "read-aloud warm-up finished"
                ),
                Err(_) => tracing::warn!(
                    warm_up_ms,
                    ok = false,
                    code = WARM_UP_TIMED_OUT,
                    "read-aloud warm-up finished"
                ),
            }
        });
        true
    }

    /// Whether a warm-up started by [`ReadAhead::start_warm_up`] is still running.
    #[must_use]
    pub fn warming(&self) -> bool {
        self.warming.load(Ordering::Acquire)
    }
}

/// Clears the warm-up flag when the warm-up task ends, however it ends.
struct WarmingFlag(Arc<AtomicBool>);

impl Drop for WarmingFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The messages a preparation asked for, and what finding them took.
#[derive(Debug, Default)]
pub struct Found {
    /// Message id to message, for every id that was found.
    pub messages: HashMap<String, Message>,
    /// How many distinct ids were answered from [`ReadAhead`] without a fetch.
    pub hits: usize,
    /// How many chat fetches this preparation made: one per window, one per thread message.
    pub fetches: usize,
    /// Whether this preparation waited for another one's window fetch instead of making its own.
    pub joined: bool,
    /// Time spent looking in [`ReadAhead`].
    pub lookup: Duration,
    /// Time spent fetching, or waiting for another preparation's fetch.
    pub fetch: Duration,
}

/// Find `ids` of `channel` for speech: from what was fetched recently, and fetch only what is
/// missing.
///
/// Without a thread, the missing ids cost one window fetch, shared with any other preparation of
/// the same channel that has just made one at least as wide. An id that is not in the window is
/// simply absent, as it always was. In a thread, each missing id is fetched on its own, up to
/// [`THREAD_FETCH_CONCURRENCY`] at once for this preparation and [`THREAD_FETCHES_IN_FLIGHT`] for
/// all of them, and the first failure fails the batch, as it did when they were fetched in series.
///
/// `began` is when the preparation started. A window fetch that finished after it is fresh
/// enough to answer this one.
///
/// The caller has already checked `channel` against the allowlist.
///
/// # Errors
///
/// The errors of [`ops::messages`] and [`ops::message_by_id_scoped`].
pub async fn messages_for_speech(
    state: &AppState,
    channel: &ChannelInfo,
    thread: Option<&str>,
    ids: &[String],
    limit: Option<u16>,
    began: Instant,
) -> Result<Found, OpError> {
    let looking = Instant::now();
    let mut found = Found {
        messages: state.read_ahead.recall(&channel.id, thread, ids),
        ..Found::default()
    };
    found.hits = found.messages.len();
    found.lookup = looking.elapsed();
    let mut seen = HashSet::new();
    let missing: Vec<String> = ids
        .iter()
        .filter(|id| !found.messages.contains_key(*id) && seen.insert(id.as_str()))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(found);
    }
    let fetching = Instant::now();
    match thread {
        None => {
            let width = ops::window_limit(state, limit);
            let mut turn = state.read_ahead.window_turn(&channel.id).await;
            if turn
                .as_ref()
                .is_some_and(|read| read.finished >= began && read.limit >= width)
            {
                found.joined = true;
                found
                    .messages
                    .extend(state.read_ahead.recall(&channel.id, None, &missing));
            } else {
                // `ops::messages` remembers the window for everyone else as well.
                let window = ops::messages(state, channel.id.as_str(), limit).await?;
                *turn = Some(WindowRead::of(&window));
                found.fetches = 1;
                let wanted: HashSet<&str> = missing.iter().map(String::as_str).collect();
                for message in window.messages {
                    if wanted.contains(message.id.as_str()) {
                        found.messages.insert(message.id.0.clone(), message);
                    }
                }
            }
        }
        Some(thread) => {
            let lookups = &state.read_ahead.lookups;
            let fetched: Vec<Message> = futures_util::stream::iter(missing)
                .map(|id| async move {
                    // Never closed, so this always holds a permit.
                    let _slot = lookups.acquire().await;
                    ops::message_by_id_scoped(state, channel.id.as_str(), &id, limit, Some(thread))
                        .await
                        .map(|(_, message)| message)
                })
                .buffered(THREAD_FETCH_CONCURRENCY)
                .try_collect()
                .await?;
            found.fetches = fetched.len();
            state
                .read_ahead
                .remember(&channel.id, Some(thread), &fetched, fetching);
            for message in fetched {
                found.messages.insert(message.id.0.clone(), message);
            }
        }
    }
    found.fetch = fetching.elapsed();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UserId;
    use crate::threads::MessageThread;

    fn message(id: &str, thread: Option<&str>) -> Message {
        Message {
            id: MessageId(id.to_owned()),
            channel_id: ChannelId("1".to_owned()),
            author: "Reader".to_owned(),
            author_id: UserId("7".to_owned()),
            author_is_bot: false,
            timestamp: "2026-10-12T01:00:00Z".to_owned(),
            spoken_time: String::new(),
            reply_to: None,
            content: format!("text of {id}"),
            spoken_content: String::new(),
            content_html: String::new(),
            noise: false,
            thread: thread.map(|id| MessageThread {
                id: id.to_owned(),
                root_message_id: None,
                is_root: false,
                reply_count: None,
                reply_count_exact: false,
            }),
            reactions: None,
            heading: None,
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn a_message_is_recalled_until_it_expires() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let then = Instant::now();
        read_ahead.remember_at(&channel, None, &[message("10", None)], then, then);
        let found = read_ahead.recall_at(&channel, None, &ids(&["10", "11"]), then);
        assert_eq!(found.len(), 1);
        assert_eq!(found["10"].content, "text of 10");
        let later = then + RECENT_TTL - Duration::from_millis(1);
        assert_eq!(
            read_ahead
                .recall_at(&channel, None, &ids(&["10"]), later)
                .len(),
            1
        );
        let expired = then + RECENT_TTL;
        assert!(read_ahead
            .recall_at(&channel, None, &ids(&["10"]), expired)
            .is_empty());
    }

    #[test]
    fn a_message_expires_counting_from_when_its_fetch_began() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let began = Instant::now();
        let landed = began + Duration::from_secs(2);
        read_ahead.remember_at(&channel, None, &[message("10", None)], began, landed);
        assert!(read_ahead
            .recall_at(&channel, None, &ids(&["10"]), began + RECENT_TTL)
            .is_empty());
        // A fetch that took the whole of the memory's life keeps nothing at all.
        read_ahead.remember_at(
            &channel,
            None,
            &[message("11", None)],
            began,
            began + RECENT_TTL,
        );
        assert!(read_ahead.is_empty());
    }

    #[test]
    fn another_channel_never_answers() {
        let read_ahead = ReadAhead::new();
        read_ahead.remember(
            &ChannelId("1".to_owned()),
            None,
            &[message("10", None)],
            Instant::now(),
        );
        assert!(read_ahead
            .recall(&ChannelId("2".to_owned()), None, &ids(&["10"]))
            .is_empty());
    }

    #[test]
    fn a_thread_lookup_needs_the_thread_the_message_was_fetched_in() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let now = Instant::now();
        // Labelled by the backend in an unscoped (flat) read.
        read_ahead.remember(&channel, None, &[message("10", Some("T"))], now);
        // Fetched in a thread-scoped read with no label of its own.
        read_ahead.remember(&channel, Some("U"), &[message("20", None)], now);
        // Neither thread, nor labelled.
        read_ahead.remember(&channel, None, &[message("30", None)], now);
        let wanted = ids(&["10", "20", "30"]);
        let in_t = read_ahead.recall(&channel, Some("T"), &wanted);
        assert_eq!(in_t.keys().collect::<Vec<_>>(), vec!["10"]);
        let in_u = read_ahead.recall(&channel, Some("U"), &wanted);
        assert_eq!(in_u.keys().collect::<Vec<_>>(), vec!["20"]);
        assert!(read_ahead
            .recall(&channel, Some("elsewhere"), &wanted)
            .is_empty());
        assert_eq!(read_ahead.recall(&channel, None, &wanted).len(), 3);
    }

    #[test]
    fn forgetting_an_edited_message_drops_only_that_one() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        read_ahead.remember(
            &channel,
            None,
            &[message("10", None), message("11", None)],
            Instant::now(),
        );
        read_ahead.forget(&channel, &MessageId("10".to_owned()));
        let found = read_ahead.recall(&channel, None, &ids(&["10", "11"]));
        assert_eq!(found.keys().collect::<Vec<_>>(), vec!["11"]);
        read_ahead.clear();
        assert!(read_ahead.is_empty());
    }

    #[test]
    fn a_fetch_that_began_before_a_forget_cannot_put_the_old_copy_back() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let began = Instant::now();
        let edited = began + Duration::from_millis(500);
        let landed = began + Duration::from_secs(2);
        // The edit arrives while the fetch is still out, and the fetch then lands its old copy.
        read_ahead.forget_at(&channel, &MessageId("10".to_owned()), edited);
        read_ahead.remember_at(
            &channel,
            None,
            &[message("10", None), message("11", None)],
            began,
            landed,
        );
        let found = read_ahead.recall_at(&channel, None, &ids(&["10", "11"]), landed);
        assert_eq!(
            found.keys().collect::<Vec<_>>(),
            vec!["11"],
            "the stale copy came back"
        );
        // A fetch that began after the edit read the new text, and is kept.
        let again = edited + Duration::from_millis(1);
        read_ahead.remember_at(&channel, None, &[message("10", None)], again, landed);
        assert_eq!(
            read_ahead
                .recall_at(&channel, None, &ids(&["10"]), landed)
                .len(),
            1
        );
    }

    #[test]
    fn a_fetch_that_began_before_a_clear_keeps_nothing() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let began = Instant::now();
        let purged = began + Duration::from_millis(500);
        read_ahead.clear_at(purged);
        read_ahead.remember_at(
            &channel,
            None,
            &[message("10", None)],
            began,
            purged + Duration::from_secs(1),
        );
        assert!(
            read_ahead.is_empty(),
            "the purge was undone by a late fetch"
        );
        read_ahead.remember_at(
            &channel,
            None,
            &[message("10", None)],
            purged + Duration::from_millis(1),
            purged + Duration::from_secs(1),
        );
        assert_eq!(read_ahead.len(), 1);
    }

    #[test]
    fn tombstones_beyond_the_bound_raise_the_floor_instead_of_vanishing() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let began = Instant::now();
        // The first tombstone is the one dropped to keep within the bound.
        for n in 0..=MAX_RECENT {
            let at = began + Duration::from_millis(1 + u64::try_from(n).expect("small"));
            read_ahead.forget_at(&channel, &MessageId(format!("f{n}")), at);
        }
        let landed = began + Duration::from_secs(5);
        read_ahead.remember_at(&channel, None, &[message("f0", None)], began, landed);
        assert!(
            read_ahead.is_empty(),
            "a dropped tombstone let the old copy back"
        );
    }

    #[test]
    fn the_oldest_go_first_beyond_the_bound() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let start = Instant::now();
        let first: Vec<Message> = (0..MAX_RECENT)
            .map(|n| message(&format!("a{n}"), None))
            .collect();
        read_ahead.remember_at(&channel, None, &first, start, start);
        let newer = start + Duration::from_millis(5);
        read_ahead.remember_at(
            &channel,
            None,
            &[message("b0", None), message("b1", None)],
            newer,
            newer,
        );
        assert_eq!(read_ahead.len(), MAX_RECENT);
        let found = read_ahead.recall_at(&channel, None, &ids(&["b0", "b1"]), newer);
        assert_eq!(found.len(), 2, "the newest were the ones dropped");
    }

    #[test]
    fn a_window_is_rebuilt_in_order_only_while_every_message_of_it_is_held() {
        // `#208 persistent-cache`: a summary's context is the messages just before its target, so
        // a window with a hole where a forgotten message was is not the window.
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let window = vec![
            message("10", None),
            message("11", None),
            message("12", None),
        ];
        read_ahead.remember(&channel, None, &window, Instant::now());
        let read = WindowRead::of(&ops::Window {
            channel: crate::model::ChannelInfo {
                id: channel.clone(),
                label: String::new(),
                writable: false,
                alias: None,
                added: false,
                provider: None,
            },
            messages: window,
            limit: 50,
            as_of: None,
        });
        let rebuilt = read_ahead.recall_window(&channel, &read).expect("all held");
        assert_eq!(
            rebuilt.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["10", "11", "12"]
        );
        assert!(read_ahead
            .recall_window(&ChannelId("2".to_owned()), &read)
            .is_none());
        read_ahead.forget(&channel, &MessageId("11".to_owned()));
        assert!(
            read_ahead.recall_window(&channel, &read).is_none(),
            "a window missing a forgotten message was answered as the window"
        );
    }

    #[test]
    fn expired_messages_are_dropped_when_more_arrive() {
        let read_ahead = ReadAhead::new();
        let channel = ChannelId("1".to_owned());
        let start = Instant::now();
        read_ahead.remember_at(&channel, None, &[message("10", None)], start, start);
        let later = start + RECENT_TTL;
        read_ahead.remember_at(&channel, None, &[message("11", None)], later, later);
        assert_eq!(read_ahead.len(), 1);
    }
}
