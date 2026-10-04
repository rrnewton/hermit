//! Private admission for an original finite TCP Close. It carries no clock or
//! continuation authority and keeps the ordinary original-Call lifecycle.
use std::sync::Arc;

use super::*;
use crate::network_replay::finite_close::FiniteCloseBirth;
use crate::network_runtime::shared_waits::JoinedSharedPrefix;
use crate::network_runtime::shared_waits::SharedAttemptAdmission;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

#[derive(Debug)]
pub(crate) struct ForegroundCloseOrigin {
    root: Arc<crate::network_runtime::ForegroundRoot>,
    epoch: u64,
    admission: Admission,
    raw: [usize; 6],
    birth: Arc<FiniteCloseBirth>,
    prefix: JoinedSharedPrefix,
}
impl ForegroundCloseOrigin {
    pub(crate) fn admission(&self) -> &Admission {
        &self.admission
    }
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.root.owner()
    }
    pub(crate) fn raw(&self) -> [usize; 6] {
        self.raw
    }
}

pub(crate) struct ForegroundCloseEntry {
    pub(crate) read: NetworkFdReadAdmission,
    pub(crate) arguments: Arguments,
    pub(crate) raw: [usize; 6],
}
impl NetworkReplayEngine {
    pub(crate) fn foreground_close_candidate(
        &self,
        owner: NetworkStreamOwner,
        read: &NetworkFdReadAdmission,
    ) -> Result<bool, NetworkReplayError> {
        Ok(self.finite_close_birth_for_read(owner, read)?.is_some())
    }
    pub(crate) fn validate_foreground_close_birth_root(
        &self,
        owner: NetworkStreamOwner,
        read: &NetworkFdReadAdmission,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> Result<(), NetworkReplayError> {
        self.check_native_retirement()?;
        let birth = self
            .finite_close_birth_for_read(owner, read)?
            .ok_or_else(|| protocol("finite Close lost original birth"))?;
        if root.owner() != owner || !birth.matches_initial_root(root) {
            return Err(protocol("finite Close changed original birth root"));
        }
        Ok(())
    }
    pub(crate) fn begin_foreground_close(
        &mut self,
        entry: ForegroundCloseEntry,
        grant: &SharedMmForegroundObservation<'_>,
        prefix: &JoinedSharedPrefix,
        physical: &SharedAttemptAdmission<'_>,
    ) -> Result<Arc<ForegroundCloseOrigin>, NetworkReplayError> {
        let ForegroundCloseEntry {
            read,
            arguments,
            raw,
        } = entry;
        self.check_native_retirement()?;
        self.validate_fd_read_grant(grant.owner(), &read)?;
        let binding = read
            .binding
            .ok_or_else(|| protocol("finite Close lacks OFD"))?;
        let birth = self
            .finite_close_birth_for_read(grant.owner(), &read)?
            .ok_or_else(|| protocol("finite Close lost birth eligibility"))?;
        if !self.uses_shared_mm_attempts()
            || !grant.admits_initial_singleton()
            || !birth.matches_initial_root(grant.root())
            || arguments.kind != Kind::Close
            || !arguments.kind.valid_operands(
                arguments.address,
                arguments.length,
                arguments.original_count,
            )
            || arguments.operation.tid != grant.owner().thread
            || arguments.files != grant.root().files()
            || arguments.files != read.publication.permit.files
            || arguments.fd != read.fd
            || arguments.binding != Some(binding)
            || read.external_grant.is_some()
            || raw[0] != arguments.fd as usize
            || !Arc::ptr_eq(grant.root(), prefix.root())
            || !physical.is_original_prefix(prefix)
            || !physical.matches_peers(self, None)?
            || !self
                .shared_call_census_excluding(None, None)?
                .rows
                .is_empty()
            || self
                .stream_calls
                .values()
                .any(|state| state.owner == grant.owner())
        {
            return Err(protocol(
                "finite Close changed original singleton/tuple/custody",
            ));
        }
        self.validate_finite_close_birth(binding.open_file, &birth)?;
        let admission = self.begin_original_call_with_read(
            grant.owner(),
            arguments,
            OriginalResultSource::Native,
            Some(read),
        )?;
        let origin = Arc::new(ForegroundCloseOrigin {
            root: grant.root().clone(),
            epoch: grant.epoch(),
            admission: admission.clone(),
            raw,
            birth,
            prefix: prefix.clone(),
        });
        // Reader transfer has completed; this infallible attachment is its sole
        // new owner. Every later failure retains the existing Call debt.
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .foreground_close = Some(origin.clone());
        Ok(origin)
    }
    pub(crate) fn foreground_close_origin(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<Option<Arc<ForegroundCloseOrigin>>, NetworkReplayError> {
        let (_, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments {
            return Err(protocol("finite Close changed original admission"));
        }
        Ok(original.foreground_close.clone())
    }
    pub(crate) fn validate_foreground_close(
        &self,
        origin: &Arc<ForegroundCloseOrigin>,
        grant: &SharedMmForegroundObservation<'_>,
        raw: [usize; 6],
    ) -> Result<(), NetworkReplayError> {
        self.check_native_retirement()?;
        let (state, original) =
            self.original_connect_state(origin.owner(), origin.admission.call)?;
        if original
            .foreground_close
            .as_ref()
            .is_none_or(|actual| !Arc::ptr_eq(actual, origin))
            || original.arguments != origin.admission.arguments
            || origin.admission.arguments.kind != Kind::Close
            || !grant.admits_initial_singleton()
            || !origin.birth.matches_initial_root(grant.root())
            || !Arc::ptr_eq(grant.root(), &origin.root)
            || grant.epoch() != origin.epoch
            || grant.owner() != origin.owner()
            || raw != origin.raw
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || original.cancel_requested
            || original.final_wait
            || original.consumed
            || original.provider_retired
            || original.backend_entered
            || original.backend_result.is_some()
            || state.physical_pin_required
            || original.pin.is_some()
            || !origin
                .prefix
                .matches_retained_peers(self, Some(origin.admission.call))?
            || !self
                .shared_call_census_excluding(None, Some(origin.admission.call))?
                .rows
                .is_empty()
        {
            return Err(protocol("finite Close changed retained Normal/entry/Call"));
        }
        let binding = origin
            .admission
            .arguments
            .binding
            .ok_or_else(|| protocol("finite Close lost binding"))?;
        self.validate_stream_call_lifetime(
            origin.owner(),
            origin.admission.call,
            binding.open_file,
        )?;
        self.validate_finite_close_birth(binding.open_file, &origin.birth)
    }
}
