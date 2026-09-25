# Timerfd virtual-time determinism — design (2026-09-25)

Cell: `backend-parity-c/timer-family-identity` (verify currently `ci: false`,
ptrace-only). Fixture `tests/backend-parity/fixtures/timer_family_identity.c`
asserts the ORDER in which timer families fire, from two threads, with widely
separated virtual deadlines. It must not be weakened; durations are not printed.

## Reproduced divergence (base 422f3f3a, ptrace, debug binary, 4 runs)

Native order is stable. Under `hermit run` the sequence flips run to run:

- runs 1,4: `timerfd_expired` BEFORE `futex_timeout_fired`/`timer_create_fired`
- runs 2,3: `timerfd_expired` AFTER `timer_create_fired`

Every other family (timer_create 80ms, futex 160ms, epoll timeout 120ms,
ITIMER_REAL 200ms, alarm 1s) is stable. Only the timerfd line moves.

## Root cause

`timer_create`/`setitimer` are emulated against the virtual clock
(`detcore/src/syscalls/time.rs` `handle_timer_settime` -> scheduler
`TimedEvents` SignalEvt; `signal.rs` `handle_setitimer`). A timerfd is not:
`handle_timerfd_create` creates a real host timerfd, and
`handle_timerfd_settime`/`gettime` pass through via
`notification_fd_control` -> `record_or_replay`. The host kernel therefore
expires the timerfd after 40ms of *host* time, while the scheduler advances
virtual time by committed turns. Whether the host timer has fired when thread
B's `epoll_wait` probe runs is host timing, so the guest-visible order of
`timerfd_expired` vs the virtual families is nondeterministic. This is a
host-timed input reaching guest-visible ordering with no deterministic drain —
exactly Axis 1 of the deterministic-scheduling review.

## Design: timerfd as a virtual-clock fd (mirrors POSIX timer emulation)

State per open timerfd description (shared on dup, in the DetFd entry):
clockid, armed deadline (LogicalTime on that clock), interval, and a
read cursor. Expirations are a pure function of virtual time:
`count(now) = 0` before deadline, else `1 + (now-deadline)/interval`
(one-shot: 1). Readiness = pending = count(now) - consumed > 0.

1. `timerfd_create`: still allocate a host timerfd as the poll/epoll vessel
   (so host epoll/poll registration and fd numbering are unchanged), but the
   host timer is NEVER armed. All guest-visible semantics come from detcore.
2. `timerfd_settime`: intercept (like `timer_settime`). Validate flags
   (TFD_TIMER_ABSTIME/CANCEL_ON_SET), compute deadline on the fd's virtual
   clock (CLOCK_MONOTONIC -> logical time; CLOCK_REALTIME -> virtual realtime;
   CLOCK_BOOTTIME = MONOTONIC), store state, return old value computed from
   prior state. Register the deadline with the scheduler as a new
   `TimedEvent::TimerFdEvt(detpid, fd)` (re-arm replaces; disarm removes) so
   `step2d_handle_empty_queue` fast-forwards virtual time to it instead of
   reporting deadlock, and periodic timers re-insert deadline+interval on pop.
   Same-deadline ordering vs SignalEvt is by the BTreeSet enum order —
   canonical, and disclosed here as the chosen tie-break.
3. `timerfd_gettime`: remaining = deadline - now on the fd clock (0 if
   expired/disarm), interval as armed; pure function of virtual time.
4. `read`: if pending > 0, write u64 pending, consumed += pending (Linux
   semantics). If none and O_NONBLOCK -> EAGAIN. If blocking: poll via the
   existing nonblocking-retry machinery (turns advance virtual time; the
   TimedEvent guarantees progress when the queue empties). Signal
   interruption follows the retry loop's existing EINTR path.
5. Readiness surfaces: `epoll_ctl` gains a detcore shadow interest list per
   epoll fd (fd -> (timerfd state ref, event template)); the host epoll_ctl
   still runs (vessel). `handle_internal_epoll_wait/pwait`: probe = host
   nonblocking epoll_wait merged with shadow timerfds whose pending > 0 at
   virtual now; if any ready, return merged set (host events keep host order,
   timerfd events appended in fd order — canonical). Blocking case keeps the
   existing retry-with-timeout loop, so a timerfd expiring mid-wait is seen at
   the first probe after virtual time crosses its deadline, and the TimedEvent
   + timeout ThreadEvt bound the wait. `poll/ppoll/select/pselect6`: same
   merge at their existing probe points. This changes no comparator: guest
   syscalls return exactly what Linux would return at that virtual instant.
6. Record/replay: no host state is armed, and every result is a function of
   virtual time and the recorded schedule, so replay reproduces it. The host
   vessel fd contributes only its number (allocated by recorded create).
7. Backends: detcore syscall layer is shared (ptrace/KVM/DBT/SaBRe/LiteInst
   in-scope-of-detcore). The cell enables ptrace only; other backends keep
   their existing disabled reasons — this change does not claim their
   qualification. KVM timed-event path (`insert_kvm_real_deadline`) is a
   separate real-timer mechanism and is not reused.

## Linux semantics preserved (Axis 2)

- Relative vs ABSTIME arming, disarm on zero it_value, old_value reporting.
- Periodic expirations accumulate as u64 count; read drains all pending.
- EAGAIN for nonblocking empty read; blocking read interruptible by signals.
- epoll level-triggered readiness (edge-triggered: readiness re-computed per
  wait; expirations consumed only by read, matching Linux ET behaviour for
  timerfd where a new expiry re-signals).
- CLOCK_REALTIME timerfds follow virtual realtime (settimeofday under Hermit
  is virtual, so no host clock jump can leak in).

## Discriminating tests

- Unit (timed_waiters): TimerFdEvt insert/replace/pop/re-arm ordering vs
  SignalEvt at the same deadline.
- Unit (timerfd state): count arithmetic one-shot/periodic, disarm, abstime.
- detcore integration: fixture binary run 20x under ptrace hermit — EV
  sequence byte-identical every run and equal to the deterministic expected
  order (timerfd 40ms first). Old code flips (measured above), so this test
  discriminates.
- Negative: a timerfd armed AFTER a posix timer must fire after it
  (deadline order, not family order); nonblocking empty read EAGAIN;
  gettime remaining decreases with virtual time only.

## Grounding / limitations

scheduler.rs, timed_waiters.rs, time.rs, signal.rs, io.rs read at base.
PROJECT_VISION/roadmap: continuous fine-grained virtual time is a product
requirement — this design never quantizes/freezes time; deadlines are exact.
ASPLOS'20 paper (DOI 10.1145/3373376.3378519) unreachable from this host
(proxy 403 for the session identity) — disclosed; no claim depends on text
beyond the determinism contract already encoded in the code and fixture.

## Revision 2 — closing the deterministic-scheduling review conditions (F1-F6)

Review verdict on revision 1: REFUSE-WITH-CONDITIONS (F1-F6). Closures:

**F1 variant position + dispatch.** `TimedEvent::TimerFdEvt(DetPid, i32)` is
appended as the LAST variant, so at one deadline the canonical pop order is
SignalEvt, ThreadEvt, TimerFdEvt (derived Ord on the enum declaration order).
Linux does not specify same-instant cross-family order; this fixed order is
the disclosed DetTrace choice. Pop action at BOTH pop sites
(`step2b_process_timed`, `step2d_handle_empty_queue`) and the `thread_status`
match: TimerFdEvt wakes no thread and delivers no signal — its scheduler job
is (a) make the deadline a fast-forward target, (b) periodic re-arm inside
`pop_if_before` (below). Guest-visible effect is computed by detcore from
virtual time, so no thread state (running/queued/polling/host-blocked/
exiting) needs a transition; a pop for a closed/exited fd is a no-op beyond
re-arm suppression (bookkeeping removed at close/exit, F2).

**F2 bookkeeping.** `TimedEvents.timerfd_timers: BTreeMap<(DetPid,i32),
SignalTimerState>` mirrors `signal_timers`: `insert_timerfd` (replace via
clear-old), `remove_timerfd` (disarm), removal in `remove_process_timers`,
and fd-close removal via a scheduler RPC from the close path. Periodic
re-arm lives in `pop_if_before` exactly like signal timers
(deadline+interval, same event reinserted); one-shot entries are dropped.

**F3 merge-point table (complete).** Interception is in the detcore
handlers, before any host injection of timerfd state:
- epoll_wait/epoll_pwait internal sequential path: merge (host probe +
  shadow-virtual ready set).
- timeout==0 probes and record/replay-mode epoll branches: shadow-virtual
  readiness is merged the same way; host results for real fds pass through.
- epoll_pwait with sigmask and epoll_pwait2 (raw): shadow pre-check +
  post-check merge; if a virtual timerfd is ready the wait returns it
  without host blocking (sigmask atomicity is preserved because the call
  then does not block, as Linux permits for ready waits).
- poll/ppoll/select/pselect6: same merge at their probe points.
- Non-sequential config (`sequentialize_threads=false`) is outside the
  strict determinism contract by configuration and keeps host behaviour —
  disclosed, not claimed.
Shadow state: keyed (epoll fd, target fd); ADD/MOD replace, DEL/close of
either fd removes; dup aliases share the OpenFileDescription-keyed timerfd
state. `maxevents`: host-ready events first, then timerfd events in fd
order, truncated to maxevents; unreported ready timerfds stay ready
(level semantics), ET interests latch per interest until pending hits 0 or
MOD re-arms. Scope bound: order between a virtual timerfd and *external*
host fds (pipes/sockets) at one drain is Hermit's pre-existing external-IO
scope and is NOT claimed exact; determinism is claimed among virtual-time
families (signals, thread timeouts, timerfds), which is what the fixture
exercises (its epoll holds only the timerfd).

**F4 read path.** `handle_read` intercepts FdType::Timerfd in detcore
before `execute_nonblockable_fd_syscall`: len<8 EINVAL; pending>0 returns
u64 count and consumes; empty+NONBLOCK EAGAIN; empty+blocking retries via
the InternalIOPolling loop (turns are committed and advance virtual time;
the registered TimerFdEvt guarantees empty-queue fast-forward), EINTR from
ResumeStatus::Signaled consumes nothing (generic loop errno). Liveness
with only pollers: poll turns commit and advance global time (the fixture's
own epoll_timeout line already fires deterministically today), and step2d
covers the empty-queue case — neither relies on host time.

**F5 semantics corrected.** gettime: disarmed -> zeros; one-shot expired ->
0 remaining; periodic -> time to NEXT expiry. CANCEL_ON_SET: valid only for
CLOCK_REALTIME (else EINVAL at settime); detcore tracks a virtual-realtime
generation bumped by clock_settime/settimeofday; a realtime timerfd armed
with the flag returns ECANCELED from read/waits after a generation change.
Clocks: MONOTONIC, REALTIME, BOOTTIME (=MONOTONIC domain; suspend time not
modelled — disclosed deviation), REALTIME_ALARM/MONOTONIC_ALARM require
CAP_WAKE_ALARM -> EPERM (guests run unprivileged); all other clockids
EINVAL. Realtime deadlines are stored in the realtime domain
(now_realtime = virtual epoch base + logical elapsed; base shifts on
settime) and converted per query, so settimeofday shifts them like Linux.
Re-arm (settime) resets the pending count to the new arming, as Linux does.

**F6 record/replay.** Handlers are intercepted in both modes; the host
vessel is never armed and no host-derived value (old_value, counts,
readiness) is recorded — all are recomputed from virtual time and the
replayed syscall sequence (create/settime/epoll_ctl rebuild identical
state). 

**Tests (expanded per review).** Same-deadline Signal/Thread/TimerFd pop
order unit test; re-arm/disarm/close/exit-at-pop; blocked read vs
epoll-only poller ordering; ET latch; periodic gettime; CANCEL_ON_SET;
maxevents truncation; record+replay of the fixture; 20x strict ptrace
fixture byte-identical.

## Revision 3 — final two conditions (binding on the implementation)

**F2 coalesced re-arm.** In `pop_if_before`, a periodic TimerFdEvt whose
deadline is more than one interval behind `current_time` re-arms ONCE to the
first deadline strictly after `current_time`
(`deadline + interval * (floor((now-deadline)/interval)+1)`), never one pop
per missed interval. Expiration *counts* still coalesce in detcore's
`count(now)` arithmetic, so no guest-visible expirations are lost.

**F5 realtime domain (corrected).** Detcore today has ONE logical clock:
`handle_clock_gettime` ignores clockid and `clock_settime`/`settimeofday`
have no handler (classification refusal). Therefore: CLOCK_REALTIME
timerfd deadlines are stored in the logical domain as
`logical_deadline = realtime_deadline - run_epoch` (the run's fixed virtual
epoch), which is exact while settime is refused. `TFD_TIMER_CANCEL_ON_SET`
is accepted for CLOCK_REALTIME but the cancel generation has NO producer in
the current clock model, so `ECANCELED` is unreachable — disclosed here and
in code comments, replacing revision 1/2's virtual-settime claim. If a
future change virtualizes clock_settime it must bump the generation and
this becomes live.
