/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use clap::Parser;
use hermit::Backend;
use tracing::metadata::LevelFilter;

use super::tracing::BoundedWriter;
use super::tracing::init_file_tracing;
use super::tracing::init_stderr_tracing;
use super::tracing::log_max_bytes;

/// Hermit provides a sandbox for deterministic and reproducible execution.
/// Arbitrary programs run inside (guests) become deterministic
/// functions of their inputs. Configuration flags control the initial
/// environment.
///
/// See the "run" and "record" subcommands to run programs within hermit.
/// In both modes, the host file system is visible
/// to the command run inside hermit, and the results will depend on the contents
/// (but not timestamps or inode numbers) of those inputs.
///
/// In run mode, networking is disallowed.  Run mode guarantees that if you
/// run twice with the same input files, you will receive bitwise identical
/// outputs from the computation.
///
/// In record mode, inputs (both files and network traffic) are captured
/// in a content addressible store (CAS).  In this preview version of
/// hermit, the CAS is stored locally in your home directory (~/.hermit).
///
/// Below are options common to all subcommands.
#[derive(Debug, Parser, Clone)]
pub struct GlobalOpts {
    /// The verbosity level of log output.
    #[clap(short, long, value_name = "LEVEL", env = "HERMIT_LOG")]
    pub log: Option<LevelFilter>,

    /// Log to a file instead of the terminal. The path is resolved on the HOST,
    /// exactly like a shell redirect, not inside the container the guest runs in.
    #[clap(long, value_name = "FILE", env = "HERMIT_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// The log file, already opened in the HOST's filename namespace.
    ///
    /// WHY THE FILE IS CARRIED INSTEAD OF RE-OPENED FROM THE PATH. Tracing has to be
    /// initialized INSIDE the container -- see `init_tracing`, whose reason is that
    /// the tracer may create a thread -- and the container mounts a fresh writable
    /// /tmp over its root (container.rs: "A fresh writable /tmp is mounted separately
    /// for ordinary scratch files"). So opening the path at that point resolves it in
    /// the GUEST namespace. For `--log-file /tmp/x.log` the create then SUCCEEDS, into
    /// the guest's tmpfs, and the file dies with the container: exit 0, no log, no
    /// warning. Measured 2026-08-20; one debugging session was lost to it.
    ///
    /// An open file descriptor is unaffected by a later mount-namespace change, so
    /// opening on the host and carrying the handle in is mechanically what a shell
    /// redirect does. Only the OPEN moves out of the container; tracing itself is
    /// still initialized inside, so the stated reason for that is untouched.
    ///
    /// `Arc` because `GlobalOpts` is `Clone` (verify clones it per run) and because
    /// each `init_tracing` needs its own owned `File`, produced with `try_clone`.
    #[clap(skip)]
    pub log_file_handle: Option<Arc<File>>,

    /// Select the process instrumentation backend. This is the preferred, global
    /// position (e.g. `hermit --backend ptrace run ...`); for backwards
    /// compatibility `run` also accepts `--backend` after the subcommand.
    #[clap(long, value_enum, value_name = "BACKEND")]
    pub backend: Option<Backend>,
}

impl GlobalOpts {
    /// Open `--log-file` in the HOST's filename namespace.
    ///
    /// Call this from `main`, before any container exists. That placement is the
    /// whole point: it is the moment a shell would perform `> file`.
    ///
    /// Reports the path on failure instead of proceeding. A run that was asked for a
    /// log and produces neither the log nor an error is indistinguishable from a run
    /// whose log was legitimately empty, and the person debugging cannot tell which.
    pub fn open_log_file(&mut self) -> Result<(), Error> {
        if let Some(path) = &self.log_file {
            let file = File::create(path).with_context(|| {
                format!("cannot open --log-file {} for writing", path.display())
            })?;
            self.log_file_handle = Some(Arc::new(file));
        }
        Ok(())
    }

    pub fn log_file_writer(&self) -> Option<File> {
        if let Some(handle) = &self.log_file_handle {
            // Each subscriber needs an owned File; `try_clone` dups the descriptor,
            // so every run writes through the same host-side open file.
            Some(
                handle
                    .try_clone()
                    .expect("cannot duplicate the host log file descriptor"),
            )
        } else {
            // An internal caller set the path directly rather than going through
            // `open_log_file` -- today that is verify's double-run setup, which
            // creates its own temp files and is measured NOT to hit the namespace
            // problem. Keep the historical behaviour for those, rather than changing
            // a path this task did not investigate.
            self.log_file
                .as_ref()
                .map(|path| File::create(path).expect("Failed to open log file"))
        }
    }

    pub fn init_liteinst_capture(&self) -> Result<super::tracing::capture::Capture, Error> {
        let filter = super::tracing::effective_filter(self.log);
        if let Some(file) = self.log_file_writer() {
            super::tracing::capture::Capture::install(
                file,
                log_max_bytes().map_err(Error::msg)?,
                filter,
            )
        } else {
            super::tracing::capture::Capture::install(detcore::util::RetryingStderr, 0, filter)
        }
    }

    /// Initalizes tracing. If using a container, this must be done *inside* of
    /// the container because the tracer may create a new thread.
    ///
    /// The file itself is NOT opened here when it came from `--log-file`; see
    /// `log_file_handle` for why opening at this point resolves the path in the
    /// guest namespace and silently loses it.
    #[must_use = "This function returns a guard that should not be immediately dropped"]
    pub fn init_tracing(&self) -> Option<impl Drop + use<>> {
        if let Some(file_writer) = self.log_file_writer() {
            // Bounded so a run that makes no progress cannot fill the disk: a
            // livelocked guest logged 928.8 GiB over 11.7 hours before this.
            // The bound is on the LOG only; the run is unaffected.
            // A malformed bound is fatal rather than silently defaulted, so a
            // typo in the value meant to DISABLE the bound cannot quietly
            // re-enable it.
            let limit = log_max_bytes().unwrap_or_else(|e| panic!("{e}"));
            let file_writer = BoundedWriter::new(file_writer, limit);
            Some(init_file_tracing(self.log, file_writer))
        } else {
            init_stderr_tracing(self.log);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn options() -> GlobalOpts {
        GlobalOpts {
            log: None,
            log_file: None,
            log_file_handle: None,
            backend: None,
        }
    }

    #[test]
    fn host_handle_wins_over_replaced_path_and_shares_offset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("log");
        let original_path = directory.path().join("original");
        let mut opts = options();
        opts.log_file = Some(path.clone());
        opts.open_log_file().unwrap();
        opts.log_file_writer()
            .unwrap()
            .write_all(b"before-")
            .unwrap();
        std::fs::rename(&path, &original_path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        opts.clone()
            .log_file_writer()
            .unwrap()
            .write_all(b"after")
            .unwrap();
        assert_eq!(std::fs::read(original_path).unwrap(), b"before-after");
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }

    #[test]
    fn path_only_destination_preserves_create_and_truncate_behavior() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("log");
        let mut opts = options();
        opts.log_file = Some(path.clone());
        opts.log_file_writer().unwrap().write_all(b"first").unwrap();
        opts.log_file_writer().unwrap().write_all(b"next").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"next");
    }

    #[test]
    fn absent_file_keeps_stderr_selection_and_standard_descriptors() {
        let stdio = || {
            [0, 1, 2].map(|descriptor| {
                std::fs::read_link(format!("/proc/self/fd/{descriptor}")).unwrap()
            })
        };
        let before = stdio();
        let mut opts = options();
        opts.open_log_file().unwrap();
        assert!(opts.log_file_writer().is_none());
        assert_eq!(stdio(), before);
    }

    #[test]
    fn host_open_failure_preserves_path_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing/log");
        let mut opts = options();
        opts.log_file = Some(path.clone());
        let error = opts.open_log_file().unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("cannot open --log-file {} for writing", path.display())
        );
        assert!(opts.log_file_handle.is_none());
    }

    #[test]
    #[should_panic(expected = "Failed to open log file")]
    fn path_only_open_failure_preserves_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let mut opts = options();
        opts.log_file = Some(directory.path().join("missing/log"));
        opts.log_file_writer();
    }
}
