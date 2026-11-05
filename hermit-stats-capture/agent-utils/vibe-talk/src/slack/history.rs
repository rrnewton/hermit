//! Bounded walks over `conversations.history` and `conversations.replies`.
//!
//! The rest of the server asks for Discord's two page shapes: the NEWEST `n` messages older than a
//! cursor, and the OLDEST `n` messages newer than one. Slack serves each feed in one fixed order —
//! history newest first, replies oldest first — and a server may cap any page well below the
//! `limit` asked for. Two of the four combinations therefore follow the feed's own order and stop
//! as soon as they have enough; the other two need the whole window between the cursor and the end.
//!
//! # Every walk is bounded, and the bound never skips a message
//!
//! A walk spends at most [`MAX_WALK_PAGES`] (or [`MAX_WINDOW_PAGES`]) requests before it changes
//! strategy, and what it returns is always gap-free against the cursor it was given: walking again
//! from the newest (or oldest) returned message continues exactly where this page stopped. This
//! matters most for [`crate::live::poll_once`], which walks forward with `after=` and would lose any
//! message a page skipped, for good.
//!
//! * In feed order (history backward, replies forward) the first pages already touch the cursor,
//!   so running out of pages simply returns a SHORTER page. The caller's next step resumes from it.
//! * Against feed order (history forward, replies backward) running out of pages leaves the
//!   messages nearest the cursor unread. Rather than return the far ones, the walk searches by
//!   time — galloping outward from the cursor with single-page probes — until a window adjacent
//!   to the cursor answers in one page, and returns that.
//! * Proving that a stretch holds no real messages means reading it, so a run of system events
//!   longer than [`MAX_NARROWING_PROBES`] probes can read is not crossed in one call. That call
//!   returns NO messages (never later ones) and remembers how far it proved the stretch empty; the
//!   next call with the same cursor — the live poller's next tick — resumes from there. A search
//!   that could not prove anything at all fails the read instead.

use serde_json::Value;

use super::client::{shape, HttpSlackClient, Raw};
use crate::chat::ChatError;

/// Most pages one in-order walk reads before returning what it has.
pub const MAX_WALK_PAGES: usize = 25;
/// Most pages a whole-window collection reads before narrowing instead.
pub const MAX_WINDOW_PAGES: usize = 10;
/// Most single-page probes the narrowing fallback sends before failing the read.
pub const MAX_NARROWING_PROBES: usize = 48;
/// The page size asked of Slack for scans; a server may answer with fewer.
pub const SCAN_PAGE_LIMIT: u16 = 200;
/// How long a stretch proven empty of real messages is remembered for the next walk.
const PROVEN_TTL: std::time::Duration = std::time::Duration::from_secs(600);
/// Most proven stretches remembered.
const PROVEN_MAX: usize = 256;

/// Which feed a walk reads.
#[derive(Clone, Copy, Debug)]
pub(super) enum Feed<'a> {
    /// `conversations.history` of one conversation: newest first, thread replies excluded.
    History(&'a str),
    /// `conversations.replies` of one thread: oldest first, the root included.
    Replies(&'a str, &'a str),
}

/// One page as Slack answered it.
pub(super) struct Page {
    pub raws: Vec<Raw>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

/// The result of a newest-first walk that may have run out of pages.
pub(super) struct Run {
    /// Real messages, oldest first, at most the number asked for (the newest ones).
    pub real: Vec<Raw>,
    /// When the walk stopped on its page budget: the oldest instant it had scanned. Everything
    /// between that instant and the oldest returned message was a system event.
    pub exhausted_at: Option<i64>,
}

/// Which side of the cursor a narrowing walk wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Toward {
    /// The oldest messages newer than the cursor, from a newest-first feed.
    Newer,
    /// The newest messages older than the cursor, from an oldest-first feed.
    Older,
}

impl Toward {
    /// Position along the walk: increasing away from the cursor.
    fn coord(self, micros: i64) -> i64 {
        match self {
            Self::Newer => micros,
            Self::Older => -micros,
        }
    }
}

/// The `want` real messages nearest the cursor, returned oldest first.
fn nearest(raws: &[Raw], want: usize, toward: Toward) -> Vec<Raw> {
    let mut real: Vec<Raw> = raws.iter().filter(|raw| raw.is_real()).cloned().collect();
    real.sort_by_key(|raw| toward.coord(raw.micros));
    real.dedup_by_key(|raw| raw.micros);
    real.truncate(want);
    real.sort_by_key(|raw| raw.micros);
    real
}

/// A bound one hour past the present, standing in for "no upper bound" where one must be named.
fn far_future_micros() -> i64 {
    jiff::Timestamp::now()
        .as_microsecond()
        .saturating_add(3_600_000_000)
}

impl HttpSlackClient {
    /// Read one page of a feed between two EXCLUSIVE bounds, or inclusive ones when asked.
    pub(super) async fn page(
        &self,
        feed: Feed<'_>,
        oldest: Option<i64>,
        latest: Option<i64>,
        inclusive: bool,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<Page, ChatError> {
        let (method, conversation) = match feed {
            Feed::History(conversation) => ("conversations.history", conversation),
            Feed::Replies(conversation, _) => ("conversations.replies", conversation),
        };
        let mut params = vec![("channel", conversation.to_owned())];
        if let Feed::Replies(_, ts) = feed {
            params.push(("ts", ts.to_owned()));
        }
        params.push(("limit", limit.clamp(1, 999).to_string()));
        if let Some(cursor) = cursor {
            params.push(("cursor", cursor.to_owned()));
        }
        if let Some(oldest) = oldest {
            params.push(("oldest", super::ids::micros_ts(oldest.max(0))));
        }
        if let Some(latest) = latest {
            params.push(("latest", super::ids::micros_ts(latest.max(0))));
        }
        if inclusive {
            params.push(("inclusive", "true".to_owned()));
        }
        let value = self.call(method, &params).await?;
        let raws = value
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| shape(&format!("{method} answer has no \"messages\" array")))?
            .iter()
            .cloned()
            .map(Raw::parse)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            // Bounds are applied here as well as upstream: `conversations.replies` may include the
            // root outside the requested window, and a lenient server may ignore a bound.
            .filter(|raw| {
                let above = oldest.is_none_or(|oldest| {
                    raw.micros > oldest || (inclusive && raw.micros == oldest)
                });
                let below = latest.is_none_or(|latest| {
                    raw.micros < latest || (inclusive && raw.micros == latest)
                });
                above && below
            })
            .collect();
        let has_more = value
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let next_cursor = value
            .get("response_metadata")
            .and_then(|metadata| metadata.get("next_cursor"))
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_owned);
        if has_more && next_cursor.is_none() {
            return Err(shape(&format!(
                "{method} reported more messages but gave no cursor to read them"
            )));
        }
        Ok(Page {
            raws,
            has_more,
            next_cursor,
        })
    }

    /// The newest `want` real messages strictly older than `latest`, from history.
    ///
    /// In feed order: on its page budget it returns what it has, which is gap-free against
    /// `latest`, and says where it stopped.
    pub(super) async fn history_newest(
        &self,
        conversation: &str,
        latest: Option<i64>,
        want: usize,
    ) -> Result<Run, ChatError> {
        let mut real: Vec<Raw> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut oldest_seen: Option<i64> = None;
        for _ in 0..MAX_WALK_PAGES {
            let ask = u16::try_from(want.saturating_sub(real.len()) + 10)
                .unwrap_or(SCAN_PAGE_LIMIT)
                .min(SCAN_PAGE_LIMIT);
            let page = self
                .page(
                    Feed::History(conversation),
                    None,
                    latest,
                    false,
                    ask,
                    cursor.as_deref(),
                )
                .await?;
            for raw in page.raws {
                oldest_seen = Some(oldest_seen.map_or(raw.micros, |seen| seen.min(raw.micros)));
                if raw.is_real() {
                    real.push(raw);
                }
            }
            if real.len() >= want || !page.has_more {
                return Ok(Run {
                    real: nearest(&real, want, Toward::Older),
                    exhausted_at: None,
                });
            }
            cursor = advance(cursor, page.next_cursor)?;
        }
        Ok(Run {
            real: nearest(&real, want, Toward::Older),
            exhausted_at: oldest_seen,
        })
    }

    /// The oldest `want` real messages strictly newer than `after`, from history.
    pub(super) async fn history_oldest_after(
        &self,
        conversation: &str,
        after: i64,
        want: usize,
    ) -> Result<Vec<Raw>, ChatError> {
        let feed = Feed::History(conversation);
        let key = format!("history:{conversation}");
        let proven = self.proven_from(&key, after);
        let start = proven.unwrap_or(after).max(after);
        // Resuming a walk that already knows the window is large: one page is enough to anchor
        // the search, and re-reading the far end in full would only repeat the last call.
        let pages = if proven.is_some() {
            1
        } else {
            MAX_WINDOW_PAGES
        };
        let (primary, complete) = self.window(feed, Some(start), None, pages).await?;
        if complete {
            return Ok(nearest(&primary, want, Toward::Newer));
        }
        match self
            .narrow(feed, Toward::Newer, start, &primary, want)
            .await?
        {
            Narrowed::Found(found) => Ok(found),
            Narrowed::Proven(to) => {
                self.remember_proven(&key, after, to);
                Ok(Vec::new())
            }
        }
    }

    /// The oldest `want` real messages of a thread strictly newer than `after`.
    ///
    /// In feed order: on its page budget it returns what it has, gap-free against `after`.
    pub(super) async fn replies_oldest_after(
        &self,
        conversation: &str,
        thread_ts: &str,
        after: Option<i64>,
        want: usize,
    ) -> Result<Vec<Raw>, ChatError> {
        let mut real: Vec<Raw> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_WALK_PAGES {
            let ask = u16::try_from(want.saturating_sub(real.len()) + 10)
                .unwrap_or(SCAN_PAGE_LIMIT)
                .min(SCAN_PAGE_LIMIT);
            let page = self
                .page(
                    Feed::Replies(conversation, thread_ts),
                    after,
                    None,
                    false,
                    ask,
                    cursor.as_deref(),
                )
                .await?;
            real.extend(page.raws.into_iter().filter(Raw::is_real));
            if real.len() >= want || !page.has_more {
                break;
            }
            cursor = advance(cursor, page.next_cursor)?;
        }
        Ok(nearest(&real, want, Toward::Newer))
    }

    /// Every message of a thread strictly newer than `after`, system events included, root
    /// excluded, oldest first, each once; or `None` when there are more than `most` of them, or
    /// when the walk ran out of pages before the end of the thread. `#282 slack-flat-skip`,
    /// remote-ops audit W8 review.
    ///
    /// In feed order, so a handful of new replies costs one call, as with
    /// [`Self::replies_oldest_after`]. Unlike that, it counts what is not kept: a reply count that
    /// grew by exactly as many messages as came back, every one of them real, is what shows that
    /// nothing older was deleted meanwhile.
    pub(super) async fn replies_all_after(
        &self,
        conversation: &str,
        thread_ts: &str,
        after: i64,
        most: usize,
    ) -> Result<Option<Vec<Raw>>, ChatError> {
        let mut all: Vec<Raw> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_WALK_PAGES {
            let ask = u16::try_from((most + 1).saturating_sub(all.len()) + 10)
                .unwrap_or(SCAN_PAGE_LIMIT)
                .min(SCAN_PAGE_LIMIT);
            let page = self
                .page(
                    Feed::Replies(conversation, thread_ts),
                    Some(after),
                    None,
                    false,
                    ask,
                    cursor.as_deref(),
                )
                .await?;
            all.extend(page.raws.into_iter().filter(|raw| raw.ts != thread_ts));
            all.sort_by_key(|raw| raw.micros);
            all.dedup_by_key(|raw| raw.micros);
            if all.len() > most {
                return Ok(None);
            }
            if !page.has_more {
                return Ok(Some(all));
            }
            cursor = advance(cursor, page.next_cursor)?;
        }
        Ok(None)
    }

    /// The newest `want` real messages of a thread strictly older than `before`, or the newest
    /// of all when `before` is `None`. The root is part of the thread and appears on its oldest page.
    pub(super) async fn replies_newest_before(
        &self,
        conversation: &str,
        thread_ts: &str,
        before: Option<i64>,
        want: usize,
    ) -> Result<Vec<Raw>, ChatError> {
        Ok(self
            .replies_newest(conversation, thread_ts, before, want)
            .await?
            .0)
    }

    /// The newest `want` real messages of a thread, as [`Self::replies_newest_before`] with no
    /// bound, and how many real replies the whole thread holds, root excluded, counted from that
    /// same read at no further call. `#282 slack-flat-skip`, remote-ops audit W8 review.
    ///
    /// The count is `None` unless one window read the thread from its root to the present: a
    /// thread too long for [`MAX_WINDOW_PAGES`] pages is answered by narrowing, which shows only
    /// the stretch next to the present, and a walk resumed past a stretch already proven empty
    /// skipped that stretch this time. Neither says anything about the rest of the thread.
    pub(super) async fn replies_newest_counted(
        &self,
        conversation: &str,
        thread_ts: &str,
        want: usize,
    ) -> Result<(Vec<Raw>, Option<u64>), ChatError> {
        self.replies_newest(conversation, thread_ts, None, want)
            .await
    }

    /// [`Self::replies_newest_before`], and the count [`Self::replies_newest_counted`] describes,
    /// whenever the read covered the whole of the thread older than `before`.
    async fn replies_newest(
        &self,
        conversation: &str,
        thread_ts: &str,
        before: Option<i64>,
        want: usize,
    ) -> Result<(Vec<Raw>, Option<u64>), ChatError> {
        let feed = Feed::Replies(conversation, thread_ts);
        let key = format!("replies:{conversation}:{thread_ts}");
        let cursor = before.unwrap_or(i64::MAX);
        let anchor = before.unwrap_or_else(far_future_micros);
        let proven = self.proven_from(&key, cursor);
        let start = proven.map_or(anchor, |proven| proven.min(anchor));
        let pages = if proven.is_some() {
            1
        } else {
            MAX_WINDOW_PAGES
        };
        let (primary, complete) = self.window(feed, None, Some(start), pages).await?;
        if complete {
            let whole = proven.is_none().then(|| {
                let mut real: Vec<i64> = primary
                    .iter()
                    .filter(|raw| raw.is_real() && raw.ts != thread_ts)
                    .map(|raw| raw.micros)
                    .collect();
                real.sort_unstable();
                real.dedup();
                u64::try_from(real.len()).unwrap_or(u64::MAX)
            });
            return Ok((nearest(&primary, want, Toward::Older), whole));
        }
        match self
            .narrow(feed, Toward::Older, start, &primary, want)
            .await?
        {
            Narrowed::Found(found) => Ok((found, None)),
            Narrowed::Proven(to) => {
                self.remember_proven(&key, cursor, to);
                Ok((Vec::new(), None))
            }
        }
    }

    /// Every message between two exclusive bounds, within `max_pages` pages.
    ///
    /// Returns what was read, system events included, and whether the window was exhausted.
    async fn window(
        &self,
        feed: Feed<'_>,
        oldest: Option<i64>,
        latest: Option<i64>,
        max_pages: usize,
    ) -> Result<(Vec<Raw>, bool), ChatError> {
        let mut raws = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..max_pages {
            let page = self
                .page(
                    feed,
                    oldest,
                    latest,
                    false,
                    SCAN_PAGE_LIMIT,
                    cursor.as_deref(),
                )
                .await?;
            raws.extend(page.raws);
            if !page.has_more {
                return Ok((raws, true));
            }
            cursor = advance(cursor, page.next_cursor)?;
        }
        Ok((raws, false))
    }

    /// Find a gap-free run adjacent to the cursor after a window collection ran out of pages.
    ///
    /// Positions are measured AWAY from the cursor (see [`Toward::coord`]), so both directions are
    /// one algorithm: the feed always returns the FAR end of a window first. `primary` is the
    /// contiguous run the window collection did read, starting at its nearest message and extending
    /// away from the cursor; the unknown gap lies between the cursor and that message.
    ///
    /// The search gallops outward from the cursor one single-page probe at a time. A probe of
    /// `(lo, end]` that fits in one page is complete: real messages in it are the answer, and if
    /// it holds none the whole of it is known to be empty, `lo` moves to `end`, and the next probe
    /// is twice as wide. A probe that does not fit shows only its far end — a run known in full
    /// from its nearest message out to `end` — which is kept, and the next probe is narrowed to
    /// stop short of it. When the gap before a kept run closes, that run is contiguous with the
    /// cursor: its nearest real messages are the answer, or, if it holds only system events, the
    /// search continues beyond it.
    ///
    /// Proving that a stretch holds no real messages means reading it, so a long enough run of
    /// system events cannot be crossed in one call. When the probes run out, the stretch already
    /// proven empty is returned as [`Narrowed::Proven`] for the caller to remember; the next call
    /// with the same cursor resumes from there instead of starting over.
    async fn narrow(
        &self,
        feed: Feed<'_>,
        toward: Toward,
        cursor: i64,
        primary: &[Raw],
        want: usize,
    ) -> Result<Narrowed, ChatError> {
        let coord = |raw: &Raw| toward.coord(raw.micros);
        let Some(ceiling) = primary.iter().map(coord).min() else {
            return Ok(Narrowed::Found(Vec::new()));
        };
        let mut lo = toward.coord(cursor);
        // Runs read in full, nearest last: (messages, how far out the run is known).
        let mut kept: Vec<(Vec<Raw>, i64)> = Vec::new();
        let mut width = ((ceiling - lo) / 2).max(1);
        for _ in 0..MAX_NARROWING_PROBES {
            let hi = kept
                .last()
                .and_then(|(run, _)| run.iter().map(coord).min())
                .unwrap_or(ceiling);
            if hi - lo <= 1 {
                let Some((run, end)) = kept.pop() else {
                    return Ok(Narrowed::Found(nearest(primary, want, toward)));
                };
                let found = nearest(&run, want, toward);
                if !found.is_empty() {
                    return Ok(Narrowed::Found(found));
                }
                // One page covered that run's span, so the same span is a good next stride.
                width = (end - hi).max(1);
                lo = end;
                continue;
            }
            let end = lo.saturating_add(width).min(hi - 1);
            let (oldest, latest) = match toward {
                Toward::Newer => (lo, end + 1),
                Toward::Older => (-(end + 1), -lo),
            };
            let page = self
                .page(
                    feed,
                    Some(oldest),
                    Some(latest),
                    false,
                    SCAN_PAGE_LIMIT,
                    None,
                )
                .await?;
            let raws: Vec<Raw> = page
                .raws
                .into_iter()
                .filter(|raw| coord(raw) > lo && coord(raw) <= end)
                .collect();
            if page.has_more {
                let nearest_seen = raws
                    .iter()
                    .map(coord)
                    .min()
                    .ok_or_else(|| shape("a page reported more messages but held none"))?;
                kept.push((raws, end));
                width = ((nearest_seen - lo) / 2).max(1);
            } else {
                let found = nearest(&raws, want, toward);
                if !found.is_empty() {
                    return Ok(Narrowed::Found(found));
                }
                lo = end;
                width = width.saturating_mul(2);
            }
        }
        if lo > toward.coord(cursor) {
            return Ok(Narrowed::Proven(match toward {
                Toward::Newer => lo,
                Toward::Older => -lo,
            }));
        }
        Err(ChatError::Refused(format!(
            "the messages next to this cursor could not be isolated within {MAX_NARROWING_PROBES} \
             narrowing requests; no partial page was returned, so nothing was skipped"
        )))
    }

    /// Where a previous call proved the stretch after `cursor` empty, if it did.
    fn proven_from(&self, key: &str, cursor: i64) -> Option<i64> {
        let mut proven = self
            .proven
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        proven.retain(|_, (_, at)| at.elapsed() < PROVEN_TTL);
        proven.get(&(key.to_owned(), cursor)).map(|(to, _)| *to)
    }

    fn remember_proven(&self, key: &str, cursor: i64, to: i64) {
        let mut proven = self
            .proven
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if proven.len() >= PROVEN_MAX {
            proven.clear();
        }
        proven.insert((key.to_owned(), cursor), (to, std::time::Instant::now()));
    }
}

/// What a narrowing search concluded.
enum Narrowed {
    /// The real messages nearest the cursor, gap-free against it (possibly none at all).
    Found(Vec<Raw>),
    /// No real message lies between the cursor and this instant; the search ran out before
    /// finding one beyond it.
    Proven(i64),
}

/// Move to the next page's cursor, refusing one that does not advance.
fn advance(current: Option<String>, next: Option<String>) -> Result<Option<String>, ChatError> {
    match next {
        Some(next) if current.as_deref() != Some(next.as_str()) => Ok(Some(next)),
        _ => Err(shape("a paging cursor did not advance")),
    }
}
