# Timerfd on the virtual clock — design as implemented (2026-09-25)

Cell: `backend-parity-c/timer-family-identity` (ptrace only). The fixture
`tests/backend-parity/fixtures/timer_family_identity.c` asserts the ORDER in
which timer families fire from two threads with widely separated deadlines.
`tests/backend-parity/fixtures/timerfd_semantics.c` checks timerfd syscall
semantics case by case against Linux. Neither fixture may be weakened.

## Reproduced divergence (base 422f3f3a, ptrace, debug binary, 4 runs)

Native order is stable. Under `hermit run` the timerfd line moved run to run:

- runs 1 and 4: `timerfd_expired` BEFORE `futex_timeout_fired`/`timer_create_fired`
- runs 2 and 3: `timerfd_expired` AFTER `timer_create_fired`

Every other family (timer_create 80ms, futex 160ms, epoll timeout 120ms,
ITIMER_REAL 200ms, alarm 1s) was stable.

## Root cause

`timer_create` and `setitimer` are emulated against the virtual clock. A
timerfd was not: `timerfd_create` made a host timerfd and
`timerfd_settime`/`gettime` passed through to the kernel. The host expired the
timer after 40ms of host time while the scheduler advanced virtual time by
committed turns, so whether the timer had fired when another thread probed
`epoll_wait` depended on host timing.

## Mode gate

`virtual_timerfds() = sequentialize_threads && !recordreplay_modes`
(`detcore/src/syscalls/files.rs`). Only then are timerfds virtual. Every other
mode keeps the previous host passthrough unchanged: create lets the kernel
validate the clock, settime and gettime go through `notification_fd_control`,
and read takes the generic notification-fd path. Record/replay and
non-sequentialized runs are excluded because their wait paths block in the
host, where a timer that is never armed in the kernel would never wake them.

## State

- The host timerfd is created as a poll/epoll vessel (fd numbering and host
  registrations are unchanged) and is NEVER armed.
- `TimerFdState` lives on the open file description
  (`Arc<Mutex<..>>` in `detcore/src/fd.rs`), so `dup` and `fork` aliases
  share one timer, as on Linux. Fields: clock,
  deadline, interval, consumed count, CANCEL_ON_SET flag, and an arming
  `generation` bumped by every settime and every consuming read.
- Expirations are a pure function of logical time: 0 before the deadline,
  else `1 + (now - deadline) / interval` (one-shot: 1); pending is that minus
  consumed.
- Clocks: REALTIME, MONOTONIC and BOOTTIME; the alarm clocks return EPERM
  (they need CAP_WAKE_ALARM, which guests lack); anything else EINVAL.
  Detcore has ONE logical clock (`clock_gettime` ignores the clock id and
  `clock_settime`/`settimeofday` are refused), so an ABSTIME deadline is
  the logical time itself for every clock and needs no conversion.
  CANCEL_ON_SET is recorded only for REALTIME+ABSTIME, as Linux does, and
  ECANCELED is unreachable because nothing can set the clock.

## Scheduler bookkeeping

`TimedEvent::TimerFdExpiry(OpenFileId)`, with `timerfd_timers` keyed by the
open file description (not by pid and fd number, which a dup or close/reopen
would alias). The event is declared last, so at one deadline the canonical pop
order is SignalEvt, ThreadEvt, TimerFdExpiry — Linux does not order
same-instant events across families and this fixed order is the disclosed
choice. Popping wakes no thread; the event exists so an otherwise empty queue
fast-forwards to the deadline instead of reporting deadlock. A periodic entry
that is several intervals behind re-arms once, to the first deadline after
now. Entries are removed on disarm, on release of the open file
(`ReleasePort` -> `release_timerfd`), and on process exit; a failed exec
re-registers the process's timerfds.

## Syscalls

- `timerfd_settime`: error order follows `fs/timerfd.c` — EFAULT for
  `new_value`, then EINVAL for flags or an invalid timespec (negative, or
  nanoseconds >= 1e9), then EBADF / EINVAL for the descriptor. An invalid
  request changes nothing. Re-arming resets the pending count; the old value
  is copied out last.
- `timerfd_gettime`: zeros when disarmed or expired one-shot; otherwise time
  to the next expiry and the armed interval.
- `read`: len < 8 is EINVAL; pending > 0 returns the u64 count and consumes
  it; empty + O_NONBLOCK is EAGAIN; otherwise it polls on scheduler turns. A
  signal returns ERESTARTSYS, so SA_RESTART restarts the read as on Linux.

## Readiness in waits (`detcore/src/syscalls/io.rs`)

- epoll: `epoll_ctl` keeps a detcore interest per epoll description, keyed
  by (fd, open file) and holding a weak link to the timerfd file, so the
  interest dies with the file's last reference as on Linux. Only EPOLLIN is
  reported. EPOLLONESHOT disables the interest until MOD. EPOLLET reports
  once per arming generation. Only events that fit in `maxevents` consume
  an edge or a oneshot.
- poll/ppoll: POLLIN on a ready timerfd is merged into the returned array.
- select/pselect6: ready timerfds are set in the read bitmap and counted.
- Blocking waits cap the host polling deadline at the earliest virtual
  expiry and merge virtual readiness after every successful probe; if a
  capped probe finds nothing (another thread consumed or re-armed the timer)
  the wait continues toward the guest deadline. Each cap is strictly later,
  so the loop advances. Remaining-time outputs are written as before.
- Clock reads are gated: a wait that watches no virtual timerfd sees no
  extra time observation, so unrelated programs keep their event streams.

## Limitations (disclosed, not claimed)

- epoll_pwait with a signal mask that must actually block, and epoll_pwait2
  on an instance watching a virtual timerfd, return ENOSYS rather than
  giving up the mask's atomicity. ppoll with a mask that must block was
  already ENOSYS.
- Nested epoll (an epoll fd inside an epoll set) does not see timerfd
  readiness through the inner instance.
- select with nfds > 64 on a zero-timeout probe is not merged.
- Timer events are appended after host events; relative order between a
  timerfd and an external host fd at one probe is Hermit's existing external
  I/O scope.
- Process exit does not send `ReleasePort` (pre-existing); if the arming
  process exits while a forked holder keeps the file, the fast-forward
  target is lost. Readiness stays correct because it is computed from time.
- BOOTTIME does not model suspend.
- Backends: the change is in shared detcore code, but only the ptrace cells
  are enabled and measured; no DBT, KVM, SaBRe or LiteInst claim is made.

## History

Revision 1 proposed converting CLOCK_REALTIME deadlines into a separate
realtime domain; that was wrong for the single logical clock and was dropped.
Revisions 1-3 keyed scheduler and epoll state by (pid, fd); an OFD-level key
replaced it because dup, fork and close/reopen alias descriptor numbers. The
rescue commit armed virtual timerfds in every mode, which would hang record/
replay and non-sequentialized waits; the mode gate above replaced that.
