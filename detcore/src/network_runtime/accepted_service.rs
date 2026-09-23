//! Accepted-only outside-provider service loop. The parent-owned unit transports
//! the private startup endpoint on stdin; all socket rights stay in durable
//! session inboxes across callbacks, errors, lost replies and controller exit.

mod process;
use std::ffi::CString;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

pub use process::run_accepted_provider_process;

use super::accepted_parent::ProviderArtifact;
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
    bootstrap: AcceptedSession,
    run: Option<AcceptedSession>,
    controller: Option<OwnedFd>,
    provider: Provider,
    incarnation: [u8; 16],
    library: CString,
    object: CString,
    bootstrap_reply: Option<u64>,
    bootstrap_sent: bool,
    run_replies: Vec<u64>,
    failure: Option<String>,
    observation: Option<PendingObservation>,
    last_observation: Option<u64>,
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
    ) -> Result<Self, (io::Error, OwnedFd)> {
        let bootstrap = AcceptedSession::new(stdin, run)?;
        Ok(Self {
            bootstrap,
            run: None,
            controller: None,
            provider: Provider::empty(),
            incarnation: run,
            library,
            object,
            bootstrap_reply: None,
            bootstrap_sent: false,
            run_replies: Vec::new(),
            failure: None,
            observation: None,
            last_observation: None,
        })
    }

    /// Error borrows rather than consumes this owner. The caller must continue
    /// recovery with its retained controller/endpoint/provider capabilities.
    pub(super) fn step(&mut self) -> io::Result<()> {
        if let Some(error) = &self.failure {
            return Err(io::Error::other(error.clone()));
        }
        let outcome = self.step_inner();
        if let Err(error) = &outcome {
            self.failure = Some(error.to_string());
        }
        outcome
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
            let run = self.incarnation;
            let provider = &mut self.provider;
            let controller = &mut self.controller;
            let run_session = &mut self.run;
            let library = &self.library;
            let object = &self.object;
            self.bootstrap.dispatch(sequence, |envelope, rights| {
                if envelope.operation != Operation::Bootstrap
                    || envelope.owner.is_some()
                    || envelope.accept.is_some()
                    || rights.len() != 2
                    || sequence != 1
                {
                    return Err(io::Error::other("invalid accepted service bootstrap"));
                }
                let expected: ProviderArtifact = serde_json::from_slice(&envelope.body)?;
                // Original rights are already durable in bootstrap.incoming.
                // These retained aliases establish the long-lived service path.
                *controller = Some(duplicate(&rights[0])?);
                *run_session = Some(
                    AcceptedSession::new(duplicate(&rights[1])?, run)
                        .map_err(|(error, _)| error)?,
                );
                if controller_exited(controller.as_ref().unwrap().as_fd())? {
                    return Err(io::Error::other(
                        "controller exited before provider startup",
                    ));
                }
                // SAFETY: from_private_stdin's owning launcher authenticated
                // immutable artifacts/dependencies before this service existed.
                let ready = unsafe { provider.open(library, object, run, &expected) }?;
                serde_json::to_vec(&ready).map_err(io::Error::other)
            })?;
            self.bootstrap_reply = Some(sequence);
            self.bootstrap_sent = self.bootstrap.try_reply(sequence)?;
            return Ok(());
        }
        let session = self.run.as_mut().unwrap();
        if let Some(pending) = &mut self.observation {
            if let Some(body) = pending.probe(Instant::now(), |ordinal| {
                self.provider.poll_creation(ordinal)
            })? {
                session.finish_observation(pending.request, body)?;
                self.run_replies.push(pending.request);
                self.observation = None;
            }
        }

        while let Some(sequence) = self.run_replies.first().copied() {
            if !session.try_reply(sequence)? {
                return Ok(());
            }
            self.run_replies.remove(0);
        }
        let Some(received) = session.try_receive()? else {
            return Ok(());
        };
        let Received::Request(sequence) = received else {
            return Err(io::Error::other(
                "unexpected acknowledgement at provider service",
            ));
        };
        let (envelope, rights, _) = session.retained_request(sequence)?;
        let request: Request = serde_json::from_slice(&envelope.body)?;
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
        let preparation = if let Request::FinishSetter {
            command,
            prepared_request,
        } = request
        {
            let (prior, pins, outcome) = session.retained_request(prepared_request)?;
            if prior.operation != Operation::PrepareSetter
                || prior.owner != envelope.owner
                || prior.accept != envelope.accept
                || pins.len() != 2
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
            Some(vec![duplicate(&pins[0])?, duplicate(&pins[1])?])
        } else {
            None
        };
        let provider = &mut self.provider;
        session.dispatch(sequence, |envelope, rights| {
            provider.dispatch(envelope, rights, preparation.as_deref())
        })?;
        // dispatch has installed the complete primary response in the durable
        // inbox. Retain the separate ACK effect before making a provider slot
        // reusable; cancellation/lost transport cannot manufacture a new call.
        let (stored, _, _) = session.retained_request(sequence)?;
        if matches!(
            stored.operation,
            Operation::EnrollListener | Operation::MatchAccepted | Operation::FinishSetter
        ) {
            let acknowledgement = session
                .acknowledge_command_completion(sequence, |envelope, body| {
                    provider.acknowledge_completed_command(envelope, body)
                })?;
            Provider::validate_command_acknowledgement(&acknowledgement)?;
        }
        self.run_replies.push(sequence);
        if session.try_reply(sequence)? {
            self.run_replies.remove(0);
        }
        Ok(())
    }

    pub(super) fn controller_has_exited(&self) -> io::Result<bool> {
        self.controller.as_ref().map_or(Ok(false), |controller| {
            controller_exited(controller.as_fd())
        })
    }

    pub(super) fn wait_transport(&self, deadline: Instant) -> io::Result<()> {
        match &self.run {
            Some(session) if self.bootstrap_sent => session.wait_transport(deadline),
            _ => self.bootstrap.wait_transport(deadline),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
