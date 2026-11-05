/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! LB7 uses real proc objects, sealed kernel carriers and ordinary children.
//! Counter restoration uses the actual in-guest clock and PerfCounter reader
//! with controlled kernel metadata as its input, rather than a second clock
//! implementation. Logical time is owned by a fixture Tool's actual ThreadState
//! and restored/read through ToolHost. This does not qualify Detcore, hardware
//! PMU delivery or a Hermit mode.

mod exec_support;

use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reverie::Error;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie_elf_loader::proc_state::AuxvSnapshot;
use reverie_elf_loader::proc_state::AuxvState;
use reverie_elf_loader::proc_state::ClockStateCarrier;
use reverie_elf_loader::proc_state::PinnedFinalElf;
use reverie_elf_loader::proc_state::ProcAliasResolver;
use reverie_elf_loader::proc_state::ProcStateError;
use reverie_elf_loader::proc_state::prepare_exec_continuation;
use reverie_inguest::guest::clock;
use reverie_inguest::guest::clock::RcbClockSnapshot;
use reverie_inguest::guest::event::SyscallDispatch;
use reverie_inguest::guest::event::SyscallEvent;
use reverie_inguest::guest::host::HostRuntime;
use reverie_inguest::guest::host::ToolHost;
use reverie_inguest::guest::rpc::CoordinatorRpc;
use reverie_inguest::trap::raw_syscall6;
use reverie_ptrace::InGuestRcbCounter;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
static FIXTURE_ROOT: OnceLock<PathBuf> = OnceLock::new();

fn fixture_root() -> &'static Path {
    FIXTURE_ROOT
        .get_or_init(|| exec_support::fixture_dir("lb7-artifacts"))
        .as_path()
}

fn artifact(name: &str) -> PathBuf {
    fixture_root().join(format!(
        "lb7-{name}-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn result(name: &str, details: &str) {
    fs::write(
        fixture_root().join(format!("lb7-{name}.result")),
        format!("PASS {name}\n{details}\n"),
    )
    .unwrap();
}

fn pin(path: impl AsRef<Path>) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(path)
        .unwrap()
}

fn bytes(words: &[u64]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

fn read_all(mut file: File) -> Vec<u8> {
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    data
}

fn assert_unsupported<T: std::fmt::Debug>(result: Result<T, ProcStateError>) {
    let error = result.unwrap_err();
    assert_eq!(error.name(), "InheritedVirtualProcStateUnsupported");
    assert!(matches!(
        error,
        ProcStateError::InheritedVirtualProcStateUnsupported { .. }
    ));
}

#[test]
fn lb7_auxv_snapshot_fresh_ofd_dup_cursor_and_seals() {
    let words = [3, 0x1122, 9, 0x3344, 31, 0x5566, 0, 0];
    let snapshot = AuxvSnapshot::new(41, &words).unwrap();
    assert_eq!(snapshot.words(), words);
    let seals = snapshot.seals().unwrap();
    let required = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    assert_eq!(seals & required, required);
    let expected = bytes(&words);
    let mut first = snapshot.open().unwrap();
    let mut duplicate = first.try_clone().unwrap();
    let mut prefix = [0; 11];
    first.read_exact(&mut prefix).unwrap();
    assert_eq!(prefix, expected[..11]);
    assert_eq!(duplicate.stream_position().unwrap(), 11);
    let mut next = [0; 7];
    duplicate.read_exact(&mut next).unwrap();
    assert_eq!(next, expected[11..18]);
    assert_eq!(first.stream_position().unwrap(), 18);
    assert_eq!(read_all(snapshot.open().unwrap()), expected);
    duplicate.seek(SeekFrom::Start(0)).unwrap();
    assert_eq!(first.stream_position().unwrap(), 0);
    assert_eq!(read_all(first), expected);

    // A writable reopen proves sealing independently of the public read-only
    // open flags. Omitting carrier seals would make this mutation succeed.
    let fd = snapshot.open().unwrap();
    let mut writer = OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .unwrap();
    assert_eq!(
        writer.write(&[1]).unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    assert_eq!(
        writer.set_len(0).unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    result(
        "auxv-ofd-seals",
        "real dup shares cursor; fresh OFD starts at zero; write/shrink EPERM",
    );
}

#[test]
fn lb7_auxv_old_ofd_retains_snapshot_after_modeled_exec() {
    let old_words = [3, 0x1234, 9, 0x5678, 0, 0];
    let new_words = [3, 0x9abc, 9, 0xdef0, 0, 0];
    let mut state = AuxvState::new(AuxvSnapshot::new(10, &old_words).unwrap());
    let mut old = state.open_current().unwrap();
    let mut old_dup = old.try_clone().unwrap();
    let mut prefix = [0; 13];
    old.read_exact(&mut prefix).unwrap();
    state
        .replace(AuxvSnapshot::new(11, &new_words).unwrap())
        .unwrap();
    assert_eq!(state.snapshot().mm_generation(), 11);
    assert_eq!(read_all(state.open_current().unwrap()), bytes(&new_words));
    let mut suffix = Vec::new();
    old_dup.read_to_end(&mut suffix).unwrap();
    assert_eq!(suffix, bytes(&old_words)[13..]);
    old.seek(SeekFrom::Start(0)).unwrap();
    assert_eq!(read_all(old_dup), bytes(&old_words));
    assert_unsupported(state.replace(AuxvSnapshot::new(11, &old_words).unwrap()));
    result(
        "auxv-mm-generation",
        "old OFD keeps generation 10 and shared cursor; new open reads generation 11",
    );
}

#[test]
fn lb7_proc_aliases_resolve_pinned_kernel_objects() {
    let resolver = ProcAliasResolver::for_current_task(21).unwrap();
    let task = resolver.task().clone();
    let state = AuxvState::new(AuxvSnapshot::new(21, &[3, 0x1111, 9, 0x2222, 0, 0]).unwrap());
    let self_alias = pin("/proc/self/auxv");
    let thread_alias = pin("/proc/thread-self/auxv");
    let task_alias = pin(format!("/proc/self/task/{}/auxv", task.tid));
    let parent = pin("/proc/self");
    // SAFETY: parent is an owned directory; auxv is a static C string.
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c"auxv".as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC,
        )
    };
    assert!(raw >= 0);
    // SAFETY: openat returned a new descriptor owned solely here.
    let relative = unsafe { File::from_raw_fd(raw) };
    let link = artifact("auxv-link");
    symlink("/proc/self/auxv", &link).unwrap();
    let symlink_alias = pin(&link);
    let fd_alias = pin(format!("/proc/self/fd/{}", self_alias.as_raw_fd()));
    let duplicate = thread_alias.try_clone().unwrap();
    for alias in [
        self_alias,
        thread_alias,
        task_alias,
        relative,
        symlink_alias,
        fd_alias,
        duplicate,
    ] {
        let resolved = resolver.resolve(&alias, &task).unwrap();
        assert_eq!(resolved.task, task);
        assert_eq!(
            read_all(state.open_alias(&resolver, &alias, &task).unwrap()),
            bytes(state.snapshot().words())
        );
    }
    fs::remove_file(link).unwrap();
    result(
        "proc-aliases",
        "self/thread-self/task-TID/dirfd/symlink/proc-fd/dup match registered kernel objects",
    );
}

#[test]
fn lb7_unregistered_proc_mount_task_and_mm_are_refused() {
    let resolver = ProcAliasResolver::for_current_task(22).unwrap();
    let task = resolver.task().clone();
    let auxv = pin("/proc/self/auxv");
    assert_unsupported(resolver.resolve(&pin("/proc/self/status"), &task));
    let fake = artifact("fake-proc-self-auxv");
    fs::write(&fake, b"not procfs").unwrap();
    assert_unsupported(resolver.resolve(&pin(&fake), &task));
    fs::remove_file(fake).unwrap();
    let mut stale = task.clone();
    stale.process_start_time += 1;
    assert_unsupported(resolver.resolve(&auxv, &stale));
    stale = task.clone();
    stale.mm_generation += 1;
    assert_unsupported(resolver.resolve(&auxv, &stale));
    let state = AuxvState::new(AuxvSnapshot::new(23, &[3, 1, 9, 2, 0, 0]).unwrap());
    assert_unsupported(state.open_alias(&resolver, &auxv, &task));
    for words in [&[][..], &[3, 1, 0][..], &[0, 0, 3, 1, 0, 0][..]] {
        assert_unsupported(AuxvSnapshot::new(1, words));
    }
    result(
        "proc-refusals",
        "unregistered endpoint/non-proc mount/stale task/stale mm refuse by design name",
    );
}

fn run_native_output(mut command: Command) -> Output {
    command.env("PYTHONDONTWRITEBYTECODE", "1");
    exec_support::run_monitored_output(
        command,
        "native proc control",
        Duration::from_secs(5),
        fixture_root(),
    )
}

fn run_native(command: Command) -> Output {
    let output = run_native_output(command);
    assert!(
        output.status.success(),
        "native child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

thread_local! {
    static COUNTER_METADATA_FD: Cell<i32> = const { Cell::new(-1) };
}

// Controlled perf input, not a hardware-counter claim. Linux 6.17's
// include/uapi/linux/perf_event.h:589-638 gives the metadata page's seqlock,
// index and offset layout. InGuestRcbCounter still constructs its real
// PerfCounter, maps this actual owned kernel memfd and reads it through
// PerfCounter::ctr_value_rdpmc, including the real seqlock protocol. There is
// no fixture implementation of guest-clock deduction, origin or restoration.
unsafe fn counter_metadata_gate(number: i64, arguments: [u64; 6]) -> i64 {
    if number == libc::SYS_perf_event_open {
        // SAFETY: the arguments contain a static name and valid memfd flags.
        let fd = unsafe {
            raw_syscall6(
                libc::SYS_memfd_create,
                [
                    c"LB7-controlled-perf-metadata".as_ptr() as u64,
                    libc::MFD_CLOEXEC as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            )
        };
        assert!(fd >= 0);
        // SAFETY: sysconf is a read-only query and the memfd is owned by this
        // constructor. The counter maps exactly one native metadata page.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(size > 0);
        assert_eq!(
            unsafe { raw_syscall6(libc::SYS_ftruncate, [fd as u64, size as u64, 0, 0, 0, 0],) },
            0
        );
        COUNTER_METADATA_FD.set(fd as i32);
        fd
    } else if number == libc::SYS_ioctl {
        // The fixture has no PMU. These are its counter constructor's RESET
        // and ENABLE requests; neither changes the controlled metadata.
        assert!(arguments[1] == 0x2403 || arguments[1] == 0x2400);
        0
    } else {
        assert!(matches!(
            number,
            libc::SYS_mmap | libc::SYS_munmap | libc::SYS_close | libc::SYS_read
        ));
        // SAFETY: the real counter supplies these raw ABI arguments and keeps
        // its owned descriptor and mapped page alive for the relevant calls.
        unsafe { raw_syscall6(number, arguments) }
    }
}

struct CounterSample {
    writer: File,
    sequence: u32,
    count: u64,
}

impl CounterSample {
    fn bind(count: u64) -> Self {
        COUNTER_METADATA_FD.set(-1);
        // SAFETY: this ordinary-context fixture gate remains callable for the
        // counter's lifetime and supplies controlled perf input on this thread.
        let counter =
            unsafe { InGuestRcbCounter::current_thread_with_syscall_gate(counter_metadata_gate) }
                .expect("the fixture needs a recognized CPU profile, not perf-event permissions");
        let fd = COUNTER_METADATA_FD.get();
        assert!(fd >= 0, "the real perf builder must invoke the input gate");
        // SAFETY: duplicate the still-owned metadata memfd, without taking the
        // counter's ownership. The fixture's writer is a separate owned slot.
        let writer = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        assert!(writer >= 0);
        // SAFETY: fcntl returned this new descriptor with sole ownership here.
        let writer = unsafe { File::from_raw_fd(writer) };
        // SAFETY: the counter was created on this thread; no instrumentation
        // or reentrant read is active while the binding is installed.
        unsafe { clock::initialize_rcb_clock_with_counter(counter) }.unwrap();
        let mut source = Self {
            writer,
            sequence: 0,
            count: 0,
        };
        source.set(count);
        // Zero offset/origin preserves the production read behavior exactly.
        assert_eq!(clock::read_guest_rcb_clock().unwrap(), count);
        source
    }

    fn set(&mut self, count: u64) {
        self.sequence += 2;
        self.writer
            .write_all_at(&(self.sequence - 1).to_le_bytes(), 8)
            .unwrap();
        self.writer
            .write_all_at(&(count as i64).to_le_bytes(), 16)
            .unwrap();
        self.writer
            .write_all_at(&self.sequence.to_le_bytes(), 8)
            .unwrap();
        self.count = count;
    }

    fn advance(&mut self, delta: u64) {
        self.set(self.count + delta);
    }
}

// The fixture Tool owns a small, ordinary Reverie ThreadState:
// [committed logical nanoseconds, last committed guest RCB count]. Its commit
// callback follows Detcore's ownership pattern (read_clock, account the delta,
// advance the owner's logical time, retain the committed RCB boundary). The
// actual ToolHost owns and restores this state; there is no test clock standing
// in for its state storage or Guest read path, and no claim to restore Detcore.
#[derive(Default)]
struct ClockOwner {
    initial: [u64; 2],
}

#[reverie::tool]
impl Tool for ClockOwner {
    type GlobalState = ();
    type ThreadState = [u64; 2];

    fn init_thread_state(
        &self,
        _tid: Pid,
        _parent: Option<(Pid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        self.initial
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        if syscall.number() == Sysno::getppid {
            let sampled = guest.read_clock()?;
            let state = guest.thread_state_mut();
            assert!(state[1] <= sampled, "owner RCB time cannot move backwards");
            state[0] += (sampled - state[1]) * NANOS_PER_OWNER_RCB;
            state[1] = sampled;
        }
        let state = guest.thread_state();
        let value = if syscall.number() == Sysno::gettid {
            state[1]
        } else {
            state[0]
        };
        Ok(i64::try_from(value).unwrap())
    }
}

struct OwnerRuntime;

impl HostRuntime for OwnerRuntime {
    fn fork_child_rebind(&self) {
        panic!("the clock fixture does not fork through the Tool")
    }

    fn emit_stage(&self, _stage: &[u8]) {}

    fn fork_child_reset(&self, _event: &SyscallEvent) {
        panic!("the clock fixture does not fork through the Tool")
    }

    fn cpuid_interception_enabled(&self) -> bool {
        false
    }

    fn exit_process_stats(&self, _tid: Pid) -> std::io::Result<()> {
        Ok(())
    }

    fn read_clock(&self) -> std::io::Result<u64> {
        clock::read_guest_rcb_clock()
    }

    fn signal_action_supported(&self, _number: i64, _args: [u64; 6]) -> bool {
        false
    }

    fn reserved_signal_mask(&self) -> u64 {
        0
    }
}

fn owner_host(initial: [u64; 2]) -> (ToolHost<ClockOwner, OwnerRuntime>, UnixStream) {
    // A retained directory supplies a short, CWD-independent AF_UNIX path.
    // The socket's real file remains under the atomically unique target fixture
    // directory; keep its directory descriptor alive through bind/connect/unlink.
    let directory = exec_support::fixture_dir("lb7-owner-rpc");
    let directory = File::open(directory).unwrap();
    let path = PathBuf::from(format!("/proc/self/fd/{}/rpc.sock", directory.as_raw_fd()));
    let listener = UnixListener::bind(&path).unwrap();
    let handshake = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        // The real CoordinatorRpc decoder receives the real () config frame.
        // The owner Tool sends no RPC: this peer supplies only that handshake.
        stream.write_all(&0_u32.to_be_bytes()).unwrap();
        stream
    });
    let rpc = CoordinatorRpc::connect(&path, |_old, _new| Ok(())).unwrap();
    let peer = handshake.join().unwrap();
    fs::remove_file(&path).unwrap();
    let host = ToolHost::new(
        ClockOwner { initial },
        rpc,
        Pid::from_raw(std::process::id() as i32),
        HashSet::from([Sysno::getpid, Sysno::getppid, Sysno::gettid]),
        false,
        OwnerRuntime,
    );
    (host, peer)
}

fn owner_call(host: &ToolHost<ClockOwner, OwnerRuntime>, number: i64) -> u64 {
    let mut event = SyscallEvent {
        number,
        args: [0; 6],
        instruction_pointer: 0,
        result: -1,
        context: 0,
        dispatch: SyscallDispatch::InstalledHook,
        guest_pkru: None,
    };
    // SAFETY: this event runs on the native calling thread with no register
    // context. The fixture Tool only reads/updates its own ThreadState and the
    // actual runtime clock; it does not inject a syscall or modify guest memory.
    unsafe { host.dispatch(&mut event) };
    u64::try_from(event.result).unwrap()
}

const CLOCK_CHILD_FD: &str = "REVERIE_LB7_CLOCK_CARRIER_FD";
const SAVED_LOGICAL_TIME: u64 = 42_731_099;
const NEW_COUNTER_ORIGIN: u64 = 50_491;
const NANOS_PER_OWNER_RCB: u64 = 10;
const CLOCK_COMPARATOR: &str = "LB7 counter trajectory must continue the saved committed count";
const LOGICAL_COMPARATOR: &str =
    "LB7 owner logical trajectory must continue the saved committed time";

fn run_clock_child(carrier: &ClockStateCarrier) -> Output {
    let fd = carrier.file().as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "lb7_clock_carrier_child", "--nocapture"])
        .env(CLOCK_CHILD_FD, fd.to_string());
    // SAFETY: pre_exec performs only fcntl on this owned carrier. Restoration
    // and the controlled counter reads execute in the child test entry, where
    // the parent can fail promptly and asynchronously clean up a timed-out child.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    run_native_output(command)
}

#[test]
fn lb7_clock_carrier_child() {
    let Ok(fd) = std::env::var(CLOCK_CHILD_FD) else {
        return;
    };
    let fd = fd.parse::<i32>().unwrap();
    // SAFETY: the parent transferred this owned descriptor to this native
    // child. No pathname is reopened to obtain the carrier.
    let carrier = unsafe { File::from_raw_fd(fd) };
    let restored = ClockStateCarrier::read(&carrier).unwrap();
    assert_eq!(restored.owner_state.len(), 24);
    let logical = u64::from_le_bytes(restored.owner_state[..8].try_into().unwrap());
    let committed = u64::from_le_bytes(restored.owner_state[8..16].try_into().unwrap());
    let old_sample = u64::from_le_bytes(restored.owner_state[16..24].try_into().unwrap());
    assert!(committed > 0);
    assert_ne!(old_sample, NEW_COUNTER_ORIGIN);

    let mut source = CounterSample::bind(NEW_COUNTER_ORIGIN);
    clock::restore_rcb_clock(RcbClockSnapshot {
        guest_count: restored.counter_offset,
    })
    .unwrap();
    let (owner, _peer) = owner_host([0; 2]);
    owner
        .restore_current_thread_state([logical, committed])
        .unwrap();
    // Read committed logical time from the actual restored ToolHost state,
    // through its Tool callback's Guest::thread_state path, not from the bytes.
    let mut logical_observed = vec![owner_call(&owner, libc::SYS_getpid)];
    let mut logical_expected = vec![SAVED_LOGICAL_TIME];
    let mut observed = vec![clock::read_guest_rcb_clock().unwrap()];
    let mut expected = vec![committed];
    let mut progress = 0;
    for delta in [1, 2, 17, 3, 29] {
        source.advance(delta);
        progress += delta;
        observed.push(clock::read_guest_rcb_clock().unwrap());
        expected.push(committed + progress);
    }
    clock::enter_rcb_handler().unwrap();
    source.advance(13);
    clock::enter_rcb_handler().unwrap();
    source.advance(4);
    observed.push(clock::read_guest_rcb_clock().unwrap());
    expected.push(committed + progress);
    clock::leave_rcb_handler().unwrap();
    clock::leave_rcb_handler().unwrap();
    observed.push(clock::read_guest_rcb_clock().unwrap());
    expected.push(committed + progress);
    source.advance(1);
    observed.push(clock::read_guest_rcb_clock().unwrap());
    expected.push(committed + progress + 1);

    // Both the genuine transfer and the serialized offset-omission mutation
    // run this same full-trajectory comparator. No failure is relabelled here.
    assert_eq!(observed, expected, "{CLOCK_COMPARATOR}");

    // Commit the RCB progress through the owner's actual Tool callback. Raw
    // clock reads alone do not advance committed logical time. Continuing with
    // individual deltas also checks the owner's accounting boundary, and both
    // active and completed nested handlers must remain excluded.
    let mut logical_progress = progress + 1;
    logical_observed.push(owner_call(&owner, libc::SYS_getppid));
    logical_expected.push(SAVED_LOGICAL_TIME + logical_progress * NANOS_PER_OWNER_RCB);
    for delta in [1, 2, 17, 3, 29] {
        source.advance(delta);
        logical_progress += delta;
        logical_observed.push(owner_call(&owner, libc::SYS_getppid));
        logical_expected.push(SAVED_LOGICAL_TIME + logical_progress * NANOS_PER_OWNER_RCB);
    }
    clock::enter_rcb_handler().unwrap();
    source.advance(13);
    clock::enter_rcb_handler().unwrap();
    source.advance(4);
    logical_observed.push(owner_call(&owner, libc::SYS_getppid));
    logical_expected.push(SAVED_LOGICAL_TIME + logical_progress * NANOS_PER_OWNER_RCB);
    clock::leave_rcb_handler().unwrap();
    clock::leave_rcb_handler().unwrap();
    logical_observed.push(owner_call(&owner, libc::SYS_getppid));
    logical_expected.push(SAVED_LOGICAL_TIME + logical_progress * NANOS_PER_OWNER_RCB);
    source.advance(1);
    logical_progress += 1;
    logical_observed.push(owner_call(&owner, libc::SYS_getppid));
    logical_expected.push(SAVED_LOGICAL_TIME + logical_progress * NANOS_PER_OWNER_RCB);

    // The logical-state omission child runs this identical owner comparator;
    // it still passes the unchanged counter comparator above.
    assert_eq!(logical_observed, logical_expected, "{LOGICAL_COMPARATOR}");
    assert_eq!(
        owner_call(&owner, libc::SYS_getpid),
        *logical_expected.last().unwrap()
    );
    assert_eq!(
        owner_call(&owner, libc::SYS_gettid),
        committed + logical_progress
    );
    assert_eq!(
        owner
            .restore_current_thread_state([0; 2])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        owner_call(&owner, libc::SYS_getpid),
        *logical_expected.last().unwrap()
    );
    assert_eq!(
        clock::restore_rcb_clock(RcbClockSnapshot { guest_count: 0 })
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    println!(
        "actual ToolHost logical trajectory={logical_observed:?}, saved committed RCB={committed}, old counter sample={old_sample}, restored counter origin={NEW_COUNTER_ORIGIN}, trajectory={observed:?}"
    );
}

#[test]
fn lb7_clock_carrier_restores_actual_runtime_reads_across_changed_origin() {
    let mut source = CounterSample::bind(19_301);
    clock::enter_rcb_handler().unwrap();
    source.advance(19);
    assert_eq!(clock::read_guest_rcb_clock().unwrap(), 19_301);
    clock::leave_rcb_handler().unwrap();
    source.advance(7);
    let (owner, _peer) = owner_host([SAVED_LOGICAL_TIME - 7 * NANOS_PER_OWNER_RCB, 19_301]);
    assert_eq!(owner_call(&owner, libc::SYS_getppid), SAVED_LOGICAL_TIME);
    let snapshot = clock::snapshot_rcb_clock().unwrap();
    assert_eq!(snapshot.guest_count, 19_308);
    let saved_logical = owner_call(&owner, libc::SYS_getpid);
    let saved_committed = owner_call(&owner, libc::SYS_gettid);
    assert_eq!(saved_logical, SAVED_LOGICAL_TIME);
    assert_eq!(saved_committed, snapshot.guest_count);
    let owner_state = bytes(&[saved_logical, saved_committed, source.count]);
    let carrier = ClockStateCarrier::new(snapshot.guest_count, &owner_state).unwrap();
    assert_eq!(
        ClockStateCarrier::read(carrier.file()).unwrap().owner_state,
        owner_state
    );
    let restored = run_clock_child(&carrier);
    assert!(
        restored.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&restored.stdout),
        String::from_utf8_lossy(&restored.stderr)
    );
    let actual = String::from_utf8_lossy(&restored.stdout);
    assert!(
        actual
            .contains("trajectory=[19308, 19309, 19311, 19328, 19331, 19360, 19360, 19360, 19361]")
    );
    assert!(actual.contains(
        "logical trajectory=[42731099, 42731629, 42731639, 42731659, 42731829, 42731859, 42732149, 42732149, 42732149, 42732159]"
    ));
    assert_eq!(clock::read_guest_rcb_clock().unwrap(), snapshot.guest_count);
    assert_eq!(owner_call(&owner, libc::SYS_getpid), saved_logical);

    // Mutation: omit the offset from the otherwise identical serialized
    // carrier. The successor restores the real API from these mutated bytes;
    // its logical/committed owner state and its comparator stay unchanged.
    let offset_omission = ClockStateCarrier::new(0, &owner_state).unwrap();
    let mutation = run_clock_child(&offset_omission);
    assert!(!mutation.status.success(), "offset omission must fail");
    let mutation_error = String::from_utf8_lossy(&mutation.stderr);
    assert!(
        mutation_error.contains(CLOCK_COMPARATOR),
        "{mutation_error}"
    );
    assert!(mutation_error.contains("left: [0, 1, 3, 20, 23, 52, 52, 52, 53]"));

    // Mutation: omit only the logical owner value in the sealed transfer. The
    // counter offset, last committed RCB boundary, restoration API and owner
    // callback remain identical. Its native successor must fail the owner read
    // trajectory assertion instead of a byte-decoding or constant check.
    let mut omitted_logical = owner_state.clone();
    omitted_logical[..8].copy_from_slice(&0_u64.to_le_bytes());
    let logical_omission = ClockStateCarrier::new(snapshot.guest_count, &omitted_logical).unwrap();
    let mutation = run_clock_child(&logical_omission);
    assert!(!mutation.status.success(), "logical omission must fail");
    let logical_mutation_error = String::from_utf8_lossy(&mutation.stderr);
    assert!(
        logical_mutation_error.contains(LOGICAL_COMPARATOR),
        "{logical_mutation_error}"
    );
    assert!(
        logical_mutation_error
            .contains("left: [0, 530, 540, 560, 730, 760, 1050, 1050, 1050, 1060]")
    );
    result(
        "clock-carrier-restoration",
        &format!(
            "actual Reverie clock snapshot/read/restore; controlled perf metadata, not hardware PMU mode qualification\nfixture Tool ThreadState restored through ToolHost::restore_current_thread_state; actual Guest::thread_state logical reads and Guest::thread_state_mut commits, not Detcore qualification\nlogical committed time={saved_logical}; saved committed/offset={}; old counter sample={}; new counter origin={NEW_COUNTER_ORIGIN}\n{actual}\noffset-omission native successor failed the unchanged full-trajectory comparator\n{mutation_error}\nlogical-omission native successor failed the unchanged owner logical trajectory comparator\n{logical_mutation_error}",
            snapshot.guest_count, source.count
        ),
    );
}

#[test]
fn lb7_owner_restoration_refuses_existing_thread_state_without_replacing_it() {
    let (owner, _peer) = owner_host([0; 2]);
    owner.restore_current_thread_state([37, 11]).unwrap();
    assert_eq!(owner_call(&owner, libc::SYS_getpid), 37);
    assert_eq!(owner_call(&owner, libc::SYS_gettid), 11);
    assert_eq!(
        owner
            .restore_current_thread_state([99, 22])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(owner_call(&owner, libc::SYS_getpid), 37);
    assert_eq!(owner_call(&owner, libc::SYS_gettid), 11);

    let (initialized, _peer) = owner_host([43, 13]);
    assert_eq!(owner_call(&initialized, libc::SYS_getpid), 43);
    assert_eq!(
        initialized
            .restore_current_thread_state([99, 22])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(owner_call(&initialized, libc::SYS_getpid), 43);
    assert_eq!(owner_call(&initialized, libc::SYS_gettid), 13);
}

#[test]
fn lb7_clock_carrier_rejects_mutability_and_invalid_extents() {
    assert_unsupported(ClockStateCarrier::new(3, &[0; 4096]));
    // SAFETY: the name is a static C string and this creates an owned memfd.
    let fd = unsafe {
        libc::memfd_create(
            c"LB7-unsealed-clock-carrier".as_ptr(),
            libc::MFD_ALLOW_SEALING,
        )
    };
    assert!(fd >= 0);
    // SAFETY: memfd_create returned this fresh descriptor with sole ownership.
    let file = unsafe { File::from_raw_fd(fd) };
    file.write_all_at(&[0; 24], 0).unwrap();
    assert_unsupported(ClockStateCarrier::read(&file));
    // SAFETY: F_ADD_SEALS changes only this owned memfd and needs a mask.
    assert_eq!(
        unsafe {
            libc::fcntl(
                fd,
                libc::F_ADD_SEALS,
                libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
            )
        },
        0
    );
    assert_unsupported(ClockStateCarrier::read(&file));
    let carrier = ClockStateCarrier::new(7, &[1, 2, 3]).unwrap();
    assert_eq!(
        carrier
            .file()
            .write_all_at(&[0; 8], 16)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
}

#[test]
fn lb7_restored_clock_preserves_active_handler_and_checks_overflow() {
    let mut source = CounterSample::bind(31);
    clock::enter_rcb_handler().unwrap();
    source.advance(9);
    clock::restore_rcb_clock(RcbClockSnapshot { guest_count: 73 }).unwrap();
    source.advance(17);
    assert_eq!(clock::read_guest_rcb_clock().unwrap(), 73);
    clock::leave_rcb_handler().unwrap();
    source.advance(1);
    assert_eq!(clock::read_guest_rcb_clock().unwrap(), 74);

    let mut source = CounterSample::bind(97);
    clock::restore_rcb_clock(RcbClockSnapshot {
        guest_count: u64::MAX,
    })
    .unwrap();
    assert_eq!(clock::read_guest_rcb_clock().unwrap(), u64::MAX);
    source.advance(1);
    assert_eq!(
        clock::read_guest_rcb_clock().unwrap_err().kind(),
        std::io::ErrorKind::Other
    );
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2));
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn auxv_value(data: &[u8], kind: u64) -> u64 {
    data.as_chunks::<16>()
        .0
        .iter()
        .find_map(|pair| {
            (u64::from_le_bytes(pair[..8].try_into().unwrap()) == kind)
                .then(|| u64::from_le_bytes(pair[8..].try_into().unwrap()))
        })
        .unwrap()
}

fn python_binary() -> PathBuf {
    // PATH python3 may be a process-launching wrapper on this host. Discover
    // the actual ELF, then exec it directly when PID/mm identity is observed.
    let mut discovery = Command::new("python3");
    discovery
        .arg("-B")
        .arg("-c")
        .arg("import sys; print(sys.executable)");
    let discovery = run_native(discovery);
    PathBuf::from(std::str::from_utf8(&discovery.stdout).unwrap().trim())
}

#[test]
fn lb7_native_exec_keeps_old_auxv_ofd_and_path_reopen_mutation_changes_mm() {
    let old = File::open("/proc/self/auxv").unwrap();
    let mut expected = [0; 8192];
    let count = old.read_at(&mut expected, 0).unwrap();
    let expected = &expected[..count];
    let mut inherited_dup = old.try_clone().unwrap();
    let fd = inherited_dup.as_raw_fd();
    // This owned slot is replaced only in the forked child with that child's
    // own pre-exec auxv OFD. Exec then detaches the very mm retained by it.
    let mut child_slot_owner = File::open("/proc/self/auxv").unwrap();
    let child_slot = child_slot_owner.as_raw_fd();
    let script = r#"import os,sys
fd=int(sys.argv[1])
child_fd=int(sys.argv[2])
child_dup=os.dup(child_fd)
old=os.read(fd,8192)
child_old=os.read(child_fd,8192)
new=open('/proc/self/auxv','rb').read()
parent_reopened=open('/proc/self/fd/'+str(fd),'rb').read()
child_reopened=open('/proc/self/fd/'+str(child_fd),'rb').read()
print(old.hex())
print(child_old.hex())
print(new.hex())
print(parent_reopened.hex())
print(child_reopened.hex())
print(os.lseek(child_dup,0,os.SEEK_CUR))
"#;
    let mut command = Command::new(python_binary());
    command
        .arg("-B")
        .arg("-c")
        .arg(script)
        .arg(fd.to_string())
        .arg(child_slot.to_string());
    // SAFETY: pre_exec uses only async-signal-safe open/dup2/close/fcntl with
    // static strings and owned descriptor slots. Both owners live through
    // completion; no guest or unrelated descriptor is overwritten.
    unsafe {
        command.pre_exec(move || {
            let child_auxv = libc::open(
                c"/proc/self/auxv".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            );
            if child_auxv < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(child_auxv, child_slot) < 0 {
                let error = std::io::Error::last_os_error();
                libc::close(child_auxv);
                return Err(error);
            }
            libc::close(child_auxv);
            for descriptor in [fd, child_slot] {
                let flags = libc::fcntl(descriptor, libc::F_GETFD);
                if flags < 0
                    || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let output = run_native(command);
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 6);
    let child_old = unhex(lines[0]);
    let own_child_old = unhex(lines[1]);
    let child_new = unhex(lines[2]);
    let parent_procfd_reopen = unhex(lines[3]);
    let path_reopen_mutation = unhex(lines[4]);
    assert_eq!(child_old, expected);
    assert_eq!(own_child_old, expected);
    assert_ne!(
        (auxv_value(&child_old, 3), auxv_value(&child_old, 9)),
        (auxv_value(&child_new, 3), auxv_value(&child_new, 9))
    );
    assert_eq!(path_reopen_mutation, child_new);
    assert_ne!(path_reopen_mutation, child_old);
    assert_eq!(parent_procfd_reopen, child_old);
    assert_eq!(lines[5].parse::<usize>().unwrap(), count);
    assert_eq!(inherited_dup.stream_position().unwrap(), count as u64);
    assert_eq!(child_slot_owner.stream_position().unwrap(), 0);
    result(
        "native-old-auxv-ofd",
        &format!(
            "old parent and child mm bytes={} preserved exactly; both dup cursors agree; new mm PHDR/ENTRY differ; child-object reopen reads new vector while parent object stays bound to parent\n{text}",
            count
        ),
    );
}

#[test]
fn lb7_native_script_final_elf_identity_and_original_execfn() {
    let python = python_binary();
    let script = artifact("original-script-F.py");
    let body = format!(
        "#!{} -B\nimport os,struct,ctypes\na=open('/proc/self/auxv','rb').read()\nv=dict(struct.iter_unpack('QQ',a))\nprint(ctypes.string_at(v[31]).hex())\ns=os.stat('/proc/self/exe')\nprint(str(s.st_dev)+':'+str(s.st_ino))\n",
        python.display()
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let final_elf = PinnedFinalElf::new(
        File::open(&python).unwrap(),
        CString::new(script.as_os_str().as_bytes()).unwrap(),
    )
    .unwrap();
    let metadata = final_elf.file().metadata().unwrap();
    let output = run_native(Command::new(&script));
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(unhex(lines[0]), final_elf.original_execfn().to_bytes());
    assert_eq!(lines[1], format!("{}:{}", metadata.dev(), metadata.ino()));
    assert_ne!(
        final_elf.original_execfn().to_bytes(),
        python.as_os_str().as_bytes()
    );
    result(
        "final-script-elf",
        "native proc exe inode matches pinned final ELF T; native AT_EXECFN matches original script F",
    );
}

#[test]
fn lb7_actual_clock_counter_continuation_is_explicitly_refused() {
    assert_unsupported(prepare_exec_continuation());
    result(
        "continuation-refusal",
        "InheritedVirtualProcStateUnsupported; the actual counter carrier fixture does not activate inherited proc/OFD or full Hermit Tool-state restoration",
    );
}
