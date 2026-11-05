//! Slack identifiers translated into the snowflake-ordered model the rest of the server speaks.
//!
//! # Message ids
//!
//! A Slack message is identified, within its conversation, by its `ts`: `"1700000000.000100"`,
//! whole seconds and a six-digit microsecond fraction. The rest of vibe-talk orders, pages, marks
//! read and cuts time ranges with [`MessageId::numeric`], [`MessageId::created_at_ms`] and
//! [`MessageId::at_time_ms`], all of which assume a Discord snowflake. So a `ts` is ENCODED as one:
//!
//! ```text
//! ms        = secs * 1000 + micros / 1000
//! snowflake = ((ms - DISCORD_EPOCH_MS) << 22) | (micros % 1000)
//! ```
//!
//! The millisecond lands in the timestamp bits, so `created_at_ms` is exact; the sub-millisecond
//! remainder (0–999) lands in the low bits that a Discord snowflake spends on worker and sequence,
//! so the encoding is lossless and orders exactly as `ts` does. A boundary minted by
//! [`MessageId::at_time_ms`] has zero low bits and decodes to `secs.mmm000`, which is precisely the
//! instant it names.
//!
//! Two consequences are deliberate. A message from before 2015-01-01 cannot be encoded (a negative
//! snowflake is not a thing) and is omitted from reads; Slack itself launched in 2013, so a very
//! old workspace can contain a few. And a numeric id whose low bits exceed 999 was not produced
//! here, so decoding refuses it rather than inventing a `ts` Slack would not recognise.
//!
//! # Channel ids
//!
//! A whole conversation is its own Slack id (`C…` public, `G…` private, `D…` direct). A channel
//! narrowed to one thread is `"{conversation}~{thread_ts}"`; both `~` and `.` are path-safe, so
//! the id travels through every route unchanged.

use crate::model::MessageId;

/// The first millisecond Discord counts snowflakes from: 2015-01-01T00:00:00Z.
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
/// Timestamp bits in a snowflake.
const TIMESTAMP_BITS: u32 = 42;
/// Low bits below the timestamp; only 0..=999 are ever used here.
const LOW_BITS: u32 = 22;
/// Separator between a conversation and its thread in a thread-scoped channel id.
pub const THREAD_SEPARATOR: char = '~';

/// Parse a Slack `ts` into microseconds since the Unix epoch.
///
/// Accepts whole seconds with an optional fraction of one to six digits, which is normalised by
/// right-padding: `"1700000000.0001"` is `…000100`. Anything else is refused.
#[must_use]
pub fn ts_micros(ts: &str) -> Option<i64> {
    let (secs, frac) = match ts.split_once('.') {
        Some((secs, frac)) if !frac.is_empty() => (secs, frac),
        Some(_) => return None,
        None => (ts, ""),
    };
    if secs.is_empty()
        || secs.len() > 12
        || frac.len() > 6
        || !secs.bytes().all(|byte| byte.is_ascii_digit())
        || !frac.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let secs: i64 = secs.parse().ok()?;
    let mut micros: i64 = if frac.is_empty() {
        0
    } else {
        frac.parse().ok()?
    };
    for _ in frac.len()..6 {
        micros *= 10;
    }
    secs.checked_mul(1_000_000)?.checked_add(micros)
}

/// Render microseconds since the Unix epoch as a canonical Slack `ts`.
#[must_use]
pub fn micros_ts(micros: i64) -> String {
    format!(
        "{}.{:06}",
        micros.div_euclid(1_000_000),
        micros.rem_euclid(1_000_000)
    )
}

/// Normalise a `ts` to its canonical six-digit form, refusing a malformed one.
#[must_use]
pub fn canonical_ts(ts: &str) -> Option<String> {
    ts_micros(ts).map(micros_ts)
}

/// Encode microseconds since the Unix epoch as a snowflake-shaped [`MessageId`].
#[must_use]
pub fn micros_message_id(micros: i64) -> Option<MessageId> {
    let ms = micros.div_euclid(1000);
    let low = u64::try_from(micros.rem_euclid(1000)).ok()?;
    let since = u64::try_from(ms.checked_sub(DISCORD_EPOCH_MS)?).ok()?;
    if since >= 1_u64 << TIMESTAMP_BITS {
        return None;
    }
    Some(MessageId(((since << LOW_BITS) | low).to_string()))
}

/// Decode a snowflake-shaped [`MessageId`] back to microseconds since the Unix epoch.
#[must_use]
pub fn message_id_micros(id: &MessageId) -> Option<i64> {
    let text = id.as_str();
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let raw: u64 = text.parse().ok()?;
    let low = raw & ((1_u64 << LOW_BITS) - 1);
    if low >= 1000 {
        return None;
    }
    let ms = i64::try_from(raw >> LOW_BITS)
        .ok()?
        .checked_add(DISCORD_EPOCH_MS)?;
    ms.checked_mul(1000)?.checked_add(i64::try_from(low).ok()?)
}

/// Encode a Slack `ts` as a [`MessageId`] that orders, pages and dates like a Discord snowflake.
///
/// `None` for a malformed `ts` and for an instant before 2015-01-01, which a snowflake cannot hold.
#[must_use]
pub fn message_id_from_ts(ts: &str) -> Option<MessageId> {
    micros_message_id(ts_micros(ts)?)
}

/// Decode a [`MessageId`] produced by [`message_id_from_ts`], or a time boundary from
/// [`MessageId::at_time_ms`], back to its canonical Slack `ts`.
#[must_use]
pub fn ts_from_message_id(id: &MessageId) -> Option<String> {
    message_id_micros(id).map(micros_ts)
}

/// Whether `id` has the shape of a Slack conversation id: `C`, `G` or `D`, then eight or more
/// upper-case letters and digits.
#[must_use]
pub fn is_conversation_id(id: &str) -> bool {
    id.len() >= 9
        && id.len() <= 32
        && matches!(id.as_bytes()[0], b'C' | b'G' | b'D')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

/// Whether `id` has the shape of a Slack workspace or enterprise id: `T` or `E` and alphanumerics.
fn is_team_id(id: &str) -> bool {
    id.len() >= 2
        && id.len() <= 32
        && matches!(id.as_bytes()[0], b'T' | b'E')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

/// A vibe-talk channel id resolved to the Slack conversation and optional thread it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelRef {
    /// The Slack conversation id.
    pub conversation: String,
    /// The canonical root `ts` when the channel is narrowed to one thread.
    pub thread_ts: Option<String>,
}

impl ChannelRef {
    /// Read a vibe-talk channel id: `C…` or `C…~ts`.
    #[must_use]
    pub fn parse(channel: &str) -> Option<Self> {
        match channel.split_once(THREAD_SEPARATOR) {
            None => is_conversation_id(channel).then(|| Self {
                conversation: channel.to_owned(),
                thread_ts: None,
            }),
            Some((conversation, ts)) => {
                let canonical = canonical_ts(ts)?;
                (is_conversation_id(conversation) && canonical == ts).then(|| Self {
                    conversation: conversation.to_owned(),
                    thread_ts: Some(canonical),
                })
            }
        }
    }

    /// The vibe-talk channel id for this reference.
    #[must_use]
    pub fn channel_id(&self) -> String {
        match &self.thread_ts {
            Some(ts) => format!("{}{THREAD_SEPARATOR}{ts}", self.conversation),
            None => self.conversation.clone(),
        }
    }
}

/// Whether `source` is a link to a Slack host at all, parseable or not.
#[must_use]
pub fn is_slack_link(source: &str) -> bool {
    reqwest::Url::parse(source.trim()).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host == "slack.com" || host.ends_with(".slack.com"))
    })
}

/// Read the `p1700000000000100` permalink segment as a canonical `ts`.
fn permalink_ts(segment: &str) -> Option<String> {
    let digits = segment.strip_prefix('p')?;
    if digits.len() != 16 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    canonical_ts(&format!("{}.{}", &digits[..10], &digits[10..]))
}

/// Parse an operator-supplied Slack reference into `(conversation id, optional thread ts)`.
///
/// Accepted shapes:
///
/// * a bare conversation id, `C0123ABCD`, or a thread-scoped channel id, `C0123ABCD~1700000000.000100`;
/// * a message or thread permalink, `https://<workspace>.slack.com/archives/C0123ABCD/p1700000000000100`,
///   where a `?thread_ts=…` query (the link to a REPLY) names the thread that reply belongs to;
/// * a conversation link, `https://<workspace>.slack.com/archives/C0123ABCD`;
/// * a web-client link, `https://app.slack.com/client/T0123/C0123ABCD`, optionally followed by
///   `/thread/C0123ABCD-1700000000.000100`.
///
/// Returns `None` for anything else, including a Slack link of some other kind.
#[must_use]
pub fn parse_source(source: &str) -> Option<(String, Option<String>)> {
    let source = source.trim();
    if is_conversation_id(source) {
        return Some((source.to_owned(), None));
    }
    if let Some((conversation, ts)) = source.split_once(THREAD_SEPARATOR) {
        return is_conversation_id(conversation)
            .then(|| canonical_ts(ts))
            .flatten()
            .map(|ts| (conversation.to_owned(), Some(ts)));
    }
    if !is_slack_link(source) {
        return None;
    }
    let url = reqwest::Url::parse(source).ok()?;
    let segments: Vec<&str> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    match segments.as_slice() {
        ["archives", conversation, rest @ ..] if is_conversation_id(conversation) => {
            let thread_query = url
                .query_pairs()
                .find(|(key, _)| key == "thread_ts")
                .map(|(_, value)| value.into_owned());
            let thread = match rest {
                [] => None,
                [message] => Some(match thread_query {
                    Some(parent) => canonical_ts(&parent)?,
                    None => permalink_ts(message)?,
                }),
                _ => return None,
            };
            Some(((*conversation).to_owned(), thread))
        }
        ["client", team, conversation, rest @ ..]
            if url.host_str() == Some("app.slack.com")
                && is_team_id(team)
                && is_conversation_id(conversation) =>
        {
            let thread = match rest {
                [] => None,
                ["thread", scoped] => {
                    let (owner, ts) = scoped.split_once('-')?;
                    if owner != *conversation {
                        return None;
                    }
                    Some(canonical_ts(ts)?)
                }
                _ => return None,
            };
            Some(((*conversation).to_owned(), thread))
        }
        _ => None,
    }
}
