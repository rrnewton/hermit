//! Inbound chat ingestion, and the fan-out that lets a page be TOLD rather than have to ask.
//!
//! Everything else in this crate is pulled: a question arrives, a channel is read, an answer goes
//! back. This module is the one place that runs without being asked, so the two decisions behind
//! it are written down here rather than left to be inferred from the code.
//!
//! There are two mutually exclusive MESSAGE producers. The built-in provider path is bounded
//! polling; an external provider adapter can instead push normalized create, update, and delete
//! events through the authenticated `/api/v1/live/events` endpoint. Both publish into the same
//! [`LiveHub`], and the browser always consumes the same SSE stream. A polled provider may also
//! receive content-free `/api/v1/live/hints`: a hint merely wakes its existing cursor, while the
//! periodic interval remains the recovery path, so it is not a second producer.
//!
//! # The built-in Discord source is bounded POLLING, not a Gateway connection
//!
//! Discord's real-time mechanism is the Gateway, and this server does not use it. It keeps a
//! per-channel snowflake cursor, seeds it with
//! [`crate::chat::ChatClient::fetch_recent`], and thereafter walks forward from it with
//! [`crate::chat::ChatClient::fetch_page`]'s `after` on an interval.
//!
//! That is a deliberate choice against the more capable option, for three reasons:
//!
//! 1. **A Gateway is three new things at once.** It needs a WebSocket client for Discord (this
//!    crate has one for ElevenLabs and none for Discord), a heartbeat / resume /
//!    session-invalidate state machine, and the PRIVILEGED `MESSAGE_CONTENT` intent — which a
//!    Discord application has to be granted, and without which every message arrives with an
//!    empty body. Polling needs none of those.
//! 2. **Polling reuses what is already tested.** `fetch_page`, [`crate::model::sort_oldest_first`]
//!    and [`crate::model::MessageId::numeric`] are the whole mechanism, and all three already have
//!    tests — including the string-versus-numeric ordering trap, which is exactly the bug a
//!    hand-rolled cursor would reintroduce, and the `before`/`after` asymmetry, which is the other
//!    one.
//! 3. **The fake can genuinely fail.** [`crate::discord::fake::FakeDiscord`] has `fail_next` and an
//!    unknown-channel refusal, so the failure path of the poll loop is exercised in the suite
//!    rather than reasoned about. A Gateway fake would be a second protocol implementation whose
//!    fidelity nobody could check.
//!
//! And one reason that is about sequencing rather than design: **the Discord layer here has never
//! run against live Discord.** The README says so in Known gaps. Making first contact and
//! introducing a stateful always-connected client in the same change is two untested things at
//! once, and when it misbehaves there is no way to tell which one is wrong.
//!
//! **A Gateway remains an upgrade path behind this same seam.** Everything above this module sees
//! [`LiveHub`]: a channel-keyed publish/subscribe with a replay tail. The external adapter endpoint
//! already publishes into it; a native Gateway implementation would do the same.
//!
//! ## What polling costs, stated plainly
//!
//! It multiplies this server's Discord request volume by one request per channel per interval,
//! forever, whether or not anybody is listening. A 429 is no longer simply passed on: the client
//! obeys Discord's `Retry-After` within a bounded budget, and only a limit it could not clear
//! reaches this loop — see [`crate::discord::ratelimit`]. That makes a tick cheaper to get wrong,
//! not free. So polling is still **off unless configured** (`discord.live_poll_seconds`, default
//! 0), the interval still has a floor the configuration refuses to go below, and a failing channel
//! still backs off rather than hammering — see [`backoff`]. Adapter authentication is separately
//! opt-in with `ingest.token`; full pushed events remain refused for a provider this server polls,
//! while content-free hints safely wake that poller without bypassing its backoff.
//!
//! The two waits nest rather than compete: the client's is the inner, precise one Discord asked
//! for, and [`backoff`] is the outer one, which never waits LESS than Discord's outstanding
//! request. There is one notion of "slow down" here, applied at two scales.
//!
//! **The backoff is not decoration, and it is not left to be read.** [`poll_forever`] sleeping longer
//! after a failure is pinned by a test that measures the loop's own elapsed time against a healthy
//! control, because deleting the one line that applies it changes no other assertion in this
//! suite: a hammering loop still fetches, still publishes and still recovers.
//!
//! ## What a busy channel costs, also stated plainly
//!
//! A tick walks FORWARD from its cursor with Discord's `after` parameter rather than re-reading
//! the most recent window, which is the difference between "at most `limit` messages per tick get
//! through" and "nothing is ever skipped". Reading the newest `limit` messages and then moving the
//! cursor to the newest of them SILENTLY DROPS everything in between when a channel produces more
//! than `limit` in one interval — no gap, no warning, and a page and an agent that are simply
//! missing lines.
//!
//! Forward paging cannot lose them: whatever is not read this tick is still after the cursor on
//! the next one. What it can do is fall behind, so the work per tick is bounded at
//! [`MAX_PAGES_PER_TICK`] pages and [`Tick::backlog`] says when that bound is what stopped it. The
//! loop logs that, once, on the way into it.
//!
//! # Decision two: the PAGE keeps the ElevenLabs conversation socket
//!
//! A message arriving in Discord should be able to reach a live voice conversation. Someone has to
//! hold the socket that carries it, and today that is the browser: this server only mints a signed
//! URL (`GET /api/v1/signed-url`) and holds no per-conversation vendor state at all. The page
//! decides what to do with the arrival — say nothing, summarise it, or read it out — and sends the
//! turn itself; nothing about that choice is known here.
//!
//! It stays that way. Moving the socket server-side would turn vibe-talk into an always-connected,
//! BILLED conversation holder — a process whose cost accrues while nobody is in the car — and it
//! would put third-party channel text on a vendor socket that no human is currently looking at.
//!
//! **The cost of that decision, said out loud: an arriving message reaches the agent only while
//! the page is open.** Close the tab and the channel keeps moving, this server keeps ingesting,
//! and the agent hears nothing until somebody opens `/voice` again and asks. That is a real
//! limitation, and the page says so rather than implying a relay that is not running.
//!
//! # Self-posted messages
//!
//! [`crate::ops::reply`] posts as the bot, and the poller then reads that post back like any
//! other message. Relaying it into the conversation would let the agent hear its own reply and
//! answer it, which is a feedback loop that spends money. So every id this server itself posted
//! is recorded here ([`LiveHub::note_self_posted`]) and travels with the published message as
//! [`LiveMessage::self_posted`]. It is deliberately NOT an author comparison: this server does
//! not know its own bot's user id — [`crate::discord::http::HttpDiscordClient`] never calls
//! `/users/@me` — so an author check would be a guess.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{broadcast, watch};

use crate::chat::{ChatClient, ChatError};
use crate::contract::{LiveDeleteEvent, LiveMessageEvent, LiveResetEvent};
use crate::model::{sort_oldest_first, ChannelId, ChannelInfo, Message, MessageId};
use crate::state::AppState;

/// How many published messages each channel keeps for replay after a dropped connection.
///
/// A bound rather than a history: this is a RECONNECTION aid, not storage. A page that has been
/// away longer than this re-reads the channel through `/messages` instead, which is why the
/// stream ends with `event: reset` rather than quietly resuming short.
pub const REPLAY_TAIL: usize = 200;

/// How far behind a subscriber may fall before it is told to re-read.
///
/// Small on purpose. A subscriber this far behind is not going to catch up by being given more
/// buffer, and silently dropping messages into a gap is the one outcome this stream must not
/// produce.
pub const BROADCAST_CAPACITY: usize = 64;

/// How many self-posted ids are remembered, across all channels.
///
/// Bounded because it is a de-duplication window, not a record: the only thing that ever consults
/// it is a message arriving from a poll seconds after this server posted it.
pub const SELF_POSTED_MEMORY: usize = 256;

/// How many adapter event ids are remembered per channel for retry de-duplication.
///
/// This is deliberately larger than [`REPLAY_TAIL`]: an adapter may retry an acknowledged event
/// after it has fallen out of a reconnecting browser's tail, and that retry must still be a no-op.
/// It is nevertheless a bound rather than durable storage. Adapters must give every distinct
/// create, update, and delete its own stable id and retry the same id only for the same event.
pub const INGEST_DEDUPE_MEMORY: usize = 1_024;

/// Largest accepted adapter event id, in bytes.
pub const MAX_EVENT_ID_BYTES: usize = 256;

/// Namespace that keeps an adapter's numeric event id from being mistaken for a message cursor.
const INGEST_EVENT_PREFIX: &str = "push:";

/// The smallest polling interval the configuration will accept, in seconds.
///
/// Not a preference. One request per channel per interval goes against a rate limit this server
/// does not yet handle, so an operator who types `1` should be refused with a reason rather than
/// quietly given a request storm.
pub const MIN_POLL_SECONDS: u64 = 5;

/// How briefly a change-hint wake-up waits for the rest of the same upstream burst.
///
/// Provider event streams commonly replay several records together after connecting. A hint is
/// deliberately content-free, so polling once for the burst is both cheaper and just as current
/// as polling once per record. The first wake-up waits only one tenth of a second; sustained
/// hint-driven reads are separately spaced by [`MIN_POLL_SECONDS`] so this fast path cannot bypass
/// the configured polling safety floor.
pub const HINT_COALESCE_MILLIS: u64 = 100;

/// What happened to the message carried by a live event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveKind {
    /// A message appeared for the first time.
    Create,
    /// The provider changed an existing message.
    Update,
    /// The provider removed an existing message.
    Delete,
}

/// One provider-neutral event, as it is published to live subscribers.
///
/// Deletes carry a private tombstone in `message`: only its channel and message ids are rendered
/// on the wire. Keeping one envelope type lets polling retain its small `LiveMessage` interface
/// while typed adapter events share the same ordered broadcast and replay tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveMessage {
    /// Stable id of this provider event. Retries reuse it; distinct changes must not.
    pub event_id: String,
    /// Create, update, or delete.
    pub kind: LiveKind,
    /// Whether the adapter classified this as catch-up/history rather than a new live event.
    pub historical: bool,
    /// The message itself. UNTRUSTED: written by whoever is in the channel.
    pub message: Message,
    /// Whether THIS SERVER posted it, through [`crate::ops::reply`].
    ///
    /// The page must not relay these into a live conversation; see the module documentation.
    pub self_posted: bool,
}

/// What a subscriber gets when it attaches.
pub struct Subscription {
    /// Messages already published that are newer than the caller's cursor, oldest first.
    pub replay: Vec<LiveMessage>,
    /// Everything published from the instant of attachment onwards.
    pub receiver: broadcast::Receiver<LiveMessage>,
    /// Whether the requested opaque cursor has fallen out of the bounded tail.
    pub reset_on_attach: bool,
}

struct Feed {
    sender: broadcast::Sender<LiveMessage>,
    tail: VecDeque<LiveMessage>,
    dedupe_order: VecDeque<String>,
    dedupe: BTreeSet<String>,
}

/// The fan-out: one publisher per channel, each with a bounded replay tail.
///
/// Feeds are created on first use rather than from the configured channel list, and the map is
/// still bounded, because **every path into this type is already allowlist-gated**:
/// [`crate::ops::watch`] resolves the channel against the configuration before subscribing, and
/// the poll loop only ever visits the configured channels. A hub that carried its own copy of the
/// allowlist would be a second definition of "which channels exist" — and two of those is how one
/// of them goes stale.
pub struct LiveHub {
    feeds: Mutex<BTreeMap<ChannelId, Feed>>,
    self_posted: Mutex<(VecDeque<MessageId>, BTreeSet<MessageId>)>,
    hinted: Mutex<BTreeMap<Option<String>, BTreeSet<ChannelId>>>,
    hint_generation: watch::Sender<u64>,
    /// The newest created message per channel the owner set to clear unread at its provider.
    /// `#232 upstream-mark-read`. Fed on every publish, never waited on: see
    /// [`crate::upstream_clear`].
    upstream_clear: std::sync::Arc<crate::upstream_clear::Candidates>,
    /// Each channel's newest window, kept current by every publish. `#210 voice-catchup-speed`.
    /// Here for the reason `upstream_clear` is: both producers reach the hub, and only the hub,
    /// so this is the one place a change cannot bypass. See [`crate::recent`].
    recent: std::sync::Arc<crate::recent::RecentWindows>,
}

impl Default for LiveHub {
    fn default() -> Self {
        Self::with_recent(std::sync::Arc::new(crate::recent::RecentWindows::new()))
    }
}

impl LiveHub {
    /// An empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty hub keeping its channel windows in `recent`. For a test that shortens the
    /// catch-up's durations; see [`crate::recent::Timing`].
    #[must_use]
    pub fn with_recent(recent: std::sync::Arc<crate::recent::RecentWindows>) -> Self {
        let (hint_generation, _receiver) = watch::channel(0);
        Self {
            feeds: Mutex::new(BTreeMap::new()),
            self_posted: Mutex::new((VecDeque::new(), BTreeSet::new())),
            hinted: Mutex::new(BTreeMap::new()),
            hint_generation,
            upstream_clear: std::sync::Arc::new(crate::upstream_clear::Candidates::new()),
            recent,
        }
    }

    /// Each channel's held window. `#210 voice-catchup-speed`.
    #[must_use]
    pub fn recent(&self) -> &std::sync::Arc<crate::recent::RecentWindows> {
        &self.recent
    }

    /// The messages waiting to be cleared at their provider. `#232 upstream-mark-read`.
    #[must_use]
    pub fn upstream_clear(&self) -> &std::sync::Arc<crate::upstream_clear::Candidates> {
        &self.upstream_clear
    }

    /// Queue one content-free request to poll `channel` now.
    ///
    /// The provider key is `None` for a single-provider deployment, matching the poll task's own
    /// key, and `Some` in a multi-provider deployment. Repeated hints coalesce until that task
    /// takes the channel. The periodic poll remains the recovery path, so a full notification
    /// buffer, process restart, or event-stream gap can delay a read but cannot make the hint the
    /// only record that a message existed.
    #[must_use]
    pub fn queue_hint(&self, provider_key: Option<&str>, channel: &ChannelId) -> bool {
        let inserted = lock(&self.hinted)
            .entry(provider_key.map(str::to_owned))
            .or_default()
            .insert(channel.clone());
        if inserted {
            let next = (*self.hint_generation.borrow()).wrapping_add(1);
            self.hint_generation.send_replace(next);
        }
        inserted
    }

    fn subscribe_hints(&self) -> watch::Receiver<u64> {
        self.hint_generation.subscribe()
    }

    fn take_hints(&self, provider_key: Option<&str>) -> BTreeSet<ChannelId> {
        lock(&self.hinted)
            .remove(&provider_key.map(str::to_owned))
            .unwrap_or_default()
    }

    /// Remember that this server posted `id`, so the poll that reads it back can say so.
    ///
    /// Bounded to [`SELF_POSTED_MEMORY`] ids, oldest evicted first.
    pub fn note_self_posted(&self, id: &MessageId) {
        let mut guard = lock(&self.self_posted);
        let (order, set) = &mut *guard;
        if !set.insert(id.clone()) {
            return;
        }
        order.push_back(id.clone());
        while order.len() > SELF_POSTED_MEMORY {
            if let Some(evicted) = order.pop_front() {
                set.remove(&evicted);
            }
        }
    }

    /// Whether `id` is one this server posted.
    #[must_use]
    pub fn is_self_posted(&self, id: &MessageId) -> bool {
        lock(&self.self_posted).1.contains(id)
    }

    /// Publish one message to `channel`, and remember it for replay.
    ///
    /// A publish with no subscribers is NOT a failure: the tail is what a page attaching a moment
    /// later reads, so the message is kept either way.
    pub fn publish(&self, channel: &ChannelId, message: Message) {
        let self_posted = self.is_self_posted(&message.id);
        let live = LiveMessage {
            event_id: message.id.as_str().to_owned(),
            kind: LiveKind::Create,
            historical: false,
            message,
            self_posted,
        };
        self.publish_inner(channel, live, false);
    }

    /// Publish one event delivered by an external adapter.
    ///
    /// Returns `false` when this channel has already accepted `event_id` inside the bounded
    /// de-duplication window. The duplicate is not broadcast or added to the replay tail.
    pub fn publish_ingested(
        &self,
        event_id: String,
        kind: LiveKind,
        historical: bool,
        channel: &ChannelId,
        message: Message,
    ) -> bool {
        let self_posted = kind != LiveKind::Delete && self.is_self_posted(&message.id);
        let live = LiveMessage {
            event_id: format!("{INGEST_EVENT_PREFIX}{event_id}"),
            kind,
            historical,
            message,
            self_posted,
        };
        self.publish_inner(channel, live, true)
    }

    fn publish_inner(&self, channel: &ChannelId, live: LiveMessage, deduplicate: bool) -> bool {
        // Both producers reach this, so it is where a created message is noted for clearing at
        // its provider: one map update and a wake-up, after the feed lock is released.
        let created = (live.kind == LiveKind::Create)
            .then(|| (live.message.id.clone(), live.message.timestamp.clone()));
        if !self.publish_to_feed(channel, live, deduplicate) {
            return false;
        }
        if let Some((id, timestamp)) = created {
            self.upstream_clear.offer_parts(channel, &id, &timestamp);
        }
        true
    }

    fn publish_to_feed(&self, channel: &ChannelId, live: LiveMessage, deduplicate: bool) -> bool {
        let mut feeds = lock(&self.feeds);
        let feed = feeds.entry(channel.clone()).or_insert_with(new_feed);
        if deduplicate {
            if !feed.dedupe.insert(live.event_id.clone()) {
                return false;
            }
            feed.dedupe_order.push_back(live.event_id.clone());
            while feed.dedupe_order.len() > INGEST_DEDUPE_MEMORY {
                if let Some(evicted) = feed.dedupe_order.pop_front() {
                    feed.dedupe.remove(&evicted);
                }
            }
        }
        self.recent.merge(channel, &live);
        feed.tail.push_back(live.clone());
        while feed.tail.len() > REPLAY_TAIL {
            feed.tail.pop_front();
        }
        let _ = feed.sender.send(live);
        true
    }

    /// Attach to a channel, replaying what was published after `after`.
    ///
    /// **The receiver is created BEFORE the tail is read, under the same lock.** That ordering is
    /// the whole correctness of this function: attaching afterwards leaves a window in which a
    /// message is published after the tail was copied and before the receiver exists, and it is
    /// gone — the one outcome the recovery story is supposed to rule out. Holding the lock across
    /// both is what makes it atomic with respect to [`LiveHub::publish`].
    ///
    /// `after` is an event id the caller has already seen. When it is still in the tail, exactly
    /// what follows it is replayed. Every missing id forces a reset, including a numeric id from the
    /// polling producer: once the bounded tail has evicted the cursor, numeric ordering can identify
    /// newer retained creates but cannot prove that nothing was lost before them. It also cannot
    /// place push updates and deletes. Replaying either a partial tail or nothing would therefore
    /// hide a gap.
    #[must_use]
    pub fn subscribe(&self, channel: &ChannelId, after: Option<&MessageId>) -> Subscription {
        let mut feeds = lock(&self.feeds);
        let feed = feeds.entry(channel.clone()).or_insert_with(new_feed);
        let receiver = feed.sender.subscribe();
        let (replay, reset_on_attach) = match after {
            None => (feed.tail.iter().cloned().collect(), false),
            Some(after) => {
                if let Some(position) = feed
                    .tail
                    .iter()
                    .rposition(|held| held.event_id == after.as_str())
                {
                    (
                        feed.tail.iter().skip(position + 1).cloned().collect(),
                        false,
                    )
                } else {
                    (Vec::new(), true)
                }
            }
        };
        Subscription {
            replay,
            receiver,
            reset_on_attach,
        }
    }
}

fn new_feed() -> Feed {
    let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
    Feed {
        sender,
        tail: VecDeque::new(),
        dedupe_order: VecDeque::new(),
        dedupe: BTreeSet::new(),
    }
}

/// The SSE event name every arriving message carries.
pub const EVENT_MESSAGE: &str = "message";
/// The SSE event name for a changed message.
pub const EVENT_MESSAGE_UPDATE: &str = "message_update";
/// The SSE event name for a removed message.
pub const EVENT_MESSAGE_DELETE: &str = "message_delete";

/// The SSE event name that ends a stream a subscriber fell too far behind on.
///
/// A page that receives this must RE-READ the channel through `/messages`. It is deliberately not
/// a resumption: the messages that fell out of the broadcast buffer are gone from this stream, and
/// resuming short would be the silent drop this whole design exists to rule out.
pub const EVENT_RESET: &str = "reset";

/// Turn one [`Subscription`] into the body of a Server-Sent Events response.
///
/// The replay tail goes out first, oldest first, then everything live. Each event carries an
/// `id:` cursor so a reconnecting browser's `Last-Event-ID` has something to send back, and a
/// typed event name so the page does not have to guess create/update/delete from payload shape.
///
/// **Every event says whether it came out of the tail (`replayed: true`) or arrived live
/// (`replayed: false`).** Without that the two are indistinguishable on the wire, and a page that
/// attaches with no `Last-Event-ID` — a fresh sign-in, a channel change, the reconnect after an
/// `event: reset` — is handed up to [`REPLAY_TAIL`] messages it must render but must NOT announce.
/// Announcing them means relaying stale channel text into a live, BILLED conversation as "a
/// message was just posted", which is the same "existing history labelled as new" failure that
/// [`poll_once`]'s seeding tick exists to prevent, arriving through the other door.
///
/// A subscriber that falls further behind than [`BROADCAST_CAPACITY`] gets one
/// [`EVENT_RESET`] and the stream ENDS. See its documentation for why that is better than
/// resuming.
///
/// **Every message is judged against the owner's noise rules on its way OUT**, not when it was
/// published. `#196 auto-read-noise`: a rule is evaluated on the message's current text each time
/// it is served, and the tail can be minutes old — a rule added or a message rescued since then
/// has to be reflected in what a page attaching now is told. `app` and `channel` are what that
/// needs; the replay burst shares one reading of the rules, and each live event takes a fresh one.
pub fn events(
    subscription: Subscription,
    app: AppState,
    channel: ChannelInfo,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    struct State {
        replay: VecDeque<LiveMessage>,
        receiver: broadcast::Receiver<LiveMessage>,
        reset_on_attach: bool,
        done: bool,
        app: AppState,
        channel: ChannelId,
        /// How the channel's provider writes, for the rendered body each message event carries.
        /// `#217 markdown-blocks`.
        markup: crate::render::Markup,
        /// The rules as they stood when the replay burst began. Up to [`REPLAY_TAIL`] events go
        /// out back to back on attach, and two store reads for each would be the attach's whole
        /// cost for no change in the answer.
        replay_noise: Option<crate::noise::NoiseFilter>,
    }
    let markup = crate::render::markup_for(&app.config, &channel);
    let start = State {
        replay: subscription.replay.into_iter().collect(),
        receiver: subscription.receiver,
        reset_on_attach: subscription.reset_on_attach,
        done: false,
        app,
        channel: channel.id,
        markup,
        replay_noise: None,
    };
    futures_util::stream::unfold(start, |mut state| async move {
        if state.reset_on_attach {
            state.reset_on_attach = false;
            state.done = true;
            return Some((Ok(reset_event(0)), state));
        }
        if let Some(mut held) = state.replay.pop_front() {
            if state.replay_noise.is_none() {
                state.replay_noise =
                    Some(crate::noise::filter_for(&state.app, &state.channel).await);
            }
            if let Some(noise) = &state.replay_noise {
                judge(noise, &state.app, &state.channel, state.markup, &mut held);
            }
            return Some((Ok(message_event(&held, true)), state));
        }
        if state.done {
            return None;
        }
        match state.receiver.recv().await {
            Ok(mut live) => {
                let noise = crate::noise::filter_for(&state.app, &state.channel).await;
                judge(&noise, &state.app, &state.channel, state.markup, &mut live);
                Some((Ok(message_event(&live, false)), state))
            }
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                state.done = true;
                Some((Ok(reset_event(missed)), state))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    })
}

/// Decide whether one outgoing event's message is noise, and render its body and find its heading
/// for the page. A delete carries no text to judge.
///
/// Rendered on the way OUT, like the noise verdict, rather than when it was published: the tail is
/// shared by every subscriber, and only this stream's reader is the page. Its paren tag is recorded
/// first, as a read records a window's (`#242 rich-headers`): the stream is how a new message
/// reaches the page, and the next one from the same agent should not wait for a re-read.
fn judge(
    noise: &crate::noise::NoiseFilter,
    app: &AppState,
    channel: &ChannelId,
    markup: crate::render::Markup,
    live: &mut LiveMessage,
) {
    if live.kind != LiveKind::Delete {
        noise.mark(std::slice::from_mut(&mut live.message));
        let message = &mut live.message;
        if let Some(tag) = crate::heading::paren_candidate(&message.content, message.author_is_bot)
        {
            app.headings
                .record(channel, &message.author_id, &message.id, tag);
        }
        let confirmed = |tag: &str| app.headings.confirmed(channel, &message.author_id, tag);
        (message.content_html, message.heading) =
            crate::render::headed_body(&message.content, markup, message.author_is_bot, &confirmed);
    }
}

fn message_event(live: &LiveMessage, from_tail: bool) -> axum::response::sse::Event {
    let event = axum::response::sse::Event::default()
        .id(&live.event_id)
        .event(match live.kind {
            LiveKind::Create => EVENT_MESSAGE,
            LiveKind::Update => EVENT_MESSAGE_UPDATE,
            LiveKind::Delete => EVENT_MESSAGE_DELETE,
        });
    // A catch-up event is history even on its first trip through this process. Folding the
    // adapter's classification into the wire's existing `replayed` flag ensures the browser may
    // render it but never announce it to a live voice conversation as something that just
    // happened.
    // `from_tail` keeps the other half apart, because the page's reads depend on it: only an event
    // from the tail is one every read begun after the attach has already seen.
    let replayed = from_tail || live.historical;
    let encoded = match live.kind {
        LiveKind::Create | LiveKind::Update => event.json_data(LiveMessageEvent {
            message: &live.message,
            self_posted: live.self_posted,
            replayed,
            from_tail,
            untrusted_content_notice: crate::untrusted::NOTICE,
        }),
        LiveKind::Delete => event.json_data(LiveDeleteEvent {
            channel_id: live.message.channel_id.clone(),
            message_id: live.message.id.clone(),
            replayed,
            from_tail,
        }),
    };
    // A `Message` is plain data and cannot fail to serialize. If that ever stops being true,
    // ending the stream with a reset is the honest answer: the page re-reads and sees the
    // message, rather than being handed a frame it cannot parse.
    encoded.unwrap_or_else(|_| reset_event(0))
}

fn reset_event(missed: u64) -> axum::response::sse::Event {
    let data = serde_json::to_string(&LiveResetEvent {
        missed,
        detail: "this subscriber fell behind; re-read the channel rather than resuming",
    })
    .unwrap_or_else(|_| format!("{{\"missed\":{missed}}}"));
    axum::response::sse::Event::default()
        .event(EVENT_RESET)
        .data(data)
}

fn lock<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How many pages one tick will walk forward before leaving the rest for the next tick.
///
/// The bound exists because "catch up completely, whatever that costs" is a request storm against
/// a rate limit this server does not handle: a channel that has been busy for an hour would be
/// fetched a hundred times in one tick. Four pages of `discord.default_fetch_limit` is 200
/// messages per channel per interval at the default, which is far more than a human channel
/// produces and still a fixed ceiling on the damage.
///
/// Being stopped by it loses NOTHING — the cursor only ever moves past what was actually published
/// — it just means the next tick continues. [`Tick::backlog`] says when that happened.
pub const MAX_PAGES_PER_TICK: usize = 4;

/// What one call to [`poll_once`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// How many messages were published to the hub.
    pub published: usize,
    /// Whether [`MAX_PAGES_PER_TICK`] is what stopped this tick rather than running out of
    /// messages.
    ///
    /// **Not a gap.** Everything unread is still after the cursor and is published by a later
    /// tick; this says the channel is arriving faster than one tick reads it, which is a latency
    /// fact worth a log line and not a loss.
    pub backlog: bool,
}

/// Read one channel once, publishing whatever is newer than `cursor`.
///
/// **The first tick seeds and publishes nothing.** `cursor` starts as `None`, and on that tick
/// this function records the newest id it saw and returns 0. Without that, the first poll after a
/// restart would republish the whole recent window, and a page that attached a second later would
/// be shown existing history labelled as newly arrived — messages the reader had already read,
/// announced as though they had just been said, and relayed into a paid conversation as news.
///
/// **Every tick after that walks FORWARD from the cursor**, with Discord's `after`, and keeps
/// walking while pages come back full — up to [`MAX_PAGES_PER_TICK`]. Reading the most recent
/// `limit` messages instead would silently drop everything between the cursor and that window the
/// moment a channel produced more than `limit` in one interval: the cursor would jump to the
/// newest id and the messages in between would never be published, never be relayed, and never be
/// reported. `after` walks the OLDEST messages newer than the cursor, so the worst a burst can do
/// is take several ticks.
///
/// Messages whose id does not parse as a snowflake are skipped rather than guessed at: there is no
/// honest way to place them relative to the cursor, and publishing them on every tick would be a
/// permanent duplicate.
///
/// # Errors
///
/// [`ChatError`] from the fetch. **The cursor is not advanced past anything that was not
/// published**, so the next successful tick publishes everything that arrived in the meantime
/// rather than skipping it. It IS advanced past what this tick already published before the
/// failure, because republishing that would relay the same message into a paid conversation twice.
pub async fn poll_once(
    chat: &dyn ChatClient,
    hub: &LiveHub,
    channel: &ChannelId,
    limit: u16,
    cursor: &mut Option<u64>,
) -> Result<Tick, ChatError> {
    let Some(seen) = *cursor else {
        let messages = chat.fetch_recent(channel, limit).await?;
        // Seed. `unwrap_or(0)` rather than leaving the cursor unset: an EMPTY channel is a
        // perfectly good starting point, and leaving it `None` would make the next tick seed
        // again and publish nothing for as long as the channel stayed quiet.
        *cursor = Some(
            messages
                .iter()
                .filter_map(|m| m.id.numeric())
                .max()
                .unwrap_or(0),
        );
        return Ok(Tick::default());
    };

    let page_size = usize::from(limit.max(1));
    let mut published = 0;
    let mut at = seen;
    let mut backlog = false;
    for page in 0..MAX_PAGES_PER_TICK {
        let after = MessageId(at.to_string());
        let mut messages = match chat.fetch_page(channel, limit, None, Some(&after)).await {
            Ok(messages) => messages,
            Err(error) => {
                // Whatever went out in an earlier page of this tick really went out. Rewinding to
                // where the tick started would republish it, and the page cannot tell a
                // republished message from a new one once it has scrolled away.
                *cursor = Some(at);
                return Err(error);
            }
        };
        sort_oldest_first(&mut messages);
        let full = messages.len() >= page_size;
        let before = at;
        for message in messages {
            let Some(id) = message.id.numeric() else {
                continue;
            };
            if id <= at {
                continue;
            }
            at = id;
            hub.publish(channel, message);
            published += 1;
        }
        // `at == before` means the page held nothing this cursor can move past — an empty answer,
        // or one made entirely of ids that do not parse. Walking again would fetch the same page
        // forever.
        if !full || at == before {
            break;
        }
        if page + 1 == MAX_PAGES_PER_TICK {
            backlog = true;
        }
    }
    *cursor = Some(at);
    Ok(Tick { published, backlog })
}

/// How long to wait after `failures` consecutive failures on a channel.
///
/// Doubling, capped. The cap matters more than the growth: an unbounded backoff means a channel
/// that failed once at three in the morning is still not being read at nine, which reads on screen
/// as a dead bridge rather than as a recovered one.
#[must_use]
pub fn backoff(interval: Duration, failures: u32, max: Duration) -> Duration {
    if failures == 0 {
        return interval;
    }
    let shift = failures.min(6);
    interval
        .saturating_mul(1_u32 << shift)
        .min(max)
        .max(interval)
}

/// The ceiling [`backoff`] doubles up to, as a multiple of the configured interval.
///
/// Sixteen intervals: long enough that a channel that has been unreachable all night is not being
/// hammered, short enough that a five-minute outage costs at most a couple of extra minutes of
/// staleness once it clears.
pub const MAX_BACKOFF_INTERVALS: u32 = 16;

/// Poll every currently allowed channel of one provider forever, publishing into `state.live`.
///
/// Never returns and never gives up. A channel that fails backs off and is tried again; it does
/// not take the loop down with it, and it does not advance its own cursor, so nothing is skipped
/// once it recovers. The allowlist is read from [`AppState::all_channels`] on every tick so a
/// channel added in the app starts being polled without restarting the server. Cursor and backoff
/// state for a removed channel is discarded before the next fetch. Spawned by `main`, once per
/// polled provider, and cancelled only by the process exiting.
///
/// `provider_key` limits the loop to that provider's channels and `chat` is that provider's own
/// client; `None` polls every channel, which is what a single-provider deployment does.
pub async fn poll_forever(
    state: AppState,
    provider_key: Option<String>,
    chat: std::sync::Arc<dyn ChatClient>,
    limit: u16,
    interval: Duration,
) {
    let mut cursors = BTreeMap::new();
    poll_state_loop(
        &state,
        provider_key.as_deref(),
        chat.as_ref(),
        limit,
        interval,
        &mut cursors,
        None,
    )
    .await;
}

async fn poll_state_loop(
    state: &AppState,
    provider_key: Option<&str>,
    chat: &dyn ChatClient,
    limit: u16,
    interval: Duration,
    cursors: &mut BTreeMap<ChannelId, Option<u64>>,
    ticks: Option<u32>,
) {
    poll_loop_with_channels(
        chat,
        state.live.as_ref(),
        || {
            state
                .all_channels()
                .into_iter()
                .filter(|channel| {
                    provider_key.is_none_or(|key| channel.provider.as_deref() == Some(key))
                })
                .map(|channel| channel.id)
                .collect()
        },
        cursors,
        LoopConfig {
            limit,
            interval,
            ticks,
            hints: Some((state.live.as_ref(), provider_key)),
        },
    )
    .await;
}

/// The body of the static-channel polling tests, with the cursors handed in and a tick budget.
///
/// `ticks: None` is the production case and never returns. The cursors are the caller's so that a
/// test can stop the loop, change the channel underneath it, and start it again WHERE IT LEFT OFF
/// — which is the only way to state "an error did not lose the cursor" as an assertion rather than
/// as a comment.
#[cfg(test)]
async fn poll_loop(
    chat: &dyn ChatClient,
    hub: &LiveHub,
    channels: &[ChannelId],
    limit: u16,
    interval: Duration,
    cursors: &mut BTreeMap<ChannelId, Option<u64>>,
    ticks: Option<u32>,
) {
    let channels = channels.to_vec();
    poll_loop_with_channels(
        chat,
        hub,
        || channels.clone(),
        cursors,
        LoopConfig {
            limit,
            interval,
            ticks,
            hints: None,
        },
    )
    .await;
}

struct LoopConfig<'a> {
    limit: u16,
    interval: Duration,
    ticks: Option<u32>,
    hints: Option<(&'a LiveHub, Option<&'a str>)>,
}

async fn poll_loop_with_channels(
    chat: &dyn ChatClient,
    hub: &LiveHub,
    mut channels: impl FnMut() -> Vec<ChannelId>,
    cursors: &mut BTreeMap<ChannelId, Option<u64>>,
    config: LoopConfig<'_>,
) {
    let LoopConfig {
        limit,
        interval,
        ticks,
        hints,
    } = config;
    let (hint_hub, hint_provider_key) =
        hints.map_or((None, None), |(hub, provider)| (Some(hub), provider));
    let max_backoff = interval.saturating_mul(MAX_BACKOFF_INTERVALS);
    let mut failures: BTreeMap<ChannelId, u32> = BTreeMap::new();
    let mut behind: BTreeMap<ChannelId, bool> = BTreeMap::new();
    let mut remaining = ticks;
    let mut hinted_only: Option<BTreeSet<ChannelId>> = None;
    let mut hint_generation = hint_hub.map(LiveHub::subscribe_hints);
    // Hinted reads must not move this deadline. Otherwise a busy channel can keep resetting an
    // interval-shaped sleep and starve reconciliation for every quieter channel on its provider.
    let mut next_reconciliation = hint_hub.map(|_| tokio::time::Instant::now());
    // The first hint is latency-sensitive and may run immediately. Once it does, subsequent
    // hint-driven reads respect the same five-second floor as configured polling.
    let mut next_hint_read = hint_hub.map(|_| tokio::time::Instant::now());
    loop {
        let all_channels = channels();
        let current: BTreeSet<_> = all_channels.iter().cloned().collect();
        // Removal is durable state, not a pause. If the same channel is added later it must seed
        // from Discord again instead of resuming an old cursor and publishing the intervening
        // history as if it had just arrived.
        cursors.retain(|channel, _| current.contains(channel));
        failures.retain(|channel, _| current.contains(channel));
        behind.retain(|channel, _| current.contains(channel));
        let requested = hinted_only.take();
        let full_reconciliation = requested.is_none();
        // A full reconciliation already reads every channel for this provider. Discard hints
        // queued before it began; anything queued after this take remains pending and wakes the
        // next read, closing the fetch race without paying for every replayed event individually.
        if requested.is_none() {
            if let Some(hint_hub) = hint_hub {
                hint_hub.take_hints(hint_provider_key);
            }
        }
        let channels: Vec<_> = match requested {
            Some(requested) => all_channels
                .into_iter()
                .filter(|channel| requested.contains(channel))
                .collect(),
            None => all_channels,
        };
        let mut wait = interval;
        for channel in &channels {
            let cursor = cursors.entry(channel.clone()).or_default();
            let failed = failures.entry(channel.clone()).or_insert(0);
            match poll_once(chat, hub, channel, limit, cursor).await {
                Ok(tick) => {
                    if *failed > 0 {
                        tracing::info!(
                            channel = %channel,
                            after_failures = *failed,
                            "live ingestion recovered for this channel"
                        );
                        *failed = 0;
                    }
                    if tick.published > 0 {
                        tracing::debug!(
                            channel = %channel,
                            published = tick.published,
                            "published live messages"
                        );
                    }
                    // Said out loud, and said once — on the transition, exactly as a failure is.
                    // A channel arriving faster than one tick reads it is not losing anything, but
                    // it IS delivering late, and a reader who cannot see that will read the delay
                    // as the bridge being broken.
                    let was_behind = behind.entry(channel.clone()).or_insert(false);
                    if tick.backlog && !*was_behind {
                        tracing::warn!(
                            channel = %channel,
                            pages = MAX_PAGES_PER_TICK,
                            limit,
                            "live ingestion is BEHIND this channel: one tick read its {} page \
                             ceiling and more was still waiting. Nothing is skipped — the cursor \
                             only moved past what was published — but delivery is running late \
                             until it catches up.",
                            MAX_PAGES_PER_TICK
                        );
                    } else if !tick.backlog && *was_behind {
                        tracing::info!(
                            channel = %channel,
                            "live ingestion caught up with this channel"
                        );
                    }
                    *was_behind = tick.backlog;
                }
                Err(error) => {
                    // ONCE at WARN, on the transition into failure. A channel that has been
                    // failing for an hour would otherwise write a line every interval, and a log
                    // that repeats itself is a log nobody reads the rest of.
                    if *failed == 0 {
                        tracing::warn!(
                            channel = %channel,
                            %error,
                            "live ingestion failed for this channel; backing off and retrying. \
                             The cursor is NOT advanced, so nothing is skipped once it recovers."
                        );
                    } else {
                        tracing::debug!(
                            channel = %channel,
                            %error,
                            failures = *failed,
                            "live ingestion still failing"
                        );
                    }
                    *failed = failed.saturating_add(1);
                    // Two slow-downs would be one too many, so they NEST: the client has already
                    // waited out whatever `Retry-After` it could afford (see
                    // `crate::discord::ratelimit`), and what reaches here is a limit that did not
                    // clear. Taking the larger of Discord's own outstanding request and this
                    // loop's doubling means the outer wait can be longer than Discord asked for
                    // but never shorter — which is the one direction that would keep hammering a
                    // limiter that has already said no.
                    let asked_for = error.retry_after().unwrap_or_default();
                    wait = wait
                        .max(backoff(interval, *failed, max_backoff))
                        .max(asked_for);
                }
            }
        }
        if let Some(left) = remaining.as_mut() {
            *left = left.saturating_sub(1);
            if *left == 0 {
                return;
            }
        }
        if full_reconciliation {
            next_reconciliation = Some(tokio::time::Instant::now() + interval);
        } else {
            next_hint_read =
                Some(tokio::time::Instant::now() + Duration::from_secs(MIN_POLL_SECONDS));
        }
        // A failing provider already asked this loop to back off. A hint must not turn that
        // protection into a hammering path, so wake-ups shorten only the ordinary healthy wait.
        if wait > interval {
            tokio::time::sleep(wait).await;
            continue;
        }
        let (Some(hint_hub), Some(hint_generation)) = (hint_hub, hint_generation.as_mut()) else {
            tokio::time::sleep(wait).await;
            continue;
        };
        let reconciliation_deadline =
            next_reconciliation.expect("a hint receiver always has a reconciliation deadline");
        hinted_only = wait_for_hints(
            hint_hub,
            hint_provider_key,
            hint_generation,
            reconciliation_deadline,
        )
        .await;
        if hinted_only.is_some() {
            tokio::time::sleep(Duration::from_millis(HINT_COALESCE_MILLIS)).await;
            let next_wake = next_hint_read
                .expect("a hint receiver always has a hint-read deadline")
                .min(reconciliation_deadline);
            tokio::time::sleep_until(next_wake).await;
            if tokio::time::Instant::now() >= reconciliation_deadline {
                // The full pass consumes every pending hint for this provider and reads every
                // channel, so prefer it once due instead of paying for a hinted read first.
                hinted_only = None;
            } else if let Some(hinted) = hinted_only.as_mut() {
                hinted.extend(hint_hub.take_hints(hint_provider_key));
            }
        }
    }
}

/// Wait for either the ordinary reconciliation tick or at least one queued change hint.
///
/// The synchronous `take_hints` before `changed` closes both startup races: a hint queued before
/// this receiver existed is still found, and one queued between the take and the await changes
/// the generation and wakes it immediately. A stale generation with an already-drained set is a
/// harmless loop, not an extra provider read.
async fn wait_for_hints(
    hub: &LiveHub,
    provider_key: Option<&str>,
    generation: &mut watch::Receiver<u64>,
    reconciliation_deadline: tokio::time::Instant,
) -> Option<BTreeSet<ChannelId>> {
    let hinted = async {
        loop {
            let pending = hub.take_hints(provider_key);
            if !pending.is_empty() {
                return pending;
            }
            if generation.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        // When both become ready together, reconciliation wins. Random branch choice here would
        // put the non-starvation guarantee at the mercy of a hot hint stream.
        biased;
        () = tokio::time::sleep_until(reconciliation_deadline) => None,
        pending = hinted => Some(pending),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::fake::FakeDiscord;
    use std::sync::Arc;

    fn hub_for(_channel: &ChannelId) -> LiveHub {
        LiveHub::new()
    }

    fn fake_with(channel: &ChannelId) -> Arc<FakeDiscord> {
        let fake = Arc::new(FakeDiscord::new());
        fake.register_channel(channel);
        fake
    }

    #[test]
    fn change_hints_coalesce_only_within_their_provider_partition() {
        let hub = LiveHub::new();
        let channel = ChannelId("1111".to_owned());
        let other = ChannelId("2222".to_owned());

        assert!(hub.queue_hint(Some("slack"), &channel));
        assert!(!hub.queue_hint(Some("slack"), &channel));
        assert!(hub.queue_hint(Some("other"), &channel));
        assert!(hub.queue_hint(None, &other));

        assert_eq!(
            hub.take_hints(Some("slack")),
            BTreeSet::from([channel.clone()])
        );
        assert!(hub.take_hints(Some("slack")).is_empty());
        assert_eq!(hub.take_hints(Some("other")), BTreeSet::from([channel]));
        assert_eq!(hub.take_hints(None), BTreeSet::from([other]));
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_reconciliation_deadline_wins_over_a_pending_hint() {
        let hub = LiveHub::new();
        let channel = ChannelId("1111".to_owned());
        let mut generation = hub.subscribe_hints();
        assert!(hub.queue_hint(None, &channel));

        assert!(
            wait_for_hints(&hub, None, &mut generation, tokio::time::Instant::now())
                .await
                .is_none(),
            "a ready hint won after the full reconciliation deadline was already due"
        );
        assert_eq!(
            hub.take_hints(None),
            BTreeSet::from([channel]),
            "the deadline path should leave the hint for the full pass to consume"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_hints_cannot_bypass_the_polling_rate_floor() {
        let interval = Duration::from_secs(3_600);
        let channel = ChannelId("1111".to_owned());
        let fake = fake_with(&channel);
        let hub = Arc::new(LiveHub::new());
        let task_fake = Arc::clone(&fake);
        let task_hub = Arc::clone(&hub);
        let task_channel = channel.clone();
        let poller = tokio::spawn(async move {
            let mut cursors = BTreeMap::new();
            poll_loop_with_channels(
                task_fake.as_ref(),
                task_hub.as_ref(),
                || vec![task_channel.clone()],
                &mut cursors,
                LoopConfig {
                    limit: 50,
                    interval,
                    ticks: Some(3),
                    hints: Some((task_hub.as_ref(), None)),
                },
            )
            .await;
        });

        while fake.fetch_count() < 1 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        assert!(hub.queue_hint(None, &channel));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(HINT_COALESCE_MILLIS)).await;
        while fake.fetch_count() < 2 {
            tokio::task::yield_now().await;
        }
        let first_hint_read = tokio::time::Instant::now();
        tokio::task::yield_now().await;

        assert!(hub.queue_hint(None, &channel));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(HINT_COALESCE_MILLIS)).await;
        let just_before_floor =
            first_hint_read + Duration::from_secs(MIN_POLL_SECONDS) - Duration::from_millis(1);
        tokio::time::advance(
            just_before_floor.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await;
        tokio::task::yield_now().await;
        assert_eq!(
            fake.fetch_count(),
            2,
            "a sustained hint stream bypassed the configured polling rate floor"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        poller.await.expect("poll task");
        assert_eq!(fake.fetch_count(), 3, "the deferred hint was never read");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hint_cannot_shorten_failure_backoff() {
        let interval = Duration::from_secs(10);
        let channel = ChannelId("1111".to_owned());
        // An unregistered channel fails every authoritative read.
        let fake = Arc::new(FakeDiscord::new());
        let hub = Arc::new(LiveHub::new());
        let task_fake = Arc::clone(&fake);
        let task_hub = Arc::clone(&hub);
        let task_channel = channel.clone();
        let poller = tokio::spawn(async move {
            let mut cursors = BTreeMap::new();
            poll_loop_with_channels(
                task_fake.as_ref(),
                task_hub.as_ref(),
                || vec![task_channel.clone()],
                &mut cursors,
                LoopConfig {
                    limit: 50,
                    interval,
                    ticks: Some(2),
                    hints: Some((task_hub.as_ref(), None)),
                },
            )
            .await;
        });

        while fake.fetch_count() < 1 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        assert!(hub.queue_hint(None, &channel));
        tokio::time::advance(Duration::from_secs(19)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            fake.fetch_count(),
            1,
            "a hint retried a failing provider before its doubled backoff elapsed"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        poller.await.expect("poll task");
        assert_eq!(fake.fetch_count(), 2, "the provider was not retried");
    }

    #[tokio::test(start_paused = true)]
    async fn steady_hints_cannot_starve_periodic_reconciliation() {
        let interval = Duration::from_secs(10);
        let noisy = ChannelId("1111".to_owned());
        let quiet = ChannelId("2222".to_owned());
        let fake = Arc::new(FakeDiscord::new());
        fake.register_channel(&noisy);
        fake.register_channel(&quiet);
        let hub = Arc::new(LiveHub::new());
        let task_fake = Arc::clone(&fake);
        let task_hub = Arc::clone(&hub);
        let task_noisy = noisy.clone();
        let task_quiet = quiet.clone();
        let poller = tokio::spawn(async move {
            let mut cursors = BTreeMap::new();
            poll_loop_with_channels(
                task_fake.as_ref(),
                task_hub.as_ref(),
                || vec![task_noisy.clone(), task_quiet.clone()],
                &mut cursors,
                LoopConfig {
                    limit: 50,
                    interval,
                    ticks: Some(3),
                    hints: Some((task_hub.as_ref(), None)),
                },
            )
            .await;
        });

        while fake.fetch_count() < 2 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(hub.queue_hint(None, &noisy));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(HINT_COALESCE_MILLIS)).await;
        while fake.fetch_count() < 3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fake.fetch_count(),
            3,
            "the hint should read only its noisy channel"
        );
        tokio::task::yield_now().await;

        // The original reconciliation deadline is still ten seconds after the startup pass,
        // not ten seconds after the hinted pass at 9.1 seconds. It must read BOTH channels now.
        tokio::time::advance(Duration::from_millis(900)).await;
        for _ in 0..100 {
            if poller.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            poller.is_finished(),
            "the hint moved the reconciliation deadline and starved the quiet channel"
        );
        poller.await.expect("poll task");
        assert_eq!(
            fake.fetch_count(),
            5,
            "the full pass did not read both channels"
        );
    }

    #[tokio::test]
    async fn the_first_tick_seeds_the_cursor_and_publishes_nothing() {
        let channel = ChannelId("1111".to_owned());
        let fake = fake_with(&channel);
        fake.seed(&channel, "codex", "said before anyone was listening");
        fake.seed(&channel, "codex", "and this too");
        let hub = hub_for(&channel);
        let mut cursor = None;

        let tick = poll_once(fake.as_ref(), &hub, &channel, 50, &mut cursor)
            .await
            .expect("poll");

        assert_eq!(
            tick.published, 0,
            "the first tick must publish nothing: existing history is not news"
        );
        assert!(cursor.is_some(), "the first tick must SEED the cursor");
        let subscription = hub.subscribe(&channel, None);
        assert!(
            subscription.replay.is_empty(),
            "a page attaching after the first tick must not be shown existing history as new"
        );
    }

    #[tokio::test]
    async fn a_message_seeded_between_ticks_is_published_exactly_once() {
        let channel = ChannelId("1111".to_owned());
        let fake = fake_with(&channel);
        fake.seed(&channel, "codex", "old");
        let hub = hub_for(&channel);
        let mut cursor = None;
        poll_once(fake.as_ref(), &hub, &channel, 50, &mut cursor)
            .await
            .expect("seed tick");

        fake.seed(&channel, "codex", "arrived");
        assert_eq!(
            poll_once(fake.as_ref(), &hub, &channel, 50, &mut cursor)
                .await
                .expect("poll")
                .published,
            1
        );
        assert_eq!(
            poll_once(fake.as_ref(), &hub, &channel, 50, &mut cursor)
                .await
                .expect("poll")
                .published,
            0,
            "a second tick with nothing new must publish nothing"
        );

        let subscription = hub.subscribe(&channel, None);
        assert_eq!(subscription.replay.len(), 1);
        assert_eq!(subscription.replay[0].message.content, "arrived");
    }

    #[tokio::test]
    async fn a_lower_snowflake_is_never_republished() {
        // The trap the cursor exists for: ordering must be NUMERIC. These two ids differ in
        // LENGTH, so the newer one is lexically SMALLER — a cursor comparison written on strings
        // publishes the old message forever and never publishes the new one.
        let channel = ChannelId("1111".to_owned());
        let fake = fake_with(&channel);
        let hub = hub_for(&channel);
        let older = fake.seed_at(&channel, "codex", "older", 1_420_070_400_001);
        let newer = fake.seed_at(&channel, "codex", "newer", 1_420_070_400_003);
        assert!(
            newer.as_str() < older.as_str() && newer.numeric() > older.numeric(),
            "this fixture is only a trap if the newer id is lexically smaller: {older} vs {newer}"
        );
        let mut cursor = older.numeric();

        let tick = poll_once(fake.as_ref(), &hub, &channel, 50, &mut cursor)
            .await
            .expect("poll");

        assert_eq!(
            tick.published, 1,
            "only the numerically newer message is new"
        );
        let subscription = hub.subscribe(&channel, None);
        assert_eq!(subscription.replay.len(), 1);
        assert_eq!(subscription.replay[0].message.content, "newer");
        assert_eq!(cursor, newer.numeric());
    }

    #[tokio::test]
    async fn a_failed_fetch_leaves_the_cursor_where_it_was() {
        let channel = ChannelId("1111".to_owned());
        let fake = fake_with(&channel);
        fake.seed(&channel, "codex", "old");
        let hub = hub_for(&channel);
        let mut cursor = None;
        poll_once(fake.as_ref(), &hub, &channel, 50, &mut cursor)
            .await
            .expect("seed tick");
        let seeded = cursor;

        fake.seed(&channel, "codex", "arrived while Discord was down");
        fake.fail_next("network down");
        assert!(poll_once(fake.as_ref(), &hub, &channel, 50, &mut cursor)
            .await
            .is_err());
        assert_eq!(cursor, seeded, "a failure must not advance the cursor");

        assert_eq!(
            poll_once(fake.as_ref(), &hub, &channel, 50, &mut cursor)
                .await
                .expect("recovered")
                .published,
            1,
            "what arrived during the failure must be published once it recovers, not skipped"
        );
    }

    #[tokio::test]
    async fn the_loop_survives_a_failure_and_keeps_polling() {
        let channel = ChannelId("1111".to_owned());
        let fake = fake_with(&channel);
        let hub = hub_for(&channel);
        fake.seed(&channel, "codex", "old");
        let mut receiver = hub.subscribe(&channel, None).receiver;
        let mut cursors = BTreeMap::new();
        let interval = Duration::from_millis(0);
        let channels = std::slice::from_ref(&channel);

        // One tick to seed the cursor.
        poll_loop(
            fake.as_ref(),
            &hub,
            channels,
            50,
            interval,
            &mut cursors,
            Some(1),
        )
        .await;
        let fetches_after_seed = fake.fetch_count();

        // Now something arrives WHILE Discord is failing, and the loop takes two more ticks: the
        // first fails, the second must still happen and must still publish. A loop that returned
        // on the error would leave the message undelivered forever.
        fake.seed(&channel, "codex", "after recovery");
        fake.fail_next("network down");
        poll_loop(
            fake.as_ref(),
            &hub,
            channels,
            50,
            interval,
            &mut cursors,
            Some(2),
        )
        .await;

        assert_eq!(
            fake.fetch_count() - fetches_after_seed,
            2,
            "the loop stopped fetching after the failure"
        );
        let got = receiver
            .try_recv()
            .expect("the message that arrived during the failure was never published");
        assert_eq!(got.message.content, "after recovery");
        assert!(
            receiver.try_recv().is_err(),
            "and it must be published ONCE, not once per tick"
        );
    }

    #[tokio::test]
    async fn removing_a_channel_discards_its_cursor_before_it_is_added_again() {
        let (state, fake) = crate::testing::state();
        let channel = ChannelId("3333333333333333333".to_owned());
        fake.seed(&channel, "codex", "present before the first poll");
        state
            .added_channels
            .write()
            .expect("added-channel allowlist")
            .push(crate::store::AddedChannel {
                channel: channel.clone(),
                label: "temporary".to_owned(),
                writable: false,
                registration_provider: None,
                added_at_ms: 1,
            });
        let mut cursors = BTreeMap::new();
        poll_state_loop(
            &state,
            None,
            state.chat.as_ref(),
            50,
            Duration::ZERO,
            &mut cursors,
            Some(1),
        )
        .await;
        assert!(
            cursors.contains_key(&channel),
            "the channel was never polled"
        );

        state
            .added_channels
            .write()
            .expect("added-channel allowlist")
            .clear();
        poll_state_loop(
            &state,
            None,
            state.chat.as_ref(),
            50,
            Duration::ZERO,
            &mut cursors,
            Some(1),
        )
        .await;
        assert!(
            !cursors.contains_key(&channel),
            "a removed channel kept its cursor"
        );

        fake.seed(&channel, "codex", "arrived while removed");
        state
            .added_channels
            .write()
            .expect("added-channel allowlist")
            .push(crate::store::AddedChannel {
                channel: channel.clone(),
                label: "temporary again".to_owned(),
                writable: false,
                registration_provider: None,
                added_at_ms: 2,
            });
        poll_state_loop(
            &state,
            None,
            state.chat.as_ref(),
            50,
            Duration::ZERO,
            &mut cursors,
            Some(1),
        )
        .await;
        assert!(
            state.live.subscribe(&channel, None).replay.is_empty(),
            "re-adding a channel published messages from while it was removed"
        );
    }

    #[tokio::test]
    async fn a_configured_channel_the_owner_hid_is_not_polled() {
        // `#199 removable-config-channels`. Hidden means off the list everywhere, and a poller
        // that kept reading the channel would still be spending the shared rate limit on it and
        // publishing to anyone who had a stream open.
        let (state, fake) = crate::testing::state();
        let hidden = ChannelId(crate::testing::WRITE_CHANNEL.to_owned());
        fake.seed(&hidden, "codex", "present before the first poll");
        state
            .hidden_channels
            .write()
            .expect("hidden-channel list")
            .insert(hidden.clone());
        let mut cursors = BTreeMap::new();
        poll_state_loop(
            &state,
            None,
            state.chat.as_ref(),
            50,
            Duration::ZERO,
            &mut cursors,
            Some(1),
        )
        .await;
        assert!(
            !cursors.contains_key(&hidden),
            "a hidden configured channel was polled"
        );
        assert!(
            cursors.contains_key(&ChannelId(crate::testing::READ_CHANNEL.to_owned())),
            "the channel still on the list was not polled"
        );
    }

    /// Run `ticks` ticks of the loop against `channel` and answer how long the loop WAITED.
    ///
    /// The clock is paused by the caller, so this is exact rather than approximate: tokio advances
    /// virtual time to the next timer deadline whenever everything is idle, and nothing here is
    /// idle for any other reason.
    async fn elapsed_over(
        fake: &FakeDiscord,
        hub: &LiveHub,
        channel: &ChannelId,
        interval: Duration,
        ticks: u32,
    ) -> Duration {
        let mut cursors = BTreeMap::new();
        let started = tokio::time::Instant::now();
        poll_loop(
            fake,
            hub,
            std::slice::from_ref(channel),
            50,
            interval,
            &mut cursors,
            Some(ticks),
        )
        .await;
        started.elapsed()
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_channel_really_backs_off_and_a_healthy_one_is_not_slowed_down() {
        // THE HEADLINE SAFETY CLAIM OF `#44 live-push`, and the one that had no test: the module
        // doc, the README and the commit body all say a failing channel backs off rather than
        // hammering a rate limit this server does not handle. Deleting the single line in
        // `poll_loop` that applies `backoff` changed NOTHING else in this suite — the loop still
        // fetched, still published, still recovered, still logged. So the assertion has to be
        // about the one observable that changes: how long the loop actually waited.
        let interval = Duration::from_secs(10);

        let healthy = ChannelId("1111".to_owned());
        let good = fake_with(&healthy);
        let hub = LiveHub::new();
        let quiet = elapsed_over(good.as_ref(), &hub, &healthy, interval, 3).await;
        assert_eq!(
            quiet,
            interval * 2,
            "three ticks of a healthy channel are two ordinary sleeps and nothing else"
        );

        // Never registered, so every fetch answers Discord's own 404 — a channel that keeps
        // failing rather than one that fails once.
        let broken = ChannelId("2222".to_owned());
        let bad = Arc::new(FakeDiscord::new());
        let noisy = elapsed_over(bad.as_ref(), &hub, &broken, interval, 3).await;

        // 20s after the first failure and 40s after the second: doubling, as `backoff` says.
        assert_eq!(
            noisy,
            Duration::from_secs(60),
            "a channel that keeps failing must be retried FURTHER apart each time; polling on the \
             ordinary interval through an outage is the request storm the interval floor and this \
             backoff exist together to prevent"
        );
        assert!(
            noisy > quiet,
            "a failing channel waited no longer than a healthy one: the backoff is not applied"
        );
        assert!(
            bad.fetch_count() >= 3,
            "and it must still be RETRIED — backing off is not giving up"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_the_client_could_not_clear_slows_this_loop_by_what_discord_asked_for() {
        // The seam that keeps this loop's backoff and the client's `Retry-After` from being two
        // rival notions of "slow down". The client waits out what it can afford; what arrives
        // here is a limit it could NOT clear, still carrying Discord's outstanding request. This
        // loop must not come back sooner than that — its own doubling says 20s after one failure,
        // and 20s is less than Discord asked for.
        //
        // Coming back early is not merely impolite. The client refuses to SEND before the window
        // opens, so an early tick does not become a request; it becomes a tick that accomplishes
        // nothing and fails again, and the channel stays dark until some later tick happens to
        // land after the window. Waiting the time Discord actually named is what makes the very
        // next tick the one that recovers.
        let interval = Duration::from_secs(10);
        assert_eq!(
            backoff(interval, 1, interval * MAX_BACKOFF_INTERVALS),
            Duration::from_secs(20),
            "the doubling backoff alone would come back well before Discord is ready"
        );

        let channel = ChannelId("3333".to_owned());
        // One attempt and almost no waiting budget, so the 429 surfaces here with its wait intact
        // rather than being slept out inside the client. This is the shape of a real long limit:
        // longer than any one caller should be held for.
        let fake = Arc::new(FakeDiscord::with_retry_policy(
            crate::discord::ratelimit::RetryPolicy {
                max_attempts: 1,
                max_total_wait: Duration::from_secs(1),
            },
        ));
        fake.register_channel(&channel);
        fake.seed(&channel, "codex", "said before anyone was listening");
        fake.rate_limit_next(1, Duration::from_secs(45), false);

        let hub = LiveHub::new();
        let elapsed = elapsed_over(fake.as_ref(), &hub, &channel, interval, 2).await;
        assert_eq!(
            elapsed,
            Duration::from_secs(45),
            "the loop came back before Discord said it could: the two waits are fighting rather \
             than nesting"
        );
        assert_eq!(
            fake.fetch_count(),
            2,
            "and the tick after the wait must actually READ — a loop that wakes early is held by \
             the client and recovers nothing"
        );
    }

    #[tokio::test]
    async fn a_burst_larger_than_one_page_is_published_in_full_rather_than_silently_dropped() {
        // The silent-loss bug this walk exists to rule out. Reading the most recent `limit`
        // messages and then moving the cursor to the newest of them drops everything in between
        // whenever a channel produces more than `limit` between two ticks: no gap event, no WARN,
        // and a page and an agent quietly missing lines while the README says nothing is skipped.
        let channel = ChannelId("1111".to_owned());
        let fake = fake_with(&channel);
        let hub = hub_for(&channel);
        let mut cursor = None;
        fake.seed(&channel, "codex", "before anyone was listening");
        poll_once(fake.as_ref(), &hub, &channel, 5, &mut cursor)
            .await
            .expect("seed tick");

        // Twelve messages, a page of five: two and a bit pages, well inside the tick ceiling.
        for n in 0..12 {
            fake.seed(&channel, "codex", &format!("burst {n}"));
        }
        let tick = poll_once(fake.as_ref(), &hub, &channel, 5, &mut cursor)
            .await
            .expect("poll");

        assert_eq!(
            tick.published, 12,
            "everything that arrived between two ticks must be published, not just the last page"
        );
        assert!(
            !tick.backlog,
            "twelve messages is not a backlog at four pages"
        );
        let replay = hub.subscribe(&channel, None).replay;
        assert_eq!(
            replay
                .iter()
                .map(|m| m.message.content.clone())
                .collect::<Vec<_>>(),
            (0..12).map(|n| format!("burst {n}")).collect::<Vec<_>>(),
            "and in order, oldest first, with none missing from the middle"
        );
        assert_eq!(
            poll_once(fake.as_ref(), &hub, &channel, 5, &mut cursor)
                .await
                .expect("poll")
                .published,
            0,
            "walking forward must not leave the cursor behind and republish the burst"
        );
    }

    #[tokio::test]
    async fn a_tick_stopped_by_its_page_ceiling_says_so_and_the_next_tick_continues() {
        // The honest bound. The walk is capped so that catching up cannot become a request storm,
        // and being capped has to be VISIBLE — otherwise "nothing is skipped" and "delivery is an
        // interval late" are the same silence.
        let channel = ChannelId("1111".to_owned());
        let fake = fake_with(&channel);
        let hub = hub_for(&channel);
        let mut cursor = None;
        poll_once(fake.as_ref(), &hub, &channel, 2, &mut cursor)
            .await
            .expect("seed tick");

        let ceiling = MAX_PAGES_PER_TICK * 2;
        for n in 0..(ceiling + 3) {
            fake.seed(&channel, "codex", &format!("m{n}"));
        }
        let first = poll_once(fake.as_ref(), &hub, &channel, 2, &mut cursor)
            .await
            .expect("poll");
        assert_eq!(first.published, ceiling, "the tick is bounded at {ceiling}");
        assert!(
            first.backlog,
            "a tick that stopped at its ceiling with more waiting must SAY it is behind"
        );

        let second = poll_once(fake.as_ref(), &hub, &channel, 2, &mut cursor)
            .await
            .expect("poll");
        assert_eq!(
            second.published, 3,
            "and the next tick continues from the cursor rather than skipping the remainder"
        );
        assert!(!second.backlog, "which is then no longer behind");
    }

    #[test]
    fn the_backoff_grows_and_then_stops_growing() {
        let interval = Duration::from_secs(10);
        let max = Duration::from_secs(160);
        assert_eq!(backoff(interval, 0, max), interval, "no failures, no delay");
        assert_eq!(backoff(interval, 1, max), Duration::from_secs(20));
        assert_eq!(backoff(interval, 2, max), Duration::from_secs(40));
        assert_eq!(
            backoff(interval, 30, max),
            max,
            "a channel that has been down all night must still be retried every {max:?}"
        );
    }

    #[test]
    fn the_replay_tail_is_bounded() {
        let channel = ChannelId("1111".to_owned());
        let hub = hub_for(&channel);
        for n in 0..(REPLAY_TAIL + 20) {
            hub.publish(&channel, message(&channel, 1_000 + n as u64, "spam"));
        }
        let subscription = hub.subscribe(&channel, None);
        assert_eq!(subscription.replay.len(), REPLAY_TAIL);
        assert_eq!(
            subscription.replay[0].message.id.as_str(),
            (1_000 + 20).to_string(),
            "the OLDEST entries are the ones dropped"
        );
    }

    #[test]
    fn adapter_retry_deduplication_is_bounded_and_evicts_oldest_first() {
        let channel = ChannelId("1111".to_owned());
        let hub = hub_for(&channel);
        for n in 0..=INGEST_DEDUPE_MEMORY {
            assert!(hub.publish_ingested(
                format!("event-{n}"),
                LiveKind::Create,
                false,
                &channel,
                message(&channel, n as u64 + 1, "m"),
            ));
        }
        assert!(
            hub.publish_ingested(
                "event-0".to_owned(),
                LiveKind::Create,
                false,
                &channel,
                message(&channel, 1, "retried after the window"),
            ),
            "the oldest id must eventually leave the bounded de-duplication window"
        );
        assert!(
            !hub.publish_ingested(
                format!("event-{INGEST_DEDUPE_MEMORY}"),
                LiveKind::Create,
                false,
                &channel,
                message(&channel, INGEST_DEDUPE_MEMORY as u64 + 1, "recent retry"),
            ),
            "a recent retry must still be suppressed"
        );
    }

    #[test]
    fn subscribing_after_an_id_replays_only_what_is_strictly_newer() {
        let channel = ChannelId("1111".to_owned());
        let hub = hub_for(&channel);
        for id in [10_u64, 20, 30] {
            hub.publish(&channel, message(&channel, id, "m"));
        }
        let subscription = hub.subscribe(&channel, Some(&MessageId("20".to_owned())));
        assert_eq!(
            subscription
                .replay
                .iter()
                .map(|m| m.message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["30"],
            "the message the caller already has must not come back"
        );
    }

    #[test]
    fn any_cursor_missing_from_the_tail_forces_a_reset_instead_of_resuming_over_a_gap() {
        let channel = ChannelId("1111".to_owned());
        let hub = hub_for(&channel);
        hub.publish(&channel, message(&channel, 10, "m"));
        for missing in ["garbage", "9", "11"] {
            let subscription = hub.subscribe(&channel, Some(&MessageId(missing.to_owned())));
            assert!(
                subscription.replay.is_empty(),
                "missing cursor {missing:?} cannot identify a safe replay boundary"
            );
            assert!(
                subscription.reset_on_attach,
                "missing cursor {missing:?} must force an authoritative re-read"
            );
        }
    }

    #[test]
    fn an_evicted_numeric_cursor_resets_instead_of_replaying_a_partial_tail() {
        let channel = ChannelId("1111".to_owned());
        let hub = hub_for(&channel);
        for id in 1..=(REPLAY_TAIL as u64 + 1) {
            hub.publish(&channel, message(&channel, id, "m"));
        }

        let subscription = hub.subscribe(&channel, Some(&MessageId("1".to_owned())));
        assert!(subscription.replay.is_empty());
        assert!(
            subscription.reset_on_attach,
            "the numeric comparison cannot reveal the event evicted between the cursor and tail"
        );
    }

    #[test]
    fn one_channels_messages_never_reach_another_channels_subscriber() {
        // The hub is keyed by channel, and it has to STAY keyed: a single shared feed would
        // deliver a private channel's text to a page watching a different one, and every other
        // test here would still pass.
        let hub = LiveHub::new();
        let watched = ChannelId("1111".to_owned());
        let other = ChannelId("2222".to_owned());
        let mut receiver = hub.subscribe(&watched, None).receiver;
        hub.publish(&other, message(&other, 10, "somewhere else"));
        assert!(
            receiver.try_recv().is_err(),
            "a message published to another channel reached this subscriber"
        );
        assert!(
            hub.subscribe(&watched, None).replay.is_empty(),
            "and it must not be in this channel's replay tail either"
        );
    }

    #[test]
    fn a_message_this_server_posted_is_published_marked_as_such() {
        let channel = ChannelId("1111".to_owned());
        let hub = hub_for(&channel);
        hub.note_self_posted(&MessageId("10".to_owned()));
        hub.publish(&channel, message(&channel, 10, "our own reply"));
        hub.publish(&channel, message(&channel, 11, "somebody else"));
        let subscription = hub.subscribe(&channel, None);
        assert_eq!(
            subscription
                .replay
                .iter()
                .map(|m| m.self_posted)
                .collect::<Vec<_>>(),
            vec![true, false],
            "without this flag the agent hears its own reply and answers it"
        );
    }

    #[test]
    fn the_self_posted_memory_is_bounded_and_forgets_the_oldest_first() {
        let hub = LiveHub::new();
        for n in 0..(SELF_POSTED_MEMORY + 5) {
            hub.note_self_posted(&MessageId(n.to_string()));
        }
        assert!(!hub.is_self_posted(&MessageId("0".to_owned())));
        assert!(hub.is_self_posted(&MessageId((SELF_POSTED_MEMORY + 4).to_string())));
    }

    fn message(channel: &ChannelId, id: u64, content: &str) -> Message {
        Message {
            thread: None,
            id: MessageId(id.to_string()),
            channel_id: channel.clone(),
            author: "codex".to_owned(),
            author_id: crate::model::UserId("7".to_owned()),
            author_is_bot: false,
            timestamp: "2026-08-20T10:00:00+00:00".to_owned(),
            spoken_time: String::new(),
            reply_to: None,
            content: content.to_owned(),
            spoken_content: String::new(),
            content_html: String::new(),
            noise: false,
            reactions: None,
            heading: None,
        }
    }
}
