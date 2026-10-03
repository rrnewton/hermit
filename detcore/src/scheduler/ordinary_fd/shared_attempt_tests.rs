use super::*;

#[test]
fn shared_observation_requires_current_normal_gate_and_exact_physical_registration() {
    // Controlled census, actual pidfd registration and actual turn issuer.
    // This is not evidence that a backend peer has been physically stopped.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (runtime, root, _metadata, _memory, _claim) =
        crate::network_runtime::controlled_foreground_runtime(tid);
    let owner = root.owner();
    let mut scheduler = Scheduler::new(&crate::config::Config::default());
    assert!(
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                scheduler
                    .shared_mm_foreground_observation(owner, lineage)
                    .map(|_| ())
            })
            .is_err()
    );
    scheduler.controlled_foreground_store_grant(&root);
    runtime
        .with_shared_foreground_lineage(owner, |lineage| {
            let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
            assert_eq!(grant.owner(), owner);
            assert!(std::sync::Arc::ptr_eq(grant.root(), &root));
            Ok(())
        })
        .unwrap();
    scheduler
        .next_turns
        .get_mut(&owner.thread)
        .unwrap()
        .protocol
        .foreground_fd
        .as_mut()
        .unwrap()
        .resume = OrdinaryFdResume::SignalResume;
    assert!(
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                scheduler
                    .shared_mm_foreground_observation(owner, lineage)
                    .map(|_| ())
            })
            .is_err()
    );
    scheduler
        .next_turns
        .get_mut(&owner.thread)
        .unwrap()
        .protocol
        .foreground_fd
        .as_mut()
        .unwrap()
        .resume = OrdinaryFdResume::Normal;
    let registered = scheduler
        .physical_thread_pidfds
        .get_mut(&owner.thread)
        .unwrap();
    registered.0 = owner.mm.for_exec(owner.thread);
    assert!(
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                scheduler
                    .shared_mm_foreground_observation(owner, lineage)
                    .map(|_| ())
            })
            .is_err()
    );
    scheduler
        .physical_thread_pidfds
        .get_mut(&owner.thread)
        .unwrap()
        .0 = owner.mm;
    scheduler.next_turns.get_mut(&owner.thread).unwrap().req = Ivar::new();
    assert!(
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                scheduler
                    .shared_mm_foreground_observation(owner, lineage)
                    .map(|_| ())
            })
            .is_err()
    );
}
