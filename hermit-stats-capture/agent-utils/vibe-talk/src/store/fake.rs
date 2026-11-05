//! An in-memory [`StateStore`] for tests.
//!
//! # It must be able to say no
//!
//! A fake that accepts every write and answers every read with a success is a yes-man: a test
//! written against it is self-certifying, green here and worth nothing about the shipped
//! [`super::sqlite::SqliteStore`]. So this one reproduces the refusals rather than the happy path
//! alone —
//!
//! * a conversation that was never written answers [`StoreError::NotFound`], and so does
//!   forgetting one twice;
//! * the same [`Retention`] bounds apply, including the per-conversation turn ceiling, the
//!   [`super::MAX_TURN_CHARS`] limit on one turn, and the count and age bounds on the cached
//!   summaries;
//! * a read mark refuses an id it cannot order, and never moves backwards, and so does a
//!   dismissal, whole batch and all;
//! * [`FakeStore::fail_next`] makes the very next operation fail, so a caller's error path can be
//!   exercised instead of assumed.
//!
//! What it deliberately cannot show is durability: it is destroyed with the process. The test
//! that a transcript survives a restart lives in [`super::sqlite`] against a real file, and it
//! has to.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{
    now_ms, Bookmark, BookmarkAnchor, BookmarkChange, CachedSummary, ChannelAlias, ConversationId,
    ConversationSummary, Pin, PinChange, PinSnapshot, Pins, ReadMark, RecordedTurn, Retention,
    StateStore, StoreError, SummaryKey, TranscriptCursor, TranscriptPage, Turn,
    UpstreamClearAttempt, UpstreamClearMode, UpstreamClearSetting, MAX_PINS_PER_CHANNEL,
    MAX_TRANSCRIPT_PAGE, MAX_TURN_CHARS,
};
use crate::model::{ChannelId, MessageId};
use crate::voice_agent::OwnerPrompt;

/// One cached summary as the fake holds it.
///
/// `made_at_ms` and `seq` are not decoration: the real store bounds this table by age and by
/// count, and a fake that kept only the text could not reproduce either — which is exactly how a
/// fake stops being able to say no.
#[derive(Clone, Debug)]
struct FakeSummary {
    text: String,
    cut_at: Option<u32>,
    made_at_ms: i64,
    /// Insertion order, standing in for SQLite's `rowid`: it breaks a tie between two entries
    /// made in the same millisecond, and it survives an overwrite, as a rowid does.
    seq: u64,
}

#[derive(Debug, Default)]
struct State {
    conversations: BTreeMap<String, Vec<Turn>>,
    marks: BTreeMap<String, (MessageId, u64, i64)>,
    summaries: BTreeMap<(String, String, String, String), FakeSummary>,
    /// `(channel, message)` to `(snowflake, when it was dismissed)`. The same shape the real
    /// table has, so the count bound and the ordering can be reproduced rather than assumed.
    dismissals: BTreeMap<(String, String), (u64, i64)>,
    /// Channel to `(alias, when it was set)`. `#39 channel-alias`.
    aliases: BTreeMap<String, (String, i64)>,
    /// Channels added from inside the app: id -> (label, writable, provider, added_at_ms).
    added: BTreeMap<String, (String, bool, Option<String>, i64)>,
    /// Configured channels taken off the list: id -> when. `#199 removable-config-channels`.
    hidden: BTreeMap<String, i64>,
    /// The owner's saved noise rules; `None` until he saves a list. `#196 auto-read-noise`.
    noise_rules: Option<Vec<String>>,
    /// The owner's "ask before the voice agent sends"; `None` until he chooses.
    /// `#264 voice-send-approval`.
    ask_before_send: Option<bool>,
    /// The owner's words for the voice agent; `None` until he saves any.
    /// `#272 voice-agent-instructions`.
    owner_prompt: Option<OwnerPrompt>,
    /// `(channel, message)` to `(when it was exempted, insertion order)`: the "not noise" list,
    /// with enough of the real table's shape to reproduce its count bound and its eviction order.
    noise_exemptions: BTreeMap<(String, String), (i64, u64)>,
    next_exemption_seq: u64,
    /// `(channel, message)` to `(pin, insertion order)`. `#206 pin-message`. The order stands in
    /// for SQLite's `rowid`, breaking a tie between two pins made in one millisecond, so the bound
    /// drops the same pin the real store would.
    pins: BTreeMap<(String, String), (Pin, u64)>,
    next_pin_seq: u64,
    /// Channel to its pin revision; absent is `0`, as a missing row is in the real store.
    pin_revisions: BTreeMap<String, i64>,
    /// Channel to its upstream-clear setting. `#232 upstream-mark-read`.
    upstream_clear: BTreeMap<String, UpstreamClearSetting>,
    /// Channel to its bookmark, cleared ones included, as the real table keeps a row for a cleared
    /// bookmark so its revision goes on growing. `#240 reading-bookmark`.
    bookmarks: BTreeMap<String, Bookmark>,
    next_summary_seq: u64,
    fail_next: Option<String>,
    appended: usize,
    purges: usize,
}

/// In-memory durable state, for tests.
#[derive(Debug)]
pub struct FakeStore {
    state: Mutex<State>,
    retention: Retention,
}

impl Default for FakeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeStore {
    /// An empty store with the default retention bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::with_retention(Retention::default())
    }

    /// An empty store with retention bounds a test chose.
    #[must_use]
    pub fn with_retention(retention: Retention) -> Self {
        Self {
            state: Mutex::new(State::default()),
            retention,
        }
    }

    /// Make the next operation — any operation — fail with a backend error saying `why`.
    ///
    /// One-shot, so a test can prove the *recovery* as well as the failure.
    pub fn fail_next(&self, why: &str) {
        self.lock().fail_next = Some(why.to_owned());
    }

    /// How many turns have been appended, refusals excluded.
    #[must_use]
    pub fn appended(&self) -> usize {
        self.lock().appended
    }

    /// How many times the store has been purged.
    #[must_use]
    pub fn purges(&self) -> usize {
        self.lock().purges
    }

    /// How many cached summaries are held right now.
    ///
    /// A count rather than the entries themselves: what tests need to know is whether a caller
    /// caused a durable write, and "how many rows are there" is the question that answers it
    /// without a test having to reconstruct the four-part key.
    #[must_use]
    pub fn cached_summaries(&self) -> usize {
        self.lock().summaries.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The same four-part key the real store uses, so the fake cannot be more forgiving than it is.
fn fake_key(key: &SummaryKey) -> (String, String, String, String) {
    (
        key.version.clone(),
        key.channel.as_str().to_owned(),
        key.message.as_str().to_owned(),
        format!("{:016x}", key.content_hash),
    )
}

/// A channel's pin revision, `0` where nothing was ever pinned.
fn fake_revision(state: &State, channel: &ChannelId) -> i64 {
    state
        .pin_revisions
        .get(channel.as_str())
        .copied()
        .unwrap_or(0)
}

/// Move a channel's pin revision on by the shared rule, and return it.
fn fake_bump(state: &mut State, channel: &ChannelId) -> i64 {
    let next = super::next_pin_revision(fake_revision(state, channel), now_ms());
    state
        .pin_revisions
        .insert(channel.as_str().to_owned(), next);
    next
}

fn armed(state: &mut State) -> Result<(), StoreError> {
    match state.fail_next.take() {
        Some(why) => Err(StoreError::Backend(why)),
        None => Ok(()),
    }
}

#[async_trait]
impl StateStore for FakeStore {
    fn describe(&self) -> String {
        "an in-memory store that is destroyed with the process".to_owned()
    }

    async fn check_writable(&self) -> Result<String, StoreError> {
        // Through `armed`, like every other operation here, so `FakeStore::fail_next` can make
        // this check go red. A writability probe that could not be made to fail would certify
        // nothing about the branch that reports a broken store.
        let mut state = self.lock();
        armed(&mut state)?;
        Ok("the in-memory store accepts writes (and forgets them at exit)".to_owned())
    }

    async fn append_turn(
        &self,
        conversation: &ConversationId,
        turn: &Turn,
    ) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        if turn.text.chars().count() > MAX_TURN_CHARS {
            return Err(StoreError::TooLarge(format!(
                "one turn may be at most {MAX_TURN_CHARS} characters"
            )));
        }
        let held = state
            .conversations
            .get(conversation.as_str())
            .map_or(0, Vec::len);
        if held >= usize::from(self.retention.max_turns_per_conversation) {
            return Err(StoreError::TooLarge(format!(
                "this conversation already holds its limit of {} turns",
                self.retention.max_turns_per_conversation
            )));
        }
        state
            .conversations
            .entry(conversation.as_str().to_owned())
            .or_default()
            .push(turn.clone());
        state.appended += 1;

        if self.retention.retain_days > 0 {
            let cutoff = now_ms() - i64::from(self.retention.retain_days) * 86_400_000;
            state
                .conversations
                .retain(|_, turns| turns.last().is_some_and(|t| t.at_ms >= cutoff));
        }
        while state.conversations.len() > usize::from(self.retention.max_conversations) {
            let oldest = state
                .conversations
                .iter()
                .min_by_key(|(id, turns)| (turns.last().map_or(0, |t| t.at_ms), (*id).clone()))
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => {
                    state.conversations.remove(&id);
                }
                None => break,
            }
        }
        Ok(())
    }

    async fn conversations(&self) -> Result<Vec<ConversationSummary>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let mut out: Vec<ConversationSummary> = state
            .conversations
            .iter()
            .map(|(id, turns)| ConversationSummary {
                id: ConversationId::parse(id).unwrap_or_else(|_| unreachable!("stored id")),
                started_at_ms: turns.first().map_or(0, |t| t.at_ms),
                last_at_ms: turns.last().map_or(0, |t| t.at_ms),
                turns: u32::try_from(turns.len()).unwrap_or(u32::MAX),
                preview: turns
                    .first()
                    .map_or_else(String::new, |t| super::preview_of(&t.text)),
            })
            .collect();
        out.sort_by(|a, b| {
            b.last_at_ms
                .cmp(&a.last_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(out)
    }

    async fn turns(&self, conversation: &ConversationId) -> Result<Vec<Turn>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state
            .conversations
            .get(conversation.as_str())
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    async fn transcript(
        &self,
        limit: u16,
        before: Option<&TranscriptCursor>,
    ) -> Result<TranscriptPage, StoreError> {
        let limit = usize::from(limit.clamp(1, MAX_TRANSCRIPT_PAGE));
        let mut state = self.lock();
        armed(&mut state)?;
        // Flattened and sorted here rather than kept sorted, because the fake's job is to answer
        // the same way the shipped store does and not to be fast. Built from the SAME
        // `conversations` map every other method reads, so a fake transcript cannot show
        // something a fake `turns` would deny.
        let mut all: Vec<RecordedTurn> = state
            .conversations
            .iter()
            .flat_map(|(id, turns)| {
                turns
                    .iter()
                    .enumerate()
                    .map(move |(index, turn)| RecordedTurn {
                        conversation: ConversationId::parse(id)
                            .unwrap_or_else(|_| unreachable!("stored id")),
                        // From 1, exactly as SQLite's `MAX(seq) + 1` assigns it.
                        seq: i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1),
                        turn: turn.clone(),
                    })
            })
            .collect();
        let key = |entry: &RecordedTurn| {
            (
                entry.turn.at_ms,
                entry.conversation.as_str().to_owned(),
                entry.seq,
            )
        };
        all.sort_by_key(|entry| std::cmp::Reverse(key(entry)));
        if let Some(cursor) = before {
            let edge = (
                cursor.at_ms,
                cursor.conversation.as_str().to_owned(),
                cursor.seq,
            );
            all.retain(|entry| key(entry) < edge);
        }
        let has_more = all.len() > limit;
        all.truncate(limit);
        let next = all.last().map(|last| TranscriptCursor {
            at_ms: last.turn.at_ms,
            conversation: last.conversation.clone(),
            seq: last.seq,
        });
        Ok(TranscriptPage {
            turns: all,
            has_more,
            next,
        })
    }

    async fn forget_conversation(&self, conversation: &ConversationId) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state
            .conversations
            .remove(conversation.as_str())
            .map(|_| ())
            .ok_or(StoreError::NotFound)
    }

    async fn forget_all_conversations(&self) -> Result<u64, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let removed = state.conversations.len() as u64;
        state.conversations.clear();
        Ok(removed)
    }

    async fn read_mark(&self, channel: &ChannelId) -> Result<Option<ReadMark>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state
            .marks
            .get(channel.as_str())
            .map(|(last_read, _, at)| ReadMark {
                channel: channel.clone(),
                last_read: last_read.clone(),
                marked_at_ms: *at,
            }))
    }

    async fn read_marks(&self) -> Result<Vec<ReadMark>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state
            .marks
            .iter()
            .map(|(channel, (last_read, _, at))| ReadMark {
                channel: ChannelId(channel.clone()),
                last_read: last_read.clone(),
                marked_at_ms: *at,
            })
            .collect())
    }

    async fn mark_read(
        &self,
        channel: &ChannelId,
        upto: &MessageId,
    ) -> Result<ReadMark, StoreError> {
        let numeric = upto.numeric().ok_or_else(|| {
            StoreError::BadId(format!(
                "{upto:?} is not a message snowflake, so this server cannot order it"
            ))
        })?;
        let mut state = self.lock();
        armed(&mut state)?;
        let at = now_ms();
        let entry = state
            .marks
            .entry(channel.as_str().to_owned())
            .or_insert_with(|| (upto.clone(), numeric, at));
        if numeric > entry.1 {
            *entry = (upto.clone(), numeric, at);
        }
        Ok(ReadMark {
            channel: channel.clone(),
            last_read: entry.0.clone(),
            marked_at_ms: entry.2,
        })
    }

    async fn forget_read_mark(&self, channel: &ChannelId) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state
            .marks
            .remove(channel.as_str())
            .map(|_| ())
            .ok_or(StoreError::NotFound)
    }

    async fn added_channels(&self) -> Result<Vec<crate::store::AddedChannel>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state
            .added
            .iter()
            .map(
                |(channel, (label, writable, registration_provider, added_at_ms))| {
                    crate::store::AddedChannel {
                        channel: ChannelId(channel.clone()),
                        label: label.clone(),
                        writable: *writable,
                        registration_provider: registration_provider.clone(),
                        added_at_ms: *added_at_ms,
                    }
                },
            )
            .collect())
    }

    async fn add_channel(
        &self,
        channel: &ChannelId,
        label: &str,
        writable: bool,
        registration_provider: Option<&str>,
        at_ms: i64,
    ) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state
            .added
            .entry(channel.0.clone())
            .and_modify(|entry| {
                entry.0 = label.to_owned();
                entry.1 = writable;
                entry.2 = registration_provider.map(str::to_owned);
            })
            .or_insert_with(|| {
                (
                    label.to_owned(),
                    writable,
                    registration_provider.map(str::to_owned),
                    at_ms,
                )
            });
        Ok(())
    }

    async fn remove_added_channel(&self, channel: &ChannelId) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state.added.remove(channel.as_str());
        Ok(())
    }

    async fn hidden_channels(&self) -> Result<Vec<ChannelId>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state.hidden.keys().cloned().map(ChannelId).collect())
    }

    async fn hide_channel(&self, channel: &ChannelId, at_ms: i64) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state.hidden.entry(channel.0.clone()).or_insert(at_ms);
        Ok(())
    }

    async fn unhide_channel(&self, channel: &ChannelId) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state.hidden.remove(channel.as_str());
        Ok(())
    }

    async fn channel_aliases(&self) -> Result<Vec<ChannelAlias>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state
            .aliases
            .iter()
            .map(|(channel, (alias, set_at_ms))| ChannelAlias {
                channel: ChannelId(channel.clone()),
                alias: alias.clone(),
                set_at_ms: *set_at_ms,
            })
            .collect())
    }

    async fn set_channel_alias(
        &self,
        channel: &ChannelId,
        alias: &str,
    ) -> Result<ChannelAlias, StoreError> {
        // Validated BEFORE `armed`, exactly as `mark_read` refuses an unorderable snowflake
        // before it touches the state: a fake that accepted a name the real store rejects is a
        // fake that certifies a bug.
        let alias = super::validate_alias(alias)?;
        let mut state = self.lock();
        armed(&mut state)?;
        let set_at_ms = now_ms();
        state
            .aliases
            .insert(channel.as_str().to_owned(), (alias.clone(), set_at_ms));
        Ok(ChannelAlias {
            channel: channel.clone(),
            alias,
            set_at_ms,
        })
    }

    async fn clear_channel_alias(&self, channel: &ChannelId) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state
            .aliases
            .remove(channel.as_str())
            .map(|_| ())
            .ok_or(StoreError::NotFound)
    }

    async fn dismissals(&self, channel: &ChannelId) -> Result<Vec<MessageId>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let mut held: Vec<(u64, MessageId)> = state
            .dismissals
            .iter()
            .filter(|((held, _), _)| held == channel.as_str())
            .map(|((_, message), (numeric, _))| (*numeric, MessageId(message.clone())))
            .collect();
        held.sort_by_key(|(numeric, _)| std::cmp::Reverse(*numeric));
        Ok(held.into_iter().map(|(_, message)| message).collect())
    }

    async fn dismiss(
        &self,
        channel: &ChannelId,
        messages: &[MessageId],
    ) -> Result<u64, StoreError> {
        // Refused BEFORE the lock and before anything is written, exactly as the real store does:
        // a batch with one unorderable id changes nothing at all.
        let ordered = messages
            .iter()
            .map(|message| {
                message
                    .numeric()
                    .map(|n| (message.clone(), n))
                    .ok_or_else(|| {
                        StoreError::BadId(format!(
                        "{message:?} is not a message snowflake, so this server cannot order it"
                    ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut state = self.lock();
        armed(&mut state)?;
        let at = now_ms();
        let mut added = 0;
        for (message, numeric) in ordered {
            let slot = (channel.as_str().to_owned(), message.as_str().to_owned());
            if state.dismissals.contains_key(&slot) {
                // Already dealt with. Not an error, and NOT a refresh of its position: the same
                // rule the real store's `DO NOTHING` enforces.
                continue;
            }
            state.dismissals.insert(slot, (numeric, at));
            added += 1;
        }
        while state.dismissals.len() > self.retention.max_dismissals as usize {
            let oldest = state
                .dismissals
                .iter()
                .min_by_key(|(slot, (_, at))| (*at, (*slot).clone()))
                .map(|(slot, _)| slot.clone());
            match oldest {
                Some(slot) => {
                    state.dismissals.remove(&slot);
                }
                None => break,
            }
        }
        Ok(added)
    }

    async fn restore(
        &self,
        channel: &ChannelId,
        messages: &[MessageId],
    ) -> Result<u64, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let mut removed = 0;
        for message in messages {
            let slot = (channel.as_str().to_owned(), message.as_str().to_owned());
            if state.dismissals.remove(&slot).is_some() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    async fn noise_rules(&self) -> Result<Option<Vec<String>>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state.noise_rules.clone())
    }

    async fn set_noise_rules(&self, rules: &[String]) -> Result<Vec<String>, StoreError> {
        // Validated BEFORE `armed`, as `set_channel_alias` is: a fake that kept a list the real
        // store refuses would certify a bug.
        let rules = crate::noise::validate_rules(rules)?;
        let mut state = self.lock();
        armed(&mut state)?;
        state.noise_rules = Some(rules.clone());
        Ok(rules)
    }

    async fn ask_before_send(&self) -> Result<Option<bool>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state.ask_before_send)
    }

    async fn set_ask_before_send(&self, ask: bool) -> Result<bool, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state.ask_before_send = Some(ask);
        Ok(ask)
    }

    async fn owner_prompt(&self) -> Result<Option<OwnerPrompt>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state.owner_prompt.clone())
    }

    async fn set_owner_prompt(&self, prompt: &OwnerPrompt) -> Result<OwnerPrompt, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state.owner_prompt = Some(prompt.clone());
        Ok(prompt.clone())
    }

    async fn noise_exemptions(&self, channel: &ChannelId) -> Result<Vec<MessageId>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state
            .noise_exemptions
            .keys()
            .filter(|(held, _)| held == channel.as_str())
            .map(|(_, message)| MessageId(message.clone()))
            .collect())
    }

    async fn exempt_from_noise(
        &self,
        channel: &ChannelId,
        messages: &[MessageId],
    ) -> Result<u64, StoreError> {
        crate::noise::validate_exempt_ids(messages)?;
        let mut state = self.lock();
        armed(&mut state)?;
        let at = now_ms();
        let mut added = 0;
        for message in messages {
            let slot = (channel.as_str().to_owned(), message.as_str().to_owned());
            if state.noise_exemptions.contains_key(&slot) {
                continue;
            }
            let seq = state.next_exemption_seq;
            state.next_exemption_seq += 1;
            state.noise_exemptions.insert(slot, (at, seq));
            added += 1;
        }
        // The same bound the real store applies, oldest first.
        while state.noise_exemptions.len() > self.retention.max_dismissals as usize {
            let oldest = state
                .noise_exemptions
                .iter()
                .min_by_key(|(_, order)| **order)
                .map(|(slot, _)| slot.clone());
            match oldest {
                Some(slot) => {
                    state.noise_exemptions.remove(&slot);
                }
                None => break,
            }
        }
        Ok(added)
    }

    async fn pins(&self, channel: &ChannelId) -> Result<Pins, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let mut held: Vec<Pin> = state
            .pins
            .iter()
            .filter(|((held, _), _)| held == channel.as_str())
            .map(|(_, (pin, _))| pin.clone())
            .collect();
        // The real store's order: by when the message was sent, a timestamp that will not parse
        // standing in with when it was pinned, then by id.
        held.sort_by_key(|pin| {
            (
                crate::clock::instant_ms(&pin.snapshot.timestamp).unwrap_or(pin.pinned_at_ms),
                pin.snapshot.message_id.clone(),
            )
        });
        Ok(Pins {
            pins: held,
            revision: fake_revision(&state, channel),
        })
    }

    async fn pins_revision(&self, channel: &ChannelId) -> Result<i64, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(fake_revision(&state, channel))
    }

    async fn pin(
        &self,
        channel: &ChannelId,
        snapshot: &PinSnapshot,
    ) -> Result<PinChange, StoreError> {
        // Validated BEFORE `armed`, as `set_noise_rules` is: a fake that kept a snapshot the real
        // store refuses would certify a bug.
        let snapshot = super::validate_pin(snapshot)?;
        let mut state = self.lock();
        armed(&mut state)?;
        let slot = (
            channel.as_str().to_owned(),
            snapshot.message_id.as_str().to_owned(),
        );
        let changed = match state.pins.get(&slot) {
            // A repeat keeps its place in the queue and only refreshes what changed.
            Some((held, _)) if held.snapshot == snapshot => false,
            Some((held, seq)) => {
                let (pinned_at_ms, seq) = (held.pinned_at_ms, *seq);
                state.pins.insert(
                    slot.clone(),
                    (
                        Pin {
                            snapshot,
                            pinned_at_ms,
                            content_html: String::new(),
                            heading: None,
                        },
                        seq,
                    ),
                );
                true
            }
            None => {
                let seq = state.next_pin_seq;
                state.next_pin_seq += 1;
                state.pins.insert(
                    slot.clone(),
                    (
                        Pin {
                            snapshot,
                            pinned_at_ms: now_ms(),
                            content_html: String::new(),
                            heading: None,
                        },
                        seq,
                    ),
                );
                true
            }
        };
        // The same bound, oldest pinned first, never the message just pinned.
        let mut others: Vec<((String, String), (i64, u64))> = state
            .pins
            .iter()
            .filter(|((held, message), _)| held == channel.as_str() && *message != slot.1)
            .map(|(key, (pin, seq))| (key.clone(), (pin.pinned_at_ms, *seq)))
            .collect();
        others.sort_by_key(|(_, order)| std::cmp::Reverse(*order));
        let mut unpinned: Vec<MessageId> = Vec::new();
        for (key, _) in others
            .into_iter()
            .skip(MAX_PINS_PER_CHANNEL.saturating_sub(1))
        {
            state.pins.remove(&key);
            unpinned.push(MessageId(key.1));
        }
        unpinned.reverse();
        let revision = if changed || !unpinned.is_empty() {
            fake_bump(&mut state, channel)
        } else {
            fake_revision(&state, channel)
        };
        Ok(PinChange {
            changed,
            pin: state.pins.get(&slot).map(|(pin, _)| pin.clone()),
            unpinned,
            revision,
        })
    }

    async fn unpin(
        &self,
        channel: &ChannelId,
        message: &MessageId,
    ) -> Result<PinChange, StoreError> {
        super::validate_pin_id(message)?;
        let mut state = self.lock();
        armed(&mut state)?;
        let slot = (channel.as_str().to_owned(), message.as_str().to_owned());
        let changed = state.pins.remove(&slot).is_some();
        let revision = if changed {
            fake_bump(&mut state, channel)
        } else {
            fake_revision(&state, channel)
        };
        Ok(PinChange {
            changed,
            pin: None,
            unpinned: Vec::new(),
            revision,
        })
    }

    async fn bookmark(&self, channel: &ChannelId) -> Result<Bookmark, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state
            .bookmarks
            .get(channel.as_str())
            .cloned()
            .unwrap_or_else(Bookmark::unset))
    }

    async fn set_bookmark(
        &self,
        channel: &ChannelId,
        anchor: Option<&BookmarkAnchor>,
        if_revision: Option<i64>,
    ) -> Result<BookmarkChange, StoreError> {
        // Validated BEFORE `armed`, as a pin is: a fake that kept an anchor the real store refuses
        // would certify a bug.
        if let Some(anchor) = anchor {
            super::validate_bookmark(anchor)?;
        }
        let mut state = self.lock();
        armed(&mut state)?;
        let held = state
            .bookmarks
            .get(channel.as_str())
            .cloned()
            .unwrap_or_else(Bookmark::unset);
        let change = super::decide_bookmark(&held, anchor, if_revision, now_ms());
        if change.changed {
            state
                .bookmarks
                .insert(channel.as_str().to_owned(), change.bookmark.clone());
        }
        Ok(change)
    }

    async fn cached_summary(&self, key: &SummaryKey) -> Result<Option<CachedSummary>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state
            .summaries
            .get(&fake_key(key))
            .map(|entry| CachedSummary {
                text: entry.text.clone(),
                cut_at: entry.cut_at,
                made_at_ms: entry.made_at_ms,
            }))
    }

    async fn cache_summary(
        &self,
        key: &SummaryKey,
        summary: &str,
        cut_at: Option<u32>,
    ) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let slot = fake_key(key);
        let seq = match state.summaries.get(&slot) {
            // An overwrite keeps its place in the queue, as it keeps its rowid in SQLite.
            Some(existing) => existing.seq,
            None => {
                let next = state.next_summary_seq;
                state.next_summary_seq += 1;
                next
            }
        };
        state.summaries.insert(
            slot,
            FakeSummary {
                text: summary.to_owned(),
                cut_at,
                made_at_ms: now_ms(),
                seq,
            },
        );

        // The same two bounds the SQLite store applies, in the same order. See
        // `super::sqlite::prune_summaries` for why the age one is the only thing that ever
        // collects an orphan.
        if self.retention.summary_retain_days > 0 {
            let cutoff = now_ms() - i64::from(self.retention.summary_retain_days) * 86_400_000;
            state.summaries.retain(|_, held| held.made_at_ms >= cutoff);
        }
        // Counted per kind, as the real store counts them: ordinary summaries, and the entries kept
        // across policies (`super::KEPT_VERSION`).
        for kept in [false, true] {
            let of_kind = |(version, _, _, _): &(String, String, String, String)| {
                (version == super::KEPT_VERSION) == kept
            };
            while state.summaries.keys().filter(|slot| of_kind(slot)).count()
                > self.retention.max_summaries as usize
            {
                let oldest = state
                    .summaries
                    .iter()
                    .filter(|(slot, _)| of_kind(slot))
                    .min_by_key(|(_, held)| (held.made_at_ms, held.seq))
                    .map(|(slot, _)| slot.clone());
                match oldest {
                    Some(slot) => {
                        state.summaries.remove(&slot);
                    }
                    None => break,
                }
            }
        }
        Ok(())
    }

    async fn touch_summary(&self, key: &SummaryKey) -> Result<bool, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let now = now_ms();
        match state.summaries.get_mut(&fake_key(key)) {
            Some(held) if held.made_at_ms < now - super::SUMMARY_TOUCH_AFTER_MS => {
                held.made_at_ms = now;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn rekey_summaries(
        &self,
        from: &str,
        to: &str,
        cap_then: u32,
    ) -> Result<u64, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        if from == to {
            return Ok(0);
        }
        let old: Vec<_> = state
            .summaries
            .iter()
            .filter(|((version, _, _, _), _)| version == from)
            .map(|(slot, held)| (slot.clone(), held.clone()))
            .collect();
        let mut moved = 0;
        for ((_, channel, message, hash), held) in old {
            state.summaries.remove(&(
                from.to_owned(),
                channel.clone(),
                message.clone(),
                hash.clone(),
            ));
            let slot = (to.to_owned(), channel, message.clone(), hash);
            if state.summaries.contains_key(&slot) {
                continue;
            }
            // The SQLite store's rule: a thread sentence's JSON `summary`, else the text itself,
            // ending "…" means it was cut at the cap the old version folded in.
            let text = if message.starts_with(crate::summarize::threads::SENTENCE_PREFIX) {
                serde_json::from_str::<serde_json::Value>(&held.text)
                    .ok()
                    .and_then(|record| record.get("summary")?.as_str().map(str::to_owned))
                    .unwrap_or_else(|| held.text.clone())
            } else {
                held.text.clone()
            };
            let cut_at = text.ends_with('…').then_some(cap_then);
            state.summaries.insert(slot, FakeSummary { cut_at, ..held });
            moved += 1;
        }
        Ok(moved)
    }

    async fn forget_channel_summaries(&self, channel: &ChannelId) -> Result<u64, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let before = state.summaries.len();
        state.summaries.retain(|(version, held_channel, _, _), _| {
            held_channel != channel.as_str() || version == super::KEPT_VERSION
        });
        Ok((before - state.summaries.len()) as u64)
    }

    async fn forget_summaries_except(&self, version: &str) -> Result<u64, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let before = state.summaries.len();
        state
            .summaries
            .retain(|(held, _, _, _), _| held == version || held == super::KEPT_VERSION);
        Ok((before - state.summaries.len()) as u64)
    }

    async fn upstream_clear_settings(&self) -> Result<Vec<UpstreamClearSetting>, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        Ok(state.upstream_clear.values().cloned().collect())
    }

    async fn set_upstream_clear_mode(
        &self,
        channel: &ChannelId,
        mode: UpstreamClearMode,
        at_ms: i64,
    ) -> Result<UpstreamClearSetting, StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        let setting = state
            .upstream_clear
            .entry(channel.as_str().to_owned())
            .or_insert_with(|| UpstreamClearSetting::unset(channel.clone()));
        setting.change_mode(mode, at_ms);
        Ok(setting.clone())
    }

    async fn record_upstream_clear(
        &self,
        channel: &ChannelId,
        attempt: &UpstreamClearAttempt,
    ) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        if let Some(setting) = state.upstream_clear.get_mut(channel.as_str()) {
            setting.record(attempt);
        }
        Ok(())
    }

    async fn purge_everything(&self) -> Result<(), StoreError> {
        let mut state = self.lock();
        armed(&mut state)?;
        state.upstream_clear.clear();
        state.conversations.clear();
        state.marks.clear();
        state.summaries.clear();
        state.dismissals.clear();
        state.aliases.clear();
        state.noise_rules = None;
        state.ask_before_send = None;
        state.owner_prompt = None;
        state.noise_exemptions.clear();
        state.pins.clear();
        state.pin_revisions.clear();
        state.bookmarks.clear();
        state.purges += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Speaker;
    use super::*;

    fn id(text: &str) -> ConversationId {
        ConversationId::parse(text).expect("valid id")
    }

    #[tokio::test]
    async fn the_fake_refuses_a_conversation_it_was_never_given() {
        let store = FakeStore::new();
        assert!(matches!(
            store.turns(&id("nope")).await,
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.forget_conversation(&id("nope")).await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn the_fake_can_be_made_to_fail_exactly_once() {
        let store = FakeStore::new();
        store.fail_next("the disk is full");
        let error = store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "hello"))
            .await
            .expect_err("the armed failure must fire");
        assert_eq!(error.code(), "storage_error", "{error}");
        assert_eq!(store.appended(), 0, "a failed append must write nothing");
        store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "hello"))
            .await
            .expect("the failure is one-shot, so recovery is testable");
        assert_eq!(store.appended(), 1);
    }

    #[tokio::test]
    async fn the_fake_enforces_the_same_retention_bounds_as_the_real_store() {
        let store = FakeStore::with_retention(Retention {
            max_conversations: 2,
            max_turns_per_conversation: 2,
            retain_days: 0,
            ..Retention::default()
        });
        for (n, at) in [("a", 1_000), ("b", 2_000), ("c", 3_000)] {
            store
                .append_turn(
                    &id(n),
                    &Turn {
                        speaker: Speaker::You,
                        text: n.to_owned(),
                        at_ms: at,
                    },
                )
                .await
                .expect("append");
        }
        let kept: Vec<String> = store
            .conversations()
            .await
            .expect("list")
            .into_iter()
            .map(|c| c.id.as_str().to_owned())
            .collect();
        assert_eq!(kept, vec!["c".to_owned(), "b".to_owned()]);

        store
            .append_turn(&id("c"), &Turn::now(Speaker::Agent, "second"))
            .await
            .expect("append");
        let error = store
            .append_turn(&id("c"), &Turn::now(Speaker::Agent, "third"))
            .await
            .expect_err("the turn ceiling must apply to the fake too");
        assert_eq!(error.code(), "too_large", "{error}");
    }

    #[tokio::test]
    async fn the_fake_read_mark_is_monotonic_and_refuses_an_unorderable_id() {
        let store = FakeStore::new();
        let channel = ChannelId("1111111111".to_owned());
        store
            .mark_read(&channel, &MessageId("1000000000000000200".to_owned()))
            .await
            .expect("mark");
        let back = store
            .mark_read(&channel, &MessageId("1000000000000000100".to_owned()))
            .await
            .expect("mark");
        assert_eq!(back.last_read.as_str(), "1000000000000000200");
        assert_eq!(
            store
                .mark_read(&channel, &MessageId("latest".to_owned()))
                .await
                .expect_err("must refuse")
                .code(),
            "bad_id"
        );
    }

    #[tokio::test]
    async fn the_fake_bounds_the_summary_cache_exactly_as_the_real_store_does() {
        // Parity, and it is load-bearing: every test of the summary path runs against this store,
        // so a fake with an unbounded cache would let an unbounded cache ship.
        let store = FakeStore::with_retention(Retention {
            max_summaries: 2,
            retain_days: 0,
            summary_retain_days: 0,
            ..Retention::default()
        });
        let key = |n: u64| SummaryKey {
            channel: ChannelId("1111111111".to_owned()),
            message: MessageId(format!("100000000000000000{n}")),
            content_hash: n,
            version: "current".to_owned(),
        };
        for n in 0..3_u64 {
            store
                .cache_summary(&key(n), &format!("summary {n}"), None)
                .await
                .expect("cache");
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            store.cached_summaries(),
            2,
            "the fake's summary cache grew past the ceiling the real one enforces"
        );
        assert_eq!(
            store.cached_summary(&key(0)).await.expect("read"),
            None,
            "the oldest entry must be the one evicted"
        );
        assert_eq!(
            store
                .cached_summary(&key(2))
                .await
                .expect("read")
                .map(|hit| hit.text)
                .as_deref(),
            Some("summary 2")
        );
    }

    #[tokio::test]
    async fn the_fake_keeps_an_entry_of_no_policy_exactly_as_the_real_store_does() {
        // `#233 llm-summarizer`: parity with the real store's kept class, which every thread-name
        // test runs against.
        let store = FakeStore::with_retention(Retention {
            max_summaries: 2,
            retain_days: 0,
            summary_retain_days: 0,
            ..Retention::default()
        });
        let key = |message: &str, version: &str| SummaryKey {
            channel: ChannelId("1111111111".to_owned()),
            message: MessageId(message.to_owned()),
            content_hash: 0,
            version: version.to_owned(),
        };
        let kept = key("thread-name:t", super::super::KEPT_VERSION);
        store
            .cache_summary(&kept, "runner-wedged", None)
            .await
            .expect("cache");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        for n in 0..5 {
            store
                .cache_summary(&key(&format!("10000000000000000{n}"), "current"), "s", None)
                .await
                .expect("cache");
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            store.cached_summaries(),
            3,
            "two summaries and the kept entry"
        );
        assert_eq!(
            store.forget_summaries_except("next").await.expect("sweep"),
            2
        );
        assert_eq!(
            store
                .cached_summary(&kept)
                .await
                .expect("read")
                .map(|hit| hit.text)
                .as_deref(),
            Some("runner-wedged")
        );
        for n in 0..3 {
            store
                .cache_summary(
                    &key(&format!("thread-name:{n}"), super::super::KEPT_VERSION),
                    "name",
                    None,
                )
                .await
                .expect("cache");
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            store.cached_summaries(),
            2,
            "kept entries are bounded on their own"
        );
        assert_eq!(store.cached_summary(&kept).await.expect("read"), None);
    }

    #[tokio::test]
    async fn the_fake_bookmark_follows_the_shared_rule_refuses_and_can_fail() {
        // `#240 reading-bookmark`. The fake answers a bookmark write as the real store does — a
        // stale revision refused, a repeat unchanged, a bad anchor refused before anything — and
        // can be made to fail, so a caller's error path is testable against it.
        use super::super::{BookmarkAnchor, BookmarkPlace};
        let store = FakeStore::new();
        let channel = ChannelId("1111111111".to_owned());
        let at = |id: &str| BookmarkAnchor {
            message_id: MessageId(id.to_owned()),
            place: BookmarkPlace::Above,
            timestamp: "2026-10-12T12:00:00Z".to_owned(),
            thread_id: None,
        };
        let first = store
            .set_bookmark(&channel, Some(&at("100")), None)
            .await
            .expect("set");
        assert!(first.applied && first.changed);
        let stale = store
            .set_bookmark(
                &channel,
                Some(&at("200")),
                Some(first.bookmark.revision - 1),
            )
            .await
            .expect("an answer");
        assert!(!stale.applied);
        assert_eq!(
            store.bookmark(&channel).await.expect("read"),
            first.bookmark
        );
        let repeat = store
            .set_bookmark(&channel, Some(&at("100")), Some(first.bookmark.revision))
            .await
            .expect("repeat");
        assert!(repeat.applied && !repeat.changed);
        let error = store
            .set_bookmark(&channel, Some(&at("")), None)
            .await
            .expect_err("a bad anchor");
        assert_eq!(error.code(), "bad_id");
        store.fail_next("the disk is full");
        store
            .set_bookmark(&channel, None, None)
            .await
            .expect_err("the armed failure fires");
        assert_eq!(
            store.bookmark(&channel).await.expect("read"),
            first.bookmark,
            "a failed write changed the bookmark"
        );
        store.purge_everything().await.expect("purge");
        assert_eq!(
            store.bookmark(&channel).await.expect("read"),
            super::super::Bookmark::unset()
        );
    }

    #[tokio::test]
    async fn the_fake_bookmark_agrees_with_a_one_slot_model_under_random_writes() {
        // `#240 reading-bookmark`. The property test `sqlite.rs` runs against a real file, run against
        // the fake, so the two cannot drift apart: random set, conditional set (with the revision
        // held, and with a stale one) and clear, across two channels, against one optional anchor
        // and one number per channel.
        use super::super::{BookmarkAnchor, BookmarkPlace};
        let store = FakeStore::new();
        let channels = [
            ChannelId("1111111111".to_owned()),
            ChannelId("2222222222".to_owned()),
        ];
        let mut model: [(i64, Option<BookmarkAnchor>); 2] = [(0, None), (0, None)];
        let mut seed: u64 = 240;
        let mut below = |n: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n
        };
        for step in 0..400 {
            let which = usize::try_from(below(2)).expect("small");
            let (held_revision, held_anchor) = model[which].clone();
            let anchor = (below(4) != 0).then(|| BookmarkAnchor {
                message_id: MessageId(format!("10000000000000{:05}", below(6))),
                place: if below(3) == 0 {
                    BookmarkPlace::Below
                } else {
                    BookmarkPlace::Above
                },
                timestamp: "2026-10-12T12:00:00Z".to_owned(),
                thread_id: None,
            });
            let condition = match below(3) {
                0 => None,
                1 => Some(held_revision),
                _ => Some(held_revision - 1),
            };
            let change = store
                .set_bookmark(&channels[which], anchor.as_ref(), condition)
                .await
                .expect("write");
            let refused = condition.is_some_and(|c| c != held_revision);
            assert_eq!(change.applied, !refused, "step {step}");
            if !refused && anchor != held_anchor {
                assert!(change.changed, "step {step}");
                assert!(change.bookmark.revision > held_revision, "step {step}");
                model[which] = (change.bookmark.revision, anchor);
            } else {
                assert!(!change.changed, "step {step}");
            }
            for (at, channel) in channels.iter().enumerate() {
                let read = store.bookmark(channel).await.expect("read");
                assert_eq!(
                    (read.revision, read.anchor),
                    model[at].clone(),
                    "step {step}"
                );
            }
        }
    }

    #[tokio::test]
    async fn the_fake_pins_bounds_and_refuses_exactly_as_the_real_store_does() {
        // Parity again: every page-facing test of `#206 pin-message` runs against this store.
        let store = FakeStore::new();
        let channel = ChannelId("1111111111".to_owned());
        let snapshot = |n: usize| PinSnapshot {
            message_id: MessageId(format!("{}", 1_000_000_000_000_001_000_u64 + n as u64)),
            author: "someone".to_owned(),
            author_id: crate::model::UserId("7".to_owned()),
            author_is_bot: false,
            content: format!("message {n}"),
            truncated: false,
            timestamp: format!("2026-10-04T12:{:02}:00Z", n % 60),
            thread_id: None,
            thread_root: false,
        };
        let first = store.pin(&channel, &snapshot(0)).await.expect("pin");
        assert!(first.changed && first.revision > 0);
        let repeat = store.pin(&channel, &snapshot(0)).await.expect("repeat");
        assert!(!repeat.changed && repeat.revision == first.revision);
        for n in 1..MAX_PINS_PER_CHANNEL {
            store.pin(&channel, &snapshot(n)).await.expect("pin");
        }
        let over = store
            .pin(&channel, &snapshot(MAX_PINS_PER_CHANNEL))
            .await
            .expect("one past the bound");
        assert_eq!(
            over.unpinned,
            vec![snapshot(0).message_id],
            "the wrong pin went"
        );
        assert_eq!(
            store.pins(&channel).await.expect("read").pins.len(),
            MAX_PINS_PER_CHANNEL
        );
        let error = store
            .pin(
                &channel,
                &PinSnapshot {
                    message_id: MessageId(String::new()),
                    ..snapshot(1)
                },
            )
            .await
            .expect_err("an empty id is not a message");
        assert_eq!(error.code(), "bad_id");
        let gone = store
            .unpin(&channel, &snapshot(1).message_id)
            .await
            .expect("unpin");
        assert!(gone.changed);
        assert!(
            !store
                .unpin(&channel, &snapshot(1).message_id)
                .await
                .expect("unpin again")
                .changed
        );
        store.fail_next("the disk is full");
        assert!(
            store.pins(&channel).await.is_err(),
            "the fake could not say no"
        );
        store.purge_everything().await.expect("purge");
        assert!(store.pins(&channel).await.expect("read").pins.is_empty());
        assert_eq!(store.pins_revision(&channel).await.expect("read"), 0);
    }
}
