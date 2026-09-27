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

pub(crate) use registration::PidfdIdentity;
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
        wire!(pub(super) $name, $raw, {$($field: $ty),*});
    };
    ($visibility:vis $name:ident, $raw:path, {$($field:ident : $ty:ty),* $(,)?}) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        $visibility struct $name {$(pub $field: $ty),*}
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
    creation:u64,cookie:u64,state:RawState,returned:i32,reserved:u32,phase:u64,original_count:u64,
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
            original_count: value.original_count,
        }
    }
}
wire!(FdAccept,ffi::FdAccept,{command:u64,accept_lease:u64,owner_mm:u64,task:u64,task_start:u64,table:u64,file:u64,install_begin:u64,install_end:u64,listener:Identity,child:Identity,creation:u64,cookie:u64,phases:u64,problem:u64,requested_fd:i32,flags:i32,returned_fd:i32,do_accept_errno:i32,});
wire!(AcceptedEffect,ffi::AcceptedEffect,{command:CommandResult,installation:FdAccept,});
wire!(pub(crate) OriginalSelection,ffi::OriginalSelection,{command:u64,call:u64,owner_mm:u64,provider:u64,task:u64,task_start:u64,table:u64,file:u64,user_address:u64,fdput_flags:u64,ready:u64,requested_fd:i32,address_length:i32,original_count:u64,});
// Vec on the private wire only: retain and validate exactly the 128 fixed
// kernel-copy bytes, including zero padding. No guest memory is reread.
wire!(pub(crate) OriginalResult,ffi::OriginalResult,{selection:OriginalSelection,address:Vec<u8>,copy_entered:u64,copy_returned:u64,copy_remaining:u64,audit_entered:u64,audit_returned:u64,security_entered:u64,security_returned:u64,complete:u64,problem:u64,audit_result:i32,security_result:i32,returned:i32,reserved:u32,});
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct OriginalEffect {
    pub command: CommandResult,
    pub original: OriginalResult,
    pub socket: Option<super::installation_observation::Capture>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_copy: Option<super::original_read_copy::Manifest>,
}
impl From<ffi::OriginalEffect> for OriginalEffect {
    fn from(raw: ffi::OriginalEffect) -> Self {
        Self {
            command: raw.command.into(),
            original: raw.original.into(),
            socket: None,
            read_copy: None,
        }
    }
}
wire!(OriginalTerminal,ffi::OriginalTerminal,{command:CommandResult,original:OriginalResult,call:u64,fd_call_present:u64,task_absent:u64,});
wire!(FdEnrollment,ffi::FdEnrollment,{command:u64,registration:u64,owner_mm:u64,task:u64,task_start:u64,table:u64,begin:u64,end:u64,expected_table:u64,phases:u64,problem:u64,slots:u32,files:u32,references:u32,mode:u32,ptrace_return:i32,reserved:u32,});
wire!(TableEnrollmentEffect,ffi::TableEnrollmentEffect,{command:CommandResult,enrollment:FdEnrollment,});
wire!(FdEvent,ffi::FdEvent,{sequence:u64,kind:u64,task:u64,task_start:u64,table:u64,file:u64,previous_file:u64,dependency:u64,accept_command:u64,fd:i32,returned:i32,complete:u64,mode:u32,status_flags:u32,device_major:u32,device_minor:u32,});
wire!(FdStatus,ffi::FdStatus,{problem:u64,next_table:u64,next_file:u64,next_event:u64,});
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
wire!(NativeBirth,ffi::NativeBirth,{command:u64,call:u64,owner_mm:u64,provider:u64,creator_task:u64,creator_start:u64,creator_table:u64,child_task:u64,child_start:u64,child_table:u64,parent_task:u64,parent_start:u64,copy_begin:u64,copy_end:u64,pidfd_install_begin:u64,pidfd_install_end:u64,pidfd_file:u64,kernel_flags:u64,ready:u64,problem:u64,shared_mm:u32,shared_files:u32,same_thread_group:u32,exit_signal:i32,requested_exit_signal:i32,pidfd_fd:i32,clear_child_tid:u64});
wire!(NativeBirthEffect,ffi::NativeBirthEffect,{command:CommandResult,birth:NativeBirth});
wire!(NativeBirthTerminal,ffi::NativeBirthTerminal,{command:CommandResult,birth:NativeBirth,call:u64,fd_call_present:u64,task_absent:u64});
/// Physical operations on the existing Call's retained NativeWorker. This is
/// private provider wiring, not a serialized network trace or guest syscall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum AuxiliaryRole {
    File,
    Receive {
        kind: ReceiveKind,
        address: u64,
        count: u64,
        provider: u64,
        file: u64,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ReceiveKind {
    Drain,
    Peek,
}
impl AuxiliaryRole {
    pub(super) fn operation(self) -> u64 {
        match self {
            Self::File => 23,
            Self::Receive {
                kind: ReceiveKind::Drain,
                ..
            } => 21,
            Self::Receive {
                kind: ReceiveKind::Peek,
                ..
            } => 22,
        }
    }
    pub(super) fn prepare_name(self) -> &'static str {
        match self {
            Self::File => "ap_prepare_auxiliary_file",
            Self::Receive {
                kind: ReceiveKind::Drain,
                ..
            } => "ap_prepare_original_recvfrom",
            Self::Receive {
                kind: ReceiveKind::Peek,
                ..
            } => "ap_prepare_original_recvmsg",
        }
    }
    pub(super) fn operands(self) -> (u64, i32, u64) {
        match self {
            Self::File => (libc::SYS_fcntl as u64, libc::F_GETFL, 0),
            Self::Receive {
                kind,
                address,
                count,
                ..
            } => (
                address,
                libc::MSG_DONTWAIT
                    | if kind == ReceiveKind::Peek {
                        libc::MSG_PEEK
                    } else {
                        0
                    },
                count,
            ),
        }
    }
    pub(super) fn valid(self) -> bool {
        match self {
            Self::File => true,
            Self::Receive {
                kind,
                address,
                count,
                provider,
                file,
            } => {
                address != 0
                    && provider != 0
                    && file != 0
                    && match kind {
                        ReceiveKind::Drain => (1..=512).contains(&count),
                        ReceiveKind::Peek => (1024..=0x7fff_f000).contains(&count),
                    }
            }
        }
    }
    pub(super) fn is_receive(self) -> bool {
        matches!(self, Self::Receive { .. })
    }
    // Initial MM generation is zero. Its authority is exact owner equality,
    // never a nonzero numeric test.
    pub(super) fn check_selection(
        self,
        selected: &OriginalSelection,
        call: u64,
        mm: u64,
        fd: i32,
        command: u64,
    ) -> io::Result<()> {
        let (address, length, count) = self.operands();
        if !self.valid()
            || selected.command != command
            || command == 0
            || selected.call != call
            || call == 0
            || selected.owner_mm != mm
            || selected.requested_fd != fd
            || fd < 0
            || selected.user_address != address
            || selected.address_length != length
            || selected.original_count != count
            || selected.provider == 0
            || selected.task == 0
            || selected.task_start == 0
            || selected.table == 0
            || selected.ready != 1
            || selected.fdput_flags & !1 != 0
        {
            return Err(io::Error::other(
                "auxiliary selection changed authenticated operands or identity",
            ));
        }
        if let Self::Receive { provider, file, .. } = self {
            // The helper's protocol entry identifies its held socket directly.
            // There is no guest fdget receipt or permission to substitute an alias.
            if selected.provider != provider || selected.file != file || selected.fdput_flags != 0 {
                return Err(io::Error::other(
                    "helper receive selected another held file or fabricated fdget",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Request {
    PrepareNativeBirth {
        call: u64,
        mm: u64,
        table: u64,
        syscall: i32,
    },
    ObserveNativeBirth {
        call: u64,
        command: u64,
        prepared_request: u64,
        child: i32,
        terminal: bool,
    },
    CollectNativeBirth {
        call: u64,
        command: u64,
        prepared_request: u64,
    },
    CancelNativeBirth {
        call: u64,
        command: u64,
        prepared_request: u64,
    },
    TerminateNativeBirth {
        call: u64,
        command: u64,
        prepared_request: u64,
    },
    RetireNativeBirth {
        call: u64,
        prepared: u64,
        observed: Option<u64>,
        completed: u64,
    },
    /// Existing allocator Call's auxiliary selection on its owned NativeWorker.
    /// Only a PIDFD crosses this channel; the regular file stays in the worker.
    ObserveTerminalSocket {
        call: u64,
        effect: OriginalEffect,
    },
    RetireTerminalSocketObservation {
        call: u64,
        observed: u64,
    },
    PrepareOriginalFileObservation {
        call: u64,
        mm: u64,
        fd: i32,
        role: AuxiliaryRole,
    },
    CollectOriginalFileObservation {
        call: u64,
        command: u64,
        prepared_request: u64,
        role: AuxiliaryRole,
    },
    RetireOriginalFileObservation {
        call: u64,
        prepared: u64,
        completed: u64,
    },
    PrepareOriginalConnect {
        kind: crate::network_replay::original_connect::Kind,
        call: u64,
        mm: u64,
        fd: i32,
        address: u64,
        length: i32,
        original_count: u64,
    },
    AwaitOriginalSelection {
        call: u64,
        command: u64,
        prepared_request: u64,
    },
    ReadOriginalCopy {
        call: u64,
        command: u64,
        prepared: u64,
        first: u64,
    },
    CollectOriginalConnect {
        kind: crate::network_replay::original_connect::Kind,
        call: u64,
        command: u64,
        prepared_request: u64,
    },
    CancelOriginalConnect {
        call: u64,
        command: u64,
        prepared_request: u64,
        selected_request: u64,
    },
    TerminateOriginalConnect {
        call: u64,
        command: u64,
        prepared_request: u64,
        selected_request: u64,
        failed_request: Option<u64>,
    },
    RetireOriginalConnect {
        failed_request: Option<u64>,
        call: u64,
        prepared: u64,
        selected: u64,
        completed: u64,
    },
    PrepareTableEnrollment {
        registration: u64,
        mm: u64,
        expected_table: u64,
    },
    CollectTableEnrollment {
        command: u64,
        prepared_request: u64,
    },
    PrepareAccept {
        identity: Identity,
        lease: u64,
        mm: u64,
        fd: i32,
        flags: i32,
    },
    CollectAccept {
        command: u64,
        prepared_request: u64,
    },
    Enroll {
        generation: u64,
    },
    AwaitFdEvent {
        sequence: u64,
        acknowledged: Option<ObservationReceipt>,
    },
    RetireFdObservation {
        receipt: ObservationReceipt,
    },
    ReadFdPublicationCut {
        permit: crate::network_replay::NetworkFdPublicationPermit,
    },
    /// A terminal Call still owns its physical effect when no live table permit
    /// exists. This only observes the same journal; it grants no table authority.
    ReadOriginalAllocatorCut {
        call: u64,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum Reply {
    NativeBirth(Observation<NativeBirth>),
    NativeBirthEffect(Observation<NativeBirthEffect>),
    NativeBirthTerminated(Observation<NativeBirthTerminal>),
    NativeBirthCanceled {
        command: u64,
        status: CallStatus,
    },
    OriginalFileObservation {
        selection: Observation<OriginalSelection>,
        effect: Option<Observation<OriginalEffect>>,
    },
    TerminalSocketObservation {
        call: u64,
        capture: super::installation_observation::Capture,
    },
    OriginalFileObservationRetired(CallStatus),
    OriginalSelection(Observation<OriginalSelection>),
    /// Both actual epoll_ctl lookups and copied event bytes from one Original
    /// invocation. A single-file selection cannot stand in for this receipt.
    OriginalControlSelection(Observation<OriginalResult>),
    OriginalReadCopy(super::original_read_copy::Chunk),
    OriginalEffect(Observation<OriginalEffect>),
    /// Positive physical retirement after actual task death, never a native result.
    OriginalTerminated(Observation<OriginalTerminal>),
    /// Exact known-uninvoked disarm, distinct from every native completion.
    OriginalCanceled {
        command: u64,
        status: CallStatus,
    },
    TableEnrollmentEffect(Observation<TableEnrollmentEffect>),
    AcceptedEffect(Observation<AcceptedEffect>),
    Command(Observation<CommandResult>),
    Prepared(Observation<u64>),
    Status(Observation<Status>),
    Creation(Observation<Creation>),
    OriginalAllocatorCut {
        call: u64,
        provider: Observation<Status>,
        status: Observation<FdStatus>,
    },
    FdPublicationCut {
        permit: crate::network_replay::NetworkFdPublicationPermit,
        provider: Observation<Status>,
        status: Observation<FdStatus>,
    },
    FdJournal {
        provider: Observation<Status>,
        status: Observation<FdStatus>,
        event: Observation<FdEvent>,
    },
    Retired,
}

#[cfg(test)]
mod profile_wire_tests {
    use super::*;
    #[test]
    fn immutable_profile_roundtrips_into_exact_ack() {
        let raw = ffi::FdEvent {
            sequence: 7,
            kind: 21,
            task: 9,
            task_start: 11,
            table: 13,
            file: 17,
            dependency: 3,
            accept_command: 5,
            fd: 1,
            complete: 1,
            mode: 0o020600,
            status_flags: libc::O_NONBLOCK as u32,
            device_major: 1,
            device_minor: 9,
            ..Default::default()
        };
        let wire: FdEvent = raw.into();
        let bytes = serde_json::to_vec(&wire).unwrap();
        let decoded: FdEvent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(ffi::FdEvent::from(decoded), raw);
    }
}

impl From<FdEvent> for ffi::FdEvent {
    fn from(e: FdEvent) -> Self {
        Self {
            sequence: e.sequence,
            kind: e.kind,
            task: e.task,
            task_start: e.task_start,
            table: e.table,
            file: e.file,
            previous_file: e.previous_file,
            dependency: e.dependency,
            accept_command: e.accept_command,
            fd: e.fd,
            returned: e.returned,
            complete: e.complete,
            mode: e.mode,
            status_flags: e.status_flags,
            device_major: e.device_major,
            device_minor: e.device_minor,
        }
    }
}

/// Internal inbox maintenance outcome; never replaces the guest/provider
/// primary response and never enters the public request/reply wire format.
#[derive(Debug, Serialize, Deserialize)]
enum CommandAcknowledgement {
    NotCollected,
    // No original command is ACKed again. None is an actual capture that did
    // not submit op14; the full refusal/race response remains in the Call.
    ObservedTerminalSocket(Option<CallStatus>),
    Observed(CallStatus),
    // Both commands remain in this one original Call response. An absent
    // auxiliary command is a retained capture refusal, not a successful match.
    ObservedSocket {
        original: CallStatus,
        auxiliary: Option<CallStatus>,
    },
}

/// Every condition that refuses a retained command identity, each with its
/// actual value, so a refusal names the field rather than a generic mismatch.
fn retained_identity_refusals(
    raw: &ffi::CommandResult,
    expected_operation: u64,
    incarnation: u64,
) -> Vec<String> {
    let mut refusals = Vec::new();
    if raw.operation != expected_operation {
        refusals.push(format!(
            "operation={} expected={expected_operation}",
            raw.operation
        ));
    }
    if raw.command == 0 {
        refusals.push("command=0".into());
    }
    if raw.phase != 1 {
        refusals.push(format!("phase={} expected=1", raw.phase));
    }
    if raw.task == 0 {
        refusals.push("task=0".into());
    }
    if raw.start_boottime == 0 {
        refusals.push("start_boottime=0".into());
    }
    if raw.identity.provider != incarnation {
        refusals.push(format!(
            "identity.provider={} expected={incarnation}",
            raw.identity.provider
        ));
    }
    if expected_operation == 6 {
        // A table enrollment names a descriptor table, not a socket: C's
        // `ap_fd_enrollment_matches` collects it only with all four zero.
        if raw.identity.object != 0 {
            refusals.push(format!(
                "identity.object={} expected=0",
                raw.identity.object
            ));
        }
        if raw.identity.namespace != 0 {
            refusals.push(format!(
                "identity.namespace={} expected=0",
                raw.identity.namespace
            ));
        }
        if raw.creation != 0 {
            refusals.push(format!("creation={} expected=0", raw.creation));
        }
        if raw.cookie != 0 {
            refusals.push(format!("cookie={} expected=0", raw.cookie));
        }
    } else if !matches!(
        expected_operation,
        4 | 7 | 8 | 9 | 10 | 11 | 12 | 18 | 19 | 20 | 21 | 22 | 23
    ) {
        if raw.identity.object == 0 {
            refusals.push("identity.object=0".into());
        }
        if raw.identity.namespace == 0 {
            refusals.push("identity.namespace=0".into());
        }
    }
    if raw.reserved != 0 {
        refusals.push(format!("reserved={}", raw.reserved));
    }
    refusals
}

fn acknowledge_response(
    envelope: &Envelope,
    body: &[u8],
    incarnation: u64,
    mut effect: impl FnMut(&ffi::CommandResult) -> ffi::CallStatus,
) -> io::Result<Vec<u8>> {
    if envelope.operation == Operation::ObserveTerminalSocket {
        let Request::ObserveTerminalSocket {
            call,
            effect: original,
        } = serde_json::from_slice(&envelope.body)?
        else {
            return Err(io::Error::other("terminal Socket ACK changed request"));
        };
        let owner = envelope
            .owner
            .filter(|_| envelope.accept.is_none())
            .ok_or_else(|| io::Error::other("terminal Socket ACK lost owner"))?;
        super::terminal_socket_observation::validate_request(owner, call, &original)?;
        let Reply::TerminalSocketObservation {
            call: actual,
            capture,
        } = serde_json::from_slice(body)?
        else {
            return Err(io::Error::other(
                "terminal Socket ACK lacks retained capture",
            ));
        };
        if actual != call || original.command.identity.provider != incarnation {
            return Err(io::Error::other(
                "terminal Socket ACK changed Call/provider",
            ));
        }
        let auxiliary = capture.command(&original)?;
        let ack = CommandAcknowledgement::ObservedTerminalSocket(
            auxiliary.map(|raw| effect(&raw).into()),
        );
        return serde_json::to_vec(&ack).map_err(io::Error::other);
    }
    let expected_operation = match envelope.operation {
        Operation::EnrollListener => 1,
        Operation::MatchAccepted => 2,
        Operation::FinishSetter => 3,
        Operation::CollectAccept => 4,
        Operation::CollectOriginalFileObservation => {
            let Request::CollectOriginalFileObservation { role, .. } =
                serde_json::from_slice(&envelope.body)?
            else {
                return Err(io::Error::other("auxiliary ACK lost its requested role"));
            };
            if !role.valid() {
                return Err(io::Error::other("auxiliary ACK has invalid role operands"));
            }
            role.operation()
        }
        Operation::CollectOriginalConnect => {
            let Request::CollectOriginalConnect { kind, .. } =
                serde_json::from_slice(&envelope.body)?
            else {
                return Err(io::Error::other(
                    "original ACK lost its exact requested kind",
                ));
            };
            kind.provider_operation()
        }
        Operation::CollectNativeBirth => 8,
        Operation::CollectTableEnrollment => 6,
        _ => {
            return Err(io::Error::other(
                "provider ACK requested for a different effect",
            ));
        }
    };
    let reply: Reply = serde_json::from_slice(body)?;
    if envelope.operation == Operation::CollectOriginalFileObservation {
        let Request::CollectOriginalFileObservation {
            call,
            command,
            role,
            ..
        } = serde_json::from_slice(&envelope.body)?
        else {
            return Err(io::Error::other("auxiliary ACK changed request kind"));
        };
        if let Reply::OriginalFileObservation {
            selection,
            effect: Some(observed),
        } = &reply
            && selection.status.returned == 0
            && selection.status.errno.is_none()
            && observed.status.returned == 0
            && observed.status.errno.is_none()
        {
            let owner = envelope
                .owner
                .filter(|_| envelope.accept.is_none())
                .ok_or_else(|| io::Error::other("auxiliary ACK has no exact Call owner"))?;
            role.check_selection(
                &selection.raw,
                call,
                owner.mm.generation(),
                selection.raw.requested_fd,
                command,
            )?;
            if observed.raw.command.original_count != role.operands().2 {
                return Err(io::Error::other(
                    "auxiliary ACK changed original helper count",
                ));
            }
        }
    }
    if let Reply::OriginalFileObservation { effect: None, .. } = &reply {
        if envelope.operation != Operation::CollectOriginalFileObservation {
            return Err(io::Error::other("auxiliary failure changed operation"));
        }
        return serde_json::to_vec(&CommandAcknowledgement::NotCollected).map_err(io::Error::other);
    }
    let auxiliary = match &reply {
        Reply::OriginalEffect(observed) if observed.raw.socket.is_some() => {
            if observed.status.returned != 0 || expected_operation != 12 {
                return Err(io::Error::other(
                    "auxiliary observation changed original collection",
                ));
            }
            Some(
                observed
                    .raw
                    .socket
                    .as_ref()
                    .unwrap()
                    .command(&observed.raw)?,
            )
        }
        _ => None,
    };
    let observation = match reply {
        Reply::Command(observation) if matches!(expected_operation, 1..=3) => observation,
        Reply::TableEnrollmentEffect(observation) if expected_operation == 6 => Observation {
            status: observation.status,
            raw: observation.raw.command,
        },
        Reply::NativeBirthEffect(observation) if expected_operation == 8 => Observation {
            status: observation.status,
            raw: observation.raw.command,
        },
        Reply::OriginalFileObservation {
            selection,
            effect: Some(observation),
        } if envelope.operation == Operation::CollectOriginalFileObservation
            && matches!(expected_operation, 21 | 22 | 23)
            && selection.status.returned == 0
            && selection.status.errno.is_none()
            && selection.raw == observation.raw.original.selection =>
        {
            Observation {
                status: observation.status,
                raw: observation.raw.command,
            }
        }
        Reply::OriginalEffect(observation)
            if matches!(expected_operation, 7 | 9 | 10 | 11 | 12 | 18 | 19 | 20) =>
        {
            Observation {
                status: observation.status,
                raw: observation.raw.command,
            }
        }
        Reply::AcceptedEffect(observation) if expected_operation == 4 => Observation {
            status: observation.status,
            raw: observation.raw.command,
        },
        _ => {
            return Err(io::Error::other(
                "provider ACK lacks its retained effect response",
            ));
        }
    };
    let ack = if observation.status.returned != 0 || observation.status.errno.is_some() {
        // C still owns any unknown/quarantined cell. Do not ACK even if a
        // partially returned struct happens to contain a nonzero ticket.
        CommandAcknowledgement::NotCollected
    } else {
        let raw: ffi::CommandResult = observation.raw.into();
        let refusals = retained_identity_refusals(&raw, expected_operation, incarnation);
        if !refusals.is_empty() {
            return Err(io::Error::other(format!(
                "provider ACK changed its retained complete identity: {}",
                refusals.join(", ")
            )));
        }
        let original = effect(&raw).into();
        match auxiliary {
            Some(auxiliary) => CommandAcknowledgement::ObservedSocket {
                original,
                auxiliary: auxiliary.map(|raw| effect(&raw).into()),
            },
            None => CommandAcknowledgement::Observed(original),
        }
    };
    serde_json::to_vec(&ack).map_err(io::Error::other)
}

// Borrow the same retained native Session for the existing operation bodies.
// The actual grouped owner enforces runtime handoff; metadata never selects or
// constructs it. Separate field borrows preserve registration/matching custody.
fn retained_session<'a>(
    classic: &'a mut Option<ffi::Session>,
    grouped: &'a mut Option<ffi::GroupedSessionOwner>,
) -> io::Result<Option<&'a mut ffi::Session>> {
    if let Some(owner) = grouped {
        if classic.is_some() {
            return Err(io::Error::other("multiple provider session owners"));
        }
        owner.session_mut().map(Some)
    } else {
        Ok(classic.as_mut())
    }
}

/// The caller retains this owner through partial open failure. The session and
/// audit are service/run owned, never local to an RPC future.
pub(super) struct Provider {
    session: Option<ffi::Session>,
    failed_open: Option<ffi::OpenFailure>,
    grouped: Option<ffi::GroupedSessionOwner>,
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
            grouped: None,
            helper: None,
            audit: None,
            library: None,
            matching: MatchRequests::default(),
            registrations: registration::Registrations::default(),
            terminal_close_started: false,
        }
    }
    /// Move actual bootstrap custody before any fallible artifact read or
    /// library load. A refusal leaves the caller's original slot unchanged.
    pub(super) fn install_grouped(
        &mut self,
        owner: &mut Option<ffi::GroupedBootstrapOwner>,
    ) -> io::Result<()> {
        if self.grouped.is_some()
            || self.session.is_some()
            || self.failed_open.is_some()
            || self.helper.is_some()
            || self.library.is_some()
            || self.terminal_close_started
        {
            return Err(io::Error::other(
                "provider bootstrap owner is already installed or used",
            ));
        }
        if owner.is_none() {
            return Err(io::Error::other("actual grouped bootstrap owner is absent"));
        }
        self.grouped = owner.take();
        Ok(())
    }
    pub(super) fn grouped_startup_mut(&mut self) -> io::Result<&mut ffi::GroupedSessionOwner> {
        if self.library.is_some() || self.terminal_close_started {
            return Err(io::Error::other(
                "grouped startup already entered provider open or retirement",
            ));
        }
        self.grouped
            .as_mut()
            .ok_or_else(|| io::Error::other("actual grouped bootstrap owner is absent"))
    }
    pub(super) fn has_grouped_owner(&self) -> bool {
        self.grouped.is_some()
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
        let result = unsafe { self.open_retained(library, object, run, expected) };
        if let Err(error) = &result {
            if let Some(owner) = &mut self.grouped {
                owner.retain_failure(error);
            }
        }
        result
    }
    unsafe fn open_retained(
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
        let mut observed = ProviderArtifact {
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
            unsafe { ffi::Library::load(library, expected.wire_format, &expected.topology) }
                .map_err(|error| io::Error::other(error.0))?;
        observed.wire_format = library.wire_format();
        observed.topology = library.topology().clone();
        self.library = Some(library.clone());
        let incarnation = u64::from_le_bytes(run[..8].try_into().unwrap());
        if incarnation == 0 {
            return Err(io::Error::other("zero accepted provider incarnation"));
        }
        if matches!(
            &observed.topology,
            super::ProviderTopology::ClassicV40 | super::ProviderTopology::FtraceV1 { .. }
        ) {
            if self.grouped.is_some() {
                return Err(io::Error::other(
                    "actual grouped broker cannot use a link-owned provider",
                ));
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
        } else {
            let owner = self.grouped.as_mut().ok_or_else(|| {
                io::Error::other("grouped provider requires retained broker capability")
            })?;
            self.audit = Some(owner.audit());
            owner.attach_library(library)?;
            owner.open_retained(object, incarnation, expected.inventory_capacity()?)?;
        }
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .ok_or_else(|| io::Error::other("accepted provider session is absent after open"))?;
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
        if let Some(owner) = &mut self.grouped {
            inventories.extend(owner.terminal_inventory());
        }
        inventories
    }

    pub(super) fn terminal_state(&self) -> serde_json::Value {
        let failure = self
            .failed_open
            .as_ref()
            .or_else(|| self.grouped.as_ref().and_then(|owner| owner.failed_open()));
        serde_json::json!({
            "opened": self.session.is_some() || self.grouped.as_ref().is_some_and(|owner| owner.has_session()),
            "open_failure": failure.map(|failure| CallStatus::from(failure.status)),
            "success_without_session": failure.is_some_and(|failure| failure.success_without_session),
            "grouped_owner_retained": self.grouped.is_some(),
            "grouped_open_call": self.grouped.as_ref().and_then(|owner| owner.open_status()).map(CallStatus::from),
            "grouped_first_failure": self.grouped.as_ref().and_then(|owner| owner.first_failure()),
            "active_setters": self.registrations.active_count(),
            "unresolved_matches": self.matching.0.values().filter(|entry| entry.observed.is_none()).count(),
            "close_started": self.terminal_close_started,
        })
    }

    /// Only the nonreturning helper process entry calls this, after observing
    /// its retained controller pidfd terminal. It closes BPF resources, never
    /// the transport's socket rights. An interrupted close cannot be retried.
    pub(super) fn close_for_process_exit(&mut self) -> io::Result<Vec<ffi::CloseReceipt>> {
        if self.grouped.is_some() {
            return Err(io::Error::other(
                "grouped close requires its original release origin and runtime owner",
            ));
        }
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

    /// The nonreturning process owner supplies the original before-release
    /// timestamp. The grouped owner independently polls its original controller
    /// pidfd, retains partial pointers and joins actual runtime cleanup custody.
    pub(super) fn close_grouped_for_process_exit(
        &mut self,
        original_start: u64,
        cutoff: u64,
    ) -> io::Result<ffi::GroupedCloseReceipt> {
        if self.terminal_close_started {
            return Err(io::Error::other("provider close already submitted"));
        }
        let owner = self
            .grouped
            .as_mut()
            .ok_or_else(|| io::Error::other("actual grouped owner is absent"))?;
        self.terminal_close_started = true;
        owner.close_terminal(original_start, cutoff).cloned()
    }
    /// A native result remains available even if later absence/peer retirement
    /// fails. This readback does not upgrade that failed close to success.
    pub(super) fn grouped_close_receipt(&self) -> Option<&ffi::GroupedCloseReceipt> {
        self.grouped
            .as_ref()
            .and_then(|owner| owner.close_receipt())
    }

    pub(super) fn acknowledge_completed_command(
        &mut self,
        envelope: &Envelope,
        retained_response: &[u8],
    ) -> io::Result<Vec<u8>> {
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .filter(|s| s.is_ready())
            .ok_or_else(|| io::Error::other("accepted provider session is not ready"))?;
        acknowledge_response(envelope, retained_response, session.incarnation(), |raw| {
            session.ack_command(raw)
        })
    }

    pub(super) fn validate_command_acknowledgement(bytes: &[u8]) -> io::Result<()> {
        match serde_json::from_slice(bytes)? {
            CommandAcknowledgement::NotCollected => Ok(()),
            CommandAcknowledgement::ObservedTerminalSocket(status)
                if status
                    .as_ref()
                    .is_none_or(|s| s.returned == 0 && s.errno.is_none()) =>
            {
                Ok(())
            }
            CommandAcknowledgement::ObservedTerminalSocket(status) => Err(io::Error::other(
                format!("terminal Socket auxiliary ACK remains unresolved: {status:?}"),
            )),
            CommandAcknowledgement::Observed(status)
                if status.returned == 0 && status.errno.is_none() =>
            {
                Ok(())
            }
            CommandAcknowledgement::ObservedSocket {
                original,
                auxiliary,
            } if original.returned == 0
                && original.errno.is_none()
                && auxiliary
                    .as_ref()
                    .is_none_or(|status| status.returned == 0 && status.errno.is_none()) =>
            {
                Ok(())
            }
            CommandAcknowledgement::ObservedSocket {
                original,
                auxiliary,
            } => Err(io::Error::other(format!(
                "original Socket auxiliary ACK unresolved: {original:?} {auxiliary:?}"
            ))),
            CommandAcknowledgement::Observed(status) => Err(io::Error::other(format!(
                "provider command ACK failed; exact primary response and ACK status remain owned: {status:?}"
            ))),
        }
    }

    pub(super) fn require_terminal_socket_acknowledgement(bytes: &[u8]) -> io::Result<()> {
        match serde_json::from_slice(bytes)? {
            CommandAcknowledgement::ObservedTerminalSocket(status)
                if status
                    .as_ref()
                    .is_none_or(|s| s.returned == 0 && s.errno.is_none()) =>
            {
                Ok(())
            }
            _ => Err(io::Error::other(
                "terminal Socket observation ACK remains owned",
            )),
        }
    }

    pub(super) fn require_uncollected_acknowledgement(bytes: &[u8]) -> io::Result<()> {
        match serde_json::from_slice(bytes)? {
            CommandAcknowledgement::NotCollected => Ok(()),
            _ => Err(io::Error::other(
                "failed original collection changed its retained no-ACK state",
            )),
        }
    }
    pub(super) fn require_original_acknowledgement(bytes: &[u8]) -> io::Result<()> {
        match serde_json::from_slice(bytes)? {
            CommandAcknowledgement::Observed(status)
                if status.returned == 0 && status.errno.is_none() =>
            {
                Ok(())
            }
            CommandAcknowledgement::ObservedSocket {
                original,
                auxiliary,
            } if original.returned == 0
                && original.errno.is_none()
                && auxiliary
                    .as_ref()
                    .is_none_or(|status| status.returned == 0 && status.errno.is_none()) =>
            {
                Ok(())
            }
            _ => Err(io::Error::other(
                "original provider command retirement was not positively acknowledged",
            )),
        }
    }

    /// Uses the original registration and borrowed preparation PIDFD_THREAD.
    /// ENODATA/ENOENT are unresolved observations, never negative certificates.
    /// The outstanding request remains in the existing service Inbox.
    pub(super) fn poll_original_selection(
        &mut self,
        envelope: &Envelope,
        pins: &[OwnedFd],
    ) -> io::Result<Option<Vec<u8>>> {
        let body = self.dispatch(envelope, &[], Some(pins))?;
        let reply: Reply = serde_json::from_slice(&body)?;
        let observation = match &reply {
            Reply::OriginalSelection(observation) => &observation.status,
            Reply::OriginalControlSelection(observation) => &observation.status,
            _ => {
                return Err(io::Error::other(
                    "original selection returned another operation",
                ));
            }
        };
        if observation.returned == 0 && observation.errno.is_none() {
            return Ok(Some(body));
        }
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .ok_or_else(|| io::Error::other("original provider absent"))?;
        let status = session.read_status();
        let fd = session.read_fd_status();
        if !status.status.succeeded()
            || !fd.status.succeeded()
            || status.raw.fatal != 0
            || fd.raw.problem != 0
        {
            return Err(io::Error::other(format!(
                "original selection observer failed: {status:?} {fd:?}"
            )));
        }
        if observation.returned != 0 && original_selection_pending(observation.errno) {
            Ok(None)
        } else {
            Err(io::Error::other(format!(
                "original selection query failed: {observation:?}"
            )))
        }
    }

    /// One read-only map probe. Pending creation publication is not a reply:
    /// the service retains the original command and continues serving peers.
    pub(super) fn poll_creation(&mut self, sequence: u32) -> io::Result<Option<Vec<u8>>> {
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .filter(|s| s.is_ready())
            .ok_or_else(|| io::Error::other("accepted provider session is not ready"))?;
        creation_response(
            session.read_status().into(),
            session.read_creation(sequence).into(),
        )
    }

    /// Only reads immutable completed rows. Allocation high-water is not a
    /// committed prefix: a producer can still be writing complete=2.
    pub(super) fn poll_fd_event(&mut self, sequence: u64) -> io::Result<(bool, Vec<u8>)> {
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .filter(|s| s.is_ready())
            .ok_or_else(|| io::Error::other("provider session not ready"))?;
        poll_fd_event_source(session, sequence)
    }
    pub(super) fn acknowledge_fd_observation(
        &mut self,
        envelope: &Envelope,
        body: &[u8],
    ) -> io::Result<Vec<u8>> {
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .filter(|s| s.is_ready())
            .ok_or_else(|| io::Error::other("provider session not ready"))?;
        acknowledge_fd_response(envelope, body, |raw| session.ack_fd_event(raw))
    }

    /// The inbox already owns `rights`. This synchronous method only borrows;
    /// failed matching cannot drop, replace or reacquire any accepted descriptor.
    pub(super) fn dispatch(
        &mut self,
        envelope: &Envelope,
        rights: &[OwnedFd],
        prepared_rights: Option<&[OwnedFd]>,
    ) -> io::Result<Vec<u8>> {
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .filter(|session| session.is_ready())
            .ok_or_else(|| io::Error::other("accepted provider session is not ready"))?;
        if matches!(
            envelope.operation,
            Operation::PrepareOriginalFileObservation
                | Operation::CollectOriginalFileObservation
                | Operation::RetireOriginalFileObservation
                | Operation::PrepareNativeBirth
                | Operation::ObserveNativeBirth
                | Operation::CollectNativeBirth
                | Operation::CancelNativeBirth
                | Operation::TerminateNativeBirth
                | Operation::PrepareSetter
                | Operation::FinishSetter
                | Operation::PrepareAccept
                | Operation::CollectAccept
                | Operation::PrepareTableEnrollment
                | Operation::CollectTableEnrollment
                | Operation::PrepareOriginalConnect
                | Operation::AwaitOriginalSelection
                | Operation::CollectOriginalConnect
                | Operation::CancelOriginalConnect
                | Operation::TerminateOriginalConnect
        ) {
            let bytes =
                self.registrations
                    .dispatch_physical(session, envelope, rights, prepared_rights)?;
            if matches!(
                serde_json::from_slice::<Request>(&envelope.body),
                Ok(Request::CollectOriginalConnect {
                    kind: crate::network_replay::original_connect::Kind::Socket,
                    ..
                })
            ) {
                let mut reply: Reply = serde_json::from_slice(&bytes)?;
                let Reply::OriginalEffect(ref mut observed) = reply else {
                    return Err(io::Error::other(
                        "Socket collection changed physical response",
                    ));
                };
                if observed.status.returned == 0 && observed.raw.original.returned >= 0 {
                    let target = prepared_rights
                        .filter(|rights| rights.len() == 1)
                        .ok_or_else(|| {
                            io::Error::other("Socket observer lost original retained PIDFD_THREAD")
                        })?;
                    if observed.raw.command.operation != 12
                        || observed.raw.original.selection.file == 0
                        || observed.raw.socket.is_some()
                    {
                        return Err(io::Error::other(
                            "Socket observer changed original installation",
                        ));
                    }
                    observed.raw.socket = Some(super::installation_observation::capture(
                        session,
                        self.helper.as_ref().unwrap().as_fd(),
                        target[0].as_fd(),
                        &observed.raw,
                    ));
                }
                return serde_json::to_vec(&reply).map_err(io::Error::other);
            }
            if matches!(
                serde_json::from_slice::<Request>(&envelope.body),
                Ok(Request::CollectOriginalConnect {
                    kind: crate::network_replay::original_connect::Kind::Read,
                    ..
                })
            ) {
                let mut reply: Reply = serde_json::from_slice(&bytes)?;
                let Reply::OriginalEffect(ref mut observed) = reply else {
                    return Err(io::Error::other(
                        "Read collection changed physical response",
                    ));
                };
                if observed.status.returned == 0 {
                    let manifest =
                        session.original_read_copy_manifest(observed.raw.command.command)?;
                    manifest.validate_for_version(
                        &observed.raw,
                        session.wire_format().copy_version(),
                    )?;
                    observed.raw.read_copy = Some(manifest);
                }
                return serde_json::to_vec(&reply).map_err(io::Error::other);
            }
            if matches!(serde_json::from_slice::<Request>(&envelope.body),
                Ok(Request::CollectOriginalFileObservation { role, .. }) if role.is_receive())
            {
                let mut reply: Reply = serde_json::from_slice(&bytes)?;
                let Reply::OriginalFileObservation {
                    effect: Some(ref mut observed),
                    ..
                } = reply
                else {
                    // Keep a failed selection's raw response, with no manifest or ACK authority.
                    return Ok(bytes);
                };
                if observed.status.returned == 0 && observed.status.errno.is_none() {
                    let manifest =
                        session.original_read_copy_manifest(observed.raw.command.command)?;
                    manifest.validate_for_version(
                        &observed.raw,
                        session.wire_format().copy_version(),
                    )?;
                    observed.raw.read_copy = Some(manifest);
                }
                return serde_json::to_vec(&reply).map_err(io::Error::other);
            }
            return Ok(bytes);
        }
        let request: Request = serde_json::from_slice(&envelope.body)?;
        let helper = self.helper.as_ref().unwrap().as_fd();
        let reply = match (envelope.operation, request) {
            (Operation::ObserveTerminalSocket, Request::ObserveTerminalSocket { call, effect })
                if rights.len() == 1 && envelope.owner.is_some() && envelope.accept.is_none() =>
            {
                super::terminal_socket_observation::validate_request(
                    envelope.owner.unwrap(),
                    call,
                    &effect,
                )?;
                if effect.command.identity.provider != session.incarnation() {
                    return Err(io::Error::other("terminal Socket capture changed provider"));
                }
                Reply::TerminalSocketObservation {
                    call,
                    capture: super::installation_observation::capture(
                        session,
                        helper,
                        rights[0].as_fd(),
                        &effect,
                    ),
                }
            }
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
            (Operation::DrainFdJournal, Request::ReadOriginalAllocatorCut { call })
                if rights.is_empty()
                    && envelope.owner.is_some()
                    && envelope.accept.is_none()
                    && call != 0 =>
            {
                let status = session.read_fd_status().into();
                let provider = session.read_status().into();
                Reply::OriginalAllocatorCut {
                    call,
                    provider,
                    status,
                }
            }
            (Operation::DrainFdJournal, Request::ReadFdPublicationCut { permit })
                if rights.is_empty()
                    && envelope.owner == Some(permit.owner)
                    && envelope.accept.is_none() =>
            {
                // A new immutable controller request, not a cached event's
                // counter snapshot. The runtime retains the matching permit.
                let status = session.read_fd_status().into();
                let provider = session.read_status().into();
                Reply::FdPublicationCut {
                    permit,
                    provider,
                    status,
                }
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

// These errors retain the same unanswered selection query. They are never an
// empty-selection or cleanup receipt. All other errors keep their refusal.
fn original_selection_pending(errno: Option<i32>) -> bool {
    matches!(errno, Some(libc::ENODATA | libc::ENOENT | libc::ESRCH))
}
#[cfg(test)]
mod original_terminal_observation_tests {
    use super::*;
    #[test]
    fn dead_task_selection_query_remains_unknown_without_swallowing_other_failures() {
        for errno in [libc::ENODATA, libc::ENOENT, libc::ESRCH] {
            assert!(original_selection_pending(Some(errno)));
        }
        for errno in [
            None,
            Some(0),
            Some(libc::EIO),
            Some(libc::EPERM),
            Some(libc::EINVAL),
        ] {
            assert!(!original_selection_pending(errno));
        }
    }
}

/// The production session and the ordered-call regression share this exact
/// read boundary. No method submits, acknowledges or retires a producer row.
trait FdEventSource {
    fn event(&mut self, sequence: u64) -> ffi::Observation<ffi::FdEvent>;
    fn fd_status(&mut self) -> ffi::Observation<ffi::FdStatus>;
    fn provider_status(&mut self) -> ffi::Observation<ffi::Status>;
}
impl FdEventSource for ffi::Session {
    fn event(&mut self, sequence: u64) -> ffi::Observation<ffi::FdEvent> {
        self.read_fd_event(sequence)
    }
    fn fd_status(&mut self) -> ffi::Observation<ffi::FdStatus> {
        self.read_fd_status()
    }
    fn provider_status(&mut self) -> ffi::Observation<ffi::Status> {
        self.read_status()
    }
}
fn poll_fd_event_source(
    source: &mut impl FdEventSource,
    sequence: u64,
) -> io::Result<(bool, Vec<u8>)> {
    // Complete=1 is immutable, and its event/file/table counters were allocated
    // before publication. Read that row first, then the monotonic counters and
    // sticky health. An earlier counter snapshot could reject an honest row
    // published between calls. This is no atomic snapshot or committed prefix.
    let event = source.event(sequence).into();
    let status = source.fd_status().into();
    let provider = source.provider_status().into();
    fd_event_response(sequence, provider, status, event)
}

/// ENODATA is the C double-read API's explicit unpublished/unstable result.
/// next_event is allocated before the hash entry exists, so ENOENT cannot prove
/// a gap is terminal. Keep the same request pending; never advance its cursor,
/// invent an empty prefix, ACK it, or claim success if it never completes.
fn fd_event_response(
    sequence: u64,
    provider: Observation<Status>,
    status: Observation<FdStatus>,
    event: Observation<FdEvent>,
) -> io::Result<(bool, Vec<u8>)> {
    let pending = provider.status.returned == 0
        && provider.raw.fatal == 0
        && status.status.returned == 0
        && status.raw.problem == 0
        && event.status.returned == -1
        && (event.status.errno == Some(libc::ENOENT)
            || event.status.errno == Some(libc::ENODATA)
                && event.raw.sequence == sequence
                && matches!(event.raw.complete, 1 | 2));
    let body = serde_json::to_vec(&Reply::FdJournal {
        provider,
        status,
        event,
    })?;
    Ok((!pending, body))
}

fn acknowledge_fd_response(
    envelope: &Envelope,
    body: &[u8],
    effect: impl FnOnce(&ffi::FdEvent) -> ffi::CallStatus,
) -> io::Result<Vec<u8>> {
    let Request::AwaitFdEvent { sequence, .. } = serde_json::from_slice(&envelope.body)? else {
        return Err(io::Error::other("wrong FD ACK request"));
    };
    let Reply::FdJournal {
        provider,
        status,
        event,
    } = serde_json::from_slice(body)?
    else {
        return Err(io::Error::other("FD ACK lacks retained raw response"));
    };
    if envelope.operation != Operation::DrainFdJournal {
        return Err(io::Error::other("wrong FD ACK operation"));
    }
    let ack = if provider.status.returned != 0
        || provider.raw.fatal != 0
        || status.status.returned != 0
        || status.raw.problem != 0
        || event.status.returned != 0
        || event.raw.complete != 1
        || event.raw.sequence != sequence
        || sequence == 0
        || sequence > status.raw.next_event
    {
        CommandAcknowledgement::NotCollected
    } else {
        CommandAcknowledgement::Observed(effect(&event.raw.into()).into())
    };
    serde_json::to_vec(&ack).map_err(io::Error::other)
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
    #[test]
    fn terminal_socket_acks_only_its_auxiliary_command_and_retains_refusals() {
        let (owner, effect, capture) = super::super::terminal_socket_observation::fixture();
        let envelope = Envelope {
            run: [1; 16],
            sequence: 7,
            owner: Some(owner),
            accept: None,
            operation: Operation::ObserveTerminalSocket,
            body: serde_json::to_vec(&Request::ObserveTerminalSocket {
                call: 19,
                effect: effect.clone(),
            })
            .unwrap(),
        };
        let body = |capture| {
            serde_json::to_vec(&Reply::TerminalSocketObservation { call: 19, capture }).unwrap()
        };
        for actual_file in [19, 20, 0] {
            let mut observed = capture.clone();
            observed.observation.as_mut().unwrap().raw.identity.object = actual_file;
            if actual_file != 19 {
                observed.metadata = None;
            }
            let mut calls = vec![];
            let ack = acknowledge_response(&envelope, &body(observed.clone()), 7, |raw| {
                calls.push((raw.operation, raw.command));
                ffi::CallStatus {
                    operation: "ap_ack_command",
                    returned: 0,
                    errno: None,
                }
            })
            .unwrap();
            assert_eq!(calls, vec![(14, 72)]); // original command71 was already retired
            Provider::require_terminal_socket_acknowledgement(&ack).unwrap();
            assert!(Provider::require_original_acknowledgement(&ack).is_err());
            assert_eq!(
                observed.checked(&effect).unwrap().is_some(),
                actual_file == 19
            );
        }
        let mut absent = capture.clone();
        absent.capture.returned = -1;
        absent.capture.errno = Some(libc::EBADF);
        absent.candidate_stat = None;
        absent.observation = None;
        absent.metadata = None;
        absent.release = None;
        let ack = acknowledge_response(&envelope, &body(absent.clone()), 7, |_| {
            panic!("numeric-slot race must not invent an auxiliary ACK")
        })
        .unwrap();
        Provider::require_terminal_socket_acknowledgement(&ack).unwrap();
        assert_eq!(absent.checked(&effect).unwrap(), None);
        let ack = acknowledge_response(&envelope, &body(capture.clone()), 7, |_| ffi::CallStatus {
            operation: "ap_ack_command",
            returned: -1,
            errno: Some(libc::EIO),
        })
        .unwrap();
        assert!(Provider::require_terminal_socket_acknowledgement(&ack).is_err());
        assert!(Provider::validate_command_acknowledgement(&ack).is_err());
        assert!(
            acknowledge_response(&envelope, &body(capture), 8, |_| panic!(
                "changed provider performed ACK"
            ))
            .is_err()
        );
    }

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
            original_count: 0,
        };
        (envelope, raw)
    }
    #[test]
    fn retained_identity_refusal_names_each_failing_field() {
        let exact = ffi::CommandResult {
            command: 9,
            operation: 1,
            task: 3,
            start_boottime: 5,
            phase: 1,
            identity: ffi::Identity {
                provider: 7,
                object: 11,
                namespace: 13,
            },
            ..Default::default()
        };
        assert!(retained_identity_refusals(&exact, 1, 7).is_empty());
        let changed = ffi::CommandResult {
            command: 0,
            operation: 2,
            task: 0,
            start_boottime: 0,
            phase: 2,
            reserved: 4,
            identity: ffi::Identity {
                provider: 8,
                object: 0,
                namespace: 0,
            },
            ..exact
        };
        assert_eq!(
            retained_identity_refusals(&changed, 1, 7),
            [
                "operation=2 expected=1",
                "command=0",
                "phase=2 expected=1",
                "task=0",
                "start_boottime=0",
                "identity.provider=8 expected=7",
                "identity.object=0",
                "identity.namespace=0",
                "reserved=4"
            ]
        );
        // The shape C collects for a table enrollment (fd-enrollment.h) is accepted,
        // and a socket-shaped identity on one is refused by name.
        let table = ffi::CommandResult {
            operation: 6,
            identity: ffi::Identity {
                provider: 7,
                object: 0,
                namespace: 0,
            },
            ..exact
        };
        assert!(retained_identity_refusals(&table, 6, 7).is_empty());
        let socket_shaped = ffi::CommandResult {
            operation: 6,
            creation: 17,
            cookie: 19,
            ..exact
        };
        assert_eq!(
            retained_identity_refusals(&socket_shaped, 6, 7),
            [
                "identity.object=11 expected=0",
                "identity.namespace=13 expected=0",
                "creation=17 expected=0",
                "cookie=19 expected=0"
            ]
        );
        // Operations without a socket object keep their existing exemption.
        let objectless = ffi::CommandResult {
            operation: 8,
            identity: ffi::Identity {
                provider: 7,
                object: 0,
                namespace: 0,
            },
            ..exact
        };
        assert!(retained_identity_refusals(&objectless, 8, 7).is_empty());
    }
    #[test]
    fn table_enrollment_ack_accepts_the_objectless_result_c_collects() {
        let (mut envelope, _) = ack_fixture();
        envelope.operation = Operation::CollectTableEnrollment;
        let command = ffi::CommandResult {
            command: 23,
            operation: 6,
            task: 5001,
            start_boottime: 29,
            phase: 1,
            identity: ffi::Identity {
                provider: 7,
                object: 0,
                namespace: 0,
            },
            ..Default::default()
        };
        let enrollment = ffi::FdEnrollment {
            command: 23,
            task: 5001,
            task_start: 29,
            table: 31,
            begin: 1,
            end: 2,
            ..Default::default()
        };
        let body = |command: ffi::CommandResult| {
            serde_json::to_vec(&Reply::TableEnrollmentEffect(Observation {
                status: CallStatus {
                    operation: "ap_collect_table_enrollment".into(),
                    returned: 0,
                    errno: None,
                },
                raw: TableEnrollmentEffect {
                    command: command.into(),
                    enrollment: enrollment.into(),
                },
            }))
            .unwrap()
        };
        let mut acked = vec![];
        acknowledge_response(&envelope, &body(command), 7, |raw| {
            acked.push(*raw);
            ffi::CallStatus {
                operation: "ap_ack_command",
                returned: 0,
                errno: None,
            }
        })
        .unwrap();
        assert_eq!(acked.len(), 1);
        assert_eq!(acked[0].command, 23);
        let socket_shaped = ffi::CommandResult {
            identity: ffi::Identity {
                provider: 7,
                object: 2,
                namespace: 3,
            },
            ..command
        };
        let error = acknowledge_response(&envelope, &body(socket_shaped), 7, |_| unreachable!())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with("identity.object=2 expected=0, identity.namespace=3 expected=0"),
            "{error}"
        );
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
    fn original_socket_collection_acks_both_exact_commands_without_repeating_the_capture() {
        let (mut envelope, _) = ack_fixture();
        envelope.operation = Operation::CollectOriginalConnect;
        envelope.body = serde_json::to_vec(&Request::CollectOriginalConnect {
            kind: crate::network_replay::original_connect::Kind::Socket,
            call: 19,
            command: 71,
            prepared_request: 1,
        })
        .unwrap();
        for actual_file in [19, 20, 0] {
            let (mut effect, mut capture) = super::super::installation_observation::fixture();
            capture.observation.as_mut().unwrap().raw.identity.object = actual_file;
            if actual_file != 19 {
                capture.metadata = None;
            }
            let auxiliary: ffi::CommandResult =
                capture.observation.as_ref().unwrap().raw.clone().into();
            let primary: ffi::CommandResult = effect.command.clone().into();
            effect.socket = Some(capture);
            let body = serde_json::to_vec(&Reply::OriginalEffect(Observation {
                status: CallStatus {
                    operation: "ap_collect_original_connect".into(),
                    returned: 0,
                    errno: None,
                },
                raw: effect,
            }))
            .unwrap();
            let mut calls = vec![];
            let ack = acknowledge_response(&envelope, &body, 7, |raw| {
                calls.push(*raw);
                ffi::CallStatus {
                    operation: "ap_ack_command",
                    returned: 0,
                    errno: None,
                }
            })
            .unwrap();
            assert_eq!(calls, vec![primary, auxiliary]);
            Provider::validate_command_acknowledgement(&ack).unwrap();
            Provider::require_original_acknowledgement(&ack).unwrap();
            assert!(Provider::require_uncollected_acknowledgement(&ack).is_err());
        }
    }
    #[test]
    fn original_socket_auxiliary_ack_failure_and_identity_mismatch_remain_failures() {
        let (mut envelope, _) = ack_fixture();
        envelope.operation = Operation::CollectOriginalConnect;
        envelope.body = serde_json::to_vec(&Request::CollectOriginalConnect {
            kind: crate::network_replay::original_connect::Kind::Socket,
            call: 19,
            command: 71,
            prepared_request: 1,
        })
        .unwrap();
        let (mut effect, capture) = super::super::installation_observation::fixture();
        effect.socket = Some(capture);
        let body = |effect| {
            serde_json::to_vec(&Reply::OriginalEffect(Observation {
                status: CallStatus {
                    operation: "ap_collect_original_connect".into(),
                    returned: 0,
                    errno: None,
                },
                raw: effect,
            }))
            .unwrap()
        };
        let mut calls = 0;
        let ack = acknowledge_response(&envelope, &body(effect.clone()), 7, |raw| {
            calls += 1;
            ffi::CallStatus {
                operation: "ap_ack_command",
                returned: if raw.operation == 14 { -1 } else { 0 },
                errno: (raw.operation == 14).then_some(libc::ESTALE),
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert!(Provider::validate_command_acknowledgement(&ack).is_err());
        assert!(Provider::require_original_acknowledgement(&ack).is_err());
        effect
            .socket
            .as_mut()
            .unwrap()
            .observation
            .as_mut()
            .unwrap()
            .raw
            .identity
            .provider += 1;
        assert!(
            acknowledge_response(&envelope, &body(effect), 7, |_| panic!(
                "changed observation ACK"
            ))
            .is_err()
        );
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

#[cfg(test)]
mod fd_ack_tests {
    use super::*;
    fn reply() -> Reply {
        let status = || CallStatus {
            operation: "read".into(),
            returned: 0,
            errno: None,
        };
        Reply::FdJournal {
            provider: Observation {
                status: status(),
                raw: ffi::Status::default().into(),
            },
            status: Observation {
                status: status(),
                raw: ffi::FdStatus {
                    next_event: 7,
                    ..Default::default()
                }
                .into(),
            },
            event: Observation {
                status: status(),
                raw: ffi::FdEvent {
                    sequence: 7,
                    complete: 1,
                    ..Default::default()
                }
                .into(),
            },
        }
    }
    fn envelope() -> Envelope {
        Envelope {
            run: [1; 16],
            sequence: 2,
            owner: None,
            accept: None,
            operation: Operation::DrainFdJournal,
            body: serde_json::to_vec(&Request::AwaitFdEvent {
                sequence: 7,
                acknowledged: None,
            })
            .unwrap(),
        }
    }
    #[test]
    fn fd_ack_never_acts_on_partial_failed_or_changed_event() {
        for cause in 0..9 {
            let mut r = reply();
            let Reply::FdJournal {
                provider,
                status,
                event,
            } = &mut r
            else {
                unreachable!()
            };
            match cause {
                0 => provider.status.returned = -1,
                1 => provider.raw.fatal = 1,
                2 => status.status.returned = -1,
                3 => status.raw.problem = 32,
                4 => event.status.returned = -1,
                5 => event.raw.complete = 2,
                6 => event.raw.sequence = 8,
                7 => status.raw.next_event = 6,
                _ => event.raw.sequence = 0,
            };
            let ack =
                acknowledge_fd_response(&envelope(), &serde_json::to_vec(&r).unwrap(), |_| {
                    panic!("unqualified raw ACK")
                })
                .unwrap();
            assert!(matches!(
                serde_json::from_slice::<CommandAcknowledgement>(&ack).unwrap(),
                CommandAcknowledgement::NotCollected
            ));
        }
    }
    #[test]
    fn fd_ack_preserves_exact_bytes_and_failure_without_enoent_inference() {
        let r = reply();
        let Reply::FdJournal { event, .. } = &r else {
            unreachable!()
        };
        let expected: ffi::FdEvent = event.raw.clone().into();
        let ack =
            acknowledge_fd_response(&envelope(), &serde_json::to_vec(&r).unwrap(), |actual| {
                assert_eq!(*actual, expected);
                ffi::CallStatus {
                    operation: "ap_ack_fd_event",
                    returned: -1,
                    errno: Some(libc::ENOENT),
                }
            })
            .unwrap();
        assert!(Provider::validate_command_acknowledgement(&ack).is_err());
        let CommandAcknowledgement::Observed(raw) = serde_json::from_slice(&ack).unwrap() else {
            panic!("lost raw failure")
        };
        assert_eq!(raw.errno, Some(libc::ENOENT));
    }
}

#[cfg(test)]
mod fd_pending_tests {
    use super::*;
    #[test]
    fn fd_unpublished_and_allocation_holes_remain_pending_with_raw_status() {
        for (errno, complete, sequence, expected_pending) in [
            (libc::ENOENT, 0, 0, true),
            (libc::ENODATA, 2, 7, true),
            (libc::ENODATA, 1, 7, true),
            (libc::ENODATA, 2, 8, false),
            (libc::EIO, 2, 7, false),
        ] {
            let ok = || CallStatus {
                operation: "read".into(),
                returned: 0,
                errno: None,
            };
            let event = Observation {
                status: CallStatus {
                    operation: "ap_read_fd_event".into(),
                    returned: -1,
                    errno: Some(errno),
                },
                raw: ffi::FdEvent {
                    sequence,
                    complete,
                    ..Default::default()
                }
                .into(),
            };
            let (ready, body) = fd_event_response(
                7,
                Observation {
                    status: ok(),
                    raw: ffi::Status::default().into(),
                },
                Observation {
                    status: ok(),
                    raw: ffi::FdStatus {
                        next_event: 7,
                        ..Default::default()
                    }
                    .into(),
                },
                event.clone(),
            )
            .unwrap();
            assert_eq!(!ready, expected_pending);
            let Reply::FdJournal {
                event: retained, ..
            } = serde_json::from_slice(&body).unwrap()
            else {
                panic!("raw pending lost")
            };
            assert_eq!(retained, event);
        }
    }
    #[test]
    fn fd_sticky_provider_failure_is_never_hidden_as_pending() {
        for cause in 0..4 {
            let ok = || CallStatus {
                operation: "read".into(),
                returned: 0,
                errno: None,
            };
            let mut provider: Observation<Status> = Observation {
                status: ok(),
                raw: ffi::Status::default().into(),
            };
            let mut status: Observation<FdStatus> = Observation {
                status: ok(),
                raw: ffi::FdStatus::default().into(),
            };
            match cause {
                0 => provider.status.returned = -1,
                1 => provider.raw.fatal = 1,
                2 => status.status.returned = -1,
                _ => status.raw.problem = 32,
            };
            let (ready, _) = fd_event_response(
                7,
                provider,
                status,
                Observation {
                    status: CallStatus {
                        operation: "read".into(),
                        returned: -1,
                        errno: Some(libc::ENOENT),
                    },
                    raw: ffi::FdEvent::default().into(),
                },
            )
            .unwrap();
            assert!(ready);
        }
    }
}

#[cfg(test)]
mod fd_probe_order_tests {
    use super::*;
    #[derive(Default)]
    struct ConcurrentPublication {
        calls: Vec<&'static str>,
        published: bool,
        problem: u64,
        fatal: u64,
    }
    fn observed<T>(raw: T) -> ffi::Observation<T> {
        ffi::Observation {
            status: ffi::CallStatus {
                operation: "ordered-call fixture",
                returned: 0,
                errno: None,
            },
            raw,
        }
    }
    impl FdEventSource for ConcurrentPublication {
        fn event(&mut self, sequence: u64) -> ffi::Observation<ffi::FdEvent> {
            self.calls.push("event");
            assert_eq!(sequence, 1);
            // The concurrent producer allocates and publishes while this read
            // is in flight. Any status read before it returns zero highwaters.
            self.published = true;
            observed(ffi::FdEvent {
                sequence,
                kind: 1,
                task: 3,
                task_start: 4,
                table: 5,
                file: 6,
                fd: 7,
                complete: 1,
                ..Default::default()
            })
        }
        fn fd_status(&mut self) -> ffi::Observation<ffi::FdStatus> {
            self.calls.push("fd_status");
            observed(if self.published {
                ffi::FdStatus {
                    next_event: 1,
                    next_table: 5,
                    next_file: 6,
                    problem: self.problem,
                }
            } else {
                ffi::FdStatus::default()
            })
        }
        fn provider_status(&mut self) -> ffi::Observation<ffi::Status> {
            self.calls.push("provider_status");
            observed(ffi::Status {
                fatal: if self.published { self.fatal } else { 0 },
                ..Default::default()
            })
        }
    }
    fn envelope() -> Envelope {
        Envelope {
            run: [1; 16],
            sequence: 2,
            owner: None,
            accept: None,
            operation: Operation::DrainFdJournal,
            body: serde_json::to_vec(&Request::AwaitFdEvent {
                sequence: 1,
                acknowledged: None,
            })
            .unwrap(),
        }
    }
    #[test]
    fn concurrent_publication_precedes_actual_counter_calls_and_exact_ack() {
        let mut source = ConcurrentPublication::default();
        let (ready, body) = poll_fd_event_source(&mut source, 1).unwrap();
        assert!(ready);
        assert_eq!(source.calls, ["event", "fd_status", "provider_status"]);
        let Reply::FdJournal { status, event, .. } = serde_json::from_slice(&body).unwrap() else {
            panic!("lost physical observation")
        };
        assert_eq!(
            (
                status.raw.next_event,
                status.raw.next_table,
                status.raw.next_file
            ),
            (1, 5, 6)
        );
        let mut history = super::super::fd_journal::History::default();
        history.retain(status.raw, event.raw.clone()).unwrap();
        assert_eq!(history.next().unwrap(), 2);
        let mut calls = 0;
        let ack = acknowledge_fd_response(&envelope(), &body, |actual| {
            calls += 1;
            assert_eq!(*actual, ffi::FdEvent::from(event.raw));
            ffi::CallStatus {
                operation: "ap_ack_fd_event",
                returned: 0,
                errno: None,
            }
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(
            matches!(serde_json::from_slice::<CommandAcknowledgement>(&ack).unwrap(),
            CommandAcknowledgement::Observed(raw) if raw.returned == 0)
        );
    }
    #[test]
    fn actual_post_publication_health_reads_prevent_acknowledgement() {
        for (problem, fatal) in [(32, 0), (0, 1)] {
            let mut source = ConcurrentPublication {
                problem,
                fatal,
                ..Default::default()
            };
            let (ready, body) = poll_fd_event_source(&mut source, 1).unwrap();
            assert!(ready);
            assert_eq!(source.calls, ["event", "fd_status", "provider_status"]);
            let ack = acknowledge_fd_response(&envelope(), &body, |_| panic!("failed health ACK"))
                .unwrap();
            assert!(matches!(
                serde_json::from_slice::<CommandAcknowledgement>(&ack).unwrap(),
                CommandAcknowledgement::NotCollected
            ));
        }
    }
}

#[cfg(test)]
mod abi6_birth_wire_tests {
    use super::*;
    #[test]
    fn birth_wire_requires_clear_tid_and_preserves_full_width() {
        let raw: NativeBirth = ffi::NativeBirth {
            clear_child_tid: 0x1234_5678_9abc_def0,
            ..Default::default()
        }
        .into();
        let mut wire = serde_json::to_value(&raw).unwrap();
        assert_eq!(
            serde_json::from_value::<NativeBirth>(wire.clone()).unwrap(),
            raw
        );
        wire.as_object_mut().unwrap().remove("clear_child_tid");
        assert!(serde_json::from_value::<NativeBirth>(wire).is_err());
    }
}

#[cfg(test)]
mod close_ack_tests {
    use super::*;
    use crate::network_replay::original_connect::Kind;
    #[test]
    fn original_ack_requires_the_exact_prepared_kind_and_keeps_close_eintr() {
        let thread = crate::types::DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        for kind in [Kind::Connect, Kind::Close] {
            let request = Request::CollectOriginalConnect {
                kind,
                call: 19,
                command: 17,
                prepared_request: 1,
            };
            let envelope = Envelope {
                run: [1; 16],
                sequence: 3,
                owner: Some(owner),
                accept: None,
                operation: Operation::CollectOriginalConnect,
                body: serde_json::to_vec(&request).unwrap(),
            };
            let command = ffi::CommandResult {
                command: 17,
                operation: kind.provider_operation(),
                phase: 1,
                task: 61,
                start_boottime: 99,
                identity: ffi::Identity {
                    provider: 5,
                    ..Default::default()
                },
                returned: -libc::EINTR,
                ..Default::default()
            };
            let body = |raw| {
                serde_json::to_vec(&Reply::OriginalEffect(Observation {
                    status: CallStatus {
                        operation: "ap_collect_original_connect".into(),
                        returned: 0,
                        errno: None,
                    },
                    raw: ffi::OriginalEffect {
                        command: raw,
                        ..Default::default()
                    }
                    .into(),
                }))
                .unwrap()
            };
            let mut calls = 0;
            let result = acknowledge_response(&envelope, &body(command), 5, |observed| {
                calls += 1;
                assert_eq!(observed.operation, kind.provider_operation());
                assert_eq!(observed.returned, -libc::EINTR);
                assert_eq!(observed.command, 17);
                ffi::CallStatus {
                    operation: "ap_ack_command",
                    returned: 0,
                    errno: None,
                }
            })
            .unwrap();
            assert_eq!(calls, 1);
            assert!(matches!(
                serde_json::from_slice::<CommandAcknowledgement>(&result).unwrap(),
                CommandAcknowledgement::Observed(CallStatus {
                    returned: 0,
                    errno: None,
                    ..
                })
            ));
            let mut wrong = command;
            wrong.operation = if kind == Kind::Close { 7 } else { 9 };
            assert!(
                acknowledge_response(&envelope, &body(wrong), 5, |_| panic!(
                    "wrong-kind ACK acted"
                ))
                .is_err()
            );
            wrong = command;
            wrong.identity.provider += 1;
            assert!(
                acknowledge_response(&envelope, &body(wrong), 5, |_| panic!(
                    "wrong-provider ACK acted"
                ))
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod file_socket_ack_tests {
    use super::*;
    use crate::network_replay::original_connect::FileOperation;
    use crate::network_replay::original_connect::Kind;
    #[test]
    fn original_file_read_socket_ack_consumes_each_exact_command_family() {
        let thread = crate::types::DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        for (kind, returned) in [
            (Kind::File(FileOperation::GetFlags), 2),
            (Kind::Read, 3),
            (Kind::Socket, 17),
            (Kind::Socket, -libc::EMFILE),
        ] {
            let request = Request::CollectOriginalConnect {
                kind,
                call: 19,
                command: 17,
                prepared_request: 1,
            };
            let envelope = Envelope {
                run: [1; 16],
                sequence: 3,
                owner: Some(owner),
                accept: None,
                operation: Operation::CollectOriginalConnect,
                body: serde_json::to_vec(&request).unwrap(),
            };
            let command = ffi::CommandResult {
                command: 17,
                operation: kind.provider_operation(),
                phase: 1,
                task: 61,
                start_boottime: 99,
                identity: ffi::Identity {
                    provider: 5,
                    ..Default::default()
                },
                returned,
                ..Default::default()
            };
            let body = |raw| {
                serde_json::to_vec(&Reply::OriginalEffect(Observation {
                    status: CallStatus {
                        operation: "ap_collect_original_connect".into(),
                        returned: 0,
                        errno: None,
                    },
                    raw: ffi::OriginalEffect {
                        command: raw,
                        ..Default::default()
                    }
                    .into(),
                }))
                .unwrap()
            };
            let mut calls = 0;
            let reply = acknowledge_response(&envelope, &body(command), 5, |observed| {
                calls += 1;
                assert_eq!(*observed, command);
                ffi::CallStatus {
                    operation: "ap_ack_command",
                    returned: 0,
                    errno: None,
                }
            })
            .unwrap();
            assert_eq!(calls, 1);
            assert!(matches!(
                serde_json::from_slice::<CommandAcknowledgement>(&reply).unwrap(),
                CommandAcknowledgement::Observed(CallStatus {
                    returned: 0,
                    errno: None,
                    ..
                })
            ));
            for bad in 0..4 {
                let mut wrong = command;
                match bad {
                    0 => wrong.operation = if kind == Kind::Socket { 11 } else { 12 },
                    1 => wrong.identity.provider += 1,
                    2 => wrong.phase = 0,
                    _ => wrong.task = 0,
                }
                assert!(
                    acknowledge_response(&envelope, &body(wrong), 5, |_| panic!(
                        "mismatched original ACK acted"
                    ))
                    .is_err()
                );
            }
        }
    }
}

impl Provider {
    pub(super) fn copy_poll_fd(&self) -> io::Result<Option<i32>> {
        if let Some(owner) = &self.grouped {
            if self.session.is_some() {
                return Err(io::Error::other("multiple provider session owners"));
            }
            return owner.copy_poll_fd();
        }
        self.session
            .as_ref()
            .filter(|s| s.is_ready())
            .map(|s| s.original_copy_poll_fd())
            .transpose()
    }
    pub(super) fn original_read_copy_ready(
        &mut self,
        pin: std::os::fd::BorrowedFd<'_>,
        command: u64,
        terminal: bool,
    ) -> io::Result<bool> {
        retained_session(&mut self.session, &mut self.grouped)?
            .ok_or_else(|| io::Error::other("Read copy provider missing"))?
            .original_read_copy_ready(pin, command, terminal)
    }
    pub(super) fn drain_copy(&mut self) -> io::Result<()> {
        if let Some(s) =
            retained_session(&mut self.session, &mut self.grouped)?.filter(|s| s.is_ready())
        {
            s.drain_original_copy()?;
        }
        Ok(())
    }
    pub(super) fn read_copy_progress(
        &mut self,
        pin: std::os::fd::BorrowedFd<'_>,
        command: u64,
    ) -> io::Result<super::original_read_copy::Progress> {
        retained_session(&mut self.session, &mut self.grouped)?
            .ok_or_else(|| io::Error::other("Read copy provider missing"))?
            .original_read_copy_progress(pin, command)
    }
    pub(super) fn copy_prefix(
        &mut self,
        pin: std::os::fd::BorrowedFd<'_>,
        command: u64,
        prepared: u64,
        first: u64,
    ) -> io::Result<Option<super::original_read_copy::Chunk>> {
        use super::original_read_copy::Chunk;
        use super::original_read_copy::End;
        use super::original_read_copy::RECORDS_PER_REPLY;
        let progress = self.read_copy_progress(pin, command)?;
        if first > progress.records {
            return Err(io::Error::other("Read copy request beyond actual prefix"));
        }
        let count = (progress.records - first).min(RECORDS_PER_REPLY as u64);
        let last = first + count == progress.records;
        let end = if last && progress.exited == 1 {
            Some(End::OriginalExit {
                protocol: progress.protocol == 1,
            })
        } else if last && progress.terminal == 1 {
            Some(End::ThreadTerminal)
        } else {
            None
        };
        if count == 0 && end.is_none() {
            return Ok(None);
        }
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .ok_or_else(|| io::Error::other("Read copy provider missing"))?;
        let mut records = Vec::new();
        for index in first..first + count {
            records.push(session.original_read_copy_record(command, index)?);
        }
        Ok(Some(Chunk {
            prepared,
            first,
            records,
            end,
        }))
    }
    pub(super) fn copy_for_terminal(
        &mut self,
        pin: std::os::fd::BorrowedFd<'_>,
        command: u64,
    ) -> io::Result<(
        Vec<super::original_read_copy::Record>,
        super::original_read_copy::End,
    )> {
        use super::original_read_copy::End;
        let progress = self.read_copy_progress(pin, command)?;
        if progress.terminal != 1 {
            return Err(io::Error::other(
                "Read copy terminal lacks positive finite drain",
            ));
        }
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .ok_or_else(|| io::Error::other("Read copy provider missing"))?;
        let mut records = Vec::new();
        for index in 0..progress.records {
            records.push(session.original_read_copy_record(command, index)?);
        }
        Ok((
            records,
            if progress.exited == 1 {
                End::OriginalExit {
                    protocol: progress.protocol == 1,
                }
            } else {
                End::ThreadTerminal
            },
        ))
    }
    /// Original C command remains retained until every record belongs to the
    /// service's existing completion Inbox. No payload lives in a new registry.
    pub(super) fn copy_for_completed(
        &mut self,
        body: &[u8],
    ) -> io::Result<Option<Vec<super::original_read_copy::Record>>> {
        let observed = match serde_json::from_slice::<Reply>(body)? {
            Reply::OriginalEffect(observed) => observed,
            Reply::OriginalFileObservation {
                effect: Some(observed),
                ..
            } => observed,
            _ => return Ok(None),
        };
        let Some(manifest) = observed.raw.read_copy else {
            return Ok(None);
        };
        if observed.status.returned != 0 {
            return Err(io::Error::other("Read copy on failed provider collection"));
        }
        let session = retained_session(&mut self.session, &mut self.grouped)?
            .ok_or_else(|| io::Error::other("Read copy provider missing"))?;
        manifest.validate_for_version(&observed.raw, session.wire_format().copy_version())?;
        let mut records = Vec::new();
        for index in 0..manifest.summary.records {
            records.push(session.original_read_copy_record(manifest.command, index)?);
        }
        if manifest.present == 1 {
            super::original_read_copy::validate_records_for_version(
                &observed.raw,
                &records,
                session.wire_format().copy_version(),
            )?;
        } else if !records.is_empty() {
            return Err(io::Error::other("absent Read protocol supplied data"));
        }
        Ok(Some(records))
    }
}

#[cfg(test)]
mod original_control_ack_tests {
    use super::*;
    use crate::network_replay::original_connect::Kind;
    use crate::types::DetTid;
    use crate::types::MmId;
    fn status() -> CallStatus {
        CallStatus {
            operation: "ap_collect_original_connect".into(),
            returned: 0,
            errno: None,
        }
    }
    fn fixture() -> (Envelope, Observation<OriginalEffect>) {
        let thread = DetTid::from_raw(61);
        let request = Envelope {
            run: [7; 16],
            sequence: 3,
            owner: Some(NetworkStreamOwner {
                thread,
                mm: MmId::initial(thread),
            }),
            accept: None,
            operation: Operation::CollectOriginalConnect,
            body: serde_json::to_vec(&Request::CollectOriginalConnect {
                kind: Kind::EpollCtl,
                call: 9,
                command: 26,
                prepared_request: 1,
            })
            .unwrap(),
        };
        let mut raw = ffi::OriginalEffect::default();
        raw.command = ffi::CommandResult {
            command: 26,
            operation: 20,
            phase: 1,
            task: 61,
            start_boottime: 99,
            identity: ffi::Identity {
                provider: 3,
                object: 0,
                namespace: 0,
            },
            original_count: 4,
            ..Default::default()
        };
        // This exercises the physical command ACK router. The independent
        // transport controls check the two-file semantic receipt itself.
        (
            request,
            Observation {
                status: status(),
                raw: raw.into(),
            },
        )
    }
    #[test]
    fn original_control_ack_uses_exact_new_opcode_and_preserves_provider_failure() {
        let (request, observed) = fixture();
        let body = serde_json::to_vec(&Reply::OriginalEffect(observed.clone())).unwrap();
        let mut calls = 0;
        let ack = acknowledge_response(&request, &body, 3, |raw| {
            calls += 1;
            assert_eq!(
                (raw.command, raw.operation, raw.identity.provider),
                (26, 20, 3)
            );
            assert_eq!((raw.identity.object, raw.identity.namespace), (0, 0));
            ffi::CallStatus {
                operation: "ap_ack_command",
                returned: 0,
                errno: None,
            }
        })
        .unwrap();
        assert_eq!(calls, 1);
        Provider::require_original_acknowledgement(&ack).unwrap();
        Provider::validate_command_acknowledgement(&ack).unwrap();
        for wrong in 0..7 {
            let mut changed = observed.clone();
            match wrong {
                0 => changed.raw.command.operation = 13, // copy-only is never original ctl
                1 => changed.raw.command.operation = 7,
                2 => changed.raw.command.command = 0,
                3 => changed.raw.command.phase = 3,
                4 => changed.raw.command.identity.provider = 4,
                5 => changed.raw.command.task = 0,
                6 => changed.raw.command.start_boottime = 0,
                _ => unreachable!(),
            }
            let bytes = serde_json::to_vec(&Reply::OriginalEffect(changed)).unwrap();
            assert!(
                acknowledge_response(&request, &bytes, 3, |_| panic!(
                    "mismatched command acknowledged"
                ))
                .is_err(),
                "case {wrong}"
            );
        }
        for returned in [-1, 0] {
            let mut failed = observed.clone();
            failed.status.returned = returned;
            failed.status.errno = Some(libc::EIO);
            let bytes = serde_json::to_vec(&Reply::OriginalEffect(failed)).unwrap();
            let ack = acknowledge_response(&request, &bytes, 3, |_| {
                panic!("failed collection acknowledged")
            })
            .unwrap();
            assert!(matches!(
                serde_json::from_slice(&ack).unwrap(),
                CommandAcknowledgement::NotCollected
            ));
            assert!(Provider::require_original_acknowledgement(&ack).is_err());
        }
        for returned in [-1, 0] {
            let ack = serde_json::to_vec(&CommandAcknowledgement::Observed(CallStatus {
                operation: "ap_ack_command".into(),
                returned,
                errno: Some(libc::EIO),
            }))
            .unwrap();
            assert!(Provider::require_original_acknowledgement(&ack).is_err());
            assert!(Provider::validate_command_acknowledgement(&ack).is_err());
        }
    }
}

#[cfg(test)]
mod auxiliary_file_ack_tests {
    use super::*;
    use crate::network_replay::original_connect::FileOperation;
    use crate::network_replay::original_connect::Kind;
    use crate::types::DetTid;
    use crate::types::MmId;
    #[test]
    fn original_openat_auxiliary_ack_requires_explicit_worker_opcode() {
        let thread = DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let envelope = Envelope {
            run: [3; 16],
            sequence: 42,
            owner: Some(owner),
            accept: None,
            operation: Operation::CollectOriginalFileObservation,
            body: serde_json::to_vec(&Request::CollectOriginalFileObservation {
                call: 17,
                command: 91,
                prepared_request: 41,
                role: AuxiliaryRole::File,
            })
            .unwrap(),
        };
        let good = |operation: &str| CallStatus {
            operation: operation.into(),
            returned: 0,
            errno: None,
        };
        let selection: OriginalSelection = ffi::OriginalSelection {
            command: 91,
            call: 17,
            owner_mm: owner.mm.generation(),
            provider: 7,
            task: 41,
            task_start: 101,
            table: 13,
            file: 19,
            requested_fd: 88,
            user_address: libc::SYS_fcntl as u64,
            address_length: libc::F_GETFL,
            ready: 1,
            ..Default::default()
        }
        .into();
        let mut effect: OriginalEffect = ffi::OriginalEffect::default().into();
        effect.original.selection = selection.clone();
        effect.command.command = 91;
        effect.command.operation = 23;
        effect.command.phase = 1;
        effect.command.task = 41;
        effect.command.start_boottime = 101;
        effect.command.identity.provider = 7;
        let reply = |effect: OriginalEffect| Reply::OriginalFileObservation {
            selection: Observation {
                status: good("ap_read_original_selection"),
                raw: selection.clone(),
            },
            effect: Some(Observation {
                status: good("ap_collect_original_connect"),
                raw: effect,
            }),
        };
        let body = serde_json::to_vec(&reply(effect.clone())).unwrap();
        let mut called = 0;
        let ack = acknowledge_response(&envelope, &body, 7, |raw| {
            called += 1;
            assert_eq!(
                (raw.command, raw.operation, raw.task, raw.start_boottime),
                (91, 23, 41, 101)
            );
            ffi::CallStatus {
                operation: "ap_ack_command",
                returned: 0,
                errno: None,
            }
        })
        .unwrap();
        assert_eq!(called, 1);
        Provider::require_original_acknowledgement(&ack).unwrap();
        for operation in [10, 13, 20] {
            let mut wrong = effect.clone();
            wrong.command.operation = operation;
            assert!(
                acknowledge_response(
                    &envelope,
                    &serde_json::to_vec(&reply(wrong)).unwrap(),
                    7,
                    |_| panic!("wrong role reached auxiliary ACK")
                )
                .is_err()
            );
        }
        // An op23 receipt is likewise not a guest OriginalFile completion.
        let mut guest = envelope;
        guest.operation = Operation::CollectOriginalConnect;
        guest.body = serde_json::to_vec(&Request::CollectOriginalConnect {
            kind: Kind::File(FileOperation::GetFlags),
            call: 17,
            command: 91,
            prepared_request: 41,
        })
        .unwrap();
        let body = serde_json::to_vec(&Reply::OriginalEffect(Observation {
            status: good("ap_collect_original_connect"),
            raw: effect,
        }))
        .unwrap();
        assert!(
            acknowledge_response(&guest, &body, 7, |_| panic!(
                "auxiliary receipt acknowledged as guest OriginalFile"
            ))
            .is_err()
        );
    }
}

#[cfg(test)]
mod helper_receive_ack_tests {
    use super::*;
    use crate::types::DetTid;
    use crate::types::MmId;
    #[test]
    fn helper_receive_ack_refuses_cross_role_file_and_operand_substitution() {
        let thread = DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        assert_eq!(owner.mm.generation(), 0);
        let ok = |name: &str| CallStatus {
            operation: name.into(),
            returned: 0,
            errno: None,
        };
        for kind in [ReceiveKind::Drain, ReceiveKind::Peek] {
            let count = if kind == ReceiveKind::Drain { 1 } else { 1024 };
            let role = AuxiliaryRole::Receive {
                kind,
                address: 0x8000,
                count,
                provider: 7,
                file: 19,
            };
            let (_, flags, _) = role.operands();
            let request = Envelope {
                run: [3; 16],
                sequence: 42,
                owner: Some(owner),
                accept: None,
                operation: Operation::CollectOriginalFileObservation,
                body: serde_json::to_vec(&Request::CollectOriginalFileObservation {
                    call: 17,
                    command: 91,
                    prepared_request: 41,
                    role,
                })
                .unwrap(),
            };
            let mut raw: OriginalEffect = ffi::OriginalEffect::default().into();
            raw.original.selection = ffi::OriginalSelection {
                command: 91,
                call: 17,
                owner_mm: owner.mm.generation(),
                provider: 7,
                task: 41,
                task_start: 101,
                table: 13,
                file: 19,
                requested_fd: 88,
                user_address: 0x8000,
                address_length: flags,
                original_count: count,
                ready: 1,
                ..Default::default()
            }
            .into();
            raw.command.command = 91;
            raw.command.operation = role.operation();
            raw.command.phase = 1;
            raw.command.task = 41;
            raw.command.start_boottime = 101;
            raw.command.identity.provider = 7;
            raw.command.original_count = count;
            let body = |raw: OriginalEffect| {
                serde_json::to_vec(&Reply::OriginalFileObservation {
                    selection: Observation {
                        status: ok("ap_read_original_selection"),
                        raw: raw.original.selection.clone(),
                    },
                    effect: Some(Observation {
                        status: ok("ap_collect_original_connect"),
                        raw,
                    }),
                })
                .unwrap()
            };
            let ack = acknowledge_response(&request, &body(raw.clone()), 7, |command| {
                assert_eq!(command.operation, role.operation());
                ffi::CallStatus {
                    operation: "ap_ack_command",
                    returned: 0,
                    errno: None,
                }
            })
            .unwrap();
            Provider::require_original_acknowledgement(&ack).unwrap();
            for case in 0..9 {
                let mut wrong = raw.clone();
                match case {
                    0 => wrong.command.operation = 23,
                    1 => wrong.original.selection.file += 1,
                    2 => wrong.original.selection.provider += 1,
                    3 => wrong.original.selection.fdput_flags = 1,
                    4 => wrong.original.selection.address_length ^= libc::MSG_PEEK,
                    5 => wrong.original.selection.original_count += 1,
                    6 => wrong.original.selection.call += 1,
                    7 => wrong.command.original_count += 1,
                    8 => wrong.original.selection.owner_mm += 1,
                    _ => unreachable!(),
                }
                assert!(
                    acknowledge_response(&request, &body(wrong), 7, |_| panic!(
                        "mutated helper role reached physical ACK"
                    ))
                    .is_err(),
                    "mutation {case}"
                );
            }
        }
    }
    #[test]
    fn auxiliary_role_is_explicit_and_helper_operands_keep_original_bounds() {
        assert!(
            serde_json::from_value::<Request>(
                serde_json::json!({"PrepareOriginalFileObservation": {
            "call": 17, "mm": 1, "fd": 88 }})
            )
            .is_err()
        );
        for kind in [ReceiveKind::Drain, ReceiveKind::Peek] {
            let count = if kind == ReceiveKind::Drain { 1 } else { 1024 };
            let make = |address, count, provider, file| AuxiliaryRole::Receive {
                kind,
                address,
                count,
                provider,
                file,
            };
            assert!(make(0x8000, count, 7, 19).valid());
            for bad in [
                make(0, count, 7, 19),
                make(0x8000, 0, 7, 19),
                make(0x8000, count, 0, 19),
                make(0x8000, count, 7, 0),
                make(0x8000, 0x7fff_f001, 7, 19),
            ] {
                assert!(!bad.valid());
            }
        }
        assert!(
            !AuxiliaryRole::Receive {
                kind: ReceiveKind::Drain,
                address: 1,
                count: 513,
                provider: 1,
                file: 1
            }
            .valid()
        );
        assert!(
            !AuxiliaryRole::Receive {
                kind: ReceiveKind::Peek,
                address: 1,
                count: 1023,
                provider: 1,
                file: 1
            }
            .valid()
        );
    }
}
