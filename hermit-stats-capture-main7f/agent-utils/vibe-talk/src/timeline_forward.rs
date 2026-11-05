//! Forward timeline reads: what changed in a view since an earlier read. `#203 incremental-refresh`.
//!
//! The owner's question was whether a refresh at 7:15, after a read at 7:00, could ask for just
//! the quarter of an hour in between. Backward pages could not: every refresh re-read the newest
//! page of the whole view. A newest read now hands back `next_after`, and a read with
//! `after=<next_after>` answers only the entries new or changed in that view since then, oldest
//! first, with a [`crate::threads::TimelineDelta`] saying whether edits and deletions are included.
//!
//! # Two kinds of cursor behind one envelope
//!
//! * **Native** — the backend answers `after` itself, and issued its own cursor in
//!   [`TimelinePage::next_after`]. vibe-talk hands that cursor back untouched. A bridge that keeps
//!   a change record answers with `complete: true`, so one call carries every edit and deletion.
//! * **Generic** — any other backend. The cursor is a provider timestamp, an overlap below it, and
//!   the entries already seen inside that overlap. [`generic_since`] reads newest pages and keeps
//!   what is newer: correct for every backend, with `complete: false`, and no cheaper than a newest
//!   read. Entries with equal timestamps are told apart by the seen set, an entry the provider
//!   lists up to [`GENERIC_OVERLAP_SECONDS`] late is still returned, and the local clock is never
//!   compared with a provider time.
//!
//! The page only ever sees the envelope, and a backend never does: [`decode`] binds each cursor to
//! the channel, view and thread it was issued for, and refuses any other before anything is read.
//! It is unsigned, like the backends' own backward cursors, and for the same reason: forging one
//! only reads the same allowlisted channel the caller can already read.
//!
//! Every row a delta carries is an upsert by id. Delivering an entry twice is harmless; skipping
//! one is the defect this must not have, so every overlap here errs toward the former.

use std::collections::{HashMap, HashSet};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::chat::{ChatClient, ChatError};
use crate::model::{ChannelId, Message};
use crate::threads::{ThreadSummary, TimelineDelta, TimelinePage, TimelineRequest, TimelineView};

/// How far below the newest entry a generic cursor keeps looking, in seconds: an entry the
/// provider makes visible this much later than its own timestamp is still returned.
pub const GENERIC_OVERLAP_SECONDS: i64 = 120;
/// The most entries a generic cursor remembers as already delivered inside its overlap.
pub const GENERIC_SEEN: usize = 64;
/// The most newest pages one generic catch-up reads before it gives up and asks for a full read.
pub const GENERIC_PAGES: usize = 3;
/// The page size a generic catch-up asks of the backend: the largest the API serves.
const GENERIC_PAGE_LIMIT: u16 = crate::ops::MAX_PAGE;
/// The longest backend cursor the envelope carries, matching the backends' own cursor bound.
pub const MAX_BACKEND_CURSOR: usize = 8192;
/// The longest envelope the HTTP route accepts, matching its bound for `before`.
pub const MAX_ENVELOPE: usize = 16384;
/// The envelope format. A different one is refused rather than guessed at.
const VERSION: u8 = 1;

/// Where a generic forward read continues from. Opaque to the page.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GenericPosition {
    /// The newest provider timestamp delivered so far, RFC 3339.
    pub at: String,
    /// Entries older than this, RFC 3339, are never returned: `at` less the overlap, raised only
    /// when the seen set was capped or the newest page proved nothing below its own oldest row.
    pub floor: String,
    /// Keys of the entries already delivered at or above `floor`, newest first, hashed.
    #[serde(default)]
    pub seen: Vec<String>,
}

impl GenericPosition {
    /// The position before anything was ever posted: the first entry is new.
    #[must_use]
    pub fn origin() -> Self {
        let epoch = Timestamp::UNIX_EPOCH.to_string();
        Self {
            at: epoch.clone(),
            floor: epoch,
            seen: Vec::new(),
        }
    }

    fn instants(&self) -> Option<(Timestamp, Timestamp)> {
        let at = self.at.parse::<Timestamp>().ok()?;
        let floor = self.floor.parse::<Timestamp>().ok()?;
        (floor <= at).then_some((at, floor))
    }

    fn well_formed(&self) -> bool {
        self.instants().is_some()
            && self.seen.len() <= GENERIC_SEEN
            && self
                .seen
                .iter()
                .all(|key| key.len() == 16 && key.bytes().all(|b| b.is_ascii_hexdigit()))
    }
}

/// One generic forward read: the delta page, and the position that continues it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenericDelta {
    /// What changed, as a delta page with `complete: false`.
    pub page: TimelinePage,
    /// Where the next forward read continues from.
    pub position: GenericPosition,
}

/// What a page-visible `next_after` stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Forward {
    /// The backend's own cursor, to be handed back to it untouched.
    Native(String),
    /// A position for [`ChatClient::fetch_timeline_since`].
    Generic(GenericPosition),
}

/// A page-visible cursor that does not decode, or belongs to another channel, view or thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("that forward cursor was not issued for this channel and view")]
pub struct Mismatch;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    v: u8,
    c: String,
    w: TimelineView,
    t: Option<String>,
    k: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    b: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    p: Option<GenericPosition>,
}

/// Wrap a forward cursor for the page, bound to the channel, view and thread it was issued for.
///
/// # Errors
///
/// [`ChatError::Shape`] when a backend issued a cursor longer than [`MAX_BACKEND_CURSOR`], or the
/// envelope would exceed [`MAX_ENVELOPE`]: a cursor the route would refuse is no cursor at all.
pub fn encode(
    channel: &ChannelId,
    view: TimelineView,
    thread: Option<&str>,
    forward: &Forward,
) -> Result<String, ChatError> {
    let (k, b, p) = match forward {
        Forward::Native(own) => {
            if own.is_empty() || own.len() > MAX_BACKEND_CURSOR {
                return Err(ChatError::Shape(format!(
                    "the backend issued a forward cursor of {} bytes; at most {MAX_BACKEND_CURSOR} are carried",
                    own.len()
                )));
            }
            ("n", Some(own.clone()), None)
        }
        Forward::Generic(position) => ("g", None, Some(position.clone())),
    };
    let envelope = Envelope {
        v: VERSION,
        c: channel.0.clone(),
        w: view,
        t: thread.map(str::to_owned),
        k: k.to_owned(),
        b,
        p,
    };
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|_| ChatError::Shape("could not encode the forward cursor".to_owned()))?;
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    if encoded.len() > MAX_ENVELOPE {
        return Err(ChatError::Shape(format!(
            "the forward cursor would be {} characters; at most {MAX_ENVELOPE} are accepted",
            encoded.len()
        )));
    }
    Ok(encoded)
}

/// Unwrap a page-visible cursor, refusing one issued for any other channel, view or thread.
///
/// # Errors
///
/// [`Mismatch`] for anything that is not an envelope this server issued for exactly this read.
pub fn decode(
    raw: &str,
    channel: &ChannelId,
    view: TimelineView,
    thread: Option<&str>,
) -> Result<Forward, Mismatch> {
    if raw.is_empty() || raw.len() > MAX_ENVELOPE {
        return Err(Mismatch);
    }
    let envelope: Envelope = URL_SAFE_NO_PAD
        .decode(raw)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(Mismatch)?;
    if envelope.v != VERSION
        || envelope.c != channel.as_str()
        || envelope.w != view
        || envelope.t.as_deref() != thread
    {
        return Err(Mismatch);
    }
    match (envelope.k.as_str(), envelope.b, envelope.p) {
        ("n", Some(own), None) if !own.is_empty() && own.len() <= MAX_BACKEND_CURSOR => {
            Ok(Forward::Native(own))
        }
        ("g", None, Some(position)) if position.well_formed() => Ok(Forward::Generic(position)),
        _ => Err(Mismatch),
    }
}

/// An entry of a view: a message, or a thread summary in the thread list.
trait Entry: Clone {
    fn id(&self) -> &str;
    /// When it last changed in a way the view orders by, if the provider said parseably.
    fn when(&self) -> Option<Timestamp>;
    /// What makes this version of it distinct from an earlier one.
    fn key(&self) -> String;
}

impl Entry for Message {
    fn id(&self) -> &str {
        self.id.as_str()
    }
    fn when(&self) -> Option<Timestamp> {
        self.timestamp.parse().ok()
    }
    // A message changes when its reactions do (`#219 emoji-reactions`): the same message with
    // other tallies is new, so a reaction inside the overlap reaches the page with the next delta.
    // That is when a delivery acknowledgement arrives, seconds after the message it acknowledges.
    // A copy that cannot see reactions keys by its id alone, as every message did before.
    fn key(&self) -> String {
        let Some(reactions) = &self.reactions else {
            return self.id.0.clone();
        };
        let mut key = format!("{}#", self.id.0);
        for reaction in reactions {
            key.push_str(&format!(
                "{}\u{1f}{}\u{1f}{}\u{1e}",
                reaction.custom_id.as_deref().unwrap_or(""),
                reaction.emoji,
                reaction.count
            ));
        }
        key
    }
}

impl Entry for ThreadSummary {
    fn id(&self) -> &str {
        &self.id
    }
    fn when(&self) -> Option<Timestamp> {
        self.updated_at.parse().ok()
    }
    // A summary changes when its thread moves; the same thread at a later activity time is new.
    fn key(&self) -> String {
        format!("{}@{}", self.id, self.updated_at)
    }
}

/// A key as the cursor holds it: FNV-1a, 64 bits, in hex. Short whatever the provider's ids look
/// like, so the seen set cannot push the envelope past the route's bound, and stable across
/// releases, unlike the standard library's hasher. A collision could only hide one entry from one
/// overlap window, with odds of about one in 2^58 for a full seen set.
fn hashed(key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn overlap() -> SignedDuration {
    SignedDuration::from_secs(GENERIC_OVERLAP_SECONDS)
}

/// The newest `GENERIC_SEEN` of `candidates` inside `[at - overlap, at]`, and the floor that goes
/// with them: raised to the oldest kept when the window held more than the cap.
fn window(at: Timestamp, candidates: HashMap<String, Timestamp>) -> (Timestamp, Vec<String>) {
    let start = at.checked_sub(overlap()).unwrap_or(Timestamp::MIN);
    let mut inside: Vec<(Timestamp, String)> = candidates
        .into_iter()
        .filter(|(_, when)| *when >= start && *when <= at)
        .map(|(key, when)| (when, key))
        .collect();
    inside.sort_by(|a, b| b.cmp(a));
    let capped = inside.len() > GENERIC_SEEN;
    inside.truncate(GENERIC_SEEN);
    let floor = match inside.last() {
        Some((oldest, _)) if capped => start.max(*oldest),
        _ => start,
    };
    (floor, inside.into_iter().map(|(_, key)| key).collect())
}

fn position_of_entries<E: Entry>(entries: &[E], has_more: bool) -> GenericPosition {
    let Some(at) = entries.iter().filter_map(Entry::when).max() else {
        // Nothing dated: the first entry ever posted, or the first with a readable time, is new.
        let mut origin = GenericPosition::origin();
        origin.seen = entries
            .iter()
            .rev()
            .take(GENERIC_SEEN)
            .map(|e| hashed(&e.key()))
            .collect();
        return origin;
    };
    // An entry without a readable time is treated as at the newest position, so it is delivered
    // once and then remembered for as long as anything is.
    let candidates = entries
        .iter()
        .map(|entry| (hashed(&entry.key()), entry.when().unwrap_or(at)))
        .collect();
    let (mut floor, seen) = window(at, candidates);
    if has_more {
        // A page that stops short proves nothing below its own oldest row: what is older is history
        // for a step back, not news for a step forward.
        if let Some(oldest) = entries.first().and_then(Entry::when) {
            floor = floor.max(oldest.min(at));
        }
    }
    GenericPosition {
        at: at.to_string(),
        floor: floor.to_string(),
        seen,
    }
}

/// The generic position a newest page establishes for its view.
#[must_use]
pub fn position_of(page: &TimelinePage, view: TimelineView) -> GenericPosition {
    if view == TimelineView::Threads {
        position_of_entries(&page.threads, page.has_more)
    } else {
        position_of_entries(&page.messages, page.has_more)
    }
}

/// What one catch-up collected from one view's newest pages.
struct Collected<E> {
    /// New or changed entries, newest page first, as found.
    found: Vec<(Timestamp, E)>,
    /// Every key at or above the floor, with its time: the seen set's next window comes from it.
    observed: HashMap<String, Timestamp>,
}

impl<E: Entry> Collected<E> {
    fn new() -> Self {
        Self {
            found: Vec::new(),
            observed: HashMap::new(),
        }
    }

    /// Fold one page in. True when the page reached below the floor, or was the last.
    fn take(
        &mut self,
        entries: Vec<E>,
        has_more: bool,
        at: Timestamp,
        floor: Timestamp,
        seen: &HashSet<&str>,
    ) -> bool {
        let reached = !has_more
            || entries
                .first()
                .is_none_or(|first| first.when().unwrap_or(at) < floor);
        for entry in entries {
            let when = entry.when().unwrap_or(at);
            if when < floor {
                continue;
            }
            let key = hashed(&entry.key());
            self.observed.insert(key.clone(), when);
            if !seen.contains(key.as_str()) {
                self.found.push((when, entry));
            }
        }
        reached
    }

    /// The oldest `limit` of what was found, oldest first, whether more remained, and the next
    /// position, whose floor is never below `floor`, the one this read started from.
    fn finish(
        mut self,
        limit: usize,
        since: &GenericPosition,
        at: Timestamp,
        floor: Timestamp,
    ) -> (Vec<E>, bool, GenericPosition) {
        // One copy of each, the first found: pages are read newest first, so it is the newest.
        let mut ids = HashSet::new();
        self.found
            .retain(|(_, entry)| ids.insert(entry.id().to_owned()));
        // Stable, so equal times keep the order the backend served them in.
        self.found.sort_by_key(|(when, _)| *when);
        let more = self.found.len() > limit;
        self.found.truncate(limit);
        let next_at = self
            .found
            .iter()
            .map(|(when, _)| *when)
            .max()
            .unwrap_or(at)
            .max(at);
        let mut candidates: HashMap<String, Timestamp> = since
            .seen
            .iter()
            .filter_map(|key| self.observed.get(key).map(|when| (key.clone(), *when)))
            .collect();
        for (when, entry) in &self.found {
            candidates.insert(hashed(&entry.key()), *when);
        }
        let (window_floor, seen) = window(next_at, candidates);
        // Never lower than where this read started. Below that floor everything was already ruled
        // out, and `observed` holds nothing from there, so a window that is not capped THIS time
        // would otherwise drop back to `at` less the overlap and offer, on the next read, the
        // entries an earlier cap pushed below the floor: every second catch-up after a busy
        // minute would send them again. `#203 incremental-refresh`.
        let position = GenericPosition {
            at: next_at.to_string(),
            floor: window_floor.max(floor).to_string(),
            seen,
        };
        (
            self.found.into_iter().map(|(_, entry)| entry).collect(),
            more,
            position,
        )
    }
}

/// What changed in a view since `since`, read from the backend's own newest pages.
///
/// The default behind [`ChatClient::fetch_timeline_since`]. Reads at most [`GENERIC_PAGES`]
/// newest pages, stopping at the first that reaches below the position's floor, and keeps every
/// entry at or above the floor that the position has not already delivered. The page's `thread`,
/// `has_threads` and `notice` are the newest page's, exactly as a newest read would say them.
///
/// # Errors
///
/// [`ChatError::CursorExpired`] when [`GENERIC_PAGES`] pages did not reach back to the floor, or
/// the position is not one this module issued; otherwise the backend's own errors.
pub async fn generic_since<C: ChatClient + ?Sized>(
    client: &C,
    channel: &ChannelId,
    request: &TimelineRequest,
    since: &GenericPosition,
) -> Result<GenericDelta, ChatError> {
    let (at, floor) = since.instants().ok_or_else(|| {
        ChatError::CursorExpired("the forward position is not one this server issued".to_owned())
    })?;
    let seen: HashSet<&str> = since.seen.iter().map(String::as_str).collect();
    let threads = request.view == TimelineView::Threads;
    let mut messages = Collected::<Message>::new();
    let mut summaries = Collected::<ThreadSummary>::new();
    let mut newest: Option<TimelinePage> = None;
    let mut before = None;
    let mut reached = false;
    for _ in 0..GENERIC_PAGES {
        let mut page = client
            .fetch_timeline(
                channel,
                &TimelineRequest {
                    view: request.view,
                    thread_id: request.thread_id.clone(),
                    before: before.take(),
                    after: None,
                    limit: GENERIC_PAGE_LIMIT,
                },
            )
            .await?;
        let has_more = page.has_more;
        let next = page.next_before.take();
        let done = if threads {
            summaries.take(
                std::mem::take(&mut page.threads),
                has_more,
                at,
                floor,
                &seen,
            )
        } else {
            messages.take(
                std::mem::take(&mut page.messages),
                has_more,
                at,
                floor,
                &seen,
            )
        };
        newest.get_or_insert(page);
        if done || next.is_none() {
            reached = true;
            break;
        }
        before = next;
    }
    if !reached {
        return Err(ChatError::CursorExpired(format!(
            "more changed in this view than {GENERIC_PAGES} pages hold; read the newest page again"
        )));
    }
    let newest = newest.unwrap_or_default();
    let limit = usize::from(request.limit.clamp(1, GENERIC_PAGE_LIMIT));
    let (page_messages, page_threads, more, position) = if threads {
        let (found, more, position) = summaries.finish(limit, since, at, floor);
        (Vec::new(), found, more, position)
    } else {
        let (found, more, position) = messages.finish(limit, since, at, floor);
        (found, Vec::new(), more, position)
    };
    Ok(GenericDelta {
        page: TimelinePage {
            messages: page_messages,
            threads: page_threads,
            thread: newest.thread,
            has_threads: newest.has_threads,
            has_more: false,
            next_before: None,
            notice: newest.notice,
            next_after: None,
            delta: Some(TimelineDelta {
                more,
                complete: false,
                deleted: Vec::new(),
                removed_threads: Vec::new(),
            }),
            as_of: newest.as_of,
        },
        position,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatIdentity;
    use crate::model::{MessageId, UserId};
    use std::sync::Mutex;

    const CHANNEL: &str = "100";

    fn channel() -> ChannelId {
        ChannelId(CHANNEL.to_owned())
    }

    fn message(id: &str, at: &str) -> Message {
        Message {
            id: MessageId(id.to_owned()),
            channel_id: channel(),
            author: "Reader".to_owned(),
            author_id: UserId("7".to_owned()),
            author_is_bot: false,
            timestamp: at.to_owned(),
            spoken_time: String::new(),
            reply_to: None,
            content: format!("message {id}"),
            spoken_content: String::new(),
            content_html: String::new(),
            thread: None,
            noise: false,
            reactions: None,
            heading: None,
        }
    }

    fn summary(id: &str, at: &str) -> ThreadSummary {
        ThreadSummary {
            id: id.to_owned(),
            root: None,
            title: format!("thread {id}"),
            reply_count: Some(1),
            reply_count_exact: true,
            updated_at: at.to_owned(),
            display_name: None,
            summary: None,
        }
    }

    /// A backend serving one view's history newest page first, `page` entries at a time.
    struct History {
        messages: Mutex<Vec<Message>>,
        threads: Mutex<Vec<ThreadSummary>>,
        page: usize,
        reads: Mutex<usize>,
    }

    impl History {
        fn of(messages: Vec<Message>, page: usize) -> Self {
            Self {
                messages: Mutex::new(messages),
                threads: Mutex::new(Vec::new()),
                page,
                reads: Mutex::new(0),
            }
        }
        fn push(&self, message: Message) {
            self.messages.lock().unwrap().push(message);
        }
        fn reads(&self) -> usize {
            *self.reads.lock().unwrap()
        }
    }

    fn cut<E: Clone>(
        all: &[E],
        before: Option<&str>,
        page: usize,
    ) -> (Vec<E>, bool, Option<String>) {
        let end = before.map_or(all.len(), |b| b.parse().unwrap());
        let start = end.saturating_sub(page);
        let more = start > 0;
        (
            all[start..end].to_vec(),
            more,
            more.then(|| start.to_string()),
        )
    }

    #[async_trait::async_trait]
    impl ChatClient for History {
        async fn identity(&self) -> Result<ChatIdentity, ChatError> {
            unreachable!()
        }
        async fn fetch_page(
            &self,
            _: &ChannelId,
            _: u16,
            _: Option<&MessageId>,
            _: Option<&MessageId>,
        ) -> Result<Vec<Message>, ChatError> {
            unreachable!()
        }
        async fn post_message(
            &self,
            _: &ChannelId,
            _: &str,
            _: Option<&MessageId>,
        ) -> Result<Message, ChatError> {
            unreachable!()
        }
        async fn fetch_timeline(
            &self,
            _: &ChannelId,
            request: &TimelineRequest,
        ) -> Result<TimelinePage, ChatError> {
            assert!(
                request.after.is_none(),
                "a generic read sends no backend cursor"
            );
            *self.reads.lock().unwrap() += 1;
            if request.view == TimelineView::Threads {
                let all = self.threads.lock().unwrap().clone();
                let (threads, has_more, next_before) =
                    cut(&all, request.before.as_deref(), self.page);
                return Ok(TimelinePage {
                    threads,
                    has_more,
                    next_before,
                    has_threads: true,
                    ..TimelinePage::default()
                });
            }
            let all = self.messages.lock().unwrap().clone();
            let (messages, has_more, next_before) = cut(&all, request.before.as_deref(), self.page);
            Ok(TimelinePage {
                messages,
                has_more,
                next_before,
                notice: Some("current notice".to_owned()),
                ..TimelinePage::default()
            })
        }
    }

    fn newest(view: TimelineView) -> TimelineRequest {
        TimelineRequest {
            view,
            limit: 50,
            ..TimelineRequest::default()
        }
    }

    async fn start(history: &History, view: TimelineView) -> GenericPosition {
        let page = history
            .fetch_timeline(&channel(), &newest(view))
            .await
            .unwrap();
        position_of(&page, view)
    }

    async fn since(history: &History, position: &GenericPosition) -> (Vec<String>, GenericDelta) {
        let delta = generic_since(history, &channel(), &newest(TimelineView::Main), position)
            .await
            .expect("a generic delta");
        let ids = delta.page.messages.iter().map(|m| m.id.0.clone()).collect();
        (ids, delta)
    }

    #[test]
    fn an_envelope_round_trips_and_is_bound_to_its_channel_view_and_thread() {
        let native = Forward::Native("bridge-cursor".to_owned());
        let raw = encode(&channel(), TimelineView::Thread, Some("t/1"), &native).unwrap();
        assert_eq!(
            decode(&raw, &channel(), TimelineView::Thread, Some("t/1")),
            Ok(native)
        );
        for (channel, view, thread) in [
            (ChannelId("101".into()), TimelineView::Thread, Some("t/1")),
            (channel(), TimelineView::Flat, None),
            (channel(), TimelineView::Thread, Some("t/2")),
        ] {
            assert_eq!(decode(&raw, &channel, view, thread), Err(Mismatch));
        }
        let generic = Forward::Generic(GenericPosition::origin());
        let raw = encode(&channel(), TimelineView::Main, None, &generic).unwrap();
        assert_eq!(
            decode(&raw, &channel(), TimelineView::Main, None),
            Ok(generic)
        );
        assert!(raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }

    #[test]
    fn malformed_and_oversized_cursors_are_refused() {
        assert_eq!(
            decode("", &channel(), TimelineView::Main, None),
            Err(Mismatch)
        );
        assert_eq!(
            decode("not base64!", &channel(), TimelineView::Main, None),
            Err(Mismatch)
        );
        let junk = URL_SAFE_NO_PAD.encode(br#"{"v":1}"#);
        assert_eq!(
            decode(&junk, &channel(), TimelineView::Main, None),
            Err(Mismatch)
        );
        let long = "x".repeat(MAX_BACKEND_CURSOR + 1);
        assert!(matches!(
            encode(&channel(), TimelineView::Main, None, &Forward::Native(long)),
            Err(ChatError::Shape(_))
        ));
        let mut bad = GenericPosition::origin();
        bad.at = "yesterday".to_owned();
        let raw = encode(&channel(), TimelineView::Main, None, &Forward::Generic(bad)).unwrap();
        assert_eq!(
            decode(&raw, &channel(), TimelineView::Main, None),
            Err(Mismatch)
        );
        // A forward cursor at the native bound still fits the route's bound once wrapped.
        let widest = encode(
            &channel(),
            TimelineView::Thread,
            Some(&"t".repeat(1024)),
            &Forward::Native("c".repeat(MAX_BACKEND_CURSOR)),
        )
        .unwrap();
        assert!(widest.len() <= MAX_ENVELOPE);
    }

    #[tokio::test]
    async fn an_empty_view_starts_at_the_beginning_so_the_first_message_is_new() {
        let history = History::of(Vec::new(), 10);
        let position = start(&history, TimelineView::Main).await;
        assert_eq!(position, GenericPosition::origin());
        history.push(message("1", "2026-10-04T07:00:00Z"));
        let (ids, delta) = since(&history, &position).await;
        assert_eq!(ids, ["1"]);
        assert_eq!(
            delta.page.delta,
            Some(TimelineDelta {
                more: false,
                complete: false,
                ..TimelineDelta::default()
            })
        );
        assert_eq!(delta.page.notice.as_deref(), Some("current notice"));
        assert!(!delta.page.has_more && delta.page.next_before.is_none());
    }

    #[tokio::test]
    async fn equal_timestamps_are_told_apart_and_nothing_is_returned_twice() {
        let at = "2026-10-04T07:00:00Z";
        let history = History::of(vec![message("1", at), message("2", at)], 10);
        let position = start(&history, TimelineView::Main).await;
        history.push(message("3", at));
        let (ids, delta) = since(&history, &position).await;
        assert_eq!(
            ids,
            ["3"],
            "a message at the same second as the newest held is still new"
        );
        let (ids, delta) = since(&history, &delta.position).await;
        assert!(ids.is_empty(), "consecutive cursors returned {ids:?} again");
        history.push(message("4", "2026-10-04T07:00:01Z"));
        let (ids, _) = since(&history, &delta.position).await;
        assert_eq!(ids, ["4"]);
    }

    #[tokio::test]
    async fn a_message_listed_late_inside_the_overlap_is_still_returned() {
        let history = History::of(vec![message("1", "2026-10-04T07:00:00Z")], 10);
        let position = start(&history, TimelineView::Main).await;
        history.push(message("2", "2026-10-04T07:05:00Z"));
        let (ids, delta) = since(&history, &position).await;
        assert_eq!(ids, ["2"]);
        // Dated 100 s before the newest delivered, made visible only now.
        let mut all = history.messages.lock().unwrap().clone();
        all.insert(1, message("late", "2026-10-04T07:03:20Z"));
        *history.messages.lock().unwrap() = all;
        let (ids, delta) = since(&history, &delta.position).await;
        assert_eq!(ids, ["late"]);
        let (ids, _) = since(&history, &delta.position).await;
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn a_catch_up_reads_back_three_pages_and_beyond_that_asks_for_a_full_read() {
        let history = History::of(vec![message("0", "2026-10-04T06:00:00Z")], 5);
        let position = start(&history, TimelineView::Main).await;
        for n in 1..=12 {
            history.push(message(
                &n.to_string(),
                &format!("2026-10-04T07:{n:02}:00Z"),
            ));
        }
        let (ids, delta) = since(&history, &position).await;
        assert_eq!(ids.len(), 12, "{ids:?}");
        assert_eq!(ids.first().map(String::as_str), Some("1"));
        assert_eq!(history.reads(), 1 + 3);
        for n in 13..=30 {
            history.push(message(
                &n.to_string(),
                &format!("2026-10-04T08:{n:02}:00Z"),
            ));
        }
        let error = generic_since(
            &history,
            &channel(),
            &newest(TimelineView::Main),
            &delta.position,
        )
        .await
        .expect_err("eighteen changes on pages of five is past three pages");
        assert!(matches!(error, ChatError::CursorExpired(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_delta_past_the_limit_says_more_and_continues_without_a_gap() {
        let history = History::of(vec![message("0", "2026-10-04T06:00:00Z")], 50);
        let position = start(&history, TimelineView::Main).await;
        for n in 1..=7 {
            history.push(message(&n.to_string(), &format!("2026-10-04T07:00:0{n}Z")));
        }
        let mut request = newest(TimelineView::Main);
        request.limit = 3;
        let mut seen = Vec::new();
        let mut position = position;
        for _ in 0..4 {
            let delta = generic_since(&history, &channel(), &request, &position)
                .await
                .unwrap();
            seen.extend(delta.page.messages.iter().map(|m| m.id.0.clone()));
            position = delta.position;
            if !delta.page.delta.unwrap().more {
                break;
            }
        }
        assert_eq!(seen, ["1", "2", "3", "4", "5", "6", "7"]);
    }

    #[tokio::test]
    async fn a_full_overlap_raises_the_floor_instead_of_growing_the_cursor() {
        let base = Timestamp::from_second(1_790_000_000).unwrap();
        let messages: Vec<Message> = (0..80)
            .map(|n| {
                message(
                    &n.to_string(),
                    &(base + SignedDuration::from_secs(n)).to_string(),
                )
            })
            .collect();
        let history = History::of(messages, 99);
        let position = start(&history, TimelineView::Main).await;
        assert_eq!(position.seen.len(), GENERIC_SEEN);
        let floor: Timestamp = position.floor.parse().unwrap();
        assert_eq!(
            floor,
            base + SignedDuration::from_secs(80 - 64),
            "floor is the oldest kept"
        );
        let (ids, _) = since(&history, &position).await;
        assert!(ids.is_empty(), "the capped window re-delivered {ids:?}");
    }

    #[tokio::test]
    async fn consecutive_cursors_after_a_capped_window_never_send_its_entries_again() {
        // Eighty messages a second apart: the newest read keeps the newest 64 and raises its floor
        // above the other sixteen. The catch-up after it sees only those 64, which no longer cap the
        // window; its floor must not fall back below them, or the one after offers the sixteen again.
        let base = Timestamp::from_second(1_790_000_000).unwrap();
        let history = History::of(
            (0..80)
                .map(|n| {
                    message(
                        &n.to_string(),
                        &(base + SignedDuration::from_secs(n)).to_string(),
                    )
                })
                .collect(),
            99,
        );
        let mut position = start(&history, TimelineView::Main).await;
        let capped: Timestamp = position.floor.parse().unwrap();
        for step in 1..=4 {
            let (ids, delta) = since(&history, &position).await;
            assert!(ids.is_empty(), "catch-up {step} sent {ids:?} again");
            let floor: Timestamp = delta.position.floor.parse().unwrap();
            assert!(
                floor >= capped,
                "catch-up {step} lowered the floor to {floor}"
            );
            position = delta.position;
        }
        history.push(message(
            "new",
            &(base + SignedDuration::from_secs(80)).to_string(),
        ));
        let (ids, delta) = since(&history, &position).await;
        assert_eq!(ids, ["new"]);
        let (ids, _) = since(&history, &delta.position).await;
        assert!(ids.is_empty(), "{ids:?}");
    }

    #[tokio::test]
    async fn thread_summaries_are_new_again_when_their_activity_moves() {
        let history = History::of(Vec::new(), 10);
        *history.threads.lock().unwrap() = vec![
            summary("a", "2026-10-04T07:00:00Z"),
            summary("b", "2026-10-04T07:01:00Z"),
        ];
        let position = start(&history, TimelineView::Threads).await;
        *history.threads.lock().unwrap() = vec![
            summary("b", "2026-10-04T07:01:00Z"),
            summary("a", "2026-10-04T07:09:00Z"),
        ];
        let delta = generic_since(
            &history,
            &channel(),
            &newest(TimelineView::Threads),
            &position,
        )
        .await
        .unwrap();
        let ids: Vec<_> = delta.page.threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["a"]);
        assert!(delta.page.has_threads);
        assert_eq!(hashed("a@x"), hashed("a@x"));
        assert_ne!(hashed("a@x"), hashed("a@y"));
    }

    #[test]
    fn a_newest_page_that_stops_short_claims_nothing_below_its_oldest_row() {
        let page = TimelinePage {
            messages: vec![
                message("5", "2026-10-04T07:00:50Z"),
                message("6", "2026-10-04T07:01:00Z"),
            ],
            has_more: true,
            next_before: Some("x".into()),
            ..TimelinePage::default()
        };
        let position = position_of(&page, TimelineView::Main);
        assert_eq!(position.floor, "2026-10-04T07:00:50Z");
        assert_eq!(position.at, "2026-10-04T07:01:00Z");
        assert_eq!(position.seen.len(), 2);
    }

    fn reacted(id: &str, at: &str, tallies: &[(&str, u32)]) -> Message {
        Message {
            reactions: Some(
                tallies
                    .iter()
                    .map(|(emoji, count)| crate::model::Reaction {
                        emoji: (*emoji).to_owned(),
                        custom: false,
                        custom_id: None,
                        count: *count,
                    })
                    .collect(),
            ),
            ..message(id, at)
        }
    }

    #[tokio::test]
    async fn a_message_whose_reactions_changed_inside_the_overlap_is_new_again() {
        // `#219 emoji-reactions`. A delivery acknowledgement lands seconds after its message, well
        // inside the overlap, and is a change to it: the catch-up carries the message again.
        let at = "2026-10-04T07:00:00Z";
        let history = History::of(vec![reacted("1", at, &[])], 10);
        let position = start(&history, TimelineView::Main).await;
        *history.messages.lock().unwrap() = vec![reacted("1", at, &[("👀", 1)])];
        let (ids, delta) = since(&history, &position).await;
        assert_eq!(ids, ["1"], "the first acknowledgement was not a change");
        assert_eq!(
            delta.page.messages[0].reactions.as_ref().map(Vec::len),
            Some(1)
        );
        let (ids, delta) = since(&history, &delta.position).await;
        assert!(
            ids.is_empty(),
            "unchanged reactions were sent again: {ids:?}"
        );
        *history.messages.lock().unwrap() = vec![reacted("1", at, &[("👀", 1), ("✅", 1)])];
        let (ids, delta) = since(&history, &delta.position).await;
        assert_eq!(ids, ["1"], "the second acknowledgement was not a change");
        *history.messages.lock().unwrap() = vec![reacted("1", at, &[("👀", 1), ("✅", 2)])];
        let (ids, delta) = since(&history, &delta.position).await;
        assert_eq!(ids, ["1"], "a count going up was not a change");
        let (ids, _) = since(&history, &delta.position).await;
        assert!(ids.is_empty());
    }

    #[test]
    fn a_copy_that_cannot_see_reactions_keys_by_its_id_as_before() {
        // Cursors issued before reactions existed hold keys of bare ids; a provider that reports
        // none must keep matching them, or every message in the overlap would come back once.
        let plain = message("1", "2026-10-04T07:00:00Z");
        assert_eq!(plain.key(), "1");
        let none = reacted("1", "2026-10-04T07:00:00Z", &[]);
        assert_ne!(
            none.key(),
            plain.key(),
            "known-none is an answer of its own"
        );
        assert_ne!(
            reacted("1", "2026-10-04T07:00:00Z", &[("👀", 1)]).key(),
            reacted("1", "2026-10-04T07:00:00Z", &[("👀", 2)]).key()
        );
    }
}
