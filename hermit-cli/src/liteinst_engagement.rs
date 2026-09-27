/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Publication of host-hybrid LiteInst counters for `--backend-engagement-json`.
//!
//! The CLI runs the guest in a forked container process, so the counters cannot
//! be returned in memory. The CLI installs a sink inside that process before it
//! starts the run; the LiteInst dispatch collects Reverie's statistics only
//! when a sink is installed and writes them to it after the tracer finishes.
//! Collection is host-side bookkeeping in the tracer and does not change what
//! the guest observes.
//!
//! The sink is an open file, not a path, so the CLI can hand over an unlinked
//! file that no guest can list. It is written at offset zero, independent of
//! the descriptor's shared file offset.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

use anyhow::Context;
use anyhow::Error;
use detcore_model::backend_engagement::LiteinstHostHybridCounters;
use reverie_liteinst::LiteinstBackendStatsSource;
use reverie_liteinst::LiteinstDispatchPath;

static SINK: Mutex<Option<File>> = Mutex::new(None);

/// While alive, LiteInst runs started by this process publish their counters to
/// one file. At most one sink may be installed at a time.
#[doc(hidden)]
#[must_use = "the sink is removed when this guard is dropped"]
pub struct LiteinstCountersSink {
    _private: (),
}

impl LiteinstCountersSink {
    /// Install `file` as the destination for the next LiteInst run's counters.
    pub fn install(file: File) -> Result<Self, Error> {
        let mut sink = SINK.lock().unwrap();
        if sink.is_some() {
            anyhow::bail!("a LiteInst counters sink is already installed");
        }
        *sink = Some(file);
        Ok(Self { _private: () })
    }
}

impl Drop for LiteinstCountersSink {
    fn drop(&mut self) {
        *SINK.lock().unwrap() = None;
    }
}

/// The installed sink, if any, as a separate descriptor for the same file.
pub(crate) fn requested_sink() -> Result<Option<File>, Error> {
    SINK.lock()
        .unwrap()
        .as_ref()
        .map(|file| {
            file.try_clone()
                .context("duplicating the LiteInst counters sink")
        })
        .transpose()
}

/// In-guest dispatch paths. The host hybrid never populates them: each one
/// would be counted by the preload runtime and aggregated over RPC, which the
/// host-hybrid stats source does not do.
const IN_GUEST_PATHS: [LiteinstDispatchPath; 5] = [
    LiteinstDispatchPath::InGuestSigsys,
    LiteinstDispatchPath::InGuestNestedSigsys,
    LiteinstDispatchPath::InGuestPhysicalSigsys,
    LiteinstDispatchPath::FallbackCompletionSigsys,
    LiteinstDispatchPath::FallbackRefusal,
];

/// Reduce Reverie's host-hybrid statistics to the counters Hermit records.
///
/// Refuses a snapshot that carries in-guest counts, because this reduction
/// reports zero ptrace-free entries on the premise that every installed hook
/// returns through the ptrace host.
pub(crate) fn host_hybrid_counters(
    source: &LiteinstBackendStatsSource,
) -> Result<LiteinstHostHybridCounters, Error> {
    let snapshot = source.snapshot();
    let paths = snapshot.dispatch_paths();
    if snapshot.process_reports() != 0 {
        anyhow::bail!(
            "LiteInst host-hybrid statistics unexpectedly carry {} in-guest process reports",
            snapshot.process_reports()
        );
    }
    for path in IN_GUEST_PATHS {
        let count = paths.count(&path);
        if count != 0 {
            anyhow::bail!(
                "LiteInst host-hybrid statistics unexpectedly carry {count} `{path}` in-guest \
                 dispatches, which this record cannot attribute"
            );
        }
    }
    let fallback_entries = paths
        .count(&LiteinstDispatchPath::CachelineStraddlerFallback)
        .checked_add(paths.count(&LiteinstDispatchPath::UnpatchableOrOtherFallback))
        .context("LiteInst fallback counters overflow")?;
    Ok(LiteinstHostHybridCounters {
        direct_hooks: paths.count(&LiteinstDispatchPath::DirectHook),
        first_site_seccomp: paths.count(&LiteinstDispatchPath::FirstSiteSeccomp),
        ptrace_installations: paths.count(&LiteinstDispatchPath::PtraceInstallation),
        fallback_entries,
        patched_sites: u64::try_from(source.distinct_rips())?,
        candidate_sites: u64::try_from(source.patch_candidates())?,
    })
}

/// Write the counters of a finished run to `sink`, replacing its contents.
pub(crate) fn publish(sink: &File, source: &LiteinstBackendStatsSource) -> Result<(), Error> {
    let counters = host_hybrid_counters(source)?;
    let mut bytes = serde_json::to_vec(&counters)?;
    bytes.push(b'\n');
    sink.set_len(0)
        .context("truncating the LiteInst counters sink")?;
    sink.write_all_at(&bytes, 0)
        .context("publishing LiteInst counters")
}
