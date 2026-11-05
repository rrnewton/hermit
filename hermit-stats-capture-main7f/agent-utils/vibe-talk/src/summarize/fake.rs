//! A [`Summarizer`] for tests that counts, records, and can refuse.
//!
//! The three assertions #49 turns on are all about how OFTEN the summariser is called — never for
//! a short message, once for a repeated request, again after the policy or the content changes —
//! and none of them can be made against a backend that does not count. This one does, and it can
//! be made to fail, which is what supplies the controls.

use std::sync::Mutex;

use async_trait::async_trait;

use super::threads::{ThreadNaming, ThreadRequest};
use super::{Summarizer, Summary, SummaryError, SummaryRequest};

/// What one call was asked to summarise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedRequest {
    /// The text of the message it was given.
    pub target: String,
    /// How many context messages rode along.
    pub context: usize,
    /// The width it was asked for.
    pub target_chars: usize,
    /// EXACTLY what a model backend would have sent — [`super::SummaryRequest::prompt`], captured
    /// rather than rebuilt. A test that constructs the prompt itself proves only that the test
    /// can build a fence; this proves the summariser was handed one.
    pub prompt: String,
}

/// A summariser that answers with a fixed shape and remembers being asked.
#[derive(Debug, Default)]
pub struct FakeSummarizer {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    requests: Vec<RecordedRequest>,
    fail_next: Option<String>,
    answer: Option<String>,
    sentence: Option<String>,
    names_threads: bool,
    threads: Vec<String>,
}

impl FakeSummarizer {
    /// A fresh one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times it has actually been asked to produce a summary.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.lock().requests.len()
    }

    /// Everything it was asked, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.lock().requests.clone()
    }

    /// A fake that also names threads, answering `thread-N` and `thread summary #N`.
    ///
    /// Separate from [`FakeSummarizer::new`] on purpose: a backend that names threads makes every
    /// write-scope thread-list read queue background work that reads the chat provider, and the
    /// many tests that count provider reads must not inherit that.
    #[must_use]
    pub fn naming_threads() -> Self {
        let fake = Self::default();
        fake.lock().names_threads = true;
        fake
    }

    /// The prompt of every thread it was asked to name, in order.
    #[must_use]
    pub fn thread_prompts(&self) -> Vec<String> {
        self.lock().threads.clone()
    }

    /// Answer every summary with `reply`, run through what the shipped backends run a reply
    /// through ([`super::agent::plain_uncut`]), instead of the numbered `summary #N`.
    /// `#208 persistent-cache`: the tests of where an answer is cut need one longer than the cap.
    pub fn answer_with(&self, reply: &str) {
        self.lock().answer = Some(reply.to_owned());
    }

    /// Answer every thread naming with `sentence` as its second line, read through what the shipped
    /// backends read a naming through ([`super::threads::parse_thread_answer`]), instead of the
    /// numbered `thread summary #N`. The name is still `thread-N`. `#208 persistent-cache`: the
    /// tests of where a sentence is cut need one longer than the cap.
    pub fn name_with(&self, sentence: &str) {
        self.lock().sentence = Some(sentence.to_owned());
    }

    /// Make the next call fail. One-shot, so recovery is testable too.
    pub fn fail_next(&self, why: &str) {
        self.lock().fail_next = Some(why.to_owned());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl Summarizer for FakeSummarizer {
    fn describe(&self) -> &'static str {
        "an in-memory summariser that counts its calls"
    }

    // Deliberately the SHIPPED slug rather than one of its own: the cache tests substitute this
    // for the real summariser, and a "fake" slug would move every key they exercise off the
    // production prefix while leaving all of them green.
    //
    // It borrows the slug and not the whole key: this fake takes the DEFAULT `policy_input`, so
    // the hash after the prefix is not the one a deployment writes. What the shipped key really
    // is, is pinned by `policy_version_for`'s own tests and by the startup sweep test, not here.
    fn backend(&self) -> &'static str {
        super::agent::BACKEND
    }

    fn names_threads(&self) -> bool {
        self.lock().names_threads
    }

    async fn name_thread(&self, request: &ThreadRequest<'_>) -> Result<ThreadNaming, SummaryError> {
        let mut state = self.lock();
        if !state.names_threads {
            return Err(SummaryError::NotConfigured("summaries.llm"));
        }
        if let Some(why) = state.fail_next.take() {
            return Err(SummaryError::Transport(why));
        }
        state.threads.push(request.prompt().to_owned());
        let n = state.threads.len();
        if let Some(sentence) = &state.sentence {
            return super::threads::parse_thread_answer(
                &format!("thread-{n}\n{sentence}"),
                request.sentence_chars,
            );
        }
        Ok(ThreadNaming {
            slug: format!("thread-{n}"),
            sentence: format!("thread summary #{n}"),
            cut_at: None,
        })
    }

    async fn summarize(&self, request: &SummaryRequest<'_>) -> Result<Summary, SummaryError> {
        let mut state = self.lock();
        if let Some(why) = state.fail_next.take() {
            return Err(SummaryError::Transport(why));
        }
        state.requests.push(RecordedRequest {
            target: request.target.content.clone(),
            context: request.context.len(),
            target_chars: request.target_chars,
            prompt: request.prompt().to_owned(),
        });
        if let Some(reply) = &state.answer {
            let (text, cut_at) = super::agent::plain_uncut(reply, request.target_chars);
            return Ok(Summary { text, cut_at });
        }
        // Deliberately NOT the message text: a fake that echoed its input would let a caller that
        // returned the raw message pass as one that summarised it.
        Ok(Summary {
            text: format!("summary #{}", state.requests.len()),
            cut_at: None,
        })
    }
}
