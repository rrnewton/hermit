//! Clearing a conversation's unread state in Slack's own client. `#258 slack-clear-unread`.
//!
//! [`crate::upstream_clear`] decides WHEN a channel's newest message is pushed; this file is what
//! one push does at Slack. Slack keeps one read cursor per conversation for each account
//! (`last_read`), and `conversations.mark` moves it. Three facts about Slack shape everything here:
//!
//! * **Slack moves the cursor backwards without complaint.** `conversations.mark` puts it wherever
//!   it is told, so the never-backwards promise of
//!   [`crate::chat::ChatClient::clear_upstream_through`] is kept HERE: read `conversations.info`'s
//!   `last_read` first, and mark only a newer `ts`. A conversation whose cursor cannot be read is
//!   refused, never marked blind.
//! * **Nothing can mark a thread read with an ordinary token.** A thread the owner follows keeps
//!   a cursor of its own, which `conversations.mark` does not touch, and the method that moves it
//!   is open only to a browser session. So replies inside threads stay unread in Slack, and a
//!   channel narrowed to one thread is refused outright: marking its conversation would also clear
//!   messages vibe-talk never received.
//! * **Read, then mark, is not one step.** Slack has no compare-and-set. If the owner reads further
//!   in Slack in the moment between the two calls, the mark moves the cursor back to the message
//!   pushed, and whatever was read in that moment shows as unread again. It is not re-read
//!   afterwards, because a re-read cannot tell that move from the push's own; it heals by itself
//!   instead, since every message vibe-talk receives in the channel is pushed in its turn, so the
//!   next push marks those messages read again.
//!
//! A refusal that no retry can fix (the bridge lacks the method, the token lacks the scope, the
//! account is not in the conversation) is answered as [`ChatError::Refused`], so the loop stops
//! for that channel and Settings says why, rather than retrying it for ever.

use serde_json::Value;

use super::client::{refused, rfc3339, shape, Patience};
use super::ids;
use super::HttpSlackClient;
use crate::chat::{ChatError, UpstreamClearOutcome};
use crate::model::{ChannelId, MessageId};

/// What a Slack read cursor covers, as the outcome's `scope` says it.
pub const SCOPE: &str = "conversation";

/// Why a channel narrowed to one thread cannot be cleared, in words for Settings.
pub const THREAD_CHANNEL_REASON: &str = "this channel is one thread of a Slack conversation, and \
     Slack's read marker covers the whole conversation, so it would also clear messages vibe-talk \
     never received; Slack offers no way to mark one thread read";

/// Why a conversation whose cursor Slack did not report cannot be cleared.
pub const NO_CURSOR_REASON: &str = "Slack did not report this conversation's read marker \
     (conversations.info carried no last_read), so vibe-talk cannot tell whether marking would \
     move it backwards; a bridge in front of Slack needs updating to pass it through";

/// Slack `error` codes after which the same push can never succeed until someone changes
/// something by hand. Every other failure is retried by the loop.
const REFUSALS: [&str; 10] = [
    "unknown_method",
    "missing_scope",
    "not_in_channel",
    "not_allowed_token_type",
    "channel_not_found",
    "restricted_action",
    "access_denied",
    "no_permission",
    "is_archived",
    "team_access_not_granted",
];

impl HttpSlackClient {
    /// Move the account's read cursor in `channel`'s conversation through `through`, or in a dry
    /// run say whether it would move. Never moves it backwards.
    pub(super) async fn clear_through(
        &self,
        channel: &ChannelId,
        through: &MessageId,
        dry_run: bool,
    ) -> Result<UpstreamClearOutcome, ChatError> {
        if !self.upstream_clear {
            return Err(refused(
                "clearing unread in Slack is turned off for this provider; the deployment \
                 operator turns it on with upstream_clear = true",
            ));
        }
        let reference = Self::channel_ref(channel)?;
        if reference.thread_ts.is_some() {
            return Err(refused(THREAD_CHANNEL_REASON));
        }
        let target_micros = ids::message_id_micros(through)
            .filter(|micros| *micros > 0)
            .ok_or_else(|| refused(&format!("{through} is not a Slack message id")))?;
        let previous = self.read_cursor(&reference.conversation).await?;
        let outcome = |advanced: bool| UpstreamClearOutcome {
            scope: SCOPE.to_owned(),
            target: rfc3339(target_micros),
            previous: previous.map(rfc3339),
            advanced,
            dry_run,
        };
        if previous.is_some_and(|cursor| cursor >= target_micros) {
            return Ok(outcome(false));
        }
        if dry_run {
            return Ok(outcome(true));
        }
        self.call_with(
            "conversations.mark",
            &[
                ("channel", reference.conversation.clone()),
                ("ts", ids::micros_ts(target_micros)),
            ],
            // The push loop is serial across channels and waits out a rate limit itself, honouring
            // Slack's Retry-After; parking here would hold every other channel's push.
            Patience::BestEffort,
        )
        .await
        .map_err(|error| refusal("conversations.mark", error))?;
        Ok(outcome(true))
    }

    /// The account's read cursor in `conversation`, in microseconds; `None` when it has read
    /// nothing there yet (Slack's `0000000000.000000`).
    async fn read_cursor(&self, conversation: &str) -> Result<Option<i64>, ChatError> {
        let value = self
            .call_with(
                "conversations.info",
                &[("channel", conversation.to_owned())],
                Patience::BestEffort,
            )
            .await
            .map_err(|error| refusal("conversations.info", error))?;
        let info = value
            .get("channel")
            .filter(|info| info.is_object())
            .ok_or_else(|| shape("conversations.info answer has no \"channel\" object"))?;
        if info
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id != conversation)
        {
            return Err(shape(
                "conversations.info answered for a different conversation",
            ));
        }
        if info.get("is_member").and_then(Value::as_bool) == Some(false) {
            return Err(refused(
                "the Slack account is not a member of this conversation, so it has no read \
                 marker there to move",
            ));
        }
        match info.get("last_read") {
            Some(Value::String(text)) => ids::ts_micros(text)
                .map(|micros| (micros > 0).then_some(micros))
                .ok_or_else(|| shape("conversations.info \"last_read\" is not a Slack ts")),
            None | Some(Value::Null) => Err(refused(NO_CURSOR_REASON)),
            Some(_) => Err(shape("conversations.info \"last_read\" is not a string")),
        }
    }
}

/// A Slack answer to `method` that no retry can change, as a refusal in words for Settings;
/// anything else unchanged.
fn refusal(method: &str, error: ChatError) -> ChatError {
    let Some((code, needed)) = HttpSlackClient::slack_error_code(&error, method) else {
        return error;
    };
    if !REFUSALS.contains(&code) {
        return error;
    }
    ChatError::Refused(match code {
        "unknown_method" => format!(
            "the Slack bridge does not offer {method} (it answered unknown_method), so it needs \
             updating before this channel can be cleared"
        ),
        "missing_scope" => format!(
            "the Slack token lacks a scope {method} needs{needed}; marking read takes \
             channels:write, groups:write, im:write or mpim:write, by the conversation's kind",
            needed = needed
                .map(|scope| format!(" ({scope})"))
                .unwrap_or_default()
        ),
        "not_in_channel" => {
            "the Slack account is not a member of this conversation (not_in_channel)".to_owned()
        }
        "not_allowed_token_type" => {
            "this kind of Slack token cannot mark conversations read (not_allowed_token_type); \
             it takes the owner's own user token"
                .to_owned()
        }
        "channel_not_found" => {
            "Slack cannot find this conversation for this account (channel_not_found)".to_owned()
        }
        "restricted_action" => {
            "Slack refused to mark this conversation read (restricted_action); a bridge in front \
             of Slack answers this when its marking read is turned off"
                .to_owned()
        }
        other => format!("Slack refused to mark this conversation read ({other})"),
    })
}
