//! Distinct finalizing-parent facts. The old live-parent child admission stays unchanged.
use super::*;
use crate::network_runtime::ForegroundRoot;

impl NativeTaskProjection {
    pub(crate) fn matches_initial_root(&self, root: &ForegroundRoot) -> bool {
        self.initial.as_ref().is_some_and(|identity| {
            root.association()
                .root_projection_identity()
                .is_ok_and(|actual| &actual == identity)
        }) && self.matches_foreground_identity(root.owner(), root.native_identity())
            && self.process() == root.logical_process()
            && std::ptr::eq(root.initial_ancestor(), root)
            && root.has_shared_mm_history()
    }

    /// Terminal history only: unlike completed_final_wait, this cannot authorize
    /// another guest operation after the initial parent has died.
    pub(crate) fn completed_child_for_initial_finalization(
        &self,
        initial: &ForegroundRoot,
    ) -> Option<&Arc<ForegroundRoot>> {
        let fact = self.final_wait.get()?;
        (!self.is_initial()
            && fact.initial_history.is_none()
            && fact.observations_complete.load(Ordering::Acquire)
            && self.matches_foreground_identity(fact.owner, fact.root.native_identity())
            && fact.root.has_shared_mm_history()
            && !fact.root.is_current(fact.owner)
            && std::ptr::eq(fact.root.initial_ancestor(), initial)
            && fact.root.same_shared_lineage(initial))
        .then_some(&fact.root)
    }

    pub(in crate::network_runtime) fn retain_initial_final_wait(
        &self,
        owner: NetworkStreamOwner,
        root: Arc<ForegroundRoot>,
        children: &[Arc<NativeTaskProjection>],
    ) -> io::Result<()> {
        if owner != root.owner()
            || !self.matches_initial_root(&root)
            || root.is_current(owner)
            || children.iter().any(|p| {
                !p.same_process(self) || p.completed_child_for_initial_finalization(&root).is_none()
            })
        {
            return Err(io::Error::other(
                "initial final wait lacks exact original terminal history",
            ));
        }
        let fact = self.final_wait.get_or_init(|| NativeTaskFinalWait {
            initial_history: Some(children.to_vec()),
            owner,
            root: root.clone(),
            observations_complete: AtomicBool::new(false),
        });
        if fact.owner != owner
            || !Arc::ptr_eq(&fact.root, &root)
            || fact.initial_history.as_ref().is_none_or(|old| {
                old.len() != children.len()
                    || old.iter().zip(children).any(|(a, b)| !Arc::ptr_eq(a, b))
            })
        {
            return Err(io::Error::other(
                "initial final wait replaced its retained history",
            ));
        }
        Ok(())
    }

    pub(crate) fn completed_initial_final_wait(&self, root: &Arc<ForegroundRoot>) -> bool {
        self.final_wait.get().is_some_and(|fact| {
            self.matches_initial_root(root)
                && fact.owner == root.owner()
                && Arc::ptr_eq(&fact.root, root)
                && !root.is_current(fact.owner)
                && fact.observations_complete.load(Ordering::Acquire)
                && fact.initial_history.as_ref().is_some_and(|children| {
                    children.iter().all(|p| {
                        p.same_process(self)
                            && p.completed_child_for_initial_finalization(root).is_some()
                    })
                })
        })
    }
}
