//! "Ask me before the voice agent sends": the owner's one switch over what `post_reply` does.
//!
//! `#264 voice-send-approval`. Since `#34 voice-chat-write-confirm` the voice agent's `post_reply`
//! has never posted anything: it leaves one pending proposal ([`crate::post_gate`]) that the owner
//! confirms outside the model's tools. That is the right default, and it stays the default. But the
//! owner drives with this, and for him "tell the coding agent to rebase" followed by a read-back
//! and a question is friction he does not want. So the confirmation is a setting he owns:
//!
//! * **On ([`Approval::Ask`], the shipped default).** Exactly the `#34` flow. The agent reads the
//!   draft back and asks "Send it?"; the voice page sends it when he taps Send, or when the page
//!   itself — never the model — hears a plain yes right after an uninterrupted read-back.
//! * **Off ([`Approval::Direct`]).** `post_reply` posts at once, through [`crate::ops::reply`] —
//!   the same function a confirmed proposal is posted with, so the allowlist, the write policy,
//!   the emptiness check and long-message splitting are the ones every other send gets — and the
//!   model is told `SENT.` with what went where, or why it did not.
//!
//! # A direct send is posted at most once
//!
//! A commit cannot be repeated: it spends a single-use handle. A direct send has no handle, and a
//! hosted tool call can time out, or be retried by its transport, after the post has landed — and
//! an owner told it failed says "try again". So the server remembers each direct send while it is
//! on its way and for [`crate::post_gate::DIRECT_REPEAT_WINDOW`] after it ends, and the same words
//! to the same place again in that time are answered from that memory instead of posted:
//! `SENT already`, or `NOT CONFIRMED` / `PARTLY SENT already` when the earlier one may have posted.
//! A send that certainly posted nothing is forgotten, so it can always be tried again. And a post
//! whose answer was lost is never reported as `NOT SENT`: it is `NOT CONFIRMED`, with the
//! instruction not to call again for the same words.
//!
//! # Where the choice lives
//!
//! The configuration file's `[conversation] ask_before_send` is the SHIPPED default; the owner's
//! own choice is one row in the store, written only by a write-scope `PUT /api/v1/voice-settings`
//! that Settings makes. No MCP tool can reach it — a model that could turn confirmation off could
//! then post anything a message it read asked for — and [`crate::mcp::protocol::dispatch`] refuses
//! any tool name the manifest does not list.
//!
//! # It fails toward asking
//!
//! [`current`] answers [`Approval::Ask`] whenever the store cannot say what the owner chose. A
//! store that is not configured at all is different: there is no choice to lose, so the configured
//! default applies, exactly as it does for the noise rules.
//!
//! # Why this is its own module
//!
//! The direct send calls [`crate::ops::reply`] and nothing below it. How a long post is cut, and
//! where its parts go — the first in the conversation and the rest in the thread under it where
//! the provider continues a long message there (`#263 post-split-per-provider`) — is decided inside
//! `ops::reply_scoped`, and keeping this out of that function means there is still exactly one
//! place that decides how a long message goes out.

use crate::model::{ChannelInfo, Message};
use crate::ops::{self, OpError};
use crate::post_gate::{Binding, DirectOutcome, Repeat};
use crate::state::AppState;
use crate::store::StoreError;

/// What `post_reply` does with a message right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// Propose it and wait for the owner's confirmation. The shipped default.
    Ask,
    /// Post it at once. The owner turned confirmation off.
    Direct,
}

impl Approval {
    /// The approval that a stored or configured `ask_before_send` means.
    #[must_use]
    pub fn from_ask(ask: bool) -> Self {
        if ask {
            Self::Ask
        } else {
            Self::Direct
        }
    }

    /// Whether the owner is asked first.
    #[must_use]
    pub fn asks(self) -> bool {
        self == Self::Ask
    }
}

/// The approval in force, as of this call. Read on every `tools/list`, profile and `post_reply`,
/// so a change in Settings reaches the server's behaviour at once.
///
/// A saved choice wins. No choice, or no store configured, is the configuration file's default.
/// Any other store failure is [`Approval::Ask`], with a warning: a broken store must never be what
/// turns confirmation off.
pub async fn current(state: &AppState) -> Approval {
    match state.store.ask_before_send().await {
        Ok(Some(ask)) => Approval::from_ask(ask),
        Ok(None) | Err(StoreError::Unavailable(_)) => {
            Approval::from_ask(state.config.conversation.ask_before_send)
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "could not read whether the voice agent must ask before it sends; asking"
            );
            Approval::Ask
        }
    }
}

/// The setting as Settings shows it: the value in force, and whether this server can change it.
///
/// `changeable` is false when no store is configured, and when the configured one cannot be read —
/// a page must not offer a switch whose saved position it cannot know.
pub async fn view(state: &AppState) -> crate::contract::VoiceSettings {
    let (ask_before_send, changeable) = match state.store.ask_before_send().await {
        Ok(Some(ask)) => (ask, true),
        Ok(None) => (state.config.conversation.ask_before_send, true),
        Err(StoreError::Unavailable(_)) => (state.config.conversation.ask_before_send, false),
        Err(error) => {
            tracing::warn!(%error, "could not read the voice settings for Settings; asking");
            (true, false)
        }
    };
    crate::contract::VoiceSettings {
        ask_before_send,
        changeable,
    }
}

/// Record the owner's choice and return the setting as it now stands. **Write scope**, and the
/// owner's act alone: there is no tool for it.
///
/// # Errors
///
/// [`OpError::Store`] when no store is configured or the write fails.
pub async fn set(state: &AppState, ask: bool) -> Result<crate::contract::VoiceSettings, OpError> {
    let stored = state.store.set_ask_before_send(ask).await?;
    // The boolean and nothing else. Settings is the only writer, so this line is the record of the
    // owner changing his mind, and the time on it is when.
    tracing::info!(
        target: "vibe_talk::access",
        event = "changed",
        ask_before_send = stored,
        "voice_settings"
    );
    Ok(crate::contract::VoiceSettings {
        ask_before_send: stored,
        changeable: true,
    })
}

/// What one `post_reply` did.
#[derive(Debug)]
pub enum VoicePost {
    /// Proposed for the owner's confirmation; nothing was posted.
    Proposed {
        /// The channel it would go to.
        channel: ChannelInfo,
        /// The proposal now pending.
        proposal: crate::post_gate::Proposal,
    },
    /// Posted at once.
    Sent {
        /// The channel it went to.
        channel: ChannelInfo,
        /// A number correlating the log line; drawn from the same counter as proposals.
        serial: u64,
        /// Every part as the chat service accepted it, in order; the first is the post itself.
        parts: Vec<Message>,
        /// The exact text posted.
        text: String,
        /// The message it answered, if any.
        reply_to: Option<String>,
    },
    /// Not posted, because the same words to the same place were sent moments ago, or are still
    /// on their way; see [`crate::post_gate::PostGate::begin_direct`].
    Repeat {
        /// The channel's name as the owner knows it, or its id when it is not one this server
        /// lists.
        channel: String,
        /// The earlier send.
        earlier: Repeat,
        /// The words, as asked for again.
        text: String,
    },
}

/// `post_reply`, as the setting says: propose it, or post it now. The one function the MCP tool and
/// `POST /api/v1/post-proposals` both call, so a client that maps the tool onto the REST route gets
/// the same behaviour and hands its model the same words.
///
/// # Errors
///
/// Whatever [`ops::propose_reply`] or [`ops::reply`] refuses with — the allowlist, the write
/// policy, an empty message — and, for a direct send, the chat service's own failure, including
/// [`OpError::PartiallyPosted`] when part of a long message went out.
pub async fn voice_post(
    state: &AppState,
    channel_id: &str,
    text: &str,
    reply_to: Option<&str>,
) -> Result<VoicePost, OpError> {
    match current(state).await {
        Approval::Ask => {
            let (channel, proposal) = ops::propose_reply(state, channel_id, text, reply_to).await?;
            Ok(VoicePost::Proposed { channel, proposal })
        }
        Approval::Direct => {
            let binding = Binding {
                channel_id: channel_id.to_owned(),
                text: text.to_owned(),
                reply_to: reply_to.map(str::to_owned),
            };
            let ticket = match state.post_gate.begin_direct(binding) {
                Ok(ticket) => ticket,
                Err(earlier) => {
                    crate::access::post_gate(
                        "direct_repeat",
                        Some(earlier.serial),
                        Some(channel_id),
                        Some(text.chars().count()),
                        Some(earlier.outcome.as_str()),
                        None,
                    );
                    let channel = ops::channels(state)
                        .await
                        .into_iter()
                        .find(|channel| channel.id.as_str() == channel_id)
                        .map_or_else(
                            || channel_id.to_owned(),
                            |channel| channel.display_name().to_owned(),
                        );
                    return Ok(VoicePost::Repeat {
                        channel,
                        earlier,
                        text: text.to_owned(),
                    });
                }
            };
            let serial = ticket.serial();
            match ops::reply(state, channel_id, text, reply_to).await {
                Ok((channel, _first, parts)) => {
                    ticket.finish(Some(DirectOutcome::Sent));
                    crate::access::post_gate(
                        "sent_direct",
                        Some(serial),
                        Some(channel.id.as_str()),
                        Some(text.chars().count()),
                        None,
                        None,
                    );
                    Ok(VoicePost::Sent {
                        channel,
                        serial,
                        parts,
                        text: text.to_owned(),
                        reply_to: reply_to.map(str::to_owned),
                    })
                }
                Err(error) => {
                    // Remembered when anything may have reached the channel, so the same words
                    // are not posted over it; forgotten when certainly nothing did.
                    ticket.finish(match &error {
                        OpError::PartiallyPosted {
                            posted, uncertain, ..
                        } if *posted > 0 => Some(if *uncertain {
                            DirectOutcome::PartlyUnconfirmed
                        } else {
                            DirectOutcome::Partly
                        }),
                        OpError::PartiallyPosted {
                            uncertain: true, ..
                        } => Some(DirectOutcome::Unconfirmed),
                        _ => None,
                    });
                    crate::access::post_gate(
                        "direct_failed",
                        Some(serial),
                        Some(channel_id),
                        Some(text.chars().count()),
                        Some(error.code()),
                        None,
                    );
                    Err(error)
                }
            }
        }
    }
}

/// What a model is told after a direct send: `SENT.` first, then what went where, in words it can
/// repeat, and that calling again would post again.
///
/// `parts` are the messages posted, in order, and the notice says where they went, read from the
/// parts themselves: a speaker told only that it went as several messages looks in the channel and
/// finds one. Where the provider continued a long post in its first part's thread (`#263
/// post-split-per-provider`) the first is in the channel and the rest in the thread under it; where
/// the first part was itself a reply inside a thread, as a Slack reply is, every part is in that
/// thread.
#[must_use]
pub fn sent_notice(
    channel: &ChannelInfo,
    text: &str,
    reply_to: Option<&str>,
    parts: &[Message],
) -> String {
    let target = match reply_to {
        Some(message) => format!(" as a reply to message {message}"),
        None => String::new(),
    };
    let count = parts.len();
    // A part with no thread, or the one a thread starts from, is in the channel; any other is a
    // reply inside a thread. A direct send goes to the conversation, so a later part inside a thread
    // is in the one its first part started — the rule the page's own receipt for a long post keeps
    // — unless the first part was already inside one: a reply where the provider threads replies,
    // whose every part then went to the answered message's thread.
    let inside = |part: &Message| part.thread.as_ref().is_some_and(|thread| !thread.is_root);
    let split = match parts {
        [] | [_] => String::new(),
        [first, rest @ ..] if inside(first) && rest.iter().all(inside) => {
            let thread = if reply_to.is_some() {
                "the thread of the message it answered"
            } else {
                "one thread"
            };
            format!("\nIt was long, so it went as {count} messages, all of them in {thread}.")
        }
        [first, rest @ ..] if !inside(first) && rest.iter().any(inside) => format!(
            "\nIt was long, so it went as {count} messages: the first in the channel, and the rest \
             in the thread under it."
        ),
        _ => format!("\nIt was long, so it went as {count} messages."),
    };
    format!(
        "SENT. Posted to {channel} (id {id}){target} as the owner's bot.\n\
         Exact text posted: {text}{split}\n\
         Tell the speaker it was sent. Do not call post_reply again for this message; that would \
         post it again.",
        channel = channel.display_name(),
        id = channel.id,
    )
}

/// What a model is told when it asked for the same words to the same place again within
/// [`crate::post_gate::DIRECT_REPEAT_WINDOW`] of an earlier direct send, or while it was still on
/// its way: nothing was posted this time, and what is known about the earlier one, first.
#[must_use]
pub fn repeat_notice(channel: &str, earlier: &Repeat, text: &str) -> String {
    let ago = earlier.age.as_secs();
    let left = earlier.left.as_secs().max(1);
    match earlier.outcome {
        DirectOutcome::Sent => format!(
            "SENT already. The same words went to {channel} {ago} seconds ago, so this call posted \
             nothing more.\n\
             Exact text posted: {text}\n\
             Tell the speaker it was sent. Do not call post_reply again for this message. If the \
             speaker really wants the same words posted a second time, they can ask again in \
             {left} seconds."
        ),
        DirectOutcome::Partly => format!(
            "PARTLY SENT already. Part of the same words reached {channel} {ago} seconds ago and \
             the rest did not, so this call posted nothing: sending the whole message again would \
             repeat the part that went. Tell the speaker that only part was posted and that they \
             can check the channel. Do not call post_reply again for this message."
        ),
        DirectOutcome::PartlyUnconfirmed => format!(
            "PARTLY SENT already. Part of the same words reached {channel} {ago} seconds ago, and \
             whether the rest was posted could not be confirmed, so this call posted nothing: \
             sending the whole message again would repeat what went. Tell the speaker that at \
             least part was posted and to check the channel. Do not call post_reply again for this \
             message."
        ),
        DirectOutcome::Unconfirmed => format!(
            "NOT CONFIRMED. The same words were sent to {channel} {ago} seconds ago and this \
             server could not tell whether they were posted, so this call posted nothing, rather \
             than risk posting them twice. Tell the speaker to check the channel. If they are not \
             there, the speaker can ask again in {left} seconds."
        ),
        DirectOutcome::InFlight => format!(
            "NOT CONFIRMED. The same words to {channel} are still being posted by an earlier \
             call, so this call posted nothing more. Tell the speaker it is still going out and to \
             check the channel. Do not call post_reply again for this message."
        ),
    }
}

/// What goes in front of a `post_reply` failure, so the first word the model reads says whether
/// anything reached the channel. True in both modes: a refused proposal sends nothing either.
///
/// A failure whose answer was lost — a timeout, a dropped connection, a gateway's 5xx, a success
/// whose body did not parse — may have posted, so it is `NOT CONFIRMED`, never `NOT SENT`: a model
/// told "not sent" says so, and the owner's "try again" would post it twice.
#[must_use]
pub fn failure_prefix(error: &OpError) -> &'static str {
    match error {
        OpError::PartiallyPosted {
            posted,
            uncertain: false,
            ..
        } if *posted > 0 => {
            "PARTLY SENT. The rest was NOT sent; do not send the whole message again. "
        }
        OpError::PartiallyPosted { posted, .. } if *posted > 0 => {
            "PARTLY SENT. Whether the rest was posted could not be confirmed; do not send the \
             whole message again, and tell the speaker to check the channel. "
        }
        OpError::PartiallyPosted {
            uncertain: true, ..
        } => {
            "NOT CONFIRMED. This server could not tell whether it was posted, so do not call \
             post_reply again for these words; tell the speaker to check the channel. "
        }
        _ => "NOT SENT. ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StateStore as _;
    use crate::testing::{self, WRITE_CHANNEL};
    use std::sync::Arc;

    fn direct_toml() -> String {
        format!(
            "{}\n[conversation]\nask_before_send = false\n",
            testing::config_toml()
        )
    }

    #[tokio::test]
    async fn with_nothing_chosen_the_shipped_default_asks() {
        let (state, _discord, _store) = testing::state_with_store();
        assert_eq!(current(&state).await, Approval::Ask);
        let shown = view(&state).await;
        assert!(shown.ask_before_send && shown.changeable);
    }

    #[tokio::test]
    async fn the_configured_default_applies_until_the_owner_chooses_and_his_choice_wins() {
        let (state, _discord, _store) = testing::state_with_store_from_toml(&direct_toml());
        assert_eq!(current(&state).await, Approval::Direct);
        set(&state, true).await.expect("save");
        assert_eq!(current(&state).await, Approval::Ask);
        set(&state, false).await.expect("save");
        assert_eq!(current(&state).await, Approval::Direct);
    }

    #[tokio::test]
    async fn no_store_means_the_configured_default_and_a_switch_that_cannot_be_moved() {
        let (state, _discord) =
            testing::state_with(Arc::new(crate::store::disabled::DisabledStore));
        assert_eq!(current(&state).await, Approval::Ask);
        let shown = view(&state).await;
        assert!(shown.ask_before_send);
        assert!(!shown.changeable);
        assert_eq!(
            set(&state, false)
                .await
                .expect_err("nowhere to keep it")
                .code(),
            "storage_not_configured"
        );
        assert_eq!(current(&state).await, Approval::Ask);
    }

    #[tokio::test]
    async fn a_store_that_fails_to_answer_asks_even_when_the_default_does_not() {
        let (state, _discord, store) = testing::state_with_store_from_toml(&direct_toml());
        store.set_ask_before_send(false).await.expect("save");
        store.fail_next("the disk is gone");
        assert_eq!(
            current(&state).await,
            Approval::Ask,
            "a broken store turned confirmation off"
        );
        store.fail_next("the disk is gone");
        let shown = view(&state).await;
        assert!(shown.ask_before_send && !shown.changeable);
    }

    #[tokio::test]
    async fn asking_proposes_and_posts_nothing() {
        let (state, discord, _store) = testing::state_with_store();
        let outcome = voice_post(&state, WRITE_CHANNEL, "on it", None)
            .await
            .expect("proposed");
        assert!(matches!(outcome, VoicePost::Proposed { .. }), "{outcome:?}");
        assert!(discord.posted().is_empty(), "asking posted something");
        assert!(state.post_gate.pending().is_some());
    }

    #[tokio::test]
    async fn direct_posts_once_through_the_shared_send_and_leaves_nothing_pending() {
        let (state, discord, _store) = testing::state_with_store_from_toml(&direct_toml());
        let outcome = voice_post(&state, WRITE_CHANNEL, "rebase onto main", Some("42"))
            .await
            .expect("sent");
        let VoicePost::Sent {
            channel,
            parts,
            text,
            reply_to,
            ..
        } = outcome
        else {
            panic!("a direct send proposed instead: {outcome:?}");
        };
        assert_eq!(discord.posted().len(), 1);
        assert!(state.post_gate.pending().is_none());
        let said = sent_notice(&channel, &text, reply_to.as_deref(), &parts);
        assert!(
            said.starts_with("SENT. Posted to lead team (id 2222222222) as a reply to message 42"),
            "{said}"
        );
        assert!(
            said.contains("Exact text posted: rebase onto main"),
            "{said}"
        );
        assert!(!said.contains("went as"), "{said}");
    }

    /// One posted part per entry of `inside`, each where it says: `None` in the channel with no
    /// thread, `Some(true)` the root of thread `t1`, `Some(false)` a reply inside it.
    async fn parts(inside: &[Option<bool>]) -> Vec<Message> {
        use crate::chat::ChatClient as _;
        let fake = crate::discord::fake::FakeDiscord::new();
        let channel = crate::model::ChannelId(WRITE_CHANNEL.to_owned());
        fake.seed(&channel, "bot", "a part");
        let one = fake.fetch_recent(&channel, 1).await.expect("seeded")[0].clone();
        inside
            .iter()
            .map(|inside| Message {
                thread: inside.map(|is_root| crate::threads::MessageThread {
                    id: "t1".to_owned(),
                    root_message_id: None,
                    is_root,
                    reply_count: None,
                    reply_count_exact: false,
                }),
                ..one.clone()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_long_direct_send_is_said_to_have_gone_where_its_parts_went() {
        let (state, _discord, _store) = testing::state_with_store();
        let channel = ops::channels(&state)
            .await
            .into_iter()
            .find(|channel| channel.id.as_str() == WRITE_CHANNEL)
            .expect("listed");
        let said = |reply_to: Option<&str>, parts: &[Message]| {
            sent_notice(&channel, "rebase onto main", reply_to, parts)
        };

        // Continued in the first part's thread: the first in the channel, the rest under it.
        let continued = said(None, &parts(&[Some(true), Some(false), Some(false)]).await);
        assert!(
            continued.contains(
                "It was long, so it went as 3 messages: the first in the channel, and the rest \
                 in the thread under it."
            ),
            "{continued}"
        );
        // The same, from a provider that did not mark the first part as a thread's root.
        let unmarked = said(None, &parts(&[None, Some(false)]).await);
        assert!(unmarked.contains("the first in the channel"), "{unmarked}");

        // A reply where the provider threads replies, as Slack does: every part, the first too, is
        // in the answered message's thread, and none of it is in the channel.
        let reply = said(
            Some("42"),
            &parts(&[Some(false), Some(false), Some(false)]).await,
        );
        assert!(
            reply.contains(
                "It was long, so it went as 3 messages, all of them in the thread of the message \
                 it answered."
            ),
            "{reply}"
        );
        assert!(!reply.contains("in the channel"), "{reply}");

        // One after another in the channel, as Discord posts them.
        let consecutive = said(Some("42"), &parts(&[None, None]).await);
        assert!(
            consecutive.contains("It was long, so it went as 2 messages.\n"),
            "{consecutive}"
        );
        assert!(!consecutive.contains("thread"), "{consecutive}");
    }

    fn partly(posted: usize, uncertain: bool) -> OpError {
        OpError::PartiallyPosted {
            posted,
            unsent: "the rest".to_owned(),
            cause: "down".to_owned(),
            retryable: true,
            resumable: true,
            thread_id: None,
            uncertain,
        }
    }

    #[test]
    fn a_failure_says_first_whether_anything_reached_the_channel() {
        assert_eq!(failure_prefix(&OpError::UnknownChannel), "NOT SENT. ");
        assert_eq!(failure_prefix(&partly(0, false)), "NOT SENT. ");
        assert_eq!(
            failure_prefix(&partly(1, false)),
            "PARTLY SENT. The rest was NOT sent; do not send the whole message again. "
        );
        let unknown_rest = failure_prefix(&partly(1, true));
        assert!(unknown_rest.starts_with("PARTLY SENT."), "{unknown_rest}");
        assert!(
            !unknown_rest.contains("NOT sent"),
            "a part that may have posted was called not sent: {unknown_rest}"
        );
    }

    #[test]
    fn a_failure_whose_answer_was_lost_is_not_confirmed_rather_than_not_sent() {
        // A timeout after the chat service took the post is the case: "not sent" would have the
        // owner say "try again", and post it twice.
        let said = failure_prefix(&partly(0, true));
        assert!(said.starts_with("NOT CONFIRMED. "), "{said}");
        assert!(
            said.contains("do not call post_reply again for these words"),
            "{said}"
        );
        assert!(!said.contains("NOT SENT"), "{said}");
    }

    #[test]
    fn a_repeat_says_first_what_is_known_about_the_earlier_send_and_that_nothing_more_went() {
        let earlier = |outcome| Repeat {
            serial: 7,
            outcome,
            age: std::time::Duration::from_secs(12),
            left: std::time::Duration::from_secs(48),
        };
        for (outcome, first) in [
            (DirectOutcome::Sent, "SENT already. "),
            (DirectOutcome::Partly, "PARTLY SENT already. "),
            (DirectOutcome::PartlyUnconfirmed, "PARTLY SENT already. "),
            (DirectOutcome::Unconfirmed, "NOT CONFIRMED. "),
            (DirectOutcome::InFlight, "NOT CONFIRMED. "),
        ] {
            let said = repeat_notice("lead team", &earlier(outcome), "rebase onto main");
            assert!(said.starts_with(first), "{outcome:?}: {said}");
            assert!(said.contains("lead team"), "{outcome:?}: {said}");
            assert!(
                said.contains("posted nothing"),
                "{outcome:?} did not say this call posted nothing: {said}"
            );
        }
        let sent = repeat_notice(
            "lead team",
            &earlier(DirectOutcome::Sent),
            "rebase onto main",
        );
        assert!(sent.contains("12 seconds ago"), "{sent}");
        assert!(
            sent.contains("Exact text posted: rebase onto main"),
            "{sent}"
        );
        assert!(sent.contains("ask again in 48 seconds"), "{sent}");

        // As the first send's own failure did: a part whose answer was lost may have posted, so
        // the rest is not said to have failed.
        let partly = repeat_notice(
            "lead team",
            &earlier(DirectOutcome::Partly),
            "rebase onto main",
        );
        assert!(partly.contains("the rest did not"), "{partly}");
        let unknown_rest = repeat_notice(
            "lead team",
            &earlier(DirectOutcome::PartlyUnconfirmed),
            "rebase onto main",
        );
        assert!(
            unknown_rest.contains("whether the rest was posted could not be confirmed"),
            "{unknown_rest}"
        );
        assert!(
            !unknown_rest.contains("did not") && !unknown_rest.contains("only part"),
            "a rest that may have posted was called not posted: {unknown_rest}"
        );
    }
}
