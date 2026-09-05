/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use clap::ValueEnum;
use serde::Deserialize;
use serde::Serialize;

/// Versioned identity of the record envelope applied before log selection.
///
/// The predicate and its name travel together in [`RecordEnvelope`], so a
/// comparison cannot silently filter records while reporting an unfiltered
/// policy. Only the fixed all-records policy is eligible for parity;
/// caller-defined predicates are deliberately non-qualifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordEnvelopePolicy {
    /// Preserve every parsed log record.
    AllRecordsV1,
    /// A predicate whose semantics are not one of the named canonical policies.
    CallerDefined,
}

impl RecordEnvelopePolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AllRecordsV1 => "all_records_v1",
            Self::CallerDefined => "caller_defined",
        }
    }

    /// Whether this exact, versioned envelope may support bitwise parity.
    pub(crate) fn is_canonical(self) -> bool {
        matches!(self, Self::AllRecordsV1)
    }
}

/// A record predicate bound to the versioned policy name that describes it.
#[derive(Clone, Copy)]
pub(crate) struct RecordEnvelope {
    policy: RecordEnvelopePolicy,
    keep_record: fn(&str) -> bool,
}

impl std::fmt::Debug for RecordEnvelope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordEnvelope")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl RecordEnvelope {
    pub(crate) const fn all_records_v1() -> Self {
        Self {
            policy: RecordEnvelopePolicy::AllRecordsV1,
            keep_record: keep_all_records,
        }
    }

    #[cfg(test)]
    pub(crate) const fn caller_defined(keep_record: fn(&str) -> bool) -> Self {
        Self {
            policy: RecordEnvelopePolicy::CallerDefined,
            keep_record,
        }
    }

    pub(crate) fn policy(self) -> RecordEnvelopePolicy {
        self.policy
    }

    pub(crate) fn predicate(self) -> fn(&str) -> bool {
        self.keep_record
    }

    #[cfg(test)]
    pub(crate) fn keeps(self, record: &str) -> bool {
        (self.keep_record)(record)
    }
}

/// User-selectable named envelopes for standalone `hermit log-diff`.
///
/// The caller-defined variant is intentionally absent: the standalone command
/// cannot claim a policy whose predicate it cannot describe in its JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub(crate) enum RecordEnvelopeArg {
    AllRecordsV1,
}

impl RecordEnvelopeArg {
    pub(crate) const fn envelope(self) -> RecordEnvelope {
        match self {
            Self::AllRecordsV1 => RecordEnvelope::all_records_v1(),
        }
    }
}

fn keep_all_records(_record: &str) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_records_envelope_keeps_every_parsed_record() {
        let envelope = RecordEnvelope::all_records_v1();
        assert!(envelope.keeps("INFO detcore: DETLOG first"));
        assert!(envelope.keeps("WARN another_target: diagnostic"));
    }
}
