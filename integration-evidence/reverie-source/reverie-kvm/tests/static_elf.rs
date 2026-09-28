/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![cfg(target_arch = "x86_64")]

use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Barrier;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use kvm_ioctls::Kvm;
use reverie::BackendChildWaitEvent;
use reverie::BackendChildWaitState;
use reverie::BackendStatsRequest;
use reverie::BackendStatsSource;
use reverie::ExitStatus;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::Subscription;
use reverie::ThreadOwnership;
use reverie::Tool;
use reverie::syscalls::CArrayPtr;
use reverie::syscalls::CStrPtr;
use reverie::syscalls::Errno;
use reverie::syscalls::Execve;
use reverie::syscalls::ExitGroup;
use reverie::syscalls::Fork;
use reverie::syscalls::FromToRaw;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use reverie_kvm::CounterTool;
use reverie_kvm::Error;
use reverie_kvm::HierarchicalCounterTool;
use reverie_kvm::HierarchicalTotals;
use reverie_kvm::KvmBackend;
use reverie_kvm::KvmBackendStats;
use reverie_kvm::KvmExitReason;
use reverie_kvm::StraceTool;

const MEMORY_SIZE: usize = 16 * 1024 * 1024;
const LOAD_ADDRESS: u64 = 0x20_0000;
const CODE_OFFSET: usize = 0x1000;
const POST_EXEC_RANDOM: [u8; 16] = *b"kvm-post-exec-ok";
static POST_EXEC_FAILURE_EXITED: AtomicBool = AtomicBool::new(false);

static NEXT_TEST_EXECUTABLE: AtomicU64 = AtomicU64::new(0);

struct TestExecutable(PathBuf);

impl TestExecutable {
    fn new(image: &[u8]) -> Self {
        let id = NEXT_TEST_EXECUTABLE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("reverie-kvm-exec-{}-{id}", std::process::id()));
        std::fs::write(&path, image).unwrap();
        Self(path)
    }
}

impl Drop for TestExecutable {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_TEST_EXECUTABLE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("reverie-kvm-coreutils-{}-{id}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn run_host_program_captured(
    program: &str,
    argv: &[&str],
    cwd: &std::path::Path,
) -> (Vec<u8>, Vec<u8>) {
    const REAL_PROGRAM_MEMORY_SIZE: usize = 256 * 1024 * 1024;

    let image = std::fs::read(program).unwrap();
    let mut backend = KvmBackend::new(REAL_PROGRAM_MEMORY_SIZE).unwrap();
    backend
        .install_static_elf_with_context(&image, argv, &["PATH=/usr/bin:/bin"], cwd)
        .unwrap();
    let (code, stdout, stderr) = backend.run_static_elf_captured().unwrap();
    assert_eq!(
        code,
        0,
        "{program} {argv:?} exited {code}; stdout={}; stderr={}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );
    (stdout, stderr)
}

fn run_host_program_with_tool_captured(
    program: &str,
    argv: &[&str],
    cwd: &std::path::Path,
) -> (Vec<u8>, Vec<u8>) {
    const REAL_PROGRAM_MEMORY_SIZE: usize = 256 * 1024 * 1024;

    let image = std::fs::read(program).unwrap();
    let mut backend = KvmBackend::new(REAL_PROGRAM_MEMORY_SIZE).unwrap();
    backend
        .install_static_elf_with_context(&image, argv, &["PATH=/usr/bin:/bin"], cwd)
        .unwrap();
    let (_, code, stdout, stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();
    assert_eq!(
        code,
        0,
        "{program} {argv:?} exited {code}; stdout={}; stderr={}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );
    (stdout, stderr)
}

fn run_host_program(program: &str, argv: &[&str], cwd: &std::path::Path) {
    let _ = run_host_program_captured(program, argv, cwd);
}

fn compile_c_program(directory: &std::path::Path, name: &str, source: &str) -> PathBuf {
    compile_c_program_with_args(directory, name, source, &[])
}

fn compile_c_program_with_args(
    directory: &std::path::Path,
    name: &str,
    source: &str,
    extra_args: &[&str],
) -> PathBuf {
    let source_path = directory.join(format!("{name}.c"));
    let executable_path = directory.join(name);
    std::fs::write(&source_path, source).unwrap();
    let output = std::process::Command::new("/usr/bin/gcc")
        .args(["-O2", "-pthread"])
        .args(extra_args)
        .arg(&source_path)
        .arg("-o")
        .arg(&executable_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "gcc failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    executable_path
}

fn set_interrupt_signal_blocked(blocked: bool) -> bool {
    // SAFETY: set and previous are initialized before libc reads or writes them.
    unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        let mut previous = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGURG);
        let how = if blocked {
            libc::SIG_BLOCK
        } else {
            libc::SIG_UNBLOCK
        };
        assert_eq!(libc::pthread_sigmask(how, &set, &mut previous), 0);
        libc::sigismember(&previous, libc::SIGURG) == 1
    }
}

#[derive(Default)]
struct PostExecLog {
    at_random: Mutex<Option<usize>>,
    calls: AtomicU64,
}

impl PostExecLog {
    fn at_random(&self) -> Option<usize> {
        *self.at_random.lock().expect("post-exec log lock poisoned")
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[reverie::global_tool]
impl GlobalTool for PostExecLog {
    type Request = usize;
    type Response = ();
    type Config = ();

    async fn receive_rpc(&self, _from: Pid, at_random: usize) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.at_random.lock().expect("post-exec log lock poisoned") = Some(at_random);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PostExecTool;

#[reverie::tool]
impl Tool for PostExecTool {
    type GlobalState = PostExecLog;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        let mut subscriptions = Subscription::none();
        subscriptions.syscalls([Sysno::execve]);
        subscriptions
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, reverie::Error> {
        guest.tail_inject(syscall).await
    }

    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        let auxv = guest.auxv();
        let address = auxv.at_random().ok_or(Errno::EINVAL)?;
        guest.send_rpc(address.as_raw()).await;
        // This lifecycle hook runs before the ELF entry point, matching execve.
        let address = unsafe { address.into_mut() };
        guest.memory().write_value(address, &POST_EXEC_RANDOM)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct CanonicalInitialExecTool;

#[reverie::tool]
impl Tool for CanonicalInitialExecTool {
    type GlobalState = PostExecLog;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        let mut subscriptions = Subscription::none();
        subscriptions.syscalls([Sysno::execve]);
        subscriptions
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, reverie::Error> {
        let Syscall::Execve(execve) = syscall else {
            unreachable!("tool only subscribes to execve")
        };
        guest
            .tail_inject(reverie::syscalls::Execveat::from(execve))
            .await
    }

    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        guest.send_rpc(0).await;
        Ok(())
    }
}

#[derive(Default)]
struct StartExecLog {
    post_exec_calls: AtomicU64,
}

impl StartExecLog {
    fn post_exec_calls(&self) -> u64 {
        self.post_exec_calls.load(Ordering::SeqCst)
    }
}

#[reverie::global_tool]
impl GlobalTool for StartExecLog {
    type Request = ();
    type Response = ();
    type Config = (usize, usize, usize);

    async fn receive_rpc(&self, _from: Pid, (): ()) {
        self.post_exec_calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct StartExecTool;

#[reverie::tool]
impl Tool for StartExecTool {
    type GlobalState = StartExecLog;
    type ThreadState = ();

    async fn handle_thread_start<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<(), reverie::Error> {
        let (path, argv, envp) = *guest.config();
        let execve = Execve::new()
            .with_path(PathPtr::from_ptr(path as *const libc::c_char))
            .with_argv(Option::<CArrayPtr<CStrPtr>>::from_raw(argv))
            .with_envp(Option::<CArrayPtr<CStrPtr>>::from_raw(envp));
        guest.inject(execve).await?;
        Err(Errno::EINVAL.into())
    }

    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        guest.send_rpc(()).await;
        Ok(())
    }
}

#[derive(Default)]
struct RpcRoundTripLog {
    response_base: u64,
    requests: Mutex<Vec<(Pid, u64)>>,
}

impl RpcRoundTripLog {
    fn requests(&self) -> Vec<(Pid, u64)> {
        self.requests
            .lock()
            .expect("RPC round-trip log lock poisoned")
            .clone()
    }
}

#[reverie::global_tool]
impl GlobalTool for RpcRoundTripLog {
    type Request = u64;
    type Response = u64;
    type Config = u64;

    async fn init_global_state(response_base: &u64) -> Self {
        Self {
            response_base: *response_base,
            requests: Mutex::default(),
        }
    }

    async fn receive_rpc(&self, from: Pid, ordinal: u64) -> u64 {
        self.requests
            .lock()
            .expect("RPC round-trip log lock poisoned")
            .push((from, ordinal));
        self.response_base + ordinal
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RpcRoundTripTool;

#[reverie::tool]
impl Tool for RpcRoundTripTool {
    type GlobalState = RpcRoundTripLog;
    type ThreadState = u64;

    fn subscriptions(_config: &u64) -> Subscription {
        let mut subscriptions = Subscription::none();
        subscriptions.syscall(Sysno::getpid);
        subscriptions
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, reverie::Error> {
        assert!(matches!(syscall, Syscall::Getpid(_)));
        let ordinal = {
            let ordinal = guest.thread_state_mut();
            *ordinal += 1;
            *ordinal
        };
        Ok(guest.send_rpc(ordinal).await as i64)
    }

    async fn on_exit_thread<G: GlobalRPC<Self::GlobalState>>(
        &self,
        _tid: Pid,
        global: &G,
        thread_state: Self::ThreadState,
        _status: ExitStatus,
    ) -> Result<(), reverie::Error> {
        let ordinal = thread_state + 1;
        assert_eq!(global.send_rpc(ordinal).await, *global.config() + ordinal);
        Ok(())
    }
}

struct ConcurrentToolStackLog {
    rendezvous: Barrier,
    tids: Mutex<Vec<Pid>>,
}

impl Default for ConcurrentToolStackLog {
    fn default() -> Self {
        Self {
            rendezvous: Barrier::new(2),
            tids: Mutex::new(Vec::new()),
        }
    }
}

impl ConcurrentToolStackLog {
    fn tids(&self) -> Vec<Pid> {
        self.tids
            .lock()
            .expect("concurrent Tool stack log poisoned")
            .clone()
    }
}

#[reverie::global_tool]
impl GlobalTool for ConcurrentToolStackLog {
    type Request = ();
    type Response = ();
    type Config = ();

    async fn receive_rpc(&self, from: Pid, (): ()) {
        self.tids
            .lock()
            .expect("concurrent Tool stack log poisoned")
            .push(from);
        self.rendezvous.wait();
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ConcurrentToolStackTool;

#[reverie::tool]
impl Tool for ConcurrentToolStackTool {
    type GlobalState = ConcurrentToolStackLog;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        let mut subscriptions = Subscription::none();
        subscriptions.syscall(Sysno::getppid);
        subscriptions
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, reverie::Error> {
        assert!(matches!(syscall, Syscall::Getppid(_)));
        let expected = u64::try_from(guest.tid().as_raw()).unwrap();
        let mut stack = guest.stack().await;
        let address = stack.push(expected);
        let guard = stack.commit()?;
        guest.send_rpc(()).await;
        let observed = guest.memory().read_value(address)?;
        if observed != expected {
            guest.tail_inject(ExitGroup::new().with_status(91)).await
        }
        drop(guard);
        Ok(guest.inject(syscall).await?)
    }
}

struct WorkerExecOverlapLog {
    rendezvous: Barrier,
    worker_errno: Mutex<Option<i32>>,
    worker_ready: Condvar,
    root_value_preserved: AtomicBool,
}

impl Default for WorkerExecOverlapLog {
    fn default() -> Self {
        Self {
            rendezvous: Barrier::new(2),
            worker_errno: Mutex::new(None),
            worker_ready: Condvar::new(),
            root_value_preserved: AtomicBool::new(false),
        }
    }
}

impl WorkerExecOverlapLog {
    fn worker_errno(&self) -> Option<i32> {
        *self
            .worker_errno
            .lock()
            .expect("worker exec result lock poisoned")
    }

    fn root_value_preserved(&self) -> bool {
        self.root_value_preserved.load(Ordering::SeqCst)
    }
}

#[reverie::global_tool]
impl GlobalTool for WorkerExecOverlapLog {
    type Request = (u8, i64);
    type Response = i64;
    type Config = (usize, usize, usize);

    async fn receive_rpc(&self, _from: Pid, (operation, value): (u8, i64)) -> i64 {
        match operation {
            // Both callbacks hold committed stack guards before either
            // proceeds to the image-replacement attempt.
            0 => {
                self.rendezvous.wait();
                0
            }
            // Publish the worker's observed errno and wake the root callback.
            1 => {
                *self
                    .worker_errno
                    .lock()
                    .expect("worker exec result lock poisoned") =
                    Some(i32::try_from(value).expect("errno must fit i32"));
                self.worker_ready.notify_all();
                0
            }
            // Wait with a bound so the pre-fix successful replacement cannot
            // leave the test blocked indefinitely.
            2 => {
                let result = self
                    .worker_errno
                    .lock()
                    .expect("worker exec result lock poisoned");
                let (result, _) = self
                    .worker_ready
                    .wait_timeout_while(result, std::time::Duration::from_secs(5), |result| {
                        result.is_none()
                    })
                    .expect("worker exec result wait poisoned");
                result.map(i64::from).unwrap_or(-1)
            }
            // Record that the root callback could still read its committed
            // value after the worker's refused image replacement.
            3 => {
                self.root_value_preserved
                    .store(value != 0, Ordering::SeqCst);
                0
            }
            _ => panic!("unexpected worker exec test operation {operation}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct WorkerExecOverlapTool;

#[reverie::tool]
impl Tool for WorkerExecOverlapTool {
    type GlobalState = WorkerExecOverlapLog;
    type ThreadState = ();

    fn subscriptions(_config: &(usize, usize, usize)) -> Subscription {
        let mut subscriptions = Subscription::none();
        subscriptions.syscall(Sysno::getppid);
        subscriptions
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, reverie::Error> {
        assert!(matches!(syscall, Syscall::Getppid(_)));
        let expected = u64::try_from(guest.tid().as_raw()).unwrap();
        let mut stack = guest.stack().await;
        let address = stack.push(expected);
        let guard = stack.commit()?;
        guest.send_rpc((0, 0)).await;

        if guest.is_main_thread() {
            let worker_errno = guest.send_rpc((2, 0)).await;
            let observed = guest.memory().read_value(address)?;
            let preserved =
                worker_errno == i64::from(Errno::ENOSYS.into_raw()) && observed == expected;
            guest.send_rpc((3, i64::from(preserved))).await;
            if !preserved {
                guest.tail_inject(ExitGroup::new().with_status(92)).await
            }
        } else {
            let (path, argv, envp) = *guest.config();
            let request = Execve::new()
                .with_path(PathPtr::from_ptr(path as *const libc::c_char))
                .with_argv(Option::<CArrayPtr<CStrPtr>>::from_raw(argv))
                .with_envp(Option::<CArrayPtr<CStrPtr>>::from_raw(envp));
            let error = guest
                .inject(request)
                .await
                .expect_err("worker image replacement unexpectedly succeeded");
            guest.send_rpc((1, i64::from(error.into_raw()))).await;
        }

        drop(guard);
        Ok(guest.inject(syscall).await?)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct DoubleForkTool;

#[reverie::tool]
impl Tool for DoubleForkTool {
    type GlobalState = ();
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        let mut subscriptions = Subscription::none();
        subscriptions.syscalls([Sysno::fork]);
        subscriptions
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, reverie::Error> {
        let Syscall::Fork(fork) = syscall else {
            panic!("expected fork, got {syscall:?}");
        };
        let first = guest.inject(fork).await?;
        let second = guest.inject(Fork::new()).await?;
        assert!(first > 0);
        assert!(second > first);
        Ok(first)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FailingPostExecTool;

#[reverie::tool]
impl Tool for FailingPostExecTool {
    type GlobalState = ();
    type ThreadState = ();

    async fn handle_post_exec<G: Guest<Self>>(&self, _guest: &mut G) -> Result<(), Errno> {
        Err(Errno::EINVAL)
    }

    async fn on_exit_thread<G: GlobalRPC<Self::GlobalState>>(
        &self,
        _tid: Pid,
        _global: &G,
        _thread_state: Self::ThreadState,
        _status: ExitStatus,
    ) -> Result<(), reverie::Error> {
        POST_EXEC_FAILURE_EXITED.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn kvm_is_unavailable(error: &kvm_ioctls::Error) -> bool {
    matches!(error.errno(), libc::ENOENT | libc::EACCES | libc::EPERM)
}

fn kvm_available(test: &str) -> bool {
    match Kvm::new() {
        Ok(_) => true,
        Err(error) if kvm_is_unavailable(&error) => {
            if std::env::var_os("REVERIE_REQUIRE_KVM").is_some() {
                panic!("{test} requires usable /dev/kvm: {error}");
            }
            eprintln!("skipping {test}: cannot open /dev/kvm: {error}");
            false
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }
}

fn assert_invalid_opcode(error: Error) {
    match error {
        Error::GuestException {
            vector,
            instruction_pointer,
            ..
        } => {
            assert_eq!(vector, 6);
            assert_eq!(instruction_pointer, LOAD_ADDRESS);
        }
        error => panic!("expected invalid-opcode exception, got {error}"),
    }
}

fn assert_page_fault(error: Error) {
    match error {
        Error::GuestException {
            vector,
            instruction_pointer,
            fault_address,
        } => {
            assert_eq!(vector, 14);
            assert_eq!(instruction_pointer, LOAD_ADDRESS);
            assert_eq!(fault_address, 0x4000_0000);
        }
        error => panic!("expected page-fault exception, got {error}"),
    }
}

fn assert_general_protection(error: Error) {
    match error {
        Error::GuestException {
            vector,
            instruction_pointer,
            ..
        } => {
            assert_eq!(vector, 13);
            assert_eq!(instruction_pointer, LOAD_ADDRESS);
        }
        error => panic!("expected general-protection exception, got {error}"),
    }
}

#[test]
fn static_elf_faults_are_reported_by_direct_and_tool_runtimes() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM exception test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let image = static_elf(&[0x0f, 0x0b]);

    let mut direct_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    direct_backend
        .install_static_elf(&image, "/bin/fault")
        .unwrap();
    assert_invalid_opcode(direct_backend.run_static_elf().unwrap_err());

    let mut tool_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    tool_backend
        .install_static_elf(&image, "/bin/fault")
        .unwrap();
    let error = match futures::executor::block_on(
        tool_backend.run_static_elf_with_tool::<StraceTool>((), true),
    ) {
        Ok(_) => panic!("tool runtime reported a guest exception as success"),
        Err(error) => error,
    };
    assert_invalid_opcode(error);

    // movabs rax, qword ptr [0x40000000], an address outside the page tables.
    let page_fault_image =
        static_elf(&[0x48, 0xa1, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00]);
    let mut page_fault_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    page_fault_backend
        .install_static_elf(&page_fault_image, "/bin/fault")
        .unwrap();
    assert_page_fault(page_fault_backend.run_static_elf().unwrap_err());

    let mut io_fault_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    io_fault_backend
        .install_static_elf(&static_elf(&[0xed]), "/bin/fault")
        .unwrap();
    assert_general_protection(io_fault_backend.run_static_elf().unwrap_err());
}

#[test]
fn static_elf_vmware_probe_reports_non_vmware_in_direct_and_tool_runtimes() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM VMware probe test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let code = [
        0xbb, 0x68, 0x58, 0x4d, 0x56, // mov ebx, 0x564d5868
        0xb9, 0x58, 0x56, 0x00, 0x00, // mov ecx, 0x5658
        0x31, 0xd2, // xor edx, edx
        0xed, // in eax, dx
        0x85, 0xdb, // test ebx, ebx
        0x75, 0x09, // jne failure
        0xb8, 0x3c, 0x00, 0x00, 0x00, // mov eax, SYS_exit
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0xb8, 0x3c, 0x00, 0x00, 0x00, // failure: mov eax, SYS_exit
        0xbf, 0x01, 0x00, 0x00, 0x00, // mov edi, 1
        0x0f, 0x05, // syscall
    ];
    let image = static_elf(&code);

    let mut direct_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    direct_backend
        .install_static_elf(&image, "/bin/vmware-probe")
        .unwrap();
    assert_eq!(direct_backend.run_static_elf().unwrap(), 0);

    let mut tool_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    tool_backend
        .install_static_elf(&image, "/bin/vmware-probe")
        .unwrap();
    let (_, code, stdout, stderr) =
        futures::executor::block_on(tool_backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();
    assert_eq!(code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn static_elf_cannot_copy_supervisor_bootstrap_memory() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM bootstrap access test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let code = [
        0xbf, 0x01, 0x00, 0x00, 0x00, // mov edi, 1
        0xbe, 0x00, 0x10, 0x00, 0x00, // mov esi, 0x1000
        0xba, 0x10, 0x00, 0x00, 0x00, // mov edx, 16
        0xb8, 0x01, 0x00, 0x00, 0x00, // mov eax, SYS_write
        0x0f, 0x05, // syscall
        0x48, 0x83, 0xf8, 0xf2, // cmp rax, -EFAULT
        0x74, 0x0e, // je success
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0xbf, 0x2a, 0x00, 0x00, 0x00, // mov edi, 42
        0x0f, 0x05, 0x0f, 0x0b, // syscall; ud2
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, 0x0f, 0x0b, // syscall; ud2
    ];

    for with_tool in [false, true] {
        let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
        backend
            .install_static_elf(&static_elf(&code), "/bin/bootstrap-access-test")
            .unwrap();
        let (exit_code, stdout, stderr) = if with_tool {
            let (_, exit_code, stdout, stderr) = futures::executor::block_on(
                backend.run_static_elf_with_tool::<StraceTool>((), true),
            )
            .unwrap();
            (exit_code, stdout, stderr)
        } else {
            backend.run_static_elf_captured().unwrap()
        };
        assert_eq!(exit_code, 0, "with_tool={with_tool}");
        assert!(stdout.is_empty(), "with_tool={with_tool}");
        assert!(stderr.is_empty(), "with_tool={with_tool}");
    }
}

#[test]
fn static_elf_forks_execs_and_waits_for_child() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM multiprocess test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let message = b"hello from fork exec\n";
    let mut target = vec![0xbf, 0x01, 0x00, 0x00, 0x00]; // mov edi, 1
    let message_operand = target.len() + 2;
    target.extend_from_slice(&[0x48, 0xbe, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rsi, message
    target.push(0xba);
    target.extend_from_slice(&(message.len() as u32).to_le_bytes()); // mov edx, len
    target.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x05]); // write
    target.extend_from_slice(&[
        0xb8, 0xe7, 0x00, 0x00, 0x00, 0x31, 0xff, 0x0f, 0x05, 0x0f, 0x0b,
    ]); // exit_group(0); ud2
    let message_address = LOAD_ADDRESS + target.len() as u64;
    target[message_operand..message_operand + 8].copy_from_slice(&message_address.to_le_bytes());
    target.extend_from_slice(message);
    let executable = TestExecutable::new(&static_elf(&target));
    let path = executable.0.to_str().unwrap().as_bytes();

    let mut root = vec![
        0x49, 0xc7, 0xc4, 0x78, 0x56, 0x34, 0x12, // mov r12, 0x12345678
        0xb8, 0x78, 0x56, 0x34, 0x12, // mov eax, 0x12345678
        0x66, 0x0f, 0x6e, 0xc0, // movd xmm0, eax
        0xb8, 0x39, 0x00, 0x00, 0x00, // mov eax, SYS_fork
        0x0f, 0x05, // syscall
        0x85, 0xc0, // test eax, eax
        0x74, 0x00, // jz child
    ];
    let child_jump = root.len() - 1;
    root.extend_from_slice(&[
        0x89, 0xc7, // mov edi, eax
        0x48, 0x83, 0xec, 0x10, // sub rsp, 16
        0x48, 0x89, 0xe6, // mov rsi, rsp
        0x31, 0xd2, // xor edx, edx
        0x45, 0x31, 0xd2, // xor r10d, r10d
        0xb8, 0x3d, 0x00, 0x00, 0x00, // mov eax, SYS_wait4
        0x0f, 0x05, // syscall
        0x8b, 0x3c, 0x24, // mov edi, dword ptr [rsp]
        0xc1, 0xef, 0x08, // shr edi, 8
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ]);
    let child_offset = root.len();
    let displacement = child_offset as isize - (child_jump + 1) as isize;
    root[child_jump] = i8::try_from(displacement).unwrap() as u8;

    root.extend_from_slice(&[
        0x49, 0x81, 0xfc, 0x78, 0x56, 0x34, 0x12, // cmp r12, 0x12345678
        0x74, 0x0e, // je callee_saved_ok
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0xbf, 0x2a, 0x00, 0x00, 0x00, // mov edi, 42
        0x0f, 0x05, 0x0f, 0x0b, // syscall; ud2
        0x66, 0x0f, 0x7e, 0xc0, // movd eax, xmm0
        0x3d, 0x78, 0x56, 0x34, 0x12, // cmp eax, 0x12345678
        0x74, 0x0e, // je fpu_ok
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0xbf, 0x2b, 0x00, 0x00, 0x00, // mov edi, 43
        0x0f, 0x05, 0x0f, 0x0b, // syscall; ud2
    ]);

    let path_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xbf, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rdi, path
    let argv_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xbe, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rsi, argv
    let envp_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xba, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rdx, envp
    root.extend_from_slice(&[
        0xb8, 0x3b, 0x00, 0x00, 0x00, 0x0f, 0x05, // execve
        0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x2a, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f,
        0x0b, // exit_group(42); ud2
    ]);

    let path_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(path);
    root.push(0);
    while !root.len().is_multiple_of(8) {
        root.push(0);
    }
    let argv_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(&path_address.to_le_bytes());
    root.extend_from_slice(&0_u64.to_le_bytes());
    let envp_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(&0_u64.to_le_bytes());
    root[path_operand..path_operand + 8].copy_from_slice(&path_address.to_le_bytes());
    root[argv_operand..argv_operand + 8].copy_from_slice(&argv_address.to_le_bytes());
    root[envp_operand..envp_operand + 8].copy_from_slice(&envp_address.to_le_bytes());

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&root), "/bin/fork-exec-test")
        .unwrap();
    let (code, stdout, stderr) = backend.run_static_elf_captured().unwrap();

    assert_eq!(code, 0);
    assert_eq!(stdout, message);
    assert!(stderr.is_empty());

    let mut tool_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    tool_backend
        .install_static_elf(&static_elf(&root), "/bin/fork-exec-test")
        .unwrap();
    let (_, code, stdout, stderr) =
        futures::executor::block_on(tool_backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();

    assert_eq!(code, 0);
    assert_eq!(stdout, message);
    assert!(stderr.is_empty());
}

#[test]
fn static_elf_self_abort_terminates_instead_of_faulting() {
    // Regression: glibc abort() writes its diagnostic, then raises SIGABRT via
    // tgkill(pid, tid, SIGABRT). Previously SIGABRT was unhandled (ENOSYS), so
    // abort() fell through to its "unreachable" hlt trap and the VM reported a
    // spurious #GP (exception vector 13). A self-directed fatal signal must now
    // terminate the process with the conventional 128 + signo status while
    // preserving output emitted before the signal.
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM self-abort test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let message = b"before abort\n";
    // write(1, message, len)
    let mut code = vec![0xbf, 0x01, 0x00, 0x00, 0x00]; // mov edi, 1
    let message_operand = code.len() + 2;
    code.extend_from_slice(&[0x48, 0xbe, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rsi, message
    code.push(0xba);
    code.extend_from_slice(&(message.len() as u32).to_le_bytes()); // mov edx, len
    code.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x05]); // mov eax, SYS_write; syscall
    // pid = getpid(); tgkill(pid, pid, SIGABRT)
    code.extend_from_slice(&[
        0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, SYS_getpid
        0x0f, 0x05, // syscall -> rax = pid
        0x89, 0xc7, // mov edi, eax  (tgid)
        0x89, 0xc6, // mov esi, eax  (tid)
        0xba, 0x06, 0x00, 0x00, 0x00, // mov edx, SIGABRT
        0xb8, 0xea, 0x00, 0x00, 0x00, // mov eax, SYS_tgkill
        0x0f, 0x05, // syscall -> must terminate here
        0x0f, 0x0b, // ud2 (only reached if the signal did not terminate us)
    ]);
    let message_address = LOAD_ADDRESS + code.len() as u64;
    code[message_operand..message_operand + 8].copy_from_slice(&message_address.to_le_bytes());
    code.extend_from_slice(message);

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&code), "/bin/self-abort")
        .unwrap();
    let (code_result, stdout, stderr) = backend.run_static_elf_captured().unwrap();

    // 128 + SIGABRT(6) == 134, matching the shell/native convention.
    assert_eq!(code_result, 128 + libc::SIGABRT);
    assert_eq!(stdout, message);
    assert!(stderr.is_empty());
}

#[test]
fn static_elf_clone_tid_side_effects_reach_guest_memory() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM clone TID test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    fn append_exit(code: &mut Vec<u8>, status: u32) {
        code.extend_from_slice(&[0xb8, 0xe7, 0x00, 0x00, 0x00]);
        code.push(0xbf);
        code.extend_from_slice(&status.to_le_bytes());
        code.extend_from_slice(&[0x0f, 0x05, 0x0f, 0x0b]);
    }

    fn patch_jump(code: &mut [u8], operand: usize, target: usize) {
        let displacement = i32::try_from(target as isize - (operand + 4) as isize).unwrap();
        code[operand..operand + 4].copy_from_slice(&displacement.to_le_bytes());
    }

    const PARENT_TID: u64 = LOAD_ADDRESS + 0x1800;
    const CHILD_TID: u64 = LOAD_ADDRESS + 0x1808;
    const REPLACEMENT_CLEAR_TID: u64 = LOAD_ADDRESS + 0x1810;
    const INVALID_TID: u64 = MEMORY_SIZE as u64 - 1;
    let flags = libc::SIGCHLD as u32
        | libc::CLONE_PARENT_SETTID as u32
        | libc::CLONE_CHILD_SETTID as u32
        | libc::CLONE_CHILD_CLEARTID as u32;

    let mut code = Vec::new();
    code.extend_from_slice(&[0xb8, 0x38, 0x00, 0x00, 0x00]); // mov eax, SYS_clone
    code.push(0xbf); // mov edi, flags
    code.extend_from_slice(&flags.to_le_bytes());
    code.extend_from_slice(&[0x31, 0xf6]); // xor esi, esi
    code.extend_from_slice(&[0x48, 0xba]); // movabs rdx, parent_tid
    code.extend_from_slice(&PARENT_TID.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xba]); // movabs r10, child_tid
    code.extend_from_slice(&CHILD_TID.to_le_bytes());
    code.extend_from_slice(&[0x0f, 0x05, 0x85, 0xc0, 0x0f, 0x84, 0x00, 0x00, 0x00, 0x00]); // syscall; jz child
    let first_child_jump = code.len() - 4;

    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, parent_tid
    code.extend_from_slice(&PARENT_TID.to_le_bytes());
    code.extend_from_slice(&[0x39, 0x01, 0x74, 0x0e]); // cmp [rcx], eax; je parent_tid_ok
    append_exit(&mut code, 61);
    code.extend_from_slice(&[
        0x89, 0xc7, // mov edi, eax
        0x48, 0x83, 0xec, 0x10, // sub rsp, 16
        0x48, 0x89, 0xe6, // mov rsi, rsp
        0x31, 0xd2, // xor edx, edx
        0x45, 0x31, 0xd2, // xor r10d, r10d
        0xb8, 0x3d, 0x00, 0x00, 0x00, // mov eax, SYS_wait4
        0x0f, 0x05, // syscall
        0x83, 0x3c, 0x24, 0x00, // cmp dword ptr [rsp], 0
        0x74, 0x0e, // je first_child_ok
    ]);
    append_exit(&mut code, 64);

    // A second clone proves invalid TID stores do not abort child creation.
    code.extend_from_slice(&[0xb8, 0x38, 0x00, 0x00, 0x00]);
    code.push(0xbf);
    code.extend_from_slice(&flags.to_le_bytes());
    code.extend_from_slice(&[0x31, 0xf6]); // xor esi, esi
    code.extend_from_slice(&[0x48, 0xba]); // movabs rdx, invalid parent_tid
    code.extend_from_slice(&INVALID_TID.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xba]); // movabs r10, invalid child_tid
    code.extend_from_slice(&INVALID_TID.to_le_bytes());
    code.extend_from_slice(&[
        0x0f, 0x05, // syscall
        0x85, 0xc0, // test eax, eax
        0x79, 0x0e, // jns clone_returned_pid_or_child
    ]);
    append_exit(&mut code, 65);
    code.extend_from_slice(&[0x0f, 0x84, 0x00, 0x00, 0x00, 0x00]); // jz child
    let invalid_child_jump = code.len() - 4;
    code.extend_from_slice(&[
        0x89, 0xc7, // mov edi, eax
        0x48, 0x89, 0xe6, // mov rsi, rsp
        0x31, 0xd2, // xor edx, edx
        0x45, 0x31, 0xd2, // xor r10d, r10d
        0xb8, 0x3d, 0x00, 0x00, 0x00, // mov eax, SYS_wait4
        0x0f, 0x05, // syscall
        0x39, 0xf8, // cmp eax, edi
        0x74, 0x0e, // je waited_for_second_child
    ]);
    append_exit(&mut code, 66);
    code.extend_from_slice(&[
        0x8b, 0x3c, 0x24, // mov edi, dword ptr [rsp]
        0xc1, 0xef, 0x08, // shr edi, 8
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x0f, 0x05, 0x0f, 0x0b, // syscall; ud2
    ]);

    let first_child = code.len();
    patch_jump(&mut code, first_child_jump, first_child);
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_tid
    code.extend_from_slice(&CHILD_TID.to_le_bytes());
    code.extend_from_slice(&[0x83, 0x39, 0x02, 0x74, 0x0e]); // cmp [rcx], 2; je
    append_exit(&mut code, 62);
    code.extend_from_slice(&[0xb8, 0xda, 0x00, 0x00, 0x00]); // set_tid_address
    code.extend_from_slice(&[0x48, 0xbf]); // movabs rdi, replacement pointer
    code.extend_from_slice(&REPLACEMENT_CLEAR_TID.to_le_bytes());
    code.extend_from_slice(&[0x0f, 0x05, 0x83, 0xf8, 0x02, 0x74, 0x0e]); // syscall; cmp eax, 2; je
    append_exit(&mut code, 63);
    append_exit(&mut code, 0);

    let invalid_store_child = code.len();
    patch_jump(&mut code, invalid_child_jump, invalid_store_child);
    append_exit(&mut code, 0);

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&code), "/bin/clone-tid-test")
        .unwrap();
    let (code, stdout, stderr) = backend.run_static_elf_captured().unwrap();
    assert_eq!(code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn static_elf_runs_glibc_clone3_thread_and_restores_parent_state() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM clone3 thread test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    fn append_exit(code: &mut Vec<u8>, status: u32) {
        code.extend_from_slice(&[0xb8, 0xe7, 0x00, 0x00, 0x00]);
        code.push(0xbf);
        code.extend_from_slice(&status.to_le_bytes());
        code.extend_from_slice(&[0x0f, 0x05, 0x0f, 0x0b]);
    }

    fn patch_jump(code: &mut [u8], operand: usize, target: usize) {
        let displacement = i32::try_from(target as isize - (operand + 4) as isize).unwrap();
        code[operand..operand + 4].copy_from_slice(&displacement.to_le_bytes());
    }

    const PARENT_TID: u64 = LOAD_ADDRESS + 0x1800;
    const CHILD_TID: u64 = LOAD_ADDRESS + 0x1808;
    const CHILD_RESULT: u64 = LOAD_ADDRESS + 0x1810;
    const CHILD_FS: u64 = LOAD_ADDRESS + 0x1818;
    const CHILD_RSP: u64 = LOAD_ADDRESS + 0x1820;
    const TLS: u64 = LOAD_ADDRESS + 0x1880;
    const CHILD_STACK: u64 = LOAD_ADDRESS + 0x1900;
    const CHILD_STACK_SIZE: u64 = 0x600;
    const CHILD_STACK_TOP: u64 = CHILD_STACK + CHILD_STACK_SIZE;
    let flags = libc::CLONE_VM as u64
        | libc::CLONE_FS as u64
        | libc::CLONE_FILES as u64
        | libc::CLONE_SIGHAND as u64
        | libc::CLONE_THREAD as u64
        | libc::CLONE_SYSVSEM as u64
        | libc::CLONE_SETTLS as u64
        | libc::CLONE_PARENT_SETTID as u64
        | libc::CLONE_CHILD_CLEARTID as u64;

    let mut code = vec![
        0x49, 0x89, 0xe4, // mov r12, rsp
        0xb8, 0x78, 0x56, 0x34, 0x12, // mov eax, 0x12345678
        0x66, 0x0f, 0x6e, 0xc0, // movd xmm0, eax
    ];
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_tid
    code.extend_from_slice(&CHILD_TID.to_le_bytes());
    code.extend_from_slice(&[0xc7, 0x01, 0xff, 0xff, 0xff, 0x7f]); // mov [rcx], sentinel
    code.extend_from_slice(&[0xb8, 0xb3, 0x01, 0x00, 0x00]); // mov eax, SYS_clone3
    let clone_args_operand = code.len() + 2;
    code.extend_from_slice(&[0x48, 0xbf, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rdi, clone_args
    code.extend_from_slice(&[0xbe, 0x58, 0x00, 0x00, 0x00]); // mov esi, sizeof(clone_args)
    code.extend_from_slice(&[0x0f, 0x05, 0x85, 0xc0, 0x0f, 0x84, 0, 0, 0, 0]); // syscall; jz child
    let child_jump = code.len() - 4;

    code.extend_from_slice(&[0x4c, 0x39, 0xe4, 0x74, 0x0e]); // cmp rsp, r12; je
    append_exit(&mut code, 81);
    code.extend_from_slice(&[0x41, 0x89, 0xc5]); // mov r13d, eax
    code.extend_from_slice(&[0x48, 0xbf]); // movabs rdi, child_tid
    code.extend_from_slice(&CHILD_TID.to_le_bytes());
    code.extend_from_slice(&[
        0xbe, 0x00, 0x00, 0x00, 0x00, // mov esi, FUTEX_WAIT
        0x44, 0x89, 0xea, // mov edx, r13d
        0x45, 0x31, 0xd2, // xor r10d, r10d
        0xb8, 0xca, 0x00, 0x00, 0x00, // mov eax, SYS_futex
        0x0f, 0x05, // syscall
        0x83, 0x3f, 0x00, // cmp dword ptr [rdi], 0
        0x75, 0xe9, // jne FUTEX_WAIT
        0x44, 0x89, 0xe8, // mov eax, r13d
    ]);
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, parent_tid
    code.extend_from_slice(&PARENT_TID.to_le_bytes());
    code.extend_from_slice(&[0x39, 0x01, 0x74, 0x0e]); // cmp [rcx], eax; je
    append_exit(&mut code, 82);
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_result
    code.extend_from_slice(&CHILD_RESULT.to_le_bytes());
    code.extend_from_slice(&[0x39, 0x01, 0x74, 0x0e]); // cmp [rcx], eax; je
    append_exit(&mut code, 83);
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_tid
    code.extend_from_slice(&CHILD_TID.to_le_bytes());
    code.extend_from_slice(&[0x83, 0x39, 0x00, 0x74, 0x0e]); // cmp dword ptr [rcx], 0; je
    append_exit(&mut code, 84);
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_fs
    code.extend_from_slice(&CHILD_FS.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xba]); // movabs rdx, tls
    code.extend_from_slice(&TLS.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x39, 0x11, 0x74, 0x0e]); // cmp [rcx], rdx; je
    append_exit(&mut code, 85);
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_rsp
    code.extend_from_slice(&CHILD_RSP.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xba]); // movabs rdx, child_stack_top
    code.extend_from_slice(&CHILD_STACK_TOP.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x39, 0x11, 0x74, 0x0e]); // cmp [rcx], rdx; je
    append_exit(&mut code, 86);
    code.extend_from_slice(&[
        0x66, 0x0f, 0x7e, 0xc0, // movd eax, xmm0
        0x3d, 0x78, 0x56, 0x34, 0x12, // cmp eax, 0x12345678
        0x74, 0x0e, // je
    ]);
    append_exit(&mut code, 87);
    code.extend_from_slice(&[
        0xb8, 0xba, 0x00, 0x00, 0x00, // mov eax, SYS_gettid
        0x0f, 0x05, // syscall
        0x83, 0xf8, 0x01, // cmp eax, 1
        0x74, 0x0e, // je
    ]);
    append_exit(&mut code, 88);
    append_exit(&mut code, 0);

    let child = code.len();
    patch_jump(&mut code, child_jump, child);
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_rsp
    code.extend_from_slice(&CHILD_RSP.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x21]); // mov [rcx], rsp
    code.extend_from_slice(&[0xb8, 0x9e, 0x00, 0x00, 0x00]); // mov eax, SYS_arch_prctl
    code.extend_from_slice(&[0xbf, 0x03, 0x10, 0x00, 0x00]); // mov edi, ARCH_GET_FS
    code.extend_from_slice(&[0x48, 0xbe]); // movabs rsi, child_fs
    code.extend_from_slice(&CHILD_FS.to_le_bytes());
    code.extend_from_slice(&[0x0f, 0x05]); // syscall
    code.extend_from_slice(&[0xb8, 0xba, 0x00, 0x00, 0x00, 0x0f, 0x05]); // gettid
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_result
    code.extend_from_slice(&CHILD_RESULT.to_le_bytes());
    code.extend_from_slice(&[0x89, 0x01]); // mov [rcx], eax
    code.extend_from_slice(&[
        0xb8, 0x3c, 0x00, 0x00, 0x00, // mov eax, SYS_exit
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ]);

    while !code.len().is_multiple_of(8) {
        code.push(0);
    }
    let clone_args_address = LOAD_ADDRESS + code.len() as u64;
    code[clone_args_operand..clone_args_operand + 8]
        .copy_from_slice(&clone_args_address.to_le_bytes());
    let mut clone_args = [0_u8; 88];
    clone_args[0..8].copy_from_slice(&flags.to_le_bytes());
    // Linux ignores pidfd without CLONE_PIDFD; glibc aliases this union slot.
    clone_args[8..16].copy_from_slice(&CHILD_TID.to_le_bytes());
    clone_args[16..24].copy_from_slice(&CHILD_TID.to_le_bytes());
    clone_args[24..32].copy_from_slice(&PARENT_TID.to_le_bytes());
    clone_args[40..48].copy_from_slice(&CHILD_STACK.to_le_bytes());
    clone_args[48..56].copy_from_slice(&CHILD_STACK_SIZE.to_le_bytes());
    clone_args[56..64].copy_from_slice(&TLS.to_le_bytes());
    code.extend_from_slice(&clone_args);

    // Exercise every worker-dispatch path so a `pthread_join`-style
    // `FUTEX_WAIT`/`CLEARTID` round trip completes (exit 0, no hang) in each:
    //   * `Direct`: the non-Tool personality (`run_process_action`).
    //   * `ToolDefault`: run a tool with *no* explicit ownership override, so the
    //     backend resolves ownership from `Tool::thread_ownership` — whose
    //     default is Tool-owned "follow children". This locks in the safe
    //     default (worker on the Tool loop, `futex` routed to the Tool) so a KVM
    //     caller no longer has to opt threads in.
    //   * `Tool(ThreadOwnership::Host)`: force the hybrid model, where
    //     `run_process_action_with_tool` falls through to the direct worker path
    //     and `futex` stays host-owned. Both execution and futex ownership are
    //     Host, so the round trip is consistent and cannot deadlock.
    //   * `Tool(ThreadOwnership::Tool)`: force the worker onto the Tool loop with
    //     `futex` routed to the Tool.
    #[derive(Debug, Clone, Copy)]
    enum WorkerDispatch {
        Direct,
        ToolDefault,
        Tool(ThreadOwnership),
    }
    for dispatch in [
        WorkerDispatch::Direct,
        WorkerDispatch::ToolDefault,
        WorkerDispatch::Tool(ThreadOwnership::Host),
        WorkerDispatch::Tool(ThreadOwnership::Tool),
    ] {
        let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
        backend
            .install_static_elf(&static_elf(&code), "/bin/clone3-thread-test")
            .unwrap();
        let (exit_code, stdout, stderr) = match dispatch {
            WorkerDispatch::Direct => backend.run_static_elf_captured().unwrap(),
            WorkerDispatch::ToolDefault => {
                // No set_thread_ownership: rely on the resolved default.
                let (_, exit_code, stdout, stderr) = futures::executor::block_on(
                    backend.run_static_elf_with_tool::<StraceTool>((), true),
                )
                .unwrap();
                (exit_code, stdout, stderr)
            }
            WorkerDispatch::Tool(ownership) => {
                backend.set_thread_ownership(ownership);
                let (_, exit_code, stdout, stderr) = futures::executor::block_on(
                    backend.run_static_elf_with_tool::<StraceTool>((), true),
                )
                .unwrap();
                (exit_code, stdout, stderr)
            }
        };
        assert_eq!(exit_code, 0, "dispatch={dispatch:?}");
        assert!(stdout.is_empty(), "dispatch={dispatch:?}");
        assert!(stderr.is_empty(), "dispatch={dispatch:?}");
    }
}

#[test]
fn host_owned_worker_descriptors_keep_read_and_readv_backend_owned() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM Host-owned descriptor test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program(
        &directory.0,
        "host-owned-worker-descriptors",
        r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/eventfd.h>
#include <sys/uio.h>
#include <unistd.h>

#define EVENT_READ_FD 198
#define VECTOR_READ_FD 199

static _Atomic int ready;

static void *worker(void *unused) {
  (void)unused;
  int event = eventfd(0, EFD_CLOEXEC);
  int pipe_fds[2];
  if (event < 0 || pipe(pipe_fds) != 0 ||
      dup2(event, EVENT_READ_FD) != EVENT_READ_FD ||
      dup2(pipe_fds[0], VECTOR_READ_FD) != VECTOR_READ_FD) {
    atomic_store_explicit(&ready, -1, memory_order_release);
    return (void *)(uintptr_t)1;
  }
  close(event);
  close(pipe_fds[0]);

  uint64_t counter = 7;
  char first = 'v';
  char second = 'r';
  struct iovec vector[2] = {
      {.iov_base = &first, .iov_len = 1},
      {.iov_base = &second, .iov_len = 1},
  };
  if (write(EVENT_READ_FD, &counter, sizeof(counter)) != sizeof(counter) ||
      writev(pipe_fds[1], vector, 2) != 2) {
    atomic_store_explicit(&ready, -1, memory_order_release);
    return (void *)(uintptr_t)1;
  }
  close(pipe_fds[1]);
  atomic_store_explicit(&ready, 1, memory_order_release);
  return NULL;
}

int main(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    return 10;
  }
  int state;
  while ((state = atomic_load_explicit(&ready, memory_order_acquire)) == 0) {
  }
  if (state < 0) {
    return 11;
  }

  uint64_t counter = 0;
  if (read(EVENT_READ_FD, &counter, sizeof(counter)) != sizeof(counter) || counter != 7) {
    return 12;
  }
  char first = 0;
  char second = 0;
  struct iovec vector[2] = {
      {.iov_base = &first, .iov_len = 1},
      {.iov_base = &second, .iov_len = 1},
  };
  if (readv(VECTOR_READ_FD, vector, 2) != 2 || first != 'v' || second != 'r') {
    return 13;
  }

  void *result = NULL;
  if (pthread_join(thread, &result) != 0 || result != NULL) {
    return 14;
  }
  return 0;
}
"#,
    );
    let executable = executable.to_str().unwrap();
    let image = std::fs::read(executable).unwrap();
    let mut backend = KvmBackend::new(256 * 1024 * 1024).unwrap();
    backend
        .install_static_elf_with_context(
            &image,
            &[executable],
            &["PATH=/usr/bin:/bin"],
            &directory.0,
        )
        .unwrap();
    backend.set_thread_ownership(ThreadOwnership::Host);
    let (trace, code, stdout, stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();

    assert_eq!(code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    let entries = trace.formatted();
    assert!(
        !entries.iter().any(|entry| entry.starts_with("read(198,")),
        "Host-owned eventfd read unexpectedly reached the Tool: {entries:?}"
    );
    assert!(
        !entries.iter().any(|entry| entry.starts_with("readv(199,")),
        "Host-owned pipe readv unexpectedly reached the Tool: {entries:?}"
    );
}

#[test]
fn ordinary_vectored_endpoints_match_native_linux() {
    if !kvm_available("KVM ordinary vectored endpoint test") {
        return;
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program(
        &directory.0,
        "ordinary-vectored-endpoints",
        r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

static int check_socket(int type, int base) {
  int fds[2];
  if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, fds) != 0) {
    return base;
  }
  char first[4] = "ABC";
  char second[6] = "DEFGH";
  struct iovec output[2] = {
      {.iov_base = first, .iov_len = 3},
      {.iov_base = second, .iov_len = 5},
  };
  if (writev(fds[0], output, 2) != 8) {
    return base + 1;
  }
  memset(first, 0, sizeof(first));
  memset(second, 0, sizeof(second));
  struct iovec input[2] = {
      {.iov_base = first, .iov_len = 3},
      {.iov_base = second, .iov_len = 5},
  };
  if (readv(fds[1], input, 2) != 8 ||
      memcmp(first, "ABC", 3) != 0 || memcmp(second, "DEFGH", 5) != 0) {
    return base + 2;
  }
  close(fds[0]);
  close(fds[1]);
  return 0;
}

int main(void) {
  int event = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  if (event < 0) {
    return 10;
  }
  uint64_t event_value = 7;
  if (write(event, &event_value, sizeof(event_value)) !=
      (ssize_t)sizeof(event_value)) {
    return 11;
  }
  uint32_t event_low = 0;
  uint32_t event_high = 0;
  struct iovec event_input[2] = {
      {.iov_base = &event_low, .iov_len = sizeof(event_low)},
      {.iov_base = &event_high, .iov_len = sizeof(event_high)},
  };
  if (readv(event, event_input, 2) != (ssize_t)sizeof(event_value)) {
    return 12;
  }
  uint64_t event_result = 0;
  memcpy(&event_result, &event_low, sizeof(event_low));
  memcpy((char *)&event_result + sizeof(event_low), &event_high,
         sizeof(event_high));
  if (event_result != event_value) {
    return 13;
  }

  event_low = 9;
  event_high = 0;
  struct iovec event_output[2] = {
      {.iov_base = &event_low, .iov_len = sizeof(event_low)},
      {.iov_base = &event_high, .iov_len = sizeof(event_high)},
  };
  errno = 0;
  if (writev(event, event_output, 2) != -1 || errno != EINVAL) {
    return 14;
  }
  errno = 0;
  if (read(event, &event_result, sizeof(event_result)) != -1 ||
      errno != EAGAIN) {
    return 15;
  }
  close(event);

  int pipe_fds[2];
  if (pipe2(pipe_fds, O_CLOEXEC) != 0) {
    return 20;
  }
  char pipe_first[4] = "pip";
  char pipe_second[6] = "e-data";
  struct iovec pipe_output[2] = {
      {.iov_base = pipe_first, .iov_len = 3},
      {.iov_base = pipe_second, .iov_len = 5},
  };
  if (writev(pipe_fds[1], pipe_output, 2) != 8) {
    return 21;
  }
  memset(pipe_first, 0, sizeof(pipe_first));
  memset(pipe_second, 0, sizeof(pipe_second));
  struct iovec pipe_input[2] = {
      {.iov_base = pipe_first, .iov_len = 3},
      {.iov_base = pipe_second, .iov_len = 5},
  };
  if (readv(pipe_fds[0], pipe_input, 2) != 8 ||
      memcmp(pipe_first, "pip", 3) != 0 ||
      memcmp(pipe_second, "e-data", 5) != 0) {
    return 22;
  }
  close(pipe_fds[0]);
  close(pipe_fds[1]);

  int result = check_socket(SOCK_STREAM, 30);
  if (result != 0) {
    return result;
  }
  result = check_socket(SOCK_DGRAM, 40);
  if (result != 0) {
    return result;
  }

  puts("ordinary-vectored-endpoints-ok");
  return 0;
}
"#,
    );

    let native = std::process::Command::new(&executable).output().unwrap();
    assert!(
        native.status.success(),
        "native fixture exited {:?}; stdout={}; stderr={}",
        native.status.code(),
        String::from_utf8_lossy(&native.stdout),
        String::from_utf8_lossy(&native.stderr),
    );
    assert_eq!(native.stdout, b"ordinary-vectored-endpoints-ok\n");
    assert!(native.stderr.is_empty());

    let executable = executable.to_str().unwrap();
    let image = std::fs::read(executable).unwrap();
    let mut backend = KvmBackend::new(256 * 1024 * 1024).unwrap();
    backend
        .install_static_elf_with_context(
            &image,
            &[executable],
            &["PATH=/usr/bin:/bin"],
            &directory.0,
        )
        .unwrap();
    let (code, stdout, stderr) = backend.run_static_elf_captured().unwrap();
    assert_eq!(
        code,
        0,
        "KVM fixture exited {code}; stdout={}; stderr={}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );
    assert_eq!(stdout, native.stdout);
    assert_eq!(stderr, native.stderr);
}

#[test]
fn vectored_fault_shape_matches_native_linux_on_kvm() {
    if !kvm_available("KVM vectored fault-shape test") {
        return;
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program(
        &directory.0,
        "vectored-fault-shape",
        r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

enum endpoint_kind { ENDPOINT_EVENTFD, ENDPOINT_PIPE, ENDPOINT_STREAM,
                     ENDPOINT_DGRAM };

struct endpoint {
  int reader;
  int writer;
};

struct fault_buffer {
  unsigned char *mapping;
  size_t length;
  unsigned char *crossing;
  unsigned char *invalid;
  unsigned char *later;
};

static int make_endpoint(enum endpoint_kind kind, struct endpoint *endpoint) {
  if (kind == ENDPOINT_EVENTFD) {
    endpoint->reader = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    endpoint->writer = endpoint->reader;
    return endpoint->reader < 0 ? -1 : 0;
  }
  int fds[2];
  if (kind == ENDPOINT_PIPE) {
    if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) != 0) {
      return -1;
    }
  } else {
    int type = kind == ENDPOINT_STREAM ? SOCK_STREAM : SOCK_DGRAM;
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, fds) != 0) {
      return -1;
    }
  }
  endpoint->reader = fds[0];
  endpoint->writer = fds[1];
  return 0;
}

static void close_endpoint(enum endpoint_kind kind, struct endpoint endpoint) {
  close(endpoint.reader);
  if (kind != ENDPOINT_EVENTFD) {
    close(endpoint.writer);
  }
}

static int make_fault_buffer(struct fault_buffer *buffer) {
  size_t page = (size_t)sysconf(_SC_PAGESIZE);
  buffer->length = 2 * page;
  buffer->mapping = mmap(NULL, buffer->length, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (buffer->mapping == MAP_FAILED) {
    return -1;
  }
  buffer->crossing = buffer->mapping + page - 4;
  buffer->invalid = buffer->mapping + page;
  buffer->later = buffer->mapping + 128;
  memcpy(buffer->crossing, "ABCD", 4);
  memcpy(buffer->later, "WXYZ", 4);
  if (mprotect(buffer->mapping + page, page, PROT_NONE) != 0) {
    return -1;
  }
  return 0;
}

static ssize_t fault_read(int positioned, int fd, struct iovec *iov,
                          int count) {
  return positioned ? preadv2(fd, iov, count, (off_t)-1, 0)
                    : readv(fd, iov, count);
}

static ssize_t fault_write(int positioned, int fd, struct iovec *iov,
                           int count) {
  return positioned ? pwritev2(fd, iov, count, (off_t)-1, 0)
                    : writev(fd, iov, count);
}

static int check_read(enum endpoint_kind kind, int positioned, int base) {
  struct endpoint endpoint;
  struct fault_buffer buffer;
  uint64_t payload = UINT64_C(0x0807060504030201);
  if (make_endpoint(kind, &endpoint) != 0 ||
      write(endpoint.writer, &payload, sizeof(payload)) != sizeof(payload) ||
      make_fault_buffer(&buffer) != 0) {
    return base;
  }
  struct iovec iov = {.iov_base = buffer.crossing, .iov_len = 8};
  errno = 0;
  ssize_t result = fault_read(positioned, endpoint.reader, &iov, 1);
  if (result != -1 || errno != EFAULT ||
      memcmp(buffer.crossing, &payload, 4) != 0) {
    return base + 1;
  }
  uint64_t remaining = 0;
  errno = 0;
  ssize_t after = read(endpoint.reader, &remaining, sizeof(remaining));
  if (kind == ENDPOINT_EVENTFD || kind == ENDPOINT_DGRAM) {
    if (after != -1 || errno != EAGAIN) {
      return base + 2;
    }
  } else if (after != sizeof(remaining) || remaining != payload) {
    return base + 3;
  }
  munmap(buffer.mapping, buffer.length);
  close_endpoint(kind, endpoint);
  return 0;
}

static int check_write(enum endpoint_kind kind, int positioned, int base) {
  struct endpoint endpoint;
  struct fault_buffer buffer;
  if (make_endpoint(kind, &endpoint) != 0 || make_fault_buffer(&buffer) != 0) {
    return base;
  }
  struct iovec iov = {.iov_base = buffer.crossing, .iov_len = 8};
  errno = 0;
  ssize_t result = fault_write(positioned, endpoint.writer, &iov, 1);
  if (result != -1 || errno != EFAULT) {
    return base + 1;
  }
  uint64_t received = 0;
  errno = 0;
  if (read(endpoint.reader, &received, sizeof(received)) != -1 ||
      errno != EAGAIN) {
    return base + 2;
  }
  munmap(buffer.mapping, buffer.length);
  close_endpoint(kind, endpoint);
  return 0;
}

static int check_eventfd_later_vector(int positioned, int base) {
  struct endpoint endpoint;
  struct fault_buffer buffer;
  uint64_t payload = 7;
  if (make_endpoint(ENDPOINT_EVENTFD, &endpoint) != 0 ||
      write(endpoint.writer, &payload, sizeof(payload)) != sizeof(payload) ||
      make_fault_buffer(&buffer) != 0) {
    return base;
  }
  struct iovec iov[2] = {
      {.iov_base = buffer.invalid, .iov_len = 4},
      {.iov_base = buffer.later, .iov_len = 4},
  };
  errno = 0;
  ssize_t result = fault_read(positioned, endpoint.reader, iov, 2);
  if (result != -1 || errno != EFAULT ||
      memcmp(buffer.later, "WXYZ", 4) != 0) {
    return base + 1;
  }
  errno = 0;
  if (read(endpoint.reader, &payload, sizeof(payload)) != -1 ||
      errno != EAGAIN) {
    return base + 2;
  }
  munmap(buffer.mapping, buffer.length);
  close_endpoint(ENDPOINT_EVENTFD, endpoint);
  return 0;
}

static int check_null_later_vector(int positioned, int base) {
  struct fault_buffer buffer;
  int fd = open("/dev/null", O_WRONLY | O_CLOEXEC);
  if (fd < 0 || make_fault_buffer(&buffer) != 0) {
    return base;
  }
  struct iovec iov[2] = {
      {.iov_base = buffer.crossing, .iov_len = 8},
      {.iov_base = buffer.later, .iov_len = 4},
  };
  errno = 0;
  if (fault_write(positioned, fd, iov, 2) != 12) {
    return base + 1;
  }
  munmap(buffer.mapping, buffer.length);
  close(fd);
  return 0;
}

int main(void) {
  for (int positioned = 0; positioned <= 1; ++positioned) {
    for (int kind = ENDPOINT_EVENTFD; kind <= ENDPOINT_DGRAM; ++kind) {
      int result = check_read((enum endpoint_kind)kind, positioned,
                              10 + positioned * 80 + kind * 8);
      if (result != 0) {
        return result;
      }
      result = check_write((enum endpoint_kind)kind, positioned,
                           40 + positioned * 80 + kind * 8);
      if (result != 0) {
        return result;
      }
    }
    int result = check_eventfd_later_vector(positioned,
                                            170 + positioned * 10);
    if (result != 0) {
      return result;
    }
    result = check_null_later_vector(positioned, 190 + positioned * 10);
    if (result != 0) {
      return result;
    }
  }
  puts("vectored-fault-shape-ok");
  return 0;
}
"#,
    );

    let native = std::process::Command::new(&executable).output().unwrap();
    assert!(
        native.status.success(),
        "native fixture exited {:?}; stdout={}; stderr={}",
        native.status.code(),
        String::from_utf8_lossy(&native.stdout),
        String::from_utf8_lossy(&native.stderr),
    );
    assert_eq!(native.stdout, b"vectored-fault-shape-ok\n");
    assert!(native.stderr.is_empty());

    let executable = executable.to_str().unwrap();
    let image = std::fs::read(executable).unwrap();
    let mut backend = KvmBackend::new(256 * 1024 * 1024).unwrap();
    backend
        .install_static_elf_with_context(
            &image,
            &[executable],
            &["PATH=/usr/bin:/bin"],
            &directory.0,
        )
        .unwrap();
    let (code, stdout, stderr) = backend.run_static_elf_captured().unwrap();
    assert_eq!(
        code,
        0,
        "KVM fixture exited {code}; stdout={}; stderr={}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );
    assert_eq!(stdout, native.stdout);
    assert_eq!(stderr, native.stderr);
}

#[test]
fn vectored_iovec_address_validation_uses_four_level_guest_abi_on_kvm() {
    if !kvm_available("KVM vectored iovec address-validation test") {
        return;
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program(
        &directory.0,
        "vectored-iovec-address-validation",
        r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>

#define PAGE_SIZE_BYTES UINT64_C(4096)
#define GUEST_LIMIT ((UINT64_C(1) << 47) - PAGE_SIZE_BYTES)
#define FIRST_NON_FOUR_LEVEL (UINT64_C(1) << 47)
#define FIVE_LEVEL_PROBE (UINT64_C(1) << 55)
#define FIVE_LEVEL_LIMIT ((UINT64_C(1) << 56) - PAGE_SIZE_BYTES)
#define MAX_RW_COUNT_VALUE UINT64_C(0x7ffff000)
#define MAX_HOST_IO_VALUE UINT64_C(0x1000000)

static int is_read_operation(int operation) {
  return operation == 0 || operation == 2 || operation == 4;
}

static ssize_t invoke(int operation, int fd, struct iovec *iov, int count,
                      off_t offset) {
  switch (operation) {
    case 0: return readv(fd, iov, count);
    case 1: return writev(fd, iov, count);
    case 2: return preadv(fd, iov, count, offset);
    case 3: return pwritev(fd, iov, count, offset);
    case 4: return preadv2(fd, iov, count, offset, 0);
    case 5: return pwritev2(fd, iov, count, offset, 0);
    default: return -2;
  }
}

static int expect_result(int operation, int fd, struct iovec *iov, int count,
                         off_t offset, ssize_t expected, int code) {
  errno = 0;
  ssize_t result = invoke(operation, fd, iov, count, offset);
  if (result != expected || (expected == -1 && errno != EFAULT)) {
    fprintf(stderr,
            "case %d: op=%d result=%zd errno=%d expected=%zd/EFAULT\n",
            code, operation, result, errno, expected);
    return code;
  }
  return 0;
}

static int expect_einval(int operation, int fd, struct iovec *iov, int count,
                         off_t offset, int code) {
  errno = 0;
  ssize_t result = invoke(operation, fd, iov, count, offset);
  if (result != -1 || errno != EINVAL) {
    fprintf(stderr, "case %d: op=%d result=%zd errno=%d expected=-1/EINVAL\n",
            code, operation, result, errno);
    return code;
  }
  return 0;
}

static int check_address_matrix(int operation, int guest_mode, int host_high,
                                int base) {
  int reading = is_read_operation(operation);
  int fd = open(reading ? "vectored-address-empty" : "/dev/null",
                reading ? O_RDWR | O_CREAT | O_TRUNC : O_WRONLY, 0600);
  if (fd < 0) return base;
  char byte = 'x';
  struct iovec iov[2];
  int result;

#define EXPECT_ONE(address, length, expected, offset) do {                 \
    iov[0].iov_base = (void *)(uintptr_t)(address);                        \
    iov[0].iov_len = (size_t)(length);                                     \
    result = expect_result(operation, fd, iov, 1, (offset), (expected),    \
                           base + __LINE__ % 89);                           \
    if (result != 0) { close(fd); return result; }                          \
  } while (0)

  iov[0].iov_base = &byte;
  iov[0].iov_len = (size_t)SSIZE_MAX + 1;
  result = expect_einval(operation, fd, iov, 1, 0, base + 10);
  if (result != 0) { close(fd); return result; }
  iov[0].iov_len = SIZE_MAX;
  result = expect_einval(operation, fd, iov, 1, 0, base + 11);
  if (result != 0) { close(fd); return result; }
  iov[0].iov_base = (void *)(uintptr_t)UINT64_MAX;
  iov[0].iov_len = (size_t)SSIZE_MAX + 1;
  result = expect_einval(operation, fd, iov, 1, 0, base + 12);
  if (result != 0) { close(fd); return result; }
  iov[0].iov_base = (void *)(uintptr_t)UINT64_MAX;
  iov[0].iov_len = 1;
  iov[1].iov_base = &byte;
  iov[1].iov_len = (size_t)SSIZE_MAX + 1;
  result = expect_einval(operation, fd, iov, 2, 0, base + 13);
  if (result != 0) { close(fd); return result; }
  iov[0].iov_base = &byte;
  iov[0].iov_len = 1;
  iov[1].iov_base = (void *)(uintptr_t)UINT64_MAX;
  iov[1].iov_len = SIZE_MAX;
  result = expect_einval(operation, fd, iov, 2, 0, base + 14);
  if (result != 0) { close(fd); return result; }

  EXPECT_ONE(UINT64_MAX, 1, -1, 0);
  EXPECT_ONE(UINT64_MAX - 3, 8, -1, 0);
  EXPECT_ONE(GUEST_LIMIT - 4, 4, reading ? 0 : 4, 0);
  EXPECT_ONE(GUEST_LIMIT, 0, 0, 0);
  EXPECT_ONE(UINT64_MAX, 0, -1, 0);

  if (guest_mode) {
    EXPECT_ONE(GUEST_LIMIT - 4, 8, -1, 0);
    EXPECT_ONE(GUEST_LIMIT, 1, -1, 0);
    EXPECT_ONE(FIRST_NON_FOUR_LEVEL, 1, -1, 0);
    EXPECT_ONE(FIRST_NON_FOUR_LEVEL, 0, -1, 0);
    EXPECT_ONE(FIVE_LEVEL_PROBE, 1, -1, 0);
  } else if (host_high) {
    EXPECT_ONE(GUEST_LIMIT - 4, 8, reading ? 0 : 8, 0);
    EXPECT_ONE(GUEST_LIMIT, 1, reading ? 0 : 1, 0);
    EXPECT_ONE(FIRST_NON_FOUR_LEVEL, 1, reading ? 0 : 1, 0);
    EXPECT_ONE(FIRST_NON_FOUR_LEVEL, 0, 0, 0);
    EXPECT_ONE(FIVE_LEVEL_PROBE, 1, reading ? 0 : 1, 0);
  }

  iov[0].iov_base = (void *)(uintptr_t)UINT64_MAX;
  iov[0].iov_len = 0;
  iov[1].iov_base = &byte;
  iov[1].iov_len = 1;
  result = expect_result(operation, fd, iov, 2, 0, -1, base + 1);
  if (result != 0) { close(fd); return result; }
  iov[0].iov_base = &byte;
  iov[0].iov_len = 1;
  iov[1].iov_base = (void *)(uintptr_t)UINT64_MAX;
  iov[1].iov_len = 0;
  result = expect_result(operation, fd, iov, 2, 0, -1, base + 2);
  if (result != 0) { close(fd); return result; }

  iov[0].iov_base = (void *)(uintptr_t)1;
  iov[0].iov_len = MAX_HOST_IO_VALUE;
  iov[1].iov_base = (void *)(uintptr_t)UINT64_MAX;
  iov[1].iov_len = 1;
  result = expect_result(operation, fd, iov, 2, 0, -1, base + 3);
  if (result != 0) { close(fd); return result; }

  iov[0].iov_base = 0;
  iov[0].iov_len = MAX_RW_COUNT_VALUE;
  iov[1].iov_base = (void *)(uintptr_t)GUEST_LIMIT;
  iov[1].iov_len = 1;
  if (guest_mode) {
    result = expect_result(operation, fd, iov, 2, 0, -1, base + 4);
  } else if (host_high) {
    result = expect_result(operation, fd, iov, 2, 0,
                           reading ? 0 : MAX_RW_COUNT_VALUE, base + 4);
  } else {
    result = 0;
  }
  if (result != 0) { close(fd); return result; }

  iov[0].iov_base = (void *)(uintptr_t)(GUEST_LIMIT - MAX_RW_COUNT_VALUE);
  iov[0].iov_len = MAX_RW_COUNT_VALUE + 1;
  result = expect_result(operation, fd, iov, 1, 0,
                         reading ? 0 : (guest_mode ? MAX_HOST_IO_VALUE
                                                  : MAX_RW_COUNT_VALUE),
                         base + 5);
  if (result != 0) { close(fd); return result; }

  errno = 0;
  if (invoke(operation, fd, (struct iovec *)(uintptr_t)UINT64_MAX, 0, 0) != 0) {
    close(fd);
    return base + 6;
  }

  if (host_high) {
    size_t count = (size_t)INT64_MAX / (size_t)FIVE_LEVEL_LIMIT + 1;
    if (count > IOV_MAX) { close(fd); return base + 7; }
    struct iovec *aggregate = calloc(count, sizeof(*aggregate));
    if (aggregate == NULL) { close(fd); return base + 8; }
    for (size_t index = 0; index < count; ++index) {
      aggregate[index].iov_base = 0;
      aggregate[index].iov_len = (size_t)FIVE_LEVEL_LIMIT;
    }
    result = expect_result(operation, fd, aggregate, (int)count, 0,
                           reading ? 0 : (guest_mode ? MAX_HOST_IO_VALUE
                                                  : MAX_RW_COUNT_VALUE),
                           base + 9);
    free(aggregate);
    if (result != 0) { close(fd); return result; }
  }

#undef EXPECT_ONE
  close(fd);
  unlink("vectored-address-empty");
  return 0;
}

static int seed_eventfd(int fd, uint64_t value) {
  return write(fd, &value, sizeof(value)) == sizeof(value) ? 0 : -1;
}

static int eventfd_after(int fd, int expect_preserved, uint64_t expected) {
  uint64_t value = 0;
  errno = 0;
  ssize_t result = read(fd, &value, sizeof(value));
  if (expect_preserved) {
    return result == sizeof(value) && value == expected ? 0 : -1;
  }
  return result == -1 && errno == EAGAIN ? 0 : -1;
}

static int check_eventfd_address(int positioned, void *address,
                                 int expect_preserved, int base) {
  int fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  uint64_t value = 7;
  if (fd < 0 || seed_eventfd(fd, value) != 0) return base;
  struct iovec iov = {.iov_base = address, .iov_len = sizeof(value)};
  int operation = positioned ? 4 : 0;
  int result = expect_result(operation, fd, &iov, 1, (off_t)-1, -1, base + 1);
  if (result == 0 && eventfd_after(fd, expect_preserved, value) != 0) {
    result = base + 2;
  }
  close(fd);
  return result;
}

static int check_oversized_file_state(int operation, int base) {
  int fd = open("vectored-ssize-state", O_RDWR | O_CREAT | O_TRUNC, 0600);
  char contents[4] = {0};
  if (fd < 0 || write(fd, "seed", 4) != 4 || lseek(fd, 0, SEEK_SET) != 0) {
    return base;
  }
  struct iovec iov = {
      .iov_base = (void *)(uintptr_t)UINT64_MAX,
      .iov_len = (size_t)SSIZE_MAX + 1,
  };
  int result = expect_einval(operation, fd, &iov, 1, 0, base + 1);
  if (result == 0 && lseek(fd, 0, SEEK_CUR) != 0) result = base + 2;
  if (result == 0 &&
      (pread(fd, contents, sizeof(contents), 0) != sizeof(contents) ||
       memcmp(contents, "seed", sizeof(contents)) != 0)) {
    result = base + 3;
  }
  close(fd);
  unlink("vectored-ssize-state");
  return result;
}

static int check_eventfd_oversized(int positioned, int base) {
  int fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  uint64_t value = 13;
  if (fd < 0 || seed_eventfd(fd, value) != 0) return base;
  struct iovec iov = {
      .iov_base = (void *)(uintptr_t)UINT64_MAX,
      .iov_len = (size_t)SSIZE_MAX + 1,
  };
  int operation = positioned ? 4 : 0;
  int result = expect_einval(operation, fd, &iov, 1, (off_t)-1, base + 1);
  if (result == 0 && eventfd_after(fd, 1, value) != 0) result = base + 2;
  close(fd);
  return result;
}

static int check_pipe_oversized(int positioned, int base) {
  int fds[2];
  if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) != 0) return base;
  struct iovec iov = {
      .iov_base = (void *)(uintptr_t)UINT64_MAX,
      .iov_len = SIZE_MAX,
  };
  int operation = positioned ? 5 : 1;
  int result = expect_einval(operation, fds[1], &iov, 1, (off_t)-1, base + 1);
  char byte;
  errno = 0;
  if (result == 0 && (read(fds[0], &byte, 1) != -1 || errno != EAGAIN)) {
    result = base + 2;
  }
  close(fds[0]);
  close(fds[1]);
  return result;
}

int main(int argc, char **argv) {
  if (argc != 2) return 2;
  int guest_mode = strcmp(argv[1], "guest") == 0;
  if (!guest_mode && strcmp(argv[1], "native") != 0) return 3;

  void *high_mapping = MAP_FAILED;
  int host_high = 0;
  if (!guest_mode) {
    high_mapping = mmap((void *)(uintptr_t)FIVE_LEVEL_PROBE, PAGE_SIZE_BYTES,
                        PROT_NONE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE |
                            MAP_FIXED_NOREPLACE,
                        -1, 0);
    if (high_mapping != MAP_FAILED) {
      if ((uintptr_t)high_mapping != FIVE_LEVEL_PROBE) return 4;
      host_high = 1;
    }
  }

  for (int operation = 0; operation < 6; ++operation) {
    int result = check_address_matrix(operation, guest_mode, host_high,
                                      20 + operation * 100);
    if (result != 0) return result;
    result = check_oversized_file_state(operation, 650 + operation * 10);
    if (result != 0) return result;
  }

  size_t page = (size_t)sysconf(_SC_PAGESIZE);
  void *protected = mmap(NULL, page, PROT_NONE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (protected == MAP_FAILED) return 700;
  for (int positioned = 0; positioned <= 1; ++positioned) {
    int result = check_eventfd_address(positioned, protected, 0,
                                       710 + positioned * 20);
    if (result != 0) return result;
    result = check_eventfd_address(positioned,
                                   (void *)(uintptr_t)UINT64_MAX, 1,
                                   715 + positioned * 20);
    if (result != 0) return result;
    if (guest_mode) {
      result = check_eventfd_address(positioned,
                                     (void *)(uintptr_t)FIVE_LEVEL_PROBE, 1,
                                     720 + positioned * 20);
      if (result != 0) return result;
    } else if (host_high) {
      result = check_eventfd_address(positioned, high_mapping, 0,
                                     720 + positioned * 20);
      if (result != 0) return result;
    }
    result = check_eventfd_oversized(positioned, 750 + positioned * 20);
    if (result != 0) return result;
    result = check_pipe_oversized(positioned, 755 + positioned * 20);
    if (result != 0) return result;
  }
  munmap(protected, page);
  if (host_high) munmap(high_mapping, PAGE_SIZE_BYTES);

  if (guest_mode) {
    puts("guest-vectored-address-ok");
  } else {
    printf("native-vectored-address-ok high=%d\n", host_high);
  }
  return 0;
}
"#,
    );

    let native = std::process::Command::new(&executable)
        .arg("native")
        .output()
        .unwrap();
    assert!(
        native.status.success(),
        "native address fixture exited {:?}; stdout={}; stderr={}",
        native.status.code(),
        String::from_utf8_lossy(&native.stdout),
        String::from_utf8_lossy(&native.stderr),
    );
    assert!(
        native.stdout == b"native-vectored-address-ok high=0\n"
            || native.stdout == b"native-vectored-address-ok high=1\n",
        "unexpected native stdout: {}",
        String::from_utf8_lossy(&native.stdout)
    );
    assert!(native.stderr.is_empty());

    let executable = executable.to_str().unwrap();
    let image = std::fs::read(executable).unwrap();
    let mut backend = KvmBackend::new(256 * 1024 * 1024).unwrap();
    backend
        .install_static_elf_with_context(
            &image,
            &[executable, "guest"],
            &["PATH=/usr/bin:/bin"],
            &directory.0,
        )
        .unwrap();
    let (code, stdout, stderr) = backend.run_static_elf_captured().unwrap();
    assert_eq!(
        code,
        0,
        "KVM address fixture exited {code}; stdout={}; stderr={}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );
    assert_eq!(stdout, b"guest-vectored-address-ok\n");
    assert!(stderr.is_empty());
}

#[test]
fn real_glibc_get_robust_list_tracks_fork_and_thread_lifecycles() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM robust-list lifecycle test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program(
        &directory.0,
        "robust-list-lifecycle",
        r#"
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int tid_pipe[2];
static int release_pipe[2];

static void *worker(void *unused) {
  (void)unused;
  pid_t tid = (pid_t)syscall(SYS_gettid);
  struct robust_list_head *self_head = NULL;
  struct robust_list_head *leader_head = NULL;
  size_t self_len = 0;
  size_t leader_len = 0;
  char byte = 0;

  if (syscall(SYS_get_robust_list, 0, &self_head, &self_len) != 0 ||
      syscall(SYS_get_robust_list, getpid(), &leader_head, &leader_len) != 0 ||
      self_head == NULL || leader_head == NULL || self_head == leader_head ||
      self_len != sizeof(*self_head) || leader_len != sizeof(*leader_head) ||
      write(tid_pipe[1], &tid, sizeof(tid)) != sizeof(tid) ||
      read(release_pipe[0], &byte, sizeof(byte)) != sizeof(byte)) {
    return (void *)(uintptr_t)1;
  }
  return NULL;
}

int main(void) {
  int ready_pipe[2];
  int child_release_pipe[2];
  if (pipe(ready_pipe) != 0 || pipe(child_release_pipe) != 0) {
    return 10;
  }

  pid_t child = fork();
  if (child < 0) {
    return 11;
  }
  if (child == 0) {
    char byte = 1;
    if (write(ready_pipe[1], &byte, sizeof(byte)) != sizeof(byte) ||
        read(child_release_pipe[0], &byte, sizeof(byte)) != sizeof(byte)) {
      _exit(12);
    }
    _exit(0);
  }

  char byte = 0;
  if (read(ready_pipe[0], &byte, sizeof(byte)) != sizeof(byte)) {
    return 13;
  }
  struct robust_list_head *head = (void *)(uintptr_t)1;
  size_t length = 0;
  if (syscall(SYS_get_robust_list, child, &head, &length) != 0 ||
      length != sizeof(*head)) {
    return 14;
  }
  byte = 1;
  if (write(child_release_pipe[1], &byte, sizeof(byte)) != sizeof(byte)) {
    return 15;
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    return 16;
  }
  errno = 0;
  if (syscall(SYS_get_robust_list, child, &head, &length) != -1 ||
      errno != ESRCH) {
    return 17;
  }

  if (pipe(tid_pipe) != 0 || pipe(release_pipe) != 0) {
    return 18;
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    return 19;
  }
  pid_t tid = 0;
  if (read(tid_pipe[0], &tid, sizeof(tid)) != sizeof(tid)) {
    return 20;
  }
  head = NULL;
  length = 0;
  if (syscall(SYS_get_robust_list, tid, &head, &length) != 0 ||
      head == NULL || length != sizeof(*head)) {
    return 21;
  }
  byte = 1;
  if (write(release_pipe[1], &byte, sizeof(byte)) != sizeof(byte)) {
    return 22;
  }
  void *result = NULL;
  if (pthread_join(thread, &result) != 0 || result != NULL) {
    return 23;
  }
  errno = 0;
  if (syscall(SYS_get_robust_list, tid, &head, &length) != -1 ||
      errno != ESRCH) {
    return 24;
  }

  puts("robust-list lifecycle ok");
  return 0;
}
"#,
    );
    let executable = executable.to_str().unwrap();
    let (stdout, stderr) =
        run_host_program_with_tool_captured(executable, &[executable], &directory.0);
    assert_eq!(stdout, b"robust-list lifecycle ok\n");
    assert!(
        stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn real_glibc_scm_rights_translate_across_thread_and_fork_tables() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM SCM_RIGHTS test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program(
        &directory.0,
        "scm-rights-translation",
        r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int sockets[2];
static int received_fds[2] = {-1, -1};

static void *receive_rights(void *unused) {
  (void)unused;
  char payload = 0;
  char control[CMSG_SPACE(2 * sizeof(int))];
  struct iovec iov = {.iov_base = &payload, .iov_len = 1};
  struct msghdr message;
  memset(&message, 0, sizeof(message));
  memset(control, 0, sizeof(control));
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = control;
  message.msg_controllen = sizeof(control);
  if (recvmsg(sockets[1], &message, MSG_CMSG_CLOEXEC) != 1 || payload != 'q' ||
      (message.msg_flags & MSG_CTRUNC) != 0) {
    return (void *)1;
  }
  struct cmsghdr *cmsg = CMSG_FIRSTHDR(&message);
  if (cmsg == NULL || cmsg->cmsg_level != SOL_SOCKET ||
      cmsg->cmsg_type != SCM_RIGHTS ||
      cmsg->cmsg_len != CMSG_LEN(2 * sizeof(int))) {
    return (void *)2;
  }
  memcpy(received_fds, CMSG_DATA(cmsg), sizeof(received_fds));
  return NULL;
}

static int send_rights(const int fds[2]) {
  char payload = 'q';
  char control[CMSG_SPACE(2 * sizeof(int))];
  struct iovec iov = {.iov_base = &payload, .iov_len = 1};
  struct msghdr message;
  memset(&message, 0, sizeof(message));
  memset(control, 0, sizeof(control));
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = control;
  message.msg_controllen = sizeof(control);
  struct cmsghdr *cmsg = CMSG_FIRSTHDR(&message);
  cmsg->cmsg_level = SOL_SOCKET;
  cmsg->cmsg_type = SCM_RIGHTS;
  cmsg->cmsg_len = CMSG_LEN(2 * sizeof(int));
  memcpy(CMSG_DATA(cmsg), fds, 2 * sizeof(int));
  return (int)sendmsg(sockets[0], &message, 0);
}

int main(void) {
  int pipe_fds[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0 || pipe(pipe_fds) != 0 ||
      sockets[0] != 3 || sockets[1] != 4 || pipe_fds[0] != 5 || pipe_fds[1] != 6) {
    return 10;
  }

  int invalid[2] = {pipe_fds[0], 999};
  errno = 0;
  if (send_rights(invalid) != -1 || errno != EBADF) {
    return 11;
  }
  if (send_rights(pipe_fds) != 1) {
    return 12;
  }

  pthread_t thread;
  if (pthread_create(&thread, NULL, receive_rights, NULL) != 0) {
    return 13;
  }
  void *thread_result = NULL;
  if (pthread_join(thread, &thread_result) != 0 || thread_result != NULL) {
    return 14;
  }
  if (received_fds[0] != 7 || received_fds[1] != 8 ||
      (fcntl(received_fds[0], F_GETFD) & FD_CLOEXEC) == 0 ||
      (fcntl(received_fds[1], F_GETFD) & FD_CLOEXEC) == 0) {
    return 15;
  }

  close(pipe_fds[0]);
  close(pipe_fds[1]);
  pid_t child = fork();
  if (child < 0) {
    return 16;
  }
  if (child == 0) {
    char byte = 'z';
    _exit(write(received_fds[1], &byte, 1) == 1 ? 0 : 17);
  }
  char byte = 0;
  int status = 0;
  if (read(received_fds[0], &byte, 1) != 1 || byte != 'z' ||
      waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    return 18;
  }

  puts("scm-rights translation ok");
  return 0;
}
"#,
    );
    let executable = executable.to_str().unwrap();
    let (stdout, stderr) =
        run_host_program_with_tool_captured(executable, &[executable], &directory.0);
    assert_eq!(stdout, b"scm-rights translation ok\n");
    assert!(
        stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn worker_exit_group_terminates_the_root_with_its_status() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM worker exit_group test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    fn append_exit_group(code: &mut Vec<u8>, status: u32) {
        code.extend_from_slice(&[0xb8, 0xe7, 0x00, 0x00, 0x00]);
        code.push(0xbf);
        code.extend_from_slice(&status.to_le_bytes());
        code.extend_from_slice(&[0x0f, 0x05, 0x0f, 0x0b]);
    }

    fn patch_jump(code: &mut [u8], operand: usize, target: usize) {
        let displacement = i32::try_from(target as isize - (operand + 4) as isize).unwrap();
        code[operand..operand + 4].copy_from_slice(&displacement.to_le_bytes());
    }

    const CHILD_TID: u64 = LOAD_ADDRESS + 0x1800;
    const CHILD_STACK: u64 = LOAD_ADDRESS + 0x1900;
    const CHILD_STACK_SIZE: u64 = 0x600;
    let flags = libc::CLONE_VM as u64
        | libc::CLONE_FS as u64
        | libc::CLONE_FILES as u64
        | libc::CLONE_SIGHAND as u64
        | libc::CLONE_THREAD as u64
        | libc::CLONE_CHILD_SETTID as u64
        | libc::CLONE_CHILD_CLEARTID as u64;

    let mut code = Vec::new();
    code.extend_from_slice(&[0xb8, 0xb3, 0x01, 0x00, 0x00]); // mov eax, SYS_clone3
    let clone_args_operand = code.len() + 2;
    code.extend_from_slice(&[0x48, 0xbf, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rdi, clone_args
    code.extend_from_slice(&[0xbe, 0x58, 0x00, 0x00, 0x00]); // mov esi, sizeof(clone_args)
    code.extend_from_slice(&[0x0f, 0x05, 0x85, 0xc0, 0x0f, 0x84, 0, 0, 0, 0]); // syscall; jz child
    let child_jump = code.len() - 4;

    code.extend_from_slice(&[0x41, 0x89, 0xc5]); // mov r13d, eax
    code.extend_from_slice(&[0x48, 0xbf]); // movabs rdi, child_tid
    code.extend_from_slice(&CHILD_TID.to_le_bytes());
    let wait = code.len();
    code.extend_from_slice(&[
        0xbe, 0x00, 0x00, 0x00, 0x00, // mov esi, FUTEX_WAIT
        0x44, 0x89, 0xea, // mov edx, r13d
        0x45, 0x31, 0xd2, // xor r10d, r10d
        0xb8, 0xca, 0x00, 0x00, 0x00, // mov eax, SYS_futex
        0x0f, 0x05, // syscall
        0x83, 0x3f, 0x00, // cmp dword ptr [rdi], 0
        0x0f, 0x85, 0, 0, 0, 0, // jne wait
    ]);
    let wait_jump = code.len() - 4;
    patch_jump(&mut code, wait_jump, wait);
    append_exit_group(&mut code, 0);

    let child = code.len();
    patch_jump(&mut code, child_jump, child);
    append_exit_group(&mut code, 37);

    while !code.len().is_multiple_of(8) {
        code.push(0);
    }
    let clone_args_address = LOAD_ADDRESS + code.len() as u64;
    code[clone_args_operand..clone_args_operand + 8]
        .copy_from_slice(&clone_args_address.to_le_bytes());
    let mut clone_args = [0_u8; 88];
    clone_args[0..8].copy_from_slice(&flags.to_le_bytes());
    clone_args[16..24].copy_from_slice(&CHILD_TID.to_le_bytes());
    clone_args[40..48].copy_from_slice(&CHILD_STACK.to_le_bytes());
    clone_args[48..56].copy_from_slice(&CHILD_STACK_SIZE.to_le_bytes());
    code.extend_from_slice(&clone_args);

    for with_tool in [false, true] {
        let was_blocked = set_interrupt_signal_blocked(true);
        let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
        backend
            .install_static_elf(&static_elf(&code), "/bin/worker-exit-group-test")
            .unwrap();
        let exit_code = if with_tool {
            let (_, exit_code, stdout, stderr) = futures::executor::block_on(
                backend.run_static_elf_with_tool::<StraceTool>((), true),
            )
            .unwrap();
            assert!(stdout.is_empty());
            assert!(stderr.is_empty());
            exit_code
        } else {
            backend.run_static_elf().unwrap()
        };
        let mut child_tid = [0; std::mem::size_of::<i32>()];
        backend.memory().read(CHILD_TID, &mut child_tid).unwrap();
        let remained_blocked = set_interrupt_signal_blocked(was_blocked);
        assert!(remained_blocked, "with_tool={with_tool}");
        assert_eq!(exit_code, 37, "with_tool={with_tool}");
        assert_eq!(i32::from_le_bytes(child_tid), 0, "with_tool={with_tool}");
    }
}

#[test]
fn real_bash_redirects_builtin_output_through_f_dupfd() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM Bash redirection test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let root = TestDirectory::new();
    let (stdout, stderr) = run_host_program_captured(
        "/bin/bash",
        &[
            "bash",
            "--norc",
            "-c",
            "printf redirected > output; printf visible",
        ],
        &root.0,
    );
    assert_eq!(stdout, b"visible");
    assert!(stderr.is_empty());
    assert_eq!(std::fs::read(root.0.join("output")).unwrap(), b"redirected");
}

#[test]
fn real_bash_small_pipeline_uses_legacy_process_clone_tid_flags() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM Bash pipeline test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let root = TestDirectory::new();
    // The child runs to completion before the parent resumes, so this covers a
    // bounded pipeline without claiming concurrent producer/consumer support.
    let (stdout, stderr) = run_host_program_captured(
        "/bin/bash",
        &["bash", "--norc", "-c", "printf abc | /usr/bin/wc -c"],
        &root.0,
    );
    assert_eq!(stdout, b"3\n");
    assert!(
        stderr.is_empty(),
        "unexpected Bash stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn real_coreutils_complete_file_mutation_workflow() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM coreutils test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let root = TestDirectory::new();
    std::fs::write(root.0.join("source"), b"payload\n").unwrap();

    run_host_program("/bin/mkdir", &["mkdir", "-p", "directory/nested"], &root.0);
    run_host_program("/usr/bin/touch", &["touch", "touched"], &root.0);
    run_host_program("/bin/chmod", &["chmod", "600", "touched"], &root.0);
    run_host_program("/bin/ln", &["ln", "source", "hard-link"], &root.0);
    run_host_program("/bin/ln", &["ln", "-s", "source", "symbolic-link"], &root.0);
    run_host_program("/bin/mv", &["mv", "hard-link", "renamed"], &root.0);
    run_host_program("/usr/bin/mkfifo", &["mkfifo", "fifo"], &root.0);
    run_host_program(
        "/usr/bin/install",
        &["install", "-m", "700", "source", "installed"],
        &root.0,
    );
    run_host_program("/bin/rm", &["rm", "renamed"], &root.0);
    run_host_program("/bin/rmdir", &["rmdir", "directory/nested"], &root.0);

    assert!(root.0.join("directory").is_dir());
    assert!(!root.0.join("directory/nested").exists());
    assert_eq!(std::fs::read(root.0.join("source")).unwrap(), b"payload\n");
    assert!(root.0.join("touched").is_file());
    assert_eq!(
        std::fs::read_link(root.0.join("symbolic-link")).unwrap(),
        std::path::Path::new("source")
    );
    assert!(
        std::fs::symlink_metadata(root.0.join("fifo"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    assert_eq!(
        std::fs::read(root.0.join("installed")).unwrap(),
        b"payload\n"
    );
    assert!(!root.0.join("renamed").exists());
}

#[test]
fn static_elf_executes_syscall_and_exits() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM static ELF test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();

    backend
        .memory_mut()
        .write(LOAD_ADDRESS + 0x1000, &[0xff])
        .unwrap();

    // Check BSS and argc, then require deterministic getpid == 1 and preserved
    // RBX. Any loader or SYSCALL return-state error takes the exit_group(42)
    // path rather than producing a false pass.
    let code = [
        0x48, 0xb8, 0x00, 0x10, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, // mov rax, 0x201000
        0x80, 0x38, 0x00, // cmp byte ptr [rax], 0
        0x75, 0x2d, // jne failure
        0x48, 0x83, 0x3c, 0x24, 0x01, // cmp qword ptr [rsp], 1
        0x75, 0x26, // jne failure
        0xbb, 0x78, 0x56, 0x34, 0x12, // mov ebx, 0x12345678
        0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, SYS_getpid
        0x0f, 0x05, // syscall
        0x48, 0x83, 0xf8, 0x01, // cmp rax, 1
        0x75, 0x14, // jne failure
        0x48, 0x81, 0xfb, 0x78, 0x56, 0x34, 0x12, // cmp rbx, 0x12345678
        0x75, 0x0b, // jne failure
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
        0xb8, 0xe7, 0x00, 0x00, 0x00, // failure: mov eax, SYS_exit_group
        0xbf, 0x2a, 0x00, 0x00, 0x00, // mov edi, 42
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    backend
        .install_static_elf(&static_elf(&code), "/bin/true")
        .unwrap();

    assert_eq!(backend.run_static_elf().unwrap(), 0);
}

#[test]
fn static_elf_tool_capture_preserves_configured_stdin() {
    if !kvm_available("KVM captured stdin test") {
        return;
    }

    let directory = TestDirectory::new();
    let stdin_path = directory.0.join("stdin");
    std::fs::write(&stdin_path, b"stdin-through-kvm\n").unwrap();
    let stdin = std::fs::File::open(stdin_path).unwrap();

    let image = std::fs::read("/bin/cat").unwrap();
    let mut backend = KvmBackend::new_with_stdin(256 * 1024 * 1024, Some(stdin)).unwrap();
    backend
        .install_static_elf_with_args(&image, &["/bin/cat"], &["PATH=/usr/bin:/bin"])
        .unwrap();

    let (_, code, stdout, stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();

    assert_eq!(code, 0);
    assert_eq!(stdout, b"stdin-through-kvm\n");
    assert!(stderr.is_empty());

    let mut backend = KvmBackend::new_with_stdin(256 * 1024 * 1024, None).unwrap();
    backend
        .install_static_elf_with_args(&image, &["/bin/cat"], &["PATH=/usr/bin:/bin"])
        .unwrap();
    let (_, code, stdout, stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();

    assert_eq!(code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn kvm_initial_rbp_and_rflags_match_native_linux_process_entry() {
    if !kvm_available("KVM initial-register parity test") {
        return;
    }

    // Capture rbp and rflags before the guest makes its first syscall. The
    // temporary stack adjustment happens only after both entry values are in
    // callee-saved registers.
    let code = [
        0x9c, // pushfq
        0x5b, // pop rbx
        0x49, 0x89, 0xec, // mov r12, rbp
        0x48, 0x83, 0xec, 0x10, // sub rsp, 16
        0x4c, 0x89, 0x24, 0x24, // mov [rsp], r12
        0x48, 0x89, 0x5c, 0x24, 0x08, // mov [rsp + 8], rbx
        0xbf, 0x01, 0x00, 0x00, 0x00, // mov edi, 1
        0x48, 0x89, 0xe6, // mov rsi, rsp
        0xba, 0x10, 0x00, 0x00, 0x00, // mov edx, 16
        0xb8, 0x01, 0x00, 0x00, 0x00, // mov eax, SYS_write
        0x0f, 0x05, // syscall
        0xb8, 0x3c, 0x00, 0x00, 0x00, // mov eax, SYS_exit
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let image = static_elf(&code);
    let executable = TestExecutable::new(&image);
    let mut permissions = std::fs::metadata(&executable.0).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable.0, permissions).unwrap();

    let native = std::process::Command::new(&executable.0).output().unwrap();
    assert!(
        native.status.success(),
        "native entry-register fixture failed: {native:?}",
    );

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&image, "/bin/entry-registers")
        .unwrap();
    let (code, kvm_stdout, kvm_stderr) = backend.run_static_elf_captured().unwrap();
    assert_eq!(
        code,
        0,
        "KVM entry-register fixture failed; stderr={}",
        String::from_utf8_lossy(&kvm_stderr),
    );

    let decode = |label: &str, bytes: &[u8]| {
        assert_eq!(bytes.len(), 16, "{label} emitted the wrong state size");
        let rbp = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let rflags = u64::from_le_bytes(bytes[8..].try_into().unwrap());
        (rbp, rflags)
    };
    let native_state = decode("native", &native.stdout);
    let kvm_state = decode("KVM", &kvm_stdout);

    assert_eq!(native_state.0, 0, "native Linux did not enter with rbp=0");
    assert_eq!(
        native_state.1, 0x202,
        "native Linux did not enter with reserved bit and IF set",
    );
    assert_eq!(
        kvm_state, native_state,
        "KVM must reproduce native Linux's observed rbp and rflags at _start",
    );
}

#[test]
fn kvm_static_elf_getppid_follows_pid_namespace_contract() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM getppid namespace test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    // A static ELF guest that issues getppid and self-checks the deterministic
    // parent PID. Linux PID-namespace semantics give a conventional root guest
    // (detcore ROOT_DETPID == 3) getppid() == 1, while namespace init (PID 1)
    // has getppid() == 0. KVM synthesizes the guest identity and must reproduce
    // those pinned semantics. Any mismatch takes the exit_group(42) path.
    #[rustfmt::skip]
    fn getppid_probe(expected_ppid: u8) -> [u8; 36] {
        [
            0xb8, 0x6e, 0x00, 0x00, 0x00,   // mov eax, SYS_getppid (110)
            0x0f, 0x05,                     // syscall
            0x48, 0x83, 0xf8, expected_ppid, // cmp rax, expected_ppid
            0x75, 0x09,                     // jne failure (skip the 9-byte success block)
            0xb8, 0xe7, 0x00, 0x00, 0x00,   // mov eax, SYS_exit_group (231)
            0x31, 0xff,                     // xor edi, edi
            0x0f, 0x05,                     // syscall  (exit_group(0))
            0xb8, 0xe7, 0x00, 0x00, 0x00,   // failure: mov eax, SYS_exit_group
            0xbf, 0x2a, 0x00, 0x00, 0x00,   // mov edi, 42
            0x0f, 0x05,                     // syscall  (exit_group(42))
            0x0f, 0x0b,                     // ud2
        ]
    }

    // Conventional root guest: detcore ROOT_DETPID == 3 => getppid() == 1.
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&getppid_probe(1)), "/bin/true")
        .unwrap();
    backend.set_root_pid(3).unwrap();
    assert_eq!(
        backend.run_static_elf().unwrap(),
        0,
        "root guest pid=3 must report getppid()==1 in the PID namespace"
    );

    // Namespace init edge case: a guest that is itself PID 1 has no parent.
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&getppid_probe(0)), "/bin/true")
        .unwrap();
    // Default root_pid is already 1; set it explicitly to document intent.
    backend.set_root_pid(1).unwrap();
    assert_eq!(
        backend.run_static_elf().unwrap(),
        0,
        "namespace-init guest pid=1 must report getppid()==0"
    );
}

fn run_stats_program(code: &[u8], name: &str, request: BackendStatsRequest) -> KvmBackendStats {
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend.set_backend_stats_request(request);
    backend.install_static_elf(&static_elf(code), name).unwrap();
    let (_, exit_code, stdout, stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();
    assert_eq!(exit_code, 0, "{name}");
    assert!(stdout.is_empty(), "{name}");
    assert!(stderr.is_empty(), "{name}");

    let snapshot = backend.backend_stats();
    assert_eq!(
        request.collect(&backend),
        request.is_enabled().then(|| snapshot.clone()),
        "{name} request and snapshot source must agree"
    );
    snapshot
}

fn stats_root_program() -> Vec<u8> {
    vec![
        0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, SYS_getpid
        0x0f, 0x05, // syscall
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ]
}

fn patch_stats_jump(code: &mut [u8], operand: usize, target: usize) {
    let displacement = i32::try_from(target as isize - (operand + 4) as isize).unwrap();
    code[operand..operand + 4].copy_from_slice(&displacement.to_le_bytes());
}

fn append_stats_exit(code: &mut Vec<u8>, group: bool) {
    let number = if group {
        libc::SYS_exit_group
    } else {
        libc::SYS_exit
    };
    code.push(0xb8); // mov eax, SYS_exit[_group]
    code.extend_from_slice(&(number as u32).to_le_bytes());
    code.extend_from_slice(&[
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ]);
}

fn stats_fork_program() -> Vec<u8> {
    let mut code = vec![
        0xb8, 0x39, 0x00, 0x00, 0x00, // mov eax, SYS_fork
        0x0f, 0x05, // syscall
        0x85, 0xc0, // test eax, eax
        0x0f, 0x84, 0, 0, 0, 0, // jz child
    ];
    let child_jump = code.len() - 4;

    code.extend_from_slice(&[
        0x89, 0xc7, // mov edi, eax
        0x48, 0x83, 0xec, 0x10, // sub rsp, 16
        0x48, 0x89, 0xe6, // mov rsi, rsp
        0x31, 0xd2, // xor edx, edx
        0x45, 0x31, 0xd2, // xor r10d, r10d
        0xb8, 0x3d, 0x00, 0x00, 0x00, // mov eax, SYS_wait4
        0x0f, 0x05, // syscall
    ]);
    append_stats_exit(&mut code, true);

    let child = code.len();
    patch_stats_jump(&mut code, child_jump, child);
    code.extend_from_slice(&[
        0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, SYS_getpid
        0x0f, 0x05, // syscall
    ]);
    append_stats_exit(&mut code, true);
    code
}

fn clone_thread_program(probe_tool_stacks: bool) -> Vec<u8> {
    const CHILD_TID: u64 = LOAD_ADDRESS + 0x1800;
    const CHILD_STACK: u64 = LOAD_ADDRESS + 0x1900;
    const CHILD_STACK_SIZE: u64 = 0x600;

    let flags = libc::CLONE_VM as u64
        | libc::CLONE_FS as u64
        | libc::CLONE_FILES as u64
        | libc::CLONE_SIGHAND as u64
        | libc::CLONE_THREAD as u64
        | libc::CLONE_SYSVSEM as u64
        | libc::CLONE_CHILD_SETTID as u64
        | libc::CLONE_CHILD_CLEARTID as u64;

    let mut code = vec![0x48, 0xb9]; // movabs rcx, child_tid
    code.extend_from_slice(&CHILD_TID.to_le_bytes());
    code.extend_from_slice(&[
        0xc7, 0x01, 0xff, 0xff, 0xff, 0x7f, // mov dword ptr [rcx], 0x7fffffff
        0xb8, 0xb3, 0x01, 0x00, 0x00, // mov eax, SYS_clone3
        0x48, 0xbf, // movabs rdi, clone_args
    ]);
    let clone_args_operand = code.len();
    code.extend_from_slice(&0_u64.to_le_bytes());
    code.extend_from_slice(&[
        0xbe, 0x58, 0x00, 0x00, 0x00, // mov esi, sizeof(clone_args)
        0x0f, 0x05, // syscall
        0x85, 0xc0, // test eax, eax
        0x0f, 0x84, 0, 0, 0, 0, // jz child
    ]);
    let child_jump = code.len() - 4;

    if probe_tool_stacks {
        code.extend_from_slice(&[
            0xb8, 0x6e, 0x00, 0x00, 0x00, // mov eax, SYS_getppid
            0x0f, 0x05, // syscall
        ]);
    }
    code.extend_from_slice(&[0x48, 0xb9]); // movabs rcx, child_tid
    code.extend_from_slice(&CHILD_TID.to_le_bytes());
    let wait = code.len();
    code.extend_from_slice(&[
        0x83, 0x39, 0x00, // cmp dword ptr [rcx], 0
        0x0f, 0x85, 0, 0, 0, 0, // jne wait
    ]);
    let wait_jump = code.len() - 4;
    patch_stats_jump(&mut code, wait_jump, wait);
    append_stats_exit(&mut code, true);

    let child = code.len();
    patch_stats_jump(&mut code, child_jump, child);
    if probe_tool_stacks {
        code.extend_from_slice(&[
            0xb8, 0x6e, 0x00, 0x00, 0x00, // mov eax, SYS_getppid
            0x0f, 0x05, // syscall
        ]);
    }
    code.extend_from_slice(&[
        0xb8, 0xba, 0x00, 0x00, 0x00, // mov eax, SYS_gettid
        0x0f, 0x05, // syscall
    ]);
    append_stats_exit(&mut code, false);

    while !code.len().is_multiple_of(8) {
        code.push(0);
    }
    let clone_args_address = LOAD_ADDRESS + code.len() as u64;
    // The branch target is patched before the trailing clone arguments are
    // appended, so the optional parent/child probes cannot make it point into
    // the data block.
    code[clone_args_operand..clone_args_operand + 8]
        .copy_from_slice(&clone_args_address.to_le_bytes());
    let mut clone_args = [0_u8; 88];
    clone_args[0..8].copy_from_slice(&flags.to_le_bytes());
    clone_args[16..24].copy_from_slice(&CHILD_TID.to_le_bytes());
    clone_args[40..48].copy_from_slice(&CHILD_STACK.to_le_bytes());
    clone_args[48..56].copy_from_slice(&CHILD_STACK_SIZE.to_le_bytes());
    code.extend_from_slice(&clone_args);
    code
}

fn assert_exact_stats(snapshot: &KvmBackendStats, hypercalls: u64, halts: u64) {
    assert_eq!(snapshot.count(KvmExitReason::Hypercall), hypercalls);
    assert_eq!(snapshot.count(KvmExitReason::Hlt), halts);
    assert_eq!(snapshot.total_exits(), hypercalls + halts, "{snapshot}");
}

#[test]
fn kvm_stats_disabled_run_records_no_exits() {
    if !kvm_available("kvm_stats_disabled_run_records_no_exits") {
        return;
    }

    let snapshot = run_stats_program(
        &stats_root_program(),
        "/bin/kvm-stats-disabled",
        BackendStatsRequest::DISABLED,
    );
    assert_exact_stats(&snapshot, 0, 0);
}

#[test]
fn kvm_stats_root_production_loop_is_exact_and_repeatable() {
    if !kvm_available("kvm_stats_root_production_loop_is_exact_and_repeatable") {
        return;
    }

    let first = run_stats_program(
        &stats_root_program(),
        "/bin/kvm-stats-root",
        BackendStatsRequest::ENABLED,
    );
    let second = run_stats_program(
        &stats_root_program(),
        "/bin/kvm-stats-root",
        BackendStatsRequest::ENABLED,
    );
    assert_exact_stats(&first, 2, 0);
    assert_eq!(first, second);
}

#[test]
fn kvm_stats_fork_process_tree_is_exact_and_repeatable() {
    if !kvm_available("kvm_stats_fork_process_tree_is_exact_and_repeatable") {
        return;
    }

    let first = run_stats_program(
        &stats_fork_program(),
        "/bin/kvm-stats-fork",
        BackendStatsRequest::ENABLED,
    );
    let second = run_stats_program(
        &stats_fork_program(),
        "/bin/kvm-stats-fork",
        BackendStatsRequest::ENABLED,
    );
    // Root: fork + wait4 + exit_group. Child: getpid + exit_group.
    assert_exact_stats(&first, 5, 1);
    assert_eq!(first, second);
}

#[test]
fn kvm_stats_clone_thread_process_tree_is_exact_and_repeatable() {
    if !kvm_available("kvm_stats_clone_thread_process_tree_is_exact_and_repeatable") {
        return;
    }

    let first = run_stats_program(
        &clone_thread_program(false),
        "/bin/kvm-stats-thread",
        BackendStatsRequest::ENABLED,
    );
    let second = run_stats_program(
        &clone_thread_program(false),
        "/bin/kvm-stats-thread",
        BackendStatsRequest::ENABLED,
    );
    // Root: clone3 + exit_group. Child: gettid + exit. The parent waits in
    // guest memory for CLONE_CHILD_CLEARTID, so the child exit is counted first.
    assert_exact_stats(&first, 4, 1);
    assert_eq!(first, second);
}

#[test]
fn tool_owned_threads_keep_independent_stack_guards_live() {
    if !kvm_available("tool_owned_threads_keep_independent_stack_guards_live") {
        return;
    }

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(
            &static_elf(&clone_thread_program(true)),
            "/bin/tool-stack-threads",
        )
        .unwrap();
    let (log, code, stdout, stderr) = futures::executor::block_on(
        backend.run_static_elf_with_tool::<ConcurrentToolStackTool>((), true),
    )
    .unwrap();

    assert_eq!(code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    let tids = log.tids();
    assert_eq!(tids.len(), 2);
    assert_ne!(tids[0], tids[1]);
}

#[test]
fn worker_exec_is_refused_without_replacing_shared_memory() {
    if !kvm_available("worker_exec_is_refused_without_replacing_shared_memory") {
        return;
    }

    let target = [
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let executable = TestExecutable::new(&static_elf(&target));
    let path = executable.0.to_str().unwrap().as_bytes();

    let mut code = clone_thread_program(true);
    let path_address = LOAD_ADDRESS + code.len() as u64;
    code.extend_from_slice(path);
    code.push(0);
    while !code.len().is_multiple_of(8) {
        code.push(0);
    }
    let argv_address = LOAD_ADDRESS + code.len() as u64;
    code.extend_from_slice(&path_address.to_le_bytes());
    code.extend_from_slice(&0_u64.to_le_bytes());
    let envp_address = LOAD_ADDRESS + code.len() as u64;
    code.extend_from_slice(&0_u64.to_le_bytes());

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(
            &static_elf(&code),
            "/bin/worker-exec-preserves-shared-memory",
        )
        .unwrap();
    let config = (
        path_address as usize,
        argv_address as usize,
        envp_address as usize,
    );
    let (log, exit_code, stdout, stderr) = futures::executor::block_on(
        backend.run_static_elf_with_tool::<WorkerExecOverlapTool>(config, true),
    )
    .unwrap();

    assert_eq!(exit_code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(log.worker_errno(), Some(Errno::ENOSYS.into_raw()));
    assert!(log.root_value_preserved());
}

#[test]
fn static_elf_executes_avx_instruction() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM AVX test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    // The host dynamic linker uses this vmovq encoding unconditionally. KVM's
    // userspace-only guest must initialize OSXSAVE and the YMM register state
    // that a Linux kernel would normally configure before entering userspace.
    let code = [
        0x31, 0xff, // xor edi, edi
        0xc4, 0xe1, 0xf9, 0x6e, 0xcf, // vmovq rdi, xmm1
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&code), "/bin/avx-probe")
        .unwrap();

    assert_eq!(backend.run_static_elf().unwrap(), 0);
}

#[test]
fn dynamic_c_guest_observes_indexed_xstate_cpuid_and_lazy_binding() {
    if !kvm_available("dynamic_c_guest_observes_indexed_xstate_cpuid_and_lazy_binding") {
        return;
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program_with_args(
        &directory.0,
        "indexed-xstate-cpuid",
        r#"
#include <cpuid.h>
#include <stdint.h>
#include <stdio.h>

struct registers {
  uint32_t eax;
  uint32_t ebx;
  uint32_t ecx;
  uint32_t edx;
};

static struct registers cpuid_xstate(uint32_t subleaf) {
  struct registers result;
  __cpuid_count(0xd, subleaf, result.eax, result.ebx, result.ecx, result.edx);
  return result;
}

static int matches(struct registers actual, struct registers expected) {
  return actual.eax == expected.eax && actual.ebx == expected.ebx &&
         actual.ecx == expected.ecx && actual.edx == expected.edx;
}

int main(void) {
  struct registers xstate = cpuid_xstate(0);
  /* KVM recomputes subleaf 0 EBX from XCR0 and the host-supported component
     layout. With x87, SSE, and AVX enabled, the guest-visible size is 0x340. */
  if (xstate.eax != 0x00000007 || xstate.ebx != 0x00000340 ||
      xstate.ecx != 0x00000340 || xstate.edx != 0) {
    return 10;
  }

  static const struct {
    uint32_t subleaf;
    struct registers expected;
  } cases[] = {
      {1, {0, 0, 0, 0}},
      {2, {0x00000100, 0x00000240, 0, 0}},
      {17, {0, 0, 0, 0}},
      {18, {0, 0, 0, 0}},
      {19, {0, 0, 0, 0}},
  };

  for (uint32_t i = 0; i < sizeof(cases) / sizeof(cases[0]); ++i) {
    if (!matches(cpuid_xstate(cases[i].subleaf), cases[i].expected)) {
      return 11 + (int)i;
    }
  }

  /* The executable is linked for lazy binding, so this first puts call makes
     the dynamic linker resolve its PLT entry after all CPUID checks pass. */
  if (puts("indexed xstate cpuid ok") < 0) {
    return 20;
  }
  return 0;
}
"#,
        &["-fno-builtin", "-Wl,-z,lazy"],
    );
    let image = std::fs::read(&executable).unwrap();
    let elf = goblin::elf::Elf::parse(&image).unwrap();
    assert!(elf.interpreter.is_some(), "test guest must be dynamic");
    let dynamic = elf
        .dynamic
        .as_ref()
        .expect("test guest must have a dynamic section");
    assert!(
        dynamic
            .dyns
            .iter()
            .all(|entry| entry.d_tag != goblin::elf::dynamic::DT_BIND_NOW),
        "test guest must not contain DT_BIND_NOW"
    );
    assert_eq!(
        dynamic.info.flags & goblin::elf::dynamic::DF_BIND_NOW,
        0,
        "test guest must not contain DF_BIND_NOW"
    );
    assert_eq!(
        dynamic.info.flags_1 & goblin::elf::dynamic::DF_1_NOW,
        0,
        "test guest must not contain DF_1_NOW"
    );
    assert!(
        elf.pltrelocs.iter().any(|relocation| {
            elf.dynsyms
                .get(relocation.r_sym)
                .and_then(|symbol| elf.dynstrtab.get_at(symbol.st_name))
                == Some("puts")
        }),
        "test guest must call puts through a PLT relocation"
    );

    let executable = executable.to_str().unwrap();
    let (stdout, stderr) = run_host_program_captured(executable, &[executable], &directory.0);
    assert_eq!(stdout, b"indexed xstate cpuid ok\n");
    assert!(stderr.is_empty());
}

#[test]
fn static_elf_receives_argv_and_envp() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM argv/envp test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    // exit_group(42): the failure path taken by every self-check below. Exactly
    // 12 bytes, so each conditional jump that skips it uses rel8 = 0x0c.
    const FAIL: [u8; 12] = [
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0xbf, 0x2a, 0x00, 0x00, 0x00, // mov edi, 42
        0x0f, 0x05, // syscall
    ];

    // The guest verifies the System V initial stack that the loader built for
    // argv = ["prog", "second"], envp = ["FOO=bar"]:
    //   [rsp+0]=argc [rsp+8]=argv0 [rsp+16]=argv1 [rsp+24]=NULL
    //   [rsp+32]=envp0 [rsp+40]=NULL
    // Any mismatch takes exit_group(42); success prints and exit_group(0).
    let message = b"hello from kvm m1\n";
    let mut code: Vec<u8> = Vec::new();
    // argc == 2
    code.extend_from_slice(&[0x48, 0x83, 0x3c, 0x24, 0x02, 0x74, 0x0c]); // cmp qword[rsp],2; je +12
    code.extend_from_slice(&FAIL);
    // argv[1] != 0
    code.extend_from_slice(&[0x48, 0x8b, 0x44, 0x24, 0x10, 0x48, 0x85, 0xc0, 0x75, 0x0c]); // mov rax,[rsp+16]; test; jne +12
    code.extend_from_slice(&FAIL);
    // envp[0] != 0
    code.extend_from_slice(&[0x48, 0x8b, 0x44, 0x24, 0x20, 0x48, 0x85, 0xc0, 0x75, 0x0c]); // mov rax,[rsp+32]; test; jne +12
    code.extend_from_slice(&FAIL);
    // envp[1] == 0 (single environment entry, then the NULL terminator)
    code.extend_from_slice(&[0x48, 0x8b, 0x44, 0x24, 0x28, 0x48, 0x85, 0xc0, 0x74, 0x0c]); // mov rax,[rsp+40]; test; je +12
    code.extend_from_slice(&FAIL);
    // write(1, message, message.len())
    code.extend_from_slice(&[0xbf, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1
    let movabs_operand = code.len() + 2;
    code.extend_from_slice(&[0x48, 0xbe, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rsi, <message vaddr>
    code.push(0xba);
    code.extend_from_slice(&(message.len() as u32).to_le_bytes()); // mov edx, len
    code.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x05]); // mov eax,SYS_write; syscall
    // exit_group(0)
    code.extend_from_slice(&[
        0xb8, 0xe7, 0x00, 0x00, 0x00, 0x31, 0xff, 0x0f, 0x05, 0x0f, 0x0b,
    ]); // mov eax,231; xor edi,edi; syscall; ud2
    let message_offset = code.len();
    code.extend_from_slice(message);
    let message_vaddr = LOAD_ADDRESS + message_offset as u64;
    code[movabs_operand..movabs_operand + 8].copy_from_slice(&message_vaddr.to_le_bytes());

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf_with_args(&static_elf(&code), &["prog", "second"], &["FOO=bar"])
        .unwrap();

    assert_eq!(backend.run_static_elf().unwrap(), 0);
}

#[test]
fn tool_receives_post_exec_with_guest_auxv() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM post-exec test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let code = [
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf_with_args(&static_elf(&code), &["prog"], &[])
        .unwrap();

    let (log, exit_code, _, _) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<PostExecTool>((), true))
            .unwrap();

    assert_eq!(exit_code, 0);
    assert_eq!(log.calls(), 1);
    let address = log
        .at_random()
        .expect("post-exec hook did not observe AT_RANDOM");
    let mut random = [0; 16];
    backend.memory().read(address as u64, &mut random).unwrap();
    assert_eq!(random, POST_EXEC_RANDOM);
}

#[test]
fn canonical_initial_execveat_does_not_reload_installed_image() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM canonical initial exec test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let code = [
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let image = static_elf(&code);
    let executable = TestExecutable::new(&image);
    let executable = executable.0.to_str().unwrap();
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend.install_static_elf(&image, executable).unwrap();

    // A second exec of the same file clears the loaded segment before
    // reloading it. This address is in mapped BSS but outside the file-backed
    // bytes, so it distinguishes forwarding the synthetic initial exec from a
    // real reload.
    const SENTINEL_ADDRESS: u64 = LOAD_ADDRESS + 0x1000;
    const SENTINEL: [u8; 16] = *b"initial-exec-ok!";
    backend
        .memory_mut()
        .write(SENTINEL_ADDRESS, &SENTINEL)
        .unwrap();

    let (log, exit_code, stdout, stderr) = futures::executor::block_on(
        backend.run_static_elf_with_tool::<CanonicalInitialExecTool>((), true),
    )
    .unwrap();

    assert_eq!(exit_code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(log.calls(), 1);
    let mut observed = [0; SENTINEL.len()];
    backend
        .memory()
        .read(SENTINEL_ADDRESS, &mut observed)
        .unwrap();
    assert_eq!(observed, SENTINEL);
}

#[test]
fn tool_receives_post_exec_after_root_execve() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM post-exec replacement test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let target = [
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let executable = TestExecutable::new(&static_elf(&target));
    let path = executable.0.to_str().unwrap().as_bytes();

    let mut root = Vec::new();
    let path_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xbf, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rdi, path
    let argv_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xbe, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rsi, argv
    let envp_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xba, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rdx, envp
    root.extend_from_slice(&[
        0xb8, 0x3b, 0x00, 0x00, 0x00, 0x0f, 0x05, // execve
        0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x2a, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f,
        0x0b, // exit_group(42); ud2
    ]);

    let path_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(path);
    root.push(0);
    while !root.len().is_multiple_of(8) {
        root.push(0);
    }
    let argv_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(&path_address.to_le_bytes());
    root.extend_from_slice(&0_u64.to_le_bytes());
    let envp_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(&0_u64.to_le_bytes());
    root[path_operand..path_operand + 8].copy_from_slice(&path_address.to_le_bytes());
    root[argv_operand..argv_operand + 8].copy_from_slice(&argv_address.to_le_bytes());
    root[envp_operand..envp_operand + 8].copy_from_slice(&envp_address.to_le_bytes());

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&root), "/bin/root-exec-test")
        .unwrap();

    let (log, exit_code, stdout, stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<PostExecTool>((), true))
            .unwrap();

    assert_eq!(exit_code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(log.calls(), 2);
}

#[test]
fn tool_executes_from_thread_start_before_initial_entry() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM thread-start exec test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let target = [
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let executable = TestExecutable::new(&static_elf(&target));
    let path = executable.0.to_str().unwrap().as_bytes();

    let mut root = vec![
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0xbf, 0x2a, 0x00, 0x00, 0x00, // mov edi, 42
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let path_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(path);
    root.push(0);
    while !root.len().is_multiple_of(8) {
        root.push(0);
    }
    let argv_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(&path_address.to_le_bytes());
    root.extend_from_slice(&0_u64.to_le_bytes());
    let envp_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(&0_u64.to_le_bytes());

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&root), "/bin/thread-start-exec-test")
        .unwrap();
    let config = (
        path_address as usize,
        argv_address as usize,
        envp_address as usize,
    );
    let (log, exit_code, stdout, stderr) = futures::executor::block_on(
        backend.run_static_elf_with_tool::<StartExecTool>(config, true),
    )
    .unwrap();

    assert_eq!(exit_code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(log.post_exec_calls(), 1);
}

#[test]
fn regular_injected_forks_complete_before_return() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM regular fork injection test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let code = [
        0xb8, 0x39, 0x00, 0x00, 0x00, // mov eax, SYS_fork
        0x0f, 0x05, // syscall
        0x89, 0xc7, // mov edi, eax
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&code), "/bin/double-injected-fork-test")
        .unwrap();

    let (_, exit_code, stdout, stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<DoubleForkTool>((), true))
            .unwrap();

    assert_eq!(exit_code, 2);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn malformed_exec_returns_enoexec_without_replacing_image() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM malformed exec test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let executable = TestExecutable::new(b"not an ELF image");
    let path = executable.0.to_str().unwrap().as_bytes();
    let mut root = Vec::new();
    let path_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xbf, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rdi, path
    let argv_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xbe, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rsi, argv
    let envp_operand = root.len() + 2;
    root.extend_from_slice(&[0x48, 0xba, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rdx, envp
    root.extend_from_slice(&[
        0xb8, 0x3b, 0x00, 0x00, 0x00, 0x0f, 0x05, // execve
        0xf7, 0xd8, // neg eax
        0x89, 0xc7, // mov edi, eax
        0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, // exit_group(errno)
        0x0f, 0x0b, // ud2
    ]);
    let path_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(path);
    root.push(0);
    while !root.len().is_multiple_of(8) {
        root.push(0);
    }
    let argv_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(&path_address.to_le_bytes());
    root.extend_from_slice(&0_u64.to_le_bytes());
    let envp_address = LOAD_ADDRESS + root.len() as u64;
    root.extend_from_slice(&0_u64.to_le_bytes());
    root[path_operand..path_operand + 8].copy_from_slice(&path_address.to_le_bytes());
    root[argv_operand..argv_operand + 8].copy_from_slice(&argv_address.to_le_bytes());
    root[envp_operand..envp_operand + 8].copy_from_slice(&envp_address.to_le_bytes());

    for with_tool in [false, true] {
        let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
        backend
            .install_static_elf(&static_elf(&root), "/bin/malformed-exec-test")
            .unwrap();
        let (exit_code, stdout, stderr) = if with_tool {
            let (_, exit_code, stdout, stderr) = futures::executor::block_on(
                backend.run_static_elf_with_tool::<StraceTool>((), true),
            )
            .unwrap();
            (exit_code, stdout, stderr)
        } else {
            backend.run_static_elf_captured().unwrap()
        };
        assert_eq!(exit_code, libc::ENOEXEC, "with_tool={with_tool}");
        assert!(stdout.is_empty(), "with_tool={with_tool}");
        assert!(stderr.is_empty(), "with_tool={with_tool}");
    }
}

#[test]
fn post_exec_failure_runs_tool_exit_lifecycle() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM post-exec failure test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    POST_EXEC_FAILURE_EXITED.store(false, Ordering::SeqCst);
    let code = [
        0xb8, 0xe7, 0x00, 0x00, 0x00, 0x31, 0xff, 0x0f, 0x05, 0x0f, 0x0b,
    ];
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf_with_args(&static_elf(&code), &["prog"], &[])
        .unwrap();

    let error = futures::executor::block_on(
        backend.run_static_elf_with_tool::<FailingPostExecTool>((), true),
    )
    .unwrap_err();

    assert!(error.to_string().contains("post-exec hook failed"));
    assert!(POST_EXEC_FAILURE_EXITED.load(Ordering::SeqCst));
}

#[test]
fn strace_tool_logs_syscalls_from_static_elf() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM strace-ELF test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    // A static ELF guest that issues getpid, write(1, "hi\n", 3), exit_group(0)
    // via real SYSCALL instructions. Each traps through the ring0 trampoline and
    // must be observed by StraceTool, whose tail_inject is serviced by the ELF
    // guest kernel (so getpid returns 1, the write prints, and exit_group ends
    // the run). The synthetic initial exec is delivered too, but this test tool
    // records only after injection and successful exec does not return.
    let message = b"hi\n";
    let mut code: Vec<u8> = Vec::new();
    code.extend_from_slice(&[0xb8, 0x27, 0x00, 0x00, 0x00, 0x0f, 0x05]); // mov eax,SYS_getpid; syscall
    code.extend_from_slice(&[0xbf, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1
    let movabs_operand = code.len() + 2;
    code.extend_from_slice(&[0x48, 0xbe, 0, 0, 0, 0, 0, 0, 0, 0]); // movabs rsi, <message vaddr>
    code.push(0xba);
    code.extend_from_slice(&(message.len() as u32).to_le_bytes()); // mov edx, len
    code.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x05]); // mov eax,SYS_write; syscall
    code.extend_from_slice(&[
        0xb8, 0xe7, 0x00, 0x00, 0x00, 0x31, 0xff, 0x0f, 0x05, 0x0f, 0x0b,
    ]); // mov eax,SYS_exit_group; xor edi,edi; syscall; ud2
    let message_offset = code.len();
    code.extend_from_slice(message);
    let message_vaddr = LOAD_ADDRESS + message_offset as u64;
    code[movabs_operand..movabs_operand + 8].copy_from_slice(&message_vaddr.to_le_bytes());

    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf_with_args(&static_elf(&code), &["prog"], &[])
        .unwrap();

    let (log, exit_code, stdout, stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, b"hi\n");
    assert!(stderr.is_empty());
    assert_eq!(
        log.syscalls(),
        vec![
            "getpid".to_string(),
            "write".to_string(),
            "exit_group".to_string(),
        ],
    );
}

#[test]
fn tool_rpc_response_reaches_intercepted_static_elf_syscall() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM RPC round-trip test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    // Each getpid is intercepted instead of injected. The local tool advances
    // its ThreadState, sends the ordinal to GlobalState, and returns the typed
    // RPC response as the guest-visible syscall result. The exit hook then
    // sends a third typed RPC through the lifecycle GlobalRPC handle. Exit 1
    // if either guest-visible round trip produced an unexpected value.
    let code = [
        0x45, 0x31, 0xe4, // xor r12d, r12d
        0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, SYS_getpid
        0x0f, 0x05, // syscall
        0x3d, 0xe9, 0x03, 0x00, 0x00, // cmp eax, 1001
        0x41, 0x0f, 0x95, 0xc4, // setne r12b
        0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, SYS_getpid
        0x0f, 0x05, // syscall
        0x3d, 0xea, 0x03, 0x00, 0x00, // cmp eax, 1002
        0x0f, 0x95, 0xc0, // setne al
        0x0f, 0xb6, 0xc0, // movzx eax, al
        0x41, 0x09, 0xc4, // or r12d, eax
        0x44, 0x89, 0xe7, // mov edi, r12d
        0xb8, 0xe7, 0x00, 0x00, 0x00, // mov eax, SYS_exit_group
        0x0f, 0x05, // syscall
        0x0f, 0x0b, // ud2
    ];
    let mut backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    backend
        .install_static_elf(&static_elf(&code), "/bin/rpc-round-trip")
        .unwrap();

    let (log, exit_code, stdout, stderr) = futures::executor::block_on(
        backend.run_static_elf_with_tool::<RpcRoundTripTool>(1000, true),
    )
    .unwrap();

    assert_eq!(exit_code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(
        log.requests(),
        vec![
            (Pid::from_raw(1), 1),
            (Pid::from_raw(1), 2),
            (Pid::from_raw(1), 3),
        ]
    );
}

#[test]
fn counter_tools_aggregate_intercepted_static_elf_syscalls() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM counter-ELF test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let code = [
        0xb8, 0x27, 0x00, 0x00, 0x00, 0x0f, 0x05, // getpid
        0xb8, 0x27, 0x00, 0x00, 0x00, 0x0f, 0x05, // getpid
        0xb8, 0xe7, 0x00, 0x00, 0x00, // exit_group
        0x31, 0xff, 0x0f, 0x05, // status 0; syscall
        0x0f, 0x0b, // ud2
    ];
    let image = static_elf(&code);

    let mut direct_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    direct_backend
        .install_static_elf(&image, "/bin/counter-rpc")
        .unwrap();
    let (counter, exit_code, stdout, stderr) = futures::executor::block_on(
        direct_backend.run_static_elf_with_tool::<CounterTool>((), true),
    )
    .unwrap();
    assert_eq!(exit_code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(counter.total(), 4);

    let mut hierarchical_backend = KvmBackend::new(MEMORY_SIZE).unwrap();
    hierarchical_backend
        .install_static_elf(&image, "/bin/hierarchical-counter-rpc")
        .unwrap();
    let (counter, exit_code, stdout, stderr) = futures::executor::block_on(
        hierarchical_backend.run_static_elf_with_tool::<HierarchicalCounterTool>((), true),
    )
    .unwrap();
    assert_eq!(exit_code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(
        counter.totals(),
        HierarchicalTotals {
            total_syscalls: 4,
            exited_procs: 1,
            exited_threads: 1,
        }
    );
}

#[test]
fn real_make_runs_a_shell_recipe_through_clone3_vfork() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM make test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }
    let root = TestDirectory::new();
    std::fs::write(
        root.0.join("Makefile"),
        "all: result.txt\nresult.txt:\n\tprintf 'make:42\\n' > result.txt\n",
    )
    .unwrap();
    run_host_program("/usr/bin/make", &["make", "-s"], &root.0);
    assert_eq!(
        std::fs::read(root.0.join("result.txt")).unwrap(),
        b"make:42\n"
    );
}

#[test]
fn real_gcc_compiles_an_object_through_child_processes() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM gcc test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }
    let root = TestDirectory::new();
    std::fs::write(
        root.0.join("fixture.c"),
        b"int hermit_compat(void) { return 42; }\n",
    )
    .unwrap();
    run_host_program(
        "/usr/bin/gcc",
        &[
            "gcc",
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-fno-ident",
            "-frandom-seed=hermit-gcc",
            "-c",
            "fixture.c",
            "-o",
            "fixture.o",
        ],
        &root.0,
    );
    assert!(root.0.join("fixture.o").is_file());
}

#[test]
fn real_patch_applies_exact_hunk_with_absent_xattrs() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM patch test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let root = TestDirectory::new();
    std::fs::write(root.0.join("file"), b"old\n").unwrap();
    std::fs::write(
        root.0.join("change.patch"),
        b"--- file\n+++ file\n@@ -1 +1 @@\n-old\n+new\n",
    )
    .unwrap();
    let (stdout, stderr) = run_host_program_captured(
        "/usr/bin/patch",
        &["patch", "--quiet", "--input=change.patch", "file"],
        &root.0,
    );
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(std::fs::read(root.0.join("file")).unwrap(), b"new\n");
}

#[test]
fn real_grep_uses_synthetic_process_maps_for_stack_discovery() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM grep test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let root = TestDirectory::new();
    std::fs::write(root.0.join("payload"), b"gamma\nbeta\nalpha\nbeta\n").unwrap();
    let (stdout, stderr) =
        run_host_program_captured("/usr/bin/grep", &["grep", "beta", "payload"], &root.0);
    assert_eq!(stdout, b"beta\nbeta\n");
    assert!(stderr.is_empty());
}

fn static_elf(code: &[u8]) -> Vec<u8> {
    let mut image = vec![0; CODE_OFFSET + code.len()];

    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = 2;
    image[5] = 1;
    image[6] = 1;
    put_u16(&mut image, 16, 2);
    put_u16(&mut image, 18, 62);
    put_u32(&mut image, 20, 1);
    put_u64(&mut image, 24, LOAD_ADDRESS);
    put_u64(&mut image, 32, 64);
    put_u16(&mut image, 52, 64);
    put_u16(&mut image, 54, 56);
    put_u16(&mut image, 56, 1);

    put_u32(&mut image, 64, 1);
    put_u32(&mut image, 68, 5);
    put_u64(&mut image, 72, CODE_OFFSET as u64);
    put_u64(&mut image, 80, LOAD_ADDRESS);
    put_u64(&mut image, 88, LOAD_ADDRESS);
    put_u64(&mut image, 96, code.len() as u64);
    put_u64(&mut image, 104, 0x2000);
    put_u64(&mut image, 112, 0x1000);
    image[CODE_OFFSET..].copy_from_slice(code);
    image
}

fn put_u16(image: &mut [u8], offset: usize, value: u16) {
    image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(image: &mut [u8], offset: usize, value: u32) {
    image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(image: &mut [u8], offset: usize, value: u64) {
    image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// A LIVE pthread worker exercises the `pid != tid` path end to end.
///
/// ⚠️ THIS IS THE CASE THE REJECTED IMPLEMENTATION COULD NOT SEE. Every earlier
/// signal test ran with `pid == tid`, so validating a thread-directed target
/// against `state.pid` looked correct. Under a real worker it is wrong in both
/// directions at once: the worker's own `tkill(gettid(), 0)` was refused with
/// ESRCH, while a LEADER-targeted request was accepted and then evaluated
/// against the worker's signal state.
#[test]
fn real_pthread_worker_signals_itself_by_tid_and_is_refused_for_the_leader() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM worker-signal test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program(
        &directory.0,
        "worker-thread-signal-identity",
        r#"
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <sys/syscall.h>
#include <unistd.h>

static atomic_int result;

static void *worker(void *unused) {
  (void)unused;
  pid_t pid = getpid();
  pid_t tid = (pid_t)syscall(SYS_gettid);

  // The premise. If a worker's tid equalled its pid this test would prove
  // nothing, which is exactly how the rejected version passed review's tests.
  if (tid == pid) {
    atomic_store(&result, 20);
    return NULL;
  }
  // A worker signalling ITSELF by tid must succeed.
  if (syscall(SYS_tkill, tid, 0) != 0) {
    atomic_store(&result, 21);
    return NULL;
  }
  // ...and by (tgid, tid).
  if (syscall(SYS_tgkill, pid, tid, 0) != 0) {
    atomic_store(&result, 22);
    return NULL;
  }
  // ⚠️ THE REFUSAL. Naming the LEADER from the worker must fail visibly rather
  // than being applied to this thread. Any success here is the rejected bug.
  if (syscall(SYS_tgkill, pid, pid, 0) == 0) {
    atomic_store(&result, 23);
    return NULL;
  }
  // A non-positive thread id is EINVAL, not ESRCH.
  if (syscall(SYS_tkill, 0, 0) == 0 || errno != EINVAL) {
    atomic_store(&result, 24);
    return NULL;
  }
  atomic_store(&result, 1);
  return NULL;
}

int main(void) {
  atomic_store(&result, 0);
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    return 10;
  }
  if (pthread_join(thread, NULL) != 0) {
    return 11;
  }
  int observed = atomic_load(&result);
  return observed == 1 ? 0 : observed;
}
"#,
    );
    let executable = executable.to_str().unwrap();
    let image = std::fs::read(executable).unwrap();
    let mut backend = KvmBackend::new(256 * 1024 * 1024).unwrap();
    backend
        .install_static_elf_with_context(
            &image,
            &[executable],
            &["PATH=/usr/bin:/bin"],
            &directory.0,
        )
        .unwrap();
    let (_trace, code, _stdout, _stderr) =
        futures::executor::block_on(backend.run_static_elf_with_tool::<StraceTool>((), true))
            .unwrap();
    assert_eq!(
        code, 0,
        "worker signal-identity guest failed with code {code}; \
         20=pid==tid so the case is vacuous, 21=tkill(gettid()) refused, \
         22=tgkill(getpid(),gettid()) refused, 23=leader-targeted call WRONGLY \
         SUCCEEDED, 24=tkill(0) not EINVAL"
    );
}

#[derive(Default)]
struct ChildWaitEventLog {
    events: Mutex<Vec<BackendChildWaitEvent>>,
}

#[reverie::global_tool]
impl GlobalTool for ChildWaitEventLog {
    type Request = ();
    type Response = ();
    type Config = ();

    async fn receive_rpc(&self, _from: Pid, (): ()) {}

    async fn on_backend_child_wait_event(
        &self,
        event: BackendChildWaitEvent,
    ) -> Result<(), reverie::Error> {
        self.events
            .lock()
            .expect("child wait-event log poisoned")
            .push(event);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ChildWaitEventTool;

#[reverie::tool]
impl Tool for ChildWaitEventTool {
    type GlobalState = ChildWaitEventLog;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        let mut subscriptions = Subscription::none();
        subscriptions.syscalls([Sysno::fork, Sysno::wait4, Sysno::rt_sigaction]);
        subscriptions
    }
}

#[test]
fn child_waitability_callback_and_auto_reap_are_observed_on_real_kvm() {
    match Kvm::new() {
        Ok(_) => {}
        Err(error) if kvm_is_unavailable(&error) => {
            eprintln!("skipping KVM child-lifecycle test: cannot open /dev/kvm: {error}");
            return;
        }
        Err(error) => panic!("failed to probe /dev/kvm: {error}"),
    }

    let directory = TestDirectory::new();
    let executable = compile_c_program(
        &directory.0,
        "child-waitability",
        r#"
#include <errno.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>

static void child_exit(int code) {
  pid_t child = fork();
  if (child < 0) _exit(90);
  if (child == 0) _exit(code);
}

int main(void) {
  struct sigaction action = {0};
  action.sa_handler = (void (*)(int))0x4321;
  sigemptyset(&action.sa_mask);
  errno = 0;
  /* Installing a real SIGCHLD handler must SUCCEED, as it does natively and under
     ptrace. What this test is really for is unchanged below: with a real handler
     installed SIGCHLD does not auto-reap, so the child must still be waitable.
     (This handler address is never invoked.) */
  if (sigaction(SIGCHLD, &action, 0) != 0) return 10;

  child_exit(7);
  int status = 0;
  if (waitpid(-1, &status, 0) <= 0 || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 7) return 11;

  action.sa_handler = SIG_IGN;
  action.sa_flags = 0;
  if (sigaction(SIGCHLD, &action, 0) != 0) return 12;
  child_exit(8);
  if (waitpid(-1, &status, 0) != -1 || errno != ECHILD) return 13;

  action.sa_handler = SIG_DFL;
  action.sa_flags = SA_NOCLDWAIT;
  if (sigaction(SIGCHLD, &action, 0) != 0) return 14;
  child_exit(9);
  if (waitpid(-1, &status, 0) != -1 || errno != ECHILD) return 15;

  return 0;
}
"#,
    );
    let executable = executable.to_str().unwrap();
    let image = std::fs::read(executable).unwrap();
    let mut backend = KvmBackend::new(256 * 1024 * 1024).unwrap();
    backend
        .install_static_elf_with_context(
            &image,
            &[executable],
            &["PATH=/usr/bin:/bin"],
            &directory.0,
        )
        .unwrap();

    let (global, code, _stdout, stderr) = futures::executor::block_on(
        backend.run_static_elf_with_tool::<ChildWaitEventTool>((), true),
    )
    .unwrap();
    assert_eq!(
        code,
        0,
        "child lifecycle guest failed with code {code}; stderr={}",
        String::from_utf8_lossy(&stderr),
    );

    let events = global
        .events
        .lock()
        .expect("child wait-event log poisoned")
        .clone();
    assert_eq!(
        events,
        vec![
            BackendChildWaitEvent {
                parent: Pid::from_raw(1),
                child: Pid::from_raw(2),
                state: BackendChildWaitState::Exited {
                    status: ExitStatus::Exited(7),
                    waitable: true
                },
            },
            BackendChildWaitEvent {
                parent: Pid::from_raw(1),
                child: Pid::from_raw(3),
                state: BackendChildWaitState::Exited {
                    status: ExitStatus::Exited(8),
                    waitable: false
                },
            },
            BackendChildWaitEvent {
                parent: Pid::from_raw(1),
                child: Pid::from_raw(4),
                state: BackendChildWaitState::Exited {
                    status: ExitStatus::Exited(9),
                    waitable: false
                },
            },
        ],
        "the backend callback must describe every real terminal waitability transition",
    );
}
