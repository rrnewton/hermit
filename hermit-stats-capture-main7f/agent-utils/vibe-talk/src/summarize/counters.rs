//! How the summary route spent its requests, in numbers only. `#208 persistent-cache`, milestone
//! `summary-keep`.
//!
//! The milestone's claims are all about what a summary request did NOT cost — no chat fetch for a
//! hit, one shared fetch for twenty misses, no model call after a restart — and they have to be
//! checkable on a deployed host without logging a key, an id or a word of text. So the route
//! counts, and `GET /api/v1/diagnostics` reports the counts. Memory only: a restart starts them
//! at zero, and nothing here is ever written anywhere.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// The counters themselves, shared by every request. See [`SummaryCounts`] for what each means.
#[derive(Debug, Default)]
pub struct SummaryCounters {
    requests: AtomicU64,
    hits: AtomicU64,
    hits_without_upstream: AtomicU64,
    shared_fetch_joins: AtomicU64,
    upstream_window_fetches: AtomicU64,
    misses: AtomicU64,
    generated: AtomicU64,
    regenerated_below_cap: AtomicU64,
    served_below_cap_read_scope: AtomicU64,
}

/// A copy of [`SummaryCounters`] at one moment, as `GET /api/v1/diagnostics` reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SummaryCounts {
    /// Summary requests that reached the route, whatever came of them.
    pub requests: u64,
    /// Requests answered from the durable store.
    pub hits: u64,
    /// Of those, the ones answered with no chat-provider call at all, because every message the
    /// row is made of was read for the page in the last thirty seconds.
    pub hits_without_upstream: u64,
    /// Requests that waited for another request's window fetch and used it instead of making one.
    pub shared_fetch_joins: u64,
    /// Chat reads the route made itself: one per channel window, one per thread message.
    pub upstream_window_fetches: u64,
    /// Requests the store could not answer, so the summariser was asked.
    pub misses: u64,
    /// Summaries the summariser produced.
    pub generated: u64,
    /// Of those, the ones asked for because the stored answer was cut shorter than the cap now in
    /// force, for a write-scope caller.
    pub regenerated_below_cap: u64,
    /// Read-scope requests served a stored answer cut shorter than the cap now in force, with no
    /// model call, because a read token may never file the longer one.
    pub served_below_cap_read_scope: u64,
}

impl SummaryCounters {
    /// All zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The counts now.
    #[must_use]
    pub fn snapshot(&self) -> SummaryCounts {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        SummaryCounts {
            requests: read(&self.requests),
            hits: read(&self.hits),
            hits_without_upstream: read(&self.hits_without_upstream),
            shared_fetch_joins: read(&self.shared_fetch_joins),
            upstream_window_fetches: read(&self.upstream_window_fetches),
            misses: read(&self.misses),
            generated: read(&self.generated),
            regenerated_below_cap: read(&self.regenerated_below_cap),
            served_below_cap_read_scope: read(&self.served_below_cap_read_scope),
        }
    }

    pub(crate) fn request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn hit(&self, without_upstream: bool, below_cap: bool) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        if without_upstream {
            self.hits_without_upstream.fetch_add(1, Ordering::Relaxed);
        }
        if below_cap {
            self.served_below_cap_read_scope
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn joined(&self) {
        self.shared_fetch_joins.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn fetched(&self, reads: usize) {
        self.upstream_window_fetches
            .fetch_add(u64::try_from(reads).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(crate) fn miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn generated(&self, below_cap: bool) {
        self.generated.fetch_add(1, Ordering::Relaxed);
        if below_cap {
            self.regenerated_below_cap.fetch_add(1, Ordering::Relaxed);
        }
    }
}
