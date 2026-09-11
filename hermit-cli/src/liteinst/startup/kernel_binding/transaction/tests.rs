use std::cell::RefCell;

use super::*;

fn metadata(inode: u64, mount: u64) -> libc::statx {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    stat.stx_mask = STAT_MASK;
    stat.stx_mode = 0o100755;
    stat.stx_dev_major = 8;
    stat.stx_dev_minor = 1;
    stat.stx_ino = inode;
    stat.stx_mnt_id = mount;
    stat.stx_size = 4096;
    stat.stx_mtime.tv_sec = 19;
    stat.stx_mtime.tv_nsec = 23;
    stat.stx_ctime.tv_sec = 29;
    stat.stx_ctime.tv_nsec = 31;
    stat
}

#[test]
fn codec_golden_and_bounds() {
    let mut bytes = [0; LIMIT];
    let runtime = metadata(101, 0);
    let alias = alias_metadata(91);
    let original = metadata(202, 81);
    let ns = ((8u64 << 32) | 1, 404);
    let parent = (ns.0, 303);
    let used = encode(
        &mut bytes,
        [10, 11, 12, 13, 14],
        [ns, parent],
        &runtime,
        &original,
        &alias,
        b"/lib/loader",
    )
    .unwrap();
    assert_eq!(used, HEADER + 12);
    assert_eq!(&bytes[..8], b"HLBIND02");
    assert_eq!(&bytes[152..160], &0u64.to_le_bytes());
    assert_eq!(&bytes[88..96], &81u64.to_le_bytes());
    assert_eq!(&bytes[96..104], &91u64.to_le_bytes());
    assert_eq!(&bytes[HEADER..used], b"/lib/loader\0");
    if let Some(path) = std::env::var_os("HERMIT_BINDING_GOLDEN_OUT") {
        std::fs::write(path, &bytes[..used]).unwrap();
    }
    for target in [b"".as_slice(), b"relative", b"/nul\0inside", &[b'/'; 4096]] {
        assert!(
            encode(
                &mut bytes,
                [10, 11, 12, 13, 14],
                [ns, parent],
                &runtime,
                &original,
                &alias,
                target
            )
            .is_err()
        );
    }
    for fds in [
        [10, 10, 12, 13, 14],
        [10, 11, 12, 2, 14],
        [-1, 11, 12, 13, 14],
        [10, 11, 12, 13, 10],
    ] {
        assert!(
            encode(
                &mut bytes,
                fds,
                [ns, parent],
                &runtime,
                &original,
                &alias,
                b"/loader"
            )
            .is_err()
        );
    }
    assert!(
        encode(
            &mut bytes,
            [10, 11, 12, 13, 14],
            [ns, ns],
            &runtime,
            &original,
            &alias,
            b"/loader"
        )
        .is_err()
    );
    assert!(
        encode(
            &mut bytes,
            [10, 11, 12, 13, 14],
            [ns, parent],
            &runtime,
            &original,
            &alias_metadata(81),
            b"/loader"
        )
        .is_err()
    );
}

#[derive(Default)]
struct State {
    cleanup_errors: Vec<i32>,
    alias_fds: [i32; 2],
    unlinked: bool,
    calls: Vec<i64>,
    isolated: bool,
    cloned: bool,
    copied: bool,
    bound: bool,
    detached: bool,
    bytes: Vec<u8>,
}
struct Model {
    state: RefCell<State>,
    fail: Option<i64>,
    fail_at: Option<usize>,
    cleanup_fail: bool,
    wrong_mount: bool,
    kernel_lookup: u8,
    plan_fds: [i32; 2],
}
impl Kernel for Model {
    fn cleanup_error(&self, error: &io::Error) {
        self.state
            .borrow_mut()
            .cleanup_errors
            .push(error.raw_os_error().unwrap());
    }
    unsafe fn call(&self, number: i64, args: [usize; 6]) -> i64 {
        let mut state = self.state.borrow_mut();
        state.calls.push(number);
        if self.fail == Some(number)
            || self.fail_at == Some(state.calls.len())
            || self.cleanup_fail && number == libc::SYS_umount2
        {
            return -i64::from(libc::EIO);
        }
        match number {
            libc::SYS_statx => {
                let path = unsafe { std::ffi::CStr::from_ptr(args[1] as *const _) }.to_bytes();
                let mut stat = if args[0] as i32 == state.alias_fds[0] && path == b"runtime" {
                    if state.unlinked {
                        return -i64::from(libc::ENOENT);
                    }
                    alias_metadata(17).link
                } else if args[0] as i32 == state.alias_fds[0] {
                    alias_metadata(17).root
                } else if args[0] as i32 == state.alias_fds[1] {
                    alias_metadata(91).link
                } else if args[0] == 10000 {
                    metadata(101, 0)
                } else if args[0] == 98 {
                    metadata(if state.cloned { 404 } else { 303 }, 7)
                } else if path == b"/lib/loader" && state.bound {
                    if args[2] == libc::AT_SYMLINK_NOFOLLOW as usize {
                        alias_metadata(91).link
                    } else {
                        metadata(101, 0)
                    }
                } else if path == b"loader-relative" && state.bound {
                    match self.kernel_lookup {
                        0 => metadata(101, 0),
                        1 => metadata(202, 81),
                        _ => metadata(101, 92),
                    }
                } else if args[0] as i32 == self.plan_fds[1] {
                    metadata(202, if state.copied { 81 } else { 51 })
                } else if path == b"/lib/loader" || path == b"/interp" || args[0] == 21 {
                    metadata(202, 51)
                } else if path == b"/program" || args[0] == 20 {
                    metadata(201, 51)
                } else {
                    metadata(101, 11)
                };
                if state.cloned && [b"/program".as_slice(), b"/interp", b"/runtime"].contains(&path)
                {
                    stat.stx_mnt_id += 30;
                }
                if path == b"/lib/loader" && state.cloned && !state.bound {
                    stat.stx_mnt_id = 81;
                }
                if self.wrong_mount && path == b"/interp" {
                    stat.stx_mnt_id = 52;
                }
                unsafe {
                    (args[4] as *mut libc::statx).write(stat);
                }
                0
            }
            libc::SYS_faccessat2 => 0,
            libc::SYS_fgetxattr => -i64::from(libc::ENODATA),
            libc::SYS_unshare => {
                assert_eq!(args[0], libc::CLONE_NEWNS as usize);
                state.cloned = true;
                0
            }
            libc::SYS_openat => {
                let path = unsafe { std::ffi::CStr::from_ptr(args[1] as *const _) }.to_bytes();
                if path == b"/proc/thread-self/ns/mnt" {
                    return 98;
                }
                assert!(state.cloned);
                99
            }
            libc::SYS_fstatfs => {
                let filesystem = args[1] as *mut libc::statfs;
                unsafe {
                    (*filesystem).f_type = 0x6e736673;
                }
                0
            }
            libc::SYS_ioctl => i64::from(libc::CLONE_NEWNS),
            libc::SYS_dup3 => {
                assert_eq!(args[0], 99);
                assert_eq!(args[1] as i32, self.plan_fds[1]);
                state.copied = true;
                0
            }
            libc::SYS_close => {
                assert!(args[0] == 98 || args[0] == 99);
                0
            }
            libc::SYS_mount => {
                if args[3] == (libc::MS_PRIVATE | libc::MS_REC) as usize {
                    assert_eq!(args[0], 0);
                    assert_eq!(args[2], 0);
                    assert_eq!(args[4], 0);
                    assert_eq!(args[5], 0);
                    assert_eq!(
                        unsafe { std::ffi::CStr::from_ptr(args[1] as *const _) }.to_bytes(),
                        b"/"
                    );
                    assert!(!state.cloned && !state.copied && !state.bound);
                    state.isolated = true;
                    return 0;
                }
                panic!("direct memfd bind must not be used");
            }
            libc::SYS_move_mount => {
                assert!(state.cloned && state.copied);
                assert_eq!(args[0] as i32, state.alias_fds[1]);
                assert_eq!(args[4], 4);
                state.bound = true;
                0
            }
            libc::SYS_readlinkat => {
                let text = b"/proc/self/fd/10000";
                unsafe {
                    std::ptr::copy_nonoverlapping(text.as_ptr(), args[2] as *mut u8, text.len());
                }
                text.len() as i64
            }
            libc::SYS_unlinkat => {
                assert!(!state.bound, "source must stay linked until restoration");
                state.unlinked = true;
                0
            }
            libc::SYS_umount2 => {
                assert!(state.bound);
                state.detached = true;
                state.bound = false;
                0
            }
            libc::SYS_pwrite64 => {
                assert!(state.bound);
                let bytes = unsafe { std::slice::from_raw_parts(args[1] as *const u8, args[2]) };
                state.bytes.extend_from_slice(bytes);
                args[2] as i64
            }
            libc::SYS_fcntl => 0,
            _ => panic!("unexpected modeled syscall {number}"),
        }
    }
}

fn fixture() -> Transaction {
    Transaction {
        record: descriptor(tempfile::tempfile().unwrap()).unwrap(),
        original: descriptor(File::open("/dev/null").unwrap()).unwrap(),
        target: CString::new("/lib/loader").unwrap(),
        interpreter_path: CString::new("/interp").unwrap(),
        kernel_interpreter_path: CString::new("loader-relative").unwrap(),
        program_path: CString::new("/program").unwrap(),
        source_path: CString::new("/runtime").unwrap(),
        runtime_path: CString::new("/proc/self/fd/10000").unwrap(),
        namespace: ((8u64 << 32) | 1, 303),
        runtime: 10000,
        original_image: 10001,
        source_fds: [20, 21, 22],
        expected: [metadata(201, 51), metadata(202, 51), metadata(101, 11)],
    }
}

fn run_binding(plan: &Transaction, model: &Model) -> io::Result<()> {
    let alias = fixture_alias(model);
    let snapshot = plan.enter_namespace(model)?;
    assert!(model.state.borrow().cloned);
    assert!(!model.state.borrow().bound);
    plan.bind(&snapshot, &alias, model)
}

fn alias_metadata(mount: u64) -> AliasIdentity {
    let mut root = metadata(606, 17);
    root.stx_mode = (libc::S_IFDIR | 0o700) as u16;
    let mut link = metadata(707, mount);
    link.stx_mode = (libc::S_IFLNK | 0o777) as u16;
    AliasIdentity { root, link }
}

fn fixture_alias(model: &Model) -> AliasSource {
    let root = descriptor(tempfile::tempfile().unwrap()).unwrap();
    let tree = descriptor(tempfile::tempfile().unwrap()).unwrap();
    model.state.borrow_mut().alias_fds = [root.as_raw_fd(), tree.as_raw_fd()];
    AliasSource {
        root,
        tree,
        identity: alias_metadata(17),
    }
}

#[test]
fn cleanup_retains_link_on_detach_failure_and_unlinks_only_after_restoration() {
    let plan = fixture();
    let mut model = Model {
        state: RefCell::new(State::default()),
        fail: None,
        fail_at: None,
        cleanup_fail: false,
        wrong_mount: false,
        kernel_lookup: 0,
        plan_fds: [plan.record_fd(), plan.original.as_raw_fd()],
    };
    let alias = fixture_alias(&model);
    let snapshot = plan.enter_namespace(&model).unwrap();
    plan.bind(&snapshot, &alias, &model).unwrap();
    model.cleanup_fail = true;
    assert_eq!(
        plan.cleanup(&snapshot, &alias, &model)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EIO)
    );
    assert!(model.state.borrow().bound && !model.state.borrow().unlinked);
    model.cleanup_fail = false;
    plan.cleanup(&snapshot, &alias, &model).unwrap();
    assert!(model.state.borrow().detached && model.state.borrow().unlinked);
    plan.cleanup(&snapshot, &alias, &model).unwrap();
}

#[test]
fn modeled_propagation_setup_requires_owned_namespace_and_preserves_identities() {
    let plan = fixture();
    let model = Model {
        state: RefCell::new(State::default()),
        fail: None,
        fail_at: None,
        cleanup_fail: false,
        wrong_mount: false,
        kernel_lookup: 0,
        plan_fds: [plan.record_fd(), plan.original.as_raw_fd()],
    };
    assert_eq!(
        isolate_mounts(&model, (plan.namespace.0, 999))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EXDEV)
    );
    assert!(!model.state.borrow().calls.contains(&libc::SYS_mount));
    let before = stat_fd(&model, plan.source_fds[1]).unwrap();
    isolate_mounts(&model, plan.namespace).unwrap();
    assert!(model.state.borrow().isolated);
    assert!(!model.state.borrow().cloned);
    let after = stat_fd(&model, plan.source_fds[1]).unwrap();
    assert!(unchanged(&before, &after));
    assert_eq!(before.stx_mnt_id, after.stx_mnt_id);
    let snapshot = plan.enter_namespace(&model).unwrap();
    assert_eq!(snapshot.paths[1].stx_mnt_id, 81);
    assert_eq!(stat_fd(&model, plan.source_fds[1]).unwrap().stx_mnt_id, 51);
    let alias = fixture_alias(&model);
    plan.bind(&snapshot, &alias, &model).unwrap();
    assert!(!model.state.borrow().detached);
}

#[test]
fn every_modeled_propagation_setup_failure_stops_before_cloning_or_binding() {
    let plan = fixture();
    let model = || Model {
        state: RefCell::new(State::default()),
        fail: None,
        fail_at: None,
        cleanup_fail: false,
        wrong_mount: false,
        kernel_lookup: 0,
        plan_fds: [plan.record_fd(), plan.original.as_raw_fd()],
    };
    let success = model();
    isolate_mounts(&success, plan.namespace).unwrap();
    let calls = success.state.borrow().calls.clone();
    assert_eq!(
        calls
            .iter()
            .filter(|number| **number == libc::SYS_mount)
            .count(),
        1
    );
    let mount = calls
        .iter()
        .position(|number| *number == libc::SYS_mount)
        .unwrap()
        + 1;
    for index in 1..=calls.len() {
        let mut failing = model();
        failing.fail_at = Some(index);
        let error = isolate_mounts(&failing, plan.namespace).unwrap_err();
        let state = failing.state.borrow();
        assert!(
            !state.cloned && !state.bound && !state.detached,
            "operation {index}"
        );
        assert_eq!(state.isolated, index > mount, "operation {index}");
        if index == mount {
            let message = error.to_string();
            assert!(message.contains("private propagation setup failed"));
            assert!(message.contains(&format!("namespace={:?}", plan.namespace)));
            assert!(message.contains("target=/ flags=MS_PRIVATE|MS_REC"));
        }
    }
}

#[test]
fn relative_interpreter_binding_uses_retained_guest_cwd_and_preserves_symlink_walk() {
    use crate::liteinst::startup::PinnedImage;
    use crate::liteinst::startup::lookup_path;
    let root = tempfile::tempdir().unwrap();
    let runner = root.path().join("runner");
    let guest = root.path().join("guest");
    std::fs::create_dir(&runner).unwrap();
    std::fs::create_dir_all(guest.join("tree/sub")).unwrap();
    std::fs::write(runner.join("loader"), b"ordinary held loader bytes").unwrap();
    std::fs::hard_link(runner.join("loader"), guest.join("loader")).unwrap();
    std::fs::hard_link(runner.join("loader"), guest.join("tree/loader")).unwrap();
    std::os::unix::fs::symlink("tree/sub", guest.join("alias")).unwrap();
    let mut command = crate::Command::new("not-executed");
    command.current_dir(&guest);
    let selected = PinnedImage::open_with_permissions(
        lookup_path(&command, std::path::Path::new("loader")).unwrap(),
        1024,
        false,
    )
    .unwrap();
    let wrong = PinnedImage::open_with_permissions(runner.join("loader"), 1024, false).unwrap();
    let selected_stat = stat_fd(&Native, selected.file().as_raw_fd()).unwrap();
    let wrong_stat = stat_fd(&Native, wrong.file().as_raw_fd()).unwrap();
    assert!(unchanged(&selected_stat, &wrong_stat));
    assert_eq!(selected_stat.stx_mnt_id, wrong_stat.stx_mnt_id);
    assert_ne!(
        binding_target(&selected).unwrap(),
        binding_target(&wrong).unwrap()
    );
    assert_eq!(
        binding_target(&selected).unwrap().as_bytes(),
        guest.join("loader").as_os_str().as_bytes()
    );
    let linked = PinnedImage::open_with_permissions(
        lookup_path(&command, std::path::Path::new("alias/../loader")).unwrap(),
        1024,
        false,
    )
    .unwrap();
    assert_eq!(
        binding_target(&linked).unwrap().as_bytes(),
        guest.join("tree/loader").as_os_str().as_bytes()
    );
}

#[test]
fn actual_child_interpreter_lookup_must_reach_runtime_and_exact_top_mount() {
    for kernel_lookup in [0, 1, 2] {
        let plan = fixture();
        let model = Model {
            state: RefCell::new(State::default()),
            fail: None,
            fail_at: None,
            cleanup_fail: false,
            wrong_mount: false,
            kernel_lookup,
            plan_fds: [plan.record_fd(), plan.original.as_raw_fd()],
        };
        assert_eq!(run_binding(&plan, &model).is_ok(), kernel_lookup == 0);
        let state = model.state.borrow();
        if kernel_lookup != 0 {
            assert!(state.detached && !state.bound);
            assert!(state.bytes.is_empty());
        }
    }
}

#[test]
fn modeled_binding_records_clone_mount_not_parent_and_rolls_back_failures() {
    for fail in [
        None,
        Some(libc::SYS_unshare),
        Some(libc::SYS_dup3),
        Some(libc::SYS_move_mount),
        Some(libc::SYS_pwrite64),
        Some(libc::SYS_fcntl),
    ] {
        let plan = fixture();
        let model = Model {
            state: RefCell::new(State::default()),
            fail,
            fail_at: None,
            cleanup_fail: false,
            wrong_mount: false,
            kernel_lookup: 0,
            plan_fds: [plan.record_fd(), plan.original.as_raw_fd()],
        };
        let result = run_binding(&plan, &model);
        assert_eq!(result.is_ok(), fail.is_none(), "{fail:?}: {result:?}");
        let state = model.state.borrow();
        if fail.is_none() {
            assert_eq!(&state.bytes[88..96], &81u64.to_le_bytes());
            assert_eq!(&state.bytes[96..104], &91u64.to_le_bytes());
            assert!(!state.detached);
        } else if matches!(fail, Some(libc::SYS_pwrite64 | libc::SYS_fcntl)) {
            assert!(state.detached);
        } else {
            assert!(!state.bound);
        }
    }
}

#[test]
fn every_modeled_producer_failure_rolls_back_or_precedes_binding() {
    let plan = fixture();
    let model = || Model {
        state: RefCell::new(State::default()),
        fail: None,
        fail_at: None,
        cleanup_fail: false,
        wrong_mount: false,
        kernel_lookup: 0,
        plan_fds: [plan.record_fd(), plan.original.as_raw_fd()],
    };
    let success = model();
    run_binding(&plan, &success).unwrap();
    for index in 1..=success.state.borrow().calls.len() {
        let mut failing = model();
        failing.fail_at = Some(index);
        assert!(run_binding(&plan, &failing).is_err(), "operation {index}");
        assert!(!failing.state.borrow().bound, "operation {index}");
    }
    let mut substituted = model();
    substituted.wrong_mount = true;
    assert_eq!(
        run_binding(&plan, &substituted).unwrap_err().raw_os_error(),
        Some(libc::ESTALE)
    );
    assert!(!substituted.state.borrow().cloned);
    let mut cleanup = model();
    cleanup.fail = Some(libc::SYS_pwrite64);
    cleanup.cleanup_fail = true;
    assert_eq!(
        run_binding(&plan, &cleanup).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(cleanup.state.borrow().cleanup_errors, [libc::EIO]);
    assert!(cleanup.state.borrow().bound && !cleanup.state.borrow().unlinked);
}

#[test]
fn modeled_final_namespace_precedes_binding_without_recloning_or_fd_translation() {
    let plan = fixture();
    let mut model = Model {
        state: RefCell::new(State::default()),
        fail: None,
        fail_at: None,
        cleanup_fail: false,
        wrong_mount: false,
        kernel_lookup: 0,
        plan_fds: [plan.record_fd(), plan.original.as_raw_fd()],
    };
    let snapshot = plan.enter_namespace(&model).unwrap();
    assert_eq!(snapshot.paths[1].stx_mnt_id, 81);
    assert_eq!(stat_fd(&model, plan.source_fds[1]).unwrap().stx_mnt_id, 51);
    assert_eq!(snapshot.original.stx_mnt_id, 81);
    assert!(!model.state.borrow().bound);
    let prepared_count = model.state.borrow().calls.len();
    let alias = fixture_alias(&model);
    plan.bind(&snapshot, &alias, &model).unwrap();
    assert!(!model.state.borrow().calls[prepared_count..].contains(&libc::SYS_unshare));
    model.state.borrow_mut().bound = false;
    model.wrong_mount = true;
    assert_eq!(
        plan.bind(&snapshot, &alias, &model)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ESTALE)
    );
    model.wrong_mount = false;
    model.state.borrow_mut().cloned = false;
    assert_eq!(
        plan.bind(&snapshot, &alias, &model)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EXDEV)
    );
}
