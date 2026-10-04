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
    PrepareCurrentCloseProfile(NetworkStreamLeaseId),
    CollectCurrentCloseProfile(NetworkStreamLeaseId),
    RetireCurrentCloseProfile(NetworkStreamLeaseId),
    PrepareExecutableSource(crate::network_replay::NetworkStreamCallId),
    CollectExecutableSource(crate::network_replay::NetworkStreamCallId),
    RetireExecutableSource(crate::network_replay::NetworkStreamCallId),
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
    current_close_issued: bool,
    birth_semantic: Option<BirthSemantic>,
    birth_completion: Option<BirthCompletion>,
    birth_creator_terminal: bool,
    birth_cleanup: Option<std::sync::Arc<super::native_birth::NativeBirthCleanup>>,
}
#[cfg(test)]
#[derive(Clone, PartialEq, Eq)]
struct FixtureRequest {
    key: Effect,
    owner: NetworkStreamOwner,
    operation: Operation,
    body: Vec<u8>,
    sequence: Option<u64>,
    error: Option<String>,
}
#[cfg(test)]
pub(super) struct FixtureHistory(Vec<FixtureRequest>);

#[derive(Debug, Default)]
struct Requests(
    BTreeMap<Effect, Submitted>,
    u64,
    u64,
    // Publication leases increase for the run. Advance only after the exact
    // profile ACK group retires, so removing its frames cannot rearm that read.
    Option<NetworkStreamLeaseId>,
);
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
        if matches!(key, Effect::PrepareCurrentCloseProfile(lease)
            if self.3.is_some_and(|retired| lease <= retired))
        {
            return Err(io::Error::other(
                "retired current Close observation cannot be submitted again",
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
                current_close_issued: false,
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
    executable_changed: std::sync::Condvar,
}
impl Controller {
    #[cfg(test)]
    pub(super) fn drive_once(&self) -> io::Result<()> {
        self.drive_once_retained(|| Ok(()))
    }

    /// Retain completed effects before waiting for the next transport edge.
    /// Collection consumers may depend on that publication before they can
    /// submit another request. Waiting first adds an idle-poll timeout to each
    /// completed phase even when its exact response is already owned.
    pub(super) fn drive_once_retained(
        &self,
        retain: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        self.drive_once_retained_with_poll(retain, |descriptors| unsafe {
            libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 50)
        })
    }

    fn drive_once_retained_with_poll(
        &self,
        retain: impl FnOnce() -> io::Result<()>,
        poll: impl FnOnce(&mut [libc::pollfd]) -> i32,
    ) -> io::Result<()> {
        let progress = (|| {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.failure {
                return Err(io::Error::other(error.clone()));
            }
            if let Err(error) = Self::progress_io(&mut state, &self.changed, self.run) {
                state.failure = Some(error.to_string());
                self.changed.notify_waiters();
                return Err(error);
            }
            Ok(!state.pending_send.is_empty())
        })();
        self.executable_changed.notify_all();
        // Preserve known replies and failures even if transport progress failed.
        // The state mutex has been released; publication may acquire it again.
        retain()?;
        let writable = progress?;
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
        let result = poll(&mut descriptors);
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
        self.executable_changed.notify_all();
    }

    pub(super) fn quiescent(&self) -> io::Result<bool> {
        let state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(io::Error::other(error.clone()));
        }
        if !state.pending_send.is_empty() {
            return Ok(false);
        }
        if state.requests.0.keys().any(|key| {
            matches!(
                key,
                Effect::PrepareNativeBirth(_)
                    | Effect::PrepareExecutableSource(_)
                    | Effect::PrepareCurrentCloseProfile(_)
            )
        }) {
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

    /// Wait for exact transport retirement without publishing a guest event.
    pub(super) async fn wait_quiescent(&self) -> io::Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.quiescent()? {
                return Ok(());
            }
            changed.await;
        }
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
            executable_changed: std::sync::Condvar::new(),
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
            Request::PrepareCurrentCloseProfile { .. } => Operation::PrepareCurrentCloseProfile,
            Request::CollectCurrentCloseProfile { .. } => Operation::CollectCurrentCloseProfile,
            Request::RetireCurrentCloseProfile { .. } => Operation::RetireCurrentCloseProfile,
            Request::PrepareExecutableSource { .. } => Operation::PrepareExecutableSource,
            Request::CollectExecutableSource { .. } => Operation::CollectExecutableSource,
            Request::RetireExecutableSource { .. } => Operation::RetireExecutableSource,
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
        if let Effect::Listener(open_file) = key
            && let Some(sequence) = state
                .requests
                .listener_sequence(open_file, operation, &body)?
            {
                return Ok(sequence);
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

    fn retire_consumed_births(
        state: &mut State,
        changed: &tokio::sync::Notify,
        run: [u8; 16],
    ) -> io::Result<()> {
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
            // Receiving the ACK wakes one pass earlier. Validated removal is
            // the separate transition which can make strict quiescence true.
            changed.notify_waiters();
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

    /// One op26 housekeeping budget covers ARM, collection and ACK. Expiry
    /// poisons this original controller even when a late raw reply is retained.
    pub(super) fn executable_response_blocking(
        &self,
        sequence: u64,
        deadline: std::time::Instant,
    ) -> io::Result<Reply> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(error) = &state.failure {
                return Err(io::Error::other(error.clone()));
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                let message = "executable source exceeded original one-second housekeeping budget";
                state.failure.get_or_insert_with(|| message.into());
                drop(state);
                self.changed.notify_waiters();
                self.executable_changed.notify_all();
                return Err(io::Error::new(io::ErrorKind::TimedOut, message));
            }
            if let Some(bytes) = state.session.response(sequence)? {
                return serde_json::from_slice(bytes).map_err(io::Error::other);
            }
            state = self
                .executable_changed
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
    }

    #[cfg(test)]
    pub(super) fn current_close_fixture_history(&self) -> FixtureHistory {
        let state = self.state.lock().unwrap();
        FixtureHistory(
            state
                .requests
                .0
                .iter()
                .map(|(key, request)| FixtureRequest {
                    key: *key,
                    owner: request.owner,
                    operation: request.operation,
                    body: request.body.clone(),
                    sequence: request.sequence,
                    error: request.error.clone(),
                })
                .collect(),
        )
    }
    #[cfg(test)]
    pub(super) fn current_close_fixture_requests(&self) -> Vec<Request> {
        self.state
            .lock()
            .unwrap()
            .requests
            .0
            .values()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }
    #[cfg(test)]
    pub(super) fn current_close_fixture_matches_history(&self, before: &FixtureHistory) -> bool {
        self.current_close_fixture_history().0 == before.0
    }
    #[cfg(test)]
    pub(super) fn current_close_fixture_preserves_history(&self, before: &FixtureHistory) -> bool {
        let state = self.state.lock().unwrap();
        state.failure.is_none()
            && state.pending_send.is_empty()
            && state.requests.0.len() == before.0.len()
            && before.0.iter().all(|prior| {
                state.requests.0.get(&prior.key).is_some_and(|now| {
                    now.owner == prior.owner
                        && now.operation == prior.operation
                        && now.body == prior.body
                        && now.sequence == prior.sequence
                        && now.error == prior.error
                })
            })
            && !state.requests.0.keys().any(|key| {
                matches!(
                    key,
                    Effect::PrepareCurrentCloseProfile(_)
                        | Effect::CollectCurrentCloseProfile(_)
                        | Effect::RetireCurrentCloseProfile(_)
                )
            })
    }
    #[cfg(test)]
    pub(super) fn current_close_fixture_history_empty(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.requests.0.is_empty() && state.pending_send.is_empty() && state.failure.is_none()
    }
    #[cfg(test)]
    pub(super) fn current_close_fixture_original_refusal(&self) -> io::Result<bool> {
        let state = self.state.lock().unwrap();
        if state.requests.0.len() != 1 || !state.pending_send.is_empty() || state.failure.is_some()
        {
            return Ok(false);
        }
        let (key, request) = state.requests.0.first_key_value().unwrap();
        if !matches!(key, Effect::PrepareOriginalConnect(_))
            || request.operation != Operation::PrepareOriginalConnect
        {
            return Ok(false);
        }
        let Some(sequence) = request.sequence else {
            return Ok(false);
        };
        let Some(body) = state.session.response(sequence)? else {
            return Ok(false);
        };
        Ok(
            matches!(serde_json::from_slice::<Reply>(body),Ok(Reply::Prepared(p)) if p.raw==0
            && p.status.operation=="ap_prepare_original_close" && p.status.returned==-1 && p.status.errno==Some(libc::EIO)),
        )
    }
    /// Issue exactly one trigger token from the retained preparation. Merely
    /// retrieving a response again may never authorize another GETREGSET.
    pub(super) fn claim_current_close_prepared(
        &self,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        sequence: u64,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .requests
            .0
            .get_mut(&Effect::PrepareCurrentCloseProfile(permit.lease))
            .filter(|e| {
                e.owner == permit.owner
                    && e.sequence == Some(sequence)
                    && e.operation == Operation::PrepareCurrentCloseProfile
                    && !e.current_close_issued
            })
            .ok_or_else(|| {
                io::Error::other("current Close trigger was already issued or changed preparation")
            })?;
        entry.current_close_issued = true;
        Ok(())
    }

    pub(super) fn retire_current_close_profile(
        &self,
        owner: NetworkStreamOwner,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        sequences: [u64; 3],
    ) -> io::Result<()> {
        let keys = [
            Effect::PrepareCurrentCloseProfile(permit.lease),
            Effect::CollectCurrentCloseProfile(permit.lease),
            Effect::RetireCurrentCloseProfile(permit.lease),
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
                    "current_close source retirement changed retained requests",
                ));
            }
        }
        if state
            .requests
            .3
            .is_some_and(|retired| permit.lease <= retired)
        {
            return Err(io::Error::other(
                "current Close retirement did not advance its original read",
            ));
        }
        state.session.retire_outgoing_current_close_profile(
            owner,
            permit.native_command_call(),
            sequences,
        )?;
        state.requests.3 = Some(permit.lease);
        for key in keys {
            state.requests.0.remove(&key);
        }
        self.changed.notify_waiters();
        Ok(())
    }

    pub(super) fn retire_executable_source(
        &self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        sequences: [u64; 3],
    ) -> io::Result<()> {
        let keys = [
            Effect::PrepareExecutableSource(call),
            Effect::CollectExecutableSource(call),
            Effect::RetireExecutableSource(call),
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
                    "executable source retirement changed retained requests",
                ));
            }
        }
        state.session.retire_outgoing_executable_source(
            owner,
            call.native_command_call(),
            sequences,
        )?;
        for key in keys {
            state.requests.0.remove(&key);
        }
        self.changed.notify_waiters();
        Ok(())
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
        Self::retire_consumed_births(state, changed, run)?;
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
    fn completed_response_is_retained_before_the_next_idle_poll() {
        use super::super::accepted_provider::CallStatus;
        use super::super::accepted_provider::Observation;
        let mut sockets = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );
        let controller =
            Controller::new(unsafe { OwnedFd::from_raw_fd(sockets[0]) }, [6; 16]).unwrap();
        let mut provider =
            AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(sockets[1]) }, [6; 16]).unwrap();
        let sequence = controller
            .prepare(
                Effect::Observation(1),
                owner(),
                &Request::ReadStatus,
                || Ok(vec![]),
            )
            .unwrap();
        controller.drive_once().unwrap();
        assert!(
            matches!(provider.try_receive().unwrap(), Some(Received::Request(s)) if s == sequence)
        );
        let reply = Reply::Status(Observation {
            status: CallStatus {
                operation: "ap_read_status".into(),
                returned: 0,
                errno: None,
            },
            raw: super::super::accepted_provider_ffi::Status::default().into(),
        });
        provider
            .dispatch(sequence, |_, _| Ok(serde_json::to_vec(&reply).unwrap()))
            .unwrap();
        assert!(provider.try_reply(sequence).unwrap());
        let retained = std::cell::Cell::new(false);
        let polled = std::cell::Cell::new(false);
        controller
            .drive_once_retained_with_poll(
                || {
                    assert!(matches!(
                        controller.retained_response(sequence)?,
                        Some(Reply::Status(_))
                    ));
                    retained.set(true);
                    Ok(())
                },
                |descriptors| {
                    assert!(
                        retained.get(),
                        "completed response waited before custody publication"
                    );
                    assert_eq!(descriptors.len(), 2);
                    assert_eq!(descriptors[0].events, libc::POLLIN);
                    polled.set(true);
                    0
                },
            )
            .unwrap();
        assert!(polled.get(), "an idle driver must still wait, not spin");
    }

    #[test]
    fn failed_transport_retains_custody_without_entering_idle_poll() {
        let mut sockets = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );
        let controller =
            Controller::new(unsafe { OwnedFd::from_raw_fd(sockets[0]) }, [6; 16]).unwrap();
        drop(unsafe { OwnedFd::from_raw_fd(sockets[1]) });
        let retained = std::cell::Cell::new(false);
        let error = controller
            .drive_once_retained_with_poll(
                || {
                    assert!(controller.state.lock().unwrap().failure.is_some());
                    retained.set(true);
                    Ok(())
                },
                |_| panic!("failed transport entered an idle wait"),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(retained.get());
    }

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

#[cfg(test)]
mod executable_tests {
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;

    use super::super::accepted_provider::CallStatus;
    use super::super::accepted_provider::Observation;
    use super::super::accepted_provider::executable_source::Intent;
    use super::super::accepted_provider::executable_source::controlled_collection;
    use super::*;
    fn owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(41);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn pair() -> (Arc<Controller>, AcceptedSession) {
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
        let wire = super::super::ProviderWireFormat::Abi11Copy5;
        (
            Arc::new(
                Controller::from_startup(unsafe { OwnedFd::from_raw_fd(pair[0]) }, [7; 16], wire)
                    .unwrap(),
            ),
            AcceptedSession::from_wire(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [7; 16], wire)
                .unwrap(),
        )
    }
    fn intent() -> Intent {
        Intent {
            command: 0,
            registration: 11,
            owner_mm: owner().mm.generation(),
            call: 13,
            address: 0x401020,
            length: 5,
            iovec: 0x700000,
            registers: 0x700100,
        }
    }
    fn status(operation: &str) -> CallStatus {
        CallStatus {
            operation: operation.into(),
            returned: 0,
            errno: None,
        }
    }
    #[test]
    fn executable_blocking_transport_has_no_tokio_dependency_and_requires_exact_ack() {
        let (controller, mut provider) = pair();
        let call = crate::network_replay::NetworkStreamCallId::controlled_fixture(13);
        let pin = std::fs::File::open("/dev/null").unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let driver = controller.clone();
        // Actual private socket/retained Inbox traffic on an independent native
        // thread. Provider C semantics below are an explicitly controlled premise.
        let worker = std::thread::spawn(move || {
            let mut phase = 0;
            while phase < 3 {
                assert!(Instant::now() < deadline, "bounded controlled service");
                driver
                    .drive_once_retained_with_poll(|| Ok(()), |_| 0)
                    .unwrap();
                if let Some(Received::Request(sequence)) = provider.try_receive().unwrap() {
                    let (envelope, rights, _) = provider.retained_request(sequence).unwrap();
                    let reply = match (
                        phase,
                        serde_json::from_slice::<Request>(&envelope.body).unwrap(),
                    ) {
                        (0, Request::PrepareExecutableSource { intent: requested }) => {
                            assert_eq!(requested, intent());
                            assert_eq!(rights.len(), 1);
                            Reply::Prepared(Observation {
                                status: status("ap_prepare_executable_source"),
                                raw: 7,
                            })
                        }
                        (
                            1,
                            Request::CollectExecutableSource {
                                call: 13,
                                command: 7,
                                prepared_request: 1,
                            },
                        ) => {
                            assert!(rights.is_empty());
                            let mut expected = intent();
                            expected.command = 7;
                            Reply::ExecutableSource(controlled_collection(expected))
                        }
                        (
                            2,
                            Request::RetireExecutableSource {
                                call: 13,
                                prepared: 1,
                                completed: 2,
                            },
                        ) => {
                            assert!(rights.is_empty());
                            provider
                                .check_incoming_executable_source(owner(), 13, 1, 2)
                                .unwrap();
                            Reply::ExecutableSourceRetired(status("ap_ack_command"))
                        }
                        _ => panic!("changed exact controlled wire sequence"),
                    };
                    provider
                        .dispatch(sequence, |_, _| Ok(serde_json::to_vec(&reply).unwrap()))
                        .unwrap();
                    if phase == 2 {
                        provider
                            .retire_incoming_executable_source(owner(), 13, [1, 2, 3])
                            .unwrap();
                    }
                    assert!(provider.try_reply(sequence).unwrap());
                    driver
                        .drive_once_retained_with_poll(|| Ok(()), |_| 0)
                        .unwrap();
                    phase += 1;
                }
                std::thread::yield_now();
            }
        });
        let prepared = controller
            .prepare(
                Effect::PrepareExecutableSource(call),
                owner(),
                &Request::PrepareExecutableSource { intent: intent() },
                || Ok(vec![pin.as_fd().try_clone_to_owned()?]),
            )
            .unwrap();
        assert_eq!(prepared, 1);
        assert!(matches!(
            controller
                .executable_response_blocking(prepared, deadline)
                .unwrap(),
            Reply::Prepared(_)
        ));
        assert!(
            !controller.quiescent().unwrap(),
            "positive ARM is still native debt"
        );
        let collected = controller
            .prepare(
                Effect::CollectExecutableSource(call),
                owner(),
                &Request::CollectExecutableSource {
                    call: 13,
                    command: 7,
                    prepared_request: prepared,
                },
                || Ok(vec![]),
            )
            .unwrap();
        assert!(matches!(
            controller
                .executable_response_blocking(collected, deadline)
                .unwrap(),
            Reply::ExecutableSource(_)
        ));
        assert!(
            !controller.quiescent().unwrap(),
            "positive collection is still ACK debt"
        );
        assert!(
            controller
                .retire_executable_source(owner(), call, [prepared, collected, 3])
                .is_err()
        );
        let retired = controller
            .prepare(
                Effect::RetireExecutableSource(call),
                owner(),
                &Request::RetireExecutableSource {
                    call: 13,
                    prepared,
                    completed: collected,
                },
                || Ok(vec![]),
            )
            .unwrap();
        assert!(matches!(
            controller
                .executable_response_blocking(retired, deadline)
                .unwrap(),
            Reply::ExecutableSourceRetired(_)
        ));
        controller
            .retire_executable_source(owner(), call, [prepared, collected, retired])
            .unwrap();
        assert!(controller.quiescent().unwrap());
        worker.join().unwrap();
    }
    #[test]
    fn executable_expiry_is_sticky_before_late_positive_reply() {
        let (controller, mut provider) = pair();
        let call = crate::network_replay::NetworkStreamCallId::controlled_fixture(13);
        let pin = std::fs::File::open("/dev/null").unwrap();
        let seq = controller
            .prepare(
                Effect::PrepareExecutableSource(call),
                owner(),
                &Request::PrepareExecutableSource { intent: intent() },
                || Ok(vec![pin.as_fd().try_clone_to_owned()?]),
            )
            .unwrap();
        controller
            .drive_once_retained_with_poll(|| Ok(()), |_| 0)
            .unwrap();
        assert!(matches!(provider.try_receive().unwrap(), Some(Received::Request(s)) if s == seq));
        assert_eq!(
            controller
                .executable_response_blocking(seq, Instant::now())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        // The original controller keeps both its request and sticky failure.
        // A retained known reply does not renew the original attempt's budget.
        let bytes = serde_json::to_vec(&Reply::Prepared(Observation {
            status: status("ap_prepare_executable_source"),
            raw: 7,
        }))
        .unwrap();
        provider.dispatch(seq, |_, _| Ok(bytes)).unwrap();
        assert!(provider.try_reply(seq).unwrap());
        {
            let mut state = controller.state.lock().unwrap();
            assert!(
                matches!(state.session.try_receive().unwrap(), Some(Received::Acknowledged(s)) if s == seq)
            );
        }
        assert!(matches!(
            controller.retained_response(seq).unwrap(),
            Some(Reply::Prepared(_))
        ));
        assert!(
            controller
                .executable_response_blocking(seq, Instant::now() + Duration::from_secs(1))
                .is_err()
        );
        assert!(controller.quiescent().is_err());
        assert!(
            controller
                .state
                .lock()
                .unwrap()
                .requests
                .0
                .contains_key(&Effect::PrepareExecutableSource(call))
        );
    }
}

#[cfg(test)]
mod terminal_drain_tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Wake;
    use std::task::Waker;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    struct Wakes(AtomicUsize);
    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn consumed_birth_cleanup(normal: bool) {
        use super::super::NetworkRuntimeResources;
        use super::super::ProviderWireFormat;
        use super::super::accepted_driver;
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let (mut owner, runtime) = unsafe {
            NetworkRuntimeResources::from_authenticated_startup(
                OwnedFd::from_raw_fd(pair[0]),
                [7; 16],
                ProviderWireFormat::Abi12Copy5,
            )
        };
        let controller = Arc::new(
            Controller::new(
                runtime
                    .shared
                    .endpoint
                    .as_ref()
                    .unwrap()
                    .as_fd()
                    .try_clone_to_owned()
                    .unwrap(),
                [7; 16],
            )
            .unwrap(),
        );
        *runtime.shared.controller.lock().unwrap() = Some(Ok(controller.clone()));
        let mut peer =
            AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [7; 16]).unwrap();
        let pin = std::fs::File::open("/dev/null").unwrap();
        let (who, observed, completed, rows) =
            super::super::accepted_transport::native_birth_test_group(1, 9, 0);
        let lease = serde_json::from_str("9").unwrap();
        let permit = crate::network_replay::NetworkFdPublicationPermit {
            owner: who,
            lease,
            files: crate::types::FilesId::initial(who.thread),
        };
        for ((envelope, body, rights), key) in rows.into_iter().zip([
            Effect::PrepareNativeBirth(lease),
            Effect::ObserveNativeBirth(lease),
            Effect::CollectNativeBirth(lease),
        ]) {
            let request = serde_json::from_slice(&envelope.body).unwrap();
            let sequence = controller
                .prepare(key, who, &request, || {
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
                matches!(peer.try_receive().unwrap(), Some(Received::Request(s)) if s == sequence)
            );
            peer.dispatch(sequence, |_, _| Ok(body.clone())).unwrap();
            if sequence == completed {
                peer.acknowledge_command_completion(sequence, |_, _| Ok(serde_json::to_vec(
                    &serde_json::json!({"Observed":{"operation":"ap_ack_command","returned":0,"errno":null}})
                ).unwrap())).unwrap();
            }
            assert!(peer.try_reply(sequence).unwrap());
            Controller::progress_io(
                &mut controller.state.lock().unwrap(),
                &controller.changed,
                [7; 16],
            )
            .unwrap();
        }
        controller
            .native_birth_semantics_consumed(
                permit,
                BirthSemantic::Child {
                    child: 62,
                    terminal: false,
                },
            )
            .unwrap();
        controller
            .native_birth_completion_consumed(permit, BirthCompletion::Returned(Ok(62)))
            .unwrap();
        let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let shared = runtime.shared.clone();
        let driven = controller.clone();
        let passes = AtomicUsize::new(0);
        *runtime.shared.driver.lock().unwrap() = Some(Ok(accepted_driver::Driver::start(
            controller.clone(),
            move || {
                shared.retain_completed_collections(&driven)?;
                let pass = passes.fetch_add(1, Ordering::SeqCst);
                if pass < 3 {
                    arrived_tx.send(pass).map_err(io::Error::other)?;
                    if pass < 2 {
                        release_rx
                            .recv_timeout(Duration::from_secs(3))
                            .map_err(io::Error::other)?;
                    }
                }
                Ok(())
            },
        )
        .unwrap()));
        assert_eq!(arrived_rx.recv_timeout(Duration::from_secs(3)).unwrap(), 0);
        let Some(Received::Request(retirement)) = peer.try_receive().unwrap() else {
            panic!("original retirement not sent");
        };
        assert_eq!(retirement, completed + 1);
        let original = Instant::now() + Duration::from_secs(1);
        *owner.shared.transport_terminal_deadline.lock().unwrap() = Some(original);
        let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
        let waker = Waker::from(wakes.clone());
        let mut context = Context::from_waker(&waker);
        let mut finishing: std::pin::Pin<Box<dyn Future<Output = io::Result<()>> + '_>> = if normal
        {
            Box::pin(unsafe { owner.finish_accepted_transport(original) })
        } else {
            Box::pin(unsafe { owner.finish_native_controller_tasks() })
        };
        let first = finishing.as_mut().poll(&mut context);
        let pending_before_ack = first.is_pending();
        let mut early = match first {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        };
        peer.retire_incoming_native_birth(who, 9, 1, observed, completed)
            .unwrap();
        peer.dispatch(retirement, |_, _| {
            serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
        })
        .unwrap();
        assert!(peer.try_reply(retirement).unwrap());
        peer.retire_sent_original_ack(retirement).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(arrived_rx.recv_timeout(Duration::from_secs(3)).unwrap(), 1);
        let ack_before_removal = controller.retained_response(retirement).unwrap().is_some()
            && !controller.quiescent().unwrap();
        let pending_after_ack = if early.is_none() {
            match finishing.as_mut().poll(&mut context) {
                Poll::Pending => true,
                Poll::Ready(result) => {
                    early = Some(result);
                    false
                }
            }
        } else {
            false
        };
        wakes.0.store(0, Ordering::SeqCst);
        release_tx.send(()).unwrap();
        assert_eq!(arrived_rx.recv_timeout(Duration::from_secs(3)).unwrap(), 2);
        let notified_after_removal = wakes.0.load(Ordering::SeqCst) > 0;
        let result = match early {
            Some(result) => result,
            None => {
                tokio::time::timeout_at(tokio::time::Instant::from_std(original), &mut finishing)
                    .await
                    .expect("original cleanup deadline")
            }
        };
        drop(finishing);
        let joined_at_return = runtime
            .shared
            .driver
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .joined();
        let deadline_at_return = *runtime.shared.transport_terminal_deadline.lock().unwrap();
        // Teardown is unconditional before the old-source positive assertion:
        // release both barriers and join the same real driver even on RED.
        owner.stop_collection_fixture(Instant::now() + Duration::from_secs(2));
        let state = controller.state.lock().unwrap();
        assert!(state.requests.0.is_empty());
        assert_eq!(state.session.terminal_custody().outgoing, 0);
        assert_eq!(state.session.terminal_custody().retained_rights, 0);
        assert_eq!(peer.terminal_custody().incoming, 0);
        assert_eq!(peer.terminal_custody().retained_rights, 0);
        assert!(peer.try_receive().unwrap().is_none());
        assert_eq!(deadline_at_return, Some(original));
        assert!(
            pending_before_ack,
            "cleanup must await the original birth retirement ACK: {result:?}"
        );
        assert!(
            ack_before_removal,
            "ACK receipt is not validated group removal"
        );
        assert!(
            pending_after_ack,
            "cleanup must await validated retirement removal"
        );
        assert!(
            notified_after_removal,
            "validated removal must wake the existing cleanup waiter without another message"
        );
        result.unwrap();
        assert!(
            joined_at_return,
            "successful cleanup must actually join the same driver"
        );
    }

    #[tokio::test]
    async fn failed_backend_waits_for_consumed_birth_ack_and_validated_removal() {
        consumed_birth_cleanup(false).await;
    }

    #[tokio::test]
    async fn normal_terminal_waits_for_consumed_birth_ack_and_validated_removal() {
        consumed_birth_cleanup(true).await;
    }

    #[tokio::test]
    async fn quiescence_wait_preserves_ready_and_sticky_failure_edges() {
        for fail_after_registration in [false, true] {
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
                Controller::new(unsafe { OwnedFd::from_raw_fd(pair[0]) }, [8; 16]).unwrap();
            let _peer = unsafe { OwnedFd::from_raw_fd(pair[1]) };
            controller.wait_quiescent().await.unwrap();
            let thread = crate::types::DetTid::from_raw(91);
            let who = NetworkStreamOwner {
                thread,
                mm: crate::types::MmId::initial(thread),
            };
            controller
                .prepare(Effect::Observation(1), who, &Request::ReadStatus, || {
                    Ok(vec![])
                })
                .unwrap();
            let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
            let waker = Waker::from(wakes.clone());
            let mut waiting = Box::pin(controller.wait_quiescent());
            if fail_after_registration {
                assert!(
                    waiting
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
            }
            controller.fail(&io::Error::other("first terminal failure"));
            controller.fail(&io::Error::other("later failure cannot replace the first"));
            assert_eq!(
                waiting.await.unwrap_err().to_string(),
                "first terminal failure"
            );
            if fail_after_registration {
                assert!(wakes.0.load(Ordering::SeqCst) > 0);
            }
            assert_eq!(
                controller.wait_quiescent().await.unwrap_err().to_string(),
                "first terminal failure"
            );
        }
    }

    #[tokio::test]
    async fn canceled_quiescence_wait_and_late_ack_cannot_renew_terminal_deadline() {
        use super::super::NetworkRuntimeResources;
        use super::super::ProviderWireFormat;
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let (mut owner, runtime) = unsafe {
            NetworkRuntimeResources::from_authenticated_startup(
                OwnedFd::from_raw_fd(pair[0]),
                [9; 16],
                ProviderWireFormat::Abi8Copy5,
            )
        };
        let controller = runtime.accepted_controller().unwrap();
        let mut peer =
            AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [9; 16]).unwrap();
        let thread = crate::types::DetTid::from_raw(91);
        let who = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let sequence = controller
            .prepare(Effect::Observation(1), who, &Request::ReadStatus, || {
                Ok(vec![])
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match peer.try_receive().unwrap() {
                    Some(Received::Request(s)) => {
                        assert_eq!(s, sequence);
                        break;
                    }
                    None => tokio::task::yield_now().await,
                    _ => panic!("different original request"),
                }
            }
        })
        .await
        .unwrap();
        let original = Instant::now() + Duration::from_secs(1);
        *owner.shared.transport_terminal_deadline.lock().unwrap() = Some(original);
        let before_cancel = {
            let mut waiting = Box::pin(unsafe { owner.finish_native_controller_tasks() });
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        };
        let outcome = unsafe { owner.finish_native_controller_tasks().await };
        let deadline_after_retry = *owner.shared.transport_terminal_deadline.lock().unwrap();
        let joined_before_ack = runtime
            .shared
            .driver
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .joined();
        peer.dispatch(sequence, |_, _| {
            serde_json::to_vec(&Reply::Status(
                super::super::accepted_provider::Observation {
                    status: super::super::accepted_provider::CallStatus {
                        operation: "ap_read_status".into(),
                        returned: 0,
                        errno: None,
                    },
                    raw: super::super::accepted_provider_ffi::Status::default().into(),
                },
            ))
            .map_err(io::Error::other)
        })
        .unwrap();
        assert!(peer.try_reply(sequence).unwrap());
        tokio::time::timeout(Duration::from_secs(1), controller.response(sequence))
            .await
            .unwrap()
            .unwrap();
        let late = unsafe { owner.finish_native_controller_tasks().await };
        let deadline_after_late = *owner.shared.transport_terminal_deadline.lock().unwrap();
        let joined_after_late = runtime
            .shared
            .driver
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .joined();
        owner.stop_collection_fixture(Instant::now() + Duration::from_secs(2));
        assert!(before_cancel);
        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("retained for unresolved")
        );
        assert!(Instant::now() >= original);
        assert_eq!(deadline_after_retry, Some(original));
        assert!(!joined_before_ack);
        assert!(late.is_err());
        assert_eq!(deadline_after_late, Some(original));
        assert!(!joined_after_late);
        assert!(peer.try_receive().unwrap().is_none());
    }
}
