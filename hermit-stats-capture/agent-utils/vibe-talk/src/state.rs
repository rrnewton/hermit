//! Shared server state, and the two policy decisions every handler goes through.

use std::sync::Arc;

use crate::agent_backend::AgentBackend;
use crate::chat::ChatClient;
use crate::config::Config;
use crate::conversation::ConversationalVoiceProvider;
use crate::elevenlabs::SignedUrlProvider;
use crate::live::LiveHub;
use crate::model::{ChannelId, ChannelInfo};
use crate::providers::ChatRouter;
use crate::retrieval::Ranker;
use crate::speech::SpeechProvider;
use crate::store::{AddedChannel, StateStore};
use crate::summarize::Summarizer;

/// Everything a request handler needs.
#[derive(Clone)]
pub struct AppState {
    /// Loaded configuration.
    pub config: Arc<Config>,
    /// Access to the configured chat providers, as one client that routes by channel.
    pub chat: Arc<dyn ChatClient>,
    /// The same providers, for the questions only the router can answer: which provider owns a
    /// channel, each provider's own capabilities, and the namespace a registration is recorded
    /// under. [`AppState::chat`] is this router seen through the provider-neutral trait.
    pub providers: Arc<ChatRouter>,
    /// Strategy for semantic random access.
    pub ranker: Arc<dyn Ranker>,
    /// Mints short-lived signed conversation URLs for the configured ElevenLabs agent.
    pub elevenlabs: Arc<dyn SignedUrlProvider>,
    /// Opens a browser-ready conversational voice session through the selected provider.
    pub conversation: Arc<dyn ConversationalVoiceProvider>,
    /// Reads messages using the configured speech backend and advertises its playback interface.
    ///
    /// Separate from minting a conversation URL: device speech needs no account, and a server
    /// audio provider can read aloud without having a conversational agent.
    pub speech: Arc<dyn SpeechProvider>,
    /// Slow-path backend (absent in v0).
    pub agent: Arc<dyn AgentBackend>,
    /// Turns one long channel message into one short line.
    ///
    /// The shipped implementation asks the deployment's ElevenLabs conversational agent, so on a
    /// server with no credentials every call fails by name and the page draws those rows as
    /// failed. Behind a trait for the same reason the store is: a second backend must be
    /// substitutable without a call site changing, and the tests need one that counts.
    /// [`crate::summarize::Summarizer::describe`] is reported to the caller so a page can never
    /// imply a summary it did not get.
    pub summarizer: Arc<dyn Summarizer>,
    /// The policy every cached summary is filed under. Computed once at startup from
    /// [`crate::summarize::policy_version`], because recomputing it per request is how the two
    /// halves of a cache key drift apart.
    pub summary_version: Arc<str>,
    /// How the summary route spent its requests, numbers only, for diagnostics.
    /// `#208 persistent-cache`; see [`crate::summarize::counters`].
    pub summary_counters: Arc<crate::summarize::counters::SummaryCounters>,
    /// Thread display names and sentences, and the background queue that produces them.
    /// `#182 thread-display-names`, `#233 llm-summarizer`. See [`crate::summarize::threads`].
    pub thread_names: Arc<crate::summarize::threads::ThreadNames>,
    /// The one place this server keeps anything between restarts.
    ///
    /// Reached only through the trait, never as a concrete database, so the backend can be
    /// replaced without touching a handler. When nothing is configured this is
    /// [`crate::store::disabled::DisabledStore`], which refuses every call and names the setting
    /// to add — it is deliberately not a silent in-memory substitute. See [`crate::store`].
    pub store: Arc<dyn StateStore>,
    /// The live fan-out: where inbound Discord messages are published, and where the SSE route
    /// subscribes.
    ///
    /// Always present, even when ingestion is off. An absent hub would make the stream route
    /// conditional on configuration, and a route that 404s for a reason unrelated to the channel
    /// allowlist is the kind of ambiguity this server spends effort avoiding. With ingestion off
    /// nobody publishes, so a subscriber simply waits — and the startup banner says which it is.
    pub live: Arc<LiveHub>,
    /// Messages resolved for server audio BEFORE the reader taps one.
    ///
    /// With server audio, turning read-aloud on prepares everything on screen and a tap then plays
    /// a ticket. Device speech reads the text already in the browser and uses no tickets.
    /// See [`crate::speech_tickets`] for why server audio uses this authority boundary.
    pub speech_tickets: Arc<crate::speech_tickets::SpeechTickets>,
    /// Messages fetched in the last thirty seconds, so preparing them for speech does not fetch
    /// them again, and the provider warm-up that preparation starts without waiting for it.
    /// `#191 read-aloud-first-lag`; see [`crate::read_ahead`].
    pub read_ahead: Arc<crate::read_ahead::ReadAhead>,
    /// The letters standing in for the long ids and hashes in channel text.
    ///
    /// Here rather than per request because the point of a letter is that it means the same account
    /// in the next message as it did in this one, and a table rebuilt per request cannot promise
    /// that. Here rather than per reader because this server has ONE channel allowlist and every
    /// reader reads through it, so a letter never names a value some reader could not already see
    /// on their own screen. [`crate::speakable::Names`] states the rule that makes that sound.
    ///
    /// It does not survive a restart, and nothing about it needs to: a letter is only ever
    /// interpreted from inside a conversation that is also gone.
    pub spoken_names: Arc<crate::speakable::SharedNames>,
    /// Channels the owner added from inside the app, joined onto the configured allowlist.
    ///
    /// Held in memory as well as in the store because the allowlist is consulted on every
    /// channel-scoped request, including from [`AppState::channel`], which is synchronous by
    /// design. Loaded once at startup and kept in step by the add and remove routes; the store is
    /// what makes it survive a restart, not what is read on the hot path.
    pub added_channels: Arc<std::sync::RwLock<Vec<AddedChannel>>>,
    /// Configured channels the owner took off his list. `#199 removable-config-channels`.
    ///
    /// Subtracted from the configured allowlist by [`AppState::channel`] and
    /// [`AppState::all_channels`], so every listing, every channel-scoped route, the live poller,
    /// diagnostics and the MCP tools stop seeing one at once rather than each having to remember
    /// to filter. In memory beside the store for the reason [`AppState::added_channels`] is.
    pub hidden_channels: Arc<std::sync::RwLock<std::collections::BTreeSet<ChannelId>>>,
    /// Serializes managed register/probe/persist and unregister/remove transactions.
    ///
    /// The provider and local store cannot share a transaction. Holding this lock across both
    /// sides ensures concurrent requests cannot observe the same channel as absent and then undo
    /// one another's upstream registration during compensation.
    pub channel_registration_lock: Arc<tokio::sync::Mutex<()>>,
    /// The one post a voice agent has proposed and nobody has confirmed yet. `#34
    /// voice-chat-write-confirm`: over MCP, `post_reply` records a proposal here and posts nothing;
    /// only the commit route, which no tool reaches, can spend it. See [`crate::post_gate`].
    pub post_gate: Arc<crate::post_gate::PostGate>,
    /// One post at a time to each destination, in the order the posts arrived. `#277 send-order`;
    /// see [`crate::post_lanes`]. Every post goes through [`crate::ops::reply_scoped`], which takes
    /// its destination's turn here.
    pub post_lanes: Arc<crate::post_lanes::PostLanes>,
    /// Caps the lines `POST /api/v1/voice-health` writes, across every caller. Per process rather
    /// than per call because the server has no call id to count by, by design.
    pub voice_health_budget: Arc<crate::voice_health::LogBudget>,
    /// The same cap for `POST /api/v1/voice-end`, kept apart so a flood of one record cannot
    /// silence the other. `#268 voice-call-drops`.
    pub voice_end_budget: Arc<crate::voice_health::LogBudget>,
    /// The paren tags each author has started messages with, per channel: what confirms a
    /// `(tag)` heading. `#242 rich-headers`; see [`crate::heading::HeadingTags`]. In memory and
    /// bounded, shared by every read and the live stream.
    pub headings: Arc<crate::heading::HeadingTags>,
    /// What each channel's recent-view shadow has counted. `#208 persistent-cache`; see
    /// [`crate::recent_view`]. In memory, counts only, empty unless a provider turns it on.
    pub recent_view_shadow: Arc<crate::recent_view::Shadow>,
    /// What the hosted voice agent's configuration last said about a client replacing its prompt.
    /// `#272 voice-agent-instructions`; see [`crate::voice_prompt::OverrideAnswers`]. In memory and
    /// short-lived, so a call's start does not wait on the vendor every time.
    pub agent_override: Arc<crate::voice_prompt::OverrideAnswers>,
}

fn added_channel_info(row: &AddedChannel, providers: &ChatRouter) -> ChannelInfo {
    // A registration recorded under a namespace no configured provider has belongs to a bridge
    // this deployment no longer runs. It stays listed and readable, but nothing may be posted
    // there until that bridge is configured again.
    let provider_matches = row
        .registration_provider
        .as_deref()
        .is_none_or(|namespace| providers.knows_namespace(namespace));
    ChannelInfo {
        id: row.channel.clone(),
        label: row.label.clone(),
        writable: row.writable && provider_matches,
        alias: None,
        added: true,
        provider: providers
            .key_for_namespace(row.registration_provider.as_deref())
            .map(str::to_owned),
    }
}

impl AppState {
    /// Restore channels previously added from inside the app into the live allowlist.
    ///
    /// The store is read once at startup. Keeping this operation on [`AppState`] makes the same
    /// restored set visible to request handlers, diagnostics, and the live poller instead of
    /// requiring each consumer to rebuild its own copy.
    pub async fn restore_added_channels(&self) -> Result<usize, crate::store::StoreError> {
        let restored = self.store.added_channels().await?;
        let count = restored.len();
        {
            let mut slot = self.added_channels.write().map_err(|_| {
                crate::store::StoreError::Backend(
                    "the in-memory added-channel allowlist lock is poisoned".to_owned(),
                )
            })?;
            *slot = restored;
        }
        self.refresh_routes();
        Ok(count)
    }

    /// Restore which configured channels the owner took off his list.
    ///
    /// Read once at startup, beside [`AppState::restore_added_channels`] and for the same reason:
    /// the allowlist is consulted synchronously on every request.
    pub async fn restore_hidden_channels(&self) -> Result<usize, crate::store::StoreError> {
        let restored = self.store.hidden_channels().await?;
        let count = restored.len();
        {
            let mut slot = self.hidden_channels.write().map_err(|_| {
                crate::store::StoreError::Backend(
                    "the in-memory hidden-channel list lock is poisoned".to_owned(),
                )
            })?;
            *slot = restored.into_iter().collect();
        }
        self.refresh_routes();
        Ok(count)
    }

    /// Whether the owner took this configured channel off his list.
    fn is_hidden(&self, id: &ChannelId) -> bool {
        self.hidden_channels
            .read()
            .is_ok_and(|hidden| hidden.contains(id))
    }

    /// The configured channel named `id`, hidden or not, or `None` when the file does not name it.
    ///
    /// For the two routes that act on a configured channel BECAUSE it is configured — taking one
    /// off the list and putting it back. Everything else asks [`AppState::channel`], which does
    /// not answer for a hidden one.
    #[must_use]
    pub fn configured_channel(&self, id: &str) -> Option<ChannelInfo> {
        self.config
            .channels
            .iter()
            .find(|c| c.id.as_str() == id)
            .cloned()
    }

    /// The configured channels the owner took off his list, in configuration order.
    ///
    /// A hidden id the file no longer names is left out: there is nothing to show again.
    #[must_use]
    pub fn hidden_configured_channels(&self) -> Vec<ChannelInfo> {
        self.config
            .channels
            .iter()
            .filter(|c| self.is_hidden(&c.id))
            .cloned()
            .collect()
    }

    /// The configured channels still on the list, in configuration order.
    #[must_use]
    pub fn listed_configured_channels(&self) -> Vec<ChannelInfo> {
        self.config
            .channels
            .iter()
            .filter(|c| !self.is_hidden(&c.id))
            .cloned()
            .collect()
    }

    /// Serve every channel through `client` alone, as a single-provider deployment would.
    ///
    /// For tests and tools that substitute the chat backend: [`AppState::chat`] and
    /// [`AppState::providers`] must always describe the same providers, so they are replaced
    /// together, keeping the first configured provider's key, namespace, and poll interval.
    pub fn replace_chat(&mut self, client: Arc<dyn ChatClient>) {
        let first = &self.config.providers[0];
        let router = Arc::new(ChatRouter::single(
            client,
            &first.namespace(),
            first.live_poll_seconds,
        ));
        self.replace_providers(router);
    }

    /// Serve channels through `providers`, routed by the allowlist this state already holds.
    ///
    /// For tests and tools that assemble their own provider set; `main` builds the router from
    /// configuration instead.
    pub fn replace_providers(&mut self, providers: Arc<ChatRouter>) {
        self.chat = providers.clone();
        self.providers = providers;
        self.refresh_routes();
    }

    /// Re-derive which provider owns each channel from the current allowlist.
    ///
    /// Called whenever the added-channel list changes, so the router reaches exactly the channels
    /// this server answers for, each through the provider it belongs to.
    pub fn refresh_routes(&self) {
        self.providers.replace_routes(&self.all_channels());
    }

    /// The namespace to record for a channel registered through `provider_key`, when one applies.
    ///
    /// A single-provider deployment records one only for a registration-managing bridge, exactly
    /// as before multi-provider support, so its stored rows keep their meaning. A multi-provider
    /// deployment records one for every addition, because that is what says which provider owns
    /// the channel after a restart.
    #[must_use]
    pub fn registration_namespace(&self, provider_key: Option<&str>) -> Option<String> {
        if !self.providers.is_multi() {
            return self
                .chat
                .supports_channel_registration()
                .then(|| self.providers.entries()[0].namespace.clone());
        }
        provider_key
            .and_then(|key| self.providers.namespace_for_key(key))
            .map(str::to_owned)
    }

    /// Look up a configured channel.
    ///
    /// A channel that is not configured does not exist as far as this server is concerned. This is
    /// the allowlist: the bot may be in many channels, but only these are reachable through the
    /// API, so a guessed snowflake cannot turn the bridge into a general-purpose Discord reader.
    /// Configured channels are searched first, then the ones added from inside the app. A
    /// configured entry WINS: the file is the operator's standing statement, and an added row that
    /// shadowed it could silently change whether the bridge may post somewhere.
    ///
    /// A configured channel the owner took off his list is not answered for at all, exactly as an
    /// added channel he removed is not: off the list means unreachable, not merely unlisted. It
    /// still wins over an added row with the same id, so hiding it cannot uncover one.
    #[must_use]
    pub fn channel(&self, id: &str) -> Option<ChannelInfo> {
        if let Some(found) = self.config.channels.iter().find(|c| c.id.as_str() == id) {
            return (!self.is_hidden(&found.id)).then(|| found.clone());
        }
        self.added_channels
            .read()
            .ok()?
            .iter()
            .find(|c| c.channel.as_str() == id)
            .map(|row| added_channel_info(row, &self.providers))
    }

    /// Every channel this server will answer for: configured first, then added, in that order.
    ///
    /// Hidden configured channels are left out, and an added row sharing one's id stays out too —
    /// the same precedence [`AppState::channel`] applies.
    #[must_use]
    pub fn all_channels(&self) -> Vec<ChannelInfo> {
        let mut all = self.listed_configured_channels();
        if let Ok(added) = self.added_channels.read() {
            for channel in added
                .iter()
                .map(|row| added_channel_info(row, &self.providers))
            {
                let configured = self.config.channels.iter().any(|c| c.id == channel.id);
                if !configured && !all.iter().any(|c| c.id == channel.id) {
                    all.push(channel);
                }
            }
        }
        all
    }

    /// Resolve a caller-requested fetch size against the configured default and ceiling.
    #[must_use]
    pub fn effective_limit(&self, requested: Option<u16>) -> u16 {
        let limits = &self.config.chat;
        requested
            .unwrap_or(limits.default_fetch_limit)
            .clamp(1, limits.max_fetch_limit)
    }

    /// Resolve a caller-requested count ceiling against the configured one.
    ///
    /// The configured value is a ceiling, never a floor: a caller may ask to look at fewer
    /// messages than the operator allows, and may not ask to look at more. Counting costs one
    /// Discord request per hundred messages against a shared rate limit, so this is the knob that
    /// keeps "how many messages are in there?" from being an expensive question.
    #[must_use]
    pub fn effective_count_cap(&self, requested: Option<u32>) -> u32 {
        let configured = self.config.chat.max_count_scan;
        requested.map_or(configured, |r| r.clamp(1, configured))
    }

    /// Channel ids this server is configured for, in configuration order.
    #[must_use]
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.config.channels.iter().map(|c| c.id.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::model::ChannelId;
    use crate::testing;

    #[test]
    fn an_unconfigured_channel_is_invisible() {
        let (state, _fake) = testing::state();
        assert!(state.channel("999").is_none());
        assert!(state.channel(testing::READ_CHANNEL).is_some());
    }

    #[test]
    fn a_hidden_configured_channel_is_unreachable_and_cannot_uncover_an_added_row() {
        // `#199 removable-config-channels`. An added row under a configured id predates the file
        // naming it; hiding the configured channel must not quietly promote that row, whose write
        // policy the operator never stated in the file.
        let (state, _fake) = testing::state();
        let id = ChannelId(testing::WRITE_CHANNEL.to_owned());
        state
            .added_channels
            .write()
            .expect("added list")
            .push(crate::store::AddedChannel {
                channel: id.clone(),
                label: "shadow".to_owned(),
                writable: false,
                registration_provider: None,
                added_at_ms: 1,
            });
        state
            .hidden_channels
            .write()
            .expect("hidden list")
            .insert(id.clone());

        assert!(state.channel(id.as_str()).is_none());
        assert!(state.all_channels().iter().all(|c| c.id != id));
        assert_eq!(
            state
                .hidden_configured_channels()
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["lead team"]
        );
        assert!(
            state.configured_channel(id.as_str()).is_some(),
            "the file still names it, which is what lets it be shown again"
        );
        assert!(state.channel(testing::READ_CHANNEL).is_some());
    }

    #[test]
    fn the_fetch_limit_is_defaulted_and_capped() {
        let (state, _fake) = testing::state();
        let max = state.config.chat.max_fetch_limit;
        let default = state.config.chat.default_fetch_limit;
        assert_eq!(state.effective_limit(None), default);
        assert_eq!(state.effective_limit(Some(1)), 1);
        assert_eq!(state.effective_limit(Some(0)), 1);
        assert_eq!(
            state.effective_limit(Some(u16::MAX)),
            max,
            "a caller must not be able to request an unbounded fetch"
        );
    }

    #[test]
    fn the_count_ceiling_is_a_ceiling_and_not_a_floor() {
        let (state, _fake) = testing::state();
        let configured = state.config.chat.max_count_scan;
        assert_eq!(state.effective_count_cap(None), configured);
        assert_eq!(state.effective_count_cap(Some(10)), 10);
        assert_eq!(state.effective_count_cap(Some(0)), 1);
        assert_eq!(
            state.effective_count_cap(Some(u32::MAX)),
            configured,
            "a caller must not be able to make this server walk a channel's whole history"
        );
    }
}
