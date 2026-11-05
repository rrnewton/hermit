//! Messages that are read automatically: the owner's noise rules. `#196 auto-read-noise`.
//!
//! A coding agent posts `_Working…_` as a thread reply the moment it starts on something, and the
//! real answer arrives later as a separate message. In the owner's main space nine of the latest
//! fifty messages were exactly that placeholder, and every one of them sat in the to-do list, got
//! an unread row, and was counted in every digest as though it were news. This module is how a
//! message like that is treated as already read without anybody tapping Done on it.
//!
//! # Read state is still OURS
//!
//! "Read" here is this server's own local read state — the same overlay `#50 todo-view` and
//! `#61 unread-status` describe in [`crate::store`]. Nothing here moves the source chat service's
//! own read cursor, and nothing here is written to it. The upstream-read route is a different act
//! and this module never reaches it.
//!
//! # Evaluated, never recorded
//!
//! The obvious implementation — dismiss every matching message as it arrives — is the wrong one,
//! for three reasons that each turn into a bug the owner cannot diagnose:
//!
//! * **An edit would not bring the message back.** Some agents edit the placeholder into the real
//!   answer. A dismissal written when the text said "Working…" would keep the finished answer out
//!   of the list forever.
//! * **Removing a rule would not undo it.** Every message the rule ever caught would stay
//!   dismissed, so a mistaken rule could only be cleaned up message by message.
//! * **It would be a write on a read.** Matching happens while serving a read, and a read-scope
//!   caller never writes anything durable here; see [`crate::http`].
//!
//! So a rule is applied to the message's CURRENT text every time a message is served, and the
//! answer rides on the message as [`crate::model::Message::noise`]. The page, the to-do list, the
//! counts, the digest and the agent's tools all read that one flag, so there is one predicate and
//! it lives here — the page deliberately has no JavaScript copy of the matcher.
//!
//! # Deliberately trivial
//!
//! Not a regular expression. A rule matches a message when the message's WHOLE text, normalised,
//! equals the rule, normalised the same way — see [`normalise`]. A rule ending in `*` matches the
//! START of a message instead. That is the whole language, and it is small on purpose: a rule
//! hides messages from the person it is for, so it has to be possible to predict exactly what one
//! will catch by reading it. A regex whose author got it slightly wrong hides real messages
//! silently, which is the one failure this feature must not have.
//!
//! # A failure shows more, never less
//!
//! When the rules or the exemptions cannot be read, nothing is treated as noise: the reader sees
//! the placeholders exactly as he did before this existed. The opposite fallback would hide
//! messages on the strength of a store that is not answering.

use std::collections::HashSet;

use crate::model::{ChannelId, Message, MessageId};
use crate::state::AppState;
use crate::store::StoreError;

/// The rules a deployment starts with, until the owner saves a list of his own.
///
/// One rule, from the evidence the owner pointed at: the placeholder a coding agent posts while it
/// works. Written with the real ellipsis because that is what the agent sends; [`normalise`]
/// makes `Working...` match it too.
pub const DEFAULT_RULES: &[&str] = &["Working…"];

/// The most rules the server will keep.
///
/// Every served message is compared against every rule, and the list is shown whole in Settings.
/// Fifty is far more than a person curates by hand and still a fixed ceiling on both costs.
pub const MAX_RULES: usize = 50;

/// The longest rule, in characters.
///
/// A rule is a short placeholder phrase. A paragraph-long rule would be a way of hiding one
/// specific real message, which is what Done is for.
pub const MAX_RULE_CHARS: usize = 200;

/// The one-line statement of how a rule matches, shown where the owner edits the list.
///
/// The server's sentence, quoted by the page rather than restated there — the same rule
/// `ALIAS_NOTICE` and `INBOX_NOTICE` follow — so the description cannot drift from the matcher it
/// describes, which is the code just below it.
pub const MATCHING_RULE: &str = "A message counts as read when its whole text is one of these. \
                                 Case, spacing, surrounding _ * ~ emphasis, and \"...\" versus \
                                 \"…\" are ignored; end a rule with * to match how a message \
                                 starts. This is vibe-talk's own read state; the chat service \
                                 is not told.";

/// The markdown emphasis characters stripped from both ends before comparing.
const EMPHASIS: [char; 3] = ['_', '*', '~'];

/// The text a rule and a message are compared as.
///
/// In order: surrounding whitespace and emphasis markers off (`_Working…_` and `**Working…**`
/// are the same placeholder), every run of whitespace collapsed to one space, lowercased, and the
/// one-character ellipsis spelled as three dots so `Working…` and `Working...` agree.
///
/// Emphasis is stripped from the ENDS only, alternating with whitespace until neither is left —
/// `_ Working… _` is the placeholder too. A marker in the middle of the text is part of the text.
#[must_use]
pub fn normalise(text: &str) -> String {
    let stripped = text.trim_matches(|c: char| c.is_whitespace() || EMPHASIS.contains(&c));
    stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .replace('…', "...")
}

/// One rule, ready to compare.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    /// The normalised text.
    key: String,
    /// Whether the rule matches the start of a message rather than the whole of it.
    prefix: bool,
}

impl Rule {
    /// Read one rule as the owner wrote it, or `None` when nothing is left to compare.
    ///
    /// A trailing `*` makes it a prefix rule — UNLESS the rule also starts with `*`, in which case
    /// the pair is emphasis and is stripped like any other. Without that exception, pasting
    /// `**Done**` from a chat app would quietly become "anything starting with done", which
    /// catches real messages; with it, a prefix rule is always one somebody meant.
    fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let (body, prefix) = match trimmed.strip_suffix('*') {
            Some(rest) if !trimmed.starts_with('*') => (rest, true),
            _ => (trimmed, false),
        };
        let key = normalise(body);
        (!key.is_empty()).then_some(Self { key, prefix })
    }

    fn matches(&self, normalised: &str) -> bool {
        if self.prefix {
            normalised.starts_with(&self.key)
        } else {
            normalised == self.key
        }
    }
}

/// A list of rules, compiled once per read.
#[derive(Clone, Debug, Default)]
pub struct Matcher {
    rules: Vec<Rule>,
}

impl Matcher {
    /// Compile the rules. One that has nothing left once normalised is skipped rather than
    /// matched: the store refuses those at the door, so meeting one here means a row written by
    /// some other version, and the safe reading of it is "matches nothing".
    #[must_use]
    pub fn new<S: AsRef<str>>(rules: &[S]) -> Self {
        Self {
            rules: rules
                .iter()
                .filter_map(|rule| Rule::parse(rule.as_ref()))
                .collect(),
        }
    }

    /// Whether `content` is noise under these rules.
    ///
    /// An empty message — an attachment with no text, say — is never noise: no rule can be empty,
    /// so there is nothing for it to equal, and "no text" is not the same as "a placeholder".
    #[must_use]
    pub fn matches(&self, content: &str) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let text = normalise(content);
        !text.is_empty() && self.rules.iter().any(|rule| rule.matches(&text))
    }
}

/// Check an owner-supplied list of rules, returning it as it will be stored.
///
/// Each rule is trimmed and kept as typed, not normalised: the list is shown back to the owner,
/// and his own spelling is what he will recognise. A rule must be non-empty, at most
/// [`MAX_RULE_CHARS`] characters, free of control characters, and have something left besides
/// markup once normalised — a bare `*` would match every message in every channel. A second rule
/// that normalises to the same thing as an earlier one is dropped, so adding `working...` beside
/// `Working…` changes nothing rather than listing one placeholder twice. An empty list is fine:
/// it is how the owner turns the feature off.
///
/// # Errors
///
/// [`StoreError::BadId`] naming what was wrong, or [`StoreError::TooLarge`] for more than
/// [`MAX_RULES`] rules.
pub fn validate_rules<S: AsRef<str>>(raw: &[S]) -> Result<Vec<String>, StoreError> {
    if raw.len() > MAX_RULES {
        return Err(StoreError::TooLarge(format!(
            "at most {MAX_RULES} noise rules are kept"
        )));
    }
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for rule in raw {
        let trimmed = rule.as_ref().trim();
        if trimmed.is_empty() {
            return Err(StoreError::BadId(
                "a noise rule must not be blank".to_owned(),
            ));
        }
        if trimmed.chars().count() > MAX_RULE_CHARS {
            return Err(StoreError::BadId(format!(
                "a noise rule must be at most {MAX_RULE_CHARS} characters"
            )));
        }
        if trimmed.chars().any(char::is_control) {
            return Err(StoreError::BadId(
                "a noise rule must not contain control characters".to_owned(),
            ));
        }
        let Some(parsed) = Rule::parse(trimmed) else {
            return Err(StoreError::BadId(format!(
                "the noise rule {trimmed:?} has no words left once emphasis and `*` are set \
                 aside, so it would match every message"
            )));
        };
        if seen.insert((parsed.key, parsed.prefix)) {
            kept.push(trimmed.to_owned());
        }
    }
    Ok(kept)
}

/// The longest message id an exemption will hold, in bytes.
///
/// The same ceiling the live-ingest route puts on a message id. An exemption names a message the
/// authenticated page was shown, so anything longer is not one.
pub const MAX_EXEMPT_ID_BYTES: usize = 512;

/// Check the ids of a "not noise" request before anything is stored.
///
/// Unlike a dismissal these are not required to be snowflakes: nothing orders them, and the list
/// is only ever consulted by exact id.
///
/// # Errors
///
/// [`StoreError::BadId`] for an empty, oversized or control-character id.
pub fn validate_exempt_ids(ids: &[MessageId]) -> Result<(), StoreError> {
    for id in ids {
        let raw = id.as_str();
        if raw.is_empty() || raw.len() > MAX_EXEMPT_ID_BYTES || raw.chars().any(char::is_control) {
            return Err(StoreError::BadId(format!(
                "{:?} is not a message id this server could have served",
                raw.chars().take(40).collect::<String>()
            )));
        }
    }
    Ok(())
}

/// The rules in force: the owner's saved list, or [`DEFAULT_RULES`] until he has saved one.
///
/// A deployment with no store configured runs on the defaults too — it cannot keep an edit, but
/// the placeholder is still noise there.
///
/// # Errors
///
/// [`StoreError`] when a configured store cannot be read. [`StoreError::Unavailable`] is NOT an
/// error here; see above.
pub async fn current_rules(state: &AppState) -> Result<Vec<String>, StoreError> {
    match state.store.noise_rules().await {
        Ok(Some(saved)) => Ok(saved),
        Ok(None) | Err(StoreError::Unavailable(_)) => Ok(defaults()),
        Err(error) => Err(error),
    }
}

/// [`DEFAULT_RULES`], owned.
#[must_use]
pub fn defaults() -> Vec<String> {
    DEFAULT_RULES
        .iter()
        .map(|rule| (*rule).to_owned())
        .collect()
}

/// What the owner's rules say about one channel's messages, right now.
///
/// Built once per read and applied to every message the read serves. Exempted messages — the
/// owner's "Not noise" on a false positive — are never noise, whatever the rules say.
#[derive(Clone, Debug, Default)]
pub struct NoiseFilter {
    matcher: Matcher,
    exempt: HashSet<String>,
}

impl NoiseFilter {
    /// A filter that calls nothing noise. What a read gets when the store cannot say.
    #[must_use]
    pub fn nothing() -> Self {
        Self::default()
    }

    /// A filter over explicit rules and exemptions, for callers that already hold both.
    #[must_use]
    pub fn new<S: AsRef<str>>(rules: &[S], exempt: &[MessageId]) -> Self {
        Self {
            matcher: Matcher::new(rules),
            exempt: exempt.iter().map(|id| id.as_str().to_owned()).collect(),
        }
    }

    /// Whether this message, as it reads NOW, is noise.
    #[must_use]
    pub fn is_noise(&self, message: &Message) -> bool {
        !self.exempt.contains(message.id.as_str()) && self.matcher.matches(&message.content)
    }

    /// Set [`Message::noise`] on every message, in both directions.
    ///
    /// Both directions matters: a message pushed by an adapter, or one read back from somewhere
    /// that already carried the flag, is re-decided here rather than trusted. What a client sent
    /// is not what this server's rules say.
    pub fn mark(&self, messages: &mut [Message]) {
        for message in messages {
            message.noise = self.is_noise(message);
        }
    }
}

/// The filter for one channel: the rules in force plus that channel's exemptions.
///
/// Never fails. A store that is not configured yields the default rules and no exemptions; a
/// store that fails yields a filter that calls nothing noise, and says so in the log — see the
/// module documentation for why that is the direction to fail in.
pub async fn filter_for(state: &AppState, channel: &ChannelId) -> NoiseFilter {
    let rules = match current_rules(state).await {
        Ok(rules) => rules,
        Err(error) => {
            tracing::warn!(%error, "could not read the noise rules; treating nothing as noise");
            return NoiseFilter::nothing();
        }
    };
    if rules.is_empty() {
        return NoiseFilter::nothing();
    }
    let exempt = match state.store.noise_exemptions(channel).await {
        Ok(ids) => ids,
        Err(StoreError::Unavailable(_)) => Vec::new(),
        Err(error) => {
            // Not "the rules without the exemptions": that would hide again exactly the messages
            // the owner said were not noise.
            tracing::warn!(%error, "could not read the noise exemptions; treating nothing as noise");
            return NoiseFilter::nothing();
        }
    };
    NoiseFilter::new(&rules, &exempt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_matcher() -> Matcher {
        Matcher::new(DEFAULT_RULES)
    }

    fn message(id: &str, content: &str) -> Message {
        Message {
            thread: None,
            id: MessageId(id.to_owned()),
            channel_id: ChannelId("1".to_owned()),
            author: "an agent".to_owned(),
            author_id: crate::model::UserId("9".to_owned()),
            author_is_bot: true,
            timestamp: "2026-10-04T10:00:00+00:00".to_owned(),
            spoken_time: String::new(),
            reply_to: None,
            content: content.to_owned(),
            spoken_content: String::new(),
            content_html: String::new(),
            noise: false,
            reactions: None,
            heading: None,
        }
    }

    #[test]
    fn the_placeholder_as_the_agent_really_sends_it_is_noise() {
        // Quoted from the evidence: markdown italics around the word and a real ellipsis.
        assert!(default_matcher().matches("_Working…_"));
    }

    #[test]
    fn three_dots_and_the_ellipsis_character_are_the_same_placeholder() {
        let matcher = default_matcher();
        assert!(matcher.matches("Working..."));
        assert!(matcher.matches("_Working..._"));
        // ...and the other way round: a rule typed with three dots catches the real character.
        assert!(Matcher::new(&["Working..."]).matches("_Working…_"));
    }

    #[test]
    fn emphasis_case_and_spacing_do_not_matter() {
        let matcher = default_matcher();
        for same in [
            "*Working…*",
            "**Working…**",
            "~~Working…~~",
            "  working…  ",
            "WORKING…",
            "_ Working… _",
            "\n_Working…_\n",
        ] {
            assert!(matcher.matches(same), "{same:?} is the same placeholder");
        }
        // Collapsing, not deleting: inner whitespace is one space, still a space.
        assert!(Matcher::new(&["still   working…"]).matches("Still working…"));
        assert!(!Matcher::new(&["still working…"]).matches("Stillworking…"));
    }

    #[test]
    fn a_real_message_that_merely_starts_with_the_placeholder_is_not_noise() {
        // The whole point of comparing the WHOLE text. The answer often opens the same way.
        let matcher = default_matcher();
        for real in [
            "Working… done. The migration ran cleanly on all three shards.",
            "_Working…_ here is what I found: the cache key ignored the region.",
            "Working",
            "Working on the flaky test now, will report back in ten minutes.",
            "I am still Working…",
        ] {
            assert!(!matcher.matches(real), "{real:?} is a real message");
        }
    }

    #[test]
    fn a_trailing_star_makes_a_prefix_rule() {
        let matcher = Matcher::new(&["Working*"]);
        assert!(matcher.matches("Working on the flaky test now"));
        assert!(matcher.matches("_working…_"));
        assert!(
            !matcher.matches("I am working"),
            "a prefix rule matches the START only"
        );
    }

    #[test]
    fn a_star_that_closes_emphasis_is_emphasis_not_a_prefix() {
        // Pasting `**Done**` from a chat app must not become "anything starting with done".
        let matcher = Matcher::new(&["**Done**"]);
        assert!(matcher.matches("Done"));
        assert!(matcher.matches("_done_"));
        assert!(
            !matcher.matches("Done with the migration; results below."),
            "a bold rule was read as a prefix rule and caught a real message"
        );
    }

    #[test]
    fn no_rules_and_empty_messages_match_nothing() {
        assert!(!Matcher::new::<&str>(&[]).matches("Working…"));
        assert!(!default_matcher().matches(""));
        assert!(!default_matcher().matches("   "));
        assert!(!Matcher::new(&["Working*"]).matches("__"));
    }

    #[test]
    fn validation_keeps_the_owners_spelling_and_drops_duplicates() {
        let kept = validate_rules(&["  Working…  ", "working...", "_WORKING…_", "Thinking…"])
            .expect("ordinary rules are fine");
        assert_eq!(kept, vec!["Working…".to_owned(), "Thinking…".to_owned()]);
        // A prefix rule is a different rule from the exact one, so both stay.
        let kept = validate_rules(&["Working…", "Working…*"]).expect("valid");
        assert_eq!(kept.len(), 2);
        assert_eq!(
            validate_rules::<&str>(&[])
                .expect("empty is how it is turned off")
                .len(),
            0
        );
    }

    #[test]
    fn validation_refuses_a_rule_that_would_match_everything() {
        for hostile in ["*", "**", "_*_", "~~", "   ", "_ _"] {
            let error = validate_rules(&[hostile]).expect_err("must refuse");
            assert_eq!(error.code(), "bad_id", "{hostile:?}: {error}");
        }
    }

    #[test]
    fn validation_bounds_the_list_and_each_rule() {
        let too_many: Vec<String> = (0..=MAX_RULES).map(|n| format!("rule {n}")).collect();
        assert_eq!(
            validate_rules(&too_many).expect_err("too many").code(),
            "too_large"
        );
        assert!(validate_rules(&["a".repeat(MAX_RULE_CHARS)]).is_ok());
        assert_eq!(
            validate_rules(&["a".repeat(MAX_RULE_CHARS + 1)])
                .expect_err("too long")
                .code(),
            "bad_id"
        );
        assert_eq!(
            validate_rules(&["Working\n…"]).expect_err("control").code(),
            "bad_id"
        );
    }

    #[test]
    fn an_exemption_wins_over_a_matching_rule() {
        let mut placeholder = message("7", "_Working…_");
        let filter = NoiseFilter::new(DEFAULT_RULES, &[]);
        assert!(filter.is_noise(&placeholder));
        let rescued = NoiseFilter::new(DEFAULT_RULES, &[MessageId("7".to_owned())]);
        assert!(!rescued.is_noise(&placeholder));
        // `mark` decides in both directions, so a flag a client sent is not trusted.
        placeholder.noise = true;
        rescued.mark(std::slice::from_mut(&mut placeholder));
        assert!(!placeholder.noise);
    }

    #[test]
    fn exemption_ids_are_checked_but_not_required_to_be_snowflakes() {
        assert!(validate_exempt_ids(&[MessageId("spaces/AAA/messages/BBB".to_owned())]).is_ok());
        for bad in [
            String::new(),
            "x".repeat(MAX_EXEMPT_ID_BYTES + 1),
            "a\nb".to_owned(),
        ] {
            assert_eq!(
                validate_exempt_ids(&[MessageId(bad.clone())])
                    .expect_err("must refuse")
                    .code(),
                "bad_id",
                "{bad:?}"
            );
        }
    }
}
