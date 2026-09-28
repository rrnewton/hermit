//! Run-owned controller requests. Futures borrow this owner; submission, SCM
//! capabilities and responses remain here if a caller disappears mid-request.

pub(crate) mod copy_wire_authority;

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::accepted_transport::AcceptedSession;
use super::accepted_transport::Envelope;
use super::accepted_transport::ObservationReceipt;
use super::accepted_transport::Operation;
use super::accepted_transport::Received;
use crate::network_replay::NetworkAcceptLeaseId;
use crate::network_replay::NetworkStreamLeaseId;
use crate::network_replay::NetworkStreamOwner;
use crate::types::OpenFileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Effect {
    PrepareNativeBirth(NetworkStreamLeaseId),
    ObserveNativeBirth(NetworkStreamLeaseId),
    CollectNativeBirth(NetworkStreamLeaseId),
    CancelNativeBirth(NetworkStreamLeaseId),
    TerminateNativeBirth(NetworkStreamLeaseId),
    RetireNativeBirth(NetworkStreamLeaseId),
    ObserveTerminalSocket(crate::network_replay::NetworkStreamCallId),
    RetireTerminalSocketObservation(crate::network_replay::NetworkStreamCallId),
    PrepareOriginalFileObservation(crate::network_replay::NetworkStreamCallId),
    CollectOriginalFileObservation(crate::network_replay::NetworkStreamCallId),
    RetireOriginalFileObservation(crate::network_replay::NetworkStreamCallId),
    PrepareOriginalConnect(crate::network_replay::NetworkStreamCallId),
    AwaitOriginalSelection(crate::network_replay::NetworkStreamCallId),
    CollectOriginalConnect(crate::network_replay::NetworkStreamCallId),
    ReadOriginalCopy(crate::network_replay::NetworkStreamCallId, u64),
    CancelOriginalConnect(crate::network_replay::NetworkStreamCallId),
    TerminateOriginalConnect(crate::network_replay::NetworkStreamCallId),
    RetireOriginalConnect(crate::network_replay::NetworkStreamCallId),
    Listener(OpenFileId),
    Match(NetworkAcceptLeaseId),
    PrepareTableEnrollment(u64),
    CollectTableEnrollment(u64),
    PrepareAccept(NetworkAcceptLeaseId),
    CollectAccept(NetworkAcceptLeaseId),
    PrepareSetter(NetworkStreamLeaseId),
    FinishSetter(NetworkStreamLeaseId),
    // Observation cuts get a run-owned identity, not a guest TID/syscall order.
    Observation(u64),
    ObservationRetirement(u64),
    FdPublicationCut(NetworkStreamLeaseId),
    OriginalAllocatorCut(crate::network_replay::NetworkStreamCallId),
    FdObservation(u64),
    FdObservationRetirement(u64),
}

/// These are consumption facts in the original retained request, not a new
/// birth registry. They cannot authorize a child or a table publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BirthSemantic {
    Child { child: i32, terminal: bool },
    Failed,
    Uninvoked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BirthCompletion {
    Returned(Result<i64, i32>),
    Uninvoked,
    CreatorTerminal,
}
#[derive(Debug)]
struct Submitted {
    owner: NetworkStreamOwner,
    operation: Operation,
    body: Vec<u8>,
    sequence: Option<u64>,
    error: Option<String>,
    copy_authority_issued: bool,
    birth_semantic: Option<BirthSemantic>,
    birth_completion: Option<BirthCompletion>,
    birth_creator_terminal: bool,
    birth_cleanup: Option<std::sync::Arc<super::native_birth::NativeBirthCleanup>>,
}
#[derive(Debug, Default)]
struct Requests(BTreeMap<Effect, Submitted>, u64, u64);
impl Requests {
    fn consume_birth(
        &mut self,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        semantic: Option<BirthSemantic>,
        completion: Option<BirthCompletion>,
    ) -> io::Result<()> {
        let Some(entry) = self.0.get_mut(&Effect::PrepareNativeBirth(permit.lease)) else {
            return Err(io::Error::other(
                "birth consumption lost retained preparation",
            ));
        };
        if entry.owner != permit.owner
            || entry.operation != Operation::PrepareNativeBirth
            || !matches!(serde_json::from_slice::<Request>(&entry.body),Ok(Request::PrepareNativeBirth{call,mm,table,..})
                if call==permit.native_command_call() && mm==permit.owner.mm.generation() && table!=0)
            || semantic.is_some_and(|v| entry.birth_semantic.is_some_and(|old| old != v))
            || completion.is_some_and(|v| entry.birth_completion.is_some_and(|old| old != v))
        {
            return Err(io::Error::other(
                "birth consumption changed original command/disposition",
            ));
        }
        if let Some(value) = semantic {
            entry.birth_semantic = Some(value);
        }
        if let Some(value) = completion {
            entry.birth_completion = Some(value);
        }
        Ok(())
    }

    fn retire_observation(&mut self, key: Effect, sequence: u64) -> io::Result<()> {
        let (id, highwater, operation) = match key {
            Effect::Observation(id) => (id, &mut self.1, Operation::DrainCreations),
            Effect::FdObservation(id) => (id, &mut self.2, Operation::DrainFdJournal),
            _ => return Err(io::Error::other("not a read-only observation")),
        };
        let entry = self
            .0
            .get(&key)
            .ok_or_else(|| io::Error::other("unknown observation effect"))?;
        if id <= *highwater || entry.sequence != Some(sequence) || entry.operation != operation {
            return Err(io::Error::other("observation retirement identity changed"));
        }
        self.0.remove(&key);
        *highwater = id;
        Ok(())
    }
}
impl Requests {
    // Listener enrollment is an OFD effect. A newly authenticated alias caller
    // may recover its original response, never replace the original envelope or
    // re-run the provider operation under another task identity.
    fn listener_sequence(
        &self,
        open_file: OpenFileId,
        operation: Operation,
        body: &[u8],
    ) -> io::Result<Option<u64>> {
        let Some(prior) = self.0.get(&Effect::Listener(open_file)) else {
            return Ok(None);
        };
        if operation != Operation::EnrollListener
            || prior.operation != operation
            || prior.body != body
        {
            return Err(io::Error::other(
                "listener recovery changed its retained request",
            ));
        }
        prior.sequence.map(Some).ok_or_else(|| {
            io::Error::other(
                prior
                    .error
                    .clone()
                    .unwrap_or_else(|| "listener request remains unresolved".into()),
            )
        })
    }
    fn prepare(
        &mut self,
        key: Effect,
        owner: NetworkStreamOwner,
        operation: Operation,
        body: &[u8],
        submit: impl FnOnce() -> io::Result<u64>,
    ) -> io::Result<u64> {
        if matches!(key, Effect::Observation(id) if id <= self.1)
            || matches!(key,Effect::FdObservation(id) if id <= self.2)
        {
            return Err(io::Error::other(
                "retired observation cannot be submitted again",
            ));
        }
        if let Some(prior) = self.0.get(&key) {
            if prior.owner != owner || prior.operation != operation || prior.body != body {
                return Err(io::Error::other(
                    "provider effect changed its retained request",
                ));
            }
            return prior.sequence.ok_or_else(|| {
                io::Error::other(
                    prior
                        .error
                        .clone()
                        .unwrap_or_else(|| "provider effect preparation remains unresolved".into()),
                )
            });
        }
        let birth_finish = match key {
            Effect::CollectNativeBirth(lease)
            | Effect::CancelNativeBirth(lease)
            | Effect::TerminateNativeBirth(lease) => Some(lease),
            _ => None,
        };
        if let Some(lease) = birth_finish {
            for other in [
                Effect::CollectNativeBirth(lease),
                Effect::CancelNativeBirth(lease),
                Effect::TerminateNativeBirth(lease),
            ] {
                if other != key && self.0.contains_key(&other) {
                    return Err(io::Error::other(
                        "birth already has an exact retained physical completion owner",
                    ));
                }
            }
        }
        self.0.insert(
            key,
            Submitted {
                owner,
                operation,
                body: body.to_vec(),
                sequence: None,
                error: None,
                copy_authority_issued: false,
                birth_semantic: None,
                birth_completion: None,
                birth_creator_terminal: false,
                birth_cleanup: None,
            },
        );
        match submit() {
            Ok(sequence) => {
                self.0.get_mut(&key).unwrap().sequence = Some(sequence);
                Ok(sequence)
            }
            Err(error) => {
                self.0.get_mut(&key).unwrap().error = Some(error.to_string());
                Err(error)
            }
        }
    }
}

#[derive(Debug)]
struct State {
    session: AcceptedSession,
    requests: Requests,
    pending_send: VecDeque<u64>,
    rejected_rights: Vec<Vec<OwnedFd>>,
    failure: Option<String>,
}

#[derive(Debug)]
pub(super) struct Controller {
    state: Mutex<State>,
    // This readiness-only alias and the session's endpoint refer to the same
    // private socket. No task future can own either descriptor's last reference.
    ready: OwnedFd,
    // Requests are submitted by scheduler futures while the run-owned driver
    // can be blocked waiting for provider input. This eventfd makes submission
    // an edge for that native poll rather than a 50 ms sampling delay.
    driver_wake: OwnedFd,
    run: [u8; 16],
    wire_format: Option<super::ProviderWireFormat>,
    copy_authority_owner: std::sync::Arc<()>,
    changed: tokio::sync::Notify,
}
impl Controller {
    /// The owned native driver calls this even if every RPC future was dropped.
    /// IO is nonblocking under the mutex; the bounded poll holds no model lock.
    pub(super) fn drive_once(&self) -> io::Result<()> {
        let writable = {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.failure {
                return Err(io::Error::other(error.clone()));
            }
            if let Err(error) = Self::progress_io(&mut state, &self.changed, self.run) {
                state.failure = Some(error.to_string());
                self.changed.notify_waiters();
                return Err(error);
            }
            !state.pending_send.is_empty()
        };
        let mut descriptors = [
            libc::pollfd {
                fd: self.ready.as_raw_fd(),
                events: libc::POLLIN | if writable { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: self.driver_wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 50) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                self.fail(&error);
                return Err(error);
            }
        }
        if descriptors[1].revents & libc::POLLIN != 0 {
            self.drain_driver_wake()?;
        }
        Ok(())
    }

    fn wake_driver(&self) -> io::Result<()> {
        let value = 1_u64;
        loop {
            let written = unsafe {
                libc::write(
                    self.driver_wake.as_raw_fd(),
                    (&value as *const u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if written == std::mem::size_of::<u64>() as isize {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // A saturated eventfd is already a retained wake request.
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(error);
        }
    }

    fn drain_driver_wake(&self) -> io::Result<()> {
        let mut value = 0_u64;
        loop {
            let read = unsafe {
                libc::read(
                    self.driver_wake.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read == std::mem::size_of::<u64>() as isize {
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(error);
        }
    }

    pub(super) fn retained_response(&self, sequence: u64) -> io::Result<Option<Reply>> {
        let state = self.state.lock().unwrap();
        // A known response remains available after a later transport failure.
        if let Some(bytes) = state.session.response(sequence)? {
            return serde_json::from_slice(bytes)
                .map(Some)
                .map_err(io::Error::other);
        }
        if let Some(error) = &state.failure {
            return Err(io::Error::other(error.clone()));
        }
        Ok(None)
    }

    /// An independently owned physical operation may outlive its last reply.
    /// Wait on this existing sticky failure and Notify, without a probe timer
    /// or a second failure owner, so its callback cannot hang after Driver exit.
    pub(super) async fn failure(&self) -> io::Error {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(error) = self.state.lock().unwrap().failure.clone() {
                return io::Error::other(error);
            }
            changed.await;
        }
    }

    pub(super) fn fail(&self, error: &io::Error) {
        self.state
            .lock()
            .unwrap()
            .failure
            .get_or_insert_with(|| error.to_string());
        self.changed.notify_waiters();
    }

    pub(super) fn quiescent(&self) -> io::Result<bool> {
        let state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(io::Error::other(error.clone()));
        }
        if !state.pending_send.is_empty() {
            return Ok(false);
        }
        if state
            .requests
            .0
            .keys()
            .any(|key| matches!(key, Effect::PrepareNativeBirth(_)))
        {
            return Ok(false);
        }
        for request in state.requests.0.values() {
            let Some(sequence) = request.sequence else {
                return Err(io::Error::other("retained request has no known submission"));
            };
            if state.session.response(sequence)?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn accepted_preparations(
        &self,
    ) -> io::Result<Vec<(NetworkStreamOwner, NetworkAcceptLeaseId, u64)>> {
        self.state
            .lock()
            .unwrap()
            .requests
            .0
            .iter()
            .filter_map(|(effect, request)| {
                let Effect::PrepareAccept(lease) = effect else {
                    return None;
                };
                Some(
                    request
                        .sequence
                        .map(|sequence| (request.owner, *lease, sequence))
                        .ok_or_else(|| {
                            io::Error::other("accepted preparation submission is unresolved")
                        }),
                )
            })
            .collect()
    }

    pub(super) fn from_startup(
        endpoint: OwnedFd,
        run: [u8; 16],
        wire_format: super::ProviderWireFormat,
    ) -> io::Result<Self> {
        Self::with_wire(endpoint, run, Some(wire_format))
    }

    #[cfg(test)]
    pub(super) fn new(endpoint: OwnedFd, run: [u8; 16]) -> io::Result<Self> {
        Self::with_wire(endpoint, run, None)
    }

    fn with_wire(
        endpoint: OwnedFd,
        run: [u8; 16],
        wire_format: Option<super::ProviderWireFormat>,
    ) -> io::Result<Self> {
        // The runtime retains its original endpoint through this setup. Closing
        // an unused duplicate on failure cannot release the original channel.
        let readiness = endpoint.as_fd().try_clone_to_owned()?;
        let ready = readiness;
        let session = AcceptedSession::from_wire(
            endpoint,
            run,
            wire_format.unwrap_or(super::ProviderWireFormat::Abi7Copy4),
        )
        .map_err(|(error, _)| error)?;
        let raw_driver_wake = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if raw_driver_wake < 0 {
            return Err(io::Error::last_os_error());
        }
        let driver_wake = unsafe { OwnedFd::from_raw_fd(raw_driver_wake) };
        Ok(Self {
            state: Mutex::new(State {
                session,
                requests: Requests::default(),
                pending_send: VecDeque::new(),
                rejected_rights: Vec::new(),
                failure: None,
            }),
            ready,
            driver_wake,
            run,
            wire_format,
            copy_authority_owner: std::sync::Arc::new(()),
            changed: tokio::sync::Notify::new(),
        })
    }

    /// All source pins are run-owned before entry. `duplicate_rights` borrows
    /// them and executes synchronously exactly once, before the first await.
    pub(super) fn prepare(
        &self,
        key: Effect,
        owner: NetworkStreamOwner,
        request: &Request,
        duplicate_rights: impl FnOnce() -> io::Result<Vec<OwnedFd>>,
    ) -> io::Result<u64> {
        let operation = match request {
            Request::AwaitFdEvent { .. }
            | Request::RetireFdObservation { .. }
            | Request::ReadFdPublicationCut { .. }
            | Request::ReadOriginalAllocatorCut { .. } => Operation::DrainFdJournal,
            Request::Enroll { .. } => Operation::EnrollListener,
            Request::ReadStatus
            | Request::ReadCreation { .. }
            | Request::AwaitCreation { .. }
            | Request::RetireObservation { .. } => Operation::DrainCreations,
            Request::PrepareSetter { .. } => Operation::PrepareSetter,
            Request::FinishSetter { .. } => Operation::FinishSetter,
            Request::PrepareNativeBirth { .. } => Operation::PrepareNativeBirth,
            Request::ObserveNativeBirth { .. } => Operation::ObserveNativeBirth,
            Request::CollectNativeBirth { .. } => Operation::CollectNativeBirth,
            Request::CancelNativeBirth { .. } => Operation::CancelNativeBirth,
            Request::TerminateNativeBirth { .. } => Operation::TerminateNativeBirth,
            Request::RetireNativeBirth { .. } => Operation::RetireNativeBirth,
            Request::ObserveTerminalSocket { .. } => Operation::ObserveTerminalSocket,
            Request::RetireTerminalSocketObservation { .. } => {
                Operation::RetireTerminalSocketObservation
            }
            Request::PrepareOriginalFileObservation { .. } => {
                Operation::PrepareOriginalFileObservation
            }
            Request::CollectOriginalFileObservation { .. } => {
                Operation::CollectOriginalFileObservation
            }
            Request::RetireOriginalFileObservation { .. } => {
                Operation::RetireOriginalFileObservation
            }
            Request::PrepareOriginalConnect { .. } => Operation::PrepareOriginalConnect,
            Request::AwaitOriginalSelection { .. } => Operation::AwaitOriginalSelection,
            Request::CollectOriginalConnect { .. } => Operation::CollectOriginalConnect,
            Request::ReadOriginalCopy { .. } => Operation::ReadOriginalCopy,
            Request::CancelOriginalConnect { .. } => Operation::CancelOriginalConnect,
            Request::TerminateOriginalConnect { .. } => Operation::TerminateOriginalConnect,
            Request::RetireOriginalConnect { .. } => Operation::RetireOriginalConnect,
            Request::ResolveAccepted => Operation::MatchAccepted,
            Request::PrepareAccept { .. } => Operation::PrepareAccept,
            Request::CollectAccept { .. } => Operation::CollectAccept,
            Request::PrepareTableEnrollment { .. } => Operation::PrepareTableEnrollment,
            Request::CollectTableEnrollment { .. } => Operation::CollectTableEnrollment,
        };
        let accept = match key {
            Effect::Match(lease) | Effect::PrepareAccept(lease) | Effect::CollectAccept(lease) => {
                Some(lease)
            }
            _ => None,
        };
        let body = serde_json::to_vec(request)?;
        let mut state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(io::Error::other(error.clone()));
        }
        if let Effect::Listener(open_file) = key {
            if let Some(sequence) = state
                .requests
                .listener_sequence(open_file, operation, &body)?
            {
                return Ok(sequence);
            }
        }
        let State {
            session,
            requests,
            pending_send,
            rejected_rights,
            ..
        } = &mut *state;
        let pending_before = pending_send.len();
        let result = requests.prepare(key, owner, operation, &body, || {
            let rights = duplicate_rights()?;
            let envelope = Envelope {
                run: self.run,
                sequence: 0,
                owner: Some(owner),
                accept,
                operation,
                body: body.clone(),
            };
            let sequence = match session.prepare(envelope, rights) {
                Ok(sequence) => sequence,
                Err((error, rights)) => {
                    rejected_rights.push(rights);
                    return Err(error);
                }
            };
            pending_send.push_back(sequence);
            Ok(sequence)
        });
        let queued = pending_send.len() != pending_before;
        drop(state);
        if queued {
            self.wake_driver()?;
        }
        result
    }

    pub(super) fn retain_native_birth_cleanup(
        &self,
        request: super::native_birth::NativeBirthCleanupRequest,
        recovery: super::native_birth::NativeBirthRecovery,
    ) -> io::Result<Option<std::sync::Arc<super::native_birth::NativeBirthCleanup>>> {
        let permit = request.permit();
        let mut state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(io::Error::other(error.clone()));
        }
        let Some(entry) = state
            .requests
            .0
            .get_mut(&Effect::PrepareNativeBirth(permit.lease))
        else {
            return Ok(None);
        };
        if entry.owner != permit.owner
            || entry.operation != Operation::PrepareNativeBirth
            || !matches!(serde_json::from_slice::<Request>(&entry.body),Ok(Request::PrepareNativeBirth {call,mm,table,..})
                if call==permit.native_command_call() && mm==permit.owner.mm.generation() && table!=0)
            || entry.birth_semantic.is_some()
        {
            return Err(io::Error::other(
                "cleanup changed original unconsumed birth reservation",
            ));
        }
        if let Some(prior) = &entry.birth_cleanup {
            if prior.request != request {
                return Err(io::Error::other(
                    "cleanup changed retained marker or native errno",
                ));
            }
            return Ok(Some(prior.clone()));
        }
        let cleanup = std::sync::Arc::new(super::native_birth::NativeBirthCleanup::new(
            request, recovery,
        ));
        entry.birth_cleanup = Some(cleanup.clone());
        self.changed.notify_waiters();
        Ok(Some(cleanup))
    }
    pub(super) fn retain_native_birth_cleanups(&self) -> io::Result<()> {
        // Never acquire the scheduler while holding Controller::state. These
        // Arcs remain in their original requests until exact retirement ACK.
        let pending: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .requests
            .0
            .values()
            .filter_map(|entry| entry.birth_cleanup.clone())
            .collect();
        for cleanup in pending {
            cleanup.progress(self)?;
        }
        Ok(())
    }
    pub(super) fn notify_birth_cleanup(&self) {
        self.changed.notify_waiters();
    }
    pub(super) async fn wait_native_birth_cleanup(
        &self,
        cleanup: &super::native_birth::NativeBirthCleanup,
    ) -> io::Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let failure = self.state.lock().unwrap().failure.clone();
            if let Some(error) = failure {
                return Err(io::Error::other(error));
            }
            if let Some(result) = cleanup.result() {
                return result.map_err(io::Error::other);
            }
            changed.await;
        }
    }
    pub(super) fn poll_native_birth_cleanup(
        &self,
        cleanup: &super::native_birth::NativeBirthCleanupRequest,
    ) -> io::Result<bool> {
        use super::native_birth::NativeBirthCleanupRequest;
        let permit = cleanup.permit();
        // A lost Prepare response still belongs to this same request. Pending
        // is not absence and does not release either common semantic gate.
        let prepared = {
            let state = self.state.lock().unwrap();
            if let Some(error) = &state.failure {
                return Err(io::Error::other(error.clone()));
            }
            let entry = state
                .requests
                .0
                .get(&Effect::PrepareNativeBirth(permit.lease))
                .ok_or_else(|| io::Error::other("cleanup lost original preparation"))?;
            let sequence = entry
                .sequence
                .ok_or_else(|| io::Error::other("cleanup preparation submission unknown"))?;
            if state.session.response(sequence)?.is_none() {
                return Ok(false);
            }
            sequence
        };
        let (same, command, _, _) = self.native_birth_preparation(permit)?;
        if same != prepared {
            return Err(io::Error::other(
                "cleanup changed original preparation sequence",
            ));
        }
        let (key, request) = match cleanup {
            NativeBirthCleanupRequest::Uninvoked { .. } => (
                Effect::CancelNativeBirth(permit.lease),
                Request::CancelNativeBirth {
                    call: permit.native_command_call(),
                    command,
                    prepared_request: prepared,
                },
            ),
            NativeBirthCleanupRequest::Failed { .. } => (
                Effect::CollectNativeBirth(permit.lease),
                Request::CollectNativeBirth {
                    call: permit.native_command_call(),
                    command,
                    prepared_request: prepared,
                },
            ),
        };
        // Requests::prepare enforces one immutable Collect/Cancel/Terminate
        // owner even when an earlier submission or physical effect is unknown.
        let sequence = self.prepare(key, permit.owner, &request, || Ok(vec![]))?;
        let mut state = self.state.lock().unwrap();
        let Some(body) = state.session.response(sequence)? else {
            return Ok(false);
        };
        state.session.check_outgoing_native_birth(
            permit.owner,
            permit.native_command_call(),
            prepared,
            None,
            sequence,
        )?;
        let matches = match (cleanup, serde_json::from_slice::<Reply>(body)?) {
            (
                NativeBirthCleanupRequest::Uninvoked { .. },
                Reply::NativeBirthCanceled {
                    command: observed,
                    status,
                },
            ) => observed == command && status.returned == 0 && status.errno.is_none(),
            (NativeBirthCleanupRequest::Failed { errno, .. }, Reply::NativeBirthEffect(value)) => {
                value.status.returned == 0
                    && value.status.errno.is_none()
                    && value.raw.command.returned == -*errno
            }
            _ => false,
        };
        if !matches {
            return Err(io::Error::other(
                "cleanup physical receipt contradicts retained common evidence",
            ));
        }
        state
            .requests
            .consume_birth(permit, None, Some(cleanup.completion()))?;
        Ok(true)
    }

    fn birth_consumption(
        &self,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        semantic: Option<BirthSemantic>,
        completion: Option<BirthCompletion>,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.requests.consume_birth(permit, semantic, completion)?;
        self.changed.notify_waiters();
        Ok(())
    }
    pub(super) fn native_birth_semantics_consumed(
        &self,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        semantic: BirthSemantic,
    ) -> io::Result<()> {
        self.birth_consumption(permit, Some(semantic), None)
    }
    pub(super) fn native_birth_completion_consumed(
        &self,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        completion: BirthCompletion,
    ) -> io::Result<()> {
        self.birth_consumption(permit, None, Some(completion))
    }

    /// Called only by actual final-wait while runtime's original task custody
    /// lock excludes prepare insertion. It latches facts on existing requests;
    /// no numeric task lookup, synthetic Tool exit, or no-child inference.
    pub(super) fn native_birth_creator_terminal(
        &self,
        owner: NetworkStreamOwner,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        for (key, entry) in &mut state.requests.0 {
            if matches!(key, Effect::PrepareNativeBirth(_)) && entry.owner == owner {
                if entry.operation != Operation::PrepareNativeBirth {
                    return Err(io::Error::other(
                        "terminal creator changed original preparation",
                    ));
                }
                entry.birth_creator_terminal = true;
            }
        }
        self.changed.notify_waiters();
        Ok(())
    }

    fn reconcile_terminal_births(state: &mut State, run: [u8; 16]) -> io::Result<()> {
        let ready: Vec<_> = state
            .requests
            .0
            .iter()
            .filter_map(|(key, entry)| {
                let Effect::PrepareNativeBirth(lease) = key else {
                    return None;
                };
                (entry.birth_creator_terminal && entry.birth_completion.is_none()).then_some((
                    *lease,
                    entry.owner,
                    entry.sequence,
                    entry.birth_semantic,
                ))
            })
            .collect();
        for (lease, owner, prepared, semantic) in ready {
            let (Some(prepared), Some(semantic)) = (prepared, semantic) else {
                continue;
            };
            let Request::PrepareNativeBirth { call, .. } =
                serde_json::from_slice(&state.requests.0[&Effect::PrepareNativeBirth(lease)].body)?
            else {
                return Err(io::Error::other("terminal creator lost preparation shape"));
            };
            let Some(prepared_body) = state.session.response(prepared)? else {
                continue;
            };
            let Reply::Prepared(p) = serde_json::from_slice(prepared_body)? else {
                return Err(io::Error::other("terminal creator lost preparation reply"));
            };
            if p.status.returned != 0 || p.status.errno.is_some() || p.raw == 0 {
                return Err(io::Error::other(
                    "terminal birth preparation failed; custody retained",
                ));
            }
            let observed = match semantic {
                BirthSemantic::Child { child, terminal } => {
                    let r = state
                        .requests
                        .0
                        .get(&Effect::ObserveNativeBirth(lease))
                        .filter(|r| {
                            r.owner == owner && r.operation == Operation::ObserveNativeBirth
                        })
                        .ok_or_else(|| {
                            io::Error::other("terminal child lost observation custody")
                        })?;
                    if !matches!(serde_json::from_slice::<Request>(&r.body),Ok(Request::ObserveNativeBirth {
                        child:c,terminal:t,..}) if c==child && (!t || terminal))
                    {
                        return Err(io::Error::other("terminal creator changed semantic child"));
                    }
                    Some(
                        r.sequence.ok_or_else(|| {
                            io::Error::other("terminal child observation unknown")
                        })?,
                    )
                }
                BirthSemantic::Failed | BirthSemantic::Uninvoked => None,
            };
            let collect = state.requests.0.get(&Effect::CollectNativeBirth(lease));
            let cancel = state.requests.0.get(&Effect::CancelNativeBirth(lease));
            if collect.is_some() && cancel.is_some() {
                return Err(io::Error::other(
                    "terminal birth has contradictory finish owners",
                ));
            }
            if let Some(finish) = collect.or(cancel) {
                if finish.owner != owner {
                    return Err(io::Error::other("terminal finish owner changed"));
                }
                let sequence = finish
                    .sequence
                    .ok_or_else(|| io::Error::other("terminal finish submission unknown"))?;
                let Some(body) = state.session.response(sequence)? else {
                    continue;
                };
                // A response whose borrower disappeared is consumed by this
                // same run owner. Failed/unknown physical completion remains a
                // failed execution with custody, never an inferred disarm.
                state
                    .session
                    .check_outgoing_native_birth(owner, call, prepared, observed, sequence)?;
                let completion = match serde_json::from_slice::<Reply>(body)? {
                    Reply::NativeBirthEffect(value)
                        if value.status.returned == 0 && value.status.errno.is_none() =>
                    {
                        let native = value.raw.command.returned;
                        BirthCompletion::Returned(if native > 0 {
                            Ok(i64::from(native))
                        } else {
                            Err(-native)
                        })
                    }
                    Reply::NativeBirthCanceled { status, .. }
                        if status.returned == 0 && status.errno.is_none() =>
                    {
                        BirthCompletion::Uninvoked
                    }
                    _ => {
                        return Err(io::Error::other(
                            "terminal birth finish failed; exact request remains retained",
                        ));
                    }
                };
                state
                    .requests
                    .0
                    .get_mut(&Effect::PrepareNativeBirth(lease))
                    .unwrap()
                    .birth_completion = Some(completion);
                continue;
            }
            // Only a positively consumed actual child permits retirement of a
            // creator that never reached ordinary collection. Missing birth
            // evidence is not a no-child result and cannot reach this branch.
            if !matches!(semantic, BirthSemantic::Child { .. }) {
                continue;
            }
            let request = Request::TerminateNativeBirth {
                call,
                command: p.raw,
                prepared_request: prepared,
            };
            let body = serde_json::to_vec(&request)?;
            let State {
                requests,
                session,
                pending_send,
                ..
            } = state;
            let sequence = requests.prepare(
                Effect::TerminateNativeBirth(lease),
                owner,
                Operation::TerminateNativeBirth,
                &body,
                || {
                    let envelope = Envelope {
                        run,
                        sequence: 0,
                        owner: Some(owner),
                        accept: None,
                        operation: Operation::TerminateNativeBirth,
                        body: body.clone(),
                    };
                    let sequence = session
                        .prepare(envelope, vec![])
                        .map_err(|(error, _)| error)?;
                    pending_send.push_back(sequence);
                    Ok(sequence)
                },
            )?;
            let Some(_) = session.response(sequence)? else {
                continue;
            };
            session.check_outgoing_native_birth(owner, call, prepared, observed, sequence)?;
            requests
                .0
                .get_mut(&Effect::PrepareNativeBirth(lease))
                .unwrap()
                .birth_completion = Some(BirthCompletion::CreatorTerminal);
        }
        Ok(())
    }

    fn retire_consumed_births(state: &mut State, run: [u8; 16]) -> io::Result<()> {
        // Bounded by active command groups. Work is owned by Controller's
        // existing driver even after a GlobalRPC future has disappeared.
        let ready: Vec<_> = state
            .requests
            .0
            .iter()
            .filter_map(|(key, entry)| {
                let Effect::PrepareNativeBirth(lease) = key else {
                    return None;
                };
                Some((
                    *lease,
                    entry.owner,
                    entry.sequence?,
                    entry.birth_semantic?,
                    entry.birth_completion?,
                ))
            })
            .collect();
        for (lease, owner, prepared, semantic, completion) in ready {
            let Request::PrepareNativeBirth { call, .. } =
                serde_json::from_slice(&state.requests.0[&Effect::PrepareNativeBirth(lease)].body)?
            else {
                return Err(io::Error::other(
                    "birth retirement lost original request shape",
                ));
            };
            let (observed, finish) = match (semantic, completion) {
                (
                    BirthSemantic::Child { child, terminal },
                    BirthCompletion::Returned(Ok(returned)),
                ) if returned == i64::from(child) && child > 0 => {
                    let observation = state
                        .requests
                        .0
                        .get(&Effect::ObserveNativeBirth(lease))
                        .filter(|r| {
                            r.owner == owner && r.operation == Operation::ObserveNativeBirth
                        })
                        .ok_or_else(|| {
                            io::Error::other("consumed child lost retained observation")
                        })?;
                    if !matches!(serde_json::from_slice::<Request>(&observation.body),Ok(Request::ObserveNativeBirth {
                        child:c,terminal:t,..}) if c==child && (!t || terminal))
                    {
                        return Err(io::Error::other("semantic child changed actual admission"));
                    }
                    (
                        Some(observation.sequence.ok_or_else(|| {
                            io::Error::other("child observation remains unknown")
                        })?),
                        Effect::CollectNativeBirth(lease),
                    )
                }
                (BirthSemantic::Child { child, terminal }, BirthCompletion::CreatorTerminal) => {
                    let prepared_entry = &state.requests.0[&Effect::PrepareNativeBirth(lease)];
                    if !prepared_entry.birth_creator_terminal {
                        return Err(io::Error::other(
                            "creator terminal consumption lacks final wait",
                        ));
                    }
                    let observation = state
                        .requests
                        .0
                        .get(&Effect::ObserveNativeBirth(lease))
                        .filter(|r| {
                            r.owner == owner && r.operation == Operation::ObserveNativeBirth
                        })
                        .ok_or_else(|| {
                            io::Error::other("terminal child lost retained observation")
                        })?;
                    if !matches!(serde_json::from_slice::<Request>(&observation.body),Ok(Request::ObserveNativeBirth {
                        child:c,terminal:t,..}) if c==child && (!t || terminal))
                    {
                        return Err(io::Error::other(
                            "terminal semantic child changed admission",
                        ));
                    }
                    (
                        Some(observation.sequence.ok_or_else(|| {
                            io::Error::other("terminal child observation unknown")
                        })?),
                        Effect::TerminateNativeBirth(lease),
                    )
                }
                (BirthSemantic::Failed, BirthCompletion::Returned(Err(errno)))
                    if (1..=4095).contains(&errno) =>
                {
                    (None, Effect::CollectNativeBirth(lease))
                }
                (BirthSemantic::Uninvoked, BirthCompletion::Uninvoked) => {
                    (None, Effect::CancelNativeBirth(lease))
                }
                _ => {
                    return Err(io::Error::other(
                        "birth physical and semantic consumption disagree",
                    ));
                }
            };
            if observed.is_none()
                && state
                    .requests
                    .0
                    .contains_key(&Effect::ObserveNativeBirth(lease))
            {
                return Err(io::Error::other(
                    "no-child consumption has a retained child effect",
                ));
            }
            let completed = state
                .requests
                .0
                .get(&finish)
                .filter(|r| r.owner == owner)
                .and_then(|r| r.sequence)
                .ok_or_else(|| io::Error::other("birth completion remains unknown"))?;
            let request = Request::RetireNativeBirth {
                call,
                prepared,
                observed,
                completed,
            };
            let body = serde_json::to_vec(&request)?;
            let key = Effect::RetireNativeBirth(lease);
            let State {
                requests,
                session,
                pending_send,
                ..
            } = state;
            let retirement =
                requests.prepare(key, owner, Operation::RetireNativeBirth, &body, || {
                    let envelope = Envelope {
                        run,
                        sequence: 0,
                        owner: Some(owner),
                        accept: None,
                        operation: Operation::RetireNativeBirth,
                        body: body.clone(),
                    };
                    let sequence = session
                        .prepare(envelope, vec![])
                        .map_err(|(error, _)| error)?;
                    pending_send.push_back(sequence);
                    Ok(sequence)
                })?;
            if session.response(retirement)?.is_none() {
                continue;
            }
            let mut keys = vec![(Effect::PrepareNativeBirth(lease), prepared)];
            if let Some(sequence) = observed {
                keys.push((Effect::ObserveNativeBirth(lease), sequence));
            }
            keys.extend([(finish, completed), (key, retirement)]);
            if keys.iter().any(|(key, sequence)| {
                pending_send.contains(sequence)
                    || requests
                        .0
                        .get(key)
                        .is_none_or(|r| r.owner != owner || r.sequence != Some(*sequence))
            }) {
                return Err(io::Error::other("birth retirement changed submitted group"));
            }
            session.retire_outgoing_native_birth(
                owner, call, prepared, observed, completed, retirement,
            )?;
            for (key, _) in keys {
                requests.0.remove(&key);
            }
        }
        Ok(())
    }

    pub(super) fn has_native_birth_preparation(
        &self,
        permit: crate::network_replay::NetworkFdPublicationPermit,
    ) -> io::Result<bool> {
        let state = self.state.lock().unwrap();
        match state
            .requests
            .0
            .get(&Effect::PrepareNativeBirth(permit.lease))
        {
            None => Ok(false),
            Some(r) if r.owner == permit.owner => Ok(true),
            _ => Err(io::Error::other("birth preparation owner changed")),
        }
    }

    /// Recover the exact original preparation without consulting a live owner.
    /// This is the existing run-owned request/Inbox, not a child registry.
    pub(super) fn native_birth_preparation(
        &self,
        permit: crate::network_replay::NetworkFdPublicationPermit,
    ) -> io::Result<(u64, u64, u64, i32)> {
        let state = self.state.lock().unwrap();
        let submitted = state
            .requests
            .0
            .get(&Effect::PrepareNativeBirth(permit.lease))
            .filter(|r| r.owner == permit.owner && r.operation == Operation::PrepareNativeBirth)
            .ok_or_else(|| io::Error::other("birth lost retained creator preparation"))?;
        let request: Request = serde_json::from_slice(&submitted.body)?;
        let Request::PrepareNativeBirth {
            call,
            mm,
            table,
            syscall,
        } = request
        else {
            return Err(io::Error::other("birth preparation changed request kind"));
        };
        if call != permit.native_command_call()
            || mm != permit.owner.mm.generation()
            || table == 0
            || !matches!(syscall, 56 | 57 | 58 | 435)
        {
            return Err(io::Error::other(
                "birth preparation changed permit/MM/table",
            ));
        }
        let sequence = submitted
            .sequence
            .ok_or_else(|| io::Error::other("birth preparation remains unknown"))?;
        let bytes = state
            .session
            .response(sequence)?
            .ok_or_else(|| io::Error::other("birth preparation has no retained response"))?;
        let Reply::Prepared(response) = serde_json::from_slice(bytes)? else {
            return Err(io::Error::other("birth preparation changed reply kind"));
        };
        if response.status.returned != 0 || response.status.errno.is_some() || response.raw == 0 {
            return Err(io::Error::other("native birth was not armed"));
        }
        Ok((sequence, response.raw, table, syscall))
    }

    pub(super) fn retire_terminal_socket_observation(
        &self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        observed: u64,
        retired: u64,
    ) -> io::Result<()> {
        let keys = [
            Effect::ObserveTerminalSocket(call),
            Effect::RetireTerminalSocketObservation(call),
        ];
        let mut state = self.state.lock().unwrap();
        for (key, sequence) in keys.into_iter().zip([observed, retired]) {
            if state
                .requests
                .0
                .get(&key)
                .is_none_or(|r| r.owner != owner || r.sequence != Some(sequence))
                || state.pending_send.contains(&sequence)
            {
                return Err(io::Error::other(
                    "terminal Socket retirement changed submitted Call",
                ));
            }
        }
        state.session.retire_outgoing_terminal_socket(
            owner,
            call.native_command_call(),
            observed,
            retired,
        )?;
        for key in keys {
            state.requests.0.remove(&key);
        }
        Ok(())
    }

    pub(super) fn retire_file_observation(
        &self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        sequences: [u64; 3],
    ) -> io::Result<()> {
        let keys = [
            Effect::PrepareOriginalFileObservation(call),
            Effect::CollectOriginalFileObservation(call),
            Effect::RetireOriginalFileObservation(call),
        ];
        let mut state = self.state.lock().unwrap();
        for (key, sequence) in keys.into_iter().zip(sequences) {
            if state
                .requests
                .0
                .get(&key)
                .is_none_or(|r| r.owner != owner || r.sequence != Some(sequence))
                || state.pending_send.contains(&sequence)
            {
                return Err(io::Error::other(
                    "auxiliary retirement changed original submitted group",
                ));
            }
        }
        let mut copies = Vec::new();
        for (key, request) in &state.requests.0 {
            if matches!(key, Effect::ReadOriginalCopy(c, _) if *c == call) {
                let sequence = request
                    .sequence
                    .ok_or_else(|| io::Error::other("helper copy request outcome unknown"))?;
                if request.owner != owner || state.pending_send.contains(&sequence) {
                    return Err(io::Error::other(
                        "helper copy retirement changed its retained owner",
                    ));
                }
                copies.push(*key);
            }
        }
        state.session.retire_outgoing_file_observation(
            owner,
            call.native_command_call(),
            sequences[0],
            sequences[1],
            sequences[2],
        )?;
        for key in copies {
            state.requests.0.remove(&key);
        }
        for key in keys {
            state.requests.0.remove(&key);
        }
        Ok(())
    }
    pub(super) fn retire_original(
        &self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        sequences: [u64; 4],
        canceled: bool,
        terminated: bool,
        failed_request: Option<u64>,
    ) -> io::Result<()> {
        let keys = [
            Effect::PrepareOriginalConnect(call),
            Effect::AwaitOriginalSelection(call),
            if terminated {
                Effect::TerminateOriginalConnect(call)
            } else if canceled {
                Effect::CancelOriginalConnect(call)
            } else {
                Effect::CollectOriginalConnect(call)
            },
            Effect::RetireOriginalConnect(call),
        ];
        let mut state = self.state.lock().unwrap();
        let mut owned: Vec<_> = keys.into_iter().zip(sequences).collect();
        for (&key, request) in &state.requests.0 {
            if matches!(key,Effect::ReadOriginalCopy(c,_) if c==call) {
                owned.push((
                    key,
                    request.sequence.ok_or_else(|| {
                        io::Error::other("Read copy retirement before submission")
                    })?,
                ));
            }
        }
        if let Some(failed) = failed_request {
            if !terminated {
                return Err(io::Error::other(
                    "nonterminal original has a failed collection frame",
                ));
            }
            owned.push((Effect::CollectOriginalConnect(call), failed));
        }
        for &(key, sequence) in &owned {
            if state
                .requests
                .0
                .get(&key)
                .is_none_or(|r| r.owner != owner || r.sequence != Some(sequence))
                || state.pending_send.contains(&sequence)
            {
                return Err(io::Error::other(
                    "original transport retirement changed submitted group",
                ));
            }
        }
        state.session.retire_outgoing_original(
            owner,
            call.native_command_call(),
            sequences,
            failed_request,
        )?;
        for (key, _) in owned {
            state.requests.0.remove(&key);
        }
        Ok(())
    }

    /// Called only after exact engine publication acknowledged this read. The
    /// returned receipt is retained by the creation cursor until peer ACK.
    pub(super) fn retire_observation(
        &self,
        id: u64,
        sequence: u64,
        body: &[u8],
    ) -> io::Result<ObservationReceipt> {
        let mut state = self.state.lock().unwrap();
        let key = Effect::Observation(id);
        let entry = state
            .requests
            .0
            .get(&key)
            .ok_or_else(|| io::Error::other("missing observation request"))?;
        if id <= state.requests.1
            || entry.sequence != Some(sequence)
            || entry.operation != Operation::DrainCreations
        {
            return Err(io::Error::other("retirement changed retained observation"));
        }
        let receipt = state.session.retire_outgoing_observation(sequence, body)?;
        state.requests.retire_observation(key, sequence)?;
        Ok(receipt)
    }

    pub(super) fn retire_fd_observation(
        &self,
        id: u64,
        sequence: u64,
        body: &[u8],
    ) -> io::Result<ObservationReceipt> {
        let mut state = self.state.lock().unwrap();
        let key = Effect::FdObservation(id);
        let entry = state
            .requests
            .0
            .get(&key)
            .ok_or_else(|| io::Error::other("missing FD observation request"))?;
        if id <= state.requests.2
            || entry.sequence != Some(sequence)
            || entry.operation != Operation::DrainFdJournal
        {
            return Err(io::Error::other(
                "FD retirement changed retained observation",
            ));
        }
        let receipt = state.session.retire_outgoing_observation(sequence, body)?;
        state.requests.retire_observation(key, sequence)?;
        Ok(receipt)
    }

    /// Transport waits do not publish guest time or select a guest wakeup.
    /// The RPC caller must separately revalidate owner/MM and semantic leases
    /// before applying a provider result to the engine.
    pub(super) async fn response(&self, sequence: u64) -> io::Result<Reply> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(reply) = self.retained_response(sequence)? {
                return Ok(reply);
            }
            // Only the native run-owned driver advances transport. This wait
            // needs no Tokio reactor and cancellation transfers no FD owner.
            changed.await;
        }
    }

    fn progress_io(
        state: &mut State,
        changed: &tokio::sync::Notify,
        run: [u8; 16],
    ) -> io::Result<()> {
        Self::reconcile_terminal_births(state, run)?;
        Self::retire_consumed_births(state, run)?;
        while let Some(next) = state.pending_send.front().copied() {
            if !state.session.try_send(next)? {
                break;
            }
            state.pending_send.pop_front();
        }
        // One received ACK per pass keeps controller work finite under a busy
        // peer. Readiness is rechecked without inventing a scheduling quantum.
        match state.session.try_receive()? {
            Some(Received::Acknowledged(_)) => changed.notify_waiters(),
            None => {}
            Some(Received::Request(_)) => {
                return Err(io::Error::other(
                    "provider sent an unexpected controller request",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newly_queued_request_wakes_the_native_driver_once() {
        let mut sockets = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );
        let endpoint = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
        let _peer = unsafe { OwnedFd::from_raw_fd(sockets[1]) };
        let controller = Controller::new(endpoint, [6; 16]).unwrap();
        let mut wake = libc::pollfd {
            fd: controller.driver_wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut wake, 1, 0) }, 0);

        let sequence = controller
            .prepare(
                Effect::Observation(1),
                owner(),
                &Request::ReadStatus,
                || Ok(vec![]),
            )
            .unwrap();
        assert_eq!(unsafe { libc::poll(&mut wake, 1, 0) }, 1);
        controller.drive_once().unwrap();
        wake.revents = 0;
        assert_eq!(unsafe { libc::poll(&mut wake, 1, 0) }, 0);

        assert_eq!(
            controller
                .prepare(
                    Effect::Observation(1),
                    owner(),
                    &Request::ReadStatus,
                    || panic!("duplicate request transferred rights"),
                )
                .unwrap(),
            sequence
        );
        assert_eq!(unsafe { libc::poll(&mut wake, 1, 0) }, 0);
    }

    #[test]
    fn native_birth_finish_keeps_one_physical_owner_after_known_or_unknown_submission() {
        let owner = owner();
        let lease = serde_json::from_str("17").unwrap();
        let finishes = [
            (
                Effect::CollectNativeBirth(lease),
                Operation::CollectNativeBirth,
                Request::CollectNativeBirth {
                    call: 17,
                    command: 7,
                    prepared_request: 1,
                },
            ),
            (
                Effect::CancelNativeBirth(lease),
                Operation::CancelNativeBirth,
                Request::CancelNativeBirth {
                    call: 17,
                    command: 7,
                    prepared_request: 1,
                },
            ),
            (
                Effect::TerminateNativeBirth(lease),
                Operation::TerminateNativeBirth,
                Request::TerminateNativeBirth {
                    call: 17,
                    command: 7,
                    prepared_request: 1,
                },
            ),
        ];
        for unknown in [false, true] {
            for (chosen, operation, request) in &finishes {
                let mut requests = Requests::default();
                let mut submissions = 0;
                let body = serde_json::to_vec(request).unwrap();
                let first = requests.prepare(*chosen, owner, *operation, &body, || {
                    submissions += 1;
                    if unknown {
                        Err(io::Error::other("physical submission outcome unknown"))
                    } else {
                        Ok(19)
                    }
                });
                assert_eq!(first.is_err(), unknown);
                for (candidate, operation, request) in &finishes {
                    let body = serde_json::to_vec(request).unwrap();
                    let recovered = requests.prepare(*candidate, owner, *operation, &body, || {
                        panic!("retained birth completion was submitted twice")
                    });
                    if candidate == chosen && !unknown {
                        assert_eq!(recovered.unwrap(), 19);
                    } else {
                        assert!(recovered.is_err());
                    }
                }
                assert_eq!(submissions, 1);
                assert_eq!(requests.0.len(), 1);
                let retained = &requests.0[chosen];
                assert_eq!(retained.sequence, if unknown { None } else { Some(19) });
                assert_eq!(retained.error.is_some(), unknown);
                // An unrelated command is not blocked by the retained lease.
                let other = serde_json::from_str("18").unwrap();
                let body = serde_json::to_vec(&Request::CollectNativeBirth {
                    call: 18,
                    command: 8,
                    prepared_request: 2,
                })
                .unwrap();
                assert_eq!(
                    requests
                        .prepare(
                            Effect::CollectNativeBirth(other),
                            owner,
                            Operation::CollectNativeBirth,
                            &body,
                            || Ok(20)
                        )
                        .unwrap(),
                    20
                );
            }
        }
    }

    #[test]
    fn native_birth_request_survives_waiter_loss_and_rejects_changed_child_or_terminal() {
        let mut requests = Requests::default();
        let owner = owner();
        let lease = serde_json::from_str("17").unwrap();
        let key = Effect::ObserveNativeBirth(lease);
        let request = Request::ObserveNativeBirth {
            call: 17,
            command: 7,
            prepared_request: 1,
            child: 42,
            terminal: false,
        };
        let body = serde_json::to_vec(&request).unwrap();
        let mut submits = 0;
        assert_eq!(
            requests
                .prepare(key, owner, Operation::ObserveNativeBirth, &body, || {
                    submits += 1;
                    Ok(2)
                })
                .unwrap(),
            2
        );
        assert_eq!(
            requests
                .prepare(key, owner, Operation::ObserveNativeBirth, &body, || panic!(
                    "lost waiter resubmitted native effect"
                ))
                .unwrap(),
            2
        );
        for (child, terminal) in [(43, false), (42, true)] {
            let changed = serde_json::to_vec(&Request::ObserveNativeBirth {
                call: 17,
                command: 7,
                prepared_request: 1,
                child,
                terminal,
            })
            .unwrap();
            assert!(
                requests
                    .prepare(
                        key,
                        owner,
                        Operation::ObserveNativeBirth,
                        &changed,
                        || panic!("changed child submitted")
                    )
                    .is_err()
            );
        }
        assert_eq!(submits, 1);
    }

    #[test]
    fn native_birth_consumption_retains_original_entry_and_rejects_changed_owner_or_outcome() {
        let owner = owner();
        let lease = serde_json::from_str("17").unwrap();
        let permit = crate::network_replay::NetworkFdPublicationPermit {
            owner,
            files: crate::types::FilesId::initial(owner.thread),
            lease,
        };
        let mut requests = Requests::default();
        let key = Effect::PrepareNativeBirth(lease);
        let body = serde_json::to_vec(&Request::PrepareNativeBirth {
            call: permit.native_command_call(),
            mm: owner.mm.generation(),
            table: 47,
            syscall: 435,
        })
        .unwrap();
        requests
            .prepare(key, owner, Operation::PrepareNativeBirth, &body, || Ok(1))
            .unwrap();
        requests
            .consume_birth(
                permit,
                Some(BirthSemantic::Child {
                    child: 42,
                    terminal: false,
                }),
                None,
            )
            .unwrap();
        assert!(requests.0[&key].birth_completion.is_none());
        requests
            .consume_birth(permit, None, Some(BirthCompletion::Returned(Ok(42))))
            .unwrap();
        assert_eq!(requests.0.len(), 1);
        assert_eq!(requests.0[&key].sequence, Some(1));
        assert!(
            requests
                .consume_birth(
                    permit,
                    Some(BirthSemantic::Child {
                        child: 43,
                        terminal: false
                    }),
                    None
                )
                .is_err()
        );
        assert!(
            requests
                .consume_birth(
                    permit,
                    Some(BirthSemantic::Child {
                        child: 42,
                        terminal: true
                    }),
                    None
                )
                .is_err()
        );
        assert!(
            requests
                .consume_birth(permit, None, Some(BirthCompletion::Uninvoked))
                .is_err()
        );
        let mut wrong = permit;
        wrong.owner.mm = wrong.owner.mm.for_exec(owner.thread);
        assert!(
            requests
                .consume_birth(wrong, None, Some(BirthCompletion::Returned(Ok(42))))
                .is_err()
        );
        assert_eq!(
            requests.0[&key].birth_completion,
            Some(BirthCompletion::Returned(Ok(42)))
        );
    }

    #[test]
    fn native_birth_driver_retires_consumed_group_without_rpc_waiter() {
        use std::os::fd::FromRawFd;
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let left = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let right = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        let controller = Controller::new(left, [7; 16]).unwrap();
        let mut provider = AcceptedSession::new(right, [7; 16]).unwrap();
        let pin = std::fs::File::open("/dev/null").unwrap();
        let (owner, observed, completed, rows) =
            super::super::accepted_transport::native_birth_test_group(1, 9, 0);
        let lease = serde_json::from_str("9").unwrap();
        let permit = crate::network_replay::NetworkFdPublicationPermit {
            owner,
            lease,
            files: crate::types::FilesId::initial(owner.thread),
        };
        let keys = [
            Effect::PrepareNativeBirth(lease),
            Effect::ObserveNativeBirth(lease),
            Effect::CollectNativeBirth(lease),
        ];
        for ((envelope, body, rights), key) in rows.into_iter().zip(keys) {
            let request: Request = serde_json::from_slice(&envelope.body).unwrap();
            let sequence = controller
                .prepare(key, owner, &request, || {
                    (0..rights)
                        .map(|_| pin.as_fd().try_clone_to_owned())
                        .collect()
                })
                .unwrap();
            assert_eq!(sequence, envelope.sequence);
            Controller::progress_io(
                &mut controller.state.lock().unwrap(),
                &controller.changed,
                [7; 16],
            )
            .unwrap();
            assert!(
                matches!(provider.try_receive().unwrap(),Some(Received::Request(s)) if s==sequence)
            );
            provider
                .dispatch(sequence, |_, _| Ok(body.clone()))
                .unwrap();
            if sequence == completed {
                provider.acknowledge_command_completion(sequence,|_,_|Ok(serde_json::to_vec(
                    &serde_json::json!({"Observed":{"operation":"ap_ack_command","returned":0,"errno":null}})).unwrap())).unwrap();
            }
            assert!(provider.try_reply(sequence).unwrap());
            Controller::progress_io(
                &mut controller.state.lock().unwrap(),
                &controller.changed,
                [7; 16],
            )
            .unwrap();
            assert!(controller.retained_response(sequence).unwrap().is_some());
        }
        assert!(!controller.quiescent().unwrap());
        controller
            .native_birth_semantics_consumed(
                permit,
                BirthSemantic::Child {
                    child: 62,
                    terminal: false,
                },
            )
            .unwrap();
        Controller::progress_io(
            &mut controller.state.lock().unwrap(),
            &controller.changed,
            [7; 16],
        )
        .unwrap();
        assert!(provider.try_receive().unwrap().is_none()); // completion not consumed yet
        assert_eq!(
            controller
                .state
                .lock()
                .unwrap()
                .session
                .terminal_custody()
                .retained_rights,
            2
        );
        controller
            .native_birth_completion_consumed(permit, BirthCompletion::Returned(Ok(62)))
            .unwrap();
        // No RPC future exists. The same production driver sends and consumes the ACK.
        Controller::progress_io(
            &mut controller.state.lock().unwrap(),
            &controller.changed,
            [7; 16],
        )
        .unwrap();
        let Some(Received::Request(retirement)) = provider.try_receive().unwrap() else {
            panic!("retirement absent");
        };
        assert_eq!(retirement, completed + 1);
        provider
            .retire_incoming_native_birth(owner, 9, 1, observed, completed)
            .unwrap();
        provider
            .dispatch(retirement, |_, _| {
                serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
            })
            .unwrap();
        assert!(!controller.quiescent().unwrap());
        assert_eq!(
            controller
                .state
                .lock()
                .unwrap()
                .session
                .terminal_custody()
                .retained_rights,
            2
        );
        assert!(provider.try_reply(retirement).unwrap());
        provider.retire_sent_original_ack(retirement).unwrap();
        Controller::progress_io(
            &mut controller.state.lock().unwrap(),
            &controller.changed,
            [7; 16],
        )
        .unwrap();
        Controller::progress_io(
            &mut controller.state.lock().unwrap(),
            &controller.changed,
            [7; 16],
        )
        .unwrap();
        assert!(controller.quiescent().unwrap());
        let state = controller.state.lock().unwrap();
        assert!(state.requests.0.is_empty());
        assert_eq!(state.session.terminal_custody().outgoing, 0);
        assert_eq!(state.session.terminal_custody().retained_rights, 0);
        assert_eq!(provider.terminal_custody().incoming, 0);
        assert_eq!(provider.terminal_custody().retained_rights, 0);
    }

    fn terminal_driver_case(kind: u8) {
        use std::os::fd::FromRawFd;
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let controller =
            Controller::new(unsafe { OwnedFd::from_raw_fd(pair[0]) }, [7; 16]).unwrap();
        let mut provider =
            AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [7; 16]).unwrap();
        let pin = std::fs::File::open("/dev/null").unwrap();
        let (owner, observed, completed, rows) = if kind == 3 {
            super::super::accepted_transport::native_birth_terminal_test_group(1, 9)
        } else {
            super::super::accepted_transport::native_birth_test_group(1, 9, kind)
        };
        let lease = serde_json::from_str("9").unwrap();
        let permit = crate::network_replay::NetworkFdPublicationPermit {
            owner,
            lease,
            files: crate::types::FilesId::initial(owner.thread),
        };
        let finish = if kind == 2 {
            Effect::CancelNativeBirth(lease)
        } else {
            Effect::CollectNativeBirth(lease)
        };
        let mut keys = vec![Effect::PrepareNativeBirth(lease)];
        if observed.is_some() {
            keys.push(Effect::ObserveNativeBirth(lease));
        }
        if kind != 3 {
            keys.push(finish);
        }
        for ((envelope, body, rights), key) in rows.iter().zip(keys) {
            let request: Request = serde_json::from_slice(&envelope.body).unwrap();
            let sequence = controller
                .prepare(key, owner, &request, || {
                    (0..*rights)
                        .map(|_| pin.as_fd().try_clone_to_owned())
                        .collect()
                })
                .unwrap();
            assert_eq!(sequence, envelope.sequence);
            Controller::progress_io(
                &mut controller.state.lock().unwrap(),
                &controller.changed,
                [7; 16],
            )
            .unwrap();
            assert!(
                matches!(provider.try_receive().unwrap(),Some(Received::Request(s)) if s==sequence)
            );
            provider
                .dispatch(sequence, |_, _| Ok(body.clone()))
                .unwrap();
            if sequence == completed && kind != 2 {
                provider.acknowledge_command_completion(sequence,|_,_|Ok(serde_json::to_vec(
                    &serde_json::json!({"Observed":{"operation":"ap_ack_command","returned":0,"errno":null}})).unwrap())).unwrap();
            }
            assert!(provider.try_reply(sequence).unwrap());
            Controller::progress_io(
                &mut controller.state.lock().unwrap(),
                &controller.changed,
                [7; 16],
            )
            .unwrap();
        }
        let semantic = match kind {
            0 | 3 => BirthSemantic::Child {
                child: 62,
                terminal: false,
            },
            1 => BirthSemantic::Failed,
            2 => BirthSemantic::Uninvoked,
            _ => unreachable!(),
        };
        controller
            .native_birth_semantics_consumed(permit, semantic)
            .unwrap();
        let wrong = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        controller.native_birth_creator_terminal(wrong).unwrap();
        Controller::progress_io(
            &mut controller.state.lock().unwrap(),
            &controller.changed,
            [7; 16],
        )
        .unwrap();
        assert!(provider.try_receive().unwrap().is_none());
        assert!(
            controller.state.lock().unwrap().requests.0[&Effect::PrepareNativeBirth(lease)]
                .birth_completion
                .is_none()
        );
        controller.native_birth_creator_terminal(owner).unwrap();
        Controller::progress_io(
            &mut controller.state.lock().unwrap(),
            &controller.changed,
            [7; 16],
        )
        .unwrap();
        if kind == 3 {
            let Some(Received::Request(sequence)) = provider.try_receive().unwrap() else {
                panic!("terminal request missing")
            };
            assert_eq!(sequence, completed);
            let (actual, _, _) = provider.retained_request(sequence).unwrap();
            assert_eq!(actual, &rows[2].0);
            // A delayed original RPC cannot submit collection/cancellation
            // after the run owner already owns terminal physical retirement.
            for (key, request) in [
                (
                    Effect::CollectNativeBirth(lease),
                    Request::CollectNativeBirth {
                        call: 9,
                        command: 26,
                        prepared_request: 1,
                    },
                ),
                (
                    Effect::CancelNativeBirth(lease),
                    Request::CancelNativeBirth {
                        call: 9,
                        command: 26,
                        prepared_request: 1,
                    },
                ),
            ] {
                assert!(
                    controller
                        .prepare(key, owner, &request, || panic!("late completion submitted"))
                        .is_err()
                );
            }
            provider
                .dispatch(sequence, |_, _| Ok(rows[2].1.clone()))
                .unwrap();
            assert!(provider.try_reply(sequence).unwrap());
            Controller::progress_io(
                &mut controller.state.lock().unwrap(),
                &controller.changed,
                [7; 16],
            )
            .unwrap();
            Controller::progress_io(
                &mut controller.state.lock().unwrap(),
                &controller.changed,
                [7; 16],
            )
            .unwrap();
        }
        let Some(Received::Request(retirement)) = provider.try_receive().unwrap() else {
            panic!("retirement absent")
        };
        assert_eq!(retirement, completed + 1);
        assert!(!controller.quiescent().unwrap());
        assert_eq!(
            controller
                .state
                .lock()
                .unwrap()
                .session
                .terminal_custody()
                .retained_rights,
            1 + usize::from(observed.is_some())
        );
        provider
            .retire_incoming_native_birth(owner, 9, 1, observed, completed)
            .unwrap();
        provider
            .dispatch(retirement, |_, _| {
                serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
            })
            .unwrap();
        assert!(provider.try_reply(retirement).unwrap());
        provider.retire_sent_original_ack(retirement).unwrap();
        Controller::progress_io(
            &mut controller.state.lock().unwrap(),
            &controller.changed,
            [7; 16],
        )
        .unwrap();
        Controller::progress_io(
            &mut controller.state.lock().unwrap(),
            &controller.changed,
            [7; 16],
        )
        .unwrap();
        assert!(controller.quiescent().unwrap());
        assert!(controller.state.lock().unwrap().requests.0.is_empty());
        assert_eq!(
            controller
                .state
                .lock()
                .unwrap()
                .session
                .terminal_custody()
                .retained_rights,
            0
        );
        assert_eq!(provider.terminal_custody().retained_rights, 0);
    }
    #[test]
    fn accepted_controller_waiter_fails_closed_when_provider_ends_its_send_direction() {
        use std::os::fd::FromRawFd;
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let controller =
            Controller::new(unsafe { OwnedFd::from_raw_fd(pair[0]) }, [7; 16]).unwrap();
        let mut provider =
            AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [7; 16]).unwrap();
        let pin = std::fs::File::open("/dev/null").unwrap();
        let (owner, _, _, rows) =
            super::super::accepted_transport::native_birth_test_group(1, 9, 0);
        let (envelope, _, rights) = &rows[0];
        let request: Request = serde_json::from_slice(&envelope.body).unwrap();
        let lease = serde_json::from_str("9").unwrap();
        let sequence = controller
            .prepare(Effect::PrepareNativeBirth(lease), owner, &request, || {
                (0..*rights)
                    .map(|_| pin.as_fd().try_clone_to_owned())
                    .collect()
            })
            .unwrap();
        controller.drive_once().unwrap();
        assert!(
            matches!(provider.try_receive().unwrap(),Some(Received::Request(s)) if s==sequence)
        );
        // A provider in retained failure never replies; without a terminal
        // signal the waiter could only be ended by an outer kill.
        controller.drive_once().unwrap();
        assert!(controller.retained_response(sequence).unwrap().is_none());
        provider.end_send_direction().unwrap();
        let error = controller.drive_once().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
        let waited = futures::executor::block_on(controller.response(sequence)).unwrap_err();
        assert!(waited.to_string().contains("end-of-stream"), "{waited}");
        // The provider keeps every received right: custody is not released.
        assert_eq!(provider.terminal_custody().retained_rights, *rights);
    }
    #[test]
    fn native_birth_final_wait_recovers_completed_collect_without_waiter() {
        terminal_driver_case(0);
    }
    #[test]
    fn native_birth_final_wait_recovers_completed_failure_without_waiter() {
        terminal_driver_case(1);
    }
    #[test]
    fn native_birth_final_wait_recovers_completed_cancel_without_waiter() {
        terminal_driver_case(2);
    }
    #[test]
    fn native_birth_final_wait_retires_admitted_child_without_creator_return() {
        terminal_driver_case(3);
    }

    fn owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(31);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    #[test]
    fn accepted_controller_cancelled_waiter_reuses_one_request_and_rights_transfer() {
        let mut requests = Requests::default();
        let key = Effect::Match(NetworkAcceptLeaseId(7));
        let mut transfers = 0;
        assert_eq!(
            requests
                .prepare(key, owner(), Operation::MatchAccepted, b"body", || {
                    transfers += 1;
                    Ok(12)
                })
                .unwrap(),
            12
        );
        // A new caller after waiter cancellation sees the same transport ID.
        assert_eq!(
            requests
                .prepare(key, owner(), Operation::MatchAccepted, b"body", || panic!(
                    "duplicated capabilities after cancellation"
                ))
                .unwrap(),
            12
        );
        assert_eq!(transfers, 1);
    }
    #[test]
    fn accepted_controller_changed_request_cannot_replace_retained_effect() {
        let mut requests = Requests::default();
        let key = Effect::Match(NetworkAcceptLeaseId(7));
        requests
            .prepare(key, owner(), Operation::MatchAccepted, b"body", || Ok(12))
            .unwrap();
        let changed = NetworkStreamOwner {
            mm: owner().mm.for_exec(owner().thread),
            ..owner()
        };
        assert!(
            requests
                .prepare(key, changed, Operation::MatchAccepted, b"body", || panic!(
                    "changed owner submitted"
                ))
                .is_err()
        );
        assert!(
            requests
                .prepare(
                    key,
                    owner(),
                    Operation::MatchAccepted,
                    b"different",
                    || panic!("changed body submitted")
                )
                .is_err()
        );
        assert!(
            requests
                .prepare(key, owner(), Operation::EnrollListener, b"body", || panic!(
                    "changed operation submitted"
                ))
                .is_err()
        );
        assert_eq!(requests.0.get(&key).unwrap().sequence, Some(12));
    }
    #[test]
    fn accepted_controller_preparation_failure_is_retained_before_caller_returns() {
        let mut requests = Requests::default();
        let key = Effect::Match(NetworkAcceptLeaseId(7));
        assert!(
            requests
                .prepare(key, owner(), Operation::MatchAccepted, b"body", || Err(
                    io::Error::other("rights retained after prepare failure")
                ))
                .is_err()
        );
        assert!(
            requests
                .prepare(key, owner(), Operation::MatchAccepted, b"body", || panic!(
                    "unknown operation resubmitted"
                ))
                .is_err()
        );
        assert!(requests.0.get(&key).unwrap().sequence.is_none());
    }
    #[test]
    fn accepted_controller_listener_alias_recovers_original_enrollment_without_task_reuse() {
        let mut requests = Requests::default();
        let ofd = OpenFileId::new_socket(owner().thread, 9);
        let key = Effect::Listener(ofd);
        requests
            .prepare(
                key,
                owner(),
                Operation::EnrollListener,
                b"generation4",
                || Ok(21),
            )
            .unwrap();
        // Current caller admission belongs to the engine/task authority. This
        // recovery does not borrow the historical owner's pidfd or resubmit.
        assert_eq!(
            requests
                .listener_sequence(ofd, Operation::EnrollListener, b"generation4")
                .unwrap(),
            Some(21)
        );
        assert_eq!(requests.0.get(&key).unwrap().owner, owner());
        assert!(
            requests
                .listener_sequence(ofd, Operation::EnrollListener, b"generation5")
                .is_err()
        );
        assert!(
            requests
                .listener_sequence(ofd, Operation::MatchAccepted, b"generation4")
                .is_err()
        );
        let failed_ofd = OpenFileId::new_socket(owner().thread, 10);
        assert!(
            requests
                .prepare(
                    Effect::Listener(failed_ofd),
                    owner(),
                    Operation::EnrollListener,
                    b"generation4",
                    || Err(io::Error::other("unknown transfer"))
                )
                .is_err()
        );
        assert!(
            requests
                .listener_sequence(failed_ofd, Operation::EnrollListener, b"generation4")
                .is_err()
        );
    }
    #[test]
    fn accepted_observer_retired_effect_cannot_be_reissued_after_cancel_or_owner_change() {
        let mut requests = Requests::default();
        let key = Effect::Observation(1);
        let first = requests
            .prepare(key, owner(), Operation::DrainCreations, b"read", || Ok(7))
            .unwrap();
        assert_eq!(
            requests
                .prepare(key, owner(), Operation::DrainCreations, b"read", || panic!(
                    "resubmit"
                ))
                .unwrap(),
            first
        );
        assert!(requests.retire_observation(key, 8).is_err());
        requests.retire_observation(key, 7).unwrap();
        assert!(
            requests
                .prepare(key, owner(), Operation::DrainCreations, b"read", || panic!(
                    "retired resubmit"
                ))
                .is_err()
        );
        assert!(requests.0.is_empty());
        assert_eq!(
            requests
                .prepare(
                    Effect::Observation(2),
                    owner(),
                    Operation::DrainCreations,
                    b"new",
                    || Ok(8)
                )
                .unwrap(),
            8
        );
        assert!(
            requests
                .retire_observation(Effect::Match(NetworkAcceptLeaseId(1)), 8)
                .is_err()
        );
        assert_eq!(requests.0.len(), 1);
    }
}
