/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#592)

//! DynamoRIO callback runtime that executes the real Detcore [`Tool`] over
//! [`reverie_dbi::DbiGuest`].

#![deny(missing_docs)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::c_void;
use std::fs;
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::pin::pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use detcore::Config;
use detcore::Detcore;
use detcore::GlobalState;
use reverie::Error;
use reverie::ExitStatus;
use reverie::Pid;
use reverie::Signal;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::CloneArgs;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Errno;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;
use reverie_dbi::DbiSyscallOutcome;
use reverie_dbi::MemoryReader;
use reverie_dbi::RegisterReader;
use reverie_dbi::SyscallInvoker;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const MAX_OBSERVED_BUFFER: usize = 1024 * 1024;

type DetcoreThreadState = <Detcore as Tool>::ThreadState;
type Emitter = reverie_dbi::RuntimeEmitter;
type Idler = reverie_dbi::RuntimeIdler;

fn emit_marker(emit: Emitter, message: &'static [u8]) {
    unsafe { emit(message.as_ptr(), message.len()) };
}

fn info_logging_enabled() -> bool {
    matches!(
        std::env::var("HERMIT_LOG")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "info" | "debug" | "trace"
    )
}

fn requires_native_process_lifecycle(sysnum: i64, args: &[u64], clone3_flags: Option<u64>) -> bool {
    match sysnum {
        libc::SYS_fork
        | libc::SYS_vfork
        | libc::SYS_wait4
        | libc::SYS_waitid
        | libc::SYS_rt_sigreturn
        | libc::SYS_execve
        | libc::SYS_execveat => true,
        libc::SYS_clone => {
            args[0] & libc::CLONE_THREAD as u64 == 0 || args[0] & libc::CLONE_VFORK as u64 != 0
        }
        libc::SYS_clone3 => clone3_flags.is_none_or(|flags| {
            flags & libc::CLONE_THREAD as u64 == 0 || flags & libc::CLONE_VFORK as u64 != 0
        }),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingThreadClone {
    sysno: Sysno,
    flags: CloneFlags,
    child_tid_address: usize,
}

fn thread_clone_metadata(
    sysnum: i64,
    args: &[u64],
    read_memory: MemoryReader,
) -> Option<PendingThreadClone> {
    let (sysno, flags, child_tid_address) = match sysnum {
        libc::SYS_clone => (
            Sysno::clone,
            CloneFlags::from_bits_retain(args[0]),
            args[3] as usize,
        ),
        libc::SYS_clone3 if args[0] != 0 && args[1] >= std::mem::size_of::<u64>() as u64 => {
            let mut clone_args: CloneArgs = unsafe { std::mem::zeroed() };
            let read_length = usize::min(args[1] as usize, std::mem::size_of::<CloneArgs>());
            let read = unsafe {
                read_memory(
                    args[0] as usize,
                    (&mut clone_args as *mut CloneArgs).cast(),
                    read_length,
                )
            };
            if read == 0 {
                return None;
            }
            (
                Sysno::clone3,
                clone_args.flags,
                clone_args.child_tid as usize,
            )
        }
        _ => return None,
    };
    (flags.contains(CloneFlags::CLONE_THREAD) && !flags.contains(CloneFlags::CLONE_VFORK))
        .then_some(PendingThreadClone {
            sysno,
            flags,
            child_tid_address,
        })
}

fn run_cooperative<F: Future<Output = ()>>(future: F, idle: Idler) {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        if RUNTIME_SHUTDOWN.load(Ordering::Acquire) {
            return;
        }
        match future.as_mut().poll(&mut context) {
            Poll::Ready(()) => return,
            Poll::Pending => unsafe { idle() },
        }
    }
}

struct Runtime {
    config: Config,
    global: GlobalState,
    tool: OnceLock<Detcore>,
}

struct ThreadRuntime {
    tid: Pid,
    state: DetcoreThreadState,
    initialized: bool,
    pending_clone: Option<PendingThreadClone>,
}

#[repr(C)]
struct NativeThreadScratch {
    branches: u64,
    observed_syscalls: u64,
    rewritten_syscalls: u64,
    runtime_state: *mut ThreadRuntime,
    runtime_started: u64,
    runtime_start_pc: usize,
}

#[derive(Default)]
struct ThreadHandoffs {
    inherited: HashMap<i32, Box<ThreadRuntime>>,
    exited: HashSet<i32>,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static THREAD_HANDOFFS: LazyLock<Mutex<ThreadHandoffs>> =
    LazyLock::new(|| Mutex::new(ThreadHandoffs::default()));
static RUNTIME_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RUNTIME_STOPPED: AtomicBool = AtomicBool::new(false);
static TOTAL_BRANCHES: AtomicU64 = AtomicU64::new(0);
static TOTAL_SYSCALLS: AtomicU64 = AtomicU64::new(0);
static TOTAL_REWRITTEN: AtomicU64 = AtomicU64::new(0);
static TOTAL_SIGNALS: AtomicU64 = AtomicU64::new(0);
static MEMORY_HASH: AtomicU64 = AtomicU64::new(FNV_OFFSET);

fn run_guest_future<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::hint::spin_loop(),
        }
    }
}

fn update_memory_hash(sysnum: i64, args: &[u64], read_memory: MemoryReader) {
    if sysnum != libc::SYS_write {
        return;
    }
    let address = args[1] as usize;
    let length = args[2] as usize;
    if address == 0 || length > MAX_OBSERVED_BUFFER {
        return;
    }

    let mut bytes = vec![0; length];
    if unsafe { read_memory(address, bytes.as_mut_ptr(), length) } == 0 {
        return;
    }

    let mut hash = FNV_OFFSET;
    for byte in sysnum
        .to_le_bytes()
        .into_iter()
        .chain(args[0].to_le_bytes())
        .chain((length as u64).to_le_bytes())
        .chain(bytes)
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    MEMORY_HASH.fetch_add(hash, Ordering::SeqCst);
}

fn error_result(error: Error) -> i64 {
    match error {
        Error::Errno(errno) => -(errno.into_raw() as i64),
        _ => -(Errno::EIO.into_raw() as i64),
    }
}

/// Returns the cdylib built beside the running Hermit binary or in Cargo's deps directory.
pub fn runtime_library_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    let direct = directory.join("libhermit.so");
    let deps = directory.join("deps/libhermit.so");
    [direct, deps]
        .into_iter()
        .find(|runtime| runtime.is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Hermit DBI runtime was not built beside {} or in its deps directory",
                    executable.display()
                ),
            )
        })
}
fn lock_native_client_build(directory: &std::path::Path) -> io::Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("build.lock"))?;
    loop {
        // SAFETY: lock owns this valid file descriptor for the lifetime of the lock.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(lock);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Builds the DynamoRIO native client against the Detcore runtime if needed.
pub fn prepare_native_client() -> io::Result<(PathBuf, PathBuf)> {
    let runtime = runtime_library_path()?;
    let directory = runtime
        .parent()
        .expect("runtime library path must have a parent")
        .join("detcore-dbi-native");
    fs::create_dir_all(&directory)?;
    let _build_lock = lock_native_client_build(&directory)?;

    let configure = Command::new("cmake")
        .arg("-S")
        .arg(reverie_dbi::native_client_source_dir())
        .arg("-B")
        .arg(&directory)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg(format!(
            "-DDynamoRIO_DIR={}",
            reverie_dbi::bundled_dynamorio_cmake_dir().display()
        ))
        .arg(format!("-DREVERIE_DBI_RUNTIME={}", runtime.display()))
        .output()?;
    if !configure.status.success() {
        return Err(io::Error::other(format!(
            "failed to configure Detcore DBI client: {}",
            String::from_utf8_lossy(&configure.stderr)
        )));
    }

    let build = Command::new("cmake")
        .arg("--build")
        .arg(&directory)
        .arg("--parallel")
        .output()?;
    if !build.status.success() {
        return Err(io::Error::other(format!(
            "failed to build Detcore DBI client: {}",
            String::from_utf8_lossy(&build.stderr)
        )));
    }

    let client = directory.join("libreverie_dbi_client.so");
    if !client.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Detcore DBI client was not built at {}", client.display()),
        ));
    }
    Ok((reverie_dbi::bundled_drrun_path().to_path_buf(), client))
}

/// Runs Detcore's async global scheduler on a DynamoRIO-managed client thread.
///
/// The native client starts this entry point before registering guest events
/// and waits for [`reverie_dbi_runtime_ready`] before allowing callbacks.
///
/// # Safety
///
/// `argument` must point to a valid [`reverie_dbi::DbiRuntimeCallbacks`] value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_background_init(argument: *mut c_void) {
    let callbacks = unsafe { &*argument.cast::<reverie_dbi::DbiRuntimeCallbacks>() };
    let emit = callbacks.emit;
    emit_marker(emit, b"detcore-dbi: background client thread entered\n");
    emit_marker(emit, b"detcore-dbi: constructing Detcore Config\n");
    let mut config = Config {
        sequentialize_threads: true,
        deterministic_io: true,
        preemption_timeout: None,
        ..Config::default()
    };
    config.validate();

    emit_marker(emit, b"detcore-dbi: initializing Detcore GlobalState\n");
    let global = GlobalState::init_for_external_scheduler(&config);
    emit_marker(emit, b"detcore-dbi: GlobalState initialized\n");
    RUNTIME
        .set(Runtime {
            config,
            global,
            tool: OnceLock::new(),
        })
        .unwrap_or_else(|_| panic!("Detcore DBI runtime initialized twice"));
    emit_marker(emit, b"detcore-dbi: background scheduler ready\n");
    let runtime = RUNTIME.get().expect("Detcore DBI runtime was initialized");
    let log_scheduler = info_logging_enabled();
    let observer = Arc::new(move |event: &'static str| {
        if log_scheduler {
            let line = format!("INFO detcore::scheduler: {event}\n");
            unsafe { emit(line.as_ptr(), line.len()) };
        }
    });
    run_cooperative(
        runtime.global.run_external_scheduler(observer),
        callbacks.idle,
    );
    RUNTIME_STOPPED.store(true, Ordering::Release);
    emit_marker(emit, b"detcore-dbi: background scheduler completed\n");
}

/// Requests shutdown of the backend-owned scheduler at process exit.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbi_runtime_process_exit() {
    RUNTIME_SHUTDOWN.store(true, Ordering::Release);
}

/// Reports whether the Detcore global scheduler is ready for guest callbacks.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbi_runtime_ready() -> i32 {
    i32::from(RUNTIME.get().is_some())
}

/// Initializes native per-thread scratch state. Detcore state is initialized
/// lazily when the callback provides the actual guest tid and pid.
///
/// # Safety
///
/// The native client must pass a valid writable scratch pointer or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_thread_init(scratch: *mut c_void) {
    unsafe {
        scratch
            .cast::<NativeThreadScratch>()
            .write(NativeThreadScratch {
                branches: 0,
                observed_syscalls: 0,
                rewritten_syscalls: 0,
                runtime_state: std::ptr::null_mut(),
                runtime_started: 0,
                runtime_start_pc: 0,
            });
    }
}

/// Releases Detcore state owned by a DynamoRIO application thread.
///
/// # Safety
///
/// `scratch` must be the pointer initialized by
/// [`reverie_dbi_runtime_thread_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_thread_exit(scratch: *mut c_void, tid: i32) {
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    let runtime_state = if scratch.runtime_state.is_null() {
        let mut handoffs = THREAD_HANDOFFS.lock().unwrap();
        match handoffs.inherited.remove(&tid) {
            Some(state) => Some(state),
            None => {
                handoffs.exited.insert(tid);
                None
            }
        }
    } else {
        let state = unsafe { Box::from_raw(scratch.runtime_state) };
        scratch.runtime_state = std::ptr::null_mut();
        Some(state)
    };
    let Some(thread) = runtime_state else {
        return;
    };
    release_thread_runtime(*thread);
}

fn release_thread_runtime(thread: ThreadRuntime) {
    let ThreadRuntime {
        tid,
        state,
        initialized,
        pending_clone: _,
    } = thread;
    if initialized || state.detpid.is_some() {
        let runtime = RUNTIME.get().expect("Detcore DBI runtime was initialized");
        let tool = runtime
            .tool
            .get()
            .expect("Detcore DBI tool was initialized");
        let _ = reverie_dbi::run_tool_thread_exit(
            tool,
            tid,
            state,
            &runtime.global,
            &runtime.config,
            ExitStatus::SUCCESS,
        );
    }
}

/// Adopts and scheduler-gates a newly created DynamoRIO application thread.
///
/// # Safety
///
/// All pointers and callbacks must remain valid for this callback.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_thread_start(
    context: *mut c_void,
    scratch: *mut c_void,
    tid: i32,
    pid: i32,
    branches: u64,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    emit: Emitter,
) -> i32 {
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    if !scratch.runtime_state.is_null() {
        return 0;
    }
    let runtime = RUNTIME.get().expect("Detcore DBI runtime was initialized");
    let tid = Pid::from_raw(tid);
    let pid = Pid::from_raw(pid);
    let tool = runtime
        .tool
        .get_or_init(|| Detcore::new(pid, &runtime.config));
    let thread = if tid == pid {
        Box::new(ThreadRuntime {
            tid,
            state: tool.init_thread_state(Tid::from_raw(tid.into()), None),
            initialized: false,
            pending_clone: None,
        })
    } else {
        if info_logging_enabled() {
            emit_marker(emit, b"detcore-dbi: child first-block gate entered\n");
        }
        loop {
            if let Some(thread) = THREAD_HANDOFFS
                .lock()
                .unwrap()
                .inherited
                .remove(&tid.as_raw())
            {
                break thread;
            }
            std::thread::yield_now();
        }
    };
    scratch.runtime_state = Box::into_raw(thread);
    let thread = unsafe { &mut *scratch.runtime_state };
    if reverie_dbi::run_tool_thread_start(
        tool,
        context as usize,
        tid,
        pid,
        branches,
        &mut thread.state,
        &runtime.global,
        &runtime.config,
        invoke_syscall,
        read_registers,
    )
    .is_err()
    {
        emit_marker(emit, b"detcore-dbi: thread-start hook failed\n");
        return 1;
    }
    if tid == pid
        && reverie_dbi::run_tool_post_exec(
            tool,
            context as usize,
            tid,
            pid,
            branches,
            &mut thread.state,
            &runtime.global,
            &runtime.config,
            invoke_syscall,
            read_registers,
        )
        .is_err()
    {
        emit_marker(emit, b"detcore-dbi: root post-exec hook failed\n");
        return 1;
    }
    thread.initialized = true;
    0
}

/// Dispatches one DynamoRIO syscall event through the real Detcore Tool.
///
/// # Safety
///
/// All pointers and callbacks must remain valid for this callback. `args` must
/// address six syscall arguments and `result` must be writable.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_pre_syscall(
    context: *mut c_void,
    scratch: *mut c_void,
    tid: i32,
    pid: i32,
    sysnum: i64,
    args: *const u64,
    branches: u64,
    result: *mut i64,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    read_memory: MemoryReader,
    emit: unsafe extern "C" fn(*const u8, usize),
) -> i32 {
    let first_event = TOTAL_SYSCALLS.fetch_add(1, Ordering::Relaxed) == 0;
    if first_event {
        let message = b"detcore-dbi: entered Rust syscall callback\n";
        unsafe { emit(message.as_ptr(), message.len()) };
    }
    let raw_args = unsafe { std::slice::from_raw_parts(args, 6) };
    let clone3_flags = if sysnum == libc::SYS_clone3
        && raw_args[0] != 0
        && raw_args[1] >= std::mem::size_of::<u64>() as u64
    {
        let mut flags = 0_u64;
        let read = unsafe {
            read_memory(
                raw_args[0] as usize,
                (&mut flags as *mut u64).cast(),
                std::mem::size_of_val(&flags),
            )
        };
        (read != 0).then_some(flags)
    } else {
        None
    };
    let pending_thread_clone = thread_clone_metadata(sysnum, raw_args, read_memory);
    if requires_native_process_lifecycle(sysnum, raw_args, clone3_flags) {
        if matches!(sysnum, libc::SYS_execve | libc::SYS_execveat) {
            RUNTIME_SHUTDOWN.store(true, Ordering::Release);
            while !RUNTIME_STOPPED.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        return 0;
    }
    if matches!(sysnum, libc::SYS_clone | libc::SYS_clone3) && pending_thread_clone.is_none() {
        // The kernel must report EFAULT/EINVAL without Detcore retaining
        // parent clone state when clone_args is only partially readable.
        return 0;
    }
    TOTAL_BRANCHES.store(branches, Ordering::Relaxed);
    update_memory_hash(sysnum, raw_args, read_memory);
    let runtime = RUNTIME
        .get()
        .expect("native client dispatched before Detcore runtime initialization");
    let tool = runtime
        .tool
        .get_or_init(|| Detcore::new(Pid::from_raw(pid), &runtime.config));
    let tid = Pid::from_raw(tid);
    let pid = Pid::from_raw(pid);
    let syscall = Syscall::from_raw(
        Sysno::from(sysnum as i32),
        SyscallArgs::new(
            raw_args[0] as usize,
            raw_args[1] as usize,
            raw_args[2] as usize,
            raw_args[3] as usize,
            raw_args[4] as usize,
            raw_args[5] as usize,
        ),
    );

    if first_event {
        let message = b"detcore-dbi: initializing Detcore thread state\n";
        unsafe { emit(message.as_ptr(), message.len()) };
    }
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    if scratch.runtime_state.is_null() {
        if first_event {
            let message = b"detcore-dbi: constructing Detcore thread state\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        if tid != pid {
            unsafe { result.write(-(Errno::EIO.into_raw() as i64)) };
            return 1;
        }
        let state = tool.init_thread_state(Tid::from_raw(tid.into()), None);
        let runtime_state = Box::new(ThreadRuntime {
            tid,
            state,
            initialized: false,
            pending_clone: None,
        });
        if first_event {
            let message = b"detcore-dbi: Detcore thread state constructed\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        scratch.runtime_state = Box::into_raw(runtime_state);
    }
    let thread = unsafe { &mut *scratch.runtime_state };
    if !thread.initialized {
        if first_event {
            let message = b"detcore-dbi: running Detcore thread-start hook\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        if let Err(error) = reverie_dbi::run_tool_thread_start(
            tool,
            context as usize,
            tid,
            pid,
            branches,
            &mut thread.state,
            &runtime.global,
            &runtime.config,
            invoke_syscall,
            read_registers,
        ) {
            unsafe { result.write(error_result(error)) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            return 1;
        }
        if first_event {
            let message = b"detcore-dbi: thread-start hook completed; running post-exec\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        if tid == pid
            && let Err(errno) = reverie_dbi::run_tool_post_exec(
                tool,
                context as usize,
                tid,
                pid,
                branches,
                &mut thread.state,
                &runtime.global,
                &runtime.config,
                invoke_syscall,
                read_registers,
            )
        {
            unsafe { result.write(-(errno.into_raw() as i64)) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            return 1;
        }
        if first_event {
            let message = b"detcore-dbi: post-exec hook completed\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        thread.initialized = true;
    }

    if first_event {
        let message = b"detcore-dbi: dispatching first syscall through Detcore\n";
        unsafe { emit(message.as_ptr(), message.len()) };
    }
    let outcome = reverie_dbi::run_tool_syscall(
        tool,
        context as usize,
        tid,
        pid,
        branches,
        &mut thread.state,
        &runtime.global,
        &runtime.config,
        syscall,
        invoke_syscall,
        read_registers,
    );
    match outcome {
        Ok(DbiSyscallOutcome::Suppress(value)) => {
            unsafe { result.write(value) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            1
        }
        Ok(DbiSyscallOutcome::AllowOriginal) => {
            if let Some(pending_thread_clone) = pending_thread_clone {
                thread.state.clone_flags = Some(pending_thread_clone.flags);
                assert!(thread.pending_clone.replace(pending_thread_clone).is_none());
            }
            0
        }
        Err(error) => {
            unsafe { result.write(error_result(error)) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            1
        }
    }
}

/// Completes a syscall that the DBI guest deferred to DynamoRIO's native path.
///
/// Thread clone is split across pre- and post-syscall callbacks so the child
/// returns through DynamoRIO application code while Detcore still inherits and
/// registers its deterministic thread state.
///
/// # Safety
///
/// The callback pointers and scratch storage must remain valid for this call.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_post_syscall(
    context: *mut c_void,
    scratch: *mut c_void,
    tid: i32,
    pid: i32,
    _sysnum: i64,
    _args: *const u64,
    branches: u64,
    original_result: i64,
    result: *mut i64,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    emit: Emitter,
) -> i32 {
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    if scratch.runtime_state.is_null() {
        return 0;
    }
    let thread = unsafe { &mut *scratch.runtime_state };
    let Some(pending) = thread.pending_clone.take() else {
        return 0;
    };
    if info_logging_enabled() {
        emit_marker(emit, b"detcore-dbi: parent clone post-hook entered\n");
    }
    let runtime = RUNTIME.get().expect("Detcore DBI runtime was initialized");
    let tool = runtime
        .tool
        .get()
        .expect("Detcore DBI tool was initialized");
    let tid = Pid::from_raw(tid);
    let pid = Pid::from_raw(pid);

    let mut child = (original_result >= 0).then(|| {
        let child_tid = Pid::from_raw(original_result as i32);
        let state = tool.init_thread_state(
            Tid::from_raw(child_tid.into()),
            Some((Tid::from_raw(tid.into()), &thread.state)),
        );
        (
            child_tid,
            Box::new(ThreadRuntime {
                tid: child_tid,
                state,
                initialized: false,
                pending_clone: None,
            }),
        )
    });

    let mut guest = reverie_dbi::DbiGuest::new(
        context as usize,
        tid,
        pid,
        None,
        branches,
        &mut thread.state,
        &runtime.global,
        &runtime.config,
        invoke_syscall,
        read_registers,
    );
    let mut child_exited_before_admission = false;
    if let Some((child_tid, child_state)) = child.take() {
        let mut child_state = Some(child_state);
        child_exited_before_admission = {
            let mut handoffs = THREAD_HANDOFFS.lock().unwrap();
            if handoffs.exited.remove(&child_tid.as_raw()) {
                true
            } else {
                assert!(
                    handoffs
                        .inherited
                        .insert(child_tid.as_raw(), child_state.take().unwrap())
                        .is_none(),
                    "duplicate inherited DBI thread state for {child_tid}"
                );
                false
            }
        };
        if child_exited_before_admission {
            drop(child_state.unwrap());
        } else if info_logging_enabled() {
            emit_marker(emit, b"detcore-dbi: child state published\n");
        }
    }
    if child_exited_before_admission {
        let _discarded =
            tool.discard_external_thread_clone(&mut guest, pending.flags, original_result);
    } else {
        let _registration = run_guest_future(tool.register_external_thread_clone(
            &mut guest,
            pending.flags,
            pending.child_tid_address,
            original_result,
        ));
    }
    match run_guest_future(tool.finish_external_thread_clone(&mut guest, pending.sysno)) {
        Ok(()) => 0,
        Err(error) => {
            unsafe { result.write(error_result(error)) };
            1
        }
    }
}

/// Routes a DynamoRIO signal-delivery event through Detcore's scheduler.
///
/// Returning one delivers the signal; zero suppresses it.
///
/// # Safety
///
/// The callback pointers and scratch storage must remain valid for this call.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_signal(
    context: *mut c_void,
    scratch: *mut c_void,
    tid: i32,
    pid: i32,
    signal: i32,
    branches: u64,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    emit: Emitter,
) -> i32 {
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    if scratch.runtime_state.is_null() {
        emit_marker(
            emit,
            b"detcore-dbi: signal arrived before thread admission\n",
        );
        return -1;
    }
    let Ok(signal) = Signal::try_from(signal) else {
        emit_marker(emit, b"detcore-dbi: unsupported realtime signal\n");
        return -1;
    };
    TOTAL_SIGNALS.fetch_add(1, Ordering::Relaxed);
    let runtime = RUNTIME.get().expect("Detcore DBI runtime was initialized");
    let tool = runtime
        .tool
        .get()
        .expect("Detcore DBI tool was initialized");
    let thread = unsafe { &mut *scratch.runtime_state };
    match reverie_dbi::run_tool_signal(
        tool,
        context as usize,
        Pid::from_raw(tid),
        Pid::from_raw(pid),
        branches,
        &mut thread.state,
        &runtime.global,
        &runtime.config,
        signal,
        invoke_syscall,
        read_registers,
    ) {
        Ok(Some(_)) => 1,
        Ok(None) => 0,
        Err(_) => {
            emit_marker(emit, b"detcore-dbi: signal hook failed\n");
            -1
        }
    }
}

/// Returns the linked Reverie Tool name for native DBI-path evidence.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbi_runtime_name() -> *const libc::c_char {
    c"Detcore".as_ptr()
}

/// Returns Detcore DBI counters and the observed guest-memory hash.
///
/// # Safety
///
/// Every output pointer must be aligned and writable for one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_totals(
    branches: *mut u64,
    syscalls: *mut u64,
    rewritten: *mut u64,
    signals: *mut u64,
    memory_hash: *mut u64,
) {
    unsafe {
        branches.write(TOTAL_BRANCHES.load(Ordering::Relaxed));
        syscalls.write(TOTAL_SYSCALLS.load(Ordering::Relaxed));
        rewritten.write(TOTAL_REWRITTEN.load(Ordering::Relaxed));
        signals.write(TOTAL_SIGNALS.load(Ordering::Relaxed));
        memory_hash.write(MEMORY_HASH.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn read_local(address: usize, output: *mut u8, length: usize) -> i32 {
        // SAFETY: tests pass a live in-process object and an adequately sized output.
        unsafe { std::ptr::copy_nonoverlapping(address as *const u8, output, length) };
        1
    }

    #[test]
    fn thread_clone_metadata_captures_clone_and_clone3_tid_state() {
        let mut args = [0_u64; 6];
        let flags =
            CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_CHILD_CLEARTID;
        args[0] = flags.bits();
        args[3] = 0x1234;
        assert_eq!(
            thread_clone_metadata(libc::SYS_clone, &args, read_local),
            Some(PendingThreadClone {
                sysno: Sysno::clone,
                flags,
                child_tid_address: 0x1234,
            })
        );

        let mut clone_args: CloneArgs = unsafe { std::mem::zeroed() };
        clone_args.flags = flags;
        clone_args.child_tid = 0x5678;
        args[0] = (&clone_args as *const CloneArgs) as u64;
        for size in [64_u64, 80, std::mem::size_of::<CloneArgs>() as u64] {
            args[1] = size;
            assert_eq!(
                thread_clone_metadata(libc::SYS_clone3, &args, read_local),
                Some(PendingThreadClone {
                    sysno: Sysno::clone3,
                    flags,
                    child_tid_address: 0x5678,
                })
            );
        }

        args[0] = libc::SIGCHLD as u64;
        assert_eq!(
            thread_clone_metadata(libc::SYS_clone, &args, read_local),
            None
        );
    }

    #[test]
    fn process_lifecycle_syscalls_stay_native() {
        let args = [0_u64; 6];
        for sysnum in [
            libc::SYS_fork,
            libc::SYS_vfork,
            libc::SYS_wait4,
            libc::SYS_waitid,
            libc::SYS_rt_sigreturn,
            libc::SYS_execve,
            libc::SYS_execveat,
        ] {
            assert!(requires_native_process_lifecycle(sysnum, &args, None));
        }
        assert!(!requires_native_process_lifecycle(
            libc::SYS_read,
            &args,
            None
        ));
    }

    #[test]
    fn clone_classification_separates_processes_from_threads() {
        let mut args = [0_u64; 6];
        args[0] = libc::SIGCHLD as u64;
        assert!(requires_native_process_lifecycle(
            libc::SYS_clone,
            &args,
            None
        ));

        args[0] = libc::CLONE_THREAD as u64;
        assert!(!requires_native_process_lifecycle(
            libc::SYS_clone,
            &args,
            None
        ));
        args[0] |= libc::CLONE_VFORK as u64;
        assert!(requires_native_process_lifecycle(
            libc::SYS_clone,
            &args,
            None
        ));
        assert!(requires_native_process_lifecycle(
            libc::SYS_clone3,
            &args,
            Some(libc::SIGCHLD as u64)
        ));
        assert!(!requires_native_process_lifecycle(
            libc::SYS_clone3,
            &args,
            Some(libc::CLONE_THREAD as u64)
        ));
        assert!(requires_native_process_lifecycle(
            libc::SYS_clone3,
            &args,
            Some((libc::CLONE_THREAD | libc::CLONE_VFORK) as u64)
        ));
        assert!(requires_native_process_lifecycle(
            libc::SYS_clone3,
            &args,
            None
        ));
    }
}
