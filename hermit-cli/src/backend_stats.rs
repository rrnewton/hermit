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
    BackendStatsRequest::new(tracing::enabled!(target: TARGET, tracing::Level::INFO))
}

pub(crate) fn report<S>(selected_backend: Backend, request: BackendStatsRequest, source: &S)
where
    S: BackendStatsSource,
{
    let Some(snapshot) = request.collect(source) else {
        return;
    };
    debug_assert_eq!(selected_backend.as_str(), S::Snapshot::BACKEND_NAME);
    tracing::info!(
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

pub(crate) struct LiteinstStatsSource<'a> {
    stats: &'a reverie_liteinst::LiteinstInstrumentationStats,
}

impl<'a> LiteinstStatsSource<'a> {
    pub(crate) const fn new(stats: &'a reverie_liteinst::LiteinstInstrumentationStats) -> Self {
        Self { stats }
    }
}

pub(crate) struct LiteinstStatsSnapshot<'a> {
    stats: &'a reverie_liteinst::LiteinstInstrumentationStats,
}

impl fmt::Display for LiteinstStatsSnapshot<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.stats.fmt(formatter)
    }
}

impl BackendStatsSnapshot for LiteinstStatsSnapshot<'_> {
    const BACKEND_NAME: &'static str = "liteinst";
}

impl<'a> BackendStatsSource for LiteinstStatsSource<'a> {
    type Snapshot = LiteinstStatsSnapshot<'a>;

    fn backend_stats(&self) -> Self::Snapshot {
        LiteinstStatsSnapshot { stats: self.stats }
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
}
