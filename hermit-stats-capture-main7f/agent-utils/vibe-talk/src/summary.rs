//! Turning one verbose agent message into one speakable line.
//!
//! **This is the digest-and-preview helper, not a summariser.** It is deliberately extractive and
//! deterministic — it truncates and flattens, with no model call, no network and no cost — and
//! that is all it claims: making a channel *listing* short enough to be spoken. Real
//! comprehension lives in [`crate::summarize`], which asks the deployment's ElevenLabs agent.
//!
//! Do not delete this module along with the extractive *summariser* that was removed from
//! [`crate::summarize`]. [`condense`] has three live callers, and one of them is
//! [`crate::summarize::agent::plain`] — the thing that flattens the AGENT'S OWN reply before it
//! reaches a page. Taking it away takes the shipped summariser's output flattener with it.
//!
//! The related-work review found that a shipped product solved the digest problem with a terse
//! system prompt rather than a summarization pipeline. When the owner wants the real thing, he
//! asks for the full message and gets it verbatim.

use serde::Serialize;

use crate::model::{Message, UserId};

/// Default width of a spoken digest line.
pub const DEFAULT_SUMMARY_CHARS: usize = 160;

/// What [`condense`] says when there is nothing speakable left.
///
/// A placeholder is right for a digest line — the message exists and has no words in it, and the
/// listener should hear that. It is NOT right for anything that paid a vendor for an answer, which
/// is why the constant is public: a caller that must tell "the model said nothing" apart from "the
/// model said something" has to be able to recognise this without repeating the literal.
pub const NO_TEXT: &str = "(no text)";

/// One entry of a channel digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DigestEntry {
    /// Snowflake, so the caller can ask for this exact message in full.
    pub id: String,
    /// Author display name.
    pub author: String,
    /// Author snowflake, so a caller can mention them with `<@id>` without a lookup step.
    /// See [`crate::model::Message::author_id`] for why this rides along instead.
    pub author_id: UserId,
    /// The EXACT instant, ISO-8601, as Discord reported it. Compute with this one.
    pub timestamp: String,
    /// The same instant in the operator's zone, already formatted for speech. READ THIS ONE ALOUD.
    /// Copied from [`crate::model::Message::spoken_time`], which explains why there are two.
    pub spoken_time: String,
    /// One speakable line. UNTRUSTED text, condensed.
    pub summary: String,
    /// Character length of the full message, so the caller can tell "short" from "a wall".
    pub full_length: usize,
    /// Whether the summary dropped anything.
    pub truncated: bool,
}

/// Condense a message body to at most `max_chars` characters of speakable text.
///
/// Fenced code, inline code, and URLs are replaced with short placeholders, because reading them
/// aloud is never what was wanted. The text that remains is the author's, unaltered.
#[must_use]
pub fn condense(content: &str, max_chars: usize) -> String {
    condense_noting_cut(content, max_chars).0
}

/// [`condense`], and whether it had to cut to fit `max_chars`.
///
/// `#208 persistent-cache`: a stored summary records the length it was cut at, so a later, larger
/// cap can tell an answer it holds whole from one it holds only the start of.
#[must_use]
pub fn condense_noting_cut(content: &str, max_chars: usize) -> (String, bool) {
    let flattened = flatten(content);
    if flattened.is_empty() {
        return (NO_TEXT.to_owned(), false);
    }
    let cut = flattened.chars().count() > max_chars;
    (truncate_on_word_boundary(&flattened, max_chars), cut)
}

/// Build a digest entry for one message.
///
/// The summary is condensed from [`Message::spoken_body`], not from the raw body. A digest exists
/// to be read out, and the prepared body is the one with the markdown off and the snowflakes named
/// — condensing the raw one put a nineteen-digit id into the very line the agent was told to say
/// aloud. `full_length` still measures the RAW body, because what it answers is "how much message
/// is there", and that is a fact about what was written rather than about how it will be said.
#[must_use]
pub fn digest_entry(message: &Message, max_chars: usize) -> DigestEntry {
    let summary = condense(message.spoken_body(), max_chars);
    DigestEntry {
        id: message.id.0.clone(),
        author: message.author.clone(),
        author_id: message.author_id.clone(),
        timestamp: message.timestamp.clone(),
        spoken_time: message.spoken().to_owned(),
        truncated: summary.ends_with('…'),
        summary,
        full_length: message.content.chars().count(),
    }
}

/// Build a digest for a batch of messages, in the order given.
#[must_use]
pub fn digest(messages: &[Message], max_chars: usize) -> Vec<DigestEntry> {
    messages
        .iter()
        .map(|m| digest_entry(m, max_chars))
        .collect()
}

/// Replace code and links with placeholders, then collapse all whitespace to single spaces.
fn flatten(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    // Fenced blocks first: they are the biggest source of unspeakable text.
    while let Some(start) = rest.find("```") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        match after.find("```") {
            Some(end) => {
                out.push_str(" [code] ");
                rest = &after[end + 3..];
            }
            None => {
                // Unterminated fence: treat the remainder as code rather than reading it aloud.
                out.push_str(" [code] ");
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);

    let mut result = String::with_capacity(out.len());
    for word in out.split_whitespace() {
        // Backticks off BEFORE the link test, so `https://...` in inline code is a link too. The
        // other order kept that URL and let a second pass replace it, and `#208 persistent-cache`
        // stores a flattened answer and cuts it again when serving: condensing twice must give
        // what condensing once does.
        let cleaned = word.trim_matches('`');
        let cleaned = if cleaned.starts_with("http://") || cleaned.starts_with("https://") {
            "[link]"
        } else {
            cleaned
        };
        if cleaned.is_empty() {
            continue;
        }
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(cleaned);
    }
    result
}

fn truncate_on_word_boundary(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return "…".to_owned();
    }
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    // Reserve one character for the ellipsis.
    let budget = max_chars - 1;
    let mut kept = String::new();
    for word in text.split(' ') {
        let extra = if kept.is_empty() { 0 } else { 1 };
        if kept.chars().count() + extra + word.chars().count() > budget {
            break;
        }
        if extra == 1 {
            kept.push(' ');
        }
        kept.push_str(word);
    }
    if kept.is_empty() {
        // One enormous word: cut it mid-word rather than returning nothing.
        kept = text.chars().take(budget).collect();
    }
    kept.push('…');
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChannelId, MessageId};

    fn message(content: &str) -> Message {
        Message {
            thread: None,
            id: MessageId("1000000000000000001".to_owned()),
            channel_id: ChannelId("c".to_owned()),
            author: "coder-bot".to_owned(),
            author_id: UserId("2000000000000000001".to_owned()),
            author_is_bot: true,
            timestamp: "2026-08-18T12:00:00+00:00".to_owned(),
            spoken_time: "08:00".to_owned(),
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
    fn a_short_message_is_returned_verbatim() {
        assert_eq!(condense("build is green", 160), "build is green");
    }

    #[test]
    fn fenced_code_is_replaced_not_read_aloud() {
        let summary = condense("here is the fix:\n```rust\nfn main() {}\n```\nlanded", 160);
        assert_eq!(summary, "here is the fix: [code] landed");
        assert!(!summary.contains("fn main"));
    }

    #[test]
    fn an_unterminated_fence_does_not_leak_the_rest_of_the_message() {
        let summary = condense("look:\n```\nsecrets and noise\nmore noise", 160);
        assert!(
            !summary.contains("noise"),
            "unterminated fence leaked: {summary}"
        );
        assert!(summary.contains("[code]"));
    }

    #[test]
    fn urls_become_a_placeholder() {
        let summary = condense(
            "see https://github.com/rrnewton/agent-utils/pull/12 for detail",
            160,
        );
        assert_eq!(summary, "see [link] for detail");
    }

    #[test]
    fn newlines_and_runs_of_space_collapse() {
        assert_eq!(condense("one\n\n   two\tthree", 160), "one two three");
    }

    #[test]
    fn empty_content_is_labelled_not_blank() {
        assert_eq!(condense("   \n  ", 160), "(no text)");
    }

    #[test]
    fn long_text_is_cut_at_a_word_boundary_within_budget() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let summary = condense(text, 20);
        assert!(summary.chars().count() <= 20, "over budget: {summary:?}");
        assert!(summary.ends_with('…'), "no ellipsis: {summary:?}");
        assert_eq!(summary, "alpha bravo charlie…");
        assert!(
            !summary.contains("charli…"),
            "truncation must not split a word: {summary:?}"
        );
    }

    #[test]
    fn a_single_enormous_word_is_still_cut() {
        let summary = condense(&"x".repeat(100), 10);
        assert_eq!(summary.chars().count(), 10);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn digest_entry_reports_the_full_length_and_truncation_flag() {
        let long = "word ".repeat(200);
        let entry = digest_entry(&message(&long), 40);
        assert!(entry.truncated);
        assert_eq!(entry.full_length, long.chars().count());
        assert_eq!(entry.author, "coder-bot");
        assert_eq!(entry.id, "1000000000000000001");
        assert_eq!(
            entry.author_id.as_str(),
            "2000000000000000001",
            "a digest line must carry the id, or replying by mention needs a lookup"
        );

        let short = digest_entry(&message("done"), 40);
        assert!(!short.truncated);
        assert_eq!(short.summary, "done");
    }

    #[test]
    fn a_digest_line_carries_both_the_spoken_time_and_the_exact_instant() {
        // A digest is what a voice agent reads out, so the speakable form has to survive the
        // condensing step — and the exact instant has to survive alongside it, unrounded.
        let entry = digest_entry(&message("done"), 40);
        assert_eq!(entry.spoken_time, "08:00");
        assert_eq!(entry.timestamp, "2026-08-18T12:00:00+00:00");

        let mut unstamped = message("done");
        unstamped.spoken_time = String::new();
        assert_eq!(
            digest_entry(&unstamped, 40).spoken_time,
            "2026-08-18T12:00:00+00:00",
            "an unstamped message must degrade to the raw instant, never to a blank"
        );
    }

    /// A deterministic corpus of awkward texts: words of every length, links bare and in inline
    /// code, fences closed and not, runs of whitespace, markdown, and characters wider than a byte.
    /// No dependency: a xorshift over a fixed vocabulary, so a failure names a reproducible case.
    fn corpus() -> Vec<String> {
        const VOCABULARY: &[&str] = &[
            "a",
            "deploy",
            "the",
            "runner",
            "stalled",
            "overnight",
            "supercalifragilisticexpialidocious-and-then-some-more-letters",
            "https://example.invalid/path?q=1",
            "`https://example.invalid/inline`",
            "`code`",
            "``",
            "`",
            "```",
            "```rust\nfn main() {}\n```",
            "\n",
            "  ",
            "\t",
            "**bold**",
            "…",
            "café",
            "naïve",
            "[link]",
            "(no text)",
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..2_000)
            .map(|_| {
                let words = usize::try_from(next() % 160).expect("small");
                let mut text = String::new();
                for _ in 0..words {
                    let pick = usize::try_from(next()).unwrap_or(0) % VOCABULARY.len();
                    text.push_str(VOCABULARY[pick]);
                    if next() % 3 != 0 {
                        text.push(' ');
                    }
                }
                text
            })
            .collect()
    }

    #[test]
    fn cutting_a_stored_answer_again_gives_what_cutting_it_once_would_have() {
        // `#208 persistent-cache` stores an answer cut at a generous ceiling and cuts it to the cap
        // in force when it is SERVED. That is only sound if the second cut lands exactly where a
        // single cut at the cap would have: otherwise what a reader sees would depend on whether
        // the answer came from the model or from the store.
        for text in corpus() {
            for cap in [9_usize, 10, 17, 40, 160, 600, 640] {
                for ceiling in [cap, cap * 2, cap * 4, cap * 16] {
                    assert_eq!(
                        condense(&condense(&text, ceiling), cap),
                        condense(&text, cap),
                        "cap {cap}, ceiling {ceiling}, text {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_cut_is_noted_only_when_something_was_cut() {
        assert_eq!(
            condense_noting_cut("build is green", 14),
            ("build is green".to_owned(), false)
        );
        let (cut, noted) = condense_noting_cut("build is green", 13);
        assert!(noted, "{cut:?}");
        assert!(cut.ends_with('…'), "{cut:?}");
        // Flattening is not cutting: a link replaced by its placeholder is not a shortened answer.
        assert_eq!(
            condense_noting_cut("see https://example.invalid/a/very/long/path", 14),
            ("see [link]".to_owned(), false)
        );
        assert_eq!(condense_noting_cut("  ", 1), (NO_TEXT.to_owned(), false));
    }

    #[test]
    fn a_link_in_inline_code_is_a_link() {
        assert_eq!(
            condense("see `https://example.invalid/x` now", 160),
            "see [link] now"
        );
    }

    #[test]
    fn digest_preserves_input_order_and_length() {
        let messages = vec![message("first"), message("second")];
        let entries = digest(&messages, 40);
        assert_eq!(
            entries
                .iter()
                .map(|e| e.summary.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }
}
