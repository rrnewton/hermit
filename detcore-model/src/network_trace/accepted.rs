//! Explicit child-creation semantics. No old V3 payload acquires this model
//! from a default field, and recorded listener values never override Replay's
//! own semantic listener generation at creation.

use super::*;

/// Portable occurrence identity, unrelated to accepter thread or descriptor.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub struct ChildCreationIdV1(pub u64);

/// Initial send-timeout contract, absent from the unchanged receive-only V3
/// socket profile. Every represented class has one explicit entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshSendTimeoutV1 {
    /// Exact recorded socket class.
    pub key: StreamSocketKeyV3,
    /// Actual fresh semantic value; a getter zero alone is not this proof.
    pub timeout: ReceiveTimeoutV3,
}

/// Guest-causal inherited options, including both timeout directions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InheritedStreamOptionsV1 {
    /// Existing receive semantics, without changing their old wire shape.
    pub receive: StreamSocketOptionsV3,
    /// Semantic send timeout, including finite zero versus infinite.
    pub send_timeout: ReceiveTimeoutV3,
}

/// Explicit inheritance rule applied to the eligible Replay listener state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildInheritanceV1 {
    /// Clone the listener's modeled fields at creation, before accept.
    LinuxTcpListenerV1,
}

/// Complete terminal accounting of a kernel-created queued child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildDispositionV1 {
    /// Exactly one unchanged V2 accept input consumes this child occurrence.
    Accepted {
        /// Portable accepted stream channel.
        channel: NetworkChannelId,
        /// Exact listener input; not an accepter syscall sequence.
        input_ordinal: u64,
    },
    /// The listener's actual final release disposed of an unaccepted child.
    /// This is not permission to discard an accepted stream's input/output.
    UnacceptedListenerClose {
        /// Known physical disposition's shared release boundary.
        release: NetworkReleaseV2,
    },
}

/// One externally created child, before any guest accept operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildCreatedV1 {
    /// Canonical one-based occurrence, shared by all accept consumers.
    pub id: ChildCreationIdV1,
    /// Exact listening channel, never a port-membership guess.
    pub listener: NetworkChannelId,
    /// Actual normalized socket class.
    pub key: StreamSocketKeyV3,
    /// Actual child local address, distinct from a requested bind constraint.
    pub local: NetworkAddressV2,
    /// Actual external peer address.
    pub peer: NetworkAddressV2,
    /// Existing continuous-time/output eligibility; no host timestamp.
    pub release: NetworkReleaseV2,
    /// Input-history cut. Only earlier inputs on this listener constrain the
    /// child; unrelated channels must not become an ordering barrier.
    pub history_prefix: u64,
    /// Replay's current listener state supplies inherited guest effects.
    pub inheritance: ChildInheritanceV1,
    /// Exact final child disposition; no unaccounted queued child at finish.
    pub disposition: ChildDispositionV1,
}

/// Required, explicit extension in the new receive-model variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedStreamModelV1 {
    /// Initial send defaults are explicit and keyed by class, not OFD/order.
    pub fresh_send_timeouts: Vec<FreshSendTimeoutV1>,
    /// Canonical creation order; no recorded accepter identity exists here.
    pub children: Vec<ChildCreatedV1>,
}

impl AcceptedStreamModelV1 {
    /// Validate complete coverage and relationships before runtime mutation.
    pub fn validate(&self, trace: &NetworkTraceV3) -> Result<(), NetworkTraceValidationError> {
        let invalid = || NetworkTraceValidationError::InvalidAcceptedStreamModel;
        if self.fresh_send_timeouts.len() != trace.fresh_stream_profiles.len() {
            return Err(invalid());
        }
        for (send, receive) in self
            .fresh_send_timeouts
            .iter()
            .zip(&trace.fresh_stream_profiles)
        {
            if send.key != receive.key || send.timeout != ReceiveTimeoutV3::Infinite {
                return Err(invalid());
            }
        }
        let epoch = trace.history.epoch_global_time()?;
        let definitions: BTreeMap<_, _> =
            trace.history.channels.iter().map(|c| (c.id, c)).collect();
        let classes: BTreeMap<_, _> = trace
            .channel_socket_classes
            .iter()
            .map(|c| (c.channel, c.key))
            .collect();
        let mut covered = BTreeSet::new();
        let mut last = BTreeMap::new();
        let mut listener_closes = BTreeMap::new();
        for child in &self.children {
            if let ChildDispositionV1::UnacceptedListenerClose { release } = child.disposition {
                if listener_closes
                    .insert(child.listener, release)
                    .is_some_and(|previous| previous != release)
                {
                    return Err(invalid());
                }
            }
        }
        for (index, child) in self.children.iter().enumerate() {
            if child.id.0 != index as u64 + 1
                || child.history_prefix > trace.history.inputs.len() as u64
            {
                return Err(invalid());
            }
            let listener = definitions.get(&child.listener).ok_or_else(invalid)?;
            if listener.role != NetworkEndpointRoleV2::Listener
                || listener.transport != NetworkTransportV2::Tcp
                || classes.get(&child.listener) != Some(&child.key)
            {
                return Err(invalid());
            }
            let domain = |a: &NetworkAddressV2| match a {
                NetworkAddressV2::Inet4 { .. } => 2,
                NetworkAddressV2::Inet6 { .. } => 10,
                _ => -1,
            };
            if domain(&child.local) != child.key.domain
                || domain(&child.peer) != child.key.domain
                || child.release.not_before_global_time < epoch
            {
                return Err(invalid());
            }
            // Listener streams cannot transmit payload; their current model's
            // output frontier is zero. Never accept an unreachable child gate.
            if child.release.after_transmitted_offset != 0 {
                return Err(invalid());
            }
            // This is a cut in the listener's observations, not a global
            // barrier involving unrelated channels' release times.
            for input in trace
                .history
                .inputs
                .iter()
                .filter(|input| input.channel == child.listener)
            {
                if (input.ordinal < child.history_prefix
                    && input.release.not_before_global_time > child.release.not_before_global_time)
                    || (input.ordinal >= child.history_prefix
                        && input.release.not_before_global_time
                            < child.release.not_before_global_time)
                {
                    return Err(invalid());
                }
            }
            if let Some(close) = listener_closes.get(&child.listener) {
                if child.release.not_before_global_time > close.not_before_global_time
                    || trace.history.inputs.iter().any(|input| {
                        input.channel == child.listener
                            && input.release.not_before_global_time > close.not_before_global_time
                    })
                {
                    return Err(invalid());
                }
            }
            if let Some((time, prefix)) = last.insert(
                child.listener,
                (child.release.not_before_global_time, child.history_prefix),
            ) {
                if time > child.release.not_before_global_time || prefix > child.history_prefix {
                    return Err(invalid());
                }
            }
            match &child.disposition {
                ChildDispositionV1::Accepted {
                    channel,
                    input_ordinal,
                } => {
                    let accepted = definitions.get(channel).ok_or_else(invalid)?;
                    let ordinal = usize::try_from(*input_ordinal).map_err(|_| invalid())?;
                    let event = trace.history.inputs.get(ordinal).ok_or_else(invalid)?;
                    if !covered.insert(*channel)
                        || accepted.accepted_from != Some(child.listener)
                        || accepted.local_address.as_ref() != Some(&child.local)
                        || accepted.peer_address.as_ref() != Some(&child.peer)
                        || classes.get(channel) != Some(&child.key)
                        || *input_ordinal < child.history_prefix
                        || event.channel != child.listener
                        || event.release.not_before_global_time
                            < child.release.not_before_global_time
                    {
                        return Err(invalid());
                    }
                    if !matches!(&event.event, NetworkInputKindV2::Accept { accepted, peer: Some(peer), ancillary: None } if accepted == channel && peer == &child.peer)
                    {
                        return Err(invalid());
                    }
                }
                ChildDispositionV1::UnacceptedListenerClose { release } => {
                    if release.not_before_global_time < child.release.not_before_global_time
                        || release.after_transmitted_offset != 0
                    {
                        return Err(invalid());
                    }
                }
            }
        }
        let accepted: BTreeSet<_> = trace
            .history
            .channels
            .iter()
            .filter(|c| {
                c.role == NetworkEndpointRoleV2::Accepted && c.transport == NetworkTransportV2::Tcp
            })
            .map(|c| c.id)
            .collect();
        if covered != accepted {
            return Err(invalid());
        }
        Ok(())
    }
}
