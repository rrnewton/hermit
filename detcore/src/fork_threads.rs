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
//!    descriptor, as glibc's own fork does. In the copy, the calling thread
//!    then goes on as a new thread, so that the copy keeps its first thread
//!    (below).
//! 3. In the copy, [`Parked::resurrect`] clones one new thread per saved
//!    context, with the same thread pointer and glibc's descriptor ID updated,
//!    and the new thread resumes the saved context: it returns from the
//!    handler, whose `rt_sigreturn` restores the thread exactly as the signal
//!    found it, from the signal frame on its own (copied) stack.
//! 4. In the template, [`Parked::release`] lets every parked thread go on.
//!
//! The copy's first thread. The thread that forks becomes the copy's first
//! thread, its thread-group leader. Once a process's leader has exited, what
//! `/proc/self` reads through the leader (`fd`, `fdinfo`, `cwd`, `exe`,
//! `maps`) is gone for every thread of the process: `/proc/self/fd/<n>`
//! fails with ENOENT. A run keeps its first thread for its whole life, but
//! the forking thread is a guest thread's supervisor, which exits with its
//! guest thread. So in the copy, before anything else, the forking thread
//! parks itself with the parking signal, and the handler starts a new thread
//! on the saved context, as [`Parked::resurrect`] does; the new thread goes
//! on as the forking thread. The first thread stays in the handler, with
//! every signal blocked and off its stack, until the copy exits. When the new
//! thread cannot start, [`hand_off_error`] says why.
//!
//! What a re-created thread does not get back: signals pending for the old
//! thread, timers aimed at its ID, and kernel state bound to its ID that is
//! not re-read (the run's perf events re-open on an ID change). x86-64 only.

use std::cell::UnsafeCell;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// The parking signal: an otherwise unused real-time signal.
const SIG_PARK: libc::c_int = 50;
/// The most threads one capture parks.
const MAX_THREADS: usize = 256;
/// The slot after the parked threads' slots: in a copy, the forking thread's
/// context, for the thread that goes on in its place ([`fork_process`]).
const SUCCESSOR: usize = MAX_THREADS;
/// A re-created thread's clone: a thread of this process with its saved
/// thread pointer, whose glibc descriptor ID field the kernel sets to the new
/// thread's ID and clears at its exit, as for any pthread.
const THREAD_FLAGS: libc::c_int = libc::CLONE_VM
    | libc::CLONE_FS
    | libc::CLONE_FILES
    | libc::CLONE_SIGHAND
    | libc::CLONE_THREAD
    | libc::CLONE_SYSVSEM
    | libc::CLONE_SETTLS
    | libc::CLONE_PARENT_SETTID
    | libc::CLONE_CHILD_CLEARTID;
/// The size of a re-created thread's first stack, where it runs only
/// `multiverse_thread_setup` before it resumes on its own stack.
const FIRST_FRAMES: usize = 64 * 1024;
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

struct Table(UnsafeCell<[Slot; MAX_THREADS + 1]>);
// SAFETY: each parked thread's slot is written by one handler (its index
// comes from a fetch_add) before PARKED counts it, and read after the forker
// saw it. The successor's slot is written and read by a copy's first thread
// while it is the copy's only thread, and then by the thread it starts.
unsafe impl Sync for Table {}

// SAFETY: Slot is plain data, valid when zeroed.
static SLOTS: Table = Table(UnsafeCell::new(unsafe { std::mem::zeroed() }));
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);
/// Threads that saved their context in the current session.
static PARKED: AtomicU32 = AtomicU32::new(0);
/// The session parked threads wait for: they go on once RELEASED >= theirs.
static RELEASED: AtomicU32 = AtomicU32::new(0);
static SESSION: AtomicU32 = AtomicU32::new(0);
/// Set in a copy while its first thread hands itself off: the parking
/// handler then saves into the successor's slot and starts the successor
/// instead of waiting for a release.
static HANDING_OFF: AtomicBool = AtomicBool::new(false);
/// The error number with which a copy's successor thread could not start,
/// or 0.
static HAND_OFF_ERROR: AtomicI32 = AtomicI32::new(0);
/// A copy's first thread once it waits for the copy's exit, or 0. It never
/// parks again, so a capture in the copy leaves it out.
static WAITING_FIRST_THREAD: AtomicI32 = AtomicI32::new(0);

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
    ".Lmultiverse_clone_child:",
    "    mov rdi, [rsp]",
    "    and rsp, -16",
    "    call multiverse_thread_setup",
    "    mov rdi, rax",
    "    jmp multiverse_park_resume",
    "3:",
    "    ret",
    "",
    // The same clone, for a copy's first thread handing itself off; the
    // child is multiverse_clone_resume's. The parent returns only clone's
    // error. Once the child exists, the parent's stack is the child's, so the
    // parent touches no memory again: it gives up its clear-TID address (the
    // descriptor's ID field is the child's now) and sleeps in pause, with
    // every signal blocked, until the copy exits.
    ".globl multiverse_hand_off",
    ".type multiverse_hand_off,@function",
    "multiverse_hand_off:",
    "    mov r10, rcx",
    "    mov eax, 56",
    "    syscall",
    "    test rax, rax",
    "    jz .Lmultiverse_clone_child",
    "    js 5f",
    "    xor edi, edi",
    "    mov eax, 218",
    "    syscall",
    "4:",
    "    mov eax, 34",
    "    syscall",
    "    jmp 4b",
    "5:",
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
    fn multiverse_hand_off(
        flags: libc::c_ulong,
        stack: *mut libc::c_void,
        ptid: *mut libc::pid_t,
        ctid: *mut libc::pid_t,
        tls: libc::c_ulong,
    ) -> libc::c_long;
}

fn slot(index: usize) -> *mut Slot {
    // SAFETY: index <= SUCCESSOR; the pointer stays inside the table.
    unsafe { (SLOTS.0.get() as *mut Slot).add(index) }
}

/// A copy's successor thread's first stack: a static buffer, not a new
/// mapping. The hand-off runs before the backend repairs the copy's address
/// space, and a mapping made then could sit where the backend puts back what
/// the fork did not copy (KVM refills the template's perf pages at their own
/// addresses, with MAP_FIXED_NOREPLACE). One hand-off per process image, so
/// one buffer; the successor leaves it at once, for its own stack.
#[repr(C, align(16))]
struct HandOffFrames(UnsafeCell<[u8; FIRST_FRAMES]>);
// SAFETY: written and used only by a copy's first thread while it is the
// copy's only thread, and then by its successor's first frames.
unsafe impl Sync for HandOffFrames {}

static HAND_OFF_FRAMES: HandOffFrames = HandOffFrames(UnsafeCell::new([0; FIRST_FRAMES]));

/// The top of the first stack at `base`, of FIRST_FRAMES bytes, with `slot`
/// written there for `multiverse_clone_resume` and `multiverse_hand_off`.
fn frames_top(base: usize, slot: *mut Slot) -> usize {
    let top = (base + FIRST_FRAMES - 64) & !15;
    // SAFETY: top lies inside the FIRST_FRAMES bytes at `base`.
    unsafe { std::ptr::write(top as *mut *mut Slot, slot) };
    top
}

/// Maps a re-created thread's first stack, with `slot` at its top for
/// `multiverse_clone_resume`, and returns the top. The mapping is never
/// unmapped: the thread leaves it at once, for its own stack, and it is small.
fn first_frames(slot: *mut Slot) -> Result<usize, std::io::Error> {
    // SAFETY: a fresh private mapping; no other memory is touched.
    let stack = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            FIRST_FRAMES,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if stack == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(frames_top(stack as usize, slot))
}

/// Called by the handler: records the thread and returns its slot, or null
/// when the table is full (the thread then does not park, and the capture
/// is refused for a missing thread).
#[unsafe(no_mangle)]
extern "C" fn multiverse_park_slot() -> *mut Slot {
    let index = if HANDING_OFF.load(Ordering::Acquire) {
        SUCCESSOR
    } else {
        let index = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
        if index >= MAX_THREADS {
            return std::ptr::null_mut();
        }
        index
    };
    let slot = slot(index);
    // SAFETY: this handler owns slot `index`: a parked thread's index comes
    // from the fetch_add above, and the successor's slot is a copy's only
    // thread's. Every call is async-signal-safe: raw system calls into
    // memory this frame owns.
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
/// parked and waits until its session is released. A copy's first thread
/// handing itself off starts its successor instead.
#[unsafe(no_mangle)]
extern "C" fn multiverse_park_wait() {
    if HANDING_OFF.load(Ordering::Acquire) {
        start_successor();
        return;
    }
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

/// On a copy's first thread, in the parking handler with every signal
/// blocked and its context saved in the successor's slot: starts the
/// successor on that context and stays in `multiverse_hand_off`, off its
/// stack, until the copy exits. Returns only when the successor could not
/// start, with the error in HAND_OFF_ERROR. Raw system calls only: this runs
/// in a signal handler.
fn start_successor() {
    let slot = slot(SUCCESSOR);
    let error = match glibc_layout() {
        Ok(offset) => {
            let top = frames_top(HAND_OFF_FRAMES.0.get() as usize, slot);
            // SAFETY: the slot holds this thread's own thread pointer; its
            // descriptor's ID field is at fs_base + offset.
            let tid_field = unsafe { ((*slot).fs_base as usize + offset) as *mut libc::pid_t };
            // SAFETY: as documented on multiverse_hand_off; it returns only
            // when no thread was started.
            let failed = unsafe {
                multiverse_hand_off(
                    THREAD_FLAGS as libc::c_ulong,
                    top as *mut libc::c_void,
                    tid_field,
                    tid_field,
                    (*slot).fs_base as libc::c_ulong,
                )
            };
            (-failed) as i32
        }
        Err(_) => libc::EINVAL,
    };
    HAND_OFF_ERROR.store(error, Ordering::Release);
}

/// In a copy, on its first thread, at once after the fork: goes on as a new
/// thread and leaves the first thread waiting for the copy's exit (see the
/// module documentation). Returns on the new thread, or on the first thread
/// when the new one could not start ([`hand_off_error`]).
fn hand_off_first_thread() {
    // SAFETY: raw system calls on this thread, the copy's only one, into
    // memory this frame owns. The handler was installed before the fork.
    unsafe {
        // The new thread registers glibc's rseq area for itself
        // (multiverse_thread_setup); the first thread gives it up first.
        let (offset, size) = RSEQ.get().copied().unwrap_or((0, 0));
        if size > 0 {
            let mut fs: u64 = 0;
            libc::syscall(libc::SYS_arch_prctl, 0x1003 /* ARCH_GET_FS */, &mut fs);
            libc::syscall(
                libc::SYS_rseq,
                (fs as i64 + offset) as u64,
                size.max(32),
                1, /* RSEQ_FLAG_UNREGISTER */
                RSEQ_SIG,
            );
        }
        // The parking signal reaches this thread even if its mask blocks
        // it; the new thread restores the mask.
        let mut park: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut park);
        libc::sigaddset(&mut park, SIG_PARK);
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_UNBLOCK,
            &park,
            &mut mask,
            8,
        );
        HANDING_OFF.store(true, Ordering::SeqCst);
        libc::syscall(
            libc::SYS_tgkill,
            libc::getpid(),
            libc::syscall(libc::SYS_gettid),
            SIG_PARK,
        );
        // Here on the new thread, or on the first one if the new one could
        // not start.
        HANDING_OFF.store(false, Ordering::SeqCst);
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            &mask,
            std::ptr::null_mut::<libc::sigset_t>(),
            8,
        );
        if HAND_OFF_ERROR.load(Ordering::Acquire) == 0 {
            WAITING_FIRST_THREAD.store(libc::getpid(), Ordering::Release);
        }
    }
}

/// In a copy, why the forking thread could not go on as a new thread, if it
/// could not ([`fork_process`]). The copy then runs on its first thread,
/// whose exit would take `/proc/self` away from every other thread.
pub(crate) fn hand_off_error() -> Option<std::io::Error> {
    match HAND_OFF_ERROR.load(Ordering::Acquire) {
        0 => None,
        errno => Some(std::io::Error::from_raw_os_error(errno)),
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

/// Installs the parking handler unless the kernel already has it. The
/// kernel's disposition is the record, never a flag in memory: a process
/// forked while another thread installs the handler copies the dispositions
/// before the memory, so it can get the old disposition with the new flag.
fn install_handler() -> Result<(), String> {
    let handler = multiverse_park_handler as *const () as usize;
    // SAFETY: reads and then installs this process's disposition of
    // SIG_PARK, which nothing else in the process uses (checked).
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(SIG_PARK, std::ptr::null(), &mut old);
        if old.sa_sigaction == handler {
            return Ok(());
        }
        if old.sa_sigaction != libc::SIG_DFL {
            return Err(format!("signal {SIG_PARK} is already handled"));
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigfillset(&mut action.sa_mask);
        if libc::sigaction(SIG_PARK, &action, std::ptr::null_mut()) != 0 {
            return Err(format!("sigaction: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
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
    // Listed with raw getdents64 (util::find_in_directory), never glibc's
    // opendir, whose buffer would come from the C library's heap.
    let mut tids: Vec<libc::pid_t> = Vec::new();
    crate::util::find_in_directory(std::path::Path::new("/proc/self/task"), |name| {
        if let Some(tid) = name.to_str().and_then(|name| name.parse().ok()) {
            tids.push(tid);
        }
        None::<()>
    })
    .map_err(|e| format!("/proc/self/task: {e}"))?;
    // In a copy, its first thread waits for the copy's exit with every
    // signal blocked and holds no state of the run (fork_process).
    let first = WAITING_FIRST_THREAD.load(Ordering::Acquire);
    tids.retain(|&tid| tid != me && tid != first && !is_user_worker(tid));
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
/// is updated (CLONE_CHILD_SETTID), as glibc's own fork does. In the copy it
/// returns 0 on the thread that goes on in this one's place, the copy's
/// first thread staying behind (see the module documentation), or on this
/// thread when that one could not start ([`hand_off_error`]).
pub(crate) fn fork_process() -> Result<libc::pid_t, String> {
    let offset = glibc_layout()?;
    install_handler()?;
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
    if pid == 0 {
        hand_off_first_thread();
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
        for index in 0..self.count {
            let slot = slot(index);
            if let Some(affinity) = affinity {
                // SAFETY: the slot is this copy's own memory; no thread uses
                // it until the clone below.
                unsafe { (*slot).affinity = *affinity };
            }
            // The new thread finds its slot at the top of its first stack.
            let top = first_frames(slot).map_err(|error| format!("mmap: {error}"))?;
            // SAFETY: the slot came from a parked thread of this process
            // image; its descriptor's ID field is at fs_base + offset.
            let tid_field = unsafe { ((*slot).fs_base as usize + offset) as *mut libc::pid_t };
            // SAFETY: as documented on multiverse_clone_resume.
            let tid = unsafe {
                multiverse_clone_resume(
                    THREAD_FLAGS as libc::c_ulong,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The tasks of process `pid`.
    fn task_count(pid: libc::pid_t) -> usize {
        let mut count = 0;
        let _ = crate::util::find_in_directory(
            std::path::Path::new(&format!("/proc/{pid}/task")),
            |name| {
                if name
                    .to_str()
                    .is_some_and(|name| name.parse::<u32>().is_ok())
                {
                    count += 1;
                }
                None::<()>
            },
        );
        count
    }

    /// A copy's forking thread is a guest thread's supervisor, which exits
    /// with its guest thread. The copy's descriptors must stay readable
    /// through /proc after that, because the sharing gate and the backend
    /// read them there. Before the copy kept its first thread, the forking
    /// thread was the copy's leader, and its exit made /proc/<copy>/fd/<n>
    /// fail with ENOENT. The forking thread blocks the parking signal here,
    /// as a supervisor thread may.
    #[test]
    fn a_copy_keeps_its_first_thread_when_the_forking_thread_exits() {
        let mut fds = [0; 2];
        // SAFETY: creates a pipe into `fds`.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let [reader, writer] = fds;
        // SAFETY: signal set operations on local sets, and this thread's mask.
        let mask = unsafe {
            let mut park: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut park);
            libc::sigaddset(&mut park, SIG_PARK);
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, &park, &mut mask);
            mask
        };
        let forked = fork_process();
        if forked == Ok(0) {
            // In the copy, on the thread that went on in the forking thread's
            // place. Raw system calls only: another thread of the test
            // process may have held a C library lock at the fork.
            // SAFETY: raw system calls, and a read of this thread's glibc
            // descriptor at the offset fork_process used.
            unsafe {
                let tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
                let mut fs: u64 = 0;
                libc::syscall(libc::SYS_arch_prctl, 0x1003 /* ARCH_GET_FS */, &mut fs);
                let descriptor = TID_OFFSET
                    .get()
                    .map(|offset| *((fs as usize + offset) as *const libc::pid_t));
                let report: u8 = if HAND_OFF_ERROR.load(Ordering::Acquire) != 0 {
                    2
                } else if tid == libc::getpid() {
                    3
                } else if descriptor != Some(tid) {
                    4
                } else {
                    0
                };
                libc::syscall(libc::SYS_write, writer, &report as *const u8, 1);
                if report != 0 {
                    libc::syscall(libc::SYS_exit_group, 0);
                }
                // This thread ends alone, as a supervisor thread ends with
                // its guest thread.
                libc::syscall(libc::SYS_exit, 0);
                libc::_exit(5);
            }
        }
        // SAFETY: restores this thread's mask and closes our write end.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
            libc::close(writer);
        }
        let pid = forked.expect("fork_process");
        let mut report = [9u8];
        let mut poll = libc::pollfd {
            fd: reader,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: polls and reads our own pipe into `report`, then closes it.
        // The copy holds the write end while it lives, so read only once the
        // pipe is ready: a copy that never reports must not block the test.
        let read = unsafe {
            let read = if libc::poll(&mut poll, 1, 30_000) > 0 {
                libc::read(reader, report.as_mut_ptr().cast(), 1)
            } else {
                -1
            };
            libc::close(reader);
            read
        };
        // Then the successor exits, and the copy is down to its first thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut status = 0;
        let ended = loop {
            // SAFETY: reaps our own child if it has ended.
            if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
                break true;
            }
            if task_count(pid) <= 1 || std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let tasks = task_count(pid);
        let link = std::fs::read_link(format!("/proc/{pid}/fd/0"));
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if !ended {
            // SAFETY: ends and reaps our own child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
        }
        assert_eq!(
            (read, report[0]),
            (1, 0),
            "the copy's report: 2 no successor started, 3 the successor is the \
             leader, 4 the descriptor's ID is not the successor's"
        );
        assert!(
            !ended,
            "the copy ended (wait status {status:#x}) when its forking thread's successor did"
        );
        assert_eq!(tasks, 1, "the copy's tasks after the successor's exit");
        assert!(
            link.is_ok(),
            "/proc/<copy>/fd/0 after the successor's exit: {link:?}"
        );
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next());
        assert_eq!(state, Some('S'), "the copy's first thread sleeps: {stat}");
    }

    /// The start addresses of this process's mappings. A mapping that grows,
    /// as the heap does, keeps its start.
    fn mapping_starts() -> std::collections::BTreeSet<usize> {
        std::fs::read_to_string("/proc/self/maps")
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split('-').next())
            .filter_map(|start| usize::from_str_radix(start, 16).ok())
            .collect()
    }

    /// In a child of glibc's fork, whose C library is usable: parks a helper
    /// thread, forks a copy, and in the copy re-creates the helper and parks
    /// again. Returns 0, or 1x for the template's park or fork, 2x for the
    /// copy's (its exit code, as the template saw it; 25 is a mapping the
    /// hand-off added), 30 for a copy that did not exit.
    fn capture_round() -> i32 {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let helper_stop = stop.clone();
        let helper = std::thread::spawn(move || {
            while !helper_stop.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        let Ok(parked) = park_others(std::time::Duration::from_secs(10)) else {
            return 10;
        };
        if parked.count() != 1 {
            parked.release();
            return 11;
        }
        let before = mapping_starts();
        let code = match fork_process() {
            Ok(0) => {
                // In the copy, on the forking thread's successor. The
                // hand-off may add no mapping: the backend's repair, which
                // runs after it, puts back what the fork did not copy at the
                // template's addresses.
                let added = mapping_starts().difference(&before).count();
                // SAFETY: gettid and getpid have no preconditions.
                let leader = unsafe { libc::syscall(libc::SYS_gettid) == libc::getpid() as i64 };
                let code = if hand_off_error().is_some() {
                    20
                } else if leader {
                    21
                } else if added != 0 {
                    25
                } else {
                    match parked.resurrect(None) {
                        Ok(1) => match park_others(std::time::Duration::from_secs(5)) {
                            Ok(again) => {
                                let count = again.count();
                                again.release();
                                if count == 1 { 0 } else { 22 }
                            }
                            Err(_) => 23,
                        },
                        _ => 24,
                    }
                };
                // SAFETY: _exit ends the copy at once.
                unsafe { libc::_exit(code) };
            }
            Ok(copy) => {
                parked.release();
                let mut status = 0;
                // SAFETY: waits for our own child.
                unsafe { libc::waitpid(copy, &mut status, 0) };
                if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else {
                    30
                }
            }
            Err(_) => {
                parked.release();
                12
            }
        };
        stop.store(true, Ordering::Release);
        let _ = helper.join();
        code
    }

    /// A capture in a copy parks the copy's threads but leaves out its first
    /// thread, which waits with every signal blocked and would never park:
    /// a capture that counted it would wait out its timeout and be refused.
    #[test]
    fn a_copy_re_creates_its_threads_and_its_capture_leaves_the_first_thread_out() {
        // Made before the fork: a child forked while another test thread is
        // still making them would inherit them half-made and wait forever.
        glibc_layout().expect("glibc's thread descriptor layout");
        install_handler().expect("the parking handler");
        // SAFETY: glibc's fork; the child runs only the round and exits.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
        if child == 0 {
            let code = std::panic::catch_unwind(capture_round).unwrap_or(90);
            // SAFETY: _exit ends the child at once.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: waits for our own child.
        unsafe { libc::waitpid(child, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "the round's wait status {status:#x}: exit code 1x the template's park \
             or fork, 2x the copy's, 30 a copy that did not exit, 90 a panic"
        );
    }
}
