//! Concrete service-side ap_* operations. The private session inbox owns every
//! transferred socket/pidfd before this code borrows it. Raw provider errors and
//! partial observations are returned without certifying or silently retrying.

mod registration;

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use serde::Deserialize;
use serde::Serialize;

use super::accepted_parent::ProviderArtifact;
use super::accepted_parent::ProviderReady;
use super::accepted_provider_ffi as ffi;
use super::accepted_transport::Envelope;
use super::accepted_transport::ObservationReceipt;
use super::accepted_transport::Operation;
use crate::network_replay::NetworkAcceptLeaseId;
use crate::network_replay::NetworkStreamOwner;

#[derive(Debug)]
struct MatchRequest {
    owner: NetworkStreamOwner,
    sequence: u64,
    observed: Option<Vec<u8>>,
    error: Option<String>,
}
#[derive(Debug, Default)]
struct MatchRequests(BTreeMap<NetworkAcceptLeaseId, MatchRequest>);
impl MatchRequests {
    fn observe(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        sequence: u64,
        effect: impl FnOnce() -> io::Result<Vec<u8>>,
    ) -> io::Result<Vec<u8>> {
        if let Some(prior) = self.0.get(&lease) {
            if prior.owner != owner || prior.sequence != sequence {
                return Err(io::Error::other(
                    "accepted matching cannot move to a fresh provider command",
                ));
            }
            return prior.observed.clone().ok_or_else(|| {
                io::Error::other(prior.error.clone().unwrap_or_else(|| {
                    "accepted matching remains submitted with unknown result".into()
                }))
            });
        }
        self.0.insert(
            lease,
            MatchRequest {
                owner,
                sequence,
                observed: None,
                error: None,
            },
        );
        match effect() {
            Ok(bytes) => {
                self.0.get_mut(&lease).unwrap().observed = Some(bytes.clone());
                Ok(bytes)
            }
            Err(error) => {
                self.0.get_mut(&lease).unwrap().error = Some(error.to_string());
                Err(error)
            }
        }
    }
}

macro_rules! wire {
    ($name:ident, $raw:path, {$($field:ident : $ty:ty),* $(,)?}) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub(super) struct $name {$(pub $field: $ty),*}
        impl From<$raw> for $name {
            fn from(raw:$raw) -> Self { Self {$($field:raw.$field.into()),*} }
        }
    }
}
wire!(Identity,ffi::Identity,{provider:u64,object:u64,namespace:u64});
impl From<Identity> for ffi::Identity {
    fn from(value: Identity) -> Self {
        Self {
            provider: value.provider,
            object: value.object,
            namespace: value.namespace,
        }
    }
}
wire!(RawState,ffi::RawState,{
    receive_timeout_ticks:i64,send_timeout_ticks:i64,lowat:i32,receive_buffer:i32,
    peek_offset:i32,socket_option_memory:i32,window_clamp:u32,userlocks:u8,
    scaling_ratio:u8,tcp_state:u8,child_spin_locked:u8,
});
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Endpoint4 {
    pub address: [u8; 4],
    pub port: u16,
    pub family: u16,
}
impl From<ffi::Endpoint4> for Endpoint4 {
    fn from(raw: ffi::Endpoint4) -> Self {
        Self {
            address: raw.address_be.to_ne_bytes(),
            port: u16::from_be(raw.port_be),
            family: raw.family,
        }
    }
}
wire!(Creation,ffi::Creation,{
    sequence:u64,listener:Identity,child:Identity,listener_generation:u64,
    mutation_epoch_enter:u64,mutation_epoch_exit:u64,overlap:u64,
    listener_before:RawState,listener_after:RawState,child_created:RawState,
    local:Endpoint4,peer:Endpoint4,cookie_at_creation:u64,phase:u64,
});
wire!(CommandResult,ffi::CommandResult,{
    command:u64,operation:u64,task:u64,start_boottime:u64,identity:Identity,
    creation:u64,cookie:u64,state:RawState,returned:i32,reserved:u32,phase:u64,
});
impl From<RawState> for ffi::RawState {
    fn from(value: RawState) -> Self {
        Self {
            receive_timeout_ticks: value.receive_timeout_ticks,
            send_timeout_ticks: value.send_timeout_ticks,
            lowat: value.lowat,
            receive_buffer: value.receive_buffer,
            peek_offset: value.peek_offset,
            socket_option_memory: value.socket_option_memory,
            window_clamp: value.window_clamp,
            userlocks: value.userlocks,
            scaling_ratio: value.scaling_ratio,
            tcp_state: value.tcp_state,
            child_spin_locked: value.child_spin_locked,
        }
    }
}
impl From<CommandResult> for ffi::CommandResult {
    fn from(value: CommandResult) -> Self {
        Self {
            command: value.command,
            operation: value.operation,
            task: value.task,
            start_boottime: value.start_boottime,
            identity: value.identity.into(),
            creation: value.creation,
            cookie: value.cookie,
            state: value.state.into(),
            returned: value.returned,
            reserved: value.reserved,
            phase: value.phase,
        }
    }
}
wire!(Status,ffi::Status,{
    fatal:u64,next_object:u64,next_creation:u64,clone_entries:u64,clone_null_returns:u64,
    created:u64,queued:u64,retired:u64,matched:u64,setters_entered:u64,setters_exited:u64,
});
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CallStatus {
    pub operation: String,
    pub returned: i32,
    pub errno: Option<i32>,
}
impl From<ffi::CallStatus> for CallStatus {
    fn from(value: ffi::CallStatus) -> Self {
        Self {
            operation: value.operation.into(),
            returned: value.returned,
            errno: value.errno,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Observation<T> {
    pub status: CallStatus,
    pub raw: T,
}
impl<T, U: From<T>> From<ffi::Observation<T>> for Observation<U> {
    fn from(value: ffi::Observation<T>) -> Self {
        Self {
            status: value.status.into(),
            raw: value.raw.into(),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Request {
    Enroll {
        generation: u64,
    },
    ReadStatus,
    ReadCreation {
        sequence: u32,
    },
    PrepareSetter {
        identity: Identity,
        before: u64,
        after: u64,
        level: i32,
        option: i32,
    },
    FinishSetter {
        command: u64,
        prepared_request: u64,
    },
    ResolveAccepted,
    AwaitCreation {
        sequence: u32,
        acknowledged: Option<ObservationReceipt>,
    },
    RetireObservation {
        receipt: ObservationReceipt,
    },
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Reply {
    Command(Observation<CommandResult>),
    Prepared(Observation<u64>),
    Status(Observation<Status>),
    Creation(Observation<Creation>),
    Retired,
}

/// Internal inbox maintenance outcome; never replaces the guest/provider
/// primary response and never enters the public request/reply wire format.
#[derive(Debug, Serialize, Deserialize)]
enum CommandAcknowledgement {
    NotCollected,
    Observed(CallStatus),
}

fn acknowledge_response(
    envelope: &Envelope,
    body: &[u8],
    incarnation: u64,
    effect: impl FnOnce(&ffi::CommandResult) -> ffi::CallStatus,
) -> io::Result<Vec<u8>> {
    let expected_operation = match envelope.operation {
        Operation::EnrollListener => 1,
        Operation::MatchAccepted => 2,
        Operation::FinishSetter => 3,
        _ => {
            return Err(io::Error::other(
                "provider ACK requested for a different effect",
            ));
        }
    };
    let Reply::Command(observation) = serde_json::from_slice(body)? else {
        return Err(io::Error::other(
            "provider ACK lacks a retained Command response",
        ));
    };
    let ack = if observation.status.returned != 0 {
        // C still owns any unknown/quarantined cell. Do not ACK even if a
        // partially returned struct happens to contain a nonzero ticket.
        CommandAcknowledgement::NotCollected
    } else {
        let raw: ffi::CommandResult = observation.raw.into();
        if raw.operation != expected_operation
            || raw.command == 0
            || raw.phase != 1
            || raw.task == 0
            || raw.start_boottime == 0
            || raw.identity.provider != incarnation
            || raw.identity.object == 0
            || raw.identity.namespace == 0
            || raw.reserved != 0
        {
            return Err(io::Error::other(
                "provider ACK changed its retained complete identity",
            ));
        }
        CommandAcknowledgement::Observed(effect(&raw).into())
    };
    serde_json::to_vec(&ack).map_err(io::Error::other)
}

/// The caller retains this owner through partial open failure. The session and
/// audit are service/run owned, never local to an RPC future.
pub(super) struct Provider {
    session: Option<ffi::Session>,
    failed_open: Option<ffi::OpenFailure>,
    helper: Option<OwnedFd>,
    audit: Option<ffi::AuditHandle>,
    library: Option<Rc<ffi::Library>>,
    matching: MatchRequests,
    registrations: registration::Registrations,
    terminal_close_started: bool,
}
impl Provider {
    pub(super) fn empty() -> Self {
        Self {
            session: None,
            failed_open: None,
            helper: None,
            audit: None,
            library: None,
            matching: MatchRequests::default(),
            registrations: registration::Registrations::default(),
            terminal_close_started: false,
        }
    }
    /// # Safety
    /// Library and object must be immutable authenticated artifacts under the
    /// reviewed outside-service deployment. Loader dependencies/environment must
    /// be trusted; RTLD_LOCAL alone does not prevent symbol interposition.
    pub(super) unsafe fn open(
        &mut self,
        library: &CStr,
        object: &CStr,
        run: [u8; 16],
        expected: &ProviderArtifact,
    ) -> io::Result<ProviderReady> {
        if self.session.is_some() || self.failed_open.is_some() || self.library.is_some() {
            return Err(io::Error::other(
                "accepted provider cannot reopen a used session",
            ));
        }
        use std::os::unix::ffi::OsStrExt;

        use sha2::Digest;
        let library_bytes = std::fs::read(std::ffi::OsStr::from_bytes(library.to_bytes()))?;
        let object_bytes = std::fs::read(std::ffi::OsStr::from_bytes(object.to_bytes()))?;
        let btf_bytes = std::fs::read("/sys/kernel/btf/vmlinux")?;
        let observed = ProviderArtifact {
            library_sha256: sha2::Sha256::digest(library_bytes).into(),
            object_sha256: sha2::Sha256::digest(object_bytes).into(),
            btf_sha256: sha2::Sha256::digest(btf_bytes).into(),
            ..expected.clone()
        };
        if observed != *expected {
            return Err(io::Error::other("provider artifact changed before loading"));
        }
        let raw = unsafe {
            libc::syscall(
                libc::SYS_pidfd_open,
                libc::syscall(libc::SYS_gettid),
                libc::O_EXCL as u32,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.helper = Some(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        let library =
            unsafe { ffi::Library::load(library) }.map_err(|error| io::Error::other(error.0))?;
        self.library = Some(library.clone());
        let incarnation = u64::from_le_bytes(run[..8].try_into().unwrap());
        if incarnation == 0 {
            return Err(io::Error::other("zero accepted provider incarnation"));
        }
        match ffi::Session::open(library, object, incarnation, expected.inventory_capacity()?) {
            Ok(session) => {
                self.audit = Some(session.audit());
                self.session = Some(session);
            }
            Err(failure) => {
                self.audit = Some(failure.audit.clone());
                self.failed_open = Some(failure);
                return Err(io::Error::other(
                    "accepted provider open failed; partial owner retained",
                ));
            }
        }
        let session = self.session.as_mut().unwrap();
        let registered = session.register_task(self.helper.as_ref().unwrap().as_fd());
        if !registered.succeeded() {
            return Err(io::Error::other(format!(
                "accepted helper registration failed: {registered:?}"
            )));
        }
        let inventory = session.identifiers();
        if !inventory.complete() {
            return Err(io::Error::other(format!(
                "accepted provider inventory incomplete: {inventory:?}"
            )));
        }
        let ready = ProviderReady {
            incarnation: run,
            provider_incarnation: incarnation,
            artifact: observed,
            programs: inventory
                .ids
                .iter()
                .filter(|id| id.kind == 1)
                .map(|id| id.id)
                .collect(),
            maps: inventory
                .ids
                .iter()
                .filter(|id| id.kind == 0)
                .map(|id| id.id)
                .collect(),
            links: inventory
                .ids
                .iter()
                .filter(|id| id.kind == 2)
                .map(|id| id.id)
                .collect(),
        };
        ready.validate(run, expected)?;
        Ok(ready)
    }

    /// Read before closing anything so the process owner can publish a recovery
    /// inventory even if ap_close or the final report fails. Both successful and
    /// partially opened sessions remain owned until the controller has exited.
    pub(super) fn terminal_inventory(&mut self) -> Vec<ffi::Inventory> {
        let mut inventories = Vec::new();
        if let Some(session) = &mut self.session {
            inventories.push(session.identifiers());
        }
        if let Some(session) = self
            .failed_open
            .as_mut()
            .and_then(|failure| failure.partial.as_mut())
        {
            inventories.push(session.identifiers());
        }
        inventories
    }

    pub(super) fn terminal_state(&self) -> serde_json::Value {
        serde_json::json!({
            "opened": self.session.is_some(),
            "open_failure": self.failed_open.as_ref().map(|failure| CallStatus::from(failure.status)),
            "success_without_session": self.failed_open.as_ref().is_some_and(|failure| failure.success_without_session),
            "active_setters": self.registrations.active_count(),
            "unresolved_matches": self.matching.0.values().filter(|entry| entry.observed.is_none()).count(),
            "close_started": self.terminal_close_started,
        })
    }

    /// Only the nonreturning helper process entry calls this, after observing
    /// its retained controller pidfd terminal. It closes BPF resources, never
    /// the transport's socket rights. An interrupted close cannot be retried.
    pub(super) fn close_for_process_exit(&mut self) -> io::Result<Vec<ffi::CloseReceipt>> {
        if self.terminal_close_started {
            return Err(io::Error::other("provider close already submitted"));
        }
        self.terminal_close_started = true;
        let mut receipts = Vec::new();
        if let Some(session) = self.session.take() {
            receipts.push(session.close());
        }
        if let Some(session) = self
            .failed_open
            .as_mut()
            .and_then(|failure| failure.partial.take())
        {
            receipts.push(session.close());
        }
        Ok(receipts)
    }

    pub(super) fn acknowledge_completed_command(
        &mut self,
        envelope: &Envelope,
        retained_response: &[u8],
    ) -> io::Result<Vec<u8>> {
        let session = self
            .session
            .as_mut()
            .filter(|s| s.is_ready())
            .ok_or_else(|| io::Error::other("accepted provider session is not ready"))?;
        acknowledge_response(envelope, retained_response, session.incarnation(), |raw| {
            session.ack_command(raw)
        })
    }

    pub(super) fn validate_command_acknowledgement(bytes: &[u8]) -> io::Result<()> {
        match serde_json::from_slice(bytes)? {
            CommandAcknowledgement::NotCollected => Ok(()),
            CommandAcknowledgement::Observed(status) if status.returned == 0 => Ok(()),
            CommandAcknowledgement::Observed(status) => Err(io::Error::other(format!(
                "provider command ACK failed; exact primary response and ACK status remain owned: {status:?}"
            ))),
        }
    }

    /// One read-only map probe. Pending creation publication is not a reply:
    /// the service retains the original command and continues serving peers.
    pub(super) fn poll_creation(&mut self, sequence: u32) -> io::Result<Option<Vec<u8>>> {
        let session = self
            .session
            .as_mut()
            .filter(|s| s.is_ready())
            .ok_or_else(|| io::Error::other("accepted provider session is not ready"))?;
        creation_response(
            session.read_status().into(),
            session.read_creation(sequence).into(),
        )
    }

    /// The inbox already owns `rights`. This synchronous method only borrows;
    /// failed matching cannot drop, replace or reacquire any accepted descriptor.
    pub(super) fn dispatch(
        &mut self,
        envelope: &Envelope,
        rights: &[OwnedFd],
        prepared_rights: Option<&[OwnedFd]>,
    ) -> io::Result<Vec<u8>> {
        let session = self
            .session
            .as_mut()
            .filter(|session| session.is_ready())
            .ok_or_else(|| io::Error::other("accepted provider session is not ready"))?;
        if matches!(
            envelope.operation,
            Operation::PrepareSetter | Operation::FinishSetter
        ) {
            return self.registrations.dispatch_physical(
                session,
                envelope,
                rights,
                prepared_rights,
            );
        }
        let request: Request = serde_json::from_slice(&envelope.body)?;
        let helper = self.helper.as_ref().unwrap().as_fd();
        let reply = match (envelope.operation, request) {
            (Operation::EnrollListener, Request::Enroll { generation }) if rights.len() == 2 => {
                // Getter executes in this service, not in the named guest task.
                Reply::Command(
                    session
                        .enroll_listener(helper, rights[0].as_fd(), generation)
                        .into(),
                )
            }
            (Operation::MatchAccepted, Request::ResolveAccepted)
                if rights.len() == 2 && envelope.owner.is_some() && envelope.accept.is_some() =>
            {
                return self.matching.observe(
                    envelope.owner.unwrap(),
                    envelope.accept.unwrap(),
                    envelope.sequence,
                    || {
                        serde_json::to_vec(&Reply::Command(
                            session.resolve_accepted(helper, rights[0].as_fd()).into(),
                        ))
                        .map_err(io::Error::other)
                    },
                );
            }
            (Operation::DrainCreations, Request::ReadStatus) if rights.is_empty() => {
                Reply::Status(session.read_status().into())
            }
            (Operation::DrainCreations, Request::ReadCreation { sequence })
                if rights.is_empty() =>
            {
                Reply::Creation(session.read_creation(sequence).into())
            }
            _ => {
                return Err(io::Error::other(
                    "accepted provider request/rights/phase mismatch",
                ));
            }
        };
        serde_json::to_vec(&reply).map_err(io::Error::other)
    }
}

fn creation_response(
    status: Observation<Status>,
    creation: Observation<Creation>,
) -> io::Result<Option<Vec<u8>>> {
    if status.status.returned != 0 || status.raw.fatal != 0 {
        return Err(io::Error::other("provider creation status is unresolved"));
    }
    if creation.status.returned == -1 && creation.status.errno == Some(libc::ENODATA) {
        return Ok(None);
    }
    if creation.status.returned == 0 && creation.raw.phase & (2 | 4) == 0 {
        return Ok(None); // CREATED may precede queue fexit; terminal is never hidden
    }
    serde_json::to_vec(&Reply::Creation(creation))
        .map(Some)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ack_fixture() -> (Envelope, ffi::CommandResult) {
        let tid = crate::types::DetTid::from_raw(41);
        let envelope = Envelope {
            run: [1; 16],
            sequence: 7,
            owner: Some(NetworkStreamOwner {
                thread: tid,
                mm: crate::types::MmId::initial(tid),
            }),
            accept: None,
            operation: Operation::FinishSetter,
            body: vec![],
        };
        let raw = ffi::CommandResult {
            command: 45,
            operation: 3,
            task: 1001,
            start_boottime: 2002,
            identity: ffi::Identity {
                provider: 17,
                object: 2,
                namespace: 3003,
            },
            creation: 4,
            cookie: 5005,
            state: ffi::RawState {
                receive_timeout_ticks: 0,
                send_timeout_ticks: 11,
                lowat: 9,
                receive_buffer: 123456,
                peek_offset: -1,
                socket_option_memory: 0,
                window_clamp: 789,
                userlocks: 7,
                scaling_ratio: 4,
                tcp_state: 10,
                child_spin_locked: 0,
            },
            returned: -libc::EINTR,
            reserved: 0,
            phase: 1,
        };
        (envelope, raw)
    }
    fn ack_body(raw: ffi::CommandResult, returned: i32) -> Vec<u8> {
        serde_json::to_vec(&Reply::Command(
            ffi::Observation {
                status: ffi::CallStatus {
                    operation: "ap_finish_setter",
                    returned,
                    errno: (returned != 0).then_some(libc::EIO),
                },
                raw,
            }
            .into(),
        ))
        .unwrap()
    }
    #[test]
    fn accepted_command_ack_wire_roundtrip_preserves_all128_bytes_fields() {
        let (_, raw) = ack_fixture();
        let wire: CommandResult = raw.into();
        let roundtrip: ffi::CommandResult = wire.into();
        assert_eq!(roundtrip, raw);
    }
    #[test]
    fn accepted_command_ack_uses_exact_retained_result_including_failed_guest_primary() {
        let (envelope, raw) = ack_fixture();
        let mut calls = 0;
        let status = acknowledge_response(&envelope, &ack_body(raw, 0), 17, |receipt| {
            calls += 1;
            assert_eq!(*receipt, raw);
            ffi::CallStatus {
                operation: "ap_ack_command",
                returned: 0,
                errno: None,
            }
        })
        .unwrap();
        assert_eq!(calls, 1);
        Provider::validate_command_acknowledgement(&status).unwrap();
    }
    #[test]
    fn accepted_command_ack_partial_error_never_acknowledges_a_plausible_ticket() {
        let (envelope, raw) = ack_fixture();
        let status = acknowledge_response(&envelope, &ack_body(raw, -1), 17, |_| {
            panic!("partial result ACK")
        })
        .unwrap();
        assert!(matches!(
            serde_json::from_slice::<CommandAcknowledgement>(&status).unwrap(),
            CommandAcknowledgement::NotCollected
        ));
    }
    #[test]
    fn accepted_command_ack_rejects_changed_complete_identity_before_c_effect() {
        let (envelope, original) = ack_fixture();
        for case in 0..9 {
            let mut raw = original;
            match case {
                0 => raw.command = 0,
                1 => raw.phase = 0,
                2 => raw.task = 0,
                3 => raw.start_boottime = 0,
                4 => raw.identity.provider += 1,
                5 => raw.identity.object = 0,
                6 => raw.identity.namespace = 0,
                7 => raw.reserved = 1,
                _ => raw.operation = 2,
            }
            assert!(
                acknowledge_response(&envelope, &ack_body(raw, 0), 17, |_| panic!(
                    "changed identity ACK"
                ))
                .is_err()
            );
        }
    }
    #[test]
    fn accepted_command_ack_estale_remains_a_failure_not_a_successful_prior_ack() {
        let (envelope, raw) = ack_fixture();
        let status = acknowledge_response(&envelope, &ack_body(raw, 0), 17, |_| ffi::CallStatus {
            operation: "ap_ack_command",
            returned: -1,
            errno: Some(libc::ESTALE),
        })
        .unwrap();
        let CommandAcknowledgement::Observed(retained) = serde_json::from_slice(&status).unwrap()
        else {
            panic!("lost actual ACK status")
        };
        assert_eq!(retained.returned, -1);
        assert_eq!(retained.errno, Some(libc::ESTALE));
        assert!(Provider::validate_command_acknowledgement(&status).is_err());
    }

    #[test]
    fn accepted_mutating_match_lost_ack_or_partial_error_never_creates_a_fresh_command() {
        let thread = crate::types::DetTid::from_raw(41);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let mut requests = MatchRequests::default();
        let lease = NetworkAcceptLeaseId(12);
        let mut effects = 0;
        let response = requests
            .observe(owner, lease, 7, || {
                effects += 1;
                Ok(vec![9, 8])
            })
            .unwrap();
        assert_eq!(effects, 1);
        assert_eq!(response, vec![9, 8]);
        assert_eq!(
            requests
                .observe(owner, lease, 7, || panic!("lost ACK reran provider"))
                .unwrap(),
            response
        );
        assert!(
            requests
                .observe(owner, lease, 8, || panic!("fresh sequence reran provider"))
                .is_err()
        );
        let unknown = NetworkAcceptLeaseId(13);
        assert!(
            requests
                .observe(owner, unknown, 9, || {
                    effects += 1;
                    Err(io::Error::other("MATCHED advanced before failed result"))
                })
                .is_err()
        );
        assert_eq!(effects, 2);
        assert!(
            requests
                .observe(owner, unknown, 9, || panic!(
                    "uncertain same request reran provider"
                ))
                .is_err()
        );
        assert!(
            requests
                .observe(owner, unknown, 10, || panic!(
                    "uncertain new request reran provider"
                ))
                .is_err()
        );
        assert_eq!(effects, 2);
    }
    #[test]
    fn accepted_provider_wire_keeps_partial_error_observation_and_network_endpoints() {
        let observation: Observation<CommandResult> = ffi::Observation {
            status: ffi::CallStatus {
                operation: "match",
                returned: -1,
                errno: Some(libc::EPROTO),
            },
            raw: ffi::CommandResult {
                creation: 17,
                cookie: 91,
                ..Default::default()
            },
        }
        .into();
        let reply = Reply::Command(observation);
        let bytes = serde_json::to_vec(&reply).unwrap();
        let Reply::Command(decoded) = serde_json::from_slice(&bytes).unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(decoded.status.returned, -1);
        assert_eq!(decoded.status.errno, Some(libc::EPROTO));
        assert_eq!(decoded.raw.creation, 17);
        assert_eq!(decoded.raw.cookie, 91);
        let address = [127, 0, 0, 1];
        let endpoint: Endpoint4 = ffi::Endpoint4 {
            address_be: u32::from_ne_bytes(address),
            port_be: 4321u16.to_be(),
            family: libc::AF_INET as u16,
        }
        .into();
        assert_eq!(endpoint.address, address);
        assert_eq!(endpoint.port, 4321);
        assert_eq!(endpoint.family, libc::AF_INET as u16);
    }
    fn observed<T>(raw: T) -> Observation<T> {
        Observation {
            status: CallStatus {
                operation: "test".into(),
                returned: 0,
                errno: None,
            },
            raw,
        }
    }
    #[test]
    fn accepted_observer_provider_waits_for_queue_but_returns_terminal_and_errors() {
        let status = observed(ffi::Status::default().into());
        let mut raw = ffi::Creation::default();
        raw.phase = 1;
        assert!(
            creation_response(status.clone(), observed(raw.into()))
                .unwrap()
                .is_none()
        );
        let missing = Observation {
            status: CallStatus {
                operation: "read_creation".into(),
                returned: -1,
                errno: Some(libc::ENODATA),
            },
            raw: raw.into(),
        };
        assert!(
            creation_response(status.clone(), missing)
                .unwrap()
                .is_none()
        );
        for phase in [3, 5, 7] {
            raw.phase = phase;
            let bytes = creation_response(status.clone(), observed(raw.into()))
                .unwrap()
                .unwrap();
            let Reply::Creation(result) = serde_json::from_slice(&bytes).unwrap() else {
                panic!("wrong reply")
            };
            assert_eq!(result.raw.phase, phase);
        }
        let mut failed = status;
        failed.raw.fatal = 64;
        assert!(creation_response(failed, observed(raw.into())).is_err());
    }
}
