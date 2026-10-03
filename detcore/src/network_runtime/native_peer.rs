//! Original-OFD queue effects owned by the run, keyed by existing stream receipts.
//!
//! No descriptor number in this module authenticates a guest FD. Admission and
//! the call/lease join come from the shared engine. Every kernel operation is
//! synchronous, uses owned scratch and bounded nonconsuming waits or
//! MSG_DONTWAIT, and retains its raw result before the caller can await an RPC.
//! This does not supply replay topology or
//! prove completion of native epoll callbacks after a peer send.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;

use serde::Deserialize;
use serde::Serialize;

use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamLeaseId;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::NetworkStreamPhysicalEffect as Effect;
use crate::network_replay::NetworkStreamPhysicalResult as ResultValue;

mod copy_exclusion;
mod early_connect;
mod shared_waits;
pub(crate) use copy_exclusion::ConfirmedNoStore;

const PUBLICATION_UNIT: usize = 1024;
const DRAIN_VIEW: usize = 512;
const MAX_RW_COUNT: usize = 0x7fff_f000;

/// Raw completion stays in the run owner even if its RPC reply is discarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// The actual Linux return, before semantic result validation.
    pub raw_return: i64,
    /// Positive errno captured immediately after a negative return.
    pub errno: Option<i32>,
    /// Exact received suffix/drain bytes, not reconstructed logical bytes.
    pub bytes: Vec<u8>,
    /// Existing shared engine completion vocabulary.
    pub confirmation: ResultValue,
    /// Local same-Call proof is intentionally absent after a serde round trip.
    #[serde(skip)]
    pub(crate) helper_copy: Option<super::helper_receive::Completion>,
}

mod raw_poll;

#[derive(Debug, Clone)]
struct Pending {
    effect: Effect,
    result: Option<Observation>,
    confirmed: bool,
    helper: Option<super::helper_receive::Held>,
}

#[derive(Debug, Default, Clone)]
struct Lease {
    pending: Option<Pending>,
    no_store_join: Option<super::native_copy_exclusion::NoStoreJoinOrigin>,
    // The probe's PEEK bytes must survive its later cursor/poll/FIONREAD effects
    // until CompleteShadowProbe actually publishes them in the shared engine.
    peek: Option<Observation>,
    private_predecessor: Option<super::helper_receive::Completion>,
}

#[derive(Debug)]
struct Call {
    id: NetworkStreamCallId,
    owner: NetworkStreamOwner,
    identity: Option<super::original_installation::FileIdentity>,
    acquisition: Result<(), i32>,
    releasing: bool,
    original: Option<std::sync::Arc<OwnedFd>>,
    leases: BTreeMap<NetworkStreamLeaseId, Lease>,
    release: Option<Release>,
    invocation: Option<OriginalConnect>,
    publication: Option<super::NativeCaptureRecovery>,
    terminal: Option<crate::network_replay::native_terminal::Admission>,
}

// An explicit component-test provider premise on this non-Clone runtime.
// Keep the slot after taking its engine so neither rearming nor reuse works.
#[cfg(test)]
#[derive(Debug)]
pub(super) struct ControlledPrivateDrain {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    engine: Option<std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>>,
}

type OriginalAdmission = crate::network_replay::original_connect::Admission;
type OriginalPin = crate::network_replay::original_connect::Pin;

#[derive(Clone)]
pub(super) struct OriginalConnect {
    pub admission: OriginalAdmission,
    pub pin: Option<OriginalPin>,
    // Supplied only by the backend's synchronous Prepared observation, never
    // deserialized from the RPC or discovered through a numeric task registry.
    pub close_metadata: Option<std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>>,
    pub installation_owner: Option<super::original_installation::Owner>,
    // Exact pre-invocation task authority survives provider request retirement.
    // It is used only to acquire a candidate, never as file-generation proof.
    pub allocation_task: Option<std::sync::Arc<OwnedFd>>,
    pub allocation_userns: Option<(u64, u64)>,
    // Kernel-returned namespace capability from this exact original task before
    // provider submission. CLONE_FILES never grants namespace equivalence.
    pub allocation_netns: Option<std::sync::Arc<OwnedFd>>,
    pub socket_observation_request: Option<u64>,
    pub socket_observation: Option<super::installation_observation::Capture>,
    pub socket_observation_retirement_request: Option<u64>,
    pub socket_observation_retired: bool,
    pub terminal_allocator_started: bool,
    pub terminal_allocator_done: bool,
    pub terminal_installation: Option<super::original_installation::Installation>,
    pub openat_observation: Option<super::openat_observation::State>,
    pub openat_published: Option<super::original_installation::OpenatPublication>,
    // None is unresolved; Some(None) is the actual no-install result.
    pub installed: Option<Option<crate::types::FdSlotBinding>>,
    pub publication: super::NativeCaptureRecovery,
    pub executor: tokio::runtime::Handle,
    pub prepare_request: Option<u64>,
    pub close_queued: bool,
    pub canceled: bool,
    pub terminal: Option<super::accepted_provider::OriginalTerminal>,
    pub terminating: bool,
    pub failed_collection: Option<(
        u64,
        super::accepted_provider::Observation<super::accepted_provider::OriginalEffect>,
    )>,
    pub prepared: Option<(u64, u64)>, // original request and command
    pub selection_request: Option<u64>,
    pub selection: Option<super::accepted_provider::OriginalSelection>,
    pub control_selection: Option<super::accepted_provider::OriginalResult>,
    pub control_history_started: bool,
    pub control_history: Option<Result<super::original_epoll_ctl::HistoricalPair, String>>,
    pub completion_request: Option<u64>,
    pub completion: Option<super::accepted_provider::OriginalEffect>,
    // Private, non-serialized observation of the same still-retained original
    // pin. An error remains available through normal physical retirement and
    // refuses semantic publication; it never changes the guest's raw result.
    early_connect: Option<Result<early_connect::Completion, String>>,
    pub copy_requests: Vec<(u64, u64)>,
    pub copy_end: Option<super::original_read_copy::End>,
    pub copy_custody: std::sync::Arc<super::original_read_copy::ReadCopyCustody>,
    pub copy_capture: Option<std::sync::Arc<super::original_read_copy::Capture>>,
    pub retirement_request: Option<u64>,
    pub retired: bool,
}
impl OriginalConnect {
    /// Preserve the actual complete provider result across final wait without
    /// inventing a backend callback or an auxiliary socket observation.
    pub(super) fn completed_allocator_effect(
        &self,
    ) -> io::Result<super::accepted_provider::OriginalEffect> {
        if let Some(effect) = &self.completion {
            return Ok(effect.clone());
        }
        let terminal = self
            .terminal
            .as_ref()
            .filter(|terminal| terminal.fd_call_present == 1 && terminal.original.complete == 1)
            .ok_or_else(|| io::Error::other("terminal allocator lacks actual complete sys_exit"))?;
        Ok(super::accepted_provider::OriginalEffect {
            command: terminal.command.clone(),
            original: terminal.original.clone(),
            socket: None,
            read_copy: None,
            send: None,
        })
    }
}
impl std::fmt::Debug for OriginalConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalConnect")
            .field("admission", &self.admission)
            .field("pin", &self.pin)
            .field("prepared", &self.prepared)
            .field("selected", &self.selection.is_some())
            .field("completed", &self.completion.is_some())
            .field("retired", &self.retired)
            .finish()
    }
}

/// Completion classification consumes actual held-pin authority in addition
/// to positive path observations. Missing hooks and errno coincidence never
/// prove that the security/copy boundary was not reached.
fn original_path_observed(
    pin: Option<OriginalPin>,
    r: &super::accepted_provider::OriginalResult,
    raw: i64,
) -> bool {
    let file = r.selection.file;
    let local = (!(0..=128).contains(&r.selection.address_length)
        && raw == -i64::from(libc::EINVAL))
        || (r.copy_entered == 1
            && r.copy_returned == 1
            && r.copy_remaining > 0
            && raw == -i64::from(libc::EFAULT))
        || (r.audit_entered == 1
            && r.audit_returned == 1
            && r.audit_result < 0
            && raw == i64::from(r.audit_result));
    match pin {
        Some(OriginalPin::Empty | OriginalPin::Path) => file == 0 && raw == -i64::from(libc::EBADF),
        Some(OriginalPin::Other) => {
            file != 0
                && (local
                    || (raw == -i64::from(libc::ENOTSOCK)
                        && r.security_entered == 0
                        && r.security_returned == 0))
        }
        Some(OriginalPin::Socket { .. }) => {
            file != 0 && (local || (r.security_entered == 1 && r.security_returned == 1))
        }
        None => false,
    }
}

/// Runtime custody, not a guest descriptor registry. Call IDs never get reused.
#[derive(Debug, Default)]
pub(super) struct Calls {
    calls: BTreeMap<NetworkStreamCallId, Call>,
}

/// Close errors are retained separately from proof that Linux released the FD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Release {
    pub original: Option<i32>,
}

impl Calls {
    pub(super) fn capture(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        original: OwnedFd,
    ) -> io::Result<()> {
        if self.calls.contains_key(&call) {
            return Err(io::Error::other("native call already captured"));
        }
        self.calls.insert(
            call,
            Call {
                id: call,
                owner,
                identity: None,
                acquisition: Ok(()),
                releasing: false,
                publication: None,
                terminal: None,
                original: Some(std::sync::Arc::new(original)),
                leases: BTreeMap::new(),
                release: None,
                invocation: None,
            },
        );
        Ok(())
    }

    pub(super) fn capture_authenticated(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        original: OwnedFd,
        identity: super::original_installation::FileIdentity,
    ) -> io::Result<()> {
        self.capture(owner, call, original)?;
        self.owned(owner, call)?.identity = Some(identity);
        Ok(())
    }

    pub(super) fn capture_original(
        &mut self,
        owner: NetworkStreamOwner,
        admission: OriginalAdmission,
        pin: Option<OwnedFd>,
        executor: tokio::runtime::Handle,
        publication: super::NativeCaptureRecovery,
    ) -> io::Result<()> {
        let call = admission.call;
        if !matches!(admission.arguments.kind, crate::network_replay::original_connect::Kind::Connect
            | crate::network_replay::original_connect::Kind::Sendto)
            && pin.is_some()
        {
            return Err(io::Error::other(
                "close cannot acquire an additional physical file reference",
            ));
        }
        if self.calls.contains_key(&call) {
            return Err(io::Error::other(
                "original capture changed exact admitted custody",
            ));
        }
        self.calls.insert(
            call,
            Call {
                id: call,
                owner,
                identity: None,
                acquisition: Ok(()),
                releasing: false,
                publication: None,
                terminal: None,
                original: pin.map(std::sync::Arc::new),
                leases: BTreeMap::new(),
                release: None,
                invocation: Some(OriginalConnect {
                    admission,
                    pin: None,
                    close_metadata: None,
                    installation_owner: None,
                    allocation_task: None,
                    allocation_userns: None,
                    allocation_netns: None,
                    socket_observation_request: None,
                    socket_observation: None,
                    socket_observation_retirement_request: None,
                    socket_observation_retired: false,
                    terminal_allocator_started: false,
                    terminal_allocator_done: false,
                    terminal_installation: None,
                    openat_observation: None,
                    openat_published: None,
                    installed: None,
                    publication,
                    executor,
                    prepare_request: None,
                    close_queued: false,
                    canceled: false,
                    terminal: None,
                    terminating: false,
                    failed_collection: None,
                    prepared: None,
                    selection_request: None,
                    selection: None,
                    control_selection: None,
                    control_history_started: false,
                    control_history: None,
                    completion_request: None,
                    completion: None,
                    early_connect: None,
                    copy_requests: Vec::new(),
                    copy_end: None,
                    copy_custody: Default::default(),
                    copy_capture: None,
                    retirement_request: None,
                    retired: false,
                }),
            },
        );
        Ok(())
    }
    pub(super) fn abandon_original_before_provider(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<()> {
        let original = self.original(owner, call)?;
        if original.prepare_request.is_some()
            || original.prepared.is_some()
            || original.selection.is_some()
        {
            return Err(io::Error::other("original provider may already be armed"));
        }
        original.retired = true;
        Ok(())
    }
    pub(super) fn original_closed(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<bool> {
        let state = self.owned(owner, call)?;
        Ok(state
            .release
            .is_some_and(|release| release.original != Some(libc::EBADF)))
    }
    pub(super) fn original_reference(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<Option<std::sync::Arc<OwnedFd>>> {
        let state = self.owned(owner, call)?;
        if state.invocation.is_none() || state.releasing {
            return Err(io::Error::other("original pin unavailable"));
        }
        Ok(state.original.clone())
    }
    pub(super) fn original_classified(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        pin: OriginalPin,
    ) -> io::Result<()> {
        let state = self.original(owner, call)?;
        if !matches!(state.admission.arguments.kind, crate::network_replay::original_connect::Kind::Connect
            | crate::network_replay::original_connect::Kind::Sendto)
            || state.pin.is_some()
            || state.admission.arguments.binding.is_none() != (pin == OriginalPin::Empty)
        {
            return Err(io::Error::other("original classification changed held pin"));
        }
        state.pin = Some(pin);
        Ok(())
    }
    pub(super) fn originals(&self) -> Vec<(NetworkStreamOwner, OriginalConnect)> {
        self.calls
            .values()
            .filter_map(|s| s.invocation.clone().map(|call| (s.owner, call)))
            .collect()
    }
    pub(super) fn original(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<&mut OriginalConnect> {
        self.owned(owner, call)?
            .invocation
            .as_mut()
            .ok_or_else(|| io::Error::other("not an original invocation"))
    }
    pub(super) fn bind_original_close_metadata(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        metadata: std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    ) -> io::Result<()> {
        let state = self.original(owner, admission.call)?;
        if state.admission != *admission
            || admission.arguments.kind != crate::network_replay::original_connect::Kind::Close
            || state.prepared.is_none()
            || state.pin.is_some()
            || state.retired
        {
            return Err(io::Error::other(
                "close preparation changed its retained Call",
            ));
        }
        if let Some(prior) = &state.close_metadata {
            if !std::sync::Arc::ptr_eq(prior, &metadata) {
                return Err(io::Error::other(
                    "close preparation changed its actual metadata owner",
                ));
            }
        } else {
            state.close_metadata = Some(metadata);
        }
        Ok(())
    }
    pub(super) fn bind_original_installation_owner(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        bound: super::original_installation::Owner,
    ) -> io::Result<()> {
        let state = self.original(owner, admission.call)?;
        if state.admission != *admission
            || !(admission.arguments.kind.allocator()
                || admission.arguments.kind
                    == crate::network_replay::original_connect::Kind::EpollCtl)
            || bound.owner != owner
            || bound.files != admission.arguments.files
            || state.prepared.is_none()
            || state.pin.is_some()
            || state.retired
        {
            return Err(io::Error::other(
                "Socket Prepared changed original Call custody",
            ));
        }
        if let Some(prior) = &state.installation_owner {
            if !prior.same(&bound) {
                return Err(io::Error::other(
                    "Socket Prepared changed actual table owner",
                ));
            }
        } else {
            state.installation_owner = Some(bound);
        }
        Ok(())
    }
    pub(super) fn original_prepared(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        request: u64,
        command: u64,
        selection: u64,
    ) -> io::Result<()> {
        let state = self.original(owner, admission.call)?;
        if state.admission != *admission
            || request == 0
            || command == 0
            || selection <= request
            || state.prepared.is_some()
            || state.selection_request.is_some()
        {
            return Err(io::Error::other(
                "original preparation changed retained command",
            ));
        }
        state.prepared = Some((request, command));
        state.selection_request = Some(selection);
        Ok(())
    }
    pub(super) fn original_control_selected(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        raw: super::accepted_provider::OriginalResult,
    ) -> io::Result<super::original_epoll_ctl::Capture> {
        let state = self.original(owner, admission.call)?;
        let bound = state
            .installation_owner
            .as_ref()
            .ok_or_else(|| io::Error::other("epoll pair lacks actual Prepared owner"))?;
        let args = &admission.arguments;
        if state.admission != *admission
            || args.kind != crate::network_replay::original_connect::Kind::EpollCtl
            || args.binding.is_some()
            || state.pin.is_some()
            || state.retired
            || state.prepared.map(|p| p.1) != Some(raw.selection.command)
            || bound.owner != owner
            || bound.files != args.files
        {
            return Err(io::Error::other("epoll pair changed original Call custody"));
        }
        let request = super::original_epoll_ctl::Request {
            command: raw.selection.command,
            call: admission.call.native_command_call(),
            owner_mm: owner.mm.generation(),
            provider: bound.provider,
            epfd: args.fd,
            operation: args.length,
            target_fd: args.original_count as u32 as i32,
            event_address: args.address,
        };
        let capture = super::original_epoll_ctl::validate_selection(&request, &raw)?;
        if (
            capture.identity.task,
            capture.identity.task_start,
            capture.identity.table,
        ) != (bound.task, bound.start, bound.table)
            || state
                .control_selection
                .as_ref()
                .is_some_and(|old| old != &raw)
        {
            return Err(io::Error::other(
                "epoll pair changed exact task/table or immutable response",
            ));
        }
        // Retain the full result before any journal wait or semantic narrowing.
        state.control_selection = Some(raw);
        Ok(capture)
    }
    pub(super) fn original_selected(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        selection: super::accepted_provider::OriginalSelection,
    ) -> io::Result<()> {
        let state = self.original(owner, admission.call)?;
        let args = &admission.arguments;
        if state.admission != *admission
            || state.prepared.map(|p| p.1) != Some(selection.command)
            || selection.call != admission.call.native_command_call()
            || selection.owner_mm != owner.mm.generation()
            || selection.provider == 0
            || selection.task == 0
            || selection.task_start == 0
            || selection.table == 0
            || selection.requested_fd != args.fd
            || selection.user_address != args.address
            || selection.address_length != args.length
            || selection.original_count != args.original_count
            || selection.ready != 1
            || selection.fdput_flags > 1
            || (selection.file == 0 && selection.fdput_flags != 0)
            || state
                .selection
                .as_ref()
                .is_some_and(|old| old != &selection)
        {
            return Err(io::Error::other(
                "original selection changed retained invocation",
            ));
        }
        match (admission.arguments.kind, state.pin) {
            (crate::network_replay::original_connect::Kind::Close, None)
                if admission.arguments.binding.is_some() == (selection.file != 0)
                    && selection.fdput_flags == 0 => {}
            (crate::network_replay::original_connect::Kind::File(_), None)
                if admission.arguments.binding.is_some() == (selection.file != 0) => {}
            // Actual Prepared metadata in the same engine Call distinguishes
            // absent/O_PATH from readable descriptions before publication.
            // This transport layer never invents a selected file from a slot.
            (crate::network_replay::original_connect::Kind::Read, None) => {}
            (crate::network_replay::original_connect::Kind::Sendto,
                Some(OriginalPin::Socket { kind: libc::SOCK_STREAM, protocol: libc::IPPROTO_TCP, .. }))
                if admission.arguments.binding.is_some() && selection.file != 0 => {}
            (
                crate::network_replay::original_connect::Kind::Socket
                | crate::network_replay::original_connect::Kind::Openat
                | crate::network_replay::original_connect::Kind::EpollCreate { .. },
                None,
            ) if admission.arguments.binding.is_none() && selection.fdput_flags == 0 => {
                let bound = state.installation_owner.as_ref().ok_or_else(|| {
                    io::Error::other("Socket selection preceded exact Prepared owner")
                })?;
                if (
                    selection.provider,
                    selection.task,
                    selection.task_start,
                    selection.table,
                ) != (bound.provider, bound.task, bound.start, bound.table)
                {
                    return Err(io::Error::other(
                        "Socket selection changed original task/table",
                    ));
                }
            }
            (
                crate::network_replay::original_connect::Kind::Connect,
                Some(OriginalPin::Empty | OriginalPin::Path),
            ) if selection.file == 0 => {}
            (
                crate::network_replay::original_connect::Kind::Connect,
                Some(OriginalPin::Other | OriginalPin::Socket { .. }),
            ) if selection.file != 0 => {}
            _ => {
                return Err(io::Error::other(
                    "actual fdget selection differs from continuously held pin",
                ));
            }
        }
        state.selection = Some(selection);
        Ok(())
    }
    pub(super) fn original_completed(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        effect: super::accepted_provider::OriginalEffect,
        raw: i64,
    ) -> io::Result<()> {
        let early_pin = if admission.arguments.kind == crate::network_replay::original_connect::Kind::Connect
            && (raw == 0 || raw == -i64::from(libc::EINPROGRESS))
        {
            self.owned(owner, admission.call)?.original.clone()
        } else {
            None
        };
        let state = self.original(owner, admission.call)?;
        let r = &effect.original;
        if let Some(observation) = &effect.socket {
            observation.command(&effect)?;
            if admission.arguments.kind != crate::network_replay::original_connect::Kind::Socket
                || raw < 0
            {
                return Err(io::Error::other(
                    "auxiliary file observation changed original operation",
                ));
            }
        }
        if state.admission != *admission
            || state.selection.as_ref() != Some(&r.selection)
            || effect.command.command != r.selection.command
            || effect.command.operation != admission.arguments.kind.provider_operation()
            || effect.command.original_count != admission.arguments.original_count
            || effect.command.phase != 1
            || i64::from(effect.command.returned) != raw
            || i64::from(r.returned) != raw
            || r.complete != 1
            || r.problem != 0
            || r.reserved != 0
            || r.address.len() != 128
            || state.completion.is_some()
        {
            return Err(io::Error::other(
                "original native completion does not match exact backend/selection",
            ));
        }
        let path = match admission.arguments.kind {
            crate::network_replay::original_connect::Kind::Sendto => {
                effect.send.as_ref()
                    .ok_or_else(|| io::Error::other("Sendto completion lacks captured skb bytes"))?
                    .validate(&effect)?;
                matches!(state.pin, Some(OriginalPin::Socket {
                    kind: libc::SOCK_STREAM, protocol: libc::IPPROTO_TCP, ..
                })) && admission.arguments.kind.valid_counted_result(raw, admission.arguments.original_count)
            }
            crate::network_replay::original_connect::Kind::EpollCtl => {
                let history = state
                    .control_history
                    .as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .ok_or_else(|| {
                        io::Error::other("epoll completion lacks retained historical pair")
                    })?;
                let early = state
                    .control_selection
                    .as_ref()
                    .ok_or_else(|| io::Error::other("epoll completion lost early pair bytes"))?;
                let capture = history.capture();
                let request = super::original_epoll_ctl::Request {
                    command: capture.identity.command,
                    call: admission.call.native_command_call(),
                    owner_mm: owner.mm.generation(),
                    provider: capture.identity.provider,
                    epfd: admission.arguments.fd,
                    operation: admission.arguments.length,
                    target_fd: admission.arguments.original_count as u32 as i32,
                    event_address: admission.arguments.address,
                };
                let (complete, returned) =
                    super::original_epoll_ctl::validate_completion(&request, &effect)?;
                state.pin.is_none()
                    && &complete == capture
                    && returned == raw
                    && early.address[..96] == r.address[..96]
                    && (early.complete != 1 || early == r)
                    && (early.address[96..104] == [0; 8] || early.address[96..] == r.address[96..])
            }
            crate::network_replay::original_connect::Kind::Connect => {
                original_path_observed(state.pin, r, raw)
            }
            crate::network_replay::original_connect::Kind::Close => {
                state.pin.is_none()
                    && r.selection.fdput_flags == 0
                    && r.selection.user_address == 0
                    && r.selection.address_length == 0
                    && r.address.iter().all(|b| *b == 0)
                    && r.copy_entered == 0
                    && r.copy_returned == 0
                    && r.copy_remaining == 0
                    && r.audit_entered == 0
                    && r.audit_returned == 0
                    && r.audit_result == 0
                    && r.security_entered == 0
                    && r.security_returned == 0
                    && r.security_result == 0
                    && (-4095..=0).contains(&raw)
                    && (r.selection.file != 0 || raw == -i64::from(libc::EBADF))
            }
            crate::network_replay::original_connect::Kind::Read => {
                state.pin.is_none()
                    && r.selection.address_length == 0
                    && r.selection.user_address == admission.arguments.address
                    && r.selection.original_count == admission.arguments.original_count
                    && r.address.iter().all(|b| *b == 0)
                    && r.copy_entered == 0
                    && r.copy_returned == 0
                    && r.copy_remaining == 0
                    && r.audit_entered == 0
                    && r.audit_returned == 0
                    && r.audit_result == 0
                    && r.security_entered == 0
                    && r.security_returned == 0
                    && r.security_result == 0
                    && admission
                        .arguments
                        .kind
                        .valid_counted_result(raw, admission.arguments.original_count)
                    && (r.selection.file != 0 || raw == -i64::from(libc::EBADF))
            }
            crate::network_replay::original_connect::Kind::Socket
            | crate::network_replay::original_connect::Kind::Openat
            | crate::network_replay::original_connect::Kind::EpollCreate { .. } => {
                state.pin.is_none()
                    && super::original_installation::allocator_result(
                        r,
                        raw,
                        admission.arguments.kind,
                        admission.arguments.original_count,
                    )
                    .is_ok()
            }
            crate::network_replay::original_connect::Kind::File(operation) => {
                state.pin.is_none()
                    && r.selection.user_address == operation.syscall() as u64
                    && r.selection.address_length == operation.command()
                    && r.address.iter().all(|b| *b == 0)
                    && r.copy_entered == 0
                    && r.copy_returned == 0
                    && r.copy_remaining == 0
                    && r.audit_entered == 0
                    && r.audit_returned == 0
                    && r.audit_result == 0
                    && r.security_entered == 0
                    && r.security_returned == 0
                    && r.security_result == 0
                    && admission.arguments.kind.valid_result(raw)
                    && (r.selection.file != 0 || raw == -i64::from(libc::EBADF))
            }
        };
        if !path {
            return Err(io::Error::other(
                "original path observation UNKNOWN; native return does not prove missing hook absence",
            ));
        }
        if admission.arguments.kind == crate::network_replay::original_connect::Kind::Connect
            && (raw == 0 || raw == -i64::from(libc::EINPROGRESS))
        {
            state.early_connect = Some(early_pin
                .as_ref()
                .ok_or_else(|| io::Error::other("early Connect lost original retained pin"))
                .and_then(|pin| early_connect::Completion::observe(pin.as_fd(), raw != 0))
                .map_err(|error| error.to_string()));
        }
        state.completion = Some(effect);
        Ok(())
    }

    pub(super) fn capture_failed(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        errno: i32,
    ) -> io::Result<()> {
        if self.calls.contains_key(&call) {
            return Err(io::Error::other("native call already captured"));
        }
        self.calls.insert(
            call,
            Call {
                id: call,
                owner,
                identity: None,
                acquisition: Err(errno),
                releasing: false,
                publication: None,
                terminal: None,
                original: None,
                leases: BTreeMap::new(),
                release: None,
                invocation: None,
            },
        );
        Ok(())
    }

    /// Missing means the original worker has not retained a known acquisition.
    /// It never means a failed or absent physical effect.
    pub(super) fn capture_outcome(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<Option<Result<(), i32>>> {
        let Some(state) = self.calls.get(&call) else {
            return Ok(None);
        };
        if state.owner != owner {
            return Err(io::Error::other("native stream owner/MM mismatch"));
        }
        Ok(Some(state.acquisition))
    }

    pub(super) fn finish_failed_capture(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        errno: i32,
    ) -> io::Result<()> {
        let state = self.owned(owner, call)?;
        if state.acquisition != Err(errno) || state.original.is_some() {
            return Err(io::Error::other(
                "native capture failure differs from retained result",
            ));
        }
        self.calls.remove(&call);
        Ok(())
    }

    fn owned(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<&mut Call> {
        let state = self
            .calls
            .get_mut(&call)
            .ok_or_else(|| io::Error::other("unknown native stream call"))?;
        if state.owner != owner {
            return Err(io::Error::other("native stream owner/MM mismatch"));
        }
        Ok(state)
    }

    /// Called only on an engine-produced lease bound to this exact active call.
    /// Logical-only calls have no physical custody and need no additional row.
    pub(super) fn bind_lease(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
    ) -> io::Result<()> {
        if !self.calls.contains_key(&call) {
            return Ok(());
        }
        let state = self.owned(owner, call)?;
        if state.invocation.is_some()
            || state.releasing
            || state.release.is_some()
            || state.leases.contains_key(&lease)
        {
            return Err(io::Error::other("native lease reused or call releasing"));
        }
        state.leases.insert(lease, Lease::default());
        Ok(())
    }

    fn lease(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> io::Result<&mut Call> {
        let state = self
            .calls
            .values_mut()
            .find(|state| state.leases.contains_key(&lease))
            .ok_or_else(|| io::Error::other("native lease has no captured original OFD"))?;
        if state.owner != owner || state.releasing || state.release.is_some() {
            return Err(io::Error::other(
                "native lease owner/MM mismatch or released",
            ));
        }
        Ok(state)
    }

    /// Latch before handing execution to the run-owned blocking worker. Cloning
    /// this Arc retains the same pin; it does not duplicate a kernel file handle.
    pub(super) fn prepare(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: Effect,
    ) -> io::Result<Execution> {
        self.prepare_with_store(owner, lease, effect, None)
    }

    pub(super) fn prepare_with_store(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: Effect,
        full: Option<&crate::network_replay::FullStoreCompletion>,
    ) -> io::Result<Execution> {
        let state = self.lease(owner, lease)?;
        let record = state.leases.get_mut(&lease).expect("found lease");
        match (&record.private_predecessor, full) {
            (Some(source), Some(full))
                if full.has_ended_full_store()
                    && full.store().owner() == owner
                    && full.store().call() == state.id
                    && full.store().lease() == lease
                    && full.store().record_completion().ok() == Some(source)
                    && full.store().source_offset() == 0
                    && effect
                        == (Effect::Drain {
                            maximum: full.store().length(),
                        })
                    && state
                        .identity
                        .is_some_and(|identity| source.binding().matches_file(identity))
                    && record.pending.is_none() => {}
            (None, None) => {}
            _ => {
                return Err(io::Error::other(
                    "private delivery lacks its exact one-use full-store successor",
                ));
            }
        }
        if record.pending.as_ref().is_some_and(|p| !p.confirmed) {
            return Err(io::Error::other(
                "native physical effect remains unresolved",
            ));
        }
        validate(&effect)?;
        let helper = if matches!(effect, Effect::Drain { .. } | Effect::Peek { .. }) {
            Some(super::helper_receive::Held::new_successor(
                owner,
                state.id,
                lease,
                state.identity.ok_or_else(|| {
                    io::Error::other("helper receive lacks captured private file identity")
                })?,
                effect.clone(),
                record.private_predecessor.clone(),
            )?)
        } else {
            None
        };
        record.pending = Some(Pending {
            effect: effect.clone(),
            result: None,
            confirmed: false,
            helper: helper.clone(),
        });
        let original = state
            .original
            .as_ref()
            .ok_or_else(|| io::Error::other("original pin released"))?
            .clone();
        Ok(Execution {
            original,
            effect,
            helper,
            publication: state.publication.clone(),
        })
    }

    /// Recovery uses the original receipt even if the callback owner has exited.
    /// Raw bytes are latched before the blocking worker completes its JoinHandle.
    pub(super) fn retain(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: &Effect,
        result: Observation,
    ) -> io::Result<()> {
        let state = self.lease(owner, lease)?;
        let record = state.leases.get_mut(&lease).expect("found lease");
        let pending = record
            .pending
            .as_mut()
            .ok_or_else(|| io::Error::other("native completion without submission"))?;
        if &pending.effect != effect || pending.result.is_some() || pending.confirmed {
            return Err(io::Error::other(
                "native completion differs from pending effect",
            ));
        }
        pending.result = Some(result.clone());
        if matches!(effect, Effect::Peek { .. }) {
            record.peek = Some(result);
        }
        Ok(())
    }

    #[cfg(test)]
    fn execute(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: Effect,
    ) -> io::Result<Observation> {
        let work = self.prepare(owner, lease, effect.clone())?;
        let helper = work.helper.clone();
        let mut result = work.perform();
        // Explicit controlled provider input for these existing local-socket
        // component tests. The production worker has no such constructor.
        if let Some(helper) = helper {
            result = helper.controlled_observation(result, 4)?;
        }
        self.retain(owner, lease, &effect, result.clone())?;
        Ok(result)
    }

    /// Validate without changing any native or engine receipt. GlobalState
    /// must call this before the engine can mutate counters or lease state.
    pub(super) fn preflight_confirmation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: &Effect,
        observed: &Observation,
    ) -> io::Result<()> {
        let state = self
            .calls
            .values()
            .find(|state| state.leases.contains_key(&lease))
            .ok_or_else(|| io::Error::other("native confirmation has no captured lease"))?;
        let pending = state.leases[&lease]
            .pending
            .as_ref()
            .ok_or_else(|| io::Error::other("native effect was not submitted"))?;
        if state.owner != owner
            || state.releasing
            || state.release.is_some()
            || pending.confirmed
            || &pending.effect != effect
            || pending.result.as_ref() != Some(observed)
        {
            return Err(io::Error::other(
                "native confirmation differs from retained kernel result",
            ));
        }
        match &pending.helper {
            Some(helper) => helper.check_completion(observed.helper_copy.as_ref())?,
            None if observed.helper_copy.is_some() => {
                return Err(io::Error::other(
                    "non-helper result carries unrelated copy custody",
                ));
            }
            None => {}
        }
        if matches!(effect, Effect::PollState) {
            raw_poll::validate(observed)?;
        }
        if let Effect::PollWait { events, .. } = effect {
            raw_poll::validate_wait(observed, *events)?;
        }
        Ok(())
    }

    pub(super) fn confirm(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: &Effect,
        observed: &Observation,
    ) -> io::Result<()> {
        self.preflight_confirmation(owner, lease, effect, observed)?;
        let state = self.lease(owner, lease)?;
        state
            .leases
            .get_mut(&lease)
            .expect("validated lease")
            .pending
            .as_mut()
            .expect("validated pending")
            .confirmed = true;
        Ok(())
    }

    /// The shared publication must consume the exact retained kernel suffix.
    pub(super) fn check_probe_bytes(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        bytes: &[u8],
    ) -> io::Result<()> {
        let Some(state) = self
            .calls
            .values()
            .find(|state| state.leases.contains_key(&lease))
        else {
            return Ok(());
        };
        if state.owner != owner
            || state.leases[&lease]
                .peek
                .as_ref()
                .is_none_or(|peek| peek.bytes != bytes)
        {
            return Err(io::Error::other(
                "shadow publication differs from retained native PEEK",
            ));
        }
        Ok(())
    }

    /// Retire only a completed initial raw scan, before any wait was prepared.
    /// The engine checks that semantic phase under its guard; this registry
    /// independently requires the exact captured Call and confirmed result.
    pub(super) fn abort_poll_lease(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
    ) -> io::Result<()> {
        let state = self
            .calls
            .get_mut(&call)
            .ok_or_else(|| io::Error::other("poll abort has no captured call"))?;
        if state.id != call
            || state.owner != owner
            || state.acquisition.is_err()
            || state.releasing
            || state.release.is_some()
            || state.invocation.is_some()
            || state
                .original
                .as_ref()
                .is_none_or(|pin| std::sync::Arc::strong_count(pin) != 1)
        {
            return Err(io::Error::other("poll abort changed active captured call"));
        }
        let record = state
            .leases
            .get(&lease)
            .ok_or_else(|| io::Error::other("poll abort has no captured lease"))?;
        let pending = record
            .pending
            .as_ref()
            .ok_or_else(|| io::Error::other("poll abort has no completed scan"))?;
        if pending.effect != Effect::PollState
            || !pending.confirmed
            || pending.helper.is_some()
            || record.peek.is_some()
            || record.no_store_join.is_some()
            || record.private_predecessor.is_some()
        {
            return Err(io::Error::other(
                "poll abort is not its confirmed initial scan",
            ));
        }
        let observed = pending
            .result
            .as_ref()
            .ok_or_else(|| io::Error::other("poll abort lacks retained raw result"))?;
        if !matches!(observed.confirmation, ResultValue::PollState { .. }) {
            return Err(io::Error::other("poll abort did not complete a raw scan"));
        }
        raw_poll::validate(observed)?;
        state.leases.remove(&lease);
        Ok(())
    }

    /// Only after the engine has successfully retired its lease.
    pub(super) fn finish_lease(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> io::Result<()> {
        let Some(state) = self
            .calls
            .values_mut()
            .find(|state| state.leases.contains_key(&lease))
        else {
            return Ok(());
        };
        if state.owner != owner
            || state.leases[&lease]
                .pending
                .as_ref()
                .is_some_and(|p| !p.confirmed)
        {
            return Err(io::Error::other(
                "native lease has an unacknowledged effect",
            ));
        }
        state.leases.remove(&lease);
        Ok(())
    }

    /// Called after BeginStreamCallRelease. Move existing handles into the owned
    /// worker; close/linger must not hold this registry's mutex.
    pub(super) fn prepare_release(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<ReleaseExecution> {
        self.prepare_release_inner(owner, call, false)
    }

    fn prepare_release_inner(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        terminal: bool,
    ) -> io::Result<ReleaseExecution> {
        let state = self.owned(owner, call)?;
        if (!terminal && !state.leases.is_empty())
            || state.releasing
            || state.release.is_some()
            || state
                .invocation
                .as_ref()
                .is_some_and(|original| !original.retired)
        {
            return Err(io::Error::other(
                "native release live, repeated or already observed",
            ));
        }
        let original = match state.original.take() {
            None => None,
            Some(original) => match std::sync::Arc::try_unwrap(original) {
                Ok(original) => Some(original),
                Err(original) => {
                    state.original = Some(original);
                    return Err(io::Error::other(
                        "native execution still retains original pin",
                    ));
                }
            },
        };
        state.releasing = true;
        Ok(ReleaseExecution { original })
    }

    pub(super) fn retain_release(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        release: Release,
    ) -> io::Result<()> {
        let state = self.owned(owner, call)?;
        if !state.releasing || state.release.is_some() {
            return Err(io::Error::other("native release was not submitted"));
        }
        state.release = Some(release);
        if release.original == Some(libc::EBADF) {
            return Err(io::Error::other(
                "owned native descriptor was already invalid",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn release(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<Release> {
        let result = self.prepare_release(owner, call)?.perform();
        self.retain_release(owner, call, result)?;
        Ok(result)
    }

    pub(super) fn finish_release(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<()> {
        let release = self
            .owned(owner, call)?
            .release
            .ok_or_else(|| io::Error::other("native release not observed"))?;
        if release.original == Some(libc::EBADF) {
            return Err(io::Error::other(
                "invalid native release cannot be acknowledged",
            ));
        }
        if self
            .owned(owner, call)?
            .invocation
            .as_ref()
            .is_some_and(|state| {
                state
                    .openat_observation
                    .as_ref()
                    .is_some_and(|observation| !observation.closed())
            })
        {
            return Err(io::Error::other(
                "original allocation still owns auxiliary observation",
            ));
        }
        if self
            .owned(owner, call)?
            .invocation
            .as_ref()
            .is_some_and(|state| {
                state.socket_observation_request.is_some() && !state.socket_observation_retired
            })
        {
            return Err(io::Error::other(
                "original allocation still owns its auxiliary socket observation",
            ));
        }
        self.calls.remove(&call);
        Ok(())
    }

    pub(super) fn settled(&self) -> io::Result<()> {
        if self.calls.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(
                "native stream call custody remains unresolved",
            ))
        }
    }
}

/// Exact retained raw effects transferred to the existing engine Call after
/// known physical close. Unknown results remain None, never inferred success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalEvidence {
    admission: crate::network_replay::native_terminal::Admission,
    acquisition: Result<(), i32>,
    leases: BTreeMap<NetworkStreamLeaseId, TerminalLease>,
    release: Release,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalLease {
    effect: Option<Effect>,
    result: Option<Observation>,
    confirmed: bool,
    peek: Option<Observation>,
    helper: Option<super::helper_receive::Receipt>,
    private_predecessor: Option<super::helper_receive::Completion>,
}
impl TerminalEvidence {
    pub(crate) fn matches(
        &self,
        admission: crate::network_replay::native_terminal::Admission,
    ) -> bool {
        self.admission == admission && self.release.original != Some(libc::EBADF)
    }
}

impl Calls {
    pub(super) fn retain_capture_publication(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        publication: super::NativeCaptureRecovery,
    ) -> io::Result<()> {
        let state = self.owned(owner, call)?;
        if state.invocation.is_some() || state.publication.is_some() {
            return Err(io::Error::other(
                "ordinary call publication already retained",
            ));
        }
        state.publication = Some(publication);
        Ok(())
    }

    pub(super) fn ordinary_retirements(
        &self,
    ) -> Vec<(
        NetworkStreamOwner,
        NetworkStreamCallId,
        super::NativeCaptureRecovery,
    )> {
        self.calls
            .iter()
            .filter_map(|(call, state)| {
                state
                    .publication
                    .as_ref()
                    .filter(|_| state.invocation.is_none())
                    .map(|publication| (state.owner, *call, publication.clone()))
            })
            .collect()
    }

    pub(super) fn known_release(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<bool> {
        let state = self.owned(owner, call)?;
        Ok(state.original.is_none()
            && state
                .release
                .is_some_and(|r| r.original != Some(libc::EBADF)))
    }

    pub(super) fn claim_terminal_release(
        &mut self,
        admission: crate::network_replay::native_terminal::Admission,
    ) -> io::Result<bool> {
        let state = self.owned(admission.owner(), admission.call())?;
        if state.invocation.is_some() || state.publication.is_none() {
            return Err(io::Error::other(
                "terminal close lacks original ordinary custody",
            ));
        }
        if let Some(prior) = state.terminal {
            return if prior == admission {
                Ok(false)
            } else {
                Err(io::Error::other("terminal close authority changed"))
            };
        }
        state.terminal = Some(admission);
        Ok(state.release.is_none())
    }

    pub(super) fn terminal_release_authorized(
        &mut self,
        admission: crate::network_replay::native_terminal::Admission,
    ) -> io::Result<()> {
        let state = self.owned(admission.owner(), admission.call())?;
        if state.terminal != Some(admission)
            || state.invocation.is_some()
            || state.publication.is_none()
        {
            return Err(io::Error::other(
                "late close is not this retained terminal Call",
            ));
        }
        Ok(())
    }

    pub(super) fn prepare_terminal_release(
        &mut self,
        admission: crate::network_replay::native_terminal::Admission,
    ) -> io::Result<ReleaseExecution> {
        self.terminal_release_authorized(admission)?;
        self.prepare_release_inner(admission.owner(), admission.call(), true)
    }

    pub(super) fn terminal_evidence(
        &mut self,
        admission: crate::network_replay::native_terminal::Admission,
    ) -> io::Result<TerminalEvidence> {
        self.terminal_release_authorized(admission)?;
        let state = self.owned(admission.owner(), admission.call())?;
        let release = state
            .release
            .ok_or_else(|| io::Error::other("terminal close has no retained result"))?;
        if release.original == Some(libc::EBADF) || state.original.is_some() {
            return Err(io::Error::other("terminal close did not retire exact pin"));
        }
        Ok(TerminalEvidence {
            admission,
            acquisition: state.acquisition,
            leases: state
                .leases
                .iter()
                .map(|(lease, value)| {
                    (
                        *lease,
                        TerminalLease {
                            effect: value.pending.as_ref().map(|p| p.effect.clone()),
                            private_predecessor: value.private_predecessor.clone(),
                            result: value.pending.as_ref().and_then(|p| p.result.clone()),
                            confirmed: value.pending.as_ref().is_some_and(|p| p.confirmed),
                            peek: value.peek.clone(),
                            helper: value
                                .pending
                                .as_ref()
                                .and_then(|p| p.helper.as_ref())
                                .map(|h| h.receipt()),
                        },
                    )
                })
                .collect(),
            release,
        })
    }
}

pub(super) fn classify_original(pin: &OwnedFd) -> io::Result<OriginalPin> {
    let flags = unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_PATH != 0 {
        return Ok(OriginalPin::Path);
    }
    let mut values = [0i32; 3];
    for (n, option) in [libc::SO_DOMAIN, libc::SO_TYPE, libc::SO_PROTOCOL]
        .into_iter()
        .enumerate()
    {
        let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                pin.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&mut values[n] as *mut i32).cast(),
                &mut size,
            )
        };
        if rc != 0 {
            let error = io::Error::last_os_error();
            if n == 0 && error.raw_os_error() == Some(libc::ENOTSOCK) {
                return Ok(OriginalPin::Other);
            }
            return Err(error);
        }
        if size as usize != std::mem::size_of::<i32>() {
            return Err(io::Error::other(
                "original socket classification length changed",
            ));
        }
    }
    Ok(OriginalPin::Socket {
        domain: values[0],
        kind: values[1],
        protocol: values[2],
    })
}

pub(super) struct ReleaseExecution {
    original: Option<OwnedFd>,
}

impl ReleaseExecution {
    pub(super) fn perform(self) -> Release {
        Release {
            original: self.original.and_then(close),
        }
    }
}

pub(super) struct Execution {
    pub(super) original: std::sync::Arc<OwnedFd>,
    pub(super) effect: Effect,
    pub(super) helper: Option<super::helper_receive::Held>,
    pub(super) publication: Option<super::NativeCaptureRecovery>,
}

impl Execution {
    /// Socket-lock acquisition can block even with MSG_DONTWAIT. Call only on
    /// the owned blocking worker, with no scheduler/engine/runtime mutex held.
    #[cfg(test)]
    pub(super) fn perform(self) -> Observation {
        execute(self.original.as_fd(), &self.effect)
    }
    pub(super) fn perform_control(self) -> io::Result<Observation> {
        if self.helper.is_some()
            || matches!(self.effect, Effect::Drain { .. } | Effect::Peek { .. })
        {
            return Err(io::Error::other(
                "helper receive requires authenticated provider execution",
            ));
        }
        Ok(execute(self.original.as_fd(), &self.effect))
    }
}

fn close(fd: OwnedFd) -> Option<i32> {
    let raw = fd.into_raw_fd();
    if unsafe { libc::close(raw) } < 0 {
        Some(errno())
    } else {
        None
    }
}

fn errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn validate(effect: &Effect) -> io::Result<()> {
    let valid = match effect {
        Effect::Peek { maximum } => (PUBLICATION_UNIT..=MAX_RW_COUNT).contains(maximum),
        Effect::Drain { maximum } => (1..=DRAIN_VIEW).contains(maximum),
        Effect::PollWait { events, timeout_ns } => {
            raw_poll::valid_wait_request(*events, *timeout_ns)
        }
        Effect::ReadPeekOffset
        | Effect::SetPeekOffset { .. }
        | Effect::PollState
        | Effect::QueuedBytes => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(io::Error::other(
            "effect is not a bounded native queue operation",
        ))
    }
}

fn observation(
    raw: i64,
    bytes: Vec<u8>,
    confirmation: impl FnOnce(i64) -> ResultValue,
) -> Observation {
    // This function must be entered immediately after the syscall: capture
    // errno before allocation, formatting, locks or another libc operation.
    let errno = (raw < 0).then(errno);
    Observation {
        raw_return: raw,
        errno,
        bytes,
        confirmation: match errno {
            Some(errno) => ResultValue::Errno(errno),
            None => confirmation(raw),
        },
        helper_copy: None,
    }
}

/// Borrowed original OFD, never a guest integer. Guest status flags are untouched.
fn execute(fd: BorrowedFd<'_>, effect: &Effect) -> Observation {
    match effect {
        Effect::Drain { maximum } => {
            let mut bytes = vec![0; *maximum];
            let raw = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    *maximum,
                    libc::MSG_DONTWAIT,
                )
            };
            let mut result = observation(raw as i64, bytes, |_| ResultValue::Drained {
                bytes: Vec::new(),
            });
            result.bytes.truncate(raw.max(0) as usize);
            if raw >= 0 {
                result.confirmation = ResultValue::Drained {
                    bytes: result.bytes.clone(),
                };
            }
            result
        }
        Effect::Peek { maximum } => {
            let prefix = maximum - PUBLICATION_UNIT;
            // Repeated iov entries discard the already-retained prefix without
            // allocating up to the guest queue length. Only the new suffix is
            // returned. This is the original recorder's recvmsg copy order.
            let vectors = prefix
                .div_ceil(DRAIN_VIEW)
                .min(libc::UIO_MAXIOV as usize - 1);
            let sink_size = if vectors == 0 {
                0
            } else {
                prefix.div_ceil(vectors)
            };
            let mut sink = vec![0u8; sink_size];
            let mut bytes = vec![0u8; PUBLICATION_UNIT];
            let mut iov = Vec::with_capacity(vectors + 1);
            let mut left = prefix;
            for _ in 0..vectors {
                let count = left.min(sink_size);
                iov.push(libc::iovec {
                    iov_base: sink.as_mut_ptr().cast(),
                    iov_len: count,
                });
                left -= count;
            }
            iov.push(libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: bytes.len(),
            });
            let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
            header.msg_iov = iov.as_mut_ptr();
            header.msg_iovlen = iov.len();
            let raw = unsafe {
                libc::recvmsg(
                    fd.as_raw_fd(),
                    &mut header,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            let mut result = observation(raw as i64, bytes, |count| ResultValue::Peeked {
                count: count as usize,
            });
            result
                .bytes
                .truncate((raw.max(0) as usize).saturating_sub(prefix));
            result
        }
        Effect::ReadPeekOffset => {
            let mut value = 0i32;
            let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
            let raw = unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEEK_OFF,
                    (&raw mut value).cast(),
                    &raw mut length,
                )
            };
            let mut result =
                observation(raw as i64, Vec::new(), |_| ResultValue::PeekOffset(value));
            if raw >= 0 && length != std::mem::size_of::<i32>() as libc::socklen_t {
                result.confirmation = ResultValue::Errno(libc::EIO);
            }
            result
        }
        Effect::SetPeekOffset { value } => {
            let raw = unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEEK_OFF,
                    (value as *const i32).cast(),
                    std::mem::size_of::<i32>() as libc::socklen_t,
                )
            };
            observation(raw as i64, Vec::new(), |_| ResultValue::Unit)
        }
        Effect::PollState => {
            raw_poll::observe(fd)
        }
        Effect::PollWait { events, timeout_ns } => raw_poll::wait(fd, *events, *timeout_ns),
        Effect::QueuedBytes => {
            let mut count = 0i32;
            let raw = unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIONREAD, &raw mut count) };
            let mut result = observation(raw as i64, Vec::new(), |_| ResultValue::QueuedBytes {
                count: count.max(0) as usize,
            });
            if raw >= 0 && count < 0 {
                result.confirmation = ResultValue::Errno(libc::EIO);
            }
            result
        }
        _ => unreachable!("validate before kernel entry"),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::time::Duration;

    use super::*;

    pub(super) fn pair() -> (OwnedFd, OwnedFd) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sender =
            TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(1))
                .unwrap();
        listener.set_nonblocking(true).unwrap();
        readable(listener.as_fd());
        let (receiver, _) = listener.accept().unwrap();
        (receiver.into(), sender.into())
    }

    fn readable(fd: BorrowedFd<'_>) {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&raw mut pfd, 1, 1000) }, 1);
        assert_ne!(pfd.revents & libc::POLLIN, 0);
    }

    fn send(fd: BorrowedFd<'_>, bytes: &[u8]) {
        assert_eq!(
            unsafe {
                libc::send(
                    fd.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            },
            bytes.len() as isize
        );
    }

    fn owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(7);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }

    // Test-only deserialization constructs an opaque fixture; production never
    // obtains authority by deserializing a number without shared-engine checks.
    struct LeaseFixture;
    impl<'de> serde::Deserializer<'de> for LeaseFixture {
        type Error = serde::de::value::Error;
        fn deserialize_any<V: serde::de::Visitor<'de>>(
            self,
            visitor: V,
        ) -> Result<V::Value, Self::Error> {
            visitor.visit_newtype_struct(serde::de::value::U64Deserializer::<Self::Error>::new(11))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
            option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
        }
    }

    fn lease() -> NetworkStreamLeaseId {
        NetworkStreamLeaseId::deserialize(LeaseFixture).unwrap()
    }

    #[test]
    fn original_peek_returns_exact_suffix_without_consumption_or_flag_change() {
        let (original, peer) = pair();
        let alias = original.try_clone().unwrap();
        let flags = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_NONBLOCK, 0);
        send(peer.as_fd(), b"prior-next-input");
        readable(original.as_fd());
        let effect = Effect::Peek {
            maximum: PUBLICATION_UNIT + 6,
        };
        validate(&effect).unwrap();
        let seen = execute(original.as_fd(), &effect);
        assert_eq!(seen.raw_return, 16);
        assert_eq!(seen.errno, None);
        assert_eq!(seen.bytes, b"next-input");
        assert_eq!(seen.confirmation, ResultValue::Peeked { count: 16 });
        let drain = execute(alias.as_fd(), &Effect::Drain { maximum: 16 });
        assert_eq!(drain.bytes, b"prior-next-input");
        assert_eq!(
            unsafe { libc::fcntl(alias.as_raw_fd(), libc::F_GETFL) },
            flags
        );
    }

    #[test]
    fn native_short_drain_retains_actual_prefix_and_later_eagain() {
        let (original, peer) = pair();
        send(peer.as_fd(), b"abc");
        readable(original.as_fd());
        let got = execute(original.as_fd(), &Effect::Drain { maximum: 512 });
        assert_eq!(got.raw_return, 3);
        assert_eq!(got.bytes, b"abc");
        assert_eq!(
            got.confirmation,
            ResultValue::Drained {
                bytes: b"abc".to_vec()
            }
        );
        let empty = execute(original.as_fd(), &Effect::Drain { maximum: 1 });
        assert_eq!(empty.raw_return, -1);
        assert_eq!(empty.errno, Some(libc::EAGAIN));
        assert!(empty.bytes.is_empty());
        assert_eq!(empty.confirmation, ResultValue::Errno(libc::EAGAIN));
        assert_eq!(
            unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
    }

    #[test]
    fn cancelled_confirmation_retains_bytes_and_refuses_duplicate_effect_and_release() {
        let (original, peer) = pair();
        let original_raw = original.as_raw_fd();
        let mut calls = Calls::default();
        let call = NetworkStreamCallId::controlled_fixture(2);
        calls
            .capture_authenticated(
                owner(),
                call,
                original,
                super::super::original_installation::FileIdentity::controlled_fixture(3, 7),
            )
            .unwrap();
        calls.bind_lease(owner(), call, lease()).unwrap();
        send(peer.as_fd(), b"receipt");
        readable(calls.calls[&call].original.as_ref().unwrap().as_fd());
        let effect = Effect::Drain { maximum: 7 };
        let result = calls.execute(owner(), lease(), effect.clone()).unwrap();
        // Simulate dropping the callback between the actual recv and engine ACK.
        assert_eq!(
            calls.calls[&call].leases[&lease()]
                .pending
                .as_ref()
                .unwrap()
                .result,
            Some(result.clone())
        );
        assert!(calls.execute(owner(), lease(), effect.clone()).is_err());
        assert!(calls.finish_lease(owner(), lease()).is_err());
        assert!(calls.release(owner(), call).is_err());
        assert!(calls.settled().is_err());
        let mut wrong = result.clone();
        wrong.bytes[0] ^= 1;
        assert!(calls.confirm(owner(), lease(), &effect, &wrong).is_err());
        calls.confirm(owner(), lease(), &effect, &result).unwrap();
        calls.finish_lease(owner(), lease()).unwrap();
        calls.release(owner(), call).unwrap();
        assert_eq!(unsafe { libc::fcntl(original_raw, libc::F_GETFD) }, -1);
        assert_eq!(errno(), libc::EBADF);
        assert!(calls.settled().is_err());
        calls.finish_release(owner(), call).unwrap();
        calls.settled().unwrap();
    }

    #[test]
    fn stale_mm_and_unbound_lease_cannot_operate_on_original_pin() {
        let (original, peer) = pair();
        let mut calls = Calls::default();
        let call = NetworkStreamCallId::controlled_fixture(3);
        calls
            .capture_authenticated(
                owner(),
                call,
                original,
                super::super::original_installation::FileIdentity::controlled_fixture(3, 7),
            )
            .unwrap();
        assert!(
            calls
                .execute(owner(), lease(), Effect::Drain { maximum: 1 })
                .is_err()
        );
        calls.bind_lease(owner(), call, lease()).unwrap();
        let stale = NetworkStreamOwner {
            mm: owner().mm.for_exec(owner().thread),
            ..owner()
        };
        send(peer.as_fd(), b"x");
        readable(calls.calls[&call].original.as_ref().unwrap().as_fd());
        assert!(
            calls
                .execute(stale, lease(), Effect::Drain { maximum: 1 })
                .is_err()
        );
        let result = calls
            .execute(owner(), lease(), Effect::Drain { maximum: 1 })
            .unwrap();
        assert_eq!(result.bytes, b"x");
    }

    #[test]
    fn peek_bytes_survive_later_queue_observation_until_exact_publication() {
        let (original, peer) = pair();
        let mut calls = Calls::default();
        let call = NetworkStreamCallId::controlled_fixture(5);
        calls
            .capture_authenticated(
                owner(),
                call,
                original,
                super::super::original_installation::FileIdentity::controlled_fixture(3, 7),
            )
            .unwrap();
        calls.bind_lease(owner(), call, lease()).unwrap();
        send(peer.as_fd(), b"retained bytes");
        readable(calls.calls[&call].original.as_ref().unwrap().as_fd());
        let peek = Effect::Peek {
            maximum: PUBLICATION_UNIT,
        };
        let result = calls.execute(owner(), lease(), peek.clone()).unwrap();
        calls.confirm(owner(), lease(), &peek, &result).unwrap();
        let poll = Effect::PollState;
        let observed = calls.execute(owner(), lease(), poll.clone()).unwrap();
        calls.confirm(owner(), lease(), &poll, &observed).unwrap();
        calls
            .check_probe_bytes(owner(), lease(), b"retained bytes")
            .unwrap();
        assert!(
            calls
                .check_probe_bytes(owner(), lease(), b"replaced bytes")
                .is_err()
        );
        assert_eq!(
            calls.calls[&call].leases[&lease()]
                .peek
                .as_ref()
                .unwrap()
                .bytes,
            b"retained bytes"
        );
    }

    #[test]
    fn helper_pending_requires_private_capture_identity_and_never_retries_unknown_effect() {
        let (original, _peer) = pair();
        let call = NetworkStreamCallId::controlled_fixture(19);
        let mut calls = Calls::default();
        calls.capture(owner(), call, original).unwrap();
        calls.bind_lease(owner(), call, lease()).unwrap();
        let effect = Effect::Drain { maximum: 1 };
        assert!(calls.prepare(owner(), lease(), effect.clone()).is_err());
        assert!(calls.calls[&call].leases[&lease()].pending.is_none());
        // Existing raw socket tests use an explicit private identity premise;
        // production obtains it only at the metadata/read-grant transfer.
        calls.calls.get_mut(&call).unwrap().identity =
            Some(super::super::original_installation::FileIdentity::controlled_fixture(3, 7));
        let work = calls.prepare(owner(), lease(), effect.clone()).unwrap();
        assert!(work.helper.is_some());
        assert!(calls.prepare(owner(), lease(), effect).is_err());
        assert!(
            calls.calls[&call].leases[&lease()]
                .pending
                .as_ref()
                .unwrap()
                .result
                .is_none()
        );
        assert!(
            !calls.calls[&call].leases[&lease()]
                .pending
                .as_ref()
                .unwrap()
                .confirmed
        );
        assert!(calls.finish_lease(owner(), lease()).is_err());
        assert!(calls.prepare_release(owner(), call).is_err());
        drop(work);
        assert!(
            calls.finish_lease(owner(), lease()).is_err(),
            "dropping callback/execution is not completion"
        );
    }

    #[test]
    fn worker_token_retains_pin_without_holding_registry_or_callback_waiter() {
        let (original, peer) = pair();
        send(peer.as_fd(), b"late");
        readable(original.as_fd());
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Calls::default()));
        let call = NetworkStreamCallId::controlled_fixture(6);
        let effect = Effect::Drain { maximum: 4 };
        let work = {
            let mut registry = calls.lock().unwrap();
            registry
                .capture_authenticated(
                    owner(),
                    call,
                    original,
                    super::super::original_installation::FileIdentity::controlled_fixture(3, 7),
                )
                .unwrap();
            registry.bind_lease(owner(), call, lease()).unwrap();
            registry.prepare(owner(), lease(), effect.clone()).unwrap()
        };
        let (release, enter) = std::sync::mpsc::channel();
        let (reply, waiter) = std::sync::mpsc::channel();
        let owned = calls.clone();
        let worker = std::thread::spawn(move || {
            enter.recv_timeout(Duration::from_secs(1)).unwrap();
            let result = work.perform();
            owned
                .lock()
                .unwrap()
                .retain(owner(), lease(), &effect, result.clone())
                .unwrap();
            let _ = reply.send(result);
        });
        // Cancellation drops the waiter, while unrelated runtime access and
        // eventual kernel completion remain possible.
        drop(waiter);
        assert!(calls.try_lock().unwrap().settled().is_err());
        release.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(
            calls.lock().unwrap().calls[&call].leases[&lease()]
                .pending
                .as_ref()
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .bytes,
            b"late"
        );
    }

    #[test]
    fn known_pin_capture_failure_is_retained_until_exact_acknowledgement() {
        let mut calls = Calls::default();
        let call = NetworkStreamCallId::controlled_fixture(7);
        calls.capture_failed(owner(), call, libc::EPERM).unwrap();
        assert!(calls.settled().is_err());
        assert!(
            calls
                .finish_failed_capture(owner(), call, libc::EBADF)
                .is_err()
        );
        calls
            .finish_failed_capture(owner(), call, libc::EPERM)
            .unwrap();
        calls.settled().unwrap();
    }

    #[test]
    fn invalid_capacity_is_refused_before_kernel_effect() {
        assert!(validate(&Effect::Drain { maximum: 0 }).is_err());
        assert!(validate(&Effect::Drain { maximum: 513 }).is_err());
        assert!(validate(&Effect::Peek { maximum: 1023 }).is_err());
        assert!(
            validate(&Effect::Peek {
                maximum: MAX_RW_COUNT + 1
            })
            .is_err()
        );
    }
}

#[cfg(test)]
mod original_path_tests {
    use super::*;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    #[test]
    fn original_missing_security_hook_is_unknown_for_a_socket_even_with_enotsock_and_zero_misses() {
        let socket = Some(OriginalPin::Socket {
            domain: libc::AF_INET,
            kind: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
        });
        let mut raw: super::super::accepted_provider::OriginalResult = ffi::OriginalResult {
            selection: ffi::OriginalSelection {
                file: 7,
                address_length: 16,
                ..Default::default()
            },
            copy_entered: 1,
            copy_returned: 1,
            returned: -libc::ENOTSOCK,
            complete: 1,
            ..Default::default()
        }
        .into();
        assert!(!original_path_observed(
            socket,
            &raw,
            -i64::from(libc::ENOTSOCK)
        ));
        assert!(original_path_observed(
            Some(OriginalPin::Other),
            &raw,
            -i64::from(libc::ENOTSOCK)
        ));
        assert!(!original_path_observed(
            None,
            &raw,
            -i64::from(libc::ENOTSOCK)
        ));
        raw.security_entered = 1;
        raw.security_returned = 1;
        assert!(original_path_observed(
            socket,
            &raw,
            -i64::from(libc::ENOTSOCK)
        ));
    }
    #[test]
    fn original_copy_fault_needs_positive_copy_completion_and_empty_needs_actual_empty_pin() {
        let socket = Some(OriginalPin::Socket {
            domain: libc::AF_INET,
            kind: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
        });
        let mut raw: super::super::accepted_provider::OriginalResult = ffi::OriginalResult {
            selection: ffi::OriginalSelection {
                file: 7,
                address_length: 16,
                ..Default::default()
            },
            returned: -libc::EFAULT,
            complete: 1,
            ..Default::default()
        }
        .into();
        assert!(!original_path_observed(
            socket,
            &raw,
            -i64::from(libc::EFAULT)
        ));
        raw.copy_entered = 1;
        raw.copy_returned = 1;
        raw.copy_remaining = 1;
        assert!(original_path_observed(
            socket,
            &raw,
            -i64::from(libc::EFAULT)
        ));
        raw.selection.file = 0;
        raw.returned = -libc::EBADF;
        assert!(!original_path_observed(
            socket,
            &raw,
            -i64::from(libc::EBADF)
        ));
        assert!(original_path_observed(
            Some(OriginalPin::Empty),
            &raw,
            -i64::from(libc::EBADF)
        ));
        assert!(original_path_observed(
            Some(OriginalPin::Path),
            &raw,
            -i64::from(libc::EBADF)
        ));
        assert!(!original_path_observed(
            Some(OriginalPin::Other),
            &raw,
            -i64::from(libc::EBADF)
        ));
    }

    fn retained_controlled_helper() -> (
        Calls,
        NetworkStreamOwner,
        NetworkStreamCallId,
        NetworkStreamLeaseId,
        Effect,
        Observation,
        crate::network_replay::NetworkReplayEngine,
    ) {
        let (mut engine, owner, call, lease, effect) =
            crate::network_replay::NetworkReplayEngine::controlled_pending_helper();
        let (original, _peer) = super::tests::pair();
        let mut calls = Calls::default();
        calls
            .capture_authenticated(
                owner,
                call,
                original,
                super::super::original_installation::FileIdentity::controlled_fixture(3, 7),
            )
            .unwrap();
        calls.bind_lease(owner, call, lease).unwrap();
        let work = calls.prepare(owner, lease, effect.clone()).unwrap();
        let held = work.helper.as_ref().unwrap();
        engine.bind_helper_copy(held.binding()).unwrap();
        let observed = held
            .controlled_observation(
                Observation {
                    raw_return: 3,
                    errno: None,
                    bytes: b"abc".to_vec(),
                    confirmation: ResultValue::Peeked { count: 3 },
                    helper_copy: None,
                },
                5,
            )
            .unwrap();
        calls
            .retain(owner, lease, &effect, observed.clone())
            .unwrap();
        drop(work);
        (calls, owner, call, lease, effect, observed, engine)
    }
    #[test]
    fn helper_nonmutating_preflight_rejects_missing_serialized_substituted_or_changed_receipts() {
        let (mut calls, owner, call, lease, effect, actual, engine) = retained_controlled_helper();
        calls
            .preflight_confirmation(owner, lease, &effect, &actual)
            .unwrap();
        let before = format!("{calls:?}");
        let engine_before = format!("{engine:?}");
        for variant in 0..8 {
            let mut changed = actual.clone();
            let mut changed_owner = owner;
            let mut changed_lease = lease;
            let mut changed_effect = effect.clone();
            match variant {
                0 => changed.helper_copy = None,
                1 => {
                    changed = serde_json::from_slice(&serde_json::to_vec(&actual).unwrap()).unwrap()
                }
                2 => changed = retained_controlled_helper().5,
                3 => changed_owner.mm = owner.mm.for_exec(owner.thread),
                4 => changed_lease = serde_json::from_value(serde_json::json!(999)).unwrap(),
                5 => changed_effect = Effect::Peek { maximum: 2048 },
                6 => changed.bytes[0] ^= 1,
                7 => changed.raw_return = 2,
                _ => unreachable!(),
            }
            assert!(
                calls
                    .preflight_confirmation(changed_owner, changed_lease, &changed_effect, &changed)
                    .is_err(),
                "variant {variant}"
            );
            assert!(
                calls
                    .confirm(changed_owner, changed_lease, &changed_effect, &changed)
                    .is_err(),
                "variant {variant}"
            );
            assert_eq!(format!("{calls:?}"), before);
            assert_eq!(format!("{engine:?}"), engine_before);
            assert!(
                !calls.calls[&call].leases[&lease]
                    .pending
                    .as_ref()
                    .unwrap()
                    .confirmed
            );
        }
        // Authentic physical custody passes preflight without discharging any
        // engine semantics or mutating the runtime's confirmed flag.
        calls
            .preflight_confirmation(owner, lease, &effect, &actual)
            .unwrap();
        assert_eq!(format!("{calls:?}"), before);
        assert!(calls.finish_lease(owner, lease).is_err());
    }
    #[test]
    fn helper_unjoined_engine_refusal_does_not_erase_runtime_result_or_confirm_it() {
        let (calls, owner, _, lease, effect, actual, mut engine) = retained_controlled_helper();
        calls
            .preflight_confirmation(owner, lease, &effect, &actual)
            .unwrap();
        let native_before = format!("{calls:?}");
        let engine_before = format!("{engine:?}");
        assert!(
            engine
                .confirm_stream_physical(owner, lease, actual.confirmation.clone())
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), engine_before);
        assert_eq!(format!("{calls:?}"), native_before);
        calls
            .preflight_confirmation(owner, lease, &effect, &actual)
            .unwrap();
    }
    #[test]
    fn helper_final_wait_physically_closes_pin_but_keeps_same_custody_in_terminal_tombstone() {
        let (mut calls, owner, call, lease, effect, actual, engine) = retained_controlled_helper();
        let original = calls.calls[&call].original.as_ref().unwrap().as_raw_fd();
        let engine = std::sync::Arc::new(std::sync::Mutex::new(engine));
        calls
            .retain_capture_publication(
                owner,
                call,
                super::super::NativeCaptureRecovery::new(
                    engine.clone(),
                    std::sync::Arc::new(tokio::sync::Notify::new()),
                    |_| {},
                ),
            )
            .unwrap();
        let held = calls.calls[&call].leases[&lease]
            .pending
            .as_ref()
            .unwrap()
            .helper
            .as_ref()
            .unwrap()
            .clone();
        let binding = held.binding();
        let weak = std::sync::Arc::downgrade(&binding);
        assert!(calls.prepare_release(owner, call).is_err());
        let admission = {
            let mut state = engine.lock().unwrap();
            assert!(state.native_stream_final_wait(owner));
            state
                .terminal_stream_admission(owner, call)
                .unwrap()
                .unwrap()
        };
        assert!(calls.claim_terminal_release(admission).unwrap());
        let work = calls.prepare_terminal_release(admission).unwrap();
        let release = work.perform();
        calls.retain_release(owner, call, release).unwrap();
        assert_eq!(unsafe { libc::fcntl(original, libc::F_GETFD) }, -1);
        assert_eq!(errno(), libc::EBADF);
        let evidence = calls.terminal_evidence(admission).unwrap();
        assert!(evidence.matches(admission));
        assert_eq!(evidence.leases[&lease].effect.as_ref(), Some(&effect));
        assert_eq!(evidence.leases[&lease].result.as_ref(), Some(&actual));
        assert!(!evidence.leases[&lease].confirmed);
        engine
            .lock()
            .unwrap()
            .retain_terminal_stream_release(admission, evidence)
            .unwrap();
        calls.finish_release(owner, call).unwrap();
        calls.settled().unwrap();
        drop(actual);
        drop(binding);
        drop(held);
        drop(calls);
        assert!(weak.upgrade().is_some());
        // Actual physical closure was possible; it was not a receive ACK.
        assert!(matches!(engine.lock().unwrap().finish(),
            Err(crate::network_replay::NetworkReplayError::UnresolvedStreamCall(id)) if id==call));
        drop(engine);
        assert!(weak.upgrade().is_none());
    }
}

impl super::NetworkRuntimeResources {
    /// Engine and actual held-file lease move together under the existing Calls
    /// mutex. The old Pending must already be positively confirmed and joined.
    /// No native effect, wait or custody mutex occurs inside this transaction.
    pub(crate) fn reserve_private_receive_span(
        &self,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        probe: NetworkStreamLeaseId,
        maximum: usize,
        offset: usize,
    ) -> io::Result<crate::network_replay::NetworkStreamChunk> {
        use crate::network_replay::NetworkStreamChunk;
        let source = engine
            .private_receive_completion(owner, probe)
            .map_err(io::Error::other)?
            .clone();
        source.joined_worker()?.validate_runtime(&self.shared)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.owned(owner, call)?;
        let old = state
            .leases
            .get(&probe)
            .ok_or_else(|| io::Error::other("private selection lost its actual old probe lease"))?;
        if state.acquisition.is_err()
            || state.original.is_none()
            || state.invocation.is_some()
            || state.releasing
            || state.release.is_some()
            || state.terminal.is_some()
            || state
                .identity
                .is_none_or(|id| !source.binding().matches_file(id))
            || source.binding().call() != call
            || source.binding().owner() != owner
            || source.binding().lease() != probe
            || old.private_predecessor.is_some()
            || state.leases.len() != 1
            || old.peek.as_ref().and_then(|p| p.helper_copy.as_ref()) != Some(&source)
            || old
                .pending
                .as_ref()
                .is_none_or(|p| !p.confirmed || p.result.is_none())
        {
            return Err(io::Error::other(
                "private selection changed exact confirmed source/file custody",
            ));
        }
        let selected = engine
            .reserve_private_receive_span(owner, call, probe, maximum, offset)
            .map_err(io::Error::other)?;
        let NetworkStreamChunk::Reserved { lease, .. } = &selected else {
            unreachable!("private nonempty source reserves a delivery")
        };
        // Engine lease allocation is global and this transaction retains Calls.
        // Preserve predecessor before dropping the actual old Pending.
        let new = Lease {
            private_predecessor: Some(source),
            ..Lease::default()
        };
        assert!(
            state.leases.insert(*lease, new).is_none(),
            "engine allocated a live native lease"
        );
        state.leases.remove(&probe).expect("validated old probe");
        Ok(selected)
    }

    pub(crate) async fn execute_private_receive_drain(
        &self,
        full: crate::network_replay::FullStoreCompletion,
    ) -> io::Result<Observation> {
        #[cfg(test)]
        {
            let engine = {
                let mut fixture = self.controlled_private_drain.lock().unwrap();
                match fixture.as_mut() {
                    None => None,
                    Some(fixture) => {
                        if fixture.owner != full.store().owner()
                            || fixture.call != full.store().call()
                        {
                            return Err(io::Error::other("controlled Drain changed owner/Call"));
                        }
                        Some(fixture.engine.take().ok_or_else(|| {
                            io::Error::other("controlled Drain was already consumed")
                        })?)
                    }
                }
            }; // Never hold the fixture mutex across the real worker/join.
            if let Some(engine) = engine {
                return self
                    .controlled_private_drain(full, engine, 5, "none", true)
                    .await;
            }
        }
        self.shared.execute_private_receive_drain(full).await
    }

    /// Join the engine plan to the actual positively confirmed Pending while
    /// both engine and Calls remain held. Preparation retires neither owner.
    pub(crate) fn prepare_private_receive_publication(
        &self,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        full: &crate::network_replay::FullStoreCompletion,
    ) -> io::Result<std::sync::Arc<crate::network_replay::PreparedPrivatePublication>> {
        let plan = engine
            .plan_private_receive_publication(full)
            .map_err(io::Error::other)?;
        let completion = plan
            .observed()
            .helper_copy
            .as_ref()
            .ok_or_else(|| io::Error::other("private publication lacks Drain custody"))?;
        completion.joined_worker()?.validate_runtime(&self.shared)?;
        full.store()
            .record_completion()
            .map_err(io::Error::other)?
            .joined_worker()?
            .validate_runtime(&self.shared)?;
        let calls = self.shared.native_streams.lock().unwrap();
        let call = calls
            .calls
            .get(&full.store().call())
            .ok_or_else(|| io::Error::other("private publication lost its actual held Call"))?;
        let confirmed = ConfirmedPrivateDrain {
            call,
            lease: full.store().lease(),
        };
        if !confirmed.matches(plan.full(), plan.observed()) {
            return Err(io::Error::other(
                "private publication lacks exact confirmed same-file Pending",
            ));
        }
        engine
            .attach_private_receive_publication(plan, &confirmed)
            .map_err(io::Error::other)
    }
}

/// A nonserializable borrow of the real Call/Pending under Calls' mutex. Its
/// private constructor and lifetime prevent a copied flag or missing lease
/// from granting publication preparation after the actual custody is gone.
pub(crate) struct ConfirmedPrivateDrain<'a> {
    call: &'a Call,
    lease: NetworkStreamLeaseId,
}
impl ConfirmedPrivateDrain<'_> {
    pub(crate) fn matches(
        &self,
        full: &crate::network_replay::FullStoreCompletion,
        observed: &Observation,
    ) -> bool {
        let store = full.store();
        let Ok(record_source) = store.record_completion() else {
            return false;
        };
        let Some(record) = self.call.leases.get(&self.lease) else {
            return false;
        };
        let Some(pending) = &record.pending else {
            return false;
        };
        let Some(completion) = &observed.helper_copy else {
            return false;
        };
        self.call.owner == store.owner()
            && self.call.id == store.call()
            && self.lease == store.lease()
            && self.call.acquisition.is_ok()
            && self.call.original.is_some()
            && self.call.invocation.is_none()
            && !self.call.releasing
            && self.call.release.is_none()
            && self.call.terminal.is_none()
            && self.call.leases.len() == 1
            && record.peek.is_none()
            && record.private_predecessor.as_ref() == Some(record_source)
            && self.call.identity.is_some_and(|id| {
                completion.binding().matches_file(id) && record_source.binding().matches_file(id)
            })
            && completion.binding().succeeds(record_source)
            && completion.binding().lease() == self.lease
            && pending.confirmed
            && pending.result.as_ref() == Some(observed)
            && pending.effect
                == (Effect::Drain {
                    maximum: store.length(),
                })
            && pending.effect == *completion.binding().effect()
            && pending
                .helper
                .as_ref()
                .is_some_and(|helper| helper.check_completion(Some(completion)).is_ok())
    }
}

#[cfg(test)]
impl super::NetworkRuntimeResources {
    /// Opt in once for this actual held Call. This supplies only controlled
    /// provider geometry; the existing helper still performs the real Drain,
    /// retains its exact result and joins its original worker.
    pub(crate) fn arm_controlled_private_drain(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        engine: std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>,
    ) -> io::Result<()> {
        let mut fixture = self.controlled_private_drain.lock().unwrap();
        if fixture.is_some() {
            return Err(io::Error::other("controlled Drain cannot be rearmed"));
        }
        let mut calls = self.shared.native_streams.lock().unwrap();
        let actual = calls.owned(owner, call)?;
        if actual.original.is_none()
            || actual.releasing
            || actual.release.is_some()
            || actual.terminal.is_some()
            || actual
                .publication
                .as_ref()
                .is_none_or(|publication| !std::ptr::eq(publication.engine(), engine.as_ref()))
        {
            return Err(io::Error::other(
                "controlled Drain lacks its held Call/engine",
            ));
        }
        *fixture = Some(ControlledPrivateDrain {
            owner,
            call,
            engine: Some(engine),
        });
        Ok(())
    }

    pub(crate) fn controlled_private_drain_consumed(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> io::Result<bool> {
        let fixture = self.controlled_private_drain.lock().unwrap();
        let fixture = fixture
            .as_ref()
            .filter(|fixture| fixture.owner == owner && fixture.call == call)
            .ok_or_else(|| io::Error::other("controlled Drain lacks its exact fixture"))?;
        Ok(fixture.engine.is_none())
    }

    pub(crate) fn private_receive_original_fixture_fd(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> i32 {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .owned(owner, call)
            .unwrap()
            .original
            .as_ref()
            .unwrap()
            .as_raw_fd()
    }

    pub(crate) fn private_receive_lease_fixture_state(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> (usize, bool, bool) {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.owned(owner, call).unwrap();
        (
            state.leases.len(),
            state
                .leases
                .values()
                .any(|l| l.private_predecessor.is_some()),
            state
                .leases
                .values()
                .any(|l| l.pending.as_ref().is_some_and(|p| !p.confirmed)),
        )
    }
}

#[cfg(test)]
impl super::NetworkRuntimeResources {
    pub(crate) fn private_publication_runtime_fixture_state(&self) -> String {
        format!("{:?}", self.shared.native_streams.lock().unwrap())
    }
    pub(crate) fn change_private_publication_runtime_fixture(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        variant: usize,
    ) {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.owned(owner, call).unwrap();
        let record = state.leases.values_mut().next().unwrap();
        match variant {
            0 => record.pending.as_mut().unwrap().confirmed = false,
            1 => record.pending = None,
            2 => record.private_predecessor = None,
            3 => record.pending.as_mut().unwrap().effect = Effect::Drain { maximum: 1 },
            4 => record.pending.as_mut().unwrap().helper = None,
            5 => state.releasing = true,
            6 => state.identity = None,
            7 => {
                record
                    .pending
                    .as_mut()
                    .unwrap()
                    .result
                    .as_mut()
                    .unwrap()
                    .helper_copy = None
            }
            _ => panic!("unknown explicit private runtime mutation"),
        }
    }
}

impl super::NetworkRuntimeResources {
    pub(crate) fn publish_private_native_receive(
        &self,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        prepared: &std::sync::Arc<crate::network_replay::PreparedPrivatePublication>,
        now: detcore_model::time::LogicalTime,
    ) -> io::Result<usize> {
        let full = prepared.full();
        let store = full.store();
        store
            .record_completion()
            .map_err(io::Error::other)?
            .joined_worker()?
            .validate_runtime(&self.shared)?;
        prepared
            .observed()
            .helper_copy
            .as_ref()
            .ok_or_else(|| io::Error::other("publication lost actual Drain"))?
            .joined_worker()?
            .validate_runtime(&self.shared)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        let actual = calls
            .calls
            .get(&store.call())
            .ok_or_else(|| io::Error::other("publication lost actual same Call"))?;
        let confirmed = ConfirmedPrivateDrain {
            call: actual,
            lease: store.lease(),
        };
        if !confirmed.matches(full, prepared.observed()) {
            return Err(io::Error::other(
                "publication changed confirmed Pending before lease retirement",
            ));
        }
        let count = engine
            .publish_private_native_receive(prepared, &confirmed, now)
            .map_err(io::Error::other)?;
        // Both owners remain locked; there is no fallible operation between
        // semantic consumption and retiring this exact positively joined lease.
        calls
            .calls
            .get_mut(&store.call())
            .unwrap()
            .leases
            .remove(&store.lease())
            .expect("validated exact same-Call lease");
        Ok(count)
    }

    pub(crate) fn publish_native_connected(
        &self,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        root: &std::sync::Arc<super::ForegroundRoot>,
        now: detcore_model::time::LogicalTime,
    ) -> io::Result<()> {
        let calls = self.shared.native_streams.lock().unwrap();
        let call = calls
            .calls
            .get(&admission.call)
            .ok_or_else(|| io::Error::other("V4 Connect publication lost actual Call"))?;
        let completed = CompletedNativeConnect { call };
        engine
            .publish_native_connected(owner, admission, root, &completed, now)
            .map_err(io::Error::other)
    }

    pub(crate) fn publish_native_sent(
        &self,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        admission: &OriginalAdmission,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: detcore_model::time::LogicalTime,
    ) -> io::Result<()> {
        let calls = self.shared.native_streams.lock().unwrap();
        let call = calls.calls.get(&admission.call)
            .ok_or_else(|| io::Error::other("Sendto publication lost actual Call"))?;
        engine.publish_native_sent(admission, grant, &CompletedNativeSend { call }, now)
            .map_err(io::Error::other)
    }
}

/// Only a borrow of the real runtime Call can supply bytes to the V4 writer.
pub(crate) struct CompletedNativeSend<'a> { call: &'a Call }
impl CompletedNativeSend<'_> {
    pub(crate) fn capture(&self, owner: NetworkStreamOwner, admission: &OriginalAdmission)
        -> io::Result<&super::original_send::Capture>
    {
        let call = self.call;
        let original = call.invocation.as_ref()
            .ok_or_else(|| io::Error::other("Sendto invocation absent"))?;
        let effect = original.completion.as_ref()
            .ok_or_else(|| io::Error::other("Sendto completion absent"))?;
        if call.owner != owner || call.id != admission.call || original.admission != *admission
            || admission.arguments.kind != crate::network_replay::original_connect::Kind::Sendto
            || call.acquisition.is_err() || call.terminal.is_some() || !call.releasing
            || call.original.is_some() || !call.leases.is_empty()
            || call.release.is_none_or(|r| r.original == Some(libc::EBADF))
            || !original.retired || !original.close_queued || original.canceled
            || original.terminating || original.terminal.is_some()
            || original.prepared.map(|p| p.1) != Some(effect.original.selection.command)
            || original.selection.as_ref() != Some(&effect.original.selection)
        {
            return Err(io::Error::other("Sendto publication lacks original return/retirement"));
        }
        let capture = effect.send.as_ref()
            .ok_or_else(|| io::Error::other("Sendto capture absent"))?;
        capture.validate(effect)?;
        Ok(capture)
    }
}

/// A synchronous borrow of the retained original native completion. The raw
/// provider response is never accepted through an Outcome/RPC reconstruction.
pub(crate) struct CompletedNativeConnect<'a> {
    call: &'a Call,
}
impl CompletedNativeConnect<'_> {
    pub(crate) fn endpoints(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        returned: i64,
    ) -> io::Result<(detcore_model::network_trace::NetworkAddressV2, detcore_model::network_trace::NetworkAddressV2)> {
        use detcore_model::network_trace::NetworkAddressV2;
        let call = self.call;
        let original = call
            .invocation
            .as_ref()
            .ok_or_else(|| io::Error::other("Connect invocation absent"))?;
        let effect = original
            .completion
            .as_ref()
            .ok_or_else(|| io::Error::other("Connect native completion absent"))?;
        let raw = &effect.original;
        if call.owner != owner
            || call.id != admission.call
            || original.admission != *admission
            || admission.arguments.kind != crate::network_replay::original_connect::Kind::Connect
            || call.acquisition.is_err()
            || call.terminal.is_some()
            || !call.releasing
            || call.original.is_some()
            || !call.leases.is_empty()
            || call.release.is_none_or(|r| r.original == Some(libc::EBADF))
            || !original.retired
            || !original.close_queued
            || original.canceled
            || original.terminating
            || original.terminal.is_some()
            || original.prepared.map(|p| p.1) != Some(raw.selection.command)
            || original.selection.as_ref() != Some(&raw.selection)
            || effect.command.operation != 7
            || (returned != 0 && returned != -i64::from(libc::EINPROGRESS))
            || i64::from(effect.command.returned) != returned
            || effect.command.phase != 1
            || i64::from(raw.returned) != returned
            || raw.complete != 1
            || raw.problem != 0
            || raw.selection.file == 0
            || raw.security_entered != 1
            || raw.security_returned != 1
            || raw.security_result != 0
            || raw.copy_entered != 1
            || raw.copy_returned != 1
            || raw.copy_remaining != 0
            || raw.address.len() != 128
        {
            return Err(io::Error::other(
                "Connect publication lacks exact successful original selection/copy/return/retirement",
            ));
        }
        let length = usize::try_from(raw.selection.address_length).map_err(io::Error::other)?;
        let bytes = raw
            .address
            .get(..length)
            .ok_or_else(|| io::Error::other("Connect sockaddr length exceeds retained copy"))?;
        let family = bytes
            .get(..2)
            .map(|b| i32::from(u16::from_ne_bytes([b[0], b[1]])))
            .ok_or_else(|| io::Error::other("Connect sockaddr has no family"))?;
        let peer = match (family, original.pin) {
            (
                libc::AF_INET,
                Some(OriginalPin::Socket {
                    domain: libc::AF_INET,
                    kind: libc::SOCK_STREAM,
                    protocol: libc::IPPROTO_TCP,
                }),
            ) if bytes.len() >= std::mem::size_of::<libc::sockaddr_in>() => {
                Ok(NetworkAddressV2::Inet4 {
                    address: bytes[4..8].try_into().unwrap(),
                    port: u16::from_be_bytes(bytes[2..4].try_into().unwrap()),
                })
            }
            (
                libc::AF_INET6,
                Some(OriginalPin::Socket {
                    domain: libc::AF_INET6,
                    kind: libc::SOCK_STREAM,
                    protocol: libc::IPPROTO_TCP,
                }),
            ) if bytes.len() >= std::mem::offset_of!(libc::sockaddr_in6, sin6_scope_id) => {
                Ok(NetworkAddressV2::Inet6 {
                    address: bytes[8..24].try_into().unwrap(),
                    port: u16::from_be_bytes(bytes[2..4].try_into().unwrap()),
                    flowinfo: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
                    scope_id: bytes
                        .get(24..28)
                        .map_or(0, |b| u32::from_ne_bytes(b.try_into().unwrap())),
                })
            }
            _ => Err(io::Error::other(
                "V4 Connect completion changed TCP family or captured peer",
            )),
        }?;
        let observed = original.early_connect.as_ref()
            .ok_or_else(|| io::Error::other("Connect has no retained endpoint observation"))?
            .as_ref().map_err(|error| io::Error::other(error.clone()))?;
        observed.confirm(&peer)?;
        Ok((peer, observed.local().clone()))
    }
}

#[cfg(test)]
pub(crate) mod native_connected_tests;

#[cfg(test)]
impl super::NetworkRuntimeResources {
    /// Capture the actual duplicated original OFD in the existing owned worker.
    /// File identity remains the explicit controlled provider premise.
    pub(crate) async fn controlled_recapture_no_store(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        original: OwnedFd,
        recovery: super::NativeCaptureRecovery,
    ) -> io::Result<crate::network_replay::NetworkStreamPinOutcome> {
        self.capture_native_stream_with_identity(
            owner,
            call,
            move || Ok(original),
            Some(super::original_installation::FileIdentity::controlled_fixture(7, 19)),
            recovery,
        )
        .await
    }

    /// Exact same captured file, actual Peek/Pending/native syscall and actual
    /// joined worker. Zero-unit provider manifest is controlled component data.
    pub(crate) async fn controlled_existing_no_store_peek(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        engine: std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>,
    ) -> io::Result<Observation> {
        let effect = Effect::Peek { maximum: 1024 };
        let work = {
            let mut calls = self.shared.native_streams.lock().unwrap();
            if !calls.owned(owner, call)?.leases.contains_key(&lease) {
                return Err(io::Error::other(
                    "same-OFD helper changed its existing cursor probe",
                ));
            }
            calls.prepare(owner, lease, effect.clone())?
        };
        let held = work.helper.as_ref().unwrap().clone();
        engine
            .lock()
            .unwrap()
            .bind_helper_copy(held.binding())
            .map_err(io::Error::other)?;
        let shared = self.shared.clone();
        let retained_effect = effect.clone();
        let (worker, reply) =
            self.shared
                .start_native_worker(tokio::runtime::Handle::current(), move || {
                    let observed = work.perform();
                    if observed.raw_return != 0
                        || observed.errno.is_some()
                        || !observed.bytes.is_empty()
                    {
                        return Err(io::Error::other(
                            "same-OFD EOF fixture did not observe actual zero",
                        ));
                    }
                    let observed = held.controlled_observation(observed, 5)?;
                    shared.native_streams.lock().unwrap().retain(
                        owner,
                        lease,
                        &retained_effect,
                        observed.clone(),
                    )?;
                    Ok(observed)
                })?;
        let observed = tokio::time::timeout(std::time::Duration::from_secs(1), reply)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)??;
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            self.shared.join_native_worker_receipt(&worker),
        )
        .await
        .map_err(io::Error::other)??;
        super::helper_receive::retain_joined_helper(
            &self.shared,
            owner,
            lease,
            &effect,
            &observed,
            joined,
        )?;
        Ok(observed)
    }
}
