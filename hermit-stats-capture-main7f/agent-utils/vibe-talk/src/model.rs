//! The normalized message and channel types the whole server speaks in.
//!
//! Provider implementations convert their wire formats into these types. The current cursor and
//! ordering helpers still describe Discord snowflakes; making those provider-neutral is a separate
//! migration because the browser and durable store rely on the same ordering contract.

use serde::{Deserialize, Serialize};

/// A Discord channel snowflake, as a string, exactly as Discord renders it.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct ChannelId(pub String);

impl ChannelId {
    /// Borrow the underlying snowflake text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Discord message snowflake, as a string.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct MessageId(pub String);

impl MessageId {
    /// Borrow the underlying snowflake text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Numeric value of the snowflake, when it parses.
    ///
    /// Discord snowflakes sort chronologically as integers but NOT as strings once they differ in
    /// length, so ordering must go through this rather than through string comparison.
    #[must_use]
    pub fn numeric(&self) -> Option<u64> {
        self.0.parse::<u64>().ok()
    }

    /// When this message was created, in milliseconds since the Unix epoch.
    ///
    /// A Discord snowflake carries its own creation time in its top 42 bits. That is what makes a
    /// time range cheap here: Discord paginates by id and offers no positional index, but an
    /// instant converts straight into an id boundary, so "since yesterday afternoon" becomes an
    /// exact `after=` cursor rather than a guess at an offset.
    #[must_use]
    pub fn created_at_ms(&self) -> Option<i64> {
        let raw = self.numeric()?;
        i64::try_from((raw >> 22) + DISCORD_EPOCH_MS).ok()
    }

    /// The id that sits exactly at `ms`, for use as a `before`/`after` boundary.
    ///
    /// NOT A REAL MESSAGE ID. The worker and sequence bits are zero, so it compares correctly
    /// against real ids and must never be handed back to a caller as though it identified a
    /// message. An instant before Discord existed clamps to the epoch, because a negative
    /// snowflake is not a thing.
    #[must_use]
    pub fn at_time_ms(ms: i64) -> Self {
        let since_epoch = u64::try_from(ms)
            .unwrap_or(0)
            .saturating_sub(DISCORD_EPOCH_MS);
        Self((since_epoch << 22).to_string())
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The first millisecond Discord counts snowflakes from: 2015-01-01T00:00:00Z.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// A Discord user snowflake, as a string.
///
/// Kept distinct from [`MessageId`] and [`ChannelId`] so the three cannot be passed for one
/// another: they are all decimal snowflakes, and a wrong one is silently plausible.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct UserId(pub String);

impl UserId {
    /// Borrow the underlying snowflake text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Render the Discord mention that actually notifies this user: `<@123…>`.
    ///
    /// Discord notifies on this exact syntax and on nothing else. Writing `@display-name` is plain
    /// text — it renders as words and pings nobody, which is the failure this exists to prevent.
    #[must_use]
    pub fn mention(&self) -> String {
        format!("<@{}>", self.0)
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One chat message as the rest of the server sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Message {
    /// Thread membership supplied by the backend; independent of a reply-to message reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<crate::threads::MessageThread>,
    /// Snowflake of this message.
    pub id: MessageId,
    /// Channel the message was read from, in the provider-neutral application namespace.
    pub channel_id: ChannelId,
    /// Display name of the author, as reported by the chat provider.
    pub author: String,
    /// Snowflake of the author.
    ///
    /// This travels with every rendered message ON PURPOSE, and the alternative — a
    /// "look up this display name" tool — was rejected. Two reasons, in order of importance:
    ///
    /// 1. **No lookup step, so nothing to guess.** The id arrives ATTACHED to the very message
    ///    being replied to, so a caller that wants to mention its author already holds the exact
    ///    snowflake. A lookup tool would put a fuzzy name match between the agent and a real
    ///    notification, and a hallucinated id pings a stranger.
    /// 2. **It bounds who can be mentioned.** The only ids a caller ever sees are those of people
    ///    and bots that have actually spoken in an allowlisted channel. A general user-lookup tool
    ///    would let the agent ping anyone in the server, which is a strictly larger capability
    ///    than this project wants to hand a voice model.
    ///
    /// Bots have ids too and they are included: addressing another coding agent by mention is a
    /// legitimate thing to want.
    pub author_id: UserId,
    /// Whether the chat provider flagged the author as a bot.
    pub author_is_bot: bool,
    /// The exact instant, ISO-8601, as reported by the chat provider. Compute with this one.
    ///
    /// Unrounded on purpose: sub-second precision is ordering information, and anything that has
    /// to decide which of two messages came first needs it. This field is never spoken; see
    /// [`Message::spoken_time`] for the one that is.
    pub timestamp: String,
    /// The same instant in the operator's configured zone, already formatted for speech: `09:51`.
    /// READ THIS ONE ALOUD.
    ///
    /// Two fields rather than one because they answer different questions, and because a single
    /// field would force whoever renders it to choose — which is exactly the choice that produced
    /// the bug. Handed an ISO string in UTC, the assistant said "thirteen fifty-one Eastern Time":
    /// it kept the digits and relabelled them. A value that is already correct when read verbatim
    /// requires no conversion and admits no such mistake.
    ///
    /// No seconds and no zone label: see [`crate::clock::spoken`], which explains what carrying
    /// them cost a listener and what dropping them trades away.
    ///
    /// EMPTY means "not yet stamped". The Discord layer constructs it empty because it holds no
    /// configuration and therefore does not know the operator's zone; [`crate::ops`] is the only
    /// filler. Use [`Message::spoken`] to read it, which falls back to the exact instant rather
    /// than to a blank.
    #[serde(default)]
    pub spoken_time: String,
    /// The message this one is a reply to, when it is one.
    ///
    /// Discord records a reply on the REPLYING message, as `message_reference.message_id`, and
    /// nowhere else — there is no "replies" field on the message being answered. So "has this been
    /// replied to?" is not a fact a single message carries; it is derived by looking at what else
    /// is loaded. That derivation belongs to the reader, which is why this field is the raw pointer
    /// and not a `has_replies` boolean:
    ///
    /// 1. **A boolean computed here would be a lie about scope.** The server answers a read with a
    ///    WINDOW of a channel, so it could only ever mean "replied to within this window", and a
    ///    field named `has_replies` would be believed to mean more than that.
    /// 2. **The pointer is the durable fact.** It is true of the message forever, independently of
    ///    what else happens to be loaded beside it.
    ///
    /// `None` for an ordinary message, and also for a reply whose reference Discord did not
    /// include. Absent is not "not a reply"; it is "no pointer was reported", and nothing should
    /// present the two differently.
    #[serde(default)]
    pub reply_to: Option<MessageId>,
    /// Raw message body. UNTRUSTED: written by whoever is in the channel.
    pub content: String,
    /// The same body rewritten for speech by [`crate::speakable`]: markdown off, instants said the
    /// way a listener places them, long ids and hashes named. READ THIS ONE ALOUD.
    ///
    /// Two fields for the same reason there are two time fields — they answer different questions.
    /// `content` is what was written and is what the SCREEN must show, character for character;
    /// this is what a voice should say, and saying it costs the reader none of the seconds that
    /// spelling a nineteen-digit snowflake costs. Neither is a substitute for the other, and a
    /// renderer that picked one for both jobs would be wrong in one of them.
    ///
    /// UNTRUSTED, exactly as much as `content` is: rewriting a body for speech is not sanitizing it
    /// for a prompt, and it still goes through [`crate::untrusted`] on the way to a model.
    ///
    /// EMPTY means "the body to say is the raw body". That covers two cases on purpose: nothing has
    /// prepared this message yet — the chat adapters hold no configuration, so [`crate::ops`] is the
    /// only filler, the same convention as [`Message::spoken_time`] — and the prepared form came
    /// back identical, which is the common case for ordinary chat with no markdown and no ids in
    /// it. One value can mean both because [`Message::spoken_body`] answers them the same way, and
    /// collapsing them is what keeps a window of a hundred messages from carrying a hundred
    /// near-copies of itself down a phone's connection. When the field IS set, something was
    /// genuinely rewritten.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spoken_content: String,
    /// The body as the page draws it: `content` rendered from Markdown and SANITIZED, by
    /// [`crate::render`]. `#217 markdown-blocks`.
    ///
    /// A third field for the same body, and it answers a third question: not what was written
    /// (`content`, which a copy keeps and the speech path falls back to) nor what a voice should
    /// say (`spoken_content`), but what the reader should SEE — lists with bullets, paragraphs with
    /// space between them, a link that is a link. HTML the server's sanitizer has already cut to a
    /// small allowlist, so the page inserts it as it stands and builds no markup of its own from
    /// channel text.
    ///
    /// EMPTY means "draw `content` as plain paragraphs", by the convention `spoken_content`
    /// follows: it is what a message carries before anything renders it — on every path toward a
    /// model, which has no use for HTML — and what it carries when rendering changed nothing,
    /// which is most chat. Filled only on the routes the page reads.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_html: String,
    /// Whether the owner's noise rules call this message read already. `#196 auto-read-noise`.
    ///
    /// DECIDED BY THIS SERVER EVERY TIME the message is served, from its current text, by
    /// [`crate::noise::NoiseFilter::mark`]; it is never stored and never trusted from whoever
    /// sent the message. That is what lets an edited placeholder come back as unread, and a
    /// removed rule un-hide everything it caught, without a single record having to change.
    ///
    /// The page reads this flag rather than matching the text itself, so there is one predicate
    /// and it is the server's. Absent means `false`, which is also what an older server meant.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub noise: bool,
    /// The emoji people reacted to this message with, and how many of each. `#219
    /// emoji-reactions`.
    ///
    /// ABSENT AND EMPTY ARE DIFFERENT ANSWERS. Absent (`None`) means this copy came by a path that
    /// cannot see reactions: a provider that does not report them, or a live echo of a message,
    /// which carries none. Empty means the source looked and there are none. So a live copy
    /// without reactions never overwrites a held copy that has them — the echo of a message
    /// arrives after the read that brought its acknowledgements — while a read replaces them, as
    /// it replaces the rest of the message. Discord omits the array when there are none, so on
    /// that provider absent from a read means none.
    ///
    /// Counts only, never who reacted: that is a separate question for each emoji, and asking it
    /// would hand this server identities it has no use for. UNTRUSTED: a custom emoji's name is
    /// written by whoever made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<Vec<Reaction>>,
    /// The tag, time and title the message's first line states, for the page to show in the row's
    /// header. `#242 rich-headers`; see [`crate::heading`].
    ///
    /// Filled where `content_html` is, on the routes only the page reads, and absent everywhere
    /// else, including from a message nothing could be read from. When it is set, the part of the
    /// first line it restates is marked in `content_html` (or, for a plain body, is
    /// `content[..cut]`), so the page can hide those words while it shows the heading and draw them
    /// exactly as before when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<Heading>,
}

/// What a message's first line says about it, lifted out for the row's header. `#242
/// rich-headers`.
///
/// Every piece is UNTRUSTED plain text out of `content`, to be drawn as text. Flat and optional
/// throughout, so the contract can grow without an older page refusing the row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Heading {
    /// Who the message says wrote it: `build-bot coordinator`, `ops2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The clock time the message states, exactly as written: `10:52 PM ET`, `15:05Z`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// The short title it opens with, emphasis markers removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// How much of `content`, from its start, the heading restates, in UTF-16 code units: the
    /// length a page's string indexes by. Always within the first line.
    pub cut: u32,
}

/// One emoji's tally on one message. `#219 emoji-reactions`.
///
/// Flat, with optional fields, rather than a union of standard and custom emoji: the contract
/// grows by optional fields without an older page refusing the row, and a union's new variant
/// would be refused (see [`crate::contract`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Reaction {
    /// What to show: the emoji itself for a standard one ("👀"), or a custom emoji's name without
    /// colons ("party-parrot"), which is all of it a page can draw without fetching an image.
    /// UNTRUSTED, and never empty.
    pub emoji: String,
    /// Whether `emoji` is a custom emoji's name rather than the emoji itself. A page draws a name
    /// as `:name:`, so it reads as an emoji's name and not as a word somebody wrote.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
    /// The provider's own id of a custom emoji: a Discord snowflake, a Google Chat uid. Opaque,
    /// never a URL and never shown; it keeps two custom emoji of the same name apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_id: Option<String>,
    /// How many reacted with it. At least one: an emoji nobody reacted with is not listed.
    pub count: u32,
}

impl Message {
    /// The time label to render: the spoken form when it has been stamped, else the raw instant.
    ///
    /// Never blank, and never invented. A message that reached a renderer unstamped is a bug in
    /// the pipeline, but the reader should still learn when it was said.
    ///
    /// This one is about the TIME. [`Message::spoken_body`] is the matching accessor for the text.
    #[must_use]
    pub fn spoken(&self) -> &str {
        if self.spoken_time.is_empty() {
            &self.timestamp
        } else {
            &self.spoken_time
        }
    }

    /// The body to say out loud: the prepared form when there is one, else the raw body.
    ///
    /// Falling back to the raw body means an unprepared message is read badly rather than read as
    /// nothing. Silence would be the worse failure: a reader who is listening cannot tell it apart
    /// from a message that never arrived.
    #[must_use]
    pub fn spoken_body(&self) -> &str {
        if self.spoken_content.is_empty() {
            &self.content
        } else {
            &self.spoken_content
        }
    }
}

/// Sort a batch of messages oldest-first, by snowflake where possible.
///
/// Messages whose id does not parse keep their relative order and sort after the parseable ones,
/// so a malformed id degrades placement instead of corrupting the whole ordering.
pub fn sort_oldest_first(messages: &mut [Message]) {
    messages.sort_by_key(|m| (m.id.numeric().is_none(), m.id.numeric().unwrap_or(0)));
}

/// A channel this server is configured to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChannelInfo {
    /// Snowflake of the channel.
    pub id: ChannelId,
    /// Human label used in speech and in the web app ("acme lead team").
    ///
    /// This one comes from the operator's CONFIGURATION file and cannot be changed at runtime.
    /// What a person actually hears is [`ChannelInfo::display_name`], because `#39 channel-alias`
    /// lets him rename a channel from inside the app without editing a file or redeploying.
    pub label: String,
    /// Whether posting into this channel is permitted at all.
    pub writable: bool,
    /// The operator's own local name for this channel, when he has set one. `#39 channel-alias`.
    ///
    /// **Local, and only ours.** It lives in this server's store, exactly as read state does; it
    /// is never sent to Discord, renames nothing there, and no one outside this deployment sees
    /// it. It is set by the OPERATOR through the web app — never by the voice agent, which would
    /// otherwise be renaming the things it is also reporting on.
    ///
    /// `None` means "no alias", which is not the same as an empty one: an empty alias is refused
    /// at the door, and clearing an alias is a separate act that puts [`ChannelInfo::label`] back.
    ///
    /// It is `None` on a channel that came straight from [`crate::config::Config`]; the store
    /// overlay in [`crate::ops`] is what fills it in, so anything reading configuration directly
    /// — the startup banner, the preflight probe — still names the label the operator wrote.
    #[serde(default)]
    pub alias: Option<String>,
    /// Whether this channel was added from inside the app rather than named in the configuration
    /// file.
    ///
    /// The page needs it to say what Remove will do. Removing an added channel forgets it. A
    /// configured one comes from a file this server reads and never writes, so removing it HIDES
    /// it instead — the file still names it, and Settings can show it again. `#199
    /// removable-config-channels`.
    #[serde(default)]
    pub added: bool,
    /// Key of the configured chat provider this channel is read through.
    ///
    /// A deployment may serve several providers at once, and the page needs to know which one a
    /// channel belongs to in order to describe it and to use only that provider's capabilities.
    /// Absent only from an older server, which had exactly one provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

impl ChannelInfo {
    /// The name to show and to say aloud.
    ///
    /// The alias when the operator set one, the configured label otherwise. Every place that
    /// renders a channel's name for a person or hands it to a model goes through here, so
    /// "the name the owner says out loud" and "the name the model was given" cannot drift.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str) -> Message {
        Message {
            thread: None,
            id: MessageId(id.to_owned()),
            channel_id: ChannelId("c".to_owned()),
            author: "a".to_owned(),
            author_id: UserId("7".to_owned()),
            author_is_bot: false,
            timestamp: "t".to_owned(),
            spoken_time: String::new(),
            reply_to: None,
            content: String::new(),
            spoken_content: String::new(),
            content_html: String::new(),
            noise: false,
            reactions: None,
            heading: None,
        }
    }

    #[test]
    fn reactions_keep_absent_and_empty_apart_on_the_wire() {
        // `#219 emoji-reactions`. Absent is "this copy cannot see reactions", empty is "there are
        // none", and a reader acts differently on the two, so neither may turn into the other.
        let mut message = msg("7");
        let absent = serde_json::to_value(&message).expect("serializes");
        assert!(absent.get("reactions").is_none(), "{absent}");
        message.reactions = Some(Vec::new());
        let empty = serde_json::to_value(&message).expect("serializes");
        assert_eq!(empty["reactions"], serde_json::json!([]));
        message.reactions = Some(vec![
            Reaction {
                emoji: "👀".to_owned(),
                custom: false,
                custom_id: None,
                count: 1,
            },
            Reaction {
                emoji: "party-parrot".to_owned(),
                custom: true,
                custom_id: Some("112233".to_owned()),
                count: 2,
            },
        ]);
        let full = serde_json::to_value(&message).expect("serializes");
        assert_eq!(
            full["reactions"],
            serde_json::json!([
                {"emoji": "👀", "count": 1},
                {"emoji": "party-parrot", "custom": true, "custom_id": "112233", "count": 2},
            ])
        );
        for value in [absent, empty, full] {
            let back: Message = serde_json::from_value(value.clone()).expect("round trips");
            assert_eq!(serde_json::to_value(&back).expect("serializes"), value);
        }
    }

    #[test]
    fn snowflakes_sort_numerically_not_lexically() {
        // Lexically "1000000000000000000" < "999999999999999999"; chronologically it is later.
        let mut messages = vec![msg("1000000000000000000"), msg("999999999999999999")];
        sort_oldest_first(&mut messages);
        assert_eq!(
            messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["999999999999999999", "1000000000000000000"],
            "sort_oldest_first must order by snowflake value, not by string"
        );
    }

    #[test]
    fn unparseable_ids_sort_last_without_dropping_messages() {
        let mut messages = vec![msg("not-a-snowflake"), msg("42"), msg("7")];
        sort_oldest_first(&mut messages);
        assert_eq!(
            messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["7", "42", "not-a-snowflake"]
        );
    }

    #[test]
    fn a_mention_is_the_syntax_discord_actually_notifies_on() {
        // The whole point of carrying the id: "@coding_agent" is plain text and pings nobody.
        assert_eq!(
            UserId("1532416065114607829".to_owned()).mention(),
            "<@1532416065114607829>"
        );
    }

    #[test]
    fn an_unstamped_message_still_reports_a_time_rather_than_a_blank() {
        let mut message = msg("7");
        message.timestamp = "2026-08-19T13:51:25+00:00".to_owned();
        assert_eq!(message.spoken(), "2026-08-19T13:51:25+00:00");
        message.spoken_time = "09:51".to_owned();
        assert_eq!(
            message.spoken(),
            "09:51",
            "once stamped, the spoken form is what a renderer must use"
        );
    }

    #[test]
    fn the_operators_own_name_wins_and_clearing_it_returns_the_configured_label() {
        // `#39 channel-alias`. Both directions, because "the alias wins" and "the label is the
        // fallback" are two claims and a broken implementation satisfies one of them.
        let mut channel = ChannelInfo {
            id: ChannelId("1532416065114607829".to_owned()),
            label: "build noise".to_owned(),
            writable: false,
            alias: None,
            added: false,
            provider: None,
        };
        assert_eq!(
            channel.display_name(),
            "build noise",
            "with no alias set, the configured label is the name"
        );
        channel.alias = Some("the build channel".to_owned());
        assert_eq!(
            channel.display_name(),
            "the build channel",
            "an alias the operator set must beat the configured label"
        );
        channel.alias = None;
        assert_eq!(
            channel.display_name(),
            "build noise",
            "clearing the alias must put the configured label back"
        );
    }

    #[test]
    fn a_channel_from_configuration_deserializes_without_an_alias_rather_than_failing() {
        // Configuration and the older shape of this struct carry no `alias` at all. It has to
        // read back as "no alias", not as a parse error.
        let channel: ChannelInfo =
            serde_json::from_str(r#"{"id":"1111111111","label":"build noise","writable":false}"#)
                .expect("a channel with no alias field still parses");
        assert_eq!(channel.alias, None);
        assert_eq!(channel.display_name(), "build noise");
    }

    #[test]
    fn numeric_rejects_non_numeric_snowflakes() {
        assert_eq!(MessageId("12".to_owned()).numeric(), Some(12));
        assert_eq!(MessageId("12x".to_owned()).numeric(), None);
    }

    #[test]
    fn a_known_snowflake_round_trips_to_its_known_millisecond() {
        // Discord's own documented worked example: 175928847299117063 was created at
        // 2016-04-30T11:18:25.796Z, which is 1462015105796 ms since the Unix epoch.
        let id = MessageId("175928847299117063".to_owned());
        assert_eq!(id.created_at_ms(), Some(1_462_015_105_796));
        assert_eq!(
            MessageId("not-a-snowflake".to_owned()).created_at_ms(),
            None,
            "an unparseable id has no creation time, and must not be given a plausible one"
        );
    }

    #[test]
    fn a_time_boundary_sorts_strictly_between_the_ids_on_either_side_of_it() {
        // This is the whole mechanism behind a time range: an instant becomes a cursor that
        // Discord's own `before`/`after` comparison places correctly among real ids.
        let earlier = MessageId("175928847299117063".to_owned());
        let earlier_ms = earlier.created_at_ms().expect("real snowflake");
        let later = MessageId::at_time_ms(earlier_ms + 60_000);
        let boundary = MessageId::at_time_ms(earlier_ms + 1);

        let n = |id: &MessageId| id.numeric().expect("numeric");
        assert!(
            n(&earlier) < n(&boundary),
            "the boundary must exclude what came before it"
        );
        assert!(n(&boundary) < n(&later), "and include what came after it");
        assert_eq!(
            boundary.created_at_ms(),
            Some(earlier_ms + 1),
            "a boundary must land on the exact millisecond it was asked for"
        );
    }

    #[test]
    fn an_instant_before_discord_existed_clamps_instead_of_wrapping() {
        // Unsigned arithmetic on a pre-2015 instant would otherwise produce an enormous id that
        // sorts AFTER everything, silently turning "since forever" into "since the far future".
        let ancient = MessageId::at_time_ms(0);
        assert_eq!(ancient.as_str(), "0");
        assert_eq!(MessageId::at_time_ms(-1_000).as_str(), "0");
    }
}
