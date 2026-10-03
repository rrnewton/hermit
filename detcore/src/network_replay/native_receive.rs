//! Actual receive byte coordinates are private physical history. They do not
//! consume the semantic queue, issue a rollback unit, or publish readiness.
use std::ops::Deref;
use std::ops::DerefMut;

use super::*;
use crate::network_runtime::original_installation::FileIdentity;
use crate::network_runtime::original_installation::Installation;
use crate::network_runtime::original_installation::Source;
use crate::network_runtime::original_read_copy::CONSUME;
use crate::network_runtime::original_read_copy::NativeAttempt;
use crate::types::FdSlotBinding;

fn invalid(message: &str) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(message.into())
}

/// The existing socket entry owns both the public profile and private native
/// provenance. Cloning/serializing the public profile cannot recreate origin.
#[derive(Debug, Clone)]
pub(super) struct Socket {
    pub(super) profile: NetworkStreamSocketState,
    native: Option<NativeReceive>,
}
impl Socket {
    pub(super) fn new(profile: NetworkStreamSocketState) -> Self {
        Self {
            profile,
            native: None,
        }
    }
}
impl Deref for Socket {
    type Target = NetworkStreamSocketState;
    fn deref(&self) -> &Self::Target {
        &self.profile
    }
}
impl DerefMut for Socket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.profile
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cut {
    bytes: u64,
    order: u64,
}
impl Cut {
    const ZERO: Self = Self { bytes: 0, order: 0 };
}
#[derive(Debug, Clone)]
struct NativeReceive {
    identity: FileIdentity,
    binding: FdSlotBinding,
    birth: NetworkStreamCallId,
    physical_observed: Cut,
}

/// One existing Call retains the opaque raw owner and its exact observation.
/// `joined` means its physical cut is known, never semantic completion.
#[derive(Debug, Clone)]
pub(super) struct RetainedAttempt {
    receipt: NativeAttempt,
    joined: bool,
}
impl RetainedAttempt {
    fn before(&self) -> Cut {
        let begin = self
            .receipt
            .unit()
            .observation
            .expect("opaque copy5 issuer")
            .begin;
        Cut {
            bytes: begin.before,
            order: begin.order,
        }
    }
    fn after(&self) -> Cut {
        let unit = self.receipt.unit();
        Cut {
            bytes: unit.observation.expect("opaque copy5 issuer").after,
            order: unit.native.order,
        }
    }
    fn consumes(&self) -> bool {
        let unit = &self.receipt.unit().native;
        unit.disposition == CONSUME && unit.returned == 0
    }
}

impl NetworkReplayEngine {
    /// Called only inside the original installation transaction. The private
    /// receipt has already passed exact publication/source/profile checks.
    pub(super) fn register_original_receive_socket(
        &mut self,
        binding: FdSlotBinding,
        receipt: &Installation,
        fresh: original_installation::FreshStreamEnrollment,
    ) -> Result<(), NetworkReplayError> {
        let Source::Socket(birth) = receipt.source() else {
            return Err(invalid(
                "receive origin requires actual original Socket installation",
            ));
        };
        if !self.shadow_mode()
            || binding.slot.fd != receipt.fd()
            || binding.slot.files != receipt.files()
            || !binding.open_file.is_socket()
        {
            return Err(invalid(
                "receive origin changed its original installed binding",
            ));
        }
        if self.mode() == NetworkEngineMode::Replay {
            // Preserve the existing recorded-profile enrollment. A Replay
            // placeholder never acquires native byte-zero authority.
            self.register_stream_socket(
                binding.open_file,
                fresh.key,
                fresh.namespace,
                fresh.observed_profile,
            )?;
            return Ok(());
        }
        let fresh_key = fresh.key;
        let identity = receipt.file_identity();
        let prior = self
            .shadow
            .as_ref()
            .unwrap()
            .sockets
            .get(&binding.open_file)
            .and_then(|socket| socket.native.as_ref());
        if prior.is_some_and(|prior| {
            prior.identity != identity || prior.binding != binding || prior.birth != birth
        }) {
            return Err(invalid(
                "receive origin cannot replace another native incarnation",
            ));
        }
        // No new fallible operation follows profile enrollment. In particular
        // actual ACK later prunes the journal without resetting this origin.
        self.register_stream_socket_profile(
            binding.open_file,
            fresh.key,
            fresh.namespace,
            fresh.observed_profile,
        )?;
        self.retain_native_fresh_send(fresh_key);
        let socket = self
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&binding.open_file)
            .unwrap();
        if socket.native.is_none() {
            socket.native = Some(NativeReceive {
                identity,
                binding,
                birth,
                physical_observed: Cut::ZERO,
            });
        }
        Ok(())
    }

    /// Same-store handles are extracted before the engine lock. All validation
    /// and planning precede mutation; no custody/Held mutex is acquired here.
    pub(crate) fn retain_native_receive_attempts(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        incoming: &[NativeAttempt],
    ) -> Result<(), NetworkReplayError> {
        if incoming.is_empty() {
            return Ok(());
        } // Historical copy4 is unchanged.
        let state = self
            .stream_calls
            .get(&call)
            .filter(|state| state.owner == owner)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        let open_file = state
            .open_file
            .ok_or_else(|| invalid("native receive Call has no retained OFD"))?;
        if self.mode() != NetworkEngineMode::Record {
            return Err(invalid(
                "native receive history requires the actual recorder",
            ));
        }
        let socket = self
            .shadow
            .as_ref()
            .and_then(|shadow| shadow.sockets.get(&open_file))
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(open_file))?;
        let origin = socket.native.as_ref().ok_or_else(|| {
            invalid("native receive lacks original installed zero-point authority")
        })?;
        let transport = match socket.key.transport {
            NetworkTransportV2::Tcp => 1,
            NetworkTransportV2::UnixStream => 2,
            _ => {
                return Err(invalid(
                    "native receive origin has unsupported stream transport",
                ));
            }
        };
        // A BTreeMap makes inspection deterministic, but ordering is proved by
        // native cuts below. Iteration order never decides a successor.
        let mut calls: BTreeMap<_, _> = self
            .stream_calls
            .iter()
            .filter(|(_, state)| state.open_file == Some(open_file))
            .map(|(id, state)| (*id, state.native_receive.clone()))
            .collect();
        let retained = calls.get_mut(&call).expect("validated existing Call");
        for receipt in incoming {
            if state.original.is_some() {
                if self.original_receive_attempt_file(owner, call, receipt)? != open_file {
                    return Err(invalid("original receive changed its retained OFD"));
                }
            } else if state
                .helper_copy
                .as_ref()
                .is_none_or(|binding| !binding.owns_attempt(receipt))
            {
                return Err(invalid(
                    "helper receive changed its exact pending raw owner",
                ));
            }
            let unit = receipt.unit();
            let observation = unit
                .observation
                .ok_or_else(|| invalid("native receive lost copy5 geometry"))?;
            if !origin
                .identity
                .matches(receipt.selection().provider, receipt.selection().file)
                || observation.begin.file != receipt.selection().file
                || observation.begin.transport != transport
            {
                return Err(invalid(
                    "native receive changed installed file or protocol identity",
                ));
            }
            if let Some(prior) = retained.iter().find(|prior| {
                prior.receipt.same_command(receipt) && prior.receipt.ordinal() == receipt.ordinal()
            }) {
                if prior.receipt.same(receipt) {
                    continue;
                }
                return Err(invalid(
                    "native receive ordinal replaced its canonical receipt",
                ));
            }
            let expected = retained
                .iter()
                .filter(|prior| prior.receipt.same_command(receipt))
                .count();
            if receipt.ordinal() != expected {
                return Err(invalid(
                    "native receive skipped its exact local completed prefix",
                ));
            }
            retained.push(RetainedAttempt {
                receipt: receipt.clone(),
                joined: false,
            });
        }
        // Distinct commands cannot both own the same successful-unit order,
        // even when its predecessor has not arrived yet. Refuse that conflict
        // now rather than retain it as an apparently viable pending prefix.
        let mut consume_orders = BTreeSet::new();
        for attempts in calls.values() {
            for attempt in attempts.iter().filter(|attempt| attempt.consumes()) {
                if !consume_orders.insert(attempt.before().order) {
                    return Err(invalid("two native receives claim the same consume order"));
                }
            }
        }
        let mut physical = origin.physical_observed;
        loop {
            let mut successor = None;
            for (id, attempts) in &calls {
                for (index, attempt) in attempts
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| !a.joined && a.consumes())
                {
                    let before = attempt.before();
                    if before == physical {
                        if successor.replace((*id, index)).is_some() {
                            return Err(invalid(
                                "two native receives claim the same consume successor",
                            ));
                        }
                    } else if before.order <= physical.order || before.bytes <= physical.bytes {
                        return Err(invalid(
                            "unjoined native receive regressed or contradicted physical frontier",
                        ));
                    }
                }
            }
            let Some((id, index)) = successor else {
                break;
            };
            let attempt = &mut calls.get_mut(&id).unwrap()[index];
            let after = attempt.after();
            // The parser already checked End; keep arithmetic local to this
            // transaction too, without trusting a numeric apparent successor.
            if physical.order.checked_add(1) != Some(after.order)
                || physical
                    .bytes
                    .checked_add(attempt.receipt.unit().native.copied)
                    != Some(after.bytes)
                || after.bytes <= physical.bytes
            {
                return Err(invalid(
                    "native receive successor overflowed or failed to consume",
                ));
            }
            attempt.joined = true;
            physical = after;
        }
        let mut cuts = vec![Cut::ZERO, physical];
        for attempts in calls.values() {
            for attempt in attempts.iter().filter(|attempt| attempt.joined) {
                cuts.push(attempt.before());
                cuts.push(attempt.after());
            }
        }
        for attempts in calls.values_mut() {
            for attempt in attempts
                .iter_mut()
                .filter(|attempt| !attempt.joined && !attempt.consumes())
            {
                let before = attempt.before();
                if cuts.contains(&before) {
                    attempt.joined = true;
                } else if before.order <= physical.order || before.bytes <= physical.bytes {
                    // A historical cut whose owner has retired is not recreated
                    // from the numbers in this later observation.
                    return Err(invalid(
                        "native receive historical cut is absent or contradictory",
                    ));
                }
            }
        }
        // Physical history only. No channel queue, semantic consume count,
        // readiness, trace, return or receipt-discharge state is touched.
        self.shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&open_file)
            .unwrap()
            .native
            .as_mut()
            .unwrap()
            .physical_observed = physical;
        for (id, attempts) in calls {
            self.stream_calls
                .get_mut(&id)
                .expect("same transaction retains Calls")
                .native_receive = attempts;
        }
        Ok(())
    }
}

#[path = "native_receive/versioned.rs"]
mod versioned;
pub(crate) use versioned::shared_send;
pub(super) use versioned::NativeEntry;
pub(super) use versioned::NativeEntryMarker;
pub(super) use versioned::NativeState;
pub(super) use versioned::shared_attempt::SharedAttempt;
pub(crate) use versioned::shared_waits;

#[path = "native_receive/private_peek.rs"]
mod private_peek;
pub(super) use private_peek::PrivateSource;
#[path = "native_receive/foreground_store.rs"]
mod foreground_store;
pub(crate) use foreground_store::ForegroundStore;
pub(crate) use foreground_store::ForegroundStoreSource;
pub(crate) use foreground_store::FullStoreCompletion;
pub(crate) use foreground_store::StoreOutcome;
#[path = "native_receive/replay_store.rs"]
mod replay_store;
pub(crate) use replay_store::ReplayReceivePlan;
pub(crate) use replay_store::ReplayStoreSource;
#[path = "native_receive/private_drain.rs"]
mod private_drain;
pub(super) use private_drain::PrivateDrain;
#[path = "native_receive/private_publish.rs"]
mod private_publish;
pub(crate) use private_publish::PreparedPrivatePublication;

#[cfg(test)]
#[path = "native_receive/tests.rs"]
mod tests;

#[path = "native_receive/no_store.rs"]
mod no_store;
pub(crate) use no_store::CompletedNoStore;
pub(crate) use no_store::NoStoreReturn;
pub(crate) use no_store::ReceiveSelection;
pub(crate) use no_store::RecordNoStore;
pub(crate) use no_store::RecordReceiveRetry;
