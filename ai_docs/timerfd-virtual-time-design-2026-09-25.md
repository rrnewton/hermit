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

## Scheduler bookkeeping: none

The scheduler holds no timerfd state. Every thread that waits on a timerfd
(blocking read, poll, ppoll, epoll_wait, epoll_pwait, select, pselect6) is a
polling thread: it keeps its place in the run queue, takes a scheduler turn
per retry, and judges readiness from the timer state at the logical time of
that retry. Such a thread never parks as a timed waiter, so a scheduler timer
event could not wake it earlier. An earlier revision registered each armed
timerfd as a scheduler timed event anyway. Measured on that revision (strict,
ptrace, debug build), it bought no liveness: a one-hour timerfd read still did
not finish within 30s. It cost two regressions. An armed periodic timerfd kept
the timed-event queue non-empty forever, so a genuinely deadlocked program
spun instead of being reported (rc 124 instead of 125). The empty-queue
fast-forward also stepped one interval at a time, so a 1s sleep beside an
unwatched 1ms periodic timerfd took 12s of wall time, and one beside a 1us
timer did not finish. With no scheduler state there is also no lifecycle to
get wrong: close, dup, fork, exec and process exit need no cleanup beyond the
open file description itself.

## Syscalls

- `timerfd_settime`: error order follows `fs/timerfd.c` — EFAULT for
  `new_value`, then EINVAL for flags or an invalid timespec (negative, or
  nanoseconds >= 1e9), then EBADF / EINVAL for the descriptor. An invalid
  request changes nothing. Re-arming resets the pending count; the old value
  is copied out last.
- `timerfd_create`: unknown flags or clocks are EINVAL, checked before the
  alarm clocks' EPERM, as in `fs/timerfd.c`.
- `timerfd_gettime`: the descriptor is resolved first (EBADF, or EINVAL for
  a non-timerfd), then the output pointer (EFAULT). Zeros when disarmed or an
  expired one-shot; otherwise time to the next expiry and the armed interval.
- `read`, `readv`, and `preadv2` with offset -1 and no flags: a total
  length below 8 is EINVAL; pending > 0 takes the count, then copies the u64
  across the iovecs, and a faulting copy is EFAULT with the count already
  taken, as on Linux; empty + O_NONBLOCK is EAGAIN; otherwise it polls on
  scheduler turns. A signal returns ERESTARTSYS, so SA_RESTART restarts the
  read as on Linux. Positioned reads (`pread64`, `preadv`, `preadv2` with an
  offset) reach the host vessel, which is an anonymous inode and returns
  ESPIPE as Linux does.

## Readiness in waits (`detcore/src/syscalls/io.rs`)

- epoll: `epoll_ctl` keeps a detcore interest per epoll description, keyed
  by (fd, open file) and holding a weak link to the timerfd file, so the
  interest dies with the file's last reference as on Linux. Only EPOLLIN is
  reported. EPOLLONESHOT disables the interest until MOD. EPOLLET reports
  once per arming generation. Only events that fit in `maxevents` consume
  an edge or a oneshot.
- poll/ppoll: POLLIN on a ready timerfd is merged into the returned array.
- select/pselect6: ready timerfds are set in the read bitmap and counted.
- Blocking waits (`wait_with_timerfds`) take a scheduler turn, probe the
  host with a zero timeout, and rescan the virtual timers after EVERY probe,
  the same way the select loops and the blocking read do. A timer that
  another thread arms, re-arms, reads, or adds to the epoll with `epoll_ctl`
  while this thread waits is therefore seen at the next retry. The loop keeps
  no timer deadline of its own; it ends on readiness, the guest timeout, a
  host error, or a signal. Every blocking epoll_wait and NULL-mask
  epoll_pwait uses this loop, so an interest added mid-wait is found; poll
  and ppoll use it when the array names a virtual timerfd. Remaining-time
  outputs are written as before.
- Clock reads are gated: a wait that watches no virtual timerfd sees no
  extra time observation, so unrelated programs keep their event streams.
  poll/ppoll skip reading the pollfd array entirely when timerfds are not
  virtual.

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
- A long blocking timerfd wait advances at the polling rate rather than
  jumping to the deadline, as the existing poll and epoll timeouts already
  do. Measured once each with the debug build (strict, ptrace, default
  log), wall time for a 120s wait: timerfd read 11.0s, poll 31.3s, epoll
  28.7s; with no timerfd, poll 34.7s and epoll 27.8s. A one-hour wait of
  either kind does not finish within 30s.
- `timerfd_gettime` and the settime old-value path forward a periodic timer
  on Linux, which raises a fresh EPOLLET edge; the model raises edges only on
  settime and consuming reads, so an EPOLLET waiter can miss that one extra
  wakeup.
- `preadv2` with offset -1 and nonzero flags, and reads through io_uring,
  AIO or splice, reach the never-armed host vessel.
- The link from an epoll interest to its timerfd file is not serialized
  (`#[serde(skip)]` in `detcore/src/fd.rs`); only the ptrace backend, which
  keeps thread state in process, was measured.
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
Revision 4 (the first pull-request head) added the scheduler timer event and
capped each blocking wait at the earliest expiry computed once per round; both
were removed after review (see "Scheduler bookkeeping" and "Readiness in
waits").
