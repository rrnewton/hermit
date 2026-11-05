//! Typed failures at an external agent runtime boundary.
use std::fmt;
/// Input remains queued because its target is not ready.
pub const EXIT_BUSY: i32 = 75;
/// Input may have been submitted and is quarantined rather than replayed.
pub const EXIT_TIMEOUT: i32 = 76;
/// What an adapter failure means for input that was being written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterErrorKind {
    /// The runtime is unavailable or answered in a way that cannot be verified.
    Unavailable,
    /// Herdr refused input because the pane no longer held the expected terminal;
    /// nothing was written.
    ExpectationFailed,
    /// The pane stopped holding the verified recipient before an input effect.
    RecipientChanged,
    /// Nothing was typed, so the prompt remains safe to retry.
    NotStaged,
    /// Input was written, then the pane failed its recipient check: it may have
    /// reached another program.
    ProbableMisroute,
    /// Input reached the wrong program, which was interrupted and told to ignore it;
    /// the message was not delivered to its recipient and may be retried.
    MisrouteRecovered,
}
/// Adapter failure with a stable process exit code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterError {
    message: String,
    kind: AdapterErrorKind,
}
impl AdapterError {
    /// Record an unavailable runtime or unverifiable runtime response.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::with_kind(AdapterErrorKind::Unavailable, message)
    }
    /// Record a Herdr refusal of input whose expected terminal did not match.
    pub fn expectation_failed(message: impl Into<String>) -> Self {
        Self::with_kind(AdapterErrorKind::ExpectationFailed, message)
    }
    /// Record a recipient check that failed around an input effect.
    pub fn recipient_changed(message: impl Into<String>) -> Self {
        Self::with_kind(AdapterErrorKind::RecipientChanged, message)
    }
    /// Record a refusal that happened before anything was typed.
    pub fn not_staged(message: impl Into<String>) -> Self {
        Self::with_kind(AdapterErrorKind::NotStaged, message)
    }
    /// Record input that was written before its recipient check failed.
    pub fn probable_misroute(message: impl Into<String>) -> Self {
        Self::with_kind(AdapterErrorKind::ProbableMisroute, message)
    }
    /// Record input that reached the wrong program, which was told to ignore it.
    pub fn misroute_recovered(message: impl Into<String>) -> Self {
        Self::with_kind(AdapterErrorKind::MisrouteRecovered, message)
    }
    fn with_kind(kind: AdapterErrorKind, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }
    /// What the failure means for input that was being written.
    pub const fn kind(&self) -> AdapterErrorKind {
        self.kind
    }
    /// Human-readable failure detail.
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Unavailable runtime status (`EX_UNAVAILABLE`).
    pub const fn exit_code(&self) -> i32 {
        69
    }
}
impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for AdapterError {}
/// Result of a runtime adapter operation.
pub type Result<T> = std::result::Result<T, AdapterError>;
