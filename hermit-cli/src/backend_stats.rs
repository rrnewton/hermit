/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit-side enablement and reporting for backend statistics.

use std::fmt;

use reverie::BackendStatsRequest;
use reverie::BackendStatsSnapshot;
use reverie::BackendStatsSource;

use crate::Backend;

const TARGET: &str = "hermit::backend_stats";

pub(crate) fn request() -> BackendStatsRequest {
    BackendStatsRequest::new(tracing::enabled!(target: TARGET, tracing::Level::DEBUG))
}

/// Report the selected backend's own run statistics.
///
/// ⚠️ THIS RECORD IS EMITTED AT DEBUG, NOT INFO, AND THAT IS LOAD-BEARING.
/// `ComparedLogScope::Info` is the `BitwiseInfoV1` parity envelope — "every INFO
/// message, exactly" — so anything emitted here at INFO is compared between two
/// runs as though it were guest behaviour. This record is not guest behaviour:
/// it is hermit describing its own harness, and BOTH its fields identify the
/// harness rather than the guest. `backend` is the backend's name, and `stats`
/// is that backend's own instrumentation (`metrics=none` for ptrace, patch-shape
/// counters for e9patch, and so on).
///
/// Measured on a real `ptrace` run before this change: of 303 INFO records, this
/// was the ONE record naming a backend. So two backends executing an identical
/// guest could never agree under the Info envelope, however correct the backends
/// were — a divergence by construction rather than a behavioural difference, and
/// the only such record in the stream.
///
/// Same-backend verification prints the same values on both sides, so nothing
/// was red; the ceiling was on any future cross-backend comparison. DEBUG keeps
/// the record fully available to `--log debug` while removing it from the
/// envelope that is compared, which is the narrow fix. Widening or renaming the
/// Info scope itself would change what a published parity claim MEANS, and that
/// is an owner decision, not this one.
pub(crate) fn report<S>(selected_backend: Backend, request: BackendStatsRequest, source: &S)
where
    S: BackendStatsSource,
{
    let Some(snapshot) = request.collect(source) else {
        return;
    };
    assert_eq!(
        selected_backend.as_str(),
        S::Snapshot::BACKEND_NAME,
        "backend statistics source does not match selected backend"
    );
    tracing::debug!(
        target: TARGET,
        backend = %selected_backend.as_str(),
        stats = %snapshot,
        "backend run complete",
    );
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PtraceStatsSource;

pub(crate) struct PtraceStatsSnapshot;

impl fmt::Display for PtraceStatsSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("metrics=none")
    }
}

impl BackendStatsSnapshot for PtraceStatsSnapshot {
    const BACKEND_NAME: &'static str = "ptrace";
}

impl BackendStatsSource for PtraceStatsSource {
    type Snapshot = PtraceStatsSnapshot;

    fn backend_stats(&self) -> Self::Snapshot {
        PtraceStatsSnapshot
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct CountingSource {
        snapshots: Cell<usize>,
    }

    struct CountingSnapshot;

    impl fmt::Display for CountingSnapshot {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("counting")
        }
    }

    impl BackendStatsSnapshot for CountingSnapshot {
        const BACKEND_NAME: &'static str = "ptrace";
    }

    impl BackendStatsSource for CountingSource {
        type Snapshot = CountingSnapshot;

        fn backend_stats(&self) -> Self::Snapshot {
            self.snapshots.set(self.snapshots.get() + 1);
            CountingSnapshot
        }
    }

    #[test]
    fn disabled_report_does_not_snapshot_backend() {
        let source = CountingSource {
            snapshots: Cell::new(0),
        };

        report(Backend::Ptrace, BackendStatsRequest::DISABLED, &source);
        assert_eq!(source.snapshots.get(), 0);
        report(Backend::Ptrace, BackendStatsRequest::ENABLED, &source);
        assert_eq!(source.snapshots.get(), 1);
    }

    #[test]
    fn baseline_ptrace_snapshot_is_explicit() {
        assert_eq!(
            PtraceStatsSource.backend_stats().to_string(),
            "metrics=none"
        );
    }

    #[test]
    #[should_panic(expected = "backend statistics source does not match selected backend")]
    fn report_rejects_a_mismatched_backend_source_in_release_builds() {
        let source = CountingSource {
            snapshots: Cell::new(0),
        };

        report(Backend::Liteinst, BackendStatsRequest::ENABLED, &source);
    }
}
