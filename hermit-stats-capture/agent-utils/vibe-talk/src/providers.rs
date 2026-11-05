//! Several chat providers behind the one [`ChatClient`] the rest of the server speaks to.
//!
//! A deployment may read Discord, a Discord-compatible bridge for another service, and Slack from
//! one running instance. Every channel belongs to exactly one configured provider, so every
//! channel-scoped call is routed to the provider that owns the channel. The few calls that name no
//! channel are answered as follows:
//!
//! * **Registration** asks each provider whether it recognises the pasted reference
//!   ([`ChatClient::claims_source`]). A provider that is certain wins; otherwise the only provider
//!   that might accept it is used; otherwise the operator must say which, by writing the
//!   provider's key and a colon in front of the reference (`gchat:spaces/AAA`).
//! * **Discovery** lists each discovery-capable provider in turn behind one opaque cursor.
//! * **Identity and capability questions** are answered for the whole deployment as "any provider
//!   can", and per provider through [`ChatRouter::entries`] for the page to use per channel.
//!
//! A deployment with one provider is a router with one entry, so it behaves exactly as it did
//! before multi-provider support existed: every call reaches that provider whatever the channel.
//!
//! Every provider's client is wrapped in a [`ProfiledClient`] as the router is built, so each
//! operation is timed whoever calls it, including the live poller, which holds a provider's
//! client directly. `#208 persistent-cache`; see [`crate::upstream_profile`].

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::chat::{ChatClient, ChatError, ChatIdentity, RegisteredChannel, SourceClaim};
use crate::config::{Config, ProviderKind};
use crate::model::{ChannelId, ChannelInfo, Message, MessageId};
use crate::upstream_profile::{ProfiledClient, UpstreamProfile};

/// One configured provider and the client that reaches it.
pub struct ProviderEntry {
    /// The key channels use to name this provider in configuration.
    pub key: String,
    /// The namespace recorded beside channels this provider registered.
    pub namespace: String,
    /// The client for this provider. Inside a [`ChatRouter`], always a [`ProfiledClient`] around
    /// the client the entry was built with.
    pub client: Arc<dyn ChatClient>,
    /// This provider's live poll interval in seconds, `0` when it is not polled.
    pub live_poll_seconds: u64,
}

impl std::fmt::Debug for ProviderEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEntry")
            .field("key", &self.key)
            .field("namespace", &self.namespace)
            .field("provider", &self.client.provider_name())
            .field("live_poll_seconds", &self.live_poll_seconds)
            .finish()
    }
}

/// Routes every channel-scoped call to the provider that owns the channel.
pub struct ChatRouter {
    entries: Vec<ProviderEntry>,
    default: Option<usize>,
    routes: RwLock<BTreeMap<ChannelId, usize>>,
    display_name: String,
    profile: Arc<UpstreamProfile>,
}

impl std::fmt::Debug for ChatRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatRouter")
            .field("entries", &self.entries)
            .field("default", &self.default)
            .finish_non_exhaustive()
    }
}

/// The separator between an explicit provider key and the reference it qualifies.
const KEY_SEPARATOR: char = ':';

/// Prefix of a discovery cursor; the provider index and its own cursor follow.
const CURSOR_PREFIX: &str = "p";

impl ChatRouter {
    /// A router over `entries`; channels that name no provider belong to `default_key`.
    ///
    /// Each entry's client is timed into a new [`UpstreamProfile`]; see
    /// [`ChatRouter::with_profile`].
    ///
    /// # Panics
    ///
    /// Panics when `entries` is empty: a deployment always has at least one provider, and the
    /// configuration loader refuses one that does not.
    #[must_use]
    pub fn new(entries: Vec<ProviderEntry>, default_key: Option<&str>) -> Self {
        Self::with_profile(entries, default_key, Arc::new(UpstreamProfile::new()))
    }

    /// [`ChatRouter::new`], timing each entry's client into `profile` under the entry's key.
    ///
    /// For a builder that hands the same profile to a provider's own client first, as
    /// [`ChatRouter::from_config`] does for Slack's per-call timings.
    ///
    /// # Panics
    ///
    /// As [`ChatRouter::new`].
    #[must_use]
    pub fn with_profile(
        mut entries: Vec<ProviderEntry>,
        default_key: Option<&str>,
        profile: Arc<UpstreamProfile>,
    ) -> Self {
        assert!(
            !entries.is_empty(),
            "a chat router needs at least one provider"
        );
        for entry in &mut entries {
            let recorder = profile.provider(&entry.key, entry.client.provider_name());
            entry.client = Arc::new(ProfiledClient::new(Arc::clone(&entry.client), recorder));
        }
        let default = match entries.len() {
            1 => Some(0),
            _ => default_key.and_then(|key| entries.iter().position(|entry| entry.key == key)),
        };
        let display_name = join_names(&entries);
        Self {
            entries,
            default,
            routes: RwLock::new(BTreeMap::new()),
            display_name,
            profile,
        }
    }

    /// A router with a single provider under the legacy key, as every pre-router deployment had.
    #[must_use]
    pub fn single(client: Arc<dyn ChatClient>, namespace: &str, live_poll_seconds: u64) -> Self {
        Self::new(
            vec![ProviderEntry {
                key: crate::config::LEGACY_DISCORD_PROVIDER_KEY.to_owned(),
                namespace: namespace.to_owned(),
                client,
                live_poll_seconds,
            }],
            None,
        )
    }

    /// Build every configured provider's client.
    ///
    /// # Errors
    ///
    /// Returns [`ChatError`] when a provider's client cannot be built.
    pub fn from_config(config: &Config) -> Result<Self, ChatError> {
        let profile = Arc::new(UpstreamProfile::new());
        let mut entries = Vec::with_capacity(config.providers.len());
        for provider in &config.providers {
            let client: Arc<dyn ChatClient> = match &provider.kind {
                ProviderKind::Discord(discord) => {
                    Arc::new(crate::discord::http::HttpDiscordClient::new(discord)?)
                }
                ProviderKind::Slack(slack) => Arc::new(
                    crate::slack::HttpSlackClient::new(&crate::slack::SlackConfig {
                        provider_name: provider.name.clone(),
                        api_base: slack.api_base.clone(),
                        token: slack.token.clone(),
                        owner_user_id: slack.owner_user_id.clone(),
                        request_timeout_seconds: slack.request_timeout_seconds,
                        channel_registration: slack.channel_registration,
                        registered_channels_writable: slack.registered_channels_writable,
                        channel_discovery: slack.channel_discovery,
                        upstream_clear: slack.upstream_clear,
                        max_post_chars: slack.max_post_chars,
                        max_post_bytes: slack.max_post_bytes,
                    })?
                    .with_shim_profile(profile.provider(&provider.key, &provider.name)),
                ),
            };
            entries.push(ProviderEntry {
                key: provider.key.clone(),
                namespace: provider.namespace(),
                client,
                live_poll_seconds: provider.live_poll_seconds,
            });
        }
        let router = Self::with_profile(entries, config.default_provider_key(), profile);
        router.replace_routes(&config.channels);
        Ok(router)
    }

    /// How long each provider's operations and calls have taken, in numbers only.
    #[must_use]
    pub fn profile(&self) -> &Arc<UpstreamProfile> {
        &self.profile
    }

    /// Every provider, in configuration order.
    #[must_use]
    pub fn entries(&self) -> &[ProviderEntry] {
        &self.entries
    }

    /// The provider configured under `key`.
    #[must_use]
    pub fn entry(&self, key: &str) -> Option<&ProviderEntry> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    /// Whether this deployment has more than one provider.
    #[must_use]
    pub fn is_multi(&self) -> bool {
        self.entries.len() > 1
    }

    /// The provider key for a stored registration namespace; `None` means the default provider.
    ///
    /// A namespace no configured provider has (the deployment changed bridges since the channel
    /// was added) also resolves to the default provider: the channel stays listed and readable as
    /// it always was, and [`ChatRouter::knows_namespace`] lets the caller withhold posting.
    #[must_use]
    pub fn key_for_namespace(&self, namespace: Option<&str>) -> Option<&str> {
        namespace
            .and_then(|namespace| {
                self.entries
                    .iter()
                    .find(|entry| entry.namespace == namespace)
            })
            .or_else(|| self.default.map(|index| &self.entries[index]))
            .map(|entry| entry.key.as_str())
    }

    /// Whether some configured provider owns registrations recorded under `namespace`.
    ///
    /// With one provider that also requires it to manage registration, as it always did: a
    /// deployment that turned registration off no longer vouches for the rows it once recorded.
    #[must_use]
    pub fn knows_namespace(&self, namespace: &str) -> bool {
        self.entries.iter().any(|entry| {
            entry.namespace == namespace
                && (self.is_multi() || entry.client.supports_channel_registration())
        })
    }

    /// The namespace to record for a channel owned by `key`.
    #[must_use]
    pub fn namespace_for_key(&self, key: &str) -> Option<&str> {
        self.entry(key).map(|entry| entry.namespace.as_str())
    }

    /// The provider key that owns `channel`, when it is known.
    #[must_use]
    pub fn key_for(&self, channel: &ChannelId) -> Option<String> {
        self.index_for(channel)
            .map(|index| self.entries[index].key.clone())
    }

    /// The provider that owns `channel`, when it is known, for a question about that channel's
    /// own capabilities rather than the deployment's. `#232 upstream-mark-read`.
    #[must_use]
    pub fn client_for(&self, channel: &ChannelId) -> Option<&Arc<dyn ChatClient>> {
        self.index_for(channel)
            .map(|index| &self.entries[index].client)
    }

    /// Replace the channel routes with those `channels` declare.
    ///
    /// Called with every channel the server answers for whenever that list changes, so a removed
    /// channel stops being routable and an added one starts.
    pub fn replace_routes(&self, channels: &[ChannelInfo]) {
        let routes = channels
            .iter()
            .filter_map(|channel| {
                let key = channel.provider.as_deref()?;
                let index = self.entries.iter().position(|entry| entry.key == key)?;
                Some((channel.id.clone(), index))
            })
            .collect();
        if let Ok(mut slot) = self.routes.write() {
            *slot = routes;
        }
    }

    fn index_for(&self, channel: &ChannelId) -> Option<usize> {
        if self.entries.len() == 1 {
            return Some(0);
        }
        self.routes.read().ok()?.get(channel).copied()
    }

    fn route(&self, channel: &ChannelId) -> Result<&ProviderEntry, ChatError> {
        self.index_for(channel)
            .map(|index| &self.entries[index])
            .ok_or_else(|| {
                ChatError::Refused(format!(
                    "channel {channel} is not assigned to any configured chat provider"
                ))
            })
    }

    fn bind(&self, channel: &ChannelId, index: usize) -> Result<(), ChatError> {
        let mut routes = self.routes.write().map_err(|_| {
            ChatError::Refused("the channel routing table lock is poisoned".to_owned())
        })?;
        match routes.get(channel) {
            Some(existing) if *existing != index => Err(ChatError::Refused(format!(
                "channel {channel} already belongs to the {} provider",
                self.entries[*existing].client.provider_name()
            ))),
            _ => {
                routes.insert(channel.clone(), index);
                Ok(())
            }
        }
    }

    fn unbind(&self, channel: &ChannelId) {
        if let Ok(mut routes) = self.routes.write() {
            routes.remove(channel);
        }
    }

    /// Which provider should register `source`, and the reference to hand it.
    fn select_registrar<'s>(&self, source: &'s str) -> Result<(usize, &'s str), ChatError> {
        if let Some((key, rest)) = source.split_once(KEY_SEPARATOR) {
            if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
                return Ok((index, rest.trim()));
            }
        }
        if self.entries.len() == 1 {
            return Ok((0, source));
        }
        let claims: Vec<SourceClaim> = self
            .entries
            .iter()
            .map(|entry| match entry.client.claims_source(source) {
                // Only a registration-managing provider resolves references it cannot recognise.
                SourceClaim::Possible if !entry.client.supports_channel_registration() => {
                    SourceClaim::Never
                }
                claim => claim,
            })
            .collect();
        let certain: Vec<usize> = indices_with(&claims, SourceClaim::Certain);
        if let [only] = certain.as_slice() {
            return Ok((*only, source));
        }
        if certain.is_empty() {
            if let [only] = indices_with(&claims, SourceClaim::Possible).as_slice() {
                return Ok((*only, source));
            }
        }
        let keys: Vec<&str> = self
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect();
        if certain.len() > 1 || claims.contains(&SourceClaim::Possible) {
            Err(ChatError::Refused(format!(
                "more than one chat provider could own that reference; put a provider key and a \
                 colon in front of it, one of: {}",
                keys.join(", ")
            )))
        } else {
            Err(ChatError::Refused(format!(
                "no configured chat provider recognises that reference ({})",
                self.display_name
            )))
        }
    }

    /// `source` as the page should hand it back to registration: prefixed with the provider key
    /// only when the bare reference would not reach the provider that listed it.
    fn qualify_source(&self, index: usize, source: String) -> String {
        match self.select_registrar(&source) {
            Ok((chosen, rest)) if chosen == index && rest == source => source,
            _ => format!("{}{KEY_SEPARATOR}{source}", self.entries[index].key),
        }
    }

    fn discovery_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.client.supports_channel_discovery())
            .map(|(index, _)| index)
            .collect()
    }
}

fn indices_with(claims: &[SourceClaim], wanted: SourceClaim) -> Vec<usize> {
    claims
        .iter()
        .enumerate()
        .filter(|(_, claim)| **claim == wanted)
        .map(|(index, _)| index)
        .collect()
}

/// "Slack", "Slack and Google Chat", "Discord, Slack, and Google Chat".
fn join_names(entries: &[ProviderEntry]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for entry in entries {
        let name = entry.client.provider_name();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    match names.as_slice() {
        [] => "Chat".to_owned(),
        [only] => (*only).to_owned(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn encode_cursor(index: usize, inner: Option<&str>) -> String {
    format!("{CURSOR_PREFIX}{index}.{}", inner.unwrap_or_default())
}

fn decode_cursor(cursor: &str) -> Option<(usize, Option<String>)> {
    let rest = cursor.strip_prefix(CURSOR_PREFIX)?;
    let (index, inner) = rest.split_once('.')?;
    let index = index.parse().ok()?;
    Some((index, (!inner.is_empty()).then(|| inner.to_owned())))
}

#[async_trait]
impl ChatClient for ChatRouter {
    fn provider_name(&self) -> &str {
        &self.display_name
    }

    fn claims_source(&self, source: &str) -> SourceClaim {
        match self.select_registrar(source) {
            Ok(_) if self.entries.len() == 1 => self.entries[0].client.claims_source(source),
            Ok(_) => SourceClaim::Certain,
            Err(_) => SourceClaim::Never,
        }
    }

    fn self_author_id(&self) -> Option<String> {
        self.default
            .and_then(|index| self.entries[index].client.self_author_id())
    }

    fn owner_author_id(&self) -> Option<String> {
        self.default
            .and_then(|index| self.entries[index].client.owner_author_id())
    }

    fn supports_threading(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.client.supports_threading())
    }

    /// Like the other deployment-wide flags, whether ANY provider does; the page reads each
    /// channel's own provider's answer from its description.
    fn replies_in_thread(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.client.replies_in_thread())
    }

    async fn fetch_timeline(
        &self,
        channel: &ChannelId,
        request: &crate::threads::TimelineRequest,
    ) -> Result<crate::threads::TimelinePage, ChatError> {
        self.route(channel)?
            .client
            .fetch_timeline(channel, request)
            .await
    }

    /// `#203 incremental-refresh`. The channel's own provider decides how to catch up, so one that
    /// overrides the generic default keeps its override behind the router. There is no
    /// deployment-wide answer: a bridge channel and a Slack channel in the same server each get
    /// their own kind of forward cursor.
    async fn fetch_timeline_since(
        &self,
        channel: &ChannelId,
        request: &crate::threads::TimelineRequest,
        since: &crate::timeline_forward::GenericPosition,
    ) -> Result<crate::timeline_forward::GenericDelta, ChatError> {
        self.route(channel)?
            .client
            .fetch_timeline_since(channel, request, since)
            .await
    }

    async fn post_in_thread(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.route(channel)?
            .client
            .post_in_thread(channel, thread_id, content, reply_to)
            .await
    }

    async fn post_in_thread_keyed(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
        key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.route(channel)?
            .client
            .post_in_thread_keyed(channel, thread_id, content, reply_to, key)
            .await
    }

    /// The channel's own provider's limit: a Slack channel and a Google Chat channel in one
    /// server are cut at their own sizes. `#263 post-split-per-provider`. A channel no provider
    /// holds cannot be posted to at all, so the default it falls back to is never spent.
    fn post_limit(&self, channel: &ChannelId) -> crate::chat::PostLimit {
        match self.route(channel) {
            Ok(entry) => entry.client.post_limit(channel),
            Err(_) => crate::chat::PostLimit::chars(crate::discord::http::DISCORD_MAX_CONTENT_LEN),
        }
    }

    fn continuation_thread(&self, channel: &ChannelId, first: &Message) -> Option<String> {
        self.route(channel)
            .ok()?
            .client
            .continuation_thread(channel, first)
    }

    async fn fetch_thread_message(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        message_id: &MessageId,
    ) -> Result<Message, ChatError> {
        self.route(channel)?
            .client
            .fetch_thread_message(channel, thread_id, message_id)
            .await
    }

    fn supports_channel_registration(&self) -> bool {
        match self.entries.as_slice() {
            [only] => only.client.supports_channel_registration(),
            entries => entries
                .iter()
                .any(|entry| entry.client.supports_channel_registration()),
        }
    }

    async fn register_channel(
        &self,
        source: &str,
        label: &str,
    ) -> Result<RegisteredChannel, ChatError> {
        let (index, reference) = self.select_registrar(source)?;
        let entry = &self.entries[index];
        let registered = if !entry.client.supports_channel_registration() && self.is_multi() {
            // Direct Discord: the reference IS the channel id, and nothing exists upstream to
            // create. Posting stays off, exactly as a direct addition defaults it.
            if entry.client.claims_source(reference) != SourceClaim::Certain {
                return Err(ChatError::Refused(format!(
                    "{} accepts only a numeric channel id",
                    entry.client.provider_name()
                )));
            }
            RegisteredChannel {
                id: ChannelId(reference.to_owned()),
                created: false,
                writable: false,
            }
        } else {
            entry.client.register_channel(reference, label).await?
        };
        if let Err(error) = self.bind(&registered.id, index) {
            if registered.created {
                if let Err(undo) = entry.client.unregister_channel(&registered.id).await {
                    tracing::warn!(channel = %registered.id, %undo, "could not undo a registration that collided with another provider");
                }
            }
            return Err(error);
        }
        Ok(registered)
    }

    async fn unregister_channel(&self, channel: &ChannelId) -> Result<(), ChatError> {
        let entry = self.route(channel)?;
        if entry.client.supports_channel_registration() || !self.is_multi() {
            entry.client.unregister_channel(channel).await?;
        }
        if self.is_multi() {
            self.unbind(channel);
        }
        Ok(())
    }

    fn supports_channel_discovery(&self) -> bool {
        !self.discovery_indices().is_empty()
    }

    async fn discover_channels(
        &self,
        request: &crate::directory::DirectoryRequest,
    ) -> Result<crate::directory::DirectoryPage, ChatError> {
        let indices = self.discovery_indices();
        if let [only] = indices.as_slice() {
            if !self.is_multi() {
                return self.entries[*only].client.discover_channels(request).await;
            }
        }
        let Some(&first) = indices.first() else {
            return Err(ChatError::Refused(
                "no configured chat provider can list its channels".to_owned(),
            ));
        };
        let (index, inner) = match request.cursor.as_deref() {
            None => (first, None),
            Some(cursor) => decode_cursor(cursor)
                .filter(|(index, _)| indices.contains(index))
                .ok_or_else(|| {
                    ChatError::Refused(
                        "that directory cursor is not one this server issued".to_owned(),
                    )
                })?,
        };
        let inner_request = crate::directory::DirectoryRequest {
            query: request.query.clone(),
            cursor: inner,
            limit: request.limit,
        };
        let page = self.entries[index]
            .client
            .discover_channels(&inner_request)
            .await?;
        let next_cursor = match page.next_cursor.as_deref() {
            Some(inner) => Some(encode_cursor(index, Some(inner))),
            None => indices
                .iter()
                .find(|candidate| **candidate > index)
                .map(|next| encode_cursor(*next, None)),
        };
        let entries = page
            .entries
            .into_iter()
            .map(|mut entry| {
                entry.source = self.qualify_source(index, entry.source);
                entry
            })
            .collect();
        Ok(crate::directory::DirectoryPage {
            entries,
            next_cursor,
            truncated: page.truncated,
        })
    }

    fn supports_upstream_read_mark(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.client.supports_upstream_read_mark())
    }

    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        let index = self.default.unwrap_or(0);
        self.entries[index].client.identity().await
    }

    async fn fetch_page(
        &self,
        channel: &ChannelId,
        limit: u16,
        before: Option<&MessageId>,
        after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        self.route(channel)?
            .client
            .fetch_page(channel, limit, before, after)
            .await
    }

    async fn probe_channel(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        self.route(channel)?
            .client
            .probe_channel(channel, limit)
            .await
    }

    async fn fetch_recent(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        self.route(channel)?
            .client
            .fetch_recent(channel, limit)
            .await
    }

    /// `#208 persistent-cache`. Forwarded, so a provider that serves its newest messages from a
    /// copy it keeps can say how old that copy is; the trait's default would read `fetch_recent`
    /// and drop the label.
    async fn fetch_recent_dated(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<crate::chat::RecentRead, ChatError> {
        self.route(channel)?
            .client
            .fetch_recent_dated(channel, limit)
            .await
    }

    async fn post_message(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.route(channel)?
            .client
            .post_message(channel, content, reply_to)
            .await
    }

    /// Whether ANY provider posts keyed messages at most once, like the other deployment-wide
    /// flags; the key itself goes to the channel's own provider, which ignores it if it cannot.
    fn supports_idempotent_posts(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.client.supports_idempotent_posts())
    }

    async fn post_message_keyed(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
        key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.route(channel)?
            .client
            .post_message_keyed(channel, content, reply_to, key)
            .await
    }

    async fn mark_read_upstream(
        &self,
        channel: &ChannelId,
        through: &MessageId,
    ) -> Result<(), ChatError> {
        self.route(channel)?
            .client
            .mark_read_upstream(channel, through)
            .await
    }

    fn supports_upstream_clear(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.client.supports_upstream_clear())
    }

    async fn clear_upstream_through(
        &self,
        channel: &ChannelId,
        through: &MessageId,
        dry_run: bool,
    ) -> Result<crate::chat::UpstreamClearOutcome, ChatError> {
        self.route(channel)?
            .client
            .clear_upstream_through(channel, through, dry_run)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::{DirectoryEntry, DirectoryRequest};
    use crate::discord::fake::FakeDiscord;

    /// A fake provider with its own name and its own idea of which references are its.
    struct Named {
        inner: Arc<FakeDiscord>,
        name: &'static str,
        claim: fn(&str) -> SourceClaim,
    }

    #[async_trait]
    impl ChatClient for Named {
        fn provider_name(&self) -> &str {
            self.name
        }
        fn claims_source(&self, source: &str) -> SourceClaim {
            (self.claim)(source)
        }
        fn self_author_id(&self) -> Option<String> {
            Some(format!("{}-self", self.name))
        }
        fn supports_channel_registration(&self) -> bool {
            self.inner.supports_channel_registration()
        }
        async fn register_channel(
            &self,
            source: &str,
            label: &str,
        ) -> Result<RegisteredChannel, ChatError> {
            ChatClient::register_channel(self.inner.as_ref(), source, label).await
        }
        async fn unregister_channel(&self, channel: &ChannelId) -> Result<(), ChatError> {
            self.inner.unregister_channel(channel).await
        }
        fn supports_channel_discovery(&self) -> bool {
            self.inner.supports_channel_discovery()
        }
        async fn discover_channels(
            &self,
            request: &DirectoryRequest,
        ) -> Result<crate::directory::DirectoryPage, ChatError> {
            self.inner.discover_channels(request).await
        }
        async fn identity(&self) -> Result<ChatIdentity, ChatError> {
            self.inner.identity().await
        }
        async fn fetch_page(
            &self,
            channel: &ChannelId,
            limit: u16,
            before: Option<&MessageId>,
            after: Option<&MessageId>,
        ) -> Result<Vec<Message>, ChatError> {
            self.inner.fetch_page(channel, limit, before, after).await
        }
        async fn post_message(
            &self,
            channel: &ChannelId,
            content: &str,
            reply_to: Option<&MessageId>,
        ) -> Result<Message, ChatError> {
            self.inner.post_message(channel, content, reply_to).await
        }
    }

    fn slack_claim(source: &str) -> SourceClaim {
        if source.starts_with('C') || source.contains("slack.com") {
            SourceClaim::Certain
        } else {
            SourceClaim::Never
        }
    }

    fn possible(_source: &str) -> SourceClaim {
        SourceClaim::Possible
    }

    fn channel(id: &str, provider: &str) -> ChannelInfo {
        ChannelInfo {
            id: ChannelId(id.to_owned()),
            label: id.to_owned(),
            writable: true,
            alias: None,
            added: false,
            provider: Some(provider.to_owned()),
        }
    }

    struct Pair {
        router: ChatRouter,
        gchat: Arc<FakeDiscord>,
        slack: Arc<FakeDiscord>,
    }

    fn pair() -> Pair {
        let gchat = Arc::new(FakeDiscord::new());
        let slack = Arc::new(FakeDiscord::new());
        let entry =
            |key: &str, name: &'static str, inner: &Arc<FakeDiscord>, claim, poll| ProviderEntry {
                key: key.to_owned(),
                namespace: format!("ns:{key}"),
                client: Arc::new(Named {
                    inner: Arc::clone(inner),
                    name,
                    claim,
                }),
                live_poll_seconds: poll,
            };
        let router = ChatRouter::new(
            vec![
                entry("gchat", "Google Chat", &gchat, possible, 0),
                entry("slack", "Slack", &slack, slack_claim, 15),
            ],
            None,
        );
        router.replace_routes(&[channel("900", "gchat"), channel("C0123ABCDE", "slack")]);
        Pair {
            router,
            gchat,
            slack,
        }
    }

    #[tokio::test]
    async fn each_channel_is_read_and_posted_through_the_provider_that_owns_it() {
        let Pair {
            router,
            gchat,
            slack,
        } = pair();
        let space = ChannelId("900".to_owned());
        let team = ChannelId("C0123ABCDE".to_owned());
        gchat.seed(&space, "ada", "from chat");
        slack.seed(&team, "grace", "from slack");

        let read = router.fetch_recent(&team, 10).await.expect("slack read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].content, "from slack");
        let read = router.fetch_recent(&space, 10).await.expect("chat read");
        assert_eq!(read[0].content, "from chat");

        router
            .post_message(&team, "hello", None)
            .await
            .expect("post");
        assert_eq!(slack.posted().len(), 1);
        assert!(
            gchat.posted().is_empty(),
            "a post must reach only the owning provider"
        );

        assert_eq!(router.key_for(&team).as_deref(), Some("slack"));
        assert_eq!(router.provider_name(), "Google Chat and Slack");
    }

    #[tokio::test]
    async fn a_channel_no_provider_owns_is_refused_rather_than_sent_somewhere() {
        let Pair { router, .. } = pair();
        let stray = ChannelId("777".to_owned());
        assert!(matches!(
            router.fetch_recent(&stray, 1).await,
            Err(ChatError::Refused(_))
        ));
        // A single provider has always answered for every channel, routed or not.
        let fake = Arc::new(FakeDiscord::new());
        fake.seed(&stray, "ada", "hi");
        let single = ChatRouter::single(fake, "ns", 0);
        assert_eq!(
            single.fetch_recent(&stray, 1).await.expect("single").len(),
            1
        );
    }

    #[tokio::test]
    async fn registration_goes_to_the_provider_that_recognises_the_reference() {
        let Pair {
            router,
            gchat,
            slack,
        } = pair();
        let team = ChannelId("C0999ZZZZZ".to_owned());
        let space = ChannelId("901".to_owned());
        slack.enable_channel_registration(&team, false, true);
        gchat.enable_channel_registration(&space, true, false);

        // Certain beats possible.
        let registered = router
            .register_channel("https://acme.slack.com/archives/C0999ZZZZZ", "team")
            .await
            .expect("slack registration");
        assert_eq!(registered.id, team);
        assert_eq!(router.key_for(&team).as_deref(), Some("slack"));
        assert!(gchat.registration_calls().is_empty());

        // Only the bridge might own a reference nobody is certain of.
        let registered = router
            .register_channel("https://chat.example/room/1", "space")
            .await
            .expect("bridge registration");
        assert_eq!(registered.id, space);
        assert_eq!(router.key_for(&space).as_deref(), Some("gchat"));

        // An explicit key wins, and the key is not part of what the provider sees.
        router
            .register_channel("gchat:C0-looks-like-slack", "forced")
            .await
            .expect("explicit provider");
        assert_eq!(
            gchat
                .registration_calls()
                .last()
                .map(|(source, _)| source.as_str()),
            Some("C0-looks-like-slack")
        );
    }

    #[tokio::test]
    async fn an_ambiguous_reference_asks_for_a_provider_key() {
        let one = Arc::new(FakeDiscord::new());
        let two = Arc::new(FakeDiscord::new());
        one.enable_channel_registration(&ChannelId("1".to_owned()), false, false);
        two.enable_channel_registration(&ChannelId("2".to_owned()), false, false);
        let entry = |key: &str, inner: &Arc<FakeDiscord>| ProviderEntry {
            key: key.to_owned(),
            namespace: key.to_owned(),
            client: Arc::new(Named {
                inner: Arc::clone(inner),
                name: "Bridge",
                claim: possible,
            }),
            live_poll_seconds: 0,
        };
        let router = ChatRouter::new(vec![entry("a", &one), entry("b", &two)], None);
        let error = router
            .register_channel("room/1", "x")
            .await
            .expect_err("ambiguous");
        assert!(error.to_string().contains("a, b"), "{error}");
        assert_eq!(
            router
                .register_channel("b:room/1", "x")
                .await
                .expect("named")
                .id,
            ChannelId("2".to_owned())
        );
    }

    #[tokio::test]
    async fn a_registration_colliding_with_another_providers_channel_is_undone() {
        let Pair { router, gchat, .. } = pair();
        // The bridge claims to have just created a channel whose id Slack already owns.
        let taken = ChannelId("C0123ABCDE".to_owned());
        gchat.enable_channel_registration(&taken, true, false);
        let error = router
            .register_channel("room/2", "clash")
            .await
            .expect_err("collision");
        assert!(error.to_string().contains("Slack"), "{error}");
        assert_eq!(gchat.unregistration_calls(), std::slice::from_ref(&taken));
        assert_eq!(router.key_for(&taken).as_deref(), Some("slack"));
    }

    #[tokio::test]
    async fn discovery_walks_every_provider_behind_one_cursor() {
        let Pair {
            router,
            gchat,
            slack,
        } = pair();
        let entry = |source: &str, name: &str| DirectoryEntry {
            source: source.to_owned(),
            name: name.to_owned(),
            registered_channel_id: None,
        };
        gchat.enable_channel_registration(&ChannelId("9".to_owned()), false, false);
        gchat.enable_channel_directory(
            vec![entry("spaces/A", "Alpha"), entry("spaces/B", "Beta")],
            false,
        );
        slack.enable_channel_registration(&ChannelId("C1".to_owned()), false, false);
        slack.enable_channel_directory(vec![entry("C0123ABCDE", "#team")], false);

        let mut cursor = None;
        let mut seen = Vec::new();
        for _ in 0..5 {
            let page = router
                .discover_channels(&DirectoryRequest {
                    query: None,
                    cursor: cursor.clone(),
                    limit: 1,
                })
                .await
                .expect("page");
            seen.extend(page.entries.into_iter().map(|entry| entry.source));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        // Slack's own ids reach Slack unqualified; the bridge's references are bare too, because
        // it is the only provider that would take them.
        assert_eq!(seen, ["spaces/A", "spaces/B", "C0123ABCDE"]);
        assert!(router
            .discover_channels(&DirectoryRequest {
                query: None,
                cursor: Some("forged".to_owned()),
                limit: 1,
            })
            .await
            .is_err());
    }

    #[test]
    fn namespaces_resolve_to_providers_and_unknown_ones_to_the_default() {
        let Pair { router, .. } = pair();
        assert_eq!(router.key_for_namespace(Some("ns:slack")), Some("slack"));
        assert_eq!(
            router.key_for_namespace(Some("ns:gone")),
            None,
            "no default among several"
        );
        assert!(router.knows_namespace("ns:gchat"));
        assert!(!router.knows_namespace("ns:gone"));
        assert_eq!(
            router.self_author_id(),
            None,
            "several providers have no single self"
        );
    }

    #[test]
    fn display_names_read_as_a_list() {
        let fake = || -> Arc<dyn ChatClient> { Arc::new(FakeDiscord::new()) };
        let entry = |key: &str, name: &'static str| ProviderEntry {
            key: key.to_owned(),
            namespace: key.to_owned(),
            client: Arc::new(Named {
                inner: Arc::new(FakeDiscord::new()),
                name,
                claim: possible,
            }),
            live_poll_seconds: 0,
        };
        assert_eq!(ChatRouter::single(fake(), "", 0).provider_name(), "Discord");
        let three = ChatRouter::new(
            vec![
                entry("a", "Discord"),
                entry("b", "Slack"),
                entry("c", "Google Chat"),
            ],
            Some("a"),
        );
        assert_eq!(three.provider_name(), "Discord, Slack, and Google Chat");
        assert_eq!(three.self_author_id().as_deref(), Some("Discord-self"));
    }
}
