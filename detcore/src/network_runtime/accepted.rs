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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubmittedAccept {
    pub listener: AcceptedPhysicalIdentity,
    pub fd: i32,
    pub flags: i32,
}
/// Exact historical installation. Private fields deliberately supply no
/// FilesId/OpenFileId/slot generation or public Installed constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HistoricalAcceptedInstallation {
    owner: NetworkStreamOwner,
    lease: NetworkAcceptLeaseId,
    command: u64,
    transition: super::fd_journal::Transition,
    resolved: Resolved,
    interference: Vec<u64>,
}
/// A negative result of the original accept, joined to the pre-invocation
/// owner and a complete journal cut. It has no public/RPC constructor.
#[derive(Debug, Clone)]
pub(crate) struct NoInstallation {
    owner: super::original_installation::Owner,
    lease: NetworkAcceptLeaseId,
    permit: crate::network_replay::NetworkFdPublicationPermit,
    errno: i32,
    dequeued: Option<Resolved>,
    command: u64,
    through: u64,
}
impl NoInstallation {
    pub(crate) fn matches(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        actual: &std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    ) -> bool {
        self.owner.owner == owner
            && self.lease == lease
            && self.permit == permit
            && self.owner.files == permit.files
            && permit.owner == owner
            && std::sync::Arc::ptr_eq(&self.owner.metadata, actual)
    }
    pub(crate) fn errno(&self) -> i32 {
        self.errno
    }
    pub(crate) fn dequeued(&self) -> Option<Resolved> {
        self.dequeued
    }
    fn same(&self, other: &Self) -> bool {
        self.owner.same(&other.owner)
            && self.lease == other.lease
            && self.permit == other.permit
            && self.errno == other.errno
            && self.dequeued == other.dequeued
            && self.command == other.command
            && self.through == other.through
    }
}
type Collection = Result<(u64, Option<Result<(), String>>), String>;

#[derive(Debug)]
struct Accept<T> {
    owner: NetworkStreamOwner,
    // Keep the admitting call with the pin until this original entry retires.
    _listener_call: NetworkStreamCallId,
    returned: Option<Result<i32, i32>>,
    pin: Option<T>,
    capture_error: Option<String>,
    matched: Option<AcceptedPhysicalIdentity>,
    resolved: Option<Resolved>,
    abandoned: bool,
    prepared_effect: Option<(u64, u64)>,
    submitted_effect: Option<SubmittedAccept>,
    historical: Option<HistoricalAcceptedInstallation>,
    installation_owner: Option<super::original_installation::Owner>,
    installed: Option<crate::types::FdSlotBinding>,
    no_installation: Option<NoInstallation>,
    no_installation_published: bool,
    installation_admission: Option<crate::network_replay::NetworkFdPublicationAdmission>,
    physical_effect:
        Option<super::accepted_provider::Observation<super::accepted_provider::AcceptedEffect>>,
    collection: Option<Collection>,
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
    pub(super) fn bind_installation_owner(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        binding: super::original_installation::Owner,
    ) -> io::Result<()> {
        let operation = self.operation(owner, lease)?;
        if let Some(prior) = &operation.installation_owner {
            return if prior.same(&binding) {
                Ok(())
            } else {
                Err(invalid(
                    "accepted preparation changed retained installation owner",
                ))
            };
        }
        if binding.owner != owner
            || operation.prepared_effect.is_some()
            || operation.returned.is_some()
        {
            return Err(invalid(
                "accepted installation owner must precede native preparation",
            ));
        }
        self.operations.get_mut(&lease).unwrap().installation_owner = Some(binding);
        Ok(())
    }

    pub(super) fn installation_owner(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<super::original_installation::Owner> {
        self.operation(owner, lease)?
            .installation_owner
            .clone()
            .ok_or_else(|| invalid("accepted installation lost pre-invocation metadata custody"))
    }

    pub(super) fn original_installation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        through: u64,
        history: &super::fd_journal::History,
    ) -> io::Result<super::original_installation::Installation> {
        let operation = self.operation(owner, lease)?;
        let historical = operation
            .historical
            .as_ref()
            .ok_or_else(|| invalid("accepted installation lacks original checked history"))?;
        let super::fd_journal::Transition::Install { begin, end } = &historical.transition else {
            return Err(invalid(
                "accepted historical receipt changed transition kind",
            ));
        };
        if historical.owner != owner
            || historical.lease != lease
            || operation.returned != Some(Ok(begin.fd))
        {
            return Err(invalid(
                "accepted historical receipt changed original return",
            ));
        }
        super::original_installation::Installation::checked(
            self.installation_owner(owner, lease)?,
            permit,
            super::original_installation::Source::Accepted {
                lease,
                child: historical.resolved,
            },
            (historical.command, begin.fd, begin.file),
            (begin.sequence, end.sequence, through),
            history,
        )
    }

    pub(super) fn retain_installed(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        binding: crate::types::FdSlotBinding,
    ) -> io::Result<()> {
        let operation = self.operation(owner, lease)?;
        let installation_owner = self.installation_owner(owner, lease)?;
        if operation.returned != Some(Ok(binding.slot.fd))
            || binding.slot.files != installation_owner.files
            || binding.generation == 0
            || !binding.open_file.is_socket()
            || operation.installed.is_some_and(|prior| prior != binding)
        {
            return Err(invalid(
                "accepted publication changed its exact local installation",
            ));
        }
        self.operations.get_mut(&lease).unwrap().installed = Some(binding);
        Ok(())
    }

    pub(super) fn installation_admission(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Option<crate::network_replay::NetworkFdPublicationAdmission>> {
        Ok(self.operation(owner, lease)?.installation_admission.clone())
    }
    pub(super) fn retain_installation_admission(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        admission: &crate::network_replay::NetworkFdPublicationAdmission,
    ) -> io::Result<()> {
        let operation = self.operation(owner, lease)?;
        let bound = operation
            .installation_owner
            .as_ref()
            .ok_or_else(|| invalid("accepted publication lost its original metadata"))?;
        if bound.owner != owner
            || bound.files != admission.permit.files
            || admission.permit.owner != owner
            || admission.recovery.is_some()
            || (operation.historical.is_none()
                && !(matches!(operation.returned, Some(Err(_)))
                    && self.collection_result(owner, lease).is_ok()))
            || operation.installed.is_some()
            || operation
                .installation_admission
                .as_ref()
                .is_some_and(|old| old != admission)
        {
            return Err(invalid("accepted publication changed retained admission"));
        }
        self.operations
            .get_mut(&lease)
            .unwrap()
            .installation_admission = Some(admission.clone());
        Ok(())
    }
    pub(super) fn installed(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Option<crate::types::FdSlotBinding>> {
        Ok(self.operation(owner, lease)?.installed)
    }

    pub(super) fn captured_result(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Result<i32, i32>> {
        self.operation(owner, lease)?
            .returned
            .ok_or_else(|| invalid("accepted original invocation has not returned"))
    }

    pub(super) fn retained_no_installation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        permit: crate::network_replay::NetworkFdPublicationPermit,
    ) -> io::Result<Option<NoInstallation>> {
        let op = self.operation(owner, lease)?;
        if op
            .no_installation
            .as_ref()
            .is_some_and(|r| r.permit != permit)
        {
            return Err(invalid("negative accepted recovery changed permit"));
        }
        Ok(op.no_installation.clone())
    }

    pub(super) fn no_installation_published(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<bool> {
        Ok(self.operation(owner, lease)?.no_installation_published)
    }
    pub(super) fn retain_no_installation_published(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        receipt: &NoInstallation,
    ) -> io::Result<()> {
        let op = self.operation(owner, lease)?;
        if op.no_installation.as_ref().is_none_or(|r| !r.same(receipt)) {
            return Err(invalid(
                "negative accepted publication changed retained proof",
            ));
        }
        self.operations
            .get_mut(&lease)
            .unwrap()
            .no_installation_published = true;
        Ok(())
    }

    pub(super) fn no_installation(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        permit: crate::network_replay::NetworkFdPublicationPermit,
        through: u64,
        history: &super::fd_journal::History,
    ) -> io::Result<NoInstallation> {
        self.collection_result(owner, lease)?;
        let op = self.operation(owner, lease)?;
        let bound = self.installation_owner(owner, lease)?;
        let submitted = op
            .submitted_effect
            .as_ref()
            .ok_or_else(|| invalid("accept operands absent"))?;
        let (_, command) = op
            .prepared_effect
            .ok_or_else(|| invalid("accept preparation absent"))?;
        let effect = op
            .physical_effect
            .as_ref()
            .ok_or_else(|| invalid("accept result absent"))?;
        let c = &effect.raw.command;
        let a = &effect.raw.installation;
        // Positive ENTERED and SYSCALL_RETURNED, never missing fd_install alone.
        if bound.owner != owner
            || permit.owner != owner
            || permit.files != bound.files
            || op
                .installation_admission
                .as_ref()
                .is_none_or(|admission| admission.permit != permit)
            || effect.status.returned != 0
            || command == 0
            || c.command != command
            || c.operation != 4
            || c.phase != 1
            || c.reserved != 0
            || c.original_count != 0
            || !(-4095..=-1).contains(&c.returned)
            || op.returned != Some(Err(-c.returned))
            || bound.provider == 0
            || c.identity.provider != bound.provider
            || bound.task == 0
            || c.task != bound.task
            || bound.start == 0
            || c.start_boottime != bound.start
            || a.command != command
            || a.accept_lease != lease.0
            || a.owner_mm != owner.mm.generation()
            || a.task != bound.task
            || a.task_start != bound.start
            || bound.table == 0
            || a.table != bound.table
            || a.requested_fd != submitted.fd
            || a.flags != submitted.flags
            || a.problem != 0
            || !matches!(a.phases, 65 | 67 | 71)
            || a.file != 0
            || a.install_begin != 0
            || a.install_end != 0
            || a.returned_fd != 0
            || (a.do_accept_errno != 0 && a.do_accept_errno != -c.returned)
            || op.pin.is_some()
            || op.historical.is_some()
            || op.installed.is_some()
            || op.matched.is_some()
            || op.resolved.is_some()
            || history
                .next()?
                .checked_sub(1)
                .is_none_or(|last| last < through)
            || history.contains_command(command)?
        {
            return Err(invalid(
                "negative accept changed original owner/return or contradicts journal",
            ));
        }
        let empty: super::accepted_provider::Identity =
            super::accepted_provider_ffi::Identity::default().into();
        if (a.phases & 2 != 0
            && (a.listener.provider != submitted.listener.provider
                || a.listener.object != submitted.listener.object
                || a.listener.namespace != submitted.listener.namespace))
            || (a.phases & 2 == 0 && a.listener != empty)
        {
            return Err(invalid(
                "negative accept changed original selected listener",
            ));
        }
        let dequeued = if a.phases & 4 != 0 {
            if a.child != c.identity
                || a.child.provider != bound.provider
                || a.child.object == 0
                || a.child.namespace == 0
                || a.child.namespace != submitted.listener.namespace
                || a.creation == 0
                || a.creation != c.creation
                || a.cookie == 0
                || a.cookie != c.cookie
            {
                return Err(invalid("negative accept changed the actual dequeued child"));
            }
            Some(Resolved {
                physical: AcceptedPhysicalIdentity {
                    provider: a.child.provider,
                    object: a.child.object,
                    namespace: a.child.namespace,
                },
                creation: a.creation,
                cookie: a.cookie,
            })
        } else {
            if a.child != empty
                || a.creation != 0
                || a.cookie != 0
                || c.identity.object != 0
                || c.identity.namespace != 0
                || c.creation != 0
                || c.cookie != 0
            {
                return Err(invalid("no-connection result contains a dequeued child"));
            }
            None
        };
        let receipt = NoInstallation {
            owner: bound,
            lease,
            permit,
            errno: -c.returned,
            dequeued,
            command,
            through,
        };
        if op
            .no_installation
            .as_ref()
            .is_some_and(|old| !old.same(&receipt))
        {
            return Err(invalid("negative accept changed retained cut"));
        }
        self.operations.get_mut(&lease).unwrap().no_installation = Some(receipt.clone());
        Ok(receipt)
    }

    pub(super) fn installation_flags(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<i32> {
        self.operation(owner, lease)?
            .submitted_effect
            .as_ref()
            .map(|s| s.flags)
            .ok_or_else(|| invalid("accepted installation lacks submitted flag operands"))
    }

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
                _listener_call: listener_call,
                returned: None,
                pin: None,
                capture_error: None,
                matched: None,
                resolved: None,
                abandoned: false,
                prepared_effect: None,
                submitted_effect: None,
                historical: None,
                installation_owner: None,
                installed: None,
                no_installation: None,
                no_installation_published: false,
                installation_admission: None,
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

    pub(super) fn retain_submission(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        submitted: SubmittedAccept,
    ) -> io::Result<()> {
        let op = self.operation(owner, lease)?;
        if op
            .submitted_effect
            .as_ref()
            .is_some_and(|old| old != &submitted)
        {
            return Err(invalid("accepted submission changed"));
        }
        self.operations.get_mut(&lease).unwrap().submitted_effect = Some(submitted);
        Ok(())
    }
    pub(super) fn installation_end(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<u64> {
        let op = self.operation(owner, lease)?;
        let effect = op
            .physical_effect
            .as_ref()
            .ok_or_else(|| invalid("accepted collection not retained"))?;
        if effect.status.returned != 0 || effect.raw.installation.install_end == 0 {
            return Err(invalid("accepted installation lacks a complete endpoint"));
        }
        Ok(effect.raw.installation.install_end)
    }
    pub(super) fn retain_historical(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        history: &super::fd_journal::History,
    ) -> io::Result<()> {
        let op = self.operation(owner, lease)?;
        self.pin(owner, lease)?;
        let (_, command) = op
            .prepared_effect
            .ok_or_else(|| invalid("accepted preparation absent"))?;
        let submitted = op
            .submitted_effect
            .as_ref()
            .ok_or_else(|| invalid("accepted submission absent"))?;
        let resolved = op
            .resolved
            .ok_or_else(|| invalid("accepted held-FD resolution absent"))?;
        let effect = op
            .physical_effect
            .as_ref()
            .ok_or_else(|| invalid("accepted physical effect absent"))?;
        let c = &effect.raw.command;
        let a = &effect.raw.installation;
        let physical = AcceptedPhysicalIdentity {
            provider: a.child.provider,
            object: a.child.object,
            namespace: a.child.namespace,
        };
        let listener = AcceptedPhysicalIdentity {
            provider: a.listener.provider,
            object: a.listener.object,
            namespace: a.listener.namespace,
        };
        if effect.status.returned != 0
            || c.operation != 4
            || c.phase != 1
            || c.reserved != 0
            || c.command != command
            || a.command != command
            || command == 0
            || c.task == 0
            || c.start_boottime == 0
            || a.task != c.task
            || a.task_start != c.start_boottime
            || a.accept_lease != lease.0
            || a.owner_mm != owner.mm.generation()
            || a.problem != 0
            || a.phases != 127
            || a.do_accept_errno != 0
            || c.returned < 0
            || a.returned_fd != c.returned
            || op.returned != Some(Ok(c.returned))
            || listener != submitted.listener
            || physical.provider != listener.provider
            || physical.namespace != listener.namespace
            || a.requested_fd != submitted.fd
            || a.flags != submitted.flags
            || c.identity != a.child
            || physical != resolved.physical
            || a.creation != resolved.creation
            || c.creation != a.creation
            || a.cookie != resolved.cookie
            || c.cookie != a.cookie
            || a.table == 0
            || a.file == 0
        {
            return Err(invalid(
                "accepted historical installation identity mismatch",
            ));
        }
        let transition = history
            .transition(a.install_end)?
            .ok_or_else(|| invalid("accepted installation endpoint still pending"))?;
        let super::fd_journal::Transition::Install { begin, end } = &transition else {
            return Err(invalid("accepted endpoint is not installation"));
        };
        if begin.sequence != a.install_begin
            || end.sequence != a.install_end
            || begin.table != a.table
            || begin.file != a.file
            || begin.fd != a.returned_fd
            || begin.task != a.task
            || begin.task_start != a.task_start
            || begin.accept_command != command
        {
            return Err(invalid("accepted journal differs from held effect"));
        }
        let receipt = HistoricalAcceptedInstallation {
            owner,
            lease,
            command,
            interference: history.interference(
                begin.sequence,
                end.sequence,
                a.table,
                a.returned_fd,
            ),
            transition,
            resolved,
        };
        if op
            .historical
            .as_ref()
            .is_some_and(|prior| prior != &receipt)
        {
            return Err(invalid("historical accepted receipt changed"));
        }
        self.operations.get_mut(&lease).unwrap().historical = Some(receipt);
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

    #[cfg(test)]
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

    #[cfg(test)]
    pub(super) fn listener_call(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<NetworkStreamCallId> {
        Ok(self.operation(owner, lease)?._listener_call)
    }

    #[cfg(test)]
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

#[cfg(test)]
mod historical_tests {
    use super::super::accepted_provider::CallStatus;
    use super::super::accepted_provider::Observation;
    use super::super::accepted_provider_ffi as ffi;
    use super::super::fd_journal::History;
    use super::*;
    fn fixture() -> (
        AcceptedCustody<u64>,
        NetworkStreamOwner,
        NetworkAcceptLeaseId,
        History,
    ) {
        let thread = crate::types::DetTid::from_raw(7);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let lease = NetworkAcceptLeaseId(29);
        let listener = AcceptedPhysicalIdentity {
            provider: 9,
            object: 2,
            namespace: 4,
        };
        let physical = AcceptedPhysicalIdentity {
            object: 3,
            ..listener
        };
        let mut custody = AcceptedCustody::default();
        custody
            .submit(owner, lease, NetworkStreamCallId::controlled_fixture(5))
            .unwrap();
        custody
            .retain_submission(
                owner,
                lease,
                SubmittedAccept {
                    listener,
                    fd: 5,
                    flags: 0,
                },
            )
            .unwrap();
        custody.retain_preparation(owner, lease, 7, 11).unwrap();
        custody
            .capture(owner, lease, Ok(8), |_| Ok(77), |_| Ok(()))
            .unwrap();
        custody
            .confirm_resolved(
                owner,
                lease,
                Resolved {
                    physical,
                    creation: 5,
                    cookie: 6,
                },
            )
            .unwrap();
        let id = ffi::Identity {
            provider: 9,
            object: 3,
            namespace: 4,
        };
        let effect = Observation {
            status: CallStatus {
                operation: "collect".into(),
                returned: 0,
                errno: None,
            },
            raw: ffi::AcceptedEffect {
                command: ffi::CommandResult {
                    command: 11,
                    operation: 4,
                    task: 10,
                    start_boottime: 11,
                    identity: id,
                    creation: 5,
                    cookie: 6,
                    returned: 8,
                    phase: 1,
                    ..Default::default()
                },
                installation: ffi::FdAccept {
                    command: 11,
                    accept_lease: 29,
                    owner_mm: owner.mm.generation(),
                    task: 10,
                    task_start: 11,
                    table: 1,
                    file: 1,
                    install_begin: 1,
                    install_end: 2,
                    listener: ffi::Identity { object: 2, ..id },
                    child: id,
                    creation: 5,
                    cookie: 6,
                    phases: 127,
                    requested_fd: 5,
                    returned_fd: 8,
                    ..Default::default()
                },
            }
            .into(),
        };
        custody
            .retain_physical_effect(owner, lease, effect)
            .unwrap();
        let begin = ffi::FdEvent {
            sequence: 1,
            kind: 1,
            task: 10,
            task_start: 11,
            table: 1,
            file: 1,
            fd: 8,
            accept_command: 11,
            complete: 1,
            ..Default::default()
        };
        let end = ffi::FdEvent {
            sequence: 2,
            kind: 2,
            dependency: 1,
            ..begin
        };
        let mut history = History::default();
        for e in [begin, end] {
            history
                .retain(
                    ffi::FdStatus {
                        next_table: 1,
                        next_file: 1,
                        next_event: 2,
                        problem: 0,
                    }
                    .into(),
                    e.into(),
                )
                .unwrap();
        }
        (custody, owner, lease, history)
    }
    #[test]
    fn accepted_history_joins_submitted_arguments_actual_callback_and_held_fd() {
        let (mut c, owner, lease, h) = fixture();
        c.retain_historical(owner, lease, &h).unwrap();
        let before = c.operations[&lease].historical.clone();
        c.retain_historical(owner, lease, &h).unwrap();
        assert_eq!(before, c.operations[&lease].historical);
        assert_eq!(*c.pin(owner, lease).unwrap(), 77);
        assert!(before.unwrap().interference.is_empty());
    }
    #[test]
    fn accepted_history_refuses_changed_scalar_or_lifetime_without_losing_pin() {
        for cause in 0..24 {
            let (mut c, owner, lease, h) = fixture();
            let op = c.operations.get_mut(&lease).unwrap();
            let raw = &mut op.physical_effect.as_mut().unwrap().raw;
            match cause {
                0 => raw.command.command += 1,
                1 => raw.command.operation = 2,
                2 => raw.command.phase = 0,
                3 => raw.command.reserved = 1,
                4 => raw.installation.task += 1,
                5 => raw.installation.task_start += 1,
                6 => raw.installation.table = 2,
                7 => raw.installation.file = 2,
                8 => raw.installation.install_begin = 2,
                9 => raw.installation.install_end = 1,
                10 => raw.installation.accept_lease += 1,
                11 => raw.installation.owner_mm += 1,
                12 => raw.installation.listener.object += 1,
                13 => raw.installation.flags = 1,
                14 => raw.installation.requested_fd += 1,
                15 => raw.installation.returned_fd += 1,
                16 => raw.installation.phases = 65,
                17 => raw.installation.problem = 1,
                18 => raw.installation.cookie += 1,
                19 => raw.installation.creation += 1,
                20 => raw.installation.child.namespace += 1,
                21 => raw.installation.do_accept_errno = 9,
                22 => raw.command.returned = -9,
                _ => op.submitted_effect = None,
            }
            assert!(
                c.retain_historical(owner, lease, &h).is_err(),
                "cause {cause}"
            );
            assert!(c.operations[&lease].historical.is_none());
            assert_eq!(*c.pin(owner, lease).unwrap(), 77);
        }
    }
}

/// Synthetic command/collection inputs for tests of the real private consumer.
/// This does not certify a native syscall or manufacture a semantic FD fact.
#[cfg(test)]
fn no_installation_fixture(
    owner: NetworkStreamOwner,
    lease: NetworkAcceptLeaseId,
    actual: std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    admission: crate::network_replay::NetworkFdPublicationAdmission,
    dequeued: bool,
) -> (AcceptedCustody<u64>, super::fd_journal::History) {
    use super::accepted_provider_ffi as ffi;
    let mut custody = AcceptedCustody::default();
    custody
        .submit(owner, lease, NetworkStreamCallId::controlled_fixture(5))
        .unwrap();
    custody
        .bind_installation_owner(
            owner,
            lease,
            super::original_installation::Owner {
                owner,
                metadata: actual,
                files: admission.permit.files,
                provider: 7,
                task: (31u64 << 32) | 31,
                start: 101,
                table: 13,
            },
        )
        .unwrap();
    custody
        .retain_submission(
            owner,
            lease,
            SubmittedAccept {
                listener: AcceptedPhysicalIdentity {
                    provider: 7,
                    object: 10,
                    namespace: 9,
                },
                fd: 5,
                flags: 0,
            },
        )
        .unwrap();
    custody.retain_preparation(owner, lease, 17, 71).unwrap();
    custody
        .capture(
            owner,
            lease,
            Err(libc::EAGAIN),
            |_| panic!("negative result cannot acquire a pin"),
            |_| Ok(()),
        )
        .unwrap();
    custody
        .retain_collection_submission(owner, lease, Ok(18))
        .unwrap();
    let child = ffi::Identity {
        provider: 7,
        object: 12,
        namespace: 9,
    };
    let effect = ffi::AcceptedEffect {
        command: ffi::CommandResult {
            command: 71,
            operation: 4,
            phase: 1,
            task: (31u64 << 32) | 31,
            start_boottime: 101,
            returned: -libc::EAGAIN,
            identity: if dequeued {
                child
            } else {
                ffi::Identity {
                    provider: 7,
                    ..Default::default()
                }
            },
            creation: if dequeued { 15 } else { 0 },
            cookie: if dequeued { 16 } else { 0 },
            ..Default::default()
        },
        installation: ffi::FdAccept {
            command: 71,
            accept_lease: lease.0,
            owner_mm: owner.mm.generation(),
            task: (31u64 << 32) | 31,
            task_start: 101,
            table: 13,
            requested_fd: 5,
            listener: ffi::Identity {
                provider: 7,
                object: 10,
                namespace: 9,
            },
            child: if dequeued { child } else { Default::default() },
            creation: if dequeued { 15 } else { 0 },
            cookie: if dequeued { 16 } else { 0 },
            phases: if dequeued { 71 } else { 67 },
            do_accept_errno: libc::EAGAIN,
            ..Default::default()
        },
    };
    custody
        .complete_collection(
            owner,
            lease,
            18,
            Ok(super::accepted_provider::Reply::AcceptedEffect(
                super::accepted_provider::Observation {
                    status: super::accepted_provider::CallStatus {
                        operation: "collect_accept".into(),
                        returned: 0,
                        errno: None,
                    },
                    raw: effect.into(),
                },
            )),
        )
        .unwrap();
    custody
        .retain_installation_admission(owner, lease, &admission)
        .unwrap();
    (custody, super::fd_journal::History::default())
}

#[cfg(test)]
mod negative_installation_tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use super::*;
    use crate::network_replay::NetworkFdPublicationAdmission;
    use crate::network_replay::NetworkFdPublicationPermit;
    use crate::network_replay::NetworkStreamLeaseId;
    use crate::types::DetTid;
    use crate::types::FilesId;
    use crate::types::MmId;
    fn fixture(
        dequeued: bool,
    ) -> (
        AcceptedCustody<u64>,
        NetworkStreamOwner,
        NetworkAcceptLeaseId,
        NetworkFdPublicationPermit,
        super::super::fd_journal::History,
    ) {
        let thread = DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let lease = NetworkAcceptLeaseId(29);
        let actual = Arc::new(Mutex::new(
            crate::tool_local::FileMetadata::empty_network_fixture(thread),
        ));
        let permit = NetworkFdPublicationPermit {
            owner,
            files: FilesId::initial(thread),
            lease: NetworkStreamLeaseId::controlled_fixture(17),
        };
        let admission = NetworkFdPublicationAdmission {
            permit,
            acknowledged_sequence: 0,
            acknowledged_generation: 0,
            recovery: None,
        };
        let (custody, history) = no_installation_fixture(owner, lease, actual, admission, dequeued);
        (custody, owner, lease, permit, history)
    }
    #[test]
    fn accepted_negative_joins_original_result_and_retains_distinct_dequeued_child() {
        for (dequeued, listener_entered) in [(false, false), (false, true), (true, true)] {
            let (mut custody, owner, lease, permit, history) = fixture(dequeued);
            if !listener_entered {
                let raw = &mut custody
                    .operations
                    .get_mut(&lease)
                    .unwrap()
                    .physical_effect
                    .as_mut()
                    .unwrap()
                    .raw;
                raw.installation.phases = 65;
                raw.installation.listener =
                    super::super::accepted_provider_ffi::Identity::default().into();
                raw.installation.do_accept_errno = 0;
            }
            let checked = custody
                .no_installation(owner, lease, permit, 0, &history)
                .unwrap();
            assert_eq!(checked.errno(), libc::EAGAIN);
            assert_eq!(checked.dequeued().is_some(), dequeued);
            assert!(custody.pin(owner, lease).is_err());
            assert!(!custody.no_installation_published(owner, lease).unwrap());
            assert!(
                custody
                    .retained_no_installation(owner, lease, permit)
                    .unwrap()
                    .unwrap()
                    .same(&checked)
            );
            custody.abandon(owner);
            assert!(
                custody
                    .no_installation(owner, lease, permit, 0, &history)
                    .is_err()
            );
        }
    }
    #[test]
    fn accepted_negative_refuses_original_identity_phase_and_installation_mutations() {
        for bad in 0..32 {
            let (mut custody, owner, lease, permit, history) = fixture(false);
            let op = custody.operations.get_mut(&lease).unwrap();
            let raw = &mut op.physical_effect.as_mut().unwrap().raw;
            match bad {
                0 => raw.command.task ^= 1,
                1 => raw.command.start_boottime += 1,
                2 => raw.installation.task ^= 1,
                3 => raw.installation.task_start += 1,
                4 => raw.installation.owner_mm += 1,
                5 => raw.installation.accept_lease += 1,
                6 => raw.installation.table += 1,
                7 => raw.command.identity.provider += 1,
                8 => raw.command.command += 1,
                9 => raw.installation.command += 1,
                10 => raw.installation.requested_fd += 1,
                11 => raw.installation.flags ^= libc::SOCK_CLOEXEC,
                12 => raw.installation.phases = 64,
                13 => raw.installation.phases = 3,
                14 => raw.installation.phases |= 8,
                15 => raw.installation.phases |= 16,
                16 => raw.installation.phases |= 32,
                17 => raw.installation.problem = 1,
                18 => raw.installation.file = 19,
                19 => raw.installation.install_begin = 1,
                20 => raw.installation.install_end = 2,
                21 => raw.installation.returned_fd = 17,
                22 => raw.command.returned = -libc::EINTR,
                23 => raw.command.returned = 0,
                24 => raw.command.phase = 0,
                25 => raw.command.operation = 12,
                26 => raw.command.original_count = 1,
                27 => raw.installation.listener.object += 1,
                28 => raw.installation.child.object = 12,
                29 => raw.command.creation = 15,
                30 => raw.installation.do_accept_errno = libc::EINTR,
                31 => raw.command.reserved = 1,
                _ => unreachable!(),
            }
            assert!(
                custody
                    .no_installation(owner, lease, permit, 0, &history)
                    .is_err(),
                "mutation {bad}"
            );
            assert!(custody.operations[&lease].no_installation.is_none());
            assert!(!custody.no_installation_published(owner, lease).unwrap());
        }
    }
    #[test]
    fn accepted_negative_requires_complete_origin_journal_and_exact_permit() {
        use super::super::accepted_provider_ffi as ffi;
        let (mut custody, owner, lease, permit, mut history) = fixture(false);
        assert!(
            custody
                .no_installation(owner, lease, permit, 1, &history)
                .is_err()
        );
        let wrong = NetworkFdPublicationPermit {
            lease: NetworkStreamLeaseId::controlled_fixture(18),
            ..permit
        };
        assert!(
            custody
                .no_installation(owner, lease, wrong, 0, &history)
                .is_err()
        );
        let begin = ffi::FdEvent {
            sequence: 1,
            kind: 1,
            table: 13,
            file: 19,
            task: (31u64 << 32) | 31,
            task_start: 101,
            fd: 17,
            accept_command: 71,
            complete: 1,
            ..Default::default()
        };
        for row in [
            begin,
            ffi::FdEvent {
                sequence: 2,
                kind: 2,
                dependency: 1,
                ..begin
            },
        ] {
            history
                .retain(
                    ffi::FdStatus {
                        next_table: 13,
                        next_file: 19,
                        next_event: 2,
                        problem: 0,
                    }
                    .into(),
                    row.into(),
                )
                .unwrap();
        }
        assert!(
            custody
                .no_installation(owner, lease, permit, 2, &history)
                .is_err(),
            "actual install contradicts errno"
        );
        assert!(custody.operations[&lease].no_installation.is_none());
    }
}

#[cfg(test)]
pub(crate) fn checked_no_installation_fixture(
    owner: NetworkStreamOwner,
    lease: NetworkAcceptLeaseId,
    actual: std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    admission: crate::network_replay::NetworkFdPublicationAdmission,
    dequeued: bool,
) -> NoInstallation {
    let permit = admission.permit;
    let (mut custody, history) = no_installation_fixture(owner, lease, actual, admission, dequeued);
    custody
        .no_installation(owner, lease, permit, 0, &history)
        .unwrap()
}
