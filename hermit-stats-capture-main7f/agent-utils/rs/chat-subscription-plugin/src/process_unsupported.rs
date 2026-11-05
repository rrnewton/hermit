//! Non-Linux refusal surface for Linux-specific process supervision.

use std::fmt;
use std::fs::File;
use std::io;
use std::num::NonZeroU16;
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::time::Duration;

use chat_subscription::{
    BackendCapabilities, BackendFailure, CancellationError, ChatSubscriptionBackend,
    ChatSubscriptionCancellation, ChatSubscriptionDriver, EventKind, ReplaySupport,
    SubscribeRequest,
};

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "chat subscription process plugins require Linux clone3 and pidfd supervision",
    )
}

/// Explicit deadlines retained for portable configuration parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessPhaseTimeouts {
    hello: Duration,
    start: Duration,
    commit: Duration,
    close: Duration,
    shutdown_grace: Duration,
}

impl ProcessPhaseTimeouts {
    /// Validate portable timeout configuration without claiming process support.
    pub fn new(
        hello: Duration,
        start: Duration,
        commit: Duration,
        close: Duration,
        shutdown_grace: Duration,
    ) -> io::Result<Self> {
        if [hello, start, commit, close]
            .into_iter()
            .any(|value| value.is_zero())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin protocol phase timeouts must be positive",
            ));
        }
        Ok(Self {
            hello,
            start,
            commit,
            close,
            shutdown_grace,
        })
    }

    /// Return the configured Hello timeout.
    #[must_use]
    pub fn hello(self) -> Duration {
        self.hello
    }
    /// Return the configured Start timeout.
    #[must_use]
    pub fn start(self) -> Duration {
        self.start
    }
    /// Return the configured Commit timeout.
    #[must_use]
    pub fn commit(self) -> Duration {
        self.commit
    }
    /// Return the configured Close timeout.
    #[must_use]
    pub fn close(self) -> Duration {
        self.close
    }
    /// Return the configured cooperative shutdown grace.
    #[must_use]
    pub fn shutdown_grace(self) -> Duration {
        self.shutdown_grace
    }
}

/// Structured non-Linux process refusal.
#[derive(Debug)]
pub struct ProcessPluginError {
    detail: String,
}

impl ProcessPluginError {
    /// Return the stable unsupported-process classification.
    #[must_use]
    pub fn code(&self) -> &str {
        "plugin_process_unsupported"
    }
    /// Return the platform refusal diagnostic.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl From<io::Error> for ProcessPluginError {
    fn from(error: io::Error) -> Self {
        Self {
            detail: error.to_string(),
        }
    }
}

impl fmt::Display for ProcessPluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.detail)
    }
}

impl std::error::Error for ProcessPluginError {}

/// Unconstructible backend marker returned only in signatures on unsupported targets.
pub struct ProcessPluginBackend {
    _private: (),
}

impl ProcessPluginBackend {
    /// Refuse process-backend close on this unsupported platform.
    pub fn close(&mut self) -> Result<(), ProcessPluginError> {
        Err(unsupported().into())
    }
}

/// Cancellation authority that truthfully reports unsupported cleanup.
#[derive(Clone)]
pub struct ProcessPluginCancellation {
    _private: (),
}

impl ProcessPluginCancellation {
    /// Return the absent process identifier.
    #[must_use]
    pub fn process_id(&self) -> u32 {
        0
    }

    /// No framed receive can run on an unsupported platform.
    #[must_use]
    pub fn receive_in_progress(&self) -> bool {
        false
    }

    /// No graceful process shutdown can run on an unsupported platform.
    #[must_use]
    pub fn graceful_shutdown_in_progress(&self) -> bool {
        false
    }

    /// Refuse process cancellation on this unsupported platform.
    pub fn cancel(&self) -> io::Result<ExitStatus> {
        Err(unsupported())
    }
}

impl ChatSubscriptionCancellation for ProcessPluginCancellation {
    fn cancel(&self) -> Result<(), CancellationError> {
        Err(CancellationError::CleanupUncertain(
            BackendFailure::new(
                "plugin_process_unsupported",
                unsupported().to_string(),
                false,
            )
            .expect("constant unsupported diagnostic is valid"),
        ))
    }
}

impl ChatSubscriptionBackend for ProcessPluginBackend {
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
        Arc::new(ProcessPluginCancellation { _private: () })
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new(
            "unsupported-process-plugin",
            ReplaySupport::CurrentOnly,
            false,
            NonZeroU16::new(1).expect("one is nonzero"),
            vec![EventKind::Heartbeat],
        )
        .expect("constant unsupported capabilities are valid")
    }

    fn subscribe(
        &mut self,
        _request: &SubscribeRequest,
    ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
        Err(BackendFailure::new(
            "plugin_process_unsupported",
            unsupported().to_string(),
            false,
        )
        .expect("constant unsupported diagnostic is valid"))
    }
}

/// Process child marker whose launch always fails before executing the command.
#[derive(Debug)]
pub struct ProcessPluginChild;

impl ProcessPluginChild {
    /// Refuse launch before executing the supplied command.
    pub fn spawn(_command: Command) -> io::Result<Self> {
        Err(unsupported())
    }

    /// Return the absent supervisor identifier.
    #[must_use]
    pub fn id(&self) -> u32 {
        0
    }

    /// Return the absent executable identifier.
    #[must_use]
    pub fn executable_id(&self) -> u32 {
        0
    }

    /// Refuse executable-status observation on this unsupported platform.
    pub fn executable_status(&self) -> io::Result<Option<ExitStatus>> {
        Err(unsupported())
    }

    /// Refuse access to protocol endpoints on this unsupported platform.
    pub fn take_transport(&mut self) -> io::Result<(File, File)> {
        Err(unsupported())
    }

    /// Refuse protocol connection on this unsupported platform.
    pub fn connect(
        self,
        _timeouts: ProcessPhaseTimeouts,
    ) -> Result<(ProcessPluginBackend, ProcessPluginCancellation), ProcessPluginError> {
        Err(unsupported().into())
    }

    /// Refuse process shutdown on this unsupported platform.
    pub fn shutdown(&mut self, _grace: Duration) -> io::Result<ExitStatus> {
        Err(unsupported())
    }

    /// Refuse cancellable process shutdown on this unsupported platform.
    pub fn shutdown_cancellable(
        &mut self,
        _grace: Duration,
        _cancellation_fd: std::os::fd::RawFd,
    ) -> io::Result<ExitStatus> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_refuses_before_executing_a_command() {
        let error = ProcessPluginChild::spawn(Command::new("must-not-run"))
            .expect_err("non-Linux process plugins are unsupported");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn cancellation_diagnostics_match_the_linux_public_surface() {
        let cancellation = ProcessPluginCancellation { _private: () };
        assert!(!cancellation.receive_in_progress());
        assert!(!cancellation.graceful_shutdown_in_progress());
    }
}
