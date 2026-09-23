//! Outside-container service ownership. The actual parent startup callback is
//! the only constructor caller; no helper is spawned in the guest PID namespace.
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Instant;

use super::release::ReleaseBootstrapError;
use super::release::ReleaseDrainPending;
use super::release::ReleaseService;

/// Actual outside-container service and its exact child/endpoint capabilities.
/// It is not serializable or clonable and must accompany the owned container
/// finalization result on every startup, work and cleanup outcome.
#[must_use = "retain together with the exact container child until cleanup is observed"]
pub struct ParentNetworkService {
    endpoint: OwnedFd,
    controller: OwnedFd,
    incarnation: [u8; 16],
    service: Option<ReleaseService>,
    completed: Vec<super::release::ReleaseCompletion>,
}

/// Startup failure with any unreaped broker still owned, never a string-only error.
#[must_use]
pub struct ParentNetworkStartFailure {
    error: std::io::Error,
    bootstrap: Option<ReleaseBootstrapError>,
    cleanup_error: Option<std::io::Error>,
    endpoint: Option<OwnedFd>,
    controller: Option<OwnedFd>,
}
impl std::fmt::Debug for ParentNetworkStartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParentNetworkStartFailure")
            .field("error", &self.error)
            .field("bootstrap", &self.bootstrap)
            .field("cleanup_error", &self.cleanup_error)
            .field("retains_endpoint", &self.endpoint.is_some())
            .field("retains_controller", &self.controller.is_some())
            .finish()
    }
}

/// Incomplete shutdown retains the whole parent session and release service.
#[must_use]
pub struct ParentNetworkDrainPending {
    error: std::io::Error,
    session: ParentNetworkService,
    release: Option<ReleaseDrainPending>,
}
impl std::fmt::Debug for ParentNetworkDrainPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParentNetworkDrainPending")
            .field("error", &self.error)
            .field("retains_release", &self.release.is_some())
            .finish()
    }
}

/// Positive outside-service settlement, independent of the guest's primary effect.
#[derive(Debug)]
pub struct ParentNetworkDrained {
    /// Private run identity, never used as network channel matching identity.
    pub incarnation: [u8; 16],
    /// Actual release broker wait status, not a fabricated success code.
    pub broker_wait_status: i32,
    // Raw completion observations, possibly repeated across cleanup retries.
    // This is deliberately not presented as a unique-job count.
    receipts: Vec<super::release::ReleaseCompletion>,
}

impl ParentNetworkService {
    /// Start only inside Container::run_with_startup_owned's parent callback.
    ///
    /// # Safety
    /// The callback owns an unreaped actual child, runs after clone but before
    /// STARTUP_READY, and has not started competing reapers/threads. The private
    /// endpoint belongs to that child's completed SCM_RIGHTS startup exchange.
    pub unsafe fn start_after_clone(
        endpoint: OwnedFd,
        controller: BorrowedFd<'_>,
        incarnation: [u8; 16],
        deadline: Instant,
    ) -> Result<Self, ParentNetworkStartFailure> {
        let raw = unsafe { libc::fcntl(controller.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if raw < 0 {
            return Err(ParentNetworkStartFailure {
                error: std::io::Error::last_os_error(),
                bootstrap: None,
                cleanup_error: None,
                endpoint: Some(endpoint),
                controller: None,
            });
        }
        let controller = unsafe { OwnedFd::from_raw_fd(raw) };
        match unsafe { ReleaseService::start_before_guests(deadline) } {
            Ok(service) => Ok(Self {
                endpoint,
                controller,
                incarnation,
                service: Some(service),
                completed: Vec::new(),
            }),
            Err(bootstrap) => Err(ParentNetworkStartFailure {
                error: std::io::Error::other(bootstrap.error.to_string()),
                bootstrap: Some(bootstrap),
                cleanup_error: None,
                endpoint: Some(endpoint),
                controller: Some(controller),
            }),
        }
    }

    fn controller_has_exited(&self) -> std::io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.controller.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut poll, 1, 0) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::other(
                "owned controller pidfd observation failed",
            ));
        }
        Ok(poll.revents & libc::POLLIN != 0)
    }

    /// Settle only after the exact controller exited. An endpoint EOF alone is
    /// insufficient, and a successful broker drain never resolves an unknown
    /// primary guest read/copy/descriptor mutation.
    pub fn drain_terminal(
        mut self,
        deadline: Instant,
    ) -> Result<ParentNetworkDrained, ParentNetworkDrainPending> {
        let exited = match self.controller_has_exited() {
            Ok(true) => true,
            Ok(false) => false,
            Err(error) => {
                return Err(ParentNetworkDrainPending {
                    error,
                    session: self,
                    release: None,
                });
            }
        };
        if !exited {
            return Err(ParentNetworkDrainPending {
                error: std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "controller has not physically exited",
                ),
                session: self,
                release: None,
            });
        }
        match self
            .service
            .take()
            .expect("service remains owned until consuming drain")
            .drain_terminal(deadline)
        {
            Ok(receipt) => {
                self.completed.extend(receipt.completed);
                Ok(ParentNetworkDrained {
                    incarnation: self.incarnation,
                    broker_wait_status: receipt.broker_wait_status,
                    receipts: self.completed,
                })
            }
            Err(release) => Err(ParentNetworkDrainPending {
                error: std::io::Error::other(release.error.to_string()),
                session: self,
                release: Some(release),
            }),
        }
    }
}
impl ParentNetworkDrainPending {
    /// Retry using the same retained service, never reconstructing from numeric IDs.
    pub fn retry(self, deadline: Instant) -> Result<ParentNetworkDrained, Self> {
        let Self {
            mut session,
            release,
            ..
        } = self;
        if let Some(release) = release {
            session.completed.extend(release.completed);
            session.service = Some(release.service);
        }
        session.drain_terminal(deadline)
    }
}
impl ParentNetworkStartFailure {
    /// Bounded cleanup of only the retained startup broker. This borrows the
    /// failure, so successful cleanup cannot erase the original startup error.
    /// A failed attempt keeps the actual broker and its error in this owner.
    pub fn retry_cleanup(&mut self, deadline: Instant) -> Result<(), &std::io::Error> {
        if let Some(cleanup) = self
            .bootstrap
            .as_mut()
            .and_then(|failure| failure.cleanup.as_mut())
        {
            if let Err(error) = cleanup.terminate_and_reap(deadline) {
                self.cleanup_error = Some(error);
                return Err(self.cleanup_error.as_ref().unwrap());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_failure_survives_successful_empty_cleanup() {
        let mut failure = ParentNetworkStartFailure {
            error: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "original startup failure",
            ),
            bootstrap: None,
            cleanup_error: None,
            endpoint: None,
            controller: None,
        };
        // No descriptor, service or child is constructed by this pure case.
        failure.retry_cleanup(Instant::now()).unwrap();
        assert_eq!(failure.error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(failure.error.to_string(), "original startup failure");
        assert!(failure.cleanup_error.is_none());
    }
}
