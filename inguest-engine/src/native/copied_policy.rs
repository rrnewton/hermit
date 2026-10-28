//! The maintained copied-process syscall gate, inside the one core DSO.
//!
//! Shared-VM/vfork children cannot enter an inherited Tool, RPC registry,
//! allocator or mutex. This exact policy is moved from the maintained native
//! Detcore owner; C forwards only its real closed-copy branch and arguments.
//! It never constructs a NativeGuest or issues a lifecycle permit.

use reverie::syscalls::Sysno;

/// The genuine client's closed-copy callback. It deliberately carries no
/// virtual TID, Tool handle, native RPC or lifecycle authority. In particular
/// it must not validate an inherited parent locator as a vfork child's own.
#[repr(C)]
pub struct CopiedCallback {
    pub version: u32,
    pub size: u32,
    pub physical_pid: i32,
    pub physical_tid: i32,
    pub image: u64,
    pub context: usize,
    pub current: Option<reverie_inguest::guest::native_rpc::NativeCurrent>,
}

/// # Safety
/// Only the actual native client's maintained closed-copy pre-syscall path
/// may call this entry. That owner supplies its current context and actual
/// physical IDs/image, with its ordinary native stack/FS and fixed allocator
/// backing intact. The held/hash-bound callback and at most six argument
/// words stay live through the call. Numeric fields do not grant Tool entry.
///
/// Status and guest action have separate channels: a negative status is a
/// mechanism refusal, never a guest errno or permission for kernel fallback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_copied_syscall_v1(
    callback: *const CopiedCallback, number: i64, arguments: *const u64,
    action: *mut i32,
) -> i32 {
    if callback.is_null() || action.is_null() { return -libc::EINVAL; }
    let callback = unsafe { &*callback };
    if callback.version != super::abi::VERSION
        || callback.size as usize != size_of::<CopiedCallback>()
        || callback.physical_pid != unsafe { libc::syscall(libc::SYS_getpid) as i32 }
        || callback.physical_tid != unsafe { libc::syscall(libc::SYS_gettid) as i32 }
        || callback.physical_pid <= 0 || callback.physical_tid <= 0
        || callback.context == 0
    { return -libc::ESTALE; }
    let Some(current) = callback.current else { return -libc::ESTALE; };
    if unsafe { current(callback.context) } != 1 { return -libc::ESTALE; }
    let (strict, report) = match super::bootstrap::copied_policy(callback.image, callback.physical_pid) {
        Ok(policy) => policy,
        Err(error) => return -error.raw_os_error().unwrap_or(libc::EPROTO),
    };
    let result = unsafe { copied_action(number, arguments, strict, report) };
    // Recheck the same real callback after the raw diagnostic/report writes;
    // a changed context cannot turn their result into an admitted guest action.
    if unsafe { current(callback.context) } != 1
        || callback.physical_pid != unsafe { libc::syscall(libc::SYS_getpid) as i32 }
        || callback.physical_tid != unsafe { libc::syscall(libc::SYS_gettid) as i32 }
        || super::bootstrap::copied_policy(callback.image, callback.physical_pid).is_err()
    { return -libc::ESTALE; }
    unsafe { action.write(result); }
    0
}

fn copied_child_flock_action(operation: Option<i32>) -> i32 {
    let Some(operation) = operation else {
        return -libc::ENOLCK;
    };
    let known_flags = libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN | libc::LOCK_NB;
    let base = operation & !libc::LOCK_NB;
    let valid = operation & !known_flags == 0
        && matches!(base, libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN);
    if !valid || base == libc::LOCK_UN || operation & libc::LOCK_NB != 0 {
        0
    } else {
        -libc::ENOLCK
    }
}

/// Whether a copied child's `fcntl` with these arguments sets or clears an OFD
/// lock (`F_OFD_SETLK`, `F_OFD_SETLKW`, whatever the lock type). Missing
/// arguments count as a change, so the strict refusal fails closed.
fn copied_child_fcntl_changes_ofd_lock(args: *const u64) -> bool {
    if args.is_null() {
        return true;
    }
    // SAFETY: Reverie's DYNAMORIO client passes its live six-element syscall
    // argument array for the duration of this callback.
    let command = unsafe { args.add(1).read() } as i32;
    matches!(command, libc::F_OFD_SETLK | libc::F_OFD_SETLKW)
}

/// Whether `sysno` reads the clock or a resource limit, which the DYNAMORIO client no
/// longer answers from a table of its own (Reverie A8 step 2) and which a copied
/// child cannot take to Detcore. A relative `clock_nanosleep` on a wall or
/// monotonic clock only waits and reads nothing; the client's table passed it
/// to the kernel too, so it is not counted here.
fn copied_child_reads_clock_or_limit(sysno: Sysno, args: *const u64) -> bool {
    match sysno {
        Sysno::clock_gettime
        | Sysno::clock_getres
        | Sysno::clock_settime
        | Sysno::gettimeofday
        | Sysno::time
        | Sysno::getrlimit
        | Sysno::setrlimit
        | Sysno::prlimit64 => true,
        Sysno::clock_nanosleep => {
            if args.is_null() {
                return true;
            }
            // SAFETY: Reverie's DYNAMORIO client passes its live six-element syscall
            // argument array for the duration of this callback.
            let (clock, flags) = unsafe { (args.read() as i32, args.add(1).read() as i32) };
            let cpu_clock = clock < 0
                || clock == libc::CLOCK_PROCESS_CPUTIME_ID
                || clock == libc::CLOCK_THREAD_CPUTIME_ID;
            flags != 0 || cpu_clock
        }
        _ => false,
    }
}

unsafe fn copied_action(sysnum: i64, args: *const u64, strict: bool, report_fd: i32) -> i32 {
    let sysno = Sysno::from(sysnum as i32);
    // Bash probes the foreground process group in a copied child before
    // running a background shell function. Hermit's captured stderr is not a
    // terminal, so the instrumented root observes ENOTTY for the same request.
    // Emulate that result instead of either exposing a host terminal's process
    // group or aborting an otherwise deterministic child.
    //
    // Every other ioctl remains fail-closed: copied children still cannot enter
    // the Rust Detcore Tool, and the ABI has no guest-memory channel for safely
    // handling socket timestamps or arbitrary device operations.
    if sysno == Sysno::ioctl && strict {
        let request = if args.is_null() {
            None
        } else {
            // SAFETY: Reverie's DYNAMORIO client passes its live six-element syscall
            // argument array for the duration of this callback.
            Some(unsafe { args.add(1).read() })
        };
        if request == Some(libc::TIOCGPGRP) {
            return -libc::ENOTTY;
        }
        return 1;
    }
    // TODO-HUMAN-REVIEW(PR-981): Copied DYNAMORIO children cannot enter the Rust
    // Detcore Tool. Strict mode therefore fails closed for receive syscalls that
    // can expose native socket timestamps. Non-strict mode retained native
    // behavior until the descriptor-import arm below, which now answers
    // recvmsg and recvmmsg with EPERM there.
    // TODO-HUMAN-REVIEW(PR-972): readlink identity canonicalization also requires
    // Detcore mediation. This ABI has neither syscall arguments nor a memory
    // writer, so a copied child must fail closed rather than expose native
    // pipe/socket inode identities.
    // A copied pre-exec child has no Detcore Tool. Preserve kernel handling
    // for malformed operations, nonblocking locks, and unlock; only blocking
    // acquisitions can strand the child behind a parent that is waiting for it.
    if sysno == Sysno::flock {
        let operation = (!args.is_null()).then(|| unsafe { args.add(1).read() as i32 });
        return copied_child_flock_action(operation);
    }
    // pidfd_getfd NEVER RUNS NATIVELY IN A COPIED CHILD because it is the one native
    // route by which a copied child can reach an open file description that
    // ANOTHER guest process holds a Detcore claim about. A sibling that took
    // LOCK_SH through Detcore holds a known mode for its description; a
    // copied child that duplicates that description with pidfd_getfd and
    // runs LOCK_UN natively (the flock arm above lets unlock through) makes
    // the sibling's claim stale, and its next refused blocking upgrade would
    // "restore" a lock it no longer holds. The instrumented root mediates
    // pidfd_getfd (self only); a copied child cannot enter that handler.
    // Evidence capture refuses this call too, but only in verify runs. The
    // protection does not depend on the unsupported-syscall policy: a stale
    // claim corrupts flock semantics (a "restored" lock the guest released),
    // not only determinism, and before copied children could be admitted
    // after a fork the up-front vfork guard, which is not policy-gated,
    // refused this shape under --allow-unsupported-syscalls too. Only the
    // form of the refusal follows the policy: the default fail-closed policy
    // ends the run by name; the --allow-unsupported-syscalls opt-out
    // answers EPERM without executing the call (the errno Linux gives when
    // ptrace access to the target is denied), as the root answers an
    // unserviceable call with an errno under that opt-out. Ending the run
    // there is not an option: a copied child's fail-closed exit under the
    // opt-out left the run hung (measured: the vfork parent stayed in
    // kernel_clone and the tree never ended). SCM_RIGHTS reception, the
    // other import route, is refused by the recvmsg arm below the same way.
    if sysno == Sysno::pidfd_getfd {
        super::diagnostics::emit(b"detcore-dynamorio: refusing pidfd_getfd in copied child: a native duplicate of another process's open file description could change a lock that process's Detcore state holds a claim about\n");
        return if strict { 1 } else { -libc::EPERM };
    }
    // An OFD lock change NEVER RUNS NATIVELY IN A COPIED CHILD because the
    // `/proc/locks` ledger in Detcore's global state records which OFD locks
    // each description holds, and a copied child's native F_OFD_SETLK or
    // F_OFD_SETLKW (an unlock included) changes the kernel's lock without
    // updating that record. An OFD row prints owner -1 in every PID
    // namespace, and the ledger keeps such a row by field match, so a stale
    // entry admits a host process's identical lock into the guest's
    // /proc/locks. The parent's up-front OFD vfork guard cannot see a lock a
    // sibling took through its own split copy of the description's model, so
    // the refusal belongs at the call. F_OFD_GETLK changes nothing and stays
    // native, as in the root, where it also sees host locks. POSIX
    // F_SETLK/F_SETLKW stay native: POSIX rows are not in the ledger (it
    // keeps OFD and flock rows only) and are kept by their owner pid, and
    // Detcore caches no POSIX lock state. The protection does not depend on
    // the unsupported-syscall policy, for the reason the pidfd_getfd arm
    // gives: the up-front OFD and flock vfork guards it stands in for are not
    // policy-gated. Under the --allow-unsupported-syscalls opt-out the call
    // is answered ENOLCK without executing, as a copied child's blocking
    // flock is (`copied_child_flock_action`), so the kernel's lock and the
    // ledger's record stay in agreement.
    if sysno == Sysno::fcntl && copied_child_fcntl_changes_ofd_lock(args) {
        super::diagnostics::emit(b"detcore-dynamorio: refusing fcntl F_OFD_SETLK/F_OFD_SETLKW in copied child: a native OFD lock change would leave the /proc/locks ledger's record of that description stale\n");
        return if strict { 1 } else { -libc::ENOLCK };
    }
    // recvmsg and recvmmsg NEVER RUN NATIVELY IN A COPIED CHILD, under any
    // policy, because an SCM_RIGHTS message is the other native route by
    // which a copied child can import an open file description another guest
    // process holds a Detcore claim about. A sibling's promoted description
    // (OpenFileState::Shared) keeps its canonical known mode across the
    // sender's sendmsg; a copied child that receives it and runs LOCK_UN
    // natively makes that claim stale, as with pidfd_getfd above. Whether a
    // call carries a control buffer cannot be told here: the msghdr is guest
    // memory and this ABI has no reader (a fault in the runtime would take
    // the place of the guest's EFAULT), so every call is refused, including
    // one without a control buffer that would import nothing. Strict
    // execution already refused both for native socket timestamps. Under the
    // default fail-closed policy the run ends by name; under the
    // --allow-unsupported-syscalls opt-out the call answers EPERM without
    // executing (a security module can refuse a receive with it), for the
    // reason the pidfd_getfd arm gives.
    if matches!(sysno, Sysno::recvmsg | Sysno::recvmmsg) {
        super::diagnostics::emit(b"detcore-dynamorio: refusing recvmsg/recvmmsg in copied child: an SCM_RIGHTS message could import an open file description another process's Detcore state holds a claim about\n");
        return if strict { 1 } else { -libc::EPERM };
    }
    if matches!(sysno, Sysno::readlink | Sysno::readlinkat) && strict {
        return 1;
    }
    // setpgid FAILS CLOSED IN STRICT MODE BECAUSE THE COPIED CHILD CAN PERFORM
    // IT BUT CANNOT RECORD IT.
    //
    // `handle_setpgid` in the instrumented root does two things: it injects the
    // call, so the KERNEL really moves the process group, and then it mirrors
    // that move into Detcore's model with `set_process_group`, "so
    // group-selecting waits do not consult host /proc state". A copied child has
    // no Detcore Tool, so it performs the first half and silently skips the
    // second: the kernel's process groups and Detcore's model diverge, and the
    // next `wait` with a negative pgid selects against a model that is now
    // wrong. That is the machinery hermit#bc9d178 reclassified setpgid to
    // determinize in the first place.
    //
    // ⚠️ AND A FIXED ERRNO IS THE WRONG SHAPE HERE, WHICH IS WHY THIS IS NOT
    // `-EPERM`. The ioctl arm above emulates ENOTTY because the instrumented
    // root observes ENOTTY too -- that is faithful. The root observes setpgid
    // SUCCEEDING, so no errno we return is faithful, and the choice is between
    // two divergences. EPERM is the quiet one: setpgid's return is widely
    // ignored, so a child would carry on believing it leads a process group it
    // does not lead. Failing closed is the loud one, and a wrong answer about
    // process-group identity is worse than no answer.
    //
    // Strict-only, matching the receive/readlink arms directly above: non-strict
    // copied children keep native behaviour, and only strict execution -- which
    // is where determinism is actually claimed -- refuses.
    if sysno == Sysno::setpgid && strict {
        return 1;
    }
    // getpgrp, getpgid and getsid FAIL CLOSED IN STRICT MODE BECAUSE THE
    // KERNEL'S ANSWER IS NOT DETCORE'S.
    //
    // The instrumented root is answered from Detcore's model (slice B3a-4c of
    // https://github.com/rrnewton/hermit/issues/4266): a group or session
    // inherited from outside the container's PID namespace reads 0, as Linux
    // shows it. In a copied child the call would run natively, and the kernel
    // answers for the process group the DYNAMORIO launcher made for the run, whose
    // id is the root's pid. A child would read a different group from its
    // parent's. As with setpgid above, no fixed errno is faithful (the root's
    // calls succeed), and a wrong answer about process-group identity is
    // worse than no answer.
    if matches!(sysno, Sysno::getpgrp | Sysno::getpgid | Sysno::getsid) && strict {
        return 1;
    }
    // Detcore answers ordinary scheduling-policy queries with SCHED_OTHER.
    // A copied child cannot enter the Tool and may inherit a different native
    // policy. Refuse that strict escape without inventing a second answer.
    if sysno == Sysno::sched_getscheduler && strict {
        // The emitter is installed before READY and inherited by copied
        // children. Use its raw write path, without an allocating/locking
        // stderr fallback on the copied runtime stack.
        super::diagnostics::emit(b"detcore-dynamorio: unsupported syscall sched_getscheduler (145) in copied child\n");
        return 1;
    }
    // Ordinary scheduler parameters are written by Detcore's shared Tool.
    // A copied child cannot enter that Tool or write its modeled output through
    // this ABI. Refuse the strict escape without a second parameter policy.
    if sysno == Sysno::sched_getparam && strict {
        super::diagnostics::emit(b"detcore-dynamorio: unsupported syscall sched_getparam (143) in copied child\n");
        return 1;
    }
    // Detcore's shared Tool owns affinity masks. A copied child cannot
    // enter that Tool to write its modeled CPU set; refuse the strict native
    // escape without installing another affinity policy here.
    if sysno == Sysno::sched_getaffinity && strict {
        super::diagnostics::emit(b"detcore-dynamorio: unsupported syscall sched_getaffinity (204) in copied child\n");
        return 1;
    }
    // Ordinary priority queries belong to Detcore's shared policy. A copied
    // child cannot enter that Tool, so strict execution must refuse the native
    // answer rather than install a second priority model here.
    if sysno == Sysno::getpriority && strict {
        super::diagnostics::emit(b"detcore-dynamorio: unsupported syscall getpriority (140) in copied child\n");
        return 1;
    }
    // A copied child's clock and resource-limit reads have no deterministic
    // answer here: the DYNAMORIO client keeps no table of its own any more, and
    // Detcore, which answers them in the root and in forked children, cannot
    // be entered from a copied child. Strict mode refuses them by name rather
    // than let a vfork child read host time or host limits; non-strict keeps
    // native behaviour, as the arms above do.
    if strict && copied_child_reads_clock_or_limit(sysno, args) {
        return 1;
    }
    if detcore::is_deterministically_refused_syscall(sysno)
        && (strict || !detcore::is_strict_only_deterministic_refusal_syscall(sysno))
    {
        return 1;
    }
    if !detcore::is_unsupported_syscall(sysno) {
        return 0;
    }
    if strict {
        1
    } else {
        append_copied_syscall_record(sysnum, report_fd);
        0
    }
}

fn append_copied_syscall_record(sysnum: i64, report_fd: i32) {
    if report_fd == -1 {
        return;
    }
    let mut buffer = [0_u8; 24];
    let mut index = buffer.len() - 1;
    buffer[index] = b'\n';
    let mut value = sysnum as u64;
    loop {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    index -= 1;
    buffer[index] = b'@';
    let _ = unsafe {
        libc::write(
            report_fd,
            buffer[index..].as_ptr().cast(),
            buffer.len() - index,
        )
    };
}
