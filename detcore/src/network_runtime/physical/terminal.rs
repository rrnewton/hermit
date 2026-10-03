//! Retain the original child owner through consuming cleanup and the distinct
//! actual final-wait observer. Neither order creates authority from absence.
use std::sync::Arc;

use super::super::native_birth_outcome::NativeTaskProjection;
use super::*;

impl<T> CustodyTasks<T> {
    pub(in crate::network_runtime) fn forget_shared_child(
        &mut self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<()> {
        let Some(task) = self.tasks.get_mut(&owner.thread) else {
            return Ok(());
        };
        if task.mm != owner.mm {
            return Err(std::io::Error::other("shared cleanup changed original MM"));
        }
        let Some(root) = &task.foreground_root else {
            self.forget(owner);
            return Ok(());
        };
        if !root.is_shared_child() {
            return self.forget_shared_initial(owner);
        }
        root.revoke();
        task.retired = true;
        task.shared_cleanup_requested = true;
        self.retire_completed_shared_child(owner)
    }

    /// The caller holds scheduler -> physical and owns the actual callback.
    /// Keep the Task until all existing terminal observers have run.
    pub(in crate::network_runtime) fn prepare_shared_final_wait(
        &mut self,
        owner: NetworkStreamOwner,
        projection: &Arc<NativeTaskProjection>,
    ) -> std::io::Result<Arc<ForegroundRoot>> {
        let bad = || std::io::Error::other("actual final wait lacks original shared child custody");
        let root = if let Some(task) = self.tasks.get(&owner.thread) {
            let root = task.foreground_root.as_ref().ok_or_else(bad)?;
            let birth = task.native_birth.as_ref().ok_or_else(bad)?;
            let raw = birth.raw();
            if task.mm != owner.mm
                || task.process != root.process()
                || task.thread != root.thread()
                || birth.child_owner() != owner
                || raw.shared_mm != 1
                || raw.shared_files != 1
                || raw.same_thread_group != 1
                || birth.terminal()
                || (
                    raw.provider,
                    raw.child_task,
                    raw.child_start,
                    raw.child_table,
                ) != root.native_identity()
            {
                return Err(bad());
            }
            root.clone()
        } else {
            // Identical repeat after explicit retirement observes an existing
            // positive fact; absence cannot create the first one.
            projection.final_wait_root(owner)?.clone()
        };
        let initial = root.initial_ancestor();
        let parent = self.tasks.get(&initial.owner().thread).ok_or_else(bad)?;
        if self.foreground_lineage_lost
            || !root.is_shared_child()
            || !root.has_shared_mm_history()
            || parent.retired
            || parent.mm != initial.owner().mm
            || !parent
                .foreground_root
                .as_ref()
                .is_some_and(|actual| std::ptr::eq(actual.as_ref(), initial))
            || !initial.is_current(initial.owner())
            || projection.is_initial()
            || projection.process() != root.logical_process()
            || !projection.matches_foreground_identity(owner, root.native_identity())
        {
            return Err(bad());
        }
        self.close_native_preparations(owner)?;
        Ok(root)
    }

    pub(in crate::network_runtime) fn retain_shared_final_wait(
        &mut self,
        owner: NetworkStreamOwner,
        projection: Arc<NativeTaskProjection>,
        root: Arc<ForegroundRoot>,
    ) -> std::io::Result<()> {
        projection.retain_final_wait(owner, root)?;
        if let Some(task) = self.tasks.get_mut(&owner.thread) {
            if task
                .shared_terminal
                .as_ref()
                .is_some_and(|old| !Arc::ptr_eq(old, &projection))
            {
                return Err(std::io::Error::other(
                    "final wait replaced historical projection owner",
                ));
            }
            task.shared_terminal = Some(projection);
        }
        Ok(())
    }

    pub(in crate::network_runtime) fn finish_shared_final_observations(
        &mut self,
        owner: NetworkStreamOwner,
        projection: &Arc<NativeTaskProjection>,
    ) -> std::io::Result<()> {
        if projection.is_initial() {
            return self.finish_shared_initial_observations(owner, projection);
        }
        let root = projection.final_wait_root(owner)?;
        if let Some(task) = self.tasks.get(&owner.thread)
            && (task.mm != owner.mm
                || !task.retired
                || !task
                    .foreground_root
                    .as_ref()
                    .is_some_and(|old| Arc::ptr_eq(old, root))
                || !task
                    .shared_terminal
                    .as_ref()
                    .is_some_and(|old| Arc::ptr_eq(old, projection)))
        {
            return Err(std::io::Error::other(
                "terminal observers lost original physical owner",
            ));
        }
        projection.finish_final_observations(owner, root)?;
        self.retire_completed_shared_child(owner)
    }

    pub(super) fn retire_completed_shared_child(&mut self, owner: NetworkStreamOwner) -> std::io::Result<()> {
        let Some(task) = self.tasks.get(&owner.thread) else {
            return Ok(());
        };
        if task.mm != owner.mm {
            return Err(std::io::Error::other("terminal retirement changed MM"));
        }
        let ready = task.shared_cleanup_requested
            && task.enrollment.as_ref().is_none_or(|e| !e.unresolved())
            && task.shared_terminal.as_ref().is_some_and(|p| {
                task.foreground_root
                    .as_ref()
                    .is_some_and(|root| p.final_observations_complete(owner, root))
            });
        if ready {
            self.tasks.remove(&owner.thread);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (
        super::super::foreground::policy_tests::SharedBirthFixture,
        crate::scheduler::Scheduler,
        Arc<NativeTaskProjection>,
    ) {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let mut scheduler = crate::scheduler::Scheduler::new(&crate::config::Config::default());
        let f = ForegroundRoot::controlled_shared_birth_after_close_setup(raw, |root, _| {
            scheduler.controlled_foreground_store_grant(root);
        })
        .await;
        scheduler.controlled_shared_birth_census(&f.parent, &f.child, &f._birth);
        let p = scheduler
            .shared_terminal_projection(f.child.owner(), f.parent.logical_process())
            .unwrap()
            .unwrap();
        (f, scheduler, p)
    }

    #[tokio::test]
    async fn shared_terminal_retains_original_owner_until_both_boundaries() {
        for cleanup_first in [false, true] {
            let (f, _scheduler, p) = fixture().await;
            let owner = f.child.owner();
            if cleanup_first {
                f.runtime.forget_shared_task(owner).unwrap();
            }
            assert!(
                f.runtime
                    .shared
                    .physical
                    .lock()
                    .unwrap()
                    .tasks
                    .contains_key(&owner.thread)
            );
            f.runtime
                .native_shared_child_terminal(owner, p.clone())
                .unwrap();
            assert!(
                f.runtime
                    .shared
                    .physical
                    .lock()
                    .unwrap()
                    .tasks
                    .contains_key(&owner.thread)
            );
            f.runtime
                .finish_shared_terminal_observations(owner, &p)
                .unwrap();
            assert_eq!(
                f.runtime
                    .shared
                    .physical
                    .lock()
                    .unwrap()
                    .tasks
                    .contains_key(&owner.thread),
                !cleanup_first
            );
            if !cleanup_first {
                f.runtime.forget_shared_task(owner).unwrap();
            }
            assert!(
                !f.runtime
                    .shared
                    .physical
                    .lock()
                    .unwrap()
                    .tasks
                    .contains_key(&owner.thread)
            );
            assert!(p.completed_final_wait(&f.parent).is_some());
            f.runtime
                .native_shared_child_terminal(owner, p.clone())
                .unwrap();
            f.runtime
                .finish_shared_terminal_observations(owner, &p)
                .unwrap();
            assert!(
                !f.runtime
                    .shared
                    .physical
                    .lock()
                    .unwrap()
                    .tasks
                    .contains_key(&owner.thread)
            );
        }
    }

    #[tokio::test]
    async fn shared_terminal_missing_or_changed_physical_issuer_never_mints_fact() {
        for fault in 0..7 {
            let (f, _scheduler, p) = fixture().await;
            let owner = f.child.owner();
            {
                let mut tasks = f.runtime.shared.physical.lock().unwrap();
                match fault {
                    0 => {
                        tasks.tasks.remove(&owner.thread);
                    }
                    1 => {
                        tasks.tasks.get_mut(&owner.thread).unwrap().mm =
                            owner.mm.for_exec(owner.thread);
                    }
                    2 => {
                        tasks.tasks.get_mut(&owner.thread).unwrap().process += 1;
                    }
                    3 => {
                        tasks.tasks.get_mut(&owner.thread).unwrap().foreground_root = None;
                    }
                    4 => {
                        tasks.tasks.get_mut(&owner.thread).unwrap().native_birth = None;
                    }
                    5 => {
                        tasks.forget(f.parent.owner());
                    }
                    6 => {
                        tasks.tasks.get_mut(&owner.thread).unwrap().foreground_root =
                            Some(f.parent.clone());
                    }
                    _ => unreachable!(),
                }
            }
            assert!(
                f.runtime
                    .native_shared_child_terminal(owner, p.clone())
                    .is_err()
            );
            assert!(p.final_wait_root(owner).is_err());
        }
    }
}
