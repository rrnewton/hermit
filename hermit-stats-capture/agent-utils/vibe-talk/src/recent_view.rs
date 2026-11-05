//! A channel's most recent messages as one read, and the shadow that checks a bridge's Flat view
//! against `/messages` before Flat is trusted to answer it. `#208 persistent-cache`, milestone
//! `bridge-recent-flat`.
//!
//! # Why
//!
//! A compatible bridge answers `/messages` by running a provider command — about two seconds,
//! through one command slot — and answers the newest page of its Flat view from a copy of the
//! channel it keeps current, in milliseconds. Both list the channel's messages, replies included,
//! newest last, under the same ids. `recent_view = "flat"` reads the second instead of the first
//! (see [`crate::discord::http`]); this module is what makes that switch something to decide on
//! evidence rather than on faith.
//!
//! # The shadow
//!
//! With `recent_view_shadow = true` (and `recent_view = "messages"`), each recent read through
//! [`read`] is checked against a read of the newest Flat page of the same size, made later in a
//! detached task so the caller never waits for it. The two answers are compared over the stretch
//! both cover — from the newer of their oldest messages (or the channel's start, for a read that
//! came back short) up to the newest message `/messages` returned — and two things are counted per
//! channel:
//!
//! * **id mismatches**: messages inside that stretch that only one of the two listed;
//! * **content-hash mismatches**: messages both listed whose text differs, compared by the same
//!   [`crate::summarize::content_hash`] the summary cache keys on.
//!
//! A Flat message newer than everything `/messages` returned is not a mismatch: it arrived after
//! that read. An empty `/messages` read bounds no stretch, so nothing is compared beside one.
//! Counts only, in memory, in the diagnostics report, with two that explain a mismatch rather than
//! hide it: `stale`, a Flat page the bridge labelled with `as_of`, and `raced`, a comparison across
//! which the live feed reported an edit or a delete in the channel. One `info` line per comparison
//! that found a difference carries the counts, and a `debug` line the message ids — never text.
//!
//! # What a check costs the reads it checks
//!
//! A Flat read is not free for the bridge, and it need not wait its turn there. The bridge this
//! was built against runs every provider command through one slot and serves a timeline read as
//! one a reader is waiting for, ahead of `/messages`. When its copy of the channel is a few minutes
//! old it first brings it forward — one provider command, about two seconds — and when it holds no
//! copy at all, as for the first read of a channel after a quiet spell or a restart of the bridge,
//! it scans the channel, for up to 50 seconds. Either way it stops the `/messages` read holding the
//! slot, runs it again afterwards, and goes ahead of every one waiting. A check started the moment
//! its read returned would therefore hold up the next read of a catch-up, which reads channel
//! after channel, or an agent's `digest_channel`: exactly the reads the shadow is meant to measure
//! and leave alone.
//!
//! So checks wait, in one queue across every channel, and run ONE AT A TIME, each only once no
//! recent read has been running for [`QUIET`]. A channel has at most one read waiting: a newer
//! read of it takes the older one's place, and a read that has waited longer than [`MAX_WAIT`] is
//! dropped, since a Flat read that much later would count every edit and delete in between. Both
//! count as `skipped`. That makes a collision unlikely, not impossible: a read that starts while a
//! check is making the bridge bring its copy forward still waits for it. And the bridge keeps a
//! copy it was asked for recently current in the background, through the same slot at the
//! priority of `/messages` itself: nothing is stopped for that, but a read can queue behind it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::chat::{ChatError, RecentRead};
use crate::config::{ProviderKind, RecentView};
use crate::model::{ChannelId, Message, MessageId};
use crate::recent::ReadMark;
use crate::state::AppState;
use crate::threads::{TimelinePage, TimelineRequest, TimelineView};

/// How long no recent read may have run before a check starts. Long enough to fall between the
/// back-to-back reads of a catch-up rather than inside them, short enough that a quiet minute
/// checks every channel read in it.
pub const QUIET: Duration = Duration::from_secs(3);

/// How long a read may wait for its check before it is dropped, unchecked. A Flat read made much
/// later than the `/messages` read it checks counts every edit and delete in between.
pub const MAX_WAIT: Duration = Duration::from_secs(120);

/// Read `channel`'s most recent `limit` messages through its provider, and queue the read for a
/// check when that provider asks for one.
///
/// # Errors
///
/// As [`crate::chat::ChatClient::fetch_recent_dated`]. The shadow never fails a read.
pub async fn read(
    state: &AppState,
    channel: &ChannelId,
    limit: u16,
) -> Result<RecentRead, ChatError> {
    // Taken before the read, like every mark: a change pushed while it runs may be in either
    // answer, so it explains a difference between them. See `crate::recent::ReadMark`. The purge
    // count is taken then too, so a read the storage purge overtakes is not kept for a check.
    let mark = shadowed(state, channel).then(|| {
        (
            state.live.recent().mark(channel),
            state.recent_view_shadow.purges(),
        )
    });
    let reading = state.recent_view_shadow.reading();
    let read = state.chat.fetch_recent_dated(channel, limit).await;
    drop(reading);
    let read = read?;
    if let Some((mark, purges)) = mark {
        let waiting = Waiting {
            channel: channel.clone(),
            limit,
            messages: read.messages.clone(),
            mark,
            purges,
            queued: Instant::now(),
        };
        if state.recent_view_shadow.enqueue(waiting) {
            tokio::spawn(check(state.clone()));
        }
    }
    Ok(read)
}

/// Whether recent reads of `channel` are `/messages` reads its provider wants checked against Flat.
#[must_use]
pub fn shadowed(state: &AppState, channel: &ChannelId) -> bool {
    state
        .providers
        .key_for(channel)
        .and_then(|key| state.config.provider(&key))
        .is_some_and(|provider| match &provider.kind {
            ProviderKind::Discord(discord) => {
                discord.recent.shadow && discord.recent.view == RecentView::Messages
            }
            ProviderKind::Slack(_) => false,
        })
}

/// The one task that checks waiting reads, one at a time, each after a quiet spell. It ends when
/// nothing is waiting; the next read queued starts another.
async fn check(state: AppState) {
    let shadow = Arc::clone(&state.recent_view_shadow);
    let mut checking = Checking {
        shadow: &shadow,
        ended: false,
    };
    loop {
        shadow.quiet().await;
        let Some(waiting) = shadow.next() else {
            // `next` let the queue go, under its lock, so a read queued from now on starts a task.
            checking.ended = true;
            return;
        };
        shadow.count(&waiting.channel, |tally| tally.reads += 1);
        let request = TimelineRequest {
            view: TimelineView::Flat,
            limit: waiting.limit,
            ..TimelineRequest::default()
        };
        let flat = state.chat.fetch_timeline(&waiting.channel, &request).await;
        let raced = state
            .live
            .recent()
            .changed_since(&waiting.channel, waiting.mark);
        shadow.finish(
            &waiting.channel,
            &waiting.messages,
            waiting.limit,
            flat,
            raced,
        );
    }
}

/// What one channel's shadow has counted since this process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    /// Flat reads started to check a `/messages` read.
    pub reads: u64,
    /// `/messages` reads never checked: a newer read of the channel took their place in the queue,
    /// they waited longer than [`MAX_WAIT`] for their turn, or the storage purge dropped them
    /// ([`Shadow::clear`]).
    pub skipped: u64,
    /// Flat reads that failed, so nothing was compared.
    pub failed: u64,
    /// Flat reads that answered and were compared.
    pub compared: u64,
    /// Of those, how many carried `as_of`: the bridge served a copy it could not bring up to date,
    /// and a mismatch beside one is explained by that.
    pub stale: u64,
    /// Of those, how many the live feed reported an edit or a delete in the channel for between
    /// the start of the `/messages` read and the end of the Flat read: a mismatch beside one may be
    /// that change rather than a disagreement. Always zero where nothing feeds this server live.
    pub raced: u64,
    /// Comparisons that found at least one mismatch of either kind.
    pub mismatched: u64,
    /// Messages, summed over every comparison, inside the stretch both reads cover that only one
    /// of them listed.
    pub id_mismatches: u64,
    /// Messages, summed over every comparison, both reads listed with different content hashes.
    pub hash_mismatches: u64,
}

/// One channel's [`Tally`], as the diagnostics report carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChannelTally {
    /// The channel that was read.
    pub channel_id: ChannelId,
    /// What its shadow counted.
    #[serde(flatten)]
    pub tally: Tally,
}

/// A `/messages` read waiting for its check.
#[derive(Clone, Debug)]
struct Waiting {
    channel: ChannelId,
    limit: u16,
    messages: Vec<Message>,
    /// Taken before the read: see [`Tally::raced`].
    mark: ReadMark,
    /// [`Shadow::purges`] when the read began: a read the storage purge overtook is not queued.
    purges: u64,
    queued: Instant,
}

/// The reads waiting for a check, and what decides when the next may start.
#[derive(Debug, Default)]
struct Queue {
    /// Oldest first, at most one per channel.
    waiting: VecDeque<Waiting>,
    /// Recent reads running now.
    reading: usize,
    /// When the last of them ended.
    last_read: Option<Instant>,
    /// Whether the checking task is running.
    checking: bool,
    /// How many times [`Shadow::clear`] has run.
    purges: u64,
}

/// Every channel's shadow counts, and the reads waiting to be checked.
#[derive(Debug)]
pub struct Shadow {
    tallies: Mutex<BTreeMap<ChannelId, Tally>>,
    queue: Mutex<Queue>,
    /// Woken when the last running recent read ends.
    idle: Notify,
    quiet: Duration,
}

impl Default for Shadow {
    fn default() -> Self {
        Self::new()
    }
}

impl Shadow {
    /// Nothing counted yet, checks waiting [`QUIET`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_quiet(QUIET)
    }

    /// Nothing counted yet, checks waiting `quiet` instead. For tests.
    #[must_use]
    pub fn with_quiet(quiet: Duration) -> Self {
        Self {
            tallies: Mutex::new(BTreeMap::new()),
            queue: Mutex::new(Queue::default()),
            idle: Notify::new(),
            quiet,
        }
    }

    /// What `channel`'s shadow has counted, zero when it has counted nothing.
    #[must_use]
    pub fn tally(&self, channel: &ChannelId) -> Tally {
        lock(&self.tallies)
            .get(channel)
            .copied()
            .unwrap_or_default()
    }

    fn count(&self, channel: &ChannelId, change: impl FnOnce(&mut Tally)) {
        change(lock(&self.tallies).entry(channel.clone()).or_default());
    }

    /// A recent read is running until the guard drops. Every read through [`read`] holds one,
    /// shadowed or not: a check waits for all of them.
    fn reading(&self) -> Reading<'_> {
        lock(&self.queue).reading += 1;
        Reading { shadow: self }
    }

    /// How many times [`Shadow::clear`] has run. Taken when a read begins, and handed back with it
    /// to [`Shadow::enqueue`].
    fn purges(&self) -> u64 {
        lock(&self.queue).purges
    }

    /// Drop every read waiting for its check, and refuse every read that began before now. For the
    /// storage purge, `DELETE /api/v1/storage`: a waiting read holds the text of the messages it
    /// read, and the purge empties this copy as it empties the catch-up's windows
    /// ([`crate::recent::RecentWindows::clear`]). Each read dropped or refused counts as skipped.
    ///
    /// A check already running keeps its read until its Flat read answers, then keeps only the
    /// counts. The counts, numbers per channel id and no text, are not cleared.
    pub fn clear(&self) {
        let dropped: Vec<ChannelId> = {
            let mut queue = lock(&self.queue);
            queue.purges += 1;
            queue
                .waiting
                .drain(..)
                .map(|waiting| waiting.channel)
                .collect()
        };
        for channel in dropped {
            self.count(&channel, |tally| tally.skipped += 1);
        }
    }

    /// Put a read in the queue, in the place of one of the same channel already waiting. Whether
    /// the caller must start the checking task.
    ///
    /// A read that began before the newest [`Shadow::clear`] is refused, as skipped, under the
    /// same lock the purge takes, so it cannot slip in after it.
    fn enqueue(&self, waiting: Waiting) -> bool {
        let channel = waiting.channel.clone();
        let (replaced, start) = {
            let mut queue = lock(&self.queue);
            if queue.purges != waiting.purges {
                drop(queue);
                self.count(&channel, |tally| tally.skipped += 1);
                return false;
            }
            let replaced = match queue.waiting.iter_mut().find(|w| w.channel == channel) {
                Some(older) => {
                    *older = waiting;
                    true
                }
                None => {
                    queue.waiting.push_back(waiting);
                    false
                }
            };
            (replaced, !std::mem::replace(&mut queue.checking, true))
        };
        if replaced {
            self.count(&channel, |tally| tally.skipped += 1);
        }
        start
    }

    /// Return once no recent read is running and none has for the quiet spell.
    async fn quiet(&self) {
        loop {
            let wait = {
                let queue = lock(&self.queue);
                if queue.reading > 0 {
                    None
                } else {
                    let since = queue.last_read.map_or(self.quiet, |at| at.elapsed());
                    if since >= self.quiet {
                        return;
                    }
                    Some(self.quiet - since)
                }
            };
            match wait {
                // `notify_one` keeps its wake-up for a waiter that has not started waiting yet, so
                // a read that ends between the check above and this line is not missed.
                None => self.idle.notified().await,
                Some(wait) => tokio::time::sleep(wait).await,
            }
        }
    }

    /// The next read to check, dropping as skipped any that waited too long; `None`, and the
    /// queue let go, when nothing is waiting.
    fn next(&self) -> Option<Waiting> {
        let mut dropped = Vec::new();
        let next = {
            let mut queue = lock(&self.queue);
            loop {
                match queue.waiting.pop_front() {
                    Some(waiting) if waiting.queued.elapsed() > MAX_WAIT => {
                        dropped.push(waiting.channel);
                    }
                    Some(waiting) => break Some(waiting),
                    None => {
                        queue.checking = false;
                        break None;
                    }
                }
            }
        };
        for channel in dropped {
            self.count(&channel, |tally| tally.skipped += 1);
        }
        next
    }

    /// Fold one finished Flat read into `channel`'s counts.
    fn finish(
        &self,
        channel: &ChannelId,
        messages: &[Message],
        limit: u16,
        flat: Result<TimelinePage, ChatError>,
        raced: bool,
    ) {
        let page = match flat {
            Ok(page) => page.into_recent(limit),
            Err(error) => {
                tracing::warn!(channel = %channel, %error, "recent view shadow: the Flat read failed");
                self.count(channel, |tally| tally.failed += 1);
                return;
            }
        };
        let difference = compare(messages, &page.messages, limit);
        let stale = page.as_of.is_some();
        self.count(channel, |tally| {
            tally.compared += 1;
            tally.stale += u64::from(stale);
            tally.raced += u64::from(raced);
            if !difference.is_empty() {
                tally.mismatched += 1;
                tally.id_mismatches += count(difference.ids.len());
                tally.hash_mismatches += count(difference.contents.len());
            }
        });
        if !difference.is_empty() {
            tracing::info!(
                channel = %channel,
                id_mismatches = difference.ids.len(),
                hash_mismatches = difference.contents.len(),
                stale,
                raced,
                "recent view shadow: the Flat view disagreed with /messages"
            );
            tracing::debug!(
                channel = %channel,
                ids = ?difference.ids,
                contents = ?difference.contents,
                "recent view shadow: the messages that differed"
            );
        }
    }
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn lock<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A recent read running. Dropping it — the read finished, or its caller gave up — starts the
/// quiet spell when it was the last.
struct Reading<'a> {
    shadow: &'a Shadow,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        let mut queue = lock(&self.shadow.queue);
        queue.reading = queue.reading.saturating_sub(1);
        queue.last_read = Some(Instant::now());
        if queue.reading == 0 {
            self.shadow.idle.notify_one();
        }
    }
}

/// The checking task, running. Torn down before it let the queue go — a panic, or the runtime
/// ending — it lets it go here, so a later read can start another.
struct Checking<'a> {
    shadow: &'a Shadow,
    ended: bool,
}

impl Drop for Checking<'_> {
    fn drop(&mut self) {
        if !self.ended {
            lock(&self.shadow.queue).checking = false;
        }
    }
}

/// Where a `/messages` read and a Flat read of the same channel disagree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Difference {
    /// Messages inside the stretch both reads cover that only one of them listed, oldest first.
    pub ids: Vec<MessageId>,
    /// Messages both listed whose content hashes differ, oldest first.
    pub contents: Vec<MessageId>,
}

impl Difference {
    /// Whether the two reads agreed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.contents.is_empty()
    }
}

/// Compare a `/messages` read with a Flat read of the same `limit`, both oldest first.
///
/// The stretch compared starts at the newer of the two reads' oldest messages — or at the
/// channel's start for a read that came back short, which claims the whole channel — and ends at
/// the newest message `messages` holds: a Flat message after that arrived after the `/messages`
/// read and is not a mismatch. An empty `messages` holds no newest message, so nothing is
/// compared: every Flat message may have arrived after it. An id that is not a number is always
/// inside the stretch.
#[must_use]
pub fn compare(messages: &[Message], flat: &[Message], limit: u16) -> Difference {
    if messages.is_empty() {
        return Difference::default();
    }
    let floor = |read: &[Message]| {
        if read.len() < usize::from(limit) {
            0
        } else {
            read.iter()
                .filter_map(|m| m.id.numeric())
                .min()
                .unwrap_or(0)
        }
    };
    let lowest = floor(messages).max(floor(flat));
    let highest = messages
        .iter()
        .filter_map(|m| m.id.numeric())
        .max()
        .unwrap_or(u64::MAX);
    let inside = |m: &&Message| {
        m.id.numeric()
            .is_none_or(|n| (lowest..=highest).contains(&n))
    };
    let ours: HashMap<&MessageId, &Message> =
        messages.iter().filter(inside).map(|m| (&m.id, m)).collect();
    let theirs: HashMap<&MessageId, &Message> =
        flat.iter().filter(inside).map(|m| (&m.id, m)).collect();
    let mut difference = Difference::default();
    let mut seen: Vec<&Message> = ours.values().chain(theirs.values()).copied().collect();
    seen.sort_by_key(|m| (m.id.numeric().is_none(), m.id.numeric().unwrap_or(0)));
    seen.dedup_by(|later, earlier| later.id == earlier.id);
    for message in seen {
        match (ours.get(&message.id), theirs.get(&message.id)) {
            (Some(a), Some(b)) => {
                if crate::summarize::content_hash(&a.content)
                    != crate::summarize::content_hash(&b.content)
                {
                    difference.contents.push(message.id.clone());
                }
            }
            _ => difference.ids.push(message.id.clone()),
        }
    }
    difference
}

/// Every channel whose recent reads are shadowed, with its counts, for the diagnostics report.
/// Listed even before its first read, so the report shows the shadow is on.
#[must_use]
pub fn report(state: &AppState) -> Vec<ChannelTally> {
    state
        .all_channels()
        .into_iter()
        .filter(|channel| shadowed(state, &channel.id))
        .map(|channel| ChannelTally {
            tally: state.recent_view_shadow.tally(&channel.id),
            channel_id: channel.id,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UserId;

    fn message(n: u64, text: &str) -> Message {
        Message {
            thread: None,
            id: MessageId(n.to_string()),
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

    fn ids(ids: &[u64]) -> Vec<MessageId> {
        ids.iter().map(|n| MessageId(n.to_string())).collect()
    }

    #[test]
    fn two_reads_of_the_same_messages_agree() {
        let read: Vec<Message> = (10..15).map(|n| message(n, "same")).collect();
        assert!(compare(&read, &read, 5).is_empty());
        assert!(compare(&[], &[], 5).is_empty());
    }

    #[test]
    fn a_message_only_one_read_listed_is_an_id_mismatch_and_changed_text_a_hash_mismatch() {
        let messages = [message(10, "a"), message(11, "b"), message(12, "c")];
        let flat = [message(10, "a"), message(12, "c, edited"), message(13, "d")];
        let difference = compare(&messages, &flat, 50);
        // 13 is newer than everything /messages returned: it arrived after that read.
        assert_eq!(difference.ids, ids(&[11]));
        assert_eq!(difference.contents, ids(&[12]));
    }

    #[test]
    fn only_the_stretch_both_full_reads_cover_is_compared() {
        // Both full, so each says nothing below its oldest message: Flat reaching one deeper is
        // not a mismatch. The newest message, which only /messages has, is.
        let messages: Vec<Message> = (11..=13).map(|n| message(n, "m")).collect();
        let flat: Vec<Message> = (10..=12).map(|n| message(n, "m")).collect();
        assert_eq!(compare(&messages, &flat, 3).ids, ids(&[13]));
        // A short read claims the whole channel, so what it lacks below the other's oldest is a
        // mismatch after all.
        let flat = [message(12, "m"), message(13, "m")];
        assert_eq!(compare(&messages, &flat, 4).ids, ids(&[11]));
    }

    #[test]
    fn a_flat_read_the_bridge_had_not_brought_up_to_date_shows_what_it_lacks() {
        let messages = [message(10, "a"), message(11, "b"), message(12, "newest")];
        let flat = [message(10, "a"), message(11, "b")];
        assert_eq!(compare(&messages, &flat, 50).ids, ids(&[12]));
    }

    #[test]
    fn an_empty_messages_read_compares_nothing() {
        // Nothing says how new the read was, so every Flat message may have arrived after it.
        let flat = [message(10, "a"), message(11, "b")];
        assert!(compare(&[], &flat, 50).is_empty());
        assert!(compare(&[], &flat, 2).is_empty());
    }

    #[test]
    fn the_counts_follow_each_comparison_and_failure() {
        let shadow = Shadow::new();
        let channel = ChannelId("1".to_owned());
        let messages = [message(10, "a"), message(11, "b")];
        let page = |messages: Vec<Message>, as_of: Option<&str>| TimelinePage {
            messages,
            as_of: as_of.map(str::to_owned),
            ..TimelinePage::default()
        };
        shadow.finish(
            &channel,
            &messages,
            50,
            Ok(page(messages.to_vec(), None)),
            false,
        );
        shadow.finish(
            &channel,
            &messages,
            50,
            Ok(page(
                vec![message(11, "b, edited")],
                Some("2026-10-12T10:00:00Z"),
            )),
            true,
        );
        shadow.finish(
            &channel,
            &messages,
            50,
            Err(ChatError::Transport("down".to_owned())),
            true,
        );
        assert_eq!(
            shadow.tally(&channel),
            Tally {
                reads: 0,
                skipped: 0,
                failed: 1,
                compared: 2,
                stale: 1,
                raced: 1,
                mismatched: 1,
                id_mismatches: 1,
                hash_mismatches: 1,
            }
        );
    }

    fn waiting(channel: &str, text: &str) -> Waiting {
        let channel = ChannelId(channel.to_owned());
        Waiting {
            mark: crate::recent::RecentWindows::new().mark(&channel),
            channel,
            limit: 50,
            messages: vec![message(10, text)],
            purges: 0,
            queued: Instant::now(),
        }
    }

    /// `#210 voice-catchup-speed`'s storage purge empties the reads waiting for a check, which hold
    /// the text they read, and refuses a read that began before it and lands after; each counts
    /// as skipped. A read that begins after the purge is queued as before.
    #[tokio::test(start_paused = true)]
    async fn the_purge_drops_every_waiting_read_and_refuses_one_that_began_before_it() {
        let shadow = Shadow::new();
        let before = shadow.purges();
        assert!(shadow.enqueue(waiting("1", "first")));
        assert!(!shadow.enqueue(waiting("2", "other")));

        shadow.clear();
        assert!(shadow.next().is_none(), "a read outlived the purge");
        assert_eq!(shadow.tally(&ChannelId("1".to_owned())).skipped, 1);
        assert_eq!(shadow.tally(&ChannelId("2".to_owned())).skipped, 1);

        let overtaken = Waiting {
            purges: before,
            ..waiting("3", "read before the purge")
        };
        assert!(!shadow.enqueue(overtaken));
        assert!(
            shadow.next().is_none(),
            "a read from before the purge was kept"
        );
        assert_eq!(shadow.tally(&ChannelId("3".to_owned())).skipped, 1);

        let after = Waiting {
            purges: shadow.purges(),
            ..waiting("3", "read after the purge")
        };
        assert!(shadow.enqueue(after));
        let next = shadow.next().expect("queued");
        assert_eq!(next.messages[0].content, "read after the purge");
        assert_eq!(shadow.tally(&ChannelId("3".to_owned())).skipped, 1);
    }

    /// One queue for every channel, one read waiting per channel: a newer read of a channel takes
    /// the older one's place, keeps its turn, and the older one counts as skipped. Only the first
    /// read queued while no check is running starts one.
    #[tokio::test(start_paused = true)]
    async fn one_read_waits_per_channel_and_a_newer_one_takes_its_place() {
        let shadow = Shadow::new();
        assert!(shadow.enqueue(waiting("1", "first")));
        assert!(!shadow.enqueue(waiting("2", "other")));
        assert!(!shadow.enqueue(waiting("1", "newer")));
        assert_eq!(shadow.tally(&ChannelId("1".to_owned())).skipped, 1);

        let next = shadow.next().expect("waiting");
        assert_eq!(next.channel, ChannelId("1".to_owned()));
        assert_eq!(next.messages[0].content, "newer");
        assert_eq!(
            shadow.next().expect("waiting").channel,
            ChannelId("2".to_owned())
        );
        // Empty: the queue is let go, so the next read queued starts a check again.
        assert!(shadow.next().is_none());
        assert!(shadow.enqueue(waiting("2", "again")));
    }

    /// A read left waiting too long is dropped as skipped: a Flat read that late would count every
    /// change in between.
    #[tokio::test(start_paused = true)]
    async fn a_read_that_waited_too_long_is_skipped() {
        let shadow = Shadow::new();
        assert!(shadow.enqueue(waiting("1", "old")));
        tokio::time::advance(MAX_WAIT / 2).await;
        assert!(!shadow.enqueue(waiting("2", "younger")));
        tokio::time::advance(MAX_WAIT / 2 + Duration::from_secs(1)).await;
        let next = shadow.next().expect("the younger read is still in time");
        assert_eq!(next.channel, ChannelId("2".to_owned()));
        assert_eq!(shadow.tally(&ChannelId("1".to_owned())).skipped, 1);
        assert_eq!(shadow.tally(&ChannelId("2".to_owned())).skipped, 0);
    }

    /// A check waits while any recent read runs, and then for the quiet spell after the last.
    #[tokio::test(start_paused = true)]
    async fn a_check_waits_for_recent_reads_to_stop_and_then_for_quiet() {
        let shadow = Arc::new(Shadow::new());
        // Nothing has been read: no reason to wait.
        tokio::time::timeout(Duration::from_millis(1), shadow.quiet())
            .await
            .expect("quiet at once");

        let first = shadow.reading();
        let second = shadow.reading();
        let waiter = {
            let shadow = Arc::clone(&shadow);
            tokio::spawn(async move { shadow.quiet().await })
        };
        tokio::time::sleep(QUIET * 4).await;
        assert!(!waiter.is_finished(), "a read is running");
        drop(first);
        tokio::time::sleep(QUIET * 4).await;
        assert!(!waiter.is_finished(), "a read is still running");
        drop(second);
        tokio::time::sleep(QUIET - Duration::from_millis(100)).await;
        assert!(!waiter.is_finished(), "inside the quiet spell");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(waiter.is_finished(), "the quiet spell is over");
    }
}
