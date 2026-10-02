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

#[test]
fn terminal_queries_keep_stdio_lineage_but_invalidate_arena_at_tool_callback() {
    use crate::fd::FdType;
    use crate::network_runtime::original_installation::FileIdentity;
    use crate::resources::Device;
    use crate::resources::ResourceID;

    for case in 0..17 {
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
        state.sched.lock().unwrap().thread_tree.add_child(owner.thread, owner.thread, true);
        install_test_registration(&state, owner.thread, Ivar::new());
        state.sched.lock().unwrap().install_test_exec_incarnation(owner.thread, owner.mm);
        state.registered_exec_mms.lock().unwrap().insert(owner.thread, owner.mm);
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            engine.register_initial_census(root.association(), &claim, owner.thread).unwrap();
        }
        tool.on_thread_state_ready(Tid::from_raw(raw), &state, &thread).unwrap();
        // Explicit component premises: a complete original stdio publication,
        // retained physical identity and actual stat profile. These are not
        // claimed as native ioctl or initial-census execution by this test.
        let mut request = libc::TCGETS;
        let mut stat = crate::stat::DetStat { mode: libc::S_IFREG | 0o600, ..Default::default() };
        let mut kind = FdType::Regular;
        match case {
            1 => request = libc::TIOCGWINSZ,
            2 => { kind = FdType::Pipe; stat.mode = libc::S_IFIFO | 0o600; }
            3 => {
                request = libc::TIOCGWINSZ;
                stat.mode = libc::S_IFCHR | 0o666;
                stat.rdev = libc::makedev(1, 3);
            }
            4 => request = libc::TCSETS,
            5 => request = libc::TIOCSWINSZ,
            6 => request = libc::FIONBIO,
            7 => request = libc::FIOCLEX,
            8 => request = libc::FIONCLEX,
            9 => request = libc::FIONREAD,
            10 => request = 0xffff_ffff,
            13 => { stat.mode = libc::S_IFCHR | 0o600; stat.rdev = libc::makedev(5, 0); }
            14 => { kind = FdType::Socket; stat.mode = libc::S_IFSOCK | 0o600; }
            _ => {}
        }
        {
            let mut actual = thread.file_metadata.lock().unwrap();
            let (mut next, change) = actual.prepare_original_installation_typed(
                owner.thread, 1, nix::fcntl::OFlag::empty(), kind, Some(stat),
            ).unwrap();
            let binding = change.after.unwrap().binding;
            if case != 12 {
                next.bind_native_installation(binding,
                    FileIdentity::controlled_fixture(root.native_identity().0, 37)).unwrap();
            }
            next.file_handles.get(&1).unwrap().set_resource(Some(ResourceID::Device(Device::ContainerStdout)));
            assert!(next.acknowledge_network_installations(&[change]));
            *actual = next;
            if case == 11 {
                // A later original allocation reuses fd1, but not its old OFD
                // or inherited stdio role. A fresh physical identity is not
                // sufficient to regain the old query exception.
                assert!(actual.remove_descriptor_binding(binding));
                let (mut next, change) = actual.prepare_original_installation_typed(
                    owner.thread, 1, nix::fcntl::OFlag::empty(), kind, Some(stat),
                ).unwrap();
                let replaced = change.after.unwrap().binding;
                assert_ne!(binding, replaced);
                next.bind_native_installation(replaced,
                    FileIdentity::controlled_fixture(root.native_identity().0, 38)).unwrap();
                assert!(next.acknowledge_network_installations(&[change]));
                *actual = next;
            }
            if case == 15 {
                assert!(actual.remove_descriptor_binding(binding));
            }
        }
        if case == 16 {
            let detached = thread.file_metadata.lock().unwrap().clone();
            thread.file_metadata = Arc::new(Mutex::new(detached));
        }
        let mmap = SyscallArgs::new(0, 4096, (libc::PROT_READ | libc::PROT_WRITE) as usize,
            (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize, -1isize as usize, 0);
        for event in [Event::Prepared, Event::Returned(0x8000)] {
            tool.on_injected_syscall_observed(Tid::from_raw(raw), &state, &mut thread,
                Sysno::mmap, mmap, event);
        }
        thread.memory_metadata.lock().unwrap().original_event_arena(owner, 0x8000).unwrap();
        let args = SyscallArgs::new(1, request as usize, 0x8000, 0, 0, 0);
        for event in [Event::Prepared, Event::Returned(-i64::from(libc::ENOTTY))] {
            tool.on_injected_syscall_observed(Tid::from_raw(raw), &state, &mut thread,
                Sysno::ioctl, args, event);
            assert_eq!(root.is_current(owner), case < 4, "case {case}, {event:?}");
            assert!(thread.memory_metadata.lock().unwrap().original_event_arena(owner, 0x8000).is_err(),
                "ioctl guest output may not retain an arena, case {case}");
            assert!(!state.sched.lock().unwrap().backend_failed(),
                "revocation cannot invent a syscall failure, case {case}");
        }
    }
}
