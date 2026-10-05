/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Collects the host identity of each file the guest opens and writes the
//! collection to the run's `host_input_log` when the run ends (see
//! `detcore_model::host_input`).
//!
//! This is an observation for the verification layer, not Detcore state:
//! nothing here is read back, no scheduling decision consults it, and nothing
//! the guest sees depends on it. The records are kept in memory until the run
//! ends, so the log stays empty while any guest runs; a run that does not end
//! normally writes nothing, which leaves its divergence unexplained rather
//! than explained by partial evidence.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use detcore_model::host_input::HostFileIdentity;
use detcore_model::host_input::HostInputLogEnd;
use detcore_model::host_input::HostInputRecord;
use tracing::warn;

/// The records of each run in this process, by the log they are written to.
/// More than one run can share a process (the KVM backend, unit tests), so
/// each log keeps its own. A run's records belong to it alone: [`begin`]
/// empties them when its global state is created, [`finish`] takes them when
/// it ends normally, and [`discard`] drops them when it fails.
static PENDING: Mutex<Option<HashMap<PathBuf, Vec<HostInputRecord>>>> = Mutex::new(None);

/// Start the run whose log is `log` with no records.
pub(crate) fn begin(log: &Path) {
    discard(log);
}

/// Drop the records of the run whose log is `log`, which writes none.
pub(crate) fn discard(log: &Path) {
    if let Some(pending) = PENDING.lock().unwrap().as_mut() {
        pending.remove(log);
    }
}

/// Record that thread `dtid`, in its syscall number `syscall`, opened `path`,
/// whose host `fstat` is `stat`, for the run whose log is `log`.
pub(crate) fn record(log: &Path, dtid: u64, syscall: u64, path: &Path, stat: &libc::stat) {
    let record = HostInputRecord {
        path: path.to_string_lossy().into_owned(),
        dtid,
        syscall,
        identity: HostFileIdentity::from_stat(stat),
    };
    PENDING
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(log.to_path_buf())
        .or_default()
        .push(record);
}

/// Write the run's records to `log`, one JSON line each, followed by a
/// [`HostInputLogEnd`] line naming how many there are, so that a reader can
/// tell a complete log from a cut one. Called once, when the run ends.
pub(crate) fn finish(log: &Path) {
    let records = PENDING
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|pending| pending.remove(log))
        .unwrap_or_default();
    let mut text = Vec::new();
    let end = HostInputLogEnd {
        records: records.len() as u64,
    };
    let encoded = records
        .iter()
        .map(serde_json::to_vec)
        .chain(std::iter::once(serde_json::to_vec(&end)))
        .try_for_each(|line| {
            text.extend(line?);
            text.push(b'\n');
            Ok::<(), serde_json::Error>(())
        });
    let written = encoded
        .map_err(std::io::Error::other)
        .and_then(|()| std::fs::write(log, &text));
    if let Err(error) = written {
        warn!(
            "[detcore] cannot write the host-input log {}: {}",
            log.display(),
            error
        );
    }
}
