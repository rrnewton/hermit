//! Persistent accept-result custody, separate from semantic descriptor ownership.
//!
//! The entry precedes injection. Its physical result and owned descriptor survive
//! provider errors, lost replies and cancellation of the adapter future.

use std::collections::BTreeMap;
use std::io;

use crate::network_replay::NetworkAcceptLeaseId;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::accepted::AcceptedPhysicalIdentity;

/// Matched physical object and creation occurrence. This is not an installed
/// descriptor fact: exact FD-slot generation still comes from table authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub physical: AcceptedPhysicalIdentity,
    pub creation: u64,
    pub cookie: u64,
}
impl Resolved {
    pub(super) fn checked(
        expected_provider: u64,
        observation: super::accepted_provider::Observation<super::accepted_provider::CommandResult>,
    ) -> io::Result<Self> {
        let raw = observation.raw;
        if observation.status.returned != 0
            || raw.returned != 0
            || raw.phase != 1
            || raw.operation != 2
            || raw.command == 0
            || raw.task == 0
            || raw.start_boottime == 0
            || raw.creation == 0
            || raw.cookie == 0
            || raw.reserved != 0
            || expected_provider == 0
            || raw.identity.provider != expected_provider
            || raw.identity.object == 0
            || raw.identity.namespace == 0
        {
            return Err(invalid(
                "accepted provider match is partial, failed or from another lifetime",
            ));
        }
        Ok(Self {
            physical: AcceptedPhysicalIdentity {
                provider: raw.identity.provider,
                object: raw.identity.object,
                namespace: raw.identity.namespace,
            },
            creation: raw.creation,
            cookie: raw.cookie,
        })
    }
}

#[derive(Debug)]
struct Accept<T> {
    owner: NetworkStreamOwner,
    listener_call: NetworkStreamCallId,
    returned: Option<Result<i32, i32>>,
    pin: Option<T>,
    capture_error: Option<String>,
    matched: Option<AcceptedPhysicalIdentity>,
    resolved: Option<Resolved>,
    abandoned: bool,
    prepared_effect: Option<(u64, u64)>,
    physical_effect:
        Option<super::accepted_provider::Observation<super::accepted_provider::AcceptedEffect>>,
    collection: Option<Result<(u64, Option<Result<(), String>>), String>>,
}

#[derive(Debug)]
pub(super) struct AcceptedCustody<T> {
    operations: BTreeMap<NetworkAcceptLeaseId, Accept<T>>,
}

impl<T> Default for AcceptedCustody<T> {
    fn default() -> Self {
        Self {
            operations: BTreeMap::new(),
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}

impl<T> AcceptedCustody<T> {
    #[cfg(test)]
    pub(super) fn collection_effect_status(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Option<(i32, i32)>> {
        Ok(self
            .receipt(owner, lease)?
            .physical_effect
            .as_ref()
            .map(|effect| (effect.status.returned, effect.raw.command.returned)))
    }
    pub(super) fn captured_return_matches(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        result: Result<i32, i32>,
    ) -> bool {
        self.receipt(owner, lease)
            .is_ok_and(|operation| operation.returned == Some(result))
    }

    pub(super) fn collection_result(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<()> {
        match &self.receipt(owner, lease)?.collection {
            Some(Ok((_, Some(result)))) => result.clone().map_err(io::Error::other),
            Some(Err(error)) => Err(io::Error::other(error.clone())),
            _ => Err(invalid("accepted collection remains unresolved")),
        }
    }
    #[cfg(test)]
    pub(super) fn recovery_result(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Option<Result<i32, i32>>> {
        self.operations
            .get(&lease)
            .filter(|op| op.owner == owner)
            .map(|op| op.returned)
            .ok_or_else(|| invalid("wrong recovery owner"))
    }
    pub(super) fn submit(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        listener_call: NetworkStreamCallId,
    ) -> io::Result<()> {
        if self.operations.contains_key(&lease) {
            return Err(invalid("accept custody submission reused"));
        }
        self.operations.insert(
            lease,
            Accept {
                owner,
                listener_call,
                returned: None,
                pin: None,
                capture_error: None,
                matched: None,
                resolved: None,
                abandoned: false,
                prepared_effect: None,
                physical_effect: None,
                collection: None,
            },
        );
        Ok(())
    }

    fn operation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<&Accept<T>> {
        self.operations
            .get(&lease)
            .filter(|op| op.owner == owner && !op.abandoned)
            .ok_or_else(|| invalid("unknown or retired accept custody owner"))
    }

    fn receipt(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<&Accept<T>> {
        self.operations
            .get(&lease)
            .filter(|op| op.owner == owner)
            .ok_or_else(|| invalid("unknown accept custody receipt owner"))
    }

    /// Latch first, acquire once, retain before any validation. No async boundary
    /// occurs in this method. A failed acquisition is not retried against a reused
    /// numeric FD and an errno never establishes that no child was dequeued.
    pub(super) fn capture(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        result: Result<i32, i32>,
        acquire: impl FnOnce(i32) -> io::Result<T>,
        validate_pin: impl FnOnce(&T) -> io::Result<()>,
    ) -> io::Result<()> {
        // Abandonment revokes publication and acquisition, not the authority to
        // report the return of this original submitted operation.
        let old = self.receipt(owner, lease)?;
        if let Some(prior) = old.returned {
            if prior != result {
                return Err(invalid("accept result changed after capture"));
            }
            return old
                .capture_error
                .as_ref()
                .map_or(Ok(()), |e| Err(io::Error::other(e.clone())));
        }
        if result.is_ok_and(|fd| fd < 0) || result.is_err_and(|e| !(1..=4095).contains(&e)) {
            return Err(invalid("invalid accept kernel result"));
        }
        let op = self.operations.get_mut(&lease).unwrap();
        op.returned = Some(result);
        let Ok(fd) = result else { return Ok(()) };
        if op.abandoned {
            let error =
                invalid("retired accept retains its result without reacquiring a descriptor");
            op.capture_error = Some(error.to_string());
            return Err(error);
        }
        match acquire(fd) {
            Ok(pin) => op.pin = Some(pin),
            Err(error) => {
                op.capture_error = Some(error.to_string());
                return Err(error);
            }
        }
        // Ownership is in the run's recovery object even if this check fails.
        if let Err(error) = validate_pin(op.pin.as_ref().unwrap()) {
            op.capture_error = Some(error.to_string());
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn prepared_effect(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<(u64, u64)> {
        self.receipt(owner, lease)?
            .prepared_effect
            .ok_or_else(|| invalid("accept provider command not prepared"))
    }

    pub(super) fn collection_submission(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Option<Result<u64, String>>> {
        let operation = self.receipt(owner, lease)?;
        if operation.returned.is_none() {
            return Err(invalid("collection precedes captured kernel return"));
        }
        Ok(operation.collection.as_ref().map(|result| {
            result
                .as_ref()
                .map(|(sequence, _)| *sequence)
                .map_err(Clone::clone)
        }))
    }

    pub(super) fn retain_collection_submission(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        result: Result<u64, String>,
    ) -> io::Result<()> {
        if self.collection_submission(owner, lease)?.is_some() {
            return Err(invalid("accepted collection submission already retained"));
        }
        self.operations.get_mut(&lease).unwrap().collection =
            Some(result.map(|sequence| (sequence, None)));
        Ok(())
    }

    pub(super) fn pending_collections(
        &self,
    ) -> Vec<(NetworkStreamOwner, NetworkAcceptLeaseId, u64)> {
        self.operations
            .iter()
            .filter_map(|(lease, operation)| match &operation.collection {
                Some(Ok((sequence, None))) => Some((operation.owner, *lease, *sequence)),
                _ => None,
            })
            .collect()
    }

    /// Store the complete provider reply/error independently of a live caller.
    /// A failed collection remains failed and cannot trigger another dispatch.
    pub(super) fn complete_collection(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        sequence: u64,
        reply: Result<super::accepted_provider::Reply, String>,
    ) -> io::Result<()> {
        match &self.receipt(owner, lease)?.collection {
            Some(Ok((actual, None))) if *actual == sequence => {}
            Some(Ok((actual, Some(_)))) if *actual == sequence => return Ok(()),
            _ => return Err(invalid("accepted collection completion changed request")),
        }
        let outcome = match reply {
            Ok(super::accepted_provider::Reply::AcceptedEffect(effect)) => self
                .retain_physical_effect(owner, lease, effect)
                .map_err(|error| error.to_string()),
            Ok(_) => Err(
                "accepted collection returned a different effect; reply retained in transport"
                    .into(),
            ),
            Err(error) => Err(error),
        };
        self.operations.get_mut(&lease).unwrap().collection = Some(Ok((sequence, Some(outcome))));
        Ok(())
    }

    pub(super) fn collections_settled(&self) -> io::Result<bool> {
        let mut pending = false;
        for operation in self.operations.values() {
            match &operation.collection {
                None if operation.returned.is_some() || operation.prepared_effect.is_some() => {
                    return Err(invalid(
                        "prepared or captured accept has no collection receipt",
                    ));
                }
                Some(Ok((_, None))) => pending = true,
                Some(Err(error)) | Some(Ok((_, Some(Err(error))))) => {
                    return Err(io::Error::other(error.clone()));
                }
                _ => {}
            }
        }
        Ok(!pending)
    }
    pub(super) fn retain_preparation(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        request: u64,
        command: u64,
    ) -> io::Result<()> {
        let prior = self.receipt(owner, lease)?;
        if request == 0
            || command == 0
            || prior
                .prepared_effect
                .is_some_and(|p| p != (request, command))
        {
            return Err(invalid("accepted provider preparation changed"));
        }
        self.operations.get_mut(&lease).unwrap().prepared_effect = Some((request, command));
        Ok(())
    }
    /// Recovery stores the provider's partial/error response even after owner
    /// abandonment. This does not grant permission to install or recapture.
    pub(super) fn retain_physical_effect(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        effect: super::accepted_provider::Observation<super::accepted_provider::AcceptedEffect>,
    ) -> io::Result<()> {
        let prior = self.receipt(owner, lease)?;
        if prior.physical_effect.as_ref().is_some_and(|p| p != &effect) {
            return Err(invalid("accepted physical effect changed after receipt"));
        }
        self.operations.get_mut(&lease).unwrap().physical_effect = Some(effect.clone());
        let prior = self.receipt(owner, lease)?;
        let (_, command) = prior
            .prepared_effect
            .ok_or_else(|| invalid("accepted physical receipt lacks preparation"))?;
        if effect.status.returned != 0 {
            return Err(invalid(
                "accepted provider collection failed; partial receipt retained",
            ));
        }
        let raw = &effect.raw;
        let returned = if raw.command.returned >= 0 {
            Ok(raw.command.returned)
        } else {
            Err(-raw.command.returned)
        };
        if raw.command.command != command
            || raw.command.operation != 4
            || raw.command.phase != 1
            || raw.installation.command != command
            || raw.installation.accept_lease != lease.0
            || raw.installation.owner_mm != owner.mm.generation()
            || prior.returned != Some(returned)
        {
            return Err(invalid(
                "accepted provider effect differs from exact submitted kernel return",
            ));
        }
        Ok(())
    }

    pub(super) fn pin(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<&T> {
        let op = self.operation(owner, lease)?;
        if op.capture_error.is_some() {
            return Err(invalid("accept capture remains unresolved"));
        }
        op.pin
            .as_ref()
            .ok_or_else(|| invalid("accept has no confirmed owned descriptor"))
    }

    pub(super) fn match_pin(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        inspect: impl FnOnce(&T) -> io::Result<AcceptedPhysicalIdentity>,
    ) -> io::Result<AcceptedPhysicalIdentity> {
        let op = self.operation(owner, lease)?;
        if let Some(identity) = op.matched {
            return Ok(identity);
        }
        let identity = inspect(self.pin(owner, lease)?)?;
        if identity.provider == 0 || identity.object == 0 || identity.namespace == 0 {
            return Err(invalid("provider returned invalid accepted identity"));
        }
        self.operations.get_mut(&lease).unwrap().matched = Some(identity);
        Ok(identity)
    }

    pub(super) fn confirm_resolved(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        resolved: Resolved,
    ) -> io::Result<Resolved> {
        let prior = self.operation(owner, lease)?;
        self.pin(owner, lease)?;
        if prior.resolved.is_some_and(|old| old != resolved)
            || prior.matched.is_some_and(|old| old != resolved.physical)
        {
            return Err(invalid(
                "accepted provider match changed its retained identity",
            ));
        }
        let op = self.operations.get_mut(&lease).unwrap();
        op.matched = Some(resolved.physical);
        op.resolved = Some(resolved);
        Ok(resolved)
    }

    pub(super) fn abandon(&mut self, owner: NetworkStreamOwner) {
        for op in self.operations.values_mut().filter(|op| op.owner == owner) {
            op.abandoned = true;
        }
    }

    pub(super) fn listener_call(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<NetworkStreamCallId> {
        Ok(self.operation(owner, lease)?.listener_call)
    }

    pub(super) fn returned(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Option<Result<i32, i32>>> {
        Ok(self.operation(owner, lease)?.returned)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::types::DetTid;
    use crate::types::MmId;
    struct Pin(Arc<AtomicUsize>);
    impl Drop for Pin {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn owner() -> NetworkStreamOwner {
        let thread = DetTid::from_raw(7);
        NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        }
    }
    #[test]
    fn accepted_capture_retains_real_holder_across_validation_failure_and_cancellation() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut custody = AcceptedCustody::default();
        let who = owner();
        let lease = NetworkAcceptLeaseId(1);
        custody
            .submit(who, lease, NetworkStreamCallId::controlled_fixture(2))
            .unwrap();
        assert!(
            custody
                .capture(
                    who,
                    lease,
                    Ok(4),
                    |_| Ok(Pin(drops.clone())),
                    |_| Err(invalid("validation failed"))
                )
                .is_err()
        );
        assert_eq!(custody.returned(who, lease).unwrap(), Some(Ok(4)));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(
            custody
                .capture(
                    who,
                    lease,
                    Ok(4),
                    |_| panic!("reopened reused fd"),
                    |_| Ok(())
                )
                .is_err()
        );
        custody.abandon(who);
        assert!(custody.pin(who, lease).is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(custody);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn accepted_capture_wrong_owner_or_return_never_substitutes_a_pin() {
        let mut custody = AcceptedCustody::default();
        let who = owner();
        let lease = NetworkAcceptLeaseId(2);
        custody
            .submit(who, lease, NetworkStreamCallId::controlled_fixture(3))
            .unwrap();
        let stale = NetworkStreamOwner {
            mm: who.mm.for_exec(who.thread),
            ..who
        };
        assert!(
            custody
                .capture(
                    stale,
                    lease,
                    Ok(4),
                    |_| panic!("wrong owner opened fd"),
                    |_| Ok(())
                )
                .is_err()
        );
        custody
            .capture(who, lease, Ok(4), |_| Ok(99u64), |_| Ok(()))
            .unwrap();
        assert!(
            custody
                .capture(who, lease, Ok(5), |_| panic!("substituted fd"), |_| Ok(()))
                .is_err()
        );
        assert_eq!(*custody.pin(who, lease).unwrap(), 99);
        assert_eq!(
            custody.listener_call(who, lease).unwrap(),
            NetworkStreamCallId::controlled_fixture(3)
        );
    }
    #[test]
    fn accepted_capture_failure_or_errno_is_retained_without_reacquiring_or_dequeue_claim() {
        let mut custody = AcceptedCustody::<u64>::default();
        let who = owner();
        let lease = NetworkAcceptLeaseId(3);
        custody
            .submit(who, lease, NetworkStreamCallId::controlled_fixture(4))
            .unwrap();
        assert!(
            custody
                .capture(
                    who,
                    lease,
                    Ok(8),
                    |_| Err(invalid("pidfd_getfd failed")),
                    |_| Ok(())
                )
                .is_err()
        );
        assert!(
            custody
                .capture(
                    who,
                    lease,
                    Ok(8),
                    |_| panic!("retried acquisition"),
                    |_| Ok(())
                )
                .is_err()
        );
        assert_eq!(custody.returned(who, lease).unwrap(), Some(Ok(8)));
        let fault = NetworkAcceptLeaseId(4);
        custody
            .submit(who, fault, NetworkStreamCallId::controlled_fixture(5))
            .unwrap();
        custody
            .capture(
                who,
                fault,
                Err(libc::EFAULT),
                |_| panic!("errno acquired fd"),
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            custody.returned(who, fault).unwrap(),
            Some(Err(libc::EFAULT))
        );
        assert!(custody.pin(who, fault).is_err());
    }
    #[test]
    fn accepted_provider_failure_cannot_drop_or_replace_the_captured_descriptor() {
        let mut custody = AcceptedCustody::default();
        let who = owner();
        let lease = NetworkAcceptLeaseId(5);
        custody
            .submit(who, lease, NetworkStreamCallId::controlled_fixture(6))
            .unwrap();
        custody
            .capture(who, lease, Ok(8), |_| Ok(42u64), |_| Ok(()))
            .unwrap();
        assert!(
            custody
                .match_pin(who, lease, |pin| {
                    assert_eq!(*pin, 42);
                    Err(invalid("unmatched provider child"))
                })
                .is_err()
        );
        assert_eq!(*custody.pin(who, lease).unwrap(), 42);
        let identity = AcceptedPhysicalIdentity {
            provider: 1,
            object: 2,
            namespace: 3,
        };
        assert_eq!(
            custody
                .match_pin(who, lease, |pin| {
                    assert_eq!(*pin, 42);
                    Ok(identity)
                })
                .unwrap(),
            identity
        );
        assert_eq!(
            custody
                .match_pin(who, lease, |_| panic!("matched a replacement"))
                .unwrap(),
            identity
        );
    }
}

#[cfg(test)]
mod resolved_tests {
    use super::*;
    use crate::network_runtime::accepted_provider::CallStatus;
    use crate::network_runtime::accepted_provider::CommandResult;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    fn observation() -> Observation<CommandResult> {
        Observation {
            status: CallStatus {
                operation: "match".into(),
                returned: 0,
                errno: None,
            },
            raw: ffi::CommandResult {
                command: 2,
                operation: 2,
                task: 11,
                start_boottime: 77,
                identity: ffi::Identity {
                    provider: 9,
                    object: 3,
                    namespace: 4,
                },
                creation: 5,
                cookie: 6,
                phase: 1,
                ..Default::default()
            }
            .into(),
        }
    }
    #[test]
    fn accepted_resolved_identity_is_retained_without_fabricating_an_installation() {
        let thread = crate::types::DetTid::from_raw(51);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let lease = NetworkAcceptLeaseId(7);
        let call = NetworkStreamCallId::controlled_fixture(8);
        let mut custody = AcceptedCustody::default();
        custody.submit(owner, lease, call).unwrap();
        custody
            .capture(owner, lease, Ok(4), |_| Ok(91u64), |_| Ok(()))
            .unwrap();
        let matched = Resolved::checked(9, observation()).unwrap();
        assert_eq!(
            custody.confirm_resolved(owner, lease, matched).unwrap(),
            matched
        );
        assert_eq!(custody.returned(owner, lease).unwrap(), Some(Ok(4)));
        assert_eq!(*custody.pin(owner, lease).unwrap(), 91);
        let changed = Resolved {
            cookie: 7,
            ..matched
        };
        assert!(custody.confirm_resolved(owner, lease, changed).is_err());
        assert_eq!(
            custody.operations.get(&lease).unwrap().resolved,
            Some(matched)
        );
        custody.abandon(owner);
        assert!(custody.confirm_resolved(owner, lease, matched).is_err());
        assert_eq!(
            custody.operations.get(&lease).unwrap().resolved,
            Some(matched)
        );
        assert_eq!(custody.operations.get(&lease).unwrap().pin, Some(91));
    }
    #[test]
    fn accepted_resolved_unknown_partial_or_wrong_lifetime_cannot_issue_identity() {
        for cause in [
            "error",
            "phase",
            "operation",
            "provider",
            "creation",
            "cookie",
        ] {
            let mut observed = observation();
            match cause {
                "error" => observed.status.returned = -1,
                "phase" => observed.raw.phase = 0,
                "operation" => observed.raw.operation = 1,
                "provider" => observed.raw.identity.provider = 10,
                "creation" => observed.raw.creation = 0,
                "cookie" => observed.raw.cookie = 0,
                _ => unreachable!(),
            }
            assert!(Resolved::checked(9, observed).is_err(), "{cause}");
        }
    }

    #[test]
    fn accepted_effect_retains_late_partial_result_after_owner_abandonment() {
        use super::super::accepted_provider::CallStatus;
        use super::super::accepted_provider::Observation;
        use super::super::accepted_provider_ffi as ffi;
        let thread = crate::types::DetTid::from_raw(7);
        let who = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let lease = NetworkAcceptLeaseId(29);
        let mut custody = AcceptedCustody::<u64>::default();
        custody
            .submit(who, lease, NetworkStreamCallId::controlled_fixture(5))
            .unwrap();
        custody.retain_preparation(who, lease, 7, 11).unwrap();
        custody.abandon(who);
        assert!(
            custody
                .capture(
                    who,
                    lease,
                    Ok(8),
                    |_| panic!("reacquired after abandonment"),
                    |_| Ok(())
                )
                .is_err()
        );
        assert!(custody.returned(who, lease).is_err());
        assert_eq!(custody.receipt(who, lease).unwrap().returned, Some(Ok(8)));
        let mut effect = Observation {
            status: CallStatus {
                operation: "collect".into(),
                returned: -1,
                errno: Some(libc::EIO),
            },
            raw: ffi::AcceptedEffect {
                command: ffi::CommandResult {
                    command: 11,
                    operation: 4,
                    returned: 8,
                    phase: 1,
                    ..Default::default()
                },
                installation: ffi::FdAccept {
                    command: 11,
                    accept_lease: lease.0,
                    owner_mm: who.mm.generation(),
                    ..Default::default()
                },
            }
            .into(),
        };
        assert!(
            custody
                .retain_physical_effect(who, lease, effect.clone())
                .is_err()
        );
        assert_eq!(
            custody.operations[&lease].physical_effect,
            Some(effect.clone())
        );
        assert!(custody.pin(who, lease).is_err());
        effect.status.returned = 0;
        effect.status.errno = None;
        assert!(custody.retain_physical_effect(who, lease, effect).is_err());
        assert_eq!(
            custody.operations[&lease]
                .physical_effect
                .as_ref()
                .unwrap()
                .status
                .errno,
            Some(libc::EIO)
        );
    }
}
