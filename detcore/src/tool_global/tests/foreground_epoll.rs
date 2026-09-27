//! Actual Tool callback plumbing with explicit model observation premises.
//! This is not a native mmap or supported-guest qualification claim.
use reverie::InjectedSyscallEvent as Event;
use reverie::Tool;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;

use super::*;
#[test]
fn foreground_arena_tool_observer_issues_at_actual_return_and_revokes_before_lineage_effect() {
    let raw = std::process::id() as i32;
    let (runtime, root, metadata, memory, claim) =
        crate::network_runtime::controlled_foreground_runtime(raw);
    let owner = root.owner();
    let (config, mut state) = stream_rpc_state(true);
    state.network_runtime = Some(runtime);
    let tool: Detcore = Detcore::new(Tid::from_raw(raw), &config);
    let mut thread = tool.init_thread_state(Tid::from_raw(raw), None);
    thread.mm_id = owner.mm;
    thread.detpid = Some(owner.thread);
    thread.file_metadata = metadata;
    thread.memory_metadata = memory;
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(owner.thread, owner.thread, true);
    install_test_registration(&state, owner.thread, Ivar::new());
    state
        .sched
        .lock()
        .unwrap()
        .install_test_exec_incarnation(owner.thread, owner.mm);
    state
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(owner.thread, owner.mm);
    {
        let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        engine.fd_table_fixture_enable();
        engine
            .register_initial_census(root.association(), &claim, owner.thread)
            .unwrap();
    }
    tool.on_thread_state_ready(Tid::from_raw(raw), &state, &thread)
        .unwrap();
    let args = SyscallArgs::new(
        0,
        4096,
        (libc::PROT_READ | libc::PROT_WRITE) as usize,
        (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
        -1isize as usize,
        0,
    );
    tool.on_injected_syscall_observed(
        Tid::from_raw(raw),
        &state,
        &mut thread,
        Sysno::mmap,
        args,
        Event::Prepared,
    );
    assert!(
        thread
            .memory_metadata
            .lock()
            .unwrap()
            .original_event_arena(owner, 0x8000)
            .is_err()
    );
    tool.on_injected_syscall_observed(
        Tid::from_raw(raw),
        &state,
        &mut thread,
        Sysno::mmap,
        args,
        Event::Returned(0x8000),
    );
    thread
        .memory_metadata
        .lock()
        .unwrap()
        .original_event_arena(owner, 0x8000)
        .unwrap();
    assert!(!state.sched.lock().unwrap().backend_failed());
    tool.on_injected_syscall_observed(
        Tid::from_raw(raw),
        &state,
        &mut thread,
        Sysno::userfaultfd,
        SyscallArgs::new(0, 0, 0, 0, 0, 0),
        Event::Prepared,
    );
    assert!(!root.is_current(owner));
    assert!(
        thread
            .memory_metadata
            .lock()
            .unwrap()
            .original_event_arena(owner, 0x8000)
            .is_err()
    );
    assert!(
        !state.sched.lock().unwrap().backend_failed(),
        "revocation is not a fabricated syscall failure"
    );
}
