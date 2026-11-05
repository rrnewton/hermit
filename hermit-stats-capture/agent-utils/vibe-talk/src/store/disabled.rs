//! The store a server runs with when no storage is configured: one that refuses.
//!
//! There are three things a server could do when `storage.path` is unset, and two of them are
//! wrong. It could keep state in memory, which looks like it works and loses everything on the
//! next restart — the worst outcome, because the loss is invisible until it matters. It could
//! pick a path itself, which puts the owner's speech somewhere he did not choose, quite possibly
//! inside a container image that is rebuilt weekly. Or it can refuse, loudly and specifically,
//! naming the setting to add.
//!
//! It refuses. That is the same posture [`crate::elevenlabs::credentials`] takes toward a missing
//! API key, for the same reason: an unconfigured feature must be distinguishable from a broken
//! one.

use async_trait::async_trait;

use super::{
    Bookmark, BookmarkAnchor, BookmarkChange, CachedSummary, ChannelAlias, ConversationId,
    ConversationSummary, PinChange, PinSnapshot, Pins, ReadMark, StateStore, StoreError,
    SummaryKey, TranscriptCursor, TranscriptPage, Turn, UpstreamClearAttempt, UpstreamClearMode,
    UpstreamClearSetting,
};
use crate::model::{ChannelId, MessageId};
use crate::voice_agent::OwnerPrompt;

/// The setting whose absence installs [`DisabledStore`].
pub const SETTING: &str = "storage.path";

/// A store that holds nothing and says so.
#[derive(Clone, Copy, Debug, Default)]
pub struct DisabledStore;

fn refuse<T>() -> Result<T, StoreError> {
    Err(StoreError::Unavailable(SETTING))
}

#[async_trait]
impl StateStore for DisabledStore {
    fn describe(&self) -> String {
        format!("no durable state ({SETTING} is not configured)")
    }

    async fn check_writable(&self) -> Result<String, StoreError> {
        refuse()
    }

    async fn append_turn(&self, _: &ConversationId, _: &Turn) -> Result<(), StoreError> {
        refuse()
    }

    async fn conversations(&self) -> Result<Vec<ConversationSummary>, StoreError> {
        refuse()
    }

    async fn turns(&self, _: &ConversationId) -> Result<Vec<Turn>, StoreError> {
        refuse()
    }

    async fn transcript(
        &self,
        _: u16,
        _: Option<&TranscriptCursor>,
    ) -> Result<TranscriptPage, StoreError> {
        // A refusal rather than an empty page, which is the whole posture of this module: "you
        // have said nothing yet" and "this server keeps no record of what you say" are different
        // facts, and a screen that cannot tell them apart will quietly claim the first.
        refuse()
    }

    async fn forget_conversation(&self, _: &ConversationId) -> Result<(), StoreError> {
        refuse()
    }

    async fn forget_all_conversations(&self) -> Result<u64, StoreError> {
        refuse()
    }

    async fn read_mark(&self, _: &ChannelId) -> Result<Option<ReadMark>, StoreError> {
        refuse()
    }

    async fn read_marks(&self) -> Result<Vec<ReadMark>, StoreError> {
        refuse()
    }

    async fn mark_read(&self, _: &ChannelId, _: &MessageId) -> Result<ReadMark, StoreError> {
        refuse()
    }

    async fn forget_read_mark(&self, _: &ChannelId) -> Result<(), StoreError> {
        refuse()
    }

    async fn channel_aliases(&self) -> Result<Vec<ChannelAlias>, StoreError> {
        refuse()
    }

    async fn added_channels(&self) -> Result<Vec<crate::store::AddedChannel>, StoreError> {
        refuse()
    }

    async fn add_channel(
        &self,
        _: &ChannelId,
        _: &str,
        _: bool,
        _: Option<&str>,
        _: i64,
    ) -> Result<(), StoreError> {
        refuse()
    }

    async fn remove_added_channel(&self, _: &ChannelId) -> Result<(), StoreError> {
        refuse()
    }

    async fn hidden_channels(&self) -> Result<Vec<ChannelId>, StoreError> {
        refuse()
    }

    async fn hide_channel(&self, _: &ChannelId, _: i64) -> Result<(), StoreError> {
        refuse()
    }

    async fn unhide_channel(&self, _: &ChannelId) -> Result<(), StoreError> {
        refuse()
    }

    async fn set_channel_alias(&self, _: &ChannelId, _: &str) -> Result<ChannelAlias, StoreError> {
        refuse()
    }

    async fn clear_channel_alias(&self, _: &ChannelId) -> Result<(), StoreError> {
        refuse()
    }

    async fn dismissals(&self, _: &ChannelId) -> Result<Vec<MessageId>, StoreError> {
        refuse()
    }

    async fn dismiss(&self, _: &ChannelId, _: &[MessageId]) -> Result<u64, StoreError> {
        refuse()
    }

    async fn restore(&self, _: &ChannelId, _: &[MessageId]) -> Result<u64, StoreError> {
        refuse()
    }

    async fn noise_rules(&self) -> Result<Option<Vec<String>>, StoreError> {
        // A refusal, not `Ok(None)`. The caller that wants the defaults when nothing is stored
        // (`noise::current_rules`) asks for them by name; answering "never saved" here would let
        // a settings screen believe it could save.
        refuse()
    }

    async fn set_noise_rules(&self, _: &[String]) -> Result<Vec<String>, StoreError> {
        refuse()
    }

    async fn ask_before_send(&self) -> Result<Option<bool>, StoreError> {
        // A refusal, not `Ok(None)`, for the noise rules' reason: Settings must not believe it
        // could save a choice. `send_approval::current` applies the configured default by name.
        refuse()
    }

    async fn set_ask_before_send(&self, _: bool) -> Result<bool, StoreError> {
        refuse()
    }

    async fn owner_prompt(&self) -> Result<Option<OwnerPrompt>, StoreError> {
        // A refusal, not `Ok(None)`, for the noise rules' reason: Settings must not believe it
        // could save. `voice_prompt::current` applies the shipped prompt by name.
        refuse()
    }

    async fn set_owner_prompt(&self, _: &OwnerPrompt) -> Result<OwnerPrompt, StoreError> {
        refuse()
    }

    async fn noise_exemptions(&self, _: &ChannelId) -> Result<Vec<MessageId>, StoreError> {
        refuse()
    }

    async fn exempt_from_noise(&self, _: &ChannelId, _: &[MessageId]) -> Result<u64, StoreError> {
        refuse()
    }

    async fn pins(&self, _: &ChannelId) -> Result<Pins, StoreError> {
        // A refusal, not an empty list: "nothing is pinned" and "this server cannot keep a pin"
        // are different facts, and a page told the first would offer a Pin that cannot work.
        refuse()
    }

    async fn pins_revision(&self, _: &ChannelId) -> Result<i64, StoreError> {
        refuse()
    }

    async fn pin(&self, _: &ChannelId, _: &PinSnapshot) -> Result<PinChange, StoreError> {
        refuse()
    }

    async fn unpin(&self, _: &ChannelId, _: &MessageId) -> Result<PinChange, StoreError> {
        refuse()
    }

    async fn bookmark(&self, _: &ChannelId) -> Result<Bookmark, StoreError> {
        // A refusal, not "no bookmark", for the pins' reason: a page told there is none would offer
        // to set one that cannot be kept. `#240 reading-bookmark`.
        refuse()
    }

    async fn set_bookmark(
        &self,
        _: &ChannelId,
        _: Option<&BookmarkAnchor>,
        _: Option<i64>,
    ) -> Result<BookmarkChange, StoreError> {
        refuse()
    }

    async fn cached_summary(&self, _: &SummaryKey) -> Result<Option<CachedSummary>, StoreError> {
        refuse()
    }

    async fn cache_summary(
        &self,
        _: &SummaryKey,
        _: &str,
        _: Option<u32>,
    ) -> Result<(), StoreError> {
        refuse()
    }

    async fn touch_summary(&self, _: &SummaryKey) -> Result<bool, StoreError> {
        refuse()
    }

    async fn rekey_summaries(&self, _: &str, _: &str, _: u32) -> Result<u64, StoreError> {
        refuse()
    }

    async fn forget_channel_summaries(&self, _: &ChannelId) -> Result<u64, StoreError> {
        refuse()
    }

    async fn forget_summaries_except(&self, _: &str) -> Result<u64, StoreError> {
        refuse()
    }

    async fn upstream_clear_settings(&self) -> Result<Vec<UpstreamClearSetting>, StoreError> {
        // A refusal, not an empty list: Settings must not offer a switch this server cannot keep,
        // and the server cannot act on a setting it would forget at the next restart.
        refuse()
    }

    async fn set_upstream_clear_mode(
        &self,
        _: &ChannelId,
        _: UpstreamClearMode,
        _: i64,
    ) -> Result<UpstreamClearSetting, StoreError> {
        refuse()
    }

    async fn record_upstream_clear(
        &self,
        _: &ChannelId,
        _: &UpstreamClearAttempt,
    ) -> Result<(), StoreError> {
        refuse()
    }

    async fn purge_everything(&self) -> Result<(), StoreError> {
        refuse()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Speaker;
    use super::*;

    #[tokio::test]
    async fn every_operation_refuses_and_names_the_setting_to_add() {
        let store = DisabledStore;
        let id = ConversationId::parse("conv").expect("valid");
        let channel = ChannelId("1111111111".to_owned());
        let message = MessageId("1000000000000000200".to_owned());

        let errors: Vec<StoreError> = vec![
            store
                .append_turn(&id, &Turn::now(Speaker::You, "hi"))
                .await
                .expect_err("append"),
            store.conversations().await.expect_err("list"),
            store.turns(&id).await.expect_err("turns"),
            store.transcript(10, None).await.expect_err("transcript"),
            store.forget_conversation(&id).await.expect_err("forget"),
            store
                .forget_all_conversations()
                .await
                .expect_err("forget all"),
            store.read_mark(&channel).await.expect_err("mark"),
            store.read_marks().await.expect_err("marks"),
            store
                .mark_read(&channel, &message)
                .await
                .expect_err("mark read"),
            store.forget_read_mark(&channel).await.expect_err("unmark"),
            store.channel_aliases().await.expect_err("aliases"),
            store
                .set_channel_alias(&channel, "the build channel")
                .await
                .expect_err("set alias"),
            store
                .clear_channel_alias(&channel)
                .await
                .expect_err("clear alias"),
            store.dismissals(&channel).await.expect_err("dismissals"),
            store
                .dismiss(&channel, std::slice::from_ref(&message))
                .await
                .expect_err("dismiss"),
            store
                .restore(&channel, std::slice::from_ref(&message))
                .await
                .expect_err("restore"),
            store.noise_rules().await.expect_err("noise rules"),
            store
                .set_noise_rules(&["Working…".to_owned()])
                .await
                .expect_err("set noise rules"),
            store
                .noise_exemptions(&channel)
                .await
                .expect_err("noise exemptions"),
            store
                .exempt_from_noise(&channel, std::slice::from_ref(&message))
                .await
                .expect_err("exempt from noise"),
            store.pins(&channel).await.expect_err("pins"),
            store
                .pins_revision(&channel)
                .await
                .expect_err("pins revision"),
            store
                .pin(
                    &channel,
                    &super::super::PinSnapshot {
                        message_id: message.clone(),
                        author: "someone".to_owned(),
                        author_id: crate::model::UserId("7".to_owned()),
                        author_is_bot: false,
                        content: "hello".to_owned(),
                        truncated: false,
                        timestamp: "2026-10-04T12:00:00Z".to_owned(),
                        thread_id: None,
                        thread_root: false,
                    },
                )
                .await
                .expect_err("pin"),
            store.unpin(&channel, &message).await.expect_err("unpin"),
            store.bookmark(&channel).await.expect_err("bookmark"),
            store
                .set_bookmark(
                    &channel,
                    Some(&super::super::BookmarkAnchor {
                        message_id: message.clone(),
                        place: super::super::BookmarkPlace::Above,
                        timestamp: "2026-10-04T12:00:00Z".to_owned(),
                        thread_id: None,
                    }),
                    None,
                )
                .await
                .expect_err("set bookmark"),
            store
                .set_bookmark(&channel, None, Some(0))
                .await
                .expect_err("clear bookmark"),
            store
                .upstream_clear_settings()
                .await
                .expect_err("upstream clear settings"),
            store
                .set_upstream_clear_mode(&channel, UpstreamClearMode::On, 1)
                .await
                .expect_err("set upstream clear mode"),
            store
                .record_upstream_clear(
                    &channel,
                    &UpstreamClearAttempt {
                        at_ms: 1,
                        message: message.clone(),
                        message_ms: 1,
                        result: super::super::UpstreamClearResult::Advanced,
                        target: None,
                        previous: None,
                        error: None,
                    },
                )
                .await
                .expect_err("record upstream clear"),
            store.purge_everything().await.expect_err("purge"),
        ];
        for error in errors {
            assert_eq!(error.code(), "storage_not_configured", "{error}");
            assert!(
                error.to_string().contains(SETTING),
                "the operator must be told which setting to add: {error}"
            );
        }
    }
}
