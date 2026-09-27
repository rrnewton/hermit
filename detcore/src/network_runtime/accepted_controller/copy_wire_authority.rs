//! Receive grammar permission issued only from this Controller's retained
//! outgoing preparation and actual selection receipt. Raw bytes are never a
//! version selector. These capabilities do not authorize guest publication.

use std::io;
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;

use super::Controller;
use super::Effect;
use super::State;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::original_connect::Kind;
use crate::network_runtime::OriginalSelection;
use crate::network_runtime::PidfdIdentity;
use crate::network_runtime::ProviderWireFormat;
use crate::network_runtime::accepted_provider::AuxiliaryRole;
use crate::network_runtime::accepted_provider::CallStatus;
use crate::network_runtime::accepted_provider::Reply;
use crate::network_runtime::accepted_provider::Request;
use crate::network_runtime::accepted_transport::Envelope;
use crate::network_runtime::accepted_transport::Operation;

/// One original acknowledged preparation. No clone, deserialization, numeric
/// pid constructor, or public field can manufacture a second issuance.
#[derive(Debug)]
pub(crate) struct PreparedCopyAuthority {
    controller: std::sync::Arc<()>,
    run: [u8; 16],
    wire: ProviderWireFormat,
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    prepared: u64,
    command: u64,
    kind: CopyKind,
    body: Vec<u8>,
    task: OwnedFd,
    task_identity: PidfdIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyKind {
    Read,
    Helper(AuxiliaryRole),
}
impl CopyKind {
    fn key(self, call: NetworkStreamCallId) -> Effect {
        match self {
            Self::Read => Effect::PrepareOriginalConnect(call),
            Self::Helper(_) => Effect::PrepareOriginalFileObservation(call),
        }
    }
    fn operation(self) -> u64 {
        match self {
            Self::Read => 11,
            Self::Helper(role) => role.operation(),
        }
    }
    fn flags(self) -> i32 {
        match self {
            Self::Read => 0,
            Self::Helper(role) => role.operands().1,
        }
    }
}

/// Same capability after an actual same-command selection receipt. Holding its
/// task description prevents numeric PID reuse from changing this authority.
#[derive(Debug)]
pub(crate) struct CopyWireAuthority {
    prepared: PreparedCopyAuthority,
    selection: OriginalSelection,
}
impl CopyWireAuthority {
    pub(crate) fn version(&self) -> u64 {
        self.prepared.wire.copy_version()
    }
    pub(crate) fn operation(&self) -> u64 {
        self.prepared.kind.operation()
    }
    pub(crate) fn flags(&self) -> i32 {
        self.prepared.kind.flags()
    }
    pub(crate) fn validate_selection(&self, selection: &OriginalSelection) -> io::Result<()> {
        if *selection != self.selection
            || PidfdIdentity::read(&self.prepared.task)? != self.prepared.task_identity
        {
            return Err(invalid(
                "copy bytes changed their authenticated selected file or held task",
            ));
        }
        Ok(())
    }
}

/// Positive dead-task physical custody with no selected fd_calls row. This
/// token has no parser/version/result API; only an empty actual terminal cut
/// can settle it, and the failed-run semantic outcome remains unchanged.
#[derive(Debug)]
pub(crate) struct EmptyCopyTerminalAuthority {
    prepared: PreparedCopyAuthority,
    terminal: crate::network_runtime::accepted_provider::OriginalTerminal,
}
impl EmptyCopyTerminalAuthority {
    pub(crate) fn validate_binding(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        command: u64,
        terminal: &crate::network_runtime::accepted_provider::OriginalTerminal,
    ) -> io::Result<()> {
        if self.prepared.owner != owner
            || self.prepared.call != call
            || self.prepared.command != command
            || *terminal != self.terminal
            || PidfdIdentity::read(&self.prepared.task)? != self.prepared.task_identity
        {
            return Err(invalid(
                "empty terminal changed original Call, task or retained diagnostic receipt",
            ));
        }
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}
fn status(value: &CallStatus, name: &str) -> io::Result<()> {
    if value.returned != 0 || value.errno.is_some() || value.operation != name {
        return Err(invalid(
            "copy authority lacks the exact successful provider operation",
        ));
    }
    Ok(())
}
fn outgoing<'a>(
    state: &'a State,
    run: [u8; 16],
    owner: NetworkStreamOwner,
    sequence: u64,
) -> io::Result<(&'a Envelope, &'a [OwnedFd], &'a [u8])> {
    let (envelope, rights, response) = state.session.acknowledged_outgoing_request(sequence)?;
    if sequence == 0
        || envelope.sequence != sequence
        || envelope.run != run
        || envelope.owner != Some(owner)
        || envelope.accept.is_some()
    {
        return Err(invalid(
            "copy authority changed its actual outgoing request ownership",
        ));
    }
    let mut entries = state
        .requests
        .0
        .values()
        .filter(|entry| entry.sequence == Some(sequence));
    let entry = entries
        .next()
        .ok_or_else(|| invalid("copy request lost its retained effect"))?;
    if entries.next().is_some()
        || entry.owner != owner
        || entry.operation != envelope.operation
        || entry.body != envelope.body
        || entry.error.is_some()
    {
        return Err(invalid(
            "copy authority changed retained effect or request bytes",
        ));
    }
    Ok((envelope, rights, response))
}

fn preparation(
    envelope: &Envelope,
    response: &[u8],
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    command: u64,
) -> io::Result<CopyKind> {
    let Reply::Prepared(ack) = serde_json::from_slice(response)? else {
        return Err(invalid("copy authority has no prepared acknowledgement"));
    };
    if command == 0 || ack.raw != command || call.native_command_call() == 0 {
        return Err(invalid("copy authority changed the native command or Call"));
    }
    match serde_json::from_slice(&envelope.body)? {
        Request::PrepareOriginalConnect {
            kind: Kind::Read,
            call: actual,
            mm,
            fd,
            address,
            length,
            original_count,
        } if envelope.operation == Operation::PrepareOriginalConnect
            && actual == call.native_command_call()
            && mm == owner.mm.generation()
            && fd >= 0
            && Kind::Read.valid_operands(address, length, original_count) =>
        {
            status(&ack.status, "ap_prepare_original_read")?;
            Ok(CopyKind::Read)
        }
        Request::PrepareOriginalFileObservation {
            call: actual,
            mm,
            fd,
            role,
        } if envelope.operation == Operation::PrepareOriginalFileObservation
            && actual == call.native_command_call()
            && mm == owner.mm.generation()
            && fd >= 0
            && role.is_receive()
            && role.valid() =>
        {
            status(&ack.status, role.prepare_name())?;
            Ok(CopyKind::Helper(role))
        }
        _ => Err(invalid(
            "copy authority request is not the exact original Read or receive helper",
        )),
    }
}

impl Controller {
    pub(in crate::network_runtime) fn prepared_copy_authority(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        prepared: u64,
        command: u64,
    ) -> io::Result<PreparedCopyAuthority> {
        let wire = self
            .wire_format
            .ok_or_else(|| invalid("copy authority lacks authenticated startup grammar"))?;
        let mut state = self.state.lock().unwrap();
        let (envelope, rights, response) = outgoing(&state, self.run, owner, prepared)?;
        let kind = preparation(envelope, response, owner, call, command)?;
        let [task] = rights else {
            return Err(invalid(
                "copy preparation lacks its one retained task description",
            ));
        };
        let task_identity = PidfdIdentity::read(task)?;
        let task = task.as_fd().try_clone_to_owned()?;
        let body = envelope.body.clone();
        let entry = state
            .requests
            .0
            .get_mut(&kind.key(call))
            .ok_or_else(|| invalid("copy preparation has no exact retained effect key"))?;
        if entry.sequence != Some(prepared) || entry.copy_authority_issued {
            return Err(invalid(
                "copy authority was already issued or changed its preparation",
            ));
        }
        entry.copy_authority_issued = true;
        Ok(PreparedCopyAuthority {
            controller: self.copy_authority_owner.clone(),
            run: self.run,
            wire,
            owner,
            call,
            prepared,
            command,
            kind,
            body,
            task,
            task_identity,
        })
    }

    /// Unlike selected-copy binding, this permits no byte interpretation.
    /// The caller must retain this token and the actual raw-first empty
    /// ThreadTerminal cut together before releasing physical empty custody.
    pub(in crate::network_runtime) fn empty_copy_terminal_authority(
        &self,
        prepared: PreparedCopyAuthority,
        terminal_sequence: u64,
    ) -> io::Result<EmptyCopyTerminalAuthority> {
        use crate::network_runtime::accepted_provider::CommandResult;
        use crate::network_runtime::accepted_provider::OriginalResult;
        use crate::network_runtime::accepted_provider_ffi as ffi;
        if prepared.kind != CopyKind::Read
            || !std::sync::Arc::ptr_eq(&prepared.controller, &self.copy_authority_owner)
            || prepared.run != self.run
            || Some(prepared.wire) != self.wire_format
            || terminal_sequence <= prepared.prepared
            || PidfdIdentity::read(&prepared.task)? != prepared.task_identity
        {
            return Err(invalid(
                "empty terminal changed startup, preparation or held task",
            ));
        }
        let state = self.state.lock().unwrap();
        let (original, rights, ack) =
            outgoing(&state, self.run, prepared.owner, prepared.prepared)?;
        if original.body != prepared.body
            || preparation(
                original,
                ack,
                prepared.owner,
                prepared.call,
                prepared.command,
            )? != CopyKind::Read
            || rights.len() != 1
            || PidfdIdentity::read(&rights[0])? != prepared.task_identity
            || !state
                .requests
                .0
                .get(&Effect::PrepareOriginalConnect(prepared.call))
                .is_some_and(|entry| {
                    entry.sequence == Some(prepared.prepared) && entry.copy_authority_issued
                })
        {
            return Err(invalid(
                "empty terminal lost its original preparation capability",
            ));
        }
        let Request::PrepareOriginalConnect { original_count, .. } =
            serde_json::from_slice(&prepared.body)?
        else {
            return Err(invalid("empty terminal preparation changed kind"));
        };
        let (envelope, rights, bytes) =
            outgoing(&state, self.run, prepared.owner, terminal_sequence)?;
        let Request::TerminateOriginalConnect {
            call,
            command,
            prepared_request,
            selected_request,
            ..
        } = serde_json::from_slice(&envelope.body)?
        else {
            return Err(invalid("empty terminal has no actual termination request"));
        };
        if envelope.operation != Operation::TerminateOriginalConnect
            || !rights.is_empty()
            || call != prepared.call.native_command_call()
            || command != prepared.command
            || prepared_request != prepared.prepared
            || selected_request <= prepared.prepared
            || selected_request >= terminal_sequence
        {
            return Err(invalid(
                "empty terminal changed exact terminal request identity",
            ));
        }
        let Reply::OriginalTerminated(observed) = serde_json::from_slice(bytes)? else {
            return Err(invalid("empty terminal has no typed diagnostic receipt"));
        };
        status(&observed.status, "ap_retire_dead_original")?;
        let query = state
            .requests
            .0
            .get(&Effect::AwaitOriginalSelection(prepared.call))
            .ok_or_else(|| invalid("empty terminal lost its original selection query"))?;
        if query.sequence != Some(selected_request)
            || query.owner != prepared.owner
            || query.operation != Operation::AwaitOriginalSelection
            || query.error.is_some()
            || !matches!(serde_json::from_slice::<Request>(&query.body),
                Ok(Request::AwaitOriginalSelection { call:c, command:k, prepared_request:p })
                    if c == call && k == command && p == prepared_request)
        {
            return Err(invalid("empty terminal changed retained selection query"));
        }
        if let Some(selected) = state.session.response(selected_request)? {
            if !matches!(serde_json::from_slice::<Reply>(selected), Ok(Reply::OriginalTerminated(ref same)) if *same == observed)
            {
                return Err(invalid(
                    "empty terminal cannot replace an observed selection or different receipt",
                ));
            }
        }
        let raw = observed.raw;
        let zero: OriginalResult = ffi::OriginalResult::default().into();
        if raw.call != call
            || raw.task_absent != 1
            || raw.fd_call_present != 0
            || raw.original != zero
            || raw.command.command != command
            || raw.command.operation != 11
            || raw.command.original_count != original_count
            || raw.command.reserved != 0
        {
            return Err(invalid(
                "empty terminal lacks exact absent-row and zero-result diagnostic proof",
            ));
        }
        match raw.command.phase {
            2 => {
                let reserved: CommandResult = ffi::CommandResult {
                    command,
                    operation: 11,
                    phase: 2,
                    original_count,
                    ..Default::default()
                }
                .into();
                if raw.command != reserved {
                    return Err(invalid(
                        "READY terminal changed exact reserved native command",
                    ));
                }
            }
            1 | 3 => {
                let provider = u64::from_le_bytes(self.run[..8].try_into().unwrap());
                if raw.command.task == 0
                    || raw.command.start_boottime == 0
                    || provider == 0
                    || raw.command.identity.provider != provider
                {
                    return Err(invalid(
                        "running/done empty terminal lost actual task or provider identity",
                    ));
                }
            }
            _ => {
                return Err(invalid(
                    "empty terminal command has an unknown native phase",
                ));
            }
        }
        Ok(EmptyCopyTerminalAuthority {
            prepared,
            terminal: raw,
        })
    }

    pub(in crate::network_runtime) fn bind_copy_authority(
        &self,
        prepared: PreparedCopyAuthority,
        selected: u64,
    ) -> io::Result<CopyWireAuthority> {
        if !std::sync::Arc::ptr_eq(&prepared.controller, &self.copy_authority_owner)
            || prepared.run != self.run
            || Some(prepared.wire) != self.wire_format
            || selected <= prepared.prepared
            || PidfdIdentity::read(&prepared.task)? != prepared.task_identity
        {
            return Err(invalid(
                "copy selection changed startup, request order or held task",
            ));
        }
        let state = self.state.lock().unwrap();
        let (original, rights, ack) =
            outgoing(&state, self.run, prepared.owner, prepared.prepared)?;
        if original.body != prepared.body
            || preparation(
                original,
                ack,
                prepared.owner,
                prepared.call,
                prepared.command,
            )? != prepared.kind
            || rights.len() != 1
            || PidfdIdentity::read(&rights[0])? != prepared.task_identity
            || !state
                .requests
                .0
                .get(&prepared.kind.key(prepared.call))
                .is_some_and(|entry| {
                    entry.sequence == Some(prepared.prepared) && entry.copy_authority_issued
                })
        {
            return Err(invalid(
                "copy selection lost its original prepared capability",
            ));
        }
        let (envelope, rights, bytes) = outgoing(&state, self.run, prepared.owner, selected)?;
        if !rights.is_empty() {
            return Err(invalid(
                "copy selection introduced new task or file authority",
            ));
        }
        let request: Request = serde_json::from_slice(&envelope.body)?;
        let reply: Reply = serde_json::from_slice(bytes)?;
        let call = prepared.call.native_command_call();
        let selection = match (prepared.kind, envelope.operation, request, reply) {
            (
                CopyKind::Read,
                Operation::AwaitOriginalSelection,
                Request::AwaitOriginalSelection {
                    call: c,
                    command,
                    prepared_request,
                },
                Reply::OriginalSelection(observed),
            ) if c == call
                && command == prepared.command
                && prepared_request == prepared.prepared =>
            {
                status(&observed.status, "ap_read_original_selection")?;
                observed.raw
            }
            (
                CopyKind::Helper(role),
                Operation::CollectOriginalFileObservation,
                Request::CollectOriginalFileObservation {
                    call: c,
                    command,
                    prepared_request,
                    role: actual,
                },
                Reply::OriginalFileObservation {
                    selection,
                    effect: Some(effect),
                },
            ) if c == call
                && command == prepared.command
                && prepared_request == prepared.prepared
                && actual == role =>
            {
                status(&selection.status, "ap_read_original_selection")?;
                status(&effect.status, "ap_collect_original_connect")?;
                let raw = &effect.raw;
                if selection.raw != raw.original.selection
                    || raw.command.command != command
                    || raw.command.operation != role.operation()
                    || raw.command.phase != 1
                    || raw.command.task != selection.raw.task
                    || raw.command.start_boottime != selection.raw.task_start
                    || raw.command.identity.provider != selection.raw.provider
                    || raw.command.original_count != selection.raw.original_count
                    || raw.command.reserved != 0
                {
                    return Err(invalid(
                        "copy helper completion changed actual command/selection",
                    ));
                }
                selection.raw
            }
            (
                CopyKind::Read,
                Operation::TerminateOriginalConnect,
                Request::TerminateOriginalConnect {
                    call: c,
                    command,
                    prepared_request,
                    selected_request,
                    ..
                },
                Reply::OriginalTerminated(terminal),
            ) if c == call
                && command == prepared.command
                && prepared_request == prepared.prepared
                && selected_request > prepared.prepared
                && selected_request < selected =>
            {
                status(&terminal.status, "ap_retire_dead_original")?;
                let query = state
                    .requests
                    .0
                    .get(&Effect::AwaitOriginalSelection(prepared.call))
                    .ok_or_else(|| {
                        invalid("terminal copy prefix lost its original selection request")
                    })?;
                if query.sequence != Some(selected_request)
                    || query.owner != prepared.owner
                    || query.operation != Operation::AwaitOriginalSelection
                    || query.error.is_some()
                    || !matches!(serde_json::from_slice::<Request>(&query.body),
                        Ok(Request::AwaitOriginalSelection { call: c, command: cmd, prepared_request: p })
                            if c == call && cmd == command && p == prepared_request)
                {
                    return Err(invalid(
                        "terminal copy prefix changed the retained selection query",
                    ));
                }
                let raw = terminal.raw;
                if raw.call != call
                    || raw.command.command != command
                    || raw.command.operation != 11
                    || raw.task_absent != 1
                    || raw.fd_call_present != 1
                    || raw.command.task != raw.original.selection.task
                    || raw.command.start_boottime != raw.original.selection.task_start
                    || raw.command.identity.provider != raw.original.selection.provider
                    || raw.command.original_count != raw.original.selection.original_count
                {
                    return Err(invalid(
                        "terminal copy prefix lacks its actual task/command/selection receipt",
                    ));
                }
                // Diagnostic prefix only: this token proves no syscall completion.
                raw.original.selection
            }
            _ => {
                return Err(invalid(
                    "copy selection is not an acknowledged same-command native receipt",
                ));
            }
        };
        let expected_provider = u64::from_le_bytes(self.run[..8].try_into().unwrap());
        if selection.command != prepared.command
            || selection.call != call
            || selection.owner_mm != prepared.owner.mm.generation()
            || selection.provider != expected_provider
            || expected_provider == 0
            || selection.task == 0
            || selection.task_start == 0
            || selection.table == 0
            || (selection.file == 0 && prepared.kind != CopyKind::Read)
            || selection.ready != 1
            || selection.fdput_flags & !1 != 0
        {
            return Err(invalid(
                "copy selection changed retained Call, provider or file identity",
            ));
        }
        match serde_json::from_slice(&prepared.body)? {
            Request::PrepareOriginalConnect {
                fd,
                address,
                length,
                original_count,
                ..
            } => {
                if selection.requested_fd != fd
                    || selection.user_address != address
                    || selection.address_length != length
                    || selection.original_count != original_count
                {
                    return Err(invalid("copy selection changed original Read operands"));
                }
            }
            Request::PrepareOriginalFileObservation { fd, role, .. } => {
                role.check_selection(
                    &selection,
                    call,
                    prepared.owner.mm.generation(),
                    fd,
                    prepared.command,
                )?;
            }
            _ => return Err(invalid("copy preparation changed kind after issuance")),
        }
        Ok(CopyWireAuthority {
            prepared,
            selection,
        })
    }
}

/// Parser controls use the production issuance path and real private transport
/// plus an actual retained pidfd. The simulated service supplies its receipt;
/// this is not a native-provider qualification or a production constructor.
#[cfg(test)]
pub(crate) fn controlled_copy_authority(
    wire: ProviderWireFormat,
    owner: NetworkStreamOwner,
    operation: u64,
    selection: OriginalSelection,
) -> io::Result<CopyWireAuthority> {
    controlled_copy_authority_with_preparation(wire, owner, operation, selection, Ok)
}

/// The custody control retains/takes the same actual issued preparation before
/// returning it to the unchanged production binder. No alternate constructor.
#[cfg(test)]
pub(crate) fn controlled_copy_authority_with_preparation(
    wire: ProviderWireFormat,
    owner: NetworkStreamOwner,
    operation: u64,
    selection: OriginalSelection,
    visit: impl FnOnce(PreparedCopyAuthority) -> io::Result<PreparedCopyAuthority>,
) -> io::Result<CopyWireAuthority> {
    let mut fixture = tests::Fixture::new(Some(wire), owner, operation, selection)?;
    let sequence = fixture.prepare(true, None)?;
    let token = fixture.controller.prepared_copy_authority(
        owner,
        fixture.call,
        sequence,
        fixture.selection.command,
    )?;
    let token = visit(token)?;
    let selected = fixture.select(None)?;
    fixture.controller.bind_copy_authority(token, selected)
}

#[cfg(test)]
pub(crate) fn controlled_empty_copy_terminal_authority(
    wire: ProviderWireFormat,
    owner: NetworkStreamOwner,
    selection_for_prepare: OriginalSelection,
    phase: u64,
    visit: impl FnOnce(PreparedCopyAuthority) -> io::Result<PreparedCopyAuthority>,
) -> io::Result<(
    EmptyCopyTerminalAuthority,
    crate::network_runtime::accepted_provider::OriginalTerminal,
)> {
    let mut fixture = tests::Fixture::new(Some(wire), owner, 11, selection_for_prepare)?;
    let sequence = fixture.prepare(true, None)?;
    let token = fixture.controller.prepared_copy_authority(
        owner,
        fixture.call,
        sequence,
        fixture.selection.command,
    )?;
    let token = visit(token)?;
    let (terminal, raw) = fixture.empty_terminal(phase, |_| {})?;
    let authority = fixture
        .controller
        .empty_copy_terminal_authority(token, terminal)?;
    Ok((authority, raw))
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;

    use super::*;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider::OriginalEffect;
    use crate::network_runtime::accepted_provider::OriginalTerminal;
    use crate::network_runtime::accepted_provider::ReceiveKind;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    use crate::network_runtime::accepted_transport::AcceptedSession;
    use crate::network_runtime::accepted_transport::Received;

    pub(super) struct Fixture {
        pub(super) controller: Controller,
        peer: AcceptedSession,
        task: OwnedFd,
        pub(super) owner: NetworkStreamOwner,
        pub(super) call: NetworkStreamCallId,
        kind: CopyKind,
        pub(super) selection: OriginalSelection,
        prepared: Option<u64>,
    }
    fn ok(name: &str) -> CallStatus {
        CallStatus {
            operation: name.into(),
            returned: 0,
            errno: None,
        }
    }
    fn owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(31);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn selection() -> OriginalSelection {
        ffi::OriginalSelection {
            command: 91,
            call: 17,
            owner_mm: 0,
            provider: 73,
            task: 31,
            task_start: 100,
            table: 47,
            file: 61,
            user_address: 0x1000,
            fdput_flags: 0,
            ready: 1,
            requested_fd: 88,
            address_length: 0,
            original_count: 2048,
        }
        .into()
    }
    impl Fixture {
        pub(super) fn new(
            wire: Option<ProviderWireFormat>,
            owner: NetworkStreamOwner,
            operation: u64,
            selection: OriginalSelection,
        ) -> io::Result<Self> {
            let mut pair = [-1; 2];
            if unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    pair.as_mut_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let left = unsafe { OwnedFd::from_raw_fd(pair[0]) };
            let right = unsafe { OwnedFd::from_raw_fd(pair[1]) };
            let mut run = [7; 16];
            run[..8].copy_from_slice(&selection.provider.to_le_bytes());
            let raw = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_open,
                    libc::syscall(libc::SYS_gettid),
                    libc::O_EXCL,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let task = unsafe { OwnedFd::from_raw_fd(raw as i32) };
            let kind = match operation {
                11 => CopyKind::Read,
                21 | 22 => CopyKind::Helper(AuxiliaryRole::Receive {
                    kind: if operation == 21 {
                        ReceiveKind::Drain
                    } else {
                        ReceiveKind::Peek
                    },
                    address: selection.user_address,
                    count: selection.original_count,
                    provider: selection.provider,
                    file: selection.file,
                }),
                _ => {
                    return Err(invalid(
                        "test fixture requires an actual supported copy operation",
                    ));
                }
            };
            Ok(Self {
                controller: Controller::with_wire(left, run, wire)?,
                peer: AcceptedSession::new(right, run).map_err(|(error, _)| error)?,
                task,
                owner,
                call: serde_json::from_value(serde_json::json!(selection.call))?,
                kind,
                selection,
                prepared: None,
            })
        }
        fn exchange(&mut self, sequence: u64, response: &Reply) -> io::Result<()> {
            Controller::progress_io(
                &mut self.controller.state.lock().unwrap(),
                &self.controller.changed,
                self.controller.run,
            )?;
            assert!(
                matches!(self.peer.try_receive()?, Some(Received::Request(s)) if s == sequence)
            );
            self.peer.dispatch(sequence, |_, _| {
                serde_json::to_vec(response).map_err(io::Error::other)
            })?;
            assert!(self.peer.try_reply(sequence)?);
            Controller::progress_io(
                &mut self.controller.state.lock().unwrap(),
                &self.controller.changed,
                self.controller.run,
            )?;
            Ok(())
        }
        pub(super) fn prepare(
            &mut self,
            acknowledge: bool,
            changed_status: Option<CallStatus>,
        ) -> io::Result<u64> {
            let request = match self.kind {
                CopyKind::Read => Request::PrepareOriginalConnect {
                    kind: Kind::Read,
                    call: self.selection.call,
                    mm: self.owner.mm.generation(),
                    fd: self.selection.requested_fd,
                    address: self.selection.user_address,
                    length: self.selection.address_length,
                    original_count: self.selection.original_count,
                },
                CopyKind::Helper(role) => Request::PrepareOriginalFileObservation {
                    call: self.selection.call,
                    mm: self.owner.mm.generation(),
                    fd: self.selection.requested_fd,
                    role,
                },
            };
            let sequence =
                self.controller
                    .prepare(self.kind.key(self.call), self.owner, &request, || {
                        Ok(vec![self.task.as_fd().try_clone_to_owned()?])
                    })?;
            self.prepared = Some(sequence);
            if acknowledge {
                let name = match self.kind {
                    CopyKind::Read => "ap_prepare_original_read",
                    CopyKind::Helper(role) => role.prepare_name(),
                };
                self.exchange(
                    sequence,
                    &Reply::Prepared(Observation {
                        status: changed_status.unwrap_or_else(|| ok(name)),
                        raw: self.selection.command,
                    }),
                )?;
            }
            Ok(sequence)
        }
        pub(super) fn select(&mut self, changed: Option<OriginalSelection>) -> io::Result<u64> {
            let selected = changed.unwrap_or_else(|| self.selection.clone());
            let (effect, request, reply) = match self.kind {
                CopyKind::Read => (
                    Effect::AwaitOriginalSelection(self.call),
                    Request::AwaitOriginalSelection {
                        call: self.selection.call,
                        command: self.selection.command,
                        prepared_request: self.prepared.unwrap(),
                    },
                    Reply::OriginalSelection(Observation {
                        status: ok("ap_read_original_selection"),
                        raw: selected,
                    }),
                ),
                CopyKind::Helper(role) => {
                    let mut raw: OriginalEffect = ffi::OriginalEffect::default().into();
                    raw.command.command = self.selection.command;
                    raw.command.operation = role.operation();
                    raw.command.phase = 1;
                    raw.command.task = selected.task;
                    raw.command.start_boottime = selected.task_start;
                    raw.command.identity.provider = selected.provider;
                    raw.command.original_count = selected.original_count;
                    raw.original.selection = selected.clone();
                    (
                        Effect::CollectOriginalFileObservation(self.call),
                        Request::CollectOriginalFileObservation {
                            call: self.selection.call,
                            command: self.selection.command,
                            prepared_request: self.prepared.unwrap(),
                            role,
                        },
                        Reply::OriginalFileObservation {
                            selection: Observation {
                                status: ok("ap_read_original_selection"),
                                raw: selected,
                            },
                            effect: Some(Observation {
                                status: ok("ap_collect_original_connect"),
                                raw,
                            }),
                        },
                    )
                }
            };
            let sequence = self
                .controller
                .prepare(effect, self.owner, &request, || Ok(vec![]))?;
            self.exchange(sequence, &reply)?;
            Ok(sequence)
        }
        pub(super) fn empty_terminal(
            &mut self,
            phase: u64,
            change: impl FnOnce(&mut OriginalTerminal),
        ) -> io::Result<(u64, OriginalTerminal)> {
            let selected_request = self.controller.prepare(
                Effect::AwaitOriginalSelection(self.call),
                self.owner,
                &Request::AwaitOriginalSelection {
                    call: self.selection.call,
                    command: self.selection.command,
                    prepared_request: self.prepared.unwrap(),
                },
                || Ok(vec![]),
            )?;
            if self
                .controller
                .retained_response(selected_request)?
                .is_none()
            {
                Controller::progress_io(
                    &mut self.controller.state.lock().unwrap(),
                    &self.controller.changed,
                    self.controller.run,
                )?;
                assert!(
                    matches!(self.peer.try_receive()?,Some(Received::Request(s)) if s == selected_request)
                );
            }
            let request = Request::TerminateOriginalConnect {
                call: self.selection.call,
                command: self.selection.command,
                prepared_request: self.prepared.unwrap(),
                selected_request,
                failed_request: None,
            };
            let sequence = self.controller.prepare(
                Effect::TerminateOriginalConnect(self.call),
                self.owner,
                &request,
                || Ok(vec![]),
            )?;
            let mut raw: OriginalTerminal = ffi::OriginalTerminal::default().into();
            raw.call = self.selection.call;
            raw.task_absent = 1;
            raw.command.command = self.selection.command;
            raw.command.operation = 11;
            raw.command.phase = phase;
            raw.command.original_count = self.selection.original_count;
            if phase != 2 {
                raw.command.task = self.selection.task;
                raw.command.start_boottime = self.selection.task_start;
                raw.command.identity.provider = self.selection.provider;
            }
            change(&mut raw);
            self.exchange(
                sequence,
                &Reply::OriginalTerminated(Observation {
                    status: ok("ap_retire_dead_original"),
                    raw: raw.clone(),
                }),
            )?;
            Ok((sequence, raw))
        }
        fn terminal(&mut self, missing_file: bool) -> io::Result<u64> {
            let selected_request = self.controller.prepare(
                Effect::AwaitOriginalSelection(self.call),
                self.owner,
                &Request::AwaitOriginalSelection {
                    call: self.selection.call,
                    command: self.selection.command,
                    prepared_request: self.prepared.unwrap(),
                },
                || Ok(vec![]),
            )?;
            // Send the actual query but deliberately do not acknowledge it.
            Controller::progress_io(
                &mut self.controller.state.lock().unwrap(),
                &self.controller.changed,
                self.controller.run,
            )?;
            assert!(
                matches!(self.peer.try_receive()?, Some(Received::Request(s)) if s == selected_request)
            );
            let request = Request::TerminateOriginalConnect {
                call: self.selection.call,
                command: self.selection.command,
                prepared_request: self.prepared.unwrap(),
                selected_request,
                failed_request: None,
            };
            let sequence = self.controller.prepare(
                Effect::TerminateOriginalConnect(self.call),
                self.owner,
                &request,
                || Ok(vec![]),
            )?;
            let mut raw: OriginalTerminal = ffi::OriginalTerminal::default().into();
            raw.call = self.selection.call;
            raw.fd_call_present = u64::from(!missing_file);
            raw.task_absent = 1;
            raw.command.command = self.selection.command;
            raw.command.operation = 11;
            raw.command.phase = 3;
            raw.command.task = self.selection.task;
            raw.command.start_boottime = self.selection.task_start;
            raw.command.identity.provider = self.selection.provider;
            raw.command.original_count = self.selection.original_count;
            raw.original.selection = self.selection.clone();
            assert_eq!(raw.original.complete, 0);
            self.exchange(
                sequence,
                &Reply::OriginalTerminated(Observation {
                    status: ok("ap_retire_dead_original"),
                    raw,
                }),
            )?;
            Ok(sequence)
        }
    }

    #[test]
    fn copy_authority_requires_startup_acknowledgement_and_actual_pidfd() {
        for variant in 0..7 {
            let mut fixture = Fixture::new(
                if variant == 0 {
                    None
                } else {
                    Some(ProviderWireFormat::Abi8Copy5)
                },
                owner(),
                11,
                selection(),
            )
            .unwrap();
            if variant == 2 {
                fixture.task = std::fs::File::open("/dev/null").unwrap().into();
            }
            let altered = match variant {
                3 => Some(CallStatus {
                    returned: -1,
                    errno: Some(libc::EIO),
                    ..ok("ap_prepare_original_read")
                }),
                4 => Some(CallStatus {
                    errno: Some(libc::EIO),
                    ..ok("ap_prepare_original_read")
                }),
                5 => Some(ok("ap_prepare_original_connect")),
                _ => None,
            };
            let sequence = fixture.prepare(variant != 1, altered).unwrap();
            let command = if variant == 6 {
                fixture.selection.command + 1
            } else {
                fixture.selection.command
            };
            assert!(
                fixture
                    .controller
                    .prepared_copy_authority(owner(), fixture.call, sequence, command)
                    .is_err(),
                "variant {variant}"
            );
            assert!(
                !fixture.controller.state.lock().unwrap().requests.0
                    [&fixture.kind.key(fixture.call)]
                    .copy_authority_issued
            );
        }
    }

    #[test]
    fn copy_authority_is_single_issue_and_cannot_move_between_controller_owners() {
        for different_controller in [false, true] {
            let mut fixture = Fixture::new(
                Some(ProviderWireFormat::Abi8Copy5),
                owner(),
                11,
                selection(),
            )
            .unwrap();
            let sequence = fixture.prepare(true, None).unwrap();
            let token = fixture
                .controller
                .prepared_copy_authority(owner(), fixture.call, sequence, fixture.selection.command)
                .unwrap();
            assert!(
                fixture
                    .controller
                    .prepared_copy_authority(
                        owner(),
                        fixture.call,
                        sequence,
                        fixture.selection.command
                    )
                    .is_err()
            );
            let selected = fixture.select(None).unwrap();
            if different_controller {
                let mut other = Fixture::new(
                    Some(ProviderWireFormat::Abi8Copy5),
                    owner(),
                    11,
                    selection(),
                )
                .unwrap();
                other.task = fixture.task.as_fd().try_clone_to_owned().unwrap();
                let p = other.prepare(true, None).unwrap();
                let _other_token = other
                    .controller
                    .prepared_copy_authority(owner(), other.call, p, other.selection.command)
                    .unwrap();
                let s = other.select(None).unwrap();
                assert_eq!(s, selected);
                assert!(other.controller.bind_copy_authority(token, s).is_err());
            } else {
                let bound = fixture
                    .controller
                    .bind_copy_authority(token, selected)
                    .unwrap();
                assert_eq!(bound.version(), 5);
                assert_eq!(bound.operation(), 11);
                assert_eq!(bound.flags(), 0);
                bound.validate_selection(&fixture.selection).unwrap();
            }
        }
    }

    #[test]
    fn copy_authority_binds_every_selected_operand_and_identity() {
        for field in [
            "command",
            "call",
            "owner_mm",
            "provider",
            "task",
            "task_start",
            "table",
            "file",
            "user_address",
            "fdput_flags",
            "ready",
            "requested_fd",
            "address_length",
            "original_count",
        ] {
            let selected = selection();
            let bound = controlled_copy_authority(
                ProviderWireFormat::Abi8Copy5,
                owner(),
                11,
                selected.clone(),
            )
            .unwrap();
            let mut changed = serde_json::to_value(&selected).unwrap();
            changed[field] = serde_json::json!(changed[field].as_u64().unwrap() + 1);
            let changed: OriginalSelection = serde_json::from_value(changed).unwrap();
            assert!(bound.validate_selection(&changed).is_err(), "{field}");
            bound.validate_selection(&selected).unwrap();
        }
        for field in [
            "command",
            "call",
            "owner_mm",
            "provider",
            "user_address",
            "requested_fd",
            "address_length",
            "original_count",
        ] {
            let mut fixture = Fixture::new(
                Some(ProviderWireFormat::Abi8Copy5),
                owner(),
                11,
                selection(),
            )
            .unwrap();
            let sequence = fixture.prepare(true, None).unwrap();
            let token = fixture
                .controller
                .prepared_copy_authority(owner(), fixture.call, sequence, fixture.selection.command)
                .unwrap();
            let mut changed = serde_json::to_value(&fixture.selection).unwrap();
            changed[field] = serde_json::json!(changed[field].as_u64().unwrap() + 1);
            let selected = fixture
                .select(Some(serde_json::from_value(changed).unwrap()))
                .unwrap();
            assert!(
                fixture
                    .controller
                    .bind_copy_authority(token, selected)
                    .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn copy_authority_negotiates_legacy_and_frontier_for_original_and_helper() {
        for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            for operation in [11, 21, 22] {
                let mut selected = selection();
                selected.address_length = match operation {
                    21 => libc::MSG_DONTWAIT,
                    22 => libc::MSG_DONTWAIT | libc::MSG_PEEK,
                    _ => 0,
                };
                if operation == 21 {
                    selected.original_count = 512;
                }
                let bound =
                    controlled_copy_authority(wire, owner(), operation, selected.clone()).unwrap();
                assert_eq!(bound.version(), wire.copy_version());
                assert_eq!(bound.operation(), operation);
                assert_eq!(bound.flags(), selected.address_length);
                bound.validate_selection(&selected).unwrap();
            }
        }
    }

    #[test]
    fn copy_authority_no_file_read_cannot_acquire_a_selected_file() {
        let mut selected = selection();
        selected.file = 0;
        let bound =
            controlled_copy_authority(ProviderWireFormat::Abi8Copy5, owner(), 11, selected.clone())
                .unwrap();
        bound.validate_selection(&selected).unwrap();
        selected.file = 61;
        assert!(bound.validate_selection(&selected).is_err());
        for operation in [21, 22] {
            selected.file = 0;
            selected.original_count = if operation == 21 { 512 } else { 2048 };
            selected.address_length =
                libc::MSG_DONTWAIT | if operation == 22 { libc::MSG_PEEK } else { 0 };
            assert!(
                controlled_copy_authority(
                    ProviderWireFormat::Abi8Copy5,
                    owner(),
                    operation,
                    selected.clone()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn empty_terminal_authority_requires_actual_reserved_or_running_command() {
        for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            for phase in [1, 2, 3] {
                let selected = selection();
                let call = serde_json::from_value(serde_json::json!(selected.call)).unwrap();
                let (token, raw) = controlled_empty_copy_terminal_authority(
                    wire,
                    owner(),
                    selected.clone(),
                    phase,
                    Ok,
                )
                .unwrap();
                token
                    .validate_binding(owner(), call, selected.command, &raw)
                    .unwrap();
                assert_eq!(raw.original, ffi::OriginalResult::default().into());
                assert_eq!(raw.fd_call_present, 0);
                assert_eq!(raw.task_absent, 1);
                let mut wrong = raw.clone();
                wrong.original.complete = 1;
                assert!(
                    token
                        .validate_binding(owner(), call, selected.command, &wrong)
                        .is_err()
                );
                assert!(
                    token
                        .validate_binding(owner(), call, selected.command + 1, &raw)
                        .is_err()
                );
                let mut other = owner();
                other.mm = other.mm.for_exec(other.thread);
                assert!(
                    token
                        .validate_binding(other, call, selected.command, &raw)
                        .is_err()
                );
            }
        }
        for phase in [0, 4, u64::MAX] {
            assert!(
                controlled_empty_copy_terminal_authority(
                    ProviderWireFormat::Abi8Copy5,
                    owner(),
                    selection(),
                    phase,
                    Ok
                )
                .is_err()
            );
        }
    }

    #[test]
    fn empty_terminal_authority_refuses_nonzero_result_or_changed_native_identity() {
        for case in 0..21 {
            let mut fixture = Fixture::new(
                Some(ProviderWireFormat::Abi8Copy5),
                owner(),
                11,
                selection(),
            )
            .unwrap();
            let sequence = fixture.prepare(true, None).unwrap();
            let token = fixture
                .controller
                .prepared_copy_authority(owner(), fixture.call, sequence, fixture.selection.command)
                .unwrap();
            let phase = if case >= 17 { 2 } else { 3 };
            let (sequence, _) = fixture
                .empty_terminal(phase, |raw| match case {
                    0 => raw.call += 1,
                    1 => raw.task_absent = 0,
                    2 => raw.fd_call_present = 1,
                    3 => raw.command.command += 1,
                    4 => raw.command.operation = 21,
                    5 => raw.command.original_count += 1,
                    6 => raw.command.reserved = 1,
                    7 => raw.command.task = 0,
                    8 => raw.command.start_boottime = 0,
                    9 => raw.command.identity.provider += 1,
                    10 => raw.original.selection.file = 1,
                    11 => raw.original.address[127] = 1,
                    12 => raw.original.returned = -libc::EBADF,
                    13 => raw.original.complete = 1,
                    14 => raw.original.copy_entered = 1,
                    15 => raw.original.problem = 1,
                    16 => raw.original.reserved = 1,
                    17 => raw.command.task = 31,
                    18 => raw.command.identity.provider = 73,
                    19 => raw.command.returned = -libc::EIO,
                    20 => raw.command.state.lowat = 1,
                    _ => unreachable!(),
                })
                .unwrap();
            assert!(
                fixture
                    .controller
                    .empty_copy_terminal_authority(token, sequence)
                    .is_err(),
                "case {case}"
            );
            assert_eq!(
                fixture
                    .controller
                    .state
                    .lock()
                    .unwrap()
                    .session
                    .terminal_custody()
                    .retained_rights,
                1
            );
        }
    }

    #[test]
    fn empty_terminal_authority_never_replaces_a_prior_selected_receipt() {
        for query in [0, 1, 2] {
            let mut fixture = Fixture::new(
                Some(ProviderWireFormat::Abi8Copy5),
                owner(),
                11,
                selection(),
            )
            .unwrap();
            let sequence = fixture.prepare(true, None).unwrap();
            let token = fixture
                .controller
                .prepared_copy_authority(owner(), fixture.call, sequence, fixture.selection.command)
                .unwrap();
            if query == 1 {
                fixture.select(None).unwrap();
            }
            let (sequence, raw) = fixture.empty_terminal(3, |_| {}).unwrap();
            if query == 2 {
                fixture.peer.begin_observation(2).unwrap();
                fixture
                    .peer
                    .finish_observation(
                        2,
                        serde_json::to_vec(&Reply::OriginalTerminated(Observation {
                            status: ok("ap_retire_dead_original"),
                            raw: raw.clone(),
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                assert!(fixture.peer.try_reply(2).unwrap());
                Controller::progress_io(
                    &mut fixture.controller.state.lock().unwrap(),
                    &fixture.controller.changed,
                    fixture.controller.run,
                )
                .unwrap();
            }
            let result = fixture
                .controller
                .empty_copy_terminal_authority(token, sequence);
            assert_eq!(result.is_ok(), query != 1);
            if let Ok(bound) = result {
                bound
                    .validate_binding(owner(), fixture.call, fixture.selection.command, &raw)
                    .unwrap();
            }
        }
    }

    #[test]
    fn empty_terminal_authority_cannot_move_between_controllers() {
        let mut first = Fixture::new(
            Some(ProviderWireFormat::Abi8Copy5),
            owner(),
            11,
            selection(),
        )
        .unwrap();
        let p = first.prepare(true, None).unwrap();
        let token = first
            .controller
            .prepared_copy_authority(owner(), first.call, p, first.selection.command)
            .unwrap();
        let mut second = Fixture::new(
            Some(ProviderWireFormat::Abi8Copy5),
            owner(),
            11,
            selection(),
        )
        .unwrap();
        second.task = first.task.as_fd().try_clone_to_owned().unwrap();
        let p = second.prepare(true, None).unwrap();
        let _other = second
            .controller
            .prepared_copy_authority(owner(), second.call, p, second.selection.command)
            .unwrap();
        let (terminal, _) = second.empty_terminal(3, |_| {}).unwrap();
        assert!(
            second
                .controller
                .empty_copy_terminal_authority(token, terminal)
                .is_err()
        );
    }

    #[test]
    fn copy_authority_terminal_receipt_preserves_only_actual_selected_prefix() {
        for missing_file in [false, true] {
            let mut fixture = Fixture::new(
                Some(ProviderWireFormat::Abi8Copy5),
                owner(),
                11,
                selection(),
            )
            .unwrap();
            let sequence = fixture.prepare(true, None).unwrap();
            let token = fixture
                .controller
                .prepared_copy_authority(owner(), fixture.call, sequence, fixture.selection.command)
                .unwrap();
            let terminal = fixture.terminal(missing_file).unwrap();
            let result = fixture.controller.bind_copy_authority(token, terminal);
            assert_eq!(result.is_err(), missing_file);
            if let Ok(bound) = result {
                bound.validate_selection(&fixture.selection).unwrap();
            }
            // Receipt remains diagnostic and incomplete, even after binding.
            let Some(Reply::OriginalTerminated(retained)) =
                fixture.controller.retained_response(terminal).unwrap()
            else {
                panic!("terminal lost");
            };
            assert_eq!(retained.raw.original.complete, 0);
            assert_eq!(retained.raw.task_absent, 1);
        }
    }
}
