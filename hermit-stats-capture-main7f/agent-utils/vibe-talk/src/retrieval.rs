//! Semantic random access: "read me the full message about the mac runner".
//!
//! This is the capability the whole project exists for. A phone screen lets you scroll; speech
//! does not, so the way to reach one message out of forty while driving is to describe it and get
//! it back in full. The API shape is therefore: a digest to hear what is there, and a *resolve*
//! call that turns a spoken description into one specific message.
//!
//! The v0 ranker is lexical — inverse document frequency over the fetched window, plus a phrase
//! bonus and a recency tiebreak. It is naive on purpose and it is behind [`Ranker`], so an
//! embedding-based implementation can replace it without touching the API, the handlers, or the
//! voice agent's tool definitions.
//!
//! Two behaviours matter more than the scoring formula, and both are tested:
//!
//! * a query that matches nothing returns NOTHING, rather than the most recent message wearing a
//!   confident label; and
//! * scoring rewards the rarer term, so "mac runner" beats a message that merely says "deploy"
//!   for the fifth time.

use serde::Serialize;

use crate::model::Message;

/// Words carrying no retrieval signal. Kept short deliberately: an over-eager stop list silently
/// destroys short queries, which are exactly the ones a driver speaks.
const STOPWORDS: &[&str] = &[
    "a", "about", "all", "and", "any", "are", "as", "at", "be", "but", "by", "did", "do", "does",
    "for", "from", "get", "had", "has", "have", "he", "her", "his", "i", "if", "in", "is", "it",
    "its", "just", "me", "my", "not", "of", "on", "or", "our", "out", "she", "so", "that", "the",
    "their", "them", "then", "there", "they", "this", "to", "up", "was", "we", "were", "what",
    "when", "which", "who", "why", "will", "with", "you", "your",
];

/// Score below which a candidate is not considered a match at all.
///
/// Without a floor, every query "succeeds" against the newest message, which is the single most
/// misleading thing a voice interface can do.
pub const MIN_MATCH_SCORE: f64 = 0.25;

/// One ranked candidate.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Candidate {
    /// The message itself, in full.
    pub message: Message,
    /// Relevance score. Only comparable within one call.
    pub score: f64,
    /// Query terms that actually appeared, so a caller can say why this one was chosen.
    pub matched_terms: Vec<String>,
}

/// The answer to a resolve call.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Resolution {
    /// Best match, when anything cleared [`MIN_MATCH_SCORE`].
    pub best: Option<Candidate>,
    /// Runners-up, best first, for "no, the other one".
    pub alternatives: Vec<Candidate>,
    /// True when the top two candidates scored close enough that the caller should disambiguate.
    pub ambiguous: bool,
}

/// A strategy for matching a natural-language description against a window of messages.
///
/// The lexical implementation ships in v0; an embedding-backed one is the intended replacement.
pub trait Ranker: Send + Sync {
    /// Rank `messages` against `query`, best first, dropping anything below the match floor.
    fn rank(&self, messages: &[Message], query: &str) -> Vec<Candidate>;
}

/// Inverse-document-frequency lexical ranking over the fetched window.
#[derive(Debug, Default, Clone, Copy)]
pub struct LexicalRanker;

impl Ranker for LexicalRanker {
    fn rank(&self, messages: &[Message], query: &str) -> Vec<Candidate> {
        let query_terms = tokenize(query);
        if query_terms.is_empty() || messages.is_empty() {
            return Vec::new();
        }
        let documents: Vec<Vec<String>> = messages
            .iter()
            .map(|m| tokenize(&format!("{} {}", m.author, m.content)))
            .collect();
        let total = messages.len() as f64;
        let lowered_query = query.to_lowercase();

        let mut scored: Vec<(usize, Candidate)> = Vec::new();
        for (index, (message, document)) in messages.iter().zip(documents.iter()).enumerate() {
            let mut score = 0.0_f64;
            let mut matched = Vec::new();
            for term in &query_terms {
                let occurrences = document.iter().filter(|t| *t == term).count();
                if occurrences == 0 {
                    continue;
                }
                let document_frequency = documents
                    .iter()
                    .filter(|d| d.iter().any(|t| t == term))
                    .count() as f64;
                // Classic smoothed IDF: a term in every message contributes almost nothing.
                let idf = (1.0 + (total + 1.0) / (document_frequency + 1.0)).ln();
                let term_weight = 1.0 + (occurrences as f64).ln();
                score += idf * term_weight;
                matched.push(term.clone());
            }
            if matched.is_empty() {
                continue;
            }
            // Reward covering more of what was said: two matched terms beat one repeated term.
            score *= matched.len() as f64 / query_terms.len() as f64;
            // A literal phrase hit is strong evidence the speaker meant this one.
            if lowered_query.len() > 3 && message.content.to_lowercase().contains(&lowered_query) {
                score += 1.0;
            }
            // Recency tiebreak, small enough that it never outweighs a real content difference.
            score += 0.001 * (index as f64 + 1.0) / total;
            if score >= MIN_MATCH_SCORE {
                scored.push((
                    index,
                    Candidate {
                        message: message.clone(),
                        score,
                        matched_terms: matched,
                    },
                ));
            }
        }
        // Deterministic: score descending, then most recent, so the same input always answers the
        // same way.
        scored.sort_by(|a, b| {
            b.1.score
                .partial_cmp(&a.1.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.0.cmp(&a.0))
        });
        scored.into_iter().map(|(_, c)| c).collect()
    }
}

/// Resolve a spoken description to one message, with runners-up.
///
/// `max_alternatives` bounds how many runners-up come back; a voice caller wants two or three, the
/// text tab can ask for more.
#[must_use]
pub fn resolve(
    ranker: &dyn Ranker,
    messages: &[Message],
    query: &str,
    max_alternatives: usize,
) -> Resolution {
    let mut ranked = ranker.rank(messages, query);
    if ranked.is_empty() {
        return Resolution {
            best: None,
            alternatives: Vec::new(),
            ambiguous: false,
        };
    }
    let best = ranked.remove(0);
    // "Ambiguous" means the runner-up is within 15% of the winner: close enough that picking
    // silently would be guessing on the owner's behalf.
    let ambiguous = ranked
        .first()
        .is_some_and(|next| next.score >= best.score * 0.85);
    ranked.truncate(max_alternatives);
    Resolution {
        best: Some(best),
        alternatives: ranked,
        ambiguous,
    }
}

/// Lowercase alphanumeric tokens of length >= 2, minus stopwords.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2 && !STOPWORDS.contains(t))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChannelId, MessageId, UserId};

    fn messages(bodies: &[(&str, &str)]) -> Vec<Message> {
        bodies
            .iter()
            .enumerate()
            .map(|(i, (author, content))| Message {
                thread: None,
                id: MessageId(format!("{}", 1_000_000_000_000_000_000_u64 + i as u64)),
                channel_id: ChannelId("c".to_owned()),
                author: (*author).to_owned(),
                author_id: UserId(format!("{}", 2_000_000_000_000_000_000_u64 + i as u64)),
                author_is_bot: true,
                timestamp: format!("2026-08-18T12:{i:02}:00+00:00"),
                spoken_time: format!("08:{i:02}"),
                reply_to: None,
                content: (*content).to_owned(),
                spoken_content: String::new(),
                content_html: String::new(),
                noise: false,
                reactions: None,
                heading: None,
            })
            .collect()
    }

    fn window() -> Vec<Message> {
        messages(&[
            ("codex-eng", "deploy started"),
            ("codex-eng", "deploy still running"),
            ("codex-eng", "deploy finished, deploy was clean"),
            (
                "codex-integ",
                "the mac runner went offline mid-deploy so the arm64 job never reported",
            ),
            ("codex-eng", "opened a pull request for the config change"),
        ])
    }

    #[test]
    fn a_rare_multi_term_query_beats_a_frequently_repeated_word() {
        let window = window();
        let resolution = resolve(&LexicalRanker, &window, "the mac runner going offline", 3);
        let best = resolution.best.expect("something must match");
        assert!(
            best.message.content.contains("mac runner"),
            "picked the wrong message: {:?}",
            best.message.content
        );
        assert!(best.matched_terms.contains(&"mac".to_owned()));
    }

    #[test]
    fn a_query_matching_nothing_resolves_to_nothing() {
        // This is the test that kills a ranker which always returns the newest message.
        let resolution = resolve(
            &LexicalRanker,
            &window(),
            "kubernetes certificate rotation",
            3,
        );
        assert!(
            resolution.best.is_none(),
            "an unmatched query must not be answered with a confident wrong message: {:?}",
            resolution.best.map(|c| c.message.content)
        );
        assert!(resolution.alternatives.is_empty());
    }

    #[test]
    fn a_query_of_only_stopwords_resolves_to_nothing() {
        let resolution = resolve(&LexicalRanker, &window(), "the and of it", 3);
        assert!(resolution.best.is_none());
    }

    #[test]
    fn an_empty_window_resolves_to_nothing() {
        let resolution = resolve(&LexicalRanker, &[], "anything at all", 3);
        assert!(resolution.best.is_none());
    }

    #[test]
    fn the_full_message_comes_back_untruncated() {
        let window = window();
        let resolution = resolve(&LexicalRanker, &window, "arm64 job", 3);
        let best = resolution.best.expect("match");
        assert_eq!(
            best.message.content, window[3].content,
            "resolve must return the message verbatim, not a summary"
        );
    }

    #[test]
    fn author_names_are_searchable() {
        let resolution = resolve(&LexicalRanker, &window(), "codex-integ", 3);
        let best = resolution.best.expect("match");
        assert_eq!(best.message.author, "codex-integ");
    }

    #[test]
    fn ties_prefer_the_more_recent_message() {
        let window = messages(&[
            ("a", "rebase conflict in the lockfile"),
            ("a", "rebase conflict in the lockfile"),
        ]);
        let resolution = resolve(&LexicalRanker, &window, "rebase conflict lockfile", 3);
        let best = resolution.best.expect("match");
        assert_eq!(
            best.message.id.as_str(),
            window[1].id.as_str(),
            "identical candidates must resolve to the newer one"
        );
        assert!(
            resolution.ambiguous,
            "two identical messages must be reported as ambiguous, not silently chosen between"
        );
    }

    #[test]
    fn a_clear_winner_is_not_reported_as_ambiguous() {
        let resolution = resolve(&LexicalRanker, &window(), "mac runner offline arm64", 3);
        assert!(resolution.best.is_some());
        assert!(!resolution.ambiguous);
    }

    #[test]
    fn alternatives_are_bounded_and_ordered() {
        let window = messages(&[
            ("a", "deploy one"),
            ("a", "deploy two"),
            ("a", "deploy three"),
            ("a", "deploy four"),
        ]);
        let resolution = resolve(&LexicalRanker, &window, "deploy", 2);
        assert!(resolution.best.is_some());
        assert_eq!(resolution.alternatives.len(), 2);
        assert!(
            resolution.alternatives[0].score >= resolution.alternatives[1].score,
            "alternatives must be ordered best first"
        );
    }

    #[test]
    fn ranking_is_deterministic() {
        let window = window();
        let first = LexicalRanker.rank(&window, "deploy runner");
        let second = LexicalRanker.rank(&window, "deploy runner");
        assert_eq!(
            first
                .iter()
                .map(|c| c.message.id.clone())
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|c| c.message.id.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn tokenize_drops_stopwords_and_single_characters() {
        assert_eq!(tokenize("The a mac runner!"), vec!["mac", "runner"]);
    }
}
