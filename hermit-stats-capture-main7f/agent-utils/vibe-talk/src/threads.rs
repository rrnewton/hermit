//! Provider-neutral channel timelines, thread summaries, and pagination.
//!
//! Thread identifiers and cursors are opaque to callers. The backend owns discovery, ordering,
//! and verification that a thread belongs to the configured parent channel.

use serde::{Deserialize, Serialize};

use crate::model::{Message, MessageId};

/// Which HTTP contract provides thread access.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadApi {
    /// The native provider's channel and thread endpoints.
    #[default]
    Native,
    /// The normalized timeline and thread endpoints of a compatible bridge.
    Bridge,
    /// Child-thread discovery and thread-specific posting are disabled.
    ///
    /// A provider-managed source that identifies one upstream conversation may still be exposed
    /// as an ordinary app channel. This setting controls the projection within that channel, not
    /// which sources appear in the channel picker.
    Off,
}

/// A view of one configured channel.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TimelineView {
    /// Messages posted directly to the channel, including thread roots.
    #[default]
    Main,
    /// Thread summaries, ordered by their latest message activity.
    Threads,
    /// Channel messages and all accessible thread messages in one chronological history.
    Flat,
    /// The history of one explicitly selected thread.
    Thread,
}

impl TimelineView {
    /// The wire spelling used in query strings.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Threads => "threads",
            Self::Flat => "flat",
            Self::Thread => "thread",
        }
    }
}

/// One page request. Every returned page is ordered oldest first.
///
/// Three kinds of read share it: the NEWEST page (neither cursor), a step BACK (`before`), and a
/// step FORWARD from an earlier newest read (`after`), which answers only what changed since then.
/// `#203 incremental-refresh`.
#[derive(Clone, Debug)]
pub struct TimelineRequest {
    /// Which channel view to read.
    pub view: TimelineView,
    /// Required only for the thread view; opaque to callers.
    pub thread_id: Option<String>,
    /// The previous page's opaque backward continuation, or `None` for the newest page.
    pub before: Option<String>,
    /// The backend's OWN forward cursor, from an earlier [`TimelinePage::next_after`] for this
    /// channel, view and thread, or `None`.
    ///
    /// Never vibe-talk's envelope: [`crate::timeline_forward`] removes that before a backend sees
    /// the request, and only ever sets this for a backend that issued the cursor. Mutually
    /// exclusive with `before`; a backend refuses both with [`crate::chat::ChatError::Refused`].
    pub after: Option<String>,
    /// Maximum entries requested. The backend may clamp this to its page ceiling.
    pub limit: u16,
}

impl Default for TimelineRequest {
    fn default() -> Self {
        Self {
            view: TimelineView::Main,
            thread_id: None,
            before: None,
            after: None,
            limit: 50,
        }
    }
}

impl TimelineRequest {
    /// Refuse a request that names both cursors: a step back and a step forward at once means
    /// nothing, and guessing which was meant would answer a question nobody asked.
    ///
    /// # Errors
    ///
    /// [`crate::chat::ChatError::Refused`] when both `before` and `after` are present.
    pub fn one_direction(&self) -> Result<(), crate::chat::ChatError> {
        if self.before.is_some() && self.after.is_some() {
            return Err(crate::chat::ChatError::Refused(
                "before and after cannot both be given".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Thread membership attached to a message without conflating it with a reply reference.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MessageThread {
    /// Opaque thread identifier, scoped to the configured channel.
    pub id: String,
    /// The original channel message, if this provider exposes one.
    pub root_message_id: Option<MessageId>,
    /// Whether this message is the original thread root.
    pub is_root: bool,
    /// Number of replies, when the provider can supply it.
    pub reply_count: Option<u64>,
    /// Whether the reply count is exact rather than a provider estimate.
    pub reply_count_exact: bool,
}

/// One thread in the thread list, independent of the underlying provider's channel model.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadSummary {
    /// Opaque identifier used for reading or posting to this thread.
    pub id: String,
    /// The original message, when available and readable.
    pub root: Option<Message>,
    /// Provider-supplied title, or a neutral fallback.
    pub title: String,
    /// Number of replies when known.
    pub reply_count: Option<u64>,
    /// Whether the reported count is exact.
    pub reply_count_exact: bool,
    /// Latest message activity, as an RFC 3339 timestamp.
    pub updated_at: String,
    /// A short hyphenated name for the thread, chosen once by a summariser and then kept even if
    /// the thread drifts, so the reader always finds it under the same name. Absent until named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// One sentence on what the thread is about now, re-summarised as it grows. Absent until
    /// summarised. See [`thread_summary_due`] for when it is refreshed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Whether a thread with `messages` messages, last summarised at `last` messages, is due again.
///
/// Backed off exponentially, then linearly: summarise at 1, 2, 4 and 8 messages, then every 10
/// after that (18, 28, ...). Early on a thread's subject is still settling and each message
/// changes it a lot; later one more message rarely does, and each summary costs a model call.
/// `last` is `None` for a thread never summarised. The display name is chosen at the first
/// summary and never changes; only the sentence follows this schedule.
#[must_use]
pub fn thread_summary_due(messages: u64, last: Option<u64>) -> bool {
    const LINEAR_AFTER: u64 = 8;
    const LINEAR_STEP: u64 = 10;
    if messages == 0 {
        return false;
    }
    let Some(last) = last else {
        return true;
    };
    if messages <= last {
        return false;
    }
    let next = if last < LINEAR_AFTER {
        // 1 -> 2 -> 4 -> 8: the next power of two above the last summarised count.
        (last + 1).next_power_of_two().min(LINEAR_AFTER)
    } else {
        last + LINEAR_STEP
    };
    messages >= next
}

/// One complete page from a channel view, never a silently truncated discovery result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TimelinePage {
    /// Messages for main, flat, or thread views, ordered oldest first.
    pub messages: Vec<Message>,
    /// Summaries for the threads view, ordered by last activity oldest first.
    pub threads: Vec<ThreadSummary>,
    /// The selected thread, for the thread view. Outside it, the one thread a channel registered as
    /// a single conversation is scoped to: that channel has no child threads (`has_threads` is
    /// false), and its main view carries the conversation's replies as well as its root.
    pub thread: Option<ThreadSummary>,
    /// Whether accessible threads exist in this channel.
    pub has_threads: bool,
    /// Whether another backward page exists.
    pub has_more: bool,
    /// Opaque continuation, present exactly when another page exists.
    pub next_before: Option<String>,
    /// Relevant provider limitation or scope information, if any.
    pub notice: Option<String>,
    /// Forward cursor for this view: what to pass as `after` to learn what changed since this
    /// page. `#203 incremental-refresh`.
    ///
    /// From a backend, present only when that backend answers `after` itself. Above the backend
    /// vibe-talk always fills it on a newest page or a delta — with its own generic cursor when
    /// the backend left it empty — and removes it from a `before` page, which says nothing about
    /// the newest end. Absent from an older bridge's answer, which therefore still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
    /// Present exactly on an answer to `after`: the page then carries only what is new or changed
    /// since that cursor, and rows absent from it are UNCHANGED, never deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<TimelineDelta>,
    /// The instant, RFC 3339, up to which the backend knows this page is complete, when it is
    /// serving data it could not bring up to date just now. Absent means now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
}

impl TimelinePage {
    /// A newest Flat page as the window a read of a channel's most recent messages answers:
    /// oldest first by id, each message once, at most `limit` of them, the newest kept, with the
    /// page's `as_of`. `#208 persistent-cache`, milestone `bridge-recent-flat`.
    ///
    /// A bridge already pages that way; this holds the shape whatever it sends, so a caller cannot
    /// tell from the shape which endpoint answered. Thread membership is kept: it is true of the
    /// message, and a reader that files messages by thread uses it. Never `exhausted`: a page that
    /// says it has no more is not a proof of a channel's start that any caller relies on yet.
    #[must_use]
    pub fn into_recent(self, limit: u16) -> crate::chat::RecentRead {
        let mut messages = self.messages;
        crate::model::sort_oldest_first(&mut messages);
        messages.dedup_by(|later, earlier| later.id == earlier.id);
        let excess = messages.len().saturating_sub(usize::from(limit));
        messages.drain(..excess);
        crate::chat::RecentRead {
            messages,
            as_of: self.as_of,
            exhausted: false,
        }
    }
}

/// What a forward read says about the changes it carries. `#203 incremental-refresh`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TimelineDelta {
    /// More changes remain past this page: ask again with `next_after` straight away.
    pub more: bool,
    /// True when this delta carries EVERY creation, edit, deletion and summary change in the view
    /// since the cursor. False when it carries additions only: a read filtered by creation time
    /// cannot see an edit or a deletion, which then reach the caller through live events or its
    /// next full read.
    pub complete: bool,
    /// Messages removed since the cursor, channel-wide in every view. Always empty when
    /// `complete` is false.
    #[serde(default)]
    pub deleted: Vec<MessageId>,
    /// Threads that left the thread list since the cursor. Threads view only; empty elsewhere.
    #[serde(default)]
    pub removed_threads: Vec<String>,
}

#[cfg(test)]
mod summary_schedule_tests {
    use super::thread_summary_due;

    /// The counts at which a thread growing one message at a time is summarised.
    fn schedule(up_to: u64) -> Vec<u64> {
        let mut last = None;
        let mut at = Vec::new();
        for messages in 0..=up_to {
            if thread_summary_due(messages, last) {
                at.push(messages);
                last = Some(messages);
            }
        }
        at
    }

    #[test]
    fn a_growing_thread_is_summarised_at_1_2_4_8_then_every_10() {
        assert_eq!(schedule(40), [1, 2, 4, 8, 18, 28, 38]);
    }

    #[test]
    fn a_thread_that_jumped_ahead_is_summarised_once_not_once_per_missed_step() {
        assert!(thread_summary_due(30, Some(2)));
        assert!(!thread_summary_due(30, Some(30)));
        assert!(!thread_summary_due(3, Some(2)), "the step after 2 is 4");
        assert!(
            !thread_summary_due(0, None),
            "an empty thread has nothing to summarise"
        );
    }
}
