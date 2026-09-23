//! Listener pins are acquired before the original listen can admit a child.
//! Exact semantic OFD/call admission belongs to the engine/table authority; this
//! map retains physical custody and never computes alias or final-close counts.

use std::collections::BTreeMap;
use std::io;

use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamOwner;
use crate::types::OpenFileId;

#[derive(Debug)]
struct Listener<T> {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    fd: i32,
    pin: Option<T>,
    error: Option<String>,
}
#[derive(Debug)]
pub(super) struct Listeners<T>(BTreeMap<OpenFileId, Listener<T>>);
impl<T> Default for Listeners<T> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}
impl<T> Listeners<T> {
    pub(super) fn capture(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        open_file: OpenFileId,
        fd: i32,
        acquire: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<()> {
        // Only an authenticated current call to this same OFD may reuse a pin.
        // Never reacquire a failed capture from a possibly reused numeric FD.
        if let Some(prior) = self.0.get(&open_file) {
            return if prior.pin.is_some() {
                Ok(())
            } else {
                Err(io::Error::other(prior.error.clone().unwrap_or_else(|| {
                    "listener capture remains submitted".into()
                })))
            };
        }
        self.0.insert(
            open_file,
            Listener {
                owner,
                call,
                fd,
                pin: None,
                error: None,
            },
        );
        match acquire() {
            Ok(pin) => {
                self.0.get_mut(&open_file).unwrap().pin = Some(pin);
                Ok(())
            }
            Err(error) => {
                self.0.get_mut(&open_file).unwrap().error = Some(error.to_string());
                Err(error)
            }
        }
    }
    pub(super) fn pin(&self, open_file: OpenFileId) -> io::Result<(NetworkStreamOwner, &T)> {
        let entry = self
            .0
            .get(&open_file)
            .ok_or_else(|| io::Error::other("listener has no capture receipt"))?;
        let pin = entry
            .pin
            .as_ref()
            .ok_or_else(|| io::Error::other("listener pin acquisition unresolved"))?;
        Ok((entry.owner, pin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(17);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    #[test]
    fn accepted_listener_pin_survives_waiter_loss_without_numeric_reacquisition() {
        let mut listeners = Listeners::default();
        let ofd = OpenFileId::new_socket(owner().thread, 51);
        let call = NetworkStreamCallId::controlled_fixture(2);
        listeners.capture(owner(), call, ofd, 4, || Ok(99)).unwrap();
        listeners
            .capture(owner(), call, ofd, 4, || {
                panic!("listener reacquired after cancellation")
            })
            .unwrap();
        assert_eq!(*listeners.pin(ofd).unwrap().1, 99);
        let receipt = listeners.0.get(&ofd).unwrap();
        assert_eq!(receipt.call, call);
        assert_eq!(receipt.fd, 4);
    }
    #[test]
    fn accepted_listener_failed_capture_remains_unknown_after_fd_reuse() {
        let mut listeners = Listeners::<u64>::default();
        let ofd = OpenFileId::new_socket(owner().thread, 51);
        let call = NetworkStreamCallId::controlled_fixture(2);
        assert!(
            listeners
                .capture(owner(), call, ofd, 4, || Err(io::Error::other(
                    "unknown capture"
                )))
                .is_err()
        );
        assert!(
            listeners
                .capture(owner(), call, ofd, 4, || panic!("reused FD captured"))
                .is_err()
        );
        assert!(listeners.pin(ofd).is_err());
    }
}

/// Only a checked response from the run-owned provider can construct this.
/// It certifies enrollment, not descriptor installation or whole-run capability.
#[derive(Debug)]
pub(crate) struct Enrollment {
    open_file: OpenFileId,
    physical: crate::network_replay::accepted::AcceptedPhysicalIdentity,
    state: crate::network_replay::NetworkStreamSocketState,
}
impl Enrollment {
    pub(crate) fn parts(
        &self,
    ) -> (
        OpenFileId,
        crate::network_replay::accepted::AcceptedPhysicalIdentity,
        &crate::network_replay::NetworkStreamSocketState,
    ) {
        (self.open_file, self.physical, &self.state)
    }
    pub(super) fn checked(
        open_file: OpenFileId,
        state: &crate::network_replay::NetworkStreamSocketState,
        expected_provider: u64,
        observation: super::accepted_provider::Observation<super::accepted_provider::CommandResult>,
    ) -> io::Result<Self> {
        use detcore_model::network_trace::ReceiveTimeoutV3;
        let ticks = |timeout| match timeout {
            ReceiveTimeoutV3::Infinite => Some(i64::MAX),
            ReceiveTimeoutV3::FiniteTicks(value) => i64::try_from(value).ok(),
        };
        let observed = &observation.raw;
        let physical = crate::network_replay::accepted::AcceptedPhysicalIdentity {
            provider: observed.identity.provider,
            object: observed.identity.object,
            namespace: observed.identity.namespace,
        };
        let raw = &observed.state;
        if observation.status.returned != 0 || observed.returned != 0 || observed.phase != 1
            || observed.operation != 1 || observed.command == 0 || observed.task == 0
            || observed.start_boottime == 0 || observed.cookie == 0 || observed.creation != 0
            || observed.reserved != 0 || expected_provider == 0 || physical.provider != expected_provider
            || physical.object == 0 || physical.namespace == 0
            || state.key.domain != libc::AF_INET || state.key.protocol != libc::IPPROTO_TCP
            || state.key.socket_type != libc::SOCK_STREAM
            || raw.tcp_state != 7 // pre-listen TCP_CLOSE, never late child enrollment
            || Some(raw.receive_timeout_ticks) != ticks(state.options.receive_timeout)
            || state.send_timeout.and_then(ticks) != Some(raw.send_timeout_ticks)
            || raw.lowat as i64 != state.options.receive_low_water as i64
            || raw.receive_buffer as i64 != state.options.receive_buffer.bytes as i64
            || state.options.peek_offset != Some(raw.peek_offset)
            || (raw.userlocks & 2 != 0) != state.options.receive_buffer.user_locked
            || raw.scaling_ratio != state.options.receive_buffer.tcp_scaling_ratio
        {
            return Err(io::Error::other(
                "provider listener enrollment does not match its admitted semantic state",
            ));
        }
        Ok(Self {
            open_file,
            physical,
            state: state.clone(),
        })
    }
}

#[cfg(test)]
mod evidence_tests {
    use detcore_model::network_trace::*;

    use super::*;
    use crate::network_runtime::accepted_provider::CallStatus;
    use crate::network_runtime::accepted_provider::CommandResult;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider_ffi as ffi;

    fn fixture() -> (
        OpenFileId,
        crate::network_replay::NetworkStreamSocketState,
        Observation<CommandResult>,
    ) {
        let thread = crate::types::DetTid::from_raw(51);
        let state = crate::network_replay::NetworkStreamSocketState {
            key: StreamSocketKeyV3 {
                transport: NetworkTransportV2::Tcp,
                domain: libc::AF_INET,
                socket_type: libc::SOCK_STREAM,
                protocol: libc::IPPROTO_TCP,
            },
            normalization: LinuxReceiveNormalizationV3 {
                hz: LinuxReceiveHzV3::Hz1000,
                peek_offset_set_supported: true,
                system_rmem_max: 20971520,
                namespace_tcp_rmem_max: 20971520,
                minimum_receive_buffer: 2304,
            },
            options: StreamSocketOptionsV3 {
                peek_offset: Some(8),
                receive_low_water: 3,
                receive_timeout: ReceiveTimeoutV3::FiniteTicks(0),
                receive_buffer: ReceiveBufferStateV3 {
                    bytes: 262144,
                    user_locked: false,
                    tcp_scaling_ratio: 128,
                },
            },
            consume_epoch: 0,
            send_timeout: Some(ReceiveTimeoutV3::FiniteTicks(2000)),
            option_generation: 4,
        };
        let raw = ffi::CommandResult {
            command: 1,
            operation: 1,
            task: 9,
            start_boottime: 45,
            identity: ffi::Identity {
                provider: 7,
                object: 1,
                namespace: 2,
            },
            cookie: 11,
            phase: 1,
            state: ffi::RawState {
                receive_timeout_ticks: 0,
                send_timeout_ticks: 2000,
                lowat: 3,
                receive_buffer: 262144,
                peek_offset: 8,
                scaling_ratio: 128,
                tcp_state: 7,
                ..Default::default()
            },
            ..Default::default()
        };
        let observation = Observation {
            status: CallStatus {
                operation: "enroll".into(),
                returned: 0,
                errno: None,
            },
            raw: raw.into(),
        };
        (OpenFileId::new_socket(thread, 0), state, observation)
    }
    #[test]
    fn accepted_listener_evidence_preserves_finite_zero_and_send_timeout_generation() {
        let (ofd, state, observation) = fixture();
        let receipt = Enrollment::checked(ofd, &state, 7, observation).unwrap();
        assert_eq!(receipt.parts().0, ofd);
        assert_eq!(receipt.parts().1.object, 1);
        assert_eq!(receipt.parts().2, &state);
        assert_eq!(
            receipt.parts().2.options.receive_timeout,
            ReceiveTimeoutV3::FiniteTicks(0)
        );
        assert_eq!(
            receipt.parts().2.send_timeout,
            Some(ReceiveTimeoutV3::FiniteTicks(2000))
        );
        assert_eq!(receipt.parts().2.option_generation, 4);
    }
    #[test]
    fn accepted_listener_evidence_retains_native_accounting_outside_option_equality() {
        let (ofd, state, observation) = fixture();
        for native_charge in [0, 328, 1024] {
            let mut observed = observation.clone();
            observed.raw.state.socket_option_memory = native_charge;
            // Accounting neutrality belongs to the exact provider's causal
            // qualification; absolute native charge is not a socket option.
            // The full raw observation remains in the transport reply ledger.
            let evidence = Enrollment::checked(ofd, &state, 7, observed).unwrap();
            assert_eq!(evidence.parts().2, &state);
        }
    }

    #[test]
    fn accepted_listener_evidence_refuses_late_partial_or_mismatched_snapshot() {
        let (ofd, state, observation) = fixture();
        for cause in [
            "provider",
            "late listen",
            "soft timeout",
            "peek",
            "buffer",
            "unknown",
            "partial",
        ] {
            let mut changed = observation.clone();
            match cause {
                "provider" => changed.raw.identity.provider = 8,
                "late listen" => changed.raw.state.tcp_state = 10,
                "soft timeout" => changed.raw.state.receive_timeout_ticks = i64::MAX,
                "peek" => changed.raw.state.peek_offset = -1,
                "buffer" => changed.raw.state.receive_buffer = 262145,
                "unknown" => changed.status.returned = -1,
                "partial" => changed.raw.phase = 0,
                _ => unreachable!(),
            }
            assert!(
                Enrollment::checked(ofd, &state, 7, changed).is_err(),
                "{cause}"
            );
        }
    }
}
