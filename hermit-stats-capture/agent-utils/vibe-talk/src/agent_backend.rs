//! The slow path: asking a coding agent for more detail than the channel contains.
//!
//! Scope note. This is a SEAM, not a feature. v0 defines the interface, wires an implementation
//! that is honest about being absent, and stops there. The endpoint exists so the voice agent's
//! tool list is stable, and so the day this is built no API shape has to change.
//!
//! The reason it is only a seam: reaching a coding agent means reaching into a developer machine,
//! and that is a much larger security decision than reading a Discord channel. This server is
//! deliberately not co-located with the coding agents and needs no access to their workspaces. Any
//! implementation must keep that property — it should post a question into the channel the agents
//! already watch, or talk to a narrow broker, and never gain a shell on a workstation.

use async_trait::async_trait;

use crate::model::ChannelId;

/// Why a detail request could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum AgentBackendError {
    /// No backend is configured in this deployment.
    #[error("no coding-agent backend is configured: this deployment can read and post to chat, but cannot ask an agent directly")]
    Unavailable,
    /// A configured backend failed.
    #[error("coding-agent backend failed: {0}")]
    Failed(String),
}

/// A way to ask the coding agents behind a channel for more detail.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Ask a question and wait for an answer.
    ///
    /// # Errors
    ///
    /// Returns [`AgentBackendError`] when no backend is configured or the configured one fails.
    async fn ask(&self, channel: &ChannelId, question: &str) -> Result<String, AgentBackendError>;
}

/// The v0 implementation: there is no backend, and it says so.
///
/// It fails rather than fabricating, and rather than quietly returning an empty answer that a
/// voice agent would read as "nothing to report".
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAgentBackend;

#[async_trait]
impl AgentBackend for NoAgentBackend {
    async fn ask(
        &self,
        _channel: &ChannelId,
        _question: &str,
    ) -> Result<String, AgentBackendError> {
        Err(AgentBackendError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_absent_backend_fails_loudly_rather_than_answering_emptily() {
        let result = NoAgentBackend
            .ask(&ChannelId("c".to_owned()), "why did the build fail?")
            .await;
        let error = result.expect_err("must not pretend to answer");
        assert!(matches!(error, AgentBackendError::Unavailable));
        assert!(
            error
                .to_string()
                .contains("no coding-agent backend is configured"),
            "the message must explain the gap to whoever hears it: {error}"
        );
    }
}
