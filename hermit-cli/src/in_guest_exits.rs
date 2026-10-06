/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! In-guest LiteInst's asynchronous exit completion, on the launcher's side.
//!
//! A guest process under in-guest LiteInst leaves the host some time after
//! Detcore grants its exit. Detcore holds turn selection until the process is
//! physically gone (`process_exits_complete_asynchronously`), and this module
//! is what tells it so. The coordinator admits each guest connection through
//! [`InGuestExitAdmission`], which receives the connecting process's pidfd
//! (`SO_PEERPIDFD`) before the connection is served and hands it to the shared
//! [`PhysicalExitWatch`]. When the process has exited and the kernel has
//! published that to its parent, the watcher reports it through the reporter
//! the launcher attached, which reaches Detcore's
//! `GlobalTool::on_backend_process_exited`. Nothing here is in Detcore (owner
//! directive 082): Detcore only holds the abstract lifetime state.
//!
//! The watcher runs on the coordinator's own tokio runtime rather than on a
//! thread: the coordinator shares the guest's PID namespace, so a thread would
//! take a process id and shift every pid the guest sees.

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use reverie_rpc_transport::Admitted;
use reverie_rpc_transport::ConnectionAdmission;
use reverie_rpc_transport::ExitReporter;

use crate::physical_exit_watch::DrainError;
use crate::physical_exit_watch::PhysicalExitWatch;

/// Admits in-guest LiteInst connections by pidfd and watches each admitted
/// process for its physical exit; see the module documentation.
#[derive(Default)]
pub(crate) struct InGuestExitAdmission {
    watch: Mutex<Option<io::Result<PhysicalExitWatch>>>,
}

impl ConnectionAdmission for InGuestExitAdmission {
    /// Called by the backend's launch on the coordinator's runtime.
    fn attach_exit_reporter(&self, reporter: Arc<dyn ExitReporter>) {
        let watch = tokio::runtime::Handle::try_current()
            .map_err(io::Error::other)
            .and_then(|handle| {
                PhysicalExitWatch::on_runtime(handle, move |pid| reporter.process_exited(pid))
            });
        *self.watch.lock().unwrap() = Some(watch);
    }

    fn admit(&self, peer: OwnedFd) -> io::Result<Admitted> {
        let process_id = guest_visible_pid(&peer)?;
        let watch = self.watch.lock().unwrap();
        match watch.as_ref() {
            Some(Ok(watch)) => watch.watch(peer, process_id)?,
            Some(Err(error)) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("in-guest LiteInst exit watcher did not start: {error}"),
                ));
            }
            None => {
                return Err(io::Error::other(
                    "in-guest LiteInst admitted a connection with no exit watcher (none attached yet, or already drained)",
                ));
            }
        }
        Ok(Admitted { process_id })
    }
}

impl InGuestExitAdmission {
    /// Waits, bounded, until every admitted process's exit has been reported.
    /// Takes the watcher out, so it ends here and admits nothing afterwards.
    pub(crate) async fn drain(&self, timeout: Duration) -> Result<(), DrainError> {
        let watch = self.watch.lock().unwrap().take();
        match watch {
            Some(Ok(watch)) => watch.drain_async(timeout).await,
            Some(Err(error)) => Err(DrainError::Watcher(error)),
            None => Ok(()),
        }
    }
}

/// The pid of `pidfd`'s process as the guest sees it: the last (innermost)
/// field of the pidfd's `NSpid:` fdinfo line, or its `Pid:` line on a kernel
/// that prints no `NSpid:`. A process that is not visible (`0`) or already
/// reaped (`-1`) is refused.
fn guest_visible_pid(pidfd: &OwnedFd) -> io::Result<i32> {
    let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?;
    let field = |name: &str| {
        fdinfo
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::to_owned)
    };
    let value = field("NSpid:")
        .and_then(|nspid| nspid.split_whitespace().last().map(str::to_owned))
        .or_else(|| field("Pid:").map(|pid| pid.trim().to_owned()))
        .ok_or_else(|| io::Error::other("pidfd fdinfo names no process id"))?;
    let pid: i32 = value
        .parse()
        .map_err(|error| io::Error::other(format!("pidfd fdinfo process id {value:?}: {error}")))?;
    if pid <= 0 {
        return Err(io::Error::other(format!(
            "the connecting process is not visible or already gone (pid {pid})"
        )));
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;
    use std::process::Command;
    use std::sync::mpsc;

    use super::*;

    fn pidfd_open(pid: u32) -> OwnedFd {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        assert!(fd >= 0, "pidfd_open: {}", io::Error::last_os_error());
        unsafe { OwnedFd::from_raw_fd(fd as i32) }
    }

    #[test]
    fn the_guest_visible_pid_is_read_from_the_pidfd() {
        let me = pidfd_open(std::process::id());
        assert_eq!(guest_visible_pid(&me).unwrap(), std::process::id() as i32);
    }

    /// Forwards each reported exit to a channel.
    struct Sender(Mutex<mpsc::Sender<i32>>);

    impl ExitReporter for Sender {
        fn process_exited(&self, pid: i32) {
            self.0.lock().unwrap().send(pid).unwrap();
        }
        fn pending_process_exits(&self) -> Vec<i32> {
            Vec::new()
        }
        fn backend_failed(&self, _failure: reverie::BackendFailure) {}
    }

    #[tokio::test]
    async fn an_admitted_process_is_reported_once_after_it_exits() {
        let admission = InGuestExitAdmission::default();
        let (sender, receiver) = mpsc::channel();
        admission.attach_exit_reporter(Arc::new(Sender(Mutex::new(sender))));
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let admitted = admission.admit(pidfd_open(child.id())).unwrap();
        assert_eq!(admitted.process_id, pid);
        admission.drain(Duration::from_secs(30)).await.unwrap();
        assert_eq!(receiver.try_recv().unwrap(), pid);
        child.wait().unwrap();
        assert!(receiver.try_recv().is_err(), "reported more than once");
    }

    #[test]
    fn admission_before_the_reporter_is_refused() {
        let admission = InGuestExitAdmission::default();
        let me = pidfd_open(std::process::id());
        assert!(admission.admit(me).is_err());
    }
}
