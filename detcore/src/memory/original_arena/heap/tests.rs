//! Component controls of the actual-observation issuer. These inputs are
//! controlled observations, not a substitute for a native guest/store test.
use super::*;

fn args(address: u64, length: u64) -> SyscallArgs {
    SyscallArgs::new(address as usize, length as usize, 0, 0, 0, 0)
}

fn observed(
    memory: &mut MemoryMetadata,
    root: &Arc<ForegroundRoot>,
    nr: Sysno,
    args: SyscallArgs,
    result: i64,
) {
    memory
        .observe_original_arena(root, nr, args, Event::Prepared)
        .unwrap();
    memory
        .observe_original_arena(root, nr, args, Event::Returned(result))
        .unwrap();
}

fn grow(memory: &mut MemoryMetadata, root: &Arc<ForegroundRoot>) {
    observed(memory, root, Sysno::brk, args(0, 0), 0x8000);
    observed(memory, root, Sysno::brk, args(0xb123, 0), 0xb123);
}

#[test]
fn heap_span_requires_native_growth_not_model_query_or_same_page_change() {
    let (root, _, memory, _) = crate::network_runtime::controlled_foreground_root(61);
    let mut memory = memory.lock().unwrap();
    memory.observe_brk(0x8000);
    memory.observe_brk(0xc000);
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    observed(&mut memory, &root, Sysno::brk, args(0, 0), 0x8123);
    observed(&mut memory, &root, Sysno::brk, args(0x8200, 0), 0x8200);
    assert!(memory.original_copy_span(root.owner(), 0x8200, 1).is_err());
    observed(&mut memory, &root, Sysno::brk, args(0xb123, 0), 0xb123);
    assert!(memory.original_copy_span(root.owner(), 0x8fff, 1).is_err());
    let span = memory.original_copy_span(root.owner(), 0xbff8, 8).unwrap();
    memory
        .validate_original_copy_span(root.owner(), &span)
        .unwrap();
    assert!(memory.original_copy_span(root.owner(), 0xbff8, 9).is_err());
    // A large logical receive capacity is not required to be backed: only the
    // selected prefix above was certified. The unused tail remains unproved.
    assert!(
        memory
            .original_copy_span(root.owner(), 0xbff8, 102400)
            .is_err()
    );
    assert!(memory.original_event_arena(root.owner(), 0xa000).is_err());
}

#[test]
fn heap_span_shrink_failure_and_every_transition_revoke_old_generation() {
    let (root, _, memory, _) = crate::network_runtime::controlled_foreground_root(61);
    let mut memory = memory.lock().unwrap();
    grow(&mut memory, &root);
    let old = memory
        .original_copy_span(root.owner(), 0x8000, 512)
        .unwrap();
    memory
        .observe_original_arena(&root, Sysno::brk, args(0x9123, 0), Event::Prepared)
        .unwrap();
    assert!(
        memory
            .validate_original_copy_span(root.owner(), &old)
            .is_err()
    );
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    memory
        .observe_original_arena(&root, Sysno::brk, args(0x9123, 0), Event::Returned(0x9123))
        .unwrap();
    memory.original_copy_span(root.owner(), 0x9fff, 1).unwrap();
    assert!(memory.original_copy_span(root.owner(), 0xa000, 1).is_err());
    assert!(
        memory
            .validate_original_copy_span(root.owner(), &old)
            .is_err()
    );
    // Genuine failed growth reports the old break and proves no new suffix.
    observed(&mut memory, &root, Sysno::brk, args(0x10000, 0), 0x9123);
    memory.original_copy_span(root.owner(), 0x8000, 1).unwrap();
    assert!(memory.original_copy_span(root.owner(), 0xa000, 1).is_err());
    observed(&mut memory, &root, Sysno::brk, args(0x8000, 0), 0x9123);
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    observed(&mut memory, &root, Sysno::brk, args(0, 0), 0x9123);
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
}

#[test]
fn heap_span_disjoint_vm_preserves_geometry_not_old_capabilities() {
    let (root, _, memory, _) = crate::network_runtime::controlled_foreground_root(61);
    let mut memory = memory.lock().unwrap();
    grow(&mut memory, &root);
    for nr in [
        Sysno::munmap,
        Sysno::mprotect,
        Sysno::pkey_mprotect,
        Sysno::madvise,
    ] {
        let old = memory.original_copy_span(root.owner(), 0x8000, 8).unwrap();
        observed(&mut memory, &root, nr, args(0x20000, 4096), 0);
        memory.original_copy_span(root.owner(), 0x8000, 8).unwrap();
        assert!(
            memory
                .validate_original_copy_span(root.owner(), &old)
                .is_err()
        );
    }
    let mapping = SyscallArgs::new(
        0,
        4096,
        libc::PROT_READ as usize,
        libc::MAP_PRIVATE as usize,
        7,
        0,
    );
    observed(&mut memory, &root, Sysno::mmap, mapping, 0x20000);
    memory
        .original_copy_span(root.owner(), 0x8000, 512)
        .unwrap();
    // Partial mprotect failure is not proof of no effects on overlapping VMAs.
    observed(
        &mut memory,
        &root,
        Sysno::mprotect,
        args(0x9000, 4096),
        -i64::from(libc::ENOMEM),
    );
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    observed(&mut memory, &root, Sysno::brk, args(0, 0), 0xb123);
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    observed(&mut memory, &root, Sysno::brk, args(0xd000, 0), 0xd000);
    memory
        .original_copy_span(root.owner(), 0xc000, 512)
        .unwrap();
    assert!(memory.original_copy_span(root.owner(), 0xbfff, 1).is_err());
}

#[test]
fn heap_span_fixed_overlap_overflow_and_untracked_vm_never_resurrect() {
    for (nr, mut operation) in [
        (
            Sysno::mmap,
            SyscallArgs::new(0x9000, 4096, 3, libc::MAP_FIXED as usize, 7, 0),
        ),
        (Sysno::munmap, args(0x7fff, 2)),
        (Sysno::mprotect, args(u64::MAX - 1, 4096)),
        (Sysno::mremap, args(0x20000, 4096)),
        (Sysno::remap_file_pages, args(0x20000, 4096)),
    ] {
        let (root, _, memory, _) = crate::network_runtime::controlled_foreground_root(61);
        let mut memory = memory.lock().unwrap();
        grow(&mut memory, &root);
        let old = memory.original_copy_span(root.owner(), 0x8000, 8).unwrap();
        memory
            .observe_original_arena(&root, nr, operation, Event::Prepared)
            .unwrap();
        assert!(
            memory
                .validate_original_copy_span(root.owner(), &old)
                .is_err(),
            "{nr:?}"
        );
        memory
            .observe_original_arena(
                &root,
                nr,
                operation,
                Event::Returned(-i64::from(libc::EINVAL)),
            )
            .unwrap();
        assert!(
            memory.original_copy_span(root.owner(), 0x8000, 1).is_err(),
            "{nr:?}"
        );
        operation = args(0, 0);
        observed(&mut memory, &root, Sysno::brk, operation, 0xb123);
        assert!(
            memory.original_copy_span(root.owner(), 0x8000, 1).is_err(),
            "{nr:?}"
        );
    }
}

#[test]
fn heap_span_qualified_query_requires_matched_return_and_unknown_ioctl_revokes() {
    let (root, _, memory, _) = crate::network_runtime::controlled_foreground_root(61);
    let mut memory = memory.lock().unwrap();
    grow(&mut memory, &root);
    let old = memory.original_copy_span(root.owner(), 0x8000, 8).unwrap();
    let query = SyscallArgs::new(1, libc::TCGETS as usize, 0x9000, 0, 0, 0);
    // Controlled classification input: the production caller additionally
    // requires the existing typed query + actual supported stdio OFD checks.
    memory
        .observe_original_memory_operation(&root, Sysno::ioctl, query, Event::Prepared, true)
        .unwrap();
    assert!(
        memory
            .validate_original_copy_span(root.owner(), &old)
            .is_err()
    );
    assert!(memory.original_copy_span(root.owner(), 0x8000, 8).is_err());
    memory
        .observe_original_arena(
            &root,
            Sysno::ioctl,
            query,
            Event::Returned(-i64::from(libc::ENOTTY)),
        )
        .unwrap();
    memory.original_copy_span(root.owner(), 0x8000, 8).unwrap();
    assert!(
        memory
            .validate_original_copy_span(root.owner(), &old)
            .is_err()
    );
    observed(
        &mut memory,
        &root,
        Sysno::ioctl,
        query,
        -i64::from(libc::ENOTTY),
    );
    assert!(memory.original_copy_span(root.owner(), 0x8000, 8).is_err());
}

#[test]
fn heap_span_refuses_missing_changed_or_interrupted_native_cause_and_inheritance() {
    let (root, _, memory, _) = crate::network_runtime::controlled_foreground_root(61);
    let (foreign, _, _, _) = crate::network_runtime::controlled_foreground_root(62);
    let mut memory = memory.lock().unwrap();
    memory
        .observe_original_arena(&root, Sysno::brk, args(0xb123, 0), Event::Returned(0xb123))
        .unwrap();
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    grow(&mut memory, &root);
    assert!(
        memory
            .clone()
            .original_copy_span(root.owner(), 0x8000, 1)
            .is_err()
    );
    let serialized = serde_json::to_vec(&*memory).unwrap();
    let decoded: MemoryMetadata = serde_json::from_slice(&serialized).unwrap();
    assert!(decoded.original_copy_span(root.owner(), 0x8000, 1).is_err());
    assert!(
        memory
            .original_copy_span(foreign.owner(), 0x8000, 1)
            .is_err()
    );
    memory
        .observe_original_arena(&root, Sysno::brk, args(0xc000, 0), Event::Prepared)
        .unwrap();
    assert!(
        memory
            .observe_original_arena(&root, Sysno::brk, args(0xd000, 0), Event::Returned(0xc000))
            .is_err()
    );
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
    grow(&mut memory, &root);
    memory
        .observe_original_arena(&root, Sysno::brk, args(0xc000, 0), Event::Prepared)
        .unwrap();
    memory
        .observe_original_arena(
            &root,
            Sysno::brk,
            args(0xc000, 0),
            Event::InterruptedBeforeEntry,
        )
        .unwrap();
    assert!(memory.original_copy_span(root.owner(), 0x8000, 1).is_err());
}
