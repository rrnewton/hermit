//! Ordered, run-owned provider observations. Observation commands are read-only;
//! cancellation retains the exact in-flight request, while a pending kernel
//! queue phase is reobserved rather than published as an empty accept queue.

use std::io;

use detcore_model::network_trace::NetworkAddressV2;
use detcore_model::network_trace::ReceiveTimeoutV3;

use super::accepted_controller::Controller;
use super::accepted_controller::Effect;
use super::accepted_provider::Creation;
use super::accepted_provider::RawState;
use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::accepted_transport::ObservationReceipt;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::NetworkStreamSocketState;
use crate::network_replay::accepted::AcceptedPhysicalIdentity;
use crate::network_replay::accepted::ChildCreationCertificate;

#[derive(Debug, Clone)]
enum Read {
    Status,
    Creation(u32),
}
#[derive(Debug, Clone)]
struct Pending {
    id: u64,
    owner: NetworkStreamOwner,
    read: Read,
}
#[derive(Debug)]
pub(super) struct Creations {
    next_creation: u64,
    next_request: u64,
    pending: Option<Pending>,
    ready: Option<ObservedCreation>,
    failure: Option<String>,
    ready_request: Option<(u64, u64, Vec<u8>)>,
    consumed_request: Option<(u64, u64, Vec<u8>)>,
    retired: Option<ObservationReceipt>,
    final_ack: Option<(NetworkStreamOwner, u64)>,
    finished: bool,
}
impl Default for Creations {
    fn default() -> Self {
        Self {
            next_creation: 1,
            next_request: 1,
            pending: None,
            ready: None,
            failure: None,
            ready_request: None,
            consumed_request: None,
            retired: None,
            final_ack: None,
            finished: false,
        }
    }
}
impl Creations {
    fn prepare(&mut self, owner: NetworkStreamOwner, read: Read) -> io::Result<Pending> {
        if let Some(pending) = &self.pending {
            return Ok(pending.clone());
        }
        let id = self.next_request;
        self.next_request = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("provider observation identity exhausted"))?;
        let pending = Pending { id, owner, read };
        self.pending = Some(pending.clone());
        Ok(pending)
    }
    /// Held under a dedicated observation mutex, never under a guest FD-table,
    /// socket-control, scheduler or engine lock. The owner outlives this future.
    pub(super) async fn next(
        &mut self,
        controller: &Controller,
        owner: NetworkStreamOwner,
        provider: u64,
    ) -> io::Result<Option<ObservedCreation>> {
        if let Some(failure) = &self.failure {
            return Err(io::Error::other(failure.clone()));
        }
        if let Some(ready) = &self.ready {
            return Ok(Some(ready.clone()));
        }
        let result = self.read(controller, owner, provider).await;
        if let Err(error) = &result {
            self.failure = Some(error.to_string());
        }
        result
    }
    fn retire_published(&mut self, controller: &Controller) -> io::Result<()> {
        if let Some((id, sequence, body)) = &self.consumed_request {
            self.retired = Some(controller.retire_observation(*id, *sequence, body)?);
            self.consumed_request = None;
        }
        Ok(())
    }
    async fn read(
        &mut self,
        controller: &Controller,
        owner: NetworkStreamOwner,
        provider: u64,
    ) -> io::Result<Option<ObservedCreation>> {
        if self.final_ack.is_some() || self.finished {
            return Err(io::Error::other(
                "creation drain already entered terminal acknowledgement",
            ));
        }
        self.retire_published(controller)?;
        let ordinal = u32::try_from(self.next_creation)
            .map_err(|_| io::Error::other("provider creation sequence overflow"))?;
        let pending = self.prepare(owner, Read::Creation(ordinal))?;
        let Read::Creation(ordinal) = pending.read else {
            return Err(io::Error::other("unexpected legacy observation request"));
        };
        let request = Request::AwaitCreation {
            sequence: ordinal,
            acknowledged: self.retired.clone(),
        };
        let sequence = controller.prepare(
            Effect::Observation(pending.id),
            pending.owner,
            &request,
            || Ok(vec![]),
        )?;
        let reply = controller.response(sequence).await?;
        let bytes = serde_json::to_vec(&reply)?;
        let Reply::Creation(observation) = reply else {
            return Err(io::Error::other("provider creation response kind changed"));
        };
        if observation.status.returned != 0 {
            return Err(io::Error::other("provider creation read failed"));
        }
        let observed = ObservedCreation::checked(provider, u64::from(ordinal), observation.raw)?
            .ok_or_else(|| {
                io::Error::other("provider replied before creation queue publication")
            })?;
        // No await separates retained result from clearing its in-flight token.
        self.ready_request = Some((pending.id, sequence, bytes));
        self.pending = None;
        self.ready = Some(observed.clone());
        Ok(Some(observed))
    }
    /// Final read retirement does not depend on a later child. Cancellation
    /// reuses this exact terminal request. Aggregate service/resource teardown
    /// still separately requires actual controller exit and custody settlement.
    pub(super) async fn finish(
        &mut self,
        controller: &Controller,
        owner: NetworkStreamOwner,
    ) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        if self.pending.is_some() || self.ready.is_some() || self.failure.is_some() {
            return Err(io::Error::other(
                "creation observation remains unresolved at final acknowledgement",
            ));
        }
        self.retire_published(controller)?;
        let Some(receipt) = self.retired.clone() else {
            self.finished = true;
            return Ok(());
        };
        let (origin, sequence) = match self.final_ack {
            Some(value) => value,
            None => {
                let sequence = controller.prepare(
                    Effect::ObservationRetirement(receipt.sequence),
                    owner,
                    &Request::RetireObservation { receipt },
                    || Ok(vec![]),
                )?;
                self.final_ack = Some((owner, sequence));
                (owner, sequence)
            }
        };
        let _ = origin; // retained envelope identity is never replaced after cancellation
        match controller.response(sequence).await? {
            Reply::Retired => {
                self.retired = None;
                self.finished = true;
                Ok(())
            }
            _ => Err(io::Error::other(
                "terminal observation acknowledgement changed response",
            )),
        }
    }
    /// Called in the same non-awaiting publication block as engine commit.
    pub(super) fn acknowledge(&mut self, evidence: &ObservedCreation) -> io::Result<()> {
        if self.ready.as_ref().map(|r| &r.raw) != Some(&evidence.raw)
            || evidence.raw.sequence != self.next_creation
        {
            return Err(io::Error::other(
                "creation acknowledgement lacks exact retained observation",
            ));
        }
        self.next_creation = self
            .next_creation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("creation cursor exhausted"))?;
        self.ready = None;
        self.consumed_request = self.ready_request.take();
        Ok(())
    }
}

/// Constructed only from a response retained by the authenticated run channel.
/// This proves an observation; the engine still binds it to its listener state.
#[derive(Debug, Clone)]
pub(crate) struct ObservedCreation {
    raw: Creation,
}
impl ObservedCreation {
    fn checked(provider: u64, sequence: u64, raw: Creation) -> io::Result<Option<Self>> {
        let identity = |p: &super::accepted_provider::Identity| {
            p.provider == provider && p.object != 0 && p.namespace != 0
        };
        if provider == 0
            || raw.sequence != sequence
            || !identity(&raw.listener)
            || !identity(&raw.child)
            || raw.listener == raw.child
            || raw.listener.namespace != raw.child.namespace
            || raw.phase & !15 != 0
            || raw.overlap != 0
            || raw.mutation_epoch_enter != raw.mutation_epoch_exit
        {
            return Err(io::Error::other(
                "child observation has unresolved identity or mutation overlap",
            ));
        }
        if raw.phase & 4 != 0 {
            return Err(io::Error::other(
                "retired unacknowledged child needs terminal custody reconciliation",
            ));
        }
        if raw.phase & 3 != 3 {
            return Ok(None);
        }
        if raw.cookie_at_creation == 0
            || raw.listener_before.tcp_state != 10
            || raw.listener_after.tcp_state != 10
            || raw.child_created.tcp_state != 3
            || raw.child_created.child_spin_locked != 1
            || !inherited_equal(&raw.listener_before, &raw.listener_after, false)
            || !inherited_equal(&raw.listener_after, &raw.child_created, true)
            || raw.local.family != libc::AF_INET as u16
            || raw.peer.family != libc::AF_INET as u16
            || raw.local.port == 0
            || raw.peer.port == 0
        {
            return Err(io::Error::other(
                "child observation does not prove completed TCP inheritance",
            ));
        }
        Ok(Some(Self { raw }))
    }
    pub(crate) fn cookie(&self) -> u64 {
        self.raw.cookie_at_creation
    }
    pub(crate) fn sequence(&self) -> u64 {
        self.raw.sequence
    }
    pub(crate) fn listener(&self) -> AcceptedPhysicalIdentity {
        AcceptedPhysicalIdentity {
            provider: self.raw.listener.provider,
            object: self.raw.listener.object,
            namespace: self.raw.listener.namespace,
        }
    }
    pub(crate) fn certificate(
        &self,
        state: &NetworkStreamSocketState,
    ) -> io::Result<ChildCreationCertificate> {
        let raw = &self.raw.listener_before;
        let ticks = |timeout| match timeout {
            ReceiveTimeoutV3::Infinite => Some(i64::MAX),
            ReceiveTimeoutV3::FiniteTicks(value) => i64::try_from(value).ok(),
        };
        if state.key.domain != libc::AF_INET
            || state.key.socket_type != libc::SOCK_STREAM
            || state.key.protocol != libc::IPPROTO_TCP
            || state.option_generation != self.raw.listener_generation
            || ticks(state.options.receive_timeout) != Some(raw.receive_timeout_ticks)
            || state.send_timeout.and_then(ticks) != Some(raw.send_timeout_ticks)
            || state.options.peek_offset != Some(raw.peek_offset)
            || i64::from(raw.lowat) != state.options.receive_low_water as i64
            || i64::from(raw.receive_buffer) != state.options.receive_buffer.bytes as i64
            || (raw.userlocks & 2 != 0) != state.options.receive_buffer.user_locked
            || raw.scaling_ratio != state.options.receive_buffer.tcp_scaling_ratio
        {
            return Err(io::Error::other(
                "child creation does not match listener semantic generation",
            ));
        }
        Ok(ChildCreationCertificate {
            sequence: self.raw.sequence,
            listener: self.listener(),
            child: AcceptedPhysicalIdentity {
                provider: self.raw.child.provider,
                object: self.raw.child.object,
                namespace: self.raw.child.namespace,
            },
            listener_generation: state.option_generation,
            inherited: state.clone(),
            local: NetworkAddressV2::Inet4 {
                address: self.raw.local.address,
                port: self.raw.local.port,
            },
            peer: NetworkAddressV2::Inet4 {
                address: self.raw.peer.address,
                port: self.raw.peer.port,
            },
        })
    }
}
fn inherited_equal(parent: &RawState, child: &RawState, clone: bool) -> bool {
    parent.receive_timeout_ticks == child.receive_timeout_ticks
        && parent.send_timeout_ticks == child.send_timeout_ticks
        && parent.lowat == child.lowat
        && parent.receive_buffer == child.receive_buffer
        && parent.peek_offset == child.peek_offset
        && parent.scaling_ratio == child.scaling_ratio
        && child.userlocks
            == if clone {
                parent.userlocks & !8
            } else {
                parent.userlocks
            }
    // window_clamp is initialized from the connection request, not copied from
    // listener options. omem is native accounting, retained raw but not equality.
}

/// The runtime cursor and the exact ready value remain locked through the
/// engine's non-awaiting publication. Dropping before commit retains the value.
pub(crate) struct Publication<'a> {
    pub(crate) evidence: ObservedCreation,
    pub(super) cursor: tokio::sync::MutexGuard<'a, Creations>,
}
impl Publication<'_> {
    pub(crate) fn acknowledge(mut self) -> io::Result<()> {
        self.cursor.acknowledge(&self.evidence)
    }
}

#[cfg(test)]
impl ObservedCreation {
    pub(crate) fn controlled_fixture(
        listener: AcceptedPhysicalIdentity,
        child: AcceptedPhysicalIdentity,
        sequence: u64,
        cookie: u64,
        state: &NetworkStreamSocketState,
    ) -> Self {
        use super::accepted_provider_ffi as ffi;
        let ticks = |value| match value {
            ReceiveTimeoutV3::Infinite => i64::MAX,
            ReceiveTimeoutV3::FiniteTicks(n) => i64::try_from(n).unwrap(),
        };
        let parent = ffi::RawState {
            receive_timeout_ticks: ticks(state.options.receive_timeout),
            send_timeout_ticks: ticks(state.send_timeout.unwrap()),
            lowat: state.options.receive_low_water as i32,
            receive_buffer: state.options.receive_buffer.bytes as i32,
            peek_offset: state.options.peek_offset.unwrap(),
            userlocks: if state.options.receive_buffer.user_locked {
                2
            } else {
                0
            },
            scaling_ratio: state.options.receive_buffer.tcp_scaling_ratio,
            tcp_state: 10,
            ..Default::default()
        };
        let raw = ffi::Creation {
            sequence,
            listener: ffi::Identity {
                provider: listener.provider,
                object: listener.object,
                namespace: listener.namespace,
            },
            child: ffi::Identity {
                provider: child.provider,
                object: child.object,
                namespace: child.namespace,
            },
            listener_generation: state.option_generation,
            listener_before: parent,
            listener_after: parent,
            child_created: ffi::RawState {
                tcp_state: 3,
                child_spin_locked: 1,
                ..parent
            },
            local: ffi::Endpoint4 {
                address_be: u32::from_ne_bytes([127, 0, 0, 1]),
                port_be: 12345u16.to_be(),
                family: libc::AF_INET as u16,
            },
            peer: ffi::Endpoint4 {
                address_be: u32::from_ne_bytes([127, 0, 0, 1]),
                port_be: 54321u16.to_be(),
                family: libc::AF_INET as u16,
            },
            cookie_at_creation: cookie,
            phase: 3,
            ..Default::default()
        };
        Self::checked(listener.provider, sequence, raw.into())
            .unwrap()
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::network_trace::*;

    use super::*;
    fn owner(n: i32) -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(n);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn fixture() -> (NetworkStreamSocketState, ObservedCreation) {
        let state = NetworkStreamSocketState {
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
        let listener = AcceptedPhysicalIdentity {
            provider: 7,
            object: 10,
            namespace: 9,
        };
        let child = AcceptedPhysicalIdentity {
            object: 11,
            ..listener
        };
        let evidence = ObservedCreation::controlled_fixture(listener, child, 1, 101, &state);
        (state, evidence)
    }
    #[test]
    fn accepted_creation_pending_queue_phase_is_never_a_ready_child() {
        let (_, evidence) = fixture();
        for phase in [0, 1, 9] {
            let mut raw = evidence.raw.clone();
            raw.phase = phase;
            assert!(ObservedCreation::checked(7, 1, raw).unwrap().is_none());
        }
        let mut cursor = Creations::default();
        assert!(cursor.acknowledge(&evidence).is_err());
        assert_eq!(cursor.next_creation, 1);
    }
    #[test]
    fn accepted_creation_raw_nonoverlap_is_required_before_semantic_inheritance() {
        let (_, evidence) = fixture();
        for case in 0..7 {
            let mut raw = evidence.raw.clone();
            match case {
                0 => raw.overlap = 1,
                1 => raw.mutation_epoch_exit += 1,
                2 => raw.listener_after.lowat += 1,
                3 => raw.child_created.send_timeout_ticks += 1,
                4 => raw.child_created.userlocks |= 2,
                5 => raw.child_created.child_spin_locked = 0,
                _ => raw.child_created.tcp_state = 1,
            }
            assert!(ObservedCreation::checked(7, 1, raw).is_err(), "case {case}");
        }
    }
    #[test]
    fn accepted_creation_preserves_native_accounting_and_clone_specific_fields() {
        let (state, evidence) = fixture();
        let mut raw = evidence.raw.clone();
        raw.listener_before.socket_option_memory = 64;
        raw.listener_after.socket_option_memory = 128;
        raw.child_created.socket_option_memory = 512;
        raw.listener_before.window_clamp = 42;
        raw.listener_after.window_clamp = 43;
        raw.child_created.window_clamp = 9000;
        raw.listener_before.userlocks |= 8;
        raw.listener_after.userlocks |= 8;
        let checked = ObservedCreation::checked(7, 1, raw.clone())
            .unwrap()
            .unwrap();
        assert_eq!(checked.raw, raw);
        assert_eq!(checked.certificate(&state).unwrap().inherited, state);
    }
    #[test]
    fn accepted_creation_rejects_wrong_lifetime_retirement_and_endpoint() {
        let (_, evidence) = fixture();
        for case in 0..7 {
            let mut raw = evidence.raw.clone();
            match case {
                0 => raw.sequence += 1,
                1 => raw.child.object = raw.listener.object,
                2 => raw.child.namespace += 1,
                3 => raw.cookie_at_creation = 0,
                4 => raw.phase |= 4,
                5 => raw.peer.family = libc::AF_INET6 as u16,
                _ => raw.local.port = 0,
            }
            assert!(ObservedCreation::checked(7, 1, raw).is_err(), "case {case}");
        }
    }
    #[test]
    fn accepted_creation_does_not_copy_a_later_listener_generation() {
        let (state, evidence) = fixture();
        assert_eq!(
            evidence
                .certificate(&state)
                .unwrap()
                .inherited
                .options
                .receive_timeout,
            ReceiveTimeoutV3::FiniteTicks(0)
        );
        for case in 0..4 {
            let mut current = state.clone();
            match case {
                0 => current.option_generation += 1,
                1 => current.options.receive_low_water = 9,
                2 => current.options.receive_timeout = ReceiveTimeoutV3::Infinite,
                _ => current.send_timeout = Some(ReceiveTimeoutV3::Infinite),
            }
            assert!(evidence.certificate(&current).is_err(), "case {case}");
        }
    }
    #[test]
    fn accepted_creation_cancelled_reader_keeps_original_request_and_cursor() {
        let mut cursor = Creations::default();
        let pending = cursor.prepare(owner(1), Read::Creation(1)).unwrap();
        let resumed = cursor.prepare(owner(2), Read::Status).unwrap();
        assert_eq!(pending.id, resumed.id);
        assert_eq!(resumed.owner, owner(1));
        assert!(matches!(resumed.read, Read::Creation(1)));
        assert_eq!(cursor.next_request, 2);
        assert_eq!(cursor.next_creation, 1);
    }
    #[tokio::test]
    async fn accepted_creation_publication_cancellation_and_exact_ack_are_distinct() {
        let (_, evidence) = fixture();
        let mut state = Creations::default();
        state.ready = Some(evidence.clone());
        let shared = tokio::sync::Mutex::new(state);
        let guard = shared.lock().await;
        drop(Publication {
            evidence: evidence.clone(),
            cursor: guard,
        });
        let mut guard = shared.lock().await;
        assert_eq!(guard.ready.as_ref().unwrap().raw, evidence.raw);
        let mut changed = evidence.clone();
        changed.raw.cookie_at_creation += 1;
        assert!(guard.acknowledge(&changed).is_err());
        assert_eq!(guard.next_creation, 1);
        guard.acknowledge(&evidence).unwrap();
        assert_eq!(guard.next_creation, 2);
        assert!(guard.ready.is_none());
        assert!(guard.acknowledge(&evidence).is_err());
    }
    #[test]
    fn accepted_observer_publication_is_required_before_transport_retirement() {
        let (_, evidence) = fixture();
        let mut cursor = Creations::default();
        cursor.ready = Some(evidence.clone());
        cursor.ready_request = Some((1, 7, b"exact reply".to_vec()));
        let mut bad = evidence.clone();
        bad.raw.sequence += 1;
        assert!(cursor.acknowledge(&bad).is_err());
        assert!(cursor.consumed_request.is_none());
        assert_eq!(cursor.ready_request.as_ref().unwrap().1, 7);
        cursor.acknowledge(&evidence).unwrap();
        assert_eq!(
            cursor.consumed_request,
            Some((1, 7, b"exact reply".to_vec()))
        );
        assert!(cursor.ready_request.is_none());
        assert!(cursor.ready.is_none());
        assert!(cursor.acknowledge(&evidence).is_err());
    }
}
