/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Forking a multi-threaded process with all of its threads (PROTOTYPE).
//!
//! `fork()` copies only the calling thread, and glibc's wrapper then marks
//! every other thread's stack free and clears its thread-specific data. A
//! copy of a run needs every thread: each guest thread has its own host
//! supervisor thread, parked in the middle of its loop. So, as CRIU restores
//! threads:
//!
//! 1. [`park_others`] sends every other thread of the process (kernel worker
//!    threads aside) a signal whose handler saves the thread's callee-saved
//!    registers, stack pointer, return address and thread pointer, then waits.
//! 2. [`fork_process`] forks with a raw `clone`, so glibc's fork bookkeeping
//!    does not run; it only updates the calling thread's ID in its glibc
//!    descriptor, as glibc's own fork does.
//! 3. In the copy, [`Parked::resurrect`] clones one new thread per saved
//!    context, with the same thread pointer and glibc's descriptor ID updated,
//!    and the new thread resumes the saved context: it returns from the
//!    handler, whose `rt_sigreturn` restores the thread exactly as the signal
//!    found it, from the signal frame on its own (copied) stack.
//! 4. In the template, [`Parked::release`] lets every parked thread go on.
//!
//! What a re-created thread does not get back: signals pending for the old
//! thread, timers aimed at its ID, and kernel state bound to its ID that is
//! not re-read (the run's perf events re-open on an ID change). x86-64 only.

use std::cell::UnsafeCell;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// The parking signal: an otherwise unused real-time signal.
const SIG_PARK: libc::c_int = 50;
/// The most threads one capture parks.
const MAX_THREADS: usize = 256;
/// `PF_USER_WORKER` in a task's flags: a kernel worker thread of the process
/// (a KVM VM's huge-page recovery worker), which blocks every signal and is
/// re-created by its own subsystem.
const PF_USER_WORKER: u64 = 0x0000_4000;
/// glibc's rseq signature on x86-64 (`RSEQ_SIG`).
const RSEQ_SIG: u32 = 0x5305_3053;

/// One parked thread.
#[repr(C)]
struct Slot {
    /// rbx, rbp, r12, r13, r14, r15, rsp, rip, as `multiverse_park_save`
    /// writes them. First, at offset 0.
    regs: [u64; 8],
    tid: libc::pid_t,
    fs_base: u64,
    robust_head: u64,
    robust_len: u64,
    comm: [u8; 16],
    affinity: libc::cpu_set_t,
}

struct Table(UnsafeCell<[Slot; MAX_THREADS]>);
// SAFETY: each slot is written by one handler (its index comes from a
// fetch_add) before PARKED counts it, and read after the forker saw it.
unsafe impl Sync for Table {}

// SAFETY: Slot is plain data, valid when zeroed.
static SLOTS: Table = Table(UnsafeCell::new(unsafe { std::mem::zeroed() }));
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);
/// Threads that saved their context in the current session.
static PARKED: AtomicU32 = AtomicU32::new(0);
/// The session parked threads wait for: they go on once RELEASED >= theirs.
static RELEASED: AtomicU32 = AtomicU32::new(0);
static SESSION: AtomicU32 = AtomicU32::new(0);

fn futex_wake(word: &AtomicU32) {
    // SAFETY: wakes waiters on a live word; no memory is written.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAKE,
            i32::MAX,
            std::ptr::null::<libc::timespec>(),
        );
    }
}

fn futex_wait(word: &AtomicU32, expected: u32, timeout: Option<std::time::Duration>) {
    let spec = timeout.map(|t| libc::timespec {
        tv_sec: t.as_secs() as libc::time_t,
        tv_nsec: t.subsec_nanos() as libc::c_long,
    });
    let spec_ptr = spec
        .as_ref()
        .map_or(std::ptr::null(), |spec| spec as *const libc::timespec);
    // SAFETY: waits on a live word while it holds `expected`.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT,
            expected,
            spec_ptr,
        );
    }
}

std::arch::global_asm!(
    ".globl multiverse_park_handler",
    ".type multiverse_park_handler,@function",
    "multiverse_park_handler:",
    "    push rbp",
    "    call multiverse_park_slot",
    "    test rax, rax",
    "    jz 2f",
    "    mov rdi, rax",
    "    call multiverse_park_save",
    "    test eax, eax",
    "    jnz 2f",
    "    call multiverse_park_wait",
    "2:",
    "    pop rbp",
    "    ret",
    "",
    ".globl multiverse_park_save",
    ".type multiverse_park_save,@function",
    "multiverse_park_save:",
    "    mov [rdi], rbx",
    "    mov [rdi + 8], rbp",
    "    mov [rdi + 16], r12",
    "    mov [rdi + 24], r13",
    "    mov [rdi + 32], r14",
    "    mov [rdi + 40], r15",
    "    lea rax, [rsp + 8]",
    "    mov [rdi + 48], rax",
    "    mov rax, [rsp]",
    "    mov [rdi + 56], rax",
    "    xor eax, eax",
    "    ret",
    "",
    ".globl multiverse_park_resume",
    ".type multiverse_park_resume,@function",
    "multiverse_park_resume:",
    "    mov rbx, [rdi]",
    "    mov rbp, [rdi + 8]",
    "    mov r12, [rdi + 16]",
    "    mov r13, [rdi + 24]",
    "    mov r14, [rdi + 32]",
    "    mov r15, [rdi + 40]",
    "    mov rsp, [rdi + 48]",
    "    mov eax, 1",
    "    jmp qword ptr [rdi + 56]",
    "",
    // rdi=flags rsi=new stack (holds the slot pointer at [rsi]) rdx=ptid
    // rcx=ctid r8=tls. The parent returns clone's result; the child runs
    // multiverse_thread_setup(slot) on the new stack, then resumes the slot.
    ".globl multiverse_clone_resume",
    ".type multiverse_clone_resume,@function",
    "multiverse_clone_resume:",
    "    mov r10, rcx",
    "    mov eax, 56",
    "    syscall",
    "    test rax, rax",
    "    jnz 3f",
    "    mov rdi, [rsp]",
    "    and rsp, -16",
    "    call multiverse_thread_setup",
    "    mov rdi, rax",
    "    jmp multiverse_park_resume",
    "3:",
    "    ret",
);

unsafe extern "C" {
    fn multiverse_park_handler(
        sig: libc::c_int,
        info: *mut libc::siginfo_t,
        context: *mut libc::c_void,
    );
    fn multiverse_clone_resume(
        flags: libc::c_ulong,
        stack: *mut libc::c_void,
        ptid: *mut libc::pid_t,
        ctid: *mut libc::pid_t,
        tls: libc::c_ulong,
    ) -> libc::c_long;
}

fn slot(index: usize) -> *mut Slot {
    // SAFETY: index < MAX_THREADS; the pointer stays inside the table.
    unsafe { (SLOTS.0.get() as *mut Slot).add(index) }
}

/// Called by the handler: records the thread and returns its slot, or null
/// when the table is full (the thread then does not park, and the capture
/// is refused for a missing thread).
#[unsafe(no_mangle)]
extern "C" fn multiverse_park_slot() -> *mut Slot {
    let index = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
    if index >= MAX_THREADS {
        return std::ptr::null_mut();
    }
    let slot = slot(index);
    // SAFETY: this handler owns slot `index` (fetch_add above). Every call
    // is async-signal-safe: raw system calls into memory this frame owns.
    unsafe {
        (*slot).tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
        let mut fs: u64 = 0;
        libc::syscall(libc::SYS_arch_prctl, 0x1003 /* ARCH_GET_FS */, &mut fs);
        (*slot).fs_base = fs;
        let mut head: u64 = 0;
        let mut len: libc::size_t = 0;
        libc::syscall(libc::SYS_get_robust_list, 0, &mut head, &mut len);
        (*slot).robust_head = head;
        (*slot).robust_len = len as u64;
        libc::prctl(libc::PR_GET_NAME, (*slot).comm.as_mut_ptr());
        libc::sched_getaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut (*slot).affinity,
        );
    }
    slot
}

/// Called by the handler after the context is saved: counts the thread as
/// parked and waits until its session is released.
#[unsafe(no_mangle)]
extern "C" fn multiverse_park_wait() {
    let session = SESSION.load(Ordering::Acquire);
    PARKED.fetch_add(1, Ordering::AcqRel);
    futex_wake(&PARKED);
    loop {
        let released = RELEASED.load(Ordering::Acquire);
        if released >= session {
            break;
        }
        futex_wait(&RELEASED, released, None);
    }
}

/// Runs on a re-created thread, on its temporary stack, before it resumes:
/// re-registers what the kernel keeps per thread, from the slot.
#[unsafe(no_mangle)]
extern "C" fn multiverse_thread_setup(slot: *mut Slot) -> *mut Slot {
    // SAFETY: the slot was filled by the thread this one replaces; every
    // call is a raw system call on this new thread.
    unsafe {
        // Everything stays blocked until rt_sigreturn restores the thread's
        // own mask, except the faults, so that a fault is reported.
        let all: libc::sigset_t = {
            let mut set = std::mem::zeroed();
            libc::sigfillset(&mut set);
            for fault in [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE] {
                libc::sigdelset(&mut set, fault);
            }
            set
        };
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            &all,
            std::ptr::null_mut::<libc::sigset_t>(),
            8,
        );
        if (*slot).robust_head != 0 {
            libc::syscall(
                libc::SYS_set_robust_list,
                (*slot).robust_head,
                (*slot).robust_len,
            );
        }
        let (offset, size) = RSEQ.get().copied().unwrap_or((0, 0));
        if size > 0 {
            libc::syscall(
                libc::SYS_rseq,
                ((*slot).fs_base as i64 + offset) as u64,
                size.max(32),
                0,
                RSEQ_SIG,
            );
        }
        libc::prctl(libc::PR_SET_NAME, (*slot).comm.as_ptr());
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &(*slot).affinity);
    }
    slot
}

/// glibc's rseq area offset from the thread pointer, and its size.
static RSEQ: std::sync::OnceLock<(i64, u32)> = std::sync::OnceLock::new();
/// The offset of `tid` in glibc's thread descriptor, which starts at the
/// thread pointer on x86-64.
static TID_OFFSET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

fn lookup(name: &str) -> Option<*const libc::c_void> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: dlsym with a NUL-terminated name.
    let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!address.is_null()).then_some(address as *const libc::c_void)
}

fn glibc_layout() -> Result<usize, String> {
    if let Some(offset) = TID_OFFSET.get() {
        return Ok(*offset);
    }
    // libthread_db's description of the field: size in bits, count, offset.
    let tid = lookup("_thread_db_pthread_tid")
        .ok_or("glibc does not describe its thread descriptor (_thread_db_pthread_tid)")?;
    // SAFETY: the symbol is three u32 values.
    let description = unsafe { std::ptr::read(tid as *const [u32; 3]) };
    if description[0] != 32 {
        return Err(format!(
            "glibc's thread ID field is {} bits wide",
            description[0]
        ));
    }
    let rseq = match (lookup("__rseq_offset"), lookup("__rseq_size")) {
        // SAFETY: glibc defines these as ptrdiff_t and unsigned int.
        (Some(offset), Some(size)) => unsafe {
            (
                std::ptr::read(offset as *const i64),
                std::ptr::read(size as *const u32),
            )
        },
        _ => (0, 0),
    };
    let _ = RSEQ.set(rseq);
    let _ = TID_OFFSET.set(description[2] as usize);
    Ok(description[2] as usize)
}

fn install_handler() -> Result<(), String> {
    static INSTALLED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            // SAFETY: reads and then installs this process's disposition of
            // SIG_PARK, which nothing else in the process uses (checked).
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                libc::sigaction(SIG_PARK, std::ptr::null(), &mut old);
                if old.sa_sigaction != libc::SIG_DFL {
                    return Err(format!("signal {SIG_PARK} is already handled"));
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = multiverse_park_handler as *const () as usize;
                action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
                libc::sigfillset(&mut action.sa_mask);
                if libc::sigaction(SIG_PARK, &action, std::ptr::null_mut()) != 0 {
                    return Err(format!("sigaction: {}", std::io::Error::last_os_error()));
                }
            }
            Ok(())
        })
        .clone()
}

fn is_user_worker(tid: libc::pid_t) -> bool {
    // Field 9 of /proc/<tid>/stat is the task's flags; the command name
    // (field 2) is in parentheses and may hold spaces.
    let Ok(stat) = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")) else {
        return false;
    };
    let Some(after) = stat.rfind(')').map(|at| &stat[at + 1..]) else {
        return false;
    };
    after
        .split_whitespace()
        .nth(6)
        .and_then(|flags| flags.parse::<u64>().ok())
        .is_some_and(|flags| flags & PF_USER_WORKER != 0)
}

/// The threads parked by [`park_others`].
#[derive(Debug)]
pub(crate) struct Parked {
    count: usize,
    session: u32,
}

/// Parks every other thread of this process (kernel worker threads aside)
/// in the parking signal's handler, with its context saved. Refused, with
/// every thread released, if one does not park within `timeout`.
pub(crate) fn park_others(timeout: std::time::Duration) -> Result<Parked, String> {
    glibc_layout()?;
    install_handler()?;
    // SAFETY: getpid and gettid have no preconditions.
    let (pid, me) = unsafe {
        (
            libc::getpid(),
            libc::syscall(libc::SYS_gettid) as libc::pid_t,
        )
    };
    let tids: Vec<libc::pid_t> = std::fs::read_dir("/proc/self/task")
        .map_err(|e| format!("/proc/self/task: {e}"))?
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse().ok())
        .filter(|&tid| tid != me && !is_user_worker(tid))
        .collect();
    if tids.len() > MAX_THREADS {
        return Err(format!("{} threads; at most {MAX_THREADS}", tids.len()));
    }
    NEXT_SLOT.store(0, Ordering::Relaxed);
    PARKED.store(0, Ordering::Relaxed);
    let session = SESSION.fetch_add(1, Ordering::AcqRel) + 1;
    let parked = Parked {
        count: tids.len(),
        session,
    };
    for &tid in &tids {
        // SAFETY: signals one thread of this process.
        if unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, SIG_PARK) } != 0 {
            parked.release();
            return Err(format!("tgkill {tid}: {}", std::io::Error::last_os_error()));
        }
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let count = PARKED.load(Ordering::Acquire);
        if count as usize >= tids.len() {
            break;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            parked.release();
            return Err(format!(
                "{count} of {} threads parked within {timeout:?}",
                tids.len()
            ));
        }
        futex_wait(&PARKED, count, Some(deadline - now));
    }
    Ok(parked)
}

/// Forks with a raw `clone`, so that glibc does not mark the parked threads'
/// stacks free in the child; only this thread's ID in its glibc descriptor
/// is updated (CLONE_CHILD_SETTID), as glibc's own fork does.
pub(crate) fn fork_process() -> Result<libc::pid_t, String> {
    let offset = glibc_layout()?;
    // SAFETY: reads this thread's thread pointer.
    let fs = unsafe {
        let mut fs: u64 = 0;
        libc::syscall(libc::SYS_arch_prctl, 0x1003 /* ARCH_GET_FS */, &mut fs);
        fs
    };
    let ctid = (fs as usize + offset) as *mut libc::pid_t;
    // SAFETY: a fork-like clone: no new stack, so the child continues on its
    // copy of this stack; ctid is this thread's descriptor ID field.
    let pid = unsafe {
        libc::syscall(
            libc::SYS_clone,
            (libc::CLONE_CHILD_SETTID | libc::CLONE_CHILD_CLEARTID | libc::SIGCHLD)
                as libc::c_ulong,
            0usize,
            std::ptr::null_mut::<libc::pid_t>(),
            ctid,
            0usize,
        )
    };
    if pid < 0 {
        return Err(format!("clone: {}", std::io::Error::last_os_error()));
    }
    Ok(pid as libc::pid_t)
}

impl Parked {
    /// The number of parked threads.
    pub(crate) fn count(&self) -> usize {
        self.count
    }

    /// In the template: lets every parked thread go on.
    pub(crate) fn release(&self) {
        RELEASED.fetch_max(self.session, Ordering::AcqRel);
        futex_wake(&RELEASED);
    }

    /// In a copy: re-creates every parked thread. Each resumes its saved
    /// context, so it returns from the parking handler where the original
    /// would have, and goes on as the original.
    /// `affinity`, when given, replaces each thread's saved CPU set, so that
    /// a copy pinned to its own CPU keeps all of its threads there.
    pub(crate) fn resurrect(&self, affinity: Option<&libc::cpu_set_t>) -> Result<usize, String> {
        let offset = glibc_layout()?;
        const STACK: usize = 64 * 1024;
        let flags = libc::CLONE_VM
            | libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SIGHAND
            | libc::CLONE_THREAD
            | libc::CLONE_SYSVSEM
            | libc::CLONE_SETTLS
            | libc::CLONE_PARENT_SETTID
            | libc::CLONE_CHILD_CLEARTID;
        for index in 0..self.count {
            let slot = slot(index);
            if let Some(affinity) = affinity {
                // SAFETY: the slot is this copy's own memory; no thread uses
                // it until the clone below.
                unsafe { (*slot).affinity = *affinity };
            }
            // SAFETY: a fresh private mapping for the new thread's first
            // frames; it is never unmapped (the thread leaves it at once,
            // for its own stack, and it is small).
            let stack = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    STACK,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if stack == libc::MAP_FAILED {
                return Err(format!("mmap: {}", std::io::Error::last_os_error()));
            }
            // The new thread finds its slot at the top of its stack.
            let top = (stack as usize + STACK - 64) & !15;
            // SAFETY: top lies inside the new mapping.
            unsafe { std::ptr::write(top as *mut *mut Slot, slot) };
            // SAFETY: the slot came from a parked thread of this process
            // image; its descriptor's ID field is at fs_base + offset.
            let tid_field = unsafe { ((*slot).fs_base as usize + offset) as *mut libc::pid_t };
            // SAFETY: as documented on multiverse_clone_resume.
            let tid = unsafe {
                multiverse_clone_resume(
                    flags as libc::c_ulong,
                    top as *mut libc::c_void,
                    tid_field,
                    tid_field,
                    (*slot).fs_base as libc::c_ulong,
                )
            };
            if tid < 0 {
                return Err(format!(
                    "clone for thread {index}: {}",
                    std::io::Error::from_raw_os_error(-tid as i32)
                ));
            }
        }
        Ok(self.count)
    }
}
