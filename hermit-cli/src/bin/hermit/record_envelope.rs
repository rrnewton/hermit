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
/// policy. Only the two fully specified policies are eligible for parity;
/// caller-defined predicates are deliberately non-qualifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordEnvelopePolicy {
    /// Preserve every parsed log record.
    AllRecordsV1,
    /// Exclude only records emitted by the DBT evidence transport about
    /// itself. Those records are real and present in a live evidence stream
    /// (`evidence_emit_image_initialization`, reverie-dbt
    /// native/client.c:863), but live DBT run verification compares them:
    /// its adapter selects [`Self::AllRecordsV1`]. This policy is offered for
    /// offline `hermit log-diff` inspection of an archived evidence log.
    DbtEvidenceTransportV1,
    /// A predicate whose semantics are not one of the named canonical policies.
    CallerDefined,
}

impl RecordEnvelopePolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AllRecordsV1 => "all_records_v1",
            Self::DbtEvidenceTransportV1 => "dbt_evidence_transport_v1",
            Self::CallerDefined => "caller_defined",
        }
    }

    /// Whether this exact, versioned envelope may support bitwise parity.
    pub(crate) fn is_canonical(self) -> bool {
        matches!(self, Self::AllRecordsV1 | Self::DbtEvidenceTransportV1)
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

    pub(crate) const fn dbt_evidence_transport_v1() -> Self {
        Self {
            policy: RecordEnvelopePolicy::DbtEvidenceTransportV1,
            keep_record: keep_dbt_evidence_record,
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
    DbtEvidenceTransportV1,
}

impl RecordEnvelopeArg {
    pub(crate) const fn envelope(self) -> RecordEnvelope {
        match self {
            Self::AllRecordsV1 => RecordEnvelope::all_records_v1(),
            Self::DbtEvidenceTransportV1 => RecordEnvelope::dbt_evidence_transport_v1(),
        }
    }
}

fn keep_all_records(_record: &str) -> bool {
    true
}

/// Return whether a parsed DBT evidence record describes the guest rather than
/// the evidence transport itself.
///
/// Archived DBT logs must remain classifiable in builds without the optional
/// DBT runtime, so this policy lives in the CLI layer and has no `dbt` feature
/// dependency.
fn keep_dbt_evidence_record(record: &str) -> bool {
    let Some((level, body)) = record.split_once(' ') else {
        return true;
    };
    !matches!(level, "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE")
        || !body
            .trim_start_matches(' ')
            .starts_with("reverie_dbt::evidence:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbt_envelope_is_narrowly_keyed_on_the_transport_target() {
        let envelope = RecordEnvelope::dbt_evidence_transport_v1();
        assert!(!envelope.keeps("INFO reverie_dbt::evidence: protected evidence initialized"));
        assert!(!envelope.keeps("WARN  reverie_dbt::evidence: direct entry must be refused"));
        assert!(
            envelope.keeps(
                "INFO detcore: DETLOG reverie_dbt::evidence: protected evidence initialized"
            )
        );
        assert!(envelope.keeps("INFO reverie_dbt::launcher: starting"));
    }

    #[test]
    fn policy_json_name_is_exact_and_round_trips() {
        let policy = RecordEnvelopePolicy::DbtEvidenceTransportV1;
        let encoded = serde_json::to_string(&policy).unwrap();
        assert_eq!(encoded, "\"dbt_evidence_transport_v1\"");
        assert_eq!(
            serde_json::from_str::<RecordEnvelopePolicy>(&encoded).unwrap(),
            policy
        );
    }
}
