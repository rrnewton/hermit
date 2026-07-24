# Deep debug: QEMU under `hermit run --strict` — turn-by-turn scheduler analysis

Backend: ptrace. Log level: debug. Relaxations: none (`--strict` ⇒
`sequentialize_threads: true`, `virtualize_time: true`, `debug_futex_mode:
Precise`, `preemption_timeout: Some(200_000_000)` RCBs, `clock_multiplier: 1.0`).
Capture: `timeout 40s ... --log debug run --strict -- qemu-system-x86_64 -m 256M
-accel tcg,thread=single -smp 1 -icount shift=0,sleep=off ...` → `/tmp/qemu_debug.log`
(8216 lines). Result: exit 124 (SIGTERM at 40s), **0 kernel console lines**.

## What the three guest threads are

| dtid | role (from syscalls + RCB accumulation) | RCBs executed | blocks on |
|------|-----------------------------------------|---------------|-----------|
| 3 | **QEMU main loop / iothread** — busy-polls the clock | 49 → 15.0M | almost never (always runnable) |
| 7 | **TCG vCPU thread** (`thread=single`) — runs guest code | 8.4M → 28.2M (~20M, the most) | `FutexWait` ×4, `SleepUntil` |
| 5 | QEMU helper (RCU/aux) — mostly sleeps | 6.22M → 6.51M (~0.28M) | `SleepUntil` ×3, `FutexWait` ×2 |

## Turn-by-turn (baseline, multiplier 1)

- **117 COMMIT turns total: dtid 3 = 105 (90%), dtid 5 = 6, dtid 7 = 6.**
- Turn sequence is round-robin-fair among *runnable* threads: `3×66, 5, 3×5, 5,
  3×13, 7, 3, 5, 3, 7, 3×10, 7, …` — the scheduler hands 5 and 7 a turn every
  time they become runnable. dtid 3 dominates only because it is the sole
  *continuously* runnable thread.
- **1515 inbound syscalls; 712 (47%) are `clock_gettime` (384) + `gettimeofday`
  (328)** — QEMU's main loop spin-reading the virtual clock. Between consecutive
  clock reads dtid 3 retires only ~30–85 RCBs → a pure busy-wait spin, not work.
- **Virtual time reached 0.583 s in 40 s real = 1.5% speed.** Throughput
  ≈ **703 K RCBs/s**.
- **Preemption never meaningfully fires.** Slice = 200 M RCBs; the whole run was
  only 28 M RCBs, so the RCB preemption timer is ~7× too coarse to ever trigger
  (1 event, at teardown). Threads yield via syscalls/futex long before any slice.
  Preemption granularity is irrelevant to this failure.
- **The discrete-event time-warp never fires.** `grep 'Deadlock avoidance' = 0`.

## Mechanism (why the boot crawls)

This is QEMU's `-icount` round-robin handoff colliding with detcore's
time-advance model:

1. vCPU thread **7** runs an icount instruction budget, then **blocks on a futex**
   waiting for the main loop to grant the next slice.
2. Main loop **3** must let the virtual clock reach the next timer deadline
   before it wakes 7 (via futex). With `-icount sleep=off` it does this by
   **busy-polling `clock_gettime`/`gettimeofday`**.
3. detcore advances virtual time by only `NANOS_PER_SYSCALL(10_000)×mult` ≈ 10 µs
   per clock read and `NANOS_PER_SCHED(500_000)×mult` per turn
   (`detcore-model/src/time.rs:36-45,484-489,603-606`). So the clock creeps, the
   main loop spins thousands of times per guest-millisecond, and the vCPU is
   starved of budget.
4. Helper **5** and vCPU **7** sit in `SleepUntil`/`FutexWait` and only wake once
   dtid 3's crawling clock finally passes their deadline (e.g. turn 109: dtid 5's
   `SleepUntil(…389_800_790)` commits only at virtual `…576_780_950` — long after
   the deadline it should have woken at).

## Root cause, in code

The scheduler **can** jump virtual time to the earliest pending deadline, but the
only such path is the *deadlock-avoidance* branch in `step2d_handle_empty_queue`
(`detcore/src/scheduler.rs:1535-1561`), and it is gated on
`self.run_queue.is_empty()` (line 1524). A thread busy-polling the clock is
**runnable**, so the run queue is never empty and the warp never fires. There is
**no idle/poll-loop detection** for a clock-spinner (only `InternalIOPolling` has
special handling). `SleepUntil` waiters are parked in `blocked.timed_waiters` and
`step2b_process_timed` only wakes those already `<= committed_time`
(scheduler.rs:1205-1209) — it never pulls time *forward* to them.

## Empirical validation

`--clock-multiplier 100000` (make each clock read advance ~100_000× more virtual
time): turns jumped **117 → 2258**, dtid 7 turns **6 → 527**, dtid 5 **6 → 49** —
i.e. the vCPU/timer handoff sped up dramatically — **but still 0 kernel lines in
30 s**. This confirms *two compounding* problems, not one:
- (A) the clock-spin wastes turns (fixable by idle-warp), and
- (B) raw sequentialized-ptrace throughput (~700 K RCBs/s) is orders of magnitude
  below a Linux boot's need (hundreds of millions–billions of branches), so even
  a perfectly-scheduled boot is slow.

## The specific scheduling decision that should change

**Extend the discrete-event time-warp to fire on an idle clock-spinner, not only
on an empty run queue.** In `step2d_handle_empty_queue`
(`scheduler.rs:1535-1561`), or a new check before returning control to a spinner:
when the only runnable thread(s) are **clock-spinning** — a run of time-read
syscalls (`clock_gettime`/`gettimeofday`/`time`) with per-turn RCB delta below a
small threshold — **and** `blocked.timed_waiters` is non-empty, advance
`committed_time` to the earliest deadline (reuse the `add_extra_time(delta)` jump
at lines 1546-1554) and wake that waiter instead of re-running the spinner.

- This is exactly QEMU's own `-icount sleep=off` clock-warp and standard
  discrete-event-simulation idle handling.
- It turns O(guest_time / 10 µs) wasted clock-poll turns into O(#deadlines).
- Determinism is preserved: the warp target is a deterministic function of the
  recorded `timed_waiters` deadlines, so L1/L2 hold.

Implementation sketch: track per-thread a counter of consecutive pure-time-read
syscalls with RCB-delta below a threshold; when it exceeds the threshold and the
thread is the sole runnable one with non-empty `timed_waiters`, treat it as idle
and warp.

### Notes / non-fixes
- **This is not a turn-allocation "fairness" bug.** The scheduler already
  round-robins runnable threads fairly; 5 and 7 are genuinely *blocked*, so
  giving dtid 3 fewer turns cannot help — there is no other runnable thread to
  switch to while they wait for time to pass.
- `--clock-multiplier` is a stopgap: it shrinks the spin constant but does not
  bound it and it degrades time fidelity.
- The idle-warp fixes (A) but not (B). A genuinely fast deterministic QEMU boot
  additionally needs higher execution throughput — the KVM/DBI-backend story,
  out of scope for the ptrace scheduler.
