/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Preserve the distinction between a replay refusal and an engine failure.
//!
//! Only errors produced while checking a guest operation against an admitted
//! replay trace, or completing that replay, can enter the refusal allowlist.
//! A message is never evidence of the class. Record and ownership failures
//! remain internal even when their diagnostic mentions a replay mismatch.

use std::fmt;

use detcore_model::network_trace::NetworkPolicy;
use serde::Deserialize;
use serde::Serialize;

use crate::network_replay::NetworkReplayError;

/// The trusted engine operation which produced an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkFailurePhase {
    /// Match endpoint facts to an admitted trace channel.
    Binding,
    /// Compare guest bytes or message metadata with recorded output.
    Transmit,
    /// Compare the guest shutdown direction and offset with recorded output.
    Shutdown,
    /// Complete a replay after its runtime returned normally.
    Completion,
    /// All other operations, including capture and ownership protocols.
    Other,
}

/// The intentionally small set of understood replay refusals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkRefusalReason {
    /// No unused trace occurrence matches the guest endpoint.
    NoMatchingChannel,
    /// Endpoint facts disagree with an existing or selected trace channel.
    ChannelEndpointMismatch,
    /// Guest output bytes or message metadata differ from the trace.
    OutboundMismatch,
    /// Guest output extends beyond the trace.
    TraceExhausted,
    /// Guest shutdown disagrees with the trace.
    UnexpectedShutdown,
    /// Replay ended with observations still unconsumed or ineligible.
    UnconsumedTrace,
    /// A channel retained unread input or unmatched output at replay exit.
    UnconsumedChannel,
}

/// A deliberate replay refusal, retained across the Tool and CLI boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPolicyRefusal {
    reason: NetworkRefusalReason,
    diagnostic: String,
}

impl NetworkPolicyRefusal {
    /// The typed reason; diagnostic wording does not determine this value.
    pub fn reason(&self) -> NetworkRefusalReason {
        self.reason
    }
}

impl fmt::Display for NetworkPolicyRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl std::error::Error for NetworkPolicyRefusal {}

/// Same-image RPC error envelope. Internal failures never acquire a refusal
/// type merely by crossing the RPC or Tool boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkRpcError {
    /// One allowlisted replay condition refused the run.
    Refusal(NetworkPolicyRefusal),
    /// An engine, adapter, ownership, or protocol invariant failed.
    Internal(String),
}

impl NetworkRpcError {
    /// Preserve an internal diagnostic without granting refusal status.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// Classify at the origin, before serialization erases the engine type.
    pub fn from_engine(
        policy: NetworkPolicy,
        phase: NetworkFailurePhase,
        error: NetworkReplayError,
    ) -> Self {
        let reason = if policy == NetworkPolicy::Replay {
            match (phase, &error) {
                (NetworkFailurePhase::Binding, NetworkReplayError::NoMatchingChannel) => {
                    Some(NetworkRefusalReason::NoMatchingChannel)
                }
                (NetworkFailurePhase::Binding, NetworkReplayError::ChannelEndpointMismatch(_)) => {
                    Some(NetworkRefusalReason::ChannelEndpointMismatch)
                }
                (NetworkFailurePhase::Transmit, NetworkReplayError::OutboundMismatch { .. }) => {
                    Some(NetworkRefusalReason::OutboundMismatch)
                }
                (NetworkFailurePhase::Transmit, NetworkReplayError::TraceExhausted(_)) => {
                    Some(NetworkRefusalReason::TraceExhausted)
                }
                (NetworkFailurePhase::Shutdown, NetworkReplayError::UnexpectedShutdown(_)) => {
                    Some(NetworkRefusalReason::UnexpectedShutdown)
                }
                (NetworkFailurePhase::Completion, NetworkReplayError::UnconsumedTrace) => {
                    Some(NetworkRefusalReason::UnconsumedTrace)
                }
                (NetworkFailurePhase::Completion, NetworkReplayError::UnconsumedChannel(_)) => {
                    Some(NetworkRefusalReason::UnconsumedChannel)
                }
                _ => None,
            }
        } else {
            None
        };
        let diagnostic = error.to_string();
        match reason {
            Some(reason) => Self::Refusal(NetworkPolicyRefusal { reason, diagnostic }),
            None => Self::Internal(diagnostic),
        }
    }

    /// A typed refusal may terminate only its explicitly owned replay controller.
    /// Default library callers still receive this error through the Tool API.
    pub(crate) fn refusal_for_owned_controller(
        &self,
        policy: NetworkPolicy,
        controller_can_exit: bool,
    ) -> Option<&NetworkPolicyRefusal> {
        match self {
            Self::Refusal(refusal) if controller_can_exit && policy == NetworkPolicy::Replay => {
                Some(refusal)
            }
            _ => None,
        }
    }

    /// Reconstitute the type before the Tool/CLI classification boundary.
    pub fn into_error(self) -> anyhow::Error {
        match self {
            Self::Refusal(refusal) => anyhow::Error::new(refusal),
            Self::Internal(message) => anyhow::Error::msg(message),
        }
    }
}

impl fmt::Display for NetworkRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refusal(refusal) => refusal.fmt(formatter),
            Self::Internal(message) => formatter.write_str(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::network_trace::NetworkChannelId;

    use super::*;

    fn allowlisted(
        index: usize,
    ) -> (
        NetworkReplayError,
        NetworkFailurePhase,
        NetworkRefusalReason,
    ) {
        let channel = NetworkChannelId(7);
        match index {
            0 => (
                NetworkReplayError::NoMatchingChannel,
                NetworkFailurePhase::Binding,
                NetworkRefusalReason::NoMatchingChannel,
            ),
            1 => (
                NetworkReplayError::ChannelEndpointMismatch(channel),
                NetworkFailurePhase::Binding,
                NetworkRefusalReason::ChannelEndpointMismatch,
            ),
            2 => (
                NetworkReplayError::OutboundMismatch {
                    channel,
                    offset: 23,
                },
                NetworkFailurePhase::Transmit,
                NetworkRefusalReason::OutboundMismatch,
            ),
            3 => (
                NetworkReplayError::TraceExhausted(channel),
                NetworkFailurePhase::Transmit,
                NetworkRefusalReason::TraceExhausted,
            ),
            4 => (
                NetworkReplayError::UnexpectedShutdown(channel),
                NetworkFailurePhase::Shutdown,
                NetworkRefusalReason::UnexpectedShutdown,
            ),
            5 => (
                NetworkReplayError::UnconsumedTrace,
                NetworkFailurePhase::Completion,
                NetworkRefusalReason::UnconsumedTrace,
            ),
            6 => (
                NetworkReplayError::UnconsumedChannel(channel),
                NetworkFailurePhase::Completion,
                NetworkRefusalReason::UnconsumedChannel,
            ),
            _ => unreachable!(),
        }
    }

    #[test]
    fn refusal_requires_both_replay_mode_and_the_exact_operation_phase() {
        for index in 0..7 {
            for policy in [
                NetworkPolicy::Deny,
                NetworkPolicy::Record,
                NetworkPolicy::Replay,
                NetworkPolicy::UnsafeLive,
            ] {
                for phase in [
                    NetworkFailurePhase::Binding,
                    NetworkFailurePhase::Transmit,
                    NetworkFailurePhase::Shutdown,
                    NetworkFailurePhase::Completion,
                    NetworkFailurePhase::Other,
                ] {
                    let (error, expected_phase, reason) = allowlisted(index);
                    let diagnostic = error.to_string();
                    let result = NetworkRpcError::from_engine(policy, phase, error);
                    assert_eq!(result.to_string(), diagnostic);
                    match result {
                        NetworkRpcError::Refusal(refusal) => {
                            assert_eq!(policy, NetworkPolicy::Replay);
                            assert_eq!(phase, expected_phase);
                            assert_eq!(refusal.reason(), reason);
                        }
                        NetworkRpcError::Internal(_) => {
                            assert!(policy != NetworkPolicy::Replay || phase != expected_phase);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn controller_shutdown_requires_capability_replay_and_exact_refusal_type() {
        for index in 0..7 {
            let (error, phase, reason) = allowlisted(index);
            let refusal = NetworkRpcError::from_engine(NetworkPolicy::Replay, phase, error);
            let same_words = NetworkRpcError::internal(refusal.to_string());
            for policy in [
                NetworkPolicy::Deny,
                NetworkPolicy::Record,
                NetworkPolicy::Replay,
                NetworkPolicy::UnsafeLive,
            ] {
                for capability in [false, true] {
                    assert_eq!(
                        refusal
                            .refusal_for_owned_controller(policy, capability)
                            .map(NetworkPolicyRefusal::reason),
                        (capability && policy == NetworkPolicy::Replay).then_some(reason),
                    );
                    assert!(
                        same_words
                            .refusal_for_owned_controller(policy, capability)
                            .is_none()
                    );
                }
            }
            assert!(
                refusal
                    .into_error()
                    .downcast_ref::<NetworkPolicyRefusal>()
                    .is_some()
            );
        }
    }

    #[test]
    fn protocol_and_storage_errors_remain_internal_in_every_phase() {
        let channel = NetworkChannelId(7);
        for phase in [
            NetworkFailurePhase::Binding,
            NetworkFailurePhase::Transmit,
            NetworkFailurePhase::Shutdown,
            NetworkFailurePhase::Completion,
            NetworkFailurePhase::Other,
        ] {
            for error in [
                NetworkReplayError::WrongMode,
                NetworkReplayError::UnknownChannel(channel),
                NetworkReplayError::ChannelAlreadyBound(channel),
                NetworkReplayError::ChannelRetired(channel),
                NetworkReplayError::TransportMismatch(channel),
                NetworkReplayError::OperationOrderMismatch(channel),
                NetworkReplayError::InvalidChannelSelection,
                NetworkReplayError::ObservedLocalDuringReplay,
                NetworkReplayError::MixedIngressCapture(channel),
                NetworkReplayError::IngressChannelMismatch {
                    bound: channel,
                    supplied: NetworkChannelId(8),
                },
                NetworkReplayError::ConsumerLocalIngressError(libc::EFAULT),
                NetworkReplayError::UnsupportedIngressObservation(channel),
                NetworkReplayError::UnsupportedReceiveFlags(libc::MSG_OOB),
                NetworkReplayError::UnsupportedEpollFlags(libc::EPOLLEXCLUSIVE as u32),
                NetworkReplayError::UnsupportedReadinessMode,
                NetworkReplayError::Overflow,
                NetworkReplayError::Io(std::io::Error::other("outbound mismatch")),
            ] {
                let result = NetworkRpcError::from_engine(NetworkPolicy::Replay, phase, error);
                assert!(matches!(result, NetworkRpcError::Internal(_)));
                assert!(
                    result
                        .into_error()
                        .downcast_ref::<NetworkPolicyRefusal>()
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn rpc_roundtrip_retains_type_and_diagnostic_without_matching_words() {
        let (error, phase, reason) = allowlisted(2);
        let refusal = NetworkRpcError::from_engine(NetworkPolicy::Replay, phase, error);
        let internal = NetworkRpcError::internal(refusal.to_string());
        for original in [refusal, internal] {
            let encoded = serde_json::to_vec(&original).unwrap();
            let restored: NetworkRpcError = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(restored, original);
            let expected_refusal = matches!(original, NetworkRpcError::Refusal(_));
            let error = restored.into_error().context("network operation failed");
            let typed = error.downcast_ref::<NetworkPolicyRefusal>();
            assert_eq!(typed.is_some(), expected_refusal);
            if let Some(typed) = typed {
                assert_eq!(typed.reason(), reason);
            }
            assert!(error.to_string().contains("network operation failed"));
            assert!(format!("{error:#}").contains("OutboundMismatch"));
        }
    }
}
