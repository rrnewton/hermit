//! Read-only replay and disposable indexing for append-only slot event logs.
//!
//! This crate never writes the event directory. Its only mutable output is a
//! derived SQLite index that can be discarded and rebuilt from the event log.
//! Event numbers are accepted only when they have an exact `i64`/`u64`
//! representation or decode to a finite `f64`; within that domain hashes use
//! the Python authority's canonical spelling. Wider integers and non-finite
//! values are refused instead of being rounded into a different hash. Event
//! documents are limited to 16 MiB and 127 nested JSON containers, and strings
//! must decode to Unicode scalar values (so lone surrogate escapes are
//! refused). Replay also refuses unknown event kinds, non-array imported holds,
//! and non-object state evidence even where the current Python reader does not
//! inspect those values; the Python writer always emits the stricter form.
//!
//! Replay validates the complete intrinsic active/archive record schema, nested
//! identities, checkout identities, historical imports, revisions, recovery
//! evidence, and hold transitions. An optional, versioned configuration and
//! captured evidence census can materialize three-valued diagnostic decisions.
//! Every decision is bound to the machine, event tip, and the canonical JSON
//! values of its configuration and evidence. Active-row decisions are also
//! bound to the slot generation and active record; pending-only decisions
//! expose null active-row fields and remain blocked until the operation closes.
//! Insignificant input whitespace and object-key order therefore do not change
//! those digests. Scope, cgroup, and checkout identities in the evidence are
//! claims, not independently observed facts. This slice has neither a read-time
//! `/proc`/boot/systemd/cgroup verifier nor fresh repository/task/owner/attempt-
//! bound TaskGraph claim evidence, so an otherwise unblocked row remains
//! `UNKNOWN` and cannot become actionable.
//! The shadow-policy JSON is not the originating `.wrkslots.yml` registry
//! configuration. Configuration-coupled checkout/repository paths, layout
//! defaults, landed refs, Git registrations, and nested-repository discovery
//! remain the Python authority's job; captured evidence cannot fill those gaps.
//!
//! `wrkslotsd` is an unpublished migration component. `explain` and `plan`
//! reconstruct and cross-check decisions from the indexed inputs, then reject
//! evidence that has aged out; they cannot authenticate a same-UID rewrite of
//! the whole disposable database, rediscover repositories absent from the
//! captured inputs, or execute decisions. There is no cleanup, rescue,
//! deletion, repository mutation, daemon, or socket surface; operators should
//! continue to use the parent `wrkslots quickstart` and `wrkslots --userguide`.

#![forbid(unsafe_code)]

mod canonical;
pub mod cli;
mod config;
mod evidence;
mod index;
mod plan;
mod policy;
mod replay;
mod schema;

use std::error::Error;
use std::fmt;

pub use canonical::canonical_sha256;
pub use index::{read_index, rebuild_index};
pub use replay::{replay, ReplaySummary};

/// An invalid event log, inaccessible file, or unusable derived index.
#[derive(Debug)]
pub struct ObserverError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ObserverError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn with_source(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for ObserverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ObserverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests;
