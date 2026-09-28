use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(target_arch = "x86_64")]
use reverie::CpuIdResult;
use reverie::Error;
use reverie::ExitStatus;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
#[cfg(target_arch = "x86_64")]
use reverie::Rdtsc;
#[cfg(target_arch = "x86_64")]
use reverie::RdtscResult;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::process::Command;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie_liteinst::LiteinstBackend;
use reverie_liteinst::STRADDLER_STALENESS_TICKS_ENV;

// Linux truncates PR_SET_NAME to 15 bytes.  Give every concurrently running
// fixture a distinct, exact-width marker so one test never mistakes another
// test's still-live process (or terminal zombie awaiting its own reaper) for a
// cleanup failure.  The cleanup assertions remain fail-closed: they still
// require zero processes carrying this test's marker.
static PROCESS_NAME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn unique_process_name() -> String {
    let sequence = PROCESS_NAME_SEQUENCE.fetch_add(1, Ordering::SeqCst) & 0x000f_ffff;
    let identity = ((std::process::id() as u64) << 20) | sequence;
    format!("li{:013x}", identity)
}

#[derive(Debug, Default)]
struct EventCounter {
    delivered: AtomicU64,
    task_creation_events: AtomicU64,
    cpuid_events: AtomicU64,
    cpuid_interception: AtomicU64,
    rdtsc_events: AtomicU64,
    last_getpid_rip: AtomicU64,
    last_getpid_r12: AtomicU64,
    helper_mprotect_callbacks: AtomicU64,
}

#[reverie::global_tool]
impl GlobalTool for EventCounter {
    type Request = u64;
    type Response = ();
    type Config = ();

    async fn receive_rpc(&self, _from: Tid, increment: u64) {
        if increment & (1_u64 << 63) != 0 {
            self.last_getpid_rip
                .store(increment & ((1_u64 << 62) - 1), Ordering::SeqCst);
            self.delivered.fetch_add(1, Ordering::SeqCst);
        } else if increment & (1_u64 << 62) != 0 {
            self.last_getpid_r12
                .store(increment & ((1_u64 << 62) - 1), Ordering::SeqCst);
        } else if increment & (1_u64 << 61) != 0 {
            self.helper_mprotect_callbacks
                .fetch_add(1, Ordering::SeqCst);
        } else if increment & (1_u64 << 60) != 0 {
            self.cpuid_events.fetch_add(1, Ordering::SeqCst);
        } else if increment & (1_u64 << 59) != 0 {
            self.rdtsc_events.fetch_add(1, Ordering::SeqCst);
        } else if increment & (1_u64 << 58) != 0 {
            self.cpuid_interception.store(1, Ordering::SeqCst);
        } else if increment & (1_u64 << 57) != 0 {
            self.task_creation_events.fetch_add(1, Ordering::SeqCst);
        } else {
            self.delivered.fetch_add(increment, Ordering::SeqCst);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[derive(Default)]
struct ActivationCpuEvents;

#[cfg(target_arch = "x86_64")]
#[reverie::tool]
impl Tool for ActivationCpuEvents {
    type GlobalState = EventCounter;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        Subscription::all()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        Ok(guest.inject(syscall).await?)
    }

    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), reverie::Errno> {
        if guest.has_cpuid_interception() {
            guest.send_rpc(1_u64 << 58).await;
        }
        Ok(())
    }

    async fn handle_cpuid_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        eax: u32,
        ecx: u32,
    ) -> Result<CpuIdResult, reverie::Errno> {
        guest.send_rpc(1_u64 << 60).await;
        // Keep the loader's required x86-64 feature floor while proving that
        // the Tool, rather than the guest, decides the observed result.
        let native = std::arch::x86_64::__cpuid_count(eax, ecx);
        Ok(CpuIdResult {
            eax: native.eax,
            ebx: native.ebx,
            ecx: native.ecx,
            edx: native.edx,
        })
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: Rdtsc,
    ) -> Result<RdtscResult, reverie::Errno> {
        guest.send_rpc(1_u64 << 59).await;
        Ok(RdtscResult {
            tsc: 0x1234_5678,
            aux: (request == Rdtsc::Tscp).then_some(0x42),
        })
    }
}

#[derive(Default)]
struct CountSyscalls;

#[reverie::tool]
impl Tool for CountSyscalls {
    type GlobalState = EventCounter;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        [Sysno::getrandom, Sysno::getpid, Sysno::mprotect]
            .into_iter()
            .collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        assert!(matches!(
            syscall.number(),
            Sysno::getrandom | Sysno::getpid | Sysno::mprotect
        ));
        if syscall.number() == Sysno::getpid {
            let regs = guest.regs().await;
            guest.send_rpc((1_u64 << 62) | regs.r12).await;
            guest.send_rpc((1_u64 << 63) | regs.rip).await;
        } else if syscall.number() == Sysno::mprotect {
            guest.send_rpc(1_u64 << 61).await;
        } else {
            guest.send_rpc(1).await;
        }
        Ok(guest.inject(syscall).await?)
    }
}

#[derive(Default)]
struct PassthroughGetpid;

#[reverie::tool]
impl Tool for PassthroughGetpid {
    type GlobalState = EventCounter;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        [Sysno::getpid].into_iter().collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        assert_eq!(syscall.number(), Sysno::getpid);
        guest.send_rpc(1).await;
        Ok(guest.inject(syscall).await?)
    }
}

#[derive(Default)]
struct PassthroughGetpidAndTaskCreation;

#[reverie::tool]
impl Tool for PassthroughGetpidAndTaskCreation {
    type GlobalState = EventCounter;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        [
            Sysno::getpid,
            Sysno::clone,
            Sysno::clone3,
            Sysno::fork,
            Sysno::vfork,
        ]
        .into_iter()
        .collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        if syscall.number() == Sysno::getpid {
            guest.send_rpc(1).await;
        } else {
            assert!(matches!(
                syscall.number(),
                Sysno::clone | Sysno::clone3 | Sysno::fork | Sysno::vfork
            ));
            guest.send_rpc(1_u64 << 57).await;
        }
        Ok(guest.inject(syscall).await?)
    }
}

#[derive(Default)]
struct ExitRecorder {
    path: PathBuf,
}

#[reverie::global_tool]
impl GlobalTool for ExitRecorder {
    type Request = i32;
    type Response = ();
    type Config = PathBuf;

    async fn init_global_state(path: &PathBuf) -> Self {
        Self { path: path.clone() }
    }

    async fn receive_rpc(&self, _from: Tid, pid: i32) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .unwrap();
        writeln!(file, "{pid}").unwrap();
    }
}

#[derive(Default)]
struct PassthroughTaskCreationAndRecordExits;

#[reverie::tool]
impl Tool for PassthroughTaskCreationAndRecordExits {
    type GlobalState = ExitRecorder;
    type ThreadState = ();

    fn subscriptions(_config: &PathBuf) -> Subscription {
        [
            Sysno::getpid,
            Sysno::clone,
            Sysno::clone3,
            Sysno::fork,
            Sysno::vfork,
        ]
        .into_iter()
        .collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        Ok(guest.inject(syscall).await?)
    }

    async fn on_exit_process<G: GlobalRPC<Self::GlobalState>>(
        self,
        pid: Pid,
        global_state: &G,
        _exit_status: ExitStatus,
    ) -> Result<(), Error> {
        global_state.send_rpc(pid.as_raw()).await;
        Ok(())
    }
}

#[derive(Default)]
struct PassthroughTaskCreationWithPendingProcessExit;

#[reverie::tool]
impl Tool for PassthroughTaskCreationWithPendingProcessExit {
    type GlobalState = EventCounter;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        [
            Sysno::getpid,
            Sysno::clone,
            Sysno::clone3,
            Sysno::fork,
            Sysno::vfork,
        ]
        .into_iter()
        .collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        Ok(guest.inject(syscall).await?)
    }

    async fn on_exit_process<G: GlobalRPC<Self::GlobalState>>(
        self,
        _pid: Pid,
        _global_state: &G,
        _exit_status: ExitStatus,
    ) -> Result<(), Error> {
        std::future::pending().await
    }
}

#[derive(Default)]
struct ObservePkey;

#[reverie::tool]
impl Tool for ObservePkey {
    type GlobalState = EventCounter;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        [Sysno::getpid, Sysno::pkey_mprotect].into_iter().collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        guest.send_rpc(1).await;
        Ok(guest.inject(syscall).await?)
    }
}

#[derive(Default)]
struct ReplaceGetpid;

#[reverie::tool]
impl Tool for ReplaceGetpid {
    type GlobalState = EventCounter;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        [Sysno::getpid].into_iter().collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        assert_eq!(syscall.number(), Sysno::getpid);
        guest.send_rpc(1).await;
        let replacement = Syscall::from_raw(Sysno::getppid, SyscallArgs::new(0, 0, 0, 0, 0, 0));
        Ok(guest.inject(replacement).await?)
    }
}

#[derive(Default)]
struct DoubleInjectGetpid;

#[reverie::tool]
impl Tool for DoubleInjectGetpid {
    type GlobalState = EventCounter;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        [Sysno::getpid].into_iter().collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        assert_eq!(syscall.number(), Sysno::getpid);
        guest.send_rpc(1).await;
        let _ = guest.inject(syscall).await?;
        Ok(guest.inject(syscall).await?)
    }
}

fn preload_path() -> PathBuf {
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_reverie-liteinst-strace"));
    let target = launcher.parent().unwrap();
    [
        target.join("libreverie_liteinst.so"),
        target.join("deps/libreverie_liteinst.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .expect("cargo did not build the LiteInst preload cdylib")
}

fn compile_fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let output = directory.path().join(name.trim_end_matches(".c"));
    let compiler = std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"));
    let result = ProcessCommand::new(compiler)
        .args(["-std=gnu11", "-O0", "-fno-pie", "-no-pie"])
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "failed to compile {}:\n{}",
        source.display(),
        String::from_utf8_lossy(&result.stderr)
    );
    (directory, output)
}

fn compile_static_fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let output = directory.path().join("li-static-exit");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"));
    let result = ProcessCommand::new(compiler)
        .args([
            "-std=gnu11",
            "-O0",
            "-nostdlib",
            "-static",
            "-fno-stack-protector",
            "-fno-pie",
            "-no-pie",
            "-Wl,--build-id=none",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "failed to compile {}:\n{}",
        source.display(),
        String::from_utf8_lossy(&result.stderr)
    );
    (directory, output)
}

fn symbol_address(binary: &std::path::Path, symbol: &str) -> u64 {
    let output = ProcessCommand::new("nm").arg(binary).output().unwrap();
    assert!(output.status.success(), "nm failed: {output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let address = fields.next()?;
            let _kind = fields.next()?;
            (fields.next()? == symbol).then(|| u64::from_str_radix(address, 16).unwrap())
        })
        .unwrap_or_else(|| panic!("missing symbol {symbol}"))
}

fn processes_named(name: &str) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap() {
        let Ok(entry) = entry else { continue };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if comm.trim() == name {
            found.push(pid);
        }
    }
    found
}

fn assert_processes_named_eventually_reaped(name: &str, context: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = processes_named(name);
        if remaining.is_empty() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let remaining_status = remaining
                .iter()
                .map(|pid| fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default())
                .collect::<Vec<_>>();
            panic!("{context}: {remaining:?} {remaining_status:?}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn assert_pid_reaped(pid: u32) {
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "failed LiteInst process {pid} remains stopped or unreaped"
    );
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
        -1,
        "failed LiteInst process {pid} still has a waitable state"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD),
        "failed LiteInst process {pid} was not fully reaped"
    );
}

struct UnrelatedStoppedProcess {
    child: Option<std::process::Child>,
}

impl UnrelatedStoppedProcess {
    fn spawn() -> Self {
        let child = ProcessCommand::new("/bin/sleep")
            .arg("300")
            .spawn()
            .expect("spawn unrelated process");
        let pid = child.id() as i32;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
            pid,
            "wait for unrelated process to stop"
        );
        assert!(
            libc::WIFSTOPPED(status),
            "unrelated process did not enter a stopped state: {status}"
        );
        Self { child: Some(child) }
    }

    fn assert_live_and_unreaped(&self) {
        let pid = self.child.as_ref().unwrap().id() as i32;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "cleanup signaled an unrelated process"
        );
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            0,
            "cleanup made an unrelated process waitable"
        );
    }

    fn kill_and_reap(mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().expect("kill unrelated process after bracket");
        child.wait().expect("reap unrelated process after bracket");
    }
}

impl Drop for UnrelatedStoppedProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn is_original_liteinst_session_refusal(text: &str, operation: &str) -> bool {
    text.contains("LiteInst session failed closed in a non-root task")
        && text.contains(operation)
        && !text.contains("LiteInst tracee cleanup failed")
        && !text.contains("notifier did not acknowledge terminal cleanup")
}

fn assert_original_liteinst_session_refusal(error: &Error, operation: &str) {
    let text = error.to_string();
    assert!(
        is_original_liteinst_session_refusal(&text, operation),
        "session failure did not retain the original refused {operation}, or cleanup replaced it: {text}"
    );
}

#[test]
fn session_refusal_assertion_rejects_terminal_cleanup_wrapper() {
    let original = "LiteInst session failed closed in a non-root task: refused vfork";
    let wrapped = format!(
        "LiteInst tracee cleanup failed after {original}: notifier did not acknowledge terminal cleanup"
    );
    assert!(is_original_liteinst_session_refusal(original, "vfork"));
    assert!(!is_original_liteinst_session_refusal(&wrapped, "vfork"));
}

async fn wait_for_pid_file(pid_file: &std::path::Path) -> u32 {
    loop {
        if let Ok(contents) = fs::read_to_string(pid_file)
            && let Ok(pid) = contents.trim().parse::<u32>()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn run_fail_closed_and_assert_reaped(command: Command, pid_file: &std::path::Path) -> Error {
    let mut run = Box::pin(LiteinstBackend::run_host_with_output_and_preload::<
        PassthroughGetpid,
    >(command, (), preload_path()));
    let mut early_result = None;
    let pid = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut run => {
                early_result = Some(result);
                wait_for_pid_file(pid_file).await
            }
            pid = wait_for_pid_file(pid_file) => pid,
        }
    })
    .await
    .expect("fail-closed fixture did not publish its pid");
    let result = if let Some(result) = early_result {
        result
    } else {
        match tokio::time::timeout(Duration::from_secs(3), &mut run).await {
            Ok(result) => result,
            Err(_) => {
                drop(run);
                assert_pid_reaped(pid);
                panic!("fail-closed fixture hung; cancellation cleanup reaped pid {pid}");
            }
        }
    };
    let error = match result {
        Ok(_) => panic!("required LiteInst runtime unexpectedly remained active"),
        Err(error) => error,
    };
    assert_pid_reaped(pid);
    error
}

#[tokio::test(flavor = "current_thread")]
async fn initial_dynamic_preload_handshake_activates_host_lifecycle() {
    let (_directory, guest) = compile_fixture("allocator_getrandom.c");
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<CountSyscalls>(
        Command::new(guest),
        (),
        preload_path(),
    )
    .await
    .unwrap();

    assert_eq!(
        global.delivered.load(Ordering::SeqCst),
        3,
        "host lifecycle missed allocator/pre-constructor entropy: {output:?}"
    );
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn initial_static_image_without_the_runtime_fails_closed() {
    let (_directory, guest) = compile_static_fixture("hybrid_static_exit.c");
    assert!(processes_named("li-static-exit").is_empty());
    let pid_directory = tempfile::tempdir().unwrap();
    let pid_file = pid_directory.path().join("guest.pid");
    let mut command = Command::new(guest);
    command.arg(&pid_file);

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
            command,
            (),
            preload_path(),
        ),
    )
    .await
    .expect("static image did not reach the entry guard");
    let error = result.expect_err("static image unexpectedly passed the entry guard");
    assert!(
        !pid_file.exists(),
        "static image executed its first side-effecting syscall before failing closed"
    );
    assert!(
        error
            .to_string()
            .contains("verify LiteInst runtime before executable entry failed")
            && error
                .to_string()
                .contains("before the required preload handshake completed"),
        "static image did not report the guarded entry boundary: {error}"
    );
    let pid = error
        .to_string()
        .split("tracee ")
        .nth(1)
        .and_then(|suffix| suffix.split(':').next())
        .and_then(|pid| pid.parse::<u32>().ok())
        .expect("entry-guard error did not identify the exact tracee");
    assert_pid_reaped(pid);
    assert!(
        processes_named("li-static-exit").is_empty(),
        "failed static image remains stopped or unreaped"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn valid_dynamic_run_observes_restored_executable_entry() {
    let (_directory, guest) = compile_fixture("hybrid_entry_guard.c");
    let (output, _global) = LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
        Command::new(guest),
        (),
        preload_path(),
    )
    .await
    .unwrap();

    assert_eq!(output.stdout, b"entry-int3=0\n", "{output:?}");
    assert!(output.status.success(), "{output:?}");
}

#[cfg(target_arch = "x86_64")]
#[tokio::test(flavor = "current_thread")]
async fn loader_cpu_events_are_determinized_before_ready_and_entry_is_restored() {
    let (_directory, guest) = compile_fixture("hybrid_pre_ready_cpu.c");
    let (output, global) =
        LiteinstBackend::run_host_with_output_and_preload::<ActivationCpuEvents>(
            Command::new(guest),
            (),
            preload_path(),
        )
        .await
        .unwrap();

    assert_eq!(output.stdout, b"entry-int3=0 probe=1\n", "{output:?}");
    assert!(output.status.success(), "{output:?}");
    if global.cpuid_interception.load(Ordering::SeqCst) == 1 {
        assert!(
            global.cpuid_events.load(Ordering::SeqCst) >= 1,
            "the pre-constructor IFUNC CPUID did not reach the Tool despite verified kernel interception"
        );
    } else {
        assert_eq!(
            global.cpuid_events.load(Ordering::SeqCst),
            0,
            "CPUID reached the Tool after the kernel reported interception unavailable"
        );
    }
    assert!(
        global.rdtsc_events.load(Ordering::SeqCst) >= 1,
        "the pre-constructor IFUNC RDTSC did not reach the Tool"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn post_start_exec_that_drops_the_preload_fails_closed() {
    let (_directory, guest) = compile_fixture("hybrid_exec_drop_preload.c");
    let pid_directory = tempfile::tempdir().unwrap();
    let pid_file = pid_directory.path().join("guest.pid");
    let mut command = Command::new(guest);
    command.arg(&pid_file);

    let error = run_fail_closed_and_assert_reaped(command, &pid_file).await;
    assert!(
        error
            .to_string()
            .contains("reject LiteInst post-start exec failed")
            && error
                .to_string()
                .contains("required preload runtime cannot be preserved across exec (phase Ready)"),
        "post-start exec did not report the lost runtime boundary: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn first_site_is_installed_once_and_hot_calls_use_liteinst() {
    let (_baseline_directory, baseline_guest) = compile_fixture("allocator_getrandom.c");
    let (baseline_output, baseline_global) = LiteinstBackend::run_host_with_output_and_preload::<
        CountSyscalls,
    >(Command::new(baseline_guest), (), preload_path())
    .await
    .unwrap();
    assert!(baseline_output.status.success(), "{baseline_output:?}");

    let (_directory, guest) = compile_fixture("hybrid_hot_site.c");
    let site = symbol_address(&guest, "reverie_liteinst_hybrid_getpid_site");
    let (output, global, stats) = LiteinstBackend::run_host_with_output_and_preload_and_stats::<
        CountSyscalls,
    >(Command::new(guest), (), preload_path())
    .await
    .unwrap();

    assert_eq!(
        output.stdout, b"calls=32 traps=1 hooks=31 ac=0 simd=1 spoofs=3\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 33, "{output:?}");
    assert_eq!(
        global.last_getpid_rip.load(Ordering::SeqCst),
        site + 2,
        "the host Tool must see the original logical post-syscall RIP"
    );
    assert_eq!(
        global.last_getpid_r12.load(Ordering::SeqCst),
        0x0012_3456_789a_bcde,
        "logical guest R12 must remain distinct from the controller HookContext base"
    );
    assert_eq!(
        global.helper_mprotect_callbacks.load(Ordering::SeqCst)
            - baseline_global
                .helper_mprotect_callbacks
                .load(Ordering::SeqCst),
        0,
        "the patch helper must add zero mprotect Tool callbacks above the loader baseline"
    );
    let decisions = stats.decision_counts();
    assert_eq!(
        decisions.into_iter().sum::<usize>(),
        stats.patch_candidates()
    );
    assert_eq!(stats.distinct_rips(), decisions[0] + decisions[1]);
    assert!(
        decisions[1] >= 1,
        "the known hot site must use a relocated patch: {stats}"
    );
    assert!(
        stats.instruction_length_counts()[3] >= 1,
        "the known hot site begins with a two-byte syscall: {stats}"
    );
    assert_eq!(
        stats.instruction_length_counts().into_iter().sum::<usize>(),
        stats.classified_candidates()
    );
    assert_eq!(
        stats.non_straddling() + stats.cacheline_straddlers(),
        stats.classified_candidates()
    );
    assert_eq!(
        stats.straddle_prefix_counts().into_iter().sum::<usize>(),
        stats.cacheline_straddlers()
    );
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn cacheline_straddler_uses_quiescent_patch_and_is_counted() {
    let (_directory, guest) = compile_fixture("hybrid_straddler_site.c");
    let site = symbol_address(&guest, "reverie_liteinst_straddler_site");
    assert_eq!(site % 64, 63, "fixture syscall must straddle a cache line");

    let (output, global, stats) = LiteinstBackend::run_host_with_output_and_preload_and_stats::<
        PassthroughGetpid,
    >(Command::new(guest), (), preload_path())
    .await
    .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"straddler-quiescent-patch-ok\n");
    assert_eq!(global.delivered.load(Ordering::SeqCst), 8);
    assert_eq!(stats.distinct_rips(), 1);
    assert_eq!(stats.patch_candidates(), 1);
    assert_eq!(stats.decision_counts(), [0, 1, 0, 0]);
    assert_eq!(stats.classified_candidates(), 1);
    assert_eq!(stats.cacheline_straddlers(), 1);
    assert_eq!(stats.non_straddling(), 0);
    assert_eq!(stats.instruction_length_counts(), [0, 0, 0, 1, 0]);
    assert_eq!(stats.straddle_prefix_counts(), [1, 0, 0, 0]);
}

#[tokio::test(flavor = "current_thread")]
async fn quiescent_helper_patches_every_cache_line_split_without_calibration() {
    let (_directory, guest) = compile_fixture("hybrid_straddler_sites.c");
    let mut command = Command::new(guest);
    command.env_remove(STRADDLER_STALENESS_TICKS_ENV);
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
        command,
        (),
        preload_path(),
    )
    .await
    .unwrap();

    assert_eq!(
        output.stdout, b"offsets=57..63 calls=14 traps=7 hooks=7\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 14, "{output:?}");
    assert!(output.status.success(), "{output:?}");
}

async fn run_cpuid_policy_mode(
    mode: Option<&str>,
) -> Option<(reverie::process::Output, EventCounter)> {
    let (_directory, guest) = compile_fixture("hybrid_cpuid_policy.c");
    let mut command = Command::new(guest);
    if let Some(mode) = mode {
        command.arg(mode);
    }
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
        command,
        (),
        preload_path(),
    )
    .await
    .unwrap();
    if output.status.code() == Some(77) {
        eprintln!("skipping: this host does not support ARCH_GET_CPUID");
        return None;
    }
    Some((output, global))
}

#[tokio::test(flavor = "current_thread")]
async fn patch_helper_restores_disabled_cpuid_after_installing_a_site() {
    let Some((output, global)) = run_cpuid_policy_mode(None).await else {
        return;
    };
    assert_eq!(
        output.stdout, b"mode=active calls=32 traps=1 hooks=31 cpuid=0\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 32, "{output:?}");
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn patch_helper_restores_disabled_cpuid_after_fallback() {
    let Some((output, global)) = run_cpuid_policy_mode(Some("fallback")).await else {
        return;
    };
    assert_eq!(
        output.stdout, b"mode=fallback calls=2 traps=1 hooks=0 cpuid=0\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 2, "{output:?}");
    assert!(output.status.success(), "{output:?}");
}

async fn run_tsc_policy_mode(
    mode: Option<&str>,
) -> Option<(reverie::process::Output, EventCounter)> {
    let (_directory, guest) = compile_fixture("hybrid_tsc_policy.c");
    let mut command = Command::new(guest);
    if let Some(mode) = mode {
        command.arg(mode);
    }
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
        command,
        (),
        preload_path(),
    )
    .await
    .unwrap();
    if output.status.code() == Some(77) {
        eprintln!("skipping: this host does not support PR_GET_TSC/PR_SET_TSC");
        return None;
    }
    Some((output, global))
}

#[tokio::test(flavor = "current_thread")]
async fn patch_helper_restores_faulting_tsc_after_installing_a_site() {
    let Some((output, global)) = run_tsc_policy_mode(None).await else {
        return;
    };
    assert_eq!(
        output.stdout, b"mode=active calls=32 traps=1 hooks=31 tsc=2\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 32, "{output:?}");
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn patch_helper_restores_faulting_tsc_after_fallback() {
    let Some((output, global)) = run_tsc_policy_mode(Some("fallback")).await else {
        return;
    };
    assert_eq!(
        output.stdout, b"mode=fallback calls=2 traps=1 hooks=0 tsc=2\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 2, "{output:?}");
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn first_discovery_event_can_replace_the_syscall() {
    let (_directory, guest) = compile_fixture("hybrid_hot_site.c");
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<ReplaceGetpid>(
        Command::new(guest),
        (),
        preload_path(),
    )
    .await
    .unwrap();

    assert_eq!(
        output.stdout, b"calls=32 traps=1 hooks=31 ac=0 simd=1 spoofs=3\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 32, "{output:?}");
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn first_discovery_event_can_inject_more_than_once() {
    let (_directory, guest) = compile_fixture("hybrid_hot_site.c");
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<DoubleInjectGetpid>(
        Command::new(guest),
        (),
        preload_path(),
    )
    .await
    .unwrap();

    assert_eq!(
        output.stdout, b"calls=32 traps=1 hooks=31 ac=0 simd=1 spoofs=3\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 32, "{output:?}");
    assert!(output.status.success(), "{output:?}");
}

/// Runs a multi-task fixture that is expected to complete, and requires the
/// guest's own end-of-run marker.
///
/// Exit status alone is not enough: every fixture here prints its marker only
/// after the task it creates has been created, run and reaped, so a regression
/// that skips the task cannot satisfy the assertion by exiting zero.
async fn run_multi_task_fixture<T>(fixture: &str, marker_line: &str) -> EventCounter
where
    T: Tool<GlobalState = EventCounter, ThreadState = ()> + 'static,
{
    let (_directory, guest) = compile_fixture(fixture);
    let name = unique_process_name();
    let pid_directory = tempfile::tempdir().unwrap();
    let pid_file = pid_directory.path().join("root.pid");
    let mut command = Command::new(guest);
    command.arg(&name).arg(&pid_file);
    let (output, global) =
        LiteinstBackend::run_host_with_output_and_preload::<T>(command, (), preload_path())
            .await
            .unwrap_or_else(|error| panic!("hybrid refused to follow {fixture}: {error}"));

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        marker_line.as_bytes(),
        "guest did not reach the end of {fixture}: {output:?}"
    );
    let root_pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_pid_reaped(root_pid);
    assert!(
        processes_named(&name).is_empty(),
        "LiteInst root/child remains stopped or as a zombie"
    );
    global
}

/// The hybrid follows a forked child instead of refusing at the clone boundary.
///
/// This exercises the supported fork path end-to-end. It does not independently
/// isolate the root-TID identity and root-stop lease re-arm mechanisms.
#[tokio::test(flavor = "current_thread")]
async fn hybrid_follows_a_forked_child() {
    let global = run_multi_task_fixture::<PassthroughGetpidAndTaskCreation>(
        "hybrid_fork.c",
        "fork-followed\n",
    )
    .await;
    assert!(
        global.task_creation_events.load(Ordering::SeqCst) > 0,
        "the task-subscribing tool did not observe the fork lifecycle"
    );
}

/// The hybrid follows a second thread created with `clone3(CLONE_THREAD)`.
///
/// This exercises the supported thread-creation path end-to-end. It does not
/// independently isolate the task-creating-site patch guard.
#[tokio::test(flavor = "current_thread")]
async fn hybrid_follows_a_created_thread() {
    let global = run_multi_task_fixture::<PassthroughGetpidAndTaskCreation>(
        "hybrid_thread.c",
        "thread-followed\n",
    )
    .await;
    assert!(
        global.task_creation_events.load(Ordering::SeqCst) > 0,
        "the task-subscribing tool did not observe the clone lifecycle"
    );
}

/// The task-subscribing tool remains active without manufacturing a task event
/// when the guest makes subscribed `getpid` calls but creates no task.
#[tokio::test(flavor = "current_thread")]
async fn task_subscriber_does_not_report_task_creation_without_one() {
    let (_directory, guest) = compile_fixture("hybrid_hot_site.c");
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<
        PassthroughGetpidAndTaskCreation,
    >(Command::new(guest), (), preload_path())
    .await
    .unwrap();

    assert_eq!(
        output.stdout, b"calls=32 traps=1 hooks=31 ac=0 simd=1 spoofs=3\n",
        "{output:?}"
    );
    assert_eq!(global.delivered.load(Ordering::SeqCst), 32, "{output:?}");
    assert_eq!(
        global.task_creation_events.load(Ordering::SeqCst),
        0,
        "the tool reported task creation for a single-task fixture: {output:?}"
    );
    assert!(output.status.success(), "{output:?}");
}

/// KNOWN GAP, committed as a reproducer rather than described: two generations
/// of children do not reliably complete under this harness.
///
/// The grandchild's new-task event belongs to a NON-root parent, which is the
/// case the cleanup guard's newborn registration has to cover -- scoping that
/// registration to the root leaves the grandchild unregistered and
/// `handle_new_task` aborts on `stored child event ownership must remain
/// registered`. That much is fixed and this fixture does reach
/// `fork-tree-followed`: it passed once here, and Hermit's
/// `determinism-stress-c/fork-tree` reaches canonical L2 under the real Detcore
/// tool, which sequentializes the guest.
///
/// It is `ignore`d because it is NOT reliable here: after that single pass it
/// wedged with no forward progress on three consecutive runs, under both this
/// tool and a variant that also subscribes to the task-creating syscalls. A
/// flaky hang is worse than no test, so it does not run by default. Do not
/// treat the fix it covers as verified until this is diagnosed and the `ignore`
/// removed.
#[tokio::test(flavor = "current_thread")]
#[ignore = "known gap: second-generation fork does not reliably complete in this harness"]
async fn hybrid_follows_a_grandchild() {
    run_multi_task_fixture::<PassthroughGetpid>("hybrid_fork_tree.c", "fork-tree-followed\n").await;
}

/// A child that execs fails the whole session; the root must not report the
/// success it would otherwise reach.
///
/// Exec after start cannot preserve the preload runtime, and that refusal now
/// happens in a task with no outer cleanup guard of its own. Two distinct
/// regressions are covered, and they fail in different ways:
///
/// * without the session-wide failure record the root reaches its own clean
///   exit and this run returns `Ok` -- a SILENT GREEN over a child that was
///   released untraced, which is strictly worse than the refusal it replaced;
/// * without releasing the failed non-root task from the tool, the
///   deterministic scheduler waits forever for a thread whose tracee the error
///   path already detached, and this test hangs rather than fails.
#[tokio::test(flavor = "current_thread")]
async fn a_child_that_execs_fails_the_session_instead_of_reporting_success() {
    let (_directory, guest) = compile_fixture("hybrid_fork_exec.c");
    let name = unique_process_name();
    let pid_directory = tempfile::tempdir().unwrap();
    let pid_file = pid_directory.path().join("root.pid");
    let exit_file = pid_directory.path().join("process-exits");
    let mut command = Command::new(guest);
    command.arg(&name).arg(&pid_file);
    let result =
        tokio::time::timeout(
            Duration::from_secs(60),
            LiteinstBackend::run_host_with_output_and_preload::<
                PassthroughTaskCreationAndRecordExits,
            >(command, exit_file.clone(), preload_path()),
        )
        .await
        .expect("a failed non-root task was never released from the tool: the session hung");

    let error = match result {
        Ok((output, _global)) => panic!(
            "SILENT GREEN: the session reported success over a child that could not be \
             followed through exec: {output:?}"
        ),
        Err(error) => error,
    };
    assert_original_liteinst_session_refusal(&error, "exec");
    let root_pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let exits = fs::read_to_string(&exit_file).unwrap_or_default();
    let non_root_exits = exits
        .lines()
        .map(|pid| pid.parse::<i32>().unwrap())
        .filter(|pid| *pid != root_pid as i32)
        .count();
    assert_eq!(
        non_root_exits, 1,
        "the failed non-root process did not run its Tool exit callback: root={root_pid} exits={exits:?}"
    );
    assert_pid_reaped(root_pid);
    assert_processes_named_eventually_reaped(
        &name,
        "failed LiteInst root/child remains stopped or as a zombie",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn post_start_exec_refusal_reaches_cleanup_while_process_exit_is_pending() {
    let (_directory, guest) = compile_fixture("hybrid_fork_exec.c");
    let name = unique_process_name();
    let pid_directory = tempfile::tempdir().unwrap();
    let pid_file = pid_directory.path().join("root.pid");
    let mut command = Command::new(guest);
    command.arg(&name).arg(&pid_file);

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        LiteinstBackend::run_host_with_output_and_preload::<
            PassthroughTaskCreationWithPendingProcessExit,
        >(command, (), preload_path()),
    )
    .await
    .expect("post-start exec refusal never reached session cleanup");
    let error = result.expect_err("post-start exec refusal reported success");
    assert_original_liteinst_session_refusal(&error, "exec");

    let root_pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_pid_reaped(root_pid);
    assert_processes_named_eventually_reaped(&name, "refused exec left a LiteInst process behind");
}

/// A root that has already exited must stop joining its child when that child
/// refuses post-start exec, even if the child's Tool exit callback is pending.
#[tokio::test(flavor = "current_thread")]
async fn post_start_exec_refusal_cancels_root_join_while_process_exit_is_pending() {
    let (_directory, guest) = compile_fixture("hybrid_fork_exec_after_root_exit.c");
    let name = unique_process_name();
    let pid_directory = tempfile::tempdir().unwrap();
    let pid_file = pid_directory.path().join("root.pid");
    let mut command = Command::new(guest);
    command.arg(&name).arg(&pid_file);
    let unrelated = UnrelatedStoppedProcess::spawn();

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        LiteinstBackend::run_host_with_output_and_preload::<
            PassthroughTaskCreationWithPendingProcessExit,
        >(command, (), preload_path()),
    )
    .await
    .expect("post-start exec refusal did not cancel the root's child join");
    let error = result.expect_err("post-start exec refusal reported success");
    assert_original_liteinst_session_refusal(&error, "exec");

    let root_pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_pid_reaped(root_pid);
    assert_processes_named_eventually_reaped(&name, "refused exec left a LiteInst process behind");
    unrelated.assert_live_and_unreaped();
    unrelated.kill_and_reap();
}

/// A vfork refusal in a non-root task fails the whole session even when it is
/// recorded only after the root has reached its own clean exit.
#[tokio::test(flavor = "current_thread")]
async fn vfork_in_a_forked_child_fails_the_session_after_root_exit() {
    let (_directory, guest) = compile_fixture("hybrid_fork_vfork.c");
    let name = unique_process_name();
    let pid_directory = tempfile::tempdir().unwrap();
    let pid_file = pid_directory.path().join("root.pid");
    let mut command = Command::new(guest);
    command.arg(&name).arg(&pid_file);
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
            command,
            (),
            preload_path(),
        ),
    )
    .await
    .expect("a refused non-root vfork was never released from the tool: the session hung");

    let error = match result {
        Ok((output, _global)) => panic!(
            "SILENT GREEN: the session reported success before a non-root vfork refusal: {output:?}"
        ),
        Err(error) => error,
    };
    assert_original_liteinst_session_refusal(&error, "vfork");
    let root_pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_pid_reaped(root_pid);
    assert_processes_named_eventually_reaped(
        &name,
        "failed LiteInst root/child remains stopped or as a zombie",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reused_mapping_invalidates_and_rediscovers_the_syscall_site() {
    let (_directory, guest) = compile_fixture("hybrid_mapping_churn.c");
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
        Command::new(guest),
        (),
        preload_path(),
    )
    .await
    .unwrap();

    assert_eq!(output.stdout, b"reuse traps=2 hooks=0\n", "{output:?}");
    assert_eq!(
        global.delivered.load(Ordering::SeqCst),
        4,
        "both generations must use the correct ptrace fallback: {output:?}"
    );
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn moving_a_patched_mapping_rejects_stale_hot_provenance() {
    let (_directory, guest) = compile_fixture("hybrid_mremap_patched.c");
    let command = Command::new(guest);
    let error = LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
        command,
        (),
        preload_path(),
    )
    .await
    .expect_err("mremap unexpectedly moved a live patched site");

    assert!(
        error
            .to_string()
            .contains("mremap overlaps an active LiteInst hook footprint"),
        "mapping was not rejected by the pre-mutation provenance check: {error}"
    );
}

async fn run_active_footprint_mode(
    mode: &str,
) -> Result<(reverie::process::Output, EventCounter), Error> {
    let (_directory, guest) = compile_fixture("hybrid_active_footprint.c");
    let mut command = Command::new(guest);
    command.arg(mode);
    LiteinstBackend::run_host_with_output_and_preload::<PassthroughGetpid>(
        command,
        (),
        preload_path(),
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn active_hook_noop_mprotect_preserves_the_hook() {
    for (mode, stdout) in [
        ("noop", "active no-op protection preserved\n"),
        ("short-noop", "active short no-op protection preserved\n"),
    ] {
        let (output, global) = run_active_footprint_mode(mode).await.unwrap();
        assert_eq!(output.stdout, stdout.as_bytes());
        assert_eq!(global.delivered.load(Ordering::SeqCst), 3);
        assert!(output.status.success(), "{output:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn active_hook_mapping_footprints_reject_mprotect_before_mutation() {
    for (mode, syscall) in [
        ("site", "mprotect"),
        ("trampoline", "mprotect"),
        ("arena-rw", "mprotect"),
        ("short-site", "mprotect"),
        ("short-trampoline", "mprotect"),
        ("short-arena-rw", "mprotect"),
        ("short-munmap", "munmap"),
        ("short-map-fixed", "mmap"),
        ("short-mremap", "mremap"),
        ("short-mremap-fixed", "mremap"),
        ("zero-old-mremap-fixed", "mremap"),
    ] {
        let error = run_active_footprint_mode(mode)
            .await
            .expect_err("active footprint mutation unexpectedly completed");
        assert!(
            error.to_string().contains(&format!(
                "{syscall} overlaps an active LiteInst hook footprint"
            )),
            "{mode} footprint was not rejected before mutation: {error}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pkey_mprotect_is_controller_owned_unless_subscribed() {
    let (_directory, guest) = compile_fixture("hybrid_active_footprint.c");
    let mut command = Command::new(&guest);
    command.arg("pkey-noop");
    let (unsubscribed_output, unsubscribed) = LiteinstBackend::run_host_with_output_and_preload::<
        PassthroughGetpid,
    >(command, (), preload_path())
    .await
    .unwrap();

    let mut command = Command::new(guest);
    command.arg("pkey-noop");
    let (subscribed_output, subscribed) = LiteinstBackend::run_host_with_output_and_preload::<
        ObservePkey,
    >(command, (), preload_path())
    .await
    .unwrap();
    assert_eq!(unsubscribed_output.stdout, subscribed_output.stdout);
    assert_eq!(
        subscribed.delivered.load(Ordering::SeqCst),
        unsubscribed.delivered.load(Ordering::SeqCst) + 1,
        "pkey_mprotect must reach the Tool exactly when subscribed"
    );

    let error = run_active_footprint_mode("pkey-site")
        .await
        .expect_err("destructive pkey_mprotect unexpectedly completed");
    assert!(
        error
            .to_string()
            .contains("pkey_mprotect overlaps an active LiteInst hook footprint"),
        "pkey_mprotect was not rejected before mutation: {error}"
    );
}

async fn cancel_host_wait(capture_output: bool) {
    let (_directory, guest) = compile_fixture("hybrid_wait_cancel.c");
    let pid_directory = tempfile::tempdir().unwrap();
    let pid_file = pid_directory.path().join("guest.pid");
    let mut command = Command::new(guest);
    command.arg(&pid_file);

    let mut run = if capture_output {
        Box::pin(LiteinstBackend::run_host_with_output_and_preload::<
            PassthroughGetpid,
        >(command, (), preload_path()))
            as std::pin::Pin<Box<dyn std::future::Future<Output = _>>>
    } else {
        Box::pin(async move {
            LiteinstBackend::run_host_with_preload::<PassthroughGetpid>(command, (), preload_path())
                .await
                .map(|(status, global)| {
                    (
                        reverie::process::Output {
                            status,
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        },
                        global,
                    )
                })
        })
    };
    let wait_for_pid = async {
        loop {
            if let Ok(contents) = fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let pid = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut run => panic!("wait completed before cancellation: {result:?}"),
            pid = wait_for_pid => pid,
        }
    })
    .await
    .expect("guest did not enter the wait phase");
    drop(run);
    assert_pid_reaped(pid);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_wait_reaps_and_unregisters_liteinst_root() {
    cancel_host_wait(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_wait_with_output_reaps_and_unregisters_liteinst_root() {
    cancel_host_wait(true).await;
}
