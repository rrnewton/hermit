//! Private common construction facts retained by the original birth reservation.
//! Neither serde nor a numeric PID lookup can issue this object. Birth-time
//! parent identity deliberately does not claim a current reparent relationship.
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use reverie::syscalls::CloneFlags;

use super::native_birth::NativeBirthAdmission;
use crate::network_replay::NetworkFdPublicationPermit;
use crate::network_replay::NetworkStreamOwner;
use crate::resources::ExternalOpId;
use crate::types::DetPid;
use crate::types::DetTid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeBirthRequest {
    pub owner: NetworkStreamOwner,
    pub process: DetPid,
    pub operation: ExternalOpId,
    pub flags: CloneFlags,
    pub child_tid_addr: usize,
    pub exit_signal: i32,
    pub priority_entropy: Option<u64>,
    pub permit: Option<NetworkFdPublicationPermit>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NativeProcessLifetime {
    local: DetPid,
    provider: u64,
    leader_task: u64,
    leader_start: u64,
}

/// Historical mapping issued by an actual held-generation admission. Retaining
/// this Arc does not retain a live task descriptor or a current parent claim.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NativeTaskProjection {
    provider: u64,
    task: u64,
    start: u64,
    thread: DetTid,
    process: Arc<NativeProcessLifetime>,
    initial: Option<super::physical::InitialProjectionIdentity>,
}
impl NativeTaskProjection {
    /// Only the scheduler's initial-census transaction calls this constructor;
    /// native task/start/provider identity comes from the private association.
    pub(crate) fn from_initial_root(
        association: &super::InitialTableAssociation,
        registration: &crate::scheduler::InitialRootRegistration<'_>,
    ) -> io::Result<Arc<Self>> {
        registration.check(association)?;
        let process = registration.process();
        let initial = association.root_projection_identity()?;
        let (provider, task, start) = association.native_root()?;
        Ok(Arc::new(Self {
            provider,
            task,
            start,
            thread: association.owner().thread,
            process: Arc::new(NativeProcessLifetime {
                local: process,
                provider,
                leader_task: task,
                leader_start: start,
            }),
            initial: Some(initial),
        }))
    }
    pub(crate) fn thread(&self) -> DetTid {
        self.thread
    }
    pub(crate) fn process(&self) -> DetPid {
        self.process.local
    }
    pub(crate) fn same_task(&self, other: &Self) -> bool {
        (self.provider, self.task, self.start) == (other.provider, other.task, other.start)
    }
    pub(crate) fn same_process(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.process, &other.process)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeBirthDisposition {
    Live,
    ExitedBeforeStart,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NativeChildOutcome {
    request: NativeBirthRequest,
    admission: NativeBirthAdmission,
    parent: Arc<NativeTaskProjection>,
    child: Arc<NativeTaskProjection>,
}
impl NativeChildOutcome {
    pub(crate) fn request(&self) -> &NativeBirthRequest {
        &self.request
    }
    pub(crate) fn admission(&self) -> &NativeBirthAdmission {
        &self.admission
    }
    pub(crate) fn flags(&self) -> CloneFlags {
        // Retain every kernel-accepted raw bit, including clone3's CLONE_NEWTIME.
        CloneFlags::from_bits_retain(self.admission.raw().kernel_flags)
    }
    pub(crate) fn child(&self) -> NetworkStreamOwner {
        self.admission.child_owner()
    }
    pub(crate) fn process(&self) -> DetPid {
        self.child.process()
    }
    pub(crate) fn clear_child_tid(&self) -> usize {
        self.admission.raw().clear_child_tid as usize
    }
    pub(crate) fn exit_signal(&self) -> i32 {
        self.admission.raw().exit_signal
    }
    pub(crate) fn parent(&self) -> &Arc<NativeTaskProjection> {
        &self.parent
    }
    pub(crate) fn projection(&self) -> &Arc<NativeTaskProjection> {
        &self.child
    }
}

#[derive(Debug, Default)]
struct Retained {
    projections: Vec<Arc<NativeTaskProjection>>,
    outcome: Option<Arc<NativeChildOutcome>>,
    consumed: Option<NativeBirthDisposition>,
    failed: bool,
}

/// One attachment on the existing NoSeq birth owner. The serializable request
/// remains unchanged; these authenticated process-local facts are serde-skipped.
#[derive(Debug)]
pub(crate) struct NativeBirthOwner {
    request: NativeBirthRequest,
    pub(crate) process_group: DetPid,
    pub(crate) session: DetPid,
    retained: Mutex<Retained>,
}
impl NativeBirthOwner {
    pub(crate) fn new(
        request: NativeBirthRequest,
        process_group: DetPid,
        session: DetPid,
        projections: Vec<Arc<NativeTaskProjection>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            request,
            process_group,
            session,
            retained: Mutex::new(Retained {
                projections,
                ..Retained::default()
            }),
        })
    }
    pub(crate) fn request(&self) -> &NativeBirthRequest {
        &self.request
    }
    pub(crate) fn validate_projection(&self, projection: &NativeTaskProjection) -> io::Result<()> {
        let state = self.retained.lock().unwrap();
        if state.outcome.is_none()
            && !state.failed
            && state
                .projections
                .iter()
                .any(|old| old.same_task(projection) && **old != *projection)
        {
            return Err(io::Error::other("historical task projection changed"));
        }
        Ok(())
    }
    pub(crate) fn retain_projection(
        &self,
        projection: Arc<NativeTaskProjection>,
    ) -> io::Result<()> {
        let mut state = self.retained.lock().unwrap();
        if state.outcome.is_some() || state.failed {
            return Ok(());
        }
        if let Some(old) = state
            .projections
            .iter()
            .find(|old| old.same_task(&projection))
        {
            if **old != *projection {
                return Err(io::Error::other("historical task projection changed"));
            }
        } else {
            state.projections.push(projection);
        }
        Ok(())
    }
    pub(crate) fn attach(
        &self,
        admission: NativeBirthAdmission,
    ) -> io::Result<Arc<NativeChildOutcome>> {
        if self.request.permit != Some(admission.permit())
            || self.request.flags != admission.flags()
        {
            return Err(io::Error::other(
                "native outcome changed the original request",
            ));
        }
        let mut state = self.retained.lock().unwrap();
        if state.failed {
            return Err(io::Error::other(
                "native child followed settled original failure",
            ));
        }
        if let Some(old) = &state.outcome {
            return if old.admission == admission {
                Ok(old.clone())
            } else {
                Err(io::Error::other(
                    "contradictory second native child outcome",
                ))
            };
        }
        let raw = admission.raw();
        if raw.clear_child_tid > usize::MAX as u64
            || (raw.kernel_flags & CloneFlags::CLONE_CHILD_CLEARTID.bits() == 0
                && raw.clear_child_tid != 0)
        {
            return Err(io::Error::other(
                "actual native clear-child-TID contradicts kernel flags",
            ));
        }
        let find = |task, start| {
            state
                .projections
                .iter()
                .find(|p| (p.provider, p.task, p.start) == (raw.provider, task, start))
                .cloned()
        };
        let creator = find(raw.creator_task, raw.creator_start).ok_or_else(|| {
            io::Error::other("native creator has no retained generation projection")
        })?;
        if creator.thread != self.request.owner.thread || creator.process() != self.request.process
        {
            return Err(io::Error::other("native creator changed logical lifetime"));
        }
        // Linux threads inherit the process's real_parent rather than naming
        // the thread that called clone. That external wait-parent is not a
        // Detcore task projection and does not own the thread's scheduler
        // lineage. The authenticated creator does. Process births still need
        // the exact chosen real_parent projection (including CLONE_PARENT).
        let parent = if raw.same_thread_group == 1 {
            creator.clone()
        } else {
            find(raw.parent_task, raw.parent_start).ok_or_else(|| {
                io::Error::other("native chosen parent remains unprojected; not Outside")
            })?
        };
        let process = if raw.same_thread_group == 1 {
            creator.process.clone()
        } else {
            Arc::new(NativeProcessLifetime {
                local: admission.child_process(),
                provider: raw.provider,
                leader_task: raw.child_task,
                leader_start: raw.child_start,
            })
        };
        let child = Arc::new(NativeTaskProjection {
            provider: raw.provider,
            task: raw.child_task,
            start: raw.child_start,
            thread: admission.child_owner().thread,
            process,
            initial: None,
        });
        let outcome = Arc::new(NativeChildOutcome {
            request: self.request.clone(),
            admission,
            parent,
            child,
        });
        state.outcome = Some(outcome.clone());
        // The immutable outcome retains only the matched actual lifetimes.
        state.projections.clear();
        Ok(outcome)
    }
    pub(crate) fn outcome(&self) -> io::Result<Arc<NativeChildOutcome>> {
        self.retained
            .lock()
            .unwrap()
            .outcome
            .clone()
            .ok_or_else(|| {
                io::Error::other("active native construction missing authenticated rebind")
            })
    }
    pub(crate) fn disposition(&self) -> Option<NativeBirthDisposition> {
        self.retained.lock().unwrap().consumed
    }
    pub(crate) fn consume(&self, disposition: NativeBirthDisposition) -> io::Result<()> {
        let mut state = self.retained.lock().unwrap();
        if state.failed
            || state.outcome.is_none()
            || state.consumed.is_some_and(|old| old != disposition)
        {
            return Err(io::Error::other("native semantic disposition changed"));
        }
        state.consumed = Some(disposition);
        Ok(())
    }
    pub(crate) fn settle_failure(&self, request: &NativeBirthRequest) -> io::Result<()> {
        let mut state = self.retained.lock().unwrap();
        if request != &self.request || state.outcome.is_some() || state.consumed.is_some() {
            return Err(io::Error::other(
                "failure changed original native request or observed child",
            ));
        }
        state.failed = true;
        state.projections.clear();
        Ok(())
    }
    pub(crate) fn complete(&self) -> bool {
        let state = self.retained.lock().unwrap();
        state.failed || state.consumed.is_some()
    }
}

#[cfg(test)]
pub(crate) fn synthetic_creator_projection() -> Arc<NativeTaskProjection> {
    Arc::new(NativeTaskProjection {
        provider: 3,
        task: (5001u64 << 32) | 5001,
        start: 29,
        thread: DetTid::from_raw(41),
        initial: None,
        process: Arc::new(NativeProcessLifetime {
            local: DetPid::from_raw(41),
            provider: 3,
            leader_task: (5001u64 << 32) | 5001,
            leader_start: 29,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::super::native_birth::synthetic_admission_for_common_consumer as proof;
    use super::*;
    fn owner(admission: &NativeBirthAdmission) -> Arc<NativeBirthOwner> {
        NativeBirthOwner::new(
            NativeBirthRequest {
                owner: admission.permit().owner,
                process: DetPid::from_raw(41),
                operation: ExternalOpId::new(DetTid::from_raw(41), 3),
                flags: admission.flags(),
                child_tid_addr: 0x1111,
                exit_signal: 17,
                priority_entropy: Some(91),
                permit: Some(admission.permit()),
            },
            DetPid::from_raw(41),
            DetPid::from_raw(41),
            vec![synthetic_creator_projection()],
        )
    }
    #[test]
    fn independent_actual_fields_preserve_every_original_request_field() {
        let actual = CloneFlags::CLONE_VM | CloneFlags::CLONE_CHILD_CLEARTID;
        let admission = proof(CloneFlags::empty(), actual, 0x223344, 12, false);
        let owner = owner(&admission);
        let original = owner.request().clone();
        let outcome = owner.attach(admission).unwrap();
        assert_eq!(outcome.request(), &original);
        assert_eq!(outcome.request().child_tid_addr, 0x1111);
        assert_eq!(outcome.request().exit_signal, 17);
        assert_eq!(outcome.flags(), actual);
        assert_eq!(outcome.clear_child_tid(), 0x223344);
        assert_eq!(outcome.exit_signal(), 12);
        assert_eq!(outcome.process(), DetPid::from_raw(42));
        assert_eq!(outcome.child().mm, outcome.request().owner.mm);
        assert_eq!(outcome.parent().thread(), DetTid::from_raw(41));
    }
    #[test]
    fn identical_outcome_reuses_arc_and_conflicting_second_outcome_fails() {
        let admission = proof(CloneFlags::empty(), CloneFlags::empty(), 0, 17, false);
        let owner = owner(&admission);
        let first = owner.attach(admission.clone()).unwrap();
        assert!(Arc::ptr_eq(&first, &owner.attach(admission).unwrap()));
        assert!(
            owner
                .attach(proof(
                    CloneFlags::empty(),
                    CloneFlags::empty(),
                    0,
                    12,
                    false
                ))
                .is_err()
        );
        assert!(owner.settle_failure(owner.request()).is_err());
        owner.consume(NativeBirthDisposition::Live).unwrap();
        owner.consume(NativeBirthDisposition::Live).unwrap();
        assert!(
            owner
                .consume(NativeBirthDisposition::ExitedBeforeStart)
                .is_err()
        );
    }
    #[test]
    fn thread_birth_uses_creator_not_external_real_parent_projection() {
        let flags = CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_FILES;
        let admission = proof(flags, flags, 0, -1, false);
        assert_ne!(
            admission.raw().parent_task,
            admission.raw().creator_task,
            "fixture must model Linux thread real_parent"
        );
        let owner = owner(&admission);
        let outcome = owner.attach(admission).unwrap();
        assert_eq!(outcome.parent().thread(), owner.request().owner.thread);
        assert_eq!(outcome.process(), owner.request().process);
    }
    #[test]
    fn exact_failure_cannot_be_relabelled_as_child_or_change_original_request() {
        let admission = proof(CloneFlags::empty(), CloneFlags::empty(), 0, 17, false);
        let owner = owner(&admission);
        let mut wrong = owner.request().clone();
        wrong.child_tid_addr += 1;
        assert!(owner.settle_failure(&wrong).is_err());
        owner.settle_failure(owner.request()).unwrap();
        assert!(owner.complete());
        assert!(owner.attach(admission).is_err());
    }
    #[test]
    fn missing_or_changed_parent_generation_is_unknown_not_outside() {
        let admission = proof(CloneFlags::empty(), CloneFlags::empty(), 0, 17, true);
        let owner = owner(&admission);
        let changed = Arc::new(NativeTaskProjection {
            provider: 4,
            ..(*synthetic_creator_projection()).clone_for_test()
        });
        let missing = NativeBirthOwner::new(
            owner.request().clone(),
            owner.process_group,
            owner.session,
            vec![changed],
        );
        assert!(missing.attach(admission).is_err());
        assert!(missing.outcome().is_err());
    }
    #[test]
    fn completed_birth_releases_unmatched_historical_projections() {
        let admission = proof(CloneFlags::empty(), CloneFlags::empty(), 0, 17, true);
        let owner = owner(&admission);
        let extra = Arc::new(NativeTaskProjection {
            task: (6001u64 << 32) | 6001,
            ..(*synthetic_creator_projection()).clone_for_test()
        });
        owner.retain_projection(extra.clone()).unwrap();
        assert_eq!(Arc::strong_count(&extra), 2);
        let outcome = owner.attach(admission).unwrap();
        assert_eq!(Arc::strong_count(&extra), 1);
        assert_eq!(outcome.parent().start, 29);
    }
    impl NativeTaskProjection {
        fn clone_for_test(&self) -> Self {
            Self {
                provider: self.provider,
                task: self.task,
                start: self.start,
                thread: self.thread,
                process: self.process.clone(),
                initial: self.initial.clone(),
            }
        }
    }
}
