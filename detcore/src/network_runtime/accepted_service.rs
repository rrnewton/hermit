//! Accepted-only outside-provider service loop. The parent-owned unit transports
//! the private startup endpoint on stdin; all socket rights stay in durable
//! session inboxes across callbacks, errors, lost replies and controller exit.

mod process;
use std::ffi::CString;
use std::io;
use std::num::NonZeroU64;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

pub use process::run_accepted_provider_process;

use super::accepted_parent::BootstrapFailure;
use super::accepted_parent::BootstrapReply;
use super::accepted_parent::ProviderArtifact;
use super::accepted_provider::PidfdIdentity;
use super::accepted_provider::Provider;
use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::accepted_transport::AcceptedSession;
use super::accepted_transport::Envelope;
use super::accepted_transport::Operation;
use super::accepted_transport::Received;
use super::accepted_transport::controller_exited;

fn duplicate(fd: &OwnedFd) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Created only with the original empty service. Candidate aliases and queued
/// SCM remain ordinary custody. Consumed only after authentication, run import,
/// liveness and the original cutoff, before the first native attempt.
struct NeverAuthorized {
    startup_cutoff_ns: NonZeroU64,
}

struct Authorized {
    controller: PidfdIdentity,
}

fn bind_controller(
    controller: &mut Option<OwnedFd>,
    never_authorized: &mut Option<NeverAuthorized>,
    original: &OwnedFd,
    duplicate: impl FnOnce(&OwnedFd) -> io::Result<OwnedFd>,
) -> io::Result<()> {
    if controller.is_some() || never_authorized.is_none() {
        return Err(io::Error::other(
            "accepted controller binding already attempted",
        ));
    }
    let cutoff = never_authorized.as_ref().unwrap().startup_cutoff_ns.get();
    if !process::monotonic_ns().is_some_and(|now| now < cutoff) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "original accepted startup cutoff elapsed before controller binding",
        ));
    }
    // Duplication does not authenticate a PIDFD or authorize provider work.
    // Keep even a wrong-type alias through dedicated PF_EXITING refusal.
    let alias = duplicate(original)?;
    *controller = Some(alias);
    PidfdIdentity::read(controller.as_ref().unwrap())?;
    Ok(())
}

fn import_run(
    candidate: &mut Option<OwnedFd>,
    session: &mut Option<AcceptedSession>,
    original: &OwnedFd,
    run: [u8; 16],
    wire: super::ProviderWireFormat,
) -> io::Result<()> {
    if candidate.is_some() || session.is_some() {
        return Err(io::Error::other("accepted run binding already attempted"));
    }
    *candidate = Some(duplicate(original)?);
    match AcceptedSession::from_wire(candidate.take().unwrap(), run, wire) {
        Ok(owned) => *session = Some(owned),
        Err((error, owned)) => {
            *candidate = Some(owned);
            return Err(error);
        }
    }
    Ok(())
}

/// The only permission transition. No fallible native operation may precede it.
fn authorize_provider(
    never: &mut Option<NeverAuthorized>,
    authorized: &mut Option<Authorized>,
    controller: &Option<OwnedFd>,
    run: &Option<AcceptedSession>,
) -> io::Result<()> {
    let pending = never
        .as_ref()
        .ok_or_else(|| io::Error::other("provider permission already spent"))?;
    if authorized.is_some() || run.is_none() {
        return Err(io::Error::other(
            "provider permission lacks original run session",
        ));
    }
    let pin = controller
        .as_ref()
        .ok_or_else(|| io::Error::other("original controller absent"))?;
    let identity = PidfdIdentity::read(pin)?;
    if controller_exited(pin.as_fd())? {
        return Err(io::Error::other(
            "controller exited before provider startup",
        ));
    }
    if !process::monotonic_ns().is_some_and(|now| now < pending.startup_cutoff_ns.get()) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "original accepted startup cutoff elapsed before provider permission",
        ));
    }
    *authorized = Some(Authorized {
        controller: identity,
    });
    never.take();
    Ok(())
}

fn command_preparation(
    session: &AcceptedSession,
    envelope: &Envelope,
    call: u64,
    command: u64,
    prepared: u64,
    operation: Operation,
) -> io::Result<Vec<OwnedFd>> {
    if envelope.owner.is_none() || envelope.accept.is_some() || call == 0 || command == 0 {
        return Err(io::Error::other(
            "original command lacks exact owner/ticket",
        ));
    }
    let (prior, pins, outcome) = session.retained_request(prepared)?;
    if prior.operation != operation || prior.owner != envelope.owner || prior.accept.is_some() {
        return Err(io::Error::other("original command changed retained target"));
    }
    let (expected, expected_pins) =
        match (operation, serde_json::from_slice::<Request>(&prior.body)?) {
            (
                Operation::PrepareOriginalConnect,
                Request::PrepareOriginalConnect { call, kind, .. },
            ) => {
                if let Request::CollectOriginalConnect {
                    kind: submitted, ..
                } = serde_json::from_slice(&envelope.body)?
                    && submitted != kind {
                        return Err(io::Error::other(
                            "original completion changed prepared syscall kind",
                        ));
                    }
                (call, 1)
            }
            (
                Operation::PrepareOriginalFileObservation,
                Request::PrepareOriginalFileObservation { call, role, .. },
            ) if role.valid() => {
                if let Request::CollectOriginalFileObservation {
                    role: submitted, ..
                } = serde_json::from_slice(&envelope.body)?
                    && submitted != role {
                        return Err(io::Error::other(
                            "auxiliary collection changed prepared role",
                        ));
                    }
                (call, if role.is_receive() { 2 } else { 1 })
            }
            (Operation::PrepareNativeBirth, Request::PrepareNativeBirth { call, .. }) => (call, 1),
            _ => {
                return Err(io::Error::other(
                    "original command names another preparation",
                ));
            }
        };
    let Reply::Prepared(observed) = serde_json::from_slice(
        outcome.ok_or_else(|| io::Error::other("original preparation remains unknown"))?,
    )?
    else {
        return Err(io::Error::other(
            "original command lacks preparation response",
        ));
    };
    if pins.len() != expected_pins
        || expected != call
        || observed.status.returned != 0
        || observed.status.errno.is_some()
        || observed.raw != command
    {
        return Err(io::Error::other(
            "original command changed preparation identity",
        ));
    }
    pins.iter().map(duplicate).collect()
}

fn original_preparation(
    session: &AcceptedSession,
    envelope: &Envelope,
    call: u64,
    command: u64,
    prepared: u64,
) -> io::Result<Vec<OwnedFd>> {
    command_preparation(
        session,
        envelope,
        call,
        command,
        prepared,
        Operation::PrepareOriginalConnect,
    )
}

fn copy_preparation(
    session: &AcceptedSession,
    envelope: &Envelope,
    call: u64,
    command: u64,
    prepared: u64,
) -> io::Result<Vec<OwnedFd>> {
    let (prior, _, _) = session.retained_request(prepared)?;
    let operation = match serde_json::from_slice::<Request>(&prior.body)? {
        Request::PrepareOriginalConnect {
            kind: crate::network_replay::original_connect::Kind::Read,
            ..
        } if prior.operation == Operation::PrepareOriginalConnect => {
            Operation::PrepareOriginalConnect
        }
        Request::PrepareOriginalFileObservation { role, .. }
            if prior.operation == Operation::PrepareOriginalFileObservation
                && role.is_receive()
                && role.valid() =>
        {
            Operation::PrepareOriginalFileObservation
        }
        _ => {
            return Err(io::Error::other(
                "native copy request lacks an authenticated receive preparation",
            ));
        }
    };
    let mut pins = command_preparation(session, envelope, call, command, prepared, operation)?;
    // Read has one task; helper receive retains [owner, worker]. Project only
    // the command target after validating that role's exact arity. These are
    // duplicates: both original rights stay in transport custody, in order.
    let command_pin = pins.pop().expect("validated copy preparation rights");
    Ok(vec![command_pin])
}

// Read's copy/terminal ring join delays only submission of the existing
// physical completion. It never substitutes for that completion or its ACK.
fn original_copy_ready(
    provider: &mut Provider,
    session: &AcceptedSession,
    envelope: &Envelope,
    rights: usize,
) -> io::Result<bool> {
    let request: Request = serde_json::from_slice(&envelope.body)?;
    if let Request::ReadOriginalCopy {
        call,
        command,
        prepared,
        first,
    } = request
    {
        if envelope.operation != Operation::ReadOriginalCopy || rights != 0 {
            return Err(io::Error::other("Read copy progress envelope mismatch"));
        }
        let pins = copy_preparation(session, envelope, call, command, prepared)?;
        if session.retained_read_copy_ready(prepared, first)? {
            return Ok(true);
        }
        let progress = provider.read_copy_progress(pins[0].as_fd(), command)?;
        if first > progress.records {
            return Err(io::Error::other(
                "Read copy progress skipped native records",
            ));
        }
        return Ok(first < progress.records || progress.exited == 1 || progress.terminal == 1);
    }
    let (call, command, prepared, terminal) = match request {
        Request::CollectOriginalConnect {
            kind: crate::network_replay::original_connect::Kind::Read,
            call,
            command,
            prepared_request,
        } if envelope.operation == Operation::CollectOriginalConnect => {
            (call, command, prepared_request, false)
        }
        Request::CollectOriginalFileObservation {
            role,
            call,
            command,
            prepared_request,
        } if envelope.operation == Operation::CollectOriginalFileObservation
            && role.is_receive() =>
        {
            (call, command, prepared_request, false)
        }
        Request::TerminateOriginalConnect {
            call,
            command,
            prepared_request,
            ..
        } if envelope.operation == Operation::TerminateOriginalConnect => {
            let (prior, _, _) = session.retained_request(prepared_request)?;
            if !matches!(
                serde_json::from_slice::<Request>(&prior.body)?,
                Request::PrepareOriginalConnect {
                    kind: crate::network_replay::original_connect::Kind::Read,
                    ..
                }
            ) {
                return Ok(true);
            }
            (call, command, prepared_request, true)
        }
        _ => return Ok(true),
    };
    if rights != 0 {
        return Err(io::Error::other("Read completion received rights"));
    }
    let pins = copy_preparation(session, envelope, call, command, prepared)?;
    provider.original_read_copy_ready(pins[0].as_fd(), command, terminal)
}

fn validate_observation(envelope: &Envelope, rights: usize) -> io::Result<()> {
    if envelope.operation != Operation::DrainCreations
        || rights != 0
        || envelope.owner.is_none()
        || envelope.accept.is_some()
    {
        return Err(io::Error::other(
            "observation envelope is not an authenticated read-only request",
        ));
    }
    Ok(())
}

// This is service maintenance, not a guest scheduling quantum, deadline or
// recorded release time. The service can process every other request meanwhile.
const OBSERVATION_MAINTENANCE: Duration = Duration::from_millis(10);
const ACTIVE_OBSERVATION_MAINTENANCE: Duration = Duration::from_millis(1);

fn observation_maintenance(active: bool) -> Duration {
    if active {
        ACTIVE_OBSERVATION_MAINTENANCE
    } else {
        OBSERVATION_MAINTENANCE
    }
}
#[derive(Debug)]
struct PendingObservation {
    request: u64,
    creation: u32,
    next_probe: Instant,
}
impl PendingObservation {
    fn probe(
        &mut self,
        now: Instant,
        read: impl FnOnce(u32) -> io::Result<Option<Vec<u8>>>,
    ) -> io::Result<Option<Vec<u8>>> {
        if now < self.next_probe {
            return Ok(None);
        }
        self.next_probe = now + OBSERVATION_MAINTENANCE;
        read(self.creation)
    }
}

#[must_use = "retain this service owner until actual controller exit and explicit provider drain"]
pub(super) struct AcceptedProviderService {
    never_authorized: Option<NeverAuthorized>,
    authorized: Option<Authorized>,
    bootstrap: AcceptedSession,
    run: Option<AcceptedSession>,
    run_candidate: Option<OwnedFd>,
    controller: Option<OwnedFd>,
    provider: Provider,
    incarnation: [u8; 16],
    library: CString,
    object: CString,
    // Actual immutable library descriptor installed before the first receive.
    // Grouped Bridge custody consumes this owner, never reopens its /proc path.
    grouped_library: Option<OwnedFd>,
    grouped_bridge: Option<super::grouped_broker::Bridge>,
    grouped_pending: Option<super::grouped_broker::RuntimeCleanup>,
    grouped_owner: Option<super::accepted_provider_ffi::GroupedBootstrapOwner>,
    grouped_leaves: Vec<OwnedFd>,
    grouped_installed: bool,
    grouped_pre_open_recovery: Option<String>,
    bootstrap_request: Option<u64>,
    bootstrap_reply: Option<u64>,
    bootstrap_sent: bool,
    bootstrap_failure: Option<BootstrapFailure>,
    bootstrap_failure_sent: bool,
    bootstrap_failure_send_error: Option<String>,
    run_replies: Vec<u64>,
    failure: Option<String>,
    observation: Option<PendingObservation>,
    last_observation: Option<u64>,
    fd_observation: Option<(u64, u64, Instant)>,
    last_fd_observation: Option<u64>,
    last_fd_probe: Option<Vec<u8>>,
    run_peer_ended: bool,
}

/// A controller's endpoint closes before its pidfd reports exit, so the run
/// peer's end-of-stream stops run effects without failing the service. It is
/// never a terminal proof; only the retained controller pidfd ends the service.
fn run_receive(
    session: &mut AcceptedSession,
    peer_ended: &mut bool,
) -> io::Result<Option<Received>> {
    match session.try_receive() {
        Err(error) if super::accepted_transport::is_peer_end_of_stream(&error) => {
            *peer_ended = true;
            Ok(None)
        }
        other => other,
    }
}

impl AcceptedProviderService {
    /// # Safety
    /// The stdin endpoint and both immutable artifacts must come from the exact
    /// reviewed parent capability-unit startup. The helper's loader environment
    /// and dependencies must satisfy the adapter's unsafe library contract.
    pub(super) unsafe fn from_private_stdin(
        stdin: OwnedFd,
        run: [u8; 16],
        library: CString,
        object: CString,
        startup_cutoff_ns: NonZeroU64,
    ) -> Result<Self, (io::Error, OwnedFd)> {
        let bootstrap = AcceptedSession::new(stdin, run)?;
        Ok(Self {
            never_authorized: Some(NeverAuthorized { startup_cutoff_ns }),
            authorized: None,
            bootstrap,
            run: None,
            run_candidate: None,
            controller: None,
            provider: Provider::empty(),
            incarnation: run,
            library,
            object,
            grouped_library: None,
            grouped_bridge: None,
            grouped_pending: None,
            grouped_owner: None,
            grouped_leaves: Vec::new(),
            grouped_installed: false,
            grouped_pre_open_recovery: None,
            bootstrap_request: None,
            bootstrap_reply: None,
            bootstrap_sent: false,
            bootstrap_failure: None,
            bootstrap_failure_sent: false,
            bootstrap_failure_send_error: None,
            run_replies: Vec::new(),
            failure: None,
            observation: None,
            last_observation: None,
            fd_observation: None,
            last_fd_observation: None,
            last_fd_probe: None,
            run_peer_ended: false,
        })
    }

    /// Error borrows rather than consumes this owner. The caller must continue
    /// recovery with its retained controller/endpoint/provider capabilities.
    pub(super) fn step(&mut self) -> io::Result<()> {
        self.check_startup_cutoff();
        if let Some(error) = &self.failure {
            return Err(io::Error::other(error.clone()));
        }
        let outcome = self.step_inner();
        if let Err(error) = &outcome {
            self.failure = Some(error.to_string());
        }
        outcome
    }

    fn check_startup_cutoff(&mut self) {
        let Some(never) = &self.never_authorized else {
            return; // An admitted controller has no guest-lifetime deadline.
        };
        let error = match process::monotonic_ns() {
            Some(now) if now < never.startup_cutoff_ns.get() => return,
            Some(_) => "original accepted startup cutoff elapsed before authorization",
            None => "original accepted startup clock unavailable before authorization",
        };
        self.failure.get_or_insert_with(|| error.into());
    }

    fn take_pre_authorization_refusal(&mut self) -> Option<NeverAuthorized> {
        self.failure.as_ref()?;
        self.never_authorized.as_ref()?;
        // Do not turn a contradictory/partially installed native owner into
        // exit authority. The one-way state is the proof; these are defenses.
        if self.authorized.is_some()
            || self.grouped_bridge.is_some()
            || self.grouped_pending.is_some()
            || self.grouped_owner.is_some()
            || !self.grouped_leaves.is_empty()
            || self.grouped_installed
            || self.bootstrap_reply.is_some()
            || self.bootstrap_sent
        {
            return None;
        }
        self.never_authorized.take()
    }

    /// Failure notification is independent of the failed provider effect. Only
    /// EAGAIN permits another send attempt; no provider callback runs again and
    /// neither a successful send nor parent acknowledgement releases custody.
    fn notify_bootstrap_failure(&mut self) {
        if self.bootstrap_reply.is_some()
            || self.bootstrap_failure_sent
            || self.bootstrap_failure_send_error.is_some()
        {
            return;
        }
        let (Some(sequence), Some(first)) = (self.bootstrap_request, self.failure.as_ref()) else {
            return;
        };
        let failure = self
            .bootstrap_failure
            .get_or_insert_with(|| BootstrapFailure {
                error: first.clone(),
            });
        match self
            .bootstrap
            .try_bootstrap_failure_reply(sequence, failure)
        {
            Ok(sent) => self.bootstrap_failure_sent = sent,
            Err(error) => self.bootstrap_failure_send_error = Some(error.to_string()),
        }
    }

    fn step_inner(&mut self) -> io::Result<()> {
        if let Some(sequence) = self.bootstrap_reply {
            if !self.bootstrap_sent {
                self.bootstrap_sent = self.bootstrap.try_reply(sequence)?;
            }
            if !self.bootstrap_sent {
                return Ok(());
            }
        } else {
            let Some(Received::Request(sequence)) = self.bootstrap.try_receive()? else {
                return Ok(());
            };
            self.bootstrap_request = Some(sequence);
            let run = self.incarnation;
            let provider = &mut self.provider;
            let controller = &mut self.controller;
            let never_authorized = &mut self.never_authorized;
            let authorized = &mut self.authorized;
            let run_session = &mut self.run;
            let run_candidate = &mut self.run_candidate;
            let library = &self.library;
            let object = &self.object;
            let grouped_library = &mut self.grouped_library;
            let grouped_bridge = &mut self.grouped_bridge;
            let grouped_pending = &mut self.grouped_pending;
            let grouped_owner = &mut self.grouped_owner;
            let grouped_leaves = &mut self.grouped_leaves;
            let grouped_installed = &mut self.grouped_installed;
            let grouped_pre_open_recovery = &mut self.grouped_pre_open_recovery;
            self.bootstrap.dispatch(sequence, |envelope, rights| {
                if !matches!(
                    (envelope.operation, rights.len()),
                    (Operation::Bootstrap, 2) | (Operation::GroupedBootstrap, 3)
                ) || envelope.owner.is_some()
                    || envelope.accept.is_some()
                    || sequence != 1
                {
                    return Err(io::Error::other("invalid accepted service bootstrap"));
                }
                let expected: ProviderArtifact = serde_json::from_slice(&envelope.body)?;
                // Original rights are already durable in bootstrap.incoming.
                // These retained aliases establish the long-lived service path.
                bind_controller(controller, never_authorized, &rights[0], duplicate)?;
                import_run(
                    run_candidate,
                    run_session,
                    &rights[1],
                    run,
                    expected.wire_format,
                )?;
                authorize_provider(never_authorized, authorized, controller, run_session)?;
                if envelope.operation == Operation::GroupedBootstrap {
                    // Retain every acquired owner before fallible import. The
                    // exact original request/three rights stay Submitted until
                    // this entire operation returns its actual READY response.
                    *grouped_pending =
                        Some(super::grouped_broker::RuntimeCleanup::retain_bootstrap(
                            duplicate(&rights[2])?,
                            run,
                        ));
                    let file = grouped_library
                        .take()
                        .ok_or_else(|| io::Error::other("actual sealed grouped library absent"))?;
                    *grouped_bridge = Some(super::grouped_broker::Bridge::retain(
                        file,
                        expected.library_sha256,
                    ));
                    let startup_result: io::Result<()> = (|| {
                        let runtime = grouped_pending.as_mut().unwrap();
                        runtime.initialize_bootstrap(controller.as_ref().unwrap().as_fd())?;
                        runtime.prepare_creation_peer()?;
                        runtime.receive_leaves()?;
                        runtime.announce_successor_creator()?;
                        runtime.duplicate_leaves_into(grouped_leaves)?;
                        let (incarnation, nonce, deadline, creator_cutoff, unit) =
                            runtime.adoption()?;
                        let endpoint = runtime.source_endpoint()?.try_clone_to_owned()?;
                        let pin = controller.as_ref().unwrap().try_clone()?;
                        *grouped_owner = Some(
                            super::accepted_provider_ffi::GroupedBootstrapOwner::retain_bootstrap(
                                grouped_bridge.take().unwrap(),
                                pin,
                                endpoint,
                                grouped_pending.take().unwrap(),
                            ),
                        );
                        provider.install_grouped(grouped_owner)?;
                        *grouped_installed = true;
                        let owned = provider.grouped_startup_mut()?;
                        // SAFETY: this is the same sealed accepted DSO authenticated
                        // before the early service was dispatched; actual grouped
                        // owners were installed before any dlopen/adoption/open.
                        unsafe {
                            owned.initialize_bridge()?;
                        }
                        owned.adopt(
                            incarnation,
                            &nonce,
                            deadline,
                            creator_cutoff,
                            &unit,
                            [
                                grouped_leaves[0].as_fd(),
                                grouped_leaves[1].as_fd(),
                                grouped_leaves[2].as_fd(),
                            ],
                        )?;
                        owned.install_retained_runtime_cleanup()?;
                        Ok(())
                    })();
                    if let Err(primary) = startup_result {
                        let cleanup = if *grouped_installed {
                            provider
                                .grouped_startup_mut()
                                .and_then(|owner| owner.recover_pre_open(&primary, grouped_leaves))
                        } else if let Some(owner) = grouped_owner.as_mut() {
                            owner.recover_pre_open(&primary, grouped_leaves)
                        } else if let (Some(runtime), Some(bridge)) =
                            (grouped_pending.as_mut(), grouped_bridge.as_mut())
                        {
                            // No native provider owner/open exists in this branch.
                            // Capture this original error now, never at later retirement.
                            runtime
                                .recover_pending_pre_open(bridge, &primary, grouped_leaves)
                                .map(|outcome| format!("{outcome:?}"))
                        } else {
                            Err(io::Error::other(
                                "actual pre-open owner unavailable; retained custody UNKNOWN",
                            ))
                        };
                        *grouped_pre_open_recovery = Some(match cleanup {
                            Ok(outcome) => format!("actual pre-open recovery: {outcome}"),
                            Err(error) => format!("pre-open recovery retained UNKNOWN: {error}"),
                        });
                        eprintln!("{}", grouped_pre_open_recovery.as_ref().unwrap());
                        // Cleanup completion does not turn this exact submitted
                        // bootstrap into READY or permit Provider::open below.
                        return Err(primary);
                    }
                }
                // SAFETY: from_private_stdin's owning launcher authenticated
                // immutable artifacts/dependencies before this service existed.
                let ready = unsafe { provider.open(library, object, run, &expected) }?;
                serde_json::to_vec(&BootstrapReply::Ready(Box::new(ready))).map_err(io::Error::other)
            })?;
            self.bootstrap_reply = Some(sequence);
            self.bootstrap_sent = self.bootstrap.try_reply(sequence)?;
            return Ok(());
        }
        self.provider.drain_copy()?;
        let session = self.run.as_mut().unwrap();
        // The existing inbox is the only request owner. Poll every admitted
        // original selection independently of its blocked guest callback.
        for sequence in session.pending_original_selections() {
            let (envelope, rights, _) = session.retained_request(sequence)?;
            if !rights.is_empty() {
                return Err(io::Error::other("original query received rights"));
            }
            let Request::AwaitOriginalSelection {
                call,
                command,
                prepared_request,
            } = serde_json::from_slice(&envelope.body)?
            else {
                return Err(io::Error::other(
                    "original pending request changed operation",
                ));
            };
            let pins = original_preparation(session, envelope, call, command, prepared_request)?;
            if let Some(body) = self.provider.poll_original_selection(envelope, &pins)? {
                session.finish_observation(sequence, body)?;
                self.run_replies.push(sequence);
            }
        }
        if let Some(pending) = &mut self.observation
            && let Some(body) = pending.probe(Instant::now(), |ordinal| {
                self.provider.poll_creation(ordinal)
            })?
        {
                session.finish_observation(pending.request, body)?;
                self.run_replies.push(pending.request);
                self.observation = None;
            }

        if let Some((request, ordinal, next_probe)) = &mut self.fd_observation
            && Instant::now() >= *next_probe
        {
                *next_probe = Instant::now() + OBSERVATION_MAINTENANCE;
                let (ready, body) = self.provider.poll_fd_event(*ordinal)?;
                self.last_fd_probe = Some(body.clone());
                if ready {
                    session.finish_observation(*request, body)?;
                    // Original bytes belong to Inbox before exact physical ACK.
                    let ack = session
                        .acknowledge_command_completion(*request, |envelope, body| {
                            self.provider.acknowledge_fd_observation(envelope, body)
                        })?;
                    Provider::validate_command_acknowledgement(&ack)?;
                    self.run_replies.push(*request);
                    self.fd_observation = None;
                }
            }

        while let Some(sequence) = self.run_replies.first().copied() {
            if !session.try_reply(sequence)? {
                return Ok(());
            }
            session.retire_sent_original_ack(sequence)?;
            self.run_replies.remove(0);
        }
        // Ring readiness is a prerequisite to the existing one-shot Read
        // collection, not another native effect. An undrained commit remains
        // in the same undispatched Inbox entry while other requests progress.
        let mut ready_read = None;
        for sequence in session.pending_original_copy_completions() {
            let (envelope, rights, _) = session.retained_request(sequence)?;
            if original_copy_ready(&mut self.provider, session, envelope, rights.len())? {
                ready_read = Some(sequence);
                break;
            }
        }
        let sequence = if let Some(sequence) = ready_read {
            sequence
        } else {
            let Some(received) = run_receive(session, &mut self.run_peer_ended)? else {
                return Ok(());
            };
            let Received::Request(sequence) = received else {
                return Err(io::Error::other(
                    "unexpected acknowledgement at provider service",
                ));
            };
            sequence
        };
        let (envelope, rights, _) = session.retained_request(sequence)?;
        let request: Request = serde_json::from_slice(&envelope.body)?;
        if let Request::RetireTerminalSocketObservation { call, observed } = &request {
            if envelope.operation != Operation::RetireTerminalSocketObservation
                || !rights.is_empty()
                || envelope.owner.is_none()
                || envelope.accept.is_some()
                || sequence <= *observed
            {
                return Err(io::Error::other(
                    "terminal Socket retirement envelope changed",
                ));
            }
            session.retire_incoming_terminal_socket(envelope.owner.unwrap(), *call, *observed)?;
            session.dispatch(sequence, |_, _| {
                serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
            })?;
            self.run_replies.push(sequence);
            return Ok(());
        }
        if !original_copy_ready(&mut self.provider, session, envelope, rights.len())? {
            return Ok(());
        }
        if let Request::ReadOriginalCopy {
            call,
            command,
            prepared,
            first,
        } = &request
        {
            if session.retained_request(sequence)?.2.is_some() {
                self.run_replies.push(sequence);
                return Ok(());
            }
            let pins = copy_preparation(session, envelope, *call, *command, *prepared)?;
            let envelope = envelope.clone();
            let rights = rights.len();
            let Some(chunk) = session.read_copy_chunk(
                &envelope,
                rights,
                (*call, *command),
                *prepared,
                *first,
                || {
                    self.provider
                        .copy_prefix(pins[0].as_fd(), *command, *prepared, *first)
                },
            )?
            else {
                return Ok(());
            };
            session.dispatch(sequence, |_, _| {
                serde_json::to_vec(&Reply::OriginalReadCopy(chunk)).map_err(io::Error::other)
            })?;
            self.run_replies.push(sequence);
            return Ok(());
        }
        if let Request::RetireNativeBirth {
            call,
            prepared,
            observed,
            completed,
        } = &request
        {
            if envelope.operation != Operation::RetireNativeBirth
                || !rights.is_empty()
                || envelope.owner.is_none()
                || envelope.accept.is_some()
                || sequence <= *completed
            {
                return Err(io::Error::other("birth retirement envelope mismatch"));
            }
            let owner = envelope.owner.unwrap();
            session.retire_incoming_native_birth(owner, *call, *prepared, *observed, *completed)?;
            session.dispatch(sequence, |_, _| {
                serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
            })?;
            self.run_replies.push(sequence);
            return Ok(());
        }
        if let Request::RetireOriginalFileObservation {
            call,
            prepared,
            completed,
        } = &request
        {
            if envelope.operation != Operation::RetireOriginalFileObservation
                || !rights.is_empty()
                || envelope.owner.is_none()
                || envelope.accept.is_some()
                || sequence <= *completed
            {
                return Err(io::Error::other("auxiliary retirement envelope mismatch"));
            }
            let owner = envelope.owner.unwrap();
            session.check_incoming_file_observation(owner, *call, *prepared, *completed)?;
            let (_, pins, _) = session.retained_request(*prepared)?;
            let pins = pins.iter().map(duplicate).collect::<io::Result<Vec<_>>>()?;
            let provider = &mut self.provider;
            session.dispatch(sequence, |envelope, rights| {
                provider.dispatch(envelope, rights, Some(&pins))
            })?;
            let (_, _, result) = session.retained_request(sequence)?;
            if !matches!(serde_json::from_slice::<Reply>(result.ok_or_else(|| io::Error::other("auxiliary retirement has no result"))?),
                Ok(Reply::OriginalFileObservationRetired(status)) if status.returned == 0 && status.errno.is_none())
            {
                return Err(io::Error::other(
                    "auxiliary task-storage retirement remains unresolved",
                ));
            }
            session.retire_incoming_file_observation(owner, *call, *prepared, *completed)?;
            self.run_replies.push(sequence);
            return Ok(());
        }
        if let Request::CollectOriginalFileObservation {
            call,
            command,
            prepared_request,
            role,
        } = &request
        {
            if envelope.operation != Operation::CollectOriginalFileObservation
                || !rights.is_empty()
                || envelope.accept.is_some()
            {
                return Err(io::Error::other("auxiliary collection envelope mismatch"));
            }
            let (prior, pins, body) = session.retained_request(*prepared_request)?;
            if prior.operation != Operation::PrepareOriginalFileObservation
                || prior.owner != envelope.owner
                || prior.accept.is_some()
                || pins.len() != if role.is_receive() { 2 } else { 1 }
                || !matches!(serde_json::from_slice::<Request>(&prior.body),
                    Ok(Request::PrepareOriginalFileObservation { call: c, role: r, .. }) if c == *call && r == *role && r.valid())
                || !matches!(serde_json::from_slice::<Reply>(body.ok_or_else(|| io::Error::other("auxiliary preparation unresolved"))?),
                    Ok(Reply::Prepared(ref p)) if p.status.returned == 0 && p.status.errno.is_none() && p.raw == *command)
            {
                return Err(io::Error::other(
                    "auxiliary collection changed retained preparation",
                ));
            }
            let pins = pins.iter().map(duplicate).collect::<io::Result<Vec<_>>>()?;
            let provider = &mut self.provider;
            session.dispatch(sequence, |envelope, rights| {
                provider.dispatch(envelope, rights, Some(&pins))
            })?;
            // Own every actual copy record in this same preparation before ACK
            // makes the helper's native command/ring rows reusable.
            if role.is_receive() && !session.read_copy_completed(sequence)? {
                let (_, _, body) = session.retained_request(sequence)?;
                let records = provider
                    .copy_for_completed(
                        body.ok_or_else(|| io::Error::other("helper receive completion missing"))?,
                    )?
                    .ok_or_else(|| {
                        io::Error::other("helper receive has no native copy manifest")
                    })?;
                session.retain_read_copy(sequence, records)?;
            }
            let ack = session.acknowledge_command_completion(sequence, |envelope, body| {
                provider.acknowledge_completed_command(envelope, body)
            })?;
            Provider::validate_command_acknowledgement(&ack)?;
            self.run_replies.push(sequence);
            return Ok(());
        }
        if let Request::RetireOriginalConnect {
            call,
            prepared,
            selected,
            completed,
            failed_request,
        } = &request
        {
            if envelope.operation != Operation::RetireOriginalConnect
                || !rights.is_empty()
                || envelope.owner.is_none()
                || envelope.accept.is_some()
                || sequence <= *completed
            {
                return Err(io::Error::other("original retirement envelope mismatch"));
            }
            let owner = envelope.owner.unwrap();
            session.retire_incoming_original(
                owner,
                *call,
                [*prepared, *selected, *completed],
                *failed_request,
            )?;
            session.dispatch(sequence, |_, _| {
                serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
            })?;
            self.run_replies.push(sequence);
            return Ok(());
        }
        if let Request::AwaitOriginalSelection {
            call,
            command,
            prepared_request,
        } = &request
        {
            if envelope.operation != Operation::AwaitOriginalSelection || !rights.is_empty() {
                return Err(io::Error::other("original selection envelope mismatch"));
            }
            // Validate the retained preparation before making this query live.
            original_preparation(session, envelope, *call, *command, *prepared_request)?;
            session.begin_observation(sequence)?;
            return Ok(());
        }
        if let Request::AwaitFdEvent {
            sequence: ordinal,
            acknowledged,
        } = &request
        {
            if envelope.operation != Operation::DrainFdJournal
                || !rights.is_empty()
                || envelope.owner.is_none()
                || envelope.accept.is_some()
                || *ordinal == 0
                || self.fd_observation.is_some()
            {
                return Err(io::Error::other("invalid FD observation envelope"));
            }
            if self.last_fd_observation.is_some()
                && acknowledged.as_ref().map(|r| r.sequence) != self.last_fd_observation
            {
                return Err(io::Error::other(
                    "FD observation skipped exact prior retirement",
                ));
            }
            if let Some(receipt) = acknowledged {
                if !receipt.fd_journal {
                    return Err(io::Error::other("FD retirement changed observation domain"));
                }
                session.retire_incoming_observation(receipt)?;
            }
            session.begin_observation(sequence)?;
            self.last_fd_observation = Some(sequence);
            self.fd_observation = Some((sequence, *ordinal, Instant::now()));
            return Ok(());
        }
        if let Request::RetireFdObservation { receipt } = &request {
            if envelope.operation != Operation::DrainFdJournal
                || !rights.is_empty()
                || envelope.owner.is_none()
                || envelope.accept.is_some()
                || !receipt.fd_journal
                || self.fd_observation.is_some()
                || self.last_fd_observation != Some(receipt.sequence)
            {
                return Err(io::Error::other("invalid final FD observation retirement"));
            }
            session.retire_incoming_observation(receipt)?;
            self.last_fd_observation = None;
            session.dispatch(sequence, |_, _| {
                serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
            })?;
            self.run_replies.push(sequence);
            return Ok(());
        }
        if matches!(
            request,
            Request::AwaitCreation { .. } | Request::RetireObservation { .. }
        ) {
            validate_observation(envelope, rights.len())?;
        }
        if let Request::AwaitCreation {
            sequence: creation,
            acknowledged,
        } = &request
        {
            if self.observation.is_some() || *creation == 0 {
                return Err(io::Error::other(
                    "creation observer already pending or has zero sequence",
                ));
            }
            if self.last_observation.is_some()
                && acknowledged.as_ref().map(|receipt| receipt.sequence) != self.last_observation
            {
                return Err(io::Error::other(
                    "next observation lacks prior exact retirement",
                ));
            }
            if let Some(receipt) = acknowledged {
                if receipt.fd_journal {
                    return Err(io::Error::other(
                        "creation retirement changed observation domain",
                    ));
                }
                session.retire_incoming_observation(receipt)?;
            }
            session.begin_observation(sequence)?;
            self.last_observation = Some(sequence);
            self.observation = Some(PendingObservation {
                request: sequence,
                creation: *creation,
                next_probe: Instant::now(),
            });
            return Ok(());
        }
        if let Request::RetireObservation { receipt } = &request {
            if receipt.fd_journal {
                return Err(io::Error::other(
                    "creation retirement changed observation domain",
                ));
            }
            if self
                .last_observation
                .is_some_and(|previous| previous != receipt.sequence)
            {
                return Err(io::Error::other(
                    "terminal acknowledgement skipped an observation",
                ));
            }
            session.retire_incoming_observation(receipt)?;
            self.last_observation = None;
            session.dispatch(sequence, |_, _| {
                serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
            })?;
            self.run_replies.push(sequence);
            return Ok(());
        }
        let terminal_query = if let Request::TerminateOriginalConnect {
            call,
            command,
            prepared_request,
            selected_request,
            failed_request,
        } = &request
        {
            if envelope.operation != Operation::TerminateOriginalConnect || !rights.is_empty() {
                return Err(io::Error::other(
                    "dead original retirement envelope mismatch",
                ));
            }
            let (query, pins, reply) = session.retained_request(*selected_request)?;
            if query.owner != envelope.owner
                || query.accept.is_some()
                || !pins.is_empty()
                || !matches!(serde_json::from_slice::<Request>(&query.body),Ok(Request::AwaitOriginalSelection {
                    call:c,command:k,prepared_request:p}) if c==*call && k==*command && p==*prepared_request)
            {
                return Err(io::Error::other(
                    "dead original retirement changed query custody",
                ));
            }
            if let Some(failed) = failed_request {
                let (prior, pins, outcome) = session.retained_request(*failed)?;
                if !(*selected_request < *failed && *failed < sequence)
                    || prior.owner != envelope.owner
                    || prior.accept.is_some()
                    || !pins.is_empty()
                    || prior.operation != Operation::CollectOriginalConnect
                    || !matches!(serde_json::from_slice::<Request>(&prior.body),Ok(Request::CollectOriginalConnect{call:c,command:k,prepared_request:p,..}) if c==*call && k==*command && p==*prepared_request)
                    || !matches!(outcome.map(serde_json::from_slice::<Reply>),Some(Ok(Reply::OriginalEffect(out))) if out.status.returned!=0)
                {
                    return Err(io::Error::other(
                        "dead original changed failed collection custody",
                    ));
                }
            }
            Some((*selected_request, *command, reply.is_none()))
        } else {
            None
        };
        let cancel_query = if let Request::CancelOriginalConnect {
            call,
            command,
            prepared_request,
            selected_request,
        } = &request
        {
            if envelope.operation != Operation::CancelOriginalConnect
                || !rights.is_empty()
                || !session
                    .pending_original_selections()
                    .contains(selected_request)
            {
                return Err(io::Error::other(
                    "known-uninvoked cancellation lacks its pending selection query",
                ));
            }
            let (query, pins, reply) = session.retained_request(*selected_request)?;
            if query.owner != envelope.owner
                || query.accept.is_some()
                || !pins.is_empty()
                || reply.is_some()
                || !matches!(serde_json::from_slice::<Request>(&query.body),Ok(Request::AwaitOriginalSelection {
                    call:c,command:k,prepared_request:p}) if c==*call && k==*command && p==*prepared_request)
            {
                return Err(io::Error::other(
                    "known-uninvoked cancellation changed selection query",
                ));
            }
            Some((*selected_request, *command))
        } else {
            None
        };
        let preparation = if let Request::ObserveNativeBirth {
            call,
            command,
            prepared_request,
            ..
        }
        | Request::CollectNativeBirth {
            call,
            command,
            prepared_request,
        }
        | Request::CancelNativeBirth {
            call,
            command,
            prepared_request,
        }
        | Request::TerminateNativeBirth {
            call,
            command,
            prepared_request,
        } = &request
        {
            let expected = if matches!(&request, Request::ObserveNativeBirth { .. }) {
                Operation::ObserveNativeBirth
            } else if matches!(&request, Request::TerminateNativeBirth { .. }) {
                Operation::TerminateNativeBirth
            } else if matches!(&request, Request::CancelNativeBirth { .. }) {
                Operation::CancelNativeBirth
            } else {
                Operation::CollectNativeBirth
            };
            if envelope.operation != expected
                || rights.len() != usize::from(expected == Operation::ObserveNativeBirth)
            {
                return Err(io::Error::other("birth envelope/rights mismatch"));
            }
            Some(command_preparation(
                session,
                envelope,
                *call,
                *command,
                *prepared_request,
                Operation::PrepareNativeBirth,
            )?)
        } else if let Request::CollectOriginalConnect {
            call,
            command,
            prepared_request,
            ..
        }
        | Request::CancelOriginalConnect {
            call,
            command,
            prepared_request,
            ..
        }
        | Request::TerminateOriginalConnect {
            call,
            command,
            prepared_request,
            ..
        } = &request
        {
            if !matches!(
                envelope.operation,
                Operation::CollectOriginalConnect
                    | Operation::CancelOriginalConnect
                    | Operation::TerminateOriginalConnect
            ) || !rights.is_empty()
            {
                return Err(io::Error::other("original completion envelope mismatch"));
            }
            Some(original_preparation(
                session,
                envelope,
                *call,
                *command,
                *prepared_request,
            )?)
        } else if let Request::FinishSetter {
            command,
            prepared_request,
        }
        | Request::CollectAccept {
            command,
            prepared_request,
        }
        | Request::CollectTableEnrollment {
            command,
            prepared_request,
        } = request
        {
            let (prior, pins, outcome) = session.retained_request(prepared_request)?;
            let expected = if envelope.operation == Operation::CollectTableEnrollment {
                Operation::PrepareTableEnrollment
            } else if envelope.operation == Operation::CollectAccept {
                Operation::PrepareAccept
            } else {
                Operation::PrepareSetter
            };
            if prior.operation != expected
                || prior.owner != envelope.owner
                || prior.accept != envelope.accept
                || pins.len()
                    != if expected == Operation::PrepareTableEnrollment {
                        1
                    } else {
                        2
                    }
            {
                return Err(io::Error::other(
                    "setter completion changed its retained owner/preparation",
                ));
            }
            let reply: Reply = serde_json::from_slice(
                outcome
                    .ok_or_else(|| io::Error::other("setter preparation has no known result"))?,
            )?;
            let Reply::Prepared(observation) = reply else {
                return Err(io::Error::other(
                    "setter completion names a different provider effect",
                ));
            };
            if observation.status.returned != 0 || observation.raw != command {
                return Err(io::Error::other(
                    "setter completion changed its exact provider command",
                ));
            }
            // These aliases can close after this synchronous dispatch because
            // their original owners remain in the run inbox throughout.
            Some(pins.iter().map(duplicate).collect::<io::Result<Vec<_>>>()?)
        } else {
            None
        };
        // The same preparation takes custody of every remaining record before
        // actual dead-task retirement may remove its native command. A partial
        // final DATA tail is diagnostic evidence, never a committed unit.
        if let Request::TerminateOriginalConnect {
            command,
            prepared_request,
            ..
        } = &request
        {
            let (prior, _, _) = session.retained_request(*prepared_request)?;
            let is_read = matches!(
                serde_json::from_slice::<Request>(&prior.body)?,
                Request::PrepareOriginalConnect {
                    kind: crate::network_replay::original_connect::Kind::Read,
                    ..
                }
            );
            if is_read && session.retained_request(sequence)?.2.is_none() {
                let pins = preparation
                    .as_ref()
                    .ok_or_else(|| io::Error::other("terminal Read target missing"))?;
                let (records, end) = self.provider.copy_for_terminal(pins[0].as_fd(), *command)?;
                session.retain_terminal_read_copy(*prepared_request, records, end)?;
            }
        }
        let provider = &mut self.provider;
        session.dispatch(sequence, |envelope, rights| {
            provider.dispatch(envelope, rights, preparation.as_deref())
        })?;
        if let Some((query, command)) = cancel_query {
            let (_, _, body) = session.retained_request(sequence)?;
            let body = body
                .ok_or_else(|| io::Error::other("cancellation reply missing"))?
                .to_vec();
            if matches!(serde_json::from_slice::<Reply>(&body),Ok(Reply::OriginalCanceled {command:k,status})
                if k==command && status.returned==0 && status.errno.is_none())
            {
                // This is the same positive C disarm, not a synthetic fdget
                // result. Both finite request owners retain that exact receipt.
                session.finish_observation(query, body)?;
                self.run_replies.push(query);
            }
        }
        if let Some((query, command, pending)) = terminal_query {
            let (_, _, body) = session.retained_request(sequence)?;
            let body = body
                .ok_or_else(|| io::Error::other("dead original retirement reply missing"))?
                .to_vec();
            if matches!(serde_json::from_slice::<Reply>(&body),Ok(Reply::OriginalTerminated(out))
                if out.status.returned==0 && out.status.errno.is_none() && out.raw.command.command==command && out.raw.task_absent==1)
                && pending
            {
                // Retain this typed death receipt in the existing unanswered
                // selection owner; never synthesize an empty fdget selection.
                session.finish_observation(query, body)?;
                self.run_replies.push(query);
            }
        }
        // Capture bytes into the existing completion owner before ACK can
        // recycle the provider slot. Each later wire reply stays below 16 KiB.
        let (stored, _, body) = session.retained_request(sequence)?;
        if stored.operation == Operation::CollectOriginalConnect {
            let body = body.ok_or_else(|| io::Error::other("original completion body missing"))?;
            let Request::CollectOriginalConnect { .. } = serde_json::from_slice(&stored.body)?
            else {
                return Err(io::Error::other("original completion request kind changed"));
            };
            // A lost/repeated reply reuses the retained result, even after its
            // positively completed provider ACK has made the slot reusable.
            if !session.read_copy_completed(sequence)?
                && let Some(records) = provider.copy_for_completed(body)? {
                    session.retain_read_copy(sequence, records)?;
                }
        }
        // dispatch has installed the complete primary response in the durable
        // inbox. Retain the separate ACK effect before making a provider slot
        // reusable; cancellation/lost transport cannot manufacture a new call.
        let (stored, _, _) = session.retained_request(sequence)?;
        if matches!(
            stored.operation,
            Operation::EnrollListener
                | Operation::MatchAccepted
                | Operation::FinishSetter
                | Operation::CollectAccept
                | Operation::CollectTableEnrollment
                | Operation::CollectOriginalConnect
                | Operation::ObserveTerminalSocket
                | Operation::CollectNativeBirth
        ) {
            let acknowledgement = session
                .acknowledge_command_completion(sequence, |envelope, body| {
                    provider.acknowledge_completed_command(envelope, body)
                })?;
            Provider::validate_command_acknowledgement(&acknowledgement)?;
        }
        self.run_replies.push(sequence);
        if cancel_query.is_some() || terminal_query.is_some() {
            // The selection cancellation must be sent before the final disarm
            // reply. Do not bypass/remove its queued reply with this fast path.
            return Ok(());
        }
        if session.try_reply(sequence)? {
            self.run_replies.remove(0);
        }
        Ok(())
    }

    /// After bootstrap the controller waits on the run session with no reply
    /// deadline of its own. End only that session's send direction so it
    /// records a sticky failure; custody is unchanged.
    pub(super) fn end_run_send_direction(&self) -> Option<Result<(), String>> {
        let session = self.run.as_ref().filter(|_| self.bootstrap_sent)?;
        Some(
            session
                .end_send_direction()
                .map_err(|error| error.to_string()),
        )
    }

    pub(super) fn run_peer_ended(&self) -> bool {
        self.run_peer_ended
    }

    pub(super) fn controller_has_exited(&self) -> io::Result<bool> {
        let Some(authorized) = &self.authorized else {
            return Ok(false); // A candidate pipe/file is never terminal authority.
        };
        let controller = self
            .controller
            .as_ref()
            .ok_or_else(|| io::Error::other("authorized controller custody missing"))?;
        if PidfdIdentity::read(controller)? != authorized.controller {
            return Err(io::Error::other("authorized controller identity changed"));
        }
        controller_exited(controller.as_fd())
    }

    pub(super) fn wait_transport(&self, deadline: Instant) -> io::Result<()> {
        match &self.run {
            Some(session) if self.bootstrap_sent => {
                session.wait_transport_with_copy(deadline, self.provider.copy_poll_fd()?)
            }
            _ => self.bootstrap.wait_transport(deadline),
        }
    }

    /// Map-backed provider completion has no transport readiness edge. Poll it
    /// promptly only while an authenticated transaction is outstanding; an
    /// idle provider retains the lower-frequency controller-liveness cadence.
    pub(super) fn maintenance_interval(&self) -> Duration {
        let active = self.bootstrap_reply.is_some_and(|_| !self.bootstrap_sent)
            || !self.run_replies.is_empty()
            || self.observation.is_some()
            || self.fd_observation.is_some()
            || self
                .run
                .as_ref()
                .is_some_and(|session| !session.pending_original_selections().is_empty());
        observation_maintenance(active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_replay::NetworkStreamOwner;
    use crate::network_replay::original_connect::Kind;
    use crate::network_runtime::PidfdIdentity;
    use crate::network_runtime::ProviderWireFormat;
    use crate::network_runtime::accepted_provider::AuxiliaryRole;
    use crate::network_runtime::accepted_provider::CallStatus;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider::ReceiveKind;

    struct CopyTask {
        pin: OwnedFd,
        stop: Option<std::sync::mpsc::Sender<()>>,
        join: Option<std::thread::JoinHandle<()>>,
    }
    impl CopyTask {
        fn new() -> Self {
            let (ready, task) = std::sync::mpsc::sync_channel(1);
            let (stop, stopped) = std::sync::mpsc::channel();
            let join = std::thread::spawn(move || {
                let raw = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_open,
                        libc::syscall(libc::SYS_gettid),
                        libc::O_EXCL,
                    )
                };
                assert!(raw >= 0, "{}", io::Error::last_os_error());
                ready
                    .send(unsafe { OwnedFd::from_raw_fd(raw as i32) })
                    .unwrap();
                let _ = stopped.recv_timeout(Duration::from_secs(30));
            });
            let pin = task.recv_timeout(Duration::from_secs(30)).unwrap();
            Self {
                pin,
                stop: Some(stop),
                join: Some(join),
            }
        }
        fn exit(&mut self) {
            self.stop.take();
            if let Some(join) = self.join.take() {
                join.join().unwrap();
                // pthread_join observes the clear-tid wake before the kernel
                // necessarily publishes PIDFD exit readiness. Establish the
                // actual death premise independently of controller_exited;
                // the test below must still check that production predicate.
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut descriptor = libc::pollfd {
                    fd: self.pin.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    assert!(!remaining.is_zero(), "test task PIDFD exit remains unknown");
                    let result = unsafe {
                        libc::poll(&mut descriptor, 1, remaining.as_millis().max(1) as i32)
                    };
                    if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
                    {
                        continue;
                    }
                    assert_eq!(result, 1, "test task PIDFD exit remains unknown");
                    assert_eq!(descriptor.revents & libc::POLLNVAL, 0);
                    assert_ne!(descriptor.revents & libc::POLLIN, 0);
                    break;
                }
            }
        }
    }
    impl Drop for CopyTask {
        fn drop(&mut self) {
            self.exit();
        }
    }
    struct CopyFixture {
        controller: AcceptedSession,
        service: AcceptedSession,
        owner: CopyTask,
        worker: CopyTask,
        copy: Envelope,
    }
    fn copy_status() -> CallStatus {
        CallStatus {
            operation: "simulated preparation".into(),
            returned: 0,
            errno: None,
        }
    }
    impl CopyFixture {
        fn new(wire: ProviderWireFormat, operation: u64) -> Self {
            let owner = CopyTask::new();
            let worker = CopyTask::new();
            assert_ne!(
                PidfdIdentity::read(&owner.pin).unwrap(),
                PidfdIdentity::read(&worker.pin).unwrap()
            );
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
            let mut controller =
                AcceptedSession::from_wire(unsafe { OwnedFd::from_raw_fd(pair[0]) }, [7; 16], wire)
                    .unwrap();
            let mut service =
                AcceptedSession::from_wire(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [7; 16], wire)
                    .unwrap();
            let thread = crate::types::DetTid::from_raw(31);
            let target = NetworkStreamOwner {
                thread,
                mm: crate::types::MmId::initial(thread),
            };
            let request = if operation == 11 {
                Request::PrepareOriginalConnect {
                    kind: Kind::Read,
                    call: 17,
                    mm: target.mm.generation(),
                    fd: 88,
                    address: 0x8000,
                    length: 0,
                    original_count: 1024,
                }
            } else {
                assert!(matches!(operation, 21 | 22));
                Request::PrepareOriginalFileObservation {
                    call: 17,
                    mm: target.mm.generation(),
                    fd: 88,
                    role: AuxiliaryRole::Receive {
                        kind: if operation == 21 {
                            ReceiveKind::Drain
                        } else {
                            ReceiveKind::Peek
                        },
                        address: 0x8000,
                        count: if operation == 21 { 1 } else { 1024 },
                        provider: 7,
                        file: 19,
                    },
                }
            };
            let prepare = Envelope {
                run: [7; 16],
                sequence: 1,
                owner: Some(target),
                accept: None,
                operation: if operation == 11 {
                    Operation::PrepareOriginalConnect
                } else {
                    Operation::PrepareOriginalFileObservation
                },
                body: serde_json::to_vec(&request).unwrap(),
            };
            let pins = if operation == 11 {
                vec![worker.pin.try_clone().unwrap()]
            } else {
                vec![
                    owner.pin.try_clone().unwrap(),
                    worker.pin.try_clone().unwrap(),
                ]
            };
            assert_eq!(controller.prepare(prepare.clone(), pins).unwrap(), 1);
            assert!(controller.try_send(1).unwrap());
            assert!(matches!(
                service.try_receive().unwrap(),
                Some(Received::Request(1))
            ));
            let copy = Envelope {
                sequence: 2,
                operation: Operation::ReadOriginalCopy,
                body: serde_json::to_vec(&Request::ReadOriginalCopy {
                    call: 17,
                    command: 91,
                    prepared: 1,
                    first: 0,
                })
                .unwrap(),
                ..prepare
            };
            Self {
                controller,
                service,
                owner,
                worker,
                copy,
            }
        }
        fn reply(&mut self, response: Reply) {
            // Simulate only the wire preparation premise. No physical provider,
            // completion, copy manifest, or native success receipt is produced.
            self.service
                .dispatch(1, |_, _| {
                    serde_json::to_vec(&response).map_err(io::Error::other)
                })
                .unwrap();
            assert!(self.service.try_reply(1).unwrap());
            assert!(matches!(
                self.controller.try_receive().unwrap(),
                Some(Received::Acknowledged(1))
            ));
        }
        fn acknowledge(&mut self) {
            self.reply(Reply::Prepared(Observation {
                status: copy_status(),
                raw: 91,
            }));
        }
        fn custody(&self) -> Vec<(i32, PidfdIdentity)> {
            self.service
                .retained_request(1)
                .unwrap()
                .1
                .iter()
                .map(|pin| {
                    assert!(unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFD) } >= 0);
                    (pin.as_raw_fd(), PidfdIdentity::read(pin).unwrap())
                })
                .collect()
        }
    }

    #[test]
    fn copy_preparation_selects_command_worker_and_preserves_ordered_custody() {
        for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            for operation in [11, 21, 22] {
                let mut fixture = CopyFixture::new(wire, operation);
                fixture.acknowledge();
                let worker = PidfdIdentity::read(&fixture.worker.pin).unwrap();
                let owner = PidfdIdentity::read(&fixture.owner.pin).unwrap();
                assert_ne!(owner, worker);
                let custody = fixture.custody();
                let identities: Vec<_> = custody.iter().map(|(_, identity)| *identity).collect();
                assert_eq!(
                    identities,
                    if operation == 11 {
                        vec![worker]
                    } else {
                        vec![owner, worker]
                    }
                );
                let selected =
                    copy_preparation(&fixture.service, &fixture.copy, 17, 91, 1).unwrap();
                assert_eq!(PidfdIdentity::read(&selected[0]).unwrap(), worker);
                assert_eq!(selected.len(), 1);
                assert!(!custody.iter().any(|(fd, _)| *fd == selected[0].as_raw_fd()));
                assert_eq!(fixture.custody(), custody);
                drop(selected);
                assert_eq!(fixture.custody(), custody);
            }
        }
    }

    #[test]
    fn copy_preparation_liveness_follows_worker_for_both_owner_death_orders() {
        for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            for operation in [21, 22] {
                for worker_dead in [false, true] {
                    let mut fixture = CopyFixture::new(wire, operation);
                    fixture.acknowledge();
                    let custody = fixture.custody();
                    if worker_dead {
                        fixture.worker.exit();
                    } else {
                        fixture.owner.exit();
                    }
                    assert_eq!(
                        controller_exited(fixture.owner.pin.as_fd()).unwrap(),
                        !worker_dead
                    );
                    assert_eq!(
                        controller_exited(fixture.worker.pin.as_fd()).unwrap(),
                        worker_dead
                    );
                    let worker = PidfdIdentity::read(&fixture.worker.pin).unwrap();
                    // A native-operation double at the production selection
                    // seam observes actual PIDFD liveness, not a canned result.
                    let mut probed = None;
                    let mut terminal_probe = |pin: &OwnedFd| {
                        probed = Some(PidfdIdentity::read(pin).unwrap());
                        controller_exited(pin.as_fd()).unwrap()
                    };
                    let selected =
                        copy_preparation(&fixture.service, &fixture.copy, 17, 91, 1).unwrap();
                    assert_eq!(terminal_probe(&selected[0]), worker_dead);
                    assert_eq!(probed, Some(worker));
                    drop(selected);
                    assert_eq!(fixture.custody(), custody);
                }
            }
        }
    }

    #[test]
    fn copy_preparation_rejects_changed_identity_role_arity_and_unresolved_preparation() {
        for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            for operation in [11, 21, 22] {
                for case in 0..17 {
                    let mut fixture = CopyFixture::new(wire, operation);
                    match case {
                        6 => {} // admitted but not dispatched/acknowledged
                        7 => {
                            assert!(
                                fixture
                                    .service
                                    .dispatch(1, |_, _| Err(io::Error::other(
                                        "unknown preparation"
                                    )))
                                    .is_err()
                            );
                        }
                        8 | 9 => fixture.reply(Reply::Prepared(Observation {
                            status: CallStatus {
                                returned: if case == 8 { -1 } else { 0 },
                                errno: Some(libc::EIO),
                                ..copy_status()
                            },
                            raw: 91,
                        })),
                        10 => fixture.reply(Reply::Retired),
                        16 => fixture.reply(Reply::Prepared(Observation {
                            status: copy_status(),
                            raw: 92,
                        })),
                        _ => fixture.acknowledge(),
                    }
                    let mut call = 17;
                    let mut command = 91;
                    let mut prepared = 1;
                    match case {
                        0 => {
                            let owner = fixture.copy.owner.as_mut().unwrap();
                            owner.mm = owner.mm.for_exec(owner.thread);
                        }
                        1 => fixture.copy.owner = None,
                        2 => {
                            fixture.copy.accept =
                                Some(crate::network_replay::NetworkAcceptLeaseId(1))
                        }
                        3 => call += 1,
                        4 => command += 1,
                        5 => prepared += 10,
                        11 => fixture
                            .service
                            .mutate_retained_request_for_test(1, |prior, pins| {
                                if operation == 11 {
                                    let mut request: Request =
                                        serde_json::from_slice(&prior.body).unwrap();
                                    let Request::PrepareOriginalConnect { kind, .. } = &mut request
                                    else {
                                        unreachable!()
                                    };
                                    *kind = Kind::Connect;
                                    prior.body = serde_json::to_vec(&request).unwrap();
                                } else {
                                    let mut request: Request =
                                        serde_json::from_slice(&prior.body).unwrap();
                                    let Request::PrepareOriginalFileObservation { role, .. } =
                                        &mut request
                                    else {
                                        unreachable!()
                                    };
                                    *role = AuxiliaryRole::File;
                                    prior.body = serde_json::to_vec(&request).unwrap();
                                    pins.remove(0); // valid File arity, still no receive authority
                                }
                            }),
                        12 => fixture
                            .service
                            .mutate_retained_request_for_test(1, |_, pins| {
                                pins.pop();
                            }),
                        13 => fixture
                            .service
                            .mutate_retained_request_for_test(1, |_, pins| {
                                pins.push(pins.last().unwrap().try_clone().unwrap());
                            }),
                        14 => fixture
                            .service
                            .mutate_retained_request_for_test(1, |prior, _| {
                                let mut request: Request =
                                    serde_json::from_slice(&prior.body).unwrap();
                                match &mut request {
                                    Request::PrepareOriginalConnect { call, .. }
                                    | Request::PrepareOriginalFileObservation { call, .. } => {
                                        *call += 1
                                    }
                                    _ => unreachable!(),
                                }
                                prior.body = serde_json::to_vec(&request).unwrap();
                            }),
                        15 => fixture
                            .service
                            .mutate_retained_request_for_test(1, |prior, _| {
                                prior.operation = Operation::PrepareNativeBirth
                            }),
                        _ => {}
                    }
                    let custody = fixture.custody();
                    assert!(
                        copy_preparation(&fixture.service, &fixture.copy, call, command, prepared)
                            .is_err(),
                        "case {case}, operation {operation}"
                    );
                    assert_eq!(fixture.custody(), custody);
                }
            }
        }
    }

    #[test]
    fn provider_maintenance_is_fast_only_for_active_transactions() {
        assert_eq!(observation_maintenance(false), OBSERVATION_MAINTENANCE);
        assert_eq!(
            observation_maintenance(true),
            ACTIVE_OBSERVATION_MAINTENANCE
        );
        assert!(ACTIVE_OBSERVATION_MAINTENANCE < OBSERVATION_MAINTENANCE);
    }

    #[test]
    fn run_peer_end_of_stream_stops_receipt_without_failure_but_invalid_packet_fails() {
        let pair = || {
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
            unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) }
        };
        let (peer, local) = pair();
        let peer = AcceptedSession::new(peer, [7; 16]).unwrap();
        let mut local = AcceptedSession::new(local, [7; 16]).unwrap();
        let mut ended = false;
        // A live idle peer is neither end-of-stream nor failure.
        assert!(run_receive(&mut local, &mut ended).unwrap().is_none());
        assert!(!ended);
        peer.end_send_direction().unwrap();
        assert!(run_receive(&mut local, &mut ended).unwrap().is_none());
        assert!(ended);
        let custody = local.terminal_custody();
        assert_eq!(
            (custody.incoming_unfinished, custody.quarantined_messages),
            (0, 0)
        );
        // A nonempty invalid packet is still a sticky transport failure.
        let (raw, local) = pair();
        let mut local = AcceptedSession::new(local, [7; 16]).unwrap();
        assert_eq!(
            unsafe { libc::send(raw.as_raw_fd(), b"x".as_ptr().cast(), 1, 0) },
            1
        );
        let mut ended = false;
        assert!(run_receive(&mut local, &mut ended).is_err());
        assert!(!ended);
        assert_eq!(local.terminal_custody().quarantined_messages, 1);
    }
    #[test]
    fn accepted_observer_service_maintenance_is_bounded_without_completing_pending_request() {
        let now = Instant::now();
        let mut pending = PendingObservation {
            request: 7,
            creation: 3,
            next_probe: now,
        };
        let mut probes = 0;
        assert!(
            pending
                .probe(now, |ordinal| {
                    assert_eq!(ordinal, 3);
                    probes += 1;
                    Ok(None)
                })
                .unwrap()
                .is_none()
        );
        for _ in 0..512 {
            assert!(
                pending
                    .probe(now, |_| panic!("busy probe before maintenance wait"))
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(probes, 1);
        assert_eq!(pending.request, 7);
        assert_eq!(pending.creation, 3);
        assert_eq!(
            pending
                .probe(now + OBSERVATION_MAINTENANCE, |_| {
                    probes += 1;
                    Ok(Some(b"queued".to_vec()))
                })
                .unwrap(),
            Some(b"queued".to_vec())
        );
        assert_eq!(probes, 2);
    }
    #[test]
    fn accepted_observer_service_validates_envelope_before_any_retirement() {
        let tid = crate::types::DetTid::from_raw(11);
        let owner = crate::network_replay::NetworkStreamOwner {
            thread: tid,
            mm: crate::types::MmId::initial(tid),
        };
        let valid = Envelope {
            run: [1; 16],
            sequence: 3,
            owner: Some(owner),
            accept: None,
            operation: Operation::DrainCreations,
            body: vec![],
        };
        validate_observation(&valid, 0).unwrap();
        for case in 0..4 {
            let mut invalid = valid.clone();
            let mut rights = 0;
            match case {
                0 => invalid.operation = Operation::EnrollListener,
                1 => rights = 2,
                2 => invalid.owner = None,
                _ => invalid.accept = Some(crate::network_replay::NetworkAcceptLeaseId(1)),
            };
            assert!(validate_observation(&invalid, rights).is_err());
        }
    }
}
