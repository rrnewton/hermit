//! Pages, timeline views, posting, registration and discovery, on top of the bounded walks.
//!
//! # The four timeline views
//!
//! * **Main** — a whole conversation's own timeline, thread roots included, newest page first.
//!   For a channel narrowed to one thread it is that thread, root and replies, and the page names
//!   the thread in [`TimelinePage::thread`] with `has_threads` false.
//! * **Threads** — roots with replies, found by scanning the newest
//!   [`THREAD_SCAN_PAGES`] pages of history, ordered by their latest reply. A scan that stops on
//!   that bound says so in the page's notice.
//! * **Thread** — one thread's messages, after `conversations.replies` has confirmed the thread
//!   exists in this conversation.
//! * **Flat** — the Main page plus the replies of the threads rooted on it, at most
//!   [`FLAT_MAX_THREADS`] threads and the newest [`FLAT_MAX_REPLIES`] replies of each, merged
//!   oldest first. A notice states the bound whenever it applied. Only the threads whose root
//!   moved since the last Flat read are read again, and only from their newest reply held; every
//!   thread is read in full at least every [`FLAT_RECONCILE`] (`HttpSlackClient::flat_replies`).
//!
//! Cursors are opaque, bound to the channel, view and thread they were issued for, and carry a
//! Slack `ts` (or, for the thread list, an activity instant and a root) as their boundary.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures_util::{stream, StreamExt as _, TryStreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::client::{refused, rfc3339, shape, HttpSlackClient, Raw};
use super::history::{Feed, SCAN_PAGE_LIMIT};
use super::ids::{self, ChannelRef};
use crate::chat::{ChatError, RegisteredChannel};
use crate::directory::{DirectoryEntry, DirectoryPage, DirectoryRequest, MAX_NAME_CHARS};
use crate::model::{ChannelId, Message, MessageId};
use crate::threads::{ThreadSummary, TimelineDelta, TimelinePage, TimelineRequest, TimelineView};

/// The most entries one timeline page returns.
pub const MAX_TIMELINE_LIMIT: u16 = 100;
/// Pages of history the thread list scans for roots.
pub const THREAD_SCAN_PAGES: usize = 10;
/// Threads whose replies one Flat page includes.
pub const FLAT_MAX_THREADS: usize = 10;
/// Newest replies of each thread one Flat page includes.
pub const FLAT_MAX_REPLIES: usize = 50;
/// How long the replies a Flat read remembers for a thread are trusted before the thread is read
/// in full again, unless the client was built with another period: the page's
/// `TIMELINE_RECONCILE_MS`. `#282 slack-flat-skip`.
pub const FLAT_RECONCILE: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Most threads whose Flat replies are remembered at once.
const FLAT_REMEMBERED_MAX: usize = 256;
/// Characters of a root's first line used as its thread title.
pub const THREAD_TITLE_CHARS: usize = 80;
/// Pages of `users.conversations` one discovery request may read while filtering.
pub const DISCOVERY_PAGES: usize = 10;
/// The page size asked of `users.conversations`.
const DISCOVERY_PAGE_LIMIT: u16 = 100;
/// The longest one timeline request may take, all of its requests together.
const TIMELINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// The conversation kinds the channel browser lists.
const DISCOVERY_TYPES: &str = "public_channel,private_channel,mpim,im";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    channel: String,
    view: TimelineView,
    thread_id: Option<String>,
    boundary: String,
}

impl Cursor {
    fn encode(&self) -> Result<String, ChatError> {
        serde_json::to_vec(self)
            .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
            .map_err(|_| shape("could not encode the timeline continuation"))
    }

    fn decode(
        raw: &str,
        channel: &ChannelId,
        request: &TimelineRequest,
    ) -> Result<Self, ChatError> {
        let cursor: Self = URL_SAFE_NO_PAD
            .decode(raw)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| refused("invalid timeline cursor; refresh this view"))?;
        if cursor.channel != channel.as_str()
            || cursor.view != request.view
            || cursor.thread_id != request.thread_id
        {
            return Err(refused(
                "the timeline cursor belongs to a different channel or view",
            ));
        }
        Ok(cursor)
    }

    /// The boundary as an instant, for every view but the thread list.
    fn instant(&self) -> Result<i64, ChatError> {
        ids::ts_micros(&self.boundary).ok_or_else(|| refused("invalid timeline cursor boundary"))
    }

    /// The boundary as `(activity, root)` instants, for the thread list.
    fn activity(&self) -> Result<(i64, i64), ChatError> {
        self.boundary
            .split_once(':')
            .and_then(|(activity, root)| Some((activity.parse().ok()?, root.parse().ok()?)))
            .ok_or_else(|| refused("invalid thread-list cursor"))
    }
}

/// A forward cursor: the newest `ts` a newest read or delta of this view delivered.
/// `#203 incremental-refresh`. A separate shape from the backward [`Cursor`], so neither can be
/// handed back as the other.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Forward {
    channel: String,
    view: TimelineView,
    thread_id: Option<String>,
    after: String,
}

impl Forward {
    fn encode(&self) -> Result<String, ChatError> {
        serde_json::to_vec(self)
            .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
            .map_err(|_| shape("could not encode the forward cursor"))
    }

    /// The cursor's `ts` as an instant, after checking it was issued for this very read.
    ///
    /// Anything else is [`ChatError::CursorExpired`], never a refusal: vibe-talk's envelope has
    /// already checked the channel, view and thread, so a cursor that does not fit here was issued
    /// before the channel's configuration changed, and the caller's answer is a newest read, which
    /// issues one that fits. `#203 incremental-refresh`.
    fn decode(raw: &str, channel: &ChannelId, request: &TimelineRequest) -> Result<i64, ChatError> {
        let gone = |detail: &str| ChatError::CursorExpired(detail.to_owned());
        let cursor: Self = URL_SAFE_NO_PAD
            .decode(raw)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| gone("this backend did not issue that forward cursor"))?;
        if cursor.channel != channel.as_str()
            || cursor.view != request.view
            || cursor.thread_id != request.thread_id
        {
            return Err(gone(
                "the forward cursor belongs to a different channel or view",
            ));
        }
        ids::ts_micros(&cursor.after).ok_or_else(|| gone("the forward cursor names no message"))
    }
}

/// Whether this view of this channel is read forward natively: one conversation's own timeline,
/// with `conversations.history oldest=`, and one thread, with `conversations.replies oldest=`.
/// `#203 incremental-refresh`. The thread list is not — a reply never moves an old root into
/// `history oldest=` — nor Flat, nor a channel narrowed to one thread; those are caught up by
/// vibe-talk's generic reader.
fn forward_native(target: &ChannelRef, view: TimelineView) -> bool {
    target.thread_ts.is_none() && matches!(view, TimelineView::Main | TimelineView::Thread)
}

fn validate_request(request: &TimelineRequest) -> Result<(), ChatError> {
    request.one_direction()?;
    if request.limit == 0
        || [&request.before, &request.after]
            .into_iter()
            .flatten()
            .any(|cursor| cursor.len() > 8192)
    {
        return Err(refused(
            "timeline page size must be positive and its cursor at most 8192 bytes",
        ));
    }
    let thread = request.thread_id.as_deref();
    if (request.view == TimelineView::Thread) != thread.is_some() {
        return Err(refused(
            "supply a thread id only when reading the thread view",
        ));
    }
    Ok(())
}

/// A thread id from a caller, as a canonical root `ts`.
fn thread_ts(thread_id: &str) -> Result<String, ChatError> {
    ids::canonical_ts(thread_id)
        .filter(|canonical| canonical == thread_id)
        .ok_or_else(|| refused("a Slack thread id is its root ts, such as 1700000000.000100"))
}

/// A cursor id from a caller, as an instant.
fn cursor_micros(id: &MessageId) -> Result<i64, ChatError> {
    ids::message_id_micros(id).ok_or_else(|| {
        refused(&format!(
            "{id} is not a message id this Slack backend issued"
        ))
    })
}

/// The first line of a root, trimmed to [`THREAD_TITLE_CHARS`].
fn title(root: Option<&Message>) -> String {
    let line = root
        .and_then(|root| root.content.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("");
    if line.is_empty() {
        return "Thread".to_owned();
    }
    if line.chars().count() <= THREAD_TITLE_CHARS {
        return line.to_owned();
    }
    let cut: String = line.chars().take(THREAD_TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// A display name the directory contract accepts: no control characters, bounded length.
fn directory_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = clean.trim();
    if clean.chars().count() <= MAX_NAME_CHARS {
        clean.to_owned()
    } else {
        clean.chars().take(MAX_NAME_CHARS).collect()
    }
}

/// `mpdm-ada--grace--alan-1` as `Group: ada, grace, alan`; anything else unchanged.
fn group_name(name: &str) -> String {
    name.strip_prefix("mpdm-")
        .and_then(|rest| rest.rsplit_once('-').map(|(members, _)| members))
        .map(|members| {
            format!(
                "Group: {}",
                members.split("--").collect::<Vec<_>>().join(", ")
            )
        })
        .unwrap_or_else(|| name.to_owned())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryCursor {
    /// Slack's cursor for the page to read; empty for the first page.
    c: String,
    /// Conversations of that page already consumed.
    o: usize,
}

/// The replies a Flat read last held for one thread, and what its root said when they were read.
/// `#282 slack-flat-skip`.
#[derive(Clone, Debug)]
pub(super) struct FlatThread {
    /// The root's `latest_reply` then, as an instant.
    latest_reply: i64,
    /// The root's `reply_count` then.
    reply_count: u64,
    /// The thread's newest real replies, oldest first, root excluded, at most
    /// [`FLAT_MAX_REPLIES`].
    replies: Vec<Raw>,
    /// When the Flat read that last read the thread in full began, so the reconcile period is
    /// counted from before anything it read: an edit made after this instant is not in it.
    full_at: std::time::Instant,
}

/// What Flat remembers between reads: the replies it holds for each thread, and when a read of
/// each thread last finished. `#282 slack-flat-skip`, remote-ops audit W8 review.
///
/// What is held for a thread is never older, reply by reply, than what any finished read of that
/// thread showed, so no read can take back an edit another has already shown. A Flat read stores
/// what it read only when no read of the thread finished after it began; a read that cannot store
/// drops what is held unless the two agree; and the thread view drops what is held when it shows
/// otherwise (`HttpSlackClient::settle_flat`, `HttpSlackClient::check_flat_against_thread`).
#[derive(Debug, Default)]
pub(super) struct FlatMemory {
    /// The replies held, by conversation and root `ts`.
    threads: std::collections::HashMap<(String, String), FlatThread>,
    /// When a read of each thread last finished, by conversation and root `ts`: a Flat read,
    /// whether it held what it read or not, or the thread view. Everything that read showed had
    /// been read by then.
    finished: std::collections::HashMap<(String, String), std::time::Instant>,
    /// The latest instant dropped from `finished` to keep it to [`FLAT_REMEMBERED_MAX`] threads,
    /// so it stands for every thread: a Flat read that began before it holds nothing it read.
    finished_floor: Option<std::time::Instant>,
}

impl FlatMemory {
    /// When a read of `key` last finished, as far as is remembered: never earlier than it did.
    fn finished_at(&self, key: &(String, String)) -> Option<std::time::Instant> {
        self.finished.get(key).copied().max(self.finished_floor)
    }

    /// Note that a read of `key` finished `now`, dropping the oldest note past
    /// [`FLAT_REMEMBERED_MAX`] threads into the floor that stands for every thread.
    fn note_finished(&mut self, key: &(String, String), now: std::time::Instant) {
        while self.finished.len() >= FLAT_REMEMBERED_MAX && !self.finished.contains_key(key) {
            let Some((oldest, at)) = self
                .finished
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, at)| (key.clone(), *at))
            else {
                break;
            };
            self.finished.remove(&oldest);
            self.finished_floor = self.finished_floor.max(Some(at));
        }
        self.finished.insert(key.clone(), now);
    }
}

/// Whether two gap-free stretches of one thread's replies, root excluded, oldest first, say the
/// same wherever they overlap, message by message as Slack sent them. Two that do not overlap
/// agree: neither says anything about the other's replies.
fn agree_where_they_overlap(held: &[Raw], read: &[&Raw]) -> bool {
    let (Some(first_read), Some(last_read)) = (read.first(), read.last()) else {
        return true;
    };
    let (Some(first_held), Some(last_held)) = (held.first(), held.last()) else {
        return true;
    };
    let overlap = first_read.micros.max(first_held.micros)..=last_read.micros.min(last_held.micros);
    let shown = read
        .iter()
        .filter(|raw| overlap.contains(&raw.micros))
        .map(|raw| (raw.micros, &raw.value));
    let kept = held
        .iter()
        .filter(|raw| overlap.contains(&raw.micros))
        .map(|raw| (raw.micros, &raw.value));
    shown.eq(kept)
}

/// One backward page of messages, before conversion.
struct RawPage {
    raws: Vec<Raw>,
    has_more: bool,
    next_before: Option<String>,
}

impl HttpSlackClient {
    /// [`crate::chat::ChatClient::fetch_page`], without the provider label.
    pub(super) async fn read_page(
        &self,
        channel: &ChannelId,
        limit: u16,
        before: Option<&MessageId>,
        after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        if before.is_some() && after.is_some() {
            return Err(refused(
                "before and after cannot both be given: the chat API does not define what that means",
            ));
        }
        let target = Self::channel_ref(channel)?;
        let want = usize::from(limit.max(1));
        let before = before.map(cursor_micros).transpose()?;
        let after = after.map(cursor_micros).transpose()?;
        let conversation = target.conversation.as_str();
        let raws = match (&target.thread_ts, after) {
            (Some(thread), Some(after)) => {
                self.replies_oldest_after(conversation, thread, Some(after), want)
                    .await?
            }
            (Some(thread), None) => {
                self.replies_newest_before(conversation, thread, before, want)
                    .await?
            }
            (None, Some(after)) => self.history_oldest_after(conversation, after, want).await?,
            (None, None) => self.history_newest(conversation, before, want).await?.real,
        };
        if raws.iter().any(Raw::is_thread_root) {
            self.mark_threaded(conversation);
        }
        Ok(self.messages(channel, &raws).await)
    }

    /// [`crate::chat::ChatClient::fetch_timeline`], without the provider label.
    pub(super) async fn timeline(
        &self,
        channel: &ChannelId,
        request: &TimelineRequest,
    ) -> Result<TimelinePage, ChatError> {
        tokio::time::timeout(TIMELINE_TIMEOUT, self.timeline_inner(channel, request))
            .await
            .map_err(|_| {
                refused("the timeline exceeded its time budget; no partial timeline was returned")
            })?
    }

    async fn timeline_inner(
        &self,
        channel: &ChannelId,
        request: &TimelineRequest,
    ) -> Result<TimelinePage, ChatError> {
        validate_request(request)?;
        let target = Self::channel_ref(channel)?;
        let cursor = request
            .before
            .as_deref()
            .map(|raw| Cursor::decode(raw, channel, request))
            .transpose()?;
        let limit = usize::from(request.limit.clamp(1, MAX_TIMELINE_LIMIT));
        let conversation = target.conversation.as_str();
        if let Some(after) = &request.after {
            // Only issued where it is answered, so a cursor here for any other view predates a
            // change to the channel — narrowed to one thread since, say: gone, as in
            // `Forward::decode`, and the page reads the newest page instead.
            if !forward_native(&target, request.view) {
                return Err(ChatError::CursorExpired(
                    "this view is not read forward by the Slack backend".to_owned(),
                ));
            }
            let after = Forward::decode(after, channel, request)?;
            return self
                .forward(channel, conversation, request, after, limit)
                .await;
        }
        if let Some(scope) = &target.thread_ts {
            return self
                .scoped_timeline(channel, conversation, scope, request, cursor, limit)
                .await;
        }
        let mut page = self
            .newest_or_older(channel, conversation, request, cursor, limit)
            .await?;
        if request.before.is_none() && forward_native(&target, request.view) {
            // The newest real message delivered, or the beginning of the conversation when there
            // is none: a forward read from there returns what came after it.
            let newest = page
                .messages
                .last()
                .and_then(|message| super::ts_from_message_id(&message.id))
                .unwrap_or_else(|| ids::micros_ts(0));
            page.next_after = Some(self.forward_cursor(channel, request, newest)?);
        }
        Ok(page)
    }

    fn forward_cursor(
        &self,
        channel: &ChannelId,
        request: &TimelineRequest,
        after: String,
    ) -> Result<String, ChatError> {
        Forward {
            channel: channel.0.clone(),
            view: request.view,
            thread_id: request.thread_id.clone(),
            after,
        }
        .encode()
    }

    /// What was posted in Main or one thread after `after`, oldest first, as a delta. `#203
    /// incremental-refresh`. Filtered by creation, so it carries no edit or deletion and says so
    /// with `complete: false`; the page then keeps its full poll for this channel.
    async fn forward(
        &self,
        channel: &ChannelId,
        conversation: &str,
        request: &TimelineRequest,
        after: i64,
        limit: usize,
    ) -> Result<TimelinePage, ChatError> {
        let (mut raws, thread) = if request.view == TimelineView::Thread {
            let thread = thread_ts(request.thread_id.as_deref().unwrap_or_default())?;
            let root = self.thread_root(conversation, &thread).await?;
            self.mark_threaded(conversation);
            let raws = self
                .replies_oldest_after(conversation, &thread, Some(after), limit + 1)
                .await?;
            (raws, Some(self.summary(channel, &root).await))
        } else {
            let raws = self
                .history_oldest_after(conversation, after, limit + 1)
                .await?;
            (raws, None)
        };
        // A server may repeat a thread's root on every replies page; it is not news.
        raws.retain(|raw| raw.micros > after);
        raws.sort_by_key(|raw| raw.micros);
        raws.dedup_by_key(|raw| raw.micros);
        let more = raws.len() > limit;
        raws.truncate(limit);
        if raws.iter().any(Raw::is_thread_root) {
            self.mark_threaded(conversation);
        }
        let next = raws
            .last()
            .map_or_else(|| ids::micros_ts(after), |raw| raw.ts.clone());
        Ok(TimelinePage {
            messages: self.messages(channel, &raws).await,
            has_threads: thread.is_some() || self.is_threaded(conversation),
            thread,
            next_after: Some(self.forward_cursor(channel, request, next)?),
            delta: Some(TimelineDelta {
                more,
                complete: false,
                deleted: Vec::new(),
                removed_threads: Vec::new(),
            }),
            ..TimelinePage::default()
        })
    }

    /// A newest or older page of an unscoped conversation's view.
    async fn newest_or_older(
        &self,
        channel: &ChannelId,
        conversation: &str,
        request: &TimelineRequest,
        cursor: Option<Cursor>,
        limit: usize,
    ) -> Result<TimelinePage, ChatError> {
        match request.view {
            TimelineView::Main => {
                let page = self
                    .main_raw(channel, conversation, request, cursor.as_ref(), limit)
                    .await?;
                Ok(TimelinePage {
                    messages: self.messages(channel, &page.raws).await,
                    has_threads: self.is_threaded(conversation),
                    has_more: page.has_more,
                    next_before: page.next_before,
                    ..TimelinePage::default()
                })
            }
            TimelineView::Flat => {
                self.flat(channel, conversation, request, cursor.as_ref(), limit)
                    .await
            }
            TimelineView::Threads => {
                self.thread_list(channel, conversation, request, cursor.as_ref(), limit)
                    .await
            }
            TimelineView::Thread => {
                let thread = thread_ts(request.thread_id.as_deref().unwrap_or_default())?;
                let root = self.thread_root(conversation, &thread).await?;
                self.mark_threaded(conversation);
                let page = self
                    .thread_raw(
                        channel,
                        conversation,
                        &thread,
                        request,
                        cursor.as_ref(),
                        limit,
                    )
                    .await?;
                Ok(TimelinePage {
                    messages: self.messages(channel, &page.raws).await,
                    thread: Some(self.summary(channel, &root).await),
                    has_threads: true,
                    has_more: page.has_more,
                    next_before: page.next_before,
                    ..TimelinePage::default()
                })
            }
        }
    }

    /// A channel narrowed to one thread: its Main is the whole thread, and it has no children.
    async fn scoped_timeline(
        &self,
        channel: &ChannelId,
        conversation: &str,
        scope: &str,
        request: &TimelineRequest,
        cursor: Option<Cursor>,
        limit: usize,
    ) -> Result<TimelinePage, ChatError> {
        if request
            .thread_id
            .as_deref()
            .is_some_and(|thread| thread != scope)
        {
            return Err(refused(
                "that thread does not belong to the configured channel",
            ));
        }
        let root = self.thread_root(conversation, scope).await?;
        let summary = self.summary(channel, &root).await;
        if request.view == TimelineView::Threads {
            return Ok(TimelinePage {
                thread: Some(summary),
                ..TimelinePage::default()
            });
        }
        let page = self
            .thread_raw(
                channel,
                conversation,
                scope,
                request,
                cursor.as_ref(),
                limit,
            )
            .await?;
        Ok(TimelinePage {
            messages: self.messages(channel, &page.raws).await,
            thread: Some(summary),
            has_threads: false,
            has_more: page.has_more,
            next_before: page.next_before,
            ..TimelinePage::default()
        })
    }

    /// One backward page of a conversation's own timeline.
    async fn main_raw(
        &self,
        channel: &ChannelId,
        conversation: &str,
        request: &TimelineRequest,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<RawPage, ChatError> {
        let before = cursor.map(Cursor::instant).transpose()?;
        let run = self.history_newest(conversation, before, limit + 1).await?;
        let mut raws = run.real;
        let overflow = raws.len() > limit;
        if overflow {
            raws.drain(..raws.len() - limit);
        }
        if raws.iter().any(Raw::is_thread_root) {
            self.mark_threaded(conversation);
        }
        // On its page budget the walk stopped short, not at the end: continue from where it
        // stopped, which is at or before the oldest message returned.
        let boundary = if overflow {
            raws.first().map(|raw| raw.micros)
        } else {
            run.exhausted_at
        };
        let next_before = boundary
            .map(|micros| self.cursor(channel, request, ids::micros_ts(micros)))
            .transpose()?;
        Ok(RawPage {
            raws,
            has_more: next_before.is_some(),
            next_before,
        })
    }

    /// One backward page of one thread, root included on its oldest page.
    async fn thread_raw(
        &self,
        channel: &ChannelId,
        conversation: &str,
        thread: &str,
        request: &TimelineRequest,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<RawPage, ChatError> {
        let before = cursor.map(Cursor::instant).transpose()?;
        let mut raws = self
            .replies_newest_before(conversation, thread, before, limit + 1)
            .await?;
        let has_more = raws.len() > limit;
        if has_more {
            raws.drain(..raws.len() - limit);
        }
        self.check_flat_against_thread(conversation, thread, &raws);
        let next_before = if has_more {
            Some(self.cursor(channel, request, raws[0].ts.clone())?)
        } else {
            None
        };
        Ok(RawPage {
            raws,
            has_more,
            next_before,
        })
    }

    fn cursor(
        &self,
        channel: &ChannelId,
        request: &TimelineRequest,
        boundary: String,
    ) -> Result<String, ChatError> {
        Cursor {
            channel: channel.0.clone(),
            view: request.view,
            thread_id: request.thread_id.clone(),
            boundary,
        }
        .encode()
    }

    /// The Main page plus the replies of the threads rooted on it, bounded.
    async fn flat(
        &self,
        channel: &ChannelId,
        conversation: &str,
        request: &TimelineRequest,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<TimelinePage, ChatError> {
        let page = self
            .main_raw(channel, conversation, request, cursor, limit)
            .await?;
        let mut roots: Vec<&Raw> = page
            .raws
            .iter()
            .filter(|raw| raw.is_thread_root())
            .collect();
        roots.sort_by_key(|root| std::cmp::Reverse(root.activity_micros()));
        let bounded = roots.len() > FLAT_MAX_THREADS
            || roots
                .iter()
                .take(FLAT_MAX_THREADS)
                .any(|root| root.reply_count() > FLAT_MAX_REPLIES as u64);
        let reads: Vec<_> = roots
            .iter()
            .take(FLAT_MAX_THREADS)
            .map(|root| self.flat_replies(conversation, root))
            .collect();
        let replies: Vec<Vec<Raw>> = stream::iter(reads).buffered(4).try_collect().await?;
        let mut raws = page.raws;
        raws.extend(replies.into_iter().flatten());
        raws.sort_by_key(|raw| raw.micros);
        raws.dedup_by_key(|raw| raw.micros);
        Ok(TimelinePage {
            messages: self.messages(channel, &raws).await,
            has_threads: self.is_threaded(conversation),
            has_more: page.has_more,
            next_before: page.next_before,
            notice: bounded.then(|| {
                format!(
                    "Flat view includes replies for at most {FLAT_MAX_THREADS} threads per page and \
                     the newest {FLAT_MAX_REPLIES} replies of each; open a thread to read all of it."
                )
            }),
            ..TimelinePage::default()
        })
    }

    /// The newest replies of one thread on a Flat page, read only as far as they changed. `#282
    /// slack-flat-skip`, remote-ops audit W8.
    ///
    /// The Flat read has just fetched `root` from history, and a root reports its `reply_count`
    /// and `latest_reply`. So a thread whose root says what it said when its replies were last
    /// read has nothing new: the replies held are reused, at no call. A thread whose root names a
    /// newer latest reply and a higher count is read forward from the newest reply held
    /// (`conversations.replies oldest=`), one call for up to a page of new replies, and kept only
    /// when exactly as many messages came back as the count grew by, every one of them a real
    /// reply, the last at the latest the root names. Every message newer than the newest held is
    /// new, so when the count grew by exactly that many, nothing older was deleted: a deletion
    /// lowers the count, and so would need more new messages than came back to hide. That holds
    /// only because system events are counted too (`replies_all_after`): one posted alongside a
    /// real reply, while an older reply was deleted, would otherwise make up the difference.
    /// Anything else reads the thread's newest replies in full, as every Flat read
    /// used to: a count that fell or did not add up (a deletion), more new replies than a Flat page
    /// shows of a thread, a thread never read, or one last read in full [`FLAT_RECONCILE`] ago or
    /// longer. A full read is held only when it agrees with the root: see the comment at the end.
    /// So a root that names no `latest_reply` is read in full on every Flat read, as it was before
    /// any of this, and so is a thread too long to count in one window (more than
    /// [`super::history::MAX_WINDOW_PAGES`] pages of replies); the saving depends on the server
    /// reporting `latest_reply`, and on the thread being short enough to count.
    ///
    /// An edit to a reply changes nothing on its root, so Flat shows it when that period runs out;
    /// the thread view, which reads the thread itself, shows it at once, and once it has, the next
    /// Flat read reads that thread in full (`check_flat_against_thread`). A Flat read of a thread
    /// that was already under way then, which may have read the thread before the edit, holds
    /// nothing it read, and so for a Flat read under way when another Flat read of the thread
    /// finishes: whichever finishes last, it cannot put back a copy older than an edit the other
    /// has already shown (`settle_flat`). The reconcile period is counted from the instant the
    /// full read began, so an edit made at any moment reaches Flat within one period of it.
    ///
    /// What a count cannot show is left to the reconcile: two lags in the server's replies feed
    /// that cancel out, a deletion it still lists and a new reply, not the newest, it does not yet
    /// show, leave the count and the latest reply as the root reports them.
    async fn flat_replies(&self, conversation: &str, root: &Raw) -> Result<Vec<Raw>, ChatError> {
        // Before anything is read or recalled: what this call holds can be no newer than this.
        let began = std::time::Instant::now();
        let key = (conversation.to_owned(), root.ts.clone());
        let latest = root.activity_micros();
        let count = root.reply_count();
        if let Some(held) = self.remembered_flat(&key) {
            if held.latest_reply == latest && held.reply_count == count {
                return Ok(held.replies);
            }
            let grew = usize::try_from(count.saturating_sub(held.reply_count))
                .ok()
                .filter(|grew| (1..=FLAT_MAX_REPLIES).contains(grew) && latest > held.latest_reply);
            if let Some(grew) = grew {
                let fresh = self
                    .replies_all_after(conversation, &root.ts, held.latest_reply, grew)
                    .await?
                    .filter(|fresh| {
                        fresh.len() == grew
                            && fresh.iter().all(Raw::is_real)
                            && fresh.last().is_some_and(|raw| raw.micros == latest)
                    });
                if let Some(fresh) = fresh {
                    let mut replies = held.replies;
                    replies.extend(fresh);
                    replies.sort_by_key(|raw| raw.micros);
                    replies.dedup_by_key(|raw| raw.micros);
                    replies.drain(..replies.len().saturating_sub(FLAT_MAX_REPLIES));
                    let hold = FlatThread {
                        latest_reply: latest,
                        reply_count: count,
                        replies: replies.clone(),
                        full_at: held.full_at,
                    };
                    self.settle_flat(key, &replies, Some(hold), began);
                    return Ok(replies);
                }
            }
        }
        let (mut replies, whole) = self
            .replies_newest_counted(conversation, &root.ts, FLAT_MAX_REPLIES)
            .await?;
        replies.retain(|raw| raw.micros != root.micros);
        // Held only when the read agrees with the root this Flat read fetched: it ends at the
        // latest reply the root names, and the whole thread, counted from the same calls, holds
        // exactly as many replies as the root counts. Counting the whole thread rather than the
        // newest FLAT_MAX_REPLIES kept makes the count mean as much for a thread of eighty
        // replies as for one of eight. A feed that lagged the root leaves a reply out, or still
        // lists one the root no longer counts; a reply posted after history was read, and perhaps
        // deleted before the next read, ends past the root's latest. A root that names no latest
        // reply is never held, since its `latest` is the root itself and the root is not among
        // `replies`: its count alone, which one reply deleted and another added leave unchanged,
        // is never trusted. Nor is a thread that could not be counted (`replies_newest_counted`:
        // too long to read in one window, or a walk resumed past a run of system events, which
        // may have returned nothing at all), or one whose count includes replies that are not
        // kept, system events in the thread. Each is read in full again on the next Flat read,
        // as before W8: no saving for it, and nothing stale.
        let agrees = replies.last().is_some_and(|raw| raw.micros == latest) && whole == Some(count);
        let hold = agrees.then(|| FlatThread {
            latest_reply: latest,
            reply_count: count,
            replies: replies.clone(),
            full_at: began,
        });
        self.settle_flat(key, &replies, hold, began);
        Ok(replies)
    }

    /// Forget the replies a Flat read holds for `thread` when a read of the thread itself shows
    /// them otherwise, so the next Flat read reads that thread in full and shows what the thread
    /// view just showed: an edited reply above all, which changes nothing on its root. Costs no
    /// call. `#282 slack-flat-skip`, remote-ops audit W8 review.
    ///
    /// `raws` is one gap-free stretch of the thread, root included when it reached that far, and
    /// what is held is another. Only where the two overlap are they compared, message by message
    /// as Slack sent them, so a thread view of the newest replies says nothing about older ones,
    /// and a reply newer than what is held is left to the root's count.
    ///
    /// Every such read is also noted as finished, whatever it found, so that a Flat read of the
    /// thread already under way, which may have read it before an edit this read shows, holds
    /// nothing (`settle_flat`). Nothing else could tell: such a Flat read may have nothing held to
    /// compare with yet, or be about to put back the very replies this drops.
    fn check_flat_against_thread(&self, conversation: &str, thread: &str, raws: &[Raw]) {
        let key = (conversation.to_owned(), thread.to_owned());
        let mut memory = self
            .flat_memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        memory.note_finished(&key, std::time::Instant::now());
        let read: Vec<&Raw> = raws.iter().filter(|raw| raw.ts != thread).collect();
        if memory
            .threads
            .get(&key)
            .is_some_and(|entry| !agree_where_they_overlap(&entry.replies, &read))
        {
            memory.threads.remove(&key);
        }
    }

    /// The replies held for a thread, unless they were read in full a reconcile period ago.
    fn remembered_flat(&self, key: &(String, String)) -> Option<FlatThread> {
        self.flat_memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .threads
            .get(key)
            .filter(|held| held.full_at.elapsed() < self.flat_reconcile)
            .cloned()
    }

    /// Settle what one Flat read of a thread leaves held for the next: `hold`, what it read, when
    /// that agrees with its root, else nothing. `read` is what it returns, and `began` when it
    /// began, before it recalled or read anything. `#282 slack-flat-skip`, remote-ops audit W8
    /// review.
    ///
    /// `hold` is stored only when no read of the thread finished after `began`, another Flat read
    /// or the thread view. Then everything this read fetched was fetched after every reply any
    /// finished read showed, and for a forward read, the replies it built on were held when it
    /// recalled them and nothing has replaced them since. Stored, it drops what a reconcile would
    /// reread anyway and, past [`FLAT_REMEMBERED_MAX`] threads, the one read in full longest ago.
    ///
    /// Otherwise nothing the client sees orders the two reads: one that began first may have read
    /// the thread last, held back by a rate limit, say, and an answer that arrives last may have
    /// been read first. So `hold` is not stored, and what is held is dropped too unless it agrees
    /// with `read` wherever the two overlap, since it may be older than what this read returns:
    /// the next Flat read then reads the thread in full. Two reads that read a thread alike, as
    /// overlapping polls usually do, keep their saving. A read that does not agree with its
    /// root drops what is held either way, as it always has.
    ///
    /// Either way the read is noted as finished, so that a read still under way that began before
    /// now can neither put its copy back over what this one left nor fill the place it emptied.
    /// The note, the check and the change happen under one lock, as do the note and the drop in
    /// `check_flat_against_thread`, so none of them interleave.
    fn settle_flat(
        &self,
        key: (String, String),
        read: &[Raw],
        hold: Option<FlatThread>,
        began: std::time::Instant,
    ) {
        let mut memory = self
            .flat_memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let finished = memory.finished_at(&key);
        memory.note_finished(&key, std::time::Instant::now());
        let held = &mut memory.threads;
        match hold {
            Some(thread) if finished.is_none_or(|at| at < began) => {
                held.retain(|_, entry| entry.full_at.elapsed() < self.flat_reconcile);
                while held.len() >= FLAT_REMEMBERED_MAX && !held.contains_key(&key) {
                    let Some(oldest) = held
                        .iter()
                        .min_by_key(|(_, entry)| entry.full_at)
                        .map(|(key, _)| key.clone())
                    else {
                        break;
                    };
                    held.remove(&oldest);
                }
                held.insert(key, thread);
            }
            Some(_) => {
                let read: Vec<&Raw> = read.iter().collect();
                if held
                    .get(&key)
                    .is_some_and(|entry| !agree_where_they_overlap(&entry.replies, &read))
                {
                    held.remove(&key);
                }
            }
            None => {
                held.remove(&key);
            }
        }
    }

    /// Thread roots with replies, from a bounded scan of recent history, by latest activity.
    async fn thread_list(
        &self,
        channel: &ChannelId,
        conversation: &str,
        request: &TimelineRequest,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<TimelinePage, ChatError> {
        let boundary = cursor.map(Cursor::activity).transpose()?;
        let mut roots: Vec<Raw> = Vec::new();
        let mut scanned = 0_usize;
        let mut complete = false;
        let mut page_cursor: Option<String> = None;
        for _ in 0..THREAD_SCAN_PAGES {
            let page = self
                .page(
                    Feed::History(conversation),
                    None,
                    None,
                    false,
                    SCAN_PAGE_LIMIT,
                    page_cursor.as_deref(),
                )
                .await?;
            scanned += page.raws.len();
            roots.extend(
                page.raws
                    .into_iter()
                    .filter(|raw| raw.is_real() && raw.is_thread_root()),
            );
            if !page.has_more {
                complete = true;
                break;
            }
            if page.next_cursor == page_cursor {
                return Err(shape("a paging cursor did not advance"));
            }
            page_cursor = page.next_cursor;
        }
        if !roots.is_empty() {
            self.mark_threaded(conversation);
        }
        let has_threads = !roots.is_empty();
        let key = |raw: &Raw| (raw.activity_micros(), raw.micros);
        roots.sort_by_key(key);
        roots.dedup_by_key(|raw| raw.micros);
        roots.retain(|raw| boundary.is_none_or(|boundary| key(raw) < boundary));
        let has_more = roots.len() > limit;
        if has_more {
            roots.drain(..roots.len() - limit);
        }
        let next_before = if has_more {
            let (activity, root) = key(&roots[0]);
            Some(self.cursor(channel, request, format!("{activity}:{root}"))?)
        } else {
            None
        };
        let mut threads = Vec::with_capacity(roots.len());
        for root in &roots {
            threads.push(self.summary(channel, root).await);
        }
        Ok(TimelinePage {
            threads,
            has_threads,
            has_more,
            next_before,
            notice: (!complete).then(|| {
                format!(
                    "Threads are found among the newest {scanned} messages of this conversation; \
                     older threads are not listed."
                )
            }),
            ..TimelinePage::default()
        })
    }

    /// The root of `thread` in `conversation`, which also proves the thread is there.
    ///
    /// Slack answers `thread_not_found` (HTTP 404 here) for a thread that is not in the
    /// conversation. A `ts` that names a reply is refused: a thread is identified by its root.
    pub(super) async fn thread_root(
        &self,
        conversation: &str,
        thread: &str,
    ) -> Result<Raw, ChatError> {
        let page = self
            .page(
                Feed::Replies(conversation, thread),
                None,
                None,
                false,
                1,
                None,
            )
            .await?;
        let root = page
            .raws
            .into_iter()
            .next()
            .ok_or_else(|| ChatError::Status {
                status: 404,
                body: "conversations.replies answered with no root message".to_owned(),
            })?;
        if root.ts != thread {
            return Err(refused(
                "that ts is not the root of a thread in the configured channel",
            ));
        }
        Ok(root)
    }

    async fn summary(&self, channel: &ChannelId, root: &Raw) -> ThreadSummary {
        let message = self
            .messages(channel, std::slice::from_ref(root))
            .await
            .into_iter()
            .next();
        ThreadSummary {
            id: root.ts.clone(),
            title: title(message.as_ref()),
            root: message,
            reply_count: Some(root.reply_count()),
            reply_count_exact: true,
            updated_at: rfc3339(root.activity_micros()),
            display_name: None,
            summary: None,
        }
    }

    /// [`crate::chat::ChatClient::post_message`], without the provider label.
    ///
    /// Slack has no reply pointer: its only reply is a thread reply. So `reply_to` on a whole
    /// conversation posts into that message's thread — starting one when it has none — and a
    /// channel narrowed to one thread always posts into that thread.
    pub(super) async fn post(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        let target = Self::channel_ref(channel)?;
        let conversation = target.conversation.as_str();
        let thread = match (&target.thread_ts, reply_to) {
            (Some(scope), _) => Some(scope.clone()),
            (None, Some(id)) => Some(self.reply_thread(conversation, id).await?),
            (None, None) => None,
        };
        self.post_raw(channel, conversation, content, thread.as_deref())
            .await
    }

    /// The thread a reply to `id` belongs in: its own, or the one it roots.
    async fn reply_thread(&self, conversation: &str, id: &MessageId) -> Result<String, ChatError> {
        let micros = cursor_micros(id)?;
        let page = self
            .page(
                Feed::History(conversation),
                Some(micros),
                Some(micros),
                true,
                1,
                None,
            )
            .await?;
        let target = page
            .raws
            .into_iter()
            .find(|raw| raw.micros == micros)
            .ok_or_else(|| {
                refused(
                    "that message is not in this conversation's own timeline; if it is a thread \
                     reply, reply in its thread instead",
                )
            })?;
        Ok(target.thread_ts().unwrap_or(target.ts))
    }

    async fn post_raw(
        &self,
        channel: &ChannelId,
        conversation: &str,
        content: &str,
        thread: Option<&str>,
    ) -> Result<Message, ChatError> {
        if content.trim().is_empty() {
            return Err(refused("message content is empty"));
        }
        // Measured as Slack is sent it, escapes and all: Slack truncates past its ceiling rather
        // than refusing, and a bridge in front of it may refuse sooner. `#263
        // post-split-per-provider`; the reply path has already cut a long post to fit.
        if let Some(refusal) = self.post_limit.refusal(content) {
            return Err(ChatError::Refused(refusal));
        }
        let mut params = vec![
            ("channel", conversation.to_owned()),
            ("text", super::mrkdwn::escape_outgoing(content)),
        ];
        if let Some(thread) = thread {
            params.push(("thread_ts", thread.to_owned()));
        }
        let value = self.call("chat.postMessage", &params).await?;
        if value.get("channel").and_then(Value::as_str) != Some(conversation) {
            return Err(shape("chat.postMessage answered for a different channel"));
        }
        let mut message = value
            .get("message")
            .filter(|message| message.is_object())
            .cloned()
            .ok_or_else(|| shape("chat.postMessage answer has no \"message\" object"))?;
        if message.get("ts").is_none() {
            message["ts"] = value.get("ts").cloned().unwrap_or(Value::Null);
        }
        let raw = Raw::parse(message)?;
        if let Some(user) = raw.value.get("user").and_then(Value::as_str) {
            self.learn_self(user);
        }
        self.messages(channel, std::slice::from_ref(&raw))
            .await
            .into_iter()
            .next()
            .ok_or_else(|| shape("chat.postMessage answered with a message that has no author"))
    }

    /// [`crate::chat::ChatClient::post_in_thread`], without the provider label.
    ///
    /// The destination is the thread; `reply_to` adds nothing Slack can carry and is ignored.
    pub(super) async fn thread_post(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        _reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        let target = Self::channel_ref(channel)?;
        let thread = self.member_thread(&target, thread_id)?;
        self.thread_root(&target.conversation, &thread).await?;
        self.post_raw(channel, &target.conversation, content, Some(&thread))
            .await
    }

    /// [`crate::chat::ChatClient::fetch_thread_message`], without the provider label.
    pub(super) async fn thread_message(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        message_id: &MessageId,
    ) -> Result<Message, ChatError> {
        let target = Self::channel_ref(channel)?;
        let thread = self.member_thread(&target, thread_id)?;
        let micros = cursor_micros(message_id)?;
        let page = self
            .page(
                Feed::Replies(&target.conversation, &thread),
                Some(micros),
                Some(micros),
                true,
                5,
                None,
            )
            .await?;
        let raw = page
            .raws
            .into_iter()
            .find(|raw| raw.micros == micros)
            .ok_or_else(|| ChatError::Status {
                status: 404,
                body: "conversations.replies has no such message in that thread".to_owned(),
            })?;
        if raw.ts != thread && raw.thread_ts().as_deref() != Some(thread.as_str()) {
            return Err(refused(
                "that message does not belong to the selected thread",
            ));
        }
        self.messages(channel, std::slice::from_ref(&raw))
            .await
            .into_iter()
            .next()
            .ok_or_else(|| shape("the message has no author"))
    }

    /// A caller's thread id, checked against a thread-scoped channel's own thread.
    fn member_thread(&self, target: &ChannelRef, thread_id: &str) -> Result<String, ChatError> {
        let thread = thread_ts(thread_id)?;
        if target
            .thread_ts
            .as_deref()
            .is_some_and(|scope| scope != thread)
        {
            return Err(refused(
                "that thread does not belong to the configured channel",
            ));
        }
        Ok(thread)
    }

    /// [`crate::chat::ChatClient::register_channel`], without the provider label.
    pub(super) async fn register(
        &self,
        source: &str,
        _label: &str,
    ) -> Result<RegisteredChannel, ChatError> {
        if !self.channel_registration {
            return Err(refused(
                "channel registration is disabled; ask the deployment operator to enable it for this backend",
            ));
        }
        let (conversation, thread) = ids::parse_source(source).ok_or_else(|| {
            refused(
                "not a Slack conversation: paste a conversation id such as C0123ABCD, a link copied \
                 from Slack (https://<workspace>.slack.com/archives/…), or an app.slack.com/client link",
            )
        })?;
        let info = self
            .call("conversations.info", &[("channel", conversation.clone())])
            .await?;
        if info
            .get("channel")
            .and_then(|channel| channel.get("id"))
            .and_then(Value::as_str)
            != Some(conversation.as_str())
        {
            return Err(shape(
                "conversations.info answered for a different conversation",
            ));
        }
        if let Some(thread) = &thread {
            self.thread_root(&conversation, thread).await?;
        }
        Ok(RegisteredChannel {
            id: ChannelId(
                ChannelRef {
                    conversation,
                    thread_ts: thread,
                }
                .channel_id(),
            ),
            // Nothing upstream was created, so vibe-talk never issues a compensating removal.
            created: false,
            writable: self.registered_writable,
        })
    }

    /// [`crate::chat::ChatClient::discover_channels`], without the provider label.
    ///
    /// Filtering by name can leave a Slack page with fewer matches than were asked for, so up to
    /// [`DISCOVERY_PAGES`] pages are read to fill one answer. The answer never exceeds the
    /// requested limit: when it fills part-way through a Slack page, the continuation records how
    /// far into that page it got and the next request resumes there. A page can still be short
    /// when the page budget runs out first; its continuation resumes correctly.
    pub(super) async fn discover(
        &self,
        request: &DirectoryRequest,
    ) -> Result<DirectoryPage, ChatError> {
        if !self.channel_discovery {
            return Err(refused(
                "channel discovery is disabled for this backend; ask the deployment operator to enable it",
            ));
        }
        let mut position = match request.cursor.as_deref() {
            Some(raw) => URL_SAFE_NO_PAD
                .decode(raw)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<DirectoryCursor>(&bytes).ok())
                .ok_or_else(|| refused("invalid channel-directory cursor; start the list again"))?,
            None => DirectoryCursor {
                c: String::new(),
                o: 0,
            },
        };
        let query = request
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let limit = usize::from(request.limit.max(1));
        let mut entries = Vec::new();
        for _ in 0..DISCOVERY_PAGES {
            let mut params = vec![
                ("types", DISCOVERY_TYPES.to_owned()),
                ("limit", DISCOVERY_PAGE_LIMIT.to_string()),
                ("exclude_archived", "true".to_owned()),
            ];
            if !position.c.is_empty() {
                params.push(("cursor", position.c.clone()));
            }
            let value = self.call("users.conversations", &params).await?;
            let channels = value
                .get("channels")
                .and_then(Value::as_array)
                .ok_or_else(|| shape("users.conversations answer has no \"channels\" array"))?;
            let next = value
                .get("response_metadata")
                .and_then(|metadata| metadata.get("next_cursor"))
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            for (index, channel) in channels.iter().enumerate().skip(position.o) {
                let Some(entry) = self.directory_entry(channel).await else {
                    continue;
                };
                if query
                    .as_deref()
                    .is_some_and(|query| !entry.name.to_lowercase().contains(query))
                {
                    continue;
                }
                entries.push(entry);
                if entries.len() == limit {
                    let rest_of_page = index + 1 < channels.len();
                    let resume = if rest_of_page {
                        Some(DirectoryCursor {
                            c: position.c.clone(),
                            o: index + 1,
                        })
                    } else {
                        next.map(|c| DirectoryCursor { c, o: 0 })
                    };
                    return Ok(DirectoryPage {
                        entries,
                        next_cursor: resume.map(|cursor| encode_directory(&cursor)).transpose()?,
                        truncated: false,
                    });
                }
            }
            match next {
                Some(c) if c != position.c => position = DirectoryCursor { c, o: 0 },
                Some(_) => return Err(shape("a paging cursor did not advance")),
                None => {
                    return Ok(DirectoryPage {
                        entries,
                        next_cursor: None,
                        truncated: false,
                    })
                }
            }
        }
        Ok(DirectoryPage {
            entries,
            next_cursor: Some(encode_directory(&position)?),
            truncated: false,
        })
    }

    /// One `users.conversations` entry as a directory entry; `None` when it has no usable id.
    async fn directory_entry(&self, channel: &Value) -> Option<DirectoryEntry> {
        let id = channel
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| ids::is_conversation_id(id))?;
        let flag = |key: &str| channel.get(key).and_then(Value::as_bool).unwrap_or(false);
        let name = channel
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty());
        let display = if flag("is_im") {
            let person = match channel.get("user").and_then(Value::as_str) {
                Some(user) => self
                    .user(user)
                    .await
                    .map_or_else(|| user.to_owned(), |(name, _)| name),
                None => id.to_owned(),
            };
            format!("DM: {person}")
        } else if flag("is_mpim") {
            group_name(name.unwrap_or(id))
        } else {
            format!("#{}", name.unwrap_or(id))
        };
        Some(DirectoryEntry {
            source: id.to_owned(),
            name: directory_name(&display),
            registered_channel_id: None,
        })
    }
}

fn encode_directory(cursor: &DirectoryCursor) -> Result<String, ChatError> {
    serde_json::to_vec(cursor)
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        .map_err(|_| shape("could not encode the channel-directory continuation"))
}
