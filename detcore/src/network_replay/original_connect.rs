//! The existing call owner across original connect's FD selection and return.
//!
//! This is neither a descriptor ledger nor a new network engine. The same
//! table permit protects capture through the *positive* original fdget receipt.
//! Owner disappearance and missing hooks never release that exclusion.

use super::*;
use crate::resources::ExternalOpId;
use crate::types::FdSlotBinding;

#[path = "original_connect/epoll_ctl.rs"]
mod epoll_ctl;
pub(crate) mod foreground_close;

/// The original syscall owned by the same admission, early observation and
/// final-result state machine. Close never acquires a duplicate guest FD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    Connect,
    Close,
    File(FileOperation),
    Read,
    Socket,
    Openat,
    EpollCreate { legacy: bool },
    EpollCtl,
    Sendto,
    BlockingSendto { timeout_ticks: u64 },
}
/// Operations on the file selected by the original kernel invocation. Numeric
/// slot operations (F_GETFD/F_SETFD) deliberately do not belong to this class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FileOperation {
    GetFlags,
}
impl FileOperation {
    pub(crate) fn syscall(self) -> reverie::syscalls::Sysno {
        match self {
            Self::GetFlags => reverie::syscalls::Sysno::fcntl,
        }
    }
    pub(crate) fn command(self) -> i32 {
        match self {
            Self::GetFlags => libc::F_GETFL,
        }
    }
}
impl Kind {
    pub(crate) fn allocator(self) -> bool {
        matches!(self, Self::Socket | Self::Openat | Self::EpollCreate { .. })
    }
    pub(crate) fn provider_operation(self) -> u64 {
        match self {
            Self::Connect => 7,
            Self::Close => 9,
            Self::File(_) => 10,
            Self::Read => 11,
            Self::Socket => 12,
            Self::Openat => 18,
            Self::EpollCreate { .. } => 19,
            Self::EpollCtl => 20,
            Self::Sendto => 24,
            Self::BlockingSendto { .. } => 25,
        }
    }
    pub(crate) fn syscall(self) -> reverie::syscalls::Sysno {
        match self {
            Self::Connect => reverie::syscalls::Sysno::connect,
            Self::Close => reverie::syscalls::Sysno::close,
            Self::File(operation) => operation.syscall(),
            Self::Read => reverie::syscalls::Sysno::read,
            Self::Socket => reverie::syscalls::Sysno::socket,
            Self::Openat => reverie::syscalls::Sysno::openat,
            Self::EpollCreate { legacy: true } => reverie::syscalls::Sysno::epoll_create,
            Self::EpollCreate { legacy: false } => reverie::syscalls::Sysno::epoll_create1,
            Self::EpollCtl => reverie::syscalls::Sysno::epoll_ctl,
            Self::Sendto | Self::BlockingSendto { .. } => reverie::syscalls::Sysno::sendto,
        }
    }
    pub(crate) fn valid_operands(self, address: u64, length: i32, original_count: u64) -> bool {
        match self {
            Self::Connect => original_count == 0,
            Self::Close => address == 0 && length == 0 && original_count == 0,
            Self::File(operation) => {
                address == operation.syscall() as u64
                    && length == operation.command()
                    && original_count == 0
            }
            Self::Read => length == 0,
            Self::Sendto => address != 0 && (1..=512).contains(&original_count)
                && matches!(length, libc::MSG_NOSIGNAL | 0x4040),
            Self::BlockingSendto { timeout_ticks } => address != 0
                && (1..=512).contains(&original_count) && length == libc::MSG_NOSIGNAL
                && (1..=i64::MAX as u64 - 1).contains(&timeout_ticks),
            Self::Socket => address <= u64::from(u32::MAX) && original_count == 0,
            Self::EpollCtl => original_count <= u64::from(u32::MAX), // target FD low int bits
            Self::Openat => true, // exact pathname/mode; Linux owns flags, access and uaccess errors
            Self::EpollCreate { .. } => {
                address == self.syscall() as u64 && length == 0 && original_count == 0
            }
        }
    }
    pub(crate) fn valid_counted_result(self, returned: i64, original_count: u64) -> bool {
        self.valid_result(returned)
            && match self {
                Self::Read | Self::Sendto | Self::BlockingSendto { .. } => returned < 0 || returned as u64 <= original_count,
                Self::Openat | Self::EpollCtl => true, // mode or target FD, not a byte limit
                _ => original_count == 0,
            }
    }
    pub(crate) fn valid_result(self, returned: i64) -> bool {
        match self {
            Self::Connect | Self::Close | Self::EpollCtl => (-4095..=0).contains(&returned),
            // F_GETFL returns an int flag word; this is not an unconstrained
            // positive-result contract for allocators, lseek or other syscalls.
            Self::File(FileOperation::GetFlags)
            | Self::Socket
            | Self::Openat
            | Self::EpollCreate { .. } => (-4095..=i64::from(i32::MAX)).contains(&returned),
            // Installed scalar Read clamps positive completion to MAX_RW_COUNT.
            // Actual kernel restart errors remain raw negative results.
            Self::Read => (-4095..=0x7fff_f000).contains(&returned),
            Self::Sendto | Self::BlockingSendto { .. } => (-4095..=512).contains(&returned),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arguments {
    pub(crate) kind: Kind,
    pub(crate) operation: ExternalOpId,
    pub(crate) files: FilesId,
    pub(crate) binding: Option<FdSlotBinding>,
    pub(crate) fd: i32,
    pub(crate) address: u64,
    pub(crate) length: i32,
    /// Full Read count/Openat mode, or the exact zero-extended epoll target int.
    /// Zero for other kinds.
    pub(crate) original_count: u64,
}
impl Arguments {
    fn same_request(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.operation == other.operation
            && self.files == other.files
            && self.fd == other.fd
            && self.address == other.address
            && self.length == other.length
            && self.original_count == other.original_count
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub(crate) call: NetworkStreamCallId,
    pub(crate) arguments: Arguments,
}

/// Failed-run retirement authority for this exact original Call. This is issued
/// only after backend final wait, provider ACK, and physical pin retirement. It
/// contains no native return or assertion that either fdget was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectionTerminal {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    files: FilesId,
}
impl SelectionTerminal {
    pub(super) fn matches(
        self,
        owner: lifetime::TaskOwner,
        files: FilesId,
        lease: lifetime::LeaseId,
    ) -> bool {
        owner.tid == self.owner.thread
            && owner.mm == self.owner.mm
            && files == self.files
            && lease.operation == ExternalOpId::new(self.owner.thread, self.call.0)
            && lease.mm == self.owner.mm
            && lease.kind == lifetime::LeaseKind::StreamCall
            && lease.ordinal == 0
    }
}

impl NetworkReplayEngine {
    pub(crate) fn original_selection_terminal_retirement(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<SelectionTerminal, NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.final_wait
            || !original.provider_submitted
            || original.command.is_none()
            || !original.provider_retired
            || !original.pin_released
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
        {
            return Err(protocol(
                "original selection lacks exact final-wait and physical retirement",
            ));
        }
        Ok(SelectionTerminal {
            owner,
            call: admission.call,
            files: admission.arguments.files,
        })
    }
}
/// Local custody is installed before the first suspending admission RPC. Only
/// its `invoked == false` value in an actually consumed ThreadState authorizes
/// the provider's known-uninvoked disarm; READY alone is not that proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Local {
    pub(crate) arguments: Arguments,
    pub(crate) raw_arguments: [usize; 6],
    pub(crate) admission: Option<Admission>,
    pub(crate) invoked: bool,
    pub(crate) returned: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileMetadataObservation {
    pub admission: Admission,
    pub logical_nonblocking: Option<bool>,
    /// Same admitted description's status flags, not a fresh numeric-FD query.
    pub status_flags: Option<i32>,
}
/// Actual held-pin classification from the existing native worker. Empty is
/// permitted only when both authoritative admission and pidfd_getfd say empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Pin {
    Empty,
    Path,
    Other,
    Socket {
        domain: i32,
        kind: i32,
        protocol: i32,
    },
}
#[derive(Debug, Clone)]
pub(super) struct OriginalCallState {
    arguments: Arguments,
    source: OriginalResultSource,
    foreground_close: Option<std::sync::Arc<foreground_close::ForegroundCloseOrigin>>,
    external_grant: Option<ExternalOpId>,
    // Original allocator admission survives mutation publication until Call ACK.
    socket_mutation: Option<NetworkFdMutationAdmission>,
    // Allocators retain Call/metadata custody before the blocking syscall,
    // but acquire the existing table permit only after actual completion.
    allocation_published: bool,
    // Same original Call owns terminal recovery. A successor is an actual live
    // owner of the same table, never a replacement incarnation of this task.
    terminal_publication: Option<NetworkStreamOwner>,
    terminal_prior_mutation: Option<NetworkFdMutationAdmission>,
    terminal_recovered_enrollment: Option<super::original_installation::Enrollment>,
    terminal_reconciled: Option<crate::network_runtime::original_installation::Installation>,
    terminal_kernel_result: Option<i64>,
    terminal_no_installation: Option<crate::network_runtime::original_installation::NoInstallation>,
    pin: Option<Pin>,
    command: Option<u64>,
    provider_submitted: bool,
    selected: Option<(u64, u64, u64, u64, u64)>, // provider/task incarnation/table/file
    // Prepared metadata predicts only the admitted read shape. The actual
    // kernel selection is still mandatory, including null for O_PATH.
    read_expects_file: Option<bool>,
    // Same raw owner as the native Driver; only completed metadata is counted.
    // No receipt is a stream release, replay unit or implicit consumption ACK.
    read_copy: Option<(
        std::sync::Arc<crate::network_runtime::original_read_copy::ReadCopyCustody>,
        usize,
    )>,
    // Same Call reserves possible semantic selections without a physical pin
    // or table permit while Linux copies the epoll event before its fdgets.
    epoll_control: Option<epoll_ctl::Control>,
    backend_result: Option<i64>,
    backend_entered: bool,
    final_wait: bool,
    uninvoked: bool,
    provider_retired: bool,
    pin_released: bool,
    consumed: bool,
    cancel_requested: bool,
    cancel_disarmed: bool,
}
#[derive(Debug, Clone, Copy)]
enum OriginalResultSource {
    Native,
    // Logical selection is not fdget; there is deliberately no physical tuple,
    // backend return, provider command or provider-retired fact in this branch.
    Recorded { selected: bool },
    // Detcore's modeled Read (procfs/RNG/network replay), not a recorded
    // Return and not a physical invocation. It uses the same lifetime Call.
    EmulatedRead { selected: bool },
}
impl OriginalCallState {
    pub(super) fn is_native_send(&self) -> bool {
        self.arguments.kind == Kind::Sendto && matches!(self.source, OriginalResultSource::Native)
    }
    pub(super) fn native_send_entry_unsubmitted(&self) -> bool {
        self.is_native_send() && self.native_entry_unsubmitted(self.arguments.operation)
    }
    pub(super) fn native_entry_cancellable(&self) -> bool {
        self.native_entry_unsubmitted(self.arguments.operation)
            && self.pin.is_none()
            && self.read_copy.is_none()
            && self.epoll_control.is_none()
            && self.terminal_publication.is_none()
            && !self.consumed
            && !self.cancel_disarmed
    }
    pub(super) fn native_entry_unsubmitted(
        &self,
        operation: crate::resources::ExternalOpId,
    ) -> bool {
        matches!(self.source, OriginalResultSource::Native)
            && matches!(self.arguments.kind, Kind::Connect | Kind::Sendto)
            && self.arguments.operation == operation
            && self.external_grant == (if self.arguments.kind == Kind::Sendto { None } else { Some(operation) })
            && self.command.is_none()
            && !self.provider_submitted
            && !self.backend_entered
            && self.backend_result.is_none()
            && self.selected.is_none()
            && !self.final_wait
            && self.uninvoked
            && !self.cancel_requested
            && !self.provider_retired
            && !self.pin_released
    }
}

fn protocol(message: &str) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(message.into())
}
impl NetworkReplayEngine {
    pub(crate) fn begin_original_connect(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
    ) -> Result<Admission, NetworkReplayError> {
        if !self.fd_table_capability()
            || arguments.kind.allocator()
            || arguments.kind == Kind::EpollCtl
            || matches!(arguments.kind, Kind::BlockingSendto { .. })
        {
            return Err(protocol(
                "original connect requires complete descriptor mutation authority",
            ));
        }
        self.begin_original_call(owner, arguments, OriginalResultSource::Native)
    }
    /// Transfer the admission fixed by the original external scheduling grant.
    /// Native preparation remains on the same Call; this token is not fdget.
    pub(crate) fn begin_original_external_from_read(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
        read: NetworkFdReadAdmission,
    ) -> Result<Admission, NetworkReplayError> {
        if !self.fd_table_capability() || !matches!(arguments.kind, Kind::Connect | Kind::Close) {
            return Err(protocol(
                "external reader transfer changed authority or operation family",
            ));
        }
        self.begin_original_call_with_read(
            owner,
            arguments,
            OriginalResultSource::Native,
            Some(read),
        )
    }

    pub(super) fn original_external_selection_owner(
        &self,
        permit: Option<super::NetworkFdPublicationPermit>,
        control: Option<super::NetworkStreamLeaseId>,
    ) -> Option<(NetworkStreamOwner, ExternalOpId)> {
        self.stream_calls.values().find_map(|state| {
            if !((permit.is_some() && state.capture_publication == permit)
                || (control.is_some() && state.capture_control == control))
            {
                return None;
            }
            let original = state.original.as_ref()?;
            let operation = original.external_grant?;
            (matches!(original.source, OriginalResultSource::Native)
                && matches!(
                    original.arguments.kind,
                    Kind::Connect | Kind::Close | Kind::Read
                )
                && original.arguments.operation == operation
                && !original.final_wait)
                .then_some((state.owner, operation))
        })
    }

    /// Transfer an already classified file read into the same original Call.
    /// Neither result provenance nor physical selection comes from the read.
    pub(crate) fn begin_original_file_from_read(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
        read: NetworkFdReadAdmission,
        source: crate::OriginalFileExecution,
    ) -> Result<Admission, NetworkReplayError> {
        if !matches!(arguments.kind, Kind::File(_) | Kind::Read) {
            return Err(protocol("file reader transfer changed operation family"));
        }
        let source = match source {
            crate::OriginalFileExecution::Native => {
                if !self.fd_table_capability() {
                    return Err(protocol(
                        "original connect requires complete descriptor mutation authority",
                    ));
                }
                OriginalResultSource::Native
            }
            crate::OriginalFileExecution::Recorded => {
                if self.mode() != NetworkEngineMode::Replay {
                    return Err(protocol(
                        "recorded file admission requires the validated Replay engine",
                    ));
                }
                OriginalResultSource::Recorded { selected: false }
            }
        };
        self.begin_original_call_with_read(owner, arguments, source, Some(read))
    }

    fn begin_original_call(
        &mut self,
        owner: NetworkStreamOwner,
        mut arguments: Arguments,
        source: OriginalResultSource,
    ) -> Result<Admission, NetworkReplayError> {
        if matches!(arguments.kind, Kind::File(_) | Kind::Read | Kind::Sendto) {
            // Keep the original RPC's scheduling and wait contract. This common
            // logical handoff is synchronous under its existing engine owner;
            // it adds no generic stream RPC or caller-visible scheduling step.
            self.validate_original_call_arguments(owner, &arguments)?;
            let read = match self.begin_fd_read(owner, arguments.files, arguments.fd)? {
                NetworkFdReadBegin::Recover => {
                    return Err(protocol(
                        "original invocation requires prior publication recovery",
                    ));
                }
                NetworkFdReadBegin::Admitted(read) => *read,
            };
            arguments.binding = read.binding;
            let result =
                self.begin_original_call_with_read(owner, arguments, source, Some(read.clone()));
            if result.is_err() {
                // There was no successful transfer and no physical submission.
                // Preserve the pre-existing begin failure's empty custody.
                self.finish_fd_read(owner, read)?;
            }
            result
        } else {
            self.begin_original_call_with_read(owner, arguments, source, None)
        }
    }

    fn validate_original_call_arguments(
        &self,
        owner: NetworkStreamOwner,
        arguments: &Arguments,
    ) -> Result<(TaskOwner, u64, NetworkStreamCallId), NetworkReplayError> {
        if matches!(arguments.kind, Kind::Connect | Kind::Sendto | Kind::BlockingSendto { .. }) {
            self.check_native_retirement()?;
        }
        if arguments.kind == Kind::Close && (arguments.address != 0 || arguments.length != 0) {
            return Err(protocol("close admission contains connect operands"));
        }
        if let Kind::File(operation) = arguments.kind
            && (arguments.address != operation.syscall() as u64
                || arguments.length != operation.command())
        {
            return Err(protocol("file operation changed its syscall/command"));
        }
        if !arguments.kind.valid_operands(
            arguments.address,
            arguments.length,
            arguments.original_count,
        ) {
            return Err(protocol(
                "original operation changed its exact scalar operand shape",
            ));
        }
        let task = self.publication_owner(owner, arguments.files)?;
        if self
            .stream_calls
            .values()
            .any(|state| state.owner == owner && state.original.is_some())
        {
            return Err(protocol(
                "an original invocation is already owned by this task/MM",
            ));
        }
        if arguments.binding.is_some_and(|binding| {
            binding.slot.files != arguments.files || binding.slot.fd != arguments.fd
        }) {
            return Err(protocol(
                "original request changed its table/operand snapshot",
            ));
        }
        let next = self
            .next_stream_call
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let call = NetworkStreamCallId(self.next_stream_call);
        Ok((task, next, call))
    }

    fn begin_original_call_with_read(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
        source: OriginalResultSource,
        read: Option<NetworkFdReadAdmission>,
    ) -> Result<Admission, NetworkReplayError> {
        self.begin_original_call_with_mutation(owner, arguments, source, read, None)
    }

    pub(crate) fn begin_original_socket(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
        mutation: NetworkFdMutationAdmission,
    ) -> Result<Admission, NetworkReplayError> {
        self.validate_original_socket_mutation(owner, &mutation)?;
        if arguments.kind != Kind::Socket
            || arguments.binding.is_some()
            || arguments.files != mutation.publication.permit.files
        {
            return Err(protocol("original Socket changed its admitted mutation"));
        }
        self.begin_original_call_with_mutation(
            owner,
            arguments,
            OriginalResultSource::Native,
            None,
            Some(mutation),
        )
    }

    pub(crate) fn begin_original_allocator(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
    ) -> Result<Admission, NetworkReplayError> {
        if !self.fd_table_capability() || !arguments.kind.allocator() || arguments.binding.is_some()
        {
            return Err(protocol(
                "original allocator requires actual owner/table custody",
            ));
        }
        self.begin_original_call_with_mutation(
            owner,
            arguments,
            OriginalResultSource::Native,
            None,
            None,
        )
    }

    fn begin_original_call_with_mutation(
        &mut self,
        owner: NetworkStreamOwner,
        mut arguments: Arguments,
        source: OriginalResultSource,
        read: Option<NetworkFdReadAdmission>,
        mutation: Option<NetworkFdMutationAdmission>,
    ) -> Result<Admission, NetworkReplayError> {
        let (task, next, call) = self.validate_original_call_arguments(owner, &arguments)?;
        let external_grant = read.as_ref().and_then(|read| read.external_grant);
        if external_grant.is_some_and(|operation| operation != arguments.operation)
            || (external_grant.is_some()
                && match arguments.kind {
                    Kind::Read => false,
                    Kind::Connect | Kind::Close => !matches!(source, OriginalResultSource::Native),
                    Kind::File(_)
                    | Kind::Sendto
                    | Kind::BlockingSendto { .. }
                    | Kind::Socket
                    | Kind::Openat
                    | Kind::EpollCreate { .. }
                    | Kind::EpollCtl => true,
                })
        {
            return Err(protocol(
                "original call changed its selected external request",
            ));
        }
        let (publication, controls) = if (arguments.kind.allocator()
            || arguments.kind == Kind::EpollCtl)
            && mutation.is_none()
        {
            if read.is_some() || arguments.binding.is_some() {
                return Err(protocol("allocator cannot consume a source descriptor"));
            }
            (None, Vec::new())
        } else if let Some(mutation) = &mutation {
            if read.is_some() {
                return Err(protocol("allocator cannot consume a descriptor read"));
            }
            self.validate_original_socket_mutation(owner, mutation)?;
            (
                Some(mutation.publication.clone()),
                mutation.controls.clone(),
            )
        } else if let Some(read) = &read {
            self.validate_fd_read(owner, read)?;
            if arguments.files != read.publication.permit.files
                || arguments.fd != read.fd
                || arguments.binding != read.binding
            {
                return Err(protocol("original file transfer changed admitted lookup"));
            }
            (
                Some(read.publication.clone()),
                read.binding
                    .zip(read.control)
                    .map(|(binding, lease)| (binding.open_file, lease))
                    .into_iter()
                    .collect(),
            )
        } else {
            let publication = self.acquire_fd_publication(owner, arguments.files)?;
            if publication.recovery.is_some() {
                self.fd_publications
                    .get_mut(&arguments.files)
                    .unwrap()
                    .active = None;
                return Err(protocol(
                    "original invocation requires prior publication recovery",
                ));
            }
            // The caller's pre-await binding is a snapshot, not authority after a
            // contending mutation. Select the actual current slot only after this
            // engine owns the table permit; retain this binding in the returned Call.
            arguments.binding = self.lifetime.descriptor_binding(task, arguments.fd).ok();
            let open_file = arguments.binding.map(|b| b.open_file);
            let controls =
                match self.begin_descriptor_controls(owner, open_file.into_iter().collect()) {
                    Ok(controls) => controls,
                    Err(error) => {
                        self.release_empty_fd_publication(owner, publication.permit)?;
                        return Err(error);
                    }
                };
            (Some(publication), controls)
        };
        let open_file = arguments.binding.map(|b| b.open_file);
        if let Some(binding) = arguments.binding {
            let retained = match source {
                OriginalResultSource::Native => {
                    self.retain_stream_call_lifetime(owner, call, binding.open_file, Some(binding))
                }
                OriginalResultSource::Recorded { .. }
                | OriginalResultSource::EmulatedRead { .. } => self
                    .retain_registered_stream_call_lifetime(
                        owner,
                        call,
                        binding.open_file,
                        Some(binding),
                    ),
            };
            if let Err(error) = retained {
                // A failed consuming transfer leaves the exact logical token
                // owned by the table. It can only be released through that token
                // or actual owner cleanup, never an unbound generic release.
                if read.is_some() {
                    return Err(error);
                }
                for (_, lease) in controls {
                    self.finish_socket_control(
                        owner,
                        lease,
                        NetworkSocketControlFinish::Unchanged,
                    )?;
                }
                self.release_empty_fd_publication(
                    owner,
                    publication
                        .as_ref()
                        .expect("descriptor call owns permit")
                        .permit,
                )?;
                return Err(error);
            }
        }
        let epoll_control = if arguments.kind == Kind::EpollCtl {
            let lease = lifetime::LeaseId {
                operation: ExternalOpId::new(owner.thread, call.0),
                mm: owner.mm,
                kind: lifetime::LeaseKind::StreamCall,
                ordinal: 0,
            };
            let ticket = self
                .lifetime
                .prepare_original_selection(
                    task,
                    lease,
                    [arguments.fd, arguments.original_count as u32 as i32],
                )
                .map_err(|error| protocol(&error.to_string()))?;
            Some(epoll_ctl::Control::new(ticket))
        } else {
            None
        };
        self.next_stream_call = next;
        self.stream_calls.insert(
            call,
            StreamCallState {
                owner,
                open_file,
                physical_pin_required: matches!(arguments.kind, Kind::Connect | Kind::Sendto | Kind::BlockingSendto { .. }) && open_file.is_some(),
                phase: StreamCallPhase::PinAcquireSubmitted,
                abandoned: false,
                final_wait: false,
                terminal_evidence: None,
                capture_publication: publication.as_ref().map(|p| p.permit),
                capture_control: controls.first().map(|x| x.1),
                helper_copy: None,
                native_receive: Vec::new(),
                private_receive: None,
                record_no_store: None,
                no_store_completed: false,
                replay_receive: None,
                foreground_store: None,
                private_drain: None,
                native_entry_attempted: None,
                native_entry: None,
                receive_policy: None,
                replay_connect: None,
                shared_attempt: None,
                original: Some(OriginalCallState {
                    arguments: arguments.clone(),
                    source,
                    foreground_close: None,
                    external_grant,
                    socket_mutation: mutation,
                    allocation_published: false,
                    terminal_publication: None,
                    terminal_prior_mutation: None,
                    terminal_recovered_enrollment: None,
                    terminal_reconciled: None,
                    terminal_kernel_result: None,
                    terminal_no_installation: None,
                    pin: None,
                    command: None,
                    provider_submitted: false,
                    selected: None,
                    read_expects_file: None,
                    read_copy: None,
                    epoll_control,
                    backend_result: None,
                    backend_entered: false,
                    final_wait: false,
                    uninvoked: true,
                    provider_retired: false,
                    pin_released: false,
                    consumed: false,
                    cancel_requested: false,
                    cancel_disarmed: false,
                }),
            },
        );
        if read.is_some() {
            self.fd_publications
                .get_mut(&arguments.files)
                .expect("transferred original file reader")
                .reader = None;
        }
        Ok(Admission { call, arguments })
    }
    pub(crate) fn begin_recorded_original_file(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
    ) -> Result<Admission, NetworkReplayError> {
        if !matches!(arguments.kind, Kind::File(_) | Kind::Read) {
            return Err(protocol("recorded file admission changed operation family"));
        }
        if self.mode() != NetworkEngineMode::Replay {
            return Err(protocol(
                "recorded file admission requires the validated Replay engine",
            ));
        }
        self.begin_original_call(
            owner,
            arguments,
            OriginalResultSource::Recorded { selected: false },
        )
    }
    /// Detcore's existing modeled Read retains the same semantic OFD while
    /// guest memory or procfs collection can suspend. No native pin is added.
    pub(crate) fn begin_emulated_read_from_read(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
        read: NetworkFdReadAdmission,
    ) -> Result<Admission, NetworkReplayError> {
        if arguments.kind != Kind::Read || !self.fd_table_capability() {
            return Err(protocol(
                "modeled Read requires admitted Read/table authority",
            ));
        }
        let admission = self.begin_original_call_with_read(
            owner,
            arguments,
            OriginalResultSource::EmulatedRead { selected: false },
            Some(read),
        )?;
        self.select_logical_original_file(owner, &admission, false)?;
        Ok(admission)
    }

    pub(crate) fn finish_emulated_read(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        returned: i64,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_call_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || state.abandoned
            || original.final_wait
            || original.arguments.kind != Kind::Read
            || !matches!(
                original.source,
                OriginalResultSource::EmulatedRead { selected: true }
            )
            || !(returned < 0 && (-4095..=-1).contains(&returned)
                || returned >= 0 && returned as u64 <= original.arguments.original_count)
        {
            return Err(protocol(
                "modeled Read completion changed selected logical Call",
            ));
        }
        self.retire_recorded_original_file(
            owner,
            admission.call,
            lifetime::TransportResolution::CompletedEmulation,
        )
    }

    pub(crate) fn select_recorded_original_file(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        self.select_logical_original_file(owner, admission, true)
    }
    fn select_logical_original_file(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        recorded: bool,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_call_state(owner, admission.call)?;
        let expected = match original.source {
            OriginalResultSource::Recorded { selected: false } => recorded,
            OriginalResultSource::EmulatedRead { selected: false } => !recorded,
            _ => false,
        };
        if original.arguments != admission.arguments
            || state.abandoned
            || original.final_wait
            || !expected
        {
            return Err(protocol(
                "recorded file selection changed logical admission",
            ));
        }
        let permit = state
            .capture_publication
            .ok_or_else(|| protocol("recorded selection lost table publication membership"))?;
        self.validate_original_exclusion_common(owner, admission.call, permit)?;
        self.fd_publications.get_mut(&permit.files).unwrap().active = None;
        let state = self.stream_calls.get_mut(&admission.call).unwrap();
        state.capture_publication = None;
        state.original.as_mut().unwrap().source = if recorded {
            OriginalResultSource::Recorded { selected: true }
        } else {
            OriginalResultSource::EmulatedRead { selected: true }
        };
        if admission.arguments.kind == Kind::Read {
            // The recorded Read keeps its same logical Call/description while
            // the existing Replayer performs guest copies. Neither table nor
            // short status control may span a fault requiring a peer.
            self.release_original_file_control(owner, admission.call)?;
        }
        Ok(())
    }
    pub(crate) fn finish_recorded_original_file(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        recorded: i64,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_call_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || state.abandoned
            || original.final_wait
            || !matches!(
                original.source,
                OriginalResultSource::Recorded { selected: true }
            )
            || !original
                .arguments
                .kind
                .valid_counted_result(recorded, original.arguments.original_count)
        {
            return Err(protocol(
                "recorded Return changed its selected logical call",
            ));
        }
        self.retire_recorded_original_file(
            owner,
            admission.call,
            lifetime::TransportResolution::CompletedAndRecorded,
        )
    }
    pub(crate) fn finish_recorded_read_interruption(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_call_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || state.abandoned
            || original.final_wait
            || original.arguments.kind != Kind::Read
            || !matches!(
                original.source,
                OriginalResultSource::Recorded { selected: true }
            )
        {
            return Err(protocol(
                "recorded Read interruption changed its selected logical call",
            ));
        }
        self.retire_recorded_original_file(
            owner,
            admission.call,
            lifetime::TransportResolution::CancellationAcknowledgedBeforeSubmission,
        )
    }
    fn retire_recorded_original_file(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        resolution: lifetime::TransportResolution,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_call_state(owner, call)?;
        if !matches!(
            original.source,
            OriginalResultSource::Recorded { .. } | OriginalResultSource::EmulatedRead { .. }
        ) || original.command.is_some()
            || original.provider_submitted
            || original.selected.is_some()
            || original.backend_result.is_some()
            || original.pin.is_some()
            || original.provider_retired
            || original.pin_released
        {
            return Err(protocol("recorded cleanup acquired native custody"));
        }
        let (file, control, permit) = (
            state.open_file,
            state.capture_control,
            state.capture_publication,
        );
        if let Some(permit) = permit {
            self.validate_original_exclusion_common(owner, call, permit)?;
        }
        if let Some(permit) = permit {
            self.fd_publications.get_mut(&permit.files).unwrap().active = None;
            self.stream_calls
                .get_mut(&call)
                .unwrap()
                .capture_publication = None;
        }
        if control.is_some() {
            self.release_original_file_control(owner, call)?;
        }
        if let Some(file) = file {
            self.release_registered_stream_call_lifetime(owner, call, file, resolution)?;
        }
        self.stream_calls.remove(&call);
        if let Some(file) = file {
            self.complete_deferred_retirement(file);
        }
        Ok(())
    }
    fn original_connect_state(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(&StreamCallState, &OriginalCallState), NetworkReplayError> {
        let (state, original) = self.original_call_state(owner, call)?;
        if !matches!(original.source, OriginalResultSource::Native) {
            return Err(protocol(
                "recorded Return has no native original-call authority",
            ));
        }
        Ok((state, original))
    }
    fn original_call_state(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(&StreamCallState, &OriginalCallState), NetworkReplayError> {
        let state = self
            .stream_calls
            .get(&call)
            .filter(|s| s.owner == owner)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        let original = state
            .original
            .as_ref()
            .ok_or(NetworkReplayError::StreamCallPhaseMismatch(call))?;
        Ok((state, original))
    }
    /// Resolve the Local marker even when the Begin reply never reached the
    /// Guest. Owner/MM plus the unique operation and original arguments bind
    /// the existing Call; a numeric TID or a missing reply supplies no receipt.
    fn original_connect_local_call(
        &self,
        owner: NetworkStreamOwner,
        local: &Local,
    ) -> Result<Option<NetworkStreamCallId>, NetworkReplayError> {
        let mut found = self.stream_calls.iter().filter_map(|(call, state)| {
            (state.owner == owner
                && state.original.as_ref().is_some_and(|original| {
                    original.arguments.operation == local.arguments.operation
                }))
            .then_some(*call)
        });
        let Some(call) = found.next() else {
            return Ok(None);
        };
        if found.next().is_some() {
            return Err(protocol("original local marker matches multiple calls"));
        }
        let (_, original) = self.original_call_state(owner, call)?;
        if !original.arguments.same_request(&local.arguments)
            || local.admission.as_ref().is_some_and(|admission| {
                admission.call != call
                    || admission.arguments != original.arguments
                    || local.arguments != admission.arguments
            })
            || (!local.invoked
                && (original.selected.is_some() || original.backend_result.is_some()))
        {
            return Err(protocol("original local marker changed actual invocation"));
        }
        Ok(Some(call))
    }
    /// A selected grant can be lost before Call transfer. The actual consumed
    /// Local identifies this still-logical reader; no command was submitted.
    /// Once transfer happened the reader is absent and the Call owns cleanup.
    fn consume_original_queued_read(
        &mut self,
        owner: NetworkStreamOwner,
        local: &Local,
    ) -> Result<(), NetworkReplayError> {
        let Some(read) = self
            .fd_publications
            .get(&local.arguments.files)
            .and_then(|state| state.reader.as_ref())
            .filter(|read| read.publication.permit.owner == owner)
            .cloned()
        else {
            return Ok(());
        };
        if local.admission.is_some()
            || local.invoked
            || local.returned.is_some()
            || !matches!(local.arguments.kind, Kind::Connect | Kind::Close | Kind::BlockingSendto { .. })
            || (matches!(local.arguments.kind, Kind::BlockingSendto { .. }) && read.external_grant.is_some())
            || read.fd != local.arguments.fd
            || read
                .external_grant
                .is_some_and(|operation| operation != local.arguments.operation)
        {
            return Err(protocol(
                "consumed original intent changed its untransferred reader",
            ));
        }
        self.validate_owned_fd_read(owner, &read)?;
        self.consume_logical_fd_read(owner, read);
        Ok(())
    }

    /// Only the actual consuming Tool callback supplies this local marker.
    /// There is no logical-owner-gone or command-READY inference here.
    pub(crate) fn original_connect_consumed(
        &mut self,
        owner: NetworkStreamOwner,
        local: &Local,
    ) -> Result<(), NetworkReplayError> {
        let found = self.original_connect_local_call(owner, local)?;
        // Consumption can precede Begin or follow the Driver's already-proved
        // terminal retirement. Absence is an idempotent no-op for that operation,
        // never authority to release another Call or fabricate a physical result.
        if found.is_none() {
            self.consume_original_queued_read(owner, local)?;
        }
        self.gone_stream_owners.insert(owner);
        let Some(call) = found else {
            return Ok(());
        };
        if matches!(
            self.original_call_state(owner, call)?.1.source,
            OriginalResultSource::Recorded { .. } | OriginalResultSource::EmulatedRead { .. }
        ) {
            // The actual consuming ThreadState proves this delegate can no
            // longer touch the snapshot. No native operation was submitted.
            return self.retire_recorded_original_file(
                owner,
                call,
                lifetime::TransportResolution::CancellationAcknowledgedBeforeSubmission,
            );
        }
        let state = self.stream_calls.get_mut(&call).unwrap();
        state.abandoned = true;
        let original = state.original.as_mut().unwrap();
        original.consumed = true;
        original.cancel_requested = !local.invoked;
        Ok(())
    }
    /// Called only by the backend's synchronous final Wait::Exited observer.
    /// Exact retained local/call identity, not the numeric TID alone, selects
    /// this invocation. It records no native result and releases no Call or
    /// physical custody; an untransferred logical reader can be consumed.
    pub(crate) fn original_connect_final_wait(
        &mut self,
        owner: NetworkStreamOwner,
        local: &Local,
    ) -> Result<bool, NetworkReplayError> {
        let found = self.original_connect_local_call(owner, local)?;
        // Unlike idempotent consuming cleanup, a returned Admission here is an
        // exact live-call assertion. Preserve wrong-owner/wrong-ID refusal.
        if found.is_none() && local.admission.is_some() {
            return Err(protocol("original terminal admission has no matching call"));
        }
        // Final wait can precede Begin admission or its reply. The exact owned
        // Local still fences delayed admission; no state is matched by TID alone.
        if found.is_none() {
            self.consume_original_queued_read(owner, local)?;
        }
        self.gone_stream_owners.insert(owner);
        let Some(call) = found else {
            return Ok(false);
        };
        let state = self.stream_calls.get_mut(&call).unwrap();
        state.abandoned = true;
        let original = state.original.as_mut().unwrap();
        original.final_wait = true;
        if !original.provider_submitted {
            original.cancel_requested = true;
        }
        Ok(true)
    }
    pub(crate) fn original_connect_task_terminal(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<bool, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments {
            return Err(protocol("original terminal query changed invocation"));
        }
        Ok(original.final_wait)
    }
    pub(crate) fn original_close_terminal_publication_pending(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<bool, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments || !original.final_wait {
            return Err(protocol("close terminal fence lacks original final wait"));
        }
        Ok(original.arguments.kind == Kind::Close
            && original.selected.is_none()
            && self.lifetime.table_exists(admission.arguments.files))
    }
    /// The same service has now positively polled its retained PIDFD_THREAD and
    /// read back removal of this exact dead invocation's owned kernel rows.
    pub(crate) fn original_connect_dead_retired(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        command: u64,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.final_wait
            || original.command != Some(command)
            || original.provider_retired
        {
            return Err(protocol(
                "dead original retirement lacks exact final-wait authority",
            ));
        }
        if original.arguments.kind.allocator()
            && !original.allocation_published
            && original.terminal_reconciled.is_none()
            && original.terminal_no_installation.is_none()
            && original.terminal_kernel_result.is_none()
            && (state.capture_publication.is_some() || !original.uninvoked)
        {
            return Err(protocol(
                "terminal Socket retains unresolved installation journal custody",
            ));
        }
        if original.arguments.kind == Kind::Close
            && original.selected.is_none()
            && self.lifetime.table_exists(admission.arguments.files)
        {
            return Err(protocol(
                "unobserved close cannot release a surviving shared table",
            ));
        }
        if original.arguments.kind.allocator() {
            // Physical provider retirement leaves this original allocation and
            // any exact prior publication lease in Call custody for recovery.
        } else if let Some(permit) = state.capture_publication {
            self.release_original_exclusion(owner, admission.call, permit)?;
        } else if matches!(original.arguments.kind, Kind::File(_)) {
            self.release_original_file_control(owner, admission.call)?;
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .provider_retired = true;
        Ok(())
    }
    pub(crate) fn original_connect_cancellation(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(bool, bool), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments {
            return Err(protocol("original cancellation changed admission"));
        }
        Ok((original.cancel_requested, original.consumed))
    }
    pub(crate) fn original_connect_disarmed(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        command: u64,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.cancel_requested
            || original.command != Some(command)
            || original.cancel_disarmed
            || original.selected.is_some()
            || original.backend_result.is_some()
        {
            return Err(protocol(
                "known-uninvoked disarm changed retained invocation",
            ));
        }
        if admission.arguments.kind == Kind::EpollCtl {
            if state.capture_publication.is_some()
                || state.capture_control.is_some()
                || original.epoll_control.is_none()
            {
                return Err(protocol(
                    "epoll disarm changed its no-exclusion reservation",
                ));
            }
            // Disarm is not the later exact transport ACK or physical release.
            self.stream_calls
                .get_mut(&admission.call)
                .unwrap()
                .original
                .as_mut()
                .unwrap()
                .cancel_disarmed = true;
            return Ok(());
        }
        if admission.arguments.kind.allocator()
            && state.capture_publication.is_none()
            && original.socket_mutation.is_none()
        {
            self.stream_calls
                .get_mut(&admission.call)
                .unwrap()
                .original
                .as_mut()
                .unwrap()
                .cancel_disarmed = true;
            return Ok(());
        }
        let permit = state
            .capture_publication
            .ok_or_else(|| protocol("uninvoked cancellation lost exclusion"))?;
        if admission.arguments.kind == Kind::Socket {
            self.discard_uninvoked_socket_mutation(owner, permit)?;
        }
        self.release_original_exclusion(owner, admission.call, permit)?;
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .cancel_disarmed = true;
        Ok(())
    }
    pub(crate) fn original_connect_cancel_retired(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.cancel_disarmed
            || original.provider_retired
        {
            return Err(protocol(
                "original canceled transport has no positive disarm",
            ));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .provider_retired = true;
        Ok(())
    }
    pub(crate) fn original_connect_provider_submitted(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if state.abandoned
            || original.arguments != admission.arguments
            || original.provider_submitted
            || original.command.is_some()
            || original.backend_result.is_some()
        {
            return Err(protocol("original provider preparation cannot be reissued"));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .provider_submitted = true;
        Ok(())
    }
    /// Only the capture worker can settle this pre-provider failure, after a
    /// known failed acquisition or the actual close of its captured pin.
    pub(crate) fn abort_original_before_provider(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.provider_submitted
            || original.command.is_some()
            || original.selected.is_some()
            || original.backend_result.is_some()
        {
            return Err(protocol(
                "original call already may own a native invocation",
            ));
        }
        if admission.arguments.kind == Kind::EpollCtl {
            if state.capture_publication.is_some()
                || state.capture_control.is_some()
                || state.open_file.is_some()
            {
                return Err(protocol(
                    "epoll pre-provider abort changed no-exclusion ownership",
                ));
            }
            let ticket = original
                .epoll_control
                .as_ref()
                .ok_or_else(|| protocol("epoll control lost its selection reservation"))?
                .ticket;
            let retired = self
                .lifetime
                .finish_original_selection(
                    ticket,
                    lifetime::TransportResolution::CancellationAcknowledgedBeforeSubmission,
                )
                .map_err(|error| protocol(&error.to_string()))?;
            self.retire_lifetime_open_files(retired);
            self.stream_calls.remove(&admission.call);
            return Ok(());
        }
        if admission.arguments.kind.allocator()
            && state.capture_publication.is_none()
            && state.open_file.is_none()
            && original.socket_mutation.is_none()
        {
            self.stream_calls.remove(&admission.call);
            return Ok(());
        }
        let (file, permit) = (
            state.open_file,
            state
                .capture_publication
                .ok_or_else(|| protocol("original pre-provider permit missing"))?,
        );
        if admission.arguments.kind == Kind::Socket {
            self.discard_uninvoked_socket_mutation(owner, permit)?;
        }
        self.release_original_exclusion(owner, admission.call, permit)?;
        if let Some(file) = file {
            self.release_stream_call_lifetime(owner, admission.call, file)?;
        }
        self.stream_calls.remove(&admission.call);
        if let Some(file) = file {
            self.complete_deferred_retirement(file);
        }
        Ok(())
    }
    /// Capture completion transfers custody to this phase; it does not invoke
    /// the ordinary stream confirmation which would release the table permit.
    #[cfg(test)]
    pub(crate) fn original_connect_prepared(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        pin: Pin,
        command: u64,
    ) -> Result<(), NetworkReplayError> {
        self.original_call_prepared(owner, admission, Some(pin), command)
    }
    pub(crate) fn original_call_prepared(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        pin: Option<Pin>,
        command: u64,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.pin.is_some()
            || original.command.is_some()
            || command == 0
            || !original.provider_submitted
            || (state.capture_publication.is_none()
                && !admission.arguments.kind.allocator()
                && !(admission.arguments.kind == Kind::EpollCtl
                    && original.epoll_control.is_some()))
            || match admission.arguments.kind {
                Kind::Connect => {
                    pin.is_none() || (pin == Some(Pin::Empty)) != state.open_file.is_none()
                }
                Kind::Sendto => !matches!(pin, Some(Pin::Socket {
                    domain: libc::AF_INET | libc::AF_INET6,
                    kind: libc::SOCK_STREAM, protocol: libc::IPPROTO_TCP,
                })) || state.open_file.is_none(),
                Kind::BlockingSendto { .. } => !matches!(pin, Some(Pin::Socket {
                    domain: libc::AF_INET, kind: libc::SOCK_STREAM, protocol: libc::IPPROTO_TCP,
                })) || state.open_file.is_none(),
                Kind::Close
                | Kind::File(_)
                | Kind::Read
                | Kind::Socket
                | Kind::Openat
                | Kind::EpollCreate { .. }
                | Kind::EpollCtl => pin.is_some(),
            }
        {
            return Err(protocol("original preparation changed capture/admission"));
        }
        let original = self
            .stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap();
        original.pin = pin;
        original.command = Some(command);
        Ok(())
    }
    /// Admission to invoke. The local known-uninvoked marker remains installed
    /// until immediately before guest.inject, covering a lost Submit reply.
    pub(crate) fn original_connect_invoked(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if state.abandoned
            || original.arguments != admission.arguments
            || !original.uninvoked
            || original.command.is_none()
            || (matches!(original.arguments.kind, Kind::Connect | Kind::Sendto | Kind::BlockingSendto { .. }) && original.pin.is_none())
        {
            return Err(protocol(
                "original invocation lacks its prepared exact admission",
            ));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .uninvoked = false;
        Ok(())
    }
    /// Optional native ENTRY observation for an exact non-Read invocation.
    /// It records only the boundary: selection, result, provider retirement,
    /// publication and scheduler ownership still require their original joins.
    /// Read has a separate required transition and must not use this one.
    pub(crate) fn original_non_read_entered(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if state.abandoned
            || original.arguments != admission.arguments
            || original.arguments.kind == Kind::Read
            || original.uninvoked
            || original.command.is_none()
            || original.backend_entered
            || original.backend_result.is_some()
            || original.final_wait
            || original.provider_retired
            || original.pin_released
            || original.consumed
            || original.cancel_requested
            || original.cancel_disarmed
        {
            return Err(protocol("non-Read entry changed its prepared invocation"));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .backend_entered = true;
        Ok(())
    }

    /// Synchronous backend ENTRY observation for the same prepared Read.
    /// It supplies neither fdget selection nor a result.
    pub(crate) fn original_read_entered(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.arguments.kind != Kind::Read
            || original.uninvoked
            || original.read_expects_file.is_none()
            || original.backend_entered
            || original.backend_result.is_some()
            || original.cancel_requested
        {
            return Err(protocol("Read entry changed its prepared invocation"));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .backend_entered = true;
        Ok(())
    }

    /// Only the actual backend-owned first-resume signal stop supplies this
    /// observation. Missing selection, READY and an errno supply no authority.
    pub(crate) fn original_read_interrupted_before_entry(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.arguments.kind != Kind::Read
            || original.uninvoked
            || original.read_expects_file.is_none()
            || original.backend_entered
            || original.selected.is_some()
            || original.backend_result.is_some()
            || original.cancel_requested
            || original.consumed
            || state.abandoned
            || state.capture_publication.is_none()
        {
            return Err(protocol(
                "Read interruption has no exact unentered invocation",
            ));
        }
        let original = self
            .stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap();
        original.uninvoked = true;
        original.cancel_requested = true;
        Ok(())
    }

    pub(crate) fn validate_original_read_interrupted(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.arguments.kind != Kind::Read
            || !original.uninvoked
            || !original.cancel_requested
            || original.backend_entered
            || original.selected.is_some()
            || original.backend_result.is_some()
            || original.consumed
            || original.final_wait
        {
            return Err(protocol(
                "Read cancellation lacks actual live pre-entry observation",
            ));
        }
        Ok(())
    }

    /// Retain the actual raw store before the Driver interprets its first
    /// reply. Native command/Call custody is independent of syscall completion.
    pub(crate) fn bind_original_read_copy(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        command: u64,
        custody: &std::sync::Arc<crate::network_runtime::original_read_copy::ReadCopyCustody>,
    ) -> Result<usize, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.arguments.kind != Kind::Read
            || original.command != Some(command)
            || !original.provider_submitted
            || original.provider_retired
            || original.cancel_disarmed
            || original.selected.is_none() && !original.final_wait
        {
            return Err(protocol(
                "Read receipt custody lacks the exact selected or terminal Call",
            ));
        }
        if let Some((held, received)) = &original.read_copy {
            if !std::sync::Arc::ptr_eq(held, custody) {
                return Err(protocol(
                    "Read receipt custody changed its retained raw store",
                ));
            }
            return Ok(*received);
        }
        custody
            .bind(owner, admission.call, command)
            .map_err(|error| protocol(&error.to_string()))?;
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .read_copy = Some((custody.clone(), 0));
        Ok(0)
    }

    /// Retain a newly completed range even if a later raw suffix was refused.
    /// Copy5 also joins authenticated physical byte/order history. Guest
    /// memory, queue consumption and release remain separate obligations.
    pub(crate) fn retain_original_read_copy(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        delta: crate::network_runtime::original_read_copy::CompletedDelta,
    ) -> Result<usize, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        let selected = delta.selection();
        let tuple = (
            selected.provider,
            selected.task,
            selected.task_start,
            selected.table,
            selected.file,
        );
        if original.arguments != admission.arguments
            || original.arguments.kind != Kind::Read
            || original.command != Some(selected.command)
            || selected.call != admission.call.native_command_call()
            || selected.owner_mm != owner.mm.generation()
            || selected.requested_fd != admission.arguments.fd
            || selected.user_address != admission.arguments.address
            || selected.address_length != admission.arguments.length
            || selected.original_count != admission.arguments.original_count
            || original.selected.is_some_and(|expected| expected != tuple)
            || original.selected.is_none() && !original.final_wait
        {
            return Err(protocol(
                "Read completed range changed its original selected Call",
            ));
        }
        let (custody, received) = original
            .read_copy
            .as_ref()
            .ok_or_else(|| protocol("Read completed range has no retained raw owner"))?;
        let next = delta
            .next(custody, *received)
            .map_err(|error| protocol(&error.to_string()))?;
        // Opaque observations were extracted under custody before this engine
        // lock. Physical history is distinct from unresolved semantic receipts.
        self.retain_native_receive_attempts(owner, admission.call, delta.native_attempts())?;
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .read_copy
            .as_mut()
            .unwrap()
            .1 = next;
        Ok(next)
    }

    /// A completed copy5 handle cannot acquire Call authority by repeating its
    /// numeric selection. Its same raw owner must already belong to this Read.
    pub(super) fn original_receive_attempt_file(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        attempt: &crate::network_runtime::original_read_copy::NativeAttempt,
    ) -> Result<OpenFileId, NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, call)?;
        let selected = attempt.selection();
        let (custody, _) = original
            .read_copy
            .as_ref()
            .ok_or_else(|| protocol("native receive has no bound original raw owner"))?;
        if original.arguments.kind != Kind::Read
            || original.uninvoked
            || attempt.operation() != 11
            || original.command != Some(selected.command)
            || !attempt.belongs_to(owner, call, custody)
            || original.selected
                != Some((
                    selected.provider,
                    selected.task,
                    selected.task_start,
                    selected.table,
                    selected.file,
                ))
                && !original.final_wait
        {
            return Err(protocol(
                "native receive is not the exact original selected Read",
            ));
        }
        state
            .open_file
            .ok_or_else(|| protocol("native receive original lacks its admitted OFD"))
    }

    /// The run-owned driver calls this while guest uaccess may be blocked.
    /// Same-slot equality is sufficient here only because capture and this
    /// original selection share uninterrupted complete table authority; a
    /// historical enrollment or later FD lookup never substitutes for that cut.
    pub(crate) fn original_connect_selected(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        command: u64,
        selected: (u64, u64, u64, u64, u64),
    ) -> Result<(), NetworkReplayError> {
        let (provider, task, start, table, file) = selected;
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.command != Some(command)
            || original.uninvoked
            || provider == 0
            || task == 0
            || start == 0
            || table == 0
        {
            return Err(protocol(
                "original selection is not this physical invocation",
            ));
        }
        // Pin classification is actual native work, not the guest's declared
        // socket family or a semantic type guessed from this numeric FD.
        match original.arguments.kind {
            Kind::EpollCtl => {
                return Err(protocol(
                    "epoll control requires its paired historical selection join",
                ));
            }
            Kind::Connect | Kind::Sendto | Kind::BlockingSendto { .. } => match original.pin {
                Some(Pin::Empty | Pin::Path) if file == 0 => {}
                Some(Pin::Other | Pin::Socket { .. }) if file != 0 => {}
                _ => {
                    return Err(protocol(
                        "original selection differs from retained physical pin",
                    ));
                }
            },
            Kind::Close | Kind::File(_)
                if original.pin.is_none()
                    && original.arguments.binding.is_some() == (file != 0) => {}
            Kind::Close | Kind::File(_) => {
                return Err(protocol(
                    "file selection differs from admitted original slot",
                ));
            }
            Kind::Read
                if original.pin.is_none() && original.read_expects_file == Some(file != 0) => {}
            Kind::Read => {
                return Err(protocol(
                    "read selection differs from its actual Prepared metadata",
                ));
            }
            Kind::Socket | Kind::Openat | Kind::EpollCreate { .. }
                if original.pin.is_none() && original.arguments.binding.is_none() => {}
            Kind::Socket | Kind::Openat | Kind::EpollCreate { .. } => {
                return Err(protocol(
                    "allocator installation acquired a source descriptor",
                ));
            }
        }
        if let Some(prior) = original.selected {
            return if prior == selected {
                Ok(())
            } else {
                Err(protocol("original selection changed after publication"))
            };
        }
        if original.arguments.kind.allocator() && state.capture_publication.is_none() {
            // fd_install is an immutable historical fact. It does not acquire
            // publication or admit metadata while the original syscall waits.
            self.stream_calls
                .get_mut(&admission.call)
                .unwrap()
                .original
                .as_mut()
                .unwrap()
                .selected = Some(selected);
            return Ok(());
        }
        let permit = state
            .capture_publication
            .ok_or_else(|| protocol("original selection lost table authority"))?;
        if original.arguments.kind == Kind::Close {
            return Err(protocol(
                "close selection requires the retained metadata publication join",
            ));
        }
        if original.arguments.kind == Kind::Socket {
            // The original installation still needs its complete interval and
            // raw return. This Socket allocator's existing permit transfers
            // to the shared publication consumer, never a second table gate.
            self.validate_original_exclusion(owner, admission.call, permit)?;
        } else if matches!(original.arguments.kind, Kind::File(_)) {
            // F_GETFL has selected its kernel-held file. Release the table so
            // unrelated descriptor operations can proceed. Its option control
            // remains until the real result, keeping the virtual flag snapshot
            // ordered with F_SETFL on an alias; no guest memory is touched.
            self.validate_original_exclusion(owner, admission.call, permit)?;
            self.fd_publications.get_mut(&permit.files).unwrap().active = None;
            self.stream_calls
                .get_mut(&admission.call)
                .unwrap()
                .capture_publication = None;
        } else {
            self.release_original_exclusion(owner, admission.call, permit)?;
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .selected = Some(selected);
        self.finish_original_file_selection(owner, admission.call)
    }
    /// The same Driver holds the real local metadata mutex before taking this
    /// engine lock. Both populations are validated before either is changed;
    /// no await or syscall occurs in this commit. Host arrival does not choose
    /// membership: the exact Call/table/slot was admitted before native entry.
    pub(crate) fn publish_original_close_selection(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        selected: &crate::network_runtime::OriginalSelection,
        metadata: &mut crate::tool_local::FileMetadata,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        let physical = (
            selected.provider,
            selected.task,
            selected.task_start,
            selected.table,
            selected.file,
        );
        if admission.arguments.kind != Kind::Close
            || original.arguments != admission.arguments
            || original.command != Some(selected.command)
            || original.uninvoked
            || original.pin.is_some()
            || selected.provider == 0
            || selected.task == 0
            || selected.task_start == 0
            || selected.table == 0
            || selected.ready != 1
            || selected.call != admission.call.native_command_call()
            || selected.owner_mm != owner.mm.generation()
            || selected.requested_fd != admission.arguments.fd
            || selected.user_address != 0
            || selected.address_length != 0
            || selected.fdput_flags != 0
            || admission.arguments.binding.is_some() != (selected.file != 0)
        {
            return Err(protocol(
                "close selection is not the admitted physical removal",
            ));
        }
        if let Some(prior) = original.selected {
            // A repeated response must not erase a newer same-FD installation.
            return if prior == physical {
                Ok(())
            } else {
                Err(protocol("close selection changed after publication"))
            };
        }
        let permit = state
            .capture_publication
            .ok_or_else(|| protocol("close selection lost its pending publication"))?;
        self.validate_original_exclusion(owner, admission.call, permit)?;
        if metadata.files_id != permit.files
            || metadata.descriptor_binding(admission.arguments.fd).ok()
                != admission.arguments.binding
        {
            return Err(protocol(
                "close publication changed the original shared local slot",
            ));
        }
        let binding = admission.arguments.binding;
        let table_live = self.lifetime.table_exists(permit.files);
        if !table_live && !original.final_wait {
            return Err(protocol(
                "close lost its lifetime table before actual owner final wait",
            ));
        }
        if let Some(binding) = binding
            && table_live
            && self
                .lifetime
                .binding_in_retained_table(binding.slot.files, binding.slot.fd)
                != Some(binding)
        {
            return Err(protocol(
                "close publication changed the original lifetime slot",
            ));
        }
        // Preserve the private original binding before its numeric slot goes
        // away; unresolved post-copy fdgets may already own that same file.
        if let Some(binding) = binding
            && let Some(identity) = metadata.native_binding_identity(binding)
        {
            if !identity.matches(selected.provider, selected.file) {
                return Err(protocol("close selected another authenticated native file"));
            }
            self.note_epoll_native_binding(binding, identity)?;
        }
        // The Call's existing semantic lease outlives this slot. It is released
        // only by the distinct final command/physical-custody retirement path.
        if let Some(binding) = binding {
            if table_live {
                let retired = self
                    .lifetime
                    .close_retained_binding(binding)
                    .map_err(|e| protocol(&e.to_string()))?;
                self.fd_lifecycle.retired_ports.extend(retired);
            }
            assert!(
                metadata.remove_descriptor_binding(binding),
                "validated original close metadata changed under its mutex"
            );
        }
        // Validation above covers every release precondition. No unrelated
        // field changed and both locks are still held, so this cannot become a
        // partially applied recoverable error.
        self.release_original_exclusion(owner, admission.call, permit)
            .expect("validated close exclusion changed without an await");
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .selected = Some(physical);
        Ok(())
    }
    // Positive selection may arrive after logical owner withdrawal. Validate
    // exact retained custody without manufacturing a live current task/MM.
    fn validate_original_exclusion(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        self.original_connect_state(owner, call)?;
        self.validate_original_exclusion_common(owner, call, permit)
    }
    fn validate_original_exclusion_common(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        let state = &self.stream_calls[&call];
        if state.owner != owner
            || state.capture_publication != Some(permit)
            || permit.owner != owner
            || self
                .fd_publications
                .get(&permit.files)
                .is_none_or(|p| p.active != Some(permit) || p.pending.is_some())
        {
            return Err(protocol(
                "original exclusion no longer owns its exact empty publication",
            ));
        }
        if let Some(lease) = state.capture_control {
            let file = state
                .open_file
                .ok_or_else(|| protocol("empty invocation has OFD control"))?;
            if self.socket_controls.get(&file).is_none_or(|c| {
                c.owner != owner || c.lease != lease || !c.physical.can_release_unchanged()
            }) {
                return Err(protocol("original exclusion changed its short OFD control"));
            }
        }
        Ok(())
    }
    fn release_original_exclusion(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        self.validate_original_exclusion(owner, call, permit)?;
        let state = &self.stream_calls[&call];
        let (file, control) = (state.open_file, state.capture_control);
        self.fd_publications.get_mut(&permit.files).unwrap().active = None;
        if control.is_some() {
            self.socket_controls.remove(&file.unwrap());
        }
        let state = self.stream_calls.get_mut(&call).unwrap();
        state.capture_publication = None;
        state.capture_control = None;
        Ok(())
    }
    /// Only the synchronous backend raw-return hook may call this. Synthetic
    /// EINTR/ERESTARTSYS from an interrupted injection has no such observation.
    pub(crate) fn original_connect_returned(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        returned: i64,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.uninvoked
            || original.final_wait
            || !original
                .arguments
                .kind
                .valid_counted_result(returned, original.arguments.original_count)
            || original
                .backend_result
                .is_some_and(|prior| prior != returned)
        {
            return Err(protocol(
                "original backend return changed invocation/result",
            ));
        }
        self.observe_foreground_epoll_return(owner, admission)?;
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .backend_result = Some(returned);
        self.finish_original_file_selection(owner, admission.call)
    }
    /// The actual synchronous Prepared callback supplies this metadata under
    /// the already-admitted table. It is not physical selection or completion.
    pub(crate) fn original_read_metadata_prepared(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        observed: &FileMetadataObservation,
    ) -> Result<(), NetworkReplayError> {
        self.validate_original_file_prepared(owner, admission)?;
        if admission.arguments.kind != Kind::Read
            || observed.admission != *admission
            || observed.status_flags.is_some() != admission.arguments.binding.is_some()
        {
            return Err(protocol(
                "read preparation changed actual metadata presence or admission",
            ));
        }
        let expects_file = observed
            .status_flags
            .is_some_and(|flags| flags & libc::O_PATH == 0);
        let original = self
            .stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap();
        if original
            .read_expects_file
            .is_some_and(|prior| prior != expects_file)
        {
            return Err(protocol("read Prepared metadata changed after observation"));
        }
        original.read_expects_file = Some(expects_file);
        Ok(())
    }
    pub(crate) fn validate_original_file_prepared(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.uninvoked
            || original.command.is_none()
            || original.selected.is_some()
            || original.backend_result.is_some()
            || original.final_wait
            || !matches!(original.arguments.kind, Kind::File(_) | Kind::Read)
            || original.pin.is_some()
        {
            return Err(protocol(
                "file metadata lacks its actual prepared invocation",
            ));
        }
        let permit = state
            .capture_publication
            .ok_or_else(|| protocol("file metadata lost its publication authority"))?;
        self.validate_original_exclusion(owner, admission.call, permit)
    }
    fn release_original_file_control(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_call_state(owner, call)?;
        if !matches!(original.arguments.kind, Kind::File(_) | Kind::Read)
            || state.capture_publication.is_some()
        {
            return Err(protocol("file control release changed selection authority"));
        }
        let Some(lease) = state.capture_control else {
            return Ok(());
        };
        let file = state
            .open_file
            .ok_or_else(|| protocol("empty selected file owns control"))?;
        if self.socket_controls.get(&file).is_none_or(|control| {
            control.owner != owner
                || control.lease != lease
                || !control.physical.can_release_unchanged()
        }) {
            return Err(protocol(
                "selected file control changed before actual completion",
            ));
        }
        self.socket_controls.remove(&file);
        self.stream_calls.get_mut(&call).unwrap().capture_control = None;
        Ok(())
    }
    fn finish_original_file_selection(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, call)?;
        if matches!(original.arguments.kind, Kind::File(_))
            && original.selected.is_some()
            && original.backend_result.is_some()
        {
            self.release_original_file_control(owner, call)?;
        }
        Ok(())
    }
    pub(crate) fn original_connect_result(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<Option<i64>, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments {
            return Err(protocol("original result changed admission"));
        }
        Ok(original.backend_result)
    }
    /// Finite transport retirement and physical close are separate from native
    /// return. Neither dropping the callback nor observing table release suffices.
    pub(crate) fn original_connect_provider_retired(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        returned: i64,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.selected.is_none()
            || original.backend_result != Some(returned)
            || original.provider_retired
        {
            return Err(protocol(
                "original provider retirement lacks exact native completion",
            ));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .provider_retired = true;
        Ok(())
    }
    pub(crate) fn original_connect_pin_released(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.provider_retired
            || original.pin_released
        {
            return Err(protocol("original pin release changed completion custody"));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .pin_released = true;
        Ok(())
    }
    pub(crate) fn original_connect_close_confirmed(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<bool, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments {
            return Err(protocol("original close query changed admission"));
        }
        Ok(original.provider_retired && original.pin_released)
    }
    pub(super) fn completed_original_allocator(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(Kind, i64), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.arguments.kind.allocator()
            || state.abandoned
            || original.final_wait
            || original.uninvoked
            || !original.provider_retired
            || !original.pin_released
            || original.selected.is_none()
            || original.allocation_published
        {
            return Err(protocol(
                "allocator publication lacks original completed Call",
            ));
        }
        let raw = original
            .backend_result
            .ok_or_else(|| protocol("allocator lost original native result"))?;
        Ok((original.arguments.kind, raw))
    }
    pub(crate) fn original_allocator_publication_admission(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<NetworkFdMutationAdmission, NetworkReplayError> {
        let (kind, raw) = self.completed_original_allocator(owner, admission)?;
        if let Some(mutation) = self
            .original_connect_state(owner, admission.call)?
            .1
            .socket_mutation
            .as_ref()
        {
            self.validate_original_socket_publication(owner, mutation, raw)?;
            return Ok(mutation.clone());
        }
        let mutation =
            self.admit_completed_original_allocation(owner, admission.arguments.files, kind, raw)?;
        let state = self.stream_calls.get_mut(&admission.call).unwrap();
        assert!(state.capture_publication.is_none());
        state.capture_publication = Some(mutation.publication.permit);
        state.original.as_mut().unwrap().socket_mutation = Some(mutation.clone());
        Ok(mutation)
    }

    pub(crate) fn original_allocator_publication(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(NetworkFdMutationAdmission, i64), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.arguments.kind.allocator()
            || !original.provider_retired
            || !original.pin_released
            || original.selected.is_none()
            || original.uninvoked
            || (original.final_wait && original.terminal_publication.is_none())
        {
            return Err(protocol(
                "Socket publication lacks exact completed original Call",
            ));
        }
        let permit = state
            .capture_publication
            .ok_or_else(|| protocol("Socket lost allocator permit"))?;
        let returned = original
            .backend_result
            .or(original.terminal_kernel_result)
            .ok_or_else(|| protocol("Socket lost its actual original result"))?;
        let mutation = original
            .socket_mutation
            .as_ref()
            .ok_or_else(|| protocol("Socket lost its original mutation admission"))?;
        if mutation.publication.permit != permit {
            return Err(protocol(
                "Socket publication changed original allocator permit",
            ));
        }
        if mutation.publication.recovery.is_some() && original.terminal_publication.is_some() {
            self.validate_terminal_allocator_recovery(mutation)?;
        } else {
            self.validate_original_socket_publication(
                mutation.publication.permit.owner,
                mutation,
                returned,
            )?;
        }
        Ok((mutation.clone(), returned))
    }

    /// Resume only this completed original Call's retained result. Generic
    /// mutation result submission continues to reject every duplicate.
    pub(crate) fn confirm_original_allocator_publication_result(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (mutation, returned) = self.original_allocator_publication(owner, admission)?;
        self.retain_original_socket_publication_result(
            mutation.publication.permit.owner,
            &mutation,
            returned,
        )
    }

    pub(super) fn validate_original_allocator_installation(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        permit: super::NetworkFdPublicationPermit,
        receipt: &crate::network_runtime::original_installation::Installation,
    ) -> Result<&Arguments, NetworkReplayError> {
        let (state, original) = self.original_connect_state(receipt.original_owner(), call)?;
        let source = match original.arguments.kind {
            Kind::Socket => crate::network_runtime::original_installation::Source::Socket(call),
            Kind::Openat => crate::network_runtime::original_installation::Source::Openat(call),
            Kind::EpollCreate { .. } => {
                crate::network_runtime::original_installation::Source::EpollCreate(call)
            }
            _ => return Err(protocol("non-allocator requested an installation receipt")),
        };
        if receipt.source() != source
            || state.capture_publication != Some(permit)
            || permit.owner != owner
            || (receipt.original_owner() != owner && original.terminal_publication != Some(owner))
            || original
                .terminal_publication
                .is_some_and(|publisher| publisher != owner)
            || !original.provider_retired
            || !original.pin_released
            || original.uninvoked
            || original.backend_result.or(original.terminal_kernel_result)
                != Some(i64::from(receipt.fd()))
            || original
                .command
                .zip(original.selected)
                .is_none_or(|(command, selected)| !receipt.matches_original(command, selected))
        {
            return Err(protocol(
                "Socket install does not belong to its retained original Call",
            ));
        }
        Ok(&original.arguments)
    }

    pub(crate) fn original_allocator_publication_finished(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        permit: super::NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.arguments.kind.allocator()
            || state.capture_publication != Some(permit)
            || !original.provider_retired
            || self
                .fd_publications
                .get(&permit.files)
                .is_some_and(|p| p.active == Some(permit))
        {
            return Err(protocol(
                "Socket cannot release Call custody before exact publication ACK",
            ));
        }
        let state = self.stream_calls.get_mut(&admission.call).unwrap();
        state.capture_publication = None;
        state.original.as_mut().unwrap().allocation_published = true;
        Ok(())
    }

    pub(crate) fn original_socket_publication(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(NetworkFdMutationAdmission, i64), NetworkReplayError> {
        if admission.arguments.kind != Kind::Socket {
            return Err(protocol("Socket wrapper changed family"));
        }
        self.original_allocator_publication(owner, admission)
    }
    pub(crate) fn confirm_original_socket_publication_result(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        if admission.arguments.kind != Kind::Socket {
            return Err(protocol("Socket wrapper changed family"));
        }
        self.confirm_original_allocator_publication_result(owner, admission)
    }
    pub(super) fn validate_original_socket_installation(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        permit: super::NetworkFdPublicationPermit,
        receipt: &crate::network_runtime::original_installation::Installation,
    ) -> Result<&Arguments, NetworkReplayError> {
        let arguments =
            self.validate_original_allocator_installation(owner, call, permit, receipt)?;
        if arguments.kind != Kind::Socket {
            return Err(protocol("Socket wrapper changed family"));
        }
        Ok(arguments)
    }
    pub(crate) fn original_socket_publication_finished(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        permit: super::NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        if admission.arguments.kind != Kind::Socket {
            return Err(protocol("Socket wrapper changed family"));
        }
        self.original_allocator_publication_finished(owner, admission, permit)
    }

    /// A final wait is not a syscall result. This separate field accepts only
    /// the retained provider's complete sys_exit receipt and keeps a missing
    /// backend Returned callback missing; it never fabricates guest completion.
    pub(crate) fn original_terminal_allocator_completed(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        result: &crate::network_runtime::OriginalResult,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        let selected = &result.selection;
        let raw = i64::from(result.returned);
        if original.arguments != admission.arguments
            || !original.arguments.kind.allocator()
            || !original.final_wait
            || original.uninvoked
            || original.provider_retired
            || original.command != Some(selected.command)
            || selected.call != admission.call.native_command_call()
            || selected.owner_mm != owner.mm.generation()
            || selected.requested_fd != admission.arguments.fd
            || selected.user_address != admission.arguments.address
            || selected.address_length != admission.arguments.length
            || selected.original_count != admission.arguments.original_count
            || selected.ready != 1
            || original.backend_result.is_some_and(|prior| prior != raw)
            || original
                .terminal_kernel_result
                .is_some_and(|prior| prior != raw)
        {
            return Err(protocol(
                "terminal allocator changed its actual sys_exit receipt",
            ));
        }
        crate::network_runtime::original_installation::allocator_result(
            result,
            raw,
            admission.arguments.kind,
            admission.arguments.original_count,
        )
        .map_err(|error| protocol(&error.to_string()))?;
        self.original_connect_selected(
            owner,
            admission,
            selected.command,
            (
                selected.provider,
                selected.task,
                selected.task_start,
                selected.table,
                selected.file,
            ),
        )?;
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .terminal_kernel_result = Some(raw);
        Ok(())
    }

    pub(crate) fn terminal_allocator_pending(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<bool, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments {
            return Err(protocol("terminal allocator changed Call"));
        }
        Ok(original.arguments.kind.allocator()
            && !original.uninvoked
            && !original.allocation_published
            && original.terminal_reconciled.is_none()
            && original.terminal_no_installation.is_none())
    }

    pub(crate) fn reconcile_terminal_failed_allocator(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        receipt: &crate::network_runtime::original_installation::NoInstallation,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.arguments.kind.allocator()
            || !(original.consumed || original.final_wait)
            || !original.provider_retired
            || !original.pin_released
            || original.uninvoked
            || original.allocation_published
            || original.terminal_reconciled.is_some()
            || original.terminal_no_installation.is_some()
            || state.capture_publication
                != original
                    .socket_mutation
                    .as_ref()
                    .map(|m| m.publication.permit)
            || original.terminal_publication.is_some()
            || original.backend_result.or(original.terminal_kernel_result)
                != Some(receipt.returned())
            || original.command != Some(receipt.command())
            || original.selected.is_none_or(|selected| selected.4 != 0)
        {
            return Err(protocol(
                "terminal failed allocator lacks its actual complete no-install result",
            ));
        }
        receipt
            .validate(owner, admission)
            .map_err(|error| protocol(&error.to_string()))?;
        let prior = original.socket_mutation.clone();
        if let Some(prior) = &prior {
            self.retire_terminal_failed_allocator_mutation(prior, receipt.returned())?;
        }
        let state = self.stream_calls.get_mut(&admission.call).unwrap();
        state.capture_publication = None;
        let original = state.original.as_mut().unwrap();
        original.terminal_prior_mutation = prior;
        original.terminal_no_installation = Some(receipt.clone());
        Ok(())
    }

    /// Authenticate the completed original effect after its actual consumer
    /// was consumed/finally waited. Physical proof and the actual metadata Arc
    /// survive independently of current task registration.
    fn terminal_original_allocator_effect(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        receipt: &crate::network_runtime::original_installation::Installation,
        actual: &std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
        metadata: &crate::tool_local::FileMetadata,
    ) -> Result<i64, NetworkReplayError> {
        use crate::network_runtime::original_installation::Source;
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        let source = match admission.arguments.kind {
            Kind::Socket => Source::Socket(admission.call),
            Kind::Openat => Source::Openat(admission.call),
            Kind::EpollCreate { .. } => Source::EpollCreate(admission.call),
            _ => {
                return Err(protocol(
                    "terminal reconciliation requires an original allocator",
                ));
            }
        };
        if original.arguments != admission.arguments
            || !(original.consumed || original.final_wait)
            || !original.provider_retired
            || !original.pin_released
            || original.uninvoked
            || original.allocation_published
            || original.terminal_reconciled.is_some()
            || receipt.original_owner() != owner
            || receipt.files() != admission.arguments.files
            || receipt.source() != source
            || original.backend_result.or(original.terminal_kernel_result)
                != Some(i64::from(receipt.fd()))
            || original
                .command
                .zip(original.selected)
                .is_none_or(|(command, selected)| !receipt.matches_original(command, selected))
        {
            return Err(protocol(
                "terminal allocator lacks exact completed original effect",
            ));
        }
        receipt
            .validate_terminal(owner, actual, metadata)
            .map_err(|e| protocol(&e.to_string()))?;
        Ok(i64::from(receipt.fd()))
    }

    /// Return a real surviving owner, ordered only to make diagnostics stable.
    /// The returned identity is revalidated with its actual metadata before any
    /// permit is acquired; it grants no current-task authority to the dead owner.
    pub(crate) fn terminal_allocator_successor(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<Option<NetworkStreamOwner>, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.arguments.kind.allocator()
            || !(original.consumed || original.final_wait)
        {
            return Err(protocol(
                "allocator successor query lacks terminal Call custody",
            ));
        }
        Ok(self
            .lifetime
            .publication_successors(admission.arguments.files)
            .into_iter()
            .map(|task| NetworkStreamOwner {
                thread: task.tid,
                mm: task.mm,
            })
            .find(|owner| !self.gone_stream_owners.contains(owner)))
    }

    /// Reuse the sole FD mutation/publisher with a current owner of the original
    /// shared table. No syscall is reissued and no result is attributed to that
    /// successor. The original Call retains the effect and publication receipt.
    pub(crate) fn begin_terminal_allocator_publication(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        publisher: NetworkStreamOwner,
        receipt: &crate::network_runtime::original_installation::Installation,
        actual: &std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
        metadata: &crate::tool_local::FileMetadata,
    ) -> Result<
        (
            NetworkFdMutationAdmission,
            crate::network_runtime::original_installation::Installation,
        ),
        NetworkReplayError,
    > {
        let returned =
            self.terminal_original_allocator_effect(owner, admission, receipt, actual, metadata)?;
        self.validate_fd_metadata(publisher, admission.arguments.files, actual, metadata)?;
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.terminal_publication.is_some() {
            return Err(protocol(
                "terminal allocator already owns a publication attempt",
            ));
        }
        let prior = original.socket_mutation.clone();
        if state.capture_publication != prior.as_ref().map(|mutation| mutation.publication.permit) {
            return Err(protocol(
                "terminal allocator changed its retained prior lease",
            ));
        }
        let mutation = if let Some(prior) = &prior {
            let recovery =
                self.terminal_allocator_recovery_batch(prior, receipt, actual, metadata)?;
            self.transfer_terminal_allocator_mutation(prior, publisher, returned, recovery)?
        } else {
            self.admit_completed_original_allocation(
                publisher,
                admission.arguments.files,
                admission.arguments.kind,
                returned,
            )?
        };
        let bound = receipt
            .for_terminal_publication(mutation.publication.permit)
            .expect("terminal receipt and actual permit were prevalidated for the same table");
        let state = self.stream_calls.get_mut(&admission.call).unwrap();
        state.capture_publication = Some(mutation.publication.permit);
        let original = state.original.as_mut().unwrap();
        original.socket_mutation = Some(mutation.clone());
        original.terminal_prior_mutation = prior;
        original.terminal_publication = Some(publisher);
        Ok((mutation, bound))
    }

    pub(super) fn retain_terminal_allocator_enrollment(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        plan: super::original_installation::Enrollment,
    ) -> Result<(), NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments || original.terminal_publication.is_none() {
            return Err(protocol("terminal recovery lost its original Call"));
        }
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .terminal_recovered_enrollment = Some(plan);
        Ok(())
    }

    /// A disappeared table has no guest observer to which an installation may
    /// be published. Release only after the actual install and removal are both
    /// proved; keep the complete receipt inside this original Call until its
    /// normal physical/transport retirement. This is failed-run reconciliation,
    /// not a synthesized guest return or successful trace publication.
    pub(crate) fn reconcile_terminal_removed_allocator(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        receipt: &crate::network_runtime::original_installation::Installation,
        actual: &std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
        metadata: &crate::tool_local::FileMetadata,
    ) -> Result<(), NetworkReplayError> {
        self.terminal_original_allocator_effect(owner, admission, receipt, actual, metadata)?;
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        let prior = original.socket_mutation.clone();
        if !receipt.removed_before_publication()
            || self.lifetime.table_exists(admission.arguments.files)
            || state.capture_publication
                != prior.as_ref().map(|mutation| mutation.publication.permit)
            || original.terminal_publication.is_some()
        {
            return Err(protocol(
                "terminal removal cannot erase a live table or pending publication",
            ));
        }
        let retained = if let Some(prior) = &prior {
            let plan = self.terminal_allocator_dead_enrollment(prior, receipt, actual, metadata)?;
            self.retire_terminal_allocator_mutation(
                prior,
                i64::from(receipt.fd()),
                plan.as_ref().map(|plan| plan.batch()),
            )?;
            plan
        } else {
            None
        };
        let state = self.stream_calls.get_mut(&admission.call).unwrap();
        state.capture_publication = None;
        let original = state.original.as_mut().unwrap();
        original.terminal_prior_mutation = prior;
        original.terminal_recovered_enrollment = retained;
        original.terminal_reconciled = Some(receipt.clone());
        Ok(())
    }

    pub(crate) fn finish_original_connect(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || !original.provider_retired
            || !original.pin_released
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
            || (original.arguments.kind.allocator()
                && !original.uninvoked
                && !original.allocation_published
                && original.terminal_reconciled.is_none()
                && original.terminal_no_installation.is_none())
        {
            return Err(protocol(
                "original call cannot retire unresolved physical/transport custody",
            ));
        }
        if let Some((custody, received)) = &original.read_copy {
            custody
                .require_no_unjoined_receipts(*received)
                .map_err(|error| protocol(&error.to_string()))?;
        }
        if matches!(original.arguments.kind, Kind::Sendto | Kind::BlockingSendto { .. }) && !original.consumed
            && !original.final_wait && !original.cancel_requested && !original.uninvoked
        {
            self.require_native_send_published(owner, admission.call)?;
        }
        let file = state.open_file;
        let control = original
            .epoll_control
            .as_ref()
            .map(|control| control.ticket);
        let terminal = original.final_wait;
        let uninvoked = original.uninvoked;
        if let Some(ticket) = control {
            let retired = if terminal {
                let proof = self.original_selection_terminal_retirement(owner, admission)?;
                self.lifetime
                    .retire_original_selection_after_terminal(ticket, proof)
            } else {
                self.lifetime.finish_original_selection(
                    ticket,
                    if uninvoked {
                        lifetime::TransportResolution::CancellationAcknowledgedBeforeSubmission
                    } else {
                        lifetime::TransportResolution::CompletedAndRecorded
                    },
                )
            }
            .map_err(|error| protocol(&error.to_string()))?;
            self.retire_lifetime_open_files(retired);
        }
        if let Some(file) = file {
            self.release_stream_call_lifetime(owner, admission.call, file)?;
        }
        self.stream_calls.remove(&admission.call);
        if let Some(file) = file {
            self.complete_deferred_retirement(file);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use detcore_model::fd::NetworkFdSlot;
    use detcore_model::fd::NetworkFdSlotReplacement;

    use super::*;
    use crate::types::DetTid;
    use crate::types::FdSlot;
    use crate::types::MmId;

    fn fixture(occupied: bool) -> (NetworkReplayEngine, NetworkStreamOwner, Arguments) {
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let mut engine = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        engine.fd_table_fixture_enable();
        let files = engine.fd_publication_fixture_register(owner, None);
        let binding = occupied.then_some(FdSlotBinding {
            slot: FdSlot { files, fd: 7 },
            generation: 1,
            open_file: OpenFileId::new_socket(thread, 1),
        });
        if let Some(binding) = binding {
            let replacement = NetworkFdSlotReplacement {
                files,
                installation_generation: 1,
                before: None,
                after: Some(NetworkFdSlot {
                    binding,
                    cloexec: false,
                }),
            };
            let effect = engine.fd_publication_fixture_effect(owner, replacement);
            let permit = engine.acquire_fd_publication(owner, files).unwrap().permit;
            let batch = NetworkFdPublicationBatch {
                files,
                sequence: 1,
                previous_generation: 0,
                through_generation: 1,
                entries: vec![NetworkFdPublicationEntry {
                    replacement,
                    effect,
                }],
            };
            engine
                .publish_fd_publication(owner, permit, &batch)
                .unwrap();
            engine
                .acknowledge_fd_publication(owner, permit, &batch)
                .unwrap();
        }
        (
            engine,
            owner,
            Arguments {
                kind: Kind::Connect,
                operation: ExternalOpId::new(thread, 10),
                files,
                binding,
                fd: 7,
                address: 0x2000,
                length: 16,
                original_count: 0,
            },
        )
    }
    #[test]
    fn non_read_entry_changes_only_the_same_native_call_boundary() {
        let (mut engine, owner, args) = fixture(false);
        let admission = engine.begin_original_connect(owner, args).unwrap();
        engine
            .original_connect_provider_submitted(owner, &admission)
            .unwrap();
        engine
            .original_connect_prepared(owner, &admission, Pin::Empty, 17)
            .unwrap();
        let before = format!("{engine:?}");
        assert!(engine.original_non_read_entered(owner, &admission).is_err());
        assert_eq!(
            format!("{engine:?}"),
            before,
            "uninvoked entry mutated the Call"
        );
        engine.original_connect_invoked(owner, &admission).unwrap();
        let mut expected = engine
            .original_connect_state(owner, admission.call)
            .unwrap()
            .1
            .clone();
        assert!(expected.selected.is_none());
        assert!(expected.backend_result.is_none());
        assert!(!expected.backend_entered);
        expected.backend_entered = true;
        engine.original_non_read_entered(owner, &admission).unwrap();
        assert_eq!(
            format!(
                "{:?}",
                engine
                    .original_connect_state(owner, admission.call)
                    .unwrap()
                    .1
            ),
            format!("{expected:?}"),
            "entry changed something other than its existing boundary bit",
        );
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            None
        );
        let before = format!("{engine:?}");
        assert!(engine.original_non_read_entered(owner, &admission).is_err());
        assert_eq!(
            format!("{engine:?}"),
            before,
            "duplicate entry mutated the Call"
        );
    }

    #[test]
    fn non_read_entry_cannot_replace_the_required_read_transition() {
        let (mut engine, owner, mut args) = fixture(false);
        args.kind = Kind::Read;
        args.length = 0;
        args.original_count = 8;
        let admission = engine.begin_original_connect(owner, args).unwrap();
        engine
            .original_connect_provider_submitted(owner, &admission)
            .unwrap();
        engine
            .original_call_prepared(owner, &admission, None, 17)
            .unwrap();
        engine.original_connect_invoked(owner, &admission).unwrap();
        engine
            .original_read_metadata_prepared(
                owner,
                &admission,
                &FileMetadataObservation {
                    admission: admission.clone(),
                    logical_nonblocking: None,
                    status_flags: None,
                },
            )
            .unwrap();
        let before = format!("{engine:?}");
        assert!(engine.original_non_read_entered(owner, &admission).is_err());
        assert_eq!(format!("{engine:?}"), before);
        engine.original_read_entered(owner, &admission).unwrap();
        let before = format!("{engine:?}");
        assert!(engine.original_read_entered(owner, &admission).is_err());
        assert!(
            engine
                .original_read_interrupted_before_entry(owner, &admission)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            None
        );
    }

    fn epoll_arguments(mut args: Arguments) -> Arguments {
        args.kind = Kind::EpollCtl;
        args.binding = None;
        args.length = libc::EPOLL_CTL_ADD;
        args.original_count = u64::from(args.fd as u32);
        args
    }
    #[test]
    fn original_epoll_reservation_allows_shared_close_and_retires_only_after_actual_disarm_ack() {
        for submitted in [false, true] {
            let (mut engine, owner, args) = fixture(true);
            let binding = args.binding.unwrap();
            let arguments = epoll_arguments(args);
            let admission = engine
                .begin_original_epoll_ctl(owner, arguments.clone())
                .unwrap();
            let peer_thread = DetTid::from_raw(62);
            let peer = NetworkStreamOwner {
                thread: peer_thread,
                mm: MmId::initial(peer_thread),
            };
            let files = engine.fd_publication_fixture_register(peer, Some(owner));
            let permit = engine.acquire_fd_publication(peer, files).unwrap().permit;
            assert!(
                engine
                    .lifetime
                    .close_retained_binding(binding)
                    .unwrap()
                    .is_empty()
            );
            assert!(!engine.lifetime.is_retired(binding.open_file));
            engine.release_empty_fd_publication(peer, permit).unwrap();
            if !submitted {
                engine
                    .abort_original_before_provider(owner, &admission)
                    .unwrap();
            } else {
                engine
                    .original_connect_provider_submitted(owner, &admission)
                    .unwrap();
                engine
                    .original_call_prepared(owner, &admission, None, 17)
                    .unwrap();
                let local = Local {
                    arguments,
                    raw_arguments: [0; 6],
                    admission: Some(admission.clone()),
                    invoked: false,
                    returned: None,
                };
                engine.original_connect_consumed(owner, &local).unwrap();
                engine
                    .original_connect_disarmed(owner, &admission, 17)
                    .unwrap();
                assert!(engine.stream_calls.contains_key(&admission.call));
                assert!(!engine.lifetime.is_retired(binding.open_file));
                assert!(engine.finish_original_connect(owner, &admission).is_err());
                engine
                    .original_connect_cancel_retired(owner, &admission)
                    .unwrap();
                assert!(engine.finish_original_connect(owner, &admission).is_err());
                engine
                    .original_connect_pin_released(owner, &admission)
                    .unwrap();
                engine.finish_original_connect(owner, &admission).unwrap();
            }
            assert!(engine.lifetime.is_retired(binding.open_file));
            assert!(!engine.stream_calls.contains_key(&admission.call));
        }
    }
    pub(super) fn epoll_metadata_fixture() -> (
        NetworkReplayEngine,
        NetworkStreamOwner,
        Admission,
        std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
        FdSlotBinding,
    ) {
        use crate::network_runtime::original_installation::FileIdentity;
        let (mut engine, owner, args) = fixture(false);
        let args = epoll_arguments(args);
        let local = crate::tool_local::FileMetadata::empty_network_fixture(owner.thread);
        let (mut local, change) = local
            .prepare_original_installation_typed(
                owner.thread,
                7,
                nix::fcntl::OFlag::empty(),
                crate::fd::FdType::Socket,
                None,
            )
            .unwrap();
        let binding = change.after.unwrap().binding;
        local
            .bind_native_installation(binding, FileIdentity::controlled_fixture(5, 31))
            .unwrap();
        assert!(local.acknowledge_network_installations(&[change]));
        let effect = engine.fd_publication_fixture_effect(owner, change);
        let permit = engine
            .acquire_fd_publication(owner, args.files)
            .unwrap()
            .permit;
        let batch = NetworkFdPublicationBatch {
            files: args.files,
            sequence: 1,
            previous_generation: 0,
            through_generation: 1,
            entries: vec![NetworkFdPublicationEntry {
                replacement: change,
                effect,
            }],
        };
        engine
            .publish_fd_publication(owner, permit, &batch)
            .unwrap();
        engine
            .acknowledge_fd_publication(owner, permit, &batch)
            .unwrap();
        let metadata = std::sync::Arc::new(std::sync::Mutex::new(local));
        engine
            .associate_fd_metadata(owner, &metadata, &metadata.lock().unwrap())
            .unwrap();
        let admission = engine.begin_original_epoll_ctl(owner, args).unwrap();
        engine
            .original_connect_provider_submitted(owner, &admission)
            .unwrap();
        engine
            .original_call_prepared(owner, &admission, None, 17)
            .unwrap();
        engine.original_connect_invoked(owner, &admission).unwrap();
        engine
            .original_epoll_ctl_metadata(owner, &admission, &metadata, &metadata.lock().unwrap())
            .unwrap();
        (engine, owner, admission, metadata, binding)
    }
    #[test]
    fn original_epoll_join_keeps_old_and_new_same_fd_at_distinct_native_cuts() {
        use crate::network_runtime::original_installation::FileIdentity;
        let (mut engine, owner, admission, metadata, old) = epoll_metadata_fixture();
        let ticket = engine.stream_calls[&admission.call]
            .original
            .as_ref()
            .unwrap()
            .epoll_control
            .as_ref()
            .unwrap()
            .ticket;
        assert!(
            engine
                .lifetime
                .close_retained_binding(old)
                .unwrap()
                .is_empty()
        );
        assert!(metadata.lock().unwrap().remove_descriptor_binding(old));
        let (mut next, change) = metadata
            .lock()
            .unwrap()
            .prepare_original_installation_typed(
                owner.thread,
                7,
                nix::fcntl::OFlag::empty(),
                crate::fd::FdType::Socket,
                None,
            )
            .unwrap();
        let new = change.after.unwrap().binding;
        next.bind_native_installation(new, FileIdentity::controlled_fixture(5, 37))
            .unwrap();
        assert!(next.acknowledge_network_installations(&[change]));
        let effect = engine.fd_publication_fixture_effect(owner, change);
        let permit = engine
            .acquire_fd_publication(owner, admission.arguments.files)
            .unwrap()
            .permit;
        let batch = NetworkFdPublicationBatch {
            files: admission.arguments.files,
            sequence: 2,
            previous_generation: 1,
            through_generation: 2,
            entries: vec![NetworkFdPublicationEntry {
                replacement: change,
                effect,
            }],
        };
        engine
            .publish_fd_publication(owner, permit, &batch)
            .unwrap();
        engine
            .acknowledge_fd_publication(owner, permit, &batch)
            .unwrap();
        *metadata.lock().unwrap() = next;
        let (history, _) = crate::network_runtime::original_epoll_ctl::controlled_history_fixture(
            owner,
            metadata.clone(),
            &admission,
            [31, 37],
        )
        .unwrap();
        // A physical identity cannot guess an unpublished native annotation.
        assert!(
            !engine
                .original_epoll_ctl_selected(owner, &admission, &history)
                .unwrap()
        );
        assert_eq!(
            engine
                .lifetime
                .original_selection_resolution(ticket)
                .unwrap(),
            None
        );
        engine
            .note_epoll_published_metadata(owner, &metadata, &metadata.lock().unwrap(), new)
            .unwrap();
        assert!(
            engine
                .lifetime
                .close_retained_binding(new)
                .unwrap()
                .is_empty()
        );
        assert!(metadata.lock().unwrap().remove_descriptor_binding(new));
        assert!(
            engine
                .original_epoll_ctl_selected(owner, &admission, &history)
                .unwrap()
        );
        assert_eq!(
            engine
                .lifetime
                .original_selection_resolution(ticket)
                .unwrap(),
            Some([Some(old), Some(new)])
        );
        assert!(
            !engine.lifetime.is_retired(old.open_file)
                && !engine.lifetime.is_retired(new.open_file)
        );
        // No result or completion is inferred from selecting two real files.
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            None
        );
        assert!(
            engine
                .original_connect_provider_retired(owner, &admission, 0)
                .is_err()
        );
        engine
            .original_connect_returned(owner, &admission, 0)
            .unwrap();
        engine
            .original_connect_provider_retired(owner, &admission, 0)
            .unwrap();
        engine
            .original_connect_pin_released(owner, &admission)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        assert!(
            engine.lifetime.is_retired(old.open_file) && engine.lifetime.is_retired(new.open_file)
        );
    }
    #[test]
    fn original_epoll_join_refuses_another_metadata_arc_and_preserves_unresolved_state() {
        let (mut engine, owner, admission, metadata, _) = epoll_metadata_fixture();
        let wrong = std::sync::Arc::new(std::sync::Mutex::new(metadata.lock().unwrap().clone()));
        let (wrong_history, _) =
            crate::network_runtime::original_epoll_ctl::controlled_history_fixture(
                owner,
                wrong,
                &admission,
                [31, 31],
            )
            .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .original_epoll_ctl_selected(owner, &admission, &wrong_history)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        let (history, _) = crate::network_runtime::original_epoll_ctl::controlled_history_fixture(
            owner,
            metadata,
            &admission,
            [31, 31],
        )
        .unwrap();
        assert!(
            engine
                .original_epoll_ctl_selected(owner, &admission, &history)
                .unwrap()
        );
        assert!(
            engine
                .original_epoll_ctl_selected(owner, &admission, &history)
                .unwrap()
        );
    }
    #[test]
    fn original_epoll_same_file_reinstallation_never_chooses_an_arbitrary_generation() {
        use crate::network_runtime::original_installation::FileIdentity;
        let (mut engine, owner, admission, metadata, old) = epoll_metadata_fixture();
        let task = lifetime::TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        engine
            .lifetime
            .duplicate(task, 7, old.open_file, 8, false)
            .unwrap();
        engine.lifetime.close_retained_binding(old).unwrap();
        engine
            .lifetime
            .duplicate(task, 8, old.open_file, 7, false)
            .unwrap();
        let new = engine.lifetime.descriptor_binding(task, 7).unwrap();
        assert_ne!(new, old);
        assert_eq!(new.open_file, old.open_file);
        engine
            .note_epoll_native_binding(new, FileIdentity::controlled_fixture(5, 31))
            .unwrap();
        let (history, _) = crate::network_runtime::original_epoll_ctl::controlled_history_fixture(
            owner,
            metadata,
            &admission,
            [31, 31],
        )
        .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .original_epoll_ctl_selected(owner, &admission, &history)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
    }
    #[test]
    fn original_epoll_terminal_cleanup_retains_unknown_pair_as_failed_run_tombstone() {
        let (mut engine, owner, admission, _, binding) = epoll_metadata_fixture();
        let ticket = engine.stream_calls[&admission.call]
            .original
            .as_ref()
            .unwrap()
            .epoll_control
            .as_ref()
            .unwrap()
            .ticket;
        let local = Local {
            arguments: admission.arguments.clone(),
            raw_arguments: [0; 6],
            admission: Some(admission.clone()),
            invoked: true,
            returned: None,
        };
        engine.original_connect_consumed(owner, &local).unwrap();
        engine
            .lifetime
            .exit(lifetime::TaskOwner {
                tid: owner.thread,
                mm: owner.mm,
            })
            .unwrap();
        assert!(!engine.lifetime.is_retired(binding.open_file));
        assert!(engine.finish_original_connect(owner, &admission).is_err());
        engine.original_connect_final_wait(owner, &local).unwrap();
        engine
            .original_connect_dead_retired(owner, &admission, 17)
            .unwrap();
        assert!(engine.finish_original_connect(owner, &admission).is_err());
        engine
            .original_connect_pin_released(owner, &admission)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        assert!(engine.lifetime.is_retired(binding.open_file));
        assert_eq!(
            engine
                .lifetime
                .original_selection_resolution(ticket)
                .unwrap(),
            None
        );
        assert_eq!(
            engine
                .lifetime
                .original_selection_candidates(ticket)
                .unwrap(),
            [binding]
        );
        assert_eq!(
            engine.lifetime.finish(),
            Err(lifetime::LifetimeError::OutstandingOwners)
        );
    }
    #[test]
    fn original_epoll_control_kind_preserves_int_operands_without_admitting_unjoined_calls() {
        assert_eq!(Kind::EpollCtl.provider_operation(), 20);
        assert_eq!(
            Kind::EpollCtl.syscall(),
            reverie::syscalls::Sysno::epoll_ctl
        );
        for fd in [i32::MIN, -1, 0, 17, i32::MAX] {
            let count = u64::from(fd as u32);
            assert!(Kind::EpollCtl.valid_operands(u64::MAX, i32::MIN, count));
            assert!(Kind::EpollCtl.valid_counted_result(-i64::from(libc::EFAULT), count));
            assert!(Kind::EpollCtl.valid_counted_result(0, count));
            assert!(!Kind::EpollCtl.valid_counted_result(1, count));
        }
        assert!(!Kind::EpollCtl.valid_operands(0, libc::EPOLL_CTL_ADD, 1u64 << 32));
        let (mut engine, owner, mut arguments) = fixture(true);
        arguments.kind = Kind::EpollCtl;
        arguments.length = libc::EPOLL_CTL_ADD;
        arguments.original_count = 7;
        arguments.binding = None;
        assert!(engine.begin_original_connect(owner, arguments).is_err());
        assert!(engine.stream_calls.is_empty());
    }
    #[test]
    fn original_read_keeps_full_operands_and_requires_separate_prepared_selection_and_return() {
        // Explicit component inputs exercise the production Call transitions;
        // they do not stand in for the provider's native issuer qualification.
        for (occupied, flags, count, returned) in [
            (true, 0, 0, 0),
            (true, 0, 1, 1),
            (true, 0, 1_u64 << 32, 3),
            (true, libc::O_PATH, 0x10000000020, -i64::from(libc::EBADF)),
            (false, 0, u64::MAX, -i64::from(libc::EBADF)),
            (
                true,
                0,
                16,
                -i64::from(reverie::syscalls::Errno::ERESTARTSYS.into_raw()),
            ),
        ] {
            let (mut engine, owner, mut args) = fixture(occupied);
            args.kind = Kind::Read;
            args.length = 0;
            args.original_count = count;
            let admission = engine.begin_original_connect(owner, args.clone()).unwrap();
            engine
                .original_connect_provider_submitted(owner, &admission)
                .unwrap();
            engine
                .original_call_prepared(owner, &admission, None, 17)
                .unwrap();
            engine.original_connect_invoked(owner, &admission).unwrap();
            let physical_file = if occupied && flags & libc::O_PATH == 0 {
                7
            } else {
                0
            };
            let before = format!("{engine:?}");
            assert!(
                engine
                    .original_connect_selected(
owner,
&admission,
17,
(1, 61, 99, 5, physical_file),
)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
            engine
                .original_read_metadata_prepared(
                    owner,
                    &admission,
                    &FileMetadataObservation {
                        admission: admission.clone(),
                        logical_nonblocking: occupied.then_some(false),
                        status_flags: occupied.then_some(flags),
                    },
                )
                .unwrap();
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                None
            );
            let before = format!("{engine:?}");
            assert!(
                engine
                    .original_connect_selected(
                        owner,
                        &admission,
                        17,
                        (1, 61, 99, 5, if physical_file == 0 { 7 } else { 0 }),
                    )
                    .is_err()
            );
            let mut wrong = admission.clone();
            wrong.arguments.original_count ^= 1_u64 << 32;
            assert!(
                engine
                    .original_connect_returned(owner, &wrong, returned)
                    .is_err()
            );
            assert!(
                engine
                    .original_connect_returned(owner, &admission, -4096)
                    .is_err()
            );
            assert!(
                engine
                    .original_connect_returned(owner, &admission, 0x7ffff001)
                    .is_err()
            );
            if count < 0x7ffff000 {
                assert!(
                    engine
                        .original_connect_returned(owner, &admission, count as i64 + 1)
                        .is_err()
                );
            }
            assert_eq!(format!("{engine:?}"), before);
            engine
                .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, physical_file))
                .unwrap();
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                None
            );
            assert_eq!(
                engine
                    .original_call_state(owner, admission.call)
                    .unwrap()
                    .0
                    .capture_publication,
                None
            );
            assert_eq!(
                engine
                    .original_call_state(owner, admission.call)
                    .unwrap()
                    .0
                    .capture_control,
                None
            );
            assert!(
                engine
                    .original_connect_provider_retired(owner, &admission, returned)
                    .is_err()
            );
            // This explicit event input represents actual Returned. An adapter's
            // synthetic restart errno cannot call this transition by itself.
            engine
                .original_connect_returned(owner, &admission, returned)
                .unwrap();
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                Some(returned)
            );
            assert_eq!(
                engine
                    .original_call_state(owner, admission.call)
                    .unwrap()
                    .1
                    .pin,
                None
            );
            engine
                .original_connect_provider_retired(owner, &admission, returned)
                .unwrap();
            engine
                .original_connect_pin_released(owner, &admission)
                .unwrap();
            engine.finish_original_connect(owner, &admission).unwrap();
        }
    }

    #[test]
    fn original_read_preentry_cancel_keeps_exact_call_until_disarm_and_ack() {
        // Explicit component observations; actual ptrace producer and provider
        // disarm/ACK are qualified separately, never inferred from these inputs.
        for occupied in [false, true] {
            let (mut engine, owner, mut args) = fixture(occupied);
            args.kind = Kind::Read;
            args.length = 0;
            args.original_count = 1u64 << 40;
            let peer = NetworkStreamOwner {
                thread: DetTid::from_raw(62),
                ..owner
            };
            assert_eq!(
                engine.fd_publication_fixture_register(peer, Some(owner)),
                args.files
            );
            let admission = engine.begin_original_connect(owner, args.clone()).unwrap();
            engine
                .original_connect_provider_submitted(owner, &admission)
                .unwrap();
            engine
                .original_call_prepared(owner, &admission, None, 17)
                .unwrap();
            engine.original_connect_invoked(owner, &admission).unwrap();
            assert!(
                engine
                    .original_read_interrupted_before_entry(owner, &admission)
                    .is_err()
            );
            engine
                .original_read_metadata_prepared(
                    owner,
                    &admission,
                    &FileMetadataObservation {
                        admission: admission.clone(),
                        logical_nonblocking: occupied.then_some(false),
                        status_flags: occupied.then_some(0),
                    },
                )
                .unwrap();
            let before = format!("{engine:?}");
            let mut wrong = admission.clone();
            wrong.arguments.original_count ^= 1u64 << 32;
            assert!(
                engine
                    .original_read_interrupted_before_entry(peer, &admission)
                    .is_err()
            );
            assert!(
                engine
                    .original_read_interrupted_before_entry(owner, &wrong)
                    .is_err()
            );
            assert!(
                engine
                    .validate_original_read_interrupted(owner, &admission)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
            engine
                .original_read_interrupted_before_entry(owner, &admission)
                .unwrap();
            engine
                .validate_original_read_interrupted(owner, &admission)
                .unwrap();
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                None
            );
            assert_eq!(
                engine
                    .original_connect_cancellation(owner, &admission)
                    .unwrap(),
                (true, false)
            );
            assert!(!engine.gone_stream_owners.contains(&owner));
            assert!(
                engine
                    .original_call_state(owner, admission.call)
                    .unwrap()
                    .0
                    .capture_publication
                    .is_some()
            );
            let before = format!("{engine:?}");
            assert!(
                engine
                    .original_read_interrupted_before_entry(owner, &admission)
                    .is_err()
            );
            assert!(engine.original_read_entered(owner, &admission).is_err());
            assert!(
                engine
                    .original_connect_returned(owner, &admission, 0)
                    .is_err()
            );
            assert!(
                engine
                    .original_connect_disarmed(owner, &admission, 18)
                    .is_err()
            );
            assert!(
                engine
                    .original_connect_cancel_retired(owner, &admission)
                    .is_err()
            );
            assert!(engine.finish_original_connect(owner, &admission).is_err());
            assert_eq!(format!("{engine:?}"), before);
            engine
                .original_connect_disarmed(owner, &admission, 17)
                .unwrap();
            assert_eq!(
                engine
                    .original_call_state(owner, admission.call)
                    .unwrap()
                    .0
                    .capture_publication,
                None
            );
            assert_eq!(
                engine
                    .original_call_state(owner, admission.call)
                    .unwrap()
                    .0
                    .capture_control,
                None
            );
            assert!(engine.finish_original_connect(owner, &admission).is_err());
            engine
                .original_connect_cancel_retired(owner, &admission)
                .unwrap();
            assert!(engine.finish_original_connect(owner, &admission).is_err());
            engine
                .original_connect_pin_released(owner, &admission)
                .unwrap();
            engine.finish_original_connect(owner, &admission).unwrap();
            assert!(!engine.gone_stream_owners.contains(&owner));
            assert!(engine.finish_original_connect(owner, &admission).is_err());
        }
    }

    fn read_copy_custody_fixture() -> (
        NetworkReplayEngine,
        NetworkStreamOwner,
        Admission,
        std::sync::Arc<crate::network_runtime::original_read_copy::ReadCopyCustody>,
    ) {
        let (mut engine, owner, mut args) = fixture(true);
        args.kind = Kind::Read;
        args.length = 0;
        args.original_count = 8;
        let admission = engine.begin_original_connect(owner, args).unwrap();
        engine
            .original_connect_provider_submitted(owner, &admission)
            .unwrap();
        engine
            .original_call_prepared(owner, &admission, None, 17)
            .unwrap();
        engine.original_connect_invoked(owner, &admission).unwrap();
        engine
            .original_read_metadata_prepared(
                owner,
                &admission,
                &FileMetadataObservation {
                    admission: admission.clone(),
                    logical_nonblocking: Some(false),
                    status_flags: Some(0),
                },
            )
            .unwrap();
        engine.original_read_entered(owner, &admission).unwrap();
        engine
            .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 7))
            .unwrap();
        (engine, owner, admission, Default::default())
    }

    #[test]
    fn original_read_custody_binds_exact_existing_call_command_and_raw_store() {
        let (mut engine, owner, admission, custody) = read_copy_custody_fixture();
        let before = format!("{engine:?}");
        assert!(
            engine
                .bind_original_read_copy(owner, &admission, 18, &custody)
                .is_err()
        );
        let foreign = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            ..owner
        };
        assert!(
            engine
                .bind_original_read_copy(foreign, &admission, 17, &custody)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(
            engine
                .bind_original_read_copy(owner, &admission, 17, &custody)
                .unwrap(),
            0
        );
        let before = format!("{engine:?}");
        let replacement = std::sync::Arc::new(
            crate::network_runtime::original_read_copy::ReadCopyCustody::default(),
        );
        assert!(
            engine
                .bind_original_read_copy(owner, &admission, 17, &replacement)
                .is_err()
        );
        assert!(custody.bind(foreign, admission.call, 17).is_err());
        assert!(custody.bind(owner, admission.call, 18).is_err());
        assert_eq!(
            engine
                .bind_original_read_copy(owner, &admission, 17, &custody)
                .unwrap(),
            0
        );
        assert_eq!(format!("{engine:?}"), before);
        let retained = engine
            .original_connect_state(owner, admission.call)
            .unwrap()
            .1
            .read_copy
            .as_ref()
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&retained.0, &custody));
    }

    #[test]
    fn original_read_completed_receipt_stays_owned_after_physical_ack_and_refuses_retirement() {
        use crate::network_runtime::OriginalSelection;
        use crate::network_runtime::original_read_copy as copy;
        let (mut engine, owner, admission, custody) = read_copy_custody_fixture();
        assert_eq!(
            engine
                .bind_original_read_copy(owner, &admission, 17, &custody)
                .unwrap(),
            0
        );
        let selected = OriginalSelection {
            provider: 1,
            command: 17,
            call: admission.call.native_command_call(),
            owner_mm: owner.mm.generation(),
            task: 61,
            task_start: 99,
            table: 5,
            file: 7,
            ready: 1,
            requested_fd: admission.arguments.fd,
            user_address: admission.arguments.address,
            original_count: 8,
            fdput_flags: 1,
            address_length: 0,
        };
        let mut bytes = vec![0; copy::RECORD_BYTES];
        bytes[..3].copy_from_slice(b"abc");
        let first = copy::Record {
            provider: 1,
            command: 17,
            call: admission.call.native_command_call(),
            task: 61,
            task_start: 99,
            sequence: 1,
            attempt: 1,
            offset: 0,
            length: 3,
            kind: 1,
            bytes,
        };
        let mut unit = first.clone();
        unit.sequence = 2;
        unit.kind = 3;
        unit.length = 72;
        unit.bytes.fill(0);
        for (slot, value) in unit.bytes[..72].as_chunks_mut::<8>().0.iter_mut().zip([
            7u64,
            4,
            0,
            3,
            3,
            0,
            55,
            1,
            copy::CONSUME,
        ]) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        let records = vec![first, unit];
        custody
            .append(
                &selected,
                0,
                records.clone(),
                Some(copy::End::OriginalExit { protocol: true }),
            )
            .unwrap();
        let delta = custody.completed_since(0).unwrap().unwrap();
        assert_eq!(
            engine
                .retain_original_read_copy(owner, &admission, delta.clone())
                .unwrap(),
            1
        );
        let before = format!("{engine:?}");
        assert!(
            engine
                .retain_original_read_copy(owner, &admission, delta)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        let capture = custody
            .collect_controlled_original_read(
                selected,
                3,
                copy::Summary {
                    version: 4,
                    initial_count: 8,
                    attempts: 1,
                    records: 2,
                    copied: 3,
                    final_count: 5,
                    protocol_returned: 3,
                    protocol_complete: 1,
                },
            )
            .unwrap();
        assert_eq!(capture.records, records);
        engine
            .original_connect_returned(owner, &admission, 3)
            .unwrap();
        engine
            .original_connect_provider_retired(owner, &admission, 3)
            .unwrap();
        engine
            .original_connect_pin_released(owner, &admission)
            .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .finish_original_connect(owner, &admission)
                .unwrap_err()
                .to_string()
                .contains("unjoined kernel copy receipts")
        );
        assert_eq!(format!("{engine:?}"), before);
        let retained = engine
            .original_connect_state(owner, admission.call)
            .unwrap()
            .1
            .read_copy
            .as_ref()
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&retained.0, &custody));
        assert_eq!(retained.1, 1);
        assert_eq!(capture.records, records);
        assert_eq!(capture.manifest.returned, 3);
    }

    #[test]
    fn original_read_entered_is_not_preentry_cancellation_authority() {
        let (mut engine, owner, mut args) = fixture(true);
        args.kind = Kind::Read;
        args.length = 0;
        args.original_count = 8;
        let admission = engine.begin_original_connect(owner, args).unwrap();
        engine
            .original_connect_provider_submitted(owner, &admission)
            .unwrap();
        engine
            .original_call_prepared(owner, &admission, None, 17)
            .unwrap();
        engine.original_connect_invoked(owner, &admission).unwrap();
        engine
            .original_read_metadata_prepared(
                owner,
                &admission,
                &FileMetadataObservation {
                    admission: admission.clone(),
                    logical_nonblocking: Some(false),
                    status_flags: Some(0),
                },
            )
            .unwrap();
        engine.original_read_entered(owner, &admission).unwrap();
        let before = format!("{engine:?}");
        assert!(engine.original_read_entered(owner, &admission).is_err());
        assert!(
            engine
                .original_read_interrupted_before_entry(owner, &admission)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            None
        );
        engine
            .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 7))
            .unwrap();
        engine
            .original_connect_returned(owner, &admission, 3)
            .unwrap();
        assert!(
            engine
                .validate_original_read_interrupted(owner, &admission)
                .is_err()
        );
        engine
            .original_connect_provider_retired(owner, &admission, 3)
            .unwrap();
        engine
            .original_connect_pin_released(owner, &admission)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
    }

    #[test]
    fn selected_external_cancellation_refuses_changed_untransferred_local_without_releasing_peer_table()
     {
        for variant in 0..5 {
            let (mut engine, owner, mut args) = fixture(true);
            args.kind = Kind::Close;
            args.address = 0;
            args.length = 0;
            let peer = NetworkStreamOwner {
                thread: DetTid::from_raw(62),
                ..owner
            };
            assert_eq!(
                engine.fd_publication_fixture_register(peer, Some(owner)),
                args.files
            );
            let NetworkFdReadBegin::Admitted(read) =
                engine.begin_fd_read(owner, args.files, args.fd).unwrap()
            else {
                panic!("unexpected recovery")
            };
            let read = *read;
            let read = engine
                .bind_fd_read_external_grant(owner, read, args.operation)
                .unwrap();
            let local = Local {
                arguments: args.clone(),
                raw_arguments: [7, 0, 0, 0, 0, 0],
                admission: None,
                invoked: false,
                returned: None,
            };
            let mut wrong = local.clone();
            match variant {
                0 => wrong.arguments.operation = ExternalOpId::new(owner.thread, 11),
                1 => wrong.arguments.fd = 8,
                2 => wrong.invoked = true,
                3 => wrong.returned = Some(0),
                4 => wrong.arguments.kind = Kind::File(FileOperation::GetFlags),
                _ => unreachable!(),
            }
            let before = format!("{engine:?}");
            assert!(engine.original_connect_consumed(owner, &wrong).is_err());
            assert_eq!(format!("{engine:?}"), before);
            assert_eq!(
                engine.native_capture_fixture_counts(args.binding.unwrap().open_file),
                (0, 1, 1, 0)
            );
            assert!(matches!(
                engine.begin_fd_read(peer, args.files, 7),
                Err(NetworkReplayError::StreamOperationBusy(_))
            ));
            engine.original_connect_consumed(owner, &local).unwrap();
            assert_eq!(
                engine.native_capture_fixture_counts(args.binding.unwrap().open_file),
                (0, 0, 0, 0)
            );
            assert!(engine.finish_fd_read(owner, read).is_err());
            let NetworkFdReadBegin::Admitted(next) =
                engine.begin_fd_read(peer, args.files, 7).unwrap()
            else {
                panic!("peer requires exact released table")
            };
            let next = *next;
            assert_eq!(next.binding, args.binding);
            engine.finish_fd_read(peer, next).unwrap();
        }
    }

    #[test]
    fn admitted_file_reader_transfers_once_without_a_second_table_or_ofd_lease() {
        let (mut engine, owner, mut args) = fixture(true);
        args.kind = Kind::File(FileOperation::GetFlags);
        args.address = reverie::syscalls::Sysno::fcntl as u64;
        args.length = libc::F_GETFL;
        let file = args.binding.unwrap().open_file;
        let NetworkFdReadBegin::Admitted(read) =
            engine.begin_fd_read(owner, args.files, args.fd).unwrap()
        else {
            panic!("unexpected recovery")
        };
        let read = *read;
        let next_lease = engine.next_stream_lease;
        let admission = engine
            .begin_original_file_from_read(
                owner,
                args.clone(),
                read.clone(),
                crate::OriginalFileExecution::Native,
            )
            .unwrap();
        assert_eq!(engine.next_stream_lease, next_lease);
        let (state, original) = engine.original_call_state(owner, admission.call).unwrap();
        assert_eq!(state.capture_publication, Some(read.publication.permit));
        assert_eq!(state.capture_control, read.control);
        assert!(!state.physical_pin_required);
        assert_eq!(original.command, None);
        assert_eq!(original.selected, None);
        assert_eq!(original.backend_result, None);
        assert_eq!(original.pin, None);
        assert!(!original.provider_submitted);
        assert!(!original.provider_retired);
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
        let before = format!("{engine:?}");
        assert!(engine.finish_fd_read(owner, read.clone()).is_err());
        assert!(
            engine
                .begin_original_file_from_read(
                    owner,
                    args,
                    read,
                    crate::OriginalFileExecution::Native
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        // No provider was submitted in this component. Use the same explicit
        // pre-provider cleanup, not a fabricated native completion/selection.
        engine
            .abort_original_before_provider(owner, &admission)
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
    }

    #[test]
    fn admitted_file_reader_wrong_lookup_keeps_the_original_token_unchanged() {
        let (mut engine, owner, mut args) = fixture(true);
        args.kind = Kind::File(FileOperation::GetFlags);
        args.address = reverie::syscalls::Sysno::fcntl as u64;
        args.length = libc::F_GETFL;
        let NetworkFdReadBegin::Admitted(read) =
            engine.begin_fd_read(owner, args.files, args.fd).unwrap()
        else {
            panic!("unexpected recovery")
        };
        let read = *read;
        for which in 0..5 {
            let mut wrong = args.clone();
            let sender = if which == 0 {
                NetworkStreamOwner {
                    mm: owner.mm.for_exec(owner.thread),
                    ..owner
                }
            } else {
                owner
            };
            match which {
                0 => {}
                1 => wrong.binding.as_mut().unwrap().generation += 1,
                2 => wrong.binding = None,
                3 => wrong.fd += 1,
                4 => wrong.kind = Kind::Close,
                _ => unreachable!(),
            }
            let before = format!("{engine:?}");
            assert!(
                engine
                    .begin_original_file_from_read(
                        sender,
                        wrong,
                        read.clone(),
                        crate::OriginalFileExecution::Native
                    )
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
        }
        engine.finish_fd_read(owner, read).unwrap();
        assert_eq!(
            engine.native_capture_fixture_counts(args.binding.unwrap().open_file),
            (0, 0, 0, 0)
        );
    }

    fn prepare(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        args: Arguments,
    ) -> Admission {
        let admission = engine.begin_original_connect(owner, args).unwrap();
        engine
            .original_connect_provider_submitted(owner, &admission)
            .unwrap();
        let pin = if admission.arguments.binding.is_some() {
            Pin::Socket {
                domain: libc::AF_INET,
                kind: libc::SOCK_STREAM,
                protocol: libc::IPPROTO_TCP,
            }
        } else {
            Pin::Empty
        };
        engine
            .original_connect_prepared(owner, &admission, pin, 17)
            .unwrap();
        admission
    }
    // Explicit component receipts test the same Call transitions used by the
    // real provider and backend. They are not native issuer qualification.
    fn prepared_file(occupied: bool) -> (NetworkReplayEngine, NetworkStreamOwner, Admission) {
        let (mut engine, owner, mut args) = fixture(occupied);
        args.kind = Kind::File(FileOperation::GetFlags);
        args.address = reverie::syscalls::Sysno::fcntl as u64;
        args.length = libc::F_GETFL;
        let admission = engine.begin_original_connect(owner, args).unwrap();
        engine
            .original_connect_provider_submitted(owner, &admission)
            .unwrap();
        assert!(
            engine
                .original_call_prepared(owner, &admission, Some(Pin::Other), 17)
                .is_err()
        );
        engine
            .original_call_prepared(owner, &admission, None, 17)
            .unwrap();
        engine.original_connect_invoked(owner, &admission).unwrap();
        (engine, owner, admission)
    }
    #[test]
    fn original_file_selection_releases_table_before_result_and_never_acquires_a_pin() {
        let (mut engine, owner, admission) = prepared_file(true);
        let file = admission.arguments.binding.unwrap().open_file;
        engine
            .validate_original_file_prepared(owner, &admission)
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
        engine
            .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 7))
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 0, 1));
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            None
        );
        let unrelated = engine
            .acquire_fd_publication(owner, admission.arguments.files)
            .unwrap();
        engine
            .release_empty_fd_publication(owner, unrelated.permit)
            .unwrap();
        assert!(engine.begin_descriptor_controls(owner, vec![file]).is_err());
        engine
            .original_connect_returned(owner, &admission, i64::from(libc::O_PATH))
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 0, 0, 1));
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            Some(i64::from(libc::O_PATH))
        );
        let state = engine
            .original_connect_state(owner, admission.call)
            .unwrap();
        assert_eq!(state.1.pin, None);
        assert!(!state.0.physical_pin_required);
        assert!(engine.finish_original_connect(owner, &admission).is_err());
        engine
            .original_connect_provider_retired(owner, &admission, i64::from(libc::O_PATH))
            .unwrap();
        engine
            .original_connect_pin_released(owner, &admission)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
    }
    #[test]
    fn original_file_return_does_not_substitute_for_selection_or_prepared_owner() {
        for occupied in [false, true] {
            let (mut engine, owner, admission) = prepared_file(occupied);
            let file = admission.arguments.binding.map(|binding| binding.open_file);
            let wrong_owner = NetworkStreamOwner {
                mm: owner.mm.for_exec(owner.thread),
                ..owner
            };
            let mut wrong = admission.clone();
            wrong.arguments.length = libc::F_GETFD;
            let before = format!("{engine:?}");
            assert!(
                engine
                    .validate_original_file_prepared(wrong_owner, &admission)
                    .is_err()
            );
            assert!(
                engine
                    .validate_original_file_prepared(owner, &wrong)
                    .is_err()
            );
            for bad in [-4096, i64::from(i32::MAX) + 1] {
                assert!(
                    engine
                        .original_connect_returned(owner, &admission, bad)
                        .is_err()
                );
                assert_eq!(format!("{engine:?}"), before);
            }
            let returned = if occupied {
                i64::from(libc::O_NONBLOCK)
            } else {
                -i64::from(libc::EBADF)
            };
            engine
                .original_connect_returned(owner, &admission, returned)
                .unwrap();
            assert!(
                engine
                    .acquire_fd_publication(owner, admission.arguments.files)
                    .is_err()
            );
            if let Some(file) = file {
                assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
            }
            assert!(
                engine
                    .original_connect_provider_retired(owner, &admission, returned)
                    .is_err()
            );
            engine
                .original_connect_selected(
                    owner,
                    &admission,
                    17,
                    (1, 61, 99, 5, if occupied { 7 } else { 0 }),
                )
                .unwrap();
            if let Some(file) = file {
                assert_eq!(engine.native_capture_fixture_counts(file), (1, 0, 0, 1));
            }
            engine
                .original_connect_provider_retired(owner, &admission, returned)
                .unwrap();
            engine
                .original_connect_pin_released(owner, &admission)
                .unwrap();
            engine.finish_original_connect(owner, &admission).unwrap();
        }
    }
    #[test]
    fn positive_original_file_results_do_not_expand_connect_or_close_results() {
        for returned in [1, i64::from(libc::O_PATH), i64::from(i32::MAX)] {
            assert!(Kind::File(FileOperation::GetFlags).valid_result(returned));
            assert!(!Kind::Connect.valid_result(returned));
            assert!(!Kind::Close.valid_result(returned));
        }
        for returned in [-4096, i64::from(i32::MAX) + 1] {
            assert!(!Kind::File(FileOperation::GetFlags).valid_result(returned));
        }
    }
    #[test]
    fn original_early_selection_releases_table_but_keeps_exact_call_until_every_terminal_receipt() {
        let (mut engine, owner, args) = fixture(true);
        let file = args.binding.unwrap().open_file;
        let admission = prepare(&mut engine, owner, args);
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
        assert!(
            engine
                .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 7),)
                .is_err()
        );
        engine.original_connect_invoked(owner, &admission).unwrap();
        assert!(
            engine
                .original_connect_selected(owner, &admission, 18, (1, 61, 99, 5, 7),)
                .is_err()
        );
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
        engine
            .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 7))
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 0, 0, 1));
        assert!(engine.finish_original_connect(owner, &admission).is_err());
        assert!(
            engine
                .original_connect_provider_retired(owner, &admission, 0)
                .is_err()
        );
        engine
            .original_connect_returned(owner, &admission, 0)
            .unwrap();
        assert!(
            engine
                .original_connect_provider_retired(owner, &admission, -libc::EINTR as i64)
                .is_err()
        );
        engine
            .original_connect_provider_retired(owner, &admission, 0)
            .unwrap();
        assert!(engine.finish_original_connect(owner, &admission).is_err());
        engine
            .original_connect_pin_released(owner, &admission)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
        assert!(
            engine
                .original_connect_returned(owner, &admission, 0)
                .is_err()
        );
    }
    #[test]
    fn original_missing_selection_after_owner_exit_does_not_release_or_become_a_negative_result() {
        let (mut engine, owner, args) = fixture(true);
        let file = args.binding.unwrap().open_file;
        let admission = prepare(&mut engine, owner, args.clone());
        engine.original_connect_invoked(owner, &admission).unwrap();
        let local = Local {
            arguments: args,
            raw_arguments: [7, 0x2000, 16, 0, 0, 0],
            admission: Some(admission.clone()),
            invoked: true,
            returned: None,
        };
        engine.original_connect_consumed(owner, &local).unwrap();
        engine.stream_owner_gone(owner);
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            None
        );
        assert!(
            engine
                .original_connect_disarmed(owner, &admission, 17)
                .is_err()
        );
        assert!(engine.finish_original_connect(owner, &admission).is_err());
        // A positive exact receipt may arrive after owner withdrawal. It is
        // still the retained task/MM and pin, not a newly inferred live owner.
        engine
            .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 7))
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 0, 0, 1));
    }
    #[test]
    fn original_lost_begin_or_submit_reply_keeps_known_uninvoked_custody_until_actual_disarm() {
        for lost_begin in [true, false] {
            let (mut engine, owner, args) = fixture(true);
            let file = args.binding.unwrap().open_file;
            let admission = prepare(&mut engine, owner, args.clone());
            if !lost_begin {
                engine.original_connect_invoked(owner, &admission).unwrap();
            }
            let local = Local {
                arguments: args,
                raw_arguments: [7, 0x2000, 16, 0, 0, 0],
                admission: (!lost_begin).then_some(admission.clone()),
                invoked: false,
                returned: None,
            };
            engine.original_connect_consumed(owner, &local).unwrap();
            engine.stream_owner_gone(owner);
            assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
            assert!(
                engine
                    .original_connect_disarmed(owner, &admission, 18)
                    .is_err()
            );
            assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
            engine
                .original_connect_disarmed(owner, &admission, 17)
                .unwrap();
            assert_eq!(engine.native_capture_fixture_counts(file), (1, 0, 0, 1));
            assert!(engine.finish_original_connect(owner, &admission).is_err());
            engine
                .original_connect_cancel_retired(owner, &admission)
                .unwrap();
            engine
                .original_connect_pin_released(owner, &admission)
                .unwrap();
            engine.finish_original_connect(owner, &admission).unwrap();
            assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
        }
    }
    #[test]
    fn original_empty_fd_uses_no_fabricated_ofd_and_never_accepts_a_nonempty_selection() {
        let (mut engine, owner, args) = fixture(false);
        let admission = prepare(&mut engine, owner, args);
        assert_eq!(engine.stream_calls[&admission.call].open_file, None);
        engine.original_connect_invoked(owner, &admission).unwrap();
        assert!(
            engine
                .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 7),)
                .is_err()
        );
        assert!(
            engine.fd_publications[&admission.arguments.files]
                .active
                .is_some()
        );
        engine
            .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 0))
            .unwrap();
        engine
            .original_connect_returned(owner, &admission, -i64::from(libc::EBADF))
            .unwrap();
        engine
            .original_connect_provider_retired(owner, &admission, -i64::from(libc::EBADF))
            .unwrap();
        engine
            .original_connect_pin_released(owner, &admission)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        assert!(engine.stream_calls.is_empty());
    }
    #[test]
    fn original_admission_uses_current_slot_under_permit_and_rejects_other_table_operand() {
        let (mut engine, owner, mut args) = fixture(true);
        let expected = args.binding.unwrap();
        args.binding.as_mut().unwrap().generation = 0;
        let admission = engine.begin_original_connect(owner, args.clone()).unwrap();
        assert_eq!(admission.arguments.binding, Some(expected));
        engine
            .abort_original_before_provider(owner, &admission)
            .unwrap();
        args.binding.as_mut().unwrap().slot.fd = 8;
        assert!(engine.begin_original_connect(owner, args).is_err());
        assert_eq!(
            engine.native_capture_fixture_counts(expected.open_file),
            (0, 0, 0, 0)
        );
    }
    #[test]
    fn selection_terminal_requires_final_wait_and_ack_after_exit_or_exec_without_inventing_lookups()
    {
        for exec in [false, true] {
            for primary_observed in [false, true] {
                let (mut engine, owner, args) = fixture(false);
                let task = lifetime::TaskOwner {
                    tid: owner.thread,
                    mm: owner.mm,
                };
                let file = OpenFileId::new_socket(owner.thread, 100);
                engine
                    .lifetime
                    .open(
                        task,
                        4,
                        lifetime::NetworkSlot {
                            open_file: file,
                            cloexec: true,
                        },
                    )
                    .unwrap();
                let admission = prepare(&mut engine, owner, args.clone());
                let lease = lifetime::LeaseId {
                    operation: ExternalOpId::new(owner.thread, admission.call.0),
                    mm: owner.mm,
                    kind: lifetime::LeaseKind::StreamCall,
                    ordinal: 0,
                };
                let ticket = engine
                    .lifetime
                    .prepare_original_selection(task, lease, [7, 4])
                    .unwrap();
                let candidates = engine
                    .lifetime
                    .original_selection_candidates(ticket)
                    .unwrap()
                    .to_vec();
                engine.original_connect_invoked(owner, &admission).unwrap();
                if primary_observed {
                    // An actual empty first fdget was observed, but this
                    // component deliberately supplies no second-lookup fact.
                    engine
                        .original_connect_selected(owner, &admission, 17, (1, 61, 99, 5, 0))
                        .unwrap();
                }
                let local = Local {
                    arguments: args,
                    raw_arguments: [7, 0x2000, 16, 0, 0, 0],
                    admission: Some(admission.clone()),
                    invoked: true,
                    returned: None,
                };
                engine.original_connect_consumed(owner, &local).unwrap();
                let replacement = if exec {
                    let peer = lifetime::TaskOwner {
                        tid: DetTid::from_raw(62),
                        mm: owner.mm,
                    };
                    engine
                        .lifetime
                        .share_table(task, peer, owner.thread)
                        .unwrap();
                    let mut allocator = crate::types::FilesIdAllocator::default();
                    let receipt = crate::types::ExecFilesReceipt {
                        caller: peer.tid,
                        process: owner.thread,
                        mm: owner.mm,
                        old_files: admission.arguments.files,
                        new_files: allocator.allocate_exec(peer.tid),
                    };
                    engine.lifetime.prepare_exec(receipt).unwrap();
                    let post_exec_mm = owner.mm.for_exec(owner.thread);
                    let event = crate::scheduler::ExecReconnect {
                        caller: peer.tid,
                        new_leader: owner.thread,
                        detpid: owner.thread,
                        pre_exec_mm: owner.mm,
                        post_exec_mm,
                        child_tid_addr: 0,
                        reconnect_priority: None,
                    };
                    assert!(
                        engine
                            .lifetime
                            .commit_exec(receipt, &event)
                            .unwrap()
                            .is_empty()
                    );
                    Some(lifetime::TaskOwner {
                        tid: owner.thread,
                        mm: post_exec_mm,
                    })
                } else {
                    assert!(engine.lifetime.exit(task).unwrap().is_empty());
                    None
                };
                assert!(!engine.lifetime.is_retired(file));
                assert_eq!(
                    engine
                        .lifetime
                        .original_selection_resolution(ticket)
                        .unwrap(),
                    None
                );
                assert!(
                    engine
                        .original_selection_terminal_retirement(owner, &admission)
                        .is_err()
                );
                assert!(engine.original_connect_final_wait(owner, &local).unwrap());
                assert!(
                    engine
                        .original_selection_terminal_retirement(owner, &admission)
                        .is_err()
                );
                engine
                    .original_connect_dead_retired(owner, &admission, 17)
                    .unwrap();
                assert!(
                    engine
                        .original_selection_terminal_retirement(owner, &admission)
                        .is_err()
                );
                engine
                    .original_connect_pin_released(owner, &admission)
                    .unwrap();
                let proof = engine
                    .original_selection_terminal_retirement(owner, &admission)
                    .unwrap();
                let changed_owner = NetworkStreamOwner {
                    mm: owner.mm.for_exec(owner.thread),
                    ..owner
                };
                assert!(
                    engine
                        .original_selection_terminal_retirement(changed_owner, &admission)
                        .is_err()
                );
                let mut changed = admission.clone();
                changed.call = NetworkStreamCallId(admission.call.0 + 1);
                assert!(
                    engine
                        .original_selection_terminal_retirement(owner, &changed)
                        .is_err()
                );
                assert_eq!(
                    engine
                        .lifetime
                        .retire_original_selection_after_terminal(ticket, proof)
                        .unwrap(),
                    BTreeSet::from([file])
                );
                assert_eq!(
                    engine
                        .lifetime
                        .original_selection_resolution(ticket)
                        .unwrap(),
                    None
                );
                assert_eq!(
                    engine
                        .lifetime
                        .original_selection_candidates(ticket)
                        .unwrap(),
                    candidates
                );
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    None
                );
                let before = engine.lifetime.clone();
                assert!(
                    engine
                        .lifetime
                        .resolve_original_selection(ticket, [None, None])
                        .is_err()
                );
                assert!(
                    engine
                        .lifetime
                        .finish_original_selection(
                            ticket,
                            lifetime::TransportResolution::CancellationAcknowledgedBeforeSubmission
                        )
                        .is_err()
                );
                assert!(
                    engine
                        .lifetime
                        .retire_original_selection_after_terminal(ticket, proof)
                        .is_err()
                );
                assert_eq!(engine.lifetime, before);
                engine.finish_original_connect(owner, &admission).unwrap();
                if let Some(owner) = replacement {
                    engine.lifetime.exit(owner).unwrap();
                }
                assert_eq!(
                    engine.lifetime.finish(),
                    Err(lifetime::LifetimeError::OutstandingOwners)
                );
            }
        }
    }

    #[test]
    fn actual_final_wait_requires_exact_call_and_never_fabricates_native_return() {
        // None covers interrupted injection; Some covers a real pre-body error
        // with no authenticated selection. Both stay unresolved while alive.
        for returned in [None, Some(-i64::from(libc::EPERM))] {
            let (mut engine, owner, args) = fixture(true);
            let file = args.binding.unwrap().open_file;
            let admission = prepare(&mut engine, owner, args.clone());
            engine.original_connect_invoked(owner, &admission).unwrap();
            if let Some(raw) = returned {
                engine
                    .original_connect_returned(owner, &admission, raw)
                    .unwrap();
            }
            let local = Local {
                arguments: args,
                raw_arguments: [7, 0x2000, 16, 0, 0, 0],
                admission: Some(admission.clone()),
                invoked: true,
                returned,
            };
            engine.original_connect_consumed(owner, &local).unwrap();
            assert!(
                !engine
                    .original_connect_task_terminal(owner, &admission)
                    .unwrap()
            );
            assert!(
                engine
                    .original_connect_dead_retired(owner, &admission, 17)
                    .is_err()
            );
            assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
            let mut reused = local.clone();
            reused.admission.as_mut().unwrap().call = NetworkStreamCallId(admission.call.0 + 1);
            assert!(engine.original_connect_final_wait(owner, &reused).is_err());
            let foreign = NetworkStreamOwner {
                mm: owner.mm.for_exec(owner.thread),
                ..owner
            };
            assert!(engine.original_connect_final_wait(foreign, &local).is_err());
            assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
            assert!(engine.original_connect_final_wait(owner, &local).unwrap());
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                returned
            );
            assert!(
                engine
                    .original_connect_returned(owner, &admission, 0)
                    .is_err()
            );
            assert!(
                engine
                    .original_connect_dead_retired(owner, &admission, 18)
                    .is_err()
            );
            assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 1, 1));
            engine
                .original_connect_dead_retired(owner, &admission, 17)
                .unwrap();
            assert_eq!(engine.native_capture_fixture_counts(file), (1, 0, 0, 1));
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                returned
            );
            assert!(engine.finish_original_connect(owner, &admission).is_err());
            engine
                .original_connect_pin_released(owner, &admission)
                .unwrap();
            engine.finish_original_connect(owner, &admission).unwrap();
            assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
        }
    }
    #[test]
    fn final_wait_before_delayed_begin_fences_only_its_exact_owner() {
        let (mut engine, owner, args) = fixture(true);
        let local = Local {
            arguments: args.clone(),
            raw_arguments: [7, 0x2000, 16, 0, 0, 0],
            admission: None,
            invoked: false,
            returned: None,
        };
        assert!(!engine.original_connect_final_wait(owner, &local).unwrap());
        assert!(matches!(engine.begin_original_connect(owner, args),
            Err(NetworkReplayError::StreamOwnerGone(actual)) if actual == owner));
        assert!(!engine.gone_stream_owners.contains(&NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        }));
        assert!(engine.stream_calls.is_empty());
    }
    #[test]
    fn consuming_after_terminal_retirement_is_idempotent_and_preserves_other_calls() {
        for reply_delivered in [false, true] {
            let (mut engine, owner, args) = fixture(true);
            let file = args.binding.unwrap().open_file;
            let admission = prepare(&mut engine, owner, args);
            if reply_delivered {
                engine.original_connect_invoked(owner, &admission).unwrap();
            }
            let local = Local {
                arguments: admission.arguments.clone(),
                raw_arguments: [7, 0x2000, 16, 0, 0, 0],
                admission: reply_delivered.then_some(admission.clone()),
                invoked: reply_delivered,
                returned: None,
            };
            assert!(engine.original_connect_final_wait(owner, &local).unwrap());
            engine
                .original_connect_dead_retired(owner, &admission, 17)
                .unwrap();
            engine
                .original_connect_pin_released(owner, &admission)
                .unwrap();
            engine.finish_original_connect(owner, &admission).unwrap();
            engine.original_connect_consumed(owner, &local).unwrap();
            assert!(engine.stream_calls.is_empty());
            assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
            assert!(
                engine
                    .fd_publications
                    .values()
                    .all(|p| p.active.is_none() && p.pending.is_none())
            );

            let thread = DetTid::from_raw(62);
            let other = NetworkStreamOwner {
                thread,
                mm: MmId::initial(thread),
            };
            let files = engine.fd_publication_fixture_register(other, None);
            let active = engine
                .begin_original_connect(
                    other,
                    Arguments {
                        kind: Kind::Connect,
                        operation: ExternalOpId::new(thread, 11),
                        files,
                        binding: None,
                        fd: 7,
                        address: 0x2000,
                        length: 16,
                        original_count: 0,
                    },
                )
                .unwrap();
            let before = format!("{engine:?}");
            engine.original_connect_consumed(owner, &local).unwrap();
            assert_eq!(format!("{engine:?}"), before);
            assert_eq!(engine.stream_calls.len(), 1);
            assert_eq!(
                engine
                    .original_connect_cancellation(other, &active)
                    .unwrap(),
                (false, false)
            );
            assert!(
                !engine
                    .original_connect_task_terminal(other, &active)
                    .unwrap()
            );
        }
    }
}
#[cfg(test)]
mod recorded_file_tests {
    use chrono::TimeZone;

    use super::*;
    use crate::network_replay::lifetime::NetworkSlot;
    use crate::types::DetTid;
    use crate::types::MmId;

    // Explicit logical ledger input for a virtual replay FD. No physical census,
    // native result, selected tuple, test capability or provider command is made.
    fn fixture() -> (NetworkReplayEngine, NetworkStreamOwner, Arguments) {
        let thread = DetTid::from_raw(71);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let mut engine = NetworkReplayEngine::replay(NetworkTraceV2 {
            epoch: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
            channels: vec![],
            inputs: vec![],
            outputs: vec![],
        })
        .unwrap();
        assert!(!engine.fd_table_capability());
        let files = engine.fd_publication_fixture_register(owner, None);
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        engine
            .lifetime
            .open(
                task,
                7,
                NetworkSlot {
                    open_file: OpenFileId::new(thread, 1),
                    cloexec: false,
                },
            )
            .unwrap();
        let arguments = Arguments {
            kind: Kind::File(FileOperation::GetFlags),
            operation: ExternalOpId::new(thread, 10),
            files,
            binding: Some(engine.lifetime.descriptor_binding(task, 7).unwrap()),
            fd: 7,
            address: reverie::syscalls::Sysno::fcntl as u64,
            length: libc::F_GETFL,
            original_count: 0,
        };
        (engine, owner, arguments)
    }
    fn no_physical_facts(
        engine: &NetworkReplayEngine,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) {
        let (_, state) = engine.original_call_state(owner, admission.call).unwrap();
        assert!(matches!(
            state.source,
            OriginalResultSource::Recorded { .. }
        ));
        assert_eq!(state.command, None);
        assert_eq!(state.pin, None);
        assert_eq!(state.selected, None);
        assert_eq!(state.backend_result, None);
        assert!(!state.provider_submitted);
        assert!(!state.provider_retired);
        assert!(!state.pin_released);
    }
    #[test]
    fn recorded_read_control_retires_only_the_selected_same_logical_call() {
        let (mut engine, owner, mut args) = fixture();
        args.kind = Kind::Read;
        args.address = 0x1000;
        args.length = 0;
        args.original_count = 4;
        let file = args.binding.unwrap().open_file;
        let admission = engine.begin_recorded_original_file(owner, args).unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .finish_recorded_read_interruption(owner, &admission)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        engine
            .select_recorded_original_file(owner, &admission)
            .unwrap();
        no_physical_facts(&engine, owner, &admission);
        let other = engine
            .acquire_fd_publication(owner, admission.arguments.files)
            .unwrap();
        engine
            .release_empty_fd_publication(owner, other.permit)
            .unwrap();
        let before = format!("{engine:?}");
        let mut wrong = admission.clone();
        wrong.arguments.original_count += 1;
        assert!(
            engine
                .finish_recorded_read_interruption(owner, &wrong)
                .is_err()
        );
        assert!(
            engine
                .finish_recorded_read_interruption(
                    NetworkStreamOwner {
                        mm: owner.mm.for_exec(owner.thread),
                        ..owner
                    },
                    &admission
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert!(engine.stream_calls.contains_key(&admission.call));
        engine
            .finish_recorded_read_interruption(owner, &admission)
            .unwrap();
        assert!(!engine.stream_calls.contains_key(&admission.call));
        assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
        let after = format!("{engine:?}");
        assert!(
            engine
                .finish_recorded_read_interruption(owner, &admission)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), after);
        assert!(!engine.fd_table_capability());
    }
    #[test]
    fn recorded_read_control_refuses_a_selected_flags_operation() {
        let (mut engine, owner, args) = fixture();
        let admission = engine.begin_recorded_original_file(owner, args).unwrap();
        engine
            .select_recorded_original_file(owner, &admission)
            .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .finish_recorded_read_interruption(owner, &admission)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        no_physical_facts(&engine, owner, &admission);
        engine
            .finish_recorded_original_file(owner, &admission, 0)
            .unwrap();
    }

    #[test]
    fn recorded_virtual_reader_transfer_never_manufactures_physical_slot_authority() {
        let (mut engine, owner, args) = fixture();
        let file = args.binding.unwrap().open_file;
        let NetworkFdReadBegin::Admitted(read) =
            engine.begin_fd_read(owner, args.files, args.fd).unwrap()
        else {
            panic!("unexpected recovery")
        };
        let read = *read;
        let before = format!("{engine:?}");
        assert!(
            engine
                .begin_original_file_from_read(
                    owner,
                    args.clone(),
                    read.clone(),
                    crate::OriginalFileExecution::Native
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        let next_lease = engine.next_stream_lease;
        let admission = engine
            .begin_original_file_from_read(
                owner,
                args,
                read.clone(),
                crate::OriginalFileExecution::Recorded,
            )
            .unwrap();
        assert_eq!(engine.next_stream_lease, next_lease);
        no_physical_facts(&engine, owner, &admission);
        assert!(engine.finish_fd_read(owner, read).is_err());
        assert!(
            engine
                .original_connect_provider_submitted(owner, &admission)
                .is_err()
        );
        engine
            .select_recorded_original_file(owner, &admission)
            .unwrap();
        no_physical_facts(&engine, owner, &admission);
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 0, 1));
        // Explicit component Return input; connected Recorder/Replayer controls
        // retain their actual serialized event-consumption path unchanged.
        engine
            .finish_recorded_original_file(owner, &admission, i64::from(libc::O_NONBLOCK))
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
        assert!(!engine.fd_table_capability());
    }

    #[test]
    fn recorded_virtual_slot_uses_logical_call_without_native_capability_or_receipts() {
        let (mut engine, owner, args) = fixture();
        let file = args.binding.unwrap().open_file;
        let admission = engine.begin_recorded_original_file(owner, args).unwrap();
        no_physical_facts(&engine, owner, &admission);
        assert!(
            engine
                .original_connect_provider_submitted(owner, &admission)
                .is_err()
        );
        assert!(engine.original_connect_invoked(owner, &admission).is_err());
        assert!(
            engine
                .original_connect_returned(owner, &admission, 0)
                .is_err()
        );
        assert!(
            engine
                .finish_recorded_original_file(owner, &admission, 0)
                .is_err()
        );
        assert!(
            engine
                .acquire_fd_publication(owner, admission.arguments.files)
                .is_err()
        );
        engine
            .select_recorded_original_file(owner, &admission)
            .unwrap();
        no_physical_facts(&engine, owner, &admission);
        let unrelated = engine
            .acquire_fd_publication(owner, admission.arguments.files)
            .unwrap();
        engine
            .release_empty_fd_publication(owner, unrelated.permit)
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 0, 1));
        engine
            .finish_recorded_original_file(owner, &admission, 0)
            .unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
        assert!(!engine.fd_table_capability());
    }
    #[test]
    fn recorded_return_wrong_owner_call_and_value_preserve_exact_custody() {
        let (mut engine, owner, args) = fixture();
        let admission = engine.begin_recorded_original_file(owner, args).unwrap();
        engine
            .select_recorded_original_file(owner, &admission)
            .unwrap();
        let before = format!("{engine:?}");
        let mut wrong = admission.clone();
        wrong.arguments.fd += 1;
        assert!(
            engine
                .finish_recorded_original_file(owner, &wrong, 0)
                .is_err()
        );
        let wrong_owner = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        assert!(
            engine
                .finish_recorded_original_file(wrong_owner, &admission, 0)
                .is_err()
        );
        for value in [-4096, i64::from(i32::MAX) + 1] {
            assert!(
                engine
                    .finish_recorded_original_file(owner, &admission, value)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
        }
        no_physical_facts(&engine, owner, &admission);
        engine
            .finish_recorded_original_file(owner, &admission, -i64::from(libc::EBADF))
            .unwrap();
    }
    #[test]
    fn recorded_call_keeps_last_logical_file_after_owner_exit_until_consuming_ack() {
        let (mut engine, owner, args) = fixture();
        let file = args.binding.unwrap().open_file;
        let admission = engine.begin_recorded_original_file(owner, args).unwrap();
        engine
            .select_recorded_original_file(owner, &admission)
            .unwrap();
        let before = format!("{engine:?}");
        engine.retire_fd_table_owner(NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        });
        assert_eq!(format!("{engine:?}"), before);
        engine.retire_fd_table_owner(owner);
        assert_eq!(engine.native_capture_fixture_counts(file), (1, 1, 0, 1));
        assert!(engine.take_lifetime_retired_ports().is_empty());
        assert!(engine.finish_fd_mutations().is_err());
        let local = Local {
            arguments: admission.arguments.clone(),
            raw_arguments: [0; 6],
            admission: Some(admission),
            invoked: false,
            returned: None,
        };
        engine.original_connect_consumed(owner, &local).unwrap();
        assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
        assert_eq!(
            engine.take_lifetime_retired_ports(),
            [file].into_iter().collect()
        );
        engine.finish_fd_mutations().unwrap();
        assert!(!engine.fd_table_capability());
    }
    #[test]
    fn recorded_delegate_consumption_releases_before_and_after_logical_selection() {
        for selected in [false, true] {
            let (mut engine, owner, args) = fixture();
            let file = args.binding.unwrap().open_file;
            let admission = engine.begin_recorded_original_file(owner, args).unwrap();
            if selected {
                engine
                    .select_recorded_original_file(owner, &admission)
                    .unwrap();
            }
            no_physical_facts(&engine, owner, &admission);
            let local = Local {
                arguments: admission.arguments.clone(),
                raw_arguments: [0; 6],
                admission: Some(admission.clone()),
                invoked: false,
                returned: None,
            };
            assert!(engine.original_connect_final_wait(owner, &local).unwrap());
            engine.original_connect_consumed(owner, &local).unwrap();
            assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
            engine.original_connect_consumed(owner, &local).unwrap();
        }
    }
}

// Reuse the existing explicit paired-history component setup. This issues no
// production capability or native observation and is absent from real builds.
#[cfg(test)]
pub(crate) fn controlled_epoll_metadata_fixture(
    installer: crate::types::DetTid,
) -> (
    NetworkReplayEngine,
    NetworkStreamOwner,
    Admission,
    std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    crate::tool_local::FileMetadata,
    detcore_model::fd::NetworkFdSlotReplacement,
) {
    let (mut engine, owner, admission, metadata, old) = tests::epoll_metadata_fixture();
    assert!(
        engine
            .lifetime
            .close_retained_binding(old)
            .unwrap()
            .is_empty()
    );
    assert!(metadata.lock().unwrap().remove_descriptor_binding(old));
    let (mut candidate, change) = metadata
        .lock()
        .unwrap()
        .prepare_original_installation_typed(
            installer,
            7,
            nix::fcntl::OFlag::empty(),
            crate::fd::FdType::Socket,
            None,
        )
        .unwrap();
    candidate
        .bind_native_installation(
            change.after.unwrap().binding,
            crate::network_runtime::original_installation::FileIdentity::controlled_fixture(5, 37),
        )
        .unwrap();
    assert!(candidate.acknowledge_network_installations(&[change]));
    (engine, owner, admission, metadata, candidate, change)
}

impl NetworkReplayEngine {
    pub(super) fn original_native_sent(
        &self, owner: NetworkStreamOwner, admission: &Admission,
    ) -> Result<i64, NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments || original.arguments.kind != Kind::Sendto
            || state.abandoned || state.final_wait || original.final_wait || original.uninvoked
            || !original.backend_entered || !original.provider_submitted || !original.provider_retired
            || !original.pin_released || original.selected.is_none() || original.cancel_requested
            || original.consumed || state.capture_publication.is_some() || state.capture_control.is_some()
            || original.external_grant.is_some()
            || admission.arguments.binding.map(|b| b.open_file) != state.open_file
        { return Err(protocol("Sendto publication lacks original result/selection/retirement")); }
        original.backend_result.ok_or_else(|| protocol("Sendto lacks actual backend result"))
    }

    pub(super) fn original_native_connected(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(OpenFileId, i64), NetworkReplayError> {
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.arguments.kind != Kind::Connect
            || state.abandoned
            || state.final_wait
            || original.final_wait
            || original.uninvoked
            || original.backend_result.is_none_or(|raw| raw != 0 && raw != -i64::from(libc::EINPROGRESS))
            || !original.provider_submitted
            || !original.provider_retired
            || !original.pin_released
            || original.selected.is_none()
            || original.cancel_requested
            || original.consumed
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
            || original.external_grant != Some(admission.arguments.operation)
            || admission.arguments.binding.map(|b| b.open_file) != state.open_file
        {
            return Err(protocol(
                "V4 Connect publication lacks exact original backend result and retirement",
            ));
        }
        state
            .open_file
            .map(|file| (file, original.backend_result.unwrap()))
            .ok_or_else(|| protocol("V4 Connect has no exact selected OFD"))
    }
}

impl NetworkReplayEngine {
    pub(in crate::network_replay) fn begin_shared_original_send_from_read(
        &mut self, owner:NetworkStreamOwner, arguments:Arguments, read:NetworkFdReadAdmission,
    )->Result<Admission,NetworkReplayError> {
        if !self.fd_table_capability() || !self.uses_shared_mm_attempts()
            || self.mode()!=NetworkEngineMode::Record
            || !matches!(arguments.kind,Kind::BlockingSendto{..}) || read.external_grant.is_some() {
            return Err(protocol("shared Sendto changed closed original transfer"));
        }
        self.begin_original_call_with_read(owner,arguments,OriginalResultSource::Native,Some(read))
    }
    pub(in crate::network_replay) fn original_shared_native_sent(
        &self, owner:NetworkStreamOwner, admission:&Admission,
    )->Result<i64,NetworkReplayError> {
        let (state,original)=self.original_connect_state(owner,admission.call)?;
        if original.arguments!=admission.arguments || !matches!(original.arguments.kind,Kind::BlockingSendto{..})
            || state.abandoned || state.final_wait || original.final_wait || original.uninvoked
            || !original.backend_entered || !original.provider_submitted || !original.provider_retired
            || !original.pin_released || original.selected.is_none() || original.cancel_requested
            || original.consumed || state.capture_publication.is_some() || state.capture_control.is_some()
            || original.external_grant.is_some() || admission.arguments.binding.map(|b|b.open_file)!=state.open_file {
            return Err(protocol("shared Sendto lacks original selection/result/retirement"));
        }
        original.backend_result.filter(|v|*v>0).ok_or_else(||protocol("shared Sendto requires positive original return"))
    }
}
