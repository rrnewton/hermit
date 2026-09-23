//! Run-owned controller requests. Futures borrow this owner; submission, SCM
//! capabilities and responses remain here if a caller disappears mid-request.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

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
    Listener(OpenFileId),
    Match(NetworkAcceptLeaseId),
    PrepareSetter(NetworkStreamLeaseId),
    FinishSetter(NetworkStreamLeaseId),
    // Observation cuts get a run-owned identity, not a guest TID/syscall order.
    Observation(u64),
    ObservationRetirement(u64),
}

#[derive(Debug)]
struct Submitted {
    owner: NetworkStreamOwner,
    operation: Operation,
    body: Vec<u8>,
    sequence: Option<u64>,
    error: Option<String>,
}
#[derive(Debug, Default)]
struct Requests(BTreeMap<Effect, Submitted>, u64);
impl Requests {
    fn retire_observation(&mut self, key: Effect, sequence: u64) -> io::Result<()> {
        let Effect::Observation(id) = key else {
            return Err(io::Error::other("not a read-only observation"));
        };
        let entry = self
            .0
            .get(&key)
            .ok_or_else(|| io::Error::other("unknown observation effect"))?;
        if id <= self.1
            || entry.sequence != Some(sequence)
            || entry.operation != Operation::DrainCreations
        {
            return Err(io::Error::other("observation retirement identity changed"));
        }
        self.0.remove(&key);
        self.1 = id;
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
        if matches!(key, Effect::Observation(id) if id <= self.1) {
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
        self.0.insert(
            key,
            Submitted {
                owner,
                operation,
                body: body.to_vec(),
                sequence: None,
                error: None,
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
    ready: AsyncFd<OwnedFd>,
    run: [u8; 16],
    changed: tokio::sync::Notify,
}
impl Controller {
    pub(super) fn new(endpoint: OwnedFd, run: [u8; 16]) -> io::Result<Self> {
        // The runtime retains its original endpoint through this setup. Closing
        // an unused duplicate on failure cannot release the original channel.
        let readiness = endpoint.as_fd().try_clone_to_owned()?;
        let ready = AsyncFd::new(readiness)?;
        let session = AcceptedSession::new(endpoint, run).map_err(|(error, _)| error)?;
        Ok(Self {
            state: Mutex::new(State {
                session,
                requests: Requests::default(),
                pending_send: VecDeque::new(),
                rejected_rights: Vec::new(),
                failure: None,
            }),
            ready,
            run,
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
            Request::Enroll { .. } => Operation::EnrollListener,
            Request::ReadStatus
            | Request::ReadCreation { .. }
            | Request::AwaitCreation { .. }
            | Request::RetireObservation { .. } => Operation::DrainCreations,
            Request::PrepareSetter { .. } => Operation::PrepareSetter,
            Request::FinishSetter { .. } => Operation::FinishSetter,
            Request::ResolveAccepted => Operation::MatchAccepted,
        };
        let accept = match key {
            Effect::Match(lease) => Some(lease),
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
        requests.prepare(key, owner, operation, &body, || {
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
        })
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

    /// Transport waits do not publish guest time or select a guest wakeup.
    /// The RPC caller must separately revalidate owner/MM and semantic leases
    /// before applying a provider result to the engine.
    pub(super) async fn response(&self, sequence: u64) -> io::Result<Reply> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let interest = {
                let mut state = self.state.lock().unwrap();
                if let Some(error) = &state.failure {
                    return Err(io::Error::other(error.clone()));
                }
                let outcome = Self::progress(&mut state, sequence, &self.changed);
                match outcome {
                    Ok(Some(reply)) => return Ok(reply),
                    Ok(None) => {
                        if state.pending_send.is_empty() {
                            Interest::READABLE
                        } else {
                            Interest::READABLE | Interest::WRITABLE
                        }
                    }
                    Err(error) => {
                        state.failure = Some(error.to_string());
                        return Err(error);
                    }
                }
            };
            // No semantic lease or blocking std::Mutex guard spans this await.
            // A sibling may drive the same pending queue after cancellation.
            tokio::select! {
                _ = changed => {},
                ready = self.ready.ready(interest) => {ready?.clear_ready();}
            }
        }
    }

    fn progress(
        state: &mut State,
        sequence: u64,
        changed: &tokio::sync::Notify,
    ) -> io::Result<Option<Reply>> {
        if let Some(bytes) = state.session.response(sequence)? {
            return serde_json::from_slice(bytes)
                .map(Some)
                .map_err(io::Error::other);
        }
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
        state
            .session
            .response(sequence)?
            .map(serde_json::from_slice)
            .transpose()
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
